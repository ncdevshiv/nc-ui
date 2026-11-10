//! The component artifact every test in this directory runs.
//!
//! This file is deliberately shared: `crates/ncui-runtime` includes it through a `#[path]` so
//! the runtime's component tests drive the same bytes the host's own tests do. Two copies of a
//! WebAssembly fixture is two fixtures that can disagree, and the one that disagrees is the one
//! nobody notices until a real component behaves unlike the tested one.
//!
//! The layout is fixed and named in constants, and every length the component compares against
//! is computed here from the string it came from, so a document that was truncated by a hand-
//! counted offset fails a test rather than passing a wrong one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture builders must abort loudly when a document does not assemble"
)]
// The artifact is one WebAssembly document. Splitting its construction across functions would
// put a reader in a position where no single screen shows the module the tests actually run.
#![allow(
    clippy::too_many_lines,
    reason = "one artifact, assembled in one place"
)]

use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ncui_abi::{ComponentManifest, HostProfile, Import, ImportKind, Surface};
use ncui_component::{ComponentGateway, ComponentLimits, ComponentRuntime, RunningComponent};
use ncui_core::{Capability, CapabilitySet};

// ---------------------------------------------------------------------------
// The fixture's memory layout
// ---------------------------------------------------------------------------

pub const MANIFEST_AT: u32 = 4_096;
pub const SCRATCH_AT: u32 = 12_288;
pub const SCRATCH_LEN: u32 = 4_096;
pub const HOST_OUT_AT: u32 = 20_480;
pub const HOST_OUT_LEN: u32 = 2_048;
pub const BUMP_AT: u32 = 32_768;

pub const CAP_GRANTED_TEXT: &str = "read.logs";
pub const CAP_DENIED_TEXT: &str = "deploy";
pub const SERVICE_TEXT: &str = "logs.read";
pub const UNDECLARED_TEXT: &str = "not.declared";
pub const METHOD_ECHO: &str = "echo";
pub const METHOD_GRANT: &str = "grant";
pub const METHOD_HOST: &str = "host";
pub const METHOD_PROBE: &str = "probe";
pub const METHOD_PEEK: &str = "peek";
pub const METHOD_SPIN: &str = "spin";
pub const METHOD_GROW: &str = "grow";

pub const OK_GRANTED: &str = r#"{"granted":true}"#;
pub const OK_DENIED: &str = r#"{"granted":false}"#;
pub const ERR_UNKNOWN: &str = r#"{"error":"unknown_method"}"#;
pub const ERR_REFUSED: &str = r#"{"error":"host_refused"}"#;
pub const ERR_TOO_SMALL: &str = r#"{"error":"response_too_small"}"#;
pub const ERR_HOST: &str = r#"{"error":"host_error"}"#;
pub const ERR_BAD_INPUT: &str = r#"{"error":"bad_input"}"#;

/// The manifest the default artifact carries, as JSON text.
///
/// `deploy` is requested but never offered by the profiles these tests use, so the narrowed
/// grant is observable: a component that came back with `deploy` was granted by something other
/// than the host.
pub fn manifest_json() -> String {
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
                .export(ncui_abi::Export {
                    name: METHOD_ECHO.to_owned(),
                    kind: ncui_abi::ExportKind::Function,
                })
                .export(ncui_abi::Export {
                    name: METHOD_GRANT.to_owned(),
                    kind: ncui_abi::ExportKind::Function,
                })
                .export(ncui_abi::Export {
                    name: METHOD_HOST.to_owned(),
                    kind: ncui_abi::ExportKind::Function,
                })
                .export(ncui_abi::Export {
                    name: METHOD_PROBE.to_owned(),
                    kind: ncui_abi::ExportKind::Function,
                })
                .export(ncui_abi::Export {
                    name: METHOD_PEEK.to_owned(),
                    kind: ncui_abi::ExportKind::Function,
                })
                .export(ncui_abi::Export {
                    name: METHOD_SPIN.to_owned(),
                    kind: ncui_abi::ExportKind::Function,
                })
                .export(ncui_abi::Export {
                    name: METHOD_GROW.to_owned(),
                    kind: ncui_abi::ExportKind::Function,
                }),
        );
    serde_json::to_string(&ncui_abi::WireManifest::of(&manifest)).expect("a test manifest")
}

/// A gateway that serves `logs.read` and counts what it was asked for.
pub struct RecordingGateway {
    calls: AtomicUsize,
    seen: Mutex<Vec<String>>,
    /// The answer `logs.read` gives, so a test can watch it travel.
    serve: Option<String>,
}

impl RecordingGateway {
    pub fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            serve: Some(r#"{"logs":["one","two"]}"#.to_owned()),
        }
    }

    pub fn that_refuses() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            serve: None,
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn asked(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ComponentGateway for RecordingGateway {
    fn call(
        &self,
        service: &str,
        capability: Option<Capability>,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{service}:{capability:?}"));
        if service != SERVICE_TEXT {
            return Err(format!("this gateway serves no service called `{service}`"));
        }
        // The request goes back inside the response, so a test can prove the bytes the host
        // wrote into the component are the bytes the component then sent back.
        let served = self.serve.as_ref().ok_or_else(|| "refused".to_owned())?;
        let mut value: serde_json::Value =
            serde_json::from_str(served).map_err(|error| error.to_string())?;
        value["received"] = input;
        Ok(value)
    }
}

/// The component artifact, as WAT text.
///
/// `manifest` is injected with its true length computed here rather than written by hand, and
/// the same goes for every literal the component compares against — the offsets are the
/// constants above, so the only thing a test can get wrong is the text.
pub fn component_wat(manifest: &str) -> String {
    let mut wat = format!(
        r#"
(module
  (import "ncui" "capability_check" (func $check (param i32 i32) (result i32)))
  (import "ncui" "host_call"
          (func $host (param i32 i32 i32 i32 i32 i32) (result i32)))
  ;; minimum one page, maximum 4096: the maximum is deliberately far above the host's cap so
  ;; that growing past the cap is a limiter decision and not a module-declaration one.
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

    // `echo` copies the request bytes into the component's own bump area, so the test sees a
    // document the component produced rather than one the host still holds.
    let _ = write!(
        wat,
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

    // `spin` never returns, and `grow` asks for more memory than the host will allow. Both are
    // here so that the bounds are tested against code that genuinely tries to escape them.
    let _ = write!(
        wat,
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
          ;; and the test fails on the document rather than on a silent counter.
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

/// The default artifact: the manifest this file declares, and the code above.
pub fn artifact() -> Vec<u8> {
    component_wat(&manifest_json()).into_bytes()
}

pub fn runtime() -> ComponentRuntime {
    ComponentRuntime::new(ComponentLimits::default()).expect("the default bounds are usable")
}

/// A profile that offers the ABI and the one capability the fixture's service needs.
pub fn read_logs_profile() -> HostProfile {
    HostProfile::new(&["1.0"], CapabilitySet::empty().with(Capability::ReadLogs))
}

pub fn open_with(gateway: Arc<RecordingGateway>) -> RunningComponent {
    let compiled = runtime().compile(&artifact()).expect("a valid artifact");
    compiled
        .open(&read_logs_profile(), gateway)
        .expect("the fixture negotiates")
}
