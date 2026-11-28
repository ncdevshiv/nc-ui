//! Native-window host-path benchmarks, measured with Criterion.
//!
//! The native-view path has three separate boundaries whose costs must not be conflated:
//!
//! * `ncui-webview` owns the wire/contract types. A command is bounds-checked before the
//!   GPUI host will accept it, and the same serde implementations are what the value uses
//!   when it is represented as JSON. The in-process GPUI command channel itself moves the
//!   structured command rather than a serialized byte string, so validation and serde
//!   serialization are measured separately here instead of being added together and
//!   called a dispatch round-trip.
//! * Events use the same contract types, but the bounded queue is provider-side. The real
//!   queue implementation is `ncui_gpui::NativeEventSink`, exported only when the
//!   `gpui-engine` feature is compiled. The default bench therefore measures the public
//!   contract cost in `ncui-webview` — bounds checking and serde serialization at several
//!   payload sizes — while `--features native-gpui` adds the real sink's emit/drain and
//!   full-queue drop behaviour.
//! * `ncui-kit-providers` does not currently expose a validation-only method on its
//!   public provider object: the concrete widget types are crate-private, and the public
//!   `NativeViewProvider` surface reaches props validation through `build`. With
//!   `--features kit-providers`, rejection benchmarks therefore call the real registered
//!   provider's real `build` implementation inside gpui's own headless test application.
//!   Invalid and unknown-field cases are exact rejection measurements because validation
//!   fails before a child is constructed. Accepted-props cases are labelled as build
//!   upper bounds, never as pure validation, because the public surface cannot separate
//!   successful validation from entity construction without changing another crate.
//!
//! No validation logic is copied or reimplemented here. Every measured operation invokes
//! the production type, provider, or queue named above. The gpui test application is used
//! only because the public trait requires a `Window`/`App` pair to reach `build`; it is
//! gpui's own test platform, not a local stand-in for provider logic.
//!
//! Run with `cargo bench -p ncui-bench --bench native_views`. Add
//! `--features native-gpui` for the bounded event sink, or `--features kit-providers`
//! for the provider measurements. Pass `-- --quick` for a fast statistical pass.

use std::hint::black_box;
use std::io::Write as _;
use std::time::Duration;

#[cfg(any(feature = "native-gpui", feature = "kit-providers"))]
use criterion::BatchSize;
#[cfg(feature = "native-gpui")]
use criterion::Throughput;
use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, criterion_group, criterion_main};
use ncui_webview::native::MAX_NATIVE_NAME_BYTES;
use ncui_webview::{
    MAX_NATIVE_PAYLOAD_BYTES, MAX_NATIVE_PROPS_BYTES, NativeViewCommand, NativeViewEvent,
    WindowRequest,
};
use serde_json::Value;

#[cfg(feature = "native-gpui")]
use ncui_webview::MAX_QUEUED_NATIVE_EVENTS;

#[cfg(feature = "kit-providers")]
use gpui::{EmptyView, TestAppContext, VisualTestContext};
#[cfg(feature = "kit-providers")]
use ncui_core::WindowId;
#[cfg(feature = "kit-providers")]
use ncui_gpui::{NativeEventSink, NativeViewContext, NativeViewProvider, providers};
#[cfg(feature = "kit-providers")]
use std::sync::Arc;

/// A benchmark that cannot set itself up is a broken benchmark, not a slow one. This is
/// the bench files' established alternative to `expect`: exit non-zero and say why.
fn fail(message: &str) -> ! {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "native-views benchmark failure: {message}");
    std::process::exit(1);
}

/// The serialized length of one fixture value, with serialisation failure treated as a
/// broken fixture rather than as a benchmark result.
fn serialized_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or_else(
        |error| fail(&format!("a fixture value could not be serialised: {error}")),
        |text| text.len(),
    )
}

/// A JSON string whose serialized form is exactly `bytes` long.
///
/// The fixture uses only ASCII `x`, so the serialized size is the content plus two quote
/// bytes. Computing it rather than hard-coding a repeat count keeps the boundary fixtures
/// exact when the boundary constant changes.
fn json_string_with_serialized_len(bytes: usize) -> Value {
    if bytes < 2 {
        fail("a serialized JSON string cannot be shorter than its two quotes");
    }
    let value = Value::String("x".repeat(bytes - 2));
    let actual = serialized_len(&value);
    if actual != bytes {
        fail(&format!(
            "the string fixture serialized to {actual} bytes, not the requested {bytes}"
        ));
    }
    value
}

