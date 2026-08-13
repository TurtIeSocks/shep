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
    /// Reload one or more sheep, one instance at a time.
    ///
    /// Each instance is replaced by a fresh one, which has to start and
    /// become ready before the instance it replaces is asked to go — so the
    /// app gets a window in which it can hand over without dropping work.
    ///
    /// An app that binds a port has to set SO_REUSEPORT itself, before it
    /// binds; shep binds nothing and cannot set it on the app's behalf.
    /// Without it the replacement fails to bind on every reload and the
    /// reload is abandoned with the old instance left serving. This command
    /// has already exited 0 by then, so process.reload_abandoned on the bus
    /// is the only report of it.
    ///
    /// That window is not zero downtime. The old listener's queue of
    /// connections it has not accepted yet is dropped when it closes, so an
    /// app that does not stop accepting and finish what it has in hand
    /// before graceful_timeout runs out loses whatever was waiting there.
    ///
    /// Exits as soon as the shepherd accepts the reload, printing the flock
    /// as it stood at that moment — a clustered app takes longer to swap
    /// than any answer can wait for. The swaps themselves are reported on
    /// the bus, under process.reload, process.reloaded and
    /// process.reload_abandoned.
    Reload(SelectorArgs),
    /// Delete one or more sheep from the flock.
    Delete(SelectorArgs),
    /// Set how many instances one app runs.
    ///
    /// An absolute count, not a change: `shep scale web 4` means web has four
    /// instances afterwards, whatever it had before. There is no +N/-N form —
    /// run it twice and get the same flock.
    ///
    /// Scaling up fills the lowest free instance slots; scaling down releases
    /// the highest, so scaling out and back returns the same slot numbers, the
    /// same SHEP_INSTANCE values and the same log files it started with.
    ///
    /// Exits as soon as the shepherd accepts, printing the instances that
    /// remain. On a scale-down the departing instances are still running their
    /// stop ladders at that point; they report themselves on the bus, under
    /// process.delete.
    ///
    /// The new count is written to the muster roll, so `shep save` and a
    /// reboot keep it.
    Scale(ScaleArgs),
    /// List the flock.
    #[command(visible_aliases = ["list", "ls"])]
    Flock,
    /// List the dogs, and nothing else.
    Dogs,
    /// Turn on a registered dog: writes `[daemon] enabled_dogs` in
    /// `shep.toml`, and starts it now if a shepherd is running.
    ///
    /// Writes the config either way and exits 0 even with no shepherd
    /// running — the dog comes up with the next one. `shep muster` is the
    /// only verb that autostarts a shepherd; this is not it.
    Enable(EnableArgs),
    /// Turn off a registered dog: removes it from `[daemon] enabled_dogs`,
    /// and stops it now if a shepherd is running.
    ///
    /// Leaves `[dog.<name>]` in place — the dog's own configuration
    /// survives a disable/enable cycle. `shep rehome` is the verb that
    /// forgets a dog entirely.
    Disable(DogArgs),
    /// Vet a binary shep has never seen and register it as a dog: writes
    /// `[daemon] adopted_dogs` and `[daemon] enabled_dogs` in `shep.toml`,
    /// and starts it now if a shepherd is running.
    ///
    /// Refuses, before touching the config at all, a path that does not
    /// exist, is not a file, has no execute bit set, or that this kernel
    /// will not exec. An adopted dog runs at the shepherd's own trust
    /// level, with no sandboxing beyond it.
    Adopt(AdoptArgs),
    /// Forget an adopted dog entirely: stops it if a shepherd is running,
    /// and removes it from `[daemon] enabled_dogs`, `[daemon]
    /// adopted_dogs`, and its own `[dog.<name>]` table.
    ///
    /// `shep disable` stops a dog without forgetting its configuration;
    /// `rehome` is the verb that forgets it.
    Rehome(DogArgs),
    /// Describe one sheep in detail.
    Describe(SelectorArgs),
    /// Send a named action to matched sheep and report what each app
    /// answers.
    ///
    /// Reaches an app over its shepherd channel — the fd-3 pipe the daemon
    /// opens when the app's Flockfile sets `channel = true`. `wait_ready`
    /// and `shutdown_with_message` both imply the same channel, so either
    /// one of the three is enough; a sheep with none of them answers a
    /// `no_channel` row instead of a reply, naming the same fields.
    ///
    /// `action` and any `params` are free-form and unvalidated here — sent
    /// to the app verbatim, on its own shepherd-channel wire, for the app
    /// itself to recognize or refuse.
    Trigger(TriggerArgs),
    /// Send a unix signal to matched sheep.
    ///
    /// Delivered to each sheep's own process, not to its process group — the
    /// lambs it forked are not signalled. This is a nudge to the application
    /// (SIGHUP to re-read config, SIGUSR1 to dump state); `shep stop` is what
    /// runs the stop ladder, and `shep reload` is what swaps instances.
    ///
    /// Accepted: SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SIGWINCH,
    /// SIGCONT, SIGKILL. The SIG prefix and the case are both optional.
    /// SIGSTOP is refused: a stopped sheep still reads online in every listing
    /// shep can produce.
    ///
    /// Delivery is not action. A signal the app blocks or ignores is reported
    /// delivered, because the kernel took it and there is nothing further shep
    /// can see.
    Signal(SignalArgs),
    /// List one fold.
    Fold(FoldArgs),
    /// Show or follow bleats (log output) for one or more sheep.
    #[command(visible_alias = "logs")]
    Bleats(BleatsArgs),
    /// Reopen log files after an external rotator has renamed them.
    Reopen(ReopenArgs),
    /// Empty the log files of one or more sheep, or the shepherd's own.
    Flush(FlushArgs),
    /// Show the alert history: `barks.jsonl`, newest last.
    ///
    /// Reads the file directly and never connects to the shepherd — the
    /// history is on disk precisely so it survives the shepherd, and the
    /// case this verb exists for is an operator reading it after a crash.
    /// Same precedent as `shep flush --daemon`, which also works on files
    /// rather than through the socket.
    Barks(BarksArgs),
    /// Check whether the shepherd answers.
    Ping,
    /// Shut the shepherd down.
    Kill,
    /// Write the muster roll now, so a reboot can bring this flock back.
    Save,
    /// Assemble the flock from the muster roll `save` wrote, starting the
    /// shepherd first if none is running.
    // Hidden alias `resurrect` (pm2's own word for this), so the muscle
    // memory carries over: `alias`, not `visible_aliases`, so it stays out
    // of `--help` rather than being taught by it. A plain `//` comment
    // rather than `///` on purpose — the paragraph above already becomes
    // this subcommand's own `--help` text, and naming the alias there would
    // defeat the point of keeping it hidden.
    #[command(alias = "resurrect")]
    Muster,
    /// Write a Flockfile from a pm2 dump. Starts nothing.
    ///
    /// Reads `--from`, or `~/.pm2/dump.pm2` if it names nothing — whichever
    /// `pm2 save` last wrote. Every clustered app is named on stderr: shep
    /// binds nothing, so N instances on one port need the app to set
    /// `SO_REUSEPORT` itself, or the second instance hits EADDRINUSE at
    /// start. Every env key the dump carried that was neither declared nor
    /// recognizable session junk is named on stderr too, and left out of
    /// the Flockfile, for the operator to decide.
    Import(ImportArgs),
    /// Install an init unit so the shepherd starts at boot.
    ///
    /// Writes a systemd unit (Linux) or a launchd plist (macOS) for the
    /// target user, carrying this binary's own path, that user's
    /// $SHEP_HOME, and the PATH of this invocation — which is what makes an
    /// interpreter installed under ~/.bun or ~/.cargo findable after a
    /// reboot.
    ///
    /// Needs root, and never asks for it: without it this prints the exact
    /// command to run and exits non-zero, so a script notices. Under sudo
    /// the unit is built for $SUDO_USER rather than root, so it supervises
    /// the flock the operator actually has.
    ///
    /// Under sudo this also warns that PATH may have been replaced by
    /// sudo's own secure_path before shep ever saw it, and shows the exact
    /// PATH about to go into the unit so you can check it yourself.
    Startup(StartupArgs),
    /// Disable and remove the unit `startup` installed.
    ///
    /// Needs root under the same rule: without it, prints the command to
    /// run and exits non-zero. A unit that is not there is reported absent
    /// rather than failing.
    Unstartup(StartupArgs),
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
    /// Run one built-in dog in the foreground. Spawned by the shepherd as
    /// `<this binary> dog <name>`; not for direct use.
    #[command(hide = true)]
    Dog(DogArgs),
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
/// flock (`stop`, `restart`, `reload`, `delete`, `describe`, `thatlldo`).
///
/// The selector is required on every one of them, because every one of them
/// acts on something. `flush` has the same rule and its own struct
/// ([`FlushArgs`]) only because it has a second target that is not a
/// selection at all.
///
/// Required means no `default_value` on the field below, and that one
/// attribute is the whole of it — adding one would turn a bare `shep stop`
/// into `shep stop all` for every verb in the list at once. It is pinned by
/// this module's own `a_selector_taking_verb_refuses_to_run_without_one`
/// (named rather than linked: that module is `#[cfg(test)]`, so an intra-doc
/// link to it does not resolve under `cargo doc`).
#[derive(Debug, clap::Args)]
pub struct SelectorArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>`
    pub selector: String,
}

/// Arguments to `shep scale`.
///
/// Not [`SelectorArgs`], and this is the only lifecycle verb that is not.
/// `instances` is a per-app number, so the target is an app NAME: no `all`,
/// no `/regex/`, no `fold:` — a selector matching two apps would have to mean
/// either four each or four in total, and neither reading is more obviously
/// right.
#[derive(Debug, clap::Args)]
pub struct ScaleArgs {
    /// The app's name
    pub name: String,
    /// How many instances it runs afterwards
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    pub count: u32,
}

/// Arguments to `shep trigger`.
///
/// Not [`SelectorArgs`]: this verb needs two more positionals than a
/// selector, `action` and the optional `params` after it, so it carries its
/// own struct rather than widening the one every other selector-taking verb
/// shares. The selector is still required — no `default_value`, matching
/// `stop`/`restart`/`reload`/`delete`/`describe` — for the same reason: this
/// reaches a running app, so the operator names the target rather than
/// trigger one against the whole flock by accident.
#[derive(Debug, clap::Args)]
pub struct TriggerArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>`
    pub selector: String,
    /// Action name — free-form, defined by the app
    pub action: String,
    /// Argument text for the action, passed through to the app verbatim
    pub params: Option<String>,
}

