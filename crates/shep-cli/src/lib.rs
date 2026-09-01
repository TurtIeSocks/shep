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
//! `serve`, `runtime`, and `dev` (spec `docs/specs/shep-v1.md` §9): a
//! hand-rolled static file server (no `axum`, no `tower-http`; the maintainer's ruling
//! — the ledger has the reasoning), a foreground no-daemon container mode
//! with a PID-1 init split for zombie reaping, and an isolated foreground
//! development flock. Three `[[bin]]` targets sit over this library: `shep`
//! itself, plus `shep-runtime` and `shep-dev`, the two container-entrypoint
//! aliases that prepend their verb before parsing (see
//! `main_runtime`/`main_dev`). The ratatui `lookout` dashboard has all four
//! panes — the flock table and shell, the bleats feed, the sheep detail
//! pane, and the host-usage strip — a name filter that narrows the table in
//! place, lambs in the detail pane, and the three action keys (`x` stop,
//! `R` restart, `L` reload) behind the `--allow-control` gate, each arming
//! a confirm rather than acting on the keypress that pressed it. Remaining
//! workspace debt,
//! none of it here: `docs/specs/deferred.md`.

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

use cli::{AdoptArgs, Commands, DaemonArgs, Format};
use cli::{Cli, GlobalArgs};
use commands::admin;
use commands::bleats;
use commands::daemon::{daemon_exit_code, run_daemon};
use commands::dev;
use commands::dogs;
use commands::import;
use commands::kv;
use commands::lifecycle;
use commands::logs;
use commands::muster;
use commands::query;
use commands::runtime;
use commands::schema;
// Imports the function directly, not the module: `commands::serve` would
// collide in this scope with the crate-root `serve` module (the static-file
// implementation `commands::serve::serve` itself calls into).
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
    if std::env::var_os("SHEP_TERM_PANIC_PROBE").is_some() {
        lookout::term::probe_panic_for_test();
    }
    // `try_parse_from`, not `parse_from`: the latter prints and exits inside
    // clap, and two of the four invocations that should carry a shepherd
    // status line -- bare `shep`, and `shep help` -- never become a `Cli` at
    // all. Holding the result lets those two be answered before clap has its
    // say. Ok invocations are unaffected.
    let parsed = Cli::try_parse_from(argv.clone());
    // Before clap ever gets to render its own "unrecognized subcommand"
    // error, check whether the token it could not place names an adopted
    // dog. Sync, and cheap for the overwhelming majority of invocations
    // that parse cleanly (`Ok` short-circuits `if let Err` immediately) —
    // see `dispatch_adopted_dog`'s own doc for the whole contract.
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
            // code, exactly as `parse_from` would have.
            err.exit();
        }
    };
    // Resolved once, here, and passed down rather than re-derived at each of
    // the two `run` arms (`cfg(unix)`/`cfg(windows)`) or at every `Streams`
    // construction inside them — see `resolve_style` and `must_render_bare`
    // for why the two steps (what is configured, and whether the hard rule
    // overrides it) are kept separate.
    let (configured, _source) = resolve_style(&cli.global);
    let level = if must_render_bare(std::io::stdout().is_terminal(), cli.global.format) {
        style::StyleLevel::Bare
    } else {
        configured
    };
    // `NO_COLOR`/`$TERM`/`$COLORTERM`/the terminal width read here, once, and
    // nowhere else -- see `style::Presentation`'s own doc for why every
    // terminal fact is resolved at this seam rather than read again inside
    // `table_of`. `must_render_bare`'s hard rule already forced `level` to
    // `Bare` above when it applies, and `Presentation::new`'s own `colour`
    // folding (`level.colour() && !no_color_set(..)`) is `false` for `Bare`
    // regardless of what the environment says, so the hard rule holds
    // either way.
    let style = style::Presentation::new(
        level,
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
        std::env::var_os("COLORTERM").as_deref(),
        output::terminal_width(),
    );
    std::process::ExitCode::from(runtime.block_on(run(cli, style)) as u8)
}

/// Before clap's own "unrecognized subcommand" error (suggestions and
/// all), checks whether the token it could not place names an adopted
/// dog — `shep <dogname> [args...]` runs it directly, git/cargo's own
/// external-subcommand precedent (`git foo` runs `git-foo`).
///
/// Resolved against **adopted dogs only**, never a `$PATH` scan: the
/// operator explicitly ran `shep adopt` for these, and `commands::dogs`'
/// own vetting already ran once, at adopt time. A `$PATH` scan would let
/// any stray binary on the machine become a shep verb.
///
/// **Built-in verbs always win, structurally, not by a check here.** This
/// only ever runs once clap has already failed to match `token` against
/// every real subcommand and alias, so a dog named `stop` can never
/// shadow `shep stop` — and `shep adopt` itself refuses to register a name
/// that collides with one (`commands::dogs::collides_with_a_verb`), since
/// such a dog could never be reached anyway.
///
/// `None` for every case that should still reach clap's own error: a
/// parse failure that is not `InvalidSubcommand`, a `$SHEP_HOME` this
/// process cannot resolve, or a name `shep.toml` has never heard of. None
/// of those earn a special message — the token just was not an adopted
/// dog, and clap's ordinary unknown-verb error, suggestions included, is
/// exactly right for that.
///
/// The explicit `err.kind()` check below is not redundant with the
/// `ContextKind::InvalidSubcommand` match just under it, even though
/// nothing in this crate's own test suite can tell the two apart: clap
/// also attaches that same context to an `ErrorKind::ArgumentConflict`
/// (`clap_builder::error::subcommand_conflict`), but only when a command
/// sets `args_conflicts_with_subcommands` — [`cli::Cli`] never does, so
/// that producer is unreachable through this binary's own parse tree
/// today. The check stays anyway, documented rather than deleted: relying
/// on an unset builder option to keep two error shapes from colliding is
/// exactly the kind of coupling that breaks silently the day someone
/// upstream of this function sets it for an unrelated reason.
fn dispatch_adopted_dog(argv: &[OsString], err: &clap::Error) -> Option<std::process::ExitCode> {
    if err.kind() != clap::error::ErrorKind::InvalidSubcommand {
        return None;
    }
    let name = match err.get(clap::error::ContextKind::InvalidSubcommand) {
        Some(clap::error::ContextValue::String(name)) => name.as_str(),
        _ => return None,
    };
    // The token's own position in the raw argv, not clap's — clap's error
    // carries the name but not where it sat, and everything after it is
    // this dog's own argv, passed through untouched.
    let index = argv.iter().position(|arg| arg.to_str() == Some(name))?;
    let global = GlobalArgs {
        home: home_before(&argv[1..index]),
        format: Format::Table,
        quiet: false,
        style: None,
    };
    let paths = resolve_paths(&global).ok()?;
    // `_readonly`, not `ShepToml::edit`: this runs on every unrecognized
    // verb, most of which are typos rather than dog names, and `edit`
    // saves unconditionally even when its closure only reads -- a lookup
    // that finds nothing would still create `$SHEP_HOME` and write a
    // `shep.toml` as a side effect of failing.
    let path = ShepToml::adopted_dog_path_readonly(&paths.daemon_config, name)
        .ok()
        .flatten()?;
    let dog_argv = argv[index + 1..].to_vec();
    Some(run_adopted_dog(&path, &paths.home, name, &dog_argv))
}

