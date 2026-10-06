//! Typed message envelopes and their two representations.
//!
//! A [`Message`] is the logical unit the runtime passes around: a request, its response, an
//! unsolicited event, or an error response. It has two representations, and they must not
//! drift (ADR-003):
//!
//! * the **binary payload**, the hot path, laid out in the crate-level specification;
//! * the **JSON object**, the control plane an agent reads (ADR-008).
//!
//! Both are covered by round-trip tests over the same messages. The JSON form exists because
//! an agent needs to *read* a message, not because the transport should ever parse JSON.
//!
//! # Why the payload fields are opaque bytes
//!
//! `params`, `result` and the event `payload` are [`Vec<u8>`], not a parsed value. A protocol
//! that interpreted them would have to know every method's schema, which would put the schema
//! registry inside the transport and give every new method a change to the hot path. The
//! method and topic names say what the bytes mean; only the handler for that method decodes
//! them. This is the concrete reason ADR-003's "serialise to JSON, copy, parse" chain does not
//! happen on the hot path.
//!
//! # Identifier nulls
//!
//! Zero is the reserved null identifier in `ncui-core`. A decoded identifier of zero is
//! therefore a protocol violation, not a value: allowing it would produce an identifier that
//! could never be re-encoded, breaking the round-trip property that every other invariant
//! depends on.

use ncui_core::{CorrelationId, ErrorKind, Limits, NcError, RequestId, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::codec::BufWriter;
use crate::error_codec::{decode_correlation, read_error, write_error};
use crate::frame::{
    self, FRAME_HEADER_BYTES, Frame, FrameDecoder, FrameFlags, FrameHeader, FrameKind,
};

/// The message payload version this build speaks.
///
/// Repeated inside every payload so a payload is self-describing: it can be validated, and
/// unit-tested, without the frame that carried it. A payload whose version disagrees with its
/// frame is rejected, never reinterpreted.
pub const PAYLOAD_VERSION: u8 = 1;

const DISCRIMINATOR_REQUEST: u8 = 0x01;
const DISCRIMINATOR_RESPONSE: u8 = 0x02;
const DISCRIMINATOR_EVENT: u8 = 0x03;
const DISCRIMINATOR_ERROR_RESPONSE: u8 = 0x04;

/// A message that crossed an NC-UI transport.
#[derive(Debug, Clone)]
pub enum Message {
    /// A call that expects exactly one response.
    Request {
        /// The method being invoked, e.g. `"window.create"`. Dispatch is on this string, so
        /// it is part of the agent-facing API surface (ADR-008) and versioned with it.
        method: String,
        /// Opaque method arguments. The handler for `method` defines the schema.
        params: Vec<u8>,
        /// Ties this message to every log, span, metric and event produced while handling it.
        /// Required, not optional: a request without one cannot be traced end to end, which
        /// is the property correlation exists to provide (spec 50).
        correlation: CorrelationId,
        /// Identifies this request so its response can be matched to it. Unique among the
        /// in-flight requests of one peer; see [`crate::RequestRegistry`].
        request_id: RequestId,
        /// Milliseconds after send at which the request is abandoned. Zero means "no
        /// deadline", which is only appropriate for a peer that has already agreed a
        /// different policy out of band.
        deadline_ms: u64,
    },
    /// A successful response to exactly one request.
    Response {
        /// The request this answers. Must match the id the request was issued with; a
        /// mismatch is a protocol violation, not a best-effort match.
        request_id: RequestId,
        /// The correlation of the request being answered, carried through so a response can
        /// be traced without the registry that issued it.
        correlation: CorrelationId,
        /// Opaque result bytes, defined by the method that was called.
        result: Vec<u8>,
    },
    /// An unsolicited notification. No request, so no request id and no response.
    Event {
        /// What happened, e.g. `"window.closed"`. Subscribers dispatch on this string.
        topic: String,
        /// Opaque event bytes.
        payload: Vec<u8>,
        /// Correlation of whatever caused the event, so a burst of events from one user action
        /// can be joined back together.
        correlation: CorrelationId,
    },
    /// A failed response to exactly one request.
    ErrorResponse {
        /// The request this answers, with the same uniqueness requirement as
        /// [`Message::Response`].
        request_id: RequestId,
        /// The correlation of the request being answered.
        correlation: CorrelationId,
        /// Why the request failed, carried with its structured fields intact.
        error: NcError,
    },
}

/// Compares two errors by the state a caller can observe.
///
/// `NcError` deliberately does not implement `PartialEq`, so `Message` cannot derive it. The
/// comparison below is over exactly what the type exposes — the kind, the operation string and
/// the correlation — which is the whole of its observable state. Deriving nothing and
/// comparing the parts is honest; pretending to a derived `PartialEq` would hide the fact
/// that this is a deliberate projection.
fn errors_equivalent(left: &NcError, right: &NcError) -> bool {
    left.kind() == right.kind()
        && left.operation_name() == right.operation_name()
        && left.correlation() == right.correlation()
}

impl PartialEq for Message {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Request {
                    method,
                    params,
                    correlation,
                    request_id,
                    deadline_ms,
                },
                Self::Request {
                    method: other_method,
                    params: other_params,
                    correlation: other_correlation,
                    request_id: other_request_id,
                    deadline_ms: other_deadline_ms,
                },
            ) => {
                method == other_method
                    && params == other_params
                    && correlation == other_correlation
                    && request_id == other_request_id
                    && deadline_ms == other_deadline_ms
            }
            (
                Self::Response {
                    request_id,
                    correlation,
                    result,
                },
                Self::Response {
                    request_id: other_request_id,
                    correlation: other_correlation,
                    result: other_result,
                },
            ) => {
                request_id == other_request_id
                    && correlation == other_correlation
                    && result == other_result
            }
            (
                Self::Event {
                    topic,
                    payload,
                    correlation,
                },
                Self::Event {
                    topic: other_topic,
                    payload: other_payload,
                    correlation: other_correlation,
                },
            ) => {
                topic == other_topic && payload == other_payload && correlation == other_correlation
            }
            (
                Self::ErrorResponse {
                    request_id,
                    correlation,
                    error,
                },
                Self::ErrorResponse {
                    request_id: other_request_id,
                    correlation: other_correlation,
                    error: other_error,
                },
            ) => {
                request_id == other_request_id
                    && correlation == other_correlation
                    && errors_equivalent(error, other_error)
            }
            _ => false,
        }
    }
}

