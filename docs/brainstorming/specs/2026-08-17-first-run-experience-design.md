# First-run experience — design

**Date:** 2026-08-17
**Status:** approved, ready for an implementation plan
**Scope:** `shep` (the CLI crate) only. No wire-protocol change, no daemon
behaviour change, no new verb behaviour beyond one new verb (`welcome`).

## The problem

Rin, coming from five years of pm2, installed shep and ran the pm2 flow:

```
cargo install shep
shep startup
error[usage]: no directory at /root/.shep; pass --home with the $SHEP_HOME this unit should carry
```

Her second attempt, `shep startup --home ~/`, laid `logs/`, `run/` and `pids/`
directly in her home directory. Correct, and not what she meant.

Three separate faults sit behind that transcript.

**1. Nothing creates the default home before a daemon boots.** shep already
defaults `$SHEP_HOME` to `~/.shep` (`shep-core/src/paths.rs:54`), and the tree
is created twice over on the way to a running daemon: `launch.rs`'s
`launch_command` pre-creates `logs/` at `DIR_MODE` so the daemon's stdio
redirect does not hit `ENOENT`, and `boot::init_dirs` then creates the full
layout inside the daemon. Both run only when a daemon is being started.
`shep startup` installs an init unit without starting anything, so it is the
one verb that needs the home to exist beforehand — and it refuses instead of
creating it. The very first command a pm2 user types is the only one that
fails.

**2. `--home` is presented as a choice when it is plumbing.** It is the first
global option in `shep --help`. In practice its users are `shep dev`
(automatic `$SHEP_DEV_HOME`), the test suite (a tempdir per test), and a
root-owned system flock baked into an init unit. A person managing their own
processes never needs it. Meanwhile `fold` — the feature that actually answers
"how do I organise this" — is verb 18 of 34 in an alphabetical wall.

**3. `shep --help`'s long description is an internal implementation note.**
The doc comment on `Cli` (`shep-cli/src/cli.rs:23-34`) explains why
`bin_name = "shep"` is load-bearing, complete with markdown bold and "Phase 15
Task 11". clap renders a doc comment as `long_about`, so that paragraph is the
first thing `shep --help` prints today.

There is no first-run output of any kind, and the v1 spec never contemplated
one.

## What this is not

- **Not a grouping feature.** `$SHEP_HOME` is the daemon's data-root: the
  socket lives at `$SHEP_HOME/run/shep.sock`, so the home *is* which shepherd
  you are talking to. `fold` is the grouping feature and already exists
  (spec §200: a `fold` field on an app plus a `fold:<name>` selector). shep
  assumes one shepherd per user; this design leans the UI into that and adds
  no second level.
- **No new grouping, no new selector, no `fold` functionality.** `fold` gains
  prominence in help text and welcome copy. Its behaviour is untouched.
- **No website changes.** The docs site gets its own pass later.
- **No new way to create a non-default home.** See "Creating a second flock".

## 1. Home creation

### The rule

| Situation | Behaviour |
|---|---|
| No `--home`, no `$SHEP_HOME` | Resolve `~/.shep`. If missing, create it and print the welcome, then run the command. |
| `--home` or `$SHEP_HOME` names a path that exists | Use it. No welcome. |
| `--home` or `$SHEP_HOME` names a path that does not exist | Refuse. Do not create. |

The asymmetry is the point. `~/.shep` is a name shep chose, so shep may
conjure it. `/srv/api` is a name the operator typed, and the likeliest reason
it does not exist is a typo. Creating it silently would turn a typo into a
second, empty, invisible flock, and the resulting bug report is "shep lost all
my processes" when the truth is "you are looking at a different flock".

### The refusal

Exact text, replacing the current message at
`shep-cli/src/commands/startup/mod.rs:235` and generalised to every command:

```
error[usage]: no flock at /srv/typo
  did you mean to drop --home? the default is ~/.shep
  to set up a flock there deliberately:  mkdir -p /srv/typo
```

