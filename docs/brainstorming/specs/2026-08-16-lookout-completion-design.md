# `shep lookout`: the three unbuilt pieces

Status: design, not a plan. Nothing here is built.
Date: 2026-08-16.
Mode: delegate. Every judgement call is mine; the Assumptions section at the
bottom is the maintainer's checkpoint, and each one can be rejected on its own without
unpicking the rest.

`docs/specs/deferred.md` records three open lookout items, and
`docs/lookout/README.md` closes with the same three under "What is still
open":

1. **Search/filter**, carrying an unresolved question from 12a: the CLI's
   selector grammar, or plain substring matching.
2. **Actions behind the gate.** The gate exists and refuses honestly; `x`
   is the only action key and it has never acted.
3. **Lambs in the detail pane**, blocked on `Describe`'s process-table walk.

This document settles all three. Together they are one phase.

Three research passes fed this. Where a pass asserted something that decides
a design choice, I read the code myself; the results, including two
corrections, are in "What I checked" below.

---

## What I checked

Every claim below was read out of this repo. I did not open
`~/GitHub/pm2`, and I edited nothing.

**The selector grammar is exact, and it misparses a half-typed regex.**
`crates/shep-core/src/selector.rs:79` matches names with `want == name` and
`:81` matches folds with `fold == Some(want.as_str())`. Both are full string
equality. The regex branch (`:44`) requires `input.len() >= 2 &&
starts_with('/') && ends_with('/')`; a half-typed `/web` fails that, falls
through every other branch, and lands in `Name("/web".to_string())` at `:52`.
So a live-typed `/web` would not error, it would silently search for a sheep
literally named `/web` and show an empty table with no reason given. That is
the fact that decides feature 1, and it is confirmed. A bare `/` is
`Name("/")` for the same reason, which the research did not mention.

**`ListFlock` really does not walk the process table, and the daemon says
why in its own doc.** `crates/shep-daemon/src/rpc.rs`: the `ListFlock` arm
calls `with_live_stats` only; the `Describe` arm calls `with_live_stats` and
then `with_lambs`. `with_lambs`'s doc states it is applied to `Describe` and
nothing else, because the walk is a second pass over every process on the
machine and a flock listing is what an operator leaves running in a loop.
`with_lambs` also short-circuits when every row has no pid, and leaves a
pid-less row's `lambs` as `None` rather than `Some(vec![])`, which is the
"not walked" case the field's doc distinguishes from "walked and empty".
Confirmed, three times over.

**Correction 1: the lamb walk is not simply "more expensive than the memory
sampler".** `limits/sample.rs`'s `identify()` builds a fresh `System` per
call, which is the point of it (a retained table caches a process name from
before an `execve` and never revises it, measured against
`/bin/sh -c 'sleep 0.6; exec sleep 5'`). But it refreshes with
`ProcessRefreshKind::nothing()`: no memory, no CPU. The 5.77 ms per 883
processes figure quoted at `with_live_stats` is for the memory-and-CPU walk,
not for this one. So `identify()` is a full enumeration with a per-process
read that is cheaper than the sampler's, plus the cost of building and
dropping a `System`. The research overstated the ratio. The conclusion does
not change: it is still a full machine enumeration, and its own doc says it
exists for "an operator's question, not a poll".

**Correction 2: the CLI does not distinguish "not walked" from "walked and
empty" in its table output.** `crates/shep-cli/src/output/mod.rs`'s
`emit_described` skips the lamb caption entirely for `None` *and* for an
empty vector, so both print nothing. The research said the pane should reuse
the shipped caption verbatim and preserve the trichotomy. It cannot do both:
there is no shipped wording for two of the three states. The pane has to say
its own sentences, and this document writes them.

**The reducer's cursor is an id, not an index**, and `rows()`,
`select_by`, `select_at`, `reseat` and `selected_index` all walk
`self.flock` (a `BTreeMap<u32, Row>`) directly. Confirmed at
`crates/shep-cli/src/lookout/app.rs:489-560`. There is no notion of a
visible subset anywhere.

**`input::map_key` is a pure function of the crossterm event**, with a
closed 7-variant `KeyPress`, and `_ => None` for every unbound key. `Esc` is
`Quit`. `/` is free. Confirmed at `input.rs:26-49`.

**The status bar has one left slot**, holding a notice if there is one and
the key hint otherwise, right-aligned against the control label with a one
column gap pinned at exactly 49 columns
(`view/status.rs`, `a_truncated_hint_still_leaves_a_gap_before_the_control_label`).
The title prints `{count} in the flock` from `app.rows().len()`.

**The layout ladder is real and tight.** `view/mod.rs`: `CHROME_ROWS = 4`,
`HOST_ROWS = 1`, `DETAIL_ROWS = 4`, `FEED_ROWS = 7`, tiers at 24/18/14/6,
and `every_pane_tier_fits_the_height_it_claims` checks chrome plus a banner
plus the panes against each threshold. The banner is already an optional
chrome row that costs the table a body row, which is the precedent for
anything new that wants a row.

**The replies to `Stop`, `Restart` and `Reload` carry `Vec<ProcessInfo>`.**
`protocol/request.rs:777-794`. `Reloading`'s doc is explicit that it is an
acceptance and not a result. This is what lets feature 2 avoid inventing a
client-side "action sent" row state; see below.

**Whistle's control surface is exactly four tools**, `start_sheep`,
`stop_sheep`, `restart_sheep`, `reload_sheep`, all behind
`[whistle] allow_control`, pinned by a hand-written table in
`whistle/catalogue.rs` that asserts nine tools total. No `delete`, no
`scale`, no `signal`, no `sendline`. Confirmed. This is the precedent
feature 2 leans on.

**The CLI has no confirmation prompt anywhere.** `grep -niE
'confirm|are you sure|--yes' crates/shep-cli/src/cli.rs` returns nothing.
`shep delete` just deletes. Confirmed, and it matters: lookout adding a
confirmation is not the TUI catching up to the CLI, it is compensating for a
hazard the CLI does not have.

**`--allow-control` exists on the `lookout` subcommand** and beats
`lookout.allow_control` in the KV store (`cli.rs:789`).

