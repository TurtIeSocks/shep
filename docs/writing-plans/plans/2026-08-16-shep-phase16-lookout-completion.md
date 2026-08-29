# Phase 16 - `shep lookout`'s three unbuilt pieces

The filter, the actions behind the gate, and lambs in the detail pane. These
are the three items `docs/specs/deferred.md` and `docs/lookout/README.md` both
still list as open, and together they finish `shep lookout`.

**The design is approved and is the specification for this plan:**
[docs/brainstorming/specs/2026-08-16-lookout-completion-design.md](../../brainstorming/specs/2026-08-16-lookout-completion-design.md).
The maintainer accepted it with no changes, including its twenty numbered assumptions.
Nothing below reopens a design question. Where this plan makes a choice the
design did not spell out, it says so under "Shapes the design named" and gives
the reason in the design's own terms.

**the maintainer's ruling on `start`, recorded so nobody re-derives it.** lookout gets
stop, restart and reload. It does not get start, even though whistle's control
surface has `start_sheep` and the design's word "identical" is therefore not
literally true of the two surfaces. That is a scope decision, it is hers, and
it is settled. Adding start later is additive: one key, one arm-time refusal
for a sheep already running, no new shape anywhere. **A task that adds a start
key has left this plan.**

Phases 1 to 15 are merged. Phase 12a shipped lookout's shell and its flock
table; 12b shipped the bleats feed, the sheep detail pane and the host-usage
strip. This phase is the rest.

---

## Global constraints

- MSRV 1.88, edition 2024, `MIT OR Apache-2.0`.
- `#![forbid(unsafe_code)]` everywhere outside shep-daemon's `sys.rs`. The
  crate this phase touches (`shep`, at `crates/shep-cli/`) already carries it
  at the top of `src/lib.rs`.
- **`PROTOCOL_VERSION` stays 1, and this phase needs no wire change at all.**
  The design says so three times. The filter is entirely client side; the
  actions use `Request::Stop` / `Restart` / `Reload`, which shipped in Phase 4;
  the lambs use `Request::Describe`, which shipped in Phase 3 and which Phase
  11 taught to carry `ProcessInfo::lambs`. `Request`, `Response`, `ProcessInfo`
  and `BusEvent` are all untouched, so there is no stability fixture to add
  (IR-35) and no CHANGELOG entry for a wire change (IR-45). **A task that
  reaches for a new `Request` or `Response` variant, or for a new field on
  `ProcessInfo`, has left scope. Stop and say so.** Step 10.2 checks this with
  a diff, not with a promise.
- IR-20: a `pub` error enum in a **library** crate carries `#[non_exhaustive]`
  with its own rationale, or documents why not. `shep` is the binary crate;
  `source::LinkError` and `link::UiGone` are the two shipped precedents, each
  carrying the sentence that says why it omits the attribute. Any new error
  type here carries the same sentence rather than leaving the omission silent.
- IR-28: every `Result`-returning public function gets an `# Errors` section.
  `FlockSource::send` is the one new one, and the design already wrote its
  section.
- IR-33 / IR-34: no sleeps, hand-rolled fakes, paused tokio clock, unique
  fixtures per test. Every clock in the reducer arrives on a `Msg`, which is
  what makes the ten-second confirm expiry testable without waiting ten
  seconds.
- IR-46: a test that can only fail by hanging carries an explicit bound.
- Every new public item gets a doc comment and a deliberate `Debug` decision.
  Nothing here carries env or secrets, so no redacted `Debug` is needed
  (IR-41); plain `#[derive(Debug)]` on each new type.
- Terminology: the daemon is **the shepherd** and only that. One managed
  process is **a sheep**; the plural is always **the flock**. A sheep's
  children are **lambs**. Destructive operations and error text stay plain.

### The commands

The package is `shep`. It was renamed from `shep-cli` in Phase 14 and the
directory is still `crates/shep-cli/`, which is expected. It now has a `[lib]`
target, so the fast loop is `--lib --bins`, not `--bins`:

```bash
cargo test -p shep --lib --bins --all-features
```

**Measured today: `558 passed; 0 failed; 3 ignored` in 1.57s** across four
result lines (the lib, plus three bin targets with no tests each). Treat the
count as a shape and not a checksum: two briefs on this project have shipped a
stale figure. What matters is that `failed` stays `0`.

The lookout subset, for iterating inside this phase:

```bash
cargo test -p shep --lib --all-features -- lookout::
```

**Measured today: `94 passed; 0 failed; 1 ignored`** (the ignored one is
`write_the_gallery`), 0.36s.

Task gate, each from its own command with `$?` read directly, never through a
pipe (in zsh a pipeline's `$?` is the last command's and `${PIPESTATUS[0]}` is
empty):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

One cargo command at a time: the workspace shares one target-dir build lock,
so two at once block rather than parallelise.

Phase gate adds:

```bash
cargo test --workspace --all-features -- --test-threads=1
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The Windows check needs `x86_64-w64-mingw32-gcc` (`brew install mingw-w64`),
because `ring`'s build script runs `cc` and `shep` has `ring` in its tree. If
that toolchain is missing on the machine running this phase, say so out loud
rather than dropping the check silently, which is how Phases 7 to 9 lost it.

### One known flake, so it is not mistaken for a regression

`serve::worker::tests::a_connection_that_stops_reading_is_dropped_at_the_deadline`
failed once on the first baseline run of this plan and passed on every run
since, including twice in isolation. It is a bounded wait on a real socket
under load, which CLAUDE.md already names as a source of false radii. If it
reddens during this phase, re-run it alone before believing it:

```bash
cargo test -p shep --lib --all-features -- serve::worker::tests::a_connection_that_stops_reading_is_dropped_at_the_deadline
```

Nothing in this phase touches `serve`.

---

## What in this plan was put through a compiler

An earlier revision of this plan shipped a GREEN block that does not compile,
because nobody ran it. Every non-trivial Rust block below has now been
scaffolded on edition 2024 and built with the real borrow checker: `reseat`,
`select_at`, `rows`, `selected_index`, `set_filter` and the `SelectLast` call
(Task 1); `arm`, `confirm`, `forget_missing_target`, `disarm_on_link_change`,
the routing rule and the `try_send` or-pattern (Task 7); `on_action_reply` and
`outcome` (Task 8); `on_lambs`, `lambs_for` and `lamb_line`'s match with its
`if lambs.is_empty()` guard (Tasks 4 and 6); and `status_line`'s six-arm chain
including `app.action().filter(|a| !a.sent)` (Task 9).

One block failed and is fixed: `self.selected = self.visible_ids().nth(index);`
in `reseat` is **E0506**, because `visible_ids` returns
`impl Iterator<Item = u32> + '_` and that temporary holds a shared borrow of
all of `self` to the end of the statement. It is written two lines now. The
line it replaced borrowed one field, `self.flock.keys()`, which is why the
shipped version was fine. **Anything a later revision adds that assigns to a
field of `self` in the same statement as a call taking `&self` has the same
bug**; the whole plan was swept for the shape and this was the only instance.

Two of Task 1's mutations were also run against the scaffold rather than
reasoned about, and both stated reasons turned out to be wrong. See Steps 1.5
and 1.6.

## Shapes the design named, and the five places this plan makes one concrete

The design is behaviour. Five of its shapes do not survive contact with the
code as written: three do not compile or leave a state unanswered, and two
would ship a defect if implemented literally. This plan fixes each in the
smallest way that keeps the stated behaviour. **Three of the five are
deviations from an assumption the maintainer approved** and are collected again in a table
at the end of this plan so she can reject any one of them on its own; each is
named here so a reviewer sees it declared rather than discovers it in a diff.

**1. `Armed` becomes `Action` plus a `Stage`.** The design writes
`struct Armed { verb, id, name, at }` and separately describes an in-flight
state that "outranks the hint and the filter line". Two `Option` fields would
admit a state the machine cannot be in (armed and in flight at once), and the
design's own "one action at a time" rule is exactly the claim that such a state
does not exist. So there is one field, `action: Option<Action>`, and `Action`
carries `stage: Stage` where `Stage` is `Armed` or `Sent`. The design's doc
comment about the target being captured at arm time and never re-read moves
onto `Action` verbatim.

**2. The status bar has SIX slots, because the filter box being typed into is
not the same thing as the filter line.** A18 says "confirm, then notice, then
filter, then hint", and the design says of the in-flight line only that it
"outranks the hint and the filter line". Both sentences use "the filter" for
two states the design never separates: a box the operator's fingers are on
right now, and a persistent applied-filter line the title also signals. Keeping
them together is what produced the two worst defects in this plan's first
draft, so they are separated here:

| # | slot | why it sits there |
|---|---|---|
| 1 | an armed confirm | A18. A question awaiting an answer outranks everything. |
| 2 | the filter box, while editing | A live interaction outranks a report of a past event. See the paragraph below: A4's stated reason for putting the filter in the bar at all rests on a premise that is false. |
| 3 | a notice | A18. |
| 4 | an in-flight action | The design's own words: it outranks the hint and the filter line, and A18 puts the notice above the filter line. |
| 5 | the applied filter line | A18. |
| 6 | the key hint | A18. |

**This is a deviation from A4 and needs the maintainer's nod.** A4 accepts, as the whole
cost of putting the filter in the status bar, that "a transient notice can
briefly cover the filter line", and justifies it with "while editing, every
keypress is text, so nothing can raise a notice". **That premise is false.**
Three notices in the shipped reducer are raised by messages that are not
keypresses at all and keep arriving while the box is open:
`Msg::BusLagged` (`app.rs:313`), `BusEvent::Dropped` (`:415`) and
`BusEvent::DaemonShutdown` (`:427`). Worse than A4 admits: `on_text_key` does
not clear the notice, so nothing an operator types takes it back off, and a
notice landing mid-word would cover the query until they pressed Enter or Esc.
Slot 2 closes that. What A4 accepts still holds for slot 5, which is the state
A4's own sketch (`filter_active`) shows.

The alternative fix, clearing `self.notice` at the top of `on_text_key`, is
rejected: it destroys the notice rather than deferring it, so an operator who
happened to be typing when the shepherd announced it was shutting down would
never learn. Slot 2 defers. The notice is still there when the box closes, and
the next ordinary keypress clears it as notices are cleared.

Consequence of slot 4 sitting below slot 3, stated so it can be rejected: a
transient notice can briefly cover the in-flight line. That is deliberate and
it is what makes a refusal visible: `arm`'s "one action is already in flight"
IS a notice, so a bar that put the in-flight line above notices would swallow
the answer to the operator's own keypress. A keypress clears the notice and the
in-flight line comes back, which is the property
`an_in_flight_line_survives_a_keypress` pins in the reducer and
`a_refusal_while_an_action_is_in_flight_reaches_the_bar` pins in the view.

**3. A `try_send` that fails needs an answer, and the design does not give
one.** The reducer enters the in-flight state and hands `run_ui` a request to
send; if the channel is full or closed, nothing would ever answer it, the bar
would keep saying "sent, waiting for the shepherd", and the one-action-at-a-
time guard would refuse every later action for the life of the process. So
there is a `Msg::Unsent { sent }`, fed by `run_ui` when `try_send` returns
`Err`, which clears the action and says plainly that it was not sent. For a
lamb fetch it does nothing, because a dropped lamb fetch already reads as "not
read yet", which is the design's own rule for a request the channel dropped.

The sentence is `{verb} {name} (id {id}): it was not sent`, and it stops
there. It does **not** say the shepherd is unreachable, because the reducer
does not know that: the channel has capacity 2, it is shared with lamb
fetches, and `run_connected` awaits each request inline, so
`TrySendError::Full` is reachable while the shepherd is perfectly reachable
and merely slow. `Full` and `Closed` get one sentence rather than two because
the operator's next move is the same either way, and because a sentence naming
a cause the code did not observe is the exact failure the `-` CPU cell exists
to prevent.

**4. `q` and `Ctrl-C` are not consumed by the routing rule.** The design says
"every other key cancels", and the routing rule as written consumes every
non-Enter key, `KeyPress::Quit` included, so `q` and Ctrl-C would stop quitting
while a prompt is up. That contradicts `input.rs`'s own shipped doctrine, which
is stated in `map_key`'s doc comment: dropping the Ctrl-C mapping "would leave
the most reflexive way out of a terminal program doing nothing, and the
operator's next move" is `kill -9` from another window, "past every restore
path `super::term` has". So `KeyPress::Quit` returns `Effect::Quit` from inside
the routing rule, above the cancel. The safety property the rule exists for is
untouched, because that property is about a cancelling key ALSO doing its
ordinary job on a target the operator has lost track of, and quitting discards
the confirm rather than acting on it. **This is a narrow carve-out of A10 and
it is named here so the maintainer can reject it**; text mode already makes the same
carve-out and the design writes it into its own key table there.

**5. An armed confirm is cleared when the link leaves `Live`.** A9 refuses
arming unless the link is `Live`, but an already-armed prompt would survive the
link going away and `confirm` does not re-check. Worse, the expiry rides the
`Msg::Tick` arm's non-`Lost` branch and `now` stops advancing once the link is
lost (`app.rs:337-342`), so an armed prompt on a frozen dashboard would never
expire and Enter would still try to send. Refusals in this design happen at arm
time so an operator never answers a question that was never going to be
honoured, and leaving the prompt up to refuse at Enter is exactly what that
rule forbids. So the `Msg::Retrying` and `Msg::Frozen` arms clear an **armed**
action, which makes "armed implies `Link::Live`" an invariant and leaves
`confirm` unchanged. An action already **sent** keeps its line: it is a real
request, and `run_connected` hands back a `Msg::Replied` carrying the `Err`
before its loop ends, so the in-flight line always resolves.

One more, smaller: the design's walked-lamb sentence is
`{n} parent-pid descendants`, which reads `1 parent-pid descendants` for a
single lamb. The line uses the singular for one. That is a wording detail
inside the design's own sentence, not a new decision.

---

## What this phase does not build

Each of these is a decision the design already made and wrote down. Every one
is something a reader of this plan will be tempted to add.

- **No `start` key.** the maintainer's ruling, above.
- **No delete, scale, signal or whisper.** Whistle drew that boundary for its
  own non-CLI control surface and the reasons transfer: each takes a parameter
  a dashboard has nowhere to put, or removes an app from the registry.
- **No selector grammar in the filter, and no hybrid.** A1. `Name` and `Fold`
  are exact-match in `shep-core/src/selector.rs`, so the grammar cannot narrow
  as you type, and a half-typed `/re` silently parses as a name search for a
  sheep literally called `/re`.
- **No fold in the filter.** A2. Name only, one rule, nothing to explain.
- **No filter over the bleats feed.** Different pane, different data source.
- **No second `:` prompt for selector-grammar targeting.** A fourth feature.
- **No client-side provisional row state.** A13. `Stopped`, `Restarted` and
  `Reloading` all carry `Vec<ProcessInfo>`, so the table updates from the
  shepherd's own words. An `online, restart sent...` in the STATUS column would
  be a guess printed in the one column whose whole job is to be true.
- **No `Describe` on the two-second poll.** A14. `with_lambs`'s own doc says
  `ListFlock` declines the walk because a flock listing is what an operator
  leaves running in a loop.
- **No change to `ListFlock`.** Reversing the daemon-side split would bill
  `shep flock`, the dogs table, whistle's `list_flock` and lookout's own poll
  for one pane. Not a lookout decision.
- **No compound `Effect` and no `Vec<Effect>`.** Two variants and one dirty
  flag cover every trigger.
- **No modal, no box, no `ratatui::widgets::Clear`.** There is no overlay
  anywhere in `lookout/` and this phase adds none. One rule under the header
  beats a full border for a pane someone reads at 3am.

---

## Baselines, measured today on this machine

**Every check in this plan states what it prints TODAY.** Six shapes of dead
check have been found on this project, several of them inside fixes for earlier
ones: a grep whose pattern misses because the real text has backticks; a zsh
glob whose no-match case errors; an expectation already true at HEAD; a
`tokio::time::timeout` around a synchronous call; `grep -rc ... | wc -l`
counting files rather than matches; and a whole-file grep whose word also
appears in a doc comment or a test name. **Count the call, not the word.** Run
each baseline before you change anything. If one does not print what this plan
says, stop: the check is broken, or the tree moved, and either way the number
downstream of it is worthless.

```bash
grep -c '^=== ' docs/lookout/frames.txt                                    # 14
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l          # 14
grep -c 'Scene::ALL.len(), 14' crates/shep-cli/src/lookout/frames.rs       #  1
grep -rn 'visible_ids' crates/ | wc -l                                     #  0
grep -c 'self\.flock\.keys()' crates/shep-cli/src/lookout/app.rs           #  3
grep -c 'self\.flock\.len()' crates/shep-cli/src/lookout/app.rs            #  3
grep -rn 'InputMode' crates/ | wc -l                                       #  0
grep -rn 'RefreshSelected' crates/ | wc -l                                 #  0
grep -c 'fn send' crates/shep-cli/src/lookout/source.rs                    #  0
grep -c 'DETAIL_ROWS: u16 = 4' crates/shep-cli/src/lookout/view/mod.rs     #  1
grep -rn 'the_detail_pane_never_mentions_lambs' crates/ | wc -l            #  1
grep -rn 'input::map_key(' crates/ | wc -l                                 #  1
grep -rn 'stop is not built yet' crates/ | wc -l                           #  2
```

`find ... | wc -l`, never a bare glob: under zsh a glob with no match raises
`no matches found` and exits non-zero, which is indistinguishable from the
check failing for the reason you cared about.

Two of those need a word about why they are the shape they are:

- `grep -c 'self\.flock\.keys()'` is **3**, not the count of the word "flock".
  The three are `reseat`, `select_at` and `selected_index`, at
  `app.rs:499`, `:527` and `:562`. Those three plus
  `self.flock.values()` in `rows()` (`:547`) and `self.flock.len()` in the
  `SelectLast` arm (`:462`) are exactly the five reads Task 1 moves. After Task
  1 this prints **0**, and that is the check that the refactor is complete
  rather than half done.
- `grep -rn 'stop is not built yet' crates/ | wc -l` is **2**: the literal in
  `app.rs:470` and the `--allow-control` flag's own doc comment in
  `cli.rs:783`. The second is the one a sweep forgets, and it is generated into
  `web/src/data/cli-reference.generated.txt`, so forgetting it publishes a
  false sentence on the website. Step 10.3 handles it.

### The gallery command in the shipped docs is a silent no-op today

`docs/lookout/frames.txt`, `docs/lookout/frames.ansi` and the doc comment on
`write_the_gallery` all say:

```bash
cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery
```

`shep-cli` is now the empty placeholder crate at `crates/shep-cli-redirect/`
(published only to keep the name from being squatted), and it has no `[[bin]]`.
So that command prints
`warning: target filter 'bins' specified, but no targets matched; this is a no-op`,
**exits 0, and writes nothing.** Anyone following the shipped instruction
believes they regenerated the gallery and did not. Measured today:

```bash
cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery; echo "EXIT=$?"   # EXIT=0, nothing written
cargo test -p shep      --lib  --all-features -- --ignored write_the_gallery; echo "EXIT=$?"   # EXIT=0, 1 passed, files rewritten byte-identically
```

The second is the real one, and it is what every "regenerate the gallery" step
below means. Task 3 fixes the three stale copies of the wrong one.

---

## Where the task gate runs, and why not after every task

`shep`'s `lookout` module tree is reached from `main` only through the
`lookout` verb, and several items in it are reached solely from
`#[cfg(test)] mod tests`. `cargo clippy --workspace --all-targets --all-features
-- -D warnings` builds the non-test lib as well, so a function written in one
task and first called in the next **fails the gate in between**, on
`dead_code`, for code that is correct and about to be live. Phase 12b hit
exactly this and recorded the cadence; this phase inherits it.

Four things here are written before their call site exists: `visible_ids` and
`set_filter` (Task 1, wired in Task 2), `App::flock_len` (Task 1, read in Task
3), `FlockSource::send` (Task 4, first useful in Task 5), and the confirm
machine's accessors (Task 7, read in Task 9).

So:

- **After Tasks 1, 2, 4, 5, 7 and 8** run the inner loop only:
  `cargo fmt --all --check`, then `cargo test -p shep --lib --bins --all-features`.
- **After Tasks 3, 6 and 9** run the full task gate. Each of those three is the
  point at which one feature is reachability-complete.
- **After Task 10** run the full task gate and the phase gate.

**Do not reach for `#[allow(dead_code)]`.** It would sit on code that is live
one task later and nobody would take it off. If the gate fails on `dead_code`
at Task 3, 6 or 9, that is a real finding: something got written and never
wired.

---

## Task order

| # | Task | Files | Gate |
|---|---|---|---|
| 1 | One visible sequence under the cursor | `app.rs` | inner |
| 2 | Two input modes and the filter keymap | `input.rs`, `app.rs` | inner |
| 3 | The filter on screen, three frames, docs | `view/status.rs`, `view/mod.rs`, `view/detail.rs`, `frames.rs`, docs | **full** |
| 4 | `FlockSource::send`, `Sent`, `Channels`, `Msg::Replied` | `source.rs`, `link.rs`, `mod.rs`, `app.rs` | inner |
| 5 | `Effect::RefreshSelected` and the coalesced lamb fetch | `app.rs`, `mod.rs` | inner |
| 6 | The lamb line, `DETAIL_ROWS` 4 to 5, two frames, docs | `view/detail.rs`, `view/mod.rs`, `frames.rs`, docs | **full** |
| 7 | The action keys and the confirm state machine | `input.rs`, `app.rs` | inner |
| 8 | The send path and the reply | `app.rs`, `mod.rs` | inner |
| 9 | The actions on screen, five frames, docs | `view/status.rs`, `frames.rs`, docs | **full** |
| 10 | The docs sweep and the phase gate | docs, `cli.rs`, `web/` | **full + phase** |

The order is the design's: filter, then lambs, then actions (A20). The filter
forces the `visible_ids` refactor and the two-mode keymap, which the confirm
state sits on top of. Lambs force the `Sent` channel and `FlockSource::send`,
which the actions then reuse for a request that can stop a running process.
Actions land last, on plumbing already exercised twice by something that
cannot.

---

# Task 1 - one visible sequence under the cursor

`crates/shep-cli/src/lookout/app.rs` only. **No key, no view, no filter UI.**

This is the task the design calls out as "the load-bearing change, and it is a
refactor rather than an addition", and it is the one most likely to be
under-scoped. Today `rows()`, `select_at`, `select_by`, `reseat` and
`selected_index` each walk `self.flock` directly. `app.rs`'s own doc says why
the cursor is an id and not an index: the flock map is replaced wholesale every
two seconds, so an index cursor would silently point at a different sheep. A
filter adds a second way for that to happen, and it is worse, because the map
does not change at all: the rows simply stop being drawn while `j` and `k` keep
stepping over them.

**Why this is its own task.** With an empty filter, `visible_ids()` yields
exactly `self.flock.keys()`, so this task is behaviour-preserving and the
twenty-one existing `app.rs` tests are its regression suite unchanged. If Tasks
2 and 3 need rework, this one stands on its own and stays merged.

### Step 1.1 - baseline

```bash
grep -c 'self\.flock\.keys()' crates/shep-cli/src/lookout/app.rs   # 3
grep -c 'self\.flock\.len()'  crates/shep-cli/src/lookout/app.rs   # 3
grep -rn 'visible_ids' crates/ | wc -l                             # 0
cargo test -p shep --lib --all-features -- lookout::app            # 21 passed
```

### Step 1.2 - RED

Add to `app.rs`'s `mod tests`. Every one of these fails to compile first
(`set_filter` and `flock_len` do not exist), which is the intended red.

