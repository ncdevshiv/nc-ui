//! Native-view command and event handlers.
//!
//! The runtime-facing half of the native window contract: a native window's view emits
//! events an agent drains, and accepts commands an agent sends. Both travel through here
//! rather than through the page's seam, because the caller is the agent — a page reaches
//! its own host directly, and what lives here is the machine-facing half the agent
//! surface exposes. They live apart from `params` for the reason `cdp_handler` does: the
//! parsing, the nested answer shape and the stable-code mapping are one contract, and a
//! reader of the pipeline should see one method table entry rather than three hundred
//! lines of another boundary's rules.
//!
//! One thing is documented here because a reader of either method needs it. The runtime
//! error model has no native-view kind — [`ErrorKind`] is `ncui-core`'s closed set, and a
//! native refusal is a *conflict with what the caller believed* or *the caller's own
//! input*, which the existing kinds already classify honestly. What the kinds cannot
//! carry is the contract's stable `native_*` failure code, and that code is the part of
//! a refusal an agent branches on. It is therefore prefixed onto the error's reason
//! verbatim — `native_view_refused: the native view refused the command: ...` — so the
//! wire message opens with the machine-readable token and a client matches it as a
//! prefix rather than parsing wording out of the middle of a sentence. This is the same
//! contract the command's own out-of-bounds refusal keeps, so the code survives every
//! layer unchanged whichever side of the boundary spoke.

use ncui_core::error::validation;
use ncui_core::{ErrorKind, NcError, Result, WindowId};
use ncui_webview::NativeViewCommand;
use serde_json::{Value, json};

use super::json_error;
use super::params::{window_id_param, window_rejection};
use crate::Runtime;

/// `ncui.window.native.events` — drain the events a native window's view has emitted.
///
/// The answer is every event queued since the last drain and the count dropped to a full
/// queue in between, both in the contract's own shape: the runtime carries events rather
/// than interpreting them, because what a view emits is the provider's business. A window
/// that is open but carries no native view is a conflict naming that fact rather than an
/// empty answer, because "no events yet" and "no events ever, this window is a page" are
/// different facts and an agent polling a page window should learn its mistake on the
/// first call rather than the never.
pub(crate) fn invoke_window_native_events(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::window_native_events";
    let id = window_id_param(params)?;
    let Some((events, dropped)) = runtime
        .windows
        .drain_native_events(id)
        .map_err(window_rejection)?
    else {
        return Err(not_native(id, OPERATION));
    };
    // Dropped events are the overload signal the contract counts them for: a view that
    // outran its consumer is exactly what an operator watches this surface to learn, so
    // the loss is counted rather than only a number inside one answer. The drain reports
    // the delta since the last drain, which is what keeps one loss counted once.
    if dropped > 0 {
        let _ = runtime.metrics.counter_inc(
            "window.native.events_dropped",
            &ncui_observability::metrics::Labels::new(),
            dropped,
        );
        tracing::warn!(
            window = %id,
            dropped,
            "native events were dropped to a full queue"
        );
    }
    tracing::debug!(
        window = %id,
        events = events.len(),
        dropped,
        "native events drained"
    );
    serde_json::to_value(json!({
        "events": events,
        "dropped": dropped,
    }))
    .map_err(|e| json_error(&e))
}

/// `ncui.window.native.command` — deliver one command to a native window's view.
///
/// The command is the contract's own, but the wire does not ask the caller to write the
/// tag: `props` alone is a props update, and `action` with an optional `payload` is a
/// verb invocation. The two are refused together and refused absent, because a command
/// that was both — or neither — is a caller mistake, and a mistake answered as if it
/// meant something is how a view ends up rebuilt with props nobody sent. The bounds are
/// checked here, before the command is sent anywhere, so a caller learns it crossed one —
/// and a host is never asked to carry a command the contract already forbids.
///
/// The view's answer is carried honestly: acceptance is `{ "delivered": true }`, and a
/// refusal reaches the wire with its stable code intact (see the module documentation).
pub(crate) fn invoke_window_native_command(runtime: &mut Runtime, params: &Value) -> Result<Value> {
    const OPERATION: &str = "dispatch::window_native_command";
    let id = window_id_param(params)?;
    let command = native_command_param(params)?;
    // The verb is captured before the command moves: the trace names what the view was
    // told rather than only that it was told something, the way `ncui.window.create`
    // traces the URL rather than only that a window opened.
    let verb = match &command {
        NativeViewCommand::UpdateProps { .. } => "update_props".to_owned(),
        NativeViewCommand::Invoke { action, .. } => action.clone(),
    };
    let sent = runtime
        .windows
        .send_native_command(id, command)
        .map_err(window_rejection)?;
    let Some(answer) = sent else {
        return Err(not_native(id, OPERATION));
    };
    answer.map_err(|error| native_view_error(&error, OPERATION))?;
    // A command reaching a native view is visible for the same reason the events are: the
    // pair is the whole conversation the native surface carries, and an operator watching
    // one half without the other would see a window talking to itself.
    let _ = runtime.metrics.counter_inc(
        "window.native.command",
        &ncui_observability::metrics::Labels::new(),
        1,
    );
    tracing::info!(window = %id, action = %verb, "native command delivered");
    serde_json::to_value(json!({ "delivered": true })).map_err(|e| json_error(&e))
}

