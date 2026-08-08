//! The `shep` binary: clap command surface, output rendering, ratatui
//! dashboard, static file server, startup-script generation, and the
//! container (`shep runtime`) and dev (`shep dev`) execution modes — all one
//! multi-call binary. Module-by-module design:
//! `docs/systematic-refactor/refactor-workspace/map.md`.

#![forbid(unsafe_code)]

mod cli;
#[cfg(unix)]
mod commands;
mod exit;
#[cfg(unix)]
mod launch;
mod output;

use std::path::PathBuf;

use clap::Parser;

use cli::{Cli, GlobalArgs};
#[cfg(unix)]
use cli::{Commands, DaemonArgs, Format};
#[cfg(unix)]
use commands::daemon::{daemon_exit_code, run_daemon};
#[cfg(unix)]
use commands::lifecycle;
#[cfg(unix)]
use commands::query;
use exit::ExitCode;
#[cfg(unix)]
use launch::launch_daemon;
use output::Streams;
#[cfg(unix)]
use shep_client::Client;
#[cfg(unix)]
use shep_client::spawn::{SpawnOutcome, connect_or_spawn};
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
/// Every command receives an already-connected client; no verb module
/// itself connects or autostarts. `Start` is the one exception at this
/// layer: [`connect_or_spawn_client`] autostarts a daemon if nothing
/// answers, and is the *only* autostart path in the binary. Every other
/// client-taking arm goes through [`connect_client`], which never spawns —
/// `shep stop` against a dead daemon must not launch a supervisor in order
/// to tell it to stop nothing. Every arm below still not wired to a real
/// verb module is a stand-in ([`not_wired`]) until the task that owns it
/// lands.
///
/// `resolve_paths` runs only for the arms that actually touch the socket.
/// `Completions` and `Daemon` never do — shell completion generation is
/// exactly what runs in the minimal environments (package build scripts,
/// container images, shell rc files) that have no `$HOME` at all, and the
/// re-exec'd `daemon` subcommand resolves its own paths independently, in
/// [`run_daemon_command`], rather than through this shared gate. Requiring
/// a resolvable home for either was a bug, not a deliberate restriction.
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
        Commands::Daemon(ref args) => {
            return run_daemon_command(&mut streams, fmt, &cli.global, args).await;
        }
        _ => {}
    }

    let paths = match resolve_paths(&cli.global) {
        Ok(paths) => paths,
        Err(code) => {
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                code.code_str(),
                "none of --home, $SHEP_HOME, or $HOME resolves a root directory",
            );
            return code;
        }
    };

    match cli.command {
        Commands::Start(ref args) => {
            match connect_or_spawn_client(&mut streams, fmt, &paths).await {
                Ok(client) => lifecycle::start(&client, &mut streams, fmt, args).await,
                Err(code) => code,
            }
        }
        Commands::Stop(ref args) | Commands::Thatlldo(ref args) => {
            match connect_client(&mut streams, fmt, &paths).await {
                Ok(client) => lifecycle::stop(&client, &mut streams, fmt, args).await,
                Err(code) => code,
            }
        }
        Commands::Restart(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => lifecycle::restart(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Delete(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => lifecycle::delete(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Flock => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::flock(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
        Commands::Describe(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::describe(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Fold(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::fold(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Bleats(_) => not_wired(&mut streams, fmt, "bleats"),
        Commands::Ping => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::ping(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
        Commands::Kill => not_wired(&mut streams, fmt, "kill"),
        Commands::Completions(_) | Commands::Daemon(_) => {
            unreachable!("handled above, before resolve_paths runs")
        }
    }
}

/// Connects to the daemon at `paths.socket`, autostarting one via
/// [`launch_daemon`] if nothing answers. The only autostart in the binary —
/// see [`run`]'s own doc.
///
/// Not unit-tested here: its coverage is `shep_client::spawn::connect_or_spawn`'s
/// own suite plus the real-binary end-to-end tier. What would need testing
/// (a real socket, a real spawned process) is exactly what those two tiers
/// already cover, and duplicating it as an in-process unit test would mean
/// either faking `connect_or_spawn` itself (testing nothing new) or spawning
/// a real child from this test binary (the hang/flake risk this project
/// avoids in unit tests).
#[cfg(unix)]
async fn connect_or_spawn_client(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
) -> Result<Client, ExitCode> {
    let launch_paths = paths.clone();
    match connect_or_spawn(&paths.socket, move || launch_daemon(&launch_paths)).await {
        Ok(SpawnOutcome::Connected(client) | SpawnOutcome::Spawned(client)) => Ok(client),
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = output::emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            Err(code)
        }
    }
}

/// Connects to the daemon at `paths.socket`. Never autostarts — see
/// [`run`]'s own doc for why that matters.
#[cfg(unix)]
async fn connect_client(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
) -> Result<Client, ExitCode> {
    match Client::connect(&paths.socket).await {
        Ok(client) => Ok(client),
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = output::emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            Err(code)
        }
    }
}

/// Resolves this invocation's own [`ShepPaths`] and runs the supervisor in
/// the foreground until a signal or `KillDaemon`.
///
/// Handled from `run`'s early dispatch block rather than through the
/// shared `resolve_paths` gate below it: that gate exists so `completions`
/// keeps working with no resolvable `$HOME` at all, and routing `daemon`
/// through it too would impose the same requirement for no reason — this
/// re-exec target resolves its own paths instead, independently of that
/// gate.
///
/// `#[cfg(unix)]`: the only caller is `run`'s unix arm — the hidden
/// `daemon` subcommand is the re-exec target `launch::launch_daemon`
/// spawns, and that launcher itself only exists on this tier.
#[cfg(unix)]
async fn run_daemon_command(
    streams: &mut Streams<'_>,
    fmt: Format,
    global: &GlobalArgs,
    args: &DaemonArgs,
) -> ExitCode {
    let paths = match resolve_paths(global) {
        Ok(paths) => paths,
        Err(code) => {
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                code.code_str(),
                "none of --home, $SHEP_HOME, or $HOME resolves a root directory",
            );
            return code;
        }
    };
    match run_daemon(paths, args).await {
        Ok(()) => ExitCode::Success,
        Err(err) => {
            let code = daemon_exit_code(&err);
            let message = format!("{err}");
            let _ = output::emit_error(&mut *streams.err, fmt, code.code_str(), &message);
            code
        }
    }
}

/// The Windows arm of `run`: parsing, help, and every unit test above still
/// work on this target; every verb below this line does not exist here yet
/// (spec §11's functional tier — named-pipe RPC is future work, tracked by
/// `ShepPaths::pipe_name`, not built here).
///
/// Routed through [`output::emit_error`], same as the unix arm's own
/// placeholders, rather than a bare `eprintln!` — so this refusal already
/// honours `--format json` rather than only ever printing prose.
///
/// `async fn` purely so the call site in `main` needs no `cfg` of its own —
/// it awaits nothing, and the resulting `clippy::unused_async` is
/// pedantic-only, so it does not trip the gate.
#[cfg(windows)]
async fn run(cli: Cli) -> ExitCode {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    // No `mut` on `streams` here (unlike the unix arm): this arm only ever
    // reborrows through the already-`&mut` `err` field, and never takes
    // `&mut streams` on the struct itself, so the binding needs no
    // mutability of its own.
    let streams = Streams {
        out: &mut out,
        err: &mut err,
    };
    let _ = output::emit_error(
        &mut *streams.err,
        cli.global.format,
        ExitCode::Failure.code_str(),
        "shep does not yet support Windows",
    );
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
    /// even though `Completions` never touches `ShepPaths` or the socket.
    /// This can't reproduce the original bug by unsetting `$HOME`
    /// (mutating the environment is `unsafe` in edition 2024, and this
    /// crate is `#![forbid(unsafe_code)]`), so instead it pins the
    /// structural fix directly: `completions` never reaches
    /// `resolve_paths` at all, so whatever `$HOME` happens to be in any
    /// environment can't matter to it.
    ///
    /// What this actually guards, precisely: if `resolve_paths` were moved
    /// back onto `completions`'s path, this fails the moment `$HOME` is
    /// unset — the exit code would become `Usage` instead of `Internal`.
    /// It does **not** catch that regression in an environment where
    /// `$HOME` is set (true of ordinary dev machines and most CI): there,
    /// the reinstated `resolve_paths` call would simply succeed and fall
    /// through to the same `not_wired("completions")` arm, so `run(cli)`
    /// still returns `Internal` either way and this assertion cannot tell
    /// the two code shapes apart. `resolve_paths` has no seam for a test to
    /// inject a controlled failure without touching the real process
    /// environment (it reads `std::env::var_os("HOME")` directly), so
    /// there is no honest way to close that gap short of unsafe env
    /// mutation or spawning the real binary as a subprocess with `$HOME`
    /// cleared — the latter is exactly the e2e tier described below, not
    /// this unit test.
    ///
    /// `daemon` used to share this test (both were routed through
    /// `not_wired`), but it no longer belongs here: it now genuinely
    /// resolves its own paths in [`run_daemon_command`] and, on success,
    /// runs the real supervisor to completion — calling `run(cli).await`
    /// on `["shep", "daemon"]` from a unit test would bind a real socket
    /// under whatever `$HOME` this process happens to have and then block
    /// forever waiting for a signal that never arrives. Exercising the
    /// daemon dispatch arm for real belongs in the e2e tier
    /// (`tests/cli_e2e.rs`), against an isolated `--home`, not here.
    #[tokio::test]
    async fn completions_never_resolves_paths() {
        use clap::Parser;
        let argv = ["shep", "completions", "bash"];
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} failed: {e}"));
        assert_eq!(run(cli).await, ExitCode::Internal);
    }
}
