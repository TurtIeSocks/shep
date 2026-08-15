# Phase 12b — the lookout's three remaining panes

`shep lookout`'s bleats feed, sheep detail pane and host-usage strip: the
three panes spec §9 names and Phase 12a deliberately did not build. Against
merged `main` at `ed09740`.

## Rin's decision, and what it rules out

She read `docs/lookout/frames.txt` and said:

> "I think the content is fine. For v1 I think just getting the basics down
> for now is a good place to start, just like you have."

Asked what that means concretely, she chose **all three remaining panes, kept
simple** — the bleats feed, the sheep detail pane, the host-usage strip, each
built as plainly as the flock table already is. No filtering UI, no elaborate
layout, just the panes spec §9 names. **The flock table stays the spine.**

So this is a KISS phase, and the rule for anyone executing it is one line:
**every time this plan tempts you to add a knob, do not.** She will ask for it
if she wants it. Spec §9's `search/filter` is named in that sentence and is
still not built here — see "What this phase does not build".

Phases 1–12a are merged: shep-core, the daemon and its supervision engine, the
log plane, the CLI's verb surface, watch/cron/memory-limit restarts,
SO_REUSEPORT reload, custom actions, the pm2 cutover, the dogs subsystem, an
audit-debt phase, the six remaining daemon-surface verbs, and lookout's shell
plus its flock table.

---

## Global constraints

- MSRV 1.88, edition 2024, `MIT OR Apache-2.0`
- `#![forbid(unsafe_code)]` everywhere outside shep-daemon's `sys.rs`
- `PROTOCOL_VERSION` stays 1; any new wire variant needs a pinned fixture.
  **This phase adds none.** Every value the three new panes render comes from
  `Request::ListFlock` (already shipped), from the sheep's own log files on
  disk, or from `sysinfo` reading this machine. If a task here finds itself
  reaching for a new `Request` or `Response` variant, or for
  `Request::Describe`, stop: it has left scope. Design decision 4 says why
  `Describe` in particular is out.
- IR-20: a `pub` error enum in a **library** crate carries `#[non_exhaustive]`
  with a rationale in its own terms, or documents why not. shep-cli is
  `[[bin]]`-only, so nothing here is in a library crate — every new type below
  still carries the comment saying so, rather than leaving the omission
  silent. `source::LinkError` and `link::UiGone` are the two shipped
  precedents.
- IR-46: a test that can only fail by hanging carries an explicit bound.
- Fast loop: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`.
  **shep-cli is `[[bin]]`-only, so it needs `--bins`, never `--lib`.** No task
  in this phase touches shep-daemon; the shep-cli loop below is the one that
  matters here.
- Task gate: fmt, clippy `-D warnings`, `cargo test --workspace --all-features`,
  `RUSTDOCFLAGS="-D warnings" cargo doc`. One cargo command at a time, `$?`
  captured directly, never through a pipe — in zsh a pipeline's `$?` is the
  last command's and `${PIPESTATUS[0]}` is empty.
- **The full task gate does not run after every task, and that is deliberate.**
  See "Where the gate runs" below — running it mid-chain would fail on
  `dead_code` for code that is about to be live.
- Baseline **1219 passed / 0 failed / 4 ignored across 17 result lines.**
- Terminology: the daemon is "the shepherd" and only that; one managed process
  is "a sheep", the plural is always "the flock"; a sheep's children are
  "lambs". Destructive operations and error text stay plain.

### Where the gate runs, and why not after every task

shep-cli is `[[bin]]`-only. In a normal build the only reachability root is
`main`, so **anything reached solely from `#[cfg(test)] mod tests` is
`dead_code`** — `frames.rs`'s own module doc records this rule and is the
reason that module is `#[cfg(test)]` rather than a plain `pub mod`. The
workspace does not deny `dead_code` in `[workspace.lints.rust]`, so
`cargo test -p shep-cli --bins` only *warns*; but
`cargo clippy --workspace --all-targets --all-features -- -D warnings` — the
task gate's second command — **fails**.

Four things in this phase are written before their call site exists:
`tail::read` (Task 5 → called in Task 6), `LocalReader`/`Local` (Task 3 →
Task 6), `host::strip_line` (Task 4 → Task 8), `detail::detail_lines`
(Task 7 → Task 8). Running the full gate after any of those four would fail
on code that is correct and about to be reachable.

So the cadence is:

- **After Tasks 1, 2, 3, 4, 5, 6 and 7** — the inner loop only:
  `cargo fmt --all --check`, then
  `cargo test -p shep-cli --bins --all-features`. Each task's Step _.x says so.
- **After Task 8** — the full task gate. Task 8 is the task that wires every
  one of those four into `draw`, so it is the first point at which the tree is
  reachability-complete.
- **After Task 9 and Task 10** — the full task gate again, and the phase gate
  at Task 10.

**Do not reach for `#[allow(dead_code)]`.** It would sit on code that is live
two tasks later, and nobody would remove it. If the gate fails on `dead_code`
at Task 8 or later, that is a real finding: something got written and never
wired.

### Reading the counts

Every task states an expected test-count delta. Treat it as a **shape, not a
checksum** — two earlier briefs in this project shipped a stale figure and cost
a review loop each. What matters is that the delta is roughly what the task
says, and that `failed` stays `0` across all 17 result lines.

One count is not a shape: **`ignored` stays at 4.** This phase adds no
`#[ignore]` — the gallery writer already exists and is extended, not
duplicated. If `ignored` moves at all, something ran that should not have.

### The exact commands

One cargo command per invocation, `$?` read directly:

```bash
cargo test -p shep-cli    --bins --all-features            # NOT --lib: shep-cli has no lib target
cargo test -p shep-cli    --test cli_e2e --all-features
cargo test -p shep-core   --lib  --all-features
cargo test -p shep-daemon --lib  --all-features -- --skip ::slow::
cargo test -p shep-client --lib  --all-features
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
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The Windows check earns its place again this phase for the same reason it did
in 12a: `sysinfo` is being reached for from a **`cfg(unix)`-gated module**, and
the check is the only thing that proves the Windows leg still compiles without
dragging the terminal stack in behind it. `sysinfo` is already an
unconditional shep-cli dependency (the metrics dog's `shep_host_*` series), so
this adds no crate to any target — Task 3's baseline measures that rather than
asserting it.

### Every check in this plan states its baseline

Phase 12a shipped three verification steps that could not fail — including a
"terminal too small" message 39 characters long, rendered by a truncating call
and asserted at 28 columns. So: **every non-cargo check below prints what it
prints TODAY, at `ed09740`, on this machine.** Run the baseline command
*before* you make the change. If it does not print what this plan says, stop
and say so — the check is broken, not the tree.

```bash
git rev-parse --short HEAD                                          # ed09740
find crates -name '*.snap' | wc -l                                  # 12
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l   #  8
find crates/shep-cli/src/lookout -name '*.rs' | wc -l               # 11
find crates/shep-cli/src/lookout/view -name '*.rs' | wc -l          #  3
grep -rn '#\[ignore' crates/ | wc -l                                # 16
grep -c 'pub fn scroll' crates/shep-cli/src/lookout/app.rs          #  1
grep -rn 'ScrollUp\|ScrollDown\|ScrollTop\|ScrollBottom' crates/ | wc -l   # 24
grep -rnF 'log.*' crates/shep-cli/src/lookout/ | wc -l              #  1  (the comment saying it is NOT subscribed)
grep -rn 'sysinfo' crates/shep-cli/src/lookout/ | wc -l             #  0
grep -rn 'MIN_WIDTH' crates/shep-cli/src/lookout/ | wc -l           #  8
grep -c '31x6' crates/shep-cli/src/lookout/view/mod.rs              #  3
grep -rn '31x6' crates/ | wc -l                                     #  5  (view/mod.rs 3, frames.rs 1, one snapshot)
grep -c '^=== ' docs/lookout/frames.txt                             #  8
grep -c 'Phase 12a' docs/lookout/README.md                          #  2
grep -rn '12b' crates/ | wc -l                                      # 11
```

**`ed09740`, not `fc3534b`.** The first draft of this plan pinned `fc3534b`
and by the time it was reviewed HEAD was two commits past it; it is now three.
The two commits since are `c9b39a8` (`web/` only) and `ed09740` (a plan file
under `docs/`), and `a56acd3` is this plan itself — none of them touches
`crates/`, so the **1219 passed / 0 failed / 4 ignored** workspace figure
measured at `fc3534b` still holds. Confirm that rather than assuming it:
`git show --stat --oneline c9b39a8 a56acd3 ed09740 | grep -c '^ crates/'`
must print `0`. If it does not, re-measure the workspace baseline before
starting.

**Cargo.lock's package count is deliberately not on that list.** It was `326`
at `fc3534b` and it is `326` at `ed09740`, but **Phase 13 (`whistle`) is in
flight in a parallel worktree and adds packages** — it was `340` in the working
tree while this revision was written. An absolute literal here would stop an
executor for a reason that has nothing to do with this phase. So the check
becomes a *comparison*, not a literal: record the number at Step 3.1, and
Step 10.5 asserts it is unchanged **across this phase**. That is the whole of
the dependency argument anyway — 12b adds no crate — and a delta is what
proves it.

`find … | wc -l`, never a bare glob: under zsh a glob with no match raises
`no matches found` and exits non-zero, indistinguishable from a check that
failed for the reason you cared about.

#### Three shapes a dead check takes, all three found in earlier plans here

**A `git diff` filtered on `^-` can never print `0`.** Unified diff opens each
file's hunk with `--- a/<path>`, which `grep '^-'` matches. Use
`git diff --numstat <paths>` (column 2 is deletions) or
`git diff -U0 <paths> | grep -c '^-[^-]'`.

**A `tokio::time::timeout` around a SYNCHRONOUS call is decoration.** The body
runs to completion on the first poll, so the timer is never armed. Three
places in this phase are exactly that trap: `view::draw`, `frames::render_text`
and `tail::read_window` are all synchronous. None may be "bounded" with a
`timeout`. If a synchronous function is suspected of being able to loop or grow
without bound, the honest bound is a **live assertion** on the thing that would
grow — the byte count read, the line count returned — not a wrapper.

**An `assert!` on a string that is already there.** Every
`assert!(x.contains(…))` below names a substring that is **not** in the
pre-change output. Where that is not obvious, the test's own doc comment says
what the string is distinguishing from. `need 33x6` is the worked example: the
string today is `need 31x6`, and Task 2 changes it, so an assertion on
`need 33x6` genuinely reddens before the change and greens after.

---

## What this phase builds

Three panes, one selection, and the frames that show them.

1. **A selected sheep.** 12a's flock table carries a scroll *offset* and no
   cursor; its README records "whether the flock table grows a selected row"
   as open for 12b. It grows one. `j`/`k`/`g`/`G` move the selection, the
   viewport follows it, and a `>` in a two-column gutter marks it.
2. **The sheep detail pane** — three lines under the table describing the
   selected sheep, built entirely from the `ProcessInfo` the flock listing
   already carries, plus its two log paths.
3. **The bleats feed** — the selected sheep's recent output, read from its
   `out_file`/`err_file` on disk with a bounded window, refreshed on the same
   two-second listing that repairs the table, and honest about what it skipped.
   **It does not subscribe to `log.*`.** Design decision 1 is the whole
   argument and it is the hard problem of this phase.
4. **The host-usage strip** — one line under the title: load average against
   this machine's core count, host memory, the flock's own CPU and memory
   totals, and host uptime. Read locally through `sysinfo`, which shep-cli
   already depends on.
5. **Height tiers.** Optional panes drop as the terminal gets shorter, in a
   fixed order, exactly the way columns already drop as it gets narrower.
6. **Six more gallery scenes**, and captions pinned claim-by-claim.

## What this phase does not build, and why

Each of these is a decision, not an omission. Every one of them is something a
reader of this plan will be tempted to add.

- **No filter or search line.** Spec §9 names `search/filter`; Rin's ruling
  above excludes it from v1 explicitly ("No filtering UI"). It also has an
  unresolved design question 12a already wrote down — whether the filter takes
  the CLI's selector grammar or plain substring matching — and that is Rin's
  call, not an implementer's. Goes to `docs/specs/deferred.md`.
- **No `Request::Describe`, and therefore no lambs in the detail pane.**
  `ProcessInfo::lambs` is `None` on a `ListFlock` reply by construction, and
  its own doc says why: the walk costs a second pass over the machine's process
  table, and a flock listing is the thing an operator leaves running in a loop.
  A dashboard polling `Describe` every two seconds would put that walk on a
  timer. The detail pane's caption must not claim lambs.
- **No actions.** `x` still refuses in both control states, with the same two
  literal sentences 12a shipped. Rin asked for panes; wiring a stop to a
  selection is a different phase and a different risk. The gate stays honestly
  described as a fat-finger catch.
- **No focus model, no pane switching, no mouse.** One keymap, one selection,
  everything on screen describes either the flock or the selected sheep.
- **No feed scrollback and no per-stream toggle.** The feed always shows the
  newest lines of both streams. A toggle is a knob.
- **No configurable pane sizes.** Heights are constants with an invariant test.
- **No colour on the selection marker.** The marker is a character, so it
  survives `NO_COLOR` and a 16-colour terminal intact.

---

## What 12b inherits and must not undo

These five are load-bearing properties of the shipped shell. Every one of them
has a test in this plan that would redden if the property were lost.

1. **`app.now` advances only while the link is not `Lost`,** so a frozen
   dashboard does not count uptime for sheep nothing can observe. This phase
   extends the rule rather than weakening it: the host sample and the feed also
   stop when the link is lost (design decision 7).
2. **The `biased` select's arms retire on exhaustion.** An exhausted stream
   returning `Ready(None)` forever would starve the arms below it and silently
   stop the dashboard updating while it still looks alive. This phase adds no
   new arm to that `select!` — the host sample rides the existing heartbeat and
   the feed rides an `Effect`, precisely so the arm-retirement reasoning does
   not have to be re-derived.
3. **Every coloured cell sits beside a word carrying the same meaning,** so
   `NO_COLOR` and a 16-colour terminal lose decoration and never information.
   **Two** new coloured things arrive this phase — the detail pane's STATUS
   word and the feed's gap notice — and both are words already. The feed's
   `err` tag is not a third: it is muted, per item 4, so nothing about it is a
   colour to lose. The selection marker is not one either, which is the whole
   of design decision 6.
4. **`--bark` red means errored, refused and destructive and NOTHING else.**
   The feed's `err` tag is `--bark`: a line a sheep wrote to stderr is not an
   error by construction. Design decision 6 resolves this — the tag is muted,
   not red.
5. **Actions are gated off by default and the gate is honestly described** as a
   fat-finger catch, not a security boundary.

---

## Design decisions made here, not deferred

### 1. The bleats feed reads log files. It does not subscribe to `log.*`.

This is the one hard problem in the phase, and 12a wrote down why before it
existed. `crates/shep-cli/src/lookout/source.rs`:

> **Not `log.*`, deliberately.** The bleats feed is Phase 12b. Subscribing to
> every line every sheep writes, in order to draw a pane that does not exist,
> would make lookout the highest-volume subscriber on the bus for no visible
> reason — and would manufacture the very `Dropped`/`Lagged` condition
> `super::link` exists to survive.

Building the pane does not make that argument go away. It makes it sharper,
because there is now a feedback loop: `run_connected` answers a `Lagged` or a
`BusEvent::Dropped` with an **immediate `ListFlock`**. So under log volume high
enough to lag this subscriber, a subscribing lookout would answer every lag
with an extra RPC — log traffic converted into request load on the shepherd, at
exactly the moment the shepherd is busiest. That is not a tuning problem; it is
the wrong shape.

Four candidates were on the table.

| | what it costs under a busy flock |
|---|---|
| **a. Subscribe `log.*`, bounded ring in the UI** | Every line of every sheep crosses the socket and this process. Highest-volume subscriber on the bus; `Lagged` becomes routine; each lag triggers a repair `ListFlock`. The ring bounds *memory*, not *traffic*. |
| **b. Subscribe `log.*`, filter to the selected sheep in the link task** | Identical bus traffic to (a) — the shepherd serialises and sends every line regardless; the filter is downstream of the drop. Saves render work, saves nothing that matters. |
| **c. Ask the shepherd to filter** | `log.out`/`log.err` topics carry no sheep identity, so the daemon has nothing to narrow by. `bleats`' own module doc records this, in those words. Fixing it is a wire change, and this phase adds none. |
| **d. Read the selected sheep's log files from disk, on a timer** | Bounded by the **reader**, not the writer. One `stat` and one 64 KiB window per file per refresh, forever, whatever the sheep writes. Zero bus traffic. No `Lagged`, no repair storm, no effect on any other subscriber. |

**(d) is what this phase builds**, and it is also the KISS answer: the code
already exists in shape. `commands/bleats.rs`'s `--no-follow` path solves the
same problem, bounded twice over — a window from the end of the file, then a
line cap once that window is split — and its module doc already argues every
consequence.

**What happens under a flock writing faster than the terminal can draw.** This
is the normal case for a busy host, not an edge, so it gets a specified answer
rather than a shrug:

- The reader seeks to `len - WINDOW` and reads forward. It therefore always
  shows the **newest** lines and never blocks, never buffers, and never grows.
  A sheep writing 100 MB/s costs one seek and one 64 KiB read per refresh, the
  same as a sheep writing nothing.
- Everything the pane does not show is **counted**, and the pane says so. This
  is the part the first draft of this plan got wrong, and it is worth being
  precise about, because "the feed lies" is the failure this whole decision
  exists to avoid.

**Lines go missing in three different places, and only two of them can be
counted in lines.**

| where | how much | known as |
|---|---|---|
| below the 64 KiB window | unknown number of lines | **bytes**, exactly |
| inside the window, above `FEED_TAIL_LINES` | exact | **lines**, exactly |
| inside `Tail::lines`, above the five rows the pane has | exact | **lines**, exactly |

The first draft counted only the first row, which is the **rare** case — a
4 MB burst. The ordinary case is the last two rows: `Tail::lines` holds up to
40 lines per file and the pane renders five of them, so a sheep writing thirty
lines between two polls loses twenty-five of them **with `missed_bytes` at
zero**. A pane that looked complete in exactly that case would be lying
whenever the flock is busy, which is when someone is watching it.

So the tail carries two counts, not one:

- **`Tail::missed_lines`** — lines the reader read and then discarded: those
  above `FEED_TAIL_LINES` per file, plus the partial line a window boundary
  cut in half. Exact.
- **`Tail::missed_bytes`** — bytes appended since the previous read that fell
  **below** the window and were therefore never read at all. Exact as a byte
  count; the number of lines in them is genuinely unknowable without reading
  them, which is the whole point of the window. Zero on the first read of a
  file, and zero when the file shrank.

The pane adds the third quantity itself — `feed.lines.len() - body`, the lines
it holds and has no room for — and renders **one** header line, which is the
ordinary header when nothing was lost and a notice when something was:

| what was lost | the header the operator reads |
|---|---|
| nothing | `bleats  api  out then err  from the log files, re-read with each listing` |
| lines only | `bleats  api  … 25 earlier lines not shown` |
| bytes only | `bleats  api  … 3.8M written before these lines was never read` |
| both | `bleats  api  … 25 earlier lines not shown, and 3.8M before them never read` |

in `attention` (butter, not bark — see decision 6) for all three notice forms.

**The wording is doing work in the two byte cases.** "was never read" rather
than "is not shown": the pane cannot say how many lines are in those bytes,
and a phrasing that put a line count on them would be inventing one. Saying
what it *did* — it did not read them — is the honest form, and it is what an
operator needs in order to know that `tail -f` on the path in the detail pane
would tell them more than this pane can.

**The gap notice REPLACES the ordinary header rather than sitting beside it.**
Two header rows would cost one of the five content rows, on the frame where
content is already scarce. The cost is that the `out then err` disclaimer is
off screen exactly when the gap notice is up; that is the right way round —
the notice is the more urgent of the two — and the disclaimer is in the
pane's own module doc, in the gallery caption, and on every frame where there
is no gap.

- A feed that silently showed the newest 5 lines of a 4 MB burst, with nothing
  saying 4 MB had gone by, would be the single most misleading thing on this
  screen. So would one that silently showed 5 of 30 lines. The notice is the
  price of choosing a poller, and it is priced in lines as well as bytes.

**What (d) loses, stated rather than discovered.**

- **Latency**: a line appears within one refresh (two seconds), not within
  milliseconds. For a dashboard whose flock table already repairs on a
  two-second listing, that is the same cadence, not a new one.
- **Only the selected sheep.** Tailing every sheep's files would be
  N × 2 × 64 KiB per refresh — unbounded in the flock size, which is the thing
  (d) was chosen to avoid. One sheep at a time is also what the detail pane and
  the feed being *about the same sheep* buys: everything below the table
  describes one thing.
- **No merge across `out` and `err`.** A log line carries no timestamp, so
  there is no key to interleave the two files on. `bleats` documents this in
  those words already, and this pane inherits it verbatim rather than inventing
  a second answer. Decision 6 says how the pane renders it.
- **Rotation and `shep flush`** can shrink a file between reads. The reader
  saturates, records the new length, and shows the new tail. No claim is made
  about the bytes that were there.

**What (d) gains that a subscription could not.** A **stopped** sheep still has
log files. A subscribing feed shows nothing at all for a sheep that has already
died — which is the exact moment an operator selects it. `bleats --no-follow`'s
module doc names this too. This is not a consolation; for a dashboard, it is
the better half of the trade.

`source::TOPICS` therefore stays `["process.*", "daemon.*"]`, and its comment
is rewritten from "the feed is 12b" to "the feed reads files, and here is why".

### 2. The refresh cadence is the flock listing's, derived and not configured.

`Msg::Snapshot` returns the new `Effect::RefreshFeed`. The link task already
lists the flock every two seconds and immediately on a drop, a lag, or `r`, so
the feed inherits all four triggers for free — no second interval, no new
`select!` arm, no constant to tune. A snapshot is also exactly when
`out_file`/`err_file` might have changed, so the paths and the tail refresh
together or not at all.

It follows that **the feed freezes when the link freezes**, because no
snapshots arrive after `Msg::Frozen`. That is the property inherited from 12a,
extended to the new pane for free rather than re-argued.

Moving the selection also returns `Effect::RefreshFeed` — but only when the
selection actually changed, so holding `k` at the top of the flock does not
re-read a file per keypress.

**And not while the link is lost, either.** The snapshot path freezes for
free; the key path does not, and that is a hole rather than a subtlety: `j` on
a frozen dashboard would re-read live log files and repaint the feed with
content newer than the banner saying "these values are frozen as of 14:32:07".
That is the same contradiction on one frame that decision 7 refuses for the
host strip, and 12a's
`the_frozen_frame_does_not_move_however_long_the_link_stays_gone` cannot catch
it because it presses no keys.

So `select_at` returns `Effect::None` while `Link::Lost`, **after** moving the
selection. The cursor still moves and the detail pane still re-renders — from
the frozen listing, which is the only thing on screen that is allowed to
change, because the operator asked for it and it describes data that is
already on the frame. No file is touched. Task 1 has the reducer test.

### 3. The blocking read happens on the UI task, and that is deliberate.

`std::fs`, not `tokio::fs`: shep-cli's tokio does not carry the `fs` feature,
and `commands/bleats.rs` already makes and states this call for the same
bounded read. One refresh costs at most two `open`+`stat`+`seek`+64 KiB `read`
pairs. `spawn_blocking` for that would add a task, a channel and a race between
the reply and the next snapshot, to hide about a millisecond.

**How often a refresh happens is the half that has to be stated honestly.**
Two triggers, not one:

- **The two-second listing.** 128 KiB every two seconds, on a task that is
  otherwise asleep. This is the steady state and it is the number the choice
  of a poller was made against.
- **A selection that moved.** `input::map_key` drops `KeyEventKind::Repeat`,
  but ordinary terminals deliver auto-repeat as a *stream of Press events*, so
  a held `j` on a two-hundred-sheep flock arrives as one moved selection per
  repeat — twenty to thirty a second on a normal key-repeat rate. Unbounded,
  that is 128 KiB per keypress, synchronously, on the task that also owns the
  redraw.

Restating the bound would be the cheap answer and it would be a bad one: the
worst case is not a documentation problem, it is a busy loop with a syscall in
it. So the read is **coalesced onto the redraw gate**, which already exists:
`Effect::RefreshFeed` sets `feed_dirty`, and `run_ui` does the read in the same
place `MIN_REDRAW` gates the draw. Three lines, the same shape as the thing
next to it, and it turns the worst case into **one two-file read per 33 ms
while a key is held** — with the read happening immediately before the frame
that shows its result, which is also the only ordering that is correct.

The bound is what makes this defensible, so the bound is a constant with a
live assertion, not a hope: `FEED_WINDOW_BYTES` (64 KiB) and `FEED_TAIL_LINES`
(40). Task 5's tests write a 4 MiB file and assert on `Tail::read_bytes` —
the bytes the reader actually pulled off disk — rather than on the size of
what it returned, and one of them keeps the file **growing during the read**,
because a static fixture cannot catch a reader that is bounded by the writer.

### 4. The detail pane renders what the listing already answered. Nothing more.

Every field on it comes from the `ProcessInfo` the flock table's own rows are
built from — id, name, status, pid, restarts, uptime, cpu, mem, fold, dog, and
the two log paths. No second request, no `Describe`, no lambs. See "What this
phase does not build" for the cost argument.

What the pane adds over the row above it is real and worth three lines: the
**untruncated** name (the table's NAME column truncates with `…`, and a
truncated name is one an operator will type into `shep stop`), the **two log
paths** (so an operator can `tail -f` them in another window, which is the
first thing they will want when the feed shows them a crash), and the fields
the current width tier has dropped.

The caption for this pane says "from the same listing the table is built from —
no extra request, and no lamb list", and that sentence is pinned by a test that
greps the frame for the absence of a lamb column. A caption claiming a thing
this pane does not do is precisely the failure 12a shipped twice.

### 5. The selection is stored as an id, and reseats by position when it dies.

`App::scroll: usize` is replaced by `App::selected: Option<u32>`.

An **index** would be wrong in a way that is silent and dangerous: the flock
map is replaced wholesale every two seconds, so a `shep delete` of an earlier
row leaves an index pointing at a different sheep, and the detail pane and the
feed then describe that different sheep with no visible change. Storing the id
makes reordering, growth and shrinkage all no-ops for the selection.

When the selected id is gone from the new map, the selection falls to whatever
now occupies **the same position**, clamped to the last row — the same
instinct `clamp_scroll` already encodes, applied to a cursor. Falling to row 0
instead would throw an operator back to the top of a two-hundred-sheep flock
every time an unrelated sheep was deleted.

The viewport offset is then **derived, not stored**: `scroll_offset(selected,
viewport, total)` centres the selection where it can and clamps at both ends.
Deriving it removes the entire class of bug where a stored offset and a stored
selection disagree, and it costs three lines:

```rust
if viewport == 0 || total <= viewport { return 0; }
let last = total - viewport;
selected.saturating_sub(viewport / 2).min(last)
```

`KeyPress::ScrollUp`/`ScrollDown`/`ScrollTop`/`ScrollBottom` are renamed to
`SelectUp`/`SelectDown`/`SelectFirst`/`SelectLast`. The keys are unchanged; the
names would otherwise say the pane scrolls, which is no longer what happens,
and names that lie are what this project's own review notes keep catching.

### 6. The selection marker is a character in a gutter, not a colour.

`>` in column 0, a blank in column 1, then the table. Reasons, in order:

- **It survives `NO_COLOR` and a 16-colour terminal.** A `REVERSED` row or a
  coloured row would make the selection a decoration-only signal, which is the
  one rule 12a's palette module is built around.
- **`>` and not `▸`.** `▸` is East-Asian *Ambiguous* width; a terminal that
  renders it double-wide would shift every column of that one row by a cell,
  which is worse than plain. The pane already ships `─` and `…`, so this is not
  a blanket ASCII rule — it is that the *cursor* is the one glyph whose width
  must not be in question.
- **A gutter, not a `Column`.** Making it a column would push all seven tier
  thresholds up by 3 and re-derive `name_width`. Instead the table is rendered
  into `width - GUTTER` starting at `x + GUTTER`, so `columns_for` and every
  threshold in it are untouched. `MIN_WIDTH` — the *table's* floor — stays 31;
  the *terminal's* floor becomes `MIN_TERM_WIDTH = 33`, and the refusal reads
  `need 33x6`.

The feed's `err` tag is muted, not `--bark`. A sheep writing to stderr is not a
sheep in trouble — most runtimes log there by default — and `--bark` is
reserved for errored, refused and destructive. The tag is the word `err`, which
carries the whole meaning on its own; muting it is a loss of decoration.

### 7. Frozen means frozen, including the host strip.

The host strip reads *this machine*, which lookout can still observe after the
shepherd dies. It stops anyway: the reducer's `Msg::Host` arm returns early
while `Link::Lost` and the strip keeps its last values.

**One enforcement point, in the reducer, not two.** An earlier draft of this
sentence said the UI loop also declines to sample. It does not, and it should
not: the loop would then carry a second copy of a rule the reducer already
owns, and the two could drift. The heartbeat samples unconditionally — it is a
memory read and a load average, microseconds — and the reducer decides whether
the dashboard is allowed to believe it. That is the same division the uptime
clock already uses for `Msg::Tick`.

The alternative — one line ticking over on a screen whose banner says "these
values are frozen as of 14:32:07" — is a contradiction on the same frame, and
an operator reading it at 3am has to work out which half to believe. Nothing
about the host matters once the dashboard cannot see the flock.

This buys a free regression test: 12a's
`the_frozen_frame_does_not_move_however_long_the_link_stays_gone` renders the
frozen scene at two clock ages and asserts the two frames are **byte
identical**. A host strip that kept updating would redden it without anyone
writing a new assertion, provided the frozen scene carries a host sample — and
Task 9 makes sure it does.

### 8. Optional panes drop as the terminal gets shorter, in a fixed order.

Exactly the shape `columns_for` already has, on the other axis.

| terminal height | panes |
|---|---|
| ≥ 24 | host strip, detail, feed |
| ≥ 18 | host strip, feed |
| ≥ 14 | host strip |
| ≥ 6 | none — 12a's frame, unchanged |

**The order is least-diagnostic-first and it is a decision.** The **detail
pane** goes first because it is the most redundant thing on the screen: every
number on it except the log paths is already in the selected row above it. The
**feed** goes second because its content exists nowhere else on screen, but a
five-line feed of a busy log is thin. The **host strip** goes last because it
is one row and nothing else on the dashboard says anything about the machine.

`MIN_HEIGHT` stays **6**, so 12a's floor, its refusal message's height number
and its `too_narrow` scene are all untouched. 24 is the classic terminal
height, and the tier table is chosen so that a plain 80×24 gets all three panes
with a seven-row flock table.

**No width tier for the panes.** Below `MIN_TERM_WIDTH` nothing draws at all;
above it, the three panes are rendered through the same `fit` the table uses,
so they truncate with a visible `…` rather than overlapping. A second dimension
of tiering would buy a marginal aesthetic and cost a whole interaction to test.
The `cramped` scene (33 columns, 26 rows) is in the gallery so Rin can see the
truncated result and say if she disagrees.

The row budget is an invariant with a test, not a comment. Fixed chrome is 4
rows (title, header, rule, status bar) plus 1 for the banner; `HOST_ROWS = 1`,
`DETAIL_ROWS = 4` (one rule, three lines), `FEED_ROWS = 7` (one rule, one
header, five lines). Every tier that includes a pane must leave the flock table
at least **3** rows with a banner on screen; the floor tier must leave it at
least 1, which is what `MIN_HEIGHT = 6` has always meant.

### 9. The host strip is host numbers and flock numbers, each labelled as such.

Segments, left to right:

```
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu 14.1%   flock mem 706.0M   up 6d 3h
```

Two things this settles.

**Every segment is self-labelled**, so a strip truncated by a narrow terminal is
never ambiguous about whose memory it is quoting. A bare `mem 12.4G` beside a
bare `mem 706.0M` is a puzzle.

**The order matters and the mechanism does not.** The order above is
least-useful-last-first: `up` goes first when the line does not fit (a host
that has been up six days explains nothing about right now), then `flock mem`,
then `flock cpu`, then `host mem`, leaving the load average — the single most
useful number about a machine running a process manager.

**That fitting is `flock::fit`, the same call every other line on this screen
already goes through.** An earlier draft of this decision built a second
mechanism for it: a `Vec<String>` of five segments, a `while … pop()` loop, a
`joined_width` helper, and a test that walked every width from 200 down to 10
recording the order things vanished. That is a second, differently-shaped
fitting path beside the one the rest of the screen uses, and Rin's ruling for
this phase is "all three panes, kept as plain as the flock table, no elaborate
layout". It is cut.

Nothing is lost by cutting it, which is why this is the easy call rather than
a concession: the segments are joined **in the drop order, left to right**, so
truncating from the right *is* the drop order, for free and with no code. The
only difference is that a segment can be cut mid-word — `flock me…` — instead
of vanishing whole, and the `…` says so, exactly as it does on every name in
the table above it. `fit` also pads to exactly `width`, so the strip cannot
overflow the terminal by construction, which is the property the drop loop was
there to guarantee.

The test that survives is the one that can fail: at 200 columns every segment
is present and self-labelled, and at 40 columns the line still begins with the
load average and ends with a visible `…`.

`load … / 10 cores` comes from `std::thread::available_parallelism()` — std, no
new call per sample, read once when `LocalReader` is constructed. A load average
without a core count is a number nobody can read.

The flock halves are **summed from the rows the dashboard already holds**, not
requested. They are `-` when no row reports a value, never `0.0%`, for the
reason `ProcessInfo::cpu_percent`'s own doc gives: `None` is unknown, and
rendering unknown as zero claims a measurement the shepherd never made.

When `sysinfo` reports the platform unsupported, the strip reads
`host  usage is not available on this platform` and keeps the flock half. That
is a real, expected case — `dog::metrics`' `sample_host` already returns `None`
for it — and it gets its own scene.

### 10. One `Local` trait, not two, and it takes `&mut self`.

Everything lookout reads that does **not** come off the socket goes through one
trait:

```rust
pub trait Local {
    fn host(&mut self) -> Option<HostSample>;
    fn tail(&mut self, out: Option<&Path>, err: Option<&Path>) -> Tail;
}
```

One trait rather than two because `run_ui` already carries two generic
parameters and a fourth would make its signature the hardest thing in the
module; and because both methods answer the same question — "what can this
process see without asking the shepherd".

`&mut self` rather than `&self` because the tail reader must remember each
file's previous length to compute the gap, and `run_ui` owns it outright — it
is not shared with the link task and never crosses a `spawn`, so a `RefCell`
would be borrowed-at-runtime machinery to work around a mutability the type
system is happy to grant. This is the opposite call from `FlockSource::flock`,
which is `&self` precisely because `run_connected` holds it across a `select!`
with an `EventSource` borrowed mutably; the reason is stated in each place.

`HostSample` and `Tail` get their own small types rather than reusing
`dog::metrics::HostReading`. The shapes overlap; the meanings do not — the
metrics dog needs a host **process count** (which costs a process-table walk)
and no load average, this needs a load average and no process count. `source.rs`
already records making exactly this call for `EventSource`, in these words: "the
repetition here is of shape, not of meaning."

### 11. The strip samples on the heartbeat, and the heartbeat is not a new arm.

`run_ui`'s `select!` is `biased` and its arm-retirement reasoning is the
subtlest thing in the module. This phase adds **no arm to it**. The host sample
rides the existing 1-second heartbeat arm; the feed rides `Effect::RefreshFeed`
off a message that was already arriving.

**Task 6 owns that wiring, and it is named here because the first draft of
this plan left it unowned.** Task 4 built the strip, the `Msg::Host` variant
and its reducer arm; Task 8 put the strip on screen; and *no task changed the
heartbeat arm*, which still yielded `Msg::Tick` and nothing else. Every test
and every gallery frame injects `Msg::Host` directly, so nothing would have
caught it — and the shipped binary would have drawn `host  not read yet`
forever, under a strip Rin had approved from `frames.txt`. Task 6 is where
`run_ui` gains its `local: L` parameter, so Task 6 is where the heartbeat arm
gains `app.update(Msg::Host { sample: local.host() })`, and Step 6.2 has a
`run_ui` test that drives one heartbeat with a `FakeLocal` and asserts
`host  load` is on the drawn frame.

Sampling memory and a load average
costs microseconds and no process-table walk (`RefreshKind::nothing()
.with_memory(...)` — deliberately *not* `.with_processes(...)`, which is what
makes `dog::metrics`' sampler expensive enough that shep-daemon's own memory
sampler runs at 15 seconds).

There is no pre-loop sample: `tokio::time::interval` yields its first tick
immediately, so the strip's `host  not read yet` state survives at most one
`MIN_REDRAW` window. It still gets a scene, because it is reachable state and
untested strings are where this project's claims rot.

### 12. `Effect` grows one variant, and `Msg::Bleats` cannot recurse.

```rust
pub enum Effect { None, PollNow, RefreshFeed, Quit }
```

`Effect::RefreshFeed` is handled in `run_ui` exactly as `PollNow` is: the
reducer says what must happen outside itself, the loop does the I/O, and the
result comes back in as a `Msg`. `Msg::Bleats` returns `Effect::None`
unconditionally — a reducer that answered its own feed update with another
refresh request would loop the UI task at full tilt, and the `let _ =` at the
call site is deliberate rather than lazy. Task 6 has a test that pins it.

**Growing this enum breaks a match in a file the reducer's task does not
otherwise touch.** `crates/shep-cli/src/lookout/mod.rs`'s `match app.update(msg)`
is exhaustive over three arms with no wildcard, so `Effect::RefreshFeed` is an
immediate E0004 there — which would mean Task 1 could not compile, its
verification step could not run, and every baseline count downstream of it was
anchored to a build that never happened. Task 1 therefore lists `mod.rs` among
its files and lands a stub arm there, which Task 6 replaces with the real read.

**The same question, asked of every other type this phase grows:**

| type | grown by | exhaustive matches that must be updated in the same task |
|---|---|---|
| `Effect` | Task 1 (`RefreshFeed`) | `lookout/mod.rs`'s `match app.update(msg)` — **the E0004 above**. Nothing else matches `Effect`; `link.rs` only names it in a doc link. |
| `Msg` | Task 4 (`Host`), Task 6 (`Bleats`) | `App::update`'s match, in `app.rs`, which each of those tasks already owns. `link.rs`, `mod.rs`, `view/mod.rs` and `frames.rs` **construct** `Msg` but never match on it. |
| `KeyPress` | Task 1 (four renames, no new variant) | `App::on_key` and `input::map_key`, both in Task 1's files. A rename is a compile error at every use, which is the correct kind of red. |
| `Scene` | Task 9 (six variants) | `label`, `caption`, `size` and `scene_with`, all four in `frames.rs`, which Task 9 owns. `size`'s `_ => (120, 20)` arm is a wildcard, so it will *not* error — Step 9.2 changes it explicitly for that reason. |
| `Stream`, `Panes` | new types | nothing matched them before they existed. |
| `Column`, `Link`, `Control`, `DogSource` | **not grown by this phase** | — |

---

## Task order and dependencies

```
Task 1  selection in the reducer            (app.rs)          — no deps
Task 2  the gutter, the marker, the floors  (view/, flock.rs) — needs 1
Task 5  the bounded tail reader and the gap (tail.rs)         — no deps
Task 3  the Local trait and the host sample (source.rs)       — needs 5
Task 4  the host strip                      (view/host.rs)    — needs 2, 3
Task 6  the bleats feed pane                (view/bleats.rs)  — needs 1, 3, 5
Task 7  the sheep detail pane               (view/detail.rs)  — needs 1, 2
Task 8  height tiers and the layout         (view/mod.rs)     — needs 4, 6, 7
Task 9  the frames: scenes, captions, gallery                  — needs 8
Task 10 docs, the phase gate, the cross-checks                 — needs 9
```

**Execution order: 1, 2, 5, 3, 4, 6, 7, 8, 9, 10.** The numbers are labels and
this list is the order — 5 runs before 3, and the sections below are in
numeric order rather than execution order. The first draft of this plan had
the dependency backwards (`Task 5 — needs 3`), which is impossible: Task 3's
`Local` trait declares `fn tail(&mut self, ..) -> super::tail::Tail` and
`LocalReader::tail` calls `super::tail::read`, neither of which exists until
Task 5 creates the module. Task 3 could not have compiled, so its verification
step could not have run, and Task 5's own baseline was measured against it.

Tasks 1 and 5 are independent and may run in parallel. So may 7 and the
3→4→6 leg once Task 2 lands. Everything else is a chain, because it is one
screen.

---

## Task 1 — `lookout/app.rs`: a selected sheep

Replace the scroll offset with a selection, add the reseat rule, add
`Effect::RefreshFeed`, and rename the four keys that no longer scroll.

**Files:** `crates/shep-cli/src/lookout/app.rs`,
`crates/shep-cli/src/lookout/input.rs`,
`crates/shep-cli/src/lookout/view/status.rs`,
`crates/shep-cli/src/lookout/mod.rs`,
`crates/shep-cli/src/lookout/view/mod.rs` (one call site — see Step 1.4).

`mod.rs` and `status.rs` are not optional extras:

- **`mod.rs`** matches `app.update(msg)` exhaustively over
  `Effect::Quit | Effect::PollNow | Effect::None` with **no wildcard**
  (`mod.rs:339-350`), so adding `Effect::RefreshFeed` is an immediate E0004
  there. Without a stub arm in this task the crate does not compile, Step 1.4
  cannot run, and every count downstream is anchored to a build that never
  happened. The stub is two lines and Task 6 replaces it.
- **`status.rs:74`** is the status bar's key hint —
  `q quit   j/k scroll   g/G top/bottom   r refresh` — printed on every one of
  the gallery frames. This task renames the keys *because* names that say the
  pane scrolls would be lying, and the hint is the only one of those names an
  operator actually reads. Renaming the enum and leaving the hint would be the
  exact failure this task exists to fix, on the one surface that is visible.

**Expected delta:** +7 tests in `shep-cli --bins` — five new reducer tests, one
new input test, one new status test. **Three** existing tests are rewritten in
place rather than added: `every_bound_key_resolves_to_its_press` (input.rs),
`a_snapshot_that_shrinks_the_flock_pulls_the_scroll_back` and
`the_scroll_offset_clamps_at_both_ends` (both app.rs, both assert on
`app.scroll()`, which this task deletes).

### Step 1.1 — baseline

```bash
grep -c 'pub fn scroll' crates/shep-cli/src/lookout/app.rs                # 1
grep -rn 'ScrollUp\|ScrollDown\|ScrollTop\|ScrollBottom' crates/ | wc -l  # 24
grep -c 'RefreshFeed' crates/shep-cli/src/lookout/app.rs                  # 0
grep -c 'j/k scroll' crates/shep-cli/src/lookout/view/status.rs           # 1
grep -rn 'j/k scroll' crates/ | wc -l                                     # 7 — the source, plus six snapshots
grep -c 'j/k select' crates/shep-cli/src/lookout/view/status.rs           # 0
grep -rn 'app.scroll()' crates/ | wc -l                                   # 8
cargo test -p shep-cli --bins --all-features                              # 379 passed; 0 failed; 2 ignored
```

The `RefreshFeed` and `j/k select` counts are `0` today and must be non-zero
after — those are the two checks in this task that cannot pass before the
change. `app.scroll()`'s **eight** call sites are the work — measured, not
estimated, because the first draft of this section guessed five: three in
`a_snapshot_that_shrinks_the_flock_pulls_the_scroll_back`, four in
`the_scroll_offset_clamps_at_both_ends`, and one in `view/mod.rs`'s
`scroll_offset` call. The accessor's own definition is `pub fn scroll(&self)`
and does not match this grep.

### Step 1.2 — RED

Add to `app.rs`'s `mod tests`. All four fail to compile today, which is the
correct kind of red for a rename.

```rust
    /// fails if the selection stops being stored as an ID. The flock map is
    /// replaced wholesale every two seconds, so an INDEX cursor silently
    /// points at a different sheep the moment an earlier row is deleted —
    /// and the detail pane and the feed would then describe that different
    /// sheep with nothing on screen changing. This is the whole reason
    /// `selected` is a `u32` and not a `usize`.
    #[test]
    fn the_selection_follows_the_sheep_and_not_the_row_number() {
        let (mut app, t0) = started();
        app.update(Msg::Key(KeyPress::SelectDown));
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(app.selected(), Some(3), "the third row, worker");

        // Sheep 1 goes away. `worker` is now row 1 rather than row 2 — an
        // index cursor would now be pointing at `api`.
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(2, "api", ProcStatus::Errored),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        assert_eq!(app.selected(), Some(3), "still worker");
        assert_eq!(app.selected_index(), Some(1), "which is now row 1");
    }

    /// fails if a selection whose sheep was deleted jumps to the top of the
    /// flock. It falls to whatever now occupies the same POSITION, clamped —
    /// throwing an operator back to row 0 of a two-hundred-sheep flock every
    /// time an unrelated sheep is deleted is the behaviour this rejects.
    #[test]
    fn a_deleted_selection_falls_to_the_row_that_took_its_place() {
        let (mut app, t0) = started();
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(app.selected(), Some(2), "api, at index 1");

        // api dies; web and worker remain. Index 1 is now worker.
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        assert_eq!(app.selected(), Some(3), "the row that took index 1");

        // The LAST row dying clamps rather than leaving the cursor past the end.
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected(), Some(3));
        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "web", ProcStatus::Online)],
            at: t0,
        });
        assert_eq!(app.selected(), Some(1));

        // An empty flock selects nothing at all, rather than an id that is gone.
        app.update(Msg::Snapshot { rows: vec![], at: t0 });
        assert_eq!(app.selected(), None);
        assert_eq!(app.selected_index(), None);
    }

    /// fails if moving the selection stops asking for a feed refresh, or
    /// starts asking for one when the selection did not move. The second half
    /// is the one that matters: `RefreshFeed` reads two files off disk, and a
    /// held `k` at the top of the flock must not do that once per keypress.
    #[test]
    fn a_selection_that_moves_refreshes_the_feed_and_one_that_cannot_does_not() {
        let (mut app, _) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::SelectDown)), Effect::RefreshFeed);
        assert_eq!(app.update(Msg::Key(KeyPress::SelectFirst)), Effect::RefreshFeed);
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectUp)),
            Effect::None,
            "already at the top: nothing moved, so nothing is re-read"
        );
        assert_eq!(app.update(Msg::Key(KeyPress::SelectLast)), Effect::RefreshFeed);
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::None,
            "already at the bottom"
        );
    }

    /// fails if a snapshot stops refreshing the feed. This is the whole
    /// cadence: the feed has no timer of its own, and inherits the two-second
    /// listing, the drop repair, the lag repair and `r` from this one line.
    /// It must NOT fire once the link is lost — a frozen dashboard re-reading
    /// log files would be the one thing on screen still moving.
    #[test]
    fn a_snapshot_refreshes_the_feed_unless_the_link_is_frozen() {
        let (mut app, t0) = started();
        assert_eq!(
            app.update(Msg::Snapshot { rows: vec![sheep(1, "web", ProcStatus::Online)], at: t0 }),
            Effect::RefreshFeed
        );
        app.update(Msg::Frozen { at_local: "2026-08-14 14:32:07".to_string() });
        assert_eq!(
            app.update(Msg::Snapshot { rows: vec![sheep(1, "web", ProcStatus::Online)], at: t0 }),
            Effect::None,
            "a frozen dashboard does not re-read anything"
        );
    }
