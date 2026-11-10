//! The asset server: a real HTTP/1.1 server that hands an application its own files.
//!
//! A web application is served. The browser will not open `file://` URLs for a page that
//! loads a module, will not let a page fetch its own siblings, and will not run a page that
//! was never given an origin. So NC-UI runs the application's assets over loopback HTTP —
//! the same shape every web framework has, with the difference that the server binds to the
//! loopback interface and nothing else.
//!
//! # What it is and is not
//!
//! It is a *file* server for one directory tree, and it is complete as one: it speaks
//! HTTP/1.1, it sets the content type from the file's extension, it answers `GET` and
//! `HEAD`, it refuses everything else, it closes the connection after each response, and
//! it bounds how much it will read from a client before it gives up.
//!
//! It is not a development server with a watcher, a bundler, a proxy, or a virtual file
//! system. Those are separate tools with separate jobs, and pretending one server is all
//! four is how a tool ends up being none of them.
//!
//! # The containment rule
//!
//! A request names a path; the path is resolved against the root by
//! [`ncui_security::path::SafePath`], which refuses traversal, absolute paths, and
//! drive-qualified paths. The resolved path is then *canonicalized* and checked again,
//! which closes the gap `SafePath` documents: a symlink planted inside the root can point
//! outside it, and only a second check against the canonical path sees that.
//!
//! # The loopback rule
//!
//! The listener binds `127.0.0.1` and nothing else. It is not configurable to bind
//! elsewhere, because the only thing this server is for is serving one application to one
//! window on this machine, and a server that can be pointed at a network interface is a
//! server that will be.

use core::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// The largest request head the server will read, in bytes.
///
/// A request head is the request line and the headers. 16 KiB is far more than any real
/// request needs and small enough that a client cannot make the server allocate without
/// bound.
pub const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

/// How long the accept loop waits for its wake before re-checking the shutdown flag.
///
/// This is a safety net, not the shutdown path: [`AssetServer::stop`] sends on the wake
/// channel, which ends the wait at once. The timeout is what guarantees the loop still
/// terminates if that send is ever lost. It is deliberately long — the loop used to poll
/// every 2 ms, and an idle application measured 0.94 % of a core over a ten-second sample
/// doing it, against 0.00 % for runtimes whose accept blocks in the kernel.
const SHUTDOWN_POLL: Duration = Duration::from_millis(250);

/// How long the server waits for a client to send its request head.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the server waits for a client to accept its response.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The port the server binds when none is asked for.
///
/// Zero means "any free port", which is what a development server wants: the port is
/// printed and the window is opened at it.
pub const DEFAULT_PORT: u16 = 0;

/// A body the runtime produced rather than the application, and what it is.
///
/// The media type is `'static` on purpose: a responder answers with a type the runtime knows at
/// compile time, so a page is never handed a type a request chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The media type.
    pub content_type: &'static str,
    /// The bytes.
    pub body: Vec<u8>,
}

/// Answers a request the runtime serves itself.
///
/// `None` means "not mine", which is how a responder declines a path rather than answering it with
/// an empty body — an empty body is a 200, and a page cannot tell it from a real one.
pub type Responder = Arc<dyn Fn(&str) -> Option<Served> + Send + Sync>;

/// One path prefix the runtime serves itself.
///
/// The page already has an origin — the loopback server serving the application — so a route is what
/// lets the runtime answer under *that* origin. A page therefore fetches a produced body the same way
/// it fetches its own files, with no per-platform URL scheme and no second server to trust.
#[derive(Clone)]
pub struct Route {
    prefix: &'static str,
    responder: Responder,
}

impl Route {
    /// A route answering every target whose path begins with `prefix`.
    ///
    /// The prefix is checked against the path alone, so a query string cannot be used to reach a
    /// route the path does not name.
    #[must_use]
    pub fn new(prefix: &'static str, responder: Responder) -> Self {
        Self { prefix, responder }
    }

    /// The body for `target`, when the target is this route's.
    fn answer(&self, target: &str) -> Option<Served> {
        let path = target.split('?').next().unwrap_or(target);
        if path != self.prefix && !path.starts_with(&format!("{}/", self.prefix)) {
            return None;
        }
        (self.responder)(target)
    }
}