---

## The shape of all three, in one paragraph

Nothing here touches the wire. The filter is entirely client side. The
actions use `Request::Stop` / `Restart` / `Reload`, which exist. The lambs
use `Request::Describe`, which exists. `ProcessInfo`, `Request`, `Response`
and `BusEvent` are unchanged, so there are no stability fixtures to add and
no CHANGELOG entry for a wire change (IR-35, IR-45). What does change is
lookout's own reducer, its keymap, one trait method on `FlockSource`, one
extra channel into the link task, and one extra line in the detail pane.

---

# Feature 1: the filter

## What it is

A live substring filter over sheep **names**, typed into the status bar,
narrowing which rows the flock table draws and which rows `j`/`k` step over.

Matching is `name.to_lowercase().contains(&query.to_lowercase())`. That is
the whole rule. It is not the CLI's grammar, it is not regex, it does not
understand `fold:`, `all`, or ids, and it does not pretend to.

## What it deliberately is not

**Not `ProcessSelector::parse`.** The two variants an operator would type
most while narrowing are exact-match: typing `w`, `we`, `web` toward a sheep
named `web-worker` matches nothing at every step and then still matches
nothing, because `Name` compares with `==`. `fold:back` never narrows toward
`fold:backend`. And a half-typed `/web` does not refuse, it silently becomes
a search for a sheep named `/web`, which puts an empty table on screen with
no sentence saying why. This project prints `-` rather than a confident zero
and says "the shepherd did not report a path" rather than leaving a bare
dash; a filter that goes quietly blank is the same defect. The grammar is
right for `shep restart web` and wrong for a box you type into one character
at a time.

**Not a hybrid.** The third option the research floated (substring by
default, selector semantics whenever the whole query happens to parse) would
make lookout's `fold:` mean prefix-ish narrowing while the CLI's `fold:`
means exact equality. Two things spelled the same and behaving differently
is worse than one thing that never claims to be the other.

**Not a search over the bleats feed.** Different feature, different pane,
different data source. Cut.

**Not a fold filter.** See assumption A2.

## Keys and modes

Two input modes. `app::InputMode` is `Normal` or `Text`, and
`input::map_key` becomes `map_key(event: &Event, mode: InputMode)`. This
keeps the crossterm edge in one file, which is that file's stated job, and
keeps `app.rs` free of terminal types.

Normal mode, unchanged except for one addition:

| key | meaning |
|---|---|
| `/` | start editing the filter, carrying the current query if there is one |

Text mode (editing the filter):

| key | meaning |
|---|---|
| printable char | append to the query |
| `Backspace` | remove the last char |
| `Enter` | apply and leave editing |
| `Esc` | clear the filter and leave editing |
| `Ctrl-C` | quit |
| everything else | nothing |

While editing, `q` types a `q`. The status bar says so, which is the
condition for the overload being acceptable at all.

Applied but not editing:

| key | meaning |
|---|---|
| `Esc` | clear the filter |
| `q`, `Ctrl-C` | quit |

`Esc` not quitting while a filter is set is the one key whose meaning
depends on state. It is acceptable because the screen always says what it
does at that moment (the status bar reads `esc clear` while filtered) and
because the title carries `3 of 6 in the flock` for as long as the filter is
on, so the state is never invisible. `q` quits from every non-editing state,
unchanged.

An empty query applied with `Enter` is the same as no filter.

## Where it lives on screen

**The status bar's left slot, not a new row.** The slot's priority becomes,
highest first: a pending action confirm, a notice, the filter, the key hint.

The reason it is not its own row is arithmetic. The layout ladder has room
for one more chrome row at every tier above the floor, but not at the floor:
at `MIN_HEIGHT` (6 rows), chrome plus a banner already leaves the table
exactly one body row, and a filter row would take it, leaving a table with
no rows and no room to print a sentence saying why. Refusing `/` below a
height threshold would need the reducer to learn the terminal size, which
`Msg::Resize` currently discards. The status bar costs nothing and needs
none of that. The permanent signal that a filter is on lives in the title,
which cannot be crowded out.

A notice can transiently cover the filter line, and only while a filter is
applied rather than being edited (while editing, every keypress is text, so
nothing can raise a notice). The title still says `3 of 6`, and the next
keypress clears the notice. That is the whole cost of the choice, stated so
it can be rejected: A4.

## The title, and the count that would otherwise lie

`title_line` prints `{app.rows().len()} in the flock`. Once `rows()` returns
the filtered set, that number understates the flock, which is exactly the
kind of confident wrong number the `-` CPU column and the frozen uptime rule
exist to prevent. While a filter is set the title reads:

```
shep lookout   /home/ada/.shep                                    3 of 6 in the flock
```

and with no filter it is unchanged.

## Selection under a filter

This is the load-bearing change, and it is a refactor rather than an
addition. Today `rows()`, `select_at`, `select_by`, `reseat` and
`selected_index` each walk `self.flock` directly. They all move onto one
private helper:

```rust
/// The ids the table draws, in id order: the whole flock, or whatever the
/// filter leaves of it.
fn visible_ids(&self) -> impl Iterator<Item = u32> + '_
```

Every one of the five reads that sequence and nothing else. A filter that
hides rows `j`/`k` still step through is the failure mode this exists to
prevent, and it is the one thing most likely to be under-scoped.

When a keystroke narrows the filter so that the selected sheep is no longer
visible, the selection falls to whatever now occupies the same position,
clamped to the last visible row. That is `reseat`'s existing rule, applied
to a new cause, and it is the right one for the same reason: snapping to row
0 throws an operator to the top of a two hundred sheep flock for typing one
more character.

When nothing matches, the selection is `None`, and the three panes below say
three different sentences, because there are three different reasons, which
is the `empty` scene's own stated principle:

- table body: `no sheep's name contains "zzz"`
- detail pane: `no sheep selected: no name contains "zzz"`
- feed header: `bleats  no sheep is selected` (unchanged, it is already true)

Note what the table body must *not* say: `the flock is empty`. The flock is
not empty. That sentence stays for the case it describes.

