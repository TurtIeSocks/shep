//! A bounded, redirect-refusing GET over `tokio-rustls`: [`get`], plus the
//! URL parsing ([`parse_url`]) and TLS setup ([`tls_connector`]) it shares
//! with `dog::bark::sinks`. An HTTP client, where `crate::http` is a
//! hand-rolled, TLS-free server. Either scheme is accepted; a caller
//! wanting only `https://` enforces that itself. A URL carrying
//! credentials before the host (`user@`, `user:pass@`) is refused.
//!
//! [`get`] refuses, in this order: a 3xx naming a `Location`; any other
//! non-2xx; any `Transfer-Encoding`, since it reads exactly
//! `Content-Length` bytes; a `Content-Length` absent, unparseable, or
//! contradicted by a second one; and one above the caller's `limit`,
//! rechecked as the body arrives. The head has its own cap,
//! [`MAX_HEADER_BYTES`]. No `Accept-Encoding` is sent.
use core::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::terminal_safe;

/// The most this module will read before the blank line that ends a
/// response's header block, status line included.
///
/// 64 KiB, within an order of magnitude of what nginx, Apache and Go's
/// `net/http` allow, and far more than any real response needs: the live
/// index answers in about 300 bytes of headers. Nothing else bounds a peer
/// that never sends a newline.
///
/// Not the caller's `limit`, which is a body budget.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

/// A URL, parsed into what [`get`] needs to reach it.
///
/// `Debug` is REDACTED (IR-41), and `path` is the field that needs it: a
/// Discord or Slack webhook URL is a bearer credential and carries its
/// token as a path segment, which is why
/// [`Sink`](crate::dog::bark::sinks::Sink) redacts its own `Debug` too. A
/// `Target` printed into a log, a panic message or an error chain must not
/// hand that token to whoever reads the log.
///
/// `host` needs no redaction: [`parse_url`] refuses a `user@` or
/// `user:pass@` authority rather than folding it into `host`.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// `true` for `https://`, `false` for `http://`.
    pub https: bool,
    /// The host, without a port.
    pub host: String,
    /// The port: the URL's own, or the scheme's default (443/80).
    pub port: u16,
    /// The request path, always starting with `/`.
    pub path: String,
}

/// Manual: a derived `Debug` would print `path` in full, and `path` is
/// where a webhook's own credential lives. Every `Target` collapses to its
/// scheme, host and port, with `path` withheld.
impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Target {{ https: {}, host: {:?}, port: {}, path: <redacted> }}",
            self.https, self.host, self.port
        )
    }
}

