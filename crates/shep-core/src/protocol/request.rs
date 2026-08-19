//! RPC frames: requests, responses, envelopes, and structured errors

use core::fmt;

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
    /// Set how many instances one app runs (see `shep stock`).
    ///
    /// # Why a name and not a selector
    ///
    /// Every other verb here takes a [`SelectorSpec`], and this one
    /// deliberately does not. `instances` is a per-app number and instance
    /// slots are allocated against the same-name group
    /// (`shep_daemon::assemble::instance_slots`), so a selector matching two
    /// apps would have to mean either "four of each" or "four in total", and
    /// neither reading is more obviously right than the other. A name has one
    /// meaning.
    ///
    /// # Why absolute and not a delta
    ///
    /// There is no `+N`/`-N` form and there will not be one. An absolute count
    /// is idempotent — run it twice, get the same flock — where two operators
    /// sending `+2` against the same app get a number neither of them asked
    /// for. This project's own trace notes also record a crash on pm2's
    /// relative-remove path, and those notes exist so shep does not reproduce
    /// what they record.
    Scale {
        /// The app's name, exactly as its config spells it. Not a selector: no
        /// `all`, no regex, no `fold:`.
        name: String,
        /// How many instances the app has when this returns. `0` is refused
        /// with [`RpcErrorCode::InvalidConfig`] — `normalize` rejects
        /// `instances == 0` for every other path into the daemon, and `shep
        /// delete` is the verb for removing an app.
        count: u32,
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
    /// Deliver one signal to every matched sheep's OWN process — never its
    /// process group (see `shep signal`).
    Signal {
        /// Which sheep. No default anywhere in the stack, matching every
        /// other verb that reaches a running process: an operator names the
        /// target rather than signal the whole flock by accident.
        selector: SelectorSpec,
        /// The signal's name, as
        /// [`OperatorSignal`](crate::signals::OperatorSignal) spells it — the
        /// `SIG` prefix and the case are both optional.
        ///
        /// A `String` rather than the enum, for the reason
        /// [`AppConfig::kill_signal`](crate::config::AppConfig::kill_signal)
        /// is one: the wire stays plain text a person can read in a capture,
        /// and the daemon re-validates regardless, because peer input is
        /// untrusted. A name outside the grammar answers
        /// [`RpcErrorCode::InvalidConfig`].
        signal: String,
    },
    /// Write one line to every matched sheep's stdin (see `shep whisper`).
    SendLine {
        /// Which sheep. No default, matching every other verb that reaches a
        /// running process.
        selector: SelectorSpec,
        /// The line, WITHOUT its terminator — the shepherd appends exactly one
        /// `\n` when it writes. Carrying the terminator here would leave "did
        /// the caller include one" as a question every hop has to re-answer,
        /// and a caller that included two would send an empty line the app
        /// never asked for.
        ///
        /// A line containing an embedded newline is refused
        /// ([`RpcErrorCode::InvalidConfig`]): it would deliver two commands
        /// where the operator typed one.
        line: String,
    },
    /// Write the muster roll now, bypassing the snapshot writer's debounce
    SaveRoll,
    /// Assemble the flock from the muster roll on disk: start every app the
    /// roll recorded running, leaving every app the flock already has exactly
    /// as it stands
    Muster,
    /// Ask for one dog's `[dog.<name>]` section, as the dog itself parses it
    DogConfig {
        /// The dog's name — the config key, not a selector
        name: String,
    },
    /// Start one dog now, marking it as coming from `source`
    EnableDog {
        /// The dog's name
        name: String,
        /// Where its binary comes from
        source: DogSource,
    },
    /// Stop and deregister one dog
    ///
    /// Answers [`Response::Deleted`], the same reply `Delete` gives: disabling
    /// deregisters exactly as `Delete` does, so this is the same fact and not
    /// a coincidence of shape. A variant of its own (`DogDisabled`, say) would
    /// carry nothing `Deleted` does not.
    DisableDog {
        /// The dog's name
        name: String,
    },
    /// Graceful daemon shutdown
    KillDaemon,
    /// Subscribe this connection to bus topics (glob patterns)
    Subscribe {
        /// Topic globs, e.g. `process.*`
        topics: Vec<String>,
    },
}

/// Where a dog came from: this binary, or one an operator adopted.
///
/// The one thing an operator wants when a dog misbehaves, which is why it
/// is a column rather than a detail. Carried on [`ProcessInfo::dog`], so a
/// listing distinguishes the two populations without a second request.
///
/// `#[non_exhaustive]`: a future source — a dog fetched from a registry,
/// say — must not need a protocol version bump (IR-20).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DogSource {
    /// An argv branch of the shep binary itself (`shep dog <name>`).
    BuiltIn,
    /// A binary an operator adopted, run at the daemon's own trust level.
    Adopted {
        /// The binary's path, exactly as the operator gave it to `adopt`.
        path: String,
    },
}

/// One process the OS reports as a descendant of a sheep.
///
/// # What this is not
///
/// It is **not** the set of processes that die with the sheep, and nothing here
/// should be read as promising that. The list is built by walking the OS's
/// parent-pid links; the stop ladder acts on the process GROUP, and the two
/// units diverge in both directions — a lamb that forks and exits leaves its
/// own children re-parented to init, out of this list and still in the group,
/// while a `setsid()` grandchild stays in this list and leaves the group.
/// shep-daemon's `limits` module doc has the full account, and it is the
/// authority; this is a pointer to it, not a second copy free to drift.
///
/// # Why a name and not a command line
///
/// `name` is the executable's name as the OS reports it (`node`, `sh`,
/// `python3`), never its argument vector. A process's argv routinely carries
/// credentials — a `--password=` flag, a URL with a token in the query string —
/// and this field rides in `shep describe --format json`, which is output
/// people paste into bug reports. A pid alone would be safe too, and was
/// considered; it was rejected because a tree of bare integers sends the
/// operator to `ps`, which is the work the tree exists to save.
///
/// # Why no memory figure
///
/// The sheep's own row already reports its whole tree's resident size
/// ([`ProcessInfo::memory_bytes`]), and a per-lamb breakdown is a profiler's
/// job. `deferred.md`'s note on this struct's growth asks for exactly this
/// restraint.
///
/// `#[non_exhaustive]`: shep-core is a published library, this type is new, and
/// the two obvious next fields (a parent pid, so a deep tree can be nested
/// rather than flattened; a start time) would otherwise be breaking additions
/// (IR-20). Build one with [`Self::new`].
// wire format: changing this is a breaking change
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lamb {
    /// The lamb's own pid.
    pub pid: u32,
    /// The executable's name, as the OS reports it. Never its command line.
    pub name: String,
}

