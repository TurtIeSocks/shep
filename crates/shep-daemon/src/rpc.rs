//! Portable RPC dispatch: verb routing, typed errors, per-call deadlines
//!
//! `dispatch` is the one function the connection layer (the unix-socket /
//! named-pipe server in this crate's private `server` module) calls per
//! request envelope. Everything here
//! compiles and tests on every platform — no `cfg(unix)`, no sockets, no
//! bytes on a wire. [`RpcContext`] bundles the daemon-wide handles a request
//! handler may touch; `Outcome` tells the caller what to do next (reply,
//! start forwarding bus events, or begin shutdown).
//!
//! ## Deadlines
//!
//! Every envelope gets a `budget`: its own `deadline_ms` (clamped to
//! `MAX_DEADLINE_MS` — a peer cannot pin a daemon task open forever), or
//! `DEFAULT_DEADLINE_MS` if it sent none. `dispatch`'s own doc explains
//! exactly what expiring a budget does and does not undo.

use core::future::Future;
use core::time::Duration;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, watch};

use shep_core::config::normalize_all;
use shep_core::protocol::{
    BusEvent, Envelope, ProcessInfo, Reply, Request, Response, RpcError, RpcErrorCode, SelectorSpec,
};
use shep_core::selector::ProcessSelector;

use crate::bus::TopicFilter;
use crate::snapshot::{FlockRegistry, SnapshotError, write_atomic};
use crate::supervisor::{SupervisorError, SupervisorHandle};

/// Deadline applied when a client sends none (spec §6: 5s default).
pub(crate) const DEFAULT_DEADLINE_MS: u64 = 5_000;
/// Ceiling on a client-supplied deadline — a peer cannot pin a daemon task open.
pub(crate) const MAX_DEADLINE_MS: u64 = 60_000;

/// Everything a request handler may touch — one clone per connection.
///
/// Every clone shares the same supervisor engine, event bus sender, flock
/// registry, and shutdown signal; the connection layer builds one from the
/// daemon's shared state and hands it to `dispatch` once per envelope.
///
/// The type is public because `tests/daemon_e2e.rs` holds one to drive
/// [`Self::shutdown`] and [`Self::snapshot_now`] without going through the
/// socket. Its fields are not: every one of them is filled in by `boot` and
/// read by `dispatch`, both in this crate.
#[derive(Clone, Debug)]
pub struct RpcContext {
    /// The supervisor engine this daemon is running.
    pub(crate) supervisor: SupervisorHandle,
    /// The daemon-wide event bus; `Subscribe` compiles a [`TopicFilter`] the
    /// connection layer hands to [`crate::bus::spawn_forwarder`] alongside a
    /// receiver off this sender.
    pub(crate) events: broadcast::Sender<BusEvent>,
    /// The muster roll's in-memory app registry — `Start` records into it.
    pub(crate) registry: FlockRegistry,
    /// Where [`Self::snapshot_now`] writes the muster roll.
    pub(crate) snapshot_path: PathBuf,
    /// This daemon's crate version, echoed in the handshake.
    pub(crate) daemon_version: String,
    /// This daemon's OS pid, echoed in the handshake.
    pub(crate) pid: u32,
    /// Flips to `true` to start graceful daemon shutdown; see [`Self::shutdown`].
    pub(crate) shutdown: Arc<watch::Sender<bool>>,
}

/// Where a muster roll landed and what it recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedRoll {
    /// The path written.
    pub path: PathBuf,
    /// How many apps the roll records.
    pub apps: u32,
}

impl RpcContext {
    /// Asks the daemon to begin graceful shutdown.
    ///
    /// Only flips the watch signal — the connection/server layer is what
    /// actually runs the kill ladder and closes listeners once it observes
    /// this go `true`. `dispatch` never calls this itself: `KillDaemon`
    /// only reports the intent via `Outcome::Shutdown`, leaving the caller
    /// to trigger it after the reply is on the wire.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Writes the muster roll now, reporting what it recorded.
    ///
    /// `None` means the supervisor engine has already stopped: there is
    /// nothing left to record and the shutdown path has already written the
    /// final roll.
    ///
    /// # Errors
    /// - [`SnapshotError`] — as `write_atomic`.
    pub async fn save_roll_now(&self) -> Result<Option<SavedRoll>, SnapshotError> {
        let Ok(infos) = self.supervisor.list_checked().await else {
            return Ok(None);
        };
        let roll = self.registry.roll(&infos, crate::now_ms());
        write_atomic(&self.snapshot_path, &roll)?;
        Ok(Some(SavedRoll {
            path: self.snapshot_path.clone(),
            // `u32` matches `SavedApp::instances_running`; a flock large
            // enough to overflow it has other problems.
            apps: u32::try_from(roll.apps.len()).unwrap_or(u32::MAX),
        }))
    }

