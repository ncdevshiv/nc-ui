//! The host-side CDP transport (ADR-017 Phase 2).
//!
//! The seam (`ncui.cdp.send`) existed before this module; what it forwarded to was a
//! page-side relay, which is honest about being a relay but leaves a page without its own
//! debugging endpoint nowhere to send a command. This module is the transport the ADR's
//! Phase 2 describes: the engine is created with `--remote-debugging-port=0`, the port is
//! read out of the `WebView2` user-data folder's `DevToolsActivePort` file, and a CDP
//! command is delivered over a loopback WebSocket to the page's own `DevTools` endpoint.
//!
//! # Why the port comes from a file
//!
//! `--remote-debugging-port=0` makes `WebView2` pick a free port, which avoids the collision
//! a fixed port would cause between two instances of the same application. The chosen port
//! (and the bearer path that a modern `WebView2` requires on the WebSocket upgrade) is
//! written by the runtime into `DevToolsActivePort` inside the browser's user-data
//! directory. That file is the platform's own record of what it opened, so reading it is
//! asking the platform rather than guessing.
//!
//! # What this module does not do
//!
//! It carries one command and reads one answer, synchronously, with a bounded wait. A
//! caller that wants a long-lived inspector session opens its own WebSocket to the same
//! endpoint — this module is the runtime's *transport*, not an inspector UI.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// The longest a single WebSocket frame's payload this client will read.
///
/// A `DevTools` answer is a JSON document; even a large heap snapshot summary is far below
/// this. A frame that declares more than this bound is a peer misbehaving, not an answer.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The longest an HTTP head on the WebSocket upgrade may be.
const MAX_HTTP_HEAD_BYTES: usize = 16 * 1024;

/// How long one CDP exchange may take end to end.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// The name of the file the `WebView2` runtime writes the debugging port into.
pub const DEVTOOLS_PORT_FILE: &str = "DevToolsActivePort";
/// Why a CDP command could not be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CdpError {
    /// The `DevToolsActivePort` file is missing or unreadable, which means the engine was
    /// not created with remote debugging on, or has not finished starting.
    PortUnavailable { reason: String },
    /// The endpoint answered and then refused the exchange.
    ConnectFailed { reason: String },
    /// The engine closed the connection or stopped answering within the bound.
    TransportFailed { reason: String },
    /// The command ran but the endpoint reported a failure for it.
    CommandFailed { reason: String },
}

impl CdpError {
    /// The stable wire name, e.g. `"cdp_port_unavailable"`, used in refusals.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PortUnavailable { .. } => "cdp_port_unavailable",
            Self::ConnectFailed { .. } => "cdp_connect_failed",
            Self::TransportFailed { .. } => "cdp_transport_failed",
            Self::CommandFailed { .. } => "cdp_command_failed",
        }
    }
}

impl core::fmt::Display for CdpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PortUnavailable { reason } => {
                write!(f, "CDP port unavailable: {reason}")
            }
            Self::ConnectFailed { reason } => write!(f, "CDP connect failed: {reason}"),
            Self::TransportFailed { reason } => write!(f, "CDP transport failed: {reason}"),
            Self::CommandFailed { reason } => write!(f, "CDP command failed: {reason}"),
        }
    }
}

/// The debugging endpoint the engine exposes: a loopback port and the bearer path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevToolsEndpoint {
    /// The loopback port the engine's `DevTools` server listens on.
    pub port: u16,
    /// The path a WebSocket upgrade must carry, or `None` when this engine build does not
    /// publish one (older runtimes accept an unauthenticated loopback upgrade).
    pub path: Option<String>,
}

/// Reads the debugging endpoint the engine wrote into its user-data directory.
///
/// `DevToolsActivePort` holds the port on its first line and the bearer path (beginning
/// with `/`) on its second. A file that is missing, empty or malformed means the engine is
/// not exposing `DevTools`, which is reported rather than guessed at.
///
/// # Errors
///
/// [`CdpError::PortUnavailable`] naming what was wrong, so a caller can distinguish "the
/// engine was not started with debugging" from "the file has not appeared yet".
pub fn read_devtools_endpoint(user_data_dir: &Path) -> Result<DevToolsEndpoint, CdpError> {
    let file = user_data_dir.join(DEVTOOLS_PORT_FILE);
    let contents = std::fs::read_to_string(&file).map_err(|error| CdpError::PortUnavailable {
        reason: format!("cannot read {}: {error}", file.display()),
    })?;
    let mut lines = contents.lines();
    let port = lines
        .next()
        .map(str::trim)
        .and_then(|line| line.parse::<u16>().ok())
        .ok_or_else(|| CdpError::PortUnavailable {
            reason: format!("{} does not start with a port", file.display()),
        })?;
    let path = lines
        .next()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(ToOwned::to_owned);
    Ok(DevToolsEndpoint { port, path })
}

