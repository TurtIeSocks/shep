# Phase 12a — the lookout's shell, and the flock table

The `lookout` verb (alias `dash`): a terminal dashboard over the shepherd.
This phase builds the shell — dependency, terminal lifecycle, palette, event
loop, link supervision — and exactly **one** pane, the flock table. Against
merged `main` at `6595df7`.

## Why this is 12a and not 12

Rin, today: *"let's start with flock table first. I need to see the panels
before I can make a full decision."*

So this phase deliberately stops one pane in. It builds the whole shell —
which is where the engineering is — plus the flock table, and then hands back
a set of **rendered frames in a file she can read**, so the full four-pane
layout gets decided with something in front of her instead of from a spec
sentence. The bleats feed, the sheep detail pane and the host-usage strip are
Phase 12b and are out of scope here. The pane abstraction is built able to
accept them; none of the three is designed in.

Phases 1–11 are merged: shep-core, the daemon and its supervision engine, the
log plane, the CLI's verb surface, watch/cron/memory-limit restarts,
SO_REUSEPORT reload, custom actions over the shepherd channel, the pm2 cutover,
the dogs subsystem with working metrics and bark dogs, an audit-debt phase, and
the six remaining daemon-surface verbs. The operator API this dashboard renders
is complete — which was Rin's stated reason (2026-08-13) for putting the verbs
before the two UI surfaces.

---

## Global constraints

- MSRV 1.88, edition 2024, `MIT OR Apache-2.0`
- `#![forbid(unsafe_code)]` in shep-core, shep-client and shep-cli
- `PROTOCOL_VERSION` stays 1; any new wire variant needs a pinned fixture.
  **This phase adds none** — lookout is a reader of `Request::ListFlock` and
  `Request::Subscribe`, both shipped. If a task in this plan finds itself
  reaching for a new `Request` variant, stop: it has left scope.
- IR-20: a `pub` error enum in a library crate carries `#[non_exhaustive]` with
  a rationale in its own terms, or documents why not. The comment is mandatory
  either way. shep-cli is a `[[bin]]`-only crate, so nothing this phase adds is
  *in* a library crate — every new error type below still carries the comment
  saying so, rather than leaving the omission silent.
- IR-46: a test that can only fail by hanging carries an explicit bound
- fast loop `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`;
  shep-cli is `[[bin]]`-only so it needs `--bins`, never `--lib`
- gate: fmt, clippy `-D warnings`, `cargo test --workspace --all-features`,
  `RUSTDOCFLAGS="-D warnings" cargo doc`; one cargo command at a time, `$?`
  captured directly, never through a pipe
- baseline **1163 passed / 0 failed / 3 ignored across 16 result lines**
- terminology: the daemon is "the shepherd" and only that; one managed process
  is "a sheep", the plural is always "the flock"; destructive operations and
  error text stay plain

### Reading the counts

Every task states an expected test-count delta. Treat it as a **shape, not a
checksum** — several earlier briefs shipped a stale figure and cost a review
loop each. What matters is that the delta this task adds is roughly what the
task says, and that `failed` stays `0` across all 16 result lines.

One count in this plan is not a shape: **`ignored` goes from 3 to 4**, exactly
once, in Task 5. That is the gallery writer, and it is the only `#[ignore]` this
phase adds. If `ignored` moves for any other reason, something ran that should
not have.

### The exact commands

One cargo command per invocation, `$?` read directly, never through a pipe:

```bash
cargo test -p shep-core   --lib  --all-features
cargo test -p shep-daemon --lib  --all-features -- --skip ::slow::
cargo test -p shep-client --lib  --all-features
cargo test -p shep-cli    --bins --all-features            # NOT --lib: shep-cli has no lib target
cargo test -p shep-cli    --test cli_e2e --all-features
```

Task gate, each from its own command:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Phase gate adds, per CLAUDE.md:

```bash
cargo test --workspace --all-features -- --test-threads=1
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

That last one is not ceremony this phase. Task 1 puts ratatui and crossterm
under `[target.'cfg(unix)'.dependencies]` precisely so the Windows leg does not
grow a terminal stack it will never use, and the cross-check is the only thing
that proves it.

### Every check in this plan states its baseline

Phase 10 shipped four verification steps that could not fail; Phase 11 shipped
several more. Every one was caught by an adversarial read or by an implementer,
never by the check itself. So: **every non-cargo check below prints its baseline
at HEAD first.** Run the baseline command before you make the change. If it does
not print what this plan says it prints, stop and say so — the check is broken,
not the tree.

Baselines taken at `6595df7`, on this machine:

```bash
grep -c '^\[\[package\]\]' Cargo.lock                       # 217
grep -c '^name = "ratatui' Cargo.lock                       # 0
grep -c '^name = "crossterm' Cargo.lock                     # 0
grep -c 'ratatui' crates/shep-cli/Cargo.toml                # 0
grep -c 'ratatui' crates/shep-cli/src/main.rs               # 2  (the module doc saying it is NOT built)
grep -c 'Lookout' crates/shep-cli/src/cli.rs                # 0
find crates/shep-cli/src -type d -name lookout | wc -l      # 0
find docs -maxdepth 1 -type d -name lookout | wc -l         # 0
find crates -name '*.snap' | wc -l                          # 4
grep -rn '#\[ignore' crates/ | wc -l                        # 14
grep -rn 'allow.control' crates/ | wc -l                    # 0
grep -c 'ratatui' docs/specs/deferred.md                    # 2
grep -c 'lookout' README.md                                 # 2
```

`find … | wc -l`, never a bare glob: under zsh a glob with no match raises
`no matches found` and exits non-zero, which is indistinguishable from a check
that failed for the reason you cared about.

#### Three shapes a dead check takes, all three found in earlier plans

**A `git diff` filtered on `^-` can never print `0`.** Unified diff opens each
file's hunk with `--- a/<path>`, which `grep '^-'` matches. Use
`git diff --numstat <paths>` (column **2** is deletions) or
`git diff -U0 <paths> | grep -c '^-[^-]'`.

**A `tokio::time::timeout` around a SYNCHRONOUS call is decoration.** The body
runs to completion on the first poll, so the timer is never armed. Two places in
this phase are exactly that trap: `view::draw` is synchronous, and so is
`frames::render_text`. Neither may be "bounded" with a `timeout`. If a
synchronous render is suspected of being able to loop forever, the honest bound
is a **live assertion** on the thing that would grow without bound — the line
count, the column count — not a wrapper.

**An `assert!` on a string that is already there.** Every `assert!(x.contains(…))`
in this plan names a substring that is **not** in the pre-change output. Where
that is not obvious, the test's own doc comment says what the string is
distinguishing from.

---

## What this phase builds

```
crates/shep-cli/src/lookout/
  mod.rs        the verb entry point and the UI loop; owns nothing but wiring
  app.rs        App + Msg + Effect: the reducer. Sync, no I/O, no ratatui, no
                crossterm, no clock. THE testable core.
  theme.rs      Palette: the design tokens mapped to terminal colours, plus
                NO_COLOR and the 16-colour downgrade
  view/
    mod.rs      draw(app, frame): three regions by arithmetic, no Layout
    flock.rs    the flock table's lines, and columns_for(width)
    status.rs   the title line, the frozen banner, and the status bar
  source.rs     FlockSource / EventSource / Shepherd, and the real impl over
                shep-client
  link.rs       the link task: subscribe AND poll, drop-repair, the reconnect
                ladder, and the freeze
  input.rs      crossterm KeyEvent -> Option<KeyPress>
  term.rs       raw mode, alternate screen, the panic hook, the Drop guard
  frames.rs     Buffer -> plain text and Buffer -> ANSI, the scene list, and
                the gallery writer
docs/lookout/
  frames.txt    the rendered frames, plain
  frames.ansi   the same frames with colour, for `less -R`
  README.md     what the frames show and what Rin is deciding from them
```

Plus `Commands::Lookout` in `cli.rs`, its wiring in `main.rs`, the CHANGELOG
entry, and the `deferred.md` / `README.md` reconciliation.

## What 12b gets, and why it is not here

- **The bleats feed pane.** It needs a bounded ring, drop markers, follow-mode,
  and id→name resolution for lines that arrive before the first listing. All of
  that is real design, and none of it can be judged before the layout is.
  Consequence for this phase, and it is load-bearing: **12a does not subscribe
  to `log.*`.** A dashboard that subscribed to every line every sheep writes,
  in order to draw a pane it does not have, would be the highest-volume
  subscriber on the bus for no visible reason — and would manufacture exactly
  the `Dropped`/`Lagged` condition the link task exists to survive.
- **The sheep detail pane.** Needs a `Describe`-on-selection effect and a
  cadence for it; `Describe` walks the machine's process table for lambs, so
  its cadence is a real cost decision, not a layout one.
- **The host-usage strip.** Needs a `sysinfo` sampler on a blocking task.
  `sysinfo` is already a shep-cli dependency (the metrics dog), so this adds no
  crates — but what the figures cost to sample is 12b's problem, and it is not
  this phase's.
- **The actions themselves** — see design decision 2.
- **Search / filter.** Spec §9 lists it. It narrows two panes; there is one.
- **A selected row.** Considered for 12a and cut (Rin, 2026-08-14) — recorded
  here so 12b's author knows it was decided rather than forgotten. A cursor
  means `selected`, a reseat rule for the wholesale snapshot replacement that
  lands every two seconds, `selected_id`, and a REVERSED row style; none of it
  has a consumer in this phase, because the detail pane it feeds is 12b and the
  one action key it could target refuses in both gate states. What a flock
  taller than the viewport genuinely needs is a **scroll offset**, and that is
  what 12a ships: `j`/`k` move the viewport by a row, `g`/`G` jump to its ends,
  and `view::flock::scroll_offset` clamps the result against the rows that
  actually exist. 12b adds selection on top of that offset, together with the
  pane that reads it. The retrofit argument that kept the control gate in 12a
  does not transfer: a gate is a routing decision every future key has to pass
  through, while a cursor is state one later pane reads.

---

## The dependency bill

`ratatui` and a backend are new to this tree. Rin is deliberate about
dependency weight: reqwest was rejected for hand-rolled HTTP over `tokio-rustls`
at +93 crates, axum was rejected the same way, and `rmcp` was accepted for
whistle on the argument that an evolving protocol is worth an SDK. A TUI is not
something to hand-roll. The open question is only whether the backend is right.

**Baseline: `Cargo.lock` holds 217 packages at `6595df7`** (`grep -c
'^\[\[package\]\]' Cargo.lock`).

**Measured: 217 → 326 packages, +109.** (`grep -c '^\[\[package\]\]' Cargo.lock`,
before and after Task 1.) **This is outside 18–24 — a finding, per this
section's own instruction, not a rounding error.** Two numbers matter here, not
one, because the raw `Cargo.lock` count and the crate count that actually
compiles for this workspace's unix build are not the same thing, and reporting
only the first would mislead in the opposite direction:

- **The raw lockfile delta is +109**, and roughly half of it (~55 packages) is
  never compiled here at all. `ratatui` 0.30.2 declares two *alternative*
  backends — `ratatui-termina` and `ratatui-termwiz` — as optional,
  non-default dependencies alongside `ratatui-crossterm`. Cargo's resolver
  locks a version for every declared optional dependency regardless of
  whether its feature is on, so `ratatui-termwiz`'s entire chain
  (`termwiz`, `termina`, `terminfo`, `termios`, `vtparse`, `filedescriptor`,
  `mac_address`, the six `wezterm-*` crates, `palette` + its four satellites,
  `rand`/`rand_core`, `uuid`, a second `thiserror`, a second `phf`) rides
  along in `Cargo.lock` without ever appearing in the compiled graph. Verified
  with `cargo tree -p shep-cli --all-features -e normal --target
  aarch64-apple-darwin`, which lists only `ratatui-core`, `ratatui-crossterm`
  and `ratatui-widgets` under `ratatui` — no `termwiz`/`termina` — and
  confirmed absent by rooting a tree directly at the package:
  `cargo tree -p ratatui@0.30.2 --target aarch64-apple-darwin -e normal`
  prints 77 lines with none of that chain in them.
- **The real, compiled delta is +48 crate names** — `cargo tree -p
  ratatui@0.30.2` ∪ `cargo tree -p crossterm@0.29.0`, both rooted at the
  unix target, deduplicated by name (61 names), minus the thirteen names from
  the "already present" table below that show up in that union
  (`bitflags`, `cfg-if`, `futures-core`, `hashbrown`, `itoa`, `libc`, `log`,
  `mio`, `rustix`, `signal-hook-registry`, `strum`, `strum_macros`,
  `unicode-width`). This is still above the 18–24 estimate, and the reason is
  concrete and named, not slop: `ratatui-crossterm` (the backend adapter) declares its own
  `crossterm` dependency edge with no `default-features = false` —
  `[dependencies.crossterm_0_29]` in its `Cargo.toml` is bare `version =
  "0.29", optional = true, package = "crossterm"`. Cargo unifies features
  across every consumer of the same resolved version, so crossterm's
  *default* feature set (`bracketed-paste`, `events`, `windows`,
  `derive-more`) ends up active alongside the four features our own entry
  names, regardless of our `default-features = false`. That pulls in
  `derive_more` + `derive_more-impl`, `convert_case`, `document-features`,
  `darling` + `darling_core` + `darling_macro`, `heck`, `ident_case`,
  `strsim`, `litrs`, `errno`, `smallvec`, `rustversion` — twelve crates this
  plan's dependency bill did not name, none of them ours to remove.
- **The "NOT windows" decision (below) still holds for compiled code.** The
  `windows` feature nominally activates, but crossterm's own manifest scopes
  `crossterm_winapi`/`winapi` under `[target.'cfg(windows)'.dependencies]`, so
  they resolve a locked version and compile on no target this workspace
  builds for on macOS or Linux. Confirmed: neither name appears in `cargo
  tree -p crossterm@0.29.0 --target aarch64-apple-darwin -e normal`.
- **Exactly one `crossterm` resolves** — `grep -c '^name = "crossterm"$'
  Cargo.lock` prints `1` — so the two-backends-fighting-over-the-tty risk this
  section originally flagged did not materialize.

Already present, so free (version resolved in `Cargo.lock` at `6595df7`):

| crate | present as | wanted by |
|---|---|---|
| `unicode-width` | 0.2.2 | ratatui |
| `bitflags` | 2.13.1 | both |
| `mio` | 1.2.2 | crossterm (`event-stream`) |
| `rustix` | 1.1.4 | crossterm (termios) |
| `signal-hook-registry` | present | crossterm (unix) |
| `futures-core` | 0.3.33 | crossterm (`event-stream`) |
| `strum` / `strum_macros` | 0.27.2 | ratatui |
| `libc` | 0.2.189 | both |
| `hashbrown`, `indexmap`, `itoa`, `cfg-if`, `log` | present | assorted |

Expected new, and nothing here builds C or needs cmake — the whole reason
`tokio-rustls`'s +10 was acceptable and reqwest's +93 was not:

- ratatui side: `ratatui`, `ratatui-core`, `ratatui-widgets`,
  `ratatui-crossterm`, `kasuari` (the layout solver), `lru` (the layout cache),
  `unicode-segmentation`, `unicode-truncate`, `itertools` + `either`,
  `compact_str` (+ `castaway`, `static_assertions`), `instability`, `indoc`
- crossterm side: `crossterm`, `parking_lot` (+ `parking_lot_core`, `lock_api`,
  `scopeguard`), `signal-hook`, `signal-hook-mio`

**Versions:** `ratatui` 0.30.x, `crossterm` 0.29.x. The pairing is not a
preference — ratatui's crossterm backend wraps a specific crossterm line, and
naming both at the matching majors is what keeps exactly one copy of crossterm
in the graph. Take the crossterm feature that ratatui itself names
(`crossterm_0_29` on ratatui, `0.29` on our direct dependency) and verify one
copy resolves, with the command in Task 1.

**Features, per IR-2 (`default-features = false` and name what is used):**

- `ratatui`: `std`, `crossterm_0_29`, `layout-cache`, `underline-color`. Not
  `all-widgets` (gates the calendar widget, unused), not `macros` (sugar; KISS),
  not `unstable-*`.
- `crossterm`: `events`, `event-stream`. **Not** `windows` — see below.

**Both go under `[target.'cfg(unix)'.dependencies]`,** the same table `nix`
already uses in this manifest. `lookout` is `#[cfg(unix)]` like `commands` and
`dog`, because it needs a unix socket to talk to a shepherd; the Windows leg of
`main.rs::run` refuses every verb before dispatching. Declaring these two
unconditionally would build a terminal stack, `crossterm_winapi` and a second
`windows-sys` face into a binary that cannot use them, and would slow the
cross-check the phase gate runs.

**Not taken, and why:**

- `tui-input` — the earlier research doc (`docs/research/lookout-tui.md`, §1)
  wanted it for the filter line's cursor arithmetic over grapheme clusters.
  There is no filter line in 12a. It arrives with search/filter in 12b or later,
  on its own argument.
- `color-eyre` / `better-panic` — this phase installs its own panic hook (design
  decision 7) and shep-cli's error surface is `ExitCode` plus the
  `output::emit_error` envelope. Neither crate fits that shape.
- Any PTY harness (`portable-pty`, `expectrl`) — `TestBackend` exercises every
  render path headlessly, and a PTY test would re-verify crossterm rather than
  shep. Rejected in the research doc for the same reason, and nothing since has
  changed it.
- `ratatui`'s `Table`/`TableState` widgets — see design decision 5b. This is a
  *narrowing of ratatui's used surface*, not a dependency decision, but it is
  the reason this phase's upstream API exposure is six items wide.

---

## Design decisions made here, not deferred

### 1. Daemon death: bounded retry, then freeze. Never exit.

**Rin's ruling.** lookout retries the connection a bounded number of times,
then says the shepherd has died and **stops updating**, leaving the last known
values on screen. It never exits on its own; the operator quits.

**The FIRST connection is not on that ladder** (Rin, 2026-08-14). The ruling
above is about a shepherd that dies *underneath* a running dashboard — the
sentence presupposes it was alive. A shepherd that was never there is a
different situation, and lookout treats it the way every other client verb
treats it. The opening `Shepherd::link` happens **before** raw mode; if it
fails, lookout emits the ordinary `daemon_unreachable` error envelope on stderr
and exits `ExitCode::DaemonUnreachable` (5), having never entered the alternate
screen. The alternative — which an earlier draft of this plan specified — was
that `shep lookout` on a machine with no shepherd opens a full-screen
dashboard, cycles "reconnecting" for eight seconds, announces a death that
never happened, sits there, and finally exits `Success`, while `shep flock` one
line earlier exited 5.

So: **the bounded ladder applies only after a link has once been established.**
Both halves are one rule said twice — the dashboard reports what it knows, and
never reports a state it was not in.

This is deliberately not `bleats`' precedent. `bleats` prints a notice and exits
cleanly when the connection ends, and that is right for a follow — a `tail -f`
whose file is gone has nothing left to do. A standing dashboard is the other
case: it lives on a second monitor, and vanishing out from under someone is
worse than admitting it is stale. Both behaviours are correct for their own
verb; the difference is written into `mod.rs`'s module doc so the next reader
does not "fix" one into the other.

**The bound, and the numbers:**

```rust
/// How many times the link task re-dials the shepherd before it gives up and
/// freezes the dashboard.
pub const RECONNECT_ATTEMPTS: u32 = 5;

/// The wait before the first re-dial. Doubles each attempt, capped at
/// [`RECONNECT_MAX_WAIT`].
pub const RECONNECT_FIRST_WAIT: Duration = Duration::from_millis(250);

/// The ceiling on that doubling.
pub const RECONNECT_MAX_WAIT: Duration = Duration::from_secs(4);
```

Five attempts at 250 / 500 / 1000 / 2000 / 4000 ms is **7.75 s of waiting**,
plus up to five handshake attempts at `shep_client::HANDSHAKE_TIMEOUT` (5 s)
each. The waiting is the part that matters: a `connect(2)` to a socket with
nothing listening fails immediately, so the realistic worst case is the 7.75 s,
and the 5 s handshake budget only binds against a shepherd that accepted and
then went silent — which is a shepherd that is *there*, and worth waiting for.

**Why ~8 s and not 30, or 2.** A shepherd being restarted deliberately — `shep
kill` then `shep muster`, or a systemd restart — is back inside that window, so
the operator watching through a restart sees "reconnecting", then recovery, and
never sees a freeze. A shepherd that is genuinely gone is declared gone before
the operator has walked away from the terminal. Thirty seconds would leave a
dead dashboard claiming to be live for half a minute; two would flip to frozen
during an ordinary restart and make the operator distrust the banner.

**What freezing means, exactly** — this is design decision 8, and it is more
than "stop polling".

### 2. Actions are gated off by default. 12a builds the gate and the refusal; 12b builds the actions.

**Rin's ruling** is that lookout is interactive but that acting on a sheep needs
a flag or config to enable, mirroring the `allow_control` precedent spec §9 sets
for whistle.

**Chosen: the gate and the refusal land in 12a; the actions land in 12b.** The
reason is the same one that makes this phase 12a. An action key needs a
confirmation affordance, and a confirmation affordance is a layout decision —
a modal, a status-bar prompt, a second keypress — which is exactly the class of
decision Rin has said she cannot make before she sees the panes. What *must*
exist before any action key is the gate, because a gate retrofitted after the
keys is a gate someone forgets to route one key through.

So 12a ships:

- `Control::{ReadOnly, Allowed}`, resolved once at startup and carried in `App`.
- Exactly **one** bound action key, `x` (stop — the destructive one, chosen on
  purpose so the gate is exercised by its worst case), which in 12a **never
  acts**. It resolves to a refusal in both states:
  - `Control::ReadOnly` → `read-only: actions need --allow-control`
  - `Control::Allowed` → `stop is not built yet`
- The status bar renders which state is in force, always, in both.

Both strings are literal. Nothing about damage gets charming — the voice rule
from the design language, and the house rule that destructive operations and
error text stay plain.

**Where the setting comes from:** `--allow-control` on the command line, or
`lookout.allow_control = "true"` in the KV store (`shep set
lookout.allow_control true`). The flag wins. No new config section, no new wire
field.

**Why the KV store and not `shep.toml`'s daemon config**, where
`whistle.allow_control` will live. whistle's control tools act *through the
shepherd*, on behalf of a client the operator is not watching, so the shepherd
has to be the authority and the flag has to be daemon-side. lookout runs as the
operator's own process, on the operator's own terminal, under the operator's own
uid — the shepherd cannot tell a lookout keypress from a `shep stop`, and would
not be entitled to refuse it if it could. So this gate is a **fat-finger catch,
not a security boundary**, and it belongs where the operator sets it. That
sentence goes in the flag's own `--help` text, because a gate that reads as a
security control and is not one is worse than no gate.

`shep_core::kv` is the right home for it on its own terms: spec §5 calls the
store the place for "ad-hoc + dog runtime tweaks", it works with no shepherd
running, and `shep set` / `shep get` already exist.

### 3. The flock table subscribes AND polls. This is bark's shape, ported.

`tokio::sync::broadcast` **drops** events for a lagging subscriber rather than
queueing them. The daemon surfaces that as `BusEvent::Dropped { count }`;
`shep-client`'s `EventStream` surfaces its own local version as
`Err(Lagged { count })`. A dashboard that only subscribed would go silently
wrong under exactly the load that makes a dashboard worth watching.

`crates/shep-cli/src/dog/bark/mod.rs`'s `run_loop` already solved this and the
shape is ported, not reinvented:

- **Subscribe for latency, poll for correctness.** `Request::ListFlock` every
  `FLOCK_POLL` (2 s). The snapshot **replaces the flock map wholesale** — the
  poll wins every conflict with the bus.
- **A dropped or lagged frame triggers an immediate poll**, rather than waiting
  for the scheduled one. The drop itself carries no information about what was
  lost; the only way to know is to ask the shepherd what things look like now.
  Both `Ok(BusEvent::Dropped { .. })` and `Err(Lagged { .. })` take this path,
  and both are also forwarded to the reducer so the status bar can say which
  side of the connection fell behind — `bleats` already distinguishes the two in
  its own notice text, for the same reason: they live on opposite ends of the
  wire and an operator cannot tell which end to investigate otherwise.
- **`interval_at(now + period, period)`, not `interval`.** A plain
  `tokio::time::interval` fires its first tick immediately, which would make the
  first poll unattributable to either a drop or the interval genuinely elapsing.
  The first listing is a separate, explicit request (see decision 4), so the
  interval's job is only the steady state. Same reasoning bark's own comment
  gives.
- **`MissedTickBehavior::Delay`**, again as bark. A dashboard that fell behind
  must not then fire a burst of catch-up polls at a shepherd that is probably
  already the reason it fell behind.

Topics subscribed: `["process.*", "daemon.*"]`. Not `log.*` — see "What 12b
gets".

**2 s, and where the number comes from.** The pane's own content changes on a
`process.*` event within milliseconds; the poll exists to repair drift, not to
animate. Two seconds is under an operator's own "is this thing live" patience
while being 15× cheaper than the once-a-frame polling a naive dashboard does.
Compare: the bark dog's fallback poll is 30 s, because nothing is watching it;
the memory-limit sampler is 15 s, because it walks the process table. A
`ListFlock` is a map lookup and one frame each way.

### 4. Subscribe first, then list. This is the opposite of `bleats`' order, and both are right.

`bleats` lists *before* it subscribes, and its module doc says why: the listing
builds the id→name cache every rendered line needs, so a line arriving before it
would be unresolvable.

lookout does the reverse. Its rows carry a whole `ProcessInfo` — a
`BusEvent::Process { info, .. }` needs nothing resolved against anything — so an
event arriving before the first snapshot upserts into an empty map perfectly
well. What lookout cannot afford is the *gap*: list-then-subscribe loses every
event between the reply and the subscription, and while the next poll repairs it
within 2 s, there is no reason to accept a hole that costs nothing to close.

Written down here because the two orders look like a copy-paste error in each
other's direction, and the next reader will assume one of them is a bug.

### 5. The palette maps; it does not quote. And the theme never costs clarity.

`docs/shep-design/README.md` is the design language. It is a *concept* reference
written before Phases 10 and 11, so parts of its copy are already out of date;
what is durable is the semantic colour assignment and the voice rules, and only
those are taken.

A terminal has 16 or 256 colours, not hex tokens. The mapping:

| Token | Hex | Means | 256-colour | 16-colour |
|---|---|---|---|---|
| `--meadow` | `#2E8B57` | online, healthy, go | `Indexed(29)` | `Green` |
| `--bark` | `#E0552B` | errored, refused, destructive — **and nothing else** | `Indexed(166)` | `Red` |
| `--butter` | `#F3C44C` | attention: starting, waiting-restart, notes | `Indexed(221)` | `Yellow` |
| `--ink-3` | `#7A8C80` | muted labels, captions, stopped | `Indexed(245)` | `DarkGray` |

**`--paper` is not mapped, and no background is painted.** The design language's
page background is `#FBF6E7`; forcing that into a terminal means fighting the
operator's own theme and losing on half of them. The terminal's background stays
the terminal's. Same for `--ink` as a foreground: ordinary text is
`Color::Reset`, which is whatever the operator already reads comfortably.

**"Errors get a colour, not a face."** Taken from the design language verbatim
and it binds here: an `errored` sheep gets `--bark` on its STATUS cell. It does
not get an emoji, a sad sheep, a `!!!`, or a blinking anything.