impl Message {
    /// The frame kind this message travels in.
    #[must_use]
    pub const fn frame_kind(&self) -> FrameKind {
        match self {
            Self::Request { .. } => FrameKind::Request,
            Self::Response { .. } => FrameKind::Response,
            Self::Event { .. } => FrameKind::Event,
            Self::ErrorResponse { .. } => FrameKind::ErrorResponse,
        }
    }

    /// The payload discriminator byte for this message.
    const fn discriminator(&self) -> u8 {
        match self {
            Self::Request { .. } => DISCRIMINATOR_REQUEST,
            Self::Response { .. } => DISCRIMINATOR_RESPONSE,
            Self::Event { .. } => DISCRIMINATOR_EVENT,
            Self::ErrorResponse { .. } => DISCRIMINATOR_ERROR_RESPONSE,
        }
    }

    /// The correlation id this message carries.
    #[must_use]
    pub const fn correlation(&self) -> CorrelationId {
        match self {
            Self::Request { correlation, .. }
            | Self::Response { correlation, .. }
            | Self::Event { correlation, .. }
            | Self::ErrorResponse { correlation, .. } => *correlation,
        }
    }

    /// The request this message concerns, when it concerns one.
    ///
    /// Returns `None` for an event, which is unsolicited by definition. A transport uses this
    /// to route a message to the right waiter without matching on the variant first.
    #[must_use]
    pub const fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Request { request_id, .. }
            | Self::Response { request_id, .. }
            | Self::ErrorResponse { request_id, .. } => Some(*request_id),
            Self::Event { .. } => None,
        }
    }

    /// The method or topic this message carries.
    ///
    /// A dispatcher switches on this string; it is the only routing key in the protocol.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Request { method, .. } => method,
            Self::Event { topic, .. } => topic,
            Self::Response { .. } | Self::ErrorResponse { .. } => "",
        }
    }

    /// Encodes the message into its frame payload, without a frame header.
    ///
    /// Split from framing so the payload layout can be tested on its own, and so a batch can
    /// reuse it. Most callers want [`Message::encode`], which adds the header.
    pub fn encode_payload(&self) -> Result<Vec<u8>> {
        let mut out = BufWriter::new();
        out.put_u8(PAYLOAD_VERSION);
        out.put_u8(self.discriminator());
        match self {
            Self::Request {
                method,
                params,
                correlation,
                request_id,
                deadline_ms,
            } => {
                out.put_u128(correlation.as_raw());
                out.put_u128(request_id.as_raw());
                out.put_u64(*deadline_ms);
                out.put_str(method)?;
                out.put_bytes(params)?;
            }
            Self::Response {
                request_id,
                correlation,
                result,
            } => {
                out.put_u128(request_id.as_raw());
                out.put_u128(correlation.as_raw());
                out.put_bytes(result)?;
            }
            Self::Event {
                topic,
                payload,
                correlation,
            } => {
                out.put_u128(correlation.as_raw());
                out.put_str(topic)?;
                out.put_bytes(payload)?;
            }
            Self::ErrorResponse {
                request_id,
                correlation,
                error,
            } => {
                out.put_u128(request_id.as_raw());
                out.put_u128(correlation.as_raw());
                write_error(error, &mut out)?;
            }
        }
        Ok(out.finish())
    }

    /// Decodes a message from a frame payload.
    ///
    /// `expected_kind` is the kind from the frame header. Both it and the payload's own
    /// discriminator must agree with the payload's contents, so a frame can never be read as a
    /// different message than it was sent as.
    pub fn decode_payload(kind: FrameKind, payload: &[u8]) -> Result<Self> {
        let expected_discriminator = match kind {
            FrameKind::Request => DISCRIMINATOR_REQUEST,
            FrameKind::Response => DISCRIMINATOR_RESPONSE,
            FrameKind::Event => DISCRIMINATOR_EVENT,
            FrameKind::ErrorResponse => DISCRIMINATOR_ERROR_RESPONSE,
            // A batch frame carries no message of its own, so it has no discriminator. The
            // batching layer unwraps it before reaching here; reaching this arm means the
            // caller skipped that, which is a protocol violation rather than a guess.
            FrameKind::Batch => {
                return Err(NcError::new(ErrorKind::Protocol {
                    reason: "a batch frame carries no message; unwrap it with decode_messages"
                        .to_owned(),
                })
                .operation("ipc::message::decode_payload"));
            }
        };
        let mut reader = frame::check_payload_header(payload, kind, expected_discriminator)?;
        let message = match kind {
            FrameKind::Request => {
                let correlation = decode_correlation(reader.get_u128()?)?;
                let request_id = RequestId::from_raw(reader.get_u128()?).ok_or_else(|| {
                    invalid_id("request id zero is reserved as the null identifier")
                })?;
                let deadline_ms = reader.get_u64()?;
                let method = reader.get_str()?.to_owned();
                let params = reader.get_bytes()?.to_vec();
                Self::Request {
                    method,
                    params,
                    correlation,
                    request_id,
                    deadline_ms,
                }
            }
            FrameKind::Response => {
                let request_id = RequestId::from_raw(reader.get_u128()?).ok_or_else(|| {
                    invalid_id("request id zero is reserved as the null identifier")
                })?;
                let correlation = decode_correlation(reader.get_u128()?)?;
                let result = reader.get_bytes()?.to_vec();
                Self::Response {
                    request_id,
                    correlation,
                    result,
                }
            }
            FrameKind::Event => {
                let correlation = decode_correlation(reader.get_u128()?)?;
                let topic = reader.get_str()?.to_owned();
                let payload = reader.get_bytes()?.to_vec();
                Self::Event {
                    topic,
                    payload,
                    correlation,
                }
            }
            FrameKind::ErrorResponse => {
                let request_id = RequestId::from_raw(reader.get_u128()?).ok_or_else(|| {
                    invalid_id("request id zero is reserved as the null identifier")
                })?;
                let correlation = decode_correlation(reader.get_u128()?)?;
                let error = read_error(&mut reader)?;
                Self::ErrorResponse {
                    request_id,
                    correlation,
                    error,
                }
            }
            FrameKind::Batch => {
                return Err(NcError::new(ErrorKind::Protocol {
                    reason: "a batch frame carries no message; unwrap it with decode_messages"
                        .to_owned(),
                })
                .operation("ipc::message::decode_payload"));
            }
        };
        reader.expect_end()?;
        Ok(message)
    }

    /// Encodes this message as a single complete frame.
    ///
    /// The encoded frame is checked against `max_frame_bytes` before it is returned, so a
    /// caller that ignores the result cannot put an oversized frame on the wire for a peer to
    /// reject. The check costs one comparison and keeps the failure where it can still be
    /// acted on.
    pub fn encode(&self, max_frame_bytes: u32) -> Result<Vec<u8>> {
        let payload = self.encode_payload()?;
        encode_payload_in_frame(self.frame_kind(), &payload, max_frame_bytes)
    }
}

