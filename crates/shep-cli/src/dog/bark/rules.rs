//! What makes a bark fire: [`Rule`], [`Trigger`] and the [`Rules`] engine
//! that turns a bus event or a reconciliation poll into zero or more
//! [`Firing`]s.
//!
//! [`Rules::on_poll`] catches what the bus drops: it evaluates the same
//! rules against the flock's current state instead of one event. Both
//! routes share one per-subject debounce in [`Rules`]'s `subjects` map, so
//! a rule fired by one route is not fired again by the other.
//!
//! Debounce is per rule per subject, never global: a global debounce would
//! silence the second sheep to go down during an incident.

use core::fmt;
use std::collections::BTreeMap;

use serde::Deserialize;
use shep_core::barks::Bark;
use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
use shep_core::status::ProcStatus;
use shep_core::values::{MemSize, UpDuration};

use super::sinks::{self, Sink, SinkConfigError};

/// How long, by default, a rule stays quiet for a subject after firing.
///
/// Five minutes: long enough that a flapping sheep does not page an
/// operator once a minute, short enough that a still-down sheep gets a
/// reminder inside the same incident rather than only its first alert.
fn default_debounce() -> UpDuration {
    UpDuration::from_millis(5 * 60 * 1_000)
}

/// One entry under `[[bark.rules]]` in `dogs.toml`.
///
/// A misspelled key anywhere in a rule is a startup error naming the bad
/// key, never a silently ignored setting. See
/// [`BarkConfig`](super::BarkConfig)'s own doc for why that posture
/// matters.
// deny_unknown_fields cannot sit here: serde refuses it alongside
// #[serde(flatten)]. Keys `sinks` and `debounce` don't claim flow into
// Trigger's own deserialize instead, which is where a typo is caught.
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema)]
pub struct Rule {
    /// What fires it.
    #[serde(flatten)]
    pub when: Trigger,
    /// Sinks by name, from `[bark.sinks]` in `dogs.toml`. At least one; a
    /// rule routing nowhere is a rule that fires into a file and is
    /// refused at startup rather than discovered during an incident.
    pub sinks: Vec<String>,
    /// How long after one firing this rule stays quiet FOR THE SAME
    /// SUBJECT. Per-subject, never global: a flock where one sheep flaps
    /// must not mute the alert for a different sheep going down.
    #[serde(default = "default_debounce")]
    pub debounce: UpDuration,
}

/// What makes a rule fire.
///
/// `deny_unknown_fields` lives here rather than on [`Rule`] — see that
/// type's own doc for why the combination with `#[serde(flatten)]` forced
/// the move.
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema)]
#[serde(tag = "on", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    /// Any of these bus event kinds, by their wire spelling
    /// (`exit`, `errored`, `online`, ...).
    Event {
        /// The kinds this rule fires on.
        kinds: Vec<String>,
    },
    /// The shepherd gave up: a sheep reached `Errored`. On by DEFAULT with
    /// no configuration at all, because it is the alert that must not be
    /// missed — the app is down and staying down — and because it cannot
    /// disagree with the shepherd: it is keyed to the shepherd's own
    /// decision rather than to a threshold bark chose.
    // A struct variant, not a unit variant: an internally tagged unit
    // variant skips field checking entirely, so deny_unknown_fields would
    // not catch a typo beside `on = "gave_up"`.
    GaveUp {},
    /// The early warning: `restarts` restarts within `within`. Opt-in,
    /// because it is the one that pages at 3am for a blip, and the
    /// threshold should be one the operator chose.
    RestartRate {
        /// How many restarts.
        restarts: u32,
        /// Within how long.
        within: UpDuration,
    },
    /// A sheep's memory crossed a ceiling, read from the reconciliation
    /// poll rather than from the bus — memory is a level, and the bus
    /// carries events.
    MemoryAbove {
        /// The ceiling.
        bytes: MemSize,
    },
}

/// The rule-kind name [`Bark::rule`] records for a firing: the same
/// snake_case spelling a `[dog.bark.rules]` entry's own `on = "..."` key
/// uses, so an operator reading `barks.jsonl` sees no vocabulary mismatch
/// against what they configured.
fn trigger_name(when: &Trigger) -> &'static str {
    match when {
        Trigger::Event { .. } => "event",
        Trigger::GaveUp {} => "gave_up",
        Trigger::RestartRate { .. } => "restart_rate",
        Trigger::MemoryAbove { .. } => "memory_above",
    }
}

