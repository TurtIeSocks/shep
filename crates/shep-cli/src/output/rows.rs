//! Every rendered payload type in the binary, and the [`Render`] impl that
//! makes each one's table and JSON renderings the same source of truth.
//!
//! Payload types live here, not under `commands/`, and that is load-bearing
//! rather than tidy: this module is pure tier and its own tests (below) name
//! every one of these types directly. A payload type defined under
//! `commands/` (`#[cfg(unix)]`) could not be named by a test running on the
//! Windows leg at all, and `commands/query.rs` (a later task) does not exist
//! yet for a test here to depend on regardless. Every type below is built
//! entirely from `ProcessInfo` / `u32` / `crate::dog_index::AvailableDog`,
//! and none of those three carries a `cfg` of any kind, so this really is
//! pure tier.

use serde::Serialize;
use shep_core::barks::{Bark, SinkOutcome};
use shep_core::protocol::{
    ActionOutcome, ActionReply, DogSource, ExitInfo, Lamb, LineOutcome, LineReply, ProcessInfo,
    SignalOutcome, SignalReply,
};
use shep_core::status::ProcStatus;

use crate::dog_index::AvailableDog;
use crate::style::Presentation;
use crate::vocabulary::{self, Role};

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
            "ID", "NAME", "STATUS", "PID", "RESTARTS", "EXIT", "CPU", "MEM", "UPTIME", "FOLD",
            "SMIT",
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
                    exit_cell(p.pid, p.last_exit),
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
                    p.smit.clone().unwrap_or_else(|| "-".to_owned()),
                ]
            })
            .collect()
    }

    /// [`Self::rows`], with every cell the governing rule ("every colour
    /// must carry information") allows dressed up:
    ///
    /// - STATUS (index 2): the face always, when `presentation.level.sheep()`;
    ///   the word too, when `status_word`; the whole cell coloured per
    ///   [`status_cell`].
    /// - ID and FOLD: always `Role::Ink3` -- chrome, the way `pm2` dims its
    ///   namespace column, never a fact about the row.
    /// - PID and SMIT: `Role::Ink3` only when the cell reads `-` -- an
    ///   absent value should not shout as loud as a real one.
    /// - RESTARTS: `Role::Ink3` at zero, `Role::Butter` above it -- a
    ///   restart count above zero is the single most useful glanceable
    ///   signal in this table.
    /// - EXIT: `Role::Bark` for a genuine failure (a nonzero code or a
    ///   signal), `Role::Ink3` otherwise (a clean `0`, or no exit recorded
    ///   yet -- both render `-` or an uneventful number).
    /// - CPU and MEM: a magnitude ramp -- see [`cpu_role`]/[`mem_role`] for
    ///   the thresholds and the reasoning behind them.
    ///
    /// Reuses [`Self::rows`] for the cell text itself rather than rebuilding
    /// it, so this method only ever decides colour, never content.
    ///
    /// `status_word` is a plain parameter, not part of `Presentation`,
    /// because it is not a fact resolved once at the seam the way `level`/
    /// `colour`/`deep_colour` are -- it is `table_of`'s (`output/mod.rs`)
    /// own per-attempt decision, local to its two calls here, and putting
    /// it on `Presentation` would have made it crate-wide state that ~100
    /// call sites construct for a question only this one method ever asks.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let mut rows = self.rows();
        for (row, p) in rows.iter_mut().zip(&self.0) {
            row[2] = status_cell(p.status, presentation, status_word);
            colour_cell(&mut row[0], Role::Ink3, presentation);
            mute_a_dash(&mut row[3], presentation);
            colour_cell(&mut row[4], restarts_role(p.restarts), presentation);
            colour_cell(&mut row[5], exit_role(p.pid, p.last_exit), presentation);
            colour_cell(&mut row[6], cpu_role(p.cpu_percent), presentation);
            colour_cell(&mut row[7], mem_role(p.memory_bytes), presentation);
            colour_cell(&mut row[9], Role::Ink3, presentation);
            mute_a_dash(&mut row[10], presentation);
        }
        rows
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
            "EXIT" => "last_exit",
            "CPU" => "cpu_percent",
            "MEM" => "memory_bytes",
            "UPTIME" => "uptime_ms",
            "FOLD" => "fold",
            "SMIT" => "smit",
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

    // Parallel to `headers()` above: `["ID", "NAME", "STATUS", "PID",
    // "RESTARTS", "EXIT", "CPU", "MEM", "UPTIME", "FOLD", "SMIT"]`. `flock`
    // is the table this whole feature is drawn of, so it is the one payload
    // type the design spec gives an explicit priority table: ID/NAME/STATUS
    // never drop (`0`), then, in the order they survive as the terminal
    // narrows (ascending priority), UPTIME, PID, MEM, RESTARTS, CPU, EXIT,
    // FOLD, SMIT -- so the real give-up order, highest priority first, is
    // SMIT, FOLD, EXIT, CPU, RESTARTS, MEM, PID, UPTIME.
    // `flock_priorities_line_up_with_flock_headers` (below) pins both the
    // length and which three columns sit at `0`, because these two arrays
    // drift silently -- a header inserted without its priority shifts every
    // priority after it onto the wrong column.
    //
    // EXIT and FOLD are the only two columns sharing the "6 and up" tier,
    // and deliberately not tied at the same number (task 49): EXIT is
    // exactly the column an operator needs most when a sheep is dead --
    // Rin's own boot-loop scenario is "errored, restarts: 15, and nothing
    // says why" -- and least when everything is healthy, where it renders
    // `-` for every row. FOLD, an organizational label rather than a
    // diagnostic, keeps its long-standing spot as the single most droppable
    // column below SMIT; EXIT sits one tier below FOLD, so a narrowing
    // terminal loses FOLD before it loses the one column that answers "why
    // is this row even here".
    //
    // SMIT sits above FOLD, at the very top: it is by far the widest
    // column, so dropping it recovers the most space for one column lost.
    // Rin's ruling is that it belongs among the first columns to yield, and
    // 8 is the literal reading of that.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 2, 4, 6, 5, 3, 1, 7, 8];
}

/// One STATUS cell, per spec §2 -- the only place in this module a face or a
/// colour for a status is decided, and now shared by every table with a
/// STATUS column rather than by `FlockRows` alone.
///
/// It used to be `FlockRows`' own, on the grounds that a dog is not a sheep
/// and the feature was never about giving one a face. That left seven of the
/// eight tables in this module plain while one was coloured, so the same dog
/// read one way under `shep dogs` and another under `shep flock`. Rin's
/// ruling is that the treatment extends: a dog is supervised exactly as a
/// sheep is, and `vocabulary.rs` stays the single source for the faces and
/// the status-to-role mapping rather than growing a second set for dogs.
///
/// - `presentation.level.sheep()` decides whether a face appears at all
///   ([`vocabulary::face`], always exactly 5 columns).
/// - `status_word` decides whether the plain status word rides beside it --
///   `table_of` (`output/mod.rs`) is the only caller that ever passes
///   `false`, on a retry once a first pass already needed to drop a whole
///   column.
/// - `presentation.colour` decides whether the whole cell (face, word, or
///   both) is wrapped in one [`crate::output::paint::style_for`] span, keyed
///   off [`vocabulary::role_of`] -- one span rather than two separately
///   styled pieces, so there is exactly one ANSI boundary for
///   [`crate::output::width::visible_width`] to discount, never two to keep
///   straight.
fn status_cell(status: ProcStatus, presentation: Presentation, status_word: bool) -> String {
    let mut text = if presentation.level.sheep() {
        let face = vocabulary::face(status);
        if status_word {
            format!("{face} {status}")
        } else {
            face.to_string()
        }
    } else {
        status.to_string()
    };
    colour_cell(&mut text, vocabulary::role_of(status), presentation);
    text
}

