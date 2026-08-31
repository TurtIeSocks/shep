//! `shep dog bark`: the webhook-alert dog.
//!
//! [`sinks`] is Task 19 — the Discord, Slack and plain-JSON webhook
//! destinations one fired [`shep_core::barks::Bark`] can be delivered to,
//! plus the pure body renderer and the async delivery function every later
//! task in this module calls. [`rules`] is Task 20 — [`rules::Rules`]
//! decides which bus events and which reconciliation-poll snapshots become
//! a [`rules::Firing`], and which are filtered out. This module (Task 21)
//! is the third piece: [`BarkConfig`] (`[dog.bark]`), and [`run_loop`],
//! which subscribes to the shepherd's bus AND polls the flock, wiring
//! `rules` and `sinks` together into a running dog. `dog::mod`'s own
//! `run_bark` (`super::run_dog`'s `"bark"` arm) is what parses
//! [`BarkConfig`], builds [`rules::Rules`], subscribes, and drives this
//! module's [`run_loop`] — the CLI-dispatch half of the wiring lives one
//! module up, next to the [`super::DogRuntime`] it needs.
//!
//! **The bus drops events, and that is what [`run_loop`] exists to
//! survive.** `tokio::sync::broadcast` discards what a lagging subscriber
//! cannot keep up with rather than queueing it — the daemon surfaces that
//! as `BusEvent::Dropped`, and this dog's own local subscription
//! ([`EventSource::next`]'s `Err(count)`) can lag the same way. A dog that
//! only listened to the bus would miss exactly the events load produces —
//! which is exactly when an alert matters most. [`run_loop`] reconciles by
//! polling the flock as well as subscribing: a dropped frame triggers an
//! immediate poll rather than waiting for the next scheduled one, and
//! [`rules::Rules`]'s own per-subject debounce is what lets an `Errored`
//! seen by both routes fire once instead of twice.

pub mod rules;
pub mod sinks;

use core::future::Future;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use shep_client::RequestError;
use shep_core::barks::{self, SinkOutcome};
use shep_core::protocol::{BusEvent, ProcessInfo};
use shep_core::values::UpDuration;
use tokio::sync::Mutex;
use tokio::time::MissedTickBehavior;

use self::rules::{Firing, Rule, Rules};
use self::sinks::Sink;
use crate::exit::ExitCode;

/// `[dog.bark]`.
///
/// `deny_unknown_fields`: a misspelled key must be a startup error naming
/// it, the same reasoning [`super::metrics::MetricsConfig`] gives for its
/// own section.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BarkConfig {
    /// Named sinks, `[dog.bark.sinks]`.
    pub sinks: BTreeMap<String, Sink>,
    /// Named rules, `[[dog.bark.rules]]`. Empty means
    /// [`Rules::default_rules`].
    pub rules: Vec<Rule>,
    /// How often the reconciliation poll runs when nothing has gone wrong.
    pub poll: UpDuration,
    /// Cap on `barks.jsonl`.
    pub history_bytes: u64,
    /// Per-delivery timeout.
    pub sink_timeout: UpDuration,
}

/// Hand-written, not derived: `#[serde(default)]` on the struct needs a
/// `Default`, and a derived one gives every field its type's zero value —
/// `poll = 0` (a bark dog that polls the shepherd in a hot loop),
/// `history_bytes = 0` (`barks.jsonl` evicted back to empty on every
/// append) and `sink_timeout = 0` (every delivery times out before it can
/// leave the process). Each default below is its own decision, not an
/// accident of `derive`.
impl Default for BarkConfig {
    fn default() -> Self {
        Self {
            sinks: BTreeMap::new(),
            rules: Vec::new(),
            // 30s: comfortably inside an operator's own patience for "is
            // this dog alive" while staying well clear of being a hot
            // loop — this is the FALLBACK cadence for when nothing has
            // gone wrong; a drop already triggers an immediate poll, so
            // this number is about steady-state cost, not responsiveness.
            poll: UpDuration::from_millis(30_000),
            // The same cap `shep-daemon`'s own writer uses for the same
            // ring (`shep_core::barks::DEFAULT_MAX_BYTES`) — one shared
            // number rather than the bark dog silently keeping more or
            // less history of its own alerts than the shepherd keeps of
            // its own dog-restart barks, in the one file both write to.
            history_bytes: barks::DEFAULT_MAX_BYTES,
            // 10s: generous next to how fast Discord and Slack actually
            // answer (well under a second in the ordinary case) while
            // staying well short of the poll cadence above, so one stuck
            // sink cannot silently absorb an entire poll interval's worth
            // of deliveries before anything notices.
            sink_timeout: UpDuration::from_millis(10_000),
        }
    }
}

