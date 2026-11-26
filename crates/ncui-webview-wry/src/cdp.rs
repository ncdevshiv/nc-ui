//! The host-side CDP transport (ADR-017 Phase 2).
//!
//! `WebView2` publishes a process-wide browser endpoint in `DevToolsActivePort`. That endpoint is
//! not a window identity: the host snapshots its page targets around serialized window creation,
//! binds each `EngineWindow` to exactly one new target, and attaches a flattened session for every
//! page command. Responses are correlated by CDP id; events and other requests' answers never
//! satisfy a command.

#[cfg(any(test, all(feature = "wry-engine", target_os = "windows")))]
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

pub use ncui_webview::CdpError;

/// The longest a single WebSocket frame's payload this client will read.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// The longest an HTTP head on the WebSocket upgrade may be.
const MAX_HTTP_HEAD_BYTES: usize = 16 * 1024;
/// How long one CDP exchange may take end to end.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// The file `WebView2` writes its debugging port and bearer path into.
pub const DEVTOOLS_PORT_FILE: &str = "DevToolsActivePort";

#[cfg(not(target_os = "windows"))]
pub(crate) fn platform_unsupported() -> CdpError {
    #[cfg(target_os = "linux")]
    let engine = "WebKitGTK";
    #[cfg(not(target_os = "linux"))]
    let engine = "this platform's embedded web engine";
    CdpError::UnsupportedPlatform {
        reason: format!("{engine} does not expose a Chrome DevTools Protocol endpoint"),
    }
}

/// The debugging endpoint: a loopback port and optional bearer path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevToolsEndpoint {
    /// The loopback port the engine's `DevTools` server listens on.
    pub port: u16,
    /// The path a WebSocket upgrade must carry, when the runtime publishes one.
    pub path: Option<String>,
}

/// Reads the endpoint `WebView2` wrote into its user-data directory.
///
/// # Errors
///
/// Returns [`CdpError::PortUnavailable`] when the file is absent, unreadable, or malformed.
pub fn read_devtools_endpoint(user_data_dir: &Path) -> Result<DevToolsEndpoint, CdpError> {
    let candidates = [
        user_data_dir.join("EBWebView").join(DEVTOOLS_PORT_FILE),
        user_data_dir.join(DEVTOOLS_PORT_FILE),
    ];
    let file = candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| CdpError::PortUnavailable {
            reason: format!(
                "no {} in {} (or its EBWebView profile directory)",
                DEVTOOLS_PORT_FILE,
                user_data_dir.display()
            ),
        })?;
    let contents = std::fs::read_to_string(file).map_err(|error| CdpError::PortUnavailable {
        reason: format!("cannot read {}: {error}", file.display()),
    })?;
    let mut lines = contents.lines();
    let port = lines
        .next()
        .map(str::trim)
        .and_then(|line| line.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| CdpError::PortUnavailable {
            reason: format!("{} does not start with a nonzero port", file.display()),
        })?;
    let path = lines
        .next()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(ToOwned::to_owned);
    Ok(DevToolsEndpoint { port, path })
}

/// Sends one command directly to the browser endpoint and returns its matching response.
///
/// Page commands must use [`send_page_command`] instead. This direct path exists for browser-domain
/// discovery such as `Browser.getVersion` and target snapshots.
///
/// # Errors
///
/// Invalid ids are rejected before connecting. Events and responses with other ids are ignored;
/// only a response with the command's positive integer id can succeed or report a command error.
pub fn send_command(
    endpoint: &DevToolsEndpoint,
    command: &serde_json::Value,
) -> Result<serde_json::Value, CdpError> {
    command_id(command)?;
    let mut stream = connect(endpoint)?;
    exchange_on_stream(&mut stream, command, Instant::now() + EXCHANGE_TIMEOUT)
}