/// Sends one CDP command to the engine's `DevTools` endpoint and returns its answer.
///
/// The exchange is one WebSocket connection, one text frame out, one frame back: the
/// `DevTools` endpoint answers the command named in `command` (which must carry an `id`, as
/// the CDP protocol requires) and the connection is closed.
///
/// # Errors
///
/// [`CdpError::ConnectFailed`] when the endpoint refuses the upgrade,
/// [`CdpError::TransportFailed`] when it stops answering, and [`CdpError::CommandFailed`]
/// when the endpoint answered with an error for this command.
pub fn send_command(
    endpoint: &DevToolsEndpoint,
    command: &serde_json::Value,
) -> Result<serde_json::Value, CdpError> {
    let mut stream = TcpStream::connect(("127.0.0.1", endpoint.port)).map_err(|error| {
        CdpError::ConnectFailed {
            reason: format!("cannot connect to 127.0.0.1:{}: {error}", endpoint.port),
        }
    })?;
    stream
        .set_read_timeout(Some(EXCHANGE_TIMEOUT))
        .map_err(|error| CdpError::ConnectFailed {
            reason: error.to_string(),
        })?;
    stream
        .set_write_timeout(Some(EXCHANGE_TIMEOUT))
        .map_err(|error| CdpError::ConnectFailed {
            reason: error.to_string(),
        })?;
    ws_upgrade(&mut stream, endpoint)?;
    ws_send_text(
        &mut stream,
        &serde_json::to_string(command).map_err(|error| CdpError::TransportFailed {
            reason: format!("the command is not serialisable: {error}"),
        })?,
    )?;
    let answer = ws_read_text(&mut stream)?;
    let value: serde_json::Value =
        serde_json::from_str(&answer).map_err(|error| CdpError::TransportFailed {
            reason: format!("the answer is not JSON: {error}"),
        })?;
    if value.get("error").is_some_and(serde_json::Value::is_object) {
        return Err(CdpError::CommandFailed {
            reason: value
                .get("error")
                .map(serde_json::Value::to_string)
                .unwrap_or_default(),
        });
    }
    Ok(value)
}

/// Performs the WebSocket upgrade handshake against the `DevTools` endpoint.
fn ws_upgrade(stream: &mut TcpStream, endpoint: &DevToolsEndpoint) -> Result<(), CdpError> {
    // The key is fixed because this client makes one throwaway connection per command: the
    // server's acceptance is what is being checked, not a session that will be reused. A
    // per-connection random key would add entropy without changing what is proven.
    const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    let path = endpoint.path.as_deref().unwrap_or("/");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: \
         Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        endpoint.port
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| CdpError::ConnectFailed {
            reason: format!("cannot send the upgrade: {error}"),
        })?;
    let head = read_http_head(stream)?;
    let ok = head.starts_with("HTTP/1.1 101") || head.starts_with("HTTP/1.0 101");
    if !ok {
        return Err(CdpError::ConnectFailed {
            reason: format!("the endpoint refused the upgrade: {}", first_line(&head)),
        });
    }
    Ok(())
}

