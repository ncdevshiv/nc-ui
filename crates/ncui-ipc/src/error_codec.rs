//! Wire representation of [`NcError`].
//!
//! [`NcError`] serialises to a human-readable JSON object, but that object is lossy: it
//! carries the error *code*, its rendered message and its operation, not the structured fields
//! of the underlying [`ErrorKind`]. That is the right shape for a log line and the wrong shape
//! for a wire format, because a peer that received it could not tell a `LimitExceeded`
//! (limit 4096, observed 4097) from any other limit violation.
//!
//! This module therefore encodes every `ErrorKind` variant structurally, so that an
//! [`crate::Message::ErrorResponse`] survives a round trip with its fields intact. It is
//! private: callers see [`NcError`] in and out.
//!
//! [`NcError`]: ncui_core::NcError
//! [`ErrorKind`]: ncui_core::ErrorKind

use ncui_core::{CorrelationId, ErrorKind, NcError, Result};

use crate::codec::{BufReader, BufWriter};

/// Version of the error record layout.
///
/// Bumped independently of the message payload version so that an error record can evolve
/// without dragging the envelope layout along with it.
pub(crate) const ERROR_RECORD_VERSION: u8 = 1;

/// A kind this build does not recognise, carried as text.
///
/// `ErrorKind` is `#[non_exhaustive]`, so a peer built against a newer `ncui-core` may send a
/// variant this build has never heard of. Discarding it would lose the only diagnostic the
/// sender has; mis-encoding it would corrupt a variant we *do* understand. Keeping the rendered
/// text and surfacing it as an internal failure is the one option that loses nothing silently.
const TAG_OPAQUE: u8 = 0x00;

const TAG_INVALID_ID: u8 = 0x01;
const TAG_CAPABILITY_DENIED: u8 = 0x02;
const TAG_VALIDATION: u8 = 0x03;
const TAG_LIMIT_EXCEEDED: u8 = 0x04;
const TAG_TIMEOUT: u8 = 0x05;
const TAG_CANCELLED: u8 = 0x06;
const TAG_NOT_FOUND: u8 = 0x07;
const TAG_CONFLICT: u8 = 0x08;
const TAG_PROTOCOL: u8 = 0x09;
const TAG_BACKPRESSURE: u8 = 0x0A;
const TAG_IO: u8 = 0x0B;
const TAG_CODEC: u8 = 0x0C;
const TAG_INTERNAL: u8 = 0x0D;

/// Operation name recorded on an error recovered from an unrecognised tag.
const OPAQUE_OPERATION: &str = "ipc::error::opaque";

/// Every stable [`std::io::ErrorKind`] this build can name, with its wire name.
///
/// `std::io::ErrorKind` is `#[non_exhaustive]`, has no `TryFrom<u8>`, and its `as_str` is
/// private — so the only stable way to carry one across a process boundary is by name. The
/// table is the single source of truth for both directions; keeping two hand-written matches
/// in step with each other would be a defect waiting to happen.
///
/// An unrecognised variant encodes as [`std::io::ErrorKind::Other`] and an unrecognised name
/// decodes as [`std::io::ErrorKind::Other`]. That degradation is deliberate and lossy in a
/// documented way, and it is limited to a diagnostic classification: no control-flow decision
/// in the runtime branches on `io::ErrorKind`, only log lines and `is_retryable` do.
const IO_KINDS: &[(std::io::ErrorKind, &str)] = &[
    (std::io::ErrorKind::NotFound, "NotFound"),
    (std::io::ErrorKind::PermissionDenied, "PermissionDenied"),
    (std::io::ErrorKind::ConnectionRefused, "ConnectionRefused"),
    (std::io::ErrorKind::ConnectionReset, "ConnectionReset"),
    (std::io::ErrorKind::HostUnreachable, "HostUnreachable"),
    (std::io::ErrorKind::NetworkUnreachable, "NetworkUnreachable"),
    (std::io::ErrorKind::ConnectionAborted, "ConnectionAborted"),
    (std::io::ErrorKind::NotConnected, "NotConnected"),
    (std::io::ErrorKind::AddrInUse, "AddrInUse"),
    (std::io::ErrorKind::AddrNotAvailable, "AddrNotAvailable"),
    (std::io::ErrorKind::NetworkDown, "NetworkDown"),
    (std::io::ErrorKind::BrokenPipe, "BrokenPipe"),
    (std::io::ErrorKind::AlreadyExists, "AlreadyExists"),
    (std::io::ErrorKind::WouldBlock, "WouldBlock"),
    (std::io::ErrorKind::InvalidInput, "InvalidInput"),
    (std::io::ErrorKind::InvalidData, "InvalidData"),
    (std::io::ErrorKind::TimedOut, "TimedOut"),
    (std::io::ErrorKind::WriteZero, "WriteZero"),
    (std::io::ErrorKind::Interrupted, "Interrupted"),
    (std::io::ErrorKind::Unsupported, "Unsupported"),
    (std::io::ErrorKind::UnexpectedEof, "UnexpectedEof"),
    (std::io::ErrorKind::OutOfMemory, "OutOfMemory"),
    (std::io::ErrorKind::IsADirectory, "IsADirectory"),
    (std::io::ErrorKind::DirectoryNotEmpty, "DirectoryNotEmpty"),
    (std::io::ErrorKind::ReadOnlyFilesystem, "ReadOnlyFilesystem"),
    (
        std::io::ErrorKind::StaleNetworkFileHandle,
        "StaleNetworkFileHandle",
    ),
    (std::io::ErrorKind::StorageFull, "StorageFull"),
    (std::io::ErrorKind::NotADirectory, "NotADirectory"),
    (std::io::ErrorKind::QuotaExceeded, "QuotaExceeded"),
    (std::io::ErrorKind::FileTooLarge, "FileTooLarge"),
    (std::io::ErrorKind::ResourceBusy, "ResourceBusy"),
    (std::io::ErrorKind::ExecutableFileBusy, "ExecutableFileBusy"),
    (std::io::ErrorKind::Deadlock, "Deadlock"),
    (std::io::ErrorKind::CrossesDevices, "CrossesDevices"),
    (std::io::ErrorKind::TooManyLinks, "TooManyLinks"),
    (std::io::ErrorKind::InvalidFilename, "InvalidFilename"),
    (
        std::io::ErrorKind::ArgumentListTooLong,
        "ArgumentListTooLong",
    ),
    (std::io::ErrorKind::Other, "Other"),
];

