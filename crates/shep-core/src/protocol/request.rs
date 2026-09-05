//! RPC frames: requests, responses, envelopes, and structured errors

use core::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::config::{AppConfig, DeclaredApp, ResetDepth};
use crate::status::ProcStatus;

/// Client's opening frame
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Client crate version (semver string)
    pub client_version: String,
    /// [`crate::protocol::PROTOCOL_VERSION`] the client speaks
    pub protocol: u32,
    /// The name this client was registered under as a dog, when it is one.
    ///
    /// `None` is what every other client truthfully is, and what a dog built
    /// before this field existed sends. The CLI never sets it: a bare
    /// `Client` has no way to, by construction, so `shep stop` cannot
    /// impersonate a dog however its environment is set.
    ///
    /// The daemon needs it on exactly one path, and it is the path where
    /// nothing else can supply it. A handshake refused for protocol skew
    /// never reaches a request, so `Request::DogConfig`'s name — the one
    /// place a dog otherwise identifies itself — is unreachable precisely
    /// when the daemon has to know WHICH dog it just refused in order to
    /// restart it (the handover design's G8). A dog already knows its own
    /// name: the daemon put it in `$SHEP_DOG_NAME` when it spawned it.
    ///
    /// Additive by construction: absent on the wire rather than `null`, and
    /// ignored by a daemon too old to know it, so
    /// [`crate::protocol::PROTOCOL_VERSION`] does not move for it. That
    /// argument deserves more care here than anywhere else, because `Hello`
    /// IS the version-negotiation frame: a daemon that rejected unknown
    /// fields would refuse a newer client BEFORE reading `protocol`, and
    /// that would be a hard break rather than an additive change. It does
    /// not — this type carries no `#[serde(deny_unknown_fields)]`, and
    /// `a_hello_without_a_dog_name_still_parses` pins both directions.
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
    // Both field names are part of the wire contract, and the byte shape is
    // pinned by `request_wire_v3`, so renaming either breaks that snapshot
    // rather than sliding through unnoticed.
    /// By app name and instance slot
    ///
    /// Added in protocol 2, and the reason that version exists: a daemon
    /// built against protocol 1 has no such variant and refuses a client
    /// that sends one at the handshake, rather than failing to decode it
    /// later. On the wire this is `{"kind":"instance","value":{"name":
    /// "web","slot":2}}`, following the enum's own `kind`/`value` tagging.
    Instance {
        /// The app name
        name: String,
        /// The instance slot, counting from 0
        slot: u32,
    },
}

/// A short marker a dog attaches to a sheep for `shep flock` to paint.
///
/// shep stores one and prints it. It does not parse it, has no opinion about
/// what it means, and never will: `▲ main@a1b2c3` is a deploy tool's
/// sentence, not shep's, and keeping it that way is what makes this a general
/// mechanism rather than one feature's field.
///
/// # The rule
///
/// A smit is non-empty once whitespace is discounted, at most
/// [`Self::MAX_CHARS`] characters, and carries no [`char::is_control`]
/// character — `\u{1b}` included, which `is_control` already covers and which
/// is named separately in this doc anyway, because it is the one an attacker
/// reaches for and a reader should not have to know the classification to see
/// that it is handled.
///
/// Refused, never repaired. The text is stored exactly as it arrived: shep
/// does not trim it, strip from it, or otherwise hand back something the
/// publisher did not send. `crate::kv`'s key grammar and value cap set the
/// same precedent for the same kind of value, and the publisher here is a
/// program, so a refusal it can see and fix beats mangling it cannot.
///
/// # Why the cap counts characters
///
/// [`Self::MAX_CHARS`] is a count of `char`s, not of bytes and not of display
/// columns. Bytes would refuse a legitimate CJK smit at roughly a third of
/// its apparent length. Display columns are what a table renderer measures,
/// but they depend on the terminal and on a width table this parser has no
/// business carrying. Characters are the honest thing a validator can promise
/// cheaply. 48 is measured against the reference smit `▲ main@a1b2c3` at
/// thirteen: room for a long branch name, without letting one column swallow
/// the table.
///
/// # Why validation lives here rather than at the renderer
///
/// `shep`'s own `output::width::sanitize_cell` deliberately KEEPS a
/// well-formed CSI sequence, because shep's colouring is made of them, so it
/// is not a guard against a third party's string. Refusing here means `shep
/// flock`, `shep describe`, `--format json`, the lookout, the MCP tool schema
/// and every bus subscriber are safe by construction instead of six places
/// each remembering.
///
/// `Debug` is derived, and that is the deliberate decision (IR-41): a smit
/// carries no environment and no secret. It is a string a dog asked to have
/// painted in public, so redacting it would hide the thing an operator is
/// debugging.
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
    /// The longest a smit may be, in characters. See the type doc for why
    /// characters rather than bytes or display columns.
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
    /// - [`SmitError::Empty`] — nothing but whitespace.
    /// - [`SmitError::TooLong`] — over [`Self::MAX_CHARS`] characters.
    /// - [`SmitError::Unprintable`] — a control character, `\u{1b}` included.
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