/// The [`ProcStatus`] a free-text STATUS cell is naming, if it is naming one.
///
/// The dog-action rows ([`DogEnabledRow`] and its three siblings) carry
/// `status` as a `String`, because it holds either a real status rendering or
/// a sentence saying why no shepherd answered. A sentence has no role, so
/// this is what keeps colour off it: a cell that names a status is coloured
/// like one, and a cell that explains something is left alone.
///
/// Matched against each variant's own [`fmt::Display`](std::fmt::Display)
/// rather than against a second table of strings, so this cannot drift from
/// the rendering it is inverting. What it CAN miss is a variant added to
/// `ProcStatus` and not added to `EVERY` below, which
/// `every_status_is_recognised_by_its_own_rendering` is the guard for.
fn status_named_by(text: &str) -> Option<ProcStatus> {
    const EVERY: [ProcStatus; 6] = [
        ProcStatus::Starting,
        ProcStatus::Online,
        ProcStatus::Stopping,
        ProcStatus::Stopped,
        ProcStatus::Errored,
        ProcStatus::WaitingRestart,
    ];
    EVERY.into_iter().find(|status| status.to_string() == text)
}

/// Colours a cell [`Role::Ink3`] when it holds the `-` placeholder, and
/// leaves it alone otherwise.
///
/// `FlockRows` applies this to PID and SMIT; the rule is that an absent value
/// must not compete with a real one, and it is the same rule wherever a table
/// prints a dash. Factored out because seven tables now want it.
fn mute_a_dash(cell: &mut String, presentation: Presentation) {
    if cell == "-" {
        colour_cell(cell, Role::Ink3, presentation);
    }
}

/// Wraps `cell` in [`crate::output::paint::style_for`]'s span for `role`, or
/// leaves it untouched when `presentation.colour` is off -- the one place
/// [`Self::rows_for`](Render::rows_for) applies colour, so every column it
/// dresses up (STATUS included, through [`status_cell`] above) goes through
/// the identical wrap rather than each cell reimplementing the same two
/// lines.
fn colour_cell(cell: &mut String, role: Role, presentation: Presentation) {
    if !presentation.colour {
        return;
    }
    let style = super::paint::style_for(role, presentation.deep_colour);
    *cell = format!("{style}{cell}{style:#}");
}

/// MEM's colour boundary, in bytes: below it, a live RSS is an ordinary
/// footprint (a small worker, a sidecar, a CLI wrapper); at or above it,
/// `Role::Butter` marks a sheep worth a second look. 128 MiB sits cleanly
/// between the two footprints a real flock actually shows side by side --
/// shep-testbed's own live flock (this task's own verification fixture)
/// carries an app at 3.8M and one at 800M, and this threshold puts them on
/// opposite sides of the ramp rather than leaving them to read identically,
/// which is the whole complaint this task exists to fix.
const MEM_ELEVATED_BYTES: u64 = 128 * 1024 * 1024;

/// [`Role`] for a MEM cell. `None` (no live process to sample) is
/// [`Role::Ink3`], the same "no honest value" colour every dash in this
/// table gets; otherwise the cell is coloured by [`MEM_ELEVATED_BYTES`]'s
/// two-tier ramp.
///
/// # What this deliberately cannot show
///
/// Two tiers saturate. A flock of several large but healthy services
/// renders its whole MEM column one uniform [`Role::Butter`], unable to
/// separate 160M from 4G, and on such a flock the colour carries no
/// information at all.
///
/// A third tier is the obvious answer and is worse. The only role left is
/// [`Role::Bark`], which is reserved for faults everywhere else in this
/// table and in the lookout both, so a healthy 4G service would render as
/// though it had broken. Adding a fifth role instead would mean adding it
/// to `vocabulary.rs`, which is deliberately the single source both
/// renderers read, and paying for it in the lookout's theme as well, for a
/// distinction only some flocks need.
///
/// So the ramp answers "is this one unusual for this flock" and not "how
/// much memory is this". `--format json` carries the exact number for any
/// reader who needs the second question answered.
fn mem_role(memory_bytes: Option<u64>) -> Role {
    match memory_bytes {
        None => Role::Ink3,
        Some(bytes) if bytes >= MEM_ELEVATED_BYTES => Role::Butter,
        Some(_) => Role::Meadow,
    }
}

/// CPU's colour boundary, in percent of one core. Sustained use at or above
/// this is unusual for a steady-state service and worth a glance;
/// below it is ordinary load, not damage -- `Role::Bark` stays reserved for
/// an actual fault (EXIT, below), never for a busy-but-healthy sheep.
const CPU_ELEVATED_PERCENT: f32 = 50.0;

/// [`Role`] for a CPU cell. `None` (not running) and `0.0%` (idle) are both
/// [`Role::Ink3`] -- neither is news, and an idle sheep printing the same
/// muted colour as one with no honest value to report is the point, not a
/// coincidence. A busy sheep is coloured by [`CPU_ELEVATED_PERCENT`]'s
/// two-tier ramp.
fn cpu_role(cpu_percent: Option<f32>) -> Role {
    match cpu_percent {
        None => Role::Ink3,
        Some(cpu) if cpu <= 0.0 => Role::Ink3,
        Some(cpu) if cpu >= CPU_ELEVATED_PERCENT => Role::Butter,
        Some(_) => Role::Meadow,
    }
}

/// [`Role`] for a RESTARTS cell: `Role::Ink3` at zero, `Role::Butter` above
/// it. Spec's own ruling (this task's brief) -- a restart count above zero
/// is the single most useful glanceable signal in the table, so it gets the
/// same "something to look at" colour `theme.rs`'s own `attention` does,
/// never `Role::Bark`, which stays reserved for a genuine fault.
const fn restarts_role(restarts: u32) -> Role {
    if restarts == 0 {
        Role::Ink3
    } else {
        Role::Butter
    }
}

/// [`Role`] for an EXIT cell, mirroring [`exit_cell`]'s own branches rather
/// than parsing the rendered text back apart: a live process (`pid.is_some()`,
/// the cell reads `-`) and a clean `0` exit both get `Role::Ink3`, the same
/// "nothing to report" colour a dash gets everywhere else in this table.
/// Only a nonzero code or a signal -- an actual failure -- earns
/// `Role::Bark`.
fn exit_role(pid: Option<u32>, last_exit: Option<ExitInfo>) -> Role {
    if pid.is_some() {
        return Role::Ink3;
    }
    match last_exit {
        Some(ExitInfo {
            code: Some(code), ..
        }) if code != 0 => Role::Bark,
        Some(ExitInfo {
            signal: Some(_), ..
        }) => Role::Bark,
        // A clean `0` exit, an exit the daemon could not characterize (both
        // fields `None`), or no exit recorded at all -- none of the three is
        // news.
        _ => Role::Ink3,
    }
}

/// The EXIT column's cell: the last exit's code or signal name for a sheep
/// that is not currently running, `-` otherwise — the same convention PID/
/// CPU/MEM already use for "no honest value to show" (`Self::rows`'s own
/// comments give each of their reasons).
///
/// Gated on `pid` rather than `status` directly: `pid` is `None` for exactly
/// the statuses with no live process to report (`Stopped`, `Errored`,
/// `WaitingRestart`), and `Some` for every status with one still on the
/// system (`Starting`, `Online`, and `Stopping` — a reload's drainee, still
/// alive mid-drain) — the same fact `Self::rows`'s own PID cell already
/// reads off `pid` rather than off `status`.
///
/// `pub(crate)`, not private: `lookout::view::flock`'s own EXIT column
/// (task 49) calls this directly rather than re-deriving the code/signal
/// split and the nix signal-name lookup a second time -- the exact drift
/// this project's "one vocabulary" rule exists to prevent. See
/// `output::mod`'s own re-export for why the visibility has to travel
/// through there too.
pub(crate) fn exit_cell(pid: Option<u32>, last_exit: Option<ExitInfo>) -> String {
    if pid.is_some() {
        return "-".to_string();
    }
    match last_exit {
        None => "-".to_string(),
        Some(ExitInfo {
            code: Some(code), ..
        }) => code.to_string(),
        Some(ExitInfo {
            signal: Some(signal),
            ..
        }) => signal_label(signal),
        // Both `None`: the daemon recorded an exit it could not characterize
        // (see `ExitInfo`'s own doc). Not "no honest value" in the same
        // sense the other two arms are, but nothing legible to print either.
        Some(ExitInfo {
            code: None,
            signal: None,
        }) => "-".to_string(),
    }
}