fn connect(endpoint: &DevToolsEndpoint) -> Result<TcpStream, CdpError> {
    let deadline = Instant::now() + EXCHANGE_TIMEOUT;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream =
        TcpStream::connect_timeout(&address, remaining(deadline)?).map_err(|error| {
            CdpError::ConnectFailed {
                reason: format!("cannot connect to {address}: {error}"),
            }
        })?;
    refresh_timeouts(&mut stream, deadline)?;
    ws_upgrade(&mut stream, endpoint, deadline)?;
    Ok(stream)
}

fn exchange_on_stream<S: Socket>(
    stream: &mut S,
    command: &serde_json::Value,
    deadline: Instant,
) -> Result<serde_json::Value, CdpError> {
    let id = command_id(command)?;
    let text = serde_json::to_string(command).map_err(|error| CdpError::TransportFailed {
        reason: format!("the command is not serialisable: {error}"),
    })?;
    ws_send_frame(stream, 0x1, text.as_bytes(), deadline)?;

    loop {
        let text = ws_read_text(stream, deadline)?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| CdpError::TransportFailed {
                reason: format!("the answer is not JSON: {error}"),
            })?;
        let Some(object) = value.as_object() else {
            return Err(CdpError::TransportFailed {
                reason: "the endpoint sent a non-object CDP message".to_owned(),
            });
        };
        let response_id = object.get("id").and_then(serde_json::Value::as_u64);
        if response_id != Some(id) {
            // CDP events have a method and no id. A response for a different id is not this
            // command's answer, even when it carries an error that looks useful.
            if response_id.is_some()
                || object
                    .get("method")
                    .is_some_and(serde_json::Value::is_string)
            {
                continue;
            }
            return Err(CdpError::TransportFailed {
                reason: "the endpoint sent neither a CDP event nor an identified response"
                    .to_owned(),
            });
        }
        if let Some(error) = object.get("error").filter(|error| error.is_object()) {
            return Err(CdpError::CommandFailed {
                reason: error.to_string(),
            });
        }
        if object
            .get("result")
            .is_some_and(serde_json::Value::is_object)
        {
            return Ok(value);
        }
        return Err(CdpError::TransportFailed {
            reason: format!("response {id} has neither an object result nor an object error"),
        });
    }
}

pub(crate) fn command_id(command: &serde_json::Value) -> Result<u64, CdpError> {
    command
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| CdpError::InvalidCommand {
            reason: "`id` must be a non-boolean positive integer".to_owned(),
        })
}

/// Attaches this command to one bound page target, then always detaches the flattened session.
///
/// `WebView2` scopes a flattened session to the WebSocket connection that attached it, so attach,
/// the page command, and detach all use one upgraded connection. A caller-provided `sessionId` is
/// replaced: allowing it through would let a page command escape the target selected by the host.
///
/// # Errors
///
/// Returns attach, command, or detach failures as typed errors. A page-command failure takes
/// precedence if detach also fails; detach failure is returned after a successful page command.
pub fn send_page_command(
    endpoint: &DevToolsEndpoint,
    target_id: &str,
    command: &serde_json::Value,
) -> Result<serde_json::Value, CdpError> {
    if target_id.is_empty() {
        return Err(CdpError::TargetBindFailed {
            reason: "the window has an empty page target id".to_owned(),
        });
    }
    // Validate before attachment, which is a side effect on the engine.
    command_id(command)?;
    let mut stream = connect(endpoint)?;
    let attached = exchange_on_stream(
        &mut stream,
        &serde_json::json!({
            "id": 1,
            "method": "Target.attachToTarget",
            "params": { "targetId": target_id, "flatten": true },
        }),
        Instant::now() + EXCHANGE_TIMEOUT,
    )?;
    let session_id = attached
        .get("result")
        .and_then(|result| result.get("sessionId"))
        .and_then(serde_json::Value::as_str)
        .filter(|session| !session.is_empty())
        .ok_or_else(|| CdpError::TransportFailed {
            reason: "Target.attachToTarget returned no non-empty sessionId".to_owned(),
        })?
        .to_owned();

    let mut scoped = command.clone();
    let object = scoped
        .as_object_mut()
        .ok_or_else(|| CdpError::InvalidCommand {
            reason: "the CDP command must be an object".to_owned(),
        })?;
    object.insert(
        "sessionId".to_owned(),
        serde_json::Value::String(session_id.clone()),
    );
    let answer = exchange_on_stream(&mut stream, &scoped, Instant::now() + EXCHANGE_TIMEOUT);
    let detached = exchange_on_stream(
        &mut stream,
        &serde_json::json!({
            "id": 1,
            "method": "Target.detachFromTarget",
            "params": { "sessionId": session_id },
        }),
        Instant::now() + EXCHANGE_TIMEOUT,
    );
    match (answer, detached) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(answer), Ok(_)) => Ok(answer),
    }
}

