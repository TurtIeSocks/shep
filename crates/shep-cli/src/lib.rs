//! `shep-cli`: clap command surface, output rendering, and the daemon
//! launch/re-exec path behind the `shep` binary.
//!
//! The public API is three entry points, [`main`], [`main_runtime`] and
//! [`main_dev`], one per `[[bin]]` target, each returning
//! [`std::process::ExitCode`] for the binary that calls it. Everything else
//! is private: embedding shep in another program is `shep-client`'s job.

#![forbid(unsafe_code)]

mod cli;
mod commands;
mod completions;
mod dog;
mod dog_index;
mod exit;
mod fetch;
mod flourish;
mod http;
mod launch;
mod lookout;
mod output;
mod serve;
mod shutdown;
mod status;
mod style;
mod terminal_safe;
mod vocabulary;
mod welcome;
mod whistle;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::path::PathBuf;

use clap::Parser;

use cli::{AdoptArgs, Commands, DaemonArgs, Format, StartArgs};
use cli::{Cli, GlobalArgs};
use commands::admin;
use commands::bleats;
use commands::daemon::{daemon_exit_code, run_daemon};
use commands::dev;
use commands::dogs;
use commands::import;
use commands::kv;
use commands::lifecycle;
use commands::lifecycle::Load;
use commands::logs;
use commands::muster;
use commands::query;
use commands::runtime;
use commands::schema;
// aliased: `commands::serve` would collide with the crate-root `serve` module
use commands::serve::serve as serve_command;
use commands::shep_toml::{ShepToml, ShepTomlError};
use commands::signal;
#[cfg(unix)]
use commands::startup;
use commands::trigger;
use commands::whisper;
use exit::ExitCode;
use launch::launch_daemon;
use output::Streams;
use shep_client::Client;
use shep_client::spawn::{SpawnOutcome, connect_or_spawn};
use shep_core::paths::ShepPaths;

use crate::commands::init;

/// The `shep` entry point. Parses this process's arguments and runs one verb.
///
/// Returns rather than exiting: the caller's `main` owns the process exit, so
/// the integration tier can call this without taking the harness down.
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
/// `daemon` and `dog` pass through untouched. The supervisor spawns those two
/// as `std::env::current_exe()` plus the verb, and under an alias binary
/// `current_exe()` is `shep-runtime`, so inserting a verb would turn
/// `shep-runtime dog metrics` into `shep runtime dog metrics`.
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
fn run_argv(argv: Vec<OsString>) -> std::process::ExitCode {
    // Env-gated hook for `tests/term_panic_order.rs`, not a clap variant, so
    // it carries no `--help` entry.
    if std::env::var_os("SHEP_TERM_PANIC_PROBE").is_some() {
        lookout::term::probe_panic_for_test();
    }
    // `try_parse_from`, not `parse_from`: the latter prints and exits inside
    // clap, so bare `shep` and `shep help` could not carry a status line.
    let parsed = Cli::try_parse_from(argv.clone());
    // Before clap renders its own "unrecognized subcommand" error, check
    // whether the token it could not place names an adopted dog.
    if let Err(ref err) = parsed
        && let Some(code) = dispatch_adopted_dog(&argv, err)
    {
        return code;
    }
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
    let cli = match parsed {
        Ok(cli) => cli,
        Err(err) => {
            #[cfg(unix)]
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::MissingSubcommand
            ) {
                runtime.block_on(print_shepherd_status(&argv));
            }
            // clap renders the help or the usage error and picks the exit
            // code, as `parse_from` would have.
            err.exit();
        }
    };
    // What is configured and whether the hard rule overrides it stay two
    // steps: `style_source` rides down to `lookout::lookout`, whose settings
    // screen reports which layer won.
    let (configured, style_source) = resolve_style(&cli.global);
    let level = if must_render_bare(std::io::stdout().is_terminal(), cli.global.format) {
        style::StyleLevel::Bare
    } else {
        configured
    };
    // Every terminal fact is read at this seam and nowhere else: `NO_COLOR`,
    // `$TERM`, `$COLORTERM` and the width.
    let style = style::Presentation::new(
        level,
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
        std::env::var_os("COLORTERM").as_deref(),
        output::terminal_width(),
    );
    std::process::ExitCode::from(
        runtime.block_on(run(cli, style, (configured, style_source))) as u8,
    )
}

/// Runs the token clap could not place as an adopted dog: `shep <dogname>
/// [args...]`, git and cargo's external-subcommand precedent.
///
/// Adopted dogs only, never a `$PATH` scan, which would let any stray binary
/// become a shep verb. Built-in verbs win structurally: this runs only once
/// clap has failed every subcommand and alias, and
/// `commands::dogs::collides_with_a_verb` refuses a colliding name.
///
/// `None` for every case that should reach clap's own unknown-verb error. The
/// `err.kind()` check is not redundant with the `InvalidSubcommand` context
/// match below it: clap attaches that context to `ArgumentConflict` too,
/// unreachable while [`cli::Cli`] sets no `args_conflicts_with_subcommands`.
fn dispatch_adopted_dog(argv: &[OsString], err: &clap::Error) -> Option<std::process::ExitCode> {
    if err.kind() != clap::error::ErrorKind::InvalidSubcommand {
        return None;
    }
    let name = match err.get(clap::error::ContextKind::InvalidSubcommand) {
        Some(clap::error::ContextValue::String(name)) => name.as_str(),
        _ => return None,
    };
    // clap's error carries the name but not where it sat. Everything after
    // that position is this dog's own argv.
    let index = argv.iter().position(|arg| arg.to_str() == Some(name))?;
    let global = GlobalArgs {
        home: home_before(&argv[1..index]),
        format: Format::Table,
        quiet: false,
        style: None,
    };
    let paths = resolve_paths(&global).ok()?;
    // `_readonly`, not `ShepToml::edit`: `edit` saves even when its closure
    // only reads, so a failed lookup would create `$SHEP_HOME` and write a
    // `shep.toml` on every mistyped verb.
    let path = ShepToml::adopted_dog_path_readonly(&paths.daemon_config, name)
        .ok()
        .flatten()?;
    let dog_argv = argv[index + 1..].to_vec();
    Some(run_adopted_dog(&path, &paths.home, name, &dog_argv))
}

/// Scans `prefix`, the argv tokens before the one clap could not place, for
/// `--home` in either `--home value` or `--home=value` form.
///
/// The only global flag [`resolve_paths`] reads. Falls back to `$SHEP_HOME`
/// by hand, since clap's own `env = "SHEP_HOME"` attribute never ran on an
/// argv it could not parse.
fn home_before(prefix: &[OsString]) -> Option<PathBuf> {
    let mut tokens = prefix.iter();
    while let Some(arg) = tokens.next() {
        if let Some(value) = arg.to_str().and_then(|s| s.strip_prefix("--home=")) {
            return Some(PathBuf::from(value));
        }
        if arg == "--home" {
            return tokens.next().map(PathBuf::from);
        }
    }
    std::env::var_os("SHEP_HOME").map(PathBuf::from)
}

/// Runs `path`, an adopted dog's binary: `extra_args` passed through as
/// typed, the two variables every dog is promised (`$SHEP_HOME` to find the
/// shepherd, `$SHEP_DOG_NAME` to name its own `[<name>]` section in
/// `dogs.toml`), stdio inherited.
///
/// `name` is the token the operator typed, so a dog run this way reads the
/// same `dogs.toml` section as the same dog run by the shepherd.
fn run_adopted_dog(
    path: &Path,
    home: &Path,
    name: &str,
    extra_args: &[OsString],
) -> std::process::ExitCode {
    let status = std::process::Command::new(path)
        .args(extra_args)
        .env("SHEP_HOME", home)
        .env("SHEP_DOG_NAME", name)
        .status();
    match status {
        Ok(status) => std::process::ExitCode::from(dog_exit_code(status)),
        Err(err) => {
            eprintln!("shep: could not run adopted dog {}: {err}", path.display());
            std::process::ExitCode::from(ExitCode::Failure as u8)
        }
    }
}

/// `status`'s own exit code, or `128 + signal` if it died by one, the shell
/// convention `commands::reap::classify` reads a reaped supervisor by.
#[cfg(unix)]
fn dog_exit_code(status: std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt as _;
    match status.code() {
        Some(code) => code as u8,
        None => (128 + status.signal().unwrap_or(0)) as u8,
    }
}

/// `status`'s own exit code.
///
/// No `128 + signal` arm: every Windows process exit carries a code, so the
/// `unwrap_or` is defensive.
#[cfg(windows)]
fn dog_exit_code(status: std::process::ExitStatus) -> u8 {
    status.code().unwrap_or(1) as u8
}