impl fmt::Debug for Route {
    /// Reports the prefix rather than the closure, which has no useful representation and whose
    /// address would change between runs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// A server that hands an application's files to a page.
///
/// The server owns a listener on the loopback interface and a thread that answers
/// requests. It is stopped explicitly, because a server that only stops when the process
/// exits cannot be restarted inside a long-lived process, and a test cannot clean up after
/// itself.
#[derive(Debug)]
pub struct AssetServer {
    root: PathBuf,
    address: SocketAddr,
    shutdown: Arc<AtomicBool>,
    /// How many responses the server has sent.
    ///
    /// Counted rather than inferred, because "the application is serving" is a claim an
    /// operator needs to be able to check.
    served: Arc<AtomicU64>,
    /// Wakes the accept loop so a stop is immediate rather than a poll interval away.
    wake: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl AssetServer {
    /// Starts a server serving `root`.
    ///
    /// The root is canonicalized once, at startup. Doing it here means every later
    /// containment check compares against the same absolute path, and a root that does not
    /// exist is reported now rather than on the first request.
    ///
    /// # Errors
    ///
    /// Returns an error when the root does not exist, is not a directory, or cannot be
    /// canonicalized, and when the listener cannot be bound.
    pub fn start(root: &Path) -> io::Result<Self> {
        Self::start_on(root, SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)))
    }

    /// Starts a server serving `root`, with `routes` answered by the runtime itself.
    ///
    /// A route is asked before the filesystem, so the runtime's namespace cannot be shadowed by a
    /// file an application happens to ship at the same path — which is the property that makes a
    /// route the runtime's rather than the application's.
    ///
    /// # Errors
    ///
    /// The same errors [`AssetServer::start`] reports.
    pub fn start_with(root: &Path, routes: Vec<Route>) -> io::Result<Self> {
        Self::start_on_with(
            root,
            SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
            routes,
        )
    }

    /// Starts a server serving `root`, bound to `address`.
    ///
    /// The address must be on the loopback interface. A server that could be pointed at a
    /// network interface would be a server that eventually is.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be used, when the address is not a loopback
    /// address, or when the listener cannot be bound.
    pub fn start_on(root: &Path, address: SocketAddr) -> io::Result<Self> {
        Self::start_on_with(root, address, Vec::new())
    }

    /// Starts a server serving `root` at `address`, with `routes` answered by the runtime itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be used, when the address is not a loopback address,
    /// or when the listener cannot be bound.
    pub fn start_on_with(root: &Path, address: SocketAddr, routes: Vec<Route>) -> io::Result<Self> {
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the asset server binds the loopback interface only; {address} is not a \
                     loopback address"
                ),
            ));
        }
        let canonical = root.canonicalize()?;
        if !canonical.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", canonical.display()),
            ));
        }
        let listener = TcpListener::bind(address)?;
        let bound = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let served = Arc::new(AtomicU64::new(0));
        let routes = Arc::new(routes);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        let worker = {
            let shutdown = Arc::clone(&shutdown);
            let served = Arc::clone(&served);
            let canonical = canonical.clone();
            let routes = Arc::clone(&routes);
            std::thread::Builder::new()
                .name("ncui-asset-server".to_owned())
                .spawn(move || {
                    accept_loop(&listener, &canonical, &shutdown, &served, &routes, &wake_rx);
                })?
        };

        Ok(Self {
            root: canonical,
            address: bound,
            shutdown,
            served,
            wake: Some(wake_tx),
            worker: Some(worker),
        })
    }

    /// The address the server is listening on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// The directory the server serves.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// How many responses the server has sent.
    #[must_use]
    pub fn responses_served(&self) -> u64 {
        self.served.load(Ordering::SeqCst)
    }

    /// The origin the server answers on, as a URL.
    ///
    /// The port is included because it is chosen by the operating system and is therefore
    /// not known before the server starts.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The URL of a path under the server's root.
    ///
    /// The path is percent-encoded per component, so a name with a space or a non-ASCII
    /// character in it produces a URL a browser can fetch.
    #[must_use]
    pub fn url_for(&self, path: &str) -> String {
        let mut url = self.origin();
        for component in path.split(['/', '\\']) {
            if component.is_empty() || component == "." {
                continue;
            }
            url.push('/');
            url.push_str(&crate::manifest::percent_encode(component));
        }
        url
    }

    /// Stops the server and waits for its thread to finish.
    ///
    /// Stopping is explicit and idempotent: a second call does nothing rather than
    /// panicking on an already-joined thread.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for AssetServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Accepts connections until the server is asked to stop.