/// One source of bus events: a frame, or a notice that frames were lost.
///
/// A trait rather than a concrete `EventStream`, so the reconciliation test
/// can drive this loop from a REAL `tokio::sync::broadcast::Receiver` with
/// a small capacity and make the bus genuinely drop events. That is the
/// property bark exists for, and a test that subscribed and saw everything
/// would prove the fast path, which was never the risk.
///
/// `broadcast::Receiver` is not a stand-in for the production source; it is
/// what the shepherd's own bus IS (`shep_daemon::bus`), one process
/// boundary away.
pub trait EventSource: Send {
    /// The next event; `Err(count)` when the source dropped `count` frames
    /// before this one; `None` when it ends.
    fn next(&mut self) -> impl Future<Output = Option<Result<BusEvent, u64>>> + Send;
}

/// What bark reads the flock through, so the loop's poll is drivable
/// without a socket.
///
/// `Sync`, not just `Send`: [`run_loop`]'s own future holds `&F` across an
/// `.await` (inside [`reconcile`]) so it can poll the same source from both
/// its lag arm and its interval arm without moving it, and a shared
/// reference held across an await point is itself part of what makes that
/// future `Send`.
pub trait FlockSource: Send + Sync {
    /// The flock as it stands.
    ///
    /// # Errors
    /// Whatever the underlying source could not answer with — for the
    /// production implementation, whatever `Request::ListFlock` failed
    /// with.
    fn flock(&self) -> impl Future<Output = Result<Vec<ProcessInfo>, RequestError>> + Send;
}

