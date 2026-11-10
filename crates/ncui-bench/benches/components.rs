//! What an untrusted artifact costs, measured on the interpreter that runs it.
//!
//! A component is code this runtime did not write, shipped by somebody else, and executed
//! inside the user's process under a grant that was negotiated rather than assumed. Two
//! questions follow from that, and both are answered by numbers rather than by argument.
//!
//! * **What does it cost to admit a component?** Loading is validation, a bounded read of the
//!   artifact's own manifest through a store that refuses every host call, a negotiation, and a
//!   link. All of it is paid before the component does anything useful, so its sum is the floor
//!   of the whole interaction — the cold start a user feels when an application hands a plugin
//!   to the host.
//! * **What does it cost to let a component speak?** One call writes a request into the guest's
//!   scratch region, dispatches inside the interpreter, copies the answer, reads it back out of
//!   guest memory and parses it. When that call reaches a service it crosses the boundary twice,
//!   once to read the request and once to write the reply.
//!
//! These are the numbers that bound an untrusted artifact. Fuel says how long a component may
//! think, the copy budget says how much of the host's allocator it may claim, and the memory cap
//! says how far it may grow — and none of those three is a schedule. A caller that has to fit
//! component work into a frame needs a duration, and a duration is precisely what a bound on
//! instruction count does not give you. The same figure decides whether an instance is worth
//! keeping alive between calls or cheap enough to rebuild on every one, which is a question you
//! cannot even ask without knowing what each of the two costs.
//!
//! A component host whose cost is unknown is a component host nobody can schedule. Without a
//! measurement, every decision about when to load, whether to retain, and how many calls fit in
//! a frame becomes a guess — and a guess about a boundary that leads into an interpreter is a
//! guess about an adversary's budget as much as about the machine's. This file exists so that
//! the guess is replaced by a number that is re-derived on every run rather than transcribed
//! from a document.
//!
//! The artifact under measurement is the same hand-assembled `WAT` the component integration
//! tests use, so the interpreter, the refusing declaration store, the gate and the memory
//! limiter are the ones that ship. Nothing here is a stand-in for them: `component.compile` is
//! wasmi validating real bytes, `component.declaration` runs real guest code to read real
//! bytes out of real guest memory, and the two invoke scenarios move JSON across the boundary
//! in both directions.
//!
//! Run with `cargo bench -p ncui-bench --bench components`. Add `-- --sample-size 10` for a
//! quick pass over every scenario, and `-- --save-baseline <name>` to keep one for comparison.

// The artifact is one WebAssembly document: splitting its construction across functions would
// leave no single screen showing the module the numbers were taken from.
#![allow(
    clippy::too_many_lines,
    reason = "one artifact, assembled in one place"
)]

use std::fmt::Write as _;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
// `std::hint::black_box` rather than criterion's deprecated re-export: the workspace builds
// with warnings denied, and a deprecated call is a defect here like any other.
use ncui_abi::wire::WireManifest;
use ncui_abi::{ComponentManifest, Export, ExportKind, HostProfile, Import, ImportKind, Surface};
use ncui_component::{
    CompiledComponent, ComponentGateway, ComponentLimits, ComponentRuntime, RunningComponent,
};
use ncui_core::{Capability, CapabilitySet};
use serde_json::Value;
use std::hint::black_box;

/// Samples taken for the groups that build an artifact. Each iteration compiles, so an
/// iteration is milliseconds and thirty of them describe the distribution well enough to watch.
const LOADING_SAMPLES: usize = 30;
/// Samples taken for the groups that call an already-open component. An iteration there is a
/// microsecond-scale call, and the path a caller repeats is the one worth resolving finely.
const CALLING_SAMPLES: usize = 200;

/// How many times the measured instance is told to grow before it is refused.
///
/// The bound exists so that a change to [`ComponentLimits`] cannot turn the setup into a loop;
/// the refusal is the real stopping condition, and it always arrives.
const GROW_ATTEMPTS: usize = 8;

// ---------------------------------------------------------------------------
// The fixture's memory layout
//
// Copied from the component suite rather than reinvented: the offsets appear inside the
// artifact's own instructions, so a length that was guessed rather than computed would show up
// as a truncated document, and a scenario that measures a failing call measures nothing.
// ---------------------------------------------------------------------------