```

    /// fails if a frozen dashboard reads a log file. The snapshot path
    /// freezes for free — no snapshots arrive after `Msg::Frozen` — but the
    /// KEY path does not, and `j` on a frozen screen would repaint the feed
    /// with content newer than the banner saying the values are frozen as of
    /// 14:32:07. That is the contradiction on one frame that design decision 7
    /// refuses for the host strip, and 12a's
    /// `the_frozen_frame_does_not_move_however_long_the_link_stays_gone`
    /// cannot catch it, because it presses no keys.
    ///
    /// The cursor still MOVES: the detail pane re-rendering from the frozen
    /// listing is the operator reading data already on the frame, which is
    /// allowed. It is touching the disk that is not.
    #[test]
    fn a_frozen_dashboard_moves_the_cursor_without_touching_a_file() {
        let (mut app, _) = started();
        app.update(Msg::Frozen { at_local: "2026-08-14 14:32:07".to_string() });

        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::None,
            "no file is read once the link is lost"
        );
        assert_eq!(app.selected(), Some(2), "but the cursor moved anyway");
        assert_eq!(app.update(Msg::Key(KeyPress::SelectLast)), Effect::None);
        assert_eq!(app.selected(), Some(3));
    }
```

And in `view/status.rs`'s `mod tests`:

```rust
    /// fails if the key hint keeps saying `scroll` after the keys stopped
    /// scrolling. This is the only one of the four renamed names an operator
    /// ever reads — it is on every frame in the gallery — so leaving it would
    /// be shipping the exact lie this task exists to remove, on the one
    /// surface where it is visible.
    ///
    /// The replacement is the same 48 characters as the original, so
    /// `a_truncated_hint_still_leaves_a_gap_before_the_control_label` at 49
    /// columns is measuring the same thing it measured before.
    #[test]
    fn the_key_hint_says_what_the_keys_now_do() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let hint: String = status_line(&app, 200)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(hint.contains("j/k select"), "got {hint:?}");
        assert!(hint.contains("g/G first/last"), "got {hint:?}");
        assert!(!hint.contains("scroll"), "the pane no longer scrolls: {hint:?}");
    }
```

And in `input.rs`'s `mod tests`, rewrite `every_bound_key_resolves_to_its_press`
to the new names, plus:

```rust
    /// fails if the four movement keys stop meaning SELECTION. They were
    /// named for scrolling in 12a and the pane genuinely scrolled; it now
    /// carries a cursor, and a name that says otherwise is the kind this
    /// project's reviews keep catching. The KEYS are unchanged — an operator's
    /// muscle memory is not what this rename touches.
    #[test]
    fn the_movement_keys_are_unchanged_and_now_mean_selection() {
        assert_eq!(map_key(&key(KeyCode::Char('j'))), Some(KeyPress::SelectDown));
        assert_eq!(map_key(&key(KeyCode::Down)), Some(KeyPress::SelectDown));
        assert_eq!(map_key(&key(KeyCode::Char('k'))), Some(KeyPress::SelectUp));
        assert_eq!(map_key(&key(KeyCode::Up)), Some(KeyPress::SelectUp));
        assert_eq!(map_key(&key(KeyCode::Char('g'))), Some(KeyPress::SelectFirst));
        assert_eq!(map_key(&key(KeyCode::Home)), Some(KeyPress::SelectFirst));
        assert_eq!(map_key(&key(KeyCode::Char('G'))), Some(KeyPress::SelectLast));
        assert_eq!(map_key(&key(KeyCode::End)), Some(KeyPress::SelectLast));
    }
```

### Step 1.3 — GREEN: `app.rs`

`KeyPress` renames — `ScrollUp` → `SelectUp`, `ScrollDown` → `SelectDown`,
`ScrollTop` → `SelectFirst`, `ScrollBottom` → `SelectLast` — with docs
rewritten to say what they now do:

```rust
    /// `k` or `Up` — the selection moves up one row.
    SelectUp,
    /// `j` or `Down` — the selection moves down one row.
    SelectDown,
    /// `g` or `Home` — the first sheep in the flock.
    SelectFirst,
    /// `G` or `End` — the last one.
    SelectLast,
```

`Effect` grows one variant:

```rust
pub enum Effect {
    /// Nothing.
    None,
    /// Ask the link task for a `ListFlock` now, rather than at the next tick.
    PollNow,
    /// Re-read the selected sheep's log files and hand the result back as
    /// [`Msg::Bleats`].
    ///
    /// The feed has no timer of its own. It rides this: a snapshot produces
    /// one (so the two-second listing, a drop repair, a lag repair and `r`
    /// all refresh it), and so does a selection that actually moved. See the
    /// phase plan's design decision 2.
    RefreshFeed,
    /// Leave.
    Quit,
}
```

The field, replacing `scroll`:

```rust
    /// Which sheep the detail pane and the bleats feed describe.
    ///
    /// An **id**, not an index. The flock map is replaced wholesale every two
    /// seconds, so an index survives a `shep delete` of an earlier row by
    /// silently pointing at a different sheep — and every pane below the table
    /// would then describe that different sheep with nothing on screen
    /// changing. `None` only for an empty flock.
    ///
    /// The viewport offset is derived from this rather than stored beside it
    /// ([`super::view::flock::scroll_offset`]), which is what makes a
    /// disagreement between a stored offset and a stored cursor impossible
    /// rather than merely unlikely.
    selected: Option<u32>,
```

The reseat, replacing `clamp_scroll`:

```rust
    /// Puts the selection back on a real sheep after the flock changed.
    ///
    /// `previous_index` is where the selection sat **before** the change, read
    /// while the old map was still in place. A selection whose id survived is
    /// left alone, whatever row it now occupies. One that did not falls to
    /// whatever occupies the same position, clamped to the last row — not to
    /// row 0, which would throw an operator back to the top of a
    /// two-hundred-sheep flock every time an unrelated sheep was deleted.
    ///
    /// Returns whether the selection changed, which is what decides between
    /// [`Effect::RefreshFeed`] and [`Effect::None`].
    fn reseat(&mut self, previous_index: Option<usize>) -> bool {
        let before = self.selected;
        if self.flock.is_empty() {
            self.selected = None;
            return before != self.selected;
        }
        if self.selected.is_some_and(|id| self.flock.contains_key(&id)) {
            return false;
        }
        let index = previous_index.unwrap_or(0).min(self.flock.len() - 1);
        self.selected = self.flock.keys().nth(index).copied();
        before != self.selected
    }
```

`Msg::Snapshot`'s arm:

```rust
            Msg::Snapshot { rows, at } => {
                if matches!(self.link, Link::Lost { .. }) {
                    return Effect::None;
                }
                let previous = self.selected_index();
                self.flock = rows
                    .into_iter()
                    .map(|info| (info.id, Row { info, anchor: at }))
                    .collect();
                self.reseat(previous);
                // Unconditional, and NOT `if reseat(..)`: the paths on the
                // selected row may have changed even when the selection did
                // not, and this is the whole of the feed's cadence. See design
                // decision 2.
                Effect::RefreshFeed
            }
```

`on_event`'s `Delete` arm reseats the same way (`let previous =
self.selected_index();` before the `remove`, then `self.reseat(previous)`),
returning `Effect::RefreshFeed` when the selection moved and `Effect::None`
otherwise. An upsert that is not a delete cannot orphan the selection, so it
reseats only when the flock was empty and now is not.

The key arms:

```rust
            KeyPress::SelectUp => self.select_by(-1),
            KeyPress::SelectDown => self.select_by(1),
            KeyPress::SelectFirst => self.select_at(0),
            KeyPress::SelectLast => self.select_at(self.flock.len().saturating_sub(1)),
```

```rust
    /// Moves the selection by `delta` rows and reports whether it moved.
    ///
    /// Clamped rather than wrapping: wrapping a two-hundred-sheep flock from
    /// the last row to the first on one keypress loses the operator's place
    /// with nothing to undo it.
    fn select_by(&mut self, delta: isize) -> Effect {
        let Some(index) = self.selected_index() else {
            return Effect::None;
        };
        let next = index.saturating_add_signed(delta);
        self.select_at(next)
    }

    /// Selects the row at `index`, clamped to the flock, and reports whether
    /// that changed anything.
    ///
    /// `Effect::None` when it did not: [`Effect::RefreshFeed`] reads two files
    /// off disk, and a held `k` at the top of the flock must not do that once
    /// per keypress.
    fn select_at(&mut self, index: usize) -> Effect {
        if self.flock.is_empty() {
            return Effect::None;
        }
        let index = index.min(self.flock.len() - 1);
        let next = self.flock.keys().nth(index).copied();
        if next == self.selected {
            return Effect::None;
        }
        self.selected = next;
        Effect::RefreshFeed
    }
```

Accessors — `scroll()` is deleted, three arrive:

```rust
    /// The selected sheep's id, or `None` for an empty flock.
    #[must_use]
    pub fn selected(&self) -> Option<u32> {
        self.selected
    }

    /// Which row of [`Self::rows`] the selection sits on.
    ///
    /// Derived every call rather than stored: see [`Self::selected`].
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.flock.keys().position(|key| *key == id)
    }

    /// The selected sheep's row, which the detail pane and the feed read.
    #[must_use]
    pub fn selected_row(&self) -> Option<&Row> {
        self.flock.get(&self.selected?)
    }
```

`select_at` also carries the frozen gate, which is why it — and not
`select_by` — is the single place that returns the effect:

```rust
        self.selected = next;
        // The cursor moved; whether anything is READ is a separate question.
        // A frozen dashboard re-reading live log files would put content on
        // screen newer than the banner saying the values are frozen, which is
        // the contradiction design decision 7 refuses for the host strip. The
        // detail pane still re-renders, from the frozen listing — that is data
        // already on the frame.
        if matches!(self.link, Link::Lost { .. }) {
            return Effect::None;
        }
        Effect::RefreshFeed
```

### Step 1.3b — GREEN: `mod.rs`'s stub arm, and the status hint

`mod.rs:339-350`'s `match app.update(msg)` is exhaustive with no wildcard, so
it gains a fourth arm in this task or the crate does not compile:

```rust
            // Task 6 replaces this with the read. Until then the reducer's
            // request is honoured by redrawing and nothing else — which is
            // correct, because there is no feed on screen yet to refresh.
            Effect::RefreshFeed => dirty = true,
```

`status.rs:74`'s hint, in the `None` arm:

```rust
        None => (
            // `select`/`first/last`, not `scroll`/`top/bottom`: the pane
            // carries a cursor now and the viewport is derived from it. Same
            // 48 characters as the 12a text, so the truncation test at 49
            // columns still measures what it was written to measure.
            "q quit   j/k select   g/G first/last   r refresh".to_string(),
            palette.muted(),
        ),
```

Rewrite `a_snapshot_that_shrinks_the_flock_pulls_the_scroll_back` in place as
`a_snapshot_that_shrinks_the_flock_pulls_the_selection_back`, asserting on
`selected_index()` where it asserted on `scroll()`.

Rewrite `the_scroll_offset_clamps_at_both_ends` in place as
`the_selection_clamps_at_both_ends` — same three key sequences, same reason in
its doc comment (wrapping a two-hundred-sheep flock on one keypress loses the
operator's place), asserting `selected_index()` where it asserted `scroll()`.
**The first draft of this plan missed this one**: it named only the snapshot
test, and this one asserts on `app.scroll()` three times, so the task would
have ended on a build error rather than on a green run.

### Step 1.4 — verify

```bash
cargo fmt --all --check
cargo test -p shep-cli --bins --all-features
```

Expect `386 passed; 0 failed; 2 ignored` — 379 + 7. Not the full task gate:
see "Where the gate runs" in Global constraints.

`view/mod.rs` calls `flock::scroll_offset(app.scroll(), ..)`, and `scroll()` is
deleted here — so this task changes that one call site to
`app.selected_index().unwrap_or(0)` even though the rest of `view/mod.rs` is
Task 2's. `scroll_offset` still has its 12a signature at this point and still
means "the offset asked for", so the call compiles and the pane behaves as it
did; Task 2 changes what the function computes.

```bash
grep -c 'RefreshFeed' crates/shep-cli/src/lookout/app.rs        # was 0; now ≥ 6
grep -c 'RefreshFeed' crates/shep-cli/src/lookout/mod.rs        # was 0; now 1 (the stub arm)
grep -rn 'ScrollUp\|ScrollDown\|ScrollTop\|ScrollBottom' crates/ | wc -l   # was 24; now 0
grep -c 'j/k select' crates/shep-cli/src/lookout/view/status.rs # was 0; now 1
grep -rn 'j/k scroll' crates/ | wc -l                           # was 7; now 6 — the six SNAPSHOTS, until Task 2 re-accepts them
grep -rn 'app.scroll()' crates/ | wc -l                         # was 5; now 0
```

### Step 1.5 — MUTATION

In `reseat`, replace the id check with an index one — store and restore the
index instead:

```rust
        // MUTATION: index cursor
        self.selected = self.flock.keys().nth(previous_index.unwrap_or(0)).copied();
```

`the_selection_follows_the_sheep_and_not_the_row_number` must fail: after
sheep 1 is deleted, the selection lands on `api` (id 2) instead of staying on
`worker` (id 3). If it passes, the test is asserting on positions that happen
to coincide and needs a flock where they do not. Revert.

### Step 1.6 — second MUTATION

In `select_at`, return `Effect::RefreshFeed` unconditionally instead of
`Effect::None` when nothing moved.
`a_selection_that_moves_refreshes_the_feed_and_one_that_cannot_does_not` must
fail on its third assertion. Revert.

### Step 1.7 — third MUTATION

In `select_at`, delete the `Link::Lost` early return.
`a_frozen_dashboard_moves_the_cursor_without_touching_a_file` must fail on its
first assertion — `Effect::RefreshFeed` where `Effect::None` was expected —
**and its second assertion must still pass**, because the cursor is supposed
to move either way. If the second one fails too, the gate was put in the wrong
place: it belongs after the move, not before it. Revert.

---

## Task 2 — the gutter, the marker, and the two floors

The flock table moves two columns right; the selected row gets a `>`; the
terminal's width floor becomes 33 while the table's stays 31.

**Files:** `crates/shep-cli/src/lookout/view/flock.rs`,
`crates/shep-cli/src/lookout/view/mod.rs`,
`crates/shep-cli/src/lookout/frames.rs`.

`frames.rs` is in the list because of one line: `frames.rs:497` asserts
`render_text(&scene(Scene::TooNarrow).1).contains("need 31x6")`. Step 2.1's
baseline in the first draft grepped only `view/mod.rs`, found three hits, and
never saw this fourth one — so the task would have gone red in a file it did
not list, at a step that claimed to be green.

**Expected delta:** **+2 net** — three new tests, and one deleted.
`view/flock.rs:403 the_scroll_offset_never_leaves_a_gap_at_the_bottom` asserts
`scroll_offset(7, 5, 20) == 7`; the centring formula returns `5`. It is not
edited, it is **deleted and superseded** by
`the_offset_keeps_the_selection_visible_and_centred_where_it_can`, which
asserts everything it asserted plus the property the old one could not state
(the selection is inside the window). Say so in the deleting commit.

Eight snapshots are re-accepted (every frame shifts two columns) — see the
note in Task 9 about why that is correct and not a wire-fixture violation.

### Step 2.1 — baseline

```bash
grep -rn '31x6' crates/ | wc -l                             # 5
grep -c '31x6' crates/shep-cli/src/lookout/view/mod.rs      # 3
grep -c '31x6' crates/shep-cli/src/lookout/frames.rs        # 1
grep -rn '33x6' crates/ | wc -l                             # 0
grep -c 'GUTTER' crates/shep-cli/src/lookout/view/mod.rs    # 0
```

The fifth `31x6` is in `snapshots/…too_narrow.snap`, which `cargo insta` will
re-accept. `33x6` is `0` everywhere today and must be non-zero after: that is
what makes the rewritten
`a_terminal_below_the_floor_says_so_instead_of_drawing` a check that can fail.
**Run the first grep, not the second alone** — the first draft of this plan ran
only the second and missed both the `frames.rs` assertion and the snapshot.

### Step 2.2 — RED

In `flock.rs`:

```rust
    /// fails if the selection marker stops being a plain character. Colour
    /// and a `REVERSED` modifier are both rejected here: every signal on this
    /// screen has to survive `NO_COLOR` and a 16-colour terminal, and a
    /// decoration-only cursor does not. `>` rather than `▸` because `▸` is
    /// East-Asian *Ambiguous* width — a terminal rendering it double-wide
    /// shifts every column of that one row by a cell, which is worse than
    /// plain.
    #[test]
    fn the_marker_is_one_ascii_column_wide_in_both_states() {
        // The two literals ARE the test. `chars().count() == 1` and
        // `is_ascii()` were in the first draft and cannot fail once these two
        // have passed — `">"` is one ASCII char by inspection — so they were
        // three assertions dressed as five.
        assert_eq!(mark(true), ">", "not `▸`: East-Asian Ambiguous width would shift the row");
        assert_eq!(mark(false), " ");
    }

    /// fails if the viewport stops keeping the selection on screen, or stops
    /// clamping at either end. A pane whose cursor has walked off the bottom
    /// draws a page the operator is not pointing at, and a detail pane
    /// describing a sheep no row on screen shows is worse than either.
    #[test]
    fn the_offset_keeps_the_selection_visible_and_centred_where_it_can() {
        // Everything fits: no scrolling, wherever the cursor is.
        assert_eq!(scroll_offset(0, 10, 6), 0);
        assert_eq!(scroll_offset(5, 10, 6), 0);
        // Taller than the viewport: centred in the middle, pinned at the ends.
        assert_eq!(scroll_offset(0, 5, 20), 0);
        assert_eq!(scroll_offset(2, 5, 20), 0);
        assert_eq!(scroll_offset(3, 5, 20), 1);
        assert_eq!(scroll_offset(10, 5, 20), 8);
        assert_eq!(scroll_offset(19, 5, 20), 15, "the last page, not past it");
        assert_eq!(scroll_offset(usize::MAX, 5, 20), 15);
        // Degenerate: a viewport of zero rows scrolls nowhere.
        assert_eq!(scroll_offset(3, 0, 20), 0);
        // And the selection is always inside the window it returns.
        for total in [1usize, 2, 7, 40, 200] {
            for viewport in [1usize, 3, 8, 25] {
                for selected in 0..total {
                    let offset = scroll_offset(selected, viewport, total);
                    assert!(
                        selected >= offset && selected < offset + viewport,
                        "selected {selected} fell outside [{offset}, {}) for total {total}",
                        offset + viewport
                    );
                }
            }
        }
    }
```

The exhaustive loop at the end is the assertion that matters; the literals
above it are there so a failure names a specific case rather than a triple.

**Delete `the_scroll_offset_never_leaves_a_gap_at_the_bottom` in the same
commit.** It is not a test this change breaks by accident: it asserts
`scroll_offset(7, 5, 20) == 7`, which is the *old* contract — "the offset is
what the caller asked for, clamped" — and the new contract is "the offset is
wherever the cursor needs the window to be". Both of its surviving clauses
(everything fits ⇒ 0; the last page is the ceiling) are in the replacement
above, at the same literals.

`frames.rs:497`'s `need 31x6` becomes `need 33x6` in the same commit. It is
one character of edit and it is the only assertion outside `view/mod.rs` that
names the floor.

In `view/mod.rs`, rewrite the floor test's three `31x6` literals to `33x6` and
add:

```rust
    /// fails if the table stops leaving room for the marker, or if the marker
    /// stops landing on the selected row. Asserted on the WHOLE line rather
    /// than with `contains`, because a `>` somewhere in a log path would
    /// satisfy `contains` and prove nothing.
    #[test]
    fn the_marker_sits_in_the_gutter_of_the_selected_row_and_nowhere_else() {
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        app.update(Msg::Snapshot {
            rows: (0..4)
                .map(|id| ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build())
                .collect(),
            at: Instant::now(),
        });
        app.update(Msg::Key(KeyPress::SelectDown));

        let frame = draw_to(&app, 100, 12);
        let rows: Vec<&str> = frame.lines().skip(3).take(4).collect();
        assert!(rows[0].starts_with("  0 "), "unselected rows keep a blank gutter: {:?}", rows[0]);
        assert!(rows[1].starts_with("> 1 "), "the marker is on row 1: {:?}", rows[1]);
        assert!(rows[2].starts_with("  2 "), "and on no other row: {:?}", rows[2]);
        assert_eq!(
            frame.lines().filter(|line| line.starts_with('>')).count(),
            1,
            "exactly one marker on the frame"
        );
    }
```

### Step 2.3 — GREEN: `flock.rs`

```rust
/// The columns the selection marker takes, to the left of the table.
///
/// One for the marker, one for the gap. The table itself is rendered into
/// `width - GUTTER` starting at `x + GUTTER`, which is why every threshold in
/// [`TIERS`] and every arithmetic in [`name_width`] is untouched by the
/// marker's arrival — see the phase plan's design decision 6.
pub const GUTTER: u16 = 2;

/// The marker for the selected row, or a blank for every other row.
///
/// A plain ASCII `>`, not a colour and not a `REVERSED` modifier: every
/// signal on this screen survives `NO_COLOR` and a 16-colour terminal, and a
/// decoration-only cursor does not. `▸` is East-Asian *Ambiguous* width, and a
/// terminal that renders it double-wide would shift every column of that one
/// row by a cell.
#[must_use]
pub const fn mark(selected: bool) -> &'static str {
    if selected { ">" } else { " " }
}
```

`scroll_offset` is rewritten to take the selection:

```rust
/// Which slice of the flock is on screen, given where the cursor is.
///
/// Derived every frame from [`super::super::app::App::selected_index`] rather
/// than stored beside it: a stored offset and a stored cursor can disagree,
/// and this way they cannot. The selection is centred where the flock is long
/// enough to allow it and pinned at both ends where it is not, so the last row
/// of the flock is always the last row of the pane.
#[must_use]
pub fn scroll_offset(selected: usize, viewport: usize, total: usize) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    let last = total - viewport;
    selected.saturating_sub(viewport / 2).min(last)
}
```

`MIN_WIDTH` keeps its value and gains a sentence saying it is the **table's**
floor, not the terminal's.

### Step 2.4 — GREEN: `view/mod.rs`

```rust
/// The narrowest terminal the dashboard draws into.
///
/// The table's own floor ([`flock::MIN_WIDTH`], 31) plus the selection
/// marker's gutter ([`flock::GUTTER`], 2). Below this the whole draw becomes
/// two short lines saying so.
pub const MIN_TERM_WIDTH: u16 = flock::MIN_WIDTH + flock::GUTTER;
```

`draw` refuses on `width < MIN_TERM_WIDTH`, and its second line becomes
`format!("need {MIN_TERM_WIDTH}x{MIN_HEIGHT}")` — nine characters, exactly as
before, so the existing argument about the refusal fitting inside the terminal
it is refusing about still holds and the `too_narrow` scene at 28 columns is
still whole.

Header, rule and rows move right:

```rust
    let table_width = width - flock::GUTTER;   // width >= MIN_TERM_WIDTH, checked above
    let columns = flock::columns_for(table_width);
    buffer.set_line(
        area.x + flock::GUTTER,
        y,
        &flock::header_line(columns, table_width, palette.muted()),
        table_width,
    );
```

and each data row writes its marker first:

```rust
        for (slot, row) in rows.iter().skip(offset).take(viewport).enumerate() {
            let slot = u16::try_from(slot).unwrap_or(0);
            let selected = app.selected() == Some(row.info.id);
            buffer.set_line(
                area.x,
                y + slot,
                &Line::from(Span::raw(flock::mark(selected))),
                1,
            );
            buffer.set_line(
                area.x + flock::GUTTER,
                y + slot,
                &flock::row_line(app, row, columns, table_width),
                table_width,
            );
        }
```

The `rule_line` under the header stays full width — it is chrome, and a rule
that stopped two columns short of the left edge would look like a rendering
bug. The empty-flock sentence stays at `area.x`.

The offset call becomes:

```rust
        let offset = flock::scroll_offset(
            app.selected_index().unwrap_or(0),
            viewport,
            rows.len(),
        );
```

### Step 2.5 — verify

```bash
cargo test -p shep-cli --bins --all-features                 # 8 snapshot failures expected
cargo insta accept --workspace                               # or review each with `cargo insta review`
cargo fmt --all --check
cargo test -p shep-cli --bins --all-features                 # 388 passed; 0 failed; 2 ignored
grep -rn '33x6' crates/ | wc -l                              # was 0; now 5
grep -rn '31x6' crates/ | wc -l                              # was 5; now 0
```

388 is 386 + 3 − 1. Still the inner loop only, not the full task gate.

Then read one accepted snapshot by eye and confirm the two-column shift is
present and uniform:

```bash
head -6 crates/shep-cli/src/lookout/snapshots/shep__lookout__frames__tests__healthy_wide.snap
```

Every table line must now begin with two spaces (or `> `) and the title line
must not have moved — the title is chrome, not table.

### Step 2.6 — MUTATION

Drop the gutter from the header only:

```rust
    buffer.set_line(area.x, y, &flock::header_line(columns, table_width, palette.muted()), table_width);
```

Every one of the eight re-accepted snapshots must fail: the header row now sits
two columns left of the data under it. A mutation that reddens only the frame
pins and no unit test is exactly what the frame pins are for, and this one
proves they are load-bearing rather than decorative. Revert.

### Step 2.7 — second MUTATION

In `scroll_offset`, drop the `.min(last)` clamp.
`the_offset_keeps_the_selection_visible_and_centred_where_it_can` must fail on
`scroll_offset(19, 5, 20)` — 17 rather than 15, which would leave two blank
rows below the last sheep. Revert.

---

## Task 3 — `lookout/source.rs`: the `Local` trait and the host sample

Everything lookout reads that does not come off the socket, behind one trait.

**Runs AFTER Task 5**, not before it. `Local::tail` returns
`super::tail::Tail` and `LocalReader::tail` calls `super::tail::read`; neither
the module nor the function exists until Task 5 creates them, so this task
cannot compile before it. The first draft of this plan had the arrow the other
way round.

**Files:** `crates/shep-cli/src/lookout/source.rs`.

**Expected delta:** +2 tests. The first draft said three; one of them could not
fail and is not written — see the end of Step 3.2.

### Step 3.1 — baseline

```bash
grep -rn 'sysinfo' crates/shep-cli/src/lookout/ | wc -l        # 0
grep -c '^\[\[package\]\]' Cargo.lock                          # RECORD IT — see below
cargo tree -p shep-cli --all-features 2>/dev/null | grep -c 'sysinfo v'   # ≥ 1 — record the exact number
```

Both counts are **recorded, not asserted against a literal**. `sysinfo` is
already an unconditional shep-cli dependency (`dog::metrics`' `shep_host_*`
series), so what this task has to prove is that **neither number moves across
this phase** — which is a delta, and a delta needs a before as well as an
after. Write both into the task report; Step 10.5 compares against them.

A literal would be worse than useless here: the count was `326` at `ed09740`
and `340` in the working tree while this plan was revised, because Phase 13
(`whistle`) is adding dependencies in a parallel worktree. An executor who
stopped on `326 != 340` would be stopping on somebody else's phase.

### Step 3.2 — RED

```rust
    /// fails if the host sampler starts walking the process table. That walk
    /// is what makes `dog::metrics`' own `sample_host` expensive enough that
    /// shep-daemon's memory sampler runs at fifteen seconds; this one runs on
    /// a one-second heartbeat and must stay a memory read and a load average.
    ///
    /// Asserted through the wall clock rather than by inspecting the
    /// `RefreshKind`, because the `RefreshKind` is exactly what a regression
    /// would change and asserting on it would be asserting the code says what
    /// it says. Fifty milliseconds is two orders of magnitude above a memory
    /// read and an order below a process-table walk on a loaded host.
    #[test]
    fn a_host_sample_is_cheap_enough_for_a_one_second_heartbeat() {
        let mut local = LocalReader::new();
        // One warm sample first: the first `System` construction pays for
        // whatever the platform caches, and this test is about the steady
        // state the heartbeat actually runs in.
        let _ = local.host();

        let started = std::time::Instant::now();
        for _ in 0..10 {
            let _ = local.host();
        }
        let each = started.elapsed() / 10;
        assert!(
            each < std::time::Duration::from_millis(50),
            "one host sample took {each:?}; the heartbeat fires every second"
        );
    }

    /// fails if the sampler starts inventing numbers on a platform sysinfo
    /// does not support. `None` is a real, expected case — `dog::metrics`'
    /// `Reading::host` says so in its own doc — and a strip rendering an
    /// unsupported platform as `0.00 load, 0 bytes` would be a lie the
    /// operator has no way to detect.
    ///
    /// Branched on `IS_SUPPORTED_SYSTEM` rather than asserting it.
    /// `sysinfo::IS_SUPPORTED_SYSTEM` is a `const bool`, and both
    /// `assert!(IS_SUPPORTED_SYSTEM)` and its negation trip
    /// `clippy::assertions_on_constants`, which is on by default and denied by
    /// the gate's `-D warnings`. The workspace carries no `allow` for it.
    #[test]
    fn an_unsupported_platform_reports_nothing_rather_than_zero() {
        let mut local = LocalReader::new();
        if sysinfo::IS_SUPPORTED_SYSTEM {
            let sample = local.host().expect("a supported platform samples");
            assert!(sample.memory_total_bytes > 0, "a supported host has memory");
            // `<`, not `<=`: see Step 3.6 — the weaker form survives a
            // mutation that reports used == total, which is the whole point of
            // running the mutation.
            assert!(sample.memory_used_bytes < sample.memory_total_bytes);
            assert!(sample.cores.is_some_and(|cores| cores >= 1));
        } else {
            assert!(local.host().is_none(), "no numbers where there is nothing to read");
        }
    }

    /// fails if the load average's denominator stops coming from std.
    ///
    /// `sysinfo` can also report a CPU count, from its own cpu list, and it is
    /// the obvious thing to reach for while writing a `sysinfo` sampler — but
    /// the two can disagree (an affinity mask, a cgroup quota), and the number
    /// this strip needs is the one the load average is actually spread across.
    #[test]
    fn the_core_count_comes_from_std_and_not_from_sysinfo() {
        let mut local = LocalReader::new();
        assert_eq!(
            local.host().and_then(|sample| sample.cores),
            std::thread::available_parallelism().ok().map(NonZeroUsize::get),
        );
    }
```

**`the_core_count_is_read_once_and_then_carried` is deliberately not here**,
and it was in the first draft. It asserted that two consecutive samples return the
same `cores`, and that a fresh `LocalReader` agrees — which is true whether the
value is cached in the struct or re-read on every call, because
`available_parallelism()` returns the same number every time it is asked. It
could not fail for the reason it existed, which is the shape this plan's own
"three shapes a dead check takes" section is about.

The caching is real and worth doing; it is simply not observable from outside
the type, so it is a code-review property and this plan says so rather than
pretending a test covers it. What IS observable is *which* number ended up
there, which is the test above.

### Step 3.3 — GREEN

```rust
/// One reading of the machine this lookout is running on.
///
/// Deliberately NOT `dog::metrics::HostReading`, whose shape this overlaps.
/// That one carries a host **process count** — which costs a process-table
/// walk — and no load average; this one is the other way round, because it is
/// read on a one-second heartbeat rather than once per Prometheus scrape.
/// `source`'s own doc already records making this call for `EventSource`, in
/// these words: the repetition here is of shape, not of meaning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostSample {
    /// One-, five- and fifteen-minute load averages.
    pub load: (f64, f64, f64),
    /// How many cores that load is spread across, from
    /// `std::thread::available_parallelism`. `None` when the platform would
    /// not say — a load average with no denominator is a number nobody can
    /// read, so the strip drops the whole segment rather than guessing 1.
    pub cores: Option<usize>,
    /// Total physical memory in bytes.
    pub memory_total_bytes: u64,
    /// Memory in use, as the platform reports it.
    pub memory_used_bytes: u64,
    /// Seconds since the host booted.
    pub uptime_seconds: u64,
}

