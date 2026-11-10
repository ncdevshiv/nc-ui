//! The component surface: untrusted artifacts, run inside the runtime.
//!
//! A page that holds `run.components` may load a `.wasm` artifact from the application's own
//! directory and call its methods. Everything the component wants from the machine it has to
//! ask for, by name, in its manifest — and the answer to that question is decided twice: once by
//! the ABI negotiation, which decides whether the component may run at all and with which
//! capabilities, and once per call by the registry's component call path, which refuses a command
//! whose requirements the component's narrowed grant does not cover.
//!
//! # What a component can and cannot reach
//!
//! * **Application commands**, by their exact name. A component that declares the service
//!   `notes.count` gets `notes.count` if the negotiation granted the capability that command
//!   requires.
//! * **Not the runtime's own methods.** `ncui.logs.query` and the rest of the agent pipeline are
//!   reached through that pipeline, with the agent's authority and its audit trail. A component
//!   is not an agent, and giving one a shorter route to runtime internals would make every
//!   decision the agent surface makes about *who is asking* decorative.
//! * **Nothing it did not declare.** The host refuses an undeclared service before this module
//!   is ever consulted, so the list of what a component may ask is the list it published.
//!
//! # Why one component is one lock
//!
//! A WebAssembly instance has one store and a store is not reentrant: two threads calling the
//! same instance would be two threads writing the same scratch memory. Each loaded component is
//! therefore held behind a mutex and calls are serialised, which is also what makes the call
//! count and fuel total a component has spent meaningful numbers rather than a race.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ncui_abi::HostProfile;
use ncui_component::{ComponentGateway, ComponentRuntime, RunningComponent};
use ncui_core::{Capability, CapabilitySet, ErrorKind, NcError, Result};
use ncui_observability::metrics::{Labels, MetricsRegistry};
use ncui_security::SafePath;
use ncui_webview::SeamError;
use serde_json::{Value, json};

use crate::Runtime;
use crate::commands::{CommandRegistration, PageCommands};

/// The capability a page must hold to load or drive a component.
pub const COMPONENT_CAPABILITY: Capability = Capability::RunComponents;

/// A running set of components, inside one application directory.
#[derive(Debug)]
pub struct Components {
    host: ComponentRuntime,
    commands: PageCommands,
    metrics: Arc<MetricsRegistry>,
    root: PathBuf,
    grantable: CapabilitySet,
    loaded: Mutex<BTreeMap<String, Loaded>>,
}

/// One loaded component.
#[derive(Debug)]
struct Loaded {
    /// The instance. `Mutex` because a store is single-threaded by construction.
    component: Arc<Mutex<RunningComponent>>,
    /// The capabilities the negotiation gave it.
    grant: CapabilitySet,
    /// The methods its manifest advertised.
    methods: Vec<String>,
    /// The services its manifest declared.
    services: Vec<String>,
    /// The ABI version agreed with the host profile.
    abi_version: String,
}

impl Components {
    /// Builds a surface rooted at `root`, granting at most `grantable`.
    ///
    /// `grantable` is what the runtime is willing to hand to *any* component — normally the
    /// page grant, so a component can never hold authority the page that loaded it does not.
    ///
    /// # Errors
    ///
    /// Propagates a host whose limits are incoherent as [`ErrorKind::Validation`].
    pub fn new(
        root: &Path,
        grantable: CapabilitySet,
        commands: PageCommands,
        metrics: Arc<MetricsRegistry>,
    ) -> Result<Self> {
        let host =
            ComponentRuntime::new(ncui_component::ComponentLimits::default()).map_err(|error| {
                NcError::new(ErrorKind::Validation {
                    field: "component_host".to_owned(),
                    reason: error.to_string(),
                })
                .operation("components::new")
            })?;
        Ok(Self {
            host,
            commands,
            metrics,
            root: root.to_path_buf(),
            grantable,
            loaded: Mutex::new(BTreeMap::new()),
        })
    }

