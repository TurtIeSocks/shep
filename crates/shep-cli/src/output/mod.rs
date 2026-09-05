//! The versioned output envelope and its two renderings: a JSON envelope
//! (`--format json`) and a padded table (`--format table`, the default).
//!
//! [`Render`] is the single source of truth for both: a payload type
//! implements it once, in [`rows`], and [`emit`] renders it either way. A
//! field added to `Serialize` and forgotten in `rows()` fails that type's
//! anti-drift test rather than silently vanishing from the table.
//!
//! `bleats` does not go through this module: a follow has no end, so it
//! emits its own newline-delimited JSON instead of an envelope.
//!
//! This module names no shep-client type and compiles on every target.

// `pub(crate)`: `lookout::theme`'s test module calls `paint::style_for`
// directly to pin the anstyle and ratatui colour bindings against each
// other, and neither `lookout` nor `style` is a descendant of `output`.
pub(crate) mod paint;
mod rows;
mod table;
// `pub(crate)` for `width::char_columns`, which `lookout::view::flock::fit`
// pads by: one rule for how wide a `char` draws, shared by the two surfaces
// that pad a cell, rather than a second copy that drifts on the first
// double-width name. The same reasoning `paint` above is public for.
pub(crate) mod width;

use std::collections::BTreeSet;
use std::io;

use serde::Serialize;
use shep_core::protocol::ProcessInfo;

use crate::exit::ExitCode;

// Re-exported for `commands/`, which names every one of these at its own
// crate-root import. `commands/` is `#[cfg(unix)]`-gated, so on Windows
// nothing names them and `unused_imports` still flags it there.
#[cfg_attr(windows, allow(unused_imports))]
pub use rows::{
    AvailableDogRows, BarkRows, DeletedIds, DogAdoptedRow, DogDisabledRow, DogEnabledRow,
    DogRehomedRow, DogRows, EmptiedFile, EmptiedFiles, FlockRows, FlushedRows, ImportRow,
    ImportRows, KillRow, KvEntry, KvRows, KvUnsetRow, LambRows, RolledSheep, RolledSheepRows,
    SavedRollRow, SentLineRows, SignalledRows, StartupStep, StartupSteps, TriggeredRows,
};
pub use table::{human_bytes, human_duration, local_timestamp, render_table};

// `pub(crate)`, not part of the block above: `exit_cell` and `cfg_cell`
// have one caller each outside this module, `lookout::view::flock::cell`'s
// EXIT and CFG columns. `lookout` is `#[cfg(unix)]`, so nothing names
// these on Windows.
#[cfg_attr(windows, allow(unused_imports))]
pub(crate) use rows::{cfg_cell, exit_cell};

use crate::cli::Format;
use crate::style::Presentation;

/// Bumped only for a breaking change to any command's `data` shape.
/// Additive fields do not bump it.
pub const SCHEMA_VERSION: u32 = 1;

/// The `--format json` envelope every command renders into, `bleats`
/// excepted (module docs above).
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
/// a manual one: print `Streams { .. }` and nothing else (pinned by this
/// module's own `streams_debug_is_the_redacted_placeholder` test).
pub struct Streams<'a> {
    /// Rendered command output: what `emit` writes to.
    ///
    /// `commands/` is `#[cfg(unix)]`-gated, so on Windows nothing reads
    /// this field and `dead_code` still flags it there.
    #[cfg_attr(windows, allow(dead_code))]
    pub out: &'a mut dyn io::Write,
    /// Diagnostics and errors: what `emit_error` writes to.
    pub err: &'a mut dyn io::Write,
    /// How much this invocation dresses up its output.
    ///
    /// Carried here rather than as a global: presentation inputs are
    /// parameters in this crate, never a call inside the rendering
    /// function (`commands/daemon.rs`'s `ansi_enabled` follows the same
    /// rule for `NO_COLOR`).
    ///
    /// `Presentation::BARE` is the safe default: a construction that
    /// reaches for the wrong value renders exactly what shep printed
    /// before this feature existed.
    pub style: Presentation,
    /// How this invocation renders: a table for a person, or JSON for a
    /// script.
    ///
    /// Carried here for the same reason as `style`: it reaches every
    /// command already.
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
    /// Returning the code lets a caller write
    /// `return streams.fail(ExitCode::Usage, &message)` rather than naming
    /// the code twice.
    ///
    /// The write's own failure is discarded: a closed stderr must not
    /// change what shep exits with.
    pub fn fail(&mut self, code: ExitCode, message: &str) -> ExitCode {
        let _ = emit_error(&mut *self.err, self.fmt, code.code_str(), message);
        code
    }

    /// Prints `message` as a notice, on stdout.
    ///
    /// Discards its write's failure for the same reason [`Self::fail`]
    /// does. Stdout only: a minority of notices belong on stderr instead
    /// (a warning beside a separate primary output, like `init`'s
    /// shadowed-file notice), which call [`emit_notice`] directly with
    /// `streams.err`. This method is for the majority shape, a notice
    /// that is the command's whole answer.
    pub fn note(&mut self, code: &str, message: &str) {
        let _ = emit_notice(&mut *self.out, self.fmt, code, message);
    }

    /// Prints `message` as a notice, on stderr.
    ///
    /// The stream is the whole difference from [`Self::note`]: a decision
    /// about the reader, not severity. `note` carries what the command
    /// produced; this carries what somebody should know about the run
    /// without it being the answer they asked for.
    ///
    /// Keeping those off stdout lets `shep dogs --available --format json
    /// | jq` work while the operator still sees that entries were skipped.
    /// Discards its write's failure for the same reason [`Self::fail`]
    /// does.
    pub fn aside(&mut self, code: &str, message: &str) {
        let _ = emit_notice(&mut *self.err, self.fmt, code, message);
    }
}