/// Arguments to `shep signal`.
///
/// Not [`SelectorArgs`]: this verb needs a second positional. The selector
/// stays required — no `default_value` — for the reason every
/// running-process verb's does: an accidental `shep signal` should be a usage
/// error, never a flock-wide SIGHUP.
#[derive(Debug, clap::Args)]
pub struct SignalArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>`
    pub selector: String,
    /// Signal name, e.g. `SIGHUP` or `hup`
    pub signal: String,
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

/// Arguments to `shep barks`.
///
/// No selector, and no `--daemon`-shaped flag either — `barks.jsonl` is one
/// file for the whole `$SHEP_HOME`, holding both the bark dog's own alerts
/// and the ones the shepherd wrote itself when an enabled dog exhausted its
/// restart budget, so there is no population within it to select a subset
/// of the way `flush` selects sheep.
#[derive(Debug, clap::Args)]
pub struct BarksArgs {
    /// Show only the last N barks
    #[arg(long)]
    pub tail: Option<usize>,
}

/// Arguments to `shep fold`.
#[derive(Debug, clap::Args)]
pub struct FoldArgs {
    /// The fold to list
    pub name: String,
}

/// Arguments to `shep disable`/`shep rehome`, and to the hidden `shep dog`
/// re-exec target.
///
/// One struct for all three, matching [`StartupArgs`]'s own precedent: a
/// dog is named, never selected — `SelectorArgs`' grammar (`all`, `/regex/`,
/// `fold:<name>`) answers "which of the flock", and a dog is not the flock.
/// `shep enable` shares this shape too, but carries a second, hidden field
/// ([`EnableArgs`]) that none of the three below has any use for, so it
/// gets a struct of its own rather than widening this one for verbs that
/// would never touch the extra field.
#[derive(Debug, clap::Args)]
pub struct DogArgs {
    /// The dog's name — the `[dog.<name>]` config key
    pub name: String,
}

/// Arguments to `shep enable`.
///
/// [`DogArgs`] plus one hidden field: `--exec` is pm2's own spelling of
/// `shep adopt`, kept as a working alias so muscle memory carries over —
/// `#[arg(hide = true)]`, not `#[command(alias = ..)]`, because the alias
/// is on an argument, not the subcommand itself. `shep enable --exec <path>
/// <name>` parses here and is routed to [`super::commands::dogs::adopt`] by
/// `main`'s own dispatch, never handled by `enable` itself: a dog already
/// built into this binary has no path to vet, so `enable` cannot carry out
/// what `adopt` does.
///
/// **Argument order is inverted from [`AdoptArgs`]'s own**, and that
/// inversion is deliberate, not an oversight to fix: pm2's own spelling
/// puts the path before the name (`--exec <path> <name>`), while `shep
/// adopt` puts the name first (`adopt <name> <path>`, decision, Rin). Both
/// arguments are strings, so a reader who assumes the two orders agree
/// introduces a silent swap that nothing short of a test catches — see
/// `main.rs`'s `the_hidden_pm2_spelling_reaches_adopt_with_the_arguments_the_right_way_round`.
#[derive(Debug, clap::Args)]
pub struct EnableArgs {
    /// The dog's name — the `[dog.<name>]` config key
    pub name: String,
    /// Hidden pm2-spelling alias for `shep adopt`: routes to `adopt` with
    /// this flag's value as the binary path
    #[arg(long, hide = true)]
    pub exec: Option<PathBuf>,
}