```rust
    /// A dashboard whose filter is set without any keymap involved. Task 2
    /// wires `/` to this; this task proves the sequence underneath it.
    ///
    /// Four sheep, two of which contain `web`: `web` at id 1 and `web-worker`
    /// at id 3, with `api` at id 2 sitting BETWEEN them in the map. The gap is
    /// deliberate. It is what makes `j` stepping over a hidden row a
    /// falsifiable claim rather than one a contiguous fixture would pass by
    /// accident.
    fn filtered(query: &str) -> App {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(2, "api", ProcStatus::Online),
                sheep(3, "web-worker", ProcStatus::Online),
                sheep(4, "cron", ProcStatus::Online),
            ],
            at: t0,
        });
        app.set_filter(query.to_string());
        app
    }

    /// fails if the table stops narrowing, or if the flock's real size stops
    /// being available beside the narrowed one. The title reads both numbers
    /// and a title that could only read the narrowed one would understate the
    /// flock, which is the same confident wrong number the `-` CPU cell and
    /// the frozen uptime rule exist to prevent.
    #[test]
    fn a_filter_narrows_the_rows_and_leaves_the_real_size_readable() {
        let app = filtered("web");
        assert_eq!(app.rows().len(), 2, "web and web-worker");
        assert_eq!(app.flock_len(), 4, "the flock did not get smaller");
    }

    /// fails if the filter matches whole names instead of substrings, which is
    /// precisely the failure the CLI's selector grammar would have had:
    /// `ProcessSelector`'s `Name` compares with `==`, so typing `w`, `we`,
    /// `web` toward `web-worker` matches nothing at every step.
    #[test]
    fn the_filter_matches_a_substring_and_not_a_whole_name() {
        assert_eq!(filtered("wor").rows().len(), 1, "web-worker, by its middle");
        assert_eq!(filtered("w").rows().len(), 2);
    }

    /// fails if either `to_lowercase` is dropped. Both directions, because
    /// dropping one of the two leaves the other test passing.
    #[test]
    fn the_filter_ignores_case_in_both_directions() {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![sheep(1, "WebEdge", ProcStatus::Online)],
            at: t0,
        });
        app.set_filter("webedge".to_string());
        assert_eq!(app.rows().len(), 1, "a lowercase query against a mixed name");
        app.set_filter("WEBEDGE".to_string());
        assert_eq!(app.rows().len(), 1, "and an uppercase one");
    }

    /// fails if `select_by` walks the whole flock again. This is the whole
    /// point of the task: `j` from the first visible row must land on the
    /// second VISIBLE row, not on whatever id happens to sit next in the map.
    #[test]
    fn j_and_k_step_only_over_visible_rows() {
        let mut app = filtered("web");
        assert_eq!(app.selected(), Some(1), "the first visible sheep");
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(app.selected(), Some(3), "web-worker, skipping api at id 2");
        app.update(Msg::Key(KeyPress::SelectDown));
        assert_eq!(app.selected(), Some(3), "clamped at the last visible row");
        app.update(Msg::Key(KeyPress::SelectUp));
        assert_eq!(app.selected(), Some(1));
    }

    /// fails if `SelectLast` measures the flock rather than the visible set.
    #[test]
    fn select_last_lands_on_the_last_visible_row() {
        let mut app = filtered("web");
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected(), Some(3), "web-worker, not cron at id 4");
    }

    /// fails if a filter that hides the selection snaps to row 0, or drops the
    /// selection entirely while rows are still visible. `reseat`'s shipped
    /// rule is that a lost selection falls to whatever now occupies the same
    /// POSITION, clamped: snapping to the top would throw an operator to the
    /// start of a two hundred sheep flock for typing one more character.
    #[test]
    fn a_filter_that_hides_the_selection_clamps_to_the_nearest_visible_row() {
        let mut app = filtered("");
        app.update(Msg::Key(KeyPress::SelectLast));
        assert_eq!(app.selected(), Some(4), "cron, position 3 of 4");
        app.set_filter("web".to_string());
        assert_eq!(
            app.selected(),
            Some(3),
            "position 3 clamps to the last visible row, which is web-worker"
        );
    }

    /// fails if nothing-matches leaves the selection pointing at a hidden
    /// sheep. Every pane below the table describes the selection, so a
    /// selection nobody can see is four panes describing a sheep that is not
    /// on screen.
    #[test]
    fn nothing_visible_means_nothing_selected() {
        let app = filtered("zzz");
        assert_eq!(app.rows().len(), 0);
        assert_eq!(app.selected(), None);
        assert_eq!(app.selected_row().is_none(), true);
        assert_eq!(app.flock_len(), 4, "the flock is still four sheep");
    }

    /// fails if a snapshot clears the filter, or rebuilds the table from the
    /// unfiltered map. The two-second `ListFlock` reply REPLACES `self.flock`
    /// wholesale and is by far the most frequent message this reducer sees, so
    /// a regression here would make the filter appear to work for two seconds
    /// and then silently widen the table under an operator who is still
    /// reading it, with the title's `2 of 4` the only thing left saying a
    /// filter is on.
    #[test]
    fn a_filter_survives_the_two_second_snapshot() {
        let mut app = filtered("web");
        let t1 = Instant::now();
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(2, "api", ProcStatus::Online),
                sheep(3, "web-worker", ProcStatus::Online),
                sheep(4, "cron", ProcStatus::Online),
            ],
            at: t1,
        });
        assert_eq!(app.filter(), "web", "the snapshot did not clear it");
        assert_eq!(app.rows().len(), 2, "and did not widen the table");
        assert_eq!(app.flock_len(), 4);
    }

    /// fails if clearing the filter does not bring the whole flock back, or
    /// leaves the selection unseated. An empty query is the same as no filter,
    /// which is also what `Enter` on an empty box has to mean.
    #[test]
    fn an_empty_query_is_the_same_as_no_filter() {
        let mut app = filtered("zzz");
        app.set_filter(String::new());
        assert_eq!(app.rows().len(), 4);
        assert_eq!(app.selected(), Some(1), "seated again");
    }
```

Run them: they do not compile. That is the red. **Nine tests, not eight** (the
verify step below counts them).

### Step 1.3 - GREEN

Three edits in `app.rs`.

**a. The field.** In `struct App`, after `selected`:

```rust
    /// The live substring filter over sheep NAMES, empty when there is none.
    ///
    /// Case-insensitive `contains`, and nothing else: not the CLI's selector
    /// grammar, not a regex, no understanding of `fold:`, `all` or ids. The
    /// grammar is exact-match on both the variants an operator would type
    /// while narrowing, so it cannot narrow as you type, and a half-typed
    /// `/re` parses as a search for a sheep literally named `/re` rather than
    /// refusing. See the design's feature 1 and assumption A1.
    ///
    /// Taken literally, spaces included, with no trimming (A6): this repo does
    /// not widen an accepted input format without a basis in the spec.
    filter: String,
```

Initialise it to `String::new()` in `App::new`.

**b. The one sequence, and the five reads that move onto it.**

```rust
    /// The ids the table draws, in id order: the whole flock, or whatever the
    /// filter leaves of it.
    ///
    /// [`Self::rows`], [`Self::select_at`], [`Self::select_by`],
    /// [`Self::reseat`] and [`Self::selected_index`] all read this sequence
    /// and nothing else. That is the whole point of it: a filter that hid rows
    /// `j` and `k` still stepped over would move the cursor onto a sheep
    /// nobody can see, and every pane below the table would then describe that
    /// sheep with nothing on screen saying so.
    ///
    /// One lowercase allocation per call, for the query only. The flock is a
    /// `BTreeMap` an operator is looking at, so it is tens of rows, not
    /// thousands, and a cached needle would be a second source of truth for
    /// [`Self::filter`].
    fn visible_ids(&self) -> impl Iterator<Item = u32> + '_ {
        let needle = self.filter.to_lowercase();
        self.flock.iter().filter_map(move |(id, row)| {
            let shown = needle.is_empty() || row.info.name.to_lowercase().contains(&needle);
            shown.then_some(*id)
        })
    }

    /// How many rows the table draws.
    fn visible_len(&self) -> usize {
        self.visible_ids().count()
    }
```

Then, in order:

```rust
            KeyPress::SelectLast => self.select_at(self.visible_len().saturating_sub(1)),
```

```rust
    fn reseat(&mut self, previous_index: Option<usize>) -> bool {
        // `selected_index`, NOT `flock.contains_key`: a selection the filter
        // is hiding is not seated, however present its id still is in the map.
        // This one line is the difference between a cursor that follows the
        // filter and one that wanders behind it.
        //
        // It comes FIRST, where the shipped `flock.is_empty()` check used to.
        // The order is behaviour-preserving (a seated selection implies at
        // least one visible row), and it is what makes Step 1.6's mutation
        // able to reach the nothing-matches case: with the emptiness test
        // above it, reverting this line changes nothing when the query
        // matches no sheep, because the early return has already fired.
        if self.selected_index().is_some() {
            return false;
        }
        let before = self.selected;
        let visible = self.visible_len();
        if visible == 0 {
            self.selected = None;
            return before != self.selected;
        }
        let index = previous_index.unwrap_or(0).min(visible - 1);
        // Bound first, then assigned. `self.selected = self.visible_ids()
        // .nth(index);` does NOT compile: `visible_ids` returns
        // `impl Iterator<Item = u32> + '_`, whose temporary holds a shared
        // borrow of ALL of `self` to the end of the statement, so the
        // assignment overlaps it and rustc raises E0506. The line this
        // replaces borrowed one FIELD (`self.flock.keys()`), which is why it
        // was fine. `select_at` below takes the same two-line shape for the
        // same reason. Verified with rustc on an edition-2024 scaffold, both
        // ways round.
        let next = self.visible_ids().nth(index);
        self.selected = next;
        before != self.selected
    }
```

```rust
    fn select_at(&mut self, index: usize) -> Effect {
        let visible = self.visible_len();
        if visible == 0 {
            return Effect::None;
        }
        let next = self.visible_ids().nth(index.min(visible - 1));
        if next == self.selected {
            return Effect::None;
        }
        self.selected = next;
        if matches!(self.link, Link::Lost { .. }) {
            return Effect::None;
        }
        Effect::RefreshFeed
    }
```

```rust
    #[must_use]
    pub fn rows(&self) -> Vec<&Row> {
        self.visible_ids()
            .filter_map(|id| self.flock.get(&id))
            .collect()
    }
```

```rust
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.visible_ids().position(|key| key == id)
    }
```

`select_by` needs no edit: it already reads `selected_index` and calls
`select_at`, both of which now speak in visible positions.

**c. The filter's own setter, and the flock's real size.**

```rust
    /// Replaces the filter and puts the selection back on a visible sheep.
    ///
    /// Private, and its only production caller is [`Self::on_key`]'s text-mode
    /// arm (Task 2). The reseat is the whole reason this is not a plain field
    /// assignment: a keystroke that narrows the query can hide the selected
    /// sheep, and the selection then falls to whatever occupies the same
    /// position, clamped to the last visible row, which is `reseat`'s shipped
    /// rule applied to a new cause.
    fn set_filter(&mut self, query: String) -> Effect {
        if self.filter == query {
            return Effect::None;
        }
        let previous = self.selected_index();
        self.filter = query;
        if self.reseat(previous) && !matches!(self.link, Link::Lost { .. }) {
            // The cursor moved, so the feed is about to describe a different
            // sheep. A frozen dashboard reads nothing, for the reason
            // `select_at` already states.
            return Effect::RefreshFeed;
        }
        Effect::None
    }

    /// The filter as typed, empty when there is none.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// How many sheep the shepherd last reported, whatever the filter hides.
    ///
    /// The title reads this beside `rows().len()`. A title that could only
    /// read the narrowed count would understate the flock while a filter is
    /// on, which is the confident wrong number this dashboard's `-` cells and
    /// its frozen uptime clock both exist to avoid.
    #[must_use]
    pub fn flock_len(&self) -> usize {
        self.flock.len()
    }
```

### Step 1.4 - verify

```bash
cargo fmt --all --check
cargo test -p shep --lib --bins --all-features
```

Expect roughly `567 passed; 0 failed; 3 ignored` (558 plus the nine above).
Every pre-existing `app.rs` test must still pass untouched. `grep -c
'self\.flock\.keys()' crates/shep-cli/src/lookout/app.rs` now prints **0** and
`grep -c 'self\.flock\.len()'` prints **1** (`flock_len`, which is the one
read of the real size that is supposed to survive); those two, and not the test
count, are the check that the refactor is complete.

Clippy will warn `dead_code` on `set_filter`, `filter` and `flock_len` in the
non-test build. That is expected and Task 2 and Task 3 clear it. Do not run the
full gate here and do not add an `allow`.

### Step 1.5 - MUTATION

Point `select_by`'s sequence back at the whole flock, exactly as the design
names it: in `selected_index`, swap `self.visible_ids()` for
`self.flock.keys().copied()`.

**Must redden `j_and_k_step_only_over_visible_rows` on its final `SelectUp`**,
and `a_filter_that_hides_the_selection_clamps_to_the_nearest_visible_row`, and
`nothing_visible_means_nothing_selected`.

Name the assertion, because the obvious guess is wrong and a mutation whose
stated reason does not match what happens is a check nobody can audit. The
first `SelectDown` still lands on `web-worker` at id 3, not on the hidden `api`
at id 2, because `select_at` walks `visible_ids()` and only the index LOOKUP is
mutated. What breaks is the round trip: `selected_index` reports id 3's
position in the whole flock (2), `select_by(-1)` asks for position 1, and
`select_at` reads position 1 of the VISIBLE set, which is id 3 again, so the
cursor never comes back up. Measured on the scaffold: `left: Some(3), right:
Some(1)`. It reddens the other two because `reseat`'s guard reads
`selected_index` as well.

### Step 1.6 - second MUTATION

In `reseat`, put `self.selected.is_some_and(|id| self.flock.contains_key(&id))`
back in place of `self.selected_index().is_some()`.

**Must redden `a_filter_that_hides_the_selection_clamps_to_the_nearest_visible_row`**
(the selection stays on `cron`, which is hidden) **and
`nothing_visible_means_nothing_selected`** (it stays on `web` at id 1, which
`zzz` hides). Both, and both only because Step 1.3 put the guard ABOVE the
`visible == 0` test: with the emptiness test first, the nothing-matches case
returns before the guard is ever read and the second half of this mutation is
a dead check. Measured both orders on a scaffold. `j_and_k_step_only_over_
visible_rows` stays green here, correctly, because it never exercises `reseat`.

This is the subtlest line in the task: the mutated version passes every test
that does not set a filter, which is every test that existed before today.
Revert.

---

# Task 2 - two input modes and the filter keymap

`crates/shep-cli/src/lookout/input.rs` and `app.rs`. Still no view change.

`map_key` becomes a function of the crossterm event **and the mode**, which
keeps the terminal edge in the one file whose stated job that is and keeps
`app.rs` free of terminal types. The mode lives in the reducer, so `run_ui`
passes `app.mode()` at the call site.

### Step 2.1 - baseline

```bash
grep -rn 'InputMode' crates/ | wc -l          # 0
grep -rn 'input::map_key(' crates/ | wc -l    # 1  (run_ui's keyboard arm)
grep -c "KeyCode::Char('/')" crates/shep-cli/src/lookout/input.rs   # 0, the key is free
cargo test -p shep --lib --all-features -- lookout::input   # 4 passed
```

### Step 2.2 - RED

In `input.rs`'s `mod tests`:

```rust
    /// fails if `map_key` starts ignoring its mode. While the filter box is
    /// open every printable key is text, `q` included, and the status bar says
    /// so for as long as that is true. A keymap that read `q` as quit while
    /// somebody was typing a sheep name would close the dashboard mid-word.
    #[test]
    fn typing_q_while_editing_types_a_letter() {
        assert_eq!(
            map_key(&key(KeyCode::Char('q')), InputMode::Text),
            Some(KeyPress::FilterChar('q'))
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('q')), InputMode::Normal),
            Some(KeyPress::Quit)
        );
    }

    /// fails if the text mode stops accepting the keys the box needs, or
    /// starts accepting keys it must not. `Esc` abandons, `Enter` applies,
    /// `Backspace` deletes, Ctrl-C still quits, and an unbound key such as
    /// `F5` types nothing.
    #[test]
    fn the_text_mode_binds_exactly_the_box_s_keys() {
        assert_eq!(
            map_key(&key(KeyCode::Backspace), InputMode::Text),
            Some(KeyPress::FilterBackspace)
        );
        assert_eq!(
            map_key(&key(KeyCode::Enter), InputMode::Text),
            Some(KeyPress::FilterApply)
        );
        assert_eq!(
            map_key(&key(KeyCode::Esc), InputMode::Text),
            Some(KeyPress::FilterAbandon)
        );
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(map_key(&ctrl_c, InputMode::Text), Some(KeyPress::Quit));
        assert_eq!(map_key(&key(KeyCode::F(5)), InputMode::Text), None);
    }

    /// fails if a shifted letter stops reaching the box. Crossterm delivers a
    /// capital as `Char('W')` with `SHIFT` set, and a mode that filtered on
    /// `modifiers.is_empty()` would swallow every capital in a sheep's name.
    #[test]
    fn a_shifted_letter_is_still_a_letter_in_the_box() {
        let shifted = Event::Key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT));
        assert_eq!(map_key(&shifted, InputMode::Text), Some(KeyPress::FilterChar('W')));
    }

    /// fails if `/` stops opening the box in normal mode.
    #[test]
    fn slash_opens_the_filter_in_normal_mode() {
        assert_eq!(
            map_key(&key(KeyCode::Char('/')), InputMode::Normal),
            Some(KeyPress::FilterStart)
        );
    }
```

`every_bound_key_resolves_to_its_press` gains the mode argument on every call
and its `Esc` expectation changes from `KeyPress::Quit` to
`KeyPress::Escape` - the reducer, not the keymap, decides what `Esc` means,
because the answer depends on whether a filter is set. Its doc comment gains
the sentence saying so. The other three `input.rs` tests take
`InputMode::Normal` and are otherwise unchanged.

In `app.rs`'s `mod tests`:

```rust
    /// fails if `Esc` starts quitting out from under a filter. It is the one
    /// key whose meaning depends on state, which is acceptable only because
    /// the status bar reads `esc clear` for as long as that is in force and
    /// the title keeps `2 of 4 in the flock` on screen.
    #[test]
    fn esc_clears_the_filter_instead_of_quitting_while_one_is_set() {
        let mut app = filtered("web");
        assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::RefreshFeed);
        assert_eq!(app.filter(), "");
        assert_eq!(app.rows().len(), 4);
    }

    /// fails if the clear becomes unconditional, which is the mirror bug: `q`
    /// and Ctrl-C quit from every non-editing state, and so does `Esc` when
    /// there is no filter to take away first.
    #[test]
    fn esc_still_quits_with_no_filter_set() {
        let (mut app, _t0) = started();
        assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::Quit);
    }

    /// fails if typing into the box stops narrowing the table live, or if
    /// `Enter` narrows it a second time. The design's `filter_editing` frame
    /// is mid-type with the table already narrowed; applying only changes
    /// which keys mean what.
    #[test]
    fn the_table_narrows_while_the_query_is_still_being_typed() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        assert_eq!(app.mode(), InputMode::Text);
        for letter in ['w', 'e', 'b'] {
            app.update(Msg::Key(KeyPress::FilterChar(letter)));
        }
        assert_eq!(app.rows().len(), 1, "narrowed before Enter");
        app.update(Msg::Key(KeyPress::FilterApply));
        assert_eq!(app.mode(), InputMode::Normal);
        assert_eq!(app.rows().len(), 1, "and applying changed nothing but the mode");
    }

    /// fails if backspace stops widening the table again.
    #[test]
    fn backspace_widens_the_table_back_out() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        app.update(Msg::Key(KeyPress::FilterChar('w')));
        app.update(Msg::Key(KeyPress::FilterChar('z')));
        assert_eq!(app.rows().len(), 0);
        app.update(Msg::Key(KeyPress::FilterBackspace));
        assert_eq!(app.rows().len(), 2, "wz became w, which matches web and worker");
    }

    /// fails if abandoning the box leaves the filter behind. `Esc` while
    /// editing clears and leaves; the two halves are one action.
    #[test]
    fn esc_while_editing_clears_the_filter_and_leaves_the_box() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        app.update(Msg::Key(KeyPress::FilterChar('w')));
        app.update(Msg::Key(KeyPress::FilterAbandon));
        assert_eq!(app.mode(), InputMode::Normal);
        assert_eq!(app.filter(), "");
        assert_eq!(app.rows().len(), 3);
    }

    /// fails if a notice survives into the filter box. Two things keep the
    /// query visible and this is one of them: opening the box takes any
    /// standing notice down (the `self.notice = None` at the top of `on_key`'s
    /// normal branch, which `FilterStart` goes through), and slot 2 of the bar
    /// keeps a notice raised LATER, while the box is open, from covering it.
    /// The second is what actually matters, because `Msg::BusLagged`,
    /// `BusEvent::Dropped` and `BusEvent::DaemonShutdown` all raise notices
    /// with no keypress involved and keep arriving while somebody types; see
    /// "Shapes the design named" #2. This test pins the first, which is what
    /// stops a stale notice reappearing the instant the box closes.
    #[test]
    fn opening_the_filter_takes_a_notice_off_the_bar() {
        let (mut app, _t0) = started();
        app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
        assert!(app.notice().is_some());
        app.update(Msg::Key(KeyPress::FilterStart));
        assert!(app.notice().is_none(), "the box is what the bar shows now");
    }
```

```rust
    /// fails if a notice raised WHILE the box is open is destroyed rather than
    /// deferred. The rejected fix for the same problem was to clear
    /// `self.notice` at the top of `on_text_key`; it would lose a
    /// `DaemonShutdown` because the operator happened to be mid-word. The bar
    /// hides the notice under the box (slot 2) and this pins that the notice
    /// itself is still there to be shown when the box closes.
    #[test]
    fn a_notice_raised_while_typing_is_deferred_and_not_destroyed() {
        let (mut app, _t0) = started();
        app.update(Msg::Key(KeyPress::FilterStart));
        app.update(Msg::Key(KeyPress::FilterChar('w')));
        app.update(Msg::Event(BusEvent::DaemonShutdown));
        app.update(Msg::Key(KeyPress::FilterChar('e')));
        assert!(
            app.notice().is_some(),
            "typing did not wipe the shepherd's announcement"
        );
        assert_eq!(app.filter(), "we", "and the box kept the query");
    }
```

`started()` builds three sheep named `web`, `api` and `worker`, at ids 1, 2
and 3. So `w` matches `web` AND `worker` (two rows), `we` matches `web` alone,
and `wz` matches nothing. Check that against the fixture at the top of
`app.rs`'s `mod tests` before trusting the numbers above and adjust the
literals, not the assertions, if it has drifted.

### Step 2.3 - GREEN: `app.rs`

`KeyPress` grows six variants and `Escape` splits off `Quit`. Keep it `Copy`;
every payload here is `Copy`.

```rust
/// Which keymap is in force.
///
/// Held by the reducer and passed to [`super::input::map_key`] at the call
/// site, so the crossterm edge stays in one file and this module keeps holding
/// no terminal types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// The ordinary dashboard keys.
    Normal,
    /// The filter box is open and every printable key is text.
    Text,
}
```

```rust
pub enum KeyPress {
    /// `q` in normal mode, or `Ctrl-C` in either.
    Quit,
    /// `Esc` in normal mode. Cancels an armed confirm if there is one, else
    /// clears the filter if there is one, else quits. The reducer decides,
    /// because the keymap cannot see any of those three states.
    Escape,
    // ... SelectUp, SelectDown, SelectFirst, SelectLast, Refresh unchanged ...
    /// `/` - open the filter box, carrying whatever query is already set.
    FilterStart,
    /// One printable character typed into the box.
    FilterChar(char),
    /// `Backspace` in the box.
    FilterBackspace,
    /// `Enter` in the box: apply and leave.
    FilterApply,
    /// `Esc` in the box: clear the filter and leave.
    FilterAbandon,
}
```

Add `mode: InputMode` to `App` (init `InputMode::Normal`) and a `pub fn mode`
accessor. Then `on_key`:

```rust
    fn on_key(&mut self, key: KeyPress) -> Effect {
        // While the box is open every KEY is text. That is not the same as
        // nothing being able to raise a notice: three of them arrive on
        // messages that are not keys at all, and the status bar rather than
        // this branch is what keeps them off the query. See `on_text_key`.
        if self.mode == InputMode::Text {
            return self.on_text_key(key);
        }
        self.notice = None;
        match key {
            KeyPress::Quit => Effect::Quit,
            // The one key whose meaning depends on state, and the screen says
            // which meaning is in force: the bar reads `esc clear` for exactly
            // as long as clearing is what it does.
            KeyPress::Escape => {
                if self.filter.is_empty() {
                    Effect::Quit
                } else {
                    self.set_filter(String::new())
                }
            }
            KeyPress::FilterStart => {
                self.mode = InputMode::Text;
                Effect::None
            }
            // ... Refresh, SelectUp, SelectDown, SelectFirst, SelectLast,
            //     Stop: unchanged from what shipped ...
            // `map_key` produces these only in text mode, which the branch at
            // the top of this function has already taken. Named rather than
            // wildcarded so a future variant does not fall silently into an
            // arm that ignores it.
            KeyPress::FilterChar(_)
            | KeyPress::FilterBackspace
            | KeyPress::FilterApply
            | KeyPress::FilterAbandon => Effect::None,
        }
    }

    /// The filter box's keymap.
    ///
    /// Ctrl-C still quits: in raw mode it is a key event and not a signal, and
    /// a text box that swallowed it would leave the operator reaching for
    /// `kill -9` from another window, past every restore path
    /// [`super::term`] has.
    ///
    /// Deliberately does NOT clear [`Self::notice`], unlike the normal-mode
    /// branch above it. A notice can be raised while this box is open, by
    /// `Msg::BusLagged`, `BusEvent::Dropped` or `BusEvent::DaemonShutdown`,
    /// none of which is a keypress; clearing here would destroy it because
    /// somebody was mid-word. The status bar hides it under the box instead
    /// and shows it when the box closes. See the phase plan's
    /// "Shapes the design named" #2.
    fn on_text_key(&mut self, key: KeyPress) -> Effect {
        match key {
            KeyPress::Quit => Effect::Quit,
            KeyPress::FilterChar(typed) => {
                let mut query = self.filter.clone();
                query.push(typed);
                self.set_filter(query)
            }
            KeyPress::FilterBackspace => {
                let mut query = self.filter.clone();
                query.pop();
                self.set_filter(query)
            }
            KeyPress::FilterApply => {
                self.mode = InputMode::Normal;
                Effect::None
            }
            KeyPress::FilterAbandon => {
                self.mode = InputMode::Normal;
                self.set_filter(String::new())
            }
            _ => Effect::None,
        }
    }