impl Lamb {
    /// One lamb.
    ///
    /// A plain constructor rather than a builder, unlike [`ProcessInfo`]: both
    /// fields are required and neither is optional or derived, which is the
    /// case a builder buys nothing for.
    #[must_use]
    pub fn new(pid: u32, name: impl Into<String>) -> Self {
        Self {
            pid,
            name: name.into(),
        }
    }
}

/// Why a sheep's process most recently stopped existing under this daemon.
///
/// Not shep-daemon's own `ExitOutcome` reused directly: that type lives
/// behind the spawn-runner seam (`ProcessRunner::wait`, in shep-daemon's
/// `runner` module) and is free to grow with whatever the real runner needs
/// to observe next without dragging a breaking wire change behind it — this
/// one only ever grows on its own say-so. The two happen to carry the same
/// two fields today because the runner's own observation IS the honest exit
/// outcome; shep-daemon converts one into the other at the point it records
/// it (`Actor::handle_exited`), rather than this crate depending on
/// shep-daemon's internals to reuse its type.
///
/// A struct, not two flat `Option<i32>` fields on [`ProcessInfo`] directly:
/// with two flat fields, "this sheep has never exited under this daemon"
/// and "it exited, killed by a signal this daemon did not name" are both
/// all-`None`, and a reader cannot tell those apart. Nested behind
/// [`ProcessInfo::last_exit`]'s own `Option`, that ambiguity moves up a
/// level where it belongs: `None` there means "never exited"; `Some` means
/// "exited, and here is what this daemon knows about it" — which itself
/// mirrors the OS's own exited-normally/killed-by-signal split
/// (`WIFEXITED`/`WIFSIGNALED`): ordinarily exactly one of `code`/`signal` is
/// `Some`. A reader must not assume both can never be `None` together,
/// though — that would still mean "this daemon recorded an exit; it could
/// not characterize how" rather than "this sheep never exited", and this
/// type does not forbid it.
///
/// No `#[non_exhaustive]`, unlike every other struct on this wire: those
/// grow because a discriminator does (`ProcessInfo`'s own doc lists its
/// four so far); `code`/`signal` is already the complete
/// exited-normally/killed-by-signal split the runner exposes, this crate
/// has no libc to derive a richer wait-status decomposition from even if it
/// wanted one, and there is no forecast next field. Adding the attribute
/// back later is a compatible change the day a real need appears (IR-16
/// style); carrying it now on nothing but "might grow" is the exact
/// speculative case IR-20 warns against.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    /// The process's own exit code, set on a normal exit (`WIFEXITED`).
    pub code: Option<i32>,
    /// The raw unix signal number that ended the process, set when it did
    /// not exit on its own (`WIFSIGNALED`) — an operator's own `shep stop`
    /// or `shep delete` included: the process still genuinely stopped by a
    /// signal, and that stays true information even though shep, not a
    /// crash, is what asked for it. Raw and platform-specific for the same
    /// reason [`crate::signals::OperatorSignal`] carries no such accessor —
    /// see that type's own module doc — so rendering this as a name
    /// (`SIGKILL` rather than `9`) is a job for whichever OS-aware layer
    /// reads it, never for this crate.
    pub signal: Option<i32>,
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
//
// `cpu_percent`/`memory_bytes` are optional for that same skew reason and
// for one of their own: a sheep that is not running has no resource use to
// report, and one that has been up for less than a sampling window has no
// honest CPU figure. All three cases render as unknown, never as zero.
//
// No `Eq`, which every other wire struct in this module derives:
// `cpu_percent` is an `f32` and floats are only partially ordered. Nothing
// compares a `ProcessInfo` for total equality — `assert_eq!` needs only
// `PartialEq`, and no listing is keyed on, hashed by, or sorted by a whole
// row.
/// `#[non_exhaustive]`: this struct has now grown a field in five separate
/// phases (`out_file`/`err_file`, then `cpu_percent`/`memory_bytes`, then
/// `dog`, then `lambs`, then `last_exit`) with no hand-edit sweep across the
/// workspace for any of them — the attribute is paying for itself exactly
/// as advertised. Corrected from an earlier version of this comment, which
/// overstated `deferred.md`'s own `ProcessInfo` entry as a warning against
/// growing this struct at all: what that entry actually defers is
/// SPLITTING it into several smaller types, and calls this attribute plus
/// [`ProcessInfo::builder`] "deliberately the opposite of forcing the split
/// early" — i.e. exactly what makes a field like `last_exit` cheap to add
/// for a concrete operator need, not a reason to withhold one. Use
/// [`ProcessInfo::builder`] to construct one; the fields stay `pub`, so
/// reading them and assigning to them are both unchanged.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Tree CPU as a percentage of one core, over the window since the
    /// daemon's last periodic sample. `None` when the sheep is not running,
    /// when it has been up for less than one sampling window, or when the
    /// peer daemon predates this field — all three of which a reader
    /// renders as unknown, never as zero.
    ///
    /// A value over 100 is a tree using more than one core, not a bug.
    pub cpu_percent: Option<f32>,
    /// Tree resident set size in bytes, current as of the reply. `None`
    /// under the same three conditions as [`Self::cpu_percent`], minus the
    /// window one — memory needs no baseline.
    pub memory_bytes: Option<u64>,
    /// Set when this entry is a dog, naming where the dog came from;
    /// `None` for a sheep.
    ///
    /// Unlike [`Self::cpu_percent`], `None` here does not need to enumerate
    /// three cases. A daemon built before dogs existed has none, so "not a
    /// dog" is the true answer whether this peer predates the field or the
    /// entry is genuinely a sheep — there is no resource-usage-style claim
    /// a stale zero could get wrong. Do not "fix" this into three cases.
    pub dog: Option<DogSource>,
    /// The processes the OS reports as descendants of this sheep, or `None`
    /// when this reply did not walk for them.
    ///
    /// `None` covers two cases and is deliberately not a third: this reply is
    /// not a `Describe` (only `Describe` walks — the walk costs a second pass
    /// over the machine's process table, and a flock listing is the thing an
    /// operator leaves running in a loop), or the peer daemon predates the
    /// field. `Some(vec![])` is the third case, and the one that means what it
    /// looks like: walked, and this sheep has no children.
    ///
    /// Read [`Lamb`]'s own doc before rendering this. The list is a parent-pid
    /// walk and is NOT the set of processes a stop kills; any output built from
    /// it has to say so where the operator will see it.
    pub lambs: Option<Vec<Lamb>>,
    /// How this sheep's process most recently stopped existing under this
    /// daemon. `None` while it has never exited under this daemon — either
    /// it has not been started yet, or it is still on its very first run —
    /// and also when the peer daemon predates this field, the same skew
    /// rule [`Self::out_file`] documents for itself.
    ///
    /// Sticky across a respawn, deliberately: this is the daemon's answer
    /// to "why did it last stop", not "is it stopped right now" — `status`
    /// and `pid` already answer that, and a sheep back `Online` after a
    /// crash still has a true story to tell about the crash that restarted
    /// it. It updates only on the next exit, never cleared by one starting
    /// back up.
    pub last_exit: Option<ExitInfo>,
}