///
/// The listener is non-blocking, so the wait between connections is a channel receive rather
/// than a sleep: a stop sends on `wake` and the loop ends at once. The timeout is the safety
/// net that bounds a lost send.
fn accept_loop(
    listener: &TcpListener,
    root: &Path,
    shutdown: &AtomicBool,
    served: &Arc<AtomicU64>,
    routes: &Arc<Vec<Route>>,
    wake: &Receiver<()>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let root = root.to_path_buf();
                let served = Arc::clone(served);
                let routes = Arc::clone(routes);
                let _ = std::thread::Builder::new()
                    .name("ncui-asset-connection".to_owned())
                    .spawn(move || {
                        let _ = serve_connection(&root, &routes, stream);
                        served.fetch_add(1, Ordering::SeqCst);
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let _ = wake.recv_timeout(SHUTDOWN_POLL);
            }
            Err(_) => return,
        }
    }
}

/// Serves one connection.
fn serve_connection(root: &Path, routes: &[Route], mut stream: TcpStream) -> io::Result<()> {
    // The accepted socket is blocking even though the listener is not, so a request is read
    // in one call rather than a readiness loop. Windows hands the listener's mode down to the
    // socket `accept` returns, and Unix does not — so without this line the first read races
    // the client's write on Windows only: the read returns `WouldBlock` before the request
    // has arrived, the handler gives up, and the client's connection is aborted instead of
    // answered. Making the mode explicit is what keeps the two platforms behaving the same.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(RESPONSE_TIMEOUT))?;

    let Some(request) = read_request_head(&mut stream)? else {
        write_response(&mut stream, &Response::bad_request(), true)?;
        return Ok(());
    };

    let Some(line) = request.lines().next() else {
        write_response(&mut stream, &Response::bad_request(), true)?;
        return Ok(());
    };
    let mut parts = line.split_whitespace();
    let Some(method) = parts.next() else {
        write_response(&mut stream, &Response::bad_request(), true)?;
        return Ok(());
    };
    let Some(target) = parts.next() else {
        write_response(&mut stream, &Response::bad_request(), true)?;
        return Ok(());
    };

    if method != "GET" && method != "HEAD" {
        write_response(&mut stream, &Response::method_not_allowed(), true)?;
        return Ok(());
    }

    // The runtime's own routes are answered first, and by the target as it arrived: a route's
    // parameters are its own business, and a route that had to be percent-decoded first would be a
    // route whose parameters the file server had already interpreted.
    for route in routes {
        if let Some(served) = route.answer(target) {
            let response = Response::ok(served.content_type, served.body);
            return write_response(&mut stream, &response, method == "GET");
        }
    }

    let path = target.split('?').next().unwrap_or(target);
    let Some(decoded) = percent_decode(path) else {
        write_response(&mut stream, &Response::bad_request(), true)?;
        return Ok(());
    };
    let relative = decoded.trim_start_matches('/');
    if relative.is_empty() {
        write_response(&mut stream, &Response::not_found(), true)?;
        return Ok(());
    }

    let Ok(resolved) = ncui_security::path::SafePath::resolve(root, relative) else {
        write_response(&mut stream, &Response::forbidden(), true)?;
        return Ok(());
    };

    // The canonical path is checked as well, because a symlink inside the root may point
    // outside it. `SafePath` resolves lexically and deliberately does not follow links, so
    // this second check is the one that sees where the file actually is.
    let canonical = match resolved.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            write_response(&mut stream, &Response::not_found(), true)?;
            return Ok(());
        }
        Err(_) => {
            write_response(&mut stream, &Response::not_found(), true)?;
            return Ok(());
        }
    };
    if !ncui_security::path::SafePath::is_contained(root, &canonical) {
        write_response(&mut stream, &Response::forbidden(), true)?;
        return Ok(());
    }
    if !canonical.is_file() {
        // A directory is not listed. A listing tells a client what the application
        // contains, and an application serves what it names, not what it happens to hold.
        write_response(&mut stream, &Response::not_found(), true)?;
        return Ok(());
    }

    let Ok(body) = std::fs::read(&canonical) else {
        write_response(&mut stream, &Response::not_found(), true)?;
        return Ok(());
    };
    let content_type = content_type_for(&canonical);
    let etag = etag_for(&canonical, body.len());
    if let Some(if_none_match) = header_value(&request, "if-none-match")
        && etag_matches(&if_none_match, &etag)
    {
        let response = Response::not_modified(etag);
        return write_response(&mut stream, &response, false);
    }
    let response = Response::ok_with_etag(content_type, body, etag);
    // A `HEAD` response carries the head a `GET` would carry, including the length, and
    // no body: the client asked for the headers, not for the file.
    write_response(&mut stream, &response, method != "HEAD")
}