Rendered through the existing `refuse(streams, fmt, ExitCode::Usage, ..)`
helper, so `--format json` gets the same content in the standard error
envelope.

### Creating a second flock

There is no command for it, deliberately. A flock home is a directory:
`mkdir -p /srv/api` is the whole procedure. `boot::init_dirs` creates
`logs/`, `pids/` and `run/` inside it on the next boot, and — per its own doc
at `boot.rs:105-112` — re-tightens every directory to `DIR_MODE` (`0700`) on
*every* boot, not just the first. A home created with a loose umask is
narrowed the moment a shepherd starts in it, so handing the operator `mkdir`
is safe as well as small.

Rejected alternatives, both considered and dropped: `shep welcome --home
<path>` doing double duty as a creator (overloads a display verb with a
side effect, and reads as a workaround), and a dedicated `shep setup <path>`
(a 39th verb, and a ceremony around `mkdir`, for a case whose real users are
`shep dev`, the test suite, and an init unit — none of which type it).

### Implementation notes

- Creation uses the existing `DirBuilder::mode(DIR_MODE)` discipline, never
  `create_dir_all` followed by `set_permissions`. Both existing call sites
  (`launch.rs:53`, `boot.rs:99`) carry doc comments explaining the TOCTOU
  window a create-then-chmod sequence leaves open. A third call site must
  match them.
- `shep dev` and `shep runtime` create their own isolated homes today and are
  exempt from the refusal rule: `$SHEP_DEV_HOME` is a throwaway session root
  that is *expected* not to exist yet.
- The resolution happens once, before command dispatch, so every verb inherits
  it rather than each verb checking.

## 2. The welcome

### When it fires

Once per home, on whichever command creates it. Never again for that home.
`shep welcome` reprints on demand — a new verb whose only job is to print this
text. `shep welcome --home /srv/api` renders the text with that flock's paths
in it; consistent with rule 1, it does not create anything.

`shep welcome` on a cold machine is the one case where both paths fire at
once: it creates `~/.shep` like any other command, and it is also the verb
that prints the welcome. It prints once, on stdout, because an explicit
invocation outranks the side-effect path.

### Where it goes

Three suppression rules, all protecting the same case — a fresh machine is
exactly where a provisioning script runs first, and `shep start server.js` on
a cold box would otherwise emit a banner into whatever is parsing it.

1. Never when `--format json` is in effect.
2. Never when the destination stream is not a terminal
   (`std::io::IsTerminal`, already used at `lookout/mod.rs:112` and
   `commands/daemon.rs:163`).
3. **stderr** when the welcome is a side effect of another command;
   **stdout** when the user ran `shep welcome` by name.

Rule 3 is the one judgement call worth flagging: it chooses scriptability over
guaranteed visibility. `shep start server.js | jq` on a virgin box stays clean,
and a human at a terminal still sees the welcome because stderr is not
redirected in ordinary interactive use.

The home is still created when output is suppressed. Suppression governs the
text, never the side effect.

### The text

```
      ,-~-.     ,-~-.     ,-~-.
     ( o.o )   ( o.o )   ( o.o )       shep 0.1.0-alpha.1
      `-^-'     `-^-'     `-^-'        flock at ~/.shep
       " "       " "       " "
    /\  /\
   ( o  o )--,   the shepherd keeps them running
    `--..--'  |
      |  |    '

Set up ~/.shep. Logs, pids and the shepherd's socket live here.

Getting started
  shep start server.js    start it and keep it alive
  shep flock              see what's running
  shep bleats server      follow its output
  shep save               remember this flock across reboots
  shep startup            bring it back after a reboot

  shep welcome            show this again
