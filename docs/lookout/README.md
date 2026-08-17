# `shep lookout` — frames

`shep lookout` (alias `dash`) is a terminal dashboard over the shepherd. It
now draws all four panes spec §9 names: the flock table (the spine, and the
only pane Phase 12a shipped), a host-usage strip above it, and a sheep
detail pane plus a bleats feed underneath a selected row — both added in
Phase 12b, kept as plain as the table that came before them.

This directory is not documentation of a shipped design. It is the thing
Rin asked for: *"let's start with flock table first. I need to see the
panels before I can make a full decision."* A TUI cannot be screenshotted
the way a web page can, so these rendered frames are how she looked at each
phase before deciding what came next.

## Reading the frames

- `frames.txt` — fourteen scenes, plain text. Open it in any editor.
- `frames.ansi` — the same fourteen scenes, with colour. Read it with
  `less -R` so the escape codes render instead of printing literally.

Both files are generated, not hand-written, and both come from the same
scene list the pinned snapshot tests read (`Scene::ALL` in
`crates/shep-cli/src/lookout/frames.rs`) — so they cannot drift from what
the test suite checks. Regenerate them with:

```bash
cargo test -p shep --bins --all-features -- --ignored write_the_gallery
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
  any action key does anything. Three action keys exist — `x` (stop), `R`
  (restart) and `L` (reload) — and none of them acts on the keypress that
  pressed it: an action key arms a confirm, Enter confirms it, any other key
  cancels it, `q` and Ctrl-C still quit even with a prompt up, and an armed
  prompt nobody answers expires after ten seconds. Read-only refuses
  outright, with a literal sentence (`read-only: actions need
  --allow-control`). The status bar always says which state is in force.
  This is a fat-finger catch, not a security boundary: lookout runs as the
  operator's own process, under the operator's own uid, so the shepherd has
  no way to refuse a keypress it cannot tell apart from `shep stop`.
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

## What 12b settled

- **A selected sheep, and the table marks it.** `j`/`k` move the selection
  by a row now, not just the viewport; `g`/`G` jump to its ends. A `>`
  gutter to the left of ID marks the selected row, and the offset re-clamps
  whenever a snapshot replaces the flock map or the selected sheep drops out
  of it. The two panes below the table both describe whichever sheep is
  selected.
- **A name filter, narrowing the table in place.** `/` opens a box in the
  status bar; typing narrows the table to sheep whose name contains the
  query, `Enter` applies it and closes the box, `Esc` cancels the edit, and
  `Ctrl-C` still quits from inside the box. Once a filter is applied, `Esc`
  clears it rather than quitting — the one carve-out to "every other key
  cancels" a filter needs, so an operator does not have to reach for `/`
  and backspace to get back to the whole flock. The title carries a second
  number, `2 of 6 in the flock`, for as long as a filter is narrowing what
  the table shows.
- **The bleats feed reads log files, not the bus.** It re-reads the
  selected sheep's `out`/`err` log files from disk on every refresh, rather
  than subscribing to the `log.*` bus topic. A busy flock costs one bounded
  64 KiB read per file per refresh; subscribing would make the dashboard the
  highest-volume subscriber on the bus for a pane most refreshes don't even
  draw. What the feed cannot show, it says: lines the
  reader saw and discarded count exactly, and bytes below its window report
  as bytes, because nothing counted the lines in those and guessing would be
  worse than saying so.
- **The detail pane reads what the table already has, with one exception.**
  Every line but the last comes from the same `ProcessInfo` the table's own
  rows are built from: the untruncated name, both log paths, and whichever
  columns the current width tier has dropped. The lamb line is the
  exception — `ProcessInfo::lambs` is `None` on the `ListFlock` reply the
  table is built from, so the pane fetches it separately with a
  `Request::Describe` on selection change and on `r`, never on the
  two-second poll, and it carries its own age stamp because of that.
- **Short terminals drop panes before they drop columns.** A plain 80×24
  gets all three: host strip, detail pane, feed. Below 24 rows the detail
  pane goes first, below 18 the feed goes with it, and below 14 the host
  strip goes too and only the flock table remains — the same shape 12a
  shipped alone. The order is least-diagnostic-first, the same principle
  the column drop already used: the detail pane only restates what the
  selected row already shows, so it is the cheapest thing to lose, while
  the feed is the only pane carrying information no other pane has. The
  `no_detail` scene in `frames.txt` is the 120×20 case — feed present,
  detail gone.

`shep lookout` ships complete as of Phase 16: the filter, lambs in the detail
pane, and the three action keys behind the gate are all built. See
[docs/specs/deferred.md](../specs/deferred.md) for the workspace's remaining
debt.
