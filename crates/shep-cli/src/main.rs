//! The `shep` binary: clap command surface, output rendering, ratatui
//! dashboard, static file server, startup-script generation, and the
//! container (`shep runtime`) and dev (`shep dev`) execution modes — all one
//! multi-call binary. Module-by-module design:
//! `docs/systematic-refactor/refactor-workspace/map.md`.

#![forbid(unsafe_code)]

mod cli;
mod exit;

use std::path::PathBuf;

use clap::Parser;

#[cfg(unix)]
use cli::Commands;
use cli::{Cli, GlobalArgs};
use exit::ExitCode;
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
/// `#[cfg(unix)]`: the only dispatch table that calls it is `run`'s unix
/// arm — the Windows arm refuses before reaching any per-verb dispatch at
/// all.
#[cfg(unix)]
fn not_wired(verb: &str) -> ExitCode {
    eprintln!("shep: {verb} is not wired yet");
    ExitCode::Internal
}

/// Parses, resolves `$SHEP_HOME`, and dispatches to the verb's own module.
///
/// Every command receives an already-connected client; no verb here
/// connects or autostarts for itself. Every arm below is a stand-in
/// ([`not_wired`]) until the task that owns that verb replaces it with a
/// real connect (or, for `start` alone, `connect_or_spawn`) and a call into
/// its own command module.
#[cfg(unix)]
async fn run(cli: Cli) -> ExitCode {
    if let Err(code) = resolve_paths(&cli.global) {
        eprintln!("shep: set --home or $SHEP_HOME: $HOME is not set");
        return code;
    }

    match cli.command {
        Commands::Start(_) => not_wired("start"),
        Commands::Stop(_) | Commands::Thatlldo(_) => not_wired("stop"),
        Commands::Restart(_) => not_wired("restart"),
        Commands::Delete(_) => not_wired("delete"),
        Commands::Flock => not_wired("flock"),
        Commands::Describe(_) => not_wired("describe"),
        Commands::Fold(_) => not_wired("fold"),
        Commands::Bleats(_) => not_wired("bleats"),
        Commands::Ping => not_wired("ping"),
        Commands::Kill => not_wired("kill"),
        Commands::Completions(_) => not_wired("completions"),
        Commands::Daemon(_) => not_wired("daemon"),
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
    #[test]
    fn home_wins_over_the_ambient_environment() {
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
}
