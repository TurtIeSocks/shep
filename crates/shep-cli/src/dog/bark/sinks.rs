//! Bark's sinks: [`Sink`], the pure [`render_body`], and the async
//! [`deliver`] that POSTs a rendered body to it.
//!
//! **The transport is hand-rolled HTTP/1.1 over `tokio-rustls`, not
//! `reqwest`.** Discord and Slack webhooks are HTTPS-only, so this is the
//! one place in the workspace that needs TLS, and Rin's ruling
//! (2026-08-12) was to reach for `tokio-rustls` + `webpki-roots` (+10ish
//! crates, no C build dependency) directly rather than `reqwest` (+76 to
//! +93 crates depending on feature set, and a C toolchain — `aws-lc-sys` —
//! under `reqwest`'s own default `rustls` feature). `rustls` does the part
//! that must not be gotten wrong — the handshake and record layer; what
//! this module owns is the same HTTP/1.1 request/response framing
//! `dog::http`'s server side (Task 13) already hand-rolls, aimed the other
//! way. See `crates/shep-cli/Cargo.toml` and this workspace's root
//! `Cargo.toml` for the accounting behind the two new dependencies.
//!
//! **`http://` is accepted, not rejected**, even though Discord and Slack
//! are always `https://`: a [`Sink::Json`] can name any operator-configured
//! endpoint, including an internal one with no TLS in front of it, and this
//! module's own test suite exercises exactly that scheme — the plaintext
//! local test server below is never a real webhook, and the TLS branch
//! ([`tls_connector`], `rustls`'s handshake, the root store built from
//! `webpki-roots`) is consequently **not** exercised by any test in this
//! module. That gap is real, not papered over: the request framing, the
//! status-line read and every `SinkError` path are what this module writes
//! itself, and they are covered; the handshake and record layer are
//! `rustls`'s own tested surface, not bark's. Closing the remaining gap
//! would mean the test harness terminating TLS itself — a second dependency
//! shape for one module's tests — and is out of scope here.
//!
//! **No redirect is ever followed.** Webhooks do not redirect, bark's own
//! needs are fire-and-forget with no connection pooling, and skipping
//! redirect-following means a sink's credential (the webhook URL itself)
//! never travels anywhere the sink's own config did not name.
//!
//! **A webhook URL is a bearer credential** — Discord and Slack embed the
//! token in the path, so anyone holding one can post to that channel.
//! [`Sink`]'s `Debug` is hand-written and redacted (IR-41): see its own doc.
//! [`SinkError`] carries none of it — a failed delivery is reported by
//! sink kind and failure kind, never by URL.
//!
//! Every item in this module is exercised by its own tests, but nothing
//! outside them calls in yet: Task 20 (reconciling the shepherd's bus
//! against `barks.jsonl`) and Task 21 (`bark::run`, the entrypoint
//! `super::super::run_dog`'s `"bark"` arm reaches) are what wire
//! [`render_body`]/[`deliver`] into a running dog. `#![allow(dead_code)]`
//! says so explicitly, matching `output/table.rs`'s and `output/mod.rs`'s
//! own forward-declaration shape, rather than inventing a call site nothing
//! needs yet.
#![allow(dead_code)]

use core::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use shep_core::barks::Bark;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;

/// One named entry under `[dog.bark.sinks]`.
///
/// `Debug` is REDACTED (IR-41): every variant carries a webhook URL, and a
/// Discord or Slack webhook URL is a bearer credential — anyone holding it
/// can post to that channel. A sink printed into a log, a panic message or
/// an error chain leaks it to whoever reads the log.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Sink {
    /// A Discord webhook: `{"content": "..."}`.
    Discord {
        /// The webhook URL.
        url: String,
    },
    /// A Slack incoming webhook: `{"text": "..."}`.
    Slack {
        /// The webhook URL.
        url: String,
    },
    /// A JSON POST with a body the operator templates.
    Json {
        /// Where to POST.
        url: String,
        /// The body, with `{subject}`, `{rule}`, `{message}` and `{at_ms}`
        /// substituted. Defaults to an object carrying all four.
        body: Option<String>,
    },
}

/// Manual, never derived: a derived `Debug` would print `url` (and, for
/// `Json`, an operator's own `body` template) in full, undoing the
/// redaction this type exists to hold. Every variant collapses to the same
/// shape — `Sink::<Variant> { url: <redacted> }` — so the variant name is
/// the only thing this ever reveals about a sink.
impl fmt::Debug for Sink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Discord { .. } => "Discord",
            Self::Slack { .. } => "Slack",
            Self::Json { .. } => "Json",
        };
        write!(f, "Sink::{variant} {{ url: <redacted> }}")
    }
}