/// Hand-written rather than derived, and that is the whole security property.
///
/// A derived impl would accept anything a `String` accepts, so a smit
/// carrying `\u{1b}[2J` would reach the daemon's memory and every listing
/// built from it. `docs/dogs.md` tells dog authors to speak this wire
/// directly, so a dog written in another language never runs
/// [`core::str::FromStr`] — the daemon has to validate what it decodes, not
/// trust that it was constructed properly.
impl<'de> Deserialize<'de> for Smit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // String, not &str: a non-borrowing deserializer cannot always borrow
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Why a string is not a [`Smit`].
///
/// `#[non_exhaustive]`: shep-core is a published library and a further
/// refusal — a grapheme-cluster cap, say, or a bidi-override rule — must not
/// break an out-of-tree consumer's `match` (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmitError {
    /// Over [`Smit::MAX_CHARS`] characters; carries the count that was sent.
    TooLong {
        /// How many characters the string held.
        chars: usize,
    },
    /// A control character, `\u{1b}` included — the sequence that would let a
    /// third party's string drive an operator's terminal.
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
        /// App configs — the daemon MUST re-normalize (peer input is
        /// untrusted); failures return [`RpcErrorCode::InvalidConfig`]
        apps: Vec<AppConfig>,
    },
    /// Register apps as flock members without starting any of them
    ///
    /// Everything [`Self::Start`] does to the flock's membership and none of
    /// what it does to processes: each app lands `Stopped`, holds no pid, and
    /// nothing is spawned. `shep add` is the verb.
    ///
    /// It exists because a Flockfile is a template. One shipping
    /// `env = { DB_PASSWORD = "" }` would otherwise have to be STARTED before
    /// it could be configured, and a process spawned against an empty
    /// database URL crashes, spends its restart budget, and has to be stopped
    /// before the operator can get anywhere near it.
    ///
    /// Idempotent by name, like the muster restore that shares its supervisor
    /// path: an app the flock already has is answered as it stands, running
    /// or not, and nothing about it changes. Config is a separate request:
    /// [`Self::ApplyConfig`] is what merges a template into an app the flock
    /// already has, and `shep add` sends both.
    ///
    /// Answers [`Response::Added`].
    Add {
        /// App configs, carried exactly as [`Self::Start`] carries them. The
        /// daemon MUST re-normalize (peer input is untrusted); failures
        /// return [`RpcErrorCode::InvalidConfig`]
        apps: Vec<AppConfig>,
    },
    /// Ask which of `apps` name a sheep the flock already has under a
    /// different config
    ///
    /// Read-only: nothing is registered, started, or changed. [`Self::Start`]
    /// on an already-registered name adds instances rather than reconciling
    /// config, which is what `shep stock` relies on; this is how a caller
    /// finds out that an edit it just read from a Flockfile is one `Start`
    /// will not apply, instead of the edit vanishing without a word.
    ///
    /// Answers [`Response::Drifted`] with one [`SheepDrift`] per app that is
    /// both registered and different. An app the flock does not have is
    /// absent from the answer, not reported as unchanged: `Start` will
    /// register it, so there is nothing to warn about.
    ConfigDrift {
        /// The configs to compare against, exactly as [`Self::Start`] would
        /// carry them. The daemon MUST re-normalize (peer input is
        /// untrusted, and an unnormalized config would report every default
        /// it has not spelled out as a difference); failures return
        /// [`RpcErrorCode::InvalidConfig`].
        apps: Vec<AppConfig>,
    },
    /// Merge each declared app into the sheep of the same name, applying
    /// what can be applied and parking the rest for that sheep's next spawn
    ///
    /// The acting half of [`Self::ConfigDrift`], which only reports. Nothing
    /// is registered, nothing is pruned and nothing running is killed: an app
    /// the flock does not have is refused by name rather than started, and a
    /// field the running child was spawned from waits for a `shep reload`
    /// instead of taking one.
    ///
    /// Additive by default, which is what `reset` exists to widen. A
    /// Flockfile arrives from the app's own repository, so a load appends
    /// what nobody has established and leaves everything an operator set
    /// since alone.
    ///
    /// Answers [`Response::Applied`] with one [`SheepApplied`] per entry in
    /// `apps`, in the order given, whether or not the app was found and
    /// whether or not anything changed. One app that cannot be applied does
    /// not cost the rest of the file its load; its refusal rides in
    /// [`SheepApplied::refused`].
    ApplyConfig {
        /// The apps to merge in, each carrying the keys its document
        /// literally wrote. The daemon MUST re-normalize the merge result
        /// (peer input is untrusted) and refuses the whole request with
        /// [`RpcErrorCode::InvalidConfig`] when two entries share a name:
        /// the second would be merged against a store the first has not
        /// written yet, so its record would be the one that survives.
        apps: Vec<DeclaredApp>,
        /// How much of what the operator has set since a template last
        /// loaded this request may overwrite. Default
        /// [`ResetDepth::None`], which overwrites nothing.
        ///
        /// `ResetDepth::Settings` was renamed to `ResetDepth::Policy` (and
        /// `File`/`Env` were added) in protocol 3, and the reason that
        /// version exists: unlike an added variant, a rename changes the
        /// wire spelling of an operation that was already shipping, so a
        /// daemon built against protocol 2 cannot decode a client sending
        /// the new name.
        reset: ResetDepth,
    },
    /// One sheep's effective config, for a pane that is about to edit it.
    ///
    /// `env` comes back emptied and its key names ride separately, so a
    /// value never crosses the wire (decision 12 of the overrides design).
    /// Read-only: nothing about the sheep changes.
    ///
    /// Answers [`Response::SheepConfig`], or
    /// [`RpcErrorCode::NotFound`] when no sheep has that name.
    SheepConfig {
        /// The sheep's name, not a selector: a pane edits one sheep, for
        /// the reason [`Self::Scale`] states at length.
        name: String,
    },
    /// Sets, replaces, or with `None` removes one env key on one sheep,
    /// recorded as an operator override. Never reads it back.
    ///
    /// Its own request rather than a [`Self::ApplyConfig`] depth, because
    /// no depth does this: `ResetDepth::None` appends only, `File` and
    /// `Policy` leave env alone, and `Env`/`All` replace the whole map with
    /// the template's. A pane cannot send the whole map, since it is never
    /// told the values it would have to send back.
    ///
    /// The running child holds the env it was spawned from, so the change
    /// parks for the next spawn exactly as `ApplyConfig` parks a
    /// respawn-only field, and `shep reload`/`shep restart` promote it.
    ///
    /// Answers [`Response::SheepEnvSet`], or
    /// [`RpcErrorCode::NotFound`] when no sheep has that name.
    SetSheepEnv {
        /// The sheep's name, not a selector, for [`Self::SheepConfig`]'s
        /// reason.
        name: String,
        /// The env key.
        key: String,
        /// The value, or `None` to remove the key.
        value: Option<String>,
    },
    /// Replaces one dog's `[<name>]` section in `dogs.toml` and publishes
    /// `config.dog.<name>` so a running dog re-reads it.
    ///
    /// The writing twin of [`Self::DogConfig`], which reads the same
    /// section.
    ///
    /// Answers [`Response::DogConfigSet`].
    SetDogConfig {
        /// The dog's name, the config key.
        name: String,
        /// The whole section, as TOML text.
        ///
        /// [`DogSectionToml`], not a bare `String`, for the reason that
        /// type's own doc gives: a section can hold a dog's credentials and
        /// this is what keeps them out of a `{:?}` (IR-41).
        toml: DogSectionToml,
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
    /// Attach a short marker to `sheep` for `shep flock` to paint, or clear
    /// it with `None`.
    ///
    /// By NAME rather than a selector, for [`Self::Scale`]'s reason (see its
    /// own doc above): a smit belongs to a sheep, not to one of its
    /// instances, and every instance of that name shows it — including one
    /// spawned after the smit was painted.
    ///
    /// Held in memory and scoped to the connection that sent it. When that
    /// connection closes, for any reason, the smits it painted go with it. A
    /// publisher therefore republishes rather than publishing on change.
    ///
    /// shep does not parse it and has no opinion about what it means.
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
    /// Ask which dogs this daemon has given up on, and which it is still
    /// waiting to hear from (`shep daemon reload`, the handover design's
    /// G13).
    ///
    /// Read-only, and about THIS daemon's own handshakes. A dog's recorded
    /// crate version describes the process that was running when it
    /// connected, so it says nothing about a dog that has since been
    /// replaced; the only thing that knows whether a dog can talk to this
    /// daemon is whether this daemon accepted its handshake. That is what
    /// this answers, which is why the reading is worth taking AFTER a
    /// reload rather than before one.
    ///
    /// Answers [`Response::DogStaleness`].
    ///
    /// # Why this variant does not move `PROTOCOL_VERSION`
    ///
    /// The same argument [`Self::HandoverFitness`] makes above, and the
    /// same gate enforces it: its only caller is `shep daemon reload`, which
    /// asks it of the successor it has just proven is running this binary's
    /// own version. An older daemon is never sent it.
    DogStaleness,
    /// Ask whether this daemon could hand its flock to a successor in place,
    /// rather than stopping it and starting it again (`shep daemon reload`).
    ///
    /// Read-only, and nothing here triggers a handover. The trigger is a
    /// signal and always was: a socket request cannot be the trigger, because
    /// the case that most needs a reload is the one where the daemon refuses
    /// the client at the handshake. What travels over the socket is the
    /// DECISION, for a reason a signal cannot serve (spec H3a) -- a signal
    /// carries no reply, so a daemon that took one, refused, and fell back to
    /// its own graceful stop would leave the client polling for a successor
    /// nobody started, with the flock down and staying down.
    ///
    /// Answers [`Response::HandoverFitness`]. Every refusal is a feature the
    /// running daemon cannot yet carry, not an error: the caller falls back
    /// to a stop-and-start, which is correct behaviour rather than a degraded
    /// one, and prints the reason to the operator who asked for the reload.
    ///
    /// # Why this variant does not move `PROTOCOL_VERSION`
    ///
    /// An older daemon cannot deserialize a variant it has never seen, which
    /// is normally what a bump is for. It is never sent to one. `daemon
    /// reload` is an exempt verb, so it connects to a mismatched daemon
    /// deliberately, learns the daemon's crate version from the handshake,
    /// and takes the stop arm for anything predating the handover without
    /// ever asking. shep-cli's `commands::daemon` holds that gate and a test
    /// of its own pins it.
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
/// `#[non_exhaustive]`: this struct grows fields over time with no hand-edit
/// sweep needed across OUT-OF-TREE callers — it forbids a struct literal
/// outside this crate, not inside it. `sample_info()` and
/// [`ProcessInfoBuilder`] both still name every field and both still need
/// updating the day a field is added; what the attribute buys is that
/// nothing downstream does. `deferred.md`'s own `ProcessInfo` entry defers
/// SPLITTING it into several smaller types, not growing it — this attribute
/// plus [`ProcessInfo::builder`] is "deliberately the opposite of forcing
/// the split early," which is what makes a field like `last_exit` cheap to
/// add for a concrete operator need, not a reason to withhold one. Use
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
    /// The marker a dog has asked to have painted beside this sheep, or
    /// `None` when no dog has painted one — which also covers a peer daemon
    /// that predates the field, the same skew rule [`Self::out_file`]
    /// documents for itself.
    ///
    /// A `String` rather than a [`Smit`], deliberately: a client decoding a
    /// listing from a daemon that already validated the text should not have
    /// to re-run the parser, and [`ProcessInfo`] is a report rather than an
    /// input. The validation that makes this safe to print happened at the
    /// daemon's ingress — see [`Smit`] for why there and not at the renderer.
    ///
    /// Every instance of a name shows the same marker: smits are keyed by
    /// sheep name, not by instance id.
    pub smit: Option<String>,
    /// Which instance slot of its app this sheep occupies, counting from 0.
    ///
    /// `None` when the peer daemon predates the field, the same skew rule
    /// [`Self::out_file`] documents for itself. Deliberately not a bare
    /// `u32` defaulted to 0: an app stocked to four instances would then
    /// report four rows all claiming slot 0, which is the silently-wrong
    /// zero [`Self::dog`] warns against. A reader that finds `None` should
    /// render exactly what it rendered before this field existed.
    pub instance: Option<u32>,
    /// Whether this dog has completed a handshake with the shepherd that is
    /// reporting it, and not been refused since; `None` for a sheep.
    ///
    /// Read [`Self::dog`]'s own doc first — this field follows it rather
    /// than [`Self::cpu_percent`], and for the same reason. `None` covers a
    /// sheep and a peer daemon that predates the field, and collapsing the
    /// two costs nothing: a sheep never handshakes with anything (it has no
    /// connection to the shepherd at all, only a supervised process), so
    /// "no handshake fact to report" is the true answer either way, and a
    /// reader that finds `None` renders exactly what it rendered before
    /// this field existed. Do not "fix" this into three cases.
    ///
    /// `Some(false)` is the one that matters and it is why this exists.
    /// [`Self::status`] reports whether a PROCESS is alive, which for a
    /// sheep is the whole truth and for a dog is not: a dog that cannot
    /// talk to the shepherd is not doing its job, however alive it is. A
    /// dog running on a protocol this shepherd refuses is exactly that, and
    /// before this field a listing reported it `online` with zero restarts
    /// while its own log filled with refusals.
    ///
    /// A fact and not a verdict, deliberately: this says whether the
    /// handshake happened, never what a renderer should print about it. A
    /// dog that has only just been spawned has not handshaken yet and is
    /// perfectly healthy, so the decision about which lifecycle states that
    /// silence is worth overriding belongs to the reader.
    pub handshook: Option<bool>,
    /// Whether the reporting shepherd has GIVEN UP on this dog — restarted
    /// it once for never answering, watched that not help, and stopped
    /// restarting it; `None` for a sheep.
    ///
    /// The same `None` rule [`Self::handshook`] documents, for the same
    /// reason: a sheep is never given up on because a sheep never had to
    /// answer anything, so "no verdict to report" is the true answer both
    /// for a sheep and for a peer daemon that predates this field, and a
    /// reader that finds `None` renders exactly what it rendered before the
    /// field existed. Do not "fix" this into three cases.
    ///
    /// **Why this is not derivable from [`Self::handshook`], which is the
    /// whole reason it exists.** `Some(false)` there covers two dogs whose
    /// rows are otherwise identical: one spawned three seconds ago that has
    /// simply not dialled back yet, and one this shepherd has permanently
    /// stopped restarting. The first needs nothing done about it and the
    /// second is an incident. Before this field the give-up was a latch
    /// inside the daemon that no listing could see, so every operator-facing
    /// surface rendered both as the same word.
    ///
    /// A fact and not a verdict, again deliberately. It says the shepherd
    /// stopped, never why — the why is what the shepherd wrote into that
    /// dog's own log when it gave up, and it is the one place that can name
    /// the evidence (`shep bleats <dog>`). A renderer that invented a cause
    /// here would be re-committing the bug this field was added during: a
    /// shepherd asserting a cause it never observed.
    pub dog_stale: Option<bool>,
    /// The [`AppConfig`] field NAMES this sheep's
    /// spec differs from a load's parked config for, in field-name order.
    /// `None` when nothing is parked (every sheep outside the window
    /// between a load that changed a `NeedsRespawn` field and the restart
    /// that picks it up), and also when the peer daemon predates the field,
    /// the same skew rule [`Self::out_file`] documents for itself.
    ///
    /// Names only, never values, for the same reason [`SheepDrift::fields`]
    /// carries names only: a differing `env` reports `"env"` and stops
    /// there (IR-41). `shep reload` is what promotes a parked config.
    ///
    /// `#[serde(skip_serializing_if = "Option::is_none")]`: most sheep are
    /// not mid-parking, so this keeps the ordinary reply free of a key that
    /// would otherwise be `null` on almost every row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<String>>,
    /// The [`AppConfig`] field NAMES an operator
    /// has set on this sheep that its current Flockfile does not declare,
    /// in field-name order. `None` when there is nothing to report: no
    /// override on record for this sheep, or a peer daemon that predates
    /// the field.
    ///
    /// Names only, never values, for the reason [`Self::pending`] gives:
    /// [`crate::overrides::AppOverrides::fields`] can hold an `env` value,
    /// and nothing in shep sends an app's env to a client (IR-41).
    ///
    /// `#[serde(skip_serializing_if = "Option::is_none")]`, for the same
    /// reason [`Self::pending`] carries it: most sheep carry no override at
    /// all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overridden: Option<Vec<String>>,
}

