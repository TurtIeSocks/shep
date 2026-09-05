//! RPC frames: requests, responses, envelopes, and structured errors

use core::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::config::{AppConfig, DeclaredApp, ResetDepth};
use crate::status::ProcStatus;

/// Client's opening frame
///
/// No `deny_unknown_fields`: refusing an unknown field here would refuse a
/// newer client before `protocol` is read.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Client crate version (semver string)
    pub client_version: String,
    /// [`crate::protocol::PROTOCOL_VERSION`] the client speaks
    pub protocol: u32,
    /// The name this client was registered under as a dog, when it is one.
    ///
    /// `None` for every other client; a bare `Client` cannot set it. The
    /// daemon needs it to name a dog it refuses at the handshake, which never
    /// reaches `Request::DogConfig`. A dog reads its own name from
    /// `$SHEP_DOG_NAME`.
    ///
    /// Absent on the wire rather than `null`, so
    /// [`crate::protocol::PROTOCOL_VERSION`] does not move for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dog_name: Option<String>,
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
    // Both field names are wire contract, pinned by `request_wire_v3`.
    /// By app name and instance slot
    ///
    /// On the wire: `{"kind":"instance","value":{"name":"web","slot":2}}`.
    Instance {
        /// The app name
        name: String,
        /// The instance slot, counting from 0
        slot: u32,
    },
}

/// A short marker a dog attaches to a sheep for `shep flock` to paint.
///
/// shep stores and prints it, never parses it: `▲ main@a1b2c3` is a
/// deploy tool's sentence.
///
/// The grammar: non-empty once whitespace is discounted, at most
/// [`Self::MAX_CHARS`] characters, no [`char::is_control`] character,
/// `\u{1b}` included. Refused, never repaired, and validated here rather
/// than at the renderer: `shep`'s own `output::width::sanitize_cell` keeps
/// a well-formed CSI sequence, since shep's colouring is made of them.
///
/// [`Self::MAX_CHARS`] counts `char`s, not bytes: a byte cap would refuse a
/// legitimate CJK smit at roughly a third of its apparent length.
///
/// `Debug` is derived: a smit carries no secret, so there is nothing to
/// redact.
///
/// # Example
/// ```
/// use shep_core::protocol::Smit;
///
/// assert_eq!("▲ main@a1b2c3".parse::<Smit>()?.as_str(), "▲ main@a1b2c3");
/// assert!("\u{1b}[2Jgone".parse::<Smit>().is_err()); // no escapes
/// # Ok::<(), shep_core::protocol::SmitError>(())
/// ```
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Smit(String);

impl Smit {
    /// The longest a smit may be, in characters.
    pub const MAX_CHARS: usize = 48;

    /// The marker as text, exactly as its publisher sent it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Smit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::str::FromStr for Smit {
    type Err = SmitError;

    /// # Errors
    /// - [`SmitError::Empty`] if the text is nothing but whitespace.
    /// - [`SmitError::TooLong`] if it is over [`Self::MAX_CHARS`] characters.
    /// - [`SmitError::Unprintable`] if it holds a control character.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.trim().is_empty() {
            return Err(SmitError::Empty);
        }
        let chars = text.chars().count();
        if chars > Self::MAX_CHARS {
            return Err(SmitError::TooLong { chars });
        }
        if text.chars().any(char::is_control) {
            return Err(SmitError::Unprintable);
        }
        Ok(Self(text.to_string()))
    }
}

/// Validates on decode: a dog written in another language speaks this wire
/// directly and never runs [`core::str::FromStr`], so a derived impl would
/// let `\u{1b}[2J` reach every listing built from a smit.
impl<'de> Deserialize<'de> for Smit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // String, not &str: a non-borrowing deserializer cannot always borrow
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Why a string is not a [`Smit`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmitError {
    /// Over [`Smit::MAX_CHARS`] characters; carries the count that was sent.
    TooLong {
        /// How many characters the string held.
        chars: usize,
    },
    /// A control character, `\u{1b}` included.
    Unprintable,
    /// Empty, or nothing but whitespace.
    Empty,
}

impl fmt::Display for SmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { chars } => write!(
                f,
                "a smit is at most {} characters; this one is {chars}",
                Smit::MAX_CHARS
            ),
            Self::Unprintable => {
                f.write_str("a smit may not contain a control character, an escape included")
            }
            Self::Empty => f.write_str("a smit may not be empty"),
        }
    }
}

impl core::error::Error for SmitError {}

