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

use std::net::IpAddr;
use std::path::PathBuf;

/// The verb groups [`HELP_TEMPLATE`] renders, and the source of truth the
/// drift test checks the real command tree against.
///
/// clap 4.6 has no subcommand grouping -- `#[command(help_heading = ..)]` on
/// a subcommand variant does not compile, checked against 4.6.6 -- so the
/// section is hand-written and this table is what keeps it from rotting. Add
/// a verb without filing it here and
/// `every_visible_verb_appears_in_exactly_one_help_group` fails.
///
/// `#[cfg(test)]` because it is exactly that: the assertions' structured copy
/// of what [`HELP_TEMPLATE`] states in prose. The template is what ships.
#[cfg(test)]
const HELP_GROUPS: &[(&str, &[&str])] = &[
    (
        "Run things",
        &[
            "start", "serve", "stop", "restart", "reload", "delete", "stock",
        ],
    ),
    (
        "See what's up",
        &["flock", "describe", "bleats", "lookout", "fold", "barks"],
    ),
    (
        "Survive reboots",
        &["save", "muster", "startup", "unstartup"],
    ),
    ("Talk to a sheep", &["trigger", "signal", "whisper"]),
    (
        "The shepherd",
        &["ping", "kill", "reopen", "flush", "set", "get", "unset"],
    ),
    (
        "Dogs and agents",
        &["dogs", "enable", "disable", "adopt", "rehome", "whistle"],
    ),
    ("Foreground runs", &["runtime", "dev"]),
    ("Coming from pm2", &["import"]),
    ("Help", &["welcome", "init", "help", "completions", "style"]),
];

/// `--help`'s shape.
///
/// `{options}`, not `{all-args}`: the latter re-emits clap's own
/// alphabetical `Commands:` list underneath the grouped one, which is the
/// wall this replaces. The options section stays generated, so `--home`'s
/// `Less common` heading is still clap's work.
const HELP_TEMPLATE: &str = "\
{about}

{usage-heading} {usage}

Getting started
  shep start server.js    start it and keep it alive
  shep flock              see what's running
  shep bleats server      follow its output
  shep save               remember this flock across reboots
  shep startup            bring it back after a reboot

Run things       start serve stop restart reload delete stock
See what's up    flock describe bleats lookout fold barks
Survive reboots  save muster startup unstartup
Talk to a sheep  trigger signal whisper
The shepherd     ping kill reopen flush set get unset
Dogs and agents  dogs enable disable adopt rehome whistle
Foreground runs  runtime dev
Coming from pm2  import
Help             welcome init help completions style

Aliases          flock: list, ls   bleats: logs   lookout: dash   stock: scale   whisper: sendline

{options}{after-help}";

/// The `shep` command line.
// `bin_name = "shep"` below is load-bearing, not decoration. Without it, clap
// renders every `Usage:` line from `argv[0]` rather than from `name` — so
// `shep-runtime --help` prints `Usage: shep-runtime runtime ...` and
// `shep-dev --help` prints `Usage: shep-dev dev ...` (verified empirically:
// both alias binaries built and run with no override, Phase 15 Task 11).
// Pinned so every rendering of a verb's own usage line reads `shep <verb>`
// regardless of which of the three `[[bin]]` targets produced it — the alias
// binaries are convenience entrypoints for exactly that invocation, not
// commands in their own right, and their own `--help` should say so.
//
// A `//` comment, not `///`, deliberately: clap renders a doc comment as
// `long_about`, so as a doc comment this paragraph WAS the opening of
// `shep --help` for three phases. See the test
// `the_top_level_help_carries_no_implementation_notes`.
#[derive(Debug, clap::Parser)]
#[command(
    name = "shep",
    bin_name = "shep",
    version,
    about = "A process manager for your flock",
    propagate_version = true,
    help_template = HELP_TEMPLATE,
    after_help = "Run `shep help <command>` for one command, or `shep welcome` for the tour."
)]
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
    /// Output format
    #[arg(long, global = true, value_enum, default_value_t = Format::Table)]
    pub format: Format,
    /// Suppress non-essential output
    ///
    /// Currently narrows `bleats`' own notices (a dropped-events count, a
    /// daemon-shutdown notice, ...): diagnostics distinct from a sheep's
    /// own line or a real error, both of which still print regardless.
    #[arg(short, long, global = true)]
    pub quiet: bool,
    /// How much this invocation dresses up its output: `full`, `plain`, or
    /// `bare`
    ///
    /// Wins over `$SHEP_STYLE` and `shep.toml`'s `[style] level`. Omit to
    /// let those decide; `shep style` reports which one answered.
    // The precedence order above is `style::resolve`'s; this field is only
    // the flag's own place in it.
    #[arg(long, global = true, value_enum)]
    pub style: Option<crate::style::StyleLevel>,
    /// Talk to a different shepherd
    ///
    /// Mostly plumbing: `shep dev` sessions, a system-wide flock, tests. You
    /// almost certainly want the default, ~/.shep.
    // Declared last on purpose. It was the first global option anyone read,
    // which announced it as a choice when it is really the daemon's
    // data-root. `{options}` renders in declaration order and ignores
    // `help_heading`, so position is the only lever available here: a `Less
    // common` section would need `{all-args}`, which re-emits the
    // alphabetical command wall this template exists to replace.
    //
    // `//`, not `///`, for the same reason the note above `Cli` is one. This
    // paragraph shipped in `shep --help` for exactly one build before the
    // render was read.
    #[arg(long, global = true, env = "SHEP_HOME")]
    pub home: Option<PathBuf>,
}

/// `--format`'s two shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Human-readable columns (the default).
    Table,
    /// A versioned JSON envelope, one object per invocation.
    Json,
}

/// Which init system a unit is written for.
///
/// Five variants, all constructible on every target: `--init` lets an
/// operator name one directly, which is also what lets a macOS machine
/// exercise the systemd, openrc and rc.d renderers at all. Selection without
/// the flag is `commands::startup::current_init` — a runtime probe on Linux,
/// where systemd and openrc share one target triple, and a compile-time fact
/// everywhere else, where nothing else the target could be exists.
///
/// It lives in `cli.rs` rather than beside the renderers because `cli.rs`
/// compiles on **every** target while `mod commands` is `#[cfg(unix)]`. A
/// field on `StartupArgs` naming a type from a unix-only module breaks
/// `cargo check --workspace --all-targets --all-features --target
/// x86_64-pc-windows-gnu`, which is a phase-gate command. `Format` above is
/// the precedent: a `clap::ValueEnum` the parse surface owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum Init {
    /// Linux + systemd: a unit file, `Type=notify`.
    Systemd,
    /// Linux + openrc: an `openrc-run` script. No readiness protocol — see
    /// the renderer's own doc.
    Openrc,
    /// macOS: a `LaunchDaemon` plist.
    Launchd,
    /// FreeBSD: an `/etc/rc.subr` script under `/usr/local/etc/rc.d`.
    FreebsdRc,
    /// OpenBSD: an `/etc/rc.d/rc.subr` script under `/etc/rc.d`.
    OpenbsdRc,
}

