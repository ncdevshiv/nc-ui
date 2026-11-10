//! The host: compile a component, read what it declares, negotiate, link, invoke.
//!
//! Two instantiations happen per component and that is deliberate. The first one runs in a
//! store whose host functions refuse everything, and it exists only to read the manifest —
//! because the manifest is produced by the component's own code, and code that has not been
//! negotiated with must not be able to reach the host while it is explaining itself. The second
//! instantiation is the live one: it carries the narrowed grant and the gateway, and it is what
//! a caller invokes.
//!
//! # What is enforced here, and where the decision came from
//!
//! Nothing. Every authority question is answered by `ncui-abi`: [`negotiate`] decides whether
//! this component may run and with which capabilities, and this module obeys the answer. What
//! lives here is the *mechanism* — fuel, the memory limiter, the copy budget, the packed
//! location decoding — plus the two links back to the host that a component can only reach if
//! the negotiation opened them.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use ncui_abi::wire::WireManifest;
use ncui_abi::{ComponentManifest, HostProfile, Negotiated, negotiate};
use ncui_core::{Capability, CapabilitySet};
use serde_json::Value;
use wasmi::{
    AsContext, AsContextMut, Config, EnforcedLimits, Engine, Error as WasmError, Extern, Func,
    Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TrapCode, Val,
};

use crate::contract::{
    ABI_MAJOR, ABI_VERSION_EXPORT, CAPABILITY_CHECK_IMPORT, HOST_CALL_IMPORT, HOST_MODULE,
    INVOKE_EXPORT, MANIFEST_EXPORT, MEMORY_EXPORT, REFUSAL_BAD_INPUT, REFUSAL_HOST_ERROR,
    REFUSAL_TOO_SMALL, REFUSAL_UNDECLARED, SCRATCH_EXPORT, input_offset, unpack,
};
use crate::error::ComponentError;
use crate::gateway::ComponentGateway;
use crate::limits::ComponentLimits;

/// A component host, holding the engine every component it compiles shares.
///
/// The engine is configuration: it decides which WebAssembly features exist at all, whether a
/// start section is permitted, and what limits the validator enforces. Sharing one across
/// components is what makes the second and later compiles cheap.
#[derive(Debug)]
pub struct ComponentRuntime {
    engine: Engine,
    limits: ComponentLimits,
}

impl ComponentRuntime {
    /// Builds a host from `limits`.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentError::InvalidLimits`] or [`ComponentError::InconsistentLimits`]
    /// when the bounds do not bound anything, or contradict each other. The host refuses to
    /// start rather than run components under a configuration that would silently allow more
    /// than the caller meant.
    pub fn new(limits: ComponentLimits) -> Result<Self, ComponentError> {
        limits.validate()?;
        let mut config = Config::default();
        // Fuel is the only thing that stops a component that does not return. Without it the
        // interpreter runs until the user kills the process, which is not a bound.
        config.consume_fuel(true);
        config.set_max_stack_height(MAX_STACK_HEIGHT);
        config.enforced_limits(EnforcedLimits::strict());
        // Instantiation must execute nothing. A start section is a component that runs before
        // the host has decided anything about it.
        config.allow_start_fn(false);
        // One memory, 32-bit addresses. Two memories would make the contract's single pointer
        // space ambiguous, and memory64 would move every bound in this crate.
        config.wasm_multi_memory(false);
        config.wasm_memory64(false);
        Ok(Self {
            engine: Engine::new(&config),
            limits,
        })
    }

    /// The bounds this host executes components under.
    #[must_use]
    pub const fn limits(&self) -> &ComponentLimits {
        &self.limits
    }

    /// Compiles an artifact into something that can be declared and opened.
    ///
    /// Runs no component code: validation and compilation only.
    ///
    /// # Errors
    ///
    /// [`ComponentError::TooLarge`] before the bytes are handed to the engine, and
    /// [`ComponentError::InvalidModule`] when the engine will not accept them — which includes
    /// an artifact with a start section.
    pub fn compile(&self, bytes: &[u8]) -> Result<CompiledComponent, ComponentError> {
        if bytes.len() > self.limits.max_module_bytes {
            return Err(ComponentError::TooLarge {
                bytes: bytes.len(),
                limit: self.limits.max_module_bytes,
            });
        }
        let module = Module::new(&self.engine, bytes)
            .map_err(|error| invalid_module_with_start_hint(&error))?;
        Ok(CompiledComponent {
            engine: self.engine.clone(),
            module,
            limits: self.limits,
            declaration: OnceLock::new(),
        })
    }
}

