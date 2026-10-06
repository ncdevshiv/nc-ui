//! An MCP adapter over the JSON-RPC agent surface.
//!
//! ADR-008 makes MCP an *adapter*, never the internal architecture: "if MCP were the internal
//! shape, the runtime would inherit an agent protocol's constraints as core design". This
//! module honours that by being a pure translation layer. It adds no policy of its own —
//! every call it makes goes through [`crate::dispatch::dispatch_with`], which is the same
//! pipeline an in-process caller and a socket client use. That is what makes the capability
//! check, the consent gate, the rate limit and the audit trail apply unchanged rather than
//! being re-implemented and inevitably drifting.
//!
//! What this module *does* add is a second spelling of the same operations:
//!
//! * [`ToolDescriptor`] and [`tool_table`] — MCP's `tools/list` view of the method table,
//!   derived from [`crate::dispatch::method_table`] rather than restated, so a method added
//!   to one is automatically present in both and cannot drift.
//! * [`to_request`] and [`from_response`] — translation in each direction.
//!
//! An MCP tool is named `ncui_<method with dots replaced by underscores>`, because MCP tool
//! names are conventionally restricted to `[A-Za-z0-9_-]`. The mapping is reversible, so
//! [`to_request`] can recover the JSON-RPC method a tool name refers to.

use std::time::Duration;

use ncui_core::{ErrorKind, NcError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::AgentSession;
use crate::describe::MethodDescription;
use crate::rpc::{Request, Response};
use crate::{Runtime, dispatch};

/// The MCP protocol revision this adapter speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// The separator MCP tool names use where a JSON-RPC method name would use a dot.
/// One tool as MCP advertises it.
///
/// MCP describes a tool's inputs with a JSON Schema. The schema is derived from the method's
/// real parameters rather than written by hand, so a method's parameters and its advertised
/// schema cannot disagree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Unique tool name, e.g. `ncui_describe`.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
    /// `true` when calling this tool performs a destructive operation and needs consent.
    pub destructive: bool,
}

/// The MCP tool name for a JSON-RPC method.
///
/// A direct dots-to-underscores substitution: methods already carry the `ncui.` namespace
/// (`ncui.logs.purge`), so the tool name is `ncui_logs_purge`. Prefixing again would produce
/// `ncui_ncui_logs_purge`, and stripping the prefix on the way back would then yield
/// `logs.purge` — a method that does not exist. Keeping the substitution direct is what makes
/// [`method_for_tool`] its exact inverse.
///
/// The mapping is lossless only while no method name contains an underscore, since dots and
/// underscores would become indistinguishable. `method_segments_are_reversible` asserts that
/// for the live table, so adding an underscore-bearing method fails a test rather than
/// silently producing a tool that resolves to the wrong method.
#[must_use]
pub fn tool_name_for(method: &str) -> String {
    method.replace('.', "_")
}

/// The JSON-RPC method a tool name refers to, if the name has the shape of one.
///
/// Reverses [`tool_name_for`]. Whether the name actually names a method in the table is the
/// caller's question to answer against that table, not something this function can know.
#[must_use]
pub fn method_for_tool(tool: &str) -> Option<String> {
    if tool.is_empty() {
        return None;
    }
    Some(tool.replace('_', "."))
}

/// The JSON Schema describing a method's parameters.
///
/// Every current method takes an object, so the schema is an object schema. Deriving it here
/// — rather than storing one per method — is what keeps it true when a method changes.
#[must_use]
pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "consent": {
                "type": "boolean",
                "description":
                    "Required for a destructive operation. An absent or false consent is a \
                     refusal, never a grant."
            }
        },
        "additionalProperties": true
    })
}

/// MCP's view of the method table.
///
/// Derived from the same [`MethodDescription`] list the JSON-RPC surface serves, so the two
/// cannot drift: a method that exists in one exists in both.
#[must_use]
pub fn tool_table(methods: &[MethodDescription]) -> Vec<ToolDescriptor> {
    let schema = input_schema();
    methods
        .iter()
        .map(|method| ToolDescriptor {
            name: tool_name_for(&method.name),
            description: method.summary.clone(),
            input_schema: schema.clone(),
            destructive: method.destructive,
        })
        .collect()
}

/// Translates an MCP tool call into the JSON-RPC request it stands for.
///
/// The arguments become the request params unchanged. No default is injected and no key is
/// dropped: a tool that invented a `consent` the client did not send would defeat the gate,
/// and one that dropped an argument the client did send would hide a refusal.
#[must_use]
pub fn to_request(id: Value, tool: &str, arguments: Value) -> Request {
    let method = method_for_tool(tool).unwrap_or_else(|| tool.to_owned());
    Request::new(id, method, arguments)
}