/// Implemented once per command payload. The two methods are the only
/// place a field's presence is decided, so a field added to one and
/// forgotten in the other is a compile error rather than a silent
/// divergence.
///
/// Not object-safe: [`headers`](Render::headers) has no receiver and
/// `Serialize` cannot be a dyn-compatible supertrait, so `Box<dyn Render>`
/// does not compile. Every call site knows its payload type statically;
/// [`emit`] dispatches generically, never dynamically.
#[allow(dead_code)]
pub trait Render: Serialize {
    /// Column headers for table output.
    fn headers() -> &'static [&'static str];
    /// One row per record, cells in `headers()` order.
    fn rows(&self) -> Vec<Vec<String>>;
    /// The rows as this presentation wants them rendered.
    ///
    /// Defaults to [`Self::rows`]; an override calls [`rows::paint`],
    /// which keys each cell on the column's NAME, not its index, so
    /// reordering columns cannot silently repoint a paint rule. The
    /// default skips the `-` placeholder rule, which every table that
    /// can render a `-` reaches through [`rows::Paint::Default`] instead.
    ///
    /// Only called from `table_of`'s boxed path; the plain path calls
    /// [`render_table`], keeping `bare` byte-identical. `status_word` is
    /// a plain parameter, not a `Presentation` field: it is `table_of`'s
    /// per-attempt retry knob.
    fn rows_for(&self, _presentation: Presentation, _status_word: bool) -> Vec<Vec<String>> {
        self.rows()
    }
    /// Table header -> JSON key, the documented name mapping
    /// (`UPTIME` -> `uptime_ms`, and so on).
    ///
    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    fn json_key_for(header: &str) -> &'static str;
    /// Serialized fields that legitimately have no column, each with a
    /// comment giving the reason. Usually empty.
    ///
    /// This constant is the only thing standing between an unmapped
    /// `Serialize` field and a silently-widened, unreviewed pass of
    /// `assert_no_drift` (rows.rs): an entry proves the count of covered
    /// keys matches, never why a field belongs here. Every entry an impl
    /// adds must carry its own inline `//` comment stating that reason
    /// (`"note", // internal only, never shown to a user`).
    const JSON_ONLY: &'static [&'static str];

    /// Per-column drop priority for [`table::render_boxed`], parallel to
    /// [`Self::headers`]: index `i` here is the priority of column `i`
    /// there. `0` never drops. The default is all zeros; leaving a real
    /// impl at the default silently opts it out of narrowing.
    ///
    /// The rule every impl in [`rows`] follows: `0` for identity and
    /// status-shaped columns, `1`-`5` reserved for [`rows::FlockRows`]'s
    /// own five columns (UPTIME 1, PID 2, MEM 3, RESTARTS 4, CPU 5), `6`
    /// and up for everything else, ranked per table by how droppable it
    /// is. `priorities_line_up_with_headers_for_every_render_impl`
    /// (rows.rs) is the anti-drift gate enforcing it.
    const PRIORITIES: &'static [u8] = &[];
}

