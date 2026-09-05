//! `shep dog bark`: the webhook-alert dog.
//!
//! [`sinks`] holds the webhook destinations and the delivery function;
//! [`rules`] decides which bus events and poll snapshots become a
//! [`rules::Firing`]. This module has [`BarkConfig`] and [`run_loop`],
//! which subscribes to the shepherd's bus and polls the flock.
//!
//! The bus is a `tokio::sync::broadcast`, so a lagging subscriber has
//! events dropped rather than queued, and load is when an alert matters
//! most. A dropped frame triggers an immediate poll, and [`rules::Rules`]'s
//! per-subject debounce is what lets an `Errored` seen by both routes fire
//! once.

pub mod rules;
pub mod sinks;

use core::future::Future;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use shep_client::RequestError;
use shep_client::dogs::DogConfig;
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
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema, DogConfig)]
#[serde(deny_unknown_fields, default)]
pub struct BarkConfig {
    /// Named sinks, `[dog.bark.sinks]`.
    ///
    /// Marked whole, not per URL, and the difference is where the mark
    /// lands. `#[shep(secret)]` names a field of THIS type, and the URL is
    /// a field of [`Sink`] one level down, so a mark there is absent from
    /// the schema shep generates for [`BarkConfig`]: every sink would go
    /// out with its bearer token as an ordinary string. Marking the map
    /// says less than marking each URL would, and it says it about the
    /// type shep actually asks. Over-redacting is the safe direction.
    #[shep(secret)]
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

/// Hand-written: `#[serde(default)]` on the struct needs a `Default`, and
/// a derived one would give `poll`, `history_bytes` and `sink_timeout`
/// their types' zero values.
impl Default for BarkConfig {
    fn default() -> Self {
        Self {
            sinks: BTreeMap::new(),
            rules: Vec::new(),
            // 30s: the fallback cadence for when nothing has gone wrong.
            // A drop already triggers an immediate poll, so this bounds
            // steady-state cost, not responsiveness.
            poll: UpDuration::from_millis(30_000),
            // The cap `shep-daemon`'s own writer uses: one shared number
            // for the one file both append to.
            history_bytes: barks::DEFAULT_MAX_BYTES,
            // 10s: well past how fast Discord and Slack answer, well short
            // of the poll cadence above, so one stuck sink cannot absorb a
            // whole interval's deliveries.
            sink_timeout: UpDuration::from_millis(10_000),
        }
    }
}

/// One source of bus events: a frame, or a notice that frames were lost.
///
/// A trait rather than a concrete `EventStream`, so a test can drive this
/// loop from a real `tokio::sync::broadcast::Receiver` with a small
/// capacity and make the bus genuinely drop events.
pub trait EventSource: Send {
    /// The next event; `Err(count)` when the source dropped `count` frames
    /// before this one; `None` when it ends.
    fn next(&mut self) -> impl Future<Output = Option<Result<BusEvent, u64>>> + Send;
}

/// What bark reads the flock through, so the loop's poll is drivable
/// without a socket.
///
/// `Sync`, not just `Send`: [`run_loop`]'s future holds `&F` across the
/// `.await` in [`reconcile`], so both its lag arm and its interval arm
/// poll the same source without moving it.
pub trait FlockSource: Send + Sync {
    /// The flock as it stands.
    ///
    /// # Errors
    /// Whatever the source failed with: in production, whatever
    /// `Request::ListFlock` failed with.
    fn flock(&self) -> impl Future<Output = Result<Vec<ProcessInfo>, RequestError>> + Send;
}

/// What bark re-asks its own `[bark]` section through, so a config change
/// is drivable without a socket.
///
/// `BusEvent::DogConfigChanged` says nothing about what changed, so the
/// frame is only a prompt: the answer is one `Request::DogConfig`.
///
/// `Sync` for the reason [`FlockSource`] is: [`run_loop`]'s future holds
/// a `&C` across the `.await` in [`reloaded_config`].
pub trait ConfigSource: Send + Sync {
    /// This dog's `[bark]` section as it stands now, empty when the file
    /// has no such section.
    ///
    /// # Errors
    /// Whatever the source failed with: in production, whatever
    /// `Request::DogConfig` failed with.
    fn section(&self) -> impl Future<Output = Result<String, RequestError>> + Send;
}

/// The rule set a `[bark]` section means: its own rules, or
/// [`rules::Rules::default_rules`] when it configured none.
///
/// Asked at startup and again on every `config.dog.bark` frame, so a
/// reloading bark cannot get different defaults from a starting one.
///
/// # Errors
/// - [`rules::RulesError`] as [`rules::Rules::new`]: a rule routing to a
///   sink the section does not define, an unknown event kind, or an
///   insecure webhook scheme.
pub fn rules_for(config: &BarkConfig) -> Result<Rules, rules::RulesError> {
    let rule_list = if config.rules.is_empty() {
        Rules::default_rules(&config.sinks)
    } else {
        config.rules.clone()
    };
    Rules::new(rule_list, &config.sinks)
}

/// Bark's loop: subscribe for speed, poll for correctness. Ends on
/// `SIGINT`/`SIGTERM` or when `events` does.
///
/// A dropped frame polls immediately, since the bus drops what a lagging
/// subscriber cannot keep up with. Firings are spawned, never awaited
/// inline, or a slow sink causes the drop this loop exists to catch.
/// Appends serialize behind an in-process [`tokio::sync::Mutex`]; the
/// cross-process race is `barks::append`'s own `flock(2)`.
///
/// A plain `fn`, not `async fn`: the returned future must borrow neither
/// `config` nor `barks_path`, so callers can spawn it.
pub fn run_loop<E: EventSource, F: FlockSource, C: ConfigSource>(
    events: E,
    flock: F,
    rules: Rules,
    config: &BarkConfig,
    barks_path: &Path,
    config_source: C,
) -> impl Future<Output = ExitCode> + Send + use<E, F, C> {
    let mut sinks = Arc::new(config.sinks.clone());
    let mut sink_timeout = config.sink_timeout.as_duration();
    let mut max_bytes = config.history_bytes;
    // `interval_at`, not `interval`: a plain `interval` fires its first
    // tick immediately, so the first poll would be attributable to the
    // timer's startup rather than to a drop or an elapsed interval.
    let mut poll_period = config.poll.as_duration();
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
                        // One connection generation: the shepherd went away,
                        // usually to exec a successor. The dog exits 0 and
                        // `autorestart` replaces it, one restart per reload;
                        // docs/specs/deferred.md tracks resubscribing instead.
                        None => break,
                        // Matched on the variant rather than on the dog's
                        // name: the subscription already narrows this to
                        // bark's own topic, `config.dog.<name>`.
                        Some(Ok(BusEvent::DogConfigChanged { .. })) => {
                            if let Some((next, next_rules)) = reloaded_config(&config_source).await {
                                // In place, never a restart: sinks and
                                // rules are pure data with no OS resource
                                // to rebind.
                                sinks = Arc::new(next.sinks.clone());
                                sink_timeout = next.sink_timeout.as_duration();
                                max_bytes = next.history_bytes;
                                // Rebuilt, which resets each rule's
                                // per-subject debounce: carrying it over
                                // a renumbered rule set would key state on
                                // an index that moved.
                                rules = next_rules;
                                if next.poll.as_duration() != poll_period {
                                    poll_period = next.poll.as_duration();
                                    poll_interval = tokio::time::interval_at(
                                        tokio::time::Instant::now() + poll_period,
                                        poll_period,
                                    );
                                    poll_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
                                }
                                eprintln!("shep dog bark: reloaded [bark] from dogs.toml");
                            }
                        }
                        Some(Ok(event)) => {
                            let firings = rules.on_event(&event, now_ms());
                            spawn_firings(firings, &sinks, &append_lock, &barks_path, sink_timeout, max_bytes);
                        }
                        Some(Err(_dropped)) => {
                            // The drop says nothing about what was lost;
                            // only the shepherd can.
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

/// Re-asks `source` for `[bark]` and rebuilds what a config change can
/// swap, or `None` when the answer cannot be used.
///
/// Every failure is reported and dropped, never propagated: a dog that
/// exited on a bad edit would stop alerting while config is being edited.
/// stderr gets the fact, never the section: `[bark]` carries webhook URLs
/// that are bearer credentials.
async fn reloaded_config<C: ConfigSource>(source: &C) -> Option<(BarkConfig, Rules)> {
    let section = match source.section().await {
        Ok(section) => section,
        Err(err) => {
            eprintln!("shep dog bark: could not re-read [bark] from the shepherd: {err}");
            return None;
        }
    };
    // Empty means the section is gone, a legitimate edit. bark keeps
    // running with no sinks and no rules.
    let config = if section.is_empty() {
        BarkConfig::default()
    } else {
        match toml::from_str::<BarkConfig>(&section) {
            Ok(config) => config,
            Err(_err) => {
                eprintln!("shep dog bark: [bark] in dogs.toml does not parse; see `shep dogs`");
                return None;
            }
        }
    };
    match rules_for(&config) {
        Ok(rules) => Some((config, rules)),
        Err(err) => {
            eprintln!("shep dog bark: keeping the running rules; the new ones are refused: {err}");
            None
        }
    }
}

/// One reconciliation pass: ask `flock` what the flock looks like now, run
/// it through `rules::on_poll`, and spawn a delivery for anything that
/// fires. Shared by [`run_loop`]'s lag arm and interval arm, so the two
/// polls are one code path.
///
/// A failed poll is logged and dropped: the next bus event or interval
/// tick tries again.
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
/// [`shep_core::barks::Bark`] to `barks_path`.
///
/// After delivery, since a [`Firing`]'s [`shep_core::barks::Bark::sinks`]
/// is empty until each sink has been tried. Written even when every sink
/// refused it: the local trail is what an operator reads when the page
/// never arrived.
///
/// `append_lock` covers only the [`barks::append`] call, a
/// read-modify-rename against one file that several of these run at once.
/// It does not replace `append`'s own cross-process `flock(2)`.
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
            // Unreachable: `Rules::new` refuses a rule routing to a sink
            // `[dog.bark.sinks]` does not define. Recorded rather than
            // panicked on.
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

/// Wall-clock milliseconds since the Unix epoch.
///
/// [`Rules::on_event`] and [`Rules::on_poll`] take a caller-supplied
/// timestamp so a test can fix it; this is the production caller.
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

    /// The sinks map carries the credential marker, and the rules beside
    /// it do not.
    ///
    /// Marked at the map rather than at each `Sink`'s `url`, because
    /// `#[shep(secret)]` names a field of the type being asked and the URL
    /// belongs to a type one level down. `Rule` is checked under `$defs`,
    /// the one place a marker could land on `Rule::sinks`.
    #[test]
    fn the_bark_schema_marks_the_sinks_map_and_leaves_the_rules_plain() {
        let schema = shep_client::dogs::config_schema::<BarkConfig>()
            .expect("`sinks` is a property of this type");
        let schema = schema.as_value();

        assert_eq!(
            schema.pointer("/properties/sinks/x-shep-secret"),
            Some(&serde_json::Value::Bool(true)),
            "every sink holds a webhook URL, which is a bearer credential"
        );
        assert_eq!(
            schema.pointer("/properties/rules/x-shep-secret"),
            None,
            "a rule names sinks and holds no credential of its own"
        );
        assert!(
            schema.pointer("/$defs/Rule/properties/sinks").is_some(),
            "the pointer below is only a check while `Rule` still has this \
             shape, so a rename that moved it must fail here first"
        );
        assert_eq!(
            schema.pointer("/$defs/Rule/properties/sinks/x-shep-secret"),
            None,
            "a rule's sinks are names, and a name is not a credential"
        );
    }

    /// [`EventSource`] over the real thing bark's subscription lags on: a
    /// `tokio::sync::broadcast::Receiver`. The production path implements
    /// the trait for [`shep_client::EventStream`] in `dog/mod.rs`.
    impl EventSource for broadcast::Receiver<BusEvent> {
        async fn next(&mut self) -> Option<Result<BusEvent, u64>> {
            match self.recv().await {
                Ok(event) => Some(Ok(event)),
                Err(broadcast::error::RecvError::Lagged(count)) => Some(Err(count)),
                Err(broadcast::error::RecvError::Closed) => None,
            }
        }
    }

    /// A [`FlockSource`] answering one fixed listing, counting how many
    /// times it was asked.
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

    /// A [`ConfigSource`] answering one fixed `[bark]` section, counting
    /// how many times it was asked. The count is what proves the loop
    /// re-asks on a `config.dog.bark` frame rather than acting on it.
    #[derive(Clone)]
    struct ScriptedConfig {
        section: Arc<String>,
        calls: Arc<std::sync::atomic::AtomicU32>,
    }

    impl ScriptedConfig {
        fn answering(section: String) -> Self {
            Self {
                section: Arc::new(section),
                calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl ConfigSource for ScriptedConfig {
        async fn section(&self) -> Result<String, RequestError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((*self.section).clone())
        }
    }

    /// Binds an ephemeral port, accepts exactly one connection, answers
    /// `status`/`body`, and hands the captured request back through the
    /// returned receiver.
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

    /// A sink that accepts one connection and then never answers, plus a
    /// signal that fires the moment it has accepted.
    ///
    /// The signal lets a caller assert an order rather than a duration,
    /// which would be a claim about how a runner schedules two tasks.
    async fn slow_sink() -> (SocketAddr, tokio::sync::oneshot::Receiver<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (connected, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (_stream, _peer) = listener.accept().await.unwrap();
            // Ignored: a caller that does not care drops the receiver.
            let _ = connected.send(());
            core::future::pending::<()>().await;
        });
        (addr, rx)
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

    /// A cheap bus event no rule below fires on: filler for overflowing
    /// the broadcast channel's small capacity.
    fn log_event(i: u32) -> BusEvent {
        BusEvent::LogOut {
            id: i,
            line: format!("log line {i}"),
        }
    }

    /// One `gave_up` rule routed to the sink named `"ops"`, the name
    /// [`config_with_sink`] defines.
    ///
    /// The debounce is a real five minutes, not zero: the reconciliation
    /// test's channel yields `errored_event("web")` again as an ordinary
    /// item after the lag notice, and a zero debounce suppresses nothing.
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

    /// A [`BarkConfig`] with one sink, `"ops"`, POSTing to `addr`. `poll`
    /// is 60s, past every timeout these tests bound themselves by, so a
    /// poll that fires is attributable to the lag path.
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

    /// Fails if the poll is only ever driven by its interval: `web`'s
    /// `errored` frame is genuinely dropped by a real broadcast channel,
    /// and the fixture's interval is 60s.
    ///
    /// A real clock, not `start_paused`: a deadline awaited through a
    /// `spawn_blocking` bridge cannot elapse under a paused one, so a
    /// regression hangs the suite instead of failing it.
    #[tokio::test]
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
            // Never asked: no `config.dog.bark` frame is sent here.
            ScriptedConfig::answering(String::new()),
        ));

        let req = tokio::time::timeout(Duration::from_secs(5), captured)
            .await
            .expect("a dropped frame must produce a delivered bark")
            .unwrap();
        assert!(String::from_utf8_lossy(&req.body).contains("web"));

        // `captured` resolves when the sink server finishes writing its
        // response, concurrently with the delivery task's own tail
        // (outcome, append). A bounded poll covers that gap.
        let recorded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let records = shep_core::barks::read(&barks_path).unwrap();
                if !records.is_empty() {
                    break records;
                }
                tokio::task::yield_now().await;
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
            "the poll ran because of the lag, not because an interval elapsed: \
             the interval is 60s and this test is milliseconds old"
        );

        loop_handle.abort();
    }