/// The stack depth bound the engine enforces, in wasmi's own height units.
///
/// Deep recursion is how an interpreter dies, and the number is stated here rather than left to
/// an engine default so that a component authored against NC-UI meets the same bound on every
/// platform.
const MAX_STACK_HEIGHT: usize = 1_024;

/// A compiled artifact whose bytes the engine has accepted.
///
/// The manifest is read once, on first need, and cached: it comes from the component's own
/// code, and re-running that read for every open would let a component be charged for the same
/// explanation repeatedly.
#[derive(Debug)]
pub struct CompiledComponent {
    engine: Engine,
    module: Module,
    limits: ComponentLimits,
    declaration: OnceLock<Result<Declaration, ComponentError>>,
}

/// What the component said about itself, in the two forms the host needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The manifest document, already parsed by the ABI's wire reader.
    pub manifest: ComponentManifest,
    /// The numeric calling-convention revision the artifact's own export returned.
    pub convention: i32,
}

impl CompiledComponent {
    /// Reads the component's declaration, through a store that cannot reach the host.
    ///
    /// # Errors
    ///
    /// [`ComponentError::MissingExport`] for a contract export the artifact lacks,
    /// [`ComponentError::AbiMismatch`] when its numeric revision is not this host's,
    /// [`ComponentError::ManifestTooLarge`] or [`ComponentError::MalformedManifest`] for the
    /// document itself, and [`ComponentError::FuelExhausted`],
    /// [`ComponentError::GuestBounds`] or [`ComponentError::Trapped`] for code that misbehaves
    /// while explaining itself.
    pub fn declaration(&self) -> Result<&Declaration, ComponentError> {
        match self.declaration.get_or_init(|| self.read_declaration()) {
            Ok(declaration) => Ok(declaration),
            Err(error) => Err(error.clone()),
        }
    }

    /// Negotiates the component with `profile` and, if the answer is yes, runs it.
    ///
    /// The order is the whole security story: the manifest is read, `negotiate` decides, and
    /// only a component the negotiation accepted is linked to a gateway that can do anything.
    ///
    /// # Errors
    ///
    /// Anything [`Self::declaration`] can return, plus [`ComponentError::NotNegotiated`] when
    /// the profile refuses the ABI or one of the imports, and [`ComponentError::UnlinkedImport`]
    /// when the module asks the linker for a symbol outside the contract.
    pub fn open(
        &self,
        profile: &HostProfile,
        gateway: Arc<dyn ComponentGateway>,
    ) -> Result<RunningComponent, ComponentError> {
        let declaration = self.declaration()?;
        let negotiated = negotiate(&declaration.manifest, profile)
            .map_err(|error| ComponentError::NotNegotiated(error.to_string()))?;
        let state = ComponentState {
            grant: negotiated.granted,
            services: declared_services(&declaration.manifest),
            gateway: Some(gateway),
            limits: self.limits,
            budget: memory_budget(&self.limits),
            host_calls: 0,
            refusals: 0,
        };
        let (instance, store) = self.instantiate(state)?;
        Ok(RunningComponent {
            calls: 0,
            fuel_used: 0,
            methods: declared_methods(&declaration.manifest),
            store,
            declaration: declaration.clone(),
            instance,
            negotiated,
            limits: self.limits,
        })
    }

    /// The ABI version this profile and this component agreed on, if they agree.
    ///
    /// Tooling that only wants to know whether a component would run uses this instead of
    /// paying for an instantiation.
    ///
    /// # Errors
    ///
    /// [`ComponentError::NotNegotiated`] when they do not, and anything
    /// [`Self::declaration`] can return.
    pub fn negotiated_with(&self, profile: &HostProfile) -> Result<Negotiated, ComponentError> {
        let declaration = self.declaration()?;
        negotiate(&declaration.manifest, profile)
            .map_err(|error| ComponentError::NotNegotiated(error.to_string()))
    }