/// Why [`parse_url`] or [`get`] failed.
///
/// `Debug` needs no redaction: every field is a URL this module was given
/// or redirected to, an HTTP status, a byte count, or an OS error.
#[derive(Debug)]
pub enum FetchError {
    /// `url` did not parse as an absolute `http://`/`https://` URL. Carries
    /// a human-readable reason, never the raw bytes of a malformed input.
    ///
    /// The reason quotes `url` only through [`url_for_message`], which
    /// withholds anything holding an `@`. That is the backstop under
    /// [`url_carries_credentials`], which reads an authority and so can
    /// misread a url far enough off the grammar. An authority that really
    /// does carry `user@` or `user:pass@` gets its own reason,
    /// [`CREDENTIALS_REFUSAL`].
    Url(String),
    /// The connection failed, the TLS handshake failed, or the response
    /// was not well-formed HTTP: no parseable status line, a header block
    /// that never reached its terminating blank line, or a `Content-Length`
    /// that was missing, not a number, or contradicted by a second
    /// `Content-Length` header on the same response.
    Transport(std::io::Error),
    /// The response was outside 2xx (including a 3xx with no `Location` to
    /// report as a [`Self::Redirect`]).
    Status(u16),
    /// The response was a 3xx naming a `Location`, refused rather than
    /// followed.
    Redirect {
        /// The `Location` header's value, [`crate::terminal_safe::sanitise`]d
        /// at capture: a string the host chose that this `Display` prints
        /// to a terminal.
        location: String,
    },
    /// The response carried a `Transfer-Encoding` header. This client reads
    /// exactly `Content-Length` bytes, so a chunked body would be
    /// misparsed rather than decoded.
    Chunked,
    /// The declared or actual body size exceeded the caller's limit.
    TooLarge {
        /// The limit the caller passed to [`get`].
        limit: usize,
    },
    /// The status line and header block together ran past
    /// [`MAX_HEADER_BYTES`] without reaching the blank line that ends them.
    /// Separate from [`Self::TooLarge`], which is the caller's body budget.
    HeadersTooLarge {
        /// [`MAX_HEADER_BYTES`], named here so the message can state it.
        limit: usize,
    },
    /// `timeout` elapsed before the exchange finished.
    Timeout,
    /// The peer closed the connection before `Content-Length` bytes
    /// arrived.
    Truncated {
        /// The `Content-Length` the response declared.
        expected: usize,
        /// How many bytes actually arrived before the peer closed.
        got: usize,
    },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(reason) => write!(f, "not a fetchable url: {reason}"),
            Self::Transport(source) => write!(f, "fetch failed: {source}"),
            Self::Status(code) => write!(f, "fetch answered {code}"),
            Self::Redirect { location } => write!(f, "fetch was redirected to {location}"),
            Self::Chunked => {
                write!(
                    f,
                    "fetch response used transfer-encoding, which this client refuses to decode"
                )
            }
            Self::TooLarge { limit } => write!(f, "fetch response exceeded the {limit}-byte limit"),
            Self::HeadersTooLarge { limit } => {
                write!(f, "fetch response headers exceeded the {limit}-byte limit")
            }
            Self::Timeout => write!(f, "fetch timed out"),
            Self::Truncated { expected, got } => {
                write!(
                    f,
                    "fetch response was truncated: expected {expected} bytes, got {got}"
                )
            }
        }
    }
}

impl core::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Transport(source) => Some(source),
            Self::Url(_)
            | Self::Status(_)
            | Self::Redirect { .. }
            | Self::Chunked
            | Self::TooLarge { .. }
            | Self::HeadersTooLarge { .. }
            | Self::Timeout
            | Self::Truncated { .. } => None,
        }
    }
}

impl From<std::io::Error> for FetchError {
    fn from(source: std::io::Error) -> Self {
        Self::Transport(source)
    }
}

/// The reason [`parse_url`] and [`super::dog::bark::sinks`] both give for
/// a URL carrying credentials. It names no URL, which is the point: the
/// text being refused is the text holding the password.
pub const CREDENTIALS_REFUSAL: &str = concat!(
    "credentials before the host (`user@` or `user:pass@`) are not supported; ",
    "the url is not echoed, since it carries one"
);

/// How `url` may be named in a message: itself, or a fixed placeholder.
///
/// Deliberately blunter than [`url_carries_credentials`], and deliberately
/// the only rule any message consults. That predicate reads an authority,
/// so it can say precisely why a sink was refused; this one asks only
/// whether printing the text might print a secret, and an `@` anywhere is
/// enough to decline.
///
/// Two rules for one question drift, and did: [`parse_url`] withheld
/// `file:///etc/user:pw@host` on its own `@` test while
/// [`available_dogs`](crate::commands::query::available_dogs) printed that
/// same url in the sentence around it, because it asked the predicate
/// instead.
pub fn url_for_message(url: &str) -> &str {
    if url.contains('@') {
        "a url that may carry credentials"
    } else {
        url
    }
}

/// Where `rest`, an authority followed by whatever came after it, stops
/// being the authority.
///
/// `/`, `?` and `#` all end it. Splitting on `/` alone reads
/// `example.com?contact=alice@example.com` as one authority, which makes
/// [`url_carries_credentials`] call an `@` in a query a credential.
fn authority_of(rest: &str) -> &str {
    match rest.find(['/', '?', '#']) {
        Some(i) => &rest[..i],
        None => rest,
    }
}