/// Everything lookout reads that does not come off the socket.
///
/// One trait rather than two, because both methods answer the same question —
/// what can this process see without asking the shepherd — and because
/// `super::run_ui` already carries two generic parameters.
///
/// `&mut self` rather than `&self`: the tail reader remembers each file's
/// length at the previous read, which is what makes the gap notice exact, and
/// `run_ui` owns this outright. That is the opposite call from
/// [`FlockSource::flock`], which is `&self` precisely because
/// [`super::link::run_connected`] holds it across a `select!` with an
/// [`EventSource`] borrowed mutably.
pub trait Local {
    /// This machine's load, memory and uptime, or `None` on a platform
    /// `sysinfo` does not support.
    fn host(&mut self) -> Option<HostSample>;

    /// The tail of one sheep's two log files. See [`super::tail`].
    fn tail(&mut self, out: Option<&Path>, err: Option<&Path>) -> super::tail::Tail;
}

/// The real one: a `sysinfo` handle and the tail reader's memory of each
/// file's length.
#[derive(Debug)]
pub struct LocalReader {
    cores: Option<usize>,
    seen: std::collections::BTreeMap<PathBuf, u64>,
}

impl LocalReader {
    /// Reads the core count once — it does not change for the life of a
    /// process — and starts with no memory of any log file.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cores: std::thread::available_parallelism().ok().map(NonZeroUsize::get),
            seen: std::collections::BTreeMap::new(),
        }
    }
}

/// Same as [`LocalReader::new`].
///
/// `clippy::new_without_default` is on by default and the gate denies
/// warnings: an argument-less `new` with no `Default` fails it.
/// [`super::term::RestoreGuard`] carries this impl and this sentence for the
/// same reason — the repetition is the lint's, not this module's.
impl Default for LocalReader {
    fn default() -> Self {
        Self::new()
    }
}

impl Local for LocalReader {
    fn host(&mut self) -> Option<HostSample> {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return None;
        }
        // Memory only. `.with_processes(..)` is what makes `dog::metrics`'
        // own sampler a process-table walk, and this runs every second.
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
        );
        let load = System::load_average();
        Some(HostSample {
            load: (load.one, load.five, load.fifteen),
            cores: self.cores,
            memory_total_bytes: system.total_memory(),
            memory_used_bytes: system.used_memory(),
            uptime_seconds: System::uptime(),
        })
    }

    fn tail(&mut self, out: Option<&Path>, err: Option<&Path>) -> super::tail::Tail {
        super::tail::read(&mut self.seen, out, err)
    }
}
```

`TOPICS`' comment is rewritten. It currently says the feed is 12b; it now says
what the feed does instead:

```rust
/// **Not `log.*`, and that is now a shipped decision rather than a deferral.**
/// The bleats feed reads the selected sheep's log files from disk
/// ([`super::tail`]) rather than subscribing. Subscribing would make lookout
/// the highest-volume subscriber on the bus, and `super::link::run_connected`
/// answers a lag or a drop with an immediate `ListFlock` — so log traffic
/// would convert into request load on the shepherd at exactly the moment the
/// shepherd is busiest. The phase plan for 12b has the full accounting,
/// including what reading files costs instead.
pub const TOPICS: &[&str] = &["process.*", "daemon.*"];
```

### Step 3.4 — verify

The imports this task adds to `source.rs`, spelled out because three of them
are easy to miss and two are re-exports rather than the crates they look like:

```rust
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use sysinfo::{MemoryRefreshKind, RefreshKind, System};
```

```bash
cargo fmt --all --check
cargo test -p shep-cli --bins --all-features                             # 398 passed
grep -c '^\[\[package\]\]' Cargo.lock                                   # unchanged from Step 3.1
cargo tree -p shep-cli --all-features 2>/dev/null | grep -c 'sysinfo v'  # unchanged from Step 3.1
```

398 is 396 + 2 — Task 5 has already run, so the running count comes from it
and not from Task 2. Inner loop only; the full gate runs at Task 8.

### Step 3.5 — MUTATION

Change `host()` to build its `System` with
`RefreshKind::nothing().with_processes(ProcessRefreshKind::everything())`
instead of `.with_memory(..)`.
`a_host_sample_is_cheap_enough_for_a_one_second_heartbeat` must fail on a
machine with a normal process table. If it does not — a very idle CI box could
in principle walk a tiny table in under 50 ms — say so in the report and lower
the bound to the point where it does redden, rather than leaving a check that
cannot fail.

### Step 3.6 — second MUTATION

Return `memory_used_bytes: system.total_memory()`.
`an_unsupported_platform_reports_nothing_rather_than_zero` must fail on its
`used <= total`… it will not, since equal satisfies `<=`. **That is the point
of running the mutation**: strengthen the assertion to
`sample.memory_used_bytes < sample.memory_total_bytes` (no real host uses every
byte it has, and if one does, the strip is the least of its problems), re-run,
and confirm the mutation now reddens. A mutation that reveals a weak assertion
is a mutation that did its job.


---

## Shared test fixtures — `lookout/view/fixtures.rs`

Three pane test modules and Task 8's layout sweep need the same dozen
`App`-building helpers. **The first draft of this plan used fifteen of them and
defined none**, which is the Phase 11 failure this project has already paid for
once — twenty-five test bodies written against helpers that did not exist.
They are written out here, once, with the task that lands each block named.

`#[cfg(test)] mod fixtures;` in `view/mod.rs`, not a plain `pub mod` — same
reason `lookout::frames` is gated that way: shep-cli is `[[bin]]`-only, so
anything reached only from tests is `dead_code` in a normal build.

**Every helper drives a real `App` through `App::update`.** `App`'s fields are
private and stay that way. A fixture that reached past the reducer could build
states the reducer cannot produce, and a pane test that passes on an impossible
state proves nothing about the pane.

### Landed by Task 4

```rust
//! Fixtures the pane test modules share. See the phase plan for why they live
//! in one file rather than three.

use std::ffi::OsStr;
use std::time::Instant;

use ratatui::text::Line;
use shep_core::protocol::{Lamb, ProcessInfo};
use shep_core::status::ProcStatus;

use super::super::app::{App, Control, KeyPress, Msg};
use super::super::source::HostSample;
use super::super::tail::{Stream, Tail, TailLine};
use super::super::theme::Palette;

/// No colour at all: the palette every fixture uses unless the test is about
/// colour.
pub fn plain() -> Palette {
    Palette::detect(None, None, None)
}

/// The 256-colour palette, for the two tests that assert on a specific
/// foreground.
pub fn coloured() -> Palette {
    Palette::detect(None, Some(OsStr::new("xterm-256color")), None)
}

/// A dashboard with `flock` listed and nothing else applied.
pub fn app_with(flock: Vec<ProcessInfo>, palette: Palette) -> App {
    let t0 = Instant::now();
    let mut app = App::new(palette, Control::ReadOnly, "/home/rin/.shep".to_string(), t0);
    app.update(Msg::Snapshot { rows: flock, at: t0 });
    app
}

/// `count` online sheep, the first `with_readings` of which report cpu and
/// memory. The rest report neither, which is the case the `-` assertions need.
pub fn flock_of(count: u32, with_readings: u32) -> Vec<ProcessInfo> {
    (0..count)
        .map(|id| {
            let reports = id < with_readings;
            ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online)
                .pid(Some(48_000 + id))
                .uptime_ms(4_512_000)
                .cpu_percent(reports.then_some(3.5))
                .memory_bytes(reports.then_some(182 << 20))
                .out_file(Some(format!("/home/rin/.shep/logs/sheep-{id}-out.log")))
                .err_file(Some(format!("/home/rin/.shep/logs/sheep-{id}-err.log")))
                .build()
        })
        .collect()
}

/// One plausible host reading: the same numbers the gallery's scenes use, so a
/// failure here and a frame Rin is looking at name the same figures.
pub fn sample() -> HostSample {
    HostSample {
        load: (2.31, 4.10, 3.88),
        cores: Some(10),
        memory_total_bytes: 32 << 30,
        memory_used_bytes: 12 * (1 << 30) + (410 << 20),
        uptime_seconds: 6 * 86_400 + 3 * 3_600,
    }
}

/// A dashboard that has had one host sample applied.
pub fn with_host(sample: HostSample, flock: Vec<ProcessInfo>) -> App {
    let mut app = app_with(flock, plain());
    app.update(Msg::Host { sample: Some(sample) });
    app
}

/// A dashboard with no host reading — **and the two ways there are of having
/// none are not the same state.**
///
/// `unsupported: true` applies `Msg::Host { sample: None }`, which is what a
/// platform `sysinfo` does not support produces; the reducer sets
/// `host_unsupported` from it and the strip says so.
/// `unsupported: false` applies **no `Msg::Host` at all**, which is the state
/// before the first heartbeat, and the strip says `not read yet` instead.
/// Passing a `None` sample for the second would produce the first, and the
/// test asserting the two sentences differ would pass by rendering one of them
/// twice.
pub fn with_host_none(flock: Vec<ProcessInfo>, unsupported: bool) -> App {
    let mut app = app_with(flock, plain());
    if unsupported {
        app.update(Msg::Host { sample: None });
    }
    app
}

/// One rendered line, styles discarded.
pub fn rendered(line: &Line<'static>) -> String {
    line.spans.iter().map(|span| span.content.as_ref()).collect()
}

/// Several rendered lines, newline-joined. Newline-joined and not
/// concatenated, so an assertion can anchor on a line boundary.
pub fn render_all(lines: &[Line<'static>]) -> String {
    lines.iter().map(rendered).collect::<Vec<_>>().join("\n")
}
```

### Landed by Task 6

```rust
/// One tail line.
pub fn line(stream: Stream, text: &str) -> TailLine {
    TailLine { stream, text: text.to_string() }
}

/// A three-sheep dashboard whose feed is `tail`, with `web` selected.
///
/// Through `Msg::Bleats` rather than by writing `App::feed`: the field is
/// private, and a fixture that set it directly could produce a feed the
/// reducer would never have accepted.
pub fn with_feed(tail: Tail) -> App {
    with_feed_and_palette(tail, plain())
}

/// The same, at a given palette.
pub fn with_feed_and_palette(tail: Tail, palette: Palette) -> App {
    let mut app = app_with(flock_of(3, 3), palette);
    app.update(Msg::Bleats { tail });
    app
}

/// The same, with the selection moved onto sheep `index` — `1` is `sheep-1`.
///
/// The selection is moved BEFORE the tail is applied, because moving it is
/// what asks for a refresh in the real loop and applying the tail is the
/// answer to that.
pub fn with_feed_and_selection(tail: Tail, index: usize) -> App {
    let mut app = app_with(flock_of(3, 3), plain());
    for _ in 0..index {
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    app.update(Msg::Bleats { tail });
    app
}

/// An empty flock: nothing is selected, and nothing can be.
pub fn with_no_selection() -> App {
    app_with(Vec::new(), plain())
}
```

### Landed by Task 7

```rust
/// A TWO-sheep dashboard with the selection on `info`, which sorts second.
///
/// Two, and the selection not on row 0, on purpose: Task 7's first mutation
/// replaces `selected_row()` with `rows().first()`, and a one-sheep fixture
/// would make that mutation invisible.
pub fn with_selection(info: ProcessInfo) -> App {
    with_selection_and_palette(info, plain())
}

/// The same, at a given palette.
pub fn with_selection_and_palette(info: ProcessInfo, palette: Palette) -> App {
    assert!(info.id > 0, "the decoy takes id 0, so the sheep under test cannot");
    let decoy = ProcessInfo::builder(0, "decoy", ProcStatus::Online).build();
    let mut app = app_with(vec![decoy, info], palette);
    app.update(Msg::Key(KeyPress::SelectDown));
    app
}

/// A sheep whose listing DID carry lambs.
///
/// `ListFlock` never populates this field — that is the whole of design
/// decision 4 — so this fixture is deliberately impossible. The pane must not
/// mention lambs even when handed some, because the failure being guarded is a
/// heading or a caption promising a list, not a missing `if let`.
pub fn sheep_with_lambs() -> ProcessInfo {
    ProcessInfo::builder(9, "gateway", ProcStatus::Online)
        .pid(Some(48_301))
        .lambs(Some(vec![Lamb::new(48_302, "node"), Lamb::new(48_303, "sh")]))
        .build()
}
```

### Landed by Task 8

```rust
/// Twelve sheep, a host sample and a feed: everything on screen at once, for
/// the tests that are about the LAYOUT rather than about any one pane.
pub fn full_app() -> App {
    let mut app = with_host(sample(), flock_of(12, 8));
    app.update(Msg::Key(KeyPress::SelectDown));
    app.update(Msg::Bleats {
        tail: Tail {
            lines: (0..12)
                .map(|n| line(Stream::Out, &format!("GET /healthz 200 {n}ms")))
                .collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 4_096,
            note: None,
        },
    });
    app
}
```

---

## Task 4 — `lookout/view/host.rs`: the host-usage strip

One line, five self-labelled segments, truncated by the same `fit` every other
line on this screen goes through.

**Files:** new `crates/shep-cli/src/lookout/view/host.rs`; new
`crates/shep-cli/src/lookout/view/fixtures.rs` (see "Shared test fixtures");
`crates/shep-cli/src/lookout/app.rs` (the `Msg::Host` variant, its arm, and
two accessors); `crates/shep-cli/src/lookout/view/mod.rs` (two `mod` lines
only — the layout is Task 8).

**Expected delta:** +5 tests — four in `view/host.rs`, one in `app.rs`.

### Step 4.1 — baseline

```bash
find crates/shep-cli/src/lookout/view -name '*.rs' | wc -l   # 3
grep -rn 'flock cpu' crates/ | wc -l                         # 0
grep -rn 'Msg::Host' crates/ | wc -l                         # 0
```

### Step 4.2 — RED

```rust
    /// fails if a segment stops saying whose number it is. A strip truncated
    /// by a narrow terminal must never leave a bare `mem 12.4G` beside a bare
    /// `mem 706.0M` — the two are the host's and the flock's, and an operator
    /// reading an incident cannot afford to guess which is which.
    #[test]
    fn every_segment_names_whose_number_it_is() {
        let app = with_host(sample(), flock_of(4, 1));
        let line = rendered(&strip_line(&app, 200));
        for segment in ["host  load", "host mem", "flock cpu", "flock mem", "up "] {
            assert!(line.contains(segment), "missing {segment:?} in {line:?}");
        }
    }

    /// fails if the strip stops truncating visibly, or if the load average
    /// stops being the segment that survives a narrow terminal.
    ///
    /// There is no drop loop and no width table. The segments are joined in
    /// the drop order — least useful last — and `flock::fit` truncates from
    /// the right, so truncating IS the drop order and this is the whole of the
    /// fitting behaviour. An earlier draft built a second mechanism for it and
    /// a test that walked every width from 200 down to 10 recording the order
    /// things vanished; Rin's ruling for this phase is "as plain as the flock
    /// table", and the ellipsis on every other line of the screen is the
    /// precedent. Three widths, not a hundred and ninety.
    #[test]
    fn a_narrow_strip_truncates_visibly_and_keeps_the_load_average() {
        let app = with_host(sample(), flock_of(4, 1));

        let narrow = rendered(&strip_line(&app, 40));
        assert!(narrow.starts_with("host  load"), "got {narrow:?}");
        assert!(narrow.ends_with('…'), "a truncation the operator can see: {narrow:?}");
        assert!(!narrow.contains("up "), "`up` is the first thing off the end");

        // At the floor the strip still says whose number it is quoting, which
        // is the reason every segment carries its own label.
        let floor = rendered(&strip_line(&app, MIN_TERM_WIDTH));
        assert!(floor.starts_with("host  load"), "got {floor:?}");

        // And where it fits, nothing is cut.
        let full = rendered(&strip_line(&app, 200));
        assert!(!full.contains('…'));
        assert!(full.contains("up 6d"), "the last segment is there: {full:?}");
    }

    /// fails if an unknown flock reading renders as zero. `ProcessInfo`'s own
    /// doc is explicit that `None` covers three cases — not running, under one
    /// sampling window, or a shepherd predating the field — and that a reader
    /// renders all three as unknown and never as zero. `0.0%` claims a
    /// measurement the shepherd never made.
    #[test]
    fn a_flock_with_no_readings_shows_a_dash_and_not_a_zero() {
        let app = with_host(sample(), vec![
            ProcessInfo::builder(1, "web", ProcStatus::Errored).build(),
        ]);
        let line = rendered(&strip_line(&app, 200));
        assert!(line.contains("flock cpu -"), "got {line:?}");
        assert!(line.contains("flock mem -"), "got {line:?}");
        assert!(!line.contains("0.0%"));
    }

    /// fails if an unsupported platform stops saying so. `None` from the
    /// sampler is a real case, and a strip that silently dropped its host half
    /// would look like a strip whose numbers had not arrived yet.
    #[test]
    fn an_unread_host_says_which_of_the_two_reasons_it_is() {
        let unsupported = with_host_none(flock_of(4, 1), true);
        assert!(
            rendered(&strip_line(&unsupported, 200))
                .contains("host  usage is not available on this platform")
        );

        let not_yet = with_host_none(flock_of(4, 1), false);
        assert!(rendered(&strip_line(&not_yet, 200)).contains("host  not read yet"));

        // Both keep the flock half, which lookout can always compute.
        assert!(rendered(&strip_line(&unsupported, 200)).contains("flock cpu"));
    }

```

**`the_strip_never_exceeds_the_width_it_was_given` is deliberately not here.**
The first draft carried it, and it could not fail: `strip_line` returns
`flock::fit(..)`, and `fit` returns **exactly** `width` characters in every
branch — it pads when short and truncates with `…` when long
(`view/flock.rs:195-209`). `line.chars().count() <= usize::from(width)` is
therefore true for any `segments` whatsoever, including one where the fitting
never happened at all, which is the one thing it claimed to be testing. The
property is real and it is `fit`'s, pinned by `fit`'s own tests in `flock.rs`.

And in `app.rs`'s `mod tests`:

```rust
    /// fails if a frozen dashboard accepts a host reading. The strip reads
    /// THIS machine, which lookout can still see after the shepherd dies — so
    /// this is the one pane that could keep ticking, and one line ticking over
    /// on a screen whose banner says the values are frozen as of 14:32:07 is a
    /// contradiction on the same frame.
    ///
    /// Asserted in the reducer, which is the single place the rule lives:
    /// `run_ui`'s heartbeat samples unconditionally and this arm decides
    /// whether the dashboard is allowed to believe it, exactly as
    /// `Msg::Tick` and the uptime clock already work.
    #[test]
    fn a_frozen_dashboard_ignores_a_host_sample() {
        let (mut app, _) = started();
        app.update(Msg::Host { sample: Some(HostSample {
            load: (2.31, 4.10, 3.88),
            cores: Some(10),
            memory_total_bytes: 32 << 30,
            memory_used_bytes: 12 << 30,
            uptime_seconds: 600,
        })});
        assert!(app.host().is_some(), "a live dashboard takes the sample");

        app.update(Msg::Frozen { at_local: "2026-08-14 14:32:07".to_string() });
        let frozen = app.host();
        assert_eq!(app.update(Msg::Host { sample: None }), Effect::None);
        assert_eq!(app.host(), frozen, "the last values stay, unchanged");
        assert!(!app.host_unsupported(), "and a refused sample changes no flag");
    }
```

### Step 4.3 — GREEN: the reducer's half

**`Msg` grows a variant, and the variant needs writing out.** The workspace
denies `missing_docs` and every existing `Msg` variant documents each of its
fields; the first draft of this plan wrote only the match arm, which is a deny
on first build.

```rust
    /// One reading of the machine this lookout is running on, off the 1-second
    /// heartbeat. `None` means `sysinfo` does not support this platform —
    /// which is a real, expected case, not a failure.
    ///
    /// Refused once the link is lost: see this arm in [`App::update`].
    Host {
        /// What the sampler saw, or `None` on an unsupported platform.
        sample: Option<super::source::HostSample>,
    },
```

`App` gains two fields and two accessors:

```rust
    /// The last host reading, or `None` before the first heartbeat and on a
    /// platform `sysinfo` does not support. [`Self::host_unsupported`] tells
    /// the strip which of the two it is looking at.
    host: Option<HostSample>,
    /// True once a sample has come back `None` — the two `None`s mean
    /// different things and the strip says different sentences for them.
    host_unsupported: bool,
```

```rust
    /// The last host reading, or `None` if there has not been one.
    #[must_use]
    pub fn host(&self) -> Option<HostSample> {
        self.host
    }

    /// Whether [`Self::host`] is `None` because the platform cannot be read,
    /// rather than because no heartbeat has fired yet.
    ///
    /// The strip says `usage is not available on this platform` for the first
    /// and `not read yet` for the second. They are different facts and an
    /// operator seeing the wrong one waits for numbers that are never coming.
    #[must_use]
    pub fn host_unsupported(&self) -> bool {
        self.host_unsupported
    }
```

```rust
            Msg::Host { sample } => {
                // The one line that keeps a frozen dashboard honest, for the
                // second time. `Msg::Tick` stops advancing `now` once the link
                // is lost; this stops the strip for the same reason. A single
                // line ticking over on a screen whose banner says the values
                // are frozen is a contradiction on one frame.
                if matches!(self.link, Link::Lost { .. }) {
                    return Effect::None;
                }
                self.host_unsupported = sample.is_none();
                self.host = sample;
                Effect::None
            }
```

### Step 4.4 — GREEN: `view/host.rs`

```rust
//! The host-usage strip: one line, five self-labelled segments.
//!
//! Half of it is read from this machine ([`super::super::source::HostSample`])
//! and half is summed from the flock the dashboard already holds. Every
//! segment names which half it belongs to, because a strip truncated by a
//! narrow terminal must never leave a bare `mem 12.4G` beside a bare
//! `mem 706.0M`.
//!
//! Segments are joined **in the drop order** — `up` last, then the flock's
//! memory, its CPU, the host's memory, with the load average first — and the
//! line is fitted with [`super::flock::fit`], the same call every other line
//! on this screen goes through. Truncating from the right therefore IS the
//! drop order, with no second mechanism to maintain, and the `…` says so the
//! way it does on every truncated name in the table above. See the phase
//! plan's design decision 9 for the machinery that was cut and why.

use ratatui::text::{Line, Span};

use super::super::app::App;
use crate::output::{human_bytes, human_duration};

/// The strip, fitted to `width`.
#[must_use]
pub fn strip_line(app: &App, width: u16) -> Line<'static> {
    // One `Span`, muted: nothing on this line is damage, and nothing on it is
    // a status word. Colour here would be decoration with no meaning behind
    // it, which is the one thing the palette module forbids.
    Line::from(Span::styled(
        super::flock::fit(&segments(app).join("   "), width),
        app.palette().muted(),
    ))
}

/// The segments, widest set first.
fn segments(app: &App) -> Vec<String> {
    let mut out = Vec::with_capacity(5);
    match app.host() {
        Some(host) => {
            let (one, five, fifteen) = host.load;
            out.push(match host.cores {
                Some(cores) => format!("host  load {one:.2} {five:.2} {fifteen:.2} / {cores} cores"),
                // No denominator: the numbers alone are not readable, so they
                // are shown without a claim about how many cores they are
                // spread over rather than with a guessed one.
                None => format!("host  load {one:.2} {five:.2} {fifteen:.2}"),
            });
            out.push(format!(
                "host mem {} / {}",
                human_bytes(host.memory_used_bytes),
                human_bytes(host.memory_total_bytes)
            ));
        }
        None if app.host_unsupported() => {
            out.push("host  usage is not available on this platform".to_string());
        }
        // Reachable for at most one redraw: `tokio::time::interval`'s first
        // tick is immediate, so the heartbeat samples before the second frame.
        // It still gets a sentence and a gallery scene — an untested string is
        // where this project's claims rot.
        None => out.push("host  not read yet".to_string()),
    }

    // Summed from the rows already on screen, never requested. `-` and not
    // `0.0%` when nothing reported: `ProcessInfo::cpu_percent`'s own doc is
    // explicit that `None` is unknown, and rendering unknown as zero claims a
    // measurement the shepherd never made.
    let rows = app.rows();
    let cpu: Option<f32> = rows
        .iter()
        .filter_map(|row| row.info.cpu_percent)
        .fold(None, |sum, value| Some(sum.unwrap_or(0.0) + value));
    let mem: Option<u64> = rows
        .iter()
        .filter_map(|row| row.info.memory_bytes)
        .fold(None, |sum, value| Some(sum.unwrap_or(0) + value));
    out.push(cpu.map_or_else(
        || "flock cpu -".to_string(),
        |cpu| format!("flock cpu {cpu:.1}%"),
    ));
    out.push(mem.map_or_else(
        || "flock mem -".to_string(),
        |mem| format!("flock mem {}", human_bytes(mem)),
    ));

    // Last, and therefore the first thing a narrow terminal loses: a host that
    // has been up six days explains nothing about right now.
    if let Some(host) = app.host() {
        out.push(format!("up {}", human_duration(host.uptime_seconds * 1_000)));
    }
    out
}
```

`human_duration` takes milliseconds, so `uptime_seconds * 1_000` is the
conversion — and `uptime_seconds` is a `u64` of seconds since boot, which
overflows `u64` milliseconds only after 584 million years.

The imports `view/host.rs`'s `mod tests` needs, since two of them come from
the parent module rather than from this one:

```rust
    use super::*;
    use super::super::MIN_TERM_WIDTH;
    use super::super::fixtures::{flock_of, rendered, sample, with_host, with_host_none};
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;
```

### Step 4.5 — verify

```bash
cargo fmt --all --check
cargo test -p shep-cli --bins --all-features        # 403 passed; 0 failed; 2 ignored
grep -rn 'Msg::Host' crates/ | wc -l                # was 0; now ≥ 4
```

403 is 398 + 5.

The strip is not on screen yet — `draw` does not call it until Task 8. That is
deliberate: it keeps the layout rewrite in one task instead of five.

**Do not run the full task gate here.** `host::strip_line` has no caller in
`main`'s tree until Task 8, and shep-cli is `[[bin]]`-only, so
`cargo clippy --workspace --all-targets --all-features -- -D warnings` fails on
`dead_code` for code that is correct. The first draft of this plan claimed
"clippy will not complain, because `view/host.rs` is reached from its own
tests" — `frames.rs`'s own module doc records that this is exactly backwards,
which is why that module is `#[cfg(test)]`. See "Where the gate runs".

### Step 4.6 — MUTATION

In the `Msg::Host` arm, delete the `Link::Lost` early return.
`a_frozen_dashboard_ignores_a_host_sample` must fail on
`assert_eq!(app.host(), frozen)`.