    /// The two steps of [`Self::declaration`], without the cache.
    fn read_declaration(&self) -> Result<Declaration, ComponentError> {
        // The refusing store: no grant, no services, no gateway. Anything the component tries to
        // do to the host while reading its own manifest is refused by construction.
        let state = ComponentState {
            grant: CapabilitySet::empty(),
            services: BTreeMap::new(),
            gateway: None,
            limits: self.limits,
            budget: memory_budget(&self.limits),
            host_calls: 0,
            refusals: 0,
        };
        let (instance, mut store) = self.instantiate(state)?;

        let convention = call_i32(&mut store, instance, &self.limits, ABI_VERSION_EXPORT)?;
        if convention != ABI_MAJOR {
            return Err(ComponentError::AbiMismatch {
                declared: convention,
                expected: ABI_MAJOR,
            });
        }

        let packed = call_i64(&mut store, instance, &self.limits, MANIFEST_EXPORT)?;
        let (pointer, length) = unpack(packed);
        if usize::try_from(length).unwrap_or(usize::MAX) > self.limits.max_manifest_bytes {
            return Err(ComponentError::ManifestTooLarge {
                bytes: length as usize,
                limit: self.limits.max_manifest_bytes,
            });
        }
        let bytes = read_guest_bytes(&store, instance, pointer, u64::from(length))?;
        let manifest = WireManifest::from_slice(&bytes)
            .map_err(|error| ComponentError::MalformedManifest(error.to_string()))?;
        // The manifest's version string and the artifact's numeric revision must name the same
        // major number. Two declarations that disagree is a component that means one thing to
        // the negotiator and another to the code that runs.
        let declared_major = manifest_major(&manifest.abi_version)?;
        if declared_major != convention {
            return Err(ComponentError::AbiMismatch {
                declared: declared_major,
                expected: convention,
            });
        }
        Ok(Declaration {
            manifest,
            convention,
        })
    }

    /// Builds a store around `state` and instantiates the module into it.
    ///
    /// The store and instance come back in the order that makes the borrow checker agree with
    /// the code: the instance is a handle into the store, and a caller that keeps both keeps the
    /// store alive by construction.
    fn instantiate(
        &self,
        state: ComponentState,
    ) -> Result<(Instance, Store<ComponentState>), ComponentError> {
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.budget);
        refill(&mut store, &self.limits)?;
        let instance = self
            .linker()?
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|error| link_failure(&error))?;
        Ok((instance, store))
    }

    /// The two imports the contract allows, and nothing else.
    ///
    /// `instantiate_and_start` fails for any import this does not define, which is how a
    /// component asking for a symbol from another runtime becomes a refusal rather than a
    /// surprise.
    fn linker(&self) -> Result<Linker<ComponentState>, ComponentError> {
        let mut linker: Linker<ComponentState> = Linker::new(&self.engine);
        linker
            .func_wrap(HOST_MODULE, CAPABILITY_CHECK_IMPORT, serve_capability_check)
            .map_err(|error| {
                ComponentError::InvalidModule(format!(
                    "the import `{HOST_MODULE}`/`{CAPABILITY_CHECK_IMPORT}` could not be \
                     defined: {error}"
                ))
            })?;
        linker
            .func_wrap(HOST_MODULE, HOST_CALL_IMPORT, serve_host_call)
            .map_err(|error| {
                ComponentError::InvalidModule(format!(
                    "the import `{HOST_MODULE}`/`{HOST_CALL_IMPORT}` could not be defined: {error}"
                ))
            })?;
        Ok(linker)
    }
}

/// Answers `ncui::capability_check`: may this component use that capability right now?
///
/// The answer comes from the negotiated grant and from nothing else. A name that is not a
/// capability is answered "no" rather than reported as an error: to a component the two are the
/// same question, and a component that could tell them apart would be reading the grant table.
fn serve_capability_check(
    mut caller: wasmi::Caller<'_, ComponentState>,
    pointer: i32,
    length: i32,
) -> Result<i32, WasmError> {
    let bound = caller.data().limits.max_name_bytes;
    let name = read_guest_text(&caller, pointer, length, bound)?;
    let granted = match name {
        Some(text) => match text.parse::<Capability>() {
            Ok(capability) => caller.data().grant.contains(capability),
            Err(_) => false,
        },
        None => false,
    };
    let state = caller.data_mut();
    state.host_calls += 1;
    if !granted {
        state.refusals += 1;
    }
    Ok(i32::from(granted))
}

