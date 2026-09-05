//! The shapes whistle's tools return.
//!
//! Structural twins of `shep_core`'s own types, with `schemars::JsonSchema`
//! derived on top so rmcp can declare each tool's output schema; this
//! module's equality tests keep a twin and its source serializing
//! identically.
//!
//! MCP has its own envelope (`CallToolResult`, `structuredContent`), so
//! these types never nest `output::OutputEnvelope`: its `schema_version`
//! and `command` fields serve a shell script, not an agent.

use schemars::JsonSchema;
use serde::Serialize;
use shep_core::barks::{Bark, SinkOutcome};
use shep_core::protocol::{DogSource, ExitInfo, Lamb, ProcessInfo};

use crate::dog::metrics::HostReading;

/// Every list-shaped tool's payload: rows under a named field.
///
/// **Not a bare `Vec`.** `Json<T>` hands `T` straight to
/// `CallToolResult::structured`, which puts it in `structured_content` —
/// `structuredContent` on the wire, which MCP types as an object. A `Vec`
/// would put a JSON array there. rmcp 3.1.2 does not stop it (its
/// `schema_for_output` stopped validating root types per SEP-2106), so this
/// would be wrong quietly rather than loudly, which is worse.
///
/// It also leaves room: a listing that later needs a `total` or a
/// `truncated` beside its rows can grow one without changing the tool's
/// output shape from array to object, which IS a breaking change for a
/// consumer.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FlockListing {
    /// The matched sheep and dogs, in the order the shepherd reported them.
    pub flock: Vec<SheepRow>,
}

/// `list_barks`' payload. Same rule, same reason as [`FlockListing`].
#[derive(Debug, Serialize, JsonSchema)]
pub struct BarkListing {
    /// The most recent alerts, oldest first.
    pub barks: Vec<BarkRow>,
}

/// One sheep, exactly as `shep flock --format json` renders it.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SheepRow {
    /// Stable numeric id.
    pub id: u32,
    /// The sheep's name.
    pub name: String,
    /// One of `starting`, `online`, `stopping`, `stopped`, `errored`,
    /// `waiting-restart`.
    pub status: String,
    /// OS pid while running.
    pub pid: Option<u32>,
    /// Restarts since registration.
    pub restarts: u32,
    /// Milliseconds since the last successful start.
    pub uptime_ms: u64,
    /// Fold membership.
    pub fold: Option<String>,
    /// Resolved stdout log path.
    pub out_file: Option<String>,
    /// Resolved stderr log path.
    pub err_file: Option<String>,
    /// Tree CPU as a percentage of one core; absent until a baseline exists.
    pub cpu_percent: Option<f32>,
    /// Tree resident set size in bytes.
    pub memory_bytes: Option<u64>,
    /// Present when this row is a dog rather than a sheep.
    pub dog: Option<DogRow>,
    /// Process-tree members, when the reply walked for them (`describe`
    /// does, `list` does not).
    pub lambs: Option<Vec<LambRow>>,
    /// How this sheep's process most recently stopped; absent while it has
    /// never exited under this daemon.
    pub last_exit: Option<ExitInfoRow>,
    /// The marker a dog has asked to have painted beside this sheep; absent
    /// when none has. Opaque text the daemon validated but never parsed.
    pub smit: Option<String>,
    /// Which instance slot of its app this sheep occupies, counting from 0;
    /// absent when the peer daemon predates the field.
    pub instance: Option<u32>,
    /// Whether this DOG has completed a handshake with the shepherd;
    /// absent for a sheep, which has none to complete.
    ///
    /// `false` is the one worth acting on: the dog's process is running and
    /// the shepherd has never heard from it, so it is not doing its job
    /// however healthy `status` looks. `status` still reads `online` there,
    /// truthfully — it describes the process, not the relationship.
    pub handshook: Option<bool>,
    /// Whether the shepherd has GIVEN UP on this dog — restarted it once
    /// for never answering, watched that not help, and stopped restarting
    /// it; absent for a sheep, which is never given up on.
    ///
    /// Not derivable from `handshook`, and that is why it is here. A dog
    /// spawned a moment ago and a dog the shepherd will never touch again
    /// are both `handshook: false` with a live process. `true` here says
    /// nothing more will happen on its own; the reason lives in that dog's
    /// own log (`shep bleats <name>`), which is the only place the shepherd
    /// recorded what it actually saw.
    pub dog_stale: Option<bool>,
    /// The `AppConfig` field names this sheep's spec differs from a load's
    /// parked config for; absent when nothing is parked. Names only, never
    /// values (IR-41), the same guarantee `ProcessInfo::pending` carries.
    ///
    /// `skip_serializing_if`, matching `ProcessInfo::pending` exactly: this
    /// type's own doc promises byte-identical JSON, and `ProcessInfo`
    /// carries this field the same way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<String>>,
    /// The `AppConfig` field names an operator has overridden on this sheep
    /// that its current Flockfile does not declare; absent when there is
    /// nothing to report. Names only, never values (IR-41), the same
    /// guarantee `ProcessInfo::overridden` carries.
    ///
    /// `skip_serializing_if`, for the same reason `Self::pending` carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overridden: Option<Vec<String>>,
}

