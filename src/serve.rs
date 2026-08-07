//! A minimal HTTP/1.1 server for the local exercise page.
//!
//! This exists because the alternative was a dependency. The page needs to talk to
//! the process that owns the `Runner`, and the whole conversation is six JSON
//! endpoints on loopback for one person — an async runtime and a routing framework
//! would be two orders of magnitude more code than the thing they carry.
//!
//! What is implemented is the subset a browser actually emits for `fetch` and a
//! top-level navigation: a request line, headers, and a `Content-Length` body. There
//! is no keep-alive, no chunked transfer, no compression, and no TLS. Every response
//! closes the connection. If some future client needs more than that, the honest fix
//! is a dependency, not growing this file.
//!
//! Containment rests on two properties, in this order: the listener is bound to
//! `127.0.0.1` so nothing off the machine can reach it, and every request carries a
//! token minted at bind time so nothing else *on* the machine reaches it by accident.
//! Neither is a substitute for the other, and both are enforced here rather than in
//! the handler, so a handler bug cannot open a hole.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Largest body accepted. Files edited in the page are source text; anything past
/// this is a mistake or an attack, and either way there is no reason to buffer it.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Largest request head accepted. Bounded separately from the body so a client that
/// never sends a blank line cannot grow the buffer without limit.
const MAX_HEAD: usize = 64 * 1024;

/// Live connections. A submit can hold a thread for a minute while the check runs, so
/// the cap exists to keep a stuck or looping client from consuming every thread and
/// starving the page that is waiting on that same run.
const MAX_CONNS: usize = 8;

/// Accept poll interval. See `Server::run` for why this is a poll at all.
const POLL: Duration = Duration::from_millis(5);

/// Per-socket read and write deadline. A connection that stops talking mid-request
/// must not hold one of the eight slots forever. This does not bound handler time —
/// the handler runs after the request is fully read.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `run` waits for in-flight responses after shutdown is requested.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path only, with the query string removed and percent-escapes left intact.
    pub path: String,
    /// Query parameters, percent-decoded, `+` treated as space.
    pub query: BTreeMap<String, String>,
    /// Header names are lowercased on parse, because HTTP field names are
    /// case-insensitive and callers otherwise depend on whatever the client sent.
    /// Use `header()` rather than indexing this directly.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|v| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: Vec<u8>) -> Self {
        Self { status, content_type: "application/json; charset=utf-8".into(), body }
    }

    pub fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8".into(),
            body: body.as_bytes().to_vec(),
        }
    }

    /// Errors are JSON too. The page has one response parser, so an error that
    /// arrives as `text/plain` turns a clear message into a parse failure.
    pub fn error(status: u16, msg: &str) -> Self {
        let body = format!("{{\"error\":\"{}\"}}", json_escape(msg));
        Self::json(status, body.into_bytes())
    }
}

/// Escape a string for a JSON string literal. Hand-rolled rather than routed through
/// `serde_json` so this module stays free of the domain's serialisation stack; the
/// inputs are our own diagnostics, not arbitrary documents.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Cloneable shutdown trigger. Handed to the handler so `POST /api/done` can end the
/// process from inside a request without the handler owning the listener.
#[derive(Debug, Clone)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

pub struct Server {
    listener: TcpListener,
    port: u16,
    token: String,
    shutdown: Arc<AtomicBool>,
}

/// Bind the loopback interface.
///
/// The address is `127.0.0.1` and never `0.0.0.0`. This is the primary containment
/// property of the whole server: bound this way the socket is unreachable from the
/// network, from other hosts on the LAN, and from anything outside the machine's
/// network namespace, so the token only has to defend against other software running
/// as the same user. Binding a wildcard address would put a shell-equivalent endpoint
/// on the network — the token is not strong enough to be the only thing in the way.
///
/// `port` 0 asks the OS for a free port; read it back with [`Server::port`].
pub fn bind(port: u16) -> Result<Server, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("failed to bind 127.0.0.1:{port}: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to read bound address: {e}"))?
        .port();
    Ok(Server { listener, port, token: mint_token(), shutdown: Arc::new(AtomicBool::new(false)) })
}

