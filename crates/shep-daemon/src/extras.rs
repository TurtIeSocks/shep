//! Lifecycle extras: what is armed when a sheep goes online, and what stops
//! when it goes terminal (spec §4, §7).
//!
//! Four subsystems run as free tasks beside the supervisor actor — the cron
//! worker ([`crate::cron`]), the memory-limit enforcer ([`crate::limits`]),
//! the liveness prober ([`crate::probes`]) and the filesystem watch
//! ([`crate::watch`]). [`ExtrasRegistry`] is what starts them and what stops
//! them, keyed the way each subsystem's own reach demands: cron and watch per
//! **name**, because both restart a whole name-group and would otherwise fire
//! N times for N instances; the enforcer and the liveness loop per **id**,
//! because each is armed against one pid.
//!
//! Disarming is the whole of what keeps a stopped sheep down. Neither a
//! triggering file save nor a cron occurrence filters by status, so a sheep
//! stays down because nothing is left armed for it — never because a trigger
//! declined to fire. The user-visible rule is one line: stopping a **name**
//! stops its watch. Not one instance of a name: `disarm` tears a group down
//! only when its LAST member leaves, so `shep stop web-1` with `web-2` still
//! up leaves the group's one watcher armed and the next save brings `web-1`
//! back. [`crate::watch`]'s module doc carries the full case for why that is
//! the accepted consequence of one watcher per name group rather than a gap.
//!
//! ## Reference
//!
//! - [`Extras`], [`ExtrasReports`] — the seams and the report wiring
//! - [`ExtrasRegistry`] — arming and disarming
//! - [`spawn_extras_reporter`] — turns a report into a guarded restart

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
/// `InstanceExtras::disarm` calls `liveness.abort()`, which does not await
/// the aborted task: a probe already inside `failures.send(..).await` when
/// it is replaced (a config-only re-arm, which changes neither the pid nor
/// the status the two older guards check) can still deliver its failure
/// after a fresh probe is already running against the same process. This
/// carries the epoch [`ExtrasRegistry::arm`] captured when THIS probe was
/// spawned, so `Actor::handle_extra_restart` can tell that stale failure
/// apart from a genuine one raised by the CURRENT probe, even though pid and
/// status agree on both.
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
/// Constructed once at boot and handed to the supervisor. Every *seam* is a
/// trait object so the engine's type does not grow a parameter per subsystem;
/// `reports` and `stats` are the exceptions and are wiring rather than seams
/// — the sampler underneath `stats` is where that subsystem's seam lives.
pub struct Extras {
    /// Wall clock the cron workers read.
    pub clock: Arc<dyn Clock>,
    /// Memory-limit mechanism.
    ///
    /// Shared rather than owned (`Arc`, not `Box`) because
    /// [`ExtrasRegistry::disarm`] has to reach it too, and its signature
    /// takes no [`Extras`]: the registry keeps a handle from the arming that
    /// needs undoing, so arming and disarming an id's memory limit stay in
    /// the one type that owns every other arm/disarm pair.
    pub enforcer: Arc<dyn LimitEnforcer>,
    /// Longest a cron worker parks before re-reading the clock, from
    /// `[daemon] max_cron_sleep`. Already defaulted: this is a value, not an
    /// option, because the layer that knew whether the user set anything is
    /// behind us.
    pub max_cron_sleep: Duration,
    /// Cloned once per arming. The enforcer swallowed its own breach sender at
    /// construction; the liveness loops are free tasks and cannot, so the
    /// sender has to reach [`ExtrasRegistry::arm`] through here.
    pub reports: ExtrasReports,
    /// Live resource readings, shared with the RPC layer so a listing can
    /// take one on demand.
    ///
    /// Shared rather than owned for the same reason [`Extras::enforcer`] is:
    /// [`ExtrasRegistry`] keeps a handle from the arming it will later have
    /// to undo. The enforcer's polling loop holds a third handle — it is what
    /// records the CPU baseline every on-demand reading measures against.
    pub stats: Arc<StatsState>,
}

impl Extras {
    /// The production wiring: system clock and polling enforcer over
    /// sysinfo.
    ///
    /// No prober: one is scoped to a single sheep's assembled environment,
    /// and boot has no sheep in scope.
    ///
    /// Must be called from within a Tokio runtime context: constructing the
    /// polling enforcer starts its sampling task immediately.
    #[must_use]
    pub fn real(reports: ExtrasReports, max_cron_sleep: Duration) -> Self {
        // One sampler behind both consumers, not two: sampling and
        // enforcement read the same process table on the same tick, and a
        // second `SysinfoSampler` would mean a second syscall walk.
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
    // Roles, not values, for the seams: neither is Debug, and printing the
    // report channels would say nothing a reader wants. `stats` is omitted
    // for the same reason as `reports` — it is wiring, and its own Debug
    // prints a role and nothing else. The sleep bound is the exception and
    // prints for real: it is a tuning knob the user set, so a daemon log that
    // dumps this struct should say what it ended up being.
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
/// report already queued when the user runs `shep stop` is delivered *after*
/// the sheep is `Stopped`, and `restart` would resurrect a process the user
/// explicitly stopped.
///
/// Both arms write a `warn!` on the way past, and for a breach that record is
/// the ONLY place the observed RSS and the limit it crossed are ever stated —
/// the bus event says `restart` and never why. Both reach a reader: the
/// binary installs a subscriber whose default level is `warn`, so a user
/// watching a process get restarted over and over finds the reason in
/// `$SHEP_HOME/logs/shepd.err.log`. `[daemon] log_level = "error"` is what
/// takes that away again.
///
/// Must be called from within a Tokio runtime context: it spawns the
/// reporting task immediately.
pub fn spawn_extras_reporter(
    mut breaches: mpsc::Receiver<LimitBreach>,
    mut liveness: mpsc::Receiver<LivenessReport>,
    supervisor: SupervisorHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Per-channel open flags, and a `select!` branch guard on each: a
        // closed `mpsc::Receiver` resolves to `None` on every subsequent
        // poll, so a branch left in consideration after its channel ended
        // would busy-spin the loop instead of falling out of it. Same shape,
        // and same reason, as `run_sheep`'s channel guards.
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
                        // No epoch to carry: a memory breach has nothing
                        // equivalent to a probe task that can be aborted
                        // mid-`send`. What it carries instead is the size it
                        // measured, which is its own staleness token -- the
                        // breach was computed under `PollingEnforcer`'s lock
                        // and is sent after that lock is released, so a
                        // ceiling re-armed in between leaves this report
                        // speaking for a limit nobody enforces any more. The
                        // actor re-asks the question against the ceiling in
                        // force when it arrives.
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
    /// One name-group's per-name tasks. Present only while at least one
    /// instance whose *configuration* asks for a per-name extra is armed, so
    /// a flock of apps configuring neither `cron_restart` nor `watch` leaves
    /// this map empty. Keyed on the configuration rather than on what an
    /// arming managed to build — see [`ExtrasRegistry::arm`].
    groups: HashMap<String, NameExtras>,
    /// One instance's per-pid extras, keyed by sheep id. Present only while
    /// at least one of them is armed.
    instances: HashMap<u32, InstanceExtras>,
    /// The epoch each id's liveness probe is CURRENTLY armed under, bumped
    /// every time [`Self::arm`] (re)arms an id, whether or not that id
    /// configures a `liveness_probe` at all, so an app that adds one later
    /// never inherits a stale count from before it did.
    ///
    /// Deliberately not the supervisor's own private `SheepSlot::epoch`: that
    /// one is bumped on a RESPAWN, when the pid changes, which is exactly
    /// the case `Actor::handle_extra_restart`'s existing pid guard already
    /// catches. This counter answers a narrower question a pid check
    /// cannot: has THIS id's liveness probe been replaced without the
    /// process underneath it changing at all, the case a config-only re-arm
    /// produces. Overloading the respawn epoch for that would either bump it
    /// on every config change too (a respawn-generation counter moving
    /// without a respawn) or leave a config-only re-arm unable to move it at
    /// all, so this is a second counter rather than a second use of the
    /// first.
    ///
    /// Lives on the registry, not on `SheepSlot`, because that is the one
    /// type that already knows when a liveness probe is actually replaced:
    /// `SheepSlot` has no visibility into an arming `ExtrasRegistry::arm`
    /// performs, and `rearm_name` (this registry's own config-only re-arm)
    /// has no visibility into `SheepSlot` the other way. Keeping the counter
    /// where the arming decision is made avoids two structs that would
    /// otherwise need to agree on it.
    liveness_epochs: HashMap<u32, u64>,
}

/// One name-group's per-name tasks, plus the armed instances keeping them
/// alive.
///
/// Either task may be `None` while the group still exists: an app that
/// configures a watch but whose watch could not be registered is a member of
/// its group all the same, and the next arming of the name retries it.
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
    /// Aborts both per-name tasks. Takes `self`: a group is torn down once
    /// and then gone, so there is no half-aborted state to represent.
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
    /// Where this id's sampling was started, so `disarm` can stop it. Not an
    /// `Option`: every sheep with a pid is sampled, so an instance that
    /// reached this type is a watched one by construction.
    stats: Arc<StatsState>,
    /// The enforcer this id's memory limit was armed against, so `disarm` can
    /// undo that arming — see [`Extras::enforcer`] for why the registry holds
    /// a handle rather than being handed one.
    limit: Option<Arc<dyn LimitEnforcer>>,
    /// The liveness loop, when the app configures `liveness_probe`.
    liveness: Option<JoinHandle<()>>,
}

impl InstanceExtras {
    /// Sampling armed and nothing else — what an app configuring neither
    /// `max_memory` nor `liveness_probe` gets, which is most of them.
    fn watched_only(stats: Arc<StatsState>) -> Self {
        Self {
            stats,
            limit: None,
            liveness: None,
        }
    }