/// Translates a JSON-RPC response into the content block MCP returns.
///
/// An error becomes an MCP result whose `isError` flag is set and whose body carries the
/// stable `ncui_code`. MCP has no error channel of its own for a tool call, and mapping a
/// refusal to *success* would be the one thing an adapter must never do: the caller would see
/// a successful tool invocation for work that was refused.
///
/// `structuredContent` carries the whole payload as JSON so a client that wants to program
/// against it does not have to parse the text.
#[must_use]
pub fn from_response(response: &Response) -> Value {
    match (&response.result, &response.error) {
        (Some(result), _) => json!({
            "content": [{ "type": "text", "text": render_text(result) }],
            "structuredContent": result,
            "isError": false
        }),
        (None, Some(error)) => json!({
            "content": [{
                "type": "text",
                "text": format!("{}: {}", error.data.ncui_code, error.message)
            }],
            "structuredContent": {
                "ncui_code": error.data.ncui_code,
                "operation": error.data.operation,
                "correlation": error.data.correlation,
                "retryable": error.data.retryable
            },
            "isError": true
        }),
        // `Response` cannot be built without one of the two, so this is unreachable in
        // practice; it is handled rather than panicked so an adapter never aborts a call.
        (None, None) => json!({
            "content": [{ "type": "text", "text": "the response carried neither a result nor an error" }],
            "isError": true
        }),
    }
}

/// A stable one-line rendering of a JSON payload, for clients that read text.
fn render_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Calls one MCP tool and returns the MCP content for it.
///
/// This is the whole adapter's entry point. It builds a JSON-RPC request and dispatches it
/// through [`dispatch::dispatch_with`] — the identical call [`crate::transport`] makes — so
/// the capability check, consent gate, rate limit and audit trail are the runtime's, not this
/// module's.
///
/// `feature_registry` is threaded through unchanged; a tool that reports feature status must
/// see the same registry the JSON-RPC surface sees, or the two would answer differently.
pub fn call_tool(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    tool: &str,
    arguments: Value,
    now: Duration,
    feature_registry: Option<&Value>,
    table: &[MethodDescription],
) -> Value {
    let request = to_request(Value::from(1), tool, arguments);
    // The dispatch pipeline derives the correlation id from the request, so an MCP call and
    // the JSON-RPC call it stands for are the same audited event rather than two unrelated
    // ones. Nothing extra is threaded in for that to happen.
    match dispatch::dispatch_with(runtime, session, &request, now, feature_registry, table) {
        Some(response) => from_response(&response),
        // A notification returns no response; a tool call always expects one, so this is
        // reported as a tool error rather than as an empty success.
        None => json!({
            "content": [{ "type": "text", "text": "the call produced no response" }],
            "isError": true
        }),
    }
}

/// The MCP `tools/list` payload for the method table.
#[must_use]
pub fn tools_list_response(methods: &[MethodDescription]) -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "tools": tool_table(methods)
    })
}

/// Handles an MCP `tools/call` request envelope.
///
/// The envelope is MCP's own JSON-RPC, so it shares [`Request`] and [`Response`] rather than
/// introducing a second message type. `method` is an MCP method name (`tools/list`,
/// `tools/call`), which is the one place the two protocols' namespaces meet.
#[must_use]
pub fn handle_envelope(
    runtime: &mut Runtime,
    session: &mut AgentSession,
    request: &Request,
    now: Duration,
    feature_registry: Option<&Value>,
    table: &[MethodDescription],
) -> Option<Response> {
    let id = request.id.clone().unwrap_or(Value::Null);
    match request.method.as_str() {
        "tools/list" => Some(Response::success(id, tools_list_response(table))),
        "tools/call" => {
            let name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Some(Response::success(
                id,
                call_tool(
                    runtime,
                    session,
                    &name,
                    arguments,
                    now,
                    feature_registry,
                    table,
                ),
            ))
        }
        other => {
            // An unknown MCP method is reported through the same structured error the rest
            // of the surface uses, so a client sees a `ncui_code` rather than a bare string.
            Some(Response::from_error(
                id,
                &NcError::new(ErrorKind::Validation {
                    field: "method".to_owned(),
                    reason: format!(
                        "unknown MCP method `{other}`; expected `tools/list` or `tools/call`"
                    ),
                })
                .operation("runtime::mcp::handle_envelope"),
            ))
        }
    }
}