/// Renders a raw unix signal number as its canonical name (`SIGKILL`), or
/// the bare number when this platform's own signal table has none for it.
///
/// [`shep_core::signals::OperatorSignal`] deliberately carries no such
/// accessor — see that type's own module doc — because a raw number is not
/// portable across platforms and shep-core has no libc to resolve one
/// against. This function does not have that problem: `shep-client` only
/// ever reaches a daemon over a local unix socket (its own crate doc says
/// so), so a `ProcessInfo` this binary is rendering was always produced by
/// a daemon running the SAME OS as this binary — the signal table to
/// resolve against is simply this platform's own.
///
/// `#[cfg(unix)]`/`#[cfg(not(unix))]` rather than a `cfg` inside the
/// function body: `nix` is a unix-only dependency of this crate (see
/// `Cargo.toml`'s `[target.'cfg(unix)'.dependencies]`), so a Windows build
/// never links it. The Windows arm is effectively dead code in practice —
/// `shep-client` being unix-only means no verb ever reaches this crate's
/// Windows leg with a real `ProcessInfo` to render — but it still has to
/// compile there, which is what `--target x86_64-pc-windows-gnu` checks.
#[cfg(unix)]
fn signal_label(raw: i32) -> String {
    nix::sys::signal::Signal::try_from(raw)
        .map_or_else(|_| raw.to_string(), |signal| signal.as_str().to_string())
}

/// Windows counterpart to the `#[cfg(unix)]` `signal_label` above — see its
/// doc for why this arm is unreachable in practice yet still has to exist.
#[cfg(not(unix))]
fn signal_label(raw: i32) -> String {
    raw.to_string()
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

    /// The same treatment `FlockRows` gives its own columns, on the five this
    /// table shares with it, so the same dog reads the same way under
    /// `shep dogs` as a sheep does under `shep flock`.
    ///
    /// STATUS takes the face and the colour ([`status_cell`]); RESTARTS,
    /// CPU and MEM take the same three ramps; a `-` in PID is muted.
    ///
    /// SOURCE is muted throughout rather than ramped. It says where a
    /// binary came from, never whether the dog is healthy, which is exactly
    /// the role FOLD plays in the sheep table -- and this table's own
    /// `PRIORITIES` already treats the two as the same kind of column.
    ///
    /// NAME and UPTIME stay plain, matching `FlockRows`: a name has no state
    /// to report, and an uptime has no threshold anyone agreed on.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let mut rows = self.rows();
        for (row, p) in rows.iter_mut().zip(&self.0) {
            colour_cell(&mut row[1], Role::Ink3, presentation);
            row[2] = status_cell(p.status, presentation, status_word);
            mute_a_dash(&mut row[3], presentation);
            colour_cell(&mut row[4], restarts_role(p.restarts), presentation);
            colour_cell(&mut row[5], cpu_role(p.cpu_percent), presentation);
            colour_cell(&mut row[6], mem_role(p.memory_bytes), presentation);
        }
        rows
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
        "out_file",
        "err_file",
        // Always `null` here: only `Describe` walks for lambs, and this
        // table renders `ListFlock`'s dog half. A dog is one process by
        // contract, so a lamb tree for one is not a rendering this table
        // needs to grow to cover.
        "lambs",
        // No EXIT column here (task 49): the task that added `last_exit`
        // scoped the new table column to `shep flock`'s own sheep table.
        // Rides the JSON anyway, same as every other field on this wire, so
        // a consumer switching on `ProcessInfo` shape alone still sees it.
        "last_exit",
        // A dog paints smits; nothing paints one on a dog. Always `null`
        // here, and in the JSON for the same shape-consistency reason as the
        // rest of this list.
        "smit",
    ];

    // Parallel to `headers()` above: `["NAME", "SOURCE", "STATUS", "PID",
    // "RESTARTS", "CPU", "MEM", "UPTIME"]`. NAME and STATUS are what
    // identify a row -- the same floor `FlockRows` uses for its own
    // ID/NAME/STATUS -- so they sit at `0`. The five columns this table
    // shares with `FlockRows` (UPTIME, PID, MEM, RESTARTS, CPU) keep that
    // table's own drop order exactly, so an operator who has learned one
    // table's behaviour is not surprised by the other. SOURCE is the one
    // column this table has that `FlockRows` does not: it says where the
    // binary came from, not whether the dog is healthy, so it is the least
    // essential column here -- the same role `FOLD` plays for the flock --
    // and drops first. `dog_priorities_line_up_with_dog_headers` (below)
    // pins both the length and which two columns sit at `0`.
    const PRIORITIES: &'static [u8] = &[0, 6, 0, 2, 4, 5, 3, 1];
}

/// One sheep's lamb tree, as `describe`'s second table.
///
/// Two columns and no more. A command line is deliberately absent — see
/// [`Lamb`]'s own doc, which is where the reasoning lives rather than
/// repeated here. Not `#[serde(transparent)]` like [`FlockRows`]/[`DogRows`]:
/// this type's JSON shape is never read. `describe`'s `--format json` arm
/// serializes the listing as `FlockRows` (each row already carrying its own
/// `lambs`), exactly as [`emit_flock`](super::emit_flock) does for dogs —
/// this type exists only to reach [`render_table`](super::render_table) for
/// the table half.
#[derive(Debug, Serialize)]
pub struct LambRows(pub Vec<Lamb>);