/// Serves `ncui::host_call`, in the order that matters: declared, then granted, then served.
///
/// Every branch that refuses increments the component's refusal count, because a component
/// probing for authority it was not given is what an operator needs to see, and a refusal that
/// is only a return code is a refusal nobody can audit.
fn serve_host_call(
    mut caller: wasmi::Caller<'_, ComponentState>,
    name_pointer: i32,
    name_length: i32,
    input_pointer: i32,
    input_length: i32,
    output_pointer: i32,
    output_length: i32,
) -> Result<i32, WasmError> {
    let name_bound = caller.data().limits.max_name_bytes;
    let copy_bound = caller.data().limits.max_guest_copy_bytes;
    let Some(service) = read_guest_text(&caller, name_pointer, name_length, name_bound)? else {
        return Ok(REFUSAL_BAD_INPUT);
    };
    let Some(body) = read_guest(&caller, input_pointer, input_length, copy_bound)? else {
        return Ok(REFUSAL_BAD_INPUT);
    };
    let Ok(input) = serde_json::from_slice::<Value>(&body) else {
        note_refusal(&mut caller);
        return Ok(REFUSAL_BAD_INPUT);
    };

    // The gate, in the order that matters: declared, then granted, then served.
    let Some(capability) = caller.data().services.get(&service).copied() else {
        note_refusal(&mut caller);
        return Ok(REFUSAL_UNDECLARED);
    };
    if capability.is_some_and(|needed| !caller.data().grant.contains(needed)) {
        note_refusal(&mut caller);
        return Ok(REFUSAL_UNDECLARED);
    }
    let Some(gateway) = caller.data().gateway.as_ref() else {
        // The declaration read. A component cannot reach the host while it is telling the host
        // what it wants.
        note_refusal(&mut caller);
        return Ok(REFUSAL_UNDECLARED);
    };
    let Ok(served) = gateway.call(&service, capability, input) else {
        note_refusal(&mut caller);
        return Ok(REFUSAL_HOST_ERROR);
    };
    let Ok(bytes) = serde_json::to_vec(&served) else {
        note_refusal(&mut caller);
        return Ok(REFUSAL_HOST_ERROR);
    };
    if output_length < 0 || usize::try_from(output_length).unwrap_or(usize::MAX) < bytes.len() {
        return Ok(REFUSAL_TOO_SMALL);
    }
    let memory = guest_memory(&caller)?;
    memory
        .write(
            caller.as_context_mut(),
            pointer_offset(output_pointer)?,
            &bytes,
        )
        .map_err(|error| {
            note_refusal(&mut caller);
            WasmError::from(error)
        })?;
    Ok(i32::try_from(bytes.len()).unwrap_or(i32::MAX))
}

/// A running component: an instance, its store, and the authority it was given.
#[derive(Debug)]
pub struct RunningComponent {
    store: Store<ComponentState>,
    instance: Instance,
    limits: ComponentLimits,
    declaration: Declaration,
    negotiated: Negotiated,
    methods: Vec<String>,
    calls: u64,
    fuel_used: u64,
}

impl RunningComponent {
    /// The capabilities this component may use, as the negotiation produced them.
    #[must_use]
    pub const fn grant(&self) -> CapabilitySet {
        self.negotiated.granted
    }

    /// The ABI version agreed with the host profile.
    #[must_use]
    pub fn abi_version(&self) -> &str {
        &self.negotiated.abi_version
    }

    /// What the component declared about itself.
    #[must_use]
    pub const fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// The methods the component declared, and may therefore be asked to run.
    #[must_use]
    pub fn methods(&self) -> &[String] {
        &self.methods
    }

    /// The services the component declared, whether or not the grant covers them.
    #[must_use]
    pub fn declared_services(&self) -> Vec<String> {
        self.store.data().services.keys().cloned().collect()
    }

    /// How many times this component has been invoked.
    #[must_use]
    pub const fn calls(&self) -> u64 {
        self.calls
    }

