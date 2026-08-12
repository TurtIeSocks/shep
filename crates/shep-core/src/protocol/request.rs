//! RPC frames: requests, responses, envelopes, and structured errors

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::status::ProcStatus;

/// Client's opening frame
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Client crate version (semver string)
    pub client_version: String,
    /// [`crate::protocol::PROTOCOL_VERSION`] the client speaks
    pub protocol: u32,
}

/// Daemon's handshake answer
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    /// Daemon crate version
    pub daemon_version: String,
    /// Protocol version the daemon speaks
    pub protocol: u32,
    /// Daemon pid
    pub pid: u32,
}

/// Serializable selector (mirror of [`crate::selector::ProcessSelector`];
/// regex travels as its source string)
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SelectorSpec {
    /// Every sheep
    All,
    /// By id
    Id(u32),
    /// By exact name
    Name(String),
    /// By regex source
    Regex(String),
    /// By fold name
    Fold(String),
}

/// One RPC request (Phase 1 verb set; later phases extend)
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Request {
    /// Liveness check
    Ping,
    /// Full flock listing
    ListFlock,
    /// Detailed info for matching sheep
    Describe {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Register + start apps
    Start {
        /// App configs — the daemon MUST re-normalize (peer input is
        /// untrusted); failures return [`RpcErrorCode::InvalidConfig`]
        apps: Vec<AppConfig>,
    },
    /// Stop matching sheep (stay registered)
    Stop {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Restart matching sheep
    Restart {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Replace each matching sheep with a fresh instance of the same app, one
    /// instance of an app at a time, so the app has a window in which it can
    /// stay reachable across the swap
    Reload {
        /// Which sheep. No default anywhere in the stack — a reload replaces
        /// running processes, so the operator names the target, exactly as
        /// `stop`/`restart`/`delete` do (see `shep reload`).
        selector: SelectorSpec,
    },
    /// Stop + deregister matching sheep
    Delete {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Reopen every matched sheep's log files, for an external rotator that
    /// has renamed them (`create`-mode rotation)
    Reopen {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Empty every matched sheep's log files: flush what is still pending,
    /// then truncate the recorded paths
    Flush {
        /// Which sheep. No default anywhere in the stack — this destroys
        /// log data, so the operator names the target (see `shep flush`).
        selector: SelectorSpec,
    },
    /// Send a named action to every matched sheep over its shepherd channel
    /// and report what each app says back (see `shep trigger`).
    Trigger {
        /// Which sheep. No default anywhere in the stack, matching
        /// `stop`/`restart`/`reload`/`delete`/`flush`: an operator names the
        /// target rather than trigger an action against the whole flock by
        /// accident.
        selector: SelectorSpec,
        /// The action name. Free-form — the daemon never declares, parses,
        /// or validates it; an app that does not recognize the name is
        /// expected to say so in its own reply rather than stay silent.
        action: String,
        /// Argument text for the action, passed through to the app
        /// verbatim. One opaque string, not structured data: the daemon
        /// holds no schema for it, matching the shepherd channel's own
        /// `action` message this ultimately becomes.
        params: Option<String>,
    },
    /// Write the muster roll now, bypassing the snapshot writer's debounce
    SaveRoll,
    /// Assemble the flock from the muster roll on disk: start every app the
    /// roll recorded running, leaving every app the flock already has exactly
    /// as it stands
    Muster,
    /// Graceful daemon shutdown
    KillDaemon,
    /// Subscribe this connection to bus topics (glob patterns)
    Subscribe {
        /// Topic globs, e.g. `process.*`
        topics: Vec<String>,
    },
}

/// Snapshot of one sheep for listings and events
// wire format: changing this is a breaking change
//
// `out_file`/`err_file` are `Option<String>`, and both halves of that are
// deliberate:
//
// String, not PathBuf. Every path already on this wire travels as a string
// (`AppConfig::script`, `cwd`, `out_file`, `err_file`, all of which ride in
// `Request::Start`), so this matches the established representation. It is
// also the safer failure mode: serde's `PathBuf` impl REFUSES a non-UTF-8
// path, and that refusal is not local — it aborts the whole `Reply`, so one
// sheep with an odd log path would blank the entire `ListFlock` for every
// other sheep. Lossy conversion daemon-side degrades exactly one field of
// one sheep instead.
//
// Option, not a bare String. Semantically the daemon always resolves both
// paths, so a required field is tempting. But the handshake only compares
// `PROTOCOL_VERSION` (see shep-daemon's `server.rs`), and adding these
// fields deliberately does NOT bump it — the evolution rule in this
// module's parent says additive fields keep the version. A daemon built
// before this field and a client built after it therefore both announce
// protocol 1 and connect happily, and that daemon's replies carry no
// `out_file` key at all. A required `String` would fail to deserialize
// there, so a new client could not list against an old daemon. `None`
// means precisely "this peer predates the field" — which readers must
// render as unknown, NOT as "this sheep has no log file".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Stable numeric id
    pub id: u32,
    /// Sheep name
    pub name: String,
    /// Lifecycle status
    pub status: ProcStatus,
    /// OS pid while running
    pub pid: Option<u32>,
    /// Restart count since registration
    pub restarts: u32,
    /// Milliseconds since last successful start
    pub uptime_ms: u64,
    /// Fold membership
    pub fold: Option<String>,
    /// Resolved stdout log path: the app's explicit
    /// [`AppConfig::out_file`] when it set one, else the daemon-derived
    /// default. `None` only when the peer daemon predates this field.
    pub out_file: Option<String>,
    /// Resolved stderr log path, resolved exactly as [`Self::out_file`]
    pub err_file: Option<String>,
}

/// What happened when the daemon tried to deliver one sheep's triggered
/// action.
///
/// `#[non_exhaustive]`: a future outcome — distinguishing a malformed reply
/// from a well-formed one, say, or a second trigger already in flight for
/// the same sheep — must not need a protocol version bump (IR-20).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActionOutcome {
    /// The app answered on the shepherd channel.
    Replied {
        /// The reply body, exactly as the app sent it.
        body: String,
    },
    /// The sheep had no reachable shepherd channel for the daemon to
    /// deliver the action over.
    NoChannel,
    /// The sheep is a reload drainee — mid-swap, on its way out — and the
    /// daemon skipped it rather than deliver the action to a process
    /// already being replaced.
    Skipped,
    /// The daemon delivered the action, but no reply arrived before the
    /// app's configured action timeout elapsed.
    TimedOut,
}

/// One matched sheep's row in a `Trigger` reply.
///
/// `EmptiedFile` (`crates/shep-cli/src/output/rows.rs`) is the precedent for
/// a non-`ProcessInfo` row: a reply body has nowhere to live on
/// [`ProcessInfo`], and [`Self::outcome`] is per-row rather than a
/// whole-request refusal because spec §9's selector grammar (`all`,
/// `/regex/`, `fold:`) makes a mixed flock the normal case — the same reason
/// `Reopen`/`Flush` report per-item failure inside a success rather than
/// failing the whole request.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReply {
    /// The sheep's stable id.
    pub id: u32,
    /// The sheep's name.
    pub name: String,
    /// What happened when the daemon tried to deliver the action.
    pub outcome: ActionOutcome,
}

/// One RPC response (pairs with [`Request`] variants)
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Response {
    /// Answer to `Ping`
    Pong,
    /// Answer to `ListFlock`
    Flock(Vec<ProcessInfo>),
    /// Answer to `Describe`
    Described(Vec<ProcessInfo>),
    /// Answer to `Start`
    Started(Vec<ProcessInfo>),
    /// Answer to `Stop`
    Stopped(Vec<ProcessInfo>),
    /// Answer to `Restart`
    Restarted(Vec<ProcessInfo>),
    /// Answer to `Reload` — an ACCEPTANCE, not a result, and the only reply
    /// in this enum carrying a flock listing that names one rather than
    /// finished work. [`Self::ShuttingDown`] is an acceptance too, sent
    /// before the daemon actually goes down, but it carries nothing.
    ///
    /// One instance costs a readiness wait plus a drain in the worst case, so
    /// a clustered app outlasts any deadline a client is allowed to ask for.
    /// The daemon therefore answers as soon as the reload is accepted, with
    /// the matched sheep as they stood at that moment, and the swaps report
    /// themselves on the bus — `process.reload`, `process.reloaded`,
    /// `process.reload_abandoned`. A matched sheep with nothing to replace is
    /// listed here as the no-op success it is, so this carries the same
    /// matches `Describe` would.
    Reloading(Vec<ProcessInfo>),
    /// Answer to `Delete` — ids removed
    Deleted(Vec<u32>),
    /// Answer to `Reopen` — every matched sheep, running or not. A sheep with
    /// no live log pump has nothing to reopen and is reported as a success,
    /// so this carries the same matches `Describe` would.
    Reopened(Vec<ProcessInfo>),
    /// Answer to `Flush` — one row per matched sheep, running or not, exactly
    /// as [`Self::Reopened`].
    ///
    /// One row per SHEEP, not per file emptied. Several sheep can share one
    /// log path (`merge_logs`, or an explicit `out_file` on a multi-instance
    /// app), and the daemon truncates each distinct path once — but the
    /// selector names sheep, so the answer names sheep, and the count here
    /// matches what `Describe` would return for the same selector.
    Flushed(Vec<ProcessInfo>),
    /// Answer to `Trigger` — one [`ActionReply`] row per matched sheep,
    /// carrying what each one answered rather than a flock listing:
    /// `ProcessInfo` has nowhere to hold a reply body.
    Triggered(Vec<ActionReply>),
    /// Answer to `SaveRoll`
    RollSaved {
        /// Absolute path of the roll the daemon wrote
        path: String,
        /// How many apps that roll records
        apps: u32,
    },
    /// Answer to `Muster` — every sheep of every app the roll restored, not
    /// only the ones this call spawned.
    ///
    /// The distinction is the whole point of the reply. Assembling a flock
    /// that is already assembled starts nothing, so a listing of what this
    /// call spawned would be empty there — indistinguishable from an empty
    /// roll, which is the one outcome an operator needs to tell apart.
    Mustered(Vec<ProcessInfo>),
    /// Answer to `Subscribe`
    Subscribed,
    /// Answer to `KillDaemon`
    ShuttingDown,
}

/// A request frame
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Per-connection request id
    pub id: u64,
    /// Client-imposed deadline (daemon aborts work past it)
    pub deadline_ms: Option<u64>,
    /// The request
    pub body: Request,
}

/// A reply frame
///
/// `result` uses serde's stock `Result` representation — the wire carries
/// `{"Ok": ...}` / `{"Err": ...}` (capitalized keys). Deliberate, pinned by
/// snapshot: stock serde beats a custom enum the client would convert anyway.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// Echoes [`Envelope::id`]
    pub id: u64,
    /// The outcome
    pub result: Result<Response, RpcError>,
}