    /// Loads the artifact at `artifact` (relative to the application directory) as `name`.
    ///
    /// # Errors
    ///
    /// A path that escapes the application directory, an artifact that cannot be read, compiled,
    /// declared or negotiated, and a name already in use all surface as [`SeamError`] with the
    /// host's own reason.
    pub fn open(&self, name: &str, artifact: &str) -> std::result::Result<Value, SeamError> {
        if name.trim().is_empty() {
            return Err(SeamError::new("invalid_params", "`name` must not be empty"));
        }
        let path = SafePath::resolve(&self.root, artifact).map_err(|error| seam_error(&error))?;
        let bytes = std::fs::read(&path).map_err(|error| {
            SeamError::new(
                "artifact_unreadable",
                format!("the artifact could not be read: {error}"),
            )
        })?;
        let profile = HostProfile::new(&[ncui_abi::HOST_ABI_VERSION], self.grantable);
        let compiled = self
            .host
            .compile(&bytes)
            .map_err(|error| component_error(&error))?;
        let gateway = Arc::new(RuntimeGateway {
            commands: self.commands.clone(),
            metrics: Arc::clone(&self.metrics),
            grant: compiled
                .declaration()
                .map_err(|error| component_error(&error))?
                .manifest
                .effective_requirements()
                .intersect(self.grantable),
            component: name.to_owned(),
        });
        let component = compiled
            .open(&profile, gateway)
            .map_err(|error| component_error(&error))?;
        let grant = component.grant();
        let methods = component.methods().to_vec();
        let services = component.declared_services();
        let abi_version = component.abi_version().to_owned();
        let summary = json!({
            "name": name,
            "artifact": artifact,
            "abi_version": abi_version,
            "grant": grant.iter().map(Capability::as_str).collect::<Vec<_>>(),
            "methods": methods,
            "services": services,
        });
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        // The check happens under the lock, and the running component is dropped after it, so a
        // rejected second load cannot leave a live instance whose authority nobody can revoke.
        if loaded.contains_key(name) {
            return Err(SeamError::new(
                "already_loaded",
                format!("a component named `{name}` is already loaded"),
            ));
        }
        loaded.insert(
            name.to_owned(),
            Loaded {
                component: Arc::new(Mutex::new(component)),
                grant,
                methods,
                services,
                abi_version,
            },
        );
        drop(loaded);
        self.counter("opened");
        tracing::info!(
            component = name,
            artifact,
            grant = ?grant.iter().map(Capability::as_str).collect::<Vec<_>>(),
            "a component was loaded and negotiated"
        );
        Ok(summary)
    }

    /// Calls `method` on the loaded component `name`.
    ///
    /// # Errors
    ///
    /// An unknown component, a method the component never advertised, and anything the host
    /// reports about the call itself.
    pub fn invoke(
        &self,
        name: &str,
        method: &str,
        input: Value,
    ) -> std::result::Result<Value, SeamError> {
        let component = {
            let loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
            loaded.get(name).map(|entry| Arc::clone(&entry.component))
        }
        .ok_or_else(|| {
            SeamError::new(
                "unknown_component",
                format!("no component named `{name}` is loaded"),
            )
        })?;
        let mut running = component.lock().unwrap_or_else(PoisonError::into_inner);
        let answer = running
            .invoke(method, input)
            .map_err(|error| component_error(&error))?;
        let summary = json!({
            "name": name,
            "method": method,
            "result": answer,
            "calls": running.calls(),
            "fuel_used": running.fuel_used(),
            "refusals": running.refusals(),
        });
        self.counter("invoked");
        tracing::info!(
            component = name,
            method,
            fuel = running.fuel_used(),
            calls = running.calls(),
            refusals = running.refusals(),
            "a component call completed"
        );
        Ok(summary)
    }

    /// Unloads `name`, dropping its instance and store.
    ///
    /// # Errors
    ///
    /// An unknown component.
    pub fn close(&self, name: &str) -> std::result::Result<Value, SeamError> {
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        let removed = loaded.remove(name).ok_or_else(|| {
            SeamError::new(
                "unknown_component",
                format!("no component named `{name}` is loaded"),
            )
        })?;
        let summary = json!({
            "name": name,
            "closed": true,
            "calls": removed.component.lock().unwrap_or_else(PoisonError::into_inner).calls(),
        });
        self.counter("closed");
        tracing::info!(component = name, "a component was unloaded");
        Ok(summary)
    }