/// Whether `url`'s authority carries a `user@` or `user:pass@` prefix.
///
/// The rule [`parse_url`] refuses on, separated out so a caller holding a
/// URL it has not parsed yet can refuse the same shape at config-load time
/// rather than at first use.
///
/// Deliberately blind to the scheme, and to whether there is one at all.
/// Keying this on `http://`/`https://` would have answered `false` for
/// `ftp://user:pass@host/` and for `HTTPS://user:pass@host/`, which
/// [`parse_url`] then refuses on the SCHEME instead, in a message that
/// quotes the whole url and hands the password back. A url this cannot
/// parse is exactly the one whose refusal must not echo it.
///
/// The cost is that `mailto:someone@example.com` reads as credentials.
/// It is refused either way, and a wrong reason on a url nothing here can
/// fetch is cheaper than a right one that prints a password.
pub fn url_carries_credentials(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_scheme, rest)| rest);
    // A scheme-relative url has an authority with no scheme in front of
    // it, so there is no `://` to split on and the authority would
    // otherwise read as the empty string before the first `/`.
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    authority_of(rest).contains('@')
}

/// Parses `url` into a [`Target`] naming where [`get`] should connect.
///
/// Hand-rolled: a fetch target is never more than a scheme, a host, an
/// optional port and a path.
///
/// Credentials before the host are refused rather than stripped: nothing
/// downstream of here sends an `Authorization` header, so a `user:pass@`
/// prefix could only ever be silently discarded or silently sent as part
/// of a hostname. `ProbeTarget::parse` refuses the same shape for the same
/// reason.
///
/// The authority ends at the first `/`, `?` or `#`. A query belongs to
/// the path from there; a fragment is dropped, being the client's own and
/// having no place in a request target.
///
/// # Errors
/// - [`FetchError::Url`] if `url` does not start with `http://` or
///   `https://`, carries a `user@` or `user:pass@` authority, names a
///   non-numeric port, or names no host. No such message quotes a `url`
///   holding an `@`; see [`url_for_message`].
pub fn parse_url(url: &str) -> Result<Target, FetchError> {
    // Ahead of the host/port split, which trusts the last colon: without
    // this, `user:pass@host:8443` parses to the host `user:pass@host` and
    // `user:pass@host` to the host `user` and the port `pass@host`, so a
    // password reaches either a `Target` field or, through that split's
    // own refusal, an error message quoting the whole URL.
    if url_carries_credentials(url) {
        return Err(FetchError::Url(CREDENTIALS_REFUSAL.to_owned()));
    }
    let (https, rest) = match url.strip_prefix("https://") {
        Some(rest) => (true, rest),
        None => match url.strip_prefix("http://") {
            Some(rest) => (false, rest),
            None => {
                return Err(FetchError::Url(format!(
                    "{} does not start with http:// or https://",
                    url_for_message(url)
                )));
            }
        },
    };
    let authority = authority_of(rest);
    let remainder = &rest[authority.len()..];
    // A fragment is the client's own: RFC 3986 gives it no meaning to a
    // server and RFC 7230's origin-form target has no field for it, so
    // carrying it into `path` would put it on the wire in the request
    // line. A query is the opposite and belongs there.
    let remainder = remainder.split('#').next().unwrap_or(remainder);
    // A `?` remainder has no leading `/` of its own and an origin-form
    // target needs one, as does an absent path.
    let path = match remainder.chars().next() {
        Some('/') => remainder.to_owned(),
        Some(_) => format!("/{remainder}"),
        None => "/".to_owned(),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse().map_err(|_err| {
                FetchError::Url(format!("{} has a non-numeric port", url_for_message(url)))
            })?,
        ),
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(FetchError::Url(format!(
            "{} has no host",
            url_for_message(url)
        )));
    }
    Ok(Target {
        https,
        host: host.to_string(),
        port,
        path,
    })
}

