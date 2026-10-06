#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests must abort loudly on failure"
)]

//! Integration tests for the lifecycle primitives: request bookkeeping, backpressure, and
//! cancellation, exercised the way a transport would drive them.
//!
//! The point of these is the *interaction*. A registry that is correct in isolation and a
//! channel that is correct in isolation can still be wrong together — a response arriving for
//! an expired request, a queue that fills exactly when a cancellation storm arrives. That is
//! what these tests cover.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ncui_core::{CorrelationId, ErrorKind, Limits, RequestId};
use ncui_ipc::{
    BackpressurePolicy, BoundedChannel, CancellationToken, Message, RequestOutcome,
    RequestRegistry, decode_messages, encode_messages,
};

/// A frame limit for the encoded messages.
const LIMIT: u32 = 64 * 1024;

#[test]
fn a_request_survives_the_whole_trip_across_the_wire_and_is_only_completed_once() {
    let registry = RequestRegistry::from_limits(&Limits::default());
    let correlation = CorrelationId::new();
    let request_id = RequestId::new();

    let request = Message::Request {
        method: "window.list".to_owned(),
        params: Vec::new(),
        correlation,
        request_id,
        deadline_ms: 5_000,
    };
    registry
        .register(request_id, correlation)
        .expect("register");

    // The request itself must survive the wire before its response is meaningful, so encode
    // and decode it rather than only asserting on the reply.
    let sent = encode_messages(&[request], LIMIT).expect("encode request");
    let decoded_request = decode_messages(&sent, LIMIT).expect("decode request");
    assert_eq!(decoded_request[0].request_id(), Some(request_id));
    assert_eq!(decoded_request[0].correlation(), correlation);
    assert_eq!(decoded_request[0].name(), "window.list");

    // What the peer sends back.
    let bytes = encode_messages(
        &[Message::Response {
            request_id,
            correlation,
            result: b"[]".to_vec(),
        }],
        LIMIT,
    )
    .expect("encode");
    let replies = decode_messages(&bytes, LIMIT).expect("decode");

    for reply in &replies {
        let id = reply.request_id().expect("a response carries a request id");
        assert_eq!(
            id, request_id,
            "the response must map to the request it answers"
        );
        assert_eq!(reply.correlation(), correlation);
    }

    let record = registry.complete(request_id).expect("complete");
    assert_eq!(record.outcome, RequestOutcome::Completed);
    assert!(record.outcome.is_answered());
    assert_eq!(registry.pending_count(), 0);

    // A duplicate delivery of the same response has nothing to map to.
    for reply in &replies {
        let id = reply.request_id().expect("request id");
        assert!(registry.complete(id).is_err());
    }
}

#[test]
fn a_response_for_an_expired_request_is_refused_rather_than_delivered() {
    // The scenario that motivates the registry: a slow peer answers after the caller gave up
    // and the identifier has been reissued. Delivering the stale answer would hand one caller
    // another's result.
    let registry = RequestRegistry::new(4, 100);
    let correlation = CorrelationId::new();
    let request_id = RequestId::new();
    registry
        .register_at(request_id, correlation, 0)
        .expect("register");

    assert_eq!(registry.expire(100), vec![request_id]);
    assert!(registry.complete(request_id).is_err());

    // The identifier is free again.
    registry
        .register_at(request_id, correlation, 100)
        .expect("re-register");
    let record = registry
        .complete(request_id)
        .expect("complete the new request");
    assert_eq!(record.correlation, correlation);
}

