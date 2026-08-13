# Phase 11 — the six remaining daemon-surface verbs

`scale`, `signal`, `sendline`, the KV store (`set`/`get`/`unset`), lambs in
`describe`, and the `channel.*` bus topic. Against merged `main` at `f73d4df`.

## Why these six, and why now

Rin's ruling, 2026-08-13: these come **before** lookout and whistle. Both of
those are surfaces over the daemon's operator API — a TUI pane that cannot
scale an app and an MCP tool list that cannot send a signal are a UI shipped
against a hole. Building the verbs first means the two surfaces get built once,
over a complete API, instead of built and then widened.

Phases 1–10 are merged: the daemon and its supervision engine, the 16-verb CLI,
the log plane, watch/cron/memory restarts, SO_REUSEPORT reload, custom actions
over the shepherd channel (now with a correlation id), the pm2 cutover, the
dogs subsystem with working metrics and bark dogs, and an audit-debt phase.

**Baseline: 1044 passed / 0 failed / 3 ignored, across 16 result lines.** Every
task below states an expected delta against that.

---

## Global constraints

- MSRV 1.88, edition 2024, `MIT OR Apache-2.0`
- `#![forbid(unsafe_code)]` in shep-core, shep-client and shep-cli; unsafe only
  in shep-daemon/src/sys.rs with per-block `// SAFETY:`
- `PROTOCOL_VERSION` stays 1; wire changes are additive under
  `#[non_exhaustive]` and must keep the pinned insta fixtures passing. Any new
  `Request`/`Response`/`BusEvent` variant needs a fixture — Phase 10 swept the
  unpinned ones and the next audit will check yours.
- IR-20, as Phase 10 rewrote it: a `pub` error enum in a library crate
  (shep-core, shep-daemon, shep-client) carries `#[non_exhaustive]` with a
  rationale in its own terms, or documents why not. Either way the comment is
  mandatory.
- IR-46: a test that can only fail by hanging must carry an explicit bound.
  "Fails only by hanging" has recurred in five phases of this project.
- the fast loop is `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`;
  shep-cli is `[[bin]]`-only so it needs `--bins`, never `--lib`, which silently
  runs nothing and reports success
- the task gate is fmt, clippy `-D warnings`, `cargo test --workspace --all-features`,
  `RUSTDOCFLAGS="-D warnings" cargo doc`; one cargo command at a time, `$?`
  captured directly, never through a pipe
- baseline 1044 passed / 0 failed / 3 ignored across 16 result lines
- terminology: the daemon is "the shepherd" and only that; one managed process
  is "a sheep" and the plural is always "the flock"; destructive operations and
  error text stay plain

### Reading the counts

Every task states an expected test count. Treat it as a **shape, not a
checksum** — three earlier briefs shipped a stale figure and cost a review loop
each. What matters is the delta this task adds and that `failed` stays `0`
across all 16 result lines.

### The exact commands

One cargo command per invocation, `$?` read directly:

```bash
cargo test -p shep-core   --lib  --all-features
cargo test -p shep-daemon --lib  --all-features -- --skip ::slow::
cargo test -p shep-daemon --lib  --all-features            # when touching extras.rs / watch/ / the sampler
cargo test -p shep-client --lib  --all-features
cargo test -p shep-cli    --bins --all-features            # NOT --lib: shep-cli has no lib target
cargo test -p shep-cli    --test cli_e2e --all-features
cargo test -p shep-daemon --test daemon_e2e --all-features
cargo test -p shep-daemon --test real_runner --all-features
```

Task gate, each from its own command:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

### Every check in this plan states its baseline

Phase 10 shipped four verification steps that could not fail: a `grep -c` whose
target string wrapped across a line break, an `ls *.snap.new` whose zsh
`nomatch` error is indistinguishable from success, a check whose stated
expectation was already false at HEAD, and a header-comment grep split across
two comment lines. So: **every non-cargo check below prints its baseline at
HEAD first**, and uses `find … | wc -l` rather than a bare glob. Run the
baseline command before you make the change. If it does not print what this
plan says it prints, stop and say so — the check is broken, not the tree.

Baselines taken at `f73d4df`, on this machine:

```bash
grep -rn "Request::Scale" crates/ | wc -l            # 0
grep -rn "SendLine\|sendline" crates/ | wc -l        # 0
grep -rn "OperatorSignal" crates/ | wc -l            # 0
grep -rni "\bkv\b" crates/ | wc -l                   # 0
grep -rn "pub struct Lamb" crates/ | wc -l           # 0
grep -c "lambs" crates/shep-core/src/protocol/request.rs        # 1 (the non_exhaustive rationale)
grep -c "channel\." crates/shep-core/src/protocol/events.rs     # 0
find crates -name '*.snap' | wc -l                   # 4
find crates/shep-core/src/protocol/snapshots -name '*.snap' | wc -l   # 3
```

---

## What this phase builds

| Task | Item | Crates touched |
|---|---|---|
| 1 | the fd-3 message types move to shep-core | core, daemon |
| 2 | the shepherd forwards fd-3 traffic onto the bus (`channel.*`) | daemon |
| 3 | `OperatorSignal` + the `Signal` wire | core |
| 4 | the shepherd delivers a signal, and `shep signal` | daemon, cli |
| 5 | the `Scale` wire | core |
| 6 | the supervisor scales an app | daemon |
| 7 | `shep scale` | cli |
| 8 | `AppConfig::stdin` + the `SendLine` wire | core |
| 9 | the spawn path pipes stdin | daemon |
| 10 | the supervisor writes a line to a sheep | daemon |
| 11 | `shep sendline` | cli |
| 12 | `shep_core::kv`, the file-locked store | core |
| 13 | `shep set` / `shep get` / `shep unset` | cli |
| 14 | the KV store end to end | cli |
| 15 | `Lamb` + `ProcessInfo::lambs` | core |
| 16 | the shepherd walks a sheep's lamb tree | daemon |
| 17 | `describe` renders the tree | cli |
| 18 | the ledger, the docs, and the phase gate | docs |

Seventeen build tasks — the sizing the survey gave, item for item — plus one
documentation task. Task 18 is not padding: six entries come out of
`deferred.md` in one commit, and splitting that across six tasks would have six
commits each leaving the ledger half true.

---

## Design decisions made here, not deferred

Six calls the survey flagged. Each is decided below, with the reasoning, and
each task implements the decision rather than relitigating it.

### 1. `scale` takes an absolute count. No `+N`/`-N`.

`shep scale web 4` means "web has four instances when this returns". There is
no relative form.

- **KISS.** One grammar, one meaning. A count is what the operator has in their
  head ("I want four") and it is what `AppConfig::instances` already stores.
- **A relative delta is ambiguous under concurrent scaling.** Two operators
  running `shep scale web +2` against a flock of two get either four or six
  depending on interleaving, and neither of them asked for a number they can
  check. An absolute count is idempotent: run it twice, get the same flock.
- **This project's own trace notes record a pm2 crash on the relative-remove
  path.** Those notes exist so we do not reproduce the bug. Not building the
  path is the strongest form of not reproducing it.

**On scale-down, the highest slot numbers go first.** `instance_slots`
(`crates/shep-daemon/src/assemble.rs:36`) allocates the *lowest free* slot, so
taking the highest first makes scale-up-then-down a round trip: `2 → 4 → 2`
leaves slots `0,1`, exactly the pair it started with. Taking the lowest first
would leave `2,3` — the same count, a different flock, different log file names
(`web-2-out.log`), and a different `SHEP_INSTANCE` for every surviving process.

**`shep scale web 0` is refused**, not treated as delete. `normalize` already
rejects `instances == 0` (`NormalizeError::ZeroInstances`), so accepting it
here would put a config through the daemon that the daemon's own validator
refuses. The refusal names `shep delete web`.

**A dog cannot be scaled.** A dog is one process by contract (spec §8); the
refusal says so.

**An app mid-reload cannot be scaled.** A reload holds two live processes in
one instance slot; scale-down picking that slot would remove one of them and
leave the swap with nothing to finish. `SupervisorError::ReloadInFlight` is
already the answer to exactly this shape.

### 2. `signal` targets the sheep, not its process group.

`shep signal web SIGHUP` delivers to the sheep's own pid. It does not use
`kill(-pgid, …)`.

The stop ladder already owns group-wide signalling and tree kill
(`RunningProcess::signal`'s own doc, `crates/shep-daemon/src/runner.rs:626`,
spells out why *that* one is group-wide: a `thing & wait` wrapper keeps its
child in a separate group, and a graceful stop has to reach both). `signal`
exists for the other job — an operator talking to the application, the
SIGHUP-to-reopen-config / SIGUSR1-to-dump-state kind of nudge. That
conversation is with the process the operator named. Broadcasting it to every
lamb sends SIGHUP to whatever `sh` wrapper and whatever `node` child happen to
be in the group, none of which the operator was addressing, and several of
which have their own meaning for the signal.

Having read spec §9 and §4: nothing there argues the other way. §4 assigns
group-wide delivery to the stop ladder by name and says nothing about `signal`;
§9 lists `signal` next to `sendline`, which is unambiguously a conversation
with the one process. The narrow reading is the one the spec supports, and it
is also the recoverable one — an operator who wanted the group can send to each
instance, while an operator who did not want the group cannot un-send.

**The accepted set is nine names**, in a grammar of its own
(`shep_core::signals::OperatorSignal`), not `KillSignal`'s four:
`SIGHUP SIGINT SIGQUIT SIGTERM SIGUSR1 SIGUSR2 SIGWINCH SIGCONT SIGKILL`.
`SIGSTOP` is refused: a stopped sheep still reads `online` in every listing and
the shepherd has no way to see the difference, so accepting it would put the
flock into a state shep cannot report. `SIGKILL` is accepted — it is the honest
spelling of "die now", the ladder already sends it, and the restart policy will
treat the exit exactly as it treats any other unexpected one.

**No raw signal numbers in shep-core.** They differ by platform (`SIGUSR1` is
10 on Linux and 30 on macOS), and shep-core is portable and has no libc. The
enum crosses the runner seam as an enum, exactly as `StopSignal` does, and
`tokio_runner.rs` maps it to `nix::sys::signal::Signal` with an explicit match.

**A reload drainee is signalled, not skipped.** `trigger` skips drainees
because an action expects a reply and a process mid-swap is on its way out. A
signal expects nothing back, and the drainee is a live process the operator's
selector matched. Skipping it would be a silent hold-back with no reply channel
in which to explain itself.

### 3. `sendline` needs stdin piped, and that is opt-in per app.

New `AppConfig` field: **`stdin`**, a bool, **default `false`**. Named to match
`channel`, which is the same shape of decision (a per-sheep pipe, off unless
asked for) and whose own doc already carries the cost argument.

Piping unconditionally is refused for three reasons, in order of weight:

- **It changes every spawn on the system.** Today every sheep gets
  `Stdio::null()` on fd 0 (`tokio_runner.rs:206`). Flipping that to a pipe for
  the whole flock is a behaviour change to processes nobody asked to change.
- **Programs detect stdin.** A closed or null stdin is how a great many
  programs decide they are non-interactive: no prompt, no pager, no readline,
  no colour. Handing them a pipe silently moves them to the other branch. `less`
  and `git` are the famous ones; a Node app calling `process.stdin.isTTY` is
  the common one.
- **It costs a descriptor and a task per sheep for the whole life of the
  process**, against spec §14.11's single-digit-MB idle-RSS goal — the same
  budget `channel`'s default is protecting.

A sheep without `stdin = true` answers `no_stdin` on a `sendline` row, naming
the field, exactly as a sheep without a channel answers `no_channel` on a
`trigger` row.

### 4. The KV store lives in shep-core, is written by the CLI, and never
touches the wire.

`crates/shep-core/src/kv.rs`, storing `$SHEP_HOME/kv.json`. `shep set`, `shep
get` and `shep unset` read and write that file directly and never connect to
the shepherd.

The deciding question is who else reads it. Spec §5 says the store is "retained
for ad-hoc + dog runtime tweaks" — so **a dog reads it**, which rules out a
CLI-private module. It does not rule out a file: a dog is `shep dog <name>`,
the same binary, so a store in shep-core is linked into every dog for free.

