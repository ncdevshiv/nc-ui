//! Components that really run: a real interpreter, a real artifact, real bytes both ways.
//!
//! The fixture lives in `common` because the runtime's component tests drive the same bytes;
//! the module documentation there explains the memory layout. What is decided here is whether
//! the host's bounds hold, and the interesting assertions are the negative ones.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

mod common;

use common::*;
use ncui_abi::HostProfile;
use ncui_component::{
    ComponentError, ComponentGateway, ComponentLimits, ComponentRuntime, RunningComponent,
};
use ncui_core::{Capability, CapabilitySet};
use std::sync::Arc;

#[test]
fn a_real_component_reads_its_own_manifest_and_answers_a_call() {
    let gateway = Arc::new(RecordingGateway::new());
    let mut component = open_with(Arc::clone(&gateway));

    // The declaration came out of the artifact, not out of this file.
    assert_eq!(component.abi_version(), "1.0");
    assert!(component.permits(Capability::ReadLogs));
    assert!(
        !component.permits(Capability::Deploy),
        "the component asked for deploy, the host never offered it, and asking is not having"
    );
    assert_eq!(
        component.grant(),
        CapabilitySet::empty().with(Capability::ReadLogs)
    );

    let answer = component
        .invoke("echo", serde_json::json!({"hello": "world", "n": 42}))
        .expect("a call");
    assert_eq!(
        answer,
        serde_json::json!({"hello": "world", "n": 42}),
        "the bytes must survive the trip through the component's own memory"
    );
    assert_eq!(component.calls(), 1);
    assert!(component.fuel_used() > 0, "a real call costs real fuel");

    // A larger document: the copy is the component's, so the bound is the host's only on the
    // way in and the way out.
    let big = serde_json::json!({"blob": "x".repeat(3_000)});
    let echoed = component.invoke("echo", big.clone()).expect("a big call");
    assert_eq!(echoed, big);
}

#[test]
fn a_component_can_ask_the_host_whether_it_holds_a_capability() {
    let gateway = Arc::new(RecordingGateway::new());
    let mut component = open_with(Arc::clone(&gateway));
    // `grant` asks the host about `read.logs`, which the negotiation granted.
    let answer = component
        .invoke("grant", serde_json::Value::Null)
        .expect("a call");
    assert_eq!(answer, serde_json::json!({"granted": true}));
    assert_eq!(component.host_calls(), 1);
    assert_eq!(component.refusals(), 0);
    // The capability question is not a service call, so the gateway never ran.
    assert_eq!(gateway.calls(), 0);
}

#[test]
fn a_component_reaches_the_host_only_for_a_service_it_declared_and_was_granted() {
    let gateway = Arc::new(RecordingGateway::new());
    let mut component = open_with(Arc::clone(&gateway));
    let answer = component
        .invoke("host", serde_json::json!({"q": "logs"}))
        .expect("the host serves the declared service");
    assert_eq!(answer["logs"], serde_json::json!(["one", "two"]));
    assert_eq!(
        answer["received"],
        serde_json::json!({"q": "logs"}),
        "the response the host wrote must be the one the component read back"
    );
    assert_eq!(gateway.calls(), 1);
    assert_eq!(
        gateway.asked(),
        vec![format!("logs.read:Some({:?})", Capability::ReadLogs)],
        "the gateway is told which capability the gate attached"
    );
    assert_eq!(component.refusals(), 0);
}

#[test]
fn a_service_the_manifest_never_declared_is_refused_rather_than_reached() {
    let gateway = Arc::new(RecordingGateway::new());
    let mut component = open_with(Arc::clone(&gateway));
    // `probe` calls the host with `not.declared`, a name absent from the manifest.
    let answer = component
        .invoke("probe", serde_json::json!({}))
        .expect("the component handles its own refusal");
    assert_eq!(answer, serde_json::json!({"error": "host_refused"}));
    assert_eq!(
        gateway.calls(),
        0,
        "an undeclared service must never reach the code that serves services"
    );
    assert!(component.refusals() >= 1);
}

