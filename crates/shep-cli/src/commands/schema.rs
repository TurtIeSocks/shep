//! `shep schema`: prints the Flockfile JSON Schema.
//!
//! The schema itself, and the guard that keeps the committed copy honest,
//! live in shep-core beside the type they describe — see
//! `shep_core::config::flockfile_schema_json`. This module is the verb and
//! nothing else, so the string the operator gets and the string the drift
//! test compares are produced by one function.

use shep_core::config::flockfile_schema_json;

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{Streams, write_outcome};

/// Prints the schema. Always succeeds.
///
/// `--format json` is deliberately ignored: the output *is* JSON, and
/// wrapping a schema in the CLI's envelope would produce a file no editor
/// could read.
pub fn schema(streams: &mut Streams<'_>, _fmt: Format) -> ExitCode {
    write_outcome(streams.out.write_all(flockfile_schema_json().as_bytes()))
}