/// The wire name for a [`std::io::ErrorKind`].
///
/// Shared with the JSON control plane so the two representations name an io kind identically.
pub(crate) fn io_kind_name(kind: std::io::ErrorKind) -> &'static str {
    IO_KINDS
        .iter()
        .find(|(candidate, _)| *candidate == kind)
        .map_or("Other", |(_, name)| *name)
}

/// The [`std::io::ErrorKind`] a wire name denotes.
///
/// Shared with the JSON control plane so the two representations name an io kind identically.
pub(crate) fn io_kind_from_name(name: &str) -> std::io::ErrorKind {
    IO_KINDS
        .iter()
        .find(|(_, candidate)| *candidate == name)
        .map_or(std::io::ErrorKind::Other, |(kind, _)| *kind)
}

/// Writes the structured fields of one `ErrorKind`, excluding its tag.
///
/// Split out so the tag table stays readable and each variant's field order is obvious at a
/// glance. The order here is the contract; the decoder below reads it back in exactly this
/// order.
fn write_kind_fields(kind: &ErrorKind, out: &mut BufWriter) -> Result<()> {
    match kind {
        ErrorKind::CapabilityDenied {
            capability,
            subject,
        } => {
            out.put_str(capability)?;
            out.put_str(subject)?;
        }
        ErrorKind::Validation { field, reason } => {
            out.put_str(field)?;
            out.put_str(reason)?;
        }
        ErrorKind::LimitExceeded {
            resource,
            limit,
            observed,
        } => {
            out.put_str(resource)?;
            out.put_u64(*limit);
            out.put_u64(*observed);
        }
        ErrorKind::Timeout { elapsed_ms } => out.put_u64(*elapsed_ms),
        ErrorKind::Cancelled => {}
        ErrorKind::NotFound { kind, id } => {
            out.put_str(kind)?;
            out.put_str(id)?;
        }
        // These three differ only in name, and the name is already carried by the tag, so
        // their bodies are genuinely identical.
        ErrorKind::InvalidId { reason }
        | ErrorKind::Conflict { reason }
        | ErrorKind::Protocol { reason } => {
            out.put_str(reason)?;
        }
        ErrorKind::Backpressure { channel, capacity } => {
            out.put_str(channel)?;
            // Carried as u64 rather than usize: a 32-bit peer must be able to describe a
            // capacity this 64-bit host allocated, and vice versa.
            out.put_u64(*capacity as u64);
        }
        ErrorKind::Io {
            operation,
            io_kind,
            message,
        } => {
            out.put_str(operation)?;
            out.put_str(io_kind_name(*io_kind))?;
            out.put_str(message)?;
        }
        ErrorKind::Codec { operation, reason } | ErrorKind::Internal { operation, reason } => {
            out.put_str(operation)?;
            out.put_str(reason)?;
        }
        other => {
            // The non_exhaustive fallback. See `TAG_OPAQUE`.
            out.put_str(&other.to_string())?;
        }
    }
    Ok(())
}