/// The TLS connector every `https://` connection this crate makes shares,
/// built once on first use: a fresh one per connection would re-walk the
/// root store and re-derive the cipher suite set every time.
pub fn tls_connector() -> &'static tokio_rustls::TlsConnector {
    static CONNECTOR: std::sync::LazyLock<tokio_rustls::TlsConnector> =
        std::sync::LazyLock::new(|| {
            let roots = tokio_rustls::rustls::RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            );
            let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
            let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring's default cipher suites cover rustls's own default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            tokio_rustls::TlsConnector::from(Arc::new(config))
        });
    &CONNECTOR
}

/// Fetches `target` with a single GET, refusing anything but a plain 2xx
/// body no larger than `limit` bytes, bounded end to end by `timeout`.
///
/// # Errors
/// Every [`FetchError`] variant can come out of this call except
/// [`FetchError::Url`], which only [`parse_url`] produces. The module doc
/// lists the refusal order.
pub async fn get(target: &Target, limit: usize, timeout: Duration) -> Result<Vec<u8>, FetchError> {
    match tokio::time::timeout(timeout, get_inner(target, limit)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(FetchError::Timeout),
    }
}

/// Connects to `target`, over TLS when `target.https`, and runs the
/// write/read exchange over whichever stream results.
async fn get_inner(target: &Target, limit: usize) -> Result<Vec<u8>, FetchError> {
    let request = build_get_request(target);
    let tcp = TcpStream::connect((target.host.as_str(), target.port)).await?;
    if target.https {
        let domain = ServerName::try_from(target.host.clone())
            .map_err(|source| FetchError::Transport(std::io::Error::other(source)))?;
        let tls = tls_connector()
            .connect(domain, tcp)
            .await
            .map_err(peer_transport_error)?;
        exchange(tls, &request, limit).await
    } else {
        exchange(tcp, &request, limit).await
    }
}

/// A transport failure the peer had a hand in wording, sanitised.
///
/// The OS writes most of these and is not the threat. A TLS handshake is
/// the exception: rustls's `CertificateError::NotValidForNameContext`
/// prints the names the peer's certificate presented, and [`FetchError`]'s
/// `Display` puts those in front of an operator.
///
/// Keeps the [`std::io::ErrorKind`] and drops the nested source, which
/// nothing walks.
fn peer_transport_error(source: std::io::Error) -> FetchError {
    FetchError::Transport(std::io::Error::new(
        source.kind(),
        terminal_safe::sanitise(&source.to_string()).0,
    ))
}

/// The request line and headers [`get_inner`] sends. `Host` names the port
/// only when it is off the scheme's own default (443/80), and there is no
/// `Accept-Encoding`.
fn build_get_request(target: &Target) -> String {
    let default_port = if target.https { 443 } else { 80 };
    let host = if target.port == default_port {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
        path = target.path,
    )
}

/// Writes `request`, flushes, then reads back the response. The flush
/// matters on the TLS branch: `tokio-rustls` buffers writes in `rustls`'s
/// record layer, and skipping it leaves a request the peer never sees.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &str,
    limit: usize,
) -> Result<Vec<u8>, FetchError> {
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    read_response(stream, limit).await
}

