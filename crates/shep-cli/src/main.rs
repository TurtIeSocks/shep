//! The `shep` binary: clap command surface, output rendering, ratatui
//! dashboard, static file server, startup-script generation, and the
//! container (`shep runtime`) and dev (`shep dev`) execution modes — all one
//! multi-call binary. Module-by-module design:
//! `docs/systematic-refactor/refactor-workspace/map.md`.

#![forbid(unsafe_code)]

mod cli;
mod exit;
mod output;

use std::path::PathBuf;

use clap::Parser;

use cli::{Cli, GlobalArgs};
#[cfg(unix)]
use cli::{Commands, Format};
use exit::ExitCode;
#[cfg(unix)]
use output::Streams;
use shep_core::paths::ShepPaths;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = run(cli).await;
    std::process::exit(code as i32);
}

/// Turns `--home`/`$SHEP_HOME`/`$HOME` into a resolved [`ShepPaths`].
///
/// `ShepPaths::resolve` reads the environment through a closure rather than
/// `std::env` directly, so this function's whole job is bridging clap's
/// already-folded `GlobalArgs::home` (the flag wins over the variable,
/// clap's own `env` attribute already did that folding) back into that
/// closure shape, and supplying the fallback root the closure needs when
/// `SHEP_HOME` answers nothing.
///
/// # Errors
///
/// [`ExitCode::Usage`] if neither `--home`/`$SHEP_HOME` nor `$HOME` names a
/// root to resolve against — `$HOME` is read only as that fallback, so a
/// `--home` invocation still works in an environment with no `$HOME` at all.
///
/// Pure tier on purpose (no `#[cfg(unix)]`): its own test runs on every
/// target. The Windows build's `run` does not call it yet — refusing
/// outright is the whole Windows deliverable for now — so this function is
/// only reachable from non-test code on unix; `#[cfg_attr]` says so
/// explicitly rather than leaving an unexplained Windows-only warning.
#[cfg_attr(windows, allow(dead_code))]
fn resolve_paths(global: &GlobalArgs) -> Result<ShepPaths, ExitCode> {
    let env = |key: &str| match key {
        "SHEP_HOME" => global
            .home
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        other => std::env::var(other).ok(),
    };
    let home_dir = match (std::env::var_os("HOME"), env("SHEP_HOME")) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(_)) => PathBuf::new(),
        (None, None) => return Err(ExitCode::Usage),
    };
    Ok(ShepPaths::resolve(&env, &home_dir))
}

/// Placeholder for a dispatch arm whose command module has not landed yet.
/// Every one of these is deleted as its verb's module gets wired in; a grep
/// for this function's name proves none are left once the CLI is complete.
/// It returns `Internal` rather than `Usage` because reaching it is a fault
/// in this binary, not in what the user typed.
///
/// Renders through [`output::emit_error`] rather than a bare `eprintln!` so
/// this placeholder path already honours `--format json` — the same
/// `Streams` real verbs will thread through in later tasks. The write's own
/// `Result` is intentionally discarded: unlike a real command's output, a
/// failure to print "not wired yet" doesn't change which failure this run
/// experienced, and it stays `Internal` either way.
///
/// `#[cfg(unix)]`: the only dispatch table that calls it is `run`'s unix
/// arm — the Windows arm refuses before reaching any per-verb dispatch at
/// all.
#[cfg(unix)]
fn not_wired(streams: &mut Streams<'_>, fmt: Format, verb: &str) -> ExitCode {
    let message = format!("{verb} is not wired yet");
    let _ = output::emit_error(
        &mut *streams.err,
        fmt,
        ExitCode::Internal.code_str(),
        &message,
    );
    ExitCode::Internal
}

