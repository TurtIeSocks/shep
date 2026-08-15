# Phase 12b — the lookout's three remaining panes

`shep lookout`'s bleats feed, sheep detail pane and host-usage strip: the
three panes spec §9 names and Phase 12a deliberately did not build. Against
merged `main` at `fc3534b`.

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
- Baseline **1219 passed / 0 failed / 4 ignored across 17 result lines.**
- Terminology: the daemon is "the shepherd" and only that; one managed process
  is "a sheep", the plural is always "the flock"; a sheep's children are
  "lambs". Destructive operations and error text stay plain.

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
prints TODAY, at `fc3534b`, on this machine.** Run the baseline command
*before* you make the change. If it does not print what this plan says, stop
and say so — the check is broken, not the tree.

```bash
git rev-parse --short HEAD                                          # fc3534b
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
grep -c '^=== ' docs/lookout/frames.txt                             #  8
grep -c 'Phase 12a' docs/lookout/README.md                          #  2
grep -c '^\[\[package\]\]' Cargo.lock                               # 326
```

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
   Three new coloured things arrive this phase — the detail pane's STATUS word,
   the feed's `err` tag, the gap notice — and all three are words already.
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
- Everything between the previous read and the window is **skipped**, and the
  pane says so. `LocalReader` remembers each file's length at the previous
  read; when the file has grown by more than the bytes the rendered lines
  cover, the feed's header is replaced by
  `… 3.8M written since the last read is not shown`, in `attention` (butter,
  not bark — see decision 6). That number is exact and computable, and Task 5
  pins it against a real temp file.
- A feed that silently showed the newest 5 lines of a 4 MB burst, with nothing
  saying 4 MB had gone by, would be the single most misleading thing on this
  screen. The gap notice is the price of choosing a poller.

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

### 3. The blocking read happens on the UI task, and that is deliberate.

`std::fs`, not `tokio::fs`: shep-cli's tokio does not carry the `fs` feature,
and `commands/bleats.rs` already makes and states this call for the same
bounded read. Per refresh the feed costs at most two `open`+`stat`+`seek`+
64 KiB `read` pairs — 128 KiB every two seconds, on a task that is otherwise
asleep. `spawn_blocking` for that would add a task, a channel and a race
between the reply and the next snapshot, to hide about a millisecond.

The bound is what makes this defensible, so the bound is a constant with a
test, not a hope: `FEED_WINDOW_BYTES` (64 KiB) and `FEED_TAIL_LINES` (40).
Task 5's test writes a 4 MiB file and asserts the reader touched a bounded
number of bytes and returned a bounded number of lines.

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
shepherd dies. It stops anyway: while `Link::Lost`, the UI loop does not sample
and the reducer would ignore the sample if it did.

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

**Segments drop from the right when they do not fit**, in that order: `up`
first (a host that has been up six days explains nothing about right now), then
`flock mem`, then `flock cpu`, then `host mem`, leaving the load average, which
is the single most useful number about a machine running a process manager.
This is a `while` loop over a `Vec<String>`, not a threshold table: the strip is
one line built from five parts, and a `TIERS`-shaped table for it would be
ceremony.

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
off a message that was already arriving. Sampling memory and a load average
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

---

## Task order and dependencies

```
Task 1  selection in the reducer            (app.rs)          — no deps
Task 2  the gutter, the marker, the floors  (view/, flock.rs) — needs 1
Task 3  the Local trait and the host sample (source.rs)       — no deps
Task 4  the host strip                      (view/host.rs)    — needs 2, 3
Task 5  the bounded tail reader and the gap (tail.rs)         — needs 3
Task 6  the bleats feed pane                (view/bleats.rs)  — needs 1, 5
Task 7  the sheep detail pane               (view/detail.rs)  — needs 1, 2
Task 8  height tiers and the layout         (view/mod.rs)     — needs 4, 6, 7
Task 9  the frames: scenes, captions, gallery                  — needs 8
Task 10 docs, the phase gate, the cross-checks                 — needs 9
```

Tasks 1 and 3 are independent and may run in parallel. So may 5 and 7 once
their inputs land. Everything else is a chain, because it is one screen.

---

## Task 1 — `lookout/app.rs`: a selected sheep

Replace the scroll offset with a selection, add the reseat rule, add
`Effect::RefreshFeed`, and rename the four keys that no longer scroll.