```

19 lines, 66 columns at its widest. Roughly a third of pm2's banner.

The version string and both occurrences of the home path are substituted at
render time, so a `--home` render and a non-default `$SHEP_HOME` both read
correctly. The art is original work, not taken from the ASCII-art corpus.

Deliberately absent: a link, a donation ask, a "Runtime Edition" subtitle, and
any mention of `--home` or `fold`. The welcome teaches the five commands that
get someone to a running, reboot-surviving process. Everything else is
`shep --help`'s job.

## 3. `shep --help`

### The leaked note

The `bin_name` paragraph moves from a `///` doc comment to a `//` comment above
the `#[derive]`. It is legitimate engineering — confirmed again while designing
this, when a scratch clap binary without `bin_name` printed `Usage: claptest` —
it simply must not be rendered to users. The `///` doc comment shrinks to its
first line, `The \`shep\` command line.`

### Layout

clap 4.6 cannot group subcommands: `#[command(help_heading = ..)]` on a
subcommand variant does not compile, verified against clap 4.6.6 in a scratch
crate. The command section is therefore hand-written inside a `help_template`,
which was verified to render in the same scratch crate, as was `help_heading`
on a *global argument* (which is what demotes `--home`).

```
A process manager for your flock

Usage: shep [OPTIONS] <COMMAND>

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
Help             welcome help completions

Options:
      --format <FORMAT>  Output format [default: table]
  -q, --quiet            Suppress non-essential output

Less common:
      --home <HOME>      Talk to a different shepherd. Mostly plumbing:
                         `shep dev` sessions, a system-wide flock, tests.
                         You almost certainly want the default, ~/.shep.
                         [env: SHEP_HOME=]

Run `shep help <command>` for one command, or `shep welcome` for the tour.
```

`fold` appears in "See what's up", which is where someone looking to organise
a large flock will find it.

### Keeping the list honest

A hand-written list rots. A test enumerates
`Cli::command().get_subcommands()`, filters out `hide = true` entries (there
are 16 hidden items today), and asserts that every remaining verb appears in
exactly one group, and that every name in the groups is a real verb. Adding a
verb without filing it fails the suite. This mirrors the exact-string
discipline already used by `docs/whistle/tools.md`'s catalogue test and
`docs/lookout/frames.txt`.

## 4. Testing

- **Home creation:** default home missing → created at `0700` with the full
  layout, welcome printed, command proceeds. Default home present → no
  welcome. Explicit `--home` missing → refusal, exit `Usage`, and the
  directory is *not* created (asserted, since the whole point is that a typo
  leaves no trace). Explicit `--home` present → used, no welcome.
- **`shep startup` on a cold machine** is a regression test in its own right,
  since it is the transcript that started this: it must create `~/.shep`,
  print the welcome, and install the unit.
- **Suppression:** welcome absent under `--format json`; absent when the
  stream is not a terminal; on stderr as a side effect and on stdout for
  `shep welcome`. The existing `Streams { out, err }` split makes each
  assertion a plain buffer comparison.
- **Welcome text** is pinned by an exact-string test, as the lookout frames
  are, so the art cannot drift unnoticed.
- **Help groups:** the enumeration test above.
- **`--help` no longer contains** `bin_name`, `Phase 15`, or `load-bearing`.
  A crude assertion, and exactly the one that would have caught this.

## 5. Assumptions

Recorded because they were judgement calls, not requirements:

1. The welcome fires once per *home*, not once per machine or per user, so a
   `--home` flock and a second machine each get their own. Rin chose this over
   pm2's fire-on-every-daemon-spawn.
2. `shep welcome` is the verb name. Plain English, consistent with the
   terminology doc's rule that straight verbs stay first-class.
3. Suppression is by stream and format, not a `--no-banner` flag. A flag can
   be added later if anyone asks; nobody has.
4. The art ships at the "flock and shepherd" size, on the understanding that
   it may be trimmed to one sheep after Rin lives with it.
5. `import` gets its own help group despite being one verb, because the
   heading "Coming from pm2" is itself a signal to the audience this project
   is courting.
6. The refusal message suggests `mkdir -p` rather than offering to create the
   directory behind a confirmation prompt. shep has no interactive prompts
   anywhere and this design does not introduce the first one.