```

`FilterStart` clears the notice through the `self.notice = None` above it, and
an empty query applied with `Enter` needs no special case: an empty
[`Self::filter`] already means no filter.

### Step 2.4 - GREEN: `input.rs`

```rust
pub fn map_key(event: &Event, mode: InputMode) -> Option<KeyPress> {
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
    if mode == InputMode::Text {
        return match key.code {
            // SHIFT is not filtered out: crossterm delivers a capital as
            // `Char('W')` with SHIFT set, and a box that dropped it could not
            // type half the sheep names in a flock. ALT is, because an
            // `Alt-w` is a command somewhere and never a letter here.
            KeyCode::Char(typed) if !key.modifiers.contains(KeyModifiers::ALT) => {
                Some(KeyPress::FilterChar(typed))
            }
            KeyCode::Backspace => Some(KeyPress::FilterBackspace),
            KeyCode::Enter => Some(KeyPress::FilterApply),
            KeyCode::Esc => Some(KeyPress::FilterAbandon),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Some(KeyPress::Quit),
        KeyCode::Esc => Some(KeyPress::Escape),
        KeyCode::Char('/') => Some(KeyPress::FilterStart),
        // ... j/k/g/G/r/x and the arrow aliases, unchanged ...
        _ => None,
    }
}
```

In `run_ui`'s keyboard arm, the one call site becomes
`input::map_key(&event, app.mode()).map(Msg::Key)`.

### Step 2.5 - verify

```bash
cargo fmt --all --check
cargo test -p shep --lib --bins --all-features
```

Roughly `578 passed` (567 plus eleven: four in `input.rs` and seven in
`app.rs`, per Step 2.2 above). `failed` stays 0.

### Step 2.6 - MUTATION

Make `map_key` ignore its mode: delete the `if mode == InputMode::Text` block.

**Must redden `typing_q_while_editing_types_a_letter`** and
`the_text_mode_binds_exactly_the_box_s_keys`. Revert.

### Step 2.7 - second MUTATION

Make the clear unconditional: in the `KeyPress::Escape` arm, drop the
`if self.filter.is_empty()` test and always `set_filter(String::new())`.

**Must redden `esc_still_quits_with_no_filter_set`**, which is the mirror bug
the design names: an `Esc` that never quits is as wrong as one that always
does. Revert.

---

# Task 3 - the filter on screen, three frames, and the docs

`view/status.rs`, `view/mod.rs`, `view/detail.rs`, `frames.rs`, plus the two
prose documents. **This task ends with the full task gate**, because it is the
first point at which every item Tasks 1 and 2 wrote has a production caller.

### Step 3.1 - baseline

```bash
grep -c '^=== ' docs/lookout/frames.txt                             # 14
find crates/shep-cli/src/lookout/snapshots -name '*.snap' | wc -l   # 14
grep -c 'Scene::ALL.len(), 14' crates/shep-cli/src/lookout/frames.rs  # 1
grep -c 'r refresh' crates/shep-cli/src/lookout/view/status.rs      # 1
grep -rn 'cargo test -p shep-cli --bins' docs/lookout/ crates/shep-cli/src/ | wc -l   # 4
```

That last one is the silent no-op command, in `frames.txt`, `frames.ansi` and
twice in `frames.rs`. This task fixes all four.

### Step 3.2 - RED

`view/status.rs`:

```rust
    /// fails if the title stops carrying the flock's real size while a filter
    /// is on. `{visible} in the flock` alone understates the flock, and an
    /// operator who cannot see that a filter is hiding rows is an operator
    /// about to conclude that sheep have vanished.
    #[test]
    fn the_title_counts_both_numbers_while_a_filter_is_on() {
        let app = filtered_app("web");
        let title = rendered(&title_line(&app, "/home/ada/.shep", 120));
        assert!(title.contains("2 of 4 in the flock"), "got {title:?}");
    }

    /// fails if the unfiltered title changes at all. It is on every frame in
    /// the gallery and nothing about this feature touches it.
    #[test]
    fn the_unfiltered_title_is_unchanged() {
        let app = filtered_app("");
        let title = rendered(&title_line(&app, "/home/ada/.shep", 120));
        assert!(title.contains("4 in the flock"), "got {title:?}");
        assert!(!title.contains(" of "), "no second number when nothing is hidden");
    }

    /// fails if the bar stops saying what the two filter keys do. The whole
    /// argument for `esc` meaning something different while a filter is set is
    /// that the screen says so at the moment it is true.
    #[test]
    fn the_bar_names_the_filter_keys_while_a_filter_is_applied() {
        let app = filtered_app("web");
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("filter \"web\""), "the query, quoted: {bar:?}");
        assert!(bar.contains("/ edit"), "got {bar:?}");
        assert!(bar.contains("esc clear"), "got {bar:?}");
    }

    /// fails if the box stops showing what is being typed, or stops naming the
    /// only three keys that mean anything while it is open.
    #[test]
    fn the_bar_carries_the_query_and_a_cursor_while_editing() {
        let app = editing_app("we");
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("filter  we\u{258f}"), "query then cursor: {bar:?}");
        assert!(bar.contains("enter applies"), "got {bar:?}");
        assert!(bar.contains("esc cancels"), "got {bar:?}");
        assert!(bar.contains("ctrl-c quits"), "got {bar:?}");
    }

    /// fails if a notice covers the box. A `Dropped` event arrives with no
    /// keypress and `on_text_key` does not clear notices, so a bar that
    /// ranked the notice higher would take the operator's half-typed query
    /// off the screen and leave it off until they pressed Enter or Esc.
    #[test]
    fn a_notice_raised_while_typing_does_not_cover_the_box() {
        let mut app = editing_app("we");
        app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("filter  we\u{258f}"), "the box is still there: {bar:?}");
        assert!(!bar.contains("dropped 3 events"), "got {bar:?}");
    }

    /// fails if the deferred notice never gets its turn. The mirror of the
    /// test above: hiding a notice under the box is only honest if closing the
    /// box shows it.
    #[test]
    fn closing_the_box_shows_the_notice_that_was_waiting() {
        let mut app = editing_app("we");
        app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
        app.update(Msg::Key(KeyPress::FilterApply));
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("dropped 3 events"), "got {bar:?}");
    }

    /// fails if `/` stops being advertised. It is the only way into the box
    /// and nothing else on the screen hints at it.
    #[test]
    fn the_read_only_hint_advertises_the_filter_key() {
        let app = filtered_app("");
        let hint = rendered(&status_line(&app, 200));
        assert!(hint.contains("/ filter"), "got {hint:?}");
    }
```

`view/mod.rs`:

```rust
    /// fails if a filter that matches nothing says the flock is empty. It is
    /// not empty, and that sentence belongs to the case it describes. Three
    /// panes say three different things here because there are three different
    /// reasons, which is the `empty` scene's own principle.
    #[test]
    fn a_filter_matching_nothing_does_not_say_the_flock_is_empty() {
        let app = filtered_app("zzz");
        let frame = draw_to(&app, 120, 30);
        assert!(
            frame.contains("no sheep's name contains \"zzz\""),
            "the table body names the query: {frame:?}"
        );
        assert!(
            !frame.contains("the flock is empty"),
            "and does not claim the flock is: {frame:?}"
        );
        assert!(
            frame.contains("no sheep selected: no name contains \"zzz\""),
            "the detail pane says its own reason: {frame:?}"
        );
        assert!(
            frame.contains("bleats  no sheep is selected"),
            "the feed's sentence is already true and is unchanged: {frame:?}"
        );
    }

    /// fails if the genuinely empty flock loses its own sentence. The mirror
    /// of the test above: one of the two branches getting the other's text is
    /// the failure, and only asserting both catches it.
    #[test]
    fn an_empty_flock_still_says_the_flock_is_empty() {
        let app = filtered_app_of(Vec::new(), "");
        let frame = draw_to(&app, 120, 30);
        assert!(frame.contains("the flock is empty"), "got {frame:?}");
        assert!(!frame.contains("no sheep's name contains"), "got {frame:?}");
    }
```

The three new fixtures, plus one private helper they share, go in
`view/fixtures.rs` beside the shipped ones, each building an `App` through
`Msg::Key` presses rather than by touching a private field. Written out, because an implementer guessing at `filtered_app_of`'s
argument order is an implementer writing a different test:

```rust
/// Four sheep at ids 1..=4 named `web`, `api`, `web-worker`, `cron`, with
/// `query` typed into the filter box and applied. An empty `query` leaves the
/// dashboard unfiltered, which is what the "nothing changed" assertions need.
///
/// Two of the four contain `web`, with `api` between them, so a fixture that
/// stepped over hidden rows would show up as a wrong count rather than as a
/// passing test.
pub fn filtered_app(query: &str) -> App {
    filtered_app_of(named_flock(), query)
}

/// [`filtered_app`] over an explicit flock, for the empty-flock mirror.
pub fn filtered_app_of(flock: Vec<ProcessInfo>, query: &str) -> App {
    let mut app = app_with(flock, plain());
    if !query.is_empty() {
        app.update(Msg::Key(KeyPress::FilterStart));
        for typed in query.chars() {
            app.update(Msg::Key(KeyPress::FilterChar(typed)));
        }
        app.update(Msg::Key(KeyPress::FilterApply));
    }
    app
}

/// The same four sheep with `query` half-typed and the box still OPEN: no
/// `FilterApply`, which is the whole difference between this and
/// [`filtered_app`].
pub fn editing_app(query: &str) -> App {
    let mut app = app_with(named_flock(), plain());
    app.update(Msg::Key(KeyPress::FilterStart));
    for typed in query.chars() {
        app.update(Msg::Key(KeyPress::FilterChar(typed)));
    }
    app
}

/// The four named sheep the filter fixtures share. `flock_of` names its sheep
/// `sheep-0`..`sheep-N`, which every query would match or miss together.
fn named_flock() -> Vec<ProcessInfo> {
    [(1, "web"), (2, "api"), (3, "web-worker"), (4, "cron")]
        .into_iter()
        .map(|(id, name)| {
            ProcessInfo::builder(id, name, ProcStatus::Online)
                .pid(Some(48_000 + id))
                .uptime_ms(4_512_000)
                .build()
        })
        .collect()
}
```

`view/status.rs`'s test module builds its `App`s inline today and imports no
fixtures. Add `use super::super::fixtures::{editing_app, filtered_app, rendered};`
to it in this task; `view/mod.rs`'s test module already imports from
`fixtures` and gains `filtered_app` and `filtered_app_of`.

### Step 3.3 - GREEN

**`view/status.rs`, `title_line`:**

```rust
    let visible = app.rows().len();
    let total = app.flock_len();
    let right = if app.filter().is_empty() {
        format!(" {total} in the flock")
    } else {
        format!(" {visible} of {total} in the flock")
    };
```

**`view/status.rs`, `status_line`'s left slot.** Six slots, highest first (see
"Shapes the design named" #2 for why the box and the applied filter line are
two of them and not one). Slots 1 and 4 are Task 9's; leave the `if let` chain
ready for them rather than restructuring twice, but do not write the arms yet.

The box sits **above** the notice. That is the ordering this task must get
right and the one a reader will be tempted to swap back, so the comment says
why in the code rather than only here.

```rust
    let (left, left_style) = if app.mode() == InputMode::Text {
        // ABOVE the notice, not below it. `Msg::BusLagged`,
        // `BusEvent::Dropped` and `BusEvent::DaemonShutdown` all raise notices
        // with no keypress involved, and they keep arriving while somebody is
        // typing a sheep name; a notice that covered the box would take the
        // query off the screen mid-word, and `on_text_key` does not clear
        // notices, so nothing the operator typed would bring it back. The
        // notice is not lost: it is what this slot shows the moment the box
        // closes. A report of a past event does not outrank an interaction in
        // progress.
        //
        // The cursor is a character, not a style: the ANSI gallery renders
        // foregrounds only, and a reversed cell would come out unstyled there.
        // Same call the selection marker already makes.
        (
            format!(
                "filter  {}\u{258f}   enter applies   esc cancels   ctrl-c quits",
                app.filter()
            ),
            palette.attention(),
        )
    } else if let Some(notice) = app.notice() {
        (
            notice.to_string(),
            if notice.is_grave() {
                palette.refusal()
            } else {
                palette.attention()
            },
        )
    } else if !app.filter().is_empty() {
        (
            format!("filter \"{}\"   / edit   esc clear", app.filter()),
            palette.muted(),
        )
    } else {
        (hint_for(app.control()), palette.muted())
    };
```

`status.rs` gains `InputMode` on its `use super::super::app::{...}` line.

with

```rust
/// The key hint.
///
/// One form for both control states in this task, which is what shipped: there
/// is nothing to advertise behind the gate yet. Task 9 gives it a second form
/// once the three action keys exist, at which point this file's standing rule
/// applies to it. Writing that second form here would put `x stop   R restart
/// L reload` on the screen of a build where two of those three keys do
/// nothing, which is the asterisk-instead-of-a-hint failure the rule is about,
/// shipped by the plan rather than by the code.
///
/// The text is 59 characters, up from the 48 that shipped. It still truncates
/// at the 39 columns the 49-column gap test leaves for it, and the first 40
/// characters are byte-identical to the old hint, so
/// `a_truncated_hint_still_leaves_a_gap_before_the_control_label` measures
/// exactly what it was written to measure and the `narrow` and `cramped`
/// frames do not move.
fn hint_for(_control: Control) -> String {
    "q quit   j/k select   g/G first/last   r refresh   / filter".to_string()
}
```

The parameter is taken and ignored on purpose: Task 9 fills it in, and a
signature that grew an argument later would touch the call site twice. Name it
`_control` so the ordinary build is quiet.

**Two shipped doc comments in the same file say 48 and become false here.**
Fix them in this task, not in Task 9, because this is the task that changes the
number:

- `a_truncated_hint_still_leaves_a_gap_before_the_control_label`: "the default
  hint is 48 characters, the label 9" becomes 59 and 9. The rest of that
  comment, including the paragraph about why 49 is not tied to the `narrow`
  scene, stays exactly as it is.
- `the_key_hint_says_what_the_keys_now_do`: "The replacement is the same 48
  characters as the original" is now two revisions stale. Say that the hint is
  59 characters and that its first 40 are unchanged, which is the property that
  keeps the 49-column test measuring the same thing.

Neither test's assertions change. Both still pass; it is only their stated
reasons that would otherwise be wrong, which is the class this project keeps
catching in review.

**`view/mod.rs`, the empty-table branch:**

```rust
    if rows.is_empty() {
        // Two sentences, because there are two reasons and an operator cannot
        // tell them apart from a blank table. `the flock is empty` stays for
        // the case it describes and no other.
        let text = if app.flock_len() == 0 {
            "the flock is empty".to_string()
        } else {
            format!("no sheep's name contains \"{}\"", app.filter())
        };
        let line = Line::from(Span::styled(text, palette.muted()));
        buffer.set_line(area.x, y, &line, width);
    } else {
```

**`view/detail.rs`, the no-selection branch:** same split, with the pane's own
wording, and still returning as many lines as `DETAIL_ROWS - 1` (three now,
four after Task 6):

```rust
        let why = if app.flock_len() == 0 {
            "no sheep selected: the flock is empty".to_string()
        } else {
            format!("no sheep selected: no name contains \"{}\"", app.filter())
        };
```

### Step 3.4 - GREEN: three frames

In `frames.rs`, add to `Scene`, to `Scene::ALL` (after `Refused`, so the
gallery reads shell, then panes, then filter, then actions), to `label`, to
`caption`, and to `size`:

```rust
    /// Mid-type: the table has already narrowed and the box is still open.
    FilterEditing,
    /// Applied and no longer editing.
    FilterActive,
    /// A query nothing matches.
    FilterNoMatch,
```

```rust
            Self::FilterEditing => "filter_editing",
            Self::FilterActive => "filter_active",
            Self::FilterNoMatch => "filter_no_match",
```

```rust
            Self::FilterEditing | Self::FilterActive | Self::FilterNoMatch => (100, 14),
```

Captions. **Every clause of every caption is one assertion in
`every_scene_shows_the_thing_it_is_named_for`, or it is deleted from the
caption.** 12a shipped two false captions and needed a fix commit; 12b shipped
a caption describing a state its frame did not show. Write the caption after
looking at the rendered frame, never before.

```rust
            Self::FilterEditing => {
                "Mid-type at 100x14. The table has already narrowed to the two sheep whose names contain the query, the title counts the narrowed set and the whole flock, and the status bar carries the query, a cursor, and the three keys that mean anything while the box is open."
            }
            Self::FilterActive => {
                "The same query applied. The box is closed, the table is still narrowed, and the bar has changed to name the two keys that now touch the filter."
            }
            Self::FilterNoMatch => {
                "A query nothing matches. The table names the query rather than claiming the flock is empty, and the title keeps the flock's real size on screen."
            }
        }
```

In `scene_with`, after the existing selection block and before the host sample,
apply the keys. Going through `Msg::Key` rather than through a private field is
deliberate: the scene then exercises the same path the operator does, so a
keymap regression reddens the gallery.

```rust
    match which {
        Scene::FilterEditing | Scene::FilterNoMatch | Scene::FilterActive => {
            app.update(Msg::Key(KeyPress::FilterStart));
            let query = if which == Scene::FilterNoMatch { "zzz" } else { "web" };
            for typed in query.chars() {
                app.update(Msg::Key(KeyPress::FilterChar(typed)));
            }
            if which == Scene::FilterActive {
                app.update(Msg::Key(KeyPress::FilterApply));
            }
        }
        _ => {}
    }
```

The three filter scenes take the default six-sheep flock, whose ids 0 and 1 are
both named `web`, so `web` narrows six rows to two and the title reads
`2 of 6 in the flock`, matching the design's sketch.

Assertions, one per caption clause:

```rust
        // "Mid-type at 100x14. The table has already narrowed to the two sheep
        //  whose names contain the query, the title counts the narrowed set
        //  and the whole flock, and the status bar carries the query, a
        //  cursor, and the three keys that mean anything while the box is
        //  open."
        let editing = render_text(&scene(Scene::FilterEditing).1);
        assert_eq!(
            editing
                .lines()
                .filter(|line| line.contains("  web  "))
                .count(),
            2,
            "two rows survived the query"
        );
        assert!(!editing.contains("billing"), "and the rest did not");
        assert!(editing.contains("2 of 6 in the flock"), "got {editing:?}");
        assert!(editing.contains("filter  web\u{258f}"), "the query and the cursor");
        for named in ["enter applies", "esc cancels", "ctrl-c quits"] {
            assert!(editing.contains(named), "the box names {named}");
        }

        // "The same query applied. The box is closed, the table is still
        //  narrowed, and the bar has changed to name the two keys that now
        //  touch the filter."
        let active = render_text(&scene(Scene::FilterActive).1);
        assert!(active.contains("filter \"web\""), "the box is closed");
        assert!(!active.contains("enter applies"), "and its keys are gone");
        assert!(active.contains("2 of 6 in the flock"), "still narrowed");
        assert!(active.contains("/ edit") && active.contains("esc clear"));

        // "A query nothing matches. The table names the query rather than
        //  claiming the flock is empty, and the title keeps the flock's real
        //  size on screen."
        let none = render_text(&scene(Scene::FilterNoMatch).1);
        assert!(none.contains("no sheep's name contains \"zzz\""));
        assert!(!none.contains("the flock is empty"));
        assert!(none.contains("0 of 6 in the flock"));
```

`assert_eq!(Scene::ALL.len(), 14)` becomes `17`.

### Step 3.5 - the shipped snapshots, and the one caption that just became false

**Ten of the fourteen `.snap` files change, not all fourteen.** The four that
do not are worth naming, because "all fourteen changed" as an acceptance check
would pass a diff in which four frames moved for a reason nobody looked at:

| frame | why it does not move |
|---|---|
| `too_narrow` | 28 columns, below `MIN_TERM_WIDTH` (33). `draw` returns two short lines and never reaches the status bar. |
| `refused` | Its bar carries the read-only notice, so the hint is not what is drawn. |
| `narrow` | 51 columns. The bar truncates the hint at 41 characters and the old and new hints share their first 40, so it renders `...   r…` either way. |
| `cramped` | 33 columns. Same, truncated at 23. |

The other ten gain `   / filter`. Re-accept them deliberately, then read the
diff:

```bash
cargo test -p shep --lib --all-features -- lookout::frames    # red: 10 changed, 3 new
cargo insta review                                            # or: INSTA_FORCE_UPDATE=1 cargo test ...
git diff --stat crates/shep-cli/src/lookout/snapshots/        # 10 changed files
```

The diff must be exactly ten files, and the only change in each must be the
status-bar row. If a table row, a title or a pane line moved, something else
moved with it and the task is not done; if one of the four above appears in the
diff, work out why before accepting it.

**One shipped caption becomes false in this phase and must be fixed here rather
than in Task 9.** `Scene::Refused`'s caption reads:

> "`x` with actions gated off. Both refusals are literal ... and the panes
> below carry on."

"Both refusals" counts the read-only refusal and `stop is not built yet`. Task
8 deletes the second, so the clause stops being traceable to anything. Change
it now to name one refusal:

```rust
            Self::Refused => {
                "`x` with actions gated off. The refusal is literal, nothing about damage gets charming, and the panes below carry on."
            }
```

and check the assertion under it still pins each surviving clause.

### Step 3.6 - regenerate the gallery, with the command that works

```bash
cargo test -p shep --lib --all-features -- --ignored write_the_gallery; echo "EXIT=$?"   # 0
grep -c '^=== ' docs/lookout/frames.txt   # 17
```

Fix the four copies of the dead command found at Step 3.1 (`GALLERY_PREAMBLE`
in `frames.rs`, the doc comment on `write_the_gallery`, and the two generated
files, which the run above rewrites from the preamble):

```text
    cargo test -p shep --lib --all-features -- --ignored write_the_gallery