**Files:** `crates/shep-cli/src/lookout/app.rs`,
`crates/shep-cli/src/lookout/input.rs`.

**Expected delta:** +5 tests in `shep-cli --bins` (three new reducer tests, one
new input test, one rewritten). Two existing reducer tests are rewritten in
place, not added.

### Step 1.1 — baseline

```bash
grep -c 'pub fn scroll' crates/shep-cli/src/lookout/app.rs                # 1
grep -rn 'ScrollUp\|ScrollDown\|ScrollTop\|ScrollBottom' crates/ | wc -l  # 24
grep -c 'RefreshFeed' crates/shep-cli/src/lookout/app.rs                  # 0
cargo test -p shep-cli --bins --all-features                              # 379 passed; 0 failed; 2 ignored
```

The `RefreshFeed` count is `0` today and must be non-zero after — this is the
one check in this task that cannot pass before the change.

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

Rewrite `a_snapshot_that_shrinks_the_flock_pulls_the_scroll_back` in place as
`a_snapshot_that_shrinks_the_flock_pulls_the_selection_back`, asserting on
`selected_index()` where it asserted on `scroll()`.

### Step 1.4 — verify

```bash
cargo test -p shep-cli --bins --all-features
```

Expect `384 passed; 0 failed; 2 ignored` — 379 + 5. `view/mod.rs` and
`frames.rs` still compile because neither names `scroll()` directly; `draw`'s
call to `flock::scroll_offset` is Task 2's.

```bash
grep -c 'RefreshFeed' crates/shep-cli/src/lookout/app.rs        # was 0; now ≥ 6
grep -rn 'ScrollUp\|ScrollDown\|ScrollTop\|ScrollBottom' crates/ | wc -l   # was 24; now 0
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

---

## Task 2 — the gutter, the marker, and the two floors

The flock table moves two columns right; the selected row gets a `>`; the
terminal's width floor becomes 33 while the table's stays 31.

**Files:** `crates/shep-cli/src/lookout/view/flock.rs`,
`crates/shep-cli/src/lookout/view/mod.rs`.

**Expected delta:** +3 tests. Eight snapshots are re-accepted (every frame
shifts two columns) — see the note in Task 9 about why that is correct and not
a wire-fixture violation.

### Step 2.1 — baseline

```bash
grep -c '31x6' crates/shep-cli/src/lookout/view/mod.rs      # 3
grep -c '33x6' crates/shep-cli/src/lookout/view/mod.rs      # 0
grep -c 'GUTTER' crates/shep-cli/src/lookout/view/mod.rs    # 0
```

`33x6` is `0` today and must be non-zero after: this is what makes the rewritten
`a_terminal_below_the_floor_says_so_instead_of_drawing` a check that can fail.

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
        assert_eq!(mark(true), ">");
        assert_eq!(mark(false), " ");
        assert_eq!(mark(true).chars().count(), 1);
        assert_eq!(mark(false).chars().count(), 1);
        assert!(mark(true).is_ascii(), "an ambiguous-width glyph would shift the row");
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
cargo test -p shep-cli --bins --all-features                 # 387 passed; 0 failed; 2 ignored
grep -c '33x6' crates/shep-cli/src/lookout/view/mod.rs       # was 0; now 3
```

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

**Files:** `crates/shep-cli/src/lookout/source.rs`.

**Expected delta:** +3 tests.

### Step 3.1 — baseline

```bash
grep -rn 'sysinfo' crates/shep-cli/src/lookout/ | wc -l        # 0
grep -c '^\[\[package\]\]' Cargo.lock                          # 326
cargo tree -p shep-cli --all-features 2>/dev/null | grep -c 'sysinfo v'   # ≥ 1 — record the exact number
```