## Sketches

Hand-drawn, at 100 columns, trailing spaces trimmed, column widths
indicative. These are not rendered frames. The real ones come from
`write_the_gallery` once this is built, and this document should be
considered superseded by them the moment they exist.

```
=== filter_editing  (100x14) ===
Mid-type. The table has already narrowed, the title counts both numbers, and the
status bar carries the query, a cursor, and the only two keys that now mean anything.

shep lookout   /home/ada/.shep                                    2 of 6 in the flock
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu 6.3%   fl…
  ID    NAME                     STATUS           PID      RESTARTS  CPU     MEM
──────────────────────────────────────────────────────────────────────────────────
> 0     web                      online           48211    0         3.4%    182.0M
  1     web                      online           48212    0         2.9%    178.0M




filter  web▏   enter applies   esc cancels   ctrl-c quits                  read-only
```

```
=== filter_active  (100x14) ===
Applied and no longer editing. The keys are back to normal, except esc, and the bar
says which two keys touch the filter.

shep lookout   /home/ada/.shep                                    2 of 6 in the flock
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu 6.3%   fl…
  ID    NAME                     STATUS           PID      RESTARTS  CPU     MEM
──────────────────────────────────────────────────────────────────────────────────
> 0     web                      online           48211    0         3.4%    182.0M
  1     web                      online           48212    0         2.9%    178.0M




filter "web"   / edit   esc clear                                          read-only
```

```
=== filter_no_match  (100x14) ===
Nothing matches. The table does not say the flock is empty, because it is not, and
the count keeps the real size on screen.

shep lookout   /home/ada/.shep                                    0 of 6 in the flock
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu -   flock…
  ID    NAME                     STATUS           PID      RESTARTS  CPU     MEM
──────────────────────────────────────────────────────────────────────────────────
no sheep's name contains "zzz"





filter  zzz▏   enter applies   esc cancels   ctrl-c quits                  read-only
```

---

# Feature 2: actions behind the gate

This is the largest of the three, and the only one where being wrong costs
an operator a running process.

## The action set

**Stop, restart, reload. Nothing else.**

Delete, scale, signal and whisper stay CLI only. This is not a fresh call:
it is the boundary whistle already drew for its own non-CLI control surface,
and the reasons transfer exactly. Those four take a parameter that a
dashboard has nowhere to put (a count, a signal name, a line of stdin) or
they remove an app from the registry, which is the one action no keypress
should be one Enter away from. Keeping lookout's surface identical to
whistle's means shep has one answer to "what can a non-CLI surface do",
rather than two independently drifting ones.

The gate is unchanged: `--allow-control` or `lookout.allow_control`, default
closed, refusing with the sentence it already refuses with. It remains a
fat-finger catch and not a security boundary, and the README already says
so.

**On `start`, raised and settled 2026-08-16.** Whistle has four control tools,
not three: `start_sheep`, narrowed to already-registered sheep. So lookout's
surface is a subset of whistle's rather than identical to it, and the case for
adding `start` is the strongest of the four left out. It needs no parameter, it
is not destructive, and it is already considered safe enough for an agent to
invoke unattended. The cost of leaving it out is that an operator watching a
stopped sheep has to drop to a shell for the one verb that would fix it.

The maintainer's call: ship the three. Recorded here because the next reader will ask the
same question, and because the answer is a scope decision rather than a
technical obstacle. Adding `start` later is additive: one key, one arm-time
refusal for a sheep that is already running, and no new shape anywhere.

## Keys

| key | verb | note |
|---|---|---|
| `x` | stop | already bound, already in the frames and in `every_bound_key_resolves_to_its_press` |
| `R` | restart | shift, because `r` is refresh |
| `L` | reload | shift, for symmetry with `R` |

`x` keeps its binding rather than moving to `s`. It is in every gallery
frame, in the input test, and in the README's account of 12a. Renaming it
would cost more than the mnemonic gains.

Action keys are advertised in the key hint **only while `Control::Allowed`**.
`view/status.rs` already made this exact call for `x`: a hint that needs a
footnote to be true is an asterisk, not a hint. So the hint has two forms:

- read-only: `q quit   j/k select   g/G first/last   r refresh   / filter`
- control enabled: `q quit   j/k select   / filter   x stop   R restart   L reload`

Both truncate on a narrow terminal through the existing `fit`, and the
49 column gap test still measures what it was written to measure, because
the read-only hint is still longer than the width at which it truncates.

## The confirmation model

**One tier, one shape, for all three verbs. Enter confirms; every other key
cancels.**

Pressing an action key never acts. It arms:

```rust
/// An action key pressed once, waiting for the operator's Enter.
///
/// The target is captured HERE, by id and by name, and never re-read from
/// the selection: a snapshot can land between the arming keypress and the
/// Enter, and a confirmation that re-read the cursor could act on a sheep
/// the operator never pointed at.
struct Armed {
    verb: ActionVerb,
    id: u32,
    name: String,
    at: Instant,
}
```

The status bar replaces its hint with a literal question naming the verb and
the exact sheep:

```
stop api (id 2)? enter confirms, any other key cancels
```

Styled `attention()`, the same butter the non-grave notice uses. Not a
modal, not a box, not a `ratatui::widgets::Clear`. There is no overlay
anywhere in `lookout/`, and the file that owns this line says why: one rule
under the header beats a full border for a pane someone reads at 3am.

### Why Enter, and why every other key cancels

The whole point of the gate is that a keystroke in a dashboard someone is
reading should not become an action. A confirm that the action key itself
could complete (press `x` twice) reintroduces exactly the double-tap the
gate exists to catch, on a keyboard that may be repeating. So the accept key
is `Enter`, it is nothing else, and pressing `x` again cancels like any
other key.

Cancelling is **silent**. No notice, no "cancelled" line: nothing happened,
and reporting nothing as if it were something trains an operator to ignore
the status bar. The hint simply comes back.

### The routing rule that makes this safe

