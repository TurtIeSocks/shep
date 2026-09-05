//! Lifecycle extras: what is armed when a sheep goes online, and what stops
//! when it goes terminal.
//!
//! Four subsystems run as free tasks beside the supervisor actor: the cron
//! worker, the memory-limit enforcer, the liveness prober and the filesystem
//! watch. [`ExtrasRegistry`] keys cron and watch per name, since both restart a
//! whole name-group, and the enforcer and the liveness loop per id.
//!
//! No trigger filters by status, so disarming is the whole of what keeps a
//! stopped sheep down. A group is torn down only when its last member leaves.

use core::fmt;
use core::time::Duration;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use shep_core::config::{AppConfig, CronSchedule, ProbeTarget};
use shep_core::values::UpDuration;

use crate::cron::{Clock, SystemClock, spawn_cron_worker};
use crate::entry::ProcessEntry;
use crate::limits::sample::{MemorySampler, SysinfoSampler};
use crate::limits::stats::StatsState;
use crate::limits::{LimitBreach, LimitEnforcer, PollingEnforcer};
use crate::probes::{LivenessFailure, Prober, spawn_liveness_task};
use crate::supervisor::SupervisorHandle;
use crate::watch::{
    DEFAULT_WATCH_DELAY, MIN_WATCH_DELAY, WatchFilter, own_log_ignores, spawn_watch_group,
};

/// A [`LivenessFailure`] paired with the epoch its probe was armed under.
///
/// `InstanceExtras::disarm` aborts without awaiting, so a probe already inside
/// `failures.send(..).await` can deliver after its replacement is running
/// against the same pid and status. The epoch tells the stale failure apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LivenessReport {
    /// The sheep's id.
    pub id: u32,
    /// The pid this loop was armed against.
    pub pid: u32,
    /// The epoch the reporting probe was armed under.
    pub epoch: u64,
}

/// Where the lifecycle extras send the two out-of-band failure reports.
///
/// The matching receivers belong to the reporting task, never to the actor.
#[derive(Debug, Clone)]
pub struct ExtrasReports {
    /// Memory-limit breaches, from the enforcer.
    pub breaches: mpsc::Sender<LimitBreach>,
    /// Sheep whose liveness probe hit `failure_threshold`.
    pub liveness: mpsc::Sender<LivenessReport>,
}

/// The four lifecycle extras, the seams they run on, and where their two
/// failure reports go.
///
/// Constructed once at boot and handed to the supervisor. Every seam is a
/// trait object so the engine's type does not grow a parameter per subsystem.
pub struct Extras {
    /// Wall clock the cron workers read.
    pub clock: Arc<dyn Clock>,
    /// Memory-limit mechanism.
    ///
    /// Shared so [`ExtrasRegistry::disarm`], whose signature takes no
    /// [`Extras`], can reach it too.
    pub enforcer: Arc<dyn LimitEnforcer>,
    /// Longest a cron worker parks before re-reading the clock, from
    /// `[daemon] max_cron_sleep`. Already defaulted: a value, not an option.
    pub max_cron_sleep: Duration,
    /// Cloned once per arming. The enforcer already holds its own breach
    /// sender; the liveness loops are free tasks and do not.
    pub reports: ExtrasReports,
    /// Live resource readings, shared with the RPC layer so a listing can take
    /// one on demand.
    pub stats: Arc<StatsState>,
}

impl Extras {
    /// The production wiring: system clock and polling enforcer over sysinfo.
    ///
    /// No prober: one is scoped to a single sheep's assembled environment.
    ///
    /// Must be called from within a Tokio runtime context: constructing the
    /// polling enforcer starts its sampling task immediately.
    #[must_use]
    pub fn real(reports: ExtrasReports, max_cron_sleep: Duration) -> Self {
        // One sampler behind both consumers: sampling and enforcement read the
        // same process table on the same tick, so a second `SysinfoSampler`
        // would mean a second syscall walk.
        let sampler: Arc<dyn MemorySampler> = Arc::new(SysinfoSampler::new());
        let stats = Arc::new(StatsState::new(Arc::clone(&sampler)));
        let enforcer =
            PollingEnforcer::start(sampler, reports.breaches.clone(), Arc::clone(&stats));
        Self {
            clock: Arc::new(SystemClock),
            enforcer: Arc::new(enforcer),
            max_cron_sleep,
            reports,
            stats,
        }
    }
}

impl fmt::Debug for Extras {
    // Roles, not values: neither seam is Debug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extras")
            .field("clock", &"<dyn Clock>")
            .field("enforcer", &"<dyn LimitEnforcer>")
            .field("max_cron_sleep", &self.max_cron_sleep)
            .finish_non_exhaustive()
    }
}

/// Restarts each sheep reported over `breaches` or `liveness`.
///
/// Ends when both senders have dropped. Owns both receivers: the actor must
/// never block on anything a subsystem controls.
///
/// Restarts go through [`SupervisorHandle::extra_restart`], never `restart`: a
/// report queued before `shep stop` is delivered after the sheep is `Stopped`,
/// and `restart` would resurrect it.
///
/// Must be called from within a Tokio runtime context: it spawns the reporting
/// task immediately.
pub fn spawn_extras_reporter(
    mut breaches: mpsc::Receiver<LimitBreach>,
    mut liveness: mpsc::Receiver<LivenessReport>,
    supervisor: SupervisorHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // A closed `mpsc::Receiver` resolves to `None` on every poll, so a
        // branch left in consideration would busy-spin the loop.
        let mut breaches_open = true;
        let mut liveness_open = true;
        while breaches_open || liveness_open {
            tokio::select! {
                maybe_breach = breaches.recv(), if breaches_open => match maybe_breach {
                    Some(breach) => {
                        tracing::warn!(
                            id = breach.id,
                            pid = breach.root_pid,
                            observed = %breach.observed,
                            limit = %breach.limit,
                            "process tree exceeded its max_memory; restarting"
                        );
                        // No epoch: a breach has no probe task to abort
                        // mid-`send`. It carries the observed size, and the
                        // actor re-checks it against the ceiling in force.
                        supervisor
                            .extra_restart(
                                breach.id,
                                breach.root_pid,
                                None,
                                Some(breach.observed),
                            )
                            .await;
                    }
                    None => breaches_open = false,
                },
                maybe_failure = liveness.recv(), if liveness_open => match maybe_failure {
                    Some(report) => {
                        tracing::warn!(
                            id = report.id,
                            pid = report.pid,
                            "liveness probe hit its failure_threshold; restarting"
                        );
                        supervisor
                            .extra_restart(report.id, report.pid, Some(report.epoch), None)
                            .await;
                    }
                    None => liveness_open = false,
                },
            }
        }
    })
}

/// Per-sheep and per-group task handles, armed on `online` and aborted on the
/// way out.
#[derive(Debug, Default)]
pub struct ExtrasRegistry {
    /// One name-group's per-name tasks. Keyed on the configuration, not on
    /// what an arming managed to build.
    groups: HashMap<String, NameExtras>,
    /// One instance's per-pid extras, keyed by sheep id. Present only while
    /// at least one of them is armed.
    instances: HashMap<u32, InstanceExtras>,
    /// The epoch each id's liveness probe is currently armed under, bumped by
    /// [`Self::arm`] whether or not that id configures a `liveness_probe`, so
    /// an app that adds one later inherits no stale count.
    ///
    /// Separate from the supervisor's `SheepSlot::epoch`, which moves on a
    /// respawn: this one answers whether a probe was replaced without the
    /// process underneath it changing.
    liveness_epochs: HashMap<u32, u64>,
}

/// One name-group's per-name tasks, plus the armed instances keeping them
/// alive.
///
/// Either task may be `None` while the group still exists: an app whose watch
/// could not be registered is a member of its group all the same.
#[derive(Debug, Default)]
struct NameExtras {
    /// The group's cron worker, when the app configures `cron_restart`.
    cron: Option<JoinHandle<()>>,
    /// The group's filesystem watch, when the app configures `watch`.
    watch: Option<JoinHandle<()>>,
    /// Ids of this name's instances that currently have anything armed. The
    /// last one leaving is what tears the two tasks above down.
    members: HashSet<u32>,
}

impl NameExtras {
    /// Aborts both per-name tasks. Takes `self`: a group is torn down once.
    fn abort(self) {
        if let Some(cron) = self.cron {
            cron.abort();
        }
        if let Some(watch) = self.watch {
            watch.abort();
        }
    }
}

/// One instance's per-pid extras.
struct InstanceExtras {
    /// Where this id's sampling was started. Not an `Option`: every sheep
    /// with a pid is sampled.
    stats: Arc<StatsState>,
    /// The enforcer this id's memory limit was armed against.
    limit: Option<Arc<dyn LimitEnforcer>>,
    /// The liveness loop, when the app configures `liveness_probe`.
    liveness: Option<JoinHandle<()>>,
}

impl InstanceExtras {
    /// Sampling armed and nothing else: what an app configuring neither
    /// `max_memory` nor `liveness_probe` gets.
    fn watched_only(stats: Arc<StatsState>) -> Self {
        Self {
            stats,
            limit: None,
            liveness: None,
        }
    }

    /// Undoes this instance's arming: sampling, the memory limit against `id`,
    /// and the liveness loop.
    fn disarm(self, id: u32) {
        self.stats.unwatch(id);
        if let Some(enforcer) = self.limit {
            enforcer.disarm(id);
        }
        if let Some(liveness) = self.liveness {
            liveness.abort();
        }
    }
}

impl fmt::Debug for InstanceExtras {
    // `Arc<dyn LimitEnforcer>` is not Debug, and the useful fact is that an
    // arming exists. `stats` is armed for every instance, hence the `..`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstanceExtras")
            .field("limit_armed", &self.limit.is_some())
            .field("liveness", &self.liveness)
            .finish_non_exhaustive()
    }
}

impl ExtrasRegistry {
    /// Arms everything an entry's configuration asks for.
    ///
    /// `prober` is scoped to this instance's assembled `SpawnSpec` and is read
    /// only by the liveness loop.
    ///
    /// Idempotent per id: arming an already-armed id disarms that id's own
    /// per-pid extras first, which is what a respawn needs. A live name-group
    /// task is left alone: it is keyed on the name and outlives any one process.
    /// Rebuilding it re-registers the OS watcher, which can fail and would
    /// silently cost the app its watch.
    pub fn arm(
        &mut self,
        entry: &ProcessEntry,
        prober: Arc<dyn Prober>,
        extras: &Extras,
        supervisor: &SupervisorHandle,
    ) {
        let config = entry.spec.config();
        let id = entry.id;

        self.disarm_instance(id);
        // Bumped ahead of `arm_instance`, which reads it only for a probe it
        // is about to spawn.
        let liveness_epoch = self.liveness_epochs.entry(id).or_insert(0);
        *liveness_epoch += 1;
        let liveness_epoch = *liveness_epoch;
        if let Some(instance) = arm_instance(entry, prober, extras, liveness_epoch) {
            self.instances.insert(id, instance);
        }

        // Membership is decided by the configuration, never by whether this
        // arming built a task: a transient `arm_watch` failure would otherwise
        // leave a still-online instance out of its own group.
        if config.cron_restart.is_none() && !config.watch {
            return;
        }
        let group = self.groups.entry(config.name.clone()).or_default();
        // A second instance of a name joins the group rather than arming a
        // second worker: both triggers already reach every instance of the name.
        group.members.insert(id);
        // A task can end on its own: a cron worker returns on a pattern with no
        // further occurrence, and the watch loop returns when its `WatchSource`
        // dies. Presence in the map is therefore not the test.
        if group.cron.as_ref().is_none_or(JoinHandle::is_finished) {
            group.cron = arm_cron(config, extras, supervisor);
        }
        // An app whose watch can never arm pays a fresh `canonicalize`, globset
        // compile and `warn!` on every re-arm, bounded by `max_restarts`. That is
        // the price of retrying the transient failures this rebuild exists for.
        if group.watch.as_ref().is_none_or(JoinHandle::is_finished) {
            group.watch = arm_watch(entry, supervisor);
        }
    }