    /// Drives `deliver_and_record` directly rather than through
    /// `run_loop`: the property belongs to that function, and the loop's
    /// event plumbing would need a second synchronization mechanism to
    /// know when a failed delivery finished.
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

    /// Fails if a slow sink stalls the loop: a bark dog that stops reading
    /// the bus while it waits drops the frames it exists to catch.
    ///
    /// The proof is an order, not a duration. The slow sink signals that
    /// it has accepted and parked, and the fast sink is reached after
    /// that; an inline-awaiting loop would still be parked. `sink_timeout`
    /// is ten minutes and both timeouts are failure guards, so their
    /// values change how long a broken loop takes to report, never what
    /// passes.
    #[tokio::test]
    async fn a_slow_sink_never_stalls_the_loop() {
        let (slow_addr, slow_connected) = slow_sink().await;
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
            sink_timeout: UpDuration::from_millis(600_000),
        };

        let (tx, rx) = tokio::sync::broadcast::channel(8);
        tx.send(errored_event("web")).unwrap();
        tx.send(process_event("api", ProcessEventKind::Online))
            .unwrap();

        let flock = ScriptedFlock::answering(Vec::new());
        let loop_handle = tokio::spawn(run_loop(
            rx,
            flock,
            rules,
            &config,
            &barks_path,
            ScriptedConfig::answering(String::new()),
        ));