/// Bark's loop: subscribe for speed, poll for correctness.
///
/// **A dropped frame polls immediately** rather than waiting for the next
/// interval. The bus is a `tokio::sync::broadcast`, so a lagging subscriber
/// has events DROPPED rather than queued; for `shep bleats` that is a
/// cosmetic notice, and for alerting it is a missed page. The subscription
/// is what makes bark fast; the poll is what makes it correct; and the
/// moment a drop is reported is exactly when correctness is in question.
///
/// Runs until `SIGINT`/`SIGTERM` (`SIGTERM` is what the shepherd's own kill
/// ladder actually sends first — see [`super::metrics::run`]'s own doc for
/// why a dog that ignores it rides the whole ladder to `SIGKILL`) or until
/// `events` ends.
///
/// Every firing is delivered by a task spawned off the select loop, never
/// awaited inline — a slow sink (Discord's own rate limit is measured in
/// seconds) must not stop this loop from reading the next bus event, or it
/// causes the exact drop it exists to catch. `barks::append` is a
/// read-modify-rename against ONE file, and several delivery tasks can be
/// mid-flight at once, so appends are serialized behind an in-process
/// [`tokio::sync::Mutex`] held only around the `append` call itself — the
/// cross-process case (this dog racing the shepherd's own writer) is
/// already covered by `barks::append`'s own `flock(2)` lock, which this
/// does not duplicate or replace.
///
/// Written as a plain `fn` returning `impl Future<..> + use<E, F>` rather
/// than as `async fn` (a deliberate, self-reported deviation from this
/// task's own literal interface, which spells this `pub async fn`):
/// edition 2024's default `impl Trait` capture rules would otherwise tie
/// the returned future to `config`'s and `barks_path`'s own borrow
/// lifetimes, even though nothing below ever holds either past this
/// function's own synchronous prefix. A future that borrows the caller's
/// `config`/`barks_path` cannot be `tokio::spawn`ed unless they happen to
/// be `'static` — exactly the shape
/// `a_dropped_frame_makes_bark_poll_and_catch_up`'s own fixture needs,
/// spawning this loop against a config and a `barks_path` that are both
/// ordinary, short-lived test locals. Everything either parameter
/// contributes is copied out — cheaply, before any `.await` — into owned
/// values the `async move` block below actually captures, so the future it
/// returns borrows neither and is `'static` on its own regardless of what
/// the caller's `config`/`barks_path` outlive. Callers still `.await` or
/// `tokio::spawn` the result exactly as they would an `async fn` — the
/// sugar difference is invisible at every call site in this module.
pub fn run_loop<E: EventSource, F: FlockSource>(
    events: E,
    flock: F,
    rules: Rules,
    config: &BarkConfig,
    barks_path: &Path,
) -> impl Future<Output = ExitCode> + Send + use<E, F> {
    let sinks = Arc::new(config.sinks.clone());
    let sink_timeout = config.sink_timeout.as_duration();
    let max_bytes = config.history_bytes;
    // `interval_at`, not `interval`: a plain `tokio::time::interval` fires
    // its first tick immediately, which would make every dog poll once at
    // startup for no reason attributable to either a drop or the interval
    // genuinely elapsing. This loop's first poll must always be explainable
    // by one of those two, never by the timer's own startup quirk —
    // `a_dropped_frame_makes_bark_poll_and_catch_up` is built entirely on
    // being able to say "the poll ran because of the lag."
    let poll_period = config.poll.as_duration();
    let barks_path = Arc::new(barks_path.to_path_buf());

    async move {
        let mut events = events;
        let mut rules = rules;
        let append_lock = Arc::new(Mutex::new(()));

        let mut sigterm = match crate::shutdown::Terminate::install() {
            Ok(sigterm) => sigterm,
            Err(err) => {
                eprintln!("shep dog bark: could not install a shutdown handler: {err}");
                return ExitCode::Failure;
            }
        };

        let mut poll_interval =
            tokio::time::interval_at(tokio::time::Instant::now() + poll_period, poll_period);
        poll_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = sigterm.recv() => break,
                next = events.next() => {
                    match next {
                        None => break,
                        Some(Ok(event)) => {
                            let firings = rules.on_event(&event, now_ms());
                            spawn_firings(firings, &sinks, &append_lock, &barks_path, sink_timeout, max_bytes);
                        }
                        Some(Err(_dropped)) => {
                            // The drop itself carries no information about
                            // what was lost — the only way to know is to
                            // ask the shepherd what things look like now.
                            reconcile(&flock, &mut rules, &sinks, &append_lock, &barks_path, sink_timeout, max_bytes).await;
                        }
                    }
                }
                _ = poll_interval.tick() => {
                    reconcile(&flock, &mut rules, &sinks, &append_lock, &barks_path, sink_timeout, max_bytes).await;
                }
            }
        }

        ExitCode::Success
    }
}

/// One reconciliation pass: ask `flock` what the flock looks like now, run
/// it through `rules::on_poll`, and spawn a delivery for anything that
/// fires. Shared by the lag arm and the interval arm of [`run_loop`]'s own
/// `select!` so "poll because of a drop" and "poll on schedule" are one
/// code path, not two that could drift.
///
/// A failed poll is logged and dropped rather than propagated: the next
/// bus event, or the next interval tick, tries again, and a transient
/// `ListFlock` failure must not take the whole dog down over one bad
/// round-trip.
async fn reconcile<F: FlockSource>(
    flock: &F,
    rules: &mut Rules,
    sinks: &Arc<BTreeMap<String, Sink>>,
    append_lock: &Arc<Mutex<()>>,
    barks_path: &Arc<PathBuf>,
    sink_timeout: Duration,
    max_bytes: u64,
) {
    match flock.flock().await {
        Ok(snapshot) => {
            let firings = rules.on_poll(&snapshot, now_ms());
            spawn_firings(
                firings,
                sinks,
                append_lock,
                barks_path,
                sink_timeout,
                max_bytes,
            );
        }
        Err(err) => eprintln!("shep dog bark: reconciliation poll failed: {err}"),
    }
}

