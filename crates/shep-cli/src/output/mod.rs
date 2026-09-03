//! The versioned output envelope and its two renderings: a JSON envelope
//! (`--format json`) and a padded table (`--format table`, the default).
//!
//! [`Render`] is the single source of truth for both — a payload type
//! implements it once, in [`rows`], and [`emit`] renders it either way from
//! that one impl. A field added to `Serialize` and forgotten in `rows()`
//! fails that type's anti-drift test rather than silently vanishing from the
//! table; see `rows`'s own test module.
//!
//! `bleats` is the one command that does not go through this module: a
//! follow has no end, so there is nothing to wrap in an envelope. It emits
//! its own newline-delimited JSON instead.
//!
//! Pure tier (spec §11): this module and its submodules name no shep-client
//! type, compile on every target, and their unit tests run on Windows.

// `pub(crate)`, not private like the other three: `lookout::theme`'s own
// test module (`#[cfg(unix)]`) calls `paint::style_for` directly, to pin
// the anstyle and ratatui colour bindings against each other. Neither
// `lookout` nor `style` is a descendant of `output`, so this name has to be
// reachable from outside it.
pub(crate) mod paint;
mod rows;
mod table;
// `pub(crate)` for `width::char_columns`, which `lookout::view::flock::fit`
// pads by: one rule for how wide a `char` draws, shared by the two surfaces
// that pad a cell, rather than a second copy that drifts on the first
// double-width name. The same reasoning `paint` above is public for.
pub(crate) mod width;

use std::io;

use serde::Serialize;
use shep_core::protocol::ProcessInfo;

use crate::exit::ExitCode;

// Re-exported for `commands/`, which names every one of these at its own
// crate-root import (`crate::output::{Streams, emit, FlockRows, ...}`).
// Tasks 7-11 have landed and every one of them is genuinely used there on
// unix. `commands/` itself is `#[cfg(unix)]`-gated (main.rs), so on Windows
// none of them is named anywhere and `unused_imports` (a name-resolution
// lint, unlike `dead_code`'s reachability one) still flags it there —
// narrowed to that target rather than dropped.
#[cfg_attr(windows, allow(unused_imports))]
pub use rows::{
    AvailableDogRows, BarkRows, DeletedIds, DogAdoptedRow, DogDisabledRow, DogEnabledRow,
    DogRehomedRow, DogRows, EmptiedFile, EmptiedFiles, FlockRows, FlushedRows, ImportRow,
    ImportRows, KillRow, KvEntry, KvRows, KvUnsetRow, LambRows, RolledSheep, RolledSheepRows,
    SavedRollRow, SentLineRows, SignalledRows, StartupStep, StartupSteps, TriggeredRows,
};
pub use table::{human_bytes, human_duration, local_timestamp, render_table};

// `pub(crate)`, not part of the block above: `exit_cell` is not one of
// `commands/`'s payload types -- it has exactly one caller outside this
// module, `lookout::view::flock::cell`'s own EXIT column (task 49), reusing
// the same code/signal rendering `FlockRows`'s EXIT column already uses
// rather than a second implementation of the same rule. `#[cfg_attr(windows,
// ...)]` for the same reason the block above carries it: `lookout` is
// `#[cfg(unix)]` (`lib.rs`), so nothing names this import on Windows.
//
// `cfg_cell` (task 12) rides the same re-export for the same reason: its
// only caller outside this module is `lookout::view::flock::cell`'s own
// CFG column, reusing the pending-over-overridden rule rather than a second
// copy of it.
#[cfg_attr(windows, allow(unused_imports))]
pub(crate) use rows::{cfg_cell, exit_cell};

use crate::cli::Format;
use crate::style::Presentation;

/// Bumped only for a breaking change to any command's `data` shape.
/// Additive fields do not bump it.
pub const SCHEMA_VERSION: u32 = 1;

/// The `--format json` envelope every command renders into, `bleats`
/// excepted (module docs above).
///
/// Not constructed outside `emit` and this module's own tests yet: no verb
/// has a real success path until Tasks 7-11 land and start calling `emit`
/// from `commands/`. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct OutputEnvelope<'a, T> {
    /// [`SCHEMA_VERSION`] at the time this envelope was produced.
    pub schema_version: u32,
    /// The verb that produced this envelope (`"flock"`, `"ping"`, ...).
    pub command: &'a str,
    /// The command's own payload.
    pub data: T,
}

/// The two streams a command writes to.
///
/// Production wires the process's own; tests wire a pair of `Vec<u8>`, which
/// is what makes every renderer assertion hermetic and safe under the
/// parallel `cargo test` gate. `&mut dyn Write` has no `Debug`, so this needs
/// a manual one — print `Streams { .. }` and nothing else (pinned by this
/// module's own `streams_debug_is_the_redacted_placeholder` test).
pub struct Streams<'a> {
    /// Rendered command output — what `emit` writes to.
    ///
    /// Read on unix: every real command in `commands/` (Tasks 7-11) passes
    /// `&mut streams.out` to `emit` for its rendered output. `commands/`
    /// itself is `#[cfg(unix)]`-gated (main.rs), so on Windows nothing
    /// reads this field yet and `dead_code` still flags it there —
    /// narrowed to that target rather than dropped.
    #[cfg_attr(windows, allow(dead_code))]
    pub out: &'a mut dyn io::Write,
    /// Diagnostics and errors — what `emit_error` writes to.
    pub err: &'a mut dyn io::Write,
    /// How much this invocation dresses up its output.
    ///
    /// Carried here rather than passed to `emit` on its own because
    /// `Streams` already reaches every command, and a global would break
    /// this crate's rule that presentation inputs are parameters, never a
    /// call inside the function that renders (`commands/daemon.rs`'s
    /// `ansi_enabled` follows the same rule for `NO_COLOR`).
    ///
    /// `Presentation::BARE` is the field's documented safe value for a
    /// construction that wants today's plain output — every test fixture in
    /// the crate uses it — so a construction that reaches for the wrong
    /// default renders exactly what shep printed before this feature, the
    /// safe direction to fail. There is no `Default` impl to enforce that
    /// automatically: `Streams` holds `&mut dyn io::Write`, which is not
    /// `Default`, so every field is always named at the call site regardless.
    pub style: Presentation,
    /// How this invocation renders: a table for a person, or JSON for a
    /// script.
    ///
    /// Carried here for the reason `style` is, one field up: it reaches
    /// every command already, and all 84 functions that took a `Streams`
    /// also took a `Format` beside it. Nothing in production ever passed a
    /// different one, so nothing loses an override it was using.
    pub fmt: Format,
}

impl std::fmt::Debug for Streams<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streams").finish_non_exhaustive()
    }
}

impl Streams<'_> {
    /// Prints `message` as an error, and hands back the code it printed.
    ///
    /// Returning the code is what lets a caller write
    /// `return streams.fail(ExitCode::Usage, &message)` rather than naming
    /// the code twice and risking the two drifting apart.
    ///
    /// The write's own failure is discarded, deliberately: a closed stderr
    /// must not change what shep exits with. That was the decision at all
    /// 91 call sites this replaces, and it is made once here instead.
    pub fn fail(&mut self, code: ExitCode, message: &str) -> ExitCode {
        let _ = emit_error(&mut *self.err, self.fmt, code.code_str(), message);
        code
    }

    /// Prints `message` as a notice, on stdout.
    ///
    /// Discards its write's failure for the same reason [`Self::fail`] does.
    /// Stdout only: a real minority of notices belong on stderr instead (a
    /// warning beside a separate primary output, like `init`'s shadowed-file
    /// notice), and those call [`emit_notice`] directly with `streams.err`:
    /// see that function's own doc for the full rule. This method exists for
    /// the majority shape, a notice that IS the command's whole answer.
    pub fn note(&mut self, code: &str, message: &str) {
        let _ = emit_notice(&mut *self.out, self.fmt, code, message);
    }

    /// Prints `message` as a notice, on stderr.
    ///
    /// The stream is the whole difference from [`Self::note`], and it is a
    /// decision about the reader rather than about severity. `note` carries
    /// what the command produced: `shep init` saying which file it wrote.
    /// This carries what somebody should know about the run without it being
    /// the answer they asked for: a Flockfile that will be shadowed, entries
    /// skipped, strings that had control characters stripped out of them.
    ///
    /// Keeping those off stdout is what lets `shep dogs --available
    /// --format json | jq` work while the operator still sees that two
    /// entries were skipped.
    ///
    /// Discards its write's failure for the same reason [`Self::fail`] does.
    pub fn aside(&mut self, code: &str, message: &str) {
        let _ = emit_notice(&mut *self.err, self.fmt, code, message);
    }
}