/// One response the server will send.
#[derive(Debug, Clone)]
struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    etag: Option<String>,
}

impl Response {
    /// A response carrying a body.
    fn ok(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
            etag: None,
        }
    }

    /// A response carrying a body and an `ETag` validator.
    fn ok_with_etag(content_type: &'static str, body: Vec<u8>, etag: String) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
            etag: Some(etag),
        }
    }

    /// A `304 Not Modified` response: the head, an `ETag`, no body, no `Content-Type`.
    fn not_modified(etag: String) -> Self {
        Self {
            status: "304 Not Modified",
            content_type: "",
            body: Vec::new(),
            etag: Some(etag),
        }
    }

    /// A response for a request the server could not read.
    fn bad_request() -> Self {
        Self::text("400 Bad Request", "the request could not be read")
    }

    /// A response for a method the server does not serve.
    fn method_not_allowed() -> Self {
        Self::text("405 Method Not Allowed", "this server serves GET and HEAD")
    }

    /// A response for a path that names nothing.
    fn not_found() -> Self {
        Self::text("404 Not Found", "no such file")
    }

    /// A response for a path that reaches outside the root.
    fn forbidden() -> Self {
        Self::text("403 Forbidden", "that path is outside the served directory")
    }

    /// A response carrying a short text body.
    fn text(status: &'static str, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
            etag: None,
        }
    }
}