**Every keypress is checked against the armed state before the ordinary
dispatch, and a cancelling keypress is consumed.** A stray `j` during a
pending confirm cancels it and does *not* also move the selection. If it did
both, the operator would see the prompt vanish and the cursor move, and the
next reflexive Enter would act on a target they had already lost track of.
This is one `if` at the top of `on_key`, and it needs its own test, because
getting it wrong is the failure mode this whole feature is about.

An immediate consequence: `/` cancels a pending confirm before entering
filter mode, so a confirm and a filter edit can never coexist. That closes
the whole interaction between features 1 and 2 by construction rather than
by rule.

### Expiry

An armed confirm expires after 10 seconds and returns the hint. It rides
the existing `Msg::Tick`, so no new timer and no sleep in the test (IR-33).
The reason is the reason for the whole feature: a prompt left armed while
the operator walks away, followed by an Enter typed at what they think is a
shell, is the same fat finger arriving by a slower route.

### What is refused, and when

Refusals happen at **arm time**, not at confirm time, so an operator never
answers a question that was never going to be honoured:

| condition | sentence | grave |
|---|---|---|
| gate closed | `read-only: actions need --allow-control` (existing text) | yes |
| link not `Live` | the same sentence `r` already gives when the link is gone | yes |
| nothing selected | `no sheep is selected` | yes |
| an action already in flight | `one action is already in flight` | yes |

One refusal happens at confirm time, and it must: if the pinned sheep left
the flock between the arming and the Enter, the request is not sent and the
bar says `stop api (id 2): it is no longer in the flock`. The same check
fires from the snapshot arm, so a prompt naming a sheep that no longer
exists is taken off the screen as soon as the reducer learns it is gone,
rather than sitting there as a question about nothing.

Refusing while the link is not `Live` is worth naming as a decision (A9). It
means an action typed during the eight second reconnect ladder is refused
rather than queued, which is the right way round: an action that lands four
seconds later, on a connection the operator has stopped watching, is worse
than one that says no.

## Between the Enter and the table catching up

**No client-side row state is invented.** This is the one place the research
proposed something I cut outright.

`Response::Stopped`, `Restarted` and `Reloading` all carry
`Vec<ProcessInfo>`: the shepherd's own rows, after the action. So the reply
is upserted into the flock map exactly the way a bus event already is, and
the table updates from the shepherd's words rather than from a guess. A
speculative `online · restart sent…` in the STATUS column would be lookout
asserting a state nothing had confirmed, in the one column whose whole job
is to be true, and it would have to negotiate with the narrow-terminal
column drop order for the privilege.

What the operator sees, in order:

1. `Enter`. Status bar: `stop api (id 2): sent, waiting for the shepherd`,
   `attention()`. This is **not** a notice: a notice is cleared by the next
   keypress, and an in-flight action whose only sign on screen could be
   wiped by a stray `j` is a dashboard hiding something it knows. It is its
   own state, and it outranks the hint and the filter line.
2. The reply lands. The returned rows are upserted. The in-flight state
   clears and a notice takes its place, which the next keypress clears as
   notices do.

The outcome sentences:

| verb | sentence |
|---|---|
| stop | `stop api (id 2): the shepherd stopped it` |
| restart | `restart api (id 2): the shepherd restarted it` |
| reload | `reload api (id 2): accepted, the swaps report themselves as they happen` |

Reload's wording is not decoration. `Response::Reloading`'s own doc says it
is an acceptance and not a result, and the swaps arrive afterwards on the
bus as `process.reload` / `process.reloaded` / `process.reload_abandoned`,
which the table already consumes. A sentence saying "reloaded" would be the
one lie this reply makes easy to tell.

**One action at a time.** A second action key while one is in flight refuses
with `one action is already in flight`. This is a real state the operator
can see rather than an invisible queue, and it keeps the in-flight line
unambiguous about which action it is about.

## Plumbing

The link task already has the shape this needs. `run_ui` owns a
`polls: Sender<()>` that `Effect::PollNow` feeds; `run_connected` selects on
it and calls `reconcile`. The action path is the same shape, one channel
over:

- `FlockSource` gains one method:

  ```rust
  /// Sends one request over this connection and returns the shepherd's
  /// answer, whatever it is.
  ///
  /// # Errors
  ///
  /// Whatever the underlying connection failed the request with.
  fn send(&self, request: Request)
      -> impl Future<Output = Result<Response, RequestError>> + Send;
  ```

  Unlike `flock()`, this does **not** swallow an unrecognised `Response`
  into an empty success. `Response` is `#[non_exhaustive]`, and a reply this
  binary does not understand becomes a grave notice saying exactly that
  rather than a silent success. IR-28: the `# Errors` section is required
  and is written above.

- `run_ui` gains a `requests: Sender<Sent>` beside `polls`, capacity 2 (one
  action plus one lamb fetch; see feature 3), sent with `try_send` for the
  same reason `polls` uses it.

- `Sent` is the echo tag, so a reply can be routed without a correlation id
  even when it is an `Err` that carries no shape of its own:

  ```rust
  enum Sent {
      Action { verb: ActionVerb, id: u32, name: String },
      Lambs { id: u32 },
  }
  ```

- `run_connected` gains one `select!` arm:

  ```rust
  Some(sent) = requests.recv() => {
      let result = flock.send(sent.request()).await;
      msgs.send(Msg::Replied { sent, result }).await.map_err(|_| UiGone)?;
  }
  ```

  Awaiting inline holds the other arms for the request's duration. That is
  already this loop's established behaviour: the poll arm awaits `reconcile`
  the same way, bounded by the client's own deadline.

- `run_connected` and `run_link` hand both receivers back up the ladder
  rather than one. A two field struct reads better than a tuple in three
  signatures and a `# Errors` doc.

- `Msg` gains `Replied { sent: Sent, result: Result<Response, RequestError> }`.

## Sketches

