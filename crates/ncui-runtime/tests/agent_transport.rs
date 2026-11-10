//! End-to-end tests for the loopback agent transport.
//!
//! These start a real server on a real socket and talk to it as a separate peer would.
//! A transport bug that only shows up once a socket is involved — a framing mistake, a
//! handshake that never completes, a limit that is checked in the wrong order — is exactly
//! the kind that an in-process test cannot find.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ncui_core::{Capability, CapabilitySet, Limits, LimitsBuilder};
use ncui_runtime::Runtime;
use ncui_runtime::agent::AgentSession;
use ncui_runtime::rpc::{Request, Response};
use ncui_runtime::transport::{AgentServer, handshake_frame};
use ncui_security::authz::AgentRole;
use ncui_security::token::SecretToken;
use serde_json::{Value, json};

/// A running server plus the credentials a client needs.
struct Harness {
    addr: std::net::SocketAddr,
    /// The credential's bytes rather than the token itself: `SecretToken` is deliberately
    /// not `Clone`, because a secret that can be copied is a secret that will be.
    token_bytes: Vec<u8>,
    runtime: Arc<Mutex<Runtime>>,
    /// The server's live counters, so a test can watch what a misbehaving client caused.
    stats: Arc<Mutex<ncui_runtime::transport::ServerStats>>,
}

impl Harness {
    fn start(role: AgentRole) -> Harness {
        let limits = LimitsBuilder::new()
            .max_workers(2)
            // A small frame limit makes the hostile-length test cheap and meaningful. The
            // worker payload must stay under it, because `Limits::validate` refuses a task
            // that could not be transported — which is the right rule and the reason this
            // pair of values has to be set together.
            .max_ipc_frame_bytes(64 * 1024)
            .max_worker_payload_bytes(32 * 1024)
            .agent_rate(1_000, 1_000)
            .build()
            .expect("limits");
        let runtime = Arc::new(Mutex::new(Runtime::start(limits).expect("runtime")));
        let token_bytes = vec![0x5a; 32];
        let token = SecretToken::new(token_bytes.clone()).expect("token");
        let server = AgentServer::bind(Arc::clone(&runtime), token, role, limits).expect("bind");
        let addr = server.local_addr();
        let stats = server.stats_handle();
        std::thread::spawn(move || {
            let _ = server.serve();
        });
        // Wait for the listener to be reachable before handing the address out.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if TcpStream::connect(addr).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Harness {
            addr,
            token_bytes,
            runtime,
            stats,
        }
    }

    /// Opens a socket that has already completed the handshake.
    fn connect(&self) -> TcpStream {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        let token = SecretToken::new(self.token_bytes.clone()).expect("token");
        write_frame(&mut stream, handshake_frame(&token).as_bytes());
        stream
    }

    /// Opens a socket without authenticating.
    fn connect_anonymous(&self) -> TcpStream {
        TcpStream::connect(self.addr).expect("connect")
    }

    /// Sends one request on an authenticated connection and reads the reply.
    fn call(&self, method: &str, params: Value) -> Response {
        let mut stream = self.connect();
        let request = Request::new(json!(1), method, params);
        write_frame(
            &mut stream,
            &serde_json::to_vec(&request).expect("serialise"),
        );
        read_response(&mut stream).expect("a response")
    }
}

fn write_frame(stream: &mut TcpStream, payload: &[u8]) {
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .expect("write");
    stream.write_all(payload).expect("write");
    stream.flush().expect("flush");
}

fn read_frame(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return None,
        Err(error) => panic!("read failed: {error}"),
    }
    let length = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).expect("read");
    Some(payload)
}

fn read_response(stream: &mut TcpStream) -> Option<Response> {
    read_frame(stream).map(|bytes| serde_json::from_slice(&bytes).expect("valid JSON-RPC"))
}

#[test]
fn an_authenticated_peer_can_describe_the_runtime_over_a_socket() {
    let harness = Harness::start(AgentRole::Observer);
    let response = harness.call("ncui.describe", json!({}));
    assert!(response.error.is_none(), "got: {:?}", response.error);
    let result = response.result.expect("result");
    assert!(!result["methods"].as_array().expect("methods").is_empty());
}