/// Orders one flock listing the way every operator-facing surface presents
/// one: by name, then by instance slot, then by id.
///
/// # Why name first
///
/// An id is assigned at registration, so ordering by it sorts the flock by
/// an accident of history rather than by anything an operator is looking
/// for. It is not stable either: a `delete all` followed by a fresh start
/// moved a real thirteen-app flock from ids 0-10 to 11-21 with nothing
/// about the apps having changed. A name is what an operator scans a long
/// listing for, and it survives that churn.
///
/// # Why id breaks the tie
///
/// A name is unique to an APP, not to a sheep: an app stocked to four
/// instances puts four rows under one name. Name alone is therefore not a
/// total order, and an unstable sort would let those four shuffle between
/// refreshes — visible in `shep flock` and worse in `shep lookout`, which
/// repolls every two seconds. The id keeps its other job unchanged: it is
/// still how an operator addresses one instance at `shep stop 11`. It stops
/// being a sort key and stays an addressing key.
///
/// This is the ONLY ordering rule in shep, and the daemon's own
/// `snapshot_all` calls this function rather than restating it. The order is
/// `(name, instance, id)`: [`ProcessInfo::instance`] now carries the slot a
/// row occupies, so a reload that hands slot 0 a fresh id no longer moves
/// that row out of place. A listing whose rows all carry `None` (an older
/// peer daemon, or a row with no slot to report) collapses to the same
/// `(name, id)` order this function used before the field existed, because
/// `None` sorts before every `Some` and every row in that listing shares it.
pub fn sort_flock(listing: &mut [ProcessInfo]) {
    listing.sort_unstable_by(|a, b| {
        (a.name.as_str(), a.instance, a.id).cmp(&(b.name.as_str(), b.instance, b.id))
    });
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

/// One registered sheep whose stored config differs from a caller's copy:
/// the answer [`Request::ConfigDrift`] is asking for
///
/// Field NAMES only, never their values. This is built to be printed at an
/// operator, and [`AppConfig::env`](crate::config::AppConfig::env) carries
/// secrets, so a differing `env` reports `"env"` and nothing more (IR-41).
/// `Debug` is derived for that reason: there is nothing here to redact.
// wire format: changing field names is a breaking change
//
// `#[non_exhaustive]`: shep-core is a published library, an out-of-tree
// consumer can match or construct this exhaustively today, and a third field
// (which side is newer, say) would break them with no version bump to say
// so (IR-20). [`SheepDrift::new`] is how the daemon builds one.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheepDrift {
    /// The sheep's name. Both configs share it by construction: it is what
    /// matched them to each other.
    pub name: String,
    /// The [`AppConfig`] fields that differ, in
    /// field-name order. Never empty: a sheep with nothing to report is left
    /// out of the answer entirely.
    pub fields: Vec<String>,
}

