//! A bounded, redirect-refusing GET over `tokio-rustls`: [`get`], plus the
//! URL parsing ([`parse_url`]) and TLS setup ([`tls_connector`]) it shares
//! with `dog::bark::sinks`. An HTTP client, where `crate::http` is a
//! hand-rolled, TLS-free server. Either scheme is accepted; a caller
//! wanting only `https://` enforces that itself.
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
/// `Debug` is derived and not redacted: `parse_url` does not strip a
/// `user:pass@host` authority, so `host` can carry credentials from a URL
/// that has them.
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// Why [`parse_url`] or [`get`] failed.
///
/// `Debug` needs no redaction: every field is a URL this module was given
/// or redirected to, an HTTP status, a byte count, or an OS error.
#[derive(Debug)]
pub enum FetchError {
    /// `url` did not parse as an absolute `http://`/`https://` URL. Carries
    /// a human-readable reason, never the raw bytes of a malformed input.
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

/// Parses `url` into a [`Target`] naming where [`get`] should connect.
///
/// Hand-rolled: a fetch target is never more than a scheme, a host, an
/// optional port and a path.
///
/// # Errors
/// - [`FetchError::Url`] if `url` does not start with `http://` or
///   `https://`, names a non-numeric port, or names no host.
pub fn parse_url(url: &str) -> Result<Target, FetchError> {
    let (https, rest) = match url.strip_prefix("https://") {
        Some(rest) => (true, rest),
        None => match url.strip_prefix("http://") {
            Some(rest) => (false, rest),
            None => {
                return Err(FetchError::Url(format!(
                    "{url} does not start with http:// or https://"
                )));
            }
        },
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse()
                .map_err(|_err| FetchError::Url(format!("{url} has a non-numeric port")))?,
        ),
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(FetchError::Url(format!("{url} has no host")));
    }
    Ok(Target {
        https,
        host: host.to_string(),
        port,
        path: path.to_string(),
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
        // The second cap check: `expected` is already bounded by `limit`
        // and `body`'s fixed size prevents `filled` exceeding it, so this
        // guards a future change to the loop rather than a reachable path.
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