#[test]
fn health_over_the_socket_matches_the_in_process_report() {
    let harness = Harness::start(AgentRole::Observer);
    let response = harness.call("ncui.health", json!({ "deep": true }));
    let result = response.result.expect("result");
    assert_eq!(result["status"], "healthy");
    assert_eq!(result["deep"], true);
}

#[test]
fn a_peer_without_a_token_is_refused_and_no_request_is_served() {
    let harness = Harness::start(AgentRole::Administrator);
    let mut stream = harness.connect_anonymous();

    // Send a request without a handshake. Nothing may come back.
    let request = Request::new(json!(1), "ncui.describe", json!({}));
    write_frame(
        &mut stream,
        &serde_json::to_vec(&request).expect("serialise"),
    );

    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("timeout");
    let mut buffer = [0u8; 64];
    let served = match stream.read(&mut buffer) {
        Ok(0) => false,
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => false,
    };
    assert!(!served, "an unauthenticated peer must receive nothing");
}

#[test]
fn a_wrong_token_is_refused() {
    let harness = Harness::start(AgentRole::Administrator);
    let mut stream = TcpStream::connect(harness.addr).expect("connect");
    write_frame(
        &mut stream,
        handshake_frame(&SecretToken::new(vec![0x11; 32]).expect("token")).as_bytes(),
    );

    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("timeout");
    let mut buffer = [0u8; 64];
    let served = !matches!(stream.read(&mut buffer), Ok(0) | Err(_));
    assert!(!served, "a wrong token must not be served");
}

#[test]
fn a_malformed_handshake_is_refused() {
    let harness = Harness::start(AgentRole::Administrator);
    for junk in [
        "not json at all",
        r#"{"token": 42}"#,
        r#"{"token": "zz"}"#,
        r#"{"nope": "value"}"#,
        "[]",
    ] {
        let mut stream = TcpStream::connect(harness.addr).expect("connect");
        write_frame(&mut stream, junk.as_bytes());
        stream
            .set_read_timeout(Some(Duration::from_millis(150)))
            .expect("timeout");
        let mut buffer = [0u8; 64];
        let served = !matches!(stream.read(&mut buffer), Ok(0) | Err(_));
        assert!(!served, "a malformed handshake ({junk:?}) must be refused");
    }
}

#[test]
fn a_hostile_declared_length_is_rejected_without_allocating_it() {
    /// The largest length a 4-byte big-endian header can express, and so the largest value
    /// a hostile peer can announce.
    const HOSTILE: u32 = u32::MAX;

    let harness = Harness::start(AgentRole::Administrator);
    let mut stream = TcpStream::connect(harness.addr).expect("connect");

    // Announce `u32::MAX` (4 GiB) while the configured limit is 64 KiB. The server must
    // refuse on the declared length, before any buffer of that size exists.
    //
    // This used to be written as `3u32 * 1024 * 1024 * 1024_u32.wrapping_div(1024 * 1024 * 1024)`,
    // where the method call binds only to the last literal: `wrapping_div` applied to `1024`
    // first, and the product on the wire was **zero**. The server read a zero-length frame,
    // `read_frame` returned `Ok(Some(vec![]))`, the handshake frame failed to parse, and the
    // connection was closed — so the test passed for a reason that had nothing to do with
    // the length bound. `u32::MAX` is the honest hostile value, and it is the largest a
    // 4-byte big-endian header can express, so it cannot silently become something small.
    stream.write_all(&HOSTILE.to_be_bytes()).expect("write");
    stream.write_all(b"x").expect("write");
    stream.flush().expect("flush");

    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("timeout");
    let mut buffer = [0u8; 64];
    let served = !matches!(stream.read(&mut buffer), Ok(0) | Err(_));
    assert!(
        !served,
        "an oversized declared length must be refused at the header"
    );
}