/// Builds a protocol error attributed to message encoding or decoding.
fn invalid_id(reason: &'static str) -> NcError {
    NcError::new(ErrorKind::InvalidId {
        reason: reason.to_owned(),
    })
    .operation("ipc::message")
}

/// Encodes one payload as a frame, enforcing the frame size limit.
///
/// The limit covers the header as well, because
/// [`Limits::max_ipc_frame_bytes`] is specified in whole frames and a caller comparing against
/// it must not be surprised by twelve extra bytes.
pub fn encode_payload_in_frame(
    kind: FrameKind,
    payload: &[u8],
    max_frame_bytes: u32,
) -> Result<Vec<u8>> {
    let total = FRAME_HEADER_BYTES + payload.len();
    let total_u32 = u32::try_from(total).map_err(|_| {
        NcError::new(ErrorKind::LimitExceeded {
            resource: "ipc.frame.bytes".to_owned(),
            limit: u64::from(max_frame_bytes),
            observed: total as u64,
        })
    })?;
    if total_u32 > max_frame_bytes {
        return Err(NcError::new(ErrorKind::LimitExceeded {
            resource: "ipc.frame.bytes".to_owned(),
            limit: u64::from(max_frame_bytes),
            observed: u64::from(total_u32),
        })
        .operation("ipc::message::encode"));
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        NcError::new(ErrorKind::LimitExceeded {
            resource: "ipc.frame.payload_bytes".to_owned(),
            limit: u64::from(max_frame_bytes),
            observed: payload.len() as u64,
        })
        .operation("ipc::message::encode")
    })?;
    let header = FrameHeader::new(kind, FrameFlags::NONE, payload_len);
    Ok(frame::encode_frame(&header, payload))
}