impl ProcessInfo {
    /// Starts a builder for one sheep's row.
    ///
    /// The three required arguments are the three fields no row can omit and
    /// no reader can default: which sheep this is, what it is called, and
    /// what state it is in. Everything else is optional, derived, or
    /// meaningfully absent, which is exactly the shape a builder is for —
    /// a nine-argument `new` would put `Option<String>, Option<String>,
    /// Option<f32>, Option<u64>` next to each other at every call site and
    /// invite a silent transposition the type system could not catch.
    ///
    /// No `#[must_use]` here: [`ProcessInfoBuilder`] already carries one,
    /// which clippy's `double_must_use` lint treats as covering this
    /// function's return too.
    pub fn builder(id: u32, name: impl Into<String>, status: ProcStatus) -> ProcessInfoBuilder {
        ProcessInfoBuilder {
            info: Self {
                id,
                name: name.into(),
                status,
                pid: None,
                restarts: 0,
                uptime_ms: 0,
                fold: None,
                out_file: None,
                err_file: None,
                cpu_percent: None,
                memory_bytes: None,
                dog: None,
                lambs: None,
                last_exit: None,
            },
        }
    }
}

/// Builds a [`ProcessInfo`], which is `#[non_exhaustive]` and so cannot be
/// written as a struct literal outside this crate.
///
/// Every setter takes the field's own type, `Option` included, rather than
/// the unwrapped value. That is deliberate and it is the difference between a
/// straight port and a rewrite: the daemon already holds `Option<u32>` for a
/// pid and `Option<f32>` for a CPU reading, so `.pid(entry.pid())` carries
/// across unchanged where `.pid(u32)` would put an `if let` ladder at every
/// call site. A setter is skipped, not passed `None`, when a row genuinely
/// has nothing to say about that field.
///
/// Defaults for the skipped fields are the ones a not-yet-running sheep has:
/// no pid, no uptime, no restarts, no resource reading, not a dog, never
/// exited.
#[derive(Debug, Clone)]
#[must_use = "a builder that is never `build`-ed produces no ProcessInfo"]
pub struct ProcessInfoBuilder {
    info: ProcessInfo,
}

impl ProcessInfoBuilder {
    /// Sets the OS pid; `None` while the sheep is not running.
    pub fn pid(mut self, pid: Option<u32>) -> Self {
        self.info.pid = pid;
        self
    }

    /// Sets the restart count since registration.
    pub fn restarts(mut self, restarts: u32) -> Self {
        self.info.restarts = restarts;
        self
    }

    /// Sets milliseconds since the last successful start.
    pub fn uptime_ms(mut self, uptime_ms: u64) -> Self {
        self.info.uptime_ms = uptime_ms;
        self
    }

    /// Sets fold membership.
    pub fn fold(mut self, fold: Option<String>) -> Self {
        self.info.fold = fold;
        self
    }

    /// Sets the resolved stdout log path.
    pub fn out_file(mut self, out_file: Option<String>) -> Self {
        self.info.out_file = out_file;
        self
    }

    /// Sets the resolved stderr log path.
    pub fn err_file(mut self, err_file: Option<String>) -> Self {
        self.info.err_file = err_file;
        self
    }

    /// Sets tree CPU as a percentage of one core.
    pub fn cpu_percent(mut self, cpu_percent: Option<f32>) -> Self {
        self.info.cpu_percent = cpu_percent;
        self
    }

    /// Sets tree resident set size in bytes.
    pub fn memory_bytes(mut self, memory_bytes: Option<u64>) -> Self {
        self.info.memory_bytes = memory_bytes;
        self
    }

    /// Marks this row a dog and names where the dog came from.
    pub fn dog(mut self, dog: Option<DogSource>) -> Self {
        self.info.dog = dog;
        self
    }

    /// Sets the sheep's lamb list; `None` when this reply did not walk for one.
    pub fn lambs(mut self, lambs: Option<Vec<Lamb>>) -> Self {
        self.info.lambs = lambs;
        self
    }

    /// Sets how this sheep's process most recently stopped; `None` while it
    /// has never exited under this daemon.
    pub fn last_exit(mut self, last_exit: Option<ExitInfo>) -> Self {
        self.info.last_exit = last_exit;
        self
    }

    /// Finishes the row.
    #[must_use]
    pub fn build(self) -> ProcessInfo {
        self.info
    }
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

/// What happened when the shepherd tried to deliver one signal.
///
/// `#[non_exhaustive]`: a future outcome — a sheep refused because it is a dog,
/// say, or a delivery held while a stop ladder runs — must not need a protocol
/// version bump (IR-20).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SignalOutcome {
    /// The kernel accepted the signal for this sheep's pid.
    ///
    /// Says the signal was delivered, not that the app did anything with it.
    /// A signal the app blocks, ignores, or has no handler for is `Delivered`
    /// exactly like one it acts on — there is nothing on this path that could
    /// tell the difference, and pretending otherwise would be the dishonest
    /// half of an honest report.
    Delivered,
    /// The sheep is registered but has no live process to signal — stopped,
    /// errored, or waiting out a restart backoff.
    NotRunning,
    /// The kernel refused the delivery; carries its reason (`ESRCH` for a
    /// process reaped between the lookup and the syscall, `EPERM` for one this
    /// daemon may not signal).
    Failed {
        /// The refusal, as the OS worded it.
        reason: String,
    },
}

