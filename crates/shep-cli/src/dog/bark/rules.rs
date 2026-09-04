//! What makes a bark fire: [`Rule`], [`Trigger`] and the [`Rules`] engine
//! that turns a bus event or a reconciliation poll into zero or more
//! [`Firing`]s.
//!
//! **The bus drops events.** `tokio::sync::broadcast` drops what a lagging
//! subscriber cannot keep up with rather than queueing it — the daemon
//! surfaces that as `BusEvent::Dropped` — so a dog that only listens to
//! [`Rules::on_event`] will miss some. [`Rules::on_poll`] is how bark
//! reconciles: it evaluates the same rule set against a *level* (the
//! flock's current [`ProcessInfo`] snapshot) rather than an *edge* (one bus
//! event), so a firing the bus lost still lands the next time the dog
//! polls. The two routes share one piece of state — [`Rules`]'s own
//! `subjects` map — keyed by subject name, which is what lets a rule that
//! fired off the bus and would also fire off the very next poll fire only
//! once: the debounce it records covers both routes, because it does not
//! know or care which one recorded it.
//!
//! **Debounce is per rule per subject, never global.** A global debounce
//! means the second sheep to go down during an incident is silent, and
//! that is the incident's most interesting fact.
//!
//! [`super::run_loop`] (Task 21) is what actually calls `on_event` and
//! `on_poll`, wiring this module and [`super::sinks`] together into a
//! running dog.

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
// The attribute enforcing that sits on `Trigger` rather than here, and it
// cannot sit here: serde does not support `deny_unknown_fields` alongside
// `#[serde(flatten)]` on one struct. The flattened field has to collect
// whatever keys this struct's own named fields do not claim, and
// `deny_unknown_fields` rejects exactly those before the flattened field
// ever sees them, so every key of a real rule, `on` included, reads as
// unknown from `Rule`'s point of view. Everything `sinks` and `debounce`
// do not consume flows into `Trigger`'s deserialize instead, which catches
// the typo one level down from where the attribute used to sit.
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
    // An empty struct variant rather than a bare unit variant,
    // deliberately, and load-bearing for the `deny_unknown_fields` above.
    // An internally tagged UNIT variant deserializes through a path that
    // never visits the rest of the map, so a stray key beside
    // `on = "gave_up"` (a misspelled `debounce`, say) parsed silently even
    // with the attribute set. Measured, not assumed, and covered by
    // `tests::a_misspelled_field_next_to_gave_up_is_still_refused` below.
    // A struct variant, even an empty one, goes through the same
    // field-checking visitor every other variant here already used. The
    // wire shape is identical either way: `on = "gave_up"` on its own
    // still parses to this variant.
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