/// One RPC request
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
        /// App configs. The daemon must re-normalize them, since peer input
        /// is untrusted; failures return [`RpcErrorCode::InvalidConfig`]
        apps: Vec<AppConfig>,
    },
    /// Register apps as flock members without starting any of them
    ///
    /// Each app lands `Stopped` and holds no pid; `shep add` is the verb.
    ///
    /// Idempotent by name: an app the flock already has is answered as it
    /// stands, running or not, and nothing about it changes.
    /// [`Self::ApplyConfig`] merges a template into one the flock already
    /// has, and `shep add` sends both.
    ///
    /// Answers [`Response::Added`].
    Add {
        /// App configs, carried exactly as [`Self::Start`] carries them. The
        /// daemon must re-normalize them, since peer input is untrusted;
        /// failures return [`RpcErrorCode::InvalidConfig`]
        apps: Vec<AppConfig>,
    },
    /// Ask which of `apps` name a sheep the flock already has under a
    /// different config
    ///
    /// Read-only. [`Self::Start`] on an already-registered name adds
    /// instances rather than reconciling config.
    ///
    /// Answers [`Response::Drifted`] with one [`SheepDrift`] per app that is
    /// both registered and different. An app the flock does not have is
    /// absent from the answer, not reported as unchanged.
    ConfigDrift {
        /// The configs to compare against, exactly as [`Self::Start`] would
        /// carry them. The daemon must re-normalize them: peer input is
        /// untrusted, and an unnormalized config would report every default
        /// it has not spelled out as a difference. Failures return
        /// [`RpcErrorCode::InvalidConfig`].
        apps: Vec<AppConfig>,
    },
    /// Merge each declared app into the sheep of the same name, applying
    /// what can be applied and parking the rest for that sheep's next spawn
    ///
    /// Nothing is registered, nothing is pruned and nothing running is
    /// killed: an app the flock does not have is refused by name, and a
    /// field the running child was spawned from waits for a `shep reload`.
    /// Additive by default; `reset` widens it.
    ///
    /// Answers [`Response::Applied`] with one [`SheepApplied`] per entry in
    /// `apps`, in the order given, found or not and changed or not. One
    /// app's refusal rides in [`SheepApplied::refused`] and does not cost
    /// the rest of the file its load.
    ApplyConfig {
        /// The apps to merge in, each carrying the keys its document
        /// literally wrote. The daemon must re-normalize the merge result,
        /// since peer input is untrusted, and refuses the whole request with
        /// [`RpcErrorCode::InvalidConfig`] when two entries share a name:
        /// the second would be merged against a store the first has not
        /// written yet.
        apps: Vec<DeclaredApp>,
        /// How much of what the operator has set since a template last
        /// loaded this request may overwrite. Default [`ResetDepth::None`],
        /// which overwrites nothing.
        ///
        /// Spelled `none`/`file`/`env`/`policy` on the wire.
        reset: ResetDepth,
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
        /// Which sheep. No default: a reload replaces running processes.
        selector: SelectorSpec,
    },
    /// Stop + deregister matching sheep
    Delete {
        /// Which sheep
        selector: SelectorSpec,
    },
    /// Set how many instances one app runs (see `shep stock`).
    ///
    /// Takes a name where every other verb takes a [`SelectorSpec`]:
    /// `instances` is a per-app number and slots are allocated against the
    /// same-name group, so a selector matching two apps could mean four of
    /// each or four in total.
    ///
    /// The count is absolute: two operators sending `+2` against the same
    /// app would get a number neither asked for.
    Scale {
        /// The app's name, exactly as its config spells it. Not a selector: no
        /// `all`, no regex, no `fold:`.
        name: String,
        /// How many instances the app has when this returns. `0` is refused
        /// with [`RpcErrorCode::InvalidConfig`]: `shep delete` is the verb
        /// for removing an app.
        count: u32,
    },
    /// Attach a short marker to `sheep` for `shep flock` to paint, or clear
    /// it with `None`.
    ///
    /// By name, not a selector: a smit belongs to a sheep, and every
    /// instance of that name shows it, one spawned after the paint included.
    ///
    /// Held in memory and scoped to the connection that sent it, so a
    /// publisher republishes rather than publishing on change.
    SetSmit {
        /// Which sheep.
        sheep: String,
        /// The marker, or `None` to clear it.
        smit: Option<Smit>,
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
        /// Which sheep. No default: this destroys log data.
        selector: SelectorSpec,
    },
    /// Send a named action to every matched sheep over its shepherd channel
    /// and report what each app says back (see `shep trigger`).
    Trigger {
        /// Which sheep. No default, matching every other verb that reaches
        /// a running process.
        selector: SelectorSpec,
        /// The action name. Free-form: the daemon never declares, parses, or
        /// validates it, and an app that does not recognize the name is
        /// expected to say so in its own reply.
        action: String,
        /// Argument text, passed through to the app verbatim. One opaque
        /// string, matching the shepherd channel's own `action` message this
        /// becomes.
        params: Option<String>,
    },
    /// Deliver one signal to every matched sheep's own process, never its
    /// process group (see `shep signal`).
    Signal {
        /// Which sheep. No default, matching every other verb that reaches
        /// a running process.
        selector: SelectorSpec,
        /// The signal's name, as
        /// [`OperatorSignal`](crate::signals::OperatorSignal) spells it. The
        /// `SIG` prefix and the case are both optional; a name outside the
        /// grammar answers [`RpcErrorCode::InvalidConfig`].
        signal: String,
    },
    /// Write one line to every matched sheep's stdin (see `shep whisper`).
    SendLine {
        /// Which sheep. No default, matching every other verb that reaches a
        /// running process.
        selector: SelectorSpec,
        /// The line, without its terminator: the shepherd appends exactly
        /// one `\n` when it writes.
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
        /// The dog's name: the config key, not a selector
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
    /// Answers [`Response::Deleted`]: disabling deregisters exactly as
    /// `Delete` does.
    DisableDog {
        /// The dog's name
        name: String,
    },
    /// Ask which dogs this daemon has given up on, and which it is still
    /// waiting to hear from (`shep daemon reload`).
    ///
    /// Read-only, and about this daemon's own handshakes: take the reading
    /// after a reload, not before one. Never sent to an older daemon, on
    /// [`Self::HandoverFitness`]'s terms.
    ///
    /// Answers [`Response::DogStaleness`].
    DogStaleness,
    /// Ask whether this daemon could hand its flock to a successor in place,
    /// rather than stopping it and starting it again (`shep daemon reload`).
    ///
    /// Read-only: the handover itself is triggered by a signal, which reaches
    /// a daemon that refuses the client at the handshake.
    ///
    /// Answers [`Response::HandoverFitness`]. A refusal is a feature the
    /// running daemon cannot carry, not an error: the caller falls back to a
    /// stop-and-start and prints the reason. Never sent to an older daemon:
    /// shep-cli's `commands::daemon` gates it on the crate version the
    /// handshake reported.
    HandoverFitness,
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
/// Carried on [`ProcessInfo::dog`], so a listing distinguishes the two
/// populations without a second request.
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
/// Not the set of processes that die with the sheep: this is a parent-pid
/// walk, where the stop ladder acts on the process group. A lamb that forks
/// and exits leaves children re-parented to init, out of this list and still
/// in the group; a `setsid()` grandchild stays in the list and leaves the
/// group.
///
/// `name` is the executable's name (`node`, `sh`), never argv, which carries
/// credentials and would ride into `shep describe --format json`. Build one
/// with [`Self::new`].
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
/// Behind [`ProcessInfo::last_exit`]'s own `Option`, so `None` there means
/// never exited. Ordinarily exactly one of `code`/`signal` is `Some`,
/// mirroring the OS's `WIFEXITED`/`WIFSIGNALED` split; both `None` together
/// is legal and means this daemon recorded an exit it could not
/// characterize.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    /// The process's own exit code, set on a normal exit (`WIFEXITED`).
    pub code: Option<i32>,
    /// The raw unix signal number that ended the process, set when it did
    /// not exit on its own (`WIFSIGNALED`). An operator's own `shep stop`
    /// counts: the process still stopped by a signal.
    ///
    /// Platform-specific, and never rendered as a name here; that is an
    /// OS-aware layer's job.
    pub signal: Option<i32>,
}

/// Snapshot of one sheep for listings and events
///
/// Construct one with [`ProcessInfo::builder`]: `#[non_exhaustive]` forbids
/// a struct literal outside this crate, though not inside it. The fields
/// stay `pub`.
// wire format: changing this is a breaking change. No `Eq`: `cpu_percent` is
// an `f32`. Paths travel as `String`, since serde's `PathBuf` refuses a
// non-UTF-8 path and would blank a whole `Reply`. Every added field is an
// `Option`, so a peer built before it sends no key and `None` reads as unknown.
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
    /// peer daemon predates the field; all three render as unknown, never as
    /// zero. A value over 100 is a tree using more than one core.
    pub cpu_percent: Option<f32>,
    /// Tree resident set size in bytes, current as of the reply. `None`
    /// under the same three conditions as [`Self::cpu_percent`], minus the
    /// window one: memory needs no baseline.
    pub memory_bytes: Option<u64>,
    /// Set when this entry is a dog, naming where the dog came from;
    /// `None` for a sheep.
    ///
    /// Two cases, not [`Self::cpu_percent`]'s three: "not a dog" is the true
    /// answer whether the peer predates the field or the entry is a sheep.
    pub dog: Option<DogSource>,
    /// The processes the OS reports as descendants of this sheep, or `None`
    /// when this reply did not walk for them.
    ///
    /// `None` covers two cases: this reply is not a `Describe` (only
    /// `Describe` walks), or the peer daemon predates the field.
    /// `Some(vec![])` is the third: walked, and this sheep has no children.
    ///
    /// Read [`Lamb`]'s own doc before rendering this: the list is not the set
    /// of processes a stop kills, and any output built from it has to say so.
    pub lambs: Option<Vec<Lamb>>,
    /// How this sheep's process most recently stopped existing under this
    /// daemon. `None` while it has never exited under this daemon, and when
    /// the peer daemon predates the field.
    ///
    /// Sticky across a respawn: it answers why the sheep last stopped, not
    /// whether it is stopped now, and updates only on the next exit.
    pub last_exit: Option<ExitInfo>,
    /// The marker a dog has asked to have painted beside this sheep, or
    /// `None` when no dog has painted one, which also covers a peer daemon
    /// that predates the field.
    ///
    /// A `String` rather than a [`Smit`]: this is a report, and the
    /// validation that makes it safe to print happened at the daemon's
    /// ingress. Every instance of a name shows the same marker, since smits
    /// are keyed by sheep name.
    pub smit: Option<String>,
    /// Which instance slot of its app this sheep occupies, counting from 0.
    ///
    /// `None` when the peer daemon predates the field. Not a bare `u32`
    /// defaulted to 0: an app stocked to four instances would report four
    /// rows all claiming slot 0.
    pub instance: Option<u32>,
    /// Whether this dog has completed a handshake with the shepherd that is
    /// reporting it, and not been refused since; `None` for a sheep.
    ///
    /// `None` on [`Self::dog`]'s two-case terms: a sheep has no connection
    /// to the shepherd, so "no handshake fact to report" is the true answer
    /// for a sheep and for a peer that predates the field alike.
    ///
    /// `Some(false)` is the one that matters: a dog on a protocol this
    /// shepherd refuses is alive, which is all [`Self::status`] reports, and
    /// not doing its job. A fact and not a verdict, though: a dog spawned a
    /// moment ago has not handshaken yet and is healthy.
    pub handshook: Option<bool>,
    /// Whether the reporting shepherd has given up on this dog: restarted it
    /// once for never answering, watched that not help, and stopped
    /// restarting it. `None` for a sheep, on [`Self::handshook`]'s terms.
    ///
    /// Not derivable from [`Self::handshook`]. `Some(false)` there covers
    /// both a dog spawned three seconds ago that has not dialled back and one
    /// this shepherd has permanently stopped restarting; the first needs
    /// nothing done and the second is an incident.
    ///
    /// A fact and not a verdict: it says the shepherd stopped, never why.
    /// The why is in that dog's own log (`shep bleats <dog>`).
    pub dog_stale: Option<bool>,
    /// The [`AppConfig`] field names this sheep's spec differs from a load's
    /// parked config for, in field-name order. `None` when nothing is parked,
    /// and when the peer daemon predates the field.
    ///
    /// Names only, never values, as [`SheepDrift::fields`] carries them: a
    /// differing `env` reports `"env"` and stops there. `shep reload`
    /// promotes a parked config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<String>>,
    /// The [`AppConfig`] field names an operator has set on this sheep that
    /// its current Flockfile does not declare, in field-name order. `None`
    /// when there is nothing to report, and when the peer daemon predates
    /// the field.
    ///
    /// Names only, never values, for [`Self::pending`]'s reason:
    /// [`crate::overrides::AppOverrides::fields`] can hold an `env` value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden: Option<Vec<String>>,
}

