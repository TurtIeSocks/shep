//! One connection per tool call.
//!
//! lookout holds a long-lived connection with a reconnect ladder and a freeze
//! state, because a dashboard that loses its shepherd must keep showing what
//! it last knew. whistle has no screen: a tool call is one request and one
//! reply, so this connects, sends, and drops. A shepherd restarted between two
//! calls is invisible — no stale handle, no ladder, no state machine.
//!
//! The cost is one `connect(2)` and one handshake per call, over a local unix
//! socket, between calls a model makes seconds apart.

use std::path::{Path, PathBuf};

use rmcp::model::CallToolResult;
use shep_client::{Client, ConnectError, RequestError};
use shep_core::protocol::{HelloAck, Request, Response};

use crate::exit::ExitCode;

/// The socket, and the one operation anything in `whistle` performs on it.
///
/// Not an error enum, so IR-20's `#[non_exhaustive]` rule does not apply; and
/// shep-cli is `[[bin]]`-only, so nothing here is in a library crate at all.
///
/// `super::read`'s five tools and `super::control`'s four both call
/// [`Self::call`]/[`Self::call_with_ack`] through `Whistle::shepherd`, one
/// held per `Whistle`. `Whistle::new` (`whistle/mod.rs`) builds that
/// `Shepherd` with [`Self::new`], from `ShepPaths::socket`.
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
    /// **Never `connect_or_spawn`.** `shep start` and `shep muster` autostart a
    /// shepherd because a person asked them to; a model calling `list_flock`
    /// against a machine with no daemon running must be told so, not handed a
    /// daemon it did not ask for.
    ///
    /// # Errors
    ///
    /// A [`CallToolResult`] with `is_error: true` — never an
    /// [`rmcp::ErrorData`] — carrying the shepherd's own message. See
    /// [`refusal`] for why the distinction is load-bearing.
    pub async fn call(&self, request: Request) -> Result<Response, CallToolResult> {
        self.call_with_ack(request)
            .await
            .map(|(_ack, response)| response)
    }

    /// [`Self::call`], plus the handshake the connection was opened with.
    ///
    /// `get_metrics` needs `daemon_version` and `daemon_pid` for
    /// `super::facts::MetricsReading`, and those live on the [`Client`]
    /// (`Client::daemon() -> &HelloAck`, shep-client/src/client.rs:175) which
    /// [`Self::call`] drops before it returns. Rather than making every caller
    /// deal with a tuple, `call` delegates here and throws the ack away.
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
        let ack = client.daemon().clone();
        let response = client.request(request).await.map_err(|err| refusal(&err));
        // Dropping the client ends its actor task and closes the socket. Done
        // explicitly rather than by scope end so the ordering is visible: the
        // reply is already in hand.
        let _ = client.close().await;
        response.map(|response| (ack, response))
    }
}