/// Prints the one-line shepherd status to stderr, for an invocation clap
/// answers by itself.
///
/// stderr rather than stdout: `shep completions zsh > _shep` writes shell
/// meant to be sourced, which would execute a status line as code.
///
/// Silent when stderr is not a terminal, and silent when `argv` names a
/// `--home`, since the parse that would say which home is the one that just
/// failed.
#[cfg_attr(windows, allow(dead_code))]
async fn print_shepherd_status(argv: &[OsString]) {
    if !std::io::stderr().is_terminal() || argv.iter().any(|a| a == "--home") {
        return;
    }
    let global = GlobalArgs {
        home: None,
        format: Format::Table,
        quiet: false,
        // Plain prose to stderr, not a rendered table: nothing on this path
        // reads `style`.
        style: None,
    };
    let Ok(paths) = resolve_paths(&global) else {
        return;
    };
    let status = status::ShepherdStatus::probe(&paths).await;
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{}", status::one_line(&status));
}

/// Turns `--home`/`$SHEP_HOME`/`$HOME` into a resolved [`ShepPaths`].
///
/// Bridges clap's already-folded `GlobalArgs::home` back into the closure
/// shape `ShepPaths::resolve` reads the environment through.
///
/// # Errors
///
/// [`ExitCode::Usage`] if neither `--home`/`$SHEP_HOME` nor `$HOME` names a
/// root to resolve against. `$HOME` is read only as that fallback, so a
/// `--home` invocation still works with no `$HOME` at all.
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

/// Parses `shep.toml`'s `[interpreters]` table into an extension to
/// interpreter map, for `lifecycle::start` to fold onto every resolved app
/// whose own `interpreter` is still unset. Precedence: `shep.toml`, then a
/// Flockfile's own field, then `--interpreter`.
///
/// Empty covers every way this layer can say nothing, a file that will not
/// parse included: `shep start` must still start a script by path while an
/// operator is mid-edit.
fn interpreters_from_config(shep_toml: Option<&str>) -> std::collections::BTreeMap<String, String> {
    shep_core::config::DaemonConfig::load(shep_toml, &|_| None)
        .map(|cfg| cfg.interpreters)
        .unwrap_or_default()
}

fn style_from_config(shep_toml: Option<&str>) -> Option<style::StyleLevel> {
    shep_core::config::DaemonConfig::load(shep_toml, &|_| None)
        .ok()?
        .style
        .level
        .and_then(|raw| style::StyleLevel::parse(&raw))
}

/// Resolves the level in force and which layer chose it: `--style`, then
/// `$SHEP_STYLE`, then `shep.toml`'s `[style] level`, then `full`.
///
/// Reads `shep.toml` via [`resolve_paths`] rather than [`ensure_home`], so
/// `--style` still works with no `$SHEP_HOME` resolvable and nothing here
/// creates a directory. An unreadable `shep.toml` reads as an empty config.
///
/// Unforced: the hard rule that `--format json` or a piped stdout means
/// [`style::StyleLevel::Bare`] is applied in [`run_argv`], so `shep style`'s
/// report says what is configured.
fn resolve_style(global: &GlobalArgs) -> (style::StyleLevel, style::StyleSource) {
    let config_text = resolve_paths(global)
        .ok()
        .and_then(|paths| std::fs::read_to_string(paths.daemon_config).ok());
    style::resolve(
        global.style,
        std::env::var("SHEP_STYLE").ok().as_deref(),
        style_from_config(config_text.as_deref()),
    )
}

/// Whether a level [`Commands::Style`]'s set form just wrote to `shep.toml`
/// is actually the level that will run.
///
/// Only `Flag` and `Env` can say no: they are the two layers
/// [`style::resolve`] puts above `shep.toml`, and so the two spellings an
/// operator needs named when the write keeps being overridden. `Config` is
/// the value this call just wrote, and `Default` cannot follow a write.
#[cfg_attr(windows, allow(dead_code))]
fn style_write_is_overridden(source: style::StyleSource) -> bool {
    matches!(source, style::StyleSource::Flag | style::StyleSource::Env)
}

/// What `shep startup`/`shep unstartup` say on Windows.
///
/// The rest of shep works here, so the message names the boundary: boot-time
/// supervision on Windows means a Service Control Manager service, a
/// different program shape from a unit template.
#[cfg(windows)]
const WINDOWS_NO_SERVICE: &str = "\
shep startup installs a boot-time service, and on Windows that means \
registering with the Service Control Manager -- not yet built (Tier B in \
docs/specs/windows-estimate.md).\n  \
the shepherd itself works here: run `shep start` in your own session, or wrap \
`shep runtime` in a service manager such as NSSM or WinSW.";

/// The hard rule: piped output and `--format json` render with no boxes, no
/// colour and no sheep, whatever `--style`/`$SHEP_STYLE`/`shep.toml` asked
/// for. `shep completions` writes shell a stray escape would execute as code.
///
/// Terminal-ness is a parameter: the real `is_terminal()` call happens once,
/// in [`run_argv`].
fn must_render_bare(stdout_is_terminal: bool, fmt: cli::Format) -> bool {
    !stdout_is_terminal || fmt == cli::Format::Json
}

