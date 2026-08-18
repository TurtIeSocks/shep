# A pretty CLI — design

**Date:** 2026-08-18
**Status:** approved, ready for an implementation plan
**Scope:** `shep` (the CLI crate) only. No wire change, no daemon change, no
new verb behaviour beyond one new verb (`style`).

## The ask

"Make it pretty. Colors. Tables. Sheep." — box-drawn tables, colour, sheep
turned up to maximum, and one command to turn it all off.

## What already exists

shep is not starting from nothing, and this design deliberately inherits
rather than invents:

- **A palette, already sheep-named.** `crates/shep-cli/src/lookout/theme.rs`
  defines `meadow`, `bark`, `butter` and `ink3`, in a 256-colour tier and a
  16-colour fallback, with `NO_COLOR` honoured and tested. It maps all six
  `ProcStatus` values onto four roles (`theme.rs:103`).
- **`anstyle` 1.0.5**, in the workspace dependency table and the ecosystem's
  standard CLI style type. **It is not yet a dependency of `shep-cli`** and
  the plan must add `anstyle.workspace = true` there; only shep-core pulls it
  today, through annotate-snippets.
- **`crossterm`**, a direct dependency of `shep-cli` — but declared inside
  the crate's **`cfg(unix)` block**, deliberately, so a Windows build does
  not link a terminal stack it can never use. Terminal width therefore has no
  crossterm source on Windows, which the 80-column fallback must cover
  unconditionally rather than as an error path. Windows refuses every verb
  today, so this costs nothing now and must still compile.
- **The `ansi_enabled(stderr_is_terminal, no_color)` pattern**
  (`commands/daemon.rs:187`) — terminal-ness as a parameter, never an
  `IsTerminal` call inside the function, so the branch is testable.
