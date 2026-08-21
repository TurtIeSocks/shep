//! A bounded, redirect-refusing GET over `tokio-rustls` — [`get`], plus the
//! URL parsing ([`parse_url`]) and TLS setup ([`tls_connector`]) it shares
//! with `dog::bark::sinks`, the module this one was carved out of.
//!
//! **This is not `crate::http`.** That module is a hand-rolled HTTP
//! *server* — deliberately TLS-free, "with no TLS to get wrong because the
//! metrics endpoint is loopback by default" (its own module doc). This
//! module is the opposite shape: an HTTP *client* that fetches one document
//! from wherever an operator points it (`shep dogs --available`'s community
//! index, served over real TLS, is never loopback). One serves a request;
//! one fetches a document; only the second needs TLS, which is the whole
//! reason these are two files rather than one growing past 800 lines with a
//! client bolted onto a module whose doc comment argues against exactly
//! that.
//!
//! [`tls_connector`], [`Target`] and [`parse_url`] used to live in
//! `dog::bark::sinks` — bark's own webhook POSTs need the identical
//! connect-and-TLS setup. They moved here unchanged so a fetch's GET and
//! bark's POST share one connection path instead of two copies of the same
//! `rustls` wiring; `sinks.rs` now imports them from here. Everything
//! downstream of the connection stays separate on purpose: bark writes a
//! POST and reads a status line, this writes a GET and reads a bounded
//! body, and each has its own error type because forcing those into one
//! shape would blur both.
//!
//! **Both schemes are accepted, deliberately** — the same transport/policy
//! split `sinks.rs` already draws (`require_secure_scheme` is bark's
//! config-time policy over a transport that itself speaks either scheme).
//! [`get`] has no opinion on scheme; a caller that wants only `https://`
//! enforces that itself, same as bark's config loader does. Without this, a
//! local plain-HTTP test server could never be exercised.
//!
//! **`get` refuses, in this order:** a 3xx naming its `Location`
//! ([`FetchError::Redirect`]); any other non-2xx ([`FetchError::Status`],
//! which is also where a 3xx with no `Location` to report ends up — there
//! is nothing left to name once that's missing); a `Transfer-Encoding`
//! header at all ([`FetchError::Chunked`] — this client reads exactly
//! `Content-Length` bytes and nothing else, so a chunked body would be
//! misparsed rather than decoded); a `Content-Length` that is absent,
//! not a number, or contradicted by a second `Content-Length` header on the
//! same response ([`FetchError::Transport`]); and a `Content-Length` above
//! the caller's `limit` ([`FetchError::TooLarge`]). The size cap is checked
//! twice — against the declared `Content-Length` before a byte of body is
//! read, and again as each chunk of the body arrives — so neither a lying
//! header nor an honest one can make this read forever.
//!
//! No `Accept-Encoding` is ever sent, so there is no `Content-Encoding` to
//! decode: measured against the real target (a GitHub Pages site), it sends
//! `Content-Length` and never chunks, and sends no `Content-Encoding`
//! unless asked for one.

use core::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;

/// A URL, parsed into what [`get`] needs to reach it — a fetch's own
/// version of what `dog::bark::sinks` calls a sink's target, since that is
/// exactly what this is: the two used to be one type before this module
/// split off.
///
/// `Debug` is derived, unlike [`crate::dog::bark::sinks::Sink`]'s own
/// hand-written and redacted one: a fetch target is a public document
/// location (the community dog index, or wherever a 3xx pointed), never a
/// bearer credential the way a webhook URL is.
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
/// `Debug` needs no redaction: every field is a URL this module was
/// explicitly given or was explicitly redirected to (never a webhook's
/// bearer credential — that's [`crate::dog::bark::sinks::Sink`]'s own
/// concern), an HTTP status, a byte count, or an OS error.
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
        /// The `Location` header's value, verbatim.
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
            | Self::Timeout
            | Self::Truncated { .. } => None,
        }
    }
}

/// Parses `url` into a [`Target`] naming where [`get`] should connect.
///
/// Hand-rolled rather than pulled from a `url` crate, same reasoning as
/// when this lived in `sinks.rs` as `parse_sink_url`: a fetch target is
/// never more than a scheme, a host, an optional port and a path, which is
/// narrow enough to parse directly.
///
/// # Errors
/// - [`FetchError::Url`] — `url` does not start with `http://` or
///   `https://`, names a port that is not a number, or names no host.
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

/// The TLS connector every `https://` connection this crate makes shares —
/// bark's webhook POSTs and `get`'s own fetches alike — built once on first
/// use rather than per request: `TlsConnector` wraps an
/// `Arc<rustls::ClientConfig>`, and building a fresh one per connection
/// would re-walk the root store and re-derive the cipher suite set every
/// time.
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
/// See this module's own doc comment for the exact refusal order; every
/// [`FetchError`] variant can come out of this call except
/// [`FetchError::Url`], which only [`parse_url`] produces.
pub async fn get(target: &Target, limit: usize, timeout: Duration) -> Result<Vec<u8>, FetchError> {
    match tokio::time::timeout(timeout, get_inner(target, limit)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(FetchError::Timeout),
    }
}