/// Why [`ensure_home`] would not hand back a layout.
///
/// A type rather than a bare [`ExitCode`]: two of the three carry the path
/// they are about, and an operator cannot act on a refusal that omits it.
#[derive(Debug)]
enum HomeRefusal {
    /// None of `--home`, `$SHEP_HOME` or `$HOME` resolved a root directory.
    Unresolved,
    /// `--home`/`$SHEP_HOME` named a directory that is not there. Never
    /// created: a named path is not a path shep may invent.
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

impl HomeRefusal {
    /// The status the command ends with.
    ///
    /// `Internal` rather than `Usage` for the io case: the operator asked for
    /// something reasonable and shep failed at it.
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
/// Every variant of [`HomeRefusal`]; see [`ensure_home_at`].
fn ensure_home(global: &GlobalArgs) -> Result<(ShepPaths, bool), HomeRefusal> {
    let paths = resolve_paths(global).map_err(|_| HomeRefusal::Unresolved)?;
    ensure_home_at(paths, global.home.is_some())
}

/// [`ensure_home`] with the environment already resolved away. `explicit` is
/// whether the operator named this home, by `--home` or `$SHEP_HOME`.
///
/// A default home is created, a named one is not: `~/.shep` is a name shep
/// chose, while `/srv/api` was typed, and creating a typo leaves a second,
/// empty, invisible flock. Only the root; `logs/`, `pids/` and `run/` stay
/// `shep_daemon::boot::init_dirs`' job.
///
/// # Errors
///
/// - [`HomeRefusal::Missing`] if `explicit` and the directory is not there.
/// - [`HomeRefusal::Io`] if the directory could not be created.
fn ensure_home_at(paths: ShepPaths, explicit: bool) -> Result<(ShepPaths, bool), HomeRefusal> {
    if paths.home.is_dir() {
        return Ok((paths, false));
    }
    if explicit {
        return Err(HomeRefusal::Missing(paths.home));
    }

    // `.mode(DIR_MODE)` at creation, never create-then-chmod: that leaves the
    // directory at the ambient umask long enough for another user to open a
    // handle that survives the chmod.
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

/// Writes `shep.toml`'s starter `[interpreters]` mapping the moment
/// `$SHEP_HOME` is first created.
///
/// Not folded into [`welcome::on_first_run`]: the banner is suppressed under
/// `--format json` and a piped stderr, and this mapping is written regardless,
/// since it is what lets a provisioning script's `shep start server.js` work
/// without `--interpreter`.
///
/// Best-effort: a failure reports to stderr and continues.
fn scaffold_first_run_interpreters(paths: &ShepPaths) {
    if let Err(err) = ShepToml::edit(&paths.daemon_config, ShepToml::write_starter_interpreters) {
        let mut err_stream = std::io::stderr();
        let _ = writeln!(
            err_stream,
            "could not write a starter interpreter mapping to {}: {err}",
            paths.daemon_config.display()
        );
    }
}

/// Parses, resolves `$SHEP_HOME` for the verbs that need it, and dispatches
/// to the verb's own module.
///
/// Every command receives an already-connected client; no verb module
/// connects or autostarts. `Start` and `Muster` are the only two arms that
/// bring a shepherd up, through [`connect_or_spawn_client`].
///
/// `Startup` and `Unstartup` skip the shared `$SHEP_HOME` gate below;
/// `unstartup` ignores `--home` entirely, since a removal is addressed by
/// the unit's path and label alone. `style` is already forced to
/// [`style::StyleLevel::Bare`] if the hard rule applies; `resolved_style` is
/// the unforced pair `Commands::Style` and the lookout settings screen read.
async fn run(
    cli: Cli,
    style: style::Presentation,
    resolved_style: (style::StyleLevel, style::StyleSource),
) -> ExitCode {
    let fmt = cli.global.format;
    // Resolved once, here: the dispatch below partially moves `cli.command`,
    // so no arm can borrow it to ask this question for itself.
    let guard = VersionGuard::for_command(&cli.command);

    // `StdoutLock`/`StderrLock` are process-wide, so the locked pair further
    // down is right only for a verb that finishes in milliseconds. A guard
    // held on the main thread for a process lifetime blocks the first record
    // any other thread writes, forever, and wedges that task.
    match cli.command {
        Commands::Completions(ref args) => {
            // Status to stderr: the script on stdout is meant to be sourced,
            // and a status line in it would be executed.
            if let Ok(paths) = resolve_paths(&cli.global) {
                let shepherd = status::ShepherdStatus::probe(&paths).await;
                if std::io::stderr().is_terminal() {
                    let mut err = std::io::stderr();
                    let _ = writeln!(err, "{}", status::one_line(&shepherd));
                }
            }
            let mut out = std::io::stdout().lock();
            return completions::completions(&mut out, args);
        }
        Commands::Daemon(ref args) => {
            // `daemon reload` stops a shepherd and starts one rather than
            // being one. Unlocked handles: a teardown ladder plus a boot
            // takes seconds.
            if let Some(cli::DaemonCmd::Reload) = args.cmd {
                let paths = match resolve_paths(&cli.global) {
                    Ok(paths) => paths,
                    Err(code) => {
                        emit_error_locked(fmt, code, UNRESOLVED_HOME);
                        return code;
                    }
                };
                let mut out = std::io::stdout();
                let mut err = std::io::stderr();
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style,
                    fmt,
                };
                return commands::daemon::reload(&mut streams, &paths, guard).await;
            }
            return run_daemon_command(fmt, &cli.global, args).await;
        }
        // Through `ensure_home`, not raw `--home`: this verb installs a unit
        // without starting anything, so `$SHEP_HOME` must exist beforehand.
        Commands::Startup(ref args) => {
            #[cfg(windows)]
            let _ = args;
            let (paths, home_is_new) = match ensure_home(&cli.global) {
                Ok(resolved) => resolved,
                Err(refusal) => {
                    let code = refusal.code();
                    emit_error_locked(fmt, code, &refusal.to_string());
                    return code;
                }
            };
            if home_is_new {
                scaffold_first_run_interpreters(&paths);
                let mut err = std::io::stderr();
                let mut sink = std::io::sink();
                let mut streams = Streams {
                    out: &mut sink,
                    err: &mut err,
                    style,
                    fmt,
                };
                welcome::on_first_run(&mut streams, &paths.home, std::io::stderr().is_terminal());
            }
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style,
                fmt,
            };
            #[cfg(unix)]
            return startup::startup(&mut streams, Some(paths.home.as_path()), args);
            #[cfg(windows)]
            return streams.fail(ExitCode::Failure, WINDOWS_NO_SERVICE);
        }
        Commands::Unstartup(ref args) => {
            #[cfg(windows)]
            let _ = args;
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style,
                fmt,
            };
            #[cfg(unix)]
            return startup::unstartup(&mut streams, args);
            #[cfg(windows)]
            return streams.fail(ExitCode::Failure, WINDOWS_NO_SERVICE);
        }
        // Needs no `$SHEP_HOME` at all, like `Completions` above.
        Commands::Schema => {
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style,
                fmt,
            };
            return schema::schema(&mut streams);
        }
        _ => {}
    }

    // Ahead of `resolve_paths`: `dev` computes its own `$SHEP_DEV_HOME`-rooted
    // paths, so the gate would refuse `shep dev` in a `$HOME`-less environment
    // for a reason the verb does not have. Unlocked handles: this runs until
    // the flock empties or a signal ends it.
    if let Commands::Dev(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style,
            fmt,
        };
        return dev::dev(
            &mut streams,
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
    if home_is_new {
        // Unconditional, unlike the welcome banner below: `shep welcome`
        // creates a fresh home too, and the scaffold that makes `shep start
        // server.js` work is owed there as well.
        scaffold_first_run_interpreters(&paths);
    }
    // `Welcome` is excluded: its own arm prints the same text to stdout a
    // moment later, and it should print once.
    if home_is_new && !matches!(cli.command, Commands::Welcome) {
        let mut err = std::io::stderr();
        let mut sink = std::io::sink();
        let mut streams = Streams {
            out: &mut sink,
            err: &mut err,
            style,
            fmt,
        };
        welcome::on_first_run(&mut streams, &paths.home, std::io::stderr().is_terminal());
    }

    // `dog` is a re-exec target like `daemon`, long-lived until signalled,
    // and writes straight to stderr rather than through a `Streams` envelope.
    if let Commands::Dog(ref args) = cli.command {
        return dog::run_dog(&args.name, paths).await;
    }

    // Split out of the dispatch below only to keep its handles unlocked; the
    // `unreachable!` at the bottom of that dispatch keeps the two in step.
    if let Commands::Bleats(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style,
            fmt,
        };
        return match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => bleats::bleats(&client, &mut streams, cli.global.quiet, args).await,
            Err(code) => code,
        };
    }

    // Not in the locked block below: this runs until the operator quits, and
    // owns stdout directly through the terminal.
    if let Commands::Lookout(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style,
            fmt,
        };
        return lookout::lookout(&mut streams, &paths, args, resolved_style).await;
    }

    // Not in the locked block below: `--foreground` runs until signalled, and
    // the quick registering half shares this function so the two flags cannot
    // validate differently.
    if let Commands::Serve(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style,
            fmt,
        };
        return serve_command(&mut streams, &paths, args).await;
    }

    // Not in the locked block below: this runs until the flock empties, and
    // the supervisor's logging boots in this same process, so an off-thread
    // write would be the first thing a guard wedged.
    if let Commands::Runtime(ref args) = cli.command {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style,
            fmt,
        };
        return runtime::runtime(&mut streams, cli.global.quiet, paths, args).await;
    }

    // No `Streams` at all: this verb owns stdout as a wire, everything written
    // there is MCP, and an `output::emit` would corrupt the peer's parse.
    if let Commands::Whistle = cli.command {
        let mut err = std::io::stderr();
        return whistle::whistle(&mut err, fmt, &paths).await;
    }

    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let mut streams = Streams {
        out: &mut out,
        err: &mut err,
        style,
        fmt,
    };

    match cli.command {
        // No client: the welcome is local text about a local directory, and
        // asking a shepherd would fail on the fresh machine it greets.
        Commands::Welcome => {
            let shepherd = status::ShepherdStatus::probe(&paths).await;
            let code = welcome::welcome(&mut streams, &paths.home);
            // After the text: the status is the one line that changes between
            // runs.
            if fmt == Format::Table && std::io::stderr().is_terminal() {
                let _ = writeln!(streams.err, "{}", status::one_line(&shepherd));
            }
            code
        }
        Commands::Style(args) => match args.level {
            // Re-resolves rather than reading the `style` parameter, which
            // the hard rule may have forced to `Bare`: this report's job is
            // saying what is configured.
            None => {
                let (level, source) = resolve_style(&cli.global);
                let message = format!("{level} (from {source})");
                streams.note("style", &message);
                ExitCode::Success
            }
            // A level turns this from a report into a write. Config first,
            // report second, so a report cannot claim a write that failed.
            Some(level) => {
                // `try_edit`, not `edit`: `set_style_level` can refuse, and
                // `edit` saves after its closure whatever it returned, which
                // would rewrite a file this call reports as untouched.
                // `result_large_err` as in `commands::shep_toml`.
                #[cfg_attr(windows, allow(clippy::result_large_err))]
                if let Err(err) =
                    ShepToml::try_edit(&paths.daemon_config, |cfg| cfg.set_style_level(level))
                {
                    let code = match err {
                        ShepTomlError::Io { .. } => ExitCode::Failure,
                        ShepTomlError::Parse { .. } | ShepTomlError::WrongShape { .. } => {
                            ExitCode::InvalidConfig
                        }
                    };
                    return streams.fail(code, &err.to_string());
                }
                // Re-resolves, so the operator is told whether the value just
                // written will run or whether `--style`/`$SHEP_STYLE` still
                // outranks the `shep.toml` this call edited.
                let (effective, source) = resolve_style(&cli.global);
                let path = paths.daemon_config.display();
                let message = if style_write_is_overridden(source) {
                    format!(
                        "wrote {level} to {path}, but {source} still governs; \
                         shep runs at {effective}"
                    )
                } else {
                    format!("wrote {level} to {path}")
                };
                streams.note("style", &message);
                ExitCode::Success
            }
        },
        Commands::Start(ref args) => {
            load_command(&mut streams, &paths, guard, args, Load::Start).await
        }
        Commands::Add(ref args) => load_command(&mut streams, &paths, guard, args, Load::Add).await,
        Commands::Stop(ref args) | Commands::Thatlldo(ref args) => {
            match connect_client(&mut streams, &paths, guard).await {
                Ok(client) => lifecycle::stop(&client, &mut streams, args).await,
                Err(code) => code,
            }
        }
        Commands::Restart(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => lifecycle::restart(&client, &mut streams, &paths, args).await,
            Err(code) => code,
        },
        Commands::Reload(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => lifecycle::reload(&client, &mut streams, args).await,
            Err(code) => code,
        },
        Commands::Delete(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => lifecycle::delete(&client, &mut streams, args).await,
            Err(code) => code,
        },
        Commands::Stock(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => lifecycle::stock(&client, &mut streams, args).await,
            Err(code) => code,
        },
        Commands::Trigger(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => trigger::trigger(&client, &mut streams, args).await,
            Err(code) => code,
        },
        Commands::Signal(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => signal::signal(&client, &mut streams, args).await,
            Err(code) => code,
        },
        Commands::Whisper(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => whisper::whisper(&client, &mut streams, args).await,
            Err(code) => code,
        },
        // Falls back to the muster roll rather than refusing: looking at the
        // flock must not be a dead end on a machine that just rebooted.
        Commands::Flock => flock_command(&mut streams, &paths, guard).await,
        // The guard arm is what makes `--available` work with no shepherd
        // running: it never reaches `connect_client`.
        Commands::Dogs(ref args) if args.available => {
            query::available_dogs(&mut streams, args).await
        }
        Commands::Dogs(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => query::dogs(&client, &mut streams, args).await,
            Err(code) => code,
        },
        // None of the four goes through the connect helpers: all four must
        // write the config with no shepherd running, so each connects
        // internally. `--exec` is the hidden pm2 spelling of `adopt`, mapped
        // here so `commands::dogs::enable` keeps its `&str` signature.
        Commands::Enable(ref args) => match &args.exec {
            Some(path) => {
                dogs::adopt(
                    &mut streams,
                    &paths,
                    &AdoptArgs {
                        path: path.clone(),
                        name: Some(args.name.clone()),
                    },
                )
                .await
            }
            None => dogs::enable(&mut streams, &paths, &args.name).await,
        },
        Commands::Disable(ref args) => dogs::disable(&mut streams, &paths, &args.name).await,
        Commands::Adopt(ref args) => dogs::adopt(&mut streams, &paths, args).await,
        Commands::Rehome(ref args) => dogs::rehome(&mut streams, &paths, &args.name).await,
        Commands::Describe(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => query::describe(&client, &mut streams, args).await,
            Err(code) => code,
        },
        Commands::Fold(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => query::fold(&client, &mut streams, args).await,
            Err(code) => code,
        },
        // Not `connect_client`: a verb reporting whether a shepherd answers
        // must not fail because the answer is "no". The exit code is still
        // `DaemonUnreachable`, so `shep ping && echo up` works.
        Commands::Ping => {
            let status = status::ShepherdStatus::probe(&paths).await;
            status::render_ping(&mut streams, &status)
        }
        // `connect_client`, not `connect_or_spawn_client`: autostarting a
        // daemon to save its empty flock would overwrite a good roll.
        Commands::Save => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => muster::save(&client, &mut streams).await,
            Err(code) => code,
        },
        Commands::Muster => match connect_or_spawn_client(&mut streams, &paths, guard).await {
            Ok(client) => muster::muster(&client, &mut streams).await,
            Err(code) => code,
        },
        Commands::Reopen(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => logs::reopen(&client, &mut streams, args).await,
            Err(code) => code,
        },
        // `--daemon` empties files this binary created and the daemon merely
        // inherited, so there is nothing to ask the socket. Not connecting is
        // the feature: a wedged shepherd is when this gets reached for.
        Commands::Flush(ref args) if args.daemon => logs::flush_daemon(&mut streams, &paths),
        Commands::Flush(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => logs::flush(&client, &mut streams, args).await,
            Err(code) => code,
        },
        // Reads a file and starts nothing, so there is nothing to ask the
        // socket. The history is on disk so it survives the shepherd.
        Commands::Barks(ref args) => dogs::barks(&mut streams, &paths, args),
        // Reads and writes `kv.json` directly and never connects to the
        // shepherd.
        Commands::Set(ref args) => kv::set(&mut streams, &paths, args),
        Commands::Get(ref args) => kv::get(&mut streams, &paths, args),
        Commands::Unset(ref args) => kv::unset(&mut streams, &paths, args),
        // Does its own connecting: `connect_client` reports and gives up,
        // which would leave an operator with a live daemon nothing can stop.
        Commands::Kill => admin::kill(&paths, &mut streams).await,
        Commands::Init(ref args) => init::init(&mut streams, args).await,
        // Reads a file and writes a file; starts nothing, so there is
        // nothing to ask the socket.
        Commands::Import(ref args) => import::import(&mut streams, args),
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
const UNRESOLVED_HOME: &str = "none of --home, $SHEP_HOME, or $HOME resolves a root directory";