    /// Total fuel this component's calls have consumed.
    #[must_use]
    pub const fn fuel_used(&self) -> u64 {
        self.fuel_used
    }

    /// How many host requests this component made that were refused.
    ///
    /// This is the number that shows a component probing for authority it was not given: a
    /// rising count with no grant change is behaving as an adversary.
    #[must_use]
    pub fn refusals(&self) -> usize {
        self.store.data().refusals
    }

    /// How many times this component has reached the host at all.
    #[must_use]
    pub fn host_calls(&self) -> usize {
        self.store.data().host_calls
    }

    /// Whether the negotiation grants `capability`.
    #[must_use]
    pub const fn permits(&self, capability: Capability) -> bool {
        self.negotiated.permits(capability)
    }

    /// Calls `method` with `input` and returns the component's JSON response.
    ///
    /// # Errors
    ///
    /// [`ComponentError::Undeclared`] for a method the manifest does not advertise,
    /// [`ComponentError::Overlong`] for a name or body past the host's bound,
    /// [`ComponentError::ScratchTooSmall`] when the component published less writable space
    /// than the call needs, [`ComponentError::FuelExhausted`] when it ran out of time,
    /// [`ComponentError::GuestBounds`] when it names a document outside its own memory, and
    /// [`ComponentError::OutputTooLarge`] or [`ComponentError::BadResult`] when what comes back
    /// is too big or is not JSON.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the caller hands the request over; the host serialises it and keeps nothing"
    )]
    pub fn invoke(&mut self, method: &str, input: Value) -> Result<Value, ComponentError> {
        if !self.methods.iter().any(|declared| declared == method) {
            return Err(ComponentError::Undeclared {
                kind: "method",
                name: method.to_owned(),
            });
        }
        let name = method.as_bytes();
        if name.len() > self.limits.max_name_bytes {
            return Err(ComponentError::Overlong {
                what: "method name".to_owned(),
                bytes: name.len(),
                limit: self.limits.max_name_bytes,
            });
        }
        let body = serde_json::to_vec(&input)
            .map_err(|error| ComponentError::Unserialisable(error.to_string()))?;
        if body.len() > self.limits.max_input_bytes {
            return Err(ComponentError::Overlong {
                what: "request body".to_owned(),
                bytes: body.len(),
                limit: self.limits.max_input_bytes,
            });
        }

        let before = self.remaining_fuel()?;
        let scratch = call_i64(&mut self.store, self.instance, &self.limits, SCRATCH_EXPORT)?;
        let (scratch_pointer, scratch_length) = unpack(scratch);
        let name_length = u32::try_from(name.len()).unwrap_or(u32::MAX);
        let payload_offset = input_offset(name_length);
        let needed = usize::try_from(payload_offset)
            .unwrap_or(usize::MAX)
            .checked_add(body.len())
            .ok_or_else(|| ComponentError::Overlong {
                what: "request".to_owned(),
                bytes: usize::MAX,
                limit: self.limits.max_input_bytes,
            })?;
        if needed > usize::try_from(scratch_length).unwrap_or(usize::MAX) {
            return Err(ComponentError::ScratchTooSmall {
                needed: u32::try_from(needed).unwrap_or(u32::MAX),
                available: scratch_length,
            });
        }
        let payload_pointer = scratch_pointer.checked_add(payload_offset).ok_or_else(|| {
            ComponentError::GuestBounds("the scratch offset overflowed".to_owned())
        })?;

        write_guest_bytes(&mut self.store, self.instance, scratch_pointer, name)?;
        write_guest_bytes(&mut self.store, self.instance, payload_pointer, &body)?;

        let packed = call_invoke(
            &mut self.store,
            self.instance,
            &self.limits,
            scratch_pointer,
            name_length,
            payload_pointer,
            u32::try_from(body.len()).unwrap_or(u32::MAX),
        )?;
        let (result_pointer, result_length) = unpack(packed);
        if usize::try_from(result_length).unwrap_or(usize::MAX) > self.limits.max_output_bytes {
            return Err(ComponentError::OutputTooLarge {
                bytes: result_length,
                limit: self.limits.max_output_bytes,
            });
        }
        let bytes = read_guest_bytes(
            &self.store,
            self.instance,
            result_pointer,
            u64::from(result_length),
        )?;
        let value = serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| ComponentError::BadResult(error.to_string()))?;

        let after = self.remaining_fuel()?;
        // Saturating rather than wrapping: fuel accounting that went negative would report a
        // component as cheaper the more it ran.
        self.fuel_used = self.fuel_used.saturating_add(before.saturating_sub(after));
        self.calls += 1;
        Ok(value)
    }

    /// The fuel left in the store, for the accounting around a call.
    fn remaining_fuel(&self) -> Result<u64, ComponentError> {
        self.store
            .get_fuel()
            .map_err(|error| ComponentError::Trapped(error.to_string()))
    }
}