/// Connects to `target`, over TLS when `target.https`, and runs the
/// write/read exchange over whichever stream results — the same shape as
/// `sinks.rs`'s own `deliver_inner`, aimed at a GET instead of a POST.
async fn get_inner(target: &Target, limit: usize) -> Result<Vec<u8>, FetchError> {
    let request = build_get_request(target);
    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(FetchError::Transport)?;
    if target.https {
        let domain = ServerName::try_from(target.host.clone())
            .map_err(|source| FetchError::Transport(std::io::Error::other(source)))?;
        let tls = tls_connector()
            .connect(domain, tcp)
            .await
            .map_err(FetchError::Transport)?;
        exchange(tls, &request, limit).await
    } else {
        exchange(tcp, &request, limit).await
    }
}

/// The request line and headers [`get_inner`] sends: `Host` names the port
/// only when it is off the scheme's own default (443/80), and there is
/// deliberately no `Accept-Encoding` — see this module's own doc comment
/// for why.
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

/// Writes `request`, flushes, then reads back the response — the explicit
/// flush matters on the TLS branch the same way `sinks.rs`'s own
/// `write_and_read` documents: `tokio-rustls` buffers writes in `rustls`'s
/// record layer, and skipping the flush can leave a request sitting there
/// the peer never sees.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &str,
    limit: usize,
) -> Result<Vec<u8>, FetchError> {
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(FetchError::Transport)?;
    stream.flush().await.map_err(FetchError::Transport)?;
    read_response(stream, limit).await
}

/// Reads the status line, then headers to the blank line that ends them,
/// then exactly `Content-Length` bytes of body — refusing everything this
/// module's own doc comment lists, in the order it lists them.
async fn read_response<S: AsyncRead + Unpin>(
    stream: S,
    limit: usize,
) -> Result<Vec<u8>, FetchError> {
    let mut reader = BufReader::new(stream);

    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(FetchError::Transport)?;
    let code = parse_status_line(&status_line)?;

    let mut location: Option<String> = None;
    let mut transfer_encoding = false;
    let mut content_length: Option<u64> = None;
    // Set on an unparseable value or a second `Content-Length` header that
    // disagrees with the first; the actual refusal is deferred to after the
    // loop so it takes its place in the documented refusal order rather
    // than jumping the queue ahead of a `Transfer-Encoding` header that
    // happens to appear later in the block.
    let mut content_length_ok = true;

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(FetchError::Transport)?;
        if read == 0 {
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
            "location" => location = Some(value.to_string()),
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
    // A 3xx with no Location has nothing left to name; it falls through to
    // the ordinary non-2xx refusal below.
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

    let mut body = vec![0u8; expected];
    let mut filled = 0;
    while filled < expected {
        let read = reader
            .read(&mut body[filled..])
            .await
            .map_err(FetchError::Transport)?;
        if read == 0 {
            return Err(FetchError::Truncated {
                expected,
                got: filled,
            });
        }
        filled += read;
        // The second of the two cap checks this module's doc comment
        // promises: `expected` was already bounded by `limit` above, and
        // `body`'s fixed size structurally prevents `filled` from ever
        // exceeding it, so this is defense against a future change to the
        // read loop above rather than a path any test can reach today.
        if filled > limit {
            return Err(FetchError::TooLarge { limit });
        }
    }
    Ok(body)
}

/// The status code out of `status_line` (`"HTTP/1.1 200 OK\r\n"`), refusing
/// anything not shaped like an HTTP status line at all — a check
/// `sinks.rs`'s own `parse_status_code` does not need, since every peer it
/// talks to (Discord, Slack, a webhook, or this module's own test harness)
/// always answers in HTTP.
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.unwrap();
            // Drains the request so the client's write never stalls on a
            // full socket buffer; the response is canned and does not
            // depend on what was asked for.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream.write_all(response).await;
            let _ = stream.shutdown().await;
        });
        Target {
            https: false,
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            path: "/".to_string(),
        }
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

    /// Extra, beyond the brief's own eight: two disagreeing `Content-Length`
    /// headers must not silently pick one — a smuggling-style ambiguity a
    /// proxy in front of the real target could exploit to make this client
    /// and whatever's downstream of it disagree about where the body ends.
    #[tokio::test]
    async fn two_disagreeing_content_lengths_are_refused() {
        let target =
            serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello!").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    /// Extra: a status line that is not HTTP at all (garbage, or an empty
    /// line from a peer that closed immediately) must not be silently
    /// treated as a parse failure that happens to land on `Status`/`Chunked`
    /// by accident of `split_whitespace` matching something numeric.
    #[tokio::test]
    async fn a_status_line_that_is_not_http_is_refused() {
        let target = serve(b"NOT HTTP AT ALL\r\n\r\n").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    /// Extra: a header block that never reaches its terminating blank line
    /// (the peer closes right after the last header) must not be read as
    /// "no headers left, body starts here" — that would make an EOF look
    /// like a zero-length body instead of the malformed response it is.
    #[tokio::test]
    async fn headers_with_no_terminating_blank_line_are_refused() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    /// Extra: a 3xx with no `Location` at all has nowhere documented to
    /// send a caller, so it must still be refused (as an ordinary
    /// [`FetchError::Status`]) rather than treated as a success with an
    /// empty body.
    #[tokio::test]
    async fn a_redirect_with_no_location_is_still_refused() {
        let target = serve(b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n").await;
        assert!(matches!(
            get(&target, 1 << 20, Duration::from_secs(5)).await,
            Err(FetchError::Status(302))
        ));
    }
}