The first draft of this plan had no test here at all and said so — "nothing in
this task's own tests reddens, and that is the finding" — deferring the catch
to Task 9's frame comparison. Deferring a catch five tasks is how a gap gets
shipped, and a reducer rule is testable in the reducer. **Step 9.6 still
re-runs this mutation at the frame level**, because the two catch different
things: this one proves the reducer refuses the sample, and that one proves the
strip is on screen and drawing from it. Revert.

### Step 4.7 — second MUTATION

In `segments`, replace the two `fold`s with `.sum()` (which yields `0` for an
empty iterator rather than `None`).
`a_flock_with_no_readings_shows_a_dash_and_not_a_zero` must fail on
`flock cpu -`. Revert.

---

## Task 5 — `lookout/tail.rs`: the bounded reader, and the gap it admits to

The answer to design decision 1, made concrete: a window from the end of each
file, a line cap on top of it, and an exact count of what was skipped.

**Runs immediately after Task 2 and before Task 3**, which needs this module
to exist.

**Files:** new `crates/shep-cli/src/lookout/tail.rs`;
`crates/shep-cli/src/lookout/mod.rs` (one `pub mod` line).

**Expected delta:** +8 tests.

### Step 5.1 — baseline

```bash
find crates/shep-cli/src/lookout -name '*.rs' | wc -l   # 11
grep -rn 'earlier lines not shown' crates/ | wc -l      # 0
grep -rn 'never read' crates/ | wc -l                   # 0
```

### The types

```rust
/// The most of one log file this pane will read to find its lines.
///
/// 64 KiB, a quarter of `commands::bleats`' own `TAIL_WINDOW_BYTES`: that path
/// shows fifty lines of a one-shot command, this one shows five lines of a
/// pane, and this read happens every two seconds for the life of the
/// dashboard rather than once. Two files at 64 KiB every two seconds is
/// 64 KiB/s of reads, whatever the flock writes.
pub const FEED_WINDOW_BYTES: u64 = 64 * 1024;

/// The most lines one file contributes, once the window is split.
///
/// A byte window alone cannot bound the line count — 64 KiB of newlines is
/// 65536 lines — and a line count alone cannot bound memory, since one
/// arbitrarily long line with no newline defeats it. Both bounds, for the same
/// reason `bleats` carries both.
pub const FEED_TAIL_LINES: usize = 40;

/// Which of a sheep's two output streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// stdout.
    Out,
    /// stderr. **Not an error.** Most runtimes log there by default, which is
    /// why the feed renders this tag muted rather than in `--bark` — that
    /// colour means errored, refused and destructive and nothing else.
    Err,
}

/// One line, with the stream it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailLine {
    /// Which file it was in.
    pub stream: Stream,
    /// The line, without its terminator. Decoded with
    /// [`String::from_utf8_lossy`]: a log file is whatever the child wrote and
    /// is under no obligation to be UTF-8, and refusing to show a log over one
    /// bad byte is the wrong failure. `bleats` makes and states the same call.
    pub text: String,
}

/// What one refresh of the feed found.
///
/// **Two miss counters, not one, and the reason is the whole of design
/// decision 1.** Lines go missing in three places: below the byte window
/// (unknowable in lines, exact in bytes), above [`FEED_TAIL_LINES`] inside the
/// window (exact), and above the five rows the pane has (exact, and the pane's
/// own to compute). The first draft of this plan counted only the first, which
/// is the RARE case; the ordinary case — a sheep writing thirty lines between
/// two polls, overrunning no window at all — went unreported, and the pane
/// looked complete exactly when the flock was busy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    /// The newest lines, oldest first — `out`'s tail, then `err`'s.
    ///
    /// **There is no merge, and that is stated rather than hidden.** A log
    /// line carries no timestamp, so there is no key to interleave the two
    /// files on, and guessing one from file order would be wrong exactly when
    /// a sheep writes to both at once. `bleats`' module doc records the same
    /// limitation for the same reason. The pane renders the LAST rows of this
    /// list, so a crash on stderr survives a chatty stdout — and its header
    /// says `out then err` rather than `out+err`, because `+` reads as one
    /// merged stream and this is two files end to end.
    pub lines: Vec<TailLine>,
    /// Lines this read **saw and discarded**, summed over both files.
    ///
    /// Those above [`FEED_TAIL_LINES`], plus one for the partial line a window
    /// boundary cut in half. Exact, and non-zero in the ordinary case: a
    /// window holds hundreds of lines and the cap keeps forty.
    ///
    /// The partial line is counted as **one** whether or not the boundary
    /// happened to fall exactly between two lines — the reader cannot tell
    /// without reading a byte it deliberately did not read, and over-counting
    /// by one is the safe direction. Claiming completeness is the failure this
    /// counter exists to prevent; being one pessimistic is not.
    pub missed_lines: usize,
    /// Bytes appended since the previous read that fell **below** the window
    /// and were therefore never read at all.
    ///
    /// Exact as a byte count. The number of LINES in them is genuinely
    /// unknowable — reading them is the thing the window exists to avoid — so
    /// the pane says "was never read" about these rather than putting a line
    /// count on them it would have to invent.
    ///
    /// Zero on the first read of a file: showing the tail of a file's history
    /// is not a gap *between two reads*, and a four-megabyte notice every time
    /// an operator selected a long-running sheep would train them to ignore
    /// the notice. Nothing is hidden by that, because the lines the window and
    /// the cap dropped are still in [`Self::missed_lines`]. Zero too when the
    /// file shrank, which is what a rotation or a `shep flush` looks like from
    /// here.
    pub missed_bytes: u64,
    /// Bytes this refresh actually pulled off disk, both files together.
    ///
    /// The bound design decision 3 claims, exposed so the tests can assert it
    /// **live** rather than argue it in a comment. Never above
    /// `2 * FEED_WINDOW_BYTES`, and — this is the part a static fixture cannot
    /// check — not above it even while the sheep is writing during the read.
    pub read_bytes: u64,
    /// Why there is nothing to show, when there is nothing to show.
    ///
    /// A sentence that names the CAUSE, not one that restates the fact. "the
    /// feed is empty" tells an operator nothing they cannot see; "this sheep
    /// has not written a log in this $SHEP_HOME" tells them whether to worry.
    pub note: Option<String>,
}
```

### Step 5.2 — RED

```rust
    /// fails if the reader stops being bounded by the READER rather than the
    /// writer. This is the property design decision 1 chose files over the bus
    /// for: a sheep writing four megabytes between two refreshes must cost one
    /// seek and one window, exactly as a silent sheep does.
    ///
    /// A live assertion, not a `tokio::time::timeout` — `read` is synchronous,
    /// so a timer around it would complete on the first poll and bound
    /// nothing. What can actually go wrong here is unbounded growth, so that
    /// is what is asserted — **on `read_bytes`, the quantity that would
    /// grow**, and not on the size of what came back. Forty short lines are
    /// under 64 KiB for any implementation whatsoever, including one that read
    /// the whole four megabytes and then threw them away; an assertion that
    /// cannot distinguish those two is not asserting the bound.
    #[test]
    fn a_four_megabyte_file_costs_one_window_and_forty_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        let line = "x".repeat(120);
        let mut body = String::new();
        while body.len() < 4 * 1024 * 1024 {
            body.push_str(&line);
            body.push('\n');
        }
        std::fs::write(&path, &body).unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&path), None);

        assert!(
            tail.read_bytes <= FEED_WINDOW_BYTES,
            "the reader pulled {} bytes off a 4 MiB file",
            tail.read_bytes
        );
        assert_eq!(tail.lines.len(), FEED_TAIL_LINES);
        assert_eq!(
            tail.missed_bytes, 0,
            "the first read of a file is not a gap BETWEEN READS"
        );
        // …and that is only defensible because the lines it did drop are
        // counted. Without this the pane draws five lines of four million and
        // says nothing.
        assert!(
            tail.missed_lines > 400,
            "a 64 KiB window of 121-byte lines holds ~540 of them and keeps 40; \
             counted only {}",
            tail.missed_lines
        );
    }

    /// fails if the reader stops counting the lines it discarded to honour
    /// [`FEED_TAIL_LINES`]. **This is the ordinary case the first draft of
    /// this plan missed entirely**, and it is the one that matters: sixty
    /// lines fit comfortably inside one 64 KiB window, so `missed_bytes` is
    /// zero and nothing about the byte accounting fires — while twenty of
    /// those lines are dropped by the cap. A pane handed a zero here draws
    /// five lines of sixty and looks complete, which it does exactly when the
    /// flock is busy and someone is watching.
    #[test]
    fn the_lines_the_cap_dropped_are_counted_even_when_no_bytes_were_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        let sixty: String = (0..60).map(|n| format!("line-{n}\n")).collect();
        assert!(
            sixty.len() < usize::try_from(FEED_WINDOW_BYTES).unwrap(),
            "the fixture has to sit well inside one window or it tests the wrong thing"
        );
        std::fs::write(&path, &sixty).unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&path), None);
        assert_eq!(tail.missed_bytes, 0, "nothing overran the window");
        assert_eq!(tail.lines.len(), FEED_TAIL_LINES);
        assert_eq!(tail.missed_lines, 20, "sixty in, forty kept");
        assert_eq!(tail.lines[0].text, "line-20", "and it is the NEWEST forty");
    }

    /// fails if the reader is bounded by the WRITER rather than by itself.
    ///
    /// `read_to_end` after a seek reads to the file's CURRENT end, not to the
    /// `len` that was just `stat`ed — so a sheep appending while the read is
    /// in flight makes both the read and its `Vec` grow past 64 KiB without
    /// limit, on the UI task. That is precisely the writer-bounded behaviour
    /// design decision 1 chose files over the bus to avoid, reintroduced by
    /// one missing call.
    ///
    /// **A static fixture cannot catch it**, however large: by the time the
    /// test runs the file has stopped growing, and `len` and the true end
    /// agree. So this one keeps a writer appending for the whole read.
    ///
    /// IR-46: bounded by construction — a fixed number of appends, a fixed
    /// number of reads, and the writer joined before the assertions. Nothing
    /// here waits on a condition, so it cannot hang whatever the reader does.
    #[test]
    fn a_file_that_grows_during_the_read_is_still_bounded_by_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        // A megabyte to start with, so every read takes the seek branch rather
        // than the whole-file one.
        std::fs::write(&path, "seed\n".repeat(200_000)).unwrap();

        let writing = path.clone();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&writing)
                .unwrap();
            let chunk = "w".repeat(64 * 1024 - 1);
            for _ in 0..512 {
                writeln!(file, "{chunk}").unwrap();
            }
        });

        let mut seen = BTreeMap::new();
        let mut worst = 0;
        for _ in 0..200 {
            worst = worst.max(read(&mut seen, Some(&path), None).read_bytes);
        }
        writer.join().unwrap();

        assert!(
            worst <= FEED_WINDOW_BYTES,
            "one read pulled {worst} bytes off a file that was still being written"
        );
        // And the writer actually ran: a green run on a file that never grew
        // would be this test passing for the wrong reason.
        assert!(
            std::fs::metadata(&path).unwrap().len() > 32 * 1024 * 1024,
            "the writer did not get far enough for this test to mean anything"
        );
    }

    /// fails if the gap notice stops being exact, or stops appearing at all.
    /// This is the answer to "what happens under a flock writing faster than
    /// the terminal can draw", and it is the half that has to be visible.
    #[test]
    fn a_file_that_grew_between_reads_reports_the_bytes_it_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let mut seen = BTreeMap::new();
        let first = read(&mut seen, Some(&path), None);
        assert_eq!(first.missed_bytes, 0);
        assert_eq!(first.lines.len(), 2);

        // Four megabytes of burst, then two lines the pane will actually show.
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let burst = "y".repeat(4 * 1024 * 1024);
        writeln!(file, "{burst}").unwrap();
        writeln!(file, "three").unwrap();
        writeln!(file, "four").unwrap();
        drop(file);

        let second = read(&mut seen, Some(&path), None);
        // `missed_bytes` is what was NEVER READ — everything appended below
        // the window. The last 64 KiB WAS read, so it is not in this number.
        // That upper bound is what distinguishes this definition from
        // `len - previous - covered`, which the first draft used and which
        // double-counted the lines the reader dropped: that form would return
        // slightly MORE than the whole burst and redden the second assertion.
        assert!(
            second.missed_bytes > 4 * 1024 * 1024 - 2 * FEED_WINDOW_BYTES,
            "got {}",
            second.missed_bytes
        );
        assert!(
            second.missed_bytes < 4 * 1024 * 1024,
            "the last window WAS read, so it does not belong in the gap: {}",
            second.missed_bytes
        );
        assert_eq!(second.lines.last().unwrap().text, "four", "the NEWEST lines survive");
        assert_eq!(
            second.missed_lines, 1,
            "the four-megabyte line the window cut in half is one line, counted"
        );

        // A third read with nothing appended reports no gap between reads —
        // but still reports the line the window cuts, because that is still
        // true, and a pane that stopped saying so would start claiming a
        // completeness it does not have.
        let third = read(&mut seen, Some(&path), None);
        assert_eq!(third.missed_bytes, 0);
        assert_eq!(third.missed_lines, 1);
    }

    /// fails if a file that SHRANK is reported as a gap. A rotation or a
    /// `shep flush` makes the file smaller between two reads, and a subtraction
    /// that wrapped would claim sixteen exabytes were skipped.
    #[test]
    fn a_truncated_file_reports_no_gap_and_re_reads_from_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        let mut seen = BTreeMap::new();
        let _ = read(&mut seen, Some(&path), None);

        std::fs::write(&path, "fresh\n").unwrap();
        let after = read(&mut seen, Some(&path), None);
        assert_eq!(after.missed_bytes, 0);
        assert_eq!(after.missed_lines, 0, "and nothing was dropped either");
        assert_eq!(after.lines.len(), 1);
        assert_eq!(after.lines[0].text, "fresh");
    }

    /// fails if a window boundary landing mid-line renders half a line as a
    /// whole one. `bleats::read_tail` makes the same discard for the same
    /// reason, and it is not cosmetic: half a log line shown as complete is a
    /// lie an operator will act on.
    #[test]
    fn a_window_boundary_discards_the_partial_line_it_lands_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-out.log");
        let filler = "z".repeat(usize::try_from(FEED_WINDOW_BYTES).unwrap());
        std::fs::write(&path, format!("{filler}PARTIAL-HEAD\nwhole-line\n")).unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&path), None);
        assert!(
            !tail.lines.iter().any(|l| l.text.contains("PARTIAL")),
            "a line cut by the window must be dropped, not shown: {:?}",
            tail.lines
        );
        assert_eq!(tail.lines.last().unwrap().text, "whole-line");
        assert_eq!(
            tail.missed_lines, 1,
            "dropped is not the same as hidden: the cut line is counted"
        );
    }

    /// fails if the two files stop being distinguishable, or if stderr stops
    /// coming last. The pane renders the LAST rows of this list, so `err`
    /// being last is what makes a crash survive a chatty stdout.
    #[test]
    fn both_streams_are_tagged_and_stderr_comes_last() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("web-out.log");
        let err = dir.path().join("web-err.log");
        std::fs::write(&out, "hello\n").unwrap();
        std::fs::write(&err, "panicked at 'boom'\n").unwrap();

        let mut seen = BTreeMap::new();
        let tail = read(&mut seen, Some(&out), Some(&err));
        assert_eq!(tail.lines[0], TailLine { stream: Stream::Out, text: "hello".to_string() });
        assert_eq!(
            tail.lines.last().unwrap(),
            &TailLine { stream: Stream::Err, text: "panicked at 'boom'".to_string() }
        );
        assert_eq!(tail.note, None, "there was something to show");
    }

    /// fails if an empty feed stops saying WHY it is empty. Three different
    /// causes, three different sentences — 12a shipped a caption claiming a
    /// sentence said why when it only stated the fact, and this is the same
    /// mistake one layer down.
    #[test]
    fn each_reason_the_feed_is_empty_gets_its_own_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen = BTreeMap::new();

        // The shepherd predates the field: no path at all.
        let unknown = read(&mut seen, None, None);
        assert!(unknown.lines.is_empty());
        assert!(
            unknown.note.as_deref().unwrap().contains("did not report a log path"),
            "got {:?}",
            unknown.note
        );

        // Never ran in this $SHEP_HOME: the shepherd creates both files at
        // spawn, so a missing file means exactly this.
        let missing = read(&mut seen, Some(&dir.path().join("nope.log")), None);
        assert!(
            missing.note.as_deref().unwrap().contains("has not written a log"),
            "got {:?}",
            missing.note
        );

        // Present but unreadable: a directory where a file should be.
        let as_dir = dir.path().join("a-directory.log");
        std::fs::create_dir(&as_dir).unwrap();
        let unreadable = read(&mut seen, Some(&as_dir), None);
        assert!(
            unreadable.note.as_deref().unwrap().contains("could not read"),
            "got {:?}",
            unreadable.note
        );

        // And an EXISTING, EMPTY file is not an error at all — a quiet sheep
        // is not a broken one.
        let quiet = dir.path().join("quiet.log");
        std::fs::write(&quiet, "").unwrap();
        let silent = read(&mut seen, Some(&quiet), None);
        assert!(silent.lines.is_empty());
        assert!(
            silent.note.as_deref().unwrap().contains("has written nothing"),
            "got {:?}",
            silent.note
        );

        // A FIFTH reason, and the one that is easy to miss: a file with
        // content but no newline anywhere in the last window. `lines` is empty
        // and `read_bytes` is 64 KiB, so "has written nothing yet" would be
        // flatly false — and false in the direction an operator acts on.
        let unterminated = dir.path().join("one-long-line.log");
        std::fs::write(
            &unterminated,
            "q".repeat(usize::try_from(FEED_WINDOW_BYTES).unwrap() + 10),
        )
        .unwrap();
        let long = read(&mut seen, Some(&unterminated), None);
        assert!(long.lines.is_empty());
        assert!(long.read_bytes > 0, "it read plenty; it just found no line");
        assert!(
            long.note.as_deref().unwrap().contains("no complete line"),
            "got {:?}",
            long.note
        );
    }
```

The imports these tests need, since three of them are easy to miss:

```rust
    use std::collections::BTreeMap;
    use std::io::Write as _;   // `writeln!` into a `File`

    use super::*;
```

### Step 5.3 — GREEN

```rust
/// One refresh: both files, tagged, with the gap admitted to.
///
/// `seen` is the caller's memory of each file's length at the previous read —
/// [`super::source::LocalReader`] owns it. It is threaded in rather than held
/// here because this function is otherwise pure over the filesystem, which is
/// what lets its tests drive it with a `BTreeMap` and a `tempdir` and no
/// dashboard at all.
///
/// `std::fs`, not `tokio::fs`: shep-cli's tokio does not carry the `fs`
/// feature, and this is a bounded read on a task that is otherwise asleep.
/// `commands::bleats` makes and states the same call.
pub fn read(
    seen: &mut BTreeMap<PathBuf, u64>,
    out: Option<&Path>,
    err: Option<&Path>,
) -> Tail {
    let mut tail = Tail::default();
    let mut notes: Vec<String> = Vec::new();

    if out.is_none() && err.is_none() {
        tail.note = Some(
            "the shepherd did not report a log path for this sheep".to_string(),
        );
        return tail;
    }

    for (stream, path) in [(Stream::Out, out), (Stream::Err, err)] {
        let Some(path) = path else { continue };
        match read_window(seen, path) {
            Ok(window) => {
                tail.read_bytes = tail.read_bytes.saturating_add(window.read_bytes);
                tail.missed_bytes = tail.missed_bytes.saturating_add(window.never_read);
                tail.missed_lines = tail.missed_lines.saturating_add(window.dropped);
                tail.lines.extend(
                    window.lines.into_iter().map(|text| TailLine { stream, text }),
                );
            }
            // The shepherd creates both files at spawn, so a missing one means
            // this sheep has never run in this `$SHEP_HOME` — a fact about the
            // flock, not a failure of the read.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => notes.push(format!(
                "this sheep has not written a log in this $SHEP_HOME ({})",
                path.display()
            )),
            Err(err) => notes.push(format!("could not read {}: {err}", path.display())),
        }
    }

    if tail.lines.is_empty() {
        tail.note = Some(if !notes.is_empty() {
            notes.join("; ")
        } else if tail.read_bytes > 0 {
            // Read plenty and found no terminator in any of it: one line
            // longer than the whole window. "has written nothing yet" would be
            // flatly false here, and false in the direction an operator acts
            // on — they would go looking for a sheep that was never started
            // instead of at a sheep writing one enormous line.
            format!(
                "this sheep's last {} of log contains no complete line",
                human_bytes(FEED_WINDOW_BYTES)
            )
        } else {
            "this sheep has written nothing yet".to_string()
        });
    }
    tail
}

/// What one file's window yielded.
///
/// A struct rather than a tuple: four returns, three of them counters that
/// differ only in what they count, is exactly the shape where positional
/// returns get transposed at the call site and nothing complains.
struct Window {
    /// The lines that survived both bounds, oldest first.
    lines: Vec<String>,
    /// Lines this read saw and discarded: those above [`FEED_TAIL_LINES`],
    /// plus one for the partial line the window boundary cut.
    dropped: usize,
    /// Bytes appended since the previous read that fell below the window and
    /// were never read.
    never_read: u64,
    /// Bytes this read pulled off disk. Never above [`FEED_WINDOW_BYTES`].
    read_bytes: u64,
}

/// One file's window: the last [`FEED_TAIL_LINES`] lines of it, what it had to
/// discard to get there, and what it never read at all.
///
/// # Errors
/// The file could not be opened, `stat`ed, seeked or read — notably
/// [`std::io::ErrorKind::NotFound`] and `EISDIR`, which [`read`] treats
/// differently from each other.
fn read_window(seen: &mut BTreeMap<PathBuf, u64>, path: &Path) -> std::io::Result<Window> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(FEED_WINDOW_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }

    // `.take(..)`, and this one call is what design decision 3's entire bound
    // rests on. `read_to_end` alone reads to the file's CURRENT end, not to
    // the `len` that was just `stat`ed — so a sheep appending while this read
    // is in flight makes both the read and this `Vec` grow without limit, on
    // the UI task. That is the writer-bounded behaviour decision 1 chose files
    // over the bus to avoid, reintroduced by one missing call, and no fixture
    // that has stopped growing can catch it.
    let mut window =
        Vec::with_capacity(usize::try_from(len.min(FEED_WINDOW_BYTES)).unwrap_or(0));
    (&mut file).take(FEED_WINDOW_BYTES).read_to_end(&mut window)?;
    let read_bytes = u64::try_from(window.len()).unwrap_or(u64::MAX);

    // A window boundary can land mid-line. Half a line shown as a whole one is
    // a lie, so the bytes up to and including the first newline are discarded
    // — and COUNTED, because a discarded line is one the pane is not showing.
    //
    // Counted as one whether or not the boundary happened to fall exactly
    // between two lines: telling those apart needs the byte at `start - 1`,
    // which is a byte this function deliberately did not read. Over-counting
    // by one is the safe direction; claiming completeness is not.
    let mut dropped = usize::from(start > 0);
    let bytes: &[u8] = if start > 0 {
        match window.iter().position(|&byte| byte == b'\n') {
            Some(newline) => &window[newline + 1..],
            // No newline in a whole window: it is all the middle of one line.
            None => &[],
        }
    } else {
        &window
    };

    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let keep_from = lines.len().saturating_sub(FEED_TAIL_LINES);
    dropped += keep_from;
    lines.drain(..keep_from);

    // Bytes that were NEVER READ: those appended since the previous read that
    // fell below the window. Not `len - previous - covered`, which the first
    // draft used: that form counts the bytes of the lines this reader saw and
    // dropped, which `dropped` already counts in lines, so the two
    // double-count each other and neither is exactly anything.
    //
    // `saturating_sub`, so a file that SHRANK — a rotation, a `shep flush` —
    // reports zero rather than sixteen exabytes.
    let previous = seen.insert(path.to_path_buf(), len);
    let never_read = match previous {
        // The first read of a file shows the tail of its history. That is not
        // a gap BETWEEN READS, and a four-megabyte notice every time an
        // operator selected a long-running sheep would train them to ignore
        // the notice. Nothing is hidden by this: the lines the window and the
        // cap dropped are in `dropped` either way.
        None => 0,
        Some(previous) => start.saturating_sub(previous),
    };
    Ok(Window { lines, dropped, never_read, read_bytes })
}
```

The imports, spelled out because `Read` is needed only for `take` and is easy
to leave out:

```rust
use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use crate::output::human_bytes;
```

### Step 5.4 — verify

```bash
cargo fmt --all --check
cargo test -p shep-cli --bins --all-features                  # 396 passed; 0 failed; 2 ignored
grep -rn 'earlier lines not shown' crates/ | wc -l            # still 0 — that string is Task 6's
```

396 is 388 + 8. Inner loop only: `tail::read` has no caller in `main`'s tree
until Task 6, so the full gate would fail on `dead_code` for code that is
correct. See "Where the gate runs".

### Step 5.5 — MUTATION: the byte gap

Delete the `seen` bookkeeping — always report nothing never-read:

```rust
    let _ = seen.insert(path.to_path_buf(), len);
    let never_read = 0;   // MUTATION
```

`a_file_that_grew_between_reads_reports_the_bytes_it_skipped` must fail on its
first `missed_bytes` assertion. Revert.

### Step 5.6 — second MUTATION: the line gap

Delete `dropped += keep_from` — count only the partial line.
`the_lines_the_cap_dropped_are_counted_even_when_no_bytes_were_skipped` must
fail (`20` becomes `0`), and so must
`a_four_megabyte_file_costs_one_window_and_forty_lines`' `missed_lines > 400`.

**This is the single most important mutation in the phase**, and it is the one
the first draft of this plan could not have run, because it had no counter to
delete. It turns the feed back into the version that shows five lines of sixty
and says nothing — the silent-truncation feed decision 1 rejected — in the
ORDINARY case, where no byte window is overrun and no other check on this
screen would notice. Revert.

### Step 5.7 — third MUTATION: the reader's bound

In `read_window`, drop the `.take(FEED_WINDOW_BYTES)`:

```rust
    file.read_to_end(&mut window)?;   // MUTATION
```

`a_file_that_grows_during_the_read_is_still_bounded_by_the_reader` must fail;
`a_four_megabyte_file_costs_one_window_and_forty_lines` must **still pass**,
which is the finding — a fixture that has stopped growing cannot tell the two
implementations apart, and that is why the growing one exists.

If the growing test does not redden, the writer is not getting scheduled
between the `stat` and the `read`. Raise the chunk count from 512 and re-run
rather than accepting a check that cannot fail; if it still will not redden on
this machine, say so in the task report with the number you got to. Revert.

