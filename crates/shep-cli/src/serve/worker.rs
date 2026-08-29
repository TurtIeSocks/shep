//! `shep serve`'s worker: binds a listener, accepts connections, and answers
//! each one against [`ServeConfig`] — the last piece Phase 15 decision 5's
//! whole argument was building toward.
//!
//! `#[cfg(unix)]`: this module binds a real listener and reads real files,
//! the same reason `serve::fs` is unix-only and `lookout`/`whistle`
//! already are. `path`, `mime`, `listing` and `auth` stay pure; this is the
//! module that actually calls them.
//!
//! The accept loop is copied from `dog::metrics::run`/`accept_forever`
//! deliberately, not reinvented: bind, race `SIGINT`/`SIGTERM` against the
//! loop, one task per connection, `Connection: close` on every reply. Two
//! things that loop does not need and this one must have (decision 4): an
//! admission [`tokio::sync::Semaphore`] and a whole-connection
//! [`tokio::time::timeout`]. The metrics dog is loopback-only and writes one
//! small in-memory body; `serve` is invited to bind wider and streams files
//! off disk, so a client that stops reading mid-transfer must not hold a
//! task, an open file and a socket for the rest of the process's life.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use super::auth::Credentials;
use super::{fs, listing, mime, path};
use crate::exit::ExitCode;
use crate::http::{self, Header, HttpError, HttpRequest};

/// How long [`http::read_request`] waits for a connected peer to finish
/// sending its request before this worker gives up on it. The same value
/// and the same reasoning as `dog::metrics`'s own `READ_TIMEOUT`: generous
/// for an ordinary client, small enough that a peer that connects and says
/// nothing does not hold a task — and, here, a semaphore permit — open
/// indefinitely.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How many connections may be in flight at once.
///
/// A permit is held for the whole life of a connection task, so this is a
/// hard ceiling on tasks, open files and sockets together. Without it a
/// client that requests a large file and then stops reading holds all three
/// indefinitely — `tokio::io::copy` waits on a full send buffer forever —
/// and a few hundred of those exhaust the process's descriptor table with no
/// error anywhere. 512 is comfortably above what a static site serving a
/// page's worth of assets needs and comfortably below a default
/// `RLIMIT_NOFILE`.
const MAX_CONNECTIONS: usize = 512;

/// The whole-connection deadline every real `shep serve` runs with: read,
/// resolve, respond and copy together.
///
/// `READ_TIMEOUT` bounds the read phase only. A static file server has no
/// legitimate minute-long request, and the alternative to a deadline is a
/// slow-read client holding a permit until the process exits.
///
/// Carried on [`ServeConfig`] rather than read directly, so a test can ask
/// for a deadline it can wait out on a real clock. The test that covers this
/// used to force sixty virtual seconds on a paused clock and raced tokio's
/// auto-advance against real socket IO, failing about one run in three.
pub const CONNECTION_DEADLINE: Duration = Duration::from_secs(60);

/// What a running `serve` worker was told to do. Built by the verb (Task 7)
/// from its flags, and by nothing else.
///
/// No `Debug`: [`Credentials`] deliberately carries none (its own doc
/// explains why — there is no line of output anywhere in shep where
/// printing a credential is the right answer), and a struct holding an
/// `Option<Credentials>` inherits that same answer rather than working
/// around it with a hand-written impl that has to remember not to print the
/// one field that matters.
pub struct ServeConfig {
    /// The docroot, already canonical and already known to be a directory.
    pub root: PathBuf,
    /// Where to listen. Loopback unless the operator said otherwise.
    pub bind: SocketAddr,
    /// Serve `<root>/index.html` for a would-be 404 that accepts HTML.
    pub spa: bool,
    /// Render a listing for a directory with no index.
    pub listing: bool,
    /// Serve paths with a leading-dot segment, and list them. Off by
    /// default: `shep serve .` in a repo checkout would otherwise publish
    /// `.env` and the whole `.git` object store (decision 4).
    pub hidden: bool,
    /// Credentials every request must satisfy, if any.
    pub auth: Option<Credentials>,
    /// Permit a symlink anywhere in the resolved path, falling back to a
    /// canonicalize-then-`starts_with` containment check that reopens the
    /// TOCTOU the default per-component walk closes. Off by default
    /// (decision 5); `--follow-symlinks` on `ServeArgs` is the only place
    /// this is ever set true. Passed straight through to `fs::contain`.
    pub follow_symlinks: bool,
    /// How long one connection may live, start to finish.
    ///
    /// [`CONNECTION_DEADLINE`] everywhere outside tests. A parameter rather
    /// than a constant read at the use site so the test covering it can pick
    /// a duration it can wait out for real, instead of pausing the clock and
    /// racing tokio's auto-advance against live socket IO.
    pub connection_deadline: Duration,
}

