# Design: the lookout settings screen

Status: designed 2026-09-04, not yet implemented. This builds decision 11 of
[the config overrides design](2026-09-02-config-overrides-design.md), and only
the `shep.toml` half of it. That decision's other half, the overrides store,
and decision 12's write-only env are a later slice.

A bare "decision 11" here means that spec's, which is the authority. This
spec's own numbered decisions are always written "decision 4 above" or
"decision 7 below".

Two things in decision 11 are out of date, and one of its claims about dogs is
wrong. All three are corrected below, with the code that settles them.

## What this is

`shep lookout` gains a settings screen, opened with `s`, that reads and writes
`$SHEP_HOME/shep.toml`. Six scalars and a per-dog on and off toggle. Editing is
behind the existing `--allow-control` gate; reading is not.

The design work is in telling an operator what a change will and will not do.
That differs per field, and decision 11's own table gets it wrong.

## What already exists

Established from the code, not assumed.

- **`ShepToml` is the one writer this binary has for `shep.toml`**
  (`crates/shep-cli/src/commands/shep_toml.rs`). `toml_edit`, so comments and
  key order survive; an exclusive advisory lock on a sibling file held across
  the read, modify and write; the new document staged at `0600`, `fsync`ed and
  renamed. `shep style`, `shep enable`, `shep adopt` and `shep rehome` all go
  through it.
- **That lock blocks with no deadline.** `ConfigLock::acquire` uses
  `FlockArg::LockExclusive` and says so at `shep_toml.rs:717`: *"`LockExclusive`
  blocks; the non-blocking variant would need a retry loop and a deadline."*
- **`try_edit`'s refusal leaves the file untouched down to its inode.** A
  closure that returns `Err` skips `save` entirely, so no rename lands and no
  fresh inode replaces the original.
- **lookout's reducer is pure.** `App::update` returns an `Effect` and does no
  I/O; `run_ui` performs it. The reducer holds no terminal types either, because
  `input::map_key` does the crossterm translation at the edge.
- **`--allow-control` is a fat-finger catch and the code says so**
  (`lookout/app.rs:48`). Three keys arm a confirm rather than acting on the
  press that armed them, and an armed confirm expires after ten seconds off
  `Msg::Tick`.
- **`StyleSource` already exists for exactly this class of bug**
  (`crates/shep-cli/src/style.rs:223`). Its own doc: *"the failure this prevents
  is an operator editing `shep.toml` and seeing nothing change, with
  `$SHEP_STYLE` set in a shell profile they have forgotten about."* `shep style`
  reports which layer decided.
- **A fresh `shep.toml` contains only `[interpreters]`.**
  `scaffold_first_run_interpreters` (`lib.rs:776`) runs at every `home_is_new`
  site and writes the starter interpreter mapping; nothing scaffolds `[daemon]`,
  `[style]` or `[whistle]`. `ShepToml::open` treats a missing file as an empty
  document, so on a fresh box every field this screen shows is absent.
- **There is a lock-free read path already.** `adopted_dog_path_readonly`
  argues it: `save`'s rename is atomic, so a concurrent writer can only be
  observed before or after it, never torn.
- **`shep style` cannot clear `[style] level`.** `set_style_level` only writes,
  and the verb with no argument reports the level in force instead of changing
  it (`cli.rs:1177`).
- **`s` and `space` are unbound.** `q`, `Esc`, `/`, `j`, `k`, `g`, `G`, `r`,
  `x`, `R`, `L` and Enter are taken.
- **`BUILT_IN_DOGS` is `["metrics", "bark"]`** (`crates/shep-cli/src/dog/mod.rs:46`).
  Every other dog name is a key of `[daemon] adopted_dogs`.

## Three corrections to decision 11

### `[dog.<name>]` is not excluded, because it is not there

Decision 11 excludes it as "a third-party dog's opaque schema". PR #112 moved
every dog's config to `$SHEP_HOME/dogs.toml`, migrated once at boot.
`RawDaemonConfig` keeps a `dog` field only so an un-migrated file still parses.
There is nothing left to exclude.