/// Where a dog came from. Mirrors `DogSource`'s tagged wire shape exactly.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DogRow {
    /// An argv branch of the shep binary itself.
    BuiltIn,
    /// A binary an operator adopted.
    Adopted {
        /// The path, as the operator gave it to `shep adopt`.
        path: String,
    },
    /// A source kind this build predates.
    ///
    /// `DogSource` is `#[non_exhaustive]` (IR-20), so `From<&DogSource>`
    /// cannot be a two-arm match — the compiler refuses it. This mirrors
    /// `output::rows::dog_source_label`'s own "unknown" fallback for the
    /// same enum, so a future daemon reporting a source kind this whistle
    /// predates gets a row rather than a build failure.
    Unknown,
}

/// One process the OS reports as a descendant of a sheep.
#[derive(Debug, Serialize, JsonSchema)]
pub struct LambRow {
    /// The lamb's own pid.
    pub pid: u32,
    /// The executable's name, as the OS reports it. Never its command line.
    pub name: String,
}

/// Why a sheep's process most recently stopped. Mirrors `ExitInfo`'s wire
/// shape exactly.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ExitInfoRow {
    /// The process's own exit code, on a normal exit.
    pub code: Option<i32>,
    /// The raw unix signal number that ended it, when it did not exit on
    /// its own.
    pub signal: Option<i32>,
}

impl From<&ProcessInfo> for SheepRow {
    fn from(info: &ProcessInfo) -> Self {
        Self {
            id: info.id,
            name: info.name.clone(),
            status: info.status.to_string(),
            pid: info.pid,
            restarts: info.restarts,
            uptime_ms: info.uptime_ms,
            fold: info.fold.clone(),
            out_file: info.out_file.clone(),
            err_file: info.err_file.clone(),
            cpu_percent: info.cpu_percent,
            memory_bytes: info.memory_bytes,
            dog: info.dog.as_ref().map(DogRow::from),
            lambs: info
                .lambs
                .as_ref()
                .map(|lambs| lambs.iter().map(LambRow::from).collect()),
            last_exit: info.last_exit.as_ref().map(ExitInfoRow::from),
            smit: info.smit.clone(),
            instance: info.instance,
            handshook: info.handshook,
            dog_stale: info.dog_stale,
            pending: info.pending.clone(),
            overridden: info.overridden.clone(),
        }
    }
}

impl From<&ExitInfo> for ExitInfoRow {
    fn from(exit: &ExitInfo) -> Self {
        Self {
            code: exit.code,
            signal: exit.signal,
        }
    }
}

impl From<&DogSource> for DogRow {
    fn from(source: &DogSource) -> Self {
        match source {
            DogSource::BuiltIn => Self::BuiltIn,
            DogSource::Adopted { path } => Self::Adopted { path: path.clone() },
            _ => Self::Unknown,
        }
    }
}

impl From<&Lamb> for LambRow {
    fn from(lamb: &Lamb) -> Self {
        Self {
            pid: lamb.pid,
            name: lamb.name.clone(),
        }
    }
}

/// One alert, exactly as `shep barks --format json` renders it.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BarkRow {
    /// Unix millis when the alert fired.
    pub at_ms: u64,
    /// The rule that fired, or `daemon` when the shepherd wrote this itself.
    pub rule: String,
    /// What it is about: a sheep's name, or a dog's.
    pub subject: String,
    /// The human-readable line.
    pub message: String,
    /// Which sinks took it. Empty when the shepherd wrote the record itself.
    pub sinks: Vec<SinkOutcomeRow>,
}