/// Reads the status line, then headers to the blank line that ends them,
/// then exactly `Content-Length` bytes of body, refusing in the order the
/// module doc lists.
async fn read_response<S: AsyncRead + Unpin>(
    stream: S,
    limit: usize,
) -> Result<Vec<u8>, FetchError> {
    // One budget for the status line and every header line together, spent
    // through a `Take`: `read_line` itself is what is unbounded. Out of
    // budget it reports a clean zero, exactly as EOF does, and
    // `headers.limit()` is what tells the two apart below.
    let mut headers = BufReader::new(stream).take(MAX_HEADER_BYTES as u64);

    let mut status_line = String::new();
    headers.read_line(&mut status_line).await?;
    let code = parse_status_line(&status_line)?;

    let mut location: Option<String> = None;
    let mut transfer_encoding = false;
    let mut content_length: Option<u64> = None;
    // Set on an unparseable value or a second, disagreeing
    // `Content-Length`; the refusal is deferred to after the loop so it
    // keeps its place in the documented order.
    let mut content_length_ok = true;

    loop {
        let mut line = String::new();
        let read = headers.read_line(&mut line).await?;
        if read == 0 {
            if headers.limit() == 0 {
                return Err(FetchError::HeadersTooLarge {
                    limit: MAX_HEADER_BYTES,
                });
            }
            return Err(FetchError::Transport(std::io::Error::other(
                "response headers ended without a blank line",
            )));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(FetchError::Transport(std::io::Error::other(
                "malformed header line: no colon",
            )));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            // Sanitised at this one seam, where a header value becomes an
            // owned string this module keeps. Not hoisted over the whole
            // `match`: `content-length` is parsed below, and cleaning it
            // first would repair `1\u{200b}2` into a `12` this would honour.
            "location" => location = Some(terminal_safe::sanitise(value).0),
            "transfer-encoding" => transfer_encoding = true,
            "content-length" => match value.parse::<u64>() {
                Ok(parsed) => match content_length {
                    Some(existing) if existing != parsed => content_length_ok = false,
                    Some(_) => {}
                    None => content_length = Some(parsed),
                },
                Err(_err) => content_length_ok = false,
            },
            _ => {}
        }
    }

    if (300..400).contains(&code)
        && let Some(location) = location
    {
        return Err(FetchError::Redirect { location });
    }
    // A 3xx with no Location has nothing left to name.
    if !(200..300).contains(&code) {
        return Err(FetchError::Status(code));
    }
    if transfer_encoding {
        return Err(FetchError::Chunked);
    }
    if !content_length_ok {
        return Err(FetchError::Transport(std::io::Error::other(
            "content-length header was not a number, or two content-length headers disagreed",
        )));
    }
    let Some(content_length) = content_length else {
        return Err(FetchError::Transport(std::io::Error::other(
            "response carried no content-length",
        )));
    };
    if content_length > limit as u64 {
        return Err(FetchError::TooLarge { limit });
    }
    // Safe: just checked `content_length <= limit`, and `limit` is itself a
    // `usize`, so `content_length` fits in one.
    let expected = content_length as usize;

    // Anything the `BufReader` read past the blank line is still in its
    // buffer, so unwrapping the `Take` resumes where the header loop
    // stopped.
    let mut reader = headers.into_inner();

    let mut body = vec![0u8; expected];
    let mut filled = 0;
    while filled < expected {
        let read = reader.read(&mut body[filled..]).await?;
        if read == 0 {
            return Err(FetchError::Truncated {
                expected,
                got: filled,
            });
        }
        filled += read;
        // Unreachable in practice: `body` is sized to `expected`, which
        // `limit` already bounds, so `filled` cannot pass it here.
        if filled > limit {
            return Err(FetchError::TooLarge { limit });
        }
    }
    Ok(body)
}