/// Orders one flock listing the way every operator-facing surface presents
/// one: `(name, instance, id)`.
///
/// Name first: an id is assigned at registration and a `delete all` plus a
/// fresh start renumbers the flock, where a name survives. A name is not a
/// total order on its own, so the id breaks the tie and stays an addressing
/// key (`shep stop 11`).
///
/// A listing whose rows all carry `None` for the slot collapses to
/// `(name, id)`, since `None` sorts before every `Some`.
pub fn sort_flock(listing: &mut [ProcessInfo]) {
    listing.sort_unstable_by(|a, b| {
        (a.name.as_str(), a.instance, a.id).cmp(&(b.name.as_str(), b.instance, b.id))
    });
}

impl ProcessInfo {
    /// Starts a builder for one sheep's row.
    ///
    /// The three arguments are the fields no row can omit and no reader can
    /// default.
    ///
    /// No `#[must_use]`: [`ProcessInfoBuilder`] carries one, which clippy's
    /// `double_must_use` lint treats as covering this return too.
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
                smit: None,
                instance: None,
                handshook: None,
                dog_stale: None,
                pending: None,
                overridden: None,
            },
        }
    }
}

/// Builds a [`ProcessInfo`], which is `#[non_exhaustive]` and so cannot be
/// written as a struct literal outside this crate.
///
/// Every setter takes the field's own type, `Option` included, so a caller
/// already holding `Option<u32>` writes `.pid(entry.pid())` rather than an
/// `if let` ladder. A setter is skipped, not passed `None`, when a row has
/// nothing to say about that field; the skipped defaults are the ones a
/// not-yet-running sheep has.
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

    /// Sets the marker a dog has painted on this sheep; `None` when none has.
    pub fn smit(mut self, smit: Option<String>) -> Self {
        self.info.smit = smit;
        self
    }

    /// Sets the instance slot; `None` when the peer daemon predates the field.
    pub fn instance(mut self, instance: Option<u32>) -> Self {
        self.info.instance = instance;
        self
    }

    /// Sets whether this dog has handshaken with the shepherd; `None` for a
    /// sheep, which has no handshake to report.
    pub fn handshook(mut self, handshook: Option<bool>) -> Self {
        self.info.handshook = handshook;
        self
    }

    /// Sets whether the shepherd has given up restarting this dog; `None`
    /// for a sheep, which is never given up on.
    pub fn dog_stale(mut self, dog_stale: Option<bool>) -> Self {
        self.info.dog_stale = dog_stale;
        self
    }

    /// Sets the field names a load has parked for this sheep's next spawn;
    /// `None` when nothing is parked.
    pub fn pending(mut self, pending: Option<Vec<String>>) -> Self {
        self.info.pending = pending;
        self
    }

    /// Sets the field names an operator has overridden on this sheep;
    /// `None` when there is nothing to report.
    pub fn overridden(mut self, overridden: Option<Vec<String>>) -> Self {
        self.info.overridden = overridden;
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
    /// The sheep is a reload drainee, mid-swap and on its way out, so the
    /// daemon skipped it rather than deliver the action to a process already
    /// being replaced.
    Skipped,
    /// The daemon delivered the action, but no reply arrived before the
    /// app's configured action timeout elapsed.
    TimedOut,
}

/// One matched sheep's row in a `Trigger` reply.
///
/// Not a [`ProcessInfo`]: a reply body has nowhere to live on one.
/// [`Self::outcome`] is per-row, since the selector grammar makes a mixed
/// flock the normal case.
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
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SignalOutcome {
    /// The kernel accepted the signal for this sheep's pid.
    ///
    /// Says the signal was delivered, not that the app did anything with it.
    /// A signal the app blocks or ignores is `Delivered` too.
    Delivered,
    /// The sheep is registered but has no live process to signal: stopped,
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
/// Per-row like [`ActionReply`]: the selector grammar makes a mixed flock
/// the normal case.
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
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LineOutcome {
    /// The line was written to the pipe and flushed.
    ///
    /// Says the bytes left the shepherd, not that the app read them. A pipe
    /// holds 64 KiB before it blocks.
    Sent,
    /// The sheep has no stdin pipe: its config does not set `stdin = true`, or
    /// it is not running.
    ///
    /// One outcome for two causes: both answer "there is no pipe here".
    NoStdin,
    /// The shepherd had a pipe and did not confirm a write to it; carries
    /// why.
    ///
    /// Three shapes reach it: the write failed (the far end is gone), the
    /// line found the sheep's queue already full, or the write did not
    /// finish inside the shepherd's own bound. The reason names which.
    ///
    /// A timed-out write is not a promise the line was never written: the
    /// bytes may be part-written into a pipe the app is not draining, and
    /// land in full the moment it drains. A line still queued behind that one
    /// is dropped once its caller gives up, so treat a retry as a second
    /// command.
    NotWritten {
        /// What went wrong, in plain English.
        reason: String,
    },
}

/// One matched sheep's row in a `SendLine` reply.
///
/// Per-row like [`ActionReply`] and [`SignalReply`].
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
/// Travels over the socket rather than the child's environment: a dog's
/// section routinely holds webhook credentials, and the socket keeps them
/// out of the process table and out of crash dumps. The manual `Debug`
/// below prints only a length, since [`Response`] derives `Debug`.
///
/// `#[serde(transparent)]`: the wire representation is a bare `String`.
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

/// Prints a length, never the section body. Pinned as an exact string by
/// `dog_section_toml_debug_does_not_leak`.
impl fmt::Debug for DogSectionToml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DogSectionToml(<{} bytes>)", self.0.len())
    }
}

/// One registered sheep whose stored config differs from a caller's copy:
/// the answer [`Request::ConfigDrift`] is asking for
///
/// Field names only, never their values. This is printed at an operator,
/// and [`AppConfig::env`](crate::config::AppConfig::env) carries secrets,
/// so a differing `env` reports `"env"` and nothing more. `Debug` is
/// derived: there is nothing here to redact.
// wire format: changing field names is a breaking change
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheepDrift {
    /// The sheep's name. Both configs share it by construction: it is what
    /// matched them to each other.
    pub name: String,
    /// The [`AppConfig`] fields that differ, in field-name order. Never
    /// empty: a sheep with nothing to report is left out of the answer.
    pub fields: Vec<String>,
}

impl SheepDrift {
    /// Builds one sheep's report.
    #[must_use]
    pub fn new(name: impl Into<String>, fields: Vec<String>) -> Self {
        Self {
            name: name.into(),
            fields,
        }
    }
}

