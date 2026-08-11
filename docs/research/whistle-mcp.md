# whistle — MCP server design notes (the UX-surface phase)

Research date: 2026-08-07 (network: live crates.io + docs.rs). Ground truth:
spec §9 (whistle tool set + gating), §2 (stdio v1.0, HTTP/SSE v1.1), §3
(whistle lives in shep-cli), §6 (wire protocol the whistle consumes), §8
(barks.jsonl is list_barks' data source), §10 (security premises), §14.7
(control gating via daemon config, not CLI flag). IR rules cited inline.

## 1. Crate choice: rmcp — confirmed, with one MSRV catch

**rmcp 3.1.2** (official MCP Rust SDK, modelcontextprotocol/rust-sdk).
Published 2026-08-07 (today), release cadence weekly, 3.0.0 landed 2026-07-28
after a beta series — actively maintained, post-1.0 API. **MSRV 1.88** (vs
our workspace 1.85 — see D1). Spec §9 already names rmcp; survey confirms it.

**API shape (3.x):**
- Server = plain struct + `#[tool_router]` impl block; each tool is a method
  with `#[tool(name = "...", description = "...")]`.
- Params: one arg `Parameters<T>` where `T: Deserialize + schemars::JsonSchema`
  — input schema derived, no hand-written JSON schema.
- Outputs: `Json<T>` (T: `Serialize + JsonSchema`) → MCP `structuredContent`
  + output schema; or `Result<CallToolResult, ErrorData>` for manual control.
- `#[tool_handler] impl ServerHandler for X {}` wires tools/list +
  tools/call; `#[tool_router(server_handler)]` shortcut emits both.
- **Composable `ToolRouter<S>` values** — routers from separate impl blocks
  (`#[tool_router(router = control_router)]`) combine with `+`. This is the
  gating mechanism for D2: build the router conditionally at startup.
- Trait alternative for big tools (`ToolBase`/`AsyncTool` +
  `ToolRouter::new().with_async_tool::<T>()`) — overkill for 9 small tools;
  macro form wins (KISS).
- Stdio: `let running = server.serve(stdio()).await?; running.waiting().await?;`
  (`stdio()` = tokio Stdin/Stdout pair; feature `transport-io`).
- Tool annotations supported: `readOnlyHint` / `destructiveHint` /
  `idempotentHint` — set them (agents use these).

**Features to enable** (IR-2: `default-features = false`, enumerate):
`server` (pulls schemars + transport-async-rw + uuid), `macros`,
`transport-io`. v1.1 adds `transport-streamable-http-server`. Skip `base64`
(default, only needed for HTTP session things), `auth`, `elicitation`.
rmcp's own transitive style is looser than ours (tokio/thiserror/tracing with
defaults) — its call, not a violation of our IR-2.