```

In the same preamble, `Phase 12b frames` becomes `Phase 16 frames` and
`the same fourteen frames with colour` becomes `the same seventeen frames with
colour`. Leave the rest of the preamble byte-identical.

**Two more copies of the count live outside the preamble** and this project's
seam rule is that a number stops counting the moment something is added beside
it. Both are in `frames.rs`:

- `Scene::caption`'s doc, "so the maintainer does not have to hold fourteen of them in her
  head" (around `frames.rs:199`).
- `every_scene_shows_the_thing_it_is_named_for`'s
  `#[allow(clippy::too_many_lines)] // fourteen captions, each pinned clause by
  clause` (around `frames.rs:714`).

Four numbers in total, then: two in the preamble and these two. By the end of
Task 9 all four read twenty-four, so each is edited three times across this
phase. Grep for the spelled-out word at the end of Tasks 3, 6 and 9 rather than
trusting memory:

```bash
grep -rn 'fourteen\|seventeen\|nineteen\|twenty-two\|twenty-four' \
  crates/shep-cli/src/lookout/frames.rs docs/lookout/frames.txt
```

### Step 3.7 - the two prose documents

- `docs/specs/deferred.md`: delete the **lookout's search/filter** entry
  outright. It is built.
- `docs/lookout/README.md`: delete the **Search/filter** bullet under "What is
  still open", and add the filter to the keymap, which is the
  "**A selected sheep, and the table marks it.**" bullet under "What 12b
  settled" (`README.md:69`). Include the sentence that `esc` clears a filter
  before it quits, and the one saying the title carries `2 of 6` for as long as
  one is on. Do NOT delete the "What is still open" heading yet: two bullets
  remain under it until Task 9.

Both edits land in this task's commit, not in Task 10's, so the document and
the feature it describes move together.

### Step 3.8 - verify: the full task gate

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Each from its own command with `$?` read directly. Clippy must be clean:
this is the point at which `set_filter`, `filter()`, `flock_len()` and
`InputMode` all have production callers, so a surviving `dead_code` warning
means something got written and never wired.

### Step 3.9 - MUTATION

In `title_line`, read the unfiltered map for the first number: replace
`app.rows().len()` with `app.flock_len()`.

**Must redden `the_title_counts_both_numbers_while_a_filter_is_on`** and the
`filter_editing` / `filter_active` / `filter_no_match` snapshots. Revert.

### Step 3.10 - second MUTATION

In `view/mod.rs`'s empty-table branch, reuse the empty-flock sentence for both
cases: delete the `if app.flock_len() == 0` split.

**Must redden `a_filter_matching_nothing_does_not_say_the_flock_is_empty`** and
the `filter_no_match` snapshot. Revert.

---

# Task 4 - `FlockSource::send`, `Sent`, `Channels`, and the reply

`source.rs`, `link.rs`, `app.rs`, and the channel's creation in `mod.rs`.

The link task already has the shape this needs. `run_ui` owns a
`polls: Sender<()>` that `Effect::PollNow` feeds; `run_connected` selects on it
and calls `reconcile`. The request path is the same shape, one channel over.

**Nothing on the wire changes.** `Request::Describe` shipped in Phase 3 and
`Request::Stop` / `Restart` / `Reload` in Phase 4. This task adds one trait
method that sends one of them and hands back whatever the shepherd said.

### Step 4.1 - baseline

```bash
grep -c 'fn send' crates/shep-cli/src/lookout/source.rs         # 0
grep -rn 'impl FlockSource for' crates/shep-cli/src/lookout/    # 2: ClientFlock, CountingFlock
grep -rn 'run_connected(' crates/shep-cli/src/lookout/ | wc -l  # 4: the definition, the call in run_link, two in tests
cargo test -p shep --lib --all-features -- lookout::link        # 5 passed
```

`crates/shep-cli/src/dog/bark/mod.rs` also has an `impl FlockSource`, and it is
**a different trait** with the same name, declared in `bark`'s own module for
the reasons `source.rs`'s doc gives. Do not touch it.

### Step 4.2 - RED

`app.rs`:

```rust
    /// fails if the three states `ProcessInfo::lambs` distinguishes get
    /// collapsed. The wire type keeps them apart on purpose: `None` means this
    /// reply did not walk, `Some(vec![])` means it walked and found nothing,
    /// and the pane says different sentences for each because they are
    /// different facts about the machine.
    #[test]
    fn a_lamb_reply_records_which_of_the_three_states_it_saw() {
        let (mut app, t0) = started();
        let walked = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .lambs(Some(vec![Lamb::new(48_220, "node")]))
            .build();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![walked])),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Walked(lambs), _)) if lambs.len() == 1));

        let empty = ProcessInfo::builder(1, "web", ProcStatus::Online)
            .lambs(Some(Vec::new()))
            .build();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![empty])),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Walked(lambs), _)) if lambs.is_empty()));

        let unwalked = ProcessInfo::builder(1, "web", ProcStatus::Stopped).build();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![unwalked])),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::NotWalked, _))));
        let _ = t0;
    }

    /// fails if a reading taken for one sheep is shown against another. The
    /// reading carries the id it was taken for and the pane asks by id, so a
    /// request dropped by a full channel and a reply for the previous
    /// selection both read as "not read yet" with no second field to track
    /// them.
    #[test]
    fn a_reading_for_another_sheep_reads_as_not_read_yet() {
        let (mut app, _t0) = started();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![
                ProcessInfo::builder(1, "web", ProcStatus::Online)
                    .lambs(Some(vec![Lamb::new(48_220, "node")]))
                    .build(),
            ])),
        });
        assert!(app.lambs_for(1).is_some());
        assert!(app.lambs_for(2).is_none(), "not this sheep's reading");
    }

    /// fails if a failed lamb fetch steals the status bar. It is a decoration
    /// on a pane, not an operator's action, and the pane already says what it
    /// does not know (A17).
    #[test]
    fn a_failed_lamb_fetch_says_so_in_the_pane_and_raises_no_notice() {
        let (mut app, _t0) = started();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Err(RequestError::Closed),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Failed, _))));
        assert!(app.notice().is_none(), "no notice for a decoration");
    }

    /// fails if an unrecognised reply is recorded as a successful walk.
    /// `Response` is `#[non_exhaustive]`; a variant this binary predates is
    /// not a lamb list and must not read as an empty one, which would say
    /// "none found" about a machine nobody looked at.
    #[test]
    fn an_unrecognised_lamb_reply_is_a_failure_and_not_an_empty_walk() {
        let (mut app, _t0) = started();
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Pong),
        });
        assert!(matches!(app.lambs_for(1), Some((LambWalk::Failed, _))));
    }

    /// fails if a reading landing after a freeze reaches the frame. Same guard
    /// and same reason as `Msg::Bleats`: the fetch is armed before the freeze
    /// can land, so a reply can still be in flight when `Msg::Frozen` arrives,
    /// and content newer than a banner saying the values are frozen is the
    /// contradiction-on-one-frame this dashboard refuses everywhere else.
    #[test]
    fn a_lamb_reply_after_a_freeze_is_refused() {
        let (mut app, _t0) = started();
        app.update(Msg::Frozen {
            at_local: "2026-08-16 09:00:00".to_string(),
        });
        app.update(Msg::Replied {
            sent: Sent::Lambs { id: 1 },
            result: Ok(Response::Described(vec![
                ProcessInfo::builder(1, "web", ProcStatus::Online)
                    .lambs(Some(vec![Lamb::new(48_220, "node")]))
                    .build(),
            ])),
        });
        assert!(app.lambs_for(1).is_none(), "the frozen frame learned nothing");
    }
```

`link.rs`:

```rust
    /// fails if a request never reaches the shepherd, or if its reply never
    /// comes back tagged with what it answered. The echo tag is what routes a
    /// reply with no correlation id, including an `Err` that carries no shape
    /// of its own.
    ///
    /// IR-46: the wait is bounded, so a loop that never sends hangs nothing.
    #[tokio::test(start_paused = true)]
    async fn a_request_reaches_the_shepherd_and_its_reply_comes_back() {
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let (request_tx, request_rx) = mpsc::channel(2);
        let (_events_tx, events_rx) = broadcast::channel(4);

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let flock = RecordingFlock {
            seen: Arc::clone(&seen),
        };
        let task = tokio::spawn(run_connected(
            flock,
            BroadcastEvents(events_rx),
            msg_tx,
            Channels {
                polls: poll_rx,
                requests: request_rx,
            },
            Duration::from_secs(3600),
        ));
        request_tx.send(Sent::Lambs { id: 7 }).await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut answered = None;
        while answered.is_none() {
            let Ok(Some(msg)) = tokio::time::timeout_at(deadline, msg_rx.recv()).await else {
                break;
            };
            if let Msg::Replied { sent, result } = msg {
                answered = Some((sent, result));
            }
        }
        task.abort();

        let (sent, result) = answered.expect("the reply came back");
        assert_eq!(sent, Sent::Lambs { id: 7 }, "tagged with what it answered");
        assert!(matches!(result, Ok(Response::Described(_))));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Request::Describe {
                selector: SelectorSpec::Id(7)
            }],
            "the id it was asked about, as a selector, and nothing else"
        );
    }
```

`RecordingFlock` is a hand-rolled `FlockSource` beside `CountingFlock` (IR-33,
no mock crate): `flock()` returns one sheep, `send()` records the `Request` and
answers `Response::Described` with one row.

**Two things break the moment `send` becomes a required trait method, and
neither is a design question. Both are compiler errors; naming them here saves
an implementer working out mid-task whether they are in scope.**

1. `CountingFlock`, the existing hand-rolled source in `link.rs`'s test module
   (`link.rs:303`), stops implementing `FlockSource`. Give it a `send` that
   answers `Ok(Response::Described(vec![]))` and ignores its argument: the
   tests that use it are about poll counting and know nothing about requests.
2. `run_ui` gains a parameter, so its call sites stop compiling. There are
   **six**: one production (`mod.rs:197`) and **five in `mod.rs`'s own test
   module** (around `:510`, `:551`, `:626`, `:693`, `:790`). Each test site
   gains a throwaway sender. Keep the receiver alive in the test that asserts
   on it (Task 5) and let it drop in the rest; a dropped receiver makes
   `try_send` return `Closed`, which is a case the reducer already answers with
   `Msg::Unsent`.

### Step 4.3 - GREEN: `source.rs`

```rust
    /// Sends one request over this connection and returns the shepherd's
    /// answer, whatever it is.
    ///
    /// Unlike [`Self::flock`], this does NOT swallow an unrecognised
    /// [`Response`] into an empty success. `Response` is `#[non_exhaustive]`,
    /// and a reply this binary does not understand is a fact the operator has
    /// to be told about: `flock()` can afford to shrug one off because the
    /// next poll asks again two seconds later, and an action or a lamb fetch
    /// has no next poll.
    ///
    /// # Errors
    ///
    /// Whatever the underlying connection failed the request with.
    fn send(&self, request: Request) -> impl Future<Output = Result<Response, RequestError>> + Send;
```

```rust
impl FlockSource for ClientFlock {
    // ... flock() unchanged ...

    async fn send(&self, request: Request) -> Result<Response, RequestError> {
        // The client's own default deadline, which is what every one of these
        // verbs already gets from the CLI: `commands::lifecycle` passes
        // `deadline: None` for stop, restart and reload, and `Reloading` is an
        // acceptance rather than a completed swap, so a longer budget would
        // buy nothing.
        self.0.request(request).await
    }
}
```

### Step 4.4 - GREEN: `app.rs`

```rust
/// One request the dashboard asked the link task to send, carried back on the
/// reply so it can be routed.
///
/// An echo tag rather than a correlation id: the answer to a request can be an
/// `Err` that carries no shape of its own, so the only thing that reliably
/// says which request a reply belongs to is the request itself, handed along
/// beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// The selected sheep's process tree.
    Lambs {
        /// Which sheep was asked about.
        id: u32,
    },
}

impl Sent {
    /// The wire request this asks for.
    #[must_use]
    pub fn request(&self) -> Request {
        match *self {
            Self::Lambs { id } => Request::Describe {
                selector: SelectorSpec::Id(id),
            },
        }
    }
}

/// What one lamb fetch came back with.
///
/// Three variants because `ProcessInfo::lambs` distinguishes three states and
/// the pane says three different sentences. The CLI has wording for only one
/// of them: `output::emit_described` skips the lamb caption for `None` and for
/// an empty vector alike, so there is nothing to borrow for the other two and
/// the pane says its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LambWalk {
    /// The shepherd walked the process table. Possibly to no descendants.
    Walked(Vec<Lamb>),
    /// The reply carried no walk at all, which for a `Describe` means this
    /// sheep has no pid to walk from.
    NotWalked,
    /// The request did not come back, or came back as something this binary
    /// does not understand.
    Failed,
}

/// One lamb reading, and which sheep it was taken for.
#[derive(Debug, Clone)]
pub struct LambReading {
    id: u32,
    at: Instant,
    walk: LambWalk,
}
```

Field on `App`:

```rust
    /// The last lamb reading, or `None` before there has been one.
    ///
    /// Keyed by the id it was taken for, so a reading for a sheep that is no
    /// longer selected, and a request a full channel dropped, both read as
    /// "not read yet" without a second field tracking them.
    lambs: Option<LambReading>,
```

`Msg` gains:

```rust
    /// A request this dashboard asked for came back. `sent` is the echo tag.
    Replied {
        /// What was asked.
        sent: Sent,
        /// What the shepherd said, or why it could not be asked.
        result: Result<Response, RequestError>,
    },
```

The arm, and the accessor the pane reads:

```rust
            Msg::Replied { sent, result } => match sent {
                Sent::Lambs { id } => self.on_lambs(id, result),
            },
```

```rust
    /// Records one lamb reading. Always [`Effect::None`]: a reducer that
    /// answered a reading with another request would spin the UI task.
    fn on_lambs(&mut self, id: u32, result: Result<Response, RequestError>) -> Effect {
        // The same guard `Msg::Bleats` carries, for the same reason: this
        // fetch is armed before a freeze can land, and a reading that reached
        // the frame afterwards would be newer than the banner saying the
        // values are frozen.
        if matches!(self.link, Link::Lost { .. }) {
            return Effect::None;
        }
        let walk = match result {
            Ok(Response::Described(rows)) => rows
                .into_iter()
                .find(|info| info.id == id)
                .map_or(LambWalk::Failed, |info| {
                    info.lambs.map_or(LambWalk::NotWalked, LambWalk::Walked)
                }),
            // An `Err`, or a reply this binary does not recognise. Neither is
            // an empty walk, and reporting either as one would say "none
            // found" about a process table nobody read.
            _ => LambWalk::Failed,
        };
        self.lambs = Some(LambReading {
            id,
            at: self.now,
            walk,
        });
        Effect::None
    }

    /// The lamb reading for sheep `id`, with its age in milliseconds as of
    /// this dashboard's own clock.
    ///
    /// `None` when there is no reading, or when the one there is was taken for
    /// a different sheep. The age comes from [`Self::now`], the same clock the
    /// uptime column reads, which means it stops when the dashboard freezes,
    /// for the same reason.
    #[must_use]
    pub fn lambs_for(&self, id: u32) -> Option<(&LambWalk, u64)> {
        let reading = self.lambs.as_ref().filter(|reading| reading.id == id)?;
        Some((
            &reading.walk,
            millis(self.now.saturating_duration_since(reading.at)),
        ))
    }
```

`Msg` keeps `#[derive(Debug, Clone)]`: `Response` is `Clone` and `RequestError`
is `Clone + PartialEq + Eq`, both checked.

### Step 4.5 - GREEN: `link.rs` and the channel

```rust
/// The receivers one connection borrows from the ladder and hands back when it
/// ends.
///
/// A two-field struct rather than a tuple: three signatures and an `# Errors`
/// section name it, and `Ok((polls, requests))` reads as nothing at any of
/// them.
#[derive(Debug)]
pub struct Channels {
    /// Out-of-band poll requests: the `r` key, and the reducer's own
    /// [`super::app::Effect::PollNow`].
    pub polls: mpsc::Receiver<()>,
    /// One-shot requests the dashboard wants sent on this connection.
    pub requests: mpsc::Receiver<Sent>,
}
```

`run_link` takes `Channels` in place of `polls`; `run_connected` takes and
returns `Channels`. The new arm:

```rust
            // `Some(sent) = ...`, not `_ = ...`: a receiver whose senders have
            // all been dropped is `Ready(None)` forever, and a `_` pattern
            // would spin this loop at full tilt. The pattern not matching
            // disables the branch instead.
            Some(sent) = channels.requests.recv() => {
                // Awaited inline, which holds the other arms for the
                // request's duration. That is already this loop's established
                // behaviour: the poll arm awaits `reconcile` the same way,
                // bounded by the client's own deadline.
                let result = flock.send(sent.request()).await;
                msgs.send(Msg::Replied { sent, result })
                    .await
                    .map_err(|_| UiGone)?;
            }
```

In `mod.rs`'s `lookout`:

```rust
    let (poll_tx, poll_rx) = mpsc::channel(8);
    // Capacity 2: one action plus one lamb fetch is the most that can be
    // outstanding, because the reducer refuses a second action while one is in
    // flight and the lamb fetch is coalesced onto the redraw gate.
    let (request_tx, request_rx) = mpsc::channel(2);
    let link = tokio::spawn(link::run_link(
        shepherd,
        opened,
        msg_tx,
        link::Channels {
            polls: poll_rx,
            requests: request_rx,
        },
        link::FLOCK_POLL,
    ));
```

and `run_ui` gains `requests: mpsc::Sender<Sent>` beside `polls`, at all six
call sites listed under Step 4.2. It has no send site until Task 5, so
**expect exactly one `unused_variables` warning between here and there.** That is why the gate does not run after this task. Do
not add an `allow` and do not add a placeholder send.

### Step 4.6 - verify

```bash
cargo fmt --all --check
cargo test -p shep --lib --bins --all-features
```

Roughly `585 passed; 0 failed; 3 ignored`.

### Step 4.7 - MUTATION

In `on_lambs`, swallow the unrecognised case into a walk: change the `_ =>`
arm to `LambWalk::Walked(Vec::new())`.

**Must redden `an_unrecognised_lamb_reply_is_a_failure_and_not_an_empty_walk`**
and `a_failed_lamb_fetch_says_so_in_the_pane_and_raises_no_notice`. This is
exactly what `flock()` does and exactly what `send` must not. Revert.

### Step 4.8 - second MUTATION

In `run_connected`'s new arm, answer the wrong request: send
`Msg::Replied { sent: Sent::Lambs { id: 0 }, result }` instead of the `sent`
that arrived.

**Must redden `a_request_reaches_the_shepherd_and_its_reply_comes_back`** on
the `assert_eq!(sent, Sent::Lambs { id: 7 })`. Revert.

---

# Task 5 - `Effect::RefreshSelected` and the coalesced lamb fetch

`app.rs` and `mod.rs`.

Today `select_at` and the `Snapshot` arm both return `Effect::RefreshFeed`, and
they now have to be told apart: a snapshot must refresh the feed (the selected
row's log paths can change) and must **not** fetch lambs, because that is the
two-second timer the daemon's own docs argue against.

### Step 5.1 - baseline

```bash
grep -rn 'RefreshSelected' crates/ | wc -l                              # 0
grep -c 'feed_dirty' crates/shep-cli/src/lookout/mod.rs                 # 6
grep -n 'Effect::RefreshFeed' crates/shep-cli/src/lookout/app.rs        # see below
cargo test -p shep --lib --all-features -- lookout::                    # 94 at HEAD, plus Tasks 1-4
```

### Step 5.2 - RED

Two of those need a word, because the first draft of this plan got both wrong
and a baseline that does not print what it claims is worthless downstream.

- **`Effect::RefreshFeed` is a `grep -n`, not a `grep -c`.** The count at HEAD
  is 12, not 4: it appears in four doc comments as well as in the code, which
  is the "a whole-file grep whose word also appears in a doc comment" shape
  from the baselines section above. What this task needs is the list of call
  sites, which is the four `Effect::RefreshFeed` expressions at `app.rs:309`
  (the `Snapshot` arm), `:389` and `:404` (the two `on_event` arms) and `:541`
  (`select_at`), plus the four test assertions at `:807`, `:811`, `:820` and
  `:842`. The last of those four is the one Step 5.3a says must NOT change,
  in `a_snapshot_refreshes_the_feed_unless_the_link_is_frozen`; the other
  three are in the shipped test Step 5.3a turns into `Effect::RefreshSelected`.
  Count the call, not the word.
- **The `lookout::` total is a moving target by construction** and this task
  runs in the middle of the phase. It prints `94 passed; 0 failed; 1 ignored`
  at HEAD, measured; Tasks 1 to 4 add roughly thirty more, so expect about 125
  here. Use it as a shape and check `failed` is 0. The number that matters in
  this task is the `RefreshSelected` grep, which is 0 before and non-zero
  after.

`app.rs`:

```rust
    /// fails if a moved selection stops asking for lambs. The pane describes
    /// the selected sheep, so the trigger is the selection changing and
    /// nothing else.
    #[test]
    fn moving_the_selection_asks_for_lambs() {
        let (mut app, _t0) = started();
        assert_eq!(
            app.update(Msg::Key(KeyPress::SelectDown)),
            Effect::RefreshSelected
        );
    }

    /// fails if the two-second listing starts asking for lambs, which is the
    /// whole cost decision inverted. `with_lambs`'s own doc says `ListFlock`
    /// declines the walk because a flock listing is what an operator leaves
    /// running in a loop; a dashboard putting a full machine enumeration on a
    /// fixed 2s clock, times however many lookout windows are open, is the
    /// daemon paying exactly the cost its own code was written to avoid.
    #[test]
    fn a_snapshot_refreshes_the_feed_and_does_not_ask_for_lambs() {
        let (mut app, t0) = started();
        assert_eq!(
            app.update(Msg::Snapshot {
                rows: vec![sheep(1, "web", ProcStatus::Online)],
                at: t0,
            }),
            Effect::RefreshFeed
        );
    }

    /// fails if a frozen dashboard asks the shepherd for anything. Inherited
    /// from `select_at`'s shipped rule rather than restated: the link is gone,
    /// so there is nothing to ask.
    #[test]
    fn nothing_is_requested_while_the_link_is_lost() {
        let (mut app, _t0) = started();
        app.update(Msg::Frozen {
            at_local: "2026-08-16 09:00:00".to_string(),
        });
        assert_eq!(app.update(Msg::Key(KeyPress::SelectDown)), Effect::None);
    }
```

`mod.rs`, modelled on `a_burst_of_selection_moves_costs_one_read_and_not_one_per_key`,
including its real-clock caveat and its nudge-then-quit shape, which its own
doc comment explains at length. Do not convert either to `start_paused`.

```rust
    /// fails if a held `j` fires one `Describe` per keypress. Ordinary
    /// terminals deliver auto-repeat as twenty to thirty Press events a
    /// second, and each one moves the selection; without the redraw gate this
    /// feature would be exactly the fixed-clock process-table walk it exists
    /// to avoid, only faster. One request per redraw window, not one per key.
    ///
    /// IR-46: bounded by a real, short sleep and a real, generous timeout.
    #[tokio::test]
    async fn a_burst_of_selection_moves_costs_one_lamb_request() {
        // ... same setup as the feed's burst test, plus:
        let (request_tx, mut request_rx) = mpsc::channel(2);
        // ... twenty SelectDown, the nudge, the quit, run_ui ...
        let mut asked = 0;
        while let Ok(sent) = request_rx.try_recv() {
            assert!(matches!(sent, Sent::Lambs { .. }));
            asked += 1;
        }
        assert_eq!(asked, 1, "twenty moves, one Describe");
    }

    /// fails if a terminal too short to draw the detail pane still pays for
    /// it. `run_ui` knows the height; the reducer does not, and does not need
    /// to.
    ///
    /// IR-46: bounded the same way.
    #[tokio::test]
    async fn no_lambs_are_requested_when_the_detail_pane_is_not_drawn() {
        // Same shape, at TestBackend::new(120, 20) - the 18-row tier, where
        // `panes_for(20).detail` is false and the host strip and feed are
        // still up. Assert the request channel is empty.
    }
```

`(120, 20)` is the size the shipped `no_detail` scene already uses for exactly
this tier, so the threshold under test is the one the gallery shows.

### Step 5.3 - GREEN: `app.rs`

```rust
    /// Re-read the selected sheep's log files AND ask the shepherd for its
    /// lambs.
    ///
    /// Held apart from [`Self::RefreshFeed`] because the two triggers differ:
    /// a snapshot must refresh the feed, since the selected row's log paths
    /// can have changed, and must not fetch lambs, since it fires every two
    /// seconds. Returned by `select_at` when the selection actually moved, and
    /// by the callers of `reseat` on the same condition they already use to
    /// choose between `RefreshFeed` and `None`.
    RefreshSelected,