/// What one app's [`Request::ApplyConfig`] did: the answer a load owes the
/// operator who ran it
///
/// One of these per app the request named, found or not and changed or not.
///
/// [`Self::applied`] and [`Self::pending`] carry field names only, never
/// their values, as [`SheepDrift`] does; the merged config never reaches a
/// client. [`Self::refused`] is prose and is scoped out of that rule: it
/// quotes values out of the file the caller just sent, never out of the
/// flock's stored config. `Debug` is derived on that basis: nothing here
/// needs redacting.
// wire format: changing field names is a breaking change
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheepApplied {
    /// The sheep's name, exactly as the request spelled it.
    pub name: String,
    /// Fields now in force, in field-name order. Empty when the load changed
    /// nothing the daemon could act on immediately.
    pub applied: Vec<String>,
    /// Fields the app picks up at its next spawn, in field-name order. Empty
    /// when nothing is waiting.
    ///
    /// `shep reload <name>` promotes them; a client rendering this list says
    /// so, since a pending list with no remedy beside it cannot be acted on.
    pub pending: Vec<String>,
    /// Why some or all of this app's change did not land, in the daemon's own
    /// words, or `None` when the whole of it did.
    ///
    /// Not the same question as the two lists being empty: a refusal raised
    /// before anything was touched leaves both empty, and so does a load with
    /// nothing to do. It is a sentence rather than a code because the message
    /// is what tells them apart.
    pub refused: Option<String>,
}

impl SheepApplied {
    /// Builds one app's report.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        applied: Vec<String>,
        pending: Vec<String>,
        refused: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            applied,
            pending,
            refused,
        }
    }
}

/// One RPC response (pairs with [`Request`] variants)
///
/// Ten variants carry a bare `Vec<ProcessInfo>`. Do not collapse them into
/// one: each names which request it answers, which is what lets a variant
/// diverge without a protocol bump. `Reloading` already means an acceptance
/// rather than a result, `Scaled` only the survivors of a scale-down, and
/// `Mustered` every sheep of every restored app rather than what this call
/// started.
// wire format: changing existing variants is a breaking change.
// `large_enum_variant` allowed, not fixed: clippy's remedy is to box
// `DogStarted`'s payload, a source break for every
// `Response::DogStarted(info)` in and out of this workspace.
#[allow(clippy::large_enum_variant)]
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
    /// Answer to `Add`: one row per app the request named, registered and
    /// spawning nothing.
    ///
    /// A row here can still be `Online`: `Add` is idempotent by name, so the
    /// reply describes the membership the request leaves behind.
    Added(Vec<ProcessInfo>),
    /// Answer to `ConfigDrift`: one entry per app that is registered under a
    /// config different from the one asked about, and no entry for anything
    /// else. An empty vector means every app asked about either matches or
    /// is not registered at all.
    Drifted(Vec<SheepDrift>),
    /// Answer to `ApplyConfig`: one entry per app the request named, in the
    /// order it named them, the refused and the unchanged included.
    ///
    /// Complete where [`Self::Drifted`] is filtered: an app missing from
    /// "what did you do to each of these" looks like one the daemon dropped.
    Applied(Vec<SheepApplied>),
    /// Answer to `Stop`
    Stopped(Vec<ProcessInfo>),
    /// Answer to `Restart`
    Restarted(Vec<ProcessInfo>),
    /// Answer to `Reload`: an acceptance, not a result.
    ///
    /// One instance costs a readiness wait plus a drain, so a clustered app
    /// outlasts any deadline a client may ask for. The daemon answers as soon
    /// as the reload is accepted, with the matched sheep as they stood then,
    /// and the swaps report themselves on the bus (`process.reload`,
    /// `process.reloaded`, `process.reload_abandoned`). A matched sheep with
    /// nothing to replace is listed as the no-op success it is.
    Reloading(Vec<ProcessInfo>),
    /// Answer to `Scale`: the app's instances that will remain, one row each,
    /// ordered by [`sort_flock`]. Every row shares one name, so that is slot
    /// order with the id breaking a tie.
    ///
    /// Scaling down, the departing instances are absent even though their
    /// kill ladders are still running; they report themselves on the bus as
    /// `process.delete`.
    Scaled(Vec<ProcessInfo>),
    /// Answer to `SetSmit`: every instance of the named sheep, one row each,
    /// carrying the smit as it now stands.
    SmitPainted(Vec<ProcessInfo>),
    /// Answer to `Delete`: ids removed
    Deleted(Vec<u32>),
    /// Answer to `Reopen`: every matched sheep, running or not. A sheep with
    /// no live log pump has nothing to reopen and is reported as a success,
    /// so this carries the same matches `Describe` would.
    Reopened(Vec<ProcessInfo>),
    /// Answer to `Flush`: one row per matched sheep, running or not, exactly
    /// as [`Self::Reopened`].
    ///
    /// One row per sheep, not per file emptied: several sheep can share one
    /// log path, and the daemon truncates each distinct path once.
    Flushed(Vec<ProcessInfo>),
    /// Answer to `Trigger`: one [`ActionReply`] row per matched sheep, rather
    /// than a flock listing, since `ProcessInfo` has nowhere to hold a reply
    /// body.
    Triggered(Vec<ActionReply>),
    /// Answer to `Signal`: one [`SignalReply`] row per matched sheep.
    ///
    /// Not a flock listing: [`ProcessInfo`] has nowhere to hold a per-sheep
    /// outcome.
    Signalled(Vec<SignalReply>),
    /// Answer to `SendLine`: one [`LineReply`] row per matched sheep.
    SentLine(Vec<LineReply>),
    /// Answer to `SaveRoll`
    RollSaved {
        /// Absolute path of the roll the daemon wrote
        path: String,
        /// How many apps that roll records
        apps: u32,
    },
    /// Answer to `Muster`: every sheep of every app the roll restored, not
    /// only the ones this call spawned.
    ///
    /// Assembling a flock that is already assembled starts nothing, so a
    /// listing of what this call spawned would be indistinguishable from an
    /// empty roll.
    Mustered(Vec<ProcessInfo>),
    /// Answer to `DogConfig`: the dog's own section, rendered back to TOML.
    ///
    /// `toml` is [`DogSectionToml`], whose manual `Debug` keeps the webhook
    /// credentials this text carries out of a `{:?}`-formatted `Response`.
    DogSection {
        /// The `[dog.<name>]` table as TOML text, empty when the file has
        /// no such section
        toml: DogSectionToml,
    },
    /// Answer to `EnableDog`: the dog as it stands now
    DogStarted(ProcessInfo),
    /// Answer to `DogStaleness`: this daemon's own handshake record, split
    /// into the dogs it has given up on and the dogs it is still waiting on.
    ///
    /// Two lists because they answer two questions. `stale` is a finding;
    /// `pending` is a reason to ask again, since a reading taken now would
    /// be a guess about them.
    ///
    /// Names only: two builds differing only in the protocol they speak
    /// report the same crate version.
    DogStaleness {
        /// Dogs this daemon has refused twice: once on the handshake that
        /// bought them a restart from disk, and again after it. It will not
        /// restart them a third time.
        stale: Vec<String>,
        /// Dogs this daemon is still waiting to hear a final answer from: one
        /// whose restart is in flight, or one it supervises that has not
        /// handshook yet. Neither stale nor known healthy.
        pending: Vec<String>,
    },
    /// Answer to `HandoverFitness`: `None` when the whole flock can be
    /// carried across a daemon handover, and otherwise the sentence saying
    /// which sheep cannot be and why.
    ///
    /// A rendered sentence rather than a structured reason: the set of things
    /// a handover cannot carry keeps changing, and the client only prints it.
    HandoverFitness {
        /// Why the flock cannot be handed over in place, or `None` when it
        /// can.
        refusal: Option<String>,
    },
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
/// `result` uses serde's stock `Result` representation: the wire carries
/// `{"Ok": ...}` / `{"Err": ...}`, with capitalized keys, pinned by snapshot.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// Echoes [`Envelope::id`]
    pub id: u64,
    /// The outcome
    pub result: Result<Response, RpcError>,
}

