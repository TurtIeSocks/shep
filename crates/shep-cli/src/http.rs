//! The little HTTP this binary needs: [`read_request`], [`write_response`]
//! and [`write_head`].
//!
//! Hand-rolled rather than pulled from a crate, and the reason is the whole
//! dependency tree: this workspace carries no HTTP server and does not want
//! one for a loopback endpoint serving one path, and — Rin's 2026-08-15
//! ruling on `serve` — no more of one for a genuinely simple static file
//! server over code this crate already owns. What it needs is a request
//! line, a header map and a body — under a hundred lines against
//! `tokio::io`, with no TLS to get wrong because the metrics endpoint is
//! loopback by default and binding it wider is the operator's explicit act.
//! `shep-daemon/src/probes/os.rs`'s hand-rolled HTTP *client* probe made the
//! same call for the same reason; this is that call's server-side twin.
//!
//! Generic over [`AsyncRead`]/[`AsyncWrite`] rather than `TcpStream`, so a
//! test drives [`read_request`]/[`write_response`]/[`write_head`] over a
//! `tokio::io::duplex` pair with no socket at all. `dog::metrics` is the one
//! caller that binds a real [`tokio::net::TcpListener`] today.

use core::fmt;
use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Ceiling on a request's head. Generous for a real client, small enough
/// that a hostile one cannot grow a dog's memory with it.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Ceiling on a declared `content-length`. The metrics dog reads no bodies
/// at all; this exists so a test sink can, and so the ceiling is one number
/// rather than a per-caller decision.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// One HTTP/1.1 request, as much of it as a dog needs.
#[derive(Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method, uppercased as it arrived.
    pub method: String,
    /// The request target, path and query together.
    pub target: String,
    /// Header names lowercased; values trimmed.
    pub headers: BTreeMap<String, String>,
    /// The body, read to `content-length` and no further.
    pub body: Vec<u8>,
}

/// Why [`read_request`] or [`write_response`] failed.
///
/// `Debug` needs no redaction (unlike `DogRunError` one directory up): every
/// field here is a size or a fixed reason string, never a header value — and
/// a header value is where an `Authorization` would be. A derived `Debug`
/// stays safe to log.
#[derive(Debug)]
pub enum HttpError {
    /// The underlying read or write failed, or the peer closed the
    /// connection before a full request arrived.
    Io(std::io::Error),
    /// The bytes read do not parse as HTTP: no request line, or a header
    /// line with no colon. Carries a fixed reason, never the offending
    /// bytes.
    Malformed(&'static str),
    /// A declared size exceeded its ceiling: the request head past
    /// [`MAX_HEADER_BYTES`], or a declared `content-length` past
    /// [`MAX_BODY_BYTES`].
    TooLarge {
        /// What exceeded its ceiling: `"head"` or `"declared content-length"`.
        what: &'static str,
        /// The ceiling that was exceeded.
        limit: usize,
    },
    /// No request arrived within the caller's `read_timeout`.
    Timeout,
    /// A header name or value passed to [`write_head`] carried a byte outside
    /// the printable ASCII range (`0x20..=0x7e`) — a CR, an LF, or anything
    /// above the range such as DEL. Carries a fixed reason, never the
    /// offending name or value: unlike this enum's other variants, the value
    /// that trips this one can be attacker-controlled (a percent-decoded
    /// request path reflected into a `Location`), so the same rule that keeps
    /// this type's `Debug` safe to log — never a header value — applies here
    /// too. `HttpError` is private to this crate — `lib.rs` declares
    /// `mod http;`, not `pub mod http;` — so IR-20's `#[non_exhaustive]`
    /// question does not arise.
    ///
    /// `write_head`'s caller is `serve::worker`, which returns this variant
    /// for a response header it cannot write out safely.
    BadHeader {
        /// What was bad: `"a header name"` or `"a header value"`.
        what: &'static str,
    },
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "http i/o error: {err}"),
            Self::Malformed(reason) => write!(f, "malformed http request: {reason}"),
            Self::TooLarge { what, limit } => {
                write!(f, "{what} exceeded the {limit}-byte ceiling")
            }
            Self::Timeout => f.write_str("no request arrived within the read timeout"),
            Self::BadHeader { what } => {
                write!(f, "{what} carried a byte outside the printable ASCII range")
            }
        }
    }
}

impl core::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Malformed(_) | Self::TooLarge { .. } | Self::Timeout | Self::BadHeader { .. } => {
                None
            }
        }
    }
}