/// Implemented once per command payload. The two methods are the ONLY place a
/// field's presence is decided, so a field added to one and forgotten in the
/// other is a compile error rather than a silent divergence.
///
/// Not object-safe: [`headers`](Render::headers) has no receiver and
/// `Serialize` cannot be a dyn-compatible supertrait, so `Box<dyn Render>`
/// does not compile. Every call site knows its payload type statically;
/// [`emit`] dispatches generically, never dynamically.
///
/// Not used outside this module's own tests yet: `commands/` — the code
/// that will implement it per payload type and call `emit` — is Tasks 7-11.
/// `#[allow(dead_code)]` says so explicitly rather than inventing a call
/// site nothing needs yet.
#[allow(dead_code)]
pub trait Render: Serialize {
    /// Column headers for table output.
    fn headers() -> &'static [&'static str];
    /// One row per record, cells in `headers()` order.
    fn rows(&self) -> Vec<Vec<String>>;
    /// The rows as this presentation wants them rendered.
    ///
    /// Defaults to [`Self::rows`]: a table with nothing to dress up says so
    /// by not implementing this. An impl that wants colour overrides it and
    /// calls [`rows::paint`], which keys each cell's treatment on the
    /// column's NAME.
    ///
    /// The default deliberately does NOT apply the `-` placeholder rule, even
    /// though that rule holds for every table that has a placeholder. It was
    /// written that way first, and the mutation that deleted it killed no
    /// test: every table in the crate that can actually render a `-` already
    /// overrides this and reaches the rule through [`rows::Paint::Default`],
    /// and the seven that do not override it cannot produce a dash at all. A
    /// default nothing reaches is a path that rots unwatched, so the rule
    /// lives in `paint` alone, where it is exercised.
    ///
    /// # Why by name, and never by index
    ///
    /// Painting by hardcoded index (`row[0]`, `row[4]`, ...) is a fact about
    /// one table's column ORDER, so reordering its columns would silently
    /// repoint every one of them: the wrong cells get painted, nothing
    /// fails to compile, and no test catches it -- a snapshot pins whatever
    /// was accepted into it, and only a human looking at a rendered table
    /// would notice RESTARTS had started wearing the memory ramp. Keyed on
    /// the name instead, one place says RESTARTS is coloured by
    /// `restarts_role`, and every table
    /// carrying a RESTARTS column gets it wherever the column sits.
    ///
    /// Only ever called from `table_of`'s
    /// boxed path — the plain path keeps calling [`render_table`], which
    /// keeps calling [`Self::rows`], and that is what makes `bare` provably
    /// byte-identical rather than merely intended to be.
    ///
    /// `status_word` is a plain parameter rather than a field on
    /// `Presentation`: it is `table_of`'s own per-attempt retry knob (spec
    /// §2's word-drops-before-a-column rule), never a fact resolved once at
    /// the seam the way every `Presentation` field is. Only a STATUS column
    /// reads it, so the retry `table_of` makes is a harmless no-op for a
    /// table that has none.
    fn rows_for(&self, _presentation: Presentation, _status_word: bool) -> Vec<Vec<String>> {
        self.rows()
    }
    /// Table header -> JSON key, the documented name mapping
    /// (`UPTIME` -> `uptime_ms`, and so on).
    ///
    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values. Every real
    /// caller (the anti-drift tests, [`render_table`]) only ever passes a
    /// value straight from `headers()`, so this is unreachable in practice —
    /// implementations still document and mark it, per house style for any
    /// panic reachable from a public signature.
    fn json_key_for(header: &str) -> &'static str;
    /// Serialized fields that legitimately have no column, each with a
    /// comment giving the reason. Usually empty.
    ///
    /// This constant is the only thing standing between an unmapped
    /// `Serialize` field and a silently-widened, unreviewed pass of
    /// `assert_no_drift` (rows.rs) — an entry proves the *count* of covered
    /// keys matches, never *why* a field belongs here. Every entry an impl
    /// adds MUST carry its own inline `//` comment stating that reason
    /// (`"note", // internal only, never shown to a user`); an entry with no
    /// comment is a review gap, not a pass.
    const JSON_ONLY: &'static [&'static str];

    /// Per-column drop priority for [`table::render_boxed`], parallel to
    /// [`Self::headers`]: index `i` here is the priority of column `i` there.
    /// `0` never drops — [`table::render_boxed`]'s own floor. The default is
    /// all zeros, kept for a hypothetical future payload with genuinely
    /// nothing to say about droppability -- leaving a real impl at the
    /// default silently opts it out of narrowing rather than narrowing in an
    /// order nobody chose, which is exactly what happened here: every impl
    /// in [`rows`] but [`rows::FlockRows`] carried the default for a full
    /// task, until the empirical reviewer caught [`rows::DogRows`] wrapping
    /// and breaking its own borders under a real terminal, right beneath a
    /// sheep table narrowing gracefully above it. The other eighteen impls
    /// carried the same defect, unnoticed because nothing had looked.
    ///
    /// The one rule every impl in [`rows`] now follows, so a reader who
    /// learns one table is not surprised by the next:
    /// - `0`: the columns that identify a row -- what names the record,
    ///   plus a STATUS/OUTCOME/RESULT-shaped column stating what happened to
    ///   it, when the table has one ([`rows::FlockRows`]/[`rows::DogRows`]'s
    ///   own STATUS is the precedent this generalizes).
    /// - `1`-`5`: reserved for the five columns [`rows::FlockRows`] itself
    ///   has (`UPTIME` 1, `PID` 2, `MEM` 3, `RESTARTS` 4, `CPU` 5), used only
    ///   when that exact concept is a column in this table -- never borrowed
    ///   for an unrelated field just because a number happens to be free,
    ///   which is what would stop the number meaning one thing crate-wide.
    /// - `6` and up: every other column, the most droppable of all --
    ///   assigned per table by how genuinely droppable each one is (a long,
    ///   unbounded free-text field before a short, glanceable one), with a
    ///   comment at the array whenever more than one column shares this
    ///   tier.
    ///
    /// `priorities_line_up_with_headers_for_every_render_impl` (`rows.rs`'s
    /// own test module) is the anti-drift gate: it checks every `Render`
    /// impl this crate defines against its own expected floor, so a table
    /// added later without a real `PRIORITIES` fails that test rather than
    /// shipping unable to narrow.
    const PRIORITIES: &'static [u8] = &[];
}

/// Renders `data` to `out` as `fmt` calls for, boxed or plain per `style`.
///
/// Called by every command in `commands/` once it has a real payload to
/// render — `write_outcome(emit(&mut *streams.out, fmt, "<verb>", data,
/// streams.style))` is the shape all of them share.
///
/// # Errors
/// The underlying write failed.
pub fn emit<T: Render>(
    out: &mut dyn io::Write,
    fmt: Format,
    command: &str,
    data: T,
    style: Presentation,
) -> io::Result<()> {
    match fmt {
        Format::Json => {
            let envelope = OutputEnvelope {
                schema_version: SCHEMA_VERSION,
                command,
                data,
            };
            serde_json::to_writer(&mut *out, &envelope)?;
            writeln!(out)
        }
        Format::Table => write!(out, "{}", table_of(&data, style)),
    }
}