    /// Aborts everything armed for `id`, and both of the name-group's per-name
    /// tasks when this was the last armed instance of the name.
    ///
    /// No trigger filters by status, so a sheep stays down because nothing is
    /// left armed for it. Aborting the watch-group handle stops the OS watch:
    /// the debouncer guard rides inside the aborted future.
    pub fn disarm(&mut self, id: u32, name: &str) {
        self.disarm_instance(id);
        // Not inside `disarm_instance`: `Self::arm` calls that first and would
        // reset the counter it is about to bump.
        self.liveness_epochs.remove(&id);

        let Some(group) = self.groups.get_mut(name) else {
            return;
        };
        // An id that was never a member leaves the group untouched; a group
        // with instances left standing keeps its tasks.
        if !group.members.remove(&id) || !group.members.is_empty() {
            return;
        }
        if let Some(group) = self.groups.remove(name) {
            group.abort();
        }
    }

    /// Rebuilds everything armed for `name`, replacing live tasks rather than
    /// keeping them.
    ///
    /// The group-scoped fields (`watch`, `ignore_watch`, `watch_delay`,
    /// `watch_options`, `cron_restart`, `cron_timezone`) are read when the task
    /// is built, so a task [`Self::arm`] left alive would keep the old values.
    /// The rebuild costs a real gap in the OS watch with no rescan.
    ///
    /// `entries` is what the caller wants armed: a stopped instance passed here
    /// joins a group whose next cron occurrence or watch event restarts it, and
    /// an empty slice aborts the group and rebuilds nothing. `prober` runs once
    /// per entry, since `assemble` bakes `SHEP_INSTANCE` into its environment.
    pub fn rearm_name(
        &mut self,
        name: &str,
        entries: &[&ProcessEntry],
        prober: impl Fn(&ProcessEntry) -> Arc<dyn Prober>,
        extras: &Extras,
        supervisor: &SupervisorHandle,
    ) {
        // Removing the entry rather than mutating it makes the rebuild take
        // `arm`'s own "no task yet" path.
        if let Some(group) = self.groups.remove(name) {
            group.abort();
        }
        for entry in entries {
            self.arm(entry, prober(entry), extras, supervisor);
        }
    }

    /// Undoes one instance's per-pid arming. A no-op for an id with none.
    fn disarm_instance(&mut self, id: u32) {
        if let Some(instance) = self.instances.remove(&id) {
            instance.disarm(id);
        }
    }

    /// The epoch `id`'s liveness probe is currently armed under, or `0` for an
    /// id that has never been armed. `Actor::handle_extra_restart` drops a
    /// [`LivenessReport`] whose epoch does not match.
    pub(crate) fn liveness_epoch(&self, id: u32) -> u64 {
        self.liveness_epochs.get(&id).copied().unwrap_or(0)
    }

    /// The ids in `name`'s armed group, or `None` when nothing of that name is
    /// armed at all.
    #[cfg(test)]
    pub(crate) fn group_members(&self, name: &str) -> Option<Vec<u32>> {
        self.groups.get(name).map(|group| {
            let mut members: Vec<u32> = group.members.iter().copied().collect();
            members.sort_unstable();
            members
        })
    }
}

impl Drop for ExtrasRegistry {
    // Here rather than a disarm loop in `begin_shutdown`, which a
    // `WaitingRestart` sheep never reaches and a panicking actor never runs. A
    // dropped `JoinHandle` detaches its task rather than cancelling it, and while
    // any task lives it holds a report sender that keeps the reporter alive.
    fn drop(&mut self) {
        for (id, instance) in self.instances.drain() {
            instance.disarm(id);
        }
        for (_name, group) in self.groups.drain() {
            group.abort();
        }
    }
}

/// Arms the per-pid extras: sampling always, the memory limit and the liveness
/// loop where the app configures them. `None` when the entry has no pid.
fn arm_instance(
    entry: &ProcessEntry,
    prober: Arc<dyn Prober>,
    extras: &Extras,
    liveness_epoch: u64,
) -> Option<InstanceExtras> {
    let config = entry.spec.config();
    let wants_anything = config.max_memory.is_some() || config.liveness_probe.is_some();
    let Some(pid) = entry.pid else {
        if wants_anything {
            // Unreachable from the transition this is called at: a sheep is
            // Online only with a live pid. Both extras are armed against a pid.
            tracing::warn!(
                id = entry.id,
                "arming a sheep with no pid; its memory limit and liveness probe stay disarmed"
            );
        }
        return None;
    };

    // Unconditional, unlike the two below: a listing reports CPU and memory for
    // every sheep.
    extras.stats.watch(entry.id, pid);
    let mut instance = InstanceExtras::watched_only(Arc::clone(&extras.stats));
    if let Some(limit) = config.max_memory {
        extras.enforcer.arm(entry.id, pid, limit);
        instance.limit = Some(Arc::clone(&extras.enforcer));
    }
    if let Some(probe) = config.liveness_probe.as_ref() {
        match ProbeTarget::parse(probe) {
            Ok(target) => {
                // `probes` knows only `LivenessFailure`, so the probe reports
                // into a private channel and this relay tags its one failure
                // with the epoch it was spawned under. Captured here rather than
                // read at delivery, when a re-arm may have moved it on.
                let (raw_tx, mut raw_rx) = mpsc::channel::<LivenessFailure>(1);
                let reports_liveness = extras.reports.liveness.clone();
                tokio::spawn(async move {
                    if let Some(failure) = raw_rx.recv().await {
                        let _ = reports_liveness
                            .send(LivenessReport {
                                id: failure.id,
                                pid: failure.pid,
                                epoch: liveness_epoch,
                            })
                            .await;
                    }
                });
                instance.liveness = Some(spawn_liveness_task(
                    entry.id,
                    pid,
                    probe.clone(),
                    target,
                    prober,
                    raw_tx,
                ));
            }
            Err(err) => {
                // `normalize` already parses both probe targets, so a config
                // that reached the daemon cannot land here. Swallowed rather
                // than `expect`-ed: a future path skipping normalization costs
                // one app its probe rather than the daemon.
                tracing::warn!(
                    id = entry.id,
                    name = config.name.as_str(),
                    %err,
                    "liveness_probe target could not be parsed; arming no liveness probe"
                );
            }
        }
    }
    Some(instance)
}

/// Spawns the name-group's cron worker, or `None` when the app configures no
/// `cron_restart` or names a pattern that will not parse.
///
/// An unparseable pattern costs the app its schedule, and the `warn!` is the
/// only record: the app comes up `online` either way.
fn arm_cron(
    config: &AppConfig,
    extras: &Extras,
    supervisor: &SupervisorHandle,
) -> Option<JoinHandle<()>> {
    let pattern = config.cron_restart.as_ref()?;
    let schedule = match CronSchedule::parse(pattern, config.cron_timezone.as_deref()) {
        Ok(schedule) => schedule,
        Err(err) => {
            tracing::warn!(
                name = config.name.as_str(),
                pattern = pattern.as_str(),
                %err,
                "cron_restart pattern could not be parsed; arming no cron worker"
            );
            return None;
        }
    };
    Some(spawn_cron_worker(
        config.name.clone(),
        schedule,
        Arc::clone(&extras.clock),
        supervisor.clone(),
        extras.max_cron_sleep,
    ))
}

/// Spawns the name-group's filesystem watch, or `None` when the app does not
/// ask to be watched, or when its root or its globs will not resolve.
///
/// Every failure here arms no watch rather than propagating: a watch root that
/// will not resolve must not take down the same app's cron worker, enforcer and
/// probe. Each writes a `warn!`, and that record is the entire signal.
///
/// Takes the whole [`ProcessEntry`] because the assembled `out_file`/`err_file`
/// are what `own_log_ignores` needs.
fn arm_watch(entry: &ProcessEntry, supervisor: &SupervisorHandle) -> Option<JoinHandle<()>> {
    let config = entry.spec.config();
    if !config.watch {
        return None;
    }
    let Some(cwd) = config.cwd.as_deref() else {
        // `normalize` rejects `watch = true` with no `cwd`. The daemon's own
        // working directory is no fallback: a systemd unit would watch `/`.
        tracing::warn!(
            name = config.name.as_str(),
            "watch is on but the app names no cwd; arming no watch"
        );
        return None;
    };
    // Canonicalized, not merely absolute: the group loop strips this prefix off
    // the absolute paths notify delivers, and on macOS a directory under
    // `/var/...` arrives from FSEvents as `/private/var/...`.
    let root = match std::fs::canonicalize(cwd) {
        Ok(root) => root,
        Err(err) => {
            tracing::warn!(
                name = config.name.as_str(),
                path = cwd,
                %err,
                "watch root could not be resolved; arming no watch"
            );
            return None;
        }
    };
    // The app's own ignores plus this sheep's log files, for whichever of them
    // the app pointed back inside the watched tree.
    let mut ignores = config.ignore_watch.clone();
    ignores.extend(own_log_ignores(
        &root,
        [entry.out_file.as_path(), entry.err_file.as_path()],
    ));
    let filter = match WatchFilter::new(&config.watch_options, &ignores) {
        Ok(filter) => filter,
        Err(err) => {
            // `normalize` compiles every `watch_options` and `ignore_watch`
            // pattern, so a config that reached the daemon cannot land here.
            tracing::warn!(
                name = config.name.as_str(),
                %err,
                "watch globs could not be compiled; arming no watch"
            );
            return None;
        }
    };
    match spawn_watch_group(
        config.name.clone(),
        root,
        filter,
        watch_delay_for(config),
        supervisor.clone(),
    ) {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!(
                name = config.name.as_str(),
                %err,
                "the OS watch could not be started; arming no watch"
            );
            None
        }
    }
}