/// A one-field object whose serialized form is exactly `bytes` long.
///
/// This is the large-props fixture. One ASCII string field keeps the size arithmetic
/// exact while still making serde walk and validate a real JSON object rather than a bare
/// string standing in for props.
fn object_props_with_serialized_len(bytes: usize) -> Value {
    let empty = serde_json::json!({"data": ""});
    let overhead = serialized_len(&empty);
    if bytes < overhead {
        fail(&format!(
            "the object fixture needs at least {overhead} bytes, not {bytes}"
        ));
    }
    let value = serde_json::json!({"data": "x".repeat(bytes - overhead)});
    let actual = serialized_len(&value);
    if actual != bytes {
        fail(&format!(
            "the object fixture serialized to {actual} bytes, not the requested {bytes}"
        ));
    }
    value
}

fn bench_command_case(
    group: &mut BenchmarkGroup<'_, WallTime>,
    case: &str,
    command: &NativeViewCommand,
) {
    group.bench_function(format!("{case}/validate"), |b| {
        b.iter(|| black_box(command).is_within_bounds());
    });
    group.bench_function(format!("{case}/serialize"), |b| {
        b.iter(|| serde_json::to_string(black_box(command)).is_ok());
    });
    group.bench_function(format!("{case}/validate_then_serialize"), |b| {
        b.iter(|| {
            black_box(command).is_within_bounds()
                && serde_json::to_string(black_box(command)).is_ok()
        });
    });
}

/// Native command validation and serialization.
///
/// `is_within_bounds` is the production check `ncui-gpui` performs in
/// `GpuiWindowHost::send_view_command` before a command is accepted. The serde side is
/// the derived implementation on the same production enum. The large fixtures sit exactly
/// at the 64 KiB payload bound, so the measured path is the largest command the boundary
/// accepts rather than an approximation of one.
fn command_validation_and_serialization(c: &mut Criterion) {
    let small_props = serde_json::json!({
        "label": "Save",
        "variant": "primary",
        "size": "md",
        "disabled": false,
        "full_width": false,
    });
    let bound_props = object_props_with_serialized_len(MAX_NATIVE_PAYLOAD_BYTES);
    let over_bound_props = object_props_with_serialized_len(MAX_NATIVE_PAYLOAD_BYTES + 1);

    let update_small = NativeViewCommand::UpdateProps { props: small_props };
    let update_at_bound = NativeViewCommand::UpdateProps { props: bound_props };
    let update_over_bound = NativeViewCommand::UpdateProps {
        props: over_bound_props,
    };
    let invoke_at_name_bound = NativeViewCommand::Invoke {
        action: "a".repeat(MAX_NATIVE_NAME_BYTES),
        payload: serde_json::json!({"row": 4, "selected": true}),
    };
    let invoke_over_name_bound = NativeViewCommand::Invoke {
        action: "a".repeat(MAX_NATIVE_NAME_BYTES + 1),
        payload: Value::Null,
    };

    if !update_small.is_within_bounds() {
        fail("the small update-props fixture is outside the command boundary");
    }
    if !update_at_bound.is_within_bounds() {
        fail("the boundary-sized update-props fixture is outside the command boundary");
    }
    if update_over_bound.is_within_bounds() {
        fail("the oversized update-props fixture was accepted by the command boundary");
    }
    if !invoke_at_name_bound.is_within_bounds() {
        fail("the 128-byte invoke action fixture is outside the command boundary");
    }
    if invoke_over_name_bound.is_within_bounds() {
        fail("the 129-byte invoke action fixture was accepted by the command boundary");
    }

    let mut group = c.benchmark_group("native_command");
    bench_command_case(&mut group, "update_props_small", &update_small);
    bench_command_case(&mut group, "update_props_at_64kib_bound", &update_at_bound);
    bench_command_case(&mut group, "update_props_over_bound", &update_over_bound);
    bench_command_case(
        &mut group,
        "invoke_action_at_128b_bound",
        &invoke_at_name_bound,
    );
    bench_command_case(
        &mut group,
        "invoke_action_over_128b_bound",
        &invoke_over_name_bound,
    );
    group.finish();
}

fn bench_event_contract_case(
    group: &mut BenchmarkGroup<'_, WallTime>,
    case: &str,
    event: &NativeViewEvent,
) {
    group.bench_function(format!("{case}/bounds_check"), |b| {
        b.iter(|| black_box(event).is_within_bounds());
    });
    group.bench_function(format!("{case}/serialize"), |b| {
        b.iter(|| serde_json::to_string(black_box(event)).is_ok());
    });
}