/// Encodes messages into the fewest frames possible.
///
/// A single message becomes one frame. Two or more become one `BATCH` frame, because at high
/// message rates the per-message transport write is the dominant cost, and a batched write is
/// still fully framed per message. Encoding an empty slice yields an empty buffer, not an
/// empty batch: there is nothing to say, and an empty batch frame would be a frame a peer has
/// to special-case.
pub fn encode_messages(messages: &[Message], max_frame_bytes: u32) -> Result<Vec<u8>> {
    // Encode every message up front so a limit violation is reported before any bytes exist,
    // rather than leaving the caller with a partial buffer it has to discard.
    let mut payloads = Vec::with_capacity(messages.len());
    for message in messages {
        payloads.push(message.encode_payload()?);
    }
    if payloads.len() <= 1 {
        let Some((message, payload)) = messages.first().zip(payloads.first()) else {
            return Ok(Vec::new());
        };
        return encode_payload_in_frame(message.frame_kind(), payload, max_frame_bytes);
    }
    let mut batch = Vec::new();
    for (message, payload) in messages.iter().zip(&payloads) {
        batch.extend_from_slice(&encode_payload_in_frame(
            message.frame_kind(),
            payload,
            max_frame_bytes,
        )?);
    }
    frame::encode_batch_frame(&batch, max_frame_bytes)
}

/// Decodes the messages of a complete byte stream.
///
/// Applies the framing rules, the batch rules and the payload rules, in that order, so a
/// violation is reported at the layer that detected it. This is the entry point a transport
/// uses: bytes in, messages out.
pub fn decode_messages(bytes: &[u8], max_frame_bytes: u32) -> Result<Vec<Message>> {
    let mut decoder = FrameDecoder::new(max_frame_bytes)?;
    decoder.feed(bytes)?;
    let mut messages = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        decode_frame_messages(&frame, max_frame_bytes, &mut messages)?;
    }
    decoder.check_complete()?;
    Ok(messages)
}

/// Decodes one frame, appending the messages it carries to `out`.
///
/// A `BATCH` frame delegates to [`frame::decode_batch`], so the nesting rule is enforced in one
/// place. Returns nothing so a batch can append many messages without an intermediate
/// allocation per message.
pub fn decode_frame_messages(
    frame: &Frame,
    max_frame_bytes: u32,
    out: &mut Vec<Message>,
) -> Result<()> {
    let header = *frame.header();
    if header.is_batch() {
        for inner in frame::decode_batch(frame.payload(), max_frame_bytes)? {
            decode_single(&inner, out)?;
        }
        return Ok(());
    }
    decode_single(frame, out)
}

/// Decodes one non-batch frame.
fn decode_single(frame: &Frame, out: &mut Vec<Message>) -> Result<()> {
    let kind = frame.header().kind;
    out.push(Message::decode_payload(kind, frame.payload())?);
    Ok(())
}

// -- JSON control plane -------------------------------------------------------------------
//
// The representations below are the agent-facing form. They are `serde`-derived rather than
// hand-written so that a field cannot be added to one and forgotten in the other, and so that
// the schema is checkable with `serde_json` without the binary layout being involved.

/// The JSON shape of a [`Message`].
///
/// `params`, `result` and `payload` are byte arrays rather than strings. An agent has to be
/// able to read them, and base64 would only move the decoding problem somewhere with fewer
/// diagnostics; a JSON array of numbers is unambiguous, and this path is not throughput
/// sensitive.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MessageJson {
    Request {
        method: String,
        params: Vec<u8>,
        correlation: CorrelationId,
        request_id: RequestId,
        deadline_ms: u64,
    },
    Response {
        request_id: RequestId,
        correlation: CorrelationId,
        result: Vec<u8>,
    },
    Event {
        topic: String,
        payload: Vec<u8>,
        correlation: CorrelationId,
    },
    ErrorResponse {
        request_id: RequestId,
        correlation: CorrelationId,
        error: ErrorJson,
    },
}

/// The JSON shape of an [`NcError`].
///
/// `NcError`'s own `Serialize` impl is lossy — it emits a code, a rendered message and the
/// operation, but not the structured fields of the `ErrorKind`. That is the right shape for a
/// log line and the wrong one for a wire or API format, because a consumer could not tell a
/// `LimitExceeded` carrying its limit and observed values from any other limit violation. This
/// mirror keeps those fields, and the tests assert the two representations agree.
#[derive(Debug, Serialize, Deserialize)]
struct ErrorJson {
    /// The error's machine-readable code, e.g. `"limit_exceeded"`.
    code: String,
    /// The operation that failed.
    operation: String,
    /// The correlation of the request this error belongs to, if any.
    correlation: Option<CorrelationId>,
    /// The structured kind.
    kind: ErrorKindJson,
}