**Colour is always redundant with text, structurally.** Every coloured cell in
12a is a cell whose *text already says the same thing*: the STATUS column prints
`errored`, and `--bark` is on top of that word. The link banner prints
`the shepherd has died`, and `--bark` is on top of that sentence. This is what
makes the two downgrades below losses of decoration rather than losses of
information — and it is the house rule ("the theme never costs clarity") made
structural instead of aspirational.

**Downgrades:**

- **`NO_COLOR` set and non-empty** → `Palette::none()`: every style is
  `Style::default()`. An empty `NO_COLOR=` is an unset one. This is the same
  rule and the same shape `commands/daemon.rs`'s `ansi_enabled` already
  implements for the daemon's own log output — a pure function taking the
  environment as arguments, so it is testable without touching `std::env`.
- **A terminal that cannot do 256 colours** → the 16-colour column. Detected
  from `COLORTERM` containing `truecolor` or `24bit`, or `TERM` containing
  `256color`; anything else gets the 16-colour palette. This errs toward the
  narrower palette on an unknown terminal, which is the recoverable direction:
  a 256-colour terminal shown 16 colours looks slightly flatter, while a
  16-colour terminal sent `\x1b[38;5;166m` can print literal garbage.
- **stdout is not a terminal** → lookout does not start at all. Exit
  `ExitCode::Usage` with `lookout needs a terminal; stdout is not one`. A TUI
  piped into a file is a usage error, not a rendering mode, and the alternative
  — emitting alternate-screen escapes into a pipe — is how a redirected
  dashboard corrupts a log. This is also what makes the refusal *testable*:
  `assert_cmd` captures stdout, so the e2e case gets this path for free.

### 5b. The flock table is drawn as lines, not with ratatui's `Table` widget.

The visual contract of this pane is "the table `shep flock` prints, live". The
CLI's own renderer (`crates/shep-cli/src/output/table.rs`) already owns that:
columns sized to the widest cell, two spaces between, **no box-drawing
characters** — "a table a user can `awk` over beats one that looks nice".
Handing the same job to `ratatui::widgets::Table` would put a second,
independent column algorithm next to the first, and the two would drift on the
first multi-byte name.

So `view/flock.rs` builds `Vec<Line>` itself and writes them with
`frame.buffer_mut().set_line(..)`. The upstream API this phase uses is six
items wide: `Frame::area`, `Frame::buffer_mut`, `Buffer::set_line`, `Line`,
`Span`, and `Style`/`Color`. Layout is arithmetic on `Rect`, not
`Layout`/`Constraint`, and no `Modifier` is ever set — 12a has no selected row
and nothing bold, so a foreground colour is the whole of its styling.

That is not an argument that ratatui is unnecessary. What it is being taken for
is the part worth taking: the backend abstraction, the double-buffered diffing
draw that makes a redraw cost only the cells that changed, the terminal
lifecycle, and `TestBackend`. 12b's panes — a scrolling feed, a gauge, a
sparkline — are where the widget library earns the rest.

### 6. Narrow terminals drop columns in a fixed order, and say so by dropping.

`columns_for(width: u16) -> &'static [Column]`, a pure function over a `const`
table. Full set, left to right: `ID NAME STATUS PID RESTARTS CPU MEM UPTIME
FOLD`. Fixed cell widths: ID 4, STATUS 15 (`waiting-restart` is the longest
status), PID 7, RESTARTS 8, CPU 6, MEM 8, UPTIME 8, FOLD 10; NAME takes the
remainder, floor 8; two spaces between columns.

Drop order as width shrinks — least diagnostic first:

| width ≥ | columns |
|---|---|
| 90 | all nine |
| 78 | drop FOLD |
| 68 | also drop RESTARTS |
| 59 | also drop PID |
| 49 | also drop MEM |
| 41 | also drop CPU |
| 31 | also drop UPTIME → `ID NAME STATUS` |
| below | the one-line refusal |

FOLD goes first because it is grouping metadata, not health. RESTARTS and PID
next because they answer follow-up questions, not "is it up". CPU and MEM are
the last two numbers to go because they are the ones that explain *why*
something is wrong. `ID NAME STATUS` is the floor because those three are the
pane.

Below 31 columns, or below 6 rows, the pane refuses instead of drawing: `too
small` on the first line, `need 31x6` on the second, and only the first when
there is a single row to write into. Named `MIN_WIDTH: u16 = 31` and
`MIN_HEIGHT: u16 = 6` (title, the link banner, header, rule, one row, status
bar — six, not five, because the banner has to fit in the state that most needs
it). A pane that
tried to draw anyway would produce overlapping garbage that reads as a crash.

**The refusal has to fit in the terminal it is refusing about.** This is the
trap, and an earlier draft of this plan fell into it: `Buffer::set_line`
truncates at `max_width` silently, and this branch exists precisely for
terminals narrower than 31 columns. `terminal too small — lookout needs 31x6`
is 39 characters, so at 28 columns the operator reads `terminal too small —
lookout` and never sees `31x6` — the one piece of information the message
exists to carry, cut off in exactly the case it was written for. Two
nine-character lines fit anything from nine columns up.

Names longer than the NAME column are truncated with a trailing `…` — never
silently cut, because a truncated name that looks whole is a name an operator
will type into `shep stop`.

### 7. Terminal restore on panic: a hook AND a guard. Both, not either.

A crash that leaves raw mode on and the alternate screen entered leaves the
operator's terminal unusable — no echo, no line editing, no visible cursor, and
often no scrollback. This is the single worst failure a TUI can have, because it
outlives the process.

**The mechanism, named:**

1. **`std::panic::set_hook`, wrapping the previous hook.** Before entering raw
   mode, take `std::panic::take_hook()`, and install a hook that calls
   `term::restore()` and *then* calls the previous hook. Order matters: restore
   first, so the default hook's backtrace is printed to a cooked terminal on the
   main screen where the operator can read and scroll it.
2. **A `TerminalGuard` with a `Drop` impl** that calls the same
   `term::restore()`. The hook does not run on an ordinary early return or a
   `?`; `Drop` does. Between them every exit path is covered except
   `panic = "abort"`, which this workspace does not set.
3. **`restore()` is idempotent** — it ignores `disable_raw_mode` failing on a
   terminal that is not in raw mode, and it is safe to run twice, because on a
   panic both mechanisms fire.
4. **Nothing that can panic is installed between the hook and raw mode.** The
   hook goes on first, then raw mode, then the alternate screen.

ratatui 0.30's own `init()` also chains a restoring panic hook. This phase does
not use it, for one reason worth stating: `init()` picks the terminal, the
backend and the hook as a bundle, and this phase needs the backend to be
swappable for `TestBackend` in order for the loop itself to be testable. Writing
the four lines above keeps that seam and costs nothing.

**SIGTERM still has to be handled.** In raw mode crossterm does not deliver
Ctrl-C as a signal — it arrives as a `KeyEvent`, which is why `q` and `Ctrl-C`
are both ordinary key bindings here. But `SIGTERM` (a `shep kill` of the wrong
pid, a session teardown, a systemd stop) is real, and a lookout that dies on it
without restoring is the same disaster as a panic. The UI loop carries a
`tokio::signal::unix::signal(SignalKind::terminate())` arm that breaks the loop
cleanly, so the guard's `Drop` runs.

### 8. Frozen means the clock stops too.

When the reconnect ladder is exhausted, `App` enters `Link::Lost`. What that
does:

- The banner appears under the title: `the shepherd has died — these values are
  frozen as of <local time>`, in `--bark`. Literal, per the voice rule.
- The link task stops: no more polls, no more re-dials, no more subscriptions.
  It ends.
- **The uptime column stops advancing.** This is the part that is easy to get
  wrong. Every row carries `(uptime_ms, anchor: Instant)` from the moment its
  value was received, and the rendered uptime is `uptime_ms + (app.now -
  anchor)`. `app.now` is advanced by the 1 s heartbeat — *and the reducer
  ignores the heartbeat's `now` while `Link::Lost`*. Without that, a frozen
  dashboard's UPTIME column keeps counting up for a sheep the shepherd can no
  longer see, which is the dashboard telling a specific lie about a specific
  process. Task 3's mutation is exactly this line.
- The row's uptime does not advance for a sheep that is not running, either,
  frozen or not: a `stopped` sheep's `uptime_ms` is a historical fact, and
  advancing it would invent one.
- Keys still work, and they stay honest about being frozen. `q` quits. `j`/`k`
  and `g`/`G` still scroll the last known rows — reading them is the whole
  point of not clearing the screen. `x` still refuses, with the same words.
  `r`, the one key that asks for I/O, does **not** silently do nothing: the
  link task has ended, so its poll receiver is gone and a request would vanish
  into a closed channel. Pressing `r` while frozen leaves a notice saying there
  is nothing left to ask. A refresh key that quietly failed on a frozen
  dashboard would be the same lie as a running clock, one keystroke smaller.
- **lookout never exits on its own.** Not on freeze, not on `Lagged`, not on
  `BusEvent::DaemonShutdown` — that last one is a *notice* here, where in
  `bleats` it precedes an exit.

### 9. Testing a TUI in a `[[bin]]`-only crate

`shep-cli` has one `[[bin]]` and no `lib` target. `cargo test -p shep-cli --lib`
silently runs nothing and reports success; the correct invocation is `--bins`.
That is already the house rule. What it means for a TUI:

- **Unit and snapshot tests live in `#[cfg(test)] mod tests` inside
  `src/lookout/*.rs`**, compiled into the bin target and run by
  `cargo test -p shep-cli --bins --all-features`. Every `commands/*.rs` in this
  crate is already this shape; nothing new is needed.
- **The render path needs no tty.** `Terminal::new(TestBackend::new(w, h))`
  renders into a plain `Buffer`. `view::draw(&app, &mut frame)` is a synchronous
  function of `&App`, so a scripted `App` plus one `draw` is a whole frame.
- **The reducer needs no runtime, no clock and no terminal.** `App::update`
  is sync, takes `Msg`, returns `Effect`, and imports nothing from ratatui or
  crossterm. `Instant` is *passed in* on `Msg::Tick { now }` and
  `Msg::Snapshot { at, .. }` rather than read inside, so every uptime assertion
  is exact arithmetic rather than a sleep.
- **The link task needs no socket.** `run_link` is generic over the `Shepherd`
  trait, and the test drives it with a hand-rolled fake — the IR-33 rule, and
  the same two-tier `const`/`script` fixture convention the daemon and bark
  already use. A fake whose event source is a real `tokio::sync::broadcast`
  receiver with capacity 2 is what makes the bus genuinely drop frames, exactly
  as bark's own reconciliation test does.
- **The UI loop needs neither.** `run_ui` takes `Terminal<B: Backend>` and the
  input as `impl Stream<Item = io::Result<Event>>`, so a test drives the whole
  loop with `TestBackend` and `futures_util::stream::iter`.
- **`tests/cli_e2e.rs` covers only the non-interactive edges**, through
  `assert_cmd` against the real binary: `--help`, the `dash` alias, and the
  not-a-tty refusal. No PTY harness.

### 10. The frames are a deliverable, not test scaffolding.

`TestBackend` renders a frame into a plain text buffer. The same mechanism that
makes a TUI testable headlessly is the mechanism that lets Rin *see* it without
a terminal — so Task 5 makes that an explicit output: `docs/lookout/frames.txt`
and `docs/lookout/frames.ansi`, eight scenes, written by a command she can run.

Two renderers over the same `Buffer`, both ours (`frames.rs`), rather than
`TestBackend`'s own `Display`: `Display`'s exact framing is an upstream
presentation detail that can change between ratatui releases, and one of the two
outputs needs to carry colour, which `Display` does not.

The gallery is built from the **same scene list** the snapshot tests use, so it
cannot rot: a layout change reddens the insta snapshots in the ordinary suite,
and regenerating the gallery is the same three scenes' worth of work.

These frames are for Rin to look at **before Phase 12b's layout is decided.**
That is their whole purpose. They are not a regression artifact and not
documentation of a shipped design.

---

## Task order and dependencies

```
Task 1  deps                       ── nothing before it
Task 2  theme.rs                   ── needs 1
Task 3  app.rs (the reducer)       ── needs nothing but std + shep-core
Task 4  view/ (flock + status)     ── needs 1, 2, 3
Task 5  frames.rs + the gallery    ── needs 4
Task 6  source.rs + link.rs        ── needs 3 (Msg); independent of 2/4/5
Task 7  term.rs + input.rs         ── needs 1
Task 8  mod.rs, the verb, the loop ── needs 3, 4, 6, 7
Task 9  docs, ledger, phase gate   ── needs everything
```

