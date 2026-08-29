# lookout TUI — UX-surface phase design notes (research)

Key: `lookout-tui` · 2026-08-07 · network was available: all versions verified
against crates.io on this date.

Scope: spec §9 "lookout (TUI): ratatui; panes = flock table, bleats feed,
sheep detail, host usage; event-driven redraw; search/filter". Lives in
`shep-cli` per spec §3 (lookout is a CLI concern). `dash` alias, per §9.

---

## 1. Crate choices (exact versions, crates.io 2026-08-07)

| Crate | Version | Features (IR-2: `default-features = false`) | Why |
|---|---|---|---|
| `ratatui` | **0.30.2** | `std`, `crossterm_0_29`, `layout-cache`, `underline-color` | Current stable of the modular rewrite (ratatui-core 0.1.2 + ratatui-widgets 0.3.2 underneath). `crossterm_0_29` pins the backend's crossterm line explicitly so it can never skew from our direct dep. Skip `all-widgets` (only gates the calendar widget — unused) and `macros` (ratatui-macros is sugar; KISS). |
| `crossterm` | **0.29.0** | `events`, `event-stream`, `windows` | Direct dep for `EventStream` (async input; needs `event-stream`) and key/event types. Same version ratatui-crossterm 0.1.2 wraps → one copy in the graph; our `event-stream` feature unifies additively. `windows` is required for the Windows functional tier (§11) and is inert elsewhere. |
| `sysinfo` | **0.38.4** | `system` | Host-usage pane (cpu/mem/load). `system` alone avoids the disk/network/component dep trees. NOTE: workspace-wide choice — §4's memory-limit poller ("cheap via sysinfo") should use the same version. |
| `tui-input` | **0.15.3** | `crossterm` | Filter-line editing state (cursor, unicode-segmentation-correct movement). Depends only on unicode-{segmentation,width}; declares ratatui `^0.30` / crossterm `^0.29` — matches. Hand-rolling cursor math over grapheme clusters is the classic TUI bug farm; 2 tiny transitive deps buy it done. |
| `insta` | already in workspace (`1.18.1` floor; latest 1.48.0) | `json` (present) | TestBackend snapshots use `assert_snapshot!` (string) — no new features needed. No bump required; semver floor already admits 1.48.0. |