```
=== confirm  (100x14) ===
`R` pressed with the gate open. Nothing has been sent. Enter sends it; anything else,
including another `R`, takes the question away and moves nothing.

shep lookout   /home/ada/.shep                                          6 in the flock
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu 14.7%   fl…
  ID    NAME                     STATUS           PID      RESTARTS  CPU     MEM
──────────────────────────────────────────────────────────────────────────────────
  0     web                      online           48211    0         3.4%    182.0M
  1     web                      online           48212    0         2.9%    178.0M
> 2     api                      online           48219    1         7.1%    241.0M
  3     billing-reconciliation…  online           48230    0         0.8%    96.0M
  4     cron                     online           48233    0         0.1%    8.0M
  5     metrics                  online           48240    0         0.4%    11.0M

restart api (id 2)? enter confirms, any other key cancels            control enabled
```

```
=== acting  (100x14) ===
Enter pressed. The request is out and nothing on the table has changed yet, because
nothing the shepherd said has changed yet. This line survives a stray keypress.

shep lookout   /home/ada/.shep                                          6 in the flock
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu 14.7%   fl…
  ID    NAME                     STATUS           PID      RESTARTS  CPU     MEM
──────────────────────────────────────────────────────────────────────────────────
  0     web                      online           48211    0         3.4%    182.0M
  1     web                      online           48212    0         2.9%    178.0M
> 2     api                      online           48219    1         7.1%    241.0M
  3     billing-reconciliation…  online           48230    0         0.8%    96.0M
  4     cron                     online           48233    0         0.1%    8.0M
  5     metrics                  online           48240    0         0.4%    11.0M

restart api (id 2): sent, waiting for the shepherd                   control enabled
```

```
=== action_refused  (100x14) ===
The shepherd refused, in its own words. `selector matched no registered sheep` is the
daemon's sentence, forwarded rather than rewritten.

shep lookout   /home/ada/.shep                                          5 in the flock
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu 11.3%   fl…
  ID    NAME                     STATUS           PID      RESTARTS  CPU     MEM
──────────────────────────────────────────────────────────────────────────────────
  0     web                      online           48211    0         3.4%    182.0M
  1     web                      online           48212    0         2.9%    178.0M
> 3     billing-reconciliation…  online           48230    0         0.8%    96.0M
  4     cron                     online           48233    0         0.1%    8.0M
  5     metrics                  online           48240    0         0.4%    11.0M


restart api (id 2): selector matched no registered sheep             control enabled
```

---

# Feature 3: lambs in the detail pane

## What it is

One more line in the sheep detail pane, listing the selected sheep's
parent-pid descendants, fetched with `Request::Describe` **when the
selection changes and when `r` is pressed**, and never on the two second
poll.

`DETAIL_ROWS` goes from 4 to 5. The tier thresholds do not move: at 24 rows
the fixed cost becomes chrome 4, banner 1, host 1, detail 5, feed 7, which
is 18, leaving 6 for the table against the tier test's floor of 3. Every
other tier has more slack, not less.

## What it deliberately is not

**Not a `Describe` on the two second poll.** `identify()`'s own doc says it
exists for an operator's question and not a poll, and `with_lambs`'s doc
says `ListFlock` declines the walk precisely because a flock listing is what
runs in a loop. A dashboard putting a full machine enumeration on a fixed
2s clock, times however many lookout windows are open, is the daemon paying
the exact cost its own code was written to avoid, for the exact access
pattern that code names.

**Not a change to `ListFlock`.** Reversing the daemon-side split would make
`shep flock`, the dogs table, whistle's `list_flock` and lookout's own 2s
poll all pay the walk, and `deferred.md` already records that as the trigger
for a much larger `ProcessInfo` split. That is not a lookout decision.

## Triggers, and the coalescing that makes them safe

`Effect` grows one variant. Today `select_at` and the `Snapshot` arm both
return `RefreshFeed`; they need to be told apart, because a snapshot must
refresh the feed (paths can change) and must not fetch lambs.

- `Effect::RefreshFeed` keeps its meaning: the feed only. Returned by the
  `Snapshot` arm.
- `Effect::RefreshSelected` is new: the feed **and** the lambs. Returned by
  `select_at` when the selection actually moved, and by `reseat` for the
  same condition it already uses to decide between `RefreshFeed` and
  `None`.

No compound effect, no `Vec<Effect>`, no bitflags. Two variants and one flag
in `run_ui`, mirroring `feed_dirty` exactly:

```rust
if lambs_dirty && may_draw && view::panes_for(height).detail {
    let _ = requests.try_send(Sent::Lambs { id });
    lambs_dirty = false;
}
```

Three things fall out of putting it behind the same `may_draw` gate the feed
already uses, and they are the reason this is the right place for it:

1. **A held `j` down a two hundred sheep flock fires one request per redraw
   window, not two hundred.** Ordinary terminals deliver 20 to 30 press
   events a second, and each one moves the selection. Without the gate this
   feature would be exactly the fixed-clock walk it exists to avoid, only
   worse. `mod.rs` already argues this for the feed's 128 KiB read.
2. **`r` gets lambs for free.** `Effect::PollNow` sets `lambs_dirty` as well
   as `dirty`, so the one key an operator presses to say "tell me again"
   refreshes everything the pane shows.
3. **A terminal too short to draw the detail pane never pays.** `run_ui`
   knows the height; the reducer does not, and does not need to.

When the link is `Lost`, `select_at` already returns `Effect::None`, so a
frozen dashboard asks for nothing. That rule is inherited rather than
restated.

## What the pane says

The reading is stored as `Option<LambReading>` carrying the id it was taken
for, the `Instant` it landed, and the result. The line is derived from the
selected id and that reading, so a reading for a different sheep, or a
request dropped by a full channel, both read as "not read yet" without a
second field to track them.

The three states `ProcessInfo::lambs` distinguishes get three sentences,
because the CLI has no wording to borrow for two of them (see Correction 2):

| state | line |
|---|---|
| walked, non-empty | `lambs  3 parent-pid descendants, read 4m ago   48220 node   48221 node   48222 node` |
| walked, empty | `lambs  none found, read 12s ago` |
| not walked (`None`, meaning no pid) | `lambs  this sheep is not running, so there is no tree to walk` |
| no reading for this sheep yet | `lambs  not read yet` |
| the request failed | `lambs  the shepherd did not answer that request` |