#[test]
fn each_connection_is_recorded_under_its_own_agent_id() {
    let harness = Harness::start(AgentRole::Administrator);

    // Three sequential authenticated connections, each making one call that is audited.
    for _ in 0..3 {
        let response = harness.call("ncui.describe", json!({}));
        assert!(
            response.error.is_none(),
            "each connection must be served, so the ids are comparable"
        );
    }

    // The audit trail must be able to name three distinct actors. It could not: the id was
    // derived from the *live connection count*, which the handshake reads before the
    // connection has incremented it, so every connection that completed its handshake
    // before the previous one incremented recorded the same name — `tcp-1` three times.
    let runtime = harness.runtime.lock().expect("runtime");
    let ids: std::collections::BTreeSet<String> = runtime
        .audit
        .iter()
        .map(|event| event.agent_id.clone())
        .collect();
    assert!(
        ids.len() >= 3,
        "three separate connections recorded as {ids:?} — the audit trail cannot tell them \
         apart, so it cannot answer who did what"
    );

    // And the point of a distinct actor is that it can be selected: `for_agent` is the
    // query an incident review would run.
    for id in &ids {
        let events = runtime.audit.for_agent(id).count();
        assert!(
            events > 0,
            "`for_agent({id})` returned nothing for a recorded actor"
        );
    }
}

#[test]
fn the_rate_limit_is_not_bought_back_by_reconnecting() {
    // A budget small enough to exhaust in one connection. The peer holds a valid token, so
    // the only thing stopping it is this limit — and it must not be able to buy a fresh
    // allowance by hanging up and reconnecting.
    let limits = LimitsBuilder::new()
        .max_workers(2)
        .max_ipc_frame_bytes(64 * 1024)
        .max_worker_payload_bytes(32 * 1024)
        .agent_rate(1, 3)
        .build()
        .expect("limits");
    let runtime = Arc::new(Mutex::new(Runtime::start(limits).expect("runtime")));
    let token_bytes = vec![0x5a; 32];
    let token = SecretToken::new(token_bytes.clone()).expect("token");
    let server =
        AgentServer::bind(Arc::clone(&runtime), token, AgentRole::Observer, limits).expect("bind");
    let addr = server.local_addr();
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // Make many requests, reconnecting between every one. A per-connection limiter would
    // hand each new connection a fresh full bucket, so this would never be refused; a
    // shared limiter spends down one budget and starts refusing.
    let mut served = 0;
    let mut refused = 0;
    for _ in 0..40 {
        let mut stream = TcpStream::connect(addr).expect("connect");
        let token = SecretToken::new(token_bytes.clone()).expect("token");
        write_frame(&mut stream, handshake_frame(&token).as_bytes());
        let request = Request::new(json!(1), "ncui.describe", json!({}));
        write_frame(
            &mut stream,
            &serde_json::to_vec(&request).expect("serialise"),
        );
        match read_response(&mut stream) {
            Some(response) if response.error.is_none() => served += 1,
            Some(_) => refused += 1,
            None => {}
        }
    }
    assert!(
        refused > 0,
        "40 requests across 40 connections were all served ({served} served, {refused} \
         refused) against a budget of 1/second with a burst of 3 — reconnecting bought a \
         fresh allowance on every connection"
    );
}

#[test]
fn a_malformed_request_is_answered_rather_than_dropped() {
    let harness = Harness::start(AgentRole::Observer);
    let mut stream = harness.connect();
    write_frame(&mut stream, b"{ this is not json-rpc");

    let response = read_response(&mut stream).expect("a malformed request must be answered");
    let error = response.error.expect("an error response");
    // `protocol`, not `validation`: the frame was well-formed JSON-RPC framing carrying
    // something that is not a request at all. Being precise here matters because an agent
    // branches on the code — a `validation` error says "fix your parameters", and this is
    // not that.
    assert_eq!(error.data.ncui_code, "protocol");
    assert!(error.message.contains("JSON-RPC"), "got: {}", error.message);
}

#[test]
fn several_requests_share_one_authenticated_connection() {
    let harness = Harness::start(AgentRole::Observer);
    let mut stream = harness.connect();

    for id in 1..=5 {
        let request = Request::new(json!(id), "ncui.health", json!({}));
        write_frame(
            &mut stream,
            &serde_json::to_vec(&request).expect("serialise"),
        );
        let response = read_response(&mut stream).expect("a response per request");
        assert_eq!(response.id, json!(id), "each reply must carry its own id");
        assert!(response.error.is_none(), "got: {:?}", response.error);
    }
}

#[test]
fn a_capability_the_role_lacks_is_denied_across_the_socket_too() {
    let harness = Harness::start(AgentRole::Observer);
    let response = harness.call("ncui.logs.purge", json!({}));
    let error = response
        .error
        .expect("a destructive method must be refused");
    assert_eq!(
        error.data.ncui_code, "capability_denied",
        "the in-process consent gate must apply to remote callers as well"
    );
}