#[test]
fn a_host_that_refuses_is_reported_to_the_component_as_a_code_not_a_crash() {
    let gateway = Arc::new(RecordingGateway::that_refuses());
    let mut component = open_with(Arc::clone(&gateway));
    let answer = component
        .invoke("host", serde_json::json!({}))
        .expect("a refusal is an answer");
    assert_eq!(answer, serde_json::json!({"error": "host_error"}));
    assert_eq!(gateway.calls(), 1);
}

#[test]
fn a_component_cannot_reach_the_host_while_it_is_declaring_itself() {
    // The artifact whose manifest read tries to call the host. If the declaration pass served
    // that call, the gateway would have run before any negotiation existed.
    let sneaky = component_wat(&manifest_json()).replace(
        r#"(func (export "ncui_manifest") (result i64)"#,
        r#"(func (export "ncui_manifest") (result i64)
    (drop (call $host (i32.const 576) (i32.const 9) (i32.const 512) (i32.const 9)
                      (i32.const 20480) (i32.const 2048)))"#,
    );
    assert_ne!(
        sneaky,
        component_wat(&manifest_json()),
        "the fixture must actually change"
    );
    let gateway = Arc::new(RecordingGateway::new());
    let host: Arc<dyn ComponentGateway> = gateway.clone();
    let compiled = runtime()
        .compile(sneaky.as_bytes())
        .expect("a valid artifact");
    let mut component = compiled
        .open(&read_logs_profile(), host)
        .expect("the sneaky manifest still parses");
    assert_eq!(
        gateway.calls(),
        0,
        "the declaration pass ran a service call: the gate is not closed while a component \
         explains itself"
    );
    // The live store is a different store, and it starts clean: what the declaration pass
    // refused belongs to the declaration pass. The same call that was refused while the
    // component was explaining itself is served once it has been negotiated with.
    assert_eq!(component.refusals(), 0);
    assert_eq!(
        component
            .invoke("host", serde_json::json!({"after": true}))
            .expect("the declared service is served after negotiation"),
        serde_json::json!({"logs": ["one", "two"], "received": {"after": true}})
    );
    assert_eq!(gateway.calls(), 1);
}

#[test]
fn a_component_that_does_not_return_is_stopped_by_its_fuel_budget() {
    let tight = ComponentLimits {
        fuel_per_invoke: 5_000,
        ..ComponentLimits::default()
    };
    let host = ComponentRuntime::new(tight).expect("tight but usable");
    let compiled = host.compile(&artifact()).expect("a valid artifact");
    let mut component = compiled
        .open(&read_logs_profile(), Arc::new(RecordingGateway::new()))
        .expect("negotiates");
    let error = component
        .invoke("spin", serde_json::Value::Null)
        .expect_err("an endless loop must be stopped");
    assert!(
        matches!(error, ComponentError::FuelExhausted { fuel } if fuel == 5_000),
        "got {error}"
    );
    // The store survives its own component: the next call is still a call.
    let answer = component
        .invoke("echo", serde_json::json!({"still": "here"}))
        .expect("a component is not disqualified by one trapped call");
    assert_eq!(answer, serde_json::json!({"still": "here"}));
}

#[test]
fn a_component_naming_memory_it_does_not_have_is_refused_not_panicked() {
    let gateway = Arc::new(RecordingGateway::new());
    let mut component = open_with(Arc::clone(&gateway));
    // `peek` asks the host to read 32 bytes starting 16 bytes before the end of its first page.
    let error = component
        .invoke("peek", serde_json::Value::Null)
        .expect_err("a range outside the component's memory");
    assert!(
        matches!(error, ComponentError::GuestBounds(_)),
        "got {error}"
    );
}

#[test]
fn a_component_cannot_grow_past_the_memory_it_was_offered() {
    // Two pages of allowance: the module declares a maximum of 4096 pages, so asking for 300
    // is legal for the artifact and refused by the host.
    let capped = ComponentLimits {
        max_memory_bytes: 2 * 1024 * 1024,
        ..ComponentLimits::default()
    };
    let host = ComponentRuntime::new(capped).expect("a bounded host");
    let compiled = host.compile(&artifact()).expect("a valid artifact");
    let mut component = compiled
        .open(&read_logs_profile(), Arc::new(RecordingGateway::new()))
        .expect("negotiates");
    // 300 pages is 18.75 MiB: legal for the module, over the 2 MiB this host agreed to.
    let error = component
        .invoke("grow", serde_json::Value::Null)
        .expect_err("the limiter must stop it");
    assert!(
        matches!(error, ComponentError::GuestBounds(_)),
        "got {error}, and a `-1` answer that the component ignored would mean memory the host \
         never agreed to"
    );
}