The age stamp sits **before** the list, not after it. `detail.rs`'s own rule
is that the rarest field goes last so a narrow terminal truncates it first;
here that rule inverts, because the caveat is the thing that must survive.
A truncated list is still honest; a list with its "read 4m ago" truncated
away is a stale reading presented as current.

The stamp uses `human_duration` over `now - reading.at`, the same formatter
and the same `App::now` the uptime column uses. Which means it stops when
the dashboard freezes, exactly like the uptime column, for exactly the same
reason.

A failed lamb fetch sets no notice and steals no status bar. It is a
decoration on a pane, not an operator's action, and the pane already says
what it does not know.

## Cost

One `Describe` per selection change or `r` press, coalesced to at most one
per redraw window, and only while the detail pane is drawn. Each one costs
the daemon two process-table passes, because `Describe` calls
`with_live_stats` and then `with_lambs`; the first of those duplicates
figures `ListFlock` supplied seconds earlier. That is worth writing down
rather than hiding: it is the price of using the verb that exists instead of
adding a wire request, and it is bounded by a human's finger rather than a
timer.

## Sketch

```
=== lambs  (100x24) ===
The detail pane with a lamb list. The age stamp sits before the list so a narrow
terminal truncates lambs rather than the caveat.

shep lookout   /home/ada/.shep                                          6 in the flock
host  load 2.31 4.10 3.88 / 10 cores   host mem 12.4G / 32.0G   flock cpu 14.7%   fl…
  ID    NAME                     STATUS           PID      RESTARTS  CPU     MEM
──────────────────────────────────────────────────────────────────────────────────
  0     web                      online           48211    0         3.4%    182.0M
  1     web                      online           48212    0         2.9%    178.0M
> 2     api                      online           48219    1         7.1%    241.0M
  3     billing-reconciliation…  online           48230    0         0.8%    96.0M
  4     cron                     online           48233    0         0.1%    8.0M
  5     metrics                  online           48240    0         0.4%    11.0M

──────────────────────────────────────────────────────────────────────────────────
sheep 2  api   online   pid 48219   restarts 1   uptime 1h 28m   cpu 7.1%   mem 241…
lambs  3 parent-pid descendants, read 4m ago   48220 node   48221 node   48222 node
out  /home/ada/.shep/logs/api-2-out.log
err  /home/ada/.shep/logs/api-2-err.log
──────────────────────────────────────────────────────────────────────────────────
bleats  api
out  GET /healthz 200 3ms
out  GET /v1/orders 200 44ms
out  POST /v1/orders 201 88ms
out  GET /v1/orders/8821 200 9ms
out  connection pool: 14/50 in use
q quit   j/k select   / filter   x stop   R restart   L reload        control enabled
```

```
=== lambs_unknown  (100x24, detail pane only) ===
A stopped sheep. The daemon leaves `lambs` as None rather than as an empty list, and
the pane says which of the two it is looking at.

sheep 4  cron   stopped   pid -   restarts 0   uptime 1h 21m   cpu -   mem -   fold -
lambs  this sheep is not running, so there is no tree to walk
out  /home/ada/.shep/logs/cron-4-out.log
err  /home/ada/.shep/logs/cron-4-err.log
```

---

# Error and refusal behaviour, all in one place

Every sentence is literal. Nothing about damage gets charming, which is the
standing rule `view/status.rs` states about itself.

| situation | what the screen says | style |
|---|---|---|
| action key, gate closed | `read-only: actions need --allow-control` | grave |
| action key, link not live | the sentence `r` already gives when the shepherd is gone | grave |
| action key, nothing selected | `no sheep is selected` | grave |
| action key, one already in flight | `one action is already in flight` | grave |
| confirm cancelled | nothing at all | n/a |
| confirm expired | nothing at all | n/a |
| confirmed, target gone | `stop api (id 2): it is no longer in the flock` | grave |
| daemon refused | the daemon's own `RpcError::message`, prefixed with the verb and target | grave |
| connection died mid-request | `RequestError`'s own `Display` for `Closed` or `Timeout`, same prefix | grave |
| unrecognised reply | `restart api (id 2): the shepherd answered something this lookout does not understand` | grave |
| reload accepted | `reload api (id 2): accepted, the swaps report themselves as they happen` | attention |
| lamb fetch failed | `lambs  the shepherd did not answer that request`, in the pane, no notice | muted |
| filter matches nothing | three distinct sentences, one per pane, none of them `the flock is empty` | muted |

On the daemon's own words: forward `RpcError::message`, not
`RequestError`'s full `Display`. The latter interpolates the code with `{:?}`
and produces `the daemon reported NotFound: selector matched no registered
sheep`, which puts a Rust identifier on an operator's screen. The message
alone is already a sentence a human wrote.

---

# Testing

The bar here is the repo's own: rendered frames are pinned with insta, every
test names the mutation that reddens it, and nothing sleeps (IR-33, IR-34).
The reducer is a synchronous function of `&mut self` and one `Msg`, so
almost all of this is plain unit testing with no runtime and no terminal.

## Filter

| test | mutation that reddens it |
|---|---|
| `a_filter_narrows_the_table_and_the_title_counts_both_numbers` | make `title_line` read the unfiltered map |
| `the_filter_matches_a_substring_not_a_whole_name` | swap `contains` for `==`, which is precisely the Option B failure |
| `the_filter_ignores_case_in_both_directions` | drop either `to_lowercase` |
| `j_and_k_step_only_over_visible_rows` | point `select_by` back at `self.flock.keys()` |
| `a_filter_that_hides_the_selection_clamps_to_the_nearest_visible_row` | snap to row 0, or set the selection to `None` |
| `a_filter_matching_nothing_does_not_say_the_flock_is_empty` | reuse the empty-flock sentence |
| `typing_q_while_editing_types_a_letter` | make `map_key` ignore its mode argument |
| `esc_clears_the_filter_instead_of_quitting_while_one_is_set` | return `Effect::Quit` from that arm |
| `esc_still_quits_with_no_filter_set` | make the clear unconditional, which is the mirror bug |
| `an_empty_query_applied_is_the_same_as_no_filter` | keep an empty filter set and narrow to nothing |