const MANIFEST_AT: u32 = 4_096;
const SCRATCH_AT: u32 = 12_288;
const SCRATCH_LEN: u32 = 4_096;
const HOST_OUT_AT: u32 = 20_480;
const HOST_OUT_LEN: u32 = 2_048;
const BUMP_AT: u32 = 32_768;

const CAP_GRANTED_TEXT: &str = "read.logs";
const CAP_DENIED_TEXT: &str = "deploy";
const SERVICE_TEXT: &str = "logs.read";
const UNDECLARED_TEXT: &str = "not.declared";
const METHOD_ECHO: &str = "echo";
const METHOD_GRANT: &str = "grant";
const METHOD_HOST: &str = "host";
const METHOD_PROBE: &str = "probe";
const METHOD_PEEK: &str = "peek";
const METHOD_SPIN: &str = "spin";
const METHOD_GROW: &str = "grow";

const OK_GRANTED: &str = r#"{"granted":true}"#;
const OK_DENIED: &str = r#"{"granted":false}"#;
const ERR_UNKNOWN: &str = r#"{"error":"unknown_method"}"#;
const ERR_REFUSED: &str = r#"{"error":"host_refused"}"#;
const ERR_TOO_SMALL: &str = r#"{"error":"response_too_small"}"#;
const ERR_HOST: &str = r#"{"error":"host_error"}"#;
const ERR_BAD_INPUT: &str = r#"{"error":"bad_input"}"#;

/// The manifest the measured artifact carries, as JSON text.
///
/// `deploy` is requested and never offered, which is what makes the grant in
/// `component.negotiate_and_open` an intersection rather than a copy.
fn manifest_json() -> String {
    let manifest = ComponentManifest::new("1.0")
        .requesting(
            CapabilitySet::empty()
                .with(Capability::ReadLogs)
                .with(Capability::Deploy),
        )
        .with_surface(
            Surface::new()
                .import(Import {
                    module: "ncui".to_owned(),
                    name: SERVICE_TEXT.to_owned(),
                    kind: ImportKind::CapabilityCall,
                    requires: Some(Capability::ReadLogs),
                })
                .export(Export {
                    name: METHOD_ECHO.to_owned(),
                    kind: ExportKind::Function,
                })
                .export(Export {
                    name: METHOD_GRANT.to_owned(),
                    kind: ExportKind::Function,
                })
                .export(Export {
                    name: METHOD_HOST.to_owned(),
                    kind: ExportKind::Function,
                })
                .export(Export {
                    name: METHOD_PROBE.to_owned(),
                    kind: ExportKind::Function,
                })
                .export(Export {
                    name: METHOD_PEEK.to_owned(),
                    kind: ExportKind::Function,
                })
                .export(Export {
                    name: METHOD_SPIN.to_owned(),
                    kind: ExportKind::Function,
                })
                .export(Export {
                    name: METHOD_GROW.to_owned(),
                    kind: ExportKind::Function,
                }),
        );
    serde_json::to_string(&WireManifest::of(&manifest))
        .unwrap_or_else(|error| fail(&format!("the bench manifest could not be written: {error}")))
}

/// A host that answers `logs.read`, and counts that it was asked.
///
/// The gateway is deliberately small. Its job in `component.invoke_with_host_call` is to be a
/// host that replies, so that the figure is the boundary's cost plus one document clone rather
/// than the cost of whatever a real service does; a slow service would swallow the measurement
/// the scenario exists to make.
#[derive(Debug)]
struct ServingGateway {
    calls: AtomicUsize,
    served: Value,
}

impl Default for ServingGateway {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            served: serde_json::json!({"logs": ["one", "two"]}),
        }
    }
}

impl ServingGateway {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl ComponentGateway for ServingGateway {
    fn call(
        &self,
        service: &str,
        capability: Option<Capability>,
        input: Value,
    ) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if service != SERVICE_TEXT {
            return Err(format!("this gateway serves no service called `{service}`"));
        }
        // The capability the gate attached is carried into the answer's shape rather than
        // dropped, so the cost of passing it is inside the measurement rather than outside it.
        let mut answer = self.served.clone();
        answer["received"] = input;
        answer["under"] = serde_json::json!(capability.map_or("none", Capability::as_str));
        Ok(answer)
    }
}

