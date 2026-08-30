//! The four tools that act, and the only ones the gate can withhold.
//!
//! Registered only when [`super::gate::Control::Allowed`]; when the gate is
//! shut this router is never constructed and its tools are absent from
//! `tools/list` entirely, so `tools/call` on one answers rmcp's own
//! `-32602 tool not found`. A model cannot be tempted by a tool it cannot see.
//!
//! **Annotations are decisions here, not defaults.** `ToolAnnotations` is a
//! wire-visible field an agent host reads to decide whether to ask a human
//! first, so a mutating tool annotated `readOnlyHint: true` would be a lie
//! told to a machine. Each value below is argued in the plan's "The nine
//! tools" section.

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use shep_core::protocol::{Request, Response, SelectorSpec};
use shep_core::status::ProcStatus;

use super::Whistle;
use super::facts::{FlockListing, SheepRow};
use super::read::SheepName;
use super::shepherd;

// `vis = "pub(crate)"` for the same reason `read.rs` carries it: the macro's
// generated constructor is private by default and `Whistle::new`
// (`whistle/mod.rs`) calls it from the PARENT module. See the plan's
// "Every rmcp API this plan names" section.
#[tool_router(router = control_router, vis = "pub(crate)")]
impl Whistle {
    /// Start a registered sheep that is not currently running.
    ///
    /// Deliberately narrow: this takes the NAME of a sheep the flock already
    /// has, and cannot introduce a process that was not already registered.
    /// `shep start` accepts a script path or a Flockfile, and a tool with that
    /// shape would be arbitrary code execution as the operator, handed to a
    /// model. No gate makes that acceptable, because the gate is not a
    /// security boundary. A wider `start` is a different tool with a different
    /// name and its own approval story, not a widening of this one.
    ///
    /// The running check reads the CURRENT state over `Request::Describe`
    /// and refuses in-process, before `Request::Restart` ever reaches the
    /// wire, when any matched instance is already `online` or `starting`.
    /// It is a courtesy, not a guarantee — see the plan's TOCTOU note: the
    /// check and the restart are two separate connections
    /// (`shepherd.rs`'s "one connection per call"), and a sheep that comes
    /// online in the gap is restarted anyway, because `Request::Restart`
    /// does not re-check.
    ///
    /// A multi-instance app refuses the WHOLE call if ANY matched instance
    /// is running — never "restart the stopped ones and skip the rest".
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
            // Whistle's OWN refusal — nothing reaches the wire past this
            // point. A single matched instance names the sheep alone; a
            // multi-instance app names the count too, so a model can tell
            // "one of one" from "two of four" rather than guessing.
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
        description = "Reload a sheep with zero downtime: a replacement is spawned and made ready before the old process is drained. Refused while a reload of the same app is already in flight. The reply is an acceptance, not a finished swap.",
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
/// `#[non_exhaustive]` (Global Constraints), so an answer this match does
/// not recognise — a variant this client predates, or simply the wrong one
/// for the request just sent — maps here rather than being guessed at.
///
/// Module-private and duplicated rather than shared with `read.rs`'s
/// identical helper: that one is private to its own module too, and a
/// cross-module reach for four lines of code would couple two files that
/// otherwise have no reason to know about each other.
fn unexpected_response() -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": "internal",
        "message": "the shepherd answered with a response this client does not understand",
    }))
}

