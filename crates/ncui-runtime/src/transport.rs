//! Serving the agent API over a loopback socket.
//!
//! Everything else in this crate works in-process: `ncui describe` returns a document to
//! whoever called it in the same process. That is enough for tests and for the CLI, and it
//! is not enough for an agent, which is a separate process by definition. This module binds
//! the same JSON-RPC surface to a loopback socket so an external agent can reach it.
//!
//! # Threat model
//!
//! Loopback is **not** trusted (spec 27, ADR-007). A local socket is reachable by every
//! process running as the same user, which includes anything a browser extension, a build
//! script, or a compromised dependency might have spawned. So the transport:
//!
//! * binds `127.0.0.1` only — never a routable interface;
//! * requires a bearer token before a single request is read;
//! * compares that token in constant time;
//! * applies the same rate limit, capability check and consent gate as the in-process path,
//!   because it dispatches through the same pipeline rather than beside it;
//! * validates the declared frame length **before** allocating, so a hostile peer cannot
//!   induce a large allocation by announcing one;
//! * records an audit event for every request, including every refusal;
//! * bounds the number of concurrent connections.
//!
//! # Framing
//!
//! A frame is a 4-byte big-endian length followed by that many bytes of UTF-8 JSON. This is
//! deliberately not the binary framing in `ncui-ipc`: that protocol carries its own magic,
//! kind and version header because it is the internal wire format. The agent protocol is
//! versioned at the JSON-RPC layer, and putting a second version negotiation on the same
//! connection would give a peer two answers to "which version are we speaking".
//!
//! A connection begins with a handshake frame carrying the bearer token. Nothing else is read
//! from a connection until the handshake succeeds, so an unauthenticated peer cannot use
//! this socket to make the runtime do work of any kind.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ncui_core::{Limits, NcError, Result};
use ncui_security::authz::AgentRole;
use ncui_security::ratelimit::RateLimiter;
use ncui_security::token::SecretToken;
use serde_json::{Value, json};
use tracing::warn;

use crate::Runtime;
use crate::agent::AgentSession;
use crate::dispatch::dispatch_with;
use crate::rpc::{Request, Response};

/// The first frame a connection must send.
///
/// The token travels as lowercase hex so the frame is valid JSON without base64 or an
/// escaping scheme of its own. This is public because a peer needs to be able to produce
/// one: the server side of a protocol that clients cannot speak is not a protocol.
#[must_use]
pub fn handshake_frame(token: &SecretToken) -> String {
    json!({ "token": hex_encode(token.expose_secret()) }).to_string()
}

/// Encodes bytes as lowercase hex.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut acc, byte| {
        let _ = write!(acc, "{byte:02x}");
        acc
    })
}

/// What a connection reported before it was closed. Surfaced for tests and for the
/// inspector, because "the server accepted a connection" is not the same claim as "the
/// server served a request".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionOutcome {
    /// The peer presented a valid token and the connection was served.
    Authenticated,
    /// The handshake was absent, malformed, or carried the wrong token.
    Rejected,
    /// The connection was closed after a limit was reached.
    Refused,
}

/// Counters describing what the server has done, for `ncui describe` and for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerStats {
    /// Connections accepted.
    pub connections: u64,
    /// Connections rejected during the handshake.
    pub rejected: u64,
    /// Connections refused because the limit was already reached.
    pub refused: u64,
    /// Requests dispatched, whatever their outcome.
    pub requests: u64,
    /// Connections that ended with an I/O or protocol error rather than a clean close.
    pub connection_errors: u64,
}

/// A JSON-RPC server for the agent API, bound to loopback.
pub struct AgentServer {
    runtime: Arc<Mutex<Runtime>>,
    listener: TcpListener,
    local_addr: SocketAddr,
    token: SecretToken,
    role: AgentRole,
    limits: Limits,
    shutdown: Arc<AtomicBool>,
    live: Arc<AtomicU64>,
    /// The feature registry served by `ncui.feature.status`, when one was supplied.
    ///
    /// The method is advertised to every authenticated agent and documented as reporting
    /// which features are complete. It previously always answered `NotFound`, because the
    /// transport passed `None` while its documentation claimed it reported "that none is
    /// loaded" — an advertised method that could never succeed. A server given a registry
    /// now serves it; a server given none still refuses, and says why.
    registry: Option<Value>,
    stats: Arc<Mutex<ServerStats>>,
}

