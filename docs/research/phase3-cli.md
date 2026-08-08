# Phase 3 CLI — design research

Status: research for Rin's review · 2026-08-08
Scope: `shep-client` + `shep-cli`, wiring the 9 daemon-implemented verbs
(`Ping`, `ListFlock`, `Describe`, `Start`, `Stop`, `Restart`, `Delete`,
`Subscribe`, `KillDaemon`) plus hidden `shep daemon`. No code in this doc —
recommendations + reasoning + rejected alternatives, one section per question.

Inputs read: [shep-v1.md](../specs/shep-v1.md) §5/§9/§10/§13,
[idiomatic-rust.md](../idiomatic-rust.md) (IR-1..45),
[terminology.md](../terminology.md), project `CLAUDE.md`, the merged
`shep-core`/`shep-daemon` source (protocol, boot, sys, rpc, server), and
current clap 4 / clap_complete / tokio docs via context7 + docs.rs (dated
2026-08-08; clap_complete 4.6.9 at fetch time).

---

## 1. Command tree shape

**Recommendation:** clap v4 **derive** API, one `Cli`/`Commands` pair in
`cli/mod.rs`, command modules **grouped by theme** (not one-file-per-verb),
global args on the top-level `Cli` struct via a flattened `GlobalArgs` with
`global = true` per field, aliases declared as clap `Command` attributes.
Do **not** pre-stub the ~21 verbs Phase 3 doesn't implement yet — the
bolt-on-without-restructuring property comes from the tree's *shape*
(one enum, grouped modules, explicit global-arg threading), not from
pre-declaring empty subcommands for `lookout`/`whistle`/`serve`/`import`/dogs.

**Reasoning:**

- **Derive over builder.** The whole workspace is derive-first (serde,
  clippy config) and Phase 3's surface (9 verbs, a handful of shared arg
  shapes) doesn't need builder's runtime flexibility. `#[derive(Parser)]` +
  `#[derive(Subcommand)]` + `#[derive(Args)]`, `#[command(flatten)]` for
  shared arg groups (selector, format) — matches map.md's original call
  ("rewrite (clap v4 derive)", map.md:196) and needs no reconciling.