/// Extracts page target identities from a `Target.getTargets` response.
#[cfg(any(test, all(feature = "wry-engine", target_os = "windows")))]
pub(crate) fn page_target_ids(response: &serde_json::Value) -> Result<BTreeSet<String>, CdpError> {
    let infos = response
        .get("result")
        .and_then(|result| result.get("targetInfos"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| CdpError::TransportFailed {
            reason: "Target.getTargets returned no targetInfos array".to_owned(),
        })?;
    let mut ids = BTreeSet::new();
    for info in infos {
        let object = info.as_object().ok_or_else(|| CdpError::TransportFailed {
            reason: "Target.getTargets returned a non-object target".to_owned(),
        })?;
        if object.get("type").and_then(serde_json::Value::as_str) != Some("page") {
            continue;
        }
        let id = object
            .get("targetId")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| CdpError::TransportFailed {
                reason: "Target.getTargets returned a page without a targetId".to_owned(),
            })?;
        ids.insert(id.to_owned());
    }
    Ok(ids)
}

/// Selects the exactly-one page target that appeared between two serialized snapshots.
#[cfg(any(test, all(feature = "wry-engine", target_os = "windows")))]
pub(crate) fn bind_new_page_target(
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> Result<String, CdpError> {
    let added: Vec<_> = after.difference(before).collect();
    match added.as_slice() {
        [target_id] => Ok((*target_id).clone()),
        [] => Err(CdpError::TargetBindFailed {
            reason: "no new page target appeared during window creation".to_owned(),
        }),
        ids => Err(CdpError::TargetBindFailed {
            reason: format!(
                "{} new page targets appeared during window creation; refusing to guess: {}",
                ids.len(),
                ids.iter()
                    .map(|id| format!("`{id}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

trait Socket: Read + Write {
    fn refresh_timeouts(&mut self, deadline: Instant) -> std::io::Result<()>;
}

impl Socket for TcpStream {
    fn refresh_timeouts(&mut self, deadline: Instant) -> std::io::Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.set_read_timeout(Some(remaining))?;
        self.set_write_timeout(Some(remaining))?;
        Ok(())
    }
}

fn remaining(deadline: Instant) -> Result<Duration, CdpError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(CdpError::TransportFailed {
            reason: "the CDP exchange exceeded its time bound".to_owned(),
        });
    }
    Ok(remaining)
}

fn refresh_timeouts<S: Socket>(stream: &mut S, deadline: Instant) -> Result<(), CdpError> {
    stream
        .refresh_timeouts(deadline)
        .map_err(|error| CdpError::TransportFailed {
            reason: format!("cannot bound the CDP exchange: {error}"),
        })
}

/// Performs the WebSocket upgrade handshake.
fn ws_upgrade<S: Socket>(
    stream: &mut S,
    endpoint: &DevToolsEndpoint,
    deadline: Instant,
) -> Result<(), CdpError> {
    const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    let path = endpoint.path.as_deref().unwrap_or("/");
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: \
         Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        endpoint.port
    );
    write_all(stream, request.as_bytes(), deadline)?;
    stream.flush().map_err(|error| CdpError::ConnectFailed {
        reason: format!("cannot flush the upgrade: {error}"),
    })?;
    let head = read_http_head(stream, deadline)?;
    if !(head.starts_with("HTTP/1.1 101") || head.starts_with("HTTP/1.0 101")) {
        return Err(CdpError::ConnectFailed {
            reason: format!("the endpoint refused the upgrade: {}", first_line(&head)),
        });
    }
    Ok(())
}

/// Sends one complete, masked frame, enforcing both frame and payload bounds.
fn ws_send_frame<S: Socket>(
    stream: &mut S,
    opcode: u8,
    payload: &[u8],
    deadline: Instant,
) -> Result<(), CdpError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(CdpError::TransportFailed {
            reason: format!(
                "a {} byte payload exceeds the {} byte frame bound",
                payload.len(),
                MAX_FRAME_BYTES
            ),
        });
    }
    let mut frame = vec![0x80 | opcode];
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
    // RFC 6455 requires client masking. This throwaway loopback connection has no reusable
    // secret; a fixed mask still satisfies the wire requirement without inventing a session key.
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(payload);
    write_all(stream, &frame, deadline)?;
    stream.flush().map_err(|error| CdpError::TransportFailed {
        reason: format!("cannot flush a frame: {error}"),
    })
}

fn write_all<S: Socket>(stream: &mut S, bytes: &[u8], deadline: Instant) -> Result<(), CdpError> {
    refresh_timeouts(stream, deadline)?;
    stream
        .write_all(bytes)
        .map_err(|error| CdpError::TransportFailed {
            reason: format!("cannot send websocket data: {error}"),
        })
}

struct WebSocketFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Reads one complete text message, skipping control frames and enforcing fragmentation rules.
fn ws_read_text<S: Socket>(stream: &mut S, deadline: Instant) -> Result<String, CdpError> {
    let mut fragmented: Option<Vec<u8>> = None;
    loop {
        let WebSocketFrame {
            fin,
            opcode,
            payload: chunk,
        } = ws_read_frame(stream, deadline)?;
        match opcode {
            0x8 => {
                // Acknowledge close before reporting that no matching answer arrived.
                let _ = ws_send_frame(stream, 0x8, &chunk, deadline);
                return Err(transport("the endpoint closed the connection"));
            }
            0x9 => ws_send_frame(stream, 0xA, &chunk, deadline)?,
            0xA => {}
            0x2 => return Err(transport("the endpoint sent a binary CDP message")),
            0x1 => {
                if fragmented.is_some() {
                    return Err(transport("a new data frame interrupted fragmented text"));
                }
                if fin {
                    return utf8_text(chunk);
                }
                fragmented = Some(chunk);
            }
            0x0 => {
                let mut message = fragmented
                    .take()
                    .ok_or_else(|| transport("a continuation frame had no fragmented message"))?;
                let total = message
                    .len()
                    .checked_add(chunk.len())
                    .ok_or_else(|| transport("the fragmented message size overflowed"))?;
                if total > MAX_FRAME_BYTES {
                    return Err(transport(format!(
                        "a fragmented message of {total} bytes exceeds the {MAX_FRAME_BYTES} byte bound"
                    )));
                }
                message.extend_from_slice(&chunk);
                if fin {
                    return utf8_text(message);
                }
                fragmented = Some(message);
            }
            _ => unreachable!("frame opcodes are checked before they are returned"),
        }
    }
}

/// Reads and validates one unmasked server frame.
fn ws_read_frame<S: Socket>(stream: &mut S, deadline: Instant) -> Result<WebSocketFrame, CdpError> {
    let mut head = [0u8; 2];
    read_exact(stream, &mut head, deadline)?;
    let fin = head[0] & 0x80 != 0;
    let rsv = head[0] & 0x70;
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    if rsv != 0 {
        return Err(transport(
            "the endpoint set a reserved WebSocket extension bit",
        ));
    }
    if masked {
        return Err(transport("the endpoint sent a masked server frame"));
    }
    let length = frame_length(stream, head[1] & 0x7F, deadline)?;
    if length > MAX_FRAME_BYTES {
        return Err(transport(format!(
            "a frame of {length} bytes exceeds the {MAX_FRAME_BYTES} byte bound"
        )));
    }
    let control = matches!(opcode, 0x8..=0xA);
    if control {
        if !fin || length > 125 {
            return Err(transport("an invalid control frame was received"));
        }
    } else if !matches!(opcode, 0x0..=0x2) {
        return Err(transport(format!("unexpected WebSocket opcode {opcode}")));
    }
    let mut payload = vec![0u8; length];
    read_exact(stream, &mut payload, deadline)?;
    Ok(WebSocketFrame {
        fin,
        opcode,
        payload,
    })
}

/// Decodes a server frame's extended length, rejecting non-minimal and impossible encodings.
fn frame_length<S: Socket>(
    stream: &mut S,
    marker: u8,
    deadline: Instant,
) -> Result<usize, CdpError> {
    match marker {
        126 => {
            let mut extended = [0u8; 2];
            read_exact(stream, &mut extended, deadline)?;
            let length = u16::from_be_bytes(extended) as usize;
            if length < 126 {
                return Err(transport("the endpoint used a non-minimal frame length"));
            }
            Ok(length)
        }
        127 => {
            let mut extended = [0u8; 8];
            read_exact(stream, &mut extended, deadline)?;
            let length = u64::from_be_bytes(extended);
            if length >> 63 != 0 {
                return Err(transport(
                    "the endpoint set the reserved 64-bit frame length bit",
                ));
            }
            let length = usize::try_from(length)
                .map_err(|_| transport("the frame length does not fit this platform"))?;
            if u16::try_from(length).is_ok() {
                return Err(transport(
                    "the endpoint used a non-minimal 64-bit frame length",
                ));
            }
            Ok(length)
        }
        plain => Ok(plain as usize),
    }
}

fn utf8_text(payload: Vec<u8>) -> Result<String, CdpError> {
    String::from_utf8(payload).map_err(|error| CdpError::TransportFailed {
        reason: format!("the answer is not valid UTF-8: {error}"),
    })
}

fn transport(reason: impl Into<String>) -> CdpError {
    CdpError::TransportFailed {
        reason: reason.into(),
    }
}

/// Reads an HTTP upgrade head without allowing an unbounded allocation.
fn read_http_head<S: Socket>(stream: &mut S, deadline: Instant) -> Result<String, CdpError> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        let mut read = 0;
        while read < byte.len() {
            refresh_timeouts(stream, deadline)?;
            match stream.read(&mut byte[read..]) {
                Ok(0) => {
                    return Err(CdpError::ConnectFailed {
                        reason: "the endpoint closed during the upgrade".to_owned(),
                    });
                }
                Ok(count) => read += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(CdpError::ConnectFailed {
                        reason: format!("cannot read the upgrade response: {error}"),
                    });
                }
            }
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

/// Reads exactly `buffer.len()` bytes under one end-to-end deadline.
fn read_exact<S: Socket>(
    stream: &mut S,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), CdpError> {
    let mut read = 0;
    while read < buffer.len() {
        refresh_timeouts(stream, deadline)?;
        match stream.read(&mut buffer[read..]) {
            Ok(0) => return Err(transport("the endpoint stopped answering")),
            Ok(count) => read += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(transport(format!("cannot read the endpoint: {error}"))),
        }
    }
    Ok(())
}

fn first_line(head: &str) -> &str {
    head.lines().next().unwrap_or("(empty response)")
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
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn endpoint(script: impl FnOnce(TcpStream) + Send + 'static) -> DevToolsEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let port = listener.local_addr().expect("address").port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            script(stream);
        });
        DevToolsEndpoint {
            port,
            path: Some("/devtools/browser/test".to_owned()),
        }
    }

    fn upgrade(stream: &mut TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            request.push_str(&line);
            if request.ends_with("\r\n\r\n") {
                break;
            }
        }
        assert!(request.starts_with("GET /devtools/browser/test "));
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .expect("upgrade");
        let mut head = [0u8; 2];
        reader.read_exact(&mut head).expect("command frame");
        let length = match head[1] & 0x7F {
            126 => {
                let mut extended = [0u8; 2];
                reader.read_exact(&mut extended).expect("extended length");
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                reader.read_exact(&mut extended).expect("extended length");
                u64::from_be_bytes(extended) as usize
            }
            plain => plain as usize,
        };
        let mut payload = vec![0u8; length + 4];
        reader.read_exact(&mut payload).expect("command payload");
    }

    fn frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![if fin { 0x80 } else { 0 } | opcode];
        assert!(payload.len() <= 125);
        bytes.push(payload.len() as u8);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn text(value: &Value) -> Vec<u8> {
        frame(0x1, true, value.to_string().as_bytes())
    }

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
    fn a_missing_malformed_or_zero_port_file_is_reported_not_guessed() {
        let root = std::env::temp_dir().join(format!("ncui-cdp-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create user data dir");
        assert_eq!(
            read_devtools_endpoint(&root).expect_err("no file").code(),
            "cdp_port_unavailable"
        );
        for contents in ["not-a-port\n", "0\n", "-1\n"] {
            std::fs::write(root.join(DEVTOOLS_PORT_FILE), contents).expect("write junk");
            assert_eq!(
                read_devtools_endpoint(&root).expect_err("junk file").code(),
                "cdp_port_unavailable"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refused_upgrade_is_reported_under_its_own_code() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("address").port();
        thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let error = send_command(
            &DevToolsEndpoint {
                port,
                path: Some("/devtools/browser/guid".to_owned()),
            },
            &json!({ "id": 1, "method": "Browser.getVersion" }),
        )
        .expect_err("a 403 is not an upgrade");
        assert_eq!(error.code(), "cdp_connect_failed");
    }

    #[test]
    fn an_event_before_the_matching_response_is_ignored() {
        let endpoint = endpoint(|mut stream| {
            upgrade(&mut stream);
            let event = json!({ "method": "Page.loadEventFired", "params": { "timestamp": 1 } });
            let answer = json!({ "id": 7, "result": { "value": "ready" } });
            stream.write_all(&text(&event)).expect("event");
            stream.write_all(&text(&answer)).expect("answer");
        });
        let answer = send_command(
            &endpoint,
            &json!({ "id": 7, "method": "Runtime.evaluate", "params": {} }),
        )
        .expect("matching response");
        assert_eq!(answer["result"]["value"], "ready");
    }

    #[test]
    fn mismatched_response_ids_never_satisfy_the_command() {
        let endpoint = endpoint(|mut stream| {
            upgrade(&mut stream);
            stream
                .write_all(&text(&json!({ "id": 8, "error": { "message": "wrong" } })))
                .expect("wrong id");
            stream
                .write_all(&text(&json!({ "id": 7, "result": { "value": "right" } })))
                .expect("right id");
        });
        let answer = send_command(&endpoint, &json!({ "id": 7, "method": "Page.reload" }))
            .expect("matching id, not the first frame");
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"]["value"], "right");
    }

    #[test]
    fn a_mismatched_response_followed_by_close_is_not_an_answer() {
        let endpoint = endpoint(|mut stream| {
            upgrade(&mut stream);
            stream
                .write_all(&text(&json!({ "id": 99, "result": { "value": "wrong" } })))
                .expect("wrong id");
            stream.write_all(&frame(0x8, true, &[])).expect("close");
        });
        let error = send_command(&endpoint, &json!({ "id": 7, "method": "Page.reload" }))
            .expect_err("close cannot answer another id");
        assert_eq!(error.code(), "cdp_transport_failed");
    }

    #[test]
    fn ping_pong_and_fragmentation_preserve_the_matching_response() {
        let endpoint = endpoint(|mut stream| {
            upgrade(&mut stream);
            let answer = json!({ "id": 7, "result": { "value": "fragmented" } }).to_string();
            let bytes = answer.as_bytes();
            stream
                .write_all(&frame(0x1, false, &bytes[..10]))
                .expect("first fragment");
            stream.write_all(&frame(0x9, true, b"alive")).expect("ping");
            let mut pong = [0u8; 11];
            stream.read_exact(&mut pong).expect("pong acknowledgement");
            assert_eq!(&pong[..2], &[0x8A, 0x85]);
            let payload: Vec<_> = pong[6..]
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ pong[2 + index % 4])
                .collect();
            assert_eq!(payload, b"alive");
            stream
                .write_all(&frame(0xA, true, b"ignored"))
                .expect("pong");
            stream
                .write_all(&frame(0x0, true, &bytes[10..]))
                .expect("continuation");
        });
        let answer = send_command(&endpoint, &json!({ "id": 7, "method": "Runtime.evaluate" }))
            .expect("fragmented matching response");
        assert_eq!(answer["result"]["value"], "fragmented");
    }

    #[test]
    fn invalid_command_ids_are_rejected_before_connecting() {
        let endpoint = DevToolsEndpoint {
            port: 1,
            path: None,
        };
        for command in [
            json!({ "method": "Page.reload" }),
            json!({ "id": true, "method": "Page.reload" }),
            json!({ "id": 0, "method": "Page.reload" }),
            json!({ "id": -1, "method": "Page.reload" }),
            json!({ "id": 1.5, "method": "Page.reload" }),
            json!({ "id": "1", "method": "Page.reload" }),
        ] {
            let error = send_command(&endpoint, &command).expect_err("invalid id");
            assert_eq!(error.code(), "cdp_invalid_command", "for {command}");
        }
    }

    #[test]
    fn an_invalid_page_command_does_not_attach() {
        let error = send_page_command(
            &DevToolsEndpoint {
                port: 1,
                path: None,
            },
            "target-a",
            &json!({ "id": false }),
        )
        .expect_err("invalid before attach");
        assert_eq!(error.code(), "cdp_invalid_command");
    }

    fn server_upgrade(stream: &mut TcpStream) -> BufReader<TcpStream> {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            request.push_str(&line);
            if request.ends_with("\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
            .expect("upgrade");
        reader
    }

    fn read_client_command(reader: &mut BufReader<TcpStream>) -> Value {
        let mut head = [0u8; 2];
        reader.read_exact(&mut head).expect("frame head");
        assert_eq!(head[0], 0x81);
        let length = match head[1] & 0x7F {
            126 => {
                let mut extended = [0u8; 2];
                reader.read_exact(&mut extended).expect("length");
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                reader.read_exact(&mut extended).expect("length");
                u64::from_be_bytes(extended) as usize
            }
            plain => plain as usize,
        };
        let mut payload = vec![0u8; length + 4];
        reader.read_exact(&mut payload).expect("payload");
        serde_json::from_slice(&payload[4..]).expect("command JSON")
    }

    fn scripted_page_exchange(page_answer: Value) -> (DevToolsEndpoint, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted sessions");
        let port = listener.local_addr().expect("address").port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&commands);
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept exchange");
            let mut reader = server_upgrade(&mut stream);
            for index in 0..3 {
                let command = read_client_command(&mut reader);
                recorder.lock().expect("recorder").push(command);
                let answer = match index {
                    0 => json!({ "id": 1, "result": { "sessionId": "session-a" } }),
                    1 => page_answer.clone(),
                    _ => json!({ "id": 1, "result": {} }),
                };
                stream.write_all(&text(&answer)).expect("scripted answer");
            }
        });
        (
            DevToolsEndpoint {
                port,
                path: Some("/devtools/browser/test".to_owned()),
            },
            commands,
        )
    }

    #[test]
    fn page_commands_propagate_session_and_detach_after_success() {
        let (endpoint, commands) =
            scripted_page_exchange(json!({ "id": 9, "result": { "value": 42 } }));
        let answer = send_page_command(
            &endpoint,
            "target-a",
            &json!({
                "id": 9,
                "method": "Runtime.evaluate",
                "sessionId": "caller-supplied",
                "params": { "expression": "6*7" },
            }),
        )
        .expect("page exchange");
        assert_eq!(answer["result"]["value"], 42);
        let commands = commands.lock().expect("recorder").clone();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0]["params"]["targetId"], "target-a");
        assert_eq!(commands[0]["params"]["flatten"], true);
        assert_eq!(commands[1]["sessionId"], "session-a");
        assert_eq!(commands[2]["method"], "Target.detachFromTarget");
        assert_eq!(commands[2]["params"]["sessionId"], "session-a");
    }

    #[test]
    fn page_commands_detach_even_after_command_failure() {
        let (endpoint, commands) =
            scripted_page_exchange(json!({ "id": 9, "error": { "message": "page failed" } }));
        let error = send_page_command(
            &endpoint,
            "target-a",
            &json!({ "id": 9, "method": "Runtime.evaluate" }),
        )
        .expect_err("page command failed");
        assert_eq!(error.code(), "cdp_command_failed");
        let commands = commands.lock().expect("recorder").clone();
        assert_eq!(commands.len(), 3, "detach must still be attempted");
        assert_eq!(commands[2]["method"], "Target.detachFromTarget");
    }

    #[test]
    fn detach_failure_is_reported_after_a_successful_page_command() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind detach failure");
        let port = listener.local_addr().expect("address").port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = server_upgrade(&mut stream);
            for index in 0..3 {
                let _ = read_client_command(&mut reader);
                let answer = match index {
                    0 => json!({ "id": 1, "result": { "sessionId": "session-a" } }),
                    1 => json!({ "id": 9, "result": { "value": 42 } }),
                    _ => json!({ "id": 1, "error": { "message": "detach failed" } }),
                };
                stream.write_all(&text(&answer)).expect("answer");
            }
        });
        let error = send_page_command(
            &DevToolsEndpoint {
                port,
                path: Some("/devtools/browser/test".to_owned()),
            },
            "target-a",
            &json!({ "id": 9, "method": "Runtime.evaluate" }),
        )
        .expect_err("detach failed");
        assert_eq!(error.code(), "cdp_command_failed");
        assert!(error.to_string().contains("detach failed"));
    }

    #[test]
    fn target_snapshot_binding_fails_closed_on_zero_or_multiple_new_pages() {
        let before = BTreeSet::from(["existing".to_owned()]);
        let zero = bind_new_page_target(&before, &before).expect_err("zero is ambiguous");
        assert_eq!(zero.code(), "cdp_target_bind_failed");
        assert!(zero.to_string().contains("no new page target"));

        let multiple = BTreeSet::from([
            "existing".to_owned(),
            "new-a".to_owned(),
            "new-b".to_owned(),
        ]);
        let error = bind_new_page_target(&before, &multiple).expect_err("multiple is ambiguous");
        assert_eq!(error.code(), "cdp_target_bind_failed");
        assert!(error.to_string().contains("new-a"));
        assert!(error.to_string().contains("new-b"));

        let one = BTreeSet::from(["existing".to_owned(), "new-a".to_owned()]);
        assert_eq!(
            bind_new_page_target(&before, &one).expect("exactly one"),
            "new-a"
        );
    }

    #[test]
    fn target_snapshots_ignore_non_page_targets() {
        let response = json!({ "result": { "targetInfos": [
            { "type": "page", "targetId": "page-a" },
            { "type": "service_worker", "targetId": "worker-a" },
            { "type": "browser", "targetId": "browser-a" },
        ] } });
        assert_eq!(
            page_target_ids(&response).expect("page snapshot"),
            BTreeSet::from(["page-a".to_owned()])
        );
    }
}