/// The store's own data: what the component was granted, what it declared, what it has done.
///
/// Written by hand rather than derived because the gateway belongs to whoever installed it, and
/// a component that could make the host print one would be a component reading the host's
/// configuration through a diagnostic.
struct ComponentState {
    grant: CapabilitySet,
    services: BTreeMap<String, Option<Capability>>,
    gateway: Option<Arc<dyn ComponentGateway>>,
    limits: ComponentLimits,
    budget: StoreLimits,
    host_calls: usize,
    refusals: usize,
}

impl core::fmt::Debug for ComponentState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComponentState")
            .field("grant", &self.grant)
            .field("services", &self.services.keys().collect::<Vec<_>>())
            .field("host_calls", &self.host_calls)
            .field("refusals", &self.refusals)
            .finish_non_exhaustive()
    }
}

/// The services the component declared, mapped to the capability each one needs.
fn declared_services(manifest: &ComponentManifest) -> BTreeMap<String, Option<Capability>> {
    manifest
        .surface
        .imports()
        .iter()
        .map(|import| (import.name.clone(), import.requires))
        .collect()
}

/// The methods the component declared.
fn declared_methods(manifest: &ComponentManifest) -> Vec<String> {
    manifest
        .surface
        .exports()
        .iter()
        .map(|export| export.name.clone())
        .collect()
}

/// The store's resource limiter, built from the host's bounds.
///
/// `trap_on_grow_failure` is on deliberately: the specification has `memory.grow` return `-1`
/// when it cannot grow, and a component that ignores the answer would go on writing into memory
/// it does not have. A trap is the honest reading of "you may not have that".
fn memory_budget(limits: &ComponentLimits) -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(limits.max_memory_bytes)
        .instances(1)
        .memories(1)
        .tables(0)
        .table_elements(0)
        .trap_on_grow_failure(true)
        .build()
}

/// The major number a manifest's version string names.
fn manifest_major(version: &str) -> Result<i32, ComponentError> {
    let head = version.split('.').next().unwrap_or_default();
    head.trim().parse::<i32>().map_err(|_| {
        ComponentError::MalformedManifest(format!(
            "`{version}` names no numeric major version to compare with the artifact"
        ))
    })
}

/// Keeps a start-section refusal readable, because it is the most confusing refusal an
/// otherwise correct component can produce.
fn invalid_module_with_start_hint(error: &WasmError) -> ComponentError {
    let message = error.to_string();
    if message.contains("start") {
        return ComponentError::InvalidModule(format!(
            "{message} — a component may not carry a start section: instantiation must not \
             execute the artifact"
        ));
    }
    ComponentError::InvalidModule(message)
}

/// Turns a linking failure into the refusal it describes.
fn link_failure(error: &WasmError) -> ComponentError {
    let message = error.to_string();
    if message.contains("import") {
        return ComponentError::UnlinkedImport(message);
    }
    ComponentError::InvalidModule(message)
}

/// A guest pointer as a host offset, refusing the shapes that cannot name a location.
///
/// A negative pointer is a component addressing memory before the start of its own linear
/// space. That is a bug in the component rather than a policy question, so it surfaces as a
/// trap the caller sees instead of being silently clamped to zero.
fn pointer_offset(pointer: i32) -> Result<usize, WasmError> {
    if pointer < 0 {
        return Err(WasmError::new(format!(
            "the component addressed memory at {pointer}, which is before the start of its own \
             linear space"
        )));
    }
    Ok(pointer as usize)
}