/// Sends one text frame. A client-to-server frame is always masked.
fn ws_send_text(stream: &mut TcpStream, text: &str) -> Result<(), CdpError> {
    let payload = text.as_bytes();
    if payload.len() > MAX_FRAME_BYTES {
        return Err(CdpError::TransportFailed {
            reason: format!(
                "the command is {} bytes, over the {} byte frame bound",
                payload.len(),
                MAX_FRAME_BYTES
            ),
        });
    }
    let mut frame = vec![0x81u8];
    let mask: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
    match payload.len() {
        0..=125 => frame.push(payload.len() as u8 | 0x80),
        126..=65_535 => {
            frame.push(0x7E | 0x80);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(127 | 0x80);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    // With an all-zero mask the payload is its own masked form, which keeps this a client
    // frame (masked, as the protocol requires) without a second buffer.
    frame.extend_from_slice(payload);
    stream
        .write_all(&frame)
        .map_err(|error| CdpError::TransportFailed {
            reason: format!("cannot send the frame: {error}"),
        })?;
    stream.flush().map_err(|error| CdpError::TransportFailed {
        reason: format!("cannot flush the frame: {error}"),
    })
}

/// Reads one text frame, reassembling continuation frames.
fn ws_read_text(stream: &mut TcpStream) -> Result<String, CdpError> {
    let mut payload: Vec<u8> = Vec::new();
    loop {
        let mut head = [0u8; 2];
        read_exact(stream, &mut head)?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0F;
        let masked = head[1] & 0x80 != 0;
        let length = match head[1] & 0x7F {
            126 => {
                let mut extended = [0u8; 2];
                read_exact(stream, &mut extended)?;
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                read_exact(stream, &mut extended)?;
                u64::from_be_bytes(extended) as usize
            }
            plain => plain as usize,
        };
        if length > MAX_FRAME_BYTES {
            return Err(CdpError::TransportFailed {
                reason: format!("a frame of {length} bytes exceeds the read bound"),
            });
        }
        let mask = if masked {
            let mut key = [0u8; 4];
            read_exact(stream, &mut key)?;
            Some(key)
        } else {
            None
        };
        let mut chunk = vec![0u8; length];
        read_exact(stream, &mut chunk)?;
        if let Some(key) = mask {
            for (index, byte) in chunk.iter_mut().enumerate() {
                *byte ^= key[index % 4];
            }
        }
        match opcode {
            0x1 | 0x0 => payload.extend_from_slice(&chunk),
            0x8 => {
                return Err(CdpError::TransportFailed {
                    reason: "the endpoint closed the connection".to_owned(),
                });
            }
            0x9 | 0xA => {
                // A ping or pong between the handshake and the answer is answered or
                // skipped, never treated as the answer.
                if opcode == 0x9 {
                    let mut pong = vec![0x8Au8, 0x80];
                    pong.extend_from_slice(&[0, 0, 0, 0]);
                    pong.extend_from_slice(&chunk);
                    stream
                        .write_all(&pong)
                        .map_err(|error| CdpError::TransportFailed {
                            reason: format!("cannot answer a ping: {error}"),
                        })?;
                }
                continue;
            }
            other => {
                return Err(CdpError::TransportFailed {
                    reason: format!("unexpected websocket opcode {other}"),
                });
            }
        }
        if fin {
            return String::from_utf8(payload).map_err(|error| CdpError::TransportFailed {
                reason: format!("the answer is not valid UTF-8: {error}"),
            });
        }
    }
}

/// Reads the HTTP head of the upgrade response.
fn read_http_head(stream: &mut TcpStream) -> Result<String, CdpError> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        let read = stream
            .read(&mut byte)
            .map_err(|error| CdpError::ConnectFailed {
                reason: format!("cannot read the upgrade response: {error}"),
            })?;
        if read == 0 {
            return Err(CdpError::ConnectFailed {
                reason: "the endpoint closed during the upgrade".to_owned(),
            });
        }
        head.push(byte[0]);
        if head.len() > MAX_HTTP_HEAD_BYTES {
            return Err(CdpError::ConnectFailed {
                reason: "the upgrade response head is too long".to_owned(),
            });
        }
        if head.ends_with(b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&head).into_owned());
        }
    }
}

/// Reads exactly `buffer.len()` bytes, reporting a short read as a transport failure.
fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<(), CdpError> {
    stream
        .read_exact(buffer)
        .map_err(|error| CdpError::TransportFailed {
            reason: format!("the endpoint stopped answering: {error}"),
        })
}

/// The first line of an HTTP head, for a refusal that names what came back.
fn first_line(head: &str) -> &str {
    head.lines().next().unwrap_or("(empty response)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing_reads_port_and_bearer_path() {
        let root = std::env::temp_dir().join(format!("ncui-cdp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create user data dir");
        std::fs::write(
            root.join(DEVTOOLS_PORT_FILE),
            "9222\n/devtools/browser/guid\n",
        )
        .expect("write port file");
        let endpoint = read_devtools_endpoint(&root).expect("an endpoint");
        assert_eq!(endpoint.port, 9222);
        assert_eq!(endpoint.path.as_deref(), Some("/devtools/browser/guid"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_without_a_bearer_path_is_accepted_with_none() {
        let root = std::env::temp_dir().join(format!("ncui-cdp-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create user data dir");
        std::fs::write(root.join(DEVTOOLS_PORT_FILE), "9223\n").expect("write port file");
        let endpoint = read_devtools_endpoint(&root).expect("an endpoint");
        assert_eq!(endpoint.port, 9223);
        assert_eq!(endpoint.path, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_or_malformed_port_file_is_reported_not_guessed() {
        let root = std::env::temp_dir().join(format!("ncui-cdp-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create user data dir");
        let error = read_devtools_endpoint(&root).expect_err("no file");
        assert_eq!(error.code(), "cdp_port_unavailable");
        std::fs::write(root.join(DEVTOOLS_PORT_FILE), "not-a-port\n").expect("write junk");
        let error = read_devtools_endpoint(&root).expect_err("junk file");
        assert_eq!(error.code(), "cdp_port_unavailable");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refused_upgrade_is_reported_under_its_own_code() {
        // A listener that closes without upgrading stands in for an engine that is up but
        // not speaking DevTools, which is the refusal a caller must be able to name.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("address").port();
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let error = send_command(
            &DevToolsEndpoint {
                port,
                path: Some("/devtools/browser/guid".to_owned()),
            },
            &serde_json::json!({ "id": 1, "method": "Browser.getVersion" }),
        )
        .expect_err("a 403 is not an upgrade");
        assert_eq!(error.code(), "cdp_connect_failed");
    }
}