/// Arguments to `shep adopt`.
///
/// Positional, name then path (decision, Rin: no `--exec` flag on this
/// verb) — the reverse order of [`EnableArgs`]'s hidden `--exec` alias; see
/// that type's own doc for why the inversion is deliberate.
#[derive(Debug, clap::Args)]
pub struct AdoptArgs {
    /// The dog's name — the `[dog.<name>]` config key
    pub name: String,
    /// Path to the dog's binary, vetted before `shep.toml` is touched
    pub path: PathBuf,
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

/// Arguments to `shep import`.
#[derive(Debug, clap::Args)]
pub struct ImportArgs {
    /// Read this pm2 dump instead of `~/.pm2/dump.pm2`
    #[arg(long)]
    pub from: Option<PathBuf>,
    /// Write the Flockfile here instead of `./Flockfile.toml`
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Print the Flockfile that would be written, and write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Overwrite an existing Flockfile
    #[arg(long)]
    pub force: bool,
}

/// Arguments shared by `shep startup` and `shep unstartup`.
///
/// One struct for both verbs, and one field: the unit is named after the
/// user it runs the shepherd as, so that user is the only thing either verb
/// needs to be told. `--home` is read from [`GlobalArgs`] by `startup` and
/// ignored by `unstartup`, which removes a unit rather than writing one.
#[derive(Debug, clap::Args)]
pub struct StartupArgs {
    /// The user the unit runs the shepherd as (default: $SUDO_USER, else the invoking user)
    #[arg(long)]
    pub user: Option<String>,
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
    /// Run supervised by an init system: do not expect to have been
    /// daemonized, and report readiness once the flock is back
    #[arg(long)]
    pub foreground: bool,
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

