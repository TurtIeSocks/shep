//! The clap command tree: [`Cli`], [`Commands`], and every argument struct.
//!
//! This is the whole parse surface of the `shep` binary in one place, pure
//! tier (spec §11): it compiles and its tests run on every target, Windows
//! included, so `Cli::command().debug_assert()` and the alias tests below
//! cover a platform that cannot build the rest of this crate.
//!
//! This module owns every argument struct in the tree, even the ones whose
//! command is not wired up yet — the whole parse surface lives in one
//! portable file rather than accreting piecemeal as each verb lands.

use std::path::PathBuf;

/// The `shep` command line.
#[derive(Debug, clap::Parser)]
#[command(name = "shep", version, about = "A process manager for your flock")]
pub struct Cli {
    /// Flags valid on every subcommand.
    #[command(flatten)]
    pub global: GlobalArgs,
    /// The verb being invoked.
    #[command(subcommand)]
    pub command: Commands,
}

/// Flags valid on every subcommand, folded into [`Cli`] via `#[command(flatten)]`.
#[derive(Debug, clap::Args)]
pub struct GlobalArgs {
    /// Override $SHEP_HOME for this invocation
    #[arg(long, global = true, env = "SHEP_HOME")]
    pub home: Option<PathBuf>,
    /// Output format
    #[arg(long, global = true, value_enum, default_value_t = Format::Table)]
    pub format: Format,
    /// Suppress non-essential output
    ///
    /// Currently narrows `bleats`' own notices (a dropped-events count, a
    /// daemon-shutdown notice, ...) — diagnostics distinct from a sheep's
    /// own line or a real error, both of which still print regardless.
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

/// `--format`'s two shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Human-readable columns (the default).
    Table,
    /// A versioned JSON envelope, one object per invocation.
    Json,
}

/// Every verb the binary understands.
#[derive(Debug, clap::Subcommand)]
pub enum Commands {
    /// Start a sheep from a script, a Flockfile, or stdin.
    Start(StartArgs),
    /// Stop one or more sheep.
    Stop(SelectorArgs),
    /// Restart one or more sheep.
    Restart(SelectorArgs),
    /// Delete one or more sheep from the flock.
    Delete(SelectorArgs),
    /// List the flock.
    #[command(visible_aliases = ["list", "ls"])]
    Flock,
    /// Describe one sheep in detail.
    Describe(SelectorArgs),
    /// List one fold.
    Fold(FoldArgs),
    /// Show or follow bleats (log output) for one or more sheep.
    #[command(visible_alias = "logs")]
    Bleats(BleatsArgs),
    /// Reopen log files after an external rotator has renamed them.
    Reopen(ReopenArgs),
    /// Empty the log files of one or more sheep, or the shepherd's own.
    Flush(FlushArgs),
    /// Check whether the shepherd answers.
    Ping,
    /// Shut the shepherd down.
    Kill,
    /// Print a shell completion script.
    ///
    /// Static only: sheep names, fold names and other daemon-side
    /// identifiers are never completed.
    Completions(CompletionArgs),
    /// Graceful stop. Easter-egg alias for `stop`.
    #[command(hide = true)]
    Thatlldo(SelectorArgs),
    /// Run the supervisor in the foreground. Spawned by the CLI; not for direct use.
    #[command(hide = true)]
    Daemon(DaemonArgs),
}

/// Arguments to `shep start`.
#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// A script path, a Flockfile, or `-` to read Flockfile JSON from stdin
    pub target: String,
    /// Name for this sheep (script form only)
    #[arg(long)]
    pub name: Option<String>,
    /// Fold to place this sheep in
    #[arg(long)]
    pub fold: Option<String>,
}

/// Arguments shared by every verb that targets an existing selection of the
/// flock (`stop`, `restart`, `delete`, `describe`, `thatlldo`).
///
/// The selector is required on every one of them, because every one of them
/// acts on something. `flush` has the same rule and its own struct
/// ([`FlushArgs`]) only because it has a second target that is not a
/// selection at all.
#[derive(Debug, clap::Args)]
pub struct SelectorArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>`
    pub selector: String,
}

/// Arguments to `shep flush`.
///
/// # Why a flag and not a reserved selector name
///
/// The shepherd's own `shepd.out.log`/`shepd.err.log` are the second thing
/// this verb can empty, and they are NOT a sheep — nothing about them is
/// expressible as a selector. Spelling them `shep flush shep` would make one
/// name mean something different depending on the Flockfile, since nothing
/// stops an app being called `shep`, and an operator who named one that would
/// find `shep flush shep` quietly emptying the wrong files. A flag cannot
/// collide with anything.
///
/// # Why it replaces the selector rather than composing with it
///
/// `--daemon` conflicts with the selector, so `shep flush all --daemon` is a
/// usage error rather than "both". Three reasons, in order of weight: the two
/// halves answer with different shapes — sheep against files — and one
/// invocation renders one payload into one envelope; the daemon's own logs
/// are the one target Rin asked never to be reached without being named, and
/// a flag that rode along with `all` would be reached by every operator who
/// ever typed `shep flush all --daemon` out of habit; and the shepherd's logs
/// are not a sheep's, so folding them into a flock answer would mean
/// inventing a row for something with no id and no name.
///
/// The selector stays required in every other case — `required_unless_present`
/// rather than a `default_value`, so a bare `shep flush` is still the usage
/// error it has always been, never "empty every log in the flock".
#[derive(Debug, clap::Args)]
pub struct FlushArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>` (required unless --daemon)
    #[arg(required_unless_present = "daemon", conflicts_with = "daemon")]
    pub selector: Option<String>,
    /// Empty the shepherd's own logs instead of any sheep's
    #[arg(long)]
    pub daemon: bool,
}

