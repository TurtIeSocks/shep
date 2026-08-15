//! The shapes whistle's tools return.
//!
//! Structural twins of `shep_core`'s own types, field for field and value for
//! value, with `schemars::JsonSchema` derived on top so rmcp can declare each
//! tool's output schema. [`SheepRow`] and `ProcessInfo` serialize to
//! byte-identical JSON, pinned by this module's own equality tests.
//!
//! **Why twins and not a `schemars` derive on `ProcessInfo` itself.** That
//! would put a schema-generation dependency into shep-core — a wire-protocol
//! crate — for a CLI concern, and shep-core's types are the wire contract for
//! the daemon socket, not for MCP. A twin plus an equality test is the cheaper
//! half of that trade, and the test is what stops the two drifting.
//!
//! **Why the vocabulary is reused when the envelope is not.** MCP carries its
//! own envelope: `CallToolResult`, with `structuredContent` and a per-tool
//! output schema. Nesting `output::OutputEnvelope` inside it would make the
//! declared schema describe `schema_version` and `command`, two fields that
//! mean everything to a shell script and nothing to an agent — and would
//! couple `SCHEMA_VERSION`, which is a promise to people running `jq` over
//! `shep flock --format json`, to whistle's contract. Different consumers,
//! different envelopes, one vocabulary.

use schemars::JsonSchema;
use serde::Serialize;
use shep_core::barks::{Bark, SinkOutcome};
use shep_core::protocol::{DogSource, Lamb, ProcessInfo};

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
///
/// Not constructed here yet: `read.rs` (Task 6) is the first caller.
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
pub struct FlockListing {
    /// The matched sheep and dogs, in the order the shepherd reported them.
    pub flock: Vec<SheepRow>,
}

/// `list_barks`' payload. Same rule, same reason as [`FlockListing`].
///
/// Not constructed here yet: `read.rs` (Task 6) is the first caller.
#[allow(dead_code)]
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
///
/// Not constructed here yet: `read.rs` (Task 6) is the first caller.
#[allow(dead_code)]
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
///
/// Not constructed here yet: `read.rs` (Task 6) is the first caller.
#[allow(dead_code)]
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

/// What `tail_bleats` returns.
///
/// Not constructed here yet: `read.rs` (Task 6) is the first caller.
#[allow(dead_code)]
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
    /// True when the tail was cut short by the line cap rather than by the
    /// end of the file. A model that cannot tell "this is all of it" from
    /// "this is the last 50" will draw the wrong conclusion from a quiet
    /// log.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::{DogSource, Lamb, ProcessInfo};
    use shep_core::status::ProcStatus;

    /// fails the moment whistle's view of a sheep drifts from the CLI's.
    ///
    /// This is DEEP equality of the serialized values, not a key-set check:
    /// a field that keeps its name and changes its shape (`status` becoming
    /// a struct, `dog` losing its tag) fails here too. `shep describe
    /// --format json` and `describe_sheep` describe the same sheep in the
    /// same words, or this reddens and somebody decides which one is right.
    ///
    /// It also catches the additive case, which is the likely one: a
    /// fourteenth field on `ProcessInfo` makes this fail with a missing key
    /// until `SheepRow` carries it or a comment here says why it does not.
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
            .build();

        assert_eq!(
            serde_json::to_value(SheepRow::from(&info)).unwrap(),
            serde_json::to_value(&info).unwrap(),
            "whistle and `--format json` must describe a sheep identically"
        );
    }

    /// fails if the every-field-populated case above is the only one that
    /// holds. A stopped sheep has `None` in six places, and a twin that
    /// rendered `null` where `ProcessInfo` renders `null` for a different
    /// reason would pass the case above and fail here.
    #[test]
    fn an_empty_sheep_row_serializes_exactly_as_process_info_does_too() {
        let info = ProcessInfo::builder(1, "idle", ProcStatus::Stopped).build();
        assert_eq!(
            serde_json::to_value(SheepRow::from(&info)).unwrap(),
            serde_json::to_value(&info).unwrap()
        );
    }

    /// fails if the schema stops describing what the struct emits. rmcp
    /// hands this schema to the model as the tool's declared output shape;
    /// a schema missing a field the tool returns teaches the model wrong.
    #[test]
    fn the_generated_schema_names_every_field_the_row_carries() {
        let schema = serde_json::to_value(schemars::schema_for!(SheepRow)).unwrap();
        let properties = schema["properties"].as_object().expect("an object schema");
        let info = ProcessInfo::builder(1, "idle", ProcStatus::Stopped).build();
        let emitted = serde_json::to_value(&info).unwrap();
        for key in emitted.as_object().unwrap().keys() {
            assert!(
                properties.contains_key(key),
                "the schema is missing `{key}`, which the tool returns"
            );
        }
    }

    /// fails if a tool's declared shape stops being one MCP will accept.
    ///
    /// Two halves, and they are different rules in rmcp 3.1.2:
    ///
    /// - **Output.** `structuredContent` is an OBJECT on the wire (rmcp's
    ///   own field doc, model.rs:3802-3803), and `Json<T>` puts `T` there
    ///   verbatim via `CallToolResult::structured` (model.rs:3963-3971).
    ///   rmcp will not stop a `Vec`: 3.1.2's `schema_for_output`
    ///   deliberately does not validate the root type (common.rs:109-120,
    ///   per SEP-2106), so the failure would be a wire-shape violation a
    ///   strict client rejects and a lenient one silently takes — the worst
    ///   kind. Hence the wrappers, and hence this test rather than a
    ///   comment.
    /// - **Input.** `schema_for_input` DOES validate (common.rs:77-96) and
    ///   the `#[tool]` macro `panic!`s on the `Err` during router
    ///   construction (rmcp-macros/tool.rs:200-208) — i.e. inside
    ///   `Whistle::new`, on every startup and in the first line of every
    ///   test in Tasks 6-10. Every argument type here is a plain struct so
    ///   this holds by construction, which is exactly what was said about
    ///   the output side before it turned out to be wrong.
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