/// Reads `length` bytes out of the component's memory, inside a host function.
///
/// Returns `Ok(None)` when the component handed the host something a bound refuses — a negative
/// address, a negative length, a length over `bound` — and `Err` only when the range is
/// genuinely outside the memory, which is a trap the component should see.
fn read_guest(
    caller: &wasmi::Caller<'_, ComponentState>,
    pointer: i32,
    length: i32,
    bound: usize,
) -> Result<Option<Vec<u8>>, WasmError> {
    if pointer < 0 || length < 0 {
        return Ok(None);
    }
    let length = length as usize;
    if length > bound {
        return Ok(None);
    }
    let memory = guest_memory(caller)?;
    let mut buffer = vec![0u8; length];
    if length > 0 {
        memory
            .read(caller.as_context(), pointer_offset(pointer)?, &mut buffer)
            .map_err(WasmError::from)?;
    }
    Ok(Some(buffer))
}

/// The same, decoded as UTF-8 text. `None` covers a bound refusal and a non-text answer
/// together: to a component, both read as "that is not a name this host will serve".
fn read_guest_text(
    caller: &wasmi::Caller<'_, ComponentState>,
    pointer: i32,
    length: i32,
    bound: usize,
) -> Result<Option<String>, WasmError> {
    Ok(match read_guest(caller, pointer, length, bound)? {
        Some(bytes) => String::from_utf8(bytes).ok(),
        None => None,
    })
}

/// The component's exported memory, looked up on every call rather than captured in a closure,
/// because the handle belongs to the store the component is running in and not to the linker.
fn guest_memory(caller: &wasmi::Caller<'_, ComponentState>) -> Result<Memory, WasmError> {
    match caller.get_export(MEMORY_EXPORT) {
        Some(Extern::Memory(memory)) => Ok(memory),
        Some(_) => Err(WasmError::new(format!(
            "the export `{MEMORY_EXPORT}` is not a memory"
        ))),
        None => Err(WasmError::new(format!(
            "the component does not export `{MEMORY_EXPORT}`"
        ))),
    }
}

fn note_refusal(caller: &mut wasmi::Caller<'_, ComponentState>) {
    caller.data_mut().refusals += 1;
    caller.data_mut().host_calls += 1;
}

/// Reads bytes out of the component's memory from the host side, with an out-of-range answer
/// reported as [`ComponentError::GuestBounds`] rather than a panic.
fn read_guest_bytes(
    store: &Store<ComponentState>,
    instance: Instance,
    pointer: u32,
    length: u64,
) -> Result<Vec<u8>, ComponentError> {
    let memory = memory_of(store, instance)?;
    let data = memory.data(store);
    let start = pointer as usize;
    let end = start
        .checked_add(usize::try_from(length).unwrap_or(usize::MAX))
        .ok_or_else(|| {
            ComponentError::GuestBounds(format!(
                "the range {pointer}+{length} overflows the address space"
            ))
        })?;
    data.get(start..end).map(<[u8]>::to_vec).ok_or_else(|| {
        ComponentError::GuestBounds(format!(
            "the range {start}..{end} is outside the component's {} bytes of memory",
            data.len()
        ))
    })
}

/// Writes bytes into the component's memory.
fn write_guest_bytes(
    store: &mut Store<ComponentState>,
    instance: Instance,
    pointer: u32,
    bytes: &[u8],
) -> Result<(), ComponentError> {
    let memory = memory_of(store, instance)?;
    let start = pointer as usize;
    memory
        .write(store.as_context_mut(), start, bytes)
        .map_err(|_| {
            ComponentError::GuestBounds(format!(
                "the host could not write {} bytes at {pointer}",
                bytes.len()
            ))
        })
}

fn memory_of(store: &Store<ComponentState>, instance: Instance) -> Result<Memory, ComponentError> {
    instance
        .get_memory(store, MEMORY_EXPORT)
        .ok_or_else(|| ComponentError::MissingExport(MEMORY_EXPORT.to_owned()))
}