/// `kind`'s wire spelling, the string `kinds` in a rule names it by. Reads
/// `ProcessEventKind`'s own `Serialize` rather than hand-listing variants,
/// falling back to an empty string, which matches nothing, if that ever
/// fails.
fn wire_spelling(kind: ProcessEventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Whether `kind` is a spelling [`ProcessEventKind`] actually has, the same
/// way [`wire_spelling`] reads the mapping rather than hand-listing it.
fn is_known_kind(kind: &str) -> bool {
    serde_json::from_value::<ProcessEventKind>(serde_json::Value::String(kind.to_owned())).is_ok()
}

/// Why [`Rules::new`] refused a configuration.
#[derive(Debug)]
pub enum RulesError {
    /// Rule at position `index` (0-based, in configuration order) routes
    /// to a sink name `[bark.sinks]` does not define.
    UnknownSink {
        /// Position in the configured rule list.
        index: usize,
        /// The sink name that does not exist.
        sink: String,
    },
    /// Rule at position `index` routes to no sink at all.
    NoSinks {
        /// Position in the configured rule list.
        index: usize,
    },
    /// Rule at position `index`'s `Event` trigger names an event kind that
    /// is not on the wire.
    UnknownKind {
        /// Position in the configured rule list.
        index: usize,
        /// The kind string that matches no [`ProcessEventKind`].
        kind: String,
    },
    /// A `[bark.sinks]` entry is a Discord or Slack webhook configured
    /// with `http://`. See [`sinks::require_secure_scheme`].
    InsecureSink(SinkConfigError),
}

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSink { index, sink } => write!(
                f,
                "rule {index} routes to sink \"{sink}\", which [bark.sinks] in dogs.toml does \
                 not define"
            ),
            Self::NoSinks { index } => write!(f, "rule {index} routes to no sink at all"),
            Self::UnknownKind { index, kind } => write!(
                f,
                "rule {index}'s event trigger names \"{kind}\", which is not an event kind on the wire"
            ),
            Self::InsecureSink(source) => write!(f, "{source}"),
        }
    }
}

impl core::error::Error for RulesError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InsecureSink(source) => Some(source),
            Self::UnknownSink { .. } | Self::NoSinks { .. } | Self::UnknownKind { .. } => None,
        }
    }
}

impl From<SinkConfigError> for RulesError {
    fn from(source: SinkConfigError) -> Self {
        Self::InsecureSink(source)
    }
}

/// Per-subject bookkeeping [`Rules`] keeps to make the bus route and the
/// poll route agree on one firing rather than two, and to let
/// [`Trigger::RestartRate`] measure a window without bark keeping its own
/// restart tally.
#[derive(Debug, Default)]
struct SubjectState {
    /// Rule index -> unix millis it last fired for this subject. Read and
    /// written by both [`Rules::on_event`] and [`Rules::on_poll`], which is
    /// the whole mechanism behind "an `Errored` seen by both routes fires
    /// once."
    last_fired: BTreeMap<usize, u64>,
    /// Rule index -> (the shepherd's restart count when this rule's
    /// window last reset for this subject, when that reset happened).
    /// Only `RestartRate` rules ever populate this.
    restart_windows: BTreeMap<usize, (u32, u64)>,
}

/// Bark's whole state: the rules, and what each subject last looked like to
/// each rule.
#[derive(Debug)]
pub struct Rules {
    rules: Vec<Rule>,
    subjects: BTreeMap<String, SubjectState>,
}

