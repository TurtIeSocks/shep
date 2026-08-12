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

mod rows;
mod table;

use std::io;

use serde::Serialize;

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
    DeletedIds, EmptiedFile, EmptiedFiles, FlockRows, FlushedRows, ImportRow, ImportRows, KillRow,
    PingRow, SavedRollRow, StartupStep, StartupSteps, TriggeredRows,
};
pub use table::{human_bytes, human_duration, render_table};

use crate::cli::Format;

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
}

impl std::fmt::Debug for Streams<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streams").finish_non_exhaustive()
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
}

/// Renders `data` to `out` in `fmt`.
///
/// Not called outside this module's own tests yet: `commands/` — the code
/// that will call it once a real payload exists to render — is Tasks 7-11.
/// `#[allow(dead_code)]` says so explicitly rather than inventing a call
/// site nothing needs yet.
///
/// # Errors
/// The underlying write failed.
#[allow(dead_code)]
pub fn emit<T: Render>(
    out: &mut dyn io::Write,
    fmt: Format,
    command: &str,
    data: T,
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
        Format::Table => write!(out, "{}", render_table(&data)),
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
/// `#[cfg_attr(windows, allow(dead_code))]` this struct also carries: its
/// one caller, `bleats.rs`, lives in `commands/`, which is
/// `#[cfg(unix)]`-gated in `main.rs`.
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

/// Renders a non-failure diagnostic to `err` in `fmt`. Same stream as
/// [`emit_error`] (stderr — a notice is not a sheep's line, and not a
/// command's own rendered output either) but a different envelope key, so a
/// `--format json` consumer can tell a diagnostic from a failure without
/// also cross-referencing the process exit code.
///
/// `code` is caller-defined, unlike `emit_error`'s `ExitCode::code_str()`:
/// a notice's code is never part of the exit-code taxonomy — that gap is
/// the whole reason this function exists rather than every notice call site
/// continuing to borrow [`emit_error`].
///
/// Not called outside this module's own tests on Windows: its one caller,
/// `bleats.rs`, lives in `commands/`, which is `#[cfg(unix)]`-gated in
/// `main.rs` — same reason [`Streams::out`] carries the same attribute.
///
/// # Errors
/// The underlying write failed.
#[cfg_attr(windows, allow(dead_code))]
pub fn emit_notice(
    err: &mut dyn io::Write,
    fmt: Format,
    code: &str,
    message: &str,
) -> io::Result<()> {
    match fmt {
        Format::Json => {
            let envelope = NoticeEnvelope {
                schema_version: SCHEMA_VERSION,
                notice: NoticeBody { code, message },
            };
            serde_json::to_writer(&mut *err, &envelope)?;
            writeln!(err)
        }
        Format::Table => writeln!(err, "notice[{code}]: {message}"),
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
    use super::*;
    use crate::output::rows::tests::sample_flock;

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
        emit(&mut json_out, Format::Json, "flock", sample_flock()).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
        assert_eq!(parsed["command"], "flock");
        assert_eq!(parsed["data"].as_array().unwrap().len(), 3);

        let mut table_out = Vec::new();
        emit(&mut table_out, Format::Table, "flock", sample_flock()).unwrap();
        let text = String::from_utf8(table_out).unwrap();
        assert!(text.contains("NAME"));
        assert!(
            !text.contains("schema_version"),
            "the envelope is a JSON-only concept"
        );
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
}