/// The status code out of `status_line` (`"HTTP/1.1 200 OK\r\n"`), refusing
/// anything not shaped like an HTTP status line at all.
fn parse_status_line(status_line: &str) -> Result<u16, FetchError> {
    let mut parts = status_line.split_whitespace();
    match parts.next() {
        Some(version) if version.starts_with("HTTP/") => {}
        _ => {
            return Err(FetchError::Transport(std::io::Error::other(
                "response did not start with an HTTP status line",
            )));
        }
    }
    parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| FetchError::Transport(std::io::Error::other("malformed http status code")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves one canned response on an ephemeral port, then stops.
    async fn serve(response: &'static [u8]) -> Target {
        serve_owned(response.to_vec()).await
    }

    /// [`serve`] for a response a test builds at run time: the header-cap
    /// cases, whose responses are tens of kilobytes of padding.
    async fn serve_owned(response: Vec<u8>) -> Target {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.unwrap();
            // Drains the request so the client's write never stalls on a
            // full socket buffer.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        });
        Target {
            https: false,
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            path: "/".to_string(),
        }
    }

    /// Serves a response that opens a header and never ends it, for as
    /// long as anything is still reading.
    async fn serve_endless_header() -> Target {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            if stream
                .write_all(b"HTTP/1.1 200 OK\r\nX-Filler: ")
                .await
                .is_err()
            {
                return;
            }
            // Ends itself: a client that refuses closes the socket, and
            // the next write fails.
            let filler = vec![b'A'; 8 * 1024];
            while stream.write_all(&filler).await.is_ok() {}
        });
        Target {
            https: false,
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            path: "/".to_string(),
        }
    }

    /// Asserts the exact cleaned string, not merely the absence of an
    /// escape: the redirect must still name where it pointed.
    #[tokio::test]
    async fn a_hostile_location_header_cannot_drive_the_terminal_it_prints_to() {
        let target = serve(
            b"HTTP/1.1 302 Found\r\nLocation: \x1b[2J\x1b]0;pwned\x07/gone\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let err = get(&target, 1 << 20, Duration::from_secs(5))
            .await
            .expect_err("refused");
        let FetchError::Redirect { location } = &err else {
            panic!("wrong variant: {err:?}")
        };
        assert_eq!(location, "[2J]0;pwned/gone", "location was not sanitised");
        assert!(
            !err.to_string().chars().any(char::is_control),
            "a control character reached the message: {:?}",
            err.to_string()
        );
    }

    /// Every response here is hostile in a different place and drives a
    /// different refusal, so a future variant that starts carrying
    /// response-derived text has to pass this to ship.
    #[tokio::test]
    async fn no_refusal_hands_a_terminal_a_character_the_host_chose() {
        let hostile: [(&'static [u8], &str); 6] = [
            (
                b"HTTP/1.1 302 Found\r\nLocation: \x1b[2J\x07\r\nContent-Length: 0\r\n\r\n",
                "a redirect naming a hostile location",
            ),
            (
                b"HTTP/1.1 404 \x1b[2JNot Found\r\nContent-Length: 0\r\n\r\n",
                "a non-2xx whose reason phrase is hostile",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: \x1b[2Jchunked\r\n\r\n0\r\n\r\n",
                "a chunked refusal whose header value is hostile",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: \x1b[2Jnope\r\n\r\n",
                "an unparseable content-length that is hostile",
            ),
            (
                b"\x1b[2J\x07 NOT HTTP AT ALL\r\n\r\n",
                "a status line that is not HTTP and is hostile",
            ),
            (
                b"HTTP/1.1 200 OK\r\n\x1b[2Jno-colon-here\r\n\r\n",
                "a header line with no colon, hostile",
            ),
        ];
        for (response, why) in hostile {
            let target = serve(response).await;
            let err = get(&target, 1 << 20, Duration::from_secs(5))
                .await
                .expect_err(why);
            let shown = err.to_string();
            assert!(
                !shown.chars().any(char::is_control),
                "{why}: a control character reached the message: {shown:?}"
            );
        }
    }

    /// The two-second budget is the forcing mechanism: a bounded refusal
    /// comes back in milliseconds.
    #[tokio::test]
    async fn a_header_that_never_ends_is_refused_rather_than_read_forever() {
        let target = serve_endless_header().await;
        let err = get(&target, 1 << 20, Duration::from_secs(2))
            .await
            .expect_err("refused");
        assert!(
            matches!(
                err,
                FetchError::HeadersTooLarge {
                    limit: MAX_HEADER_BYTES
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_header_block_just_under_the_cap_is_still_read() {
        let tail = b"\r\nContent-Length: 5\r\n\r\nhello";
        let head = b"HTTP/1.1 200 OK\r\nX-Pad: ";
        let pad = MAX_HEADER_BYTES - head.len() - tail.len();
        let mut response = head.to_vec();
        response.extend(std::iter::repeat_n(b'A', pad));
        response.extend_from_slice(tail);
        let target = serve_owned(response).await;
        let body = get(&target, 1 << 20, Duration::from_secs(5))
            .await
            .expect("read");
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn a_content_length_body_is_read_exactly() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").await;
        let body = get(&target, 1 << 20, Duration::from_secs(5))
            .await
            .expect("read");
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn a_chunked_response_is_refused_rather_than_misparsed() {
        let target =
            serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n")
                .await;
        assert!(matches!(
            get(&target, 1 << 20, Duration::from_secs(5)).await,
            Err(FetchError::Chunked)
        ));
    }

    #[tokio::test]
    async fn a_redirect_is_refused_and_names_where_it_pointed() {
        let target = serve(
            b"HTTP/1.1 301 Moved\r\nLocation: https://elsewhere/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let err = get(&target, 1 << 20, Duration::from_secs(5))
            .await
            .expect_err("refused");
        let FetchError::Redirect { location } = err else {
            panic!("wrong variant: {err:?}")
        };
        assert_eq!(location, "https://elsewhere/");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_refused_before_it_is_read() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n").await;
        assert!(matches!(
            get(&target, 10, Duration::from_secs(5)).await,
            Err(FetchError::TooLarge { limit: 10 })
        ));
    }

    #[tokio::test]
    async fn a_non_2xx_carries_its_status() {
        let target = serve(b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n").await;
        assert!(matches!(
            get(&target, 1 << 20, Duration::from_secs(5)).await,
            Err(FetchError::Status(500))
        ));
    }

    #[tokio::test]
    async fn a_peer_that_closes_mid_body_is_an_error_not_a_short_read() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").await;
        let err = get(&target, 1 << 20, Duration::from_secs(5))
            .await
            .expect_err("refused");
        assert!(
            matches!(
                err,
                FetchError::Truncated {
                    expected: 10,
                    got: 5
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_content_length_is_refused() {
        let target = serve(b"HTTP/1.1 200 OK\r\n\r\nbody").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    #[test]
    fn a_url_that_is_not_http_or_https_is_refused() {
        assert!(matches!(
            parse_url("file:///etc/passwd"),
            Err(FetchError::Url(_))
        ));
        assert!(matches!(parse_url("not a url"), Err(FetchError::Url(_))));
    }

    /// Both halves matter. The refusal is the fix; the exact string is
    /// what proves the refusal did not itself print the password, which is
    /// what quoting `url` in the message would have done.
    #[test]
    fn a_url_carrying_credentials_is_refused_without_echoing_them() {
        for url in [
            "https://user:hunter2@example.com:8443/webhook",
            "https://user:hunter2@example.com/webhook",
            "http://hunter2@example.com/webhook",
            // Schemes this module cannot fetch at all. Each used to be
            // refused on the scheme, in a message quoting the whole url.
            "ftp://user:hunter2@example.com/webhook",
            "HTTPS://user:hunter2@example.com/webhook",
            "user:hunter2@example.com/webhook",
            // Scheme-relative: an authority with nothing in front of it,
            // so there is no `://` to find and the leading `//` has to be
            // stepped over or the authority reads as empty.
            "//user:hunter2@example.com/webhook",
        ] {
            let err = parse_url(url).unwrap_err();
            assert!(matches!(err, FetchError::Url(_)), "{url}: {err:?}");
            assert_eq!(
                err.to_string(),
                "not a fetchable url: credentials before the host (`user@` or `user:pass@`) are \
                 not supported; the url is not echoed, since it carries one",
                "{url}"
            );
            assert!(!format!("{err} {err:?}").contains("hunter2"), "{url}");
        }
    }

    /// The backstop under [`url_carries_credentials`]. That predicate
    /// reads an authority, and a url far enough off the grammar is one it
    /// can misread: in each of these the `@` is in a path, so the
    /// predicate says `false` and some other refusal is what sees the url.
    /// Every refusal names the url through [`url_for_message`] rather than
    /// trusting the predicate to have been right about where the authority
    /// ended.
    #[test]
    fn no_refusal_quotes_a_url_holding_an_at_sign() {
        for url in [
            // Refused on the scheme.
            "file:///etc/pass@wd",
            // Refused on the port, with the `@` off in the path.
            "https://example.com:notaport/etc/pass@wd",
        ] {
            assert!(!url_carries_credentials(url), "{url}");
            let err = parse_url(url).unwrap_err();
            let rendered = format!("{err} {err:?}");
            assert!(!rendered.contains("pass@wd"), "{url}: {rendered}");
            assert!(
                rendered.contains("a url that may carry credentials"),
                "{url}: {rendered}"
            );
        }
    }

    /// Neither a query nor a fragment is part of the host. Reading them as
    /// authority made an `@` in a query look like a credential, so
    /// `?contact=alice@example.com` was refused as one.
    ///
    /// They part company after that: a query is the request target's, a
    /// fragment is the client's and is dropped.
    #[test]
    fn a_query_joins_the_path_and_a_fragment_is_dropped() {
        for (url, path) in [
            (
                "https://example.com?contact=alice@example.com",
                "/?contact=alice@example.com",
            ),
            ("https://example.com#a@b", "/"),
            ("https://example.com/hook#frag", "/hook"),
            ("https://example.com/hook?a=b#frag", "/hook?a=b"),
            ("https://example.com/hook?a=b", "/hook?a=b"),
            ("https://example.com", "/"),
        ] {
            let target = parse_url(url).unwrap_or_else(|err| panic!("{url}: {err}"));
            assert_eq!(target.host, "example.com", "{url}");
            assert_eq!(target.port, 443, "{url}");
            assert_eq!(target.path, path, "{url}");
        }
        assert!(!url_carries_credentials(
            "https://example.com?contact=alice@example.com"
        ));
    }

    /// The door the fragment would have gone out of. `parse_url` dropping
    /// it is the fix; this is the assertion that nothing downstream puts
    /// it back.
    #[test]
    fn no_fragment_reaches_the_request_line() {
        let target = parse_url("https://example.com/hook?a=b#sentinelfragment")
            .expect("a url with a query and a fragment parses");
        let request = build_get_request(&target);
        assert!(
            request.starts_with("GET /hook?a=b HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(!request.contains("sentinelfragment"), "{request}");
    }

    /// A Discord webhook URL is a bearer credential and carries its token
    /// as a path segment, so `Target`'s `Debug` withholds the path the way
    /// `Sink`'s withholds the whole URL.
    #[test]
    fn a_targets_debug_redacts_the_path() {
        let target = parse_url("https://discord.com/api/webhooks/123/s3cr3t-token")
            .expect("a plain https url parses");
        let rendered = format!("{target:?}");
        assert_eq!(
            rendered,
            r#"Target { https: true, host: "discord.com", port: 443, path: <redacted> }"#
        );
        assert!(!rendered.contains("s3cr3t-token"));
    }

    /// A smuggling-style ambiguity: picking one would let a proxy make
    /// this client and whatever is downstream of it disagree about where
    /// the body ends.
    #[tokio::test]
    async fn two_disagreeing_content_lengths_are_refused() {
        let target =
            serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    #[tokio::test]
    async fn a_status_line_that_is_not_http_is_refused() {
        let target = serve(b"NOT HTTP AT ALL\r\n\r\n").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    /// An EOF here must not read as a zero-length body.
    #[tokio::test]
    async fn headers_with_no_terminating_blank_line_are_refused() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    /// Refused as an ordinary [`FetchError::Status`], having nowhere to
    /// point a caller.
    #[tokio::test]
    async fn a_redirect_with_no_location_is_still_refused() {
        let target = serve(b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n").await;
        assert!(matches!(
            get(&target, 1 << 20, Duration::from_secs(5)).await,
            Err(FetchError::Status(302))
        ));
    }
}