/// The component artifact, as `WAT` text.
///
/// `manifest` is injected with its true length computed here rather than written by hand, and
/// the same goes for every literal the component compares against — the offsets are the
/// constants above, so the only thing this file can get wrong is the text.
fn component_wat(manifest: &str) -> String {
    let mut wat = format!(
        r#"
(module
  (import "ncui" "capability_check" (func $check (param i32 i32) (result i32)))
  (import "ncui" "host_call"
          (func $host (param i32 i32 i32 i32 i32 i32) (result i32)))
  ;; minimum one page, maximum 4096: the maximum is far above the host's cap so that growing is
  ;; a limiter decision and not a module-declaration one, which is what `extend_room` uses.
  (memory (export "memory") 1 4096)
  (data (i32.const {MANIFEST_AT}) {manifest:?})
  (data (i32.const 512) {CAP_GRANTED_TEXT:?})
  (data (i32.const 544) {CAP_DENIED_TEXT:?})
  (data (i32.const 576) {SERVICE_TEXT:?})
  (data (i32.const 608) {UNDECLARED_TEXT:?})
  (data (i32.const 640) {METHOD_ECHO:?})
  (data (i32.const 672) {METHOD_GRANT:?})
  (data (i32.const 704) {METHOD_HOST:?})
  (data (i32.const 736) {METHOD_PROBE:?})
  (data (i32.const 768) {METHOD_PEEK:?})
  (data (i32.const 800) {METHOD_SPIN:?})
  (data (i32.const 832) {METHOD_GROW:?})
  (data (i32.const 864) {OK_GRANTED:?})
  (data (i32.const 928) {OK_DENIED:?})
  (data (i32.const 992) {ERR_UNKNOWN:?})
  (data (i32.const 1056) {ERR_REFUSED:?})
  (data (i32.const 1120) {ERR_TOO_SMALL:?})
  (data (i32.const 1184) {ERR_HOST:?})
  (data (i32.const 1248) {ERR_BAD_INPUT:?})
  (global $out (mut i32) (i32.const {BUMP_AT}))

  (func $pack (param $p i32) (param $n i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $n))))

  ;; Byte-for-byte comparison. The caller always guards the length first, because
  ;; `i32.and` does not short-circuit and a guarded call that still read the bytes would
  ;; trap on a hostile pointer instead of falling through.
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

  (func $is (param $p i32) (param $n i32) (param $lit i32) (param $lit_len i32) (result i32)
    (i32.and (i32.eq (local.get $n) (local.get $lit_len))
             (call $streq (local.get $p) (local.get $lit) (local.get $lit_len))))

  (func (export "ncui_abi_version") (result i32) i32.const 1)
  (func (export "ncui_manifest") (result i64)
    (call $pack (i32.const {MANIFEST_AT}) (i32.const {manifest_len})))
  (func (export "ncui_scratch") (result i64)
    (call $pack (i32.const {SCRATCH_AT}) (i32.const {SCRATCH_LEN})))
"#,
        manifest_len = manifest.len(),
    );

    // `echo` copies the request bytes into the component's own bump area, so the measured call
    // is a document the component produced rather than one the host still holds.
    wat.push_str(
        r"
  (func $echo (param $p i32) (param $n i32) (result i64)
    (local $start i32)
    (local.set $start (global.get $out))
    (global.set $out
      (i32.and (i32.add (i32.add (local.get $start) (local.get $n)) (i32.const 8))
               (i32.xor (i32.const 7) (i32.const -1))))
    (memory.copy (local.get $start) (local.get $p) (local.get $n))
    (call $pack (local.get $start) (local.get $n)))
",
    );

    // `spin` never returns and `grow` asks for memory. Both are here because the bounds they
    // exercise are the ones the host is argued from, and `grow` is the only way an instance can
    // be given room for a long measurement.
    wat.push_str(
        r"
  (func $spin
    (loop $l (br $l)))
  (func $grow (param $pages i32)
    (drop (memory.grow (local.get $pages))))
",
    );

    let _ = write!(
        wat,
        r#"
  ;; The one entry point: dispatch on the method name the host wrote into the scratch region.
  (func (export "ncui_invoke") (param $np i32) (param $nl i32) (param $ip i32) (param $il i32)
        (result i64)
    (local $rc i32)
    (if (call $is (local.get $np) (local.get $nl) (i32.const {ECHO_AT}) (i32.const {ECHO_LEN}))
      (then (return (call $echo (local.get $ip) (local.get $il)))))
    (if (call $is (local.get $np) (local.get $nl) (i32.const {GRANT_AT}) (i32.const {GRANT_LEN}))
      (then
        (local.set $rc (call $check (i32.const {CAP_GRANTED_AT}) (i32.const {CAP_GRANTED_LEN})))
        (return
          (call $pack
            (select (i32.const {OK_GRANTED_AT}) (i32.const {OK_DENIED_AT}) (local.get $rc))
            (select (i32.const {OK_GRANTED_LEN}) (i32.const {OK_DENIED_LEN})
                    (local.get $rc))))))
    (if (call $is (local.get $np) (local.get $nl) (i32.const {HOST_AT}) (i32.const {HOST_LEN}))
      (then
        (local.set $rc
          (call $host (i32.const {SERVICE_AT}) (i32.const {SERVICE_LEN})
                      (local.get $ip) (local.get $il)
                      (i32.const {HOST_OUT_AT_I}) (i32.const {HOST_OUT_LEN_I})))
        (if (i32.lt_s (local.get $rc) (i32.const 0))
          (then
            (if (i32.eq (local.get $rc) (i32.const -1))
              (then (return (call $pack (i32.const {ERR_SMALL_AT}) (i32.const {ERR_SMALL_LEN})))))
            (if (i32.eq (local.get $rc) (i32.const -3))
              (then (return (call $pack (i32.const {ERR_HOST_AT}) (i32.const {ERR_HOST_LEN})))))
            (if (i32.eq (local.get $rc) (i32.const -4))
              (then (return (call $pack (i32.const {ERR_BAD_AT}) (i32.const {ERR_BAD_LEN})))))
            (return (call $pack (i32.const {ERR_REFUSED_AT}) (i32.const {ERR_REFUSED_LEN})))))
        (return (call $pack (i32.const {HOST_OUT_AT_I}) (local.get $rc)))))
    (if (call $is (local.get $np) (local.get $nl) (i32.const {PROBE_AT}) (i32.const {PROBE_LEN}))
      (then
        (local.set $rc
          (call $host (i32.const {UNDECLARED_AT}) (i32.const {UNDECLARED_LEN})
                      (local.get $ip) (local.get $il)
                      (i32.const {HOST_OUT_AT_I}) (i32.const {HOST_OUT_LEN_I})))
        (if (i32.ge_s (local.get $rc) (i32.const 0))
          ;; A service the manifest never declared was served. The component says so loudly,
          ;; and the scenario fails on the document rather than on a silent counter.
          (then (return (call $pack (i32.const {OK_DENIED_AT}) (i32.const {OK_DENIED_LEN})))))
        (return (call $pack (i32.const {ERR_REFUSED_AT}) (i32.const {ERR_REFUSED_LEN})))))
    (if (call $is (local.get $np) (local.get $nl) (i32.const {PEEK_AT}) (i32.const {PEEK_LEN}))
      (then
        ;; A range that runs off the end of the component's own memory.
        (drop (call $check (i32.const 65520) (i32.const 32)))
        (return (call $pack (i32.const {OK_GRANTED_AT}) (i32.const {OK_GRANTED_LEN})))))
    (if (call $is (local.get $np) (local.get $nl) (i32.const {SPIN_AT}) (i32.const {SPIN_LEN}))
      (then (call $spin)
            (return (call $pack (i32.const {OK_GRANTED_AT}) (i32.const {OK_GRANTED_LEN})))))
    (if (call $is (local.get $np) (local.get $nl) (i32.const {GROW_AT}) (i32.const {GROW_LEN}))
      (then (call $grow (i32.const 300))
            (return (call $pack (i32.const {OK_GRANTED_AT}) (i32.const {OK_GRANTED_LEN})))))
    (return (call $pack (i32.const {ERR_UNKNOWN_AT}) (i32.const {ERR_UNKNOWN_LEN}))))
)
"#,
        ECHO_AT = 640,
        ECHO_LEN = METHOD_ECHO.len(),
        GRANT_AT = 672,
        GRANT_LEN = METHOD_GRANT.len(),
        HOST_AT = 704,
        HOST_LEN = METHOD_HOST.len(),
        PROBE_AT = 736,
        PROBE_LEN = METHOD_PROBE.len(),
        PEEK_AT = 768,
        PEEK_LEN = METHOD_PEEK.len(),
        SPIN_AT = 800,
        SPIN_LEN = METHOD_SPIN.len(),
        GROW_AT = 832,
        GROW_LEN = METHOD_GROW.len(),
        CAP_GRANTED_AT = 512,
        CAP_GRANTED_LEN = CAP_GRANTED_TEXT.len(),
        SERVICE_AT = 576,
        SERVICE_LEN = SERVICE_TEXT.len(),
        UNDECLARED_AT = 608,
        UNDECLARED_LEN = UNDECLARED_TEXT.len(),
        OK_GRANTED_AT = 864,
        OK_GRANTED_LEN = OK_GRANTED.len(),
        OK_DENIED_AT = 928,
        OK_DENIED_LEN = OK_DENIED.len(),
        ERR_UNKNOWN_AT = 992,
        ERR_UNKNOWN_LEN = ERR_UNKNOWN.len(),
        ERR_REFUSED_AT = 1056,
        ERR_REFUSED_LEN = ERR_REFUSED.len(),
        ERR_SMALL_AT = 1120,
        ERR_SMALL_LEN = ERR_TOO_SMALL.len(),
        ERR_HOST_AT = 1184,
        ERR_HOST_LEN = ERR_HOST.len(),
        ERR_BAD_AT = 1248,
        ERR_BAD_LEN = ERR_BAD_INPUT.len(),
        HOST_OUT_AT_I = HOST_OUT_AT,
        HOST_OUT_LEN_I = HOST_OUT_LEN,
    );
    wat
}