impl Rules {
    /// Builds the engine, refusing a configuration that cannot work.
    ///
    /// # Errors
    /// - [`RulesError::UnknownSink`]: a rule routes to a sink
    ///   `[dog.bark.sinks]` does not define.
    /// - [`RulesError::NoSinks`]: a rule routes to no sink.
    /// - [`RulesError::UnknownKind`]: an `Event` rule names a kind that is
    ///   not on the wire.
    /// - [`RulesError::InsecureSink`]: a `[dog.bark.sinks]` entry is a
    ///   Discord or Slack webhook using `http://`, checked whether or not
    ///   any rule routes to it.
    pub fn new(rules: Vec<Rule>, sinks: &BTreeMap<String, Sink>) -> Result<Self, RulesError> {
        for (name, sink) in sinks {
            sinks::require_secure_scheme(name, sink)?;
        }
        for (index, rule) in rules.iter().enumerate() {
            if rule.sinks.is_empty() {
                return Err(RulesError::NoSinks { index });
            }
            for sink in &rule.sinks {
                if !sinks.contains_key(sink) {
                    return Err(RulesError::UnknownSink {
                        index,
                        sink: sink.clone(),
                    });
                }
            }
            if let Trigger::Event { kinds } = &rule.when {
                for kind in kinds {
                    if !is_known_kind(kind) {
                        return Err(RulesError::UnknownKind {
                            index,
                            kind: kind.clone(),
                        });
                    }
                }
            }
        }
        Ok(Self {
            rules,
            subjects: BTreeMap::new(),
        })
    }

    /// The default rule set, for a `[dog.bark]` that configured none: one
    /// `GaveUp` rule routed to every configured sink.
    #[must_use]
    pub fn default_rules(sinks: &BTreeMap<String, Sink>) -> Vec<Rule> {
        vec![Rule {
            when: Trigger::GaveUp {},
            sinks: sinks.keys().cloned().collect(),
            debounce: default_debounce(),
        }]
    }

    /// Whether rule `idx` may fire for `subject` now, recording the firing
    /// when it can. Shared by the bus route and the poll route, so an
    /// event both see fires once.
    fn try_fire(&mut self, idx: usize, subject: &str, now_ms: u64, debounce: UpDuration) -> bool {
        let state = self.subjects.entry(subject.to_owned()).or_default();
        let ready = state
            .last_fired
            .get(&idx)
            .is_none_or(|&last| now_ms.saturating_sub(last) >= debounce.as_millis());
        if ready {
            state.last_fired.insert(idx, now_ms);
        }
        ready
    }

    /// Whether a `RestartRate` rule has accumulated `threshold` or more
    /// restarts for `subject` since the window opened, sliding the window
    /// forward once `within` has elapsed.
    ///
    /// The baseline starts at zero on first observation, so restarts from
    /// before bark's own first poll count toward it. Once `within` elapses
    /// with no new firing, the baseline resets to the current count, so a
    /// sheep that stopped flapping stops re-triggering the rule.
    fn restart_window_crossed(
        &mut self,
        idx: usize,
        subject: &str,
        current_restarts: u32,
        threshold: u32,
        within: UpDuration,
        now_ms: u64,
    ) -> bool {
        let state = self.subjects.entry(subject.to_owned()).or_default();
        let window = state.restart_windows.entry(idx).or_insert((0, now_ms));
        if now_ms.saturating_sub(window.1) > within.as_millis() {
            *window = (current_restarts, now_ms);
        }
        current_restarts.saturating_sub(window.0) >= threshold
    }

    /// What one bus event fires, after debounce.
    #[must_use]
    pub fn on_event(&mut self, event: &BusEvent, now_ms: u64) -> Vec<Firing> {
        let BusEvent::Process {
            event: kind, info, ..
        } = event
        else {
            return Vec::new();
        };
        let kind = *kind;
        let kind_wire = wire_spelling(kind);
        let mut firings = Vec::new();
        for idx in 0..self.rules.len() {
            let debounce = self.rules[idx].debounce;
            let trigger = self.rules[idx].when.clone();
            let message = match &trigger {
                Trigger::Event { kinds } if kinds.iter().any(|k| k == &kind_wire) => {
                    Some(format!("{} {kind_wire}", info.name))
                }
                Trigger::GaveUp {} if kind == ProcessEventKind::Errored => {
                    Some(format!("{} gave up: restart budget exhausted", info.name))
                }
                _ => None,
            };
            let Some(message) = message else { continue };
            if !self.try_fire(idx, &info.name, now_ms, debounce) {
                continue;
            }
            let sinks = self.rules[idx].sinks.clone();
            firings.push(Firing {
                bark: Bark {
                    at_ms: now_ms,
                    rule: trigger_name(&trigger).to_owned(),
                    subject: info.name.clone(),
                    message,
                    sinks: Vec::new(),
                },
                sinks,
            });
        }
        firings
    }

