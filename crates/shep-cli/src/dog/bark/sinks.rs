//! Bark's sinks: [`Sink`], the pure [`render_body`], and the async
//! [`deliver`] that POSTs a rendered body to it.
//!
//! Hand-rolled HTTP/1.1 over `tokio-rustls`, not `reqwest`: fewer
//! transitive dependencies and no C toolchain. The connect-and-TLS setup
//! lives in `crate::fetch`, shared with `shep dogs --available`.
//!
//! `http://` is accepted for a [`Sink::Json`] endpoint; only
//! [`require_secure_scheme`] rejects it, for Discord and Slack. No
//! redirect is ever followed. This module's own tests exercise only the
//! plaintext path; the TLS handshake is `rustls`'s tested surface, not
//! this module's.

use core::fmt;
use std::time::Duration;

use serde::Deserialize;
use shep_client::dogs::DogConfig;
use shep_core::barks::Bark;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::fetch::{self, Target};

/// One named entry under `[dog.bark.sinks]`.
///
/// `Debug` is REDACTED (IR-41): every variant carries a webhook URL, and a
/// Discord or Slack webhook URL is a bearer credential — anyone holding it
/// can post to that channel. A sink printed into a log, a panic message or
/// an error chain leaks it to whoever reads the log.
///
/// `#[shep(secret)]` says the same thing to a schema that the `Debug` says
/// to a log. It reaches a pane only for a dog whose whole config IS a sink,
/// since the marks a schema carries are the ones the type shep asked about
/// declared; bark's own section is asked as [`super::BarkConfig`], which
/// marks the map instead and says why.
#[derive(Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema, DogConfig)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Sink {
    /// A Discord webhook: `{"content": "..."}`.
    Discord {
        /// The webhook URL.
        #[shep(secret)]
        url: String,
    },
    /// A Slack incoming webhook: `{"text": "..."}`.
    Slack {
        /// The webhook URL.
        #[shep(secret)]
        url: String,
    },
    /// A JSON POST with a body the operator templates.
    Json {
        /// Where to POST.
        #[shep(secret)]
        url: String,
        /// The body, with `{subject}`, `{rule}`, `{message}` and `{at_ms}`
        /// substituted. Defaults to an object carrying all four.
        body: Option<String>,
    },
}