/// The measured artifact: the manifest above, compiled by the engine from `WAT` text.
fn artifact() -> Vec<u8> {
    component_wat(&manifest_json()).into_bytes()
}

/// The bounds the measured host runs under.
///
/// Only the memory cap differs from the defaults, and only because of `extend_room`: the
/// artifact's `echo` answers from a bump area that never moves backwards, so an instance that
/// is reused across a whole measurement needs room for every call it will be asked to make.
/// Leaving the cap at its default would end the loop by measuring a trap, which is a number
/// about nothing.
fn bench_limits() -> ComponentLimits {
    ComponentLimits {
        max_memory_bytes: 64 * 1024 * 1024,
        ..ComponentLimits::default()
    }
}

fn host() -> ComponentRuntime {
    ComponentRuntime::new(bench_limits())
        .unwrap_or_else(|error| fail(&format!("the bench host's bounds are unusable: {error}")))
}

/// A profile that offers the ABI and the one capability the artifact's service needs.
fn read_logs_profile() -> HostProfile {
    HostProfile::new(&["1.0"], CapabilitySet::empty().with(Capability::ReadLogs))
}

fn gateway() -> Arc<dyn ComponentGateway> {
    Arc::new(ServingGateway::default())
}

/// Compiles the artifact afresh.
///
/// Freshness is the point: the declaration is cached on the compiled artifact, so a scenario
/// that reused one would measure a `OnceLock` rather than the guest-code read that produced it.
fn compile_fresh(runtime: &ComponentRuntime, bytes: &[u8]) -> CompiledComponent {
    runtime
        .compile(bytes)
        .unwrap_or_else(|error| fail(&format!("the bench artifact would not compile: {error}")))
}