/// The JSON shape of an [`ncui_core::ErrorKind`], tagged by variant name.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "variant", rename_all = "snake_case")]
enum ErrorKindJson {
    InvalidId {
        reason: String,
    },
    CapabilityDenied {
        capability: String,
        subject: String,
    },
    Validation {
        field: String,
        reason: String,
    },
    LimitExceeded {
        resource: String,
        limit: u64,
        observed: u64,
    },
    Timeout {
        elapsed_ms: u64,
    },
    Cancelled,
    NotFound {
        kind: String,
        id: String,
    },
    Conflict {
        reason: String,
    },
    Protocol {
        reason: String,
    },
    Backpressure {
        channel: String,
        capacity: u64,
    },
    Io {
        operation: String,
        io_kind: String,
        message: String,
    },
    Codec {
        operation: String,
        reason: String,
    },
    Internal {
        operation: String,
        reason: String,
    },
}

impl From<&ErrorKindJson> for ErrorKind {
    fn from(json: &ErrorKindJson) -> Self {
        match json {
            ErrorKindJson::InvalidId { reason } => Self::InvalidId {
                reason: reason.clone(),
            },
            ErrorKindJson::CapabilityDenied {
                capability,
                subject,
            } => Self::CapabilityDenied {
                capability: capability.clone(),
                subject: subject.clone(),
            },
            ErrorKindJson::Validation { field, reason } => Self::Validation {
                field: field.clone(),
                reason: reason.clone(),
            },
            ErrorKindJson::LimitExceeded {
                resource,
                limit,
                observed,
            } => Self::LimitExceeded {
                resource: resource.clone(),
                limit: *limit,
                observed: *observed,
            },
            ErrorKindJson::Timeout { elapsed_ms } => Self::Timeout {
                elapsed_ms: *elapsed_ms,
            },
            ErrorKindJson::Cancelled => Self::Cancelled,
            ErrorKindJson::NotFound { kind, id } => Self::NotFound {
                kind: kind.clone(),
                id: id.clone(),
            },
            ErrorKindJson::Conflict { reason } => Self::Conflict {
                reason: reason.clone(),
            },
            ErrorKindJson::Protocol { reason } => Self::Protocol {
                reason: reason.clone(),
            },
            ErrorKindJson::Backpressure { channel, capacity } => Self::Backpressure {
                channel: channel.clone(),
                capacity: *capacity as usize,
            },
            ErrorKindJson::Io {
                operation,
                io_kind,
                message,
            } => Self::Io {
                operation: operation.clone(),
                // The same table the binary codec uses, so the two representations cannot
                // disagree about what a name means.
                io_kind: crate::error_codec::io_kind_from_name(io_kind),
                message: message.clone(),
            },
            ErrorKindJson::Codec { operation, reason } => Self::Codec {
                operation: operation.clone(),
                reason: reason.clone(),
            },
            ErrorKindJson::Internal { operation, reason } => Self::Internal {
                operation: operation.clone(),
                reason: reason.clone(),
            },
        }
    }
}

impl From<&ErrorKind> for ErrorKindJson {
    fn from(kind: &ErrorKind) -> Self {
        match kind {
            ErrorKind::InvalidId { reason } => Self::InvalidId {
                reason: reason.clone(),
            },
            ErrorKind::CapabilityDenied {
                capability,
                subject,
            } => Self::CapabilityDenied {
                capability: capability.clone(),
                subject: subject.clone(),
            },
            ErrorKind::Validation { field, reason } => Self::Validation {
                field: field.clone(),
                reason: reason.clone(),
            },
            ErrorKind::LimitExceeded {
                resource,
                limit,
                observed,
            } => Self::LimitExceeded {
                resource: resource.clone(),
                limit: *limit,
                observed: *observed,
            },
            ErrorKind::Timeout { elapsed_ms } => Self::Timeout {
                elapsed_ms: *elapsed_ms,
            },
            ErrorKind::Cancelled => Self::Cancelled,
            ErrorKind::NotFound { kind, id } => Self::NotFound {
                kind: kind.clone(),
                id: id.clone(),
            },
            ErrorKind::Conflict { reason } => Self::Conflict {
                reason: reason.clone(),
            },
            ErrorKind::Protocol { reason } => Self::Protocol {
                reason: reason.clone(),
            },
            ErrorKind::Backpressure { channel, capacity } => Self::Backpressure {
                channel: channel.clone(),
                capacity: *capacity as u64,
            },
            ErrorKind::Io {
                operation,
                io_kind,
                message,
            } => Self::Io {
                operation: operation.clone(),
                io_kind: crate::error_codec::io_kind_name(*io_kind).to_owned(),
                message: message.clone(),
            },
            ErrorKind::Codec { operation, reason } => Self::Codec {
                operation: operation.clone(),
                reason: reason.clone(),
            },
            ErrorKind::Internal { operation, reason } => Self::Internal {
                operation: operation.clone(),
                reason: reason.clone(),
            },
            other => Self::Internal {
                // The non_exhaustive fallback, mirroring the binary codec: preserve the text
                // rather than inventing a variant we do not know.
                operation: "ipc::json::opaque".to_owned(),
                reason: other.to_string(),
            },
        }
    }
}