impl std::fmt::Debug for AgentServer {
    /// Reports the server's shape without disclosing the credential.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentServer")
            .field("local_addr", &self.local_addr)
            .field("role", &self.role)
            .field("token", &"<redacted>")
            .field("limits", &self.limits)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl AgentServer {
    /// Binds a server to an ephemeral loopback port.
    ///
    /// An ephemeral port rather than a fixed one keeps tests independent and means a
    /// developer running two instances does not collide.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the limits are incoherent, and an I/O error when
    /// the socket cannot be bound.
    pub fn bind(
        runtime: Arc<Mutex<Runtime>>,
        token: SecretToken,
        role: AgentRole,
        limits: Limits,
    ) -> Result<Self> {
        Self::bind_with_registry(runtime, token, role, limits, None)
    }

    /// Binds a server that serves `registry` through `ncui.feature.status`.
    ///
    /// A running runtime has no registry on disk by construction, so one is supplied by the
    /// caller rather than discovered. Without it the method reports that no registry is
    /// loaded, which is a truthful answer to a question the caller cannot otherwise answer
    /// over the socket — and is now documented as a refusal rather than presented as a
    /// method that works.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the limits are incoherent, and an I/O error when
    /// the socket cannot be bound.
    pub fn bind_with_registry(
        runtime: Arc<Mutex<Runtime>>,
        token: SecretToken,
        role: AgentRole,
        limits: Limits,
        registry: Option<Value>,
    ) -> Result<Self> {
        limits.validate()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            runtime,
            listener,
            local_addr,
            token,
            role,
            limits,
            shutdown: Arc::new(AtomicBool::new(false)),
            live: Arc::new(AtomicU64::new(0)),
            registry,
            stats: Arc::new(Mutex::new(ServerStats::default())),
        })
    }

    /// The address a client should connect to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> ServerStats {
        *self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A live handle to the counters.
    ///
    /// [`AgentServer::serve`] consumes the server, so a test that wants to observe what
    /// happened after handing it off needs a handle rather than a snapshot. Without this,
    /// `connection_errors` could only be asserted by rebuilding the server in-process — and
    /// in practice nobody did, so a counter that exists to make a class of client bug
    /// visible was itself unverified.
    #[must_use]
    pub fn stats_handle(&self) -> Arc<Mutex<ServerStats>> {
        Arc::clone(&self.stats)
    }

    /// Serves until [`AgentServer::shutdown`] is called.
    ///
    /// Each connection is handled on its own thread, bounded by
    /// `Limits::max_in_flight_requests` so a peer cannot exhaust the process by opening
    /// sockets.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the accept loop fails for a reason other than the listener
    /// being closed by shutdown.
    pub fn serve(self) -> Result<()> {
        // One budget for the whole server, not one per connection. Constructing it here
        // means a peer that reconnects spends from the bucket it already drained, so the
        // documented rate is the rate the peer actually gets.
        let rate_limiter = RateLimiter::new(
            self.limits.agent_rate_per_second,
            self.limits.agent_rate_burst,
        )?;
        // `SecretToken` is deliberately neither `Clone` nor `Debug`, so it is moved into
        // the shared state rather than copied.
        let shared = Arc::new(ServerShared {
            runtime: Arc::clone(&self.runtime),
            token: self.token,
            role: self.role,
            limits: self.limits,
            shutdown: Arc::clone(&self.shutdown),
            live: Arc::clone(&self.live),
            next_connection_id: AtomicU64::new(1),
            rate_limiter: Arc::new(Mutex::new(rate_limiter)),
            registry: self.registry.clone(),
            stats: Arc::clone(&self.stats),
        });
        let shutdown = Arc::clone(&self.shutdown);
        let live = Arc::clone(&self.live);
        let limits = self.limits;
        let bump = Arc::clone(&self.stats);

        // The page pump. A page's messages are served even when no agent is connected,
        // because the page is the *application* and the agent is the observer — an
        // application that only ran when something was watching it would not be an
        // application. The pump stops when the server does.
        {
            let runtime = Arc::clone(&shared.runtime);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    let mut runtime = runtime
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let now = runtime.elapsed();
                    runtime.pump_pages(now);
                    drop(runtime);
                    // A short sleep rather than a busy loop: the pump is idle most of the
                    // time, and a busy loop would burn a core for the sake of a page that
                    // has nothing to say.
                    std::thread::sleep(Duration::from_millis(25));
                }
            });
        }

        for incoming in self.listener.incoming() {
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            let stream = match incoming {
                Ok(stream) => stream,
                Err(error) => {
                    // A refused connection is not a reason to tear the listener down; the
                    // next accept may well succeed.
                    warn!(%error, "agent server failed to accept a connection");
                    continue;
                }
            };
            let mut counters = bump
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if live.load(Ordering::Acquire) >= limits.max_in_flight_requests as u64 {
                counters.refused += 1;
                drop(counters);
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }
            counters.connections += 1;
            drop(counters);
            let served = Arc::clone(&shared);
            std::thread::spawn(move || {
                // A connection handler fails when a peer hangs up mid-frame or sends
                // something unreadable. Discarding that would make a misbehaving client
                // completely invisible, so it is counted and logged; the connection ends
                // either way, and a hung-up client is not a server error.
                if let Err(error) = served.handle(stream) {
                    served.bump(|stats| stats.connection_errors += 1);
                    warn!(%error, "agent connection ended with an error");
                }
            });
        }
        Ok(())
    }

    /// Stops the accept loop.
    ///
    /// Connecting to the listener once unblocks the pending `accept`, which is the only
    /// portable way to wake one.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.local_addr);
    }
}