    /// What is loaded, and what each one holds.
    #[must_use]
    pub fn list(&self) -> Value {
        let loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        json!({
            "components": loaded.iter().map(|(name, entry)| json!({
                "name": name,
                "abi_version": entry.abi_version,
                "grant": entry.grant.iter().map(Capability::as_str).collect::<Vec<_>>(),
                "methods": entry.methods,
                "services": entry.services,
            })).collect::<Vec<_>>(),
            "count": loaded.len(),
        })
    }

    /// The number of components currently loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn counter(&self, operation: &str) {
        let labels = Labels::from_pairs([("op", operation)]).unwrap_or_else(|_| Labels::new());
        let _ = self.metrics.counter_inc("component.calls", &labels, 1);
    }
}

/// A component's route to the application's commands.
///
/// Constructed per component with that component's narrowed grant closed over, so there is no
/// way for one component's call to be authorised by another's authority.
struct RuntimeGateway {
    commands: PageCommands,
    metrics: Arc<MetricsRegistry>,
    grant: CapabilitySet,
    component: String,
}

impl ComponentGateway for RuntimeGateway {
    fn call(
        &self,
        service: &str,
        capability: Option<Capability>,
        input: Value,
    ) -> std::result::Result<Value, String> {
        let answer = self.commands.component_call(service, self.grant, input);
        let outcome = if answer.is_ok() { "served" } else { "refused" };
        let labels = Labels::from_pairs([("op", outcome)]).unwrap_or_else(|_| Labels::new());
        let _ = self.metrics.counter_inc("component.host_calls", &labels, 1);
        match answer {
            Ok(value) => Ok(value),
            Err(error) => {
                tracing::info!(
                    component = self.component.as_str(),
                    service,
                    capability = capability.map_or("none", Capability::as_str),
                    code = error.code.as_str(),
                    "a component's service call was refused"
                );
                Err(error.message)
            }
        }
    }
}

/// Maps a host error onto the bridge's error shape.
fn component_error(error: &ncui_component::ComponentError) -> SeamError {
    let code = match &error {
        ncui_component::ComponentError::NotNegotiated(_) => "component_refused",
        ncui_component::ComponentError::FuelExhausted { .. } => "component_out_of_time",
        ncui_component::ComponentError::Undeclared { .. } => "unknown_method",
        ncui_component::ComponentError::TooLarge { .. }
        | ncui_component::ComponentError::InvalidModule(_)
        | ncui_component::ComponentError::UnlinkedImport(_) => "component_invalid",
        _ => "component_failed",
    };
    SeamError {
        code: code.to_owned(),
        message: error.to_string(),
        retryable: matches!(error, ncui_component::ComponentError::FuelExhausted { .. }),
    }
}

/// Maps a containment or filesystem failure onto the bridge's error shape.
fn seam_error(error: &NcError) -> SeamError {
    SeamError {
        code: "artifact_path_refused".to_owned(),
        message: error.to_string(),
        retryable: false,
    }
}