    /// What the reconciliation poll fires: everything the bus should have
    /// carried and did not, plus the level-triggered rules that have no bus
    /// event at all.
    ///
    /// Reads `ProcessInfo::restarts`, the shepherd's own count, rather
    /// than tallying restarts itself: a private tally could drift from
    /// what the shepherd acts on.
    #[must_use]
    pub fn on_poll(&mut self, flock: &[ProcessInfo], now_ms: u64) -> Vec<Firing> {
        let mut firings = Vec::new();
        for info in flock {
            for idx in 0..self.rules.len() {
                let debounce = self.rules[idx].debounce;
                let trigger = self.rules[idx].when.clone();
                let message = match &trigger {
                    Trigger::Event { .. } => None,
                    Trigger::GaveUp {} => (info.status == ProcStatus::Errored).then(|| {
                        format!("{} gave up: restart budget exhausted", info.name)
                    }),
                    Trigger::RestartRate { restarts, within } => self
                        .restart_window_crossed(
                            idx,
                            &info.name,
                            info.restarts,
                            *restarts,
                            *within,
                            now_ms,
                        )
                        .then(|| {
                            format!(
                                "{} restarted {} times, at or past the {restarts}-within-{within} early warning",
                                info.name, info.restarts
                            )
                        }),
                    Trigger::MemoryAbove { bytes } => info.memory_bytes.and_then(|used| {
                        (used >= bytes.bytes()).then(|| {
                            format!(
                                "{} memory at {}, at or above the {bytes} limit",
                                info.name,
                                MemSize::from_bytes(used)
                            )
                        })
                    }),
                };
                let Some(message) = message else { continue };
                if !self.try_fire(idx, &info.name, now_ms, debounce) {
                    continue;
                }
                let sinks = self.rules[idx].sinks.clone();
                firings.push(Firing {
                    bark: Bark {
                        at_ms: now_ms,
                        rule: trigger_name(&trigger).to_owned(),
                        subject: info.name.clone(),
                        message,
                        sinks: Vec::new(),
                    },
                    sinks,
                });
            }
        }
        firings
    }
}

/// One rule firing for one subject: the bark to write and where to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    /// The record, with [`Bark::sinks`] still empty until delivery fills
    /// it in.
    pub bark: Bark,
    /// The sink names it routes to.
    pub sinks: Vec<String>,
}

#[cfg(test)]
mod tests {
    use shep_core::protocol::ProcessInfo;

    use super::*;