/// Scans `prefix` — the argv tokens before the one clap could not place —
/// for `--home`, in either `--home value` or `--home=value` form. The only
/// global flag [`resolve_paths`] reads, and the only one
/// [`dispatch_adopted_dog`] needs reconstructed: the rest of `GlobalArgs`
/// (`--format`, `--quiet`, `--style`) governs only how this crate renders
/// its own output, never which binary `shep.toml` names for a dog.
///
/// Falls back to `$SHEP_HOME` from the environment when `prefix` names no
/// `--home` — reproducing by hand the fallback clap's own `env =
/// "SHEP_HOME"` attribute gives `GlobalArgs::home` on a parse that
/// succeeds, for the one parse that did not.
///
/// `#[cfg(unix)]` for the same reason [`dispatch_adopted_dog`] (its only
/// caller) is: nothing here is unix-specific, but leaving it uncompiled on
/// Windows rather than merely unreachable keeps the Windows tier's own
/// `cargo check` gate free of one more dead-code warning for a function
/// with no caller there at all.
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

/// Runs `path` — an adopted dog's binary — the way an operator invoking it
/// by name expects: `extra_args` passed through exactly as typed, the two
/// environment variables every dog is promised (`$SHEP_HOME` to find the
/// shepherd, `$SHEP_DOG_NAME` to name its own `[dog.<name>]` section),
/// stdio inherited so an interactive dog behaves like any other program run
/// directly from a shell.
///
/// `name` is the token the operator typed, which is what
/// [`dispatch_adopted_dog`] resolved `path` from, so a dog run this way and
/// the same dog run by the shepherd agree about what they are called. They
/// have to: a dog invoked here to print or check its own configuration
/// would otherwise be reading a different section than the one it runs
/// under.
///
/// **A second invocation mode, deliberately distinct from the supervised
/// one.** A dog the shepherd starts gets no argv and these same two env
/// entries (`shep_daemon::dogs::dog_app`'s own doc — that contract is
/// unchanged by this function existing); a dog an operator names on the
/// command line gets whatever they typed after it, because passing
/// arguments through is the entire reason to invoke it this way instead
/// of through `shep enable`.
///
/// Exit code mirrors shell convention via [`dog_exit_code`]: the child's
/// own code, or `128 + signal` if it died by one — the same reading
/// `commands::reap::classify` gives a reaped supervisor.
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

/// `status`'s own exit code, or `128 + signal` if it died by one — the
/// shell convention `commands::reap::classify` already reads a reaped
/// supervisor's status by.
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
/// No `128 + signal` arm, because there are no signals: every Windows
/// process exit carries a code, so `ExitStatus::code()` is never `None`
/// there and the shell convention the unix arm reproduces has nothing to
/// encode. The `unwrap_or` is defensive only — reaching it would mean the OS
/// reported an exit with no status at all.
#[cfg(windows)]
fn dog_exit_code(status: std::process::ExitStatus) -> u8 {
    status.code().unwrap_or(1) as u8
}