/// Grows the instance through its own declared method until the host's cap refuses.
///
/// The refusal is the stopping condition rather than a failure: it is the limiter answering the
/// way the specification says it must, and the loop ends the moment it does. Everything here is
/// outside the measured region, so the price of the room is not charged to the call.
fn extend_room(component: &mut RunningComponent) {
    for _ in 0..GROW_ATTEMPTS {
        if component.invoke(METHOD_GROW, Value::Null).is_err() {
            return;
        }
    }
}

/// Opens the artifact, gives it room, and proves it behaves before anything is timed.
///
/// A scenario that silently measured a failing call would be worse than no scenario: the number
/// would look plausible and describe an error path. So both directions across the boundary are
/// exercised once here, the answers are compared with the ones the contract promises, and the
/// run stops if they are not what they should be.
fn opened_component() -> RunningComponent {
    let runtime = host();
    let compiled = compile_fresh(&runtime, &artifact());
    let served = Arc::new(ServingGateway::default());
    let mut component = compiled
        .open(&read_logs_profile(), served.clone())
        .unwrap_or_else(|error| fail(&format!("the bench artifact would not open: {error}")));

    let echoed = component
        .invoke(METHOD_ECHO, serde_json::json!({"check": 1}))
        .unwrap_or_else(|error| fail(&format!("the bench echo call failed: {error}")));
    if echoed != serde_json::json!({"check": 1}) {
        fail("the artifact's echo did not return the bytes the host wrote");
    }

    let served_answer = component
        .invoke(METHOD_HOST, serde_json::json!({"check": 1}))
        .unwrap_or_else(|error| fail(&format!("the bench host call failed: {error}")));
    if served_answer.get("logs").is_none() || served.calls() != 1 {
        fail("the declared service did not reach the gateway exactly once");
    }

    extend_room(&mut component);
    let after = component
        .invoke(METHOD_ECHO, serde_json::json!({"room": 1}))
        .unwrap_or_else(|error| fail(&format!("the grown instance stopped answering: {error}")));
    if after != serde_json::json!({"room": 1}) {
        fail("the grown instance no longer echoes what it is handed");
    }
    component
}