    /// fails if clap accepts `shep scale web 0`. The refusal exists daemon-side
    /// too, and deliberately in both places — but a usage error should not cost a
    /// connection, and `range(1..)` is what puts the accepted range into `--help`.
    #[test]
    fn scale_refuses_a_count_of_zero_before_it_reaches_the_wire() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "scale", "web", "0"]).is_err());
        assert!(Cli::try_parse_from(["shep", "scale", "web", "1"]).is_ok());
    }

    /// fails if `scale` grows a default target. `shep scale 4` must be a usage
    /// error, never "scale whatever app happens to be first".
    #[test]
    fn scale_requires_both_the_name_and_the_count() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "scale"]).is_err());
        assert!(Cli::try_parse_from(["shep", "scale", "web"]).is_err());
    }

    /// Every verb sharing [`SelectorArgs`] must refuse to run without one.
    ///
    /// Fails if that struct's `selector` field ever gains a `default_value`.
    /// That is a one-line edit reaching six verbs at once, and it is worth a
    /// case of its own precisely because it looks harmless: it does not break
    /// a single other test, and what it changes is that `shep stop` — typed
    /// by an operator who then remembered which sheep they meant — becomes
    /// `shep stop all` instead of a usage error. `reopen` and `bleats`
    /// deliberately do have that default (see
    /// [`bleats_and_reopen_default_to_every_sheep`]); the difference is that
    /// neither of them ends a process.
    ///
    /// The explicit form is asserted alongside, for the reason
    /// [`flush_refuses_to_run_without_a_selector`] gives: a verb that had
    /// stopped accepting any selector at all would pass the first half on its
    /// own.
    ///
    /// `trigger` joins this group too, but cannot share the loop above
    /// verbatim: [`TriggerArgs`] carries two required positionals, not one,
    /// so a bare `shep trigger web` (selector only) is already a usage error
    /// for missing `action` regardless of whether `selector` itself has a
    /// default — that loop's second assertion would pass by accident. What
    /// pins `selector` specifically is the same thing
    /// `home_flag_is_wired_to_the_shep_home_env_var` checks for `--home`:
    /// the clap `Arg` itself, read directly off `trigger`'s own `Command`.
    #[test]
    fn a_selector_taking_verb_refuses_to_run_without_one() {
        use clap::{CommandFactory, Parser};
        for verb in [
            "stop", "restart", "reload", "delete", "describe", "thatlldo",
        ] {
            assert!(
                Cli::try_parse_from(["shep", verb]).is_err(),
                "`shep {verb}` with no selector must be a usage error, never \
                 the whole flock"
            );
            assert!(
                Cli::try_parse_from(["shep", verb, "web"]).is_ok(),
                "`shep {verb} web` must still parse"
            );
        }

        assert!(
            Cli::try_parse_from(["shep", "trigger"]).is_err(),
            "`shep trigger` with neither selector nor action must be a usage error"
        );
        assert!(
            Cli::try_parse_from(["shep", "trigger", "web", "reload-config"]).is_ok(),
            "`shep trigger web reload-config` (selector, then action) must parse"
        );

        let cmd = Cli::command();
        let trigger = cmd.find_subcommand("trigger").unwrap();
        let selector_arg = trigger
            .get_arguments()
            .find(|a| a.get_id().as_str() == "selector")
            .expect("TriggerArgs must still carry a `selector` field");
        assert!(
            selector_arg.is_required_set(),
            "trigger's selector must stay required, never default to the whole flock"
        );
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

    /// `shep barks` takes no selector and defaults `--tail` to `None` (every
    /// bark); `--tail N` parses to `Some(N)`. Fails if either the bare form
    /// stops parsing or `--tail` stops being optional — a `default_value`
    /// on it would turn "show everything" into a silent 10-line window with
    /// nothing to name why.
    #[test]
    fn barks_takes_no_selector_and_tail_defaults_to_everything() {
        use clap::Parser;
        let bare = Cli::try_parse_from(["shep", "barks"]).unwrap().command;
        let Commands::Barks(args) = bare else {
            panic!("`shep barks` must parse with no selector")
        };
        assert_eq!(args.tail, None);

        let tailed = Cli::try_parse_from(["shep", "barks", "--tail", "20"])
            .unwrap()
            .command;
        let Commands::Barks(args) = tailed else {
            panic!("expected barks")
        };
        assert_eq!(args.tail, Some(20));
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

        for hidden in ["thatlldo", "daemon", "dog"] {
            assert!(
                cmd.find_subcommand(hidden).unwrap().is_hide_set(),
                "{hidden} must stay hidden from --help"
            );
        }
        for visible in [
            "start",
            "flock",
            "bleats",
            "reload",
            "reopen",
            "flush",
            "barks",
            "trigger",
            "enable",
            "disable",
            "adopt",
            "rehome",
            "ping",
            "kill",
            "save",
            "muster",
            "startup",
            "unstartup",
            "completions",
        ] {
            assert!(
                !cmd.find_subcommand(visible).unwrap().is_hide_set(),
                "{visible} must stay visible in --help"
            );
        }
    }

    /// fails if `Commands::Dog` is wired to another verb, or if it is not
    /// hidden. It is a re-exec target, not something an operator runs.
    #[test]
    fn the_dog_subcommand_parses_and_stays_hidden() {
        use clap::{CommandFactory, Parser};

        let parsed = Cli::try_parse_from(["shep", "dog", "metrics"])
            .unwrap()
            .command;
        let Commands::Dog(args) = parsed else {
            panic!("expected dog")
        };
        assert_eq!(args.name, "metrics");

        let cmd = Cli::command();
        assert!(
            cmd.find_subcommand("dog").unwrap().is_hide_set(),
            "dog must stay hidden from --help"
        );
    }

    /// Fails if `enable`'s pm2-spelled `--exec` alias loses its `hide =
    /// true` and starts teaching itself in `--help` — the whole reason it
    /// is an argument-level hide rather than a documented flag: `shep
    /// adopt` is the verb the help text should point an operator at.
    #[test]
    fn the_exec_alias_stays_hidden_from_help() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let enable = cmd.find_subcommand("enable").unwrap();
        let exec_arg = enable
            .get_arguments()
            .find(|a| a.get_id().as_str() == "exec")
            .expect("EnableArgs must still carry a hidden `exec` field");
        assert!(
            exec_arg.is_hide_set(),
            "--exec must stay hidden from --help"
        );
    }
}