Tasks 3 and 6 are the two that carry real risk (the reducer's reconciliation
invariants; the link task's drop-repair and reconnect ladder). Tasks 2, 4, 5 and
7 are bounded and mechanical once their inputs exist. Tasks 3 and 6 can be built
in parallel with 2/4/5 if two implementers are available — 6 depends on 3 only
for the `Msg` enum, which Task 3 lands in its first step.

---

## Task 1 — the dependencies, measured

**Files modified:**
- `Cargo.toml` — workspace entries for `ratatui` and `crossterm`.
- `crates/shep-cli/Cargo.toml` — the `[target.'cfg(unix)'.dependencies]` edge.
- `crates/shep-cli/src/main.rs` — the module doc's "not built" paragraph.
- This plan file — the measured crate delta replaces the estimate.

### Step 1.1 — baseline

Run these four, and confirm each prints what is written beside it. If any does
not, stop:

```bash
grep -c '^\[\[package\]\]' Cargo.lock          # 217
grep -c '^name = "ratatui' Cargo.lock          # 0
grep -c '^name = "crossterm' Cargo.lock        # 0
grep -c 'ratatui' crates/shep-cli/Cargo.toml   # 0
```

### Step 1.2 — GREEN: the manifests

`Cargo.toml`, in `[workspace.dependencies]`, after the `sysinfo` entry:

```toml
# The lookout TUI (spec §9). A terminal UI is the one thing in this tree that
# is genuinely not worth hand-rolling — the cell diffing, the resize handling
# and the cursor arithmetic are the whole cost, and all three are exactly what
# an established TUI crate has already got right. Same argument that took
# `rmcp` for whistle, and the opposite of the one that rejected reqwest: this
# is a rendering library with no protocol, no TLS and no C toolchain behind it.
#
# Features per IR-2. `crossterm_0_29` pins the backend's crossterm line
# explicitly so it can never skew from our own direct dependency below;
# `layout-cache` is the memoized layout solver; `underline-color` is what makes
# a coloured underline expressible without a second style pass. Not
# `all-widgets` (gates the calendar widget alone, unused) and not `macros`
# (ratatui-macros is sugar; KISS).
ratatui = { version = "0.30", default-features = false, features = ["std", "crossterm_0_29", "layout-cache", "underline-color"] }
# The backend, named directly rather than reached through ratatui's re-export:
# `crossterm::event::EventStream` (the async input source the UI loop selects
# on) lives behind the `event-stream` feature, and a feature can only be
# enabled by a crate that names the dependency. `events` is the key/resize
# types. NOT `windows`: both of these sit under
# `[target.'cfg(unix)'.dependencies]` in shep-cli, because `lookout` is
# `#[cfg(unix)]` like `commands` and `dog`, and the Windows leg of
# `main.rs::run` refuses every verb before dispatching to any of them.
crossterm = { version = "0.29", default-features = false, features = ["events", "event-stream"] }
```

`crates/shep-cli/Cargo.toml`, in the existing `[target.'cfg(unix)'.dependencies]`
table next to `nix`:

```toml
# The lookout TUI. Unix-only for the same reason `nix` above is: `src/lookout/`
# is `#[cfg(unix)]`, because it needs a unix socket to reach a shepherd, and
# the Windows build refuses every verb before it could reach this module.
# Declaring these two unconditionally would build a terminal stack,
# `crossterm_winapi` and a second `windows-sys` face into a binary that cannot
# use any of it — and would slow the `--target x86_64-pc-windows-gnu` check the
# phase gate runs.
ratatui.workspace = true
crossterm.workspace = true
```

Run `cargo check -p shep-cli --all-features`. Expect `EXIT=0` and a lock update.

### Step 1.3 — MEASURE, and write the number down

```bash
grep -c '^\[\[package\]\]' Cargo.lock          # was 217; this is the real delta
grep -c '^name = "crossterm"$' Cargo.lock      # must be exactly 1
grep -c '^name = "ratatui' Cargo.lock          # ratatui + ratatui-core + ratatui-widgets + ratatui-crossterm
```

The second one is the check that matters: **exactly one `crossterm`**. Two
copies means ratatui's `crossterm_0_29` feature and our direct `0.29` did not
unify, and every `KeyEvent` this crate constructs would be a different type from
the one ratatui's backend reads. That is a compile error in the good case and a
silent no-op in the bad one.

Then replace the estimate in this plan's **"The dependency bill"** section with
the measured number, in this form:

> **Measured: 217 → N packages, +D.** (`grep -c '^\[\[package\]\]' Cargo.lock`,
> before and after Task 1.)

If `D` is outside 18–24, say so in the task report before continuing. It does
not block — it is a finding about the estimate, and Rin asked for the number.

Also record what the new crates actually are, so the next audit does not have to
re-derive it:

```bash
git diff -U0 Cargo.lock | grep '^+name = ' | sed 's/^+name = //' | sort
```

### Step 1.4 — confirm the upstream API surface this phase uses

**This step is a confirmation, not a before/after check, and it deliberately
states no baseline** — an earlier draft claimed these greps "print `0` today
only because the crates were not fetched", which is false: all five crates are
already unpacked under `~/.cargo/registry/src/`, so every one of them passes at
HEAD and none of them could ever have gone from red to green. A check that
cannot fail is worse than no check, and this plan's own rule
(§"Every check in this plan states its baseline") is what caught it.

What this step IS for: the rest of the plan writes these six call sites out in
full, and if the resolved version spells any of them differently, the
implementer should find that out here rather than in Task 4.

```bash
RC=$(ls -d ~/.cargo/registry/src/*/ | head -1)
ls -d "$RC"ratatui-core-* "$RC"ratatui-0.30* "$RC"crossterm-0.29*
```

Each of these must print at least `1`. Note the exact paths and the `const`
spellings — `ratatui-0.30.2` has **no** `src/backend/` directory at all (its
`backend` is a re-export module in `lib.rs`), and `Frame::area` /
`Frame::buffer_mut` are `pub const fn`, so a pattern of `pub fn area` matches
nothing:

```bash
grep -c 'pub fn set_line'         "$RC"ratatui-core-*/src/buffer/buffer.rs       # >= 1
grep -c 'pub const fn area'       "$RC"ratatui-core-*/src/terminal/frame.rs      # >= 1
grep -c 'pub const fn buffer_mut' "$RC"ratatui-core-*/src/terminal/frame.rs      # >= 1
grep -c 'pub struct TestBackend'  "$RC"ratatui-core-*/src/backend/test.rs        # >= 1
grep -c 'pub const fn buffer'     "$RC"ratatui-core-*/src/backend/test.rs        # >= 1
grep -c 'pub struct EventStream'  "$RC"crossterm-0.29.0/src/event/stream.rs      # >= 1
```

Verified at plan time, against exactly these files, so a mismatch is a real
signal rather than a typo in this plan:
`Buffer::set_line(&mut self, x: u16, y: u16, line: &Line<'_>, max_width: u16)
-> (u16, u16)` (buffer.rs:373 — note it **truncates at `max_width`**, which is
design decision 6's whole point), `Frame::area(&self) -> Rect` (frame.rs:68),
`Frame::buffer_mut(&mut self) -> &mut Buffer` (frame.rs:207),
`Buffer::area(&self) -> &Rect` (buffer.rs:113),
`Buffer::cell(impl Into<Position>) -> Option<&Cell>` (buffer.rs:179),
`Cell::{fg, modifier}` public fields plus `Cell::symbol(&self) -> &str`
(cell.rs:49/59/105), `TestBackend` (backend/test.rs:32) with
`buffer(&self) -> &Buffer` (test.rs:103) and
`type Error = core::convert::Infallible` (test.rs:250),
`Terminal::backend(&self) -> &B` (terminal/backend.rs:16),
`Line::style(self, impl Into<Style>)` (text/line.rs:343),
`ratatui::backend::TestBackend` re-exported unconditionally with no feature
gate (ratatui-0.30.2/src/lib.rs:505), and
`impl Stream for EventStream` (crossterm stream.rs:101).

If any path or spelling has moved, **fix the call sites, not the plan's
intent**: what each item is for is stated in design decision 5b, and the intent
survives a module rename. Report which ones moved.

### Step 1.5 — GREEN: the module doc stops saying ratatui is absent

`crates/shep-cli/src/main.rs`, the module doc currently reads — **all four
sentences, main.rs:5-11**, which is the whole paragraph and not the first two
thirds of it. A find/replace against a shorter quotation leaves the
"Recorded here as deliberately absent" sentence behind and the file ends up
saying it twice:

```rust
//! A ratatui dashboard (`lookout`), a static file server (`serve`), and the
//! container (`shep runtime`) and dev (`shep dev`) execution modes are
//! spec'd (`docs/specs/shep-v1.md` §9) but not built — this crate depends on
//! neither `ratatui`, `axum` nor `tower-http`, and there is no `[[bin]]`
//! beyond `shep` itself. Recorded here as deliberately absent rather than
//! letting them quietly read as shipped; full inventory:
//! `docs/specs/deferred.md`.
```

Replace the whole of it with:

```rust
//! A static file server (`serve`) and the container (`shep runtime`) and dev
//! (`shep dev`) execution modes are spec'd (`docs/specs/shep-v1.md` §9) but
//! not built — this crate depends on neither `axum` nor `tower-http`, and
//! there is no `[[bin]]` beyond `shep` itself. The ratatui `lookout` dashboard
//! has its shell and its flock table (Phase 12a); its other three panes — the
//! bleats feed, the sheep detail and the host-usage strip — are 12b. Recorded
//! here as deliberately absent or deliberately partial rather than letting
//! either read as shipped; full inventory: `docs/specs/deferred.md`.
```

`lookout` is written as plain code, **not** as an intra-doc link. `mod lookout`
is private (and `#[cfg(unix)]`), and a link from a crate-level doc to a private
item is a `private_intra_doc_links` warning, which
`RUSTDOCFLAGS="-D warnings" cargo doc` in the task gate turns into a failure.

### Step 1.6 — verify

```bash
grep -c 'ratatui' crates/shep-cli/src/main.rs   # was 2, now 1 (the new sentence names it once)
```

The `1` is load-bearing in both directions: `2` means the old paragraph is
still there, `0` means the replacement dropped the word and the doc no longer
tells a reader that this crate carries a TUI stack at all.

Then:

```bash
cargo check -p shep-cli --all-features
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The second is the one that proves the `cfg(unix)` placement. It must be
`EXIT=0`, and it must not have compiled ratatui — confirm with:

```bash
cargo tree -p shep-cli --target x86_64-pc-windows-gnu -e normal | grep -c ratatui   # 0
cargo tree -p shep-cli --target aarch64-apple-darwin  -e normal | grep -c ratatui   # >= 1
```

Both lines must print what is written. The second exists so the first cannot
pass by `cargo tree` simply not working.

### Step 1.7 — MUTATION

In `crates/shep-cli/Cargo.toml`, move the two lines from
`[target.'cfg(unix)'.dependencies]` up into the plain `[dependencies]` table.

Run `cargo tree -p shep-cli --target x86_64-pc-windows-gnu -e normal | grep -c ratatui`.

**Must go red:** it prints a non-zero count — ratatui is now in the Windows
graph. Revert.

### Step 1.8 — gate

The full task gate. No test-count change is expected here: **+0**.

---

## Task 2 — `lookout/theme.rs`: the palette, and both downgrades

**Files created:**
- `crates/shep-cli/src/lookout/mod.rs` — the module declaration only, at this
  point.
- `crates/shep-cli/src/lookout/theme.rs`.

**Files modified:**
- `crates/shep-cli/src/main.rs` — `#[cfg(unix)] mod lookout;`.

### Step 2.1 — baseline

```bash
find crates/shep-cli/src -type d -name lookout | wc -l   # 0
```

### Step 2.2 — RED

Create `crates/shep-cli/src/lookout/theme.rs` with only its test module, and add
the module tree so it compiles:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// fails if `NO_COLOR` stops being honoured, or if an EMPTY `NO_COLOR=`
    /// starts being treated as set. The empty case is the one that regresses
    /// silently: a user who exports `NO_COLOR=` with no value would lose every
    /// colour in the dashboard and have nothing to blame it on. Same rule
    /// `commands::daemon::ansi_enabled` already pins for the shepherd's own
    /// log output.
    #[test]
    fn no_color_flattens_the_palette_and_an_empty_one_does_not() {
        let off = Palette::detect(Some(OsStr::new("1")), None, None);
        assert_eq!(off.status(ProcStatus::Errored), Style::default());
        assert_eq!(off.muted(), Style::default());

        let empty = Palette::detect(Some(OsStr::new("")), Some(OsStr::new("xterm-256color")), None);
        assert_ne!(empty.status(ProcStatus::Errored), Style::default());
    }

    /// fails if an unknown terminal starts being handed 256-colour indices.
    /// The recoverable direction is the narrow one: a 256-colour terminal shown
    /// 16 colours looks flatter, while a 16-colour terminal sent
    /// `\x1b[38;5;166m` can print the escape as literal text.
    #[test]
    fn an_unknown_terminal_gets_the_sixteen_colour_palette() {
        let sixteen = Palette::detect(None, Some(OsStr::new("vt100")), None);
        assert_eq!(sixteen.status(ProcStatus::Errored).fg, Some(Color::Red));
        assert_eq!(sixteen.status(ProcStatus::Online).fg, Some(Color::Green));

        let deep = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        assert_eq!(deep.status(ProcStatus::Errored).fg, Some(Color::Indexed(166)));

        let truecolor = Palette::detect(None, Some(OsStr::new("dumb")), Some(OsStr::new("truecolor")));
        assert_eq!(truecolor.status(ProcStatus::Online).fg, Some(Color::Indexed(29)));
    }

    /// fails if `--bark` leaks onto anything that is not errored, refused or
    /// destructive. The design language reserves that colour and says so in
    /// those words; `waiting-restart` is the live temptation, because it is a
    /// state an operator worries about — and it is `--butter`, attention, not
    /// damage.
    #[test]
    fn bark_is_reserved_for_errored_and_nothing_else() {
        let p = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let bark = Some(Color::Indexed(166));
        assert_eq!(p.status(ProcStatus::Errored).fg, bark);
        for other in [
            ProcStatus::Online,
            ProcStatus::Starting,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::WaitingRestart,
        ] {
            assert_ne!(p.status(other).fg, bark, "{other} must not be bark-coloured");
        }
        // The two non-status uses the design language does allow.
        assert_eq!(p.alarm().fg, bark);
        assert_eq!(p.refusal().fg, bark);
    }

    /// fails if a status stops being distinguishable by TEXT alone. Colour in
    /// this dashboard is always redundant with the word beside it — that is
    /// what makes `NO_COLOR` a loss of decoration rather than a loss of
    /// information, and it is the house rule that the theme never costs
    /// clarity. This test is the rule, written where it can fail.
    #[test]
    fn every_status_is_legible_with_no_colour_at_all() {
        let off = Palette::detect(Some(OsStr::new("1")), None, None);
        let mut seen = std::collections::BTreeSet::new();
        for status in [
            ProcStatus::Online,
            ProcStatus::Starting,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
        ] {
            assert_eq!(off.status(status), Style::default());
            assert!(seen.insert(status.to_string()), "two statuses share one word");
        }
        assert_eq!(seen.len(), 6);
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find type
`Palette` in this scope``.

### Step 2.3 — GREEN

`crates/shep-cli/src/lookout/theme.rs`:

```rust
//! The design language's semantic colours, mapped onto a terminal.
//!
//! `docs/shep-design/README.md` assigns meaning to four tokens — `--meadow`
//! for online and healthy, `--bark` for errored and refused and destructive
//! and nothing else, `--butter` for attention, `--ink-3` for muted labels and
//! captions — and states one rule this module exists to keep: *errors get a
//! colour, not a face*. A terminal has 16 or 256 colours rather than hex
//! tokens, so this maps rather than quotes.
//!
//! Two things are deliberately NOT mapped. `--paper` is the design language's
//! page background; painting it here would fight the operator's own terminal
//! theme and lose on half of them, so no background is painted at all and
//! ordinary text is [`Color::Reset`]. `--barn` is scenery-only in the design
//! language's own words, and there is no scenery in a dashboard.
//!
//! **Colour is always redundant with text here.** Every coloured cell is a cell
//! whose text already says the same thing: the STATUS column prints `errored`
//! and `--bark` sits on top of that word. That is what makes both downgrades
//! below losses of decoration rather than losses of information.

use ratatui::style::{Color, Style};
use std::ffi::OsStr;

use shep_core::status::ProcStatus;

/// The four semantic colours, resolved for one terminal.
///
/// Constructed once at startup by [`Palette::detect`] and carried in
/// `super::app::App`; never re-derived per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    meadow: Option<Color>,
    bark: Option<Color>,
    butter: Option<Color>,
    ink3: Option<Color>,
}

impl Palette {
    /// Resolves the palette from the environment, taken as arguments rather
    /// than read here.
    ///
    /// A pure function over its inputs, the same shape (and for the same
    /// testability reason) as `crate::commands::daemon::ansi_enabled`: the
    /// caller in `super::mod` does the `std::env` reads.
    ///
    /// - `no_color` set and **non-empty** flattens everything. An empty
    ///   `NO_COLOR=` is an unset one — the cross-ecosystem convention, and the
    ///   one already pinned for the shepherd's own log output.
    /// - `colorterm` containing `truecolor` or `24bit`, or `term` containing
    ///   `256color`, gets the 256-colour indices.
    /// - Anything else gets the 16 named colours. Erring narrow is the
    ///   recoverable direction: a deep terminal shown 16 colours looks
    ///   flatter, while a shallow one sent `\x1b[38;5;166m` can print the
    ///   escape as literal text.
    #[must_use]
    pub fn detect(
        no_color: Option<&OsStr>,
        term: Option<&OsStr>,
        colorterm: Option<&OsStr>,
    ) -> Self {
        if no_color.is_some_and(|value| !value.is_empty()) {
            return Self {
                meadow: None,
                bark: None,
                butter: None,
                ink3: None,
            };
        }
        let deep = colorterm.is_some_and(|value| {
            let value = value.to_string_lossy();
            value.contains("truecolor") || value.contains("24bit")
        }) || term.is_some_and(|value| value.to_string_lossy().contains("256color"));

        if deep {
            // xterm-256 indices chosen as the nearest neighbours of the design
            // language's own hexes: 29 #00875f for --meadow #2E8B57, 166
            // #d75f00 for --bark #E0552B, 221 #ffd75f for --butter #F3C44C,
            // 245 #8a8a8a for --ink-3 #7A8C80.
            Self {
                meadow: Some(Color::Indexed(29)),
                bark: Some(Color::Indexed(166)),
                butter: Some(Color::Indexed(221)),
                ink3: Some(Color::Indexed(245)),
            }
        } else {
            Self {
                meadow: Some(Color::Green),
                bark: Some(Color::Red),
                butter: Some(Color::Yellow),
                ink3: Some(Color::DarkGray),
            }
        }
    }

    /// The style for one sheep's STATUS cell.
    ///
    /// `Errored` is the only status that gets `--bark`; `waiting-restart` is
    /// `--butter`, because it is a state to watch rather than damage that has
    /// happened. `Stopping` and `Stopped` are muted: a sheep that was asked to
    /// go and went is not a problem.
    #[must_use]
    pub fn status(self, status: ProcStatus) -> Style {
        let colour = match status {
            ProcStatus::Online => self.meadow,
            ProcStatus::Starting | ProcStatus::WaitingRestart => self.butter,
            ProcStatus::Errored => self.bark,
            ProcStatus::Stopping | ProcStatus::Stopped => self.ink3,
        };
        Self::fg(colour)
    }

    /// Muted: column headers, the home path in the title, key hints.
    #[must_use]
    pub fn muted(self) -> Style {
        Self::fg(self.ink3)
    }

    /// Damage that has happened: the frozen banner, a failed poll.
    #[must_use]
    pub fn alarm(self) -> Style {
        Self::fg(self.bark)
    }

    /// A refused action. `--bark`'s third permitted use, per the design
    /// language's own list — errored, refused, destructive.
    #[must_use]
    pub fn refusal(self) -> Style {
        Self::fg(self.bark)
    }

    /// Something to look at that is not damage: reconnecting, a dropped-event
    /// notice.
    #[must_use]
    pub fn attention(self) -> Style {
        Self::fg(self.butter)
    }

    fn fg(colour: Option<Color>) -> Style {
        colour.map_or_else(Style::default, |colour| Style::default().fg(colour))
    }
}
```

`crates/shep-cli/src/lookout/mod.rs`, for now:

```rust
//! `shep lookout` (alias `dash`): the terminal dashboard.
//!
//! Phase 12a builds the shell and one pane. The rest is 12b — see this file's
//! sibling `docs/lookout/README.md`.

pub mod theme;
```

`crates/shep-cli/src/main.rs`, beside the other `#[cfg(unix)]` module lines:

```rust
#[cfg(unix)]
mod lookout;
```

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+4**.

### Step 2.4 — MUTATION

In `theme.rs`, change `Palette::status`'s `WaitingRestart` arm from
`self.butter` to `self.bark`.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `bark_is_reserved_for_errored_and_nothing_else` fails on the
`WaitingRestart` iteration with the message `waiting-restart must not be
bark-coloured`. `every_status_is_legible_with_no_colour_at_all` must stay
**green** — it is testing a different property, and a mutation that reddens both
would mean the two tests are one test written twice.

Revert.

### Step 2.5 — second MUTATION

In `Palette::detect`, change `no_color.is_some_and(|value| !value.is_empty())`
to `no_color.is_some()`.

**Must go red:** `no_color_flattens_the_palette_and_an_empty_one_does_not`
fails on its `assert_ne!` — the empty `NO_COLOR=` now flattens.

Revert, then run the full task gate.

---

## Task 3 — `lookout/app.rs`: the reducer

The testable core. Sync, no I/O, no runtime, no terminal types, and **no
clock** — every `Instant` is passed in on the message, so every uptime
assertion is exact arithmetic rather than a sleep.

**Files created:**
- `crates/shep-cli/src/lookout/app.rs`.

**Files modified:**
- `crates/shep-cli/src/lookout/mod.rs` — `pub mod app;`.

**Produces, for Tasks 4, 6 and 8:**

```rust
pub enum Msg {
    Snapshot { rows: Vec<ProcessInfo>, at: Instant },
    Event(BusEvent),
    BusLagged { count: u64 },
    Retrying { attempt: u32 },
    Relinked,
    Frozen { at_local: String },
    Key(KeyPress),
    Tick { now: Instant },
    Resize,
}
pub enum Effect { None, PollNow, Quit }
pub enum KeyPress { Quit, ScrollUp, ScrollDown, ScrollTop, ScrollBottom, Refresh, Stop }
pub enum Control { ReadOnly, Allowed }
pub enum Link { Live, Retrying { attempt: u32 }, Lost { at_local: String } }
pub struct App { /* ...; built by App::new(palette, control, home, now) */ }
pub struct Row { pub info: ProcessInfo, pub anchor: Instant }
```

### Step 3.1 — RED

Add `crates/shep-cli/src/lookout/app.rs` with only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::ProcessEventKind;

    fn sheep(id: u32, name: &str, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(id, name, status)
            .pid(Some(1000 + id))
            .uptime_ms(60_000)
            .build()
    }

    fn started() -> (App, Instant) {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(2, "api", ProcStatus::Errored),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        (app, t0)
    }

    /// fails if the poll stops being the truth. A bus event upserts, but a
    /// snapshot REPLACES: the bus is lossy by construction, so a sheep the
    /// events invented — or one they never learned had been deleted — must not
    /// survive the next listing. Event-sourcing a lossy bus is the exact drift
    /// this reducer exists to prevent.
    #[test]
    fn a_snapshot_replaces_the_flock_wholesale() {
        let (mut app, t0) = started();
        app.update(Msg::Event(BusEvent::Process {
            event: ProcessEventKind::Start,
            info: sheep(9, "ghost", ProcStatus::Starting),
            manually: true,
            at_ms: 0,
        }));
        assert_eq!(app.rows().len(), 4, "the bus event upserted");

        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.rows().len(), 1);
        assert!(app.rows().iter().all(|row| row.info.id == 1));
    }

    /// fails if a snapshot that shrinks the flock leaves the viewport scrolled
    /// past the end of it. The map is REPLACED wholesale every two seconds, so
    /// an offset that was valid for six sheep can outlive four of them — and a
    /// pane scrolled past its own last row draws nothing at all, which reads
    /// as a crash rather than as a small flock.
    #[test]
    fn a_snapshot_that_shrinks_the_flock_pulls_the_scroll_back() {
        let (mut app, t0) = started();
        app.update(Msg::Key(KeyPress::ScrollBottom));
        assert_eq!(app.scroll(), 2);

        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.scroll(), 0, "the offset came back with the flock");

        app.update(Msg::Snapshot { rows: vec![], at: t0 });
        assert_eq!(app.scroll(), 0, "an empty flock scrolls nowhere");
    }

    /// fails if a dropped or lagged frame stops triggering an immediate poll.
    /// The drop carries no information about what was lost, so the only repair
    /// is to ask the shepherd what things look like now — bark's own reason,
    /// and the reason this dashboard does not go silently wrong under load.
    #[test]
    fn a_drop_and_a_lag_both_ask_for_an_immediate_poll() {
        let (mut app, _) = started();
        assert_eq!(
            app.update(Msg::Event(BusEvent::Dropped { count: 12 })),
            Effect::PollNow
        );
        assert_eq!(app.update(Msg::BusLagged { count: 3 }), Effect::PollNow);
        assert_eq!(
            app.update(Msg::Event(BusEvent::Process {
                event: ProcessEventKind::Online,
                info: sheep(1, "web", ProcStatus::Online),
                manually: false,
                at_ms: 0,
            })),
            Effect::None,
            "an ordinary event needs no repair"
        );
    }

    /// fails if the two drop conditions stop being told apart. `Dropped` is the
    /// SHEPHERD's outbound queue overflowing; `Lagged` is this process failing
    /// to read its own socket fast enough. They live on opposite ends of the
    /// connection, and an operator cannot tell which end to investigate if the
    /// notice reads the same. `bleats` pins the same distinction.
    #[test]
    fn a_shepherd_side_drop_and_a_local_lag_read_differently() {
        let (mut app, _) = started();
        app.update(Msg::Event(BusEvent::Dropped { count: 12 }));
        let shepherd_side = app.notice().expect("a drop leaves a notice").to_string();
        app.update(Msg::BusLagged { count: 3 });
        let local = app.notice().expect("a lag leaves a notice").to_string();

        assert!(shepherd_side.contains("the shepherd dropped"));
        assert!(local.contains("lookout fell behind"));
        assert_ne!(shepherd_side, local);
    }

    /// fails if the uptime column stops advancing between polls. A dashboard
    /// whose uptime only moves on the 2s poll reads as frozen when it is not.
    #[test]
    fn a_running_sheeps_uptime_advances_with_the_heartbeat() {
        let (mut app, t0) = started();
        assert_eq!(app.uptime_ms(app.rows()[0].info.id), Some(60_000));
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(5),
        });
        assert_eq!(app.uptime_ms(1), Some(65_000));
    }

    /// fails if a FROZEN dashboard keeps counting. This is the specific lie
    /// this whole state exists to avoid: the shepherd is gone, so nothing on
    /// screen is known to still be true, and an UPTIME column that keeps
    /// ticking asserts second by second that a process is still running when
    /// nothing can see it.
    #[test]
    fn a_frozen_dashboard_stops_the_uptime_clock() {
        let (mut app, t0) = started();
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(5),
        });
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        let at_freeze = app.uptime_ms(1);
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(400),
        });
        assert_eq!(app.uptime_ms(1), at_freeze, "the clock stopped with the link");
        assert_eq!(at_freeze, Some(65_000));
    }

    /// fails if a sheep that is not running has its uptime animated. A stopped
    /// sheep's `uptime_ms` is a historical fact — how long it ran — and
    /// advancing it invents one.
    #[test]
    fn a_stopped_sheeps_uptime_does_not_advance() {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Stopped)],
            at: t0,
        });
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(30),
        });
        assert_eq!(app.uptime_ms(1), Some(60_000));
    }

    /// fails if the control gate stops refusing, or starts refusing with
    /// whimsy. Both refusals are literal: the design language's own standing
    /// rule is that nothing about damage gets charming, and a stop is damage.
    #[test]
    fn the_stop_key_refuses_in_both_control_states() {
        let (mut app, _) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::Stop)), Effect::None);
        let read_only = app.notice().expect("a refusal is a notice").to_string();
        assert!(read_only.contains("read-only"));
        assert!(read_only.contains("--allow-control"));

        let t0 = Instant::now();
        let mut allowed = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/rin/.shep".to_string(),
            t0,
        );
        allowed.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(allowed.update(Msg::Key(KeyPress::Stop)), Effect::None);
        let not_built = allowed.notice().expect("a refusal is a notice").to_string();
        assert!(not_built.contains("not built yet"));
        assert_ne!(read_only, not_built);
    }

    /// fails if lookout learns to exit on its own. A `DaemonShutdown` is a
    /// notice here, where in `bleats` it precedes a clean exit — the whole
    /// point of Rin's ruling is that a standing dashboard admits it is stale
    /// rather than vanishing. Only `q` quits.
    #[test]
    fn nothing_but_a_keypress_quits() {
        let (mut app, _) = started();
        for msg in [
            Msg::Event(BusEvent::DaemonShutdown),
            Msg::Event(BusEvent::Dropped { count: 1 }),
            Msg::BusLagged { count: 1 },
            Msg::Retrying { attempt: 5 },
            Msg::Frozen {
                at_local: "2026-08-14 14:32:07".to_string(),
            },
        ] {
            assert_ne!(app.update(msg), Effect::Quit);
        }
        assert_eq!(app.update(Msg::Key(KeyPress::Quit)), Effect::Quit);
    }

    /// fails if a reconnect leaves the dashboard in a state that says nothing.
    /// `Retrying` has to be visible while it is happening — an operator
    /// watching a shepherd restart should see it, and should see it clear.
    #[test]
    fn the_link_state_walks_live_to_retrying_to_lost_and_back() {
        let (mut app, t0) = started();
        assert_eq!(app.link(), &Link::Live);

        app.update(Msg::Retrying { attempt: 1 });
        assert_eq!(app.link(), &Link::Retrying { attempt: 1 });

        app.update(Msg::Relinked);
        assert_eq!(app.link(), &Link::Live);

        app.update(Msg::Retrying { attempt: 5 });
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        assert_eq!(
            app.link(),
            &Link::Lost {
                at_local: "2026-08-14 14:32:07".to_string()
            }
        );

        // A late snapshot after the freeze must not silently unfreeze it: the
        // link task has ended, so there is nothing left to produce one, and
        // accepting it would be accepting a message that cannot exist.
        app.update(Msg::Snapshot { rows: vec![], at: t0 });
        assert!(matches!(app.link(), Link::Lost { .. }));
    }

    /// fails if scrolling up at the first row or down at the last one wraps or
    /// panics. Clamping is the choice: wrapping a two-hundred-sheep flock from
    /// the last row to the first on one keypress loses the operator's place
    /// with nothing to undo it.
    #[test]
    fn the_scroll_offset_clamps_at_both_ends() {
        let (mut app, _) = started();
        for _ in 0..10 {
            app.update(Msg::Key(KeyPress::ScrollUp));
        }
        assert_eq!(app.scroll(), 0, "up past the first row stays on it");
        for _ in 0..10 {
            app.update(Msg::Key(KeyPress::ScrollDown));
        }
        assert_eq!(app.scroll(), 2, "down past the last row stays on it");
        app.update(Msg::Key(KeyPress::ScrollTop));
        assert_eq!(app.scroll(), 0);
        app.update(Msg::Key(KeyPress::ScrollBottom));
        assert_eq!(app.scroll(), 2);
    }

    /// fails if `r` stops asking for a poll while the link is live, or starts
    /// pretending to ask once it is frozen. It is the one key that does I/O,
    /// and it is what an operator presses when they do not believe the screen
    /// — so it has to be honest in both directions. The link task ENDS at the
    /// freeze (design decision 8), which drops its poll receiver, so an
    /// `Effect::PollNow` after that is a `try_send` into a closed channel:
    /// the operator presses the key and gets silence with no reason given.
    #[test]
    fn refresh_polls_while_live_and_says_why_it_cannot_once_frozen() {
        let (mut app, _) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::Refresh)), Effect::PollNow);

        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        assert_eq!(
            app.update(Msg::Key(KeyPress::Refresh)),
            Effect::None,
            "there is no link task left to ask"
        );
        let notice = app.notice().expect("a refusal is a notice").to_string();
        assert!(notice.contains("the shepherd is gone"));
        assert!(notice.contains("nothing left to ask"));
    }

    /// fails if a notice outlives the keypress after it. A stale refusal still
    /// on screen a minute later is read as a live one.
    #[test]
    fn the_next_keypress_clears_the_notice() {
        let (mut app, _) = started();
        app.update(Msg::Key(KeyPress::Stop));
        assert!(app.notice().is_some());
        app.update(Msg::Key(KeyPress::ScrollDown));
        assert!(app.notice().is_none());
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find type
`App` in this scope``.

### Step 3.2 — GREEN

`crates/shep-cli/src/lookout/app.rs`:

```rust
//! The lookout's state and its reducer: `Msg` in, `Effect` out.
//!
//! Everything about this module is chosen so it can be tested without a
//! terminal, a runtime, a socket, or a sleep:
//!
//! - **No I/O.** [`App::update`] is a synchronous function of `&mut self` and
//!   one [`Msg`]. Work that has to happen outside comes back out as an
//!   [`Effect`] for the caller in `super::mod` to run.
//! - **No terminal types.** Nothing here imports `ratatui` beyond
//!   [`super::theme::Palette`] (which is a style value, not a widget) or
//!   `crossterm` at all — `super::input` maps a `KeyEvent` to a [`KeyPress`]
//!   before it reaches this module.
//! - **No clock.** Every `Instant` arrives on the message
//!   ([`Msg::Tick`], [`Msg::Snapshot`]). A test asserts on uptime arithmetic
//!   exactly rather than sleeping and hoping.
//!
//! **The flock map is keyed by sheep id, and the poll is the truth.** The bus
//! is lossy by construction (`tokio::sync::broadcast` drops for a lagging
//! subscriber), so a flock view built from events alone WILL drift.
//! [`Msg::Event`] upserts for latency; [`Msg::Snapshot`] replaces the whole map
//! and wins every conflict. The only cursor 12a carries is a **scroll offset**
//! — which row of the flock sits first on screen — re-clamped against the map
//! every time it is replaced, because an offset that was valid for six sheep
//! outlives four of them by two seconds. A *selected* row, and the reseat rule
//! a wholesale replacement would need for it, arrive in 12b together with the
//! detail pane that reads them; the phase plan's "What 12b gets" section
//! records that as a decision rather than an omission.

use core::fmt;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo};
use shep_core::status::ProcStatus;

use super::theme::Palette;

/// Whether this lookout may act on a sheep.
///
/// Default is [`Self::ReadOnly`], per Rin's ruling, mirroring the
/// `allow_control` precedent spec §9 sets for whistle. Turned on by
/// `--allow-control` or by `lookout.allow_control` in the KV store.
///
/// **This is a fat-finger catch, not a security boundary.** lookout runs as the
/// operator's own process under the operator's own uid; anyone who can run it
/// can run `shep stop`. The gate exists so a keystroke in a dashboard someone
/// is reading does not become an action they did not intend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Actions refuse. The default.
    ReadOnly,
    /// Actions are permitted — in 12a there are none, so they refuse with a
    /// different sentence.
    Allowed,
}

/// The keys lookout binds, named by what they mean rather than by which key
/// produces them.
///
/// A plain enum, rather than `crossterm::event::KeyEvent`, so this module and
/// its tests never touch a terminal crate: `super::input::map_key` does the
/// translation at the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    /// `q`, `Esc`, or `Ctrl-C`.
    Quit,
    /// `k` or `Up` — the viewport moves up one row.
    ScrollUp,
    /// `j` or `Down` — the viewport moves down one row.
    ScrollDown,
    /// `g` or `Home` — the top of the flock.
    ScrollTop,
    /// `G` or `End` — the bottom of it.
    ScrollBottom,
    /// `r` — poll now.
    Refresh,
    /// `x` — the one action key. Refuses in both control states in 12a; see
    /// the plan's design decision 2.
    Stop,
}

/// Everything that can change the dashboard.
#[derive(Debug, Clone)]
pub enum Msg {
    /// A `Request::ListFlock` reply landed. `at` is when it was received, and
    /// becomes every row's uptime anchor.
    Snapshot {
        /// The flock as the shepherd reported it.
        rows: Vec<ProcessInfo>,
        /// When the reply was received.
        at: Instant,
    },
    /// One frame off the bus.
    Event(BusEvent),
    /// This client's own receiver fell behind and discarded frames — the
    /// local half of the drop problem, distinct from
    /// [`BusEvent::Dropped`], which is the shepherd's own queue.
    BusLagged {
        /// How many frames this process lost.
        count: u64,
    },
    /// The link task is re-dialling; `attempt` is 1-based.
    Retrying {
        /// Which attempt is in flight.
        attempt: u32,
    },
    /// The link task reconnected and re-subscribed.
    Relinked,
    /// The reconnect ladder is exhausted. Everything on screen is now frozen.
    ///
    /// `at_local` is a pre-formatted local timestamp rather than an instant to
    /// format here: this module holds no clock and no formatter, and a frozen
    /// banner whose text is supplied is a banner a snapshot test can pin.
    Frozen {
        /// When the link was declared lost, already rendered for display.
        at_local: String,
    },
    /// One key.
    Key(KeyPress),
    /// The 1s heartbeat. `now` is what advances every running sheep's uptime.
    Tick {
        /// The current instant, read by the caller.
        now: Instant,
    },
    /// The terminal changed size; nothing to update but the frame is stale.
    Resize,
}

/// What the caller has to do after an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Nothing.
    None,
    /// Ask the link task for a `ListFlock` now, rather than at the next tick.
    PollNow,
    /// Leave.
    Quit,
}

/// The connection's state, as the dashboard reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// Connected and subscribed.
    Live,
    /// Re-dialling. `attempt` is 1-based and bounded by
    /// `super::link::RECONNECT_ATTEMPTS`.
    Retrying {
        /// Which attempt is in flight.
        attempt: u32,
    },
    /// The ladder is exhausted. Terminal: nothing moves this state, and the
    /// values on screen stay exactly as they were.
    Lost {
        /// When it was declared lost, already rendered for display.
        at_local: String,
    },
}

/// One sheep's row: what the shepherd said, and when it said it.
#[derive(Debug, Clone)]
pub struct Row {
    /// The shepherd's own snapshot of this sheep.
    pub info: ProcessInfo,
    /// When [`Self::info`] was received — the origin for this row's live
    /// uptime, so a value two seconds old is never rendered as current.
    pub anchor: Instant,
}

/// A short line the status bar shows instead of the key hints, cleared by the
/// next keypress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    text: String,
    /// True for a refusal or a damage report — the status bar picks
    /// [`Palette::refusal`] over [`Palette::attention`].
    grave: bool,
}

impl Notice {
    /// Whether this notice is a refusal or a damage report rather than an
    /// informational one.
    #[must_use]
    pub fn is_grave(&self) -> bool {
        self.grave
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// The whole dashboard's state.
#[derive(Debug)]
pub struct App {
    flock: BTreeMap<u32, Row>,
    /// Which row of [`Self::flock`] is first on screen. An offset, not a
    /// selection: nothing in 12a reads "the sheep the operator is pointing
    /// at", and the view clamps this against its own viewport height as well
    /// ([`super::view::flock::scroll_offset`]).
    scroll: usize,
    link: Link,
    notice: Option<Notice>,
    palette: Palette,
    control: Control,
    /// The `$SHEP_HOME` this lookout watches, for the title line. Held here
    /// rather than threaded through `super::view::draw`: it never changes for
    /// the life of the process, and a render function taking it as an argument
    /// would make every call site — including eight scene fixtures — carry it.
    home: String,
    /// The clock the view reads. Advanced by [`Msg::Tick`] — and deliberately
    /// NOT advanced once the link is [`Link::Lost`], which is what stops a
    /// frozen dashboard's uptime column from counting up for a sheep nothing
    /// can see.
    now: Instant,
}

impl App {
    /// A dashboard with an empty flock, a live link, and no notice.
    #[must_use]
    pub fn new(palette: Palette, control: Control, home: String, now: Instant) -> Self {
        Self {
            flock: BTreeMap::new(),
            scroll: 0,
            link: Link::Live,
            notice: None,
            palette,
            control,
            home,
            now,
        }
    }

    /// Applies one message and reports what the caller must do next.
    pub fn update(&mut self, msg: Msg) -> Effect {
        match msg {
            Msg::Snapshot { rows, at } => {
                // A snapshot cannot arrive after a freeze — the link task has
                // ended by then, so there is nothing left to produce one. If
                // one does, it is a message from a task that should not exist,
                // and accepting it would silently un-freeze a dashboard whose
                // banner says otherwise.
                if matches!(self.link, Link::Lost { .. }) {
                    return Effect::None;
                }
                self.flock = rows
                    .into_iter()
                    .map(|info| (info.id, Row { info, anchor: at }))
                    .collect();
                self.clamp_scroll();
                Effect::None
            }
            Msg::Event(event) => self.on_event(event),
            Msg::BusLagged { count } => {
                self.notice = Some(Notice {
                    text: format!("lookout fell behind and lost {count} events; re-reading the flock"),
                    grave: false,
                });
                Effect::PollNow
            }
            Msg::Retrying { attempt } => {
                if !matches!(self.link, Link::Lost { .. }) {
                    self.link = Link::Retrying { attempt };
                }
                Effect::None
            }
            Msg::Relinked => {
                if !matches!(self.link, Link::Lost { .. }) {
                    self.link = Link::Live;
                }
                Effect::None
            }
            Msg::Frozen { at_local } => {
                self.link = Link::Lost { at_local };
                Effect::None
            }
            Msg::Tick { now } => {
                // The one line that keeps a frozen dashboard honest.
                if !matches!(self.link, Link::Lost { .. }) {
                    self.now = now;
                }
                Effect::None
            }
            Msg::Resize => Effect::None,
            Msg::Key(key) => self.on_key(key),
        }
    }

    fn on_event(&mut self, event: BusEvent) -> Effect {
        match event {
            BusEvent::Process { event, info, .. } => {
                if matches!(event, ProcessEventKind::Delete) {
                    self.flock.remove(&info.id);
                } else {
                    let anchor = self.now;
                    self.flock.insert(info.id, Row { info, anchor });
                }
                self.clamp_scroll();
                Effect::None
            }
            // The shepherd's own outbound queue overflowed for this
            // subscriber. Deliberately worded differently from
            // `Msg::BusLagged` above: the two failures live on opposite ends of
            // the connection, and an operator cannot tell which end to
            // investigate if they read the same. `bleats` pins the identical
            // distinction.
            BusEvent::Dropped { count } => {
                self.notice = Some(Notice {
                    text: format!("the shepherd dropped {count} events; re-reading the flock"),
                    grave: false,
                });
                Effect::PollNow
            }
            // A notice, not an exit. `bleats` prints this and then leaves,
            // which is right for a follow; a standing dashboard that vanished
            // when the shepherd went down would take the last known state with
            // it. The link task will find the socket gone, climb the reconnect
            // ladder, and freeze if it runs out.
            BusEvent::DaemonShutdown => {
                self.notice = Some(Notice {
                    text: "the shepherd is shutting down".to_string(),
                    grave: true,
                });
                Effect::None
            }
            // `BusEvent` is `#[non_exhaustive]`: a variant a newer shepherd
            // added must not take the dashboard down, and must not be reported
            // as anything. `Dropped` and `DaemonShutdown` are NOT in this arm
            // — both are named variants this binary understands.
            _ => Effect::None,
        }
    }

    fn on_key(&mut self, key: KeyPress) -> Effect {
        self.notice = None;
        match key {
            KeyPress::Quit => Effect::Quit,
            // The one key that does I/O, and it refuses honestly once the
            // link task has ended: its poll receiver is gone by then, so an
            // `Effect::PollNow` would be a `try_send` into a closed channel
            // and the operator would get silence with no reason for it.
            KeyPress::Refresh => {
                if matches!(self.link, Link::Lost { .. }) {
                    self.notice = Some(Notice {
                        text: "the shepherd is gone — nothing left to ask".to_string(),
                        grave: true,
                    });
                    return Effect::None;
                }
                Effect::PollNow
            }
            KeyPress::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
                Effect::None
            }
            KeyPress::ScrollDown => {
                self.scroll = (self.scroll + 1).min(self.last_row());
                Effect::None
            }
            KeyPress::ScrollTop => {
                self.scroll = 0;
                Effect::None
            }
            KeyPress::ScrollBottom => {
                self.scroll = self.last_row();
                Effect::None
            }
            KeyPress::Stop => {
                // Both texts are literal. The design language's standing rule
                // is that nothing about damage gets charming, and a stop is
                // damage; the house rule says destructive operations and error
                // text stay plain.
                let text = match self.control {
                    Control::ReadOnly => {
                        "read-only: actions need --allow-control".to_string()
                    }
                    Control::Allowed => "stop is not built yet".to_string(),
                };
                self.notice = Some(Notice { text, grave: true });
                Effect::None
            }
        }
    }

    /// The index of the last row that exists, or `0` for an empty flock.
    ///
    /// The ceiling on [`Self::scroll`]. Clamped rather than wrapping:
    /// wrapping a two-hundred-sheep flock from the last row to the first on
    /// one keypress loses the operator's place with nothing to undo it.
    fn last_row(&self) -> usize {
        self.flock.len().saturating_sub(1)
    }

    /// Pulls the scroll offset back inside a flock that just got smaller.
    ///
    /// [`Msg::Snapshot`] replaces the map wholesale, so an offset that was
    /// valid two seconds ago can now point past the end. A pane scrolled past
    /// its own last row draws nothing at all, which an operator reads as a
    /// crash rather than as a small flock.
    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.last_row());
    }

    /// The flock, in id order.
    #[must_use]
    pub fn rows(&self) -> Vec<&Row> {
        self.flock.values().collect()
    }

    /// Which row of the flock is first on screen.
    ///
    /// A request, not a result: the view clamps it again against its own
    /// viewport height, because this module does not know how tall the
    /// terminal is. See [`super::view::flock::scroll_offset`].
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The link state, as the status bar reports it.
    #[must_use]
    pub fn link(&self) -> &Link {
        &self.link
    }

    /// The current notice, if the last message left one.
    #[must_use]
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// The resolved palette.
    #[must_use]
    pub fn palette(&self) -> Palette {
        self.palette
    }

    /// Whether actions are permitted.
    #[must_use]
    pub fn control(&self) -> Control {
        self.control
    }

    /// The `$SHEP_HOME` this lookout watches.
    #[must_use]
    pub fn home(&self) -> &str {
        &self.home
    }

    /// One sheep's uptime as of this dashboard's own clock, in milliseconds.
    ///
    /// A **running** sheep's uptime advances between polls, from the anchor its
    /// row carries — the alternative, showing a number that only moves every
    /// two seconds, reads as a frozen dashboard when it is not. A sheep that is
    /// not running does not advance: its `uptime_ms` is a historical fact about
    /// how long it ran, and animating it would invent one.
    ///
    /// While the link is [`Link::Lost`], nothing advances at all, because
    /// `self.now` stops.
    #[must_use]
    pub fn uptime_ms(&self, id: u32) -> Option<u64> {
        let row = self.flock.get(&id)?;
        if !matches!(row.info.status, ProcStatus::Online | ProcStatus::Starting) {
            return Some(row.info.uptime_ms);
        }
        let elapsed = self.now.saturating_duration_since(row.anchor);
        Some(row.info.uptime_ms.saturating_add(millis(elapsed)))
    }
}

