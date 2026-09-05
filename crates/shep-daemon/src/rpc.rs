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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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

/// Every dog name this shepherd may hold a section for, running or not.
///
/// Seeded at boot from the CLI, which owns `shep.toml`. A
/// `Request::EnableDog` adds a name, so a dog adopted against a running
/// shepherd needs no reload. Never shrunk: `shep disable` leaves the
/// section in `dogs.toml`, where a disabled dog still wants configuring.
/// A set because the only question asked is membership, behind a mutex
/// because every connection holds a clone.
#[derive(Debug, Clone)]
pub(crate) struct KnownDogs {
    names: Arc<Mutex<BTreeSet<String>>>,
}

impl KnownDogs {
    /// Wraps a boot-time seed.
    pub(crate) fn new(names: BTreeSet<String>) -> Self {
        Self {
            names: Arc::new(Mutex::new(names)),
        }
    }

    /// Whether this shepherd may hold a section for `name`.
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.lock().contains(name)
    }

    /// Records that `name` is a dog this shepherd knows about.
    pub(crate) fn insert(&self, name: &str) {
        self.lock().insert(name.to_owned());
    }

    /// A poisoned lock is recovered rather than propagated, as
    /// [`crate::dogs::DogRefusals`] does: these are names with no invariant
    /// a panic mid-write could have broken.
    fn lock(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.names.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

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
    /// Every dog name this shepherd may hold a section for, running or
    /// not. See [`KnownDogs`].
    pub(crate) known_dogs: KnownDogs,
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
                            // The one place this daemon learns of a dog it
                            // was not told about at boot. `shep adopt` and
                            // `shep enable` both arrive here, and both have
                            // just written the name into `shep.toml`, which
                            // this crate does not read. Recorded on the
                            // success arm only: the refusal above is a
                            // sheep holding the name, and that is not a dog
                            // to remember.
                            ctx.known_dogs.insert(&info.name);
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
        // The two config-pane reads and writes. Neither takes a selector:
        // a pane edits one sheep, so an unknown name is `NotFound` here
        // rather than an empty match.
        Request::SheepConfig { name } => match ctx.supervisor.sheep_config(name.clone()).await {
            Ok(Some(view)) => reply(Ok(Response::SheepConfig(Box::new(view)))),
            Ok(None) => reply(Err(RpcError {
                code: RpcErrorCode::NotFound,
                message: format!("no sheep named {name}"),
                daemon_version: None,
            })),
            Err(err) => reply(Err(rpc_error(&err))),
        },
        Request::SetSheepEnv { name, key, value } => {
            match ctx
                .supervisor
                .set_sheep_env(name.clone(), key.clone(), value)
                .await
            {
                // Recorded exactly as `Start`, `Add`, `Scale` and
                // `ApplyConfig` record theirs, and for the reason
                // `Scale`'s arm gives: the muster roll is written from the
                // registry (`snapshot::muster`) and nothing on the restore
                // path reads the override store, so an env edit that
                // skipped this would survive a `shep daemon reload` -- the
                // handover blob carries `pending` -- and vanish on a cold
                // restart. The same field class behaving differently
                // depending on which request set it is the bug.
                Ok(Some(app)) => {
                    ctx.registry.record(&[app]);
                    reply(Ok(Response::SheepEnvSet { name, key }))
                }
                Ok(None) => reply(Err(RpcError {
                    code: RpcErrorCode::NotFound,
                    message: format!("no sheep named {name}"),
                    daemon_version: None,
                })),
                Err(err) => reply(Err(rpc_error(&err))),
            }
        }
        Request::SetSheepField { name, key, value } => {
            match ctx
                .supervisor
                .set_sheep_field(name.clone(), key.clone(), value)
                .await
            {
                // Recorded exactly as `SetSheepEnv` records its own, and
                // for that arm's reason: the muster roll is written from
                // the registry and nothing on the restore path reads the
                // override store, so a field edit that skipped this would
                // survive a `shep daemon reload` and vanish on a cold
                // restart.
                Ok(Some(set)) => {
                    ctx.registry.record(&[set.app]);
                    reply(Ok(Response::SheepFieldSet {
                        name,
                        key,
                        pending: set.pending,
                    }))
                }
                Ok(None) => reply(Err(RpcError {
                    code: RpcErrorCode::NotFound,
                    message: format!("no sheep named {name}"),
                    daemon_version: None,
                })),
                Err(err) => reply(Err(rpc_error(&err))),
            }
        }
        // The inverse of every other config door's guard, and deliberately
        // so: `dogs.toml` holds dogs' sections and nothing else, so what
        // this one refuses is a SHEEP, and a name that is no dog of this
        // shepherd's. Asked before the file is opened, so a mistyped name
        // cannot leave a table behind for a dog that will never exist.
        //
        // Asked of [`RpcContext::known_dogs`] and NOT of the running
        // flock. A guard on `supervisor.list()` compiles, passes a test
        // that enables a dog first, and refuses the one dog this feature
        // exists for: the dog-config design's decision 4 is that the dog
        // most in need of configuring is the one that is disabled or has
        // never started, so an operator who adopts a dog and opens its
        // pane to set a webhook before switching it on would be told
        // there is no such dog. The running set is still consulted, as a
        // widening: a dog adopted and enabled since this shepherd booted
        // is not in the list the CLI handed over at boot, and refusing
        // that one would be the same bug in its second shape.
        //
        // Written here rather than through the supervisor: `dogs.toml` is
        // not supervisor state the way the override store is, and the two
        // things this needs -- the file's path and the bus -- are both
        // already in scope. The daemon and not the client writes it because
        // the daemon is the only publisher of `config.dog.<name>`, and a
        // section written with nothing publishing that topic leaves a
        // running dog reading the old one.
        Request::SetDogConfig { name, toml } => {
            let running_dog = || async {
                ctx.supervisor
                    .list()
                    .await
                    .iter()
                    .any(|info| info.name == name && info.dog.is_some())
            };
            if !ctx.known_dogs.contains(&name) && !running_dog().await {
                return reply(Err(RpcError {
                    code: RpcErrorCode::NotFound,
                    message: format!(
                        "no dog named {name}; `shep adopt` or `shep enable` makes one known \
                         to this shepherd"
                    ),
                    daemon_version: None,
                }));
            }
            match crate::dogs::set_dog_section(&ctx.dogs_config, &name, toml.as_str()) {
                Ok(()) => {
                    crate::bus::publish_dog_config_changed(
                        &ctx.events,
                        std::slice::from_ref(&name),
                    );
                    reply(Ok(Response::DogConfigSet { name }))
                }
                Err(err) => reply(Err(RpcError {
                    code: RpcErrorCode::InvalidConfig,
                    message: err.to_string(),
                    daemon_version: None,
                })),
            }
        }
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
        // `InvalidConfig`, like `InvalidScale` above and for its reason: a
        // request aimed at a dog is one the caller can aim elsewhere. The
        // bare payload is the same sentence `apply_one` puts in front of an
        // operator whose Flockfile named a dog, so the two doors read alike.
        SupervisorError::IsADog(msg) => RpcError {
            code: RpcErrorCode::InvalidConfig,
            message: msg.clone(),
            daemon_version: None,
        },
        // `InvalidConfig`, like `InvalidScale` above and for its reason:
        // this is something the caller asked for that it can ask
        // differently, and telling an operator "unexpected daemon-side
        // failure" about their own env key would send them to the wrong
        // place entirely. The bare payload is `normalize`'s own refusal,
        // which already names the key.
        SupervisorError::InvalidEnv(msg) => RpcError {
            code: RpcErrorCode::InvalidConfig,
            message: msg.clone(),
            daemon_version: None,
        },
        // `InvalidConfig`, beside `InvalidEnv` above and for its reason:
        // every shape that reaches it is the caller's own key or value,
        // which it can ask differently. The bare payload rather than
        // `err.to_string()`, again like `InvalidEnv`: the message already
        // names the field.
        SupervisorError::InvalidField(msg) => RpcError {
            code: RpcErrorCode::InvalidConfig,
            message: msg.clone(),
            daemon_version: None,
        },
        // `Internal`, on the same rule the log-maintenance pair above
        // states: an override store that cannot be read or written is an
        // unexpected daemon-side failure, and there is no code for it that
        // a client predating this build could decode. `err.to_string()`
        // rather than the bare payload, so the reader is told the store was
        // the thing that failed and not the request.
        SupervisorError::Overrides(_) => RpcError {
            code: RpcErrorCode::Internal,
            message: err.to_string(),
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
    use shep_core::config::{AppConfig, ApplyGroup, DeclaredApp, ResetDepth, apply_group};
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

    /// Starts one sheep named `web` carrying a secret env value, which is
    /// the fixture the config-pane cases below all want.
    async fn start_web_with_a_secret(ctx: &RpcContext) {
        let mut config = AppConfig::minimal("web", "./srv");
        config
            .env
            .insert("DB_PASS".to_string(), "hunter2".to_string());
        let started =
            reply_of(dispatch(envelope(1, Request::Start { apps: vec![config] }), ctx).await);
        assert!(started.result.is_ok(), "{:?}", started.result);
    }

    /// fails if a dog can be handed a sheep's env override. A dog runs at
    /// the daemon's own trust level and its binary is what `shep adopt`
    /// vetted, so a `PATH`, an `LD_PRELOAD` or a `DYLD_INSERT_LIBRARIES`
    /// parked for its next respawn is arbitrary code at that level. Nothing
    /// in the tree sends this yet; the socket is live, which is the whole
    /// reason it is refused at the daemon rather than at a caller.
    ///
    /// Asserts the store and the entry as well as the code: a refusal that
    /// answered `InvalidConfig` after writing would be the same hole with a
    /// better error message.
    #[tokio::test(start_paused = true)]
    async fn a_dog_is_refused_an_env_override_rather_than_given_one() {
        let h = harness(vec![ProcScript::never_exits()]);
        let dog = enable_dog(&h.ctx, 1, "bark").await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetSheepEnv {
                        name: "bark".to_string(),
                        key: "PATH".to_string(),
                        value: Some("/tmp/evil".to_string().into()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Err(err) = reply.result else {
            panic!("a dog was given an env override")
        };
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(err.message.contains("bark is a dog"), "{}", err.message);

        assert!(
            shep_core::overrides::get(&h.ctx.paths.overrides, "bark")
                .unwrap()
                .is_none(),
            "the refusal still wrote the store"
        );
        let described = reply_of(
            dispatch(
                envelope(
                    3,
                    Request::Describe {
                        selector: SelectorSpec::Name("bark".to_string()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::Described(infos)) = described.result else {
            panic!("expected Described")
        };
        assert_eq!(infos[0].id, dog.id);
        assert_eq!(infos[0].pending, None, "the refusal still parked a config");
    }

    /// fails if a dog's whole `AppConfig` can be read through the sheep
    /// config pane. No other request hands a client a config at all, so this
    /// would be a read surface that exists for dogs and nothing else, and a
    /// dog's config is what `shep adopt` vetted rather than anything an
    /// operator is meant to edit here.
    #[tokio::test(start_paused = true)]
    async fn a_dogs_config_is_not_readable_through_the_sheep_config_pane() {
        let h = harness(vec![ProcScript::never_exits()]);
        enable_dog(&h.ctx, 1, "bark").await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SheepConfig {
                        name: "bark".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Err(err) = reply.result else {
            panic!("a dog's config was served to a pane")
        };
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(err.message.contains("bark is a dog"), "{}", err.message);
    }

    /// Reads one sheep's config view, for the cases that assert on it.
    async fn sheep_config_view(
        ctx: &RpcContext,
        id: u64,
        name: &str,
    ) -> shep_core::protocol::SheepConfigView {
        let reply = reply_of(
            dispatch(
                envelope(
                    id,
                    Request::SheepConfig {
                        name: name.to_string(),
                    },
                ),
                ctx,
            )
            .await,
        );
        match reply.result {
            Ok(Response::SheepConfig(view)) => *view,
            other => panic!("expected SheepConfig, got {other:?}"),
        }
    }

    /// fails if a config pane's answer carries an env VALUE, and fails if
    /// it stops listing the keys. Both halves are the point: a pane that
    /// cannot name the keys cannot offer to edit them, and one that is
    /// handed the values has put a secret on a socket for nothing
    /// (decision 12 of the overrides design, IR-41).
    #[tokio::test(start_paused = true)]
    async fn sheep_config_answers_with_env_emptied_and_its_keys_listed() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SheepConfig {
                        name: "web".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::SheepConfig(view)) = reply.result else {
            panic!("expected SheepConfig")
        };
        assert_eq!(view.name, "web");
        assert!(view.config.env.is_empty());
        assert_eq!(view.env_keys, ["DB_PASS"]);
    }

    /// fails if an unknown name reads as a daemon fault. A pane asking
    /// about a sheep that was deleted under it is a normal thing to have
    /// happen, and `Internal` would send its operator looking for a bug in
    /// the shepherd.
    #[tokio::test(start_paused = true)]
    async fn sheep_config_for_an_unknown_name_is_not_found_not_internal() {
        let h = harness(vec![]);
        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::SheepConfig {
                        name: "ghost".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Err(err) = reply.result else {
            panic!("expected a refusal")
        };
        assert_eq!(err.code, RpcErrorCode::NotFound);
    }

    /// fails if a set env key does not reach the store, and fails if it
    /// reaches the running child instead of parking. The running process
    /// was handed its environment at spawn and cannot be handed another,
    /// so an edit reported as applied would be an edit the operator
    /// believes is in force and is not.
    #[tokio::test(start_paused = true)]
    async fn set_sheep_env_writes_the_store_and_parks_env_until_a_respawn() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetSheepEnv {
                        name: "web".to_string(),
                        key: "NEW".to_string(),
                        value: Some("1".to_string().into()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert!(
            matches!(reply.result, Ok(Response::SheepEnvSet { .. })),
            "{:?}",
            reply.result
        );

        let stored = shep_core::overrides::get(&h.ctx.paths.overrides, "web")
            .unwrap()
            .unwrap();
        assert_eq!(stored.fields["env"]["NEW"], "1");

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
        let Ok(Response::Described(infos)) = described.result else {
            panic!("expected Described")
        };
        assert_eq!(
            infos[0].pending.as_deref(),
            Some(["env".to_string()].as_slice())
        );
    }

    /// fails if a removal leaves the sheep marked as overridden. The CFG
    /// column reads `ProcessEntry::overridden`, so an `env` key left in the
    /// store's field set after the last value under it is gone marks a
    /// sheep that no longer differs from its Flockfile.
    #[tokio::test(start_paused = true)]
    async fn removing_the_last_env_override_stops_marking_the_sheep() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        for (id, value) in [(2, Some("1".to_string().into())), (3, None)] {
            let reply = reply_of(
                dispatch(
                    envelope(
                        id,
                        Request::SetSheepEnv {
                            name: "web".to_string(),
                            key: "NEW".to_string(),
                            value,
                        },
                    ),
                    &h.ctx,
                )
                .await,
            );
            assert!(reply.result.is_ok(), "{:?}", reply.result);
        }

        let stored = shep_core::overrides::get(&h.ctx.paths.overrides, "web")
            .unwrap()
            .unwrap();
        assert!(!stored.fields.contains_key("env"), "{stored:?}");

        let reply = reply_of(
            dispatch(
                envelope(
                    4,
                    Request::SheepConfig {
                        name: "web".to_string(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Ok(Response::SheepConfig(view)) = reply.result else {
            panic!("expected SheepConfig")
        };
        assert!(view.overridden.is_empty(), "{view:?}");
    }

    /// fails if an env edit survives a `shep daemon reload` and is lost by
    /// a cold restart, which is the shape this bug took. The muster roll is
    /// written from the `FlockRegistry` and nothing on the restore path
    /// reads the override store, so a handler that parks a config without
    /// recording it looks correct in every live test and forgets the edit
    /// the next time the machine comes up.
    ///
    /// Asserts the roll rather than the registry's own accessor: the roll
    /// is what `shep muster` actually restores from.
    #[tokio::test(start_paused = true)]
    async fn a_set_env_reaches_the_roll_a_cold_restart_would_come_back_on() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetSheepEnv {
                        name: "web".to_string(),
                        key: "NEW".to_string(),
                        value: Some("1".to_string().into()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert!(reply.result.is_ok(), "{:?}", reply.result);

        let infos = list_flock(&h.ctx, 3).await;
        let roll = h.ctx.registry.roll(&infos, 0);
        let web = roll
            .apps
            .iter()
            .find(|entry| entry.app.name == "web")
            .expect("web is in the roll");
        assert_eq!(web.app.env.get("NEW").map(String::as_str), Some("1"));
    }

    /// fails if removing an env key the operator never overrode records
    /// nothing. `map.remove` is a no-op for a key that was supplied by the
    /// app's own config, so without a tombstone the store comes back empty,
    /// the CFG column marks nothing, and the removal lives only in
    /// `ProcessEntry::pending` -- for a change the operator just made.
    ///
    /// `removing_the_last_env_override_stops_marking_the_sheep` cannot
    /// reach this: it removes a key it set itself, which is the one case
    /// that must NOT leave a tombstone.
    #[tokio::test(start_paused = true)]
    async fn removing_a_key_the_operator_never_set_is_still_recorded() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetSheepEnv {
                        name: "web".to_string(),
                        key: "DB_PASS".to_string(),
                        value: None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert!(reply.result.is_ok(), "{:?}", reply.result);

        let stored = shep_core::overrides::get(&h.ctx.paths.overrides, "web")
            .unwrap()
            .expect("the removal is recorded");
        assert_eq!(
            stored.fields["env"]["DB_PASS"],
            serde_json::Value::Null,
            "a removal is a tombstone, not an absence"
        );

        let view = sheep_config_view(&h.ctx, 3, "web").await;
        assert_eq!(view.overridden, ["env"]);
        assert!(!view.env_keys.contains(&"DB_PASS".to_string()));
    }

    /// fails if a removal stops being REPORTED once the Flockfile that
    /// declares the key is loaded again.
    ///
    /// Two things have to compose and neither is pinned by the tombstone
    /// test above, which stops before any load. `merge_declared`'s env loop
    /// must skip a key held in `overridden_env`, which counts a null like
    /// any other entry, so the file's value does not come back. And
    /// `establish_env`, which runs after that loop, must NOT spend the
    /// tombstone the way it spends an override carrying a value, or the
    /// store comes back empty and `overridden` stops naming `env` while the
    /// sheep genuinely still differs from its file.
    ///
    /// The second is the operator-visible half: `overridden` is the only
    /// signal a sheep diverges from its Flockfile, and the `CFG` column in
    /// `shep flock` and `shep lookout` reads it. A removal is the one edit
    /// that can make it claim identical when it is not.
    ///
    /// Loaded TWICE on purpose. The second load only skips the key because
    /// the first one preserved the record, so a single load would pass
    /// against a build that spent the tombstone and leaned on
    /// `declared_env` alone.
    #[tokio::test(start_paused = true)]
    async fn a_removed_key_stays_removed_and_stays_reported_across_reloads() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let removed = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetSheepEnv {
                        name: "web".to_string(),
                        key: "DB_PASS".to_string(),
                        value: None,
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert!(removed.result.is_ok(), "{:?}", removed.result);

        // The Flockfile still declares the key the operator removed, which
        // is the whole point: a deploy re-runs the same file.
        for id in [3, 4] {
            let loaded = reply_of(
                dispatch(
                    envelope(
                        id,
                        Request::ApplyConfig {
                            apps: vec![DeclaredApp {
                                config: {
                                    let mut app = AppConfig::minimal("web", "./srv");
                                    app.env
                                        .insert("DB_PASS".to_string(), "fromfile".to_string());
                                    app
                                },
                                declared: ["name", "script"]
                                    .iter()
                                    .map(|k| (*k).to_string())
                                    .collect(),
                                declared_env: ["DB_PASS"]
                                    .iter()
                                    .map(|k| (*k).to_string())
                                    .collect(),
                            }],
                            reset: ResetDepth::None,
                        },
                    ),
                    &h.ctx,
                )
                .await,
            );
            let Ok(Response::Applied(report)) = loaded.result else {
                panic!("expected Applied")
            };
            assert_eq!(report[0].refused, None);

            let stored = shep_core::overrides::get(&h.ctx.paths.overrides, "web")
                .unwrap()
                .expect("the removal is still recorded");
            assert_eq!(
                stored.fields["env"]["DB_PASS"],
                serde_json::Value::Null,
                "load {id} spent the tombstone"
            );

            let view = sheep_config_view(&h.ctx, id + 10, "web").await;
            assert!(
                !view.env_keys.contains(&"DB_PASS".to_string()),
                "load {id} put the file's value back"
            );
            assert_eq!(view.overridden, ["env"], "load {id} stopped reporting it");
        }
    }

    /// fails if `SetSheepEnv` writes the store before it validates. Two
    /// halves, and the second is the one a refactor breaks: `SHEP_NAME` is
    /// injected per instance and refused in a hand-written env, so this is
    /// a real refusal an operator can meet from a free-text pane, and a
    /// handler that wrote first would leave a stored override for a config
    /// the daemon will not accept.
    #[tokio::test(start_paused = true)]
    async fn an_env_key_normalize_refuses_is_invalid_config_and_writes_nothing() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetSheepEnv {
                        name: "web".to_string(),
                        key: "SHEP_NAME".to_string(),
                        value: Some("impostor".to_string().into()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Err(err) = reply.result else {
            panic!("a reserved env key was accepted")
        };
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(
            shep_core::overrides::get(&h.ctx.paths.overrides, "web")
                .unwrap()
                .is_none(),
            "the refusal still wrote the store"
        );
    }

    /// fails if an unreadable override store is reported as anything but a
    /// daemon-side fault. It is the one failure here that is neither the
    /// caller's request nor a refusal they can act on by asking
    /// differently, which is the whole reason it has a variant of its own
    /// rather than sharing `InvalidEnv`'s.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_override_store_is_internal_not_a_bad_request() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;
        std::fs::write(&h.ctx.paths.overrides, "{ this is not json").unwrap();

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetSheepEnv {
                        name: "web".to_string(),
                        key: "NEW".to_string(),
                        value: Some("1".to_string().into()),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        let Err(err) = reply.result else {
            panic!("an unreadable store was reported as success")
        };
        assert_eq!(err.code, RpcErrorCode::Internal);
        assert!(
            err.message.contains("overrides store unusable"),
            "{}",
            err.message
        );
    }

    /// Sends one `SetSheepField` and hands back the reply.
    async fn set_field(
        ctx: &RpcContext,
        id: u64,
        name: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<Response, RpcError> {
        reply_of(
            dispatch(
                envelope(
                    id,
                    Request::SetSheepField {
                        name: name.to_string(),
                        key: key.to_string(),
                        value,
                    },
                ),
                ctx,
            )
            .await,
        )
        .result
    }

    /// fails if a pane edit stops being visible as an operator override.
    ///
    /// **This is the whole reason `SetSheepField` exists.** The same edit
    /// through `ApplyConfig` at `ResetDepth::File` moved the field and then
    /// spent the override for it in `merge_declared`, so the key was gone
    /// from `overridden`, `shep flock`'s CFG column counted nothing, and
    /// the pane's `*` marker never appeared for a value the operator had
    /// just set. Asserted end to end -- through `SheepConfig`, which the
    /// pane reads, and `ListFlock`, which the CFG column reads -- because
    /// the request in isolation was correct the whole time it was wrong.
    #[tokio::test(start_paused = true)]
    async fn a_field_edit_is_reported_as_an_operator_override() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let view = sheep_config_view(&h.ctx, 2, "web").await;
        assert!(
            !view.overridden.contains(&"max_restarts".to_string()),
            "nothing is overridden before the edit: {:?}",
            view.overridden
        );

        let reply = set_field(&h.ctx, 3, "web", "max_restarts", serde_json::json!(40)).await;
        assert!(
            matches!(reply, Ok(Response::SheepFieldSet { .. })),
            "{reply:?}"
        );

        let stored = shep_core::overrides::get(&h.ctx.paths.overrides, "web")
            .unwrap()
            .expect("the edit is recorded");
        assert_eq!(stored.fields["max_restarts"], 40);

        let view = sheep_config_view(&h.ctx, 4, "web").await;
        assert_eq!(view.config.max_restarts, 40, "the pane shows the new value");
        assert!(
            view.overridden.contains(&"max_restarts".to_string()),
            "the `*` marker reads this: {:?}",
            view.overridden
        );

        // The CFG column's own source, which is a different code path from
        // the pane's and is the half that was silently wrong.
        let infos = list_flock(&h.ctx, 5).await;
        let web = infos
            .iter()
            .find(|info| info.name == "web")
            .expect("web is in the flock");
        assert!(
            web.overridden
                .as_deref()
                .is_some_and(|fields| fields.contains(&"max_restarts".to_string())),
            "{:?}",
            web.overridden
        );
    }

    /// fails if the four-way apply classification stops governing this
    /// door. A `Live` field is in force now, a `NeedsRespawn` field parks
    /// and says so, and the pane's cost column promises exactly this.
    #[tokio::test(start_paused = true)]
    async fn a_live_field_applies_now_and_a_respawn_field_parks() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let reply = set_field(&h.ctx, 2, "web", "max_restarts", serde_json::json!(40)).await;
        let Ok(Response::SheepFieldSet { pending, .. }) = reply else {
            panic!("{reply:?}")
        };
        assert!(!pending, "max_restarts is Live and is in force now");
        let view = sheep_config_view(&h.ctx, 3, "web").await;
        assert!(
            !view.pending.contains(&"max_restarts".to_string()),
            "{:?}",
            view.pending
        );

        let reply = set_field(&h.ctx, 4, "web", "script", serde_json::json!("./next")).await;
        let Ok(Response::SheepFieldSet { pending, .. }) = reply else {
            panic!("{reply:?}")
        };
        assert!(pending, "script needs a respawn");
        let view = sheep_config_view(&h.ctx, 5, "web").await;
        assert!(
            view.pending.contains(&"script".to_string()),
            "{:?}",
            view.pending
        );
        // And the Live edit is still in force beside the parked one.
        assert_eq!(view.config.max_restarts, 40);
        assert_eq!(
            view.overridden,
            ["max_restarts", "script"],
            "both are the operator's"
        );
    }

    /// fails if a `Live` field that cannot reach the running child stops
    /// saying so.
    ///
    /// **One of the two cases that make `pending` worth carrying at all.**
    /// The other three answers this reply gives -- `max_restarts` in force,
    /// `script` parked -- a client could have derived from `apply_group`
    /// without asking. This one it cannot: `reached_spec` builds a SUBSET
    /// of the config, running plus the one field that reaches, and
    /// `normalize` checks fields against each other. `watch` needs a `cwd`,
    /// and a `cwd` that is itself still parked is not on the running spec
    /// for the subset to find, so a `Live` field parks. If this ever stops
    /// being reachable, `pending` is a restatement of `apply_group` and
    /// should be deleted.
    #[tokio::test(start_paused = true)]
    async fn a_live_field_whose_subset_will_not_normalize_parks_instead() {
        let h = harness(vec![ProcScript::never_exits()]);
        // No `cwd`, which is what makes `watch` refusable on its own.
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

        // `cwd` is NeedsRespawn, so this parks and the RUNNING spec still
        // has none.
        let reply = set_field(&h.ctx, 2, "web", "cwd", serde_json::json!("/srv")).await;
        let Ok(Response::SheepFieldSet { pending, .. }) = reply else {
            panic!("{reply:?}")
        };
        assert!(pending, "cwd needs a respawn");

        // `watch` is Live, so `apply_group` alone predicts "in force now".
        // The merge is valid -- it carries the parked `cwd` -- but the
        // subset is `running + watch`, which is a watch with no directory.
        let reply = set_field(&h.ctx, 3, "web", "watch", serde_json::json!(true)).await;
        let Ok(Response::SheepFieldSet { pending, .. }) = reply else {
            panic!("{reply:?}")
        };
        assert_eq!(
            apply_group("watch"),
            ApplyGroup::Live,
            "the premise: apply_group predicts this one applies now"
        );
        assert!(
            pending,
            "a Live field the running child cannot be given still parks"
        );

        // And the pane's own durable marker agrees, so an operator who
        // misses the status line still sees it on the row.
        let view = sheep_config_view(&h.ctx, 4, "web").await;
        assert!(
            view.pending.contains(&"watch".to_string()),
            "{:?}",
            view.pending
        );
    }

    /// fails if `autostart` stops reporting as in force.
    ///
    /// **The second case that makes `pending` worth carrying.** It is
    /// `ApplyGroup::NextSpawn`, so `apply_group` predicts an operator must
    /// restart for it, and that would be telling them to do nothing useful:
    /// `snapshot::restorable` reads it at muster or boot rather than at a
    /// spawn, so it is in force the moment it lands on the stored spec.
    /// `kill_signal` is its group-mate and really is read at a spawn, and
    /// is asserted beside it so this pins the carve-out rather than the
    /// whole group.
    #[tokio::test(start_paused = true)]
    async fn autostart_reports_in_force_and_its_group_mate_reports_pending() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        for key in ["autostart", "kill_signal"] {
            assert_eq!(
                apply_group(key),
                ApplyGroup::NextSpawn,
                "the premise: both are the same group"
            );
        }

        let reply = set_field(&h.ctx, 2, "web", "autostart", serde_json::json!(false)).await;
        let Ok(Response::SheepFieldSet { pending, .. }) = reply else {
            panic!("{reply:?}")
        };
        assert!(
            !pending,
            "autostart is read at muster, so a restart would do nothing"
        );

        let reply = set_field(&h.ctx, 3, "web", "kill_signal", serde_json::json!("SIGINT")).await;
        let Ok(Response::SheepFieldSet { pending, .. }) = reply else {
            panic!("{reply:?}")
        };
        assert!(pending, "kill_signal really is read at a spawn");
    }

    /// fails if a dog can be handed a config field. The same hole
    /// `a_dog_is_refused_an_env_override_rather_than_given_one` closes for
    /// `env`, and sharper: this door reaches `script` and `args` directly,
    /// and a dog runs at the daemon's own trust level.
    ///
    /// Asserts the store as well as the code: a refusal that answered
    /// `InvalidConfig` after writing would be the same hole with a better
    /// error message.
    #[tokio::test(start_paused = true)]
    async fn a_dog_is_refused_a_config_field_rather_than_given_one() {
        let h = harness(vec![ProcScript::never_exits()]);
        let dog = enable_dog(&h.ctx, 1, "bark").await;

        let reply = set_field(&h.ctx, 2, "bark", "script", serde_json::json!("/tmp/evil")).await;
        let Err(err) = reply else {
            panic!("a dog was given a config field")
        };
        assert_eq!(err.code, RpcErrorCode::InvalidConfig);
        assert!(err.message.contains("bark is a dog"), "{}", err.message);
        assert!(
            shep_core::overrides::get(&h.ctx.paths.overrides, "bark")
                .unwrap()
                .is_none(),
            "the refusal still wrote the store"
        );
        drop(dog);
    }

    /// fails if the three keys this door does not own stop being refused.
    ///
    /// `env` would be replaced wholesale by a request that carries one
    /// value, wiping every other key; `instances` and `name` are
    /// Structural, and the count moves through `shep stock`.
    #[tokio::test(start_paused = true)]
    async fn env_and_the_structural_fields_are_refused_by_this_door() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        for (id, key, value) in [
            (2, "env", serde_json::json!({ "A": "1" })),
            (3, "instances", serde_json::json!(4)),
            (4, "name", serde_json::json!("other")),
        ] {
            let reply = set_field(&h.ctx, id, "web", key, value).await;
            let Err(err) = reply else {
                panic!("{key} was accepted")
            };
            assert_eq!(err.code, RpcErrorCode::InvalidConfig, "{key}");
            assert!(err.message.contains(key), "{key}: {}", err.message);
        }
        assert!(
            shep_core::overrides::get(&h.ctx.paths.overrides, "web")
                .unwrap()
                .is_none(),
            "a refusal still wrote the store"
        );
    }

    /// fails if `SetSheepField` writes the store before it validates, or
    /// reports a caller's own bad key or value as a daemon fault.
    ///
    /// Three shapes, and each is the caller's: a key `AppConfig` has no
    /// field for, a value that will not deserialize into the field it
    /// names, and a value that deserializes and then fails `normalize`.
    #[tokio::test(start_paused = true)]
    async fn a_field_this_build_refuses_is_invalid_config_and_writes_nothing() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        for (id, key, value) in [
            (2, "no_such_field", serde_json::json!(1)),
            (3, "max_restarts", serde_json::json!("forty")),
            (
                4,
                "cron_restart",
                serde_json::json!("not a cron expression"),
            ),
        ] {
            let reply = set_field(&h.ctx, id, "web", key, value).await;
            let Err(err) = reply else {
                panic!("{key} was accepted")
            };
            assert_eq!(err.code, RpcErrorCode::InvalidConfig, "{key}");
            assert!(
                shep_core::overrides::get(&h.ctx.paths.overrides, "web")
                    .unwrap()
                    .is_none(),
                "{key}: the refusal still wrote the store"
            );
        }
    }

    /// fails if a store fault is reported as the caller's mistake. The
    /// ordering twin of the test above, and the pair is what
    /// `SetSheepEnv`'s own two pin for its door.
    #[tokio::test(start_paused = true)]
    async fn an_unreadable_store_is_internal_for_a_field_edit_too() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;
        std::fs::write(&h.ctx.paths.overrides, "{ this is not json").unwrap();

        let reply = set_field(&h.ctx, 2, "web", "max_restarts", serde_json::json!(40)).await;
        let Err(err) = reply else {
            panic!("an unreadable store was reported as success")
        };
        assert_eq!(err.code, RpcErrorCode::Internal);
        assert!(
            err.message.contains("overrides store unusable"),
            "{}",
            err.message
        );
    }

    /// fails if a field edit stops reaching the muster roll, which is the
    /// registry record `rpc.rs` writes rather than anything the supervisor
    /// does. Nothing on the restore path reads the override store, so an
    /// edit that skipped it would survive a `shep daemon reload` and vanish
    /// on a cold restart.
    #[tokio::test(start_paused = true)]
    async fn a_field_edit_reaches_the_muster_roll() {
        let h = harness(vec![ProcScript::never_exits()]);
        start_web_with_a_secret(&h.ctx).await;

        let reply = set_field(&h.ctx, 2, "web", "max_restarts", serde_json::json!(40)).await;
        assert!(reply.is_ok(), "{reply:?}");

        let infos = list_flock(&h.ctx, 3).await;
        let roll = h.ctx.registry.roll(&infos, 0);
        let web = roll
            .apps
            .iter()
            .find(|entry| entry.app.name == "web")
            .expect("web is in the roll");
        assert_eq!(web.app.max_restarts, 40);
    }

    /// fails if a variant added to `Request` never gets a dispatch arm.
    ///
    /// The one test here that cannot be replaced by reading the code.
    /// `Request` is `#[non_exhaustive]` and this module's dispatch ends in
    /// a wildcard, so a variant with NO arm compiles, passes every other
    /// test in this file, and answers "this daemon does not implement that
    /// request" at runtime -- a message that sends an operator to upgrade a
    /// daemon that is already current. The wire snapshots cannot catch it
    /// either: they are hand-written literals with no completeness check.
    ///
    /// Every request below names something that does not exist, so a real
    /// arm refuses each of them and nothing is asserted about the refusal
    /// but which one it is.
    ///
    /// **The list is hand-written and nothing makes it exhaustive.** There
    /// is no way to enumerate a `#[non_exhaustive]` enum's variants at
    /// runtime, so a fourth variant added to `Request` is covered by this
    /// test only if its author adds it here. If you are adding one, add it
    /// here.
    #[tokio::test(start_paused = true)]
    async fn every_new_variant_reaches_an_arm_and_not_the_wildcard() {
        let h = harness(vec![]);
        let requests = [
            Request::SheepConfig {
                name: "ghost".to_string(),
            },
            Request::SetSheepEnv {
                name: "ghost".to_string(),
                key: "K".to_string(),
                value: None,
            },
            Request::SetSheepField {
                name: "ghost".to_string(),
                key: "max_restarts".to_string(),
                value: serde_json::json!(1),
            },
            Request::SetDogConfig {
                name: "ghost".to_string(),
                toml: String::new().into(),
            },
        ];
        for (id, request) in requests.into_iter().enumerate() {
            let reply = reply_of(
                dispatch(
                    envelope(
                        u64::try_from(id).expect("three requests fit a u64"),
                        request,
                    ),
                    &h.ctx,
                )
                .await,
            );
            let Err(err) = reply.result else {
                continue;
            };
            assert_ne!(
                err.message, "this daemon does not implement that request",
                "a config-pane variant fell through to the wildcard"
            );
        }
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
        assert!(toml.as_str().contains("30s"));
    }

    /// fails if the section is written without the topic going out, which
    /// is the whole difference between this door and a CLI writing the file
    /// itself: a running dog subscribed to `config.dog.<name>` is the only
    /// reader that finds out a section moved. Asserted on a SUBSCRIBER
    /// rather than on the publish call, the way the dog contract's own
    /// `bark_subscribes_to_its_own_config_topic` does -- a test that the
    /// publisher ran proves nothing about what a dog hears.
    #[tokio::test(start_paused = true)]
    async fn set_dog_config_writes_the_file_and_a_subscriber_hears_about_it() {
        let h = harness(vec![ProcScript::never_exits()]);
        enable_dog(&h.ctx, 1, "bark").await;
        let mut sub = h.ctx.events.subscribe();

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetDogConfig {
                        name: "bark".to_string(),
                        toml: "poll = \"30s\"\n".to_string().into(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );
        assert_eq!(
            reply.result.unwrap(),
            Response::DogConfigSet {
                name: "bark".to_string()
            }
        );

        let text = std::fs::read_to_string(&h.ctx.dogs_config).unwrap();
        assert!(text.contains("poll = \"30s\""), "{text}");

        // The dog's own spawn narrates itself on the same bus, so the
        // topic wanted here is not necessarily the first frame waiting.
        let mut topics = Vec::new();
        while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            topics.push(event.to_event().topic().into_owned());
            if topics
                .last()
                .is_some_and(|topic| topic == "config.dog.bark")
            {
                break;
            }
        }
        assert!(
            topics.iter().any(|topic| topic == "config.dog.bark"),
            "{topics:?}"
        );
    }

    /// fails if the write door is guarded on the RUNNING set. The dog most
    /// in need of configuring is the one that is switched off: an operator
    /// adopts a dog, sets its webhook, and only then enables it, which is
    /// the sequence the dog-config design's decision 4 argues for by name.
    /// A guard on `supervisor.list()` refuses exactly that dog, and this
    /// one has never been started at all.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_is_adopted_and_never_started_can_still_be_configured() {
        let mut h = harness(vec![]);
        h.ctx.known_dogs = KnownDogs::new(["otel".to_string()].into_iter().collect());

        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::SetDogConfig {
                        name: "otel".to_string(),
                        toml: "endpoint = \"http://127.0.0.1:4317\"\n".to_string().into(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        assert_eq!(
            reply.result.unwrap(),
            Response::DogConfigSet {
                name: "otel".to_string()
            }
        );
        let text = std::fs::read_to_string(&h.ctx.dogs_config).unwrap();
        assert!(text.contains("4317"), "{text}");
    }

    /// fails for the same reason as the case above, one step further along:
    /// a built-in dog an operator has enabled but that is not up right now
    /// -- crashed, stopped, or simply not spawned on this boot -- is still
    /// a dog whose section this shepherd holds. The harness starts nothing,
    /// so `bark` is known and absent from the flock.
    #[tokio::test(start_paused = true)]
    async fn a_dog_that_is_enabled_but_not_running_can_still_be_configured() {
        let h = harness(vec![]);
        assert!(h.ctx.supervisor.list().await.is_empty());

        let reply = reply_of(
            dispatch(
                envelope(
                    1,
                    Request::SetDogConfig {
                        name: "bark".to_string(),
                        toml: "poll = \"30s\"\n".to_string().into(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        assert_eq!(
            reply.result.unwrap(),
            Response::DogConfigSet {
                name: "bark".to_string()
            }
        );
    }

    /// fails if the boot-time list is the ONLY thing consulted. A dog
    /// adopted and enabled since this shepherd started is not in the list
    /// the CLI handed over at boot -- that list is refreshed by a `shep
    /// daemon reload` and by nothing else -- so a guard that stopped there
    /// would refuse a dog that is up and answering right now.
    #[tokio::test(start_paused = true)]
    async fn a_dog_adopted_since_boot_is_reached_through_the_running_flock() {
        let mut h = harness(vec![ProcScript::never_exits()]);
        h.ctx.known_dogs = KnownDogs::new(BTreeSet::new());
        enable_dog(&h.ctx, 1, "bark").await;

        let reply = reply_of(
            dispatch(
                envelope(
                    2,
                    Request::SetDogConfig {
                        name: "bark".to_string(),
                        toml: "poll = \"30s\"\n".to_string().into(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        assert_eq!(
            reply.result.unwrap(),
            Response::DogConfigSet {
                name: "bark".to_string()
            }
        );
    }

    /// fails if the known set goes stale the moment a dog stops running.
    ///
    /// The boot-time list is a snapshot, so a dog adopted against a running
    /// shepherd is in neither half of the old guard once it is off the
    /// flock, and both of these were refused:
    ///
    /// - adopt, disable, configure, enable
    /// - adopt, the dog crashes for want of config, configure
    ///
    /// The second is bark's own situation on a fresh install, and
    /// `docs/dogs.md` promises configure-then-enable works.
    #[tokio::test(start_paused = true)]
    async fn a_dog_adopted_since_boot_stays_configurable_once_it_is_disabled() {
        let mut h = harness(vec![ProcScript::never_exits()]);
        h.ctx.known_dogs = KnownDogs::new(BTreeSet::new());
        enable_dog(&h.ctx, 1, "bark").await;
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
        assert!(disabled.result.is_ok(), "{:?}", disabled.result);
        assert!(
            !h.ctx
                .supervisor
                .list()
                .await
                .iter()
                .any(|info| info.name == "bark"),
            "the flock must not hold it, or the widening answers and the test means nothing"
        );

        let reply = reply_of(
            dispatch(
                envelope(
                    3,
                    Request::SetDogConfig {
                        name: "bark".to_string(),
                        toml: "poll = \"30s\"\n".to_string().into(),
                    },
                ),
                &h.ctx,
            )
            .await,
        );

        assert_eq!(
            reply.result.unwrap(),
            Response::DogConfigSet {
                name: "bark".to_string()
            }
        );
    }

    /// fails if the guard is the one every other config door carries. This
    /// is the inverse: `dogs.toml` holds dogs' sections and nothing else,
    /// so a SHEEP's name is what has to be refused here, and a name nobody
    /// registered with it. Refused before the file is opened, so a
    /// mistyped name cannot leave a stray table behind for a dog that will
    /// never exist.
    #[tokio::test(start_paused = true)]
    async fn setting_a_dogs_config_over_a_sheeps_name_is_refused_and_writes_nothing() {
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
        assert!(started.result.is_ok(), "{:?}", started.result);
        std::fs::write(&h.ctx.dogs_config, "[bark]\npoll = \"60s\"\n").unwrap();

        for (id, name) in [(2, "web"), (3, "ghost")] {
            let reply = reply_of(
                dispatch(
                    envelope(
                        id,
                        Request::SetDogConfig {
                            name: name.to_string(),
                            toml: "poll = \"30s\"\n".to_string().into(),
                        },
                    ),
                    &h.ctx,
                )
                .await,
            );
            let Err(err) = reply.result else {
                panic!("{name} is not a dog and must be refused")
            };
            assert_eq!(err.code, RpcErrorCode::NotFound, "{err:?}");
            assert!(err.message.contains(name), "{err:?}");
        }

        assert_eq!(
            std::fs::read_to_string(&h.ctx.dogs_config).unwrap(),
            "[bark]\npoll = \"60s\"\n"
        );
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