/// Calls a no-parameter export that returns `i32`.
fn call_i32(
    store: &mut Store<ComponentState>,
    instance: Instance,
    limits: &ComponentLimits,
    name: &str,
) -> Result<i32, ComponentError> {
    let mut results = [Val::I32(0)];
    call_void(store, instance, limits, name, &mut results)?;
    match &results[0] {
        Val::I32(value) => Ok(*value),
        other => Err(ComponentError::BadExport {
            name: name.to_owned(),
            reason: format!("it returned {other:?}, the contract wants an i32"),
        }),
    }
}

/// Calls a no-parameter export that returns `i64`.
fn call_i64(
    store: &mut Store<ComponentState>,
    instance: Instance,
    limits: &ComponentLimits,
    name: &str,
) -> Result<i64, ComponentError> {
    let mut results = [Val::I64(0)];
    call_void(store, instance, limits, name, &mut results)?;
    match &results[0] {
        Val::I64(value) => Ok(*value),
        other => Err(ComponentError::BadExport {
            name: name.to_owned(),
            reason: format!("it returned {other:?}, the contract wants an i64"),
        }),
    }
}

/// Calls `ncui_invoke` with the four packed arguments the contract defines.
///
/// The casts are the contract rather than an oversight: a wasm `i32` parameter carries the same
/// 32 bits as the guest pointer it encodes, so a pointer above `i32::MAX` is passed as a
/// negative number and read back as the same unsigned value on the other side.
#[allow(
    clippy::cast_possible_wrap,
    reason = "a wasm i32 parameter carries the same 32 bits as the guest pointer it encodes"
)]
fn call_invoke(
    store: &mut Store<ComponentState>,
    instance: Instance,
    limits: &ComponentLimits,
    name_pointer: u32,
    name_length: u32,
    input_pointer: u32,
    input_length: u32,
) -> Result<i64, ComponentError> {
    let args = [
        Val::I32(name_pointer as i32),
        Val::I32(name_length as i32),
        Val::I32(input_pointer as i32),
        Val::I32(input_length as i32),
    ];
    let mut results = [Val::I64(0)];
    let function = export(store, instance, INVOKE_EXPORT)?;
    refill(store, limits)?;
    function
        .call(store.as_context_mut(), &args, &mut results)
        .map_err(|error| classify(&error, limits.fuel_per_invoke))?;
    match &results[0] {
        Val::I64(value) => Ok(*value),
        other => Err(ComponentError::BadExport {
            name: INVOKE_EXPORT.to_owned(),
            reason: format!("it returned {other:?}, the contract wants an i64"),
        }),
    }
}

/// Calls a no-parameter export.
fn call_void(
    store: &mut Store<ComponentState>,
    instance: Instance,
    limits: &ComponentLimits,
    name: &str,
    results: &mut [Val],
) -> Result<(), ComponentError> {
    let function = export(store, instance, name)?;
    refill(store, limits)?;
    function
        .call(store.as_context_mut(), &[], results)
        .map_err(|error| classify(&error, limits.fuel_per_invoke))
}

/// Resolves an export as a function, refusing a missing one by name.
fn export(
    store: &Store<ComponentState>,
    instance: Instance,
    name: &str,
) -> Result<Func, ComponentError> {
    instance
        .get_func(store, name)
        .ok_or_else(|| ComponentError::MissingExport(name.to_owned()))
}

/// Gives the next entry into guest code its full budget.
///
/// Every call refills first, so a component is stopped by the call that spent the budget rather
/// than by whatever happens to come after it.
fn refill(
    store: &mut Store<ComponentState>,
    limits: &ComponentLimits,
) -> Result<(), ComponentError> {
    store
        .set_fuel(limits.fuel_per_invoke)
        .map_err(|error| ComponentError::Trapped(error.to_string()))
}

/// Maps an engine error onto the refusal an operator can act on.
fn classify(error: &WasmError, fuel: u64) -> ComponentError {
    match error.as_trap_code() {
        Some(TrapCode::OutOfFuel) => ComponentError::FuelExhausted { fuel },
        Some(TrapCode::MemoryOutOfBounds) => ComponentError::GuestBounds(error.to_string()),
        Some(TrapCode::GrowthOperationLimited) => ComponentError::GuestBounds(format!(
            "{error} (the component grew past the host's memory cap)"
        )),
        Some(code) => ComponentError::Trapped(format!("{code:?}: {error}")),
        None => ComponentError::Trapped(error.to_string()),
    }
}
