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

// Re-exported for `commands/`, which names every one of these at its own
// crate-root import (`crate::output::{Streams, emit, FlockRows, ...}`) once
// Tasks 7-11 land. None of the four is named by this literal source file
// today — only by other modules and tests reaching through this re-export
// — so `unused_imports` (a name-resolution lint, unlike `dead_code`'s
// reachability one) sees no reference and flags it. `#[allow]` says so
// explicitly rather than inventing a call site nothing needs yet.
#[allow(unused_imports)]
pub use rows::{DeletedIds, FlockRows, KillRow, PingRow};
pub use table::{human_duration, render_table};

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
/// a manual one — print `Streams { .. }` and nothing else.
///
/// Not constructed from non-test code on Windows: `main.rs` only builds one
/// in its `#[cfg(unix)]` `run` arm — the Windows arm refuses before
/// reaching any dispatch that would need one, until spec §11's Windows
/// functional tier lands. `#[cfg_attr]` says so explicitly rather than
/// leaving an unexplained Windows-only warning.
#[cfg_attr(windows, allow(dead_code))]
pub struct Streams<'a> {
    /// Rendered command output — what `emit` writes to.
    ///
    /// Not read anywhere yet: `main.rs`'s only caller today is the
    /// placeholder dispatch (`not_wired`), which only ever writes a
    /// diagnostic to `err`. Tasks 7-11 pass `&mut streams.out` to `emit` for
    /// a real command's rendered output. `#[allow(dead_code)]` says so
    /// explicitly rather than inventing a call site nothing needs yet.
    #[allow(dead_code)]
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
/// Not called from non-test code on Windows: its only production call sites
/// (`not_wired`, `run`'s `resolve_paths` error branch) are in `main.rs`'s
/// `#[cfg(unix)]` arm, until spec §11's Windows functional tier lands.
/// `#[cfg_attr]` says so explicitly rather than leaving an unexplained
/// Windows-only warning — and that in turn is why [`ErrorEnvelope`] and
/// [`ErrorBody`], only ever built inside this function's `Format::Json` arm,
/// need no annotation of their own.
///
/// # Errors
/// The underlying write failed.
#[cfg_attr(windows, allow(dead_code))]
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
        Format::Table => writeln!(err, "error: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::ExitCode;
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
}
