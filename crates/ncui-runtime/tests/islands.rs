//! A page's island, over the wire, on a real GPU.
//!
//! What this proves is the join ADR-002 asks for: the runtime registers a route on the page's own
//! origin, a request for a frame is answered with pixels the GPU drew, and the parts of the frame the
//! island does not cover are transparent — which is what lets the page show through around it rather
//! than having a rectangle pasted over the document.
//!
//! These tests need a graphics adapter. When there is none — a CI runner with no GPU, a container
//! without a device — they report that they were skipped rather than failing, because "this machine
//! has no GPU" is a fact about the machine, not a defect in the runtime.

#![cfg(feature = "gpu-islands")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::task::{Context, Poll};

use ncui_app::{AssetServer, Route};
use ncui_gpu_renderer::device::{DeviceFactory, WgpuDeviceFactory};

/// A directory that is removed when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// A directory holding one page.
    fn with_page() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-islands-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the directory is created");
        std::fs::write(path.join("index.html"), "<p>a page</p>").expect("the page is written");
        Self { path }
    }

    /// The directory itself.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Drives a future to completion on the current thread.
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Whether this machine has a graphics adapter, and so can answer an island request at all.
fn has_gpu() -> bool {
    block_on(WgpuDeviceFactory.create()).is_ok()
}

/// The status, content type and body of a GET.
fn get(server: &AssetServer, target: &str) -> (String, Option<String>, Vec<u8>) {
    let mut stream = TcpStream::connect(server.local_addr()).expect("the server accepts");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .expect("the timeout is set");
    let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the response is read");

    let boundary = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has a head and a body");
    let head = String::from_utf8_lossy(&raw[..boundary]).into_owned();
    let body = raw[boundary + 4..].to_vec();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let content_type = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.trim().to_owned());
    (status, content_type, body)
}

/// A server with the island route registered, and its scratch directory.
fn server() -> (AssetServer, Scratch) {
    let scratch = Scratch::with_page();
    let route: Route = ncui_runtime::islands::Islands::new().route();
    let server = AssetServer::start_with(scratch.path(), vec![route]).expect("the server starts");
    (server, scratch)
}

#[test]
fn a_page_fetches_an_island_and_gets_the_pixels_the_gpu_drew() {
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    let (status, content_type, body) = get(
        &server,
        "/__ncui/island?w=32&h=16&x=4&y=4&iw=16&ih=8&colour=ff8000ff",
    );
    assert_eq!(status, "200");
    assert_eq!(
        content_type.as_deref(),
        Some("application/octet-stream"),
        "the frame is bytes, and a page wraps them in an ImageData"
    );
    assert_eq!(
        body.len(),
        32 * 16 * 4,
        "a whole frame of four bytes a pixel"
    );

    // The middle of the island is the island's colour.
    let middle = ((8 * 32) + 12) * 4;
    assert_eq!(
        &body[middle..middle + 4],
        &[255, 128, 0, 255],
        "the colour the request named must be where the island was"
    );
    // A corner the island does not cover is transparent, so the page shows through there.
    assert_eq!(
        &body[0..4],
        &[0, 0, 0, 0],
        "everything outside the island must be transparent rather than black"
    );
}

#[test]
fn a_request_that_does_not_describe_a_frame_is_not_a_frame() {
    // Each of these is a request the file server answers, because there is no file there and there is
    // no island either. A page that asked wrongly learns it asked wrongly rather than receiving a
    // frame of the wrong shape.
    let (server, _scratch) = server();
    for target in [
        // No query at all.
        "/__ncui/island",
        // Missing a field: there is no size to draw at.
        "/__ncui/island?w=32&h=16&x=4&y=4&iw=16&ih=8",
        // No colour.
        "/__ncui/island?w=32&h=16&x=4&y=4&iw=16&ih=8&colour=",
        // A colour that is not four channels.
        "/__ncui/island?w=32&h=16&x=4&y=4&iw=16&ih=8&colour=ff80",
        // Past the area bound.
        "/__ncui/island?w=8192&h=8192&x=0&y=0&iw=1&ih=1&colour=ff8000ff",
        // A side of zero.
        "/__ncui/island?w=0&h=16&x=0&y=0&iw=1&ih=1&colour=ff8000ff",
        // An island that starts outside the frame it is drawn in.
        "/__ncui/island?w=16&h=16&x=16&y=0&iw=8&ih=8&colour=ff8000ff",
        // An island with no size.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=0&ih=8&colour=ff8000ff",
        // A field nobody defined, which is how a typo is caught rather than ignored.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=8&ih=8&colour=ff8000ff&depth=4",
    ] {
        let (status, _, _) = get(&server, target);
        assert_eq!(status, "404", "`{target}` must not be answered as a frame");
    }
}

#[test]
fn the_rest_of_the_origin_is_still_the_application() {
    // The route is the runtime's namespace and nothing else: a page's own files are served exactly as
    // they were, which is what makes the island an addition rather than a change.
    let (server, _scratch) = server();
    let (status, content_type, body) = get(&server, "/index.html");
    assert_eq!(status, "200");
    assert_eq!(content_type.as_deref(), Some("text/html; charset=utf-8"));
    assert_eq!(body, b"<p>a page</p>");
}