impl SheepDrift {
    /// Builds one sheep's report.
    ///
    /// No builder, unlike [`ProcessInfo`]: both fields are required and
    /// neither can be defaulted, so there is no optional surface for one to
    /// spare a caller.
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
/// One of these per app the request named, whether or not the app was found
/// and whether or not anything about it changed. A load that quietly skipped
/// an app would leave an operator reading a Flockfile that says one thing and
/// a flock doing another, which is the failure this whole verb exists to fix.
///
/// [`Self::applied`] and [`Self::pending`] carry field NAMES only, never
/// their values, exactly as [`SheepDrift`] carries them and for the same
/// reason: this is built to be printed at an operator, and
/// [`AppConfig::env`](crate::config::AppConfig::env) carries secrets, so an
/// applied `env` reports `"env"` and nothing more (IR-41). The merged config
/// itself never reaches a client at all -- the daemon keeps it, because a
/// config is not something a client needs and `env` is in it.
///
/// **[`Self::refused`] is prose and is deliberately not held to that**, so
/// the rule above is scoped to the two lists rather than stated of the whole
/// type. A refusal is a sentence an operator reads, and the useful ones name
/// the thing that was refused: the daemon's own instance-count refusal
/// quotes the count the file asked for, and a rejected `{{...}}` template
/// quotes the offending fragment. Those are values out of the FILE the
/// caller just sent, not values out of the flock's stored config, which is
/// what makes them safe to echo; see that field's own doc. A refusal that
/// needs to name an `env` value is one the daemon must word differently, not
/// one this type can prevent.
///
/// `Debug` is derived on that basis: `refused` holds only what the daemon
/// chose to put in front of an operator anyway, so there is nothing here to
/// redact that redacting would help.
// wire format: changing field names is a breaking change
//
// `#[non_exhaustive]`: shep-core is a published library, an out-of-tree
// consumer can match or construct this exhaustively today, and a fourth
// field (which of the pending fields a reload would promote, say) would
// break them with no version bump to say so (IR-20). [`SheepApplied::new`]
// is how the daemon builds one.
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
    /// `shep reload <name>` is what promotes them; a client rendering this
    /// list says so, because a pending list with no remedy beside it is a
    /// report nobody can act on.
    pub pending: Vec<String>,
    /// Why some or all of this app's change did not land, in the daemon's own
    /// words, or `None` when the whole of it did.
    ///
    /// Not the same question as the two lists being empty. A refusal raised
    /// before anything was touched leaves both empty, and so does a load with
    /// nothing to do; a refusal raised after the flock was already reshaped
    /// arrives beside lists that carry what did land. The message is what
    /// tells them apart, which is why it is a sentence rather than a code.
    pub refused: Option<String>,
}

impl SheepApplied {
    /// Builds one app's report.
    ///
    /// No builder, matching [`SheepDrift::new`]: all four fields are required
    /// and none can be defaulted to something honest, so there is no optional
    /// surface for one to spare a caller.
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

/// One sheep's effective config as a pane sees it: every field but env's
/// values, plus which fields an operator has overridden and which are
/// waiting on a respawn.
///
/// The answer to [`Request::SheepConfig`], and the one reply in this module
/// that carries a whole [`AppConfig`]. [`SheepApplied`] deliberately carries
/// field NAMES alone, and the difference is what each is for: that one is
/// printed at an operator who already has the file, this one feeds a pane
/// that is about to edit fields it has to be able to show first.
// wire format: changing field names is a breaking change
//
// `#[non_exhaustive]`: shep-core is a published library and a sixth field
// would otherwise break an out-of-tree consumer's construction of this with
// no version bump to say so (IR-20). [`SheepConfigView::new`] is how the
// daemon builds one, and it is what enforces the emptied `env`.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheepConfigView {
    /// The sheep's name.
    pub name: String,
    /// The effective config with `env` cleared. Every remaining field is
    /// operator-supplied policy the pane is about to let them edit, so
    /// withholding a value would make the pane unusable while protecting
    /// nothing.
    pub config: AppConfig,
    /// The env keys, so the pane can list them. Never the values.
    pub env_keys: Vec<String>,
    /// Field names an operator has set that the Flockfile does not declare.
    pub overridden: Vec<String>,
    /// Field names parked until the next respawn.
    pub pending: Vec<String>,
}

impl SheepConfigView {
    /// Builds one, clearing `env` and recording its keys.
    ///
    /// The clearing happens HERE rather than at the one call site, so a
    /// second caller cannot forget it: this constructor is the only way to
    /// build the type outside this crate, since `#[non_exhaustive]` blocks
    /// a literal.
    #[must_use]
    pub fn new(mut config: AppConfig, overridden: Vec<String>, pending: Vec<String>) -> Self {
        let env_keys = config.env.keys().cloned().collect();
        config.env.clear();
        Self {
            name: config.name.clone(),
            config,
            env_keys,
            overridden,
            pending,
        }
    }
}