    /// Undoes this instance's arming: sampling, the memory limit against
    /// `id`, and the liveness loop. Takes `self` for the same reason
    /// [`NameExtras::abort`] does.
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
    // `Arc<dyn LimitEnforcer>` is not Debug — and the useful fact about it
    // here is that an arming exists, not which mechanism performed it.
    // `stats` is omitted rather than redacted: it is armed for every instance
    // that exists, so printing it would print a constant. Hence
    // `finish_non_exhaustive` — a field really is being left out.
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
    /// `prober` is scoped to this instance's assembled `SpawnSpec` and is
    /// read only by the liveness loop; an entry configuring no
    /// `liveness_probe` never touches it.
    ///
    /// Idempotent per id: arming an already-armed id disarms that id's own
    /// per-pid extras first, which is what a respawn needs — the new process
    /// has a new pid. A *live* name-group task is deliberately left alone on a
    /// re-arm: it is keyed on the name, it outlives any one process, and
    /// rebuilding the watch on every restart would re-register the OS watcher
    /// each time — a step that can fail, and whose failure would silently cost
    /// an app the watch that restarted it.
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
        // Bumped unconditionally, ahead of `arm_instance` even for an entry
        // configuring no `liveness_probe` at all; see
        // `Self::liveness_epochs`'s doc for why this lives here rather than
        // on `SheepSlot`. `arm_instance` only ever reads it for a probe it is
        // about to spawn, so an id that never arms a probe pays nothing for
        // an epoch nothing consults.
        let liveness_epoch = self.liveness_epochs.entry(id).or_insert(0);
        *liveness_epoch += 1;
        let liveness_epoch = *liveness_epoch;
        if let Some(instance) = arm_instance(entry, prober, extras, liveness_epoch) {
            self.instances.insert(id, instance);
        }

        // Group membership is decided by the CONFIGURATION, never by whether
        // this arming managed to build a task. An `arm_watch` that failed
        // transiently — inotify's `max_user_watches` exhausted, say — would
        // otherwise leave a still-online instance out of its own group, and
        // stopping some LATER instance whose arm did succeed would tear the
        // watch down while the earlier one is still running.
        if config.cron_restart.is_none() && !config.watch {
            return;
        }
        let group = self.groups.entry(config.name.clone()).or_default();
        // A second instance of a name joins the group rather than arming a
        // second worker, since both triggers already reach every instance of
        // the name.
        group.members.insert(id);
        // Presence in the map is NOT the test, because a task can end on its
        // own: a cron worker returns on a pattern with no further occurrence,
        // and the watch loop returns when its `WatchSource` dies. Either
        // leaves a finished handle behind, and without this the app would have
        // silently lost its schedule or its watch with nothing left to rebuild
        // it. `arm_cron`/`arm_watch` each return `None` for an app that asked
        // for nothing, so the app that configured only one of the two pays
        // nothing for the other's call.
        if group.cron.as_ref().is_none_or(JoinHandle::is_finished) {
            group.cron = arm_cron(config, extras, supervisor);
        }
        // An app whose watch can NEVER arm — a cwd that will never resolve —
        // pays a fresh `canonicalize`, a fresh globset compile and a fresh
        // `warn!` on every re-arm, rather than being attempted once and then
        // left alone. That is the price of retrying the transient failures
        // (`max_user_watches` exhausted, a cwd not yet created) that the
        // rebuild exists for, and it is bounded by `max_restarts`; permanent
        // failure gets no dedupe on purpose, because telling the two apart
        // means keeping per-name failure state that would then need its own
        // invalidation. Deliberate, not an oversight.
        if group.watch.as_ref().is_none_or(JoinHandle::is_finished) {
            group.watch = arm_watch(entry, supervisor);
        }
    }