It is not daemon-mediated, and that is the part worth writing down. `[dog.<name>]`
config goes over the socket because the alternative on the table was the
child's *environment*, which is readable from the process table, inherited by
every grandchild and captured into crash dumps (spec §8's third departure). A
`0600` file inside a `0700` `$SHEP_HOME`, read by a process running as the same
uid, has none of those properties — the socket buys nothing over it. What a
daemon-mediated store would cost is real: `shep set` would stop working with no
shepherd running, which breaks the provisioning shape every other config verb
in this tree supports (`shep enable` writes `shep.toml` and exits 0 with no
daemon; `shep barks` and `shep flush --daemon` both work on files), and it
would put a second reader of one file behind a versioned wire for no gain.

`PROTOCOL_VERSION` is untouched by this item. No new `Request` variant.

**The locking shape is the one already proven twice in this tree** —
`shep_core::barks::append` (`crates/shep-core/src/barks.rs:291`) and
`ShepToml::edit` (`crates/shep-cli/src/commands/shep_toml.rs:101`). Exclusive
`flock(2)` on a **sibling** `kv.json.lock`, never on the target, because the
`rename` that installs new content replaces the inode the lock is held on;
content staged through a **uniquely-named** temp file (a fixed `.tmp` name had
one writer's `rename` consume the other's staging file and kill the loser with
`ENOENT`), created at mode `0600` at open time rather than chmod'd afterwards,
`fsync`ed, then `rename`d. Do not invent a third shape.

**Keys are flat, not paths.** A key is one opaque string matching
`[A-Za-z0-9._-]{1,128}`, not starting with `.`. `bark.cooldown` is one key
whose name contains a dot, not a path into a nested object. map.md's note
inherited "dotted/colon key parse w/ quotes" from pm2's own store; the project's
standing decision is that pm2 formats live only in the importer, and a nesting
grammar here would be a second config language next to the Flockfile, with its
own quoting rules, for a store the spec itself calls "not the primary config
path". The narrow alphabet also means `shep get $key` never needs quoting.

**`unset` clears everything behind `--all`, not behind a magic key name.**
`shep unset all` would mean something different depending on whether an
operator has a key called `all` — the identical collision `FlushArgs`' own doc
argues through for `shep flush shep`. A flag cannot collide.

### 5. Lambs render as pid + name. No cmdline, no per-lamb memory.

`Lamb { pid, name }`, where `name` is the executable name the OS reports.

- **pid alone is nearly useless.** A tree view whose rows are bare integers
  makes the operator run `ps` anyway, which is the thing the tree existed to
  save.
- **cmdline is refused.** A process's argv routinely carries credentials
  (`--password=`, `?token=`), and `shep describe --format json` is the output
  people paste into issues. `sysinfo::Process::name()` is the executable name,
  not argv, so it carries the useful half and not the dangerous one.
- **Per-lamb memory is refused.** The sheep's own row already reports the tree
  total, the numbers would invite "why don't these add up" (they do), and
  `deferred.md`'s `ProcessInfo` entry explicitly warns against growing this
  struct speculatively.

**The output must not imply a guarantee the walk does not make.**
`limits/mod.rs`'s own module doc (lines 18–41) already records that the ppid
walk is not the kill unit, and names both directions of divergence: a
double-forked descendant leaves the ppid tree but stays in the process group
(killed, never listed), and a `setsid()` grandchild stays in the ppid tree but
leaves the group (listed, never killed). So:

- the field is named `lambs` and its doc says "the processes the OS reports as
  descendants of this sheep's pid", never "the processes that die with it";
- `describe`'s table caption for the section reads
  `Lambs of web (id 3) — parent-pid descendants of 4242, which is not exactly
  the set a stop kills`, so the caveat is on the screen and not only in a doc;
- the type doc carries both divergence directions in full, and links
  `limits`' own note rather than restating it a second time free to drift.

`ProcessInfo::lambs` is `Option<Vec<Lamb>>`, not `Vec<Lamb>`. `None` means "this
reply did not walk the tree" — a peer daemon predating the field, or any reply
that is not a `Describe`. `Some(vec![])` means "walked, found none". That is the
same three-state honesty `out_file` and `cpu_percent` already carry in this
struct, and collapsing it to a bare `Vec` would render a pre-field daemon's
reply as "this sheep has no lambs".

**Only `Describe` populates it**, never `ListFlock`. The walk costs a second
whole-process-table refresh; a flock listing is the thing an operator leaves
running in a loop.

### 6. `channel.*` carries child→shepherd traffic only.

Three topics: `channel.ready`, `channel.metric`, `channel.action_reply`. The
shepherd's own `Shutdown` and `Action` writes do not go on the bus.

The security question is clean and the survey confirmed it: unlike a dog's
`[dog.<name>]` section, nothing on the shepherd channel is a credential — a
`Ready` is empty, a `Metric` is a name and a float, an `ActionReply` is text the
app chose to publish, and the outbound half is an action name plus operator-typed
params. So the event carries **the real message**, not a redacted summary. No
`DogSectionToml`-style newtype is needed here, and a derived `Debug` is safe.

Child→shepherd only, for four reasons:

- **It is the gap `deferred.md` actually names.** Its `channel.*` entry says
  `Ready`/`Metric` traffic and a stale or unprompted `action-reply` "stay just
  as invisible as before" — every one of those is inbound. The outbound half is
  not invisible: a `Shutdown` is reported by `process.stop`, and an `Action` is
  answered to the operator who sent it by `Response::Triggered`.
- **A `channel.action` event would be the only bus event reporting a request
  rather than an outcome.** Every `ProcessEventKind` is a thing that happened to
  a sheep. A dispatch the app has not answered yet is not one.
- **It would make the bus a loop.** A dog that both subscribes to `channel.*`
  and calls `Trigger` would see its own dispatches come back.
- **Cost.** `BusEvent`'s own doc is explicit that a new variant is additive for
  the protocol version but not free for a subscriber that predates it, and asks
  the cost be weighed before adding one. Two extra topics for traffic that has
  a reporter already does not clear that bar.

Adding the outbound half later stays additive: a second variant with two more
topics, no version bump, and `channel.*` subscribers pick it up automatically.

**The fd-3 message types move to shep-core to make this possible.** A bus event
carries the message, and a bus event is a shep-core type — so a copy of
`ChildMessage` in shep-core alongside the original in shep-daemon would be two
spellings of one wire with no test able to compare them. Task 1 moves the
types and re-exports them from `shep_daemon::channel`, so no import in the tree
changes.

---

## Task order and dependencies

Tasks 1–2 (channel), 3–4 (signal), 5–7 (scale), 8–11 (sendline), 12–14 (KV) and
15–17 (lambs) are six independent chains. Within a chain the order is binding.
Across chains it is not, with two exceptions:

- **Task 8 and Task 15 both change a pinned snapshot** (`request_wire_v1` gains
  a field on `AppConfig`; `reply_wire_v1` and `bus_event_wire_v1` gain a field
  on `ProcessInfo`). Do not run them concurrently in two worktrees — the second
  will land on a snapshot the first already moved and the diff stops being
  reviewable.
- **Task 1 moves types between crates.** Land it before anything else touches
  `crates/shep-daemon/src/channel.rs`.

---

## Task 1 — the shepherd channel's message types move to shep-core

**Builds:** the precondition for `channel.*`. No behaviour change.

**Files created:**
- `crates/shep-core/src/protocol/channel.rs` — the moved module.

**Files modified:**
- `crates/shep-core/src/protocol/mod.rs` — declare + re-export.
- `crates/shep-daemon/src/channel.rs` — becomes a re-export shim.

**Produces, for Tasks 2 and 10:**

```rust
// crates/shep-core/src/protocol/channel.rs
pub const CHANNEL_VERSION: &str = "1";

pub enum ChildMessage {
    Ready,
    Metric { name: String, value: f64 },
    ActionReply { action: String, body: String, id: Option<u64> },
}

pub enum ShepherdMessage {
    Shutdown,
    Action { name: String, params: Option<String>, id: u64 },
}
```

Re-exported from `shep_core::protocol` and, unchanged, from
`shep_daemon::channel` — so every existing `use crate::channel::{ChildMessage,
ShepherdMessage}` in shep-daemon keeps compiling untouched.

### Step 1.1 — move the file

`git mv crates/shep-daemon/src/channel.rs crates/shep-core/src/protocol/channel.rs`

Then edit the moved file's module doc. The old one names shep-daemon's own
readers; the new one has to say why a portable crate owns an fd-3 wire:

```rust
//! The shepherd channel: the newline-JSON wire carried on fd 3 between the
//! shepherd and each spawned child.
//!
//! [`ChildMessage`] flows child -> shepherd (readiness, metrics, action
//! replies); [`ShepherdMessage`] flows shepherd -> child (shutdown request,
//! custom actions). Framing (newline-JSON over `BufReader::lines()`) is wired
//! by shep-daemon's real runner; this module only pins the message shapes.
//!
//! # Why this lives in shep-core
//!
//! It did not, until `BusEvent::Channel` (spec §6's `channel.*` topic) began
//! carrying a [`ChildMessage`] verbatim to every subscriber. A bus event is a
//! shep-core type, so the message it carries has to be one too — and a second
//! copy of these shapes in shep-daemon would be two spellings of one wire that
//! no test could compare across the crate boundary. shep-daemon re-exports
//! both types from its own `channel` module, so nothing that already names
//! them had to change.
//!
//! Both enums are deliberately NOT `#[non_exhaustive]`, unlike everything else
//! under `protocol`. There is no handshake on fd 3 and no version to negotiate
//! (`CHANNEL_VERSION` is a stamp, not a negotiation — see its own doc), so a
//! new variant here is a change every app that speaks this wire has to be told
//! about out of band. Leaving them exhaustive means the compiler names every
//! site that has to decide something, [`BusEvent::topic`] included, which is
//! exactly the review a change on this wire deserves.
//!
//! This module pins the wire shapes; it is not the app-author-facing contract.
//! An app that wants to speak this wire — including why it should reply to a
//! [`ShepherdMessage::Action`] even when it does not recognize the name, how an
//! echoed `id` gets a reply matched to its exact trigger and what the
//! name-and-order fallback costs an app that does not echo it, and the `params`
//! quoting gap — wants `docs/shepherd-channel.md` at the repository root.
```

The rest of the file — both enums, every doc comment on them, and the whole
`#[cfg(test)] mod tests` block with its five round-trip fixtures — moves
verbatim. Do not rewrite any of it. The `[`ProcIo::to_child`](crate::runner::ProcIo::to_child)`
intra-doc link in the old module doc is the one thing that cannot survive the
move (that type is in shep-daemon); it is gone from the replacement above.

### Step 1.2 — wire it into shep-core

`crates/shep-core/src/protocol/mod.rs`, alongside the existing module
declarations and re-exports:

```rust
pub mod channel;

pub use channel::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
```

Match the file's existing declaration/`pub use` ordering rather than appending
at the end.

### Step 1.3 — shep-daemon re-exports

Replace the whole of `crates/shep-daemon/src/channel.rs` with:

```rust
//! The shepherd channel's message shapes, re-exported from shep-core.
//!
//! These types moved to [`shep_core::protocol::channel`] when
//! [`BusEvent::Channel`](shep_core::protocol::BusEvent) began carrying a
//! [`ChildMessage`] to bus subscribers: the event is a shep-core type, so the
//! message had to become one. This module stays as the name shep-daemon knows
//! them by — `crate::channel::ChildMessage` is written across the runner, the
//! supervisor, the scripted fake and `tests/real_runner.rs`, and a re-export
//! keeps every one of those spellings correct.
//!
//! Nothing is defined here. Add a variant, a field or a fixture in shep-core.

pub use shep_core::protocol::channel::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
```

### Step 1.4 — the move compiles and the fixtures still pass

```bash
cargo test -p shep-core --lib --all-features
```

Expect green, **+5 tests** for this crate — the five fd-3 round-trip fixtures
that moved with the file (`ready_wire_fixture_round_trips`,
`metric_wire_fixture_round_trips`, `an_action_reply_without_an_id_round_trips`,
`an_action_reply_with_an_echoed_id_round_trips`, `shutdown_wire_fixture_round_trips`,
`an_action_carries_its_id_with_or_without_params` — six, in fact; count them in
the moved file rather than trusting this sentence).

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

Expect green with **−6** for this crate, the same tests, now counted in
shep-core. Net workspace zero.

Baseline for the "nothing else changed" check — run **before** the move:

```bash
grep -rn "crate::channel::" crates/shep-daemon/src | wc -l
```

At `f73d4df` this prints a non-zero count. Run it again after Step 1.3 and it
must print **the same number**: the re-export means no call site moves. A
smaller number means someone "helpfully" rewrote imports to point at shep-core,
which is not this task and makes the diff unreviewable.

### Step 1.5 — MUTATION

In `crates/shep-core/src/protocol/channel.rs`, change

```rust
#[serde(tag = "kind", rename_all = "kebab-case")]
```

on `ChildMessage` to

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
```

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `an_action_reply_without_an_id_round_trips` and
`an_action_reply_with_an_echoed_id_round_trips` both fail — `action-reply`
becomes `action_reply` and neither the fixture string nor the round trip
matches. `ready_wire_fixture_round_trips` and `metric_wire_fixture_round_trips`
must stay **green** (`ready` and `metric` are single words, identical under
both renames). A mutation that reddens all four means the fixtures are only
proving "serde does something", not the exact kebab spelling.

Revert.

### Step 1.6 — CHANGELOGs and gate

`crates/shep-core/CHANGELOG.md`, `[Unreleased]` → `Additions`:
"`protocol::channel` — the shepherd channel's `ChildMessage`/`ShepherdMessage`
shapes, moved here from shep-daemon so a bus event can carry one."

`crates/shep-daemon/CHANGELOG.md`, `[Unreleased]` → `Changes`:
"`channel::ChildMessage`/`ShepherdMessage` are re-exports of the shep-core
types now. Same names, same wire, same imports."

Then the full task gate.

---

## Task 2 — the shepherd forwards fd-3 traffic onto the bus

**Builds:** spec §6's `channel.*` topic. Closes `deferred.md`'s
**`channel.*` bus topic** entry (removed in Task 18).

**Files modified:**
- `crates/shep-core/src/protocol/events.rs` — the variant, the topics, fixtures.
- `crates/shep-daemon/src/supervisor.rs` — `run_sheep`'s `from_child` arm.
- `crates/shep-daemon/tests/daemon_e2e.rs` — one real-child case.

**Consumes from Task 1:** `shep_core::protocol::ChildMessage`.

**Produces:**

```rust
// crates/shep-core/src/protocol/events.rs
BusEvent::Channel { id: u32, message: ChildMessage }
// topics: "channel.ready" | "channel.metric" | "channel.action_reply"
```

### Step 2.1 — RED: the topics do not exist

Add to `crates/shep-core/src/protocol/events.rs`'s `#[cfg(test)] mod tests`:

```rust
/// fails if a shepherd-channel message maps to the wrong dotted topic. A
/// subscriber that asked for `channel.metric` and silently receives nothing
/// has no other way to find out, and `channel.*` matches whatever typo is
/// there — so the exact strings are the contract, not the prefix.
#[test]
fn every_shepherd_channel_message_has_its_own_topic() {
    for (message, topic) in [
        (ChildMessage::Ready, "channel.ready"),
        (
            ChildMessage::Metric {
                name: "rps".to_string(),
                value: 42.0,
            },
            "channel.metric",
        ),
        (
            ChildMessage::ActionReply {
                action: "gc".to_string(),
                body: "ok".to_string(),
                id: Some(7),
            },
            "channel.action_reply",
        ),
    ] {
        let event = BusEvent::Channel {
            id: 3,
            message: message.clone(),
        };
        assert_eq!(event.topic(), topic, "{message:?}");
    }
}

/// fails if `channel.*` stops reaching every one of the three. The glob a
/// dashboard writes is the prefix, so a topic that drifted out from under it
/// (`channel_ready`, say) would be unreachable by the only pattern anyone
/// actually subscribes with.
#[test]
fn the_channel_glob_reaches_all_three_topics() {
    for message in [
        ChildMessage::Ready,
        ChildMessage::Metric {
            name: "rps".to_string(),
            value: 1.0,
        },
        ChildMessage::ActionReply {
            action: "gc".to_string(),
            body: String::new(),
            id: None,
        },
    ] {
        let topic = BusEvent::Channel { id: 1, message }.topic();
        assert!(
            topic.starts_with("channel."),
            "`{topic}` is not under the channel.* glob"
        );
    }
}

/// fails if the event stops carrying the message body. The whole argument for
/// putting the real message on the bus rather than a summary is that nothing
/// on this wire is a credential — a reply body that arrived truncated or
/// replaced would make the topic useless for the case it exists for, a
/// dashboard watching what apps actually say.
#[test]
fn a_channel_event_carries_the_message_verbatim() {
    let event = BusEvent::Channel {
        id: 3,
        message: ChildMessage::ActionReply {
            action: "gc".to_string(),
            body: "freed 12MB".to_string(),
            id: Some(7),
        },
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("freed 12MB"), "{json}");
    assert_eq!(serde_json::from_str::<BusEvent>(&json).unwrap(), event);
}
```

Add `use crate::protocol::channel::ChildMessage;` to the test module's imports.

Run:

```bash
cargo test -p shep-core --lib --all-features
```

**Expected failure — for the stated reason:** compile error, ``no variant or
associated item named `Channel` found for enum `BusEvent` ``. Not an assertion
failure. If it compiles and the topics come back wrong, the variant exists and
`topic()` does not have its arms — also a correct red.

### Step 2.2 — GREEN: the variant

In `crates/shep-core/src/protocol/events.rs`, add to `BusEvent` after `LogErr`
and before `Dropped`:

```rust
    /// One message a sheep wrote on its shepherd channel (fd 3).
    ///
    /// Child->shepherd only. The shepherd's own writes — the
    /// `{"kind":"shutdown"}` of `shutdown_with_message`, and an `action` a
    /// `Trigger` dispatched — are deliberately not here. Every one of them is
    /// something an operator or the daemon just did and already has a
    /// reporter: a shutdown message is followed by `process.stop`, and an
    /// action is answered to the caller that sent it by
    /// `Response::Triggered`. Putting them here as well would make this the
    /// only event on the bus reporting a REQUEST rather than an outcome, and
    /// would loop a dog that both subscribes and triggers back onto its own
    /// dispatches. Adding the outbound half later stays additive — another
    /// variant, more `channel.` topics, no version bump — so this is a
    /// narrowing, not a door closed.
    ///
    /// `message` is the app's own text, whole and unredacted, unlike the
    /// `[dog.<name>]` config that travels as [`DogSectionToml`]. Nothing on
    /// this wire is a credential: `Ready` is empty, `Metric` is a name and a
    /// float, and an `ActionReply` body is text the app chose to publish to
    /// whoever triggered it. That is what makes a derived `Debug` safe here.
    ///
    /// [`DogSectionToml`]: crate::protocol::DogSectionToml
    Channel {
        /// The sheep that wrote it.
        id: u32,
        /// The message, exactly as it came off fd 3.
        message: ChildMessage,
    },
```

Add the import at the top of the module: `use crate::protocol::channel::ChildMessage;`

And in `BusEvent::topic`, between the `LogErr` and `Dropped` arms:

```rust
            // Total over `ChildMessage`, with no wildcard, and that is the
            // point of leaving that enum exhaustive (see its module doc): a
            // fourth kind on fd 3 fails to compile here until someone decides
            // what its topic is, rather than defaulting into a topic no
            // subscriber ever asked for.
            Self::Channel { message, .. } => match message {
                ChildMessage::Ready => "channel.ready",
                ChildMessage::Metric { .. } => "channel.metric",
                ChildMessage::ActionReply { .. } => "channel.action_reply",
            },
```

Run:

```bash
cargo test -p shep-core --lib --all-features
```

Expect green, **+3** tests.

### Step 2.3 — GREEN: pin the wire shape

Extend `bus_event_wire_snapshots` in the same file. After the `lifecycle`
extension and before the `insta::assert_json_snapshot!` line:

```rust
        // All three shepherd-channel topics, over one sheep id, because the
        // adjacent-tagged shape puts the message's own `kind` INSIDE `data`
        // next to `id` — a nesting that is easy to get wrong by hand and
        // invisible in a round-trip test, which only proves this crate agrees
        // with itself.
        events.extend([
            BusEvent::Channel {
                id: 3,
                message: ChildMessage::Ready,
            },
            BusEvent::Channel {
                id: 3,
                message: ChildMessage::Metric {
                    name: "rps".to_string(),
                    value: 42.0,
                },
            },
            BusEvent::Channel {
                id: 3,
                message: ChildMessage::ActionReply {
                    action: "gc".to_string(),
                    body: "freed 12MB".to_string(),
                    id: Some(7),
                },
            },
        ]);
```

Run the test. It fails with an insta diff. Review the diff: it must be **three
new objects appended**, each `{"event":"channel","data":{"id":3,"message":{...}}}`,
and **no change to any existing row**. Then accept:

```bash
INSTA_UPDATE=always cargo test -p shep-core --lib --all-features
git diff --stat crates/shep-core/src/protocol/snapshots/
```

The `git diff --stat` must show exactly one file changed, with insertions and
**zero deletions**. Deletions mean an existing pinned row moved, which this
task does not do.

Baseline check that the accept did not leave a stray pending file:

```bash
find crates -name '*.snap.new' | wc -l
```

At HEAD this prints `0`; after the accept it must print `0` again. (`find … |
wc -l`, not a glob: a bare `ls *.snap.new` under zsh's `nomatch` errors on the
success case, which is indistinguishable from failure — Phase 10 shipped that
check and it could not fail.)

### Step 2.4 — RED: the shepherd does not forward

Add to `crates/shep-daemon/src/supervisor.rs`'s `#[cfg(test)] mod tests`, near
the existing action-reply cases (`actor_with_an_open_channel` is at line 8726):

```rust
/// fails if a `ready` on fd 3 reaches only the readiness machinery and never
/// the bus. Readiness already had a consumer, which is exactly why it is the
/// case worth pinning: forwarding must be a SECOND thing the arm does, not a
/// replacement for the first.
#[tokio::test(start_paused = true)]
async fn a_ready_on_the_channel_reaches_both_the_bus_and_the_readiness_wait() {
    let h = harness(vec![ProcScript::runs_forever()]);
    let mut events = h.ctx.events.subscribe();
    let mut app = AppConfig::minimal("web", "./srv");
    app.channel = true;
    app.wait_ready = true;
    let id = register_sheep(&h, app).await;

    h.ctx
        .supervisor
        .deliver_child_message_for_test(id, ChildMessage::Ready)
        .await;

    // Bounded (IR-46): without the timeout this case can only fail by
    // hanging on a bus that never receives.
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap() {
                BusEvent::Channel { id, message } => break (id, message),
                _ => continue,
            }
        }
    })
    .await
    .expect("no channel event within 5s");

    assert_eq!(seen, (id, ChildMessage::Ready));

    // The readiness half still works: the sheep goes Online off this same
    // message, which is what it did before the bus ever saw one.
    let listed = h.ctx.supervisor.list().await;
    assert_eq!(
        listed.iter().find(|i| i.id == id).unwrap().status,
        ProcStatus::Online
    );
}

/// fails if a metric is still only a `tracing::debug!`. That log line was the
/// whole of what a metric ever produced, and a subscriber could not read it —
/// this is the case that says the topic exists for a reason.
#[tokio::test(start_paused = true)]
async fn a_metric_on_the_channel_reaches_the_bus_with_its_name_and_value() {
    let h = harness(vec![ProcScript::runs_forever()]);
    let mut events = h.ctx.events.subscribe();
    let mut app = AppConfig::minimal("web", "./srv");
    app.channel = true;
    let id = register_sheep(&h, app).await;

    h.ctx
        .supervisor
        .deliver_child_message_for_test(
            id,
            ChildMessage::Metric {
                name: "rps".to_string(),
                value: 42.0,
            },
        )
        .await;

    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let BusEvent::Channel { id, message } = events.recv().await.unwrap() {
                break (id, message);
            }
        }
    })
    .await
    .expect("no channel event within 5s");

    assert_eq!(
        seen,
        (
            id,
            ChildMessage::Metric {
                name: "rps".to_string(),
                value: 42.0,
            }
        )
    );
}
```

`deliver_child_message_for_test` does not exist. It is the seam this test
needs: the scripted fake owns the `from_child` sender and nothing in the actor
API can push one. Add it in Step 2.5 alongside the forwarding.

Run:

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

**Expected failure — for the stated reason:** compile error, ``no method named
`deliver_child_message_for_test` found for struct `SupervisorHandle` ``.

### Step 2.5 — GREEN: forward, and the test seam

In `crates/shep-daemon/src/supervisor.rs`, `run_sheep`'s `from_child` arm
(currently lines 4678–4705). Replace the whole `match maybe_msg` body's three
`Some(..)` arms with a shape that forwards first and then does what it already
did:

```rust
            maybe_msg = from_child.recv(), if from_child_open => {
                match maybe_msg {
                    Some(message) => {
                        // Forwarded BEFORE it is acted on, and unconditionally.
                        // A subscriber's view of fd 3 must not depend on
                        // whether this daemon happens to have a consumer for
                        // that kind: an `action-reply` nobody is waiting for
                        // is dropped a few lines below, and `deferred.md`
                        // names exactly that message as the traffic the bus
                        // exists to stop losing.
                        let _ = events.send(BusEvent::Channel {
                            id,
                            message: message.clone(),
                        });
                        match message {
                            ChildMessage::Ready => {
                                let _ = actor_tx.send(Msg::Ready { id }).await;
                            }
                            ChildMessage::Metric { name, value } => {
                                tracing::debug!(
                                    id,
                                    name,
                                    value,
                                    "child metric forwarded to the bus as channel.metric"
                                );
                            }
                            ChildMessage::ActionReply {
                                action,
                                body,
                                // The child's `id` is the DISPATCH's, not the
                                // sheep's; `id` above is the sheep's. Renamed
                                // at the boundary so no line downstream has to
                                // hold both meanings.
                                id: stamp,
                            } => {
                                let _ = actor_tx
                                    .send(Msg::ActionReply { id, action, body, stamp })
                                    .await;
                            }
                        }
                    }
                    None => from_child_open = false,
                }
            }
```

The `message.clone()` is one allocation per fd-3 line. That is the same cost
`BusEvent::LogOut` already pays per log line a few arms up, on traffic that is
orders of magnitude heavier; say so in review rather than reaching for an `Arc`.

Then the test seam. Add to `impl SupervisorHandle`, beside the other
crate-private command methods:

```rust
    /// Delivers `message` to `id`'s sheep task as though the child had written
    /// it on fd 3.
    ///
    /// Test-only, and it exists because the scripted fake owns the sending
    /// half of `ProcIo::from_child` and hands a test no way to reach it. The
    /// real path is proven against a real child in `tests/daemon_e2e.rs`; this
    /// is what lets the engine-tier cases run under the paused clock.
    #[cfg(test)]
    pub(crate) async fn deliver_child_message_for_test(&self, id: u32, message: ChildMessage) {
        let _ = self
            .tx
            .send(Msg::DeliverChildMessage { id, message })
            .await;
    }
```

with a matching `Msg` variant and an actor arm that hands it to the sheep's own
task. **Read `Msg` and the actor's `run` loop before writing this**: if a
simpler seam exists by the time you get here — the fake growing a
`ScriptedRunner::child_message(spawn_index, msg)` that pushes straight into the
`from_child` sender it already holds — prefer it. That is the better shape,
because it exercises `run_sheep`'s real arm rather than a second path into it.
Take the fake route if `ScriptedRunner` already retains the sender; take the
`Msg` route only if it does not.

Run:

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

Expect green, **+2** tests.

### Step 2.6 — GREEN: a real child, end to end

Add to `crates/shep-daemon/tests/daemon_e2e.rs`, modelled on the existing
two-round-trip trigger case:

```rust
/// A real child writing on a real fd 3, observed by a real subscriber over a
/// real socket. The engine-tier cases prove the actor forwards; only this
/// proves the whole path — socketpair, newline framing, the bus, the topic
/// filter, and the frame encoder — carries a `channel.metric` to a client that
/// asked for `channel.*` and nothing else.
#[tokio::test]
async fn a_childs_metric_reaches_a_channel_subscriber_over_the_socket() {
    let mut fixture = Fixture::boot().await;
    let mut sub = fixture.client().await;
    sub.subscribe(vec!["channel.*".to_string()]).await;

    // Writes one metric on fd 3 and then sleeps, so the sheep is still alive
    // when the assertion runs.
    let script = r#"printf '{"kind":"metric","name":"rps","value":42}\n' >&3; sleep 30"#;
    let mut app = AppConfig::minimal("chatty", "/bin/sh");
    app.args = vec!["-c".to_string(), script.to_string()];
    app.channel = true;

    let mut ops = fixture.client().await;
    ops.request(Request::Start { apps: vec![app] }).await;

    // Bounded (IR-46): a subscriber that never receives would otherwise hang
    // this test rather than fail it.
    let frame = tokio::time::timeout(Duration::from_secs(10), sub.next_event())
        .await
        .expect("no channel.* frame within 10s");

    match frame {
        BusEvent::Channel {
            message: ChildMessage::Metric { name, value },
            ..
        } => {
            assert_eq!(name, "rps");
            assert!((value - 42.0).abs() < f64::EPSILON, "{value}");
        }
        other => panic!("subscribed to channel.*, received {other:?}"),
    }

    fixture.shutdown().await;
}
```

Adapt `Fixture`/`client()`/`subscribe`/`next_event` to whatever the file's own
helpers are actually called — read them first; the shapes above are the
intent, not a promise about names. The `log.out`/`process.*` subscriber cases
already in that file are the pattern to copy.

Run:

```bash
cargo test -p shep-daemon --test daemon_e2e --all-features
```

Expect green, **+1**.

### Step 2.7 — MUTATION

In `crates/shep-core/src/protocol/events.rs`, change

```rust
                ChildMessage::Metric { .. } => "channel.metric",
```

to

```rust
                ChildMessage::Metric { .. } => "channel.ready",
```

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `every_shepherd_channel_message_has_its_own_topic` fails on the
metric row. `the_channel_glob_reaches_all_three_topics` must stay **green** —
`channel.ready` is still under the glob — and that is the point of having both:
the prefix test alone would wave this through.

Revert, then a second mutation in `crates/shep-daemon/src/supervisor.rs`:
delete the `let _ = events.send(BusEvent::Channel { … });` line from
`run_sheep`'s `from_child` arm. Run
`cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Must go red:** both new supervisor cases time out at their 5s bound and fail
with `no channel event within 5s` — a *failure*, not a hang, which is the
whole reason the bound is there. Every pre-existing action-reply and readiness
case must stay **green**: the forward is additive and removing it must not
disturb what the arm already did.

Revert.

### Step 2.8 — CHANGELOGs and gate

`crates/shep-core/CHANGELOG.md` → `Additions`: "`BusEvent::Channel` and the
`channel.ready` / `channel.metric` / `channel.action_reply` topics (spec §6)."

`crates/shep-daemon/CHANGELOG.md` → `Additions`: "Every shepherd-channel
message a sheep writes is forwarded to the bus, including an `action-reply`
no trigger is waiting for."

Then the full task gate.

---

## Task 3 — `OperatorSignal`, and the `Signal` wire

**Builds:** the grammar and the frames `shep signal` needs. No behaviour yet.

**Files created:**
- `crates/shep-core/src/signals.rs`

**Files modified:**
- `crates/shep-core/src/lib.rs` — declare the module.
- `crates/shep-core/src/protocol/request.rs` — `Request::Signal`,
  `Response::Signalled`, `SignalReply`, `SignalOutcome`, fixtures.

**Produces, for Task 4:**

```rust
// crates/shep-core/src/signals.rs
pub enum OperatorSignal { Hup, Int, Quit, Term, Usr1, Usr2, Winch, Cont, Kill }
impl OperatorSignal {
    pub const ACCEPTED: [&'static str; 9];
    pub fn parse(name: &str) -> Option<Self>;
    pub fn as_str(self) -> &'static str;
}

// crates/shep-core/src/protocol/request.rs
Request::Signal { selector: SelectorSpec, signal: String }
Response::Signalled(Vec<SignalReply>)
pub struct SignalReply { pub id: u32, pub name: String, pub outcome: SignalOutcome }
#[non_exhaustive] pub enum SignalOutcome { Delivered, NotRunning, Failed { reason: String } }
```

### Step 3.1 — RED: the grammar

Create `crates/shep-core/src/signals.rs` with only the test module first, so
the red is a compile error against a type that does not exist:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `ACCEPTED` and `as_str` disagree. The list is what a refusal
    /// prints, so an operator picking a replacement word is reading it — a
    /// name advertised but not parsed sends them in a circle.
    #[test]
    fn every_accepted_name_round_trips_through_parse() {
        for name in OperatorSignal::ACCEPTED {
            let parsed = OperatorSignal::parse(name)
                .unwrap_or_else(|| panic!("`{name}` is advertised but not parsed"));
            assert_eq!(parsed.as_str(), name);
        }
    }

    /// fails if the bare form or a lowercase spelling stops parsing. Both are
    /// accepted for the reason `KillSignal` accepts both: an operator types
    /// what `kill -l` prints, and that is the bare form.
    #[test]
    fn the_prefix_and_the_case_are_both_optional() {
        assert_eq!(OperatorSignal::parse("hup"), Some(OperatorSignal::Hup));
        assert_eq!(OperatorSignal::parse("SigUsr1"), Some(OperatorSignal::Usr1));
        assert_eq!(OperatorSignal::parse("WINCH"), Some(OperatorSignal::Winch));
    }

    /// fails if SIGSTOP is ever waved through. It is the one real, spellable,
    /// deliverable signal this grammar refuses, and the refusal is the design:
    /// a stopped sheep still reads `online` in every listing shep can produce,
    /// so accepting it would put the flock in a state the shepherd cannot
    /// report on.
    #[test]
    fn sigstop_is_refused_because_the_shepherd_could_not_report_it() {
        assert_eq!(OperatorSignal::parse("SIGSTOP"), None);
        assert_eq!(OperatorSignal::parse("stop"), None);
    }

    /// fails if a name outside the table parses. `SIGSEGV` is the shape that
    /// matters: a real signal, plausibly typed, that shep has no business
    /// delivering on an operator's behalf.
    #[test]
    fn a_name_outside_the_table_does_not_parse() {
        assert_eq!(OperatorSignal::parse("SIGSEGV"), None);
        assert_eq!(OperatorSignal::parse(""), None);
        assert_eq!(OperatorSignal::parse("9"), None);
    }

    /// fails if this grammar stops covering the one `kill_signal` already
    /// accepts. The two exist for different jobs and are allowed to differ —
    /// but the operator-facing set being NARROWER than the config-facing one
    /// would mean a signal shep sends on every stop is one an operator may not
    /// ask for by name, which is indefensible in either direction.
    #[test]
    fn every_kill_signal_name_is_also_an_operator_signal() {
        for name in crate::config::KillSignal::ACCEPTED {
            assert!(
                OperatorSignal::parse(name).is_some(),
                "`{name}` is a kill_signal but not an operator signal"
            );
        }
    }
}
```

Add `pub mod signals;` to `crates/shep-core/src/lib.rs`.

Run:

```bash
cargo test -p shep-core --lib --all-features
```

**Expected failure — for the stated reason:** compile error, ``cannot find type
`OperatorSignal` in this scope``.

### Step 3.2 — GREEN: the grammar

Prepend to `crates/shep-core/src/signals.rs`:

```rust
//! The signals `shep signal` may name.
//!
//! A grammar of its own, next to [`KillSignal`](crate::config::KillSignal)'s
//! four rather than replacing them, because the two answer different
//! questions. `KillSignal` is what a Flockfile's `kill_signal` may say: a
//! signal the stop ladder can deliver as its polite rung and then escalate
//! PAST. This one is what an operator may hand a running app: a nudge, with no
//! ladder behind it and no escalation to follow. `SIGHUP` belongs here and not
//! there; `SIGKILL` belongs here and not there for the opposite reason.
//!
//! # No raw numbers
//!
//! Deliberately no `as_raw`. Signal numbers are not portable — `SIGUSR1` is 10
//! on Linux and 30 on macOS, `SIGCONT` is 18 and 19 — and shep-core is the
//! portable crate with no libc to ask. The enum crosses shep-daemon's runner
//! seam as an enum, exactly as `StopSignal` does, and `tokio_runner.rs` is the
//! one place that turns it into something the kernel understands.
//!
//! # What is not here, and why
//!
//! `SIGSTOP` parses to nothing. It is deliverable and an operator might mean
//! it, but a `SIGSTOP`ed sheep still reads `online` in `shep flock`, in
//! `describe`, on the bus and to every dog — the shepherd owns no mechanism
//! that could see the difference. Refusing it keeps shep from producing a
//! flock state it cannot describe. `SIGCONT` IS accepted, because an operator
//! who stopped a sheep by some other route needs a way back.

/// A signal `shep signal` may name.
///
/// Nine, not every signal on the platform. Each one here is something an
/// operator plausibly means to say to an application, and nothing here is a
/// signal shep would be delivering on the kernel's behalf (`SIGSEGV`,
/// `SIGBUS`, `SIGPIPE` and the rest are the kernel's to send, not an
/// operator's).
///
/// Exhaustive, not `#[non_exhaustive]`, matching
/// [`KillSignal`](crate::config::KillSignal) and for the same reason (IR-20:
/// don't cargo-cult it). Growth is possible but is not anticipated, and a
/// caller matching on all nine — shep-daemon's own mapping to `nix` is the one
/// that matters — should get a compile error the day a tenth arrives rather
/// than a silent wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorSignal {
    /// `SIGHUP` — hang up. The near-universal "re-read your configuration".
    Hup,
    /// `SIGINT` — interrupt, what Ctrl-C sends.
    Int,
    /// `SIGQUIT` — quit, core-dumping by default. Several runtimes dump every
    /// thread's stack on it instead.
    Quit,
    /// `SIGTERM` — the polite stop. Sending it here bypasses the stop ladder
    /// entirely: shep does not start a `kill_timeout`, does not escalate, and
    /// does not mark the sheep stopped. Use `shep stop` for a stop.
    Term,
    /// `SIGUSR1` — user-defined signal 1.
    Usr1,
    /// `SIGUSR2` — user-defined signal 2, the one several runtimes reserve for
    /// a graceful restart.
    Usr2,
    /// `SIGWINCH` — terminal resized. Harmless to nearly everything, which is
    /// what makes it the signal to test a wiring with.
    Winch,
    /// `SIGCONT` — continue a stopped process.
    Cont,
    /// `SIGKILL` — unblockable, immediate. The restart policy will see the
    /// exit as any other unexpected one and act on it: an app with
    /// `autorestart` on comes back.
    Kill,
}

impl OperatorSignal {
    /// Every spelling this grammar accepts, canonical form, in the order a
    /// refusal lists them.
    ///
    /// Public because it is rendered into the refusal an operator reads and
    /// into `shep signal --help`; a second hand-written list in either place
    /// is one free to drift.
    pub const ACCEPTED: [&'static str; 9] = [
        "SIGHUP", "SIGINT", "SIGQUIT", "SIGTERM", "SIGUSR1", "SIGUSR2", "SIGWINCH", "SIGCONT",
        "SIGKILL",
    ];

    /// Parses one signal name, case-insensitively, with or without the `SIG`
    /// prefix. `None` for anything else, including a raw number — a number
    /// means different signals on different platforms, and shep will not guess
    /// which one an operator meant.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "SIGHUP" | "HUP" => Some(Self::Hup),
            "SIGINT" | "INT" => Some(Self::Int),
            "SIGQUIT" | "QUIT" => Some(Self::Quit),
            "SIGTERM" | "TERM" => Some(Self::Term),
            "SIGUSR1" | "USR1" => Some(Self::Usr1),
            "SIGUSR2" | "USR2" => Some(Self::Usr2),
            "SIGWINCH" | "WINCH" => Some(Self::Winch),
            "SIGCONT" | "CONT" => Some(Self::Cont),
            "SIGKILL" | "KILL" => Some(Self::Kill),
            _ => None,
        }
    }

    /// The canonical name, always `SIG`-prefixed and uppercase.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hup => "SIGHUP",
            Self::Int => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Term => "SIGTERM",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
            Self::Winch => "SIGWINCH",
            Self::Cont => "SIGCONT",
            Self::Kill => "SIGKILL",
        }
    }
}
```

Run:

```bash
cargo test -p shep-core --lib --all-features
```

Expect green, **+5** tests.

### Step 3.3 — RED: the wire

Add to `crates/shep-core/src/protocol/request.rs`'s test module:

```rust
/// fails if a `Signal` frame stops carrying the signal name as plain text, or
/// if the outcome rows stop distinguishing their three cases. The name travels
/// as a `String` on purpose (`AppConfig::kill_signal` does the same): the wire
/// stays readable and the daemon re-validates, which it has to do anyway
/// because peer input is untrusted.
#[test]
fn a_signal_request_and_its_reply_round_trip() {
    let request = Request::Signal {
        selector: SelectorSpec::Name("web".to_string()),
        signal: "SIGHUP".to_string(),
    };
    let json = serde_json::to_string(&request).unwrap();
    assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);

    let reply = Response::Signalled(vec![
        SignalReply {
            id: 1,
            name: "web".to_string(),
            outcome: SignalOutcome::Delivered,
        },
        SignalReply {
            id: 2,
            name: "web".to_string(),
            outcome: SignalOutcome::NotRunning,
        },
        SignalReply {
            id: 3,
            name: "api".to_string(),
            outcome: SignalOutcome::Failed {
                reason: "no such process".to_string(),
            },
        },
    ]);
    let json = serde_json::to_string(&reply).unwrap();
    assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), reply);
    // The three tags, spelled out: a variant renamed in Rust changes these
    // strings mechanically, compiles clean, and breaks a client matching on
    // them with nothing to say why.
    assert!(json.contains(r#""kind":"delivered""#), "{json}");
    assert!(json.contains(r#""kind":"not_running""#), "{json}");
    assert!(json.contains(r#""kind":"failed""#), "{json}");
}
```

Run `cargo test -p shep-core --lib --all-features`.

**Expected failure — for the stated reason:** compile error, ``no variant named
`Signal` found for enum `Request` ``.

### Step 3.4 — GREEN: the frames

In `crates/shep-core/src/protocol/request.rs`, add to `Request` after
`Trigger`:

```rust
    /// Deliver one signal to every matched sheep's OWN process — never its
    /// process group (see `shep signal`).
    Signal {
        /// Which sheep. No default anywhere in the stack, matching every
        /// other verb that reaches a running process: an operator names the
        /// target rather than signal the whole flock by accident.
        selector: SelectorSpec,
        /// The signal's name, as
        /// [`OperatorSignal`](crate::signals::OperatorSignal) spells it — the
        /// `SIG` prefix and the case are both optional.
        ///
        /// A `String` rather than the enum, for the reason
        /// [`AppConfig::kill_signal`](crate::config::AppConfig::kill_signal)
        /// is one: the wire stays plain text a person can read in a capture,
        /// and the daemon re-validates regardless, because peer input is
        /// untrusted. A name outside the grammar answers
        /// [`RpcErrorCode::InvalidConfig`].
        signal: String,
    },
```

and to `Response`, after `Triggered`:

```rust
    /// Answer to `Signal` — one [`SignalReply`] row per matched sheep.
    ///
    /// Not a flock listing: what a caller wants back is per-instance delivery,
    /// and [`ProcessInfo`] has nowhere to hold it. Same reasoning, and the
    /// same row-shaped answer, as [`Self::Triggered`].
    Signalled(Vec<SignalReply>),
```

Then, beside `ActionOutcome`/`ActionReply`:

```rust
/// What happened when the shepherd tried to deliver one signal.
///
/// `#[non_exhaustive]`: a future outcome — a sheep refused because it is a dog,
/// say, or a delivery held while a stop ladder runs — must not need a protocol
/// version bump (IR-20).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SignalOutcome {
    /// The kernel accepted the signal for this sheep's pid.
    ///
    /// Says the signal was delivered, not that the app did anything with it.
    /// A signal the app blocks, ignores, or has no handler for is `Delivered`
    /// exactly like one it acts on — there is nothing on this path that could
    /// tell the difference, and pretending otherwise would be the dishonest
    /// half of an honest report.
    Delivered,
    /// The sheep is registered but has no live process to signal — stopped,
    /// errored, or waiting out a restart backoff.
    NotRunning,
    /// The kernel refused the delivery; carries its reason (`ESRCH` for a
    /// process reaped between the lookup and the syscall, `EPERM` for one this
    /// daemon may not signal).
    Failed {
        /// The refusal, as the OS worded it.
        reason: String,
    },
}

/// One matched sheep's row in a `Signal` reply.
///
/// Shaped exactly like [`ActionReply`] and for the same reason: spec §9's
/// selector grammar (`all`, `/regex/`, `fold:`) makes a mixed flock the normal
/// case, so a per-row outcome beats a whole-request refusal that would leave
/// the operator unable to tell which half was taken.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalReply {
    /// The sheep's stable id.
    pub id: u32,
    /// The sheep's name.
    pub name: String,
    /// What happened when the shepherd tried to deliver the signal.
    pub outcome: SignalOutcome,
}
```

Export both from `crates/shep-core/src/protocol/mod.rs` alongside `ActionReply`
and `ActionOutcome`.

Run `cargo test -p shep-core --lib --all-features`. Expect green, **+1**.

### Step 3.5 — GREEN: pin the frames

Add to `request_wire_snapshots`, after the `Trigger` envelope:

```rust
            // `SIGHUP` rather than `SIGTERM`: TERM is what the stop ladder
            // already sends, so a fixture using it could not tell a `signal`
            // frame from a stop's. HUP is the signal this verb exists for.
            Envelope {
                id: 12,
                deadline_ms: None,
                body: Request::Signal {
                    selector: SelectorSpec::Name("web".to_string()),
                    signal: "SIGHUP".to_string(),
                },
            },
```

(renumber to whatever the next free envelope id in that vector is — read it;
the ids are sequential and the last one is not `11` by guarantee.)

And to `reply_wire_snapshots`, one `Reply` carrying
`Response::Signalled(vec![…])` with all three outcomes, mirroring the
`Triggered` row already there.

Run, review the insta diff (**appended rows only, zero deletions**), accept
with `INSTA_UPDATE=always`, then:

```bash
git diff --stat crates/shep-core/src/protocol/snapshots/
find crates -name '*.snap.new' | wc -l      # 0 at HEAD, must be 0 after
```

### Step 3.6 — MUTATION

In `crates/shep-core/src/signals.rs`, change

```rust
            "SIGWINCH" | "WINCH" => Some(Self::Winch),
```

to

```rust
            "SIGWINCH" | "WINCH" => Some(Self::Cont),
```

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `every_accepted_name_round_trips_through_parse` fails on
`SIGWINCH` — it parses, but `as_str` gives `SIGCONT` back.
`the_prefix_and_the_case_are_both_optional` also fails on its `WINCH` row.
`sigstop_is_refused_because_the_shepherd_could_not_report_it` and
`a_name_outside_the_table_does_not_parse` must stay **green**: this mutation
does not widen the grammar, and tests that redden on it would be measuring
"parse returns Some" rather than which signal.

Revert.

### Step 3.7 — CHANGELOG and gate

`crates/shep-core/CHANGELOG.md` → `Additions`: "`signals::OperatorSignal`, the
nine signals `shep signal` may name; `Request::Signal` /
`Response::Signalled` / `SignalReply` / `SignalOutcome`."

Then the full task gate.

---

## Task 4 — the shepherd delivers a signal, and `shep signal`

**Builds:** `shep signal <selector> <signal>`, end to end. Closes half of
`deferred.md`'s **`scale`, `signal`, `sendline`** entry.

**Files modified:**
- `crates/shep-daemon/src/runner.rs` — `RunningProcess::signal_process`.
- `crates/shep-daemon/src/tokio_runner.rs` — the real implementation.
- `crates/shep-daemon/src/fake.rs` — the scripted implementation + a reader.
- `crates/shep-daemon/src/supervisor.rs` — the second sheep-task mailbox,
  `Command::Signal`, `SupervisorHandle::signal`.
- `crates/shep-daemon/src/rpc.rs` — the `Request::Signal` arm.
- `crates/shep-cli/src/cli.rs` — the `Signal` subcommand + `SignalArgs`.
- `crates/shep-cli/src/commands/signal.rs` — new.
- `crates/shep-cli/src/commands/mod.rs`, `crates/shep-cli/src/main.rs` — wiring.
- `crates/shep-cli/src/output/rows.rs` — `SignalledRows`.
- `crates/shep-daemon/tests/real_runner.rs` — one real-child case.

**Consumes from Task 3:** `OperatorSignal`, `Request::Signal`,
`Response::Signalled`, `SignalReply`, `SignalOutcome`.

### Step 4.0 — read this before writing the mailbox

`SheepSlot::to_child`'s own doc (`supervisor.rs:1252`) records an invariant this
task can break:

> That mailbox holds [`SHEEP_CTL_CAPACITY`] messages, [`Self::ctl`]'s senders
> `try_send` into it, and [`Actor::claim_manual`] ignores a `Full` there
> *because a queued [`SheepCtl::Kill`] means the ladder is already running*.
> That argument holds only while `Kill` is the sole occupant of those four
> slots: put anything else in them and the same code can drop a `Kill`.

So **do not add a `SheepCtl::Signal` variant.** A burst of signals would fill
the mailbox, `claim_manual`'s `try_send` would come back `Full`, and it would
read that as "the ladder is already running" and drop a stop that never
happened. Signals get a mailbox of their own, on the same task.

### Step 4.1 — RED: the runner seam

Add to `crates/shep-daemon/src/fake.rs`'s test module:

```rust
/// fails if a signal aimed at one sheep is recorded as a group delivery, or
/// not recorded at all. `signal` and `signal_process` are two different
/// contracts against the same OS primitive, and a fake that answered both from
/// one counter could not tell a reviewer which one the supervisor called.
#[tokio::test(start_paused = true)]
async fn a_process_signal_is_recorded_apart_from_a_group_signal() {
    let runner = ScriptedRunner::new(vec![ProcScript::runs_forever()]);
    let (mut proc, _io) = runner.spawn(&spec_for("web")).unwrap();

    proc.signal_process(OperatorSignal::Hup).unwrap();
    proc.signal_process(OperatorSignal::Usr1).unwrap();

    assert_eq!(
        runner.process_signals(0),
        vec![OperatorSignal::Hup, OperatorSignal::Usr1]
    );
    assert!(
        runner.signals(0).is_empty(),
        "a per-process signal must not be counted as a group signal"
    );
}
```

(`spec_for` — reuse whatever spec helper that module's tests already use; read
them first.)

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Expected failure — for the stated reason:** compile error, ``no method named
`signal_process` found for struct `FakeProc` ``.

### Step 4.2 — GREEN: the seam, defaulted so it is additive

In `crates/shep-daemon/src/runner.rs`, add to `trait RunningProcess`, after
`signal`:

```rust
    /// Sends `sig` to this sheep's OWN process, never its process group.
    ///
    /// The counterpart to [`Self::signal`], and the difference between them is
    /// the whole design of `shep signal`. That one is group-wide because a
    /// stop has to reach a `thing & wait` wrapper's child too. This one is not,
    /// because it exists for a conversation between an operator and one
    /// application: a `SIGHUP` broadcast to every process in the group reaches
    /// whatever `sh` and whatever runtime child happen to be in it, none of
    /// which the operator addressed and several of which have their own
    /// meaning for the signal.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] — delivery failed (`ESRCH` for a
    ///   process reaped between the lookup and the syscall, `EPERM` for one
    ///   this daemon may not signal), or this implementation has no per-process
    ///   delivery at all.
    ///
    /// # Default implementation
    ///
    /// Refuses. A defaulted method rather than a required one so that adding
    /// it did not break an out-of-tree implementor of this trait, which is a
    /// `pub` trait in a published library — the same courtesy `#[non_exhaustive]`
    /// buys an enum (IR-20). An implementation that can deliver a signal to one
    /// process overrides it; one that cannot says so honestly instead of
    /// silently widening to the group.
    fn signal_process(&mut self, sig: OperatorSignal) -> Result<(), RunnerError> {
        let _ = sig;
        Err(RunnerError::SignalFailed(
            "this runner cannot signal a single process".to_string(),
        ))
    }
```

with `use shep_core::signals::OperatorSignal;` at the top of the module.

In `crates/shep-daemon/src/fake.rs`, on `impl RunningProcess for FakeProc`:

```rust
    // Recorded on its own list, not `record_signal`'s. A scripted proc models
    // one process with no descendants, so the two deliveries are
    // indistinguishable in what they REACH here — but they are entirely
    // distinguishable in which one the supervisor called, and that is the fact
    // a test of `shep signal` needs. Notably this does NOT resolve the wait:
    // `signal` does (it is the stop ladder's polite rung and the scripted proc
    // obeys it), and a nudge that killed the sheep would make every case here
    // vacuous.
    fn signal_process(&mut self, sig: OperatorSignal) -> Result<(), RunnerError> {
        self.state.record_process_signal(sig);
        Ok(())
    }
```

with `process_signals: Mutex<Vec<OperatorSignal>>` on `ProcState`, a
`record_process_signal` beside `record_signal`, and a
`ScriptedRunner::process_signals(&self, spawn_index: usize) -> Vec<OperatorSignal>`
reader beside the existing `signals`.

In `crates/shep-daemon/src/tokio_runner.rs`, on `impl RunningProcess for TokioProc`:

```rust
    fn signal_process(&mut self, sig: OperatorSignal) -> Result<(), RunnerError> {
        // POSITIVE pid, unlike `signal_group`'s negative one. That single
        // character is the difference between the two contracts, so it gets
        // its own function rather than a boolean on the existing one.
        let pid = i32::try_from(self.pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| {
                RunnerError::SignalFailed(format!(
                    "pid {} is not a signallable process id",
                    self.pid
                ))
            })?;
        signal::kill(Pid::from_raw(pid), to_nix_operator_signal(sig))
            .map_err(|error| RunnerError::SignalFailed(error.to_string()))
    }
```

and, beside `to_nix_signal`:

```rust
/// Maps [`OperatorSignal`] to the nix [`Signal`] it names.
///
/// An explicit match, like [`to_nix_signal`], rather than a numeric
/// conversion: shep-core deliberately holds no raw signal numbers (they differ
/// by platform — `SIGUSR1` is 10 on Linux and 30 on macOS), so this is the one
/// place in the workspace where the two vocabularies meet, and an unmapped
/// variant must be a compile error rather than a runtime one.
fn to_nix_operator_signal(sig: OperatorSignal) -> Signal {
    match sig {
        OperatorSignal::Hup => Signal::SIGHUP,
        OperatorSignal::Int => Signal::SIGINT,
        OperatorSignal::Quit => Signal::SIGQUIT,
        OperatorSignal::Term => Signal::SIGTERM,
        OperatorSignal::Usr1 => Signal::SIGUSR1,
        OperatorSignal::Usr2 => Signal::SIGUSR2,
        OperatorSignal::Winch => Signal::SIGWINCH,
        OperatorSignal::Cont => Signal::SIGCONT,
        OperatorSignal::Kill => Signal::SIGKILL,
    }
}
```

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.
Expect green, **+1**.

### Step 4.3 — RED: the supervisor delivers

Add to `crates/shep-daemon/src/supervisor.rs`'s test module:

```rust
/// fails if `signal` reaches the group instead of the process, or reaches
/// nothing. The group assertion is the load-bearing half — a supervisor that
/// called `signal` rather than `signal_process` would look correct in every
/// other respect and would deliver SIGHUP to every lamb.
#[tokio::test(start_paused = true)]
async fn a_signal_reaches_the_sheeps_own_process_and_not_its_group() {
    let runner = SharedRunner::new(vec![ProcScript::runs_forever()]);
    let h = harness_with_runner(runner.clone());
    let id = register_sheep(&h, AppConfig::minimal("web", "./srv")).await;

    let rows = h
        .ctx
        .supervisor
        .signal(ProcessSelector::Id(id), OperatorSignal::Hup)
        .await
        .unwrap();

    assert_eq!(
        rows,
        vec![SignalReply {
            id,
            name: "web".to_string(),
            outcome: SignalOutcome::Delivered,
        }]
    );
    assert_eq!(runner.process_signals(0), vec![OperatorSignal::Hup]);
    assert!(
        runner.signals(0).is_empty(),
        "shep signal must not reach the process group"
    );
}

/// fails if a registered-but-dead sheep is reported as delivered. `Delivered`
/// is the only outcome that claims the kernel took the signal, so a stopped
/// sheep answering it would be the report lying about the one thing it says.
#[tokio::test(start_paused = true)]
async fn a_stopped_sheep_answers_not_running_rather_than_delivered() {
    let h = harness(vec![ProcScript::runs_forever()]);
    let id = register_sheep(&h, AppConfig::minimal("web", "./srv")).await;
    h.ctx
        .supervisor
        .stop(ProcessSelector::Id(id))
        .await
        .unwrap();

    let rows = h
        .ctx
        .supervisor
        .signal(ProcessSelector::Id(id), OperatorSignal::Hup)
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, SignalOutcome::NotRunning);
}

/// fails if a selector matching nothing is answered with an empty success. It
/// is `NotFound`, exactly as it is for every other selector-taking verb —
/// `shep signal typo SIGHUP` exiting 0 would be the worst possible answer.
#[tokio::test(start_paused = true)]
async fn a_selector_that_matches_nothing_is_not_found() {
    let h = harness(vec![]);
    let err = h
        .ctx
        .supervisor
        .signal(ProcessSelector::Name("ghost".to_string()), OperatorSignal::Hup)
        .await
        .unwrap_err();
    assert_eq!(err, SupervisorError::NotFound);
}

/// fails if a reload drainee is skipped. `trigger` skips one because an action
/// expects a reply from a process on its way out; a signal expects nothing
/// back, and the drainee is a live process the operator's selector matched.
/// Holding it back would be a silent refusal with no channel in which to
/// explain itself.
#[tokio::test(start_paused = true)]
async fn a_reload_drainee_is_signalled_like_any_other_live_sheep() {
    let (h, runner, drainee) = actor_with_stopping_drainee().await;

    let rows = h
        .ctx
        .supervisor
        .signal(ProcessSelector::Id(drainee), OperatorSignal::Hup)
        .await
        .unwrap();

    assert_eq!(rows[0].outcome, SignalOutcome::Delivered);
    assert!(runner.process_signals(0).contains(&OperatorSignal::Hup));
}
```

`harness_with_runner` / `SharedRunner` / `actor_with_stopping_drainee` all
exist in that module (`actor_with_stopping_drainee` at line 6323); adapt names
and return shapes to what is actually there rather than to these sketches.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Expected failure — for the stated reason:** compile error, ``no method named
`signal` found for struct `SupervisorHandle` ``.

### Step 4.4 — GREEN: the second mailbox, and the command

**The mailbox.** In `crates/shep-daemon/src/supervisor.rs`:

```rust
/// One signal delivery a sheep task is asked to perform, plus where the answer
/// goes.
///
/// A mailbox of its own rather than a [`SheepCtl`] variant, and this is not
/// tidiness. `SheepCtl`'s queue is bounded at [`SHEEP_CTL_CAPACITY`],
/// [`Actor::claim_manual`] `try_send`s into it, and it treats a `Full` there as
/// proof that a kill ladder is already running — an argument that holds only
/// while `Kill` is the sole occupant of those slots (see
/// [`SheepSlot::to_child`]'s own doc, which says so in as many words). A burst
/// of signals sharing that queue would make `claim_manual` drop a stop and
/// report success for it.
#[derive(Debug)]
struct SignalRequest {
    /// What to deliver, to this sheep's own pid.
    sig: OperatorSignal,
    /// Fires with what the delivery came to. A dropped sender means the sheep
    /// task ended between the send and the delivery, which the caller reads as
    /// the sheep no longer running.
    done: oneshot::Sender<Result<(), RunnerError>>,
}
```

`spawn_sheep_task` returns both senders:

```rust
/// The two mailboxes a live sheep task listens on.
struct SheepHandles {
    /// The kill ladder's, whose one-message-kind invariant is documented on
    /// [`SignalRequest`].
    ctl: mpsc::Sender<SheepCtl>,
    /// Signal deliveries.
    signals: mpsc::Sender<SignalRequest>,
}
```

`SheepSlot` gains `signals: Option<mpsc::Sender<SignalRequest>>`, set on every
successful spawn and **cleared exactly where `to_child` is cleared** — for the
same reason `to_child`'s own doc gives: the receiving task parks on `recv()`
and a sender left on a dead slot parks it for as long as the sheep stays
registered.

`run_sheep` grows a fourth `select!` branch, guarded the same way the others
are:

```rust
            maybe_signal = signal_rx.recv(), if signals_open => {
                match maybe_signal {
                    Some(SignalRequest { sig, done }) => {
                        // Delivered from the task that OWNS the proc, never
                        // from the actor off a recorded pid. The owning task
                        // is the only place that knows the child has not been
                        // reaped, which is what closes the pid-reuse ABA race
                        // the same way `RunningProcess::signal` already does
                        // for the stop ladder.
                        let _ = done.send(proc.signal_process(sig));
                    }
                    None => signals_open = false,
                }
            }
```

**The command.** `Command::Signal { selector, sig, reply }`, with an actor
handler shaped on `begin_action` (`supervisor.rs:3823`) — collect matched ids,
`NotFound` on none, one row per id, and the deliveries fanned out off a task of
its own rather than on the actor loop:

```rust
    /// Delivers one signal to every matched sheep's own process.
    ///
    /// Off the actor loop, like [`Self::begin_action`] and for the same
    /// reason: each delivery is a round trip through a sheep task, and the
    /// actor must not park on one. Unlike an action there is nothing to wait
    /// out — a `kill(2)` either returns or does not — so the fan-out here is
    /// bounded by the syscall, not by a configured timeout.
    ///
    /// A sheep with no live task answers [`SignalOutcome::NotRunning`] without
    /// a round trip at all: `slot.signals` is `None` for exactly the states
    /// that have no process (`Stopped`, `Errored`, `WaitingRestart`).
    fn begin_signal(
        &mut self,
        selector: &ProcessSelector,
        sig: OperatorSignal,
        reply: oneshot::Sender<Result<Vec<SignalReply>, SupervisorError>>,
    ) {
        let matched = self.matching_ids(selector);
        if matched.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }

        let mut settled = Vec::new();
        let mut waits = Vec::new();
        for id in matched {
            let slot = self
                .sheep
                .get(&id)
                .expect("begin_signal: `matched` holds ids read off this map a moment ago");
            let name = slot.entry.spec.config().name.clone();
            let Some(signals) = slot.signals.clone() else {
                settled.push(SignalReply {
                    id,
                    name,
                    outcome: SignalOutcome::NotRunning,
                });
                continue;
            };
            let (done, answer) = oneshot::channel();
            if signals.try_send(SignalRequest { sig, done }).is_err() {
                // A full queue means this sheep's task has not drained several
                // signals yet, which for a syscall-fast handler means it is
                // busy dying; a closed one means it already has. Both are
                // "there is no process here to signal", reported as the
                // refusal it is rather than as a delivery.
                settled.push(SignalReply {
                    id,
                    name,
                    outcome: SignalOutcome::NotRunning,
                });
                continue;
            }
            waits.push((id, name, answer));
        }

        spawn_signal_task(settled, waits, reply);
    }
```

with `spawn_signal_task` mirroring `spawn_trigger_task`: await every `answer`,
map `Ok(Ok(()))` → `Delivered`, `Ok(Err(err))` → `Failed { reason:
err.to_string() }`, and a **dropped sender** (`Err(RecvError)`) → `NotRunning`
— the task ended before it could deliver, which means the process did too.
Sort the rows by id, as the trigger task does, so the answer's order is the
selector's and not the scheduler's.

`SupervisorHandle::signal(selector, sig)` is the crate-private wrapper, shaped
exactly like `trigger`, with a `# Errors` section naming `NotFound` and
`EngineStopped`.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.
Expect green, **+4**.

### Step 4.5 — GREEN: the RPC arm

In `crates/shep-daemon/src/rpc.rs`, after the `Request::Trigger` arm:

```rust
        Request::Signal { selector, signal } => {
            // Re-validated here even though the CLI validated it: peer input is
            // untrusted, and this is the same rule `Request::Start`'s own
            // `normalize_all` follows a few arms up.
            let Some(sig) = OperatorSignal::parse(&signal) else {
                return reply(Err(RpcError {
                    code: RpcErrorCode::InvalidConfig,
                    message: format!(
                        "`{signal}` is not a signal shep will send; accepted: {}",
                        OperatorSignal::ACCEPTED.join(", ")
                    ),
                }));
            };
            match selector_of(selector) {
                Ok(selector) => match ctx.supervisor.signal(selector, sig).await {
                    Ok(rows) => reply(Ok(Response::Signalled(rows))),
                    Err(err) => reply(Err(rpc_error(&err))),
                },
                Err(err) => reply(Err(err)),
            }
        }
```

Match the arm's surrounding style — read the `Trigger` arm (line 311) and
follow its exact `selector_of` / `reply` shape rather than the sketch above.

Add a dispatch-tier test in that module's own `#[cfg(test)] mod tests`:

```rust
/// fails if a bad signal name reaches the supervisor. It must be refused at the
/// dispatch boundary with `InvalidConfig`, not turned into a `NotFound` or an
/// `Internal` deeper in — an operator who typed `SIGHUPP` needs the accepted
/// list, and only this arm has it.
#[tokio::test]
async fn a_signal_name_outside_the_grammar_is_refused_with_the_accepted_list() {
    let h = harness(vec![]);
    let reply = reply_of(
        dispatch(
            envelope(
                1,
                Request::Signal {
                    selector: SelectorSpec::All,
                    signal: "SIGHUPP".to_string(),
                },
            ),
            &h.ctx,
        )
        .await,
    );
    let err = reply.result.unwrap_err();
    assert_eq!(err.code, RpcErrorCode::InvalidConfig);
    assert!(err.message.contains("SIGHUPP"), "{}", err.message);
    assert!(err.message.contains("SIGHUP"), "{}", err.message);
    assert!(err.message.contains("SIGUSR2"), "{}", err.message);
}
```

Note the first two asserts are not redundant: `SIGHUPP` contains `SIGHUP`, so
the second alone would pass on a message that only echoed the input. The third
is what proves the list is really there.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.
Expect green, **+1**.

### Step 4.6 — GREEN: a real child takes a real SIGUSR1

Add to `crates/shep-daemon/tests/real_runner.rs`:

```rust
/// A real child, a real `kill(2)`, and the one assertion the scripted tier
/// cannot make: the signal reached the sheep and NOT the lamb it forked.
///
/// The wrapper traps SIGUSR1 and prints one word; the lamb traps it and prints
/// another. A group delivery prints both. Bounded (IR-46): without the timeout
/// a signal that reached nobody would hang the read rather than fail it.
#[tokio::test]
async fn a_process_signal_reaches_the_sheep_and_not_its_lamb() {
    // The lamb arms its trap and announces itself, so the test can wait for it
    // to be READY rather than racing the fork. Without that line the signal can
    // land before `trap` runs in the subshell and the assertion passes for the
    // wrong reason.
    let script = r#"
        trap 'echo sheep-got-it' USR1
        ( trap 'echo lamb-got-it' USR1; echo lamb-ready; while :; do sleep 0.1; done ) &
        while :; do sleep 0.1; done
    "#;
    let mut spec = spec_for_program("/bin/sh", &["-c", script]);
    spec.stdin = false;
    let runner = TokioRunner;
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();

    // Wait for the lamb's trap to be armed.
    let ready = tokio::time::timeout(Duration::from_secs(10), io.logs.recv())
        .await
        .expect("the lamb did not announce itself within 10s")
        .expect("log channel closed");
    assert_eq!(ready.line, "lamb-ready");

    proc.signal_process(OperatorSignal::Usr1).unwrap();

    let answer = tokio::time::timeout(Duration::from_secs(10), io.logs.recv())
        .await
        .expect("nothing answered the signal within 10s")
        .expect("log channel closed");
    assert_eq!(answer.line, "sheep-got-it");

    // And nothing else follows it. A group delivery would put `lamb-got-it` on
    // the same stream; a bounded read that times out is the proof it did not.
    let extra = tokio::time::timeout(Duration::from_secs(2), io.logs.recv()).await;
    assert!(
        extra.is_err(),
        "the lamb answered too: {extra:?} — the signal reached the group"
    );

    proc.kill_tree().unwrap();
}
```

Two notes for the implementer:

- `spec_for_program` is a stand-in for whatever that file's own spec helper is
  called; read it first. The `stdin = false` line is explicit rather than
  defaulted so this case does not silently change meaning when Task 9 lands.
- The negative assertion is a **bounded read that must time out**, which is the
  one place in this plan where a timeout expiring is the success case. That is
  legitimate here and only here: there is no event that means "the lamb did not
  get it", so waiting a bounded interval for one is the only shape available.
  The two seconds is against a `trap` handler that fires in microseconds.

Run `cargo test -p shep-daemon --test real_runner --all-features`. Expect
green, **+1**.

> If `sh`'s `trap` handling on the CI macOS runner makes this flaky, the
> fallback that still fails honestly is to have the lamb write to a file and
> assert the file is absent after a bounded wait. Do not weaken it to "the
> sheep got it" alone — the not-the-lamb half is the entire point.

### Step 4.7 — GREEN: the verb

`crates/shep-cli/src/cli.rs`, in `Commands` after `Trigger`:

```rust
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
```

```rust
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
```

`crates/shep-cli/src/commands/signal.rs` — copy `trigger.rs`'s shape exactly
(it is the closest sibling: selector + extra positional + per-row outcome
table), with these differences:

- validate the signal name **locally** before the round trip:

```rust
    let Some(sig) = OperatorSignal::parse(&args.signal) else {
        let message = format!(
            "`{}` is not a signal shep will send; accepted: {}",
            args.signal,
            OperatorSignal::ACCEPTED.join(", ")
        );
        let _ = emit_error(&mut *streams.err, fmt, ExitCode::Usage.code_str(), &message);
        return ExitCode::Usage;
    };
```

  Local first, exactly as `parse_selector` is: a malformed argument is a usage
  error the operator caused, and it should cost neither a connection nor a
  daemon round trip. The daemon re-validates anyway (Step 4.5) because a client
  is not the only thing that can send a frame.

- send the **canonical** spelling, `sig.as_str().to_string()`, not
  `args.signal`. So the wire carries `SIGHUP` whether the operator typed `hup`,
  `Hup` or `SIGHUP`, and a packet capture reads the same for all three.
- the client's plain default deadline is right here: nothing on this path waits
  on an app. No `TRIGGER_DEADLINE` equivalent.

`crates/shep-cli/src/output/rows.rs` — `SignalledRows(pub Vec<SignalReply>)`
with headers `["ID", "NAME", "OUTCOME", "DETAIL"]`, `describe_signal_outcome`
mapping `Delivered` → `("delivered", String::new())`, `NotRunning` →
`("not_running", "no live process to signal".to_string())`, `Failed { reason }`
→ `("failed", reason.clone())`, and a **wildcard arm** rendering an unknown
variant as `("unknown", format!("{outcome:?}"))` — `SignalOutcome` is
`#[non_exhaustive]`, so this client must compile against a future one, exactly
as `describe_outcome` already does for `ActionOutcome`.

Wire `Commands::Signal` in `main.rs` next to `Commands::Trigger`, and
`pub mod signal;` in `commands/mod.rs`.

Port the four unit tests from `trigger.rs`'s own test module, adapted: a
malformed selector never reaches the wire; a bad signal name never reaches the
wire and exits `Usage`; the envelope carries the canonical spelling; an
unrecognised response exits `Internal`.

Run:

```bash
cargo test -p shep-cli --bins --all-features
```

Expect green, **+4**.

### Step 4.8 — GREEN: `shep signal` end to end

Add one case to `crates/shep-cli/tests/cli_e2e.rs`, following whatever verb
there is closest in shape: start a sheep, `shep signal <name> SIGWINCH`, assert
exit 0 and one `delivered` row. `SIGWINCH` is the right signal for an e2e —
harmless to essentially everything, so the assertion is about delivery and not
about what the child did.

Run `cargo test -p shep-cli --test cli_e2e --all-features`. Expect green,
**+1**.

### Step 4.9 — MUTATION

Two, and both must be run.

**One:** in `crates/shep-daemon/src/supervisor.rs`'s `run_sheep` signal branch,
change `proc.signal_process(sig)` to `proc.signal(StopSignal::Term)`.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Must go red:** `a_signal_reaches_the_sheeps_own_process_and_not_its_group`
fails on `runner.signals(0).is_empty()` — the group list is no longer empty.
That is the assertion that exists for this exact mutation; if it stays green
the test is only proving something was signalled.

**Two:** in `crates/shep-cli/src/commands/signal.rs`, change the request body's
`signal: sig.as_str().to_string()` to `signal: args.signal.clone()`.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** the envelope test fails — a lowercase `hup` typed by the
operator now goes on the wire as `hup` rather than `SIGHUP`. If it stays green,
that test is feeding it an already-canonical name and needs its input changed
to a lowercase one before it can prove anything.

Revert both.

### Step 4.10 — CHANGELOGs and gate

`crates/shep-daemon/CHANGELOG.md` → `Additions`: "`RunningProcess::signal_process`
(defaulted, so it is additive for an out-of-tree implementor) and
`Request::Signal` — one signal to one sheep's own process, not its group."

`crates/shep-cli/CHANGELOG.md` → `Additions`: "`shep signal <selector> <signal>`."

Then the full task gate.

---

## Task 5 — the `Scale` wire

**Files modified:**
- `crates/shep-core/src/protocol/request.rs` — `Request::Scale`,
  `Response::Scaled`, fixtures.

**Produces, for Tasks 6 and 7:**

```rust
Request::Scale { name: String, count: u32 }
Response::Scaled(Vec<ProcessInfo>)
```

### Step 5.1 — RED

Add to `crates/shep-core/src/protocol/request.rs`'s test module:

```rust
/// fails if `Scale` grows a selector. It takes an app NAME, and that is the
/// design: `instances` is a per-app number and instance slots are allocated
/// per name-group, so `shep scale /web.*/ 4` would have to mean either four
/// each or four total and there is no reading of it that is not a guess.
#[test]
fn a_scale_request_names_one_app_and_a_count() {
    let request = Request::Scale {
        name: "web".to_string(),
        count: 4,
    };
    let json = serde_json::to_string(&request).unwrap();
    assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    assert!(json.contains(r#""kind":"scale""#), "{json}");
    assert!(json.contains(r#""name":"web""#), "{json}");
    // No `selector` key at all — the shape that says this verb is not one of
    // the selector-taking family.
    assert!(!json.contains("selector"), "{json}");
}

/// fails if `Scaled` stops being distinguishable from the eight other replies
/// carrying a bare `Vec<ProcessInfo>`. Each of those names which request it
/// answers precisely so it can diverge later without a protocol bump — the
/// enum's own doc says not to collapse them, and this is the test that notices.
#[test]
fn a_scaled_reply_carries_its_own_tag() {
    let json = serde_json::to_string(&Response::Scaled(vec![])).unwrap();
    assert_eq!(json, r#"{"kind":"scaled","data":[]}"#);
}
```

Run `cargo test -p shep-core --lib --all-features`.

**Expected failure — for the stated reason:** compile error, ``no variant named
`Scale` found for enum `Request` ``.

### Step 5.2 — GREEN

`Request`, after `Delete`:

```rust
    /// Set how many instances one app runs (see `shep scale`).
    ///
    /// # Why a name and not a selector
    ///
    /// Every other verb here takes a [`SelectorSpec`], and this one
    /// deliberately does not. `instances` is a per-app number and instance
    /// slots are allocated against the same-name group
    /// (`shep_daemon::assemble::instance_slots`), so a selector matching two
    /// apps would have to mean either "four of each" or "four in total", and
    /// neither reading is more obviously right than the other. A name has one
    /// meaning.
    ///
    /// # Why absolute and not a delta
    ///
    /// There is no `+N`/`-N` form and there will not be one. An absolute count
    /// is idempotent — run it twice, get the same flock — where two operators
    /// sending `+2` against the same app get a number neither of them asked
    /// for. This project's own trace notes also record a crash on pm2's
    /// relative-remove path, and those notes exist so shep does not reproduce
    /// what they record.
    Scale {
        /// The app's name, exactly as its config spells it. Not a selector: no
        /// `all`, no regex, no `fold:`.
        name: String,
        /// How many instances the app has when this returns. `0` is refused
        /// with [`RpcErrorCode::InvalidConfig`] — `normalize` rejects
        /// `instances == 0` for every other path into the daemon, and `shep
        /// delete` is the verb for removing an app.
        count: u32,
    },
