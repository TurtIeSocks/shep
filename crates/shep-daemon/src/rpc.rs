//! Portable RPC dispatch: verb routing, typed errors, per-call deadlines
//!
//! `dispatch` is the one function the connection layer calls per request
//! envelope. Everything here compiles and tests on every platform: no
//! `cfg(unix)`, no sockets, no bytes on a wire. [`RpcContext`] bundles the
//! daemon-wide handles a request handler may touch; `Outcome` tells the
//! caller what to do next (reply, forward bus events, or begin shutdown).
//!
//! Every envelope gets a `budget`: its own `deadline_ms`, clamped to
//! `MAX_DEADLINE_MS` so a peer cannot pin a daemon task open, or
//! `DEFAULT_DEADLINE_MS` when it sent none.

use core::future::Future;
use core::time::Duration;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use shep_core::config::{DeclaredApp, NormalizeError, ResolvedApp, normalize_all};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{
    Envelope, Lamb, ProcessInfo, Reply, Request, Response, RpcError, RpcErrorCode, SelectorSpec,
    SheepApplied,
};
use shep_core::selector::ProcessSelector;
use shep_core::signals::OperatorSignal;

use crate::bus::{Bus, TopicFilter};
use crate::dogs::DogSpec;
use crate::limits::stats::StatsState;
use crate::snapshot::{FlockRegistry, SnapshotError, write_atomic};
use crate::supervisor::{Applied, ConnId, SupervisorError, SupervisorHandle};

/// Deadline applied when a client sends none (spec §6: 5s default).
pub(crate) const DEFAULT_DEADLINE_MS: u64 = 5_000;
/// Ceiling on a client-supplied deadline: a peer cannot pin a daemon task open.
pub(crate) const MAX_DEADLINE_MS: u64 = 60_000;

/// Everything a request handler may touch; one clone per connection.
///
/// Every clone shares the same supervisor engine, event bus sender, flock
/// registry, and shutdown signal. The connection layer builds one from the
/// daemon's shared state and hands it to `dispatch` once per envelope.
///
/// Public because `tests/daemon_e2e.rs` holds one to drive [`Self::shutdown`]
/// and [`Self::snapshot_now`] without going through the socket. Its fields
/// are not.
#[derive(Clone, Debug)]
pub struct RpcContext {
    /// The supervisor engine this daemon is running.
    pub(crate) supervisor: SupervisorHandle,
    /// The daemon-wide event bus; `Subscribe` compiles a [`TopicFilter`] the
    /// connection layer hands to [`crate::bus::spawn_forwarder`] alongside a
    /// receiver off this sender.
    pub(crate) events: Bus,
    /// The muster roll's in-memory app registry; `Start` records into it.
    pub(crate) registry: FlockRegistry,
    /// Where [`Self::snapshot_now`] writes the muster roll.
    pub(crate) snapshot_path: PathBuf,
    /// Where `DogConfig` reads a dog's `[<name>]` section from: this home's
    /// `dogs.toml`, not `shep.toml`. The key carries no prefix.
    ///
    /// Re-read per request rather than held as parsed config, so `shep
    /// disable X && shep enable X` picks up an edited section
    /// (`crate::dogs::dog_section`).
    pub(crate) dogs_config: PathBuf,
    /// This daemon's `$SHEP_HOME` layout, for assembling a dog's app config.
    pub(crate) paths: ShepPaths,
    /// This daemon's crate version, echoed in the handshake.
    pub(crate) daemon_version: String,
    /// Which dogs this daemon has refused at the handshake, and how often.
    ///
    /// Written and read by the connection layer's handshake, to decide
    /// whether a refused dog earns its one restart from disk or has already
    /// had it ([`crate::dogs::DogRefusals`]).
    pub(crate) dog_refusals: crate::dogs::DogRefusals,
    /// What has connected to this daemon's socket, by peer pid.
    ///
    /// Written by the connection layer, the one place that can see a peer's
    /// credentials, and read by [`crate::dogs::record_silent_dog`]. It tells
    /// a dog that never reached the socket apart from one that reached it and
    /// did not name itself: two silences with opposite fixes.
    pub(crate) peer_contacts: crate::dogs::PeerContacts,
    /// This daemon's OS pid, echoed in the handshake.
    pub(crate) pid: u32,
    /// Flips to `true` to start graceful daemon shutdown; see [`Self::shutdown`].
    pub(crate) shutdown: Arc<watch::Sender<bool>>,
    /// The live resource readings [`with_live_stats`] takes a sample from.
    ///
    /// The same state the supervisor's extras hold: they decide which sheep
    /// is watched and record the periodic CPU baseline, and this side reads
    /// against it.
    pub(crate) stats: Arc<StatsState>,
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
    /// Only flips the watch signal; the connection layer runs the kill
    /// ladder and closes listeners once it observes this go `true`.
    /// `dispatch` never calls it: `KillDaemon` reports the intent through
    /// `Outcome::Shutdown`, so the caller triggers it after the reply is on
    /// the wire.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Announces that these dogs' `dogs.toml` sections changed.
    ///
    /// The one place a `config.dog.<name>` frame comes from: the publisher
    /// has to be inside the daemon process, because that is where the bus
    /// is. The CLI's other two writers of `dogs.toml` say nothing.
    ///
    /// Public because the caller is `shep`'s own boot, which runs the
    /// migration before this daemon exists.
    pub fn announce_dog_config(&self, dogs: &[String]) {
        crate::bus::publish_dog_config_changed(&self.events, dogs);
    }

    /// Writes the muster roll now, reporting what it recorded.
    ///
    /// `None` means the supervisor engine has already stopped: there is
    /// nothing left to record and the shutdown path has already written the
    /// final roll.
    ///
    /// # Errors
    /// - [`SnapshotError`] as `write_atomic` reports it.
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
    /// - [`SnapshotError`] as `write_atomic` reports it.
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
/// The deadline [`budget`] computes bounds the reply, not the actor's work:
/// dropping the work future only stops the daemon waiting on the supervisor,
/// and a command already handed to a sheep-owning task runs to completion.
/// So a `DeadlineExceeded` reply to `Start` means no answer within the
/// budget, not that nothing happened; a client that retries must reconcile
/// with `ListFlock`.
pub(crate) async fn dispatch(envelope: Envelope, conn: ConnId, ctx: &RpcContext) -> Outcome {
    let id = envelope.id;
    with_deadline(
        id,
        budget(envelope.deadline_ms),
        run(id, conn, envelope.body, ctx),
    )
    .await
}

// `+ Send`: awaited inside the per-connection `tokio::spawn`, so the bound is
// stated rather than inferred.
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
                daemon_version: None,
            }),
        }),
    }
}