    /// Writes the muster roll now, discarding what it recorded.
    ///
    /// # Errors
    /// - [`SnapshotError`] — as `write_atomic`.
    pub async fn snapshot_now(&self) -> Result<(), SnapshotError> {
        self.save_roll_now().await.map(|_| ())
    }
}

/// What the connection layer must do with a dispatched request.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Send this reply and keep reading.
    Reply(Reply),
    /// Send this reply, then start forwarding events through `filter`.
    Subscribe {
        /// The `Subscribed` (or error) reply to send first.
        reply: Reply,
        /// Compiled topic matcher for [`crate::bus::spawn_forwarder`].
        filter: TopicFilter,
    },
    /// Send this reply, then trigger daemon shutdown and close.
    Shutdown(Reply),
}

/// The deadline this envelope gets: its own, clamped, or the default.
#[must_use]
pub(crate) fn budget(deadline_ms: Option<u64>) -> Duration {
    // clamp's lower bound is 1ms so a literal `0` means "expire immediately"
    // rather than silently becoming "no deadline at all".
    Duration::from_millis(
        deadline_ms
            .unwrap_or(DEFAULT_DEADLINE_MS)
            .clamp(1, MAX_DEADLINE_MS),
    )
}

/// Dispatches one request envelope against `ctx`, returning what the
/// connection layer must do with the result.
///
/// The deadline [`budget`] computes bounds *the reply*, not the actor's
/// work: dropping the work future (via `with_deadline`'s timeout) only
/// stops the daemon waiting on the supervisor — the command already handed
/// to a sheep-owning task keeps running to completion. So a
/// `DeadlineExceeded` reply to `Start` means "no answer within your
/// budget," not "nothing happened"; a client that retries must reconcile
/// with `ListFlock` rather than assume its request never landed. Anything
/// stronger would need per-command cancellation inside the actor, which the
/// supervisor's locked `Command` surface (Phase 2a) deliberately does not
/// have.
pub(crate) async fn dispatch(envelope: Envelope, ctx: &RpcContext) -> Outcome {
    let id = envelope.id;
    with_deadline(
        id,
        budget(envelope.deadline_ms),
        run(id, envelope.body, ctx),
    )
    .await
}

// `+ Send`: this future is awaited inside the per-connection tokio::spawn, so
// the bound is stated rather than inferred (Global Constraints).
async fn with_deadline<F: Future<Output = Outcome> + Send>(
    id: u64,
    budget: Duration,
    work: F,
) -> Outcome {
    match tokio::time::timeout(budget, work).await {
        Ok(outcome) => outcome,
        Err(_) => Outcome::Reply(Reply {
            id,
            result: Err(RpcError {
                code: RpcErrorCode::DeadlineExceeded,
                message: format!(
                    "the request deadline of {} ms expired before the daemon finished",
                    budget.as_millis()
                ),
            }),
        }),
    }
}

async fn run(id: u64, request: Request, ctx: &RpcContext) -> Outcome {
    let reply = |result| Outcome::Reply(Reply { id, result });
    match request {
        Request::Ping => reply(Ok(Response::Pong)),
        Request::ListFlock => match ctx.supervisor.list_checked().await {
            Ok(infos) => reply(Ok(Response::Flock(infos))),
            Err(err) => reply(Err(rpc_error(&err))),
        },
        Request::Describe { selector } => match selector_of(selector) {
            Err(err) => reply(Err(err)),
            Ok(selector) => match ctx.supervisor.list_checked().await {
                Err(err) => reply(Err(rpc_error(&err))),
                Ok(infos) => {
                    let hits: Vec<_> = infos
                        .into_iter()
                        .filter(|i| selector.matches(&i.name, i.id, i.fold.as_deref()))
                        .collect();
                    if hits.is_empty() {
                        reply(Err(not_found()))
                    } else {
                        reply(Ok(Response::Described(hits)))
                    }
                }
            },
        },
        // Peer input is untrusted: re-normalize before anything is registered.
        Request::Start { apps } => match normalize_all(apps) {
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
            })),
            Ok(resolved) => {
                ctx.registry.record(&resolved);
                match ctx.supervisor.start(resolved).await {
                    Ok(infos) => reply(Ok(Response::Started(infos))),
                    Err(err) => reply(Err(rpc_error(&err))),
                }
            }
        },
        Request::Stop { selector } => {
            selector_call(id, selector, |s| ctx.supervisor.stop(s), Response::Stopped).await
        }
        Request::Restart { selector } => {
            selector_call(
                id,
                selector,
                |s| ctx.supervisor.restart(s),
                Response::Restarted,
            )
            .await
        }
        // `Reloading` names an acceptance, and the deadline machinery above
        // is why it has to. The supervisor answers this one as soon as the
        // reload is taken, before the first replacement is spawned, so the
        // reply lands well inside any budget; a reply that waited for the
        // swaps would routinely outlive `MAX_DEADLINE_MS` and be abandoned by
        // `with_deadline` while the reload it asked for went on running.
        Request::Reload { selector } => {
            selector_call(
                id,
                selector,
                |s| ctx.supervisor.reload(s),
                Response::Reloading,
            )
            .await
        }
        Request::Reopen { selector } => {
            selector_call(
                id,
                selector,
                |s| ctx.supervisor.reopen(s),
                Response::Reopened,
            )
            .await
        }
        Request::Flush { selector } => {
            selector_call(id, selector, |s| ctx.supervisor.flush(s), Response::Flushed).await
        }
        Request::Trigger {
            selector,
            action,
            params,
        } => trigger(id, selector, action, params, ctx).await,
        Request::Delete { selector } => match selector_of(selector) {
            Err(err) => reply(Err(err)),
            Ok(selector) => match ctx.supervisor.delete(selector).await {
                Ok(ids) => reply(Ok(Response::Deleted(ids))),
                Err(err) => reply(Err(rpc_error(&err))),
            },
        },
        Request::SaveRoll => match ctx.save_roll_now().await {
            Ok(Some(saved)) => reply(Ok(Response::RollSaved {
                // Lossy on purpose, matching `to_info`'s treatment of log
                // paths: a non-UTF-8 roll path must degrade one field, not
                // abort the whole reply.
                path: saved.path.to_string_lossy().into_owned(),
                apps: saved.apps,
            })),
            Ok(None) => reply(Err(RpcError {
                code: RpcErrorCode::Internal,
                message: "the supervisor engine has stopped; no roll was written".to_string(),
            })),
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::Internal,
                message: err.to_string(),
            })),
        },
        Request::Subscribe { topics } => match TopicFilter::new(&topics) {
            Ok(filter) => Outcome::Subscribe {
                reply: Reply {
                    id,
                    result: Ok(Response::Subscribed),
                },
                filter,
            },
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
            })),
        },
        Request::KillDaemon => Outcome::Shutdown(Reply {
            id,
            result: Ok(Response::ShuttingDown),
        }),
        // `Request` is #[non_exhaustive]: a verb from a newer client that this
        // daemon has never heard of is an error, not a panic.
        _ => reply(Err(RpcError {
            code: RpcErrorCode::Internal,
            message: "this daemon does not implement that request".to_string(),
        })),
    }
}