/// Manual: a derived `Debug` would print `url` in full. Every variant
/// collapses to `Sink::<Variant> { url: <redacted> }`.
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
    /// Not exposed past this module: [`deliver`] and
    /// [`require_secure_scheme`] are the only callers.
    fn url(&self) -> &str {
        match self {
            Self::Discord { url } | Self::Slack { url } | Self::Json { url, .. } => url,
        }
    }

    /// `"discord"`/`"slack"` for the two HTTPS-only kinds, `None` for
    /// [`Sink::Json`], an operator's own endpoint that may have no TLS.
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
/// `Debug` needs no redaction, unlike [`Sink`]'s own: it carries only the
/// sink's config key, never its url.
///
/// Not `#[non_exhaustive]`: shep-cli is `[[bin]]`-only with no published
/// surface, so no downstream match needs protecting from a new variant.
#[derive(Debug)]
pub enum SinkConfigError {
    /// Sink `name` is a [`Sink::Discord`] or [`Sink::Slack`] (`kind`) not
    /// configured with `https://`.
    InsecureScheme {
        /// The sink's config key under `[dog.bark.sinks]`, never the url.
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
/// A Discord or Slack webhook url is the bearer credential; `http://`
/// would let anyone on the wire capture it. discord.com and slack.com
/// serve `https://` only, so no legitimate use is removed.
/// [`Sink::Json`] is left permissive: an operator's own endpoint may
/// legitimately have no TLS.
///
/// # Errors
/// - [`SinkConfigError::InsecureScheme`]: `sink` is [`Sink::Discord`] or
///   [`Sink::Slack`] with a url not starting `https://`.
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
/// `Debug` needs no redaction, unlike [`Sink`]'s own: every field is an OS
/// error, status code, or response line, never a webhook url.
#[derive(Debug)]
pub enum SinkError {
    /// The rendered body is not valid JSON: a templated `body` can
    /// produce this, the default body cannot.
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

impl From<std::io::Error> for SinkError {
    fn from(source: std::io::Error) -> Self {
        Self::Transport { source }
    }
}

/// The body `sink` sends for `bark`: pure, and the half worth testing
/// exhaustively.
///
/// # Errors
/// - [`SinkError::Template`]: the rendered body is not valid JSON, which a
///   templated `body` can produce.
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

/// Substitutes `{subject}`, `{rule}`, `{message}` and `{at_ms}` in
/// `template` with `bark`'s own fields, JSON-escaped except `at_ms`, which
/// is a bare number.
///
/// A single forward pass, never sequential `.replace()` calls: a
/// substituted value can itself contain another token's literal text (a
/// sheep named `{at_ms}`), and a sequential replace would rewrite it on a
/// later pass. `rest` only shrinks from the front, so a token can never
/// match inside text already written to `out`.
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

/// `s`, escaped for use inside a JSON string's quotes, not a JSON string
/// literal itself: [`substitute`]'s own template already supplies the
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
/// - [`SinkError::Template`]: as [`render_body`].
/// - [`SinkError::Transport`]: the request failed or timed out.
/// - [`SinkError::Status`]: the endpoint answered outside 2xx, carrying
///   the status and the first line of the body.
pub async fn deliver(sink: &Sink, bark: &Bark, timeout: Duration) -> Result<(), SinkError> {
    let body = render_body(sink, bark)?;
    let target = fetch::parse_url(sink.url()).map_err(|source| SinkError::Transport {
        source: std::io::Error::other(source),
    })?;
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

/// The request line, headers and blank line [`deliver_inner`] sends ahead
/// of `body`. `Host` names the port only when it is off the scheme's own
/// default (443/80).
fn build_request(target: &Target, body: &str) -> String {
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
async fn deliver_inner(target: &Target, body: &str) -> Result<(), SinkError> {
    let request = build_request(target, body);
    let tcp = TcpStream::connect((target.host.as_str(), target.port)).await?;
    if target.https {
        let domain =
            ServerName::try_from(target.host.clone()).map_err(|source| SinkError::Transport {
                source: std::io::Error::other(source),
            })?;
        let tls = fetch::tls_connector().connect(domain, tcp).await?;
        write_and_read(tls, &request).await
    } else {
        write_and_read(tcp, &request).await
    }
}

/// Writes `request`, flushes, then reads back the status line (and, on a
/// non-2xx, one diagnostic line of body).
///
/// The explicit `flush` matters on the TLS branch: `tokio-rustls` buffers
/// writes in `rustls`'s own record layer, and skipping it can leave a
/// request sitting in a buffer the peer never sees. The same flush on a
/// plain `TcpStream` is redundant, not wrong.
async fn write_and_read<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &str,
) -> Result<(), SinkError> {
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    read_response(stream).await
}

/// Reads the status line off `stream`; a 2xx reply is read no further. A
/// non-2xx reads past the remaining header lines to the blank line that
/// ends them, or to EOF, then takes one more line for
/// [`SinkError::Status`]'s diagnostic.
async fn read_response<S: AsyncRead + Unpin>(stream: S) -> Result<(), SinkError> {
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).await?;
    let code = parse_status_code(&status_line)?;
    if (200..300).contains(&code) {
        return Ok(());
    }

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut diagnostic = String::new();
    reader.read_line(&mut diagnostic).await?;
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

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;
    use crate::http::{HttpRequest, read_request, write_response};

    /// Every variant's `url` carries the credential marker; nothing else
    /// does. The key is spelled out here, not read from
    /// `shep_core::dogs::SECRET_KEY`, since the constant and the schema
    /// agreeing is the thing under test.
    #[test]
    fn every_sink_variant_marks_its_url_and_leaves_the_rest_plain() {
        let schema = shep_client::dogs::config_schema::<Sink>()
            .expect("`url` is a property of all three variants");
        let variants = schema
            .as_value()
            .get("oneOf")
            .and_then(|it| it.as_array())
            .expect("an internally tagged enum is a oneOf");
        assert_eq!(variants.len(), 3);

        for variant in variants {
            assert_eq!(
                variant.pointer("/properties/url/x-shep-secret"),
                Some(&serde_json::Value::Bool(true)),
                "a webhook URL is a bearer credential in every variant"
            );
            assert_eq!(
                variant.pointer("/properties/kind/x-shep-secret"),
                None,
                "the tag names the variant and is not a credential"
            );
        }
    }

    /// A fired alert; only `subject` and `message` vary across these tests.
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

    /// Binds an ephemeral port, accepts one connection, answers
    /// `status`/`body`, and hands the captured request back. Never a real
    /// webhook.
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

    /// An app named `we"b` would break the template's JSON if its name
    /// were interpolated raw.
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

    /// The literal token `{at_ms}` embedded in `bark.message` must not be
    /// rewritten by a later per-field substitution pass.
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

    /// Against a local server, never a real webhook.
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

    /// Discord's rate-limit 429 arrives exactly this way.
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

    #[test]
    fn a_sinks_debug_never_prints_its_webhook() {
        let rendered = format!("{:?}", discord_sink());
        assert_eq!(rendered, "Sink::Discord { url: <redacted> }");
        assert!(!rendered.contains("discord.com"));
    }

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

    /// Unlike Discord and Slack, a `Json` sink's endpoint is the
    /// operator's own; plain `http://` is legitimate.
    #[test]
    fn a_json_sink_over_http_is_accepted() {
        let sink = Sink::Json {
            url: "http://127.0.0.1:8080/hook".to_string(),
            body: None,
        };
        require_secure_scheme("ops", &sink).unwrap();
    }

    // `Sink` has no `#[serde(flatten)]` near `deny_unknown_fields`, unlike
    // `rules::Rule`. These tests parse real TOML rather than building
    // `Sink` directly, so a future flatten conflict here would be caught.

    /// The exact inline-table shape `docs/dogs.md` publishes.
    #[test]
    fn the_docs_discord_sink_parses_from_toml() {
        let sink: Sink = toml::from_str(
            r#"kind = "discord"
url = "https://discord.com/api/webhooks/..."
"#,
        )
        .unwrap();
        assert_eq!(
            sink,
            Sink::Discord {
                url: "https://discord.com/api/webhooks/...".to_owned(),
            }
        );
    }

    /// Not in the docs' worked example, but a documented `kind`.
    #[test]
    fn a_slack_sink_parses_from_toml() {
        let sink: Sink = toml::from_str(
            r#"kind = "slack"
url = "https://hooks.slack.com/services/T0/B0/tok"
"#,
        )
        .unwrap();
        assert_eq!(
            sink,
            Sink::Slack {
                url: "https://hooks.slack.com/services/T0/B0/tok".to_owned(),
            }
        );
    }

    /// The `audit` fragment `docs/dogs.md` publishes; `body` stays `None`.
    #[test]
    fn the_docs_json_sink_parses_from_toml() {
        let sink: Sink = toml::from_str(
            r#"kind = "json"
url = "https://example.internal/hook"
"#,
        )
        .unwrap();
        assert_eq!(
            sink,
            Sink::Json {
                url: "https://example.internal/hook".to_owned(),
                body: None,
            }
        );
    }

    #[test]
    fn a_json_sink_s_body_template_parses_from_toml() {
        let sink: Sink = toml::from_str(
            r#"kind = "json"
url = "https://example.internal/hook"
body = "{\"text\": \"{message}\"}"
"#,
        )
        .unwrap();
        assert_eq!(
            sink,
            Sink::Json {
                url: "https://example.internal/hook".to_owned(),
                body: Some(r#"{"text": "{message}"}"#.to_owned()),
            }
        );
    }

    #[test]
    fn a_misspelled_sink_field_is_refused_with_the_bad_key_named() {
        let err = toml::from_str::<Sink>(
            r#"kind = "discord"
urll = "https://discord.com/api/webhooks/..."
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("urll"),
            "the error must name the misspelled key, not just fail: {err}"
        );
    }

    #[test]
    fn an_unknown_sink_kind_is_refused_with_the_bad_value_named() {
        let err = toml::from_str::<Sink>(
            r#"kind = "discrod"
url = "https://discord.com/api/webhooks/..."
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("discrod"),
            "the error must name the bad value: {err}"
        );
    }
}