async fn run(id: u64, conn: ConnId, request: Request, ctx: &RpcContext) -> Outcome {
    let reply = |result| Outcome::Reply(Reply { id, result });
    match request {
        Request::Ping => reply(Ok(Response::Pong)),
        // One of the two verbs that pays for a live reading; `with_live_stats`
        // says why every lifecycle verb below goes without.
        Request::ListFlock => match ctx.supervisor.list_checked().await {
            Ok(infos) => reply(Ok(Response::Flock(with_dog_contact(
                &ctx.dog_refusals,
                with_live_stats(&ctx.stats, infos).await,
            )))),
            Err(err) => reply(Err(rpc_error(&err))),
        },
        // The other one. Sampled after the selector has narrowed the
        // listing, so the join below runs over the matched rows alone.
        Request::Describe { selector } => match selector_of(selector) {
            Err(err) => reply(Err(err)),
            Ok(selector) => match ctx.supervisor.list_checked().await {
                Err(err) => reply(Err(rpc_error(&err))),
                Ok(infos) => {
                    // The rule `Actor::matching_ids` applies to every
                    // lifecycle verb, repeated here because this filter is
                    // over `ProcessInfo`s: a dog is not a flock member, so a
                    // sweep passes it by and an exact selector reaches it.
                    let exact = selector.is_exact();
                    let hits: Vec<_> = infos
                        .into_iter()
                        .filter(|i| exact || i.dog.is_none())
                        .filter(|i| selector.matches(&i.name, i.id, i.fold.as_deref(), i.instance))
                        .collect();
                    if hits.is_empty() {
                        reply(Err(not_found()))
                    } else {
                        let hits = with_live_stats(&ctx.stats, hits).await;
                        let hits = with_lambs(&ctx.stats, hits).await;
                        reply(Ok(Response::Described(with_dog_contact(
                            &ctx.dog_refusals,
                            hits,
                        ))))
                    }
                }
            },
        },
        // Peer input is untrusted: re-normalize before anything is registered.
        Request::Start { apps } => match normalize_all(apps) {
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
                daemon_version: None,
            })),
            Ok(resolved) => {
                ctx.registry.record(&resolved);
                match ctx.supervisor.start(resolved).await {
                    Ok(infos) => reply(Ok(Response::Started(infos))),
                    Err(err) => reply(Err(rpc_error(&err))),
                }
            }
        },
        // The membership half of `Start` with none of the spawning, and the
        // same untrusted-peer rule. Recorded in the registry as `Start` is:
        // an added app is a flock member that happens to be stopped, so a
        // `shep save` after a `shep add` has to write it.
        Request::Add { apps } => match normalize_all(apps) {
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
                daemon_version: None,
            })),
            Ok(resolved) => {
                ctx.registry.record(&resolved);
                match ctx.supervisor.register_at_rest(resolved).await {
                    Ok(infos) => reply(Ok(Response::Added(infos))),
                    Err(err) => reply(Err(rpc_error(&err))),
                }
            }
        },
        // Re-normalized for the reason `Start` is, plus one of its own: an
        // unnormalized config would report every default it did not spell out
        // as a difference. Nothing is recorded, since this answers a question
        // and must not change what the next `shep save` writes.
        Request::ConfigDrift { apps } => match normalize_all(apps) {
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
                daemon_version: None,
            })),
            Ok(resolved) => match ctx.supervisor.config_drift(resolved).await {
                Ok(drifted) => reply(Ok(Response::Drifted(drifted))),
                Err(err) => reply(Err(rpc_error(&err))),
            },
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
        // `Reloading` names an acceptance: the supervisor answers as soon as
        // the reload is taken, before the first replacement is spawned. A
        // reply that waited for the swaps would routinely outlive
        // `MAX_DEADLINE_MS` and be abandoned by `with_deadline`.
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
        Request::Signal { selector, signal } => signal_request(id, selector, signal, ctx).await,
        Request::SendLine { selector, line } => {
            // Refused here, not silently split by the writer: a line
            // carrying a newline would be delivered as two commands where the
            // operator typed one. `\r` too, since CRLF reaches a shell as a
            // command with a stray carriage return in it.
            if line.contains(['\n', '\r']) {
                return reply(Err(RpcError {
                    code: RpcErrorCode::InvalidConfig,
                    message: "a line may not contain a newline or a carriage return; \
                              send one line per request"
                        .to_string(),
                    daemon_version: None,
                }));
            }
            match selector_of(selector) {
                Ok(selector) => match ctx.supervisor.send_line(selector, line).await {
                    Ok(rows) => reply(Ok(Response::SentLine(rows))),
                    Err(err) => reply(Err(rpc_error(&err))),
                },
                Err(err) => reply(Err(err)),
            }
        }
        Request::Delete { selector } => match selector_of(selector) {
            Err(err) => reply(Err(err)),
            Ok(selector) => match ctx.supervisor.delete(selector).await {
                Ok(ids) => reply(Ok(Response::Deleted(ids))),
                Err(err) => reply(Err(rpc_error(&err))),
            },
        },
        Request::Scale { name, count } => match ctx.supervisor.scale(&name, count).await {
            Ok(scaled) => {
                // Recorded unconditionally: without it `shep stock web 4`
                // then `shep save` writes `instances = 2` and the next reboot
                // undoes the scale. Unconditionally, since a partial scale-up
                // leaves real instances the roll has to know about too.
                let achieved = scaled.achieved();
                let requested = scaled.requested;
                ctx.registry.record(&[scaled.app]);
                match scaled.shortfall {
                    None => reply(Ok(Response::Scaled(scaled.instances))),
                    // Non-zero exit: the operator asked for four and has
                    // three. The sentence names both numbers, so a reader can
                    // tell a scale that achieved nothing from one that nearly
                    // finished.
                    Some(message) => reply(Err(RpcError {
                        code: RpcErrorCode::SpawnFailed,
                        message: format!(
                            "scaled {name} to {achieved} of {requested} requested; \
                             the next instance would not spawn: {message}"
                        ),
                        daemon_version: None,
                    })),
                }
            }
            Err(err) => reply(Err(rpc_error(&err))),
        },
        // Scoped to `conn`, which is what makes a smit ephemeral: the
        // connection layer forgets this one's marks in its own tail. `smit`
        // arrives already validated by `Smit`'s hand-written `Deserialize`,
        // so the only refusal left here is a name nothing holds.
        Request::SetSmit { sheep, smit } => {
            match ctx.supervisor.set_smit(conn, &sheep, smit).await {
                Ok(infos) => reply(Ok(Response::SmitPainted(infos))),
                Err(err) => reply(Err(rpc_error(&err))),
            }
        }
        Request::SaveRoll => match ctx.save_roll_now().await {
            Ok(Some(saved)) => reply(Ok(Response::RollSaved {
                // Lossy, as `to_info` treats log paths: a non-UTF-8 roll
                // path degrades one field rather than the whole reply.
                path: saved.path.to_string_lossy().into_owned(),
                apps: saved.apps,
            })),
            Ok(None) => reply(Err(RpcError {
                code: RpcErrorCode::Internal,
                message: "the supervisor engine has stopped; no roll was written".to_string(),
                daemon_version: None,
            })),
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::Internal,
                message: err.to_string(),
                daemon_version: None,
            })),
        },
        // The same restore `boot` runs, called the same way
        // (`crate::snapshot::muster`).
        Request::Muster => {
            match crate::snapshot::muster(&ctx.snapshot_path, &ctx.registry, &ctx.supervisor).await
            {
                Err(err) => reply(Err(RpcError {
                    code: RpcErrorCode::Internal,
                    message: err.to_string(),
                    daemon_version: None,
                })),
                Ok(names) => match ctx.supervisor.list_checked().await {
                    Err(err) => reply(Err(rpc_error(&err))),
                    // Every sheep of every app the roll restored, not only
                    // the ones this call spawned (`Response::Mustered`).
                    Ok(infos) => reply(Ok(Response::Mustered(
                        infos
                            .into_iter()
                            .filter(|info| names.contains(&info.name))
                            .collect(),
                    ))),
                },
            }
        }
        // Re-read per request, never cached: `shep disable X && shep enable
        // X` bounces a dog to reload its configuration, and a copy taken at
        // boot would answer with the section as it was. A dog subscribed to
        // `config.dog.<name>` reaches this same arm without going down.
        Request::DogConfig { name } => match crate::dogs::dog_section(&ctx.dogs_config, &name) {
            Ok(toml) => reply(Ok(Response::DogSection { toml: toml.into() })),
            Err(err) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: err.to_string(),
                daemon_version: None,
            })),
        },
        Request::EnableDog { name, source } => {
            let spec = DogSpec { name, source };
            match crate::dogs::dog_app(&spec, &ctx.paths) {
                Err(err) => reply(Err(RpcError {
                    code: RpcErrorCode::InvalidConfig,
                    message: err.to_string(),
                    daemon_version: None,
                })),
                Ok(app) => {
                    // Read before `start_dog` takes the app. An operator
                    // reading the dog's log during an upgrade is usually
                    // asking which file the spawn resolved to.
                    let script = app.config().script.clone();
                    match ctx.supervisor.start_dog(app, spec.source).await {
                        // `start_dog` is idempotent by name, so what comes
                        // back is whatever already holds it. An unmarked entry
                        // means a sheep holds it: nothing was spawned, so the
                        // refusal has nothing to undo.
                        Ok(info) if info.dog.is_none() => reply(Err(RpcError {
                            code: RpcErrorCode::InvalidConfig,
                            message: format!(
                                "a sheep is already registered as `{}`; rename it or give the dog another name",
                                spec.name
                            ),
                            daemon_version: None,
                        })),
                        Ok(info) => {
                            // Wording is about the binary this shepherd
                            // resolved, not about a spawn having happened:
                            // `start_dog` is idempotent by name, so this may
                            // be a dog that was already running.
                            crate::dogs::narrate(
                                &ctx.events,
                                &info,
                                &format!(
                                    "shep has this dog enabled, running the binary at {script}"
                                ),
                            )
                            .await;
                            reply(Ok(Response::DogStarted(info)))
                        }
                        Err(err) => reply(Err(rpc_error(&err))),
                    }
                }
            }
        }
        // Through `delete` with an exact `Name` selector: disabling a dog
        // reuses the stop-then-deregister path every sheep takes rather than
        // opening a second way to end a supervised process.
        Request::DisableDog { name } => {
            match ctx.supervisor.delete(ProcessSelector::Name(name)).await {
                Ok(ids) => reply(Ok(Response::Deleted(ids))),
                Err(err) => reply(Err(rpc_error(&err))),
            }
        }
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
                daemon_version: None,
            })),
        },
        Request::DogStaleness => {
            let (stale, pending) = dog_staleness(ctx).await;
            reply(Ok(Response::DogStaleness { stale, pending }))
        }
        Request::HandoverFitness => reply(Ok(Response::HandoverFitness {
            refusal: handover_refusal(ctx).await,
        })),
        Request::KillDaemon => Outcome::Shutdown(Reply {
            id,
            result: Ok(Response::ShuttingDown),
        }),
        // The acting half of `ConfigDrift` above, and the one arm here that
        // changes a running flock's config without replacing anything.
        Request::ApplyConfig { apps, reset } => match duplicate_name(&apps) {
            Some(name) => reply(Err(RpcError {
                code: RpcErrorCode::InvalidConfig,
                message: NormalizeError::DuplicateName(name).to_string(),
                daemon_version: None,
            })),
            None => match ctx.supervisor.apply_config(apps, reset).await {
                Ok(applied) => {
                    // Recorded unconditionally, as the `Scale` arm above is:
                    // an apply that reached the stored spec must reach the
                    // roll too. An app whose merge produced no honest config
                    // carries `None` and is skipped rather than invented.
                    let recorded: Vec<ResolvedApp> =
                        applied.iter().filter_map(|a| a.app.clone()).collect();
                    ctx.registry.record(&recorded);
                    reply(Ok(Response::Applied(
                        applied.into_iter().map(SheepApplied::from).collect(),
                    )))
                }
                Err(err) => reply(Err(rpc_error(&err))),
            },
        },
        // `Request` is #[non_exhaustive]: a verb from a newer client that this
        // daemon has never heard of is an error, not a panic.
        _ => reply(Err(RpcError {
            code: RpcErrorCode::Internal,
            message: "this daemon does not implement that request".to_string(),
            daemon_version: None,
        })),
    }
}

