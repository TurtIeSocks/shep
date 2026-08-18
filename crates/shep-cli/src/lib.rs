//! `shep-cli`: clap command surface, output rendering, and the daemon
//! launch/re-exec path behind the `shep` binary. Module-by-module design:
//! `docs/systematic-refactor/refactor-workspace/map.md`.
//!
//! The crate's whole public API is three entry points — [`main`],
//! [`main_runtime`], [`main_dev`] — each returning
//! [`std::process::ExitCode`] for the binary that calls it. Every other item
//! is private. Embedding shep in another program is `shep-client`'s job, not
//! this crate's: that crate is the published embedding API, re-exports
//! shep-core, and carries none of the process-ownership assumptions (a clap
//! tree that is `#[cfg(unix)]` in half its dispatch, an exit code it expects
//! to own) this one does.
//!
//! `serve`, `runtime`, and `dev` (spec `docs/specs/shep-v1.md` §9) shipped in
//! Phase 15: a hand-rolled static file server (no `axum`, no `tower-http`;
//! Rin's ruling — the ledger has the reasoning), a foreground no-daemon
//! container mode with a PID-1 init split for zombie reaping, and an isolated
//! foreground development flock. Three `[[bin]]` targets sit over this
//! library: `shep` itself, plus `shep-runtime` and `shep-dev`, the two
//! container-entrypoint aliases that prepend their verb before parsing (see
//! `main_runtime`/`main_dev`). The ratatui `lookout` dashboard ships complete
//! as of Phase 16: all four panes — the flock table and shell (Phase 12a),
//! plus the bleats feed, the sheep detail pane and the host-usage strip
//! (Phase 12b) — a name filter that narrows the table in place, lambs in the
//! detail pane, and the three action keys (`x` stop, `R` restart, `L`
//! reload) behind the `--allow-control` gate, each arming a confirm rather
//! than acting on the keypress that pressed it. Remaining workspace debt,
//! none of it here: `docs/specs/deferred.md`.

#![forbid(unsafe_code)]

mod cli;
#[cfg(unix)]
mod commands;
mod completions;
#[cfg(unix)]
mod dog;
mod exit;
mod http;
#[cfg(unix)]
mod launch;
#[cfg(unix)]
mod lookout;
mod output;
mod serve;
#[cfg(unix)]
mod whistle;

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

#[cfg(unix)]
use cli::{AdoptArgs, Commands, DaemonArgs, Format};
use cli::{Cli, GlobalArgs};
#[cfg(unix)]
use commands::admin;
#[cfg(unix)]
use commands::bleats;
#[cfg(unix)]
use commands::daemon::{daemon_exit_code, run_daemon};
#[cfg(unix)]
use commands::dev;
#[cfg(unix)]
use commands::dogs;
#[cfg(unix)]
use commands::import;
#[cfg(unix)]
use commands::kv;
#[cfg(unix)]
use commands::lifecycle;
#[cfg(unix)]
use commands::logs;
#[cfg(unix)]
use commands::muster;
#[cfg(unix)]
use commands::query;
#[cfg(unix)]
use commands::runtime;
#[cfg(unix)]
use commands::schema;
// Imports the function directly, not the module: `commands::serve` would
// collide in this scope with the crate-root `serve` module (the static-file
// implementation `commands::serve::serve` itself calls into).
#[cfg(unix)]
use commands::serve::serve as serve_command;
#[cfg(unix)]
use commands::signal;
#[cfg(unix)]
use commands::startup;
#[cfg(unix)]
use commands::trigger;
#[cfg(unix)]
use commands::whisper;
use exit::ExitCode;
#[cfg(unix)]
use launch::launch_daemon;
use output::Streams;
#[cfg(unix)]
use shep_client::Client;
#[cfg(unix)]
use shep_client::spawn::{SpawnOutcome, connect_or_spawn};
use shep_core::paths::ShepPaths;

/// The `shep` entry point. Parses this process's arguments and runs one verb.
///
/// Returns rather than exiting, so the caller's `main` owns the process exit —
/// which is also what lets the integration tier call this without taking the
/// test harness down with it.
#[must_use]
pub fn main() -> std::process::ExitCode {
    run_argv(std::env::args_os().collect())
}

/// The `shep-runtime` entry point: `shep runtime`, with the verb supplied.
#[must_use]
pub fn main_runtime() -> std::process::ExitCode {
    run_argv(alias_argv("runtime", std::env::args_os().collect()))
}

/// The `shep-dev` entry point: `shep dev`, with the verb supplied.
#[must_use]
pub fn main_dev() -> std::process::ExitCode {
    run_argv(alias_argv("dev", std::env::args_os().collect()))
}

/// Builds the argument vector an alias binary should be parsed as: `verb`
/// inserted after argv[0].
///
/// **Except for `daemon` and `dog`.** Both are hidden re-exec targets that the
/// supervisor spawns as `std::env::current_exe()` plus the verb —
/// `shep_daemon::dogs` for a built-in dog, `crate::launch::launch_command` for
/// the shepherd itself. Under an alias binary, `current_exe()` is
/// `shep-runtime`, so inserting a verb here would turn `shep-runtime dog
/// metrics` into `shep runtime dog metrics` and every dog in a container would
/// die at its first exec. Those two argument vectors are never typed by a
/// human; they are constructed in exactly three places in this workspace —
/// the two named above, plus `crate::commands::startup::unit`, which renders
/// `{exec} daemon --foreground` into a systemd unit and a launchd plist. That
/// third one never reaches here: `shep startup` is not a verb an alias binary
/// can spell.
fn alias_argv(verb: &str, mut argv: Vec<OsString>) -> Vec<OsString> {
    let passthrough = matches!(
        argv.get(1).and_then(|arg| arg.to_str()),
        Some("daemon" | "dog")
    );
    if !passthrough {
        argv.insert(1, OsString::from(verb));
    }
    argv
}