A dogs config pane is separate later work with its own spec
([the dog config design](2026-09-03-dog-config-design.md), decision 9). Dogs
now publish a config schema, but the pane itself is not built.

### Decision 13 does not apply to this screen

Decision 13 generates panes from `flockfile_schema_json()`. That describes the
Flockfile. `shep.toml` is described by `DaemonConfig`, a struct shep owns, whose
sections are three known shapes with six scalars between them. The screen is
written against those fields by name.

### A dog toggle costs nothing, and applies now

Decision 11's table puts `[dog.*]` under "needs `shep daemon reload`". That is
wrong for the toggle, and the reason is that `shep enable` is two steps rather
than one. It writes the file, then sends `Request::EnableDog` to the shepherd,
which starts the dog immediately (`commands/dogs.rs:193`). `shep disable`
mirrors it with `Request::DisableDog` (`commands/dogs.rs:349`). Routed through
that code, a toggle in lookout applies live and needs no reload.

The handover asymmetry decision 11 describes is real, and it is about a
different path: what a `daemon reload` does to an `enabled_dogs` list somebody
edited by hand. All three of its claims verify:

| Claim | Where |
| --- | --- |
| A successor computes the socket path and discards it | `boot.rs:1170` computes it; `boot.rs:1178`'s handover arm calls `rehydrate` and never `bind_socket` |
| `do_start_dog` short-circuits on a name collision | `supervisor.rs:3355` returns the existing slot's `to_info` |
| A name removed from `enabled_dogs` keeps running | `spawn_enabled_dogs` (`dogs.rs:280`) iterates the enabled list only, and nothing walks the inherited flock for de-enabled names |

Because the toggle does not go through a handover, none of that reaches the
screen. What does reach it is the RUNNING column, which shows the asymmetry as a
fact when an operator has hit it by another route.

## Two things decision 11 does not cover

### The shepherd's own env and flags

The daemon boots `file < env < flags` (`commands/daemon.rs:318`, reading
`std::env::var` and its own argv), and a handover successor inherits both
through `execve`. So all four `[daemon]` scalars can be shadowed by
`SHEP_LOG_JSON`, `SHEP_LOG_LEVEL`, `SHEP_SOCKET`, `SHEP_MAX_CRON_SLEEP` or the
matching `shep daemon` flags. A `log_level` edit followed by a reload is inert
if the shepherd was booted with `SHEP_LOG_LEVEL` set.

`HelloAck` carries `daemon_version`, `protocol` and `pid`, so **lookout cannot
see either layer.** It belongs to a different process.

`[style]` is the exception, and it inverts: `$SHEP_STYLE` and `--style` are
lookout's own env and argv, so lookout can name the layer in force exactly,
reusing `StyleSource`.

This is the same defect class as decision 11's own socket note, one layer up.

### An absent field, which is the common case

`DaemonSection`, `WhistleSection` and `StyleSection` are all
`#[serde(default)]`, so `DaemonConfig::load` returns a fully populated struct
whether the file declared a key or not. `log_level` reads `warn` for a file that
says `warn` and for a file that says nothing, and by then the difference is
gone.

On a fresh `$SHEP_HOME` that is every field on the screen, so it is the state
most operators open it in rather than an edge case. And it is the same defect as
the section above: a layer the screen cannot see. That one was caught by
reading the boot path. This one hides inside a struct that looks like an answer.

## Decisions

### 1. A screen, not a fourth stacked pane

`s` swaps the dashboard body for the settings screen and `s` or `Esc` swaps back.
The title line and the status bar stay put.

At 24 rows the three existing panes claim 13 of them and settings needs about
ten, so a fourth tier would sit above 34 rows and most terminals would never
reach it. Settings is also a modal task rather than a monitoring surface: an
operator opens it, changes something, and leaves.

### 2. Per field, arm and confirm

Editing a field arms a candidate and shows a prompt naming that field's own
apply cost. Enter applies it. One field per write.

This is the shape `x`, `R` and `L` already use, down to the ten-second expiry,
so an operator who has learned one has learned the other. A staged set with one
apply was considered and refused: a half-finished set is a state the reducer has
no shape for, and the batching it buys matters only for dogs, which are the
cheapest edits on the screen.