/// Renders `data` to `out` as `fmt` calls for, boxed or plain per `style`.
///
/// Called by every command in `commands/` once it has a real payload to
/// render: `write_outcome(emit(&mut *streams.out, fmt, "<verb>", data,
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

/// Renders one [`Render`] payload as [`render_table`] or
/// [`table::render_boxed`], whichever `presentation.level` calls for.
///
/// Factored out so `emit`, `emit_flock` and `emit_described` make this
/// decision once. `presentation.width` is [`terminal_width`] already
/// resolved at the seam, injected here rather than measured, so this
/// stays testable at any width.
///
/// Renders twice when the first pass drops a column: the STATUS word
/// drops first, so a first attempt asks [`Render::rows_for`] with the
/// word on, and only if [`table::render_boxed_ex`] hid a column does a
/// second attempt ask again with the word off.
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
/// `crossterm` is a `shep-cli` dependency only inside its `cfg(unix)`
/// block, so a Windows build does not link a terminal stack it can never
/// use. A width of `0`, which some terminals and CI harnesses report, is
/// treated the same as absent: `render_boxed` would otherwise read it as
/// drop every droppable column.
///
/// `pub(crate)` rather than private: its one caller, `lib.rs`'s
/// `run_argv`, resolves [`crate::style::Presentation::width`] once at the
/// seam, never [`table_of`] itself.
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

/// Renders one flock listing: the sheep table, then the dogs table
/// beneath it whenever any dog is registered.
///
/// JSON stays one array, every entry carrying its own `dog` marker.
/// Table partitions on [`ProcessInfo::dog`], rendering sheep and dogs
/// each through [`table_of`], with a blank line and `Dogs` caption
/// between them only when a dog exists. [`silence_pointer`] adds one
/// line under the dogs table when a dog is silent.
///
/// # Errors
/// The underlying write failed.
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

