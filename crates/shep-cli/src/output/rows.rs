//! Every rendered payload type in the binary, and the [`Render`] impl that
//! makes each one's table and JSON renderings the same source of truth.
//!
//! Payload types live here, not under `commands/`, and that is load-bearing
//! rather than tidy: this module is pure tier and its own tests (below) name
//! every one of these types directly. A payload type defined under
//! `commands/` (`#[cfg(unix)]`) could not be named by a test running on the
//! Windows leg at all, and `commands/query.rs` (a later task) does not exist
//! yet for a test here to depend on regardless. Every type below is built
//! entirely from `ProcessInfo` / `u32`, and shep-core carries no `cfg` of any
//! kind, so this really is pure tier.

use serde::Serialize;
use shep_core::barks::{Bark, SinkOutcome};
use shep_core::protocol::{
    ActionOutcome, ActionReply, DogSource, LineOutcome, LineReply, ProcessInfo, SignalOutcome,
    SignalReply,
};

use super::Render;

/// `Vec<ProcessInfo>` for every verb whose reply carries one: `flock`,
/// `describe`, `fold`, `start`, `stop`, `restart`, `reopen`, `flush`. A
/// newtype because `ProcessInfo` is shep-core's and the orphan
/// rule forbids implementing our `Render` on it directly. `transparent` so
/// the JSON is a plain array of `ProcessInfo`, not a wrapper object.
///
/// Constructed from a real `Response` under `commands/`, by `query.rs`,
/// `lifecycle.rs` and `logs.rs`. The rule is the authority on both lists,
/// not the lists: a new flock-shaped verb joins them without touching this
/// type, and neither one is a bound on what renders here.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct FlockRows(pub Vec<ProcessInfo>);