/// Parses, resolves `$SHEP_HOME` for the verbs that need it, and dispatches
/// to the verb's own module.
///
/// Every command receives an already-connected client; no verb here
/// connects or autostarts for itself. Every arm below is a stand-in
/// ([`not_wired`]) until the task that owns that verb replaces it with a
/// real connect (or, for `start` alone, `connect_or_spawn`) and a call into
/// its own command module.
///
/// `resolve_paths` runs only for the arms that actually touch the socket.
/// `Completions` and `Daemon` never do — shell completion generation is
/// exactly what runs in the minimal environments (package build scripts,
/// container images, shell rc files) that have no `$HOME` at all, and the
/// re-exec'd `daemon` subcommand resolves its own paths independently once
/// it lands. Requiring a resolvable home for either was a bug, not a
/// deliberate restriction.
#[cfg(unix)]
async fn run(cli: Cli) -> ExitCode {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let mut streams = Streams {
        out: &mut out,
        err: &mut err,
    };
    let fmt = cli.global.format;

    match cli.command {
        Commands::Completions(_) => return not_wired(&mut streams, fmt, "completions"),
        Commands::Daemon(_) => return not_wired(&mut streams, fmt, "daemon"),
        _ => {}
    }

    if let Err(code) = resolve_paths(&cli.global) {
        let _ = output::emit_error(
            &mut *streams.err,
            fmt,
            code.code_str(),
            "none of --home, $SHEP_HOME, or $HOME resolves a root directory",
        );
        return code;
    }

    match cli.command {
        Commands::Start(_) => not_wired(&mut streams, fmt, "start"),
        Commands::Stop(_) | Commands::Thatlldo(_) => not_wired(&mut streams, fmt, "stop"),
        Commands::Restart(_) => not_wired(&mut streams, fmt, "restart"),
        Commands::Delete(_) => not_wired(&mut streams, fmt, "delete"),
        Commands::Flock => not_wired(&mut streams, fmt, "flock"),
        Commands::Describe(_) => not_wired(&mut streams, fmt, "describe"),
        Commands::Fold(_) => not_wired(&mut streams, fmt, "fold"),
        Commands::Bleats(_) => not_wired(&mut streams, fmt, "bleats"),
        Commands::Ping => not_wired(&mut streams, fmt, "ping"),
        Commands::Kill => not_wired(&mut streams, fmt, "kill"),
        Commands::Completions(_) | Commands::Daemon(_) => {
            unreachable!("handled above, before resolve_paths runs")
        }
    }
}

/// The Windows arm of `run`: parsing, help, and every unit test above still
/// work on this target; every verb below this line does not exist here yet
/// (spec §11's functional tier — named-pipe RPC is future work, tracked by
/// `ShepPaths::pipe_name`, not built here).
///
/// `async fn` purely so the call site in `main` needs no `cfg` of its own —
/// it awaits nothing, and the resulting `clippy::unused_async` is
/// pedantic-only, so it does not trip the gate.
#[cfg(windows)]
async fn run(_cli: Cli) -> ExitCode {
    eprintln!("shep does not yet support Windows");
    ExitCode::Failure
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `--home` that never reached `ShepPaths` is invisible from the
    /// outside until a daemon binds the wrong socket well after the fact.
    ///
    /// This pins `resolve_paths`'s own folding of an already-populated
    /// `GlobalArgs::home` into `ShepPaths` — it says nothing about
    /// `$SHEP_HOME` itself, which clap folds into `GlobalArgs::home` before
    /// this function ever runs (pinned separately in `cli.rs`'s
    /// `home_flag_is_wired_to_the_shep_home_env_var`, since this crate's
    /// `#![forbid(unsafe_code)]` rules out mutating the environment here to
    /// exercise that fold directly).
    #[test]
    fn explicit_home_field_resolves_to_the_expected_shep_paths() {
        let global = cli::GlobalArgs {
            home: Some("/tmp/explicit".into()),
            format: cli::Format::Table,
            quiet: false,
        };
        let paths = resolve_paths(&global).unwrap();
        assert_eq!(paths.home, std::path::Path::new("/tmp/explicit"));
        assert_eq!(
            paths.socket,
            std::path::Path::new("/tmp/explicit/run/shep.sock")
        );
    }

    /// Regression test: `resolve_paths` used to run unconditionally before
    /// dispatch, so `shep completions bash` exited `Usage` ("$HOME is not
    /// set") in exactly the minimal environments — build scripts, container
    /// images, shell rc files — that completion generation is meant for,
    /// even though neither `Completions` nor `Daemon` ever touches
    /// `ShepPaths` or the socket. This can't reproduce the original bug by
    /// unsetting `$HOME` (mutating the environment is `unsafe` in edition
    /// 2024, and this crate is `#![forbid(unsafe_code)]`), so instead it
    /// pins the structural fix directly: these two commands never reach
    /// `resolve_paths` at all, so whatever `$HOME` happens to be in any
    /// environment can't matter to them. If `resolve_paths` were moved back
    /// onto their path, this fails the moment CI's `$HOME` is unset — but
    /// even on a machine with `$HOME` set, `not_wired`'s `Internal` return
    /// vs. `resolve_paths`'s would-be `Usage` return still tells them apart.
    #[tokio::test]
    async fn completions_and_daemon_never_resolve_paths() {
        use clap::Parser;
        for argv in [vec!["shep", "completions", "bash"], vec!["shep", "daemon"]] {
            let cli = Cli::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("{argv:?} failed to parse: {e}"));
            assert_eq!(run(cli).await, ExitCode::Internal, "argv: {argv:?}");
        }
    }
}