/// Spawns one delivery task per firing, so [`run_loop`]'s own `select!`
/// returns to reading the next event immediately rather than waiting on any
/// of them.
fn spawn_firings(
    firings: Vec<Firing>,
    sinks: &Arc<BTreeMap<String, Sink>>,
    append_lock: &Arc<Mutex<()>>,
    barks_path: &Arc<PathBuf>,
    sink_timeout: Duration,
    max_bytes: u64,
) {
    for firing in firings {
        let sinks = Arc::clone(sinks);
        let append_lock = Arc::clone(append_lock);
        let barks_path = Arc::clone(barks_path);
        tokio::spawn(async move {
            deliver_and_record(
                firing,
                &sinks,
                &append_lock,
                &barks_path,
                sink_timeout,
                max_bytes,
            )
            .await;
        });
    }
}

/// Delivers `firing` to each of its named sinks, then writes the resulting
/// [`shep_core::barks::Bark`] — with [`shep_core::barks::Bark::sinks`] filled in from what delivery actually
/// did — to `barks_path`.
///
/// The record is written AFTER delivery, deliberately: a [`Firing`]'s own
/// `bark.sinks` starts empty because what each sink made of it is not known
/// until it has been tried, and this is the one place that fills it in
/// honestly. Written unconditionally, even when every sink refused it — the
/// local trail in `barks.jsonl` is what an operator reads when the page
/// never arrived, and it is most valuable exactly when the sink is the
/// thing that broke.
///
/// `append_lock` is held only around the [`barks::append`] call itself:
/// several of these run concurrently (one per firing, spawned rather than
/// awaited inline — see [`run_loop`]'s own doc), and `append` is a
/// read-modify-rename against one file, so two concurrent appends racing
/// that sequence would lose whichever one loses the final rename. The lock
/// covers exactly that race and nothing upstream of it (rendering,
/// delivery); it does not touch or duplicate `barks::append`'s own
/// cross-process `flock(2)` lock, which is what keeps this dog and the
/// shepherd's own writer from doing the same thing to each other.
async fn deliver_and_record(
    firing: Firing,
    sinks: &BTreeMap<String, Sink>,
    append_lock: &Mutex<()>,
    barks_path: &Path,
    sink_timeout: Duration,
    max_bytes: u64,
) {
    let mut bark = firing.bark;
    let mut outcomes = Vec::with_capacity(firing.sinks.len());
    for name in &firing.sinks {
        let outcome = match sinks.get(name) {
            Some(sink) => match sinks::deliver(sink, &bark, sink_timeout).await {
                Ok(()) => SinkOutcome {
                    sink: name.clone(),
                    error: None,
                },
                Err(err) => SinkOutcome {
                    sink: name.clone(),
                    error: Some(err.to_string()),
                },
            },
            // Unreachable in practice: `Rules::new` already refuses a rule
            // routing to a sink `[dog.bark.sinks]` does not define, before
            // this loop ever runs. Recorded rather than panicked on — the
            // same posture the rest of this dog takes toward a state it
            // believes cannot happen, per this crate's own stance against
            // `todo!()`/`unreachable!()` where a plain value works instead.
            None => SinkOutcome {
                sink: name.clone(),
                error: Some("sink not configured".to_owned()),
            },
        };
        outcomes.push(outcome);
    }
    bark.sinks = outcomes;

    let _guard = append_lock.lock().await;
    if let Err(err) = barks::append(barks_path, &bark, max_bytes) {
        eprintln!("shep dog bark: could not record a fired bark: {err}");
    }
}