```

Then: `select_at` returns `RefreshSelected` instead of `RefreshFeed`;
`set_filter` (Task 1) does the same; the `ProcessEventKind::Delete` arm and the
`was_empty` upsert arm in `on_event` return `RefreshSelected`; **the `Snapshot`
arm keeps `RefreshFeed`, unconditionally, exactly as it is.**

### Step 5.3a - the four assertions this reddens, and why they are updated

Changing what `select_at` returns changes what four existing tests observe.
Three of them shipped; one this plan wrote in Task 2. **Updating a shipped
test's expectation is exactly the thing a review should stop, so it is written
down here as an intended consequence rather than discovered in a diff, and the
commit message says so.**

- `a_selection_that_moves_refreshes_the_feed_and_one_that_cannot_does_not`
  (`app.rs`, shipped): its three `assert_eq!(..., Effect::RefreshFeed)` on
  `SelectDown`, `SelectFirst` and `SelectLast` become `Effect::RefreshSelected`.
  Its two `Effect::None` assertions do not move, and they are the half of the
  test that carries its real claim: a selection that could not move asks for
  nothing. Its doc comment gains one sentence saying the effect now covers the
  lambs as well as the feed.
- `esc_clears_the_filter_instead_of_quitting_while_one_is_set` (Task 2): its
  one `Effect::RefreshFeed` becomes `Effect::RefreshSelected`, because clearing
  a filter moves the cursor and therefore changes which sheep every pane below
  the table is describing.

`a_snapshot_refreshes_the_feed_unless_the_link_is_frozen` (`app.rs`, shipped)
must NOT change. It asserts `Effect::RefreshFeed` from the `Snapshot` arm, and
that is the whole cost decision this task exists to preserve. A diff that
touched it has inverted the feature.

### Step 5.4 - GREEN: `mod.rs`

Beside `feed_dirty`, mirroring it exactly:

```rust
    // Set by `Effect::RefreshSelected` and by `Effect::PollNow`, cleared once
    // the coalesced request below has gone out. A flag rather than an `mpsc`
    // arm, for the reason this function's doc gives about the `biased`
    // select's arm retirement.
    let mut lambs_dirty = false;
```

In the loop, beside the feed's coalesced read and behind the same `may_draw`
gate:

```rust
        if lambs_dirty && may_draw {
            // The height is read here and not in the reducer: `run_ui` knows
            // the terminal, `App` deliberately does not, and a terminal too
            // short to draw the detail pane must not pay for a process-table
            // walk it cannot show. A size that cannot be read is treated as
            // too short, which errs toward asking for nothing.
            let height = terminal.size().map_or(0, |size| size.height);
            if view::panes_for(height).detail {
                if let Some(id) = app.selected() {
                    // `try_send`, for `Effect::PollNow`'s reason: a full
                    // channel means a request is already queued, and a
                    // dropped lamb fetch reads as "not read yet", which the
                    // pane already knows how to say.
                    let _ = requests.try_send(Sent::Lambs { id });
                }
            }
            lambs_dirty = false;
        }
```

and in the effect dispatch:

```rust
            Effect::PollNow => {
                let _ = polls.try_send(());
                // `r` means "tell me again", so it refreshes everything the
                // pane shows and not only the table.
                lambs_dirty = true;
                dirty = true;
            }
            Effect::RefreshFeed => {
                feed_dirty = true;
                dirty = true;
            }
            Effect::RefreshSelected => {
                feed_dirty = true;
                lambs_dirty = true;
                dirty = true;
            }
```

`lambs_dirty = false` sits **outside** the `panes_for` test, so a short
terminal clears the flag rather than accumulating one request per keypress to
fire the moment somebody makes the window taller.

### Step 5.5 - verify

```bash
cargo fmt --all --check
cargo test -p shep --lib --bins --all-features
```

Roughly `590 passed; 0 failed; 3 ignored`. The `unused_variables` warning from
Task 4 is gone.

### Step 5.6 - MUTATION

Return `Effect::RefreshSelected` from the `Msg::Snapshot` arm.

**Must redden `a_snapshot_refreshes_the_feed_and_does_not_ask_for_lambs`.**
This is the whole cost decision inverted, and it is the one mutation in this
phase whose damage is invisible on screen: the dashboard would look identical
and the shepherd would walk the process table every two seconds per open
window. Revert.

### Step 5.7 - second MUTATION

Send the request from the `Effect::RefreshSelected` arm instead of from behind
`may_draw`.

**Must redden `a_burst_of_selection_moves_costs_one_lamb_request`**, which then
counts twenty. Revert.

### Step 5.8 - third MUTATION

Drop the `view::panes_for(height).detail` test.

**Must redden `no_lambs_are_requested_when_the_detail_pane_is_not_drawn`.**
Revert.

---

# Task 6 - the lamb line, `DETAIL_ROWS` 4 to 5, two frames

`view/detail.rs`, `view/mod.rs`, `frames.rs`, and the two prose documents.
**Full task gate at the end.**

### Step 6.1 - baseline

```bash
grep -c 'DETAIL_ROWS: u16 = 4' crates/shep-cli/src/lookout/view/mod.rs      # 1
grep -rn 'the_detail_pane_never_mentions_lambs' crates/ | wc -l             # 1
grep -c '^=== ' docs/lookout/frames.txt                                     # 17
cargo test -p shep --lib --all-features -- lookout::view::                  # note the number
```

### Step 6.2 - the test that must be deleted, not adjusted

`the_detail_pane_never_mentions_lambs` in `view/detail.rs` fails if the words
`lamb`, `LAMB`, `children` or `tree` appear in the rendered pane. It was
correct when it was written; this feature is the thing it was written to
prevent. **Delete it, and say so in the commit message.** Deleting a test is
the kind of thing a review should stop, so it is written down here as an
intended consequence (A19) rather than discovered in a diff.

Its replacement is strictly stronger: the old test proved the pane said
nothing, the new one proves it says the right one of five things.

`view/fixtures.rs`'s `sheep_with_lambs` stays. Its doc says it is deliberately
impossible for a `ListFlock` reply, which is still true, and it is now the
fixture for the pane's walked state.

### Step 6.3 - RED

`view/detail.rs`:

```rust
    /// fails if the pane collapses any two of the five states it can be in.
    /// Three of them are distinctions `ProcessInfo::lambs` was built to keep
    /// (walked and non-empty, walked and empty, not walked at all) and the CLI
    /// has wording for only the first, so the other four sentences are this
    /// pane's own.
    #[test]
    fn the_pane_says_which_lamb_state_it_is_in() {
        let cases: [(LambWalk, &str); 3] = [
            (
                LambWalk::Walked(vec![Lamb::new(48_220, "node"), Lamb::new(48_221, "node")]),
                "lambs  2 parent-pid descendants, read ",
            ),
            (LambWalk::Walked(Vec::new()), "lambs  none found, read "),
            (
                LambWalk::NotWalked,
                "lambs  this sheep is not running, so there is no tree to walk",
            ),
        ];
        for (walk, expected) in cases {
            let app = with_lamb_reading(walk);
            let rendered = render_all(&detail_lines(&app, 200));
            assert!(rendered.contains(expected), "expected {expected:?} in {rendered:?}");
        }

        let failed = with_lamb_reading(LambWalk::Failed);
        assert!(
            render_all(&detail_lines(&failed, 200))
                .contains("lambs  the shepherd did not answer that request")
        );

        let unread = with_selection(sheep_with_lambs());
        assert!(render_all(&detail_lines(&unread, 200)).contains("lambs  not read yet"));
    }

    /// fails if a single lamb reads as "1 parent-pid descendants".
    #[test]
    fn one_lamb_is_a_descendant_and_not_descendants() {
        let app = with_lamb_reading(LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        let rendered = render_all(&detail_lines(&app, 200));
        assert!(rendered.contains("1 parent-pid descendant, read "), "got {rendered:?}");
    }

    /// fails if the staleness stamp moves after the list, or goes away.
    /// `detail.rs`'s standing rule is that the rarest field goes last so a
    /// narrow terminal truncates it first; here that rule inverts, because a
    /// truncated list is still honest and a list whose "read 4m ago" was
    /// truncated away is a stale reading presented as current.
    #[test]
    fn the_lamb_line_carries_its_age_before_its_list() {
        let app = with_lamb_reading(LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        let line = rendered(&detail_lines(&app, 200)[1]);
        let stamp = line.find("read ").expect("a stamp");
        let list = line.find("48220").expect("a list");
        assert!(stamp < list, "the caveat must survive truncation: {line:?}");
    }

    /// fails if the pane starts showing a reading taken for another sheep.
    #[test]
    fn a_reading_for_another_sheep_is_not_drawn_here() {
        // with_lamb_reading pins its reading to the selected sheep's id;
        // this one pins it to a different one and expects the unread sentence.
        let app = with_lamb_reading_for(11, LambWalk::Walked(vec![Lamb::new(48_220, "node")]));
        assert!(render_all(&detail_lines(&app, 200)).contains("lambs  not read yet"));
    }

    /// fails if the stamp reads a live clock instead of `App::now`, and fails
    /// again if a frozen dashboard's stamp creeps. Two halves, and BOTH are
    /// needed: the first proves the stamp moves at all, the second proves it
    /// stops when the banner says the values did.
    ///
    /// This is a unit test rather than a two-age frame comparison, and that is
    /// the point. Rendering the frozen scene at two ages cannot fail for this
    /// mutation: both renders happen at the same wall-clock instant, so a live
    /// clock produces the same string in both and the frames stay identical.
    /// Here the two ages differ by construction, because they are `Msg::Tick`
    /// arithmetic rather than elapsed time. No sleep (IR-33).
    #[test]
    fn the_stamp_ages_on_a_live_dashboard_and_stops_on_a_frozen_one() {
        let (mut app, t0) = app_with_lamb_reading_at(LambWalk::Walked(vec![Lamb::new(
            48_220, "node",
        )]));
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(120),
        });
        let live = lamb_line_of(&app);
        assert!(live.contains("read 2m ago"), "the stamp aged: {live:?}");

        app.update(Msg::Frozen {
            at_local: "2026-08-16 09:00:00".to_string(),
        });
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(3_600),
        });
        assert_eq!(
            lamb_line_of(&app),
            live,
            "a frozen dashboard's reading must not age"
        );
    }
```

Two helpers in `view/fixtures.rs`, and the two `with_lamb_reading*` fixtures
the tests above use. Written out, because the `LambWalk`-to-`Msg::Replied`
mapping is not mechanical: the reducer never takes a `LambWalk` directly, it
derives one from a reply, so each variant needs the reply that produces it.

```rust
/// [`with_selection`] over [`sheep_with_lambs`] (id 9), with one lamb reading
/// applied for that sheep.
pub fn with_lamb_reading(walk: LambWalk) -> App {
    with_lamb_reading_for(9, walk)
}

/// The same, with the reading pinned to `id` instead, so a test can hand the
/// pane a reading that belongs to a different sheep.
pub fn with_lamb_reading_for(id: u32, walk: LambWalk) -> App {
    let mut app = with_selection(sheep_with_lambs());
    app.update(Msg::Replied {
        sent: Sent::Lambs { id },
        result: reply_for(id, &walk),
    });
    app
}

/// [`with_lamb_reading`] plus the `Instant` the dashboard started at, for the
/// one test that needs to tick the clock forward itself.
pub fn app_with_lamb_reading_at(walk: LambWalk) -> (App, Instant) {
    let t0 = Instant::now();
    let mut app = with_selection(sheep_with_lambs());
    app.update(Msg::Tick { now: t0 });
    app.update(Msg::Replied {
        sent: Sent::Lambs { id: 9 },
        result: reply_for(9, &walk),
    });
    (app, t0)
}

/// The reply that makes the reducer record `walk`. There is no way to set a
/// `LambWalk` directly and there should not be: a fixture that reached past
/// `on_lambs` would stop testing the mapping this pane depends on.
///
/// `Failed` is produced by an `Err` rather than by an unrecognised `Ok`,
/// because the two are the same state and `Err` is the one an operator
/// actually meets.
fn reply_for(id: u32, walk: &LambWalk) -> Result<Response, RequestError> {
    let lambs = match walk {
        LambWalk::Failed => return Err(RequestError::Closed),
        LambWalk::NotWalked => None,
        LambWalk::Walked(lambs) => Some(lambs.clone()),
    };
    Ok(Response::Described(vec![
        ProcessInfo::builder(id, "gateway", ProcStatus::Online)
            .pid(Some(48_301))
            .lambs(lambs)
            .build(),
    ]))
}

/// The pane's lamb line alone, for the tests that compare two renderings of
/// it. Panics if the pane has none, so a regression that dropped the line
/// entirely cannot pass by comparing two absences.
pub fn lamb_line_of(app: &App) -> String {
    render_all(&super::detail::detail_lines(app, 200))
        .lines()
        .find(|line| line.starts_with("lambs  "))
        .map(str::to_string)
        .expect("the pane has a lamb line")
}
```

`view/mod.rs`:

```rust
    /// fails if the pane grew a line and the tier table did not grow with it.
    /// `every_pane_tier_fits_the_height_it_claims` picks `DETAIL_ROWS` up
    /// automatically and should stay green; if it does not, the tier table is
    /// wrong, not the test. At 24 rows the fixed cost becomes chrome 4, banner
    /// 1, host 1, detail 5, feed 7, which is 18, leaving 6 for the table
    /// against the tier test's floor of 3.
    #[test]
    fn the_detail_pane_claims_the_rows_it_draws() {
        let app = with_selection(sheep_with_lambs());
        assert_eq!(
            detail::detail_lines(&app, 120).len(),
            usize::from(DETAIL_ROWS - 1),
            "one rule plus its content lines"
        );
    }
```

### Step 6.4 - GREEN

`view/mod.rs`: `const DETAIL_ROWS: u16 = 5;` with its doc becoming "one rule
and four lines". Nothing else in the tier table moves.

`view/detail.rs`: **three claims in this file count or describe the pane and
all three become false.** They are one edit each and the third is the one a
sweep forgets:

1. The module doc's opening claim, "no second request, no `Request::Describe`,
   and therefore no lamb list", is replaced by what is true: the pane's other
   lines come from the `ProcessInfo` the flock table's own rows carry, and the
   lamb line alone comes from a `Describe` fetched on selection change and on
   `r`, never on the poll.
2. The module doc's first line, "The sheep detail pane: three lines about the
   selected sheep", becomes four.
3. `detail_lines`'s own doc, "The pane's three content lines. Its rule is
   [`super::draw`]'s", becomes four.

The no-selection branch returns four lines (its `why` sentence plus three
blanks). The selected branch inserts the lamb line second, between the sheep
line and `out`:

```rust
/// The lamb line: what the last walk found, and how old it is.
///
/// The age comes first. This file's own rule is that the rarest field goes
/// last so a narrow terminal truncates it first, and here that rule inverts:
/// a truncated list is still honest, while a list whose stamp was truncated
/// away is a stale reading presented as current.
///
/// It does not repeat the CLI's "not exactly the set a stop kills" clause.
/// "parent-pid descendants" is already precisely true, and forty characters of
/// warning on every frame trains an operator to stop reading the pane (A16).
fn lamb_line(app: &App, id: u32, width: u16, palette: Palette) -> Line<'static> {
    let text = match app.lambs_for(id) {
        None => "lambs  not read yet".to_string(),
        Some((LambWalk::Failed, _)) => "lambs  the shepherd did not answer that request".to_string(),
        Some((LambWalk::NotWalked, _)) => {
            "lambs  this sheep is not running, so there is no tree to walk".to_string()
        }
        Some((LambWalk::Walked(lambs), age)) if lambs.is_empty() => {
            format!("lambs  none found, read {} ago", human_duration(age))
        }
        Some((LambWalk::Walked(lambs), age)) => {
            let noun = if lambs.len() == 1 {
                "descendant"
            } else {
                "descendants"
            };
            let list = lambs
                .iter()
                .map(|lamb| format!("{} {}", lamb.pid, lamb.name))
                .collect::<Vec<_>>()
                .join("   ");
            format!(
                "lambs  {} parent-pid {noun}, read {} ago   {list}",
                lambs.len(),
                human_duration(age)
            )
        }
    };
    Line::from(Span::styled(fit(&text, width), palette.muted()))
}
```

`human_duration` is already imported here for the uptime cell, and it takes
milliseconds, which is what `lambs_for` returns.

### Step 6.5 - GREEN: two frames

Two scenes, both at `(120, 30)` so they sit beside `healthy_wide` and show all
three panes. Both are `Control::ReadOnly` here; Task 9 flips `Lambs` to
`Control::Allowed` so that the gallery has one frame showing the
control-enabled key hint, which means its snapshot changes a second time in
that task. That is expected, and it is the only frame in the gallery that
changes twice this phase.

```rust
    /// The detail pane with a lamb list.
    Lambs,
    /// The detail pane on a sheep with no pid, where the shepherd had no tree
    /// to walk.
    LambsUnknown,
```

`Lambs` takes the default six-sheep flock with the selection already on `api`
(id 2, the third row, which the shipped selection block puts it on) and one
`Msg::Replied` carrying a walk of three.

`LambsUnknown` needs two edits that are easy to miss and that the compiler will
only catch one of:

1. **The flock match arm.** The `Errored` flock is the only one whose `cron` at
   id 4 is `ProcStatus::Stopped` with no pid, which is the state this scene
   exists to show. That flock lives in the `Scene::Errored | Scene::Frozen`
   arm, so the arm becomes
   `Scene::Errored | Scene::Frozen | Scene::LambsUnknown`. Miss this and the
   scene silently draws the default flock, whose `cron` is `Online` with a pid,
   and the frame shows the wrong sentence rather than failing to build.
2. **Two more `SelectDown`s.** The shipped selection block applies exactly two
   to every scene outside its four-scene exclusion list, which lands the cursor
   on `api` at id 2. `cron` is id 4, two rows further down, so the scene block
   applies two more of its own. This one is not a compiler error either: the
   frame would render `sheep 2  api` under a caption about a stopped sheep.

In `scene_with`, after the feed and before the `Retrying` / `Frozen` block, so
that the frozen scene's reading is applied while the link is still live:

```rust
        Scene::Lambs => {
            app.update(Msg::Replied {
                sent: Sent::Lambs { id: 2 },
                result: Ok(Response::Described(vec![
                    ProcessInfo::builder(2, "api", ProcStatus::Online)
                        .pid(Some(48_219))
                        .lambs(Some(vec![
                            Lamb::new(48_220, "node"),
                            Lamb::new(48_221, "node"),
                            Lamb::new(48_222, "node"),
                        ]))
                        .build(),
                ])),
            });
        }
```

**`Scene::Frozen` gets a reading too**, applied the same way and before
`Msg::Frozen`, so the gallery carries one frame of a lamb line on a frozen
dashboard. It is applied while the link is still `Live` because `on_lambs`
refuses once it is `Lost`, which is the same guard `Msg::Bleats` carries.

**It is not there to support a test.** The first draft of this plan pinned the
freeze property with a two-age frame comparison, modelled on the uptime
column's. That check cannot fail: both renders happen at the same wall-clock
instant, so an implementation reading a live clock produces the same string in
both and the frames stay identical. Worse, `App::now` stops at `t0 + 7s` for
the frozen scene and the reading is taken at the same instant, so the stamp is
`read 0s ago` in both renders whatever the implementation does. The property
lives in `the_stamp_ages_on_a_live_dashboard_and_stops_on_a_frozen_one` in
`detail.rs` (Step 6.3), where the two ages differ by construction. The frozen
frame is a picture, and pictures are what the maintainer reads; this one is not also a
test.

Captions, each clause pinned:

```rust
            Self::Lambs => {
                "The detail pane with a lamb list: how many descendants the shepherd's walk found, how old that reading is, and each lamb's pid and executable name. The stamp sits before the list so a narrow terminal truncates lambs rather than the caveat."
            }
            Self::LambsUnknown => {
                "The same pane on a stopped sheep. The shepherd had no pid to walk from and left the field unset rather than empty, and the line says which of the two it is looking at rather than reporting none found."
            }
```

Assertions:

```rust
        let lambs = render_text(&scene(Scene::Lambs).1);
        assert!(lambs.contains("lambs  3 parent-pid descendants, read "));
        assert!(lambs.contains("48220 node"), "each lamb's pid and name");
        let line = lambs
            .lines()
            .find(|line| line.starts_with("lambs  "))
            .expect("the lamb line");
        assert!(
            line.find("read ").unwrap() < line.find("48220").unwrap(),
            "the stamp comes before the list"
        );

        let unknown = render_text(&scene(Scene::LambsUnknown).1);
        assert!(unknown.contains("lambs  this sheep is not running, so there is no tree to walk"));
        assert!(!unknown.contains("none found"), "which is the other sentence");
        assert!(unknown.contains("sheep 4  cron"), "on the stopped sheep");
```

and one line added to the shipped
`the_frozen_frame_does_not_move_however_long_the_link_stays_gone`, which
already renders `Scene::Frozen` at two ages and compares whole frames. Now that
the frozen scene has a lamb line, that comparison covers it for free, and one
assertion makes the coverage deliberate rather than incidental:

```rust
        assert!(
            ten_minutes.lines().any(|line| line.starts_with("lambs  ")),
            "the frozen frame has a lamb line for the comparison above to cover"
        );