/// The tag byte for a kind.
fn kind_tag(kind: &ErrorKind) -> u8 {
    match kind {
        ErrorKind::InvalidId { .. } => TAG_INVALID_ID,
        ErrorKind::CapabilityDenied { .. } => TAG_CAPABILITY_DENIED,
        ErrorKind::Validation { .. } => TAG_VALIDATION,
        ErrorKind::LimitExceeded { .. } => TAG_LIMIT_EXCEEDED,
        ErrorKind::Timeout { .. } => TAG_TIMEOUT,
        ErrorKind::Cancelled => TAG_CANCELLED,
        ErrorKind::NotFound { .. } => TAG_NOT_FOUND,
        ErrorKind::Conflict { .. } => TAG_CONFLICT,
        ErrorKind::Protocol { .. } => TAG_PROTOCOL,
        ErrorKind::Backpressure { .. } => TAG_BACKPRESSURE,
        ErrorKind::Io { .. } => TAG_IO,
        ErrorKind::Codec { .. } => TAG_CODEC,
        ErrorKind::Internal { .. } => TAG_INTERNAL,
        _ => TAG_OPAQUE,
    }
}

/// Reads the structured fields of one kind, given its tag.
fn read_kind_fields(tag: u8, input: &mut BufReader<'_>) -> Result<ErrorKind> {
    let kind = match tag {
        TAG_INVALID_ID => ErrorKind::InvalidId {
            reason: input.get_str()?.to_owned(),
        },
        TAG_CAPABILITY_DENIED => ErrorKind::CapabilityDenied {
            capability: input.get_str()?.to_owned(),
            subject: input.get_str()?.to_owned(),
        },
        TAG_VALIDATION => ErrorKind::Validation {
            field: input.get_str()?.to_owned(),
            reason: input.get_str()?.to_owned(),
        },
        TAG_LIMIT_EXCEEDED => ErrorKind::LimitExceeded {
            resource: input.get_str()?.to_owned(),
            limit: input.get_u64()?,
            observed: input.get_u64()?,
        },
        TAG_TIMEOUT => ErrorKind::Timeout {
            elapsed_ms: input.get_u64()?,
        },
        TAG_CANCELLED => ErrorKind::Cancelled,
        TAG_NOT_FOUND => ErrorKind::NotFound {
            kind: input.get_str()?.to_owned(),
            id: input.get_str()?.to_owned(),
        },
        TAG_CONFLICT => ErrorKind::Conflict {
            reason: input.get_str()?.to_owned(),
        },
        TAG_PROTOCOL => ErrorKind::Protocol {
            reason: input.get_str()?.to_owned(),
        },
        TAG_BACKPRESSURE => ErrorKind::Backpressure {
            channel: input.get_str()?.to_owned(),
            capacity: input.get_u64()? as usize,
        },
        TAG_IO => {
            let operation = input.get_str()?.to_owned();
            let io_kind = io_kind_from_name(input.get_str()?);
            let message = input.get_str()?.to_owned();
            ErrorKind::Io {
                operation,
                io_kind,
                message,
            }
        }
        TAG_CODEC => ErrorKind::Codec {
            operation: input.get_str()?.to_owned(),
            reason: input.get_str()?.to_owned(),
        },
        TAG_INTERNAL => ErrorKind::Internal {
            operation: input.get_str()?.to_owned(),
            reason: input.get_str()?.to_owned(),
        },
        TAG_OPAQUE => ErrorKind::Internal {
            operation: OPAQUE_OPERATION.to_owned(),
            reason: input.get_str()?.to_owned(),
        },
        unknown => {
            return Err(NcError::new(ErrorKind::Codec {
                operation: "ipc::error::decode".to_owned(),
                reason: format!("unknown error kind tag 0x{unknown:02x}"),
            }));
        }
    };
    Ok(kind)
}

/// Appends `error` to `out` in the error record layout.
pub(crate) fn write_error(error: &NcError, out: &mut BufWriter) -> Result<()> {
    out.put_u8(ERROR_RECORD_VERSION);
    out.put_u8(kind_tag(error.kind()));
    write_kind_fields(error.kind(), out)?;
    out.put_str(error.operation_name())?;
    match error.correlation() {
        Some(id) => {
            out.put_u8(1);
            out.put_u128(id.as_raw());
        }
        None => out.put_u8(0),
    }
    Ok(())
}