/// Handshake outcome: `HelloAck` or a typed refusal, since version skew is
/// an error rather than silence. Same `Ok`/`Err` wire shape as
/// [`Reply::result`]; refusals use [`RpcErrorCode::ProtocolMismatch`].
pub type HelloReply = Result<HelloAck, RpcError>;

/// Structured RPC failure
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// Machine-readable code
    pub code: RpcErrorCode,
    /// Human-readable message (plain English, no theme)
    pub message: String,
    /// The daemon's own crate version, when it chose to name it.
    ///
    /// Set on a [`RpcErrorCode::ProtocolMismatch`] refusal, the only place a
    /// client can learn it, since [`HelloAck::daemon_version`] never arrives
    /// there. `None` on every other error, and on a refusal from a daemon
    /// built before the field existed, so a reader treats `None` as unknown
    /// and takes the conservative path.
    ///
    /// Absent on the wire rather than `null`, so
    /// [`crate::protocol::PROTOCOL_VERSION`] does not move for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
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
    /// `#[non_exhaustive]` forces a `_` arm on any match written outside this
    /// crate, which would swallow a variant added here and never updated
    /// there (`crates/shep-cli/src/exit.rs` maps every code to an exit
    /// status).
    pub const ALL: [Self; 6] = [
        Self::NotFound,
        Self::InvalidConfig,
        Self::SpawnFailed,
        Self::ProtocolMismatch,
        Self::Internal,
        Self::DeadlineExceeded,
    ];

    /// Never called; exists so this crate fails to build if a variant is
    /// added to [`RpcErrorCode`] without also being added to [`Self::ALL`].
    ///
    /// A match here is still checked for exhaustiveness, and each arm indexes
    /// a fixed literal position into [`Self::ALL`], so growing the enum
    /// without growing the array is an out-of-bounds constant index.
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
            out_file: Some("/home/ada/.shep/logs/web-0-out.log".to_string()),
            err_file: Some("/home/ada/.shep/logs/web-0-err.log".to_string()),
            // 12.5: an insta JSON snapshot is stable across platforms only
            // for a float the binary representation holds exactly.
            cpu_percent: Some(12.5),
            memory_bytes: Some(48 * 1024 * 1024),
            dog: None,
            lambs: None,
            last_exit: Some(ExitInfo {
                code: Some(1),
                signal: None,
            }),
            smit: None,
            instance: None,
            handshook: None,
            dog_stale: None,
            // Left at the builder's default: this fixture feeds
            // `reply_wire_snapshots` and `bus_event_wire_snapshots`, so a
            // `Some(..)` moves pinned bytes.
            pending: None,
            overridden: None,
        }
    }

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

    /// Every field is given a value distinct from every other field's
    /// default, so a copy-pasted setter body shows up as a mismatch.
    #[test]
    fn every_setter_writes_its_own_field_and_no_other() {
        let built = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .restarts(1)
            .uptime_ms(60_000)
            .fold(Some("backend".to_string()))
            .out_file(Some("/home/ada/.shep/logs/web-0-out.log".to_string()))
            .err_file(Some("/home/ada/.shep/logs/web-0-err.log".to_string()))
            .cpu_percent(Some(12.5))
            .memory_bytes(Some(48 * 1024 * 1024))
            .dog(None)
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();

        // `sample_info()` is a struct literal on purpose: it is the one
        // place that names every field by hand, so this comparison fails the
        // day the struct grows a field the builder cannot set.
        assert_eq!(built, sample_info());

        // `sample_info()`'s `dog` is `None`, the builder's default too, so an
        // empty `dog` setter body would pass the comparison above. It cannot
        // be changed: it feeds pinned snapshots.
        assert_eq!(
            ProcessInfo::builder(1, "metrics", ProcStatus::Online)
                .dog(Some(DogSource::BuiltIn))
                .build()
                .dog,
            Some(DogSource::BuiltIn),
            "an empty `dog` setter body is invisible to the comparison above"
        );

        // `lambs`, on `dog`'s terms above.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .lambs(Some(vec![Lamb::new(4243, "node")]))
                .build()
                .lambs,
            Some(vec![Lamb::new(4243, "node")]),
            "an empty `lambs` setter body is invisible to the comparison above"
        );

        // `smit`, on the same terms, and the field a third party writes: an
        // empty setter body drops every dog's mark.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .smit(Some("\u{25b2} main@a1b2c3".to_string()))
                .build()
                .smit
                .as_deref(),
            Some("\u{25b2} main@a1b2c3"),
            "an empty `smit` setter body is invisible to the comparison above"
        );

        // `handshook`, on the same terms.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .handshook(Some(false))
                .build()
                .handshook,
            Some(false),
            "an empty `handshook` setter body is invisible to the comparison above"
        );

        // `dog_stale`, paired with `handshook`: both default to `None`.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .dog_stale(Some(true))
                .build()
                .dog_stale,
            Some(true),
            "an empty `dog_stale` setter body is invisible to the comparison above"
        );

        // `pending`, on the same terms.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .pending(Some(vec!["env".to_string()]))
                .build()
                .pending,
            Some(vec!["env".to_string()]),
            "an empty `pending` setter body is invisible to the comparison above"
        );

        // `overridden`, on the same terms.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .overridden(Some(vec!["cwd".to_string()]))
                .build()
                .overridden,
            Some(vec!["cwd".to_string()]),
            "an empty `overridden` setter body is invisible to the comparison above"
        );
    }

    #[test]
    fn lambs_distinguishes_not_walked_from_walked_and_empty() {
        let not_walked = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
        assert_eq!(not_walked.lambs, None);

        let walked_empty = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .lambs(Some(Vec::new()))
            .build();
        assert_eq!(walked_empty.lambs, Some(Vec::new()));
    }

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

    /// argv holds credentials (`--password=`, `?token=`) and
    /// `shep describe --format json` is output people paste into issues.
    #[test]
    fn a_lamb_is_a_pid_and_an_executable_name() {
        let lamb = Lamb::new(4243, "node");
        let json = serde_json::to_string(&lamb).unwrap();
        assert_eq!(json, r#"{"pid":4243,"name":"node"}"#);
        assert_eq!(serde_json::from_str::<Lamb>(&json).unwrap(), lamb);
    }

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

    #[test]
    fn v1_process_info_without_a_dog_marker_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log","cpu_percent":12.5,"memory_bytes":50331648}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.dog, None);
    }

    /// No field here carries `#[serde(default)]`: serde's derive resolves a
    /// missing key to `None` for a field whose type is syntactically
    /// `Option<...>`.
    #[test]
    fn a_process_info_without_a_last_exit_key_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log","cpu_percent":12.5,"memory_bytes":50331648,"dog":null,"lambs":null}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.last_exit, None);
    }

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
        // The three tags, spelled out: a variant renamed in Rust changes
        // these strings, compiles clean, and breaks a client matching on them.
        assert!(json.contains(r#""kind":"delivered""#), "{json}");
        assert!(json.contains(r#""kind":"not_running""#), "{json}");
        assert!(json.contains(r#""kind":"failed""#), "{json}");
    }

    /// `instances` is a per-app number, so `shep stock /web.*/ 4` could mean
    /// four each or four total.
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
        // No `selector` key at all: this verb is not one of the
        // selector-taking family.
        assert!(!json.contains("selector"), "{json}");
    }

    #[test]
    fn a_scaled_reply_carries_its_own_tag() {
        let json = serde_json::to_string(&Response::Scaled(vec![])).unwrap();
        assert_eq!(json, r#"{"kind":"scaled","data":[]}"#);
    }

    /// `Add` and `Start` carry byte-identical payloads and differ by their
    /// `kind` alone.
    #[test]
    fn an_add_request_and_its_reply_round_trip() {
        let request = Request::Add {
            apps: vec![AppConfig::minimal("web", "./srv")],
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        assert!(json.contains(r#""kind":"add""#), "{json}");

        let reply = Response::Added(vec![]);
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), reply);
        assert!(json.contains(r#""kind":"added""#), "{json}");
    }

    /// `NotWritten`'s reason is the only thing separating "the app is not
    /// reading its stdin" from "the pipe broke".
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

    #[test]
    fn a_line_carrying_a_newline_is_still_one_field_on_the_wire() {
        let request = Request::SendLine {
            selector: SelectorSpec::All,
            line: "a\nb".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        // Escaped, not literal: the frame stays one JSON object. Refusing
        // it is the daemon's job, not serde's.
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
            // `All` rather than a named sheep: the selector `shep reopen`
            // sends when given no argument.
            Envelope {
                id: 5,
                deadline_ms: None,
                body: Request::Reopen {
                    selector: SelectorSpec::All,
                },
            },
            // The same selector as the row above, so the two log-plane rows
            // differ by their `kind` and by nothing else.
            Envelope {
                id: 6,
                deadline_ms: None,
                body: Request::Flush {
                    selector: SelectorSpec::All,
                },
            },
            // The same selector as the `stop` row: `reload` under `stop`'s tag
            // shows up here as two identical objects.
            Envelope {
                id: 7,
                deadline_ms: None,
                body: Request::Reload {
                    selector: SelectorSpec::Name("web".to_string()),
                },
            },
            // `action`/`params` match channel.rs's with-params fixture
            // verbatim, so a trigger reads the same at every hop.
            Envelope {
                id: 8,
                deadline_ms: None,
                body: Request::Trigger {
                    selector: SelectorSpec::Name("web".to_string()),
                    action: "set-log-level".to_string(),
                    params: Some("debug".to_string()),
                },
            },
            // A fieldless verb: a bare `{"kind":"..."}` with no `selector` key.
            Envelope {
                id: 9,
                deadline_ms: None,
                body: Request::SaveRoll,
            },
            // Paired with the `save_roll` row: they differ by their `kind` alone.
            Envelope {
                id: 10,
                deadline_ms: None,
                body: Request::Muster,
            },
            // The three dog verbs. `enable_dog` and `disable_dog` differ by
            // their `kind` and by `source` alone.
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
            // `Id`, `Regex` and `Fold` are three newtypes the wire tells apart
            // only by their `kind` tag: a `Fold` under `regex`'s tag turns
            // `shep restart fold:api` into a regex match.
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
            // `SIGHUP` rather than `SIGTERM`: the stop ladder already sends
            // TERM, so a TERM fixture could not tell the two frames apart.
            Envelope {
                id: 17,
                deadline_ms: None,
                body: Request::Signal {
                    selector: SelectorSpec::Name("web".to_string()),
                    signal: "SIGHUP".to_string(),
                },
            },
            // The one verb here whose body has no `selector` key.
            Envelope {
                id: 18,
                deadline_ms: None,
                body: Request::Scale {
                    name: "web".to_string(),
                    count: 4,
                },
            },
            // The line carries no terminator on the wire, since the shepherd
            // appends it.
            Envelope {
                id: 19,
                deadline_ms: None,
                body: Request::SendLine {
                    selector: SelectorSpec::All,
                    line: "reload-config".to_string(),
                },
            },
            // Both halves of the `Option` are pinned, a paint and a clear, so a
            // dog author does not have to guess the clear frame's shape.
            Envelope {
                id: 20,
                deadline_ms: None,
                body: Request::SetSmit {
                    sheep: "web".to_string(),
                    smit: Some(
                        "\u{25b2} main@a1b2c3"
                            .parse()
                            .expect("the reference smit is valid"),
                    ),
                },
            },
            Envelope {
                id: 21,
                deadline_ms: None,
                body: Request::SetSmit {
                    sheep: "web".to_string(),
                    smit: None,
                },
            },
            // An empty `apps`: `start`'s row already pins the payload type, so
            // this row's own are the tag and the key the list travels under.
            Envelope {
                id: 22,
                deadline_ms: None,
                body: Request::ConfigDrift { apps: Vec::new() },
            },
            // The only struct-shaped `SelectorSpec` variant, so the only place
            // `"kind":"instance"` and the `slot` key are pinned.
            Envelope {
                id: 23,
                deadline_ms: None,
                body: Request::Restart {
                    selector: SelectorSpec::Instance {
                        name: "web".to_string(),
                        slot: 2,
                    },
                },
            },
            // The one request an older daemon must never be sent: shep-cli
            // gates it on the daemon's crate version.
            Envelope {
                id: 24,
                deadline_ms: None,
                body: Request::HandoverFitness,
            },
            // The second request gated on the daemon's crate version.
            Envelope {
                id: 25,
                deadline_ms: None,
                body: Request::DogStaleness,
            },
            // The only request carrying a `DeclaredApp`: a merge keys on what a
            // document claimed. `declared_env` is non-empty to show it holds
            // env key names and no env value, and `reset` is pinned at a
            // non-default depth.
            Envelope {
                id: 26,
                deadline_ms: None,
                body: Request::ApplyConfig {
                    apps: vec![DeclaredApp {
                        config: AppConfig::minimal("web", "./srv"),
                        declared: ["name", "script"]
                            .iter()
                            .map(|k| (*k).to_string())
                            .collect(),
                        declared_env: ["DATABASE_URL"].iter().map(|k| (*k).to_string()).collect(),
                    }],
                    reset: ResetDepth::Policy,
                },
            },
            // The same app as the `start` row above: the two differ by their
            // `kind` alone, so a mis-tagged `add` shows up as two identical
            // objects.
            Envelope {
                id: 27,
                deadline_ms: None,
                body: Request::Add {
                    apps: vec![AppConfig::minimal("web", "./srv")],
                },
            },
        ];
        insta::assert_json_snapshot!("request_wire_v3", requests);
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
                    daemon_version: None,
                }),
            },
            // `ActionReply` is not a `ProcessInfo`. `Replied` is the
            // struct-shaped `ActionOutcome` variant and so the one worth
            // pinning.
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
            // The only struct-shaped `Response` variant; every other one is
            // a newtype over a `Vec` or a unit, both proven above.
            Reply {
                id: 5,
                result: Ok(Response::RollSaved {
                    path: "/home/ada/.shep/flock.json".to_string(),
                    apps: 2,
                }),
            },
            // The present `dog` marker; `sample_info()` pins the absent one.
            // `Adopted` because it is the variant carrying a payload.
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
            // The section crosses the wire as text, never a typed structure.
            Reply {
                id: 7,
                result: Ok(Response::DogSection {
                    toml: "port = 9615\n".to_string().into(),
                }),
            },
            // The only `Response` variant carrying a bare `ProcessInfo`
            // rather than a `Vec`: `enable` starts exactly one dog.
            Reply {
                id: 8,
                result: Ok(Response::DogStarted(ProcessInfo {
                    id: 4,
                    name: "metrics".to_string(),
                    dog: Some(DogSource::BuiltIn),
                    ..sample_info()
                })),
            },
            // Each row below carries the smallest body that shows its wire
            // shape: the tag is what is being pinned. `Deleted` is a
            // `Vec<u32>`; `Subscribed` and `ShuttingDown` carry nothing.
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
            // `Signalled`, mirroring the `Triggered` row: one row per
            // `SignalOutcome` variant, so no tag is left unproven.
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
            // `SentLine`, mirroring the `Signalled` row: one row per
            // `LineOutcome` variant.
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
            // A walked lamb tree; every other row pins the `null` shape.
            Reply {
                id: 23,
                result: Ok(Response::Described(vec![
                    ProcessInfo::builder(3, "web", ProcStatus::Online)
                        .pid(Some(4242))
                        .lambs(Some(vec![Lamb::new(4243, "node"), Lamb::new(4244, "sh")]))
                        .build(),
                ])),
            },
            // The killed-by-signal shape of `last_exit`; every row above pins
            // the exited-normally one. `SIGTERM`'s raw number, since this
            // crate carries no name for it.
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
            // The one row that pins a smit on the wire; `sample_info()` carries
            // none.
            Reply {
                id: 25,
                result: Ok(Response::SmitPainted(vec![
                    ProcessInfo::builder(3, "web", ProcStatus::Online)
                        .pid(Some(4242))
                        .smit(Some("\u{25b2} main@a1b2c3".to_string()))
                        .build(),
                ])),
            },
            // A sheep drifting in one field and a sheep drifting in several.
            // `env` is one of them: the name travels and the value never does.
            Reply {
                id: 26,
                result: Ok(Response::Drifted(vec![
                    SheepDrift::new("web", vec!["cwd".to_string()]),
                    SheepDrift::new(
                        "api",
                        vec!["args".to_string(), "env".to_string(), "script".to_string()],
                    ),
                ])),
            },
            // The present shape of `instance`; every row above pins its absence.
            Reply {
                id: 27,
                result: Ok(Response::Flock(vec![
                    ProcessInfo::builder(9, "web", ProcStatus::Online)
                        .pid(Some(5150))
                        .instance(Some(2))
                        .build(),
                ])),
            },
            // Both shapes of the handover answer; the difference between them
            // is a `null`.
            Reply {
                id: 28,
                result: Ok(Response::HandoverFitness { refusal: None }),
            },
            Reply {
                id: 29,
                result: Ok(Response::HandoverFitness {
                    refusal: Some("sheep 'web' has a shepherd channel".to_string()),
                }),
            },
            // Both lists non-empty and different: the two carry the same wire
            // shape.
            Reply {
                id: 30,
                result: Ok(Response::DogStaleness {
                    stale: vec!["metrics".to_string()],
                    pending: vec!["bark".to_string()],
                }),
            },
            // A dog whose process is up and which has never answered this
            // shepherd. `dog_stale: false` is the silence still being waited
            // out; the row below is the one it has given up on.
            Reply {
                id: 31,
                result: Ok(Response::Flock(vec![
                    ProcessInfo::builder(10, "log-rotate", ProcStatus::Online)
                        .pid(Some(208_341))
                        .dog(Some(DogSource::Adopted {
                            path: "/usr/local/bin/shep-log-rotate".to_string(),
                        }))
                        .handshook(Some(false))
                        .dog_stale(Some(false))
                        .build(),
                ])),
            },
            Reply {
                id: 32,
                result: Ok(Response::Flock(vec![
                    ProcessInfo::builder(10, "log-rotate", ProcStatus::Online)
                        .pid(Some(208_341))
                        .dog(Some(DogSource::Adopted {
                            path: "/usr/local/bin/shep-log-rotate".to_string(),
                        }))
                        .handshook(Some(false))
                        .dog_stale(Some(true))
                        .build(),
                ])),
            },
            // Three entries, one per shape a load produces: applied, pending,
            // refused. `env` is a pending name on purpose: the name travels
            // and the value never does.
            Reply {
                id: 32,
                result: Ok(Response::Applied(vec![
                    SheepApplied::new("web", vec!["max_memory".to_string()], Vec::new(), None),
                    SheepApplied::new(
                        "api",
                        Vec::new(),
                        vec!["args".to_string(), "env".to_string()],
                        None,
                    ),
                    SheepApplied::new(
                        "worker",
                        Vec::new(),
                        Vec::new(),
                        Some("worker is not registered".to_string()),
                    ),
                ])),
            },
            // `Added`'s tag, all a fixture can prove for a `Vec<ProcessInfo>`
            // variant. Down here because every id in this vector is
            // hand-written.
            Reply {
                id: 33,
                result: Ok(Response::Added(vec![])),
            },
        ];
        insta::assert_json_snapshot!("reply_wire_v3", replies);
    }

    /// Asserts on the JSON, not the struct: a `Vec<String>` cannot say which
    /// of the two a string is, so a build carrying a value would typecheck.
    #[test]
    fn a_sheep_applied_carries_names_and_never_values() {
        let applied = SheepApplied::new(
            "web",
            vec!["cwd".to_string()],
            vec!["env".to_string()],
            None,
        );
        let json = serde_json::to_string(&applied).unwrap();
        assert!(json.contains("\"env\""), "the NAME travels: {json}");
        assert!(
            !json.contains("DATABASE_URL"),
            "and no value ever does: {json}"
        );
    }

    #[test]
    fn a_sheep_applied_debug_prints_the_names_it_was_given() {
        let applied = SheepApplied::new("web", vec!["cwd".to_string()], Vec::new(), None);
        assert_eq!(
            format!("{applied:?}"),
            "SheepApplied { name: \"web\", applied: [\"cwd\"], pending: [], refused: None }"
        );
    }

    #[test]
    fn a_process_info_without_a_smit_key_still_deserializes() {
        let fixture = r#"{"id":1,"name":"web","status":"online","pid":42,"restarts":0,"uptime_ms":10,"fold":null,"out_file":null,"err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":null,"lambs":null,"last_exit":null}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.smit, None);
    }

    /// The fixture is a dog's row, where `None` means "render this as it
    /// rendered before the field existed", never "never handshaken".
    #[test]
    fn a_process_info_without_a_handshook_key_still_deserializes() {
        let fixture = r#"{"id":1,"name":"metrics","status":"online","pid":42,"restarts":0,"uptime_ms":10,"fold":null,"out_file":null,"err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":{"kind":"built_in"},"lambs":null,"last_exit":null,"smit":null,"instance":0}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.handshook, None);
        assert_eq!(info.dog, Some(DogSource::BuiltIn));
    }

    /// The fixture carries `handshook: false`, the case that matters: `None`
    /// is "no verdict to report", never "it has not given up".
    #[test]
    fn a_process_info_without_a_dog_stale_key_still_deserializes() {
        let fixture = r#"{"id":1,"name":"metrics","status":"online","pid":42,"restarts":0,"uptime_ms":10,"fold":null,"out_file":null,"err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":{"kind":"built_in"},"lambs":null,"last_exit":null,"smit":null,"instance":0,"handshook":false}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.dog_stale, None);
        assert_eq!(info.handshook, Some(false));
    }

    /// A dog written in another language speaks this wire directly and never
    /// runs `FromStr`.
    #[test]
    fn a_smit_is_validated_when_it_is_deserialized_not_only_when_parsed() {
        for bad in [
            r#""\u001b[2Jgone""#.to_string(),                    // an escape
            r#""a\nb""#.to_string(),                             // a newline
            r#""""#.to_string(),                                 // empty
            r#""   ""#.to_string(),                              // whitespace
            format!(r#""{}""#, "x".repeat(Smit::MAX_CHARS + 1)), // too long
        ] {
            assert!(
                serde_json::from_str::<Smit>(&bad).is_err(),
                "a daemon must refuse this on the wire: {bad}"
            );
        }
        assert!(serde_json::from_str::<Smit>(r#""\u25b2 main@a1b2c3""#).is_ok());
    }

    /// The hand-written `Deserialize` agrees with the derived `Serialize`
    /// only while the serialize side stays transparent.
    #[test]
    fn a_smit_travels_as_a_bare_string() {
        let smit: Smit = "\u{25b2} main@a1b2c3".parse().expect("valid");
        let json = serde_json::to_string(&smit).unwrap();
        assert_eq!(json, "\"\u{25b2} main@a1b2c3\"");
        assert_eq!(serde_json::from_str::<Smit>(&json).unwrap(), smit);
    }

    #[test]
    fn a_smit_is_capped_in_characters_not_bytes() {
        let cjk = "\u{7f8a}".repeat(Smit::MAX_CHARS);
        assert_eq!(cjk.len(), Smit::MAX_CHARS * 3);
        assert!(cjk.parse::<Smit>().is_ok(), "{cjk}");
        assert_eq!(
            "x".repeat(Smit::MAX_CHARS + 1).parse::<Smit>(),
            Err(SmitError::TooLong {
                chars: Smit::MAX_CHARS + 1
            })
        );
    }

    #[test]
    fn a_smit_is_stored_exactly_as_it_arrived() {
        let padded: Smit = "  main@a1b2c3  ".parse().expect("valid");
        assert_eq!(padded.as_str(), "  main@a1b2c3  ");
        assert_eq!(padded.to_string(), "  main@a1b2c3  ");
    }

    #[test]
    fn v1_fixture_still_deserializes() {
        // Committed byte fixture from protocol v1. If this breaks, bump
        // PROTOCOL_VERSION and record it in the CHANGELOG.
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
            dog_name: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"client_version":"0.1.0","protocol":3}"#);
    }

    #[test]
    fn a_dogs_hello_names_the_dog_and_nothing_elses_does() {
        let dog = Hello {
            client_version: "0.1.0".to_string(),
            protocol: PROTOCOL_VERSION,
            dog_name: Some("metrics".to_string()),
        };
        let json = serde_json::to_string(&dog).unwrap();
        assert_eq!(
            json,
            r#"{"client_version":"0.1.0","protocol":3,"dog_name":"metrics"}"#
        );
        assert_eq!(serde_json::from_str::<Hello>(&json).unwrap(), dog);
    }

    /// `Hello` is the version-negotiation frame, so `deny_unknown_fields`
    /// here would refuse a newer client before `protocol` is read, leaving
    /// neither peer able to report the skew.
    #[test]
    fn a_hello_without_a_dog_name_still_parses() {
        let fixture = r#"{"client_version":"0.1.14","protocol":2}"#;
        let hello: Hello = serde_json::from_str(fixture).unwrap();
        assert_eq!(hello.protocol, 2);
        assert_eq!(hello.dog_name, None);

        // The other direction: an older daemon ignores a key it does not
        // know. `unknown_to_an_older_daemon` stands in for `dog_name`.
        let newer = r#"{"client_version":"9.9.9","protocol":2,"dog_name":"metrics","unknown_to_an_older_daemon":true}"#;
        let hello: Hello = serde_json::from_str(newer).unwrap();
        assert_eq!(hello.protocol, 2);
        assert_eq!(hello.dog_name.as_deref(), Some("metrics"));
    }

    #[test]
    fn hello_reply_carries_typed_skew_error() {
        let refusal: HelloReply = Err(RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 1, client sent 2".to_string(),
            daemon_version: None,
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
        // Committed byte fixture, protocol v1.
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
    fn v1_process_info_without_stats_still_deserializes() {
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend","out_file":"/l/o.log","err_file":"/l/e.log"}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.cpu_percent, None);
        assert_eq!(info.memory_bytes, None);
    }

    #[test]
    fn v1_process_info_without_log_paths_still_deserializes() {
        // Committed byte fixture from before `out_file`/`err_file` existed.
        let fixture = r#"{"id":3,"name":"web","status":"online","pid":4242,"restarts":1,"uptime_ms":60000,"fold":"backend"}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.id, 3);
        assert_eq!(info.out_file, None);
        assert_eq!(info.err_file, None);
    }

    #[test]
    fn an_old_client_still_decodes_a_new_process_info() {
        // `ProcessInfo` carries no `deny_unknown_fields`, unlike the config
        // types in `crate::config`, so extra keys are ignored.
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
    fn an_rpc_error_without_a_daemon_version_serializes_exactly_as_before() {
        // `skip_serializing_if` is what makes the field free: no
        // `"daemon_version":null` key for an older client to ignore.
        let plain = RpcError {
            code: RpcErrorCode::NotFound,
            message: "no sheep".to_string(),
            daemon_version: None,
        };
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"code":"not_found","message":"no sheep"}"#
        );
    }

    #[test]
    fn a_v1_rpc_error_fixture_deserializes_with_no_daemon_version() {
        let fixture =
            r#"{"code":"protocol_mismatch","message":"daemon speaks protocol 1, client sent 2"}"#;
        let err: RpcError = serde_json::from_str(fixture).unwrap();
        assert_eq!(err.code, RpcErrorCode::ProtocolMismatch);
        assert_eq!(err.daemon_version, None);
    }

    #[test]
    fn an_old_client_ignores_an_rpc_error_field_it_has_never_seen() {
        // `RpcError` carries no `deny_unknown_fields`, so an optional field
        // may be added without moving `PROTOCOL_VERSION`.
        #[derive(Deserialize)]
        struct OldRpcError {
            code: RpcErrorCode,
            message: String,
        }

        let current = serde_json::to_string(&RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 1, client sent 2".to_string(),
            daemon_version: Some("0.1.16".to_string()),
        })
        .unwrap();
        let old: OldRpcError = serde_json::from_str(&current).expect("must tolerate");
        assert_eq!(old.code, RpcErrorCode::ProtocolMismatch);
        assert_eq!(old.message, "daemon speaks protocol 1, client sent 2");
    }

    #[test]
    fn deadline_exceeded_code_serializes_snake_case() {
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
        // The shared snapshots exercise only `Replied`, the struct-shaped
        // variant.
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

    /// The listing is empty on purpose: `reply_wire_snapshots` pins the row
    /// field by field.
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
        // A dog's section routinely holds webhook credentials. Pinned as an
        // exact string, so a `#[derive(Debug)]` on `DogSectionToml` fails
        // here.
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

    /// The fixture cannot agree under either candidate order: by id it is
    /// `web/1, api/2, web/0`, by name `api, web, web`. The two `web` rows
    /// are the tiebreak half, seeded out of order.
    #[test]
    fn a_listing_sorts_by_name_then_by_id() {
        let mut listing = vec![
            ProcessInfo::builder(1, "web", ProcStatus::Online).build(),
            ProcessInfo::builder(2, "api", ProcStatus::Online).build(),
            ProcessInfo::builder(0, "web", ProcStatus::Online).build(),
        ];
        sort_flock(&mut listing);

        let seen: Vec<(&str, u32)> = listing
            .iter()
            .map(|info| (info.name.as_str(), info.id))
            .collect();
        assert_eq!(
            seen,
            vec![("api", 2), ("web", 0), ("web", 1)],
            "name first, then id inside a name"
        );
    }

    #[test]
    fn an_instance_slot_survives_a_round_trip_and_defaults_to_absent() {
        let with = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .instance(Some(2))
            .build();
        assert_eq!(with.instance, Some(2));

        let without = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
        assert_eq!(
            without.instance, None,
            "a row nobody set a slot on says so, rather than claiming slot 0"
        );
    }

    #[test]
    fn a_reply_from_a_daemon_without_the_field_deserializes_as_absent() {
        let json = r#"{"id":1,"name":"web","status":"online","pid":null,
            "restarts":0,"uptime_ms":0,"fold":null,"out_file":null,
            "err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":null,
            "lambs":null,"last_exit":null,"smit":null}"#;
        let info: ProcessInfo = serde_json::from_str(json).expect("older reply still parses");
        assert_eq!(info.instance, None);
    }

    #[test]
    fn sort_flock_orders_by_slot_before_id() {
        // A reload gave slot 0 a fresh, higher id. Slot order must still win.
        let mut listing = vec![
            ProcessInfo::builder(9, "web", ProcStatus::Online)
                .instance(Some(0))
                .build(),
            ProcessInfo::builder(2, "web", ProcStatus::Online)
                .instance(Some(1))
                .build(),
        ];
        sort_flock(&mut listing);
        assert_eq!(
            listing.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![9, 2],
            "slot 0 leads even though its id is higher"
        );
    }

    #[test]
    fn sort_flock_falls_back_to_id_when_no_row_carries_a_slot() {
        let mut listing = vec![
            ProcessInfo::builder(5, "web", ProcStatus::Online).build(),
            ProcessInfo::builder(3, "web", ProcStatus::Online).build(),
        ];
        sort_flock(&mut listing);
        assert_eq!(
            listing.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![3, 5],
            "an older daemon's listing sorts exactly as it does today"
        );
    }
}