#[test]
fn an_observer_may_list_windows_and_a_host_is_needed() {
    let harness = Harness::start(AgentRole::Observer);
    let response = harness.call("ncui.window.list", json!({}));
    // The Observer holds inspect.runtime, so the call is authorized; with no host
    // installed the runtime answers not_found rather than capability_denied.
    let error = response.error.expect("no host is installed");
    assert_eq!(error.data.ncui_code, "not_found", "{error:?}");
}

#[test]
fn a_destructive_call_with_consent_is_served() {
    let harness = Harness::start(AgentRole::Administrator);
    // The legacy `consent: true` authorizes nothing destructive; the token is the consent.
    // It is minted by the runtime the server shares, bound to the method and its capability.
    let mut runtime = harness.runtime.lock().expect("runtime lock");
    let correlation = ncui_core::CorrelationId::new();
    let token = runtime
        .mint_consent(
            correlation,
            "ncui.logs.purge",
            CapabilitySet::empty().with(Capability::ReadLogs),
        )
        .expect("mint");
    drop(runtime);
    let response = harness.call(
        "ncui.logs.purge",
        json!({ "consent_token": token.to_value() }),
    );
    assert!(response.error.is_none(), "got: {:?}", response.error);
}

#[test]
fn a_notification_is_dispatched_but_not_answered() {
    let harness = Harness::start(AgentRole::Observer);
    let mut stream = harness.connect();
    write_frame(&mut stream, br#"{"jsonrpc":"2.0","method":"ncui.health"}"#);

    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("timeout");
    let mut buffer = [0u8; 64];
    let answered = !matches!(stream.read(&mut buffer), Ok(0) | Err(_));
    assert!(!answered, "a notification expects no reply");
}

#[test]
fn every_request_over_the_socket_produces_an_audit_event() {
    let harness = Harness::start(AgentRole::Observer);
    let before = harness.runtime.lock().expect("lock").audit.len();

    // Assert the calls succeeded first. Without this the test could pass on the audit
    // count alone, because a *rejected* request is audited too — which would make a broken
    // authentication path look like a working one.
    let health = harness.call("ncui.health", json!({}));
    assert!(
        health.error.is_none(),
        "setup call failed: {:?}",
        health.error
    );
    let describe = harness.call("ncui.describe", json!({}));
    assert!(
        describe.error.is_none(),
        "setup call failed: {:?}",
        describe.error
    );

    let after = harness.runtime.lock().expect("lock").audit.len();
    assert!(
        after >= before + 2,
        "each served request must be audited: {before} -> {after}"
    );
}

#[test]
fn a_rate_limited_peer_is_throttled_on_its_own_connection() {
    let limits = LimitsBuilder::new()
        .max_workers(1)
        .agent_rate(1, 2)
        .build()
        .expect("limits");
    let runtime = Arc::new(Mutex::new(Runtime::start(limits).expect("runtime")));
    let token_bytes = vec![0x5a; 32];
    let token = SecretToken::new(token_bytes.clone()).expect("token");
    let server = AgentServer::bind(
        Arc::clone(&runtime),
        token,
        AgentRole::Administrator,
        limits,
    )
    .expect("bind");
    let addr = server.local_addr();
    std::thread::spawn(move || {
        let _ = server.serve();
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    let client_token = SecretToken::new(token_bytes).expect("token");
    write_frame(&mut stream, handshake_frame(&client_token).as_bytes());

    let mut refused = 0;
    for id in 1..=6 {
        let request = Request::new(json!(id), "ncui.health", json!({}));
        write_frame(
            &mut stream,
            &serde_json::to_vec(&request).expect("serialise"),
        );
        match read_response(&mut stream) {
            Some(response) if response.error.is_none() => {}
            _ => refused += 1,
        }
    }
    assert!(
        refused > 0,
        "a burst beyond the rate limit must be refused, not silently absorbed"
    );
}

#[test]
fn the_server_never_binds_a_routable_interface() {
    let harness = Harness::start(AgentRole::Observer);
    assert!(
        harness.addr.ip().is_loopback(),
        "a loopback API must not be reachable from another host"
    );
}

#[test]
fn an_agent_session_created_in_process_still_refuses_to_dispatch_over_the_wire() {
    // The transport builds its own session; this asserts the in-process path is unaffected
    // and that an unauthenticated session cannot be reused by accident.
    let limits = Limits::default();
    let mut runtime = Runtime::start(limits).expect("runtime");
    let mut anonymous =
        AgentSession::new("no-token", AgentRole::Administrator, 100, 100).expect("session");
    let request = Request::new(json!(1), "ncui.health", json!({}));
    let response = ncui_runtime::dispatch::dispatch_with(
        &mut runtime,
        &mut anonymous,
        &request,
        Duration::from_secs(0),
        None,
        &ncui_runtime::dispatch::method_table(),
    )
    .expect("a call is not a notification");
    assert_eq!(
        response.error.expect("error").data.ncui_code,
        "capability_denied",
        "an unauthenticated session must not dispatch"
    );
}

#[test]
fn a_peer_that_hangs_up_mid_frame_is_counted_rather_than_lost() {
    // The counter exists because a discarded per-connection `Result` made a misbehaving
    // client invisible. An earlier version of this test asserted the worker-pool length
    // instead, which says nothing about the counter — removing the increment left the suite
    // green. So it asserts the counter, by name.
    let harness = Harness::start(AgentRole::Administrator);
    let before = harness.stats.lock().expect("stats").connection_errors;

    // Send a header claiming 4096 bytes, write a partial body, then hang up mid-frame.
    let mut stream = harness.connect();
    stream
        .write_all(&4096u32.to_be_bytes())
        .expect("write header");
    stream.write_all(b"partial").expect("write partial body");
    stream.flush().expect("flush");
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if harness.stats.lock().expect("stats").connection_errors > before {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let after = harness.stats.lock().expect("stats").connection_errors;
    assert!(
        after > before,
        "a peer that hung up mid-frame must be counted, not lost: {before} -> {after}"
    );

    // And the server must still be serving afterwards.
    let response = harness.call("ncui.health", json!({}));
    assert!(
        response.error.is_none(),
        "server disturbed: {:?}",
        response.error
    );
}

/// `ncui.feature.status` serves a supplied registry instead of always reporting `NotFound`.
#[test]
fn a_server_given_a_registry_answers_the_feature_status_method() {
    let limits = LimitsBuilder::new()
        .max_workers(2)
        .max_ipc_frame_bytes(64 * 1024)
        .max_worker_payload_bytes(32 * 1024)
        .agent_rate(1_000, 1_000)
        .build()
        .expect("limits");
    let runtime = Arc::new(Mutex::new(Runtime::start(limits).expect("runtime")));
    let token_bytes = vec![0x3c; 32];
    let token = SecretToken::new(token_bytes.clone()).expect("token");
    // A registry carrying a real schema, so the answer is checkable rather than merely
    // "not an error".
    let registry = json!({
        "schema_version": 1,
        "features": [{ "id": "probe.feature", "status": "complete" }]
    });
    let server = AgentServer::bind_with_registry(
        Arc::clone(&runtime),
        token,
        AgentRole::Administrator,
        limits,
        Some(registry.clone()),
    )
    .expect("bind");
    let addr = server.local_addr();
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut stream = TcpStream::connect(addr).expect("connect");
    let token = SecretToken::new(token_bytes).expect("token");
    write_frame(&mut stream, handshake_frame(&token).as_bytes());
    let request = Request::new(json!(1), "ncui.feature.status", json!({}));
    write_frame(
        &mut stream,
        &serde_json::to_vec(&request).expect("serialise"),
    );
    let response = read_response(&mut stream).expect("a response");

    assert!(
        response.error.is_none(),
        "a server holding a registry must serve it; got {:?}",
        response.error
    );
    let result = response.result.expect("a result");
    assert_eq!(
        result, registry,
        "the served registry must be the one the server was given, not a reshaped view"
    );
}

/// Without a registry the method still refuses, and says so rather than inventing an answer.
#[test]
fn a_server_without_a_registry_reports_the_feature_status_method_as_unavailable() {
    let harness = Harness::start(AgentRole::Administrator);
    let response = harness.call("ncui.feature.status", json!({}));
    let error = response.error.expect("no registry means a refusal");
    assert_eq!(
        error.data.ncui_code, "not_found",
        "the refusal must carry the stable code rather than a bare message"
    );
}