/// Reads one request off `stream`, bounded in both size and time.
///
/// Hand-rolled rather than pulled from a crate, and the reason is the whole
/// dependency tree: this workspace carries no HTTP server and does not want
/// one for a loopback endpoint serving one path. What it needs is a request
/// line, a header map and a body — under a hundred lines against
/// `tokio::io`, with no TLS to get wrong because the metrics endpoint is
/// loopback by default and binding it wider is the operator's explicit act.
///
/// Both bounds are load-bearing. `MAX_HEADER_BYTES` is what stops a peer
/// sending headers forever; `read_timeout` is what stops one that opens a
/// connection and says nothing from holding a task open. A metrics endpoint
/// is reachable by anything that can reach the port, which on a shared host
/// is more than the operator.
///
/// # Errors
/// - [`HttpError::Io`] — the read failed or the peer closed mid-request.
/// - [`HttpError::Malformed`] — no request line, or a header with no colon.
/// - [`HttpError::TooLarge`] — the head exceeded [`MAX_HEADER_BYTES`], or
///   the declared `content-length` exceeded [`MAX_BODY_BYTES`].
/// - [`HttpError::Timeout`] — the request did not arrive within
///   `read_timeout`.
pub async fn read_request<R: AsyncRead + Unpin>(
    stream: &mut R,
    read_timeout: Duration,
) -> Result<HttpRequest, HttpError> {
    match tokio::time::timeout(read_timeout, read_request_unbounded_time(stream)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(HttpError::Timeout),
    }
}

/// [`read_request`]'s body, split out so the timeout wraps exactly this and
/// nothing else.
async fn read_request_unbounded_time<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<HttpRequest, HttpError> {
    // `stream.take(N)` is what turns "a peer that never sends a line
    // terminator" into a bounded read: once the take's budget is spent,
    // the underlying `poll_read` reports EOF and `read_head`'s own length
    // check fires on that same call, rather than `read_until` waiting
    // forever for a `\n` that is never coming.
    let mut reader = BufReader::new(stream.take(MAX_HEADER_BYTES as u64 + 1));
    let head = read_head(&mut reader).await?;
    let (method, target, headers) = parse_head(&head)?;

    let body = if let Some(declared) = headers.get("content-length") {
        let len: usize = declared
            .parse()
            .map_err(|_err| HttpError::Malformed("content-length is not a number"))?;
        if len > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge {
                what: "declared content-length",
                limit: MAX_BODY_BYTES,
            });
        }
        // The head-reading ceiling above no longer applies to the body —
        // reuse the same `Take` (so any bytes already buffered past the
        // blank line are not lost) but reset its budget to exactly the
        // declared length, so a read past `content-length` is structurally
        // impossible rather than merely untried.
        reader.get_mut().set_limit(len as u64);
        let mut body = vec![0_u8; len];
        reader.read_exact(&mut body).await.map_err(HttpError::Io)?;
        body
    } else {
        Vec::new()
    };

    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