/// Runs `shep serve`'s worker until it is signalled: binds `cfg.bind` and
/// serves until `SIGINT` or `SIGTERM` — the latter is what the shepherd's
/// own kill ladder actually sends first (`shep disable`'s first rung), and a
/// worker that only handles `SIGINT` rides the whole ladder to `SIGKILL` on
/// every `shep stop`, which is slow and looks like a hang.
///
/// A refused bind is fatal: this worker's whole purpose is to serve that
/// port, and one that is running but bound to nothing is worse than a
/// registered sheep `shep flock` reports as errored, because the first
/// looks fine from the outside.
///
/// `run`'s only caller is `commands::serve`, which the `--foreground` flag
/// dispatches straight to this function. This module's own tests never call
/// `run` itself: they drive [`accept_forever`] directly, because `run`'s
/// `SIGTERM` handling needs a real stop ladder to exercise, which only the
/// e2e tier can provide.
pub async fn run(cfg: ServeConfig) -> ExitCode {
    let listener = match TcpListener::bind(cfg.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("shep serve: could not bind {}: {err}", cfg.bind);
            return ExitCode::Failure;
        }
    };
    let mut sigterm = match crate::shutdown::Terminate::install() {
        Ok(sigterm) => sigterm,
        Err(err) => {
            eprintln!("shep serve: could not install a SIGTERM handler: {err}");
            return ExitCode::Failure;
        }
    };
    let cfg = Arc::new(cfg);
    let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
        () = accept_forever(listener, cfg, semaphore) => {}
    }
    ExitCode::Success
}

/// Accepts connections off `listener` forever, one task per connection.
/// Never returns on its own — [`run`] races it against the two shutdown
/// signals, and a test drives it directly, aborting the
/// [`tokio::task::JoinHandle`] it comes back on rather than waiting for a
/// return that never happens.
///
/// A permit is acquired **before** the task is spawned, and
/// non-blockingly: a connection that arrives once the cap is already full is
/// closed immediately rather than queued for a permit that may never come
/// free — queuing would still hold the accepted socket, just without a task
/// attached to it yet, which is the same descriptor exhaustion under a
/// different name.
async fn accept_forever(listener: TcpListener, cfg: Arc<ServeConfig>, semaphore: Arc<Semaphore>) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let cfg = Arc::clone(&cfg);
                tokio::spawn(async move {
                    let _permit = permit;
                    let deadline = cfg.connection_deadline;
                    let _ = tokio::time::timeout(deadline, handle_connection(stream, cfg)).await;
                });
            }
            Err(err) => {
                eprintln!("shep serve: accept failed: {err}");
            }
        }
    }
}