#[test]
fn an_error_response_completes_the_request_with_a_preserved_error() {
    let registry = RequestRegistry::new(4, 1_000);
    let correlation = CorrelationId::new();
    let request_id = RequestId::new();
    registry
        .register(request_id, correlation)
        .expect("register");

    let bytes = encode_messages(
        &[Message::ErrorResponse {
            request_id,
            correlation,
            error: ncui_core::NcError::new(ErrorKind::Timeout { elapsed_ms: 250 }),
        }],
        LIMIT,
    )
    .expect("encode");
    let replies = decode_messages(&bytes, LIMIT).expect("decode");

    let reply = &replies[0];
    assert!(matches!(reply, Message::ErrorResponse { .. }));
    let record = registry
        .fail(
            request_id,
            ncui_core::NcError::new(ErrorKind::Timeout { elapsed_ms: 250 }),
        )
        .expect("fail");
    match record.outcome {
        RequestOutcome::Failed(error) => assert_eq!(error.code(), "timeout"),
        ref other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn backpressure_under_the_reject_policy_is_reported_to_the_producer() {
    let channel =
        BoundedChannel::named("agent.inbox", 2, BackpressurePolicy::Reject).expect("channel");
    assert!(channel.send(1u8).is_ok());
    assert!(channel.send(2u8).is_ok());

    let err = channel.send(3u8).expect_err("a full channel must reject");
    match err.kind() {
        ErrorKind::Backpressure {
            channel: name,
            capacity,
        } => {
            assert_eq!(name, "agent.inbox");
            assert_eq!(*capacity, 2);
        }
        other => panic!("expected Backpressure, got {other:?}"),
    }
    assert!(err.is_retryable(), "backpressure is retryable by policy");
    assert_eq!(
        channel.len(),
        2,
        "a rejected item must not displace a queued one"
    );
    assert_eq!(channel.dropped_count(), 0, "Reject must not drop");
}

#[test]
fn backpressure_under_the_drop_oldest_policy_is_counted_not_hidden() {
    let channel =
        BoundedChannel::named("log.ring", 3, BackpressurePolicy::DropOldest).expect("channel");
    for i in 0..10u8 {
        channel.send(i).expect("send");
    }
    assert_eq!(channel.len(), 3);
    assert_eq!(
        channel.dropped_count(),
        7,
        "every loss must be visible in metrics"
    );
    assert_eq!(channel.recv(), Some(7));
    assert_eq!(channel.recv(), Some(8));
    assert_eq!(channel.recv(), Some(9));
}

#[test]
fn backpressure_under_the_block_policy_waits_rather_than_losing_work() {
    let channel = Arc::new(
        BoundedChannel::named("scheduler.queue", 1, BackpressurePolicy::Block).expect("channel"),
    );
    channel.send("first".to_owned()).expect("send");
    assert!(channel.is_full());

    let consumer = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || channel.recv())
    };
    // Blocks until the consumer drains, then succeeds. Nothing is dropped and nothing fails.
    channel
        .send("second".to_owned())
        .expect("send after the consumer drains");
    assert_eq!(consumer.join().expect("join").as_deref(), Some("first"));
    assert_eq!(channel.recv().as_deref(), Some("second"));
    assert_eq!(channel.dropped_count(), 0);
}

#[test]
fn no_policy_can_push_the_queue_past_its_capacity() {
    // The invariant ADR-003 states without exception, checked for every policy under enough
    // pressure that a policy bug would show up.
    for policy in [
        BackpressurePolicy::Block,
        BackpressurePolicy::DropOldest,
        BackpressurePolicy::Reject,
    ] {
        let channel = BoundedChannel::named("bounded", 4, policy).expect("channel");
        for i in 0..1_000u32 {
            // Block is exercised through `try_send` so the test cannot park; the point here is
            // the bound, which every send path shares.
            let _ = channel.try_send(i);
            assert!(
                channel.len() <= channel.capacity(),
                "{policy:?} exceeded its capacity at item {i}"
            );
        }
        assert_eq!(channel.len(), 4);
    }
}

#[test]
fn a_cancellation_reaches_work_that_is_already_running_on_another_thread() {
    let token = CancellationToken::new();
    let worker_token = token.clone();

    let worker = std::thread::spawn(move || {
        // Stand-in for a long operation: loop until told to stop.
        let mut iterations = 0u64;
        while !worker_token.is_cancelled() {
            iterations += 1;
            std::hint::spin_loop();
            if iterations > 50_000_000 {
                // A guard so a broken cancellation fails the test instead of hanging it.
                return None;
            }
        }
        Some(iterations)
    });

    std::thread::sleep(Duration::from_millis(5));
    token.cancel();
    let ran = worker.join().expect("join");
    assert!(ran.is_some(), "the worker must observe the cancellation");
    assert!(token.is_cancelled());
}