    fn one_sink(name: &str) -> BTreeMap<String, Sink> {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            name.to_owned(),
            Sink::Json {
                url: "http://localhost/hook".to_owned(),
                body: None,
            },
        );
        sinks
    }

    fn base_info(name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(1, name, status)
            .pid(Some(4242))
            .uptime_ms(1_000)
            .build()
    }

    fn errored_info(name: &str) -> ProcessInfo {
        base_info(name, ProcStatus::Errored)
    }

    fn online_info(name: &str) -> ProcessInfo {
        base_info(name, ProcStatus::Online)
    }

    fn process_event(name: &str, kind: ProcessEventKind) -> BusEvent {
        BusEvent::Process {
            event: kind,
            info: base_info(name, ProcStatus::Online),
            manually: false,
            at_ms: 0,
        }
    }

    fn errored_event(name: &str) -> BusEvent {
        process_event(name, ProcessEventKind::Errored)
    }

    fn restart_event(name: &str) -> BusEvent {
        process_event(name, ProcessEventKind::Restart)
    }

    fn gave_up_rules() -> Rules {
        let sinks = one_sink("ops");
        Rules::new(
            vec![Rule {
                when: Trigger::GaveUp {},
                sinks: vec!["ops".to_owned()],
                debounce: default_debounce(),
            }],
            &sinks,
        )
        .unwrap()
    }

    fn restart_rate_rules(restarts: u32, within: UpDuration) -> Rules {
        let sinks = one_sink("ops");
        Rules::new(
            vec![Rule {
                when: Trigger::RestartRate { restarts, within },
                sinks: vec!["ops".to_owned()],
                debounce: default_debounce(),
            }],
            &sinks,
        )
        .unwrap()
    }

    fn rule_to(sink: &str) -> Rule {
        Rule {
            when: Trigger::GaveUp {},
            sinks: vec![sink.to_owned()],
            debounce: default_debounce(),
        }
    }

    #[test]
    fn an_errored_seen_by_both_routes_fires_once() {
        let mut rules = gave_up_rules();
        let first = rules.on_event(&errored_event("web"), 1_000);
        assert_eq!(first.len(), 1);
        let second = rules.on_poll(&[errored_info("web")], 2_000);
        assert!(second.is_empty(), "the debounce covers the other route");
    }

    #[test]
    fn the_poll_fires_what_the_bus_never_carried() {
        let mut rules = gave_up_rules();
        let fired = rules.on_poll(&[errored_info("web")], 1_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].bark.subject, "web");
    }

    #[test]
    fn one_flapping_sheep_does_not_mute_another_going_down() {
        let mut rules = gave_up_rules();
        assert_eq!(rules.on_event(&errored_event("web"), 1_000).len(), 1);
        assert_eq!(rules.on_event(&errored_event("api"), 1_100).len(), 1);
        assert!(rules.on_event(&errored_event("web"), 1_200).is_empty());
    }

    /// `info.restarts` says 9; only 3 restart events are fed through
    /// `on_event`, so passing requires reading `info.restarts` rather than
    /// a tally kept from the events.
    #[test]
    fn the_early_warning_counts_the_shepherds_restarts_and_not_its_own() {
        let mut rules = restart_rate_rules(5, UpDuration::from_millis(60_000));
        for at in [1_000, 2_000, 3_000] {
            let _ = rules.on_event(&restart_event("web"), at);
        }
        let mut info = online_info("web");
        info.restarts = 9;
        let fired = rules.on_poll(&[info], 4_000);
        assert_eq!(
            fired.len(),
            1,
            "9 restarts crosses a threshold of 5; 3 does not"
        );
    }

    #[test]
    fn a_rule_routed_at_a_sink_that_does_not_exist_is_refused_at_startup() {
        let err = Rules::new(vec![rule_to("pager")], &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, RulesError::UnknownSink { .. }));
        // Exact, not a `contains`: this string reaches an operator through
        // `Display`.
        assert_eq!(
            err.to_string(),
            "rule 0 routes to sink \"pager\", which [bark.sinks] in dogs.toml does not define"
        );
    }

    #[test]
    fn a_bark_with_sinks_and_no_rules_still_alerts_when_the_shepherd_gives_up() {
        let sinks = one_sink("ops");
        let rules = Rules::default_rules(&sinks);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].when, Trigger::GaveUp {});
        assert_eq!(rules[0].sinks, vec!["ops"]);
    }

    #[test]
    fn a_rule_with_no_sinks_at_all_is_refused_at_startup() {
        let rule = Rule {
            when: Trigger::GaveUp {},
            sinks: Vec::new(),
            debounce: default_debounce(),
        };
        let err = Rules::new(vec![rule], &one_sink("ops")).unwrap_err();
        assert!(matches!(err, RulesError::NoSinks { .. }));
    }

    #[test]
    fn an_event_rule_naming_an_unknown_kind_is_refused_at_startup() {
        let rule = Rule {
            when: Trigger::Event {
                kinds: vec!["exit".to_owned(), "not_a_real_kind".to_owned()],
            },
            sinks: vec!["ops".to_owned()],
            debounce: default_debounce(),
        };
        let err = Rules::new(vec![rule], &one_sink("ops")).unwrap_err();
        assert!(matches!(err, RulesError::UnknownKind { .. }));
        assert!(err.to_string().contains("not_a_real_kind"));
    }

    #[test]
    fn an_event_rule_does_not_fire_on_a_kind_it_was_not_given() {
        let sinks = one_sink("ops");
        let mut rules = Rules::new(
            vec![Rule {
                when: Trigger::Event {
                    kinds: vec!["exit".to_owned()],
                },
                sinks: vec!["ops".to_owned()],
                debounce: default_debounce(),
            }],
            &sinks,
        )
        .unwrap();
        let online = rules.on_event(&process_event("web", ProcessEventKind::Online), 1_000);
        assert!(online.is_empty(), "online was not in the configured kinds");
        let exit = rules.on_event(&process_event("web", ProcessEventKind::Exit), 1_100);
        assert_eq!(exit.len(), 1, "exit was, and should still fire");
    }

    #[test]
    fn restart_rate_fires_at_the_threshold_and_not_one_below_it() {
        let mut rules = restart_rate_rules(5, UpDuration::from_millis(60_000));
        let mut below = online_info("web");
        below.restarts = 4;
        assert!(
            rules.on_poll(&[below], 1_000).is_empty(),
            "4 restarts is below a threshold of 5"
        );
        let mut at = online_info("web");
        at.restarts = 5;
        assert_eq!(
            rules.on_poll(&[at], 1_100).len(),
            1,
            "5 restarts meets a threshold of 5"
        );
    }

    /// Debounce is zeroed: only the window's own logic keeps this quiet.
    #[test]
    fn restart_rate_window_slides_once_it_elapses() {
        let sinks = one_sink("ops");
        let mut rules = Rules::new(
            vec![Rule {
                when: Trigger::RestartRate {
                    restarts: 5,
                    within: UpDuration::from_millis(1_000),
                },
                sinks: vec!["ops".to_owned()],
                debounce: UpDuration::from_millis(0),
            }],
            &sinks,
        )
        .unwrap();

        let mut info = online_info("web");
        info.restarts = 5;
        assert_eq!(
            rules.on_poll(&[info.clone()], 0).len(),
            1,
            "5 restarts opens the window past threshold"
        );

        // Window elapsed and the count did not move: it resets, with
        // nothing new to warn about.
        assert!(
            rules.on_poll(&[info.clone()], 2_000).is_empty(),
            "no new restarts since the window reset"
        );

        // Five more restarts inside the new window crosses it again.
        info.restarts = 10;
        assert_eq!(
            rules.on_poll(&[info], 2_100).len(),
            1,
            "5 more restarts inside the new window crosses it again"
        );
    }

    #[test]
    fn memory_above_fires_at_the_ceiling_and_not_one_byte_below_it() {
        let sinks = one_sink("ops");
        let mut rules = Rules::new(
            vec![Rule {
                when: Trigger::MemoryAbove {
                    bytes: MemSize::from_bytes(1_000),
                },
                sinks: vec!["ops".to_owned()],
                debounce: default_debounce(),
            }],
            &sinks,
        )
        .unwrap();

        let mut below = online_info("web");
        below.memory_bytes = Some(999);
        assert!(
            rules.on_poll(&[below], 1_000).is_empty(),
            "999 is below 1000"
        );

        let mut at = online_info("web");
        at.memory_bytes = Some(1_000);
        assert_eq!(rules.on_poll(&[at], 1_100).len(), 1, "1000 meets 1000");
    }

    /// Unknown memory (a stopped sheep, or one not yet sampled) must read
    /// as "cannot alert", never as zero.
    #[test]
    fn memory_above_does_not_fire_when_usage_is_unknown() {
        let sinks = one_sink("ops");
        let mut rules = Rules::new(
            vec![Rule {
                when: Trigger::MemoryAbove {
                    bytes: MemSize::from_bytes(1_000),
                },
                sinks: vec!["ops".to_owned()],
                debounce: default_debounce(),
            }],
            &sinks,
        )
        .unwrap();
        let info = online_info("web");
        assert!(info.memory_bytes.is_none());
        assert!(rules.on_poll(&[info], 1_000).is_empty());
    }

    #[test]
    fn gave_up_does_not_fire_on_event_for_a_non_errored_kind() {
        let mut rules = gave_up_rules();
        let online = rules.on_event(&process_event("web", ProcessEventKind::Online), 1_000);
        assert!(online.is_empty(), "GaveUp fires on Errored only");
        let restart = rules.on_event(&restart_event("web"), 1_100);
        assert!(restart.is_empty(), "GaveUp fires on Errored only");
    }

    #[test]
    fn gave_up_does_not_fire_on_poll_for_a_non_errored_status() {
        let mut rules = gave_up_rules();
        let fired = rules.on_poll(&[online_info("web")], 1_000);
        assert!(fired.is_empty(), "GaveUp fires when status is Errored only");
    }

    #[test]
    fn debounce_boundary_is_inclusive_at_exactly_its_own_duration() {
        let mut rules = gave_up_rules();
        let debounce_ms = default_debounce().as_millis();
        assert_eq!(rules.on_event(&errored_event("web"), 0).len(), 1);
        assert!(
            rules
                .on_event(&errored_event("web"), debounce_ms - 1)
                .is_empty(),
            "one millisecond short of the debounce must still be quiet"
        );
        assert_eq!(
            rules.on_event(&errored_event("web"), debounce_ms).len(),
            1,
            "exactly at the debounce it may fire again"
        );
    }

    // The tests above build `Rule`/`Trigger` as Rust values, never running
    // `Deserialize`. The tests below parse real TOML strings.

    /// The exact shape `docs/dogs.md` and `web/src/pages/docs/dogs.astro`
    /// publish as copy-pasteable.
    #[test]
    fn the_docs_gave_up_rule_parses_from_toml() {
        let rule: Rule = toml::from_str(
            r#"
on = "gave_up"
sinks = ["oncall", "audit"]
"#,
        )
        .unwrap();
        assert_eq!(rule.when, Trigger::GaveUp {});
        assert_eq!(rule.sinks, vec!["oncall", "audit"]);
        assert_eq!(rule.debounce, default_debounce(), "no override in the TOML");
    }

    /// `within`'s `"2m"` form uses [`UpDuration`]'s own duration grammar.
    #[test]
    fn the_docs_restart_rate_rule_parses_from_toml() {
        let rule: Rule = toml::from_str(
            r#"
on = "restart_rate"
restarts = 5
within = "2m"
sinks = ["oncall"]
"#,
        )
        .unwrap();
        assert_eq!(
            rule.when,
            Trigger::RestartRate {
                restarts: 5,
                within: UpDuration::from_millis(2 * 60_000),
            }
        );
    }

    /// Not in the published docs, but a real `Trigger` variant a rule can
    /// name.
    #[test]
    fn an_event_rule_parses_from_toml() {
        let rule: Rule = toml::from_str(
            r#"
on = "event"
kinds = ["exit", "errored"]
sinks = ["oncall"]
"#,
        )
        .unwrap();
        assert_eq!(
            rule.when,
            Trigger::Event {
                kinds: vec!["exit".to_owned(), "errored".to_owned()],
            }
        );
    }

    /// `bytes`'s `"512M"` form uses [`MemSize`]'s own grammar.
    #[test]
    fn a_memory_above_rule_parses_from_toml() {
        let rule: Rule = toml::from_str(
            r#"
on = "memory_above"
bytes = "512M"
sinks = ["oncall"]
"#,
        )
        .unwrap();
        assert_eq!(
            rule.when,
            Trigger::MemoryAbove {
                // Binary units: MemSize's grammar is MiB, not MB.
                bytes: MemSize::from_bytes(512 * 1024 * 1024),
            }
        );
    }

    #[test]
    fn a_rule_s_debounce_override_parses_from_toml() {
        let rule: Rule = toml::from_str(
            r#"
on = "gave_up"
sinks = ["oncall"]
debounce = "10m"
"#,
        )
        .unwrap();
        assert_eq!(rule.debounce, UpDuration::from_millis(10 * 60_000));
    }

    #[test]
    fn a_misspelled_trigger_field_is_refused_with_the_bad_key_named() {
        let err = toml::from_str::<Rule>(
            r#"
on = "restart_rate"
retsarts = 5
within = "2m"
sinks = ["oncall"]
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("retsarts"),
            "the error must name the misspelled key, not just fail: {err}"
        );
    }

    /// A bare `GaveUp` unit variant would skip field checking for the rest
    /// of the map, missing exactly this typo.
    #[test]
    fn a_misspelled_field_next_to_gave_up_is_still_refused() {
        let err = toml::from_str::<Rule>(
            r#"
on = "gave_up"
sinks = ["oncall"]
debuonce = "10m"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("debuonce"),
            "the error must name the misspelled key, not just fail: {err}"
        );
    }

    /// `sinsk` matches no field on `Rule` or `Trigger`, so it is simply
    /// absent, and the error names the missing `sinks` field instead of
    /// the typo.
    #[test]
    fn a_misspelled_sinks_field_is_refused_as_a_missing_field() {
        let err = toml::from_str::<Rule>(
            r#"
on = "gave_up"
sinsk = ["oncall"]
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("sinks"),
            "the error must name the missing field: {err}"
        );
    }

    #[test]
    fn an_unknown_on_variant_is_refused_with_the_bad_value_named() {
        let err = toml::from_str::<Rule>(
            r#"
on = "gav_up"
sinks = ["oncall"]
"#,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("gav_up"),
            "the error must name the bad value: {message}"
        );
        assert!(
            message.contains("gave_up"),
            "the error must also name a real variant, so a typo suggests its own fix: {message}"
        );
    }
}