/// Validation and compilation of the artifact's bytes, with no guest code running yet.
///
/// This is the cost a host pays for every component it is handed, and the one bound that an
/// attacker controls directly: the artifact's size and its instruction density are chosen by
/// whoever shipped it. `ComponentLimits::max_module_bytes` caps the bytes; this says what the
/// bytes then cost.
fn compile_cost(c: &mut Criterion) {
    let runtime = host();
    let bytes = artifact();
    c.bench_function("component.compile", |b| {
        b.iter(|| {
            let compiled = compile_fresh(&runtime, &bytes);
            // Nothing is read out of the artifact on purpose: reading its declaration would fold
            // `component.declaration` into this figure. Dropping it here is honest too — a host
            // that loads a component and then refuses it pays the teardown as well.
            let _ = black_box(&compiled);
        });
    });
}

/// The bounded read of the manifest through the refusing store.
///
/// The headline of this scenario is that reading a component's self-description *runs the
/// component*. It costs an instantiation of a second store, three guest calls, a bounded copy
/// out of guest memory and a wire parse, and it is paid before the negotiation has decided
/// anything — which is exactly why the store it runs in can reach nothing. The result is cached,
/// so the artifact is recompiled outside the measured region and this is the cold cost, not a
/// `OnceLock` hit.
fn declaration_cost(c: &mut Criterion) {
    let runtime = host();
    let bytes = artifact();
    c.bench_function("component.declaration", |b| {
        b.iter_batched(
            || compile_fresh(&runtime, &bytes),
            |compiled| {
                let declaration = compiled.declaration().unwrap_or_else(|error| {
                    fail(&format!("the artifact would not declare itself: {error}"))
                });
                declaration.manifest.surface.imports().len()
                    + declaration.manifest.surface.exports().len()
            },
            BatchSize::PerIteration,
        );
    });
}

/// Negotiation plus the live link: everything between a compiled artifact and a callable one.
///
/// This is the second instantiation, the one that carries the grant and the gateway. Along with
/// `component.compile` it is the whole admission cost, and their ratio is the argument for any
/// cache a host might keep: if opening dominates, retaining a compiled artifact is worth the
/// memory; if it does not, the cache is buying very little.
fn open_cost(c: &mut Criterion) {
    let runtime = host();
    let bytes = artifact();
    let profile = read_logs_profile();
    let gateway = gateway();
    c.bench_function("component.open", |b| {
        b.iter_batched(
            || compile_fresh(&runtime, &bytes),
            |compiled| {
                let component = compiled
                    .open(&profile, Arc::clone(&gateway))
                    .unwrap_or_else(|error| {
                        fail(&format!("the bench artifact would not open: {error}"))
                    });
                // Reading the grant back proves the instance is the negotiated one rather than
                // an object the optimiser is free to discard.
                usize::from(component.permits(Capability::ReadLogs)) + component.methods().len()
            },
            BatchSize::PerIteration,
        );
    });
}