/// Handshake outcome: `HelloAck` or a typed refusal (spec §6 —
/// version skew is an error, not silence). Same `Ok`/`Err` wire shape
/// as [`Reply::result`]; refusals use [`RpcErrorCode::ProtocolMismatch`].
pub type HelloReply = Result<HelloAck, RpcError>;

/// Structured RPC failure
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// Machine-readable code
    pub code: RpcErrorCode,
    /// Human-readable message (plain English, no theme)
    pub message: String,
}

/// Machine-readable RPC error codes
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RpcErrorCode {
    /// Selector matched nothing
    NotFound,
    /// Config failed validation daemon-side
    InvalidConfig,
    /// Spawn failed (exec error, permissions)
    SpawnFailed,
    /// Handshake protocol version mismatch
    ProtocolMismatch,
    /// Unexpected daemon-side failure
    Internal,
    /// The request's deadline expired before the daemon finished it
    DeadlineExceeded,
}

impl RpcErrorCode {
    /// Every variant, for code that needs to iterate them all.
    ///
    /// `#[non_exhaustive]` forces a `_` arm on any match written outside
    /// this crate, which would silently swallow a variant added here and
    /// never updated there (shep-cli's exit-code mapping test is the
    /// motivating case — see `crates/shep-cli/src/exit.rs`). Downstream
    /// crates should iterate `ALL` instead of hand-writing their own list
    /// that the compiler can't check.
    ///
    /// Kept honest by a private `assert_all_lists_every_variant` fn right
    /// below: read that doc for how a forgotten variant is caught here,
    /// where `#[non_exhaustive]` has no effect.
    pub const ALL: [Self; 6] = [
        Self::NotFound,
        Self::InvalidConfig,
        Self::SpawnFailed,
        Self::ProtocolMismatch,
        Self::Internal,
        Self::DeadlineExceeded,
    ];

