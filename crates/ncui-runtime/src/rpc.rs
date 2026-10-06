//! The JSON-RPC 2.0 request and response values.
//!
//! The wire format is specified in `protocols/jsonrpc/README.md`; this module is its
//! implementation. Two decisions are worth stating here because they are easy to get
//! wrong:
//!
//! * **`id` may be absent.** A notification carries no `id` and expects no response. If
//!   `id` were required, a client could not use the same send path for notifications and
//!   calls, and the server would have to invent a reply to send.
//! * **Errors carry a stable `ncui_code`.** The numeric JSON-RPC code is coarse; the
//!   machine-readable `ncui_code` is what an agent actually branches on, and it is part of
//!   the public API surface.

use ncui_core::{ErrorKind, NcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC 2.0 request, or a notification when `id` is absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version. Always `"2.0"`.
    pub jsonrpc: String,
    /// Correlation identifier. Absent for a notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// The method name, e.g. `"ncui.health"`.
    pub method: String,
    /// Named parameters. Always an object; positional parameters are not accepted.
    #[serde(default = "empty_params")]
    pub params: Value,
}

fn empty_params() -> Value {
    Value::Object(serde_json::Map::new())
}

impl Request {
    /// Builds a request with the given id and parameters.
    #[must_use]
    pub fn new(id: Value, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: Some(id),
            method: method.into(),
            params,
        }
    }

    /// Whether this is a notification, i.e. expects no response.
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// The protocol version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// The version of the NC-UI agent interface itself, negotiated independently of the
/// runtime version (ADR-008). Clients negotiate against this, not against the crate
/// version, so the surface can stabilise while the runtime keeps moving.
pub const API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A successful or failed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Echoes the request id, or is `null` for a parse failure where no id is known.
    pub id: Value,
    /// The result, present on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error, present on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// The `error` member of a failed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// Coarse JSON-RPC code.
    pub code: i32,
    /// Human-readable message.
    pub message: String,
    /// Machine-readable detail.
    pub data: RpcErrorData,
}

/// The `data` member of an [`RpcError`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcErrorData {
    /// Stable machine-readable code, e.g. `"capability_denied"`.
    pub ncui_code: String,
    /// The operation that failed.
    pub operation: String,
    /// The correlation id of the request, when the error arose under one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    /// Whether retrying could plausibly succeed.
    pub retryable: bool,
}

impl Response {
    /// A success response.
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A failure response.
    #[must_use]
    pub fn failure(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Builds a failure response from an [`NcError`].
    #[must_use]
    pub fn from_error(id: Value, error: &NcError) -> Self {
        Self::failure(id, RpcError::from_nc(error))
    }
}

impl RpcError {
    /// Maps a runtime error onto the wire representation.
    #[must_use]
    pub fn from_nc(error: &NcError) -> Self {
        Self {
            code: json_rpc_code(error.kind()),
            message: error.to_string(),
            data: RpcErrorData {
                ncui_code: error.code().to_owned(),
                operation: error.operation_name().to_owned(),
                correlation: error.correlation().map(|id| id.to_string()),
                retryable: error.is_retryable(),
            },
        }
    }
}

/// Maps an [`ErrorKind`] onto a JSON-RPC error code.
///
/// `ErrorKind` is `#[non_exhaustive]`, so this mapping cannot be made to fail at compile
/// time when a variant is added. The wildcard arm below is therefore deliberately
/// pessimistic — a new failure class surfaces as a generic server error rather than being
/// silently misreported as a client error. The test
/// `every_known_kind_maps_to_its_documented_code` pins the mapping for every kind that
/// exists today; adding a variant to `ncui_core` should extend that table.
#[must_use]
pub const fn json_rpc_code(kind: &ErrorKind) -> i32 {
    match kind {
        ErrorKind::InvalidId { .. } | ErrorKind::Validation { .. } => -32602,
        ErrorKind::Protocol { .. } => -32600,
        ErrorKind::CapabilityDenied { .. } => -32001,
        ErrorKind::LimitExceeded { .. } => -32002,
        ErrorKind::Timeout { .. } => -32003,
        ErrorKind::Cancelled => -32004,
        ErrorKind::NotFound { .. } => -32005,
        ErrorKind::Conflict { .. } => -32006,
        ErrorKind::Backpressure { .. } => -32008,
        ErrorKind::Internal { .. } | ErrorKind::Io { .. } | ErrorKind::Codec { .. } => SERVER_ERROR,
        _ => {
            // A failure class added to `ncui_core` after this mapping was written. Reported
            // as an internal error until it is given a deliberate code of its own.
            SERVER_ERROR
        }
    }
}

/// The JSON-RPC code for a failure the server is responsible for.
const SERVER_ERROR: i32 = -32603;

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::CorrelationId;
    use serde_json::json;

    #[test]
    fn a_request_round_trips_through_json() {
        let request = Request::new(json!(1), "ncui.health", json!({ "deep": true }));
        let text = serde_json::to_string(&request).expect("serializable");
        let back: Request = serde_json::from_str(&text).expect("deserializable");
        assert_eq!(back, request);
        assert!(!back.is_notification());
    }