    /// Aborts everything armed for `id`, and both of the name-group's
    /// per-name tasks — its watch and its cron worker — when this was the
    /// last armed instance of the name.
    ///
    /// This is what stops a stopped sheep from being restarted by a file save
    /// or a schedule: neither trigger filters by status, so a sheep stays down
    /// because nothing is left armed for it, not because something declined
    /// to restart it.
    ///
    /// Aborting the watch-group handle is sufficient to stop the OS watch:
    /// the debouncer guard rides inside the aborted future, so no second drop
    /// is needed and none is available.
    pub fn disarm(&mut self, id: u32, name: &str) {
        self.disarm_instance(id);
        // Not inside `disarm_instance`: `Self::arm` calls that first and
        // would otherwise reset the very counter it is about to bump,
        // pinning every id's epoch at 1 forever and defeating the whole
        // point of it. This is the FULL teardown (a sheep going terminal),
        // so nothing left in `self.sheep` will ever compare against `id`
        // again; removing it here is hygiene against an unbounded map on a
        // daemon that runs for months, not a correctness requirement.
        self.liveness_epochs.remove(&id);

        let Some(group) = self.groups.get_mut(name) else {
            return;
        };
        // Both halves matter. An id that was never a member leaves the group
        // untouched (disarming an unknown id is a no-op, not a teardown of
        // somebody else's watcher), and a group with instances left standing
        // keeps its tasks.
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
    /// [`Self::arm`] deliberately preserves a running cron or watch task, so
    /// that a reload's replacement instance arming before the drainee disarms
    /// does not tear down a watcher the drainee still needs. That is right for
    /// the transition it was written for and wrong for a config change: the
    /// group-scoped fields (`watch`, `ignore_watch`, `watch_delay`,
    /// `watch_options`) and the cron ones (`cron_restart`, `cron_timezone`)
    /// are read when the task is built, so a task that survives keeps the old
    /// values for as long as it lives.
    ///
    /// Takes every entry of the name rather than one id, because the group is
    /// per-name: disarming a single instance of a multi-instance app leaves
    /// the group standing, by design.
    ///
    /// # What this loses
    ///
    /// The OS watch is torn down and rebuilt with a real gap and no rescan, so
    /// a file saved during it is missed. Same gap any watcher restart has.
    /// `stats.watch()` clears the pid's CPU baseline, so `shep flock` shows a
    /// blank CPU cell for one poll interval. Both are documented rather than
    /// closed; see the design spec.
    // `extras` is `pub(crate)`, so an unconsumed method here reads as dead
    // code to a plain `cargo build`/`clippy`, unlike a genuinely public
    // crate's API surface. Its own tests are the only caller for now, ahead
    // of a later task in the same slice wiring it into the supervisor's
    // config-apply path. `#[allow(dead_code)]` names that pre-wiring state
    // explicitly, in a plain comment rather than the rustdoc above, so
    // deleting the attribute when that task lands takes this note with it
    // instead of leaving a doc comment claiming nobody calls this.
    #[allow(dead_code)]
    pub fn rearm_name(
        &mut self,
        name: &str,
        entries: &[&ProcessEntry],
        prober: Arc<dyn Prober>,
        extras: &Extras,
        supervisor: &SupervisorHandle,
    ) {
        // Abort the group's own tasks before rebuilding, which is the whole
        // difference from `arm`. Removing the entry rather than mutating it
        // means the rebuild below takes `arm`'s own "no task yet" path, so
        // there is one construction site rather than two.
        if let Some(group) = self.groups.remove(name) {
            group.abort();
        }
        for entry in entries {
            self.arm(entry, Arc::clone(&prober), extras, supervisor);
        }
    }

    /// Undoes one instance's per-pid arming: the memory limit and the
    /// liveness loop. A no-op for an id that had neither.
    fn disarm_instance(&mut self, id: u32) {
        if let Some(instance) = self.instances.remove(&id) {
            instance.disarm(id);
        }
    }

    /// The epoch `id`'s liveness probe is CURRENTLY armed under, or `0` for
    /// an id that has never been armed. `Actor::handle_extra_restart` drops a
    /// [`LivenessReport`] whose own epoch does not match this, which is what
    /// tells a failure raised by a since-replaced probe apart from one raised
    /// by the probe running now.
    pub(crate) fn liveness_epoch(&self, id: u32) -> u64 {
        self.liveness_epochs.get(&id).copied().unwrap_or(0)
    }
}

impl Drop for ExtrasRegistry {
    // Nothing armed outlives the registry. This is the teardown rather than a
    // disarm loop in `begin_shutdown` because it covers BOTH ways the actor
    // can end, and a future transition cannot forget it:
    //
    // - A graceful shutdown only kills sheep whose `ctl.is_some()`. A
    //   `WaitingRestart` sheep has none, never exits, and so never reaches
    //   `handle_exited`'s terminal branches — its liveness loop, its enforcer
    //   arming and its name-group's cron worker and watch would all survive
    //   the actor.
    // - An actor that panics never runs `begin_shutdown` at all.
    //
    // A `JoinHandle` merely dropped DETACHES its task rather than cancelling
    // it, so every one of them has to be aborted by hand. Cron and watch do
    // eventually notice `EngineStopped` and return, but only at their next
    // iteration: the watch parks on its debounce channel holding the OS
    // watcher until some unrelated file changes, and a liveness loop keeps
    // spawning a real `sh` per interval until its threshold — and while any
    // one of them lives it holds a report sender, so the reporting task never
    // ends either.
    fn drop(&mut self) {
        for (id, instance) in self.instances.drain() {
            instance.disarm(id);
        }
        for (_name, group) in self.groups.drain() {
            group.abort();
        }
    }
}

/// Arms the per-pid extras — sampling always, the memory limit and the
/// liveness loop where the app configures them — returning `None` only when
/// the entry has no pid to arm them against.
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
            // Not reachable from the transition this is called at (a sheep is
            // Online only with a live pid), and not a panic either: both
            // extras are armed against a pid, so there is nothing honest to
            // arm without one.
            tracing::warn!(
                id = entry.id,
                "arming a sheep with no pid; its memory limit and liveness probe stay disarmed"
            );
        }
        return None;
    };

    // Unconditional, unlike the two below it: a listing reports CPU and
    // memory for every sheep, and an app that configures no `max_memory` is
    // the ordinary case rather than an opted-out one.
    extras.stats.watch(entry.id, pid);
    let mut instance = InstanceExtras::watched_only(Arc::clone(&extras.stats));
    if let Some(limit) = config.max_memory {
        extras.enforcer.arm(entry.id, pid, limit);
        instance.limit = Some(Arc::clone(&extras.enforcer));
    }
    if let Some(probe) = config.liveness_probe.as_ref() {
        match ProbeTarget::parse(probe) {
            Ok(target) => {
                // `spawn_liveness_task` only ever knows `LivenessFailure`;
                // it lives in `probes`, which has no business knowing about
                // an epoch this module invented. So the probe reports into a
                // private one-shot channel instead of `extras.reports`
                // directly, and this relay tags the single failure it may
                // ever produce (the probe's own doc: it reports once, then
                // ends) with the epoch THIS probe was spawned under, before
                // forwarding it on to the shared reporter. Capturing the
                // epoch here rather than reading it back off the registry at
                // delivery time is the whole point: by delivery time a
                // config-only re-arm may already have moved it on.
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
                // that reached the daemon cannot land here — swallowed instead
                // of `expect`-ed so a future path that skips normalization
                // costs one app its liveness probe rather than the daemon.
                // The `warn!` below is the whole of the visible sign: the app
                // still comes up online with no liveness probe, and nothing
                // in its status or on the bus says it ever asked for one.
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
/// `cron_restart` (or names a pattern that will not parse).
///
/// An unparseable pattern costs the app its schedule, and the `warn!` below is
/// the only record that a `cron_restart` was asked for and dropped — the app
/// comes up `online` either way, and no status or bus event carries it.
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
    // `max_cron_sleep` rides on `Extras` rather than being read off the app:
    // it tunes how the shepherd wakes up, not how any one app behaves.
    Some(spawn_cron_worker(
        config.name.clone(),
        schedule,
        Arc::clone(&extras.clock),
        supervisor.clone(),
        extras.max_cron_sleep,
    ))
}

/// Spawns the name-group's filesystem watch, or `None` when the app does not
/// ask to be watched (or when its root or its globs will not resolve).
///
/// Every failure here arms no watch rather than propagating: a watch root that
/// will not resolve must not take down the same app's cron worker, enforcer
/// and probe. Each one writes a `warn!` on the way out, and that record is the
/// entire signal: the app comes up `online` with no watch, and neither its
/// status nor the bus says a watch was ever asked for. See
/// `a_watch_root_that_will_not_resolve_says_in_the_log_which_app_lost_its_watch`
/// for the arm that is pinned as a contract rather than a promise.
///
/// Takes the whole [`ProcessEntry`] rather than its config because the
/// assembled `out_file`/`err_file` are what `own_log_ignores` needs, and the
/// entry is the only place this arming can read the paths the sheep's child is
/// really writing to without re-deriving them.
fn arm_watch(entry: &ProcessEntry, supervisor: &SupervisorHandle) -> Option<JoinHandle<()>> {
    let config = entry.spec.config();
    if !config.watch {
        return None;
    }
    let Some(cwd) = config.cwd.as_deref() else {
        // `normalize` rejects `watch = true` with no `cwd`, so this is
        // unreachable for a config that came through it — and the daemon's
        // own working directory is not an acceptable fallback (a systemd unit
        // would watch `/`).
        tracing::warn!(
            name = config.name.as_str(),
            "watch is on but the app names no cwd; arming no watch"
        );
        return None;
    };
    // Canonicalized, not merely absolute: the group loop matches by stripping
    // this prefix off the absolute paths notify delivers, and on macOS a
    // directory under `/var/...` arrives from FSEvents as `/private/var/...`.
    // Without this, every strip fails and the watch fires never.
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
    // the app pointed back inside the tree it asked to have watched. See
    // `own_log_ignores` for why the default globs cannot cover that.
    let mut ignores = config.ignore_watch.clone();
    ignores.extend(own_log_ignores(
        &root,
        [entry.out_file.as_path(), entry.err_file.as_path()],
    ));
    let filter = match WatchFilter::new(&config.watch_options, &ignores) {
        Ok(filter) => filter,
        Err(err) => {
            // `normalize` compiles every `watch_options` and `ignore_watch`
            // pattern, so a config that reached the daemon cannot land here —
            // reported instead of `expect`-ed so a future path that skips
            // normalization costs one app its watch rather than the daemon.
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

/// The debounce window an app's watch is armed with: its own `watch_delay`
/// when it set one, [`DEFAULT_WATCH_DELAY`] otherwise, floored either way at
/// [`MIN_WATCH_DELAY`].
///
/// The floor is a last line of defence, not the first: `shep-core`'s
/// `normalize` already refuses `watch_delay = "0"`, and every value it accepts
/// passes through here unchanged. See [`MIN_WATCH_DELAY`] for why it is a
/// single millisecond where its two siblings are a full second.
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

    /// Generous bound on how long a test may wait for something on the
    /// (paused) tokio clock. Costs no real wall-clock time: the paused
    /// runtime auto-advances to this deadline only if nothing else becomes
    /// ready first.
    const EVENT_WAIT: Duration = Duration::from_secs(120);

    /// A window that spans a whole hourly cron occurrence and then some, for
    /// the negative halves: crossing the occurrence and making the claim are
    /// the same call, per Global Constraints rule 11. A `try_recv` after an
    /// `advance` would read empty whether or not the worker was ever aborted.
    const PAST_THE_NEXT_OCCURRENCE: Duration = Duration::from_secs(3_700);

    /// How long a real-clock test waits for a liveness report that should
    /// arrive. Generous enough that a loaded runner cannot turn a genuine
    /// pass into a flake.
    const LIVENESS_DEADLINE: Duration = Duration::from_secs(10);

    /// The shortest interval `spawn_liveness_task` honours — anything smaller
    /// is floored there, so a fixture naming a smaller number would be a lie
    /// about what its test waits for. Declared as a literal rather than
    /// imported because `probes`' own floor is private to that module.
    const PROBE_INTERVAL: UpDuration = UpDuration::from_millis(1_000);

    fn dt(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid RFC3339 timestamp")
    }

    /// The registry-tier fixture: a paused-clock-driven wall clock, a
    /// recording enforcer, and both report receivers held by the test rather
    /// than by a reporter (there is none — a registry test asserts the report
    /// itself, never a restart it did not trigger).
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

    /// One supervisor engine over a scripted runner with plenty of
    /// `never_exits` procs — enough for the initial starts plus every restart
    /// a *broken* implementation could produce, so no negative assertion here
    /// can pass merely because the script ran out (an exhausted script makes
    /// the supervisor emit `Errored`, not `Restart`).
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

    /// A prober that never fails — the neutral value for a case that arms no
    /// liveness probe but must still hand `arm` one.
    fn idle_prober() -> Arc<dyn Prober> {
        Arc::new(ScriptedProber::new(vec![]))
    }

    /// A prober that fails every probe — a liveness loop armed with one
    /// reports as soon as it has taken `failure_threshold` samples, so a case
    /// asserting SILENCE against one is asserting the loop is gone.
    fn failing_prober() -> Arc<dyn Prober> {
        Arc::new(ScriptedProber::new(vec![Err(ProbeFailure::Timeout)]))
    }

    /// Yields until `task` reports finished, panicking if it never does.
    /// Yielding rather than advancing: a worker that ends does so on its
    /// first poll, and an `advance` would resolve unrelated timers with it.
    async fn settle_finished(task: &JoinHandle<()>) {
        for _ in 0..100 {
            if task.is_finished() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the task never finished");
    }

    /// Waits up to `window` for a `Restart` naming `name`, panicking if none
    /// arrives.
    async fn expect_restart(
        rx: &mut broadcast::Receiver<SharedEvent>,
        name: &str,
        window: Duration,
    ) -> ProcessInfo {
        expect_restart_event(rx, name, window).await.0
    }

    /// [`expect_restart`], plus the `manually` flag the bus put on that
    /// restart — [`BusEvent::Process`]'s own claim about who caused it ("True
    /// when a user action caused it").
    ///
    /// Split out rather than folded into `expect_restart`'s return type
    /// because only the handful of cases below read the flag, and every other
    /// caller wants the snapshot alone.
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

    /// Waits up to `window` for a `Restart` naming `name`, panicking if one
    /// arrives.
    ///
    /// A bounded `timeout` + `recv`, never a bare `try_recv` (Global
    /// Constraints rule 11): the window is what carries the paused clock past
    /// the occurrence the abort was supposed to stop, so the same call both
    /// crosses it and makes the claim.
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
                Err(_) => return, // window elapsed with nothing matching — expected
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
                // A negative assertion cannot skip events: the ones the
                // broadcast channel dropped may include the very `Restart`
                // this forbids, so continuing here would return success on an
                // overflow. `expect_restart` may skip them safely — the worst
                // a lag costs it is a timeout — but this one has to fail
                // loudly instead of failing open. Same fix, same reason, as
                // `watch`'s own copy of this helper.
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

    /// Waits up to `window` for a liveness failure, panicking if one arrives.
    /// A bounded `timeout` + `recv` for the same reason as
    /// [`assert_no_restart_within`].
    async fn assert_no_liveness_within(rx: &mut mpsc::Receiver<LivenessReport>, window: Duration) {
        match tokio::time::timeout(window, rx.recv()).await {
            Err(_) => {} // window elapsed with nothing arriving — expected
            Ok(Some(failure)) => panic!("unexpected liveness failure observed: {failure:?}"),
            Ok(None) => panic!("the liveness channel disconnected while checking for silence"),
        }
    }

    /// Crosses one hourly occurrence in steps far finer than either sleep cap
    /// under discussion, so the worker's own cadence — not the test's —
    /// decides how often it wakes. A single `advance(3600s)` would resolve
    /// whatever sleep is pending in one shot regardless.
    async fn cross_one_hour() {
        for _ in 0..120 {
            tokio::time::advance(Duration::from_secs(30)).await;
        }
    }

    // ------------------------------------------------------------------
    // The registry: what gets armed, and what stops.
    // ------------------------------------------------------------------

    // `Extras`' Debug is hand-rolled because neither seam is Debug, and it
    // deliberately prints ROLES for the seams while printing `max_cron_sleep`
    // for real — that field is a knob the user set, so a daemon log dumping
    // this struct should say what it ended up being. Pinned as an exact string
    // (IR-41): fails if a later edit starts printing a channel's innards, or
    // quietly stops printing the one field that carries a real value.
    #[tokio::test(start_paused = true)]
    async fn extras_debug_names_the_seams_by_role_and_the_sleep_bound_by_value() {
        let rig = rig(Duration::from_secs(300));
        assert_eq!(
            format!("{:?}", rig.extras),
            r#"Extras { clock: "<dyn Clock>", enforcer: "<dyn LimitEnforcer>", max_cron_sleep: 300s, .. }"#
        );
    }

    // `InstanceExtras`' Debug is hand-rolled for the same reason `Extras`' is:
    // `Arc<dyn LimitEnforcer>` is not Debug, and the fact worth printing is
    // that an arming exists rather than which mechanism performed it. Pinned
    // as an exact string in both directions (IR-41).
    //
    // Both halves are load-bearing. `limit_armed: true` alone cannot tell a
    // correct `self.limit.is_some()` from a hardcoded `true`, and
    // `limit_armed: false` alone cannot tell it from a hardcoded `false`; the
    // inversion to `is_none()` moves the boolean either way, so it takes an
    // armed instance AND an unarmed one to say the field reports its own
    // field. The unarmed half comes from an app configuring no `max_memory`,
    // which — since sampling is armed for every sheep — is a real registry
    // entry rather than a value only a fixture could build.
    //
    // The trailing `..` is `stats`, which every instance carries and which
    // therefore prints nothing a reader could act on. That it stays out of
    // the string is part of the claim: a Debug that started printing an `Arc`
    // address would change this.
    //
    // The liveness loop is left unarmed on purpose. Its field is a derived
    // `Option<JoinHandle<()>>`, and a live one renders as
    // `Some(JoinHandle { id: Id(2) })` — a tokio-internal task counter that
    // shifts with every task this fixture happens to spawn first, so pinning
    // it would pin tokio's numbering rather than this crate's redaction.
    //
    // fails if the struct is renamed out from under its `debug_struct` (a
    // daemon log naming a type nobody can grep for), and fails if
    // `limit_armed` reports anything other than whether a limit is armed.
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

    // `Extras::real` is the production wiring and the only constructor `boot`
    // calls, so nothing else in this crate would notice it handing the
    // enforcer a channel of its own (`mpsc::channel(1).0` compiles and reports
    // into the void). Real sysinfo over this very test process, whose RSS is
    // comfortably over one byte, on the paused clock the polling loop sleeps
    // on: the breach has to come back out of the sender this test kept.
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

    // fails if `arm` stops gating its per-name work on the configuration —
    // the shape that hands every app in the flock a group, and, with
    // `arm_watch`'s own guard gone too, a watcher on its own cwd. The cwd is
    // a real directory rather than absent so this app is one a watcher COULD
    // be registered on, instead of one that structurally cannot reach that
    // far. The positive cases below are what make the rest able to fail: they
    // prove both maps really do fill.
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
        // Sampling is the exception, and deliberately so: it is armed for
        // every sheep with a pid, so this app has an `InstanceExtras`
        // carrying nothing else. `every_sheep_with_a_pid_is_watched_even_
        // with_no_limit_and_no_probe` is where that half is asserted.
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

    // fails if only limit-carrying sheep get watched. An app with no
    // `max_memory` is the ordinary case, and a listing reporting `-` for
    // every one of them is the bug this split exists to fix. The disarm at
    // the end is the other half: a watch that is never dropped samples a dead
    // pid forever, and hands its CPU baseline to whatever process the OS next
    // gives that number to.
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

    // fails if `arm_watch` stops consulting `config.watch`. A cron-restarting
    // app is a member of its name group whatever it thinks of watching, so it
    // is the one shape that reaches `arm_watch` without asking to be watched —
    // and its cwd is a real directory, so a watcher really would be registered
    // on it.
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

    // fails if `cron_timezone` is dropped on the way to `CronSchedule::parse`.
    // `Etc/GMT+5` is UTC MINUS five (POSIX inverts the sign), so 05:00 local
    // is 10:00Z — while the same pattern read as UTC fires at 05:00Z, five
    // hours inside the silent window below.
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

    // fails if a watch root that will not resolve takes down the arm path for
    // the same app's cron worker — the whole reason `arm_watch` reports its
    // failures instead of propagating them.
    //
    // A mistyped glob cannot trigger this case: `normalize` compiles both
    // glob lists, so `app_with` would reject such a config before this test
    // could arm it. An unresolvable cwd is the config-shaped watch failure
    // that survives normalization.
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

    // `arm_watch` arms nothing on every failure and lets the sheep go `online`
    // regardless, so its `warn!` is the ENTIRE observable difference between
    // an app that is being watched and one that asked to be and is not. The
    // case above pins that the watch is absent; this one pins that the daemon
    // says so, which is the half an operator can actually act on.
    //
    // The app's name is deliberately unlike anything else in the record — the
    // message, the target and the rendered `io::Error` all lack it — so
    // `name="unwatchable"` can only have come from the field.
    //
    // fails if the unresolvable-root arm stops writing its record (the app
    // comes back to being unwatched in silence), and fails if the record drops
    // the `name` field, which on a flock of twelve is the difference between a
    // fault an operator can fix and one they can only observe.
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

    // fails if a cron worker is armed per instance rather than per name. Two
    // workers on the same schedule read the wall clock about twice as often
    // as one, which is the only trace a second worker leaves: the bus cannot
    // tell them apart, because two `restart(Name)` commands racing the same
    // sheep are collapsed into one by the actor's own first-command-wins
    // dedupe. The bound matches `cron`'s own `ten_minute_cap` case, which
    // pins a single worker on this exact configuration at well under 20.
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
            // reads close to now — `advance` jumps first and polls after.
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

    // fails if `arm` hands the enforcer the sheep's id where its pid belongs,
    // and fails if a re-arm keeps the first arming — the "arms once and never
    // updates" bug, which leaves a respawned sheep's limit enforced against
    // the process it replaced.
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

    // fails two ways, and needs both halves to be able to fail at all: a
    // disarm that tears the group down while another instance is still armed
    // (no restart would arrive after the first disarm), and a disarm that
    // leaves the worker running after the LAST instance leaves (a restart
    // would arrive after the second). A bare "nothing after the last disarm"
    // assertion passes just as happily against a worker that never fired.
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
        // Two instances are online, and `ProcessSelector::Name` reaches both,
        // so one occurrence produces two `Restart` events. Both are drained
        // before the claim below: a leftover would be indistinguishable from
        // a worker that outlived its disarm.
        expect_restart(&mut rx, "web", EVENT_WAIT).await;
        expect_restart(&mut rx, "web", EVENT_WAIT).await;

        registry.disarm(1, "web");
        assert!(
            !registry.groups.contains_key("web"),
            "the last instance leaving must take the group with it"
        );
        assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
    }

    // fails if `disarm` reads "this id was not a member" as "the last member
    // just left" and tears down a group every one of whose instances is still
    // armed — and fails if it panics on a name it never saw. The restart at
    // the end is what proves the group really survived, rather than merely
    // still having a map entry.
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

    // fails if `arm`'s per-name work keys on the group's map ENTRY existing
    // rather than on its tasks being alive. A cron worker that ended on its
    // own leaves a finished handle behind, and an `arm` that returns on map
    // presence alone never rebuilds it — the app silently loses its schedule
    // with nothing left to notice. The clock is what makes the claim: a fresh
    // worker on this pattern reads it exactly once before returning, so a
    // second reading means a second worker really was spawned.
    #[tokio::test(start_paused = true)]
    async fn a_cron_worker_that_ended_on_its_own_is_rebuilt_on_the_next_arm() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        // 30 February: a pattern croner parses and then finds no occurrence
        // for, which is the `Ok(None)` arm `spawn_cron_worker` returns on.
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

    // fails if group membership is recorded from what an arming managed to
    // BUILD rather than from what the app CONFIGURED. This app asks for a
    // watch and gets none — its cwd does not resolve — so under the
    // build-keyed reading the group does not exist and this instance is in no
    // group at all; a later instance of the same name whose watch DID arm
    // would then own the group alone, and stopping it would tear the watch
    // down with this one still online.
    //
    // The enforcer assertion is the other half: a watch that could not be
    // armed costs this app its watch and nothing else.
    #[tokio::test(start_paused = true)]
    async fn an_instance_whose_watch_could_not_be_armed_still_joins_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (handle, _rx, _fixture) = spawn_test_fixture();
        let rig = rig(DEFAULT_MAX_CRON_SLEEP);
        let mut registry = ExtrasRegistry::default();
        let limit = MemSize::from_bytes(500);
        // `normalize` rejects `watch = true` with no cwd and compiles both
        // glob lists, but never checks that the cwd it names resolves — so
        // `canonicalize` failing is the one watch-arming failure a config
        // that came through it can reach.
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

    // fails if `ExtrasRegistry` has no `Drop` that aborts what it armed — the
    // teardown a shutdown depends on, since `begin_shutdown` only kills sheep
    // holding a live task and a `WaitingRestart` sheep holds none. A dropped
    // `JoinHandle` detaches its task rather than cancelling it, so the loop
    // would outlive the actor and keep probing.
    //
    // The kept registry is the control: it proves these loops really do
    // report, so the silence demanded of the dropped one could have failed.
    // Both orderings are caught — under a missing `Drop` BOTH report, and
    // whichever lands first fails one of the two assertions.
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

    // fails if `Drop` aborts the per-instance extras and leaves the name-group
    // tasks running (`group.abort()` replaced by a discard). This is the half
    // the whole `Drop` argument rests on: a `WaitingRestart` sheep holds no
    // live task, so `begin_shutdown` never kills it and `handle_exited` never
    // runs its terminal branches — its name group's cron worker outlives the
    // actor. A dropped `JoinHandle` detaches its task rather than cancelling
    // it, so that worker goes on restarting a name whose engine is gone.
    //
    // Two NAMES, not two instances of one: the bus attributes a restart to a
    // name and never to the registry that armed the worker, so the control
    // and the subject have to be tellable apart on the wire. `kept` is that
    // control — it proves a worker on this very schedule really does fire, so
    // the silence demanded of `dropped` could have failed.
    //
    // Twelve scripts against six spawns at worst: two starts, `kept`'s two
    // occurrences, and — under the broken implementation — `dropped`'s two.
    // The surviving worker needs something to respawn from, since an
    // exhausted script makes the supervisor emit `Errored` rather than the
    // `Restart` the negative helper matches on.
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
        // reads close to now — `advance` jumps first and polls after.
        tokio::task::yield_now().await;
        drop(discarded);

        cross_one_hour().await;
        expect_restart(&mut rx, "kept", EVENT_WAIT).await;
        assert_no_restart_within(&mut rx, "dropped", PAST_THE_NEXT_OCCURRENCE).await;
    }

    // fails if `disarm` does not abort the liveness loop it armed. A healthy
    // loop never ends on its own — it returns only after reporting — so a
    // stopped or deleted sheep would leak a task probing a pid that is gone.
    // Same control-and-silence shape as the `Drop` case above, for the same
    // reason: the sibling instance left armed proves the silence could fail.
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

    // fails if `arm` wires the liveness loop to a throwaway channel of its
    // own (`let (tx, _rx) = mpsc::channel(1)` — it compiles, probes forever,
    // and reports into the void) instead of the shared sender on
    // `Extras::reports`, and fails if it passes the id where the pid belongs.
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
        // The scripted prober repeats its last outcome forever, so a loop
        // that kept probing after reporting would report again inside this
        // window.
        assert_no_liveness_within(&mut rig.liveness, interval * 3).await;
    }

    // fails if `arm_watch` hands `watch_delay` to the debouncer as written.
    // `notify-debouncer-full` derives its poll tick as `delay / 4` and sleeps
    // it on a dedicated OS thread, so a zero makes that thread spin (see
    // `MIN_WATCH_DELAY`'s own doc for what that costs).
    //
    // A direct call rather than an armed registry, and deliberately so:
    // `shep-core`'s `normalize` refuses `watch_delay = "0"`, and a
    // `ResolvedApp` is only obtainable through it, so no fixture in this
    // crate can carry a zero as far as `ExtrasRegistry::arm`. This floor
    // exists for the caller that skipped normalization — a future boot path,
    // or a bug — which is exactly the shape a bare `AppConfig` here stands
    // in for. It is also the only observable the floor has: with the tick
    // gone to zero the watch still delivers every event, it just burns a
    // core doing it, so there is no batch, no restart and no call count that
    // could tell the two apart.
    #[test]
    fn a_zero_watch_delay_is_floored_before_it_reaches_the_debouncer() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.watch_delay = Some(UpDuration::from_millis(0));
        assert_eq!(watch_delay_for(&app), MIN_WATCH_DELAY);
    }

    // fails if the floor is raised to something a Flockfile may legitimately
    // ask for. `normalize` accepts every non-zero `watch_delay`, so a floor
    // above one millisecond would silently lengthen a save-to-restart round
    // trip the user deliberately shortened — the clamp-nobody-announces
    // failure that the probe interval's config-time rejection exists to
    // avoid. Also fails if an app naming no delay stops getting the default.
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

    // fails if the reporter drops every report, or restarts something other
    // than the id the breach names, or if `handle_extra_restart` stops
    // declaring `CommandOrigin::Automatic`: a process the daemon restarted
    // because it outgrew its `max_memory` would reach every subscriber as
    // `manually: true`, indistinguishable from an operator's `shep restart`.
    #[tokio::test(start_paused = true)]
    async fn a_breach_naming_the_running_pid_restarts_that_sheep() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        // The ceiling the synthetic breach below names. Carried on the app
        // rather than left at `None` so the fixture describes a sheep that
        // could really have produced that report: the actor now re-asks a
        // breach against the ceiling in force, and an app configuring none
        // has none in force.
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

    // The liveness twin of the case above, and it needs to exist separately:
    // the reporter's two arms are two `select!` branches, so a `liveness` arm
    // that drops every failure it reads leaves the breach arm — and every
    // assertion riding on it — perfectly green. fails if that arm never calls
    // `extra_restart`, calls it for the wrong id, or restarts the sheep as
    // though a person had asked for it.
    #[tokio::test(start_paused = true)]
    async fn a_liveness_failure_naming_the_running_pid_restarts_that_sheep() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        handle.start(vec![app_with("web", |_| {})]).await.unwrap();
        let pid = handle.list().await[0].pid.expect("a live sheep has a pid");
        let (_breach_tx, breach_rx) = mpsc::channel(4);
        let (live_tx, live_rx) = mpsc::channel(4);
        let _reporter = spawn_extras_reporter(breach_rx, live_rx, handle.clone());

        // `spawn_test_fixture` wires no `Extras` at all, so the actor's own
        // registry never arms anything and its epoch for id 0 stays at the
        // default `0` `ExtrasRegistry::liveness_epoch` reports for an id it
        // has never seen. This report is injected straight past the
        // (nonexistent) real probe, so it has to agree with that default
        // rather than with the `1` a real arm would produce.
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

    // fails if `handle_extra_restart` drops its unknown-id guard for an
    // `.expect(…)`. A report for an id a `Delete` already removed is an
    // ordinary race, not a fault, and it must not take the whole engine down
    // with it — which is what the surviving `list()` proves: a panicked actor
    // closes its mailbox, and `list` panics on the way out.
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

    // fails against a guard that checks the pid but not the status. A gated
    // app between its spawn and its readiness result is `Starting` with a LIVE
    // pid — the one state in which the pid guard passes and the status guard
    // is the only thing left — so an unguarded command would kill a process
    // that has not finished starting and restart it as though it had.
    //
    // `a_breach_for_a_stopped_sheep_restarts_nothing` cannot make this claim:
    // `handle_exited` nulls `entry.pid` before every terminal transition, so
    // that case rides the pid guard alone.
    #[tokio::test(start_paused = true)]
    async fn an_extra_restart_for_a_sheep_that_is_still_starting_restarts_nothing() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        let app = app_with("web", |app| {
            app.wait_ready = true;
            // Long enough that the readiness wait cannot resolve inside this
            // test's own windows, so the sheep stays `Starting` throughout.
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

    // fails against a reporter that calls the public `restart`:
    // `ProcessSelector::Id` matches regardless of status, so `restart` on a
    // stopped sheep falls to `apply_immediate` and respawns it
    // unconditionally — resurrecting a process the user explicitly stopped,
    // and reporting success. The runner is scripted with spare procs on
    // purpose: that resurrection needs something to spawn from, and a fixture
    // sized to the happy path would let this test pass against the broken
    // implementation for the wrong reason (an exhausted script makes the
    // supervisor emit `Errored`, which no assertion here is watching for).
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

    // fails against a guard that checks status but not pid: a breach raised
    // for the process a restart already replaced would restart its healthy
    // successor and reset that successor's budget. The control half at the
    // end — the SAME reporter, fed the CURRENT pid, really does restart — is
    // what proves the negative half could have failed.
    #[tokio::test(start_paused = true)]
    async fn a_breach_carrying_the_previous_pid_restarts_nothing() {
        let (handle, mut rx, _fixture) = spawn_test_fixture();
        // The ceiling both synthetic breaches below name; see the case above
        // for why the fixture carries it.
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

    // fails if the actor never disarms on a clean stop: the cron worker keeps
    // its schedule, `ProcessSelector::Name` matches a stopped sheep too, and
    // the next occurrence brings back a sheep the user stopped. The cron
    // restart observed first is what makes the silence afterwards able to
    // fail.
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

    /// Waits up to `window` for `kind` naming `id`, panicking if none arrives.
    ///
    /// [`expect_restart`]'s shape, keyed by id and kind rather than by name: a
    /// swap puts two entries under ONE name, so a name alone cannot say which
    /// half of it an event is about.
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

    // fails if the swap's own ordering inverts: the drainee's extras disarmed
    // before the replacement arms, which is what "drain first, spawn second" —
    // the shape a rolling restart takes — would produce. Every reload would
    // then tear the name group down and put it back, and `arm`'s doc says what
    // that costs: re-registering the OS watcher each time, a step that can fail
    // and whose failure silently leaves an app without the watch that restarts
    // it.
    //
    // The registry case above says the registry HONOURS that ordering; this
    // one says the engine still PERFORMS it. Neither implies the other, and
    // the ordering itself lives in the swap rather than in the registry.
    //
    // The clock reading is the observation, and it is the only one that
    // survives out here. A torn-down-and-rebuilt group is otherwise identical
    // from the actor's outside — it fires on the same schedule, and the
    // registry's own fields are private to this module's other tier — but a
    // rebuild re-spawns the cron worker, and `spawn_cron_worker` reads the wall
    // clock on its first poll to derive its next occurrence. One reading is
    // therefore the difference between a group that was rebuilt and one that
    // was left alone.
    //
    // `max_cron_sleep` is 600s against a swap costing at most `listen_timeout`
    // (3s) plus `graceful_timeout` (8s) of virtual time, so the SURVIVING
    // worker cannot wake inside the window and take a reading of its own —
    // which would read exactly like the rebuild being watched for.
    #[tokio::test(start_paused = true)]
    async fn a_reload_leaves_the_name_groups_cron_worker_where_it_was() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        // Two procs, counted: the original and the one replacement a reload of
        // a one-instance app performs. A third would be answered "script
        // exhausted", which abandons the reload — no overlap, and a clock
        // count that proves nothing.
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

    /// A harness whose scripted runner hands out one proc that exits at once
    /// and then plenty that never do, plus a cron-restarting app that parks in
    /// a long backoff after any exit.
    ///
    /// The two cases below both need a sheep sitting in `WaitingRestart` — the
    /// one state that reaches a terminal transition through `apply_immediate`
    /// rather than `handle_exited`, because its process is already gone.
    fn backoff_harness(clock: &Arc<TestClock>) -> Harness {
        harness_with_extras(
            {
                let mut scripts = vec![ProcScript::never_exits()];
                scripts.push(ProcScript::const_exit(1));
                // Spare procs so a BROKEN implementation has something to
                // respawn from: without them the scripted runner reports
                // "script exhausted", the supervisor emits `Errored` instead
                // of `Restart`, and the negative assertions below would pass
                // against a completely unarmed disarm.
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

    /// The cron-restarting app the two backoff cases start, parked in a
    /// backoff far longer than either case's own window so its pending
    /// `RestartDue` can never be what a restart came from.
    fn backoff_app() -> shep_core::config::ResolvedApp {
        app_with("web", |app| {
            app.cron_restart = Some("0 * * * *".to_string());
            app.restart_delay = Some(UpDuration::from_millis(3 * 60 * 60 * 1_000));
        })
    }

    /// Drives the actor until `id` reports `status`, panicking if it never
    /// does. Each `list()` is a full round trip through the actor's mailbox,
    /// so this makes progress rather than merely observing it.
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

    // fails if `apply_immediate`'s Stop arm does not disarm. A sheep waiting
    // out its restart backoff has no live task, so its stop never reaches
    // `handle_exited`'s terminal branches at all — and `ProcessSelector::Name`
    // matches a stopped sheep just as happily, so the next occurrence brings
    // back a sheep the user stopped. The cron restart observed first (the one
    // that lands the sheep in the backoff) is what makes the silence
    // afterwards able to fail.
    #[tokio::test(start_paused = true)]
    async fn stopping_a_sheep_mid_backoff_still_stops_its_cron_worker() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        let h = backoff_harness(&clock);
        let mut rx = h.ctx.events.subscribe();
        h.ctx.supervisor.start(vec![backoff_app()]).await.unwrap();

        // The cron occurrence restarts it onto the immediately-exiting proc,
        // which lands it in its three-hour backoff.
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

    // fails if `apply_immediate`'s Delete arm does not disarm: the slot is
    // deregistered, but the name-group's cron worker keeps firing at a name
    // that no longer exists — a task that outlives the flock it belonged to.
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
        // A surviving worker would `restart(Name("web"))`, find nothing, and
        // log at debug — no bus event. What it CAN still do is re-register
        // nothing while running forever, so the observable claim is the
        // clock: a live worker keeps reading it every `max_cron_sleep`.
        let reads_after_delete = clock.reads();
        assert_no_restart_within(&mut rx, "web", PAST_THE_NEXT_OCCURRENCE).await;
        assert_eq!(
            clock.reads(),
            reads_after_delete,
            "a deleted sheep's cron worker must stop reading the clock, not merely stop finding sheep"
        );
    }

    // A cron occurrence reaches a name-group's instances through two doors,
    // and only one of them ever had a test. A RUNNING instance is killed and
    // respawned from `handle_exited`'s forced-restart branch; one already
    // sitting out its restart backoff has no live task, so the same occurrence
    // restarts it from `apply_immediate` instead. Both have to say the same
    // thing about who caused it, and neither answer is "a person".
    //
    // fails if either respawn site hardcodes the `manually` flag it emits, or
    // if the cron worker reaches for the operator's `restart` in place of
    // `restart_automatic`. `BusEvent::Process`'s `manually` is documented as
    // "True when a user action caused it", so a subscriber cannot otherwise
    // tell a nightly schedule apart from someone deploying.
    #[tokio::test(start_paused = true)]
    async fn a_cron_restart_is_never_reported_as_a_user_action() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        // The same fixture the two cases above stand on, and its script pool
        // is what makes the second half reachable rather than incidental: the
        // first occurrence respawns onto a proc that exits at once, and the
        // spares behind it are what the second occurrence respawns from. A
        // pool sized to a correct run would answer that second spawn
        // `SpawnFailed("script exhausted")` and emit `Errored`, which carries
        // no `Restart` for either implementation to be judged on.
        let h = backoff_harness(&clock);
        let mut rx = h.ctx.events.subscribe();
        h.ctx.supervisor.start(vec![backoff_app()]).await.unwrap();

        // Door one: the sheep is online, so the occurrence takes it through
        // the kill ladder and `handle_exited` performs the respawn.
        cross_one_hour().await;
        let (running, manually) = expect_restart_event(&mut rx, "web", EVENT_WAIT).await;
        assert_eq!(running.restarts, 1);
        assert!(
            !manually,
            "a cron occurrence is nobody typing `shep restart`"
        );

        // Door two: that respawn exited immediately into a three-hour backoff,
        // so the next occurrence — an hour later, well inside it — finds no
        // live task and restarts the sheep from `apply_immediate`.
        settle_into(&h.ctx.supervisor, 0, ProcStatus::WaitingRestart).await;
        cross_one_hour().await;
        let (backing_off, manually) = expect_restart_event(&mut rx, "web", EVENT_WAIT).await;
        assert_eq!(backing_off.restarts, 2);
        assert!(
            !manually,
            "the same occurrence must answer the same way through `apply_immediate`"
        );
    }

    // fails if `respawn`'s Err arm does not disarm. It lands the sheep in the
    // same `Errored` status `Decision::Errored` does, and is reachable in an
    // ordinary deploy — a cron occurrence (or a crash-restart, or a manual
    // one) whose respawn cannot spawn, because the binary was replaced
    // mid-deploy or the cwd is gone. Without the disarm the name group's cron
    // worker stays live against a sheep that will never come back.
    //
    // The clock is the observable claim, exactly as in the delete case above:
    // a failed respawn emits `Errored`, never `Restart`, so a surviving worker
    // leaves no bus event behind — only its every-`max_cron_sleep` reading.
    #[tokio::test(start_paused = true)]
    async fn a_respawn_that_cannot_spawn_stops_the_name_groups_cron_worker() {
        let clock = Arc::new(TestClock::starting_at(dt("2026-01-01T00:00:00Z")));
        // EXACTLY one script, and that is the point: the initial start
        // consumes it, so the cron occurrence's respawn finds the script
        // exhausted, `ScriptedRunner` reports `SpawnFailed`, and `respawn`
        // takes its Err arm.
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

    // fails if the actor never arms the enforcer at the Online transition, or
    // arms it with the sheep's id where its pid belongs: the scripted table
    // holds exactly one process, the pid the scripted runner hands the first
    // spawn, so an arming against any other number sums to zero and never
    // breaches however long the test waits.
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

    // fails if the readiness-GATED transition to `Online` does not arm.
    // `arm_extras` is reached from three transitions and only the two ungated
    // ones were proven; reverting `handle_ready_result`'s `went_online` call
    // to a plain `emit` leaves every other case in this file green while every
    // app that configures readiness silently loses all four of its extras.
    //
    // The readiness wait ends in a timeout rather than a signal — a scripted
    // proc writes no `{"kind":"ready"}` — which is the same `Online` and the
    // same arming site. The `Starting` assertion is what keeps this case from
    // quietly degrading into a second copy of the ungated one above.
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

    // fails if the actor never arms the liveness loop at the Online
    // transition, or arms it with the wrong id or pid.
    //
    // Real time and a real `OsProber`, because that is what the actor builds
    // — the paused clock does not move a real TCP connect.
    #[tokio::test]
    async fn the_actor_arms_the_liveness_loop_against_the_spawned_pid() {
        // Reserve a port, then release it: nothing ever listens there, so
        // every probe fails with a connection refusal, with no listener to
        // race and no port to reserve for real.
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

    /// fails if re-arming leaves the old group tasks running. `arm` deliberately
    /// keeps a live cron or watch task, which is right for a reload's overlap and
    /// wrong for a config change: those tasks read `watch`, `ignore_watch`,
    /// `watch_delay`, `watch_options`, `cron_restart` and `cron_timezone` when
    /// they are BUILT, so a task that survives keeps the old values forever.
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

        registry.rearm_name("web", &[&entry], idle_prober(), &rig.extras, &handle);
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

    /// fails if rearm_name tears a group down without rebuilding it. An app left
    /// with no watcher at all is worse than one left with a stale watcher.
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

        registry.rearm_name("web", &[&entry], idle_prober(), &rig.extras, &handle);
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

    /// fails if rearm_name reaches into another app's group. The registry is
    /// keyed by name and a rebuild stays inside one name.
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

        registry.rearm_name("web", &[&web_entry], idle_prober(), &rig.extras, &handle);
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

    /// fails if `rearm_name`'s delegation to `arm` per entry stops sharing
    /// one group across a name's own instances. Today that sharing is only
    /// TRANSITIVELY protected, by `arm`'s own idempotency test, because
    /// `rearm_name` never builds a task itself; a future refactor that stops
    /// delegating and builds per entry instead would orphan a multi-instance
    /// app's membership with nothing in this diff to catch it. Task 8 calls
    /// `rearm_name` with every entry of a name, so this is the path any app
    /// running more than one instance actually exercises.
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

        registry.rearm_name(
            "web",
            &[&entry_a, &entry_b],
            idle_prober(),
            &rig.extras,
            &handle,
        );
        tokio::task::yield_now().await;

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

    /// Tests that wait on real filesystem events or real elapsed time.
    ///
    /// The inner loop skips this module with `--skip ::slow::`; the full
    /// suite still runs them because nothing here is `#[ignore]`d.
    mod slow {
        use super::*;

        // The overlap a reload runs on. A replacement takes a NEW id in the
        // drainee's instance slot and arms BEFORE the drainee's exit disarms the
        // old id, so `disarm` finds a member still standing and the name group's
        // two tasks are never touched. Nothing in this type states that ordering
        // or enforces it — it belongs to the sequence the swap runs — so it is
        // pinned here, at the tier where "the same worker" is a thing that can be
        // said out loud.
        //
        // WHAT DISTINGUISHES A REBUILD FROM A SURVIVAL. A group torn down and put
        // straight back is otherwise INDISTINGUISHABLE from one never touched:
        // `arm` re-spawns a cron worker on the same schedule and re-registers a
        // watcher on the same root, both succeed in a test environment, and
        // `groups["web"].cron.is_some()`, the member set below, and every restart
        // the worker goes on to fire read identically under both orderings. The
        // observation that does tell them apart is TASK IDENTITY: `JoinHandle::id`
        // names one spawned task, and a rebuilt group's tasks are different ones.
        //
        // The two `AbortHandle`s are what make that identity airtight rather than
        // merely likely. tokio allows a task id to be reused once the task has
        // exited AND no handle to it is left alive — which is exactly the state a
        // teardown produces, since `NameExtras::abort` aborts both tasks and drops
        // both handles. Holding an `AbortHandle` keeps the id reserved for the
        // ORIGINAL task for the whole test, so a rebuilt task cannot be handed the
        // same one and pass this by coincidence. They are held and never fired.
        //
        // The clock count is a second, independent half, and it is what stands if
        // tokio's id semantics ever move: `spawn_cron_worker` reads the wall clock
        // once on its first poll to derive its next occurrence, so a rebuild costs
        // a reading that a survival does not. That is a claim about work performed
        // rather than about identity.
        //
        // fails if `arm` stops leaving a live name-group task alone — the
        // `is_none_or(is_finished)` guard dropped, so every re-arm re-registers the
        // OS watcher, a step that can fail and whose failure silently costs an app
        // the watch that restarts it — and fails if `disarm` stops keying its
        // teardown on the member set emptying.
        #[tokio::test(start_paused = true)]
        async fn a_replacement_arming_before_the_drainee_disarms_keeps_the_groups_own_tasks() {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(&dir);
            let root = tempfile::tempdir().unwrap();
            let (handle, _rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            // Both per-name extras, because the overlap has to hold for both — and
            // the watch is the one whose rebuild `arm`'s own doc calls out as
            // costly rather than merely wasteful.
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
            // Lets the cron worker reach its first poll, so the reading taken
            // below is a settled number rather than a race with it.
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

            // The overlap, in the order a swap performs it: the replacement takes
            // a new id in the drainee's instance slot and goes `Online` first, and
            // only then does the drainee's exit disarm the old id.
            registry.arm(
                &armed_entry(1, 0, 2000, app, &paths),
                idle_prober(),
                &rig.extras,
                &handle,
            );
            registry.disarm(0, "web");
            // Lets a REBUILT worker reach its own first poll. Without this the
            // clock count below could read unchanged because the replacement's
            // worker had not run yet, rather than because there is no such worker.
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

        // Boundary sweep (IR-40): a name group with zero online instances —
        // armed and disarmed again before it ever gets to do anything.
        //
        // The nearest existing case,
        // `only_the_last_instance_leaving_stops_the_name_groups_cron_worker`,
        // reaches its teardown through two members and a fired occurrence in
        // between, so it says nothing about a group that empties before its first
        // one. That window is not hypothetical: an app whose first spawn is
        // stopped or deleted straight away — a bad deploy, a `shep start` the
        // operator immediately reverses — passes through exactly this shape, and
        // a worker leaked there restarts a flock nobody is running.
        //
        // The assertions before the disarm are what keep the silence afterwards
        // from being vacuous: they say a real cron worker and a real OS watcher
        // were armed and a real enforcer arming existed, so the quiet below is a
        // teardown rather than an app that never armed anything.
        //
        // The negative half is `assert_no_restart_within` over
        // `PAST_THE_NEXT_OCCURRENCE`, the bounded window Global Constraints rule
        // 11 asks for: the same call crosses the hourly occurrence and makes the
        // claim.
        //
        // fails if `disarm` tears a group down only once it has fired at least
        // once, or keys the teardown on anything other than the member set
        // emptying.
        #[tokio::test(start_paused = true)]
        async fn a_group_disarmed_before_its_first_occurrence_leaves_no_worker_behind() {
            let dir = tempfile::tempdir().unwrap();
            let paths = test_paths(&dir);
            let root = tempfile::tempdir().unwrap();
            let (handle, mut rx, _fixture) = spawn_test_fixture();
            let rig = rig(DEFAULT_MAX_CRON_SLEEP);
            let mut registry = ExtrasRegistry::default();
            // Both per-name extras and one per-pid extra, so a single disarm has
            // to reach all three.
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

        // The watch twin of the case above, and it needs to exist separately: the
        // gate is two independent conditions, so a watch half narrowed to
        // `group.watch.is_none()` leaves the cron case — and every other
        // assertion in this file — perfectly green while an app that ended its
        // watch stays unwatched forever. fails if that half stops asking whether
        // the handle is ALIVE rather than merely present.
        //
        // The ending is forced with `abort` rather than by killing the
        // `WatchSource` the loop really returns on. That source dies with the
        // debouncer's own OS thread, which nothing available to a test reaches —
        // deleting the watched tree does not close it — and both leave the map in
        // the one state the gate reads: a finished handle.
        //
        // The save at the end is what makes the claim behavioural rather than
        // structural. A rebuilt watch has to have re-registered a real OS watcher
        // on the root; replacing the handle alone would restart nothing.
        //
        // Real time and a real filesystem, like every case that drives notify.
        // Twelve scripts against two spawns: the start, and the save's restart.
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

        // fails if a watched app is never armed at all; if the root is not
        // canonicalized (on macOS a tempdir under `/var/...` is delivered as
        // `/private/var/...`, every `strip_prefix` fails, and the watch fires
        // never); or if the last instance leaving does not abort the group —
        // which is the whole of "stopping a name stops its watch" (one instance
        // here, so the name and the instance are the same thing).
        //
        // Real time and a real filesystem: notify's backend is the OS, and a
        // paused clock does not move it.
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
                // The one owner of this subsystem's real-time constants
                // (`watch::real_time`), converted rather than re-declared.
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

        // fails if `DEFAULT_WATCH_DELAY` is not what an app naming no
        // `watch_delay` gets. It is 500ms; any fallback long enough to matter (a
        // stray `Duration::from_secs(600)`, say) leaves the save below with no
        // restart inside the deadline, and an app that asked to be watched would
        // in production appear to be watched by nothing.
        //
        // Real time and a real filesystem, like every case that drives notify.
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

        // The watch's own door into the same claim the cron case makes, and it
        // needs its own case: the two subsystems pick their `SupervisorHandle`
        // method independently, so a watch loop calling the operator's `restart`
        // leaves every cron assertion green.
        //
        // fails if `run_group` restarts through `restart` rather than
        // `restart_automatic` — under which a file save reaches every subscriber
        // as `manually: true`, and an editor's autosave is reported as a deploy.
        //
        // Real time and a real filesystem, like every case that drives notify.
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

        // fails if `arm_watch` builds its filter from empty slices rather than
        // from the app's own `watch_options`/`ignore_watch` — the shape under
        // which every ignore rule the user wrote is silently discarded and a build
        // directory's own churn restarts the app forever. The trigger at the end
        // is what makes the silence able to fail: the same watch, the same window,
        // a name the filter does not ignore.
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

        // fails if the watch's ignore set is the default globs plus `ignore_watch`
        // and nothing else. An app naming an explicit `out_file`/`err_file` under
        // its own `cwd` then restarts on its own log writes, and the loop is
        // self-sustaining: the startup line trips the debounce, the debounce
        // restarts the group, the restart writes another startup line.
        // `max_restarts` cannot end it — an automatic restart resets the budget —
        // and `**/logs/**` does not cover it, since nothing makes an explicit log
        // path live in a directory called `logs`.
        //
        // BOTH log paths are pointed inside the tree, so an implementation that
        // derives an ignore from `out_file` alone still reddens. The trigger at the
        // end is what makes the silence able to fail: the same watch, the same
        // window, a sibling file the filter has no reason to ignore.
        //
        // Real time and a real filesystem, like every case that drives notify.
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
                // Absolute, under the watched tree, and named nothing like `logs`
                // — the arrangement that really does put a shep write inside the
                // tree an app asked to have watched.
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

        // fails if the prober handed to a liveness loop is built from the app's
        // own `config.env` rather than the ASSEMBLED spec's, or built once at
        // boot and shared: `probe_exec` runs `env_clear().envs(&self.env)`, and
        // `SHEP_INSTANCE` is written by `assemble` and by nothing else, so under
        // either bug the variable expands to nothing, `live-` matches no file,
        // and BOTH instances report. Also fails if the actor assembles with a
        // hardcoded instance 0, under which neither reports.
        //
        // A file, not a port: `test -f` flips fail->pass with no listener, no
        // reserved port and no race, so the only thing this case can fail on is
        // the environment the probe ran with. Real time, and `#[cfg(unix)]` on
        // the test rather than on anything else, so the Windows leg still
        // compiles and runs every case above.
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
            // path does — and it is the same `assemble` call the prober's env
            // came from, so this pins WHICH instance id 1 is rather than assuming
            // the allocation order.
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
            // Both orderings must fail against the broken implementations above:
            // under them BOTH instances report and which arrives first is a race,
            // so the window that catches the other one is not optional. A bounded
            // `timeout` + `recv` spanning two further probe cycles, per rule 11.
            assert_no_liveness_within(&mut h.liveness, PROBE_INTERVAL.as_duration() * 3).await;
        }
    }
}