    /// Never called; exists purely so this crate fails to build if a
    /// variant is added to [`RpcErrorCode`] without also adding it to
    /// [`Self::ALL`].
    ///
    /// `#[non_exhaustive]` only forces a wildcard arm on matches written
    /// *outside* this crate — inside the crate that defines the enum, a
    /// match with no `_` arm is still checked for exhaustiveness (E0004),
    /// so a new variant breaks this build until it gets an arm here. Each
    /// arm indexes a fixed literal position into [`Self::ALL`], so growing
    /// the enum without growing the array is caught too: rustc denies an
    /// out-of-bounds constant array index by default.
    #[allow(dead_code)]
    const fn assert_all_lists_every_variant(code: Self) -> Self {
        match code {
            Self::NotFound => Self::ALL[0],
            Self::InvalidConfig => Self::ALL[1],
            Self::SpawnFailed => Self::ALL[2],
            Self::ProtocolMismatch => Self::ALL[3],
            Self::Internal => Self::ALL[4],
            Self::DeadlineExceeded => Self::ALL[5],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::status::ProcStatus;

    fn sample_info() -> ProcessInfo {
        ProcessInfo {
            id: 3,
            name: "web".to_string(),
            status: ProcStatus::Online,
            pid: Some(4242),
            restarts: 1,
            uptime_ms: 60_000,
            fold: Some("backend".to_string()),
            out_file: Some("/home/rin/.shep/logs/web-0-out.log".to_string()),
            err_file: Some("/home/rin/.shep/logs/web-0-err.log".to_string()),
        }
    }

    #[test]
    fn request_wire_snapshots() {
        let requests = vec![
            Envelope {
                id: 1,
                deadline_ms: Some(5000),
                body: Request::Ping,
            },
            Envelope {
                id: 2,
                deadline_ms: None,
                body: Request::ListFlock,
            },
            Envelope {
                id: 3,
                deadline_ms: None,
                body: Request::Stop {
                    selector: SelectorSpec::Name("web".to_string()),
                },
            },
            Envelope {
                id: 4,
                deadline_ms: None,
                body: Request::Start {
                    apps: vec![AppConfig::minimal("web", "./srv")],
                },
            },
            // `All` rather than a named sheep: it is the selector `shep
            // reopen` sends when given no argument, and the one a signal can
            // ever mean, so it is the row worth pinning.
            Envelope {
                id: 5,
                deadline_ms: None,
                body: Request::Reopen {
                    selector: SelectorSpec::All,
                },
            },
            // Deliberately the same selector as the row above, so the two
            // log-plane rows differ by their `kind` and by nothing else: a
            // `Flush` that serialized under `reopen`'s tag — the shape a
            // copy-pasted variant takes — shows up here as two identical
            // objects rather than as a diff a reader has to compare field by
            // field. `shep flush` demands an explicit selector, so `all` is
            // not a default here the way it is for `reopen`; it is simply the
            // widest thing an operator can type.
            Envelope {
                id: 6,
                deadline_ms: None,
                body: Request::Flush {
                    selector: SelectorSpec::All,
                },
            },
            // The same selector as the `stop` row above, for the reason the
            // pair above share theirs: `reload` is the third verb that
            // demands an explicit selector and replaces what it matches, so
            // the variant it would be copy-pasted from is `stop`. Serialized
            // under `stop`'s tag it shows up here as two identical objects
            // rather than as a diff a reader has to compare field by field.
            Envelope {
                id: 7,
                deadline_ms: None,
                body: Request::Reload {
                    selector: SelectorSpec::Name("web".to_string()),
                },
            },
            // `action`/`params` here match the spec's own §9 example
            // (`trigger web set-log-level debug`) and channel.rs's
            // with-params fixture verbatim, so a reader tracing a trigger
            // from the CLI through the client↔daemon wire to the fd-3 wire
            // sees the same two strings at every hop rather than three
            // unrelated examples.
            Envelope {
                id: 8,
                deadline_ms: None,
                body: Request::Trigger {
                    selector: SelectorSpec::Name("web".to_string()),
                    action: "set-log-level".to_string(),
                    params: Some("debug".to_string()),
                },
            },
            // The first fieldless verb added since `Ping`/`ListFlock`, and
            // pinned for that reason: a fieldless variant serializes as a
            // bare `{"kind":"..."}` with no `selector` key at all, so a
            // reader comparing this row against `stop`'s sees the whole
            // difference between the two shapes in one place.
            Envelope {
                id: 9,
                deadline_ms: None,
                body: Request::SaveRoll,
            },
            // Paired with the `save_roll` row above so the two halves of the
            // roll — the direction that writes it and the direction that
            // assembles from it — sit next to each other, differing by their
            // `kind` and by nothing else.
            Envelope {
                id: 10,
                deadline_ms: None,
                body: Request::Muster,
            },
        ];
        insta::assert_json_snapshot!("request_wire_v1", requests);
    }

    #[test]
    fn reply_wire_snapshots() {
        let replies = vec![
            Reply {
                id: 1,
                result: Ok(Response::Pong),
            },
            Reply {
                id: 2,
                result: Ok(Response::Flock(vec![sample_info()])),
            },
            Reply {
                id: 3,
                result: Err(RpcError {
                    code: RpcErrorCode::NotFound,
                    message: "no sheep matches `web`".to_string(),
                }),
            },
            // Unlike `Reopened`/`Flushed`/`Reloading` above (all wire-identical
            // to `Flock`, just under a different `kind` tag, so pinning `Flock`
            // once already covers their shape), `Triggered` carries a genuinely
            // different row — `ActionReply` is not a `ProcessInfo` — so it earns
            // its own entry. `Replied` is the struct-shaped variant of
            // `ActionOutcome`, and so the one worth pinning here: the three
            // unit variants serialize as bare `{"kind":"..."}`, a shape already
            // proven by every fieldless variant elsewhere on this wire.
            Reply {
                id: 4,
                result: Ok(Response::Triggered(vec![ActionReply {
                    id: 3,
                    name: "web".to_string(),
                    outcome: ActionOutcome::Replied {
                        body: "ok".to_string(),
                    },
                }])),
            },
            // The only struct-shaped `Response` variant, so the one worth
            // pinning here: every other variant on this wire is a newtype
            // over a Vec or a unit, both shapes already proven above.
            Reply {
                id: 5,
                result: Ok(Response::RollSaved {
                    path: "/home/rin/.shep/flock.json".to_string(),
                    apps: 2,
                }),
            },
        ];
        insta::assert_json_snapshot!("reply_wire_v1", replies);
    }

    #[test]
    fn v1_fixture_still_deserializes() {
        // Committed byte fixture from protocol v1 — if this breaks, bump
        // PROTOCOL_VERSION and record it in the CHANGELOG (IR-35).
        let fixture = r#"{"id":7,"deadline_ms":null,"body":{"kind":"stop","selector":{"kind":"name","value":"web"}}}"#;
        let env: Envelope = serde_json::from_str(fixture).unwrap();
        assert_eq!(env.id, 7);
        assert!(matches!(
            env.body,
            Request::Stop { selector: SelectorSpec::Name(ref n) } if n == "web"
        ));
    }

    #[test]
    fn hello_handshake_shape() {
        let hello = Hello {
            client_version: "0.1.0".to_string(),
            protocol: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"client_version":"0.1.0","protocol":1}"#);
    }

    #[test]
    fn hello_reply_carries_typed_skew_error() {
        let refusal: HelloReply = Err(RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 1, client sent 2".to_string(),
        });
        let json = serde_json::to_string(&refusal).unwrap();
        assert_eq!(
            json,
            r#"{"Err":{"code":"protocol_mismatch","message":"daemon speaks protocol 1, client sent 2"}}"#
        );
        let back: HelloReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, refusal);
    }