/// The debounce window an app's watch is armed with: its own `watch_delay` when
/// it set one, [`DEFAULT_WATCH_DELAY`] otherwise, floored at [`MIN_WATCH_DELAY`].
///
/// The floor is a last line of defence: `normalize` already refuses
/// `watch_delay = "0"`.
fn watch_delay_for(config: &AppConfig) -> Duration {
    config
        .watch_delay
        .map(UpDuration::as_duration)
        .unwrap_or(DEFAULT_WATCH_DELAY)
        .max(MIN_WATCH_DELAY)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::{DateTime, Utc};
    use tokio::sync::broadcast;

    use super::*;
    use crate::bus::SharedEvent;
    use crate::cron::DEFAULT_MAX_CRON_SLEEP;
    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::limits::PollingEnforcer;
    use crate::limits::sample::ProcessRss;
    use crate::probes::ProbeFailure;
    use crate::supervisor::spawn_supervisor;
    use crate::testing::{
        ArmCall, Harness, RecordingEnforcer, ScriptedProber, ScriptedSampler, TestClock, app_with,
        armed_entry, capture_logs, harness, harness_with_extras, idle_stats, probe_config,
        test_paths, touch,
    };
    use crate::watch::real_time;
    use shep_core::config::{ProbeConfig, ProbeKind};
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
    use shep_core::selector::ProcessSelector;
    use shep_core::status::ProcStatus;
    use shep_core::values::{MemSize, UpDuration};

    /// Generous bound on how long a test may wait on the paused tokio clock.
    /// Costs no real time: the runtime auto-advances here only if nothing else
    /// becomes ready first.
    const EVENT_WAIT: Duration = Duration::from_secs(120);

    /// Spans a whole hourly cron occurrence and then some, so a negative
    /// assertion crosses the occurrence and makes its claim in one call.
    const PAST_THE_NEXT_OCCURRENCE: Duration = Duration::from_secs(3_700);

    /// How long a real-clock test waits for a liveness report that should
    /// arrive. Generous enough that a loaded runner cannot flake it.
    const LIVENESS_DEADLINE: Duration = Duration::from_secs(10);

    /// The shortest interval `spawn_liveness_task` honours. A literal, because
    /// `probes`' own floor is private to that module.
    const PROBE_INTERVAL: UpDuration = UpDuration::from_millis(1_000);

    fn dt(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid RFC3339 timestamp")
    }

    /// The registry-tier fixture: paused clock, recording enforcer, and both
    /// report receivers held by the test rather than by a reporter.
    struct Rig {
        extras: Extras,
        enforcer: Arc<RecordingEnforcer>,
        clock: Arc<TestClock>,
        liveness: mpsc::Receiver<LivenessReport>,
        _breaches: mpsc::Receiver<LimitBreach>,
    }

    fn rig(max_cron_sleep: Duration) -> Rig {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let enforcer = Arc::new(RecordingEnforcer::default());
        let (breach_tx, breaches) = mpsc::channel(8);
        let (live_tx, liveness) = mpsc::channel(8);
        Rig {
            extras: Extras {
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
                enforcer: Arc::clone(&enforcer) as Arc<dyn LimitEnforcer>,
                max_cron_sleep,
                reports: ExtrasReports {
                    breaches: breach_tx,
                    liveness: live_tx,
                },
                stats: idle_stats(),
            },
            enforcer,
            clock,
            liveness,
            _breaches: breaches,
        }
    }

    /// One supervisor engine over a scripted runner with enough `never_exits`
    /// procs that no negative assertion passes because the script ran out: an
    /// exhausted script makes the supervisor emit `Errored`.
    fn spawn_test_fixture() -> (
        SupervisorHandle,
        broadcast::Receiver<SharedEvent>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (events, rx) = crate::bus::test_bus(64);
        let runner = ScriptedRunner::new(vec![ProcScript::never_exits(); 12]);
        let handle = spawn_supervisor(runner, test_paths(&dir), events);
        (handle, rx, dir)
    }

    /// A prober that never fails: the neutral value for a case that arms no
    /// liveness probe but must still hand `arm` one.
    fn idle_prober() -> Arc<dyn Prober> {
        Arc::new(ScriptedProber::new(vec![]))
    }

    /// A prober that fails every probe, so a case asserting silence against one
    /// is asserting the loop is gone.
    fn failing_prober() -> Arc<dyn Prober> {
        Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]))
    }

    /// Yielding rather than advancing: an `advance` would resolve other timers.
    async fn settle_finished(task: &JoinHandle<()>) {
        for _ in 0..100 {
            if task.is_finished() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the task never finished");
    }

    async fn expect_restart(
        rx: &mut broadcast::Receiver<SharedEvent>,
        name: &str,
        window: Duration,
    ) -> ProcessInfo {
        expect_restart_event(rx, name, window).await.0
    }

    /// [`expect_restart`], plus the `manually` flag the bus put on that
    /// restart.
    async fn expect_restart_event(
        rx: &mut broadcast::Receiver<SharedEvent>,
        name: &str,
        window: Duration,
    ) -> (ProcessInfo, bool) {
        let restart = async {
            loop {
                match rx.recv().await.map(|event| event.to_event()) {
                    Ok(BusEvent::Process {
                        event: ProcessEventKind::Restart,
                        info,
                        manually,
                        ..
                    }) if info.name == name => return (info, manually),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(err) => panic!("event stream closed before a restart of {name}: {err}"),
                }
            }
        };
        match tokio::time::timeout(window, restart).await {
            Ok(observed) => observed,
            Err(_) => panic!("timed out waiting for a restart of {name}"),
        }
    }

    /// A bounded `timeout` + `recv`, never a bare `try_recv`: the window carries
    /// the paused clock past the occurrence the abort was supposed to stop.
    async fn assert_no_restart_within(
        rx: &mut broadcast::Receiver<SharedEvent>,
        name: &str,
        window: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv())
                .await
                .map(|received| received.map(|event| event.to_event()))
            {
                Err(_) => return, // window elapsed with nothing matching
                Ok(Ok(BusEvent::Process {
                    event: ProcessEventKind::Restart,
                    info,
                    ..
                })) if info.name == name => {
                    panic!(
                        "unexpected restart of {name} observed (restarts={})",
                        info.restarts
                    );
                }
                Ok(Ok(_)) => continue,
                // A negative assertion cannot skip events: a dropped one may be
                // the `Restart` this forbids. `expect_restart` may skip them,
                // since a lag costs it only a timeout.
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    panic!(
                        "event stream lagged by {skipped} while checking for no restart of \
                         {name}: a skipped event may have been the restart this forbids"
                    )
                }
                Ok(Err(err)) => {
                    panic!("event channel closed while checking for no restart of {name}: {err}")
                }
            }
        }
    }

    async fn expect_liveness(
        rx: &mut mpsc::Receiver<LivenessReport>,
        window: Duration,
    ) -> LivenessReport {
        match tokio::time::timeout(window, rx.recv()).await {
            Ok(Some(failure)) => failure,
            Ok(None) => panic!("the liveness channel closed before a failure arrived"),
            Err(_) => panic!("timed out waiting for a liveness failure"),
        }
    }

    async fn assert_no_liveness_within(rx: &mut mpsc::Receiver<LivenessReport>, window: Duration) {
        match tokio::time::timeout(window, rx.recv()).await {
            Err(_) => {} // window elapsed with nothing arriving
            Ok(Some(failure)) => panic!("unexpected liveness failure observed: {failure:?}"),
            Ok(None) => panic!("the liveness channel disconnected while checking for silence"),
        }
    }

    /// Crosses one hourly occurrence in steps far finer than either sleep cap,
    /// so the worker's own cadence decides how often it wakes.
    async fn cross_one_hour() {
        for _ in 0..120 {
            tokio::time::advance(Duration::from_secs(30)).await;
        }
    }

    // ------------------------------------------------------------------
    // The registry: what gets armed, and what stops.
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn extras_debug_names_the_seams_by_role_and_the_sleep_bound_by_value() {
        let rig = rig(Duration::from_secs(300));
        assert_eq!(
            format!("{:?}", rig.extras),
            r#"Extras { clock: "<dyn Clock>", enforcer: "<dyn LimitEnforcer>", max_cron_sleep: 300s, .. }"#
        );
    }

    #[tokio::test(start_paused = true)]
    async fn instance_extras_debug_reports_an_arming_without_naming_the_enforcer() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let capped = app_with("web", |app| app.max_memory = Some(MemSize::from_bytes(500)));
        let uncapped = app_with("api", |app| app.max_memory = None);

        registry.arm(
            &armed_entry(0, 0, 1000, capped, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        registry.arm(
            &armed_entry(1, 0, 1001, uncapped, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );

        assert_eq!(
            format!("{:?}", registry.instances[&0]),
            "InstanceExtras { limit_armed: true, liveness: None, .. }"
        );
        assert_eq!(
            format!("{:?}", registry.instances[&1]),
            "InstanceExtras { limit_armed: false, liveness: None, .. }"
        );
    }

    // Real sysinfo over this very test process, whose RSS is comfortably over
    // one byte, on the paused clock the polling loop sleeps on.
    #[tokio::test(start_paused = true)]
    async fn real_extras_wire_the_enforcer_to_the_reports_channel() {
        let (breach_tx, mut breaches) = mpsc::channel(4);
        let (live_tx, _liveness) = mpsc::channel(4);
        let extras = Extras::real(
            ExtrasReports {
                breaches: breach_tx,
                liveness: live_tx,
            },
            Duration::from_secs(300),
        );
        assert_eq!(
            format!("{extras:?}"),
            r#"Extras { clock: "<dyn Clock>", enforcer: "<dyn LimitEnforcer>", max_cron_sleep: 300s, .. }"#,
            "`real` must carry the sleep bound it was handed, not re-derive one"
        );

        let limit = MemSize::from_bytes(1);
        extras.enforcer.arm(3, std::process::id(), limit);
        let breach = match tokio::time::timeout(EVENT_WAIT, breaches.recv()).await {
            Ok(Some(breach)) => breach,
            Ok(None) => panic!("the breach channel closed before a breach arrived"),
            Err(_) => panic!("timed out waiting for a breach from the real enforcer"),
        };
        assert_eq!(breach.id, 3);
        assert_eq!(breach.root_pid, std::process::id());
        assert_eq!(breach.limit, limit);
    }

    // The cwd is a real directory, so this app is one a watcher could really
    // have been registered on.
    #[tokio::test(start_paused = true)]
    async fn an_app_configuring_no_extras_arms_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();

        let app = app_with("web", |app| {
            app.cwd = Some(root.path().display().to_string());
        });
        let entry = armed_entry(0, 0, 1000, app, &paths);
        registry.arm(&entry, idle_prober(), &rig.extras, &handle);

        assert!(
            registry.groups.is_empty(),
            "an app with neither cron_restart nor watch must arm no name-group tasks"
        );
        // Sampling is the exception: it is armed for every sheep with a pid.
        assert_eq!(
            format!("{:?}", registry.instances[&0]),
            "InstanceExtras { limit_armed: false, liveness: None, .. }",
            "an app with neither max_memory nor liveness_probe must arm nothing beyond sampling"
        );
        assert!(
            rig.enforcer.arms().is_empty(),
            "an app with no max_memory must not reach the enforcer at all"
        );
    }

    // The disarm at the end is the other half: a watch never dropped samples a
    // dead pid forever, and hands its CPU baseline to whatever gets that number.
    #[tokio::test(start_paused = true)]
    async fn every_sheep_with_a_pid_is_watched_even_with_no_limit_and_no_probe() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.max_memory = None;
            app.liveness_probe = None;
        });

        registry.arm(
            &armed_entry(7, 0, 4242, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );

        assert_eq!(
            rig.extras.stats.watched_for_test(),
            vec![(7, 4242)],
            "an app with neither max_memory nor a liveness_probe is the ORDINARY case, and a \
             listing reporting `-` for every one of them is what this split exists to fix"
        );
        assert!(
            rig.enforcer.arms().is_empty(),
            "sampling is not enforcement: an app with no ceiling must still not be armed"
        );

        registry.disarm(7, "web");
        assert!(rig.extras.stats.watched_for_test().is_empty());
    }

    // A cron-restarting app is a member of its name group whatever it thinks of
    // watching, so it is the one shape that reaches `arm_watch` unwatched. Its
    // cwd is real, so a watcher really would register.
    #[tokio::test(start_paused = true)]
    async fn a_cron_only_app_with_a_real_cwd_arms_no_watch() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string());
            app.cwd = Some(root.path().display().to_string());
        });

        registry.arm(
            &armed_entry(0, 0, 1000, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );

        let group = &registry.groups["web"];
        assert!(group.cron.is_some(), "the cron worker is what armed here");
        assert!(
            group.watch.is_none(),
            "an app that did not ask to be watched must get no watcher on its cwd"
        );
    }

    // `Etc/GMT+5` is UTC minus five, POSIX inverting the sign, so 05:00 local is
    // 10:00Z. Read as UTC it fires five hours inside the silent window below.
    #[tokio::test(start_paused = true)]
    async fn a_cron_pattern_is_resolved_in_the_apps_own_timezone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 5 * * *".to_string());
            app.cron_timezone = Some("Etc/GMT+5".to_string());
        });
        handle.start(vec![app.clone()]).await.unwrap();

        registry.arm(
            &armed_entry(0, 0, 1000, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        tokio::task::yield_now().await;

        // Six hours from midnight UTC: past a UTC reading of the pattern,
        // still four hours short of the app's own.
        assert_no_restart_within(&mut rx, "web", Duration::from_secs(6 * 3_600)).await;
        expect_restart(&mut rx, "web", Duration::from_secs(6 * 3_600)).await;
    }

    // An unresolvable cwd is the one config-shaped watch failure that survives
    // normalization: `normalize` compiles both glob lists.
    #[tokio::test(start_paused = true)]
    async fn a_watch_root_that_will_not_resolve_costs_the_app_its_watch_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.watch = true;
            app.cwd = Some(root.path().join("no-such-directory").display().to_string());
            app.cron_restart = Some("0 * * * *".to_string());
        });

        registry.arm(
            &armed_entry(0, 0, 1000, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );

        let group = &registry.groups["web"];
        assert!(group.watch.is_none(), "the watch root cannot have resolved");
        assert!(
            group.cron.is_some(),
            "an unresolvable watch root must not cost this app its cron worker too"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_root_that_will_not_resolve_says_in_the_log_which_app_lost_its_watch() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("unwatchable", |app| {
            app.watch = true;
            app.cwd = Some(root.path().join("no-such-directory").display().to_string());
        });
        let entry = armed_entry(0, 0, 1000, app, &paths);

        let records = capture_logs(|| {
            registry.arm(&entry, idle_prober(), &rig.extras, &handle);
        });

        assert!(
            registry.groups["unwatchable"].watch.is_none(),
            "precondition: the watch root cannot have resolved"
        );
        assert!(
            records.contains("watch root could not be resolved"),
            "arming no watch must be reported, not swallowed: {records:?}"
        );
        assert!(
            records.contains("WARN"),
            "an app silently losing its watch is a warning, not a debug detail: {records:?}"
        );
        assert!(
            records.contains(r#"name="unwatchable""#),
            "the record must name the app that lost its watch: {records:?}"
        );
    }

    // The clock count is the only trace a second worker leaves: two
    // `restart(Name)` commands racing the same sheep are collapsed by the
    // actor's first-command-wins dedupe.
    #[tokio::test(start_paused = true)]
    async fn a_second_instance_of_a_name_arms_no_second_cron_worker() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        let rig = rig(Duration::from_secs(600));
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string());
            app.instances = 2;
        });
        handle.start(vec![app.clone()]).await.unwrap();

        for (id, instance, pid) in [(0, 0, 1000), (1, 1, 1001)] {
            let entry = armed_entry(id, instance, pid, app.clone(), &paths);
            registry.arm(&entry, idle_prober(), &rig.extras, &handle);
            // Lets the worker commit to its first `next` while the clock still
            // reads close to now: `advance` jumps first and polls after.
            tokio::task::yield_now().await;
        }

        assert_eq!(registry.groups.len(), 1, "one group, not one per instance");
        assert_eq!(
            registry.groups["web"].members,
            HashSet::from([0, 1]),
            "both instances must be recorded as keeping the group alive"
        );

        cross_one_hour().await;
        expect_restart(&mut rx, "web", EVENT_WAIT).await;
        assert!(
            rig.clock.reads() < 20,
            "one cron worker reads the clock ~13 times over this hour; two read ~26 (got {})",
            rig.clock.reads()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_respawn_re_arms_the_enforcer_with_the_new_pid() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let limit = MemSize::from_bytes(500);
        let app = app_with("web", |app| app.max_memory = Some(limit));

        registry.arm(
            &armed_entry(4, 0, 1000, app.clone(), &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        registry.arm(
            &armed_entry(4, 0, 2000, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );

        assert_eq!(
            rig.enforcer.arms(),
            vec![
                ArmCall {
                    id: 4,
                    root_pid: 1000,
                    limit,
                },
                ArmCall {
                    id: 4,
                    root_pid: 2000,
                    limit,
                },
            ]
        );
        assert_eq!(
            rig.enforcer.disarms(),
            vec![4],
            "a re-arm must undo the previous arming rather than leaking it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_last_instance_leaving_stops_the_name_groups_cron_worker() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string());
            app.instances = 2;
        });
        handle.start(vec![app.clone()]).await.unwrap();
        for (id, instance, pid) in [(0, 0, 1000), (1, 1, 1001)] {
            let entry = armed_entry(id, instance, pid, app.clone(), &paths);
            registry.arm(&entry, idle_prober(), &rig.extras, &handle);
            tokio::task::yield_now().await;
        }

        registry.disarm(0, "web");
        assert_eq!(
            registry.groups["web"].members,
            HashSet::from([1]),
            "a non-last disarm drops only its own membership"
        );
        cross_one_hour().await;
        // `ProcessSelector::Name` reaches both online instances, so one
        // occurrence produces two `Restart` events. A leftover would read as a
        // worker that outlived its disarm.
        expect_restart(&mut rx, "web", EVENT_WAIT).await;
        expect_restart(&mut rx, "web", EVENT_WAIT).await;

        registry.disarm(1, "web");
        assert!(
            !registry.groups.contains_key("web"),
            "the last instance leaving must take the group with it"
        );
        assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
    }

    #[tokio::test(start_paused = true)]
    async fn disarming_an_id_that_was_never_armed_leaves_the_group_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string())
        });
        handle.start(vec![app.clone()]).await.unwrap();
        registry.arm(
            &armed_entry(0, 0, 1000, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        tokio::task::yield_now().await;

        registry.disarm(99, "web"); // a name that exists, an id that does not
        registry.disarm(0, "other"); // an id that exists, a name that does not

        assert_eq!(registry.groups["web"].members, HashSet::from([0]));
        cross_one_hour().await;
        expect_restart(&mut rx, "web", EVENT_WAIT).await;
    }

    // The clock makes the claim: a fresh worker on this pattern reads it once
    // before returning, so a second reading means a second worker.
    #[tokio::test(start_paused = true)]
    async fn a_cron_worker_that_ended_on_its_own_is_rebuilt_on_the_next_arm() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        // 30 February: a pattern croner parses and finds no occurrence for,
        // which is the `Ok(None)` arm `spawn_cron_worker` returns on.
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 0 30 2 *".to_string());
        });

        registry.arm(
            &armed_entry(0, 0, 1000, app.clone(), &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        settle_finished(
            registry.groups["web"]
                .cron
                .as_ref()
                .expect("the first arm spawns a worker"),
        )
        .await;
        assert_eq!(
            rig.clock.reads(),
            1,
            "a worker on a pattern with no occurrence reads the clock once and ends"
        );

        registry.arm(
            &armed_entry(0, 0, 2000, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        settle_finished(
            registry.groups["web"]
                .cron
                .as_ref()
                .expect("the re-arm must leave a worker behind"),
        )
        .await;
        assert_eq!(
            rig.clock.reads(),
            2,
            "a re-arm must rebuild a name-group task that ended on its own"
        );
    }

    // This app asks for a watch and gets none. Under a build-keyed membership it
    // would be in no group at all, so stopping a later instance whose watch did
    // arm would tear the watch down with this one still online.
    #[tokio::test(start_paused = true)]
    async fn an_instance_whose_watch_could_not_be_armed_still_joins_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let limit = MemSize::from_bytes(500);
        // `canonicalize` failing is the one watch-arming failure `normalize`
        // lets through: it never checks that the cwd resolves.
        let missing = dir.path().join("no-such-directory");
        let app = app_with("web", |app| {
            app.watch = true;
            app.cwd = Some(missing.display().to_string());
            app.max_memory = Some(limit);
        });

        registry.arm(
            &armed_entry(0, 0, 1000, app, &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );

        let group = registry
            .groups
            .get("web")
            .expect("a watched app is a member of its group whether or not the watch armed");
        assert!(group.watch.is_none(), "this app's watch cannot have armed");
        assert_eq!(group.members, HashSet::from([0]));
        assert_eq!(
            rig.enforcer.arms(),
            vec![ArmCall {
                id: 0,
                root_pid: 1000,
                limit,
            }],
            "a watch that could not be armed must not take the memory limit with it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_registry_stops_the_liveness_loop_it_armed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let mut rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let app = app_with("web", |app| {
            app.liveness_probe = Some(ProbeConfig {
                failure_threshold: 1,
                ..probe_config(ProbeKind::Tcp, "localhost:5432")
            });
        });
        let interval = app
            .config()
            .liveness_probe
            .as_ref()
            .expect("the fixture just set one")
            .interval
            .as_duration();

        let mut kept = ExtrasRegistry::default();
        kept.arm(
            &armed_entry(8, 1, 5678, app.clone(), &paths),
            failing_prober(),
            &rig.extras,
            &handle,
        );
        let mut discarded = ExtrasRegistry::default();
        discarded.arm(
            &armed_entry(7, 0, 1234, app, &paths),
            failing_prober(),
            &rig.extras,
            &handle,
        );
        drop(discarded);

        let failure = expect_liveness(&mut rig.liveness, EVENT_WAIT).await;
        assert_eq!(
            failure,
            LivenessReport {
                id: 8,
                pid: 5678,
                epoch: 1
            },
            "only the registry that is still alive may report"
        );
        assert_no_liveness_within(&mut rig.liveness, interval * 3).await;
    }

    // Two names, not two instances of one: the bus attributes a restart to a
    // name, so the control and the subject have to be tellable apart on the
    // wire. `kept` is that control.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_registry_stops_the_name_group_worker_it_armed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let hourly = |name: &str| {
            app_with(name, |app| {
                app.cron_restart = Some("0 * * * *".to_string());
            })
        };
        handle
            .start(vec![hourly("kept"), hourly("dropped")])
            .await
            .unwrap();

        let mut kept = ExtrasRegistry::default();
        kept.arm(
            &armed_entry(0, 0, 1000, hourly("kept"), &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        let mut discarded = ExtrasRegistry::default();
        discarded.arm(
            &armed_entry(1, 0, 1001, hourly("dropped"), &paths),
            idle_prober(),
            &rig.extras,
            &handle,
        );
        // Lets both workers commit to their first `next` while the clock still
        // reads close to now.
        tokio::task::yield_now().await;
        drop(discarded);

        cross_one_hour().await;
        expect_restart(&mut rx, "kept", EVENT_WAIT).await;
        assert_no_restart_within(&mut rx, "dropped", PAST_THE_NEXT_OCCURRENCE).await;
    }

    // A healthy liveness loop never ends on its own, so a stopped sheep would
    // leak a task probing a pid that is gone.
    #[tokio::test(start_paused = true)]
    async fn disarming_an_id_stops_its_liveness_loop() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let mut rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.instances = 2;
            app.liveness_probe = Some(ProbeConfig {
                failure_threshold: 1,
                ..probe_config(ProbeKind::Tcp, "localhost:5432")
            });
        });
        let interval = app
            .config()
            .liveness_probe
            .as_ref()
            .expect("the fixture just set one")
            .interval
            .as_duration();

        for (id, instance, pid) in [(0, 0, 1000), (1, 1, 1001)] {
            registry.arm(
                &armed_entry(id, instance, pid, app.clone(), &paths),
                failing_prober(),
                &rig.extras,
                &handle,
            );
        }
        registry.disarm(0, "web");

        let failure = expect_liveness(&mut rig.liveness, EVENT_WAIT).await;
        assert_eq!(
            failure,
            LivenessReport {
                id: 1,
                pid: 1001,
                epoch: 1
            },
            "only the instance that is still armed may report"
        );
        assert_no_liveness_within(&mut rig.liveness, interval * 3).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_liveness_threshold_reports_this_instances_id_and_pid() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let mut rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.liveness_probe = Some(ProbeConfig {
                failure_threshold: 2,
                ..probe_config(ProbeKind::Tcp, "localhost:5432")
            });
        });
        let interval = app
            .config()
            .liveness_probe
            .as_ref()
            .expect("the fixture just set one")
            .interval
            .as_duration();

        registry.arm(
            &armed_entry(7, 0, 1234, app, &paths),
            Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)])),
            &rig.extras,
            &handle,
        );

        let failure = expect_liveness(&mut rig.liveness, EVENT_WAIT).await;
        assert_eq!(
            failure,
            LivenessReport {
                id: 7,
                pid: 1234,
                epoch: 1
            }
        );
        // The scripted prober repeats its last outcome forever, so a loop still
        // probing after its report would report again inside this window.
        assert_no_liveness_within(&mut rig.liveness, interval * 3).await;
    }

    // `notify-debouncer-full` derives its poll tick as `delay / 4` and sleeps it
    // on a dedicated OS thread, so a zero makes that thread spin. A direct call,
    // since `normalize` refuses `watch_delay = "0"` and no fixture can carry one
    // as far as `ExtrasRegistry::arm`.
    #[test]
    fn a_zero_watch_delay_is_floored_before_it_reaches_the_debouncer() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_delay = Some(UpDuration::from_millis(0));
        assert_eq!(watch_delay_for(&app), MIN_WATCH_DELAY);
    }

    // `normalize` accepts every non-zero `watch_delay`, so a floor above one
    // millisecond would lengthen a round trip the user shortened on purpose.
    #[test]
    fn a_watch_delay_the_config_layer_accepts_is_never_clamped() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_delay = Some(UpDuration::from_millis(1));
        assert_eq!(watch_delay_for(&app), Duration::from_millis(1));

        app.watch_delay = Some(UpDuration::from_millis(20));
        assert_eq!(watch_delay_for(&app), Duration::from_millis(20));

        app.watch_delay = None;
        assert_eq!(watch_delay_for(&app), DEFAULT_WATCH_DELAY);
    }

    // ------------------------------------------------------------------
    // The reporter: a report becomes a guarded restart, or nothing.
    // ------------------------------------------------------------------

    // Without `CommandOrigin::Automatic` a memory-breach restart reaches every
    // subscriber as `manually: true`, indistinguishable from `shep restart`.
    #[tokio::test(start_paused = true)]
    async fn a_breach_naming_the_running_pid_restarts_that_sheep() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        // The ceiling the synthetic breach below names: the actor re-asks a
        // breach against the ceiling in force.
        handle
            .start(vec![app_with("web", |app| {
                app.max_memory = Some(MemSize::from_bytes(500));
            })])
            .await
            .unwrap();
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        let (breach_tx, breach_rx) = mpsc::channel(4);
        let (_live_tx, live_rx) = mpsc::channel(4);
        let _reporter = spawn_extras_reporter(breach_rx, live_rx, handle.clone());

        breach_tx
            .send(LimitBreach {
                id: 0,
                root_pid: pid,
                observed: MemSize::from_bytes(900),
                limit: MemSize::from_bytes(500),
            })
            .await
            .unwrap();

        let (info, manually) = expect_restart_event(&mut rx, "web", EVENT_WAIT).await;
        assert_eq!(info.id, 0);
        assert_eq!(info.restarts, 1);
        assert!(
            !manually,
            "nobody typed this: a memory breach is the daemon's own doing"
        );
    }

    // Separate from the breach case because the reporter's two arms are two
    // `select!` branches: a broken `liveness` arm leaves the breach one green.
    #[tokio::test(start_paused = true)]
    async fn a_liveness_failure_naming_the_running_pid_restarts_that_sheep() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        handle.start(vec![app_with("web", |_| {})]).await.unwrap();
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        let (_breach_tx, breach_rx) = mpsc::channel(4);
        let (live_tx, live_rx) = mpsc::channel(4);
        let _reporter = spawn_extras_reporter(breach_rx, live_rx, handle.clone());

        // `spawn_test_fixture` wires no `Extras`, so the actor's epoch for id 0
        // stays at the `0` reported for an unseen id, which this report matches.
        live_tx
            .send(LivenessReport {
                id: 0,
                pid,
                epoch: 0,
            })
            .await
            .unwrap();

        let (info, manually) = expect_restart_event(&mut rx, "web", EVENT_WAIT).await;
        assert_eq!(info.id, 0);
        assert_eq!(info.restarts, 1);
        assert!(
            !manually,
            "nobody typed this: a liveness failure is the daemon's own doing"
        );
    }

    // A report for an id a `Delete` already removed is an ordinary race. The
    // surviving `list()` is the proof: a panicked actor closes its mailbox.
    #[tokio::test(start_paused = true)]
    async fn an_extra_restart_for_an_unknown_id_leaves_the_engine_running() {
        let (handle, _rx, _fixture) = spawn_test_fixture();
        handle.start(vec![app_with("web", |_| {})]).await.unwrap();

        handle.extra_restart(99, 4242, None, None).await;

        assert_eq!(
            handle.list().await.len(),
            1,
            "the actor must still be serving after a report for an id it does not know"
        );
    }

    // A gated app between its spawn and its readiness result is `Starting` with
    // a live pid, the one state in which the pid guard passes and the status
    // guard is all that is left.
    #[tokio::test(start_paused = true)]
    async fn an_extra_restart_for_a_sheep_that_is_still_starting_restarts_nothing() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        let app = app_with("web", |app| {
            app.wait_ready = true;
            // Long enough that the readiness wait cannot resolve inside this
            // test's windows.
            app.listen_timeout = UpDuration::from_millis(6 * 60 * 60 * 1_000);
        });
        handle.start(vec![app]).await.unwrap();
        let listing = handle.list().await;
        assert_eq!(
            listing[0].status,
            ProcStatus::Starting,
            "this case is only meaningful while the sheep is gated on readiness"
        );
        let pid = listing[0].pid.expect("a spawned sheep has a pid");

        handle.extra_restart(0, pid, None, None).await;

        assert_no_restart_within(&mut rx, "web", Duration::from_secs(30)).await;
        assert_eq!(handle.list().await[0].restarts, 0);
    }

    // `ProcessSelector::Id` matches regardless of status, so the public
    // `restart` on a stopped sheep respawns it. The runner carries spare procs
    // so that resurrection would have something to spawn from.
    #[tokio::test(start_paused = true)]
    async fn a_breach_for_a_stopped_sheep_restarts_nothing() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        handle.start(vec![app_with("web", |_| {})]).await.unwrap();
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        let (breach_tx, breach_rx) = mpsc::channel(4);
        let (_live_tx, live_rx) = mpsc::channel(4);
        let _reporter = spawn_extras_reporter(breach_rx, live_rx, handle.clone());

        handle
            .stop(ProcessSelector::Id(0))
            .await
            .expect("the sheep stops");
        breach_tx
            .send(LimitBreach {
                id: 0,
                root_pid: pid,
                observed: MemSize::from_bytes(900),
                limit: MemSize::from_bytes(500),
            })
            .await
            .unwrap();

        assert_no_restart_within(&mut rx, "web", Duration::from_secs(30)).await;
        let listing = handle.list().await;
        assert_eq!(listing[0].status, ProcStatus::Stopped);
        assert_eq!(listing[0].restarts, 0);
    }

    // A breach raised for the process a restart already replaced would restart
    // its healthy successor.
    #[tokio::test(start_paused = true)]
    async fn a_breach_carrying_the_previous_pid_restarts_nothing() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        // The ceiling both synthetic breaches below name.
        handle
            .start(vec![app_with("web", |app| {
                app.max_memory = Some(MemSize::from_bytes(500));
            })])
            .await
            .unwrap();
        let stale_pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        let (breach_tx, breach_rx) = mpsc::channel(4);
        let (_live_tx, live_rx) = mpsc::channel(4);
        let _reporter = spawn_extras_reporter(breach_rx, live_rx, handle.clone());

        handle
            .restart(ProcessSelector::Id(0))
            .await
            .expect("the sheep restarts");
        expect_restart(&mut rx, "web", EVENT_WAIT).await;
        let live_pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        assert_ne!(stale_pid, live_pid, "the respawn must have a new pid");

        let breach = |root_pid| LimitBreach {
            id: 0,
            root_pid,
            observed: MemSize::from_bytes(900),
            limit: MemSize::from_bytes(500),
        };
        breach_tx.send(breach(stale_pid)).await.unwrap();
        assert_no_restart_within(&mut rx, "web", Duration::from_secs(30)).await;
        assert_eq!(
            handle.list().await[0].restarts,
            1,
            "a stale breach must not bump the restart count"
        );

        breach_tx.send(breach(live_pid)).await.unwrap();
        let info = expect_restart(&mut rx, "web", EVENT_WAIT).await;
        assert_eq!(
            info.restarts, 2,
            "the same reporter, fed the current pid, must restart"
        );
    }

    // ------------------------------------------------------------------
    // The actor tier: that the engine really arms and disarms at the
    // transitions, not just that the registry can.
    // ------------------------------------------------------------------

    // `ProcessSelector::Name` matches a stopped sheep too, so a cron worker that
    // keeps its schedule brings back a sheep the user stopped.
    #[tokio::test(start_paused = true)]
    async fn stopping_the_last_instance_stops_its_cron_worker() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let h = harness_with_extras(vec![ProcScript::never_exits(); 12], |reports| Extras {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            enforcer: Arc::new(RecordingEnforcer::default()),
            max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
            reports,
            stats: idle_stats(),
        });
        let mut rx = h.ctx.events.subscribe();
        h.ctx
            .supervisor
            .start(vec![app_with("web", |app| {
                app.cron_restart = Some("0 * * * *".to_string());
            })])
            .await
            .unwrap();

        cross_one_hour().await;
        expect_restart(&mut rx, "web", EVENT_WAIT).await;

        h.ctx
            .supervisor
            .stop(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the sheep stops");
        assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
        assert_eq!(h.ctx.supervisor.list().await[0].status, ProcStatus::Stopped);
    }

    /// Keyed by id and kind rather than by name: a swap puts two entries under
    /// one name.
    async fn expect_process_event(
        rx: &mut broadcast::Receiver<SharedEvent>,
        id: u32,
        kind: ProcessEventKind,
        window: Duration,
    ) {
        let wanted = async {
            loop {
                match rx.recv().await.map(|event| event.to_event()) {
                    Ok(BusEvent::Process { event, info, .. }) if event == kind && info.id == id => {
                        return;
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(err) => panic!("event stream closed before {kind:?} for id {id}: {err}"),
                }
            }
        };
        if tokio::time::timeout(window, wanted).await.is_err() {
            panic!("timed out waiting for {kind:?} for id {id}");
        }
    }

    // The clock reading is the only observation out here: a rebuild re-spawns
    // the cron worker, which reads the wall clock on its first poll.
    // `max_cron_sleep` is 600s against a swap costing at most 11s of virtual
    // time, so a surviving worker cannot wake inside the window and read too.
    #[tokio::test(start_paused = true)]
    async fn a_reload_leaves_the_name_groups_cron_worker_where_it_was() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        // Two procs, counted: the original and the one replacement a reload of
        // a one-instance app performs. A third is answered "script exhausted".
        let h = harness_with_extras(vec![ProcScript::never_exits(); 2], |reports| Extras {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            enforcer: Arc::new(RecordingEnforcer::default()),
            max_cron_sleep: Duration::from_secs(600),
            reports,
            stats: idle_stats(),
        });
        let mut rx = h.ctx.events.subscribe();
        h.ctx
            .supervisor
            .start(vec![app_with("web", |app| {
                app.cron_restart = Some("0 * * * *".to_string());
            })])
            .await
            .unwrap();
        expect_process_event(&mut rx, 0, ProcessEventKind::Online, EVENT_WAIT).await;
        // Lets the armed worker reach its first poll before the count is taken.
        tokio::task::yield_now().await;

        let reads_before = clock.reads();
        assert_eq!(
            reads_before, 1,
            "fixture check: one armed cron worker takes exactly one reading, \
             so a rebuild's second one is a countable difference rather than \
             noise — and a count of 0 here would mean the worker had not run \
             yet, which would make the claim below vacuous"
        );

        h.ctx
            .supervisor
            .reload(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the reload is accepted");
        expect_process_event(&mut rx, 1, ProcessEventKind::Online, EVENT_WAIT).await;
        expect_process_event(&mut rx, 0, ProcessEventKind::Delete, EVENT_WAIT).await;

        let listed = h.ctx.supervisor.list().await;
        assert_eq!(
            listed.iter().map(|info| info.id).collect::<Vec<_>>(),
            vec![1],
            "fixture check: the swap must have run to completion, or there was \
             never an overlap for the ordering to matter in"
        );
        assert_eq!(
            clock.reads(),
            reads_before,
            "the name group's cron worker must have been left where it was: a \
             rebuilt one reads the clock again to derive its next occurrence"
        );
    }

    /// A harness whose runner hands out one proc that exits at once and then
    /// plenty that never do, plus a cron-restarting app that parks in a long
    /// backoff after any exit.
    ///
    /// The two cases below need a sheep in `WaitingRestart`, the one state that
    /// reaches a terminal transition through `apply_immediate`.
    fn backoff_harness(clock: &Arc<TestClock>) -> Harness {
        harness_with_extras(
            {
                let mut scripts = vec![ProcScript::never_exits()];
                scripts.push(ProcScript::const_exit(1));
                // Spare procs so a broken implementation has something to
                // respawn from: without them the supervisor emits `Errored` and
                // the negative assertions below pass vacuously.
                scripts.extend([ProcScript::never_exits(); 8]);
                scripts
            },
            |reports| Extras {
                clock: Arc::clone(clock) as Arc<dyn Clock>,
                enforcer: Arc::new(RecordingEnforcer::default()),
                max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
                reports,
                stats: idle_stats(),
            },
        )
    }

    /// The cron-restarting app the two backoff cases start, parked in a backoff
    /// far longer than either case's own window, so its pending `RestartDue` can
    /// never be what a restart came from.
    fn backoff_app() -> shep_core::config::ResolvedApp {
        app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string());
            app.restart_delay = Some(UpDuration::from_millis(3 * 60 * 60 * 1_000));
        })
    }

    /// Each `list()` is a full round trip through the actor's mailbox, so this
    /// makes progress rather than merely observing it.
    async fn settle_into(supervisor: &SupervisorHandle, id: u32, status: ProcStatus) {
        for _ in 0..200 {
            tokio::task::yield_now().await;
            let listing = supervisor.list().await;
            if listing
                .iter()
                .any(|info| info.id == id && info.status == status)
            {
                return;
            }
        }
        panic!("id {id} never reached {status:?}");
    }

    // A sheep waiting out its restart backoff has no live task, so its stop
    // never reaches `handle_exited`'s terminal branches, and
    // `ProcessSelector::Name` matches a stopped sheep just as happily.
    #[tokio::test(start_paused = true)]
    async fn stopping_a_sheep_mid_backoff_still_stops_its_cron_worker() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let h = backoff_harness(&clock);
        let mut rx = h.ctx.events.subscribe();
        h.ctx.supervisor.start(vec![backoff_app()]).await.unwrap();

        // The cron occurrence restarts it onto the immediately-exiting proc,
        // landing it in its three-hour backoff.
        cross_one_hour().await;
        expect_restart(&mut rx, "web", EVENT_WAIT).await;
        settle_into(&h.ctx.supervisor, 0, ProcStatus::WaitingRestart).await;

        h.ctx
            .supervisor
            .stop(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the sheep stops");
        assert_eq!(h.ctx.supervisor.list().await[0].status, ProcStatus::Stopped);
        // Spans the next occurrence, and stays well inside the three-hour
        // backoff, so a restart arriving here could only be the cron worker.
        assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
    }

    // Without a disarm the slot is deregistered while the name-group's cron
    // worker keeps firing at a name nothing answers to.
    #[tokio::test(start_paused = true)]
    async fn deleting_a_sheep_mid_backoff_takes_its_cron_worker_with_it() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let h = backoff_harness(&clock);
        let mut rx = h.ctx.events.subscribe();
        h.ctx.supervisor.start(vec![backoff_app()]).await.unwrap();

        cross_one_hour().await;
        expect_restart(&mut rx, "web", EVENT_WAIT).await;
        settle_into(&h.ctx.supervisor, 0, ProcStatus::WaitingRestart).await;

        h.ctx
            .supervisor
            .delete(ProcessSelector::Name("web".to_string()))
            .await
            .expect("the sheep is deleted");
        assert!(h.ctx.supervisor.list().await.is_empty());
        // A surviving worker would `restart(Name("web"))`, find nothing, and log
        // at debug, emitting no bus event. The clock is the observable claim.
        let reads_after_delete = clock.reads();
        assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
        assert_eq!(
            clock.reads(),
            reads_after_delete,
            "a deleted sheep's cron worker must stop reading the clock, not merely stop finding sheep"
        );
    }

    // A cron occurrence reaches a group's instances through two doors: a running
    // instance from `handle_exited`'s forced-restart branch, one sitting out its
    // backoff from `apply_immediate`. Both must report `manually: false`.
    #[tokio::test(start_paused = true)]
    async fn a_cron_restart_is_never_reported_as_a_user_action() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        // The script pool makes the second half reachable: the first occurrence
        // respawns onto a proc that exits at once, and the spares behind it are
        // what the second respawns from.
        let h = backoff_harness(&clock);
        let mut rx = h.ctx.events.subscribe();
        h.ctx.supervisor.start(vec![backoff_app()]).await.unwrap();

        cross_one_hour().await;
        let (running, manually) = expect_restart_event(&mut rx, "web", EVENT_WAIT).await;
        assert_eq!(running.restarts, 1);
        assert!(
            !manually,
            "a cron occurrence is nobody typing `shep restart`"
        );

        // That respawn exited into a three-hour backoff, so the next occurrence
        // is well inside it and finds no live task.
        settle_into(&h.ctx.supervisor, 0, ProcStatus::WaitingRestart).await;
        cross_one_hour().await;
        let (backing_off, manually) = expect_restart_event(&mut rx, "web", EVENT_WAIT).await;
        assert_eq!(backing_off.restarts, 2);
        assert!(
            !manually,
            "the same occurrence must answer the same way through `apply_immediate`"
        );
    }

    // `respawn`'s Err arm is reachable in an ordinary deploy: a binary replaced
    // mid-deploy, or a cwd that is gone. A failed respawn emits `Errored` and
    // never `Restart`, so a surviving worker leaves only its clock readings.
    #[tokio::test(start_paused = true)]
    async fn a_respawn_that_cannot_spawn_stops_the_name_groups_cron_worker() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        // Exactly one script: the initial start consumes it, so the cron
        // occurrence's respawn finds it exhausted and `respawn` takes its Err
        // arm.
        let h = harness_with_extras(vec![ProcScript::never_exits()], |reports| Extras {
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            enforcer: Arc::new(RecordingEnforcer::default()),
            max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
            reports,
            stats: idle_stats(),
        });
        let mut rx = h.ctx.events.subscribe();
        h.ctx
            .supervisor
            .start(vec![app_with("web", |app| {
                app.cron_restart = Some("0 * * * *".to_string());
            })])
            .await
            .unwrap();

        cross_one_hour().await;
        settle_into(&h.ctx.supervisor, 0, ProcStatus::Errored).await;

        let reads_after_error = clock.reads();
        assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
        assert_eq!(
            clock.reads(),
            reads_after_error,
            "an errored sheep's cron worker must stop reading the clock"
        );
    }

    // The scripted table holds exactly one process, the pid the runner hands the
    // first spawn, so an arming against any other number never breaches.
    #[tokio::test(start_paused = true)]
    async fn the_actor_arms_the_memory_limit_against_the_spawned_pid() {
        let mut h = harness_with_extras(vec![ProcScript::never_exits(); 4], |reports| {
            let sampler: Arc<dyn MemorySampler> =
                Arc::new(ScriptedSampler::new(vec![vec![ProcessRss {
                    pid: 1000,
                    parent: None,
                    bytes: 900,
                    cpu_ms: 0,
                }]]));
            let stats = Arc::new(StatsState::new(Arc::clone(&sampler)));
            Extras {
                clock: Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z"))),
                enforcer: Arc::new(PollingEnforcer::start(
                    sampler,
                    reports.breaches.clone(),
                    Arc::clone(&stats),
                )),
                max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
                reports,
                stats,
            }
        });
        h.ctx
            .supervisor
            .start(vec![app_with("web", |app| {
                app.max_memory = Some(MemSize::from_bytes(500));
            })])
            .await
            .unwrap();
        let pid = h.ctx.supervisor.list().await[0]
            .pid
            .expect("a live sheep has a pid");

        let breach = match tokio::time::timeout(EVENT_WAIT, h.breaches.recv()).await {
            Ok(Some(breach)) => breach,
            Ok(None) => panic!("the breach channel closed before a breach arrived"),
            Err(_) => panic!("timed out waiting for a breach"),
        };

        assert_eq!(breach.id, 0);
        assert_eq!(
            breach.root_pid, pid,
            "the enforcer must be armed against the pid the sheep is actually running as"
        );
        assert_eq!(breach.observed.bytes(), 900);
    }

    // The gated one of `arm_extras`'s three transitions:
    // `handle_ready_result`'s `went_online` reverted to a plain `emit` leaves
    // every other case in this file green while every readiness-gated app loses
    // all four extras. The readiness wait ends in a timeout, the same site.
    #[tokio::test(start_paused = true)]
    async fn the_actor_arms_a_readiness_gated_app_once_it_comes_online() {
        let mut h = harness_with_extras(vec![ProcScript::never_exits(); 4], |reports| {
            let sampler: Arc<dyn MemorySampler> =
                Arc::new(ScriptedSampler::new(vec![vec![ProcessRss {
                    pid: 1000,
                    parent: None,
                    bytes: 900,
                    cpu_ms: 0,
                }]]));
            let stats = Arc::new(StatsState::new(Arc::clone(&sampler)));
            Extras {
                clock: Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z"))),
                enforcer: Arc::new(PollingEnforcer::start(
                    sampler,
                    reports.breaches.clone(),
                    Arc::clone(&stats),
                )),
                max_cron_sleep: DEFAULT_MAX_CRON_SLEEP,
                reports,
                stats,
            }
        });
        h.ctx
            .supervisor
            .start(vec![app_with("web", |app| {
                app.wait_ready = true;
                app.max_memory = Some(MemSize::from_bytes(500));
            })])
            .await
            .unwrap();
        let listing = h.ctx.supervisor.list().await;
        assert_eq!(
            listing[0].status,
            ProcStatus::Starting,
            "a gated app must not be Online when its Start reply lands"
        );
        let pid = listing[0].pid.expect("a spawned sheep has a pid");

        let breach = match tokio::time::timeout(EVENT_WAIT, h.breaches.recv()).await {
            Ok(Some(breach)) => breach,
            Ok(None) => panic!("the breach channel closed before a breach arrived"),
            Err(_) => panic!("timed out waiting for a breach"),
        };
        assert_eq!(breach.id, 0);
        assert_eq!(
            breach.root_pid, pid,
            "the gated path must arm against the pid the sheep is running as"
        );
    }

    // Real time and a real `OsProber`, because that is what the actor builds:
    // the paused clock does not move a real TCP connect.
    #[tokio::test]
    async fn the_actor_arms_the_liveness_loop_against_the_spawned_pid() {
        // Reserve a port, then release it: nothing ever listens there, so every
        // probe fails with a connection refusal.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);

        let mut h = harness(vec![ProcScript::never_exits(); 4]);
        h.ctx
            .supervisor
            .start(vec![app_with("web", |app| {
                app.liveness_probe = Some(ProbeConfig {
                    failure_threshold: 1,
                    interval: PROBE_INTERVAL,
                    timeout: UpDuration::from_millis(500),
                    ..probe_config(ProbeKind::Tcp, &addr.to_string())
                });
            })])
            .await
            .unwrap();
        let pid = h.ctx.supervisor.list().await[0]
            .pid
            .expect("a live sheep has a pid");

        let failure = expect_liveness(&mut h.liveness, LIVENESS_DEADLINE).await;
        assert_eq!(
            failure,
            LivenessReport {
                id: 0,
                pid,
                epoch: 1
            }
        );
    }

    /// `arm` keeps a live cron or watch task, which is right for a reload's
    /// overlap and wrong for a config change: those tasks read their
    /// group-scoped config when they are built.
    #[tokio::test(start_paused = true)]
    async fn rearm_name_replaces_a_live_group_task() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string());
            app.watch = true;
            app.cwd = Some(root.path().display().to_string());
        });
        handle.start(vec![app.clone()]).await.unwrap();

        let entry = armed_entry(0, 0, 1000, app.clone(), &paths);
        registry.arm(&entry, idle_prober(), &rig.extras, &handle);
        tokio::task::yield_now().await;

        let before_cron = registry.groups["web"].cron.as_ref().unwrap().abort_handle();
        let before_watch = registry.groups["web"]
            .watch
            .as_ref()
            .unwrap()
            .abort_handle();

        registry.rearm_name("web", &[&entry], |_| idle_prober(), &rig.extras, &handle);
        tokio::task::yield_now().await;

        let after_cron = registry.groups["web"].cron.as_ref().unwrap().abort_handle();
        let after_watch = registry.groups["web"]
            .watch
            .as_ref()
            .unwrap()
            .abort_handle();
        assert_ne!(
            before_cron.id(),
            after_cron.id(),
            "the cron worker survived a rearm"
        );
        assert_ne!(
            before_watch.id(),
            after_watch.id(),
            "the watch task survived a rearm"
        );
    }

    /// An app left with no watcher at all is worse than one left with a stale
    /// watcher.
    #[tokio::test(start_paused = true)]
    async fn rearm_name_leaves_the_group_armed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string());
            app.watch = true;
            app.cwd = Some(root.path().display().to_string());
        });
        handle.start(vec![app.clone()]).await.unwrap();

        let entry = armed_entry(0, 0, 1000, app.clone(), &paths);
        registry.arm(&entry, idle_prober(), &rig.extras, &handle);
        tokio::task::yield_now().await;

        registry.rearm_name("web", &[&entry], |_| idle_prober(), &rig.extras, &handle);
        tokio::task::yield_now().await;

        let group = &registry.groups["web"];
        assert!(
            group.cron.as_ref().is_some_and(|cron| !cron.is_finished()),
            "the group must still have a live cron worker after a rearm"
        );
        assert!(
            group
                .watch
                .as_ref()
                .is_some_and(|watch| !watch.is_finished()),
            "the group must still have a live watch task after a rearm"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rearm_name_leaves_another_apps_group_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let web_root = tempfile::tempdir().unwrap();
        let worker_root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let web = app_with("web", |app| {
            app.watch = true;
            app.cwd = Some(web_root.path().display().to_string());
        });
        let worker = app_with("worker", |app| {
            app.watch = true;
            app.cwd = Some(worker_root.path().display().to_string());
        });
        handle
            .start(vec![web.clone(), worker.clone()])
            .await
            .unwrap();

        let web_entry = armed_entry(0, 0, 1000, web.clone(), &paths);
        let worker_entry = armed_entry(1, 0, 1001, worker.clone(), &paths);
        registry.arm(&web_entry, idle_prober(), &rig.extras, &handle);
        registry.arm(&worker_entry, idle_prober(), &rig.extras, &handle);
        tokio::task::yield_now().await;

        let worker_watch_before = registry.groups["worker"]
            .watch
            .as_ref()
            .unwrap()
            .abort_handle();

        registry.rearm_name(
            "web",
            &[&web_entry],
            |_| idle_prober(),
            &rig.extras,
            &handle,
        );
        tokio::task::yield_now().await;

        let worker_watch_after = registry.groups["worker"]
            .watch
            .as_ref()
            .unwrap()
            .abort_handle();
        assert_eq!(
            worker_watch_before.id(),
            worker_watch_after.id(),
            "rearming \"web\" must not touch \"worker\"'s group"
        );
    }

    /// One group shared across a name's instances is only transitively protected
    /// by `arm`'s own idempotency test, since `rearm_name` builds no task
    /// itself.
    #[tokio::test(start_paused = true)]
    async fn rearm_name_rebuilds_a_multi_instance_group_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let root = tempfile::tempdir().unwrap();
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let app = app_with("web", |app| {
            app.instances = 2;
            app.cron_restart = Some("0 * * * *".to_string());
            app.watch = true;
            app.cwd = Some(root.path().display().to_string());
        });
        handle.start(vec![app.clone()]).await.unwrap();

        let entry_a = armed_entry(0, 0, 1000, app.clone(), &paths);
        let entry_b = armed_entry(1, 1, 1001, app.clone(), &paths);
        registry.arm(&entry_a, idle_prober(), &rig.extras, &handle);
        registry.arm(&entry_b, idle_prober(), &rig.extras, &handle);
        tokio::task::yield_now().await;

        let before_cron = registry.groups["web"].cron.as_ref().unwrap().abort_handle();
        let before_watch = registry.groups["web"]
            .watch
            .as_ref()
            .unwrap()
            .abort_handle();

        // The prober closure is the seam that pins one prober per entry:
        // `assemble` bakes `SHEP_INSTANCE` into the environment a prober runs
        // with, so a shared one would probe every instance as one. The call
        // count and order are what is observable here.
        let probed = std::sync::Mutex::new(Vec::new());
        registry.rearm_name(
            "web",
            &[&entry_a, &entry_b],
            |entry| {
                probed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(entry.id);
                idle_prober()
            },
            &rig.extras,
            &handle,
        );
        tokio::task::yield_now().await;

        assert_eq!(
            probed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            [0, 1],
            "each instance must get its own prober, in id order"
        );

        let group = &registry.groups["web"];
        assert_eq!(
            group.members,
            HashSet::from([0, 1]),
            "both instances must still be members after a rearm, not just the last one arm'd"
        );
        assert!(
            group.cron.is_some(),
            "the group must hold exactly one cron task, not zero"
        );
        assert!(
            group.watch.is_some(),
            "the group must hold exactly one watch task, not zero"
        );
        let after_cron = group.cron.as_ref().unwrap().abort_handle();
        let after_watch = group.watch.as_ref().unwrap().abort_handle();
        assert_ne!(
            before_cron.id(),
            after_cron.id(),
            "the cron worker must be rebuilt by the rearm, not left over"
        );
        assert_ne!(
            before_watch.id(),
            after_watch.id(),
            "the watch task must be rebuilt by the rearm, not left over"
        );
    }

    /// Tests that wait on real filesystem events or real elapsed time. The
    /// inner loop skips them with `--skip ::slow::`; the full suite runs them.
    mod slow {
        use super::*;

        // The overlap a reload runs on: the replacement arms before the
        // drainee's exit disarms the old id, so `disarm` finds a member still
        // standing. Task identity tells a surviving group from a rebuilt one, and
        // the two `AbortHandle`s are held unfired so tokio cannot reuse the id.
        #[tokio::test(start_paused = true)]
        async fn a_replacement_arming_before_the_drainee_disarms_keeps_the_groups_own_tasks() {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(&dir);
            let root = tempfile::tempdir().unwrap();
            let (handle, _rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            // Both per-name extras, because the overlap has to hold for both.
            let app = app_with("web", |app| {
                app.cron_restart = Some("0 * * * *".to_string());
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
            });
            handle.start(vec![app.clone()]).await.unwrap();

            // The drainee: id 0, holding instance slot 0.
            registry.arm(
                &armed_entry(0, 0, 1000, app.clone(), &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );
            // Lets the cron worker reach its first poll, so the reading below
            // is settled rather than racing it.
            tokio::task::yield_now().await;

            let group = &registry.groups["web"];
            let cron = group
                .cron
                .as_ref()
                .expect("fixture check: the cron worker must have armed")
                .abort_handle();
            let watch = group
                .watch
                .as_ref()
                .expect("fixture check: the watch must have armed")
                .abort_handle();
            let reads_before = rig.clock.reads();

            // The overlap, in the order a swap performs it: the replacement
            // takes a new id in the drainee's slot and goes `Online` first.
            registry.arm(
                &armed_entry(1, 0, 2000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );
            registry.disarm(0, "web");
            // Lets a rebuilt worker reach its own first poll, so an unchanged
            // count below means there is none rather than that it had not run.
            tokio::task::yield_now().await;

            let group = &registry.groups["web"];
            assert_eq!(
                group.members,
                HashSet::from([1]),
                "fixture check: the drainee must really have left a group the \
             replacement had already joined — this reads the same under either \
             ordering, which is why it cannot be the claim that matters"
            );
            assert_eq!(
                group.cron.as_ref().map(JoinHandle::id),
                Some(cron.id()),
                "the group must still hold the cron worker it was armed with, not \
             an identical one put back in its place"
            );
            assert_eq!(
                group.watch.as_ref().map(JoinHandle::id),
                Some(watch.id()),
                "and the watch it was armed with, whose rebuild means re-registering \
             the OS watcher"
            );
            assert_eq!(
                rig.clock.reads(),
                reads_before,
                "a surviving cron worker performs no startup work; a rebuilt one \
             reads the clock again to derive its next occurrence"
            );
        }

        // A name group with zero online instances, armed and disarmed before it
        // ever fires. An app whose first spawn is stopped straight away passes
        // through this shape, and a worker leaked there restarts a flock nobody
        // is running.
        #[tokio::test(start_paused = true)]
        async fn a_group_disarmed_before_its_first_occurrence_leaves_no_worker_behind() {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(&dir);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            // Both per-name extras and one per-pid extra, so a single disarm
            // has to reach all three.
            let app = app_with("web", |app| {
                app.cron_restart = Some("0 * * * *".to_string());
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
                app.max_memory = Some(MemSize::from_bytes(1024));
            });
            handle.start(vec![app.clone()]).await.unwrap();

            registry.arm(
                &armed_entry(0, 0, 1000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );
            let group = &registry.groups["web"];
            assert_eq!(group.members, HashSet::from([0]));
            assert!(group.cron.is_some(), "the cron worker must have armed");
            assert!(group.watch.is_some(), "the watch must have armed");
            assert_eq!(rig.enforcer.arms().len(), 1);

            registry.disarm(0, "web");

            assert!(
                registry.groups.is_empty(),
                "a group whose only member left before its first occurrence must go with it"
            );
            assert!(
                registry.instances.is_empty(),
                "the same disarm must take the instance's own extras too"
            );
            assert_eq!(rig.enforcer.disarms(), vec![0]);
            assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
        }

        // The watch twin of the case above, and separate because the gate is two
        // independent conditions. The ending is forced with `abort` rather than
        // by killing the `WatchSource` the loop really returns on, which dies
        // with an OS thread no test reaches; both leave a finished handle.
        #[tokio::test]
        async fn a_watch_that_ended_on_its_own_is_rebuilt_on_the_next_arm() {
            let home = tempfile::tempdir().unwrap();
            let paths = test_paths(&home);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            let app = app_with("web", |app| {
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
                app.watch_delay = Some(UpDuration::from_millis(
                    real_time::TEST_DELAY.as_millis() as u64
                ));
            });
            handle.start(vec![app.clone()]).await.unwrap();

            registry.arm(
                &armed_entry(0, 0, 1000, app.clone(), &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );
            let armed = registry.groups["web"]
                .watch
                .as_ref()
                .expect("the first arm registers a watcher");
            armed.abort();
            settle_finished(armed).await;

            registry.arm(
                &armed_entry(0, 0, 2000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );

            touch(root.path(), "trigger.txt").unwrap();
            let info = expect_restart(&mut rx, "web", real_time::SMOKE_DEADLINE).await;
            assert_eq!(info.restarts, 1);
        }

        // An unresolved root fires never: on macOS a tempdir under `/var/...` is
        // delivered as `/private/var/...` and every `strip_prefix` fails.
        #[tokio::test]
        async fn a_watched_app_restarts_on_a_save_and_goes_quiet_once_disarmed() {
            let home = tempfile::tempdir().unwrap();
            let paths = test_paths(&home);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            let app = app_with("web", |app| {
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
                // From `watch::real_time`, the owner of this subsystem's
                // real-time constants.
                app.watch_delay = Some(UpDuration::from_millis(
                    real_time::TEST_DELAY.as_millis() as u64
                ));
            });
            handle.start(vec![app.clone()]).await.unwrap();
            registry.arm(
                &armed_entry(0, 0, 1000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );

            touch(root.path(), "trigger.txt").unwrap();
            let info = expect_restart(&mut rx, "web", real_time::SMOKE_DEADLINE).await;
            assert_eq!(info.restarts, 1);

            registry.disarm(0, "web");
            touch(root.path(), "after-disarm.txt").unwrap();
            assert_no_restart_within(&mut rx, "web", real_time::NO_EVENT_WINDOW).await;
        }

        // `DEFAULT_WATCH_DELAY` is 500ms, so any longer fallback leaves the save
        // below with no restart inside the deadline.
        #[tokio::test]
        async fn a_watched_app_naming_no_delay_still_restarts_on_a_save() {
            let home = tempfile::tempdir().unwrap();
            let paths = test_paths(&home);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            let app = app_with("web", |app| {
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
                // No `watch_delay`: this case exists for the default.
            });
            handle.start(vec![app.clone()]).await.unwrap();
            registry.arm(
                &armed_entry(0, 0, 1000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );

            touch(root.path(), "trigger.txt").unwrap();
            let info = expect_restart(&mut rx, "web", real_time::SMOKE_DEADLINE).await;
            assert_eq!(info.restarts, 1);
        }

        // The watch's own door into the claim the cron case makes, and separate
        // because the two subsystems pick their `SupervisorHandle` method
        // independently: through `restart`, an autosave is reported as a deploy.
        #[tokio::test]
        async fn a_watch_restart_is_not_reported_as_a_user_action() {
            let home = tempfile::tempdir().unwrap();
            let paths = test_paths(&home);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            let app = app_with("web", |app| {
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
                app.watch_delay = Some(UpDuration::from_millis(
                    real_time::TEST_DELAY.as_millis() as u64
                ));
            });
            handle.start(vec![app.clone()]).await.unwrap();
            registry.arm(
                &armed_entry(0, 0, 1000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );

            touch(root.path(), "trigger.txt").unwrap();

            let (info, manually) =
                expect_restart_event(&mut rx, "web", real_time::SMOKE_DEADLINE).await;
            assert_eq!(info.restarts, 1);
            assert!(
                !manually,
                "a file changing under a watched tree is not a user action"
            );
        }

        // A filter built from empty slices discards every ignore rule the user
        // wrote.
        #[tokio::test]
        async fn a_watched_app_ignores_the_paths_its_ignore_watch_names() {
            let home = tempfile::tempdir().unwrap();
            let paths = test_paths(&home);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            let app = app_with("web", |app| {
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
                app.ignore_watch = vec!["ignored.txt".to_string()];
                app.watch_delay = Some(UpDuration::from_millis(
                    real_time::TEST_DELAY.as_millis() as u64
                ));
            });
            handle.start(vec![app.clone()]).await.unwrap();
            registry.arm(
                &armed_entry(0, 0, 1000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );

            touch(root.path(), "ignored.txt").unwrap();
            assert_no_restart_within(&mut rx, "web", real_time::NO_EVENT_WINDOW).await;

            touch(root.path(), "trigger.txt").unwrap();
            let info = expect_restart(&mut rx, "web", real_time::SMOKE_DEADLINE).await;
            assert_eq!(info.restarts, 1);
        }

        // The default globs plus `ignore_watch` alone let an app naming an
        // explicit `out_file`/`err_file` under its own `cwd` restart on its own
        // log writes forever: the default log glob covers only a directory named
        // `logs`, and an automatic restart resets the budget.
        #[tokio::test]
        async fn a_watched_app_ignores_its_own_log_writes() {
            let home = tempfile::tempdir().unwrap();
            let paths = test_paths(&home);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            let app = app_with("web", |app| {
                app.watch = true;
                app.cwd = Some(root.path().display().to_string());
                // Absolute, under the watched tree, and named nothing like
                // `logs`: a shep write really does land inside the tree.
                app.out_file = Some(root.path().join("app-out.txt").display().to_string());
                app.err_file = Some(root.path().join("app-err.txt").display().to_string());
                app.watch_delay = Some(UpDuration::from_millis(
                    real_time::TEST_DELAY.as_millis() as u64
                ));
            });
            handle.start(vec![app.clone()]).await.unwrap();
            registry.arm(
                &armed_entry(0, 0, 1000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );

            touch(root.path(), "app-out.txt").unwrap();
            touch(root.path(), "app-err.txt").unwrap();
            assert_no_restart_within(&mut rx, "web", real_time::NO_EVENT_WINDOW).await;

            touch(root.path(), "trigger.txt").unwrap();
            let info = expect_restart(&mut rx, "web", real_time::SMOKE_DEADLINE).await;
            assert_eq!(info.restarts, 1);
        }

        // `probe_exec` runs `env_clear().envs(&self.env)` and `SHEP_INSTANCE` is
        // written by `assemble` alone, so a prober built from `config.env`, or
        // built once and shared, expands it to nothing and both instances report.
        // A file, not a port: `test -f` needs no listener and cannot race.
        #[cfg(unix)]
        #[tokio::test]
        async fn each_instances_liveness_probe_runs_with_its_own_assembled_env() {
            let markers = tempfile::tempdir().unwrap();
            // Instance 0's marker exists; instance 1's never will.
            std::fs::write(markers.path().join("live-0"), b"").unwrap();

            let mut h = harness(vec![ProcScript::never_exits(); 4]);
            h.ctx
                .supervisor
                .start(vec![app_with("web", |app| {
                    app.instances = 2;
                    app.liveness_probe = Some(ProbeConfig {
                        failure_threshold: 1,
                        interval: PROBE_INTERVAL,
                        timeout: UpDuration::from_millis(5_000),
                        ..probe_config(
                            ProbeKind::Exec,
                            &format!(
                                r#"test -f "{}/live-$SHEP_INSTANCE""#,
                                markers.path().display()
                            ),
                        )
                    });
                })])
                .await
                .unwrap();

            let listing = h.ctx.supervisor.list().await;
            // `ProcessInfo` carries no instance number, but the assembler's log
            // path does, from the same `assemble` call, so this pins which
            // instance id 1 is rather than assuming the allocation order.
            assert!(
                listing[1]
                    .out_file
                    .as_ref()
                    .is_some_and(|path| path.ends_with("web-1-out.log")),
                "id 1 must be instance 1: {:?}",
                listing[1].out_file
            );
            let instance_one_pid = listing[1].pid.expect("a live sheep has a pid");

            let failure = expect_liveness(&mut h.liveness, LIVENESS_DEADLINE).await;
            assert_eq!(
                failure,
                LivenessReport {
                    id: 1,
                    pid: instance_one_pid,
                    epoch: 1,
                },
                "only the instance whose own marker is missing may report"
            );
            // Both instances report under the bugs above and which arrives
            // first is a race, so the window catching the other is not optional.
            assert_no_liveness_within(&mut h.liveness, PROBE_INTERVAL.as_duration() * 3).await;
        }
    }
}
