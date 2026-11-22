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
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// The largest request head the server will read, in bytes.
///
/// A request head is the request line and the headers. 16 KiB is far more than any real
/// request needs and small enough that a client cannot make the server allocate without
/// bound.
pub const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

/// The largest request body the server will read, in bytes.
///
/// A `POST` body is how a page moves a large buffer to the runtime without JSON-encoding it
/// (the binary data channel). 64 MiB is far more than an application buffer needs and small
/// enough that a client cannot make the server allocate without bound. A body declaring more
/// than this is refused before a byte of it is read.
pub const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The three-letter English month abbreviations an HTTP-date uses, indexed `0` = January.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

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
/// The responder is handed the request target (path and query) and the request body. A `GET`
/// or `HEAD` carries no body, so the body is empty for the routes that only produce a body
/// (an island frame, the reload bridge); a `POST` carries the bytes the page sent, which is
/// how a large buffer reaches the runtime un-encoded.
///
/// `None` means "not mine", which is how a responder declines a path rather than answering it with
/// an empty body — an empty body is a 200, and a page cannot tell it from a real one.
pub type Responder = Arc<dyn Fn(&str, &[u8]) -> Option<Served> + Send + Sync>;

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
    fn answer(&self, target: &str, body: &[u8]) -> Option<Served> {
        let path = target.split('?').next().unwrap_or(target);
        if path != self.prefix && !path.starts_with(&format!("{}/", self.prefix)) {
            return None;
        }
        (self.responder)(target, body)
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

        let shutdown = Arc::new(AtomicBool::new(false));
        let served = Arc::new(AtomicU64::new(0));
        let routes = Arc::new(routes);
        let worker = {
            let shutdown = Arc::clone(&shutdown);
            let served = Arc::clone(&served);
            let canonical = canonical.clone();
            let routes = Arc::clone(&routes);
            std::thread::Builder::new()
                .name("ncui-asset-server".to_owned())
                .spawn(move || {
                    accept_loop(&listener, &canonical, &shutdown, &served, &routes);
                })?
        };

        Ok(Self {
            root: canonical,
            address: bound,
            shutdown,
            served,
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
    ///
    /// The accept loop blocks in `accept()`, so a flag alone would not wake it — the loop
    /// would sit in the kernel until the next connection. Opening one loopback connection
    /// to our own address hands `accept()` a connection to return at once; the loop sees
    /// the shutdown flag, drops the throwaway stream, and exits. This is what lets the
    /// accept block in the kernel (zero idle CPU) *and* still stop immediately, rather
    /// than trading one against the other through a poll interval.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // A dropped connection is expected here: the accept loop is being told to stop, so
        // it will not answer this probe. Ignore the result either way — the join below is
        // what confirms the loop actually ended.
        let _ = TcpStream::connect(self.address);
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
/// The listener is blocking, so `accept()` parks in the kernel until a connection arrives
/// and costs nothing while the application is idle — measured at 0.00 % of a core, the same
/// as runtimes whose accept blocks, and against the 0.94 % the previous 2 ms poll read. A
/// connection is therefore answered the moment it arrives rather than on the next poll, and
/// a stop wakes the loop by opening one throwaway connection (`AssetServer::stop`), so the
/// two goals that a poll trades against — idle CPU and accept latency — are both met.
fn accept_loop(
    listener: &TcpListener,
    root: &Path,
    shutdown: &AtomicBool,
    served: &Arc<AtomicU64>,
    routes: &Arc<Vec<Route>>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        // Blocking accept. The only error this normally produces is an interrupted or
        // aborted accept, and a spurious wakeup is handled by looping rather than by
        // treating it as fatal: a busy server must not die because one accept came back
        // empty. The shutdown flag is re-read at the top of the loop, so the throwaway
        // connection `stop` opens is accepted, seen to be during shutdown, and dropped.
        let accepted = listener.accept();
        let Ok((stream, _)) = accepted else {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            continue;
        };
        if shutdown.load(Ordering::SeqCst) {
            // This is the probe `stop` opened to wake us, or a client that raced a stop.
            // Either way the server is done: drop the stream rather than answering it.
            drop(stream);
            return;
        }
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

    // A `POST` is how a page moves a large buffer to the runtime without JSON-encoding it: the
    // body is the payload, read here once and handed to the route. `GET` and `HEAD` carry no
    // body, so their routes see an empty one.
    let body = if method == "POST" {
        match read_body(&mut stream, &request)? {
            Some(body) => body,
            None => {
                // The body declared more than the server will read; refuse before
                // reading a byte of it.
                write_response(&mut stream, &Response::payload_too_large(), true)?;
                return Ok(());
            }
        }
    } else if method != "GET" && method != "HEAD" {
        write_response(&mut stream, &Response::method_not_allowed(), true)?;
        return Ok(());
    } else {
        Vec::new()
    };

    // The runtime's own routes are answered first, and by the target as it arrived: a route's
    // parameters are its own business, and a route that had to be percent-decoded first would be a
    // route whose parameters the file server had already interpreted.
    for route in routes {
        if let Some(served) = route.answer(target, &body) {
            let response = Response::ok(served.content_type, served.body);
            // A `POST` answer carries its body like a `GET` does; only a `HEAD` omits it.
            return write_response(&mut stream, &response, method != "HEAD");
        }
    }

    // A `POST` is answered only by a route that claims its path. The file server serves `GET`
    // and `HEAD`, so a `POST` no route claimed must not fall through to reading a file — it is
    // refused here rather than served the file's bytes.
    if method != "GET" && method != "HEAD" {
        write_response(&mut stream, &Response::method_not_allowed(), true)?;
        return Ok(());
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
    // The modification time drives both validators, so it is read once and handed to
    // each: the `ETag` folds it into its strong value and `Last-Modified` renders it as
    // an HTTP-date. Two validators, one read.
    let modified = modification_time(&canonical);
    let etag = etag_for(modified, body.len());
    let last_modified = modified.map(http_date);
    let matches_condition = is_condition_met(&request, &etag, modified);
    if matches_condition {
        let response = Response::not_modified(etag, last_modified);
        return write_response(&mut stream, &response, false);
    }
    let response = Response::ok_with_etag(content_type, body, etag, last_modified);
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
    last_modified: Option<String>,
}

impl Response {
    /// A response carrying a body.
    fn ok(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
            etag: None,
            last_modified: None,
        }
    }

    /// A response carrying a body, an `ETag` validator and a `Last-Modified` date.
    fn ok_with_etag(
        content_type: &'static str,
        body: Vec<u8>,
        etag: String,
        last_modified: Option<String>,
    ) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
            etag: Some(etag),
            last_modified,
        }
    }

    /// A `304 Not Modified` response: the head, both validators, no body, no `Content-Type`.
    ///
    /// A `304` must carry the same validators the `200` would have, so a cache that
    /// stored the representation can keep using it and keep validating against it.
    fn not_modified(etag: String, last_modified: Option<String>) -> Self {
        Self {
            status: "304 Not Modified",
            content_type: "",
            body: Vec::new(),
            etag: Some(etag),
            last_modified,
        }
    }

    /// A response for a request the server could not read.
    fn bad_request() -> Self {
        Self::text("400 Bad Request", "the request could not be read")
    }

    /// A response for a method the server does not serve.
    fn method_not_allowed() -> Self {
        Self::text(
            "405 Method Not Allowed",
            "this server serves GET, HEAD and POST",
        )
    }

    /// A response for a `POST` body larger than the server will read.
    ///
    /// Refused before a byte of the body is read, so a client cannot make the server
    /// allocate without bound by declaring a huge `Content-Length`.
    fn payload_too_large() -> Self {
        Self::text(
            "413 Payload Too Large",
            "the request body is larger than this server reads",
        )
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
            last_modified: None,
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
    if let Some(last_modified) = &response.last_modified {
        let _ = write!(head, "Last-Modified: {last_modified}\r\n");
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

/// The file's modification time, or `None` when the filesystem reports none.
fn modification_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Builds the `ETag` validator for a file: `"<mtime_ns>-<len>"`, quoted per RFC 9110.
///
/// `modified` is the file's modification time, `None` when the filesystem reports none.
/// `mtime_ns` becomes `0` for `None` or a pre-epoch time. `len` is the byte length the
/// server actually serves, so the validator and `Content-Length` can never disagree. The
/// strong form is used: the value changes whenever the file does.
#[must_use]
fn etag_for(modified: Option<SystemTime>, len: usize) -> String {
    let mtime_ns: u128 = modified
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    format!("\"{mtime_ns}-{len}\"")
}

/// Renders a time as an HTTP-date: IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`
/// (RFC 9110 §5.6.1). This is the one format `Last-Modified` and `If-Modified-Since`
/// speak, and the only one this server emits.
#[must_use]
fn http_date(time: SystemTime) -> String {
    let Ok(secs) = time.duration_since(SystemTime::UNIX_EPOCH) else {
        // A pre-epoch time has no HTTP-date. `Last-Modified` is omitted rather than
        // guessed at, and a dateless response simply cannot be conditionally fetched
        // by date — which is the honest answer for a file older than the epoch.
        return String::new();
    };
    let (year, month, day, hour, minute, second, weekday) = civil_from_unix(secs.as_secs());
    format!("{weekday}, {day:02} {month} {year:04} {hour:02}:{minute:02}:{second:02} GMT")
}

/// Splits seconds since the Unix epoch into civil date and time, plus the weekday name.
///
/// The days-from-civil algorithm (Howard Hinnant's) is used rather than a date library:
/// the workspace carries no chronology dependency, and a proleptic Gregorian conversion
/// is a dozen lines of arithmetic. Dates before 1970 are not produced here because the
/// only caller converts a duration-since-epoch, which is non-negative by construction.
#[must_use]
fn civil_from_unix(secs: u64) -> (u64, &'static str, u64, u64, u64, u64, &'static str) {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;

    // Shift the epoch to March 1 of the year 0 so the leap-day is the last day of the
    // year and the leap correction falls out of the year count cleanly.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    // `mp` runs 0..=11 over March..February of the shifted year. Mapping it back to a
    // civil month: March (mp=0) is index 2, and January/February wrap to indices 0/1
    // *of the following* civil year, so the year only steps back for mp >= 10.
    let month_index = if mp < 10 { mp + 2 } else { mp - 10 };
    let year = if mp >= 10 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };

    // 1970-01-01 was a Thursday (index 4 with Sunday as 0).
    let weekday = WEEKDAYS[((days + 4) % 7) as usize];
    (
        year,
        MONTHS[month_index as usize],
        day,
        hour,
        minute,
        second,
        weekday,
    )
}

/// Whether a file modified at `modified` is unchanged as of an `If-Modified-Since` date.
///
/// RFC 9110 §13.1.2: the condition is "not modified" when the modification date is
/// *earlier than or equal to* the date the client sends. HTTP-dates carry 1-second
/// resolution, so both sides are truncated to whole seconds before comparing — otherwise
/// a file modified at `…37.5` would compare *greater* than the `…37` date the client
/// echoes back from `Last-Modified`, and every conditional request would miss. A
/// sub-second `Last-Modified` is a validator this format cannot round-trip, so the
/// comparison is done in whole seconds, which is what the format actually carries.
///
/// An unparseable date is treated as no condition at all rather than as a mismatch, so a
/// client that sends a date this server cannot read still gets its representation — a
/// false 304 would be worse, because it would hand back an empty body and the client
/// would show nothing.
#[must_use]
fn modified_within(if_modified_since: &str, modified: SystemTime) -> bool {
    let Some(condition) = parse_http_date(if_modified_since) else {
        return false;
    };
    // Truncate both to whole seconds: the HTTP-date format has no sub-second field, so
    // comparing anything finer would make the condition depend on digits neither side
    // transmitted.
    let modified_secs = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_secs());
    let condition_secs = condition
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_secs());
    modified_secs <= condition_secs
}

/// Parses an HTTP-date in IMF-fixdate form into a time since the Unix epoch.
///
/// Only IMF-fixdate is accepted (`Sun, 06 Nov 1994 08:49:37 GMT`). The two obsolete
/// RFC 9110 forms (`Sunday, 06-Nov-94 …` and `Sun Nov  6 08:49:37 1994`) are declined:
/// a client that emits one of them is answered with a full `200` rather than a `304`,
/// which is the safe direction. Returns `None` for anything else.
#[must_use]
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let value = value.trim();
    let rest = value.split_once(", ").map_or(value, |(_, rest)| rest);
    let mut parts = rest.split(' ').filter(|part| !part.is_empty());
    let day: u64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let year: u64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if parts.next()? != "GMT" {
        return None;
    }
    if !(hour < 24 && minute < 60 && second < 60) {
        return None;
    }

    let month_index = MONTHS.iter().position(|name| *name == month_name)? as u64;

    let days = days_from_civil(year, month_index, day)?;
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Days since the Unix epoch for a proleptic-Gregorian date, or `None` if out of range.
///
/// `month_index` is 0-based civil (`0` = January). The inverse of [`civil_from_unix`]:
/// January and February are folded into the previous March-based year, so the leap
/// correction stays uniform, exactly as in the forward conversion.
fn days_from_civil(year: u64, month_index: u64, day: u64) -> Option<u64> {
    if month_index > 11 || !(1..=31).contains(&day) {
        return None;
    }
    let m = month_index + 1; // 1-based civil month, as the classic conversion expects.
    let (y, mp) = if m <= 2 {
        (year.checked_sub(1)?, m + 9) // Jan/Feb: previous year, mp 10/11
    } else {
        (year, m - 3) // Mar..Dec: this year, mp 0..9
    };
    let era = y / 400;
    let yoe = y % 400;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4)?
        .checked_sub(yoe / 100)?
        .checked_add(doy)?;
    era.checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)
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