/// Renders one [`Render`] payload as [`render_table`] or [`table::render_boxed`],
/// whichever `presentation.level` calls for.
///
/// The one branch [`emit`], [`emit_flock`] and [`emit_described`] all make on
/// every table they render, factored out here so those five call sites --
/// `emit` once, `emit_flock` and `emit_described` twice each, one per table
/// either renders -- stay one decision instead of reimplementing it five
/// times. [`table::render_boxed`]
/// needs a terminal width; `presentation.width` is [`terminal_width`] already
/// resolved once at the seam (`lib.rs`'s `run_argv`) rather than re-measured
/// by this function itself -- see [`crate::style::Presentation`]'s own doc
/// for why a value injected here, not a call made here, is what keeps this
/// function testable at any width a test chooses.
///
/// The boxed path renders twice when the first pass drops a column. Spec §2:
/// the STATUS word is the first thing dropped from that column, before any
/// whole column is — so the first attempt asks [`Render::rows_for`] for
/// everything, including the word, and only if [`table::render_boxed_ex`]
/// says it had to hide a column does a second attempt ask again with the
/// word turned off. Two render passes over one small table is nothing, and
/// it keeps [`table::render_boxed`] exactly as ignorant of what a STATUS
/// cell is as it was before this function existed — the retry is a second
/// call with different row *data*, never a second code path in the
/// renderer itself. For every payload but [`rows::FlockRows`] the two
/// passes produce identical rows (the default [`Render::rows_for`] ignores
/// its `status_word` parameter entirely), so the retry is a
/// wasted-but-harmless no-op rather than a behaviour change.
fn table_of<T: Render>(data: &T, presentation: Presentation) -> String {
    if !presentation.level.boxes() {
        return render_table(data);
    }
    let headers = T::headers();
    let width = presentation.width;
    let wide = table::render_boxed_ex(
        headers,
        &data.rows_for(presentation, true),
        T::PRIORITIES,
        width,
    );
    if wide.dropped.is_empty() {
        return wide.rendered;
    }
    table::render_boxed_ex(
        headers,
        &data.rows_for(presentation, false),
        T::PRIORITIES,
        width,
    )
    .rendered
}

/// The terminal's width, or 80 when there is not one.
///
/// `crossterm` is a `shep-cli` dependency only inside its `cfg(unix)` block
/// — deliberately, so a Windows build does not link a terminal stack it can
/// never use — so the fallback is unconditional rather than an error path.
/// A width of `0`, which some terminals and CI harnesses report, is treated
/// the same as absent: `render_boxed` would otherwise read it as "drop every
/// droppable column," for no reason a real terminal ever gave it.
///
/// `pub(crate)` rather than private: its one real caller is `lib.rs`'s
/// `run_argv`, which resolves [`crate::style::Presentation::width`] once,
/// at the same seam that resolves the style level and forces
/// [`crate::style::StyleLevel::Bare`] for a pipe -- never [`table_of`]
/// itself. See [`crate::style::Presentation`]'s own doc for why a live call
/// here instead would read the real controlling terminal on every render,
/// including under `cargo test`.
pub(crate) fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        crossterm::terminal::size().map_or(80, |(w, _)| match w {
            0 => 80,
            w => usize::from(w),
        })
    }
    #[cfg(not(unix))]
    {
        80
    }
}

/// Renders one flock listing: the sheep table, then the dogs table beneath
/// it whenever any dog is registered.
///
/// `Format::Json` renders exactly what [`emit`] would for the whole
/// listing — one array, every entry, each carrying its own `dog` marker.
/// The machine surface keeps the single registry the two tables are a
/// rendering OF, so a consumer never has to reassemble one from two.
///
/// `Format::Table` partitions `listing` on [`ProcessInfo::dog`], renders
/// the sheep half through [`table_of::<FlockRows>`](table_of) as `flock`
/// always has, and — only when the dogs half is non-empty — appends a blank
/// line, a `Dogs` caption, and the dogs half through
/// [`table_of::<DogRows>`](table_of). Nothing about widths, padding,
/// char-counting, the empty-payload header rule or the boxed/plain choice is
/// reimplemented here: both calls go through the one [`table_of`] every
/// other payload uses, sized independently because the two tables share no
/// columns. A flock with no dogs prints exactly what it printed before this
/// type existed — no caption, no second table.
///
/// Under the dogs table, and only when a dog is silent, [`silence_pointer`]
/// adds one line naming where that word is explained. A flock whose dogs are
/// all talking prints nothing extra, which is the same rule the `Dogs`
/// caption itself follows.
///
/// # Errors
/// The underlying write failed.
///
/// Its only caller, `commands::query::flock`, lives in `commands/`, which is
/// `#[cfg(unix)]`-gated in `main.rs` — same reason [`Streams::out`] and
/// [`emit_notice`] carry the same attribute.
#[cfg_attr(windows, allow(dead_code))]
pub fn emit_flock(
    out: &mut dyn io::Write,
    fmt: Format,
    command: &str,
    listing: Vec<ProcessInfo>,
    style: Presentation,
) -> io::Result<()> {
    match fmt {
        Format::Json => emit(out, fmt, command, FlockRows(listing), style),
        Format::Table => {
            let (dogs, sheep): (Vec<ProcessInfo>, Vec<ProcessInfo>) =
                listing.into_iter().partition(|p| p.dog.is_some());
            write!(out, "{}", table_of(&FlockRows(sheep), style))?;
            if dogs.is_empty() {
                return Ok(());
            }
            // Read before `DogRows` takes the rows, which is the only
            // reason it is not read after the table is written.
            let pointer = silence_pointer(&dogs);
            write!(out, "\nDogs\n")?;
            write!(out, "{}", table_of(&DogRows(dogs), style))?;
            match pointer {
                None => Ok(()),
                Some(line) => writeln!(out, "\n{line}"),
            }
        }
    }
}

/// The one line under the dogs table that says where `silent` is explained,
/// or nothing at all when no dog is silent.
///
/// **A pointer and not the explanation.** The explanation runs to a
/// paragraph per dog (`vocabulary::silence_note`) and this table is the
/// thing an operator leaves running in a loop — the same argument
/// `ProcessInfo::lambs` makes for not walking the process tree on every
/// listing. Three silent dogs would put three paragraphs under a
/// three-row table on every refresh, which is how a warning stops being
/// read.
///
/// **Outside the table, deliberately.** Nothing here touches a column, a
/// width or a drop priority: the table is rendered and finished before this
/// runs, so a long list of names wraps in the terminal rather than
/// squeezing STATUS off the side of it.
///
/// Named rather than counted. "1 dog is silent" would make the operator go
/// find which, and the names are what they type into the next command.
fn silence_pointer(dogs: &[ProcessInfo]) -> Option<String> {
    let silent: Vec<&str> = dogs
        .iter()
        .filter(|dog| rows::silence_note(dog).is_some())
        .map(|dog| dog.name.as_str())
        .collect();
    match silent.as_slice() {
        [] => None,
        [only] => Some(format!(
            "`{only}` is silent -- its process is up and it has never answered this shepherd. \
             Run `shep describe {only}` for what that means and what to do about it."
        )),
        many => Some(format!(
            "these dogs are silent -- their processes are up and they have never answered this \
             shepherd: {}. Run `shep describe <name>` for what that means and what to do about \
             it.",
            many.join(", ")
        )),
    }
}