`space` arms and re-arms. Each press advances the candidate and resets the
clock, which is the only way to reach the fourth of six log levels without
cancelling and re-arming.

### 3. Both free-text fields are editable, validated under the lock

Four of the six scalars are booleans or closed enums. `socket` and
`max_cron_sleep` are free text, and both get an editor reusing the filter box's
`InputMode::Text` keymap.

Validation happens in one place, inside the `try_edit` closure: mutate the
document, render it, run `DaemonConfig::load` over the result, and return `Err`
on refusal. A refusal reopens the editor with the typed text intact and the
loader's own message as a grave notice.

Not at arm time, deliberately. `MIN_CRON_SLEEP` is private to `shep-core`, so an
arm-time pre-check would re-derive the one-second floor locally, which is
duplicating a rule to save a keystroke. Validating under the lock also catches a
document another process made bad between the read and the write, which no
pre-check can.

### 4. The screen reads the document, and every scalar carries a source

The screen reads presence out of the `toml_edit` document, key by key, because
that is the fact `DaemonConfig::load` destroys. `DaemonConfig` keeps two jobs
and loses one: it supplies the effective value when a key is absent, and it
validates inside `try_edit`, but it is not what the screen reads a value from.

That gives every scalar a SOURCE, and the vocabulary already exists.
`StyleSource`'s own `Display` renders `--style`, `$SHEP_STYLE`, `shep.toml` and
`the default`. Only `[style]` ever reaches the first two, because only those
layers are lookout's own process.

```
  [daemon]
> log_level        warn                       the default    needs: shep daemon reload
  log_json         false                      the default    needs: shep daemon reload
  socket           ~/.shep/run/shep.sock      the default    needs: full stop and start
  max_cron_sleep   30s                        shep.toml      needs: shep daemon reload

  [whistle]
  allow_control    false                      the default    needs: shep whistle restart

  [style]
  level            full                       $SHEP_STYLE
```

The column is headed SOURCE and not IN FORCE, and the difference carries weight.
`shep.toml` is a true statement about where lookout read the value. It is never
a claim that the shepherd is using it, which the confirm handles separately for
the two layers lookout cannot see.

`socket` shows `paths.socket` rather than an empty cell, because that is the
socket this lookout is connected over, so it is the live answer by construction.

### 5. Unsetting exists where the field is optional and no verb owns it

`socket` and `max_cron_sleep` can be unset from an empty text editor, which
deletes the key and returns the row to `the default`.

`style.level` is `Option<String>` too, so the types alone would give it an unset.
It does not get one: `shep style` owns that key, `set_style_level` only ever
writes, and the verb with no argument reports rather than clears. lookout does
not grow a capability the verb that owns the field lacks.

`log_level`, `log_json` and `allow_control` are not optional and always write a
key. So the screen can move those three from `the default` to `shep.toml` and
not back, which is a real limitation and is stated on the docs page rather than
left for an operator to discover.

### 6. The apply cost is per field, and the caveat lands in the confirm

| Field | Edit | Cost |
| --- | --- | --- |
| `[style] level` | cycle | nothing, the next command reads it |
| `[whistle] allow_control` | toggle | `shep whistle` restarted |
| `[daemon] log_level` | cycle | `shep daemon reload` |
| `[daemon] log_json` | toggle | `shep daemon reload` |
| `[daemon] max_cron_sleep` | text | `shep daemon reload` |
| `[daemon] socket` | text | the shepherd stopped and started |
| a dog | toggle | nothing, it applies now |

The shadowing caveat goes in the confirm rather than in an always-visible header,
because it arrives where the operator is deciding and costs nothing on a screen
that has other things to say. A `[daemon]` confirm names the variable and the
flag by their own spellings.

`[style]`'s row is the one that states its layer instead of hedging, because
lookout can see it: `full   in force from $SHEP_STYLE`.

`socket` says both halves. A reload will not move it, and an env or flag may
shadow it anyway.