/// `kind`'s wire spelling — the same string `[dog.bark.rules]`'s `kinds`
/// list names it by. Reads it off `ProcessEventKind`'s own
/// `Serialize` (`rename_all = "snake_case"`) rather than hand-listing the
/// variants a second time, so a new variant never needs this file updated
/// to be matchable. Never fails in practice — every variant serializes to
/// a bare string — and falls back to an empty string, which cannot equal
/// any configured kind, rather than panicking, on the day that stops being
/// true.
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
    /// - [`RulesError::UnknownSink`] — a rule routes to a sink name
    ///   `[dog.bark.sinks]` does not define. Refused at startup rather than
    ///   at 3am: the rule would fire correctly and deliver nowhere.
    /// - [`RulesError::NoSinks`] — a rule routes to none at all.
    /// - [`RulesError::UnknownKind`] — an `Event` rule names an event kind
    ///   that is not on the wire, which is a typo and not a future event:
    ///   bark and the shepherd ship in one binary.
    /// - [`RulesError::InsecureSink`] — a `[dog.bark.sinks]` entry is a
    ///   Discord or Slack webhook configured with `http://`. Checked
    ///   against every sink, whether or not any rule currently routes to
    ///   it — an unused insecure sink is still sitting in the config as a
    ///   footgun for the next rule that does.
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

    /// Whether rule `idx` may fire for `subject` right now, and records the
    /// firing when it can. The one piece of state a bus-route firing and a
    /// poll-route firing both consult — the mechanism behind "an `Errored`
    /// seen by both routes fires once."
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

    /// Whether a `RestartRate` rule's window has accumulated `threshold` or
    /// more restarts for `subject`, sliding the window forward once
    /// `within` has elapsed since it opened.
    ///
    /// The window's baseline restart count starts at zero the first time a
    /// subject is observed by this rule — bark cannot know when restarts
    /// that predate its own first poll happened, and for an early-warning
    /// rule the conservative reading of unknown history is to count it,
    /// not discount it. From then on the window only carries restarts that
    /// happened inside it: once `within` has passed since the window
    /// opened with no new firing, the next poll resets the baseline to the
    /// count it already saw, so a sheep that stopped flapping stops
    /// re-triggering the rule just because the old count is still above
    /// threshold.
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
    /// Reads `ProcessInfo::restarts` — the shepherd's own count — rather
    /// than a tally bark kept. A private tally drifts from the number the
    /// shepherd acts on, and the operator would be told a different story
    /// from the one the supervisor believes.
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
    /// The record, with [`Bark::sinks`] still EMPTY: what each sink made of
    /// it is not known until it has been tried, and the loop fills that in
    /// before the record is written. A `Firing` carrying delivery outcomes
    /// would be claiming a delivery that has not happened.
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

    /// fails if the same `Errored` fires twice when both routes see it —
    /// once off the bus, once off the poll a second later. An operator
    /// paged twice for one outage stops trusting the page, and this is the
    /// shape reconciliation introduces the moment it exists.
    #[test]
    fn an_errored_seen_by_both_routes_fires_once() {
        let mut rules = gave_up_rules();
        let first = rules.on_event(&errored_event("web"), 1_000);
        assert_eq!(first.len(), 1);
        let second = rules.on_poll(&[errored_info("web")], 2_000);
        assert!(second.is_empty(), "the debounce covers the other route");
    }

    /// fails if the poll cannot fire what the bus never delivered — which
    /// is the entire reason bark polls at all.
    #[test]
    fn the_poll_fires_what_the_bus_never_carried() {
        let mut rules = gave_up_rules();
        let fired = rules.on_poll(&[errored_info("web")], 1_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].bark.subject, "web");
    }

    /// fails if the debounce is global rather than per subject. The second
    /// sheep to go down during an incident is the incident's most
    /// interesting fact, and a global debounce silences it.
    #[test]
    fn one_flapping_sheep_does_not_mute_another_going_down() {
        let mut rules = gave_up_rules();
        assert_eq!(rules.on_event(&errored_event("web"), 1_000).len(), 1);
        assert_eq!(rules.on_event(&errored_event("api"), 1_100).len(), 1);
        assert!(rules.on_event(&errored_event("web"), 1_200).is_empty());
    }

    /// fails if bark keeps its own restart tally. The shepherd's count is
    /// the number it acts on; a private one drifts, and the operator is
    /// told a different story from the one the supervisor believes. The
    /// fixture makes them DISAGREE — the info says 9, and bark has seen
    /// three events — so an implementation reading either one passes only
    /// if it reads the right one.
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

    /// fails if a rule routed at a sink nobody defined is accepted. It
    /// would fire correctly for months and deliver nowhere, and the first
    /// time anyone finds out is the incident it was written for.
    #[test]
    fn a_rule_routed_at_a_sink_that_does_not_exist_is_refused_at_startup() {
        let err = Rules::new(vec![rule_to("pager")], &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, RulesError::UnknownSink { .. }));
        // Exact, not a `contains`, because the sink name is not the part
        // that drifted. This sentence reaches an operator's terminal
        // through `Display` rather than through an `eprintln!` literal,
        // which is how it kept sending them to `[dog.bark.sinks]` for a
        // whole branch after that section moved to `[bark.sinks]` in
        // dogs.toml: the sweep that fixed the literals could not see it.
        assert_eq!(
            err.to_string(),
            "rule 0 routes to sink \"pager\", which [bark.sinks] in dogs.toml does not define"
        );
    }

    /// fails if a `[bark]` with sinks and no rules alerts on nothing.
    /// "The shepherd gave up" is on by default with nothing to tune — that
    /// is what makes it the alert that cannot be missed.
    #[test]
    fn a_bark_with_sinks_and_no_rules_still_alerts_when_the_shepherd_gives_up() {
        let sinks = one_sink("ops");
        let rules = Rules::default_rules(&sinks);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].when, Trigger::GaveUp {});
        assert_eq!(rules[0].sinks, vec!["ops"]);
    }

    /// fails if a rule with an empty `sinks` list is accepted — the same
    /// half of the "routes nowhere" contract `UnknownSink` does not cover:
    /// an empty list names no *nonexistent* sink to complain about, so it
    /// needs its own check.
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

    /// fails if a typo'd event kind is accepted rather than refused at
    /// startup — the same "found during an incident, not before" failure
    /// mode `UnknownSink` guards against, for the other half of a rule's
    /// configuration.
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

    /// fails if an `Event` rule fires on a kind it was not configured
    /// with — the negative half of kind matching, which has no test of its
    /// own if only the positive match is checked.
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

    /// fails if `RestartRate` fires below its own threshold, or fails to
    /// fire exactly at it — the boundary on both sides, not just a value
    /// comfortably past it.
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

    /// fails if a `RestartRate` window never slides: once `within` has
    /// elapsed with no new restarts, a sheep that stopped flapping should
    /// stop re-tripping the rule just because the old count is still past
    /// threshold — debounce is zeroed here so only the window's own logic
    /// can be what keeps it quiet.
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

        // Window elapsed (2_000ms > 1_000ms since it opened at 0), and the
        // count did not move — the window resets and there is nothing new
        // to warn about.
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

    /// fails if `MemoryAbove` fires on a level below its own ceiling, or
    /// fails to fire exactly at it.
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

    /// fails if `MemoryAbove` fires (or panics) on a sheep whose memory is
    /// unknown — a stopped sheep, or one the daemon hasn't sampled yet.
    /// Unknown must read as "cannot alert", never as zero.
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

    /// fails if `GaveUp` fires on the bus route for anything other than
    /// `Errored` — the quiet half of "the alert that must not be missed".
    /// `GaveUp` is on by default with no configuration at all; a rule that
    /// fires on every event kind is as useless as one that never fires,
    /// because it trains an operator to ignore it. Proven by mutating the
    /// `on_event` match arm from `Trigger::GaveUp {} if kind ==
    /// ProcessEventKind::Errored` to an unconditional `Trigger::GaveUp {}`:
    /// before this test existed, all 14 other `rules::` tests stayed green
    /// under that mutation because none of them fed `gave_up_rules()`
    /// anything but an `Errored` event or status.
    #[test]
    fn gave_up_does_not_fire_on_event_for_a_non_errored_kind() {
        let mut rules = gave_up_rules();
        let online = rules.on_event(&process_event("web", ProcessEventKind::Online), 1_000);
        assert!(online.is_empty(), "GaveUp fires on Errored only");
        let restart = rules.on_event(&restart_event("web"), 1_100);
        assert!(restart.is_empty(), "GaveUp fires on Errored only");
    }

    /// fails if `GaveUp` fires on the poll route for a status other than
    /// `Errored` — the same quiet half as
    /// [`gave_up_does_not_fire_on_event_for_a_non_errored_kind`], for the
    /// route that has exactly the same `info.status ==
    /// ProcStatus::Errored` guard and exactly the same gap without a test.
    #[test]
    fn gave_up_does_not_fire_on_poll_for_a_non_errored_status() {
        let mut rules = gave_up_rules();
        let fired = rules.on_poll(&[online_info("web")], 1_000);
        assert!(fired.is_empty(), "GaveUp fires when status is Errored only");
    }

    /// fails if the debounce boundary is off by one in either direction:
    /// one millisecond short of it must still be quiet, and exactly at it
    /// must fire again.
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

    // The tests above all build `Rule`/`Trigger` as Rust values, which
    // never runs `Deserialize` at all and is exactly how the shipped bug
    // passed every one of them: `Rule`'s `#[serde(flatten)]` combined with
    // `#[serde(deny_unknown_fields)]` made every `[[dog.bark.rules]]` entry
    // refuse to parse, and nothing here noticed. The tests below parse
    // real TOML strings instead — see `Rule`'s and `Trigger`'s own docs
    // for the fix these prove.

    /// fails if the docs' own `on = "gave_up"` rule cannot be parsed from
    /// TOML — the exact shape `docs/dogs.md` and
    /// `web/src/pages/docs/dogs.astro` publish as copy-pasteable.
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

    /// fails if the docs' own `on = "restart_rate"` rule cannot be parsed
    /// from TOML, or if `within`'s `"2m"` string form (the same
    /// [`UpDuration`] grammar every other duration field in `shep.toml`
    /// accepts) is not honored inside a flattened [`Trigger`].
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

    /// fails if an `event` rule cannot be parsed from TOML — not shown in
    /// the published docs, but a real `Trigger` variant a `[[bark.rules]]`
    /// entry can name, and the same flatten mechanism the docs' two forms
    /// exercise.
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

    /// fails if a `memory_above` rule cannot be parsed from TOML, or if
    /// `bytes`'s `"512M"` string form ([`MemSize`]'s own grammar) is not
    /// honored inside a flattened [`Trigger`] — the fourth and last
    /// variant, rounding out coverage of every rule form this parser
    /// accepts.
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
                // Binary units — MemSize's grammar is MiB, not MB.
                bytes: MemSize::from_bytes(512 * 1024 * 1024),
            }
        );
    }

    /// fails if a rule's own `debounce` override does not survive parsing
    /// alongside a flattened `Trigger` — [`Rule`]'s one other field beyond
    /// `sinks`, and the one most likely to silently regress if a future
    /// change reshuffles which fields `Rule` claims directly versus
    /// forwards to `Trigger`.
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

    /// fails if a misspelled key inside a trigger with its own fields
    /// (`restarts` typo'd as `retsarts`) is silently accepted rather than
    /// refused with the bad key named — the protection
    /// `#[serde(deny_unknown_fields)]` exists for, now living on
    /// [`Trigger`] rather than [`Rule`]. See [`Rule`]'s own doc for why.
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

    /// fails if a misspelled key sitting next to `on = "gave_up"` is
    /// silently accepted — the exact gap a bare `Trigger::GaveUp` unit
    /// variant left even with `deny_unknown_fields` set, because `serde`
    /// deserializes an internally tagged unit variant through a path that
    /// never inspects the rest of the map. Proven by mutating `GaveUp {}`
    /// (an empty *struct* variant) back to a bare `GaveUp` unit variant:
    /// every other test in this file still passes, since none of them
    /// parses a misspelled field next to `on = "gave_up"` — this is the
    /// one that would have caught the exact way the shipped fix's own
    /// protection could still leak.
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

    /// fails if a misspelled `sinks` is accepted rather than refused. The
    /// error names the field that is now missing (`sinks`, required with
    /// no default) rather than the typo'd key itself (`sinsk` never
    /// matches any field `Rule` or `Trigger` know about, so it is simply
    /// absent from both) — still a startup refusal an operator can act on,
    /// just phrased from the other direction than the trigger-field and
    /// `GaveUp`-neighbor cases above.
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

    /// fails if an unknown `on` variant is accepted rather than refused
    /// with the bad value and the known ones both named.
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