/// Saturating `Duration` -> milliseconds. A lookout left open for 580 million
/// years is not the failure this guards; the cast is what clippy's
/// `cast_possible_truncation` would otherwise deny.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
```

`crates/shep-cli/src/lookout/mod.rs` grows `pub mod app;`.

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+13**.

### Step 3.3 — MUTATION (the one that matters)

In `App::update`'s `Msg::Tick` arm, remove the guard:

```rust
Msg::Tick { now } => {
    self.now = now;          // was: only while the link is not Lost
    Effect::None
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `a_frozen_dashboard_stops_the_uptime_clock` fails — the frozen
dashboard's uptime advanced from 65 s to 460 s while the shepherd was gone.
`a_running_sheeps_uptime_advances_with_the_heartbeat` must stay **green**: it
covers the other half of the same rule, and a mutation that reddened both would
mean the two tests are one test written twice.

Revert.

### Step 3.4 — second MUTATION

In `Msg::Snapshot`'s arm, replace the wholesale replacement with an upsert:

```rust
for info in rows {
    self.flock.insert(info.id, Row { info, anchor: at });
}
```

**Must go red:** `a_snapshot_replaces_the_flock_wholesale` fails on the second
`assert_eq!` — `ghost`, which the bus invented and the shepherd never listed,
survived the snapshot. Revert.

### Step 3.5 — third MUTATION

In `clamp_scroll`, replace the body with nothing (`{}`), leaving the offset
wherever the last keypress put it.

**Must go red:** `a_snapshot_that_shrinks_the_flock_pulls_the_scroll_back`
fails on its first assertion — the offset is still `2` against a flock of one,
which is a pane scrolled past its own last row and therefore a pane that draws
no rows at all. `the_scroll_offset_clamps_at_both_ends` must stay **green**: it
covers the keypress half of the same rule, and a mutation that reddened both
would mean the two tests are one test written twice.

Revert.

### Step 3.6 — fourth MUTATION

In `on_key`'s `KeyPress::Refresh` arm, delete the `Link::Lost` guard so it
returns `Effect::PollNow` unconditionally.

**Must go red:** `refresh_polls_while_live_and_says_why_it_cannot_once_frozen`
fails on its second assertion — `r` on a frozen dashboard claims to have asked
for a poll, and the caller's `try_send` then drops it into the closed channel
the ended link task left behind. The operator presses the one key that does I/O
and gets nothing, with no reason on screen.

Revert, then run the full task gate.

---

## Task 4 — `lookout/view/`: the flock table and the status bar

**Files created:**
- `crates/shep-cli/src/lookout/view/mod.rs`
- `crates/shep-cli/src/lookout/view/flock.rs`
- `crates/shep-cli/src/lookout/view/status.rs`

**Files modified:**
- `crates/shep-cli/src/lookout/mod.rs` — `pub mod view;`

**Consumes:** Task 2's `Palette`, Task 3's `App`/`Link`/`Control`/`Notice`.

### The layout, by arithmetic

No `Layout`, no `Constraint` — six rows of `Rect` maths, top to bottom:

```
y = 0                  title:   "shep lookout   <home>"        …   "<n> in the flock"
y = 1        (only when the link is not Live) the banner
next                   header:  the column names, muted
next                   rule:    a run of '─', muted
next .. height-2       rows
y = height-1           status bar: link state, control state, keys or a notice
```

`MIN_WIDTH = 31`, `MIN_HEIGHT = 6` — the six lines above with exactly one data
row. Below either, the whole draw is two short lines: `too small`, then
`need 31x6`. Short because they have to survive `set_line`'s silent truncation
in a terminal narrower than `MIN_WIDTH`, which is the only kind of terminal
this branch ever draws in — design decision 6.

### Step 4.1 — RED

`crates/shep-cli/src/lookout/view/flock.rs`, test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the drop order changes without someone re-arguing it. FOLD is
    /// grouping metadata and goes first; CPU and MEM are the last two numbers
    /// to go because they are the ones that explain WHY something is wrong;
    /// ID/NAME/STATUS is the floor because those three are the pane.
    #[test]
    fn columns_drop_in_a_fixed_order_as_the_terminal_narrows() {
        assert_eq!(columns_for(200).len(), 9);
        assert_eq!(columns_for(90).len(), 9);
        assert!(!columns_for(89).contains(&Column::Fold));
        assert!(columns_for(89).contains(&Column::Restarts));
        assert!(!columns_for(67).contains(&Column::Restarts));
        assert!(!columns_for(58).contains(&Column::Pid));
        assert!(!columns_for(48).contains(&Column::Mem));
        assert!(!columns_for(40).contains(&Column::Cpu));
        assert_eq!(
            columns_for(31),
            &[Column::Id, Column::Name, Column::Status]
        );
        // Every tier keeps the three that ARE the pane.
        for width in [31u16, 40, 48, 58, 67, 89, 200] {
            let cols = columns_for(width);
            for required in [Column::Id, Column::Name, Column::Status] {
                assert!(cols.contains(&required), "width {width} dropped {required:?}");
            }
        }
    }

    /// fails if a tier can render wider than the terminal it was chosen for.
    /// This is the check that makes the table above a claim rather than a
    /// wish: every tier's fixed widths plus its separators plus the minimum
    /// NAME must fit in that tier's own threshold.
    #[test]
    fn every_tier_fits_the_width_it_claims() {
        for width in MIN_WIDTH..=200 {
            let cols = columns_for(width);
            let fixed: u16 = cols.iter().map(|c| c.width()).sum();
            let gaps = u16::try_from(cols.len() - 1).unwrap() * 2;
            assert!(
                fixed + gaps + NAME_MIN <= width,
                "width {width} chose {} columns needing {}",
                cols.len(),
                fixed + gaps + NAME_MIN
            );
        }
    }

    /// fails if a long name is cut without saying so. A truncated name that
    /// looks whole is a name an operator will type into `shep stop`.
    #[test]
    fn a_name_too_long_for_its_column_ends_in_an_ellipsis() {
        let cut = fit("payments-reconciliation-worker", 12);
        assert_eq!(cut.chars().count(), 12);
        assert!(cut.ends_with('…'));
        assert!(cut.starts_with("payments"));
        assert_eq!(fit("web", 12), "web         ");
    }

    /// fails if `fit` starts counting bytes. `output::table::render_table`'s
    /// own doc records having avoided the same bug for the same reason.
    ///
    /// **These assert on CONTENT, not on length**, and that is the whole
    /// point: an earlier draft of this test only checked
    /// `fit(..).chars().count() == width`, which is `width` in *both* branches
    /// under either measurement — so the byte-vs-char mutation could not
    /// redden it. The observable difference lives in the PAD branch, where a
    /// three-character nine-byte name asks for nine columns of padding budget
    /// it does not need and comes out short, and at the exactly-fits boundary,
    /// where a byte count truncates a string that already fits.
    #[test]
    fn fit_counts_characters_not_bytes_when_it_pads_and_when_it_truncates() {
        // Pad branch. `日本語` is 3 chars / 9 bytes: char-counted it gets 3
        // trailing spaces, byte-counted it falls into the truncate branch.
        assert_eq!(fit("日本語", 6), "日本語   ");
        // Exactly fits. 7 chars / 11 bytes — a byte count cuts it to
        // `ünïcöd…`.
        assert_eq!(fit("ünïcödé", 7), "ünïcödé");
        // Truncate branch, pinning which prefix survives.
        assert_eq!(fit("日本語アプリ", 5), "日本語ア…");
    }

    /// fails if the viewport stops being clamped to the rows that exist. An
    /// offset past the last row draws an empty pane, which reads as a crash
    /// rather than as a short flock; an offset inside a flock that fits on
    /// screen would scroll rows off the top for no reason.
    #[test]
    fn the_scroll_offset_never_leaves_a_gap_at_the_bottom() {
        // Everything fits: no scrolling, whatever was asked for.
        assert_eq!(scroll_offset(0, 10, 6), 0);
        assert_eq!(scroll_offset(4, 10, 6), 0);
        // Taller than the viewport: the last page is the ceiling, so the
        // bottom row is always the flock's own last row.
        assert_eq!(scroll_offset(0, 5, 20), 0);
        assert_eq!(scroll_offset(7, 5, 20), 7);
        assert_eq!(scroll_offset(19, 5, 20), 15);
        assert_eq!(scroll_offset(usize::MAX, 5, 20), 15);
        // Degenerate: a viewport of zero rows scrolls nowhere.
        assert_eq!(scroll_offset(3, 0, 20), 0);
    }
}
```

`crates/shep-cli/src/lookout/view/mod.rs`, test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use crate::lookout::app::{App, Control, Msg};
    use crate::lookout::theme::Palette;

    fn draw_to(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(app, frame)).unwrap();
        crate::lookout::frames::render_text(terminal.backend().buffer())
    }

    /// fails if a terminal too small to hold the pane is drawn into anyway,
    /// **or if the refusal stops fitting in the terminal it is refusing
    /// about.** Overlapping garbage in a 20-column terminal reads as a crash,
    /// and the operator's next move is to kill the process rather than resize.
    ///
    /// The second half is the one that regresses silently. `Buffer::set_line`
    /// truncates at `max_width` without complaint, and this branch exists for
    /// terminals narrower than 31 columns — so a refusal written as one
    /// 39-character sentence loses `31x6` off the right-hand edge at exactly
    /// the widths that need to read it. Both assertions below are on the WHOLE
    /// line, trimmed, rather than on `contains`, because `contains` passes on
    /// a truncated line as happily as on a whole one.
    #[test]
    fn a_terminal_below_the_floor_says_so_instead_of_drawing() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let frame = draw_to(&app, 28, 8);
        let mut lines = frame.lines();
        assert_eq!(lines.next().unwrap().trim_end(), "too small");
        assert_eq!(lines.next().unwrap().trim_end(), "need 31x6");
        assert!(!frame.contains("STATUS"), "no header was drawn");

        // Narrower still, and taller than the floor in rows: the numbers must
        // survive here too, because a 12-column terminal is precisely the case
        // this message exists for.
        let cramped = draw_to(&app, 12, 8);
        assert!(
            cramped.lines().nth(1).unwrap().trim_end() == "need 31x6",
            "the dimensions were cut off in the terminal that needed them"
        );

        // One row to write into: the second line has nowhere to go, and the
        // draw must not reach past the buffer for it.
        let single = draw_to(&app, 20, 1);
        assert_eq!(single.lines().next().unwrap().trim_end(), "too small");
        assert_eq!(single.lines().count(), 1);
    }

    /// fails if an empty flock renders as a blank pane. A bare empty screen
    /// does not tell an operator whether the shepherd has nothing to run or
    /// whether the dashboard is broken — the same reason
    /// `output::table::render_table` prints its header row for an empty
    /// payload.
    #[test]
    fn an_empty_flock_still_prints_the_header_and_says_it_is_empty() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let frame = draw_to(&app, 100, 12);
        assert!(frame.contains("STATUS"));
        assert!(frame.contains("the flock is empty"));
    }

    /// fails if a frozen dashboard does not say so where it cannot be missed.
    /// This is the whole of Rin's ruling made visible: last values on screen,
    /// and a sentence admitting they are stale.
    #[test]
    fn a_frozen_link_puts_the_banner_under_the_title() {
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        app.update(Msg::Frozen {
            at_local: "2026-08-14 14:32:07".to_string(),
        });
        let frame = draw_to(&app, 100, 12);
        let banner = frame.lines().nth(1).expect("a second line").to_string();
        assert!(banner.contains("the shepherd has died"));
        assert!(banner.contains("2026-08-14 14:32:07"));
    }

    /// fails if the control state stops being visible. An operator who does not
    /// know whether their dashboard can act is one keystroke from finding out
    /// the wrong way.
    #[test]
    fn the_status_bar_always_says_which_control_state_is_in_force() {
        let now = Instant::now();
        let read_only = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            now,
        );
        assert!(draw_to(&read_only, 100, 12).contains("read-only"));

        let allowed = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/rin/.shep".to_string(),
            now,
        );
        let frame = draw_to(&allowed, 100, 12);
        assert!(frame.contains("control enabled"));
        assert!(!frame.contains("read-only"));
    }

    /// fails if a draw panics at a degenerate or an enormous size. IR-40's
    /// boundary sweep: the failure mode this catches is an arithmetic
    /// underflow on `height - 1` in a one-row terminal.
    #[test]
    fn drawing_never_panics_across_the_size_sweep() {
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        app.update(Msg::Snapshot {
            rows: (0..200)
                .map(|id| {
                    ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
                })
                .collect(),
            at: Instant::now(),
        });
        for (width, height) in [(1, 1), (20, 3), (31, 6), (80, 24), (250, 60), (400, 200)] {
            let _ = draw_to(&app, width, height);
        }
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
function `columns_for` in this scope``.

### Step 4.2 — GREEN: `view/flock.rs`

```rust
//! The flock table: which columns fit, and what each row's cells say.
//!
//! The visual contract is "the table `shep flock` prints, live", so this
//! builds `Line`s itself rather than handing the job to
//! `ratatui::widgets::Table`. `crate::output::table::render_table` already owns
//! the house column algorithm — widest cell, two spaces between, no
//! box-drawing — and a second, independent algorithm beside it would drift on
//! the first multi-byte name. Cell values come from the same
//! `crate::output::{human_bytes, human_duration}` the CLI's own rows use, so a
//! number reads identically in both surfaces.
//!
//! Widths here are FIXED per column rather than measured from content, which is
//! the one deliberate departure from `render_table`: a live table whose columns
//! resize as a pid gains a digit is a table that shivers.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{App, Row};
use crate::output::{human_bytes, human_duration};

/// The narrowest terminal the pane will draw into.
///
/// `ID` + `NAME` (floor 8) + `STATUS` (15, the width of `waiting-restart`) plus
/// two separators. Below this the whole draw becomes one line saying so —
/// see [`super::draw`].
pub const MIN_WIDTH: u16 = 31;

/// The shortest terminal the pane will draw into: title, banner, header, rule,
/// one data row, status bar.
pub const MIN_HEIGHT: u16 = 6;

/// The floor on the NAME column, which takes whatever the fixed columns leave.
pub const NAME_MIN: u16 = 8;

/// One column of the flock table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// The sheep's stable numeric id.
    Id,
    /// Its name. The flexible column.
    Name,
    /// Its lifecycle status — the one coloured cell.
    Status,
    /// Its OS pid while running.
    Pid,
    /// Restarts since registration.
    Restarts,
    /// Tree CPU as a percentage of one core.
    Cpu,
    /// Tree resident set size.
    Mem,
    /// Time since its last successful start.
    Uptime,
    /// Fold membership.
    Fold,
}

impl Column {
    /// The header text, matching `output::rows::FlockRows::headers` exactly —
    /// one vocabulary across both surfaces.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Id => "ID",
            Self::Name => "NAME",
            Self::Status => "STATUS",
            Self::Pid => "PID",
            Self::Restarts => "RESTARTS",
            Self::Cpu => "CPU",
            Self::Mem => "MEM",
            Self::Uptime => "UPTIME",
            Self::Fold => "FOLD",
        }
    }

    /// The fixed width of this column's cells. `Name` reports `0` — it is the
    /// column that takes the remainder, and [`name_width`] computes it.
    #[must_use]
    pub const fn width(self) -> u16 {
        match self {
            Self::Id => 4,
            Self::Name => 0,
            // 15: `waiting-restart`, the longest of the six statuses. A status
            // is never truncated — it is the pane.
            Self::Status => 15,
            Self::Pid => 7,
            Self::Restarts => 8,
            Self::Cpu => 6,
            Self::Mem => 8,
            Self::Uptime => 8,
            Self::Fold => 10,
        }
    }
}

const ALL: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Restarts,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
    Column::Fold,
];
const NO_FOLD: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Restarts,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
];
const NO_RESTARTS: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Pid,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
];
const NO_PID: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Cpu,
    Column::Mem,
    Column::Uptime,
];
const NO_MEM: &[Column] = &[
    Column::Id,
    Column::Name,
    Column::Status,
    Column::Cpu,
    Column::Uptime,
];
const NO_CPU: &[Column] = &[Column::Id, Column::Name, Column::Status, Column::Uptime];
const FLOOR: &[Column] = &[Column::Id, Column::Name, Column::Status];

/// Width thresholds, widest first. Each entry is the narrowest terminal that
/// still gets that column set.
///
/// The drop order is least-diagnostic first and it is a decision, not an
/// accident of ordering: FOLD is grouping metadata rather than health;
/// RESTARTS and PID answer follow-up questions rather than "is it up"; CPU and
/// MEM are the last two numbers to go because they are the ones that explain
/// WHY something is wrong. `ID NAME STATUS` is the floor because those three
/// are the pane.
const TIERS: &[(u16, &[Column])] = &[
    (90, ALL),
    (78, NO_FOLD),
    (68, NO_RESTARTS),
    (59, NO_PID),
    (49, NO_MEM),
    (41, NO_CPU),
    (MIN_WIDTH, FLOOR),
];

/// The widest column set that fits `width`.
#[must_use]
pub fn columns_for(width: u16) -> &'static [Column] {
    TIERS
        .iter()
        .find(|(threshold, _)| width >= *threshold)
        .map_or(FLOOR, |(_, columns)| *columns)
}

/// What NAME gets, once the fixed columns and the separators are paid for.
#[must_use]
pub fn name_width(width: u16, columns: &[Column]) -> u16 {
    let fixed: u16 = columns.iter().map(|column| column.width()).sum();
    let gaps = u16::try_from(columns.len().saturating_sub(1)).unwrap_or(0) * 2;
    width
        .saturating_sub(fixed)
        .saturating_sub(gaps)
        .max(NAME_MIN)
}

/// `text` in exactly `width` characters: padded on the right, or truncated
/// with a trailing `…`.
///
/// Counted in `char`s, never bytes — `{:<w$}` pads by character count, so a
/// byte measurement over-pads every multi-byte name. `output::table` records
/// having made the same choice for the same reason.
///
/// A truncated name that looked whole would be a name an operator types into
/// `shep stop`, so the ellipsis is not cosmetic.
#[must_use]
pub fn fit(text: &str, width: u16) -> String {
    let width = usize::from(width);
    let count = text.chars().count();
    if count <= width {
        let mut out = String::from(text);
        out.extend(core::iter::repeat_n(' ', width - count));
        return out;
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// The header line: every column name, muted.
#[must_use]
pub fn header_line(columns: &[Column], width: u16, style: Style) -> Line<'static> {
    let name = name_width(width, columns);
    let mut text = String::new();
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            text.push_str("  ");
        }
        let cell_width = if *column == Column::Name {
            name
        } else {
            column.width()
        };
        text.push_str(&fit(column.header(), cell_width));
    }
    Line::from(Span::styled(text, style))
}

/// One sheep's line. The STATUS cell is the only one that carries colour.
///
/// No row style beyond that: 12a has no selected row (see the phase plan's
/// "What 12b gets"), so there is nothing here for a REVERSED modifier to mean.
#[must_use]
pub fn row_line(app: &App, row: &Row, columns: &[Column], width: u16) -> Line<'static> {
    let palette = app.palette();
    let name = name_width(width, columns);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(columns.len() * 2);
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let cell_width = if *column == Column::Name {
            name
        } else {
            column.width()
        };
        let text = fit(&cell(app, row, *column), cell_width);
        let style = if *column == Column::Status {
            palette.status(row.info.status)
        } else {
            Style::default()
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// One cell's text.
///
/// `-` rather than an empty cell for every unknown, exactly as
/// `output::rows::FlockRows::rows` does and for the same stated reason: an
/// empty cell in a padded table is indistinguishable from a rendering bug, and
/// `0.0%` would claim a measurement the shepherd never made.
fn cell(app: &App, row: &Row, column: Column) -> String {
    let info = &row.info;
    match column {
        Column::Id => info.id.to_string(),
        Column::Name => info.name.clone(),
        Column::Status => info.status.to_string(),
        Column::Pid => info.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        Column::Restarts => info.restarts.to_string(),
        Column::Cpu => info
            .cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        Column::Mem => info.memory_bytes.map_or_else(|| "-".to_string(), human_bytes),
        // The live value, not the snapshot's — `App::uptime_ms` advances a
        // RUNNING sheep between polls and stops entirely once the link is lost.
        Column::Uptime => app
            .uptime_ms(info.id)
            .map_or_else(|| "-".to_string(), human_duration),
        Column::Fold => info.fold.clone().unwrap_or_else(|| "-".to_string()),
    }
}

/// Which slice of the flock is on screen: the first row to draw, given what
/// `App` was asked for and how many rows there is room for.
///
/// `requested` is [`super::super::app::App::scroll`] — the reducer's own
/// offset, which it clamps to the flock's length but cannot clamp to the
/// terminal's height, because it does not know it. This is the second clamp,
/// and it is the one that stops a scrolled pane leaving blank rows above the
/// status bar: once the flock is taller than the viewport, the furthest down
/// it can go is the page that ends on the last row.
///
/// Derived every frame rather than stored: the flock map is replaced wholesale
/// every two seconds, and a stored *result* would have to be reconciled
/// against a list that changed underneath it.
#[must_use]
pub fn scroll_offset(requested: usize, viewport: usize, total: usize) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    requested.min(total - viewport)
}
```

### Step 4.3 — GREEN: `view/status.rs`

```rust
//! The three chrome lines: the title, the link banner, and the status bar.
//!
//! Every sentence here is literal. The design language's standing rule is that
//! nothing about damage gets charming, and this file is where all of shep's
//! damage reporting on this screen lives — the frozen banner, the drop notice,
//! the refusal.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::app::{App, Control, Link};
use super::flock::fit;

/// The title: what this is, where it points, and how big the flock is.
#[must_use]
pub fn title_line(app: &App, home: &str, width: u16) -> Line<'static> {
    let palette = app.palette();
    let left = format!("shep lookout   {home}");
    let count = app.rows().len();
    let right = format!("{count} in the flock");
    Line::from(vec![
        Span::raw(fit(
            &left,
            width.saturating_sub(u16::try_from(right.chars().count()).unwrap_or(0)),
        )),
        Span::styled(right, palette.muted()),
    ])
}