/// The one line under the dogs table that says where `silent` is
/// explained, or nothing at all when no dog is silent.
///
/// A pointer, not the explanation: that runs to a paragraph per dog
/// (`vocabulary::silence_note`), too much for a table an operator leaves
/// running in a loop. Rendered after the table, outside it, so a long
/// list of names wraps in the terminal rather than squeezing STATUS off
/// the side of it. Named rather than counted, since the names are what
/// the operator types into the next command.
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
/// tree beneath it when the reply walked and found any.
///
/// A silent row also gets a paragraph from
/// [`crate::vocabulary::silence_note`] (what [`silence_pointer`] points
/// at). Pending and Overridden headings follow, once per name, naming
/// `shep reload <name>` as what promotes a parked config. Never shorten
/// the caption to "process tree": the walk follows parent-pid links
/// while the stop ladder acts on the process group, and the two diverge.
///
/// # Errors
/// The underlying write failed.
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
            // Once per name, not once per row: a parked or overridden
            // config belongs to the app, and the daemon writes the same
            // entry onto every slot of a name. The rows themselves stay
            // per instance, since that is a claim about the process.
            let mut said: BTreeSet<&str> = BTreeSet::new();
            for sheep in &flock.0 {
                if !said.insert(sheep.name.as_str()) {
                    continue;
                }
                if let Some(fields) = sheep.pending.as_deref().filter(|f| !f.is_empty()) {
                    writeln!(
                        out,
                        "\nPending for {}, parked by a load; `shep reload {}` promotes it:",
                        sheep.name, sheep.name,
                    )?;
                    for field in fields {
                        writeln!(out, "  {field}")?;
                    }
                }
                if let Some(fields) = sheep.overridden.as_deref().filter(|f| !f.is_empty()) {
                    writeln!(
                        out,
                        "\nOverridden for {}, fields its current Flockfile does not declare:",
                        sheep.name,
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
/// `code` is a string this function only prints, not the exit code, but
/// it prints on both surfaces: JSON carries it in `error.code`, and
/// table mode names it too, so a human at a terminal sees the same
/// failure name a script would.
///
/// # Errors
/// The underlying write failed.
pub fn emit_error(
    err: &mut dyn io::Write,
    fmt: Format,
    code: &str,
    message: &str,
) -> io::Result<()> {
    // Sanitised once here, the only place every caller passes through. Both
    // formats: `jq -r .error.message` would unescape a hostile message
    // right back onto a terminal. `code` is never sanitised, since every
    // caller passes a literal or `ExitCode::code_str()`.
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
/// A sibling of [`ErrorEnvelope`], not a reuse of it: a notice must not
/// read as a failure on the wire, so it gets its own envelope key.
///
/// Only ever constructed by [`emit_notice`]. `#[cfg_attr(windows,
/// allow(dead_code))]`: every caller lives in `commands/` or `lib.rs`'s
/// `#[cfg(unix)]` arms.
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

/// Renders a non-failure diagnostic to `out` in `fmt`, keyed differently
/// than [`emit_error`] so a `--format json` consumer can tell a
/// diagnostic from a failure without checking the exit code.
///
/// `out` is a plain parameter: a notice beside a separate primary output
/// passes `streams.err`; one that is the command's whole answer passes
/// `streams.out`. `code` is caller-defined, never part of
/// `emit_error`'s exit-code taxonomy. A caller already holding a
/// [`Streams`] can use [`Streams::note`] instead.
///
/// # Errors
/// The underlying write failed.
#[cfg_attr(windows, allow(dead_code))]
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
/// A write failure is [`ExitCode::Failure`], except
/// [`io::ErrorKind::BrokenPipe`], which is [`ExitCode::Success`]:
/// `shep flock | head` closes the pipe on purpose, and that is not a
/// failed command.
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

    /// One sheep (`"web"`), one dog (`"bark"`): the smallest listing that
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

    /// The dogs table needs no flag: a dead bark dog is what an operator
    /// needs to notice, and hiding it means finding out by not being
    /// paged.
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
        // The dogs table carries an ID column, and its columns line up
        // with the sheep table's for every header the two share.
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
    /// `given_up` is the latch: `Some(true)` for a dog the shepherd has
    /// stopped restarting, `Some(false)` for one it is still waiting on,
    /// `None` for a shepherd too old to have an opinion.
    fn silent_dog(name: &str, given_up: Option<bool>) -> ProcessInfo {
        let mut info = dog_info(name, DogSource::BuiltIn);
        info.status = ProcStatus::Online;
        info.handshook = Some(false);
        info.dog_stale = given_up;
        info
    }

    /// `silent` names a relationship, not a state, and an operator cannot
    /// act on it from this table alone: the paragraph lives in `describe`.
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

    /// Adding a consequence for `silent` must add no column and move no
    /// cell.
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

    /// The same rule the `Dogs` caption itself follows: a listing with
    /// nothing to report prints nothing extra.
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

    /// Both rows read `silent` everywhere else; only `dog_stale` says
    /// whether anything further is going to happen. The give-up arm must
    /// not name a cause: the shepherd's own account lives in the dog's log.
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

    /// The machine surface is the single registry: one array, every entry
    /// carrying its own marker, never split to match the tables.
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

    /// An empty table still prints its header row, so a caption here
    /// would surface a bare header line under every dogless listing.
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

    /// The same rule `emit_flock` follows for a flock with no dogs.
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

    /// The same rule `emit_flock`'s JSON arm follows for dogs.
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

    /// `shep reload <name>` is the one fact an operator reading this
    /// table cannot get anywhere else.
    #[test]
    fn describe_names_pending_and_overridden_fields_and_the_promoting_verb() {
        let info = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .pending(Some(vec!["env".to_string(), "cwd".to_string()]))
            .overridden(Some(vec!["max_restarts".to_string()]))
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

        assert!(rendered.contains("Pending for web"), "{rendered}");
        assert!(rendered.contains("shep reload web"), "{rendered}");
        assert!(rendered.contains("env"), "{rendered}");
        assert!(rendered.contains("cwd"), "{rendered}");
        assert!(rendered.contains("Overridden for web"), "{rendered}");
        assert!(rendered.contains("max_restarts"), "{rendered}");
    }

    /// The same rule
    /// `a_sheep_with_no_lambs_renders_exactly_what_it_did_before` follows
    /// for lambs.
    #[test]
    fn a_sheep_with_neither_list_renders_neither_heading() {
        // Both spellings of "nothing to say": `None`, and `Some(vec![])`,
        // which is what the store answers for an app with no parked
        // config. The `as_deref` filter is what keeps the second from
        // heading over no fields.
        for (pending, overridden) in [
            (None, None),
            (Some(Vec::new()), Some(Vec::new())),
            (None, Some(Vec::new())),
            (Some(Vec::new()), None),
        ] {
            let info = ProcessInfo::builder(3, "web", ProcStatus::Online)
                .pid(Some(4242))
                .pending(pending.clone())
                .overridden(overridden.clone())
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

            assert!(
                !rendered.contains("Pending for"),
                "{pending:?}/{overridden:?}: {rendered}"
            );
            assert!(
                !rendered.contains("Overridden for"),
                "{pending:?}/{overridden:?}: {rendered}"
            );
        }
    }

    /// `apply_one` writes the same store entry onto every slot of a name,
    /// so three rows carry three identical lists.
    #[test]
    fn a_clustered_app_prints_each_config_section_once() {
        let rows: Vec<ProcessInfo> = (0..3)
            .map(|slot| {
                ProcessInfo::builder(slot, "web", ProcStatus::Online)
                    .pid(Some(4242 + slot))
                    .instance(Some(slot))
                    .pending(Some(vec!["cwd".to_string()]))
                    .overridden(Some(vec!["max_restarts".to_string()]))
                    .build()
            })
            .collect();
        let mut out = Vec::new();
        emit_described(
            &mut out,
            Format::Table,
            "describe",
            rows,
            Presentation::BARE,
        )
        .unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert_eq!(rendered.matches("Pending for web").count(), 1, "{rendered}");
        assert_eq!(
            rendered.matches("Overridden for web").count(),
            1,
            "{rendered}"
        );
    }

    /// The same rule `the_json_surface_stays_one_array_with_lambs_on_each_row`
    /// pins for lambs.
    #[test]
    fn describes_json_surface_carries_pending_and_overridden_on_each_row() {
        let info = ProcessInfo::builder(3, "web", ProcStatus::Online)
            .pid(Some(4242))
            .pending(Some(vec!["env".to_string()]))
            .overridden(Some(vec!["cwd".to_string()]))
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
        assert_eq!(rows[0]["pending"][0], "env");
        assert_eq!(rows[0]["overridden"][0], "cwd");
    }

    /// `Streams` carries `&mut dyn io::Write`, which has no `Debug` of its
    /// own, so the manual impl is the only thing standing between a future
    /// refactor and a `Debug` that leaks whatever the streams hold.
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

    /// A notice's JSON envelope keys on `notice`, not `error`: a consumer
    /// parsing `--format json` stderr must tell a diagnostic from a
    /// failure without also reading the process exit code.
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

    /// `notice[code]: message`, not `error[code]: message`: the same
    /// grammar `emit_error` uses, but a different word.
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

    // --- pin the wire bytes ----------------------------------------------

    // These three tests snapshot the literal bytes `emit_error`/
    // `emit_notice` write, in both formats, so a refactor across call
    // sites has something byte-exact to answer to.

    /// Both emitters, both formats: an ESC or BEL in the message must
    /// never reach the stream.
    #[test]
    fn no_escape_reaches_a_stream_through_either_emitter() {
        // `fetch.rs` sanitises the two error texts that come off the wire,
        // but the guarantee has to live where every caller passes through.
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

    // --- Colour, and the face in the STATUS column ------------------------

    use std::ffi::OsStr;

    use crate::style::StyleLevel;

    /// `NO_COLOR` removes colour at `full`, leaving sheep and boxes alone.
    /// Asserted on the rendered string, not the resolved [`Presentation`]:
    /// the struct could fold `NO_COLOR` in correctly and a bug in
    /// `rows::status_cell` could still emit an escape regardless.
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

    /// The face appears at `full`; at `plain` the plain word alone does
    /// (`plain` is "no sheep", not "no colour"); neither survives at
    /// `bare`.
    ///
    /// Run with `-- --nocapture` to read what each level looks like: an
    /// exact-string test proves the code matches a string, not that the
    /// result is legible.
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

    /// The STATUS word drops before any whole column does.
    /// `waiting-restart` (15 characters) is the longest status word,
    /// chosen so face-plus-word forces a column past a width face-alone
    /// fits. Exercises `Render::rows_for` and `table::render_boxed_ex`
    /// directly, the same two calls `table_of`'s own two-pass retry makes.
    ///
    /// Width 90, not this module's usual 80: `SMIT` and `CFG` cost the
    /// same extra columns `output/table.rs`'s own tests record.
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
    /// `rows`/`rows_for`.
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