```

Without it the whole-frame compare would keep passing on a frame that lost its
lamb line entirely, which is the "passes for the wrong reason" shape that
test's own doc already warns about for the uptime column. It does not make the
compare able to catch a live clock; nothing rendered at one instant can. That
is `the_stamp_ages_on_a_live_dashboard_and_stops_on_a_frozen_one`'s job.

`assert_eq!(Scene::ALL.len(), 17)` becomes `19`.

### Step 6.6 - snapshots, gallery, docs

Every frame that draws the detail pane gains a row and loses a table row, so
**ten of the seventeen `.snap` files change**, plus the two new ones. The ten
are exactly the frames tall enough for `panes_for(height).detail` to be true:
`cramped`, `empty`, `errored`, `feed_gap`, `feed_missing`, `frozen`,
`healthy_wide`, `host_unknown`, `refused` and `retrying`. The seven that do not
are `narrow`, `no_detail`, `table_only` and `too_narrow`, which are too short
for a detail pane, and Task 3's three filter frames, which are 100x14 and
therefore also too short. Re-accept, then read the diff: the only changes must
be one added `lambs  ` line per detail-drawing frame and one fewer table row.

```bash
cargo test -p shep --lib --all-features -- lookout::frames
cargo insta review
cargo test -p shep --lib --all-features -- --ignored write_the_gallery; echo "EXIT=$?"   # 0
grep -c '^=== ' docs/lookout/frames.txt    # 19
```

Update the frame count to nineteen in all four places Step 3.6 lists, and run
that step's grep to check none was missed. It becomes twenty-four in Task 9.
Then:

- `docs/specs/deferred.md`: delete the **lambs in the detail pane** entry.
- `docs/lookout/README.md`: delete the **Lambs in the detail pane** bullet
  under "What is still open", and rewrite the bullet that describes the pane
  under "What 12b settled" (`README.md:84`), which today opens
  "**The detail pane reads what the table already has.** No second request..."
  and is now flatly false. Say that every line but one still reads what the
  table already has, that the lamb line is fetched with `Describe` on selection
  change and on `r` and never on the poll, and that it carries its age because
  of that. Leave the "What is still open" heading in place: one bullet, the
  actions, is still under it until Task 9.

### Step 6.7 - verify: the full task gate

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

`cargo doc` matters here: `view/detail.rs`'s module doc has just been rewritten
and it carries intra-doc links.

### Step 6.8 - MUTATION

Collapse `NotWalked` and the empty `Walked` into one sentence.

**Must redden `the_pane_says_which_lamb_state_it_is_in`** and the
`lambs_unknown` snapshot. This is the exact distinction the wire type was built
to keep. Revert.

### Step 6.9 - second MUTATION

Move the stamp after the list in the walked arm.

**Must redden `the_lamb_line_carries_its_age_before_its_list`** and the `lambs`
snapshot and its caption assertion. Revert.

### Step 6.10 - third MUTATION

Set `DETAIL_ROWS` back to 4 while the pane still draws four lines.

**Must redden `the_detail_pane_claims_the_rows_it_draws`** and every frame
snapshot with a detail pane, because the fourth line is drawn over the feed's
rule. Revert.

---

# Task 7 - the action keys and the confirm state machine

`input.rs` and `app.rs`, plus one arm in `run_ui`. This is the outbound half:
a key arms, Enter confirms, and the reducer hands `run_ui` a request to send.
Task 8 is what happens when the shepherd answers.

**This is the task where being wrong costs an operator a running process.**
Everything here is about a keystroke in a dashboard someone is reading not
becoming an action they did not intend.

### Step 7.1 - baseline

```bash
grep -c "KeyPress::Stop" crates/shep-cli/src/lookout/app.rs      # 4
grep -rn "KeyPress::Stop" crates/ | wc -l                        # 7
grep -rn 'stop is not built yet' crates/ | wc -l                 # 2
grep -c "KeyCode::Char('R')" crates/shep-cli/src/lookout/input.rs # 0, the key is free
grep -c "KeyCode::Char('L')" crates/shep-cli/src/lookout/input.rs # 0, the key is free
```

`KeyPress::Stop` becomes `KeyPress::Action(ActionVerb::Stop)`. **Seven sites,
measured, not five**, and one of them is a test that has to be rewritten rather
than renamed:

| site | what happens to it |
|---|---|
| `app.rs:463` | the `KeyPress::Stop` match arm, replaced by `arm(verb)` |
| `app.rs:994` | `the_stop_key_refuses_in_both_control_states`, read-only half |
| `app.rs:1010` | the same test, control-enabled half. **Rewritten, see below** |
| `app.rs:1131` | a press inside another test, renamed |
| `input.rs:46` | `KeyCode::Char('x') => Some(KeyPress::Stop)`, renamed |
| `input.rs:85` | `every_bound_key_resolves_to_its_press`, renamed |
| `frames.rs:518` | the `Refused` scene's key press, renamed |

The variant declaration itself is an eighth site the grep does not match,
because it is spelled `Stop,` inside `enum KeyPress`.

**`the_stop_key_refuses_in_both_control_states` becomes a read-only-only
refusal test.** Its whole subject is that `x` refuses in BOTH gate states, and
half of that stops being true in this task: behind an open gate `x` now arms.
Renaming the variant inside it would leave a test asserting a refusal that no
longer happens, so the control-enabled half moves out and becomes
`an_action_key_arms_a_confirm_and_sends_nothing` below. Rename the test to
`the_action_keys_refuse_while_the_gate_is_closed` or fold it into
`every_action_key_refuses_while_the_gate_is_closed`, which is strictly
stronger because it covers all three verbs. **Say in the commit message that a
shipped test lost half its subject and why**, the same way Task 6 says it about
the deleted one.

### Step 7.2 - RED

The four that matter most are the four about the confirm. `allowed()` is a new
fixture beside `started()`: the same three sheep at `Control::Allowed`, with
the selection moved off both the first and the last row, so a selection
assertion can fail in either direction.

```rust
    /// `started()`'s three sheep with the gate open and the cursor parked in
    /// the middle, on `api` at id 2.
    ///
    /// Mid-list on purpose. Half the tests below assert that a stray `j` did
    /// NOT move the cursor, and a cursor already clamped at either end would
    /// pass those tests whether the routing rule consumed the key or not.
    fn allowed() -> App {
        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(1, "web", ProcStatus::Online),
                sheep(2, "api", ProcStatus::Online),
                sheep(3, "worker", ProcStatus::Online),
            ],
            at: t0,
        });
        app.update(Msg::Tick { now: t0 });
        app.update(Msg::Key(KeyPress::SelectDown));
        app
    }
```

The `Msg::Tick` is not decoration: it sets `App::now`, which is the clock
`arm` stamps onto the armed action and which
`a_confirm_expires_after_ten_seconds_of_ticks` then advances.

```rust
    /// fails if an action key acts. It arms, and nothing has been sent: the
    /// whole point of the gate is that one keystroke in a dashboard somebody
    /// is reading does not become an action.
    #[test]
    fn an_action_key_arms_a_confirm_and_sends_nothing() {
        let mut app = allowed();
        assert_eq!(app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop))), Effect::None);
        let armed = app.action().expect("armed");
        assert_eq!(armed.verb, ActionVerb::Stop);
        assert_eq!(armed.id, 2);
        assert_eq!(armed.name, "api");
        assert!(!armed.sent, "nothing has gone out");
    }

    /// fails if the action key itself confirms, or if only `Esc` cancels. A
    /// confirm the action key could complete is the double-tap the gate exists
    /// to catch, on a keyboard that may be repeating.
    #[test]
    fn only_enter_confirms_and_every_other_key_cancels() {
        for key in [
            KeyPress::SelectDown,
            KeyPress::SelectUp,
            KeyPress::SelectFirst,
            KeyPress::Refresh,
            KeyPress::Escape,
            KeyPress::FilterStart,
            KeyPress::Action(ActionVerb::Stop),
            KeyPress::Action(ActionVerb::Restart),
        ] {
            let mut app = allowed();
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
            assert!(app.action().is_some(), "armed before {key:?}");
            assert_eq!(app.update(Msg::Key(key)), Effect::None, "{key:?} sent something");
            assert!(app.action().is_none(), "{key:?} did not cancel");
        }

        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert!(
            matches!(app.update(Msg::Key(KeyPress::Confirm)), Effect::Send(_)),
            "and Enter is the one key that sends"
        );
    }

    /// fails if a cancelling keypress ALSO does its ordinary job. This is the
    /// failure mode the whole feature is about: the operator would see the
    /// prompt vanish and the cursor move, and the next reflexive Enter would
    /// act on a target they had already lost track of.
    ///
    /// Three assertions, because each catches a different half of the bug: the
    /// prompt is gone, the cursor did NOT move, and no effect leaked out. The
    /// selection is parked mid-list by `allowed()` so a `j` genuinely could
    /// move it.
    #[test]
    fn a_cancelling_key_is_consumed_and_does_not_also_move_the_selection() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        let before = app.selected();
        let effect = app.update(Msg::Key(KeyPress::SelectDown));
        assert!(app.action().is_none(), "the stray j cancelled the confirm");
        assert_eq!(app.selected(), before, "and did not also move the cursor");
        assert_eq!(effect, Effect::None, "nor ask for a feed read or a walk");
    }

    /// fails if the confirm re-reads the cursor at Enter time. A snapshot can
    /// land between the arming keypress and the Enter, and a confirmation
    /// built from `self.selected` would then act on a sheep the operator never
    /// pointed at. The snapshot below deletes the armed sheep's neighbour so
    /// the cursor genuinely reseats.
    #[test]
    fn the_confirm_is_pinned_to_the_id_it_was_armed_on() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Snapshot {
            rows: vec![
                sheep(2, "api", ProcStatus::Online),
                sheep(9, "new", ProcStatus::Online),
            ],
            at: Instant::now(),
        });
        let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter sends");
        };
        assert_eq!(
            sent,
            Sent::Action {
                verb: ActionVerb::Stop,
                id: 2,
                name: "api".to_string()
            }
        );
    }

    /// fails if a confirm whose sheep left the flock sends anyway. The one
    /// refusal that has to happen at confirm time rather than at arm time.
    #[test]
    fn a_confirm_whose_sheep_left_the_flock_refuses_instead_of_sending() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Event(BusEvent::Process {
            event: ProcessEventKind::Delete,
            info: sheep(2, "api", ProcStatus::Stopped),
            manually: true,
            at_ms: 0,
        }));
        // The prompt came off the screen as soon as the reducer learned the
        // sheep was gone, rather than sitting there as a question about
        // nothing.
        assert!(app.action().is_none());
        assert_eq!(app.update(Msg::Key(KeyPress::Confirm)), Effect::None);
    }

    /// fails if a prompt left armed while the operator walks away never
    /// expires. Driven by `Msg::Tick`, so there is no sleep here (IR-33).
    #[test]
    fn a_confirm_expires_after_ten_seconds_of_ticks() {
        let mut app = allowed();
        let t0 = Instant::now();
        app.update(Msg::Tick { now: t0 });
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Reload)));
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(9),
        });
        assert!(app.action().is_some(), "nine seconds is still armed");
        app.update(Msg::Tick {
            now: t0 + Duration::from_secs(10),
        });
        assert!(app.action().is_none(), "ten is not");
    }

    /// fails if the gate is checked for one key and not the others.
    #[test]
    fn every_action_key_refuses_while_the_gate_is_closed() {
        for verb in [ActionVerb::Stop, ActionVerb::Restart, ActionVerb::Reload] {
            let (mut app, _t0) = started();
            app.update(Msg::Key(KeyPress::Action(verb)));
            assert!(app.action().is_none(), "{verb:?} armed behind a closed gate");
            assert_eq!(
                app.notice().map(ToString::to_string).as_deref(),
                Some("read-only: actions need --allow-control"),
                "{verb:?}"
            );
        }
    }

    /// fails if arming is allowed while the link is not live. An action typed
    /// during the eight second reconnect ladder would otherwise queue and land
    /// seconds later, on a connection the operator has stopped watching (A9).
    #[test]
    fn every_action_key_refuses_while_the_link_is_not_live() {
        for link in [
            Msg::Retrying { attempt: 2 },
            Msg::Frozen {
                at_local: "2026-08-16 09:00:00".to_string(),
            },
        ] {
            let mut app = allowed();
            app.update(link);
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
            assert!(app.action().is_none());
            assert!(app.notice().is_some_and(Notice::is_grave));
        }
    }

    /// fails if a second action can be armed while one is in flight. The
    /// in-flight line names one action; a second one would make it ambiguous
    /// which (A12).
    #[test]
    fn a_second_action_refuses_while_one_is_in_flight() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        assert!(app.action().is_some_and(|action| action.sent));
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("one action is already in flight")
        );
        let action = app.action().expect("the first one is untouched");
        assert_eq!(action.verb, ActionVerb::Stop);
        assert!(action.sent);
    }

    /// fails if the in-flight line is stored as a notice. A notice is cleared
    /// by the next keypress, and an in-flight action whose only sign on screen
    /// could be wiped by a stray `j` is a dashboard hiding something it knows.
    #[test]
    fn an_in_flight_line_survives_a_keypress() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Key(KeyPress::SelectDown));
        assert!(
            app.action().is_some_and(|action| action.sent),
            "the keypress moved the cursor and left the in-flight state alone"
        );
    }

    /// fails if `q` or Ctrl-C stops quitting while a prompt is up. The routing
    /// rule consumes every other key; this is the one carve-out, and
    /// `input.rs`'s own doc says why: an operator whose most reflexive way out
    /// of a terminal program stops working reaches for `kill -9` from another
    /// window, past every restore path `term` has. Quitting discards the
    /// confirm rather than acting on it, so nothing this rule protects is
    /// weakened. See the phase plan's "Shapes the design named" #4.
    #[test]
    fn quit_still_quits_while_a_confirm_is_armed() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        assert_eq!(app.update(Msg::Key(KeyPress::Quit)), Effect::Quit);
    }

    /// fails if Enter does something when nothing is armed. It reaches the
    /// ordinary match in two states an operator can be in: nothing armed at
    /// all, and one action already in flight. Neither may send.
    #[test]
    fn enter_outside_an_armed_confirm_does_nothing() {
        let mut app = allowed();
        assert_eq!(app.update(Msg::Key(KeyPress::Confirm)), Effect::None);
        assert!(app.action().is_none());

        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        assert!(app.action().is_some_and(|action| action.sent), "in flight");
        assert_eq!(
            app.update(Msg::Key(KeyPress::Confirm)),
            Effect::None,
            "a second Enter does not re-send"
        );
    }

    /// fails if a request the channel could not take leaves the dashboard
    /// claiming an action is in flight forever, which would also refuse every
    /// later action for the life of the process.
    #[test]
    fn a_request_that_could_not_be_sent_says_so_and_clears_the_state() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        let Effect::Send(sent) = app.update(Msg::Key(KeyPress::Confirm)) else {
            panic!("Enter sends");
        };
        app.update(Msg::Unsent { sent });
        assert!(app.action().is_none());
        assert!(app.notice().is_some_and(Notice::is_grave));
    }
```

Plus `input.rs`: `every_bound_key_resolves_to_its_press` gains `R` and `L`, and
its doc comment's sentence about `x` never acting gets stronger rather than
weaker, because `x` now does act.

```rust
        assert_eq!(
            map_key(&key(KeyCode::Char('x')), InputMode::Normal),
            Some(KeyPress::Action(ActionVerb::Stop))
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('R')), InputMode::Normal),
            Some(KeyPress::Action(ActionVerb::Restart))
        );
        assert_eq!(
            map_key(&key(KeyCode::Char('L')), InputMode::Normal),
            Some(KeyPress::Action(ActionVerb::Reload))
        );
        assert_eq!(map_key(&key(KeyCode::Char('r')), InputMode::Normal), Some(KeyPress::Refresh));
```

That last line is not padding: `r` and `R` differing by a shift is exactly the
pair a keymap regression would swap, and refresh sitting next to restart is why
`R` is on shift in the first place (A8).

### Step 7.3 - GREEN: `app.rs`

```rust
/// What an action key does.
///
/// Three verbs, and deliberately not four: `start` is whistle's and the CLI's,
/// by the maintainer's ruling. Delete, scale, signal and whisper stay CLI-only for
/// whistle's own reasons: each takes a parameter a dashboard has nowhere to
/// put, or removes an app from the registry, which is the one action no
/// keypress should be one Enter away from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionVerb {
    /// `x`. Stops the sheep; it stays registered.
    Stop,
    /// `R`, on shift because `r` is refresh.
    Restart,
    /// `L`, on shift for symmetry with `R`.
    Reload,
}

impl ActionVerb {
    /// The word the prompt and every outcome sentence begin with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Reload => "reload",
        }
    }
}
```

```rust
/// Whether an action has been sent yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Armed, waiting for the operator's Enter. Nothing has gone out.
    Armed,
    /// Sent, waiting for the shepherd.
    Sent,
}

/// The one action this dashboard is in the middle of.
///
/// The target is captured HERE, by id and by name, and never re-read from the
/// selection: a snapshot can land between the arming keypress and the Enter,
/// and a confirmation that re-read the cursor could act on a sheep the
/// operator never pointed at.
///
/// One field on [`App`] rather than two `Option`s, so "armed" and "in flight"
/// cannot both be true. That is the same claim the one-action-at-a-time rule
/// makes, made in the type instead of in a guard.
#[derive(Debug, Clone)]
struct Action {
    verb: ActionVerb,
    id: u32,
    name: String,
    /// When it was armed. Only an armed action expires.
    at: Instant,
    stage: Stage,
}

/// What the status bar needs to know about the action in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionState<'a> {
    /// Which verb.
    pub verb: ActionVerb,
    /// The pinned sheep's id.
    pub id: u32,
    /// The pinned sheep's name, as it was when the key was pressed.
    pub name: &'a str,
    /// False while it is a question, true once it has gone out.
    pub sent: bool,
}
```

```rust
/// How long an armed confirm waits for its Enter.
///
/// Ten seconds. A prompt left armed while the operator walks away, followed by
/// an Enter typed at what they think is a shell, is the same fat finger
/// arriving by a slower route. It rides `Msg::Tick`, so it costs one `Instant`
/// comparison and no timer (A11).
pub const CONFIRM_EXPIRY: Duration = Duration::from_secs(10);
```

`App` gains `action: Option<Action>`, and:

```rust
    /// The action in progress, for the status bar.
    #[must_use]
    pub fn action(&self) -> Option<ActionState<'_>> {
        let action = self.action.as_ref()?;
        Some(ActionState {
            verb: action.verb,
            id: action.id,
            name: &action.name,
            sent: action.stage == Stage::Sent,
        })
    }
```

**The routing rule**, at the top of `on_key`, after the text-mode branch and
before everything else:

```rust
        // Checked BEFORE the ordinary dispatch, and a cancelling keypress is
        // CONSUMED. A stray `j` during a pending confirm cancels it and does
        // not also move the selection: if it did both, the operator would see
        // the prompt vanish and the cursor move, and the next reflexive Enter
        // would act on a target they had already lost track of.
        //
        // Cancelling is silent. Nothing happened, and reporting nothing as if
        // it were something trains an operator to ignore the status bar.
        //
        // One consequence worth naming: `/` cancels before it opens the filter
        // box, so a confirm and a filter edit can never coexist. That closes
        // the whole interaction between the two features by construction
        // rather than by rule.
        if self.action.as_ref().is_some_and(|action| action.stage == Stage::Armed) {
            if key == KeyPress::Confirm {
                return self.confirm();
            }
            // The one key the cancel does not consume. `input.rs`'s own doc
            // says why: dropping Ctrl-C would leave the most reflexive way out
            // of a terminal program doing nothing, and the operator's next
            // move is `kill -9` from another window, past every restore path
            // `super::term` has. Quitting DISCARDS the confirm rather than
            // acting on it, so the property this rule exists for is untouched;
            // that property is about a cancelling key also doing its ordinary
            // job on a target the operator has lost track of. Text mode makes
            // the same carve-out. See the phase plan's "Shapes the design
            // named" #4.
            if key == KeyPress::Quit {
                return Effect::Quit;
            }
            self.action = None;
            return Effect::None;
        }
```

`KeyPress` gains `Confirm` (`Enter` in normal mode) and
`Action(ActionVerb)` replaces `Stop`. `map_key`'s normal-mode arm gains
`KeyCode::Enter => Some(KeyPress::Confirm)` and the three action keys.

`on_key`'s ordinary match, which is exhaustive, therefore needs an arm for
`Confirm` as well. **Say what it does rather than leaving it to be guessed:**

```rust
            // Enter means nothing outside an armed confirm. It reaches this
            // match whenever nothing is armed, which includes while an action
            // is IN FLIGHT, because the routing rule above only fires on
            // `Stage::Armed`. Named rather than swept into a wildcard: on the
            // one key whose job is to confirm a stop, an unspecified arm is
            // one edit away from being the wrong arm.
            KeyPress::Confirm => Effect::None,
```

and the new `Action` arm is `KeyPress::Action(verb) => self.arm(verb)`.

**Arming:**

```rust
    /// Arms a confirm, or refuses and says why.
    ///
    /// Every refusal happens HERE rather than at confirm time, so an operator
    /// never answers a question that was never going to be honoured. Every
    /// sentence is literal: the standing rule is that nothing about damage
    /// gets charming, and a stop is damage.
    ///
    /// The ladder's order is the design's error table, read top to bottom:
    /// gate, link, nothing selected, one already in flight. It only shows when
    /// two conditions hold at once, and there is no reason to reorder an
    /// approved table.
    fn arm(&mut self, verb: ActionVerb) -> Effect {
        let refusal = if self.control == Control::ReadOnly {
            Some("read-only: actions need --allow-control".to_string())
        } else if !matches!(self.link, Link::Live) {
            // The same sentence `r` already gives when the link is gone,
            // moved into `LINK_GONE` by this task and reused rather than
            // retyped.
            Some(LINK_GONE.to_string())
        } else if self.selected_row().is_none() {
            Some("no sheep is selected".to_string())
        } else if self.action.is_some() {
            Some("one action is already in flight".to_string())
        } else {
            None
        };
        if let Some(text) = refusal {
            self.notice = Some(Notice { text, grave: true });
            return Effect::None;
        }
        let row = self.selected_row().expect("checked just above");
        self.action = Some(Action {
            verb,
            id: row.info.id,
            name: row.info.name.clone(),
            at: self.now,
            stage: Stage::Armed,
        });
        Effect::None
    }
```

**Move the existing literal**, do not retype it: cut the sentence out of the
`KeyPress::Refresh` arm into

```rust
/// The sentence `r` gives when the link is gone. The action keys refuse with
/// the same one, so the two cannot drift apart.
const LINK_GONE: &str = ...;
```

byte for byte, and have both arms read the constant.

**Confirming:**

```rust
    /// The operator's Enter. Sends, or refuses because the target left.
    fn confirm(&mut self) -> Effect {
        let Some(action) = self.action.take() else {
            return Effect::None;
        };
        // The whole flock, not the visible set: a filter typed after arming
        // hides a sheep, it does not remove it.
        if !self.flock.contains_key(&action.id) {
            self.notice = Some(Notice {
                text: format!(
                    "{} {} (id {}): it is no longer in the flock",
                    action.verb.label(),
                    action.name,
                    action.id
                ),
                grave: true,
            });
            return Effect::None;
        }
        let sent = Sent::Action {
            verb: action.verb,
            id: action.id,
            name: action.name.clone(),
        };
        self.action = Some(Action {
            stage: Stage::Sent,
            ..action
        });
        Effect::Send(sent)
    }

    /// Takes an armed prompt off the screen once its sheep is gone, rather
    /// than leaving a question about nothing. Called from the `Snapshot` arm
    /// and from the `Delete` arm; an action already in flight keeps its line.
    fn forget_missing_target(&mut self) {
        let gone = self.action.as_ref().is_some_and(|action| {
            action.stage == Stage::Armed && !self.flock.contains_key(&action.id)
        });
        if gone {
            self.action = None;
        }
    }
```

**Expiry**, inside the `Msg::Tick` arm's existing non-`Lost` branch, beside
`self.now = now`:

```rust
                    let expired = self.action.as_ref().is_some_and(|action| {
                        action.stage == Stage::Armed
                            && now.saturating_duration_since(action.at) >= CONFIRM_EXPIRY
                    });
                    if expired {
                        self.action = None;
                    }
```

**And the invariant that makes putting it there safe.** `now` stops advancing
once the link is `Lost`, so this check never runs again after a freeze; on its
own that would leave an armed prompt on a frozen dashboard forever, with Enter
still willing to send. A9 refuses arming unless the link is `Live` and this
design's whole refusal doctrine is that an operator never answers a question
that was never going to be honoured, so the answer is not a second check in
`confirm` but keeping the invariant true:

```rust
    /// Takes an armed prompt off the screen when the link stops being live.
    ///
    /// Called from the `Msg::Retrying` and `Msg::Frozen` arms. A9 refuses to
    /// ARM unless the link is `Live`; without this, a prompt armed a moment
    /// earlier would outlive the connection it was going to be sent over, and
    /// on a frozen dashboard it would never expire either, because `now` stops
    /// advancing and the expiry check rides it. Silent, like every other
    /// cancel: nothing happened, and the banner appearing on the same frame
    /// already says why.
    ///
    /// An action already SENT keeps its line. It is a real request, and
    /// `run_connected` answers it with an `Err` before its loop ends, so the
    /// in-flight line always resolves rather than hanging.
    fn disarm_on_link_change(&mut self) {
        if self.action.as_ref().is_some_and(|action| action.stage == Stage::Armed) {
            self.action = None;
        }
    }
```

With that, "armed implies `Link::Live`" holds everywhere and `confirm` needs no
link check at all.

Add the test:

```rust
    /// fails if an armed prompt outlives the connection it was going to be
    /// sent over. Both halves matter: `Retrying` because A9 says an action
    /// typed during the reconnect ladder is refused rather than queued, and
    /// `Frozen` because a frozen dashboard's `now` stops advancing, so an
    /// armed prompt there would never expire either.
    #[test]
    fn a_link_that_stops_being_live_takes_an_armed_prompt_down() {
        for link in [
            Msg::Retrying { attempt: 2 },
            Msg::Frozen {
                at_local: "2026-08-16 09:00:00".to_string(),
            },
        ] {
            let mut app = allowed();
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
            assert!(app.action().is_some(), "armed while live");
            app.update(link);
            assert!(app.action().is_none(), "and gone once the link is not");
            assert_eq!(
                app.update(Msg::Key(KeyPress::Confirm)),
                Effect::None,
                "so Enter has nothing to send"
            );
        }
    }
```

**`Sent` and `Effect` grow:**

```rust
    /// One action against one sheep. `name` rides along so a reply can be
    /// reported by name even after the sheep has left the flock.
    Action {
        /// Which verb.
        verb: ActionVerb,
        /// The pinned sheep.
        id: u32,
        /// Its name at arm time.
        name: String,
    },