/// One matched sheep's row in a `Signal` reply.
///
/// Shaped exactly like [`ActionReply`] and for the same reason: spec §9's
/// selector grammar (`all`, `/regex/`, `fold:`) makes a mixed flock the normal
/// case, so a per-row outcome beats a whole-request refusal that would leave
/// the operator unable to tell which half was taken.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalReply {
    /// The sheep's stable id.
    pub id: u32,
    /// The sheep's name.
    pub name: String,
    /// What happened when the shepherd tried to deliver the signal.
    pub outcome: SignalOutcome,
}

/// What happened when the shepherd tried to write one line to a sheep's stdin.
///
/// `#[non_exhaustive]`: a future outcome — a sheep refused because its pipe is
/// backed up, say — must not need a protocol version bump (IR-20).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LineOutcome {
    /// The line was written to the pipe and flushed.
    ///
    /// Says the bytes left the shepherd, not that the app read them. A pipe
    /// holds 64 KiB before it blocks, so a short line to an app that never
    /// reads its stdin is `Sent` — which is honest, because there is nothing
    /// on this path that could tell the difference and a supervisor inventing
    /// one would be guessing.
    Sent,
    /// The sheep has no stdin pipe: its config does not set `stdin = true`, or
    /// it is not running.
    ///
    /// One outcome for two causes, deliberately. The row is read to answer
    /// "why did my line not arrive", and both answers are "there is no pipe
    /// here"; splitting them would put the operator in front of a distinction
    /// with the same fix behind it. A sheep that is not running is visible as
    /// such in `shep flock`, which is where that question belongs.
    NoStdin,
    /// The shepherd had a pipe and did not confirm a write to it; carries
    /// why.
    ///
    /// Three shapes reach it: the write failed (the far end is gone —
    /// normally the app exiting between the lookup and the write), the line
    /// arrived to find the sheep's queue already full, or the write did not
    /// finish inside the shepherd's own bound. The reason names which,
    /// because the operator's next move differs.
    ///
    /// # The last shape does not promise the line was never written
    ///
    /// "Did not confirm", not "could not write", and the difference is the
    /// operator's whole decision about retrying. A write that timed out is a
    /// write the shepherd stopped WAITING for: the bytes may be part-written
    /// into a pipe the app is not draining, and they land in full the moment
    /// it does. There is no way to take them back — abandoning a write
    /// halfway would leave a partial line in the pipe, which is worse than a
    /// slow one.
    ///
    /// What the shepherd does do is drop a line still QUEUED behind that one
    /// once its caller has given up, so retrying a `sendline` cannot pile
    /// duplicates up behind a wedged pipe and deliver them together later.
    /// The first line of a retry sequence is the one that can still arrive
    /// late; treat a retry as a second command, not a repeat of the first.
    NotWritten {
        /// What went wrong, in plain English.
        reason: String,
    },
}

/// One matched sheep's row in a `SendLine` reply.
///
/// Same shape and same argument as [`ActionReply`] and [`SignalReply`]: spec
/// §9's selector grammar makes a mixed flock the normal case, so an outcome
/// per row beats a whole-request refusal.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineReply {
    /// The sheep's stable id.
    pub id: u32,
    /// The sheep's name.
    pub name: String,
    /// What happened.
    pub outcome: LineOutcome,
}

/// A dog's `[dog.<name>]` config section, carried as TOML text.
///
/// This travels over the socket rather than the child's environment for
/// exactly one reason: a dog's section routinely holds webhook credentials
/// (a Discord or Slack URL with a bearer token embedded), and the socket
/// path keeps that out of the process table and out of crash dumps. A
/// derived `Debug` on [`Response`] would undo that the moment something
/// logs a reply — see the manual `Debug` below, which prints only a length.
///
/// `#[serde(transparent)]` makes the wire representation identical to a
/// bare `String`: this newtype changes nothing about
/// [`crate::protocol::PROTOCOL_VERSION`] or the pinned snapshot fixtures.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DogSectionToml(String);

impl DogSectionToml {
    /// The TOML text, empty when the file has no such section.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for DogSectionToml {
    fn from(toml: String) -> Self {
        Self(toml)
    }
}

impl core::ops::Deref for DogSectionToml {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

/// Debug does not print the section body (IR-41) — see the type doc for why.
/// Exact-string-tested below (`dog_section_toml_debug_does_not_leak`) so a
/// future `#[derive(Debug)]` fails that test instead of silently reopening
/// the leak.
impl fmt::Debug for DogSectionToml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DogSectionToml(<{} bytes>)", self.0.len())
    }
}