- **Grouped modules, not one-per-verb.** 9 verbs today is too few to justify
  9 files (same "split when it stops paying for itself" logic as this
  project's own >500-line file-split convention). Group by the natural
  lifecycle clusters:
  - `cli/commands/flock.rs` — `start`/`stop`/`restart`/`delete` (+ future
    `reload`/`scale`) and the two read verbs `flock`/`describe`. These share
    one selector-parsing helper and one `ProcessInfo` renderer, so keeping
    them together avoids duplicating that glue across files.
  - `cli/commands/daemon.rs` — hidden `daemon` (foreground boot) + `kill`
    (`KillDaemon`). Both talk to daemon *lifecycle*, not flock lifecycle.
  - `cli/commands/subscribe.rs` — the `Subscribe`-backed primitive (§5
    below); `bleats`'s full log-tail UX (reverse block reader, `LogFormat`)
    is Phase 4 per map.md's `logs.rs` entry, but the streaming *client*
    primitive belongs in Phase 3 since the daemon RPC exists now.
  - Everything else (`lookout`, `whistle`, `serve`, `import`, `dogs`, `fold`,
    `muster`, `signal`/`sendline`/`trigger`, `startup`, `set`/`get`/`unset`,
    `dev`/`runtime`) gets **no file and no enum variant yet**. Adding one in
    Phase 4-6 is: new file in `commands/`, new `Commands` variant, new match
    arm — genuinely additive, nothing to restructure.
  - Rejected: stub every spec §9 verb now so `--help` is "complete" from
    day one — violates KISS/YAGNI (speculative surface for RPCs that don't
    exist), and a stub that can only ever error is worse UX than an absent
    command clap already reports cleanly as unknown.
- **Global args.** Define `GlobalArgs { format: Format, home: Option<PathBuf>,
  quiet: bool, ... }` as `#[derive(Args, Clone)]`, `#[command(flatten)]`ed
  once into `Cli`, each field `#[arg(global = true, ...)]` for **positional
  flexibility** (`shep --format json flock` and `shep flock --format json`
  both parse). Do **not** also flatten `GlobalArgs` into every leaf `Args`
  struct to "read" the value there — that's the known clap-derive footgun
  (a global arg's parsed value only round-trips into structs that declare
  the same field). Instead: parse once (`Cli::parse()`), read `cli.global`
  at the top level, and thread `&GlobalArgs` explicitly into every command
  handler alongside its own leaf `Args`. Simpler than fighting clap's
  propagation model, and every handler's signature documents exactly what
  context it gets.
- **Aliases.** `Command::alias()` is **hidden** from `--help`;
  `Command::visible_alias()`/`visible_aliases()` show up (confirmed via
  clap docs). Per terminology.md's usage rule 1 ("straight verbs are
  first-class aliases forever" — meant to be discoverable):
  - `flock`: `.visible_alias("list").visible_alias("ls")`.
  - `bleats`: `.visible_alias("logs")`.
  - `stop`: `.alias("thatlldo")` — **hidden**, since spec §9 itself calls it
    an "easter-egg alias"; a visible easter egg in `--help` isn't much of
    one. (Judgment call — flagged below, confirm with Rin.)
  - `muster`/`resurrect`: **not** a `Command::alias()` on `muster` — see the
    next bullet.
  - `dev`/`runtime`, `enable`/`disable` etc.: no aliasing needed, land as-is
    when their phase arrives.
- **`muster`'s dual meaning fights simple aliasing.** `shep muster` (bare)
  restores; `shep muster save` saves — modeled as
  `Commands::Muster { #[command(subcommand)] action: Option<MusterAction> }`
  where `MusterAction::Save` is the only variant and `None` means restore.
  Aliasing the whole `muster` *Command* to `resurrect` would make
  `shep resurrect save` parse too, which is wrong — `resurrect` only ever
  restores. Recommend: `resurrect` is its **own hidden top-level
  `Commands` variant** (`.hide(true)`, no subcommand of its own) that calls
  the exact same restore function `muster`'s `None` arm calls. Small
  duplication of a match arm, not of behavior — correct per this project's
  own DRY-serves-KISS ordering (a little repetition beats a wrong shared
  abstraction).
- **Selector parsing as a clap `value_parser`.** `ProcessSelector::parse`
  already exists and is tested (`crates/shep-core/src/selector.rs:31`).
  Wire it as `#[arg(value_parser = parse_selector)]` with a thin adapter
  returning `Result<ProcessSelector, String>` — clap intercepts a `value_parser`
  `Err` and renders it as one of its own usage errors, which (per Q3) already
  exits 2. A bad selector string becoming a *usage* error for free, with zero
  bespoke error-formatting code, is a direct payoff of using the value_parser
  seam instead of validating post-parse.

**Rejected alternatives (one line each):**
- Builder API — no runtime-configured surface exists yet to justify it; pure
  overhead vs. derive for this shape.
- One file per verb — 9 files for 9 verbs fragments shared selector/render
  glue for no cohesion benefit at this size.
- Pre-stubbing all ~30 verbs now — speculative generality; violates YAGNI,
  misleads `--help` users into thinking unshipped verbs partially work.
- Global args flattened into every leaf `Args` struct — hits clap-derive's
  known global-propagation footgun for no benefit over reading `cli.global`
  once at the top.

---

## 2. Daemon autostart

**Recommendation:** the **connect-or-spawn state machine lives in
`shep-client`** (matches map.md's client.rs note: "ping → auto-spawn daemon
→ connect state machine kept", map.md:177), parameterized over a caller-supplied
async "launcher" so shep-client stays free of OS-specific spawn/fd-passing
code. `shep-cli` supplies: (a) the fd-adoption prelude in `main()`, (b) the
`daemon` subcommand handler that calls `shep_daemon::boot`, (c) the launcher
implementation (builds the `Command`, wires the readiness pipe, detaches).
Losing a boot race is handled by **retrying `connect()`**, not by treating
"spawn's child exited/failed" as terminal failure.

**Full sequence:**

1. **`main()`'s literal first statements**, before any tokio runtime exists:
   check `SHEP_READY_FD`. If set, this process *is* the daemonizing child —
   `unsafe { shep_daemon::sys::adopt_fd(fd) }` here, nowhere else. This is
   non-negotiable per `sys.rs`'s own contract (§"code that will fight this"
   below) — `#[tokio::main]` is off the table for this binary's `main`.
2. If `SHEP_READY_FD` is unset: normal foreground invocation. Parse args.
   Any verb but `daemon` needing the daemon calls
   `shep_client::Client::connect_or_launch(paths, launcher)`.
3. **Try `connect()`** (plain UDS dial, not the whole handshake yet — or the
   full handshake, doesn't matter which, as long as failure classification
   below is on the *connect* step specifically):
   - Success → done.
   - `ConnectionRefused` / `NotFound` (no socket, or a stale one — the
     client can't tell the difference and doesn't need to; the daemon's own
     `bind_socket` stale-socket recovery, `boot.rs:335-374`, is what
     actually cleans that up once a daemon wins the race) → **spawn**.
   - Any other IO error (permission denied on the socket path, etc.) →
     hard failure, do **not** spawn (spawning over a permissions problem
     would create a second daemon fighting over `$SHEP_HOME` for no reason).
4. **Spawn:** the launcher (shep-cli-owned) creates a pipe
   (`nix::unistd::pipe()`), passes the write end into the child at a chosen
   fd via `command-fds` (already a workspace dep, used the same way by
   `shep-daemon`'s shepherd channel — `Cargo.toml:77`), sets
   `SHEP_READY_FD=<n>`, builds `Command::new(current_exe()).arg("daemon")`,
   detaches via `tokio::process::Command::process_group(0)` (new pgid = own
   pid, so shell Ctrl-C doesn't reach the daemon — available at the
   workspace's tokio floor 1.40 per root `Cargo.toml`'s own comment on why
   that floor was picked), and null's stdio (or redirects the daemon's own
   tracing output to a file under `$SHEP_HOME/logs/` — flagged as
   underspecified below, since there's currently no daemon-self-log path
   distinct from per-sheep logs).
5. **Await readiness with a timeout**, racing three things via
   `tokio::select!`: the pipe read, `child.wait()`, and a timer. Recommend
   **3000ms** — `boot()` reports readiness (`boot.rs:502-512`, step 3)
   strictly *before* muster restore (step 4), so a slow flock restore never
   inflates this wait; everything readiness gates on (signal install, dirs,
   pidfile flock, socket bind) is fast local syscalls.
   - Pipe yields `DaemonReady{pid, version}` → connect the real socket (one
     retry with short backoff is cheap insurance, though the ordering
     guarantee makes it unlikely to be needed).
   - `child.wait()` resolves first (child exited without writing) → **this
     is the race-loser case, and the fix is: retry `connect()` a few times
     with short backoff (e.g. 3 attempts, 100-300ms apart) before reporting
     failure.** See below for why.
   - Timer fires with neither → one last probe-connect (maybe the pipe
     write just raced oddly), then report `DaemonUnreachable` and best-effort
     kill the orphaned child.
6. **The concurrency requirement** ("two `shep start` calls at once must not
   produce two daemons or one spurious error") is resolved entirely by step
   5's retry-after-failed-spawn, not by any new locking on the client side:
   both CLIs independently see no socket, both spawn `shep daemon`, both
   children race `PidfileLock::acquire` (`boot.rs:190-297`, already
   `flock(2)`-exclusive and proven race-free by
   `two_concurrent_boots_on_a_stale_socket_exactly_one_wins`,
   `boot.rs:1129-1197+`). The **loser's `boot()` returns
   `BootError::AlreadyRunning` before `bind_socket` ever runs** (`boot.rs:452-456`'s
   own doc: "this is the FIRST thing that can fail... before `bind_socket`
   ever runs") — so the losing child exits fast and cleanly. The losing
   CLI's job is just to not treat "my spawned child died" as fatal: it
   retries `connect()`, and by the time it does, the *winner*'s socket is
   very likely already bound (winner's own bind happens right after it wins
   the lock). No new client-side coordination needed — the daemon side's
   existing `flock` already makes this safe; the client side only needs to
   not give up too early.

**Underspecified — flag for Rin:** where does an auto-spawned (detached, no
terminal) daemon's own tracing output go? Per-sheep logs have a defined home
(`$SHEP_HOME/logs/`); the daemon's *own* stdout/stderr today has no
documented destination once detached. Recommend a `shepd.log` under the same
directory, written unconditionally (not just in the auto-spawn case, for
consistency between `shep daemon` run manually vs. auto-spawned) — not
decided here since it's a boot.rs/tracing-init concern, not purely CLI.

**Wire-stability note:** `DaemonReady`'s line is *already* marked wire-stable
(`// wire format: shep-cli parses this line; changing it is a breaking
change`, `boot.rs:386`). If a future revision wants failure detail (not just
success) carried over that same pipe, that's a breaking-format change under
the same IR-35-style discipline as the RPC protocol — not a free CLI-side
add. This design doesn't need it (child-exit-status + retry-connect covers
the failure cases above), so it's not proposed, but flagging it so nobody
extends `DaemonReady` casually later.

**Rejected alternatives:**
- Autostart logic entirely inside shep-cli — leaves every future embedder
  (dogs, a whistle server, TUI) to reimplement the same dance; map.md's
  client.rs note explicitly wants it reusable.
- Client-side mutex/lockfile to prevent the double-spawn race — redundant
  and weaker than the daemon's own `flock(2)`, which already closes the
  race authoritatively; duplicating it client-side is dead code that could
  itself drift out of sync.
- Treating "spawned child exited nonzero" as immediate user-facing failure —
  wrong in the AlreadyRunning race case, which is a *normal*, expected
  outcome under concurrent starts, not an error.

---

## 3. Exit codes

**Recommendation:** a small `#[non_exhaustive]` `ExitCode` enum living in
**shep-cli** (not shep-core — see reasoning), fieldless, `Copy`, mapped from
`RpcErrorCode` via an infallible `From` impl (IR-12: `From` only when
infallible).

```
Success            = 0
GeneralError       = 1   // catch-all / unexpected local failure
Usage              = 2   // clap's own usage-error code; ALSO runtime's fail-fast code (see below)
NoMatch            = 3   // selector matched nothing        (RpcErrorCode::NotFound)
InvalidConfig      = 4   // bad Flockfile/AppConfig, local or daemon-rejected (RpcErrorCode::InvalidConfig)
DaemonUnreachable  = 5   // no daemon, autostart failed, connect() failed post-spawn
ProtocolMismatch   = 6   // version skew                     (RpcErrorCode::ProtocolMismatch)
DaemonInternal     = 7   // RpcErrorCode::Internal / SpawnFailed
DeadlineExceeded   = 8   // RpcErrorCode::DeadlineExceeded
```

**Reasoning:**

- **Home: shep-cli, not shep-core.** Exit codes are an OS-process-exit
  contract; only a *binary* calls `std::process::exit`. shep-core is
  explicitly "shared types... depends on no sibling" and shep-client is a
  library too — neither ever exits a process. Putting `ExitCode` in shep-cli
  keeps the crate boundaries meaning what the architecture table says they
  mean, even though nothing here is a hard compile-time layering violation.
  `impl From<RpcErrorCode> for ExitCode` is legal there (local type is the
  `impl` target, orphan rules are fine) without shep-core needing to know
  the concept exists.
- **`#[non_exhaustive]`, with reason (IR-20).** `RpcErrorCode` is already
  `#[non_exhaustive]` (`request.rs:184`) and documented as growing — the
  exit-code enum inherits that growth pressure directly, so this is a case
  where `#[non_exhaustive]` earns its IR-20 "growth is anticipated" bar
  rather than being cargo-culted. Matching over `RpcErrorCode` inside the
  `From` impl needs a wildcard arm (`_ => ExitCode::DaemonInternal`) to stay
  exhaustive against future daemon-side additions — worth a comment at that
  match noting it's intentional, not a forgotten case.
- **The `Usage = 2` collision is real and I recommend accepting it, not
  designing around it.** clap itself always exits 2 on an argv-parse
  failure, and that number is not meaningfully reconfigurable without
  intercepting every clap error path for no real benefit. Spec §9's
  `runtime` section separately specifies "fail-fast exit code 2" for the
  container mode's empty-flock auto-exit — a *different* situation sharing
  the same number. Both really do mean "this invocation didn't do what you
  wanted, check stderr," and matching clap's own convention costs nothing;
  giving `runtime` a different number (say 9) would be a one-line change if
  Rin prefers distinguishability, but I'd default to the collision-is-fine
  reading unless told otherwise. **Flagged explicitly below as a spec
  ambiguity**, since a scriptable caller genuinely cannot tell "bad CLI
  invocation" from "runtime container found nothing to supervise" from the
  exit code alone.
- **Local (pre-RPC) failures need their own home too.** A `CliError` enum in
  shep-cli aggregates `Rpc(RpcError)`, `Connect(shep_client::ConnectError)`,
  `Config(...)` etc., each with its own `exit_code()` — clap's own usage
  errors are mostly intercepted and exited by clap itself before reaching
  this enum at all (Q1's `value_parser` seam is exactly this: selector
  syntax errors never reach `CliError`, clap handles them as usage errors
  directly).

**Rejected alternatives:**
- `ExitCode` in shep-core, next to `RpcErrorCode` — tempting for locality,
  but misassigns an OS-process concept to a crate whose whole point is
  "shared types with no process-exit opinions."
- Giving `runtime`'s fail-fast a distinct code to avoid the clap collision —
  technically cleaner, but spec §9 says "code 2" explicitly; changing it is
  a real (if small) spec deviation, not a free cleanup, so left as a
  flagged option rather than a decision made here.
- `TryFrom<RpcErrorCode>` instead of `From` — wrong per IR-12: the mapping
  is total (every `RpcErrorCode` maps to *some* `ExitCode`), so `TryFrom`
  would just be `From` wearing an unnecessary `Result`.

---

## 4. Output

**Recommendation:** "versioned" = a **top-level envelope with a schema
version**, reusing the wire protocol's own evolution model
(`PROTOCOL_VERSION` + "additive keeps the version, breaking bumps it",
`protocol/mod.rs:21-29`) rather than inventing a second scheme. Output types
live in **shep-cli**, defaulting to the *existing* core wire types
(`ProcessInfo`, `Response`) wrapped in the envelope wherever they already
say what the user wants — new CLI-only structs only where the CLI adds
something the wire type doesn't carry.

**Reasoning:**

- **Envelope shape:** `{"schema_version": 1, "command": "flock", "data": <payload>}`
  (exact field names TBD at implementation time) — one version number for
  the whole CLI JSON surface, not per-command versions. A consumer branches
  on `schema_version` once, not per command; this is the same tradeoff the
  wire protocol already made and it's proven out there.
- **This IS a stability surface, same rigor as IR-35.** Arguably *more*
  exposed than the UDS wire protocol: the socket is same-uid-only and
  low-visibility, while `shep flock --format json` piped into `jq` in
  someone's cron job is exactly the kind of integration point third parties
  actually build on. Recommend the same discipline: byte-fixture tests per
  `Response` variant's JSON rendering (mirroring
  `v1_fixture_still_deserializes`-style tests), committed at the shep-cli
  e2e tier (`assert_cmd`, IR-39) since format is whole-binary behavior, not
  a library type — breaking a fixture = version bump + CHANGELOG entry,
  never a silent snapshot re-accept (IR-35's own rule, ported verbatim).
- **Table/JSON sync is a test, not a review reminder.** The structural risk
  is the human-table renderer silently dropping a field that serde's
  derive-based JSON output includes automatically the moment it's added to
  `ProcessInfo`. Recommend one render function per command that switches on
  `Format` internally (so there's exactly one call site to update, not two
  scattered ones), *plus* a concrete test asserting every field of the
  source struct is either a table column or explicitly, comment-documented
  as excluded — turning "don't forget the table" into something CI catches
  (IR-40's "boundary sweeps as a habit," applied to struct-field coverage
  instead of byte offsets).
- **stdout vs. stderr:** the actual requested data (table or JSON) is the
  *only* thing on stdout; everything else (progress, warnings, human-legible
  errors) goes to stderr, `--format` or not — keeps `shep flock --format
  json | jq` safe from interleaved noise. `--format json` errors are still
  rendered as a JSON object (so a script parsing errors has one shape to
  target) but **to stderr, not stdout** — stdout stays "only ever the
  success payload, or nothing," which is the simpler invariant for pipe
  consumers, and the exit code (Q3) is already the primary machine-readable
  error signal.
- **IR-41 and `--with-env` don't currently connect to anything.** See "code
  that will fight this" below — this is real missing wire surface, not a
  CLI rendering detail.

**Rejected alternatives:**
- Per-command JSON versions instead of one global schema version — more
  fine-grained in theory, but multiplies the version-tracking surface for
  little payoff at 9 commands, and breaks the "branch once" property that
  makes the wire protocol's single version pleasant to consume.
- Structured JSON errors on stdout — doubles the schema stdout consumers
  must handle (success-shape vs. error-shape) for a distinction the exit
  code already communicates for free.
- Output types living in shep-core (so shep-client/embedders get them too)
  — most of the value (`ProcessInfo`, `Response`) is already there; a
  *rendering* envelope with CLI-only presentation fields belongs with the
  thing that renders, not the wire crate that has no concept of "table."

---

## 5. Streaming verbs

**Recommendation:** a **named `Stream` struct** (IR-15: named public
streams, not `-> impl Stream`), `Client::subscribe(topics) -> Result<EventStream, ClientError>`,
backed by a single **connection-actor task** per `Client` that demultiplexes
`ServerFrame::Reply` (routed to a pending-request table by envelope id) from
`ServerFrame::Event` (forwarded into the stream's channel) — mirroring
exactly what the daemon already does on its side of the same connection.

**Reasoning:**

- **Why a Stream, not a callback.** The Global-Constraints rule this
  project already applies at `rpc.rs:145-146` and `runner.rs:103` ("any
  tokio trait method a spawned task calls needs an explicitly `Send`
  future... `+ Send` on the future bound... rather than relying on
  inference through a trait object") would bite a callback-based design
  immediately: an async callback trait needs RPITIT with an explicit
  `+ Send` bound, since `async fn` in a trait (AFIT) gives the caller no way
  to state that. A `Stream` sidesteps the whole question — `poll_next` is
  plain, no async-trait desugaring involved — which is both simpler and the
  more idiomatic fit per IR-15 anyway. **Note for accuracy:** the task
  brief cited this as "IR-9" — the *actual* numbered IR-9 in
  `idiomatic-rust.md` is the unrelated `clippy.toml doc-valid-idents` rule
  (`idiomatic-rust.md:38`). The RPITIT-`+Send` rule is real and already
  load-bearing in this codebase, but it lives in the phase plans' "Global
  Constraints" sections (`docs/writing-plans/plans/2026-08-07-shep-phase2b-daemon-plane.md:31`),
  not as a numbered IR rule today — flagged as a citation to fix, not a
  design problem.
- **Why one connection-actor, not a stream-per-request-plus-separate-
  event-connection.** The daemon's own design interleaves `Reply` and
  `Event` on the *same* connection after `Subscribe` (`ServerFrame`'s
  untagged decode, `frame.rs:27-32`; proven end-to-end by
  `subscribe_streams_only_matching_events`, `server.rs:559-623`) — ordinary
  requests keep working after subscribing. The client should mirror that:
  one persistent connection actor per `Client`, so `client.list_flock()`
  and an active `subscribe()` stream compose naturally on one connection.
  This also reinforces IR-30's doctest guidance to "reuse one Client" — the
  actor task *is* the reusable resource that guidance is protecting.
- **Second `subscribe()` call replaces the first, matching the daemon.**
  Server-side, "a second Subscribe REPLACES the first" is explicit
  (`server.rs:360-363`: one topic list per connection, not a growing
  union). The client should make the *first* `EventStream` visibly end
  (close its channel so `poll_next` returns `None`) the moment a second
  `subscribe()` runs — a clear, testable contract, not a silent second
  writer. Callers wanting two independent live subscriptions need a second
  `Client`/connection, not two logical subscriptions over one.
- **Ctrl-C.** `tokio::signal::ctrl_c() -> io::Result<()>` (portable
  unix+windows, confirmed via tokio docs) in a single `tokio::select!`
  alongside `stream.next()` in the CLI's follow loop. No daemon-style
  "install once, loop forever" complexity is needed here — `ctrl_c()`'s
  documented caveat that it replaces the process's SIGINT disposition for
  its whole lifetime is a non-issue for a short-lived, single-purpose
  follow command that exits the moment the signal fires. On Ctrl-C: break
  the loop, drop the `Client`/stream — no explicit unsubscribe RPC exists or
  is needed, since the daemon already treats a closed connection as
  sufficient cleanup (`converse`'s forwarder-abort-on-every-exit-path
  handling, `server.rs:323-343`).
- **Daemon shuts down mid-stream.** Server-side teardown broadcasts
  `BusEvent::DaemonShutdown` before closing anything
  (`RunningDaemon::run`'s teardown step 3, `boot.rs:718-719`). Recommend
  the connection actor forward that event into the stream like any other
  (so a `--follow` consumer sees an explicit line, not a bare disconnect),
  then close its channel cleanly on the subsequent EOF rather than
  surfacing a raw IO error — `bleats --follow` losing its daemon should
  read as an orderly end-of-stream (exit 0), not a crash.

**Rejected alternatives:**
- Async callback trait — hits the RPITIT+Send requirement for zero benefit
  over `Stream`; extra ceremony, same information.
- Exposing `mpsc::Receiver<BusEvent>` directly instead of a named wrapper —
  violates IR-15 (unnamed/foreign type at a public boundary) and leaks
  tokio-specific plumbing where `futures`/`tokio_stream` combinators would
  rather compose over a real `Stream` impl.
- A second, separate connection dedicated purely to events — works, but
  throws away the daemon's own "one connection, interleaved" design for no
  gain, and doubles handshake/auth overhead per `Client`.

---

## 6. Completions

**Recommendation:** ship **static (`aot`) completions only in Phase 3**
(`shep completion <shell>`, `clap_complete::aot::generate`). **Defer dynamic
sheep-name completion** to a later phase — a clear recommendation, not a
hedge.

**Reasoning:**

- **Current API, confirmed via docs.rs (clap_complete 4.6.9, fetched
  2026-08-08):** dynamic completion lives in the `engine`/`env` modules
  (`CompleteEnv`, `ArgValueCompleter`, `CompletionCandidate`), and both are
  **explicitly gated behind the `unstable-dynamic` cargo feature** and
  documented as experimental — this is *not* the stable path clap_complete
  offers; `aot` (formerly `generate`/`shells`, now the stable, non-deprecated
  home) is. The custom completer function is **synchronous**:
  `fn(&OsStr) -> Vec<CompletionCandidate>` — no async, no built-in timeout
  or cancellation. Any "short-timeout daemon query" has to be hand-built
  *inside* that sync function (spin up a throwaway
  `tokio::runtime::Builder::new_current_thread()`, `.block_on(tokio::time::timeout(...))`),
  since clap_complete provides nothing for this itself.
- **"Silent degrade" is concretely:** wrap every failure mode (connect
  refused, IO error, timeout, even runtime-build failure) in a fallback to
  an empty `Vec<CompletionCandidate>` — never propagate an error into the
  shell's completion UI. Recommend a **short (~150-200ms) outer timeout**
  distinct from the RPC's own `deadline_ms` — `rpc.rs`'s `budget()` only
  bounds the *daemon's* processing time once connected
  (`rpc.rs:110-120`), not the client's connect+round-trip time, and its
  5s default (`DEFAULT_DEADLINE_MS`, `rpc.rs:36`) is far too slow for a
  tab-press to feel responsive even if silent-degrade eventually saves it.
- **Why defer, concretely:**
  1. Building on an upstream-`unstable` API for foundational, "shouldn't
     need restructuring" Phase 3 work risks exactly the kind of churn Q1's
     tree design is trying to avoid, in the one place least insulated from
     upstream (a cargo feature flag, not our own module boundary).
  2. It needs a *working, hardened* `Client::connect` short-timeout path —
     Phase 3 is already introducing new, race-sensitive connect-or-spawn
     code (Q2); wiring that same young code into a synchronous, on-every-
     keystroke shell hot path multiplies what's being stabilized at once.
  3. Static completions already deliver most of the value (verb, flag, and
     alias completion — the bulk of what gets tab-completed) with zero
     daemon coupling and zero unstable-feature risk, and are trivial to
     wire alongside the `Commands` enum itself.
  4. Dynamic name completion is a small, strictly additive layer
     (`ArgValueCompleter` attached to one `Arg`) that a later phase can bolt
     on once the connect path has real production mileage — consistent with
     this doc's Q1 stance of not building ahead of what's proven needed.
- Recommend the Phase 3 plan **name this deferral explicitly** as a Phase
  4+ follow-up rather than letting it silently drop off spec §9's list.

**Rejected alternatives:**
- Build dynamic completion now behind `unstable-dynamic` — technically
  possible, but stacks upstream-experimental risk on top of freshly-written
  connect-or-spawn code, for a feature that's genuinely additive later.
- Roll a hand-written dynamic completion mechanism instead of
  clap_complete's (e.g. a custom `COMPLETE=` env shim) — reinvents exactly
  what `engine`/`env` already does, for a stability profile no better than
  just using the unstable feature directly.

---

## Spec §9 gaps / contradictions found

1. **Exit code 2 collision** (Q3): clap's own usage-error exit code and
   spec's explicit `runtime` fail-fast code are both literally 2, but mean
   different things a caller can't distinguish by exit code alone.
   Recommend accepting it (documented reasoning above); flagged as a
   decision needing an explicit yes rather than an assumption.
2. **`--with-env` has no wire surface to hang off.** `ProcessInfo`
   (`request.rs:96-111`, the type `ListFlock`/`Describe`/`Start`/etc. all
   return) carries no `env` field at all. `AppConfig`'s IR-41 redaction
   (manual `Debug`, exact-string-tested, `config/app.rs:148`) exists at the
   *config* level, not the runtime-status level the CLI's read verbs
   actually use. `--with-env` therefore needs new, additive wire work
   (`ProcessInfo.env: Option<BTreeMap<String,String>>`, populated only on
   request) — this is shep-core + shep-daemon work the Phase 3 plan needs
   to sequence, not a pure shep-cli rendering flag.
3. **`reload` and `scale` are listed as core verbs in spec §9's flat list**,
   but the daemon's `Request` enum has no `Reload` (and no scale-adjustment)
   variant yet — consistent with the Phase 2a plan's explicit deferral
   ("reload execution... Phase 4; `ReloadState` lands as data only"), but
   someone reading §9 in isolation could reasonably assume both ship
   alongside `start`/`stop`/`restart` in Phase 3. Recommend the Phase 3
   plan state explicitly that these two CLI slots are **not** wired this
   phase (no RPC to call), to head off over-building.
4. **`thatlldo`'s hidden-vs-visible status is unstated.** Spec §9 calls it
   an "easter-egg alias" but doesn't say hidden; the task brief's own
   phrasing calls out only `resurrect` as explicitly hidden. Recommendation
   above (hidden) is a judgment call, not a spec fact — flagged for
   confirmation.
5. **The readiness-pipe line (`DaemonReady`) is already wire-stable**
   (`boot.rs:386`) even though it's not part of the client<->daemon UDS
   protocol proper — a detail easy to miss since it lives next to `boot()`
   rather than in `protocol/`. Any Q2-adjacent design that wants to carry
   more than success over that pipe inherits the same fixture+CHANGELOG
   obligation as the RPC wire types.

## Code that will fight these designs

1. **`sys.rs`'s `adopt_fd` ordering contract is a hard constraint on
   `main()`'s shape**, not a suggestion: its own doc names "the CLI's
   `main`... its literal first fd-touching statement, before a tokio
   runtime — or anything else — exists" as the intended caller
   (`sys.rs:23-24`, `123-133`). This rules out `#[tokio::main]` on
   `shep-cli`'s `main` outright — the attribute builds the runtime (and its
   poller fds) before your function body runs, which is precisely the
   ordering the safety contract forbids. The project has already hit the
   failure mode this guards against once (`sys.rs`'s own essay, scenario
   (c): an earlier `boot()` revision adopted post-bind and closed its own
   listener). The Phase 3 plan should state "no `#[tokio::main]`, manual
   `Runtime::build()` after the fd check" as an explicit rule, not leave it
   for an implementer to discover from `sys.rs`'s doc.
2. **`shep-cli`'s `Cargo.toml` has zero dependencies today** — no `clap`,
   `tokio`, `command-fds`, or `nix`. All of it needs adding with the
   workspace's existing IR-2 discipline (`default-features = false` +
   enumerated features + a `# Option:` comment per line), including
   deliberately *not* adding `tokio`'s `macros` feature for `#[tokio::main]`
   convenience, per point 1.
3. **Root `Cargo.toml`'s comments on `nix` and `command-fds` currently say
   "Only shep-daemon"** (`Cargo.toml:73`, `:77`). Q2's design needs both in
   `shep-cli` too (pipe creation + fd-passing for the readiness handshake) —
   those comments go stale the moment shep-cli depends on either, and
   should be corrected in the same change that adds the dependency, not
   left to drift.
4. **`RpcServer::serve` has no post-handshake idle timeout and no
   per-connection-count cap, by explicit design** (`server.rs:97-104`,
   "Explicit non-goals"). A `bleats --follow` client killed with `SIGKILL`
   (not Ctrl-C) leaves its server-side connection task running until the OS
   eventually errors the write — not fixable from the client side, and not
   this phase's problem to fix daemon-side either, but the Q5 follow-command
   design shouldn't assume the server will notice and clean up after a
   client that exits uncleanly; it should always close its own connection
   deliberately on every exit path it controls.
5. **Nothing here actively fights Q1/Q3's `#[non_exhaustive]` reliance** —
   `Request`/`Response`/`RpcErrorCode` are already `#[non_exhaustive]`
   (`request.rs:52`, `117`, `184`), which is exactly what lets the exit-code
   mapping and the "bolt on new verbs later" command tree work without
   forcing a breaking change on either side. Noted as a place the existing
   design already supports Phase 3 cleanly, not a fight.