#[test]
fn a_method_the_manifest_never_advertised_cannot_be_called() {
    let gateway = Arc::new(RecordingGateway::new());
    let mut component = open_with(Arc::clone(&gateway));
    let error = component
        .invoke("not.declared", serde_json::Value::Null)
        .expect_err("the manifest is the list of what may be asked");
    let ComponentError::Undeclared { kind, name } = error else {
        panic!("expected an undeclared-method refusal, got {error}");
    };
    assert_eq!(kind, "method");
    assert_eq!(name, "not.declared");
    assert_eq!(component.calls(), 0, "a refused call is not a call");
}

#[test]
fn a_response_too_big_for_the_bound_is_refused_before_it_is_read() {
    let tiny = ComponentLimits {
        max_output_bytes: 8,
        ..ComponentLimits::default()
    };
    let host = ComponentRuntime::new(tiny).expect("a tight output bound");
    let compiled = host.compile(&artifact()).expect("a valid artifact");
    let mut component = compiled
        .open(&read_logs_profile(), Arc::new(RecordingGateway::new()))
        .expect("negotiates");
    let error = component
        .invoke("echo", serde_json::json!({"big": "y".repeat(64)}))
        .expect_err("the bound is the host's");
    assert!(
        matches!(error, ComponentError::OutputTooLarge { .. }),
        "got {error}"
    );
}

#[test]
fn a_request_too_big_for_the_bound_is_refused_before_anything_is_written() {
    let tight = ComponentLimits {
        max_input_bytes: 32,
        ..ComponentLimits::default()
    };
    let host = ComponentRuntime::new(tight).expect("a tight input bound");
    let compiled = host.compile(&artifact()).expect("a valid artifact");
    let mut component = compiled
        .open(&read_logs_profile(), Arc::new(RecordingGateway::new()))
        .expect("negotiates");
    let error = component
        .invoke("echo", serde_json::json!({"big": "z".repeat(512)}))
        .expect_err("a request body is not unlimited because it is ours");
    assert!(
        matches!(&error, ComponentError::Overlong { what, .. } if what == "request body"),
        "got {error}"
    );
}

#[test]
fn a_component_with_less_scratch_than_the_call_needs_is_told_so() {
    // The same artifact with a scratch region of eight bytes.
    let cramped = component_wat(&manifest_json()).replace(
        &format!("(call $pack (i32.const {SCRATCH_AT}) (i32.const {SCRATCH_LEN}))"),
        &format!("(call $pack (i32.const {SCRATCH_AT}) (i32.const 8))"),
    );
    let host = runtime();
    let compiled = host.compile(cramped.as_bytes()).expect("a valid artifact");
    let mut component = compiled
        .open(&read_logs_profile(), Arc::new(RecordingGateway::new()))
        .expect("negotiates");
    let error = component
        .invoke("echo", serde_json::json!({"payload": "q".repeat(64)}))
        .expect_err("eight bytes cannot hold the name and the payload");
    assert!(
        matches!(error, ComponentError::ScratchTooSmall { available, .. } if available == 8),
        "got {error}"
    );
}

#[test]
fn an_artifact_with_a_start_section_never_runs_at_all() {
    // Instantiation must not execute the artifact: a component that runs before the host has
    // decided anything about it is a component that decided for itself.
    let started = component_wat(&manifest_json()).replace(
        "  (global $out (mut i32)",
        "  (start $spin)\n  (global $out (mut i32)",
    );
    let error = runtime()
        .compile(started.as_bytes())
        .expect_err("a start section is refused");
    assert!(
        matches!(&error, ComponentError::InvalidModule(reason) if reason.to_lowercase().contains("start")),
        "got {error}"
    );
}