Not taken: `tokio-stream` (futures-util already in workspace; `StreamExt::next()` covers it), `better-panic`/`color-eyre` (ratatui 0.30's `init()` installs a restoring panic hook — enough), `tui-textarea` (multi-line editor, overkill), any PTY test harness (see §6).

### The MSRV problem (decide first — it gates everything above)

Workspace `rust-version = 1.85` (IR-4). But:

- ratatui 0.30.2 → MSRV **1.88.0** (0.30.0 → 1.86)
- sysinfo 0.38.4 → MSRV **1.88** (0.39.x → 1.95 — too new, ~Apr 2026)
- crossterm 0.29.0 → 1.63 (fine); tui-input declares none (trivial deps)

**Recommendation: bump workspace MSRV 1.85 → 1.88** (released 2025-06-26, >13 months old at time of writing — comfortably conservative). Take ratatui 0.30.2 + sysinfo 0.38.4. Fallback if the maintainer vetoes the bump: ratatui **0.29.0** (MSRV 1.74, pre-modular but API-similar for our usage) + sysinfo **0.36.1** (1.75) + tui-input **0.14.x** — a known-good but aging island; the 0.29→0.30 migration later is mostly import paths. The bump is the better trade: the UX-surface phase shouldn't build on a line that was already superseded when the phase started.

---

## 2. Module shape (`crates/shep-cli/src/lookout/`)

```
lookout/
  mod.rs        entry: pub fn run(handle: impl DaemonHandle) -> anyhow::Result<()>
                terminal lifecycle (ratatui::init/restore), owns the event loop
  msg.rs        Msg + Effect enums (the reducer's vocabulary)
  app.rs        App state + `fn update(&mut self, Msg) -> Effect` — sync, pure-ish,
                zero I/O, zero terminal types. THE testable core.
  view/
    mod.rs      fn draw(app: &mut App, frame: &mut Frame) — layout + dispatch
    flock.rs    flock table pane (TableState)
    bleats.rs   bleats feed pane (ring + follow mode)
    detail.rs   sheep detail pane
    host.rs     host usage strip (heft: cpu/mem/load gauges + sparkline)
    status.rs   bottom bar: mode, filter input, conn state, key hints
  input.rs      KeyEvent -> Option<Msg> keymap, per (InputMode, Focus)
  client.rs     trait DaemonHandle { list_flock, describe, subscribe } + the
                real impl over shep-client; fake lives in the test fixture mod
  hostprobe.rs  sysinfo sampler: spawn_blocking task -> mpsc<HostSample>
  ring.rs       BleatRing: bounded VecDeque<BleatLine> + drop-marker insertion
```

Rules honored: reducer never touches ratatui types (renderable headless);
view never mutates App except widget scroll states; all I/O behind
`DaemonHandle` so IR-33's hand-rolled fake slots in; `anyhow` is fine here
(shep-cli only, IR-18). Tuning constants (`POLL_INTERVAL_MS = 2_000`,
`MIN_REDRAW_MS = 33`, `BLEAT_RING_CAP = 2_000`, `HEARTBEAT_MS = 1_000`) are
named consts with rationale comments per IR-26.

### Event loop skeleton (mod.rs)

```rust
loop {
    tokio::select! {
        biased;                                   // input first: latency
        ev  = term_events.next()   => msg = input::map(ev),
        ev  = sub.next()           => msg = Msg::Client(ev),   // BusEvent / conn state
        res = flock_poll.tick()    => effect = Effect::RefreshFlock,
        s   = host_rx.recv()       => msg = Msg::Host(s),
        _   = heartbeat.tick()     => msg = Msg::Tick,         // 1 s: uptime column
        _   = redraw_gate, if dirty => { terminal.draw(|f| view::draw(&mut app, f))?; dirty = false; }
    }
    dirty |= matches!(app.update(msg), changed);
    run_effect(effect).await;                     // RefreshFlock / Describe / Quit
}
```

`redraw_gate` = `sleep_until(last_draw + MIN_REDRAW_MS)` armed only while
dirty → redraw coalescing for free: a burst of 500 log lines mutates state
500 times, draws ≤ 30 times/s. Resize event forces dirty. Quit on `q` /
`Ctrl-C` / `Esc` (normal mode).

---

## 3. Hardest design decisions

### D1 — Redraw model: pure event-driven vs fixed tick. → Event-driven, dirty-flag, throttled; plus a 1 s heartbeat

Fixed-tick redraw (pm2 `monit` style) burns CPU idle and still lags input.
Pure event-driven starves time-derived cells (uptime column). Hybrid above:
every Msg marks dirty; draw only when dirty, throttled to `MIN_REDRAW_MS`;
a 1 s `Tick` re-derives displayed uptimes so the column advances with zero
daemon traffic. Uptime derivation: store `(uptime_ms, Instant::now())` at
receipt, display `uptime_ms + anchor.elapsed()` — never trust a stale number,
never poll to animate it (spec §9 "event-driven redraw" satisfied literally).

### D2 — State architecture: Elm-style reducer vs component objects. → Single `App` reducer (Msg → update → Effect), views are pure functions

The focus question ("how BusEvent stream and periodic ListFlock merge into
one app-state reducer") answers itself once everything is a `Msg`: input,
bus events, poll results, host samples, ticks, and connection changes all
funnel through `App::update`. Payoff is IR-36-style testing — a scripted
`Vec<Msg>` produces a deterministic App, then one TestBackend render pins the
frame. Component-trait architectures (each pane owning handlers) scatter the
merge logic across four panes and make the reconciliation invariants (D3)
untestable in one place. Effects (`RefreshFlock`, `Describe(id)`, `Quit`)
come back out of the reducer so it stays sync and I/O-free.

### D3 — Bus/poll reconciliation: event-sourced vs poll-as-truth. → Events are latency hints; ListFlock snapshot is truth; `Dropped` triggers immediate repair poll

The bus is deliberately lossy (spec §6: bounded per-subscriber queue,
drop-oldest, `Dropped{count}` notice) — an event-sourced flock view WILL
drift. Model: `BTreeMap<u32, SheepRow>` keyed by sheep id.
`BusEvent::Process{info, ..}` upserts (it carries a full `ProcessInfo`
snapshot at event time — no partial-update merge problem);
`process.delete` removes. Every `POLL_INTERVAL_MS` (2 s) a `ListFlock`
snapshot **replaces the map wholesale** — poll wins every conflict.
`Dropped` and reconnect each fire an immediate out-of-band `RefreshFlock`
(and insert a "bus dropped N events" marker line in the bleats feed).
Same pattern the bark dog uses (§8: "subscribes process.* + polls state as
reconciliation") — one house answer to a lossy bus.
Selection survives replacement because App stores `selected_id: Option<u32>`,
never a row index; the view derives the index against the sorted+filtered
rows each frame and writes it into `TableState` (which App retains only for
its scroll `offset`). Bleats feed resolves `id → name` via the flock map;
a line arriving before the first snapshot renders as `#id` until resolved.

### D4 — What the client lib must expose (cross-phase constraint on shep-client)

`shep-client` is a stub today. lookout needs, and the plan for the client
phase must provide:

```rust
async fn list_flock(&mut self) -> Result<Vec<ProcessInfo>, ClientError>;
async fn describe(&mut self, SelectorSpec) -> Result<Vec<ProcessInfo>, ClientError>;
async fn subscribe(&mut self, topics: &[&str]) -> Result<EventSub, ClientError>;
// EventSub: NAMED public stream struct (IR-15), Item = ClientEvent
enum ClientEvent { Bus(BusEvent), Disconnected, Reconnected }
```

The load-bearing part is `ClientEvent` wrapping connection state: spec §6
puts reconnect (100 ms ×1.5, cap 5 s) inside the client, so the TUI can only
show a "reconnecting…" banner — and re-poll + re-subscribe on recovery — if
the stream *surfaces* those transitions as items. If the client phase ships
a bare `Stream<Item = BusEvent>`, lookout has no reconnect UX and the
UX-surface phase gets blocked on a client API change. Flag this in the
client phase's spec.
lookout subscribes `["process.*", "log.out", "log.err", "daemon.*"]`.

### D5 — Input modes and filter semantics. → Two-mode keymap; tui-input for the line editor; filter = case-insensitive substring over name + fold

`InputMode::{Normal, Filter}` × `Focus::{Flock, Bleats, Detail}` (host strip
is not focusable). Normal: `q` quit, `Tab`/`BackTab` cycle focus, `j/k/↑/↓`
select, `g/G` home/end, `/` → Filter mode, `f` toggle bleats follow, `Esc`
clear filter. Filter mode: tui-input owns the line; `Enter` applies, `Esc`
cancels; filter narrows the flock table AND the bleats feed (by sheep) live.
Substring beats regex for v1 (KISS; selectors' `/regex/` grammar can arrive
later as a `/`-prefixed filter without breaking anything). Keyboard only in
v1 — no mouse capture (one less terminal state to restore, and scroll-wheel
support drags in kitty-protocol edge cases; revisit on demand).

### D6 — Terminal restore on panic/error. → ratatui 0.30 `init()`/`restore()` + error-path restore in `run`

`ratatui::init()` enters raw mode + alternate screen AND chains a panic hook
that restores the terminal before the default hook prints — the
cooked-terminal-garbled-panic problem is solved upstream in 0.30. Our
obligation is the *error* path: `lookout::run` catches `Err`, calls
`ratatui::restore()` (idempotent), then returns the error for normal CLI
rendering. No custom hook machinery, no color-eyre. One subtlety: install
nothing before `init()` that could panic while raw mode is half-entered.

---

## 4. Testing strategy (IR-33..39, spec §12)

- **Reducer unit tests (bulk of coverage).** `App::update` is sync + I/O-free:
  feed literal `Msg` sequences, assert state. Targets: selection survives
  wholesale snapshot replace (by id, not index); delete of the selected sheep
  moves selection sanely; `Dropped` yields `Effect::RefreshFlock` + marker
  line; follow-mode pins to tail until user scrolls, resumes on `G`/`f`;
  filter narrows both panes; unknown-id log lines render `#id` then resolve.
- **Frame snapshots: `TestBackend` + insta.**
  `Terminal::new(TestBackend::new(80, 24))` → `terminal.draw(|f| view::draw(&mut app, f))`
  → `insta::assert_snapshot!(terminal.backend())` (TestBackend's `Display`
  renders the buffer as text). One snapshot per pane state that matters:
  empty flock, populated+selected, filtered, errored sheep styling,
  reconnecting banner, drop-marker in feed. NOTE: these are UI snapshots,
  NOT wire fixtures — re-accepting after a deliberate layout change is fine
  (contrast IR-35, where re-accept is forbidden). Say so in a comment at the
  snapshot module top so nobody applies wire discipline to a border glyph.
- **Paused-clock sequence tests (IR-33/36).** `#[tokio::test(start_paused = true)]`
  over the event loop with the fake `DaemonHandle`: assert poll instants land
  at the pinned array `[2 s, 4 s, 6 s]`; a `Dropped` at t=2.5 s inserts an
  immediate poll; redraw coalescing draws ≤ ⌈burst/MIN_REDRAW_MS⌉ frames for
  a 500-line log burst (count `draw` calls via the fake terminal).
- **Fixtures (IR-33/34).** shep-cli crate-root `#[cfg(test)] mod test`:
  `sample_flock()`, `bus_exit(id)`, and a two-tier fake `DaemonHandle` —
  `const_handle(flock)` / `script_handle(vec![...])` mirroring the daemon's
  `const_proc`/`script_proc` convention. Hand-rolled, no mock crates. Unique
  literals per test.
- **Property tier (IR-37).** proptest: arbitrary `Msg` interleavings →
  reducer never panics; `selected_id` always ∈ flock ∪ {None}; ring len ≤
  `BLEAT_RING_CAP`; follow-mode invariant (offset == tail ⇔ following).
  Case count env-capped in CI.
- **Boundary sweeps (IR-40).** Render at 0/1/200 sheep; terminal sizes
  20×5 (degenerate), 80×24, 250×60 — draw must not panic, panes must clamp.
  Filter matching nothing. Bleat line wider than the pane.
- **E2E (IR-39).** assert_cmd only for the non-interactive edges:
  `shep lookout` with no daemon and no spawn permission → typed error, exit
  code, NOT a hang; `shep lookout --help` snapshot; `dash` alias resolves.
  No PTY harness (portable-pty/expectrl rejected: flaky in CI, and
  TestBackend already exercises every render path headlessly — a PTY test
  would only re-verify crossterm itself).
- **IR-38 compile-only test:** not applicable — `DaemonHandle` is
  crate-internal, not public API.

---

## 5. Eventual plan — task list (titles only)

- ~~Decide + land workspace MSRV 1.85 → 1.88~~ — DONE 2026-08-07, dep set unblocked
- Add lookout deps to workspace + shep-cli (ratatui/crossterm/sysinfo/tui-input, IR-2 features)
- lookout scaffold: module tree, terminal lifecycle, panic/error restore
- Msg/Effect vocabulary + App skeleton + crate-root test fixture module (fake DaemonHandle)
- Event loop: select! merge, dirty-flag redraw gate, heartbeat, paused-clock loop tests
- Flock map reconciliation: bus upsert + snapshot replace + Dropped repair (reducer tests)
- Flock table pane: TableState, selection-by-id, sort, status styling + frame snapshots
- Bleats ring + feed pane: follow mode, drop markers, id→name resolution
- Sheep detail pane: describe-on-selection effect + cadence
- Host usage strip: sysinfo sampler task (spawn_blocking) + gauges/sparkline
- Filter input: tui-input wiring, mode keymap, dual-pane narrowing
- Status bar: mode/keys/conn banner; reconnect re-poll + re-subscribe
- proptest reducer invariants + boundary render sweep
- E2E: no-daemon error path, --help snapshot, dash alias
- shep-client phase amendment: EventSub yields ClientEvent (conn transitions) — cross-phase dependency
- Docs: lookout module decision-guide doc + keybinding reference (IR-27)

---

## 6. Open questions for the maintainer

1. ~~MSRV bump 1.85 → 1.88 — yes/no (fallback island documented in §1).~~
   **RESOLVED 2026-08-07: bumped.** serde-saphyr forced it before this phase
   was reached, so the fallback island in §1 is moot — take ratatui 0.30.2 +
   sysinfo 0.38.4.
2. Should filter accept the CLI selector grammar (`/regex/`, `fold:x`) in v1,
   or is substring enough until someone asks?
3. Host strip: cpu+mem+load only, or also per-sheep cpu/mem sparklines in the
   detail pane (needs daemon-side metrics in `Describe` — protocol growth)?