/// Registers the component commands the page grant covers.
///
/// The root is the application directory: a page names an artifact relative to it, and
/// [`ncui_security::SafePath`] is what decides that it stays there.
///
/// # Errors
///
/// Returns an error when a registration is refused, which would mean the command table and the
/// grant disagree.
pub fn register(runtime: &Runtime, root: &Path) -> Result<Vec<CommandRegistration>> {
    if !runtime.page_grant().contains(COMPONENT_CAPABILITY) {
        return Ok(Vec::new());
    }
    let components = Arc::new(Components::new(
        root,
        runtime.page_grant(),
        runtime.page_commands.clone(),
        Arc::clone(&runtime.metrics),
    )?);
    let mut registrations = Vec::new();
    // All four commands ask for the same capability. `run.components` is destructive, so the
    // consent gate that the authorization pipeline applies to destructive capabilities covers
    // each of them without a per-command flag that could drift from the capability table.
    for (name, summary) in [
        (
            "component.open",
            "Load a component artifact from the application directory and negotiate it.",
        ),
        (
            "component.invoke",
            "Call a method of a loaded component with a JSON argument.",
        ),
        (
            "component.close",
            "Unload a loaded component, dropping its instance and its store.",
        ),
        (
            "component.list",
            "List the loaded components, what they hold and what they may be asked to do.",
        ),
    ] {
        let surface = Arc::clone(&components);
        let handler = move |payload: Value| -> std::result::Result<Value, SeamError> {
            run_command(&surface, name, &payload)
        };
        registrations.push(
            runtime
                .register_page_command(
                    name,
                    CapabilitySet::empty().with(COMPONENT_CAPABILITY),
                    summary,
                    handler,
                )
                .map_err(|error| {
                    NcError::new(ErrorKind::Validation {
                        field: "component_command".to_owned(),
                        reason: format!("`{name}` could not be registered: {error}"),
                    })
                    .operation("components::register")
                })?,
        );
    }
    Ok(registrations)
}

/// Runs one component command from its payload.
fn run_command(
    components: &Arc<Components>,
    command: &str,
    payload: &Value,
) -> std::result::Result<Value, SeamError> {
    match command {
        "component.open" => {
            let name = text_field(payload, "name")?;
            let artifact = text_field(payload, "artifact")?;
            components.open(&name, &artifact)
        }
        "component.invoke" => {
            let name = text_field(payload, "name")?;
            let method = text_field(payload, "method")?;
            let input = payload.get("input").cloned().unwrap_or(Value::Null);
            components.invoke(&name, &method, input)
        }
        "component.close" => {
            let name = text_field(payload, "name")?;
            components.close(&name)
        }
        _ => Ok(components.list()),
    }
}