/// The first name two entries of an `ApplyConfig` share, if any.
///
/// `handle_apply_config` reads the override store once for the whole request
/// and writes it once at the end, so a second entry of the same name merges
/// against the store as the first entry found it: the first entry's record is
/// overwritten and nothing says so.
///
/// Refused whole rather than per app, since a document naming one app twice
/// is malformed rather than partly wrong.
///
/// Linear in a `BTreeSet`, matching `normalize_all`: a request carries the
/// apps one Flockfile declared.
fn duplicate_name(apps: &[DeclaredApp]) -> Option<String> {
    let mut seen = BTreeSet::new();
    apps.iter()
        .find(|app| !seen.insert(app.config.name.as_str()))
        .map(|app| app.config.name.clone())
}

/// The wire form of one app's load, with the merged config dropped.
///
/// `Applied` carries the whole merged [`ResolvedApp`] because `rpc.rs` hands
/// it to the registry; [`SheepApplied`] does not. A client has no use for the
/// config, and `env` is in it, so the conversion is where the config stops.
impl From<Applied> for SheepApplied {
    fn from(applied: Applied) -> Self {
        Self::new(
            applied.name,
            applied.applied,
            applied.pending,
            applied.refused,
        )
    }
}

/// The dogs this daemon has given up on, and the dogs it is still waiting to
/// hear from.
///
/// Stale is [`DogRefusals::stale`](crate::dogs::DogRefusals::stale): refused,
/// restarted from the binary on disk, refused again. A version cannot answer
/// it, since two dog builds differing only in protocol report the same one.
///
/// Pending has two sources: a dog refused once is mid-restart, and a
/// supervised dog that has never handshaken has not been asked yet, which is
/// what a carried dog is between the exec and its reconnect. Only a dog with
/// a process counts. `shep daemon reload` polls this every 50ms, so the
/// silent-dog ladder must stay on a clock rather than be driven from here.
async fn dog_staleness(ctx: &RpcContext) -> (Vec<String>, Vec<String>) {
    let stale = ctx.dog_refusals.stale();
    let mut pending = ctx.dog_refusals.restarting();
    // A stopped engine has no dogs left to wait on, so its rows are not
    // worth an error: the refusal record above is still the honest answer.
    if let Ok(infos) = ctx.supervisor.list_checked().await {
        pending.extend(crate::dogs::silent_dogs(&infos, &ctx.dog_refusals));
    }
    pending.sort();
    pending.dedup();
    (stale, pending)
}

/// Why this shepherd cannot hand its flock to a successor in place, or
/// `None` when it can.
///
/// The sentence is rendered here rather than as a structured reason on the
/// wire, for the reason [`Response::HandoverFitness`] gives: the client does
/// nothing with it but print it.
///
/// An engine that has stopped is a refusal too, not an error: the caller
/// asked whether to signal a shepherd.
#[cfg(unix)]
async fn handover_refusal(ctx: &RpcContext) -> Option<String> {
    match ctx.supervisor.handover_fitness().await {
        Ok(crate::handover::Fitness::Carryable) => None,
        Ok(crate::handover::Fitness::Refused(reason)) => Some(reason.to_string()),
        Err(err) => Some(format!(
            "this shepherd could not check whether its flock can be handed over ({err})"
        )),
    }
}

/// Windows has no `execve`, so there is no image for a successor to become
/// and every flock is refused.
///
/// A refusal rather than an unimplemented request: this one is answered, and
/// the answer sends `shep daemon reload` to the stop-and-start arm.
#[cfg(windows)]
#[expect(
    clippy::unused_async,
    reason = "one signature for both platforms; the unix arm awaits the supervisor"
)]
async fn handover_refusal(_ctx: &RpcContext) -> Option<String> {
    Some(
        "this shepherd runs on Windows, which has no `execve`, so its flock cannot be handed to \
         a successor in place"
            .to_string(),
    )
}

/// Fills in each running sheep's live CPU and memory.
///
/// Sampled here rather than inside the supervisor: the actor must never
/// block, and the reading is a syscall walk over the host's whole process
/// table, so it runs on a blocking-pool thread.
///
/// Joined by pid, not by id: [`StatsState`] keys on the root pid it was armed
/// against, which is the number [`ProcessInfo::pid`] carries. Only `ListFlock`
/// and `Describe` call this; the lifecycle verbs answer with [`ProcessInfo`]
/// too, but none of them is where an operator reads resource usage.
async fn with_live_stats(stats: &Arc<StatsState>, mut infos: Vec<ProcessInfo>) -> Vec<ProcessInfo> {
    let stats = Arc::clone(stats);
    let Ok(sample) = tokio::task::spawn_blocking(move || stats.sample_now()).await else {
        // The blocking pool is gone or the task panicked: report the flock
        // without stats rather than fail a listing over a decoration.
        return infos;
    };
    for info in &mut infos {
        if let Some(reading) = info.pid.and_then(|pid| sample.get(&pid)) {
            info.cpu_percent = reading.cpu_percent;
            info.memory_bytes = Some(reading.memory_bytes);
        }
    }
    infos
}