/// What one sink made of one alert. Names the sink by its
/// `[dog.bark.sinks]` config key, never by its webhook URL — the property
/// `Bark`'s own doc calls the reason that type is safe to print, carried
/// across to the twin so it stays true here.
#[derive(Debug, Serialize, JsonSchema)]
pub struct SinkOutcomeRow {
    /// The sink's name from `[dog.bark.sinks]`.
    pub sink: String,
    /// `None` when it was delivered; the failure otherwise.
    pub error: Option<String>,
}

impl From<&Bark> for BarkRow {
    fn from(bark: &Bark) -> Self {
        Self {
            at_ms: bark.at_ms,
            rule: bark.rule.clone(),
            subject: bark.subject.clone(),
            message: bark.message.clone(),
            sinks: bark.sinks.iter().map(SinkOutcomeRow::from).collect(),
        }
    }
}

impl From<&SinkOutcome> for SinkOutcomeRow {
    fn from(outcome: &SinkOutcome) -> Self {
        Self {
            sink: outcome.sink.clone(),
            error: outcome.error.clone(),
        }
    }
}

/// What `get_metrics` returns: the flock's own numbers plus the machine's.
#[derive(Debug, Serialize, JsonSchema)]
pub struct MetricsReading {
    /// The shepherd's crate version, from the handshake.
    ///
    /// From [`super::shepherd::Shepherd::call_with_ack`], not from the
    /// reply: the handshake lives on the `Client` (`Client::daemon() ->
    /// &HelloAck`, shep-client/src/client.rs:175) and plain `call` drops the
    /// client before it returns, so `get_metrics` would have no way to fill
    /// this field.
    pub daemon_version: String,
    /// The shepherd's pid, from the same handshake and the same call.
    pub daemon_pid: u32,
    /// Every registered entry, sheep and dogs alike.
    pub flock: Vec<SheepRow>,
    /// Host totals, absent on a platform `sysinfo` does not support.
    pub host: Option<HostRow>,
}

/// The machine the flock runs on.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HostRow {
    /// Total physical memory in bytes.
    pub memory_total_bytes: u64,
    /// Memory in use, as the platform reports it.
    pub memory_used_bytes: u64,
    /// How many processes the host is running, the flock included.
    pub processes: u64,
    /// Seconds since the host booted.
    pub uptime_seconds: u64,
}

impl From<&HostReading> for HostRow {
    fn from(host: &HostReading) -> Self {
        Self {
            memory_total_bytes: host.memory_total_bytes,
            memory_used_bytes: host.memory_used_bytes,
            // usize to u64 is infallible: every target this workspace
            // ships is 64-bit.
            processes: host.processes as u64,
            uptime_seconds: host.uptime_seconds,
        }
    }
}