/// `LambRows` gets NO colour, and that is a decision rather than an
/// omission.
///
/// Both its columns are identity: which process, and what it is running. A
/// lamb has no status, no restart count, no resource reading, and no
/// placeholder -- `pid` is a real number on every row and `name` is a real
/// string. There is nothing here for a colour to carry, and the rule is that
/// every colour carries information. Muting one of two columns would just be
/// picking one to look faded.
impl Render for LambRows {
    fn headers() -> &'static [&'static str] {
        &["PID", "NAME"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|lamb| vec![lamb.pid.to_string(), lamb.name.clone()])
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "PID" => "pid",
            "NAME" => "name",
            other => panic!("LambRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()` above: `["PID", "NAME"]`. Both are floor
    // columns at `0`, not one placeholder for the other: a lamb has
    // exactly these two facts, both are identity (which process, which
    // command), and there is nothing left over to designate droppable --
    // unlike `FlockRows`/`DogRows`, which each have a column beyond their
    // own floor. Two columns is already below `render_boxed`'s own floor
    // of three, so this table never actually narrows regardless of what
    // the array says; it is still spelled out explicitly (rather than left
    // at the trait's all-zero default) so a header added here later does
    // not silently inherit "never drops" by omission.
    // `lamb_priorities_line_up_with_lamb_headers` (below) pins the length.
    const PRIORITIES: &'static [u8] = &[0, 0];
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

    /// The dog-action rows' shared treatment, spelled out here once and
    /// pointed at from the other three.
    ///
    /// SOURCE is muted, the same call `DogRows` makes: it says where the
    /// binary came from, never whether anything is healthy.
    ///
    /// STATUS is coloured only when it NAMES a status. This field holds
    /// either a real `ProcStatus` rendering or a sentence saying why no
    /// shepherd answered ([`Self::status`]'s own doc), and a sentence has no
    /// role to wear -- colouring it would be decoration, which is the one
    /// thing the rule here forbids. [`status_named_by`] is what tells the two
    /// apart.
    ///
    /// SHEPHERD is left plain deliberately, and it was the closest call in
    /// this table. `false` is worth knowing -- it means the config changed
    /// and nothing is running yet -- but the STATUS cell beside it already
    /// says so in a whole sentence, so a colour here would be a second
    /// decoration saying what the text already says. That is the same
    /// reasoning `lookout/theme.rs` gives for not putting a face in its own
    /// flock pane.
    ///
    /// NAME stays plain, matching every other table in this module.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let mut rows = self.rows();
        let row = &mut rows[0];
        colour_cell(&mut row[1], Role::Ink3, presentation);
        if let Some(status) = status_named_by(&row[3]) {
            row[3] = status_cell(status, presentation, status_word);
        }
        rows
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

    // Parallel to `headers()` above: `["NAME", "SOURCE", "SHEPHERD",
    // "STATUS"]`. NAME and STATUS are the floor, the same role they play in
    // `DogRows`. SOURCE and SHEPHERD are this type's own two extras (see
    // `Render::PRIORITIES`'s own doc for the `6`-and-up rule); SOURCE drops
    // first, one round before SHEPHERD -- the same "least essential" role
    // `DogRows` already gives it (where the binary came from, not whether
    // the dog is healthy), and `render_boxed`'s floor of three means a
    // 4-column table like this one only ever gets to drop the single
    // highest-priority extra, so this ordering also decides which of the two
    // an operator actually loses at a narrow width.
    // `priorities_line_up_with_headers_for_every_render_impl` (this module's
    // test section) pins the length and the floor for every impl, this one
    // included.
    const PRIORITIES: &'static [u8] = &[0, 7, 6, 0];
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

    /// Same treatment, same reasoning, as [`DogEnabledRow::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let mut rows = self.rows();
        let row = &mut rows[0];
        colour_cell(&mut row[1], Role::Ink3, presentation);
        if let Some(status) = status_named_by(&row[3]) {
            row[3] = status_cell(status, presentation, status_word);
        }
        rows
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

    // Same shape, same reasoning, as `DogEnabledRow::PRIORITIES` -- this
    // type shares its headers exactly.
    const PRIORITIES: &'static [u8] = &[0, 7, 6, 0];
}

/// `shep adopt <path> [--name <name>]`: what the config edit and, if a shepherd is
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

    /// Same treatment, same reasoning, as [`DogEnabledRow::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let mut rows = self.rows();
        let row = &mut rows[0];
        colour_cell(&mut row[1], Role::Ink3, presentation);
        if let Some(status) = status_named_by(&row[3]) {
            row[3] = status_cell(status, presentation, status_word);
        }
        rows
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

    // Same shape, same reasoning, as `DogEnabledRow::PRIORITIES` -- this
    // type shares its headers exactly.
    const PRIORITIES: &'static [u8] = &[0, 7, 6, 0];
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

    /// Same treatment, same reasoning, as [`DogEnabledRow::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let mut rows = self.rows();
        let row = &mut rows[0];
        colour_cell(&mut row[1], Role::Ink3, presentation);
        if let Some(status) = status_named_by(&row[3]) {
            row[3] = status_cell(status, presentation, status_word);
        }
        rows
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

    // Same shape, same reasoning, as `DogEnabledRow::PRIORITIES` -- this
    // type shares its headers exactly.
    const PRIORITIES: &'static [u8] = &[0, 7, 6, 0];
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

    /// Two of the four columns, and no more, because two of them have
    /// nothing to say.
    ///
    /// ID is muted, exactly as `FlockRows` mutes its own: it never changes
    /// and it is chrome beside the thing this table is about.
    ///
    /// A `-` in either path column is muted, the placeholder rule this
    /// module applies everywhere. A real path is left plain: it is the
    /// subject of the table rather than a reading about it, there is no
    /// threshold to ramp against and no fault to mark, and colouring the
    /// widest column on the row for no information would be exactly the
    /// decoration the rule forbids.
    ///
    /// No STATUS column here at all -- see this type's own doc for why
    /// `flush` renders the files rather than the lifecycle -- so there is no
    /// face and no status colour to give.
    fn rows_for(&self, presentation: Presentation, _status_word: bool) -> Vec<Vec<String>> {
        let mut rows = self.rows();
        for row in &mut rows {
            colour_cell(&mut row[0], Role::Ink3, presentation);
            mute_a_dash(&mut row[2], presentation);
            mute_a_dash(&mut row[3], presentation);
        }
        rows
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
        // Same lifecycle-field reasoning as `status`/`pid`/`restarts` above:
        // `flush` neither reads nor changes why a sheep last exited, and a
        // column for it would still push OUT_FILE/ERR_FILE off the side of
        // a terminal. Stays in the JSON for shape consistency with every
        // other verb answering `ProcessInfo`.
        "last_exit",
        // And the same again for the mark a dog painted: a flush neither
        // reads nor changes it.
        "smit",
    ];

    // Parallel to `headers()` above: `["ID", "NAME", "OUT_FILE",
    // "ERR_FILE"]`. ID and NAME are the floor -- the same two `FlockRows`
    // uses for its own row identity. OUT_FILE and ERR_FILE are this table's
    // whole reason for existing (this type's own doc), so unlike most
    // extras neither is a minor detail -- but they are still the two
    // longest, most unbounded cells this table ever renders (this type's
    // own doc: "an absolute path... often longer than every other column
    // put together"), so both sit at `Render::PRIORITIES`'s `6`-and-up tier
    // rather than at the floor. ERR_FILE survives one round longer than
    // OUT_FILE: an operator chasing a crash reads stderr first, and
    // `render_boxed`'s floor of three means a 4-column table like this one
    // only ever drops the single highest-priority extra, so this ordering
    // decides which path an operator actually loses at a narrow width.
    const PRIORITIES: &'static [u8] = &[0, 0, 7, 6];
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

    // Parallel to `headers()` above: `["STREAM", "FILE", "RESULT"]`. STREAM
    // (which of the shepherd's own two files) and RESULT (what happened to
    // it) are the floor -- the same STATUS-shaped role `FlockRows`/`DogRows`
    // give their own outcome column. FILE, the one extra, is the absolute
    // path -- the single `6`-and-up column here. Three columns is already
    // `render_boxed`'s own floor, so this table never actually narrows
    // regardless of what the array says (the same case `LambRows::PRIORITIES`
    // documents); still spelled out explicitly so a header added here later
    // does not silently inherit "never drops" by omission.
    const PRIORITIES: &'static [u8] = &[0, 6, 0];
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

    // One column, and it is the row's whole identity -- nothing to
    // designate droppable, the same `LambRows::PRIORITIES` reasoning for a
    // type with nothing left over.
    const PRIORITIES: &'static [u8] = &[0];
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

    // Two columns, both the whole point of this report -- the same
    // `LambRows::PRIORITIES` reasoning: nothing here is less essential than
    // anything else, so both sit at the floor rather than one drop order
    // nobody chose.
    const PRIORITIES: &'static [u8] = &[0, 0];
}

/// One sheep as the muster roll remembers it, for `shep flock` when no
/// shepherd is running.
///
/// `status` is always `"stopped"`: a roll records what *was* registered, and
/// with no shepherd answering, nothing from it is up. Stating it in a column
/// rather than leaving the reader to infer it from context, because the
/// whole point of this rendering is that "no shepherd" and "no processes"
/// look identical if you do not say which one you mean.
#[derive(Debug, Serialize)]
pub struct RolledSheep {
    /// The sheep's name, as saved.
    pub name: String,
    /// How many instances were running when the roll was written.
    pub instances: u32,
    /// Always `"stopped"`.
    pub status: &'static str,
}

/// Every sheep in a muster roll.
#[derive(Debug, Serialize)]
pub struct RolledSheepRows(pub Vec<RolledSheep>);