/// Every verb the binary understands.
#[derive(Debug, clap::Subcommand)]
pub enum Commands {
    /// Start a sheep from a script, a Flockfile, or stdin.
    Start(StartArgs),
    /// Serve a directory over plain HTTP, as a managed sheep.
    ///
    /// Registers a sheep whose command line is this invocation, canonicalized
    /// and with `--foreground` appended — the same worker that answers every
    /// request also IS the registered sheep, so `shep describe` shows exactly
    /// what will run again on a restart.
    ///
    /// Binds loopback (`127.0.0.1:8080`) by default. A wider `--bind` is
    /// allowed, not refused, and gets a stderr notice naming what it exposes
    /// — the docroot is published to anything that can reach the port, and
    /// unencrypted unless the operator puts a proxy in front. `--auth`
    /// narrows that to anyone who also has the password, still sent as plain
    /// HTTP basic auth.
    ///
    /// Dotfiles, directory listings, and any symlink under the docroot are
    /// all refused by default — `--hidden`, `--listing`, and
    /// `--follow-symlinks` opt back in, each with its own reason a repo
    /// checkout or a deploy layout might need it.
    ///
    /// `--foreground` runs the worker directly in this terminal instead of
    /// registering a sheep — also how the registered sheep runs; the flag on
    /// the end of its own command line is the only difference.
    Serve(ServeArgs),
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
    /// Set how many instances one app runs — the stocking rate.
    ///
    /// An absolute count, not a change: `shep stock web 4` means web has four
    /// instances afterwards, whatever it had before. There is no +N/-N form —
    /// run it twice and get the same flock.
    ///
    /// Stocking up fills the lowest free instance slots; stocking down releases
    /// the highest, so stocking out and back returns the same slot numbers, the
    /// same SHEP_INSTANCE values and the same log files it started with.
    ///
    /// Exits as soon as the shepherd accepts, printing the instances that
    /// remain. On a stock-down the departing instances are still running their
    /// stop ladders at that point; they report themselves on the bus, under
    /// process.delete.
    ///
    /// The new count is written to the muster roll, so `shep save` and a
    /// reboot keep it.
    #[command(visible_alias = "scale")]
    Stock(StockArgs),
    /// List the flock.
    #[command(visible_aliases = ["list", "ls"])]
    Flock,
    /// List the dogs, and nothing else. `--available` lists the community
    /// index of dogs you could adopt instead of the ones this shepherd is
    /// running.
    Dogs(DogsArgs),
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
    /// The path can be given as-is, with a leading `~/`, or as a bare name
    /// already on `$PATH` (`cargo install` puts one there). Refuses, before
    /// touching the config at all, a path that resolves to nothing that
    /// exists, is not a file, has no execute bit set, or that this kernel
    /// will not exec — and refuses a name that already names a built-in
    /// verb or alias, since such a dog could never be reached. An adopted
    /// dog runs at the shepherd's own trust level, with no sandboxing
    /// beyond it. Once adopted, `shep <name> [args...]` runs it directly,
    /// passing `args` through untouched — a second invocation mode from
    /// the one the shepherd itself uses to supervise it.
    Adopt(AdoptArgs),
    /// Forget an adopted dog entirely: stops it if a shepherd is running,
    /// and removes it from `[daemon] enabled_dogs`, `[daemon]
    /// adopted_dogs`, and its own `[dog.<name>]` table.
    ///
    /// `shep disable` stops a dog without forgetting its configuration;
    /// `rehome` is the verb that forgets it.
    Rehome(DogArgs),
    /// Describe one sheep in detail.
    ///
    /// Includes the sheep's lambs: the processes the OS reports as
    /// descendants of its pid. That is not the same set the stop ladder
    /// kills, which acts on the process group — a double-forked descendant
    /// leaves this list and is still killed, and a setsid() one stays in it
    /// and survives.
    ///
    /// Lamb names are executable names, never command lines.
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
    /// Write one line to matched sheep's stdin.
    ///
    /// Only reaches an app whose Flockfile sets `stdin = true`. Nothing else
    /// implies it — unlike the shepherd channel, which `wait_ready` and
    /// `shutdown_with_message` both turn on — because nothing in shep needs a
    /// sheep's stdin except this verb. A sheep without it answers a `no_stdin`
    /// row naming the field.
    ///
    /// One line, and the terminator is shep's to add: a line containing a
    /// newline or a carriage return is a usage error rather than two commands.
    ///
    /// `sent` means the bytes were written and flushed to the pipe, not that
    /// the app read them. A pipe holds 64 KiB before it blocks, so a short line
    /// to an app that never reads its stdin is still `sent`.
    #[command(visible_alias = "sendline")]
    Whisper(WhisperArgs),
    /// List one fold.
    Fold(FoldArgs),
    /// Show or follow bleats (log output) for one or more sheep.
    #[command(visible_alias = "logs")]
    Bleats(BleatsArgs),
    /// Watch the flock on a live dashboard.
    ///
    /// Reads the shepherd two ways at once: it subscribes to the event bus so
    /// the screen moves as things happen, and it re-lists the flock every two
    /// seconds so a dropped event cannot leave the screen quietly wrong.
    ///
    /// If the shepherd stops answering, lookout re-dials a few times and then
    /// says so and stops updating. The values on screen stay exactly as they
    /// were, and it does not exit — you do.
    ///
    /// Needs a terminal: with stdout redirected it refuses rather than writing
    /// escape sequences into a file.
    #[command(visible_alias = "dash")]
    Lookout(LookoutArgs),
    /// Serve the MCP interface on stdin/stdout for an AI agent.
    ///
    /// Speaks the Model Context Protocol over stdio: an agent host launches
    /// this process and talks JSON-RPC to it on the pipe. It writes nothing
    /// else to stdout, because stdout is the wire.
    ///
    /// Five read-only tools are always offered. The four that act —
    /// start_sheep, stop_sheep, restart_sheep, reload_sheep — exist only when
    /// `[whistle] allow_control = true` in `$SHEP_HOME/shep.toml`.
    ///
    /// That gate is a guard against an agent acting on its own reading of
    /// your flock, not a security boundary: whistle runs as you, so anything
    /// it could do you can already do with `shep stop`. There is deliberately
    /// no flag for it — legibility, not containment: a boolean in
    /// `shep.toml` has a diff and an mtime an operator can audit, and
    /// `--home`/`SHEP_HOME` already choose which `shep.toml` that is, so a
    /// flag would open nothing those don't already.
    Whistle,
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
    /// Store a value in the shepherd's key/value store.
    ///
    /// Reads and writes `$SHEP_HOME/kv.json` directly and never connects to the
    /// shepherd — the store is for ad-hoc notes and dog settings, and it has to
    /// work while nothing is running, exactly as `shep enable` does.
    ///
    /// Keys are flat: letters, digits, `.`, `_` and `-`, up to 128 bytes, not
    /// starting with a dot. A dot is part of the name — `bark.cooldown` is one
    /// key, not a path into anything.
    Set(KvSetArgs),
    /// Read one value from the store, or list the whole store with no key.
    Get(KvGetArgs),
    /// Remove one key from the store, or every key with --all.
    Unset(KvUnsetArgs),
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
    /// Write a commented Flockfile to start from
    Init(InitArgs),
    /// Boot a shepherd in this process, run one Flockfile's flock in the
    /// foreground, and exit once nothing is left online.
    ///
    /// Meant for a container: no daemonization, no re-exec, no saved muster
    /// roll — `--no-restore` is always on, because a container starts from
    /// its Flockfile every time, never from a roll left on the image by a
    /// previous run.
    ///
    /// Bleats stream to this process's own stdout/stderr while it runs, so
    /// `docker logs` is the flock's log without any extra plumbing. The
    /// shepherd is still reachable over its own socket the whole time —
    /// `shep flock` from a second terminal, or `docker exec`, works exactly
    /// as it would against a daemonized one.
    ///
    /// Exits 0 once the flock has been empty and clean (every sheep
    /// `stopped`, none `errored`) for three consecutive two-second polls —
    /// a batch job finishing its work. Exits 11 (`flock_empty`) instead when
    /// the flock emptied with at least one sheep `errored` — a restart
    /// budget exhausted, or a spawn that never came up — so an orchestrator
    /// reading the exit status can tell "finished" from "died" and restart
    /// the container only for the second.
    Runtime(RuntimeArgs),
    /// Run one Flockfile's flock in an isolated, throwaway foreground
    /// session: `$SHEP_DEV_HOME` (default `~/.shep-dev`), forced `watch =
    /// true` on every app, and a full stop-and-delete teardown when it ends.
    ///
    /// **`--home` and `$SHEP_HOME` are ignored.** Isolation is the whole
    /// feature: an operator who exports `$SHEP_HOME` for their real flock
    /// gets a stderr notice rather than a `dev` session that shares it and
    /// silently forces `watch = true` onto production apps.
    ///
    /// Ends the moment the flock empties or this process is signalled —
    /// whichever comes first — and either way leaves nothing running and no
    /// shepherd behind. A `shep dev` that leaked a supervisor would stop
    /// being trusted.
    Dev(DevArgs),
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
    /// Writes an init unit for the target user — a systemd unit
    /// (`Type=notify`), a launchd plist, an openrc script, or a FreeBSD or
    /// OpenBSD `rc.d` script, picked automatically for the running target
    /// or named explicitly with `--init` below. Every unit carries this
    /// binary's own path, that user's $SHEP_HOME, and the PATH of this
    /// invocation — which is what makes an interpreter installed under
    /// ~/.bun or ~/.cargo findable after a reboot.
    ///
    /// The openrc and BSD scripts are rendered and pinned by exact-string
    /// tests; nobody on this project has run them on their own init system.
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
    /// Disable and remove whichever unit `startup` installed — systemd,
    /// openrc, launchd, or a BSD `rc.d` script.
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
    /// Print the welcome: the sheep, and the five commands worth knowing.
    ///
    /// The same text a fresh `$SHEP_HOME` prints once on its own. Here it is
    /// the command's output rather than a diagnostic, so it goes to stdout.
    Welcome,
    /// Show or set how much shep dresses up its output
    ///
    /// `full` is sheep, boxes and colour; `plain` drops the sheep; `bare` is
    /// plain text. With no level, prints the one in force and where it came
    /// from.
    Style(StyleArgs),
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
    /// Print the Flockfile JSON Schema. Hidden: the schema is committed at
    /// `crates/shep-core/assets/flockfile.schema.json`, and this is how it is
    /// regenerated.
    #[command(hide = true)]
    Schema,
}