### Step 5.8 — fourth MUTATION

In `read_window`, `lines.truncate(FEED_TAIL_LINES)` instead of
`lines.drain(..keep_from)` — keep the OLDEST lines rather than the newest.
`a_file_that_grew_between_reads_reports_the_bytes_it_skipped` must fail on
`assert_eq!(second.lines.last().unwrap().text, "four")`, and
`the_lines_the_cap_dropped_are_counted_even_when_no_bytes_were_skipped` on
`lines[0] == "line-20"`. Revert.

### Step 5.9 — fifth MUTATION

In `read_window`, drop the partial-line discard (always use `&window`).
`a_window_boundary_discards_the_partial_line_it_lands_in` must fail — a line
beginning `zzzz…PARTIAL-HEAD` appears. Revert.

---

## Task 6 — `lookout/view/bleats.rs`: the feed pane

**Files:** new `crates/shep-cli/src/lookout/view/bleats.rs`;
`crates/shep-cli/src/lookout/view/fixtures.rs` (the feed block);
`crates/shep-cli/src/lookout/app.rs` (the `Msg::Bleats` variant, its arm and
the accessor); `crates/shep-cli/src/lookout/mod.rs` — **three changes, not
one**: `run_ui` gains its `local: L` parameter, the `Effect::RefreshFeed` stub
Task 1 left becomes a coalesced read, **and the heartbeat arm starts producing
`Msg::Host`**; `crates/shep-cli/src/lookout/view/mod.rs` (the `pub mod` line).

The heartbeat is the one to read twice. Task 4 wrote `Msg::Host`, its reducer
arm and the strip; **nothing produced the message.** Every pane test and every
gallery frame injects it directly, so the whole phase would have gone green
with the shipped binary drawing `host  not read yet` forever, under a strip Rin
had approved from `frames.txt`. `run_ui` gains `local` here, so the sampling
belongs here.

**Expected delta:** +10 tests — seven in `view/bleats.rs`, one in `app.rs`,
two in `lookout/mod.rs`.

### Step 6.1 — baseline

```bash
grep -rn 'earlier lines not shown' crates/ | wc -l       # 0
grep -c 'Msg::Bleats' crates/shep-cli/src/lookout/mod.rs # 0
grep -c 'local.host()' crates/shep-cli/src/lookout/mod.rs # 0
grep -c 'feed_dirty' crates/shep-cli/src/lookout/mod.rs  # 0
grep -c 'Msg::Tick' crates/shep-cli/src/lookout/mod.rs   # 1 — the heartbeat arm, which this task changes
```

### Step 6.2 — RED

```rust
    /// fails if the BYTE half of the gap notice stops reaching the screen.
    /// Task 5 makes the number exact; this is the half that makes it visible,
    /// and without it the feed silently shows five lines of a four-megabyte
    /// burst.
    ///
    /// "was never read", not "is not shown": the pane cannot say how many
    /// lines are in those bytes — reading them is what the window exists to
    /// avoid — so it says what the reader DID rather than inventing a count.
    #[test]
    fn a_byte_gap_replaces_the_header_and_says_how_much_was_never_read() {
        let app = with_feed(Tail {
            lines: vec![line(Stream::Out, "still here")],
            missed_lines: 0,
            missed_bytes: 4_000_000,
            read_bytes: 65_536,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(
            rendered.contains("3.8M written before these lines was never read"),
            "got {rendered:?}"
        );
        // And the ordinary header is gone while the gap notice is up: two
        // header lines would cost one of the five content rows.
        assert!(!rendered.contains("re-read with each listing"));
    }

    /// fails if the pane claims completeness in the ORDINARY case. **This is
    /// the test the first draft of this plan did not have**, and its absence
    /// was the phase's worst defect: thirty lines fit inside one 64 KiB window
    /// with room to spare, so `missed_bytes` is zero and the byte notice never
    /// fires — while twenty-five of those thirty lines are not on screen. A
    /// feed that lies is worse than no feed, and it would have lied exactly
    /// when the flock was busy, which is when someone is watching it.
    #[test]
    fn a_pane_that_cannot_show_every_line_it_holds_says_how_many() {
        let app = with_feed(Tail {
            lines: (0..30).map(|n| line(Stream::Out, &format!("line-{n}"))).collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 4_096,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(rendered.contains("… 25 earlier lines not shown"), "got {rendered:?}");
        assert!(
            !rendered.contains("never read"),
            "no bytes were skipped, so nothing may claim any were: {rendered:?}"
        );
    }

    /// fails if the two kinds of loss get merged into one number. They are
    /// different facts: the reader COUNTED the lines it dropped, and it never
    /// looked at the bytes below the window at all. Adding an invented line
    /// count for the second, or dropping the first because the second is
    /// bigger, would both be the pane claiming to know something it does not.
    #[test]
    fn both_kinds_of_gap_are_named_separately_in_one_line() {
        let app = with_feed(Tail {
            lines: (0..30).map(|n| line(Stream::Out, &format!("line-{n}"))).collect(),
            missed_lines: 500,
            missed_bytes: 4_000_000,
            read_bytes: 131_072,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 200, 6));
        assert!(
            rendered.contains("… 525 earlier lines not shown, and 3.8M before them never read"),
            "500 the reader dropped plus 25 the pane has no room for: {rendered:?}"
        );
    }

    /// fails if the ordinary header stops naming the sheep, the streams, or
    /// the fact that this is a re-read rather than a live stream. An operator
    /// who reads this pane as `tail -f` will draw wrong conclusions from a
    /// two-second gap in a log, and the pane is the only place that can say
    /// so.
    ///
    /// `out then err`, not `out+err`. `+` reads as one merged stream, and this
    /// is two files rendered end to end with no interleaving at all — a log
    /// line carries no timestamp, so there is no key to merge on. A sheep with
    /// forty stdout lines and one old stderr line shows the stale stderr line
    /// UNDER the fresh stdout ones, and the header is the only place on screen
    /// that can say why.
    #[test]
    fn the_header_says_which_sheep_and_that_it_is_a_re_read() {
        let app = with_feed_and_selection(
            Tail {
                lines: vec![line(Stream::Out, "hello")],
                missed_lines: 0,
                missed_bytes: 0,
                read_bytes: 6,
                note: None,
            },
            1,
        );
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(rendered.contains("bleats  sheep-1"), "got {rendered:?}");
        assert!(rendered.contains("out then err"), "got {rendered:?}");
        assert!(!rendered.contains("out+err"), "`+` reads as a merge: {rendered:?}");
        assert!(rendered.contains("re-read with each listing"), "got {rendered:?}");
    }

    /// fails if the pane stops showing the NEWEST lines, **or stops saying
    /// that the older ones went**. A feed that scrolled off the bottom would
    /// show an operator the beginning of a burst and hide its end, which is
    /// the opposite of what a dashboard is for; a feed that showed the end and
    /// said nothing about the beginning would look complete, which is worse.
    ///
    /// The first draft of this test asserted only the ordering, and so
    /// certified the silence as correct.
    #[test]
    fn the_pane_shows_the_last_lines_that_fit_and_says_so() {
        let app = with_feed(Tail {
            lines: (0..40).map(|n| line(Stream::Out, &format!("line-{n}"))).collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 4_096,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        for n in 35..40 {
            assert!(rendered.contains(&format!("out  line-{n}")), "line-{n} is on screen");
        }
        assert!(!rendered.contains("out  line-34"), "and line-34 is not: {rendered:?}");
        assert!(
            rendered.contains("… 35 earlier lines not shown"),
            "and the pane says the other thirty-five went: {rendered:?}"
        );
    }

    /// fails if `err` stops being distinguishable from `out` by TEXT, or
    /// starts being `--bark` red. A sheep writing to stderr is not a sheep in
    /// trouble — most runtimes log there by default — and `--bark` means
    /// errored, refused and destructive and nothing else.
    #[test]
    fn the_stream_tag_is_a_word_and_stderr_is_not_bark() {
        let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let app = with_feed_and_palette(
            Tail {
                lines: vec![line(Stream::Out, "fine"), line(Stream::Err, "warning")],
                missed_lines: 0,
                missed_bytes: 0,
                read_bytes: 32,
                note: None,
            },
            palette,
        );
        let lines = feed_lines(&app, 120, 6);
        let rendered = render_all(&lines);
        assert!(rendered.contains("out  fine"));
        assert!(rendered.contains("err  warning"));
        let bark = palette.alarm().fg;
        for line in &lines {
            for span in &line.spans {
                assert_ne!(span.style.fg, bark, "nothing in this pane is bark: {span:?}");
            }
        }
    }

    /// fails if an empty feed stops saying why. Task 5 produces the sentence;
    /// this asserts it survives to the screen instead of being swallowed by a
    /// blank pane — the exact caption 12a got wrong, one layer up.
    #[test]
    fn an_empty_feed_prints_the_reason_rather_than_nothing() {
        let app = with_feed(Tail {
            lines: Vec::new(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 0,
            note: Some("this sheep has written nothing yet".to_string()),
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(rendered.contains("this sheep has written nothing yet"));

        // No sheep selected at all is a fourth reason, and it is the pane's
        // own to state — Task 5 never runs in that case.
        let empty = with_no_selection();
        let rendered = render_all(&feed_lines(&empty, 120, 6));
        assert!(rendered.contains("no sheep is selected"), "got {rendered:?}");
    }

    /// fails if `Msg::Bleats` starts asking for another refresh. A reducer
    /// that answered its own feed update with `Effect::RefreshFeed` would spin
    /// the UI task at full tilt, re-reading two files as fast as the loop can
    /// go — the one recursion this design can have.
    #[test]
    fn applying_a_tail_does_not_ask_for_another_one() {
        let (mut app, _) = started();
        assert_eq!(app.update(Msg::Bleats { tail: Tail::default() }), Effect::None);
    }
```

And in `lookout/mod.rs`'s `mod tests`, against a `FakeLocal` that reads nothing
and counts what it was asked for:

```rust
    /// A `Local` that touches no disk: a fixed sample, a fixed tail, and a
    /// count of each call. `Arc`, because `run_ui` takes the reader by value
    /// and only gives the terminal back.
    #[derive(Clone, Default)]
    struct FakeLocal {
        sample: Option<HostSample>,
        hosts: Arc<AtomicUsize>,
        tails: Arc<AtomicUsize>,
    }

    impl Local for FakeLocal {
        fn host(&mut self) -> Option<HostSample> {
            self.hosts.fetch_add(1, Ordering::Relaxed);
            self.sample
        }

        fn tail(&mut self, _out: Option<&Path>, _err: Option<&Path>) -> Tail {
            self.tails.fetch_add(1, Ordering::Relaxed);
            Tail::default()
        }
    }

    /// fails if **nothing ever produces a `Msg::Host`.** The strip renders
    /// from `App::host`, the reducer arm was written in Task 4, and every pane
    /// test and every gallery frame injects the message directly — so a
    /// heartbeat that still yielded only `Msg::Tick` leaves the shipped binary
    /// drawing `host  not read yet` forever with nothing red anywhere on the
    /// suite. That is what the first draft of this plan shipped, and it is
    /// invisible to every other check in the phase.
    ///
    /// Asserted on the READER rather than on a frame, because at this task the
    /// strip is not on screen yet — `draw` does not call it until Task 8, and
    /// Task 8 adds `a_heartbeat_puts_the_host_strip_on_the_frame` for the
    /// other half. What is testable here is the call, and the missing call is
    /// the bug.
    ///
    /// IR-46: bounded — a `timeout` around a genuinely async loop, and a quit
    /// queued on a timer, so it cannot hang.
    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_asks_the_local_reader_for_a_host_sample() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let local = FakeLocal::default();
        let hosts = Arc::clone(&local.hosts);

        // After the 1-second heartbeat, so the tick lands first.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            let _ = msg_tx.send(Msg::Key(KeyPress::Quit)).await;
        });

        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, stream::empty(), msg_rx, poll_tx, local),
        )
        .await
        .expect("the loop left within ten seconds");

        assert!(
            hosts.load(Ordering::Relaxed) >= 1,
            "the heartbeat fired and never sampled the host"
        );
    }

    /// fails if a burst of keypresses costs one two-file read per key.
    ///
    /// `input::map_key` drops `KeyEventKind::Repeat`, but ordinary terminals
    /// deliver auto-repeat as a stream of Press events — so a held `j` on a
    /// long flock is twenty to thirty moved selections a second, and an
    /// uncoalesced `Effect::RefreshFeed` would put a synchronous 128 KiB read
    /// behind every one of them, on the task that also owns the redraw. The
    /// read is coalesced onto the `MIN_REDRAW` gate for that reason, so a
    /// burst costs one read rather than twenty-one.
    ///
    /// `assert_eq!(1)` and not `<= 2`: the exact number is the property. One
    /// snapshot and twenty moves arrive with no time between them, so nothing
    /// is read until the clock passes `MIN_REDRAW`, and then it is read once.
    ///
    /// IR-46: bounded, same shape as the test above.
    #[tokio::test(start_paused = true)]
    async fn a_burst_of_selection_moves_costs_one_read_and_not_one_per_key() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let local = FakeLocal::default();
        let tails = Arc::clone(&local.tails);

        let at = Instant::now();
        msg_tx
            .send(Msg::Snapshot {
                rows: (0..8)
                    .map(|id| {
                        ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
                    })
                    .collect(),
                at,
            })
            .await
            .unwrap();
        for _ in 0..20 {
            msg_tx.send(Msg::Key(KeyPress::SelectDown)).await.unwrap();
        }
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            let _ = msg_tx.send(Msg::Key(KeyPress::Quit)).await;
        });

        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, stream::empty(), msg_rx, poll_tx, local),
        )
        .await
        .expect("the loop left within ten seconds");

        assert_eq!(
            tails.load(Ordering::Relaxed),
            1,
            "a snapshot and twenty selection moves must coalesce into one read"
        );
    }
```

### Step 6.3 — GREEN

**`Msg` grows its second variant of the phase**, and it needs writing out —
`missing_docs` is denied and every existing variant documents its fields:

```rust
    /// One refresh of the selected sheep's log files, in answer to an
    /// [`Effect::RefreshFeed`] this reducer asked for.
    ///
    /// Returns [`Effect::None`] unconditionally; see its arm.
    Bleats {
        /// What the read found, including what it could not show.
        tail: super::tail::Tail,
    },
```

`App` gains `feed: Tail` (defaulting to `Tail::default()`), a `feed()`
accessor:

```rust
    /// The selected sheep's most recent output, as of the last refresh.
    #[must_use]
    pub fn feed(&self) -> &Tail {
        &self.feed
    }
```

and the arm:

```rust
            // Always `Effect::None`. A reducer that answered its own feed
            // update with another refresh request would spin the UI task at
            // full tilt; the `let _ =` at the call site in `run_ui` is
            // deliberate rather than lazy.
            Msg::Bleats { tail } => {
                self.feed = tail;
                Effect::None
            }
```

`view/bleats.rs`:

```rust
/// The feed's lines: one header, then the newest lines that fit.
///
/// `rows` is how many lines the pane has, excluding its rule. The header takes
/// one of them, always — either the ordinary one naming the sheep and the
/// cadence, or the gap notice, which REPLACES it rather than sitting beside
/// it. Two header rows would cost a fifth of the pane.
#[must_use]
pub fn feed_lines(app: &App, width: u16, rows: usize) -> Vec<Line<'static>> {
    let palette = app.palette();
    let mut out = Vec::with_capacity(rows);
    let feed = app.feed();
    let body = rows.saturating_sub(1);

    // Lines go missing in three places and this is where two of them are
    // added up: the ones the READER discarded (above the forty-line cap, plus
    // the line the window boundary cut) and the ones this PANE holds and has
    // no room for. Both exact. The third — bytes below the window — cannot be
    // counted in lines at all and is reported separately, in bytes. See the
    // phase plan's design decision 1.
    let lost_lines = feed.missed_lines + feed.lines.len().saturating_sub(body);

    out.push(match app.selected_row() {
        None => Line::from(Span::styled(
            fit("bleats  no sheep is selected", width),
            palette.muted(),
        )),
        Some(row) => match gap_notice(lost_lines, feed.missed_bytes) {
            Some(notice) => Line::from(Span::styled(
                fit(&format!("bleats  {}  {notice}", row.info.name), width),
                // Attention, not alarm: a sheep writing faster than a
                // two-second poll is busy, not broken. `--bark` means errored,
                // refused and destructive.
                palette.attention(),
            )),
            // `out then err`, not `out+err`: `+` reads as one merged stream,
            // and there is no merge — a log line carries no timestamp, so
            // there is no key to interleave two files on. This header is the
            // only place on screen that can say so.
            None => Line::from(Span::styled(
                fit(
                    &format!(
                        "bleats  {}  out then err  from the log files, re-read with each listing",
                        row.info.name
                    ),
                    width,
                ),
                palette.muted(),
            )),
        },
    });

    if feed.lines.is_empty() {
        if let Some(note) = feed.note.as_deref() {
            out.push(Line::from(Span::styled(fit(note, width), palette.muted())));
        }
        return out;
    }
    // The LAST lines that fit: a feed that showed the beginning of a burst and
    // hid its end is the opposite of what a dashboard is for. `err` comes
    // after `out` in `Tail::lines`, so a crash on stderr survives a chatty
    // stdout for free.
    let skip = feed.lines.len().saturating_sub(body);
    for line in feed.lines.iter().skip(skip) {
        let tag = match line.stream {
            Stream::Out => "out",
            Stream::Err => "err",
        };
        out.push(Line::from(vec![
            // Muted, both of them. The word carries the whole meaning, and a
            // red `err` would say a stderr line is damage.
            Span::styled(format!("{tag}  "), palette.muted()),
            Span::raw(fit(&line.text, width.saturating_sub(5))),
        ]));
    }
    out
}

/// What the header says about what is not on screen, or `None` when
/// everything is.
///
/// **Two quantities, because they are two different facts, and merging them
/// would mean inventing one of them.** `lines` is exact — the reader counted
/// what it discarded and the pane counts what it has no room for. `bytes` is
/// exact as bytes and *unknowable as lines*: reading them is precisely what
/// the 64 KiB window exists to avoid. So the wording for the byte half says
/// what the reader DID — it never read them — rather than putting a line count
/// on them that nothing measured.
fn gap_notice(lines: usize, bytes: u64) -> Option<String> {
    match (lines, bytes) {
        (0, 0) => None,
        (0, bytes) => Some(format!(
            "… {} written before these lines was never read",
            human_bytes(bytes)
        )),
        (lines, 0) => Some(format!("… {}", earlier_lines(lines))),
        (lines, bytes) => Some(format!(
            "… {}, and {} before them never read",
            earlier_lines(lines),
            human_bytes(bytes)
        )),
    }
}

/// `1 earlier line` / `25 earlier lines`. A sentence with the wrong plural on
/// it reads as a rendering bug, and this one is on screen during an incident.
fn earlier_lines(count: usize) -> String {
    if count == 1 {
        "1 earlier line not shown".to_string()
    } else {
        format!("{count} earlier lines not shown")
    }
}
```

### Step 6.3b — GREEN: `run_ui`, in three places

**One.** The signature gains the reader, and the three call sites
(`mod.rs:191`, and the two in `mod tests`) pass one:

```rust
pub async fn run_ui<B: Backend, S, L>(
    mut app: App,
    mut terminal: Terminal<B>,
    events: S,
    mut msgs: mpsc::Receiver<Msg>,
    polls: mpsc::Sender<()>,
    mut local: L,
) -> Terminal<B>
where
    S: Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
    L: source::Local,
```

`lookout()` passes `source::LocalReader::new()`; the two existing tests pass a
`FakeLocal::default()`, which samples nothing and returns `Tail::default()`.

**Two.** `Effect::RefreshFeed` — the stub Task 1 left — becomes a *request*,
not a read. The read itself moves up to the redraw gate:

```rust
            // Not the read. `Effect::RefreshFeed` arrives once per moved
            // selection, and ordinary terminals deliver a held `j` as twenty
            // to thirty Press events a second — so doing the I/O here would
            // put a synchronous 128 KiB read behind every repeat, on the task
            // that also owns the redraw. Coalesced onto `MIN_REDRAW` below,
            // which is three lines and the same gate the draw already uses.
            Effect::RefreshFeed => {
                feed_dirty = true;
                dirty = true;
            }
```

and at the top of the loop, where `MIN_REDRAW` already lives:

```rust
        // One gate, read once, so the feed cannot be refreshed on a frame that
        // is not about to be drawn — and, more importantly, is refreshed
        // BEFORE the frame that shows it rather than after.
        let may_draw = last_draw.is_none_or(|at| at.elapsed() >= MIN_REDRAW);
        if feed_dirty && may_draw {
            // The paths are cloned out before `app` is borrowed mutably.
            let (out, err) = app.selected_row().map_or((None, None), |row| {
                (row.info.out_file.clone(), row.info.err_file.clone())
            });
            let tail = local.tail(
                out.as_deref().map(Path::new),
                err.as_deref().map(Path::new),
            );
            // `let _`: `Msg::Bleats` returns `Effect::None` by construction —
            // see its arm in the reducer — and acting on a returned effect
            // here would be the one place this design could recurse.
            let _ = app.update(Msg::Bleats { tail });
            feed_dirty = false;
            dirty = true;
        }
        if dirty && may_draw {
            let _ = terminal.draw(|frame| view::draw(&app, frame));
            dirty = false;
            last_draw = Some(Instant::now());
        }
```

with `let mut feed_dirty = false;` beside `let mut dirty = true;`.

**Three — and this is the one the first draft of this plan left out
entirely.** The heartbeat arm starts producing the host sample:

```rust
            _ = heartbeat.tick() => {
                // The host sample rides this arm rather than adding a fifth
                // one. The `biased` select's arm-retirement reasoning is the
                // subtlest thing in this module and this phase deliberately
                // does not re-derive it; sampling memory and a load average is
                // microseconds and no process-table walk.
                //
                // Sampled unconditionally, and REFUSED by the reducer once the
                // link is lost — one enforcement point, the same division
                // `Msg::Tick` and the uptime clock already use. `let _`:
                // `Msg::Host` returns `Effect::None` by construction.
                let _ = app.update(Msg::Host { sample: local.host() });
                Some(Msg::Tick { now: Instant::now() })
            }
```

`run_ui`'s doc comment gains one paragraph saying the `select!` gained **no
arm** this phase, that the host sample rides arm 4 and the feed rides the
redraw gate, and why that matters — the arm-retirement reasoning above it is
the subtlest thing in the module and nothing here should make a reader
re-derive it.

The imports `view/bleats.rs` and its `mod tests` need:

```rust
// module
use ratatui::text::{Line, Span};

use super::super::app::App;
use super::super::tail::Stream;
use super::flock::fit;
use crate::output::human_bytes;

// mod tests
    use super::*;
    use super::super::fixtures::{
        coloured, line, render_all, with_feed, with_feed_and_palette,
        with_feed_and_selection, with_no_selection,
    };
    use super::super::super::tail::Tail;
```

### Step 6.4 — verify

```bash
cargo fmt --all --check
cargo test -p shep-cli --bins --all-features                    # 413 passed; 0 failed; 2 ignored
grep -rn 'earlier lines not shown' crates/ | wc -l              # was 0; now ≥ 3
grep -c 'local.host()' crates/shep-cli/src/lookout/mod.rs       # was 0; now 1
grep -c 'feed_dirty' crates/shep-cli/src/lookout/mod.rs         # was 0; now 4
```

413 is 403 + 10. Still the inner loop: `detail::detail_lines` and
`host::strip_line` are both still unreachable from `main`, so the clippy gate
would fail on `dead_code`. Task 8 is where it runs.

### Step 6.5 — MUTATION

In `feed_lines`, `.take(body)` instead of `.skip(skip)`.
`the_pane_shows_the_last_lines_that_fit_and_says_so` must fail. Revert.

### Step 6.6 — second MUTATION

Style the `err` tag with `palette.alarm()`.
`the_stream_tag_is_a_word_and_stderr_is_not_bark` must fail on its span sweep.
Revert.

### Step 6.7 — third MUTATION: the line half of the gap

In `feed_lines`, drop the pane's own term:
`let lost_lines = feed.missed_lines;`.
`a_pane_that_cannot_show_every_line_it_holds_says_how_many` and
`the_pane_shows_the_last_lines_that_fit_and_says_so` must both fail, and
`both_kinds_of_gap_are_named_separately_in_one_line` must fail on the number
(500 rather than 525). **The byte test must still pass**, which is the finding:
this mutation restores exactly the first draft's behaviour, and the byte-only
check that draft carried could not see it. Revert.

### Step 6.8 — fourth MUTATION: the sample nobody takes

In the heartbeat arm, delete the `app.update(Msg::Host { .. })` line so the arm
yields only `Msg::Tick` again — the first draft's behaviour.
`the_heartbeat_asks_the_local_reader_for_a_host_sample` must fail.

Then check the second half of the finding: **run the whole suite with the
mutation still applied** and confirm that this is the *only* red. Every pane
test and every gallery frame injects `Msg::Host` directly, so if any other
check reddens, it is coupled to the loop in a way nothing has written down.
Revert.

### Step 6.9 — fifth MUTATION: the coalescing

Move the read back into the `Effect::RefreshFeed` arm — do the I/O there and
drop `feed_dirty` entirely.
`a_burst_of_selection_moves_costs_one_read_and_not_one_per_key` must fail with
21 reads rather than 1. Revert.

---

## Task 7 — `lookout/view/detail.rs`: the sheep detail pane

Three lines about the selected sheep, from the listing already in hand.

**Files:** new `crates/shep-cli/src/lookout/view/detail.rs`;
`crates/shep-cli/src/lookout/view/fixtures.rs` (the selection block);
`crates/shep-cli/src/lookout/view/mod.rs` (the `pub mod` line).

**Expected delta:** +4 tests.

### Step 7.1 — baseline

```bash
grep -rn 'no sheep selected' crates/ | wc -l    # 0
grep -rn 'lambs' crates/shep-cli/src/lookout/ | wc -l   # 0 — and it stays 0
```

The helpers this task's tests use — `with_selection`,
`with_selection_and_palette`, `sheep_with_lambs`, `render_all` — are in
"Shared test fixtures". `with_selection` builds a **two**-sheep flock with the
selection on the second, which is what makes Step 7.5's mutation able to
redden; a one-sheep fixture would make it invisible.

### Step 7.2 — RED