/// How many pixels of a frame are darker than a light background.
/// Counts the pixels that carry ink: a pixel that is neither the row's own colour nor
/// transparent.
fn ink(body: &[u8]) -> usize {
    body.as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| {
            pixel[3] > 0 && u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2]) < 384
        })
        .count()
}

/// Counts the rows of the frame that carry ink, which is how tall the drawn content is.
fn ink_span(body: &[u8], width: usize) -> usize {
    body.chunks(width * 4)
        .filter(|row| {
            row.as_chunks::<4>().0.iter().any(|pixel| {
                pixel[3] > 0
                    && u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2]) < 384
            })
        })
        .count()
}

#[test]
fn a_row_with_text_is_drawn_with_its_letters() {
    // The defect this closes: a row was a rectangle, and a row that named text had no text on it.
    // The same island is asked for twice, once plain and once with a letter on it; the difference
    // between the two frames is the letter.
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    let plain = "/__ncui/island?w=64&h=32&x=0&y=0&iw=64&ih=32&rows=32:ffffffff";
    let (status, _, body) = get(&server, plain);
    assert_eq!(status, "200", "the plain island is served");
    assert_eq!(body.len(), 64 * 32 * 4, "a whole frame comes back");
    assert_eq!(ink(&body), 0, "a white row has no ink on it");

    // One row, 32 pixels tall, white, with a 24-pixel black "H".
    let lettered = "/__ncui/island?w=64&h=32&x=0&y=0&iw=64&ih=32&rows=32:ffffffff:000000ff:24:H";
    let (status, _, body) = get(&server, lettered);
    assert_eq!(status, "200", "the island with text is served");
    assert_eq!(body.len(), 64 * 32 * 4);
    let drawn = ink(&body);
    assert!(
        drawn > 20,
        "the letter must be drawn: {drawn} pixels of ink in a 64x32 frame"
    );
    // The letter occupies a fraction of the row, not the whole of it: a renderer that filled the
    // row would also have ink, and this is what tells the two apart.
    assert!(
        drawn < 64 * 32 / 2,
        "the letter must not fill the row: {drawn} pixels of ink"
    );

    // The row is white where the letter is not: the text is drawn over the row, not instead of it.
    let top_left = &body[0..4];
    assert_eq!(
        top_left,
        [255, 255, 255, 255],
        "the row's own colour is kept"
    );
}

#[test]
fn a_row_taller_than_one_line_wraps_its_text() {
    // The defect this closes: a row drew its text on one line, so a row tall enough for
    // several lines showed one and clipped the rest. The same island is asked for twice — once
    // with a text far wider than the row and once with a single letter — and the wrapped frame's
    // ink spans more of the row's height. That comparison is the measurement: a font's own
    // metrics differ between machines, but wrapped text always occupies more lines than one
    // letter.
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    let wrapped = "/__ncui/island?w=64&h=96&x=0&y=0&iw=64&ih=96&rows=96:ffffffff:000000ff:16:aaaa%20bbbb%20cccc%20dddd";
    let (status, _, wrapped_body) = get(&server, wrapped);
    assert_eq!(status, "200", "the wrapping row is served");
    let single = "/__ncui/island?w=64&h=96&x=0&y=0&iw=64&ih=96&rows=96:ffffffff:000000ff:16:H";
    let (status, _, single_body) = get(&server, single);
    assert_eq!(status, "200", "the single-letter row is served");

    let wrapped_span = ink_span(&wrapped_body, 64);
    let single_span = ink_span(&single_body, 64);
    assert!(
        wrapped_span > single_span * 2,
        "wrapped text must occupy more of the row than one line: {wrapped_span} rows against \
         {single_span}"
    );
}

#[test]
fn a_row_whose_text_is_not_encoded_is_refused() {
    // The text is percent-encoded because a comma separates rows and a colon separates fields. A
    // request that puts a comma in the text is a request whose rows cannot be read, and it is
    // refused rather than guessed at.
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    let broken = "/__ncui/island?w=32&h=32&x=0&y=0&iw=32&ih=32&rows=32:ffffffff:000000ff:16:two,words,32:ffffffff";
    let (status, _, _) = get(&server, broken);
    assert_eq!(
        status, "404",
        "a row list whose text carries a raw comma is not a row list"
    );
}