#[test]
fn a_dropped_cancellation_future_leaves_the_token_usable() {
    let token = CancellationToken::new();
    for _ in 0..10_000 {
        // Each future registers on drop-park and must deregister, or the token's waiter list
        // grows without bound.
        let future = token.cancelled();
        drop(future);
    }
    // The token still works after all that churn.
    token.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn a_full_dispatch_loop_keeps_registry_and_channel_consistent() {
    // The integration that matters: requests flowing through a bounded queue to a registry,
    // under all three backpressure policies. A `Block` queue with no consumer blocks forever —
    // that is what blocking means — so a real consumer runs alongside the producer.
    //
    // Every request must reach exactly one terminal outcome, and the registry must never lose
    // or double-count one. What differs per policy is *how* the extra work is resolved:
    // `Reject` never queues it, `DropOldest` discards it, and in both cases the request must
    // still be released, or it leaks a registry slot until its deadline.
    const REQUESTS: usize = 64;
    for policy in [
        BackpressurePolicy::Block,
        BackpressurePolicy::DropOldest,
        BackpressurePolicy::Reject,
    ] {
        let registry = Arc::new(RequestRegistry::new(REQUESTS * 2, 60_000));
        let channel =
            Arc::new(BoundedChannel::<Message>::named("dispatch", 8, policy).expect("channel"));
        let correlation = CorrelationId::new();
        let terminal = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let consumer = {
            let registry = Arc::clone(&registry);
            let channel = Arc::clone(&channel);
            let terminal = Arc::clone(&terminal);
            std::thread::spawn(move || {
                loop {
                    let outcome = channel.recv_timeout(Duration::from_millis(50));
                    let Some(message) = (match outcome {
                        Ok(Some(message)) => Some(message),
                        // Closed and drained: shutdown.
                        Ok(None) => return,
                        // Timed out: the producer may simply be between sends.
                        Err(e) if matches!(e.kind(), ErrorKind::Timeout { .. }) => None,
                        // Any other failure is a real defect, not a timing artefact.
                        Err(e) => panic!("unexpected receive failure: {e}"),
                    }) else {
                        continue;
                    };
                    let id = message.request_id().expect("a request carries its id");
                    registry.complete(id).expect("complete");
                    terminal.fetch_add(1, Ordering::SeqCst);
                }
            })
        };

        for _ in 0..REQUESTS {
            let request_id = RequestId::new();
            registry
                .register(request_id, correlation)
                .expect("register");
            let message = Message::Request {
                method: "window.list".to_owned(),
                params: Vec::new(),
                correlation,
                request_id,
                deadline_ms: 60_000,
            };
            if let Err(e) = channel.send(message) {
                assert!(
                    matches!(e.kind(), ErrorKind::Backpressure { .. }),
                    "unexpected send failure: {e}"
                );
                // Work the queue refused must not stay registered.
                registry
                    .cancel(request_id)
                    .expect("cancel the refused request");
                terminal.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Close first, so the consumer drains everything the channel accepted and stops. Only
        // then is the registry reconciled: expiring while the consumer is still completing
        // would race it, and a request could be expired and then completed.
        channel.close();
        consumer
            .join()
            .expect("consumer must exit once the channel closes");

        // Whatever the policy discarded or refused to deliver is still registered. Expiry is
        // the safety net that releases it, which is exactly why the registry has deadlines.
        let leaked = registry.expire(u64::MAX);
        terminal.fetch_add(leaked.len(), Ordering::SeqCst);

        assert_eq!(
            terminal.load(Ordering::SeqCst),
            REQUESTS,
            "{policy:?}: every request needs exactly one terminal outcome"
        );
        assert_eq!(
            registry.pending_count(),
            0,
            "{policy:?}: the registry must drain"
        );
    }
}

#[test]
fn the_defaults_are_a_coherent_configuration_for_the_ipc_layer() {
    // The limits the crate reads must be valid on their own; an invalid default would only
    // surface when a message was actually sent.
    Limits::default().validate().expect("default limits");
    let registry = RequestRegistry::from_limits(&Limits::default());
    assert!(registry.max_in_flight() > 0);
    assert!(registry.request_timeout_ms() > 0);
    assert!(ncui_ipc::frame::FrameDecoder::new(Limits::default().max_ipc_frame_bytes).is_ok());
}