/// Renders one `describe` answer: the sheep table, then each sheep's lamb
/// tree beneath it when the reply walked for one and found any.
///
/// `Format::Json` renders exactly what [`emit`] would for the whole
/// listing — one array, every row carrying its own `lambs`. The machine
/// surface keeps the single listing the tables are a rendering OF, so a
/// consumer never has to reassemble one; the same rule [`emit_flock`]
/// follows for dogs.
///
/// `Format::Table` renders the sheep through
/// [`table_of::<FlockRows>`](table_of), exactly as `describe` always has,
/// then — only for a sheep whose `lambs` is `Some` and non-empty — a blank
/// line, a caption, and that sheep's lambs through
/// [`table_of::<LambRows>`](table_of).
///
/// A sheep with no lambs, and a sheep whose reply did not walk for any,
/// both print exactly what `describe` printed before this function
/// existed: no caption, no second table.
///
/// # The silence note
///
/// A row whose STATUS reads `silent` also gets a paragraph, between the
/// table and the lamb trees. This is the per-entity view, so it is where the
/// long form belongs: `shep flock` points here (see [`silence_pointer`]) and
/// this is what it points at. [`crate::vocabulary::silence_note`] owns every
/// word of it, including the part that matters most — whether this shepherd
/// is still waiting on the dog or has permanently given up on it, which is a
/// latch no surface reported at all before it was put on the wire.
/// After the lambs, the same table walk prints a Pending heading and an
/// Overridden heading per sheep that has either (task 12), each followed by
/// the field names `ProcessInfo::pending`/`ProcessInfo::overridden` carry.
/// A sheep with neither prints neither heading, unchanged from before those
/// fields existed. The Pending heading names `shep reload <name>` as what
/// promotes it, since that is the one verb that does.
///
/// # The caption
///
/// It names what was walked and what that is not, in one line, because the
/// operator reading this output is reading neither [`Lamb`](shep_core::protocol::Lamb)'s
/// doc nor `--help`. The walk follows parent-pid links; the stop ladder
/// acts on the process group; the two diverge in both directions. Do not
/// shorten the caption to "process tree" — that is the claim this wording
/// exists to avoid.
///
/// # Errors
/// The underlying write failed.
///
/// Its only caller, `commands::query::describe_selector`, lives in
/// `commands/`, which is `#[cfg(unix)]`-gated in `main.rs` — same reason
/// [`emit_flock`] carries the same attribute.
#[cfg_attr(windows, allow(dead_code))]
pub fn emit_described(
    out: &mut dyn io::Write,
    fmt: Format,
    command: &str,
    listing: Vec<ProcessInfo>,
    style: Presentation,
) -> io::Result<()> {
    match fmt {
        Format::Json => emit(out, fmt, command, FlockRows(listing), style),
        Format::Table => {
            let flock = FlockRows(listing);
            write!(out, "{}", table_of(&flock, style))?;
            // Before the lamb trees, because this explains a cell in the
            // table directly above it and a lamb table would put a second
            // table between the two.
            for sheep in &flock.0 {
                if let Some(note) = rows::silence_note(sheep) {
                    writeln!(out, "\n{note}")?;
                }
            }
            for sheep in &flock.0 {
                let Some(lambs) = &sheep.lambs else {
                    continue;
                };
                if lambs.is_empty() {
                    continue;
                }
                writeln!(
                    out,
                    "\nLambs of {} (id {}) — parent-pid descendants of {}, which is not exactly \
                     the set a stop kills",
                    sheep.name,
                    sheep.id,
                    sheep
                        .pid
                        .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                )?;
                write!(out, "{}", table_of(&LambRows(lambs.clone()), style))?;
            }
            for sheep in &flock.0 {
                if let Some(fields) = sheep.pending.as_deref().filter(|f| !f.is_empty()) {
                    writeln!(
                        out,
                        "\nPending for {} (id {}), parked by a load; `shep reload {}` \
                         promotes it:",
                        sheep.name, sheep.id, sheep.name,
                    )?;
                    for field in fields {
                        writeln!(out, "  {field}")?;
                    }
                }
                if let Some(fields) = sheep.overridden.as_deref().filter(|f| !f.is_empty()) {
                    writeln!(
                        out,
                        "\nOverridden for {} (id {}), fields its current Flockfile does \
                         not declare:",
                        sheep.name, sheep.id,
                    )?;
                    for field in fields {
                        writeln!(out, "  {field}")?;
                    }
                }
            }
            Ok(())
        }
    }
}

/// The `--format json` shape of a failure: `{"schema_version", "error":
/// {"code", "message"}}`.
#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    schema_version: u32,
    error: ErrorBody<'a>,
}

/// The `error` object inside [`ErrorEnvelope`].
#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

/// Renders a failure to `err` in `fmt`. `code` is `ExitCode::code_str()`.
///
/// `code` is a string this function only prints — the exit code stays the
/// caller's — but it prints on both surfaces: JSON already carried it in
/// `error.code`, and table mode used to drop it silently, which left a
/// human at a terminal with no name for the failure a script could see.
///
/// # Errors
/// The underlying write failed.
pub fn emit_error(
    err: &mut dyn io::Write,
    fmt: Format,
    code: &str,
    message: &str,
) -> io::Result<()> {
    // Sanitised here rather than at each caller, because here is the only
    // place every caller passes through: a hostile `Location:` header (see
    // `terminal_safe`'s own doc) is one example of error text that can
    // carry somebody else's bytes, and shep emits colour itself, so a
    // reader cannot tell shep's escapes from an attacker's.
    //
    // Both arms, not just the table one. `serde_json` escapes a control byte
    // into `\u001b` so a terminal never renders it, but a script doing
    // `shep ... --format json | jq -r .error.message` unescapes it straight
    // back onto a terminal.
    //
    // `code` is deliberately not sanitised: every one is a `&'static str`
    // from `ExitCode::code_str` or a literal at the call site, so none of
    // them is ever attacker-supplied.
    //
    // Costs one scan of a message shep is about to print anyway.
    // `terminal_safe::sanitise` returns early when there is nothing
    // unprintable, which is every message shep writes itself.
    let (message, _) = crate::terminal_safe::sanitise(message);
    let message = message.as_str();
    match fmt {
        Format::Json => {
            let envelope = ErrorEnvelope {
                schema_version: SCHEMA_VERSION,
                error: ErrorBody { code, message },
            };
            serde_json::to_writer(&mut *err, &envelope)?;
            writeln!(err)
        }
        Format::Table => writeln!(err, "error[{code}]: {message}"),
    }
}

/// The `--format json` shape of a non-failure diagnostic: `{"schema_version",
/// "notice": {"code", "message"}}`.
///
/// A deliberate sibling of [`ErrorEnvelope`], not a reuse of it: `bleats`'
/// own notices (`log_path_unknown`, `log_unreadable`, `dropped`,
/// `daemon_shutdown`, `lagged`) used to go out through [`emit_error`], whose
/// codes are otherwise exactly [`crate::exit::ExitCode::code_str`]'s
/// taxonomy — a `--format json` consumer parsing stderr had no way to tell
/// "the daemon is shutting down, informationally" from "this command
/// failed", even on a clean run that exits 0. `cli_e2e.rs`'s
/// `assert_json_error` pins the opposite rule for real errors: JSON on
/// stderr means the command failed. A notice must not read that way, so it
/// gets its own envelope key instead of a borrowed one.
///
/// Only ever constructed by [`emit_notice`], whose own doc explains the
/// `#[cfg_attr(windows, allow(dead_code))]` this struct also carries: every
/// one of its callers lives in `commands/` or in `lib.rs`'s `#[cfg(unix)]`
/// arms, so nothing on Windows ever reaches this either.
#[derive(Debug, Serialize)]
#[cfg_attr(windows, allow(dead_code))]
struct NoticeEnvelope<'a> {
    schema_version: u32,
    notice: NoticeBody<'a>,
}

/// The `notice` object inside [`NoticeEnvelope`].
#[derive(Debug, Serialize)]
#[cfg_attr(windows, allow(dead_code))]
struct NoticeBody<'a> {
    code: &'a str,
    message: &'a str,
}