/// Fills in each dog's two connection facts, which no sheep has and the
/// supervisor does not hold: whether it has ever answered this shepherd, and
/// whether this shepherd has given up on it.
///
/// Connection state lives in [`DogRefusals`](crate::dogs::DogRefusals) on the
/// RPC context, so it is joined here as `with_live_stats` is: two map lookups
/// per row, and `stale()` called once for the whole listing. Both fields,
/// because a dog spawned a moment ago and one this shepherd has stopped
/// restarting are both `handshook: Some(false)` with a live process.
///
/// Applied to `ListFlock` and `Describe` alone. A sheep is skipped rather
/// than set to `Some(false)`, having no handshake with this shepherd at all.
fn with_dog_contact(
    refusals: &crate::dogs::DogRefusals,
    mut infos: Vec<ProcessInfo>,
) -> Vec<ProcessInfo> {
    let stale = refusals.stale();
    for info in &mut infos {
        if info.dog.is_some() {
            info.handshook = Some(refusals.has_handshook(&info.name));
            info.dog_stale = Some(stale.contains(&info.name));
        }
    }
    infos
}

/// Fills each row's `lambs` from a fresh walk of the process table.
///
/// Applied to `Describe` and to nothing else: the walk is a second pass over
/// every process on the machine, and a flock listing is the thing an operator
/// leaves running in a loop.
///
/// A row with no pid is left `None` rather than `Some(vec![])`, which is the
/// "not walked" case the field's own doc distinguishes from "walked and
/// empty".
async fn with_lambs(stats: &Arc<StatsState>, mut infos: Vec<ProcessInfo>) -> Vec<ProcessInfo> {
    if infos.iter().all(|info| info.pid.is_none()) {
        // Nothing to walk for: skip the table refresh entirely rather than
        // pay for it and assign `None` anyway.
        return infos;
    }
    let stats = Arc::clone(stats);
    let pids: Vec<u32> = infos.iter().filter_map(|info| info.pid).collect();
    let Ok(walked) = tokio::task::spawn_blocking(move || {
        // One index for the whole reply: `describe all` walks the machine's
        // process table once, not once per row.
        let index = stats.lamb_index();
        pids.into_iter()
            .map(|pid| (pid, stats.lambs_of(&index, pid)))
            .collect::<HashMap<u32, Vec<Lamb>>>()
    })
    .await
    else {
        // The blocking pool is gone or the task panicked: describe the sheep
        // without their trees rather than fail the request over a decoration.
        return infos;
    };
    for info in &mut infos {
        if let Some(lambs) = info.pid.and_then(|pid| walked.get(&pid)) {
            info.lambs = Some(lambs.clone());
        }
    }
    infos
}

fn rpc_error(err: &SupervisorError) -> RpcError {
    match err {
        SupervisorError::NotFound => not_found(),
        SupervisorError::SpawnFailed(msg) => RpcError {
            code: RpcErrorCode::SpawnFailed,
            message: msg.clone(),
            daemon_version: None,
        },
        // The same code as `SpawnFailed`: `RpcErrorCode` is versioned, and a
        // client predating a new code cannot decode the reply at all. The
        // bare payload rather than `err.to_string()`, since this message
        // already opens with "nothing was registered".
        SupervisorError::CannotStart(msg) => RpcError {
            code: RpcErrorCode::SpawnFailed,
            message: msg.clone(),
            daemon_version: None,
        },
        // `Internal`, an unexpected daemon-side failure, and no code of its
        // own since a client predating a new one could not decode the reply.
        // `err.to_string()` rather than the bare payload: `Display` is the
        // only thing distinguishing the two once they share a code.
        SupervisorError::ReopenFailed(_) | SupervisorError::FlushFailed(_) => RpcError {
            code: RpcErrorCode::Internal,
            message: err.to_string(),
            daemon_version: None,
        },
        // `Internal` under protest: an app already being reloaded is a
        // conflict the caller can act on, and the wire has no code for one.
        // `Display` names the app, which is the part that says what to do.
        SupervisorError::ReloadInFlight(_) => RpcError {
            code: RpcErrorCode::Internal,
            message: err.to_string(),
            daemon_version: None,
        },
        // Every `InvalidScale` is something the caller can ask differently: a
        // count of `0`, a dog, or an app whose earlier scale is still
        // shutting instances down. That last one is a conflict, like
        // `ReloadInFlight`, and the wire has no code for one.
        SupervisorError::InvalidScale(msg) => RpcError {
            code: RpcErrorCode::InvalidConfig,
            message: msg.clone(),
            daemon_version: None,
        },
        SupervisorError::EngineStopped => RpcError {
            code: RpcErrorCode::Internal,
            message: "the supervisor engine has stopped".to_string(),
            daemon_version: None,
        },
    }
}

fn not_found() -> RpcError {
    RpcError {
        code: RpcErrorCode::NotFound,
        message: "selector matched no registered sheep".to_string(),
        daemon_version: None,
    }
}