/// Arguments to `shep start`.
#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// Selectors, script paths, Flockfiles, or `-` to read Flockfile JSON
    /// from stdin
    ///
    /// A target is resolved in four tiers and the first one that matches
    /// wins: a sheep the flock already has, by id or by name; then a fold,
    /// written either as `fold:<name>` or as the bare fold name; then a
    /// Flockfile, by its extension; then a path on disk, started as a
    /// script.
    ///
    /// So `shep start backed` starts the fold `backed` even when a file
    /// called `backed` is sitting in the current directory. Write `./backed`
    /// to mean the file: a sheep name may never contain a path separator, so
    /// a target carrying one is always a path.
    ///
    /// The wildcard selectors work here too, and mean what they mean
    /// everywhere else: `all`, `/regex/`, and glob patterns such as
    /// `web-*`. They reach only sheep the flock already has, since there is
    /// nothing to register. A sheep already running is reported and left
    /// alone; `restart` is the verb that replaces one.
    ///
    /// Omit the targets to start the Flockfile in the current directory, or,
    /// when there is none, to bring a shepherd up with nothing running yet.
    ///
    /// Several are started in turn, not atomically: if the second fails the
    /// first is already up, and the exit code is the first failure. `--name`
    /// is refused with more than one, since a name is unique to one sheep.
    #[arg(num_args = 0..)]
    pub targets: Vec<String>,
    /// Name for this sheep (script form only)
    #[arg(long)]
    pub name: Option<String>,
    /// Fold to place this sheep in
    #[arg(long)]
    pub fold: Option<String>,
    /// Working directory to run in (default: where you ran `shep start`)
    #[arg(long)]
    pub cwd: Option<String>,
    /// Interpreter to run the script with, overriding both shep.toml's
    /// extension mapping and a Flockfile app's own interpreter field.
    ///
    /// The precedence, lowest to highest: shep.toml's interpreters table
    /// (matched against the script's extension), then a Flockfile's own
    /// interpreter for that app, then this flag. shep never guesses an
    /// interpreter on its own; every one of those three is something an
    /// operator wrote down. Pass "none" to run the script directly,
    /// overriding a mapping or a Flockfile that would otherwise pick one.
    #[arg(long)]
    pub interpreter: Option<String>,
    /// Read TARGET as a Flockfile rather than as a script path.
    ///
    /// Required for a `.js` Flockfile and the only way to reach one: shep
    /// reads a `.js` config by running it through node, which is arbitrary
    /// code execution, so it never happens because a file merely has that
    /// extension. Without this flag `shep start server.js` starts
    /// `server.js` as a script, which is what it has always meant.
    #[arg(long)]
    pub flockfile: bool,
}