/// Renders a non-failure diagnostic to whichever stream `out` names, in
/// `fmt`, with a different envelope key than [`emit_error`] so a
/// `--format json` consumer can tell a diagnostic from a failure without
/// also cross-referencing the process exit code.
///
/// `out` is a plain parameter rather than always `streams.err`, because a
/// notice plays two different roles depending on the verb: `bleats.rs`'s
/// own follow-mode notices (the daemon shutting down, a lagged read) and
/// `commands::muster::muster`'s "the roll restored nothing" are diagnostic
/// asides beside a *separate* primary output (a log stream, a table), and
/// pass `streams.err` -- the stream [`emit_error`] also uses, since a
/// notice is not a sheep's line and not that separate output either. But
/// `Commands::Style`'s no-table report and `start_bare_shepherd`'s (both
/// `lib.rs`) pass `streams.out`: neither verb renders anything else, so the
/// notice IS the command's whole answer, and belongs where an operator
/// piping this command's real output expects to find it.
///
/// `code` is caller-defined, unlike `emit_error`'s `ExitCode::code_str()`:
/// a notice's code is never part of the exit-code taxonomy — that gap is
/// the whole reason this function exists rather than every notice call site
/// continuing to borrow [`emit_error`].
///
/// Not called outside this module's own tests on Windows: every caller
/// above -- `bleats.rs`/`muster.rs` in `commands/`, `Commands::Style`/
/// `start_bare_shepherd` in `lib.rs` -- is `#[cfg(unix)]`-gated, directly or
/// through `commands/`'s own module-level gate in `main.rs`.
///
/// # Errors
/// The underlying write failed.
#[cfg_attr(windows, allow(dead_code))]
/// A caller that already holds a [`Streams`] and wants stdout can use
/// [`Streams::note`] instead, which supplies the writer and the format.
pub fn emit_notice(
    out: &mut dyn io::Write,
    fmt: Format,
    code: &str,
    message: &str,
) -> io::Result<()> {
    // Sanitised for the reason [`emit_error`] is, one function up.
    let (message, _) = crate::terminal_safe::sanitise(message);
    let message = message.as_str();
    match fmt {
        Format::Json => {
            let envelope = NoticeEnvelope {
                schema_version: SCHEMA_VERSION,
                notice: NoticeBody { code, message },
            };
            serde_json::to_writer(&mut *out, &envelope)?;
            writeln!(out)
        }
        Format::Table => writeln!(out, "notice[{code}]: {message}"),
    }
}

/// Turns the result of an `emit`/`emit_error` write into the exit code that
/// write earned.
///
/// The one rule, stated once so Tasks 7-11 do not each reinvent it at their
/// own `emit` call site: a write failure is [`ExitCode::Failure`], except
/// [`io::ErrorKind::BrokenPipe`], which is [`ExitCode::Success`] —
/// `shep flock | head` closes the pipe on purpose, and that is not a failed
/// command.
///
/// Not called outside this module's own tests yet: `commands/` — the code
/// that will call `emit`/`emit_error` and hand this function their `Result`
/// — is Tasks 7-11. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
#[allow(dead_code)]
#[must_use]
pub fn write_outcome(result: io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::Success,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::Success,
        Err(_) => ExitCode::Failure,
    }
}

#[cfg(test)]
mod tests {
    use shep_core::protocol::{DogSource, Lamb};
    use shep_core::status::ProcStatus;

    use super::*;
    use crate::output::rows::tests::{dog_info, sample_flock, sample_info};

    /// A sheep named `name`, otherwise `rows::tests::sample_info`'s usual
    /// fixture. A thin wrapper rather than reaching for that function
    /// directly: this module's own tests build listings by name (`"web"` a
    /// sheep, `"bark"` a dog), and this is the sheep half of that shape.
    fn sheep_info(name: &str) -> ProcessInfo {
        sample_info(1, name, 60_000)
    }

    /// One sheep (`"web"`), one dog (`"bark"`) — the smallest listing that
    /// exercises `emit_flock`'s split, shared by the three tests below.
    fn mixed_listing() -> Vec<ProcessInfo> {
        vec![sheep_info("web"), dog_info("bark", DogSource::BuiltIn)]
    }

    /// Pins the JSON envelope's exact shape (`--format json` is a stability
    /// surface, same discipline as the wire protocol). A field renamed or
    /// reordered here is a `schema_version` bump, not a silent re-accept.
    #[test]
    fn the_json_envelope_shape_is_pinned() {
        let out = OutputEnvelope {
            schema_version: SCHEMA_VERSION,
            command: "flock",
            data: sample_flock(),
        };
        insta::assert_json_snapshot!(out);
    }