/// Parses `argv` and runs it on a fresh multi-threaded runtime.
///
/// The runtime is built here rather than by `#[tokio::main]` on each entry
/// point, so the three of them share one construction and the `argv` seam
/// above stays testable without one.
fn run_argv(argv: Vec<OsString>) -> std::process::ExitCode {
    // A hidden, env-gated hook for `tests/term_panic_order.rs` — not a
    // clap variant, so it carries no `--help` entry and no command-surface
    // footprint. See `lookout::term::probe_panic_for_test`'s doc for why
    // this exists and what it replaces.
    #[cfg(unix)]
    if std::env::var_os("SHEP_TERM_PANIC_PROBE").is_some() {
        lookout::term::probe_panic_for_test();
    }
    let cli = Cli::parse_from(argv);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("shep: could not start an async runtime: {err}");
            return std::process::ExitCode::from(ExitCode::Failure as u8);
        }
    };
    std::process::ExitCode::from(runtime.block_on(run(cli)) as u8)
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

/// Why [`ensure_home`] would not hand back a layout.
///
/// A type rather than a bare [`ExitCode`] because two of the three carry the
/// path they are about, and an operator cannot act on a refusal that does not
/// name it.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug)]
enum HomeRefusal {
    /// None of `--home`, `$SHEP_HOME` or `$HOME` resolved a root directory.
    Unresolved,
    /// `--home`/`$SHEP_HOME` named a directory that is not there. Never
    /// created: see [`ensure_home_at`] for why a named path is not a path
    /// shep may invent.
    Missing(PathBuf),
    /// The default home did not exist and could not be created.
    Io {
        /// The directory whose creation failed.
        path: PathBuf,
        /// What the filesystem said.
        source: std::io::Error,
    },
}

impl core::fmt::Display for HomeRefusal {
    /// The operator-facing message, remedy included.
    ///
    /// Multi-line for [`Self::Missing`] on purpose: the two ways out are the
    /// whole value of the message, and a reader who has just mistyped a path
    /// is exactly the reader who needs them spelled out.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unresolved => f.write_str(UNRESOLVED_HOME),
            Self::Missing(path) => write!(
                f,
                "no flock at {path}\n  \
                 did you mean to drop --home? the default is ~/.shep\n  \
                 to set up a flock there deliberately:  mkdir -p {path}",
                path = path.display(),
            ),
            Self::Io { path, source } => {
                write!(f, "could not create {}: {source}", path.display())
            }
        }
    }
}

impl core::error::Error for HomeRefusal {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Unresolved | Self::Missing(_) => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

#[cfg_attr(windows, allow(dead_code))]
impl HomeRefusal {
    /// The status the command ends with.
    ///
    /// `Internal` rather than `Usage` for the io case: the operator asked for
    /// something reasonable and shep failed to do it, which is not a usage
    /// error however it reads at the terminal.
    fn code(&self) -> ExitCode {
        match self {
            Self::Unresolved | Self::Missing(_) => ExitCode::Usage,
            Self::Io { .. } => ExitCode::Internal,
        }
    }
}

/// Resolves `$SHEP_HOME` and makes sure the directory is there, reporting
/// whether this call is what created it.
///
/// # Errors
///
/// Every variant of [`HomeRefusal`]; see [`ensure_home_at`], which this wraps
/// with the environment resolved.
#[cfg_attr(windows, allow(dead_code))]
fn ensure_home(global: &GlobalArgs) -> Result<(ShepPaths, bool), HomeRefusal> {
    let paths = resolve_paths(global).map_err(|_| HomeRefusal::Unresolved)?;
    ensure_home_at(paths, global.home.is_some())
}

/// [`ensure_home`] with the environment already resolved away.
///
/// The asymmetry between a default home and a named one is the whole point.
/// `~/.shep` is a name shep chose, so shep may conjure it; `/srv/api` is a
/// name the operator typed, and the likeliest reason it is not there is a
/// typo. Creating a typo'd path silently would leave a second, empty,
/// invisible flock behind, and the bug report that follows is "shep lost all
/// my processes" when the truth is "you are looking at a different flock".
///
/// Only the root is created here. `logs/`, `pids/` and `run/` remain
/// `shep_daemon::boot::init_dirs`' job, which runs on every boot and
/// re-tightens all of them. This exists for the commands that need the root
/// before any daemon has started, `startup` above all.
///
/// Split from [`ensure_home`] so the rule is testable without mutating
/// `$HOME`, which is process-global and shared by every test in this binary.
/// `ShepPaths::resolve` takes its environment as a closure for the same
/// reason; this follows that idiom one layer up. `explicit` is whether the
/// operator named this home themselves, by either `--home` or `$SHEP_HOME`.
///
/// # Errors
///
/// - [`HomeRefusal::Missing`] — `explicit`, and the directory is not there.
/// - [`HomeRefusal::Io`] — the directory could not be created.
#[cfg_attr(windows, allow(dead_code))]
fn ensure_home_at(paths: ShepPaths, explicit: bool) -> Result<(ShepPaths, bool), HomeRefusal> {
    if paths.home.is_dir() {
        return Ok((paths, false));
    }
    if explicit {
        return Err(HomeRefusal::Missing(paths.home));
    }

    // `.mode(DIR_MODE)` at creation rather than `create_dir_all` followed by
    // `set_permissions`, matching `launch.rs`'s log directory and
    // `boot::create_dir_at_dir_mode`: a create-then-chmod sequence leaves a
    // window in which the directory exists at whatever the ambient umask
    // allows, and on a shared machine that window is enough for another user
    // to open a handle that survives the later chmod. Do not "simplify" this.
    let built = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(shep_daemon::boot::DIR_MODE)
                .create(&paths.home)
        }
        #[cfg(not(unix))]
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .create(&paths.home)
        }
    };

    match built {
        Ok(()) => Ok((paths, true)),
        Err(source) => Err(HomeRefusal::Io {
            path: paths.home,
            source,
        }),
    }
}