/// One RPC response (pairs with [`Request`] variants)
///
/// Ten variants carry a bare `Vec<ProcessInfo>` (`Flock`, `Described`,
/// `Started`, `Stopped`, `Restarted`, `Reloading`, `Scaled`, `Reopened`,
/// `Flushed`, `Mustered`), and that repetition is intentional — do not
/// collapse them into one. Each names which request it answers, which is what
/// lets a variant diverge later without a protocol bump: `Reloading` already
/// means an acceptance rather than a result, `Scaled` already means only the
/// survivors on a scale-down rather than every matched row, and `Mustered`
/// already means "every sheep of every restored app" rather than "what this
/// call started". A single `Listing(Vec<ProcessInfo>)` would have to
/// relitigate all three as a breaking change.
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
    /// Answer to `Scale` — the app's instances that will REMAIN, one row each,
    /// in instance-slot order.
    ///
    /// Scaling up, these are the instances that exist, the new ones included,
    /// and the answer is complete.
    ///
    /// Scaling down, these are the survivors and the departing instances are
    /// deliberately absent, even though they are still running their kill
    /// ladders as this reply is written. The operator asked for a number; this
    /// is that number of rows. Listing the departing ones as well would answer
    /// a `scale web 2` with four rows, which is the one thing the reply must
    /// not do. The departures report themselves on the bus as `process.delete`
    /// — the same split `Reloading` already makes between an acceptance and
    /// the swaps that follow it.
    Scaled(Vec<ProcessInfo>),
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
    /// Answer to `Signal` — one [`SignalReply`] row per matched sheep.
    ///
    /// Not a flock listing: what a caller wants back is per-instance delivery,
    /// and [`ProcessInfo`] has nowhere to hold it. Same reasoning, and the
    /// same row-shaped answer, as [`Self::Triggered`].
    Signalled(Vec<SignalReply>),
    /// Answer to `SendLine` — one [`LineReply`] row per matched sheep.
    SentLine(Vec<LineReply>),
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
    /// Answer to `DogConfig` — the dog's own section, rendered back to TOML.
    ///
    /// `toml` is [`DogSectionToml`], not a bare `String`: this text
    /// routinely carries webhook credentials, and the newtype's manual
    /// `Debug` keeps them out of a `{:?}`-formatted `Response` — see that
    /// type's docs for why the section travels over the socket at all.
    DogSection {
        /// The `[dog.<name>]` table as TOML text, empty when the file has
        /// no such section
        toml: DogSectionToml,
    },
    /// Answer to `EnableDog` — the dog as it stands now
    DogStarted(ProcessInfo),
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
            // 12.5 rather than a rounder-looking 12.3: an insta JSON
            // snapshot is only stable across platforms for a float the
            // binary representation holds exactly.
            cpu_percent: Some(12.5),
            memory_bytes: Some(48 * 1024 * 1024),
            dog: None,
            lambs: None,
            // `restarts: 1` above already says this sheep crashed once and
            // came back; a code rather than `None` is the honest exit that
            // caused it, not a fact this fixture invents.
            last_exit: Some(ExitInfo {
                code: Some(1),
                signal: None,
            }),
        }
    }

    /// fails if the builder's defaults drift from what a registered-but-not-yet
    /// running sheep actually looks like. A builder that quietly defaulted
    /// `uptime_ms` to something non-zero, or `restarts` to 1, would put a wrong
    /// number in front of an operator with nothing to compare it against.
    #[test]
    fn a_builder_with_nothing_set_is_a_sheep_that_has_not_run() {
        let info = ProcessInfo::builder(3, "web", ProcStatus::Stopped).build();

        assert_eq!(info.id, 3);
        assert_eq!(info.name, "web");
        assert_eq!(info.status, ProcStatus::Stopped);
        assert_eq!(info.pid, None);
        assert_eq!(info.restarts, 0);
        assert_eq!(info.uptime_ms, 0);
        assert_eq!(info.fold, None);
        assert_eq!(info.out_file, None);
        assert_eq!(info.err_file, None);
        assert_eq!(info.cpu_percent, None);
        assert_eq!(info.memory_bytes, None);
        assert_eq!(info.dog, None);
        assert_eq!(info.lambs, None);
        assert_eq!(info.last_exit, None);
    }

    /// fails if any setter writes a field other than its own — the failure a
    /// twelve-field builder is most likely to ship, and one no individual
    /// round-trip test would catch. Every field is given a value distinct from
    /// every other field's default, so a copy-pasted setter body shows up as a
    /// mismatch rather than as a coincidence.
    #[test]
    fn every_setter_writes_its_own_field_and_no_other() {
        let built = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .restarts(1)
            .uptime_ms(60_000)
            .fold(Some("backend".to_string()))
            .out_file(Some("/home/rin/.shep/logs/web-0-out.log".to_string()))
            .err_file(Some("/home/rin/.shep/logs/web-0-err.log".to_string()))
            .cpu_percent(Some(12.5))
            .memory_bytes(Some(48 * 1024 * 1024))
            .dog(None)
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();

        // `sample_info()` is still a struct literal, on purpose: it is the one
        // place in the workspace that names every field by hand, so this
        // comparison fails the day the struct grows a field the builder cannot
        // set. That is the point of comparing against it rather than against
        // another builder call.
        assert_eq!(built, sample_info());

        // `dog` is the one field the comparison above cannot speak for, and it
        // is the field the whole dogs subsystem reads. `sample_info()`'s `dog`
        // is `None`, which is also the builder's default, so a `dog` setter with
        // an EMPTY BODY passes the assert_eq! above and passes it for the wrong
        // reason. `sample_info()` cannot be changed to `Some(..)` to fix that —
        // it feeds `reply_wire_snapshots` and `bus_event_wire_snapshots`, so
        // altering it moves pinned bytes. So the field gets its own line, with a
        // value nothing defaults to.
        assert_eq!(
            ProcessInfo::builder(1, "metrics", ProcStatus::Online)
                .dog(Some(DogSource::BuiltIn))
                .build()
                .dog,
            Some(DogSource::BuiltIn),
            "an empty `dog` setter body is invisible to the comparison above"
        );

        // `lambs` is the second field the comparison above cannot speak for,
        // for the identical reason `dog` is the first: `sample_info()`'s value
        // is `None`, which is also the builder's default, so an EMPTY `lambs`
        // setter body passes the `assert_eq!` above. And `sample_info()` still
        // cannot be changed to a `Some(..)` — it feeds `reply_wire_snapshots`
        // and `bus_event_wire_snapshots`, so altering it moves pinned bytes.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .lambs(Some(vec![Lamb::new(4243, "node")]))
                .build()
                .lambs,
            Some(vec![Lamb::new(4243, "node")]),
            "an empty `lambs` setter body is invisible to the comparison above"
        );
    }

    /// fails if `lambs` collapses to a bare `Vec`. The three states are the point:
    /// a peer that predates the field and a reply that did not walk the tree are
    /// both `None`, and a sheep that really has no children is `Some(vec![])`. A
    /// `Vec` would render the first two as "this sheep has no lambs", which is a
    /// claim neither of them makes.
    #[test]
    fn lambs_distinguishes_not_walked_from_walked_and_empty() {
        let not_walked = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
        assert_eq!(not_walked.lambs, None);

        let walked_empty = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .lambs(Some(Vec::new()))
            .build();
        assert_eq!(walked_empty.lambs, Some(Vec::new()));
    }

    /// fails if a `ProcessInfo` from a daemon that predates the field stops
    /// deserializing. That is the whole reason the field is optional and the reason
    /// `PROTOCOL_VERSION` does not move for it — an old daemon's reply carries no
    /// `lambs` key at all, and a required field there would mean a new client could
    /// not list against an old daemon.
    #[test]
    fn a_process_info_without_a_lambs_key_still_deserializes() {
        let fixture = r#"{
            "id": 3, "name": "web", "status": "online", "pid": 4242,
            "restarts": 0, "uptime_ms": 100, "fold": null,
            "out_file": null, "err_file": null,
            "cpu_percent": null, "memory_bytes": null, "dog": null
        }"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.lambs, None);
    }

    /// fails if a lamb stops carrying its name, or starts carrying a command line.
    /// The name is `sysinfo`'s executable name, never argv — argv routinely holds
    /// credentials (`--password=`, `?token=`) and `shep describe --format json` is
    /// output people paste into issues.
    #[test]
    fn a_lamb_is_a_pid_and_an_executable_name() {
        let lamb = Lamb::new(4243, "node");
        let json = serde_json::to_string(&lamb).unwrap();
        assert_eq!(json, r#"{"pid":4243,"name":"node"}"#);
        assert_eq!(serde_json::from_str::<Lamb>(&json).unwrap(), lamb);
    }

    /// fails if `DogSource` loses its `tag = "kind"` or its snake_case
    /// rename, and fails if `Adopted`'s `path` is renamed — any of the three
    /// changes one of these two strings while every type-level test in this
    /// module keeps passing. The marker is what the CLI splits two tables on
    /// and what the metrics dog reports a health gauge from, so a silent
    /// rename here is a silently empty dogs table.
    #[test]
    fn a_dog_source_serializes_snake_case_under_its_kind() {
        assert_eq!(
            serde_json::to_string(&DogSource::BuiltIn).unwrap(),
            r#"{"kind":"built_in"}"#
        );
        let adopted = DogSource::Adopted {
            path: "/usr/local/bin/shep-otel".to_string(),
        };
        let wire = r#"{"kind":"adopted","path":"/usr/local/bin/shep-otel"}"#;
        assert_eq!(serde_json::to_string(&adopted).unwrap(), wire);
        assert_eq!(serde_json::from_str::<DogSource>(wire).unwrap(), adopted);
    }

    /// fails if `dog` stops being optional. A daemon built before dogs
    /// sends a reply with no such key and still announces protocol 1, so a
    /// required field would make a current client unable to list against it
    /// at all — the same skew rule `out_file` and `cpu_percent` are pinned
    /// under, and the same committed-byte-fixture proof.
    #[test]
    fn v1_process_info_without_a_dog_marker_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log","cpu_percent":12.5,"memory_bytes":50331648}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.dog, None);
    }

    /// fails if `last_exit` stops being optional. A daemon built before this
    /// field sends a reply with no such key and still announces protocol 1 —
    /// the same skew rule every other field added after `Hello`/`HelloAck`
    /// were fixed is pinned under.
    ///
    /// This is also the empirical proof behind task 49's own open question:
    /// none of `ProcessInfo`'s fields carry `#[serde(default)]`, and there is
    /// no container-level one either, yet the doc comments on `out_file` and
    /// `cpu_percent` both claim "`None` only when the peer daemon predates
    /// this field" as though one existed. Serde's `Deserialize` derive
    /// special-cases a field whose type is syntactically `Option<...>`: a
    /// missing key resolves to `None` without `#[serde(default)]` doing
    /// anything, because the derive macro recognizes the `Option` wrapper
    /// itself and generates that fallback for it. Those doc comments were
    /// right; they just named the wrong mechanism, or none. This test pins
    /// the real one for `last_exit` specifically — with `dog` and `lambs`
    /// present but `last_exit` genuinely absent from the JSON below — rather
    /// than leaving it as an inference from `v1_process_info_without_a_dog_
    /// marker_still_deserializes` above.
    #[test]
    fn a_process_info_without_a_last_exit_key_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log","cpu_percent":12.5,"memory_bytes":50331648,"dog":null,"lambs":null}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.last_exit, None);
    }

    /// fails if a `Signal` frame stops carrying the signal name as plain text, or
    /// if the outcome rows stop distinguishing their three cases. The name travels
    /// as a `String` on purpose (`AppConfig::kill_signal` does the same): the wire
    /// stays readable and the daemon re-validates, which it has to do anyway
    /// because peer input is untrusted.
    #[test]
    fn a_signal_request_and_its_reply_round_trip() {
        let request = Request::Signal {
            selector: SelectorSpec::Name("web".to_string()),
            signal: "SIGHUP".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);

        let reply = Response::Signalled(vec![
            SignalReply {
                id: 1,
                name: "web".to_string(),
                outcome: SignalOutcome::Delivered,
            },
            SignalReply {
                id: 2,
                name: "web".to_string(),
                outcome: SignalOutcome::NotRunning,
            },
            SignalReply {
                id: 3,
                name: "api".to_string(),
                outcome: SignalOutcome::Failed {
                    reason: "no such process".to_string(),
                },
            },
        ]);
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), reply);
        // The three tags, spelled out: a variant renamed in Rust changes these
        // strings mechanically, compiles clean, and breaks a client matching on
        // them with nothing to say why.
        assert!(json.contains(r#""kind":"delivered""#), "{json}");
        assert!(json.contains(r#""kind":"not_running""#), "{json}");
        assert!(json.contains(r#""kind":"failed""#), "{json}");
    }

    /// fails if `Scale` grows a selector. It takes an app NAME, and that is the
    /// design: `instances` is a per-app number and instance slots are allocated
    /// per name-group, so `shep stock /web.*/ 4` would have to mean either four
    /// each or four total and there is no reading of it that is not a guess.
    #[test]
    fn a_scale_request_names_one_app_and_a_count() {
        let request = Request::Scale {
            name: "web".to_string(),
            count: 4,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        assert!(json.contains(r#""kind":"scale""#), "{json}");
        assert!(json.contains(r#""name":"web""#), "{json}");
        // No `selector` key at all — the shape that says this verb is not one of
        // the selector-taking family.
        assert!(!json.contains("selector"), "{json}");
    }

    /// fails if `Scaled` stops being distinguishable from the eight other replies
    /// carrying a bare `Vec<ProcessInfo>`. Each of those names which request it
    /// answers precisely so it can diverge later without a protocol bump — the
    /// enum's own doc says not to collapse them, and this is the test that notices.
    #[test]
    fn a_scaled_reply_carries_its_own_tag() {
        let json = serde_json::to_string(&Response::Scaled(vec![])).unwrap();
        assert_eq!(json, r#"{"kind":"scaled","data":[]}"#);
    }

    /// fails if the three outcomes stop being tellable apart on the wire, or if
    /// `NotWritten` stops carrying its reason. That reason is the only thing that
    /// distinguishes "the app is not reading its stdin" from "the pipe broke", and
    /// the operator's next move differs between them.
    #[test]
    fn a_send_line_request_and_its_reply_round_trip() {
        let request = Request::SendLine {
            selector: SelectorSpec::Name("repl".to_string()),
            line: "reload-config".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);

        let reply = Response::SentLine(vec![
            LineReply {
                id: 1,
                name: "repl".to_string(),
                outcome: LineOutcome::Sent,
            },
            LineReply {
                id: 2,
                name: "web".to_string(),
                outcome: LineOutcome::NoStdin,
            },
            LineReply {
                id: 3,
                name: "stuck".to_string(),
                outcome: LineOutcome::NotWritten {
                    reason: "the app did not read its stdin within 2s".to_string(),
                },
            },
        ]);
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), reply);
        assert!(json.contains(r#""kind":"sent""#), "{json}");
        assert!(json.contains(r#""kind":"no_stdin""#), "{json}");
        assert!(json.contains("did not read its stdin"), "{json}");
    }

    /// fails if a newline can ride inside the line. The wire carries ONE line and
    /// the writer appends the terminator, so an embedded newline would deliver two
    /// commands where the operator typed one — the shape that turns a typo into an
    /// unintended second instruction to a REPL.
    #[test]
    fn a_line_carrying_a_newline_is_still_one_field_on_the_wire() {
        let request = Request::SendLine {
            selector: SelectorSpec::All,
            line: "a\nb".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        // Escaped, not literal: the frame stays one JSON object. Rejecting it is
        // the daemon's job (see `shep whisper`), not serde's, and this pins that
        // the wire itself does not quietly split it.
        assert!(json.contains(r#""line":"a\nb""#), "{json}");
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
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
            // The three dog verbs together, in the order an operator meets
            // them: ask for a section, start a dog, stop one. Adjacent on
            // purpose — `enable_dog` and `disable_dog` differ by their
            // `kind` and by `source`, so a `DisableDog` accidentally given
            // `EnableDog`'s tag shows up here as two near-identical objects
            // rather than as a diff a reader has to compare field by field.
            Envelope {
                id: 11,
                deadline_ms: None,
                body: Request::DogConfig {
                    name: "bark".to_string(),
                },
            },
            Envelope {
                id: 12,
                deadline_ms: None,
                body: Request::EnableDog {
                    name: "metrics".to_string(),
                    source: DogSource::BuiltIn,
                },
            },
            Envelope {
                id: 13,
                deadline_ms: None,
                body: Request::DisableDog {
                    name: "metrics".to_string(),
                },
            },
            // The three selector shapes no fixture reached before Phase 10.
            // Grouped and adjacent on purpose: `Id`, `Regex` and `Fold` are
            // three newtypes over three different inner types, and the wire
            // tells them apart only by their own `kind` tag — a `Fold` that
            // serialized under `regex`'s tag is a `shep restart fold:api`
            // that silently becomes a regex match, which is a wrong set of
            // sheep restarted and not an error anyone sees.
            Envelope {
                id: 14,
                deadline_ms: None,
                body: Request::Describe {
                    selector: SelectorSpec::Id(7),
                },
            },
            Envelope {
                id: 15,
                deadline_ms: None,
                body: Request::Describe {
                    selector: SelectorSpec::Regex("^web-".to_string()),
                },
            },
            Envelope {
                id: 16,
                deadline_ms: None,
                body: Request::Describe {
                    selector: SelectorSpec::Fold("api".to_string()),
                },
            },
            // `SIGHUP` rather than `SIGTERM`: TERM is what the stop ladder
            // already sends, so a fixture using it could not tell a `signal`
            // frame from a stop's. HUP is the signal this verb exists for.
            Envelope {
                id: 17,
                deadline_ms: None,
                body: Request::Signal {
                    selector: SelectorSpec::Name("web".to_string()),
                    signal: "SIGHUP".to_string(),
                },
            },
            // The one verb in this enum whose body has no `selector` key at
            // all — a reader comparing this row against `stop`'s sees the
            // whole difference in one place.
            Envelope {
                id: 18,
                deadline_ms: None,
                body: Request::Scale {
                    name: "web".to_string(),
                    count: 4,
                },
            },
            // `SelectorSpec::All` rather than a named sheep, mirroring the
            // `reopen`/`flush` rows above: it is the widest thing an operator
            // can type, and the line carries no terminator on the wire — the
            // shepherd appends it — so a fixture with one proves that half of
            // the contract too.
            Envelope {
                id: 19,
                deadline_ms: None,
                body: Request::SendLine {
                    selector: SelectorSpec::All,
                    line: "reload-config".to_string(),
                },
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
            // `sample_info()` above pins the absent marker (a sheep's
            // `"dog": null`); this row is the only place the present one is
            // pinned, and `Adopted` rather than `BuiltIn` because it is the
            // variant carrying a payload — the unit variant's shape is
            // already proven by every fieldless variant on this wire.
            Reply {
                id: 6,
                result: Ok(Response::Flock(vec![ProcessInfo {
                    id: 7,
                    name: "otel".to_string(),
                    dog: Some(DogSource::Adopted {
                        path: "/usr/local/bin/shep-otel".to_string(),
                    }),
                    ..sample_info()
                }])),
            },
            // The opaque blob, pinned as a blob: the daemon renders a TOML
            // table into a string and never a typed structure, so what this
            // row proves is that the section crosses the wire as text.
            Reply {
                id: 7,
                result: Ok(Response::DogSection {
                    toml: "port = 9615\n".to_string().into(),
                }),
            },
            // The only `Response` variant carrying a BARE `ProcessInfo`
            // rather than a `Vec` of them: `enable` starts exactly one dog,
            // and a one-element list would invite a reader to wonder when it
            // holds two.
            Reply {
                id: 8,
                result: Ok(Response::DogStarted(ProcessInfo {
                    id: 4,
                    name: "metrics".to_string(),
                    dog: Some(DogSource::BuiltIn),
                    ..sample_info()
                })),
            },
            // The eleven variants no fixture reached before Phase 10. The
            // existing comment on the `Triggered` row is right that pinning
            // `Flock` once already proves the `Vec<ProcessInfo>` SHAPE — but
            // it does not prove any of these variants' own `kind` tags, and
            // three of them are not `Vec<ProcessInfo>`-shaped at all
            // (`Deleted` is a `Vec<u32>`, `Subscribed` and `ShuttingDown`
            // carry nothing). Each row below therefore carries the emptiest
            // legal body: what is being pinned here is the tag, and a body
            // repeated eight times would bury it.
            Reply {
                id: 9,
                result: Ok(Response::Described(vec![])),
            },
            Reply {
                id: 10,
                result: Ok(Response::Started(vec![])),
            },
            Reply {
                id: 11,
                result: Ok(Response::Stopped(vec![])),
            },
            Reply {
                id: 12,
                result: Ok(Response::Restarted(vec![])),
            },
            Reply {
                id: 13,
                result: Ok(Response::Reloading(vec![])),
            },
            Reply {
                id: 14,
                result: Ok(Response::Deleted(vec![7, 8])),
            },
            Reply {
                id: 15,
                result: Ok(Response::Reopened(vec![])),
            },
            Reply {
                id: 16,
                result: Ok(Response::Flushed(vec![])),
            },
            Reply {
                id: 17,
                result: Ok(Response::Mustered(vec![])),
            },
            Reply {
                id: 18,
                result: Ok(Response::Subscribed),
            },
            Reply {
                id: 19,
                result: Ok(Response::ShuttingDown),
            },
            // `Signalled`, mirroring the `Triggered` row above: three rows,
            // one per `SignalOutcome` variant, so a reader sees the whole
            // shape of the reply in one pinned fixture rather than one row
            // that happens to hit `Delivered` and leaves the other two tags
            // unproven.
            Reply {
                id: 20,
                result: Ok(Response::Signalled(vec![
                    SignalReply {
                        id: 1,
                        name: "web".to_string(),
                        outcome: SignalOutcome::Delivered,
                    },
                    SignalReply {
                        id: 2,
                        name: "web".to_string(),
                        outcome: SignalOutcome::NotRunning,
                    },
                    SignalReply {
                        id: 3,
                        name: "api".to_string(),
                        outcome: SignalOutcome::Failed {
                            reason: "no such process".to_string(),
                        },
                    },
                ])),
            },
            Reply {
                id: 21,
                result: Ok(Response::Scaled(vec![sample_info()])),
            },
            // `SentLine`, mirroring the `Signalled` row above: three rows, one
            // per `LineOutcome` variant, so a reader sees the whole shape of
            // the reply in one pinned fixture rather than one row that
            // happens to hit `Sent` and leaves the other two tags unproven.
            Reply {
                id: 22,
                result: Ok(Response::SentLine(vec![
                    LineReply {
                        id: 1,
                        name: "repl".to_string(),
                        outcome: LineOutcome::Sent,
                    },
                    LineReply {
                        id: 2,
                        name: "web".to_string(),
                        outcome: LineOutcome::NoStdin,
                    },
                    LineReply {
                        id: 3,
                        name: "stuck".to_string(),
                        outcome: LineOutcome::NotWritten {
                            reason: "the app did not read its stdin within 2s".to_string(),
                        },
                    },
                ])),
            },
            // A `Described` row with a real lamb tree. The `null` shape is pinned
            // on every other row here; this is the one that pins what a walked
            // sheep serializes as, which is the shape a `describe` consumer
            // actually parses.
            Reply {
                id: 23,
                result: Ok(Response::Described(vec![
                    ProcessInfo::builder(3, "web", ProcStatus::Online)
                        .pid(Some(4242))
                        .lambs(Some(vec![Lamb::new(4243, "node"), Lamb::new(4244, "sh")]))
                        .build(),
                ])),
            },
            // `sample_info()` pins `last_exit`'s "exited normally" shape
            // (`code` set, `signal` absent) on every row above; this is the
            // only place the other one — killed by a signal, `code` absent
            // — is pinned. `SIGTERM`'s raw number (15) rather than a
            // symbolic one, because [`ExitInfo::signal`]'s own doc says this
            // crate carries no name for it; naming one is a job for
            // whichever OS-aware layer renders this field.
            Reply {
                id: 24,
                result: Ok(Response::Flock(vec![
                    ProcessInfo::builder(5, "worker", ProcStatus::Stopped)
                        .restarts(1)
                        .last_exit(Some(ExitInfo {
                            code: None,
                            signal: Some(15),
                        }))
                        .build(),
                ])),
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

    /// fails if the two fields stop being optional. A daemon built before
    /// them sends a reply with no such keys, and both peers still announce
    /// protocol 1 — a required field would make a current client unable to
    /// list against that daemon at all.
    #[test]
    fn v1_process_info_without_stats_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log"}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.cpu_percent, None);
        assert_eq!(info.memory_bytes, None);
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

    /// fails if any of the three verbs or either reply is given a `rename`,
    /// or if `Response`'s `content = "data"` is dropped. `disable_dog`'s
    /// answer is `Deleted`, which no other test in this module pairs with
    /// this verb — a handler wired to answer `Deleted` for `EnableDog` would
    /// still round-trip, and this is where the pairing is written down.
    #[test]
    fn the_dog_verbs_serialize_snake_case_with_their_payloads_under_data() {
        assert_eq!(
            serde_json::to_string(&Request::DogConfig {
                name: "bark".to_string()
            })
            .unwrap(),
            r#"{"kind":"dog_config","name":"bark"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::DisableDog {
                name: "bark".to_string()
            })
            .unwrap(),
            r#"{"kind":"disable_dog","name":"bark"}"#
        );
        let section = Response::DogSection {
            toml: "port = 9615\n".to_string().into(),
        };
        let wire = r#"{"kind":"dog_section","data":{"toml":"port = 9615\n"}}"#;
        assert_eq!(serde_json::to_string(&section).unwrap(), wire);
        assert_eq!(serde_json::from_str::<Response>(wire).unwrap(), section);
    }

    #[test]
    fn dog_section_toml_debug_does_not_leak() {
        // IR-41: a dog's `[dog.<name>]` section routinely holds webhook
        // credentials (a Discord/Slack URL with a bearer token embedded).
        // `Response` derives `Debug`, so this is the one thing standing
        // between that token and any future `tracing::debug!("{:?}", reply)`.
        // Exact string pinned so a lazy `#[derive(Debug)]` refactor on
        // `DogSectionToml` fails this test instead of silently reopening
        // the leak.
        let toml: DogSectionToml =
            "webhook_url = \"https://discord.com/api/webhooks/1/super-secret-token\"\n"
                .to_string()
                .into();
        assert_eq!(format!("{toml:?}"), "DogSectionToml(<70 bytes>)");

        let response = Response::DogSection { toml };
        assert_eq!(
            format!("{response:?}"),
            "DogSection { toml: DogSectionToml(<70 bytes>) }"
        );
    }
}