/// Answers exactly one request on `stream`, in the order decision 6 pins:
/// read, auth, method, body, resolve, hidden, contain, then a directory or
/// a file. Every reply — refusal, redirect, listing or file — is logged as
/// one access-log line (decision 16) before this returns.
async fn handle_connection(mut stream: TcpStream, cfg: Arc<ServeConfig>) {
    let peer = stream.peer_addr().ok();

    // 1. read, with its own bounded timeout. On any error there is no
    // well-formed request to answer, so nothing is logged and nothing is
    // written back.
    let request = match http::read_request(&mut stream, READ_TIMEOUT).await {
        Ok(request) => request,
        Err(_err) => return,
    };

    let raw_path = request
        .target
        .split(['?', '#'])
        .next()
        .unwrap_or(request.target.as_str());
    // The logged path starts as the raw target, escaped — decision 16's
    // fallback for every refusal that happens before resolution succeeds.
    // It is overwritten with the *resolved* path the moment resolution
    // does succeed, a few lines down.
    let mut logged_path = escape_for_log(raw_path);

    // 2. auth — before path resolution (decision 6, pinned by
    // `an_unauthenticated_request_is_401_whatever_the_path_says`): an
    // unauthenticated client must not be able to use 400-vs-404 to map the
    // filesystem before it has proven who it is.
    if let Some(creds) = &cfg.auth {
        let header = request.headers.get("authorization").map(String::as_str);
        if !creds.satisfies(header) {
            let body = b"unauthorized\n";
            send(
                &mut stream,
                401,
                "text/plain",
                body,
                vec![Header {
                    name: "WWW-Authenticate",
                    value: "Basic realm=\"shep\"",
                }],
            )
            .await;
            log_access(peer, &request.method, &logged_path, 401, body.len() as u64);
            return;
        }
    }

    // 3. method — this server answers GET and HEAD only.
    if request.method != "GET" && request.method != "HEAD" {
        let body = b"method not allowed; this server answers GET and HEAD\n";
        send(
            &mut stream,
            405,
            "text/plain",
            body,
            vec![Header {
                name: "Allow",
                value: "GET, HEAD",
            }],
        )
        .await;
        log_access(peer, &request.method, &logged_path, 405, body.len() as u64);
        return;
    }

    // 4. body — this server never reads one.
    let has_declared_body = request
        .headers
        .get("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|len| len > 0)
        || request.headers.contains_key("transfer-encoding");
    if has_declared_body {
        let body = b"this server does not accept a request body\n";
        send(&mut stream, 400, "text/plain", body, vec![]).await;
        log_access(peer, &request.method, &logged_path, 400, body.len() as u64);
        return;
    }

    // 5. resolve.
    let segments = match path::resolve(&request.target) {
        Ok(segments) => segments,
        Err(refusal) => {
            let body = refusal.to_string();
            send(&mut stream, 400, "text/plain", body.as_bytes(), vec![]).await;
            log_access(peer, &request.method, &logged_path, 400, body.len() as u64);
            return;
        }
    };
    logged_path = format!("/{}", segments.join("/"));

    // 6. hidden — a 404, not a 403, so it does not distinguish "hidden"
    // from "missing" (decision 6). Before any filesystem access, so it
    // costs nothing and leaks nothing.
    if path::is_hidden(&segments) && !cfg.hidden {
        let (status, bytes) = send_not_found(&mut stream, &cfg, &request).await;
        log_access(peer, &request.method, &logged_path, status, bytes);
        return;
    }

    // 7. contain — the syscall tier's containment walk and its symlink
    // refusal (default mode) or canonicalize-and-check (`--follow-symlinks`).
    let Some(resolved) = fs::contain(&cfg.root, &segments, cfg.follow_symlinks).await else {
        let (status, bytes) = send_not_found(&mut stream, &cfg, &request).await;
        log_access(peer, &request.method, &logged_path, status, bytes);
        return;
    };

    // 8. metadata: a directory goes to step 9, anything else to step 10 —
    // "anything else" includes a fifo, a socket or a device node, which
    // `fs::open_regular` refuses on its own.
    let metadata = match tokio::fs::metadata(&resolved).await {
        Ok(metadata) => metadata,
        Err(_err) => {
            let (status, bytes) = send_not_found(&mut stream, &cfg, &request).await;
            log_access(peer, &request.method, &logged_path, status, bytes);
            return;
        }
    };

    if metadata.is_dir() {
        // 9. directory.
        if !raw_path.ends_with('/') {
            let mut location = String::from("/");
            location.push_str(
                &segments
                    .iter()
                    .map(|segment| listing::encode_segment(segment))
                    .collect::<Vec<_>>()
                    .join("/"),
            );
            location.push('/');
            match respond(
                &mut stream,
                301,
                "text/plain",
                0,
                vec![Header {
                    name: "Location",
                    value: location.as_str(),
                }],
            )
            .await
            {
                Ok(()) => log_access(peer, &request.method, &logged_path, 301, 0),
                Err(_err) => {
                    // The response-splitting lock refused the `Location` it
                    // was just given — unreachable in practice, since
                    // `encode_segment` already produces printable ASCII, but
                    // this is the defensive answer rather than a half
                    // response or a panic.
                    let body = b"could not build the redirect\n";
                    send(&mut stream, 500, "text/plain", body, vec![]).await;
                    log_access(peer, &request.method, &logged_path, 500, body.len() as u64);
                }
            }
            return;
        }

        if let Some((file, len)) = fs::open_regular(&resolved.join("index.html")).await {
            let status = serve_file(
                &mut stream,
                &request,
                mime::content_type("index.html"),
                file,
                len,
            )
            .await;
            log_access(peer, &request.method, &logged_path, status, len);
            return;
        }

        if cfg.listing {
            let entries = read_listing(&resolved, cfg.hidden)
                .await
                .unwrap_or_default();
            let mut prefix = String::from("/");
            prefix.push_str(&segments.join("/"));
            if !prefix.ends_with('/') {
                prefix.push('/');
            }
            let html = listing::render(&prefix, &entries);
            send(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                html.as_bytes(),
                vec![],
            )
            .await;
            log_access(peer, &request.method, &logged_path, 200, html.len() as u64);
            return;
        }

        let (status, bytes) = send_not_found(&mut stream, &cfg, &request).await;
        log_access(peer, &request.method, &logged_path, status, bytes);
        return;
    }

    // 10. file.
    let Some((file, len)) = fs::open_regular(&resolved).await else {
        let (status, bytes) = send_not_found(&mut stream, &cfg, &request).await;
        log_access(peer, &request.method, &logged_path, status, bytes);
        return;
    };
    let content_type = mime::content_type(&logged_path);
    let status = serve_file(&mut stream, &request, content_type, file, len).await;
    log_access(peer, &request.method, &logged_path, status, len);
}

/// Answers a 404 — or, if `cfg.spa` is set, the method is `GET`/`HEAD`, and
/// the request's `Accept` header names `text/html`, serves the docroot's
/// `index.html` with a 200 instead (decision 10). Every 404 this worker
/// would otherwise answer funnels through here, so the one-`Accept`-header
/// gate lives in exactly one place rather than being wired into some
/// refusals and forgotten at others (Step 6.4's mutation 3 is precisely
/// this gate going missing at one call site).
///
/// Returns the status actually answered and the number of body bytes, for
/// the caller's access-log line.
async fn send_not_found(
    stream: &mut TcpStream,
    cfg: &ServeConfig,
    request: &HttpRequest,
) -> (u16, u64) {
    if cfg.spa
        && matches!(request.method.as_str(), "GET" | "HEAD")
        && request
            .headers
            .get("accept")
            .is_some_and(|accept| accept.contains("text/html"))
        && let Some((file, len)) = fs::open_regular(&cfg.root.join("index.html")).await
    {
        let status = serve_file(stream, request, mime::content_type("index.html"), file, len).await;
        if status == 200 {
            return (200, len);
        }
    }
    let body = b"not found\n";
    send(stream, 404, "text/plain", body, vec![]).await;
    (404, body.len() as u64)
}

/// Streams `file` (already open, `len` bytes long per its own metadata) as
/// the body of a 200. The head goes through [`respond`], the same as every
/// other reply this worker sends; the body does not, because a streamed
/// file has no `&[u8]` to hand it.
///
/// `file.take(len)`, not a bare copy: `len` came from the open handle's own
/// metadata, and a file that grew between the open and this copy would
/// otherwise desync the framing this already declared in `Content-Length`.
/// A file that instead *shrank* still desyncs — fewer bytes than declared —
/// and `Connection: close` (every reply in this crate's `http` module
/// carries it) is what turns that into a client-visible truncation rather
/// than a hang.
///
/// `HEAD` never copies the body, only its length.
async fn serve_file(
    stream: &mut TcpStream,
    request: &HttpRequest,
    content_type: &str,
    file: tokio::fs::File,
    len: u64,
) -> u16 {
    match respond(stream, 200, content_type, len, vec![]).await {
        Ok(()) => {
            if request.method == "GET" {
                let mut body = file.take(len);
                let _ = tokio::io::copy(&mut body, stream).await;
            }
            200
        }
        Err(_err) => 500,
    }
}

/// Reads `dir`'s entries, dropping a leading-dot name unless `hidden` is
/// set — the same predicate `path::is_hidden` applies to a whole resolved
/// path, called here on one filename at a time, so the two cannot drift
/// apart. Directories sort before files, each group alphabetically; the
/// ordering is built here, by which `Vec` a name lands in and in what order
/// they are appended, rather than by inspecting a rendered entry's kind
/// after the fact.
async fn read_listing(dir: &std::path::Path, hidden: bool) -> std::io::Result<Vec<listing::Entry>> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut read_dir = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !hidden && path::is_hidden(std::slice::from_ref(&name)) {
            continue;
        }
        if entry.file_type().await?.is_dir() {
            dirs.push(name);
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    let mut entries = Vec::with_capacity(dirs.len() + files.len());
    entries.extend(dirs.into_iter().map(listing::Entry::dir));
    entries.extend(files.into_iter().map(listing::Entry::file));
    Ok(entries)
}

/// Writes exactly one response head. Every reply this worker sends funnels
/// through here, and this is the one place the nosniff header is appended —
/// decision 4's whole argument, since the reply that renders
/// attacker-influenced filenames into HTML (the listing) is one of the
/// replies that must carry it, same as every refusal and the streamed file.
/// The body — empty for a redirect, a small `&[u8]` for a refusal or the
/// listing, or a streamed file ([`serve_file`]) — is always the caller's to
/// write afterward.
///
/// # Errors
/// [`HttpError`] if the underlying write failed, or if a header value
/// carried a control byte (`write_head`'s response-splitting lock, decision
/// 5's response-splitting note) — the caller answers with nothing further,
/// since nothing has reached the stream yet.
async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    content_length: u64,
    extra_headers: Vec<Header<'_>>,
) -> Result<(), HttpError> {
    let mut headers = extra_headers;
    headers.push(Header {
        name: "X-Content-Type-Options",
        value: "nosniff",
    });
    http::write_head(stream, status, content_type, content_length, &headers).await
}