/// Mint a 128-bit token, hex-encoded.
///
/// This is NOT cryptographic randomness. It is the wall clock in nanoseconds, the
/// address of a fresh heap allocation, and the process id, run through a bit mixer.
/// An attacker who knows roughly when the process started and can guess the
/// allocator's layout could search a far smaller space than 2^128.
///
/// That is accepted deliberately, and only because of what the token has to defend:
/// the listener is loopback-bound and the machine has one user, so the threat is a
/// stray web page in that user's own browser probing localhost ports — which cannot
/// read the token from the terminal, cannot read the process's memory, and gets one
/// guess per request against a server that only lives for the length of a practice
/// session. Pulling in a CSPRNG would mean a dependency; `/dev/urandom` would mean a
/// filesystem read that fails differently on every platform. If this server ever
/// stops being loopback-only, this function is the first thing that must change.
fn mint_token() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let heap = Box::new(0u8);
    let addr = &*heap as *const u8 as usize as u64;
    let pid = std::process::id() as u64;
    let again =
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) as u64;

    let lo = mix(nanos as u64 ^ addr ^ pid.rotate_left(17));
    let hi = mix((nanos >> 64) as u64 ^ addr.rotate_left(32) ^ again ^ mix(lo));
    format!("{lo:016x}{hi:016x}")
}

/// splitmix64's finalizer: avalanches the low-entropy inputs above so the hex output
/// does not visibly expose the clock.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl Server {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle { flag: Arc::clone(&self.shutdown) }
    }

    /// Serve until a handler calls [`ShutdownHandle::shutdown`]. Blocks the caller.
    ///
    /// Accept is polled rather than blocked on. A blocking `accept` cannot be
    /// interrupted from another thread with only `std`, so shutdown would not take
    /// effect until the next request arrived — which, for a page that shuts the server
    /// down and then stops talking to it, is never. The alternatives were a self-connect
    /// wakeup (a second socket, a loopback dial that can fail on its own, and a
    /// sentinel connection the accept loop has to recognise and discard) or a raw
    /// `poll` via libc (a dependency). A 5ms poll on one foreground process the user is
    /// sitting in front of costs a rounding error of a core and no code, so it wins.
    pub fn run<H>(self, handler: H) -> Result<(), String>
    where
        H: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        self.listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed to set non-blocking accept: {e}"))?;

        let handler = Arc::new(handler);
        let token = Arc::new(self.token.clone());
        let live = Arc::new(AtomicUsize::new(0));

        while !self.shutdown.load(Ordering::SeqCst) {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                    continue;
                }
                // A single failed accept — a descriptor limit, a client that vanished
                // between SYN and accept — is not a reason to end the session.
                Err(e) => {
                    eprintln!("serve: accept failed: {e}");
                    std::thread::sleep(POLL);
                    continue;
                }
            };

            // Linux does not inherit O_NONBLOCK across accept, but that is a platform
            // detail; state it rather than depend on it.
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

            // Reserve a slot before spawning, so the rejection path costs no thread.
            let slot = Slot::take(&live);
            let Some(slot) = slot else {
                let mut stream = stream;
                write_response(&mut stream, &Response::error(503, "too many connections"));
                continue;
            };

            let handler = Arc::clone(&handler);
            let token = Arc::clone(&token);
            std::thread::spawn(move || {
                let _slot = slot;
                serve_connection(stream, &*handler, &token);
            });
        }

        // The request that flipped the shutdown flag is still on a connection thread
        // that has not written its response yet: the handler returns, the accept loop
        // wakes within 5ms and returns, `main` exits, and the detached thread dies
        // mid-write. The page would see its final POST fail rather than succeed.
        // Wait for the live count to fall to zero, bounded, so a submit that is still
        // grading cannot turn the exit into a hang.
        let deadline = std::time::Instant::now() + DRAIN_GRACE;
        while live.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        Ok(())
    }
}