fn text_field(payload: &Value, field: &str) -> std::result::Result<String, SeamError> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| SeamError::new("invalid_params", format!("`{field}` must be a string")))
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CommandHandler, CommandTarget};
    use ncui_core::Limits;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A directory the test created and must remove.
    ///
    /// The suite owns what it acquires: a scratch directory that outlives the test is a leak
    /// that makes the next run differ from this one.
    pub(super) struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        pub(super) fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the clock is after the epoch")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("ncui-components-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).expect("the scratch directory is creatable");
            Self { path }
        }

        pub(super) fn write_artifact(&self, name: &str, bytes: &[u8]) {
            std::fs::write(self.path.join(name), bytes).expect("the artifact is writable");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // The mini fixture's layout. Every length the component compares against is computed from
    // the Rust string that produced it, so a document truncated by a hand-counted offset fails
    // a test instead of passing a wrong one.
    const MANIFEST_AT: u32 = 64;
    const CAP_READ_LOGS_AT: u32 = 512;
    const SERVICE_AT: u32 = 544;
    const METHOD_ECHO_AT: u32 = 640;
    const METHOD_HOST_AT: u32 = 672;
    const ERR_REFUSED_AT: u32 = 704;
    const SCRATCH_AT: u32 = 1024;
    const SCRATCH_LEN: u32 = 2048;
    const HOST_OUT_AT: u32 = 4096;
    const HOST_OUT_LEN: u32 = 2048;

    const CAP_READ_LOGS: &str = "read.logs";
    const SERVICE: &str = "notes.count";
    const INTERNAL: &str = "ncui.logs.query";
    const METHOD_ECHO: &str = "echo";
    const METHOD_HOST: &str = "host";
    const ERR_REFUSED: &str = r#"{"error":"refused"}"#;

    /// The manifest the mini artifact carries: one host service, and the two methods the
    /// artifact implements.
    ///
    /// Both halves are declared because the host enforces both — a method that is not on the
    /// declaration cannot be called, and a service that is not on it cannot be served.
    fn mini_manifest(service: &str, requires: Capability) -> String {
        let manifest = ncui_abi::ComponentManifest::new(ncui_abi::HOST_ABI_VERSION)
            .requesting(CapabilitySet::empty().with(requires))
            .with_surface(
                ncui_abi::Surface::new()
                    .import(ncui_abi::Import {
                        module: "ncui".to_owned(),
                        name: service.to_owned(),
                        kind: ncui_abi::ImportKind::CapabilityCall,
                        requires: Some(requires),
                    })
                    .export(ncui_abi::Export {
                        name: METHOD_ECHO.to_owned(),
                        kind: ncui_abi::ExportKind::Function,
                    })
                    .export(ncui_abi::Export {
                        name: METHOD_HOST.to_owned(),
                        kind: ncui_abi::ExportKind::Function,
                    }),
            );
        serde_json::to_string(&ncui_abi::WireManifest::of(&manifest)).expect("a test manifest")
    }

    /// A component with two methods: echo, which answers from its own memory, and host,
    /// which asks the runtime to do the work.
    fn mini_artifact(service: &str, requires: Capability) -> Vec<u8> {
        let manifest = mini_manifest(service, requires);
        format!(
            r#"
(module
  (import "ncui" "capability_check" (func $check (param i32 i32) (result i32)))
  (import "ncui" "host_call" (func $host (param i32 i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1 4)
  (data (i32.const {MANIFEST_AT}) {manifest:?})
  (data (i32.const {CAP_READ_LOGS_AT}) {CAP_READ_LOGS:?})
  (data (i32.const {SERVICE_AT}) {service:?})
  (data (i32.const {METHOD_ECHO_AT}) {METHOD_ECHO:?})
  (data (i32.const {METHOD_HOST_AT}) {METHOD_HOST:?})
  (data (i32.const {ERR_REFUSED_AT}) {ERR_REFUSED:?})

  (func $pack (param $p i32) (param $n i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $n))))
  (func $streq (param $a i32) (param $b i32) (param $n i32) (result i32)
    (local $i i32)
    (local $bad i32)
    (loop $cont
      (local.set $bad (i32.or (local.get $bad)
        (i32.ne (i32.load8_u (i32.add (local.get $a) (local.get $i)))
                (i32.load8_u (i32.add (local.get $b) (local.get $i))))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $cont (i32.lt_u (local.get $i) (local.get $n))))
    (i32.eqz (local.get $bad)))

  ;; The length is compared first, in an if rather than an i32.and, because wasm and does
  ;; not short-circuit: a guarded comparison that still read the bytes would trap on a hostile
  ;; pointer instead of falling through to the next method.
  (func $is (param $p i32) (param $n i32) (param $lit i32) (param $lit_len i32) (result i32)
    (if (result i32)
      (i32.ne (local.get $n) (local.get $lit_len))
      (then (i32.const 0))
      (else (call $streq (local.get $p) (local.get $lit) (local.get $lit_len)))))

  (func (export "ncui_abi_version") (result i32) i32.const 1)
  (func (export "ncui_manifest") (result i64)
    (call $pack (i32.const {MANIFEST_AT}) (i32.const {manifest_len})))
  (func (export "ncui_scratch") (result i64)
    (call $pack (i32.const {SCRATCH_AT}) (i32.const {SCRATCH_LEN})))

  (func (export "ncui_invoke") (param $np i32) (param $nl i32) (param $ip i32) (param $il i32)
        (result i64)
    (local $rc i32)
    (if (call $is (local.get $np) (local.get $nl) (i32.const {METHOD_ECHO_AT})
                  (i32.const {echo_len}))
      (then (return (call $pack (local.get $ip) (local.get $il)))))
    (if (call $is (local.get $np) (local.get $nl) (i32.const {METHOD_HOST_AT})
                  (i32.const {host_len}))
      (then
        (local.set $rc (call $host (i32.const {SERVICE_AT}) (i32.const {service_len})
                                   (local.get $ip) (local.get $il)
                                   (i32.const {HOST_OUT_AT}) (i32.const {HOST_OUT_LEN})))
        (if (i32.lt_s (local.get $rc) (i32.const 0))
          (then (return (call $pack (i32.const {ERR_REFUSED_AT}) (i32.const {refused_len})))))
        (return (call $pack (i32.const {HOST_OUT_AT}) (local.get $rc)))))
    (return (call $pack (i32.const {ERR_REFUSED_AT}) (i32.const {refused_len}))))
)
"#,
            manifest_len = manifest.len(),
            echo_len = METHOD_ECHO.len(),
            host_len = METHOD_HOST.len(),
            service_len = service.len(),
            refused_len = ERR_REFUSED.len(),
        )
        .into_bytes()
    }

    /// The handler behind a registered component command.
    pub(super) fn command(runtime: &Runtime, name: &str) -> CommandHandler {
        match runtime
            .page_commands
            .get(name)
            .expect("the command is registered")
            .target
        {
            CommandTarget::Native(handler) => handler,
            CommandTarget::RuntimeMethod(_) => panic!("component commands are native"),
        }
    }

    /// A runtime whose pages hold capabilities, with the component surface registered.
    ///
    /// The registrations are returned rather than dropped: a command handle is what says the
    /// surface is still installed, and a test that dropped them would be testing a runtime
    /// that has already begun to tear down.
    fn runtime_with(
        root: &Path,
        capabilities: &[Capability],
    ) -> (Runtime, Vec<CommandRegistration>) {
        let mut runtime = Runtime::start(Limits::default()).expect("a runtime starts");
        runtime.grant_page_capabilities(capabilities);
        let registrations = register(&runtime, root).expect("registration succeeds");
        (runtime, registrations)
    }

    #[test]
    fn a_component_is_loaded_from_the_application_directory_and_called() {
        let scratch = Scratch::new();
        scratch.write_artifact("notes.wasm", &mini_artifact(SERVICE, Capability::ReadLogs));
        let (runtime, _registrations) = runtime_with(&scratch.path, &[Capability::RunComponents]);
        let open = command(&runtime, "component.open");
        let summary = open(json!({"name": "notes", "artifact": "notes.wasm"})).expect("it loads");
        assert_eq!(summary["abi_version"], ncui_abi::HOST_ABI_VERSION);
        assert_eq!(
            summary["grant"],
            json!([CAP_READ_LOGS]),
            "the negotiation granted exactly what the host offered and the component asked for"
        );
        assert_eq!(summary["services"], json!([SERVICE]));

        let invoke = command(&runtime, "component.invoke");
        let answer = invoke(json!({
            "name": "notes",
            "method": "echo",
            "input": {"from": "the runtime", "n": 7}
        }))
        .expect("the call runs");
        assert_eq!(
            answer["result"],
            json!({"from": "the runtime", "n": 7}),
            "the bytes must survive the round trip through the component's memory"
        );
        assert_eq!(answer["calls"], 1);
        assert!(
            answer["fuel_used"].as_u64().expect("a number") > 0,
            "a real call burns real fuel"
        );

        let list = command(&runtime, "component.list");
        assert_eq!(list(json!({})).expect("a list")["count"], 1);
        let close = command(&runtime, "component.close");
        assert!(
            close(json!({"name": "notes"})).expect("a close")["closed"]
                .as_bool()
                .expect("a boolean")
        );
        assert_eq!(list(json!({})).expect("a list")["count"], 0);
    }

    #[test]
    fn a_component_serves_an_application_command_the_negotiation_granted() {
        let scratch = Scratch::new();
        scratch.write_artifact("notes.wasm", &mini_artifact(SERVICE, Capability::ReadLogs));
        let mut runtime = Runtime::start(Limits::default()).expect("a runtime starts");
        runtime.grant_page_capabilities(&[Capability::RunComponents, Capability::ReadLogs]);
        let counted = Arc::new(AtomicUsize::new(0));
        let handler_counted = Arc::clone(&counted);
        let _notes = runtime
            .register_page_command(
                SERVICE,
                CapabilitySet::empty().with(Capability::ReadLogs),
                "Count the notes.",
                move |_: Value| {
                    let seen = handler_counted.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"notes": seen + 1}))
                },
            )
            .expect("the application command registers");
        let registrations = register(&runtime, &scratch.path).expect("the surface registers");
        let open = command(&runtime, "component.open");
        let invoke = command(&runtime, "component.invoke");
        open(json!({"name": "notes", "artifact": "notes.wasm"})).expect("it loads");
        let answer = invoke(json!({"name": "notes", "method": "host", "input": {"q": 1}}))
            .expect("the host serves the declared service");
        assert_eq!(
            answer["result"],
            json!({"notes": 1}),
            "the component must receive the application command's own answer"
        );
        assert_eq!(counted.load(Ordering::SeqCst), 1);
        assert_eq!(answer["refusals"], 0);
        drop(registrations);
    }

    #[test]
    fn a_component_cannot_reach_a_runtime_internal_method_through_a_service() {
        // ncui.logs.query is a built-in runtime method behind the same capability this
        // component's service asks for. The capability is not the only question: the runtime's
        // own pipeline is reached as an agent, with an agent's audit trail, or not at all.
        let scratch = Scratch::new();
        scratch.write_artifact("spy.wasm", &mini_artifact(INTERNAL, Capability::ReadLogs));
        let (runtime, _registrations) = runtime_with(
            &scratch.path,
            &[Capability::RunComponents, Capability::ReadLogs],
        );
        command(&runtime, "component.open")(json!({"name": "spy", "artifact": "spy.wasm"}))
            .expect("the artifact loads: the service it declares is grantable");
        let answer = command(&runtime, "component.invoke")(json!({
            "name": "spy",
            "method": "host",
            "input": {}
        }))
        .expect("the refusal is an answer, not a failure");
        assert_eq!(
            answer["result"],
            json!({"error": "refused"}),
            "a runtime-internal method is not a component service"
        );
        assert!(answer["refusals"].as_u64().expect("a number") >= 1);
    }

    #[test]
    fn an_unauthorized_page_gets_no_component_surface_at_all() {
        let scratch = Scratch::new();
        let mut runtime = Runtime::start(Limits::default()).expect("a runtime starts");
        let registrations = register(&runtime, &scratch.path).expect("nothing to register");
        assert!(
            registrations.is_empty(),
            "without run.components there is no component surface"
        );
        assert!(
            runtime.page_commands.get("component.open").is_none(),
            "and no command was declared"
        );
        // A page that holds only a non-destructive capability still gets nothing.
        runtime.grant_page_capabilities(&[Capability::ReadLogs]);
        assert!(
            register(&runtime, &scratch.path)
                .expect("still nothing")
                .is_empty(),
            "read.logs is not permission to run code"
        );
    }

    #[test]
    fn a_component_artifact_path_cannot_escape_the_application_directory() {
        let scratch = Scratch::new();
        scratch.write_artifact("notes.wasm", &mini_artifact(SERVICE, Capability::ReadLogs));
        let escape = scratch
            .path
            .parent()
            .expect("the scratch directory has a parent");
        let outside = escape.join("outside.wasm");
        std::fs::write(&outside, mini_artifact(SERVICE, Capability::ReadLogs))
            .expect("a file outside the root is writable");
        let (runtime, _registrations) = runtime_with(&scratch.path, &[Capability::RunComponents]);
        let error = command(&runtime, "component.open")(json!({
            "name": "outside",
            "artifact": "../outside.wasm"
        }))
        .expect_err("the root contains the request");
        assert_eq!(error.code, "artifact_path_refused", "{error:?}");
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn loading_a_second_component_under_a_taken_name_is_refused() {
        let scratch = Scratch::new();
        scratch.write_artifact("a.wasm", &mini_artifact(SERVICE, Capability::ReadLogs));
        let (runtime, _registrations) = runtime_with(&scratch.path, &[Capability::RunComponents]);
        let open = command(&runtime, "component.open");
        open(json!({"name": "notes", "artifact": "a.wasm"})).expect("the first load works");
        let error = open(json!({"name": "notes", "artifact": "a.wasm"}))
            .expect_err("a name is one component");
        assert_eq!(error.code, "already_loaded", "{error:?}");
    }

    #[test]
    fn an_artifact_that_will_not_negotiate_is_refused_before_it_runs() {
        // The artifact declares a service needing `delete.data`, which no page grant of this
        // runtime includes. That import cannot be resolved, so the component is refused outright
        // rather than run with a surface it was not built against.
        const UNOFFERED: &str = "notes.delete";
        let scratch = Scratch::new();
        scratch.write_artifact(
            "wipe.wasm",
            &mini_artifact(UNOFFERED, Capability::DeleteData),
        );
        let mut runtime = Runtime::start(Limits::default()).expect("a runtime starts");
        runtime.grant_page_capabilities(&[Capability::RunComponents]);
        let registrations = register(&runtime, &scratch.path).expect("the surface registers");
        let error = command(&runtime, "component.open")(json!({
            "name": "wipe",
            "artifact": "wipe.wasm"
        }))
        .expect_err("no grant, no component");
        assert_eq!(error.code, "component_refused", "{error:?}");
        assert!(error.message.contains(UNOFFERED), "{error:?}");
        assert!(
            error.message.contains("delete.data"),
            "the refusal must name the capability that is missing: {error:?}"
        );
        assert_eq!(
            command(&runtime, "component.list")(json!({})).expect("a list")["count"],
            0,
            "and nothing was left running"
        );
        drop(registrations);
    }

    #[test]
    fn running_components_is_a_destructive_capability_with_a_stable_name() {
        // Destructive is the shape that makes the consent gate apply, and the name is the wire
        // form an application manifest writes.
        assert!(Capability::RunComponents.is_destructive());
        assert_eq!(Capability::RunComponents.as_str(), "run.components");
        assert_eq!(
            Capability::RunComponents.scope(),
            ncui_core::CapabilityScope::Runtime
        );
        assert!(
            Capability::ALL.contains(&Capability::RunComponents),
            "and it is in the table ncui describe renders"
        );
    }
}