/// [`respond`] plus the in-memory body every reply except a streamed file
/// or an empty redirect carries. Writes nothing further if the head itself
/// was refused.
async fn send(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    headers: Vec<Header<'_>>,
) {
    if respond(stream, status, content_type, body.len() as u64, headers)
        .await
        .is_ok()
    {
        let _ = stream.write_all(body).await;
    }
}

/// One access-log line per request (decision 16), written to stdout —
/// `serve` runs as a registered sheep, so `shep bleats <name>` is this log
/// with no new plumbing.
///
/// `path` is already the resolved path where resolution succeeded, or the
/// raw target where it did not — [`escape_for_log`] has already run on it
/// either way by the time this is called.
fn log_access(peer: Option<SocketAddr>, method: &str, path: &str, status: u16, bytes: u64) {
    let peer = peer.map_or_else(|| "-".to_string(), |addr| addr.to_string());
    println!("{peer} \"{method} {path}\" {status} {bytes}");
}

/// Escapes every byte outside printable ASCII (`0x20..=0x7e`) as `\xNN`.
///
/// Not decoration: the raw request target can carry any byte a client
/// sends, and an operator reading `shep bleats` in a terminal would
/// otherwise be handed a stranger's ANSI escape sequences.
fn escape_for_log(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if (0x20..=0x7e).contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("\\x{byte:02X}"));
        }
    }
    out
}