/// Connection-count reservation, released on drop so a panicking connection thread
/// cannot leak a slot and shrink the cap for the rest of the session.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(live: &Arc<AtomicUsize>) -> Option<Slot> {
        let prior = live.fetch_add(1, Ordering::SeqCst);
        let slot = Slot(Arc::clone(live));
        if prior >= MAX_CONNS {
            return None; // `slot` drops here and undoes the increment.
        }
        Some(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve_connection<H>(mut stream: TcpStream, handler: &H, token: &str)
where
    H: Fn(&Request) -> Response,
{
    let req = match read_request(&mut stream) {
        Ok(Some(req)) => req,
        // Client closed or never finished. Port probes look like this; say nothing.
        Ok(None) => return,
        Err(resp) => {
            write_response(&mut stream, &resp);
            return;
        }
    };

    if let Some(resp) = reject(&req, token) {
        write_response(&mut stream, &resp);
        return;
    }

    // A handler that panics on one malformed exercise must cost that request, not the
    // session. Unwinding out of this thread would drop the connection with no status
    // and leave the page hanging on a fetch that never resolves.
    let resp = match catch_unwind(AssertUnwindSafe(|| handler(&req))) {
        Ok(resp) => resp,
        Err(_) => Response::error(500, "handler panicked"),
    };
    write_response(&mut stream, &resp);
}

/// Central gate. Runs before the handler for every request, so authorisation is not
/// something each endpoint has to remember.
fn reject(req: &Request, token: &str) -> Option<Response> {
    // Any page already open in the user's browser can `fetch` a localhost port and,
    // for a simple request, the browser will send it whether or not we consent — CORS
    // withholds the *response* from that page, not the request from us. A cross-origin
    // caller always attaches `Origin`, so refusing every `Origin` that is not one of
    // our own loopback forms turns a state-changing POST into a rejected one. A
    // top-level navigation sends no `Origin` at all, which is why absence is allowed;
    // that path is covered by the query token instead.
    if let Some(origin) = req.header("origin") {
        if !origin_ok(origin) {
            return Some(Response::error(403, "bad origin"));
        }
    }

    let want = token.as_bytes();
    let ok = if req.path == "/" || req.path == "/index.html" {
        req.query.get("t").is_some_and(|t| ct_eq(t.as_bytes(), want))
    } else if req.path.starts_with("/api/") {
        req.header("x-benkyou-token").is_some_and(|t| ct_eq(t.as_bytes(), want))
    } else {
        // Anything else is not a secret: unknown paths reach the handler and get a 404
        // there. Adding an authenticated static-asset route would mean the page has to
        // thread the token through every URL it emits, for no gain — the page is
        // inlined into the one authenticated response.
        true
    };

    if ok {
        None
    } else {
        Some(Response::error(403, "bad token"))
    }
}

fn origin_ok(origin: &str) -> bool {
    origin.starts_with("http://127.0.0.1:") || origin.starts_with("http://localhost:")
}

/// Compare without an early exit on the first differing byte, so the time taken does
/// not narrow down how much of a guessed token was correct. Length is compared up
/// front and does leak; a token of the wrong length is not a near miss worth hiding.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    /// More bytes are needed before this buffer is a request. Not an error on a
    /// socket; the read loop uses it as "keep going".
    Incomplete,
    Malformed,
    TooLarge,
    LengthRequired,
}

impl ParseError {
    fn response(&self) -> Response {
        match self {
            // Only reachable if a caller asks for a response to `Incomplete`; the read
            // loop treats it as a continuation. Reported as a client error either way.
            ParseError::Incomplete | ParseError::Malformed => {
                Response::error(400, "malformed request")
            }
            ParseError::TooLarge => Response::error(413, "request body too large"),
            ParseError::LengthRequired => Response::error(411, "content-length required"),
        }
    }
}

