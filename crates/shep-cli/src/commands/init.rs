//! `shep init`: writing a Flockfile to start from.
//!
//! The scaffold itself lives in [`shep_core::config::scaffold`], beside the
//! grammar it is a specimen of. This module owns only the operator-facing
//! half: which file to write, in which format, and when to refuse.

use std::io::Write;
use std::path::{Path, PathBuf};

use shep_core::config::{Depth, FlockFormat, Scaffold, discover};

use crate::{
    Streams, cli::InitArgs, commands::runtime::get_cwd, exit::ExitCode, output::emit_notice,
};

/// Writes a scaffolded Flockfile.
///
/// The extension chooses the language, in all three of the ways a path can
/// be arrived at: given explicitly, discovered under `--force`, or defaulted
/// to `Flockfile.toml`. That is the whole reason `--force` is safe now --
/// it rewrites the file that is there in the language that file already
/// speaks, rather than dropping TOML into a `.yaml` and leaving something
/// no parser will accept.
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

    // Truncate in place rather than staging and renaming. `shep style`'s
    // writer stages, because it edits a file it must not corrupt half way;
    // this one replaces the whole contents, and truncating keeps the inode
    // and follows a symlink to whatever the operator pointed it at, which is
    // what somebody who symlinked their Flockfile meant.
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
        // Writing a second Flockfile beside an existing one is legal and
        // almost never meant: discovery walks a fixed order, so the new file
        // can be one shep never reads. Said out loud rather than refused,
        // because an operator naming a path explicitly may well be migrating.
        if let Some(existing) = discover(cwd)
            && existing != path
        {
            let _ = emit_notice(
                &mut *streams.err,
                streams.fmt,
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
            // this cannot be `None` -- but a plain `expect` here would be a
            // panic on a path an operator can reach, so it degrades to TOML
            // rather than aborting.
            let format = FlockFormat::from_path(&existing).unwrap_or(FlockFormat::Toml);
            Ok((existing, format))
        }
        None => Ok((cwd.join("Flockfile.toml"), FlockFormat::Toml)),
    }
}

fn refuse(streams: &mut Streams<'_>, message: &str) -> ExitCode {
    streams.fail(ExitCode::Usage, message)
}