#[cfg(test)]
mod example_tests {
    use super::tests::command;
    use super::*;
    use ncui_core::Limits;
    use serde_json::json;

    /// The artifact the counter example ships, driven through the commands the page calls.
    ///
    /// A shipped example is a claim: it says this is how an application loads a component. A
    /// claim nobody checks is how an example rots into a file that no longer runs, so this test
    /// loads the real artifact from the real example directory through the real native
    /// filesystem command and the real gate. If the generator's output or the contract changes,
    /// this fails rather than the reader of the example.
    #[test]
    fn the_example_component_loads_and_reads_a_file_through_the_runtime() {
        // Resolved before use: `SafePath` refuses a root containing `..`, which is the right
        // answer for a root and the reason the runtime resolves the application directory
        // before it hands it to any command.
        let root = std::fs::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/counter"),
        )
        .expect("the example directory exists");
        assert!(
            root.join("plugins/greeter.wat").is_file(),
            "the example's artifact must exist: {}",
            root.display()
        );
        let mut runtime = Runtime::start(Limits::default()).expect("a runtime starts");
        runtime.grant_page_capabilities(&[
            Capability::RunComponents,
            Capability::FilesystemRead,
            Capability::ReadMetrics,
        ]);
        let _native = crate::native::register(&runtime, &root, None).expect("the native surface");
        let _components = register(&runtime, &root).expect("the component surface");

        let opened = command(&runtime, "component.open")(json!({
            "name": "greeter",
            "artifact": "plugins/greeter.wat"
        }))
        .expect("the shipped artifact loads and negotiates");
        assert_eq!(opened["services"], json!(["fs.read_text"]));
        assert_eq!(
            opened["grant"],
            json!(["filesystem.read"]),
            "the component is granted what it asked for and what the host offers, and nothing else"
        );

        let answer = command(&runtime, "component.invoke")(json!({
            "name": "greeter",
            "method": "greet",
            "input": {}
        }))
        .expect("the component reaches the file through the runtime");
        let text = answer["result"]["text"]
            .as_str()
            .expect("the response carries the file's text");
        assert!(
            text.contains("filesystem.read"),
            "the component read the application's own notes file: {text}"
        );
        assert_eq!(answer["refusals"], 0);
    }
}
