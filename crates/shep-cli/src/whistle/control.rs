//! The four tools that act, and the only ones the gate can withhold.
//!
//! Registered only when [`super::gate::Control::Allowed`]; when the gate is
//! shut this router is never constructed, so `tools/list` omits them and
//! `tools/call` on one answers rmcp's own `-32602 tool not found`.
//!
//! `ToolAnnotations` is wire-visible: an agent host reads it to decide
//! whether to ask a human first, so a mutating tool must never claim
//! `readOnlyHint: true`.

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use shep_core::protocol::{Request, Response, SelectorSpec};
use shep_core::status::ProcStatus;

use super::Whistle;
use super::facts::{FlockListing, SheepRow};
use super::read::SheepName;
use super::shepherd;

// vis = "pub(crate)": the generated constructor is private by default, but
// `Whistle::new` in the parent module calls it directly.
#[tool_router(router = control_router, vis = "pub(crate)")]
impl Whistle {
    /// Restarts an already-registered sheep by name; cannot start a script
    /// or Flockfile the flock does not already know, since that would be
    /// arbitrary code execution handed to a model.
    ///
    /// The running check is TOCTOU: it reads `Request::Describe` before
    /// `Request::Restart` ever reaches the wire, across two separate
    /// connections, so a sheep that comes online in the gap is restarted
    /// anyway. Refuses the whole call if any matched instance is running.
    #[tool(
        name = "start_sheep",
        description = "Start a registered sheep that is currently stopped. Cannot register new processes — the sheep must already be in the flock. The running check is a courtesy, not a guarantee: a sheep that comes up between the check and the call is restarted. For a multi-instance app, the whole call is refused if any instance is running.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    pub async fn start_sheep(
        &self,
        Parameters(SheepName { name }): Parameters<SheepName>,
    ) -> Result<Json<FlockListing>, CallToolResult> {
        let flock = match self
            .shepherd
            .call(Request::Describe {
                selector: SelectorSpec::Name(name.clone()),
            })
            .await?
        {
            Response::Described(flock) => flock,
            _ => return Err(unexpected_response()),
        };
        let running = flock
            .iter()
            .filter(|info| matches!(info.status, ProcStatus::Online | ProcStatus::Starting))
            .count();
        if running > 0 {
            // whistle's own refusal: nothing reaches the wire past this
            // point. Names the count too for a multi-instance app.
            let message = if flock.len() > 1 {
                format!(
                    "{name}: {running} of {} instances are already running; use restart_sheep",
                    flock.len()
                )
            } else {
                format!("{name} is already running; use restart_sheep")
            };
            return Err(shepherd::own_refusal("already_running", message));
        }
        match self
            .shepherd
            .call(Request::Restart {
                selector: SelectorSpec::Name(name),
            })
            .await?
        {
            Response::Restarted(flock) => Ok(Json(FlockListing {
                flock: flock.iter().map(SheepRow::from).collect(),
            })),
            _ => Err(unexpected_response()),
        }
    }

    /// Stop a sheep. It stays registered.
    #[tool(
        name = "stop_sheep",
        description = "Stop a running sheep through the graceful kill ladder. The sheep stays registered and can be started again. Whatever it was doing stops.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    pub async fn stop_sheep(
        &self,
        Parameters(SheepName { name }): Parameters<SheepName>,
    ) -> Result<Json<FlockListing>, CallToolResult> {
        let selector = SelectorSpec::Name(name);
        match self.shepherd.call(Request::Stop { selector }).await? {
            Response::Stopped(flock) => Ok(Json(FlockListing {
                flock: flock.iter().map(SheepRow::from).collect(),
            })),
            _ => Err(unexpected_response()),
        }
    }

    /// Restart a sheep: kill, then spawn.
    #[tool(
        name = "restart_sheep",
        description = "Restart a sheep: the current process is killed and a new one spawned. There is a gap with no process running. Use reload_sheep instead if the app must stay reachable.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    pub async fn restart_sheep(
        &self,
        Parameters(SheepName { name }): Parameters<SheepName>,
    ) -> Result<Json<FlockListing>, CallToolResult> {
        let selector = SelectorSpec::Name(name);
        match self.shepherd.call(Request::Restart { selector }).await? {
            Response::Restarted(flock) => Ok(Json(FlockListing {
                flock: flock.iter().map(SheepRow::from).collect(),
            })),
            _ => Err(unexpected_response()),
        }
    }

    /// Reload a sheep: spawn the replacement, then drain the old one.
    #[tool(
        name = "reload_sheep",
        description = "Reload a sheep. Usually a replacement is spawned and made ready before the old process is drained, which is an overlap rather than zero downtime: mid-swap both are alive. An app with a readiness probe and no reuse_port is drained first instead, so it does have a gap. Refused while a reload of the same app is already in flight. The reply is an acceptance, not a finished swap.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    pub async fn reload_sheep(
        &self,
        Parameters(SheepName { name }): Parameters<SheepName>,
    ) -> Result<Json<FlockListing>, CallToolResult> {
        let selector = SelectorSpec::Name(name);
        match self.shepherd.call(Request::Reload { selector }).await? {
            Response::Reloading(flock) => Ok(Json(FlockListing {
                flock: flock.iter().map(SheepRow::from).collect(),
            })),
            _ => Err(unexpected_response()),
        }
    }
}