### 7. lookout does not trigger the reload

`shep daemon reload` is a config pre-flight, a dog migration, a `HandoverFitness`
question and then `execve` signalling (`commands/daemon.rs:657`). It is not a
wire request, and a handover severs lookout's own link mid-frame. Reproducing it
inside the reducer would be a second implementation of the most dangerous command
in the repository.

lookout names the command. The operator runs it.

### 8. The screen opens read-only when the gate is off

Every value shows; the edit keys refuse by notice, the way `x`, `R` and `L`
already do.

Reading `shep.toml` is not a privileged act: anyone who can run lookout can read
the file. The screen leaks nothing either, which is checked rather than assumed
under decision 11 below. And a read-only screen is still diagnostic, since it
names the style layer in force and shows which dogs are enabled against which
are running.

### 9. The dogs section shows three columns

Name, what the file says, what the shepherd reports, and where the binary comes
from. The middle two are joined by name from `App::flock`, which already carries
every running dog: `ProcessInfo.dog` marks one, and Phase 3b's `handshook` is
what makes a dog that never completed a handshake read `silent` rather than
`online`.

```
  [dogs]   space arms, Enter applies; a dog needs no reload

  NAME       IN FILE     RUNNING     SOURCE
> metrics    enabled     online      built in
  bark       enabled     silent      built in
  otel       -           online      /usr/local/bin/shep-otel
  ledger     enabled     -           /opt/ledger/bin/dog
```

Two rows there are drift an operator cannot see anywhere else. `otel` is running
while disabled in the file, which is what "a removed name keeps running" looks
like. `ledger` is enabled and absent, which is a dog that failed to start.

SOURCE is the widest column and the first to drop, mirroring `columns_for`'s own
tier table in `view/flock.rs` and the reasoning in its doc.

### 10. The toggle reuses `shep enable`'s decision, not its reporting

`shep enable`'s file half is a `try_edit` closure holding a real decision: which
`DogSource` this name resolves to, whether it names a dog at all, and the
mutation. Its daemon half writes rows to stdout, which lookout does not have.

So the cut is at that seam:

```rust
pub(crate) fn enable_in_config(path: &Path, name: &str) -> Result<DogSource, EnableRefusal>
pub(crate) fn disable_in_config(path: &Path, name: &str) -> Result<DogSource, ShepTomlError>
```

`dogs::enable` and `dogs::disable` call those and keep their reporting; lookout
calls the same two and reports through `Notice`. Nothing re-implements what a dog
name means.

### 11. Nothing on this screen is sensitive, and that is checked

`StyleSection` and `WhistleSection` both carry an explicit note that their
`Debug` is derived rather than redacted because neither holds a secret;
`DaemonSection` derives `Debug` too. `DaemonConfig`'s own manual `Debug` redacts
one field, `dog`, which this screen does not touch. So no redacted `Debug` is
owed here (IR-41), and the call site says so rather than leaving it silent.

## Data flow

Two new effects, both performed in `run_ui` on `spawn_blocking`. The blocking
matters for the write: the config lock has no deadline, so a concurrent
`shep adopt` would otherwise freeze the redraw, the tick and the bus drain
together. The read takes no lock at all, following
`adopted_dog_path_readonly`'s argument, and rides `spawn_blocking` anyway so
that the rule is "no file I/O on the redraw task" rather than a judgement call
per site.

```
s              -> Effect::LoadSettings   -> Msg::Settings { fields, dogs }
Enter, armed   -> Effect::WriteSetting   -> Msg::SettingWritten { edit, result }
```

A scalar write ends there, in a notice. A dog write chains one step further,
because its file half and its daemon half are two acts:

```
Msg::SettingWritten { Ok(source) } -> Effect::Send(Sent::Dog { .. })
                                   -> Msg::Replied -> notice
```

File first, then the daemon, which is `shep enable`'s own order. `Sent` gains a
`Dog` variant carrying the name and the `DogSource` the write returned, so
`Sent::request()` builds `EnableDog` and `DisableDog` the same way it already
builds `Stop`, `Restart` and `Reload`.

