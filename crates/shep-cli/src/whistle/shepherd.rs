//! One connection per tool call.
//!
//! lookout holds a long-lived connection with a reconnect ladder and a
//! freeze state, because a dashboard must keep showing what it last knew.
//! whistle has no screen: it connects, sends, and drops. A shepherd
//! restarted between two calls is invisible; there is no stale handle to
//! notice it.
//!
//! One `connect(2)` and one handshake per call, over a local unix socket,
//! is cheap between calls a model makes seconds apart.

use std::path::{Path, PathBuf};

use rmcp::model::CallToolResult;
use shep_client::{Client, ConnectError, RequestError};
use shep_core::protocol::{HelloAck, Request, Response};

use crate::exit::ExitCode;

/// The socket, and the one operation anything in `whistle` performs on it.
///
/// `super::read`'s five tools and `super::control`'s four both call
/// [`Self::call`]/[`Self::call_with_ack`] through `Whistle::shepherd`, one
/// held per `Whistle`.
#[derive(Debug, Clone)]
pub struct Shepherd {
    socket: PathBuf,
}

impl Shepherd {
    /// Wraps a socket path. Connects to nothing until [`Self::call`].
    #[must_use]
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Connects, sends one request, and drops the connection.
    ///
    /// Never `connect_or_spawn`: a model calling `list_flock` against a
    /// machine with no daemon running must be told so, not handed one it
    /// did not ask for.
    ///
    /// # Errors
    ///
    /// A [`CallToolResult`] with `is_error: true`, never an
    /// [`rmcp::ErrorData`], carrying the shepherd's own message.
    pub async fn call(&self, request: Request) -> Result<Response, CallToolResult> {
        self.call_with_ack(request)
            .await
            .map(|(_ack, response)| response)
    }

    /// [`Self::call`], plus the handshake the connection was opened with.
    ///
    /// `get_metrics` needs `daemon_version` and `daemon_pid`, which live on
    /// the [`Client`] that [`Self::call`] drops before returning; `call`
    /// delegates here and throws the ack away.
    ///
    /// # Errors
    ///
    /// As [`Self::call`].
    pub async fn call_with_ack(
        &self,
        request: Request,
    ) -> Result<(HelloAck, Response), CallToolResult> {
        let client = Client::connect(&self.socket)
            .await
            .map_err(|err| connect_refusal(&self.socket, &err))?;
        // `whistle` drives the daemon on every tool call, so it can never be
        // one of `RECOVERY_VERBS`.
        refuse_if_skewed(&client)?;
        let ack = client.daemon().clone();
        let response = client.request(request).await.map_err(|err| refusal(&err));
        // Dropping the client ends its actor task and closes the socket. Done
        // explicitly rather than by scope end so the ordering is visible: the
        // reply is already in hand.
        let _ = client.close().await;
        response.map(|response| (ack, response))
    }
}

/// Refuses `client` if its shepherd disagrees with this binary's crate
/// version, reusing [`crate::refuse_version_skew`].
///
/// Never writes to stdout: `out` is [`std::io::sink`] and only `err` is
/// the real stderr, since stdout is whistle's MCP transport. The model
/// sees a separate, in-band [`CallToolResult`] built here.
///
/// # Errors
///
/// A [`CallToolResult`] with `is_error: true` when the shepherd's crate
/// version differs from this binary's.
fn refuse_if_skewed(client: &Client) -> Result<(), CallToolResult> {
    let mut sink = std::io::sink();
    let mut err = std::io::stderr();
    let mut streams = crate::output::Streams {
        out: &mut sink,
        err: &mut err,
        style: crate::style::Presentation::BARE,
        fmt: crate::cli::Format::Table,
    };
    crate::refuse_version_skew(&mut streams, client, crate::VersionGuard::Enforce).map_err(
        |_code| {
            CallToolResult::structured_error(serde_json::json!({
                "code": crate::exit::ExitCode::VersionSkew.code_str(),
                "message": format!(
                    "this shep is {}, the running shepherd is {}; \
                     `cargo install shep` replaced the binary without \
                     restarting it — run `shep daemon reload`",
                    env!("CARGO_PKG_VERSION"),
                    client.daemon().daemon_version,
                ),
            }))
        },
    )
}

/// A connect failure, as an in-band tool error naming the socket once.
///
/// `ConnectError`'s `Display` already prints the path, so this wrapper adds
/// only the words that say what is missing rather than what failed.
fn connect_refusal(socket: &Path, err: &ConnectError) -> CallToolResult {
    let _ = socket; // named in the signature for call-site readability; the
    // path itself comes out of `err`'s own Display.
    CallToolResult::structured_error(serde_json::json!({
        "code": "no_shepherd",
        "message": format!("no shepherd is running: {err}"),
    }))
}

/// A request failure, as an in-band tool error carrying the shepherd's words.
///
/// `structured_error`, not `Err(ErrorData)`: MCP reserves protocol errors
/// for unknown tools and malformed params, and a daemon refusal is an
/// execution failure the model must see and can act on.
///
/// The message passes through unaltered, even when the code is imprecise:
/// `rpc.rs` maps `SupervisorError::ReloadInFlight` to `RpcErrorCode::Internal`
/// under protest, since the wire has no conflict code yet.
fn refusal(err: &RequestError) -> CallToolResult {
    let (code, message) = match err {
        // `exit.rs` is the one place this binary spells a daemon error
        // code; a second `match` here would drift. The message is
        // untouched: it routinely carries an app's own name.
        RequestError::Rpc(rpc) => (
            ExitCode::from(rpc.code).code_str().to_string(),
            rpc.message.clone(),
        ),
        // `Timeout`, `Closed` and `Wire` each have a `Display` that already
        // says what happened in one clause; there is nothing to add.
        other => ("transport".to_string(), other.to_string()),
    };
    CallToolResult::structured_error(serde_json::json!({
        "code": code,
        "message": message,
    }))
}

