//! Throughput benchmarks, measured with Criterion.
//!
//! Where `latency` answers "how long does one operation take at each percentile", this
//! answers "how many operations complete per second". Criterion is used here because its
//! statistical machinery — outlier detection and change detection against a saved
//! baseline — is exactly what a throughput gate needs, and reimplementing it would be
//! worse than using it.
//!
//! Run with `cargo bench -p ncui-bench --bench throughput`. Pass
//! `-- --save-baseline <name>` to record a baseline and `-- --baseline <name>` to compare.

use std::path::Path;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ncui_core::{Capability, CapabilitySet, CorrelationId, RequestId, SemVer};
use ncui_ipc::message::{Message, encode_messages};
use ncui_security::path::SafePath;
use ncui_state::patch::{Change, Patch};
use ncui_state::store::Store;

const MAX_FRAME: u32 = 16 * 1024 * 1024;

fn core_operations(c: &mut Criterion) {
    let all = CapabilitySet::all();

    c.bench_function("core/semver_parse", |b| {
        b.iter(|| SemVer::parse("1.2.3").is_ok());
    });

    c.bench_function("core/capability_subset_check", |b| {
        b.iter(|| all.is_subset_of(all));
    });

    c.bench_function("core/capability_membership", |b| {
        b.iter(|| all.contains(Capability::Gpu));
    });
}

fn ipc_operations(c: &mut Criterion) {
    let request = Message::Request {
        method: "ncui.component.inspect".to_owned(),
        params: br#"{"id":"abc"}"#.to_vec(),
        correlation: CorrelationId::new(),
        request_id: RequestId::new(),
        deadline_ms: 30_000,
    };
    let batch: Vec<Message> = (0..64)
        .map(|_| Message::Event {
            topic: "ncui.state.patch".to_owned(),
            payload: vec![7u8; 256],
            correlation: CorrelationId::new(),
        })
        .collect();

    c.bench_function("ipc/message_encode", |b| {
        b.iter(|| request.encode(MAX_FRAME).is_ok());
    });

    c.bench_function("ipc/batch_encode_64_events", |b| {
        b.iter(|| encode_messages(&batch, MAX_FRAME).is_ok());
    });
}

fn state_operations(c: &mut Criterion) {
    c.bench_function("state/patch_coalesce_512", |b| {
        b.iter_batched(
            || {
                let patch: Patch<u64> = (0..512)
                    .map(|i| Change::upsert(format!("window.{i}"), i))
                    .collect();
                patch
            },
            |mut patch| patch.coalesce(),
            BatchSize::LargeInput,
        );
    });

    c.bench_function("state/store_insert_128", |b| {
        b.iter(|| {
            let mut store: Store<u64> = Store::new();
            for i in 0..128u64 {
                store.insert(format!("k{i}"), i);
            }
            store.len()
        });
    });
}

fn security_operations(c: &mut Criterion) {
    let root = std::env::temp_dir();
    let token = vec![0x5Au8; 32];

    c.bench_function("security/path_resolve_allowed", |b| {
        b.iter(|| SafePath::resolve(&root, "src/core/engine.rs").is_ok());
    });

    // The rejection path matters as much as the accept path: a traversal payload must be
    // refused as quickly as a legitimate one, or the guard is a cheap place to be slow.
    c.bench_function("security/path_resolve_traversal", |b| {
        b.iter(|| SafePath::resolve(Path::new("/root"), "../../etc/passwd").is_err());
    });

    c.bench_function("security/constant_time_eq", |b| {
        b.iter(|| ncui_security::token::constant_time_eq(&token, &token));
    });
}

criterion_group!(
    benches,
    core_operations,
    ipc_operations,
    state_operations,
    security_operations
);
criterion_main!(benches);