impl ErrorJson {
    /// Builds the JSON form of an error.
    fn from_error(error: &NcError) -> Self {
        Self {
            code: error.code().to_owned(),
            operation: error.operation_name().to_owned(),
            correlation: error.correlation(),
            kind: ErrorKindJson::from(error.kind()),
        }
    }

    /// Rebuilds the error.
    fn to_error(&self) -> NcError {
        let mut error = NcError::new(ErrorKind::from(&self.kind)).operation(self.operation.clone());
        if let Some(correlation) = self.correlation {
            error = error.correlated(correlation);
        }
        error
    }
}

impl Message {
    /// Converts to the JSON control-plane representation.
    ///
    /// Fallible rather than infallible-by-assertion: every field here happens to be a value
    /// `serde_json` always renders, but returning `Result` means that if that ever stops being
    /// true the caller finds out, instead of receiving a `Null` that parses as a real answer.
    pub fn to_json_value(&self) -> Result<serde_json::Value> {
        let json = match self {
            Self::Request {
                method,
                params,
                correlation,
                request_id,
                deadline_ms,
            } => MessageJson::Request {
                method: method.clone(),
                params: params.clone(),
                correlation: *correlation,
                request_id: *request_id,
                deadline_ms: *deadline_ms,
            },
            Self::Response {
                request_id,
                correlation,
                result,
            } => MessageJson::Response {
                request_id: *request_id,
                correlation: *correlation,
                result: result.clone(),
            },
            Self::Event {
                topic,
                payload,
                correlation,
            } => MessageJson::Event {
                topic: topic.clone(),
                payload: payload.clone(),
                correlation: *correlation,
            },
            Self::ErrorResponse {
                request_id,
                correlation,
                error,
            } => MessageJson::ErrorResponse {
                request_id: *request_id,
                correlation: *correlation,
                error: ErrorJson::from_error(error),
            },
        };
        serde_json::to_value(json).map_err(|e| {
            NcError::new(ErrorKind::Codec {
                operation: "ipc::message::to_json".to_owned(),
                reason: e.to_string(),
            })
        })
    }

    /// Converts to a compact JSON string.
    pub fn to_json_string(&self) -> Result<String> {
        serde_json::to_string(&self.to_json_value()?).map_err(|e| {
            NcError::new(ErrorKind::Codec {
                operation: "ipc::message::to_json".to_owned(),
                reason: e.to_string(),
            })
        })
    }

    /// Rebuilds a message from its JSON control-plane representation.
    pub fn from_json_value(value: &serde_json::Value) -> Result<Self> {
        let json: MessageJson = decode_json(value, "ipc::message::from_json")?;
        let message = match json {
            MessageJson::Request {
                method,
                params,
                correlation,
                request_id,
                deadline_ms,
            } => Self::Request {
                method,
                params,
                correlation,
                request_id,
                deadline_ms,
            },
            MessageJson::Response {
                request_id,
                correlation,
                result,
            } => Self::Response {
                request_id,
                correlation,
                result,
            },
            MessageJson::Event {
                topic,
                payload,
                correlation,
            } => Self::Event {
                topic,
                payload,
                correlation,
            },
            MessageJson::ErrorResponse {
                request_id,
                correlation,
                error,
            } => Self::ErrorResponse {
                request_id,
                correlation,
                error: error.to_error(),
            },
        };
        Ok(message)
    }

    /// Rebuilds a message from a JSON string.
    pub fn from_json_str(text: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| {
            NcError::new(ErrorKind::Codec {
                operation: "ipc::message::from_json".to_owned(),
                reason: e.to_string(),
            })
        })?;
        Self::from_json_value(&value)
    }
}

/// Deserialises `value` into `T`, attributing failure to `operation`.
fn decode_json<T: DeserializeOwned>(
    value: &serde_json::Value,
    operation: &'static str,
) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|e| {
        NcError::new(ErrorKind::Codec {
            operation: operation.to_owned(),
            reason: e.to_string(),
        })
    })
}