fn selector_of(spec: SelectorSpec) -> Result<ProcessSelector, RpcError> {
    ProcessSelector::try_from(spec).map_err(|err| RpcError {
        code: RpcErrorCode::InvalidConfig,
        message: err.to_string(),
        daemon_version: None,
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

/// `Trigger`'s own resolve-then-map path. [`selector_call`] cannot serve it:
/// that helper maps `Vec<ProcessInfo>`, and `Response::Triggered` carries
/// `Vec<ActionReply>`, a row `ProcessInfo` cannot hold a reply body on.
///
/// How long each app gets to answer is `AppConfig::action_timeout`, one value
/// per matched sheep, read where the wait is armed (`Actor::begin_action`).
/// `shep_core::config::normalize` refuses only a value no caller could ever
/// outlast; one past the default budget is accepted, and the caller's own
/// deadline decides whether that pays off.
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

/// `Signal`'s own resolve-then-map path, mirroring [`trigger`]. The signal
/// name is re-validated here even though the CLI validated it too: peer input
/// is untrusted, the rule `Request::Start` follows a few arms up.
async fn signal_request(id: u64, spec: SelectorSpec, signal: String, ctx: &RpcContext) -> Outcome {
    let result = match OperatorSignal::parse(&signal) {
        None => Err(RpcError {
            code: RpcErrorCode::InvalidConfig,
            message: format!(
                "`{signal}` is not a signal shep will send; accepted: {}",
                OperatorSignal::ACCEPTED.join(", ")
            ),
            daemon_version: None,
        }),
        Some(sig) => match selector_of(spec) {
            Err(err) => Err(err),
            Ok(selector) => ctx
                .supervisor
                .signal(selector, sig)
                .await
                .map(Response::Signalled)
                .map_err(|err| rpc_error(&err)),
        },
    };
    Outcome::Reply(Reply { id, result })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FIRST_SCRIPTED_PID, ProcScript};
    use crate::limits::MEMORY_POLL_INTERVAL;
    use crate::testing::{
        Harness, SCRIPTED_TREE_BYTES, harness, harness_identifying, harness_with_stats, identity,
    };
    use shep_core::config::{AppConfig, DeclaredApp, ResetDepth};
    use shep_core::protocol::{
        ActionOutcome, ActionReply, DogSource, Request, Response, RpcErrorCode, SelectorSpec,
    };
    use shep_core::status::ProcStatus;
    use shep_core::values::UpDuration;
    use std::collections::BTreeSet;
    use tokio::time::Instant;

    /// Dispatches on a connection of its own, shadowing [`super::dispatch`]
    /// so no case here has to name a [`ConnId`]. One fresh id per call:
    /// nothing here spans two requests on the same connection.
    async fn dispatch(envelope: Envelope, ctx: &RpcContext) -> Outcome {
        super::dispatch(envelope, ConnId::next(), ctx).await
    }

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

    /// The harness is scripted with no processes, which is the forcing
    /// mechanism: `ScriptedRunner::spawn` refuses with `script exhausted`
    /// once the list is empty, so a build that routed this at `do_start`
    /// lands `Errored` rather than `Stopped` with no pid.
    #[tokio::test(start_paused = true)]
    async fn add_registers_a_stopped_member_and_spawns_nothing() {
        let h = harness(vec![]);
        let added = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Add {
                        apps: vec![AppConfig::minimal("web", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Added(infos) = added.result.unwrap() else {
            panic!("expected added")
        };
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].status, ProcStatus::Stopped);
        assert_eq!(infos[0].pid, None, "nothing was spawned");

        // The roll can only be built if `Add` recorded the config, and an app
        // registered and never started is precisely the one a roll would
        // otherwise forget.
        let roll = h.ctx.registry.roll(&infos, 0);
        assert_eq!(roll.apps.len(), 1);
        assert_eq!(roll.apps[0].app.script, "./srv");
        assert_eq!(roll.apps[0].instances_running, 0);

        let listed = reply_of(dispatch(envelope(2, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else {
            panic!("expected flock")
        };
        assert_eq!(
            flock.len(),
            1,
            "it is a member of the flock, just a still one"
        );
    }

    /// fails if `Add` trusts what a peer sent it. Same rule as `Start`: the
    /// socket is the boundary, and an empty name is the shape `normalize`
    /// refuses.
    #[tokio::test(start_paused = true)]
    async fn add_re_normalizes_untrusted_peer_config() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Add {
                        apps: vec![AppConfig::minimal("", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::InvalidConfig);
    }

    /// The sheep is online, the case that matters: re-running `shep add
    /// Flockfile.toml` after editing the file must not stop a service. One
    /// script, so a second spawn would fail rather than pass quietly.
    #[tokio::test(start_paused = true)]
    async fn a_second_add_leaves_a_running_sheep_exactly_as_it_was() {
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
        let Response::Started(before) = started.result.unwrap() else {
            panic!("expected started")
        };

        let added = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Add {
                        apps: vec![AppConfig::minimal("web", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Added(after) = added.result.unwrap() else {
            panic!("expected added")
        };
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].id, before[0].id,
            "the same sheep, not a second one"
        );
        assert_eq!(after[0].status, ProcStatus::Online, "still running");
        assert_eq!(after[0].pid, before[0].pid, "the same process");

        let listed = reply_of(dispatch(envelope(3, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else {
            panic!("expected flock")
        };
        assert_eq!(flock.len(), 1, "one row, not two");
    }

    /// One app as a Flockfile would declare it: the config, plus the keys
    /// the document literally wrote. `declared` is what an apply keys on, so
    /// a fixture that left it empty would declare nothing and apply nothing.
    fn declared(name: &str, script: &str, keys: &[&str]) -> DeclaredApp {
        DeclaredApp {
            config: AppConfig::minimal(name, script),
            declared: keys.iter().map(|k| (*k).to_string()).collect(),
            declared_env: BTreeSet::new(),
        }
    }

    /// `handle_apply_config` reads the override store once for the whole
    /// file, so a second entry of the same name merges against the store as
    /// the first entry found it and the first entry's record is lost.
    /// `normalize_all` refuses a duplicate on the `Start` path and is not on
    /// this one.
    #[tokio::test(start_paused = true)]
    async fn apply_config_refuses_a_request_naming_one_app_twice() {
        let h = harness(vec![ProcScript::never_exits()]);
        let _started = reply_of(
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

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::ApplyConfig {
                        apps: vec![
                            declared("web", "./one", &["script"]),
                            declared("web", "./two", &["script"]),
                        ],
                        reset: ResetDepth::None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(err.message.contains("web"), "the name is named: {err:?}");

        // Refused BEFORE anything was touched, which is the half an error
        // code alone does not prove: the flock still runs what `Start`
        // registered, not either of the two scripts the request carried.
        let listed = reply_of(dispatch(envelope(3, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else {
            panic!("expected flock")
        };
        let roll = h.ctx.registry.roll(&flock, 0);
        assert_eq!(roll.apps[0].app.script, "./srv");
    }

    /// The `Scale` arm's reasoning, applied to this one: a change that
    /// reached the stored spec and not the roll is undone by the next
    /// reboot.
    #[tokio::test(start_paused = true)]
    async fn apply_config_records_what_it_applied_in_the_registry() {
        let h = harness(vec![ProcScript::never_exits()]);
        let _started = reply_of(
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

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::ApplyConfig {
                        apps: vec![DeclaredApp {
                            config: {
                                let mut app = AppConfig::minimal("web", "./srv");
                                app.max_restarts = 99;
                                app
                            },
                            declared: ["max_restarts"].iter().map(|k| (*k).to_string()).collect(),
                            declared_env: BTreeSet::new(),
                        }],
                        reset: ResetDepth::None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Applied(report) = reply.result.unwrap() else {
            panic!("expected applied")
        };
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].name, "web");
        assert_eq!(report[0].applied, vec!["max_restarts".to_string()]);
        assert_eq!(report[0].refused, None);

        let listed = reply_of(dispatch(envelope(3, Request::ListFlock), &h.ctx).await);
        let Response::Flock(flock) = listed.result.unwrap() else {
            panic!("expected flock")
        };
        let roll = h.ctx.registry.roll(&flock, 0);
        assert_eq!(roll.apps[0].app.max_restarts, 99);
    }

    /// One app that cannot be applied must not cost the rest of the file its
    /// load, so a miss is a per-app refusal inside an `Ok`, never an `Err`.
    #[tokio::test(start_paused = true)]
    async fn apply_config_refuses_an_unregistered_app_inside_the_reply() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::ApplyConfig {
                        apps: vec![declared("ghost", "./srv", &["script"])],
                        reset: ResetDepth::None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Response::Applied(report) = reply.result.unwrap() else {
            panic!("expected applied")
        };
        assert_eq!(report.len(), 1);
        let refused = report[0].refused.as_deref().unwrap_or_default();
        assert!(
            refused.contains("ghost"),
            "the refusal names the app: {refused}"
        );
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

    /// Fails if `Reopen` is left to `run`'s catch-all arm, which answers
    /// `Internal` for a request this daemon implements, or if it is routed
    /// to another verb's supervisor call: `Stop` would stop the sheep.
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

    /// Fails if the arm is routed to another verb's supervisor call while
    /// keeping `Response::Reloading`, which no assertion on the reply alone
    /// can see. What separates a reload is the flock it leaves behind: two
    /// entries in one instance slot, the drainee `Stopping` under its
    /// original id and a replacement `Starting` under a new one.
    ///
    /// The mid-swap state is not a race: nothing advances the clock, and
    /// `ListFlock` is queued to an actor that runs `handle_reload` to
    /// completion before it takes another message. Three scripts, of which a
    /// correct run uses two; the third is sized for the spawn a broken arm
    /// performs, so it lands as a live entry rather than as `Errored`.
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

    /// The daemon's code becomes the CLI's exit status and its message is
    /// all that is printed. Fails if either refusal answers a code that is
    /// not `Internal`, since neither has one of its own and
    /// `SupervisorError`'s `Display` is what tells them apart. Fails too if
    /// `ReloadInFlight`'s arm drops the app's name, which says which reload
    /// to wait for.
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
    /// without reporting the failure: a peer regex the daemon cannot compile
    /// is the client's usage error. The shared `selector_call` is what keeps
    /// it; a hand-rolled arm answering `Reloading` could still lose it.
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

    /// `TimedOut` rather than `NoChannel` is the assertion that matters: the
    /// action was really delivered and waited on, and `web`'s 3s
    /// `action_timeout` elapsed inside the request's own budget. Raise it
    /// past [`DEFAULT_DEADLINE_MS`] and the reply becomes `DeadlineExceeded`
    /// instead, which names no sheep; that ordering is pinned right below in
    /// `an_oversized_action_timeout_loses_the_race`.
    ///
    /// Nothing answers, because the harness keeps no handle on its runner.
    #[tokio::test(start_paused = true)]
    async fn trigger_routes_to_the_flock_and_reports_each_match_within_the_budget() {
        // Two apps, not one: ids start at 0, so a single-app harness would
        // give `web` id 0, indistinguishable from a row-mapping bug that
        // leaves the field's default. `other` first pushes `web` to id 1.
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

    /// A bad signal name must be refused at the dispatch boundary with
    /// `InvalidConfig`: an operator who typed `SIGHUPP` needs the accepted
    /// list, and only this arm has it.
    #[tokio::test]
    async fn a_signal_name_outside_the_grammar_is_refused_with_the_accepted_list() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Signal {
                        selector: SelectorSpec::All,
                        signal: "SIGHUPP".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(err.message.contains("SIGHUPP"), "{}", err.message);
        assert!(err.message.contains("SIGHUP"), "{}", err.message);
        assert!(err.message.contains("SIGUSR2"), "{}", err.message);
    }

    /// Refused at the dispatch boundary, so it never reaches `send_line`.
    /// There is no sheep in this fixture to answer it, so a `NotFound` here
    /// would mean the refusal was skipped rather than that it fired.
    #[tokio::test]
    async fn a_line_carrying_a_newline_is_refused_before_it_reaches_the_supervisor() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::SendLine {
                        selector: SelectorSpec::All,
                        line: "reload\nrm -rf /".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(err.message.contains("newline"), "{}", err.message);
    }

    /// `web`'s `action_timeout` is set past the 5s default budget and the
    /// request carries no deadline of its own, so under the paused clock
    /// `dispatch`'s own `with_deadline` wins and the reply is
    /// `DeadlineExceeded` rather than a `Triggered` row.
    /// `shep_core::config::normalize` refuses only a timeout no caller could
    /// ever satisfy, so anything under that has to lose this race.
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

    /// A selector matching no registered sheep is a whole-request
    /// `NotFound`, kept separate from a per-row `NoChannel`, which only
    /// appears inside a non-empty match.
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

    /// Fails if the `ReopenFailed | FlushFailed` arm answers any other code:
    /// `SpawnFailed` exits 7 and reads as "could not start it". Fails too if
    /// it sends the bare payload instead of `err.to_string()`, since once the
    /// two share a wire code `Display` is all that tells a reader which half
    /// of the log plane failed.
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
        // Drives a real envelope's `deadline_ms` through `dispatch` into
        // `budget`. `Stop` on an `ignores_signals()` sheep waits the full
        // 1600ms `kill_timeout` ladder, far past this 1ms deadline, while a
        // build passing `budget(None)` would take the 5s default and pass.
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

    /// The assertion reads the file the reply named and compares its app
    /// count against the number the reply claimed, so a handler answering
    /// `apps: 0` for a two-app flock reddens here.
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

    /// fails if the muster roll keeps the pre-scale count. This is the test for the
    /// bug that is invisible until a reboot: the roll is what `shep muster` reads,
    /// so a scale missing from it is a scale that silently reverts.
    #[tokio::test]
    async fn a_scale_is_recorded_in_the_roll_the_next_muster_reads() {
        let h = harness(vec![ProcScript::never_exits(); 4]);
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        reply_of(dispatch(envelope(1, Request::Start { apps: vec![app] }), &h.ctx).await);

        reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Scale {
                        name: "web".to_string(),
                        count: 4,
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        let reply = reply_of(dispatch(envelope(3, Request::SaveRoll), &h.ctx).await);
        let Ok(Response::RollSaved { path, .. }) = reply.result else {
            panic!("expected RollSaved, got {:?}", reply.result)
        };
        let roll = crate::snapshot::read(std::path::Path::new(&path)).unwrap();
        assert_eq!(roll.apps[0].app.instances, 4);
    }

    /// `web` at two instances, scaled to four, with one script left so the
    /// first new spawn succeeds and the second fails. Three instances are
    /// then running: a roll saying `2` stops one at the next muster, a roll
    /// saying `4` brings up a count that never ran. Only `3` is the truth,
    /// and it gets there only if the handler records off the `Err` path too.
    ///
    /// The reply is asserted as well as the roll: recording what the daemon
    /// did must not turn "three of four" into a success.
    #[tokio::test]
    async fn a_partial_scale_is_recorded_in_the_roll_and_still_reported_short() {
        let h = harness(vec![ProcScript::never_exits(); 3]);
        let mut app = AppConfig::minimal("web", "./srv");
        app.instances = 2;
        reply_of(dispatch(envelope(1, Request::Start { apps: vec![app] }), &h.ctx).await);

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Scale {
                        name: "web".to_string(),
                        count: 4,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let err = reply.result.unwrap_err();
        assert_eq!(err.code, RpcErrorCode::SpawnFailed);
        assert!(
            err.message.contains("3 of 4"),
            "the operator has to be told both numbers: {}",
            err.message
        );

        let saved = reply_of(dispatch(envelope(3, Request::SaveRoll), &h.ctx).await);
        let Ok(Response::RollSaved { path, .. }) = saved.result else {
            panic!("expected RollSaved, got {:?}", saved.result)
        };
        let roll = crate::snapshot::read(std::path::Path::new(&path)).unwrap();
        assert_eq!(
            roll.apps[0].app.instances, 3,
            "the roll must hold the three instances really running — not the \
             pre-scale two, and not the four that were asked for"
        );
    }

    /// Fails if the handler forwards `snapshot_now`'s engine-stopped `Ok(())`
    /// as a success. A save that wrote nothing and said "saved" is the
    /// failure mode an operator reboots into.
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

    /// Assembling a flock that is already assembled starts nothing, so a
    /// reply naming only what this call spawned cannot be told from "the roll
    /// was empty".
    ///
    /// One script: `web`'s first start consumes it, so a muster that started
    /// the roll's apps unconditionally would exhaust the pool and land a
    /// second, `Errored` `web` in the listing. The count and the name
    /// assertion are what catch it.
    #[tokio::test]
    async fn a_second_muster_still_reports_the_flock_the_roll_restored() {
        let h = harness(vec![ProcScript::never_exits()]);
        reply_of(
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
        reply_of(dispatch(envelope(2, Request::SaveRoll), &h.ctx).await);

        let reply = reply_of(dispatch(envelope(3, Request::Muster), &h.ctx).await);
        let Ok(Response::Mustered(infos)) = reply.result else {
            panic!("expected Mustered, got {:?}", reply.result)
        };
        assert_eq!(
            infos.len(),
            1,
            "the sheep the roll restores, not the ones this call spawned"
        );
        assert_eq!(infos[0].name, "web");
        assert_eq!(infos[0].status, ProcStatus::Online);
    }

    /// Starts `web` through `h` and returns nothing: no case below asserts
    /// on the start reply itself.
    async fn start_web(h: &Harness) {
        reply_of(
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
    }

    /// The flock a `ListFlock` on `ctx` answers with.
    ///
    /// # Panics
    ///
    /// If the reply is anything but `Flock`, which is a fixture bug.
    async fn list_flock(ctx: &RpcContext, id: u64) -> Vec<ProcessInfo> {
        let reply = reply_of(dispatch(envelope(id, Request::ListFlock), ctx).await);
        let Ok(Response::Flock(infos)) = reply.result else {
            panic!("expected Flock, got {:?}", reply.result)
        };
        infos
    }

    /// Without a live sample the fields come back `None` for a running sheep,
    /// which a reader renders as `-` and an operator reads as "shep cannot
    /// see it".
    #[tokio::test]
    async fn list_flock_carries_a_live_memory_reading_for_a_running_sheep() {
        // The harness's sampler is scripted, so the number below is the
        // fixture's and not the machine's; this asserts the plumbing, not
        // sysinfo. `ScriptedRunner` hands out `FIRST_SCRIPTED_PID`, and the
        // scripted reading describes a tree rooted at that same pid.
        let h = harness_with_stats(vec![ProcScript::never_exits()]);
        start_web(&h).await;

        let infos = list_flock(&h.ctx, 2).await;
        assert_eq!(infos[0].pid, Some(FIRST_SCRIPTED_PID));
        assert_eq!(infos[0].memory_bytes, Some(SCRIPTED_TREE_BYTES));
        assert_eq!(
            infos[0].cpu_percent, None,
            "no periodic baseline has been recorded, and a number invented \
             from the read's own window is worse than an empty cell"
        );
    }

    /// A baseline exists here, so a real number has to come back, and the
    /// second listing says which window produced it: 1500 CPU-ms over the
    /// 15 s since the baseline is 10%, while the same counter over the
    /// millisecond since the previous listing is hundreds of percent.
    #[tokio::test]
    async fn list_flock_measures_cpu_from_the_periodic_baseline_not_from_the_previous_listing() {
        let h = harness_with_stats(vec![ProcScript::never_exits()]);
        start_web(&h).await;
        // A baseline dated one poll interval back, which is what the tick
        // would have left behind had one fired: the clock here is real, so a
        // test that waited for the enforcer's own tick would wait 15 s.
        let last_tick = Instant::now()
            .checked_sub(MEMORY_POLL_INTERVAL)
            .expect("the monotonic clock is older than one poll interval");
        h.stats.record_baseline_now(last_tick);

        let first = list_flock(&h.ctx, 2).await[0]
            .cpu_percent
            .expect("a baseline exists, so a running sheep has a CPU figure");
        let second = list_flock(&h.ctx, 3).await[0]
            .cpu_percent
            .expect("a baseline exists, so a running sheep has a CPU figure");

        assert!(
            (5.0..=10.05).contains(&first),
            "1500 CPU-ms over the 15 s since the baseline is 10%; got {first}"
        );
        assert!(
            (second - first).abs() < 1.0,
            "the second listing divided by the gap between the two LISTINGS \
             rather than by the window since the tick: {first} then {second}"
        );
    }

    /// `Describe` is the second of the two verbs an operator reads resource
    /// usage from, and an implementation wired into `ListFlock` alone passes
    /// every other case here.
    #[tokio::test]
    async fn describe_carries_a_live_reading_too() {
        let h = harness_with_stats(vec![ProcScript::never_exits()]);
        start_web(&h).await;

        let described = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Describe {
                        selector: SelectorSpec::Name("web".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::Described(infos)) = described.result else {
            panic!("expected Described, got {:?}", described.result)
        };
        assert_eq!(infos[0].memory_bytes, Some(SCRIPTED_TREE_BYTES));
    }

    /// The join is keyed on the pid a reading was taken against; one falling
    /// back to the id, or to the first reading in the sample, would print one
    /// sheep's resource use against another.
    ///
    /// Two sheep, and both are needed: stopping a sheep unwatches it, so a
    /// listing holding only the stopped one leaves the sample empty and every
    /// join misses.
    #[tokio::test]
    async fn a_sheep_with_no_pid_reports_no_stats() {
        let h = harness_with_stats(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        start_web(&h).await;
        reply_of(
            dispatch(
                envelope(
                    2,
                    Request::Start {
                        apps: vec![AppConfig::minimal("worker", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        reply_of(
            dispatch(
                envelope(
                    3,
                    Request::Stop {
                        selector: SelectorSpec::Name("worker".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        let infos = list_flock(&h.ctx, 4).await;
        let named = |name: &str| {
            infos
                .iter()
                .find(|info| info.name == name)
                .unwrap_or_else(|| panic!("{name} is missing from the listing"))
        };
        // The scripted table describes the first spawn's pid and no other,
        // so this is the one row carrying a reading, and the one a fallback
        // join would hand to its neighbour.
        assert_eq!(named("web").pid, Some(FIRST_SCRIPTED_PID));
        assert_eq!(named("web").memory_bytes, Some(SCRIPTED_TREE_BYTES));

        assert_eq!(named("worker").pid, None);
        assert_eq!(named("worker").memory_bytes, None);
        assert_eq!(named("worker").cpu_percent, None);
    }

    /// A 5.77 ms syscall walk over the host's whole process table, on every
    /// `start`, buys a reading nobody reads there.
    ///
    /// Asserted on `Started` rather than on `Stopped`: a stopped sheep has no
    /// pid, so its row comes back empty whether or not the verb sampled and
    /// the assertion would hold for either implementation.
    #[tokio::test]
    async fn a_lifecycle_reply_carries_no_stats() {
        let h = harness_with_stats(vec![ProcScript::never_exits()]);
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
        let Ok(Response::Started(infos)) = started.result else {
            panic!("expected Started, got {:?}", started.result)
        };
        assert_eq!(
            infos[0].pid,
            Some(FIRST_SCRIPTED_PID),
            "a row with no pid would report no stats however the verb behaved"
        );
        assert_eq!(
            infos[0].memory_bytes, None,
            "only `flock` and `describe` take a live sample"
        );
        assert_eq!(infos[0].cpu_percent, None);
    }

    /// Enables `name` as a built-in dog through the real dispatch path,
    /// returning the entry it registered.
    async fn enable_dog(ctx: &RpcContext, id: u64, name: &str) -> ProcessInfo {
        let reply = reply_of(
            dispatch(
                envelope(
                    id,
                    Request::EnableDog {
                        name: name.to_string(),
                        source: DogSource::BuiltIn,
                    },
                ),
                ctx,
            )
            .await,
        );
        let Ok(Response::DogStarted(info)) = reply.result else {
            panic!("expected DogStarted, got {:?}", reply.result)
        };
        info
    }

    /// A handler that answered `Deleted(vec![])` without stopping anything
    /// passes every type-level test and leaves the dog running after `shep
    /// disable` reported success.
    #[tokio::test(start_paused = true)]
    async fn disabling_a_dog_stops_it_and_takes_it_off_the_listing() {
        let h = harness(vec![ProcScript::never_exits()]);
        let info = enable_dog(&h.ctx, 1, "bark").await;
        assert_eq!(info.dog, Some(DogSource::BuiltIn));

        let disabled = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::DisableDog {
                        name: "bark".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(disabled.result.unwrap(), Response::Deleted(vec![info.id]));
        assert!(h.ctx.supervisor.list().await.is_empty());
    }

    /// The file is written after the harness built its context, so a reader
    /// that cached at boot answers the empty string here.
    #[tokio::test(start_paused = true)]
    async fn a_dog_config_request_reads_the_file_as_it_stands_now() {
        let h = harness(vec![]);
        std::fs::write(&h.ctx.dogs_config, "[bark]\ndebounce = \"30s\"\n").unwrap();
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::DogConfig {
                        name: "bark".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::DogSection { toml }) = reply.result else {
            panic!("expected DogSection, got {:?}", reply.result)
        };
        assert!(toml.contains("30s"));
    }

    /// Both halves: a filter that excluded dogs outright would leave `shep
    /// describe bark` unable to answer at all, and a listing that includes
    /// them puts a row in the flock table with nowhere to go.
    #[tokio::test(start_paused = true)]
    async fn describe_sweeps_past_a_dog_and_still_answers_when_one_is_named() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
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
        assert!(started.result.is_ok(), "{:?}", started.result);
        let dog = enable_dog(&h.ctx, 2, "bark").await;

        let swept = reply_of(
            dispatch(
                envelope(
                    3,
                    Request::Describe {
                        selector: SelectorSpec::All,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::Described(hits)) = swept.result else {
            panic!("expected Described, got {:?}", swept.result)
        };
        assert_eq!(
            hits.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            vec!["web"],
            "`all` is the flock, not the kennel"
        );

        let named = reply_of(
            dispatch(
                envelope(
                    4,
                    Request::Describe {
                        selector: SelectorSpec::Name("bark".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::Described(hits)) = named.result else {
            panic!("expected Described, got {:?}", named.result)
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, dog.id);
    }

    /// `start_dog` is idempotent by name, so the squatter comes back as an
    /// `Ok`, and a caller that trusted it would print "bark enabled", write
    /// `enabled_dogs = ["bark"]`, and never have a dog.
    #[tokio::test(start_paused = true)]
    async fn enabling_a_dog_over_a_sheeps_name_is_refused_rather_than_faked() {
        let h = harness(vec![ProcScript::never_exits()]);
        let started = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Start {
                        apps: vec![AppConfig::minimal("bark", "./srv")],
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert!(started.result.is_ok(), "{:?}", started.result);

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::EnableDog {
                        name: "bark".to_string(),
                        source: DogSource::BuiltIn,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Err(err) = reply.result else {
            panic!("expected a refusal, got {:?}", reply.result)
        };
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(
            err.message.contains("bark"),
            "the refusal names the collision: {}",
            err.message
        );
        let listed = h.ctx.supervisor.list().await;
        assert_eq!(listed.len(), 1, "nothing was started: {listed:?}");
        assert_eq!(listed[0].dog, None);
    }

    /// The split is a cost decision (`with_lambs`) and nothing else enforces
    /// it: both arms build their rows from the same `snapshot_all`, so a
    /// helper applied in the wrong place looks correct at every other level.
    #[tokio::test(start_paused = true)]
    async fn only_describe_carries_a_lamb_tree() {
        // A process table where FIRST_SCRIPTED_PID really has a child, so a
        // walk that runs finds something and a walk that does not is
        // distinguishable from one that found nothing.
        let h = harness_identifying(
            vec![ProcScript::never_exits()],
            vec![
                identity(FIRST_SCRIPTED_PID, None, "srv"),
                identity(FIRST_SCRIPTED_PID + 1, Some(FIRST_SCRIPTED_PID), "node"),
            ],
        );
        reply_of(
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

        let listed = reply_of(dispatch(envelope(2, Request::ListFlock), &h.ctx).await);
        let Ok(Response::Flock(rows)) = listed.result else {
            panic!("expected a flock listing");
        };
        assert!(
            rows.iter().all(|row| row.lambs.is_none()),
            "ListFlock must not walk the process table"
        );

        let described = reply_of(
            dispatch(
                envelope(
                    3,
                    Request::Describe {
                        selector: SelectorSpec::Name("web".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::Described(rows)) = described.result else {
            panic!("expected a describe listing");
        };
        assert_eq!(
            rows[0].lambs,
            Some(vec![Lamb::new(FIRST_SCRIPTED_PID + 1, "node")])
        );
    }

    /// Registers one built-in dog on `ctx`'s supervisor, the same way
    /// `spawn_enabled_dogs` does at boot.
    async fn start_dog(ctx: &RpcContext, name: &str) -> ProcessInfo {
        let spec = DogSpec {
            name: name.to_string(),
            source: DogSource::BuiltIn,
        };
        let app = crate::dogs::dog_app(&spec, &ctx.paths).expect("the dog fixture must assemble");
        ctx.supervisor
            .start_dog(app, DogSource::BuiltIn)
            .await
            .expect("the dog fixture must start")
    }

    /// The two lists `Request::DogStaleness` answers with.
    async fn staleness(ctx: &RpcContext) -> (Vec<String>, Vec<String>) {
        let reply = reply_of(dispatch(envelope(1, Request::DogStaleness), ctx).await);
        let Ok(Response::DogStaleness { stale, pending }) = reply.result else {
            panic!("expected a dog staleness answer");
        };
        (stale, pending)
    }

    /// The sheep is the point, not scenery: a reader that walked every row
    /// instead of every dog row would hold an operator's reload open waiting
    /// for `web` to handshake, which a sheep never does.
    #[tokio::test]
    async fn a_flock_of_ordinary_sheep_has_nothing_stale_and_nothing_pending() {
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
        started.result.expect("the sheep must start");

        assert_eq!(staleness(&h.ctx).await, (Vec::new(), Vec::new()));
    }

    /// `shep flock` printed `(o.o) online`, restarts 0, for a dog whose own
    /// log was filling with protocol refusals: `status` answers a question
    /// the operator was not asking. Both halves are asserted, since losing
    /// the liveness would be the same defect pointed the other way.
    #[tokio::test]
    async fn a_listing_says_which_dogs_have_answered_this_shepherd() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        start_dog(&h.ctx, "metrics").await;

        let silent = list_flock(&h.ctx, 1).await;
        let dog = silent
            .iter()
            .find(|info| info.name == "metrics")
            .expect("the dog must be listed");
        assert_eq!(dog.handshook, Some(false));
        assert_eq!(
            dog.status,
            ProcStatus::Online,
            "the process is up, and the listing still says so"
        );

        h.ctx.dog_refusals.handshook("metrics");
        let talking = list_flock(&h.ctx, 2).await;
        assert_eq!(
            talking
                .iter()
                .find(|info| info.name == "metrics")
                .expect("still listed")
                .handshook,
            Some(true)
        );
    }

    /// A sheep does not speak this protocol at all, so it has no handshake to
    /// report and `None` is the only honest answer. `Some(false)` here would
    /// paint every sheep in the flock as broken.
    #[tokio::test]
    async fn a_sheep_carries_no_handshake_fact_at_all() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web(&h).await;

        let infos = list_flock(&h.ctx, 1).await;
        assert_eq!(infos[0].name, "web");
        assert_eq!(infos[0].handshook, None);
        assert_eq!(
            infos[0].dog_stale, None,
            "a sheep is never given up on, because it was never asked to answer"
        );
    }

    /// Both rows are `handshook: Some(false)` with a live process. One needs
    /// nothing done about it, the dog having been spawned a moment ago; the
    /// other is a dog this shepherd will never restart again.
    #[tokio::test]
    async fn a_listing_says_which_silent_dogs_this_shepherd_gave_up_on() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_dog(&h.ctx, "metrics").await;

        let waiting = list_flock(&h.ctx, 1).await;
        let dog = waiting
            .iter()
            .find(|info| info.name == "metrics")
            .expect("the dog must be listed");
        assert_eq!(dog.handshook, Some(false));
        assert_eq!(
            dog.dog_stale,
            Some(false),
            "a dog that has not answered YET is not one this shepherd gave up on"
        );

        // The ladder, driven the same way `a_dog_being_restarted_is_pending_
        // and_then_stale` drives it: one refusal buys the restart, the second
        // is the give-up.
        h.ctx.dog_refusals.refused("metrics");
        h.ctx.dog_refusals.refused("metrics");

        let given_up = list_flock(&h.ctx, 2).await;
        let dog = given_up
            .iter()
            .find(|info| info.name == "metrics")
            .expect("still listed");
        assert_eq!(dog.dog_stale, Some(true));
        assert_eq!(
            dog.status,
            ProcStatus::Online,
            "the process is still up, and the listing still says so"
        );

        // And it heals: a dog that gets in clears everything held against
        // it, so the listing must stop reporting the give-up.
        h.ctx.dog_refusals.handshook("metrics");
        let talking = list_flock(&h.ctx, 3).await;
        let dog = talking
            .iter()
            .find(|info| info.name == "metrics")
            .expect("still listed");
        assert_eq!(dog.handshook, Some(true));
        assert_eq!(dog.dog_stale, Some(false));
    }

    /// fails if `describe` answers a different question from `flock` about
    /// the same dog. It is the other verb an operator reads a listing from,
    /// and the one `shep describe <dog>` reaches by name.
    #[tokio::test]
    async fn describe_carries_the_handshake_fact_too() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_dog(&h.ctx, "metrics").await;

        let described = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::Describe {
                        selector: SelectorSpec::Name("metrics".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::Described(rows)) = described.result else {
            panic!("expected a describe listing");
        };
        assert_eq!(rows[0].handshook, Some(false));
    }

    /// A carried dog holds that state for the whole gap between the exec and
    /// its reconnect, so a report taken while it holds would read "nothing
    /// stale" as "every dog came back".
    #[tokio::test]
    async fn a_dog_that_has_not_handshaken_is_pending() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_dog(&h.ctx, "metrics").await;

        assert_eq!(
            staleness(&h.ctx).await,
            (Vec::new(), vec!["metrics".to_string()])
        );

        h.ctx.dog_refusals.handshook("metrics");
        assert_eq!(
            staleness(&h.ctx).await,
            (Vec::new(), Vec::new()),
            "a dog talking to this shepherd is settled and is not worth reporting"
        );
    }

    /// A refused dog passes through this state on its way to being stale, so
    /// a reader that treated it as settled would report every stale dog as
    /// healthy. Drives the ladder rather than asserting on the record,
    /// because the claim is about what a caller over the wire sees.
    #[tokio::test]
    async fn a_dog_being_restarted_is_pending_and_then_stale() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_dog(&h.ctx, "metrics").await;
        h.ctx.dog_refusals.handshook("metrics");

        h.ctx.dog_refusals.refused("metrics");
        assert_eq!(
            staleness(&h.ctx).await,
            (Vec::new(), vec!["metrics".to_string()]),
            "one refusal buys a restart; it does not condemn the dog"
        );

        h.ctx.dog_refusals.refused("metrics");
        assert_eq!(
            staleness(&h.ctx).await,
            (vec!["metrics".to_string()], Vec::new()),
            "a stale dog is a finding, not something still to wait on"
        );
    }

    /// A dog with no process, out of its restart budget, parked in a backoff
    /// or stopped by an operator, cannot handshake, so waiting on one would
    /// make every later reload pay the whole budget for a dog already
    /// reported broken everywhere else.
    #[tokio::test]
    async fn a_dog_that_has_stopped_running_is_not_waited_on() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_dog(&h.ctx, "metrics").await;
        assert_eq!(staleness(&h.ctx).await.1, vec!["metrics".to_string()]);

        h.ctx
            .supervisor
            .stop(ProcessSelector::Name("metrics".to_string()))
            .await
            .expect("the dog must stop");

        assert_eq!(
            staleness(&h.ctx).await,
            (Vec::new(), Vec::new()),
            "a dog that is not running has nothing to answer with"
        );
    }
}