`PROTOCOL_VERSION` does not move. Both requests already exist.

## Keys

| Key | Does |
| --- | --- |
| `s` | opens from the dashboard, closes back to it |
| `j` `k` `Up` `Down` `g` `G` | move the cursor across sections and into the dogs list |
| `space` | arms a candidate, and advances it on each further press |
| `Enter` | opens a text field's editor, or applies an armed confirm |
| `Esc` | cancels the confirm, else the editor, else closes the screen |
| `r` | re-reads `shep.toml`, so another process's write shows up |

`Esc` never quits from this screen. That is the cascade `KeyPress::Escape`'s own
doc already describes, with one arm swapped.

`space` and `s` are unbound on the dashboard and stay that way: the keymap emits
them and the reducer decides, because the keymap cannot see whether the screen is
open. That is the same division the existing `Escape` arm is built on, so
`InputMode` gains no third variant.

The four `KeyPress::Filter*` variants are renamed `Text*`. The settings editor
needs the identical keymap, and a variant named for the filter box would be
naming a destination the keymap cannot see. This touches shipped code and the
keymap's own test, so it lands first, on its own, before any of this screen
exists.

`App.selected` is untouched by opening or closing the screen, so the flock
cursor and the filter both survive the swap by construction. A test pins it
rather than leaving it to inspection. The settings cursor itself resets to the
first field on every open, because the open re-reads and the dogs list can
change length underneath a remembered position.

## One divergence from the sheep confirm

`Msg::Tick` stops advancing `self.now` once the link is `Lost`, so a sheep
confirm never expires on a frozen dashboard, and `disarm_on_link_change` clears
it. A settings edit is local file I/O over a file that is not stale, so it
diverges:

| On a lost link | A sheep action | Settings |
| --- | --- | --- |
| an armed confirm | disarmed | survives, and still expires, off the raw tick |
| the six scalars | not applicable | still editable |
| a dog toggle | not applicable | refuses with the existing `LINK_GONE` sentence |

A dog toggle needs the link because its second step is a request. The scalars
never leave the machine.

## Testing

No sleeps. Expiry rides synthesised `Instant`s through `Msg::Tick`, mirroring
`a_confirm_expires_after_ten_seconds_of_ticks` (IR-46).

| Tier | Proves |
| --- | --- |
| reducer, `lookout/app.rs` | arming, candidate cycling, expiry, the `Esc` cascade, the read-only refusals, the lost-link split above, and that `Msg::SettingWritten { Ok(..) }` yields `Effect::Send` |
| render, `lookout/view/settings.rs` | the screen at several widths, for SOURCE's drop tiers |
| `commands/dogs.rs` | `enable_in_config` and `disable_in_config` against the existing fixtures |
| refusal | a `max_cron_sleep` under the floor is refused, and the file is byte-identical afterwards |
| absence | a `shep.toml` holding only `[interpreters]`, which is what a fresh home has, renders every scalar as `the default`, and a file that declares `log_level = "warn"` renders the same value as `shep.toml` |
| selection | the flock cursor and the filter survive a swap out to settings and back |

Every new test is mutated to prove it is not vacuous, and the mutation is checked
to have applied rather than trusted.

## Docs

`docs/lookout/frames.txt` gains settings frames through the existing
`write_the_gallery` ignored test, in the shape the file already uses.
`web/src/pages/docs/lookout.astro` gains the `s` key and the screen. Then
`cargo build --release`, `./web/scripts/generate-cli-reference.sh`, and both
`astro build` and `astro check`.

## Out of scope

- **`adopted_dogs` is not editable yet.** `shep adopt` vets a candidate by
  spawning it and probing `--version`, refusing a protocol mismatch, and a raw
  edit walks past that vetting. This is a later slice rather than a closed door:
  the dogs table already renders the path in its SOURCE column, so making that
  column writable is an addition, and what it has to route through is the probe.
- **`[interpreters]`**, a free-form extension map with no field list to render
  (decision 11).
- **Decision 11's overrides half**, and decision 12's write-only env. Both are
  a later slice.
- **Triggering the reload**, per decision 7 above.