/// Redacted (IR-41): `config` carries `args` and `cwd`, which routinely hold
/// a token or a home directory, and this type is what a `{:?}` on a
/// [`Response`] would print. The three lists are counted rather than named
/// for the same reason -- `env_keys` is a key set, which is itself worth
/// keeping out of a log.
impl fmt::Debug for SheepConfigView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SheepConfigView {{ name: {:?}, env_keys: {}, overridden: {}, pending: {} }}",
            self.name,
            self.env_keys.len(),
            self.overridden.len(),
            self.pending.len()
        )
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
//
// `large_enum_variant` allowed, not fixed: `DogStarted` holds a whole
// `ProcessInfo` inline where every other variant holds a `Vec` of them, and
// adding `smit` to that struct is what pushed the spread past the lint's
// threshold. Clippy's remedy is to box the payload, which would be a source
// break for every `Response::DogStarted(info)` in and out of this workspace
// — for nothing: a `Response` is built once per reply and serialized
// immediately, so the size it occupies on one stack frame in between is not
// a cost anybody pays. The wire shape is identical either way.
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
    /// A row here can still be `Online`. `Add` is idempotent by name, so an
    /// app the flock already had is answered as it stands rather than
    /// replaced. The reply describes the membership the request leaves
    /// behind, not work it did.
    Added(Vec<ProcessInfo>),
    /// Answer to `ConfigDrift`: one entry per app that is registered under a
    /// config different from the one asked about, and no entry for anything
    /// else. An empty vector means every app asked about either matches or
    /// is not registered at all.
    Drifted(Vec<SheepDrift>),
    /// Answer to `ApplyConfig`: one entry per app the request named, in the
    /// order it named them, including the apps that were refused and the
    /// apps that had nothing to change.
    ///
    /// Complete where [`Self::Drifted`] is filtered, and the difference is
    /// deliberate. A drift report answers "what is different", so a matching
    /// app has nothing to say; a load answers "what did you do to each of
    /// these", and an app missing from that answer is indistinguishable from
    /// an app the daemon silently dropped.
    Applied(Vec<SheepApplied>),
    /// Answer to `SheepConfig`: one sheep's config with `env` emptied and
    /// its keys listed beside it.
    SheepConfig(SheepConfigView),
    /// Answer to `SetSheepEnv`: the key that was set or removed.
    ///
    /// Never the value, and never the resulting env map. This reply exists
    /// to confirm which key moved, and echoing what was just written back
    /// down a socket would undo the whole point of `SheepConfig` withholding
    /// it (IR-41).
    SheepEnvSet {
        /// The sheep.
        name: String,
        /// The key.
        key: String,
    },
    /// Answer to `SetDogConfig`: the section was written and the topic
    /// published.
    DogConfigSet {
        /// The dog.
        name: String,
    },
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
    // The order is cited as [`sort_flock`]'s shared rule rather than restated
    // as this reply's own, so the two cannot drift apart.
    /// Answer to `Scale` — the app's instances that will REMAIN, one row
    /// each, by name, then by instance slot, then by id ([`sort_flock`]).
    /// Every row shares one name here, so in practice that is slot order,
    /// with the id breaking a tie only where two rows report the same slot.
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
    /// Answer to `SetSmit` — every instance of the named sheep, one row
    /// each, each carrying the smit as it now stands.
    ///
    /// Its own variant rather than one of the ten above, on this enum's own
    /// stated terms: each of them names which request it answers so that one
    /// can diverge later without a protocol bump. A future `SetSmit` reply
    /// that also reported which connection holds the mark would have nowhere
    /// to go if this shared `Scaled`.
    SmitPainted(Vec<ProcessInfo>),
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
    /// Answer to `DogStaleness` — this daemon's own handshake record, split
    /// into the dogs it has given up on and the dogs it is still waiting on.
    ///
    /// Two lists rather than one because they are answers to two different
    /// questions, and only one of them is reportable. `stale` is a
    /// finding: those dogs were refused, restarted from the binary on disk,
    /// and refused again. `pending` is a reason to ask again: those
    /// dogs have not finished settling, so a reading taken now would be a
    /// guess about them rather than a fact.
    ///
    /// Names only. What a stale dog's crate version is does not answer the
    /// question a caller is asking — two builds differing only in the
    /// protocol they speak report the same version — so carrying one here
    /// would invite exactly the inference it cannot support.
    DogStaleness {
        /// Dogs this daemon has refused twice: once on the handshake that
        /// bought them a restart from disk, and again after it. It will not
        /// restart them a third time (the handover design's G8).
        stale: Vec<String>,
        /// Dogs this daemon is still waiting to hear a final answer from —
        /// one whose restart is in flight, or one it supervises that has
        /// not handshook yet. Neither stale nor known healthy.
        pending: Vec<String>,
    },
    /// Answer to `HandoverFitness`: `None` when the whole flock can be
    /// carried across a daemon handover, and otherwise the sentence saying
    /// which sheep cannot be and why.
    ///
    /// A rendered sentence rather than a structured reason, deliberately. The
    /// set of things a handover cannot yet carry is exactly the set of things
    /// that phase has not built, so it changes with every phase that widens
    /// it, and a wire enum would make each of those a protocol change for a
    /// string the client does nothing with but print. The daemon owns the
    /// wording because the daemon owns the gate.
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
    /// The daemon's own crate version, when it chose to name it.
    ///
    /// Set on a [`RpcErrorCode::ProtocolMismatch`] refusal, where it is the
    /// only place a client can learn it: the refusal reports the daemon's
    /// PROTOCOL, and [`HelloAck::daemon_version`] never arrives. `shep
    /// daemon reload` picks its mechanism by version, and a protocol bump is
    /// exactly when that choice matters.
    ///
    /// `None` on every other error, and on any refusal from a daemon built
    /// before this field existed — which no upgrade can change, so a reader
    /// must treat `None` as "unknown" and take the conservative path.
    ///
    /// Additive by construction: absent on the wire rather than `null`, and
    /// ignored by a client too old to know it, so
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
            out_file: Some("/home/ada/.shep/logs/web-0-out.log".to_string()),
            err_file: Some("/home/ada/.shep/logs/web-0-err.log".to_string()),
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
            smit: None,
            instance: None,
            handshook: None,
            dog_stale: None,
            // Left at the builder's own default, like `dog`/`lambs` above
            // this fixture: this feeds `reply_wire_snapshots` and
            // `bus_event_wire_snapshots`, so a `Some(..)` here would move
            // pinned bytes. `every_setter_writes_its_own_field_and_no_other`
            // exercises the setter body on its own, the same way it does
            // for `dog`, `lambs`, `smit` and `handshook`.
            pending: None,
            overridden: None,
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

        // `smit` is the third, on the same terms, and it is the field a
        // third party writes — so an empty setter body here would silently
        // drop every dog's mark rather than merely lose a decoration.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .smit(Some("\u{25b2} main@a1b2c3".to_string()))
                .build()
                .smit
                .as_deref(),
            Some("\u{25b2} main@a1b2c3"),
            "an empty `smit` setter body is invisible to the comparison above"
        );

        // `handshook` is the fourth field, on the same terms as the three
        // above: `sample_info()`'s value is `None`, which is also the
        // builder's default, so an EMPTY `handshook` setter body would pass
        // the `assert_eq!` above. `sample_info()` still cannot be changed to
        // a `Some(..)` — it feeds `reply_wire_snapshots` and
        // `bus_event_wire_snapshots`, so altering it moves pinned bytes.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .handshook(Some(false))
                .build()
                .handshook,
            Some(false),
            "an empty `handshook` setter body is invisible to the comparison above"
        );

        // `dog_stale` is the fifth, and the pairing is the point: it and
        // `handshook` are both `None` by default, so a setter that dropped
        // this one would leave every dog row saying "silent" and never
        // "given up on" -- the exact distinction the field was added to
        // carry.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .dog_stale(Some(true))
                .build()
                .dog_stale,
            Some(true),
            "an empty `dog_stale` setter body is invisible to the comparison above"
        );

        // `pending` is the sixth field, on the same terms as the five above.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .pending(Some(vec!["env".to_string()]))
                .build()
                .pending,
            Some(vec!["env".to_string()]),
            "an empty `pending` setter body is invisible to the comparison above"
        );

        // `overridden` is the seventh and last, on the same terms.
        assert_eq!(
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .overridden(Some(vec!["cwd".to_string()]))
                .build()
                .overridden,
            Some(vec!["cwd".to_string()]),
            "an empty `overridden` setter body is invisible to the comparison above"
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
    /// This is also the empirical proof of a subtle point: none of
    /// `ProcessInfo`'s fields carry `#[serde(default)]`, and there is
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
    /// fails if an `Add` decodes as anything but an `Add`.
    ///
    /// `Add` and `Start` carry byte-identical payloads and differ by their
    /// `kind` alone, so the tag is the entire distinction between registering
    /// an app and spawning it. The snapshot above pins what this ENCODES to;
    /// this pins what a daemon reading those bytes gets back, which is the
    /// half that decides whether a process starts.
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

    /// fails if a config pane's answer can carry an env VALUE. The pane
    /// edits everything else about a sheep, so the config itself has to
    /// travel; `env` is the one map in it that holds secrets, and decision
    /// 12 of the overrides design is that the keys travel and the values
    /// never do (IR-41).
    #[test]
    fn a_sheep_config_view_never_carries_an_env_value() {
        let mut config = AppConfig::minimal("web", "./srv");
        config
            .env
            .insert("DB_PASS".to_string(), "hunter2".to_string());
        let view = SheepConfigView::new(config, Vec::new(), Vec::new());
        assert!(view.config.env.is_empty());
        assert_eq!(view.env_keys, ["DB_PASS"]);
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
    }

    /// fails if this type's `Debug` ever prints the config it carries. A
    /// `{:?}` on a `Response` reaches it, and `config` holds `args` and
    /// `cwd` as well as the env keys (IR-41).
    #[test]
    fn a_sheep_config_views_debug_is_the_exact_redacted_string() {
        let mut config = AppConfig::minimal("web", "./srv");
        config.env.insert("A".to_string(), "1".to_string());
        let view = SheepConfigView::new(config, vec!["max_restarts".to_string()], Vec::new());
        assert_eq!(
            format!("{view:?}"),
            r#"SheepConfigView { name: "web", env_keys: 1, overridden: 1, pending: 0 }"#
        );
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
            // The second verb here with no `selector` key, and the only one
            // whose payload a third party writes. Both halves of its
            // `Option` are pinned — a paint and a clear — because a dog
            // author reading this fixture needs the clear frame's exact
            // shape and would otherwise have to guess `null`.
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
            // An EMPTY `apps`, unlike `start`'s row above. The two carry the
            // identical payload type, so a second `AppConfig` blob here would
            // pin nothing `start`'s blob does not already pin, at fifty lines
            // of snapshot. What is genuinely this row's own is the tag and
            // the key the list travels under, and an empty list shows both.
            Envelope {
                id: 22,
                deadline_ms: None,
                body: Request::ConfigDrift { apps: Vec::new() },
            },
            // The only STRUCT-shaped `SelectorSpec` variant, and the one
            // whose serialized shape moved `PROTOCOL_VERSION` from 1 to 2.
            // Every other selector on this wire is a unit or a newtype, both
            // already pinned by the rows above, so this row is the only place
            // `"kind":"instance"` and the `slot` key are held to anything.
            // Without it, renaming the field or flattening the variant turned
            // nothing red on the exact type the version bump was for.
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
            // The one request in this enum that an older daemon must never
            // be sent, so the one whose exact tag matters most: shep-cli
            // gates it on the daemon's crate version, and a rename here
            // would be a variant nothing on either side recognises.
            Envelope {
                id: 24,
                deadline_ms: None,
                body: Request::HandoverFitness,
            },
            // The second request gated on the daemon's crate version, and
            // pinned beside the first for that reason: the two are asked by
            // the same verb, of the two daemons either side of the same
            // handover, and a rename of either is a variant nothing on
            // either side recognises.
            Envelope {
                id: 25,
                deadline_ms: None,
                body: Request::DogStaleness,
            },
            // The only request carrying a `DeclaredApp` rather than a bare
            // `AppConfig`, and the two key sets beside the config are the
            // whole reason it does: a merge keys on what a document CLAIMED,
            // so a reader that dropped `declared` would apply every default
            // the document never wrote. `declared_env` is pinned non-empty
            // for the same reason and one more -- it holds env key NAMES,
            // and a fixture is where an out-of-tree reader learns that no
            // env VALUE travels under it (IR-41).
            //
            // `reset` is pinned at its non-default depth. The default
            // serializes as `"none"`, which is the one value a reader could
            // get right by accident.
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
            // The same app as the `start` row above, deliberately: the two
            // requests carry identical payloads and differ by their `kind`
            // alone, so an `add` that serialized under `start`'s tag -- the
            // shape a copy-pasted variant takes -- shows up here as two
            // identical objects rather than as a diff a reader has to
            // compare field by field. The same trick the `reopen`/`flush`
            // pair above plays.
            Envelope {
                id: 27,
                deadline_ms: None,
                body: Request::Add {
                    apps: vec![AppConfig::minimal("web", "./srv")],
                },
            },
            // The three config-pane requests. `SheepConfig` takes a name
            // rather than a selector, like `Scale` and `SetSmit` above and
            // for their reason: a pane edits one sheep.
            Envelope {
                id: 28,
                deadline_ms: None,
                body: Request::SheepConfig {
                    name: "web".to_string(),
                },
            },
            // `value` is pinned as `Some`, because the `None` spelling is
            // what REMOVES the key and a reader that guessed the two apart
            // wrongly would delete an operator's env instead of setting it.
            // The value is a placeholder, not a secret: this is the one
            // request in the enum that carries an env value at all, and it
            // travels in one direction only -- nothing ever reads it back
            // (decision 12 of the overrides design).
            Envelope {
                id: 29,
                deadline_ms: None,
                body: Request::SetSheepEnv {
                    name: "web".to_string(),
                    key: "DATABASE_URL".to_string(),
                    value: Some("postgres://localhost/app".to_string()),
                },
            },
            // The second request carrying a `DogSectionToml`, and pinned
            // beside its reader: `DogConfig` asks for a section and this
            // writes one back, so the two have to agree about the shape a
            // section takes on the wire.
            Envelope {
                id: 30,
                deadline_ms: None,
                body: Request::SetDogConfig {
                    name: "bark".to_string(),
                    toml: "debounce = \"30s\"\n".to_string().into(),
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
                    path: "/home/ada/.shep/flock.json".to_string(),
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
            // The existing comment on the `Triggered` row is right that pinning
            // `Flock` once already proves the `Vec<ProcessInfo>` SHAPE — but
            // it does not prove any of these variants' own `kind` tags, and
            // three of them are not `Vec<ProcessInfo>`-shaped at all
            // (`Deleted` is a `Vec<u32>`, `Subscribed` and `ShuttingDown`
            // carry nothing). Each row below therefore carries the smallest
            // body that shows its wire shape — empty where empty is legal,
            // `Deleted`'s two ids where the shape needs elements: what is
            // being pinned here is the tag, and a body repeated eight times
            // would bury it.
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
            // The one row that pins a smit on the wire. `sample_info()`
            // carries none, deliberately (see `every_setter_writes_its_own_
            // field_and_no_other` for why it cannot), so without this row
            // the field is pinned only in its absent shape — and the absent
            // shape is not the one a dog's reader has to parse.
            Reply {
                id: 25,
                result: Ok(Response::SmitPainted(vec![
                    ProcessInfo::builder(3, "web", ProcStatus::Online)
                        .pid(Some(4242))
                        .smit(Some("\u{25b2} main@a1b2c3".to_string()))
                        .build(),
                ])),
            },
            // Two entries in one reply, and each is the shape the other is
            // not: a sheep drifting in one field and a sheep drifting in
            // several. `env` is deliberately one of them, because reporting
            // it as a bare NAME is the whole security property of this row
            // (IR-41) and a fixture is where an out-of-tree reader learns
            // that no value ever travels with it.
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
            // `sample_info()` pins `instance`'s absent shape (`None`, an old
            // peer or a single-instance app); every row above reuses it, so
            // without this row the present shape (`Some(2)`, a live slot on
            // a scaled app) is never on the wire at all.
            Reply {
                id: 27,
                result: Ok(Response::Flock(vec![
                    ProcessInfo::builder(9, "web", ProcStatus::Online)
                        .pid(Some(5150))
                        .instance(Some(2))
                        .build(),
                ])),
            },
            // Both shapes of the handover answer, because the difference
            // between them is a `null` and a caller that read the key's
            // presence rather than its value would pass on one and refuse
            // every flock on the other.
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
            // Both lists non-empty and DIFFERENT, because the two carry the
            // same wire shape and a reply that filled one from the other
            // would be invisible in a fixture that used the same names
            // twice. Empty is the shape an ordinary reload sees, and it is
            // already proven by every `Vec`-carrying row above.
            Reply {
                id: 30,
                result: Ok(Response::DogStaleness {
                    stale: vec!["metrics".to_string()],
                    pending: vec!["bark".to_string()],
                }),
            },
            // `sample_info()` pins `handshook`'s absent shape (`None`, a
            // sheep or an older peer); every row above reuses it, so
            // without this row the shape that actually changes an
            // operator's reading — a dog whose process is up and which has
            // never answered this shepherd — is never on the wire at all.
            //
            // `dog_stale: false` is not padding: this is the silence the
            // shepherd is still waiting out, and the row below is the one it
            // has given up on. The two differ in exactly one byte-level key
            // and in what every operator-facing surface must say about them,
            // so pinning one without the other would leave the distinction
            // untested on the wire it travels over.
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
            // Three entries, because the three shapes a load produces are
            // not interchangeable and a reader that saw only one would
            // guess wrong about the others: an app that applied cleanly, an
            // app whose change is waiting for a respawn, and an app that
            // was refused outright. The refusal's `null` twin is pinned by
            // the first two rows, so a reader learns the key is always
            // present rather than sometimes absent.
            //
            // `env` is one of the pending names on purpose. Reporting it as
            // a bare NAME is this reply's whole security property (IR-41),
            // and a fixture is where an out-of-tree reader learns that no
            // value ever travels with it.
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
            // `Added`'s tag. It is a `Vec<ProcessInfo>` like three of the
            // rows above, so the tag is the only thing about it a fixture can
            // prove, and empty is the shape that proves it -- the same
            // reasoning the block of empty rows further up states for its
            // own. Down here rather than beside `Started`, where it belongs
            // by meaning, because every id in this vector is hand-written and
            // an insertion in the middle renumbers twenty rows for nothing.
            Reply {
                id: 33,
                result: Ok(Response::Added(vec![])),
            },
            // The config pane's answer, and the row that proves its whole
            // security property: `env` serializes as an empty object while
            // `env_keys` names the key beside it, so an out-of-tree reader
            // learns here that a value never travels (IR-41).
            Reply {
                id: 34,
                result: Ok(Response::SheepConfig(SheepConfigView::new(
                    {
                        let mut config = AppConfig::minimal("web", "./srv");
                        config
                            .env
                            .insert("DATABASE_URL".to_string(), "postgres://x".to_string());
                        config
                    },
                    vec!["max_restarts".to_string()],
                    vec!["env".to_string()],
                ))),
            },
            // The two acknowledgements. Neither echoes what was written:
            // `SheepEnvSet` names the key and not its value, for the reason
            // the row above pins, and `DogConfigSet` names the dog and not
            // the section.
            Reply {
                id: 35,
                result: Ok(Response::SheepEnvSet {
                    name: "web".to_string(),
                    key: "DATABASE_URL".to_string(),
                }),
            },
            Reply {
                id: 36,
                result: Ok(Response::DogConfigSet {
                    name: "bark".to_string(),
                }),
            },
        ];
        insta::assert_json_snapshot!("reply_wire_v3", replies);
    }

    /// fails if `applied` or `pending` ever carries a field's VALUE rather
    /// than its name. This reply is printed at an operator and `env` values
    /// are secrets, so the wire form of those two lists has to be names
    /// alone (IR-41). `refused` is prose and is scoped out of that rule at
    /// the type -- see its own doc for why -- so it is left `None` here
    /// rather than asserted on.
    ///
    /// Asserts on the serialized JSON rather than on the struct, because the
    /// struct's `Vec<String>` cannot say which of the two a string is: a
    /// build that put `DATABASE_URL=postgres://...` in the list would type-
    /// check and pass any assertion made on the field's shape.
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

    /// fails if `SheepApplied`'s `Debug` grows a value. Derived today because
    /// there is nothing here to redact, and that is a property of what the
    /// daemon puts IN it rather than of the derive, so it is worth one exact
    /// string (IR-41).
    #[test]
    fn a_sheep_applied_debug_prints_the_names_it_was_given() {
        let applied = SheepApplied::new("web", vec!["cwd".to_string()], Vec::new(), None);
        assert_eq!(
            format!("{applied:?}"),
            "SheepApplied { name: \"web\", applied: [\"cwd\"], pending: [], refused: None }"
        );
    }

    /// fails if the new field breaks an older peer, on the same terms as
    /// `last_exit` and `lambs` before it. A daemon that predates smits sends
    /// no `smit` key, and this decoding to `None` rather than erroring is
    /// why the field cost `PROTOCOL_VERSION` no bump of its own. The
    /// constant has moved since, for a reason unrelated to this field, so
    /// this says what the field did rather than what the constant is.
    #[test]
    fn a_process_info_without_a_smit_key_still_deserializes() {
        let fixture = r#"{"id":1,"name":"web","status":"online","pid":42,"restarts":0,"uptime_ms":10,"fold":null,"out_file":null,"err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":null,"lambs":null,"last_exit":null}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.smit, None);
    }

    /// fails if `handshook` breaks an older peer, on the same terms as
    /// `smit` and `instance` before it. A daemon that predates the field
    /// sends no `handshook` key, so this decoding to `None` rather than
    /// erroring is why the field cost `PROTOCOL_VERSION` no bump of its
    /// own: the evolution rule in this module's parent says an additive
    /// optional field keeps the version, and a required one would make a
    /// current client unable to list against that daemon at all. The
    /// constant has moved since, for a reason unrelated to this field.
    ///
    /// The fixture is a DOG's row, deliberately — that is the one row where
    /// the missing key changes what a renderer prints, and `None` there has
    /// to keep meaning "render this exactly as it rendered before the field
    /// existed" rather than "this dog has never handshaken".
    #[test]
    fn a_process_info_without_a_handshook_key_still_deserializes() {
        let fixture = r#"{"id":1,"name":"metrics","status":"online","pid":42,"restarts":0,"uptime_ms":10,"fold":null,"out_file":null,"err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":{"kind":"built_in"},"lambs":null,"last_exit":null,"smit":null,"instance":0}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.handshook, None);
        assert_eq!(info.dog, Some(DogSource::BuiltIn));
    }

    /// fails if `dog_stale` breaks an older peer, on the same terms as
    /// `handshook` before it -- the same additive-optional rule, the same
    /// unchanged `PROTOCOL_VERSION`.
    ///
    /// The fixture carries `handshook: false`, which is the case that
    /// matters. A shepherd old enough to report a dog silent but too old to
    /// say whether it gave up on that dog must keep rendering exactly the
    /// word it rendered before -- `None` here is "this shepherd has no
    /// verdict to report", never "it has not given up".
    #[test]
    fn a_process_info_without_a_dog_stale_key_still_deserializes() {
        let fixture = r#"{"id":1,"name":"metrics","status":"online","pid":42,"restarts":0,"uptime_ms":10,"fold":null,"out_file":null,"err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":{"kind":"built_in"},"lambs":null,"last_exit":null,"smit":null,"instance":0,"handshook":false}"#;
        let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
        assert_eq!(info.dog_stale, None);
        assert_eq!(info.handshook, Some(false));
    }

    /// fails if the daemon accepts a smit it should refuse. [`Smit`] must
    /// validate on the way IN, not only in `FromStr`: `docs/dogs.md` tells
    /// dog authors to speak this wire directly, so a dog written in another
    /// language never runs our parser.
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

    /// fails if a smit stops travelling as a bare JSON string. It is a
    /// newtype with a hand-written `Deserialize`, and the pair only agrees
    /// with itself if the serialize side stays transparent — a `Smit` that
    /// serialized as `{"0":"..."}` would round-trip through nothing.
    #[test]
    fn a_smit_travels_as_a_bare_string() {
        let smit: Smit = "\u{25b2} main@a1b2c3".parse().expect("valid");
        let json = serde_json::to_string(&smit).unwrap();
        assert_eq!(json, "\"\u{25b2} main@a1b2c3\"");
        assert_eq!(serde_json::from_str::<Smit>(&json).unwrap(), smit);
    }

    /// fails if the cap starts counting bytes or display columns. Forty-eight
    /// CJK characters are 144 bytes and roughly 96 columns, and all three
    /// numbers disagree — a byte cap would refuse this legitimate smit at a
    /// third of its apparent length.
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

    /// fails if a smit is repaired rather than refused. Trimming or stripping
    /// would hand an operator a mark its publisher never sent, and would put
    /// shep in the business of editing a string it has agreed not to
    /// understand.
    #[test]
    fn a_smit_is_stored_exactly_as_it_arrived() {
        let padded: Smit = "  main@a1b2c3  ".parse().expect("valid");
        assert_eq!(padded.as_str(), "  main@a1b2c3  ");
        assert_eq!(padded.to_string(), "  main@a1b2c3  ");
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
            dog_name: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(json, r#"{"client_version":"0.1.0","protocol":3}"#);
    }

    /// fails if a non-dog client's `Hello` grows a key. The CLI is the
    /// overwhelming majority of handshakes and sends `dog_name: None`, so
    /// `skip_serializing_if` is what keeps this addition free on the wire
    /// for every client that is not a dog — and what makes the bytes above
    /// byte-identical to the ones protocol 2 shipped with.
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

    /// fails if `Hello` gains `#[serde(deny_unknown_fields)]`, or if
    /// `dog_name` stops being optional — the two ways this addition could
    /// become a wire break after the fact.
    ///
    /// `Hello` is the version-negotiation frame, which makes it the one
    /// place where rejecting an unknown field would be unrecoverable: the
    /// daemon would refuse a newer client BEFORE reading `protocol`, so
    /// neither peer could report the skew that caused it. The fixture below
    /// is the committed bytes a client built before this field sends
    /// (IR-35), and the second half is the same rule in the other
    /// direction — an older daemon parsing a newer client's frame.
    #[test]
    fn a_hello_without_a_dog_name_still_parses() {
        let fixture = r#"{"client_version":"0.1.14","protocol":2}"#;
        let hello: Hello = serde_json::from_str(fixture).unwrap();
        assert_eq!(hello.protocol, 2);
        assert_eq!(hello.dog_name, None);

        // The other direction: whatever an older daemon does not know, it
        // must ignore rather than refuse. `unknown_to_an_older_daemon`
        // stands in for `dog_name` as that daemon would see it.
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
    fn an_rpc_error_without_a_daemon_version_serializes_exactly_as_before() {
        // `skip_serializing_if` is what makes this addition free: a daemon
        // with nothing to say puts the same bytes on the wire it always did,
        // not a `"daemon_version":null` key an older client would have to
        // ignore. Pinned as an exact string, because "additive" is a claim
        // about bytes.
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
        // Committed byte fixture from before this field existed (IR-35): the
        // skew direction that matters, a CURRENT client reading an OLD
        // daemon's refusal. It must read as `None` rather than failing to
        // decode, or the field breaks the upgrade it exists to smooth.
        let fixture =
            r#"{"code":"protocol_mismatch","message":"daemon speaks protocol 1, client sent 2"}"#;
        let err: RpcError = serde_json::from_str(fixture).unwrap();
        assert_eq!(err.code, RpcErrorCode::ProtocolMismatch);
        assert_eq!(err.daemon_version, None);
    }

    #[test]
    fn an_old_client_ignores_an_rpc_error_field_it_has_never_seen() {
        // Step 1 of the handover work: proof that `RpcError` may grow an
        // optional field WITHOUT moving `PROTOCOL_VERSION`. Like
        // `ProcessInfo` above, `RpcError` carries no `deny_unknown_fields`,
        // so a client built before a field decodes a daemon that sends it
        // rather than failing the handshake on it.
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

    /// The fixture is built so the two candidate orders CANNOT agree: read
    /// by id it is `web/1, api/2, web/0`, read by name it is
    /// `api, web, web`. A listing that happened to be alphabetical already,
    /// or whose ids happened to ascend with its names, would pass under
    /// either rule and prove nothing.
    ///
    /// The two `web` rows are the tiebreak half, and they are the reason a
    /// multi-instance fixture is required: their ids are seeded out of order
    /// (1 before 0), so a sort keyed on name alone would leave them as it
    /// found them and fail the last assertion while passing the first.
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
        // The skew case the Option exists for: an older shepherd's JSON has no
        // `instance` key at all.
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