/// Emits one error envelope to stderr under a lock taken for just that write.
///
/// The lock keeps the envelope whole: under `--format json`
/// [`output::emit_error`] writes it as many small writes on an unbuffered
/// `Stderr`, and a record from a worker thread landing between two of them
/// tears the envelope in half.
fn emit_error_locked(fmt: Format, code: ExitCode, message: &str) {
    let mut err = std::io::stderr().lock();
    let _ = output::emit_error(&mut err, fmt, code.code_str(), message);
}

/// Connects to the daemon at `paths.socket`, autostarting one via
/// [`launch_daemon`] if nothing answers. `Start` and `Muster` are the two
/// arms that dispatch through this rather than [`connect_client`].
///
/// `pub(crate)`: `commands::daemon`'s reload starts the successor it just
/// stopped through this same autostart.
pub(crate) async fn connect_or_spawn_client(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: VersionGuard,
) -> Result<Client, ExitCode> {
    let launch_paths = paths.clone();
    match connect_or_spawn(&paths.socket, move || launch_daemon(&launch_paths)).await {
        // One arm for both outcomes: a daemon just spawned is this same binary
        // and cannot skew, and one already up is what the guard is for.
        Ok(SpawnOutcome::Connected(client) | SpawnOutcome::Spawned(client)) => {
            refuse_version_skew(streams, &client, guard)?;
            Ok(client)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            Err(streams.fail(code, &err.to_string()))
        }
    }
}

/// `shep start` and `shep add`, which share everything but one arm.
///
/// Bare, either verb means the Flockfile in this directory. The two disagree
/// only about what an empty directory means.
async fn load_command(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: VersionGuard,
    args: &StartArgs,
    mode: Load,
) -> ExitCode {
    let discovered = if args.targets.is_empty() {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| shep_core::config::flockfile::discover(&cwd))
    } else {
        None
    };
    if args.targets.is_empty() && discovered.is_none() {
        // Bare `shep start` in an empty directory means "bring a shepherd
        // up". `shep add` cannot mean that: it would register nothing.
        return match mode {
            Load::Start => start_bare_shepherd(streams, paths, guard).await,
            Load::Add => streams.fail(
                ExitCode::Usage,
                "no target and no Flockfile in this directory",
            ),
        };
    }
    let shep_toml_text = std::fs::read_to_string(&paths.daemon_config).ok();
    let interpreters = interpreters_from_config(shep_toml_text.as_deref());
    let client = match connect_or_spawn_client(streams, paths, guard).await {
        Ok(client) => client,
        Err(code) => return code,
    };
    match mode {
        Load::Start => {
            lifecycle::start(&client, streams, args, discovered.as_deref(), &interpreters).await
        }
        Load::Add => {
            lifecycle::add(&client, streams, args, discovered.as_deref(), &interpreters).await
        }
    }
}

/// `shep start` with no target and no Flockfile in sight: bring a shepherd up
/// and stop there.
///
/// The only way to get a shepherd without also starting a process: every
/// other route needs a target or a saved roll. Reports rather than re-boots
/// when one is already up.
async fn start_bare_shepherd(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: VersionGuard,
) -> ExitCode {
    let before = status::ShepherdStatus::probe(paths).await;
    if let Some(online) = &before.online {
        let message = format!(
            "shepherd already up (pid {}). `shep start <target>` adds a sheep.",
            online.pid
        );
        streams.aside("start", &message);
        return ExitCode::Success;
    }
    match connect_or_spawn_client(streams, paths, guard).await {
        Ok(client) => {
            // Asked after the boot: bringing the shepherd up restores the
            // muster roll, so a flock that looked empty may have members now.
            let restored = client
                .request(shep_core::protocol::Request::ListFlock)
                .await;
            let known = match &restored {
                Ok(shep_core::protocol::Response::Flock(procs)) => procs.len(),
                _ => 0,
            };
            let message = if known == 0 {
                format!(
                    "shepherd up, flock at {}. Nothing running yet; \
                     `shep start <target>` adds a sheep.",
                    paths.home.display()
                )
            } else {
                format!(
                    "shepherd up, flock at {}. {known} sheep restored from the roll; \
                     `shep flock` lists them.",
                    paths.home.display()
                )
            };
            streams.note("start", &message);
            ExitCode::Success
        }
        Err(code) => code,
    }
}

/// Renders a failed connect for an operator rather than for a library caller.
///
/// The absent-socket case gets its own sentence and the next command: on a
/// machine where no shepherd has ever run, `shep-client`'s `ENOENT` reads as
/// a broken install, about a path the operator did not choose. Every other
/// failure keeps the library's wording, since `EACCES` and `ECONNREFUSED`
/// each mean something specific.
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