**Fallbacks if rmcp had been immature (it isn't):**
- `rust-mcp-sdk` 1.0.1 (2026-07-26) — community, schema-generated types,
  active; viable plan B.
- Hand-rolled: MCP stdio is JSON-RPC 2.0 over line-delimited stdio; we
  already own serde + framing discipline. ~600 lines for
  initialize/tools/list/tools/call. Plan C, only if the SDK fights us.
- Dead on arrival: `mcp-attr` (0.0.7, stale 2025-05), `mcpr` (0.2.3, stale),
  `mcp-sdk` (0.0.3, stale), `poem-mcpserver` (framework-tied).

## 2. Version pins (crates.io, verified today)

| crate | pin | note |
|---|---|---|
| rmcp | `3.1.2` | `default-features = false, features = ["server", "macros", "transport-io"]`; MSRV 1.88 |
| schemars | `1.2.2` | rmcp `server` re-exports it; same crate serves spec §5 AppConfig schema export — one workspace pin |
| insta | already `1.18.1` in workspace | tool-schema snapshots |
| assert_cmd | `2.2.2` | e2e (IR-39), if not already pinned by an earlier phase |

No new transport/framing deps: whistle talks to the daemon via shep-client.

## 3. Module shape

```
crates/shep-cli/src/whistle/
├── mod.rs        # pub fn run(args) -> ExitCode: load config, build router,
│                 #   serve(stdio()), waiting(). Transport setup lives ONLY here
│                 #   (v1.1 HTTP = second arm in this file, tools untouched).
├── server.rs     # WhistleServer struct (holds SHEP_HOME paths + allow_control
│                 #   + client factory); #[tool_handler] ServerHandler impl
│                 #   (name "shep-whistle", version, instructions text).
├── tools_read.rs # #[tool_router(router = read_router)]: list_flock,
│                 #   describe_sheep, get_metrics, tail_bleats, list_barks.
├── tools_ctl.rs  # #[tool_router(router = control_router)]: start_sheep,
│                 #   stop_sheep, restart_sheep, reload_sheep. Each handler
│                 #   re-checks allow_control (defense in depth, D2).
└── types.rs      # Parameters/Output structs, all Deserialize/Serialize +
                  #   JsonSchema; sheep→wire SelectorSpec conversion (IR-14
                  #   flavor: accept name | id | "all" | fold:<n> as one field).
```

shep-core change: add `WhistleSection { allow_control: bool /* default false */ }`
to `DaemonConfig` (new `[whistle]` table in shep.toml, `deny_unknown_fields`
like `DaemonSection`). Env override `SHEP_WHISTLE_ALLOW_CONTROL` follows the
existing `load()` layering.

Logging discipline: stdout is the MCP wire — **nothing but JSON-RPC on
stdout**; tracing subscriber to stderr only. Worth a one-line test (spawn,
assert stdout's first byte is `{`).

## 4. Tool → shep-client mapping

| MCP tool | daemon interaction | notes |
|---|---|---|
| `list_flock` | `Request::ListFlock` → `Response::Flock` | output = Vec<ProcessInfo-shaped struct>; include dogs with a `dog: bool` badge (agents should see them; mirrors `--all`) |
| `describe_sheep` | `Request::Describe{selector}` | selector param per types.rs; include fold, lambs tree when Describe grows it |
| `get_metrics` | new additive `Request::Metrics{selector}` (see D4) | cpu/mem/restarts/uptime per sheep + daemon self stats |
| `tail_bleats` | the same RPC `shep bleats` uses (planned `TailLogs{selector, lines}`) | non-streaming: last N lines (default 100, cap ~1000), stdout+stderr interleaved with stream tags; NO follow mode in v1 (MCP tools are request/response) |
| `list_barks` | none — read `$SHEP_HOME/barks.jsonl` directly | spec §8 mandates the file as the data source; tail-N param; tolerate torn last line (ring-capped file) |
| `start_sheep` | `Request::Start{apps}` or start-by-name of a registered stopped sheep | gated |
| `stop_sheep` | `Request::Stop{selector}` | gated; `destructiveHint = true` |
| `restart_sheep` | `Request::Restart{selector}` | gated |
| `reload_sheep` | `Request::Reload{selector}` (lands with the reload phase) | gated |

All five read tools: `readOnlyHint = true`. Tool descriptions must decode the
theme for agents ("List the flock — all processes managed by the shep
daemon") — terminology.md rule 4/5: names stay themed (spec'd), descriptions
stay precise.

## 5. Hardest design decisions

**D1 — MSRV: rmcp needs 1.88, workspace said 1.85 (IR-4). RESOLVED
2026-08-07 — option (a) taken, and forced earlier than this doc expected:
serde-saphyr's let-chains made 1.88 a present-tense requirement, not a
future one. Workspace `rust-version` and the CI matrix are both 1.88. See
`refactor-workspace/decision-briefs.md`. Kept below for the reasoning.**
Options: (a) bump workspace MSRV to 1.88; (b) bump only shep-cli's
`rust-version`. **Recommend (a).** The shipped artifact is the shep-cli
binary, so the effective MSRV is 1.88 either way; a split MSRV is a fiction
that complicates the CI matrix (IR-44 MSRV row) for zero user benefit. 1.88
is >1 year old at ship time. One-line change + CI pin bump + idiomatic-rust.md
edit, done in the PR that introduces rmcp. Flag to Rin in that PR.

**D2 — Control gating: absent vs erroring, and who reads the flag.**
**Recommend: tools absent from `tools/list` when `allow_control = false`.**
Mechanism: `read_router() + (allow_control).then(control_router)` composed
once at startup. Rationale: agents plan from tools/list; a listed-but-refusing
tool burns agent turns on guaranteed failures and tempts retry loops. A call
to an absent tool gets rmcp's standard "tool not found" error — correct
signal. Defense in depth: each control handler ALSO re-checks the flag and
returns an explanatory error — protects against router-wiring regressions,
costs one branch (test both layers). Flag source: whistle loads
`DaemonConfig::load` itself from `$SHEP_HOME/shep.toml` + env (same shared
loader, no drift-by-reimplementation). NOT a new RPC: allow_control is not a
security boundary (any same-uid process already has full UDS control — spec
§10); it's an agent-capability policy, and the config file is the auditable
source of truth (§14.7). Config edits require whistle restart — document;
MCP `tools/list_changed` hot-reload is a non-goal for v1.

**D3 — Daemon connection lifecycle: per-call connect, never auto-spawn.**
**Recommend connect-per-tool-call** via shep-client (UDS connect + Hello ≈
microseconds; agent call rates are ~seconds). Avoids shared-connection
locking in `&self` handlers, makes tools stateless, and reconnect logic is
free (each call is fresh). Explicitly do NOT use the client's
connect-or-spawn path: an observability surface silently booting a daemon is
a surprising side effect (worse: agent typo → daemon spawned under the MCP
host's environment). Daemon down ⇒ tool error "shep daemon not running —
start it with `shep start …`". Revisit (cache the connection) only if
profiling ever shows it matters.

**D4 — get_metrics source: daemon RPC, not scraping the metrics dog.**
**Recommend a new additive `Request::Metrics{selector}` → `Response::Metrics`.**
The daemon already samples cpu/mem (memory-limit poll, §4) — expose that
snapshot over RPC. Scraping the dog's Prometheus endpoint (9615) would make
whistle break whenever the metrics dog is disabled, add an HTTP client dep,
and force Prometheus text parsing. Wire addition is non-breaking
(`#[non_exhaustive]` enums, additive per §6); coordinate the exact shape with
the metrics-dog phase so both consume one snapshot type.

**D5 — Error + output surfacing.**
Success: typed `Json<T>` outputs for every tool (structured content + output
schema — agents parse fields, not prose). Failures: domain errors (daemon
down, `NotFound`, `InvalidConfig`) → `CallToolResult` with `is_error = true`
and an actionable plain-English message (agent-visible, recoverable);
programmer errors (serialization bugs) → protocol-level `ErrorData::internal`.
Map `RpcErrorCode` → message verbatim-ish; never leak env values (redaction
already handled daemon-side, §10 — whistle must not add a `--with-env`
equivalent in v1).

## 6. HTTP/SSE v1.1 implications (design now, build later)

rmcp's `transport-streamable-http-server` feature serves the SAME
`ServerHandler` — tool layer is transport-agnostic by construction, so the
only v1.1 work is in `whistle/mod.rs`: a second transport arm
(StreamableHttpService + session manager) behind `shep whistle --http` or
`[whistle] http_bind`. Design consequences to honor NOW:
1. No stdio assumptions inside tools (no stdin reads, no process-exit
   coupling) — transport setup confined to `mod.rs`.
2. HTTP crosses the §10 trust boundary (UDS peer-cred no longer vouches for
   the caller): bind 127.0.0.1 by default, plan a bearer-token config key;
   leave `[whistle]` room for `http_bind` / `http_token` (don't add fields
   yet — `deny_unknown_fields` means adding later is clean).
3. Multi-session: per-call daemon connections (D3) already make concurrent
   MCP sessions safe — no shared mutable state to retrofit.

## 7. Testing strategy (IR-33..39)

- **Tool-surface snapshots (the IR-35 analogue):** insta JSON snapshot of
  `read_router().list_all()` + `control_router().list_all()` — names,
  descriptions, input/output schemas. The MCP tool surface is a public
  contract; schema drift must be a deliberate snapshot review, never silent.
- **Handler units (IR-33):** handlers call the daemon through a small trait
  (`FlockApi`: list/describe/metrics/tail/start/stop/restart/reload) owned by
  whistle; real impl = shep-client, test impl = hand-rolled scripted fake (no
  mock frameworks). Unit-test each tool: happy path (typed output fields),
  daemon-down (is_error + message), NotFound mapping.
- **Gating tests (D2 both layers):** allow_control=false → exactly 5 tools
  listed, control call → tool-not-found error (snapshot the wire error);
  allow_control=true → 9 tools; handler-level re-check tested by wiring the
  control router with the flag forced false in the fake server.
- **list_barks file tier (IR-34):** own tempdir per test — empty file,
  N-line file, torn final line, missing file (⇒ empty list, not error).
- **E2E (IR-39):** assert_cmd spawns `shep whistle` with fresh `$SHEP_HOME`;
  speak real JSON-RPC over stdio: `initialize` → assert serverInfo,
  `tools/list` → insta snapshot, `tools/call list_flock` with no daemon →
  is_error message. One live-daemon e2e (harness from earlier phases) doing
  list_flock + stop_sheep-with-gate-on. Event-driven waits, no sleeps.
- **Stdout purity test:** spawned whistle writes nothing but JSON-RPC to
  stdout (tracing on stderr).
- Paused-clock (IR-36) mostly N/A — whistle owns no timers; RPC deadline
  behavior is shep-client's test surface.

## 8. Eventual plan — task list (titles only)

- Bump workspace MSRV 1.85 → 1.88 (rmcp); update IR-4 + CI pins
- Add `[whistle]` section (`allow_control`) to DaemonConfig + env layering
- Add rmcp + schemars workspace pins (features enumerated, IR-2/3 comments)
- `Request::Metrics` / `Response::Metrics` wire pair + fixtures + snapshots (IR-35)
- whistle module skeleton: run(), WhistleServer, ServerHandler info + instructions
- FlockApi trait + shep-client impl + scripted test fake
- Read tools: list_flock, describe_sheep (typed outputs + schemas)
- get_metrics tool over Metrics RPC
- tail_bleats tool over the bleats RPC (dep: logs phase)
- list_barks tool (barks.jsonl reader + torn-line tolerance)
- Control tools behind composed router + handler-level re-check
- Tool-surface insta snapshots (read + control routers)
- Gating unit tests (list shape + not-found + re-check layer)
- Stdio serve loop + stderr-only tracing + stdout purity test
- `shep whistle` clap subcommand wiring
- E2E: initialize/tools-list/no-daemon error; live-daemon list + gated stop
- Docs: whistle module doc (decision guide per IR-27), SECURITY.md whistle
  paragraph (agent-capability policy vs security boundary), README example