    #[test]
    fn v1_reply_fixture_still_deserializes() {
        // Committed byte fixture, protocol v1 (IR-35).
        let ok = r#"{"id":1,"result":{"Ok":{"kind":"pong"}}}"#;
        let reply: Reply = serde_json::from_str(ok).unwrap();
        assert!(matches!(reply.result, Ok(Response::Pong)));
        let err = r#"{"id":2,"result":{"Err":{"code":"not_found","message":"no sheep"}}}"#;
        let reply: Reply = serde_json::from_str(err).unwrap();
        assert_eq!(reply.result.unwrap_err().code, RpcErrorCode::NotFound);
    }

    #[test]
    fn v1_hello_ack_fixture_still_deserializes() {
        let fixture = r#"{"Ok":{"daemon_version":"0.1.0","protocol":1,"pid":4242}}"#;
        let ack: HelloReply = serde_json::from_str(fixture).unwrap();
        assert_eq!(ack.unwrap().pid, 4242);
    }

    #[test]
    fn v1_process_info_without_log_paths_still_deserializes() {
        // Committed byte fixture from before `out_file`/`err_file` existed
        // (IR-35). The handshake only compares PROTOCOL_VERSION, which this
        // addition deliberately did not bump, so a daemon built at this
        // vintage still connects to a current client and sends exactly these
        // bytes. Absent keys must land as `None`, not as a decode error.
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend"}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.id, 3);
        assert_eq!(info.out_file, None);
        assert_eq!(info.err_file, None);
    }