/// Reads and validates the command a native-command call carries.
///
/// The wire shape is flattened rather than the contract's tagged object: the caller names
/// `props` or `action`, and the tag those two imply is written here. Exactly one of them
/// may be present — the pair together is two commands pretending to be one, and neither
/// is no command at all, and both are refused with the rule named rather than a guess at
/// which half the caller meant. A `payload` without an `action` is refused for the same
/// reason: it is input that means nothing, and silently dropping it would accept a call
/// the caller did not make.
fn native_command_param(params: &Value) -> Result<NativeViewCommand> {
    const OPERATION: &str = "dispatch::native_command_param";
    // `null` is absence, matching every other optional field on this surface: a caller
    // that writes `"props": null` asked for nothing, which the neither-rule then names.
    let props = params.get("props").filter(|value| !value.is_null());
    let action = params.get("action").filter(|value| !value.is_null());
    let payload = params.get("payload").filter(|value| !value.is_null());
    let command = match (props, action) {
        (Some(props), None) => {
            if payload.is_some() {
                return Err(validation(
                    "payload",
                    "belongs to an `action`: a props update carries no payload",
                )
                .operation(OPERATION));
            }
            if !props.is_object() {
                return Err(validation(
                    "props",
                    "must be an object: the props the view is rebuilt with",
                )
                .operation(OPERATION));
            }
            NativeViewCommand::UpdateProps {
                props: props.clone(),
            }
        }
        (None, Some(action)) => {
            let verb = action
                .as_str()
                .filter(|verb| !verb.is_empty())
                .ok_or_else(|| {
                    validation(
                        "action",
                        "must be a non-empty string: the verb the native view answers",
                    )
                    .operation(OPERATION)
                })?;
            NativeViewCommand::Invoke {
                action: verb.to_owned(),
                payload: payload.cloned().unwrap_or(Value::Null),
            }
        }
        (Some(_), Some(_)) => {
            return Err(validation(
                "props",
                "and `action` must not be given together: a command is either a props \
                 update or an invoked verb, so name exactly one",
            )
            .operation(OPERATION));
        }
        (None, None) => {
            return Err(validation(
                "props",
                "or `action` must be given: a command is either a props update or an \
                 invoked verb, so name exactly one",
            )
            .operation(OPERATION));
        }
    };
    if !command.is_within_bounds() {
        // The same stable code the host would have answered with, because the failure is
        // the same failure whether the runtime or the view catches it: an agent branches
        // on the code, not on which layer of the boundary spoke.
        return Err(validation(
            "command",
            "native_out_of_bounds: the command is larger than the bounds the native \
             contract allows",
        )
        .operation(OPERATION));
    }
    Ok(command)
}

/// The conflict an open window without a native view is answered with.
///
/// The wording is stable for the same reason the contract's codes are: an agent that
/// drives both kinds of window branches on the difference between "the window is gone"
/// and "the window was never native", and a message that drifted would break the branch.
fn not_native(id: WindowId, operation: &'static str) -> NcError {
    NcError::new(ErrorKind::Conflict {
        reason: format!("window {id} is not a native window"),
    })
    .operation(operation)
}

/// Preserves the native contract's stable failure codes through the runtime error model.
///
/// The stable `native_*` code is prefixed onto the reason verbatim, because the wire
/// error is what an agent branches on and the code is the part of the refusal that must
/// survive every layer — see the module documentation for the mechanism. The kinds are
/// the runtime's own vocabulary for *who is at fault*: a window that is gone or a view
/// that refuses is a conflict with what the caller believed, while a command outside the
/// contract's bounds is the caller's own input, refused the same way every other invalid
/// parameter is refused.
pub(crate) fn native_view_error(
    error: &ncui_webview::NativeViewError,
    operation: &'static str,
) -> NcError {
    let reason = format!("{}: {error}", error.code());
    match error {
        ncui_webview::NativeViewError::OutOfBounds { .. } => NcError::new(ErrorKind::Validation {
            field: "command".to_owned(),
            reason,
        })
        .operation(operation),
        ncui_webview::NativeViewError::WindowGone
        | ncui_webview::NativeViewError::Refused { .. }
        | ncui_webview::NativeViewError::EngineUnavailable { .. } => {
            NcError::new(ErrorKind::Conflict { reason }).operation(operation)
        }
    }
}

#[cfg(test)]
mod tests {
    use ncui_webview::NativeViewError;

    use super::*;