/// Arguments to `shep fold`.
#[derive(Debug, clap::Args)]
pub struct FoldArgs {
    /// The fold to list
    pub name: String,
}

/// The selector the verbs that take an optional one fall back to.
///
/// One owner for the string, shared rather than spelled twice: `bleats` and
/// `reopen` default to the same thing on purpose, and a copy that drifted
/// would leave one of them quietly targeting something else.
const DEFAULT_SELECTOR: &str = "all";

/// Arguments to `shep bleats` (alias `logs`).
#[derive(Debug, clap::Args)]
pub struct BleatsArgs {
    /// Which sheep (default: all)
    #[arg(default_value = DEFAULT_SELECTOR)]
    pub selector: String,
    /// Print the tail of each sheep's log file and exit, instead of following
    #[arg(long)]
    pub no_follow: bool,
    /// Only stderr
    #[arg(long, conflicts_with = "out")]
    pub err: bool,
    /// Only stdout
    #[arg(long, conflicts_with = "err")]
    pub out: bool,
}

/// Arguments to `shep reopen`.
///
/// The selector is optional, defaulting to [`DEFAULT_SELECTOR`], where
/// `stop`/`restart`/`delete` all demand one: those destroy something, and
/// a reopen destroys nothing — it swaps a file handle for another handle on
/// the same path. Rotating every sheep at once is also the ordinary case, a
/// `postrotate` stanza having just renamed the whole log directory.
#[derive(Debug, clap::Args)]
pub struct ReopenArgs {
    /// Which sheep (default: all)
    #[arg(default_value = DEFAULT_SELECTOR)]
    pub selector: String,
}

/// Arguments to `shep completions`.
#[derive(Debug, clap::Args)]
pub struct CompletionArgs {
    /// Shell to generate a completion script for
    #[arg(value_enum)]
    pub shell: clap_complete::aot::Shell,
}