// `unix` because its cases bind a raw `UnixListener` to stand in for a live shepherd.
// The transport itself is portable (`shep_core::transport`) and its
// own tests cover both platforms; these fixtures simply predate the
// seam and were never rewritten onto it.
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

    /// How long a test waits before deciding a tool call hung rather than
    /// failed — IR-46: every await in a test needs a forcing mechanism.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// A [`shep_core::protocol::HelloAck`] this binary's own version guard
    /// never refuses — `shep_client::testing::sample_ack`'s fixed `"9.9.9"`
    /// always would, now that every tool call in this file goes through
    /// `Shepherd::call_with_ack`'s guard.
    fn matching_ack() -> shep_core::protocol::HelloAck {
        shep_core::protocol::HelloAck {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            ..shep_client::testing::sample_ack()
        }
    }

    fn whistle_at(socket: std::path::PathBuf) -> Whistle {
        // None of these four tools ever reads `barks.jsonl` — that path is
        // `read::list_barks`' alone — so a nonexistent one is fine here, and
        // every other `ShepPaths` field beside `socket` is likewise unread
        // by anything a control tool does.
        let paths = ShepPaths {
            home: std::path::PathBuf::new(),
            daemon_config: std::path::PathBuf::new(),
            snapshot: std::path::PathBuf::new(),
            logs: std::path::PathBuf::new(),
            pids: std::path::PathBuf::new(),
            run: std::path::PathBuf::new(),
            socket,
            barks: std::path::PathBuf::from("/nonexistent/barks.jsonl"),
            kv: std::path::PathBuf::new(),
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

    /// Binds ONE listener and answers each new connection, in order, with
    /// the next scripted reply — success or error — from `replies`. A
    /// reply PER CONNECTION, not per request: `shepherd.rs`'s "one
    /// connection per call" means `start_sheep`'s Describe-then-Restart
    /// round trip opens two separate connections, and unlike
    /// `shep_client::testing::fake_daemon_accepting_repeatedly` (which
    /// answers every connection identically) this needs to hand the SECOND
    /// one a different reply than the first.
    ///
    /// Binding ONCE and looping `accept()` — rather than unbinding and
    /// rebinding `path` between connections, the way `shepherd.rs`'s own
    /// `two_calls_survive_a_shepherd_that_restarted_in_between` does when
    /// the TEST ITSELF drives both calls sequentially — avoids racing the
    /// tool's own second `connect()` (which nothing here controls the
    /// scheduling of) against a rebind that has not happened yet.
    ///
    /// Panics if `replies` runs out before connections do, or on any
    /// accept/handshake/decode/encode failure — test scaffolding, same
    /// failure mode as `shep_client::testing`'s own fakes.
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

    /// Binds, accepts one connection, answers the handshake, reads the one
    /// request that follows (counting it), and then never replies — the
    /// shape a control tool's client-side deadline exists to catch. Unlike
    /// `shep_client::testing::fake_client_that_never_replies`, this does not
    /// connect its own `Client`: the tool under test makes its own
    /// connection via `Shepherd`, so a fake that dialed first would just be
    /// a second, unused peer.
    ///
    /// Binds exactly once and never loops back to `accept()`, so a retried
    /// second connection would find nothing listening — proof by
    /// construction, on top of the counter, that a second request cannot
    /// silently succeed unnoticed.
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

    /// fails if `start_sheep` stops refusing a running sheep, or starts
    /// refusing it with a message that names no way forward. The refusal is
    /// whistle's OWN — it happens after the `Describe` check but before
    /// `Request::Restart` reaches the wire, which is why the shared counter
    /// below reads 1, not 2: a second request WOULD be recorded if one were
    /// sent, and the assertion is that none was.
    #[tokio::test]
    async fn start_sheep_refuses_a_running_sheep_and_names_restart_sheep() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        // sample_info() is already Online, named "web" — exactly the
        // already-running case this test needs.
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

    /// fails if `start_sheep` stops working for a sheep that IS stopped.
    /// This is the tool's whole reason to exist, and it is
    /// `Request::Restart` on the wire — `supervisor.rs`'s
    /// `ManualKind::Restart` respawns a sheep that is not running, so
    /// "start" and "restart" are one daemon path.
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

    /// fails if a partly-running multi-instance app is partly started. A
    /// four-instance `api` with two online must refuse the WHOLE call and
    /// say how many — never "restart the stopped two and skip the rest".
    /// `supervisor.rs:424-432` is explicit that a partly-accepted selector
    /// leaves the caller unable to tell which half was taken, and a model
    /// is the caller least able to work it out.
    ///
    /// The fake daemon answers the `Describe` with four rows, two `Online`;
    /// the assertion is that NO second request arrived (the shared counter
    /// reads 1, not 2) and that the message carries both the count and
    /// `restart_sheep`.
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

    /// fails if a daemon refusal stops reaching the model intact. The
    /// message asserted here is the shepherd's own, verbatim:
    /// `supervisor.rs`'s `SupervisorError::ReloadInFlight` renders as
    /// "<name> is already being reloaded" and arrives as
    /// `RpcErrorCode::Internal` — a code `rpc.rs` itself documents as
    /// wrong-but-decodable. whistle passes both through.
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

    /// fails if a mutating call is ever retried. A `restart_sheep` whose
    /// reply was merely slow, retried, is two outages. The fake daemon here
    /// answers the handshake, reads the one request that follows, and then
    /// never replies; the assertion is that exactly ONE request arrived and
    /// the tool reported a timeout rather than a second attempt.
    ///
    /// IR-46: bounded by the client's own deadline (`DEFAULT_DEADLINE` +
    /// `DEADLINE_GRACE`, 7s) against a paused clock, which auto-advances
    /// once nothing else is runnable — the same shape
    /// `request_reply.rs`'s `a_deadline_expires_client_side_when_the_daemon_never_answers`
    /// already uses, so this never waits seven real seconds.
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
