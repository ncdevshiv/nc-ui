//! The asset server, exercised over a real socket.
//!
//! These tests open a real TCP connection to a real listener and speak HTTP to it. The
//! point is that the server is a server: it binds, it accepts, it parses, it answers, and
//! it closes. A test that called the server's internals would pass even if the server
//! could not be reached at all, which is the failure that matters most here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use ncui_app::{AssetServer, Route, Served};

/// A directory that is removed when the test ends.
///
/// The name carries the process id and a counter so two tests running in parallel cannot
/// collide on the same directory.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Creates a directory with the given files in it.
    fn with_files(files: &[(&str, &str)]) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "ncui-app-assets-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch directory is created");
        for (name, contents) in files {
            let target = path.join(name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).expect("the parent directory is created");
            }
            std::fs::write(target, contents).expect("the fixture file is written");
        }
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

/// One HTTP response, read off the wire.
struct Reply {
    status: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    /// The value of a header, if it is present.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Sends a request to the server and reads the whole answer.
fn request(server: &AssetServer, method: &str, path: &str) -> Reply {
    request_with_headers(server, method, path, &[])
}

/// Sends a request with extra headers and reads the whole answer.
fn request_with_headers(
    server: &AssetServer,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Reply {
    let mut stream = TcpStream::connect(server.local_addr()).expect("the server accepts");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("the timeout is set");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (name, value) in headers {
        use std::fmt::Write as _;
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("the request is written");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("the response is read");
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("the response has a head and a body");
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_owned(),
    }
}

#[test]
fn a_file_is_served_with_its_content_type() {
    let scratch = Scratch::with_files(&[
        ("index.html", "<!doctype html><title>Hello</title>"),
        ("app.js", "export const answer = 42;"),
    ]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");

    let page = request(&server, "GET", "/index.html");
    assert_eq!(page.status, "200");
    assert_eq!(
        page.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(page.body, "<!doctype html><title>Hello</title>");
    assert_eq!(
        page.header("Content-Length").map(str::to_owned),
        Some(page.body.len().to_string()),
        "the declared length must be the length of the body that was sent"
    );
    assert_eq!(page.header("Connection"), Some("close"));

    let script = request(&server, "GET", "/app.js");
    assert_eq!(script.status, "200");
    assert_eq!(
        script.header("Content-Type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(script.body, "export const answer = 42;");
}

#[test]
fn a_file_in_a_subdirectory_is_served() {
    let scratch = Scratch::with_files(&[("pages/deep/page.html", "<p>deep</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    let page = request(&server, "GET", "/pages/deep/page.html");
    assert_eq!(page.status, "200");
    assert_eq!(page.body, "<p>deep</p>");
}

#[test]
fn a_percent_encoded_path_reaches_the_file_it_names() {
    let scratch = Scratch::with_files(&[("my page.html", "<p>spaced</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    let page = request(&server, "GET", "/my%20page.html");
    assert_eq!(page.status, "200");
    assert_eq!(page.body, "<p>spaced</p>");
}

#[test]
fn a_file_that_does_not_exist_is_refused() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    let page = request(&server, "GET", "/missing.html");
    assert_eq!(page.status, "404");
}

#[test]
fn a_path_that_leaves_the_root_is_refused() {
    // The classic traversal, in both the plain and the encoded spelling.
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    for path in [
        "/../secret.txt",
        "/../../secret.txt",
        "/%2e%2e/secret.txt",
        "/..%2fsecret.txt",
    ] {
        let page = request(&server, "GET", path);
        assert!(
            page.status == "403" || page.status == "404",
            "{path} produced {}, which is neither a refusal nor a not-found",
            page.status
        );
        assert!(
            !page.body.contains("secret"),
            "{path} returned the file outside the root"
        );
    }
}

#[test]
fn a_directory_is_not_listed() {
    let scratch = Scratch::with_files(&[("pages/page.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    let page = request(&server, "GET", "/pages");
    assert_eq!(page.status, "404");
    assert!(
        !page.body.contains("page.html"),
        "a directory listing would tell a client what the application holds"
    );
}

#[test]
fn a_method_the_server_does_not_serve_is_refused() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    for method in ["POST", "PUT", "DELETE", "OPTIONS"] {
        let page = request(&server, method, "/index.html");
        assert_eq!(page.status, "405", "{method} was not refused");
    }
}

#[test]
fn a_head_request_returns_the_length_without_the_body() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    let page = request(&server, "HEAD", "/index.html");
    assert_eq!(page.status, "200");
    assert_eq!(page.header("Content-Length"), Some("11"));
    assert!(
        page.body.is_empty(),
        "a HEAD response must not carry the body; got: {:?}",
        page.body
    );
}

#[test]
fn a_request_with_no_path_is_refused() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    let page = request(&server, "GET", "/");
    assert_eq!(page.status, "404");
}

#[test]
fn the_server_binds_the_loopback_interface_only() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let address = "0.0.0.0:0".parse().expect("the address parses");
    let error =
        AssetServer::start_on(scratch.path(), address).expect_err("a public bind is refused");
    assert!(
        error.to_string().contains("loopback"),
        "the refusal must say why; got: {error}"
    );
}

#[test]
fn the_server_reports_the_origin_it_serves_on() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");
    assert!(server.origin().starts_with("http://127.0.0.1:"));
    assert_eq!(
        server.url_for("pages/a b.html"),
        format!("{}/pages/a%20b.html", server.origin())
    );
}

#[test]
fn a_root_that_does_not_exist_is_refused_at_startup() {
    let missing = std::env::temp_dir().join("ncui-app-missing-root-that-does-not-exist");
    let _ = std::fs::remove_dir_all(&missing);
    let error = AssetServer::start(&missing).expect_err("a missing root is refused");
    assert!(error.kind() == std::io::ErrorKind::NotFound, "got: {error}");
}

/// Sends a request and reads the answer with its body as bytes.
///
/// Separate from `request` because a route may answer with pixels, which are not text: reading them
/// as a `String` would replace every byte that is not valid UTF-8 and the assertion would then be
/// about the lossy conversion rather than about the body.
fn request_bytes(server: &AssetServer, path: &str) -> (String, Option<String>, Vec<u8>) {
    let mut stream = TcpStream::connect(server.local_addr()).expect("the server accepts");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("the timeout is set");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
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

#[test]
fn a_route_answers_a_path_the_runtime_serves_itself() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    // The responder is asked with the target as it arrived, query string and all: a route's
    // parameters are its own business, and a route whose parameters had been interpreted by the file
    // server first would be a route the file server had an opinion about.
    let asked = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&asked);
    let server = AssetServer::start_with(
        scratch.path(),
        vec![Route::new(
            "/__ncui/island",
            Arc::new(move |target: &str| {
                seen.fetch_add(1, Ordering::SeqCst);
                Some(Served {
                    content_type: "application/octet-stream",
                    body: target.as_bytes().to_vec(),
                })
            }),
        )],
    )
    .expect("the server starts");

    let (status, content_type, body) = request_bytes(&server, "/__ncui/island?w=2&h=1");
    assert_eq!(status, "200");
    assert_eq!(content_type.as_deref(), Some("application/octet-stream"));
    assert_eq!(
        String::from_utf8_lossy(&body),
        "/__ncui/island?w=2&h=1",
        "the responder must be handed the whole target"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[test]
fn a_route_wins_over_a_file_at_the_same_path() {
    // The runtime's namespace is the runtime's. An application that happens to ship a file at the
    // same path must not be able to answer a request the runtime was going to serve itself — that is
    // the property that makes a route a route rather than a fallback.
    let scratch = Scratch::with_files(&[
        ("index.html", "<p>here</p>"),
        ("__ncui/island", "a file the application shipped"),
    ]);
    let server = AssetServer::start_with(
        scratch.path(),
        vec![Route::new(
            "/__ncui/island",
            Arc::new(|_target: &str| {
                Some(Served {
                    content_type: "application/octet-stream",
                    body: b"from the runtime".to_vec(),
                })
            }),
        )],
    )
    .expect("the server starts");

    let (status, _, body) = request_bytes(&server, "/__ncui/island");
    assert_eq!(status, "200");
    assert_eq!(body, b"from the runtime");
}

#[test]
fn a_path_a_route_does_not_claim_is_still_served_from_the_filesystem() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let asked = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&asked);
    let server = AssetServer::start_with(
        scratch.path(),
        vec![Route::new(
            "/__ncui/island",
            Arc::new(move |_target: &str| {
                seen.fetch_add(1, Ordering::SeqCst);
                Some(Served {
                    content_type: "application/octet-stream",
                    body: Vec::new(),
                })
            }),
        )],
    )
    .expect("the server starts");

    let page = request(&server, "GET", "/index.html");
    assert_eq!(page.status, "200");
    assert_eq!(page.body, "<p>here</p>");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "a responder must not be asked about a path that is not its route"
    );

    // A path that begins with the same characters but is not under the prefix is not the route
    // either: `/__ncui/islanding` is a different path from `/__ncui/island`.
    let _ = request_bytes(&server, "/__ncui/islanding");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
}

#[test]
fn a_full_get_serves_the_file_with_a_stable_etag() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");

    let first = request(&server, "GET", "/index.html");
    assert_eq!(first.status, "200");
    let etag = first
        .header("ETag")
        .expect("a successful file response carries a validator")
        .to_owned();
    // Strong form: "<mtime_ns>-<len>", len = body length.
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    let len_suffix = format!("-{}\"", first.body.len());
    assert!(
        etag.ends_with(&len_suffix),
        "the validator must end with the served length: {etag}"
    );

    // Stable while the file is untouched.
    let second = request(&server, "GET", "/index.html");
    assert_eq!(second.status, "200");
    assert_eq!(second.header("ETag"), Some(etag.as_str()));

    // HEAD carries the same validator without the body.
    let head = request(&server, "HEAD", "/index.html");
    assert_eq!(head.status, "200");
    assert_eq!(head.header("ETag"), Some(etag.as_str()));
    assert!(head.body.is_empty());
}

#[test]
fn a_conditional_get_with_a_matching_etag_is_304_with_no_body() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");

    let full = request(&server, "GET", "/index.html");
    let etag = full
        .header("ETag")
        .expect("a successful file response carries a validator")
        .to_owned();

    let cached = request_with_headers(&server, "GET", "/index.html", &[("If-None-Match", &etag)]);
    assert_eq!(cached.status, "304");
    assert!(cached.body.is_empty(), "a 304 must not carry a body");
    assert_eq!(cached.header("Content-Length"), Some("0"));
    assert_eq!(cached.header("ETag"), Some(etag.as_str()));

    // Weak comparison is permitted by RFC 9110: W/"…" must also match.
    let weak = format!("W/{etag}");
    let weak_cached =
        request_with_headers(&server, "GET", "/index.html", &[("If-None-Match", &weak)]);
    assert_eq!(weak_cached.status, "304");

    // HEAD parity: the same condition yields 304.
    let head_cached =
        request_with_headers(&server, "HEAD", "/index.html", &[("If-None-Match", &etag)]);
    assert_eq!(head_cached.status, "304");
    assert!(head_cached.body.is_empty());
}

#[test]
fn a_conditional_get_with_a_stale_etag_is_200_with_the_body() {
    let scratch = Scratch::with_files(&[("index.html", "<p>here</p>")]);
    let server = AssetServer::start(scratch.path()).expect("the server starts");

    let full = request(&server, "GET", "/index.html");
    let etag = full
        .header("ETag")
        .expect("a successful file response carries a validator")
        .to_owned();
    assert!(!etag.contains("999999"), "test premise: {etag}");

    let stale = request_with_headers(
        &server,
        "GET",
        "/index.html",
        &[("If-None-Match", "\"999999-11\"")],
    );
    assert_eq!(stale.status, "200");
    assert_eq!(stale.body, "<p>here</p>");
    assert_eq!(stale.header("ETag"), Some(etag.as_str()));
}