Frames: `filter_editing`, `filter_active`, `filter_no_match`. A layout or
wording change reddens the snapshot; regenerating the gallery is one
command, and the gallery and the snapshots read the same `Scene::ALL`, so
they cannot drift.

## Actions

The ones that matter most are the four about the confirm, because they are
where a wrong process gets stopped.

| test | mutation that reddens it |
|---|---|
| `an_action_key_arms_a_confirm_and_sends_nothing` | send on the first press |
| `only_enter_confirms_and_every_other_key_cancels` (over `j`, `k`, `x`, `R`, `r`, `/`, `g`) | make the action key itself confirm, or make only `Esc` cancel |
| `a_cancelling_key_is_consumed_and_does_not_also_move_the_selection` | fall through to the ordinary dispatch after cancelling |
| `the_confirm_is_pinned_to_the_id_it_was_armed_on` | build the request from `self.selected` at Enter time; the test lands a snapshot that reseats the cursor in between |
| `a_confirm_whose_sheep_left_the_flock_refuses_instead_of_sending` | send anyway |
| `a_confirm_expires_after_ten_seconds_of_ticks` | never expire (driven by `Msg::Tick`, no sleep) |
| `every_action_key_refuses_while_the_gate_is_closed` | check the gate for `x` only |
| `every_action_key_refuses_while_the_link_is_not_live` | allow arming while `Retrying` |
| `a_second_action_refuses_while_one_is_in_flight` | drop the in-flight guard |
| `an_in_flight_line_survives_a_keypress` | store it as a `Notice` |
| `an_accepted_stop_upserts_the_rows_the_shepherd_returned` | ignore the reply's rows and wait for the poll |
| `a_reload_reply_does_not_claim_the_swap_finished` | word it as "reloaded" |
| `a_daemon_refusal_reaches_the_bar_in_the_daemons_own_words` | replace `err.message` with a canned string |
| `an_unrecognised_reply_says_so_rather_than_reading_as_success` | swallow it into `Ok`, which is what `flock()` does and what `send` must not |
| `an_action_reaches_the_shepherd_and_its_reply_comes_back` (link level, hand-rolled `FlockSource`, paused clock) | drop the reply, or answer the wrong request |

Frames: `confirm`, `acting`, `action_refused`. The `refused` scene stays as
it is; it pins the read-only refusal, which this feature does not change.

One existing test needs care rather than deletion:
`every_bound_key_resolves_to_its_press` in `input.rs` gains `R` and `L` and
gains a mode argument. Its doc already says why it exists, and that reason
gets stronger once `x` really acts.

## Lambs

| test | mutation that reddens it |
|---|---|
| `moving_the_selection_asks_for_lambs` | return `RefreshFeed` instead of `RefreshSelected` |
| `a_snapshot_refreshes_the_feed_and_does_not_ask_for_lambs` | return `RefreshSelected` from the `Snapshot` arm, which is the whole cost decision inverted |
| `a_held_key_coalesces_into_one_request_per_redraw_window` | send from the `Effect` arm rather than behind `may_draw` |
| `no_lambs_are_requested_when_the_detail_pane_is_not_drawn` | drop the `panes_for(height).detail` check |
| `nothing_is_requested_while_the_link_is_lost` | fetch anyway |
| `the_pane_says_which_of_the_three_lamb_states_it_is_in` | collapse `None` and the empty vector into one sentence, which is the exact distinction the wire type was built to keep |
| `the_lamb_line_carries_its_age_before_its_list` | move the stamp after the list, or drop it |
| `a_frozen_dashboard_does_not_age_its_lamb_reading` (render at two ages, assert the frames are identical) | read a live clock instead of `App::now` |
| `a_reading_for_another_sheep_reads_as_not_read_yet` | show it regardless of id |

Frames: `lambs`, `lambs_unknown`.

**One existing test must be deleted, not adjusted.**
`the_detail_pane_never_mentions_lambs` in `detail.rs` fails if the words
"lamb", "children" or "tree" appear in the rendered pane. It was correct
when it was written and this feature makes it wrong. Deleting a test is the
kind of thing a review should stop, so it is written down here as an
intended consequence rather than discovered in a diff. Its replacement is
`the_pane_says_which_of_the_three_lamb_states_it_is_in`, which is strictly
stronger: the old test proved the pane said nothing, the new one proves it
says the right one of three things.

## Layout

`every_pane_tier_fits_the_height_it_claims` picks up `DETAIL_ROWS = 5`
automatically and should stay green. If it does not, the tier table is
wrong, not the test. Mutation: set `DETAIL_ROWS` back to 4 while the pane
draws five lines, and the frames redden instead.

## Docs

Three documents assert the current state and become false as each feature
lands: `docs/specs/deferred.md`'s three entries, `docs/lookout/README.md`'s
"What is still open", and `docs/lookout/frames.txt` plus `frames.ansi`. The
frames are regenerated by one command and cannot drift, because the gallery
and the snapshots read the same scene list. The two prose documents can
drift and should be edited in the same commit as the feature they describe.

---

# Suggested order

Filter, then lambs, then actions. The filter forces the `visible_ids`
refactor and the two mode keymap, both of which the confirm state sits on
top of. Lambs forces the `Sent` channel and `FlockSource::send`, which the
actions then reuse for a request that matters. Actions land last, on
plumbing that has already been exercised twice by something that cannot stop
a process.

---

# What I cut, and why

- **Delete, scale, signal and whisper as lookout actions.** Whistle drew
  this boundary already; two non-CLI surfaces with different powers is a
  worse story than one.
- **The typed-name confirmation tier.** It only existed to make delete safe
  enough to ship, and delete is not shipping. It also required `map_key` to
  pass printable characters through for the action path, which is now needed
  only for the filter.
- **The client-side "action sent" row overlay.** The replies carry the
  shepherd's own rows. An overlay would be a guess printed in the one column
  whose job is to be true.
- **`ProcessSelector::parse` as the filter engine, and the hybrid.** Reasons
  under feature 1.