/// The banner, when there is one. `None` while the link is live.
///
/// The frozen sentence is the whole of Rin's ruling in one line: it names what
/// happened, and it names when the values stopped being current, so an operator
/// reading a screen full of `online` knows exactly how much to trust it.
#[must_use]
pub fn banner_line(app: &App) -> Option<Line<'static>> {
    let palette = app.palette();
    match app.link() {
        Link::Live => None,
        Link::Retrying { attempt } => Some(Line::from(Span::styled(
            format!("the shepherd stopped answering — reconnecting (attempt {attempt})"),
            palette.attention(),
        ))),
        Link::Lost { at_local } => Some(Line::from(Span::styled(
            format!("the shepherd has died — these values are frozen as of {at_local}"),
            palette.alarm(),
        ))),
    }
}

/// The bottom line: a notice if there is one, else the key hints; then the
/// control state, always.
#[must_use]
pub fn status_line(app: &App, width: u16) -> Line<'static> {
    let palette = app.palette();
    let (left, left_style) = match app.notice() {
        Some(notice) => (
            notice.to_string(),
            if notice.is_grave() {
                palette.refusal()
            } else {
                palette.attention()
            },
        ),
        None => (
            "q quit   j/k scroll   g/G top/bottom   r refresh   x stop".to_string(),
            palette.muted(),
        ),
    };
    // Always rendered, in both states. An operator who does not know whether
    // their dashboard can act is one keystroke from finding out the wrong way.
    let right = match app.control() {
        Control::ReadOnly => "read-only",
        Control::Allowed => "control enabled",
    };
    let right_len = u16::try_from(right.chars().count()).unwrap_or(0);
    Line::from(vec![
        Span::styled(fit(&left, width.saturating_sub(right_len)), left_style),
        Span::styled(right, palette.muted()),
    ])
}

/// A run of `─` across the pane, under the header.
///
/// One rule, not a box. `output::table`'s own doc argues that a table a user
/// can `awk` over beats one that looks nice; the same instinct applies to a
/// pane an operator reads at 3am, and a full border costs two columns and two
/// rows of the thing they are trying to read.
#[must_use]
pub fn rule_line(style: Style, width: u16) -> Line<'static> {
    Line::from(Span::styled("─".repeat(usize::from(width)), style))
}
```

### Step 4.4 — GREEN: `view/mod.rs`

```rust
//! `draw`: one `App`, one `Frame`, six regions of arithmetic.
//!
//! No `Layout`, no `Constraint`, no widget. The upstream surface this whole
//! phase touches is six items wide — `Frame::area`, `Frame::buffer_mut`,
//! `Buffer::set_line`, `Line`, `Span`, `Style` — which is what makes the render
//! path both testable and cheap to keep working across a ratatui release. See
//! the phase plan's design decision 5b for the argument.

pub mod flock;
pub mod status;

use ratatui::Frame;
use ratatui::text::{Line, Span};

use super::app::App;
use self::flock::{MIN_HEIGHT, MIN_WIDTH};

/// Renders the whole dashboard.
///
/// Synchronous and total: every branch below draws something, and the
/// degenerate cases draw a sentence rather than nothing. A blank pane cannot
/// tell an operator whether the shepherd has nothing to run or whether the
/// dashboard is broken — the same reason `output::table::render_table` prints
/// its header row for an empty payload.
pub fn draw(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area();
    let (width, height) = (area.width, area.height);
    let palette = app.palette();

    if width < MIN_WIDTH || height < MIN_HEIGHT {
        // Two nine-character lines, not one 39-character sentence.
        // `Buffer::set_line` truncates at `max_width` in silence, and this
        // branch exists for terminals narrower than `MIN_WIDTH` — so a refusal
        // that does not fit inside `MIN_WIDTH` loses its own numbers at
        // exactly the widths that need them. Nine columns is the floor at
        // which both lines are still whole, and below that nothing helps.
        if width == 0 || height == 0 {
            return;
        }
        let first = Line::from(Span::raw("too small"));
        frame.buffer_mut().set_line(area.x, area.y, &first, width);
        if height >= 2 {
            let second = Line::from(Span::raw(format!("need {MIN_WIDTH}x{MIN_HEIGHT}")));
            frame.buffer_mut().set_line(area.x, area.y + 1, &second, width);
        }
        return;
    }

    let mut y = area.y;
    let buffer = frame.buffer_mut();

    buffer.set_line(area.x, y, &status::title_line(app, app.home(), width), width);
    y += 1;

    if let Some(banner) = status::banner_line(app) {
        buffer.set_line(area.x, y, &banner, width);
        y += 1;
    }

    let columns = flock::columns_for(width);
    buffer.set_line(area.x, y, &flock::header_line(columns, width, palette.muted()), width);
    y += 1;
    buffer.set_line(area.x, y, &status::rule_line(palette.muted(), width), width);
    y += 1;

    // Everything from here to the line above the status bar.
    let viewport = usize::from(area.y + height - 1 - y);
    let rows = app.rows();
    if rows.is_empty() {
        let line = Line::from(Span::styled("the flock is empty", palette.muted()));
        buffer.set_line(area.x, y, &line, width);
    } else {
        let offset = flock::scroll_offset(app.scroll(), viewport, rows.len());
        for (slot, row) in rows.iter().skip(offset).take(viewport).enumerate() {
            let line = flock::row_line(app, row, columns, width);
            let slot = u16::try_from(slot).unwrap_or(0);
            buffer.set_line(area.x, y + slot, &line, width);
        }
    }

    buffer.set_line(area.x, area.y + height - 1, &status::status_line(app, width), width);
}
```

`crates/shep-cli/src/lookout/mod.rs` grows `pub mod view;`.

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+10**.

> `view/mod.rs`'s tests call `crate::lookout::frames::render_text`, which Task 5
> creates. Land Task 5's `frames.rs` first, or stub `render_text` here and let
> Task 5 replace the stub — either way, the two tasks touch the same two
> functions and should be done by the same implementer, in this order.

### Step 4.5 — MUTATION

In `flock.rs`, change `TIERS`' floor entry from `(MIN_WIDTH, FLOOR)` to
`(MIN_WIDTH, NO_CPU)` — i.e. keep UPTIME at the narrowest tier.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `every_tier_fits_the_width_it_claims` fails at width 31 —
`4 + 15 + 8` fixed plus `6` of separators plus `NAME_MIN` needs 41 columns and
was handed 31. `columns_drop_in_a_fixed_order_as_the_terminal_narrows` also
fails, on its `columns_for(31)` equality. Both reddening is correct here and is
not duplication: one is asserting the ORDER, the other the ARITHMETIC, and this
mutation breaks both properties at once.

Revert.

### Step 4.6 — second MUTATION

In `fit`, replace `text.chars().count()` with `text.len()`.

**Must go red:** `fit_counts_characters_not_bytes_when_it_pads_and_when_it_truncates`
fails on its FIRST assertion — `日本語` measures 9 bytes against a width of 6,
so it takes the truncate branch instead of the pad branch and comes out
`日本語…` where the column wanted `日本語   `. The second assertion fails too,
for the same reason at the exactly-fits boundary.
`a_name_too_long_for_its_column_ends_in_an_ellipsis` stays green, because every
name in it is ASCII — which is exactly why it could never have caught this on
its own.

### Step 4.7 — third MUTATION

In `draw`'s too-small branch, replace the two short lines with the single
sentence `terminal too small — lookout needs {MIN_WIDTH}x{MIN_HEIGHT}` written
with one `set_line`.

**Must go red:** `a_terminal_below_the_floor_says_so_instead_of_drawing` fails
on its first `assert_eq!` — at 28 columns the line reads
`terminal too small — lookout`, and `31x6` sat at columns 35-38 and was
truncated away by `set_line`. This is the mutation that pins the property the
message exists for: not "a refusal is printed" but "a refusal that fits".

Revert, then run the full task gate.

---

## Task 5 — `lookout/frames.rs`: the scenes, the snapshots, and the gallery

**This is the task that makes the phase worth its own milestone.** Its output
is not test infrastructure — it is a file of rendered frames Rin reads before
Phase 12b's layout is decided.

**Files created:**
- `crates/shep-cli/src/lookout/frames.rs`
- `crates/shep-cli/src/lookout/snapshots/` (insta, eight `.snap` files)
- `docs/lookout/frames.txt`, `docs/lookout/frames.ansi`, `docs/lookout/README.md`

**Files modified:**
- `crates/shep-cli/src/lookout/mod.rs` — `#[cfg(test)] pub mod frames;`

**`#[cfg(test)]`, not a plain `pub mod`.** `shep-cli` is `[[bin]]`-only, so
`pub` exempts nothing from `dead_code`: in a binary crate the only reachability
root is `main`, which is why `output::Streams::out`, `ExitCode::Success`,
`output::table::render_table` and half a dozen other `pub` items in this crate
already carry `#[cfg_attr(windows, allow(dead_code))]` or a bare
`#[allow(dead_code)]` with a comment saying "not called outside this module's
own tests yet". Every item in `frames.rs` is in exactly that position —
`render_text`, `render_ansi`, `Scene`, `Scene::ALL`, `label`, `caption`, `size`,
`scene` and the gallery writer have no non-test caller at all — so under
`cargo clippy --workspace --all-targets --all-features -- -D warnings` the
plain form fails the task gate on eight `dead_code` warnings. `view`'s tests and
the gallery writer are both `cfg(test)`, so gating the module loses nothing.

### Step 5.1 — baseline

```bash
find crates -name '*.snap' | wc -l                       # 4
find docs -maxdepth 1 -type d -name lookout | wc -l      # 0
grep -rn '#\[ignore' crates/ | wc -l                     # 14
```

### The eight scenes

One `const`-shaped list, used by BOTH the snapshot tests and the gallery
writer, so the gallery cannot rot: a layout change reddens the insta snapshots
in the ordinary suite, and regenerating the gallery is the same command.

| scene | size | shows |
|---|---|---|
| `healthy_wide` | 120x20 | a healthy flock at a comfortable width — all nine columns |
| `errored` | 120x20 | one sheep `errored`, one `waiting-restart`, one `stopped` — the colour rule |
| `empty` | 100x12 | an empty flock: header row, and a sentence saying it is empty |
| `narrow` | 49x14 | what degrades, and how — FOLD, RESTARTS, PID and MEM dropped; CPU and UPTIME survive |
| `too_narrow` | 28x8 | below the floor: the one-line refusal |
| `retrying` | 120x20 | the reconnect banner, mid-ladder |
| `frozen` | 120x20 | **the shepherd has died** — last values, frozen clock |
| `refused` | 120x20 | the read-only refusal in the status bar |

`healthy_wide`, `narrow`, `errored`, `empty` and `frozen` are the five Rin named
as the minimum. The other three are cheap next to them and each answers a
question the five raise.

### Step 5.2 — RED