/// The verbs a version skew must never refuse, spelled the way an operator
/// types them.
///
/// A verb belongs here only if it is a way out of a skew, never merely one
/// that is inconvenient to lose: a guard whose remedy is itself guarded
/// leaves a live daemon and a live flock nothing can touch.
/// [`VERSION_SKEW_REMEDY`] reads `shep daemon reload` out of this list rather
/// than spelling it twice.
const RECOVERY_VERBS: [&str; 3] = ["kill", "daemon reload", "ping"];

/// Which [`RECOVERY_VERBS`] entry `command` is, or `None` for an ordinary
/// verb the version guard applies to.
///
/// Returns the name rather than a bool so a test can hold this mapping and
/// that list against each other.
fn recovery_verb(command: &Commands) -> Option<&'static str> {
    match command {
        Commands::Kill => Some("kill"),
        Commands::Ping => Some("ping"),
        // A bare `shep daemon` is the hidden boot re-exec, which reaches no
        // shepherd and so has nothing to be exempt from.
        Commands::Daemon(args) => match args.cmd {
            Some(cli::DaemonCmd::Reload) => Some("daemon reload"),
            None => None,
        },
        _ => None,
    }
}

/// Whether a shepherd of a different version refuses this invocation.
///
/// `pub(crate)`: `lookout`, `whistle` and `foreground` call `Client::connect`
/// in their own modules and name this at each connect site, always
/// [`Self::Enforce`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionGuard {
    /// Refuse: this verb needs a shepherd that agrees with this binary.
    Enforce,
    /// Never refuse, whatever the shepherd answers: one of
    /// [`RECOVERY_VERBS`].
    Exempt,
}

impl VersionGuard {
    /// The guard that applies to `command`.
    fn for_command(command: &Commands) -> Self {
        match recovery_verb(command) {
            Some(_) => Self::Exempt,
            None => Self::Enforce,
        }
    }
}

/// Why a skew happens, as the two lines the table form prints.
///
/// Held as rendered lines rather than one string: the JSON form joins them
/// with a space and the table form with a newline.
const VERSION_SKEW_CAUSE: [&str; 2] = [
    "`cargo install shep` replaced the binary. It did not restart the",
    "shepherd, which is still running the old code.",
];

/// The one command that fixes a skew.
///
/// Read out of [`RECOVERY_VERBS`] because the two have to agree: a remedy the
/// guard itself refused would be a dead end.
const VERSION_SKEW_REMEDY: &str = RECOVERY_VERBS[1];

/// The imperative naming [`VERSION_SKEW_REMEDY`], in the two shapes the
/// formats need.
///
/// A `--format json` consumer gets a single-line message with the command
/// inside it. The table form has layout, and its label carries no blank line
/// after it: it sits directly on the command so the two read as one thing.
fn version_skew_instruction(fmt: Format) -> String {
    match fmt {
        Format::Json => format!("Run `shep {VERSION_SKEW_REMEDY}`."),
        Format::Table => format!("Run:\n  shep {VERSION_SKEW_REMEDY}"),
    }
}

/// Refuses a shepherd whose crate version differs from this binary's.
///
/// Any difference, not only a protocol difference: `cargo install shep`
/// replaces the binary and leaves the shepherd running the old code, so the
/// two can agree on every byte of the wire while disagreeing about what a
/// verb does. Compares [`shep_core::protocol::HelloAck::daemon_version`]
/// against `CARGO_PKG_VERSION`, read from a handshake that already succeeded.
///
/// # Errors
/// [`ExitCode::VersionSkew`], after writing the refusal to `streams`, when
/// `guard` is [`VersionGuard::Enforce`] and the shepherd reports a different
/// version. A [`VersionGuard::Exempt`] verb is always `Ok`.
pub(crate) fn refuse_version_skew(
    streams: &mut Streams<'_>,
    client: &Client,
    guard: VersionGuard,
) -> Result<(), ExitCode> {
    let running = client.daemon().daemon_version.as_str();
    if guard == VersionGuard::Exempt || running == env!("CARGO_PKG_VERSION") {
        return Ok(());
    }
    let code = ExitCode::VersionSkew;
    let summary = format!(
        "this shep is {}, the running shepherd is {running}",
        env!("CARGO_PKG_VERSION")
    );
    match streams.fmt {
        // One line, one envelope. A `--format json` consumer has no use for
        // the layout below, and it still gets every fact in `error.message`.
        Format::Json => {
            let cause = VERSION_SKEW_CAUSE.join(" ");
            let instruction = version_skew_instruction(Format::Json);
            streams.fail(code, &format!("{summary}. {cause} {instruction}"));
        }
        // Written straight to the stream, not through `Streams::fail`, whose
        // `terminal_safe::sanitise` collapses every `\n` to a space: the
        // remedy has to sit on a line of its own to be copied.
        Format::Table => {
            let cause = VERSION_SKEW_CAUSE.join("\n");
            // `daemon_version` arrives over the socket and can carry an escape
            // sequence that forges lines on the operator's terminal. This
            // branch bypasses `emit_error`, so it sanitises that value itself.
            let summary = crate::terminal_safe::sanitise(&summary).0;
            let instruction = version_skew_instruction(Format::Table);
            let _ = writeln!(
                streams.err,
                "error[{}]: {summary}\n\n{cause}\n\n{instruction}",
                code.code_str()
            );
        }
    }
    Err(code)
}

/// Connects to the daemon at `paths.socket`. Never autostarts.
///
/// The one seam every verb needing a [`Client`] passes through, so a shepherd
/// of a different version is refused here and no verb has to remember to ask.
async fn connect_client(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: VersionGuard,
) -> Result<Client, ExitCode> {
    match Client::connect(&paths.socket).await {
        Ok(client) => {
            refuse_version_skew(streams, &client, guard)?;
            Ok(client)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            Err(streams.fail(code, &unreachable_message(&err)))
        }
    }
}

/// `shep flock`'s own dispatch, split out of [`run`] so a test can drive it
/// against a real fixture socket.
///
/// Uses its own `Client::connect`, not [`connect_client`], because that
/// helper reports and gives up and this arm has a roll fallback to reach.
/// The version guard is applied by hand here.
///
/// A refusal is not an absence. The roll fallback is for
/// [`shep_client::ConnectError::Connect`] alone, nothing listening at all;
/// every other variant means the shepherd is there and answered.
async fn flock_command(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: VersionGuard,
) -> ExitCode {
    match Client::connect(&paths.socket).await {
        Ok(client) => match refuse_version_skew(streams, &client, guard) {
            Ok(()) => query::flock(&client, streams).await,
            // A skew is not an absence either: the roll fallback below would
            // print a listing while hiding why every other verb is refusing.
            Err(code) => code,
        },
        // The roll fallback is a table-format affordance: under `--format
        // json` a failed invocation leaves stdout empty and puts an error
        // envelope on stderr, absence included.
        Err(_) if streams.fmt == Format::Json => {
            match connect_client(streams, paths, guard).await {
                Ok(client) => query::flock(&client, streams).await,
                Err(code) => code,
            }
        }
        Err(shep_client::ConnectError::Connect { .. }) => query::flock_from_roll(streams, paths),
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &flock_connect_refusal_message(&err))
        }
    }
}

/// Renders `err` for [`flock_command`]'s refusal arm: the shepherd is there,
/// so this reports what it did and names the fix rather than the roll's "no
/// shepherd running".
fn flock_connect_refusal_message(err: &shep_client::ConnectError) -> String {
    format!("{err}; run `shep {VERSION_SKEW_REMEDY}`")
}