/// A connect failure, as an in-band tool error naming the socket ONCE.
///
/// `ConnectError`'s own `Display` already prints
/// ``could not connect to `<path>`: <source>`` (shep-client's
/// connection.rs:78-80), so this wrapper does not repeat the path — it adds
/// only the words that say what is missing rather than what failed, which is
/// what a model can act on.
///
/// [`Shepherd::call_with_ack`] is its only call site — reached from every
/// tool in `super::read` and `super::control`.
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
/// **`CallToolResult::structured_error`, not `Err(ErrorData)`.** rmcp turns an
/// `Err(ErrorData)` into a JSON-RPC protocol error — `impl IntoCallToolResult
/// for ErrorData` returns `Err(self)` (rmcp handler/server/tool.rs:119-123) —
/// and MCP reserves protocol errors for unknown tools and malformed params. A
/// host is free to show one to the user and not to the model. A daemon refusal
/// is an execution failure the model must see and can act on, so it goes
/// in-band with `is_error: true` (rmcp model.rs:3990).
///
/// The daemon's message is passed through unaltered, including the cases where
/// its code is imprecise — `rpc.rs` maps `SupervisorError::ReloadInFlight` to
/// `RpcErrorCode::Internal` and says in its own comment that it does so "under
/// protest", the right answer being a conflict code the wire does not have
/// yet. A model reading "api is already being reloaded" can act on that. A
/// model reading a nicer code whistle invented would be reading fiction.
///
/// Reached the same way [`connect_refusal`] is: through `super::read`'s and
/// `super::control`'s tools.
fn refusal(err: &RequestError) -> CallToolResult {
    let (code, message) = match err {
        // `ExitCode::from(RpcErrorCode)` then `code_str()`, rather than a
        // second `match` spelling the codes out here: `exit.rs` is already the
        // one place this binary decides how a daemon error code is spelled
        // (`not_found`, `invalid_config`, ...) — see exit.rs:71 and :95 — and
        // a copy would be a second spelling to drift. The MESSAGE is untouched
        // — no lowercasing, no rewrapping — because it routinely carries an
        // app's own name, and `Api` is not `api`.
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

/// whistle's OWN refusal, before anything reaches the wire.
///
/// One shape for both kinds, so a model never has to learn two. `super::read`
/// (Task 6) is the first caller — an unreadable log file (`tail_bleats`) and
/// an unreadable `barks.jsonl` (`list_barks`) both go through this, rather
/// than each tool inventing its own error shape. `start_sheep`'s
/// already-running refusal (`whistle/control.rs`, Task 7) will be a third.
///
/// Not an error enum, so IR-20's `#[non_exhaustive]` rule does not apply; and
/// shep-cli is `[[bin]]`-only, so nothing here is in a library crate at all.
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

    /// fails if a daemon-side refusal stops reaching the model verbatim, or
    /// stops being IN-BAND. shep does not paraphrase the shepherd: "api is
    /// already being reloaded" is actionable and a whistle-invented
    /// replacement is not — but a message a host routes to the user instead
    /// of the model is just as lost. `is_error: true` on a `CallToolResult`
    /// is what keeps it in front of the model; an `Err(ErrorData)` becomes a
    /// JSON-RPC protocol error (rmcp handler/server/tool.rs:119-123) and the
    /// host decides.
    #[test]
    fn a_daemon_refusal_is_an_in_band_error_keeping_its_own_message() {
        let result = refusal(&RequestError::Rpc(RpcError {
            code: RpcErrorCode::Internal,
            message: "api is already being reloaded".to_string(),
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

    /// fails if an unreachable shepherd stops naming the socket. "connection
    /// refused" alone tells a model nothing it can act on; the path is what
    /// an operator greps for.
    ///
    /// `ConnectError::Connect` is a STRUCT variant carrying both fields
    /// (`crates/shep-client/src/connection.rs:44-49`) — constructing it as a
    /// tuple variant does not compile.
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
        // ONCE, not twice. `ConnectError`'s own `Display` already prints
        // ``could not connect to `<path>` `` (connection.rs:78-80), so a
        // wrapper that prepends the path too says it twice — which reads as
        // two different sockets to anything skimming.
        assert_eq!(
            message.matches("/nonexistent/shep/run/shep.sock").count(),
            1,
            "the socket path appears once, not once per layer: {message}"
        );
    }

    /// fails if `call` starts holding a connection between calls. Two calls
    /// against a shepherd that was restarted in between must both succeed —
    /// this is the whole reason there is no ladder here.
    ///
    /// IR-46: bounded, because a `call` that hung on a dead handle would
    /// otherwise hang the suite rather than fail it.
    #[tokio::test]
    async fn two_calls_survive_a_shepherd_that_restarted_in_between() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());

        let (first, first_served) =
            shep_client::testing::fake_daemon_accepting_repeatedly(&socket, Response::Pong);
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
        // AWAITED, not just aborted. `JoinHandle::abort` only REQUESTS
        // cancellation — the task's resources, the bound listener among
        // them, are released when it is actually reclaimed. On unix that
        // race is invisible because the socket file is unlinked explicitly
        // below; on Windows the still-live pipe instance makes the rebind
        // fail with `ERROR_ACCESS_DENIED`, since `Listener::bind` asks for
        // `first_pipe_instance`. Awaiting the cancellation is what makes
        // "the shepherd went away" actually true before the next one binds.
        let _ = first.await;
        // Unix only: on that tier the listener leaves a socket FILE behind
        // that outlives the aborted task, so the address stays connectable
        // until it is unlinked and the test would not be simulating a
        // departed shepherd without this. A named pipe has no directory
        // entry and stops existing when its last handle closes — which the
        // abort above already did — so there is nothing to remove, and
        // asking to remove it fails with `ERROR_INVALID_PARAMETER` (87)
        // rather than doing nothing.
        #[cfg(unix)]
        std::fs::remove_file(&socket).unwrap();

        let (second, _second_served) =
            shep_client::testing::fake_daemon_accepting_repeatedly(&socket, Response::Pong);
        let two = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            shepherd.call(Request::Ping),
        )
        .await
        .expect("the second call finished within ten seconds");
        assert!(two.is_ok(), "a fresh connection per call needs no ladder");
        second.abort();
    }
}