    #[test]
    fn a_request_without_an_id_is_a_notification() {
        let request: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"ncui.health"}"#).expect("valid");
        assert!(request.is_notification());
        assert_eq!(
            request.params,
            json!({}),
            "params must default to an object"
        );
    }

    #[test]
    fn a_success_response_carries_a_result_and_no_error() {
        let response = Response::success(json!(7), json!({"status":"ok"}));
        assert!(response.error.is_none());
        assert_eq!(response.result, Some(json!({"status":"ok"})));

        let text = serde_json::to_string(&response).expect("serializable");
        assert!(
            !text.contains("error"),
            "absent error must be omitted: {text}"
        );
    }

    #[test]
    fn an_error_response_carries_stable_machine_readable_detail() {
        let correlation = CorrelationId::new();
        let error = NcError::new(ErrorKind::CapabilityDenied {
            capability: "read.logs".to_owned(),
            subject: "agent-1".to_owned(),
        })
        .operation("agent::authorize")
        .correlated(correlation);
        let response = Response::from_error(json!(7), &error);

        let rpc = response
            .error
            .expect("a failure response must carry an error");
        assert_eq!(rpc.code, -32001);
        assert_eq!(rpc.data.ncui_code, "capability_denied");
        assert_eq!(rpc.data.operation, "agent::authorize");
        assert_eq!(rpc.data.correlation, Some(correlation.to_string()));
        assert!(!rpc.data.retryable);
        assert!(response.result.is_none());
    }

    #[test]
    fn every_known_kind_maps_to_its_documented_code() {
        // One entry per `ErrorKind` variant. Adding a variant to `ncui_core` should add an
        // entry here, which is what turns the wildcard arm from a silent gap into a
        // reviewed decision.
        let expected: Vec<(ErrorKind, i32)> = vec![
            (ErrorKind::InvalidId { reason: "r".into() }, -32602),
            (
                ErrorKind::CapabilityDenied {
                    capability: "c".into(),
                    subject: "s".into(),
                },
                -32001,
            ),
            (
                ErrorKind::Validation {
                    field: "f".into(),
                    reason: "r".into(),
                },
                -32602,
            ),
            (
                ErrorKind::LimitExceeded {
                    resource: "r".into(),
                    limit: 1,
                    observed: 2,
                },
                -32002,
            ),
            (ErrorKind::Timeout { elapsed_ms: 1 }, -32003),
            (ErrorKind::Cancelled, -32004),
            (
                ErrorKind::NotFound {
                    kind: "k".into(),
                    id: "i".into(),
                },
                -32005,
            ),
            (ErrorKind::Conflict { reason: "r".into() }, -32006),
            (ErrorKind::Protocol { reason: "r".into() }, -32600),
            (
                ErrorKind::Backpressure {
                    channel: "c".into(),
                    capacity: 1,
                },
                -32008,
            ),
            (
                ErrorKind::Io {
                    operation: "o".into(),
                    io_kind: std::io::ErrorKind::Other,
                    message: "m".into(),
                },
                -32603,
            ),
            (
                ErrorKind::Codec {
                    operation: "o".into(),
                    reason: "r".into(),
                },
                -32603,
            ),
            (
                ErrorKind::Internal {
                    operation: "o".into(),
                    reason: "r".into(),
                },
                -32603,
            ),
        ];
        for (kind, code) in expected {
            assert_eq!(json_rpc_code(&kind), code, "wrong code for {kind:?}");
        }
    }

    #[test]
    fn every_error_kind_maps_to_a_json_rpc_code() {
        let kinds = [
            ErrorKind::InvalidId { reason: "r".into() },
            ErrorKind::CapabilityDenied {
                capability: "c".into(),
                subject: "s".into(),
            },
            ErrorKind::Validation {
                field: "f".into(),
                reason: "r".into(),
            },
            ErrorKind::LimitExceeded {
                resource: "r".into(),
                limit: 1,
                observed: 2,
            },
            ErrorKind::Timeout { elapsed_ms: 1 },
            ErrorKind::Cancelled,
            ErrorKind::NotFound {
                kind: "k".into(),
                id: "i".into(),
            },
            ErrorKind::Conflict { reason: "r".into() },
            ErrorKind::Protocol { reason: "r".into() },
            ErrorKind::Backpressure {
                channel: "c".into(),
                capacity: 1,
            },
            ErrorKind::Io {
                operation: "o".into(),
                io_kind: std::io::ErrorKind::Other,
                message: "m".into(),
            },
            ErrorKind::Codec {
                operation: "o".into(),
                reason: "r".into(),
            },
            ErrorKind::Internal {
                operation: "o".into(),
                reason: "r".into(),
            },
        ];
        for kind in &kinds {
            let code = json_rpc_code(kind);
            assert!(
                (-32768..=0).contains(&code),
                "JSON-RPC codes must be negative and in the implementation range, got {code}"
            );
        }
    }

    #[test]
    fn retryability_survives_the_mapping() {
        let retryable = NcError::new(ErrorKind::Timeout { elapsed_ms: 5 });
        assert!(
            Response::from_error(json!(1), &retryable)
                .error
                .expect("error")
                .data
                .retryable
        );

        let fatal = NcError::new(ErrorKind::Protocol {
            reason: "bad".into(),
        });
        assert!(
            !Response::from_error(json!(1), &fatal)
                .error
                .expect("error")
                .data
                .retryable
        );
    }
}