    /// An implementation that always wrote prose (ignoring `fmt`) would fail
    /// this: `--format json` must still be parseable on a failure, not just
    /// on success.
    #[test]
    fn an_error_under_format_json_is_a_parseable_object() {
        let mut err = Vec::new();
        emit_error(
            &mut err,
            Format::Json,
            ExitCode::NotFound.code_str(),
            "no sheep matched",
        )
        .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&err)
            .expect("under --format json a failure must be parseable, not prose");
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        assert_eq!(json["error"]["code"], "not_found");
        assert_eq!(json["error"]["message"], "no sheep matched");
    }

    /// An implementation that always JSON-encoded (ignoring `fmt`) would
    /// fail this: table mode is for a human at a terminal, not a script.
    #[test]
    fn an_error_under_format_table_is_plain_text() {
        let mut err = Vec::new();
        emit_error(
            &mut err,
            Format::Table,
            ExitCode::NotFound.code_str(),
            "no sheep matched",
        )
        .unwrap();
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("no sheep matched"));
        assert!(
            text.contains("not_found"),
            "table mode used to drop `code` silently; a human at a terminal needs the same \
             failure name a script would get from JSON: {text}"
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_err(),
            "table mode is not JSON"
        );
    }

    /// `emit` must not put the envelope wrapper on the table surface, and
    /// must not put the table on the JSON surface. An implementation that
    /// ignored `fmt` and always JSON-encoded would pass both format tests
    /// above individually but fail this one.
    #[test]
    fn emit_honours_the_format_it_is_given() {
        let mut json_out = Vec::new();
        emit(
            &mut json_out,
            Format::Json,
            "flock",
            sample_flock(),
            Presentation::BARE,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(parsed["command"], "flock");
        assert_eq!(parsed["data"].as_array().unwrap().len(), 3);

        let mut table_out = Vec::new();
        emit(
            &mut table_out,
            Format::Table,
            "flock",
            sample_flock(),
            Presentation::BARE,
        )
        .unwrap();
        let text = String::from_utf8(table_out).unwrap();
        assert!(text.contains("NAME"));
        assert!(
            !text.contains("schema_version"),
            "the envelope is a JSON-only concept"
        );
    }

    /// fails if the two populations are rendered into one table, or if the
    /// dogs table is hidden behind a flag. Both halves: the sheep table must
    /// not carry the dog's row, and the dogs table must appear with no flag
    /// at all — a bark dog that has died is precisely what an operator needs
    /// to notice, and hiding it means finding out by NOT being paged.
    #[test]
    fn a_flock_listing_prints_the_dogs_in_their_own_table() {
        let mut out = Vec::new();
        emit_flock(
            &mut out,
            Format::Table,
            "flock",
            mixed_listing(),
            Presentation::BARE,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();

        let (sheep_table, dogs_table) = text.split_once("\nDogs\n").expect("a Dogs caption");
        assert!(sheep_table.contains("web"));
        assert!(!sheep_table.contains("bark"), "a dog is not a sheep");
        assert!(dogs_table.contains("bark"));
        assert!(!dogs_table.contains("web"));
        // The dogs table DOES carry an ID column, and its columns line up
        // with the sheep table's for every header the two share. It used to
        // lead with NAME and put SOURCE second, so the two tables printed one
        // under the other disagreed on the position of every column after the
        // first. `DogRows`' own doc carries the ruling.
        assert!(
            dogs_table.starts_with("ID"),
            "the dogs table leads with ID, as the sheep table does: {dogs_table}"
        );
        let shared: Vec<&str> = dogs_table
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .take(9)
            .collect();
        assert_eq!(
            shared,
            [
                "ID", "NAME", "STATUS", "PID", "RESTARTS", "EXIT", "CPU", "MEM", "UPTIME"
            ],
            "the nine shared columns, in the sheep table's own order"
        );
        assert!(
            dogs_table
                .lines()
                .next()
                .unwrap()
                .trim_end()
                .ends_with("SOURCE"),
            "and this table's own column last"
        );
    }

    /// A silent dog: process up, and it has never answered this shepherd.
    /// `given_up` is the latch -- `Some(true)` for a dog the shepherd has
    /// stopped restarting, `Some(false)` for one it is still waiting on,
    /// `None` for a shepherd too old to have an opinion.
    fn silent_dog(name: &str, given_up: Option<bool>) -> ProcessInfo {
        let mut info = dog_info(name, DogSource::BuiltIn);
        info.status = ProcStatus::Online;
        info.handshook = Some(false);
        info.dog_stale = given_up;
        info
    }

    /// fails if `silent` appears in a flock table with nothing to follow it
    /// to.
    ///
    /// The word is right for the cell and wrong on its own: it names a
    /// relationship rather than a state of the process, and an operator
    /// cannot act on it from the table. This one line is the whole of the
    /// fix at this surface -- the paragraph lives in `describe`, and this
    /// says so by name.
    #[test]
    fn a_silent_dog_is_pointed_at_the_view_that_explains_it() {
        let mut out = Vec::new();
        emit_flock(
            &mut out,
            Format::Table,
            "flock",
            vec![sheep_info("web"), silent_dog("log-rotate", Some(true))],
            Presentation::BARE,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("silent"), "the cell still says it: {text}");
        assert!(
            text.contains("shep describe log-rotate"),
            "and the pointer names the dog, so it can be typed: {text}"
        );
    }

    /// fails if the pointer squeezes into the table rather than sitting
    /// under it. The dogs table's columns are pinned by
    /// `a_flock_listing_prints_the_dogs_in_their_own_table`; this pins the
    /// other half of the same rule -- that adding a consequence for `silent`
    /// added no column and moved no cell.
    #[test]
    fn the_silence_pointer_sits_below_the_table_and_changes_no_column() {
        // The SAME dog either way, differing only in whether it has
        // answered. A different dog would widen the NAME column on its own
        // and the comparison below would be measuring the fixture rather
        // than the pointer.
        let silent = vec![sheep_info("web"), silent_dog("log-rotate", Some(true))];
        let mut talking = silent.clone();
        talking[1].handshook = Some(true);
        talking[1].dog_stale = Some(false);

        let render = |listing: Vec<ProcessInfo>| {
            let mut out = Vec::new();
            emit_flock(
                &mut out,
                Format::Table,
                "flock",
                listing,
                Presentation::BARE,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };

        let with_pointer = render(silent);
        let header = with_pointer
            .split_once("\nDogs\n")
            .expect("a Dogs caption")
            .1
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            header,
            render(talking)
                .split_once("\nDogs\n")
                .expect("a Dogs caption")
                .1
                .lines()
                .next()
                .unwrap(),
            "the pointer is prose under the table, never a column in it"
        );
        assert!(
            with_pointer.trim_end().ends_with("what to do about it."),
            "and it comes last, after the table it annotates: {with_pointer}"
        );
    }

    /// fails if a flock whose dogs are all talking grows a line about
    /// silence. The same rule the `Dogs` caption itself follows: a listing
    /// with nothing to report prints exactly what it printed before this
    /// existed.
    #[test]
    fn a_flock_with_no_silent_dog_says_nothing_about_silence() {
        let mut out = Vec::new();
        let mut talking = dog_info("bark", DogSource::BuiltIn);
        talking.handshook = Some(true);
        talking.dog_stale = Some(false);
        emit_flock(
            &mut out,
            Format::Table,
            "flock",
            vec![sheep_info("web"), talking],
            Presentation::BARE,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("silent"), "{text}");
        assert!(!text.contains("shep describe"), "{text}");
    }

    /// fails if `describe` reports a dog the shepherd has given up on the
    /// same way it reports one it is still waiting for.
    ///
    /// This is the whole point of the per-entity view. Both rows read
    /// `silent` in every table shep prints; the difference between them is
    /// whether anything further is going to happen, and until `dog_stale`
    /// reached the wire no surface said. The give-up arm must also NOT name
    /// a cause -- the shepherd wrote what it actually saw into the dog's own
    /// log, and inventing a second account here is the exact bug this phase
    /// was opened for.
    #[test]
    fn describe_says_whether_the_shepherd_has_given_up_on_a_silent_dog() {
        let render = |info: ProcessInfo| {
            let mut out = Vec::new();
            emit_described(
                &mut out,
                Format::Table,
                "describe",
                vec![info],
                Presentation::BARE,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };

        let waiting = render(silent_dog("log-rotate", Some(false)));
        assert!(
            waiting.contains("restarts a dog once"),
            "a dog still inside its budget is told what happens next: {waiting}"
        );
        assert!(
            !waiting.contains("GIVEN UP"),
            "and nothing has been given up on yet: {waiting}"
        );

        let given_up = render(silent_dog("log-rotate", Some(true)));
        assert!(
            given_up.contains("GIVEN UP"),
            "the latch is the thing no other surface reports: {given_up}"
        );
        assert!(
            given_up.contains("shep bleats log-rotate"),
            "and it sends the reader to the log that holds the evidence: {given_up}"
        );
        assert!(
            !given_up.contains("rebuild or reinstall it and run"),
            "it must not restate the daemon's verdict, which it cannot know: {given_up}"
        );

        let unknown = render(silent_dog("log-rotate", None));
        assert!(
            unknown.contains("too old to say"),
            "an older shepherd's silence about the latch is reported, not guessed: {unknown}"
        );
    }

    /// fails if a sheep, or a dog that is talking, picks up a paragraph it
    /// has no use for. `describe` is the verb an operator runs on one
    /// healthy sheep constantly, and a note under every one of those would
    /// be the surest way to stop the note being read.
    #[test]
    fn describe_says_nothing_extra_about_a_row_that_is_not_silent() {
        let mut talking = dog_info("bark", DogSource::BuiltIn);
        talking.handshook = Some(true);
        talking.dog_stale = Some(false);

        for info in [sheep_info("web"), talking] {
            let mut out = Vec::new();
            emit_described(
                &mut out,
                Format::Table,
                "describe",
                vec![info],
                Presentation::BARE,
            )
            .unwrap();
            let rendered = String::from_utf8(out).unwrap();
            assert!(!rendered.contains("never answered"), "{rendered}");
        }
    }

    /// fails if the JSON surface is split to match the tables. The machine
    /// surface IS the single registry — one array, every entry, each
    /// carrying its own marker — and a consumer that had to reassemble one
    /// from two would be paying for a rendering decision.
    #[test]
    fn the_json_surface_stays_one_array_of_every_entry() {
        let mut out = Vec::new();
        emit_flock(
            &mut out,
            Format::Json,
            "flock",
            mixed_listing(),
            Presentation::BARE,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["data"].as_array().unwrap().len(), 2);
        assert_eq!(json["data"][0]["dog"], serde_json::Value::Null);
        assert_eq!(json["data"][1]["dog"]["kind"], "built_in");
    }

    /// fails if a flock with no dogs prints an empty second table. An empty
    /// table still prints its header row (`render_table`'s own rule), so a
    /// caption and a bare header line would appear under every listing on
    /// every machine running no dogs at all.
    #[test]
    fn a_flock_with_no_dogs_prints_one_table_and_no_caption() {
        let mut out = Vec::new();
        emit_flock(
            &mut out,
            Format::Table,
            "flock",
            vec![sheep_info("web")],
            Presentation::BARE,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Dogs"));
    }

    /// fails if the caption stops saying what the list is not. This is the
    /// whole honesty requirement for the feature: the walk is a parent-pid
    /// tree and the kill is a process group, they diverge in both
    /// directions, and the operator reading the table is reading neither
    /// the type doc nor `--help`.
    #[test]
    fn the_lamb_caption_does_not_promise_the_kill_set() {
        let info = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .lambs(Some(vec![Lamb::new(4243, "node")]))
            .build();
        let mut out = Vec::new();
        emit_described(
            &mut out,
            Format::Table,
            "describe",
            vec![info],
            Presentation::BARE,
        )
        .unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert!(rendered.contains("parent-pid descendants"), "{rendered}");
        assert!(
            rendered.contains("not exactly the set a stop kills"),
            "{rendered}"
        );
        // And the row itself, so the caption is not the only thing being
        // asserted.
        assert!(rendered.contains("4243"), "{rendered}");
        assert!(rendered.contains("node"), "{rendered}");
    }

    /// fails if a sheep with no lambs grows an empty section. `describe`
    /// printed one table before this task and must print exactly that for
    /// the overwhelmingly common sheep — the same rule `emit_flock` follows
    /// for a flock with no dogs.
    #[test]
    fn a_sheep_with_no_lambs_renders_exactly_what_it_did_before() {
        let bare = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .build();
        let walked_empty = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .lambs(Some(Vec::new()))
            .build();

        for info in [bare, walked_empty] {
            let mut out = Vec::new();
            emit_described(
                &mut out,
                Format::Table,
                "describe",
                vec![info.clone()],
                Presentation::BARE,
            )
            .unwrap();
            let rendered = String::from_utf8(out).unwrap();
            assert!(!rendered.contains("Lambs of"), "{rendered}");
        }
    }

    /// fails if the JSON surface changes shape. `--format json` stays one
    /// array of `ProcessInfo`, each row carrying its own `lambs` — a
    /// consumer must not have to reassemble a listing out of two payloads,
    /// which is the same rule `emit_flock`'s own JSON arm follows for dogs.
    #[test]
    fn the_json_surface_stays_one_array_with_lambs_on_each_row() {
        let info = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .lambs(Some(vec![Lamb::new(4243, "node")]))
            .build();
        let mut out = Vec::new();
        emit_described(
            &mut out,
            Format::Json,
            "describe",
            vec![info],
            Presentation::BARE,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let rows = value["data"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["lambs"][0]["pid"], 4243);
    }

    /// `Streams` carries `&mut dyn io::Write`, which has no `Debug` of its
    /// own, so the manual impl is the only thing standing between a future
    /// refactor and either a compile error or (worse, if someone works
    /// around it) a `Debug` that leaks whatever the streams happen to hold.
    /// Precedent: `shep-core/src/config/app.rs`'s `debug_redacts_env_values`
    /// (IR-41 — exact-string pin so a lazy derive can't slip back in).
    #[test]
    fn streams_debug_is_the_redacted_placeholder() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        assert_eq!(format!("{streams:?}"), "Streams { .. }");
    }

    #[test]
    fn write_outcome_treats_a_broken_pipe_as_success() {
        // `shep flock | head` closes the pipe on purpose; that is not a
        // failed command.
        let broken = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(write_outcome(Err(broken)), ExitCode::Success);
    }

    #[test]
    fn write_outcome_treats_every_other_write_error_as_failure() {
        let other = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(write_outcome(Err(other)), ExitCode::Failure);
    }

    #[test]
    fn write_outcome_treats_ok_as_success() {
        assert_eq!(write_outcome(Ok(())), ExitCode::Success);
    }

    /// Item 4 (whole-branch review): a notice's JSON envelope must key on
    /// `notice`, not `error` — a consumer parsing `--format json` stderr
    /// needs to tell a diagnostic from a failure without also having to
    /// know the process exit code.
    #[test]
    fn a_notice_under_format_json_uses_the_notice_key_not_the_error_key() {
        let mut err = Vec::new();
        emit_notice(
            &mut err,
            Format::Json,
            "daemon_shutdown",
            "the daemon is shutting down",
        )
        .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&err)
            .expect("under --format json a notice must be parseable, not prose");
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        assert_eq!(json["notice"]["code"], "daemon_shutdown");
        assert_eq!(json["notice"]["message"], "the daemon is shutting down");
        assert!(
            json.get("error").is_none(),
            "a notice must not also carry an `error` key: {json}"
        );
    }

    /// The table-mode sibling of the JSON test above: `notice[code]:
    /// message`, not `error[code]: message` — the same visual grammar
    /// `emit_error` uses, but a different word, so a human at a terminal can
    /// tell the two apart at a glance too.
    #[test]
    fn a_notice_under_format_table_is_plain_text_prefixed_notice() {
        let mut err = Vec::new();
        emit_notice(
            &mut err,
            Format::Table,
            "dropped",
            "the daemon dropped 3 events",
        )
        .unwrap();
        let text = String::from_utf8(err).unwrap();
        assert!(text.starts_with("notice[dropped]:"), "{text}");
        assert!(text.contains("the daemon dropped 3 events"));
    }

    // --- cli-plumbing-ergonomics Task 1: pin the wire bytes -------------
    //
    // The refactor in flight (Tasks 2-3 of that plan) touches 91
    // `emit_error` call sites, 84 signatures and 66 `map_err`s, exactly
    // the diff shape where a behaviour change hides in the noise. These
    // three tests snapshot the literal bytes `emit_error`/`emit_notice`
    // write today, in both formats, so that refactor has something byte-
    // exact to answer to instead of "the suite is still green."

    /// `emit_error`'s two renderings, side by side: `error[code]: message`
    /// on the table surface, the `ErrorEnvelope` JSON object on the other.
    #[test]
    fn no_escape_reaches_a_stream_through_either_emitter() {
        // The class fix. `fetch.rs` sanitises the two error texts that come
        // off the wire today, but the guarantee has to live where every
        // caller passes through, or the next one to carry somebody else's
        // bytes reintroduces the hole silently.
        let hostile = "cleared\u{1b}[2Jand\u{1b}]0;retitled\u{7}";
        for fmt in [Format::Table, Format::Json] {
            for (what, mut out) in [("error", Vec::new()), ("notice", Vec::new())] {
                if what == "error" {
                    emit_error(&mut out, fmt, "failure", hostile).unwrap();
                } else {
                    emit_notice(&mut out, fmt, "whatever", hostile).unwrap();
                }
                assert!(
                    !out.contains(&0x1b),
                    "{what} in {fmt:?} let an ESC through: {:?}",
                    String::from_utf8_lossy(&out)
                );
                assert!(
                    !out.contains(&0x07),
                    "{what} in {fmt:?} let a BEL through: {:?}",
                    String::from_utf8_lossy(&out)
                );
            }
        }
    }

    #[test]
    fn what_an_error_looks_like_on_the_wire() {
        for (fmt, name) in [(Format::Table, "table"), (Format::Json, "json")] {
            let mut out = Vec::new();
            emit_error(
                &mut out,
                fmt,
                ExitCode::Usage.code_str(),
                "no flock at /tmp/x",
            )
            .unwrap();
            insta::assert_snapshot!(format!("error_{name}"), String::from_utf8(out).unwrap());
        }
    }

    /// `emit_notice`'s two renderings: `notice[code]: message` on the table
    /// surface, the `NoticeEnvelope` JSON object on the other. The
    /// `notice` key is the whole reason this function exists rather than
    /// reusing `emit_error`, so its shape belongs in the baseline too.
    #[test]
    fn what_a_notice_looks_like_on_the_wire() {
        for (fmt, name) in [(Format::Table, "table"), (Format::Json, "json")] {
            let mut out = Vec::new();
            emit_notice(&mut out, fmt, "init", "wrote /tmp/x/Flockfile.toml").unwrap();
            insta::assert_snapshot!(format!("notice_{name}"), String::from_utf8(out).unwrap());
        }
    }

    /// Quotes and a backslash render differently in the two formats (JSON
    /// escapes them, the table surface prints them raw), so a message
    /// carrying both is what would catch a change to either rendering path
    /// that a plain-ASCII message would not.
    #[test]
    fn an_error_message_with_awkward_bytes_survives_both_formats() {
        for (fmt, name) in [(Format::Table, "table"), (Format::Json, "json")] {
            let mut out = Vec::new();
            emit_error(
                &mut out,
                fmt,
                ExitCode::InvalidConfig.code_str(),
                r#"bad "quoted" \path"#,
            )
            .unwrap();
            insta::assert_snapshot!(
                format!("error_awkward_{name}"),
                String::from_utf8(out).unwrap()
            );
        }
    }

    // --- Task 5b: colour, and the face in the STATUS column ------------

    use std::ffi::OsStr;

    use crate::style::StyleLevel;

    /// Spec §5: `NO_COLOR` removes colour at `full`, leaving sheep and
    /// boxes alone. Asserted on the rendered STRING, not on the resolved
    /// [`Presentation`]: the struct could fold `NO_COLOR` in correctly and
    /// a bug in `rows::status_cell` could still emit an escape regardless.
    #[test]
    fn no_color_at_full_keeps_sheep_and_boxes_but_drops_colour() {
        let presentation =
            Presentation::new(StyleLevel::Full, Some(OsStr::new("1")), None, None, 80);
        assert!(
            !presentation.colour,
            "NO_COLOR must veto colour even at full"
        );

        let flock = FlockRows(vec![
            ProcessInfo::builder(1, "web", ProcStatus::Online).build(),
        ]);
        let rendered = table_of(&flock, presentation);

        assert!(
            rendered.contains("(o.o)"),
            "full still draws the face: {rendered}"
        );
        assert!(rendered.contains('┌'), "full still draws boxes: {rendered}");
        assert!(
            !rendered.contains('\u{1b}'),
            "NO_COLOR must leave no escape byte: {rendered:?}"
        );
    }

    /// The byte-identical rule, made mechanical: `bare` must never emit an
    /// ANSI escape, regardless of status or how loud the environment's
    /// colour support would otherwise be.
    #[test]
    fn bare_emits_no_escape_at_all() {
        let flock = FlockRows(vec![
            ProcessInfo::builder(1, "web", ProcStatus::Errored).build(),
        ]);
        let rendered = table_of(&flock, Presentation::BARE);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(
            rendered.contains("errored"),
            "today's plain word survives: {rendered}"
        );
        assert!(!rendered.contains("(x.x)"), "no face at bare: {rendered}");
    }

    /// Spec §2: the face appears at `full`; at `plain` the plain word alone
    /// does (`plain` is "no sheep", not "no colour" — colour still
    /// survives); neither survives at `bare`.
    ///
    /// Also the demo this task's brief asks for: run with `-- --nocapture`
    /// and read what each level actually looks like. An exact-string test
    /// proves the code matches a string, not that the result is legible —
    /// `welcome.rs` shipped a silently unaligned sheep past a passing one,
    /// because the expected value was written with the same mistake this
    /// printed output lets a human actually catch.
    #[test]
    fn the_three_levels_render_the_status_column_differently_and_look_right() {
        let flock = FlockRows(vec![
            ProcessInfo::builder(1, "web", ProcStatus::Online).build(),
            ProcessInfo::builder(2, "worker", ProcStatus::Errored).build(),
            ProcessInfo::builder(3, "cron", ProcStatus::Stopped).build(),
        ]);

        let full = table_of(
            &flock,
            Presentation::new(
                StyleLevel::Full,
                None,
                Some(OsStr::new("xterm-256color")),
                None,
                80,
            ),
        );
        println!("--- full ---\n{full}");
        assert!(full.contains("(o.o)"), "{full}");
        assert!(full.contains("(x.x)"), "{full}");
        assert!(full.contains("(-.-)"), "{full}");
        assert!(
            full.contains('\u{1b}'),
            "full at a deep terminal colours the cell: {full:?}"
        );

        let plain = table_of(
            &flock,
            Presentation::new(
                StyleLevel::Plain,
                None,
                Some(OsStr::new("xterm-256color")),
                None,
                80,
            ),
        );
        println!("--- plain ---\n{plain}");
        assert!(!plain.contains("(o.o)"), "no face at plain: {plain}");
        assert!(plain.contains("online"), "{plain}");
        assert!(plain.contains('\u{1b}'), "plain still colours: {plain:?}");

        let bare = table_of(&flock, Presentation::BARE);
        println!("--- bare ---\n{bare}");
        assert!(!bare.contains("(o.o)"), "{bare}");
        assert!(!bare.contains('\u{1b}'), "{bare:?}");
    }

    /// Spec §2: the STATUS word is the first thing dropped from that
    /// column, before any whole column is. `waiting-restart` (15
    /// characters) is the longest status word, chosen so face-plus-word
    /// alone forces a column past a width face-alone comfortably fits —
    /// exercising `Render::rows_for` and `table::render_boxed_ex` directly,
    /// the same two calls `table_of`'s own two-pass retry makes -- `table_of`
    /// could be driven at this same chosen width too now that width is an
    /// injected `Presentation` field rather than a real-terminal read, but
    /// this test stays at the lower level anyway, to pin the exact retry
    /// mechanics rather than `table_of`'s outer wrapping around them.
    ///
    /// Width 90, not this module's usual 80: task 7's `SMIT` column, empty
    /// here and the highest priority number, is what face-alone now needs
    /// dropped at 80 -- the same seven-column cost `output/table.rs`'s own
    /// tests record for adding a column nobody's row fills -- and task 12's
    /// own `CFG` column moved it six columns further still, from 84 to 90,
    /// for the same reason (`output/table.rs`'s own
    /// `full_wide_pins_face_word_and_colour_for_a_mixed_flock` has that
    /// arithmetic).
    #[test]
    fn the_word_drops_before_a_whole_column_does() {
        let flock = FlockRows(vec![
            ProcessInfo::builder(1, "a", ProcStatus::WaitingRestart).build(),
        ]);
        let presentation = Presentation::new(StyleLevel::Full, None, None, None, 90);
        let headers = FlockRows::headers();

        let wide = table::render_boxed_ex(
            headers,
            &flock.rows_for(presentation, true),
            FlockRows::PRIORITIES,
            90,
        );
        assert!(
            !wide.dropped.is_empty(),
            "face-plus-word should already force a drop at 90: {}",
            wide.rendered
        );

        let narrow = table::render_boxed_ex(
            headers,
            &flock.rows_for(presentation, false),
            FlockRows::PRIORITIES,
            90,
        );
        assert!(
            narrow.dropped.is_empty(),
            "face-alone should fit every column at 90: {}",
            narrow.rendered
        );
        assert!(narrow.rendered.contains("FOLD"), "{}", narrow.rendered);
        assert!(narrow.rendered.contains("(>_<)"), "{}", narrow.rendered);
        assert!(
            !narrow.rendered.contains("waiting-restart"),
            "{}",
            narrow.rendered
        );
    }

    /// The JSON arms serialize the payload directly and never call
    /// `rows`/`rows_for` — asserted rather than trusted, since a future
    /// refactor that routed JSON through `rows_for` "for consistency" would
    /// be exactly the byte-identical rule breaking silently.
    #[test]
    fn colour_never_reaches_format_json() {
        let flock = FlockRows(vec![
            ProcessInfo::builder(1, "web", ProcStatus::Errored).build(),
        ]);
        let presentation = Presentation::new(
            StyleLevel::Full,
            None,
            Some(OsStr::new("xterm-256color")),
            None,
            80,
        );
        let mut out = Vec::new();
        emit(&mut out, Format::Json, "flock", flock, presentation).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\u{1b}'), "{text}");

        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["data"][0]["status"], "errored");
    }
}