```

`Response`, after `Reloading`:

```rust
    /// Answer to `Scale` — the app's instances that will REMAIN, one row each,
    /// in instance-slot order.
    ///
    /// Scaling up, these are the instances that exist, the new ones included,
    /// and the answer is complete.
    ///
    /// Scaling down, these are the survivors and the departing instances are
    /// deliberately absent, even though they are still running their kill
    /// ladders as this reply is written. The operator asked for a number; this
    /// is that number of rows. Listing the departing ones as well would answer
    /// a `scale web 2` with four rows, which is the one thing the reply must
    /// not do. The departures report themselves on the bus as `process.delete`
    /// — the same split `Reloading` already makes between an acceptance and
    /// the swaps that follow it.
    Scaled(Vec<ProcessInfo>),
```

Run. Expect green, **+2**.

### Step 5.3 — GREEN: pin it

Add to `request_wire_snapshots` an `Envelope` carrying
`Request::Scale { name: "web".to_string(), count: 4 }`, and to
`reply_wire_snapshots` a `Reply` carrying `Response::Scaled(vec![sample_info()])`,
with a comment on the request row noting that this is the one verb in the enum
whose body has no `selector` key, so a reader comparing it against `stop`'s row
sees the whole difference in one place.

Review the insta diff (appended rows only, **zero deletions**), accept with
`INSTA_UPDATE=always`, then:

```bash
git diff --stat crates/shep-core/src/protocol/snapshots/
find crates -name '*.snap.new' | wc -l      # 0 at HEAD, must be 0 after
```

### Step 5.4 — MUTATION

Change `Scaled(Vec<ProcessInfo>)`'s serde tag by adding
`#[serde(rename = "reloading")]` to it.

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `a_scaled_reply_carries_its_own_tag` fails on the exact string,
and the `reply_wire_v1` snapshot fails with a diff on that row. Two independent
failures for one mutation is the shape wanted here — the enum's own doc warns
against collapsing these variants, and one test alone would be one edit away
from not noticing.