/// Arguments to the hidden `shep daemon` subcommand.
#[derive(Debug, clap::Args)]
pub struct DaemonArgs {
    /// Boot without restoring the saved muster roll
    #[arg(long)]
    pub no_restore: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_tree_parses_and_is_internally_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert(); // clap's own structural self-check
    }

    #[test]
    fn list_and_ls_both_reach_flock() {
        use clap::Parser;
        for argv in [["shep", "flock"], ["shep", "list"], ["shep", "ls"]] {
            assert!(matches!(
                Cli::try_parse_from(argv).unwrap().command,
                Commands::Flock
            ));
        }
    }

    #[test]
    fn logs_reaches_bleats() {
        use clap::Parser;
        assert!(matches!(
            Cli::try_parse_from(["shep", "logs"]).unwrap().command,
            Commands::Bleats(_)
        ));
    }

    /// Pins [`DEFAULT_SELECTOR`] on both verbs that carry it.
    ///
    /// Fails if either loses its `default_value`: the bare invocation
    /// becomes a clap usage error instead of targeting the flock, which for
    /// `reopen` is the whole reason a signal — which carries no selector —
    /// can mean this verb at all. Both halves matter: an explicit selector
    /// must still win, or the default would be a hardcoded `all` wearing a
    /// default's clothes.
    #[test]
    fn bleats_and_reopen_default_to_every_sheep() {
        use clap::Parser;
        let bare = Cli::try_parse_from(["shep", "reopen"]).unwrap().command;
        let Commands::Reopen(args) = bare else {
            panic!("`shep reopen` must parse with no selector")
        };
        assert_eq!(args.selector, "all");

        let bare = Cli::try_parse_from(["shep", "bleats"]).unwrap().command;
        let Commands::Bleats(args) = bare else {
            panic!("`shep bleats` must parse with no selector")
        };
        assert_eq!(args.selector, "all");

        let named = Cli::try_parse_from(["shep", "reopen", "web"])
            .unwrap()
            .command;
        let Commands::Reopen(args) = named else {
            panic!("expected reopen")
        };
        assert_eq!(args.selector, "web");
    }

    /// The other side of [`bleats_and_reopen_default_to_every_sheep`]: the
    /// log-plane verb that destroys data must NOT have a default.
    ///
    /// Fails if `flush` is ever given a `default_value`, or moved onto
    /// [`ReopenArgs`] — either of which turns a bare `shep flush`, the single
    /// most likely slip of the finger this CLI offers, from a usage error
    /// into "empty every log file in the flock" with nothing to undo it. The
    /// explicit form is asserted alongside, so a verb that rejected every
    /// selector could not pass the first half alone.
    ///
    /// `required_unless_present = "daemon"` is what keeps the first half true
    /// now that the selector is an `Option`: without it, a bare `shep flush`
    /// parses to `selector: None` and reaches the handler.
    #[test]
    fn flush_refuses_to_run_without_a_selector() {
        use clap::Parser;
        assert!(
            Cli::try_parse_from(["shep", "flush"]).is_err(),
            "`shep flush` with no selector must be a usage error, never the \
             whole flock"
        );

        let named = Cli::try_parse_from(["shep", "flush", "all"])
            .unwrap()
            .command;
        let Commands::Flush(args) = named else {
            panic!("expected flush")
        };
        assert_eq!(args.selector.as_deref(), Some("all"));
        assert!(
            !args.daemon,
            "a plain flush must not reach the shepherd's own logs"
        );
    }

    /// Fails if `--daemon` stops replacing the selector — either by gaining a
    /// selector of its own (the bare form stops parsing) or by losing
    /// `conflicts_with` (the combined form starts parsing).
    ///
    /// Both halves are the decision [`FlushArgs`]'s doc argues for. The bare
    /// form must work, because it is the only spelling of "empty the
    /// shepherd's own logs" and requiring a sheep selector alongside it would
    /// be nonsense. The combined form must NOT, because an operator typing
    /// `shep flush all --daemon` out of habit is exactly the accident that
    /// keeping the two targets apart exists to prevent.
    #[test]
    fn the_daemon_flag_replaces_the_selector_rather_than_riding_along_with_it() {
        use clap::Parser;
        let bare = Cli::try_parse_from(["shep", "flush", "--daemon"])
            .expect("`shep flush --daemon` is the only spelling there is")
            .command;
        let Commands::Flush(args) = bare else {
            panic!("expected flush")
        };
        assert!(args.daemon);
        assert_eq!(args.selector, None);

        assert!(
            Cli::try_parse_from(["shep", "flush", "all", "--daemon"]).is_err(),
            "the shepherd's own logs are a separate act, never a rider on a \
             flock-wide flush"
        );
    }

    #[test]
    fn format_defaults_to_table_and_accepts_json() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["shep", "flock"]).unwrap();
        assert_eq!(cli.global.format, Format::Table);
        let cli = Cli::try_parse_from(["shep", "--format", "json", "flock"]).unwrap();
        assert_eq!(cli.global.format, Format::Json);
    }

    /// `std::env::set_var` is `unsafe` in edition 2024 and this crate is
    /// `#![forbid(unsafe_code)]`, so nothing here can establish an ambient
    /// `$SHEP_HOME` and observe clap actually reading it. The next best
    /// thing, and the thing that actually matters for `$SHEP_HOME` to keep
    /// working, is pinning that clap was *told* to read it: if `env =
    /// "SHEP_HOME"` (`cli.rs:30`) is ever deleted, this fails.
    #[test]
    fn home_flag_is_wired_to_the_shep_home_env_var() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let home_arg = cmd
            .get_arguments()
            .find(|a| a.get_id().as_str() == "home")
            .expect("GlobalArgs::home must still be a flattened argument named `home`");
        assert_eq!(home_arg.get_env(), Some(std::ffi::OsStr::new("SHEP_HOME")));
    }

    /// Pins `Flock`'s and `Bleats`'s visible aliases, and that the hidden
    /// verbs (`thatlldo`, the internal `daemon` re-exec target) stay hidden
    /// from `--help`. A `visible_aliases`/`aliases` swap, or a dropped
    /// `hide = true`, passes every other test in this module but changes
    /// user-facing behavior silently.
    #[test]
    fn alias_visibility_and_hiding_are_pinned() {
        use clap::CommandFactory;
        let cmd = Cli::command();

        let flock = cmd.find_subcommand("flock").unwrap();
        assert_eq!(
            flock.get_visible_aliases().collect::<Vec<_>>(),
            ["list", "ls"]
        );

        let bleats = cmd.find_subcommand("bleats").unwrap();
        assert_eq!(bleats.get_visible_aliases().collect::<Vec<_>>(), ["logs"]);

        for hidden in ["thatlldo", "daemon"] {
            assert!(
                cmd.find_subcommand(hidden).unwrap().is_hide_set(),
                "{hidden} must stay hidden from --help"
            );
        }
        for visible in [
            "start",
            "flock",
            "bleats",
            "reopen",
            "flush",
            "ping",
            "kill",
            "completions",
        ] {
            assert!(
                !cmd.find_subcommand(visible).unwrap().is_hide_set(),
                "{visible} must stay visible in --help"
            );
        }
    }
}