    #[test]
    fn an_old_client_still_decodes_a_new_process_info() {
        // The other skew direction: a client built before the fields reads a
        // current daemon's reply. `ProcessInfo` carries no
        // `deny_unknown_fields` (unlike the config types in
        // `crate::config`), so the two extra keys are ignored rather than
        // refused — which is what makes this addition version-preserving.
        #[derive(Deserialize)]
        struct V1ProcessInfo {
            id: u32,
            fold: Option<String>,
        }

        let current = serde_json::to_string(&sample_info()).unwrap();
        let old: V1ProcessInfo = serde_json::from_str(&current).unwrap();
        assert_eq!(old.id, 3);
        assert_eq!(old.fold.as_deref(), Some("backend"));
    }

    #[test]
    fn deadline_exceeded_code_serializes_snake_case() {
        // Additive variant (evolution rule): the existing codes keep their
        // strings, so v1 byte fixtures above still deserialize unchanged.
        assert_eq!(
            serde_json::to_string(&RpcErrorCode::DeadlineExceeded).unwrap(),
            "\"deadline_exceeded\""
        );
        assert_eq!(
            serde_json::from_str::<RpcErrorCode>("\"deadline_exceeded\"").unwrap(),
            RpcErrorCode::DeadlineExceeded
        );
    }