- **ASCII sheep** in `welcome.rs`, and the lesson from writing them: a `\`
  line-continuation in a Rust string strips the next line's leading
  whitespace, which silently unindented a sheep and no exact-string test
  could see it.

## Non-goals

- **No wire change.** Rendering is a CLI concern; the daemon has no opinion
  about how anyone likes their tables.
- **No second palette.** The colour vocabulary is `lookout/theme.rs`'s.
- **No shared rendering code with `lookout`.** See "The two-table seam".
- **No change to what any verb does.** Only to how its output looks.

## 1. The boxed renderer

`output/table.rs` grows a boxed mode beside the current plain one. Three
things make it non-trivial, and each is a named test in §5.

### Visible width, not string length

A styled cell is `\x1b[32m(o.o)\x1b[0m`: 15 bytes, 5 columns. Padding must
measure **visible** width or every border after the first coloured cell
drifts right. This is not a hypothetical — three hand-drawn mockups during
this design had misaligned borders, each time by exactly this mistake.

Visible width means: strip ANSI, then count. The renderer never pads a
styled string directly; it computes on the plain text and applies style
after.

### Column priority and adaptive dropping

Every column declares a priority. The renderer drops the highest-priority
number until the table fits the terminal, with a floor of three columns.

| priority | columns |
|---|---|
| 0 (never dropped) | `ID`, `NAME`, `STATUS` |
| 1 | `UPTIME` |
| 2 | `PID` |
| 3 | `MEM` |
| 4 | `RESTARTS` |
| 5 | `CPU` |
| 6 | `FOLD` |

A dim footer names what was hidden and points at the escape hatch:

```
  CPU, FOLD, RESTARTS hidden — widen, or --format json
```

Nothing is ever hidden silently. `--format json` always carries every field.

Measured from a working prototype, on the real nine-column flock listing:

- 120 columns available: all nine fit (table is 90 wide)
- 80 columns: `FOLD` drops, table is exactly 80
- 56 columns: `CPU`, `FOLD`, `RESTARTS` drop, and `STATUS` narrows to the
  face alone

### Terminal width

From `crossterm`, falling back to 80 when there is no tty. Adaptation never
runs on piped output, because piped output is not a table at all (§3).

## 2. Status: a sheep face, and the word when there is room

`STATUS` renders the face always, and the word when the width budget allows.
The word is the first thing dropped from that column, before any whole
column is.

| status | face | palette role |
|---|---|---|
| Online | `(o.o)` grazing | `meadow` |
| Starting, WaitingRestart | `(o~o)` waking | `butter` |
| Stopping, Stopped | `(-.-)` asleep | `ink3` |
| Errored | `(x.x)` startled | `bark` |

The status-to-role mapping is taken verbatim from
`lookout/theme.rs:103`'s `status()`, so the two renderings agree by
construction rather than by intent. **The face vocabulary is new and must be
added to `theme.rs` beside the colours**, not defined in `output/`, so that
`lookout` can adopt the same faces without a second source of truth.

Face-plus-word is 15 columns; face alone is 5, which is *narrower* than
today's plain `stopped` plus padding. Whimsy buys column budget here rather
than spending it.

## 3. Style levels

One dial, persisted in `shep.toml` under a new `[style]` section, read by the
CLI only.

| level | sheep | boxes | colour |
|---|---|---|---|
| `full` (default) | yes | yes | yes |
| `plain` | no | yes | yes |
| `bare` | no | no | no |

Resolution order, first hit wins: `--style` flag, `$SHEP_STYLE`,
`shep.toml`, then `full`.

`NO_COLOR` removes colour at any level, orthogonally. It is a colour
convention, not a layout one; honouring it at `full` is the entire reason it
is a separate axis. `theme.rs` already implements the exact semantics
(including that an empty `NO_COLOR=` counts as unset) and its rule is
reused rather than restated.

`shep style` with no argument prints the level **and where it came from**,
which is what an operator needs when an env var and a config file disagree.

### The hard rule

**Piped output and `--format json` are byte-identical to today.** No boxes,
no colour, no faces, no adaptation. `bare` is exactly what a pipe gets, so
`SHEP_STYLE=bare` reproduces piped output at a terminal.

This is not politeness. `cli_e2e` asserts on exact stdout; `shep completions`
emits 1900 lines of shell that would *execute* a stray border; and
`exit_codes_and_stream_discipline` already caught stdout pollution once
during this session's earlier work.

## 4. Where the sheep go

Beyond the status column, sheep appear in exactly three places — each a
moment with nothing else to look at:

- **An empty flock.** The state where a new user most needs a next step and
  currently gets a bare header row.
- **A flock entirely stopped.** Visually distinct from empty, which it is
  not today.
- **`shep muster`.** The one verb whose whole job is a milestone.

The count scales with the flock and **caps at five**, so `shep muster` over
forty processes does not paint a field.

### Never on errors, never on destruction

`docs/terminology.md` already sets this rule: destructive ops and error text
stay plain, the theme never costs clarity. So:

- **No sheep on any error.** Errors keep colour, because red is information,
  and lose everything else. A sheep beside `error[not_found]` makes a
  failure harder to read and reads as flippant to someone debugging at 2am.
- **No milestone art for `kill`, `delete` or `stop`.** They are destructive;
  a cheerful sheep after deleting a service is exactly wrong.

## 5. Testing

- **A property test over the renderer.** For any rows, any terminal width,
  any style level, and any mix of styled and unstyled cells: every line of
  the output has identical visible width, and that width does not exceed the
  terminal. This is the bug that will actually happen, so it gets the
  strongest test. **`proptest` is a dev-dependency of `shep-daemon`, not of
  `shep-cli`**: it is in the workspace table, and the plan must add
  `proptest.workspace = true` to `shep-cli`'s `[dev-dependencies]`.
- **Exact-string tests per level.** `full`, `plain` and `bare` each pinned,
  the way `docs/lookout/frames.txt` pins a TUI frame. Art drifts silently
  otherwise — and per `welcome.rs`'s experience, a pinned string can still
  hide an indentation bug, so the art is also rendered and read by a human
  once before it ships.
- **The existing e2e suite is the pipe test.** `cli_e2e` must pass
  *unchanged*. If a border or an escape reaches piped stdout, it fails. No
  new test needed.
- **`NO_COLOR` at `full`.** Sheep and boxes survive; colour does not.
- **Style precedence.** Flag over env over `shep.toml` over default, each
  pinned, including what `shep style` reports as the source.
- **Column dropping.** The priority order above, asserted at three widths,
  plus the footer naming exactly what was hidden.

## 6. The two-table seam

`shep lookout` already renders a flock table, in ratatui, with its own
palette. After this there are two flock tables with two renderers, and that
is a real seam worth stating rather than discovering.

They **share the vocabulary and not the code**: `theme.rs` owns the palette
and (newly) the face glyphs; `output/table.rs` owns box drawing for the CLI;
ratatui owns layout for the TUI. Sharing rendering would mean making one of
them render through the other's model, which is a much larger change than
this design earns.

The risk this leaves is drift: someone changes a face in one place and not
the other. The mitigation is that both read the same constants, so a face
change is a one-line edit in `theme.rs` that both pick up. A colour or face
defined in `output/` rather than `theme.rs` is a review defect.

## 7. Dependency facts, checked

Three claims in the first draft of this spec were wrong in the same way: a
crate present in the *workspace* table was assumed present in `shep-cli`.
Verified against the manifests rather than assumed:

| what | where it actually is | action |
|---|---|---|
| `anstyle` | workspace table only | add to `shep-cli` deps |
| `proptest` | workspace table, `shep-daemon` dev-deps | add to `shep-cli` dev-deps |
| `crossterm` | `shep-cli`, inside `cfg(unix)` | width falls back to 80 off-unix |
| `insta` | `shep-cli` dev-deps already | use for the per-level snapshots |

## 8. Assumptions

Recorded because they were judgement calls, not requirements:

1. `full` is the default. Rin asked for maximum whimsy; someone who wants
   less has `shep style` and a one-line config.
2. The floor is three columns (`ID`, `NAME`, `STATUS`). Below the width even
   that needs, the table renders wider than the terminal rather than
   dropping identity columns.
3. Faces are ASCII, not emoji. A `🐑` is double-width, inconsistently so
   across terminals, and cannot take a foreground colour — three reasons the
   width maths would become guesswork.
4. The style setting lives in `shep.toml`, not the KV store. The KV store is
   for operator notes; this is configuration.
5. `shep style` is a first-class verb rather than a hidden one. It is the
   documented escape hatch, and an off-switch nobody can find is not one.
6. Milestone sheep are capped at five rather than scaled logarithmically.
   The cap is easier to reason about and no one needs to count sheep.