impl Render for RolledSheepRows {
    fn headers() -> &'static [&'static str] {
        &["NAME", "INSTANCES", "STATUS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|s| vec![s.name.clone(), s.instances.to_string(), s.status.to_owned()])
            .collect()
    }

    /// # Panics
    /// If `header` is not one of [`Self::headers`]'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "INSTANCES" => "instances",
            "STATUS" => "status",
            other => panic!("RolledSheepRows has no column {other}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()` above: `["NAME", "INSTANCES", "STATUS"]`.
    // NAME and STATUS are the floor, the same STATUS-shaped role
    // `FlockRows`/`DogRows` give their own outcome column -- even though
    // this one is always `"stopped"`, it is still the fact this whole
    // rendering exists to state (this type's own doc). INSTANCES is the one
    // `6`-and-up extra. Three columns is already `render_boxed`'s own floor,
    // so this table never actually narrows regardless (the same case
    // `LambRows::PRIORITIES` documents); still spelled out for the reason
    // that one gives.
    const PRIORITIES: &'static [u8] = &[0, 6, 0];
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

    // Two columns, both the whole point of this report (this type's own
    // doc) -- the same `LambRows::PRIORITIES` reasoning: nothing here is
    // less essential than anything else.
    const PRIORITIES: &'static [u8] = &[0, 0];
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

    // Parallel to `headers()` above: `["NAME", "SCRIPT", "INSTANCES",
    // "REUSE_PORT"]`. NAME is the floor -- the one column that says which
    // app a row is about. SCRIPT, INSTANCES and REUSE_PORT are this table's
    // three `6`-and-up extras, ranked by how genuinely droppable each is:
    // SCRIPT is an unbounded path (the same reason `FlushedRows` keeps its
    // own paths off the floor) and drops first; REUSE_PORT is the one column
    // this type's own doc says "an operator scans for at a glance", so it
    // survives longest. `render_boxed`'s floor of three means a 4-column
    // table like this one only ever drops the single highest-priority
    // extra, so in practice only SCRIPT is ever lost to a narrow terminal.
    const PRIORITIES: &'static [u8] = &[0, 8, 7, 6];
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

    // Parallel to `headers()` above: `["ACTION", "TARGET", "RESULT"]`.
    // TARGET (the file or command a step acted on) and RESULT (what
    // happened) are the floor -- the same STATUS-shaped role
    // `FlockRows`/`DogRows` give their own outcome column, generalized to
    // "what a step did, and to what". ACTION is the one `6`-and-up extra.
    // Three columns is already `render_boxed`'s own floor, so this table
    // never actually narrows regardless (the same case
    // `LambRows::PRIORITIES` documents); still spelled out for the reason
    // that one gives.
    const PRIORITIES: &'static [u8] = &[6, 0, 0];
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

    // Parallel to `headers()` above: `["ID", "NAME", "OUTCOME", "DETAIL"]`.
    // ID and NAME are the floor `FlockRows` itself uses for row identity;
    // OUTCOME joins them for the same STATUS-shaped reason `FlockRows`/
    // `DogRows` give their own outcome column -- an operator needs to know
    // whether the trigger succeeded even more than the free-text detail
    // explaining why. DETAIL, this table's one unbounded free-text column
    // (`Self::rows`'s own doc: capped at 80 chars, still the longest cell
    // this table renders), is the sole `6`-and-up extra, and `render_boxed`'s
    // floor of three means dropping it is the only narrowing this table ever
    // does -- exactly the three-essential-columns shape `FlockRows` itself
    // has.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 6];
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

    // Same shape, same reasoning, as `TriggeredRows::PRIORITIES` -- this
    // type shares its headers exactly.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 6];
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
/// carrying what happened when the shepherd tried to write `shep whisper`'s
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

    // Same shape, same reasoning, as `TriggeredRows::PRIORITIES` -- this
    // type shares its headers exactly.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 6];
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

    // Parallel to `headers()` above: `["WHEN", "RULE", "SUBJECT", "MESSAGE",
    // "SINKS"]`. WHEN, RULE and SUBJECT are the floor -- when a bark fired,
    // which rule fired it, and which sheep it is about, the three facts that
    // identify a record an operator is scanning an alert feed for. MESSAGE
    // and SINKS are this table's two `6`-and-up extras: MESSAGE is
    // free-form, app-or-rule-chosen text of unbounded length (the same
    // reason `TriggeredRows` keeps its own DETAIL column droppable) and
    // drops first; SINKS, a short delivered/failed summary
    // ([`sinks_cell`]'s own doc), survives one round longer. `render_boxed`'s
    // floor of three means both can be lost to a narrow terminal, landing
    // back on exactly WHEN/RULE/SUBJECT.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 7, 6];
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

    // Two columns, both the whole point of a KV entry -- the same
    // `LambRows::PRIORITIES` reasoning: a key with no value, or a value with
    // no key, is not a row at all.
    const PRIORITIES: &'static [u8] = &[0, 0];
}

/// `shep dogs --available`'s community-index listing.
///
/// `transparent`, matching every other `Vec<T>` payload in this file: the
/// JSON is a plain array of [`AvailableDog`] objects, not a wrapper
/// object. Constructed by `commands/query.rs`'s `available_dogs`, from
/// [`crate::dog_index::fetch_index`] -- never from a `Response`: the
/// community index never touches the daemon wire.
///
/// Every string [`AvailableDog`] carries is already sanitised
/// (`dog_index`'s own module doc is the security boundary for that), so
/// this impl clones fields straight through with no further escaping.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct AvailableDogRows(pub Vec<AvailableDog>);

impl Render for AvailableDogRows {
    fn headers() -> &'static [&'static str] {
        &["NAME", "PACKAGE", "CATEGORY", "DESCRIPTION"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|dog| {
                vec![
                    dog.name.clone(),
                    dog.package.clone(),
                    dog.category.clone(),
                    dog.description.clone(),
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
            "PACKAGE" => "package",
            "CATEGORY" => "category",
            "DESCRIPTION" => "description",
            other => panic!("AvailableDogRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // The whole reason the detail view exists (`AvailableDog::adopt_as`'s
        // own doc): the name to build an adopt line from, never a column,
        // because a table has no room to explain why it differs from NAME.
        "adopt_as",
        // Long and rarely glanced at in a table row; the detail view is
        // where an operator reads it before running the install command.
        "repo", "license",
        // The tagged `DogSourceKind` object -- what the detail view's
        // install line is built from, not a column this table renders.
        "source",
    ];

    // NAME and PACKAGE are what identify a row -- a dog's display name and
    // its real, adoptable identity -- so both sit at the floor, the same
    // role `DogRows`' own NAME/STATUS play. CATEGORY and DESCRIPTION are
    // both droppable, and DESCRIPTION is the long, unbounded free-text
    // field of the two, so it goes first -- the same "long field before
    // short, glanceable one" rule `Render::PRIORITIES`' own doc states.
    const PRIORITIES: &'static [u8] = &[0, 0, 6, 7];
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

    // One column, and it is the row's whole identity -- nothing to
    // designate droppable, the same `LambRows::PRIORITIES` reasoning for a
    // type with nothing left over.
    const PRIORITIES: &'static [u8] = &[0];
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
            // Every row here is `pid: Some(..)` -- running -- so `exit_cell`
            // renders `-` regardless of this value (task 49's own gate: EXIT
            // only shows for a sheep with no live pid). Populated anyway,
            // for the same "every Option field Some" reason the rest of
            // this fixture is: `restarts(id)` above already implies a prior
            // exit for every row past id 0, and leaving this `None` would
            // make it a field the drift test's JSON-key check stops
            // exercising via this fixture.
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            // Fixed rather than id-derived, like `fold` and `cpu_percent`
            // above. The literal a real dog paints, taken from shep-deploy's
            // own renderer, so the drift check compares the cell against a
            // string shep will actually be handed rather than a placeholder.
            // Left `None` it would serialize as `null`, which
            // `assert_no_drift`'s cell check skips rather than compares, and
            // SMIT would quietly stop being watched: swapping the cell for a
            // bogus string still passed the drift test until this line
            // existed.
            .smit(Some("\u{25b2} main@a1b2c3".to_string()))
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
        // the doc comment on `assert_no_drift` above. EXIT joins them for a
        // different reason: its own JSON value (`last_exit`) is a nested
        // object, not a scalar `assert_no_drift`'s per-cell comparison knows
        // how to stringify, and every `sample_flock` row is `pid: Some(..)`
        // besides, so the honest cell is always `-` regardless of what
        // `last_exit` says.
        assert_no_drift(
            &sample_flock(),
            |j| &j[0],
            &["UPTIME", "CPU", "MEM", "EXIT"],
        );
    }

    /// fails if the EXIT column stops reading `pid` before `last_exit`, or
    /// stops rendering a code/signal legibly. `sample_flock`'s own fixture
    /// cannot exercise this: every row there is `pid: Some(..)`, so the
    /// cell is always `-` regardless of what `last_exit` says (task 49's
    /// own design: EXIT answers "why is it not running", so a sheep that IS
    /// running has nothing for it to say).
    #[test]
    fn the_exit_column_shows_the_last_exit_only_for_a_sheep_that_is_not_running() {
        let headers = FlockRows::headers();
        let at = |cells: &[String], h: &str| {
            cells[headers.iter().position(|x| *x == h).unwrap()].clone()
        };

        // Never exited: no pid, no `last_exit`.
        let never_run = ProcessInfo::builder(1, "fresh", ProcStatus::Stopped).build();
        // Exited with a code: no pid, `last_exit` carries one.
        let crashed = ProcessInfo::builder(2, "crashed", ProcStatus::Errored)
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();
        // Killed by a signal: no pid, `last_exit` carries one.
        let killed = ProcessInfo::builder(3, "killed", ProcStatus::Stopped)
            .last_exit(Some(ExitInfo {
                code: None,
                signal: Some(9),
            }))
            .build();
        // Running again after a past exit: `last_exit` is still `Some`
        // (sticky across a respawn — `ProcessInfo::last_exit`'s own doc),
        // but a live pid means there is nothing for this column to say.
        let running_again = ProcessInfo::builder(4, "recovered", ProcStatus::Online)
            .pid(Some(4242))
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();

        let rows = FlockRows(vec![never_run, crashed, killed, running_again]).rows();
        assert_eq!(at(&rows[0], "EXIT"), "-");
        assert_eq!(at(&rows[1], "EXIT"), "1");
        #[cfg(unix)]
        assert_eq!(at(&rows[2], "EXIT"), "SIGKILL");
        // Windows never carries a real `ProcessInfo` to render (`shep-
        // client` is unix-only, so no verb reaches this crate's Windows leg
        // with real data) but this file still has to compile there — see
        // `signal_label`'s own doc for why its Windows arm is a bare number.
        #[cfg(not(unix))]
        assert_eq!(at(&rows[2], "EXIT"), "9");
        assert_eq!(at(&rows[3], "EXIT"), "-");
    }

    /// fails if `LambRows` grows a field that never reaches the table, or
    /// swaps PID and NAME between its two columns.
    #[test]
    fn lamb_rows_do_not_drift() {
        assert_no_drift(
            &LambRows(vec![Lamb::new(4243, "node"), Lamb::new(4244, "sh")]),
            |j| &j[0],
            &[],
        );
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

    /// The live index's own single entry (`web/public/dogs.json`), the same
    /// fixture `dog_index`'s own tests build from, so this module's own
    /// coverage stays aimed at a real published shape rather than one
    /// invented here.
    fn sample_available_dog() -> AvailableDog {
        AvailableDog {
            name: "Spot".to_string(),
            package: "shep-log-rotate".to_string(),
            adopt_as: "log-rotate".to_string(),
            description: "Rotates grown log files and asks the shepherd to reopen them."
                .to_string(),
            repo: "https://github.com/TurtIeSocks/shep-log-rotate".to_string(),
            license: "MIT OR Apache-2.0".to_string(),
            category: "logs".to_string(),
            source: crate::dog_index::DogSourceKind::CargoGit {
                url: "https://github.com/TurtIeSocks/shep-log-rotate".to_string(),
            },
        }
    }

    /// fails if `AvailableDogRows` grows a field that never reaches the
    /// table, or forgets a `JSON_ONLY` reason -- the same gate every other
    /// payload has. `adopt_as`/`repo`/`license`/`source` all serialize but
    /// are covered by `JSON_ONLY` rather than a column (this type's own
    /// doc says why); `assert_no_drift`'s key-set check still requires
    /// each to carry a reason there, or this test fails on an unexplained
    /// field. `formatted` is empty: every one of the four real columns is a
    /// plain string, with no nested object or human-only rendering to skip
    /// the way `DogRows`' own `SOURCE` needs.
    #[test]
    fn available_dog_rows_do_not_drift() {
        assert_no_drift(
            &AvailableDogRows(vec![sample_available_dog()]),
            |j| &j[0],
            &[],
        );
    }

    // --- Whole-branch review item 2: every `Render` impl gets a real
    // `PRIORITIES` --------------------------------------------------------

    /// One `Render` impl's own check, called once per type below by
    /// [`priorities_line_up_with_headers_for_every_render_impl`]. Generic
    /// over `T` alone, no instance needed -- `headers()` and `PRIORITIES`
    /// are both associated items with no `&self`, so there is nothing to
    /// construct just to read them.
    ///
    /// Two failure modes, both real: `headers()` and `PRIORITIES` are
    /// parallel arrays edited by hand in two different places, so a header
    /// inserted or reordered without its priority shifts every priority
    /// after the gap onto the wrong column, and `render_boxed` starts
    /// dropping the wrong one under a narrow terminal -- caught by the
    /// length check. And a type whose `PRIORITIES` marks the wrong columns
    /// `0` (too many, too few, or the wrong ones) either pins a column open
    /// that should narrow, or lets an identity column vanish first -- caught
    /// by the floor check.
    fn assert_priorities_match_headers<T: Render>(floor: &[&str]) {
        let headers = T::headers();
        let priorities = T::PRIORITIES;
        assert_eq!(
            headers.len(),
            priorities.len(),
            "{}: headers() has {} columns but PRIORITIES has {} — they must move together",
            std::any::type_name::<T>(),
            headers.len(),
            priorities.len(),
        );
        let actual_floor: Vec<&str> = headers
            .iter()
            .zip(priorities)
            .filter(|&(_, &p)| p == 0)
            .map(|(&h, _)| h)
            .collect();
        assert_eq!(
            actual_floor,
            floor,
            "{}: the columns at priority 0 do not match this type's own intended floor",
            std::any::type_name::<T>(),
        );
    }

    /// The anti-drift gate for [`Render::PRIORITIES`] across every payload
    /// type this crate defines, not one test per type: Task 5b gave
    /// `DogRows`/`LambRows` a real array after the empirical reviewer caught
    /// `DogRows` wrapping and breaking its own borders under a real
    /// terminal, directly beneath a sheep table narrowing gracefully above
    /// it -- but the reasoning was never carried past those two, and the
    /// other eighteen `Render` impls in this module carried the trait's
    /// all-zero default (silent "never narrows") until this task. A single
    /// test that walks every impl, rather than one bespoke test per type,
    /// is what makes a table added later without a real `PRIORITIES` fail
    /// here instead of shipping the same defect a fourth time.
    ///
    /// Each `floor` list is this type's own intended set of never-drop
    /// columns -- see [`Render::PRIORITIES`]'s own doc for the rule that
    /// produced it (`0` for identity, `1`-`5` reserved for the five
    /// `FlockRows` columns, `6` and up for everything else), and the
    /// `PRIORITIES` array beside each impl above for the reasoning specific
    /// to that table.
    #[test]
    fn priorities_line_up_with_headers_for_every_render_impl() {
        assert_priorities_match_headers::<FlockRows>(&["ID", "NAME", "STATUS"]);
        assert_priorities_match_headers::<DogRows>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<LambRows>(&["PID", "NAME"]);
        assert_priorities_match_headers::<DogEnabledRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<DogDisabledRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<DogAdoptedRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<DogRehomedRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<FlushedRows>(&["ID", "NAME"]);
        assert_priorities_match_headers::<EmptiedFiles>(&["STREAM", "RESULT"]);
        assert_priorities_match_headers::<DeletedIds>(&["ID"]);
        assert_priorities_match_headers::<KillRow>(&["PID", "SOCKET_REMOVED"]);
        assert_priorities_match_headers::<RolledSheepRows>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<SavedRollRow>(&["FILE", "APPS"]);
        assert_priorities_match_headers::<ImportRows>(&["NAME"]);
        assert_priorities_match_headers::<StartupSteps>(&["TARGET", "RESULT"]);
        assert_priorities_match_headers::<TriggeredRows>(&["ID", "NAME", "OUTCOME"]);
        assert_priorities_match_headers::<SignalledRows>(&["ID", "NAME", "OUTCOME"]);
        assert_priorities_match_headers::<SentLineRows>(&["ID", "NAME", "OUTCOME"]);
        assert_priorities_match_headers::<BarkRows>(&["WHEN", "RULE", "SUBJECT"]);
        assert_priorities_match_headers::<KvRows>(&["KEY", "VALUE"]);
        assert_priorities_match_headers::<KvUnsetRow>(&["REMOVED"]);
        assert_priorities_match_headers::<AvailableDogRows>(&["NAME", "PACKAGE"]);
    }

    /// The floor-set check above cannot see two non-floor columns trading
    /// numbers, because both stay non-zero and the lengths still agree. The
    /// flock listing is the one table whose drop order the spec states
    /// outright, so that order is pinned here column by column: a swap
    /// between, say, `EXIT` and `CPU` would change what an operator loses
    /// first on a narrowing terminal, and nothing else would notice.
    ///
    /// `EXIT` before `CPU` is the one placement here worth arguing about.
    /// It reads `-` for every running sheep, so on a healthy flock it is an
    /// empty column outranking a live number, which is why it sits where it
    /// does. The case against: a narrow terminal is most often being read
    /// BECAUSE something is wrong, and for a dead sheep `EXIT` is the only
    /// column still saying anything while `CPU` has gone to `-` itself.
    /// Left as it is rather than flipped on one person's reading; if it
    /// moves, this test is the place that records the decision.
    #[test]
    fn the_flock_listing_drops_its_columns_in_the_documented_order() {
        let mut ranked: Vec<(&str, u8)> = FlockRows::headers()
            .iter()
            .copied()
            .zip(FlockRows::PRIORITIES.iter().copied())
            .collect();
        ranked.sort_by_key(|&(_, priority)| priority);

        let order: Vec<&str> = ranked.iter().map(|&(header, _)| header).collect();
        assert_eq!(
            order,
            vec![
                // The three that identify a sheep, and so never drop.
                "ID", "NAME", "STATUS", //
                // Then, in the order they SURVIVE as the terminal narrows
                // (ascending priority; the real give-up order is the
                // reverse of this): the ones answering "is it healthy"
                // outlast the ones answering "which one is it".
                "UPTIME", "PID", "MEM", "RESTARTS", "CPU", "EXIT", "FOLD", "SMIT",
            ],
            "the flock listing's drop order changed; if that is deliberate, \
             change this test and say why in the commit"
        );
    }

    // --- Colour: MEM/CPU/RESTARTS/EXIT/ID/FOLD/placeholder roles ----------
    //
    // These pin the boundary of each ramp directly against `Role`, rather
    // than through a snapshot: a snapshot passes whatever was accepted into
    // it, so it cannot by itself prove a colour is keyed to the right fact.
    // These can fail on their own if a threshold or a branch moves.

    /// fails if MEM's ramp boundary moves without a test noticing, or if
    /// either side of it stops being the role the governing rule ("every
    /// colour must carry information") calls for: `None` is the same
    /// "nothing to report" colour a dash gets everywhere else in the table,
    /// and the boundary itself is inclusive on the `Butter` side.
    #[test]
    fn mem_role_ramps_at_its_documented_boundary() {
        assert_eq!(mem_role(None), Role::Ink3);
        assert_eq!(mem_role(Some(MEM_ELEVATED_BYTES - 1)), Role::Meadow);
        assert_eq!(mem_role(Some(MEM_ELEVATED_BYTES)), Role::Butter);
        // The two live figures this task's own verification fixture named:
        // a light app and a heavy one must land on opposite sides.
        assert_eq!(mem_role(Some(3_800_000)), Role::Meadow, "3.8M is light");
        assert_eq!(mem_role(Some(800_000_000)), Role::Butter, "800M is heavy");
    }

    /// fails on the same class of regression as `mem_role`'s own test,
    /// pointed at CPU: idle (`0.0%`) must stay `Ink3` even though it is
    /// technically "below the ramp", since idle is not news, and the
    /// boundary itself is inclusive on the `Butter` side.
    #[test]
    fn cpu_role_ramps_at_its_documented_boundary() {
        assert_eq!(cpu_role(None), Role::Ink3);
        assert_eq!(cpu_role(Some(0.0)), Role::Ink3);
        assert_eq!(cpu_role(Some(0.1)), Role::Meadow);
        assert_eq!(cpu_role(Some(CPU_ELEVATED_PERCENT - 0.1)), Role::Meadow);
        assert_eq!(cpu_role(Some(CPU_ELEVATED_PERCENT)), Role::Butter);
        assert_eq!(cpu_role(Some(99.0)), Role::Butter);
    }

    /// fails if a restart count of exactly zero stops being muted, or if
    /// one restart stops being coloured at all -- an operator's single most
    /// useful glanceable signal in this table (this task's own brief).
    #[test]
    fn restarts_role_is_ink3_only_at_exactly_zero() {
        assert_eq!(restarts_role(0), Role::Ink3);
        assert_eq!(restarts_role(1), Role::Butter);
        assert_eq!(restarts_role(u32::MAX), Role::Butter);
    }

    /// fails if `exit_role` starts painting a clean `0` exit, a still-running
    /// sheep, or an uncharacterised exit as if they were a fault, or if it
    /// stops painting a genuine one. Mirrors `exit_cell`'s own branches
    /// directly rather than going through the rendered `-`/number text, so
    /// this cannot pass by accident the way a text-sniffing test could if
    /// `exit_cell`'s own formatting ever changed.
    #[test]
    fn exit_role_is_bark_only_for_a_genuine_failure() {
        // Still running: the cell itself reads `-`, and running is not a
        // fault regardless of what a *previous* exit recorded.
        assert_eq!(
            exit_role(
                Some(1234),
                Some(ExitInfo {
                    code: Some(1),
                    signal: None
                })
            ),
            Role::Ink3
        );
        // Not running, no exit ever recorded.
        assert_eq!(exit_role(None, None), Role::Ink3);
        // Not running, a clean exit.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: Some(0),
                    signal: None
                })
            ),
            Role::Ink3
        );
        // Not running, the daemon could not characterize the exit.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: None,
                    signal: None
                })
            ),
            Role::Ink3
        );
        // Not running, a genuine nonzero exit code.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: Some(1),
                    signal: None
                })
            ),
            Role::Bark
        );
        // Not running, killed by a signal.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: None,
                    signal: Some(9)
                })
            ),
            Role::Bark
        );
    }

    // --- Colour: the seven tables that are not the flock listing ---------
    //
    // Colour was commissioned as "colour the flock table" and delivered
    // exactly that, so `colour_cell` appeared in one of the eight `Render`
    // impls in this module and nowhere else. These pin what each of the
    // other seven now does AND what it deliberately does not, because
    // "nothing is coloured" and "everything is coloured" are both wrong and
    // a presence check alone cannot tell either from the rule.
    //
    // Every assertion below compares against the exact painted string rather
    // than looking for an escape byte. A presence check would pass on a cell
    // painted the WRONG role, which is the failure that matters here: a
    // healthy dog painted `Bark` reads as broken.

    /// The 256-colour presentation these cases render at.
    fn coloured() -> Presentation {
        use crate::style::StyleLevel;
        Presentation::new(
            StyleLevel::Full,
            None,
            Some(std::ffi::OsStr::new("xterm-256color")),
            None,
            200,
        )
    }

    /// `text` as `colour_cell` would paint it for `role`. Built through
    /// `paint::style_for`, the same function the renderer calls, so this
    /// compares the ROLE and not merely the presence of an escape.
    fn painted(text: &str, role: Role) -> String {
        let mut cell = text.to_string();
        colour_cell(&mut cell, role, coloured());
        cell
    }

    /// One dog, with readings chosen so every ramp lands on a known side:
    /// four restarts (above zero), 0.0% CPU (idle, which is muted rather
    /// than green), and 3 MiB (below the MEM boundary).
    fn sample_dog(status: ProcStatus, pid: Option<u32>) -> ProcessInfo {
        ProcessInfo::builder(9, "log-rotate", status)
            .pid(pid)
            .restarts(4)
            .uptime_ms(41_000)
            .cpu_percent(pid.map(|_| 0.0))
            .memory_bytes(pid.map(|_| 3 * 1024 * 1024))
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            }))
            .build()
    }

    /// fails if the dogs table stops matching the flock table on the five
    /// columns the two share, or if it starts colouring a column that has
    /// nothing to say.
    ///
    /// The same dog used to read one way under `shep dogs` and another under
    /// `shep flock`, because only one of the two tables had ever been
    /// coloured.
    #[test]
    fn the_dogs_table_is_coloured_by_the_flock_tables_own_rules() {
        let rows =
            DogRows(vec![sample_dog(ProcStatus::Online, Some(14_110))]).rows_for(coloured(), true);
        let row = &rows[0];

        assert_eq!(row[0], "log-rotate", "NAME is plain, as in the flock table");
        assert_eq!(row[1], painted("adopted", Role::Ink3), "SOURCE is chrome");
        assert_eq!(
            row[2],
            painted("(o.o) online", Role::Meadow),
            "STATUS takes the face and the role, from vocabulary.rs"
        );
        assert_eq!(row[3], "14110", "a real PID is left plain");
        assert_eq!(row[4], painted("4", Role::Butter), "RESTARTS above zero");
        assert_eq!(row[5], painted("0.0%", Role::Ink3), "idle CPU is not news");
        assert_eq!(row[6], painted("3.0M", Role::Meadow), "MEM below the ramp");
        assert_eq!(row[7], "41s", "UPTIME is plain, as in the flock table");
    }

    /// fails if a stopped dog's `-` placeholders stop being muted, or if its
    /// STATUS stops carrying the role a stopped sheep's does.
    ///
    /// The sibling of the case above, and it exists because that one cannot
    /// reach the placeholder branch: a running dog has a real PID, CPU and
    /// MEM in every cell.
    #[test]
    fn a_stopped_dogs_placeholders_are_muted() {
        let rows = DogRows(vec![sample_dog(ProcStatus::Stopped, None)]).rows_for(coloured(), true);
        let row = &rows[0];

        assert_eq!(row[2], painted("(-.-) stopped", Role::Ink3));
        assert_eq!(row[3], painted("-", Role::Ink3), "PID");
        assert_eq!(row[5], painted("-", Role::Ink3), "CPU");
        assert_eq!(row[6], painted("-", Role::Ink3), "MEM");
    }

    /// fails if a dog-action row colours a STATUS cell holding a SENTENCE.
    ///
    /// `DogEnabledRow::status` is a `String` carrying either a real status
    /// rendering or a sentence saying why no shepherd answered. A sentence
    /// has no role, so painting it would be decoration, which the governing
    /// rule forbids. Both halves are asserted in one case because a build
    /// that coloured everything and a build that coloured nothing each pass
    /// one half.
    #[test]
    fn a_dog_action_row_colours_a_status_and_never_a_sentence() {
        let acted = DogEnabledRow {
            name: "log-rotate".to_string(),
            source: DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            },
            shepherd_acted: true,
            status: "online".to_string(),
        };
        let row = &acted.rows_for(coloured(), true)[0];
        assert_eq!(row[1], painted("adopted", Role::Ink3), "SOURCE is chrome");
        assert_eq!(row[3], painted("(o.o) online", Role::Meadow));

        let sentence = "no shepherd running; the config was written";
        let unacted = DogEnabledRow {
            name: "log-rotate".to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: sentence.to_string(),
        };
        let row = &unacted.rows_for(coloured(), true)[0];
        assert_eq!(row[3], sentence, "a sentence is left exactly as it was");
    }

    /// fails if the SHEPHERD column starts carrying a colour, or if NAME
    /// does.
    ///
    /// Deliberate, and it was the closest call in that table. `false` is
    /// worth knowing, but the STATUS cell beside it already says so in a
    /// whole sentence, and a colour that repeats its neighbour is
    /// decoration.
    #[test]
    fn a_dog_action_row_leaves_the_name_and_the_shepherd_column_plain() {
        let row = &DogDisabledRow {
            name: "log-rotate".to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: "no shepherd running".to_string(),
        }
        .rows_for(coloured(), true)[0];
        assert_eq!(row[0], "log-rotate");
        assert_eq!(row[2], "false");
    }

    /// fails if `rehome`'s own `-` SOURCE stops being muted.
    ///
    /// `DogRehomedRow` is the only one of the four whose SOURCE can be
    /// absent, and it reaches the same muting either way -- it is chrome
    /// when it says `adopted` and a placeholder when it says `-`.
    #[test]
    fn a_rehomed_row_with_nothing_to_forget_still_mutes_its_source() {
        let row = &DogRehomedRow {
            name: "metrics".to_string(),
            source: None,
            shepherd_acted: true,
            status: "stopped".to_string(),
        }
        .rows_for(coloured(), true)[0];
        assert_eq!(row[1], painted("-", Role::Ink3));
        assert_eq!(row[3], painted("(-.-) stopped", Role::Ink3));
    }

    /// fails if `flush`'s table stops muting its ID, or starts colouring a
    /// real path.
    ///
    /// A path is the SUBJECT of this table rather than a reading about it:
    /// no threshold to ramp against, no fault to mark, and the widest column
    /// on the row. Only the `-` a peer daemon predating the field produces
    /// is muted.
    #[test]
    fn a_flushed_row_mutes_its_id_and_its_dash_and_leaves_a_path_alone() {
        let mut without = sample_info(1, "cron", 0);
        without.out_file = None;
        without.err_file = None;
        let rows =
            FlushedRows(vec![sample_info(0, "web", 60_000), without]).rows_for(coloured(), true);

        assert_eq!(rows[0][0], painted("0", Role::Ink3), "ID is chrome");
        assert_eq!(rows[0][1], "web", "NAME is plain");
        assert_eq!(
            rows[0][2], "/logs/web-0-out.log",
            "a real path carries no colour"
        );
        assert_eq!(rows[1][2], painted("-", Role::Ink3), "the placeholder does");
        assert_eq!(rows[1][3], painted("-", Role::Ink3));
    }

    /// fails if `LambRows` grows a colour.
    ///
    /// Both its columns are identity and neither has a state, a reading or a
    /// placeholder, so there is nothing for a colour to carry. Pinned rather
    /// than left implicit because "this one is deliberately plain" and "this
    /// one was forgotten" look identical in a diff, and the second is
    /// exactly what happened to the other seven tables.
    #[test]
    fn lamb_rows_carry_no_colour_at_all() {
        let rows = LambRows(vec![Lamb::new(48_302, "node")]).rows_for(coloured(), true);
        assert_eq!(rows[0], vec!["48302".to_string(), "node".to_string()]);
    }

    /// fails if a `ProcStatus` variant is added without being added to
    /// `status_named_by`'s own list.
    ///
    /// That list is what decides whether a dog-action row's STATUS cell is
    /// coloured, and a variant missing from it would silently render plain
    /// rather than fail to compile. Driven off `Display`, so it also fails
    /// if the two ever disagree.
    #[test]
    fn every_status_is_recognised_by_its_own_rendering() {
        for status in [
            ProcStatus::Starting,
            ProcStatus::Online,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
        ] {
            assert_eq!(
                status_named_by(&status.to_string()),
                Some(status),
                "{status} is not recognised by its own rendering"
            );
        }
        assert_eq!(
            status_named_by("no shepherd running"),
            None,
            "and a sentence is not mistaken for one"
        );
    }

    /// fails if `rows_for` stops colouring ID/FOLD as chrome, stops muting a
    /// `-` placeholder, or starts colouring a real (non-dash) PID/SMIT value
    /// it was never asked to. Goes through the real seam (`rows_for`, not
    /// the role helpers directly) because this is the one property that is
    /// about which CELLS get touched, not about a threshold.
    #[test]
    fn chrome_and_placeholder_columns_are_coloured_and_nothing_else_is() {
        use crate::style::{Presentation, StyleLevel};

        let presentation = Presentation::new(
            StyleLevel::Full,
            None,
            Some(std::ffi::OsStr::new("xterm-256color")),
            None,
            200,
        );
        // One row with a real PID and no fold (so PID is a real value, and
        // FOLD is the placeholder) and one without a PID (so PID is the
        // placeholder).
        let mut running = sample_info(0, "web", 60_000);
        running.fold = None;
        let mut stopped = sample_info(1, "cron", 0);
        stopped.pid = None;
        stopped.fold = None;
        let flock = FlockRows(vec![running, stopped]);

        let rows = flock.rows_for(presentation, true);

        // ID: chrome, always coloured.
        assert!(rows[0][0].contains('\u{1b}'), "{:?}", rows[0][0]);
        assert!(rows[1][0].contains('\u{1b}'), "{:?}", rows[1][0]);
        // PID: a real value is left plain; the placeholder is coloured.
        assert!(!rows[0][3].contains('\u{1b}'), "{:?}", rows[0][3]);
        assert!(rows[1][3].contains('\u{1b}'), "{:?}", rows[1][3]);
        // FOLD: chrome, always coloured, `-` here on both rows.
        assert!(rows[0][9].contains('\u{1b}'), "{:?}", rows[0][9]);
        assert!(rows[1][9].contains('\u{1b}'), "{:?}", rows[1][9]);
    }
}