        // The order is the assertion: the slow sink has a connection and
        // is parked on it, so the loop is provably mid-delivery.
        tokio::time::timeout(Duration::from_secs(30), slow_connected)
            .await
            .expect("the slow sink must be reached at all, or this test proves nothing")
            .unwrap();

        // Then the fast sink is reached anyway. A loop awaiting firings
        // inline would still be parked on the delivery above.
        let req = tokio::time::timeout(Duration::from_secs(30), fast_captured)
            .await
            .expect(
                "the fast sink must be reached while a slow sink is still in \
                 flight; a slow sink must not stall the loop",
            )
            .unwrap();
        assert_eq!(req.method, "POST");

        loop_handle.abort();
    }

    /// Fails if an unconfigured `[dog.bark]` polls in a hot loop, keeps no
    /// history, or times every delivery out instantly: what
    /// `#[derive(Default)]` would give this struct.
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

    /// Fails if `[dog.bark]` cannot parse the fragment `docs/dogs.md` and
    /// `web/src/pages/docs/dogs.astro` publish, copy-pasted here relative
    /// to `[dog.bark]` the way `runtime.config::<BarkConfig>()` sees it,
    /// so `[sinks]`/`[[rules]]` rather than the full paths.
    ///
    /// The only other `toml::from_str::<BarkConfig>` in this module parses
    /// an empty document, which never deserializes a [`rules::Rule`].
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
        // Parsing is necessary but not sufficient: the dog starts only if
        // `Rules::new` accepts both rules against the sinks beside them.
        Rules::new(config.rules, &config.sinks).expect("both documented rules route to real sinks");
    }

    /// Fails if a `config.dog.bark` frame does not reach bark's sinks: the
    /// next firing is delivered to the sink the new section names, over a
    /// real socket, while the old sink's server is still up.
    ///
    /// The loop is the same loop throughout, which is the other half of
    /// what this pins: bark's config is pure data with no OS resource
    /// attached, so it swaps in place instead of restarting.
    ///
    /// A real clock, not `start_paused`, or a broken swap hangs the suite
    /// rather than failing it.
    #[tokio::test]
    async fn a_config_change_swaps_barks_sinks_in_place() {
        let (old_addr, mut old_captured) = one_shot_sink(200, "").await;
        let (new_addr, new_captured) = one_shot_sink(200, "").await;
        let dir = tempfile::tempdir().unwrap();
        let barks_path = dir.path().join("barks.jsonl");
        let source = ScriptedConfig::answering(format!(
            "[sinks.ops]\nkind = \"json\"\nurl = \"http://{new_addr}/hook\"\n"
        ));

        let (tx, rx) = broadcast::channel(16);
        let loop_handle = tokio::spawn(run_loop(
            rx,
            ScriptedFlock::answering(Vec::new()),
            gave_up_rules(),
            &config_with_sink(old_addr, &barks_path),
            &barks_path,
            source.clone(),
        ));

        // One receiver, one queue: the loop awaits the re-ask before it
        // takes the second event, so the delivery below is attributable
        // to the new section.
        tx.send(BusEvent::DogConfigChanged {
            dog: "bark".to_owned(),
        })
        .unwrap();
        tx.send(errored_event("web")).unwrap();

        let req = tokio::time::timeout(Duration::from_secs(5), new_captured)
            .await
            .expect("the bark must reach the sink the new section names")
            .unwrap();
        assert!(String::from_utf8_lossy(&req.body).contains("web"));

        assert_eq!(source.calls(), 1, "one frame, one re-ask");
        assert!(
            !loop_handle.is_finished(),
            "bark swaps its config in place; it does not exit to pick one up"
        );
        // `try_recv`, never an await: the old sink's server is still
        // parked in `accept`, so it holds its sender alive and an await
        // would never return. The delivery above has already happened.
        assert!(
            old_captured.try_recv().is_err(),
            "the sink the old section named must be left alone"
        );

        loop_handle.abort();
    }
}