/// The `ncui-webview` half of the native-event boundary.
///
/// This is deliberately unconditional: it uses only the public contract crate. It is not
/// presented as queue throughput. `NativeEventSink::emit` calls the same
/// `is_within_bounds` implementation before touching the queue, so these figures are also
/// the contract-cost component visible inside the real sink measurements.
fn event_contract_costs(c: &mut Criterion) {
    let null_event = NativeViewEvent::new("button", "clicked");
    let small_payload_event = NativeViewEvent::new("input", "changed")
        .with_payload(serde_json::json!({"value": "hello", "origin": "interaction"}));
    let payload_1kib_event = NativeViewEvent::new("editor", "changed")
        .with_payload(json_string_with_serialized_len(1_024));
    let payload_at_bound_event = NativeViewEvent::new("editor", "changed")
        .with_payload(json_string_with_serialized_len(MAX_NATIVE_PAYLOAD_BYTES));
    let payload_over_bound_event = NativeViewEvent::new("editor", "changed").with_payload(
        json_string_with_serialized_len(MAX_NATIVE_PAYLOAD_BYTES + 1),
    );

    if !payload_at_bound_event.is_within_bounds() {
        fail("the boundary-sized event fixture is outside the event boundary");
    }
    if payload_over_bound_event.is_within_bounds() {
        fail("the oversized event fixture was accepted by the event boundary");
    }

    let mut group = c.benchmark_group("native_event_contract");
    bench_event_contract_case(&mut group, "null_payload", &null_event);
    bench_event_contract_case(&mut group, "small_object_payload", &small_payload_event);
    bench_event_contract_case(&mut group, "payload_1kib", &payload_1kib_event);
    bench_event_contract_case(
        &mut group,
        "payload_at_64kib_bound",
        &payload_at_bound_event,
    );
    bench_event_contract_case(
        &mut group,
        "payload_over_64kib_bound",
        &payload_over_bound_event,
    );
    group.finish();
}