/// The gate on its own and then with the instantiation, so the two are separable.
///
/// `ncui-abi`'s negotiation is already measured as `abi.negotiate`, in microseconds on a
/// manifest with far more imports than this artifact declares. What is not known until this
/// scenario runs is whether the gate matters next to the interpreter at all. It is the question
/// a reviewer asks when someone proposes to make negotiation stricter: the answer is the
/// difference between these two figures and `component.open`.
fn negotiate_and_open_cost(c: &mut Criterion) {
    let runtime = host();
    let bytes = artifact();
    let profile = read_logs_profile();
    let gateway = gateway();
    c.bench_function("component.negotiate_and_open", |b| {
        b.iter_batched(
            || compile_fresh(&runtime, &bytes),
            |compiled| {
                let negotiated = compiled.negotiated_with(&profile).unwrap_or_else(|error| {
                    fail(&format!("the gate refused the artifact: {error}"))
                });
                let resolvable = negotiated.resolvable_imports;
                // The declaration the gate just read is cached, so the open that follows is the
                // instantiation alone: this is the sequence a host that has already decided to
                // trust an artifact goes through, not two cold reads.
                let component = compiled
                    .open(&profile, Arc::clone(&gateway))
                    .unwrap_or_else(|error| fail(&format!("the artifact would not open: {error}")));
                resolvable
                    + usize::from(component.permits(Capability::ReadLogs))
                    + usize::from(!component.permits(Capability::Deploy))
            },
            BatchSize::PerIteration,
        );
    });
}

/// One call across the boundary, with the instance kept alive.
///
/// Reuse is the whole design of this scenario: an instance is stateful, so the alternative —
/// opening per iteration — would measure `component.open` again and hide the thing a caller
/// actually asks, which is how many components can be called before a frame is gone. What is
/// timed is the call path end to end: the scratch read, the request serialisation, two writes
/// into guest memory, the dispatch, the copy the component makes inside itself, the packed
/// location read back and the JSON parse.
fn invoke_echo_cost(c: &mut Criterion) {
    let mut component = opened_component();
    let input = serde_json::json!({"n": 1});
    c.bench_function("component.invoke_echo", |b| {
        b.iter(|| {
            let answer = component
                .invoke(METHOD_ECHO, black_box(input.clone()))
                .unwrap_or_else(|error| fail(&format!("the bench echo call failed: {error}")));
            answer.as_object().map_or(0, serde_json::Map::len)
        });
    });
}

/// One call that goes out to the host and comes back.
///
/// This is the expensive half of the boundary and the one an adversary chooses: the guest reads
/// its request out of its own memory, the host reads the service name and the body out of the
/// same memory, the gateway answers, the host serialises and *writes* the reply back into the
/// component's memory, and the component reads it again. The gap between this and
/// `component.invoke_echo` is what a host service costs a component, and it is the figure a
/// caller should use to decide how many service-backed calls belong in one frame.
fn invoke_with_host_call_cost(c: &mut Criterion) {
    let mut component = opened_component();
    let input = serde_json::json!({"q": "logs"});
    c.bench_function("component.invoke_with_host_call", |b| {
        b.iter(|| {
            let answer = component
                .invoke(METHOD_HOST, black_box(input.clone()))
                .unwrap_or_else(|error| fail(&format!("the bench host call failed: {error}")));
            answer
                .get("logs")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        });
    });
}

criterion_group! {
    name = admitting;
    config = Criterion::default()
        .sample_size(LOADING_SAMPLES)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(700));
    targets = compile_cost, declaration_cost, open_cost, negotiate_and_open_cost
}

criterion_group! {
    name = calling;
    config = Criterion::default()
        .sample_size(CALLING_SAMPLES)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(900));
    targets = invoke_echo_cost, invoke_with_host_call_cost
}

criterion_main!(admitting, calling);

/// Stops the run with the reason, on the harness's own stream.
///
/// A bench binary has no test harness to fail through, and a measurement taken from a component
/// that refused to load is not a measurement. So every fixture step that can fail routes here
/// rather than swallowing the error or panicking: the run exits non-zero and names the failure,
/// which is the only honest outcome for a scenario that did not run.
fn fail(message: &str) -> ! {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "component benchmark failure: {message}");
    std::process::exit(1);
}