/// Decodes the messages of a byte stream, taking the frame limit from `limits`.
///
/// The convenience a transport actually wants: one call, no risk of the decoder and the
/// configuration disagreeing about how large a frame may be.
pub fn decode_messages_with_limits(bytes: &[u8], limits: &Limits) -> Result<Vec<Message>> {
    decode_messages(bytes, limits.max_ipc_frame_bytes)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )]

    use super::*;

    fn ids() -> (CorrelationId, RequestId) {
        (CorrelationId::new(), RequestId::new())
    }

    /// One message of each variant, for table-driven assertions.
    fn all_variants() -> Vec<Message> {
        let (correlation, request_id) = ids();
        vec![
            Message::Request {
                method: "window.create".to_owned(),
                params: b"{}".to_vec(),
                correlation,
                request_id,
                deadline_ms: 1_000,
            },
            Message::Response {
                request_id,
                correlation,
                result: b"{\"ok\":true}".to_vec(),
            },
            Message::Event {
                topic: "window.closed".to_owned(),
                payload: Vec::new(),
                correlation,
            },
            Message::ErrorResponse {
                request_id,
                correlation,
                error: NcError::new(ErrorKind::Cancelled).operation("ipc::test"),
            },
        ]
    }

    #[test]
    fn accessors_agree_with_the_variant() {
        let (correlation, request_id) = ids();
        let request = Message::Request {
            method: "window.create".to_owned(),
            params: Vec::new(),
            correlation,
            request_id,
            deadline_ms: 0,
        };
        assert_eq!(request.frame_kind(), FrameKind::Request);
        assert_eq!(request.correlation(), correlation);
        assert_eq!(request.request_id(), Some(request_id));
        assert_eq!(request.name(), "window.create");

        let event = Message::Event {
            topic: "window.closed".to_owned(),
            payload: Vec::new(),
            correlation,
        };
        assert_eq!(event.frame_kind(), FrameKind::Event);
        // An event is unsolicited, so it must not claim a request id a responder could match.
        assert_eq!(event.request_id(), None);
        assert_eq!(event.name(), "window.closed");
    }

    #[test]
    fn only_the_variants_that_answer_something_name_a_request() {
        // The event is the odd one out, and that is the point: a subscriber that matched a
        // request id against an event would find none, which is correct, not a bug.
        for message in all_variants() {
            let expected = !matches!(message, Message::Event { .. });
            assert_eq!(
                message.request_id().is_some(),
                expected,
                "{} should {} a request id",
                message.frame_kind(),
                if expected { "carry" } else { "not carry" }
            );
        }
    }

    #[test]
    fn messages_of_different_kinds_are_never_equal() {
        // A response must never compare equal to an error response, even for the same ids:
        // collapsing them would let a failure be read as a success.
        let (correlation, request_id) = ids();
        let response = Message::Response {
            request_id,
            correlation,
            result: Vec::new(),
        };
        let error = Message::ErrorResponse {
            request_id,
            correlation,
            error: NcError::new(ErrorKind::Cancelled),
        };
        assert_ne!(response, error);
        assert_ne!(error, response);
    }

    #[test]
    fn an_error_response_equality_compares_the_errors_observable_state() {
        let (correlation, request_id) = ids();
        let make = |error| Message::ErrorResponse {
            request_id,
            correlation,
            error,
        };
        let a = make(
            NcError::new(ErrorKind::Timeout { elapsed_ms: 5 })
                .operation("op")
                .correlated(correlation),
        );
        let b = make(
            NcError::new(ErrorKind::Timeout { elapsed_ms: 5 })
                .operation("op")
                .correlated(correlation),
        );
        assert_eq!(a, b, "identical errors compare equal");

        let different_operation = make(
            NcError::new(ErrorKind::Timeout { elapsed_ms: 5 })
                .operation("other")
                .correlated(correlation),
        );
        assert_ne!(
            a, different_operation,
            "the operation is part of the identity"
        );

        let different_correlation = make(
            NcError::new(ErrorKind::Timeout { elapsed_ms: 5 })
                .operation("op")
                .correlated(CorrelationId::new()),
        );
        assert_ne!(
            a, different_correlation,
            "the correlation is part of the identity"
        );

        let different_kind = make(
            NcError::new(ErrorKind::Cancelled)
                .operation("op")
                .correlated(correlation),
        );
        assert_ne!(a, different_kind, "the kind is part of the identity");
    }

    #[test]
    fn a_payload_truncated_at_any_offset_is_rejected() {
        for message in all_variants() {
            let payload = message.encode_payload().expect("encode payload");
            for cut in 0..payload.len() {
                let outcome = Message::decode_payload(message.frame_kind(), &payload[..cut]);
                assert!(
                    outcome.is_err(),
                    "truncating {cut} byte(s) of a {} payload must be rejected",
                    message.frame_kind()
                );
            }
        }
    }

    #[test]
    fn a_payload_with_an_extra_trailing_byte_is_rejected() {
        for message in all_variants() {
            let mut payload = message.encode_payload().expect("encode payload");
            payload.push(0xFF);
            assert!(
                Message::decode_payload(message.frame_kind(), &payload).is_err(),
                "trailing bytes must be rejected"
            );
        }
    }

    #[test]
    fn a_payload_decoded_under_the_wrong_frame_kind_is_rejected() {
        for message in all_variants() {
            let payload = message.encode_payload().expect("encode payload");
            for kind in [
                FrameKind::Request,
                FrameKind::Response,
                FrameKind::Event,
                FrameKind::ErrorResponse,
            ] {
                if kind == message.frame_kind() {
                    continue;
                }
                assert!(
                    Message::decode_payload(kind, &payload).is_err(),
                    "a {} payload must not decode as {kind}",
                    message.frame_kind()
                );
            }
        }
    }

    #[test]
    fn an_unsupported_payload_version_is_rejected() {
        let message = all_variants().remove(0);
        let mut payload = message.encode_payload().expect("encode payload");
        payload[0] = PAYLOAD_VERSION.wrapping_add(1);
        let err = Message::decode_payload(message.frame_kind(), &payload)
            .expect_err("an unknown payload version must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::Protocol { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn an_unknown_discriminator_is_rejected() {
        let mut payload = all_variants().remove(0).encode_payload().expect("encode");
        payload[1] = 0xEE;
        let err = Message::decode_payload(FrameKind::Request, &payload)
            .expect_err("an unknown discriminator must be rejected");
        assert!(err.to_string().contains("does not match"), "got: {err}");
    }

    #[test]
    fn a_length_prefix_past_the_end_of_the_payload_is_rejected() {
        // A request whose method length claims far more than the payload holds. The reader
        // must compare against what is actually there, not allocate first.
        let mut payload = vec![PAYLOAD_VERSION, 0x01];
        payload.extend_from_slice(&1u128.to_be_bytes());
        payload.extend_from_slice(&1u128.to_be_bytes());
        payload.extend_from_slice(&0u64.to_be_bytes());
        payload.extend_from_slice(&u32::MAX.to_be_bytes()); // method length
        payload.extend_from_slice(&u32::MAX.to_be_bytes()); // params length
        assert!(Message::decode_payload(FrameKind::Request, &payload).is_err());
    }

    #[test]
    fn invalid_utf8_in_a_method_is_rejected() {
        let mut payload = vec![PAYLOAD_VERSION, 0x01];
        payload.extend_from_slice(&1u128.to_be_bytes());
        payload.extend_from_slice(&1u128.to_be_bytes());
        payload.extend_from_slice(&0u64.to_be_bytes());
        payload.extend_from_slice(&2u32.to_be_bytes());
        payload.extend_from_slice(&[0xFF, 0xFE]); // never valid UTF-8
        payload.extend_from_slice(&0u32.to_be_bytes());
        let err = Message::decode_payload(FrameKind::Request, &payload)
            .expect_err("invalid UTF-8 must be rejected");
        assert!(err.to_string().contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn a_batch_frame_is_not_a_message() {
        let payload = vec![PAYLOAD_VERSION, 0x01];
        assert!(Message::decode_payload(FrameKind::Batch, &payload).is_err());
        assert!(Message::decode_payload(FrameKind::Batch, &[]).is_err());
    }

    #[test]
    fn a_json_object_that_is_not_a_message_is_rejected() {
        assert!(Message::from_json_str("{}").is_err());
        assert!(Message::from_json_str("null").is_err());
        assert!(Message::from_json_str("[]").is_err());
        assert!(Message::from_json_str("not json").is_err());
    }

    #[test]
    fn json_round_trip_preserves_an_errors_structured_fields() {
        // The reason the JSON form exists at all: `NcError`'s own `Serialize` is lossy, so a
        // message rendered through it could not tell two limit violations apart.
        let (correlation, request_id) = ids();
        let message = Message::ErrorResponse {
            request_id,
            correlation,
            error: NcError::new(ErrorKind::LimitExceeded {
                resource: "ipc.request.in_flight".to_owned(),
                limit: 4096,
                observed: 4097,
            })
            .operation("ipc::request::register")
            .correlated(correlation),
        };
        let json = message.to_json_value().expect("to json");
        let text = serde_json::to_string(&json).expect("render");
        assert!(
            text.contains("4096") && text.contains("4097"),
            "got: {text}"
        );

        let back = Message::from_json_value(&json).expect("from json");
        assert_eq!(back, message);
        match back {
            Message::ErrorResponse { error, .. } => match error.kind() {
                ErrorKind::LimitExceeded {
                    resource,
                    limit,
                    observed,
                } => {
                    assert_eq!(resource, "ipc.request.in_flight");
                    assert_eq!(*limit, 4096);
                    assert_eq!(*observed, 4097);
                }
                other => panic!("expected LimitExceeded, got {other:?}"),
            },
            ref other => panic!("expected an error response, got {other:?}"),
        }
    }

    #[test]
    fn a_json_io_error_round_trips_its_kind_through_the_shared_table() {
        let (correlation, request_id) = ids();
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::WouldBlock,
        ] {
            let message = Message::ErrorResponse {
                request_id,
                correlation,
                error: NcError::new(ErrorKind::Io {
                    operation: "fs::read".to_owned(),
                    io_kind: kind,
                    message: "denied".to_owned(),
                }),
            };
            let back = Message::from_json_value(&message.to_json_value().expect("to json"))
                .expect("from json");
            assert_eq!(back, message, "io kind {kind:?} did not survive JSON");
        }
    }
}