#[test]
fn a_page_asks_for_the_accessible_description_of_the_rows_it_draws() {
    // A canvas says nothing to a screen reader, and the page owns the DOM: the runtime owes the page
    // the island's content in a form it can mirror. This is that, and it comes from the same model
    // the renderer draws from, so what a reader is told cannot drift from what the page shows.
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    let target = "/__ncui/island?w=64&h=32&x=0&y=0&iw=64&ih=32\
                  &rows=16:ffffffff:000000ff:12:First,16:ffffffff:000000ff:12:Second\
                  &a11y=1&label=Notes";
    let (status, content_type, body) = get(&server, target);
    assert_eq!(status, "200", "the description is served");
    assert_eq!(
        content_type.as_deref(),
        Some("application/json"),
        "a description is JSON, not pixels"
    );
    let description: serde_json::Value =
        serde_json::from_slice(&body).expect("the description is JSON");
    assert_eq!(description["label"], "Notes");
    let rows = description["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2, "both visible rows are described");
    assert_eq!(rows[0]["text"], "First");
    assert_eq!(rows[1]["text"], "Second");
    // The places are the ones the renderer draws at: the rows are stacked from the island's top.
    assert_eq!(rows[0]["y"], 0.0);
    assert_eq!(rows[0]["height"], 16.0);
    assert_eq!(rows[1]["y"], 16.0);
    assert_eq!(rows[0]["width"], 64.0, "a row spans the island's width");
}

#[test]
fn a_description_with_no_label_is_not_a_description() {
    // The label is the one thing the page cannot infer: the runtime knows the rows, not what they
    // are for. A description with no label announces nothing, so the request is refused rather than
    // answered with something a reader cannot use.
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    for target in [
        // No label at all.
        "/__ncui/island?w=64&h=32&x=0&y=0&iw=64&ih=32&rows=16:ffffffff&a11y=1",
        // A label that is only spaces.
        "/__ncui/island?w=64&h=32&x=0&y=0&iw=64&ih=32&rows=16:ffffffff&a11y=1&label=%20%20",
        // A label without the description flag: a page that names an island it did not ask about.
        "/__ncui/island?w=64&h=32&x=0&y=0&iw=64&ih=32&rows=16:ffffffff&label=Notes",
        // A flag that is neither 1 nor absent.
        "/__ncui/island?w=64&h=32&x=0&y=0&iw=64&ih=32&rows=16:ffffffff&a11y=2&label=Notes",
    ] {
        let (status, _, _) = get(&server, target);
        assert_eq!(status, "404", "{target} must not be answered");
    }
}

#[test]
fn a_page_asks_for_rows_and_the_gpu_draws_them_where_the_model_says() {
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    // Three rows of different heights and colours, in a frame the island fills.
    let (status, _, body) = get(
        &server,
        "/__ncui/island?w=16&h=32&x=0&y=0&iw=16&ih=32&rows=8:ff0000ff,8:00ff00ff,16:0000ffff",
    );
    assert_eq!(status, "200");
    assert_eq!(body.len(), 16 * 32 * 4);

    // Each row is where its height puts it, and is the colour the page gave it.
    let at = |y: usize| {
        let index = ((y * 16) + 8) * 4;
        [
            body[index],
            body[index + 1],
            body[index + 2],
            body[index + 3],
        ]
    };
    assert_eq!(
        at(4),
        [255, 0, 0, 255],
        "the first row is the first eight pixels"
    );
    assert_eq!(at(12), [0, 255, 0, 255], "the second row follows it");
    assert_eq!(at(20), [0, 0, 255, 255], "the third is twice as tall");
    assert_eq!(
        at(31),
        [0, 0, 255, 255],
        "and reaches the bottom of the island"
    );
}

#[test]
fn scrolling_the_rows_moves_which_ones_are_drawn() {
    if !has_gpu() {
        return;
    }
    let (server, _scratch) = server();
    // The same three rows in an island half as tall as its content, scrolled past the first: what is
    // drawn starts at the second row, and the frame below the island is the page's, not the island's.
    let (status, _, body) = get(
        &server,
        "/__ncui/island?w=16&h=32&x=0&y=0&iw=16&ih=16&rows=8:ff0000ff,8:00ff00ff,16:0000ffff&scroll=8",
    );
    assert_eq!(status, "200");
    let at = |y: usize| {
        let index = ((y * 16) + 8) * 4;
        [
            body[index],
            body[index + 1],
            body[index + 2],
            body[index + 3],
        ]
    };
    assert_eq!(
        at(4),
        [0, 255, 0, 255],
        "the scrolled-past row is not drawn"
    );
    assert_eq!(
        at(12),
        [0, 0, 255, 255],
        "the last row has moved up by the scroll"
    );
    assert_eq!(
        at(20),
        [0, 0, 0, 0],
        "the frame below the island belongs to the page, so it is transparent"
    );
}

#[test]
fn a_request_that_describes_two_contents_or_none_is_not_a_frame() {
    let (server, _scratch) = server();
    for target in [
        // Both a colour and rows: the renderer would have to choose.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=16&ih=16&colour=ff0000ff&rows=8:ff0000ff",
        // A row of no height is not a row.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=16&ih=16&rows=0:ff0000ff",
        // A row taller than any row may be.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=16&ih=16&rows=99999:ff0000ff",
        // A row whose colour is not a colour.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=16&ih=16&rows=8:red",
        // A row with no colour at all.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=16&ih=16&rows=8",
        // A scroll with nothing to scroll.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=16&ih=16&scroll=8",
        // An empty row list.
        "/__ncui/island?w=16&h=16&x=0&y=0&iw=16&ih=16&rows=",
    ] {
        let (status, _, _) = get(&server, target);
        assert_eq!(status, "404", "`{target}` must not be answered as a frame");
    }
}