    #[test]
    fn action_outcome_kinds_serialize_snake_case_and_round_trip() {
        // The shared snapshots above exercise exactly one `ActionOutcome`
        // variant (`Replied`, the only struct-shaped one, in
        // `reply_wire_snapshots`) — nothing else there would catch a rename
        // of `no_channel`, `skipped`, or `timed_out`. Pinned here instead,
        // the same way `deadline_exceeded_code_serializes_snake_case` pins a
        // lone `RpcErrorCode` variant above.
        let cases = [
            (
                ActionOutcome::Replied {
                    body: "pong".to_string(),
                },
                r#"{"kind":"replied","body":"pong"}"#,
            ),
            (ActionOutcome::NoChannel, r#"{"kind":"no_channel"}"#),
            (ActionOutcome::Skipped, r#"{"kind":"skipped"}"#),
            (ActionOutcome::TimedOut, r#"{"kind":"timed_out"}"#),
        ];
        for (outcome, wire) in cases {
            assert_eq!(
                serde_json::to_string(&outcome).unwrap(),
                wire,
                "{outcome:?}"
            );
            assert_eq!(
                serde_json::from_str::<ActionOutcome>(wire).unwrap(),
                outcome
            );
        }
    }

    /// fails if `SaveRoll` or `RollSaved` is given a `rename`, or if
    /// `Response`'s `content = "data"` is dropped — either changes these two
    /// strings while every type-level test in this module keeps passing.
    #[test]
    fn save_roll_serializes_snake_case_with_its_payload_under_data() {
        assert_eq!(
            serde_json::to_string(&Request::SaveRoll).unwrap(),
            r#"{"kind":"save_roll"}"#
        );
        let reply = Response::RollSaved {
            path: "/tmp/flock.json".to_string(),
            apps: 3,
        };
        let wire = r#"{"kind":"roll_saved","data":{"path":"/tmp/flock.json","apps":3}}"#;
        assert_eq!(serde_json::to_string(&reply).unwrap(), wire);
        assert_eq!(serde_json::from_str::<Response>(wire).unwrap(), reply);
    }

    /// fails if `Muster` or `Mustered` is given a `rename`, or if `Mustered`
    /// is declared fieldless — any of the three changes one of these two
    /// strings while every type-level test in this module keeps passing.
    ///
    /// The listing is empty on purpose. `Mustered` carries the same
    /// `Vec<ProcessInfo>` `Flock` does, and `reply_wire_snapshots` already
    /// pins that row field by field; what is unpinned until here is this
    /// variant's own tag and whether its payload lands under `data` at all.
    #[test]
    fn muster_serializes_snake_case_with_its_listing_under_data() {
        assert_eq!(
            serde_json::to_string(&Request::Muster).unwrap(),
            r#"{"kind":"muster"}"#
        );
        let reply = Response::Mustered(Vec::new());
        let wire = r#"{"kind":"mustered","data":[]}"#;
        assert_eq!(serde_json::to_string(&reply).unwrap(), wire);
        assert_eq!(serde_json::from_str::<Response>(wire).unwrap(), reply);
    }
}