/// whistle's own refusal, before anything reaches the wire.
///
/// One shape for every caller: an unreadable log file (`tail_bleats`), an
/// unreadable `barks.jsonl` (`list_barks`), and `start_sheep`'s
/// already-running refusal all go through this.
pub fn own_refusal(code: &str, message: String) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": code,
        "message": message,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::{RpcError, RpcErrorCode};

    /// A [`HelloAck`] whose version [`refuse_if_skewed`] never refuses,
    /// since `sample_ack`'s `"9.9.9"` always would.
    fn matching_ack() -> HelloAck {
        HelloAck {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            ..shep_client::testing::sample_ack()
        }
    }

    /// shep does not paraphrase the shepherd: `is_error: true` keeps the
    /// message in front of the model, where an `Err(ErrorData)` would
    /// become a host-routed protocol error instead.
    #[test]
    fn a_daemon_refusal_is_an_in_band_error_keeping_its_own_message() {
        let result = refusal(&RequestError::Rpc(RpcError {
            code: RpcErrorCode::Internal,
            message: "api is already being reloaded".to_string(),
            daemon_version: None,
        }));
        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .expect("a refusal carries structured content a model can branch on");
        assert_eq!(structured["message"], "api is already being reloaded");
        assert_eq!(
            structured["code"], "internal",
            "and the code, so a model can tell a conflict from a not-found: {structured}"
        );
    }

    /// "connection refused" alone tells a model nothing it can act on; the
    /// path is what an operator greps for.
    ///
    /// `ConnectError::Connect` is a struct variant, not a tuple one.
    #[test]
    fn an_unreachable_shepherd_names_the_socket_once() {
        let socket = std::path::Path::new("/nonexistent/shep/run/shep.sock");
        let result = connect_refusal(
            socket,
            &ConnectError::Connect {
                path: socket.to_path_buf(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            },
        );
        assert_eq!(result.is_error, Some(true));
        let message = result.structured_content.expect("structured")["message"]
            .as_str()
            .expect("a string")
            .to_string();
        assert!(message.contains("/nonexistent/shep/run/shep.sock"));
        assert!(
            message.contains("no shepherd"),
            "and says what is missing, not just what failed: {message}"
        );
        // Once, not twice: `ConnectError`'s own `Display` already prints
        // the path, so a wrapper that prepends it too reads as two sockets.
        assert_eq!(
            message.matches("/nonexistent/shep/run/shep.sock").count(),
            1,
            "the socket path appears once, not once per layer: {message}"
        );
    }

    /// Bounded: a `call` that hung on a dead handle would otherwise hang
    /// the suite rather than fail it.
    #[tokio::test]
    async fn two_calls_survive_a_shepherd_that_restarted_in_between() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());

        let (first, first_served) = shep_client::testing::fake_daemon_accepting_repeatedly_with_ack(
            &socket,
            matching_ack(),
            Response::Pong,
        );
        let shepherd = Shepherd::new(socket.clone());
        let one = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            shepherd.call(Request::Ping),
        )
        .await
        .expect("the first call finished within ten seconds");
        assert!(one.is_ok());
        assert_eq!(
            first_served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one call is one connection — not zero, and not a retry"
        );

        // The shepherd goes away entirely: task aborted, socket file removed.
        // A `Shepherd` holding a connection would be holding a dead one.
        first.abort();
        // Awaited, not just aborted: `abort` only requests cancellation,
        // and the bound listener is released only once it is reclaimed.
        // On Windows an unreclaimed pipe fails the rebind below with
        // `ERROR_ACCESS_DENIED`.
        let _ = first.await;
        // Unix only: the listener leaves a socket file behind that outlives
        // the aborted task, so it must be unlinked to simulate a departed
        // shepherd. A named pipe has no directory entry to remove.
        #[cfg(unix)]
        std::fs::remove_file(&socket).unwrap();

        let (second, _second_served) =
            shep_client::testing::fake_daemon_accepting_repeatedly_with_ack(
                &socket,
                matching_ack(),
                Response::Pong,
            );
        let two = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            shepherd.call(Request::Ping),
        )
        .await
        .expect("the second call finished within ten seconds");
        assert!(two.is_ok(), "a fresh connection per call needs no ladder");
        second.abort();
    }

    /// Must refuse without writing to stdout, the one byte this module's
    /// doc says must never happen.
    #[tokio::test]
    async fn a_version_skewed_shepherd_is_an_in_band_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let ack = HelloAck {
            daemon_version: "0.1.8".to_string(),
            protocol: shep_core::protocol::PROTOCOL_VERSION,
            pid: 4242,
        };
        let (client, _fake) = shep_client::testing::fake_client_with_ack(&addr, ack).await;

        let result = super::refuse_if_skewed(&client).expect_err("a skew must be refused");
        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .expect("a refusal carries structured content a model can branch on");
        assert_eq!(structured["code"], "version_skew");
        let message = structured["message"].as_str().expect("a string message");
        assert!(message.contains(env!("CARGO_PKG_VERSION")), "{message}");
        assert!(message.contains("0.1.8"), "{message}");
    }

    /// A shepherd of this binary's own version is not a skew.
    #[tokio::test]
    async fn a_matching_version_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let ack = HelloAck {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: shep_core::protocol::PROTOCOL_VERSION,
            pid: 4242,
        };
        let (client, _fake) = shep_client::testing::fake_client_with_ack(&addr, ack).await;

        super::refuse_if_skewed(&client).expect("a matching version is not a skew");
    }
}