`crates/shep-cli/src/lookout/frames.rs`, test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the plain renderer starts carrying escape bytes, or stops
    /// producing one line per buffer row. This is the renderer the view's own
    /// assertions read through, so a change here silently changes what nine
    /// other tests are asserting on.
    #[test]
    fn the_plain_renderer_is_one_line_per_row_and_no_escapes() {
        let text = render_text(&scene(Scene::HealthyWide).1);
        assert_eq!(text.lines().count(), 20);
        assert!(!text.contains('\u{1b}'), "plain means plain");
        for line in text.lines() {
            assert_eq!(line.chars().count(), 120, "every row is the full width");
        }
    }

    /// fails if the ANSI renderer stops emitting colour, or stops resetting.
    /// A frame that sets a colour and never resets it bleeds into whatever the
    /// operator's terminal prints next — which for a file read through
    /// `less -R` is the rest of the file.
    #[test]
    fn the_ansi_renderer_colours_the_errored_row_and_always_resets() {
        let ansi = render_ansi(&scene(Scene::Errored).1);
        assert!(ansi.contains("\u{1b}[38;5;166m"), "bark, on the errored status");
        for line in ansi.lines() {
            assert!(
                line.is_empty() || line.ends_with("\u{1b}[0m"),
                "every line resets before its newline"
            );
        }
    }

    /// fails if a scene stops rendering what it is named for. Each assertion
    /// is the one sentence that scene exists to show Rin — if one of these
    /// stops being true, the frame she is looking at is not the frame this
    /// plan promised her.
    #[test]
    fn every_scene_shows_the_thing_it_is_named_for() {
        assert!(render_text(&scene(Scene::Empty).1).contains("the flock is empty"));
        assert!(render_text(&scene(Scene::TooNarrow).1).contains("need 31x6"));
        assert!(render_text(&scene(Scene::Frozen).1).contains("the shepherd has died"));
        assert!(render_text(&scene(Scene::Retrying).1).contains("reconnecting"));
        assert!(render_text(&scene(Scene::Refused).1).contains("--allow-control"));
        assert!(render_text(&scene(Scene::Errored).1).contains("errored"));
        assert!(render_text(&scene(Scene::HealthyWide).1).contains("FOLD"));

        // The narrow scene's caption in the gallery makes four specific
        // claims about which columns survive at this width. Each is asserted
        // here, so a scene rendered at a width that contradicts its own
        // caption reddens the suite rather than shipping to Rin — `STATUS`
        // alone would not, since STATUS is in the floor tier and present at
        // every width the pane draws at.
        let narrow = render_text(&scene(Scene::Narrow).1);
        assert!(narrow.contains("CPU"), "CPU survives the narrow tier");
        assert!(narrow.contains("UPTIME"), "and so does UPTIME");
        for gone in ["FOLD", "RESTARTS", "PID", "MEM"] {
            assert!(!narrow.contains(gone), "the narrow tier dropped {gone}");
        }
    }

    /// fails if a frozen frame keeps counting. This is the one thing the
    /// frozen scene exists to show Rin, and it is the property design
    /// decision 8 is about.
    ///
    /// **Rendered twice at two different clock ages and compared**, rather
    /// than compared against the healthy scene: those two frames differ by a
    /// banner line, five statuses and a row shift, so an `assert_ne!` between
    /// them holds whether or not the clock stopped and cannot detect the
    /// regression its name claims. The live pair at the bottom is what keeps
    /// the frozen pair honest — without it, a `render_text` that emitted no
    /// UPTIME column at all would satisfy the first assertion perfectly.
    #[test]
    fn the_frozen_frame_does_not_move_however_long_the_link_stays_gone() {
        let ten_minutes = render_text(&scene_with(Scene::Frozen, Duration::from_secs(600)));
        let sixteen_hours = render_text(&scene_with(Scene::Frozen, Duration::from_secs(60_000)));
        assert_eq!(
            ten_minutes, sixteen_hours,
            "the frozen frame's uptime column advanced after the link was lost"
        );

        let live_ten = render_text(&scene_with(Scene::HealthyWide, Duration::from_secs(600)));
        let live_sixteen = render_text(&scene_with(Scene::HealthyWide, Duration::from_secs(60_000)));
        assert_ne!(
            live_ten, live_sixteen,
            "a LIVE frame's uptime column must advance, or the assertion above passes for the wrong reason"
        );
    }

    /// The frame pins. NOT wire fixtures: re-accepting these after a
    /// deliberate layout change is correct and expected, which is the opposite
    /// of IR-35's rule for the protocol snapshots in shep-core. Nobody may
    /// apply wire discipline to a border glyph.
    #[test]
    fn frames_are_pinned() {
        for which in Scene::ALL {
            let (label, buffer) = scene(*which);
            insta::assert_snapshot!(label, render_text(&buffer));
        }
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
function `render_text` in this scope``.

### Step 5.3 — GREEN

`crates/shep-cli/src/lookout/frames.rs`:

```rust
//! Rendering a drawn [`Buffer`] back out as text — plain, and with colour —
//! plus the scene list both the frame snapshots and the gallery are built
//! from.
//!
//! **Why this exists at all.** `TestBackend` renders a frame into a plain text
//! buffer with no terminal involved, which is what makes a TUI testable
//! headlessly. It is also, for exactly the same reason, what lets a reviewer
//! SEE the dashboard without running it — so this module's output is a
//! deliverable (`docs/lookout/frames.txt`, `docs/lookout/frames.ansi`) and not
//! only test scaffolding.
//!
//! **Why not `TestBackend`'s own `Display`.** Two reasons, both practical: its
//! exact framing is an upstream presentation detail that can change between
//! ratatui releases, and it carries no colour, while one of the two outputs
//! here has to.
//!
//! **Why one scene list.** The gallery and the snapshot tests read the same
//! [`Scene::ALL`], so the gallery cannot silently drift from what the suite
//! checks: a layout change reddens the snapshots in the ordinary run, and
//! regenerating the gallery is one command.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

use shep_core::protocol::ProcessInfo;
use shep_core::status::ProcStatus;

use super::app::{App, Control, KeyPress, Msg};
use super::theme::Palette;
use super::view::draw;

/// One rendered buffer as plain text: one line per row, trailing spaces kept,
/// no escapes.
///
/// Trailing spaces are kept on purpose. A frame is a fixed-size grid, and
/// trimming makes a right-aligned cell — the flock count in the title, the
/// control state in the status bar — look as though it moved.
#[must_use]
pub fn render_text(buffer: &Buffer) -> String {
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

/// The same buffer with SGR escapes, for reading through `less -R`.
///
/// Every line ends with a reset before its newline. A frame that set a colour
/// and never reset it would bleed into whatever came next — which, in a file,
/// is the rest of the file.
#[must_use]
pub fn render_ansi(buffer: &Buffer) -> String {
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let mut current = String::new();
        for x in area.left()..area.right() {
            let Some(cell) = buffer.cell((x, y)) else {
                continue;
            };
            let wanted = sgr(cell.fg);
            if wanted != current {
                out.push_str("\u{1b}[0m");
                out.push_str(&wanted);
                current = wanted;
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\u{1b}[0m");
        out.push('\n');
    }
    out
}

/// The SGR sequence for one cell's foreground.
///
/// Foreground only, because a foreground is the only thing 12a's palette sets
/// — there is no selected row and nothing is bold. A modifier a 12b pane
/// introduces renders unstyled here rather than as a wrong style, and this
/// function grows a case for it then.
fn sgr(fg: Color) -> String {
    let mut out = String::new();
    match fg {
        Color::Reset => {}
        Color::Indexed(index) => {
            let _ = write!(out, "\u{1b}[38;5;{index}m");
        }
        Color::Red => out.push_str("\u{1b}[31m"),
        Color::Green => out.push_str("\u{1b}[32m"),
        Color::Yellow => out.push_str("\u{1b}[33m"),
        Color::DarkGray => out.push_str("\u{1b}[90m"),
        _ => {}
    }
    out
}

/// The scenes the frame snapshots pin and the gallery renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// A healthy flock at a comfortable width.
    HealthyWide,
    /// One sheep errored, one waiting to restart, one stopped.
    Errored,
    /// Nothing registered.
    Empty,
    /// A narrow terminal: four columns dropped.
    Narrow,
    /// Below the floor.
    TooNarrow,
    /// Mid-reconnect.
    Retrying,
    /// The shepherd is gone and the values are frozen.
    Frozen,
    /// The read-only refusal.
    Refused,
}

impl Scene {
    /// Every scene, in the order they appear in the gallery.
    pub const ALL: &'static [Self] = &[
        Self::HealthyWide,
        Self::Errored,
        Self::Empty,
        Self::Narrow,
        Self::TooNarrow,
        Self::Retrying,
        Self::Frozen,
        Self::Refused,
    ];

    /// The snapshot name and the gallery heading.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::HealthyWide => "healthy_wide",
            Self::Errored => "errored",
            Self::Empty => "empty",
            Self::Narrow => "narrow",
            Self::TooNarrow => "too_narrow",
            Self::Retrying => "retrying",
            Self::Frozen => "frozen",
            Self::Refused => "refused",
        }
    }

    /// One sentence saying what this frame is for, printed above it in the
    /// gallery so Rin does not have to hold eight of them in her head.
    #[must_use]
    pub const fn caption(self) -> &'static str {
        match self {
            Self::HealthyWide => "A healthy flock at 120 columns: all nine columns fit.",
            Self::Errored => "One errored, one waiting to restart, one stopped. Colour is on the STATUS word and nowhere else.",
            Self::Empty => "No sheep registered. The header row still prints, and a sentence says why the pane is empty.",
            Self::Narrow => "49 columns: FOLD, RESTARTS, PID and MEM are gone, in that order. CPU and UPTIME survive because they explain WHY.",
            Self::TooNarrow => "28 columns: below the floor, the pane refuses rather than drawing overlapping garbage. The refusal is two short lines so it still fits.",
            Self::Retrying => "The shepherd stopped answering. Five attempts over about eight seconds before this becomes the next frame.",
            Self::Frozen => "The ladder ran out. Last known values stay; the uptime clock has stopped; lookout does not exit.",
            Self::Refused => "`x` with actions gated off. Both refusals are literal — nothing about damage gets charming.",
        }
    }

    /// The terminal size this scene is rendered at.
    #[must_use]
    pub const fn size(self) -> (u16, u16) {
        match self {
            Self::Empty => (100, 12),
            // 49, not 46. `columns_for` picks the first tier whose threshold
            // is <= the width, and 46 lands on the `41` tier — which has
            // already dropped CPU, so a scene rendered there would contradict
            // its own caption in the gallery Rin reads. 49 is the `NO_MEM`
            // tier: four columns gone, CPU and UPTIME still there.
            Self::Narrow => (49, 14),
            Self::TooNarrow => (28, 8),
            _ => (120, 20),
        }
    }
}

/// Builds one scene and returns its label with the buffer it drew into.
///
/// Ten minutes of dashboard age: long enough that a frozen frame whose clock
/// had kept running would be obvious in the gallery at a glance, and it is
/// what both the pinned snapshots and `docs/lookout/frames.txt` render at.
#[must_use]
pub fn scene(which: Scene) -> (&'static str, Buffer) {
    (which.label(), scene_with(which, Duration::from_secs(600)))
}

/// One scene, `age` after its opening snapshot.
///
/// The parameter exists for one test:
/// `the_frozen_frame_does_not_move_however_long_the_link_stays_gone` renders
/// the frozen scene at two ages and asserts the two frames are identical,
/// which is the only shape in which "the clock stopped" is a falsifiable
/// claim about a whole frame.
///
/// Deterministic by construction: the palette is forced to the 256-colour set
/// regardless of this machine's `TERM`, the clock is an explicit `Instant`
/// advanced by exact `Duration`s, and the frozen timestamp is a literal. A
/// scene that read the environment or the wall clock would produce a different
/// gallery on every machine and a snapshot that could never be pinned.
#[must_use]
fn scene_with(which: Scene, age: Duration) -> Buffer {
    use std::ffi::OsStr;

    let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
    let t0 = Instant::now();
    let mut app = App::new(
        palette,
        Control::ReadOnly,
        "/home/rin/.shep".to_string(),
        t0,
    );

    let flock = match which {
        Scene::Empty => Vec::new(),
        Scene::Errored | Scene::Frozen => vec![
            sheep(0, "web", ProcStatus::Online, Some(48_211), 0, Some(3.4), Some(182 << 20), Some("edge")),
            sheep(1, "web", ProcStatus::Online, Some(48_212), 0, Some(2.9), Some(178 << 20), Some("edge")),
            sheep(2, "api", ProcStatus::Errored, None, 14, None, None, Some("edge")),
            sheep(3, "billing-reconciliation-worker", ProcStatus::WaitingRestart, None, 3, None, None, None),
            sheep(4, "cron", ProcStatus::Stopped, None, 0, None, None, None),
            sheep(5, "metrics", ProcStatus::Online, Some(48_240), 0, Some(0.4), Some(11 << 20), None),
        ],
        _ => vec![
            sheep(0, "web", ProcStatus::Online, Some(48_211), 0, Some(3.4), Some(182 << 20), Some("edge")),
            sheep(1, "web", ProcStatus::Online, Some(48_212), 0, Some(2.9), Some(178 << 20), Some("edge")),
            sheep(2, "api", ProcStatus::Online, Some(48_219), 1, Some(7.1), Some(241 << 20), Some("edge")),
            sheep(3, "billing-reconciliation-worker", ProcStatus::Online, Some(48_230), 0, Some(0.8), Some(96 << 20), None),
            sheep(4, "cron", ProcStatus::Online, Some(48_233), 0, Some(0.1), Some(8 << 20), None),
            sheep(5, "metrics", ProcStatus::Online, Some(48_240), 0, Some(0.4), Some(11 << 20), None),
        ],
    };
    app.update(Msg::Snapshot { rows: flock, at: t0 });
    app.update(Msg::Tick {
        now: t0 + Duration::from_secs(7),
    });

    match which {
        Scene::Retrying => {
            app.update(Msg::Retrying { attempt: 3 });
        }
        Scene::Frozen => {
            app.update(Msg::Frozen {
                at_local: "2026-08-14 14:32:07".to_string(),
            });
        }
        Scene::Refused => {
            app.update(Msg::Key(KeyPress::Stop));
        }
        _ => {}
    }

    // The last tick, `age` after the opening snapshot. For every live scene
    // this is what advances the UPTIME column; for the frozen one it must
    // change nothing at all, because the reducer stopped accepting `now` when
    // the link was lost. That asymmetry is the whole of design decision 8, and
    // rendering the same scene at two ages is how it becomes testable.
    app.update(Msg::Tick { now: t0 + age });

    let (width, height) = which.size();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(&app, frame)).unwrap();
    terminal.backend().buffer().clone()
}

/// One row's worth of shepherd reply, spelled out so each scene reads as a
/// plausible flock rather than as six copies of one sheep.
#[allow(clippy::too_many_arguments)]
fn sheep(
    id: u32,
    name: &str,
    status: ProcStatus,
    pid: Option<u32>,
    restarts: u32,
    cpu: Option<f32>,
    memory: Option<u64>,
    fold: Option<&str>,
) -> ProcessInfo {
    ProcessInfo::builder(id, name, status)
        .pid(pid)
        .restarts(restarts)
        .uptime_ms(4_512_000 + u64::from(id) * 91_000)
        .cpu_percent(cpu)
        .memory_bytes(memory)
        .fold(fold.map(str::to_string))
        .build()
}
```

Run `cargo test -p shep-cli --bins --all-features`, then `cargo insta accept`
after **reading every one of the eight frames**. Do not accept a snapshot you
have not looked at — these eight files are the phase's deliverable, and an
accepted-but-unread frame defeats the whole point of the task.

Expect green, **+5**, and:

```bash
find crates -name '*.snap' | wc -l                                   # was 4, now 12
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l    # 8
```

### Step 5.4 — GREEN: the gallery writer

Append to `frames.rs` (outside the test module, so the constant is documented
like any other item — the module itself is already `#[cfg(test)]`, so no
per-item gate is needed):

```rust
/// The header both gallery files open with.
///
/// Not a doc comment on the test: this text is read by a person opening
/// `docs/lookout/frames.txt` with no context at all, and it is the only place
/// that says where those frames came from.
const GALLERY_PREAMBLE: &str = "shep lookout — Phase 12a frames
================================

These are real frames, rendered headlessly through ratatui's TestBackend by

    cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery

Nothing here is a mockup.

frames.ansi is the same eight frames with colour; read it with `less -R`.

They are here to be looked at BEFORE Phase 12b's layout is decided. 12a builds
the shell and one pane on purpose — the bleats feed, the sheep detail pane and
the host-usage strip are 12b, and how those three sit beside this one is the
decision these frames exist to inform.
";
```

and, inside the test module:

```rust
    /// Writes `docs/lookout/frames.txt` and `docs/lookout/frames.ansi`.
    ///
    /// `#[ignore]` because it writes into the repository, which no ordinary
    /// test run may do. Run it deliberately:
    ///
    /// ```text
    /// cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery
    /// ```
    ///
    /// This is the ONE ignored test this phase adds — the `ignored` count in
    /// the workspace summary goes 3 -> 4, exactly once.
    ///
    /// It cannot rot: every frame it writes is `render_text`/`render_ansi` over
    /// the same `Scene::ALL` the pinned snapshots above read, so a layout
    /// change reddens the ordinary suite first.
    #[test]
    #[ignore = "writes into docs/lookout; run it deliberately"]
    fn write_the_gallery() {
        // Absolute, derived from the manifest — so it lands in the same place
        // whatever directory the run started in.
        let dir =
            std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/lookout"));
        std::fs::create_dir_all(dir).unwrap();

        let mut plain = String::from(GALLERY_PREAMBLE);
        let mut ansi = String::from(GALLERY_PREAMBLE);
        for which in Scene::ALL {
            let (label, buffer) = scene(*which);
            let (width, height) = which.size();
            let heading = format!("\n\n=== {label}  ({width}x{height}) ===\n{}\n\n", which.caption());
            plain.push_str(&heading);
            plain.push_str(&render_text(&buffer));
            ansi.push_str(&heading);
            ansi.push_str(&render_ansi(&buffer));
        }
        std::fs::write(dir.join("frames.txt"), &plain).unwrap();
        std::fs::write(dir.join("frames.ansi"), &ansi).unwrap();

        // A live assertion, not a `timeout`: this function is synchronous, so a
        // `tokio::time::timeout` around it would complete on its first poll and
        // bound nothing at all. What can actually go wrong here is a scene
        // rendering empty, and that is what these two check.
        assert!(
            plain.lines().count() > 100,
            "eight frames is more than a hundred lines"
        );
        assert_eq!(plain.matches("=== ").count(), Scene::ALL.len());
    }
```

Run it:

```bash
cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery
```

Then verify. Every one of these prints `0` (or errors on a missing path) at
HEAD, so each can fail:

```bash
find docs/lookout -name 'frames.*' | wc -l                  # 2
grep -c '^=== ' docs/lookout/frames.txt                     # 8
grep -c 'the shepherd has died' docs/lookout/frames.txt     # 1
grep -c '38;5;166' docs/lookout/frames.ansi                 # at least 1
grep -c '38;5;166' docs/lookout/frames.txt                  # 0 — plain stays plain
```

The last two are a pair on purpose: the first alone would pass if both files
were written with the ANSI renderer.

### Step 5.5 — GREEN: `docs/lookout/README.md`

Write it with this content — it is what a reader lands on:

- what `shep lookout` is, and that 12a built the shell plus one pane
- how to read `frames.txt` / `frames.ansi`, and the one command that
  regenerates both
- **What 12a settled:** the retry-then-freeze rule and its numbers, and that a
  shepherd which was never running is refused like every other verb rather than
  frozen about; the control gate and the sentence that it is a fat-finger catch
  and not a security boundary; the rule that colour is always redundant with
  text; the fixed column-drop order; and that the keyboard scrolls the viewport
  but does not select a row — there is no cursor until the pane that reads one
  exists
- **What is still open for 12b:** where the other three panes sit and which are
  focusable; whether the flock table grows a selected row and what marks it;
  which actions the gate lets through and what confirms them; whether filter
  takes the CLI selector grammar or plain substring

### Step 5.6 — MUTATION

In `render_ansi`, delete the `out.push_str("\u{1b}[0m");` immediately before
`out.push('\n')`.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `the_ansi_renderer_colours_the_errored_row_and_always_resets`
fails on the per-line reset assertion — a frame that leaves a colour set at the
end of a line bleeds it into everything below it in the file. Revert.

### Step 5.7 — second MUTATION

In `Scene::size`, change `Self::Narrow` from `(49, 14)` to `(120, 14)`.

**Must go red:** `every_scene_shows_the_thing_it_is_named_for` fails on the
first `!contains(gone)` iteration — the "narrow" scene is now wide and shows
every column, so the gallery frame Rin was promised would show nothing about
degradation. `frames_are_pinned` reddens too, which is correct rather than
duplicative: the snapshot *is* the frame, and this mutation changes the frame.

Revert.

### Step 5.8 — third MUTATION

In `Scene::size`, change `Self::Narrow` from `(49, 14)` to `(46, 14)` — the
width an earlier draft of this plan specified.

**Must go red:** the same `!contains(gone)` loop passes, but
`assert!(narrow.contains("CPU"))` fails: `columns_for(46)` picks the first tier
whose threshold is `<= 46`, which is the `41` tier, and CPU is already gone
there. Five columns dropped, not four. The point of this mutation is that the
old width was not a rendering bug — it produced a perfectly good frame with a
caption underneath it, in the gallery Rin reads, describing a different frame.

Revert, then run the full task gate.

---

## Task 6 — `lookout/source.rs` and `lookout/link.rs`: subscribe, poll, repair, freeze

The other half of the real engineering. This is bark's `run_loop` ported, plus
the reconnect ladder and the freeze that bark has no notion of.

**Files created:**
- `crates/shep-cli/src/lookout/source.rs`
- `crates/shep-cli/src/lookout/link.rs`

**Files modified:**
- `crates/shep-cli/src/lookout/mod.rs` — `pub mod link; pub mod source;`

**Consumes:** Task 3's `Msg`.

### Why the link runs in its own task, not in the UI loop's `select!`

Reading the bus needs `&mut` (an `EventStream` is a stream), and issuing a
`ListFlock` needs the `Client`. Holding both in one `tokio::select!` over one
object is a borrow conflict — which is exactly why bark's `run_loop` takes two
separate values, `events: E` by value and `flock: &F` by shared reference.

lookout has an obligation bark does not: it must **rebuild both halves at once**
on a reconnect. A design that kept them as two independent parameters would let
a test — and eventually the real code — pair a fresh flock source with a stale
event stream, a state the real connection cannot be in.

So the connection lives in its own task, behind one factory trait that hands
back both halves together, and it talks to the UI over two channels: `Msg`s out,
poll requests in. The UI loop then selects over terminal input, that `Msg`
channel, a heartbeat and the redraw gate — none of which borrow each other.

### Where the first listing happens, decided once

**`run_connected` opens with a `reconcile`, and every count in its tests is
written against that.** This is stated here because the alternative is
defensible and the two are indistinguishable at a glance: bark's `run_loop`
(`crates/shep-cli/src/dog/bark/mod.rs:183`) starts straight at its `select!`
with no opening listing, these tests were first drafted from it, and every
count-based assertion in them was therefore off by one against the
implementation on the same page — including the one that pins the drop repair,
which passed under the mutation that was supposed to redden it.

The opening `reconcile` stays, and it belongs to the *connection* rather than
to the ladder, because a reconnect needs the same first listing a cold start
does and there is exactly one place to put it that gets both. So: **every
connection begins with one listing, and the poll counters below count it.**
A test asserting "one poll" against this loop is asserting the opening listing
happened and nothing else did.

### Where the FIRST connection is opened, decided once

`run_link` does not dial the first connection: it is **handed** one, already
open, and only dials again after that one ends. The opening dial is
`lookout::lookout`'s, before raw mode, so a shepherd that was never running
produces the ordinary `daemon_unreachable` refusal every other client verb
produces rather than eight seconds of alternate-screen theatre ending in
`ExitCode::Success` — design decision 1's second half. It also removes the
`opened_once` flag an earlier draft carried, and the bug in it: `opened_once`
was set only by a *successful* dial, so "first dial fails, second succeeds"
never announced [`Msg::Relinked`] and left a fully live dashboard showing
`reconnecting (attempt 1)` forever. The condition below is `attempt > 0` —
gated on whether a `Retrying` was **announced**, which is the thing the banner
is showing.

### Step 6.1 — RED

`crates/shep-cli/src/lookout/link.rs`, test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use shep_core::protocol::{ProcessEventKind, ProcessInfo};
    use shep_core::status::ProcStatus;
    use tokio::sync::broadcast;

    // Named only from here: `run_link`'s own error arm never spells the type.
    use crate::lookout::source::LinkError;

    fn sheep(id: u32) -> ProcessInfo {
        ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
    }

    /// A flock source that counts what it was asked, so a test can assert on
    /// WHY a poll happened rather than only that one did.
    struct CountingFlock {
        polls: Arc<AtomicU64>,
    }

    impl FlockSource for CountingFlock {
        async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![sheep(1)])
        }
    }

    /// A REAL `broadcast::Receiver` with a tiny capacity, so the bus genuinely
    /// drops frames. IR-33: hand-rolled, no mock crate. A fake that delivered
    /// everything would prove the fast path, which was never the risk.
    struct BroadcastEvents(broadcast::Receiver<BusEvent>);

    impl EventSource for BroadcastEvents {
        async fn next_event(&mut self) -> Option<Result<BusEvent, Lagged>> {
            match self.0.recv().await {
                Ok(event) => Some(Ok(event)),
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    Some(Err(Lagged { count }))
                }
                Err(broadcast::error::RecvError::Closed) => None,
            }
        }
    }

    /// fails if a lagging subscriber stops triggering an immediate poll. This
    /// is the property the whole two-route design exists for: `broadcast`
    /// DROPS for a subscriber that falls behind, so a dashboard that only
    /// subscribed would go silently wrong under exactly the load that makes it
    /// worth watching. Bark's own reconciliation test is built the same way and
    /// for the same reason.
    ///
    /// IR-46: the wait on the message channel is bounded — a link task that
    /// never polls would otherwise hang this test rather than fail it.
    #[tokio::test(start_paused = true)]
    async fn a_lagging_subscriber_polls_immediately_instead_of_waiting() {
        let (tx, rx) = broadcast::channel(2);
        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);

        // Overrun the capacity-2 channel before the loop ever reads it.
        for id in 0..8 {
            let _ = tx.send(BusEvent::Process {
                event: ProcessEventKind::Online,
                info: sheep(id),
                manually: false,
                at_ms: 0,
            });
        }

        let flock = CountingFlock {
            polls: Arc::clone(&polls),
        };
        let task = tokio::spawn(run_connected(
            flock,
            BroadcastEvents(rx),
            msg_tx,
            poll_rx,
            // A poll period far longer than this test, so ANY poll beyond the
            // opening listing is attributable to the lag and to nothing else.
            Duration::from_secs(3600),
        ));

        // The repair is what this test is about, so it waits for the SNAPSHOT
        // the repair produces rather than for the `BusLagged` notice that
        // precedes it: `run_connected` forwards the notice first and polls
        // second, so a test that stopped at the notice would read the counter
        // in a race with the poll it is counting.
        let mut saw_lagged = false;
        let mut repaired = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(saw_lagged && repaired) {
            let Ok(Some(msg)) = tokio::time::timeout_at(deadline, msg_rx.recv()).await else {
                break;
            };
            match msg {
                Msg::BusLagged { .. } => saw_lagged = true,
                Msg::Snapshot { .. } if saw_lagged => repaired = true,
                _ => {}
            }
        }
        task.abort();

        assert!(saw_lagged, "the lag reached the reducer");
        assert!(repaired, "the lag was repaired by a listing rather than left to the interval");
        assert_eq!(
            polls.load(Ordering::SeqCst),
            2,
            "the opening listing, plus exactly one repair for the lag; the one-hour interval caused none"
        );
    }

    /// fails if a shepherd-side drop stops triggering a repair. `Dropped` is a
    /// real, named `BusEvent` this binary understands — it must NOT fall into
    /// the catch-all arm for variants a newer shepherd added.
    #[tokio::test(start_paused = true)]
    async fn a_shepherd_side_drop_polls_and_is_forwarded() {
        let (tx, rx) = broadcast::channel(16);
        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let _ = tx.send(BusEvent::Dropped { count: 9 });

        let task = tokio::spawn(run_connected(
            CountingFlock {
                polls: Arc::clone(&polls),
            },
            BroadcastEvents(rx),
            msg_tx,
            poll_rx,
            Duration::from_secs(3600),
        ));

        // The FIRST message on this channel is the opening listing's snapshot,
        // not the drop — every connection begins with one listing. Scanning
        // for the drop rather than asserting on message one is the difference
        // between testing this loop and testing bark's, which has no opening
        // listing and is where these fixtures came from.
        let mut forwarded = false;
        let mut repaired = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(forwarded && repaired) {
            let Ok(Some(msg)) = tokio::time::timeout_at(deadline, msg_rx.recv()).await else {
                break;
            };
            match msg {
                Msg::Event(BusEvent::Dropped { count: 9 }) => forwarded = true,
                Msg::Snapshot { .. } if forwarded => repaired = true,
                _ => {}
            }
        }
        task.abort();

        assert!(forwarded, "the drop reached the reducer");
        assert!(repaired, "and it triggered a repair listing");
        assert_eq!(
            polls.load(Ordering::SeqCst),
            2,
            "the opening listing, plus one repair for the drop"
        );
    }

    /// fails if the ladder stops being bounded, or stops ending in a freeze.
    /// Rin's ruling in one test: bounded retry, then a message saying the
    /// shepherd has died, then nothing. Never an exit.
    ///
    /// `start_paused` so the 250/500/1000/2000/4000 ms waits cost no wall
    /// clock; the `timeout` bound is real, because `run_link` failing to ever
    /// give up is exactly the regression this catches (IR-46).
    #[tokio::test(start_paused = true)]
    async fn the_ladder_is_bounded_and_ends_frozen() {
        struct NeverConnects {
            attempts: Arc<AtomicU64>,
        }

        impl Shepherd for NeverConnects {
            type Flock = CountingFlock;
            type Events = BroadcastEvents;

            async fn link(&mut self) -> Result<(Self::Flock, Self::Events), LinkError> {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                Err(LinkError::Unreachable("nothing is listening".to_string()))
            }
        }

        let attempts = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);

        // `run_link` is HANDED its first connection rather than dialling for
        // it — the opening dial is `lookout::lookout`'s, before raw mode, so
        // that a shepherd which was never there refuses like every other verb
        // instead of freezing a dashboard about it. This opening connection
        // ends at once: its sender is dropped, so the subscription closes on
        // the first read and the ladder takes over.
        let (opening_tx, opening_rx) = broadcast::channel(1);
        drop(opening_tx);

        let task = tokio::spawn(run_link(
            NeverConnects {
                attempts: Arc::clone(&attempts),
            },
            (
                CountingFlock {
                    polls: Arc::new(AtomicU64::new(0)),
                },
                BroadcastEvents(opening_rx),
            ),
            msg_tx,
            poll_rx,
            Duration::from_secs(2),
        ));

        let mut seen = Vec::new();
        let done = tokio::time::timeout(Duration::from_secs(120), async {
            while let Some(msg) = msg_rx.recv().await {
                let frozen = matches!(msg, Msg::Frozen { .. });
                seen.push(msg);
                if frozen {
                    break;
                }
            }
        })
        .await;
        assert!(done.is_ok(), "the ladder gave up rather than retrying forever");

        // One immediate re-dial the moment the connection ended, then
        // RECONNECT_ATTEMPTS more behind the 250/500/1000/2000/4000 ms waits.
        // Only the delayed ones announce a `Retrying`, which is why the two
        // counts below differ by exactly one.
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            u64::from(RECONNECT_ATTEMPTS) + 1
        );
        let retries = seen
            .iter()
            .filter(|msg| matches!(msg, Msg::Retrying { .. }))
            .count();
        assert_eq!(retries, usize::try_from(RECONNECT_ATTEMPTS).unwrap());
        assert!(matches!(seen.last(), Some(Msg::Frozen { .. })));

        // And it ENDS. A link task still alive after a freeze would keep a
        // dead connection's machinery running behind a screen that says it is
        // gone.
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert!(ended.is_ok(), "the link task ended after freezing");
    }

    /// fails if a reconnect that succeeds does not re-subscribe and re-list.
    /// A relink that reconnected but kept the old (dead) subscription would
    /// leave the dashboard live-looking and permanently stale — worse than the
    /// freeze, because nothing says so.
    #[tokio::test(start_paused = true)]
    async fn a_successful_relink_reports_live_on_the_first_success() {
        struct FailsOnce {
            done: bool,
            polls: Arc<AtomicU64>,
            /// Holds the reconnected subscription's SENDER open.
            ///
            /// Not decoration. An earlier version of this fixture built the
            /// channel with `let (_tx, rx)` and dropped the sender on the
            /// spot, so the "successful" relink handed back a stream that
            /// ended on its first read; the ladder went round a second time,
            /// and the `Relinked` this test observed came from that second
            /// cycle. It passed while the first relink announced nothing at
            /// all — which was the actual bug, and the one this test claims
            /// to be about.
            keepalive: Option<broadcast::Sender<BusEvent>>,
        }

        impl Shepherd for FailsOnce {
            type Flock = CountingFlock;
            type Events = BroadcastEvents;

            async fn link(&mut self) -> Result<(Self::Flock, Self::Events), LinkError> {
                if self.done {
                    let (tx, rx) = broadcast::channel(16);
                    self.keepalive = Some(tx);
                    return Ok((
                        CountingFlock {
                            polls: Arc::clone(&self.polls),
                        },
                        BroadcastEvents(rx),
                    ));
                }
                self.done = true;
                Err(LinkError::Unreachable("not yet".to_string()))
            }
        }

        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);

        // The opening connection ends immediately, with its own counter, so
        // `polls` below counts only what the RECONNECTED one listed.
        let (opening_tx, opening_rx) = broadcast::channel(1);
        drop(opening_tx);

        let task = tokio::spawn(run_link(
            FailsOnce {
                done: false,
                polls: Arc::clone(&polls),
                keepalive: None,
            },
            (
                CountingFlock {
                    polls: Arc::new(AtomicU64::new(0)),
                },
                BroadcastEvents(opening_rx),
            ),
            msg_tx,
            poll_rx,
            Duration::from_secs(2),
        ));

        let mut retried = false;
        let mut relinked = false;
        let mut listed_after_relink = false;
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(msg) = msg_rx.recv().await {
                match msg {
                    Msg::Retrying { attempt: 1 } => retried = true,
                    Msg::Relinked => relinked = true,
                    Msg::Snapshot { .. } if relinked => {
                        listed_after_relink = true;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        task.abort();

        assert!(retried, "the failed dial put the banner up");
        assert!(relinked, "and the FIRST successful re-dial took it down again");
        assert!(listed_after_relink, "the fresh connection re-listed the flock");
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "exactly the reconnected connection's opening listing"
        );
    }

    /// fails if the scheduled poll stops firing on its own schedule, or starts
    /// firing immediately at startup. The first poll must always be
    /// attributable — to the opening listing, to a drop, or to the interval
    /// genuinely elapsing — never to `tokio::time::interval`'s own quirk of
    /// yielding its first tick at once. Bark's loop names the same trap.
    #[tokio::test(start_paused = true)]
    async fn the_scheduled_poll_lands_on_the_interval_and_not_at_zero() {
        let (tx, rx) = broadcast::channel(16);
        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, _msg_rx) = mpsc::channel(256);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let task = tokio::spawn(run_connected(
            CountingFlock {
                polls: Arc::clone(&polls),
            },
            BroadcastEvents(rx),
            msg_tx,
            poll_rx,
            Duration::from_secs(2),
        ));

        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "the opening listing, and NOTHING from the timer before its period elapsed"
        );
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(
            polls.load(Ordering::SeqCst),
            3,
            "the opening listing, plus t=2s and t=4s"
        );
        drop(tx);
        task.abort();
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
function `run_link` in this scope``.

### Step 6.2 — GREEN: `lookout/source.rs`

```rust
//! What the link task reads the shepherd through, and the real implementation
//! over `shep-client`.
//!
//! Three traits rather than one concrete type, so [`super::link::run_link`] is
//! drivable with no socket at all: the tests that matter here are about a bus
//! that genuinely drops frames and a shepherd that genuinely will not answer,
//! and neither is reachable through a real connection on demand.
//!
//! **Why two source traits and not one object.** Reading the bus needs `&mut`
//! and issuing a `ListFlock` needs a shared reference, and `tokio::select!`
//! cannot hold both against one value. `crate::dog::bark` split its own pair
//! for exactly this reason and this follows it.
//!
//! **Why they are declared here and not shared with bark.** The shapes look
//! alike; the meanings differ. Bark's `EventSource::next` yields
//! `Result<BusEvent, u64>` because a dog only needs the count; this one yields
//! `Result<BusEvent, `[`Lagged`]`>` because the status bar prints the notice
//! and has to distinguish it from the shepherd's own `BusEvent::Dropped`. A
//! shared home for two six-line traits, one of which would then be generic
//! over its error type to serve both callers, is a worse trade than the
//! duplication — the repetition here is of shape, not of meaning.

use core::fmt;
use core::future::Future;
use std::path::{Path, PathBuf};

use shep_client::{Client, ConnectError, EventStream, Lagged, RequestError};
use shep_core::protocol::{BusEvent, ProcessInfo, Request, Response};

use crate::exit::ExitCode;

/// The topics lookout subscribes to.
///
/// `process.*` is what the flock table is made of; `daemon.*` carries
/// `BusEvent::Dropped` and `BusEvent::DaemonShutdown`, both of which this
/// dashboard reports rather than ignores.
///
/// **Not `log.*`, deliberately.** The bleats feed is Phase 12b. Subscribing to
/// every line every sheep writes, in order to draw a pane that does not exist,
/// would make lookout the highest-volume subscriber on the bus for no visible
/// reason — and would manufacture the very `Dropped`/`Lagged` condition
/// [`super::link`] exists to survive.
pub const TOPICS: &[&str] = &["process.*", "daemon.*"];

/// Reading the flock. `&self`, so [`super::link::run_connected`] can hold it
/// across the same `select!` that holds an [`EventSource`] mutably.
pub trait FlockSource: Send + Sync {
    /// The flock as it stands.
    ///
    /// # Errors
    /// Whatever the underlying source could not answer with — for the real
    /// implementation, whatever `Request::ListFlock` failed with.
    fn flock(&self) -> impl Future<Output = Result<Vec<ProcessInfo>, RequestError>> + Send;
}

/// One source of bus frames.
pub trait EventSource: Send {
    /// The next frame; `Err(`[`Lagged`]`)` when this client's own receiver fell
    /// behind and discarded frames; `None` when the subscription ends, which
    /// is how a dead connection announces itself.
    fn next_event(&mut self) -> impl Future<Output = Option<Result<BusEvent, Lagged>>> + Send;
}

/// Opens a connection and hands back both halves of it together.
///
/// One factory rather than two independently-refreshable parameters: a
/// reconnect rebuilds the request path and the subscription at the same
/// moment, and a signature that let a caller replace one without the other
/// would admit a state the real connection cannot be in.
pub trait Shepherd: Send {
    /// This connection's request half.
    type Flock: FlockSource;
    /// This connection's subscription half.
    type Events: EventSource;

    /// Connects and subscribes.
    ///
    /// # Errors
    /// [`LinkError::Unreachable`] when the socket would not answer or the
    /// handshake failed, [`LinkError::Refused`] when it answered and then
    /// refused the subscription.
    fn link(&mut self) -> impl Future<Output = Result<(Self::Flock, Self::Events), LinkError>> + Send;
}

/// Why opening a connection failed.
///
/// No `#[non_exhaustive]`, and that is a decision rather than an oversight:
/// IR-20's obligation is on `pub` error enums in LIBRARY crates, and shep-cli
/// is `[[bin]]`-only — there is no downstream to break, and every match on this
/// type is in this crate. Stated here rather than left silent, which is the
/// half of IR-20 that applies either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// Nothing answered at the socket, or the handshake did not complete.
    Unreachable(String),
    /// The shepherd answered and speaks a different wire version.
    ///
    /// Held apart from [`Self::Unreachable`] for one reason: it is the single
    /// connect failure with its own exit code, and `main.rs`'s
    /// `connect_client` — the path every other client verb takes — already
    /// makes that distinction. A lookout that reported a version skew as
    /// "the shepherd did not answer" would send the operator to check whether
    /// the daemon is running, which it is.
    Protocol(String),
    /// The shepherd answered but refused the subscription.
    Refused(String),
}

impl LinkError {
    /// The exit code this reports when it happens on the FIRST dial, before
    /// the dashboard exists.
    ///
    /// Only the first dial reaches this: once a link has been established, a
    /// failure is a rung on [`super::link::run_link`]'s ladder and never an
    /// exit. Derived from `ExitCode::from(&ConnectError)` at conversion time
    /// rather than re-decided here, so this and every other verb's mapping
    /// cannot drift apart.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Protocol(_) => ExitCode::ProtocolMismatch,
            Self::Unreachable(_) | Self::Refused(_) => ExitCode::DaemonUnreachable,
        }
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "the shepherd did not answer: {why}"),
            Self::Protocol(why) => write!(f, "{why}"),
            Self::Refused(why) => write!(f, "the shepherd refused the subscription: {why}"),
        }
    }
}

impl core::error::Error for LinkError {}

impl From<ConnectError> for LinkError {
    fn from(err: ConnectError) -> Self {
        // `ExitCode::from(&ConnectError)` is the existing taxonomy — reused
        // rather than re-derived, so the two cannot skew.
        if ExitCode::from(&err) == ExitCode::ProtocolMismatch {
            return Self::Protocol(err.to_string());
        }
        Self::Unreachable(err.to_string())
    }
}

/// The request half of a live connection.
#[derive(Debug)]
pub struct ClientFlock(Client);

impl FlockSource for ClientFlock {
    async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
        match self.0.request(Request::ListFlock).await? {
            Response::Flock(flock) => Ok(flock),
            // `Response` is `#[non_exhaustive]`; a reply this binary does not
            // recognise is not a reason to tear the dashboard down, and the
            // next poll asks again.
            _unrecognised => Ok(Vec::new()),
        }
    }
}

impl EventSource for EventStream {
    async fn next_event(&mut self) -> Option<Result<BusEvent, Lagged>> {
        self.next().await
    }
}

/// The real thing: a socket path that can be dialled again.
#[derive(Debug)]
pub struct UnixShepherd {
    socket: PathBuf,
}

impl UnixShepherd {
    /// Watches the shepherd listening at `socket`.
    #[must_use]
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }
}

impl Shepherd for UnixShepherd {
    type Flock = ClientFlock;
    type Events = EventStream;

    async fn link(&mut self) -> Result<(Self::Flock, Self::Events), LinkError> {
        // `Client::connect`, never `connect_or_spawn`: opening a dashboard
        // must not start a shepherd, and a RECONNECT starting one would be
        // worse still — it would resurrect a supervisor the operator may have
        // just killed on purpose, from a process whose whole job is to watch.
        // `main.rs`'s own dispatch draws the same line for every verb but
        // `start` and `muster`.
        let client = Client::connect(&self.socket).await?;
        let topics = TOPICS.iter().map(|topic| (*topic).to_string()).collect();
        let stream = client
            .subscribe(topics)
            .await
            .map_err(|err| LinkError::Refused(err.to_string()))?;
        Ok((ClientFlock(client), stream))
    }
}
```

### Step 6.3 — GREEN: `lookout/link.rs`

```rust
//! The link task: subscribe for latency, poll for correctness, repair on a
//! drop, climb a bounded ladder on a disconnect, and freeze when it runs out.
//!
//! **The bus drops events, and that is what this module exists to survive.**
//! `tokio::sync::broadcast` discards what a lagging subscriber cannot keep up
//! with rather than queueing it — the shepherd surfaces that as
//! `BusEvent::Dropped`, and this process's own receiver can lag the same way
//! (`shep_client::Lagged`). A dashboard that only subscribed would miss exactly
//! the events load produces, which is exactly when a dashboard matters. The
//! answer is the one `crate::dog::bark::run_loop` already proved: subscribe AND
//! poll, and let a dropped or lagged frame trigger an immediate poll rather
//! than waiting for the scheduled one. The drop itself carries no information
//! about what was lost, so asking the shepherd what things look like now is the
//! only repair there is.
//!
//! **The freeze is Rin's ruling and it is not `bleats`' behaviour.** `bleats`
//! prints a notice and exits when its connection ends, which is right for a
//! follow. A standing dashboard that vanished would take the last known state
//! of the flock with it, at the moment an operator most wants to read it. So
//! this climbs [`RECONNECT_ATTEMPTS`] rungs and then sends [`Msg::Frozen`] and
//! ENDS — no more polls, no more dials, no more subscriptions. The UI loop
//! keeps running with the last values on screen until the operator quits.