/// A reply shape none of these four tools asked for. `Response` is
/// `#[non_exhaustive]`, so a variant this client predates, or simply the
/// wrong one for the request sent, maps here instead of being guessed at.
///
/// Duplicated rather than shared with `read.rs`'s identical helper: each
/// stays private to its own module.
fn unexpected_response() -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": "internal",
        "message": "the shepherd answered with a response this client does not understand",
    }))
}

// unix only: these fixtures bind a raw `UnixListener`, though the real
// transport (`shep_core::transport`) is portable.
#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use shep_core::protocol::{
        Envelope, Hello, HelloReply, Reply, RpcError, RpcErrorCode, decode_frame, encode_frame,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::task::JoinHandle;

    use shep_core::paths::ShepPaths;

    use super::*;
    use crate::whistle::gate;

    /// How long a test waits for a tool call before treating it as hung.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// A [`shep_core::protocol::HelloAck`] whose version matches this
    /// binary, since `sample_ack`'s fixed `"9.9.9"` would be refused by
    /// the guard in `Shepherd::call_with_ack`.
    fn matching_ack() -> shep_core::protocol::HelloAck {
        shep_core::protocol::HelloAck {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            ..shep_client::testing::sample_ack()
        }
    }

    fn whistle_at(socket: std::path::PathBuf) -> Whistle {
        // Only `socket` is read by a control tool; the rest of `ShepPaths`
        // can be nonexistent, `barks` included.
        let paths = ShepPaths {
            home: std::path::PathBuf::new(),
            daemon_config: std::path::PathBuf::new(),
            dogs_config: std::path::PathBuf::new(),
            snapshot: std::path::PathBuf::new(),
            logs: std::path::PathBuf::new(),
            pids: std::path::PathBuf::new(),
            run: std::path::PathBuf::new(),
            socket,
            barks: std::path::PathBuf::from("/nonexistent/barks.jsonl"),
            kv: std::path::PathBuf::new(),
            overrides: std::path::PathBuf::new(),
        };
        Whistle::new(paths, gate::Control::Allowed)
    }

    async fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await.unwrap();
        buf
    }

    async fn write_frame<T: serde::Serialize>(stream: &mut UnixStream, value: &T) {
        let bytes = encode_frame(value).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&bytes).await.unwrap();
    }

    async fn answer_handshake(stream: &mut UnixStream) {
        let hello_bytes = read_frame(stream).await;
        let _hello: Hello = decode_frame(&hello_bytes).unwrap();
        let ack: HelloReply = Ok(matching_ack());
        write_frame(stream, &ack).await;
    }

    /// Binds one listener and answers each new connection, in order, with
    /// the next reply from `replies`: one reply per connection, since each
    /// tool call opens its own.
    ///
    /// Panics if `replies` runs out before connections do, or on any
    /// accept/handshake/decode/encode failure.
    fn serve_connections_in_sequence(
        path: &Path,
        replies: Vec<Result<Response, (RpcErrorCode, String)>>,
    ) -> JoinHandle<Vec<Envelope>> {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let mut envelopes = Vec::with_capacity(replies.len());
            for result in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                answer_handshake(&mut stream).await;
                let request_bytes = read_frame(&mut stream).await;
                let envelope: Envelope = decode_frame(&request_bytes).unwrap();
                let reply = Reply {
                    id: envelope.id,
                    result: result.map_err(|(code, message)| RpcError {
                        code,
                        message,
                        daemon_version: None,
                    }),
                };
                write_frame(&mut stream, &reply).await;
                envelopes.push(envelope);
            }
            envelopes
        })
    }

    /// Accepts one connection, answers the handshake, counts the one
    /// request that follows, then never replies. Binds exactly once, so a
    /// retried second connection would find nothing listening.
    fn serve_one_request_then_hang(path: &Path) -> (JoinHandle<()>, Arc<AtomicU32>) {
        let listener = UnixListener::bind(path).unwrap();
        let served = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&served);
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            answer_handshake(&mut stream).await;
            let _ = read_frame(&mut stream).await;
            counter.fetch_add(1, Ordering::SeqCst);
            core::future::pending::<()>().await
        });
        (handle, served)
    }

    /// The shared counter reads 1, not 2: the refusal happens before
    /// `Request::Restart` reaches the wire.
    #[tokio::test]
    async fn start_sheep_refuses_a_running_sheep_and_names_restart_sheep() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        // sample_info() is already Online, named "web".
        let (daemon, served) = shep_client::testing::fake_daemon_accepting_repeatedly_with_ack(
            &socket,
            matching_ack(),
            Response::Described(vec![shep_client::testing::sample_info()]),
        );

        let whistle = whistle_at(socket);
        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            whistle.start_sheep(Parameters(SheepName {
                name: "web".to_string(),
            })),
        )
        .await
        .expect("start_sheep must return within the test timeout")
        .err()
        .expect("an already-running sheep must be refused");

        assert_eq!(result.is_error, Some(true));
        let message = result.structured_content.expect("structured content")["message"]
            .as_str()
            .expect("a string")
            .to_string();
        assert!(message.contains("web"), "must name the sheep: {message}");
        assert!(
            message.contains("already running"),
            "must say why: {message}"
        );
        assert!(
            message.contains("restart_sheep"),
            "the refusal must name the way forward: {message}"
        );

        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "the Describe reaches the wire; the Restart must not"
        );
        daemon.abort();
    }

    /// `start_sheep` and `restart_sheep` are one daemon path:
    /// `Request::Restart` respawns a sheep that is not running.
    #[tokio::test]
    async fn start_sheep_sends_a_restart_for_a_stopped_sheep() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let mut stopped = shep_client::testing::sample_info();
        stopped.status = shep_core::status::ProcStatus::Stopped;
        let mut restarted = shep_client::testing::sample_info();
        restarted.status = shep_core::status::ProcStatus::Starting;

        let served = serve_connections_in_sequence(
            &socket,
            vec![
                Ok(Response::Described(vec![stopped])),
                Ok(Response::Restarted(vec![restarted])),
            ],
        );

        let whistle = whistle_at(socket);
        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            whistle.start_sheep(Parameters(SheepName {
                name: "web".to_string(),
            })),
        )
        .await
        .expect("start_sheep must return within the test timeout")
        .expect("a stopped sheep must be started, not refused");

        assert_eq!(result.0.flock.len(), 1);
        assert_eq!(result.0.flock[0].status, "starting");

        let envelopes = served.await.expect("the fake daemon task must not panic");
        assert_eq!(envelopes.len(), 2, "Describe, then Restart");
        match &envelopes[0].body {
            Request::Describe { selector } => {
                assert_eq!(selector, &SelectorSpec::Name("web".to_string()));
            }
            other => panic!("expected Describe first, got {other:?}"),
        }
        match &envelopes[1].body {
            Request::Restart { selector } => {
                assert_eq!(selector, &SelectorSpec::Name("web".to_string()));
            }
            other => panic!("expected Restart second, got {other:?}"),
        }
    }

    /// Four instances, two online: refuses the whole call and names the
    /// count. The shared counter stays at 1; no second request is sent.
    #[tokio::test]
    async fn start_sheep_refuses_the_whole_call_when_any_instance_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let rows: Vec<_> = [
            shep_core::status::ProcStatus::Online,
            shep_core::status::ProcStatus::Online,
            shep_core::status::ProcStatus::Stopped,
            shep_core::status::ProcStatus::Stopped,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, status)| {
            let mut info = shep_client::testing::sample_info();
            info.id = u32::try_from(i).unwrap() + 1;
            info.name = "api".to_string();
            info.status = status;
            info
        })
        .collect();

        let (daemon, served) = shep_client::testing::fake_daemon_accepting_repeatedly_with_ack(
            &socket,
            matching_ack(),
            Response::Described(rows),
        );

        let whistle = whistle_at(socket);
        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            whistle.start_sheep(Parameters(SheepName {
                name: "api".to_string(),
            })),
        )
        .await
        .expect("start_sheep must return within the test timeout")
        .err()
        .expect("a partly-running app must be refused, not partly started");

        let message = result.structured_content.expect("structured content")["message"]
            .as_str()
            .expect("a string")
            .to_string();
        assert!(message.contains("api"), "must name the app: {message}");
        assert!(message.contains("2 of 4"), "must name the count: {message}");
        assert!(message.contains("restart_sheep"));

        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "the whole call is refused before a second (Restart) request is ever sent"
        );
        daemon.abort();
    }

    /// The message and `RpcErrorCode::Internal` pass through verbatim,
    /// though `rpc.rs` documents that code as wrong but decodable.
    #[tokio::test]
    async fn a_reload_already_in_flight_reaches_the_model_in_the_shepherds_own_words() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let served = serve_connections_in_sequence(
            &socket,
            vec![Err((
                RpcErrorCode::Internal,
                "api is already being reloaded".to_string(),
            ))],
        );

        let whistle = whistle_at(socket);
        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            whistle.reload_sheep(Parameters(SheepName {
                name: "api".to_string(),
            })),
        )
        .await
        .expect("reload_sheep must return within the test timeout")
        .err()
        .expect("a daemon-side refusal must surface as a tool error");

        assert_eq!(result.is_error, Some(true));
        let structured = result
            .structured_content
            .expect("a refusal carries structured content a model can branch on");
        assert_eq!(structured["message"], "api is already being reloaded");
        assert_eq!(
            structured["code"], "internal",
            "and the code, so a model can tell a conflict from a not-found: {structured}"
        );

        served.await.expect("the fake daemon task must not panic");
    }

    /// A retried mutating call would be two outages, not one. Runs on a
    /// paused clock, which auto-advances past the client's deadline
    /// (`DEFAULT_DEADLINE` + `DEADLINE_GRACE`) without a real wait.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_control_call_is_reported_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let (daemon, served) = serve_one_request_then_hang(&socket);

        let whistle = whistle_at(socket);
        let result = whistle
            .restart_sheep(Parameters(SheepName {
                name: "web".to_string(),
            }))
            .await
            .err()
            .expect("a daemon that never answers must surface as a tool error, not hang");

        let message = result.structured_content.expect("structured content")["message"]
            .as_str()
            .expect("a string")
            .to_string();
        assert!(
            message.contains("no reply within"),
            "must be the client-side deadline firing, not something else: {message}"
        );
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "exactly one request must reach the daemon; a retry would need a second \
             connection, and this listener never accepts one"
        );
        daemon.abort();
    }
}