fn rpc_error(err: &SupervisorError) -> RpcError {
    match err {
        SupervisorError::NotFound => not_found(),
        SupervisorError::SpawnFailed(msg) => RpcError {
            code: RpcErrorCode::SpawnFailed,
            message: msg.clone(),
        },
        // `Internal` — an "unexpected daemon-side failure", which a log path
        // the daemon can no longer open, or can no longer empty, both are.
        // No code of its own: the wire enum is versioned, and a client that
        // predates a new code cannot decode the reply at all, which would
        // cost the operator the message as well. The message names every
        // path that failed either way, and `err.to_string()` rather than the
        // bare payload so the reader is told which of the two it is —
        // `SupervisorError`'s `Display` is the only thing that still
        // distinguishes them once they share a code.
        SupervisorError::ReopenFailed(_) | SupervisorError::FlushFailed(_) => RpcError {
            code: RpcErrorCode::Internal,
            message: err.to_string(),
        },
        // `Internal` under protest, and the wire verb should revisit it
        // rather than inherit it: an app already being reloaded is a CONFLICT
        // the caller can act on — wait, or reload something else — and not an
        // unexpected daemon-side failure at all. A code of its own is the
        // right answer and is a wire change, not a mapping change:
        // `RpcErrorCode` is versioned, `RpcErrorCode::ALL` and shep-cli's
        // exit-code mapping both grow with it, and a client predating a new
        // code cannot decode the reply. Until that is deliberate, a less
        // precise code carrying the message beats a refusal an operator
        // cannot read — `Display` names the app, which is the part that says
        // what to do about it.
        SupervisorError::ReloadInFlight(_) => RpcError {
            code: RpcErrorCode::Internal,
            message: err.to_string(),
        },
        SupervisorError::EngineStopped => RpcError {
            code: RpcErrorCode::Internal,
            message: "the supervisor engine has stopped".to_string(),
        },
    }
}

fn not_found() -> RpcError {
    RpcError {
        code: RpcErrorCode::NotFound,
        message: "selector matched no registered sheep".to_string(),
    }
}

fn selector_of(spec: SelectorSpec) -> Result<ProcessSelector, RpcError> {
    ProcessSelector::try_from(spec).map_err(|err| RpcError {
        code: RpcErrorCode::InvalidConfig,
        message: err.to_string(),
    })
}

/// The helper every selector-in, flock-out verb shares: convert the selector,
/// call the supervisor, map the hits through the passed `Response`
/// constructor.
///
/// The future bound is stated, not inferred, because the whole chain is
/// awaited inside the per-connection `tokio::spawn`.
async fn selector_call<F, Fut>(
    id: u64,
    spec: SelectorSpec,
    call: F,
    ok: fn(Vec<ProcessInfo>) -> Response,
) -> Outcome
where
    F: FnOnce(ProcessSelector) -> Fut + Send,
    Fut: Future<Output = Result<Vec<ProcessInfo>, SupervisorError>> + Send,
{
    let result = match selector_of(spec) {
        Ok(selector) => call(selector).await.map(ok).map_err(|err| rpc_error(&err)),
        Err(err) => Err(err),
    };
    Outcome::Reply(Reply { id, result })
}