impl Sink {
    /// This sink's webhook URL, whichever variant it is.
    ///
    /// Not exposed past this module — a caller reaching for a sink's URL
    /// directly is exactly the leak [`Sink`]'s own `Debug` guards against;
    /// [`parse_sink_url`] and [`require_secure_scheme`] are the only
    /// callers.
    fn url(&self) -> &str {
        match self {
            Self::Discord { url } | Self::Slack { url } | Self::Json { url, .. } => url,
        }
    }

    /// `"discord"`/`"slack"` for the two kinds that are HTTPS-only on the
    /// real service, `None` for [`Sink::Json`] — an operator's own
    /// endpoint, which may legitimately have no TLS in front of it.
    fn https_only_kind(&self) -> Option<&'static str> {
        match self {
            Self::Discord { .. } => Some("discord"),
            Self::Slack { .. } => Some("slack"),
            Self::Json { .. } => None,
        }
    }
}

/// Why a `[dog.bark.sinks]` entry was refused at config-load time.
///
/// `Debug` needs no redaction, unlike [`Sink`]'s own: the only field this
/// carries is the sink's config key, never its url.
#[derive(Debug)]
pub enum SinkConfigError {
    /// Sink `name` is a [`Sink::Discord`] or [`Sink::Slack`] (`kind`)
    /// configured with `http://`.
    InsecureScheme {
        /// The sink's config key under `[dog.bark.sinks]` — never the url,
        /// which is the credential this refusal exists to protect.
        name: String,
        /// `"discord"` or `"slack"`.
        kind: &'static str,
    },
}

impl fmt::Display for SinkConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsecureScheme { name, kind } => write!(
                f,
                "sink \"{name}\" is a {kind} webhook configured with http://; \
                 {kind} only serves https://, and a {kind} webhook url is a \
                 bearer credential that must not travel in cleartext"
            ),
        }
    }
}

impl core::error::Error for SinkConfigError {}

/// Refuses a Discord or Slack sink configured with `http://`.
///
/// A Discord or Slack webhook url IS the bearer credential — the token
/// lives in the path — so an `http://` scheme lets anyone on the wire
/// capture it and post as that integration forever. This removes no
/// legitimate use: discord.com and slack.com serve `https://` only, so an
/// `http://` url to either could never have worked anyway.
/// [`Sink::Json`] is left permissive — an operator pointing bark at an
/// internal endpoint over plain `http://` is a legitimate arrangement, and
/// [`parse_sink_url`] still accepts it at delivery time.
///
/// # Errors
/// - [`SinkConfigError::InsecureScheme`] — `sink` is [`Sink::Discord`] or
///   [`Sink::Slack`] and its configured url does not start with
///   `https://`.
pub fn require_secure_scheme(name: &str, sink: &Sink) -> Result<(), SinkConfigError> {
    let Some(kind) = sink.https_only_kind() else {
        return Ok(());
    };
    if sink.url().starts_with("https://") {
        Ok(())
    } else {
        Err(SinkConfigError::InsecureScheme {
            name: name.to_owned(),
            kind,
        })
    }
}

/// Why [`render_body`] or [`deliver`] failed.
///
/// `Debug` needs no redaction, unlike [`Sink`]'s own: every field here is
/// an OS error, an HTTP status code, or the first line of a response body —
/// never a sink's webhook URL. Hiding that is `Sink`'s job, not this type's.
#[derive(Debug)]
pub enum SinkError {
    /// The rendered body is not valid JSON — a templated `body` can
    /// produce this; the default (untemplated) body cannot.
    Template {
        /// The JSON parser's complaint against the rendered body.
        message: String,
    },
    /// The request could not be sent at all, or the sink did not answer
    /// within the caller's `timeout`.
    Transport {
        /// The underlying I/O failure, or a fixed reason for a
        /// malformed sink URL / HTTP response this module rejected before
        /// any I/O was attempted.
        source: std::io::Error,
    },
    /// The endpoint answered outside 2xx.
    Status {
        /// The HTTP status code.
        code: u16,
        /// The first line of the response body.
        message: String,
    },
}

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Template { message } => {
                write!(f, "templated sink body is not valid json: {message}")
            }
            Self::Transport { source } => write!(f, "sink delivery failed: {source}"),
            Self::Status { code, message } => write!(f, "sink answered {code}: {message}"),
        }
    }
}