/// Reads the request head — start line through the terminating blank line —
/// one `\n`-delimited chunk at a time off `reader`, refusing growth past
/// [`MAX_HEADER_BYTES`]. `reader`'s own size ceiling ([`read_request_unbounded_time`]'s
/// `Take`) is what makes that refusal happen at all rather than hanging: see
/// this module's tests for the flood case this guards against.
async fn read_head<S: AsyncRead + Unpin>(reader: &mut BufReader<S>) -> Result<Vec<u8>, HttpError> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(HttpError::Io)?;
        if read == 0 {
            // The take's budget has not been exceeded (that case returns
            // below, on this same call, before a next iteration is ever
            // reached) — this is the peer genuinely closing the connection.
            return Err(HttpError::Io(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        head.extend_from_slice(&line);
        if head.len() > MAX_HEADER_BYTES {
            return Err(HttpError::TooLarge {
                what: "head",
                limit: MAX_HEADER_BYTES,
            });
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    Ok(head)
}

/// Parses a complete head (as [`read_head`] returns it) into the method,
/// target and header map [`HttpRequest`] carries.
fn parse_head(head: &[u8]) -> Result<(String, String, BTreeMap<String, String>), HttpError> {
    let mut lines = head.split(|&b| b == b'\n');
    let request_line = lines
        .next()
        .ok_or(HttpError::Malformed("no request line"))?;
    let request_line = strip_trailing_cr(request_line);
    let request_line = core::str::from_utf8(request_line)
        .map_err(|_err| HttpError::Malformed("request line is not valid utf-8"))?;

    let mut parts = request_line.splitn(3, ' ');
    let method = parts
        .next()
        .filter(|token| !token.is_empty())
        .ok_or(HttpError::Malformed("no method in the request line"))?;
    let target = parts
        .next()
        .filter(|token| !token.is_empty())
        .ok_or(HttpError::Malformed("no target in the request line"))?;

    let mut headers = BTreeMap::new();
    for line in lines {
        let line = strip_trailing_cr(line);
        if line.is_empty() {
            break;
        }
        let line = core::str::from_utf8(line)
            .map_err(|_err| HttpError::Malformed("header line is not valid utf-8"))?;
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpError::Malformed("header line has no colon"))?;
        headers.insert(name.trim().to_lowercase(), value.trim().to_string());
    }

    Ok((method.to_string(), target.to_string(), headers))
}

/// Strips one trailing `\r` left by splitting on `\n` alone; a bare `\n`
/// line (no `\r`) is returned unchanged.
fn strip_trailing_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Writes one response and nothing else: `Connection: close` on every
/// reply, so neither side has to reason about keep-alive, pipelining, or a
/// half-read body left in the buffer.
///
/// The status line's reason phrase is empty (`HTTP/1.1 200 `) rather than
/// looked up from a table — RFC 7230 §3.1.2 allows a zero-length
/// reason-phrase, and every caller in this workspace (a metrics scraper) reads
/// the status code, not the reason text.
///
/// # Errors
/// - The underlying write failed.
pub async fn write_response<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), HttpError> {
    let head = format!(
        "HTTP/1.1 {status} \r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(HttpError::Io)?;
    stream.write_all(body).await.map_err(HttpError::Io)?;
    Ok(())
}

/// One extra response header: a name and a value, both already final.
///
/// Built by `serve::worker`, which attaches these to a response before
/// `write_head` writes them out.
pub struct Header<'a> {
    /// The header name, written exactly as given.
    pub name: &'a str,
    /// The value, written exactly as given — refused if it carries a control
    /// byte, see [`write_head`].
    pub value: &'a str,
}

/// Writes a response head and stops, leaving the body to the caller.
///
/// [`write_response`] is still the right function when the whole body is
/// already a `&[u8]` — the metrics dog's exposition is exactly that. This one
/// exists for `serve`, which streams a file it has not read: `content_length`
/// comes from the file's metadata and the caller copies the bytes afterwards.
///
/// Every response carries `Connection: close`, the same as [`write_response`],
/// so a caller that writes fewer bytes than it declared closes the connection
/// and the client sees a truncated response rather than a hang.
///
/// # Errors
/// - [`HttpError::Io`] — the write failed.
/// - [`HttpError::BadHeader`] — a header name or value carries a byte outside
///   the printable ASCII range. A `Location` built from a request path is the
///   case this exists for: a percent-encoded CRLF that reached this far would
///   otherwise split the response and let a client inject headers of its own.
///   The caller answers 500; nothing is written to the stream first, so the
///   refusal cannot itself produce a malformed response.
pub async fn write_head<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    content_length: u64,
    headers: &[Header<'_>],
) -> Result<(), HttpError> {
    for header in headers {
        if has_control_byte(header.name) {
            return Err(HttpError::BadHeader {
                what: "a header name",
            });
        }
        if has_control_byte(header.value) {
            return Err(HttpError::BadHeader {
                what: "a header value",
            });
        }
    }

    let mut head = format!(
        "HTTP/1.1 {status} \r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\n"
    );
    for header in headers {
        head.push_str(header.name);
        head.push_str(": ");
        head.push_str(header.value);
        head.push_str("\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");

    stream
        .write_all(head.as_bytes())
        .await
        .map_err(HttpError::Io)
}

/// Whether `s` carries a byte outside the printable ASCII range
/// (`0x20..=0x7e`) — a CR, an LF, or anything above it such as DEL. Any of
/// these, written unescaped into a response head, splits it: a CRLF pair
/// starts a new header line, and a lone CR or LF is enough for some clients.
fn has_control_byte(s: &str) -> bool {
    s.bytes().any(|b| !(0x20..=0x7e).contains(&b))
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    /// fails if the body is read past `content-length`, or if a request with
    /// no body blocks waiting for one. The second half is what a bare
    /// `read_to_end` gets wrong, and it hangs rather than failing — which is
    /// why this test is bounded and why the timeout is a parameter at all.
    #[tokio::test]
    async fn a_request_is_read_to_its_declared_length_and_no_further() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client
            .write_all(
                b"POST /hook HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello-and-then-some",
            )
            .await
            .unwrap();
        let req = tokio::time::timeout(
            Duration::from_secs(5),
            read_request(&mut server, Duration::from_secs(1)),
        )
        .await
        .expect("read_request must not hang on a body it already has")
        .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/hook");
        assert_eq!(req.body, b"hello");
        assert_eq!(
            req.headers.get("content-length").map(String::as_str),
            Some("5")
        );
        assert_eq!(
            req.headers.get("host").map(String::as_str),
            Some("x"),
            "names lowercase"
        );
    }

    /// fails if a peer can grow the dog's memory by sending headers
    /// forever. The metrics endpoint is reachable by anything that can
    /// reach the port; on a shared host that is more than the operator.
    #[tokio::test]
    async fn a_head_past_the_ceiling_is_refused_rather_than_buffered() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let mut flood = b"GET / HTTP/1.1\r\n".to_vec();
        // `repeat_n`, not `repeat().take()` (brief's literal test text used
        // the latter): clippy's `manual_repeat_n` denies it under this
        // workspace's `-D warnings`, and the two are behaviorally identical.
        flood.extend(std::iter::repeat_n(b'x', MAX_HEADER_BYTES + 1));
        client.write_all(&flood).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(5),
                read_request(&mut server, Duration::from_secs(1))
            )
            .await
            .expect("the ceiling must fail, never hang")
            .unwrap_err(),
            HttpError::TooLarge { .. }
        ));
    }

    /// fails if a peer that connects and says nothing holds a task open.
    /// The tokio clock is paused, so this measures the timeout rather than
    /// waiting for it.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_says_nothing_is_dropped_at_the_timeout() {
        let (_client, mut server) = tokio::io::duplex(64);
        let err = read_request(&mut server, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(err, HttpError::Timeout), "{err:?}");
    }

    /// fails if `Connection: close` is dropped from the response. Without
    /// it a client is entitled to keep the connection open and wait for a
    /// second reply that never comes, and `curl 127.0.0.1:9615/metrics`
    /// hangs after printing the exposition.
    #[tokio::test]
    async fn every_response_closes_its_connection() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        write_response(&mut server, 200, "text/plain", b"ok")
            .await
            .unwrap();
        drop(server);
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        let response = String::from_utf8(buf).unwrap();
        assert!(response.contains("Connection: close\r\n"), "{response:?}");
        assert!(response.starts_with("HTTP/1.1 200 "), "{response:?}");
        assert!(response.ends_with("ok"), "{response:?}");
    }

    /// fails if a header value carrying CRLF is written to the stream —
    /// response splitting, reachable from a percent-encoded path in a
    /// `Location`.
    #[tokio::test]
    async fn a_header_value_with_a_control_byte_is_refused_before_anything_is_written() {
        for (name, value) in [
            ("Location", "/a\r\nSet-Cookie: x=1"), // the pair
            ("Location", "/a\rSet-Cookie: x=1"),   // a lone CR: enough on its own
            ("Location", "/a\nSet-Cookie: x=1"),   // a lone LF
            ("Location", "/a\u{7f}b"),             // DEL, above the control range
            ("X-Bad\r\nInjected", "ok"),           // the NAME is checked too
        ] {
            let (mut client, mut server) = tokio::io::duplex(4096);
            let err = write_head(&mut server, 301, "text/html", 0, &[Header { name, value }])
                .await
                .unwrap_err();
            assert!(
                matches!(err, HttpError::BadHeader { .. }),
                "{name}: {err:?}"
            );
            drop(server);
            let mut buf = Vec::new();
            client.read_to_end(&mut buf).await.unwrap();
            assert!(
                buf.is_empty(),
                "{name}: nothing may reach the stream: {buf:?}"
            );
        }
    }

    /// fails if the extra headers are dropped, or if the declared length stops
    /// matching what the caller was told to write.
    #[tokio::test]
    async fn a_head_carries_its_extra_headers_and_its_declared_length() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        write_head(
            &mut server,
            200,
            "text/css",
            42,
            &[Header {
                name: "X-Content-Type-Options",
                value: "nosniff",
            }],
        )
        .await
        .unwrap();
        drop(server);
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        let head = String::from_utf8(buf).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 "), "{head:?}");
        assert!(head.contains("Content-Length: 42\r\n"), "{head:?}");
        assert!(head.contains("Content-Type: text/css\r\n"), "{head:?}");
        assert!(
            head.contains("X-Content-Type-Options: nosniff\r\n"),
            "{head:?}"
        );
        assert!(head.contains("Connection: close\r\n"), "{head:?}");
        assert!(
            head.ends_with("\r\n\r\n"),
            "a head and nothing else: {head:?}"
        );
    }
}