/// The real provider-side bounded event queue.
///
/// The non-drop case constructs each event outside the measured region, emits one event,
/// and drains it in the same iteration, so the queue never accumulates. The full case is
/// prefilled to exactly `MAX_QUEUED_NATIVE_EVENTS`; every measured emit then performs the
/// production overflow decision — pop the oldest event, count the drop, and push the new
/// one. Event construction is setup rather than measurement because the question here is
/// the queue machinery, not the provider-specific code that created the event.
#[cfg(feature = "native-gpui")]
fn native_event_sink(c: &mut Criterion) {
    use ncui_gpui::NativeEventSink;

    let small_event = NativeViewEvent::new("input", "changed")
        .with_payload(serde_json::json!({"value": "hello", "origin": "interaction"}));
    let bound_event = NativeViewEvent::new("editor", "changed")
        .with_payload(json_string_with_serialized_len(MAX_NATIVE_PAYLOAD_BYTES));
    if !small_event.is_within_bounds() || !bound_event.is_within_bounds() {
        fail("a sink fixture is outside the event boundary");
    }

    let mut group = c.benchmark_group("native_event_sink");
    group.throughput(Throughput::Elements(1));
    group.bench_function("emit_drain_small_object", |b| {
        let sink = NativeEventSink::new();
        b.iter_batched(
            || small_event.clone(),
            |event| {
                sink.emit(event);
                black_box(sink.poll()).is_some()
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("emit_drain_payload_at_64kib_bound", |b| {
        let sink = NativeEventSink::new();
        b.iter_batched(
            || bound_event.clone(),
            |event| {
                sink.emit(event);
                black_box(sink.poll()).is_some()
            },
            BatchSize::SmallInput,
        );
    });

    let full_sink = NativeEventSink::new();
    for index in 0..MAX_QUEUED_NATIVE_EVENTS {
        full_sink.emit(
            NativeViewEvent::new("slider", "moved").with_payload(serde_json::json!({
                "index": index,
            })),
        );
    }
    if full_sink.dropped() != 0 {
        fail("prefilling the sink to its documented bound already dropped an event");
    }
    group.bench_function("emit_when_full_drops_oldest", |b| {
        b.iter_batched(
            || {
                NativeViewEvent::new("slider", "moved").with_payload(serde_json::json!({
                    "index": MAX_QUEUED_NATIVE_EVENTS,
                }))
            },
            |event| {
                full_sink.emit(event);
                full_sink.dropped()
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

/// Without the optional GPUI engine binding the sink type is not public API, so there is
/// no queue implementation to measure honestly. The unconditional contract benches above
/// still run; this no-op only keeps registration shape simple.
#[cfg(not(feature = "native-gpui"))]
fn native_event_sink(_c: &mut Criterion) {}

fn bench_window_request_case(
    group: &mut BenchmarkGroup<'_, WallTime>,
    case: &str,
    request: &WindowRequest,
) {
    group.bench_function(case, |b| {
        b.iter(|| black_box(request).validate().is_ok());
    });
}

/// Native `WindowRequest` validation.
///
/// The request boundary is provider-agnostic: it validates the provider identifier's
/// shape and serializes the props solely to enforce the 64 KiB transport bound. Schema
/// validation happens later, inside the provider. The large accepted fixture is exactly
/// at that transport bound, and the oversized fixture measures the rejection path.
fn window_request_validation(c: &mut Criterion) {
    let small = WindowRequest::native("nc-ui-kit::settings-tabs").with_props(serde_json::json!({
        "active": "general",
        "panels": {
            "general": "Application-wide preferences.",
            "appearance": "Theme and density.",
        },
    }));
    let at_bound = WindowRequest::native("nc-ui-kit::settings-tabs")
        .with_props(object_props_with_serialized_len(MAX_NATIVE_PROPS_BYTES));
    let over_bound = WindowRequest::native("nc-ui-kit::settings-tabs")
        .with_props(object_props_with_serialized_len(MAX_NATIVE_PROPS_BYTES + 1));

    if small.validate().is_err() {
        fail("the small native window-request fixture did not validate");
    }
    if at_bound.validate().is_err() {
        fail("the boundary-sized native window-request fixture did not validate");
    }
    if over_bound.validate().is_ok() {
        fail("the oversized native window-request fixture validated");
    }

    let mut group = c.benchmark_group("native_window_request");
    bench_window_request_case(&mut group, "small_props_validate", &small);
    bench_window_request_case(&mut group, "props_at_64kib_bound_validate", &at_bound);
    bench_window_request_case(&mut group, "props_over_bound_reject", &over_bound);
    group.finish();
}

/// A gpui headless application window used only to satisfy the public provider trait's
/// `build` signature. The validation and provider code under measurement are production;
/// this object supplies the window/app references that trait requires.
#[cfg(feature = "kit-providers")]
struct ProviderHarness {
    visual: VisualTestContext,
    events: Arc<NativeEventSink>,
    window: WindowId,
}

#[cfg(feature = "kit-providers")]
impl ProviderHarness {
    fn new() -> Self {
        let mut app = TestAppContext::single();
        let window = app.add_window(|_, _| EmptyView);
        let visual = VisualTestContext::from_window(window.into(), &app);
        Self {
            visual,
            events: Arc::new(NativeEventSink::new()),
            window: WindowId::new(),
        }
    }

    fn check_build(
        &mut self,
        provider: &Arc<dyn NativeViewProvider>,
        props: &Value,
        expected_error: Option<&str>,
        case: &str,
    ) {
        let context = NativeViewContext {
            window: self.window,
            props: props.clone(),
            events: Arc::clone(&self.events),
            services: None,
        };
        let outcome = self
            .visual
            .update(|window, cx| provider.build(context, window, cx));
        match (expected_error, outcome) {
            (None, Ok(_)) => {}
            (Some(fragment), Err(reason)) if reason.contains(fragment) => {}
            (None, Err(reason)) => fail(&format!(
                "{case} was expected to build but was refused: {reason}"
            )),
            (Some(fragment), Err(reason)) => fail(&format!(
                "{case} was refused for the wrong reason; expected `{fragment}` in `{reason}`"
            )),
            (Some(fragment), Ok(_)) => fail(&format!(
                "{case} unexpectedly built; expected a refusal containing `{fragment}`"
            )),
        }
    }
}

/// Measures one public provider-build props case.
///
/// Context construction is outside the measured region. The measured region enters a
/// headless gpui window update — the public trait's required `&mut Window`/`&mut App`
/// shape — and calls the registered provider's production `build` implementation. For a
/// refusal this is the exact public rejection path: `KitProvider::build` validates and
/// returns before constructing the child. For an acceptance it necessarily includes
/// child/root entity construction, which is why those benchmark names say
/// `build_upper_bound`.
#[cfg(feature = "kit-providers")]
fn bench_provider_build_case(
    group: &mut BenchmarkGroup<'_, WallTime>,
    harness: &mut ProviderHarness,
    provider: &Arc<dyn NativeViewProvider>,
    case: &str,
    props: &Value,
    expected_error: Option<&str>,
) {
    harness.check_build(provider, props, expected_error, case);
    let events = Arc::clone(&harness.events);
    let window_id = harness.window;
    let props_fixture = props.clone();
    group.bench_function(case.to_owned(), |b| {
        b.iter_batched(
            || NativeViewContext {
                window: window_id,
                props: props_fixture.clone(),
                events: Arc::clone(&events),
                services: None,
            },
            |context| {
                harness.visual.update(|window, cx| {
                    let outcome = provider.build(context, window, cx);
                    black_box(outcome.is_ok())
                })
            },
            BatchSize::SmallInput,
        );
    });
}

/// Kit provider props handling through the only public provider entry point.
///
/// The benchmark intentionally names the visibility boundary. `KitWidget::parse_props` is
/// public trait machinery, but the concrete implementations (`ButtonWidget`,
/// `SettingsTabsWidget`, and the rest) are `pub(crate)` inside `ncui-kit-providers`; the
/// type-erased `NativeViewProvider` trait exposes `build`, not `validate_props`. These
/// are therefore real public-path measurements, with rejection and acceptance labelled
/// differently rather than silently treated as the same thing.
///
/// `dropdown/semantic_rejection_after_decode` is especially useful: dropdown performs a
/// semantic selected-index check after the complete shape validation and typed decode.
/// That input passes every structural type check and then fails semantically before any
/// child construction, so it exercises essentially the full validation pipeline without
/// measuring gpui entity creation.
#[cfg(feature = "kit-providers")]
fn kit_provider_props(c: &mut Criterion) {
    if ncui_kit_providers::register_all() == 0 {
        fail("the kit provider registry accepted no providers");
    }
    let button = providers()
        .get("nc-ui-kit::button")
        .unwrap_or_else(|| fail("nc-ui-kit::button was not registered"));
    let settings_tabs = providers()
        .get("nc-ui-kit::settings-tabs")
        .unwrap_or_else(|| fail("nc-ui-kit::settings-tabs was not registered"));
    let dropdown = providers()
        .get("nc-ui-kit::dropdown")
        .unwrap_or_else(|| fail("nc-ui-kit::dropdown was not registered"));

    let mut harness = ProviderHarness::new();
    let mut group = c.benchmark_group("kit_provider_props");

    // The control is not a provider measurement. It makes the gpui `VisualTestContext`
    // crossing visible separately so the provider figures are not misread as isolated
    // serde validation numbers.
    group.bench_function("gpui_window_update_control", |b| {
        b.iter(|| harness.visual.update(|_, _| 0usize));
    });

    bench_provider_build_case(
        &mut group,
        &mut harness,
        &button,
        "button/valid_props_build_upper_bound",
        &serde_json::json!({
            "label": "Save",
            "variant": "primary",
            "size": "md",
        }),
        None,
    );
    bench_provider_build_case(
        &mut group,
        &mut harness,
        &button,
        "button/invalid_label_rejection",
        &serde_json::json!({"label": 4}),
        Some("label"),
    );
    bench_provider_build_case(
        &mut group,
        &mut harness,
        &button,
        "button/unknown_field_rejection",
        &serde_json::json!({"labell": "Save"}),
        Some("labell"),
    );

    bench_provider_build_case(
        &mut group,
        &mut harness,
        &settings_tabs,
        "settings_tabs/valid_nested_props_build_upper_bound",
        &serde_json::json!({
            "active": "general",
            "panels": {
                "general": "Application-wide preferences.",
                "appearance": "Theme, density and visual presentation.",
                "advanced": "Low-level options.",
            },
        }),
        None,
    );
    bench_provider_build_case(
        &mut group,
        &mut harness,
        &settings_tabs,
        "settings_tabs/invalid_nested_panel_rejection",
        &serde_json::json!({"panels": {"appearance": 2}}),
        Some("panels.appearance"),
    );
    bench_provider_build_case(
        &mut group,
        &mut harness,
        &settings_tabs,
        "settings_tabs/unknown_nested_panel_rejection",
        &serde_json::json!({"panels": {"plugins": "Enable plugins"}}),
        Some("panels.plugins"),
    );

    bench_provider_build_case(
        &mut group,
        &mut harness,
        &dropdown,
        "dropdown/semantic_rejection_after_decode",
        &serde_json::json!({"options": ["Alpha", "Beta"], "selected": 2}),
        Some("selected"),
    );
    group.finish();
}

/// Without the kit-provider feature there is no public validation-only provider path to
/// measure. The unconditional benches remain real measurements; nothing is substituted
/// for the private widget implementations here.
#[cfg(not(feature = "kit-providers"))]
fn kit_provider_props(_c: &mut Criterion) {}

criterion_group! {
    name = native_views;
    config = Criterion::default()
        .sample_size(100)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(700));
    targets =
        command_validation_and_serialization,
        event_contract_costs,
        native_event_sink,
        window_request_validation,
        kit_provider_props
}

criterion_main!(native_views);