/// Writes one response to a connection.
///
/// `include_body` is false for a `HEAD` response: the head still carries the length the
/// body would have had, but the body is not sent.
fn write_response(
    stream: &mut TcpStream,
    response: &Response,
    include_body: bool,
) -> io::Result<()> {
    use std::fmt::Write as _;
    let mut head = String::new();
    let _ = write!(head, "HTTP/1.1 {}\r\n", response.status);
    if !response.content_type.is_empty() {
        let _ = write!(head, "Content-Type: {}\r\n", response.content_type);
    }
    if let Some(etag) = &response.etag {
        let _ = write!(head, "ETag: {etag}\r\n");
    }
    let _ = write!(
        head,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    if include_body {
        stream.write_all(&response.body)?;
    }
    stream.flush()
}

/// Builds the `ETag` validator for a file: `"<mtime_ns>-<len>"`, quoted per RFC 9110.
///
/// `mtime_ns` is the file's modified time in nanoseconds since the Unix epoch, or `0`
/// when the filesystem reports none (or a pre-epoch time). `len` is the byte length
/// the server actually serves, so the validator and `Content-Length` can never
/// disagree. The strong form is used: the value changes whenever the file does.
#[must_use]
fn etag_for(path: &Path, len: usize) -> String {
    let mtime_ns: u128 = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    format!("\"{mtime_ns}-{len}\"")
}

/// Extracts a header value from the request head, case-insensitively.
///
/// The head is the request line followed by one `Name: value` per line. Returns the
/// first matching line's value stripped of surrounding whitespace, or `None` when
/// the header is absent. Deliberately forgiving: a malformed line is skipped rather
/// than failing the whole head, because a header this server does not need must not
/// turn an otherwise valid request into a 400.
fn header_value(head: &str, name: &str) -> Option<String> {
    for line in head.lines().skip(1) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// Whether `if_none_match` matches the current `ETag`.
///
/// Supports `*` (any current representation), a single strong or weak tag, and a
/// comma-separated list. Comparison is on the unquoted form: both sides are
/// stripped of any `W/` weak prefix and surrounding quotes, so `W/"1-2"`,
/// `"1-2"` and a bare `1-2` all compare equal to the emitted strong tag.
fn etag_matches(if_none_match: &str, current: &str) -> bool {
    let current = normalize_etag(current);
    for part in if_none_match.split(',') {
        let candidate = part.trim();
        if candidate == "*" {
            return true;
        }
        if !candidate.is_empty() && normalize_etag(candidate) == current {
            return true;
        }
    }
    false
}

/// The unquoted form of an entity tag, minus any weak prefix.
fn normalize_etag(value: &str) -> &str {
    let value = value.trim();
    let value = value.strip_prefix("W/").unwrap_or(value).trim();
    value
        .strip_prefix('"')
        .unwrap_or(value)
        .strip_suffix('"')
        .unwrap_or(value)
}

/// Reads the head of a request, up to the blank line that ends it.
///
/// Returns `None` when the request is malformed or larger than the bound.
fn read_request_head(stream: &mut TcpStream) -> io::Result<Option<String>> {
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte)?;
        if read == 0 {
            // The client closed before sending a complete head.
            return Ok(None);
        }
        head.push(byte[0]);
        if head.len() > MAX_REQUEST_HEAD_BYTES {
            return Ok(None);
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            return Ok(Some(String::from_utf8_lossy(&head).into_owned()));
        }
    }
}

/// Decodes a percent-encoded URL path.
///
/// Returns `None` when the encoding is malformed, which is a bad request rather than a
/// path to guess at.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            let high = (high as char).to_digit(16)?;
            let low = (low as char).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The content type for a file, from its extension.
///
/// An unknown extension is served as `application/octet-stream`, which is the honest
/// answer: the server does not know what the bytes are, and saying so is better than
/// guessing `text/html` and letting the browser execute them.
#[must_use]
pub fn content_type_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percent_encoded_path_is_decoded() {
        assert_eq!(percent_decode("/a%20b.html").as_deref(), Some("/a b.html"));
        assert_eq!(
            percent_decode("/plain.html").as_deref(),
            Some("/plain.html")
        );
        assert_eq!(percent_decode("/%zz").as_deref(), None);
        assert_eq!(percent_decode("/%4").as_deref(), None);
    }

    #[test]
    fn the_content_type_follows_the_extension() {
        assert_eq!(
            content_type_for(Path::new("a.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type_for(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for(Path::new("a.wasm")), "application/wasm");
        assert_eq!(
            content_type_for(Path::new("a.unknown")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for(Path::new("a.HTML")),
            "text/html; charset=utf-8",
            "the extension comparison must not depend on case"
        );
    }

    #[test]
    fn an_etag_matches_its_strong_weak_and_listed_forms() {
        let etag = "\"123-45\"";
        assert!(etag_matches(etag, etag));
        assert!(etag_matches("\"123-45\"", etag));
        assert!(etag_matches("W/\"123-45\"", etag));
        assert!(etag_matches("123-45", etag));
        assert!(etag_matches("\"0-0\", \"123-45\"", etag));
        assert!(etag_matches("*", etag));
        assert!(!etag_matches("\"123-44\"", etag));
        assert!(!etag_matches("\"124-45\"", etag));
        assert!(!etag_matches("", etag));
    }

    #[test]
    fn a_header_is_found_case_insensitively() {
        let head = "GET /a.html HTTP/1.1\r\nhost: localhost\r\nIF-NONE-MATCH: \"1-2\"\r\n\r\n";
        assert_eq!(
            header_value(head, "If-None-Match").as_deref(),
            Some("\"1-2\"")
        );
        assert_eq!(header_value(head, "X-Absent"), None);
    }
}