/// Resolves this invocation's own [`ShepPaths`] and runs the supervisor in
/// the foreground until a signal or `KillDaemon`.
///
/// Takes no [`Streams`] of its own: the supervisor writes its diagnostics
/// through the subscriber `commands::daemon::install_log_subscriber`
/// installs, and the two error envelopes here each go under their own
/// short-lived lock.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`ShepPaths`] rooted at `root`, so the rule can be exercised without
    /// touching the process-global `$HOME`.
    #[cfg(unix)]
    fn paths_at(root: &std::path::Path) -> ShepPaths {
        let home = root.join(".shep").to_string_lossy().into_owned();
        let env = |key: &str| (key == "SHEP_HOME").then(|| home.clone());
        ShepPaths::resolve(&env, std::path::Path::new("/nonexistent"))
    }

    /// `~/.shep` is a name shep chose, so shep may create it.
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

    #[cfg(unix)]
    #[test]
    fn home_before_reads_a_separate_value_argument() {
        let prefix = [OsString::from("--home"), OsString::from("/tmp/x")];
        assert_eq!(home_before(&prefix), Some(PathBuf::from("/tmp/x")));
    }

    #[cfg(unix)]
    #[test]
    fn home_before_reads_an_equals_form() {
        let prefix = [OsString::from("--home=/tmp/y")];
        assert_eq!(home_before(&prefix), Some(PathBuf::from("/tmp/y")));
    }

    #[cfg(unix)]
    #[test]
    fn home_before_skips_unrelated_tokens_before_finding_home() {
        let prefix = [
            OsString::from("--format"),
            OsString::from("json"),
            OsString::from("--home"),
            OsString::from("/tmp/z"),
        ];
        assert_eq!(home_before(&prefix), Some(PathBuf::from("/tmp/z")));
    }

    #[cfg(unix)]
    #[test]
    fn dog_exit_code_reads_a_normal_exit_status() {
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::ExitStatus::from_raw(7 << 8);
        assert_eq!(dog_exit_code(status), 7);
    }

    #[cfg(unix)]
    #[test]
    fn dog_exit_code_reads_128_plus_signal_for_a_signalled_status() {
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::ExitStatus::from_raw(9); // SIGKILL, no WIFEXITED bit
        assert_eq!(dog_exit_code(status), 128 + 9);
    }

    /// Does not cover the `err.kind()` check in `dispatch_adopted_dog`: a
    /// `MissingRequiredArgument` carries no `ContextKind::InvalidSubcommand`
    /// either, so the context match below it answers `None` regardless.
    #[cfg(unix)]
    #[test]
    fn dispatch_adopted_dog_is_none_for_a_parse_error_that_is_not_invalid_subcommand() {
        let argv: Vec<OsString> = ["shep", "adopt"].into_iter().map(OsString::from).collect();
        let err = Cli::try_parse_from(&argv).unwrap_err();
        assert_ne!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert!(dispatch_adopted_dog(&argv, &err).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dispatch_adopted_dog_is_none_for_a_name_shep_toml_has_never_heard_of() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let argv: Vec<OsString> = ["shep", "--home"]
            .into_iter()
            .map(OsString::from)
            .chain([home.into_os_string(), OsString::from("nosuchdog")])
            .collect();
        let err = Cli::try_parse_from(&argv).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert!(dispatch_adopted_dog(&argv, &err).is_none());
    }

    /// `home` is never pre-created here, unlike the neighbouring
    /// `dispatch_adopted_dog` tests: that absence is what is asserted.
    #[cfg(unix)]
    #[test]
    fn dispatch_adopted_dog_creates_nothing_for_a_missing_shep_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        assert!(!home.exists(), "test setup must start with no $SHEP_HOME");

        let argv: Vec<OsString> = ["shep", "--home"]
            .into_iter()
            .map(OsString::from)
            .chain([home.clone().into_os_string(), OsString::from("nosuchdog")])
            .collect();
        let err = Cli::try_parse_from(&argv).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);

        assert!(dispatch_adopted_dog(&argv, &err).is_none());
        assert!(
            !home.exists(),
            "a failed dog lookup must never create $SHEP_HOME: {}",
            home.display()
        );
    }

    /// `std::process::ExitCode` cannot be inspected, so this asserts only that
    /// a real spawn-and-wait happened. `cli_e2e.rs` pins the argv and
    /// `SHEP_HOME` contract against the real binary.
    #[cfg(unix)]
    #[test]
    fn dispatch_adopted_dog_finds_a_dog_shep_toml_really_has() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let script = dir.path().join("mydog.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mut mode = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&script, mode).unwrap();
        ShepToml::edit(&home.join("shep.toml"), |cfg| {
            cfg.adopt_dog("mydog", &script);
        })
        .unwrap();

        let argv: Vec<OsString> = ["shep", "--home"]
            .into_iter()
            .map(OsString::from)
            .chain([
                home.into_os_string(),
                OsString::from("mydog"),
                OsString::from("koji"),
            ])
            .collect();
        let err = Cli::try_parse_from(&argv).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);

        assert!(
            dispatch_adopted_dog(&argv, &err).is_some(),
            "an adopted dog must dispatch instead of falling through to clap's own error"
        );
    }

    /// The name asserted is the token the operator typed, never the script's
    /// file stem: `mydog.sh` is adopted here as `telemetry`.
    #[cfg(unix)]
    #[test]
    fn a_dog_run_by_name_is_given_the_same_home_and_name_the_shepherd_gives_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let seen = dir.path().join("seen");
        let script = dir.path().join("mydog.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$SHEP_HOME\" \"$SHEP_DOG_NAME\" \"$1\" > {}\n",
                seen.display()
            ),
        )
        .unwrap();
        let mut mode = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
        std::fs::set_permissions(&script, mode).unwrap();

        run_adopted_dog(&script, &home, "telemetry", &[OsString::from("koji")]);

        let seen = std::fs::read_to_string(&seen).unwrap();
        assert_eq!(
            seen.lines().collect::<Vec<_>>(),
            vec![
                home.display().to_string().as_str(),
                "telemetry",
                // The name arrives beside the operator's arguments, never in
                // place of them.
                "koji",
            ]
        );
    }

    /// The mode is why this cannot be `create_dir_all`: create-then-chmod
    /// leaves the directory at the ambient umask between the two syscalls.
    #[cfg(unix)]
    #[test]
    fn a_created_home_is_owner_only_from_the_moment_it_exists() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let (paths, _) = ensure_home_at(paths_at(root.path()), false).unwrap();
        let mode = std::fs::metadata(&paths.home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "a fresh $SHEP_HOME must be owner-only");
    }

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

    /// `shep-dev` on its own must be `shep dev`, not `shep`.
    #[test]
    fn an_alias_with_no_arguments_still_supplies_its_verb() {
        let argv = alias_argv("dev", vec!["shep-dev".into()]);
        assert_eq!(
            argv,
            vec![OsString::from("shep-dev"), OsString::from("dev")]
        );
    }

    /// `shep_daemon::dogs` spawns a built-in dog as `current_exe() dog
    /// <name>`, which under `shep-runtime` is this argument vector.
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

    /// The pass-through is an exact match, not a prefix: a sheep named
    /// `dogfood` must still reach `runtime`.
    #[test]
    fn the_pass_through_matches_the_whole_argument_and_not_a_prefix() {
        let argv = alias_argv("runtime", vec!["shep-runtime".into(), "dogfood".into()]);
        assert_eq!(argv[1], OsString::from("runtime"));
        assert_eq!(argv[2], OsString::from("dogfood"));
    }

    /// A well-formed alias vector must still reach the verb: a `runtime`
    /// subcommand taking a required positional would not.
    #[test]
    fn the_alias_vector_parses_to_the_expected_command() {
        use clap::Parser;
        use cli::Commands;
        let argv = alias_argv(
            "dog",
            vec!["shep-runtime".into(), "dog".into(), "metrics".into()],
        );
        let cli = Cli::try_parse_from(argv).expect("the passthrough vector must parse");
        assert!(matches!(cli.command, Commands::Dog(_)));
    }

    /// `--supervise` is the init's own re-exec.
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

    /// Without `propagate_version` the alias binaries have no working
    /// `--version`: `shep-runtime --version` parses as `shep runtime
    /// --version`.
    #[test]
    fn a_subcommand_answers_version() {
        use clap::Parser;
        let err = Cli::try_parse_from(["shep", "dogs", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn save_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "save"]).unwrap().command,
            Commands::Save
        ));
    }

    #[test]
    fn dogs_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "dogs"]).unwrap().command,
            Commands::Dogs(_)
        ));
    }

    #[test]
    fn dogs_available_parses_with_its_filter() {
        use clap::Parser;
        use cli::Commands;
        let parsed = Cli::try_parse_from(["shep", "dogs", "--available", "spot"])
            .unwrap()
            .command;
        let Commands::Dogs(args) = parsed else {
            panic!("expected dogs")
        };
        assert!(args.available);
        assert_eq!(args.filter.as_deref(), Some("spot"));
    }

    /// `DogArgs` is outside the `SelectorArgs` family, so `cli.rs`'s
    /// requiredness check does not reach its `name` positional.
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

    /// `adopt` needs only `path`; `name` is an optional `--name` flag.
    /// `rehome` shares `DogArgs` with `disable`, so it needs only the name.
    #[test]
    fn adopt_and_rehome_parse_to_their_own_commands_and_require_their_arguments() {
        use clap::Parser;
        use cli::Commands;

        let adopted =
            Cli::try_parse_from(["shep", "adopt", "/opt/bin/shep-otel", "--name", "otel"])
                .unwrap()
                .command;
        let Commands::Adopt(args) = adopted else {
            panic!("expected adopt")
        };
        assert_eq!(args.path, PathBuf::from("/opt/bin/shep-otel"));
        assert_eq!(args.name, Some("otel".to_string()));

        // `--name` is optional: a bare path still parses, with no name.
        let unnamed = Cli::try_parse_from(["shep", "adopt", "/opt/bin/shep-otel"])
            .unwrap()
            .command;
        let Commands::Adopt(args) = unnamed else {
            panic!("expected adopt")
        };
        assert_eq!(args.path, PathBuf::from("/opt/bin/shep-otel"));
        assert_eq!(args.name, None);

        let rehomed = Cli::try_parse_from(["shep", "rehome", "otel"])
            .unwrap()
            .command;
        let Commands::Rehome(args) = rehomed else {
            panic!("expected rehome")
        };
        assert_eq!(args.name, "otel");

        assert!(
            Cli::try_parse_from(["shep", "adopt"]).is_err(),
            "`shep adopt` with no path must be a usage error"
        );
        assert!(
            Cli::try_parse_from(["shep", "rehome"]).is_err(),
            "`shep rehome` with no name must be a usage error"
        );
    }

    /// `--exec`'s value and `enable`'s positional `name` are both strings, so
    /// the two landing in the wrong `AdoptArgs` fields is otherwise silent.
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

        // A plain `enable` carries no path, the branch the dispatch reads
        // to decide `enable` against `adopt`.
        let plain = Cli::try_parse_from(["shep", "enable", "metrics"])
            .unwrap()
            .command;
        let Commands::Enable(args) = plain else {
            panic!("expected enable")
        };
        assert_eq!(args.exec, None);
    }

    #[test]
    fn muster_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "muster"]).unwrap().command,
            Commands::Muster
        ));
    }

    /// Pins clap's parse only. An arm that parses correctly and calls the
    /// wrong function needs a real invocation, which `cli_e2e.rs` covers.
    #[test]
    fn import_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "import"]).unwrap().command,
            Commands::Import(_)
        ));
    }

    /// Pins clap's parse only; `cli_e2e.rs`'s
    /// `barks_reads_the_history_with_no_shepherd_running` proves the arm
    /// reaches `dogs::barks`.
    #[test]
    fn barks_parses_to_its_own_command() {
        use clap::Parser;
        use cli::Commands;
        assert!(matches!(
            Cli::try_parse_from(["shep", "barks"]).unwrap().command,
            Commands::Barks(_)
        ));
    }

    /// Pins clap's parse only; the dispatch arms are covered below.
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

    /// The two verbs are told apart by `--home`: `startup` refuses a
    /// `$SHEP_HOME` that is not there, and `unstartup` ignores `--home`, since
    /// a removal is addressed by the unit's path and label.
    ///
    /// Skipped as root: `unstartup` would reach a real `systemctl` or
    /// `launchctl` against whatever this machine has installed.
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
            run(
                cli,
                style::Presentation::BARE,
                (style::StyleLevel::Full, style::StyleSource::Default),
            )
            .await,
            ExitCode::Usage,
            "startup must refuse a $SHEP_HOME that is not there"
        );

        let cli = Cli::try_parse_from(["shep", "--home", missing, "unstartup"]).unwrap();
        assert_ne!(
            run(
                cli,
                style::Presentation::BARE,
                (style::StyleLevel::Full, style::StyleSource::Default),
            )
            .await,
            ExitCode::Usage,
            "unstartup removes a unit and never reads the home a --home names"
        );
    }

    /// `resurrect` exists for a pm2 muscle-memory invocation, so it must
    /// reach `muster` and stay out of `--help`.
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

    /// Pins `resolve_paths`'s folding of an already-populated
    /// `GlobalArgs::home` only. `$SHEP_HOME` reaches that field through clap,
    /// and is pinned in `cli.rs`.
    #[test]
    fn explicit_home_field_resolves_to_the_expected_shep_paths() {
        let global = cli::GlobalArgs {
            home: Some("/tmp/explicit".into()),
            format: cli::Format::Table,
            quiet: false,
            style: None,
        };
        let paths = resolve_paths(&global).unwrap();
        assert_eq!(paths.home, std::path::Path::new("/tmp/explicit"));
        // The control address is a socket file on unix and a named-pipe name
        // on Windows, so `--home` is asserted to reach both derivations.
        #[cfg(unix)]
        assert_eq!(
            paths.socket,
            std::path::Path::new("/tmp/explicit/run/shep.sock")
        );
        #[cfg(windows)]
        assert_eq!(paths.socket, std::path::Path::new(&paths.pipe_name()));
    }

    #[test]
    fn must_render_bare_is_true_exactly_for_a_piped_stdout_or_a_json_format() {
        assert!(
            !must_render_bare(true, cli::Format::Table),
            "a real terminal asking for a table gets to render one"
        );
        assert!(
            must_render_bare(false, cli::Format::Table),
            "piped stdout must render bare even under --format table"
        );
        assert!(
            must_render_bare(true, cli::Format::Json),
            "--format json must render bare even at a real terminal"
        );
        assert!(must_render_bare(false, cli::Format::Json));
    }

    #[test]
    fn style_from_config_reads_the_level_and_is_lenient_about_everything_else() {
        assert_eq!(
            style_from_config(Some("[style]\nlevel = \"plain\"\n")),
            Some(style::StyleLevel::Plain)
        );
        assert_eq!(style_from_config(None), None, "no file at all");
        assert_eq!(style_from_config(Some("")), None, "an empty file");
        assert_eq!(
            style_from_config(Some("[style")),
            None,
            "a file that will not parse"
        );
        assert_eq!(
            style_from_config(Some("[daemon]\nlog_level = \"info\"\n")),
            None,
            "a config with no [style] table at all"
        );
        assert_eq!(
            style_from_config(Some("[style]\nlevel = \"loud\"\n")),
            None,
            "a level this build does not recognise"
        );
    }

    /// Both must go through [`style::StyleLevel::parse`] rather than
    /// `clap::ValueEnum::from_str`, which does not trim.
    #[test]
    fn style_from_config_trims_the_same_way_shep_style_does() {
        for raw in ["full", " full ", "\tfull\n", "FULL", " FuLl "] {
            assert_eq!(
                style_from_config(Some(&format!("[style]\nlevel = {raw:?}\n"))),
                Some(style::StyleLevel::Full),
                "shep.toml's own level must accept {raw:?} exactly as \
                 $SHEP_STYLE would"
            );
            assert_eq!(
                style::resolve(None, Some(raw), None),
                (style::StyleLevel::Full, style::StyleSource::Env),
                "$SHEP_STYLE must accept {raw:?}"
            );
        }
    }

    /// About the wiring of the flag and a real file into `style::resolve`,
    /// not about the precedence rule itself, which `style.rs` pins.
    #[test]
    fn resolve_style_reads_the_flag_and_the_real_shep_toml_it_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shep.toml"), "[style]\nlevel = \"plain\"\n").unwrap();
        let global = cli::GlobalArgs {
            home: Some(dir.path().to_path_buf()),
            format: cli::Format::Table,
            quiet: false,
            style: None,
        };
        assert_eq!(
            resolve_style(&global),
            (style::StyleLevel::Plain, style::StyleSource::Config),
            "with no flag, shep.toml's own level answers"
        );

        let global = cli::GlobalArgs {
            style: Some(style::StyleLevel::Bare),
            ..global
        };
        assert_eq!(
            resolve_style(&global),
            (style::StyleLevel::Bare, style::StyleSource::Flag),
            "the flag wins over the very shep.toml that set plain above"
        );
    }

    #[test]
    fn style_write_is_overridden_only_by_flag_or_env() {
        assert!(style_write_is_overridden(style::StyleSource::Flag));
        assert!(style_write_is_overridden(style::StyleSource::Env));
        assert!(!style_write_is_overridden(style::StyleSource::Config));
        assert!(!style_write_is_overridden(style::StyleSource::Default));
    }

    /// What `shep style <level>` writes must be what `style_from_config`, the
    /// reader `resolve_style` uses, reads back.
    ///
    /// `#[cfg(unix)]`: the write goes through `commands::shep_toml`'s
    /// `ConfigLock`, whose `cfg(windows)` arm nothing in this crate executes.
    #[cfg(unix)]
    #[tokio::test]
    async fn style_with_a_level_writes_shep_toml_and_the_config_reads_it_back() {
        use clap::Parser;

        for (raw, expected) in [
            ("full", style::StyleLevel::Full),
            ("plain", style::StyleLevel::Plain),
            ("bare", style::StyleLevel::Bare),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().to_str().unwrap();
            let cli = Cli::try_parse_from(["shep", "--home", home, "style", raw]).unwrap();
            assert_eq!(
                run(
                    cli,
                    style::Presentation::BARE,
                    (style::StyleLevel::Full, style::StyleSource::Default),
                )
                .await,
                ExitCode::Success,
                "style {raw}"
            );

            let written = std::fs::read_to_string(dir.path().join("shep.toml")).unwrap();
            assert_eq!(
                style_from_config(Some(&written)),
                Some(expected),
                "style {raw}"
            );
        }
    }

    /// The no-arg form is a report and only a report. `#[cfg(unix)]` to stay
    /// paired with the test above.
    #[cfg(unix)]
    #[tokio::test]
    async fn style_with_no_level_reports_and_writes_nothing() {
        use clap::Parser;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let cli = Cli::try_parse_from(["shep", "--home", home, "style"]).unwrap();
        assert_eq!(
            run(
                cli,
                style::Presentation::BARE,
                (style::StyleLevel::Full, style::StyleSource::Default),
            )
            .await,
            ExitCode::Success
        );

        assert!(
            !dir.path().join("shep.toml").exists(),
            "the no-arg form must not create a shep.toml that was not there"
        );
    }

    /// Pins the structural rule: `completions` never reaches `resolve_paths`.
    ///
    /// It cannot pin the behaviour. With `$HOME` set, a reinstated
    /// `resolve_paths` would succeed and fall through to the same call, so
    /// `run` returns `Success` either way, and unsetting `$HOME` here is
    /// `unsafe` in edition 2024. The e2e tier spawns the real binary with
    /// `$HOME` cleared instead.
    #[tokio::test]
    async fn completions_never_resolves_paths() {
        use clap::Parser;
        let argv = ["shep", "completions", "bash"];
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} failed: {e}"));
        assert_eq!(
            run(
                cli,
                style::Presentation::BARE,
                (style::StyleLevel::Full, style::StyleSource::Default),
            )
            .await,
            ExitCode::Success
        );
    }

    /// `ENOENT` is what a person meets where no shepherd has ever run.
    /// `EACCES` is a socket this user may not have, and `shep start` would be
    /// the wrong fix for it.
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

    /// A [`Streams`] over two byte buffers, so a refusal's exact text can be
    /// read back. `BARE` because these tests assert on words, not colour.
    fn buffered_streams<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Streams<'a> {
        Streams {
            out,
            err,
            style: style::Presentation::BARE,
            fmt: Format::Table,
        }
    }

    /// A real [`Client`], past a real handshake, whose peer announced
    /// `version`. The [`shep_client::testing::FakeDaemon`] is returned so it
    /// outlives the client.
    async fn client_announcing(
        addr: &std::path::Path,
        version: &str,
    ) -> (Client, shep_client::testing::FakeDaemon) {
        let ack = shep_core::protocol::HelloAck {
            daemon_version: version.to_owned(),
            protocol: shep_core::protocol::PROTOCOL_VERSION,
            pid: 4242,
        };
        shep_client::testing::fake_client_with_ack(addr, ack).await
    }

    /// The case a protocol-only check misses: the wire versions agree and the
    /// crate versions do not.
    #[tokio::test]
    async fn a_version_difference_with_no_protocol_difference_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _fake) = client_announcing(&addr, "0.1.8").await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = buffered_streams(&mut out, &mut err);
        let code = refuse_version_skew(&mut streams, &client, VersionGuard::Enforce)
            .expect_err("a differing crate version must be refused");
        assert_eq!(code, ExitCode::VersionSkew);
    }

    #[tokio::test]
    async fn the_error_names_the_command_that_fixes_it() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _fake) = client_announcing(&addr, "0.1.8").await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = buffered_streams(&mut out, &mut err);
            let _ = refuse_version_skew(&mut streams, &client, VersionGuard::Enforce);
        }
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("error[version_skew]"), "{text}");
        assert!(text.contains("this shep is"), "{text}");
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        assert!(text.contains("the running shepherd is 0.1.8"), "{text}");
        assert!(
            text.contains("`cargo install shep` replaced the binary"),
            "{text}"
        );
        assert!(text.contains("shep daemon reload"), "{text}");
    }

    /// The indentation is what makes the remedy line copyable, so it sits on
    /// its own line rather than folded into the sentence above it.
    #[tokio::test]
    async fn the_table_form_names_the_remedy_as_an_instruction_not_only_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _fake) = client_announcing(&addr, "0.1.8").await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = buffered_streams(&mut out, &mut err);
            let _ = refuse_version_skew(&mut streams, &client, VersionGuard::Enforce);
        }
        let text = String::from_utf8(err).unwrap();

        // No blank line between the label and the command: a gap reads as
        // two unrelated things.
        assert!(
            text.contains("Run:\n  shep daemon reload"),
            "the label must sit directly on the copyable line it points at: {text}"
        );
        // The sentence above the indented line must not repeat the command.
        assert_eq!(
            text.matches("shep daemon reload").count(),
            1,
            "the remedy is named once, not restated in prose: {text}"
        );
    }

    /// A daemon that answered the handshake and refused it is not an absence:
    /// "no shepherd running" would send the operator to the roll instead.
    #[tokio::test]
    async fn flock_reports_a_refusal_as_a_refusal_not_as_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        let refusal = shep_core::protocol::RpcError {
            code: shep_core::protocol::RpcErrorCode::ProtocolMismatch,
            message: "this daemon speaks protocol 1, this client speaks 2".to_string(),
            daemon_version: Some("0.1.8".to_string()),
        };
        let _daemon = shep_client::testing::fake_daemon(&paths.socket, Err(refusal)).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = buffered_streams(&mut out, &mut err);
            flock_command(&mut streams, &paths, VersionGuard::Enforce).await
        };

        assert_ne!(code, ExitCode::Success);
        let text = String::from_utf8(err).unwrap();
        assert!(
            !text.contains("no shepherd running"),
            "a refusal is not an absence: {text}"
        );
        assert!(text.contains("shep daemon reload"), "{text}");
    }

    #[tokio::test]
    async fn flock_still_falls_back_to_the_roll_for_a_genuine_absence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        // No socket bound at `paths.socket`, so `connect(2)` itself fails.

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = buffered_streams(&mut out, &mut err);
            flock_command(&mut streams, &paths, VersionGuard::Enforce).await
        };

        assert_eq!(code, ExitCode::DaemonUnreachable);
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("no shepherd running"), "{text}");
    }

    #[tokio::test]
    async fn a_matching_version_passes_without_a_word() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _fake) = client_announcing(&addr, env!("CARGO_PKG_VERSION")).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = buffered_streams(&mut out, &mut err);
            refuse_version_skew(&mut streams, &client, VersionGuard::Enforce)
                .expect("a matching version is not a skew");
        }
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    #[tokio::test]
    async fn the_recovery_verbs_are_not_refused() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let (client, _fake) = client_announcing(&addr, "0.1.8").await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = buffered_streams(&mut out, &mut err);
            refuse_version_skew(&mut streams, &client, VersionGuard::Exempt)
                .expect("a recovery verb is never refused on version skew");
        }
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    /// An empty [`StartArgs`], for asking which guard `add` gets. The guard
    /// reads the verb alone, so no field here reaches it.
    fn add_args() -> StartArgs {
        StartArgs {
            targets: Vec::new(),
            name: None,
            fold: None,
            cwd: None,
            interpreter: None,
            flockfile: false,
            reset: None,
        }
    }

    /// A [`DaemonArgs`] carrying `cmd` and nothing else, for asking which
    /// guard the `daemon` verb's two shapes get.
    fn daemon_args(cmd: Option<cli::DaemonCmd>) -> DaemonArgs {
        DaemonArgs {
            cmd,
            no_restore: false,
            foreground: false,
            log_json: None,
            log_level: None,
            socket: None,
            max_cron_sleep: None,
        }
    }

    /// Fails if a verb leaves [`RECOVERY_VERBS`], or arrives without being
    /// listed there with its reason.
    #[test]
    fn every_exempt_verb_is_one_of_the_documented_recovery_verbs() {
        for command in [
            Commands::Kill,
            Commands::Ping,
            Commands::Daemon(daemon_args(Some(cli::DaemonCmd::Reload))),
        ] {
            let verb =
                recovery_verb(&command).unwrap_or_else(|| panic!("{command:?} must stay exempt"));
            assert!(
                RECOVERY_VERBS.contains(&verb),
                "{verb} is exempt but undocumented"
            );
        }
        assert_eq!(
            recovery_verb(&Commands::Daemon(daemon_args(Some(cli::DaemonCmd::Reload)))),
            Some("daemon reload"),
            "the verb the skew refusal names must be the verb the skew guard exempts"
        );
        // The hidden boot re-exec, which reaches no shepherd at all.
        assert_eq!(recovery_verb(&Commands::Daemon(daemon_args(None))), None);
        assert_eq!(
            VersionGuard::for_command(&Commands::Daemon(daemon_args(None))),
            VersionGuard::Enforce
        );
        assert_eq!(recovery_verb(&Commands::Flock), None);
        assert_eq!(
            VersionGuard::for_command(&Commands::Flock),
            VersionGuard::Enforce
        );
        assert_eq!(
            VersionGuard::for_command(&Commands::Kill),
            VersionGuard::Exempt
        );
        // `add` carries a request an older shepherd cannot decode, and it
        // reaches `Enforce` through the `_` arm rather than by being named.
        assert_eq!(
            VersionGuard::for_command(&Commands::Add(add_args())),
            VersionGuard::Enforce
        );
    }

    /// Drives `kill`'s dispatch arm, the one with its own connect, against a
    /// home no shepherd owns.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_dispatches_kill_to_the_socket_free_path() {
        use clap::Parser;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        // `pids/` holds the lock `kill`'s socket-free path reads, `run/` the
        // control socket it tries first.
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.to_string_lossy().into_owned()),
            std::path::Path::new("/nonexistent"),
        );
        std::fs::create_dir_all(&paths.pids).unwrap();
        std::fs::create_dir_all(&paths.run).unwrap();
        let argv = ["shep", "--home", home.to_str().unwrap(), "kill"];
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} failed: {e}"));

        assert_eq!(
            run(
                cli,
                style::Presentation::BARE,
                (style::StyleLevel::Full, style::StyleSource::Default),
            )
            .await,
            ExitCode::DaemonUnreachable,
            "`kill` against an unowned home must reach its own socket-free path"
        );
    }
}