```rust
    /// fails if the pane starts claiming a lamb list. `ProcessInfo::lambs` is
    /// `None` on a `ListFlock` reply by construction, and its own doc says
    /// why: the walk costs a second pass over the machine's process table, and
    /// a flock listing is the thing an operator leaves running in a loop. A
    /// dashboard polling `Describe` every two seconds would put that walk on a
    /// timer.
    ///
    /// Asserted on the RENDERED pane rather than on the source, because the
    /// failure this guards is a caption or a heading promising something the
    /// pane cannot show.
    #[test]
    fn the_detail_pane_never_mentions_lambs() {
        let app = with_selection(sheep_with_lambs());
        let rendered = render_all(&detail_lines(&app, 200));
        for forbidden in ["lamb", "LAMB", "children", "tree"] {
            assert!(!rendered.contains(forbidden), "found {forbidden:?} in {rendered:?}");
        }
    }

    /// fails if the pane stops showing what the ROW above it cannot. Three
    /// things justify four rows of screen: the untruncated name (the NAME
    /// column ends in `…`, and a truncated name is one an operator types into
    /// `shep stop`), and both log paths (the first thing anyone wants after
    /// the feed shows them a crash).
    #[test]
    fn the_pane_adds_the_full_name_and_both_log_paths() {
        let app = with_selection(
            ProcessInfo::builder(7, "payments-reconciliation-worker", ProcStatus::Errored)
                .out_file(Some("/home/rin/.shep/logs/payments-out.log".to_string()))
                .err_file(Some("/home/rin/.shep/logs/payments-err.log".to_string()))
                .build(),
        );
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(rendered.contains("payments-reconciliation-worker"), "the whole name");
        assert!(rendered.contains("out  /home/rin/.shep/logs/payments-out.log"));
        assert!(rendered.contains("err  /home/rin/.shep/logs/payments-err.log"));
    }

    /// fails if the STATUS word stops carrying its own colour, or if anything
    /// else on the pane starts carrying one. Same rule as the table's: the
    /// coloured cell is the cell whose text already says the same thing.
    #[test]
    fn only_the_status_word_is_coloured() {
        let palette = Palette::detect(None, Some(OsStr::new("xterm-256color")), None);
        let app = with_selection_and_palette(
            ProcessInfo::builder(2, "api", ProcStatus::Errored).build(),
            palette,
        );
        let lines = detail_lines(&app, 200);
        let coloured: Vec<&str> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.style.fg == palette.alarm().fg)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(coloured, vec!["errored"], "got {coloured:?}");
    }

    /// fails if an unselectable pane stops saying WHY it is empty. "no sheep
    /// selected" alone restates what the operator can already see; the cause
    /// is that the flock is empty, and that is what the sentence has to carry.
    /// 12a shipped a caption claiming a sentence said why when it only stated
    /// the fact — this is the same mistake, refused one layer down.
    #[test]
    fn an_empty_flock_says_why_the_pane_has_nothing_to_describe() {
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/rin/.shep".to_string(),
            Instant::now(),
        );
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(
            rendered.contains("no sheep selected: the flock is empty"),
            "got {rendered:?}"
        );
    }
```

### Step 7.3 — GREEN

```rust
//! The sheep detail pane: three lines about the selected sheep.
//!
//! **Everything here comes from the `ProcessInfo` the flock table's own rows
//! are built from.** No second request, no `Request::Describe`, and therefore
//! no lamb list — `ProcessInfo::lambs` is `None` on a `ListFlock` reply by
//! construction, and its doc says why: the walk costs a second pass over the
//! machine's process table, and a flock listing is the thing an operator
//! leaves running in a loop.
//!
//! What it adds over the row above it: the UNTRUNCATED name (the NAME column
//! ends in `…`, and a truncated name is one an operator types into
//! `shep stop`), both log paths (the first thing anyone wants once the feed
//! shows them a crash), and whichever fields the current width tier has
//! dropped.

use ratatui::text::{Line, Span};

use super::super::app::App;
use super::flock::fit;
use crate::output::{human_bytes, human_duration};

/// The pane's three content lines. Its rule is [`super::draw`]'s.
#[must_use]
pub fn detail_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let palette = app.palette();
    let Some(row) = app.selected_row() else {
        return vec![
            // Names the CAUSE, not the fact. An operator can see the pane is
            // empty; what they cannot see is whether that is a broken
            // dashboard or a shepherd with nothing registered.
            Line::from(Span::styled(
                fit("no sheep selected: the flock is empty", width),
                palette.muted(),
            )),
            Line::from(Span::raw(String::new())),
            Line::from(Span::raw(String::new())),
        ];
    };
    let info = &row.info;

    // Everything except the status word, which is the one coloured cell —
    // exactly the table's rule, for exactly the table's reason.
    let head = format!("sheep {}  {}   ", info.id, info.name);
    let status = info.status.to_string();
    let rest = format!(
        "   pid {}   restarts {}   uptime {}   cpu {}   mem {}   fold {}{}",
        info.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        info.restarts,
        app.uptime_ms(info.id).map_or_else(|| "-".to_string(), human_duration),
        info.cpu_percent.map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        info.memory_bytes.map_or_else(|| "-".to_string(), human_bytes),
        info.fold.as_deref().unwrap_or("-"),
        // Last, so it is the first thing a narrow terminal truncates: a dog is
        // a rare row, and every field before it is true of every row.
        match &info.dog {
            None => String::new(),
            Some(DogSource::BuiltIn) => "   dog built-in".to_string(),
            Some(DogSource::Adopted { path }) => format!("   dog adopted {path}"),
            // `DogSource` is `#[non_exhaustive]`: a source a newer shepherd
            // added must not take the pane down, and must not be reported as
            // anything it is not.
            _ => "   dog (unrecognised source)".to_string(),
        }
    );
    let used = head.chars().count() + status.chars().count();

    vec![
        Line::from(vec![
            Span::raw(head),
            Span::styled(status, palette.status(info.status)),
            Span::raw(fit(&rest, width.saturating_sub(u16::try_from(used).unwrap_or(width)))),
        ]),
        path_line("out", info.out_file.as_deref(), width, palette),
        path_line("err", info.err_file.as_deref(), width, palette),
    ]
}

/// One log-path line, or a sentence saying why there is none.
///
/// `None` means the shepherd predates the field — `ProcessInfo::out_file`'s own
/// doc — which is a fact about the peer, not about this sheep, and the
/// sentence says so rather than leaving a bare `-` that reads like a missing
/// file.
fn path_line(
    label: &str,
    path: Option<&str>,
    width: u16,
    palette: super::super::theme::Palette,
) -> Line<'static> {
    let text = match path {
        Some(path) => format!("{label}  {path}"),
        None => format!("{label}  this shepherd did not report a path"),
    };
    Line::from(Span::styled(fit(&text, width), palette.muted()))
}
```

The imports `view/detail.rs`'s `mod tests` needs — `DogSource` is in the
module rather than the tests, beside the other `shep_core` types:

```rust
// module
use shep_core::protocol::DogSource;

// mod tests
    use super::*;
    use super::super::fixtures::{
        coloured, render_all, sheep_with_lambs, with_selection, with_selection_and_palette,
    };
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;
```

### Step 7.4 — verify

```bash
cargo fmt --all --check
cargo test -p shep-cli --bins --all-features               # 417 passed; 0 failed; 2 ignored
grep -rn 'Describe' crates/shep-cli/src/lookout/ | wc -l   # still 0
```

417 is 413 + 4. Inner loop only — `detail::detail_lines` has no caller in
`main`'s tree until Task 8.

`Describe` is the scope fence for this phase, and it is checked rather than
trusted. **`lambs` is no longer greppable to zero**, because
`fixtures::sheep_with_lambs` names it on purpose: the pane must stay silent
about lambs even when handed some, since the failure being guarded is a
caption promising a list, not a missing `if let`. Grep for `Describe` alone.

### Step 7.5 — MUTATION

In `detail_lines`, replace `app.selected_row()` with `app.rows().first().copied()`.
`the_pane_adds_the_full_name_and_both_log_paths` must fail: `with_selection`
builds a two-sheep flock with a `decoy` at id 0 and the selection moved onto
the sheep under test, so `first()` returns the wrong one. If it passes, the
fixture is not the one in "Shared test fixtures" — a one-sheep flock makes this
mutation invisible, which is why the fixture asserts `info.id > 0`. Revert.

### Step 7.6 — second MUTATION

Colour the whole first line with `palette.status(info.status)`.
`only_the_status_word_is_coloured` must fail with three entries instead of one.
Revert.

---

## Task 8 — height tiers, and the layout

Everything built so far reaches the screen.

**Files:** `crates/shep-cli/src/lookout/view/mod.rs`;
`crates/shep-cli/src/lookout/view/fixtures.rs` (`full_app`);
`crates/shep-cli/src/lookout/mod.rs` (one `run_ui` test).

**Expected delta:** +5 tests, and all eight snapshots re-accepted again.

**This is the task the full gate runs after.** Every one of the four modules
written before its call site — `tail::read`, `LocalReader`, `host::strip_line`,
`detail::detail_lines` — is wired into `draw` or `run_ui` by the end of it, so
this is the first point at which the tree is reachability-complete and
`cargo clippy --workspace --all-targets --all-features -- -D warnings` can pass.
See "Where the gate runs".

### Step 8.1 — baseline

```bash
grep -c 'panes_for' crates/shep-cli/src/lookout/view/mod.rs   # 0
grep -c 'HOST_ROWS\|DETAIL_ROWS\|FEED_ROWS' crates/shep-cli/src/lookout/view/mod.rs   # 0
grep -c 'host::strip_line\|detail::detail_lines\|bleats::feed_lines' crates/shep-cli/src/lookout/view/mod.rs   # 0
```

The third one is the reachability check: it is `0` before this task and must be
`3` after, which is the same fact the clippy gate is about to assert the hard
way.

### The layout, by arithmetic

No `Layout`, no `Constraint`, no widget — 12a's design decision 5b, unchanged.
Top to bottom:

```
row 0                 title                       always
row +1                banner                      when the link is not live
row +1                host strip                  HOST_ROWS   = 1
row +1                column header               always
row +1                rule                        always
rows …                the flock table             the remainder, ≥ 1
                      rule ┐
                      sheep detail  ├ DETAIL_ROWS = 4
                      × 3          ┘
                      rule ┐
                      feed header   ├ FEED_ROWS   = 7
                      × 5          ┘
last row              status bar                  always
```

The bottom stack is laid out **upward from the status bar**, so the flock table
takes whatever is left in the middle and nothing has to know the flock's length
in advance.

### Step 8.2 — RED

```rust
    /// fails if a tier can render taller than the terminal it was chosen for.
    /// The height twin of `every_tier_fits_the_width_it_claims`, and the check
    /// that makes the tier table a claim rather than a wish: every tier's
    /// fixed rows, plus a banner, plus a flock table worth having, must fit in
    /// that tier's own threshold.
    #[test]
    fn every_pane_tier_fits_the_height_it_claims() {
        for height in MIN_HEIGHT..=200 {
            let panes = panes_for(height);
            let fixed = CHROME_ROWS + 1 /* banner */ + panes.rows();
            // A tier that shows a pane must leave the table at least three
            // rows; the floor tier, which shows none, only has to leave one —
            // which is what MIN_HEIGHT has always meant.
            let floor = if panes.rows() == 0 { 1 } else { 3 };
            assert!(
                fixed + floor <= height,
                "height {height} chose {panes:?}, needing {} rows",
                fixed + floor
            );
        }
    }

    /// fails if the drop order changes without someone re-arguing it. The
    /// DETAIL pane goes first because it is the most redundant thing on the
    /// screen — every number on it but the log paths is in the row above it.
    /// The FEED goes second: its content exists nowhere else, but five lines
    /// of a busy log is thin. The HOST STRIP goes last: one row, and nothing
    /// else on the dashboard says anything about the machine.
    #[test]
    fn panes_drop_in_a_fixed_order_as_the_terminal_shortens() {
        assert_eq!(panes_for(60), Panes { host: true, detail: true, feed: true });
        assert_eq!(panes_for(24), Panes { host: true, detail: true, feed: true });
        assert_eq!(panes_for(23), Panes { host: true, detail: false, feed: true });
        assert_eq!(panes_for(18), Panes { host: true, detail: false, feed: true });
        assert_eq!(panes_for(17), Panes { host: true, detail: false, feed: false });
        assert_eq!(panes_for(14), Panes { host: true, detail: false, feed: false });
        assert_eq!(panes_for(13), Panes::NONE);
        assert_eq!(panes_for(MIN_HEIGHT), Panes::NONE, "12a's frame, untouched");
    }

    /// fails if a pane ever draws over the status bar, over the flock table,
    /// or off the bottom of the buffer. `Buffer::set_line` outside the area is
    /// a panic in debug and a silent no-op otherwise, and the arithmetic here
    /// has four moving parts.
    ///
    /// A live sweep, not a `timeout`: `draw` is synchronous, so a timer around
    /// it would complete on its first poll and bound nothing.
    #[test]
    fn every_pane_lands_inside_its_own_rows_across_the_size_sweep() {
        let mut app = full_app();
        app.update(Msg::Frozen { at_local: "2026-08-14 14:32:07".to_string() });
        for height in MIN_HEIGHT..=60 {
            for width in [MIN_TERM_WIDTH, 40, 51, 80, 120, 200] {
                let frame = draw_to(&app, width, height);
                let lines: Vec<&str> = frame.lines().collect();
                let panes = panes_for(height);

                // NOT `lines.len() == height`: `frames::render_text` maps
                // `(0..area.height)` over `(0..area.width)` by construction,
                // so that holds for any `draw` whatsoever — including one that
                // drew nothing at all. It is a property of the renderer, not
                // of this layout, and asserting it here would be a check that
                // cannot fail.
                let last = lines.last().unwrap();
                assert!(
                    last.contains("read-only"),
                    "the status bar survived at {width}x{height}: {last:?}"
                );
                // The row above the status bar belongs to the bottom-most pane
                // that is up, so it is never blank — a blank one means the
                // upward layout left a hole, which is the failure mode the
                // arithmetic here actually has.
                if panes.feed || panes.detail {
                    let above = lines[lines.len() - 2];
                    assert!(
                        !above.trim().is_empty(),
                        "a blank row above the status bar at {width}x{height}"
                    );
                }
                // And every pane that is up appears exactly once, so nothing
                // overlapped anything else.
                if panes.host {
                    assert_eq!(
                        lines.iter().filter(|l| l.starts_with("host  ")).count(),
                        1,
                        "the strip at {width}x{height}"
                    );
                }
                if panes.feed {
                    assert_eq!(
                        lines.iter().filter(|l| l.starts_with("bleats  ")).count(),
                        1,
                        "the feed header at {width}x{height}"
                    );
                }
                if panes.detail {
                    // The PATH prefix, not a bare `out  ` — the feed's own
                    // body lines are tagged `out  ` too, and counting those
                    // would make this assertion depend on how many log lines
                    // the fixture happens to carry.
                    assert_eq!(
                        lines
                            .iter()
                            .filter(|l| l.starts_with("out  /home/rin/.shep/logs/"))
                            .count(),
                        1,
                        "the detail pane's out path at {width}x{height}"
                    );
                }
            }
        }
    }

    /// fails if a heartbeat does not put the host strip on the frame. The
    /// other half of Task 6's `the_heartbeat_asks_the_local_reader_for_a_host
    /// _sample`, which could only assert the call because the strip was not
    /// drawable yet. This is the end-to-end: a `Local` that reports a sample,
    /// one heartbeat, and the numbers on the rendered frame.
    ///
    /// It is the only check in the phase that would catch a heartbeat that
    /// sampled and a `draw` that ignored the result, or the reverse.
    ///
    /// IR-46: bounded — a quit queued on a timer, inside a `timeout`.
    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_puts_the_host_strip_on_the_frame() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let local = FakeLocal { sample: Some(fixtures::sample()), ..FakeLocal::default() };

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            let _ = msg_tx.send(Msg::Key(KeyPress::Quit)).await;
        });

        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let terminal = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, stream::empty(), msg_rx, poll_tx, local),
        )
        .await
        .expect("the loop left within ten seconds");

        let frame = crate::lookout::frames::render_text(terminal.backend().buffer());
        assert!(
            frame.contains("host  load 2.31 4.10 3.88 / 10 cores"),
            "the strip drew the sample the heartbeat took: {frame}"
        );
        assert!(!frame.contains("not read yet"), "and not the pre-heartbeat sentence");
    }

    /// fails if the flock table stops being the spine. Rin's ruling in one
    /// test: whatever else is on screen, the table gets the remainder, and at
    /// the tier where all three panes are up it still has room for more than
    /// a couple of rows.
    #[test]
    fn the_flock_table_keeps_the_middle_of_the_screen() {
        let app = full_app();          // twelve sheep
        let frame = draw_to(&app, 120, 24);
        let data_rows = frame
            .lines()
            .filter(|line| line.starts_with("  ") || line.starts_with("> "))
            .filter(|line| line.trim_start().starts_with(|c: char| c.is_ascii_digit()))
            .count();
        assert!(data_rows >= 5, "the table got {data_rows} rows at 120x24");
    }
```

The last of those five goes in **`lookout/mod.rs`**'s `mod tests`, not
`view/mod.rs`'s — it drives `run_ui`, and that is where `run_ui`'s other tests
and `FakeLocal` already are. The other four are `view/mod.rs`'s.

For it to reach `fixtures::sample()`, `view/mod.rs` declares the fixtures
module as `#[cfg(test)] pub mod fixtures;` — the same shape
`lookout::frames` already uses (`mod.rs:26-27`: `#[cfg(test)]` on a `pub mod`,
so the items exist under `cargo test` and do not exist at all otherwise).

### Step 8.3 — GREEN

```rust
/// Rows the chrome always takes: title, column header, rule, status bar.
const CHROME_ROWS: u16 = 4;

/// The host strip is one line.
const HOST_ROWS: u16 = 1;

/// The detail pane: one rule and three lines.
const DETAIL_ROWS: u16 = 4;

/// The bleats feed: one rule, one header, five lines.
const FEED_ROWS: u16 = 7;

/// Which optional panes a terminal of a given height gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    /// The host-usage strip, under the title.
    pub host: bool,
    /// The sheep detail pane, under the table.
    pub detail: bool,
    /// The bleats feed, under that.
    pub feed: bool,
}

impl Panes {
    /// The flock table alone — 12a's frame, and what every terminal shorter
    /// than [`PANE_TIERS`]' last threshold gets.
    pub const NONE: Self = Self { host: false, detail: false, feed: false };

    /// How many rows these panes take together.
    #[must_use]
    pub const fn rows(self) -> u16 {
        let mut rows = 0;
        if self.host { rows += HOST_ROWS; }
        if self.detail { rows += DETAIL_ROWS; }
        if self.feed { rows += FEED_ROWS; }
        rows
    }
}

/// Height thresholds, tallest first. Each entry is the shortest terminal that
/// still gets that pane set.
///
/// The drop order is least-diagnostic-first and it is a decision, not an
/// accident of ordering — see this module's own doc and the phase plan's
/// design decision 8. 24 is not arbitrary either: it is the classic terminal
/// height, and the table is chosen so a plain 80×24 gets all three panes with
/// a flock table worth reading.
const PANE_TIERS: &[(u16, Panes)] = &[
    (24, Panes { host: true, detail: true, feed: true }),
    (18, Panes { host: true, detail: false, feed: true }),
    (14, Panes { host: true, detail: false, feed: false }),
    (MIN_HEIGHT, Panes::NONE),
];

/// The widest pane set that fits `height`.
#[must_use]
pub fn panes_for(height: u16) -> Panes {
    PANE_TIERS
        .iter()
        .find(|(threshold, _)| height >= *threshold)
        .map_or(Panes::NONE, |(_, panes)| *panes)
}
```

`draw`'s body, after the refusal branch:

```rust
    let panes = panes_for(height);
    let mut y = area.y;
    let bottom = area.y + height - 1;      // the status bar's row
    let buffer = frame.buffer_mut();

    buffer.set_line(area.x, y, &status::title_line(app, app.home(), width), width);
    y += 1;
    if let Some(banner) = status::banner_line(app) {
        buffer.set_line(area.x, y, &banner, width);
        y += 1;
    }
    if panes.host {
        buffer.set_line(area.x, y, &host::strip_line(app, width), width);
        y += 1;
    }

    // The bottom stack, laid out UPWARD from the status bar, so the table gets
    // the middle without anything having to know the flock's length first.
    let mut floor = bottom;
    let feed_at = panes.feed.then(|| { floor -= FEED_ROWS; floor });
    let detail_at = panes.detail.then(|| { floor -= DETAIL_ROWS; floor });

    // …header, rule, and the table's rows from `y` up to `floor`, exactly as
    // Task 2 left them, with `viewport = usize::from(floor - y)`…

    if let Some(top) = detail_at {
        buffer.set_line(area.x, top, &status::rule_line(palette.muted(), width), width);
        for (offset, line) in detail::detail_lines(app, width).iter().enumerate() {
            let offset = u16::try_from(offset).unwrap_or(0);
            buffer.set_line(area.x, top + 1 + offset, line, width);
        }
    }
    if let Some(top) = feed_at {
        buffer.set_line(area.x, top, &status::rule_line(palette.muted(), width), width);
        let rows = usize::from(FEED_ROWS - 1);
        for (offset, line) in bleats::feed_lines(app, width, rows).iter().enumerate() {
            let offset = u16::try_from(offset).unwrap_or(0);
            buffer.set_line(area.x, top + 1 + offset, line, width);
        }
    }

    buffer.set_line(area.x, bottom, &status::status_line(app, width), width);
```

`detail_lines` returns exactly three lines and `feed_lines` at most
`FEED_ROWS - 1`; both are enforced by their own tests rather than trusted here,
and `every_pane_lands_inside_its_own_rows_across_the_size_sweep` is the
backstop.

### Step 8.4 — verify

```bash
cargo test -p shep-cli --bins --all-features        # snapshot failures expected
cargo insta review                                  # LOOK at these — the panes are new
cargo test -p shep-cli --bins --all-features        # 422 passed; 0 failed; 2 ignored
grep -c 'host::strip_line\|detail::detail_lines\|bleats::feed_lines' crates/shep-cli/src/lookout/view/mod.rs   # was 0; now 3
```

422 is 417 + 5.

`insta review` rather than `insta accept` here, deliberately: this is the first
time the three panes appear on a frame, and the reviewer's eye is the only
thing that catches a pane rendering in the wrong place at a plausible size.

**Then the full task gate, for the first time this phase**, each from its own
command with `$?` read directly:

```bash
cargo fmt --all --check;                                                    echo "EXIT=$?"
cargo clippy --workspace --all-targets --all-features -- -D warnings;       echo "EXIT=$?"
cargo test --workspace --all-features;                                      echo "EXIT=$?"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features;  echo "EXIT=$?"
```

The clippy run is the one carrying information here: everything written in
Tasks 3-7 has been unreachable from `main` until this task wired it, so this is
the run that proves nothing was written and then forgotten. A `dead_code`
failure here names a real gap — do not silence it, wire it.

### Step 8.5 — MUTATION

Swap the middle two `PANE_TIERS` entries so the feed drops before the detail
pane.
`panes_drop_in_a_fixed_order_as_the_terminal_shortens` must fail at
`panes_for(23)`. Revert.

### Step 8.6 — second MUTATION

Raise `FEED_ROWS` to 12 without touching `PANE_TIERS`.
`every_pane_tier_fits_the_height_it_claims` must fail at the `18` tier
(4 + 1 + 1 + 12 + 3 = 21 > 18). This is the check that keeps the constants and
the thresholds honest with each other, and it is the one a future pane resize
will trip first. Revert.

### Step 8.7 — third MUTATION

Lay the bottom stack out downward instead of upward — compute `detail_at` from
`y` rather than `floor`.
`every_pane_lands_inside_its_own_rows_across_the_size_sweep` must fail: at a
short terminal the detail pane lands on the flock table's rows and the status
bar is overwritten or missing. Confirm it fails on one of the **new**
assertions — the `out  ` count, or the non-blank row above the status bar —
and not only on the old row-count one, which is gone precisely because it
could not distinguish this. Revert.

### Step 8.8 — fourth MUTATION

In `draw`, drop the `host::strip_line` call, leaving `panes.host` to reserve a
row nothing writes into.
`a_heartbeat_puts_the_host_strip_on_the_frame` must fail, and so must
`every_pane_lands_inside_its_own_rows_across_the_size_sweep`'s `host  ` count.
The pane-count assertions are what turn "the layout reserved the right rows"
into "the right things are in them", and that distinction is the whole of this
task. Revert.

---

## Task 9 — the frames: six new scenes, and captions that cannot lie

`docs/lookout/frames.txt` is rendered headlessly through ratatui's
`TestBackend`. It is both the test mechanism and the only way a human sees a
TUI, so it is a deliverable and not scaffolding.

**Files:** `crates/shep-cli/src/lookout/frames.rs`, its `snapshots/`,
`docs/lookout/frames.txt`, `docs/lookout/frames.ansi`,
`crates/shep-cli/src/lookout/view/status.rs` (one stale doc comment — see
Step 9.2).

**Expected delta:** +2 tests, snapshots 8 → 14, `ignored` stays at 4. Two
existing `frames.rs` tests are rewritten in place rather than added — see
Step 9.2's list.

### Step 9.1 — baseline

```bash
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l   # 8
grep -c '^=== ' docs/lookout/frames.txt                             # 8
grep -rn '#\[ignore' crates/ | wc -l                                # 16
grep -c 'Phase 12a' crates/shep-cli/src/lookout/frames.rs           # 1
grep -c '120' crates/shep-cli/src/lookout/frames.rs                 # 3 — a caption, `Scene::size`'s default arm, one test literal
```

### THE RULE FOR THIS TASK

**Pin every caption by a test.** 12a shipped two false captions — "colour is
on the STATUS word and nowhere else" when the chrome carried the same grey,
and "a sentence says why the pane is empty" when it only stated the fact — and
the one caption that was pinned claim-by-claim was the one caught before
shipping. Untested prose is where this project's claims rot.

So: **every clause of every caption is one assertion in
`every_scene_shows_the_thing_it_is_named_for`, or it is deleted from the
caption.** A caption is not allowed to say a thing the frame is not asserted to
show. When writing a caption, write the assertion first.

### Step 9.2 — the scenes

`Scene::ALL` grows from eight to fourteen. Existing sizes change where the
gutter or the tiers made them wrong.

