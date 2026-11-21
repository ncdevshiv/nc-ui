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

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ncui_app::{AssetServer, Route, Served};
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

/// A benchmark that cannot set itself up is a broken benchmark, not a slow one: report why and
/// stop. This is the bench-files' idiom for setup that must abort loudly without `expect`.
fn fail(message: &str) -> ! {
    use std::io::Write as _;
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let _ = writeln!(out, "throughput benchmark failure: {message}");
    std::process::exit(1);
}

/// The binary data channel: a page `POST`ing a buffer to a route and reading it back.
///
/// This is A1's reframed target. The page bridge carries every payload as a JSON string, so a
/// large buffer is escaped on the way out and parsed on the way in — the runtime pays JSON's
/// cost twice. This sweep measures the binary channel's round-trip throughput at each payload
/// size, against a real listener over a real socket, so the number is a transport measurement
/// rather than an encoding estimate. It exists to show where the binary channel wins: the
/// crossover is the payload size past which moving a buffer raw beats moving it as JSON.
fn binary_channel_sweep(c: &mut Criterion) {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // A scratch root the server serves, and one echo route that hands a POST body straight
    // back. The echo isolates the transport: whatever the round trip costs is the channel,
    // not any handler work.
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let root = std::env::temp_dir().join(format!(
        "ncui-bench-channel-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root)
        .unwrap_or_else(|error| fail(&format!("the scratch root could not be created: {error}")));
    let server = AssetServer::start_with(
        &root,
        vec![Route::new(
            "/__ncui/transfer",
            Arc::new(|_target: &str, body: &[u8]| {
                Some(Served {
                    content_type: "application/octet-stream",
                    body: body.to_vec(),
                })
            }),
        )],
    )
    .unwrap_or_else(|error| fail(&format!("the server would not start: {error}")));

    let mut group = c.benchmark_group("binary_channel/round_trip");
    for size in [1024, 4096, 16_384, 65_536, 262_144, 1_048_576] {
        let payload = vec![0xA5u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &payload, |b, payload| {
            b.iter_batched(
                || {
                    TcpStream::connect(server.local_addr())
                        .unwrap_or_else(|error| fail(&format!("the server refused: {error}")))
                },
                |mut stream| {
                    let head = format!(
                        "POST /__ncui/transfer HTTP/1.1\r\nHost: localhost\r\n\
                         Content-Length: {}\r\n\r\n",
                        payload.len()
                    );
                    stream
                        .write_all(head.as_bytes())
                        .unwrap_or_else(|error| fail(&format!("the head would not send: {error}")));
                    stream
                        .write_all(payload)
                        .unwrap_or_else(|error| fail(&format!("the body would not send: {error}")));
                    let mut raw = Vec::with_capacity(payload.len());
                    stream
                        .read_to_end(&mut raw)
                        .unwrap_or_else(|error| fail(&format!("the echo would not read: {error}")));
                    raw
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
    let _ = std::fs::remove_dir_all(&root);
}

/// The cost of deciding a locale's text direction.
///
/// C4's non-GPU half derives the direction from the locale on every `describe` and every GPU
/// island's accessible description. This measures that derivation so the claim "it adds nothing"
/// is a number rather than an assertion: the lookup is a language-subtag match against a fixed
/// set, and it is on the describe/island path, so it had better be nanoseconds.
fn locale_derivation(c: &mut Criterion) {
    use ncui_a11y::TextDirection;
    c.bench_function("locale/direction_for_locale", |b| {
        b.iter(|| TextDirection::for_locale("he-IL"));
    });
    c.bench_function("locale/direction_for_ltr_locale", |b| {
        b.iter(|| TextDirection::for_locale("en-US"));
    });
}

/// The per-row cost of deciding a GPU island's text layout: how many wrapped lines fit the row,
/// and where the ellipsis goes when they do not.
///
/// This is the C4 clipped-row fix. Every row of every island frame now calls `fit_lines` — one
/// float divide and a floor — where before it computed the block height with a multiply, so the
/// common case (a block that fits) should be unchanged; and a block that overflows additionally
/// shapes a one-character ellipsis and truncates the last line. Both paths are measured here over
/// synthetic lines, so the number is the layout decision's own cost and not the GPU render around
/// it.
fn island_row_elision(c: &mut Criterion) {
    use ncui_gpu_renderer::text::{ShapedGlyph, ShapedLine, ShapedText, elide_line, fit_lines};

    // A line sixteen tall of `count` glyphs that each advance eight: the unit of a wrapped block.
    let line = |count: usize| ShapedLine {
        glyphs: (0..count)
            .map(|index| ShapedGlyph {
                glyph_id: u16::try_from(index).unwrap_or(0),
                cluster: u32::try_from(index).unwrap_or(0),
                x_advance: 8.0,
                x_offset: 0.0,
                y_offset: 0.0,
            })
            .collect(),
        width: count as f32 * 8.0,
        ascent: 16.0,
        descent: 0.0,
    };

    // A block that fits its row: three lines in a row sixty-four tall. This is the common case,
    // the one every row that is not overflowing pays for.
    let fitting: Vec<ShapedLine> = (0..3).map(|_| line(4)).collect();
    c.bench_function("island/fit_lines_block_fits", |b| {
        b.iter(|| fit_lines(&fitting, 64.0));
    });

    // A block that overflows: eight lines in a row forty tall, so lines are left out and the last
    // is marked. This is the path the fix added.
    let overflowing: Vec<ShapedLine> = (0..8).map(|_| line(4)).collect();
    c.bench_function("island/fit_lines_block_overflows", |b| {
        b.iter(|| fit_lines(&overflowing, 40.0));
    });

    // Truncating the last visible line to make room for the marker, on a full line that must give
    // glyphs back.
    let ellipsis = ShapedText {
        glyphs: vec![ShapedGlyph {
            glyph_id: 0,
            cluster: 0,
            x_advance: 20.0,
            x_offset: 0.0,
            y_offset: 0.0,
        }],
        width: 20.0,
        ascent: 0.0,
        descent: 0.0,
    };
    c.bench_function("island/elide_line_to_make_room", |b| {
        b.iter_batched(
            || line(10),
            |mut last| elide_line(&mut last, &ellipsis, 100.0),
            BatchSize::SmallInput,
        );
    });
}

/// The cost of splitting a line into bidirectional runs before shaping it.
///
/// C4's GPU text path now segments every line into single-direction runs so an embedded
/// right-to-left word is not drawn backwards. `bidi_runs` is that split, and it runs on every
/// shaped line, so the plain left-to-right case — the overwhelmingly common one — had better be
/// cheap. Measured against a left-to-right line and a mixed-direction line so the cost of the
/// segmentation and of the extra shaper calls is visible against the line that needs none of it.
fn bidi_segmentation(c: &mut Criterion) {
    use ncui_gpu_renderer::text::bidi_runs;
    let left_to_right = "the quick brown fox jumps over the lazy dog";
    let mixed = "the price is 12 שקלים and the total is 34 דולר";

    c.bench_function("bidi/split_left_to_right_line", |b| {
        b.iter(|| bidi_runs(left_to_right));
    });
    c.bench_function("bidi/split_mixed_direction_line", |b| {
        b.iter(|| bidi_runs(mixed));
    });
}

criterion_group!(
    benches,
    core_operations,
    ipc_operations,
    state_operations,
    security_operations,
    binary_channel_sweep,
    locale_derivation,
    island_row_elision,
    bidi_segmentation
);
criterion_main!(benches);