/// Arguments to `shep serve`.
///
/// `PartialEq` is derived for one reason and it is a test: Step 7.4's
/// round-trip asserts the whole struct, so a field added without teaching
/// `sheep_args` about it fails by construction rather than by somebody
/// remembering to extend a list of `assert!`s.
#[derive(Debug, PartialEq, Eq, clap::Args)]
pub struct ServeArgs {
    /// Directory to serve
    pub root: PathBuf,
    /// Port to listen on
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Address to bind. Loopback unless you say otherwise — a wider bind
    /// publishes every file under the directory to anything that can reach
    /// the port.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: IpAddr,
    /// Name for this sheep (default: the directory's own name)
    #[arg(long)]
    pub name: Option<String>,
    /// Fold to place this sheep in
    #[arg(long)]
    pub fold: Option<String>,
    /// Serve index.html for paths that do not exist, for a single-page app.
    /// Only for requests that accept HTML, so a missing script still 404s.
    #[arg(long)]
    pub spa: bool,
    /// List a directory that has no index.html. Off by default: a listing
    /// publishes every filename under it.
    #[arg(long)]
    pub listing: bool,
    /// Serve files and directories whose names begin with a dot. Off by
    /// default: serving a project directory would otherwise publish `.env`
    /// and the whole `.git` history. The one real use is
    /// `.well-known/acme-challenge`.
    #[arg(long)]
    pub hidden: bool,
    /// Follow symlinks under the docroot, reopening the check-then-open race
    /// refused by default. Needed for deploy layouts like
    /// `current -> releases/2026-08-15`; off unless you ask for it.
    #[arg(long)]
    pub follow_symlinks: bool,
    /// File holding one `user:password` line, mode 0600, required on every
    /// request. Sent over plain HTTP — base64, not encryption.
    #[arg(long)]
    pub auth: Option<PathBuf>,
    /// Serve in this terminal instead of registering a sheep.
    ///
    /// This is also how the registered sheep runs: the command line in
    /// `shep describe` is this one with the flag on the end.
    #[arg(long)]
    pub foreground: bool,
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
    /// One or more: name, id, `all`, `zeus-*`, `/regex/`, or `fold:<name>`
    ///
    /// Several are applied in turn, not atomically: `shep stop a b c` where
    /// `b` matches nothing still stops `a` and `c`, and the exit code is the
    /// first failure.
    ///
    /// A pattern carrying `*`, `?`, `[` or `{` is a glob, anchored, so
    /// `zeus-*` selects `zeus-auth` and not `my-zeus-auth`. Quote it: your
    /// shell expands `zeus-*` against filenames first, and zsh refuses
    /// outright when none match. A name with no such character is exact, so
    /// `web.1` is the sheep called `web.1`.
    #[arg(required = true, num_args = 1..)]
    pub selectors: Vec<String>,
}

/// Arguments to `shep stock`.
///
/// Not [`SelectorArgs`], and this is the only lifecycle verb that is not.
/// `instances` is a per-app number, so the target is an app NAME: no `all`,
/// no `/regex/`, no `fold:` — a selector matching two apps would have to mean
/// either four each or four in total, and neither reading is more obviously
/// right.
#[derive(Debug, clap::Args)]
pub struct StockArgs {
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

/// Arguments to `shep whisper`.
///
/// Not [`SelectorArgs`]: this verb needs a second positional, the line
/// itself. The selector stays required — no `default_value` — for the same
/// reason every running-process verb's does: an accidental `shep whisper`
/// should be a usage error, never sent to the whole flock.
#[derive(Debug, clap::Args)]
pub struct WhisperArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>`
    pub selector: String,
    /// The line, without a trailing newline — shep adds exactly one
    pub line: String,
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

/// Arguments to `shep set`.
#[derive(Debug, clap::Args)]
pub struct KvSetArgs {
    /// The key
    pub key: String,
    /// The value
    pub value: String,
}

/// Arguments to `shep get`.
#[derive(Debug, clap::Args)]
pub struct KvGetArgs {
    /// The key; omit to list every key
    pub key: Option<String>,
}

/// Arguments to `shep unset`.
///
/// `--all` rather than a reserved key name, for the reason [`FlushArgs`]'s own
/// doc gives about `shep flush shep`: nothing stops an operator having a key
/// called `all`, and `shep unset all` would then mean something different
/// depending on their own store. A flag cannot collide.
#[derive(Debug, clap::Args)]
pub struct KvUnsetArgs {
    /// The key to remove
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub key: Option<String>,
    /// Remove every key
    #[arg(long)]
    pub all: bool,
}

/// Arguments to `shep fold`.
#[derive(Debug, clap::Args)]
pub struct FoldArgs {
    /// The fold to list
    pub name: String,
}

/// `shep dogs`, and the index of dogs you could adopt.
#[derive(Debug, clap::Args)]
pub struct DogsArgs {
    /// List the dogs published in the community index instead of the ones
    /// this shepherd is running. Needs no shepherd.
    #[arg(long)]
    pub available: bool,
    /// Narrow the listing to entries whose name, package or description
    /// contains this text, case-insensitively.
    #[arg(value_name = "FILTER")]
    pub filter: Option<String>,
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
/// `name` here is a required positional, unlike [`AdoptArgs`]'s own
/// (optional, defaulted from the binary's file stem) — pm2's own spelling
/// carries no such default, and `enable --exec` exists only to keep that
/// spelling working verbatim, not to gain `adopt`'s newer conveniences. A
/// reader relying on the two shapes agreeing is a mistake nothing but a
/// test catches — see `main.rs`'s
/// `the_hidden_pm2_spelling_reaches_adopt_with_the_arguments_the_right_way_round`.
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
/// `path` is the one positional; `--name` is optional and defaults to the
/// binary's file stem with a leading `shep-` stripped, the way `cargo`
/// strips `cargo-` from its own external subcommands (`shep-log-rotate`
/// defaults to `log-rotate`). Previously both were required positionals,
/// name first (`adopt <name> <path>`) — a breaking CLI change, decision
/// Rin: `shep adopt <path>` alone now works for a binary whose name is
/// already the name you want, matching `shep start <script>`'s own
/// optional `--name`.
#[derive(Debug, clap::Args)]
pub struct AdoptArgs {
    /// Path to the dog's binary, vetted before `shep.toml` is touched.
    /// Resolved before vetting: as given, with a leading `~/` expanded, or
    /// looked up on `$PATH` if it names no directory — first hit wins.
    pub path: PathBuf,
    /// The dog's name — the `[dog.<name>]` config key. Defaults to the
    /// binary's file stem with a leading `shep-` stripped.
    #[arg(long)]
    pub name: Option<String>,
}

/// The selector the verbs that take an optional one fall back to.
///
/// One owner for the string, shared rather than spelled twice: `bleats` and
/// `reopen` default to the same thing on purpose, and a copy that drifted
/// would leave one of them quietly targeting something else.
const DEFAULT_SELECTOR: &str = "all";

/// How much history `shep bleats` prints before it starts following.
///
/// Fifteen because that is what pm2 has trained every operator to expect,
/// and because the number only has to be large enough to carry the reason a
/// sheep died. It is small enough that following a healthy flock still
/// starts nearly empty.
pub const DEFAULT_BLEAT_LINES: usize = 15;

/// Arguments to `shep bleats` (alias `logs`).
#[derive(Debug, clap::Args)]
pub struct BleatsArgs {
    /// Which sheep (default: all)
    #[arg(default_value = DEFAULT_SELECTOR)]
    pub selector: String,
    /// Print the tail of each sheep's log file and exit, instead of following
    #[arg(long)]
    pub no_follow: bool,
    /// How many existing lines of each stream to print before following
    ///
    /// A sheep that already crashed has said everything it is going to say,
    /// so following alone shows an empty screen while the reason sits in the
    /// file. This prints that much history first, then follows.
    ///
    /// Counted per stream, so the default prints up to this many lines of
    /// stdout and up to this many of stderr for each matched sheep. Narrow
    /// it with `--out` or `--err`.
    ///
    /// `0` prints no history at all, following only what arrives next.
    #[arg(long, default_value_t = DEFAULT_BLEAT_LINES, value_name = "N")]
    pub lines: usize,
    /// Only stderr
    #[arg(long, conflicts_with = "out")]
    pub err: bool,
    /// Only stdout
    #[arg(long, conflicts_with = "err")]
    pub out: bool,
}

/// Arguments to `shep lookout`.
#[derive(Debug, clap::Args)]
pub struct LookoutArgs {
    /// Open the dashboard's action gate. Off by default.
    ///
    /// With the gate open, `x` (stop), `R` (restart) and `L` (reload) each
    /// arm a confirm instead of acting on the keypress that pressed it;
    /// Enter sends the request, any other key cancels, and an unanswered
    /// confirm expires after ten seconds. Off, all three refuse outright
    /// with `read-only: actions need --allow-control`.
    ///
    /// A guard against a keystroke in a window you were reading, not a
    /// security boundary: lookout runs as you, so anything it could do you can
    /// already do with `shep stop`. Can also be set with `shep set
    /// lookout.allow_control true`; this flag wins.
    #[arg(long)]
    pub allow_control: bool,
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
/// One struct for both verbs: the unit is named after the user it runs the
/// shepherd as, and, since Task 6, after which init system it targets.
/// `--home` is read from [`GlobalArgs`] by `startup` and ignored by
/// `unstartup`, which removes a unit rather than writing one.
#[derive(Debug, clap::Args)]
pub struct StartupArgs {
    /// The user the unit runs the shepherd as (default: $SUDO_USER, else the invoking user)
    #[arg(long)]
    pub user: Option<String>,
    /// Write a unit for this init system instead of the detected one.
    ///
    /// `unstartup` takes it too: a unit installed under one init has to be
    /// removable after the host has changed to another.
    #[arg(long, value_enum)]
    pub init: Option<Init>,
}

/// Arguments to `shep completions`.
#[derive(Debug, clap::Args)]
pub struct CompletionArgs {
    /// Shell to generate a completion script for
    #[arg(value_enum)]
    pub shell: clap_complete::aot::Shell,
}

/// Arguments to `shep style`.
#[derive(Debug, clap::Args)]
pub struct StyleArgs {
    /// `full`, `plain`, or `bare`
    ///
    /// Sets `shep.toml`'s `[style] level`. Omit to report the level
    /// currently in force instead of changing it.
    // The same `StyleLevel` grammar `--style` parses, so `shep style loud`
    // and `shep --style loud` are rejected identically -- see
    // `style_verb_parses_the_same_grammar_as_the_style_flag` below.
    #[arg(value_enum)]
    pub level: Option<crate::style::StyleLevel>,
}

/// Arguments to the hidden `shep daemon` subcommand.
///
/// The last four are the CLI-flag layer of spec §5's `file < env < flags`,
/// one per `SHEP_*` variable `DaemonConfig::load` already reads. They live
/// here rather than on `GlobalArgs` because they configure **the shepherd**,
/// and this is the only invocation that runs one — `--log-level` on
/// `shep flock` would configure nothing.
///
/// Their real audience is an init unit's `ExecStart`, which can now say
/// `shep daemon --foreground --log-level info` without a config file.
#[derive(Debug, clap::Args)]
pub struct DaemonArgs {
    /// Boot without restoring the saved muster roll
    #[arg(long)]
    pub no_restore: bool,
    /// Run supervised by an init system: do not expect to have been
    /// daemonized, and report readiness once the flock is back
    #[arg(long)]
    pub foreground: bool,
    /// Emit the shepherd's own logs as JSON lines (overrides shep.toml and
    /// SHEP_LOG_JSON). Accepts 1, 0, true, false; bare means true.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = bool_flag
    )]
    pub log_json: Option<bool>,
    /// Lowest severity of the shepherd's own records that reaches its log
    #[arg(long, value_name = "LEVEL", value_parser = log_level_flag)]
    pub log_level: Option<shep_core::config::LogLevel>,
    /// Control-socket path override
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Longest a cron worker sleeps before re-deriving its next occurrence
    #[arg(long, value_name = "DURATION", value_parser = duration_flag)]
    pub max_cron_sleep: Option<shep_core::values::UpDuration>,
}