impl core::error::Error for SinkError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Transport { source } => Some(source),
            Self::Template { .. } | Self::Status { .. } => None,
        }
    }
}

/// The body `sink` sends for `bark` — pure, and the half worth testing
/// exhaustively.
///
/// # Errors
/// - [`SinkError::Template`] — the rendered body is not valid JSON, which
///   a templated `body` can produce and which every one of these endpoints
///   refuses with a 400 an operator would otherwise have to guess at.
pub fn render_body(sink: &Sink, bark: &Bark) -> Result<String, SinkError> {
    let body = match sink {
        Sink::Discord { .. } => serde_json::json!({ "content": bark.message }).to_string(),
        Sink::Slack { .. } => serde_json::json!({ "text": bark.message }).to_string(),
        Sink::Json { body: None, .. } => serde_json::json!({
            "subject": bark.subject,
            "rule": bark.rule,
            "message": bark.message,
            "at_ms": bark.at_ms,
        })
        .to_string(),
        Sink::Json {
            body: Some(template),
            ..
        } => {
            let rendered = substitute(template, bark);
            serde_json::from_str::<serde_json::Value>(&rendered).map_err(|source| {
                SinkError::Template {
                    message: source.to_string(),
                }
            })?;
            rendered
        }
    };
    Ok(body)
}