The last one is the measurement that matters. `sysinfo` is **already** an
unconditional shep-cli dependency (`dog::metrics`' `shep_host_*` series), so
this task must not change either count. Record both numbers before and after
and say so in the task report — that is the whole of the dependency argument,
and it is a measurement rather than a claim.

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
    #[test]
    fn an_unsupported_platform_reports_nothing_rather_than_zero() {
        let mut local = LocalReader::new();
        match local.host() {
            // This machine supports it, so the numbers must be real ones.
            Some(sample) => {
                assert!(sysinfo::IS_SUPPORTED_SYSTEM);
                assert!(sample.memory_total_bytes > 0, "a supported host has memory");
                assert!(sample.memory_used_bytes <= sample.memory_total_bytes);
                assert!(sample.cores.is_some_and(|cores| cores >= 1));
            }
            None => assert!(!sysinfo::IS_SUPPORTED_SYSTEM),
        }
    }

    /// fails if the core count is re-read on every sample. It comes from
    /// `std::thread::available_parallelism`, which does not change for the
    /// life of a process, and the strip renders it beside the load average on
    /// a one-second heartbeat.
    #[test]
    fn the_core_count_is_read_once_and_then_carried() {
        let mut local = LocalReader::new();
        let first = local.host().and_then(|sample| sample.cores);
        let second = local.host().and_then(|sample| sample.cores);
        assert_eq!(first, second);
        assert_eq!(first, LocalReader::new().host().and_then(|s| s.cores));
    }
```

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

```bash
cargo test -p shep-cli --bins --all-features                             # 390 passed
grep -c '^\[\[package\]\]' Cargo.lock                                   # still 326
cargo tree -p shep-cli --all-features 2>/dev/null | grep -c 'sysinfo v'  # unchanged from Step 3.1
```

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

## Task 4 — `lookout/view/host.rs`: the host-usage strip

One line, five self-labelled segments, dropped from the right when they do not
fit.

**Files:** new `crates/shep-cli/src/lookout/view/host.rs`;
`crates/shep-cli/src/lookout/app.rs` (the `Msg::Host` arm and the accessor);
`crates/shep-cli/src/lookout/view/mod.rs` (the `pub mod` line only — the
layout is Task 8).

**Expected delta:** +5 tests.

### Step 4.1 — baseline

```bash
find crates/shep-cli/src/lookout/view -name '*.rs' | wc -l   # 3
grep -rn 'flock cpu' crates/ | wc -l                         # 0
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

    /// fails if the drop order changes without someone re-arguing it. `up`
    /// goes first — a host that has been up six days explains nothing about
    /// right now — and the load average is last to go, because it is the
    /// single most useful number about a machine running a process manager.
    ///
    /// The widths are derived from the rendered segments rather than written
    /// as literals, so this test does not have to be edited every time a
    /// number gains a digit.
    #[test]
    fn segments_drop_from_the_right_in_a_fixed_order() {
        let app = with_host(sample(), flock_of(4, 1));
        let full = rendered(&strip_line(&app, 200));
        let full_width = u16::try_from(full.trim_end().chars().count()).unwrap();

        let one_short = rendered(&strip_line(&app, full_width - 1));
        assert!(!one_short.contains("up "), "`up` is the first to go");
        assert!(one_short.contains("flock mem"), "and nothing else went with it");

        // Walk the width down and record the order things disappear.
        let mut gone = Vec::new();
        for width in (10..=full_width).rev() {
            let line = rendered(&strip_line(&app, width));
            for segment in ["up ", "flock mem", "flock cpu", "host mem", "host  load"] {
                if !line.contains(segment) && !gone.contains(&segment) {
                    gone.push(segment);
                }
            }
        }
        assert_eq!(gone, vec!["up ", "flock mem", "flock cpu", "host mem", "host  load"]);
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

    /// fails if the strip ever renders wider than the terminal it was given.
    /// `Buffer::set_line` truncates in silence, so a strip that overflowed
    /// would lose its rightmost segment without the drop logic ever running —
    /// which is the bug the drop logic exists to prevent, hidden.
    #[test]
    fn the_strip_never_exceeds_the_width_it_was_given() {
        let app = with_host(sample(), flock_of(40, 3));
        for width in [MIN_TERM_WIDTH, 40, 51, 80, 100, 120, 200, 400] {
            let line = rendered(&strip_line(&app, width));
            assert!(
                line.chars().count() <= usize::from(width),
                "{} chars at width {width}",
                line.chars().count()
            );
        }
    }
```

### Step 4.3 — GREEN: the reducer's half

`App` gains one field and one accessor:

```rust
    /// The last host reading, or `None` before the first heartbeat and on a
    /// platform `sysinfo` does not support. [`Self::host_supported`] tells the
    /// strip which of the two it is looking at.
    host: Option<HostSample>,
    /// False once a sample has come back `None` from a supported platform's
    /// sampler — the two `None`s mean different things and the strip says
    /// different sentences for them.
    host_unsupported: bool,
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
//! Segments drop from the RIGHT when they do not fit, least useful first:
//! `up`, then the flock's memory, then its CPU, then the host's memory,
//! leaving the load average. A `while` loop over a `Vec<String>` rather than
//! a threshold table like [`super::flock::TIERS`] — this is one line built
//! from five parts, and a tier table for it would be ceremony.

use ratatui::text::{Line, Span};

use super::super::app::App;
use crate::output::{human_bytes, human_duration};

/// The strip, fitted to `width`.
#[must_use]
pub fn strip_line(app: &App, width: u16) -> Line<'static> {
    let mut segments = segments(app);
    while joined_width(&segments) > usize::from(width) && segments.len() > 1 {
        segments.pop();
    }
    // One `Span`, muted: nothing on this line is damage, and nothing on it is
    // a status word. Colour here would be decoration with no meaning behind
    // it, which is the one thing the palette module forbids.
    Line::from(Span::styled(
        super::flock::fit(&segments.join("   "), width),
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

    if let Some(host) = app.host() {
        out.push(format!("up {}", human_duration(host.uptime_seconds * 1_000)));
    }
    out
}

/// How wide `segments` would render, separators included.
fn joined_width(segments: &[String]) -> usize {
    segments.iter().map(|s| s.chars().count()).sum::<usize>()
        + segments.len().saturating_sub(1) * 3
}
```

`human_duration` takes milliseconds, so `uptime_seconds * 1_000` is the
conversion — and `uptime_seconds` is a `u64` of seconds since boot, which
overflows `u64` milliseconds only after 584 million years.

### Step 4.5 — verify

```bash
cargo test -p shep-cli --bins --all-features        # 395 passed; 0 failed; 2 ignored
```

The strip is not on screen yet — `draw` does not call it until Task 8. That is
deliberate: it keeps the layout rewrite in one task instead of five. Clippy
will not complain, because `view/host.rs` is reached from its own tests.

### Step 4.6 — MUTATION

In the `Msg::Host` arm, delete the `Link::Lost` early return.
Nothing in this task's own tests reddens — **and that is the finding**. The
test that catches it is 12a's
`the_frozen_frame_does_not_move_however_long_the_link_stays_gone`, and it only
catches it once the frozen scene carries a host sample and the strip is on
screen. Record this in the task report as an explicit dependency: **Task 9 must
give the frozen scene a host sample, and Step 9.6 re-runs this mutation.** A
mutation with no red is a gap in coverage, and writing it down is what stops it
being discovered in review instead.

### Step 4.7 — second MUTATION

In `segments`, replace the two `fold`s with `.sum()` (which yields `0` for an
empty iterator rather than `None`).
`a_flock_with_no_readings_shows_a_dash_and_not_a_zero` must fail on
`flock cpu -`. Revert.

---

## Task 5 — `lookout/tail.rs`: the bounded reader, and the gap it admits to

The answer to design decision 1, made concrete: a window from the end of each
file, a line cap on top of it, and an exact count of what was skipped.

**Files:** new `crates/shep-cli/src/lookout/tail.rs`;
`crates/shep-cli/src/lookout/mod.rs` (one `pub mod` line).

**Expected delta:** +6 tests.

### Step 5.1 — baseline

```bash
find crates/shep-cli/src/lookout -name '*.rs' | wc -l   # 11
grep -rn 'written since the last read' crates/ | wc -l  # 0
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    /// The newest lines, oldest first — `out`'s tail, then `err`'s.
    ///
    /// **There is no merge, and that is stated rather than hidden.** A log
    /// line carries no timestamp, so there is no key to interleave the two
    /// files on, and guessing one from file order would be wrong exactly when
    /// a sheep writes to both at once. `bleats`' module doc records the same
    /// limitation for the same reason. The pane renders the LAST rows of this
    /// list, so a crash on stderr survives a chatty stdout.
    pub lines: Vec<TailLine>,
    /// Bytes appended since the previous read that [`Self::lines`] does not
    /// cover.
    ///
    /// Zero on the first read of a file — showing the tail of a file's history
    /// is not a gap — and zero when the file shrank, which is what a rotation
    /// or a `shep flush` looks like from here. Non-zero is the normal case for
    /// a busy sheep, and the pane says so where it cannot be missed: a feed
    /// that silently showed the newest five lines of a four-megabyte burst
    /// would be the most misleading thing on this screen.
    pub missed_bytes: u64,
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
    /// is what is asserted.
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
        assert_eq!(tail.lines.len(), FEED_TAIL_LINES);
        let held: usize = tail.lines.iter().map(|l| l.text.len()).sum();
        assert!(
            held < usize::try_from(FEED_WINDOW_BYTES).unwrap(),
            "held {held} bytes of a 4 MiB file"
        );
        assert_eq!(tail.missed_bytes, 0, "the first read of a file is not a gap");
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
        assert!(second.missed_bytes > 4 * 1024 * 1024 - 1024, "got {}", second.missed_bytes);
        assert!(second.missed_bytes < 5 * 1024 * 1024);
        assert_eq!(second.lines.last().unwrap().text, "four", "the NEWEST lines survive");

        // A third read with nothing appended reports no gap at all.
        let third = read(&mut seen, Some(&path), None);
        assert_eq!(third.missed_bytes, 0);
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
    }
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
            Ok((lines, missed)) => {
                tail.missed_bytes = tail.missed_bytes.saturating_add(missed);
                tail.lines.extend(
                    lines.into_iter().map(|text| TailLine { stream, text }),
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
        tail.note = Some(if notes.is_empty() {
            "this sheep has written nothing yet".to_string()
        } else {
            notes.join("; ")
        });
    }
    tail
}

/// The last [`FEED_TAIL_LINES`] lines of one file, and how many bytes have
/// been appended since the previous read that those lines do not cover.
///
/// # Errors
/// The file could not be opened, `stat`ed, seeked or read — notably
/// [`std::io::ErrorKind::NotFound`] and `EISDIR`, which [`read`] treats
/// differently from each other.
fn read_window(
    seen: &mut BTreeMap<PathBuf, u64>,
    path: &Path,
) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(FEED_WINDOW_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut window = Vec::new();
    file.read_to_end(&mut window)?;

    // A window boundary can land mid-line. Half a line shown as a whole one is
    // a lie, so the bytes up to and including the first newline are discarded.
    let bytes: &[u8] = if start > 0 {
        match window.iter().position(|&byte| byte == b'\n') {
            Some(newline) => &window[newline + 1..],
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
    lines.drain(..keep_from);

    // What the kept lines actually cover, in bytes, terminators included. The
    // gap is what grew beyond that — `saturating_sub` twice, so a file that
    // SHRANK (a rotation, a `shep flush`) reports no gap rather than sixteen
    // exabytes.
    let covered: u64 = lines
        .iter()
        .map(|line| u64::try_from(line.len()).unwrap_or(u64::MAX).saturating_add(1))
        .sum();
    let previous = seen.insert(path.to_path_buf(), len);
    let missed = match previous {
        // The first read of a file shows the tail of its history. That is not
        // a gap, and reporting it as one would put a four-megabyte notice on
        // screen every time an operator selected a long-running sheep.
        None => 0,
        Some(previous) => len.saturating_sub(previous).saturating_sub(covered),
    };
    Ok((lines, missed))
}
```

### Step 5.4 — verify

```bash
cargo test -p shep-cli --bins --all-features                  # 401 passed; 0 failed; 2 ignored
grep -rn 'written since the last read' crates/ | wc -l        # still 0 — that string is Task 6's
```

### Step 5.5 — MUTATION

Delete the `seen` bookkeeping — always return `missed = 0`:

```rust
    let _ = seen.insert(path.to_path_buf(), len);
    Ok((lines, 0))   // MUTATION
```

`a_file_that_grew_between_reads_reports_the_bytes_it_skipped` must fail on its
`missed_bytes > 4 MiB - 1 KiB` assertion. This is the single most important
mutation in the phase: it turns the feed back into the silent-truncation
version design decision 1 rejected, and nothing else on the screen would say
so. Revert.

### Step 5.6 — second MUTATION

In `read_window`, `lines.truncate(FEED_TAIL_LINES)` instead of
`lines.drain(..keep_from)` — keep the OLDEST lines rather than the newest.
`a_file_that_grew_between_reads_reports_the_bytes_it_skipped` must fail on
`assert_eq!(second.lines.last().unwrap().text, "four")`. Revert.

### Step 5.7 — third MUTATION

In `read_window`, drop the partial-line discard (always use `&window`).
`a_window_boundary_discards_the_partial_line_it_lands_in` must fail — a line
beginning `zzzz…PARTIAL-HEAD` appears. Revert.

---

## Task 6 — `lookout/view/bleats.rs`: the feed pane

**Files:** new `crates/shep-cli/src/lookout/view/bleats.rs`;
`crates/shep-cli/src/lookout/app.rs` (`Msg::Bleats` and the accessor);
`crates/shep-cli/src/lookout/mod.rs` (`Effect::RefreshFeed`'s handler in
`run_ui`).

**Expected delta:** +6 tests.

### Step 6.1 — baseline

```bash
grep -rn 'written since the last read' crates/ | wc -l   # 0
grep -c 'Msg::Bleats' crates/shep-cli/src/lookout/mod.rs # 0
```

### Step 6.2 — RED

```rust
    /// fails if the gap notice stops reaching the screen. Task 5 makes the
    /// number exact; this is the half that makes it visible, and without it
    /// the feed silently shows five lines of a four-megabyte burst.
    #[test]
    fn a_gap_replaces_the_header_and_says_how_much_went_by() {
        let app = with_feed(Tail {
            lines: vec![line(Stream::Out, "still here")],
            missed_bytes: 4_000_000,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(
            rendered.contains("3.8M written since the last read is not shown"),
            "got {rendered:?}"
        );
        // And the ordinary header is gone while the gap notice is up: two
        // header lines would cost one of the five content rows.
        assert!(!rendered.contains("re-read with each listing"));
    }

    /// fails if the ordinary header stops naming the sheep, the streams, or
    /// the fact that this is a re-read rather than a live stream. An operator
    /// who reads this pane as `tail -f` will draw wrong conclusions from a
    /// two-second gap in a log, and the pane is the only place that can say
    /// so.
    #[test]
    fn the_header_says_which_sheep_and_that_it_is_a_re_read() {
        let app = with_feed_and_selection(
            Tail { lines: vec![line(Stream::Out, "hello")], missed_bytes: 0, note: None },
            "api",
        );
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(rendered.contains("bleats  api"), "got {rendered:?}");
        assert!(rendered.contains("out+err"), "got {rendered:?}");
        assert!(rendered.contains("re-read with each listing"), "got {rendered:?}");
    }

    /// fails if the pane stops showing the NEWEST lines. A feed that scrolled
    /// off the bottom would show an operator the beginning of a burst and hide
    /// its end, which is the opposite of what a dashboard is for.
    #[test]
    fn the_pane_shows_the_last_lines_that_fit_and_not_the_first() {
        let app = with_feed(Tail {
            lines: (0..40).map(|n| line(Stream::Out, &format!("line-{n}"))).collect(),
            missed_bytes: 0,
            note: None,
        });
        let rendered = render_all(&feed_lines(&app, 120, 6));
        assert!(rendered.contains("line-39"), "the newest line is on screen");
        assert!(!rendered.contains("line-0\n"), "the oldest is not");
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
                missed_bytes: 0,
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
            missed_bytes: 0,
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

### Step 6.3 — GREEN

`App` gains `feed: Tail` (defaulting to `Tail::default()`), a `feed()`
accessor, and the arm:

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

    out.push(match app.selected_row() {
        None => Line::from(Span::styled(
            fit("bleats  no sheep is selected", width),
            palette.muted(),
        )),
        Some(row) if feed.missed_bytes > 0 => Line::from(Span::styled(
            fit(
                &format!(
                    "bleats  {}  … {} written since the last read is not shown",
                    row.info.name,
                    human_bytes(feed.missed_bytes)
                ),
                width,
            ),
            // Attention, not alarm: a sheep writing faster than a two-second
            // poll is busy, not broken. `--bark` means errored, refused and
            // destructive.
            palette.attention(),
        )),
        Some(row) => Line::from(Span::styled(
            fit(
                &format!(
                    "bleats  {}  out+err  from the log files, re-read with each listing",
                    row.info.name
                ),
                width,
            ),
            palette.muted(),
        )),
    });

    let body = rows.saturating_sub(1);
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
```

`run_ui`'s new effect arm:

```rust
            Effect::RefreshFeed => {
                // The paths are cloned out before `app` is borrowed mutably.
                let (out, err) = app.selected_row().map_or((None, None), |row| {
                    (row.info.out_file.clone(), row.info.err_file.clone())
                });
                let tail = local.tail(
                    out.as_deref().map(Path::new),
                    err.as_deref().map(Path::new),
                );
                // `let _`: `Msg::Bleats` returns `Effect::None` by
                // construction — see its arm in the reducer — and acting on a
                // returned effect here would be the one place this design
                // could recurse.
                let _ = app.update(Msg::Bleats { tail });
                dirty = true;
            }
```

and `run_ui` gains `mut local: L` with `L: Local`, wired from `lookout()` as
`source::LocalReader::new()`. `run_ui`'s doc comment gains one paragraph saying
the `select!` gained **no arm** this phase and why that matters (the
arm-retirement reasoning above it is the subtlest thing in the module).

The two existing `run_ui` tests get a `FakeLocal` that returns
`(None, Tail::default())`.

### Step 6.4 — verify

```bash
cargo test -p shep-cli --bins --all-features                    # 407 passed; 0 failed; 2 ignored
grep -rn 'written since the last read' crates/ | wc -l          # was 0; now ≥ 2
```

### Step 6.5 — MUTATION

In `feed_lines`, `.take(body)` instead of `.skip(skip)`.
`the_pane_shows_the_last_lines_that_fit_and_not_the_first` must fail. Revert.

### Step 6.6 — second MUTATION

Style the `err` tag with `palette.alarm()`.
`the_stream_tag_is_a_word_and_stderr_is_not_bark` must fail on its span sweep.
Revert.

---

## Task 7 — `lookout/view/detail.rs`: the sheep detail pane

Three lines about the selected sheep, from the listing already in hand.

**Files:** new `crates/shep-cli/src/lookout/view/detail.rs`.

**Expected delta:** +4 tests.

### Step 7.1 — baseline

```bash
grep -rn 'no sheep selected' crates/ | wc -l    # 0
grep -rn 'lambs' crates/shep-cli/src/lookout/ | wc -l   # 0 — and it stays 0
```

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

### Step 7.4 — verify

```bash
cargo test -p shep-cli --bins --all-features               # 411 passed; 0 failed; 2 ignored
grep -rn 'lambs\|Describe' crates/shep-cli/src/lookout/ | wc -l   # still 0
```

That last one is the scope fence for this phase, and it is checked rather than
trusted.

### Step 7.5 — MUTATION

In `detail_lines`, replace `app.selected_row()` with `app.rows().first().copied()`.
`the_pane_adds_the_full_name_and_both_log_paths` must fail once the fixture has
more than one sheep — **check that it does**; if `with_selection` builds a
one-sheep flock, the mutation cannot redden and the fixture needs a second
sheep before the mutation is meaningful. Revert.

### Step 7.6 — second MUTATION

Colour the whole first line with `palette.status(info.status)`.
`only_the_status_word_is_coloured` must fail with three entries instead of one.
Revert.

---

## Task 8 — height tiers, and the layout

Everything built so far reaches the screen.

**Files:** `crates/shep-cli/src/lookout/view/mod.rs`.

**Expected delta:** +4 tests, and all eight snapshots re-accepted again.

### Step 8.1 — baseline

```bash
grep -c 'panes_for' crates/shep-cli/src/lookout/view/mod.rs   # 0
grep -c 'HOST_ROWS\|DETAIL_ROWS\|FEED_ROWS' crates/shep-cli/src/lookout/view/mod.rs   # 0
```

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
                assert_eq!(
                    frame.lines().count(),
                    usize::from(height),
                    "{width}x{height} drew the wrong number of rows"
                );
                let last = frame.lines().last().unwrap();
                assert!(
                    last.contains("read-only"),
                    "the status bar survived at {width}x{height}: {last:?}"
                );
            }
        }
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
cargo test -p shep-cli --bins --all-features        # 415 passed; 0 failed; 2 ignored
```

`insta review` rather than `insta accept` here, deliberately: this is the first
time the three panes appear on a frame, and the reviewer's eye is the only
thing that catches a pane rendering in the wrong place at a plausible size.

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
bar is overwritten or missing. Revert.

---

## Task 9 — the frames: six new scenes, and captions that cannot lie

`docs/lookout/frames.txt` is rendered headlessly through ratatui's
`TestBackend`. It is both the test mechanism and the only way a human sees a
TUI, so it is a deliverable and not scaffolding.

**Files:** `crates/shep-cli/src/lookout/frames.rs`, its `snapshots/`,
`docs/lookout/frames.txt`, `docs/lookout/frames.ansi`.

**Expected delta:** +5 tests, snapshots 8 → 14, `ignored` stays at 4.

### Step 9.1 — baseline

```bash
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l   # 8
grep -c '^=== ' docs/lookout/frames.txt                             # 8
grep -rn '#\[ignore' crates/ | wc -l                                # 16
grep -c 'Phase 12a' crates/shep-cli/src/lookout/frames.rs           # 1
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

and a feed, varying by scene: ordinary lines, a `missed_bytes: 4_012_000`
burst for `feed_gap`, and a `note` for `feed_missing`. The `Msg::Host` for the
frozen scene is applied **before** `Msg::Frozen`, because the reducer refuses
it after — which is the property, and which the two-age comparison then pins.

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

        // "The feed under a burst: four megabytes were written between two
        //  reads and are not on screen, and the pane says so instead of
        //  showing five lines as though they were all of it."
        let gap = render_text(&scene(Scene::FeedGap).1);
        assert!(gap.contains("written since the last read is not shown"));
        assert!(gap.contains("3.8M"), "the exact figure, not a vague one");
        assert!(!gap.contains("re-read with each listing"), "the gap replaces the header");

        // "The selected sheep has never written a log in this $SHEP_HOME. The
        //  feed names that cause rather than sitting blank."
        let missing = render_text(&scene(Scene::FeedMissing).1);
        assert!(missing.contains("has not written a log in this $SHEP_HOME"));

        // "20 rows: the detail pane is the first to go, because every number
        //  on it but the log paths is already in the row above it."
        let no_detail = render_text(&scene(Scene::NoDetail).1);
        assert!(no_detail.contains("bleats  "), "the feed stayed");
        assert!(no_detail.contains("host  load"), "and so did the strip");
        assert!(!no_detail.contains("sheep 2  api"), "the detail pane went");

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
        for line in cramped.lines() {
            assert_eq!(line.chars().count(), 33, "no row over- or under-ran");
        }

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
        assert_eq!(labels.len(), Scene::ALL.len());
        assert_eq!(labels.len(), 14);
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
header line — a sheep writing faster than the two-second refresh has its
skipped output counted and reported rather than silently dropped.
";
```

### Step 9.6 — verify, and re-run Task 4's uncaught mutation

```bash
cargo test -p shep-cli --bins --all-features                        # snapshot failures for six new scenes
cargo insta review                                                  # accept each new scene AFTER LOOKING AT IT
cargo test -p shep-cli --bins --all-features                        # 420 passed; 0 failed; 2 ignored
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l   # was 8; now 14
grep -rn '#\[ignore' crates/ | wc -l                                # still 16
cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery
grep -c '^=== ' docs/lookout/frames.txt                             # was 8; now 14
```

**Then re-run Task 4's first mutation**, which had no red at the time: delete
the `Link::Lost` early return from the `Msg::Host` arm.
`the_frozen_frame_does_not_move_however_long_the_link_stays_gone` must now
fail, because the frozen scene carries a host sample and the strip is on
screen. If it does not fail, the frozen scene is not getting a host sample and
the scene builder is wrong. Revert.

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
```

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

Baseline for the third, measured at `fc3534b` on this machine:
**1219 passed / 0 failed / 4 ignored across 17 result lines.** Expected after
this phase: **roughly 1260 / 0 / 4 across 17 lines.** The pass count is a
shape; `failed = 0`, `ignored = 4` and `17 lines` are not.

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
grep -c '^\[\[package\]\]' Cargo.lock                                     # 326, unchanged
cargo tree -p shep-cli --all-features 2>/dev/null | grep -c 'sysinfo v'   # unchanged from Step 3.1
git diff --numstat Cargo.lock                                             # 0 lines either way
```

`git diff --numstat`, not `git diff | grep '^-'` — a unified diff opens every
file's hunk with `--- a/<path>`, which `grep '^-'` matches, so that form can
never print zero.

### Step 10.6 — the final read

```bash
grep -rn '12b' crates/ | wc -l          # was 11; expect 0 or 1 (the feed's own argument may cite the phase)
grep -rn 'not built yet\|is Phase 12' crates/shep-cli/src/lookout/ | wc -l
```

The second one should find only `x`'s refusal (`stop is not built yet`), which
is still true. Anything else is a sentence that outlived what it described.

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
> the bus, and the bytes it skips are counted and shown.