Revert.

### Step 5.5 — CHANGELOG and gate

`crates/shep-core/CHANGELOG.md` → `Additions`: "`Request::Scale` /
`Response::Scaled` — set an app's instance count."

Then the full task gate.

---

## Task 6 — the supervisor scales an app

**Files modified:**
- `crates/shep-daemon/src/supervisor.rs` — `Command::Scale`, `Actor::handle_scale`,
  `SupervisorHandle::scale`, `Scaled`, `SupervisorError::InvalidScale`.
- `crates/shep-daemon/src/rpc.rs` — the `Request::Scale` arm, and the registry
  re-record.

**Consumes from Task 5:** `Request::Scale`, `Response::Scaled`.

**Produces, for Task 7:**

```rust
// crates/shep-daemon/src/supervisor.rs
pub(crate) struct Scaled {
    pub(crate) instances: Vec<ProcessInfo>,
    pub(crate) app: ResolvedApp,
}
impl SupervisorHandle {
    pub(crate) async fn scale(&self, name: &str, count: u32) -> Result<Scaled, SupervisorError>;
}
SupervisorError::InvalidScale(String)   // -> RpcErrorCode::InvalidConfig
```

### Step 6.1 — RED

Add to `crates/shep-daemon/src/supervisor.rs`'s test module:

```rust
/// fails if scaling up does not take the lowest free slots. Slot numbers are
/// visible to the app (`SHEP_INSTANCE`) and to the filesystem
/// (`web-2-out.log`), so which ones a scale hands out is a contract, not an
/// implementation detail.
#[tokio::test(start_paused = true)]
async fn scaling_up_fills_the_lowest_free_slots() {
    let h = harness(vec![ProcScript::runs_forever(); 4]);
    let mut app = AppConfig::minimal("web", "./srv");
    app.instances = 2;
    register_app(&h, app).await;

    let scaled = h.ctx.supervisor.scale("web", 4).await.unwrap();

    assert_eq!(scaled.instances.len(), 4);
    assert_eq!(instance_slots_of(&h, "web").await, vec![0, 1, 2, 3]);
}

/// fails if scaling down takes the LOWEST slots. Taking the highest is what
/// makes 2 -> 4 -> 2 a round trip back to slots 0 and 1; taking the lowest
/// would leave 2 and 3 — the same count, a different flock, different log
/// files, and a different SHEP_INSTANCE for every survivor.
#[tokio::test(start_paused = true)]
async fn scaling_down_removes_the_highest_slots_so_a_round_trip_returns() {
    let h = harness(vec![ProcScript::runs_forever(); 4]);
    let mut app = AppConfig::minimal("web", "./srv");
    app.instances = 2;
    register_app(&h, app).await;

    h.ctx.supervisor.scale("web", 4).await.unwrap();
    let scaled = h.ctx.supervisor.scale("web", 2).await.unwrap();

    assert_eq!(scaled.instances.len(), 2);
    // Settle the two kill ladders the scale-down started.
    settle(&h).await;
    assert_eq!(instance_slots_of(&h, "web").await, vec![0, 1]);
}

/// fails if a scale forgets to write the new count back onto the app. Without
/// this, `shep scale web 4 && shep save` records `instances = 2` and the next
/// reboot silently reverts the scale — the bug is invisible until the machine
/// comes back.
#[tokio::test(start_paused = true)]
async fn a_scale_updates_the_stored_instance_count() {
    let h = harness(vec![ProcScript::runs_forever(); 4]);
    let mut app = AppConfig::minimal("web", "./srv");
    app.instances = 2;
    register_app(&h, app).await;

    let scaled = h.ctx.supervisor.scale("web", 4).await.unwrap();

    assert_eq!(scaled.app.config().instances, 4);
    // And on every surviving slot, not only on the returned copy: `respawn`
    // reassembles from the slot's own stored spec, so a slot left at 2 would
    // keep saying 2 to everything that reads it.
    assert!(
        stored_instance_counts(&h, "web").await.iter().all(|n| *n == 4),
        "a slot kept the pre-scale count"
    );
}

/// fails if scaling to the count an app already has does anything at all.
/// Idempotence is the whole argument for an absolute count over a delta, and
/// an operator re-running a provisioning script must not restart the flock.
#[tokio::test(start_paused = true)]
async fn scaling_to_the_current_count_is_a_no_op() {
    let h = harness(vec![ProcScript::runs_forever(); 2]);
    let mut app = AppConfig::minimal("web", "./srv");
    app.instances = 2;
    register_app(&h, app).await;
    let before = h.ctx.supervisor.list().await;

    let scaled = h.ctx.supervisor.scale("web", 2).await.unwrap();

    assert_eq!(scaled.instances.len(), 2);
    let after = h.ctx.supervisor.list().await;
    assert_eq!(
        after.iter().map(|i| i.id).collect::<Vec<_>>(),
        before.iter().map(|i| i.id).collect::<Vec<_>>(),
        "a no-op scale replaced processes"
    );
}

/// fails if `scale <name> 0` is accepted. `normalize` refuses `instances == 0`
/// on every other path into the daemon, so accepting it here would put a config
/// through the engine that the engine's own validator rejects.
#[tokio::test(start_paused = true)]
async fn scaling_to_zero_is_refused_and_names_delete() {
    let h = harness(vec![ProcScript::runs_forever()]);
    register_app(&h, AppConfig::minimal("web", "./srv")).await;

    let err = h.ctx.supervisor.scale("web", 0).await.unwrap_err();

    let SupervisorError::InvalidScale(message) = err else {
        panic!("expected InvalidScale, got {err:?}");
    };
    assert!(message.contains("delete"), "{message}");
}

/// fails if a dog can be scaled. A dog is one process by contract (spec §8) —
/// two metrics dogs would race for the same listen port, and two bark dogs
/// would double every alert.
#[tokio::test(start_paused = true)]
async fn a_dog_cannot_be_scaled() {
    let (h, _sheep, dog_name) = actor_with_a_sheep_and_a_dog().await;

    let err = h.ctx.supervisor.scale(&dog_name, 2).await.unwrap_err();

    let SupervisorError::InvalidScale(message) = err else {
        panic!("expected InvalidScale, got {err:?}");
    };
    assert!(message.contains("dog"), "{message}");
}

/// fails if an app mid-reload can be scaled. A reload holds two live processes
/// in one instance slot; a scale-down picking that slot removes one of them and
/// leaves the swap with nothing to finish.
#[tokio::test(start_paused = true)]
async fn an_app_mid_reload_refuses_a_scale() {
    let (h, _runner, _drainee) = actor_with_stopping_drainee().await;

    let err = h.ctx.supervisor.scale("web", 4).await.unwrap_err();

    assert_eq!(err, SupervisorError::ReloadInFlight("web".to_string()));
}

/// fails if an unregistered name is anything but NotFound. `shep scale typo 4`
/// exiting 0 would be the worst answer available.
#[tokio::test(start_paused = true)]
async fn scaling_an_unregistered_app_is_not_found() {
    let h = harness(vec![]);
    assert_eq!(
        h.ctx.supervisor.scale("ghost", 2).await.unwrap_err(),
        SupervisorError::NotFound
    );
}
```

`register_app`, `instance_slots_of`, `stored_instance_counts` and `settle` are
small helpers to add in this module beside the existing `register_sheep` /
`actor_with_a_sheep_and_a_dog` (line 6522) / `actor_with_stopping_drainee`
(line 6323). Read those first and follow their shapes.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Expected failure — for the stated reason:** compile error, ``no method named
`scale` found for struct `SupervisorHandle` ``.

### Step 6.2 — GREEN: the error variant

`SupervisorError`, after `ReloadInFlight`:

```rust
    /// A `Scale` the engine will not perform; carries the refusal in plain
    /// English, naming what to do instead.
    ///
    /// Three shapes reach it: a count of `0` (`normalize` refuses
    /// `instances == 0`, so accepting it here would admit a config the
    /// engine's own validator rejects — `shep delete` is the verb), a target
    /// that is a dog (one process by contract, spec §8), and a rescaled config
    /// that failed `normalize`, which is unreachable through this path and is
    /// carried rather than `expect`ed because a supervisor does not panic on
    /// peer input.
    ///
    /// Maps to [`RpcErrorCode::InvalidConfig`], not `Internal`: every one of
    /// those is something the caller asked for that it can ask differently.
    InvalidScale(String),
```

with a `Display` arm: `Self::InvalidScale(msg) => write!(f, "cannot scale: {msg}")`.

### Step 6.3 — GREEN: the handler

```rust
/// What a completed scale produced.
///
/// Two things, because two different layers need one each: the RPC arm replies
/// with `instances` and re-records `app` in the muster roll. Returning the
/// config rather than having the actor reach into the roll keeps
/// [`crate::snapshot::FlockRegistry`] a thing the rpc layer owns, which is
/// where `Request::Start` already keeps it.
#[derive(Debug)]
pub(crate) struct Scaled {
    /// The app's surviving instances, in instance-slot order.
    pub(crate) instances: Vec<ProcessInfo>,
    /// The app's config as it now stands, with the new `instances` count.
    pub(crate) app: ResolvedApp,
}
```

```rust
    /// Sets `name`'s instance count to `count`.
    ///
    /// # Slot allocation, both ways
    ///
    /// Up: [`instance_slots`] hands out the lowest free slots, exactly as a
    /// `Start` does. Down: the HIGHEST-numbered slots are deregistered first,
    /// which is what makes the two symmetric — scale a two-instance app to
    /// four and back and it is running slots 0 and 1 again, with the same log
    /// paths and the same `SHEP_INSTANCE` values it started with. Taking the
    /// lowest first would leave slots 2 and 3: the same count, a different
    /// flock.
    ///
    /// # What a scale-down does to the instances it removes
    ///
    /// Deregisters them — the same thing `Delete` does, through the same
    /// machinery, because a `Stop` would leave them registered and still
    /// holding their slots, and the next `Start` of the app would then find
    /// four slots taken and allocate a fifth.
    ///
    /// # Why the reply does not wait for them
    ///
    /// Each removal runs a kill ladder capped by the app's own `kill_timeout`,
    /// and a caller's RPC budget is capped at 60s (`crate::rpc`'s
    /// `MAX_DEADLINE_MS`), so a large scale-down cannot be covered by any reply
    /// a caller is allowed to wait for. The answer is the survivors; the
    /// departures report themselves on the bus as `process.delete`. Same split
    /// [`Self::handle_reload`] already makes.
    fn handle_scale(
        &mut self,
        name: &str,
        count: u32,
        reply: oneshot::Sender<Result<Scaled, SupervisorError>>,
    ) {
        let mut slots: Vec<(u32, u32)> = self
            .sheep
            .iter()
            .filter(|(_, slot)| slot.entry.spec.config().name == name)
            .map(|(id, slot)| (slot.entry.instance, *id))
            .collect();
        if slots.is_empty() {
            let _ = reply.send(Err(SupervisorError::NotFound));
            return;
        }
        slots.sort_unstable();

        if count == 0 {
            let _ = reply.send(Err(SupervisorError::InvalidScale(format!(
                "an app runs at least one instance; use `shep delete {name}` to remove it"
            ))));
            return;
        }
        if self
            .sheep
            .get(&slots[0].1)
            .is_some_and(|slot| slot.entry.dog.is_some())
        {
            let _ = reply.send(Err(SupervisorError::InvalidScale(format!(
                "{name} is a dog, and a dog runs one process"
            ))));
            return;
        }
        if self.reloads.contains_key(name) {
            let _ = reply.send(Err(SupervisorError::ReloadInFlight(name.to_string())));
            return;
        }

        // Re-normalized rather than mutated in place: `ResolvedApp` keeps its
        // config private precisely so that holding one proves it passed
        // `normalize` (`normalize.rs`'s own note), and a scale that edited the
        // field behind that door would be the first thing in the tree to hold
        // one that had not.
        let mut config = self
            .sheep
            .get(&slots[0].1)
            .expect("handle_scale: id read off this map a moment ago")
            .entry
            .spec
            .config()
            .clone();
        config.instances = count;
        let rescaled = match normalize(config) {
            Ok(app) => app,
            Err(err) => {
                let _ = reply.send(Err(SupervisorError::InvalidScale(err.to_string())));
                return;
            }
        };

        // Every surviving slot, not just the ones this call touches: `respawn`
        // reassembles from the slot's own stored spec, so a slot left holding
        // the pre-scale config would keep reporting the old count to
        // everything that reads it, `shep describe` included.
        for (_, id) in &slots {
            if let Some(slot) = self.sheep.get_mut(id) {
                slot.entry.spec = rescaled.clone();
            }
        }

        let current = u32::try_from(slots.len()).unwrap_or(u32::MAX);
        let credentials = self
            .sheep
            .get(&slots[0].1)
            .expect("handle_scale: id read off this map a moment ago")
            .entry
            .credentials;

        let survivors: Vec<u32> = match count.cmp(&current) {
            Ordering::Equal => slots.iter().map(|(_, id)| *id).collect(),
            Ordering::Greater => {
                let existing: Vec<u32> = slots.iter().map(|(instance, _)| *instance).collect();
                let mut ids: Vec<u32> = slots.iter().map(|(_, id)| *id).collect();
                for instance in instance_slots(&existing, count - current) {
                    match self.spawn_fresh(&rescaled, instance, credentials, None) {
                        Ok(info) => ids.push(info.id),
                        Err(message) => {
                            // Partial, and said so. The instances already
                            // spawned stay: they are real processes serving
                            // real traffic, and unwinding them would turn one
                            // failed spawn into an outage of everything this
                            // call had already brought up.
                            let _ = reply.send(Err(SupervisorError::SpawnFailed(message)));
                            return;
                        }
                    }
                }
                ids
            }
            Ordering::Less => {
                let cut = usize::try_from(count).unwrap_or(usize::MAX);
                let (keep, remove) = slots.split_at(cut);
                let removed: Vec<u32> = remove.iter().map(|(_, id)| *id).collect();
                self.begin_manual_ids(
                    removed,
                    ManualKind::Delete,
                    CommandOrigin::Operator,
                    // The removals' own terminal snapshots go nowhere: this
                    // reply is the survivors, and the departures report
                    // themselves on the bus.
                    ReplyKind::Ids(oneshot::channel().0),
                );
                keep.iter().map(|(_, id)| *id).collect()
            }
        };

        let mut instances: Vec<ProcessInfo> = survivors
            .iter()
            .filter_map(|id| self.sheep.get(id).map(|slot| to_info(&slot.entry)))
            .collect();
        instances.sort_unstable_by_key(|info| info.id);
        let _ = reply.send(Ok(Scaled {
            instances,
            app: rescaled,
        }));
    }
```

**`begin_manual_ids` does not exist yet.** Extract it from `begin_manual`
(`supervisor.rs:2018`): `begin_manual` becomes `matching_ids` + `NotFound` +
a call to the new function, and the whole per-id loop plus the `PendingReply`
push moves into it unchanged. Do **not** write a second copy of that loop —
it holds the `held_off_by_a_swap` carve-out, the `claim_manual` call and the
`pending_delete` flag, and a second copy would be a second place for each of
those to be fixed in only one.

`Command::Scale { name, count, reply }`, an arm in `handle_command`, and:

```rust
    /// Sets `name`'s instance count.
    ///
    /// # Errors
    ///
    /// - [`SupervisorError::NotFound`] — no app of that name is registered.
    /// - [`SupervisorError::InvalidScale`] — a count of `0`, or a target that
    ///   is a dog.
    /// - [`SupervisorError::ReloadInFlight`] — the app is mid-reload.
    /// - [`SupervisorError::SpawnFailed`] — an instance would not spawn.
    ///   Instances already spawned by this call stay running.
    /// - [`SupervisorError::EngineStopped`] — the actor is gone.
    pub(crate) async fn scale(&self, name: &str, count: u32) -> Result<Scaled, SupervisorError>
```

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.
Expect green, **+8**, and every pre-existing `stop`/`restart`/`delete` case
green too — `begin_manual`'s extraction is the risky part of this task, and
those are what prove it was a move rather than a rewrite.

### Step 6.4 — GREEN: the RPC arm, and the muster roll

In `crates/shep-daemon/src/rpc.rs`, after the `Request::Delete` arm:

```rust
        Request::Scale { name, count } => match ctx.supervisor.scale(&name, count).await {
            Ok(scaled) => {
                // Re-recorded here, and this line is the whole reason `scale`
                // hands back the config at all: without it `shep scale web 4`
                // followed by `shep save` writes a roll saying `instances = 2`,
                // and the scale is silently undone by the next reboot — a bug
                // that cannot be seen until the machine comes back.
                ctx.registry.record(&[scaled.app]);
                reply(Ok(Response::Scaled(scaled.instances)))
            }
            Err(err) => reply(Err(rpc_error(&err))),
        },
```

and in `rpc_error`, an arm mapping `SupervisorError::InvalidScale(msg)` to
`RpcError { code: RpcErrorCode::InvalidConfig, message: msg.clone() }`.

Add a dispatch-tier test that a scale survives a save:

```rust
/// fails if the muster roll keeps the pre-scale count. This is the test for the
/// bug that is invisible until a reboot: the roll is what `shep muster` reads,
/// so a scale missing from it is a scale that silently reverts.
#[tokio::test]
async fn a_scale_is_recorded_in_the_roll_the_next_muster_reads() {
    let h = harness(vec![ProcScript::runs_forever(); 4]);
    let mut app = AppConfig::minimal("web", "./srv");
    app.instances = 2;
    reply_of(dispatch(envelope(1, Request::Start { apps: vec![app] }), &h.ctx).await);

    reply_of(
        dispatch(
            envelope(
                2,
                Request::Scale {
                    name: "web".to_string(),
                    count: 4,
                },
            ),
            &h.ctx,
        )
        .await,
    );

    let saved = h.ctx.snapshot_now().await.unwrap();
    let roll = crate::snapshot::read(&saved).unwrap();
    assert_eq!(roll.apps[0].app.instances, 4);
}
```

Adapt to `snapshot_now`/`read`'s real signatures — read them.

Run. Expect green, **+1**.

### Step 6.5 — MUTATION

In `handle_scale`'s `Ordering::Less` arm, change

```rust
                let (keep, remove) = slots.split_at(cut);
```

to

```rust
                let (remove, keep) = slots.split_at(slots.len() - cut);
```

— which removes the LOWEST slots instead of the highest, keeping the count
correct.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Must go red:** `scaling_down_removes_the_highest_slots_so_a_round_trip_returns`
fails on `assert_eq!(instance_slots_of(&h, "web").await, vec![0, 1])`, getting
`vec![2, 3]`. Every other scale case must stay **green** — the counts are all
still right, which is exactly why a test asserting only `len()` could not catch
this and why the slot assertion is the one that matters.

Second mutation: delete the
`for (_, id) in &slots { … slot.entry.spec = rescaled.clone(); }` loop.

**Must go red:** `a_scale_updates_the_stored_instance_count` fails on its
`stored_instance_counts` assertion while `scaled.app.config().instances` is
still 4 — proving the returned copy and the stored slots are two different
facts and the test checks both.

Revert.

### Step 6.6 — CHANGELOG and gate

`crates/shep-daemon/CHANGELOG.md` → `Additions`: "`Request::Scale` — set an
app's instance count. Scale-up takes the lowest free slots, scale-down releases
the highest, and the new count is written back to the muster roll."

Then the full task gate.

---

## Task 7 — `shep scale`

**Files modified:**
- `crates/shep-cli/src/cli.rs` — `Scale(ScaleArgs)`.
- `crates/shep-cli/src/commands/lifecycle.rs` — the verb.
- `crates/shep-cli/src/main.rs` — wiring.
- `crates/shep-cli/tests/cli_e2e.rs` — one case.

**Consumes from Task 5:** `Request::Scale`, `Response::Scaled`.

### Step 7.1 — RED

Add to `crates/shep-cli/src/commands/lifecycle.rs`'s test module:

```rust
/// fails if the envelope carries anything but the name and the count. `scale`
/// is the one verb here that does NOT parse a selector, and a copy-pasted
/// `parse_selector` would turn `web` into `SelectorSpec::Name("web")` and send
/// a frame the daemon has no arm for.
#[tokio::test]
async fn the_request_carries_the_app_name_and_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams {
        out: &mut out,
        err: &mut err,
    };
    let _ = scale(
        &client,
        &mut streams,
        Format::Table,
        &ScaleArgs {
            name: "web".to_string(),
            count: 4,
        },
    )
    .await;

    let envelope = envelopes.recv().await.unwrap();
    assert_eq!(
        envelope.body,
        Request::Scale {
            name: "web".to_string(),
            count: 4,
        }
    );
}

/// fails if an `InvalidConfig` refusal is swallowed or remapped. A count of 0
/// is the shape an operator will actually type, and it has to come back as
/// exit 4 with the daemon's own sentence, not as a generic failure.
#[tokio::test]
async fn an_invalid_scale_exits_invalid_config_and_prints_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, _served) = fake_client_replying_err(
        &path,
        RpcErrorCode::InvalidConfig,
        "an app runs at least one instance; use `shep delete web` to remove it",
    )
    .await;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = {
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        scale(
            &client,
            &mut streams,
            Format::Table,
            &ScaleArgs {
                name: "web".to_string(),
                count: 1,
            },
        )
        .await
    };
    assert_eq!(code, ExitCode::InvalidConfig);
    assert!(
        String::from_utf8(err).unwrap().contains("shep delete web"),
        "the daemon's own sentence has to reach the operator"
    );
}
```

And to `crates/shep-cli/src/cli.rs`'s own test module:

```rust
/// fails if clap accepts `shep scale web 0`. The refusal exists daemon-side
/// too, and deliberately in both places — but a usage error should not cost a
/// connection, and `range(1..)` is what puts the accepted range into `--help`.
#[test]
fn scale_refuses_a_count_of_zero_before_it_reaches_the_wire() {
    assert!(Cli::try_parse_from(["shep", "scale", "web", "0"]).is_err());
    assert!(Cli::try_parse_from(["shep", "scale", "web", "1"]).is_ok());
}

/// fails if `scale` grows a default target. `shep scale 4` must be a usage
/// error, never "scale whatever app happens to be first".
#[test]
fn scale_requires_both_the_name_and_the_count() {
    assert!(Cli::try_parse_from(["shep", "scale"]).is_err());
    assert!(Cli::try_parse_from(["shep", "scale", "web"]).is_err());
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
struct `ScaleArgs` ``.

### Step 7.2 — GREEN

`crates/shep-cli/src/cli.rs`, in `Commands` after `Delete`:

```rust
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
```

```rust
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
```

`crates/shep-cli/src/commands/lifecycle.rs` — the verb, through that module's
existing `request_and_render`-style helper:

```rust
/// Sets `args.name`'s instance count, and renders the instances that remain.
///
/// No `parse_selector` call, unlike every other verb in this module: `scale`
/// takes a name. See [`ScaleArgs`]'s own doc for why.
pub async fn scale(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &ScaleArgs,
) -> ExitCode
```

rendering `FlockRows` — the reply is a flock listing and there is no reason for
a table of its own.

Deadline: `START_DEADLINE`, not the plain default. A scale-up spawns processes,
which is the same work `shep start` already asks for the longer budget to
cover.

Wire `Commands::Scale` in `main.rs`.

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+4**.

### Step 7.3 — GREEN: end to end

Add to `crates/shep-cli/tests/cli_e2e.rs`: start a one-instance app, `shep
scale <name> 3`, assert exit 0 and that `shep flock` then lists three rows for
that name; `shep scale <name> 1`, assert `shep flock` settles back to one.
Bound the settle with the file's existing polling helper rather than a bare
sleep — a scale-down that never completes must fail the test, not hang it
(IR-46).

Run `cargo test -p shep-cli --test cli_e2e --all-features`. Expect green, **+1**.

### Step 7.4 — MUTATION

In `crates/shep-cli/src/cli.rs`, remove
`#[arg(value_parser = clap::value_parser!(u32).range(1..))]` from
`ScaleArgs::count`.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `scale_refuses_a_count_of_zero_before_it_reaches_the_wire`
fails on its first assert — `shep scale web 0` now parses.
`scale_requires_both_the_name_and_the_count` must stay **green**: it is testing
a different property, and a mutation that reddens both would mean the two tests
are one test written twice.

Revert.

### Step 7.5 — CHANGELOG and gate

`crates/shep-cli/CHANGELOG.md` → `Additions`: "`shep scale <name> <count>`."

Then the full task gate.

---

## Task 8 — `AppConfig::stdin`, and the `SendLine` wire

**Files modified:**
- `crates/shep-core/src/config/app.rs` — the field, its default, its doc.
- `crates/shep-core/src/protocol/request.rs` — `Request::SendLine`,
  `Response::SentLine`, `LineReply`, `LineOutcome`, fixtures.

**Produces, for Tasks 9–11:**

```rust
AppConfig::stdin: bool                    // default false
Request::SendLine { selector: SelectorSpec, line: String }
Response::SentLine(Vec<LineReply>)
pub struct LineReply { pub id: u32, pub name: String, pub outcome: LineOutcome }
#[non_exhaustive] pub enum LineOutcome { Sent, NoStdin, NotWritten { reason: String } }
```

> **This task moves a pinned snapshot.** `request_wire_v1` contains a full
> `AppConfig` (the `Request::Start` row), so adding a field changes it by
> exactly one line. Do not run this concurrently with Task 15, which moves
> `reply_wire_v1` and `bus_event_wire_v1` for the same reason.

### Step 8.1 — RED: the field

Add to `crates/shep-core/src/config/app.rs`'s test module:

```rust
/// fails if `stdin` defaults to anything but false. The default is the whole
/// decision: piping stdin for every sheep would change how a great many
/// programs behave (a closed stdin is how they decide they are
/// non-interactive), and would hold a descriptor and a task per sheep for the
/// life of the process.
#[test]
fn stdin_is_not_piped_unless_the_app_asks() {
    let app = AppConfig::minimal("web", "./srv");
    assert!(!app.stdin);
    let parsed: AppConfig = toml::from_str("name = \"web\"\nscript = \"./srv\"").unwrap();
    assert!(!parsed.stdin);
}

/// fails if the Flockfile key is spelled anything but `stdin`. It is a
/// contract with every config file already written against it the moment this
/// ships, and `deny_unknown_fields` means a rename is a hard parse failure for
/// the operator rather than a silently ignored key.
#[test]
fn the_flockfile_key_is_stdin() {
    let parsed: AppConfig =
        toml::from_str("name = \"web\"\nscript = \"./srv\"\nstdin = true").unwrap();
    assert!(parsed.stdin);
}
```

Run `cargo test -p shep-core --lib --all-features`.

**Expected failure — for the stated reason:** compile error, ``no field `stdin`
on type `AppConfig` ``.

### Step 8.2 — GREEN: the field

In `AppConfig`, immediately after `channel` so the two pipe-opening options sit
together:

```rust
    /// Open a pipe on this sheep's stdin, so `shep sendline` can write to it.
    ///
    /// Defaults to `false`, and the default is the decision rather than a
    /// convenience. Without it a sheep gets `/dev/null` on fd 0, which is what
    /// every sheep has had until now, and three things argue for keeping it
    /// that way unless an app asks otherwise:
    ///
    /// - Flipping it for the whole flock is a behaviour change to processes
    ///   nobody asked to change.
    /// - **Programs detect stdin.** A closed or null fd 0 is how a great many
    ///   programs decide they are non-interactive — no prompt, no pager, no
    ///   readline, no colour. Handing them a pipe silently moves them to the
    ///   other branch.
    /// - It costs a descriptor and a pump task per sheep for the whole life of
    ///   the process, against spec §14.11's single-digit-MB idle-RSS goal — the
    ///   same budget [`Self::channel`]'s own default is protecting.
    ///
    /// Unlike `channel`, nothing implies this: `wait_ready` and
    /// `shutdown_with_message` both need fd 3 and so turn `channel` on for you,
    /// while nothing in shep needs a sheep's stdin except an operator typing
    /// `shep sendline`. A sheep without it answers a `no_stdin` row and names
    /// this field.
    ///
    /// The pipe's write end lives as long as the sheep does, so the app sees
    /// EOF on stdin when the process is on its way out, never before.
    pub stdin: bool,
```

and `stdin: false,` in `impl Default for AppConfig` (which is a hand-written
literal at `app.rs:203` — the compiler will name it).

Run `cargo test -p shep-core --lib --all-features`.

**Expect a snapshot failure**, not a pass: `request_wire_snapshots` fails
because the `Request::Start` row's `AppConfig` gained a key. Review the diff —
it must be **exactly one added line, `"stdin": false,`, in one object, and
nothing else** — then accept:

```bash
INSTA_UPDATE=always cargo test -p shep-core --lib --all-features
git diff crates/shep-core/src/protocol/snapshots/shep_core__protocol__request__tests__request_wire_v1.snap
```

The `git diff` must show one `+` line and zero `-` lines. Then green, **+2**.

### Step 8.3 — RED: the wire

Add to `crates/shep-core/src/protocol/request.rs`'s test module:

```rust
/// fails if the three outcomes stop being tellable apart on the wire, or if
/// `NotWritten` stops carrying its reason. That reason is the only thing that
/// distinguishes "the app is not reading its stdin" from "the pipe broke", and
/// the operator's next move differs between them.
#[test]
fn a_send_line_request_and_its_reply_round_trip() {
    let request = Request::SendLine {
        selector: SelectorSpec::Name("repl".to_string()),
        line: "reload-config".to_string(),
    };
    let json = serde_json::to_string(&request).unwrap();
    assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);

    let reply = Response::SentLine(vec![
        LineReply {
            id: 1,
            name: "repl".to_string(),
            outcome: LineOutcome::Sent,
        },
        LineReply {
            id: 2,
            name: "web".to_string(),
            outcome: LineOutcome::NoStdin,
        },
        LineReply {
            id: 3,
            name: "stuck".to_string(),
            outcome: LineOutcome::NotWritten {
                reason: "the app did not read its stdin within 2s".to_string(),
            },
        },
    ]);
    let json = serde_json::to_string(&reply).unwrap();
    assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), reply);
    assert!(json.contains(r#""kind":"sent""#), "{json}");
    assert!(json.contains(r#""kind":"no_stdin""#), "{json}");
    assert!(json.contains("did not read its stdin"), "{json}");
}

/// fails if a newline can ride inside the line. The wire carries ONE line and
/// the writer appends the terminator, so an embedded newline would deliver two
/// commands where the operator typed one — the shape that turns a typo into an
/// unintended second instruction to a REPL.
#[test]
fn a_line_carrying_a_newline_is_still_one_field_on_the_wire() {
    let request = Request::SendLine {
        selector: SelectorSpec::All,
        line: "a\nb".to_string(),
    };
    let json = serde_json::to_string(&request).unwrap();
    // Escaped, not literal: the frame stays one JSON object. Rejecting it is
    // the daemon's job (see `shep sendline`), not serde's, and this pins that
    // the wire itself does not quietly split it.
    assert!(json.contains(r#""line":"a\nb""#), "{json}");
    assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
}
```

Run. **Expected failure — for the stated reason:** compile error, ``no variant
named `SendLine` found for enum `Request` ``.

### Step 8.4 — GREEN: the frames

`Request`, after `Signal`:

```rust
    /// Write one line to every matched sheep's stdin (see `shep sendline`).
    SendLine {
        /// Which sheep. No default, matching every other verb that reaches a
        /// running process.
        selector: SelectorSpec,
        /// The line, WITHOUT its terminator — the shepherd appends exactly one
        /// `\n` when it writes. Carrying the terminator here would leave "did
        /// the caller include one" as a question every hop has to re-answer,
        /// and a caller that included two would send an empty line the app
        /// never asked for.
        ///
        /// A line containing an embedded newline is refused
        /// ([`RpcErrorCode::InvalidConfig`]): it would deliver two commands
        /// where the operator typed one.
        line: String,
    },
```

`Response`, after `Signalled`:

```rust
    /// Answer to `SendLine` — one [`LineReply`] row per matched sheep.
    SentLine(Vec<LineReply>),
```

and the two types, beside `SignalReply`/`SignalOutcome`:

```rust
/// What happened when the shepherd tried to write one line to a sheep's stdin.
///
/// `#[non_exhaustive]`: a future outcome — a sheep refused because its pipe is
/// backed up, say — must not need a protocol version bump (IR-20).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LineOutcome {
    /// The line was written to the pipe and flushed.
    ///
    /// Says the bytes left the shepherd, not that the app read them. A pipe
    /// holds 64 KiB before it blocks, so a short line to an app that never
    /// reads its stdin is `Sent` — which is honest, because there is nothing
    /// on this path that could tell the difference and a supervisor inventing
    /// one would be guessing.
    Sent,
    /// The sheep has no stdin pipe: its config does not set `stdin = true`, or
    /// it is not running.
    ///
    /// One outcome for two causes, deliberately. The row is read to answer
    /// "why did my line not arrive", and both answers are "there is no pipe
    /// here"; splitting them would put the operator in front of a distinction
    /// with the same fix behind it. A sheep that is not running is visible as
    /// such in `shep flock`, which is where that question belongs.
    NoStdin,
    /// The shepherd had a pipe and could not write to it; carries why.
    ///
    /// Two shapes reach it: the write failed (the far end is gone — normally
    /// the app exiting between the lookup and the write), or it did not finish
    /// inside the shepherd's own bound, which means the pipe is full because
    /// the app is not reading. The reason names which, because the operator's
    /// next move differs.
    NotWritten {
        /// What went wrong, in plain English.
        reason: String,
    },
}

/// One matched sheep's row in a `SendLine` reply.
///
/// Same shape and same argument as [`ActionReply`] and [`SignalReply`]: spec
/// §9's selector grammar makes a mixed flock the normal case, so an outcome
/// per row beats a whole-request refusal.
// wire format: changing this is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineReply {
    /// The sheep's stable id.
    pub id: u32,
    /// The sheep's name.
    pub name: String,
    /// What happened.
    pub outcome: LineOutcome,
}
```

Export both from `protocol/mod.rs`. Add a `Request::SendLine` envelope and a
`Response::SentLine` reply to the two wire snapshots, review the appended-rows
diff, accept, then:

```bash
git diff --stat crates/shep-core/src/protocol/snapshots/
find crates -name '*.snap.new' | wc -l      # 0 at HEAD, must be 0 after
```

Run `cargo test -p shep-core --lib --all-features`. Expect green, **+2**.

### Step 8.5 — MUTATION

In `crates/shep-core/src/config/app.rs`, change `stdin: false,` in the `Default`
impl to `stdin: true,`.

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `stdin_is_not_piped_unless_the_app_asks` fails on both of its
asserts, AND `request_wire_snapshots` fails with `"stdin": true` in the diff.
`the_flockfile_key_is_stdin` must stay **green** — it sets the key explicitly,
so it proves the spelling and says nothing about the default, which is the
division of labour those two tests are for.

Revert.

### Step 8.6 — CHANGELOG and gate

`crates/shep-core/CHANGELOG.md` → `Additions`: "`AppConfig::stdin` (default
`false`) — pipe a sheep's stdin so `shep sendline` can reach it;
`Request::SendLine` / `Response::SentLine` / `LineReply` / `LineOutcome`."

Then the full task gate.

---

## Task 9 — the spawn path pipes stdin

**Files modified:**
- `crates/shep-daemon/src/runner.rs` — `SpawnSpec::stdin`, `ProcIo::to_stdin`,
  `StdinWrite`, `RunnerError::WriteFailed`.
- `crates/shep-daemon/src/assemble.rs` — carry the flag onto the spec.
- `crates/shep-daemon/src/tokio_runner.rs` — the pipe and its pump.
- `crates/shep-daemon/src/fake.rs` — the scripted half.
- `crates/shep-daemon/tests/real_runner.rs` — one real-child case.

**Consumes from Task 8:** `AppConfig::stdin`.

**Produces, for Task 10:**

```rust
// crates/shep-daemon/src/runner.rs
pub struct SpawnSpec { /* … */ pub stdin: bool }

pub struct StdinWrite {
    pub line: String,
    pub done: oneshot::Sender<Result<(), RunnerError>>,
}

pub struct ProcIo { /* … */ pub to_stdin: mpsc::Sender<StdinWrite> }

RunnerError::WriteFailed(String)
```

### Step 9.0 — the shape to copy, and the shape not to

`to_stdin` is **not** an `Option`. It mirrors `ProcIo::to_child` exactly: always
a sender, and when the app did not ask for a pipe the runner **drops the
receiver** so `is_closed()` answers the question. `tokio_runner.rs:284` already
argues for that shape on the channel — "close both ends immediately rather than
leaving them dangling, so a stray send fails fast instead of silently buffering
into a channel nobody will ever drain" — and one pattern beats two.

The supervisor-side consequence is in Task 10: `SheepSlot::to_stdin` is
**cleared on exit**, exactly as `to_child` is and for the reason `to_child`'s
own doc gives (the writer task parks on `recv()` and a sender kept on a dead
slot parks it, and its half of the pipe, for as long as the sheep stays
registered). Do not copy `log_ctl`'s never-cleared shape here: its argument
depends on the pump having a second way to end, which this task's writer does
not have.

### Step 9.1 — RED

Add to `crates/shep-daemon/src/assemble.rs`'s test module:

```rust
/// fails if `stdin` does not reach the spec. It is the one field on the way to
/// the runner whose default is "closed", so a spec assembled without it would
/// silently give an opted-in app `/dev/null` and make every sendline row read
/// `no_stdin` with nothing to point at.
#[test]
fn the_stdin_flag_reaches_the_spawn_spec() {
    let mut app = AppConfig::minimal("repl", "./repl");
    app.stdin = true;
    let spec = assemble(&normalize(app).unwrap(), 0, &test_paths(), None);
    assert!(spec.stdin);
}

/// fails if `stdin` is implied by something. `channel` is implied by
/// `wait_ready` and `shutdown_with_message` because both need fd 3; nothing in
/// shep needs fd 0, so nothing may turn it on behind the operator's back.
#[test]
fn nothing_else_turns_stdin_on() {
    let mut app = AppConfig::minimal("web", "./srv");
    app.channel = true;
    app.wait_ready = true;
    app.shutdown_with_message = true;
    let spec = assemble(&normalize(app).unwrap(), 0, &test_paths(), None);
    assert!(spec.channel, "the fixture should still open a channel");
    assert!(!spec.stdin);
}
```

And to `crates/shep-daemon/src/fake.rs`'s test module:

```rust
/// fails if the scripted runner answers a stdin write for a spec that asked for
/// no pipe. A fake that accepted every write would make `no_stdin` unreachable
/// from the engine tier and every test of it vacuous — the same trap
/// `spec.channel` already had to be taught about.
#[tokio::test(start_paused = true)]
async fn a_spawn_without_stdin_hands_back_a_closed_writer() {
    let runner = ScriptedRunner::new(vec![ProcScript::runs_forever()]);
    let (_proc, io) = runner.spawn(&spec_for("web")).unwrap();
    assert!(io.to_stdin.is_closed());
}
```

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Expected failure — for the stated reason:** compile error, ``no field `stdin`
on type `SpawnSpec` ``.

### Step 9.2 — GREEN: the seam

`crates/shep-daemon/src/runner.rs`:

```rust
/// One line to write to a sheep's stdin, and where the answer goes.
///
/// The acknowledgement is the point, exactly as it is on [`LogCtl`]: an
/// `mpsc::send` only proves the message was queued, and a caller told "sent"
/// on that basis would be told it about a line still sitting in a channel
/// behind a pipe the app has stopped reading. The `oneshot` fires after the
/// bytes are written AND flushed, which is the strongest claim this side of
/// the pipe can honestly make.
#[derive(Debug)]
pub struct StdinWrite {
    /// The line, without its terminator — the writer appends exactly one `\n`.
    pub line: String,
    /// Fires once the line has landed, or with why it could not.
    ///
    /// A dropped sender means the writer task ended before serving this
    /// request, which happens when the child's stdin closed — the caller reads
    /// that as the pipe being gone.
    pub done: oneshot::Sender<Result<(), RunnerError>>,
}
```

`SpawnSpec` gains, next to `channel`:

```rust
    /// Pipe the child's stdin, so `shep sendline` can write to it. `false`
    /// gives the child `/dev/null` on fd 0, which is what every sheep gets
    /// unless its config sets `stdin = true`.
    pub stdin: bool,
```

`ProcIo` gains, next to `to_child`:

```rust
    /// The shepherd's writing end of this sheep's stdin.
    ///
    /// Always present, and closed rather than absent when the app did not ask
    /// for a pipe: the runner drops the receiving end in that case, so
    /// `is_closed()` is the one question a caller has to ask — the same shape
    /// [`Self::to_child`] uses for a sheep configured without a shepherd
    /// channel.
    ///
    /// Hold it only for as long as the child is alive. The task on the far end
    /// parks on `recv()` and has no other way to finish, so a sender kept past
    /// the child's exit parks that task and holds the pipe's write end with it.
    pub to_stdin: mpsc::Sender<StdinWrite>,
```

`RunnerError` gains:

```rust
    /// A write to a child's stdin failed (carries the OS message, or the
    /// shepherd's own bound when the app was not reading).
    WriteFailed(String),
```

with a `Display` arm: `Self::WriteFailed(msg) => write!(f, "stdin write failed: {msg}")`.

`crates/shep-daemon/src/assemble.rs` — set `stdin: config.stdin` on the
assembled spec, beside the existing `channel: config.channel || config.wait_ready
|| config.shutdown_with_message` line, and extend that function's `# Log paths`
-style doc block with a short "Stdin" note saying the flag is carried straight
through and implied by nothing.

Run. **Expect a wall of "missing field `stdin`" and "missing field `to_stdin`"
errors** at every `SpawnSpec`/`ProcIo` literal in the tree. That is the
compiler doing the sweep; fix each site. `SpawnSpec` has no builder and this
task deliberately does not add one — it is one bool, and `ProcessInfo`'s own
builder exists because that struct had grown a field in three separate phases.

### Step 9.3 — GREEN: the real pipe

`crates/shep-daemon/src/tokio_runner.rs`, replacing `command.stdin(Stdio::null());`
at line 206:

```rust
        // `/dev/null` unless the app asked for a pipe. Piping unconditionally
        // would change how a great many programs behave — a closed fd 0 is how
        // they decide they are non-interactive — so this follows `spec.channel`
        // in being opened only on request. See `AppConfig::stdin`.
        command.stdin(if spec.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
```

and, after the spawn, beside `spawn_log_pump`:

```rust
        let (to_stdin_tx, to_stdin_rx) = mpsc::channel(CHANNEL_CAPACITY);
        if spec.stdin {
            spawn_stdin_pump(child.stdin.take(), to_stdin_rx);
        } else {
            // Dropped rather than left dangling, so a caller's `is_closed()`
            // says "no pipe here" immediately instead of the send silently
            // buffering into a channel nobody will drain — the same choice the
            // no-channel arm above makes for fd 3.
            drop(to_stdin_rx);
        }
```

```rust
/// Writes lines to one child's stdin, one at a time, acknowledging each.
///
/// Serial on purpose: two concurrent writers to one pipe can interleave
/// mid-line, and a REPL reading the result would see a command neither caller
/// sent. Serial also means a line queued behind one the app is not reading
/// waits — which is correct, and which is why the caller bounds its own wait
/// rather than this task bounding the write (a write abandoned halfway would
/// leave a partial line in the pipe, which is worse than a slow one).
///
/// Ends when the last sender drops, which closes the child's stdin and gives
/// the app EOF. That is the sheep task letting go of `ProcIo`, i.e. the child
/// exiting — never before.
fn spawn_stdin_pump(stdin: Option<ChildStdin>, mut rx: mpsc::Receiver<StdinWrite>) {
    tokio::spawn(async move {
        let Some(mut stdin) = stdin else {
            // `Stdio::piped()` was set and `child.stdin` was still `None`,
            // which std does not do — but answering nothing would hang every
            // caller, so the requests are drained and refused instead.
            while let Some(StdinWrite { done, .. }) = rx.recv().await {
                let _ = done.send(Err(RunnerError::WriteFailed(
                    "this child has no stdin pipe".to_string(),
                )));
            }
            return;
        };
        while let Some(StdinWrite { line, done }) = rx.recv().await {
            let mut bytes = line.into_bytes();
            // Exactly one terminator, appended here and nowhere else. The wire
            // carries the line without one (`Request::SendLine::line`), so this
            // is the single place the question "is a newline included" is ever
            // answered.
            bytes.push(b'\n');
            let result = match stdin.write_all(&bytes).await {
                Ok(()) => stdin.flush().await,
                Err(error) => Err(error),
            };
            let _ = done.send(result.map_err(|error| RunnerError::WriteFailed(error.to_string())));
        }
    });
}
```

with `use tokio::io::AsyncWriteExt as _;` and `use tokio::process::ChildStdin;`.

`crates/shep-daemon/src/fake.rs` — mirror `spec.channel`'s handling: when
`spec.stdin` is set, spawn a task that answers every `StdinWrite` with `Ok(())`
and records the line on a `Vec<String>` a test can read back
(`ScriptedRunner::stdin_lines(spawn_index)`); when it is not, drop the receiver.
Document, in the same voice as the `spec.channel` note already there, that the
fake writes to no real pipe and that back-pressure and a real EOF are
`tests/real_runner.rs`'s tier.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.
Expect green, **+3**.

### Step 9.4 — GREEN: a real child reads a real line

Add to `crates/shep-daemon/tests/real_runner.rs`:

```rust
/// A real pipe on a real fd 0. `cat` echoes whatever is written to it, so the
/// line coming back on stdout is proof the whole path worked — the pipe was
/// created, mapped to fd 0, written, flushed, and read by the child.
///
/// Bounded (IR-46): a line that never arrives must fail this test, not hang it.
#[tokio::test]
async fn a_real_child_reads_a_line_written_to_its_stdin() {
    let mut spec = spec_for_program("/bin/cat", &[]);
    spec.stdin = true;
    let runner = TokioRunner;
    let (_proc, mut io) = runner.spawn(&spec).unwrap();

    let (done, ack) = tokio::sync::oneshot::channel();
    io.to_stdin
        .send(StdinWrite {
            line: "hello sheep".to_string(),
            done,
        })
        .await
        .unwrap();
    ack.await.unwrap().unwrap();

    let line = tokio::time::timeout(Duration::from_secs(10), io.logs.recv())
        .await
        .expect("no stdout line within 10s")
        .expect("log channel closed");
    assert!(!line.err);
    assert_eq!(line.line, "hello sheep");
}

/// fails if a spec that did not ask for a pipe gets one anyway. `/dev/null` on
/// fd 0 is what every sheep has had until now, and `cat` reading EOF
/// immediately — exiting rather than waiting — is the observable difference.
#[tokio::test]
async fn a_child_that_did_not_ask_for_stdin_gets_eof_at_once() {
    let mut spec = spec_for_program("/bin/cat", &[]);
    spec.stdin = false;
    let runner = TokioRunner;
    let (mut proc, io) = runner.spawn(&spec).unwrap();

    assert!(io.to_stdin.is_closed());
    let outcome = tokio::time::timeout(Duration::from_secs(10), proc.wait())
        .await
        .expect("cat did not exit on EOF within 10s");
    assert_eq!(outcome.code, Some(0));
}
```

Adapt `spec_for_program` to whatever that file's own spec helper is called.

Run `cargo test -p shep-daemon --test real_runner --all-features`. Expect
green, **+2**.

### Step 9.5 — MUTATION

In `crates/shep-daemon/src/tokio_runner.rs`, change

```rust
            bytes.push(b'\n');
```

to

```rust
            bytes.push(b' ');
```

Run `cargo test -p shep-daemon --test real_runner --all-features`.

**Must go red:** `a_real_child_reads_a_line_written_to_its_stdin` times out at
its 10s bound with `no stdout line within 10s` — `cat` is line-buffered against
a pipe and never sees a terminator. That the failure is a *bounded timeout*
rather than a hang is the reason the bound is written; without it this mutation
would wedge the suite.

Second mutation: change the `command.stdin(…)` conditional to
`command.stdin(Stdio::piped());` unconditionally. Run
`cargo test -p shep-daemon --test real_runner --all-features`.

**Must go red:** `a_child_that_did_not_ask_for_stdin_gets_eof_at_once` fails on
`io.to_stdin.is_closed()` — and, if that assert were removed, would then time
out waiting for a `cat` that is holding an open pipe. Two independent failures
for the opt-in property, which is the one this whole task's default rests on.

Revert both.

### Step 9.6 — CHANGELOG and gate

`crates/shep-daemon/CHANGELOG.md` → `Additions`: "`SpawnSpec::stdin` and
`ProcIo::to_stdin` — an opt-in pipe on a sheep's stdin, with a per-line
acknowledgement."

Then the full task gate.

---

## Task 10 — the supervisor writes a line to a sheep

**Files modified:**
- `crates/shep-daemon/src/supervisor.rs` — `SheepSlot::to_stdin`,
  `Command::SendLine`, `Actor::begin_send_line`, `SupervisorHandle::send_line`,
  `STDIN_WRITE_TIMEOUT`.
- `crates/shep-daemon/src/rpc.rs` — the `Request::SendLine` arm.

**Consumes:** Task 8's wire types, Task 9's `StdinWrite`/`ProcIo::to_stdin`.

**Produces, for Task 11:** `Response::SentLine` served by a real daemon.

### Step 10.1 — RED

Add to `crates/shep-daemon/src/supervisor.rs`'s test module:

```rust
/// fails if a line does not reach the sheep's pipe. The fake records what it
/// was handed, so this asserts the line and not merely that something happened.
#[tokio::test(start_paused = true)]
async fn a_line_reaches_a_sheep_that_asked_for_stdin() {
    let runner = SharedRunner::new(vec![ProcScript::runs_forever()]);
    let h = harness_with_runner(runner.clone());
    let mut app = AppConfig::minimal("repl", "./repl");
    app.stdin = true;
    let id = register_sheep(&h, app).await;

    let rows = h
        .ctx
        .supervisor
        .send_line(ProcessSelector::Id(id), "reload-config".to_string())
        .await
        .unwrap();

    assert_eq!(
        rows,
        vec![LineReply {
            id,
            name: "repl".to_string(),
            outcome: LineOutcome::Sent,
        }]
    );
    assert_eq!(runner.stdin_lines(0), vec!["reload-config".to_string()]);
}

/// fails if a sheep without `stdin = true` is answered anything but `no_stdin`
/// — and especially if it is answered `Sent`, which would claim a line landed
/// somewhere that has no pipe at all.
#[tokio::test(start_paused = true)]
async fn a_sheep_without_stdin_answers_no_stdin() {
    let h = harness(vec![ProcScript::runs_forever()]);
    let id = register_sheep(&h, AppConfig::minimal("web", "./srv")).await;

    let rows = h
        .ctx
        .supervisor
        .send_line(ProcessSelector::Id(id), "hello".to_string())
        .await
        .unwrap();

    assert_eq!(rows[0].outcome, LineOutcome::NoStdin);
}

/// fails if a mixed flock is refused as a whole. Half the sheep having a pipe
/// is the normal case under `all`, and a refusal that took the reachable half
/// with it would leave the operator unable to tell which half was taken — the
/// same rule `Reopen`, `Flush`, `Trigger` and `Signal` all follow.
#[tokio::test(start_paused = true)]
async fn a_mixed_flock_reports_per_sheep_rather_than_failing() {
    let runner = SharedRunner::new(vec![ProcScript::runs_forever(); 2]);
    let h = harness_with_runner(runner.clone());
    let mut piped = AppConfig::minimal("repl", "./repl");
    piped.stdin = true;
    let piped_id = register_sheep(&h, piped).await;
    let plain_id = register_sheep(&h, AppConfig::minimal("web", "./srv")).await;

    let rows = h
        .ctx
        .supervisor
        .send_line(ProcessSelector::All, "hello".to_string())
        .await
        .unwrap();

    let outcome = |id| rows.iter().find(|r| r.id == id).unwrap().outcome.clone();
    assert_eq!(outcome(piped_id), LineOutcome::Sent);
    assert_eq!(outcome(plain_id), LineOutcome::NoStdin);
    // id-sorted, like every other row-shaped reply, so the answer's order is
    // the selector's and not the scheduler's.
    assert!(rows.windows(2).all(|w| w[0].id < w[1].id));
}

/// fails if a wait on an app that never reads its stdin has no bound (IR-46).
/// This is the case that can only fail by hanging, so it is the one that has to
/// carry an explicit deadline — and the outcome has to name the bound, because
/// "the app is not reading" and "the pipe broke" have different fixes.
#[tokio::test(start_paused = true)]
async fn a_write_that_never_lands_times_out_and_says_so() {
    let runner = SharedRunner::new(vec![ProcScript::runs_forever()]);
    // A runner whose stdin task accepts requests and never answers them —
    // exactly what a full pipe looks like from this side.
    let h = harness_with_runner(runner.silent_stdin());
    let mut app = AppConfig::minimal("stuck", "./stuck");
    app.stdin = true;
    let id = register_sheep(&h, app).await;

    let rows = tokio::time::timeout(
        STDIN_WRITE_TIMEOUT * 4,
        h.ctx
            .supervisor
            .send_line(ProcessSelector::Id(id), "hello".to_string()),
    )
    .await
    .expect("send_line did not honour its own bound")
    .unwrap();

    let LineOutcome::NotWritten { reason } = rows[0].outcome.clone() else {
        panic!("expected NotWritten, got {:?}", rows[0].outcome);
    };
    assert!(reason.contains("read"), "{reason}");
}

/// fails if a selector matching nothing is answered with an empty success.
#[tokio::test(start_paused = true)]
async fn a_selector_that_matches_nothing_is_not_found_for_send_line() {
    let h = harness(vec![]);
    assert_eq!(
        h.ctx
            .supervisor
            .send_line(ProcessSelector::Name("ghost".to_string()), "x".to_string())
            .await
            .unwrap_err(),
        SupervisorError::NotFound
    );
}
```

`SharedRunner::silent_stdin` is a small variant to add in `fake.rs` beside the
existing scripted behaviours — the counterpart of `never_reports_its_exit`,
which is the precedent for "accepts the request, answers nothing".

Run. **Expected failure — for the stated reason:** compile error, ``no method
named `send_line` found for struct `SupervisorHandle` ``.

### Step 10.2 — GREEN

```rust
/// How long the shepherd waits for one line to land in a sheep's stdin before
/// reporting [`LineOutcome::NotWritten`].
///
/// A bound is not optional here (IR-46): a pipe fills at 64 KiB and the write
/// then blocks until the app reads, which an app that never reads never does —
/// so an unbounded wait is a request that can only end by the caller's own
/// deadline expiring, which tells the operator nothing about why.
///
/// Two seconds, and fixed rather than per-app. `AppConfig::action_timeout` is
/// per-app because an action's duration is the APP's work; a pipe write is the
/// kernel's, and the only thing a longer wait would buy is more time for an app
/// that is not reading its stdin to start. Comfortably under the 5s an RPC
/// caller gets when it sends no deadline of its own, so the honest
/// `not_written` row reaches the caller rather than racing its budget.
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
```

`SheepSlot` gains `to_stdin: Option<mpsc::Sender<StdinWrite>>`, written on every
successful spawn from `io.to_stdin.clone()` and **cleared wherever `to_child`
is cleared** — the compiler will name both sites once the field exists, and the
field's own doc should say "cleared with [`Self::to_child`], for the reason
that field's doc gives" rather than restating the argument.

Add `SheepSlot::open_stdin`, the twin of `open_channel`:

```rust
    /// This sheep's stdin sender while something is still there to receive on
    /// it, and `None` when nothing is.
    ///
    /// Read off the channel rather than off `AppConfig::stdin` so there is no
    /// second copy of the fact free to disagree — exactly as
    /// [`Self::open_channel`] reads the channel rather than `AppConfig::channel`.
    /// Both halves matter: `to_stdin` is cleared when a process ends under this
    /// id, and `is_closed` catches an app that never asked for a pipe, whose
    /// receiver the runner dropped at spawn.
    fn open_stdin(&self) -> Option<&mpsc::Sender<StdinWrite>> {
        self.to_stdin
            .as_ref()
            .filter(|to_stdin| !to_stdin.is_closed())
    }
```

`Actor::begin_send_line` is `begin_action`'s shape, simplified: match, `NotFound`
on empty, one row per id, `NoStdin` where `open_stdin()` is `None`, and the
rest fanned out onto a task that awaits each `done` under
`tokio::time::timeout(STDIN_WRITE_TIMEOUT, …)`:

- `Ok(Ok(Ok(())))` → `Sent`
- `Ok(Ok(Err(err)))` → `NotWritten { reason: err.to_string() }`
- `Ok(Err(_recv))` → `NoStdin` (the writer task ended; the process is gone)
- `Err(_elapsed)` → `NotWritten { reason: format!("the app did not read its
  stdin within {}s", STDIN_WRITE_TIMEOUT.as_secs()) }`

Rows sorted by id, exactly as `spawn_trigger_task` does.

**A reload drainee gets the line**, not a `Skipped`, for the reason Task 4 gives
for a signal: the reply is not a conversation and the drainee is a live process
the selector matched. Note this in `begin_send_line`'s doc so a reviewer
comparing it against `begin_action` sees the difference is deliberate.

`SupervisorHandle::send_line(selector, line)` with the usual `# Errors` block.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.
Expect green, **+5**.

### Step 10.3 — GREEN: the RPC arm

In `crates/shep-daemon/src/rpc.rs`, after the `Signal` arm:

```rust
        Request::SendLine { selector, line } => {
            // Refused here, not silently split by the writer: a line carrying a
            // newline would be delivered as two commands where the operator
            // typed one, and to a REPL the second one is an instruction nobody
            // sent. `\r` too — a line ending in CRLF reaches a shell as a
            // command with a stray carriage return in it.
            if line.contains(['\n', '\r']) {
                return reply(Err(RpcError {
                    code: RpcErrorCode::InvalidConfig,
                    message: "a line may not contain a newline or a carriage return; \
                              send one line per request"
                        .to_string(),
                }));
            }
            match selector_of(selector) {
                Ok(selector) => match ctx.supervisor.send_line(selector, line).await {
                    Ok(rows) => reply(Ok(Response::SentLine(rows))),
                    Err(err) => reply(Err(rpc_error(&err))),
                },
                Err(err) => reply(Err(err)),
            }
        }
```

with a dispatch-tier test that a `\n`-carrying line is refused with
`InvalidConfig` and never reaches the supervisor.

Run. Expect green, **+1**.

### Step 10.4 — MUTATION

In `begin_send_line`'s fan-out task, remove the `tokio::time::timeout(…)`
wrapper and await the `done` receiver directly.

Run `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.

**Must go red:** `a_write_that_never_lands_times_out_and_says_so` fails at its
own `STDIN_WRITE_TIMEOUT * 4` bound with `send_line did not honour its own
bound` — a *failure*, not a hung suite, which is the property IR-46 asks for and
the reason the test wraps the call rather than trusting the code under test to
finish.

Second mutation: change `open_stdin`'s body to `self.to_stdin.as_ref()`, dropping
the `is_closed` filter.

**Must go red:** `a_sheep_without_stdin_answers_no_stdin` fails — the slot holds
a sender whose receiver the runner dropped, so the send fails and the row comes
back `NotWritten` instead of `NoStdin`. That is exactly the "second copy of the
fact" the `open_channel` doc warns about, caught.

Revert both.

### Step 10.5 — CHANGELOG and gate

`crates/shep-daemon/CHANGELOG.md` → `Additions`: "`Request::SendLine` — one
line to a sheep's stdin, per-sheep outcome, bounded at two seconds."

Then the full task gate.

---

## Task 11 — `shep sendline`

**Files modified:**
- `crates/shep-cli/src/cli.rs` — `SendLine(SendLineArgs)`.
- `crates/shep-cli/src/commands/sendline.rs` — new.
- `crates/shep-cli/src/commands/mod.rs`, `main.rs` — wiring.
- `crates/shep-cli/src/output/rows.rs` — `SentLineRows`.
- `crates/shep-daemon/tests/daemon_e2e.rs` — one real-child case.

### Step 11.1 — RED

`crates/shep-cli/src/commands/sendline.rs`'s test module. One shared fixture,
then four cases — the same four `trigger.rs` carries, plus the newline one this
verb needs:

```rust
#[cfg(test)]
mod tests {
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    use super::*;

    fn args(selector: &str, line: &str) -> SendLineArgs {
        SendLineArgs {
            selector: selector.to_string(),
            line: line.to_string(),
        }
    }

    /// Runs the verb against a fake daemon that captures envelopes, and hands
    /// back the exit code, stdout, stderr and the capture channel.
    async fn run(
        args: &SendLineArgs,
    ) -> (
        ExitCode,
        Vec<u8>,
        Vec<u8>,
        tokio::sync::mpsc::Receiver<shep_core::protocol::Envelope>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            sendline(&client, &mut streams, Format::Table, args).await
        };
        (code, out, err, envelopes)
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    /// A verb that skipped the client-side parse would send it and exit
    /// `NotFound` instead of `Usage`, and the daemon would see a request it
    /// never should have.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let (code, _out, _err, mut envelopes) = run(&args("/[/", "gc")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally, never reach the wire"
        );
    }

    /// fails if a line carrying a newline reaches the wire. The daemon refuses
    /// it too, and deliberately in both places — but the operator gets a faster
    /// and more specific answer from the side that knows what they typed, and
    /// this side must not spend a connection to learn it.
    #[tokio::test]
    async fn a_line_with_an_embedded_newline_exits_usage_without_a_round_trip() {
        let (code, _out, err, mut envelopes) = run(&args("repl", "gc\nquit")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a line with a newline must fail locally, never reach the wire"
        );
        let rendered = String::from_utf8(err).unwrap();
        assert!(rendered.contains("one line"), "{rendered}");
    }

    /// fails if a carriage return slips through. `\r` is the one an operator
    /// produces by accident — pasting from a file with CRLF endings — and it
    /// reaches a shell as a command with a stray control character in it.
    #[tokio::test]
    async fn a_line_with_a_carriage_return_is_refused_too() {
        let (code, _out, _err, mut envelopes) = run(&args("repl", "gc\r")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(envelopes.try_recv().is_err());
    }

    /// The envelope's own `body`, not just that the call succeeded. Two
    /// mistakes only this catches: a selector converted with the wrong helper,
    /// and a terminator appended client-side — the wire's contract is that the
    /// line does NOT carry one, and the shepherd's writer is the single place
    /// that adds it.
    #[tokio::test]
    async fn the_request_carries_the_selector_and_the_bare_line() {
        let (_code, _out, _err, mut envelopes) = run(&args("repl", "gc")).await;
        let envelope = envelopes.recv().await.unwrap();
        assert_eq!(
            envelope.body,
            Request::SendLine {
                selector: SelectorSpec::Name("repl".to_string()),
                line: "gc".to_string(),
            }
        );
    }

    /// A response this client does not recognise (the fake daemon's generic
    /// `Pong`, standing in for a `Response` variant this verb's `match` has no
    /// arm for) must not be read as any of the outcomes — it is `Internal`, the
    /// rule every other verb's extract follows for `Response`'s
    /// `#[non_exhaustive]`.
    #[tokio::test]
    async fn an_unrecognised_response_exits_internal() {
        let (code, out, err, _envelopes) = run(&args("repl", "gc")).await;
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }

    /// fails if a `NotFound` reply is swallowed. A selector that matched no
    /// registered sheep is the one way this verb fails as a whole request —
    /// distinct from a matched sheep with no pipe, which is a `no_stdin` ROW.
    #[tokio::test]
    async fn a_not_found_reply_exits_not_found_rather_than_being_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            sendline(&client, &mut streams, Format::Table, &args("ghost", "gc")).await
        };
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty());
    }
}
```

Note what `an_unrecognised_response_exits_internal` and
`the_request_carries_the_selector_and_the_bare_line` share: the same fake, the
same call, different assertions. The fake daemon always answers `Pong`, so the
first proves the extract's fallthrough and the second proves the envelope — and
neither can be collapsed into the other, because a verb that built the wrong
envelope would still get a `Pong` back.

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
struct `SendLineArgs` ``.

### Step 11.2 — GREEN

`crates/shep-cli/src/cli.rs`:

```rust
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
    SendLine(SendLineArgs),
```

Spec §9 spells the verb `sendline`, one word. clap's default for a
`SendLine` variant is `send-line`, so this needs
`#[command(name = "sendline")]` on the variant — and a `cli.rs` test that
`shep sendline` parses and, for good measure, that `shep send-line` does not,
so the spelling is pinned rather than incidental.

```rust
/// Arguments to `shep sendline`.
#[derive(Debug, clap::Args)]
pub struct SendLineArgs {
    /// name, id, `all`, `/regex/`, or `fold:<name>`
    pub selector: String,
    /// The line, without a trailing newline — shep adds exactly one
    pub line: String,
}
```

`commands/sendline.rs` — `trigger.rs`'s shape, with the local newline check
ahead of the round trip (`ExitCode::Usage`, message naming the rule) and the
client's plain default deadline, since the daemon's own bound is 2s.

`output/rows.rs` — `SentLineRows(pub Vec<LineReply>)`, headers
`["ID", "NAME", "OUTCOME", "DETAIL"]`:

- `Sent` → `("sent", String::new())`
- `NoStdin` → `("no_stdin", "no stdin pipe — set stdin = true".to_string())` —
  naming the field for the same reason `TriggeredRows` names `channel = true`:
  the row an operator hits is the one place they learn why.
- `NotWritten { reason }` → `("not_written", reason.clone())`
- wildcard → `("unknown", format!("{outcome:?}"))`, because `LineOutcome` is
  `#[non_exhaustive]`.

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+7**
(the six cases in Step 11.1 plus the verb-spelling one in `cli.rs`).

### Step 11.3 — GREEN: end to end, against a real child

Add to `crates/shep-daemon/tests/daemon_e2e.rs`:

```rust
/// A real pipe, a real child, a real socket. The child echoes what it reads,
/// so the `log.out` frame coming back is proof the line went all the way down
/// and the app's answer came all the way up — the one claim no tier below this
/// can make.
///
/// Bounded (IR-46) at both ends: the reply, and the log frame.
#[tokio::test]
async fn a_line_written_to_a_real_sheeps_stdin_comes_back_on_its_stdout() {
    let mut fixture = Fixture::boot().await;
    let mut sub = fixture.client().await;
    sub.subscribe(vec!["log.out".to_string()]).await;

    let mut app = AppConfig::minimal("echoer", "/bin/sh");
    app.args = vec![
        "-c".to_string(),
        "while IFS= read -r line; do echo \"got $line\"; done".to_string(),
    ];
    app.stdin = true;

    let mut ops = fixture.client().await;
    ops.request(Request::Start { apps: vec![app] }).await;

    let reply = tokio::time::timeout(
        Duration::from_secs(10),
        ops.request(Request::SendLine {
            selector: SelectorSpec::Name("echoer".to_string()),
            line: "ping".to_string(),
        }),
    )
    .await
    .expect("no reply to send_line within 10s");

    let Ok(Response::SentLine(rows)) = reply else {
        panic!("expected SentLine, got {reply:?}");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, LineOutcome::Sent);

    // `Sent` only claims the bytes reached the pipe. This is the half that
    // proves the app actually read them.
    let echoed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let BusEvent::LogOut { line, .. } = sub.next_event().await {
                if line.contains("got ping") {
                    break line;
                }
            }
        }
    })
    .await
    .expect("the app never echoed the line within 10s");
    assert_eq!(echoed, "got ping");

    fixture.shutdown().await;
}
```

Adapt `Fixture`/`client()`/`request`/`subscribe`/`next_event` to that file's own
helper names — read them first; the shapes above are the intent. Task 2's
`channel.*` case is the nearest sibling and was written against the same
helpers.

Run `cargo test -p shep-daemon --test daemon_e2e --all-features`. Expect
green, **+1**.

### Step 11.4 — MUTATION

In `crates/shep-cli/src/commands/sendline.rs`, change the request body's
`line: args.line.clone()` to `line: format!("{}\n", args.line)`.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `the_request_carries_the_selector_and_the_bare_line` fails on
the envelope comparison. If it stays green, that test is comparing something
weaker than the whole `Request::SendLine` body and needs fixing before it can
prove anything.

Then run `cargo test -p shep-daemon --test daemon_e2e --all-features` with the
same mutation still applied — the e2e must **also** go red, on the daemon's
newline refusal (`InvalidConfig`), which proves the two checks are independent
rather than one written twice.

Revert.

### Step 11.5 — CHANGELOG and gate

`crates/shep-cli/CHANGELOG.md` → `Additions`: "`shep sendline <selector>
<line>`, for apps whose Flockfile sets `stdin = true`."

Then the full task gate.

---

## Task 12 — `shep_core::kv`, the file-locked store

**Files created:**
- `crates/shep-core/src/kv.rs`

**Files modified:**
- `crates/shep-core/src/lib.rs` — declare the module.
- `crates/shep-core/src/paths.rs` — `ShepPaths::kv`.

**Produces, for Tasks 13–14:**

```rust
// crates/shep-core/src/kv.rs
pub const KV_VERSION: u32 = 1;
pub const MAX_KEY_BYTES: usize = 128;
pub const MAX_VALUE_BYTES: usize = 4096;

pub fn all(path: &Path) -> Result<BTreeMap<String, String>, KvError>;
pub fn get(path: &Path, key: &str) -> Result<Option<String>, KvError>;
pub fn set(path: &Path, key: &str, value: &str) -> Result<(), KvError>;
pub fn unset(path: &Path, key: &str) -> Result<bool, KvError>;
pub fn clear(path: &Path) -> Result<u32, KvError>;

#[non_exhaustive] pub enum KvError { Io(io::Error), Decode(serde_json::Error),
                                     InvalidKey(String), ValueTooLong { key: String, len: usize },
                                     FutureVersion(u32) }

// crates/shep-core/src/paths.rs
ShepPaths::kv: PathBuf     // $SHEP_HOME/kv.json
```

### Step 12.0 — the shape to copy

Read `crates/shep-core/src/barks.rs` first, specifically `RingLock`
(line 291), `lock_path` (line 363), `create_ring_file` (line 278) and
`write_ring` (line 246). This module is the third instance of that pattern and
must not invent a fourth. Concretely:

- exclusive `flock(2)` on a **sibling** `kv.json.lock`, held across the whole
  read-modify-write, because the `rename` that installs new content replaces
  the inode a lock on the target would be held on;
- a **uniquely-named** temp file in the same directory, not a fixed `.tmp` —
  a shared name had one writer's `rename` consume the other's staging file and
  kill the loser with `ENOENT`;
- mode `0600` **at creation**, via `tempfile::Builder::permissions`, not a
  `chmod` afterwards — no window at whatever the umask leaves;
- `sync_all`, then `persist` (`rename(2)`);
- `#[cfg(unix)]` on the lock, with the documented Windows no-op and the same
  reasoning `RingLock` already carries.

### Step 12.1 — RED: the path

Add to `crates/shep-core/src/paths.rs`'s existing
`default_layout_under_home_dir` test:

```rust
        assert_eq!(p.kv, Path::new("/home/rin/.shep/kv.json"));
```

Run `cargo test -p shep-core --lib --all-features`.

**Expected failure — for the stated reason:** compile error, ``no field `kv` on
type `ShepPaths` ``. (This is an *existing* test gaining an assertion rather
than a new test, deliberately: the layout is one fact and this file is where it
is pinned.)

### Step 12.2 — GREEN: the path

`ShepPaths` gains, after `barks`:

```rust
    /// Key/value store: `kv.json`
    pub kv: PathBuf,
```

set in `resolve` as `kv: home.join("kv.json"),`. Run; green.

### Step 12.3 — RED: the store

Create `crates/shep-core/src/kv.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a set value cannot be read back, or if the file is not created
    /// on first write. Everything else here is a refusal or a race; this is the
    /// one case that says the store stores.
    #[test]
    fn a_value_survives_a_write_and_a_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "bark.cooldown", "30s").unwrap();
        assert_eq!(get(&path, "bark.cooldown").unwrap(), Some("30s".to_string()));
    }

    /// fails if a missing store is an error rather than an empty one. `shep get`
    /// against a fresh `$SHEP_HOME` is the first thing anyone runs, and an
    /// `ENOENT` in their face would be wrong: the store has no keys, which is
    /// a fact, not a failure.
    #[test]
    fn a_store_that_does_not_exist_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        assert!(all(&path).unwrap().is_empty());
        assert_eq!(get(&path, "anything").unwrap(), None);
    }

    /// fails if `unset` stops distinguishing a key it removed from one that was
    /// never there. `shep unset typo` has to be able to say so rather than
    /// exiting 0 on a no-op the operator will read as success.
    #[test]
    fn unset_reports_whether_the_key_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "a", "1").unwrap();
        assert!(unset(&path, "a").unwrap());
        assert!(!unset(&path, "a").unwrap());
    }

    /// fails if `clear` misreports how much it removed, or leaves anything.
    #[test]
    fn clear_empties_the_store_and_counts_what_it_took() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "a", "1").unwrap();
        set(&path, "b", "2").unwrap();
        assert_eq!(clear(&path).unwrap(), 2);
        assert!(all(&path).unwrap().is_empty());
        assert_eq!(clear(&path).unwrap(), 0);
    }

    /// fails if the key grammar widens. Each rejection here is deliberate: a
    /// key goes onto a shell command line (`shep get $k`) and into a JSON
    /// object, so whitespace, control characters and an empty name all have to
    /// be refused at the door rather than quoted around forever.
    #[test]
    fn the_key_grammar_refuses_what_it_says_it_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        for bad in ["", " ", "a b", "a\nb", "a/b", "a:b", ".hidden", "a\"b", "$HOME"] {
            assert!(
                matches!(set(&path, bad, "1"), Err(KvError::InvalidKey(_))),
                "`{bad}` was accepted as a key"
            );
        }
        for good in ["a", "bark.cooldown", "metrics_port", "a-b", "A1.b-c_d"] {
            assert!(set(&path, good, "1").is_ok(), "`{good}` was refused");
        }
    }

    /// fails if a key that merely CONTAINS a dot is treated as a path into a
    /// nested object. `bark.cooldown` is one key whose name has a dot in it —
    /// the store is flat, and the dot is a naming convention, not a grammar.
    #[test]
    fn a_dotted_key_is_one_flat_key_and_not_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "bark.cooldown", "30s").unwrap();
        set(&path, "bark.sink", "discord").unwrap();
        let stored = all(&path).unwrap();
        assert_eq!(stored.len(), 2);
        assert!(stored.contains_key("bark.cooldown"));
        assert_eq!(get(&path, "bark").unwrap(), None);
        // And on disk, not just in the map: a nested writer would produce
        // `{"bark":{"cooldown":…}}` and this is what notices.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(r#""bark.cooldown""#), "{raw}");
    }

    /// fails if an oversized value is stored. The store is `$SHEP_HOME`'s
    /// smallest file and is read whole on every access; a cap keeps it from
    /// quietly becoming a blob store.
    #[test]
    fn an_oversized_value_is_refused_by_name_and_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        let big = "x".repeat(MAX_VALUE_BYTES + 1);
        let err = set(&path, "a", &big).unwrap_err();
        let KvError::ValueTooLong { key, len } = err else {
            panic!("expected ValueTooLong, got {err:?}");
        };
        assert_eq!(key, "a");
        assert_eq!(len, MAX_VALUE_BYTES + 1);
    }

    /// fails if a store written by a future shep is silently overwritten. This
    /// file is small but it is an operator's, and clobbering it on a downgrade
    /// would be an unrecoverable loss for no gain.
    #[test]
    fn a_store_from_a_future_shep_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        std::fs::write(&path, r#"{"version":99,"entries":{"a":"1"}}"#).unwrap();
        assert!(matches!(all(&path), Err(KvError::FutureVersion(99))));
        assert!(matches!(set(&path, "b", "2"), Err(KvError::FutureVersion(99))));
        // Untouched, which is the half that matters.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(r#""a":"1""#), "{raw}");
    }

    /// fails if the file is created group- or world-readable. `$SHEP_HOME` is
    /// already `0700`, so this is belt-and-braces — and it is the mode a `tar`,
    /// a `cp -p` or a backup carries out of that directory with the file, where
    /// no directory mode follows it. Same argument `barks.jsonl` records.
    #[cfg(unix)]
    #[test]
    fn the_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        set(&path, "a", "1").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    /// fails if two concurrent writers lose each other's keys. This is not a
    /// theoretical race: `barks.jsonl` lost half of 400 records to exactly this
    /// shape before it grew the same advisory lock, and the store has the same
    /// two-writer future (an operator's `shep set` and a dog's own).
    ///
    /// Bounded (IR-46): the join is under a timeout, so a lock that deadlocks
    /// fails this test instead of hanging the suite.
    #[test]
    fn two_concurrent_writers_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        const PER_WRITER: usize = 100;

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        for writer in 0..2 {
            let path = path.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                for n in 0..PER_WRITER {
                    set(&path, &format!("w{writer}.k{n}"), "v").unwrap();
                }
                done_tx.send(()).unwrap();
            });
        }
        drop(done_tx);
        for _ in 0..2 {
            done_rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("a writer did not finish within 60s");
        }

        assert_eq!(all(&path).unwrap().len(), PER_WRITER * 2);
    }
}
```

Add `pub mod kv;` to `crates/shep-core/src/lib.rs`.