```

```rust
            Self::Action { verb, id, .. } => {
                let selector = SelectorSpec::Id(id);
                match verb {
                    ActionVerb::Stop => Request::Stop { selector },
                    ActionVerb::Restart => Request::Restart { selector },
                    ActionVerb::Reload => Request::Reload { selector },
                }
            }
```

`Effect` gains `Send(Sent)` and **loses `Copy`** (`Sent::Action` carries a
`String`). It keeps `Debug, Clone, PartialEq, Eq`; nothing matches on an
`Effect` by reference, so no call site changes.

`Msg` gains:

```rust
    /// A request the caller could not hand to the link task.
    ///
    /// The reducer has already entered the in-flight state by the time
    /// `run_ui` tries to send, so a `try_send` that fails has to come back:
    /// otherwise the bar keeps saying "sent, waiting for the shepherd" about a
    /// request nobody has, and the one-action-at-a-time guard refuses every
    /// later action for the life of the process.
    Unsent {
        /// What could not be sent.
        sent: Sent,
    },
```

```rust
            Msg::Unsent { sent } => match sent {
                Sent::Action { verb, id, name } => {
                    self.action = None;
                    self.notice = Some(Notice {
                        // "it was not sent", and no cause. The reducer does
                        // not know one: the channel holds 2, it is shared with
                        // lamb fetches, and `run_connected` awaits each
                        // request inline, so `Full` is reachable while the
                        // shepherd is perfectly reachable and merely slow.
                        // Naming a cause nothing observed is the failure the
                        // `-` CPU cell exists to prevent.
                        text: format!("{} {name} (id {id}): it was not sent", verb.label()),
                        grave: true,
                    });
                    Effect::None
                }
                // A dropped lamb fetch already reads as "not read yet", which
                // is what the pane says. Nothing to report and nothing to
                // clear.
                Sent::Lambs { .. } => Effect::None,
            },
```

Finally, `Control::Allowed`'s own doc comment says today that there is exactly
one action key and that no action lands behind the gate. Rewrite it to what is
now true.

### Step 7.4 - GREEN: `run_ui`'s one new arm

```rust
            Effect::Send(sent) => {
                // `try_send`, not `send`: blocking the UI on a full channel
                // would stall the screen for as long as the shepherd is slow.
                // A failure comes straight back to the reducer rather than
                // being dropped, because the reducer is already showing an
                // in-flight line about it.
                if let Err(err) = requests.try_send(sent) {
                    let (mpsc::error::TrySendError::Full(sent)
                    | mpsc::error::TrySendError::Closed(sent)) = err;
                    // `let _`: `Msg::Unsent` returns `Effect::None` by
                    // construction, and acting on a returned effect here is
                    // the one place this design could recurse.
                    let _ = app.update(Msg::Unsent { sent });
                }
                dirty = true;
            }
```

### Step 7.5 - verify

```bash
cargo fmt --all --check
cargo test -p shep --lib --bins --all-features
```

Roughly `605 passed; 0 failed; 3 ignored`. `grep -rn 'stop is not built yet'
crates/ | wc -l` now prints **1**: the literal is gone from `app.rs` and only
`cli.rs`'s flag doc still carries it, which Step 10.3 fixes.

### Step 7.6 - MUTATION: the one this task exists for

Delete the `return Effect::None` after `self.action = None` in the routing
rule, so a cancelling key falls through to the ordinary dispatch.

**Must redden `a_cancelling_key_is_consumed_and_does_not_also_move_the_selection`**
on both the selection assertion and the effect assertion. This is the mutation
that passes a hand test and fails in practice: the prompt does vanish, so a
human pressing `j` and looking at the bar sees exactly what they expect, while
the cursor has quietly moved under it. Revert.

### Step 7.7 - second MUTATION

Make the action key itself confirm: change the routing rule's test to
`if key == KeyPress::Confirm || key == KeyPress::Action(action.verb)`.

**Must redden `only_enter_confirms_and_every_other_key_cancels`** on the
`KeyPress::Action(ActionVerb::Stop)` iteration. Revert.

### Step 7.8 - third MUTATION

Build the request from `self.selected` at Enter time instead of from the pinned
id: in `confirm`, replace `action.id` with `self.selected.unwrap_or_default()`.

**Must redden `the_confirm_is_pinned_to_the_id_it_was_armed_on`**, whose
snapshot reseats the cursor between the arm and the Enter. Revert.

### Step 7.9 - fourth MUTATION

Allow arming while `Retrying`: change `!matches!(self.link, Link::Live)` to
`matches!(self.link, Link::Lost { .. })`.

**Must redden `every_action_key_refuses_while_the_link_is_not_live`** on the
`Msg::Retrying` iteration. Revert.

---

# Task 8 - the reply

`app.rs`. The inbound half, and the place where the design's honesty rule lives:
**no client-side row state is invented.**

`Response::Stopped`, `Restarted` and `Reloading` all carry `Vec<ProcessInfo>`,
which is the shepherd's own rows after the action. So the reply is upserted
into the flock map exactly the way a bus event already is, and the table
updates from the shepherd's words rather than from a guess.

### Step 8.1 - baseline

```bash
grep -c 'Msg::Replied { sent, result }' crates/shep-cli/src/lookout/app.rs   # 1: the arm
grep -c 'Sent::Action' crates/shep-cli/src/lookout/app.rs                    # 0
cargo test -p shep --lib --all-features -- lookout::app                      # note the number
```

Not a bare `grep -c 'Msg::Replied'`: Task 4's tests construct one eight times
over, so that grep prints nine after Task 4 and counting it would be the
"count the call, not the word" failure the baselines section names. The arm is
what this task extends, so the arm is what the baseline pins.

### Step 8.2 - RED

```rust
    /// fails if the reply's rows are thrown away and the table left to wait
    /// for the next poll. The shepherd's own rows are right there.
    #[test]
    fn an_accepted_stop_upserts_the_rows_the_shepherd_returned() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Replied {
            sent: Sent::Action {
                verb: ActionVerb::Stop,
                id: 2,
                name: "api".to_string(),
            },
            result: Ok(Response::Stopped(vec![sheep(2, "api", ProcStatus::Stopped)])),
        });
        assert_eq!(
            app.rows()
                .iter()
                .find(|row| row.info.id == 2)
                .map(|row| row.info.status),
            Some(ProcStatus::Stopped),
            "the table shows what the shepherd said, without waiting for a poll"
        );
        assert_eq!(
            app.notice().map(ToString::to_string).as_deref(),
            Some("stop api (id 2): the shepherd stopped it")
        );
        assert!(app.action().is_none(), "the in-flight state cleared");
    }

    /// fails if a reload reply claims the swap finished. `Response::Reloading`
    /// is an ACCEPTANCE, its own doc says so, and the swaps arrive afterwards
    /// on the bus as `process.reload` / `process.reloaded` /
    /// `process.reload_abandoned`, which the table already consumes. A
    /// sentence saying "reloaded" would be the one lie this reply makes easy
    /// to tell.
    #[test]
    fn a_reload_reply_does_not_claim_the_swap_finished() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Reload)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Replied {
            sent: Sent::Action {
                verb: ActionVerb::Reload,
                id: 2,
                name: "api".to_string(),
            },
            result: Ok(Response::Reloading(vec![sheep(2, "api", ProcStatus::Online)])),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert_eq!(
            said,
            "reload api (id 2): accepted, the swaps report themselves as they happen"
        );
        assert!(!said.contains("reloaded"), "got {said:?}");
    }

    /// fails if the daemon's own words are replaced with a canned string. The
    /// message is a sentence a human wrote; `RequestError`'s full `Display`
    /// interpolates the code with `{:?}` and would put a Rust identifier on an
    /// operator's screen.
    #[test]
    fn a_daemon_refusal_reaches_the_bar_in_the_daemons_own_words() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Replied {
            sent: Sent::Action {
                verb: ActionVerb::Restart,
                id: 2,
                name: "api".to_string(),
            },
            result: Err(RequestError::Rpc(RpcError {
                code: RpcErrorCode::NotFound,
                message: "selector matched no registered sheep".to_string(),
            })),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert_eq!(said, "restart api (id 2): selector matched no registered sheep");
        assert!(!said.contains("NotFound"), "no Rust identifiers: {said:?}");
        assert!(app.notice().is_some_and(Notice::is_grave));
    }

    /// fails if a connection that died mid-request reports as anything else.
    #[test]
    fn a_connection_that_died_mid_request_says_so_under_the_same_prefix() {
        let mut app = allowed();
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
        app.update(Msg::Key(KeyPress::Confirm));
        app.update(Msg::Replied {
            sent: Sent::Action {
                verb: ActionVerb::Stop,
                id: 2,
                name: "api".to_string(),
            },
            result: Err(RequestError::Closed),
        });
        let said = app.notice().map(ToString::to_string).unwrap_or_default();
        assert!(said.starts_with("stop api (id 2): "), "got {said:?}");
        assert!(said.contains(&RequestError::Closed.to_string()));
    }

    /// fails if a reply this binary does not understand reads as success.
    /// `Response` is `#[non_exhaustive]`, and swallowing an unrecognised
    /// variant into `Ok` is what `flock()` does and what this must not: a
    /// stop that silently reported success while the sheep kept running is the
    /// worst outcome this feature has.
    ///
    /// The second half is the sharper case: the RIGHT SHAPE for the wrong
    /// verb. A `Stopped` answering a `Restart` carries rows and would upsert
    /// happily.
    #[test]
    fn an_unrecognised_reply_says_so_rather_than_reading_as_success() {
        for reply in [
            Response::Pong,
            Response::Stopped(vec![sheep(2, "api", ProcStatus::Stopped)]),
        ] {
            let mut app = allowed();
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
            app.update(Msg::Key(KeyPress::Confirm));
            app.update(Msg::Replied {
                sent: Sent::Action {
                    verb: ActionVerb::Restart,
                    id: 2,
                    name: "api".to_string(),
                },
                result: Ok(reply),
            });
            assert_eq!(
                app.notice().map(ToString::to_string).as_deref(),
                Some(
                    "restart api (id 2): the shepherd answered something this lookout does not understand"
                )
            );
            assert!(app.notice().is_some_and(Notice::is_grave));
        }
    }
```

### Step 8.3 - GREEN

```rust
            Msg::Replied { sent, result } => match sent {
                Sent::Lambs { id } => self.on_lambs(id, result),
                Sent::Action { verb, id, name } => self.on_action_reply(verb, id, &name, result),
            },
```

```rust
    /// One action's answer: the shepherd's rows upserted, and one sentence.
    ///
    /// No provisional row state is invented anywhere on this path. The three
    /// replies carry `Vec<ProcessInfo>`, so the table updates from the
    /// shepherd's own words; an `online, restart sent...` in the STATUS column
    /// would be a guess printed in the one column whose whole job is to be
    /// true, and it would have to negotiate with the narrow-terminal column
    /// drop order for the privilege.
    fn on_action_reply(
        &mut self,
        verb: ActionVerb,
        id: u32,
        name: &str,
        result: Result<Response, RequestError>,
    ) -> Effect {
        self.action = None;
        let prefix = format!("{} {name} (id {id})", verb.label());
        // Each verb accepts its own reply and no other. A `Stopped` answering
        // a `Restart` carries rows and would upsert perfectly happily, which
        // is why the guards are on the arms rather than a single
        // rows-carrying match.
        let rows = match result {
            Ok(Response::Stopped(rows)) if verb == ActionVerb::Stop => rows,
            Ok(Response::Restarted(rows)) if verb == ActionVerb::Restart => rows,
            Ok(Response::Reloading(rows)) if verb == ActionVerb::Reload => rows,
            Ok(_unrecognised) => {
                self.notice = Some(Notice {
                    text: format!(
                        "{prefix}: the shepherd answered something this lookout does not understand"
                    ),
                    grave: true,
                });
                return Effect::None;
            }
            // The daemon's own message, not `RequestError`'s full `Display`:
            // the latter interpolates the code with `{:?}` and produces
            // "the daemon reported NotFound: ...", which puts a Rust
            // identifier on an operator's screen. The message alone is
            // already a sentence a human wrote.
            Err(RequestError::Rpc(err)) => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: {}", err.message),
                    grave: true,
                });
                return Effect::None;
            }
            Err(other) => {
                self.notice = Some(Notice {
                    text: format!("{prefix}: {other}"),
                    grave: true,
                });
                return Effect::None;
            }
        };
        let anchor = self.now;
        let was_empty = self.flock.is_empty();
        for info in rows {
            self.flock.insert(info.id, Row { info, anchor });
        }
        self.notice = Some(Notice {
            text: format!("{prefix}: {}", outcome(verb)),
            grave: false,
        });
        if was_empty && self.reseat(None) {
            return Effect::RefreshSelected;
        }
        Effect::None
    }
```

```rust
/// What the bar says once the shepherd has answered.
///
/// Reload's wording is not decoration. `Response::Reloading` is an acceptance
/// and not a result, and the swaps arrive afterwards on the bus, which the
/// table already consumes.
const fn outcome(verb: ActionVerb) -> &'static str {
    match verb {
        ActionVerb::Stop => "the shepherd stopped it",
        ActionVerb::Restart => "the shepherd restarted it",
        ActionVerb::Reload => "accepted, the swaps report themselves as they happen",
    }
}
```

### Step 8.4 - verify

```bash
cargo fmt --all --check
cargo test -p shep --lib --bins --all-features
```

Roughly `611 passed; 0 failed; 3 ignored`.

### Step 8.5 - MUTATION

Ignore the reply's rows and wait for the poll: delete the `for info in rows`
loop.

**Must redden `an_accepted_stop_upserts_the_rows_the_shepherd_returned`** on
the status assertion. Revert.

### Step 8.6 - second MUTATION

Word reload's outcome as `the shepherd reloaded it`.

**Must redden `a_reload_reply_does_not_claim_the_swap_finished`.** Revert.

### Step 8.7 - third MUTATION

Drop the per-verb guards: match `Ok(Response::Stopped(rows) |
Response::Restarted(rows) | Response::Reloading(rows))` in one arm.

**Must redden `an_unrecognised_reply_says_so_rather_than_reading_as_success`**
on its second iteration, the `Stopped`-for-a-`Restart` case. The first
iteration passes either way, which is why the loop has two. Revert.

---

# Task 9 - the actions on screen, five frames, and the docs

`view/status.rs` and `frames.rs`, plus the two prose documents. **Full task
gate at the end.**

### Step 9.1 - baseline

```bash
grep -c 'hint_for' crates/shep-cli/src/lookout/view/status.rs     # 2: the fn and its one call
grep -c 'x stop' crates/shep-cli/src/lookout/view/status.rs       # 0, the hint has one form so far
grep -c '^=== ' docs/lookout/frames.txt                           # 19
grep -c 'Scene::ALL.len(), 19' crates/shep-cli/src/lookout/frames.rs  # 1
```

### Step 9.2 - RED

`view/status.rs`:

```rust
    /// fails if the prompt stops naming the verb and the exact sheep, or stops
    /// saying what answers it. A question an operator has to guess the answer
    /// to is worse than no question.
    #[test]
    fn an_armed_confirm_names_the_verb_the_sheep_and_the_answer() {
        let app = armed_app(ActionVerb::Restart);
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("restart api (id 2)?"), "got {bar:?}");
        assert!(bar.contains("enter confirms, any other key cancels"), "got {bar:?}");
    }

    /// fails if the in-flight line stops saying that the shepherd has not
    /// answered yet. Nothing on the table has changed at this point, because
    /// nothing the shepherd said has changed yet, and the bar is the only
    /// thing on screen that knows a request is out.
    #[test]
    fn an_in_flight_action_says_it_is_waiting() {
        let app = acting_app(ActionVerb::Stop);
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("stop api (id 2): sent, waiting for the shepherd"), "got {bar:?}");
    }

    /// fails if the prompt loses its place at the top of the bar. A question
    /// awaiting an answer outranks a report of something that already
    /// happened, which outranks a persistent state the title also signals
    /// (A18).
    #[test]
    fn the_confirm_outranks_a_notice_and_the_filter_line() {
        let app = armed_app_with_a_filter_and_a_notice();
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("stop api (id 2)?"), "got {bar:?}");
        assert!(!bar.contains("filter \""), "the filter line is below it: {bar:?}");
    }

    /// fails if a refusal made while an action is in flight never reaches the
    /// screen. THIS is the test the four-slot ordering would have failed, and
    /// no reducer test can catch it: `arm`'s "one action is already in flight"
    /// is a `Notice`, so a bar that ranked the in-flight line above notices
    /// would set it, assert it in the reducer, and never draw it. The operator
    /// presses `R` while a stop is out, nothing on screen changes, and the
    /// dashboard has silently swallowed the answer to their own keypress.
    ///
    /// The second half is the same defect arriving from the bus rather than
    /// from a key: `Dropped`, `BusLagged` and `DaemonShutdown` would all be
    /// invisible for as long as an action was in flight.
    #[test]
    fn a_refusal_while_an_action_is_in_flight_reaches_the_bar() {
        let mut app = acting_app(ActionVerb::Stop);
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("one action is already in flight"),
            "the refusal is on the bar, not only in the reducer: {bar:?}"
        );

        let mut app = acting_app(ActionVerb::Stop);
        app.update(Msg::Event(BusEvent::DaemonShutdown));
        let bar = rendered(&status_line(&app, 120));
        assert!(bar.contains("the shepherd is shutting down"), "got {bar:?}");
    }

    /// fails if the in-flight line does not come back once the notice is
    /// cleared. The mirror of the test above, and what makes ranking the
    /// notice higher honest rather than lossy: the covering is transient, and
    /// the next keypress ends it.
    #[test]
    fn the_in_flight_line_comes_back_when_the_notice_clears() {
        let mut app = acting_app(ActionVerb::Stop);
        app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        app.update(Msg::Key(KeyPress::SelectDown));
        let bar = rendered(&status_line(&app, 120));
        assert!(
            bar.contains("stop api (id 2): sent, waiting for the shepherd"),
            "got {bar:?}"
        );
    }

    /// fails if the action keys are advertised behind a closed gate. This
    /// file's standing rule: a hint that needs a footnote to be true is an
    /// asterisk, not a hint.
    #[test]
    fn the_action_keys_are_advertised_only_when_the_gate_is_open() {
        let closed = rendered(&status_line(&filtered_app(""), 200));
        for key in ["x stop", "R restart", "L reload"] {
            assert!(!closed.contains(key), "{key} advertised read-only: {closed:?}");
        }
        let open = rendered(&status_line(&allowed_app(), 200));
        for key in ["x stop", "R restart", "L reload"] {
            assert!(open.contains(key), "{key} missing when the gate is open: {open:?}");
        }
        assert!(open.contains("/ filter"), "and the filter key survives both forms");
    }
```

The shipped `a_truncated_hint_still_leaves_a_gap_before_the_control_label` at
49 columns stays exactly as it is and must stay green: the read-only hint is 59
characters (Task 3 updated its doc comment to say so), the label is 9, and the
hint still truncates at that width, which is the combination the test was
written to measure. `a_wide_status_line_still_pads_out_to_the_full_width` uses
`Control::Allowed` at 120 columns and also stays green: the control hint is 62
characters against 103 of room.

The four fixtures these tests use, in `view/fixtures.rs`:

```rust
/// [`filtered_app`]'s four sheep with the gate open and the cursor on `api`
/// at id 2, which is the sheep every action assertion in this file names.
pub fn allowed_app() -> App {
    let mut app = app_with(named_flock(), plain());
    app.set_control_for_tests(Control::Allowed);
    app.update(Msg::Key(KeyPress::SelectDown));
    app
}

/// [`allowed_app`] with `verb` armed and nothing sent.
pub fn armed_app(verb: ActionVerb) -> App {
    let mut app = allowed_app();
    app.update(Msg::Key(KeyPress::Action(verb)));
    app
}

/// [`armed_app`] confirmed: the request is out and the reply has not landed.
pub fn acting_app(verb: ActionVerb) -> App {
    let mut app = armed_app(verb);
    app.update(Msg::Key(KeyPress::Confirm));
    app
}

/// An armed confirm with a filter applied AND a notice standing, so the bar
/// has something in three slots at once and the ordering assertion has
/// something to fail on.
///
/// Order matters: the filter is applied first, then the action is armed, then
/// the notice is raised - NOT the other way round. Arming is a keypress, and
/// `on_key`'s normal branch opens with `self.notice = None`, so arming AFTER
/// the notice would wipe the very notice this fixture exists to leave
/// standing. `Msg::Event(BusEvent::Dropped { .. })` never passes through
/// `on_key` at all, so raising the notice last is what makes it survive.
pub fn armed_app_with_a_filter_and_a_notice() -> App {
    let mut app = filtered_app("api");
    app.set_control_for_tests(Control::Allowed);
    app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
    app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
    app
}
```

`App::new` takes `Control` and the shipped `app_with` hard-codes
`Control::ReadOnly`, so `set_control_for_tests` is a `#[cfg(test)] pub(crate)`
setter on `App` rather than a second copy of `app_with`. One line, and it keeps
the four fixtures above reading as a chain instead of as four near-duplicates
of the same eight-line builder.

### Step 9.3 - GREEN

`hint_for` gains its second form:

```rust
fn hint_for(control: Control) -> String {
    match control {
        Control::ReadOnly => "q quit   j/k select   g/G first/last   r refresh   / filter",
        // `g/G` and `r` drop out to make room. They are the two an operator
        // rediscovers by pressing them; an action key is not.
        Control::Allowed => "q quit   j/k select   / filter   x stop   R restart   L reload",
    }
    .to_string()
}
```

and `status_line`'s left slot gains its two remaining entries. **They are not
adjacent.** The armed confirm is slot 1, above the filter box; the in-flight
line is slot 4, BELOW the notice. Task 3 built the chain with both gaps in it,
so this is two insertions, not one:

```rust
    let (left, left_style) = if let Some(action) = app.action().filter(|a| !a.sent) {
        // Slot 1. A18: a question awaiting an answer outranks everything,
        // including the filter box, which it cannot coexist with anyway
        // because `/` cancels a confirm before it opens the box.
        (
            format!(
                "{} {} (id {})? enter confirms, any other key cancels",
                action.verb.label(),
                action.name,
                action.id
            ),
            palette.attention(),
        )
    } else if app.mode() == InputMode::Text {
        // Slot 2, from Task 3, unchanged.
        ...
    } else if let Some(notice) = app.notice() {
        // Slot 3, from Task 3, unchanged.
        ...
    } else if let Some(action) = app.action() {
        // Slot 4. BELOW the notice, and that is load-bearing rather than a
        // concession. `arm`'s "one action is already in flight" IS a notice,
        // so a bar that put this line above notices would swallow the answer
        // to the operator's own keypress: they press `R` while a stop is out,
        // the screen does not change, and the dashboard has hidden a refusal
        // it made. Every bus-raised notice (`Dropped`, `BusLagged`,
        // `DaemonShutdown`) would be equally invisible for as long as an
        // action was in flight. A18 puts the notice above the filter line and
        // the design puts this line above the filter line; the notice above
        // this is what satisfies both.
        //
        // What the design DOES require of this state is that a keypress
        // cannot wipe it, and that is a property of the reducer, not of this
        // order: the next keypress clears the notice and this line comes back.
        // `an_in_flight_line_survives_a_keypress` pins the first half and
        // `a_refusal_while_an_action_is_in_flight_reaches_the_bar` the second.
        let text = format!(
            "{} {} (id {}): sent, waiting for the shepherd",
            action.verb.label(),
            action.name,
            action.id
        );
        // `attention`, the same butter the non-grave notice uses. Not a modal,
        // not a box, not a `ratatui::widgets::Clear`: there is no overlay
        // anywhere in this module, and one rule under the header beats a full
        // border for a pane somebody reads at 3am.
        (text, palette.attention())
    } else if !app.filter().is_empty() {
        // Slot 5, from Task 3, unchanged.
        ...
    } else {
        // Slot 6, from Task 3, unchanged.
        ...
    };
```

The `.filter(|a| !a.sent)` on the first arm is what splits one field across two
slots. `ActionState` is `Copy`, so this costs nothing and reads better than
matching on `sent` inside a single arm and then having to place that arm at one
priority or the other.