use core::fmt;
use std::time::Duration;

use shep_client::{Lagged, RequestError};
use shep_core::protocol::BusEvent;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::app::Msg;
// `LinkError` is deliberately NOT imported: the error arm below never names
// the type, and an unused import fails `-D warnings`. The tests import it.
use super::source::{EventSource, FlockSource, Shepherd};

/// How often the flock is re-listed when nothing has gone wrong.
///
/// Two seconds. The pane's content already changes on a `process.*` event
/// within milliseconds; this exists to repair drift, not to animate. Two
/// seconds is inside an operator's own "is this thing live" patience while
/// being far cheaper than the per-frame polling a naive dashboard does. For
/// scale: the bark dog's fallback poll is 30s because nothing is watching it,
/// and the memory-limit sampler is 15s because it walks the process table — a
/// `ListFlock` is a map lookup and one frame each way.
pub const FLOCK_POLL: Duration = Duration::from_secs(2);

/// How many times the link re-dials before it gives up and freezes.
///
/// Five, at [`RECONNECT_FIRST_WAIT`] doubling to [`RECONNECT_MAX_WAIT`], is
/// 250 + 500 + 1000 + 2000 + 4000 ms — **7.75 seconds** of waiting. A shepherd
/// being restarted deliberately (`shep kill` then `shep muster`, or a systemd
/// restart) is back inside that window, so an operator watching through a
/// restart sees "reconnecting" and then recovery and never sees a freeze. A
/// shepherd that is genuinely gone is declared gone before that operator has
/// walked away from the terminal.
///
/// Thirty seconds would leave a dead dashboard claiming to be live for half a
/// minute. Two would flip to frozen during an ordinary restart and teach the
/// operator to distrust the banner.
pub const RECONNECT_ATTEMPTS: u32 = 5;

/// The wait before the first re-dial.
pub const RECONNECT_FIRST_WAIT: Duration = Duration::from_millis(250);

/// The ceiling on the doubling.
pub const RECONNECT_MAX_WAIT: Duration = Duration::from_secs(4);

/// The dashboard stopped listening: its [`Msg`] channel is closed.
///
/// The only condition [`run_connected`] reports that is not a reconnect —
/// every other failure it can meet is a rung on the ladder. A named type
/// rather than `()` for two reasons, one of them a gate: an exported
/// `Result<_, ()>` trips `clippy::result_unit_err`, which
/// `cargo clippy -- -D warnings` turns into a task-gate failure. The other is
/// that "the UI is gone" reads at a call site where `Err(())` does not.
///
/// No `#[non_exhaustive]`, for the same reason [`super::source::LinkError`]
/// carries none: IR-20's obligation is on `pub` error types in LIBRARY crates,
/// and shep-cli is `[[bin]]`-only. Said out loud rather than left silent,
/// which is the half of IR-20 that applies either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiGone;

impl fmt::Display for UiGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the dashboard stopped listening")
    }
}

impl core::error::Error for UiGone {}

/// Runs `opened` and everything that replaces it, until the ladder runs out.
///
/// Sends [`Msg`]s to the UI over `msgs`, and takes a request for an
/// out-of-band poll on `polls` (the `r` key, and the reducer's own
/// [`super::app::Effect::PollNow`]).
///
/// **`opened` is handed in, already connected.** The first dial belongs to
/// `super::lookout`, which makes it before entering raw mode so that a
/// shepherd which was never running refuses the way every other client verb
/// refuses — `daemon_unreachable`, exit 5, no alternate screen — instead of
/// eight seconds of reconnect banner about a death that never happened. Rin's
/// retry-then-freeze ruling is about a shepherd that dies *underneath* a
/// running dashboard, and this signature is where the distinction is enforced
/// rather than described.
///
/// Ends — and only ends — after sending [`Msg::Frozen`], or when the UI stops
/// listening. Everything else it can encounter is a rung on the ladder.
pub async fn run_link<S: Shepherd>(
    mut shepherd: S,
    opened: (S::Flock, S::Events),
    msgs: mpsc::Sender<Msg>,
    mut polls: mpsc::Receiver<()>,
    period: Duration,
) {
    let mut attempt = 0u32;
    let mut wait = RECONNECT_FIRST_WAIT;
    let mut connection = Some(opened);

    loop {
        let (flock, events) = match connection.take() {
            Some(pair) => pair,
            None => match shepherd.link().await {
                Ok(pair) => pair,
                Err(err) => {
                    // Not surfaced as its own Msg: the reducer's `Retrying`
                    // state is what the banner reads, and a per-attempt error
                    // string would put a different sentence on screen every
                    // 250ms during an ordinary restart.
                    let _ = err;
                    attempt += 1;
                    if attempt > RECONNECT_ATTEMPTS {
                        let _ = msgs
                            .send(Msg::Frozen {
                                at_local: local_now(),
                            })
                            .await;
                        return;
                    }
                    let _ = msgs.send(Msg::Retrying { attempt }).await;
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(RECONNECT_MAX_WAIT);
                    continue;
                }
            },
        };

        // `attempt > 0`, NOT "a connection was opened once before". The gate
        // is on whether a `Retrying` was ANNOUNCED, because `Relinked` is what
        // takes that banner down again — and this is where an earlier draft
        // was wrong: a flag set only by a successful dial left the sequence
        // "first dial fails, second succeeds" showing
        // `reconnecting (attempt 1)` over a fully live dashboard, for the rest
        // of the session.
        if attempt > 0 && msgs.send(Msg::Relinked).await.is_err() {
            return;
        }
        attempt = 0;
        wait = RECONNECT_FIRST_WAIT;

        match run_connected(flock, events, msgs.clone(), polls, period).await {
            // The connection ended. `polls` comes back so the next rung can
            // hand it to the next connection — passed by value rather than by
            // `&mut` because `run_connected` is `tokio::spawn`ed directly by
            // its own tests, and a spawned future cannot borrow a caller's
            // local.
            Ok(returned) => polls = returned,
            // The UI is gone. Nothing left to report to.
            Err(UiGone) => return,
        }
    }
}

/// One connection's lifetime: an opening listing, then subscribe-and-poll
/// until the subscription ends.
///
/// Returns the poll receiver on `Ok`, so the next rung of the ladder can hand
/// it to the next connection.
///
/// # Errors
/// [`UiGone`] when the dashboard's own [`Msg`] channel has closed — the only
/// condition here that is not a reconnect. A failed poll, a dropped frame, a
/// lagging receiver and the subscription ending are all handled in place or
/// handed back to the ladder as `Ok`.
///
/// **Order matters, and it is the opposite of `bleats`'.** `bleats` lists
/// before it subscribes, because its id/name cache has to exist before the
/// first line arrives. lookout subscribes first: its rows carry a whole
/// `ProcessInfo`, so an event arriving before the first snapshot upserts into
/// an empty map perfectly well — while list-then-subscribe would lose every
/// event in the gap for no gain. `Shepherd::link` has already subscribed by the
/// time this is called; the listing below is the first thing that happens
/// after.
pub async fn run_connected<F: FlockSource, E: EventSource>(
    flock: F,
    mut events: E,
    msgs: mpsc::Sender<Msg>,
    mut polls: mpsc::Receiver<()>,
    period: Duration,
) -> Result<mpsc::Receiver<()>, UiGone> {
    // The opening listing. Every connection begins with one — a cold start and
    // a reconnect need the same first snapshot, and this is the one place that
    // serves both. Every poll count in this module's tests counts it.
    reconcile(&flock, &msgs).await?;

    // `interval_at`, not `interval`: a plain `tokio::time::interval` yields its
    // first tick immediately, which would make the first scheduled poll
    // unattributable — it would fire whether or not the interval had elapsed.
    // The opening listing above is the startup poll, deliberately and visibly.
    // Bark's own loop names the same trap.
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    // A dashboard that fell behind must not then fire a burst of catch-up polls
    // at a shepherd that is probably why it fell behind.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => reconcile(&flock, &msgs).await?,
            _ = polls.recv() => reconcile(&flock, &msgs).await?,
            next = events.next_event() => match next {
                // The subscription ended: the connection is gone. Back to the
                // ladder.
                None => return Ok(polls),
                Some(Ok(event)) => {
                    // `Dropped` is a real, named variant this binary
                    // understands, so it gets a repair as well as being
                    // forwarded — it must NOT be treated as an ordinary event.
                    let repair = matches!(event, BusEvent::Dropped { .. });
                    msgs.send(Msg::Event(event)).await.map_err(|_| UiGone)?;
                    if repair {
                        reconcile(&flock, &msgs).await?;
                    }
                }
                Some(Err(Lagged { count })) => {
                    msgs.send(Msg::BusLagged { count })
                        .await
                        .map_err(|_| UiGone)?;
                    reconcile(&flock, &msgs).await?;
                }
            },
        }
    }
}

/// One listing, forwarded as a snapshot.
///
/// A failed poll is dropped rather than propagated: the next event or the next
/// tick tries again, and one bad round trip must not take the connection down
/// — the same call bark's own `reconcile` makes. A poll that fails because the
/// connection is dead will be followed by the subscription ending, which is the
/// condition that does climb the ladder.
async fn reconcile<F: FlockSource>(flock: &F, msgs: &mpsc::Sender<Msg>) -> Result<(), UiGone> {
    match flock.flock().await {
        Ok(rows) => msgs
            .send(Msg::Snapshot {
                rows,
                at: std::time::Instant::now(),
            })
            .await
            .map_err(|_| UiGone),
        Err(RequestError::Closed) => Ok(()),
        Err(_other) => Ok(()),
    }
}

/// The wall-clock moment of a freeze, already formatted.
///
/// Formatted here rather than in `super::app`, which holds no clock and no
/// formatter — see [`Msg::Frozen`]'s own doc. Local, not UTC, for the same
/// reason `output::table::local_timestamp` is: this is read during an incident,
/// at a terminal, by someone thinking in wall-clock time.
fn local_now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
```

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+5**.

### Step 6.4 — MUTATION (the one that matters)

In `run_connected`'s `Some(Err(Lagged { .. }))` arm, delete the
`reconcile(&flock, &msgs).await?;` line.

Run `cargo test -p shep-cli --bins --all-features`.

**Must go red:** `a_lagging_subscriber_polls_immediately_instead_of_waiting`
fails twice over — `assert!(repaired, ...)` because no snapshot follows the
`BusLagged`, and the poll count because it stays at `1` (the opening listing)
where the test wants `2`. That is precisely the silent-drift failure the
two-route design exists to prevent.

**This is the check that was dead in an earlier draft, and it is worth knowing
why**: the test asserted `polls == 1` while the implementation's opening
`reconcile` already made that `1` on its own, so deleting the repair left the
assertion passing. The single most load-bearing mutation in the phase went
green. Every poll count in this module now counts the opening listing
explicitly, which is what makes the `2` above falsifiable.

`a_shepherd_side_drop_polls_and_is_forwarded` must stay **green**: it covers
the other route, and a mutation reddening both would mean the two tests are one
test written twice.

Revert.

### Step 6.5 — second MUTATION

In `run_link`, change `if attempt > RECONNECT_ATTEMPTS` to
`if attempt > u32::MAX`.

**Must go red:** `the_ladder_is_bounded_and_ends_frozen` fails at its
`assert!(done.is_ok(), ...)` — the two-minute bound expires with no `Frozen`
message, because the ladder now climbs forever. This is the test IR-46 is about:
without that `timeout`, this mutation would hang the suite rather than fail it.

Revert.

### Step 6.6 — third MUTATION

In `run_connected`, change `interval_at(now + period, period)` to
`tokio::time::interval(period)`.

**Must go red:** `the_scheduled_poll_lands_on_the_interval_and_not_at_zero`
fails on its first assertion — the counter reads `2` at t=1.9s where the test
wants `1`, because the timer's own startup tick fired a poll alongside the
opening listing and nothing about it was attributable to the dashboard. The
second assertion fails too, at `4` against `3`.

Revert.

### Step 6.7 — fourth MUTATION

In `run_link`, change the relink announcement's guard from `if attempt > 0` to
`if attempt > 1`.

**Must go red:** `a_successful_relink_reports_live_on_the_first_success` fails
on `assert!(relinked, ...)` — one failed dial then a success announces nothing,
so `App` stays in `Link::Retrying { attempt: 1 }` and the banner reads
`the shepherd stopped answering — reconnecting (attempt 1)` over a dashboard
that is applying fresh snapshots. It is the mirror image of the frozen-clock
lie this phase exists to prevent, and the fixture's `keepalive` field is what
makes it visible: with the sender dropped the relinked connection ended at
once, the ladder went round again, and the second cycle's `Relinked` covered
for the first cycle's missing one.

Revert, then run the full task gate.

---

## Task 7 — `lookout/term.rs` and `lookout/input.rs`: raw mode, the panic hook, the keymap

The task that decides whether a crash costs the operator their terminal.

**Files created:**
- `crates/shep-cli/src/lookout/term.rs`
- `crates/shep-cli/src/lookout/input.rs`

**Files modified:**
- `crates/shep-cli/src/lookout/mod.rs` — `pub mod input; pub mod term;`

### Step 7.1 — RED

`crates/shep-cli/src/lookout/input.rs`, test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// fails if a key stops resolving, or starts resolving to the wrong thing.
    /// `x` in particular: it is the one key wired to an action, so a keymap
    /// that silently rebound it would be a keymap that acts on the wrong
    /// intent once 12b makes the action real.
    #[test]
    fn every_bound_key_resolves_to_its_press() {
        assert_eq!(map_key(&key(KeyCode::Char('q'))), Some(KeyPress::Quit));
        assert_eq!(map_key(&key(KeyCode::Esc)), Some(KeyPress::Quit));
        assert_eq!(map_key(&key(KeyCode::Char('j'))), Some(KeyPress::ScrollDown));
        assert_eq!(map_key(&key(KeyCode::Down)), Some(KeyPress::ScrollDown));
        assert_eq!(map_key(&key(KeyCode::Char('k'))), Some(KeyPress::ScrollUp));
        assert_eq!(map_key(&key(KeyCode::Up)), Some(KeyPress::ScrollUp));
        assert_eq!(map_key(&key(KeyCode::Char('g'))), Some(KeyPress::ScrollTop));
        assert_eq!(map_key(&key(KeyCode::Char('G'))), Some(KeyPress::ScrollBottom));
        assert_eq!(map_key(&key(KeyCode::Char('r'))), Some(KeyPress::Refresh));
        assert_eq!(map_key(&key(KeyCode::Char('x'))), Some(KeyPress::Stop));
        assert_eq!(map_key(&key(KeyCode::Char('z'))), None);
    }

    /// fails if Ctrl-C stops quitting. In raw mode crossterm does NOT deliver
    /// Ctrl-C as a signal — it arrives here as an ordinary key event — so if
    /// this mapping goes away, the most-reflexive way out of a terminal
    /// program stops working and the operator's next move is `kill -9` from
    /// another window, which skips every restore path this module has.
    #[test]
    fn ctrl_c_quits_because_raw_mode_swallows_the_signal() {
        let event = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(map_key(&event), Some(KeyPress::Quit));
        // Without the modifier it is not a binding at all.
        assert_eq!(map_key(&key(KeyCode::Char('c'))), None);
    }

    /// fails if key REPEATS and RELEASES start being handled as presses. On a
    /// terminal that reports them (Windows consoles, and any terminal with the
    /// kitty keyboard protocol on), a held `x` would fire the action once per
    /// repeat — which is exactly the fat-finger case the control gate exists
    /// for, arriving through the keymap instead.
    #[test]
    fn only_a_press_counts() {
        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(map_key(&Event::Key(release)), None);

        let mut repeat = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(map_key(&Event::Key(repeat)), None);
    }
}
```

`crates/shep-cli/src/lookout/term.rs`, test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if `restore` stops being safe to call twice. Both the panic hook
    /// and the guard's `Drop` fire on a panic, so the second call is not an
    /// edge case — it is the ordinary path through a crash, and a `restore`
    /// that panicked on its second call would abort the process inside the
    /// panic handler and leave the terminal exactly as broken as doing nothing.
    #[test]
    fn restore_is_idempotent_outside_raw_mode() {
        restore();
        restore();
    }

    /// fails if the guard stops restoring on an ordinary drop. The panic hook
    /// does not run on a `?` or an early `return`; this is the half that does.
    #[test]
    fn the_guard_restores_when_it_is_dropped() {
        let restored = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let _guard = RestoreGuard::with_action({
                let restored = std::sync::Arc::clone(&restored);
                move || {
                    restored.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
        }
        assert_eq!(restored.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``cannot find
function `map_key` in this scope``.

### Step 7.2 — GREEN: `lookout/input.rs`

```rust
//! `crossterm::event::Event` -> [`KeyPress`]. The whole crossterm-typed edge
//! of the keyboard, kept in one small file so `super::app` never imports a
//! terminal crate and its reducer tests never construct one.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use super::app::KeyPress;

/// The [`KeyPress`] this event means, or `None` for a key lookout does not
/// bind.
///
/// Only `KeyEventKind::Press` counts. Terminals that report repeats and
/// releases (Windows consoles, and anything with the kitty keyboard protocol
/// enabled) would otherwise fire an action once per repeat of a held key —
/// which is the fat-finger case the control gate exists for, arriving through
/// the keymap instead of through the operator.
///
/// **`Ctrl-C` is a binding, not a signal.** In raw mode crossterm delivers it
/// as an ordinary key event; there is no `SIGINT` to catch. Dropping this
/// mapping would leave the most reflexive way out of a terminal program doing
/// nothing, and the operator's next move — `kill -9` from another window —
/// skips every restore path `super::term` has.
#[must_use]
pub fn map_key(event: &Event) -> Option<KeyPress> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(KeyPress::Quit),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(KeyPress::Quit),
        KeyCode::Char('j') | KeyCode::Down => Some(KeyPress::ScrollDown),
        KeyCode::Char('k') | KeyCode::Up => Some(KeyPress::ScrollUp),
        KeyCode::Char('g') | KeyCode::Home => Some(KeyPress::ScrollTop),
        KeyCode::Char('G') | KeyCode::End => Some(KeyPress::ScrollBottom),
        KeyCode::Char('r') => Some(KeyPress::Refresh),
        KeyCode::Char('x') => Some(KeyPress::Stop),
        _ => None,
    }
}
```

### Step 7.3 — GREEN: `lookout/term.rs`

```rust
//! Raw mode, the alternate screen, and getting out of both no matter how the
//! process ends.
//!
//! **This is the worst failure a TUI can have**, because it outlives the
//! process: a crash that leaves raw mode on and the alternate screen entered
//! leaves the operator with no echo, no line editing, no visible cursor and
//! often no scrollback, in a shell that looks broken and is.
//!
//! Two mechanisms, both, because neither covers the other's case:
//!
//! 1. **A panic hook** ([`install_panic_hook`]) that restores and *then* calls
//!    the previous hook. Order matters: restoring first puts the default hook's
//!    backtrace on a cooked terminal, on the main screen, where it can be read
//!    and scrolled. A hook does not run on an ordinary early return.
//! 2. **A [`RestoreGuard`]** whose `Drop` restores. `Drop` does not run under
//!    `panic = "abort"` — which this workspace does not set — and covers every
//!    `?` and early `return` the hook does not.
//!
//! [`restore`] is idempotent, because on a panic BOTH of them fire.
//!
//! Nothing that can panic is installed between the hook and raw mode: the hook
//! goes on first, then raw mode, then the alternate screen.
//!
//! **ratatui 0.30's own `init()` would install a restoring hook too.** It is
//! not used here for one reason: it picks the terminal, the backend and the
//! hook as a bundle, and this phase needs the backend swappable for
//! `TestBackend` so the UI loop itself is testable. The four lines below keep
//! that seam and cost nothing.

use std::io::{self, Stdout, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Puts the terminal back the way it was found.
///
/// Every step ignores its own failure: this runs from a panic hook, where
/// there is nothing sensible to do with an error and where returning one would
/// mean skipping the steps after it. Safe to call twice, and routinely is.
pub fn restore() {
    let mut out = io::stdout();
    let _ = crossterm::execute!(out, LeaveAlternateScreen, Show);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Chains a restoring panic hook in front of whatever hook is installed.
///
/// Call before [`enter`], and only once per process.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Enters raw mode and the alternate screen, and hides the cursor.
///
/// **Fails clean.** Raw mode goes on first and the alternate screen second, so
/// there is a window in which the first step has succeeded and the second has
/// not — and a bare `?` there would return `Err` with raw mode still ON, to a
/// caller that is about to return an exit code and never had a guard. The
/// operator would be left with no echo and no line editing, which
/// this module's own doc calls the worst failure a TUI can have. So
/// the second step restores before it reports. The caller arms its
/// [`RestoreGuard`] before calling this as well; both, not either, is the same
/// argument the panic hook and the guard are two of.
///
/// # Errors
/// Whatever `crossterm` could not do to the terminal.
pub fn enter() -> io::Result<Stdout> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    if let Err(err) = crossterm::execute!(out, EnterAlternateScreen, Hide) {
        restore();
        return Err(err);
    }
    Ok(out)
}

/// Restores on drop.
///
/// Holds a closure rather than calling [`restore`] directly so its own test can
/// observe that dropping it acts, without a terminal to act on — the behaviour
/// under test is "the guard runs its action exactly once when it goes out of
/// scope", and that is what regresses if someone converts this to a plain
/// struct with a manual teardown call.
pub struct RestoreGuard {
    action: Option<Box<dyn FnOnce()>>,
}

impl core::fmt::Debug for RestoreGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RestoreGuard")
            .field("armed", &self.action.is_some())
            .finish()
    }
}

impl RestoreGuard {
    /// A guard that calls [`restore`] when it is dropped.
    #[must_use]
    pub fn new() -> Self {
        Self::with_action(restore)
    }

    /// A guard that calls `action` when it is dropped.
    #[must_use]
    pub fn with_action(action: impl FnOnce() + 'static) -> Self {
        Self {
            action: Some(Box::new(action)),
        }
    }
}

impl Default for RestoreGuard {
    /// The same guard [`RestoreGuard::new`] builds.
    ///
    /// Present because `clippy::new_without_default` is a default-on style
    /// lint and `cargo clippy -- -D warnings` is in the task gate — an
    /// argument-less `new` with no `Default` fails it.
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if let Some(action) = self.action.take() {
            action();
        }
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+5**.

### Step 7.4 — MUTATION

In `map_key`, delete the `if key.kind != KeyEventKind::Press { return None; }`
guard.

**Must go red:** `only_a_press_counts` fails on both of its assertions — a held
`x` now fires once per repeat. `every_bound_key_resolves_to_its_press` stays
green, because every event it constructs is already a press. Revert.

### Step 7.5 — second MUTATION

In `install_panic_hook`, swap the two statements so `previous(info)` runs before
`restore()`.

This one **cannot be caught by a unit test** — there is no in-process way to
observe a panic backtrace landing on the alternate screen. Verify it by hand,
once, and say in the task report that it was done by hand:

```bash
# In a real terminal, with a real shepherd running:
SHEP_LOOKOUT_PANIC=1 cargo run -p shep-cli -- lookout
```

behind a temporary `if std::env::var_os("SHEP_LOOKOUT_PANIC").is_some() {
panic!("deliberate") }` at the top of `lookout::run`. With the hook in the
correct order the backtrace is readable on the main screen; with the two swapped
it is painted onto the alternate screen and vanishes when the terminal restores.
Remove the temporary panic and the swap afterwards.

Recorded here rather than quietly skipped: this plan's own rule is that every
verification step must be able to fail, and this one fails only under a human
eye. Saying so is the honest version.

### Step 7.6 — gate

The full task gate.

---

## Task 8 — `shep lookout`, and the UI loop

**Files created:** none — `lookout/mod.rs` grows from a module list into the
verb.

**Files modified:**
- `crates/shep-cli/src/lookout/mod.rs`
- `crates/shep-cli/src/cli.rs` — `Lookout(LookoutArgs)`, the `dash` alias
- `crates/shep-cli/src/main.rs` — wiring
- `crates/shep-cli/tests/cli_e2e.rs` — three cases

### Step 8.1 — baseline

```bash
grep -c 'Lookout' crates/shep-cli/src/cli.rs      # 0
grep -rn 'allow.control' crates/ | wc -l          # 0
```

### Step 8.2 — RED

`crates/shep-cli/src/cli.rs`'s test module:

```rust
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
        let Commands::Lookout(default) = Cli::try_parse_from(["shep", "lookout"]).unwrap().command
        else {
            panic!("lookout parses to its own variant")
        };
        assert!(!default.allow_control);

        let Commands::Lookout(flagged) =
            Cli::try_parse_from(["shep", "lookout", "--allow-control"]).unwrap().command
        else {
            panic!("lookout parses to its own variant")
        };
        assert!(flagged.allow_control);
    }
```

`crates/shep-cli/src/lookout/mod.rs`'s test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shep_core::protocol::BusEvent;

    use crate::lookout::app::{App, Control, KeyPress};
    use crate::lookout::theme::Palette;

    /// fails if the loop stops drawing, or stops leaving on `q`. This is the
    /// whole loop under test with no terminal and no socket: a `TestBackend`
    /// for the screen and a finite `Stream` for the keyboard.
    ///
    /// IR-46: bounded, because a loop that never sees its quit key would hang
    /// the suite rather than fail it.
    #[tokio::test(start_paused = true)]
    async fn the_loop_draws_and_quits_on_a_keypress() {
        let (msg_tx, msg_rx) = mpsc::channel(16);
        let (poll_tx, _poll_rx) = mpsc::channel(1);
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let keys = stream::iter(vec![
            Ok(crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
            ))),
        ]);

        drop(msg_tx);
        let done = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, keys, msg_rx, poll_tx),
        )
        .await;
        let terminal = done.expect("the loop left on `q` within ten seconds");
        let frame = crate::lookout::frames::render_text(terminal.backend().buffer());
        assert!(frame.contains("shep lookout"), "it drew at least once");
    }

    /// fails if the loop stops asking for a poll when the reducer says to. The
    /// `Effect::PollNow` a drop produces has to reach the link task, or the
    /// repair the link task exists for never happens.
    ///
    /// **This test is also the starvation pin**, and it is the reason
    /// `stream::empty()` is the key source rather than an accident of
    /// convenience. An empty stream is `Poll::Ready(None)` on its first poll
    /// and on every poll after it, so in a `biased` select with the keyboard
    /// above the message channel, an implementation that did not retire the
    /// keyboard arm would win that arm forever and never read either message
    /// queued below — the loop would spin until the `timeout` fired and this
    /// test would fail on its `expect`. It cannot pass by accident: the two
    /// messages it sends are on the arm that has to be reachable.
    #[tokio::test(start_paused = true)]
    async fn a_drop_forwards_a_poll_request_to_the_link_task() {
        let (msg_tx, msg_rx) = mpsc::channel(16);
        let (poll_tx, mut poll_rx) = mpsc::channel(4);
        msg_tx
            .send(Msg::Event(BusEvent::Dropped { count: 4 }))
            .await
            .unwrap();
        msg_tx.send(Msg::Key(KeyPress::Quit)).await.unwrap();
        drop(msg_tx);

        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, stream::empty(), msg_rx, poll_tx),
        )
        .await
        .expect("the loop left within ten seconds");

        assert_eq!(poll_rx.try_recv(), Ok(()), "the poll request was forwarded");
    }

    /// fails if the KV store stops being a source for the gate. The flag has to
    /// win, and the store has to work — `shep set lookout.allow_control true`
    /// is the whole point of not adding a config section for one bool.
    #[test]
    fn the_flag_wins_over_the_store_and_the_store_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let kv = dir.path().join("kv.json");
        assert_eq!(resolve_control(false, &kv), Control::ReadOnly);

        shep_core::kv::set(&kv, "lookout.allow_control", "true").unwrap();
        assert_eq!(resolve_control(false, &kv), Control::Allowed);
        assert_eq!(resolve_control(true, &kv), Control::Allowed);

        shep_core::kv::set(&kv, "lookout.allow_control", "false").unwrap();
        assert_eq!(resolve_control(false, &kv), Control::ReadOnly);
        assert_eq!(
            resolve_control(true, &kv),
            Control::Allowed,
            "the flag wins over a store that says no"
        );
    }
}
```

Run `cargo test -p shep-cli --bins --all-features`.

**Expected failure — for the stated reason:** compile error, ``no variant named
`Lookout` found for enum `Commands` ``.

And **extend the existing pin** at `crates/shep-cli/src/cli.rs:942`,
`alias_visibility_and_hiding_are_pinned` — the test that already reads
`get_visible_aliases()` for every other aliased verb. Two edits, in the places
its own shape dictates:

```rust
        let lookout = cmd.find_subcommand("lookout").unwrap();
        assert_eq!(lookout.get_visible_aliases().collect::<Vec<_>>(), ["dash"]);
```

and `"lookout"` joins the `visible` list its final loop walks. `dash` is pm2's
own word for this screen and `docs/terminology.md` records it as a first-class
alias rather than a hidden one — the house rule is that every themed word has a
straight twin, forever, and this is the only assertion in the crate that can
tell `visible_alias = "dash"` from `alias = "dash"`.

### Step 8.3 — GREEN: the verb

`crates/shep-cli/src/cli.rs`, in `Commands`, after `Bleats`:

```rust
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
```

```rust
/// Arguments to `shep lookout`.
#[derive(Debug, clap::Args)]
pub struct LookoutArgs {
    /// Let the dashboard act on a sheep. Off by default.
    ///
    /// A guard against a keystroke in a window you were reading, not a
    /// security boundary: lookout runs as you, so anything it could do you can
    /// already do with `shep stop`. Can also be set with `shep set
    /// lookout.allow_control true`; this flag wins.
    #[arg(long)]
    pub allow_control: bool,
}
```

### Step 8.4 — GREEN: `lookout/mod.rs`

```rust
//! `shep lookout` (alias `dash`): the terminal dashboard.
//!
//! Phase 12a builds the shell and one pane, the flock table. The bleats feed,
//! the sheep detail pane and the host-usage strip are 12b —
//! `docs/lookout/README.md` says what those frames are for.
//!
//! **Two tasks, two channels.** The link task ([`link::run_link`]) owns the
//! connection: it subscribes, polls, repairs on a drop, climbs a bounded
//! reconnect ladder and freezes when it runs out. The UI loop ([`run_ui`])
//! owns the screen: it reads the keyboard, applies [`app::Msg`]s to
//! [`app::App`], and redraws. They talk over an `mpsc` in each direction —
//! `Msg`s out of the link, poll requests into it. Neither borrows the other,
//! which is what lets each be tested on its own.
//!
//! **This verb does not exit when the shepherd dies.** `bleats` does, and that
//! is right for a follow. A standing dashboard that vanished would take the
//! last known state of the flock with it, at the moment an operator most wants
//! to read it — Rin's ruling, and the reason [`link::RECONNECT_ATTEMPTS`]
//! exists.

pub mod app;
// `#[cfg(test)]`: every item in `frames` is read by tests and by the gallery
// writer, and by nothing else. `shep-cli` is `[[bin]]`-only, so `pub` exempts
// nothing from `dead_code` — see `output::table::render_table`, which carries
// an `#[allow(dead_code)]` and a comment saying exactly this.
#[cfg(test)]
pub mod frames;
pub mod input;
pub mod link;
pub mod source;
pub mod term;
pub mod theme;
pub mod view;

use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use shep_core::paths::ShepPaths;
use tokio::sync::mpsc;

use self::app::{App, Control, Effect, Msg};
// The trait, for the opening dial below. `BusEvent` and `KeyPress` are named
// only from the test module and are imported there, not here: an import used
// solely by `#[cfg(test)]` code warns in the ordinary build, and `-D warnings`
// makes that a task-gate failure.
use self::source::Shepherd;
use self::theme::Palette;
use crate::cli::{Format, LookoutArgs};
use crate::exit::ExitCode;
use crate::output::{self, Streams};

/// How often the uptime column is re-derived.
///
/// One second. Nothing on the wire changes on this tick — it exists so a
/// running sheep's UPTIME advances between the two-second polls instead of
/// stepping. Time-derived cells are the one thing a purely event-driven redraw
/// starves.
pub const HEARTBEAT: Duration = Duration::from_secs(1);

/// The floor on the gap between two draws.
///
/// ~30 frames a second. A `shep muster` of a large flock emits a `process.*`
/// event per sheep within a second or two; without this gate each one costs a
/// full render. The gate makes a burst of N events cost at most one draw per
/// 33ms rather than N draws, and it is armed only while something is actually
/// dirty, so an idle dashboard draws nothing at all.
pub const MIN_REDRAW: Duration = Duration::from_millis(33);

/// Runs the dashboard, and returns the [`ExitCode`] to exit with.
///
/// Three refusals, all of them before a single escape byte is written:
///
/// - [`ExitCode::Usage`] when stdout is not a terminal.
/// - [`ExitCode::DaemonUnreachable`] (or [`ExitCode::ProtocolMismatch`]) when
///   the FIRST connection fails — see [`source::LinkError::exit_code`]. A
///   shepherd that was never running is not the case Rin's retry-then-freeze
///   ruling is about, and lookout refuses it exactly as `shep flock` would.
/// - [`ExitCode::Failure`] when the terminal could not be put into raw mode.
///
/// After that it does not exit on its own at all: the ladder and the freeze
/// take over, and the operator quits.
pub async fn lookout(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &LookoutArgs,
) -> ExitCode {
    // A TUI piped into a file is a usage error, not a rendering mode: the
    // alternative is writing alternate-screen escapes into somebody's log.
    // This is also what makes the refusal testable — `assert_cmd` captures
    // stdout, so the e2e case gets this path for free.
    if !std::io::stdout().is_terminal() {
        let _ = output::emit_error(
            &mut *streams.err,
            fmt,
            ExitCode::Usage.code_str(),
            "lookout needs a terminal; stdout is not one",
        );
        return ExitCode::Usage;
    }

    // The FIRST dial, and it happens HERE — before the palette, before the
    // panic hook, before raw mode, and before anything has been drawn. A
    // shepherd that was never running gets the same refusal `shep flock` gets:
    // one error envelope on stderr and exit 5. Rin's "lookout never exits on
    // its own" is about a shepherd that dies *underneath* a running dashboard;
    // opening the alternate screen to spend eight seconds reconnecting to
    // something that was never there, announcing a death that never happened
    // and then exiting `Success`, is a different thing and a worse one.
    //
    // Everything after this point is the running-dashboard case, which is the
    // one the ladder is for.
    let mut shepherd = source::UnixShepherd::new(&paths.socket);
    let opened = match shepherd.link().await {
        Ok(opened) => opened,
        Err(err) => {
            let code = err.exit_code();
            let _ = output::emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            return code;
        }
    };

    let palette = Palette::detect(
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
        std::env::var_os("COLORTERM").as_deref(),
    );
    let control = resolve_control(args.allow_control, &paths.kv);
    let app = App::new(
        palette,
        control,
        paths.home.to_string_lossy().into_owned(),
        Instant::now(),
    );

    // Hook first, then the guard, then raw mode, then the alternate screen —
    // nothing that can panic in between. See `term`'s own module doc.
    term::install_panic_hook();
    // ARMED BEFORE `enter()`, deliberately. `enter` turns raw mode on and then
    // enters the alternate screen; if the second step fails, an earlier draft
    // of this function returned `Err` with raw mode still on and no guard yet
    // in existence, leaving the operator's shell with no echo and no line
    // editing — the failure design decision 7 calls the worst a TUI can have,
    // reached through the error path of the very function that prevents it.
    // `restore()` is documented idempotent and safe outside raw mode, so
    // arming it early costs nothing on the paths that never enter.
    let _guard = term::RestoreGuard::new();
    let out = match term::enter() {
        Ok(out) => out,
        Err(err) => {
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Failure.code_str(),
                &format!("could not put the terminal into raw mode: {err}"),
            );
            return ExitCode::Failure;
        }
    };

    let terminal = match Terminal::new(CrosstermBackend::new(out)) {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Failure.code_str(),
                &format!("could not open the terminal: {err}"),
            );
            return ExitCode::Failure;
        }
    };

    let (msg_tx, msg_rx) = mpsc::channel(1024);
    let (poll_tx, poll_rx) = mpsc::channel(8);
    // The connection opened above is handed straight in, so the link task
    // never dials for its first one.
    let link = tokio::spawn(link::run_link(
        shepherd,
        opened,
        msg_tx,
        poll_rx,
        link::FLOCK_POLL,
    ));

    let events = crossterm::event::EventStream::new();
    let _ = run_ui(app, terminal, events, msg_rx, poll_tx).await;
    link.abort();
    ExitCode::Success
}