| scene | size | what it is for |
|---|---|---|
| `healthy_wide` | 120×30 | all three panes, a selected row, everything live |
| `errored` | 120×30 | selection parked on the errored sheep |
| `empty` | 100×28 | nothing registered: three panes, three different sentences |
| `narrow` | **51**×14 | host strip only; four table columns dropped |
| `too_narrow` | 28×8 | below the floor: `need 33x6` |
| `retrying` | 120×30 | mid-reconnect |
| `frozen` | 120×30 | the ladder ran out — **carries a host sample** |
| `refused` | 120×30 | `x` with actions gated off |
| `no_detail` | 120×20 | the 18-tier: host strip and feed, no detail pane |
| `table_only` | 120×12 | below 14: 12a's frame, unchanged |
| `feed_gap` | 120×30 | the feed under a burst: the gap notice |
| `feed_missing` | 120×30 | the selected sheep has never written a log |
| `cramped` | 33×26 | all three panes at the narrowest terminal that draws |
| `host_unknown` | 120×30 | `sysinfo` says the platform is unsupported |

`narrow` moves from 49 to **51** columns and this is not cosmetic: the tier is
now chosen on `width - GUTTER`, so 49 columns would land on the `41` tier —
which has already dropped CPU — and the scene would contradict its own caption
in the gallery Rin reads. 12a's own comment records making this exact
correction once already, for the same reason.

**Five edits this table implies that a table cell does not make**, each of
which has to be written out or it will not happen:

1. **`Scene::size`'s default arm becomes `(120, 30)`.** It is
   `_ => (120, 20)` today, and `_` is a wildcard, so adding six variants will
   compile silently and render four of them at the wrong height. `Empty` gains
   its own arm at `(100, 28)`, `Narrow` moves to `(51, 14)`, and the six new
   scenes each get one.
2. **`frames.rs`'s `the_plain_renderer_is_one_line_per_row_and_no_escapes`
   hardcodes `text.lines().count() == 20`** for `HealthyWide`. It becomes
   `30`. The `120` in the same test is unchanged.
3. **`frames.rs:497`'s `need 31x6`** — already changed to `33x6` by Task 2.
   Confirm it, do not change it twice.
4. **`sheep()` gains both log paths.** It takes eight arguments and already
   carries `#[allow(clippy::too_many_arguments)]`; adding two more would make
   it ten. Derive them instead, inside the function:
   `.out_file(Some(format!("/home/rin/.shep/logs/{name}-{id}-out.log")))` and
   the matching `-err`. Deterministic, no new parameters, and it gives the
   detail pane something to render and `no_detail` something to assert the
   absence of.
5. **`view/status.rs`'s `a_truncated_hint_still_leaves_a_gap_before_the
   _control_label`** says it is pinned at 49 columns because that is "the
   `narrow` gallery scene's own width". After this task it is not. The test
   stays at 49 — 49 is still exactly where a 48-character hint truncates while
   a 9-character label fits, which is the property — but the sentence claiming
   it mirrors a scene becomes false and is rewritten to say what 49 actually
   is. A comment that outlives what it described is the same species of rot as
   a false caption.

### Step 9.2b — the selection, which four assertions depend on

`scene_with` moves the selection with two `Msg::Key(KeyPress::SelectDown)`
before the scene-specific messages, for `HealthyWide`, `Errored`, `NoDetail`,
`FeedGap`, `FeedMissing`, `Cramped` and `HostUnknown`:

```rust
    // Onto `api`, id 2, in both flocks — the third row of each. A fresh
    // snapshot selects the FIRST id, so without this every "sheep 2  api"
    // and "bleats  api" assertion below is asserting about `web` at id 0 and
    // failing for a reason that has nothing to do with the pane.
    if !matches!(which, Scene::Empty | Scene::Narrow | Scene::TooNarrow | Scene::TableOnly) {
        app.update(Msg::Key(KeyPress::SelectDown));
        app.update(Msg::Key(KeyPress::SelectDown));
    }
```

The four excluded scenes have either no flock (`Empty`) or no pane below the
table to describe, so moving the cursor in them would change a snapshot for no
reason. `Errored`'s caption claims the selection is parked on the errored
sheep, which is id 2 in its flock, so it is in.

**Order matters for `Frozen`.** Its `Msg::Host` and its selection are applied
**before** `Msg::Frozen`, because the reducer refuses both after — which is the
property, and which the two-age frame comparison then pins.

`scene_with` gains the two new messages, built directly rather than through a
`Local` fake — a scene needs determinism, not a trait:

```rust
    // Every live scene gets a host sample. The FROZEN one gets it too, and
    // that is load-bearing: `the_frozen_frame_does_not_move_however_long_the
    // _link_stays_gone` renders the frozen scene at two clock ages and
    // compares the frames byte for byte, so a host strip that kept updating
    // after the link was lost reddens it with no new assertion written. A
    // frozen scene with no host sample would leave that mutation uncaught —
    // Task 4's own mutation step says so out loud.
    if which != Scene::HostUnknown {
        app.update(Msg::Host { sample: Some(HostSample {
            load: (2.31, 4.10, 3.88),
            cores: Some(10),
            memory_total_bytes: 32 << 30,
            memory_used_bytes: 12 * (1 << 30) + (410 << 20),
            uptime_seconds: 6 * 86_400 + 3 * 3_600,
        })});
    } else {
        app.update(Msg::Host { sample: None });
    }
```

and a feed, varying by scene:

- **most scenes** — six or so ordinary `Stream::Out` lines, `missed_lines: 0`,
  `missed_bytes: 0`, so the ordinary header is what the gallery shows;
- **`feed_gap`** — thirty lines with `missed_lines: 500` and
  `missed_bytes: 4_012_000`, which is the both-kinds case: the header reads
  `… 525 earlier lines not shown, and 3.8M before them never read`
  (500 the reader dropped, plus 25 the five-row pane has no room for, and
  `human_bytes(4_012_000)` is `3.8M`). This is the frame that has to be
  legible, because it is the one an operator sees during an incident;
- **`feed_missing`** — no lines, no counts, and a `note` naming the cause.

The `Msg::Host` and the two `SelectDown`s for the frozen scene are applied
**before** `Msg::Frozen`, because the reducer refuses both after — which is the
property, and which the two-age comparison then pins.

### Step 9.3 — RED: the caption pins

Extend `every_scene_shows_the_thing_it_is_named_for` with one assertion per
caption clause. The captions and their pins, written together:

```rust
        // "All three panes at 120x30: the host strip under the title, the
        //  detail pane and the bleats feed under the table. `>` marks the
        //  selected sheep, and every pane below the table describes it."
        let wide = render_text(&scene(Scene::HealthyWide).1);
        assert!(wide.contains("host  load 2.31 4.10 3.88 / 10 cores"), "the host strip");
        assert!(wide.contains("sheep 2  api"), "the detail pane, on the selected sheep");
        assert!(wide.contains("bleats  api"), "and the feed, on the same one");
        assert_eq!(
            wide.lines().filter(|line| line.starts_with('>')).count(),
            1,
            "exactly one selection marker"
        );

        // "No sheep registered. Each of the three panes says why it is empty,
        //  and the three sentences are different because the three reasons
        //  are."
        let empty = render_text(&scene(Scene::Empty).1);
        assert!(empty.contains("the flock is empty"), "the table's own sentence");
        assert!(empty.contains("no sheep selected: the flock is empty"), "the detail pane's");
        assert!(empty.contains("bleats  no sheep is selected"), "the feed's");
        assert!(empty.contains("flock cpu -"), "and the strip shows no reading, not zero");

        // "51 columns: FOLD, RESTARTS, PID and MEM are gone, in that order.
        //  CPU and UPTIME survive because they explain WHY. The host strip
        //  fits; the detail pane and the feed do not, at 14 rows."
        let narrow = render_text(&scene(Scene::Narrow).1);
        assert!(narrow.contains("CPU") && narrow.contains("UPTIME"));
        for gone in ["FOLD", "RESTARTS", "PID", "MEM"] {
            assert!(!narrow.contains(gone), "the narrow tier dropped {gone}");
        }
        assert!(narrow.contains("host  load"), "the strip is up at 14 rows");
        assert!(!narrow.contains("bleats  "), "the feed is not");
        assert!(!narrow.contains("sheep 0  "), "and neither is the detail pane");

        // "28 columns: below the floor, the pane refuses rather than drawing
        //  overlapping garbage. Two short lines, so the refusal still fits the
        //  terminal it is refusing about."
        let too_narrow = render_text(&scene(Scene::TooNarrow).1);
        let mut lines = too_narrow.lines();
        assert_eq!(lines.next().unwrap().trim_end(), "too small");
        assert_eq!(lines.next().unwrap().trim_end(), "need 33x6");

        // "The feed under a burst: four megabytes were never read and some
        //  hundreds of lines were read and dropped. The pane counts both, and
        //  counts them separately, because it knows the second exactly and
        //  cannot know how many lines are in the first."
        let gap = render_text(&scene(Scene::FeedGap).1);
        assert!(gap.contains("earlier lines not shown"), "the lines it dropped");
        assert!(gap.contains("3.8M"), "the exact figure, not a vague one");
        assert!(gap.contains("never read"), "and what it never looked at");
        assert!(!gap.contains("re-read with each listing"), "the gap replaces the header");

        // "The selected sheep has never written a log in this $SHEP_HOME. The
        //  feed names that cause rather than sitting blank."
        let missing = render_text(&scene(Scene::FeedMissing).1);
        assert!(missing.contains("has not written a log in this $SHEP_HOME"));

        // "20 rows: the detail pane is the first to go, because every number
        //  on it but the log paths is already in the row above it."
        let no_detail = render_text(&scene(Scene::NoDetail).1);
        assert!(no_detail.contains("bleats  api"), "the feed stayed, on the selection");
        assert!(no_detail.contains("host  load"), "and so did the strip");
        // The ABSENCE, pinned to something only the detail pane can emit. The
        // first draft asserted `!contains("sheep 2  api")`, which passes just
        // as well when the selection is on sheep 0 and the pane drew
        // perfectly — a check that cannot fail for the reason it exists. The
        // log-path prefix is the detail pane's alone: the feed's body lines
        // are tagged `out  ` too, but they carry log TEXT, not a path.
        assert!(
            !no_detail.contains("out  /home/rin/.shep/logs/"),
            "the detail pane went"
        );

        // "12 rows: no optional panes at all. This is 12a's frame, and the
        //  only thing that changed is the two-column gutter the marker sits in."
        let table_only = render_text(&scene(Scene::TableOnly).1);
        assert!(!table_only.contains("host  load"));
        assert!(!table_only.contains("bleats  "));
        assert!(table_only.contains("STATUS"), "the table is still there");

        // "33 columns, 26 rows: the narrowest terminal that draws, with all
        //  three panes up. Everything truncates with an ellipsis; nothing
        //  overlaps."
        let cramped = render_text(&scene(Scene::Cramped).1);
        assert!(cramped.contains('…'), "something truncated, visibly");
        // NOT `line.chars().count() == 33` on every row: `render_text` maps
        // `(0..area.width)` for every row by construction, so that is true of
        // any frame at any width, including a blank one. What "nothing
        // overlaps" actually means is that each pane's own marker appears
        // exactly once, which is a claim about this layout.
        for marker in ["host  ", "bleats  ", "out  /home/rin/.shep/logs/"] {
            assert_eq!(
                cramped.lines().filter(|line| line.starts_with(marker)).count(),
                1,
                "{marker:?} appears once at 33 columns"
            );
        }
        assert!(
            cramped.lines().last().unwrap().contains("read-only"),
            "and the status bar is still the last row"
        );

        // The four scenes carried over from 12a all changed meaning this
        // phase — three panes, a marker, a strip, and 20 rows becoming 30 —
        // so their captions were rewritten and each new clause is pinned here
        // rather than left as prose nobody checked.

        // "The shepherd stopped answering. Five attempts over about eight
        //  seconds before this becomes the next frame. Every pane below the
        //  table keeps describing the selected sheep from the last listing."
        let retrying = render_text(&scene(Scene::Retrying).1);
        assert!(retrying.contains("reconnecting"));
        assert!(retrying.contains("sheep 2  api"), "the detail pane is still up");
        assert!(retrying.contains("host  load"), "and so is the strip");

        // "The ladder ran out. Last known values stay, the uptime clock has
        //  stopped, and so has the host strip — one line ticking over on a
        //  frozen screen is a contradiction on the same frame."
        let frozen = render_text(&scene(Scene::Frozen).1);
        assert!(frozen.contains("the shepherd has died"));
        assert!(
            frozen.contains("host  load 2.31 4.10 3.88 / 10 cores"),
            "the strip kept its LAST values rather than blanking"
        );

        // "One errored, one waiting to restart, one stopped, with the
        //  selection parked on the errored sheep. Each row's own STATUS cell
        //  is the only coloured cell in that row."
        let errored = render_text(&scene(Scene::Errored).1);
        assert!(errored.contains("errored"));
        assert!(errored.contains("sheep 2  api"), "the selection is on the errored sheep");
        assert_eq!(
            errored.lines().filter(|line| line.starts_with('>')).count(),
            1,
            "exactly one marker, on that row"
        );

        // "`x` with actions gated off. Both refusals are literal — nothing
        //  about damage gets charming — and the panes below carry on."
        let refused = render_text(&scene(Scene::Refused).1);
        assert!(refused.contains("--allow-control"));
        assert!(refused.contains("bleats  api"), "a refusal does not blank the screen");

        // "sysinfo reports this platform unsupported. The strip says so and
        //  keeps the flock's own totals, which lookout can always compute."
        let unknown = render_text(&scene(Scene::HostUnknown).1);
        assert!(unknown.contains("host  usage is not available on this platform"));
        assert!(unknown.contains("flock cpu"), "the half lookout can compute survives");
```

Plus one test that makes the rule itself mechanical:

```rust
    /// fails if a scene is added to `Scene::ALL` without a caption, or with a
    /// caption nobody pinned. The second half cannot be checked by a machine —
    /// but the first half can, and a scene with no caption is how an unpinned
    /// one gets in.
    #[test]
    fn every_scene_has_a_caption_and_a_distinct_label() {
        let mut labels = std::collections::BTreeSet::new();
        for which in Scene::ALL {
            assert!(labels.insert(which.label()), "two scenes share {}", which.label());
            let caption = which.caption();
            assert!(caption.len() > 30, "{} has a stub caption", which.label());
            assert!(caption.ends_with('.'), "{}'s caption is not a sentence", which.label());
        }
        // `labels.len() == Scene::ALL.len()` is not asserted: the `insert`
        // above already guarantees it, so it would be a line that cannot fail.
        // The literal can — it is what catches a scene added to the enum and
        // not to `ALL`, or the reverse.
        assert_eq!(Scene::ALL.len(), 14);
    }
```

### Step 9.4 — GREEN: `sgr` grows nothing, and that is checked

`frames.rs`'s `sgr` renders foregrounds only, and its own doc says a modifier a
12b pane introduced would render unstyled here rather than as a wrong style.
**No pane in this phase introduces one** — the selection marker is a character,
the feed tags are muted, the detail pane's only colour is a status word. Pin
it:

```rust
    /// fails if a 12b pane introduced a text MODIFIER. `sgr` renders
    /// foregrounds only — its own doc says a modifier would come out unstyled
    /// — and this phase deliberately introduced none: the selection marker is
    /// a character, not a `REVERSED` row, precisely so `NO_COLOR` and a
    /// 16-colour terminal lose nothing. If this ever reddens, `sgr` needs a
    /// case before the gallery is regenerated, or the modifier needs removing.
    #[test]
    fn no_scene_uses_a_modifier_the_ansi_renderer_would_drop() {
        for which in Scene::ALL {
            let buffer = scene(*which).1;
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let cell = &buffer[(buffer.area.x + x, buffer.area.y + y)];
                    assert!(
                        cell.modifier.is_empty(),
                        "{} has a modifier at {x},{y}",
                        which.label()
                    );
                }
            }
        }
    }
```

### Step 9.5 — GREEN: the gallery preamble

`GALLERY_PREAMBLE` is rewritten. It currently ends with three sentences about
12b being undecided; those are now false and must go. The replacement says what
the fourteen frames are, and says the one thing a reader of a poller-backed
feed has to know:

```rust
const GALLERY_PREAMBLE: &str = "shep lookout — Phase 12b frames
================================

These are real frames, rendered headlessly through ratatui's TestBackend by

    cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery

Nothing here is a mockup.

frames.ansi is the same fourteen frames with colour; read it with `less -R`.

All four panes are here: the flock table (the spine), the host-usage strip,
the sheep detail pane and the bleats feed. `>` marks the selected sheep, and
every pane below the table describes that one sheep.

The feed reads the selected sheep's log files from disk and re-reads them with
each flock listing. It is not a live subscription, and it says so on its own
header line: `out then err` because the two files are shown end to end with no
interleaving, and `re-read with each listing` because a two-second gap in this
pane is the refresh, not the sheep.

When the pane cannot show everything, the header says what went instead. Lines
it read and dropped are counted exactly; bytes below its 64 KiB window were
never read at all, so those are reported in bytes, because nothing counted the
lines in them and guessing would be worse than saying so.
";
```

### Step 9.6 — verify, and re-run Task 4's uncaught mutation

```bash
cargo test -p shep-cli --bins --all-features                        # snapshot failures for six new scenes
cargo insta review                                                  # accept each new scene AFTER LOOKING AT IT
cargo test -p shep-cli --bins --all-features                        # 424 passed; 0 failed; 2 ignored
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l   # was 8; now 14
grep -rn '#\[ignore' crates/ | wc -l                                # still 16
cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery
grep -c '^=== ' docs/lookout/frames.txt                             # was 8; now 14
```

424 is 422 + 2. Then the full task gate again, each from its own command.

**Then re-run Task 4's mutation at the frame level**: delete the `Link::Lost`
early return from the `Msg::Host` arm.

Two tests must now fail. `a_frozen_dashboard_ignores_a_host_sample` reddens in
the reducer, where it has reddened since Task 4 — Task 4's own step covers
that. The one being checked here is
`the_frozen_frame_does_not_move_however_long_the_link_stays_gone`, which
renders the frozen scene at two clock ages and compares the frames byte for
byte: it reddens only because the frozen scene carries a host sample and the
strip is on screen, and it is the only check in the phase that would catch a
strip that kept updating *on the frame Rin is looking at* rather than in the
reducer.

If that second one does not fail, the frozen scene is not getting a host sample
and the scene builder is wrong — the two-age comparison is comparing two frames
whose strips both say `not read yet`. Revert.

### Step 9.7 — read the gallery

Open `docs/lookout/frames.txt` and read all fourteen frames. This is the step
whose output goes to Rin, and it is the only one no test can do. Look for:

- a pane that is technically correct and unreadable
- a caption that says something the frame does not show — every clause of
  every caption has an assertion above, so any clause you cannot trace to one
  is a clause to delete
- the `cramped` scene: if 33 columns is genuinely unusable, say so in the task
  report rather than fixing it unilaterally. A pane width floor is a knob, and
  knobs are Rin's call this phase.

---

## Task 10 — docs, the gate, and the cross-checks

**Files:** `docs/lookout/README.md`, `docs/specs/deferred.md`, `CLAUDE.md`,
`crates/shep-cli/src/lookout/mod.rs`'s module doc,
`crates/shep-cli/src/cli.rs`'s `LookoutArgs` doc.

**Expected delta:** 0 tests.

### Step 10.1 — baseline

```bash
grep -c 'Phase 12a' docs/lookout/README.md                    # 2
grep -c 'are Phase 12b' crates/shep-cli/src/lookout/mod.rs    # 1
grep -c '12b' crates/shep-cli/src/cli.rs                      # 0 — but 'this phase wires the gate' is there
grep -rn '12b' crates/ | wc -l                                # 11
grep -c 'Tasks 7-11 land' crates/shep-cli/src/output/table.rs # 1
```

That last one is a sentence that has been stale since Phase 11:
`output::table::human_duration` carries `#[allow(dead_code)]` and a doc saying
it has "no real caller until Tasks 7-11 land". Tasks 7-11 landed, and this
phase gives it two more callers (`view/host.rs` and `view/detail.rs`), so the
`allow` and its sentence both go. Removing it is not cosmetic: an
`#[allow(dead_code)]` left on live code is the thing that makes the next real
`dead_code` finding invisible.

Every one of those eleven is a sentence saying a pane does not exist yet. All
eleven are now false. This step is not cosmetic: a module doc that says the
bleats feed is a future phase, sitting above the bleats feed, is the same
species of rot as a false caption.

### Step 10.2 — the docs

- **`docs/lookout/README.md`** — rewritten. Its "What is still open for 12b"
  section is replaced by "What 12b settled" (the four panes, the selection, the
  feed's file-reading decision and its cost, the height tiers) and a shorter
  "What is still open" carrying only what genuinely is: search/filter, actions
  behind the gate, and lambs in the detail pane. **This is public-facing prose
  in a repository Rin publishes** — draft it, then run the `humanizer` skill
  over it before it is final, matching the voice of the existing README rather
  than inventing a new one.
- **`docs/specs/deferred.md`** — three entries, each with the reason rather
  than just the name: `search/filter` (Rin's v1 ruling, plus the unresolved
  grammar question), lookout actions (the gate exists and refuses honestly),
  lambs in the detail pane (`Describe`'s process-table walk on a two-second
  timer).
- **`CLAUDE.md`** — the status paragraph gains 12b: the three remaining panes,
  the selection, and the one-line statement that the feed reads files rather
  than subscribing, because that is the decision a future phase is most likely
  to try to "fix".
- **`crates/shep-cli/src/lookout/mod.rs`** — the module doc's opening
  paragraph, which currently says 12a builds one pane and names the other three
  as 12b, is rewritten to describe the four-pane screen and to point at the
  feed's own module for the volume argument.

### Step 10.3 — the task gate

Each from its own command, `$?` captured directly, never through a pipe:

```bash
cargo fmt --all --check;                                                    echo "EXIT=$?"
cargo clippy --workspace --all-targets --all-features -- -D warnings;       echo "EXIT=$?"
cargo test --workspace --all-features;                                      echo "EXIT=$?"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features;  echo "EXIT=$?"
```

Baseline for the third, measured at `fc3534b` and still standing at `ed09740`
(nothing between them touches `crates/` — see "Every check in this plan states
its baseline"): **1219 passed / 0 failed / 4 ignored across 17 result lines.**

Expected after this phase: **roughly 1264 / 0 / 4 across 17 lines** — 1219 plus
the 45 this plan's tasks add, task by task in execution order:
7 + 2 + 8 + 2 + 5 + 10 + 4 + 5 + 2, where Task 2's `2` is three new tests less
the one `view/flock.rs` test it deletes. The pass count is a shape and the
arithmetic above is there so that a number far off it is a signal rather than a
shrug; `failed = 0`, `ignored = 4` and `17 lines` are not shapes.

### Step 10.4 — the phase gate

```bash
cargo test --workspace --all-features -- --test-threads=1;   echo "EXIT=$?"
```

Not ceremony: this was red on `main` before Phase 5 and caught a real
regression in Phase 6. This phase adds a reader of the filesystem to a test
suite that already runs `tempdir`s in parallel, so a serial run is exactly the
shape that would expose a `tail.rs` test leaking state between cases.

```bash
cargo check -p shep-daemon --all-targets --all-features \
  --target x86_64-unknown-linux-gnu;                         echo "EXIT=$?"
cargo check --workspace --all-targets --all-features \
  --target x86_64-pc-windows-gnu;                            echo "EXIT=$?"
```

The Windows leg is the one that earns its place this phase. `lookout/` is
`#[cfg(unix)]`, and `source.rs` now names `sysinfo` — which shep-cli already
depends on unconditionally, so nothing should change. `cargo check`, not
clippy: shep-daemon's `boot`/`sys`/`server`/`tokio_runner` are `cfg(unix)`, so
on Windows 51 dead-code warnings fall out of code that is not dead anywhere we
ship. Needs `brew install mingw-w64` for `ring`'s `cc` build script.

Record `EXIT=` for all seven commands in the phase report. A pipeline's `$?` in
zsh is the last command's, so none of these may be piped into anything.

### Step 10.5 — the dependency measurement, restated

```bash
grep -c '^\[\[package\]\]' Cargo.lock                                     # equal to Step 3.1's number
cargo tree -p shep-cli --all-features 2>/dev/null | grep -c 'sysinfo v'   # unchanged from Step 3.1
git diff --numstat Cargo.lock                                             # 0 lines either way
```

**Compared against Step 3.1's recorded numbers, not against a literal.** Phase
13 (`whistle`) is adding packages in a parallel worktree, so an absolute figure
here would stop an executor for a reason that has nothing to do with this
phase. What 12b has to prove is that it added nothing, and that is a delta.
`git diff --numstat` is the one that settles it outright: if `Cargo.lock` has
no changed lines on this branch, no count can have moved.

`git diff --numstat`, not `git diff | grep '^-'` — a unified diff opens every
file's hunk with `--- a/<path>`, which `grep '^-'` matches, so that form can
never print zero.

### Step 10.6 — the final read

```bash
grep -rn '12b' crates/ | wc -l          # was 11; expect 0 or 1 (the feed's own argument may cite the phase)
grep -rn 'not built yet\|is Phase 12' crates/shep-cli/src/lookout/ | wc -l
grep -c 'Tasks 7-11 land' crates/shep-cli/src/output/table.rs   # was 1; now 0
grep -rn 'j/k scroll' crates/ | wc -l                           # was 7; now 0
grep -rn 'out+err' crates/ | wc -l                              # 0 before and after
```

The second one should find only `x`'s refusal (`stop is not built yet`), which
is still true. Anything else is a sentence that outlived what it described.

The last two are about the two phrases on surfaces an operator reads. They are
different in kind and the difference is worth stating rather than letting a
reader assume both are checks:

- **`j/k scroll` is a real check.** It is `7` today — the hint in `status.rs`
  and six snapshots that render it — and `0` after, so it can fail.
- **`out+err` is `0` before and after**, because it is a string this phase
  considered and did not ship: the feed header says `out then err`, since `+`
  reads as one merged stream and there is no merge. It cannot fail today and it
  is here as a guard against reintroduction, not as evidence. Saying which of
  the two it is costs one line and stops the next reader counting it as proof.

---

## Merge

Per CLAUDE.md's default: merge locally into `main`, then delete the branch and
the worktree in the same response. `cargo publish` is never run without
`--dry-run`, `git push` and `git tag` are Rin's, and nothing in this phase
touches `web/` or `docs/shep-design/`.

## One-line summary of the phase, for the commit body

> lookout grows its three remaining panes: a host-usage strip, a sheep detail
> pane, and a bleats feed that reads the selected sheep's log files from disk
> rather than subscribing to `log.*` — so a busy flock costs one bounded read
> per refresh instead of making the dashboard the highest-volume subscriber on
> the bus. What the feed cannot show, it says: the lines it read and dropped
> counted exactly, and the bytes below its window reported as bytes, because
> nothing counted the lines in those and guessing would be worse than saying
> so.