/// Reads one error record, rejecting trailing bytes inside the record.
pub(crate) fn read_error(input: &mut BufReader<'_>) -> Result<NcError> {
    let version = input.get_u8()?;
    if version != ERROR_RECORD_VERSION {
        return Err(NcError::new(ErrorKind::Codec {
            operation: "ipc::error::decode".to_owned(),
            reason: format!(
                "unsupported error record version {version}, this build speaks {ERROR_RECORD_VERSION}"
            ),
        }));
    }
    let tag = input.get_u8()?;
    let kind = read_kind_fields(tag, input)?;
    let operation = input.get_str()?.to_owned();
    let correlation = match input.get_u8()? {
        0 => None,
        1 => Some(decode_correlation(input.get_u128()?)?),
        other => {
            return Err(NcError::new(ErrorKind::Codec {
                operation: "ipc::error::decode".to_owned(),
                reason: format!("correlation presence flag must be 0 or 1, got {other}"),
            }));
        }
    };
    let mut error = NcError::new(kind).operation(operation);
    if let Some(id) = correlation {
        error = error.correlated(id);
    }
    Ok(error)
}

/// Decodes a raw 128-bit identifier, rejecting the reserved null value.
///
/// Zero is not a legal identifier: `ncui-core` reserves it so that `Option<Id>` is cheaper
/// than a validity flag. A wire value of zero therefore cannot be turned into an id, and
/// accepting it would produce an id that can never round-trip.
pub(crate) fn decode_correlation(raw: u128) -> Result<CorrelationId> {
    CorrelationId::from_raw(raw).ok_or_else(|| {
        NcError::new(ErrorKind::InvalidId {
            reason: "identifier zero is reserved as the null identifier".to_owned(),
        })
    })
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

    /// Encodes, then decodes, an error and returns the result.
    fn round_trip(error: &NcError) -> Result<NcError> {
        let mut writer = BufWriter::new();
        write_error(error, &mut writer)?;
        let bytes = writer.finish();
        let mut reader = BufReader::new(&bytes, "ipc::error::test");
        let decoded = read_error(&mut reader)?;
        reader.expect_end()?;
        Ok(decoded)
    }

    /// Every `ErrorKind` a caller can construct on this platform.
    fn sample_kinds() -> Vec<ErrorKind> {
        vec![
            ErrorKind::InvalidId {
                reason: "bad".to_owned(),
            },
            ErrorKind::CapabilityDenied {
                capability: "filesystem.write".to_owned(),
                subject: "agent:7".to_owned(),
            },
            ErrorKind::Validation {
                field: "title".to_owned(),
                reason: "empty".to_owned(),
            },
            ErrorKind::LimitExceeded {
                resource: "ipc.in_flight".to_owned(),
                limit: 4096,
                observed: 4097,
            },
            ErrorKind::Timeout { elapsed_ms: 30_000 },
            ErrorKind::Cancelled,
            ErrorKind::NotFound {
                kind: "component".to_owned(),
                id: "abc".to_owned(),
            },
            ErrorKind::Conflict {
                reason: "duplicate request id".to_owned(),
            },
            ErrorKind::Protocol {
                reason: "bad magic".to_owned(),
            },
            ErrorKind::Backpressure {
                channel: "scheduler.queue".to_owned(),
                capacity: 1024,
            },
            ErrorKind::Io {
                operation: "fs::read".to_owned(),
                io_kind: std::io::ErrorKind::PermissionDenied,
                message: "access denied".to_owned(),
            },
            ErrorKind::Codec {
                operation: "ipc::decode".to_owned(),
                reason: "truncated".to_owned(),
            },
            ErrorKind::Internal {
                operation: "registry::register".to_owned(),
                reason: "invariant violated".to_owned(),
            },
        ]
    }

    #[test]
    fn every_kind_survives_a_round_trip_with_its_fields_intact() {
        let correlation = CorrelationId::new();
        for kind in sample_kinds() {
            let original = NcError::new(kind)
                .operation("ipc::test")
                .correlated(correlation);
            let decoded = round_trip(&original).expect("every kind must round-trip");
            assert_eq!(decoded.kind(), original.kind(), "kind mismatch");
            assert_eq!(decoded.operation_name(), "ipc::test");
            assert_eq!(decoded.correlation(), Some(correlation));
        }
    }

    #[test]
    fn an_error_without_a_correlation_round_trips() {
        let original = NcError::new(ErrorKind::Cancelled).operation("ipc::test");
        let decoded = round_trip(&original).unwrap();
        assert_eq!(decoded.correlation(), None);
        assert_eq!(decoded.operation_name(), "ipc::test");
    }

    #[test]
    fn an_empty_operation_survives() {
        // `NcError` allows an empty operation string; it must not be confused with absence.
        let original = NcError::new(ErrorKind::Cancelled).operation("");
        let decoded = round_trip(&original).unwrap();
        assert_eq!(decoded.operation_name(), "");
    }

    #[test]
    fn every_named_io_kind_round_trips() {
        for (kind, name) in IO_KINDS {
            let original = NcError::new(ErrorKind::Io {
                operation: "io".to_owned(),
                io_kind: *kind,
                message: "m".to_owned(),
            });
            let decoded = round_trip(&original).unwrap();
            match decoded.kind() {
                ErrorKind::Io { io_kind, .. } => {
                    assert_eq!(*io_kind, *kind, "io kind {name} did not round-trip");
                }
                other => panic!("expected an Io error, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unknown_io_name_degrades_to_other_rather_than_failing() {
        let mut writer = BufWriter::new();
        writer.put_u8(ERROR_RECORD_VERSION);
        writer.put_u8(TAG_IO);
        writer.put_str("io").unwrap();
        writer.put_str("SomethingFromTheFuture").unwrap();
        writer.put_str("m").unwrap();
        writer.put_str("op").unwrap();
        writer.put_u8(0);
        let bytes = writer.finish();

        let mut reader = BufReader::new(&bytes, "ipc::error::test");
        let decoded = read_error(&mut reader).unwrap();
        match decoded.kind() {
            ErrorKind::Io { io_kind, .. } => assert_eq!(*io_kind, std::io::ErrorKind::Other),
            other => panic!("expected an Io error, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_tag_is_rejected_rather_than_guessed() {
        let mut writer = BufWriter::new();
        writer.put_u8(ERROR_RECORD_VERSION);
        writer.put_u8(0xEE);
        let bytes = writer.finish();
        let mut reader = BufReader::new(&bytes, "ipc::error::test");
        let err = read_error(&mut reader).expect_err("unknown tag must be rejected");
        assert!(
            err.to_string().contains("unknown error kind tag"),
            "got: {err}"
        );
    }

    #[test]
    fn an_unsupported_record_version_is_rejected() {
        let bytes = [ERROR_RECORD_VERSION + 1, TAG_CANCELLED];
        let mut reader = BufReader::new(&bytes, "ipc::error::test");
        let err = read_error(&mut reader).expect_err("bad version must be rejected");
        assert!(
            err.to_string().contains("unsupported error record version"),
            "got: {err}"
        );
    }

    #[test]
    fn a_null_correlation_is_rejected() {
        // Built with the writer rather than a byte literal, so the layout under test is the
        // layout the writer actually produces.
        let mut writer = BufWriter::new();
        writer.put_u8(ERROR_RECORD_VERSION);
        writer.put_u8(TAG_CANCELLED);
        writer.put_str("op").unwrap();
        writer.put_u8(1); // correlation present
        writer.put_u128(0); // the reserved null identifier
        let bytes = writer.finish();

        let mut reader = BufReader::new(&bytes, "ipc::error::test");
        let err = read_error(&mut reader).expect_err("a null correlation must be rejected");
        assert!(
            matches!(err.kind(), ErrorKind::InvalidId { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn a_bad_presence_flag_is_rejected() {
        let mut writer = BufWriter::new();
        writer.put_u8(ERROR_RECORD_VERSION);
        writer.put_u8(TAG_CANCELLED);
        writer.put_str("op").unwrap();
        writer.put_u8(7);
        let bytes = writer.finish();
        let mut reader = BufReader::new(&bytes, "ipc::error::test");
        assert!(read_error(&mut reader).is_err());
    }

    #[test]
    fn a_truncated_record_is_rejected() {
        let mut writer = BufWriter::new();
        write_error(
            &NcError::new(ErrorKind::LimitExceeded {
                resource: "r".to_owned(),
                limit: 1,
                observed: 2,
            })
            .operation("op"),
            &mut writer,
        )
        .unwrap();
        let full = writer.finish();
        for cut in 0..full.len() {
            let mut reader = BufReader::new(&full[..cut], "ipc::error::test");
            assert!(
                read_error(&mut reader).is_err(),
                "truncating to {cut} byte(s) must be rejected"
            );
        }
    }
}
