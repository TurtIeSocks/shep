//! `shep schema`: prints the Flockfile JSON Schema.
//!
//! The schema itself, and the guard that keeps the committed copy honest,
//! live in shep-core beside the type they describe
//! (`shep_core::config::flockfile_schema_json`).

use shep_core::config::flockfile_schema_string;

use crate::exit::ExitCode;
use crate::output::{Streams, write_outcome};

/// Prints the schema. Always succeeds.
///
/// `--format json` is ignored: the output is already JSON, and an envelope
/// around it would produce a file no editor could read.
pub fn schema(streams: &mut Streams<'_>) -> ExitCode {
    write_outcome(streams.out.write_all(flockfile_schema_string().as_bytes()))
}