#[test]
fn an_artifact_importing_something_other_than_the_two_services_never_links() {
    let foreign = component_wat(&manifest_json()).replace(
        r#"  (import "ncui" "capability_check" (func $check (param i32 i32) (result i32)))"#,
        r#"  (import "ncui" "capability_check" (func $check (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
          (func $fd_write (param i32 i32 i32 i32) (result i32)))"#,
    );
    let compiled = runtime()
        .compile(foreign.as_bytes())
        .expect("it still compiles");
    let error = compiled
        .open(&read_logs_profile(), Arc::new(RecordingGateway::new()))
        .expect_err("it must not link");
    assert!(
        matches!(&error, ComponentError::UnlinkedImport(reason) if reason.contains("wasi_snapshot_preview1")),
        "got {error}"
    );
}

#[test]
fn an_artifact_built_for_another_convention_is_refused_before_negotiation() {
    let future = component_wat(&manifest_json()).replace(
        r#"(func (export "ncui_abi_version") (result i32) i32.const 1)"#,
        r#"(func (export "ncui_abi_version") (result i32) i32.const 2)"#,
    );
    let compiled = runtime().compile(future.as_bytes()).expect("it compiles");
    let error = compiled
        .declaration()
        .expect_err("but it does not declare itself to this host");
    assert!(
        matches!(
            error,
            ComponentError::AbiMismatch {
                declared: 2,
                expected: 1
            }
        ),
        "got {error}"
    );
}