The full order, and the reason each slot sits where it does, is in
"Shapes the design named" #2 at the top of this plan. It is six slots, not
five, and the in-flight line is fourth.

### Step 9.4 - GREEN: five frames

**Five scenes, not three.** The three the design sketched, plus two states an
operator will meet that nothing else in the gallery shows. Both were missing
from this plan's first draft and one of them it explicitly promised:

```rust
    /// An action key pressed with the gate open. Nothing has been sent.
    Confirm,
    /// Enter pressed. The request is out.
    Acting,
    /// The shepherd refused, in its own words.
    ActionRefused,
    /// The shepherd did it, and the bar says so in the non-grave style.
    ActionAccepted,
    /// An action key pressed while the link is coming back.
    ActionRefusedOffline,
```

- **`ActionAccepted`** is the only frame in the gallery that shows an action
  SUCCEEDING. Without it, all three action frames are a question, a wait and a
  refusal, and the one styling decision in feature 2 that is not `refusal()`
  (the outcome sentence is `attention()`, per the design's error table) is
  never rendered. It also pins the reply's rows reaching the table: the frame
  shows `api` as `stopped`, which is the shepherd's own row and not a guess.
- **`ActionRefusedOffline`** is the frame the closing section of this plan
  promises the maintainer will judge from. That section says arming while `Retrying`
  refuses with the sentence `r` gives when the link is GONE, one row under a
  banner saying the shepherd is being reconnected to, and that "if it reads
  wrong in the gallery, the fix is one new sentence". It cannot read wrong in a
  gallery that does not contain it. This is the frame that puts the two
  disagreeing rows on one screen so the question can actually be answered.

`(100, 14)` for all five, and a per-scene control state, which the six
action-carrying scenes need and the other eighteen do not:

```rust
    /// Whether this scene's dashboard may act.
    ///
    /// `Lambs` is in here as well as the three action scenes: its bar has
    /// nothing in the left slot, so it is the one frame in the gallery that
    /// shows the control-enabled key hint.
    #[must_use]
    pub const fn control(self) -> Control {
        match self {
            Self::Confirm
            | Self::Acting
            | Self::ActionRefused
            | Self::ActionAccepted
            | Self::ActionRefusedOffline
            | Self::Lambs => Control::Allowed,
            _ => Control::ReadOnly,
        }
    }
```

`scene_with` reads `which.control()` where it currently passes
`Control::ReadOnly`. Two more arms in the same match block that applies the
filter keys.

`ActionRefusedOffline` raises its own `Msg::Retrying` rather than going in the
shipped `Retrying`/`Frozen` block, because the order is the whole state it
shows: the link has to stop being live BEFORE the key is pressed, or `arm`
would accept it.

```rust
        Scene::ActionRefusedOffline => {
            app.update(Msg::Retrying { attempt: 3 });
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
        }
```

```rust
        Scene::Confirm | Scene::Acting | Scene::ActionRefused | Scene::ActionAccepted => {
            app.update(Msg::Key(KeyPress::Action(ActionVerb::Restart)));
            if which != Scene::Confirm {
                app.update(Msg::Key(KeyPress::Confirm));
            }
            if which == Scene::ActionAccepted {
                app.update(Msg::Replied {
                    sent: Sent::Action {
                        verb: ActionVerb::Restart,
                        id: 2,
                        name: "api".to_string(),
                    },
                    result: Ok(Response::Restarted(vec![restarted_api()])),
                });
            }
            if which == Scene::ActionRefused {
                // The sheep leaves the flock while the request is out, which
                // is what makes the daemon's own sentence the true one.
                app.update(Msg::Snapshot {
                    rows: flock_without_api(),
                    at: t0,
                });
                app.update(Msg::Replied {
                    sent: Sent::Action {
                        verb: ActionVerb::Restart,
                        id: 2,
                        name: "api".to_string(),
                    },
                    result: Err(RequestError::Rpc(RpcError {
                        code: RpcErrorCode::NotFound,
                        message: "selector matched no registered sheep".to_string(),
                    })),
                });
            }
        }
```

Captions, each clause pinned:

```rust
            Self::Confirm => {
                "`R` pressed with the gate open. Nothing has been sent: the bar asks a question naming the verb and the exact sheep, and `api` is still online in the table behind it."
            }
            Self::Acting => {
                "Enter pressed. The request is out and nothing on the table has changed, because nothing the shepherd has said has changed: `api` is still online and the cursor has not moved."
            }
            Self::ActionAccepted => {
                "The shepherd answered. The bar says what it did, in the non-grave style a refusal does not get, and the table shows the row the reply carried rather than waiting for the next poll."
            }
            Self::ActionRefused => {
                "The shepherd refused while the request was out, and its own sentence is forwarded rather than rewritten. The sheep has left the flock in the listing behind it, so the table is one row shorter and the cursor has moved to the row below."
            }
            Self::ActionRefusedOffline => {
                "An action key pressed while the link is coming back. The refusal is the same sentence `r` gives, one row under a banner saying the shepherd is being reconnected to."
            }
```

**Three clauses came off the first draft's captions rather than being pinned**,
which is this file's own rule ("every clause of every caption is one assertion,
or it is deleted from the caption") and the rule 12a and 12b each shipped a
violation of:

- Confirm's "Enter is the only key that answers it, and every other key takes
  the question away and moves nothing" is a claim about behaviour across eight
  keypresses. A frame is one render; it cannot show it. It is
  `only_enter_confirms_and_every_other_key_cancels`'s claim and it stays in the
  reducer, with a `// see:` comment beside the caption pointing at it.
- Acting's "This line is not a notice, so a stray keypress cannot wipe it" is
  the same shape, pinned by `an_in_flight_line_survives_a_keypress`. Same
  treatment.
- Confirm's replacement clause, "`api` is still online in the table behind it",
  IS pinnable and is worth more than the sentence it replaces: it is the frame
  proving that arming did not act.

`ActionRefusedOffline`'s caption deliberately does not say whether the two rows
reading differently is right. That is the question the frame exists to put in
front of the maintainer, and a caption that answered it would be this plan deciding A9 on
her behalf. See the closing section.

Assertions:

A small helper, because four of these assertions name a specific row of the
table and `contains` over the whole frame cannot tell one row from another:

```rust
    /// The table row for `name`, or `None` if the table does not draw one.
    ///
    /// The selection marker (`>`) sits on the row itself, in its own
    /// one-character gutter column ahead of the id, so a marked row's tokens
    /// are shifted one place right of an unmarked row's: `nth(1)` is the id,
    /// not the name. Stripping the marker first keeps both cases on the same
    /// token index, and the marker never appears anywhere else on a line, so
    /// `trim_start_matches` cannot eat anything else.
    fn row_for<'a>(frame: &'a str, name: &str) -> Option<&'a str> {
        frame.lines().find(|line| {
            line.trim_start_matches('>').split_whitespace().nth(1) == Some(name)
        })
    }

    /// Whether the MARKED row's name starts with `prefix`. For a name the
    /// NAME column has truncated, the exact truncated string depends on
    /// terminal width, so a literal expected value would be wrong at any
    /// width this test was not written against. `prefix` only needs to fit
    /// inside the eight-column floor `name_width` never shrinks below to be
    /// safe here.
    fn marked_row_name_starts_with(frame: &str, prefix: &str) -> bool {
        frame.lines().any(|line| {
            line.starts_with('>')
                && line
                    .trim_start_matches('>')
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|name| name.starts_with(prefix))
        })
    }
```

```rust
        let confirm = render_text(&scene(Scene::Confirm).1);
        assert!(confirm.contains("restart api (id 2)? enter confirms, any other key cancels"));
        assert!(confirm.contains("control enabled"), "the gate is open");
        assert!(
            row_for(&confirm, "api").is_some_and(|row| row.contains("online")),
            "nothing was sent, so api is still online: {confirm:?}"
        );

        let acting = render_text(&scene(Scene::Acting).1);
        assert!(acting.contains("restart api (id 2): sent, waiting for the shepherd"));
        assert!(
            row_for(&acting, "api").is_some_and(|row| row.starts_with('>')),
            "the table is untouched: the marker is still on api"
        );
        assert!(
            row_for(&acting, "api").is_some_and(|row| row.contains("online")),
            "and the row still says what the shepherd last said"
        );

        let accepted = render_text(&scene(Scene::ActionAccepted).1);
        assert!(accepted.contains("restart api (id 2): the shepherd restarted it"));
        assert!(
            row_for(&accepted, "api").is_some_and(|row| row.contains("48299")),
            "the reply's own row reached the table without waiting for a poll"
        );

        let refused = render_text(&scene(Scene::ActionRefused).1);
        assert!(refused.contains("restart api (id 2): selector matched no registered sheep"));
        assert!(!refused.contains("NotFound"), "no Rust identifiers on the bar");
        assert!(refused.contains("5 in the flock"), "one row shorter");
        assert!(row_for(&refused, "api").is_none(), "api is the row that went");
        assert!(
            marked_row_name_starts_with(&refused, "billing"),
            "and the cursor has moved to the row below: {refused:?}"
        );

        let offline = render_text(&scene(Scene::ActionRefusedOffline).1);
        assert!(offline.contains("reconnecting (attempt 3)"), "the banner");
        assert!(offline.contains("nothing left to ask"), "and the refusal under it");

        // The one frame in the gallery whose left slot is empty while the gate
        // is open, which makes it the only one that shows the control hint.
        let lambs_bar = render_text(&scene(Scene::Lambs).1);
        for key in ["x stop", "R restart", "L reload"] {
            assert!(lambs_bar.contains(key), "the control hint names {key}");
        }
```

`restarted_api()` is the row `ActionAccepted`'s reply carries: `api` at id 2,
`ProcStatus::Online`, **pid 48299**, restarts 2, uptime near zero. A different
pid from the listing's 48219 is what makes "the reply's own row reached the
table" falsifiable; the same pid would pass whether the rows were upserted or
ignored.

`flock_without_api()` is the default six-sheep flock (the one `scene_with`
already builds for every scene besides `Empty`, `Errored` and `Frozen`) with
id 2 removed: five rows, ids 0, 1, 3, 4, 5.

`assert_eq!(Scene::ALL.len(), 19)` becomes `24`.

### Step 9.5 - snapshots, gallery, docs

The `lambs` snapshot changes (its control state and therefore its hint), the
five new ones appear, and nothing else should move: exactly one changed file
and five new ones. Read the diff and check that.

```bash
cargo test -p shep --lib --all-features -- lookout::frames
cargo insta review
git diff --stat crates/shep-cli/src/lookout/snapshots/   # 1 changed, 5 new
cargo test -p shep --lib --all-features -- --ignored write_the_gallery; echo "EXIT=$?"   # 0
grep -c '^=== ' docs/lookout/frames.txt   # 24
```

The frame count becomes twenty-four in all four places Step 3.6 lists: twice in
`GALLERY_PREAMBLE` and twice more in `frames.rs` (`Scene::caption`'s doc and
the `too_many_lines` allow). Run that step's grep to check none was missed.

- `docs/specs/deferred.md`: delete the **lookout actions** entry. That is the
  third and last of this phase's three, and deleting it makes one more line in
  that file false: the build-queue item at `deferred.md:28`, "**The rest of the
  v1.0 surface** - lookout, serve, dev/runtime, ...", which lists lookout as
  unbuilt surface. Take lookout off that list. Nothing else in the queue moves.
- `docs/lookout/README.md`: delete the **Actions behind the gate** bullet.
  That empties "What is still open" of all three bullets, so **the heading goes
  too** rather than standing over nothing; replace the section with one
  sentence saying lookout ships complete and pointing at
  `docs/specs/deferred.md` for the workspace's remaining debt. Then rewrite the
  gate bullet under "What 12a settled" (`README.md:45`), which today says
  exactly one action key exists and that it never acts. Keep the sentence
  saying the gate is a fat-finger catch and not a security boundary. Add the
  confirmation model in one line: an action key arms, Enter confirms, any other
  key cancels, `q` and Ctrl-C still quit, and an armed prompt expires after ten
  seconds.

### Step 9.6 - verify: the full task gate

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

### Step 9.7 - MUTATION

Store the in-flight line as a `Notice`: in `on_action_reply`'s caller path,
have `confirm` set `self.notice` and clear `self.action` instead of setting
`Stage::Sent`.

**Must redden `an_in_flight_line_survives_a_keypress`** (Task 7) and
`a_second_action_refuses_while_one_is_in_flight`, and the `acting` snapshot
keeps its line only until the next key. Revert.

### Step 9.8 - second MUTATION

Advertise the action keys in both control states: make `hint_for` return the
`Allowed` text unconditionally.

**Must redden `the_action_keys_are_advertised_only_when_the_gate_is_open`** and
every read-only snapshot in the gallery. Revert.

---

# Task 10 - the docs sweep and the phase gate

No behaviour. This is the task that stops the tree describing a lookout that no
longer exists.

### Step 10.1 - the sweep

```bash
grep -rn 'stop is not built yet' crates/ web/ docs/specs/ docs/lookout/ | wc -l   # 2 before, 0 after
grep -rn 'search/filter' docs/specs/deferred.md docs/lookout/README.md | wc -l    # 0 by now
grep -c 'cargo test -p shep-cli --bins' docs/lookout/frames.txt                   # 0 by now
```

**Two, not one.** Measured at HEAD, that phrase is in five files: `cli.rs`,
`app.rs`, `docs/lookout/README.md`, `docs/specs/deferred.md`, and
`web/src/data/cli-reference.generated.txt`. Tasks 7 and 9 delete three of them
(the `app.rs` literal, the README bullet, the deferred entry), leaving `cli.rs`
and the generated file. Both are Step 10.3's, and they are one edit rather than
two: the generated file is `cli.rs`'s own `--help` rendered, so fixing the doc
and regenerating clears both. Do not go looking for a fifth site.

The three feature tasks each deleted their own `deferred.md` entry and their own
README bullet, so those two greps should already be zero. If they are not, the
task that owned them did not finish, and this is where that is caught rather
than at review.

What is left:

1. **`crates/shep-cli/src/lib.rs`'s module doc.** It currently says the
   dashboard's "search/filter and its actions are what remains", names `x`
   (stop) as the only action key, and says it refuses either way. All of that
   is now false. Rewrite it to say lookout ships complete, and keep the
   pointer to `docs/specs/deferred.md`.
2. **`crates/shep-cli/src/lookout/mod.rs`'s module doc**, which describes the
   panes and the two tasks. Add the filter and the request channel in a
   sentence each; the `select!`'s arm-retirement paragraph is still accurate
   and this phase deliberately did not touch that loop's arms, so leave it.
3. **`crates/shep-cli/src/cli.rs`'s `--allow-control` doc.** It says "This
   phase wires the gate but not the actions" and quotes `stop is not built
   yet`. Replace with what the flag now does: opens the gate for stop, restart
   and reload, each of which arms a confirm rather than acting. Keep the
   fat-finger sentence and the `shep set lookout.allow_control true` pointer.
4. **`CLAUDE.md`'s status paragraph**, which ends at Phase 12b and lists the
   feed, the detail pane and the host strip as 12b with the filter and actions
   open. Add Phase 16.

### Step 10.2 - the check that no wire changed

This needs a `BASE_SHA`, captured **before Task 1 touches anything** and
carried through the phase:

```bash
BASE_SHA=$(git rev-parse HEAD)   # run this once, before Task 1
```

```bash
git diff --stat "$BASE_SHA" -- crates/shep-core/ crates/shep-client/src/ | wc -l   # 0
git diff --stat "$BASE_SHA" -- crates/shep-core/tests/ | wc -l                     # 0
grep -rn 'PROTOCOL_VERSION' crates/shep-core/src/protocol/ | head -3               # unchanged
```

Not `git diff main`: the phase branch is cut from `main`, so if the work is
merged locally before this runs, or if it is run on `main` itself, that diff is
trivially empty and proves nothing. It is the "expectation already true at
HEAD" shape from the baselines section, arriving inside the check meant to
catch exactly that class. `$BASE_SHA` is a real fixed point and the diff
against it can genuinely fail.

The second line is not redundant with the first: `crates/shep-core/tests/` is
where the stability fixtures live, and a new or changed one is precisely what a
wire change would produce. The first line does not cover it.

The first two are the real ones and both can fail: any edit under `shep-core`,
`shep-core/tests/` or the client's `src/` prints a line. This phase's whole
claim is that it needed no wire change, and these are the only checks that test
it rather than repeating it.

### Step 10.3 - the generated CLI reference

`cli.rs`'s flag doc is rendered into `web/src/data/cli-reference.generated.txt`
by the real binary's `--help`, so editing the doc without regenerating
publishes the old sentence on the website:

```bash
cargo build --release
./web/scripts/generate-cli-reference.sh
git diff --stat web/src/data/cli-reference.generated.txt   # exactly one file, the allow-control block
grep -c 'stop is not built yet' web/src/data/cli-reference.generated.txt   # 1 before, 0 after
```

### Step 10.4 - the phase gate

Each from its own command, `$?` read directly:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features -- --test-threads=1
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The serial run is not ceremony: it was red on `main` before Phase 5 and it
caught a real regression in Phase 6. The two cross-checks are per-phase, not
per-task; give them their own `CARGO_TARGET_DIR` if you want the host cache
left alone.

Nothing in this phase touches shep-daemon, and lookout is already
`#[cfg(unix)]`, so the Windows check is asking its usual question: does the
tree still compile for a target nobody has implemented yet.

### Step 10.5 - the last look, at the thing the maintainer actually reads

```bash
less -R docs/lookout/frames.ansi
```

Twenty-four frames. **Read every caption against the frame beneath it.** A
caption clause that cannot be pointed at in the frame is a bug in the caption,
and this project has shipped that twice: 12a shipped two false captions and
needed a fix commit, and 12b shipped a caption describing a state its frame did
not show.

---

## The frames, and what each one proves

Ten new, twelve changed. The maintainer decides what this looks like from these, not
from a spec sentence.

Twelve and not fourteen. **`too_narrow` and `narrow` do not change at all in
this phase**, and knowing which two is what makes a wrong-sized diff visible at
each of the three gate points:

- `too_narrow` is 28 columns, below `MIN_TERM_WIDTH`, so `draw` returns two
  short lines: no status bar to gain `/ filter`, no detail pane to gain a row.
- `narrow` is 51x14. Its bar truncates the hint at 41 characters and the old
  and new hints share their first 40, so Task 3 does not move it; 14 rows is
  the host-only tier, so Task 6 does not either.

The other twelve split into Task 3's ten (the hint), Task 6's ten (the detail
row) and an overlap of eight. `refused` and `cramped` are in Task 6's ten but
not Task 3's; `no_detail` and `table_only` are in Task 3's but not Task 6's.

| Frame | Size | What it proves |
|---|---|---|
| `filter_editing` | 100x14 | The table narrows while the query is still being typed; the title carries both numbers; the bar shows the query, a cursor, and the three keys that mean anything while the box is open. |
| `filter_active` | 100x14 | Applied and closed. The table is still narrowed and the bar has changed to `/ edit` and `esc clear`, which is what makes `esc` meaning two things acceptable. |
| `filter_no_match` | 100x14 | Zero matches names the query instead of claiming the flock is empty, and the title keeps the flock's real size on screen. |
| `lambs` | 120x30 | The detail pane's fifth line: the count, the age stamp before the list, and each lamb's pid and name. Also the only frame whose bar shows the control-enabled key hint. |
| `lambs_unknown` | 120x30 | A sheep with no pid. "not walked" and "walked and found none" are different sentences, which is the distinction the wire type was built to keep. |
| `confirm` | 100x14 | An action key armed and nothing sent. The question names the verb and the exact sheep, and `api` is still online in the table behind it. |
| `acting` | 100x14 | Enter pressed. The request is out and the table has not moved: same marker, same row, same status. |
| `action_accepted` | 100x14 | The shepherd did it. The outcome sentence in the non-grave style, and the reply's own row in the table, with a pid the listing never carried. |
| `action_refused` | 100x14 | The shepherd's own sentence forwarded, with no Rust identifier in it, over a listing that has lost the sheep. |
| `action_refused_offline` | 100x14 | An action key while the link is coming back. The banner and the refusal on one frame, which is what makes the question in the next section answerable. |
| `refused` (changed) | 120x30 | The read-only refusal, with a caption that no longer claims there are two refusals. |
| the other eleven (changed) | as before | One extra detail-pane row and the filter key in the hint, and nothing else. |
| `too_narrow`, `narrow` (unchanged) | as before | Below `MIN_TERM_WIDTH`, and truncating the hint to a shared 40-character prefix either way, respectively - neither gains a detail row or a filter key, so this phase moves neither. |

## One thing worth looking at in the frames, and why it is not being changed here

Arming an action while the link is `Retrying` refuses with the sentence `r`
gives when the link is **gone**, while the banner one row above says the
shepherd is being reconnected to. Two rows of the same frame then describe the
connection differently. That is A9's literal wording and the maintainer accepted it, so
this plan implements it as written rather than relitigating it.

**`action_refused_offline` is that frame**, added in Task 9 for this paragraph
and no other reason. The first draft of this plan said "if it reads wrong in
the gallery, the fix is one new sentence" while rendering no such frame, which
made the sentence unanswerable. It is answerable now: open `frames.ansi`, find
`action_refused_offline`, and read the banner against the line under it. If it
reads wrong, the fix is one new sentence for the `Retrying` case in `arm`'s
refusal ladder and nothing else moves.

## Commits

One commit per item, conventional style, as the punch-list rule requires. The
ten tasks map cleanly onto ten commits; three of them carry a doc edit for the
feature they finish, which is the point.

Four commit messages have to say something in particular. Three of them are
about a shipped test losing or changing its subject, which is the one thing in
this phase a reviewer should stop on if it arrives unannounced:

- **Task 3** fixes a documented command that silently did nothing. Say that,
  because anyone who ran it believed they had regenerated the gallery.
- **Task 5** changes three assertions in the shipped
  `a_selection_that_moves_refreshes_the_feed_and_one_that_cannot_does_not` from
  `Effect::RefreshFeed` to `Effect::RefreshSelected`, and one in Task 2's `esc`
  test. Say that a shipped test's expectation was updated and why: the effect
  now covers the lamb fetch as well as the feed read, and the assertions that
  carry the test's real claim, the two `Effect::None`s, did not move.
- **Task 6** deletes `the_detail_pane_never_mentions_lambs`. Say so, with A19's
  reason: the feature is the thing that test was written to prevent, and its
  replacement proves the pane says the right one of five things rather than
  proving it says nothing.
- **Task 7** takes half the subject away from
  `the_stop_key_refuses_in_both_control_states`: behind an open gate, `x` now
  arms instead of refusing. Say that the control-enabled half moved into
  `an_action_key_arms_a_confirm_and_sends_nothing` rather than being deleted.

## Three deviations from the approved design, in one place

The maintainer approved twenty numbered assumptions and this plan implements them. Three
places depart, each because implementing the assumption literally would ship a
defect. They are named in "Shapes the design named" at the top with their
reasoning; they are collected here so a reviewer does not have to find them.
**Each can be rejected on its own, and rejecting one costs one task, not the
phase.**

| # | assumption | what this plan does instead | why |
|---|---|---|---|
| 1 | **A4** | The filter box being typed into gets its own bar slot, above notices. A4's accepted cost, that a notice can cover the filter, applies to the applied-filter line only. | A4's stated reason ("while editing, every keypress is text, so nothing can raise a notice") is false: `BusLagged`, `Dropped` and `DaemonShutdown` raise notices with no keypress, and `on_text_key` never clears them, so the query would go off screen mid-word and stay off. |
| 2 | **A10** | `q` and `Ctrl-C` are not consumed by the confirm's cancel; they quit. | `input.rs`'s shipped doctrine is that the most reflexive way out of a terminal program must keep working, and quitting discards the confirm rather than acting on it. Text mode already carves out the same key. |
| 3 | **A9** | An armed confirm is cleared when the link stops being `Live`, rather than surviving to be refused at Enter. | A9 refuses at ARM time so an operator never answers a question that was never going to be honoured. Leaving the prompt up inverts that, and on a frozen dashboard it would never expire, because the expiry rides a clock that has stopped. |

None of the three changes the action set, the keys, the sentences or the wire.
If the maintainer rejects one, the plan reverts to the design's literal reading for that
one item and the corresponding test comes out with it.