/// Wall-clock milliseconds since the Unix epoch — the one real-time read
/// this loop needs.
///
/// [`Rules::on_event`]/[`Rules::on_poll`] both take a caller-supplied
/// timestamp rather than reading the clock themselves, precisely so a test
/// can drive them with fixed values; this is the one caller in the
/// production path that needs a real one. Mirrors `shep-daemon`'s own
/// `now_ms` (`pub(crate)` there, so this is its own copy rather than a
/// shared one — the two crates do not share a dependency edge that would
/// carry it).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use shep_core::barks::Bark;
    use shep_core::protocol::ProcessEventKind;
    use shep_core::status::ProcStatus;
    use tokio::sync::{broadcast, oneshot};

    use super::*;
    use crate::http::{HttpRequest, read_request, write_response};

    /// [`EventSource`] over the real thing bark's local subscription lags
    /// on: a `tokio::sync::broadcast::Receiver`. Only ever built by tests —
    /// the production path implements this trait for
    /// [`shep_client::EventStream`] instead, in `dog/mod.rs`, over the same
    /// kind of channel one process boundary away.
    impl EventSource for broadcast::Receiver<BusEvent> {
        async fn next(&mut self) -> Option<Result<BusEvent, u64>> {
            match self.recv().await {
                Ok(event) => Some(Ok(event)),
                Err(broadcast::error::RecvError::Lagged(count)) => Some(Err(count)),
                Err(broadcast::error::RecvError::Closed) => None,
            }
        }
    }

    /// A [`FlockSource`] that always answers the same fixed listing,
    /// counting how many times it was asked — the reconciliation test's own
    /// proof that a poll ran (or did not).
    #[derive(Clone)]
    struct ScriptedFlock {
        answer: Arc<Vec<ProcessInfo>>,
        calls: Arc<std::sync::atomic::AtomicU32>,
    }

    impl ScriptedFlock {
        fn answering(answer: Vec<ProcessInfo>) -> Self {
            Self {
                answer: Arc::new(answer),
                calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl FlockSource for ScriptedFlock {
        async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((*self.answer).clone())
        }
    }

    /// Binds an ephemeral port, accepts exactly one connection, answers
    /// `status`/`body`, and hands the captured request back through the
    /// returned receiver. The same shape `sinks.rs`'s own test module
    /// builds, duplicated rather than shared across a `#[cfg(test)]`
    /// boundary neither module exposes to the other.
    async fn one_shot_sink(
        status: u16,
        body: &str,
    ) -> (SocketAddr, oneshot::Receiver<HttpRequest>) {
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

    /// Accepts exactly one connection and then never answers it — up, but
    /// stalled forever, exactly the shape `sink_timeout` exists for.
    async fn slow_sink() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _peer) = listener.accept().await.unwrap();
            core::future::pending::<()>().await;
        });
        addr
    }

    /// Awaits `fut` against `timeout`, under a genuinely real clock rather
    /// than the paused one `#[tokio::test(start_paused = true)]` installs.
    ///
    /// Bridges through [`tokio::task::spawn_blocking`] — the technique
    /// `tokio::time::pause`'s own doc names under "Preventing auto-advance"
    /// — because a plain `tokio::time::timeout(..).await` races real
    /// socket I/O against the paused clock's auto-advance and reliably
    /// LOSES: with nothing else runnable, tokio jumps straight from "now"
    /// to the next pending timer's deadline in one step (confirmed against
    /// `tokio` 1.53's own `time::park_thread_timeout`, which does exactly
    /// one non-blocking, zero-duration I/O poll before deciding to jump —
    /// not a bounded real wait), and a loopback TCP round trip does not
    /// finish inside that single zero-duration poll. `spawn_blocking`
    /// inhibits auto-advance for as long as it runs (tokio tracks this
    /// with a plain counter — see `Clock::inhibit_auto_advance`), so the
    /// nested `Handle::block_on` below waits out `timeout` on the ACTUAL
    /// wall clock while this test's spawned server and this dog's own
    /// delivery task keep running normally on the runtime's own worker
    /// thread, freed by the calling test task moving off it.
    ///
    /// A bare `tokio::time::timeout` loses this race 100% of the time under
    /// a paused clock, never intermittently: a minimal reproduction (two
    /// tasks, a real `TcpListener`, nothing else) confirmed it. The
    /// property each test asserts — a real delivery over a real socket
    /// happened — is unchanged; only the mechanism used to wait for it
    /// without fighting the paused clock is different.
    async fn await_real_io<T: Send + 'static>(
        timeout: Duration,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, tokio::time::error::Elapsed> {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || handle.block_on(tokio::time::timeout(timeout, fut)))
            .await
            .expect("the spawn_blocking bridge task must not itself panic")
    }

    fn base_info(name: &str, status: ProcStatus, restarts: u32) -> ProcessInfo {
        ProcessInfo::builder(1, name, status)
            .pid(Some(4242))
            .restarts(restarts)
            .uptime_ms(1_000)
            .build()
    }

    fn errored_info(name: &str, restarts: u32) -> ProcessInfo {
        base_info(name, ProcStatus::Errored, restarts)
    }

    fn process_event(name: &str, kind: ProcessEventKind) -> BusEvent {
        BusEvent::Process {
            event: kind,
            info: base_info(name, ProcStatus::Online, 0),
            manually: false,
            at_ms: 0,
        }
    }

    fn errored_event(name: &str) -> BusEvent {
        process_event(name, ProcessEventKind::Errored)
    }

    /// A cheap, distinct bus event that no rule below fires on — filler for
    /// overflowing the broadcast channel's own small capacity.
    fn log_event(i: u32) -> BusEvent {
        BusEvent::LogOut {
            id: i,
            line: format!("log line {i}"),
        }
    }

    /// One `gave_up` rule routed to the sink named `"ops"`. The name is
    /// shared with [`config_with_sink`]'s own sink map — a rule routing
    /// somewhere `[dog.bark.sinks]` does not define is refused at
    /// [`Rules::new`], not something a test fixture can afford to get
    /// wrong either.
    ///
    /// Debounce is a real, non-zero five minutes — NOT zero. The
    /// reconciliation test drains a broadcast channel whose last few
    /// buffered entries include `errored_event("web")` a second time
    /// (it survives the overflow as one of the tail messages, so
    /// `events.next()` yields it again as an ordinary, non-lagged item
    /// after the lag notice), which would fire `GaveUp` for "web" a
    /// second time through `on_event` if nothing suppressed it — and a
    /// zero debounce suppresses nothing. A real debounce is what makes
    /// `rules::Rules`'s own "an `Errored` seen by both routes fires once"
    /// guarantee (`rules.rs`'s own `an_errored_seen_by_both_routes_fires_once`)
    /// actually hold here.
    fn gave_up_rules() -> Rules {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "ops".to_owned(),
            Sink::Json {
                url: "http://127.0.0.1:1/hook".to_owned(),
                body: None,
            },
        );
        Rules::new(
            vec![rules::Rule {
                when: rules::Trigger::GaveUp {},
                sinks: vec!["ops".to_owned()],
                debounce: UpDuration::from_millis(5 * 60_000),
            }],
            &sinks,
        )
        .unwrap()
    }

    /// A [`BarkConfig`] with one sink, `"ops"`, POSTing to `addr` — matching
    /// the name [`gave_up_rules`] routes to. `poll` is 60s, comfortably
    /// past every timeout these tests bound themselves by, so a poll that
    /// fires is attributable to the lag path and never to the interval
    /// racing it.
    fn config_with_sink(addr: SocketAddr, _barks_path: &Path) -> BarkConfig {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "ops".to_owned(),
            Sink::Json {
                url: format!("http://{addr}/hook"),
                body: None,
            },
        );
        BarkConfig {
            sinks,
            rules: Vec::new(),
            poll: UpDuration::from_millis(60_000),
            history_bytes: barks::DEFAULT_MAX_BYTES,
            sink_timeout: UpDuration::from_millis(5_000),
        }
    }

    /// THE test this dog exists for. fails if the poll is only ever driven
    /// by its interval: `web`'s `errored` frame is genuinely dropped by a
    /// real broadcast channel, so a loop that reconciles on a timer alone
    /// stays silent for the whole poll interval — and under a paused clock,
    /// forever.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_frame_makes_bark_poll_and_catch_up() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        for i in 0..64 {
            tx.send(log_event(i)).unwrap();
        }
        tx.send(errored_event("web")).unwrap();

        // The drop is real, or this test proves nothing.
        assert!(
            matches!(rx.recv().await, Err(broadcast::error::RecvError::Lagged(n)) if n > 0),
            "the fixture must actually overflow the channel"
        );

        let (tx2, rx2) = tokio::sync::broadcast::channel(4);
        for i in 0..64 {
            tx2.send(log_event(i)).unwrap();
        }
        tx2.send(errored_event("web")).unwrap();

        let (addr, captured) = one_shot_sink(200, "").await;
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");
        let flock = ScriptedFlock::answering(vec![errored_info("web", 16)]);

        let loop_handle = tokio::spawn(run_loop(
            rx2,
            flock.clone(),
            gave_up_rules(),
            &config_with_sink(addr, &barks_path),
            &barks_path,
        ));

        let req = await_real_io(Duration::from_secs(5), captured)
            .await
            .expect("a dropped frame must produce a delivered bark")
            .unwrap();
        assert!(String::from_utf8_lossy(&req.body).contains("web"));

        // `captured` resolves the instant the sink server finishes writing
        // its response — concurrently with, not strictly after, the
        // delivery task's own remaining tail (reading that response,
        // building the outcome, appending to `barks.jsonl`). A short,
        // bounded poll — not a sleep the test just hopes is long enough —
        // covers that ordinary scheduling gap.
        let recorded = await_real_io(Duration::from_secs(5), {
            let barks_path = barks_path.clone();
            async move {
                loop {
                    let records = shep_core::barks::read(&barks_path).unwrap();
                    if !records.is_empty() {
                        break records;
                    }
                    tokio::task::yield_now().await;
                }
            }
        })
        .await
        .expect("the delivered bark must be recorded promptly after delivery");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].subject, "web");
        assert_eq!(recorded[0].sinks[0].error, None);

        assert_eq!(
            flock.calls(),
            1,
            "the poll ran because of the lag, not because an interval elapsed \
             — the clock is paused, so no interval has"
        );

        loop_handle.abort();
    }

    /// fails if a sink that refuses the delivery costs the record. The
    /// local trail is what an operator reads when the page never arrived,
    /// and it is most valuable exactly when the sink is the thing that
    /// broke. Drives `deliver_and_record` directly rather than through
    /// `run_loop`: what this proves — a failed delivery still gets written
    /// — is a property of that function, and testing it there is
    /// deterministic where going through the loop's own event plumbing
    /// would need a second synchronization mechanism just to know when a
    /// failed delivery finished being not-delivered.
    #[tokio::test]
    async fn a_bark_is_recorded_even_when_every_sink_refuses_it() {
        let (addr, _captured) = one_shot_sink(500, "refused").await;
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");

        let mut sinks = BTreeMap::new();
        sinks.insert(
            "ops".to_owned(),
            Sink::Json {
                url: format!("http://{addr}/hook"),
                body: None,
            },
        );
        let append_lock = Mutex::new(());
        let firing = Firing {
            bark: Bark {
                at_ms: 1_000,
                rule: "gave_up".to_owned(),
                subject: "web".to_owned(),
                message: "web gave up: restart budget exhausted".to_owned(),
                sinks: Vec::new(),
            },
            sinks: vec!["ops".to_owned()],
        };

        deliver_and_record(
            firing,
            &sinks,
            &append_lock,
            &barks_path,
            Duration::from_secs(5),
            barks::DEFAULT_MAX_BYTES,
        )
        .await;

        let recorded = shep_core::barks::read(&barks_path).unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "a refused delivery must still be recorded"
        );
        assert_eq!(recorded[0].subject, "web");
        assert!(
            recorded[0].sinks[0].error.is_some(),
            "the 500 must be recorded as a failed delivery, not silently dropped"
        );
    }

    /// fails if a slow sink stalls the loop. Discord's rate limit is
    /// measured in seconds, and a bark dog that stops reading the bus while
    /// it waits starts DROPPING the frames it exists to catch — the loop
    /// would cause the exact fault it is built to survive.
    ///
    /// One rule routes to a sink that never answers; a second, independent
    /// rule routes to one that answers immediately. Both fire back to back,
    /// before the loop is given a chance to run either delivery to
    /// completion. If firings were awaited inline rather than spawned, the
    /// fast sink could only be reached after the slow one's own
    /// `sink_timeout` (10s of virtual time) elapsed; this asserts it is
    /// reached in well under that, with no timer needing to fire at all.
    #[tokio::test(start_paused = true)]
    async fn a_slow_sink_never_stalls_the_loop() {
        let slow_addr = slow_sink().await;
        let (fast_addr, fast_captured) = one_shot_sink(200, "").await;
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");

        let mut sinks = BTreeMap::new();
        sinks.insert(
            "slow".to_owned(),
            Sink::Json {
                url: format!("http://{slow_addr}/hook"),
                body: None,
            },
        );
        sinks.insert(
            "fast".to_owned(),
            Sink::Json {
                url: format!("http://{fast_addr}/hook"),
                body: None,
            },
        );
        let rules = Rules::new(
            vec![
                rules::Rule {
                    when: rules::Trigger::GaveUp {},
                    sinks: vec!["slow".to_owned()],
                    debounce: UpDuration::from_millis(0),
                },
                rules::Rule {
                    when: rules::Trigger::Event {
                        kinds: vec!["online".to_owned()],
                    },
                    sinks: vec!["fast".to_owned()],
                    debounce: UpDuration::from_millis(0),
                },
            ],
            &sinks,
        )
        .unwrap();
        let config = BarkConfig {
            sinks,
            rules: Vec::new(),
            poll: UpDuration::from_millis(60_000),
            history_bytes: barks::DEFAULT_MAX_BYTES,
            sink_timeout: UpDuration::from_millis(10_000),
        };

        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(errored_event("web")).unwrap();
        tx.send(process_event("api", ProcessEventKind::Online))
            .unwrap();

        let flock = ScriptedFlock::answering(Vec::new());
        let loop_handle = tokio::spawn(run_loop(rx, flock, rules, &config, &barks_path));

        let req = await_real_io(Duration::from_millis(50), fast_captured)
            .await
            .expect(
                "the fast sink must be reached promptly; a slow sink in flight \
                 must not stall the loop",
            )
            .unwrap();
        assert_eq!(req.method, "POST");

        loop_handle.abort();
    }

    /// fails if a `[dog.bark]` with no configuration at all polls in a hot
    /// loop, keeps no history, or times every delivery out instantly — what
    /// `#[derive(Default)]` would silently give this struct. The same
    /// shape `MetricsConfig`'s own `the_default_bind_is_loopback` uses, and
    /// for the same reason: an empty `[dog.bark]` is the ordinary case.
    #[test]
    fn an_empty_section_gets_sane_defaults_not_zeros() {
        let parsed: BarkConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, BarkConfig::default());
        assert_eq!(BarkConfig::default().poll.as_millis(), 30_000);
        assert_eq!(
            BarkConfig::default().history_bytes,
            barks::DEFAULT_MAX_BYTES
        );
        assert_eq!(BarkConfig::default().sink_timeout.as_millis(), 10_000);
    }

    /// fails if `[dog.bark]` cannot parse the exact `shep.toml` fragment
    /// `docs/dogs.md` and `web/src/pages/docs/dogs.astro` publish as the
    /// worked example — copy-pasted here relative to `[dog.bark]` the way
    /// `runtime.config::<BarkConfig>()` sees it (that section already
    /// stripped, so `[sinks]`/`[[rules]]` rather than
    /// `[dog.bark.sinks]`/`[[dog.bark.rules]]`).
    ///
    /// This is the regression test for the shipped bug: `on_empty_section`
    /// above is the only other test in this module that calls
    /// `toml::from_str::<BarkConfig>`, and an empty document never
    /// deserializes a single [`rules::Rule`], so it passed on v0.1.18 even
    /// though `[[dog.bark.rules]]` could not parse at all — `Rule`'s
    /// `#[serde(flatten)]` field and its (then) `deny_unknown_fields`
    /// rejected `on` itself as an unknown key. See `rules.rs`'s own
    /// `Rule`/[`rules::Trigger`] docs for the fix.
    #[test]
    fn the_documented_bark_config_parses_from_toml() {
        let toml_str = r#"
[sinks]
oncall = { kind = "discord", url = "https://discord.com/api/webhooks/..." }
audit = { kind = "json", url = "https://example.internal/hook" }

[[rules]]
on = "gave_up"
sinks = ["oncall", "audit"]

[[rules]]
on = "restart_rate"
restarts = 5
within = "2m"
sinks = ["oncall"]
"#;
        let config: BarkConfig =
            toml::from_str(toml_str).expect("the documented [dog.bark] example must parse");
        assert_eq!(config.sinks.len(), 2);
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].when, rules::Trigger::GaveUp {});
        assert_eq!(config.rules[0].sinks, vec!["oncall", "audit"]);
        assert_eq!(
            config.rules[1].when,
            rules::Trigger::RestartRate {
                restarts: 5,
                within: UpDuration::from_millis(2 * 60_000),
            }
        );
        assert_eq!(config.rules[1].sinks, vec!["oncall"]);
        // Both rules must also survive `Rules::new`'s own validation
        // against the sinks parsed alongside them — the parse succeeding
        // is necessary but not sufficient for the dog to actually start.
        Rules::new(config.rules, &config.sinks).expect("both documented rules route to real sinks");
    }
}