/// State every connection handler shares.
struct ServerShared {
    runtime: Arc<Mutex<Runtime>>,
    token: SecretToken,
    role: AgentRole,
    limits: Limits,
    shutdown: Arc<AtomicBool>,
    live: Arc<AtomicU64>,
    /// Hands out a distinct identity per accepted connection.
    ///
    /// `live` counts connections *currently open* and is decremented when one ends, so it
    /// is not an identity: deriving a name from it gave every connection that completed its
    /// handshake before the previous one incremented the counter the same name, and three
    /// sequential authenticated clients all recorded as `tcp-1`. An audit trail that cannot
    /// distinguish its actors cannot answer "who did this", which is the stated purpose of
    /// the capability pipeline. This counter is monotonic and never reused within a serve.
    next_connection_id: AtomicU64,
    /// The rate limiter shared by every connection handler.
    ///
    /// Constructing a limiter per connection handed a peer holding the bearer token a fresh
    /// full bucket on every reconnect, so the aggregate rate was `burst × concurrent
    /// connections` — with the default limits, 400 × 4096 ≈ 1.6M requests/second against a
    /// documented 200/second. Sharing it makes the documented limit the real one: the
    /// budget belongs to the authenticated agent, not to each TCP session it opens.
    rate_limiter: Arc<Mutex<RateLimiter>>,
    /// The registry `ncui.feature.status` serves, when the caller supplied one.
    registry: Option<Value>,
    stats: Arc<Mutex<ServerStats>>,
}

impl ServerShared {
    /// Applies a change to the shared counters.
    fn bump(&self, apply: impl FnOnce(&mut ServerStats)) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply(&mut stats);
    }

    fn handle(&self, stream: TcpStream) -> std::io::Result<ConnectionOutcome> {
        self.live.fetch_add(1, Ordering::AcqRel);
        let result = self.serve_connection(&stream);
        self.live.fetch_sub(1, Ordering::AcqRel);
        drop(stream);
        result
    }

    fn serve_connection(&self, stream: &TcpStream) -> std::io::Result<ConnectionOutcome> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
        let mut reader = stream.try_clone()?;
        let mut writer = stream.try_clone()?;

        // --- handshake ---------------------------------------------------
        let Some(presented) = read_frame(&mut reader, self.limits.max_ipc_frame_bytes)? else {
            return Ok(self.reject());
        };
        let Ok(handshake) = serde_json::from_slice::<Value>(&presented) else {
            return Ok(self.reject());
        };
        let Some(encoded) = handshake.get("token").and_then(Value::as_str) else {
            return Ok(self.reject());
        };
        let Some(bytes) = decode_hex(encoded) else {
            return Ok(self.reject());
        };

        // Constant-time comparison against the expected credential. A rejected peer learns
        // nothing from *which* part of its guess was wrong.
        if !self.token.verify(&bytes) {
            return Ok(self.reject());
        }

        // The presented bytes already matched in constant time, so constructing a token
        // from them cannot fail for any reason the peer controls.
        let Some(presented_token) = SecretToken::new(bytes).ok() else {
            return Ok(self.reject());
        };
        // The identity comes from a monotonic counter rather than the live-connection
        // count: two connections that both completed their handshake before either
        // incremented `live` would otherwise record the same actor, and the audit trail
        // exists to name the actor.
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let mut session = match AgentSession::with_limiter(
            format!("tcp-{connection_id}"),
            self.role,
            Arc::clone(&self.rate_limiter),
        ) {
            Ok(session) => session,
            Err(error) => {
                warn!(%error, "agent server could not create a session");
                return Ok(self.reject());
            }
        };
        if session.authenticate(&presented_token, &self.token).is_err() {
            return Ok(self.reject());
        }

        // --- request loop ------------------------------------------------
        let started = Instant::now();
        while !self.shutdown.load(Ordering::Acquire) {
            let Some(frame) = read_frame(&mut reader, self.limits.max_ipc_frame_bytes)? else {
                break;
            };
            self.bump(|stats| stats.requests += 1);

            let response = match serde_json::from_slice::<Request>(&frame) {
                Ok(request) => {
                    let now = started.elapsed();
                    let mut runtime = self
                        .runtime
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match dispatch_with(
                        &mut runtime,
                        &mut session,
                        &request,
                        now,
                        self.registry.as_ref(),
                        &method_table(),
                    ) {
                        Some(response) => response,
                        None => continue,
                    }
                }
                // A malformed frame is answered rather than dropped, so a client learns
                // what it did wrong instead of seeing a silent hang.
                Err(error) => Response::failure(
                    Value::Null,
                    crate::rpc::RpcError::from_nc(
                        &NcError::new(ncui_core::ErrorKind::Protocol {
                            reason: format!("request is not valid JSON-RPC: {error}"),
                        })
                        .operation("agent::transport"),
                    ),
                ),
            };

            let encoded = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
            if write_frame(&mut writer, &encoded, self.limits.max_ipc_frame_bytes).is_err() {
                break;
            }
        }

        let _ = writer.flush();
        Ok(ConnectionOutcome::Authenticated)
    }

    /// Records a refusal and closes the connection.
    ///
    /// The refusal is deliberately uninformative: the peer is not told whether its token
    /// was wrong, malformed, or absent, because that difference is a probing aid. Closing
    /// without a reply is itself the answer, and it is cheaper for a peer that is guessing
    /// than a protocol-level error would be.
    fn reject(&self) -> ConnectionOutcome {
        self.bump(|stats| stats.rejected += 1);
        ConnectionOutcome::Rejected
    }
}