Run `cargo test -p shep-core --lib --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
function `set` in this scope`` and friends.

### Step 12.4 — GREEN: the store

Prepend to `crates/shep-core/src/kv.rs`:

```rust
//! `kv.json`: the shepherd's key/value store (spec §5).
//!
//! A flat map of short strings under `$SHEP_HOME`, for ad-hoc operator notes
//! and dog runtime tweaks. Explicitly **not** the primary config path — a
//! Flockfile is what configures a sheep and `shep.toml` is what configures the
//! shepherd and its dogs. This is the place for the things neither of those
//! has a field for.
//!
//! # Why this is a file and not an RPC
//!
//! Spec §5 says the store is for "ad-hoc + dog runtime tweaks", so a dog reads
//! it — which rules out keeping it private to shep-cli, and is why it lives
//! here, where every crate in the workspace and every `shep dog <name>` gets it
//! for free. It does NOT follow that it has to go over the socket. A dog's
//! `[dog.<name>]` section travels that way because the alternative on the table
//! was the child's ENVIRONMENT, which is readable from the process table,
//! inherited by every grandchild and captured into crash dumps (spec §8). A
//! `0600` file inside a `0700` `$SHEP_HOME`, opened by a process running as the
//! same user, has none of those properties, so the socket would buy nothing —
//! while costing the thing every other config verb in this tree provides:
//! `shep set` works with no shepherd running, exactly as `shep enable` and
//! `shep barks` do.
//!
//! # Writing
//!
//! Every mutation is a read-modify-rename under an exclusive advisory lock on a
//! sibling `kv.json.lock`, with the new content staged through a uniquely-named
//! `0600` temp file, `fsync`ed and `rename`d over the original. That is the
//! same shape `barks::append` uses, for the same reasons and after the same
//! bug: two processes appending to `barks.jsonl` silently lost half of each
//! other's records until an advisory lock landed there, and a shared temp name
//! had one writer's `rename` consume the other's staging file. Do not
//! reimplement either half here — it is a third instance of one pattern, not a
//! third pattern.
//!
//! # Keys
//!
//! One flat string per key, matching `[A-Za-z0-9._-]`, 1 to
//! [`MAX_KEY_BYTES`], not starting with `.`. A dot is part of a NAME, not a
//! path: `bark.cooldown` is one key, and there is no nested object behind it.
//! map.md inherited a dotted-path parse from pm2's own store; this project's
//! standing decision is that pm2's formats live only in the importer, and a
//! nesting grammar here would be a second config language — with its own
//! quoting rules — for a store the spec itself calls not the primary config
//! path. The narrow alphabet also means `shep get $key` never needs quoting.
```

Then the constants, the file struct, the errors and the five functions. The
parts worth spelling out:

```rust
/// The on-disk format's version.
///
/// A store carrying a HIGHER version is refused rather than read or replaced
/// ([`KvError::FutureVersion`]): the file is small, it is an operator's, and
/// there is no undo for a downgrade that overwrites it. The muster roll's
/// `SNAPSHOT_VERSION` is the precedent.
pub const KV_VERSION: u32 = 1;

/// Longest key this store accepts, in bytes.
pub const MAX_KEY_BYTES: usize = 128;

/// Longest value this store accepts, in bytes.
///
/// The store is read whole on every access, and a cap is what keeps it from
/// quietly becoming a blob store — which it would, because it is the only
/// writable thing in `$SHEP_HOME` with no schema.
pub const MAX_VALUE_BYTES: usize = 4096;

/// The file's shape: a version and a flat map.
///
/// `BTreeMap`, not `HashMap`, so the file is written in key order and two
/// writes of the same content produce byte-identical files — which makes the
/// store diffable, greppable, and safe to keep in a dotfiles repository.
#[derive(Debug, Default, Serialize, Deserialize)]
struct KvFile {
    version: u32,
    entries: BTreeMap<String, String>,
}
```

```rust
/// Error type returned by this module.
///
/// `#[non_exhaustive]`: shep-core is a published library and this enum is
/// reachable from it, so a further failure shape — a store whose size exceeded
/// a future cap, say — must not break an out-of-tree consumer's `match`
/// (IR-20).
///
/// Wraps `io::Error`/`serde_json::Error` directly rather than stringifying
/// them, matching [`BarkError`](crate::barks::BarkError), so callers keep the
/// underlying diagnostic through [`core::error::Error::source`] — at the cost,
/// documented there too, of not deriving `Clone`/`PartialEq`/`Eq` (IR-19's
/// exception for variants wrapping `io::Error`).
#[non_exhaustive]
#[derive(Debug)]
pub enum KvError {
    /// The store could not be read, written, or replaced.
    Io(std::io::Error),
    /// The store's JSON could not be parsed.
    ///
    /// Refused rather than repaired: unlike `barks.jsonl`, which is read during
    /// an incident and so forgives a bad line, this file is a map an operator
    /// wrote and a partial read of it would silently drop keys that are still
    /// on disk.
    Decode(serde_json::Error),
    /// A key outside the grammar; carries it verbatim so the message can quote
    /// what was typed.
    InvalidKey(String),
    /// A value over [`MAX_VALUE_BYTES`].
    ValueTooLong {
        /// The key it was being stored under.
        key: String,
        /// Its length in bytes.
        len: usize,
    },
    /// The store on disk is a version this build does not understand; carries
    /// that version. Nothing was written.
    FutureVersion(u32),
}
```

```rust
/// Checks one key against the grammar.
///
/// # Errors
/// [`KvError::InvalidKey`] — empty, over [`MAX_KEY_BYTES`], starting with `.`,
/// or containing anything outside `[A-Za-z0-9._-]`.
fn check_key(key: &str) -> Result<(), KvError> {
    let ok = !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && !key.starts_with('.')
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(KvError::InvalidKey(key.to_string()))
    }
}
```

`read_file` returns `KvFile::default()` for a missing file (`ErrorKind::NotFound`
only — every other io error propagates), refuses a higher `version`, and is
called under the lock by every mutating function. `write_file` is `barks`'
`write_ring` with `serde_json::to_writer_pretty` and a trailing newline instead
of the line loop. `KvLock` is `RingLock` with the file name changed; its doc
should say "the same lock `barks::RingLock` documents, on this file" and point
there rather than restating the inode argument a third time.

`all` and `get` take the lock too. A reader that skipped it could observe the
old inode mid-`rename` — harmless, since the rename is atomic and it would read
a whole old file — but taking it costs one `open` and removes the question
entirely; say so in a comment so the next reader does not "optimize" it away
without knowing what it is buying.

Run `cargo test -p shep-core --lib --all-features`. Expect green, **+10**.

### Step 12.5 — MUTATION

Two.

**One:** in `check_key`, change `&& !key.starts_with('.')` to `&& true`.

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `the_key_grammar_refuses_what_it_says_it_refuses` fails on
`.hidden`. Every other case in that test — and
`a_dotted_key_is_one_flat_key_and_not_a_path`, which uses a dot in the middle —
must stay **green**: this mutation widens exactly one rule, and a test that
reddened on all of them would be measuring "check_key rejects things".

**Two:** in `KvLock::acquire`, replace the lock with `Ok(Self {})` (keep the
temp-file and rename path intact, so only the lock is gone).

Run `cargo test -p shep-core --lib --all-features` **three times** —
`two_concurrent_writers_lose_nothing` is a race, and a single green run of a
race proves nothing.

**Must go red on at least one run:** `two_concurrent_writers_lose_nothing` fails
its final `assert_eq!(…, 200)` with a number below 200, each writer's rename
having discarded keys the other had just added. If three runs all pass, raise
`PER_WRITER` until it fails, then restore the lock and confirm it passes at that
higher number — a race test that cannot be made to fail is not testing the lock.

Revert both.

### Step 12.6 — CHANGELOG and gate

`crates/shep-core/CHANGELOG.md` → `Additions`: "`kv` — the file-locked
key/value store at `$SHEP_HOME/kv.json` (spec §5), and `ShepPaths::kv`."

Then the full task gate.

---

## Task 13 — `shep set` / `shep get` / `shep unset`

**Files created:**
- `crates/shep-cli/src/commands/kv.rs`

**Files modified:**
- `crates/shep-cli/src/cli.rs` — three subcommands and their args.
- `crates/shep-cli/src/commands/mod.rs`, `main.rs` — wiring.
- `crates/shep-cli/src/output/rows.rs` — `KvRows`.

**Consumes from Task 12:** `shep_core::kv`, `ShepPaths::kv`.

### Step 13.1 — RED: the clap surface

Add to `crates/shep-cli/src/cli.rs`'s test module:

```rust
/// fails if `shep unset` with no key and no --all is accepted. It would have to
/// mean either nothing or everything, and the everything reading is
/// unrecoverable.
#[test]
fn unset_needs_a_key_or_the_all_flag() {
    assert!(Cli::try_parse_from(["shep", "unset"]).is_err());
    assert!(Cli::try_parse_from(["shep", "unset", "a"]).is_ok());
    assert!(Cli::try_parse_from(["shep", "unset", "--all"]).is_ok());
}

/// fails if `--all` composes with a key. `shep unset a --all` would be an
/// operator asking for one thing and a flag doing something far larger —
/// the same conflict `shep flush all --daemon` is a usage error for.
#[test]
fn unset_refuses_a_key_and_all_together() {
    assert!(Cli::try_parse_from(["shep", "unset", "a", "--all"]).is_err());
}

/// fails if `shep get` starts requiring a key. Bare `get` listing the whole
/// store is the discovery path — an operator who does not remember what they
/// set has nowhere else to look.
#[test]
fn get_takes_an_optional_key() {
    assert!(Cli::try_parse_from(["shep", "get"]).is_ok());
    assert!(Cli::try_parse_from(["shep", "get", "a"]).is_ok());
}

/// fails if `set` becomes anything but two required positionals. A `set` with a
/// defaultable value would let `shep set a` silently store an empty string.
#[test]
fn set_needs_both_a_key_and_a_value() {
    assert!(Cli::try_parse_from(["shep", "set", "a"]).is_err());
    assert!(Cli::try_parse_from(["shep", "set", "a", "1"]).is_ok());
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** clap does not know these
subcommands, so `try_parse_from` returns `Err` for **all** of them — including
the three lines asserting `is_ok()`. That is the right red and it is worth
naming: the first failing assertion will be an `is_ok()` one, not an `is_err()`
one.

### Step 13.2 — GREEN: the verbs

`crates/shep-cli/src/cli.rs`, grouped together after `Barks`:

```rust
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
```

```rust
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
```

`crates/shep-cli/src/commands/kv.rs` — three functions taking `&ShepPaths`, no
`Client` anywhere in the module (`dogs::barks` is the precedent: read it for the
shape):

```rust
pub fn set(streams: &mut Streams<'_>, fmt: Format, paths: &ShepPaths, args: &KvSetArgs) -> ExitCode;
pub fn get(streams: &mut Streams<'_>, fmt: Format, paths: &ShepPaths, args: &KvGetArgs) -> ExitCode;
pub fn unset(streams: &mut Streams<'_>, fmt: Format, paths: &ShepPaths, args: &KvUnsetArgs) -> ExitCode;
```

Exit codes, mapped from `KvError` and stated here so they are a decision rather
than whatever fell out:

| Case | Code |
|---|---|
| `InvalidKey` / `ValueTooLong` | `Usage` (2) — the operator typed it |
| `FutureVersion` | `InvalidConfig` (4) — the file on disk is the problem |
| `Decode` | `InvalidConfig` (4) — same |
| `Io` | `Failure` (1) |
| `get <key>` where the key is absent | `NotFound` (3) |
| `unset <key>` where the key is absent | `NotFound` (3) |

`shep get` on a missing key exiting `NotFound` rather than 0-with-nothing is
what makes `shep get k || echo default` work in a script, which is the shape
this store exists to serve.

`output/rows.rs` — `KvRows(pub Vec<(String, String)>)`, headers `["KEY",
"VALUE"]`, `json_key_for` mapping to `key`/`value`. Serialize as a list of
objects rather than as a JSON map: the envelope's `data` is an array for every
other verb in this binary, and one verb answering with an object would be the
only thing a consumer has to special-case.

Add unit tests in `commands/kv.rs` against a `tempfile::tempdir` `$SHEP_HOME`:
set-then-get round trip; `get` on an absent key exits `NotFound` and writes
nothing to stdout; `unset --all` empties the store; a bad key exits `Usage`
without creating the file at all.

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+8**
(four clap cases, four command cases).

### Step 13.3 — MUTATION

In `crates/shep-cli/src/cli.rs`, remove `conflicts_with = "all"` from
`KvUnsetArgs::key`.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `unset_refuses_a_key_and_all_together` fails —
`shep unset a --all` now parses. `unset_needs_a_key_or_the_all_flag` must stay
**green**, since `required_unless_present` is untouched, and that separation is
why the two properties are two tests.

Revert. Second mutation: in `commands/kv.rs`, change `get`'s absent-key arm to
return `ExitCode::Success`.

**Must go red:** the `get`-on-an-absent-key case fails on its exit code. If it
does not, that test is asserting only on stdout and needs the code assertion
added before it can prove anything.

Revert.

### Step 13.4 — CHANGELOG and gate

`crates/shep-cli/CHANGELOG.md` → `Additions`: "`shep set` / `shep get` /
`shep unset` (spec §5's KV store). They read and write
`$SHEP_HOME/kv.json` directly and never connect to the shepherd."

Then the full task gate.

---

## Task 14 — the KV store end to end

**Files modified:**
- `crates/shep-cli/tests/cli_e2e.rs`
- `docs/kv.md` — new, the operator-facing contract.
- `README.md` — the verb list.

### Step 14.1 — the e2e

Add to `crates/shep-cli/tests/cli_e2e.rs`, in the style of the file's existing
no-daemon cases (`shep barks` is the closest):

```rust
/// The whole store, through the real binary, with no shepherd anywhere. That
/// last part is the assertion that matters: `shep set` has to work on a machine
/// where nothing is running, because that is when provisioning happens.
#[test]
fn the_kv_store_works_with_no_shepherd_running() {
    // fresh $SHEP_HOME under a tempdir; no `shep daemon` started anywhere
    // shep set bark.cooldown 30s        -> 0
    // shep get bark.cooldown            -> 0, stdout contains 30s
    // shep get missing                  -> 3 (not found)
    // shep set metrics_port 9615        -> 0
    // shep get                          -> 0, stdout contains both keys
    // shep unset bark.cooldown          -> 0
    // shep get bark.cooldown            -> 3
    // shep unset --all                  -> 0
    // shep get                          -> 0, and lists nothing
    // shep set "bad key" x              -> 2 (usage)
}
```

Check the JSON surface too, in the same case or a sibling:
`shep --format json get` parses as an object whose `data` is an **array**, and
whose `schema_version` is `SCHEMA_VERSION` — the envelope shape every other
verb produces.

Run `cargo test -p shep-cli --test cli_e2e --all-features`. Expect green,
**+1 or +2**.

### Step 14.2 — the operator doc

Create `docs/kv.md`, following `docs/dogs.md`'s register (operator-facing,
plain, no theme in the error text). It has to cover, because nothing else will:

- what the store is for, and what it is **not** — a Flockfile configures a
  sheep, `shep.toml` configures the shepherd and its dogs, this is for what
  neither has a field for;
- the three verbs, with a worked example;
- the key grammar, and that a dot is part of a name;
- that a value is a string, capped at 4 KiB;
- that the file is `$SHEP_HOME/kv.json`, mode `0600`, and safe to keep in a
  dotfiles repository (`BTreeMap` ordering makes it stable across writes);
- that the store works with no shepherd running, and that a dog reads it
  through `shep_core::kv` rather than over the socket, with the reasoning in
  one sentence;
- that concurrent writers are serialised by an advisory lock, so two
  provisioning scripts cannot lose each other's keys.

Link it from `README.md` beside `docs/dogs.md`, and add the three verbs to
whatever verb list that file carries.

Baseline before editing:

```bash
grep -c "docs/dogs.md" README.md          # non-zero at HEAD; the anchor to add beside
grep -c "shep set" README.md              # 0 at HEAD
```

After: the second must be non-zero.

### Step 14.3 — gate

No mutation step: this task adds no branch. The e2e in 14.1 is itself the check,
and it fails if any of the ten invocations returns the wrong code.

Full task gate.

---

## Task 15 — `Lamb`, and `ProcessInfo::lambs`

**Files modified:**
- `crates/shep-core/src/protocol/request.rs` — `Lamb`, the field, the builder
  setter, fixtures.

**Produces, for Tasks 16–17:**

```rust
#[non_exhaustive]
pub struct Lamb { pub pid: u32, pub name: String }
impl Lamb { pub fn new(pid: u32, name: impl Into<String>) -> Self; }

ProcessInfo::lambs: Option<Vec<Lamb>>
ProcessInfoBuilder::lambs(self, lambs: Option<Vec<Lamb>>) -> Self
```

> **This task moves two pinned snapshots** (`reply_wire_v1` and
> `bus_event_wire_v1` both carry `ProcessInfo` rows). Do not run it
> concurrently with Task 8.

### Step 15.1 — RED

Add to `crates/shep-core/src/protocol/request.rs`'s test module:

```rust
/// fails if `lambs` collapses to a bare `Vec`. The three states are the point:
/// a peer that predates the field and a reply that did not walk the tree are
/// both `None`, and a sheep that really has no children is `Some(vec![])`. A
/// `Vec` would render the first two as "this sheep has no lambs", which is a
/// claim neither of them makes.
#[test]
fn lambs_distinguishes_not_walked_from_walked_and_empty() {
    let not_walked = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
    assert_eq!(not_walked.lambs, None);

    let walked_empty = ProcessInfo::builder(1, "web", ProcStatus::Online)
        .lambs(Some(Vec::new()))
        .build();
    assert_eq!(walked_empty.lambs, Some(Vec::new()));
}

/// fails if a `ProcessInfo` from a daemon that predates the field stops
/// deserializing. That is the whole reason the field is optional and the reason
/// `PROTOCOL_VERSION` does not move for it — an old daemon's reply carries no
/// `lambs` key at all, and a required field there would mean a new client could
/// not list against an old daemon.
#[test]
fn a_process_info_without_a_lambs_key_still_deserializes() {
    let fixture = r#"{
        "id": 3, "name": "web", "status": "online", "pid": 4242,
        "restarts": 0, "uptime_ms": 100, "fold": null,
        "out_file": null, "err_file": null,
        "cpu_percent": null, "memory_bytes": null, "dog": null
    }"#;
    let info: ProcessInfo = serde_json::from_str(fixture).unwrap();
    assert_eq!(info.lambs, None);
}

/// fails if a lamb stops carrying its name, or starts carrying a command line.
/// The name is `sysinfo`'s executable name, never argv — argv routinely holds
/// credentials (`--password=`, `?token=`) and `shep describe --format json` is
/// output people paste into issues.
#[test]
fn a_lamb_is_a_pid_and_an_executable_name() {
    let lamb = Lamb::new(4243, "node");
    let json = serde_json::to_string(&lamb).unwrap();
    assert_eq!(json, r#"{"pid":4243,"name":"node"}"#);
    assert_eq!(serde_json::from_str::<Lamb>(&json).unwrap(), lamb);
}
```

Run `cargo test -p shep-core --lib --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
struct `Lamb` ``.

### Step 15.2 — GREEN

```rust
/// One process the OS reports as a descendant of a sheep.
///
/// # What this is not
///
/// It is **not** the set of processes that die with the sheep, and nothing here
/// should be read as promising that. The list is built by walking the OS's
/// parent-pid links; the stop ladder acts on the process GROUP, and the two
/// units diverge in both directions — a lamb that forks and exits leaves its
/// own children re-parented to init, out of this list and still in the group,
/// while a `setsid()` grandchild stays in this list and leaves the group.
/// shep-daemon's `limits` module doc has the full account, and it is the
/// authority; this is a pointer to it, not a second copy free to drift.
///
/// # Why a name and not a command line
///
/// `name` is the executable's name as the OS reports it (`node`, `sh`,
/// `python3`), never its argument vector. A process's argv routinely carries
/// credentials — a `--password=` flag, a URL with a token in the query string —
/// and this field rides in `shep describe --format json`, which is output
/// people paste into bug reports. A pid alone would be safe too, and was
/// considered; it was rejected because a tree of bare integers sends the
/// operator to `ps`, which is the work the tree exists to save.
///
/// # Why no memory figure
///
/// The sheep's own row already reports its whole tree's resident size
/// ([`ProcessInfo::memory_bytes`]), and a per-lamb breakdown is a profiler's
/// job. `deferred.md`'s note on this struct's growth asks for exactly this
/// restraint.
///
/// `#[non_exhaustive]`: shep-core is a published library, this type is new, and
/// the two obvious next fields (a parent pid, so a deep tree can be nested
/// rather than flattened; a start time) would otherwise be breaking additions
/// (IR-20). Build one with [`Self::new`].
// wire format: changing this is a breaking change
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lamb {
    /// The lamb's own pid.
    pub pid: u32,
    /// The executable's name, as the OS reports it. Never its command line.
    pub name: String,
}

impl Lamb {
    /// One lamb.
    ///
    /// A plain constructor rather than a builder, unlike [`ProcessInfo`]: both
    /// fields are required and neither is optional or derived, which is the
    /// case a builder buys nothing for.
    #[must_use]
    pub fn new(pid: u32, name: impl Into<String>) -> Self {
        Self {
            pid,
            name: name.into(),
        }
    }
}
```

`ProcessInfo` gains, after `dog`:

```rust
    /// The processes the OS reports as descendants of this sheep, or `None`
    /// when this reply did not walk for them.
    ///
    /// `None` covers two cases and is deliberately not a third: this reply is
    /// not a `Describe` (only `Describe` walks — the walk costs a second pass
    /// over the machine's process table, and a flock listing is the thing an
    /// operator leaves running in a loop), or the peer daemon predates the
    /// field. `Some(vec![])` is the third case, and the one that means what it
    /// looks like: walked, and this sheep has no children.
    ///
    /// Read [`Lamb`]'s own doc before rendering this. The list is a parent-pid
    /// walk and is NOT the set of processes a stop kills; any output built from
    /// it has to say so where the operator will see it.
    pub lambs: Option<Vec<Lamb>>,
```

Add `lambs: None` to `ProcessInfo::builder`'s initializer, and the setter:

```rust
    /// Sets the sheep's lamb list; `None` when this reply did not walk for one.
    pub fn lambs(mut self, lambs: Option<Vec<Lamb>>) -> Self {
        self.info.lambs = lambs;
        self
    }
```

Extend the `#[non_exhaustive]` rationale on `ProcessInfo` — it currently says
"`deferred.md` already names the next one — `lambs`, for `describe`'s tree
view". That sentence is now history rather than a forecast; rewrite it to say
the field landed under the attribute without a sweep, which is the attribute
paying for itself, and name whatever the next candidate is (or say there is
none).

Export `Lamb` from `protocol/mod.rs`.

Run `cargo test -p shep-core --lib --all-features`.

**Expect two snapshot failures**, `reply_wire_v1` and `bus_event_wire_v1`. Review
both diffs: each must be **`"lambs": null` added once per `ProcessInfo` object
and nothing else**. Then accept and check:

```bash
INSTA_UPDATE=always cargo test -p shep-core --lib --all-features
git diff crates/shep-core/src/protocol/snapshots/ | grep -c '^-'
```

That last command's baseline at HEAD is `0` (a clean tree diffs to nothing), and
after the accept it must **still print 0** — every changed line is an addition.
A non-zero count means an existing pinned line moved, which this task does not
do.

Then add one row to `reply_wire_snapshots` carrying a `ProcessInfo` with a
**populated** `lambs`, so the non-null shape is pinned as well as the null one:

```rust
        // A `Described` row with a real lamb tree. The `null` shape is pinned
        // on every other row here; this is the one that pins what a walked
        // sheep serializes as, which is the shape a `describe` consumer
        // actually parses.
        replies.push(Reply {
            id: 12,
            result: Ok(Response::Described(vec![
                ProcessInfo::builder(3, "web", ProcStatus::Online)
                    .pid(Some(4242))
                    .lambs(Some(vec![Lamb::new(4243, "node"), Lamb::new(4244, "sh")]))
                    .build(),
            ])),
        });
```

Accept, re-check the deletion count, then green, **+3**.

### Step 15.3 — MUTATION

Change the field to `pub lambs: Vec<Lamb>` with `#[serde(default)]`, updating
the builder and the tests' types minimally so it compiles.

Run `cargo test -p shep-core --lib --all-features`.

**Must go red:** `lambs_distinguishes_not_walked_from_walked_and_empty` fails —
`not_walked.lambs` and `walked_empty.lambs` are now the same value, which is the
exact conflation the `Option` exists to prevent.
`a_process_info_without_a_lambs_key_still_deserializes` stays **green** (the
`serde(default)` covers it), which is worth noticing: skew compatibility alone
would not have caught this, and the three-state test is what does.

Revert.

### Step 15.4 — CHANGELOG and gate

`crates/shep-core/CHANGELOG.md` → `Additions`: "`Lamb` and
`ProcessInfo::lambs` — a sheep's process-tree members, populated by `Describe`
only. `None` means the reply did not walk for them, never that there are none."

Then the full task gate.

---

## Task 16 — the shepherd walks a sheep's lamb tree

**Files modified:**
- `crates/shep-daemon/src/limits/sample.rs` — `ProcessIdentity`,
  `MemorySampler::identify`, `SysinfoSampler`'s implementation.
- `crates/shep-daemon/src/limits/stats.rs` — `StatsState::lambs_of`.
- `crates/shep-daemon/src/rpc.rs` — `with_lambs`, applied to `Describe` only.

**Consumes from Task 15:** `Lamb`, `ProcessInfo::lambs`.

### Step 16.0 — do not widen `ProcessRss`

`ProcessRss` is `Copy` and is the row type of a whole-machine table the polling
enforcer walks every 15 seconds. Adding a `String` to it would take the `Copy`
away and allocate once per process on the machine, per tick, for a field only
`describe` ever reads.

So identity is a **second, on-demand method** on the same trait, with a default
returning nothing. That default is what keeps the addition non-breaking for an
out-of-tree implementor of a `pub` trait, and it is honest — a sampler that
cannot report names says so, and `describe` renders no lamb rows rather than
wrong ones.

The cost is one extra process-table refresh per `describe`. That is an operator
command, not a poll, and `with_live_stats` already does one; say so in the
method's doc rather than sharing a table between two callers with different
lifetimes.

### Step 16.1 — RED

Add to `crates/shep-daemon/src/limits/stats.rs`'s test module:

```rust
/// fails if the walk includes the sheep's own pid. They are LAMBS — the sheep
/// is the row this list hangs off, and repeating it there would double it in
/// every rendering.
#[tokio::test]
async fn the_lamb_walk_excludes_the_sheeps_own_pid() {
    let table = vec![
        identity(100, None, "srv"),
        identity(101, Some(100), "node"),
        identity(102, Some(101), "sh"),
    ];
    let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));

    let lambs = stats.lambs_of(100);

    assert_eq!(
        lambs,
        vec![Lamb::new(101, "node"), Lamb::new(102, "sh")],
        "the root pid must not appear among its own lambs"
    );
}

/// fails if the walk stops at the first generation. A `sh` wrapper that execs a
/// runtime that forks workers is three deep and is the ordinary case, not an
/// exotic one.
#[tokio::test]
async fn the_lamb_walk_reaches_every_generation() {
    let table = vec![
        identity(100, None, "sh"),
        identity(101, Some(100), "node"),
        identity(102, Some(101), "node"),
        identity(103, Some(102), "node"),
    ];
    let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));
    assert_eq!(stats.lambs_of(100).len(), 3);
}

/// fails if a sibling subtree leaks in. Two sheep of the same app run side by
/// side, so a walk that took everything with a parent would report each one's
/// children under the other.
#[tokio::test]
async fn a_sibling_subtree_is_not_this_sheeps() {
    let table = vec![
        identity(100, None, "srv"),
        identity(101, Some(100), "mine"),
        identity(200, None, "srv"),
        identity(201, Some(200), "theirs"),
    ];
    let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));
    assert_eq!(stats.lambs_of(100), vec![Lamb::new(101, "mine")]);
}

/// fails if a cycle in the parent links spins forever. The kernel does not
/// produce one, but a fixture can and a truncated `/proc` read might —
/// `TreeIndex::total_over` already terminates on this and the lamb walk must
/// too.
///
/// Bounded (IR-46): this case's only failure mode without a bound is a hang.
#[tokio::test]
async fn a_parent_link_cycle_terminates() {
    let table = vec![identity(100, Some(101), "a"), identity(101, Some(100), "b")];
    let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));
    let lambs = tokio::time::timeout(Duration::from_secs(5), async { stats.lambs_of(100) })
        .await
        .expect("the lamb walk did not terminate within 5s");
    assert_eq!(lambs, vec![Lamb::new(101, "b")]);
}

/// fails if the rows come back in whatever order the map yielded. `describe`'s
/// output is read by people and diffed by scripts; an unstable order makes both
/// worse for no gain.
#[tokio::test]
async fn lambs_come_back_in_pid_order() {
    let table = vec![
        identity(100, None, "srv"),
        identity(103, Some(100), "c"),
        identity(101, Some(100), "a"),
        identity(102, Some(100), "b"),
    ];
    let stats = StatsState::new(Arc::new(ScriptedSampler::identifying(vec![table])));
    assert_eq!(
        stats.lambs_of(100).iter().map(|l| l.pid).collect::<Vec<_>>(),
        vec![101, 102, 103]
    );
}

/// fails if a sampler that cannot report names produces bogus rows instead of
/// none. The trait's default `identify` returns nothing, and every consumer has
/// to read that as "unknown", never as "this sheep has no lambs".
#[tokio::test]
async fn a_sampler_that_cannot_identify_reports_no_lambs() {
    // ScriptedSampler::new(..) implements only `sample`, taking the default
    // `identify`.
    let stats = StatsState::new(Arc::new(ScriptedSampler::new(vec![vec![]])));
    assert!(stats.lambs_of(100).is_empty());
}
```

`identity(..)` and `ScriptedSampler::identifying(..)` are new test helpers
beside the existing `rss`/`rss_cpu` and `ScriptedSampler::new`.

Run `cargo test -p shep-daemon --lib --all-features`. **Not the fast loop** —
this touches the sampler, and CLAUDE.md's own note says the unfiltered lib suite
is the one to run when it does.

**Expected failure — for the stated reason:** compile error, ``cannot find
function `identity` `` / ``no method named `lambs_of` ``.

### Step 16.2 — GREEN

`crates/shep-daemon/src/limits/sample.rs`:

```rust
/// One process's identity: who it is and whose child it is.
///
/// Separate from [`ProcessRss`] rather than a widening of it, and that is a
/// cost decision, not a taste one. `ProcessRss` is `Copy` and is the row type of
/// a whole-machine table the polling enforcer walks every
/// [`MEMORY_POLL_INTERVAL`](super::MEMORY_POLL_INTERVAL); putting a `String` on
/// it would take `Copy` away and allocate once per process on the machine, per
/// tick, for a field only `shep describe` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// The process's own pid.
    pub pid: u32,
    /// Its parent's pid, absent for the roots of the process table.
    pub parent: Option<u32>,
    /// The executable's name as the OS reports it — never its command line.
    /// See `shep_core::protocol::Lamb` for why the distinction is load-bearing.
    pub name: String,
}
```

and on the trait, after `sample`:

```rust
    /// Every process currently visible, with the name the OS reports for it.
    ///
    /// Called on demand by `shep describe` and by nothing else — a lamb tree is
    /// an operator asking a question, not a poll. It performs its own table
    /// walk rather than sharing [`Self::sample`]'s: the two have different
    /// lifetimes (one is a 15-second tick, this is a request) and a shared
    /// table would have to be either stale for this or retained for that.
    ///
    /// # Default implementation
    ///
    /// Returns nothing. Defaulted so that adding this to a `pub` trait did not
    /// break an out-of-tree implementor (the courtesy `#[non_exhaustive]` buys
    /// an enum — IR-20), and honest rather than convenient: a sampler that
    /// cannot report identities says so, and every consumer must read an empty
    /// answer as "unknown", never as "this sheep has no lambs".
    fn identify(&self) -> Vec<ProcessIdentity> {
        Vec::new()
    }
```

`SysinfoSampler::identify` refreshes with `ProcessRefreshKind::nothing()` — no
`.with_memory()`, no `.with_cpu()`, since neither is read here — and maps
`process.name().to_string_lossy().into_owned()`. Comment that the refresh kind
is deliberately narrower than `sample`'s and that widening it would make an
operator command as expensive as the poll.

`crates/shep-daemon/src/limits/stats.rs`:

```rust
    /// Every process the OS reports as a descendant of `root_pid`, in pid
    /// order, excluding `root_pid` itself.
    ///
    /// A parent-pid walk, with the same cycle-safe shape
    /// [`TreeIndex::total_over`] uses and for the same reason: the kernel does
    /// not produce a cycle in the parent links, but a fixture can and a torn
    /// `/proc` read might, and a walk that spun on one would hang a request
    /// rather than answer it.
    ///
    /// **This is not the set of processes a stop kills.** The kill acts on the
    /// process group, which diverges from the ppid tree in both directions —
    /// this module's own doc has the account. Anything rendering this list owes
    /// the operator that caveat where they can see it.
    pub(crate) fn lambs_of(&self, root_pid: u32) -> Vec<Lamb> {
        let table = self.sampler.identify();
        let mut children_of: HashMap<u32, Vec<usize>> = HashMap::new();
        for (index, entry) in table.iter().enumerate() {
            if let Some(parent) = entry.parent {
                children_of.entry(parent).or_default().push(index);
            }
        }

        // `visited` seeded with the root, which does two things at once: it
        // keeps the sheep out of its own lamb list, and it terminates a cycle
        // that leads back to it.
        let mut visited: HashSet<u32> = HashSet::from([root_pid]);
        let mut stack = vec![root_pid];
        let mut lambs = Vec::new();
        while let Some(pid) = stack.pop() {
            for index in children_of.get(&pid).into_iter().flatten() {
                let entry = &table[*index];
                if !visited.insert(entry.pid) {
                    continue;
                }
                lambs.push(Lamb::new(entry.pid, entry.name.clone()));
                stack.push(entry.pid);
            }
        }
        lambs.sort_unstable_by_key(Lamb::pid_key);
        lambs
    }
```

(`Lamb` is `#[non_exhaustive]` but its fields are `pub`, so
`lambs.sort_unstable_by_key(|lamb| lamb.pid)` works directly — use that rather
than inventing a `pid_key` helper; the sketch above names one only to keep the
line short.)