/// Read one request off the socket.
///
/// `Ok(None)` means the peer closed before completing a request. The loop re-runs the
/// full parse after each read rather than parsing the head once and then counting
/// bytes, so the function under test is exactly the function in production; re-parsing
/// a few hundred bytes of head per 64 KiB chunk is not measurable.
fn read_request(stream: &mut TcpStream) -> Result<Option<Request>, Response> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match parse_request(&buf) {
            Ok(req) => return Ok(Some(req)),
            Err(ParseError::Incomplete) => {}
            Err(e) => return Err(e.response()),
        }
        match stream.read(&mut chunk) {
            Ok(0) if buf.is_empty() => return Ok(None),
            Ok(0) => return Ok(None),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return Ok(None),
        }
    }
}

/// Parse a complete request out of a byte buffer.
///
/// Only CRLF line endings are accepted. Bare-LF requests exist in hand-typed telnet
/// sessions and nowhere else; every browser and every `fetch` sends CRLF, and being
/// lenient here means two framing rules to keep in agreement.
fn parse_request(buf: &[u8]) -> Result<Request, ParseError> {
    let Some(head_end) = find_head_end(buf) else {
        if buf.len() > MAX_HEAD {
            return Err(ParseError::Malformed);
        }
        return Err(ParseError::Incomplete);
    };

    let head = std::str::from_utf8(&buf[..head_end - 4]).map_err(|_| ParseError::Malformed)?;
    let mut lines = head.split("\r\n");

    let start = lines.next().ok_or(ParseError::Malformed)?;
    let mut parts = start.split(' ');
    let method = parts.next().filter(|s| !s.is_empty()).ok_or(ParseError::Malformed)?;
    let target = parts.next().filter(|s| !s.is_empty()).ok_or(ParseError::Malformed)?;
    // The version is required by the grammar but nothing here varies on it: every
    // response is framed as if this were HTTP/1.0 with an explicit close.
    let version = parts.next().ok_or(ParseError::Malformed)?;
    if !version.starts_with("HTTP/") || parts.next().is_some() {
        return Err(ParseError::Malformed);
    }

    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(ParseError::Malformed)?;
        if name.is_empty() || name.contains(' ') {
            return Err(ParseError::Malformed);
        }
        // Repeated headers: last wins. The subset of clients that matter never repeat
        // the ones read here, and folding into a list would be a shape no caller wants.
        headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
    }

    let (path, query) = split_target(target);

    let rest = &buf[head_end..];
    let body = match headers.get("content-length") {
        Some(raw) => {
            let len: usize = raw.trim().parse().map_err(|_| ParseError::Malformed)?;
            if len > MAX_BODY {
                return Err(ParseError::TooLarge);
            }
            if rest.len() < len {
                return Err(ParseError::Incomplete);
            }
            rest[..len].to_vec()
        }
        // Chunked transfer is not implemented, and silently treating a chunked body as
        // empty would corrupt a file save rather than fail it. 411 says what is wrong.
        None if !rest.is_empty() || headers.contains_key("transfer-encoding") => {
            return Err(ParseError::LengthRequired);
        }
        None => Vec::new(),
    };

    Ok(Request {
        method: method.to_string(),
        path,
        query,
        headers,
        body,
    })
}

/// Index one past the CRLFCRLF that ends the head.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Split a request target into a path and a decoded query map. The path keeps its
/// percent-escapes: routing compares it against fixed ASCII literals, and decoding it
/// would let `%2f` forge a path separator.
fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    let target = target.split('#').next().unwrap_or(target);
    let (path, raw) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let mut query = BTreeMap::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(k), percent_decode(v));
    }
    (path.to_string(), query)
}

