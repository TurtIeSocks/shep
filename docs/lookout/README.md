# `shep lookout` — Phase 12a frames

`shep lookout` (alias `dash`) is a terminal dashboard over the shepherd.
Phase 12a builds the whole shell — the dependency, terminal lifecycle,
palette, event loop, and link supervision — plus exactly **one** pane: the
flock table. The bleats feed, the sheep detail pane and the host-usage strip
are Phase 12b, and are not built yet.

This directory is not documentation of a shipped design. It is the thing
Rin asked for: *"let's start with flock table first. I need to see the
panels before I can make a full decision."* A TUI cannot be screenshotted
the way a web page can, so these rendered frames are how she sees it before
12b's layout gets decided.

## Reading the frames

- `frames.txt` — eight scenes, plain text. Open it in any editor.
- `frames.ansi` — the same eight scenes, with colour. Read it with
  `less -R` so the escape codes render instead of printing literally.

Both files are generated, not hand-written, and both come from the same
scene list the pinned snapshot tests read (`Scene::ALL` in
`crates/shep-cli/src/lookout/frames.rs`) — so they cannot drift from what
the test suite checks. Regenerate them with:

```bash
cargo test -p shep-cli --bins --all-features -- --ignored write_the_gallery
```

## What 12a settled

- **Daemon death: bounded retry, then freeze, never exit.** The link task
  re-dials the shepherd 5 times, at 250/500/1000/2000/4000 ms — about 7.75 s
  of waiting — before it gives up. Once the ladder is exhausted, lookout
  shows the frozen banner (`the shepherd has died — these values are frozen
  as of <time>`), stops polling and re-dialling, and leaves the last known
  values on screen. The uptime column stops advancing with it: a frozen
  dashboard whose clock kept counting would be lying about a specific sheep
  by name. lookout never exits on its own — the operator quits with `q`.
  A shepherd that was **never** running is a different case: that connect
  attempt happens before raw mode is entered, and a failure there is the
  ordinary `daemon_unreachable` refusal every other verb gives, not eight
  seconds of a full-screen dashboard cycling "reconnecting" for a shepherd
  that was never there.
- **Actions are gated off by default, and it says so.** `--allow-control`
  (or `lookout.allow_control = "true"` in the KV store) has to be set before
  any action key does anything. In 12a exactly one action key exists, `x`
  (stop), and it never acts — it refuses in both states, with a literal
  sentence (`read-only: actions need --allow-control`, or `stop is not
  built yet` once control is allowed but the action isn't). The status bar
  always says which state is in force. This is a fat-finger catch, not a
  security boundary: lookout runs as the operator's own process, under the
  operator's own uid, so the shepherd has no way to refuse a keypress it
  cannot tell apart from `shep stop`.
- **Colour is always redundant with text.** Every coloured cell says the
  same thing in words that the colour is repeating — the STATUS column
  prints `errored` under `--bark`, the banner prints `the shepherd has
  died` under `--bark`. Nothing here is colour-only, so `NO_COLOR` and a
  16-colour terminal both lose decoration, never information.
- **Narrow terminals drop columns in a fixed order**, least diagnostic
  first: FOLD, then RESTARTS and PID, then MEM, then CPU, then UPTIME,
  leaving `ID NAME STATUS` as the floor. Below 31 columns or 6 rows the pane
  refuses outright rather than draw overlapping garbage, with a two-line
  message short enough to survive the narrowest terminal it is warning
  about.
- **The keyboard scrolls; it does not select.** `j`/`k` move the viewport
  by a row, `g`/`G` jump to its ends, and the offset is re-clamped whenever
  a snapshot replaces the flock map. There is no cursor and no selected row
  in 12a — a selection needs a detail pane to read it, and that pane is
  12b.

## What is still open for 12b

- Where the other three panes (bleats feed, sheep detail, host-usage strip)
  sit, and which are focusable.
- Whether the flock table grows a selected row, and what marks it.
- Which actions the control gate lets through once it is wired to
  something, and what confirms them before they run.
- Whether a filter line takes the CLI's own selector grammar or plain
  substring matching.