/// Parses, resolves `$SHEP_HOME` for the verbs that need it, and dispatches
/// to the verb's own module.
///
/// Every command receives an already-connected client; no verb module
/// itself connects or autostarts. `Start` and `Muster` are the two
/// exceptions at this layer: both dispatch through
/// [`connect_or_spawn_client`], which autostarts a daemon if nothing
/// answers — for `Start` because starting a sheep against a dead daemon
/// means bringing one up first, and for `Muster` because that is the whole
/// point of the verb: assembling the flock from the saved roll has to work
/// against a freshly booted machine, where nothing is listening yet. Every
/// other client-taking arm goes through [`connect_client`], which never
/// spawns — `shep stop` against a dead daemon must not launch a supervisor
/// in order to tell it to stop nothing.
///
/// `resolve_paths` runs only for the arms that actually touch the socket.
/// `Completions` and `Daemon` never do — shell completion generation is
/// exactly what runs in the minimal environments (package build scripts,
/// container images, shell rc files) that have no `$HOME` at all, and the
/// re-exec'd `daemon` subcommand resolves its own paths independently, in
/// [`run_daemon_command`], rather than through this shared gate. Requiring
/// a resolvable home for either was a bug, not a deliberate restriction.
///
/// `Startup` and `Unstartup` are dispatched from the same early block, for
/// a different reason: they resolve their own `$SHEP_HOME` from the TARGET
/// user's passwd entry rather than from this process's environment, so the
/// shared gate would both impose a requirement they do not have and hand
/// them the wrong answer under `sudo`, which resets `$HOME` to root's.
///
/// `Dog` DOES go through this shared gate, unlike `Completions`/`Daemon`: a
/// dog resolves `$SHEP_HOME` exactly the way every ordinary verb does (the
/// daemon sets it as the one environment variable a dog's child inherits).
/// It is still dispatched immediately after, ahead of the locked-streams
/// block below, for the unrelated reason that block's own comment gives —
/// it runs until signalled, so it may not hold a stdout/stderr guard for a
/// process lifetime.
#[cfg(unix)]
async fn run(cli: Cli) -> ExitCode {
    let fmt = cli.global.format;

    // `StdoutLock`/`StderrLock` are process-wide and are held for as long as
    // the guard lives, so the locked pair further down is right for exactly
    // one shape of verb: one that finishes in milliseconds. For those, holding
    // both across the dispatch buys one lock acquisition instead of one per
    // write, and no interleaving with anything.
    //
    // Three verbs are not that shape. `daemon` runs until a signal, with
    // `commands::daemon::install_log_subscriber` rendering the daemon's own
    // records to `std::io::stderr()` from tokio worker threads; `bleats`
    // without `--no-follow` follows until Ctrl-C; `dog` runs until it is
    // signalled too, the same re-exec shape as `daemon`. None of the three
    // may hold a guard across that, so `bleats` takes unlocked handles and
    // `daemon`/`dog` take none at all — `dog::run_dog` writes its own
    // diagnostics straight to `eprintln!`, needing no `Streams` value in the
    // first place. (`completions` is dispatched early as well, for the
    // unrelated reason this function's own doc gives — it must work where no
    // `$HOME` resolves — and is a millisecond verb like the rest.)
    //
    // `Stderr`'s lock is re-entrant only for the thread that took it, so a
    // guard held here — on the runtime's main thread, for one of those
    // lifetimes — blocks the first record written by any other thread,
    // forever, taking the task that wrote it with it. That is not a
    // hypothetical: it wedged the daemon on its first warning, silently, with
    // an empty `shepd.err.log` (2026-08-09). `bleats` has not been wedged,
    // because nothing in that process writes to stderr off-thread today; the
    // shape is the same one, and the default panic hook — which writes through
    // this very handle — is enough to make it live. Unlocked handles take the
    // lock per write and release it.
    //
    // Only the `daemon` half of that fix is under test. `a_real_memory_breach_
    // restarts_a_sheep` reddens if this function's `daemon` arm takes a guard
    // again, because it reads the record that would be blocked. Nothing
    // covers the `bleats` arm below: every e2e `bleats` call passes
    // `--no-follow`, so no test has ever held that arm open long enough for a
    // guard to matter, and re-locking its handles would go unnoticed here.
    // Following mode is what a case would have to drive — a `bleats` that
    // stays up until it is signalled — and there is no such case today.
    match cli.command {
        Commands::Completions(ref args) => {
            let mut out = std::io::stdout().lock();
            return completions::completions(&mut out, args);
        }
        Commands::Daemon(ref args) => return run_daemon_command(fmt, &cli.global, args).await,
        // Routed through `ensure_home` rather than reading `--home` raw:
        // this is the verb that installs a unit without starting anything,
        // so it is the one that needs `$SHEP_HOME` to exist beforehand, and
        // for three phases it was the only command that refused instead of
        // creating it. `install` keeps its own check as well -- see
        // `a_shep_home_that_does_not_exist_is_refused` for the trap that
        // guards -- but this gate fires first and says more.
        Commands::Startup(ref args) => {
            let (paths, home_is_new) = match ensure_home(&cli.global) {
                Ok(resolved) => resolved,
                Err(refusal) => {
                    let code = refusal.code();
                    emit_error_locked(fmt, code, &refusal.to_string());
                    return code;
                }
            };
            let _ = home_is_new; // bound in the welcome task
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            return startup::startup(&mut streams, fmt, Some(paths.home.as_path()), args);
        }
        Commands::Unstartup(ref args) => {
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            return startup::unstartup(&mut streams, fmt, args);
        }
        // Needs no `$SHEP_HOME` at all — same reasoning as `Completions`
        // just above, and the same early spot for it.
        Commands::Schema => {
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            return schema::schema(&mut streams, fmt);
        }
        _ => {}
    }

    // Ahead of `resolve_paths` on purpose, not merely alongside `Serve`/
    // `Runtime` below it: `dev` computes its own `$SHEP_DEV_HOME`-rooted
    // paths (decision 15) and never reads `paths` from that shared gate, so
    // routing it through the gate first would make a `$HOME`-less
    // environment refuse `shep dev` for a reason the verb does not have —
    // `commands::dev::dev_home`'s own doc gives the isolation argument this
    // ordering protects. Unlocked handles for the same reason as `lookout`,
    // `serve` and `runtime`: this runs until the flock empties or a signal
    // ends it, in this same process.
    if let Commands::Dev(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        return dev::dev(
            &mut streams,
            fmt,
            cli.global.quiet,
            cli.global.home.is_some(),
            args,
        )
        .await;
    }

    let (paths, home_is_new) = match ensure_home(&cli.global) {
        Ok(resolved) => resolved,
        Err(refusal) => {
            let code = refusal.code();
            emit_error_locked(fmt, code, &refusal.to_string());
            return code;
        }
    };
    let _ = home_is_new; // bound in the welcome task

    // `dog` is a re-exec target like `daemon` — long-lived until signalled —
    // and, per `dog::run_dog`'s own doc, writes its own diagnostics straight
    // to stderr rather than through a `Streams`/`--format json` envelope, so
    // unlike every verb below it needs no `Streams` value at all, let alone
    // a locked one.
    if let Commands::Dog(ref args) = cli.command {
        return dog::run_dog(&args.name, paths).await;
    }

    // Split out of the dispatch below only to keep its handles unlocked, for
    // the reason the block comment above gives; it is otherwise an arm like
    // any other, and the `unreachable!` at the bottom of that dispatch is what
    // keeps the two from drifting apart.
    if let Commands::Bleats(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        return match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => bleats::bleats(&client, &mut streams, fmt, cli.global.quiet, args).await,
            Err(code) => code,
        };
    }

    // Not in the locked block below, for the reason that block's own comment
    // gives: this verb runs until the operator quits, and a `StdoutLock` held
    // across that lifetime wedges the first off-thread write. It also owns
    // stdout directly, through the terminal, which a guard would fight.
    if let Commands::Lookout(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        return lookout::lookout(&mut streams, fmt, &paths, args).await;
    }

    // Not in the locked block below, for the reason `lookout`'s own comment
    // above gives: `--foreground` runs until signalled, and a `StdoutLock`
    // held that long wedges the first off-thread write. The registering half
    // is quick, but it shares this one function with the foreground half —
    // `commands::serve::serve`'s own doc — so the two flags cannot validate
    // differently by running through two different dispatch spots.
    if let Commands::Serve(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        return serve_command(&mut streams, fmt, &paths, args).await;
    }

    // Not in the locked block below, for the same reason as `daemon`,
    // `bleats`, `lookout` and `serve` above: this runs until the flock
    // empties or a signal ends the supervisor, and a `StdoutLock` held that
    // long wedges the first off-thread write — here, the supervisor's own
    // logging, booted in this same process. `commands::foreground::run`'s
    // own doc carries the rest of the reasoning.
    if let Commands::Runtime(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        return runtime::runtime(&mut streams, fmt, cli.global.quiet, paths, args).await;
    }

    // Not in the locked block below, and it takes NO `Streams` at all. This
    // verb owns stdout as a wire: everything written there is MCP, and an
    // `output::emit` call on this path would corrupt the peer's parse. It also
    // runs until the peer closes the pipe, which is the same reason `bleats`
    // and `lookout` are up here — a `StdoutLock` held for a process lifetime
    // wedges the first off-thread write.
    if let Commands::Whistle = cli.command {
        let mut err = std::io::stderr();
        return whistle::whistle(&mut err, fmt, &paths).await;
    }

    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let mut streams = Streams {
        out: &mut out,
        err: &mut err,
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
        Commands::Reload(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => lifecycle::reload(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Delete(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => lifecycle::delete(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Stock(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => lifecycle::stock(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Trigger(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => trigger::trigger(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Signal(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => signal::signal(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Whisper(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => whisper::whisper(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Flock => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::flock(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
        Commands::Dogs => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::dogs(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
        // None of the four goes through `connect_client`/
        // `connect_or_spawn_client`: all four must still write the config
        // with no shepherd running at all (decision 11, `commands::dogs`'
        // own module doc), so each attempts its own connection internally
        // and tolerates a failure to reach one, rather than being refused
        // before it ever gets that far.
        //
        // `--exec` is the hidden pm2 spelling of `adopt` (`EnableArgs`'
        // own doc) — checked here, not inside `commands::dogs`, so that
        // module's `enable` keeps the simple `&str` signature every other
        // verb here has, and the argument-order inversion this alias
        // carries lives at the one seam that has to know about it.
        Commands::Enable(ref args) => match &args.exec {
            Some(path) => {
                dogs::adopt(
                    &mut streams,
                    fmt,
                    &paths,
                    &AdoptArgs {
                        name: args.name.clone(),
                        path: path.clone(),
                    },
                )
                .await
            }
            None => dogs::enable(&mut streams, fmt, &paths, &args.name).await,
        },
        Commands::Disable(ref args) => dogs::disable(&mut streams, fmt, &paths, &args.name).await,
        Commands::Adopt(ref args) => dogs::adopt(&mut streams, fmt, &paths, args).await,
        Commands::Rehome(ref args) => dogs::rehome(&mut streams, fmt, &paths, &args.name).await,
        Commands::Describe(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::describe(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Fold(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::fold(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        Commands::Ping => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => query::ping(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
        // `connect_client`, not `connect_or_spawn_client`: saving the roll
        // of a daemon that is not running is not a thing, and autostarting
        // one to save an empty flock would overwrite a good roll with an
        // empty one.
        Commands::Save => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => muster::save(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
        Commands::Muster => match connect_or_spawn_client(&mut streams, fmt, &paths).await {
            Ok(client) => muster::muster(&client, &mut streams, fmt).await,
            Err(code) => code,
        },
        Commands::Reopen(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => logs::reopen(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        // The one verb with two targets, and the only arm that can finish
        // without a client: `--daemon` empties files this binary created and
        // the daemon merely inherited (`launch::launch_command`), so there is
        // nothing to ask the socket. Not connecting is the feature rather
        // than an optimisation — a wedged or stopped shepherd is exactly when
        // an operator reaches for this, and `connect_client` never autostarts
        // one to be told to do nothing.
        Commands::Flush(ref args) if args.daemon => logs::flush_daemon(&mut streams, fmt, &paths),
        Commands::Flush(ref args) => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => logs::flush(&client, &mut streams, fmt, args).await,
            Err(code) => code,
        },
        // Reads a file and writes nothing; starts nothing, so — like
        // `--daemon`'s own arm just above — there is nothing to ask the
        // socket. The history is on disk precisely so it survives the
        // shepherd (`commands::dogs`' own module doc on this verb).
        Commands::Barks(ref args) => dogs::barks(&mut streams, fmt, &paths, args),
        // Reads and writes `kv.json` directly and never connects to the
        // shepherd — `commands::kv`'s own module doc gives the reasoning,
        // shared with `Barks` just above.
        Commands::Set(ref args) => kv::set(&mut streams, fmt, &paths, args),
        Commands::Get(ref args) => kv::get(&mut streams, fmt, &paths, args),
        Commands::Unset(ref args) => kv::unset(&mut streams, fmt, &paths, args),
        Commands::Kill => match connect_client(&mut streams, fmt, &paths).await {
            Ok(client) => admin::kill(client, &mut streams, fmt).await,
            Err(code) => code,
        },
        // Reads a file and writes a file; starts nothing, so there is
        // nothing to ask the socket. `logs::flush_daemon` is the other arm
        // that finishes without a client.
        Commands::Import(ref args) => import::import(&mut streams, fmt, args),
        Commands::Completions(_)
        | Commands::Daemon(_)
        | Commands::Startup(_)
        | Commands::Unstartup(_)
        | Commands::Schema
        | Commands::Bleats(_)
        | Commands::Lookout(_)
        | Commands::Whistle
        | Commands::Serve(_)
        | Commands::Runtime(_)
        | Commands::Dev(_)
        | Commands::Dog(_) => {
            unreachable!("handled above: before the shared $SHEP_HOME gate, or on unlocked handles")
        }
    }
}

/// What both [`resolve_paths`] call sites report when nothing resolves a root.
#[cfg(unix)]
const UNRESOLVED_HOME: &str = "none of --home, $SHEP_HOME, or $HOME resolves a root directory";

/// Emits one error envelope to stderr under a lock taken for just that write.
///
/// The lock is what keeps the envelope whole. Under `--format json`,
/// [`output::emit_error`] writes it with `serde_json::to_writer` — many small
/// writes on an unbuffered `Stderr` — and then a newline. A record from a
/// still-live worker thread landing between two of those writes tears the
/// envelope in half, and with `log_json = true` that also breaks line-oriented
/// parsing of `shepd.err.log`, where both end up.
///
/// Short-lived is the entire distinction from the guard [`run`]'s own comment
/// warns about: it is a guard held for a whole process lifetime that wedges
/// the daemon, never one held across a single write.
#[cfg(unix)]
fn emit_error_locked(fmt: Format, code: ExitCode, message: &str) {
    let mut err = std::io::stderr().lock();
    let _ = output::emit_error(&mut err, fmt, code.code_str(), message);
}

/// Connects to the daemon at `paths.socket`, autostarting one via
/// [`launch_daemon`] if nothing answers. `Start` and `Muster` are the two
/// arms that dispatch through this rather than [`connect_client`] — see
/// [`run`]'s own doc.
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

/// Renders a failed connect for an operator rather than for a library caller.
///
/// `shep-client`'s own `Display` is correct and deliberately plain: it names
/// the path and forwards the OS error, which is what an embedder needs. It is
/// the wrong sentence for the most common way a person meets this error, which
/// is running a read-only verb on a machine where no shepherd has ever run.
/// The socket file is simply absent, `connect(2)` returns `ENOENT`, and the
/// operator is told `No such file or directory (os error 2)` about a path they
/// did not choose and were not expecting to think about. It reads as a broken
/// install; it means an empty pasture.
///
/// So the absent-socket case gets its own sentence and the next command. Every
/// other failure keeps the library's wording, because `EACCES` and
/// `ECONNREFUSED` mean something specific and an operator hitting them needs
/// the detail: a refused connection is a socket that exists with nothing
/// listening, which is a stale file rather than a missing shepherd.
///
/// This lives here and not in `shep-client` on purpose. That crate is
/// published for embedders, and a library has no business telling its caller
/// to run a shell command.
#[cfg(unix)]
fn unreachable_message(err: &shep_client::ConnectError) -> String {
    match err {
        shep_client::ConnectError::Connect { path, source }
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            format!(
                "no shepherd is running (no socket at `{}`); \
                 start one with `shep start <target>`",
                path.display()
            )
        }
        other => other.to_string(),
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
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                code.code_str(),
                &unreachable_message(&err),
            );
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
/// Takes no [`Streams`] of its own, unlike every other arm: the supervisor
/// writes its own diagnostics through the subscriber
/// `commands::daemon::install_log_subscriber` installs, and the only two
/// writes left for this function are the error envelopes below — each under
/// its own short-lived lock ([`emit_error_locked`]), which is the opposite of
/// the guard held for a process lifetime that wedged the daemon.
///
/// `#[cfg(unix)]`: the only caller is `run`'s unix arm — the hidden
/// `daemon` subcommand is the re-exec target `launch::launch_daemon`
/// spawns, and that launcher itself only exists on this tier.
#[cfg(unix)]
async fn run_daemon_command(fmt: Format, global: &GlobalArgs, args: &DaemonArgs) -> ExitCode {
    let paths = match resolve_paths(global) {
        Ok(paths) => paths,
        Err(code) => {
            emit_error_locked(fmt, code, UNRESOLVED_HOME);
            return code;
        }
    };
    match run_daemon(paths, args).await {
        Ok(()) => ExitCode::Success,
        Err(err) => {
            let code = daemon_exit_code(&err);
            emit_error_locked(fmt, code, &err.to_string());
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

    /// A [`ShepPaths`] rooted at `root`, standing in for whatever
    /// `resolve_paths` would have produced, so the rule can be exercised
    /// without touching the process-global `$HOME`.
    #[cfg(unix)]
    fn paths_at(root: &std::path::Path) -> ShepPaths {
        let home = root.join(".shep").to_string_lossy().into_owned();
        let env = |key: &str| (key == "SHEP_HOME").then(|| home.clone());
        ShepPaths::resolve(&env, std::path::Path::new("/nonexistent"))
    }

    /// The transcript that started this: a fresh machine, the pm2 flow
    /// (`cargo install shep` then `shep startup`), and the very first
    /// command fails. `~/.shep` is a name shep chose, so shep may create it.
    #[cfg(unix)]
    #[test]
    fn a_missing_default_home_is_created_and_reported_as_new() {
        let root = tempfile::tempdir().unwrap();

        let (paths, created) =
            ensure_home_at(paths_at(root.path()), false).expect("a default home is created");
        assert_eq!(paths.home, root.path().join(".shep"));
        assert!(
            created,
            "the first call must report that it created the home"
        );
        assert!(
            paths.home.is_dir(),
            "the home must exist on disk afterwards"
        );

        let (_, created_again) =
            ensure_home_at(paths_at(root.path()), false).expect("second call succeeds");
        assert!(
            !created_again,
            "a home that was already there is not newly created"
        );
    }

    /// A path the operator typed is not a path shep may invent. The likeliest
    /// reason it is missing is a typo, and creating it would turn that typo
    /// into a second, empty, invisible flock -- after which the bug report is
    /// "shep lost all my processes".
    #[cfg(unix)]
    #[test]
    fn an_explicitly_named_missing_home_is_refused_and_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(&root.path().join("srv").join("typo"));
        let named = paths.home.clone();

        let refusal = ensure_home_at(paths, true).expect_err("a named missing home is refused");
        assert_eq!(refusal.code(), ExitCode::Usage);
        let message = refusal.to_string();
        assert!(
            message.contains(&named.display().to_string()),
            "the refusal must name the path it refused: {message}"
        );
        assert!(
            message.contains("~/.shep"),
            "the refusal must point at the default as the way out: {message}"
        );
        assert!(
            !named.exists(),
            "a refused path must be left on disk exactly as it was found"
        );
    }

    /// The mode is why this cannot be `create_dir_all`: a create-then-chmod
    /// sequence leaves the directory at the ambient umask for as long as the
    /// two syscalls are apart, and on a shared machine that is long enough
    /// for another user to open a handle that survives the chmod.
    #[cfg(unix)]
    #[test]
    fn a_created_home_is_owner_only_from_the_moment_it_exists() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let (paths, _) = ensure_home_at(paths_at(root.path()), false).unwrap();
        let mode = std::fs::metadata(&paths.home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "a fresh $SHEP_HOME must be owner-only");
    }

    /// fails if an alias binary stops supplying its own verb.
    #[test]
    fn an_alias_supplies_its_verb() {
        let argv = alias_argv(
            "runtime",
            vec!["shep-runtime".into(), "./Flockfile.toml".into()],
        );
        assert_eq!(
            argv,
            vec![
                OsString::from("shep-runtime"),
                OsString::from("runtime"),
                OsString::from("./Flockfile.toml"),
            ]
        );
    }

    /// fails if the alias with no arguments at all stops naming its verb —
    /// `shep-dev` on its own must be `shep dev`, not `shep`.
    #[test]
    fn an_alias_with_no_arguments_still_supplies_its_verb() {
        let argv = alias_argv("dev", vec!["shep-dev".into()]);
        assert_eq!(
            argv,
            vec![OsString::from("shep-dev"), OsString::from("dev")]
        );
    }

    /// fails if an alias binary rewrites the two hidden re-exec verbs.
    ///
    /// This is the container-killer: `shep_daemon::dogs` spawns a built-in dog
    /// as `current_exe() dog <name>`, and under `shep-runtime` that is this
    /// argument vector. A prepend here makes every dog exit with a clap usage
    /// error the moment `shep runtime` enables one.
    #[test]
    fn an_alias_passes_the_two_re_exec_verbs_through_untouched() {
        for verb in ["daemon", "dog"] {
            let argv = alias_argv(
                "runtime",
                vec!["shep-runtime".into(), verb.into(), "metrics".into()],
            );
            assert_eq!(
                argv[1],
                OsString::from(verb),
                "{verb} must not be rewritten"
            );
            assert_eq!(argv.len(), 3, "{verb}: nothing may be inserted");
        }
    }

    /// fails if the pass-through is written as a prefix or contains test
    /// rather than an exact match — a sheep legitimately named `dogfood`
    /// must still reach `runtime`, not be mistaken for the `dog` re-exec.
    #[test]
    fn the_pass_through_matches_the_whole_argument_and_not_a_prefix() {
        let argv = alias_argv("runtime", vec!["shep-runtime".into(), "dogfood".into()]);
        assert_eq!(argv[1], OsString::from("runtime"));
        assert_eq!(argv[2], OsString::from("dogfood"));
    }

    /// fails if the alias vector is well-formed and still does not reach the
    /// verb — a `runtime` subcommand that took a required positional, say.
    #[test]
    fn the_alias_vector_parses_to_the_expected_command() {
        use clap::Parser;
        // `Commands` is imported here and not via `super::*`: the top-level
        // `use cli::Commands` is `#[cfg(unix)]`-gated alongside every verb
        // module, and this test — like every other one in this file — must
        // still compile under the Windows cross-check. Matches
        // `save_parses_to_its_own_command`'s existing shape.
        use cli::Commands;
        let argv = alias_argv(
            "dog",
            vec!["shep-runtime".into(), "dog".into(), "metrics".into()],
        );
        let cli = Cli::try_parse_from(argv).expect("the passthrough vector must parse");
        assert!(matches!(cli.command, Commands::Dog(_)));
    }

    /// The Task 1 sibling for `runtime`: the alias vector [`main_runtime`]
    /// builds actually reaches `Commands::Runtime`, with `--supervise` set —
    /// the init's own re-exec, not a person's invocation.
    #[test]
    fn the_runtime_alias_vector_parses_to_the_runtime_command() {
        use clap::Parser;
        use cli::Commands;
        let argv = alias_argv("runtime", vec!["shep-runtime".into(), "--supervise".into()]);
        let cli = Cli::try_parse_from(argv).unwrap();
        let Commands::Runtime(args) = cli.command else {
            panic!("expected runtime")
        };
        assert!(args.supervise);
    }

    /// fails if `propagate_version` is dropped, which leaves the two alias
    /// binaries with no working `--version` at all: `shep-runtime --version`
    /// is parsed as `shep runtime --version`, and without propagation that is
    /// a clap usage error. `--version` is the one alias invocation a
    /// packager's smoke test actually runs.
    #[test]
    fn a_subcommand_answers_version() {
        use clap::Parser;
        let err = Cli::try_parse_from(["shep", "dogs", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    /// fails if `Commands::Save` is wired to another verb's function. The
    /// dispatch arms carried no unit coverage at all until recently, and a
    /// verb pointed at the wrong handler was invisible workspace-wide.
    ///
    /// `Commands` is imported locally rather than via `super::*`: the
    /// top-level `use cli::Commands` (main.rs:31) is `#[cfg(unix)]`-gated
    /// alongside every verb module, but this test — like every other one in
    /// this file — must still compile on the Windows target, where that
    /// import does not exist.
    #[test]
    fn save_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "save"]).unwrap().command,
            Commands::Save
        ));
    }

    /// fails if `Commands::Dogs` is wired to another verb's function. The
    /// dispatch arms carried no unit coverage at all until recently, and a
    /// verb pointed at the wrong handler was invisible workspace-wide.
    #[test]
    fn dogs_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "dogs"]).unwrap().command,
            Commands::Dogs
        ));
    }

    /// fails if `Commands::Enable`/`Commands::Disable` are wired to another
    /// verb's function, or if either loses its required `name` positional —
    /// the same gap `save_parses_to_its_own_command` closes for `save`, plus
    /// the `SelectorArgs`-family requiredness check
    /// `a_selector_taking_verb_refuses_to_run_without_one` (`cli.rs`) runs
    /// for every selector-taking verb, neither of which `DogArgs` shares.
    #[test]
    fn enable_and_disable_parse_to_their_own_commands_and_require_a_name() {
        use clap::Parser;
        use cli::Commands;

        let enabled = Cli::try_parse_from(["shep", "enable", "metrics"])
            .unwrap()
            .command;
        let Commands::Enable(args) = enabled else {
            panic!("expected enable")
        };
        assert_eq!(args.name, "metrics");

        let disabled = Cli::try_parse_from(["shep", "disable", "metrics"])
            .unwrap()
            .command;
        let Commands::Disable(args) = disabled else {
            panic!("expected disable")
        };
        assert_eq!(args.name, "metrics");

        assert!(
            Cli::try_parse_from(["shep", "enable"]).is_err(),
            "`shep enable` with no name must be a usage error"
        );
        assert!(
            Cli::try_parse_from(["shep", "disable"]).is_err(),
            "`shep disable` with no name must be a usage error"
        );
    }

    /// The `adopt`/`rehome` sibling of
    /// `enable_and_disable_parse_to_their_own_commands_and_require_a_name`.
    /// `adopt` needs both positionals; `rehome` shares `DogArgs` with
    /// `disable`, so it needs only the name.
    #[test]
    fn adopt_and_rehome_parse_to_their_own_commands_and_require_their_arguments() {
        use clap::Parser;
        use cli::Commands;

        let adopted = Cli::try_parse_from(["shep", "adopt", "otel", "/opt/bin/shep-otel"])
            .unwrap()
            .command;
        let Commands::Adopt(args) = adopted else {
            panic!("expected adopt")
        };
        assert_eq!(args.name, "otel");
        assert_eq!(args.path, PathBuf::from("/opt/bin/shep-otel"));

        let rehomed = Cli::try_parse_from(["shep", "rehome", "otel"])
            .unwrap()
            .command;
        let Commands::Rehome(args) = rehomed else {
            panic!("expected rehome")
        };
        assert_eq!(args.name, "otel");

        assert!(
            Cli::try_parse_from(["shep", "adopt"]).is_err(),
            "`shep adopt` with neither name nor path must be a usage error"
        );
        assert!(
            Cli::try_parse_from(["shep", "adopt", "otel"]).is_err(),
            "`shep adopt otel` with no path must be a usage error"
        );
        assert!(
            Cli::try_parse_from(["shep", "rehome"]).is_err(),
            "`shep rehome` with no name must be a usage error"
        );
    }

    /// fails if `enable --exec` routes to `enable` (which would try to run
    /// a built-in dog named after a path), and fails if the argument order
    /// is read as `adopt`'s. The two orders are inverted
    /// (`EnableArgs`'/`AdoptArgs`' own docs), and a swap here is silent:
    /// both arguments are strings, so nothing but a pinned assertion on
    /// which field holds which value would catch one crossing the other.
    #[test]
    fn the_hidden_pm2_spelling_reaches_adopt_with_the_arguments_the_right_way_round() {
        use clap::Parser;
        use cli::Commands;

        let parsed = Cli::try_parse_from(["shep", "enable", "--exec", "/opt/bin/d", "otel"])
            .unwrap()
            .command;
        let Commands::Enable(args) = parsed else {
            panic!("expected enable")
        };
        assert_eq!(args.name, "otel");
        assert_eq!(args.exec, Some(PathBuf::from("/opt/bin/d")));

        // A plain `enable` (no `--exec`) still carries no path — this is
        // the branch main's own dispatch reads to decide `enable` vs
        // `adopt`.
        let plain = Cli::try_parse_from(["shep", "enable", "metrics"])
            .unwrap()
            .command;
        let Commands::Enable(args) = plain else {
            panic!("expected enable")
        };
        assert_eq!(args.exec, None);
    }

    /// fails if `Commands::Muster` is wired to another verb's function —
    /// the same gap `save_parses_to_its_own_command` closes for `save`.
    #[test]
    fn muster_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "muster"]).unwrap().command,
            Commands::Muster
        ));
    }

    /// fails if `Commands::Import` is wired to another verb's function — the
    /// same gap `save_parses_to_its_own_command` closes for `save`. This
    /// pins clap's own parse only; it cannot see a dispatch arm in `run`
    /// that parses correctly and then calls the wrong function — that class
    /// of bug needs a real invocation of the compiled binary, which is what
    /// `cli_e2e.rs`'s own import case (asserting the envelope's `command`
    /// field, the way `saving_the_roll_then_mustering_reports_the_same_flock`
    /// already does for `save`/`muster`) is for.
    #[test]
    fn import_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "import"]).unwrap().command,
            Commands::Import(_)
        ));
    }

    /// The `barks` sibling of `import_parses_to_its_own_command` — same
    /// reasoning, same limit: this pins clap's own parse only, and
    /// `cli_e2e.rs`'s `barks_reads_the_history_with_no_shepherd_running` is
    /// what proves the dispatch arm above actually reaches
    /// `dogs::barks` rather than merely parsing to the right variant.
    #[test]
    fn barks_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "barks"]).unwrap().command,
            Commands::Barks(_)
        ));
    }

    /// fails if `Commands::Startup` or `Commands::Unstartup` is wired to
    /// another verb's function — the same gap
    /// `save_parses_to_its_own_command` closes for `save`. This pins clap's
    /// parse only; the dispatch arms themselves are covered by
    /// `startup_and_unstartup_reach_their_own_verbs` below.
    #[test]
    fn startup_and_unstartup_parse_to_their_own_commands() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "startup"]).unwrap().command,
            Commands::Startup(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["shep", "unstartup"]).unwrap().command,
            Commands::Unstartup(_)
        ));
        let named = Cli::try_parse_from(["shep", "startup", "--user", "deploy"])
            .unwrap()
            .command;
        let Commands::Startup(args) = named else {
            panic!("expected startup")
        };
        assert_eq!(args.user.as_deref(), Some("deploy"));
    }

    /// fails if either verb's dispatch arm calls the other's function, or is
    /// moved below the shared `$SHEP_HOME` gate.
    ///
    /// The two verbs are told apart by what they do with `--home`, which is
    /// the one observable difference that does not depend on the machine
    /// this runs on: `startup` refuses a `$SHEP_HOME` that is not there
    /// (`Usage`, the sudo-trap gate), and `unstartup` ignores `--home`
    /// entirely, because a removal is addressed by the unit's path and label
    /// alone. So a `Startup` arm calling `unstartup` stops returning
    /// `Usage`, and an `Unstartup` arm calling `startup` starts returning
    /// it. Routing either through the main dispatch below without adding it
    /// there hits that block's `unreachable!` instead.
    ///
    /// What it cannot catch, honestly: with `$HOME` set — true of ordinary
    /// dev machines and most CI — `resolve_paths` would succeed if these
    /// arms were moved behind it, and both verbs would then reach the same
    /// functions and return the same codes. That is
    /// `completions_never_resolves_paths`'s own caveat, for the same reason
    /// (mutating the environment is `unsafe` in edition 2024 and this crate
    /// is `#![forbid(unsafe_code)]`); the gate matters under `sudo`, where
    /// `$HOME` is root's rather than absent, and only the real binary can
    /// be run that way.
    ///
    /// Skipped as root: `unstartup` would reach a real `systemctl`/
    /// `launchctl` against whatever this machine actually has installed, and
    /// no test in this crate may.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn startup_and_unstartup_reach_their_own_verbs() {
        use clap::Parser;

        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: as root these verbs really install and remove a system unit");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");
        let missing = missing.to_str().unwrap();

        let cli = Cli::try_parse_from(["shep", "--home", missing, "startup"]).unwrap();
        assert_eq!(
            run(cli).await,
            ExitCode::Usage,
            "startup must refuse a $SHEP_HOME that is not there"
        );

        let cli = Cli::try_parse_from(["shep", "--home", missing, "unstartup"]).unwrap();
        assert_ne!(
            run(cli).await,
            ExitCode::Usage,
            "unstartup removes a unit and never reads the home a --home names"
        );
    }

    /// fails if `resurrect` stops reaching `muster`, or starts showing up in
    /// `--help`. It exists for a pm2 muscle-memory invocation, not to be
    /// taught.
    #[test]
    fn resurrect_is_a_hidden_alias_for_muster() {
        use clap::{CommandFactory, Parser};
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "resurrect"]).unwrap().command,
            Commands::Muster
        ));
        let cmd = Cli::command();
        let muster = cmd.find_subcommand("muster").unwrap();
        assert!(
            muster.get_visible_aliases().next().is_none(),
            "resurrect must stay out of --help"
        );
    }

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
    /// unset — the exit code would become `Usage` instead of `Success`.
    /// It does **not** catch that regression in an environment where
    /// `$HOME` is set (true of ordinary dev machines and most CI): there,
    /// the reinstated `resolve_paths` call would simply succeed and fall
    /// through to the same real `completions` call this test already
    /// exercises, so `run(cli)` still returns `Success` either way and this
    /// assertion cannot tell the two code shapes apart. `resolve_paths` has
    /// no seam for a test to inject a controlled failure without touching
    /// the real process environment (it reads `std::env::var_os("HOME")`
    /// directly), so there is no honest way to close that gap short of
    /// unsafe env mutation or spawning the real binary as a subprocess with
    /// `$HOME` cleared — the latter is exactly the e2e tier described
    /// below, not this unit test.
    ///
    /// `daemon` used to share this test (both were routed through the same
    /// placeholder dispatch arm), but it no longer belongs here: it now genuinely
    /// resolves its own paths in [`run_daemon_command`] and, on success,
    /// runs the real supervisor to completion — calling `run(cli).await`
    /// on `["shep", "daemon"]` from a unit test would bind a real socket
    /// under whatever `$HOME` this process happens to have and then block
    /// forever waiting for a signal that never arrives. Exercising the
    /// daemon dispatch arm for real belongs in the e2e tier
    /// (`tests/cli_e2e.rs`), against an isolated `--home`, not here.
    ///
    /// `#[cfg(unix)]`: what this proves is specific to the unix arm's
    /// early-dispatch shape — that `Completions` is one of the handful of
    /// verbs routed around `resolve_paths` rather than through it. The
    /// windows arm has no such split to prove anything about: it refuses
    /// every verb unconditionally, `completions` included, before it can
    /// even become a question of whether paths were resolved. Windows is
    /// shep's 0% tier by deliberate, standing decision (`CLAUDE.md`: "every
    /// verb prints \"not yet supported\" and exits") — carving out
    /// `completions` as the one working verb there is a real product
    /// decision, not one this test should make unilaterally by asserting
    /// `Success` against an arm that was never built to return it.
    #[cfg(unix)]
    #[tokio::test]
    async fn completions_never_resolves_paths() {
        use clap::Parser;
        let argv = ["shep", "completions", "bash"];
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} failed: {e}"));
        assert_eq!(run(cli).await, ExitCode::Success);
    }

    /// fails if the absent-socket case stops naming the next command, or if a
    /// different `connect(2)` failure starts being flattened into it.
    ///
    /// `ENOENT` is the shape a person meets on a machine where no shepherd has
    /// ever run, and the whole point of the special case is that it stops
    /// reading like a broken install. `EACCES` is a genuinely different
    /// problem: the socket is there and this user may not have it. Telling
    /// someone to `shep start` in that case would send them to the wrong fix.
    #[cfg(unix)]
    #[test]
    fn an_absent_socket_names_the_next_command_and_other_failures_do_not() {
        use std::io::{Error, ErrorKind};

        let absent = shep_client::ConnectError::Connect {
            path: std::path::PathBuf::from("/root/.shep/run/shep.sock"),
            source: Error::from(ErrorKind::NotFound),
        };
        assert_eq!(
            unreachable_message(&absent),
            "no shepherd is running (no socket at `/root/.shep/run/shep.sock`); \
             start one with `shep start <target>`"
        );

        let denied = shep_client::ConnectError::Connect {
            path: std::path::PathBuf::from("/root/.shep/run/shep.sock"),
            source: Error::from(ErrorKind::PermissionDenied),
        };
        let text = unreachable_message(&denied);
        assert!(
            text.starts_with("could not connect to `/root/.shep/run/shep.sock`:"),
            "a permission failure must keep the library's wording, got {text:?}"
        );
        assert!(
            !text.contains("shep start"),
            "a permission failure must not send the operator to `shep start`, got {text:?}"
        );
    }
}