/// Percent-decode one query component, treating `+` as a space per the form encoding
/// browsers use. Malformed escapes are kept verbatim rather than dropped: a token that
/// fails to decode should fail the comparison, not silently become a different string.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match hex_pair(bytes[i + 1], bytes[i + 2]) {
                Some(b) => {
                    out.push(b);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    Some(nibble(hi)? << 4 | nibble(lo)?)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Write the response and close.
///
/// `Cache-Control: no-store` matters more than it looks: the exercise page and the
/// token in its URL end up in browser history and disk cache otherwise, and a cached
/// `GET /api/exercise/0` would show the learner a stale file after a save.
fn write_response(stream: &mut TcpStream, resp: &Response) {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len(),
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    if stream.write_all(&resp.body).is_err() {
        return;
    }
    let _ = stream.flush();
    // Half-close, then drain briefly. Dropping a socket that still has unread request
    // bytes queued makes the kernel send RST, which can discard the response we just
    // wrote — the client sees a connection error instead of the 403 explaining itself.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 1024];
    while let Ok(n) = stream.read(&mut sink) {
        if n == 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Request, ParseError> {
        parse_request(raw.as_bytes())
    }

    #[test]
    fn parses_request_line_and_headers() {
        let req = parse("GET /api/session HTTP/1.1\r\nHost: 127.0.0.1:7777\r\nX-Benkyou-Token: abc\r\n\r\n")
            .unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/api/session");
        assert!(req.query.is_empty());
        assert!(req.body.is_empty());
        assert_eq!(req.header("x-benkyou-token"), Some("abc"));
        // Lookup is case-insensitive in both directions.
        assert_eq!(req.header("X-Benkyou-Token"), Some("abc"));
        assert_eq!(req.header("host"), Some("127.0.0.1:7777"));
    }

    #[test]
    fn parses_query_and_keeps_path_escaped() {
        let req = parse("GET /?t=de%61d+beef&x=%2Fy HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.path, "/");
        assert_eq!(req.query.get("t").map(String::as_str), Some("dead beef"));
        assert_eq!(req.query.get("x").map(String::as_str), Some("/y"));

        let req = parse("GET /a%2Fb HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.path, "/a%2Fb");
    }

    #[test]
    fn reads_exactly_content_length_bytes() {
        let req = parse("PUT /api/file HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello, trailing junk")
            .unwrap();
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn short_body_is_incomplete_not_malformed() {
        let err = parse("PUT /api/file HTTP/1.1\r\nContent-Length: 5\r\n\r\nhel").unwrap_err();
        assert_eq!(err, ParseError::Incomplete);
    }

    #[test]
    fn head_without_blank_line_is_incomplete() {
        assert_eq!(parse("GET / HTTP/1.1\r\nHost: x\r\n").unwrap_err(), ParseError::Incomplete);
        assert_eq!(parse("").unwrap_err(), ParseError::Incomplete);
    }

    #[test]
    fn oversized_head_is_rejected_rather_than_buffered() {
        let raw = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(MAX_HEAD));
        assert_eq!(parse(&raw).unwrap_err(), ParseError::Malformed);
    }

    #[test]
    fn oversized_content_length_is_413() {
        let raw = format!("PUT /api/file HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert_eq!(parse(&raw).unwrap_err(), ParseError::TooLarge);
        assert_eq!(parse(&raw).unwrap_err().response().status, 413);
    }

    #[test]
    fn body_without_content_length_is_411() {
        let err = parse("POST /api/run HTTP/1.1\r\n\r\n{}").unwrap_err();
        assert_eq!(err, ParseError::LengthRequired);
        assert_eq!(err.response().status, 411);

        let chunked = parse("POST /api/run HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap_err();
        assert_eq!(chunked, ParseError::LengthRequired);
    }

    #[test]
    fn no_body_and_no_content_length_is_fine() {
        assert!(parse("POST /api/next HTTP/1.1\r\n\r\n").unwrap().body.is_empty());
    }

    #[test]
    fn malformed_start_lines_are_rejected() {
        for raw in [
            "GET\r\n\r\n",
            "GET /\r\n\r\n",
            "GET / HTTP/1.1 extra\r\n\r\n",
            "GET / SPDY/1\r\n\r\n",
            " / HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(parse(raw).unwrap_err(), ParseError::Malformed, "{raw:?}");
        }
        assert_eq!(
            parse("GET / HTTP/1.1\r\nnot-a-header\r\n\r\n").unwrap_err(),
            ParseError::Malformed
        );
    }

    #[test]
    fn percent_decoding_handles_truncated_escapes() {
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%20"), " ");
        assert_eq!(percent_decode("%c3%a9"), "é");
        // Kept verbatim so a mangled token stays a mismatch instead of becoming a
        // different, possibly valid, string.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn constant_time_compare_matches_equality() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"deadbeef", b"deadbeef"));
        assert!(!ct_eq(b"deadbeef", b"deadbeee"));
        assert!(!ct_eq(b"Deadbeef", b"deadbeef"));
        assert!(!ct_eq(b"dead", b"deadbeef"));
        assert!(!ct_eq(b"deadbeef", b""));
    }

    #[test]
    fn origin_must_be_loopback() {
        assert!(origin_ok("http://127.0.0.1:7777"));
        assert!(origin_ok("http://localhost:1"));
        assert!(!origin_ok("http://evil.example"));
        assert!(!origin_ok("https://127.0.0.1:7777"));
        // No port means not one of ours, and `127.0.0.1.evil.com` must not slip past
        // a prefix test that forgot the colon.
        assert!(!origin_ok("http://127.0.0.1"));
        assert!(!origin_ok("http://127.0.0.1.evil.com/"));
        assert!(!origin_ok("http://localhost.evil.com/"));
        assert!(!origin_ok("null"));
    }

    fn req(raw: &str) -> Request {
        parse(raw).unwrap()
    }

    #[test]
    fn gate_requires_query_token_on_the_page() {
        let token = "0123456789abcdef";
        assert!(reject(&req("GET /?t=0123456789abcdef HTTP/1.1\r\n\r\n"), token).is_none());
        assert!(reject(&req("GET /index.html?t=0123456789abcdef HTTP/1.1\r\n\r\n"), token)
            .is_none());
        for raw in ["GET / HTTP/1.1\r\n\r\n", "GET /?t=wrong HTTP/1.1\r\n\r\n"] {
            let resp = reject(&req(raw), token).expect("must be refused");
            assert_eq!(resp.status, 403);
            assert_eq!(resp.body, br#"{"error":"bad token"}"#);
        }
    }

    #[test]
    fn gate_requires_header_token_on_api() {
        let token = "0123456789abcdef";
        let ok = "POST /api/run HTTP/1.1\r\nX-Benkyou-Token: 0123456789abcdef\r\n\r\n";
        assert!(reject(&req(ok), token).is_none());
        // The query token does not authorise the API, and vice versa.
        let wrong_channel = "POST /api/run HTTP/1.1\r\n\r\n";
        assert_eq!(reject(&req(wrong_channel), token).unwrap().status, 403);
        let page_style = "POST /api/run?t=0123456789abcdef HTTP/1.1\r\n\r\n";
        assert_eq!(reject(&req(page_style), token).unwrap().status, 403);
    }

    #[test]
    fn gate_refuses_foreign_origin_before_token() {
        let token = "0123456789abcdef";
        let raw = "POST /api/run HTTP/1.1\r\nOrigin: http://evil.example\r\nX-Benkyou-Token: 0123456789abcdef\r\n\r\n";
        let resp = reject(&req(raw), token).expect("must be refused");
        assert_eq!(resp.status, 403);
        assert_eq!(resp.body, br#"{"error":"bad origin"}"#);

        let good = "POST /api/run HTTP/1.1\r\nOrigin: http://localhost:7777\r\nX-Benkyou-Token: 0123456789abcdef\r\n\r\n";
        assert!(reject(&req(good), token).is_none());
    }

    #[test]
    fn error_bodies_are_valid_json() {
        let resp = Response::error(500, "he said \"no\"\nand\\left");
        assert_eq!(resp.status, 500);
        assert_eq!(
            String::from_utf8(resp.body).unwrap(),
            r#"{"error":"he said \"no\"\nand\\left"}"#
        );
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn token_is_128_bits_of_hex() {
        let t = mint_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