/// Substitutes `{subject}`, `{rule}`, `{message}` and `{at_ms}` in `template`
/// with `bark`'s own fields — the three strings JSON-escaped (never quoted:
/// the template's own literal quotes already surround the token, the same
/// way an operator writes `"{message}"`, not `{message}`), `at_ms` as a bare
/// number.
///
/// A SINGLE forward pass over `template`, never four sequential
/// whole-string `.replace()` calls: `rest` only ever shrinks from the
/// front, and once a substituted value is pushed onto `out` it is never
/// looked at again. A sheep named `{at_ms}` makes `bark.message` literally
/// contain the text `{at_ms}` — sequential replaces would paste that in
/// during the `{message}` pass and then rewrite it during the `{at_ms}`
/// pass that follows, corrupting the sheep's name inside the rendered
/// alert. This pass structurally cannot do that: `{at_ms}` only ever
/// matches inside `rest`, which is what remains of the *template*, not
/// what has already been written to `out`.
fn substitute(template: &str, bark: &Bark) -> String {
    let tokens: [(&str, String); 4] = [
        ("{subject}", json_escape(&bark.subject)),
        ("{rule}", json_escape(&bark.rule)),
        ("{message}", json_escape(&bark.message)),
        ("{at_ms}", bark.at_ms.to_string()),
    ];
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(brace) = rest.find('{') {
        out.push_str(&rest[..brace]);
        rest = &rest[brace..];
        match tokens.iter().find(|(token, _)| rest.starts_with(token)) {
            Some((token, value)) => {
                out.push_str(value);
                rest = &rest[token.len()..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `s`, escaped for use inside a JSON string's quotes — not a JSON string
/// literal itself, since [`substitute`]'s own template already supplies the
/// surrounding quotes.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
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

/// POSTs `bark` to `sink`, bounded by `timeout`.
///
/// # Errors
/// - [`SinkError::Template`] — as [`render_body`].
/// - [`SinkError::Transport`] — the request failed or timed out.
/// - [`SinkError::Status`] — the endpoint answered outside 2xx, carrying
///   the status and the first line of the body. Discord's own rate-limit
///   429 arrives this way and reads as one.
pub async fn deliver(sink: &Sink, bark: &Bark, timeout: Duration) -> Result<(), SinkError> {
    let body = render_body(sink, bark)?;
    let target = parse_sink_url(sink.url())?;
    // Wraps connect, the TLS handshake (when `target.https`), the write and
    // the status-line read together, so a sink that accepts the connection
    // and then says nothing cannot wedge this past `timeout` regardless of
    // which stage stalls.
    match tokio::time::timeout(timeout, deliver_inner(&target, &body)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(SinkError::Transport {
            source: std::io::Error::from(std::io::ErrorKind::TimedOut),
        }),
    }
}

/// A sink's webhook URL, parsed into what [`deliver`] needs to reach it.
struct SinkTarget {
    /// `true` for `https://`, `false` for `http://`.
    https: bool,
    /// The host, without a port.
    host: String,
    /// The port: the URL's own, or the scheme's default (443/80).
    port: u16,
    /// The request path, always starting with `/`.
    path: String,
}

/// Parses `url` into a [`SinkTarget`].
///
/// Hand-rolled rather than pulled from a `url` crate: a sink's URL is never
/// more than a scheme, a host, an optional port and a path, and that is
/// narrow enough to parse directly.
fn parse_sink_url(url: &str) -> Result<SinkTarget, SinkError> {
    let bad_url = |reason: &'static str| SinkError::Transport {
        source: std::io::Error::other(reason),
    };
    let (https, rest) = match url.strip_prefix("https://") {
        Some(rest) => (true, rest),
        None => match url.strip_prefix("http://") {
            Some(rest) => (false, rest),
            None => return Err(bad_url("sink url must start with http:// or https://")),
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
                .map_err(|_err| bad_url("sink url has a non-numeric port"))?,
        ),
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(bad_url("sink url has no host"));
    }
    Ok(SinkTarget {
        https,
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// The request line, headers and blank line [`deliver_inner`] sends ahead
/// of `body` — `Host` names the port only when it is off the scheme's own
/// default (443/80), matching every sink this module tests, which binds an
/// ephemeral one.
fn build_request(target: &SinkTarget, body: &str) -> String {
    let default_port = if target.https { 443 } else { 80 };
    let host = if target.port == default_port {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        path = target.path,
        len = body.len(),
    )
}

/// Connects to `target`, over TLS when `target.https`, and runs the
/// write/read exchange over whichever stream results.
async fn deliver_inner(target: &SinkTarget, body: &str) -> Result<(), SinkError> {
    let request = build_request(target, body);
    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|source| SinkError::Transport { source })?;
    if target.https {
        let domain =
            ServerName::try_from(target.host.clone()).map_err(|source| SinkError::Transport {
                source: std::io::Error::other(source),
            })?;
        let tls = tls_connector()
            .connect(domain, tcp)
            .await
            .map_err(|source| SinkError::Transport { source })?;
        write_and_read(tls, &request).await
    } else {
        write_and_read(tcp, &request).await
    }
}

/// Writes `request`, flushes, then reads back the status line (and, on a
/// non-2xx, one diagnostic line of body).
///
/// The explicit `flush` matters on the TLS branch: `tokio-rustls` buffers
/// writes in `rustls`'s own record layer, and its module doc says directly
/// that `poll_flush` is what pushes them to the underlying stream — skip it
/// and a request can sit in a buffer the peer never sees. The same flush on
/// a plain `TcpStream` is redundant, not wrong, so one code path serves
/// both rather than branching only to skip it on the plaintext side.
async fn write_and_read<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &str,
) -> Result<(), SinkError> {
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|source| SinkError::Transport { source })?;
    stream
        .flush()
        .await
        .map_err(|source| SinkError::Transport { source })?;
    read_response(stream).await
}

/// Reads the status line off `stream`; a 2xx reply is read no further —
/// Discord's and Slack's own success bodies carry nothing this module acts
/// on. A non-2xx reads past the remaining header lines to the blank line
/// that ends them (or to EOF, gracefully — a peer that closes right after
/// the status line has no headers to skip, not a fault), then takes one
/// more line for [`SinkError::Status`]'s diagnostic.
async fn read_response<S: AsyncRead + Unpin>(stream: S) -> Result<(), SinkError> {
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(|source| SinkError::Transport { source })?;
    let code = parse_status_code(&status_line)?;
    if (200..300).contains(&code) {
        return Ok(());
    }

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|source| SinkError::Transport { source })?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut diagnostic = String::new();
    reader
        .read_line(&mut diagnostic)
        .await
        .map_err(|source| SinkError::Transport { source })?;
    Err(SinkError::Status {
        code,
        message: diagnostic.trim_end().to_string(),
    })
}

/// The status code out of an HTTP/1.x status line (`"HTTP/1.1 429 ..."`).
fn parse_status_code(status_line: &str) -> Result<u16, SinkError> {
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| SinkError::Transport {
            source: std::io::Error::other("malformed http status line"),
        })
}

/// The TLS connector every `https://` delivery shares, built once on first
/// use rather than per request: `TlsConnector` wraps an
/// `Arc<rustls::ClientConfig>`, and building a fresh one per delivery would
/// re-walk the root store and re-derive the cipher suite set on every bark.
fn tls_connector() -> &'static tokio_rustls::TlsConnector {
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

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;
    use crate::dog::http::{HttpRequest, read_request, write_response};

    /// A representative fired alert, tagged by `subject` and `message` — the
    /// two fields these tests vary. `at_ms`/`rule` are fixed since no test
    /// here exercises them directly except through the default body and
    /// `{rule}`/`{at_ms}` substitution.
    fn bark_for(subject: &str, message: &str) -> Bark {
        Bark {
            at_ms: 1_700_000_000_000,
            rule: "watchdog".to_string(),
            subject: subject.to_string(),
            message: message.to_string(),
            sinks: Vec::new(),
        }
    }

    fn discord_sink() -> Sink {
        Sink::Discord {
            url: "https://discord.com/api/webhooks/1/super-secret-token".to_string(),
        }
    }

    fn slack_sink() -> Sink {
        Sink::Slack {
            url: "https://hooks.slack.com/services/T0/B0/super-secret-token".to_string(),
        }
    }

    /// Binds an ephemeral port, accepts exactly one connection, answers
    /// `status`/`body`, and hands the captured request back through the
    /// returned receiver. Hand-rolled over `tokio::net::TcpListener`,
    /// reading with Task 13's [`read_request`] — never a real webhook.
    async fn one_shot_sink(
        status: u16,
        body: &str,
    ) -> (std::net::SocketAddr, oneshot::Receiver<HttpRequest>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut stream, _peer) = listener.accept().await.unwrap();
            let req = read_request(&mut stream, Duration::from_secs(5))
                .await
                .unwrap();
            write_response(&mut stream, status, "application/json", body.as_bytes())
                .await
                .unwrap();
            let _ = tx.send(req);
        });
        (addr, rx)
    }

    /// fails if Discord's body is sent under Slack's key or vice versa.
    /// Both are one-key JSON objects over the same transport, so a swap
    /// compiles, delivers, and is answered with a 400 nobody sees until an
    /// incident — the alert is simply never posted.
    #[test]
    fn each_webhook_gets_the_body_its_own_endpoint_expects() {
        let bark = bark_for("web", "the shepherd gave up on web");
        let discord: serde_json::Value =
            serde_json::from_str(&render_body(&discord_sink(), &bark).unwrap()).unwrap();
        assert_eq!(discord["content"], "the shepherd gave up on web");
        assert!(discord.get("text").is_none());

        let slack: serde_json::Value =
            serde_json::from_str(&render_body(&slack_sink(), &bark).unwrap()).unwrap();
        assert_eq!(slack["text"], "the shepherd gave up on web");
        assert!(slack.get("content").is_none());
    }

    /// fails if a templated body is sent without being checked. Every one
    /// of these endpoints answers a malformed body with a 400, and an
    /// operator reading "400" has no way to know their template lost a
    /// brace — this is the one failure bark can name precisely.
    #[test]
    fn a_template_that_does_not_render_json_is_refused_before_it_is_sent() {
        let sink = Sink::Json {
            url: "http://127.0.0.1:1/".to_string(),
            body: Some(r#"{"text": "{message}"#.to_string()),
        };
        assert!(matches!(
            render_body(&sink, &bark_for("web", "x")),
            Err(SinkError::Template { .. })
        ));
    }

    /// fails if a substituted value is interpolated raw. A sheep's name and
    /// a bark's message are shep's own prose, but the message quotes an
    /// app's name, and an app named `we"b` would break the template's JSON
    /// the same way it would break a Prometheus label.
    #[test]
    fn a_substituted_value_is_json_escaped_into_the_template() {
        let sink = Sink::Json {
            url: "http://127.0.0.1:1/".to_string(),
            body: Some(r#"{"text": "{message}"}"#.to_string()),
        };
        let bark = bark_for("web", r#"app "we"b" crashed"#);
        let rendered = render_body(&sink, &bark).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["text"], bark.message);
    }

    /// fails if a placeholder token that ends up INSIDE an already
    /// substituted field's own value gets rewritten by a later
    /// substitution pass. Bark builds its "gave up" messages by
    /// interpolating a sheep's own name, so a sheep literally named
    /// `{at_ms}` makes `bark.message` contain that exact text; a naive
    /// sequential `.replace()` per field pastes that text in during the
    /// `{message}` pass and then rewrites it during the `{at_ms}` pass that
    /// follows — silently corrupting the sheep's name inside an alert
    /// someone is reading during an incident.
    #[test]
    fn a_placeholder_inside_a_substituted_value_survives_later_passes() {
        let bark = Bark {
            at_ms: 12_345,
            rule: "gave_up".to_string(),
            subject: "web".to_string(),
            message: "{at_ms} gave up: restart budget exhausted".to_string(),
            sinks: Vec::new(),
        };
        let sink = Sink::Json {
            url: "http://127.0.0.1:1/".to_string(),
            body: Some(r#"{"text": "{message}", "stamp": {at_ms}}"#.to_string()),
        };
        let rendered = render_body(&sink, &bark).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(
            value["text"], "{at_ms} gave up: restart budget exhausted",
            "the literal {{at_ms}} carried inside the message must not be rewritten"
        );
        assert_eq!(value["stamp"], 12_345);
    }

    /// The delivery half, against a local server and never a real webhook.
    /// fails if the POST goes out with the wrong method, path or
    /// content-type — three things a receiving endpoint rejects and a unit
    /// test over `render_body` alone can say nothing about.
    #[tokio::test]
    async fn a_delivery_posts_json_to_the_url_it_was_given() {
        let (addr, captured) = one_shot_sink(200, "").await;
        let sink = Sink::Json {
            url: format!("http://{addr}/hook"),
            body: None,
        };
        deliver(&sink, &bark_for("web", "x"), Duration::from_secs(5))
            .await
            .unwrap();
        let req = tokio::time::timeout(Duration::from_secs(5), captured)
            .await
            .expect("the sink server must receive a request")
            .unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/hook");
        assert_eq!(
            req.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&req.body).unwrap()["subject"],
            "web"
        );
    }

    /// fails if a non-2xx is treated as delivered. Discord's rate-limit 429
    /// arrives exactly this way, and a bark counted as delivered when it
    /// was refused is the failure mode alerting exists to not have.
    #[tokio::test]
    async fn a_refused_delivery_is_a_failure_carrying_the_status() {
        let (addr, _captured) = one_shot_sink(429, "rate limited").await;
        let err = deliver(
            &Sink::Json {
                url: format!("http://{addr}/"),
                body: None,
            },
            &bark_for("web", "x"),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SinkError::Status { code: 429, .. }));
    }

    /// fails if `Sink`'s Debug prints a URL. A webhook URL is a bearer
    /// credential: whoever reads the log can post to that channel.
    #[test]
    fn a_sinks_debug_never_prints_its_webhook() {
        let rendered = format!("{:?}", discord_sink());
        assert_eq!(rendered, "Sink::Discord { url: <redacted> }");
        assert!(!rendered.contains("discord.com"));
    }

    /// fails if a Discord webhook over `http://` is accepted. The webhook
    /// url IS the bearer credential, and discord.com serves `https://`
    /// only, so no legitimate `http://` use is being removed.
    #[test]
    fn a_discord_sink_over_http_is_refused() {
        let sink = Sink::Discord {
            url: "http://discord.com/api/webhooks/1/super-secret-token".to_string(),
        };
        let err = require_secure_scheme("ops", &sink).unwrap_err();
        assert!(matches!(
            err,
            SinkConfigError::InsecureScheme {
                kind: "discord",
                ..
            }
        ));
        assert!(!err.to_string().contains("discord.com"));
    }

    /// fails if a Slack webhook over `http://` is accepted — the same
    /// credential-in-cleartext footgun as Discord's.
    #[test]
    fn a_slack_sink_over_http_is_refused() {
        let sink = Sink::Slack {
            url: "http://hooks.slack.com/services/T0/B0/super-secret-token".to_string(),
        };
        let err = require_secure_scheme("ops", &sink).unwrap_err();
        assert!(matches!(
            err,
            SinkConfigError::InsecureScheme { kind: "slack", .. }
        ));
        assert!(!err.to_string().contains("hooks.slack.com"));
    }

    /// fails if a `Json` sink over `http://` is refused. Unlike Discord and
    /// Slack, a `Json` sink's endpoint is the operator's own — pointing it
    /// at an internal service with no TLS in front of it is a legitimate
    /// arrangement, not a footgun.
    #[test]
    fn a_json_sink_over_http_is_accepted() {
        let sink = Sink::Json {
            url: "http://127.0.0.1:8080/hook".to_string(),
            body: None,
        };
        require_secure_scheme("ops", &sink).unwrap();
    }
}