/// `Trigger`'s own resolve-then-map path. [`selector_call`] cannot serve
/// it: that helper is typed `Fut: Future<Output = Result<Vec<ProcessInfo>,
/// SupervisorError>>` with `ok: fn(Vec<ProcessInfo>) -> Response`, and
/// `Response::Triggered` carries `Vec<ActionReply>` — a distinct row
/// `ProcessInfo` cannot hold a reply body on. Everything else about the shape
/// is the same: convert the selector, call the supervisor, map the rows.
///
/// How long each app gets to answer is not decided here any more: it is
/// `AppConfig::action_timeout`, one value per matched sheep rather than one
/// for the whole flock, read off each sheep's own config where the wait is
/// armed (`Actor::begin_action`). What used to be this function's own
/// `ACTION_TIMEOUT` constant is now that field's problem, including staying
/// under [`DEFAULT_DEADLINE_MS`] — `shep_core::config::normalize` refuses a
/// value no caller could ever be given enough deadline to outlast; a value
/// merely past the *default* budget is accepted; the caller's own deadline is
/// what decides whether that pays off.
async fn trigger(
    id: u64,
    spec: SelectorSpec,
    action: String,
    params: Option<String>,
    ctx: &RpcContext,
) -> Outcome {
    let result = match selector_of(spec) {
        Err(err) => Err(err),
        Ok(selector) => ctx
            .supervisor
            .trigger(selector, action, params)
            .await
            .map(Response::Triggered)
            .map_err(|err| rpc_error(&err)),
    };
    Outcome::Reply(Reply { id, result })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::ProcScript;
    use crate::testing::harness;
    use shep_core::config::AppConfig;
    use shep_core::protocol::{
        ActionOutcome, ActionReply, Request, Response, RpcErrorCode, SelectorSpec,
    };
    use shep_core::status::ProcStatus;
    use shep_core::values::UpDuration;

    fn envelope(id: u64, body: Request) -> Envelope {
        Envelope {
            id,
            deadline_ms: None,
            body,
        }
    }

    fn reply_of(outcome: Outcome) -> Reply {
        match outcome {
            Outcome::Reply(reply) | Outcome::Subscribe { reply, .. } | Outcome::Shutdown(reply) => {
                reply
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ping_answers_pong_on_the_same_envelope_id() {
        let h = harness(vec![]);
        let reply = reply_of(dispatch(envelope(9, Request::Ping), &h.ctx).await);
        assert_eq!(reply.id, 9);
        assert_eq!(reply.result.unwrap(), Response::Pong);
    }

    #[tokio::test(start_paused = true)]
    async fn start_registers_the_config_and_lists_it() {
        let h = harness(vec![ProcScript::never_exits()]);
        let started = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("web", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Started(infos) = started.result.unwrap() else {
            panic!("expected started")
        };
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].status, ProcStatus::Online);

        // The roll can only be built if Start recorded the config.
        let roll = h.ctx.registry.roll(&infos, 0);
        assert_eq!(roll.apps.len(), 1);
        assert_eq!(roll.apps[0].app.script, "./srv");

        let listed = reply_of(dispatch(envelope(2, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else {
            panic!("expected flock")
        };
        assert_eq!(flock.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn start_re_normalizes_untrusted_peer_config() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    #[tokio::test(start_paused = true)]
    async fn a_selector_matching_nothing_is_not_found() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Stop {
                        selector: SelectorSpec::Name("ghost".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::NotFound);
    }

    #[tokio::test(start_paused = true)]
    async fn a_bad_peer_regex_is_invalid_config_not_a_panic() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Describe {
                        selector: SelectorSpec::Regex("((".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    #[tokio::test(start_paused = true)]
    async fn describe_filters_by_fold() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let mut api = AppConfig::minimal("api", "./a");
        api.fold = Some("backend".to_string());
        dispatch(
            envelope(
                1,
                Request::Start {
                    apps: vec![api, AppConfig::minimal("web", "./w")],
                },
            ),
            &h.ctx,
        )
        .await;
        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Describe {
                        selector: SelectorSpec::Fold("backend".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Described(hits) = reply.result.unwrap() else {
            panic!("expected described")
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "api");
    }

    /// Fails if `Reopen` is left to the catch-all arm at the bottom of
    /// `run` — a verb this daemon has never heard of — which answers
    /// `Internal` for a request it in fact implements. Also fails if it is
    /// routed to another verb's supervisor call: `Stop` would answer
    /// `Response::Stopped` and, worse, stop the sheep.
    #[tokio::test(start_paused = true)]
    async fn reopen_routes_to_the_supervisor_and_leaves_the_sheep_running() {
        let h = harness(vec![ProcScript::never_exits()]);
        dispatch(
            envelope(
                1,
                Request::Start {
                    apps: vec![AppConfig::minimal("web", "./srv")],
                },
            ),
            &h.ctx,
        )
        .await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Reopen {
                        selector: SelectorSpec::Name("web".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Reopened(infos) = reply.result.unwrap() else {
            panic!("expected reopened")
        };
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].status, ProcStatus::Online);

        let listed = reply_of(dispatch(envelope(3, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else {
            panic!("expected flock")
        };
        assert_eq!(
            flock[0].status,
            ProcStatus::Online,
            "a reopen must not disturb the sheep it reopens"
        );
    }

    /// Fails if `Reload` is left to the catch-all arm at the bottom of `run`,
    /// which answers `Internal` for a request this daemon in fact implements.
    ///
    /// Fails too if the arm is routed to another verb's supervisor call while
    /// keeping `Response::Reloading` as its answer — the shape a copy-pasted
    /// arm takes, and one no assertion on the reply alone can see. What
    /// separates a reload from every other verb is the flock it leaves
    /// behind: two entries in one instance slot, the drainee `Stopping` under
    /// its original id and a replacement `Starting` under a new one. A
    /// `restart` leaves one entry under the same id, a `stop` one entry
    /// `Stopped`.
    ///
    /// The mid-swap state is what is asserted, and it is not a race: nothing
    /// here advances the clock, so the replacement's readiness wait cannot
    /// elapse between the two dispatches. `ListFlock` is queued to an actor
    /// that has already finished `handle_reload` — the reply is sent inside
    /// it, before the swap starts, but the whole function runs to completion
    /// before the actor takes another message.
    ///
    /// Three scripts, of which a correct run uses two: the original and its
    /// replacement. The third is sized for the spawn a broken arm performs
    /// that a correct one does not — a `restart` misroute's respawn on top of
    /// a swap — so it lands as a live entry rather than as the
    /// `SpawnFailed("script exhausted")` that an exhausted pool turns into
    /// `Errored`, which reads like a different failure entirely.
    #[tokio::test(start_paused = true)]
    async fn reload_routes_to_the_supervisor_and_starts_a_swap() {
        let h = harness(vec![ProcScript::never_exits(); 3]);
        dispatch(
            envelope(
                1,
                Request::Start {
                    apps: vec![AppConfig::minimal("web", "./srv")],
                },
            ),
            &h.ctx,
        )
        .await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Reload {
                        selector: SelectorSpec::Name("web".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Reloading(accepted) = reply.result.unwrap() else {
            panic!("expected reloading")
        };
        assert_eq!(accepted.len(), 1);
        assert_eq!(
            accepted[0].status,
            ProcStatus::Online,
            "the answer is the flock as it stood when the reload was accepted"
        );

        let listed = reply_of(dispatch(envelope(3, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else {
            panic!("expected flock")
        };
        assert_eq!(
            flock.len(),
            2,
            "a swap in progress is two entries in one instance slot, not one: {flock:?}"
        );
        assert_eq!(flock[0].id, accepted[0].id);
        assert_eq!(flock[0].status, ProcStatus::Stopping);
        assert_ne!(
            flock[1].id, accepted[0].id,
            "the replacement takes a new id"
        );
        assert_eq!(flock[1].status, ProcStatus::Starting);
    }

    /// The whole of what an operator sees when a reload is refused, and both
    /// refusals reach them the same way: the daemon's code becomes the CLI's
    /// exit status, and the daemon's message is the only thing printed.
    ///
    /// Fails if either arm answers a code that is not `Internal` — the
    /// `RpcErrorCode` set is versioned, so neither refusal has one of its
    /// own, and `SupervisorError`'s `Display` is what is left to tell them
    /// apart. Fails too if `ReloadInFlight`'s arm drops the app's name: the
    /// name is the actionable half of that message, because it says which
    /// reload to wait for.
    ///
    /// `ReloadInFlight` reaching the wire at all is pinned in `supervisor`
    /// (`a_second_reload_of_an_app_already_reloading_is_refused`), as is a
    /// reload arriving after a shutdown has begun
    /// (`a_reload_is_refused_once_a_shutdown_has_begun`, which answers
    /// `EngineStopped`). What neither reaches is this mapping.
    #[test]
    fn a_refused_reload_is_internal_and_says_which_refusal_it_was() {
        let in_flight = rpc_error(&SupervisorError::ReloadInFlight("web".to_string()));
        assert_eq!(in_flight.code, RpcErrorCode::Internal);
        assert_eq!(in_flight.message, "web is already being reloaded");

        let shutting_down = rpc_error(&SupervisorError::EngineStopped);
        assert_eq!(shutting_down.code, RpcErrorCode::Internal);
        assert_eq!(shutting_down.message, "the supervisor engine has stopped");
    }

    /// Fails if `Reload` skips the selector conversion, or converts it
    /// without reporting the failure — a peer regex the daemon cannot compile
    /// is the client's usage error, not an internal one. A hand-rolled arm
    /// that answered `Reloading` correctly could still lose this; it is the
    /// shared `selector_call` helper that keeps it.
    #[tokio::test(start_paused = true)]
    async fn a_bad_reload_selector_is_invalid_config() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Reload {
                        selector: SelectorSpec::Regex("((".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    /// Fails if `Reopen` skips the selector conversion, or converts it
    /// without reporting the failure: a peer regex the daemon cannot compile
    /// is the client's usage error, not an internal one.
    #[tokio::test(start_paused = true)]
    async fn a_bad_reopen_selector_is_invalid_config() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Reopen {
                        selector: SelectorSpec::Regex("((".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    /// Fails if `Trigger` is left to the catch-all arm at the bottom of
    /// `run` — a verb this daemon has never heard of — which answers
    /// `Internal` for a request it in fact implements. Fails too if the row
    /// stops carrying the sheep it is about.
    ///
    /// `TimedOut` rather than `NoChannel` is the assertion that matters, and
    /// it is two claims at once. The action was really delivered and really
    /// waited on — `NoChannel` is what a build that never reached the sheep
    /// would answer — and `web`'s `action_timeout` (3s, `AppConfig::minimal`'s
    /// spec default) elapsed INSIDE the request's own budget. Raise it past
    /// [`DEFAULT_DEADLINE_MS`] and this stops being a row at all:
    /// `with_deadline` fires first and the reply becomes `DeadlineExceeded`,
    /// which names no sheep and says nothing about which of them answered —
    /// pinned right below, in `an_oversized_action_timeout_loses_the_race`.
    /// The ordering is what both cases pin; the constants are pinned in
    /// `budgets_default_and_clamp`.
    ///
    /// Nothing answers because the harness keeps no handle on its runner, so
    /// no case here can put a reply on a child's end of the channel. An app's
    /// own words reaching a caller is pinned a tier down, in
    /// `a_triggered_action_answers_with_the_apps_reply`.
    #[tokio::test(start_paused = true)]
    async fn trigger_routes_to_the_flock_and_reports_each_match_within_the_budget() {
        // Two apps, not one: ids start at 0, so a single-app harness would
        // give `web` id 0 — indistinguishable from a row-mapping bug that
        // drops the real id and leaves the field's default. Registering
        // `other` first pushes `web` to id 1, a value only the real mapping
        // produces.
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let mut web = AppConfig::minimal("web", "./srv");
        web.channel = true;
        let started = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("other", "./o"), web],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Started(started) = started.result.unwrap() else {
            panic!("expected started")
        };
        let web_id = started
            .iter()
            .find(|i| i.name == "web")
            .expect("web registered")
            .id;
        assert_ne!(web_id, 0, "the test's own premise: web must not be id 0");

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Trigger {
                        selector: SelectorSpec::Name("web".to_string()),
                        action: "gc".to_string(),
                        params: None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Triggered(rows) = reply.result.unwrap() else {
            panic!("expected triggered")
        };
        assert_eq!(
            rows,
            vec![ActionReply {
                id: web_id,
                name: "web".to_string(),
                outcome: ActionOutcome::TimedOut,
            }]
        );
    }

    /// Fails if the daemon's own per-app wait is ever allowed to reach or
    /// exceed the caller's default RPC budget. `shep_core::config::normalize`
    /// only refuses an `action_timeout` no caller could ever satisfy however
    /// long a deadline it asks for (its own ceiling, 58s); anything between
    /// that and [`DEFAULT_DEADLINE_MS`] normalizes fine, on the documented
    /// understanding that a caller sending no deadline of its own has to lose
    /// this exact race. This case drives that failure mode for real rather
    /// than asserting a constant comparison: `web`'s `action_timeout` is set
    /// past the 5s default budget, the request carries no deadline of its own
    /// (`envelope`'s `deadline_ms: None`), and under the paused clock
    /// `dispatch`'s own `with_deadline` wins — the reply is
    /// `DeadlineExceeded`, never a `Triggered` row, honest or otherwise.
    ///
    /// Confirmed by mutating the sibling case above rather than by
    /// construction: raising `AppConfig::minimal`'s `action_timeout` there to
    /// 9s (past `DEFAULT_DEADLINE_MS`, same value this case uses) reproduced
    /// exactly this — `RpcError { code: DeadlineExceeded, message: "the
    /// request deadline of 5000 ms expired before the daemon finished" }`
    /// where that test expected a `TimedOut` row. This case pins that same
    /// shape as its own assertion, so a regression that reintroduces it fails
    /// the suite outright instead of needing a mutation run to surface again.
    #[tokio::test(start_paused = true)]
    async fn an_oversized_action_timeout_loses_the_race() {
        let h = harness(vec![ProcScript::never_exits()]);
        let mut web = AppConfig::minimal("web", "./srv");
        web.channel = true;
        web.action_timeout = UpDuration::from_millis(9_000); // > DEFAULT_DEADLINE_MS (5s)
        reply_of(dispatch(envelope(1, Request::Start { apps: vec![web] }), &h.ctx).await);

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Trigger {
                        selector: SelectorSpec::Name("web".to_string()),
                        action: "gc".to_string(),
                        params: None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(
            reply.result.unwrap_err().code,
            RpcErrorCode::DeadlineExceeded,
            "an action_timeout past the caller's default budget must lose that race, not \
             report an honest TimedOut row nobody can reach"
        );
    }

    /// Fails if `Trigger` skips the selector conversion, or converts it
    /// without reporting the failure: a peer regex the daemon cannot compile
    /// is the client's usage error, not an internal one.
    #[tokio::test(start_paused = true)]
    async fn a_bad_trigger_selector_is_invalid_config() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Trigger {
                        selector: SelectorSpec::Regex("((".to_string()),
                        action: "gc".to_string(),
                        params: None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    /// A selector matching no registered sheep is a whole-request `NotFound`
    /// — same as every other selector-in verb, `a_selector_matching_nothing_is_not_found`
    /// pins it for `Stop` — kept separate from a per-row `NoChannel`, which
    /// only ever appears inside a non-empty match and never on its own.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_matching_nothing_is_not_found() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Trigger {
                        selector: SelectorSpec::Name("ghost".to_string()),
                        action: "gc".to_string(),
                        params: None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::NotFound);
    }

    /// The whole of an operator's feedback loop for a rotation that failed:
    /// the daemon's code becomes the CLI's exit status (`Internal` is 9), and
    /// the daemon's message is the only thing printed about which path went
    /// wrong.
    ///
    /// Fails if the `ReopenFailed | FlushFailed` arm answers any other code —
    /// `SpawnFailed` exits 7 and reads as "could not start it", which is a
    /// different call to whoever is paged. Fails too if that arm sends the
    /// bare payload instead of `err.to_string()`: once the two share one wire
    /// code, `SupervisorError`'s `Display` is the only thing left telling a
    /// reader which half of the log plane failed, and both payloads are just
    /// paths and reasons.
    #[test]
    fn a_log_plane_failure_is_internal_and_says_which_half_failed() {
        let reopen = rpc_error(&SupervisorError::ReopenFailed(
            "web (id 0): could not reopen /logs/web-out.log: Permission denied".to_string(),
        ));
        assert_eq!(reopen.code, RpcErrorCode::Internal);
        assert_eq!(
            reopen.message,
            "log reopen failed: web (id 0): could not reopen \
             /logs/web-out.log: Permission denied"
        );

        let flush = rpc_error(&SupervisorError::FlushFailed(
            "/logs/web-out.log: Permission denied".to_string(),
        ));
        assert_eq!(flush.code, RpcErrorCode::Internal);
        assert_eq!(
            flush.message,
            "log flush failed: /logs/web-out.log: Permission denied"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn subscribe_hands_back_a_compiled_filter() {
        let h = harness(vec![]);
        let outcome = dispatch(
            envelope(
                1,
                Request::Subscribe {
                    topics: vec!["process.*".to_string()],
                },
            ),
            &h.ctx,
        )
        .await;
        let Outcome::Subscribe { reply, filter } = outcome else {
            panic!("expected subscribe")
        };
        assert_eq!(reply.result.unwrap(), Response::Subscribed);
        assert_eq!(filter.patterns(), ["process.*"]);

        let bad = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Subscribe {
                        topics: vec!["[".to_string()],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(bad.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    #[tokio::test(start_paused = true)]
    async fn kill_daemon_asks_for_shutdown_without_taking_the_engine_down_itself() {
        let mut h = harness(vec![]);
        let Outcome::Shutdown(reply) = dispatch(envelope(1, Request::KillDaemon), &h.ctx).await
        else {
            panic!("expected a shutdown outcome")
        };
        assert_eq!(reply.result.unwrap(), Response::ShuttingDown);
        // Dispatch only reports the intent; the connection layer triggers it.
        assert!(!*h.shutdown_rx.borrow_and_update());
        h.ctx.shutdown();
        assert!(h.shutdown_rx.changed().await.is_ok());
        assert!(*h.shutdown_rx.borrow());
    }

    #[test]
    fn budgets_default_and_clamp() {
        assert_eq!(budget(None), Duration::from_millis(DEFAULT_DEADLINE_MS));
        assert_eq!(budget(Some(250)), Duration::from_millis(250));
        assert_eq!(budget(Some(0)), Duration::from_millis(1));
        assert_eq!(
            budget(Some(u64::MAX)),
            Duration::from_millis(MAX_DEADLINE_MS)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn envelope_deadline_ms_actually_bounds_the_reply() {
        // `budgets_default_and_clamp` proves `budget()` in isolation, and
        // `work_past_its_deadline_answers_deadline_exceeded` proves
        // `with_deadline` in isolation — neither one exercises the wire from
        // `Envelope::deadline_ms` into `budget()` inside `dispatch` itself
        // (rpc.rs line ~139: `budget(envelope.deadline_ms)`). A reviewer
        // changing that call to `budget(None)` left the full suite green,
        // which is exactly the gap this test closes: it drives a REAL
        // envelope with a client-supplied deadline through `dispatch`
        // against a supervisor command that provably takes longer than that
        // deadline, and checks the reply is `DeadlineExceeded`.
        //
        // "Provably longer": `Stop` on an `ignores_signals()` sheep goes
        // through the real kill ladder (`kill.rs::kill_process`), which
        // SIGTERMs (ignored — that's the whole point of this script), then
        // waits up to `app.kill_timeout` (1600ms, `AppConfig::minimal`'s
        // spec default) before escalating to SIGKILL — and the supervisor's
        // reply to `stop()` doesn't land until that whole ladder resolves
        // (`begin_manual`/`PendingReply`, supervisor.rs). `never_exits()`
        // does NOT work for this: despite the name, it still `obeys_signal`
        // and resolves its `wait()` the instant `signal()` is called, with
        // no virtual time elapsed at all — `ignores_signals()` is the one
        // script that actually forces the full `kill_timeout` wait. A 1ms
        // client deadline is far below that 1600ms floor, so under the
        // paused clock `dispatch`'s own `tokio::time::timeout` is
        // guaranteed to fire first — provided `envelope.deadline_ms`
        // genuinely reached `budget`. If it didn't (e.g. `budget(None)`
        // uses the 5s default), 1600ms < 5000ms and this would NOT time
        // out — see the revert experiment in merge-blockers-report.md,
        // which changes exactly that line and confirms this test fails.
        let h = harness(vec![ProcScript::ignores_signals()]);
        dispatch(
            envelope(
                1,
                Request::Start {
                    apps: vec![AppConfig::minimal("web", "./srv")],
                },
            ),
            &h.ctx,
        )
        .await;

        let reply = reply_of(
            dispatch(
                Envelope {
                    id: 2,
                    deadline_ms: Some(1),
                    body: Request::Stop {
                        selector: SelectorSpec::Name("web".to_string()),
                    },
                },
                &h.ctx,
            )
            .await,
        );
        let err = reply.result.unwrap_err();
        assert_eq!(
            err.code,
            RpcErrorCode::DeadlineExceeded,
            "a 1ms client deadline against a 1600ms kill ladder must expire, not {err:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn work_past_its_deadline_answers_deadline_exceeded() {
        // Driven at the deadline seam with a future that never finishes: the
        // paused clock auto-advances the moment the test parks, so this is
        // instant and exact.
        let outcome = with_deadline(
            5,
            Duration::from_millis(250),
            std::future::pending::<Outcome>(),
        )
        .await;
        let reply = reply_of(outcome);
        assert_eq!(reply.id, 5);
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::DeadlineExceeded);
        assert!(err.message.contains("250 ms"), "{}", err.message);
    }

    /// fails if `SaveRoll` stops writing, or writes without reporting: the
    /// assertion reads the file the reply named and compares its app count
    /// against the number the reply claimed, so a handler that answered
    /// `apps: 0` for a two-app flock — or named a path it never wrote —
    /// reddens here rather than in an operator's terminal after a reboot.
    #[tokio::test]
    async fn save_roll_writes_the_file_it_names_and_counts_what_it_recorded() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![
                            AppConfig::minimal("web", "./srv"),
                            AppConfig::minimal("worker", "./work"),
                        ],
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        let reply = reply_of(dispatch(envelope(2, Request::SaveRoll), &h.ctx).await);
        let Ok(Response::RollSaved { path, apps }) = reply.result else {
            panic!("expected RollSaved, got {:?}", reply.result)
        };
        assert_eq!(apps, 2);

        let roll = crate::snapshot::read(std::path::Path::new(&path)).unwrap();
        assert_eq!(roll.apps.len(), 2, "the reply's count must match the file");
        assert_eq!(path, h.ctx.snapshot_path.display().to_string());
    }

    /// fails if the handler forwards `snapshot_now`'s engine-stopped
    /// `Ok(())` as a success. That is the whole reason this verb exists:
    /// a save that wrote nothing and said "saved" is the failure mode an
    /// operator reboots into.
    #[tokio::test]
    async fn save_roll_against_a_stopped_engine_is_an_error_not_a_silent_success() {
        let h = harness(vec![]);
        h.ctx.supervisor.shutdown().await;

        let reply = reply_of(dispatch(envelope(1, Request::SaveRoll), &h.ctx).await);
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::Internal);
        assert!(
            err.message.contains("engine"),
            "the operator must be told why nothing was written: {}",
            err.message
        );
    }
}