Run `cargo test -p shep-daemon --lib --all-features`. Expect green, **+6**.

### Step 16.3 — GREEN: only `Describe` walks

`crates/shep-daemon/src/rpc.rs`, beside `with_live_stats`:

```rust
/// Fills each row's `lambs` from a fresh walk of the process table.
///
/// Applied to `Describe` and to nothing else. `ListFlock` deliberately does not
/// walk: the walk is a second pass over every process on the machine, and a
/// flock listing is the thing an operator leaves running in a loop — while a
/// `describe` is one sheep, once, on purpose.
///
/// A row with no pid is left `None` rather than set to `Some(vec![])`: a sheep
/// that is not running has no tree to walk, which is the "not walked" case the
/// field's own doc distinguishes from "walked and empty".
async fn with_lambs(stats: &Arc<StatsState>, mut infos: Vec<ProcessInfo>) -> Vec<ProcessInfo> {
    if infos.iter().all(|info| info.pid.is_none()) {
        // Nothing to walk for: skip the table refresh entirely rather than pay
        // for it and assign `None` anyway.
        return infos;
    }
    let stats = Arc::clone(stats);
    let pids: Vec<u32> = infos.iter().filter_map(|info| info.pid).collect();
    let Ok(walked) = tokio::task::spawn_blocking(move || {
        pids.into_iter()
            .map(|pid| (pid, stats.lambs_of(pid)))
            .collect::<HashMap<u32, Vec<Lamb>>>()
    })
    .await
    else {
        // The blocking pool is gone or the task panicked: describe the sheep
        // without their trees rather than fail the request over a decoration —
        // exactly what `with_live_stats` does one function up.
        return infos;
    };
    for info in &mut infos {
        if let Some(lambs) = info.pid.and_then(|pid| walked.get(&pid)) {
            info.lambs = Some(lambs.clone());
        }
    }
    infos
}
```

`stats.lambs_of` is called once per matched row, and each call refreshes the
table. For a `describe` of one sheep that is one refresh; for
`shep describe all` on a large flock it is one per row. **Hoist the refresh**:
have `lambs_of` take a pre-built index, and add
`StatsState::lamb_index(&self) -> LambIndex` that walks once — the same split
`TreeIndex` already makes for exactly this reason ("summing several roots out of
the same table … build one `TreeIndex` per table and call `sum_from` per root").
Do this in the same step, not as a follow-up: `TreeIndex`'s own doc is the
in-tree argument for it and shipping the quadratic version would be shipping
against a lesson already written down.

Wire it into the `Request::Describe` arm alongside `with_live_stats`, and add a
dispatch test:

```rust
/// fails if `ListFlock` starts walking, or if `Describe` stops. The split is a
/// cost decision (`with_lambs`' own doc) and nothing else enforces it — both
/// arms build their rows from the same `snapshot_all`, so a helper applied in
/// the wrong place looks correct at every other level.
#[tokio::test]
async fn only_describe_carries_a_lamb_tree() {
    // A process table where FIRST_SCRIPTED_PID really has a child, so a walk
    // that runs finds something and a walk that does not is distinguishable
    // from one that found nothing.
    let h = harness_identifying(
        vec![ProcScript::runs_forever()],
        vec![
            identity(FIRST_SCRIPTED_PID, None, "srv"),
            identity(FIRST_SCRIPTED_PID + 1, Some(FIRST_SCRIPTED_PID), "node"),
        ],
    );
    reply_of(
        dispatch(
            envelope(
                1,
                Request::Start {
                    apps: vec![AppConfig::minimal("web", "./srv")],
                },
            ),
            &h.ctx,
        )
        .await,
    );

    let listed = reply_of(dispatch(envelope(2, Request::ListFlock), &h.ctx).await);
    let Ok(Response::Flock(rows)) = listed.result else {
        panic!("expected a flock listing");
    };
    assert!(
        rows.iter().all(|row| row.lambs.is_none()),
        "ListFlock must not walk the process table"
    );

    let described = reply_of(
        dispatch(
            envelope(
                3,
                Request::Describe {
                    selector: SelectorSpec::Name("web".to_string()),
                },
            ),
            &h.ctx,
        )
        .await,
    );
    let Ok(Response::Described(rows)) = described.result else {
        panic!("expected a describe listing");
    };
    assert_eq!(
        rows[0].lambs,
        Some(vec![Lamb::new(FIRST_SCRIPTED_PID + 1, "node")])
    );
}
```

`harness_identifying` is a sibling of the existing `harness_sampling`
(`crates/shep-daemon/src/testing.rs:409`), taking an identity table alongside
the scripted memory readings.

Run `cargo test -p shep-daemon --lib --all-features`. Expect green, **+1**.

### Step 16.4 — MUTATION

In `lambs_of`, change

```rust
        let mut visited: HashSet<u32> = HashSet::from([root_pid]);
```

to

```rust
        let mut visited: HashSet<u32> = HashSet::new();
```

Run `cargo test -p shep-daemon --lib --all-features`.

**Must go red:** `a_parent_link_cycle_terminates` fails at its 5s bound — pid
100's parent is 101 and 101's is 100, so the walk revisits the root and never
settles. `the_lamb_walk_excludes_the_sheeps_own_pid` stays **green** here,
because that fixture's root has no parent and never comes back around; the two
tests cover the two things the seeding does and neither one alone would notice
this.

Revert. Second mutation: in `rpc.rs`, apply `with_lambs` to the `ListFlock` arm
as well.

**Must go red:** `only_describe_carries_a_lamb_tree` fails on the `ListFlock`
half.

Revert.

### Step 16.5 — CHANGELOG and gate

`crates/shep-daemon/CHANGELOG.md` → `Additions`: "`MemorySampler::identify`
(defaulted) and `StatsState::lambs_of` — a sheep's parent-pid descendants,
walked on demand and carried by `Describe` only."

Then the full task gate.

---

## Task 17 — `describe` renders the tree

**Files modified:**
- `crates/shep-cli/src/output/rows.rs` — `LambRows`.
- `crates/shep-cli/src/output/mod.rs` — `emit_described`.
- `crates/shep-cli/src/commands/query.rs` — `describe_selector` uses it.
- `crates/shep-cli/src/cli.rs` — `Describe`'s `--help`.
- `crates/shep-cli/tests/cli_e2e.rs` — one case.

**Consumes from Task 15:** `ProcessInfo::lambs`, `Lamb`.

### Step 17.0 — the caveat has to be on the screen

The rendering must not imply a guarantee the walk does not make. Three places
carry it and each has a different reader:

1. `Lamb`'s type doc (Task 15) — the programmer reading the wire.
2. `shep describe --help` — the operator wondering what the section means.
3. **The table caption itself** — the operator looking at the output right now,
   who is reading neither of the above.

The caption is the one that matters and it is one line:

```
Lambs of web (id 3) — parent-pid descendants of 4242, which is not exactly the set a stop kills
```

Not "processes killed with this sheep". Not "process tree". The phrase
"parent-pid descendants" says what was actually walked, and the clause after the
dash says what it is not.

### Step 17.1 — RED

Add to `crates/shep-cli/src/output/mod.rs`'s test module:

```rust
/// fails if the caption stops saying what the list is not. This is the whole
/// honesty requirement for the feature: the walk is a parent-pid tree and the
/// kill is a process group, they diverge in both directions, and the operator
/// reading the table is reading neither the type doc nor `--help`.
#[test]
fn the_lamb_caption_does_not_promise_the_kill_set() {
    let info = ProcessInfo::builder(3, "web", ProcStatus::Online)
        .pid(Some(4242))
        .lambs(Some(vec![Lamb::new(4243, "node")]))
        .build();
    let mut out = Vec::new();
    emit_described(&mut out, Format::Table, "describe", vec![info]).unwrap();
    let rendered = String::from_utf8(out).unwrap();

    assert!(rendered.contains("parent-pid descendants"), "{rendered}");
    assert!(rendered.contains("not exactly the set a stop kills"), "{rendered}");
    // And the row itself, so the caption is not the only thing being asserted.
    assert!(rendered.contains("4243"), "{rendered}");
    assert!(rendered.contains("node"), "{rendered}");
}

/// fails if a sheep with no lambs grows an empty section. `describe` printed one
/// table before this task and must print exactly that for the overwhelmingly
/// common sheep — the same rule `emit_flock` follows for a flock with no dogs.
#[test]
fn a_sheep_with_no_lambs_renders_exactly_what_it_did_before() {
    let bare = ProcessInfo::builder(3, "web", ProcStatus::Online)
        .pid(Some(4242))
        .build();
    let walked_empty = ProcessInfo::builder(3, "web", ProcStatus::Online)
        .pid(Some(4242))
        .lambs(Some(Vec::new()))
        .build();

    for info in [bare, walked_empty] {
        let mut out = Vec::new();
        emit_described(&mut out, Format::Table, "describe", vec![info.clone()]).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(!rendered.contains("Lambs of"), "{rendered}");
    }
}

/// fails if the JSON surface changes shape. `--format json` stays one array of
/// `ProcessInfo`, each row carrying its own `lambs` — a consumer must not have
/// to reassemble a listing out of two payloads, which is the same rule
/// `emit_flock`'s own JSON arm follows for dogs.
#[test]
fn the_json_surface_stays_one_array_with_lambs_on_each_row() {
    let info = ProcessInfo::builder(3, "web", ProcStatus::Online)
        .pid(Some(4242))
        .lambs(Some(vec![Lamb::new(4243, "node")]))
        .build();
    let mut out = Vec::new();
    emit_described(&mut out, Format::Json, "describe", vec![info]).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let rows = value["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["lambs"][0]["pid"], 4243);
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
function `emit_described` ``.

### Step 17.2 — GREEN

`output/rows.rs`:

```rust
/// One sheep's lamb tree, as `describe`'s second table.
///
/// Two columns and no more. A command line is deliberately absent — see
/// [`Lamb`]'s own doc, which is where the reasoning lives rather than repeated
/// here.
#[derive(Debug, Serialize)]
pub struct LambRows(pub Vec<Lamb>);
```

with `headers()` of `["PID", "NAME"]`, `json_key_for` mapping to `pid`/`name`,
and `const JSON_ONLY: &'static [&'static str] = &[];`.

`output/mod.rs`:

```rust
/// Renders one `describe` answer: the sheep table, then each sheep's lamb tree
/// beneath it when the reply walked for one and found any.
///
/// `Format::Json` renders exactly what [`emit`] would for the whole listing —
/// one array, every row carrying its own `lambs`. The machine surface keeps the
/// single listing the tables are a rendering OF, so a consumer never has to
/// reassemble one; the same rule [`emit_flock`] follows for dogs.
///
/// `Format::Table` renders the sheep through
/// [`render_table::<FlockRows>`](render_table), exactly as `describe` always
/// has, then — only for a sheep whose `lambs` is `Some` and non-empty — a blank
/// line, a caption, and that sheep's lambs through
/// [`render_table::<LambRows>`](render_table).
///
/// A sheep with no lambs, and a sheep whose reply did not walk for any, both
/// print exactly what `describe` printed before this function existed: no
/// caption, no second table.
///
/// # The caption
///
/// It names what was walked and what that is not, in one line, because the
/// operator reading this output is reading neither [`Lamb`]'s doc nor
/// `--help`. The walk follows parent-pid links; the stop ladder acts on the
/// process group; the two diverge in both directions. Do not shorten the
/// caption to "process tree" — that is the claim this wording exists to avoid.
///
/// # Errors
/// The underlying write failed.
pub fn emit_described(
    out: &mut dyn io::Write,
    fmt: Format,
    command: &str,
    listing: Vec<ProcessInfo>,
) -> io::Result<()>
```

Caption line, built per sheep:

```rust
                writeln!(
                    out,
                    "Lambs of {} (id {}) — parent-pid descendants of {}, which is not exactly \
                     the set a stop kills",
                    info.name,
                    info.id,
                    info.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                )?;
```

> Note for the implementer and for whoever writes the next check against this
> string: it is **split across two source lines** by a `\` continuation. A
> `grep` for the whole sentence will not match the source. That is exactly the
> shape of one of Phase 10's four checks that could not fail, so the tests in
> Step 17.1 assert on the *rendered* output, never on the file.

`commands/query.rs` — `describe_selector` renders through `emit_described`
rather than `request_and_render`'s single-`Render` path, with a doc comment
saying why, mirroring the one `flock` already carries for `emit_flock`. `fold`
delegates to `describe_selector`, so it gets the tree too — which is right: a
fold listing is a describe.

`cli.rs` — extend `Describe`'s `--help`:

```rust
    /// Describe one sheep in detail.
    ///
    /// Includes the sheep's lambs: the processes the OS reports as descendants
    /// of its pid. That is not the same set the stop ladder kills, which acts
    /// on the process group — a double-forked descendant leaves this list and
    /// is still killed, and a setsid() one stays in it and survives.
    ///
    /// Lamb names are executable names, never command lines.
    Describe(SelectorArgs),
```

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+3**.

### Step 17.3 — GREEN: end to end

Add to `crates/shep-cli/tests/cli_e2e.rs`: start a sheep that forks a child
(`sh -c 'sleep 300 & wait'`), then `shep describe <name>` and assert the output
contains `Lambs of`, `sleep`, and the caption's `not exactly the set a stop
kills`. Poll for the lamb to appear rather than sleeping a fixed interval, with
a bound (IR-46) — the fork is a real process and its appearance in the table is
a race.

Run `cargo test -p shep-cli --test cli_e2e --all-features`. Expect green, **+1**.

### Step 17.4 — MUTATION

In `emit_described`, change the caption to `"Lambs of {} (id {})"` — dropping
the clause.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `the_lamb_caption_does_not_promise_the_kill_set` fails on
`not exactly the set a stop kills`.
`a_sheep_with_no_lambs_renders_exactly_what_it_did_before` must stay **green** —
it asserts on `Lambs of`, which this mutation preserves, and that separation is
what makes the first test about the caveat rather than about the section.

Revert. Second mutation: change the `Some` guard so an empty lamb list still
renders a caption and an empty table.

**Must go red:** `a_sheep_with_no_lambs_renders_exactly_what_it_did_before`
fails on its `walked_empty` iteration and passes on its `bare` one, which is why
that test loops over both.

Revert.

### Step 17.5 — CHANGELOG and gate

`crates/shep-cli/CHANGELOG.md` → `Additions`: "`shep describe` renders each
sheep's lambs beneath its row, captioned with what the parent-pid walk is and
what it is not. A sheep with no lambs prints exactly what it printed before."

Then the full task gate.

---

## Task 18 — the ledger, the docs, and the phase gate

**Files modified:**
- `docs/specs/deferred.md` — six entries out, one in.
- `docs/specs/shep-v1.md` — §5 and §9 notes.
- `docs/shepherd-channel.md` — the `channel.*` topic.
- `README.md` — the verb list and the test count.
- `CLAUDE.md` — the status paragraph.

### Step 18.1 — the ledger

Remove from `docs/specs/deferred.md`'s "Named as v1.0 in spec §2/§9, not yet
built" section:

- **`scale`, `signal`, `sendline`** — the whole entry.
- **`set`/`get`/`unset`** — the whole entry.
- **`channel.*` bus topic** — the whole entry.
- **Lambs in `describe`'s tree view** — the whole entry.

And from the build-queue paragraph at the top (item 2), strike
`scale/signal/sendline`, `the KV store`, `the `channel.*` topic` and
`lambs in describe` from the list of what remains, leaving lookout, whistle,
serve, dev/runtime, `.js` Flockfile, schemars, the daemon-config flags layer,
and openrc + BSD rc.d.

Add to the "Not deferred" section, in that section's own voice — what landed
and what is still open beyond it:

```markdown
**The six daemon-surface verbs** (spec §4, §5, §6, §9) **shipped** on
`feat/phase11-verbs`: `shep scale <name> <count>` (absolute counts only —
scale-up fills the lowest free instance slots, scale-down releases the
highest, and the new count is written back to the muster roll so a reboot
keeps it); `shep signal <selector> <signal>`, delivered to each sheep's own
process and not its group, over `signals::OperatorSignal`'s nine names;
`shep sendline <selector> <line>`, for apps whose Flockfile opts in with
`stdin = true`; the KV store (`shep set`/`get`/`unset` over
`shep_core::kv`, a `0600` `$SHEP_HOME/kv.json` under the same sibling-lockfile
and atomic-rename shape `barks.jsonl` and `shep.toml` already use, reachable
by a dog without going over the socket — operator contract: `docs/kv.md`);
`ProcessInfo::lambs` and `describe`'s tree view, populated by `Describe`
alone and captioned with what the parent-pid walk is not; and the
`channel.*` bus topic, carrying every message a sheep writes on fd 3,
including an `action-reply` no trigger is waiting for.

What each of those does NOT do, recorded so it is not rediscovered as drift:

- `scale` has no relative `+N`/`-N` form and will not grow one — an absolute
  count is idempotent and pm2's relative-remove path is one of the crashes
  the trace notes exist to keep us from reproducing.
- `signal` refuses `SIGSTOP`: a stopped sheep still reads `online` in every
  listing the shepherd can produce, so accepting it would put the flock in a
  state shep cannot report.
- `sendline`'s `Sent` means the bytes were written and flushed to the pipe,
  not that the app read them. A pipe holds 64 KiB before it blocks, and there
  is nothing on that path that could tell the difference.
- The KV store is flat. A dot in a key is part of the name, not a path.
- `lambs` is a parent-pid walk and is not the kill unit, in both directions
  (`shep-daemon`'s `limits` module doc has the account). Only `Describe`
  populates it; `ListFlock` deliberately does not walk.
- `channel.*` carries child→shepherd traffic only. The shepherd's own
  `shutdown` and `action` writes are already reported by `process.stop` and by
  `Response::Triggered`; adding them stays additive if that changes.
```

Add one new entry under "Known debt, recorded rather than built":

```markdown
### `shep signal` cannot reach a sheep's lambs, on purpose

`signal` delivers to the sheep's own pid. An operator who wants a whole
process tree to get a `SIGHUP` — the nginx-worker shape — has no verb for it:
`stop` signals the group but also runs a kill ladder behind it, and there is
no group-wide nudge.

Deferred rather than built because the two are genuinely different asks and
one flag on `signal` (`--group`) would make the safe reading the non-default
one. What would force it: an app class where the sheep is a supervisor that
does not forward signals to its own workers, which is a real shape and simply
has not come up here yet.
```

### Step 18.2 — the spec

`docs/specs/shep-v1.md` needs two amendments, written in the voice §8 and §9
already use for a decision taken against the spec as written — the reasoning,
not just the corrected sentence.

**§5, the KV store paragraph.** It currently says "file-locked JSON; not the
primary config path". Extend it to say where the file is, that keys are flat,
and that a dog reads it through shep-core rather than over the socket —
naming why that differs from `[dog.<name>]` (credentials in an environment
versus a `0600` file read by the same uid).

**§9, the verb list.** `scale`, `signal`, `sendline` and `set`/`get`/`unset` are
already listed. Add a short note after the `trigger` amendment recording that
`scale` is absolute-only and `signal` is per-process, with one sentence of
reasoning each, so the next reader finds the decision where they find the verb.

**§6, the topic list.** It already names `channel.*`. Add the three concrete
topics and the one-line note that the outbound half is deliberately absent.

`docs/shepherd-channel.md` — add a short section telling an app author that
everything they write on fd 3 is published on the bus under `channel.*`, and
that this is a reason to keep a reply body free of anything they would not want
a dashboard to show. That is not a security warning (nothing on this wire is a
credential, which is why the topic carries the real message) but it IS new
information for someone choosing what to put in a reply.

### Step 18.3 — README and CLAUDE.md

`README.md`:

- add the six verbs to the verb list;
- link `docs/kv.md` beside `docs/dogs.md`;
- update the test count. Its current figure decays every phase — Phase 10
  already fixed it once. Take the new number from the actual gate run in Step
  18.4, not from this plan's arithmetic.

`CLAUDE.md`'s "Status / workflow" paragraph currently ends at Phase 10. Replace
the "Phase 7 … in flight" style sentence with a Phase 11 line naming the six
verbs, and update the "What's built vs. deferred" pointer if the deferred list's
shape changed enough to matter.

Baselines, run before editing:

```bash
grep -c "shep scale" README.md            # 0 at HEAD
grep -c "docs/kv.md" README.md            # 0 at HEAD
grep -c "Phase 11" CLAUDE.md              # 0 at HEAD
```

Each must be non-zero afterwards.

### Step 18.4 — the phase gate

Not the task gate. The phase gate, per CLAUDE.md — the four commands, plus:

```bash
cargo test --workspace --all-features -- --test-threads=1
```

This one is not ceremony: it was red on `main` before Phase 5 and it caught a
real regression in Phase 6. This phase adds two new fan-out-off-the-actor paths
(`begin_signal`, `begin_send_line`), a second mailbox per sheep task, and a
writer task per opted-in sheep — every one of which is the kind of thing a
serial run finds and a parallel one hides.

Plus both `benches/` gates.

Record the final counts here, in this file, replacing this sentence: the
baseline was **1044 passed / 0 failed / 3 ignored across 16 result lines**, and
the phase's own delta is roughly **+97** across the workspace — summing this
plan's own per-task figures: shep-core ~+34, shep-daemon lib ~+27 (net of the
six fd-3 fixtures Task 1 moves OUT of it and into shep-core), shep-cli ~+26,
and the integration tiers ~+10 between `cli_e2e`, `daemon_e2e` and
`real_runner`.

Treat that as a shape, not a checksum. Two things matter more than the number:
`failed` is `0` on all sixteen lines, on both the parallel and the serial run;
and it is still **sixteen** lines, because this phase adds no new test binary —
a seventeenth would mean someone added an integration file this plan did not
ask for.

---

## What this phase does NOT build, and why

Recorded here as well as in `deferred.md`, so a reviewer of this plan does not
have to go and check whether an omission was a decision.

- **A relative `scale +2` / `scale -2`.** Argued above; not a scheduling
  deferral, a refusal.
- **`shep signal --group`.** A group-wide nudge is a different verb's job and
  making it a flag would put the safe reading behind an option. `deferred.md`
  entry written in Task 18.
- **`SIGSTOP`.** The shepherd cannot report a stopped sheep, so it will not
  create one.
- **`sendline` with stdin piped for every sheep.** The opt-in default is the
  decision; a flag to override it per invocation would be the same change
  wearing a hat, since the pipe has to exist at spawn time.
- **A KV store over the socket.** Argued above. If a future dog needs a *write*
  the shepherd has to serialise against its own state — which is not what this
  store is for — that is when an RPC earns its place, and the file format does
  not change when it does.
- **Nested KV keys.** A second config language for the store the spec calls not
  the primary config path.
- **A command line on `Lamb`.** Argued above: argv carries credentials and
  `describe --format json` is pasted into issues.
- **Per-lamb memory or CPU.** The sheep's row carries the tree total;
  `deferred.md`'s own note on `ProcessInfo` asks for this restraint.
- **`ListFlock` walking the lamb tree.** Cost. A flock listing runs in a loop;
  a describe does not.
- **`channel.*` carrying the shepherd's own writes.** Argued above. Additive
  later if it turns out to be wanted.
- **Splitting `ProcessInfo` into identity / logs / stats / dog.** `deferred.md`
  names `lambs` as the field that would force the question, and Phase 10 made
  the field cheap to add specifically so the split would not be forced early.
  It is now added, and the row is still coherent — one sheep, everything known
  about it. Revisit when a second consumer needs a different projection, not
  because a field landed.