- **Filtering the bleats feed's text.** A different feature.
- **A second `:` prompt for selector-grammar targeting.** The research
  itself flagged this as the cheaper way to add power-user precision later.
  It is a fourth feature, and this phase already has three.
- **`Effect` growing a compound or `Vec` shape.** Two variants and a dirty
  flag cover every trigger.
- **Reversing `ListFlock`'s no-walk decision.** Not lookout's call, recorded
  in three places, and it would bill every other caller for one pane.
- **Filtering on fold as well as name.** See A2; it is one line to add later
  and it costs a sentence of explanation now.

---

# Assumptions

Each of these is a call I made rather than asked about. Reject any single
one without touching the others.

**A1. The filter matches with plain case-insensitive substring, not the CLI
grammar.** Reasoning: `Name` and `Fold` are exact-match, so the grammar
cannot narrow as you type, and a half-typed `/re` silently becomes a name
search. Flip cost: high. This is the load-bearing choice of feature 1.

**A2. It matches the name only, not the fold.** Reasoning: one field means
one rule and nothing to explain, and an operator typing `edge` and seeing
rows whose names do not contain `edge` needs a sentence to make sense of it.
Folds already have a first-class answer in `shep fold` and in the FOLD
column. Flip cost: low, roughly one line plus a sentence in the filter's
own hint saying it searches both.

**A3. `/` is the key.** Reasoning: convention, from less, htop and vim, and
it is unbound. Nothing in the spec says so. Flip cost: trivial.

**A4. The filter lives in the status bar, not on its own row.** Reasoning:
at `MIN_HEIGHT` a new chrome row takes the table's last body row and leaves
nowhere to say why the table is blank; avoiding that needs either a new
height threshold plumbed into the reducer or a raised `MIN_HEIGHT`, and the
title already carries the permanent "a filter is on" signal. Cost of the
choice: a transient notice can briefly cover the filter line. Flip cost:
medium. The ladder has room at every tier above the floor; only the floor
tier needs an answer.

**A5. `Esc` clears the filter instead of quitting whenever one is set.**
Reasoning: it is the conventional key, and the status bar says `esc clear`
for as long as the overload is in force, so no key ever means something the
screen does not say. `q` and `Ctrl-C` quit from every non-editing state.
Flip cost: trivial. The alternative is that clearing takes `/` then `Esc`.

**A6. The query is taken literally, including spaces, with no trimming.**
Reasoning: trimming is a lenience the spec does not ask for, and this repo's
own rule is not to widen an accepted input format without a basis. Flip
cost: trivial.

**A7. The action set is stop, restart and reload.** Reasoning: whistle's
existing boundary, for the same underlying reasons. Flip cost: adding delete
is a real design change, not a key binding, because it should carry a
heavier confirmation than the other three; the research's typed-name design
is recorded above if the maintainer wants it.

**A8. `x` keeps stop; `R` is restart and `L` is reload.** Reasoning: `x` is
already shipped, tested and in the frames; `r` is taken by refresh, so the
new two go on shift for symmetry. `L` for reload is the weakest mnemonic
here. Flip cost: trivial.

**A9. Actions refuse unless the link is `Live`.** Reasoning: an action typed
during the reconnect ladder would otherwise queue and land seconds later, on
a connection the operator has stopped watching. Refusing says no
immediately. Flip cost: low.

**A10. One tier of confirmation for all three verbs: Enter confirms, every
other key cancels silently.** Reasoning: an action key that also confirms
reintroduces the double-tap the gate exists to catch; a "cancelled" notice
reports a non-event and trains people to ignore the bar. Flip cost: low.

**A11. An armed confirm expires after ten seconds.** Reasoning: it is the
same fat finger arriving more slowly, and it costs one `Instant` compared in
the tick arm. Ten seconds is a guess; anything from five to thirty is
defensible. Flip cost: trivial.

**A12. One action in flight at a time, refused rather than queued.**
Reasoning: it keeps the in-flight line unambiguous about which action it
describes, and it makes the state visible instead of hidden in a channel.
Flip cost: low.

**A13. Replies are upserted into the flock map, and no provisional row state
is invented.** Reasoning: `Stopped`, `Restarted` and `Reloading` all carry
`ProcessInfo`, so the shepherd's own words are available and a guess is not
needed. Flip cost: high; this is the honest core of feature 2.

**A14. Lambs are fetched on selection change and on `r`, never on the
poll.** Reasoning: the daemon's own cost model, stated in `identify()`'s and
`with_lambs`'s docs. The cost is visible staleness, which the age stamp
declares. Flip cost: high; the alternative is the fixed-clock walk both docs
argue against.

**A15. The pane grows a line, so `DETAIL_ROWS` goes 4 to 5.** Reasoning: the
tier thresholds do not have to move, and the alternative is squeezing lambs
onto a line that is already full. Cost: every terminal loses one table body
row, and ten pinned frames change. Flip cost: medium.

**A16. The lamb line carries "read Nm ago" before the list, and does not
repeat the CLI's "not exactly the set a stop kills" clause.** Reasoning: the
staleness caveat must survive truncation and the list need not; and the line
saying "parent-pid descendants" is already precisely true, so omitting the
extra clause does not make it dishonest, while repeating forty characters of
warning on every frame trains people to stop reading the pane. Flip cost:
trivial, though the clause will not fit on a narrow terminal.

**A17. A failed lamb fetch is silent in the pane and raises no notice.**
Reasoning: it is a decoration, not an operator's action, and it should not
take the status bar away from something that is. Flip cost: trivial.

**A18. The status bar's priority is confirm, then notice, then filter, then
hint.** Reasoning: a question awaiting an answer outranks a report of
something that already happened, which outranks a persistent state that the
title also signals. Flip cost: trivial.

**A19. `the_detail_pane_never_mentions_lambs` is deleted.** Reasoning: this
feature is the thing it was written to prevent, and its replacement is
strictly stronger. Flip cost: none, but it should be a deliberate line in a
commit message rather than a surprise in a diff.

**A20. Build order is filter, lambs, actions.** Reasoning: each stage lays
plumbing the next one needs, and the riskiest feature lands on machinery
that has already been exercised twice. Flip cost: trivial.