/// Arguments to `shep init`.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Where to write it. The extension picks the format: toml, yaml, yml,
    /// json or json5. Defaults to Flockfile.toml in this directory
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
    /// Show every option the grammar has, not just the common ones
    #[arg(long)]
    pub all: bool,
    /// Overwrite the Flockfile that is already here, keeping its own format
    #[arg(long)]
    pub force: bool,
}

/// Arguments to `shep runtime`.
#[derive(Debug, clap::Args)]
pub struct RuntimeArgs {
    /// Flockfile to run (default: discovered in the current directory)
    pub target: Option<String>,
    /// Run the supervisor in this process rather than splitting off an init.
    ///
    /// Set by the init half of a PID-1 split when it re-execs this binary,
    /// and never by a person. Also a safety catch: with this set the split
    /// cannot happen, so a mis-read pid can never produce a fork loop.
    #[arg(long, hide = true)]
    pub supervise: bool,
}

/// Arguments to `shep dev`.
///
/// No `--home` of its own, and the global one does not apply — see
/// `commands::dev::dev_home`.
#[derive(Debug, clap::Args)]
pub struct DevArgs {
    /// Script or Flockfile to run (default: discovered in this directory)
    pub target: Option<String>,
    /// Name for this sheep (script form only)
    #[arg(long)]
    pub name: Option<String>,
}

/// clap value parser over shep's own four boolean spellings — NOT clap's
/// `BoolishValueParser`, which also takes yes/no/y/n/on/off and would widen
/// the grammar on the flag side only.
fn bool_flag(value: &str) -> Result<bool, String> {
    shep_core::config::parse_daemon_bool(value)
        .ok_or_else(|| format!("expected one of 1, 0, true, false; got `{value}`"))
}

/// clap value parser over [`shep_core::config::LogLevel::from_name`] — the
/// same lowercase-only grammar `SHEP_LOG_LEVEL` accepts.
fn log_level_flag(value: &str) -> Result<shep_core::config::LogLevel, String> {
    shep_core::config::LogLevel::from_name(value).ok_or_else(|| {
        format!("expected one of off, error, warn, info, debug, trace; got `{value}`")
    })
}