impl Render for FlockRows {
    fn headers() -> &'static [&'static str] {
        &[
            "ID", "NAME", "STATUS", "PID", "RESTARTS", "CPU", "MEM", "UPTIME", "FOLD",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|p| {
                vec![
                    p.id.to_string(),
                    p.name.clone(),
                    p.status.to_string(),
                    // `-` rather than an empty cell: an empty cell in a
                    // padded table is indistinguishable from a rendering bug.
                    p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                    p.restarts.to_string(),
                    // `-` for the same reason, and for the same stated
                    // reason PID uses it: a sheep that is not running, or
                    // has been up for less than one sampling window, has no
                    // honest number to report, and `0.0%` would claim the
                    // daemon never made — "this sheep is using no CPU".
                    p.cpu_percent
                        .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
                    p.memory_bytes
                        .map_or_else(|| "-".to_string(), super::human_bytes),
                    super::human_duration(p.uptime_ms),
                    p.fold.clone().unwrap_or_else(|| "-".to_string()),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            "STATUS" => "status",
            "PID" => "pid",
            "RESTARTS" => "restarts",
            "CPU" => "cpu_percent",
            "MEM" => "memory_bytes",
            "UPTIME" => "uptime_ms",
            "FOLD" => "fold",
            other => panic!("FlockRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // Absolute log paths, often longer than every other column put
        // together — a column here would wreck the table `flock` exists to
        // print. They ride the JSON so a programmatic consumer can find a
        // sheep's logs without re-deriving paths the daemon alone resolves.
        "out_file", "err_file",
        // No SOURCE column, because every row this table renders is a
        // sheep — `dog` is always `null` here. A dog gets its own table
        // with its own SOURCE column; this field rides the JSON only so a
        // consumer that switches on `ProcessInfo` shape alone still sees it.
        "dog",
        // Always `null` here: only `Describe` walks for lambs, and `flock`
        // is `ListFlock`. `describe`'s own row type gets a LAMBS rendering
        // in a later task; this list just keeps the shape consistent with
        // every other verb answering `ProcessInfo`.
        "lambs",
    ];
}

/// The dogs half of a flock listing: the `ProcessInfo`s whose `dog` marker
/// is set, rendered by where they came from rather than by their place in
/// the flock.
///
/// No `ID` column, and that is the point of the split rather than an
/// omission: ids reflect spawn order across one registry, so a dog booted
/// alongside the flock lands among the sheep's numbers. Nobody sees that,
/// because the two populations are never rendered together — which is what
/// makes the shared id space cost nothing at the surface.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DogRows(pub Vec<ProcessInfo>);

/// `DogSource`'s table rendering, shared by every payload with a SOURCE
/// column: `DogSource` is `#[non_exhaustive]` (IR-20), so a kind this client
/// predates renders `unknown` rather than failing to compile against a
/// future daemon.
fn dog_source_label(source: &DogSource) -> &'static str {
    match source {
        DogSource::BuiltIn => "built-in",
        DogSource::Adopted { .. } => "adopted",
        _ => "unknown",
    }
}

impl Render for DogRows {
    fn headers() -> &'static [&'static str] {
        &[
            "NAME", "SOURCE", "STATUS", "PID", "RESTARTS", "CPU", "MEM", "UPTIME",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|p| {
                vec![
                    p.name.clone(),
                    // Never the adopted path — see `Self::JSON_ONLY`'s
                    // sibling reasoning on `FlockRows` for why a path stays
                    // out of the table. `None` reads as `-`: this row only
                    // exists because some caller filtered on
                    // `dog.is_some()`, so a `None` here is a caller bug, not
                    // a value this type should panic over.
                    p.dog.as_ref().map_or("-".to_string(), |source| {
                        dog_source_label(source).to_string()
                    }),
                    p.status.to_string(),
                    p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                    p.restarts.to_string(),
                    p.cpu_percent
                        .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
                    p.memory_bytes
                        .map_or_else(|| "-".to_string(), super::human_bytes),
                    super::human_duration(p.uptime_ms),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "SOURCE" => "dog",
            "STATUS" => "status",
            "PID" => "pid",
            "RESTARTS" => "restarts",
            "CPU" => "cpu_percent",
            "MEM" => "memory_bytes",
            "UPTIME" => "uptime_ms",
            other => panic!("DogRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // Ids reflect spawn order across the one registry shared with the
        // sheep half; a dog booted alongside the flock lands among the
        // sheep's own numbers. No column, because the two populations are
        // never rendered together for that number to be compared against —
        // see this type's own doc comment.
        "id",
        // Fold membership is a sheep concept — a dog is supervised, never
        // grouped for a selector to match by fold.
        "fold",
        // Same reason `FlockRows` keeps them out of its own table: absolute
        // paths, often longer than every other column put together. They
        // ride the JSON so a programmatic consumer can still find them.
        "out_file", "err_file",
        // Always `null` here: only `Describe` walks for lambs, and this
        // table renders `ListFlock`'s dog half. A dog is one process by
        // contract, so a lamb tree for one is not a rendering this table
        // needs to grow to cover.
        "lambs",
    ];
}

/// `shep enable <name>`: what the config edit and, if a shepherd is
/// running, the resulting `EnableDog` RPC actually did.
///
/// Constructed by `commands/dogs.rs`'s `enable`, whether or not a shepherd
/// answered — [`Self::shepherd_acted`] and [`Self::status`] are exactly how
/// a `--format json` consumer tells the two outcomes apart without also
/// having to parse a table caption or a stderr notice.
#[derive(Debug, Serialize)]
pub struct DogEnabledRow {
    /// The dog's name.
    pub name: String,
    /// Where its binary comes from, as `commands/dogs.rs`'s `dog_source`
    /// read it out of `shep.toml`: [`DogSource::Adopted`], carrying the
    /// path `shep adopt` recorded, for a name in `[daemon] adopted_dogs`,
    /// and [`DogSource::BuiltIn`] for any name that is not.
    pub source: DogSource,
    /// Whether a shepherd was reached and asked to start the dog. `false`
    /// means only the config changed — decision 11: `enable` never
    /// autostarts a shepherd to act on its own edit.
    pub shepherd_acted: bool,
    /// The dog's resulting status: a real `ProcStatus` rendering
    /// (`"online"`, `"starting"`, ...) when a shepherd started it, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

impl Render for DogEnabledRow {
    fn headers() -> &'static [&'static str] {
        &["NAME", "SOURCE", "SHEPHERD", "STATUS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.name.clone(),
            dog_source_label(&self.source).to_string(),
            self.shepherd_acted.to_string(),
            self.status.clone(),
        ]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "SOURCE" => "source",
            "SHEPHERD" => "shepherd_acted",
            "STATUS" => "status",
            other => panic!("DogEnabledRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `shep disable <name>`: what the config edit and, if a shepherd is
/// running, the resulting `DisableDog` RPC actually did.
///
/// Constructed by `commands/dogs.rs`'s `disable`. [`Self::source`] is not
/// echoed from any RPC reply — `Request::DisableDog` answers
/// `Response::Deleted`, which carries only ids — so it comes from the same
/// `shep.toml` lookup [`DogEnabledRow::source`] uses: an adopted dog
/// reports as adopted here, whichever of the two verbs stopped it.
#[derive(Debug, Serialize)]
pub struct DogDisabledRow {
    /// The dog's name.
    pub name: String,
    /// Where its binary comes from — see this type's own doc.
    pub source: DogSource,
    /// Whether a shepherd was reached and asked to stop the dog.
    pub shepherd_acted: bool,
    /// The dog's resulting status: `"stopped"` when a shepherd acted, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

impl Render for DogDisabledRow {
    fn headers() -> &'static [&'static str] {
        &["NAME", "SOURCE", "SHEPHERD", "STATUS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.name.clone(),
            dog_source_label(&self.source).to_string(),
            self.shepherd_acted.to_string(),
            self.status.clone(),
        ]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "SOURCE" => "source",
            "SHEPHERD" => "shepherd_acted",
            "STATUS" => "status",
            other => panic!("DogDisabledRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `shep adopt <name> <path>`: what the config edit and, if a shepherd is
/// running, the resulting `EnableDog` RPC actually did.
///
/// Constructed by `commands/dogs.rs`'s `adopt`. [`Self::source`] is always
/// [`DogSource::Adopted`] — this is the verb that vetted the path in the
/// first place, so it never has to look one up the way
/// [`DogEnabledRow::source`] does.
#[derive(Debug, Serialize)]
pub struct DogAdoptedRow {
    /// The dog's name.
    pub name: String,
    /// Always [`DogSource::Adopted`], carrying the vetted, canonicalized
    /// path `adopt` just recorded.
    pub source: DogSource,
    /// Whether a shepherd was reached and asked to start the dog. `false`
    /// means only the config changed — decision 11: no verb in this module
    /// autostarts a shepherd to act on its own edit.
    pub shepherd_acted: bool,
    /// The dog's resulting status: a real `ProcStatus` rendering
    /// (`"online"`, `"starting"`, ...) when a shepherd started it, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

impl Render for DogAdoptedRow {
    fn headers() -> &'static [&'static str] {
        &["NAME", "SOURCE", "SHEPHERD", "STATUS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.name.clone(),
            dog_source_label(&self.source).to_string(),
            self.shepherd_acted.to_string(),
            self.status.clone(),
        ]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "SOURCE" => "source",
            "SHEPHERD" => "shepherd_acted",
            "STATUS" => "status",
            other => panic!("DogAdoptedRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `shep rehome <name>`: what the config edit and, if a shepherd is
/// running, the resulting `DisableDog` RPC actually did.
///
/// Constructed by `commands/dogs.rs`'s `rehome`, which reads `shep.toml`'s
/// own `[daemon] adopted_dogs` entry before erasing it — the same lookup
/// [`DogEnabledRow`]/[`DogDisabledRow`] make, except that here it is an
/// [`Option`], because `rehome` reports what it FORGOT and a name it never
/// adopted is nothing forgotten. So this carries whatever that read found:
/// [`DogSource::Adopted`] for a dog `shep adopt` registered, or `None` for
/// a name `shep.toml` never had an entry for (a built-in dog, or a name
/// this document has never heard of) — `rehome` still runs in that case,
/// since forgetting a registration that already does not exist is not a
/// fault.
#[derive(Debug, Serialize)]
pub struct DogRehomedRow {
    /// The dog's name.
    pub name: String,
    /// Where its binary came from, read before this verb forgot it — see
    /// this type's own doc for what `None` means.
    pub source: Option<DogSource>,
    /// Whether a shepherd was reached and asked to stop the dog.
    pub shepherd_acted: bool,
    /// The dog's resulting status: `"stopped"` when a shepherd acted, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

impl Render for DogRehomedRow {
    fn headers() -> &'static [&'static str] {
        &["NAME", "SOURCE", "SHEPHERD", "STATUS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.name.clone(),
            // `-` for `None`, matching `DogRows`' own rule for the same
            // shape of field — see that type's own `rows` for why.
            self.source.as_ref().map_or_else(
                || "-".to_string(),
                |source| dog_source_label(source).to_string(),
            ),
            self.shepherd_acted.to_string(),
            self.status.clone(),
        ]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "SOURCE" => "source",
            "SHEPHERD" => "shepherd_acted",
            "STATUS" => "status",
            other => panic!("DogRehomedRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `Response::Flushed(Vec<ProcessInfo>)` — the sheep a `shep flush` matched,
/// rendered by the FILES it emptied rather than by their lifecycle.
///
/// Constructed by `commands/logs.rs`'s `flush`. Serializes exactly as
/// [`FlockRows`] does, over the same `Vec<ProcessInfo>` and the same
/// `transparent` newtype, so `--format json` is byte-identical to what it
/// answered before this type existed — the paths were always in the JSON.
/// Only the table differs.
///
/// # Why flush gets its own columns
///
/// `flush` is the one verb in the flock-shaped family whose subject is a set
/// of FILES. `out_file`/`err_file` are free-form config taken verbatim, so a
/// mistyped one makes this verb empty something that is not a log at all —
/// and until now the table answered with `STATUS`, `PID`, `RESTARTS`,
/// `UPTIME` and `FOLD`, none of which say what was destroyed. An operator
/// reading a `flush` table wants the blast radius, which is exactly the two
/// columns [`FlockRows`] keeps out of its own table for being too wide.
///
/// The lifecycle fields are still in the JSON (see [`Self::JSON_ONLY`]) —
/// nothing was removed from the payload, only from this verb's columns.
///
/// One row per SHEEP, as `Response::Flushed` is: several sheep can share a
/// log path and the daemon truncates each distinct path once, so the same
/// path can appear twice here. That is honest about what the selector
/// matched, which is what the reply is keyed on.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct FlushedRows(pub Vec<ProcessInfo>);

impl Render for FlushedRows {
    fn headers() -> &'static [&'static str] {
        &["ID", "NAME", "OUT_FILE", "ERR_FILE"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|p| {
                vec![
                    p.id.to_string(),
                    p.name.clone(),
                    // `-` for the same reason `FlockRows` uses it: an empty
                    // cell in a padded table reads as a rendering bug. Here
                    // it means a peer daemon that predates the field, never
                    // a sheep with no log file.
                    p.out_file.clone().unwrap_or_else(|| "-".to_string()),
                    p.err_file.clone().unwrap_or_else(|| "-".to_string()),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            "OUT_FILE" => "out_file",
            "ERR_FILE" => "err_file",
            other => panic!("FlushedRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // A sheep's lifecycle and its resource use, neither of which a flush
        // reads or changes. They stay in the JSON because
        // `Response::Flushed` carries the same `ProcessInfo` every other
        // verb answers with, and a consumer switching on the envelope's
        // `command` should not find the record shape switching with it — but
        // a column each would push the two paths this verb exists to report
        // off the side of a terminal.
        "status",
        "pid",
        "restarts",
        "uptime_ms",
        "fold",
        "cpu_percent",
        "memory_bytes",
        // Same reason `FlockRows` keeps it out of its own table: `flush`
        // matches sheep, so every row here is a sheep and `dog` is always
        // `null`. Stays in the JSON for the same shape-consistency reason
        // the rest of this list does.
        "dog",
        // Always `null` here: only `Describe` walks for lambs, and `flush`
        // is not `Describe`. Same shape-consistency reason as the rest of
        // this list.
        "lambs",
    ];
}

/// One of the shepherd's own log files, and what `shep flush --daemon` made
/// of it.
///
/// Not a `ProcessInfo` and not derived from one: these two files belong to no
/// sheep, have no id and no name, and never travel over the wire — the CLI
/// owns them, empties them itself, and reports what it did. That is the whole
/// reason `--daemon` renders its own payload instead of joining
/// [`FlockRows`].
#[derive(Debug, Serialize)]
pub struct EmptiedFile {
    /// Which of the shepherd's streams this file takes: `stdout` or `stderr`.
    pub stream: &'static str,
    /// The file's absolute path, as this invocation resolved `$SHEP_HOME`.
    pub file: String,
    /// `emptied` when the file was truncated, `absent` when there was no such
    /// file — already empty, and not created just to say so.
    pub result: &'static str,
}

/// `shep flush --daemon`: one row per file the shepherd logs into.
///
/// Constructed by `commands/logs.rs`'s `flush`, from the files it truncated.
/// `transparent` so the JSON is a plain array, matching every other payload
/// that reports a list.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct EmptiedFiles(pub Vec<EmptiedFile>);

impl Render for EmptiedFiles {
    fn headers() -> &'static [&'static str] {
        &["STREAM", "FILE", "RESULT"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|f| vec![f.stream.to_string(), f.file.clone(), f.result.to_string()])
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "STREAM" => "stream",
            "FILE" => "file",
            "RESULT" => "result",
            other => panic!("EmptiedFiles::headers() does not include {other:?}"),
        }
    }

    // Every field is a column. The paths are long, which is the objection
    // `FlockRows` answers by keeping its own two out of the table — but here
    // the path IS the answer: a verb that emptied a file and would not say
    // which one has reported nothing.
    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `Response::Deleted(Vec<u32>)` — the ids that were removed.
///
/// Constructed by `commands/lifecycle.rs`'s `delete`, from a real
/// `Response`.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DeletedIds(pub Vec<u32>);

impl Render for DeletedIds {
    fn headers() -> &'static [&'static str] {
        &["ID"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0.iter().map(|id| vec![id.to_string()]).collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            other => panic!("DeletedIds::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `ping`: the daemon identity the handshake already told us.
///
/// Constructed by `commands/query.rs`'s `ping`, from the real `HelloAck`
/// `Client::daemon` holds.
#[derive(Debug, Serialize)]
pub struct PingRow {
    /// Daemon crate version, read off the handshake `HelloAck`.
    pub daemon_version: String,
    /// Daemon pid, from the same handshake.
    pub pid: u32,
}

impl Render for PingRow {
    fn headers() -> &'static [&'static str] {
        &["DAEMON_VERSION", "PID"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.daemon_version.clone(), self.pid.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "DAEMON_VERSION" => "daemon_version",
            "PID" => "pid",
            other => panic!("PingRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `kill`: what teardown actually achieved.
///
/// Constructed by `commands/admin.rs`'s `kill`, after tearing the daemon
/// down.
#[derive(Debug, Serialize)]
pub struct KillRow {
    /// Daemon pid at the moment of kill, read before the connection dropped.
    pub pid: u32,
    /// Whether the daemon removed its own socket file before exiting.
    pub socket_removed: bool,
}

impl Render for KillRow {
    fn headers() -> &'static [&'static str] {
        &["PID", "SOCKET_REMOVED"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.pid.to_string(), self.socket_removed.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "PID" => "pid",
            "SOCKET_REMOVED" => "socket_removed",
            other => panic!("KillRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `Response::RollSaved` — where the muster roll landed, and what it
/// recorded.
///
/// Constructed by `commands/muster.rs`'s `save`, from a real `Response`.
/// Every field is a column — `JSON_ONLY: &[]` — for [`EmptiedFiles`]' own
/// stated reason: a verb that wrote a file and would not say which one has
/// reported nothing.
#[derive(Debug, Serialize)]
pub struct SavedRollRow {
    /// The roll's path, exactly as the daemon reported it.
    pub file: String,
    /// How many apps that roll records.
    pub apps: u32,
}

impl Render for SavedRollRow {
    fn headers() -> &'static [&'static str] {
        &["FILE", "APPS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.file.clone(), self.apps.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "FILE" => "file",
            "APPS" => "apps",
            other => panic!("SavedRollRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// One app `shep import` read out of a pm2 dump.
///
/// Constructed by `commands/import/mod.rs`'s `import`, from the apps a
/// dump was converted into — not from any wire `Response`, since this verb
/// asks the daemon nothing. `REUSE_PORT` is the column an operator scans
/// for at a glance; `import`'s own stderr notes are where they learn what
/// to do about a `true` one (`shep` binds nothing, so the app itself has to
/// set `SO_REUSEPORT`).
#[derive(Debug, Serialize)]
pub struct ImportRow {
    /// The app's name, which is also the key its instance rows were grouped by.
    pub name: String,
    /// The script the app runs.
    pub script: String,
    /// How many instances of it the dump recorded running.
    pub instances: u32,
    /// Whether the app has to set `SO_REUSEPORT` itself (pm2 cluster mode).
    pub reuse_port: bool,
}

/// `shep import`: one row per app the dump was collapsed into.
///
/// `transparent` so the JSON is a plain array, matching every other payload
/// that reports a list.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct ImportRows(pub Vec<ImportRow>);

impl Render for ImportRows {
    fn headers() -> &'static [&'static str] {
        &["NAME", "SCRIPT", "INSTANCES", "REUSE_PORT"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|row| {
                vec![
                    row.name.clone(),
                    row.script.clone(),
                    row.instances.to_string(),
                    row.reuse_port.to_string(),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "SCRIPT" => "script",
            "INSTANCES" => "instances",
            "REUSE_PORT" => "reuse_port",
            other => panic!("ImportRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// One step `shep startup` or `shep unstartup` took.
///
/// Constructed by `commands/startup/mod.rs`, from the unit file it wrote or
/// removed and the init-system commands it ran — not from any wire
/// `Response`, since neither verb asks the shepherd anything.
#[derive(Debug, Serialize)]
pub struct StartupStep {
    /// What was done: `wrote`, `removed`, `ran`.
    pub action: &'static str,
    /// The file or command it was done to.
    pub target: String,
    /// `ok`, `absent`, or the failure in one line.
    ///
    /// `absent` is the [`EmptiedFile`] spelling, and means the same thing
    /// here: an `unstartup` found no unit to remove, which is the state it
    /// was asked to produce rather than a failure.
    pub result: String,
}

/// `shep startup`/`shep unstartup`: one row per step, in the order the steps
/// were taken.
///
/// Every step is reported even when an earlier one failed — a half-installed
/// unit is worse than a fully-attempted one, and the operator needs every row
/// to know which half. `transparent` so the JSON is a plain array, matching
/// every other payload that reports a list.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct StartupSteps(pub Vec<StartupStep>);

impl Render for StartupSteps {
    fn headers() -> &'static [&'static str] {
        &["ACTION", "TARGET", "RESULT"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|step| {
                vec![
                    step.action.to_string(),
                    step.target.clone(),
                    step.result.clone(),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ACTION" => "action",
            "TARGET" => "target",
            "RESULT" => "result",
            other => panic!("StartupSteps::headers() does not include {other:?}"),
        }
    }

    // Every field is a column, for [`EmptiedFiles`]' own reason: a verb that
    // wrote or removed a system file and would not say which one has
    // reported nothing.
    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `Response::Triggered(Vec<ActionReply>)` — one row per matched sheep, each
/// carrying what happened when the daemon tried to deliver `shep trigger`'s
/// action to it.
///
/// `EmptiedFile`'s own doc gives the reason this exists rather than
/// implementing [`Render`] on [`ActionReply`] directly: the orphan rule
/// forbids it (`ActionReply` is shep-core's), so every payload here is a
/// newtype this crate owns instead.
///
/// `transparent` over `Vec<ActionReply>`, so `--format json` carries every
/// reply exactly as the daemon sent it — `id`, `name`, and the `outcome`
/// object verbatim, `body` included, in full, un-truncated and with
/// embedded newlines intact. The table cannot make the same promise; see
/// [`Self::rows`]'s own doc for why, and for what it does instead.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct TriggeredRows(pub Vec<ActionReply>);

/// A `Replied` body longer than this many `char`s is truncated in the
/// table — never in JSON, where [`TriggeredRows`]'s own doc explains the
/// body always rides whole. Picked to leave room for `ID`/`NAME`/`OUTCOME`
/// on an ordinary terminal without either column doing its own wrapping,
/// which `render_table` does not support.
const TRIGGER_BODY_PREVIEW_CHARS: usize = 80;

impl Render for TriggeredRows {
    fn headers() -> &'static [&'static str] {
        &["ID", "NAME", "OUTCOME", "DETAIL"]
    }

    /// One row per matched sheep. `OUTCOME` is the short, stable kind
    /// (`replied`, `no_channel`, `skipped`, `timed_out` — [`ActionOutcome`]'s
    /// own `kind` tag); `DETAIL` is where the four variants actually differ,
    /// via [`describe_outcome`]:
    ///
    /// - `Replied` — the reply body, through [`preview_body`].
    /// - `NoChannel` — names the config field that would have opened one,
    ///   because nothing else user-facing does (see this crate's own
    ///   `cli.rs` for the same reasoning on `--help`'s side).
    /// - `Skipped` — why: a reload drainee, mid-swap.
    /// - `TimedOut` — why: no reply inside the app's own `action_timeout`.
    ///
    /// # Why `Replied`'s body is collapsed for the table
    ///
    /// `body` is arbitrary, app-chosen text of unknown length — unlike every
    /// other cell this crate renders, nothing bounds it. Two problems, both
    /// [`preview_body`] answers: `render_table` pads every cell in a column
    /// to its widest (`table.rs`), so one sheep answering with a long
    /// diagnostic dump would stretch DETAIL for every row in the table to
    /// match it; and `render_table` writes exactly one line per row
    /// (`write_row`), so an unescaped newline in a body would split that row
    /// across output lines and desync every column beneath it for the rest
    /// of the render. Capping the length and escaping embedded newlines
    /// fixes both without touching what `--format json` carries.
    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|reply| {
                let (outcome, detail) = describe_outcome(&reply.outcome);
                vec![
                    reply.id.to_string(),
                    reply.name.clone(),
                    outcome.to_string(),
                    detail,
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            // Both table columns are read off the one `outcome` object:
            // OUTCOME is its `kind` tag, DETAIL is a rendering of the rest.
            // Neither is a bare echo of a JSON scalar the way every other
            // header here is, which is why both are in `assert_no_drift`'s
            // own `formatted` list rather than compared cell-for-cell.
            "OUTCOME" | "DETAIL" => "outcome",
            other => panic!("TriggeredRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// [`TriggeredRows::rows`]'s per-outcome split: the short, stable `OUTCOME`
/// label and the human `DETAIL` text.
///
/// `ActionOutcome` is `#[non_exhaustive]` (shep-core's own Global
/// Constraints — a future outcome must not need a protocol version bump),
/// so this carries a wildcard arm: a variant this client predates renders
/// as `unknown` with its `Debug` form, rather than failing to compile.
fn describe_outcome(outcome: &ActionOutcome) -> (&'static str, String) {
    match outcome {
        ActionOutcome::Replied { body } => ("replied", preview_body(body)),
        // Names the config field: nothing else user-facing says a trigger
        // needs one, so the row that hits this is the one place an
        // operator learns why. `cli.rs`'s `Trigger` variant doc names it
        // too, on the `--help` side.
        ActionOutcome::NoChannel => (
            "no_channel",
            "no shepherd channel — set channel = true, or wait_ready / \
             shutdown_with_message, which imply it"
                .to_string(),
        ),
        ActionOutcome::Skipped => (
            "skipped",
            "mid-reload — a fresh instance is replacing this one".to_string(),
        ),
        ActionOutcome::TimedOut => (
            "timed_out",
            "no reply within the app's own action_timeout".to_string(),
        ),
        other => ("unknown", format!("{other:?}")),
    }
}

/// Collapses a `Replied` body to one line, capped at
/// [`TRIGGER_BODY_PREVIEW_CHARS`] `char`s — see [`TriggeredRows::rows`]'s
/// own doc for why both are needed. Embedded `\n`/`\r` become the
/// two-character escapes `\n`/`\r` (never a literal newline, which is the
/// thing being escaped); a cap that cuts the body off leaves a trailing
/// `...` so the cell reads as partial rather than complete.
fn preview_body(body: &str) -> String {
    let mut preview = String::new();
    let mut truncated = false;
    for (seen, ch) in body.chars().enumerate() {
        if seen == TRIGGER_BODY_PREVIEW_CHARS {
            truncated = true;
            break;
        }
        match ch {
            '\n' => preview.push_str("\\n"),
            '\r' => preview.push_str("\\r"),
            other => preview.push(other),
        }
    }
    if truncated {
        preview.push_str("...");
    }
    preview
}

/// `Response::Signalled(Vec<SignalReply>)` — one row per matched sheep, each
/// carrying what happened when the shepherd tried to deliver `shep signal`'s
/// signal to it.
///
/// Shaped exactly like [`TriggeredRows`], for the same reason
/// [`SignalReply`]'s own doc gives: a per-row outcome, since spec §9's
/// selector grammar makes a mixed flock the normal case.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct SignalledRows(pub Vec<SignalReply>);

impl Render for SignalledRows {
    fn headers() -> &'static [&'static str] {
        &["ID", "NAME", "OUTCOME", "DETAIL"]
    }

    /// One row per matched sheep. `OUTCOME` is the short, stable kind
    /// (`delivered`, `not_running`, `failed` — [`SignalOutcome`]'s own `kind`
    /// tag); `DETAIL` is where the three variants differ, via
    /// [`describe_signal_outcome`].
    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|reply| {
                let (outcome, detail) = describe_signal_outcome(&reply.outcome);
                vec![
                    reply.id.to_string(),
                    reply.name.clone(),
                    outcome.to_string(),
                    detail,
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            // Both table columns are read off the one `outcome` object, same
            // as `TriggeredRows::json_key_for`'s own reasoning.
            "OUTCOME" | "DETAIL" => "outcome",
            other => panic!("SignalledRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// [`SignalledRows::rows`]'s per-outcome split: the short, stable `OUTCOME`
/// label and the human `DETAIL` text.
///
/// `SignalOutcome` is `#[non_exhaustive]` (shep-core's own Global
/// Constraints), so this carries a wildcard arm: a variant this client
/// predates renders as `unknown` with its `Debug` form, rather than failing
/// to compile.
fn describe_signal_outcome(outcome: &SignalOutcome) -> (&'static str, String) {
    match outcome {
        SignalOutcome::Delivered => ("delivered", String::new()),
        SignalOutcome::NotRunning => ("not_running", "no live process to signal".to_string()),
        SignalOutcome::Failed { reason } => ("failed", reason.clone()),
        other => ("unknown", format!("{other:?}")),
    }
}

/// `Response::SentLine(Vec<LineReply>)` — one row per matched sheep, each
/// carrying what happened when the shepherd tried to write `shep sendline`'s
/// line to its stdin.
///
/// Shaped exactly like [`TriggeredRows`]/[`SignalledRows`], for the same
/// reason [`LineReply`]'s own doc gives: a per-row outcome, since spec §9's
/// selector grammar makes a mixed flock the normal case.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct SentLineRows(pub Vec<LineReply>);

impl Render for SentLineRows {
    fn headers() -> &'static [&'static str] {
        &["ID", "NAME", "OUTCOME", "DETAIL"]
    }

    /// One row per matched sheep. `OUTCOME` is the short, stable kind
    /// (`sent`, `no_stdin`, `not_written` — [`LineOutcome`]'s own `kind`
    /// tag); `DETAIL` is where the three variants differ, via
    /// [`describe_line_outcome`].
    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|reply| {
                let (outcome, detail) = describe_line_outcome(&reply.outcome);
                vec![
                    reply.id.to_string(),
                    reply.name.clone(),
                    outcome.to_string(),
                    detail,
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            // Both table columns are read off the one `outcome` object, same
            // as `TriggeredRows::json_key_for`'s own reasoning.
            "OUTCOME" | "DETAIL" => "outcome",
            other => panic!("SentLineRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// [`SentLineRows::rows`]'s per-outcome split: the short, stable `OUTCOME`
/// label and the human `DETAIL` text.
///
/// `LineOutcome` is `#[non_exhaustive]` (shep-core's own Global
/// Constraints), so this carries a wildcard arm: a variant this client
/// predates renders as `unknown` with its `Debug` form, rather than failing
/// to compile.
fn describe_line_outcome(outcome: &LineOutcome) -> (&'static str, String) {
    match outcome {
        LineOutcome::Sent => ("sent", String::new()),
        // Names the config field, same reasoning as
        // `describe_outcome`'s own `NoChannel` arm: the row an operator hits
        // is the one place they learn why.
        LineOutcome::NoStdin => ("no_stdin", "no stdin pipe — set stdin = true".to_string()),
        LineOutcome::NotWritten { reason } => ("not_written", reason.clone()),
        other => ("unknown", format!("{other:?}")),
    }
}

/// `Vec<Bark>` — `shep barks`' own payload, newest last exactly as it sits
/// on disk (`shep_core::barks::read`'s own order — a ring is appended to,
/// never re-sorted) and as `--tail` counts from.
///
/// `transparent`, matching every other `Vec<T>` payload in this file: the
/// JSON is a plain array, not a wrapper object.
///
/// Never built from a `Response` — `commands/dogs.rs`'s `barks` reads
/// `shep_core::barks::read` straight off `barks.jsonl`, never connecting to
/// the shepherd (that module's own doc: the history is on disk precisely so
/// it survives the shepherd). `Bark` is shep-core's, so this newtype is what
/// the orphan rule requires to implement [`Render`] on it.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct BarkRows(pub Vec<Bark>);

impl Render for BarkRows {
    fn headers() -> &'static [&'static str] {
        &["WHEN", "RULE", "SUBJECT", "MESSAGE", "SINKS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|b| {
                vec![
                    super::local_timestamp(b.at_ms),
                    b.rule.clone(),
                    b.subject.clone(),
                    b.message.clone(),
                    sinks_cell(&b.sinks),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "WHEN" => "at_ms",
            "RULE" => "rule",
            "SUBJECT" => "subject",
            "MESSAGE" => "message",
            "SINKS" => "sinks",
            other => panic!("BarkRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// Renders one [`Bark::sinks`] list for the `SINKS` column: a delivered sink
/// by its bare name (`ops`), a refused one with `(failed)` appended
/// (`ops(failed)`) so the failure is visible in the table an operator is
/// already reading rather than only in `--format json`'s `error` field —
/// and never the sink's own error text, which can quote a webhook's HTTP
/// response and would widen an already-tight column for a detail
/// `--format json` already carries in full.
///
/// `-` for an empty list: [`Bark::sinks`]'s own doc says empty means the
/// shepherd wrote the record itself, with no sinks and no webhook code — the
/// same "no honest value" case every other `-` cell in this file marks,
/// never a delivery this dog attempted and lost track of.
fn sinks_cell(sinks: &[SinkOutcome]) -> String {
    if sinks.is_empty() {
        return "-".to_string();
    }
    sinks
        .iter()
        .map(|outcome| {
            if outcome.error.is_some() {
                format!("{}(failed)", outcome.sink)
            } else {
                outcome.sink.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One row of `shep get`'s whole-store listing.
///
/// A named-field struct rather than a bare `(String, String)` tuple: a tuple
/// serializes to a JSON array (`["a","1"]`), and the store's own design
/// decision (Task 13's spec) is that the payload is "a list of objects
/// rather than a JSON map" — a tuple's array-of-arrays shape is neither, and
/// would make every consumer index into position 0/1 instead of reading a
/// `key`/`value` field.
#[derive(Debug, Serialize)]
pub struct KvEntry {
    /// The key, exactly as stored — already validated by
    /// [`shep_core::kv`]'s grammar by the time this is constructed.
    pub key: String,
    /// Its value.
    pub value: String,
}

/// `shep get`'s whole-store listing (bare `shep get`), or one key's own
/// entry (`shep get <key>`).
///
/// `transparent`, matching every other `Vec<T>` payload in this file: the
/// JSON is a plain array of [`KvEntry`] objects, not a wrapper object —
/// the envelope's `data` is an array for every other verb in this binary,
/// and a KV listing answering with a JSON map would be the one payload a
/// consumer has to special-case.
///
/// Constructed by `commands/kv.rs`, from `shep_core::kv::all`/`kv::get` —
/// never from a `Response`: the store never touches the wire (Task 12's own
/// doc).
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct KvRows(pub Vec<KvEntry>);

impl Render for KvRows {
    fn headers() -> &'static [&'static str] {
        &["KEY", "VALUE"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|entry| vec![entry.key.clone(), entry.value.clone()])
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "KEY" => "key",
            "VALUE" => "value",
            other => panic!("KvRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `shep unset`'s own report: how many keys the store lost.
///
/// A count rather than the removed keys themselves: `shep_core::kv::clear`
/// hands back only how many entries it dropped, not which ones — the store
/// never materializes the full set it is about to empty just to name it in
/// a report — so a single key's success and `--all`'s share this one shape
/// rather than two.
///
/// Constructed by `commands/kv.rs`'s `unset`.
#[derive(Debug, Serialize)]
pub struct KvUnsetRow {
    /// How many keys were removed: always `1` for a single-key `unset`
    /// (a key that was not there exits [`crate::exit::ExitCode::NotFound`]
    /// before this is ever built), and `shep_core::kv::clear`'s own count
    /// for `--all`.
    pub removed: u32,
}

impl Render for KvUnsetRow {
    fn headers() -> &'static [&'static str] {
        &["REMOVED"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.removed.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "REMOVED" => "removed",
            other => panic!("KvUnsetRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeSet;

    use shep_core::status::ProcStatus;

    use super::*;

    pub(crate) fn sample_info(id: u32, name: &str, uptime_ms: u64) -> ProcessInfo {
        // Every `Option` field `Some`: `flock_rows_do_not_drift` below pins
        // each cell against its own JSON value, and a `None` serializes as
        // `null`, which that check skips rather than compares — so a field
        // left empty here is a column the drift test stops watching. `dog`
        // is the one exception, left at the builder's `None` default: it is
        // `JSON_ONLY` (see `FlockRows::JSON_ONLY`), not a column, so
        // `assert_no_drift`'s cell check never reads it — and `None` is the
        // honest value besides, since every row `sample_flock` builds is a
        // sheep.
        ProcessInfo::builder(id, name, ProcStatus::Online)
            .pid(Some(1000 + id))
            .restarts(id)
            .uptime_ms(uptime_ms)
            .fold(Some("backend".to_string()))
            .out_file(Some(format!("/logs/{name}-0-out.log")))
            .err_file(Some(format!("/logs/{name}-0-err.log")))
            // Fixed rather than id-derived, like `fold` above: every sample
            // sheep shares one reading. `memory_bytes` is the same value
            // `human_bytes`'s own doc uses to show it is not `MemSize`'s
            // `Display` — 50 462 720 bytes is not a round number of MiB, and
            // rendering it as "48.1M" is the whole point of that function.
            .cpu_percent(Some(12.5))
            .memory_bytes(Some(50_462_720))
            .build()
    }

    /// Three fully-populated sheep, shared by every test in this module and
    /// by `output`'s own envelope/emit tests.
    pub(crate) fn sample_flock() -> FlockRows {
        FlockRows(vec![
            sample_info(1, "web", 60_000),
            sample_info(2, "worker", 120_000),
            sample_info(3, "cron", 30_000),
        ])
    }

    pub(crate) fn info_with_uptime_ms(uptime_ms: u64) -> ProcessInfo {
        sample_info(1, "web", uptime_ms)
    }

    /// A dog-shaped `ProcessInfo`: `sample_info` with `dog` set to `source`.
    /// The id is fixed rather than threaded through as a parameter, like
    /// `fold`/`cpu_percent` in `sample_info` itself — `DogRows` has no `ID`
    /// column (its own doc comment says why), so no test needs one that
    /// varies. `pub(crate)` so `output::mod`'s own tests can build a mixed
    /// sheep-and-dog listing without a second copy of this helper.
    pub(crate) fn dog_info(name: &str, source: DogSource) -> ProcessInfo {
        let mut info = sample_info(1, name, 60_000);
        info.dog = Some(source);
        info
    }

    /// The anti-drift gate, written once and instantiated three times — once
    /// per payload type with JSON object keys (`DeletedIds` has none — see
    /// its own test below), per this task's own rule.
    ///
    /// Three checks, each catching a mutation the other two miss:
    /// 1. Serializes a fully-populated value, collects its JSON object keys,
    ///    and asserts they match `headers()` after `json_key_for`, so a
    ///    field added to `Serialize` and forgotten in `rows()` fails here
    ///    rather than silently vanishing from the table.
    /// 2. Every row's cell count must equal `headers().len()` — a dropped or
    ///    added cell shifts every later column without changing the row
    ///    *count*, which `table_and_json_report_the_same_record_count`
    ///    checks but this doesn't.
    /// 3. The first row's cell for each non-`formatted` header is pinned
    ///    against that same field's own JSON value — a cell-count check
    ///    alone cannot see two same-arity cells swapped (e.g. NAME and
    ///    STATUS trading places).
    ///
    /// `formatted` lists headers whose table cell is a human-only rendering
    /// of the field rather than the field's raw value (`FlockRows`'s
    /// `UPTIME`, formatted by `human_duration` — see `table.rs`'s own tests
    /// for that formatting's coverage instead). Comparing those cells
    /// against the raw JSON value would either duplicate that formatting
    /// here or spuriously fail; every other header IS compared cell-for-cell,
    /// which is what actually catches a swap.
    fn assert_no_drift<T: Render>(
        value: &T,
        first_record: fn(&serde_json::Value) -> &serde_json::Value,
        formatted: &[&str],
    ) {
        let json = serde_json::to_value(value).unwrap();
        let record = first_record(&json);
        let keys: BTreeSet<&str> = record
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();

        let covered: BTreeSet<&str> = T::headers()
            .iter()
            .map(|h| T::json_key_for(h))
            .chain(T::JSON_ONLY.iter().copied())
            .collect();

        assert_eq!(
            keys, covered,
            "a serialized field is a column, or it is in JSON_ONLY with a reason — never neither"
        );

        let rows = value.rows();
        for row in &rows {
            assert_eq!(
                row.len(),
                T::headers().len(),
                "a row has {} cells but headers() has {} — a dropped or added cell changes no \
                 row *count*, so table_and_json_report_the_same_record_count would miss it",
                row.len(),
                T::headers().len(),
            );
        }

        let Some(row) = rows.first() else {
            return;
        };
        for (i, header) in T::headers().iter().enumerate() {
            if formatted.contains(header) {
                continue;
            }
            let key = T::json_key_for(header);
            let expected = match &record[key] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                // Not exercised by today's fully-populated fixtures (see
                // `sample_info`'s own comment on why every `Option` is
                // `Some`); skipped rather than panicking so a future
                // `None`-carrying fixture doesn't fail here for an unrelated
                // reason.
                serde_json::Value::Null => continue,
                other => panic!(
                    "{header} ({key}) serialized to {other:?}; teach this match how to \
                     stringify it, or add {header} to `formatted`"
                ),
            };
            assert_eq!(
                row[i], expected,
                "{header} cell does not match its own JSON field {key:?} — swapped or \
                 substituted with a neighbouring column?"
            );
        }
    }

    #[test]
    fn flock_rows_do_not_drift() {
        // UPTIME, CPU and MEM are formatted (`human_duration`/`human_bytes`),
        // not raw echoes of `uptime_ms`/`cpu_percent`/`memory_bytes` — see
        // the doc comment on `assert_no_drift` above.
        assert_no_drift(&sample_flock(), |j| &j[0], &["UPTIME", "CPU", "MEM"]);
    }

    /// fails if `SOURCE` renders the adopted binary's path into the table.
    /// A path is wider than every other column combined and would push
    /// UPTIME off a terminal — the same reason `FlockRows` keeps the log
    /// paths out of its own table, and the path is still one `--format
    /// json` away.
    #[test]
    fn the_source_column_names_a_kind_and_leaves_the_path_to_json() {
        let rows = DogRows(vec![
            dog_info("metrics", DogSource::BuiltIn),
            dog_info(
                "otel",
                DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            ),
        ]);
        let headers = DogRows::headers();
        let at = |cells: &[String], h: &str| {
            cells[headers.iter().position(|x| *x == h).unwrap()].clone()
        };
        assert_eq!(at(&rows.rows()[0], "SOURCE"), "built-in");
        assert_eq!(at(&rows.rows()[1], "SOURCE"), "adopted");

        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json[1]["dog"]["path"], "/usr/local/bin/shep-otel");
    }

    /// The anti-drift gate for this type. Fails if a `ProcessInfo` field is
    /// serialized with neither a column nor a `JSON_ONLY` entry.
    ///
    /// `SOURCE` joins `formatted` alongside `UPTIME`/`CPU`/`MEM`: its own
    /// JSON value is the tagged `DogSource` object (`{"kind": "built_in"}`
    /// or `{"kind": "adopted", "path": ...}`), not a plain string this
    /// gate's cell comparison knows how to stringify — the test above pins
    /// that mapping instead.
    #[test]
    fn dog_rows_do_not_drift() {
        assert_no_drift(
            &DogRows(vec![dog_info("metrics", DogSource::BuiltIn)]),
            |j| &j[0],
            &["UPTIME", "CPU", "MEM", "SOURCE"],
        );
    }

    /// fails if `DogEnabledRow` grows a field that never reaches the table —
    /// the same gate every other payload has. `SOURCE` is `formatted` for
    /// the same reason `dog_rows_do_not_drift` gives.
    #[test]
    fn dog_enabled_row_does_not_drift() {
        assert_no_drift(
            &DogEnabledRow {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
                shepherd_acted: true,
                status: "online".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// The other side of `dog_enabled_row_does_not_drift`, for the payload
    /// `disable` renders.
    #[test]
    fn dog_disabled_row_does_not_drift() {
        assert_no_drift(
            &DogDisabledRow {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
                shepherd_acted: false,
                status: "not running; will not start with the next shepherd".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// The `adopt` sibling of `dog_enabled_row_does_not_drift` — `SOURCE`
    /// is `formatted` for the same reason: it serializes to the tagged
    /// `DogSource` object, not a plain string.
    #[test]
    fn dog_adopted_row_does_not_drift() {
        assert_no_drift(
            &DogAdoptedRow {
                name: "otel".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
                shepherd_acted: true,
                status: "online".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// The `rehome` sibling, exercised once with a recorded source (the
    /// ordinary case: forgetting a dog `adopt` registered) and once with
    /// `None` (rehoming a name `shep.toml` never had an `adopted_dogs`
    /// entry for) — `assert_no_drift`'s own `Value::Null` branch is what
    /// lets the second case pass without `SOURCE` needing to be
    /// `formatted` for it too.
    #[test]
    fn dog_rehomed_row_does_not_drift_with_or_without_a_source() {
        assert_no_drift(
            &DogRehomedRow {
                name: "otel".to_string(),
                source: Some(DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                }),
                shepherd_acted: true,
                status: "stopped".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
        assert_no_drift(
            &DogRehomedRow {
                name: "ghost".to_string(),
                source: None,
                shepherd_acted: false,
                status: "not running; will not start with the next shepherd".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// fails if a sheep with no reading renders an empty cell or a zero. A
    /// zero is a claim — "this sheep is using no CPU" — and the daemon says
    /// `None` precisely when it cannot make that claim.
    #[test]
    fn a_sheep_with_no_reading_renders_a_dash_not_a_zero() {
        let mut info = sample_info(1, "web", 60_000);
        info.cpu_percent = None;
        info.memory_bytes = None;
        let rows = FlockRows(vec![info]);
        let cells = &rows.rows()[0];
        let headers = FlockRows::headers();
        let cpu = cells[headers.iter().position(|h| *h == "CPU").unwrap()].clone();
        let mem = cells[headers.iter().position(|h| *h == "MEM").unwrap()].clone();
        assert_eq!(cpu, "-");
        assert_eq!(mem, "-");
    }

    #[test]
    fn ping_row_does_not_drift() {
        assert_no_drift(
            &PingRow {
                daemon_version: "9.9.9".into(),
                pid: 4242,
            },
            |j| j,
            &[],
        );
    }

    /// Fails if a `ProcessInfo` field goes missing from both the columns and
    /// [`FlushedRows::JSON_ONLY`] — the same gate every other payload has.
    /// The lifecycle keys are only allowed off the table because they are
    /// named there, with a reason, rather than silently dropped.
    #[test]
    fn flushed_rows_do_not_drift() {
        assert_no_drift(&FlushedRows(sample_flock().0), |j| &j[0], &[]);
    }

    /// Fails if `flush` and the other flock-shaped verbs stop agreeing on the
    /// record, which is what would make an operator's `--format json` parser
    /// need a special case keyed on the envelope's `command`.
    ///
    /// Two payload types over one `Vec<ProcessInfo>` is a shape that invites
    /// exactly that drift: a field added to one impl's `Serialize` and not
    /// the other, or a `transparent` dropped from one of them, changes the
    /// JSON for `flush` alone. Each type's own drift test would still pass —
    /// they check a type against itself. Only comparing the two catches it.
    #[test]
    fn a_flush_serializes_the_same_record_the_other_flock_verbs_do() {
        let flock = serde_json::to_value(sample_flock()).unwrap();
        let flushed = serde_json::to_value(FlushedRows(sample_flock().0)).unwrap();
        assert_eq!(
            flock, flushed,
            "the table may differ between these two verbs; the JSON payload may not"
        );
    }

    #[test]
    fn emptied_files_do_not_drift() {
        assert_no_drift(
            &EmptiedFiles(vec![
                EmptiedFile {
                    stream: "stdout",
                    file: "/home/x/.shep/logs/shepd.out.log".to_string(),
                    result: "emptied",
                },
                EmptiedFile {
                    stream: "stderr",
                    file: "/home/x/.shep/logs/shepd.err.log".to_string(),
                    result: "absent",
                },
            ]),
            |j| &j[0],
            &[],
        );
    }

    #[test]
    fn kill_row_does_not_drift() {
        assert_no_drift(
            &KillRow {
                pid: 4242,
                socket_removed: true,
            },
            |j| j,
            &[],
        );
    }

    /// fails if `SavedRollRow` grows a field that never reaches the table —
    /// the same gate `flock_rows_do_not_drift` applies, instantiated for a
    /// payload whose every field is a column.
    #[test]
    fn saved_roll_row_does_not_drift() {
        let row = SavedRollRow {
            file: "/home/rin/.shep/flock.json".to_string(),
            apps: 9,
        };
        assert_no_drift(&row, |json| json, &[]);
    }

    /// fails if `ImportRow` grows a field that never reaches the table —
    /// the same gate every other payload has.
    #[test]
    fn import_rows_do_not_drift() {
        assert_no_drift(
            &ImportRows(vec![
                ImportRow {
                    name: "api".to_string(),
                    script: "/srv/api/dist/server.js".to_string(),
                    instances: 2,
                    reuse_port: true,
                },
                ImportRow {
                    name: "worker".to_string(),
                    script: "/srv/worker/dist/worker.js".to_string(),
                    instances: 1,
                    reuse_port: false,
                },
            ]),
            |j| &j[0],
            &[],
        );
    }

    /// fails if `StartupStep` grows a field that never reaches the table —
    /// the same gate every other payload has. The two rows cover both
    /// shapes the payload carries: a file that was written, and a command
    /// that was run and failed.
    #[test]
    fn startup_steps_do_not_drift() {
        assert_no_drift(
            &StartupSteps(vec![
                StartupStep {
                    action: "wrote",
                    target: "/etc/systemd/system/shep-deploy.service".to_string(),
                    result: "ok".to_string(),
                },
                StartupStep {
                    action: "ran",
                    target: "systemctl enable --now shep-deploy.service".to_string(),
                    result: "Failed to enable unit: Unit file is masked.".to_string(),
                },
            ]),
            |j| &j[0],
            &[],
        );
    }

    /// `DeletedIds` is `#[serde(transparent)]` over `Vec<u32>`, so it
    /// serializes as a bare JSON array of numbers with no object keys —
    /// `assert_no_drift`'s key-set comparison has nothing to compare
    /// against, and `json_key_for("ID") -> "id"` names a key that never
    /// exists in this type's JSON at all. This test is `DeletedIds`'s drift
    /// coverage instead: it pins each row's one cell against the array
    /// element at the same position, so a `rows()` that dropped, reordered,
    /// or mis-rendered an id still fails.
    #[test]
    fn deleted_ids_rows_match_their_own_json_values() {
        let ids = DeletedIds(vec![10, 20, 30]);
        let json = serde_json::to_value(&ids).unwrap();
        let array = json.as_array().unwrap();
        let rows = ids.rows();

        assert_eq!(rows.len(), array.len());
        for (row, value) in rows.iter().zip(array) {
            assert_eq!(row.len(), 1, "DeletedIds::headers() has exactly one column");
            assert_eq!(row[0], value.to_string());
        }
    }

    #[test]
    fn table_and_json_report_the_same_record_count() {
        let rows = sample_flock(); // three sheep
        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 3);
        assert_eq!(
            rows.rows().len(),
            3,
            "the two renderings must never disagree on how many records exist"
        );

        let ids = DeletedIds(vec![1, 2, 3, 4]);
        assert_eq!(
            serde_json::to_value(&ids)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(ids.rows().len(), 4);
    }

    fn sample_replies() -> TriggeredRows {
        TriggeredRows(vec![
            ActionReply {
                id: 1,
                name: "web".to_string(),
                outcome: ActionOutcome::Replied {
                    body: "pong".to_string(),
                },
            },
            ActionReply {
                id: 2,
                name: "worker".to_string(),
                outcome: ActionOutcome::NoChannel,
            },
        ])
    }

    /// OUTCOME and DETAIL both derive from `outcome`, a nested JSON object
    /// rather than a scalar — the reason both are in `assert_no_drift`'s own
    /// `formatted` list, per this fn's doc on the third check it otherwise
    /// runs. What this still catches: a field added to `ActionReply`'s
    /// `Serialize` (or `ActionOutcome`'s) with no column and no `JSON_ONLY`
    /// entry, and a row whose cell count drifts from `headers()`'s.
    #[test]
    fn triggered_rows_do_not_drift() {
        assert_no_drift(&sample_replies(), |j| &j[0], &["OUTCOME", "DETAIL"]);
    }

    #[test]
    fn triggered_rows_render_id_name_and_outcome_kind() {
        let rows = sample_replies().rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "1");
        assert_eq!(rows[0][1], "web");
        assert_eq!(rows[0][2], "replied");
        assert_eq!(rows[1][0], "2");
        assert_eq!(rows[1][1], "worker");
        assert_eq!(rows[1][2], "no_channel");
    }

    /// An operator reading a `no_channel` row must find the config field
    /// that would have avoided it, right there in the row — not just in
    /// `--help`.
    #[test]
    fn a_no_channel_detail_names_the_config_field() {
        let rows = sample_replies().rows();
        let detail = &rows[1][3];
        assert!(
            detail.contains("channel = true"),
            "a no_channel row must name the field that opens one: {detail}"
        );
        assert!(
            detail.contains("wait_ready") && detail.contains("shutdown_with_message"),
            "and the two fields that imply it: {detail}"
        );
    }

    #[test]
    fn skipped_and_timed_out_details_say_why() {
        let skipped = describe_outcome(&ActionOutcome::Skipped).1;
        assert!(skipped.to_lowercase().contains("reload"), "{skipped}");

        let timed_out = describe_outcome(&ActionOutcome::TimedOut).1;
        assert!(
            timed_out.to_lowercase().contains("action_timeout"),
            "{timed_out}"
        );
    }

    #[test]
    fn a_short_single_line_body_previews_unchanged() {
        assert_eq!(preview_body("pong"), "pong");
    }

    /// A body exactly at the cap is not truncated — only a body that has a
    /// character *past* it is, per [`preview_body`]'s own `seen ==
    /// TRIGGER_BODY_PREVIEW_CHARS` check firing one character too late for
    /// an exact-length body to reach it.
    #[test]
    fn a_body_exactly_at_the_cap_is_not_truncated() {
        let exact = "x".repeat(TRIGGER_BODY_PREVIEW_CHARS);
        assert_eq!(preview_body(&exact), exact);
    }

    #[test]
    fn a_body_past_the_cap_is_truncated_with_a_trailing_marker() {
        let over = "x".repeat(TRIGGER_BODY_PREVIEW_CHARS + 1);
        let preview = preview_body(&over);
        let expected = "x".repeat(TRIGGER_BODY_PREVIEW_CHARS) + "...";
        assert_eq!(preview, expected);
    }

    /// A multi-line body would otherwise split a table row across output
    /// lines (`TriggeredRows::rows`'s own doc) — this pins that an embedded
    /// newline never reaches the table cell as a literal newline.
    #[test]
    fn embedded_newlines_and_carriage_returns_are_escaped_not_literal() {
        let preview = preview_body("line one\nline two\r\nline three");
        assert!(!preview.contains('\n'));
        assert!(!preview.contains('\r'));
        assert!(preview.contains("\\n"));
        assert!(preview.contains("\\r"));
    }

    /// `--format json` carries the real body verbatim — untruncated and with
    /// real newlines — even though the table cell for the same row is
    /// collapsed. This is the assertion that would fail if truncation or
    /// escaping ever leaked into `Serialize` instead of staying in
    /// [`TriggeredRows::rows`] alone.
    #[test]
    fn json_carries_the_real_body_the_table_cannot() {
        let long_body = format!(
            "{}\nsecond line",
            "x".repeat(TRIGGER_BODY_PREVIEW_CHARS * 2)
        );
        let replies = TriggeredRows(vec![ActionReply {
            id: 1,
            name: "web".to_string(),
            outcome: ActionOutcome::Replied {
                body: long_body.clone(),
            },
        }]);
        let json = serde_json::to_value(&replies).unwrap();
        assert_eq!(json[0]["outcome"]["body"], long_body);

        let table_cell = &replies.rows()[0][3];
        assert_ne!(
            *table_cell, long_body,
            "the table cell must be the collapsed preview, not the real body"
        );
    }

    fn sample_signal_replies() -> SignalledRows {
        SignalledRows(vec![
            SignalReply {
                id: 1,
                name: "web".to_string(),
                outcome: SignalOutcome::Delivered,
            },
            SignalReply {
                id: 2,
                name: "worker".to_string(),
                outcome: SignalOutcome::NotRunning,
            },
        ])
    }

    /// OUTCOME and DETAIL both derive from `outcome`, a nested JSON object
    /// rather than a scalar — same reasoning as `triggered_rows_do_not_drift`.
    #[test]
    fn signalled_rows_do_not_drift() {
        assert_no_drift(&sample_signal_replies(), |j| &j[0], &["OUTCOME", "DETAIL"]);
    }

    #[test]
    fn signalled_rows_render_id_name_and_outcome_kind() {
        let rows = sample_signal_replies().rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "1");
        assert_eq!(rows[0][1], "web");
        assert_eq!(rows[0][2], "delivered");
        assert_eq!(rows[1][0], "2");
        assert_eq!(rows[1][1], "worker");
        assert_eq!(rows[1][2], "not_running");
    }

    #[test]
    fn a_failed_signal_details_the_kernels_reason() {
        let rows = SignalledRows(vec![SignalReply {
            id: 1,
            name: "web".to_string(),
            outcome: SignalOutcome::Failed {
                reason: "No such process".to_string(),
            },
        }])
        .rows();
        assert_eq!(rows[0][2], "failed");
        assert_eq!(rows[0][3], "No such process");
    }

    fn sample_line_replies() -> SentLineRows {
        SentLineRows(vec![
            LineReply {
                id: 1,
                name: "repl".to_string(),
                outcome: LineOutcome::Sent,
            },
            LineReply {
                id: 2,
                name: "worker".to_string(),
                outcome: LineOutcome::NoStdin,
            },
        ])
    }

    /// OUTCOME and DETAIL both derive from `outcome`, a nested JSON object
    /// rather than a scalar — same reasoning as `triggered_rows_do_not_drift`.
    #[test]
    fn sent_line_rows_do_not_drift() {
        assert_no_drift(&sample_line_replies(), |j| &j[0], &["OUTCOME", "DETAIL"]);
    }

    #[test]
    fn sent_line_rows_render_id_name_and_outcome_kind() {
        let rows = sample_line_replies().rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "1");
        assert_eq!(rows[0][1], "repl");
        assert_eq!(rows[0][2], "sent");
        assert_eq!(rows[1][0], "2");
        assert_eq!(rows[1][1], "worker");
        assert_eq!(rows[1][2], "no_stdin");
    }

    /// An operator reading a `no_stdin` row must find the config field that
    /// would have avoided it, right there in the row — not just in
    /// `--help`, the same rule `a_no_channel_detail_names_the_config_field`
    /// pins for `trigger`.
    #[test]
    fn a_no_stdin_detail_names_the_config_field() {
        let rows = sample_line_replies().rows();
        let detail = &rows[1][3];
        assert!(
            detail.contains("stdin = true"),
            "a no_stdin row must name the field that opens one: {detail}"
        );
    }

    #[test]
    fn a_not_written_line_details_the_reason() {
        let rows = SentLineRows(vec![LineReply {
            id: 1,
            name: "repl".to_string(),
            outcome: LineOutcome::NotWritten {
                reason: "pipe is full".to_string(),
            },
        }])
        .rows();
        assert_eq!(rows[0][2], "not_written");
        assert_eq!(rows[0][3], "pipe is full");
    }

    /// Two barks: one the bark dog delivered to a live sink, one it
    /// refused, and one the shepherd wrote itself with no sinks at all —
    /// [`sinks_cell`]'s three cases in one fixture, shared by every test
    /// below.
    fn sample_barks() -> BarkRows {
        BarkRows(vec![
            Bark {
                at_ms: 1_700_000_000_000,
                rule: "restart-storm".to_string(),
                subject: "web".to_string(),
                message: "3 restarts in 60s".to_string(),
                sinks: vec![SinkOutcome {
                    sink: "ops".to_string(),
                    error: None,
                }],
            },
            Bark {
                at_ms: 1_700_000_060_000,
                rule: "daemon".to_string(),
                subject: "worker".to_string(),
                message: "restart budget exhausted".to_string(),
                sinks: vec![],
            },
        ])
    }

    /// fails if `BarkRows` grows a field that never reaches the table —
    /// the same gate every other payload has. `WHEN` and `SINKS` are both
    /// human renderings of their own JSON field (a formatted timestamp, and
    /// a delivered/failed label rather than the raw `SinkOutcome` array), so
    /// both sit in `formatted` for the reason `assert_no_drift`'s own doc
    /// gives.
    #[test]
    fn bark_rows_do_not_drift() {
        assert_no_drift(&sample_barks(), |j| &j[0], &["WHEN", "SINKS"]);
    }

    /// `sinks_cell`'s own coverage: a delivered sink renders bare, a refused
    /// one carries `(failed)`, and a shepherd-authored bark with no sinks at
    /// all renders `-` rather than an empty cell — the same "no honest
    /// value" rule every other `-` cell in this file follows.
    #[test]
    fn sinks_render_delivered_failed_and_empty() {
        let delivered = Bark {
            sinks: vec![SinkOutcome {
                sink: "ops".to_string(),
                error: None,
            }],
            ..sample_barks().0[0].clone()
        };
        assert_eq!(sinks_cell(&delivered.sinks), "ops");

        let failed = Bark {
            sinks: vec![SinkOutcome {
                sink: "ops".to_string(),
                error: Some("connection refused".to_string()),
            }],
            ..sample_barks().0[0].clone()
        };
        assert_eq!(sinks_cell(&failed.sinks), "ops(failed)");

        assert_eq!(sinks_cell(&[]), "-");
    }

    /// Multiple sinks on one bark render as a comma-separated list, each
    /// carrying its own delivered/failed label independently — the shape a
    /// bark fanned out to more than one `[dog.bark.sinks]` entry actually
    /// has.
    #[test]
    fn multiple_sinks_each_carry_their_own_outcome() {
        let sinks = vec![
            SinkOutcome {
                sink: "ops".to_string(),
                error: None,
            },
            SinkOutcome {
                sink: "oncall".to_string(),
                error: Some("timed out".to_string()),
            },
        ];
        assert_eq!(sinks_cell(&sinks), "ops, oncall(failed)");
    }

    /// A sink's error text is never a bare word alone — this pins that the
    /// cell carries no more than the sink's own name plus `(failed)`, never
    /// the error string itself, which can quote a webhook's HTTP response.
    #[test]
    fn a_failed_sinks_error_text_never_reaches_the_cell() {
        let sinks = vec![SinkOutcome {
            sink: "ops".to_string(),
            error: Some("HTTP 401 from discord.com/api/webhooks/...".to_string()),
        }];
        let cell = sinks_cell(&sinks);
        assert_eq!(cell, "ops(failed)");
        assert!(
            !cell.contains("401") && !cell.contains("discord"),
            "the error text must stay out of the table cell: {cell}"
        );
    }

    /// `shep barks` is newest-last, matching the file on disk
    /// (`shep_core::barks::read`'s own order) — this pins that `rows()`
    /// preserves that order rather than reversing or re-sorting it.
    #[test]
    fn bark_rows_stay_in_the_order_they_were_given() {
        let rows = sample_barks().rows();
        assert_eq!(rows[0][2], "web", "the older bark stays first");
        assert_eq!(rows[1][2], "worker", "the newer bark stays last");
    }

    /// fails if `KvRows` grows a field that never reaches the table — the
    /// same gate every other payload has. Neither column is a formatted
    /// rendering of anything else, so `formatted` is empty.
    #[test]
    fn kv_rows_do_not_drift() {
        let rows = KvRows(vec![KvEntry {
            key: "bark.cooldown".to_string(),
            value: "30s".to_string(),
        }]);
        assert_no_drift(&rows, |j| &j[0], &[]);
    }

    /// fails if `KvUnsetRow` grows a field that never reaches the table —
    /// the same gate every other payload has.
    #[test]
    fn kv_unset_row_does_not_drift() {
        assert_no_drift(&KvUnsetRow { removed: 2 }, |j| j, &[]);
    }
}