/// Decodes lowercase hex, or `None` if the text is not a valid encoding.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

/// Reads one length-prefixed frame.
///
/// The declared length is validated against `max_bytes` **before** any buffer is resized.
/// A peer that announces four gigabytes must cost us nothing.
///
/// Returns `Ok(None)` at a clean end of stream, so a closed connection is not an error.
fn read_frame(reader: &mut TcpStream, max_bytes: u32) -> std::io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }

    let length = u32::from_be_bytes(header);
    if length > max_bytes {
        // Refusing here is the whole point: the allocation that `length` would justify has
        // not happened yet.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame of {length} bytes exceeds the {max_bytes} byte limit"),
        ));
    }

    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// Writes one length-prefixed frame.
fn write_frame(writer: &mut TcpStream, payload: &[u8], max_bytes: u32) -> std::io::Result<()> {
    if payload.len() as u64 > u64::from(max_bytes) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response exceeds the negotiated frame limit",
        ));
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// The method table every connection dispatches against.
///
/// Named for what it is, unlike the predecessor that returned this list from a function
/// called `registry_view` and was passed as the *registry* argument to `dispatch_with` —
/// where it could never satisfy the lookup, which is why `ncui.feature.status` answered
/// `NotFound` on every call.
fn method_table() -> Vec<crate::describe::MethodDescription> {
    crate::dispatch::method_table()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_security::token::SecretToken;

    fn token(byte: u8) -> SecretToken {
        SecretToken::new(vec![byte; 32]).expect("token")
    }

    #[test]
    fn hex_round_trips() {
        let bytes = vec![0x00, 0x0f, 0xff, 0xa5];
        assert_eq!(decode_hex(&hex_encode(&bytes)), Some(bytes));
        assert_eq!(decode_hex("zz"), None, "invalid hex must be refused");
        assert_eq!(decode_hex("abc"), None, "odd length must be refused");
    }

    #[test]
    fn a_handshake_frame_is_valid_json_carrying_the_token() {
        let frame = handshake_frame(&token(0x5a));
        let parsed: Value = serde_json::from_str(&frame).expect("handshake must be valid JSON");
        assert_eq!(parsed["token"], hex_encode(&[0x5a; 32]));
    }

    #[test]
    fn the_server_binds_loopback_only() {
        let limits = Limits::default();
        let runtime = Arc::new(Mutex::new(Runtime::start(limits).expect("runtime")));
        let server =
            AgentServer::bind(runtime, token(1), AgentRole::Observer, limits).expect("bind");
        assert!(
            server.local_addr().ip().is_loopback(),
            "the server must never bind a routable interface"
        );
    }

    #[test]
    fn incoherent_limits_are_refused_rather_than_clamped() {
        let broken = Limits {
            max_ipc_frame_bytes: 0,
            ..Limits::default()
        };
        let runtime = Arc::new(Mutex::new(
            Runtime::start(Limits::default()).expect("runtime"),
        ));
        let err = AgentServer::bind(runtime, token(1), AgentRole::Observer, broken)
            .expect_err("incoherent limits must be refused");
        assert_eq!(err.code(), "validation");
    }

    #[test]
    fn stats_start_at_zero() {
        let limits = Limits::default();
        let runtime = Arc::new(Mutex::new(Runtime::start(limits).expect("runtime")));
        let server =
            AgentServer::bind(runtime, token(1), AgentRole::Observer, limits).expect("bind");
        assert_eq!(server.stats(), ServerStats::default());
    }
}