/// clap value parser over `UpDuration`'s `FromStr`.
fn duration_flag(value: &str) -> Result<shep_core::values::UpDuration, String> {
    value
        .parse::<shep_core::values::UpDuration>()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// `web/`, three directories above this file, only when it actually
    /// exists.
    ///
    /// Same helper, and the same reasoning, as
    /// `dog_index::tests::workspace_web_dir` -- see that one for why this
    /// has to be a runtime check rather than `include_str!`. Duplicated
    /// rather than shared: two call sites and six lines each did not earn a
    /// crate-wide test-support module.
    fn workspace_web_dir() -> Option<PathBuf> {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web"));
        dir.is_dir().then(|| dir.to_path_buf())
    }

    /// Reads `web/{relative}`, or `None` outside the workspace checkout
    /// (see [`workspace_web_dir`]).
    ///
    /// `web/` present but `relative` missing is real drift inside the
    /// checkout, not a published-crate build, and panics accordingly.
    ///
    /// # Panics
    /// Inside the workspace, if `relative` cannot be read.
    fn read_workspace_web_file(relative: &str) -> Option<String> {
        let dir = workspace_web_dir()?;
        Some(
            std::fs::read_to_string(dir.join(relative)).unwrap_or_else(|err| {
                panic!("web/{relative} exists in the workspace but could not be read: {err}")
            }),
        )
    }

    #[test]
    fn the_command_tree_parses_and_is_internally_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert(); // clap's own structural self-check
    }

    /// fails when a visible verb is missing from the docs site's CLI
    /// reference generator.
    ///
    /// The generator's `VERBS` array is hand-kept, and regenerating the
    /// reference refreshes only what that array already names -- so a verb
    /// left out of it is invisible in the published docs and the generator
    /// reports success. `style` and `welcome` shipped that way, found
    /// 2026-08-23 when `init` did the same.
    ///
    /// `help` is clap's own subcommand and the one deliberate omission.
    ///
    /// Skips outside the workspace checkout -- see
    /// [`read_workspace_web_file`].
    #[test]
    fn every_visible_verb_reaches_the_docs_site_generator() {
        use clap::CommandFactory;

        let Some(generator) = read_workspace_web_file("scripts/generate-cli-reference.sh") else {
            return;
        };
        const NOT_DOCUMENTED: &[&str] = &["help"];

        let (_, rest) = generator
            .split_once("VERBS=(")
            .expect("the generator declares a VERBS array");
        let (block, _) = rest.split_once(')').expect("the VERBS array closes");
        let listed: Vec<&str> = block.split_whitespace().collect();

        let command = Cli::command();
        let missing: Vec<&str> = command
            .get_subcommands()
            .filter(|verb| !verb.is_hide_set())
            .map(clap::Command::get_name)
            .filter(|name| !NOT_DOCUMENTED.contains(name) && !listed.contains(name))
            .collect();

        assert!(
            missing.is_empty(),
            "these verbs would be missing from the published CLI reference: {missing:?}\n\
             add them to VERBS in web/scripts/generate-cli-reference.sh and re-run it"
        );
    }

    /// Every visible verb is filed under exactly one heading, and every name
    /// under a heading is a real verb. A hand-written list rots the first
    /// time somebody adds a command; this is what stops it, the way
    /// `docs/whistle/tools.md`'s catalogue test stops that list rotting.
    #[test]
    fn every_visible_verb_appears_in_exactly_one_help_group() {
        use clap::CommandFactory;
        let command = Cli::command();
        let visible: Vec<String> = command
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .map(|s| s.get_name().to_string())
            .collect();
        let filed: Vec<&str> = HELP_GROUPS
            .iter()
            .flat_map(|(_, verbs)| verbs.iter().copied())
            .collect();

        for verb in &visible {
            let times = filed.iter().filter(|f| *f == verb).count();
            assert_eq!(
                times, 1,
                "`{verb}` appears in {times} help groups; it must appear in exactly one"
            );
        }
        for name in &filed {
            // `help` is clap's own, generated rather than declared, so it is
            // the one filed name with no matching subcommand to find.
            assert!(
                *name == "help" || visible.iter().any(|v| v == name),
                "a help group names `{name}`, which is not a visible verb"
            );
        }
    }

    /// Every visible alias is named in `--help`, and only real ones are.
    ///
    /// The grouped listing replaced clap's own `Commands:` block, which used
    /// to render `[aliases: list, ls]` beside each verb. Nothing carried that
    /// forward, so for several phases `shep --help` did not mention a single
    /// alias while all six kept working -- found 2026-08-19 when the docs
    /// site's generated CLI reference could no longer find any.
    ///
    /// Derived from clap rather than compared against a second list, so
    /// adding `visible_alias` to a verb fails here until the line says so.
    #[test]
    fn the_help_template_names_every_visible_alias() {
        use clap::CommandFactory;
        let command = Cli::command();
        let mut expected: Vec<String> = command
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .filter_map(|s| {
                let aliases: Vec<&str> = s.get_visible_aliases().collect();
                (!aliases.is_empty()).then(|| format!("{}: {}", s.get_name(), aliases.join(", ")))
            })
            .collect();
        expected.sort();

        let line = HELP_TEMPLATE
            .lines()
            .find(|l| l.starts_with("Aliases"))
            .expect("HELP_TEMPLATE has an Aliases line");

        for entry in &expected {
            assert!(
                line.contains(entry.as_str()),
                "`--help`'s Aliases line does not name `{entry}`: {line}"
            );
        }

        // And nothing invented: every `verb: ` on the line is a real one.
        for token in line.trim_start_matches("Aliases").split_whitespace() {
            if let Some(verb) = token.strip_suffix(':') {
                assert!(
                    expected.iter().any(|e| e.starts_with(&format!("{verb}:"))),
                    "`--help` names aliases for `{verb}`, which has none"
                );
            }
        }
    }

    /// `HELP_TEMPLATE` is a literal and `HELP_GROUPS` is structured data, so
    /// the two can disagree. They may not: the template is what users read
    /// and the table is what the drift test above checks.
    #[test]
    fn the_help_template_and_the_group_table_agree() {
        for (heading, verbs) in HELP_GROUPS {
            let line = HELP_TEMPLATE
                .lines()
                .find(|l| l.starts_with(heading))
                .unwrap_or_else(|| panic!("`{heading}` is missing from HELP_TEMPLATE"));
            for verb in *verbs {
                assert!(
                    line.split_whitespace().any(|w| w == *verb),
                    "`{verb}` is filed under `{heading}` but is not on that line: {line}"
                );
            }
        }
    }

    /// The five commands that get someone to a reboot-surviving process.
    #[test]
    fn the_help_opens_with_a_worked_example() {
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        assert!(
            help.contains("Getting started"),
            "no getting-started block:\n{help}"
        );
        assert!(
            help.contains("shep start server.js"),
            "no worked example:\n{help}"
        );
    }

    /// `--home` is plumbing, not a choice, and it was the first global option
    /// anyone read.
    #[test]
    fn home_is_the_last_global_option_a_reader_meets() {
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        let home = help.find("--home").expect("--home is still documented");
        let format = help.find("--format").expect("--format is documented");
        let quiet = help.find("--quiet").expect("--quiet is documented");
        assert!(
            home > format && home > quiet,
            "--home must come after the options people actually choose:\n{help}"
        );
    }

    /// `--help` is the first thing a stranger reads, and for three phases it
    /// opened with this crate's own reasoning about clap's `bin_name`.
    /// clap turns a doc comment into `long_about`, and nobody ran the
    /// command after writing the comment.
    #[test]
    fn the_top_level_help_carries_no_implementation_notes() {
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        for leak in [
            "bin_name",
            "Phase 15",
            "load-bearing",
            "argv[0]",
            // A clap template placeholder reaching the render means a doc
            // comment is discussing the template rather than the command.
            "{options}",
            "{all-args}",
            "help_heading",
        ] {
            assert!(
                !help.contains(leak),
                "`shep --help` still contains the internal note {leak:?}:\n{help}"
            );
        }
    }

    /// `--help` is the largest body of user-facing copy in the product, and
    /// `welcome.rs`, `status.rs` and `output/table.rs` each pin "no em or en
    /// dashes in copy a user reads" for their own copy while nothing pinned
    /// this one -- exactly how an em dash on `--quiet`'s help text and Rust
    /// intra-doc-link syntax on `--style`'s both reached a real terminal
    /// before anyone ran the binary and read the rendered output rather
    /// than the doc comment that produced it.
    #[test]
    fn the_top_level_help_has_no_dashes_or_doc_link_syntax() {
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        assert!(
            !help.contains('\u{2014}'),
            "an em dash reached --help, which this project's copy rules forbid:\n{help}"
        );
        assert!(
            !help.contains('\u{2013}'),
            "an en dash reached --help, which this project's copy rules forbid:\n{help}"
        );
        assert!(
            !help.contains("[`"),
            "Rust intra-doc-link syntax reached --help -- an aside meant for a reader of the \
             source, not the terminal, belongs on a `//` comment rather than a `///` one:\n{help}"
        );
    }

    #[test]
    fn log_json_has_three_states() {
        use clap::Parser;
        let cases = [
            (vec!["shep", "daemon"], None),
            (vec!["shep", "daemon", "--log-json"], Some(true)),
            (vec!["shep", "daemon", "--log-json=false"], Some(false)),
            (vec!["shep", "daemon", "--log-json=1"], Some(true)),
        ];
        for (argv, expected) in cases {
            match Cli::try_parse_from(&argv).unwrap().command {
                Commands::Daemon(args) => assert_eq!(args.log_json, expected, "{argv:?}"),
                other => panic!("expected Daemon, got {other:?}"),
            }
        }
    }

    /// fails if the flag grammar widens past the env grammar — the exact
    /// drift `parse_daemon_bool` exists to prevent.
    #[test]
    fn the_flag_bool_grammar_matches_the_env_grammar() {
        use clap::Parser;
        for wider in ["--log-json=yes", "--log-json=on", "--log-json=TRUE"] {
            assert!(
                Cli::try_parse_from(["shep", "daemon", wider]).is_err(),
                "{wider} must not parse"
            );
        }
    }

    /// fails if `runtime` stops parsing, or if `--supervise` becomes
    /// visible. It is the init's own re-exec flag; a person typing it
    /// should not find it in `--help`.
    #[test]
    fn runtime_parses_and_its_supervise_flag_is_hidden() {
        use clap::Parser;

        let bare = Cli::try_parse_from(["shep", "runtime"]).unwrap();
        let Commands::Runtime(args) = bare.command else {
            panic!("expected runtime")
        };
        assert_eq!(args.target, None, "no target means discover");
        assert!(!args.supervise, "a person never sets this");

        let with_target = Cli::try_parse_from(["shep", "runtime", "./Flockfile.toml"]).unwrap();
        let Commands::Runtime(args) = with_target.command else {
            panic!("expected runtime")
        };
        assert_eq!(args.target.as_deref(), Some("./Flockfile.toml"));

        let supervised = Cli::try_parse_from(["shep", "runtime", "--supervise"]).unwrap();
        let Commands::Runtime(args) = supervised.command else {
            panic!("expected runtime")
        };
        assert!(args.supervise, "the init passes --supervise to its child");

        use clap::CommandFactory;
        let cmd = Cli::command();
        let runtime = cmd.find_subcommand("runtime").unwrap();
        assert!(!runtime.is_hide_set(), "runtime is a real, documented verb");
        let supervise_arg = runtime
            .get_arguments()
            .find(|a| a.get_id().as_str() == "supervise")
            .expect("RuntimeArgs must still carry a hidden `supervise` field");
        assert!(
            supervise_arg.is_hide_set(),
            "--supervise must stay hidden from --help"
        );
    }

    #[test]
    fn start_takes_a_flockfile_flag_and_defaults_it_off() {
        use clap::Parser;
        let plain = Cli::try_parse_from(["shep", "start", "srv.js"]).unwrap();
        let flagged = Cli::try_parse_from(["shep", "start", "srv.js", "--flockfile"]).unwrap();
        match (plain.command, flagged.command) {
            (Commands::Start(a), Commands::Start(b)) => {
                assert!(!a.flockfile, "absent means script form");
                assert!(b.flockfile);
            }
            other => panic!("expected two Start commands, got {other:?}"),
        }
    }

    /// fails if serve stops binding loopback by default. Spec §10 fixes it and
    /// nothing else in the phase asserts it: delete `default_value` and clap
    /// requires the flag, write `Option<IpAddr>` instead and an unspecified
    /// default silently binds 0.0.0.0.
    #[test]
    fn serve_binds_loopback_on_port_8080_unless_told_otherwise() {
        use clap::Parser;
        use std::net::{IpAddr, Ipv4Addr};
        let cli = Cli::try_parse_from(["shep", "serve", "./x"]).unwrap();
        let Commands::Serve(args) = cli.command else {
            panic!("expected serve")
        };
        assert_eq!(args.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(args.port, 8080);
        assert!(!args.listing, "decision 9");
        assert!(!args.hidden, "decision 4");
        assert!(
            !args.follow_symlinks,
            "decision 5 — the refusal is the safe default"
        );
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

    /// fails if clap accepts `shep stock web 0`. The refusal exists daemon-side
    /// too, and deliberately in both places — but a usage error should not cost a
    /// connection, and `range(1..)` is what puts the accepted range into `--help`.
    #[test]
    fn stock_refuses_a_count_of_zero_before_it_reaches_the_wire() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "stock", "web", "0"]).is_err());
        assert!(Cli::try_parse_from(["shep", "stock", "web", "1"]).is_ok());
    }

    /// fails if `stock` grows a default target. `shep stock 4` must be a usage
    /// error, never "stock whatever app happens to be first".
    #[test]
    fn stock_requires_both_the_name_and_the_count() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "stock"]).is_err());
        assert!(Cli::try_parse_from(["shep", "stock", "web"]).is_err());
    }

    /// Precedent: [`list_and_ls_both_reach_flock`]. Fails if `scale` stops
    /// being a visible alias for `stock`, or if either spelling stops
    /// reaching [`Commands::Stock`].
    #[test]
    fn stock_and_scale_both_reach_stock() {
        use clap::Parser;
        for argv in [["shep", "stock", "web", "3"], ["shep", "scale", "web", "3"]] {
            assert!(matches!(
                Cli::try_parse_from(argv).unwrap().command,
                Commands::Stock(_)
            ));
        }
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

    /// fails if `shep unset` with no key and no --all is accepted. It would have to
    /// mean either nothing or everything, and the everything reading is
    /// unrecoverable.
    #[test]
    fn unset_needs_a_key_or_the_all_flag() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "unset"]).is_err());
        assert!(Cli::try_parse_from(["shep", "unset", "a"]).is_ok());
        assert!(Cli::try_parse_from(["shep", "unset", "--all"]).is_ok());
    }

    /// fails if `--all` composes with a key. `shep unset a --all` would be an
    /// operator asking for one thing and a flag doing something far larger —
    /// the same conflict `shep flush all --daemon` is a usage error for.
    #[test]
    fn unset_refuses_a_key_and_all_together() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "unset", "a", "--all"]).is_err());
    }

    /// fails if `shep get` starts requiring a key. Bare `get` listing the whole
    /// store is the discovery path — an operator who does not remember what they
    /// set has nowhere else to look.
    #[test]
    fn get_takes_an_optional_key() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "get"]).is_ok());
        assert!(Cli::try_parse_from(["shep", "get", "a"]).is_ok());
    }

    /// fails if `set` becomes anything but two required positionals. A `set` with a
    /// defaultable value would let `shep set a` silently store an empty string.
    #[test]
    fn set_needs_both_a_key_and_a_value() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "set", "a"]).is_err());
        assert!(Cli::try_parse_from(["shep", "set", "a", "1"]).is_ok());
    }

    #[test]
    fn format_defaults_to_table_and_accepts_json() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["shep", "flock"]).unwrap();
        assert_eq!(cli.global.format, Format::Table);
        let cli = Cli::try_parse_from(["shep", "--format", "json", "flock"]).unwrap();
        assert_eq!(cli.global.format, Format::Json);
    }

    /// fails if `--style` stops being optional (a run must still work with
    /// nothing said, falling through to `$SHEP_STYLE`/`shep.toml`/default —
    /// see `style::resolve`), or if a level clap now rejects or mis-parses.
    #[test]
    fn style_flag_defaults_to_unset_and_accepts_the_three_levels() {
        use crate::style::StyleLevel;
        use clap::Parser;

        let cli = Cli::try_parse_from(["shep", "flock"]).unwrap();
        assert_eq!(cli.global.style, None);

        for (raw, expected) in [
            ("full", StyleLevel::Full),
            ("plain", StyleLevel::Plain),
            ("bare", StyleLevel::Bare),
        ] {
            let cli = Cli::try_parse_from(["shep", "--style", raw, "flock"]).unwrap();
            assert_eq!(cli.global.style, Some(expected), "--style {raw}");
        }

        assert!(Cli::try_parse_from(["shep", "--style", "loud", "flock"]).is_err());
    }

    /// fails if `StyleArgs::level` reverts to `Option<String>` (the
    /// original defect: a value that parsed but was read by nothing), or
    /// if the verb's grammar ever drifts from `--style`'s -- a value
    /// clap accepts on one spelling and rejects on the other would leave
    /// an operator unable to guess which one is broken.
    #[test]
    fn style_verb_parses_the_same_grammar_as_the_style_flag() {
        use crate::style::StyleLevel;
        use clap::Parser;

        let cli = Cli::try_parse_from(["shep", "style"]).unwrap();
        match cli.command {
            Commands::Style(args) => {
                assert_eq!(args.level, None, "bare `shep style` still reports")
            }
            other => panic!("expected Style, got {other:?}"),
        }

        for (raw, expected) in [
            ("full", StyleLevel::Full),
            ("plain", StyleLevel::Plain),
            ("bare", StyleLevel::Bare),
        ] {
            let cli = Cli::try_parse_from(["shep", "style", raw]).unwrap();
            match cli.command {
                Commands::Style(args) => assert_eq!(args.level, Some(expected), "style {raw}"),
                other => panic!("expected Style, got {other:?}"),
            }
        }

        let bad_flag = Cli::try_parse_from(["shep", "--style", "loud", "flock"]).unwrap_err();
        let bad_verb = Cli::try_parse_from(["shep", "style", "loud"]).unwrap_err();
        assert_eq!(
            bad_flag.kind(),
            bad_verb.kind(),
            "a bad value fails the same way through either spelling"
        );
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

    /// Pins `Flock`'s, `Bleats`'s, `Stock`'s and `Whisper`'s visible
    /// aliases, and that the hidden verbs (`thatlldo`, the internal `daemon`
    /// re-exec target) stay hidden from `--help`. A
    /// `visible_aliases`/`aliases` swap, or a dropped `hide = true`, passes
    /// every other test in this module but changes user-facing behavior
    /// silently.
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

        let stock = cmd.find_subcommand("stock").unwrap();
        assert_eq!(stock.get_visible_aliases().collect::<Vec<_>>(), ["scale"]);

        let whisper = cmd.find_subcommand("whisper").unwrap();
        assert_eq!(
            whisper.get_visible_aliases().collect::<Vec<_>>(),
            ["sendline"]
        );

        let lookout = cmd.find_subcommand("lookout").unwrap();
        assert_eq!(lookout.get_visible_aliases().collect::<Vec<_>>(), ["dash"]);

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
            "lookout",
            "reload",
            "reopen",
            "flush",
            "barks",
            "trigger",
            "stock",
            "whisper",
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

    /// Pins the spelling spec §9 gives — `sendline`, one word, never
    /// `send-line` — now carried as a literal `visible_alias` rather than a
    /// kebab-cased derive, so nothing wires up the two-word spelling by
    /// accident.
    #[test]
    fn sendline_is_spelled_one_word() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["shep", "sendline", "web", "gc"]).is_ok());
        assert!(Cli::try_parse_from(["shep", "send-line", "web", "gc"]).is_err());
    }

    /// Precedent: [`logs_reaches_bleats`]. Fails if `sendline` stops being a
    /// visible alias for `whisper`, or if either spelling stops reaching
    /// [`Commands::Whisper`].
    #[test]
    fn sendline_reaches_whisper() {
        use clap::Parser;
        for argv in [
            ["shep", "whisper", "web", "gc"],
            ["shep", "sendline", "web", "gc"],
        ] {
            assert!(matches!(
                Cli::try_parse_from(argv).unwrap().command,
                Commands::Whisper(_)
            ));
        }
    }

    /// fails if `dash` stops reaching the same verb as `lookout`.
    ///
    /// **A resolution claim, and only that.** `try_parse_from` answers
    /// identically whether the attribute is `visible_alias` or the hidden
    /// `alias`, so this test cannot see the difference and must not claim to:
    /// the visibility pin belongs in
    /// `alias_visibility_and_hiding_are_pinned`, which already owns that job
    /// for `flock`/`bleats`/`stock`/`whisper` and is extended below.
    #[test]
    fn dash_and_lookout_resolve_to_the_same_verb() {
        use clap::Parser;
        assert!(matches!(
            Cli::try_parse_from(["shep", "dash"]).unwrap().command,
            Commands::Lookout(_)
        ));
        assert!(matches!(
            Cli::try_parse_from(["shep", "lookout"]).unwrap().command,
            Commands::Lookout(_)
        ));
    }

    /// fails if the control gate stops being off by default, or stops being
    /// reachable from the flag. Rin's ruling: acting on a sheep needs a flag or
    /// config, mirroring `whistle.allow_control`.
    #[test]
    fn actions_are_off_unless_the_flag_says_otherwise() {
        use clap::Parser;
        let Commands::Lookout(default) = Cli::try_parse_from(["shep", "lookout"]).unwrap().command
        else {
            panic!("lookout parses to its own variant")
        };
        assert!(!default.allow_control);

        let Commands::Lookout(flagged) =
            Cli::try_parse_from(["shep", "lookout", "--allow-control"])
                .unwrap()
                .command
        else {
            panic!("lookout parses to its own variant")
        };
        assert!(flagged.allow_control);
    }

    /// fails if `shep whistle` stops parsing, or grows an argument. The
    /// absence of `--allow-control` is a decision (spec §14.7), so it is
    /// asserted rather than left to be noticed.
    #[test]
    fn whistle_takes_no_arguments_and_has_no_control_flag() {
        use clap::Parser;
        assert!(matches!(
            Cli::try_parse_from(["shep", "whistle"]).unwrap().command,
            Commands::Whistle
        ));
        assert!(
            Cli::try_parse_from(["shep", "whistle", "--allow-control"]).is_err(),
            "whistle's gate is `[whistle] allow_control` in shep.toml, and a flag would \
             let an agent host's own config open it in the same line that adds the server"
        );
    }
}
