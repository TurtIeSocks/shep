//! `shep init`: writing a Flockfile to start from.
//!
//! The scaffold itself lives in [`shep_core::config::scaffold`], beside the
//! grammar it is a specimen of. This module owns only the operator-facing
//! half: which file to write, in which format, and when to refuse.

use std::io::Write;
use std::path::{Path, PathBuf};

use shep_core::config::{Depth, FlockFormat, Scaffold, discover};

use crate::{Streams, cli::InitArgs, commands::runtime::get_cwd, exit::ExitCode};

/// Writes a scaffolded Flockfile.
///
/// The extension chooses the language however the path was arrived at: given
/// explicitly, discovered under `--force`, or defaulted to `Flockfile.toml`.
pub async fn init(streams: &mut Streams<'_>, args: &InitArgs) -> ExitCode {
    let cwd = match get_cwd(streams) {
        Ok(cwd) => cwd,
        Err(code) => return code,
    };

    let (path, format) = match target(streams, &cwd, args) {
        Ok(target) => target,
        Err(code) => return code,
    };

    let text = match Scaffold::new(format, depth(args)).build() {
        Ok(text) => text,
        Err(err) => {
            return streams.fail(ExitCode::Usage, &err.to_string());
        }
    };

    // Truncate in place: keeps the inode, and follows a symlink to what the
    // operator pointed it at.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()));

    match written {
        Ok(()) => {
            streams.note("init", &format!("wrote {}", path.display()));
            ExitCode::Success
        }
        // Not `Usage`: a read-only directory or a full disk is not the
        // operator getting the command wrong.
        Err(err) => streams.fail(ExitCode::Failure, &format!("{}: {err}", path.display())),
    }
}

fn depth(args: &InitArgs) -> Depth {
    if args.all { Depth::All } else { Depth::Curated }
}

/// Which file to write, and in which language.
///
/// # Errors
/// [`ExitCode::Usage`] when a Flockfile is already there and `--force` was
/// not given, or when an explicit path carries an extension shep cannot
/// read.
fn target(
    streams: &mut Streams<'_>,
    cwd: &Path,
    args: &InitArgs,
) -> Result<(PathBuf, FlockFormat), ExitCode> {
    if let Some(given) = &args.path {
        let path = cwd.join(given);
        let Some(format) = FlockFormat::from_path(&path) else {
            return Err(refuse(
                streams,
                &format!(
                    "{} is not a Flockfile shep can read; use .toml, .yaml, \
                     .yml, .json or .json5",
                    path.display()
                ),
            ));
        };
        if path.exists() && !args.force {
            return Err(refuse(
                streams,
                &format!(
                    "{} already exists; pass --force to replace it",
                    path.display()
                ),
            ));
        }
        // Discovery walks a fixed order, so a second Flockfile beside an
        // existing one can be one shep never reads. Said, not refused.
        if let Some(existing) = discover(cwd)
            && existing != path
        {
            streams.aside(
                "init_shadowed",
                &format!(
                    "{} is already here and shep reads it first; {} will be ignored \
                     until you remove it",
                    existing.display(),
                    path.display()
                ),
            );
        }
        return Ok((path, format));
    }

    match discover(cwd) {
        Some(existing) => {
            if !args.force {
                return Err(refuse(
                    streams,
                    &format!(
                        "{} already exists; pass --force to replace it",
                        existing.display()
                    ),
                ));
            }
            // Every name `discover` returns carries a known extension, so
            // this cannot be `None`. Degrades rather than panicking.
            let format = FlockFormat::from_path(&existing).unwrap_or(FlockFormat::Toml);
            Ok((existing, format))
        }
        None => Ok((cwd.join("Flockfile.toml"), FlockFormat::Toml)),
    }
}

fn refuse(streams: &mut Streams<'_>, message: &str) -> ExitCode {
    streams.fail(ExitCode::Usage, message)
}