/// Whether a conditional `GET`/`HEAD` is satisfied by a file's current validators, so the
/// server answers `304 Not Modified` rather than resending the body.
///
/// `If-None-Match` takes precedence over `If-Modified-Since` (RFC 9110 §13.1.3): a client
/// that sends both is making an entity-tag request that also carries a date, and the tag is
/// the stronger claim. The date is consulted only when no tag was sent.
fn is_condition_met(request: &str, etag: &str, modified: Option<SystemTime>) -> bool {
    match header_value(request, "if-none-match") {
        Some(if_none_match) => etag_matches(&if_none_match, etag),
        None => match (
            header_value(request, "if-modified-since").as_deref(),
            modified,
        ) {
            (Some(if_modified_since), Some(modified)) => {
                modified_within(if_modified_since, modified)
            }
            _ => false,
        },
    }
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
/// Reads a `POST` request body of `Content-Length` bytes, bounded by
/// [`MAX_REQUEST_BODY_BYTES`].
///
/// The head has already been read, so the body — if any arrived with it — is still in the socket
/// and is read here directly. A body that declares more than the limit is refused *before* a byte
/// is read (`None`), so a client cannot make the server allocate without bound. A missing or
/// malformed `Content-Length` is treated as an empty body: there is nothing to read, and a route
/// that wanted a body sees none rather than the server guessing.
fn read_body(stream: &mut TcpStream, head: &str) -> io::Result<Option<Vec<u8>>> {
    let Some(value) = header_value(head, "Content-Length") else {
        return Ok(Some(Vec::new()));
    };
    let Ok(length) = value.trim().parse::<usize>() else {
        return Ok(Some(Vec::new()));
    };
    if length > MAX_REQUEST_BODY_BYTES {
        return Ok(None);
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body)?;
    Ok(Some(body))
}

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

    #[test]
    fn the_epoch_renders_as_the_canonical_http_date() {
        // The one date the format is named for: 1970-01-01T00:00:00Z was a Thursday.
        assert_eq!(
            http_date(SystemTime::UNIX_EPOCH),
            "Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }

    #[test]
    fn a_known_instant_renders_as_the_rfc_example_date() {
        // RFC 9110 §5.6.1 uses 1994-11-06T08:49:37Z throughout; it was a Sunday.
        let secs = 784_111_777_u64;
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(
            http_date(time),
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "the rendered date must match the RFC's own example"
        );
    }

    #[test]
    fn a_date_round_trips_through_parse() {
        // Every date this server emits must parse back to the same instant, or a client
        // echoing Last-Modified would get a full 200 instead of a 304.
        for secs in [0_u64, 1, 86_399, 784_111_777, 1_700_000_000, 4_102_444_799] {
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            let rendered = http_date(time);
            assert_eq!(parse_http_date(&rendered), Some(time), "at {secs}s");
        }
    }

    #[test]
    fn a_leap_day_round_trips() {
        // 2024-02-29 is a real leap day; a date conversion that drops it would parse a
        // Last-Modified from a leap day into the wrong instant.
        let rendered = "Thu, 29 Feb 2024 12:00:00 GMT";
        let parsed = parse_http_date(rendered).expect("the leap day parses");
        assert_eq!(http_date(parsed), rendered, "the leap day round-trips");
    }

    #[test]
    fn an_unparseable_date_is_not_a_condition() {
        // A date the server cannot read must not produce a 304 (that would hand back an
        // empty body); it is treated as no condition and the file is served in full.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        for value in ["", "not a date", "Sat, 06 Nov 1994 08:49:37 PST", "0"] {
            assert!(
                !modified_within(value, now),
                "{value:?} must not be read as a satisfied condition"
            );
        }
    }

    #[test]
    fn a_modified_time_at_or_before_the_date_satisfies_the_condition() {
        // RFC 9110 §13.1.2: "not modified" when the file's date is <= the client's date.
        let file = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
        assert!(modified_within(&http_date(earlier), file), "file <= client");
        assert!(
            !modified_within(&http_date(later), file),
            "a file newer than the client's date must be served"
        );
    }
}