// `unix` because the server cases assert file modes on served files — guarantees the Windows tier
// deliberately makes differently, each argued at its own call site
// above. What Windows claims instead is covered by `tests/cli_e2e.rs`
// and by the real-flock verification in the Windows port's own notes;
// this module's unix coverage is unchanged.
#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use tokio::task::JoinHandle;

    use super::*;
    use crate::serve::auth;

    /// A running worker bound to an OS-assigned loopback port. Aborts its
    /// accept loop on drop, so a worker left listening past its own test
    /// cannot hold a port for the rest of the binary's run — the same
    /// shape as `dog::metrics`'s own `RunningDog`.
    struct RunningServer {
        addr: SocketAddr,
        handle: JoinHandle<()>,
    }

    impl RunningServer {
        fn addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl Drop for RunningServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// Creates a docroot at `root` (a child of the caller's own `TempDir` —
    /// every test below owns its outer guard directly, so nothing this tier
    /// writes lands outside one) holding `files`, each `(relative path,
    /// contents)`.
    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        std::fs::create_dir_all(root).unwrap();
        for (path, contents) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, contents).unwrap();
        }
    }

    /// A default [`ServeConfig`] over `root`, bound to an OS-assigned
    /// loopback port — never a fixed one, which is how a suite starts
    /// failing on a developer's machine for reasons unrelated to the change
    /// under test.
    fn config(root: &Path) -> ServeConfig {
        ServeConfig {
            root: std::fs::canonicalize(root).unwrap(),
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            spa: false,
            listing: false,
            hidden: false,
            auth: None,
            follow_symlinks: false,
            connection_deadline: CONNECTION_DEADLINE,
        }
    }

    /// The one credential pair every auth-enabled test below authenticates
    /// with.
    const TEST_USER: &str = "alice:s3cret";

    fn config_with_auth(root: &Path) -> ServeConfig {
        let creds_path = root
            .parent()
            .expect("root must sit inside the caller's own tempdir")
            .join("creds");
        std::fs::write(&creds_path, format!("{TEST_USER}\n")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut built = config(root);
        built.auth = Some(auth::load(&creds_path).unwrap());
        built
    }

    fn config_with_spa(root: &Path) -> ServeConfig {
        let mut built = config(root);
        built.spa = true;
        built
    }

    fn config_with_listing(root: &Path) -> ServeConfig {
        let mut built = config(root);
        built.listing = true;
        built
    }

    fn config_with_hidden(root: &Path) -> ServeConfig {
        let mut built = config(root);
        built.hidden = true;
        built
    }

    fn config_with_follow_symlinks(root: &Path) -> ServeConfig {
        let mut built = config(root);
        built.follow_symlinks = true;
        built
    }

    /// Binds `config.bind`, reads back the OS-assigned address, and serves
    /// it in the background with a real admission semaphore — the same cap
    /// production code uses, so a test of the cap needs no separate one.
    async fn serve_on_free_port(config: ServeConfig) -> RunningServer {
        let listener = TcpListener::bind(config.bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let handle = tokio::spawn(accept_forever(listener, Arc::new(config), semaphore));
        RunningServer { addr, handle }
    }

    /// One HTTP response, parsed just enough for these tests: a status
    /// code, lower-cased header names, and the body as text.
    #[derive(Debug)]
    struct Response {
        status: u16,
        headers: HashMap<String, String>,
        body: String,
    }

    fn parse_response(raw: &[u8]) -> Response {
        let text = String::from_utf8_lossy(raw);
        let mut halves = text.splitn(2, "\r\n\r\n");
        let head = halves.next().unwrap_or_default();
        let body = halves.next().unwrap_or_default().to_string();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        Response {
            status,
            headers,
            body,
        }
    }

    /// Sends one raw request (already a full, correctly terminated HTTP/1.1
    /// head) and returns the parsed response. Every step is wrapped in a
    /// generous timeout: a test that hangs here is a bug in the worker
    /// under test, not a reason to hang the suite (IR-46 — the timeout
    /// wraps the asynchronous call, never a synchronous one).
    async fn send_request(addr: SocketAddr, request: &str) -> Response {
        let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .expect("connect must not hang")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), stream.write_all(request.as_bytes()))
            .await
            .expect("write must not hang")
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("read must not hang")
            .unwrap();
        parse_response(&buf)
    }

    async fn get(addr: SocketAddr, target: &str) -> Response {
        send_request(
            addr,
            &format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        )
        .await
    }

    async fn get_with_accept(addr: SocketAddr, target: &str, accept: &str) -> Response {
        send_request(
            addr,
            &format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nAccept: {accept}\r\n\r\n"),
        )
        .await
    }

    async fn get_auth(addr: SocketAddr, target: &str) -> Response {
        send_request(
            addr,
            &format!(
                "GET {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {}\r\n\r\n",
                base64_encode(TEST_USER)
            ),
        )
        .await
    }

    async fn request_with_method(addr: SocketAddr, method: &str, target: &str) -> Response {
        send_request(
            addr,
            &format!("{method} {target} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        )
        .await
    }

    /// Standard base64 (RFC 4648), test-only: `auth`'s own decoder is
    /// private to its module, and production code here only ever answers a
    /// header, never builds one.
    fn base64_encode(input: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied();
            let b2 = chunk.get(2).copied();
            out.push(ALPHABET[(b0 >> 2) as usize] as char);
            out.push(ALPHABET[(((b0 << 4) | (b1.unwrap_or(0) >> 4)) & 0x3f) as usize] as char);
            out.push(match b1 {
                Some(b1) => {
                    ALPHABET[(((b1 << 2) | (b2.unwrap_or(0) >> 6)) & 0x3f) as usize] as char
                }
                None => '=',
            });
            out.push(match b2 {
                Some(b2) => ALPHABET[(b2 & 0x3f) as usize] as char,
                None => '=',
            });
        }
        out
    }

    /// The traversal table again, this time end to end over a real socket,
    /// so it covers the handler's ordering and not only the resolver.
    ///
    /// Each case asserts the exact status. `assert_ne!(status, 200)` would
    /// pass against a server that 500s on everything, which is the shape a
    /// security test fails in.
    #[tokio::test]
    async fn every_traversal_shape_is_refused_over_a_real_socket() {
        // One guard over everything: the docroot is a CHILD of the tempdir,
        // and the negative control is its sibling. `tree.path().parent()`
        // would be `/tmp` or `/var/folders/.../T` — shared, world-writable,
        // never cleaned up by the `TempDir` guard, and a predictable name
        // two concurrent runs of this suite would race on.
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(
            &root,
            &[
                ("index.html", "<h1>home</h1>"),
                ("assets/app.css", "body{}"),
            ],
        );
        std::fs::write(outer.path().join("secret.txt"), "nope").unwrap();
        let server = serve_on_free_port(config(&root)).await;

        // positive control first: the server serves.
        assert_eq!(get(server.addr(), "/index.html").await.status, 200);
        assert_eq!(get(server.addr(), "/assets/app.css").await.status, 200);

        for (target, want) in [
            ("/../secret.txt", 400),
            ("/../../etc/passwd", 400),
            ("/%2e%2e/secret.txt", 400),
            ("/..%2fsecret.txt", 400),
            ("/x%00.png", 400),
            ("/a%0d%0aSet-Cookie:%20x", 400),
            ("/C:/Windows/System32/config/SAM", 400),
            ("/etc/passwd", 404),
            ("/nope.txt", 404),
            ("/.env", 404),
        ] {
            let response = get(server.addr(), target).await;
            assert_eq!(response.status, want, "{target} answered {response:?}");
            assert!(!response.body.contains("nope"), "{target} leaked the file");
            assert_eq!(
                response.headers["x-content-type-options"], "nosniff",
                "{target}: every response carries it, refusals included"
            );
        }
    }

    /// fails if a symlink out of the docroot is served over the socket —
    /// the handler's use of `contain`, not the pure function's own test.
    #[tokio::test]
    async fn a_symlink_out_of_the_docroot_is_a_404_and_not_a_body() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("ok.txt", "served")]);
        std::fs::write(outer.path().join("secret.txt"), "nope").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("escape.txt"))
            .unwrap();
        let server = serve_on_free_port(config(&root)).await;

        assert_eq!(
            get(server.addr(), "/ok.txt").await.status,
            200,
            "positive control"
        );

        let response = get(server.addr(), "/escape.txt").await;
        assert_eq!(response.status, 404);
        assert!(!response.body.contains("nope"), "{response:?}");
    }

    /// fails if the follow-symlinks flag on `ServeConfig` does not actually
    /// reach `fs::contain` through the worker — Task 3 already pins
    /// `contain`'s own behavior;
    /// this is the wiring between the flag on `ServeConfig` and the
    /// function call, over a real socket, on the exact deploy layout Rin's
    /// ruling names.
    #[tokio::test]
    async fn a_symlinked_deploy_layout_is_served_only_with_follow_symlinks() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(
            &root,
            &[("releases/2026-08-15/index.html", "<h1>home</h1>")],
        );
        std::os::unix::fs::symlink(root.join("releases/2026-08-15"), root.join("current")).unwrap();

        let refusing = serve_on_free_port(config(&root)).await;
        assert_eq!(
            get(refusing.addr(), "/current/index.html").await.status,
            404
        );

        let following = serve_on_free_port(config_with_follow_symlinks(&root)).await;
        let response = get(following.addr(), "/current/index.html").await;
        assert_eq!(response.status, 200);
        assert!(response.body.contains("<h1>home</h1>"));
    }

    /// fails if `--follow-symlinks` is mistaken for "trust every path"
    /// instead of "permit a symlink component, still enforce containment".
    /// A symlink that leaves the docroot must still 404 with the flag on —
    /// the canonicalize fallback (decision 5) is a different check, not the
    /// absence of one.
    #[tokio::test]
    async fn follow_symlinks_does_not_let_a_symlink_escape_the_root() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("index.html", "<h1>home</h1>")]);
        std::fs::write(outer.path().join("secret.txt"), "nope").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("escape.txt"))
            .unwrap();

        let server = serve_on_free_port(config_with_follow_symlinks(&root)).await;
        let response = get(server.addr(), "/escape.txt").await;
        assert_eq!(response.status, 404);
        assert!(
            !response.body.contains("nope"),
            "the escape must not be served"
        );
    }

    /// fails if auth is checked after path resolution. An unauthenticated
    /// client must not be able to tell a refused traversal (400) from a
    /// missing file (404) apart — that difference maps the filesystem.
    #[tokio::test]
    async fn an_unauthenticated_request_is_401_whatever_the_path_says() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("index.html", "<h1>home</h1>")]);
        std::fs::write(outer.path().join("secret.txt"), "nope").unwrap();
        let server = serve_on_free_port(config_with_auth(&root)).await;

        for target in ["/index.html", "/../secret.txt", "/nope.txt"] {
            let response = get(server.addr(), target).await;
            assert_eq!(response.status, 401, "{target}");
            assert!(
                response.headers.contains_key("www-authenticate"),
                "{target}"
            );
        }
        // positive control: with the credential, the same three answer
        // 200/400/404, not 401.
        assert_eq!(get_auth(server.addr(), "/index.html").await.status, 200);
        assert_eq!(get_auth(server.addr(), "/../secret.txt").await.status, 400);
        assert_eq!(get_auth(server.addr(), "/nope.txt").await.status, 404);
    }

    /// fails if a POST is served, or if the 405 forgets to say what is
    /// allowed.
    #[tokio::test]
    async fn a_method_other_than_get_or_head_is_405_with_an_allow_header() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("index.html", "<h1>home</h1>")]);
        let server = serve_on_free_port(config(&root)).await;

        let response = request_with_method(server.addr(), "POST", "/index.html").await;
        assert_eq!(response.status, 405);
        assert_eq!(response.headers["allow"], "GET, HEAD");
        assert!(!response.body.contains("<h1>home</h1>"), "{response:?}");
    }

    /// fails if a HEAD grows a body, or loses its Content-Length.
    #[tokio::test]
    async fn a_head_carries_the_length_and_no_body() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("index.html", "<h1>home</h1>")]);
        let server = serve_on_free_port(config(&root)).await;

        let response = request_with_method(server.addr(), "HEAD", "/index.html").await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.headers["content-length"],
            "<h1>home</h1>".len().to_string()
        );
        assert!(response.body.is_empty(), "{response:?}");
    }

    /// fails if a directory without a trailing slash is served in place,
    /// which breaks every relative link in the page it serves.
    #[tokio::test]
    async fn a_directory_without_a_trailing_slash_redirects_to_one() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("docs/index.html", "<h1>docs</h1>")]);
        let server = serve_on_free_port(config(&root)).await;

        let response = get(server.addr(), "/docs").await;
        assert_eq!(response.status, 301);
        assert_eq!(response.headers["location"], "/docs/");
    }

    /// fails if listing is on by default. Off is the decision (decision 9);
    /// a default that enumerates filenames is the kind of default nobody
    /// revisits.
    #[tokio::test]
    async fn a_directory_with_no_index_is_404_unless_listing_is_on() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("empty/.keep", "")]);

        let plain = serve_on_free_port(config(&root)).await;
        assert_eq!(get(plain.addr(), "/empty/").await.status, 404);

        let listing_on = serve_on_free_port(config_with_listing(&root)).await;
        let response = get(listing_on.addr(), "/empty/").await;
        assert_eq!(response.status, 200);
        assert!(response.body.contains("Index of"), "{response:?}");
    }

    /// fails if the SPA fallback fires for an asset request. A missing
    /// `/assets/app.js` must 404, not answer HTML with a 200 — the browser
    /// error that produces names a script type and never the missing file.
    #[tokio::test]
    async fn the_spa_fallback_serves_index_for_navigations_and_404s_for_assets() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("index.html", "<h1>home</h1>")]);
        let server = serve_on_free_port(config_with_spa(&root)).await;

        let nav = get_with_accept(server.addr(), "/deep/link", "text/html").await;
        assert_eq!(nav.status, 200);
        assert!(nav.body.contains("<h1>home</h1>"));

        let asset = get_with_accept(server.addr(), "/assets/missing.js", "*/*").await;
        assert_eq!(asset.status, 404);
    }

    /// fails if `nosniff` is dropped, or if the MIME table is not reaching
    /// the response.
    #[tokio::test]
    async fn a_css_file_is_served_as_css_with_nosniff() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("app.css", "body{}")]);
        let server = serve_on_free_port(config(&root)).await;

        let response = get(server.addr(), "/app.css").await;
        assert_eq!(response.status, 200);
        assert_eq!(response.headers["content-type"], "text/css; charset=utf-8");
        assert_eq!(response.headers["x-content-type-options"], "nosniff");
        assert_eq!(response.body, "body{}");
    }

    /// fails if a body larger than any buffer is truncated or read whole
    /// into memory. 2 MiB is enough to cross every buffer in this path and
    /// small enough to write in a test.
    #[tokio::test]
    async fn a_file_larger_than_the_buffers_is_served_whole() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        std::fs::create_dir_all(&root).unwrap();
        let big = "a".repeat(2 * 1024 * 1024);
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let server = serve_on_free_port(config(&root)).await;

        let response = get(server.addr(), "/big.txt").await;
        assert_eq!(response.status, 200);
        assert_eq!(response.headers["content-length"], big.len().to_string());
        assert_eq!(response.body.len(), big.len());
    }

    /// fails if a dotfile is served, or if `--hidden` stops serving one.
    /// Both halves in one test: `shep serve .` in a repo checkout
    /// publishing `.env` is the failure this refusal exists for, and
    /// `.well-known/acme-challenge/x` is the one real reason the flag
    /// exists rather than a hard ban.
    #[tokio::test]
    async fn a_dotfile_is_404_unless_hidden_is_set() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(
            &root,
            &[
                (".env", "SECRET=1"),
                (".git/config", "[core]"),
                ("index.html", "<h1>home</h1>"),
                (".well-known/acme-challenge/x", "token"),
            ],
        );

        let plain = serve_on_free_port(config(&root)).await;
        assert_eq!(get(plain.addr(), "/.env").await.status, 404);
        assert_eq!(get(plain.addr(), "/.git/config").await.status, 404);
        assert_eq!(
            get(plain.addr(), "/index.html").await.status,
            200,
            "positive control"
        );

        let hidden = serve_on_free_port(config_with_hidden(&root)).await;
        assert_eq!(
            get(hidden.addr(), "/.well-known/acme-challenge/x")
                .await
                .status,
            200
        );
    }

    /// fails if a listing names a file the server will not serve. A listing
    /// that prints `.env` and then 404s on it has leaked the filename,
    /// which is the whole reason listing is off by default. Also the one
    /// response that renders attacker-influenced text into HTML, so
    /// `nosniff` is asserted here too.
    #[tokio::test]
    async fn a_listing_omits_hidden_entries_and_carries_nosniff() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[(".env", "SECRET=1"), ("app.js", "console.log(1)")]);
        let server = serve_on_free_port(config_with_listing(&root)).await;

        let response = get(server.addr(), "/").await;
        assert_eq!(response.status, 200);
        assert!(!response.body.contains(".env"), "{response:?}");
        assert!(response.body.contains("app.js"), "{response:?}");
        assert_eq!(response.headers["x-content-type-options"], "nosniff");
    }

    /// fails if a directory whose name is not ASCII answers 500. The
    /// redirect's `Location` is built from the resolved segments and must
    /// be percent-encoded through the same function the listing hrefs use —
    /// unencoded, the header value carries bytes `write_head` refuses, and
    /// the operator gets a 500 on a directory that exists.
    #[tokio::test]
    async fn a_directory_with_a_non_ascii_name_redirects_rather_than_500ing() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("документы/index.html", "<h1>docs</h1>")]);
        let server = serve_on_free_port(config(&root)).await;

        let response = get(server.addr(), "/документы").await;
        assert_eq!(response.status, 301);
        assert_eq!(
            response.headers["location"],
            "/%D0%B4%D0%BE%D0%BA%D1%83%D0%BC%D0%B5%D0%BD%D1%82%D1%8B/"
        );
    }

    /// fails if there is no ceiling on concurrent connections. Opens
    /// `MAX_CONNECTIONS` sockets and holds every one of them open (the
    /// server is left mid-`read_request`, waiting on a request that never
    /// arrives, for its own `READ_TIMEOUT` — the semaphore permit for each
    /// is held for exactly as long as that task is alive), then asserts a
    /// connection past the cap is closed within a fraction of a second
    /// rather than held. Without the semaphore this test does not fail by
    /// hanging forever — it fails by the deadline below actually elapsing,
    /// which is the production symptom in miniature: a connection that
    /// should have been refused was accepted and left to wait instead.
    #[tokio::test]
    async fn connections_beyond_the_cap_are_closed_rather_than_queued_forever() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        write_tree(&root, &[("index.html", "<h1>home</h1>")]);
        let server = serve_on_free_port(config(&root)).await;

        let mut held = Vec::with_capacity(MAX_CONNECTIONS);
        for _ in 0..MAX_CONNECTIONS {
            let stream =
                tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(server.addr()))
                    .await
                    .expect("connect must not hang")
                    .unwrap();
            held.push(stream);
        }
        // Give every handler task a moment to actually reach its own
        // `read_request` call (and so acquire its permit) before the
        // one-over-the-cap connection below is attempted.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut over_cap =
            tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(server.addr()))
                .await
                .expect("connect must not hang")
                .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_millis(500), over_cap.read_to_end(&mut buf))
            .await
            .expect("a connection beyond the cap must close promptly rather than hang")
            .unwrap();
        assert!(
            buf.is_empty(),
            "nothing should be written to a refused connection: {buf:?}"
        );

        drop(held);
    }

    /// fails if a connection that stops reading mid-body is held for the
    /// life of the process.
    ///
    /// On a REAL clock with a deliberately tiny deadline, not a paused one.
    /// The paused-clock version of this test failed about one run in three:
    /// tokio auto-advances a paused clock to the earliest pending timer
    /// whenever the runtime goes idle, so the assertion held only if the
    /// server task had already registered its own deadline timer by the
    /// time the runtime first idled. That was a race against real socket
    /// IO -- a connect, a write, and an 8 MiB body -- and when it lost, the
    /// test's own guard was the earliest timer and fired instead. An
    /// earlier attempt to fix it removed a bounded read for the same
    /// reason and left the underlying race in place.
    ///
    /// Nothing here waits on virtual time, so there is no ordering to race.
    #[tokio::test]
    async fn a_connection_that_stops_reading_is_dropped_at_the_deadline() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("www");
        // Larger than an ordinary kernel send buffer, so the server's write
        // genuinely blocks on backpressure once the client stops reading,
        // rather than completing into the buffer regardless.
        let big = "a".repeat(8 * 1024 * 1024);
        write_tree(&root, &[("big.txt", &big)]);
        let mut cfg = config(&root);
        // Short enough to wait out in a test, long enough that a loaded
        // machine still gets the connection established and the write
        // started before it fires.
        cfg.connection_deadline = Duration::from_millis(300);
        let server = serve_on_free_port(cfg).await;

        let mut stream = TcpStream::connect(server.addr()).await.unwrap();
        stream
            .write_all(b"GET /big.txt HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        // Never read anything: connect and send only. The deadline must
        // close the connection, which the client sees as EOF. The guard is
        // generous because it exists to turn a hang into a failure, not to
        // measure the deadline.
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut rest))
            .await
            .expect("the deadline must close the connection rather than hang forever")
            .unwrap();
    }
}