/// What `tail_bleats` returns.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BleatTail {
    /// The sheep this came from.
    pub name: String,
    /// The id it resolved to.
    pub id: u32,
    /// Lines from the stdout log, oldest first. Empty when the file is
    /// missing or the sheep never had one.
    pub out: Vec<String>,
    /// Lines from the stderr log, oldest first.
    pub err: Vec<String>,
    /// True when the tail was cut short — by the line cap, by the 256 KiB
    /// read window, or both — rather than reaching the start of the file.
    /// A model that cannot tell "this is all of it" from "this is the last
    /// 50" will draw the wrong conclusion from a quiet log.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::{DogSource, Lamb, ProcessInfo};
    use shep_core::status::ProcStatus;

    /// Deep equality of the serialized values, not a key-set check: a field
    /// that keeps its name but changes shape fails here too.
    ///
    /// Every `Option` field on the fixture is `Some`, since a `None` with
    /// `skip_serializing_if` omits the key on both sides and would hide a
    /// missing `SheepRow` field.
    #[test]
    fn a_sheep_row_serializes_exactly_as_process_info_does() {
        let info = ProcessInfo::builder(7, "api", ProcStatus::WaitingRestart)
            .pid(Some(4242))
            .restarts(3)
            .uptime_ms(61_000)
            .fold(Some("web".to_string()))
            .out_file(Some("/tmp/api-out.log".to_string()))
            .err_file(Some("/tmp/api-err.log".to_string()))
            .cpu_percent(Some(12.5))
            .memory_bytes(Some(1024 * 1024))
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/dog".to_string(),
            }))
            .lambs(Some(vec![Lamb::new(4243, "node")]))
            .pending(Some(vec!["env".to_string()]))
            .overridden(Some(vec!["cwd".to_string()]))
            .build();

        assert_eq!(
            serde_json::to_value(SheepRow::from(&info)).unwrap(),
            serde_json::to_value(&info).unwrap(),
            "whistle and `--format json` must describe a sheep identically"
        );
    }

    /// A stopped sheep has `None` in six places; catches a twin that
    /// renders `null` for a different reason than `ProcessInfo` does.
    #[test]
    fn an_empty_sheep_row_serializes_exactly_as_process_info_does_too() {
        let info = ProcessInfo::builder(1, "idle", ProcStatus::Stopped).build();
        assert_eq!(
            serde_json::to_value(SheepRow::from(&info)).unwrap(),
            serde_json::to_value(&info).unwrap()
        );
    }

    /// Fully populated, since a `skip_serializing_if` field is simply
    /// absent from `emitted` when `None`, and an all-`None` fixture would
    /// never test whether `pending`/`overridden` are in the schema.
    #[test]
    fn the_generated_schema_names_every_field_the_row_carries() {
        let schema = serde_json::to_value(schemars::schema_for!(SheepRow)).unwrap();
        let properties = schema["properties"].as_object().expect("an object schema");
        let info = ProcessInfo::builder(7, "api", ProcStatus::WaitingRestart)
            .pid(Some(4242))
            .restarts(3)
            .uptime_ms(61_000)
            .fold(Some("web".to_string()))
            .out_file(Some("/tmp/api-out.log".to_string()))
            .err_file(Some("/tmp/api-err.log".to_string()))
            .cpu_percent(Some(12.5))
            .memory_bytes(Some(1024 * 1024))
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/dog".to_string(),
            }))
            .lambs(Some(vec![Lamb::new(4243, "node")]))
            .pending(Some(vec!["env".to_string()]))
            .overridden(Some(vec!["cwd".to_string()]))
            .build();
        let emitted = serde_json::to_value(&info).unwrap();
        for key in emitted.as_object().unwrap().keys() {
            assert!(
                properties.contains_key(key),
                "the schema is missing `{key}`, which the tool returns"
            );
        }
    }

    /// `structuredContent` must be an object on the wire. `schema_for_output`
    /// (rmcp 3.1.2) does not validate the root type, so a `Vec` here would
    /// pass silently; this test is the only guard against it.
    ///
    /// Input schemas are validated by the `#[tool]` macro itself, at router
    /// construction, so they need no equivalent test.
    #[test]
    fn every_declared_tool_shape_is_object_rooted() {
        for (label, schema) in [
            ("FlockListing", schemars::schema_for!(FlockListing)),
            ("BarkListing", schemars::schema_for!(BarkListing)),
            ("MetricsReading", schemars::schema_for!(MetricsReading)),
            ("BleatTail", schemars::schema_for!(BleatTail)),
        ] {
            let value = serde_json::to_value(schema).unwrap();
            assert_eq!(
                value["type"], "object",
                "{label} is a tool's declared output and must be object-rooted"
            );
        }
    }

    /// fails if a bark row drifts from `shep barks --format json`.
    #[test]
    fn a_bark_row_serializes_exactly_as_a_bark_does() {
        let bark = Bark {
            at_ms: 1_700_000_000_000,
            rule: "restart-loop".to_string(),
            subject: "api".to_string(),
            message: "api restarted 5 times in 60s".to_string(),
            sinks: vec![
                SinkOutcome {
                    sink: "ops-slack".to_string(),
                    error: None,
                },
                SinkOutcome {
                    sink: "pager".to_string(),
                    error: Some("502 from the webhook".to_string()),
                },
            ],
        };
        assert_eq!(
            serde_json::to_value(BarkRow::from(&bark)).unwrap(),
            serde_json::to_value(&bark).unwrap()
        );
    }
}