#[test]
fn a_manifest_and_an_artifact_that_disagree_about_the_version_are_refused() {
    // The artifact says 1 and the document inside it says 2.0. Reading only one of the two
    // would let a component be linked against a convention it was not built for.
    let mut text = manifest_json();
    text = text.replace(r#""abi_version":"1.0""#, r#""abi_version":"2.0""#);
    assert!(
        text.contains("2.0"),
        "the manifest text must actually carry the disagreeing version"
    );
    let compiled = runtime()
        .compile(component_wat(&text).as_bytes())
        .expect("it compiles");
    let error = compiled
        .open(&read_logs_profile(), Arc::new(RecordingGateway::new()))
        .expect_err("two versions of one component is a refusal");
    assert!(
        matches!(error, ComponentError::AbiMismatch { .. }),
        "got {error}"
    );
}

#[test]
fn a_component_is_negotiated_with_the_profile_and_not_with_the_artifact() {
    // The same bytes, two profiles: the host that offers nothing grants nothing, and the
    // refusal of a forbidden import comes from the negotiation rather than the interpreter.
    let compiled = runtime().compile(&artifact()).expect("a valid artifact");
    let locked = HostProfile::locked_down("1.0");
    let error = compiled
        .open(&locked, Arc::new(RecordingGateway::new()))
        .expect_err("the service import cannot be resolved by a host that grants nothing");
    assert!(
        matches!(error, ComponentError::NotNegotiated(_)),
        "got {error}"
    );

    let unknown_abi = HostProfile::new(&["9.9"], CapabilitySet::all());
    assert!(
        matches!(
            compiled.open(&unknown_abi, Arc::new(RecordingGateway::new())),
            Err(ComponentError::NotNegotiated(_))
        ),
        "a component built for 1.0 is not adapted to a host that offers 9.9"
    );

    let negotiated = compiled
        .negotiated_with(&read_logs_profile())
        .expect("the matching profile negotiates");
    assert!(negotiated.permits(Capability::ReadLogs));
    assert!(!negotiated.permits(Capability::Deploy));
}

#[test]
fn an_oversized_artifact_is_refused_before_the_engine_sees_it() {
    let small = ComponentLimits {
        max_module_bytes: 64,
        ..ComponentLimits::default()
    };
    let host = ComponentRuntime::new(small).expect("a tiny bound");
    let error = host
        .compile(&artifact())
        .expect_err("the bytes are bigger than the bound");
    assert!(
        matches!(error, ComponentError::TooLarge { limit: 64, .. }),
        "got {error}"
    );
}

#[test]
fn a_manifest_larger_than_the_bound_is_refused_without_being_read() {
    // A huge manifest document that still parses as a manifest: the bound is about the host's
    // allocation, so the refusal must arrive before the copy.
    let padding = "w".repeat(400_000);
    let text = manifest_json().replace(
        &format!("\"name\":\"{SERVICE_TEXT}\""),
        &format!("\"name\":\"{SERVICE_TEXT}\",\"_pad\":\"{padding}\""),
    );
    assert!(text.contains("_pad"), "the padding must be in the document");
    // The default fixture declares one page of minimum memory; a document bigger than the
    // manifest bound does not fit in it, so this artifact asks for eight pages up front.
    let artifact_text = component_wat(&text).replace(
        "(memory (export \"memory\") 1 4096)",
        "(memory (export \"memory\") 16 4096)",
    );
    assert_ne!(
        artifact_text,
        component_wat(&text),
        "the fixture must really carry the oversized document"
    );
    let compiled = runtime()
        .compile(artifact_text.as_bytes())
        .expect("the artifact itself is fine");
    let error = compiled
        .declaration()
        .expect_err("the bound refuses it before the copy");
    assert!(
        matches!(error, ComponentError::ManifestTooLarge { limit, .. } if limit == ComponentLimits::default().max_manifest_bytes),
        "got {error}"
    );
}

#[test]
fn a_host_that_is_misconfigured_refuses_to_start() {
    let broken = ComponentLimits {
        max_module_bytes: 0,
        ..ComponentLimits::default()
    };
    assert!(matches!(
        ComponentRuntime::new(broken),
        Err(ComponentError::InvalidLimits(_))
    ));
}

#[test]
fn the_declaration_is_read_once_and_the_grant_survives_a_second_open() {
    let compiled = runtime().compile(&artifact()).expect("a valid artifact");
    let first = compiled.declaration().expect("read once");
    let again = compiled.declaration().expect("read from the cache");
    assert_eq!(first, again);
    assert_eq!(first.convention, 1);
    assert_eq!(first.manifest.surface.imports().len(), 1);

    let counter = Arc::new(RecordingGateway::new());
    let gateway: Arc<dyn ComponentGateway> = counter.clone();
    let mut second = compiled
        .open(&read_logs_profile(), gateway)
        .expect("a second component from the same artifact");
    // The manifest came from the cache, and the live store still reached the gateway normally.
    assert_eq!(
        second
            .invoke("host", serde_json::json!({"again": true}))
            .expect("served"),
        serde_json::json!({"logs": ["one", "two"], "received": {"again": true}})
    );
    assert_eq!(counter.calls(), 1);
}

#[test]
fn a_component_can_be_hand_a_grant_it_did_not_ask_for_and_still_loses_it() {
    // A profile offering everything narrows to the manifest's own request: `deploy` was asked
    // for, so it is granted; `filesystem.write` was not, so a permissive host does not hand it
    // out uninvited.
    let compiled = runtime().compile(&artifact()).expect("a valid artifact");
    let generous = HostProfile::permissive("1.0");
    let mut component = compiled
        .open(&generous, Arc::new(RecordingGateway::new()))
        .expect("a permissive host still negotiates");
    assert!(component.permits(Capability::ReadLogs));
    assert!(component.permits(Capability::Deploy));
    assert!(
        !component.permits(Capability::FilesystemWrite),
        "permissive means willing, not unasking"
    );
    let answer = component
        .invoke("grant", serde_json::Value::Null)
        .expect("the component can ask");
    assert_eq!(answer, serde_json::json!({"granted": true}));
}

#[test]
fn the_host_is_send_so_a_component_can_be_run_off_the_main_thread() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ComponentRuntime>();
    assert_send_sync::<ncui_component::CompiledComponent>();
    assert_send_sync::<RunningComponent>();
    let compiled = runtime().compile(&artifact()).expect("a valid artifact");
    let gateway: Arc<dyn ComponentGateway> = Arc::new(RecordingGateway::new());
    let component = compiled
        .open(&read_logs_profile(), gateway)
        .expect("negotiates");
    // And it really does run there: the store moves with it.
    let answer = std::thread::spawn(move || {
        let mut component = component;
        component
            .invoke("echo", serde_json::json!({"thread": 1}))
            .expect("a call from another thread")
    })
    .join()
    .expect("the component thread does not panic");
    assert_eq!(answer, serde_json::json!({"thread": 1}));
}