/// Prints the one-line shepherd status to stderr, for an invocation clap
/// answers by itself.
///
/// stderr rather than stdout so `shep help > file` and
/// `shep completions zsh > _shep` stay clean -- the completion script above
/// all, which is 1900 lines of shell meant to be sourced, and would execute
/// a status line as code.
///
/// Silent when stderr is not a terminal, matching the welcome, and silent
/// when `argv` names a `--home`: the parse that would have told us which
/// home is the parse that just failed, and a line naming the wrong flock is
/// worse than no line.
#[cfg_attr(windows, allow(dead_code))]
async fn print_shepherd_status(argv: &[OsString]) {
    if !std::io::stderr().is_terminal() || argv.iter().any(|a| a == "--home") {
        return;
    }
    let global = GlobalArgs {
        home: None,
        format: Format::Table,
        quiet: false,
        // This status line is plain prose to stderr, not a rendered
        // command's table — nothing here reads `style`, so there is no
        // level for this synthetic `GlobalArgs` to get right.
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
/// target, and [`resolve_style`] below calls it from `run_argv` — shared,
/// unconditional code — to find `shep.toml` for the style level's config
/// layer. That is the one Windows call site today; the Windows build's `run`
/// itself still does not call it, since refusing outright is the whole
/// Windows deliverable for now.
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

/// Parses `shep.toml`'s `[style] level` into a [`style::StyleLevel`].
///
/// `None` covers every way this layer can say nothing: no file, a file that
/// will not parse, no `[style]` table, or a `level` this build does not
/// recognise. All of those read the same as "the config did not answer" —
/// [`style::resolve`]'s own contract for its `config` parameter — rather
/// than as a refusal, matching [`whistle::gate::resolve_control`]'s reading
/// of a broken `shep.toml` as "no" rather than an error.
///
/// Takes the file's already-read text, not a path, for the same reason
/// `resolve_control` does: every case becomes a pure function of a string,
/// testable without a tempdir.
///
/// The `&|_| None` environment closure is [`shep_core::config::DaemonConfig::load`]'s
/// layering for `SHEP_LOG_JSON`/`SHEP_LOG_LEVEL`/`SHEP_SOCKET`/
/// `SHEP_MAX_CRON_SLEEP` — none of which touches `[style]` in either
/// direction, so `None` here defends nothing and costs nothing either.
///
/// Parses the level through [`style::StyleLevel::parse`], the same function
/// `resolve_style` hands `$SHEP_STYLE` to -- not `clap::ValueEnum::from_str`,
/// which does not trim whitespace (see [`style::StyleLevel`]'s own doc);
/// `style_from_config_trims_the_same_way_shep_style_does` (below) pins that
/// the two agree.
/// Parses `shep.toml`'s `[interpreters]` table into shep-cli's own
/// extension -> interpreter map, for `lifecycle::start` to fold onto every
/// resolved app whose own `interpreter` is still unset (task 47's
/// precedence: shep.toml, then a Flockfile's own field, then
/// `--interpreter`).
///
/// Empty covers every way this layer can say nothing -- the same set
/// [`style_from_config`] enumerates for itself just below: no file, a file
/// that will not parse, or simply no `[interpreters]` table. Reading a
/// broken `shep.toml` as "no mapping" rather than a hard error is
/// deliberate for the same reason it is there: `shep start` must still be
/// able to start a plain script by path, or honour an explicit
/// `--interpreter`, even when the file an operator is mid-edit on happens
/// not to parse this moment.
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
/// `$SHEP_STYLE`, then `shep.toml`'s `[style] level`, then `full` —
/// [`style::resolve`]'s own precedence order.
///
/// Reads `shep.toml` itself via [`resolve_paths`] rather than waiting for
/// [`ensure_home`]: `--style` and `$SHEP_STYLE` must still work with no
/// `$SHEP_HOME` resolvable at all (a container with no `$HOME`, say), and
/// `resolve_paths` is the side-effect-free half of path resolution — unlike
/// `ensure_home`, it never creates a directory just to answer this question.
/// A `shep.toml` that cannot be read yet (paths unresolved, or the file is
/// simply not there) reads the same as an empty config: the layer below it
/// in precedence answers instead.
///
/// This is deliberately NOT where the hard rule (`--format json` and a
/// non-terminal stdout force [`style::StyleLevel::Bare`]) is applied — see
/// [`run_argv`]. `shep style`'s own report reads this function's unforced
/// result: an operator piping `shep style | cat` needs to see what is
/// actually configured, not `bare` every time regardless, which is exactly
/// the "edited shep.toml and saw nothing change" failure
/// [`style::StyleSource`]'s doc says this command exists to prevent.
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

/// Whether a level [`Commands::Style`]'s set form just wrote to
/// `shep.toml` is actually the level that will run, given which layer
/// [`resolve_style`] reports the moment after that write.
///
/// Only `Flag` and `Env` can say no: those are the two layers
/// [`style::resolve`]'s precedence puts above `shep.toml`, so they are the
/// two spellings an operator needs named when the write they just asked
/// for will keep being overridden. `Config` is the value this call just
/// wrote, and `Default` cannot follow a successful write to it -- an
/// unwritten `[style] level` is exactly what a successful write just
/// stopped being true.
///
/// A pure decision over [`style::StyleSource`] rather than reading
/// `$SHEP_STYLE`/`--style` again here, matching [`must_render_bare`]'s own
/// idiom below: the real call happens once, at the call site in `run`, and
/// this stays testable without the environment mutation this crate's
/// `#![forbid(unsafe_code)]` rules out in a test.
///
/// Its only real call site is inside `Commands::Style`'s set-form arm, in
/// this file's `#[cfg(unix)]` `run` -- the same reason `output::Streams::out`
/// and `flourish::empty_flock` each carry the same
/// `#[cfg_attr(windows, allow(dead_code))]`.
#[cfg_attr(windows, allow(dead_code))]
fn style_write_is_overridden(source: style::StyleSource) -> bool {
    matches!(source, style::StyleSource::Flag | style::StyleSource::Env)
}

/// Whether output must render exactly as it always has: no boxes, no
/// colour, no sheep, regardless of what `--style`/`$SHEP_STYLE`/`shep.toml`
/// asked for.
///
/// The hard rule from the spec (`docs/brainstorming/specs/2026-08-18-pretty-cli-design.md`
/// §3, "The hard rule"): piped output and `--format json` must be
/// byte-identical to before this feature, because `cli_e2e` asserts exact
/// stdout through a pipe and `shep completions` writes ~1900 lines of shell
/// a stray escape would execute as code.
///
/// Takes terminal-ness as a parameter rather than calling `is_terminal()`
/// itself, matching this crate's own idiom for a presentation input
/// (`commands/daemon.rs`'s `ansi_enabled` does the same for `NO_COLOR`): the
/// real call happens once, at [`run_argv`], and this stays a pure decision
/// the test below can exercise directly.
/// What `shep startup`/`shep unstartup` say on Windows.
///
/// A refusal that names the boundary rather than a bare "unsupported": the
/// rest of shep works on this platform, and an operator who has just watched
/// `shep start` succeed needs to know why this one verb does not, and what
/// to do instead.
///
/// Boot-time supervision on Windows means a real service registered with the
/// Service Control Manager, which is a different program shape rather than a
/// sixth unit template — see `commands/mod.rs`'s note on the gated module.
#[cfg(windows)]
const WINDOWS_NO_SERVICE: &str = "\
shep startup installs a boot-time service, and on Windows that means \
registering with the Service Control Manager -- not yet built (Tier B in \
docs/specs/windows-estimate.md).\n  \
the shepherd itself works here: run `shep start` in your own session, or wrap \
`shep runtime` in a service manager such as NSSM or WinSW.";

fn must_render_bare(stdout_is_terminal: bool, fmt: cli::Format) -> bool {
    !stdout_is_terminal || fmt == cli::Format::Json
}

/// Why [`ensure_home`] would not hand back a layout.
///
/// A type rather than a bare [`ExitCode`] because two of the three carry the
/// path they are about, and an operator cannot act on a refusal that does not
/// name it.
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
/// `#[cfg(unix)]`, like [`UNRESOLVED_HOME`] which the `Display` impl reads:
/// the Windows `run` refuses before any home is resolved, so none of this is
/// reachable there. A `cfg_attr(windows, allow(dead_code))` compiled it on
/// Windows instead and broke the build on a constant that does not exist
/// there.
///
/// # Errors
///
/// - [`HomeRefusal::Missing`] — `explicit`, and the directory is not there.
/// - [`HomeRefusal::Io`] — the directory could not be created.
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

/// Writes `shep.toml`'s starter `[interpreters]` mapping the moment
/// `$SHEP_HOME` is first created (task 47) -- see
/// [`ShepToml::write_starter_interpreters`] for what it writes and why.
///
/// Called from every `home_is_new` site alongside [`welcome::on_first_run`],
/// but not folded into that function: the welcome banner is suppressed
/// under `--format json` and a piped stderr (a fresh machine is exactly
/// where a provisioning script runs first), and the scaffold this writes
/// must happen regardless -- the mapping is what lets that very script's
/// `shep start server.js` work without also passing `--interpreter`.
///
/// Best-effort: a failure here (a full disk, a permissions problem) must
/// not turn "shep created your home directory" into a failed command, so
/// this only ever reports to stderr and continues. [`ShepToml::edit`]
/// creates `$SHEP_HOME` itself if this runs before [`ensure_home_at`]'s own
/// directory creation is visible to it, so ordering between the two does
/// not matter; this is called after it regardless, for a `paths.home` that
/// is already known to exist.
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

/// Dispatches a parsed CLI command using the resolved presentation settings.
///
/// Commands that require a shepherd connect through the appropriate client
/// path, while local and long-running commands are handled directly.
///
/// # Examples
///
/// ```no_run
/// let exit_code = run(cli, style).await;
/// assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
/// ```
///
/// `cli` contains the parsed command and global options. `style` specifies the
/// presentation settings used for command output.
///
/// # Returns
///
/// The process exit code produced by the dispatched command.
async fn run(cli: Cli, style: style::Presentation) -> ExitCode {
    let fmt = cli.global.format;
    // Resolved once, here, rather than at each of the seventeen call sites
    // that reach a shepherd: `cli.command` is partially moved by the
    // dispatch below, so an arm cannot borrow it to ask this question, and
    // an arm that could would be an arm that could forget to.
    let guard = VersionGuard::for_command(&cli.command);

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
            // Status to stderr before the script: `shep completions zsh`
            // writes 1900 lines of shell to stdout, meant to be redirected
            // or sourced, and a status line in it would be executed.
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
            // `daemon reload` is a different verb wearing the same word:
            // it stops a shepherd and starts one rather than being one.
            // Unlocked handles, for the reason `bleats` takes them -- this
            // runs for as long as a shepherd's teardown ladder plus a boot,
            // which is seconds, not the milliseconds the locked pair below
            // is right for.
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
        // Routed through `ensure_home` rather than reading `--home` raw:
        // this is the verb that installs a unit without starting anything,
        // so it is the one that needs `$SHEP_HOME` to exist beforehand, and
        // for three phases it was the only command that refused instead of
        // creating it. `install` keeps its own check as well -- see
        // `a_shep_home_that_does_not_exist_is_refused` for the trap that
        // guards -- but this gate fires first and says more.
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
        // Needs no `$SHEP_HOME` at all — same reasoning as `Completions`
        // just above, and the same early spot for it.
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
        // Unconditional, unlike the welcome banner just below: this must
        // run for `shep welcome` too, which also creates a fresh home but
        // is excluded from the banner call (its own arm prints the same
        // text a moment later). The banner's exclusion is about not
        // printing twice; there is no equivalent reason to skip the
        // scaffold that makes `shep start server.js` work afterward.
        scaffold_first_run_interpreters(&paths);
    }
    // `Welcome` is excluded because its own arm prints the same text to
    // stdout a moment later, and a `shep welcome` that created the home
    // should print once, not twice.
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
            style,
            fmt,
        };
        return match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => bleats::bleats(&client, &mut streams, cli.global.quiet, args).await,
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
            style,
            fmt,
        };
        return lookout::lookout(&mut streams, &paths, args).await;
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
            style,
            fmt,
        };
        return serve_command(&mut streams, &paths, args).await;
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
            style,
            fmt,
        };
        return runtime::runtime(&mut streams, cli.global.quiet, paths, args).await;
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
        style,
        fmt,
    };

    match cli.command {
        // No client: the welcome is local text about a local directory, and
        // asking a shepherd for it would make `shep welcome` fail on exactly
        // the fresh machine it exists to greet.
        Commands::Welcome => {
            let shepherd = status::ShepherdStatus::probe(&paths).await;
            let code = welcome::welcome(&mut streams, &paths.home);
            // After the text, not before: the welcome ends on "shep welcome
            // shows this again", and the status is the one line that changes
            // between runs.
            if fmt == Format::Table && std::io::stderr().is_terminal() {
                let _ = writeln!(streams.err, "{}", status::one_line(&shepherd));
            }
            code
        }
        Commands::Style(args) => match args.level {
            // Deliberately re-resolves rather than reading `style` (this
            // function's own parameter): that value is already forced to
            // `Bare` under the hard rule (piped stdout, `--format json`),
            // and this report's whole job is telling an operator what is
            // configured -- reporting `bare` every time it was piped
            // would hide the answer `shep style` exists to give. See
            // `resolve_style`.
            None => {
                let (level, source) = resolve_style(&cli.global);
                let message = format!("{level} (from {source})");
                streams.note("style", &message);
                ExitCode::Success
            }
            // A level turns this from a report into a write. Config
            // first, report second -- the same order `commands::dogs`
            // uses for its own four verbs -- so a failed report can never
            // claim a write that did not happen.
            Some(level) => {
                // `try_edit`, not `edit`: the closure itself can refuse
                // (`set_style_level`'s own `Result`, when `style` is
                // already there as something other than a table), and
                // that refusal must never reach `ShepToml::save` -- `edit`
                // always saves after its closure runs regardless of what
                // the closure returned, which would rewrite (new inode,
                // mode forced to `CONFIG_FILE_MODE`) a file this call is
                // reporting as untouched.
                // `result_large_err` on the closure, for the same reason and
                // on the same platform as the module-wide allow in
                // `commands::shep_toml` — see the banner there. The error
                // type is that module's; this is one call site of it.
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
                // Re-resolves for the same reason the no-arg branch does:
                // this is what tells the operator whether the value just
                // written is what will actually run, or whether
                // `--style`/`$SHEP_STYLE` still outranks the `shep.toml`
                // this call just edited -- the exact "edited shep.toml
                // and saw nothing change" confusion `StyleSource`'s own
                // doc says this command exists to prevent.
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
        // Bare `shep start` means the Flockfile in this directory, the way
        // `shep runtime` and `shep dev` already read one -- and when there is
        // none, it means "bring a shepherd up", which is the only way to get
        // one without also starting a process.
        Commands::Start(ref args) => {
            let discovered = if args.targets.is_empty() {
                std::env::current_dir()
                    .ok()
                    .and_then(|cwd| shep_core::config::flockfile::discover(&cwd))
            } else {
                None
            };
            if args.targets.is_empty() && discovered.is_none() {
                return start_bare_shepherd(&mut streams, &paths, guard).await;
            }
            let shep_toml_text = std::fs::read_to_string(&paths.daemon_config).ok();
            let interpreters = interpreters_from_config(shep_toml_text.as_deref());
            match connect_or_spawn_client(&mut streams, &paths, guard).await {
                Ok(client) => {
                    lifecycle::start(
                        &client,
                        &mut streams,
                        args,
                        discovered.as_deref(),
                        &interpreters,
                    )
                    .await
                }
                Err(code) => code,
            }
        }
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
        // Falls back to the muster roll rather than refusing: looking at
        // the flock must not be a dead end on a machine that has just
        // rebooted, which is exactly where someone most needs to look.
        // The roll fallback is a table-format affordance only. Under
        // `--format json` a failed invocation must leave stdout empty and put
        // an error envelope on stderr -- `exit_codes_and_stream_discipline`
        // enforces that, and it is right to: a consumer that asked for
        // machine output should not have to tell a real listing apart from a
        // consolation prize. So JSON keeps the refusal, humans get the roll.
        //
        // Its own `Client::connect`, not `connect_client`, because that
        // helper reports and gives up and this arm has a fallback to reach.
        // The version guard therefore has to be applied by hand here — the
        // one dispatch arm where it is not inherited from the helper.
        //
        // Split into `flock_command` so a test can drive it directly against
        // a real fixture socket without going through `run`'s own argv
        // parsing (Task 5's own note: `run`'s dispatch arms were otherwise
        // untested, held only by the compiler).
        Commands::Flock => flock_command(&mut streams, &paths, guard).await,
        // The guard arm is what makes `--available` work with no shepherd
        // running at all: it never reaches `connect_client`, so a
        // community-index listing does not fail on a `$SHEP_HOME` where no
        // daemon was ever started.
        Commands::Dogs(ref args) if args.available => {
            query::available_dogs(&mut streams, args).await
        }
        Commands::Dogs(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => query::dogs(&client, &mut streams, args).await,
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
        // verb here has, and the field mapping this alias needs (`--exec`'s
        // path becomes `AdoptArgs::path`, `enable`'s own positional `name`
        // becomes `AdoptArgs::name`) lives at the one seam that has to know
        // about it.
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
        // Not `connect_client`: a verb whose whole job is reporting whether
        // a shepherd answers must not fail because the answer is "no". It
        // probes and reports, keeping `DaemonUnreachable` as the exit code so
        // `shep ping && echo up` still works.
        Commands::Ping => {
            let status = status::ShepherdStatus::probe(&paths).await;
            status::render_ping(&mut streams, &status)
        }
        // `connect_client`, not `connect_or_spawn_client`: saving the roll
        // of a daemon that is not running is not a thing, and autostarting
        // one to save an empty flock would overwrite a good roll with an
        // empty one.
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
        // The one verb with two targets, and the only arm that can finish
        // without a client: `--daemon` empties files this binary created and
        // the daemon merely inherited (`launch::launch_command`), so there is
        // nothing to ask the socket. Not connecting is the feature rather
        // than an optimisation — a wedged or stopped shepherd is exactly when
        // an operator reaches for this, and `connect_client` never autostarts
        // one to be told to do nothing.
        Commands::Flush(ref args) if args.daemon => logs::flush_daemon(&mut streams, &paths),
        Commands::Flush(ref args) => match connect_client(&mut streams, &paths, guard).await {
            Ok(client) => logs::flush(&client, &mut streams, args).await,
            Err(code) => code,
        },
        // Reads a file and writes nothing; starts nothing, so — like
        // `--daemon`'s own arm just above — there is nothing to ask the
        // socket. The history is on disk precisely so it survives the
        // shepherd (`commands::dogs`' own module doc on this verb).
        Commands::Barks(ref args) => dogs::barks(&mut streams, &paths, args),
        // Reads and writes `kv.json` directly and never connects to the
        // shepherd — `commands::kv`'s own module doc gives the reasoning,
        // shared with `Barks` just above.
        Commands::Set(ref args) => kv::set(&mut streams, &paths, args),
        Commands::Get(ref args) => kv::get(&mut streams, &paths, args),
        Commands::Unset(ref args) => kv::unset(&mut streams, &paths, args),
        // The one verb that does its own connecting. `connect_client`'s
        // contract is to report and give up, and giving up is what left an
        // operator with a live daemon nothing could stop, so `kill` takes
        // the paths and decides for itself — see `commands::admin`'s module
        // doc.
        Commands::Kill => admin::kill(&paths, &mut streams).await,
        Commands::Init(ref args) => init::init(&mut streams, args).await,
        // Reads a file and writes a file; starts nothing, so there is
        // nothing to ask the socket. `logs::flush_daemon` is the other arm
        // that finishes without a client.
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
/// `pub(crate)`: `commands::daemon`'s reload starts the successor it just
/// stopped through this same autostart, rather than growing a second one.
pub(crate) async fn connect_or_spawn_client(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: VersionGuard,
) -> Result<Client, ExitCode> {
    let launch_paths = paths.clone();
    match connect_or_spawn(&paths.socket, move || launch_daemon(&launch_paths)).await {
        // The guard applies to a shepherd this call CONNECTED to as much as
        // to one it started: a daemon it just spawned is this same binary
        // and can never skew, and one that was already up is exactly the
        // case the guard exists for.
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

/// `shep start` with no target and no Flockfile in sight: bring a shepherd up
/// and stop there.
///
/// The only way to get a shepherd without also starting a process. Every
/// other route either needs a target (`start <script>`) or a saved roll
/// (`muster`), which left a fresh machine with no way to reach a running
/// shepherd at all -- and 37 of 39 verbs need one.
///
/// Reports rather than re-boots when one is already up, because typing
/// `shep start` twice should say what happened, not silently do nothing.
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
            // Asked after the boot, not before: bringing the shepherd up
            // restores the muster roll, so a flock that looked empty a
            // moment ago may have members now -- and saying "nothing
            // running yet" over a listed sheep is how this message would
            // start lying the moment membership began surviving a restart.
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
/// `kill` and `daemon reload` are how an operator gets OUT of a skew;
/// `ping` is how they see what is running without being refused. A guard
/// whose remedy is itself guarded is the trap this whole check exists to
/// remove — the incident behind it left a live daemon, a live flock, and no
/// command in shep able to touch either. So a verb belongs on this list only
/// if it is a way out of that state, never merely one that is inconvenient
/// to lose.
///
/// `shep daemon reload` is the command [`refuse_version_skew`] names, so the
/// two are read out of this one list rather than spelled twice -- see
/// [`VERSION_SKEW_REMEDY`].
const RECOVERY_VERBS: [&str; 3] = ["kill", "daemon reload", "ping"];

/// Which [`RECOVERY_VERBS`] entry `command` is, or `None` for an ordinary
/// verb the version guard applies to.
///
/// Returns the name rather than a bool so a test can hold this mapping and
/// that documented list against each other, which is what keeps a verb from
/// becoming exempt without its reason being written down.
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
/// `pub(crate)`: Task 5b's three verbs (`lookout`, `whistle`, `foreground`)
/// bypass the seams in this file that would otherwise apply this for them,
/// by calling `Client::connect` inside their own module — so each names
/// this directly, always as [`Self::Enforce`], at its own connect site.
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
/// Held as its rendered lines rather than as one string, because the JSON
/// form joins them with a space and the table form with a newline; one
/// constant means the two renderings cannot drift into saying different
/// things.
const VERSION_SKEW_CAUSE: [&str; 2] = [
    "`cargo install shep` replaced the binary. It did not restart the",
    "shepherd, which is still running the old code.",
];

/// The one command that fixes a skew.
///
/// Read out of [`RECOVERY_VERBS`] rather than spelled again, because the two
/// have to agree: a remedy the guard itself refused is exactly the trap this
/// design exists to remove. Dropping `daemon reload` from that list changes
/// what this refusal prints, and the test pinning the wording catches it.
const VERSION_SKEW_REMEDY: &str = RECOVERY_VERBS[1];

/// The imperative sentence pointing at [`VERSION_SKEW_REMEDY`], shared by
/// The imperative naming the remedy, in the two shapes the formats need.
///
/// Deliberately two strings rather than one shared sentence. A `--format
/// json` consumer gets a single-line message and needs the command inside
/// it; the table form has layout, and printing the same sentence there
/// would name the command twice -- once in prose and once on the indented
/// line below it, which reads worse than the bare line it was meant to fix.
///
/// What must not drift is the COMMAND, and it cannot: both interpolate
/// [`VERSION_SKEW_REMEDY`], itself read out of [`RECOVERY_VERBS`] rather
/// than written twice. The framing around it differing per format is the
/// point, not a leak.
///
/// The table label carries no blank line after it, unlike every other gap
/// in this block. An operator hit this refusal in production and was not
/// certain the trailing indented line was a command to run at all, so the
/// label sits directly on top of it -- the association is the fix.
fn version_skew_instruction(fmt: Format) -> String {
    match fmt {
        Format::Json => format!("Run `shep {VERSION_SKEW_REMEDY}`."),
        Format::Table => format!("Run:\n  shep {VERSION_SKEW_REMEDY}"),
    }
}

/// Refuses a shepherd whose crate version differs from this binary's.
///
/// Any difference, not only a protocol difference. A protocol-only check
/// misses the case the incident actually hit: `cargo install shep` replaces
/// the binary and leaves the shepherd running the old code, and the two can
/// agree on every byte of the wire while disagreeing about what a verb does.
/// So the comparison is [`shep_core::protocol::HelloAck::daemon_version`]
/// against this crate's own `CARGO_PKG_VERSION`, on a handshake that
/// SUCCEEDED. Nothing here crosses the wire that did not already.
///
/// The message names the fix rather than the condition, because naming the
/// condition is what left an operator stuck: every verb refused, and no
/// sentence anywhere saying that reloading the shepherd was the way out.
///
/// `pub(crate)`, for the three sites Task 5b guards directly — see
/// [`VersionGuard`]'s own doc.
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
        // Written straight to the stream rather than through
        // [`Streams::fail`], which routes into `output::emit_error` and so
        // through `terminal_safe::sanitise` — and that collapses every `\n`
        // to a space. Right for daemon-supplied text, wrong for this fixed
        // block: the remedy has to sit on a line of its own to be seen and
        // copied. The `error[code]: message` first line is emit_error's own
        // shape, kept identical so this refusal reads like every other one.
        Format::Table => {
            let cause = VERSION_SKEW_CAUSE.join("\n");
            // `summary` interpolates `daemon_version`, which arrives over the
            // socket and is therefore attacker-shaped: a daemon this client
            // did not build can put a newline or an escape sequence in it and
            // forge lines on the operator's terminal. `emit_error` sanitises
            // for exactly this reason and this branch bypasses it to keep the
            // multi-line layout, so it sanitises the interpolated value
            // itself. The fixed text around it is ours and needs nothing.
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

/// Connects to the daemon at `paths.socket`. Never autostarts — see
/// [`run`]'s own doc for why that matters.
///
/// `guard` is this invocation's [`VersionGuard`]: a connected shepherd of a
/// different version is refused here, at the one seam every verb that needs
/// a [`Client`] passes through, so no verb has to remember to ask.
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
/// directly against a real fixture socket (Task 5's own note: `run`'s
/// dispatch arms were otherwise untested, held only by the compiler).
///
/// Uses its own `Client::connect`, not [`connect_client`], because that
/// helper reports and gives up and this arm has a roll fallback to reach.
/// The version guard therefore has to be applied by hand here — the one
/// dispatch arm where it is not inherited from the helper.
///
/// # A refusal is not an absence (spec G4, Task 6)
///
/// The roll fallback below is for a genuine absence only —
/// [`shep_client::ConnectError::Connect`], `connect(2)` itself failing
/// because nothing is listening. Every other [`shep_client::ConnectError`]
/// variant means a connection WAS established: the shepherd is there, and
/// either refused the handshake outright ([`shep_client::ConnectError::
/// ProtocolMismatch`]) or something went wrong reaching it cleanly after
/// connecting ([`shep_client::ConnectError::Io`],
/// [`shep_client::ConnectError::Wire`],
/// [`shep_client::ConnectError::HandshakeClosed`],
/// [`shep_client::ConnectError::HandshakeTimeout`] — that variant's own doc
/// says outright "something is bound but not answering"). None of those
/// five is an absence, so a blanket `Err(_) => flock_from_roll(..)`
/// reported all of them as "no shepherd running". That is what sent the
/// incident's operator to the muster-roll path instead of `shep daemon
/// reload`, while the shepherd was alive and answering the refusal.
async fn flock_command(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    guard: VersionGuard,
) -> ExitCode {
    match Client::connect(&paths.socket).await {
        Ok(client) => match refuse_version_skew(streams, &client, guard) {
            Ok(()) => query::flock(&client, streams).await,
            // A skew is not an absence either: the shepherd answered, so
            // the roll fallback below would print a listing while hiding
            // the reason every other verb is refusing.
            Err(code) => code,
        },
        // The roll fallback is a table-format affordance only. Under
        // `--format json` a failed invocation must leave stdout empty and
        // put an error envelope on stderr -- `exit_codes_and_stream_
        // discipline` enforces that, and it is right to: a consumer that
        // asked for machine output should not have to tell a real listing
        // apart from a consolation prize. So JSON always reports through
        // `connect_client`, absence included; humans get the roll below,
        // absence only.
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

/// Renders `err` for [`flock_command`]'s refusal arm: the shepherd IS
/// there, so this reports what it did and names the fix rather than the
/// roll's "no shepherd running", which would be the opposite of the truth.
fn flock_connect_refusal_message(err: &shep_client::ConnectError) -> String {
    format!("{err}; run `shep {VERSION_SKEW_REMEDY}`")
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

    /// fails if `home_before` stops reading `--home value` -- the common
    /// form, and the one every existing invocation in this file's own
    /// argv-construction tests already uses.
    #[cfg(unix)]
    #[test]
    fn home_before_reads_a_separate_value_argument() {
        let prefix = [OsString::from("--home"), OsString::from("/tmp/x")];
        assert_eq!(home_before(&prefix), Some(PathBuf::from("/tmp/x")));
    }

    /// fails if `home_before` stops reading the `--home=value` form clap
    /// itself also accepts.
    #[cfg(unix)]
    #[test]
    fn home_before_reads_an_equals_form() {
        let prefix = [OsString::from("--home=/tmp/y")];
        assert_eq!(home_before(&prefix), Some(PathBuf::from("/tmp/y")));
    }

    /// fails if `home_before` only ever checks the first token instead of
    /// scanning the whole prefix -- `--home` can follow other global flags
    /// in a real invocation (`shep --format json --home /tmp/z mydog`).
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

    /// fails if `dog_exit_code` stops reading a normal exit's own code.
    #[cfg(unix)]
    #[test]
    fn dog_exit_code_reads_a_normal_exit_status() {
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::ExitStatus::from_raw(7 << 8);
        assert_eq!(dog_exit_code(status), 7);
    }

    /// fails if `dog_exit_code` stops applying the `128 + signal` shell
    /// convention `commands::reap::classify` already reads a reaped
    /// supervisor's status by.
    #[cfg(unix)]
    #[test]
    fn dog_exit_code_reads_128_plus_signal_for_a_signalled_status() {
        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::ExitStatus::from_raw(9); // SIGKILL, no WIFEXITED bit
        assert_eq!(dog_exit_code(status), 128 + 9);
    }

    /// fails if `dispatch_adopted_dog` starts trying to look up a dog for
    /// any parse failure at all, not only an unrecognized-subcommand one --
    /// a missing required argument (`shep adopt` with no path) must reach
    /// clap's own usage error exactly as it always has.
    ///
    /// Does not, on its own, mutation-cover the explicit `err.kind()`
    /// check inside `dispatch_adopted_dog`: a `MissingRequiredArgument`
    /// carries no `ContextKind::InvalidSubcommand` value either, so the
    /// context-match just below that check already returns `None` here
    /// with or without it. That check's own doc names the narrower case
    /// (an `ArgumentConflict` clap does not produce anywhere in this
    /// binary's own command tree) it guards against instead.
    #[cfg(unix)]
    #[test]
    fn dispatch_adopted_dog_is_none_for_a_parse_error_that_is_not_invalid_subcommand() {
        let argv: Vec<OsString> = ["shep", "adopt"].into_iter().map(OsString::from).collect();
        let err = Cli::try_parse_from(&argv).unwrap_err();
        assert_ne!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
        assert!(dispatch_adopted_dog(&argv, &err).is_none());
    }

    /// fails if `dispatch_adopted_dog` invents a dog for a name
    /// `shep.toml` has never heard of -- this must fall through to clap's
    /// own unknown-verb error (suggestions included), not a silent,
    /// unrelated failure.
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

    /// fails if `dispatch_adopted_dog`'s lookup creates anything on a
    /// plain "no such dog" answer. CodeRabbit's finding on PR #4, verified
    /// against the real code before fixing: `ShepToml::edit` calls
    /// `open_locked`, which calls `create_home_dir` unconditionally, and
    /// `edit` itself calls `doc.save()` unconditionally even when its
    /// closure only read. Routing this lookup through `edit` meant a
    /// plain typo like `shep flcok`, on a machine with no `$SHEP_HOME`
    /// yet, created the directory and wrote an empty `shep.toml` as a
    /// side effect of failing to find a dog. A read that fails should
    /// leave the filesystem exactly as it found it.
    ///
    /// `home` here is never pre-created, unlike this file's other two
    /// `dispatch_adopted_dog` tests just above and below -- the point of
    /// this one is exactly that absence.
    ///
    /// Mutation check: routing the lookup in `dispatch_adopted_dog` back
    /// through `ShepToml::edit` reddens this -- both `home` and
    /// `home.join("shep.toml")` come to exist.
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

    /// fails if `dispatch_adopted_dog` stops finding a dog `shep.toml`
    /// really does have registered, once clap has already failed to parse
    /// its name as a subcommand -- the fast-tier half of issue 3's
    /// dispatch; `cli_e2e.rs`'s own case drives the real compiled binary
    /// end to end and pins the argv/`SHEP_HOME` contract this test does
    /// not reach (`std::process::ExitCode` carries no way to inspect the
    /// value it wraps, so this only asserts that a real spawn-and-wait
    /// happened, not which code it returned).
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

    /// fails if `shep <name>` runs a dog under a different contract than the
    /// shepherd does. Both channels promise the same two variables
    /// (`shep_daemon::dogs::dog_app`), and this is the half an operator
    /// drives by hand -- a dog invoked here to print or check its own
    /// configuration would otherwise read a different `[dog.<name>]` section
    /// than the one it runs under, which is the whole failure
    /// `SHEP_DOG_NAME` exists to end.
    ///
    /// The name asserted is the token the operator typed, never the script's
    /// file stem: `mydog.sh` is adopted here as `telemetry`, and the key is
    /// the name.
    ///
    /// No race to wait out: `run_adopted_dog` uses `Command::status`, which
    /// does not return until the child has exited, so the file it writes is
    /// complete by the time this reads it.
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
                // Still passed through untouched: the name arrives beside
                // the operator's arguments, never in place of them.
                "koji",
            ]
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
            Commands::Dogs(_)
        ));
    }

    /// Pins `--available` and its filter together, so the flag is not
    /// merely present but carries the right value through to the arg
    /// struct `main`'s dispatch reads.
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
    /// `adopt` needs only its one positional, `path` — `name` is an
    /// optional `--name` flag, matching `shep start <script>`'s own
    /// optional `--name`. `rehome` shares `DogArgs` with `disable`, so it
    /// needs only the name.
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

    /// fails if `enable --exec` routes to `enable` (which would try to run
    /// a built-in dog named after a path), and fails if `--exec`'s value
    /// and `enable`'s own positional `name` land in the wrong fields once
    /// `main`'s dispatch turns them into an `AdoptArgs`. Both are strings,
    /// so a swap here is silent — nothing but a pinned assertion on which
    /// field holds which value would catch one crossing the other.
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
            run(cli, style::Presentation::BARE).await,
            ExitCode::Usage,
            "startup must refuse a $SHEP_HOME that is not there"
        );

        let cli = Cli::try_parse_from(["shep", "--home", missing, "unstartup"]).unwrap();
        assert_ne!(
            run(cli, style::Presentation::BARE).await,
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
            style: None,
        };
        let paths = resolve_paths(&global).unwrap();
        assert_eq!(paths.home, std::path::Path::new("/tmp/explicit"));
        // The control address is the one field that is a different KIND of
        // thing per platform — a socket file on unix, a named-pipe name on
        // Windows. `ShepPaths::socket`'s own doc carries the argument; this
        // asserts that `--home` reaches that derivation on both, rather than
        // skipping the check on the platform where it changed.
        #[cfg(unix)]
        assert_eq!(
            paths.socket,
            std::path::Path::new("/tmp/explicit/run/shep.sock")
        );
        #[cfg(windows)]
        assert_eq!(paths.socket, std::path::Path::new(&paths.pipe_name()));
    }

    /// fails if the hard rule's condition drifts from "piped, or JSON" —
    /// either a missing OR that only forces on one of the two, or an
    /// inverted terminal check, would leave a border or an escape reaching
    /// piped stdout.
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

    /// fails if `style_from_config` stops reading a real `[style] level`, or
    /// starts treating a missing file, a broken file, an absent `[style]`
    /// table, or an unrecognised level name as anything but "the config did
    /// not answer" — matching `whistle::gate::resolve_control`'s reading of
    /// a broken `shep.toml` as "no" rather than an error.
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

    /// Pins that `style_from_config` and `resolve_style` agree on
    /// `$SHEP_STYLE` whitespace, padding and all -- see [`style::StyleLevel`]'s
    /// own doc for why both must go through `StyleLevel::parse` rather than
    /// `clap::ValueEnum::from_str`.
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

    /// fails if `resolve_style` stops honouring `--style`, ignores
    /// `$SHEP_HOME`'s `shep.toml` in favour of the wrong file, or reorders
    /// the flag-over-config precedence `style::resolve` already owns —
    /// this test is about `resolve_style` actually wiring the flag and a
    /// real file into that function, not about the precedence rule itself
    /// (pinned directly in `style.rs`).
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

    /// fails if the override warning stops firing for the two layers that
    /// actually outrank `shep.toml` (`--style`, `$SHEP_STYLE`), or starts
    /// firing for `Config` (the value a write just produced) or `Default`
    /// (impossible right after a successful write) -- either direction
    /// would leave `Commands::Style`'s set form either silent about a
    /// write that will not take effect, or crying wolf about one that
    /// will.
    #[test]
    fn style_write_is_overridden_only_by_flag_or_env() {
        assert!(style_write_is_overridden(style::StyleSource::Flag));
        assert!(style_write_is_overridden(style::StyleSource::Env));
        assert!(!style_write_is_overridden(style::StyleSource::Config));
        assert!(!style_write_is_overridden(style::StyleSource::Default));
    }

    /// fails if `shep style <level>` stops actually writing `shep.toml` --
    /// the defect this task exists to fix -- for any of the three levels,
    /// or if what it writes is not what `style_from_config` (the same
    /// reader `resolve_style` uses) reads back.
    ///
    /// `#[cfg(unix)]` because every verb refuses on Windows with "shep does
    /// not yet support Windows", so there is no write to observe there. This
    /// gate was missing when the test landed and turned CI's two Windows legs
    /// red; the local gate could not see it, because a macOS `cargo test`
    /// never compiles a Windows arm and the windows-gnu cross-check is
    /// `cargo check`, which does not run anything.
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
                run(cli, style::Presentation::BARE).await,
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

    /// fails if `shep style` with no level ever writes `shep.toml` -- the
    /// no-arg form is a report, and only a report, the same guarantee
    /// `resolve_style_reads_the_flag_and_the_real_shep_toml_it_names`
    /// above pins for what it *reads*.
    /// `#[cfg(unix)]` for the same reason as the test above: the verb
    /// refuses on Windows before it can report anything.
    #[cfg(unix)]
    #[tokio::test]
    async fn style_with_no_level_reports_and_writes_nothing() {
        use clap::Parser;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let cli = Cli::try_parse_from(["shep", "--home", home, "style"]).unwrap();
        assert_eq!(run(cli, style::Presentation::BARE).await, ExitCode::Success);

        assert!(
            !dir.path().join("shep.toml").exists(),
            "the no-arg form must not create a shep.toml that was not there"
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
        assert_eq!(run(cli, style::Presentation::BARE).await, ExitCode::Success);
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

    /// A [`Streams`] over two byte buffers, so a refusal's exact text can be
    /// read back. `BARE`/`Table` because these tests assert on words, not on
    /// colour.
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

    /// The case a protocol-only check misses entirely, and the one the
    /// incident actually hit: the wire versions agree, the crate versions do
    /// not, and `cargo install shep` left a new binary talking to an old
    /// shepherd.
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

    /// fails if the refusal stops naming the fix. A message that names only
    /// the condition is what this guard exists to replace: the operator whose
    /// box this bricked could read every word of "protocol mismatch" and
    /// still not know that reloading the shepherd was the way out.
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

    /// The table form used to print the remedy as a bare indented line with
    /// no imperative anywhere near it, so an operator who hit this in
    /// production was not certain the last line was a command to run. This
    /// pins the sentence that now points at it, and that the sentence sits
    /// on its own line ABOVE the copyable command rather than folded into
    /// it -- the indentation is what makes the line copyable and stays
    /// untouched.
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

        // The label sits DIRECTLY on the command, no blank line between:
        // that adjacency is the whole fix, and a gap here is the shape an
        // operator read as two unrelated things.
        assert!(
            text.contains("Run:\n  shep daemon reload"),
            "the label must sit directly on the copyable line it points at: {text}"
        );
        // Named once, not twice. An earlier attempt printed the sentence
        // "Run `shep daemon reload`." above the indented line, which reads
        // worse than the bare line it was meant to fix.
        assert_eq!(
            text.matches("shep daemon reload").count(),
            1,
            "the remedy is named once, not restated in prose: {text}"
        );
    }

    /// Task 6 / spec G4: `lib.rs`'s old blanket `Err(_) =>
    /// query::flock_from_roll(..)` treated every connect failure as an
    /// absence, so a daemon that answered the handshake and refused it
    /// (the incident case) was reported as "no shepherd running" -- sending
    /// the operator to the roll instead of `shep daemon reload`.
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

    /// The other half of Task 6: a genuinely absent daemon -- nothing
    /// listening at all -- must still fall back to the muster roll. That
    /// fallback is a real feature (a machine that just rebooted), not the
    /// bug; narrowing the match must not remove it.
    #[tokio::test]
    async fn flock_still_falls_back_to_the_roll_for_a_genuine_absence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        std::fs::create_dir_all(&paths.run).unwrap();
        // No socket bound at `paths.socket` -- `connect(2)` itself fails.

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

    /// A daemon of this binary's own version is not a skew, so nothing is
    /// written and nothing is refused.
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

    /// A guard whose remedy is itself guarded is the trap this design exists
    /// to remove: an exempt verb reaches a skewed daemon and says nothing.
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

    /// fails if a verb leaves [`RECOVERY_VERBS`], or if one arrives without
    /// being listed there with its reason.
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
    }

    /// Nothing else drives [`run`]'s dispatch arms, so the wiring behind each
    /// verb is held only by the compiler — and `kill`'s arm is the one that
    /// changed most recently, from a shared `connect_client` to its own
    /// connect. This drives the arm end to end against a home no shepherd
    /// owns and asserts the code that arm's own path produces.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_dispatches_kill_to_the_socket_free_path() {
        use clap::Parser;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        // The two directories a booted shepherd would have made: `pids/`
        // holds the lock `kill`'s socket-free path reads, and `run/` the
        // control socket it first tries to connect over.
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.to_string_lossy().into_owned()),
            std::path::Path::new("/nonexistent"),
        );
        std::fs::create_dir_all(&paths.pids).unwrap();
        std::fs::create_dir_all(&paths.run).unwrap();
        let argv = ["shep", "--home", home.to_str().unwrap(), "kill"];
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} failed: {e}"));

        assert_eq!(
            run(cli, style::Presentation::BARE).await,
            ExitCode::DaemonUnreachable,
            "`kill` against an unowned home must reach its own socket-free path"
        );
    }
}