/// Whether this lookout may act, from the flag or from the KV store.
///
/// The flag wins. The store is `$SHEP_HOME/kv.json` —
/// `shep set lookout.allow_control true` — rather than a new `[lookout]`
/// section in `shep.toml`, because this gate is the operator's own and not the
/// shepherd's: lookout runs as the operator, and the shepherd cannot tell one
/// of its keypresses from a `shep stop`. `whistle.allow_control` is daemon-side
/// for the opposite reason — its control tools act for a client nobody is
/// watching.
///
/// A store that cannot be read is read as "no". A dashboard that failed open on
/// an unreadable file would be a gate that disappears exactly when something is
/// wrong with the machine.
#[must_use]
pub fn resolve_control(flag: bool, kv: &Path) -> Control {
    if flag {
        return Control::Allowed;
    }
    match shep_core::kv::get(kv, "lookout.allow_control") {
        Ok(Some(value)) if value == "true" => Control::Allowed,
        _ => Control::ReadOnly,
    }
}

/// The UI loop.
///
/// Generic over the backend and over the key source, so a test drives the whole
/// thing with a `TestBackend` and a finite `Stream` — no terminal, no socket.
/// Returns the terminal so that test can read the last frame.
///
/// Four arms, `biased` in this order:
///
/// 1. **`SIGTERM`.** In raw mode Ctrl-C is a key event, not a signal, so this
///    arm is not about the operator — it is a session teardown or a `kill`.
///    Breaking the loop lets the `RestoreGuard` in the caller run, which is the
///    difference between a tidy exit and a broken terminal.
/// 2. **The keyboard**, for latency: a keypress that waited behind a burst of
///    bus events feels like a hung program.
/// 3. **The link's messages.**
/// 4. **The heartbeat**, which advances the uptime column and nothing else.
///
/// **`biased` makes an exhausted arm a hazard, not a detail.** A `Stream` that
/// has ended returns `Poll::Ready(None)` immediately and forever, and an `mpsc`
/// whose senders are all dropped does the same — so an arm that has run dry,
/// sitting above another one in a biased select, wins every iteration and
/// starves everything below it for the life of the loop. Concretely: a
/// persistent stdin read error would freeze the display while the link and the
/// heartbeat never got polled, and a key source that ended would spin this loop
/// at full tilt forever. So arms 2 and 3 each carry a **branch precondition**
/// (`, if !keys_done` / `, if !link_done`) that takes them out of the running
/// for good once their source is done. Arm 4 is unconditional, so the `select!`
/// always has an enabled branch. Arm 1 needs the same idea in its other form —
/// `std::future::pending()` — because a missing signal handler is not a
/// condition that changes.
///
/// The redraw is not an arm: it happens after the `select!`, gated on `dirty`
/// and on [`MIN_REDRAW`] having elapsed.
pub async fn run_ui<B: Backend, S>(
    mut app: App,
    mut terminal: Terminal<B>,
    events: S,
    mut msgs: mpsc::Receiver<Msg>,
    polls: mpsc::Sender<()>,
) -> Terminal<B>
where
    S: Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
{
    let mut events = events;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )
    .ok();

    // Set once each, when their source runs dry. See this function's doc.
    let mut keys_done = false;
    let mut link_done = false;

    let mut dirty = true;
    // `Option`, not `Instant::now() - MIN_REDRAW`: subtracting from a fresh
    // `Instant` is a panic on a platform whose monotonic clock starts near
    // zero, and "has never drawn" is what the first iteration actually means.
    let mut last_draw: Option<Instant> = None;

    loop {
        if dirty && last_draw.is_none_or(|at| at.elapsed() >= MIN_REDRAW) {
            let _ = terminal.draw(|frame| view::draw(&app, frame));
            dirty = false;
            last_draw = Some(Instant::now());
        }

        let msg = tokio::select! {
            biased;
            () = async {
                match sigterm.as_mut() {
                    Some(signal) => {
                        signal.recv().await;
                    }
                    // No handler could be installed; this arm must then never
                    // complete, rather than completing immediately and
                    // spinning the loop.
                    None => std::future::pending().await,
                }
            } => break,
            event = events.next(), if !keys_done => match event {
                Some(Ok(crossterm::event::Event::Resize(..))) => Some(Msg::Resize),
                Some(Ok(event)) => input::map_key(&event).map(Msg::Key),
                // A key source that has ENDED, or has started erroring. The
                // real one does neither in ordinary use; a test's ends on its
                // last scripted key, and a stdin whose descriptor has gone bad
                // errors on every poll rather than once. Both conditions are
                // permanent, and both retire this arm rather than being
                // shrugged off: an arm that keeps completing immediately,
                // above the link and the heartbeat in a `biased` select,
                // freezes the display and spins the process at full tilt. The
                // dashboard keeps running on the other arms; the operator
                // quits with a signal.
                Some(Err(_)) | None => {
                    keys_done = true;
                    None
                }
            },
            msg = msgs.recv(), if !link_done => match msg {
                Some(msg) => Some(msg),
                // Every sender dropped: the link task ended without freezing,
                // which only happens if it was aborted. Keep the last frame up
                // — and retire this arm, because a closed `mpsc` is `Ready`
                // forever and would otherwise starve the heartbeat below it.
                None => {
                    link_done = true;
                    None
                }
            },
            _ = heartbeat.tick() => Some(Msg::Tick { now: Instant::now() }),
        };

        // Nothing to apply. Not a spin risk, and it is worth naming why there
        // are exactly three ways to get here: an unbound keypress (which needs
        // a fresh keystroke), and each of the two sources retiring (once each,
        // ever). Nothing on this path can repeat without something new
        // happening, so there is no sleep and no yield.
        let Some(msg) = msg else { continue };

        match app.update(msg) {
            Effect::Quit => break,
            Effect::PollNow => {
                // `try_send`, not `send`: a full poll channel means a repair is
                // already queued, and blocking the UI on it would stall the
                // screen for exactly as long as the shepherd is slow. A CLOSED
                // one means the link task has ended — which the reducer
                // already accounts for, by refusing `r` outright once the link
                // is `Lost` rather than letting a request vanish here.
                let _ = polls.try_send(());
                dirty = true;
            }
            Effect::None => dirty = true,
        }
    }

    terminal
}
```

`crates/shep-cli/src/main.rs`, in the dispatch — and **not** in the locked block.
`lookout` runs until the operator quits, which is the same shape `bleats` and
`daemon` are excluded for: a `StdoutLock` held for a process lifetime blocks the
first record any other thread writes, forever. Put it beside the `bleats` early
dispatch:

```rust
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
```

and `Commands::Lookout(_)` joins the `unreachable!` arm's list at the bottom of
the match, so the two dispatch sites cannot drift.

Run `cargo test -p shep-cli --bins --all-features`. Expect green, **+5**.

### Step 8.5 — GREEN: end to end

`crates/shep-cli/tests/cli_e2e.rs`, three cases:

```rust
/// fails if `shep lookout` writes terminal escapes into a pipe. `assert_cmd`
/// captures stdout, so this exercises the not-a-tty refusal exactly as a
/// `shep lookout > dash.txt` would — and it is the case that keeps a redirected
/// dashboard from corrupting a file. Also proves the verb does not HANG
/// without a terminal, which is the regression that would cost CI a job rather
/// than a test (IR-46: `.timeout(CMD_TIMEOUT)` is on the chain).
#[test]
fn shep_lookout_refuses_when_stdout_is_not_a_terminal() {
    let home = TempDir::new().unwrap();
    let output = shep(home.path())
        .arg("lookout")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("needs a terminal"));
}

/// fails if the `dash` alias stops reaching the same verb. Same refusal, same
/// code, through the other spelling.
#[test]
fn shep_dash_is_the_same_verb() {
    let home = TempDir::new().unwrap();
    let output = shep(home.path())
        .arg("dash")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr).unwrap().contains("needs a terminal"));
}

/// fails if `--help` stops naming the gate. `--help` is where an operator
/// learns that the dashboard is read-only by default, and the flag's own text
/// is where they learn the gate is not a security boundary.
///
/// **Two assertions this test deliberately does not make.** It does not assert
/// `text.contains("dash")`: the verb's own about-text says "live dashboard"
/// and the flag's help says "the dashboard", so that substring is there
/// whether or not the alias is — delete `visible_alias` and it still passes.
/// The alias is pinned in `cli.rs`'s `alias_visibility_and_hiding_are_pinned`,
/// through `get_visible_aliases()`, which is the only assertion that can tell
/// the difference. And it asserts on `security boundary`, not on the whole
/// sentence: `wrap_help` is enabled on this crate's clap, so clap re-wraps
/// long help at the detected terminal width and a longer phrase can land
/// across a line break on one machine and not another.
#[test]
fn shep_lookout_help_names_the_gate() {
    let home = TempDir::new().unwrap();
    let output = shep(home.path())
        .args(["lookout", "--help"])
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("--allow-control"));
    assert!(text.contains("security boundary"));
}
```

Run `cargo test -p shep-cli --test cli_e2e --all-features`. Expect green, **+3**.

**There is no e2e case for the no-shepherd refusal, and that is structural.**
The `is_terminal` guard runs first and `assert_cmd` always gives the child a
pipe, so a `shep lookout` launched from a test never reaches the opening dial
— it exits `2` on the tty check before it can exit `5` on the connection. The
order is right (a TUI in a pipe is a usage error whether or not a shepherd
exists, and checking it first costs nothing), so the refusal is covered by
construction rather than by a case here: the dial is the first statement after
the guard, its `Err` arm is four lines, and `source::LinkError::exit_code` has
the mapping. Said out loud rather than left as a gap someone finds later.

### Step 8.6 — MUTATION

In `lookout::lookout`, delete the `is_terminal` guard.

Run `cargo test -p shep-cli --test cli_e2e --all-features`.

**Must go red:** `shep_lookout_refuses_when_stdout_is_not_a_terminal` fails on
`assert_eq!(output.status.code(), Some(2))`, and *how* it gets there depends on
the machine, which is worth knowing before the run: with a controlling
terminal, `enable_raw_mode` succeeds against `/dev/tty`, the verb draws into a
pipe and runs until `.timeout(CMD_TIMEOUT)` kills it — which is why the case
carries that timeout. Without one, `enable_raw_mode` fails with `ENOTTY` and
the verb exits `1`. Either way the exit code is not `2` and the case reddens;
do not treat the timeout path as the only correct outcome.

Revert.

### Step 8.7 — second MUTATION

In `resolve_control`, change the flag branch to `if false`.

**Must go red:** `the_flag_wins_over_the_store_and_the_store_is_read` fails on
its last assertion — the flag no longer overrides a store that says `false`.
`actions_are_off_unless_the_flag_says_otherwise` stays green, because that one
tests clap and not the resolution. Revert, then run the full task gate.

---

## Task 9 — the ledger, the docs, and the phase gate

**Files modified:**
- `crates/shep-cli/CHANGELOG.md`
- `docs/specs/deferred.md`
- `README.md`
- `docs/terminology.md` — the lookout row's "built" column
- This plan file — the measured dependency number from Task 1, if it was not
  written back then

### Step 9.1 — baseline

```bash
grep -c 'ratatui' docs/specs/deferred.md                                  # 2
grep -c '`ratatui` is not a dependency' docs/specs/deferred.md            # 1
grep -c 'lookout' README.md                                               # 2
grep -c 'the terminal dashboard | `shep lookout` (alias `dash`) | no' README.md   # 1
grep -c 'Not started: the lookout TUI' README.md                          # 1
```

The second one carries its backticks on purpose. `deferred.md:71` reads
`` `ratatui` is not a dependency of any crate. `` — with the crate name in
code ticks — so the unbackticked pattern an earlier draft of Step 9.5 used
matches nothing at HEAD, prints `0` before the edit and `0` after it, and can
never fail. It was the only check pinning this file's rewrite.

### Step 9.2 — the CHANGELOG

`crates/shep-cli/CHANGELOG.md` → `## [Unreleased]` → `### Additions`:

```markdown
- Add `shep lookout` (alias `dash`), the terminal dashboard. This first cut is
  the shell plus one pane, the flock table: it subscribes to the bus so the
  screen moves as things happen, and re-lists the flock every two seconds so a
  dropped event cannot leave it quietly wrong. If the shepherd stops answering
  it re-dials five times over about eight seconds, then says so and stops
  updating — the last known values stay on screen and it does not exit. Acting
  on a sheep is off unless `--allow-control` or
  `shep set lookout.allow_control true` says otherwise. The bleats feed, the
  sheep detail pane and the host-usage strip are next.
```

### Step 9.3 — `docs/specs/deferred.md`

The **lookout** entry currently reads:

```markdown
**lookout** (spec §9, §13) — the ratatui TUI (`lookout`/`dash` verb).
`ratatui` is not a dependency of any crate.
```

Both sentences are now false. Replace with:

```markdown
**lookout's other three panes** (spec §9, §13) — `shep lookout` ships its
shell and its flock table (Phase 12a). The bleats feed, the sheep detail pane
and the host-usage strip are not built, and neither is search/filter. Actions
have a gate (`--allow-control`, `lookout.allow_control`) and a refusal; there
are no actions behind it yet. lookout does not subscribe to `log.*` — the feed
that would read it does not exist, and a dashboard that subscribed anyway would
be the highest-volume subscriber on the bus for a pane it does not draw.
```

Then re-read the whole file rather than only that entry, per the standing rule
this ledger has carried since Phase 9: check that no other entry has become
false. `ratatui` is now a dependency, so any other sentence claiming otherwise
goes with it.

### Step 9.4 — `README.md`

Two edits:

- The lexicon row: `| the lookout | the terminal dashboard | `shep lookout`
  (alias `dash`) | no |` → `partly`, matching how the docs' own terminology
  table spells a partial build.
- The "What's not built yet" paragraph: `Not started: the lookout TUI, the
  whistle MCP server, …` — the lookout is started, so it comes out of that list
  and gets its own sentence saying what it has and what it does not.

`docs/terminology.md`'s lookout row gets the same treatment if it carries a
built column.

### Step 9.5 — verify

```bash
grep -c '`ratatui` is not a dependency' docs/specs/deferred.md   # was 1, now 0
grep -c 'Not started: the lookout TUI' README.md                 # was 1, now 0
grep -rn 'allow.control' crates/ | wc -l                         # was 0, now >= 4
find docs/lookout -type f | wc -l                                # 3
```

Backticks in the first pattern, matching Step 9.1's baseline: without them it
prints `0` on both sides of the edit.

### Step 9.6 — the phase gate

Each from its own command, `$?` captured directly:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features -- --test-threads=1
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

Plus both `benches/` gates, per CLAUDE.md.

Expect the summary to have moved from **1163 passed / 0 failed / 3 ignored
across 16 result lines** to roughly **1213 passed / 0 failed / 4 ignored across
16 result lines** — the counts are a shape, the `failed` is not, and the
`ignored` is not: it is 4, from the one gallery writer, and nothing else.

The serial run is not ceremony. It was red on `main` before Phase 5 and it
caught a real regression in Phase 6, and this phase adds a test that spawns
tasks and one that writes files.

### Step 9.7 — hand Rin the frames

The last step of the phase is not a command. Send her:

- `docs/lookout/frames.txt` and `docs/lookout/frames.ansi` (`less -R` for the
  second)
- the open questions from `docs/lookout/README.md`: where the other three panes
  sit and which are focusable; whether the flock table grows a selected row and
  what marks it; which actions the gate lets through and what confirms them;
  whether filter takes the CLI selector grammar or plain substring

That is what 12a was for.

---

## What this phase does NOT build, and why

- **The bleats feed, the sheep detail pane, the host-usage strip.** 12b. Each
  is named above with its own reason; the short version is that all three are
  layout decisions and the layout is what these frames exist to decide.
- **Any action.** The gate and both refusals ship; the actions do not. An
  action key needs a confirmation affordance, and that is a layout decision.
- **Search / filter.** Spec §9 lists it. It narrows two panes and there is one.
  `tui-input`, which the earlier research doc wanted for its cursor arithmetic,
  is not taken and will arrive on its own argument.
- **Mouse support.** One less terminal state to restore, and scroll-wheel
  handling drags in kitty-protocol edge cases. Keyboard only.
- **A `[lookout]` section in `shep.toml`.** The one setting this phase has lives
  in the KV store, for the reason design decision 2 gives. A config section for
  one bool, in a file the shepherd reads, for a gate the shepherd is not the
  authority on, would be the wrong shape in three ways at once.
- **Any wire change.** `PROTOCOL_VERSION` stays 1 and no `Request`,
  `Response` or `BusEvent` variant is added. lookout is a reader of two RPCs
  that already ship.
- **A PTY test harness.** `TestBackend` exercises every render path headlessly;
  a PTY test would re-verify crossterm rather than shep.

---

## Two things a reviewer should push on

Written down because they are the calls most likely to be wrong, and naming
them is cheaper than defending them later.

1. **Five attempts / ~8 seconds may be too short for a machine under real
   load.** The argument for it is in design decision 1 and the numbers are
   named constants in one file, so moving them is a one-line change and a
   comment edit. What is *not* adjustable without a redesign is the shape —
   bounded, then frozen, never exiting — and that is Rin's ruling rather than
   an implementation detail.
2. **The uptime column advancing between polls is a small invention.** The
   shepherd reports `uptime_ms` as of the reply; lookout adds the elapsed time
   since. It is what stops the column from stepping every two seconds, and it
   stops dead when the link is lost — but it does mean the number on screen is
   derived rather than reported. If Rin would rather see only what the shepherd
   said, `App::uptime_ms` is the one function to change and its two tests are
   the two to invert.