    #[test]
    fn every_native_view_error_keeps_its_stable_code_in_the_reason() {
        for (error, stable) in [
            (NativeViewError::WindowGone, "native_window_gone"),
            (
                NativeViewError::Refused {
                    reason: "no such verb".to_owned(),
                },
                "native_view_refused",
            ),
            (
                NativeViewError::EngineUnavailable {
                    reason: "gone".to_owned(),
                },
                "native_engine_unavailable",
            ),
            (
                NativeViewError::OutOfBounds {
                    reason: "too large".to_owned(),
                },
                "native_out_of_bounds",
            ),
        ] {
            let mapped = native_view_error(&error, "dispatch::test");
            let wire = format!("{mapped}");
            assert!(
                wire.contains(stable),
                "{stable} must survive into the wire error: {wire}"
            );
        }
    }

    #[test]
    fn a_refusal_outside_the_bounds_is_classified_as_the_callers_input() {
        let error = native_view_error(
            &NativeViewError::OutOfBounds {
                reason: "too large".to_owned(),
            },
            "dispatch::test",
        );
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "an out-of-bounds answer is the caller's own input, refused as such: {error:?}"
        );
        for refusal in [
            NativeViewError::WindowGone,
            NativeViewError::Refused {
                reason: "no".to_owned(),
            },
            NativeViewError::EngineUnavailable {
                reason: "gone".to_owned(),
            },
        ] {
            let mapped = native_view_error(&refusal, "dispatch::test");
            assert!(
                matches!(mapped.kind(), ErrorKind::Conflict { .. }),
                "{refusal}: {mapped:?}"
            );
        }
    }

    #[test]
    fn a_command_with_neither_props_nor_action_is_refused() {
        for params in [json!({}), json!({ "payload": { "row": 1 } })] {
            let error = native_command_param(&params).expect_err("neither is no command");
            assert!(
                matches!(error.kind(), ErrorKind::Validation { .. }),
                "{error:?}"
            );
            assert!(
                error.to_string().contains("props") && error.to_string().contains("action"),
                "the refusal names both halves of the rule: {error}"
            );
        }
    }

    #[test]
    fn a_command_with_both_props_and_action_is_refused() {
        let error = native_command_param(&json!({
            "props": { "rows": 3 },
            "action": "focus",
        }))
        .expect_err("both is two commands pretending to be one");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("must not be given together"),
            "the refusal names the rule: {error}"
        );
    }

    #[test]
    fn props_and_action_each_parse_into_the_command_they_mean() {
        let update = native_command_param(&json!({ "props": { "rows": 3 } }))
            .expect("props alone is a props update");
        assert_eq!(
            update,
            NativeViewCommand::UpdateProps {
                props: json!({ "rows": 3 }),
            }
        );
        let invoke =
            native_command_param(&json!({ "action": "select_row", "payload": { "row": 4 } }))
                .expect("action is a verb invocation");
        assert_eq!(
            invoke,
            NativeViewCommand::Invoke {
                action: "select_row".to_owned(),
                payload: json!({ "row": 4 }),
            }
        );
        // A missing payload is the contract's own default rather than a refusal: the verb
        // is the whole of what was asked for.
        let bare = native_command_param(&json!({ "action": "focus" })).expect("a bare verb");
        assert_eq!(
            bare,
            NativeViewCommand::Invoke {
                action: "focus".to_owned(),
                payload: Value::Null,
            }
        );
    }

    #[test]
    fn null_props_and_null_action_are_absence_not_presence() {
        let error = native_command_param(&json!({ "props": null, "action": null }))
            .expect_err("explicit nulls ask for nothing");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn props_that_are_not_an_object_are_refused() {
        for props in [json!("rows"), json!(3), json!([1, 2])] {
            let error = native_command_param(&json!({ "props": props }))
                .expect_err("the props of a view are an object");
            assert!(
                matches!(error.kind(), ErrorKind::Validation { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_payload_beside_props_is_input_that_means_nothing() {
        let error = native_command_param(&json!({
            "props": { "rows": 3 },
            "payload": { "row": 1 },
        }))
        .expect_err("a payload without an action is dead input");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("payload"),
            "the refusal names the dead field: {error}"
        );
    }

    #[test]
    fn an_empty_action_is_not_a_verb() {
        for action in [json!(""), json!(3)] {
            let error = native_command_param(&json!({ "action": action }))
                .expect_err("an action that is not a non-empty string is not a verb");
            assert!(
                matches!(error.kind(), ErrorKind::Validation { .. }),
                "{error:?}"
            );
        }
    }

    #[test]
    fn an_oversized_command_is_refused_before_any_window_is_asked() {
        let error = native_command_param(&json!({
            "action": "select_row",
            "payload": { "data": "x".repeat(70_000) },
        }))
        .expect_err("a command past the contract's bound is refused");
        assert!(
            matches!(error.kind(), ErrorKind::Validation { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("native_out_of_bounds"),
            "the refusal carries the stable code: {error}"
        );
    }
}
