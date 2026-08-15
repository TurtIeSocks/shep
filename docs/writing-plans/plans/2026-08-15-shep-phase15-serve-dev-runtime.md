# Phase 15 — `serve`, `dev`, and `runtime`

The last three verbs spec §9 names and `docs/specs/deferred.md` still lists as
unbuilt: `shep serve` (a static file server run as a managed sheep), `shep dev`
(an isolated foreground development flock) and `shep runtime` (the foreground,
no-daemon container mode). With them the v1.0 CLI surface is closed except for
the Windows functional tier, which the ledger keeps deliberately last.

This phase is last for a structural reason, not an ordering preference. Spec §3
asks for `shep-runtime` and `shep-dev` as `[[bin]]` aliases for container
entrypoints. `crates/shep-cli` has exactly one `[[bin]]` today and no `lib.rs`,
and Cargo cannot share a module tree between binary targets without a library
crate underneath them. So this phase begins by turning shep-cli into a library
with thin binaries on top — an architectural change nothing else in the project
needs, made once, at the end, when nothing else is in flight to collide with it.

## Prerequisite: this phase starts after 12b, 13 and 14 are merged

Task 1 renames `crates/shep-cli/src/main.rs` to `crates/shep-cli/src/lib.rs`.
Every in-flight branch that touches shep-cli — lookout 12b, whistle 13, config
and packaging 14 — edits that file or adds a `mod` line to it. Starting Task 1
while any of them is unmerged converts a clean rebase into a manual one, on the
one file that dispatches every verb in the binary.

Confirm before Task 1, and do not start on a `no` (each prints a path if that
phase has landed, nothing if it has not):

```bash
git log --oneline -1 -- crates/shep-cli/src/whistle/mod.rs      # Phase 13
git log --oneline -1 -- crates/shep-cli/src/lookout/bleats.rs   # Phase 12b
git log --oneline -1 -- crates/shep-core/assets                 # Phase 14
```

Those three paths are this plan's guesses at each phase's most distinctive
artefact, taken from their own plans. If a path is empty and you believe the
phase landed anyway, check the phase's plan for what it actually shipped rather
than assuming. Phase 14 in particular is severable task by task, so its
artefact may differ; what matters is that no branch with unmerged shep-cli
edits exists, not that these exact three files do.

---

## Baseline

**Do not pin a SHA and do not trust a test count from this document.** Three
phases were in flight while this was written. Two earlier plans on this project
had to be corrected for pinning a tip that moved underneath them; one shipped a
stale test count as if it were a checksum.

Establish ancestry first:

```bash
git merge-base --is-ancestor "$(git log -1 --format=%H -- docs/writing-plans/plans/2026-08-15-shep-phase15-serve-dev-runtime.md)" HEAD; echo "ancestor=$?"
```

`ancestor=0` means the commit that wrote this plan is in your history, so the
content checks below were derived against a tree yours descends from.
`ancestor=1` means a rebase: **re-derive every check in this document before
trusting one**, and say so in the task report.

Then take your own test baseline:

```bash
cargo test --workspace --all-features
```

Write down every result line it prints. That is your baseline; each task states
its delta as an **enumeration of named tests**, never as a total to check off,
and `failed` stays `0` on every line the whole way.

### Content checks, exactly as printed on this machine, 2026-08-15

```bash
grep -cF '[[bin]]' crates/shep-cli/Cargo.toml                      # 1
ls crates/shep-cli/src/lib.rs                                      # No such file or directory
ls crates/shep-cli/src/bin                                         # No such file or directory
grep -c '^pub ' crates/shep-cli/src/main.rs                        # 0 (grep exits 1)
grep -rn "shep-runtime\|shep-dev" crates | wc -l                   # 0
grep -c "Serve\|Dev(\|Runtime(" crates/shep-cli/src/cli.rs         # 0 (grep exits 1)
grep -rn "axum\|tower-http" Cargo.lock | wc -l                     # 0
grep -c '^name = "ring"' Cargo.lock                                # 1
grep -rni "percent_decode\|percent-decode\|percent_encoding" crates | wc -l   # 0
grep -rni "nosniff\|X-Content-Type-Options" crates | wc -l         # 0
grep -c "FlockEmpty\|flock_empty" crates/shep-cli/src/exit.rs      # 0 (grep exits 1)
grep -rn "dog::http\|dog/http.rs" crates | wc -l                   # 6
grep -c "write_response" crates/shep-cli/src/dog/http.rs           # 5
grep -c "request.method" crates/shep-cli/src/dog/metrics/mod.rs    # 0 (grep exits 1)
grep -c "not built" crates/shep-cli/src/main.rs                    # 2
grep -c "resolves that" docs/specs/shep-v1.md                      # 1
grep -cF 'one `[[bin]]`' docs/specs/deferred.md                    # 1
grep -rn "set_child_subreaper" crates | wc -l                      # 0
grep -c "#\[test\]\|#\[tokio::test\]" crates/shep-cli/src/main.rs  # 13
```

**Four of these carry scope or a flag that is load-bearing. Do not drop it.**

`grep -cF '[[bin]]'` — **`-F` is not optional.** Without it, `[[bin]]` is a
grep character class (`[[bin]` matches one of `[`, `b`, `i`, `n`) and the count
is meaningless. This was found while writing this plan: `grep -c "one \`[[bin]]\`"
docs/specs/deferred.md` prints `0` as a regex and `1` with `-F`, and the `0`
reads exactly like "the ledger no longer says that", which is false. Every
check in this document that looks for `[[bin]]` uses `-F`.

`grep -c "request.method"` on the metrics dog prints `0`, and that zero is a
**finding, not an absence** — it is the proof that today's HTTP surface does no
method routing at all (decision 4). It is a baseline for a fact, not for a
change; nothing in this phase makes it non-zero, because `serve` gets its own
handler.

`grep -rn "percent_decode..."` scoped to a pattern, not to the word `percent`:
the bare word prints `115` in `crates`, all of them `cpu_percent`. A check that
counts `cpu_percent` is a check that never moves.

`grep -c "not built" crates/shep-cli/src/main.rs` prints **2** and only one of
the two is this phase's: line 7 is the module doc claiming serve/dev/runtime do
not exist (Task 12 deletes it), line 548 is a Windows named-pipe note that must
survive. A post-task expectation of `0` here would be wrong; the expectation is
`1`.

Several of these exit `1` while printing `0`. That is fine at a prompt and
**fatal under `set -e`**, which is how a dead check got into an earlier phase.
Append `|| true` if you script them.

### The dead-check shapes this project has actually shipped

Five, all found in real plans here. Before writing any check below, state what
it prints **today**:

1. **The pattern that cannot match the real text** — backticks, or a phrase
   wrapped across a line break. `grep -n` the surrounding words and read what is
   there.
2. **The glob whose no-match case errors** — `ls crates/**/*.rs` under zsh fails
   the command rather than printing nothing.
3. **The expectation already true at HEAD** — verifies nothing. Every baseline
   above is printed for this reason.
4. **The bound that is not a bound** — `tokio::time::timeout` around a
   synchronous call bounds nothing, and a harness-level timeout fails the whole
   binary while naming no test.
5. **`grep -rc … | wc -l`** — counts files searched, not matches. Use
   `grep -rn … | wc -l`.

A sixth, specific to this phase: **a security test that passes for the wrong
reason.** A traversal test that asserts "not 200" passes when the server is
simply broken and answers 500 to everything. Every refusal test in Tasks 3 and
6 asserts the **exact status and the exact refusal reason**, and each is paired
with a positive control in the same test that proves the server serves the
legitimate neighbour of the refused path.

---

## What this phase adds beyond spec, and why

Collected here because the "does not build" section at the end lists only
omissions. Each is argued in the decision it belongs to.

- **A new exit code, 11 (`flock_empty`)** — spec §9 hands this phase an
  unresolved collision in as many words and does not say how to resolve it.
  Decision 13.
- **A PID-1 init split** — spec §9 asks for "PID-1 zombie reaping"; map.md
  sketches "subreaper + WNOHANG loop", which cannot work in the supervisor's own
  process. Decision 14.
- **`$SHEP_DEV_HOME`** — spec §9 fixes `~/.shep-dev` and names no override. The
  e2e tier needs one, or `cargo test` writes into the developer's real home.
  Decision 15.
- **`serve --foreground` as a visible flag** — the spec says serve runs as a
  managed sheep and does not say how the sheep is spelled. Decision 11.
- **An access log line per request** — not named in spec §9. **Severable**:
  cutting it costs debuggability and nothing else. Decision 16.
- **`X-Content-Type-Options: nosniff` on every `serve` response** — not named in
  the spec. One header, and without it a docroot holding user-supplied files is
  a stored-XSS surface. Decision 4.

---

## Global constraints

- MSRV 1.88, edition 2024, `MIT OR Apache-2.0`.
- `#![forbid(unsafe_code)]` in shep-core, shep-client and shep-cli; unsafe only
  in `shep-daemon/src/sys.rs` with per-block `// SAFETY:`. Task 1 moves the
  attribute from `main.rs` to `lib.rs` and puts it on each of the three thin
  binaries as well, so no target loses it.
- `PROTOCOL_VERSION` stays **1**. Nothing in this phase touches the wire. If a
  task makes you reach for a new `Request`/`Response`/`BusEvent` variant, stop —
  you have taken a wrong turn. `serve` registers an ordinary `AppConfig` through
  the existing `Request::Start`, and neither `dev` nor `runtime` sends anything
  the CLI does not send today. No `.snap` file changes; if you are editing one,
  re-read the task.
- **IR-20** and its rationale-comment requirement. Task 1 makes shep-cli a
  **library** crate, which invalidates the *stated reason* on
  `ExitCode`'s missing `#[non_exhaustive]` ("this is a binary crate, so there is
  no downstream matcher"). The conclusion survives — `ExitCode` stays private —
  but the sentence must be corrected rather than left standing as a false claim.
  Step 1.7.
- **IR-46**: every `await` in a test needs a forcing mechanism the test itself
  sets. This phase adds more async than any since the log plane. Every accept
  loop test drives a real loopback listener with its own `tokio::time::timeout`
  around the *asynchronous* call (not around a synchronous one — dead-check
  shape 4), and every debounce test runs on a paused clock.
- **The fast loop is `cargo test -p shep-daemon --lib --all-features -- --skip
  ::slow::`**, unchanged; nothing here touches the daemon's own crate.
- **shep-cli's own loop changes in Task 1, and this is the trap that will
  actually bite.** Today `cargo test -p shep-cli --lib` runs *nothing* and
  reports success, so the project's rule is `--bins`. After Task 1 every unit
  test in the crate lives in the library and `--bins` is what runs nothing.
  From Task 1 onward use:

  ```bash
  cargo test -p shep-cli --lib --bins --all-features
  ```

  Both, so the command is correct on either side of the rename and correct
  afterwards. Task 12 corrects `CLAUDE.md`, which states the `--bins` rule as a
  fact about this repo.
- The task gate is fmt, clippy `-D warnings`, `cargo test --workspace
  --all-features`, `RUSTDOCFLAGS="-D warnings" cargo doc`; **one cargo command
  at a time**, `$?` read directly and never through a pipe (in zsh a pipeline's
  `$?` is the last command's and `${PIPESTATUS[0]}` is empty).
- Terminology: the daemon is **the shepherd** and only that; one managed process
  is **a sheep**; the plural is always **the flock**; a sheep's own children are
  **lambs**. Destructive operations and error text stay plain — a 403 says what
  was refused, not what the sheepdog thought of it.

### The exact commands

```bash
cargo test -p shep-core   --lib  --all-features
cargo test -p shep-daemon --lib  --all-features -- --skip ::slow::
cargo test -p shep-client --lib  --all-features
cargo test -p shep-cli    --lib --bins --all-features
cargo test -p shep-cli    --test cli_e2e --all-features
cargo test -p shep-daemon --test daemon_e2e --all-features
```

Task 10 adds one target that only exists on Linux:

```bash
cargo test -p shep-cli --test reaper --all-features
```

Task gate, each from its own command:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Phase cross-checks, once at the merge, each with its own `CARGO_TARGET_DIR`:

```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

**The Windows check matters more this phase than it has since Phase 6, and two
tasks can break it in a way this machine cannot see.**

`crates/shep-cli/src/cli.rs` carries no `cfg` at all — it is the pure tier, and
it compiles on Windows while `mod commands` in `main.rs` does not. Any type
`cli.rs` names must therefore exist on every target. Tasks 7, 9 and 11 add
`ServeArgs`, `RuntimeArgs` and `DevArgs` to that file, so **every field type in
them must be portable**: `String`, `PathBuf`, `u16`, `bool`, `SocketAddr`. Not
`nix::sys::signal::Signal`, not a type from `serve::worker`.

The second trap is the one this plan was warned about by name: **a `const fn`
whose Linux arm calls a non-const method compiles here because the arm is
cfg-ed away.** Task 10 is where that lands — it has a `cfg(target_os = "linux")`
arm for the subreaper test and a `cfg(unix)` reaper. Keep the reaper's decision
logic in one `cfg`-free pure function over an enum the caller constructs, so the
platform arms are two lines each and cannot hide a compile error.

`cargo check -p shep-cli --target x86_64-unknown-linux-gnu` is **not** in the
gate and will fail on this machine inside `ring`'s build script, which runs
`cc` and needs a Linux cross C toolchain. That is the known gap, not a defect;
attempt it, report what happened, and rely on CI's real Linux runner for the
Linux arms. The Windows one needs `brew install mingw-w64` for the same reason
and was last measured green at 8.42s (Phase 10, 2026-08-13).

---

## Design decisions made here, not deferred

Sixteen. The first two are the architectural step that gates the phase; five
through eleven are `serve`, which is almost entirely a security decision;
twelve through fifteen are `dev` and `runtime`.

### 1. The library extraction exposes THREE FUNCTIONS and nothing else

`crates/shep-cli/src/lib.rs` is the crate's whole public API:

```rust
/// Runs `shep` and returns the status the process should exit with.
pub fn main() -> std::process::ExitCode;
/// Runs `shep-runtime`: `shep runtime`, with the verb already supplied.
pub fn main_runtime() -> std::process::ExitCode;
/// Runs `shep-dev`: `shep dev`, with the verb already supplied.
pub fn main_dev() -> std::process::ExitCode;
```

Every module stays private (`mod cli;`, not `pub mod cli;`). No argument
struct, no `Commands`, no `ExitCode`, no `Streams`, no `Client` re-export. The
three functions return **`std::process::ExitCode`**, a type from `std`, so even
the return value adds nothing to the surface — `crate::exit::ExitCode` stays
private and is converted at the boundary.

**The reason is that shep-cli is published.** `docs/releasing.md` was written
on 2026-08-14 with `cargo publish --workspace` and `cargo install shep-cli` in
it. Whatever is `pub` in this crate is therefore a promise to crates.io, and
three things follow that do not follow for an internal library:

- **Semver.** Every `pub mod` is a surface that a downstream `use` can pin. The
  CLI's internals move every phase — `dog/` shipped as a directory where map.md
  said a file, `metrics.rs` became `metrics/`, and this phase moves `http.rs`
  out of `dog/` entirely. Any of those is a breaking change the moment a module
  is public, and none of them is a breaking change to anyone.
- **docs.rs.** The rendered page for `shep-cli` is what somebody sees when they
  look the crate up. With `pub mod` throughout, that page is a tour of the
  binary's internals, and every private-by-intention doc comment in this crate
  (there are hundreds, several of them arguing about locks and signal races)
  becomes published prose. With three functions it is three lines and a link to
  the README, which is what a person looking up a CLI wants.
- **Support surface.** A public `Commands` enum invites "let me embed shep's
  clap tree", and the answer to that request has to be no anyway: the tree is
  `#[cfg(unix)]` in half its dispatch and assumes it owns the process's exit.

The counter-argument, stated so it is not rediscovered: a full surface would
let a downstream tool drive shep in-process without shelling out. That want is
real and shep already answers it — **`shep-client` is the embedding API**, it
is published, it re-exports shep-core, and it is designed for exactly this.
Exposing shep-cli as a second, worse embedding API would be a second promise
about the same capability.

Version 0.1.0-alpha.1 makes a mistake here cheap to correct — pre-1.0 semver
permits breaking on a patch bump. That is an argument for not agonising, not an
argument for publishing a surface we would then have to keep. Widening later is
one line; narrowing later is a yank.

**Guard, and its honest limit.** There is no test that asserts a module is
private: asserting non-compilation needs `trybuild`, a new dev-dependency for
one check. The guard is therefore a grep in Task 1's verification —
`grep -c '^pub ' crates/shep-cli/src/lib.rs` must print exactly `3` — plus one
human read of the rendered `cargo doc` page. Say this limit out loud in the task
report rather than letting the grep read as stronger than it is.

### 2. Three real `[[bin]]` targets, so argv[0] is never read — and the re-exec trap that creates

Spec §3 says "argv[0] dispatch to the same subcommands". That phrase describes
the busybox shape, where one binary is hardlinked under several names and reads
`argv[0]` at runtime to decide what it is. With three real `[[bin]]` targets,
each binary knows what it is **at compile time**:

```rust
// crates/shep-cli/src/bin/shep-runtime.rs
#![forbid(unsafe_code)]
fn main() -> std::process::ExitCode { shep_cli::main_runtime() }
```

No `basename` parsing, no table of known names, and no wrong answer when
somebody copies the binary somewhere under a different name. The user-visible
contract spec §3 asks for is unchanged.

Rejected: **three `[[bin]]` entries sharing `path = "src/main.rs"`**, which
needs no library at all. Cargo permits it, and it compiles this crate's whole
module tree three times and links three full binaries — ratatui, rustls, rmcp
and all. That is a per-build cost on every CI run and every `cargo install`,
paid to avoid a `lib.rs` whose public surface we are choosing anyway.

**Each alias entry point prepends its verb to the argument vector**, so
`shep-runtime ./Flockfile.toml --format json` is parsed as
`["shep", "runtime", "./Flockfile.toml", "--format", "json"]`. Consequence to
document rather than fix: `shep-runtime --help` prints usage lines that say
`shep runtime`. Renaming the clap command per binary would fix the cosmetics
and put a second name into every error message; the prepend is honest about
what it does.

**The trap.** `shep_daemon::dogs` spawns a built-in dog as
`std::env::current_exe() dog <name>` (`crates/shep-daemon/src/dogs.rs:140`),
and `crate::launch::launch_command` spawns the shepherd as
`std::env::current_exe() daemon`. Under `shep-runtime`, `current_exe()` is
`shep-runtime` — so a naive prepend turns `shep-runtime dog metrics` into
`shep runtime dog metrics`, which is a clap error, and the metrics dog dies in
every container that enables it. Nothing in the test suite would notice, because
no test runs a dog through an alias binary.

**The rule, which is a rule and not a heuristic:** an alias prepends its verb
**unless the first argument is exactly `daemon` or `dog`** — the two hidden
re-exec verbs, the two the supervisor spawns by path. Written once, in one
function, with the reason in its doc comment, and pinned by four tests (Step
1.5). It is not a guess about what the user meant: those two argument vectors
are never typed by a human and are constructed in exactly two places in this
workspace, both of which this plan cites by path.

### 3. `serve` is hand-rolled on the metrics dog's HTTP surface. Rin's ruling, and what that surface actually is

**Rin's ruling, binding (2026-08-15):** `serve` is hand-rolled on the HTTP
surface the metrics dog already has, **not** axum, even though spec §9 names
axum and tower-http. Her reasoning: serve is genuinely simple over code we
already own, while an evolving protocol like MCP is worth an SDK — which is why
she upheld the spec's `rmcp` in the same breath as overruling its `axum`.

That ruling settles the dependency question. It does not settle what the
existing surface covers, and the honest answer is: less than the phrase "the
HTTP surface we already have" suggests.

**`crates/shep-cli/src/dog/http.rs` (373 lines) does:**

- reads one HTTP/1.1 request: method, target, headers (names lowercased, values
  trimmed), body to a declared `content-length`;
- bounds it — 8 KiB head, 64 KiB declared body, a caller-supplied read timeout,
  and a `Take` under the `BufReader` so a peer that never sends a line
  terminator fails instead of hanging;
- writes one response — status, `Content-Type`, `Content-Length`,
  `Connection: close`, body — and closes;
- is generic over `AsyncRead`/`AsyncWrite`, so its tests run over
  `tokio::io::duplex` with no socket.

**It does not do, and each of these is a thing a static file server needs:**

- **any URL handling at all.** `target` is an opaque `String`. The query split
  is one `split('?')` in the metrics dog's own handler. No percent-decoding
  exists anywhere in this workspace (`grep -rni "percent_decode…" crates` → 0).
- **any path resolution.** Nothing joins a request to a directory today.
- **MIME types.** `write_response` takes `content_type: &str` and the one caller
  passes a literal.
- **method routing.** `grep -c "request.method" dog/metrics/mod.rs` → **0**: the
  metrics dog never looks at the method, so `DELETE /metrics` returns the
  exposition. That is fine for one loopback route and is not fine for serve.
- **extra headers.** The signature is `(stream, status, content_type, body)`.
  There is no way to emit `Location`, `WWW-Authenticate`, or
  `X-Content-Type-Options`. Task 2 adds one.
- **bodies that are not already in memory.** `body: &[u8]`. Serving a 2 GiB
  video would read it into a `Vec` first. Task 2 adds a head-then-stream form.
- keep-alive, pipelining, ranges, conditional requests, chunked
  transfer-encoding, compression, TLS, HTTP/2, access logging. All absent, all
  deliberately.

So "hand-rolled on the surface we have" means: keep the request reader and the
bounds exactly as they are, add one response-writing function, and write the
static-file half from scratch — which is where the whole of the risk is.

**Task 2 moves the file** from `crates/shep-cli/src/dog/http.rs` to
`crates/shep-cli/src/http.rs`. It stops being the dog's private helper the
moment `serve` uses it, and a `use crate::dog::http` in a `serve` module would
be a lie about the layering. `git mv` plus six import sites
(`grep -rn "dog::http\|dog/http.rs" crates | wc -l` → 6).

### 4. What `serve` builds, and what the spec does not ask for

Spec §9's whole sentence is: "static file server as a managed sheep (axum +
tower-http, SPA fallback, dir listing, constant-time basic auth from creds
file)". Spec §10 adds: "Metrics and serve bind 127.0.0.1 by default."

**Required, because the spec names it:** SPA fallback; directory listing;
constant-time basic auth read from a credentials file; loopback default bind.

**Required, because the job is impossible without it and the spec assumes it:**
path resolution (decision 5) and MIME types. A server that answers `text/plain`
for `.css` does not render a page in any modern browser, so a MIME table is not
a feature to weigh — it is the difference between working and not. It is a
fixed `const` table of about twenty-five extensions with
`application/octet-stream` as the fallback, not a `mime_guess` dependency for
a lookup that is a `match`.

**Not required, and not built.** Each is named again in the closing section:

- **Range requests.** Not in the spec. `Accept-Ranges` is never sent and a
  `Range` header is ignored, which HTTP permits — the client gets the whole
  body with a 200. Cost: seeking in a `<video>` served by `shep serve` re-fetches
  from zero. Ledger entry, v1.1 candidate.
- **Conditional requests / caching.** No `Last-Modified`, no `ETag`, no
  `If-Modified-Since`. Every request re-reads the file.
- **Compression, TLS, HTTP/2, keep-alive.** `Connection: close` on every
  response, as the existing writer already does.
- **Hidden-file filtering.** A dotfile in the docroot is served like any other
  file, so a `.env` or a `.git/config` sitting in a directory the operator chose
  to publish is published. This is the no-code option and it is what pm2's serve
  does; it is written into `--help` in as many words rather than left for
  somebody to discover. Ledger entry.
- **`PM2_SERVE_*` environment compatibility** (map.md names it). shep's own
  rule is that every knob it owns is `SHEP_`-prefixed; reading another tool's
  variables would be shep configuring itself from a namespace it does not own.

**One header is added that the spec does not name:
`X-Content-Type-Options: nosniff`, on every response.** Without it a browser may
sniff a served file's bytes and decide a `.txt` is HTML, which turns any docroot
containing user-supplied files into a stored-XSS surface. It is one constant
header on a path that already writes three.

### 5. Path resolution: split first, decode second, and six refusals

This is the whole verb. A static file server resolves an attacker-controlled
string against a directory, and path traversal is the oldest bug in the
category. The resolver is a **pure function in the pure tier** —
`crates/shep-cli/src/serve/path.rs`, no `cfg`, no I/O, `&str` in and a
`Result<RelPath, Refusal>` out — so it compiles on Windows, is covered by the
cross-check, and its tests need no filesystem.

```rust
/// Resolves a request target to a root-relative path, or refuses it.
pub fn resolve(target: &str) -> Result<Vec<String>, Refusal>;
```

**The order is load-bearing and the obvious order is wrong.** Decoding the
whole target and then splitting on `/` lets `%2f` create separators after the
fact, so `..%2f..%2fetc%2fpasswd` becomes `../../etc/passwd` *after* the
traversal check has already run on a single segment. The correct order, and the
one implemented:

1. Refuse a target that does not start with `/` (an absolute-form target like
   `GET http://host/x`, or a bare `../etc/passwd`).
2. Cut the query at the first `?` and discard it. Cut a `#` fragment too — a
   client should never send one, and a server that treats it as a filename is
   one more surface.
3. **Split on `/`.**
4. **Then percent-decode each segment**, byte by byte, refusing a malformed
   escape (`%`, `%z`, `%4`).
5. Refuse a decoded segment containing any of: a NUL byte; any other control
   byte (`< 0x20` or `0x7f`); `/`; `\`; or bytes that are not valid UTF-8.
6. Walk: `""` and `"."` are skipped, `".."` pops, and **a `".."` with nothing
   left to pop is a refusal**, not a clamp to root. Everything else is pushed.

The five shapes named in this phase's brief, and exactly which rule refuses
each — every one of them a test in Step 3.2:

| Request | Refused by | Status |
|---|---|---|
| `GET /../../etc/passwd` | rule 6, the pop with an empty stack | 400 |
| `GET /%2e%2e/%2e%2e/etc/passwd` | rule 4 decodes to `..`, then rule 6 | 400 |
| `GET /..%2f..%2fetc/passwd` | rule 5 — the decoded segment contains `/` | 400 |
| `GET /etc/passwd` (absolute path) | not refused — there is nothing to refuse. Every segment is pushed onto a stack that starts empty and is joined onto the root, so a leading `/` is an empty first segment and `etc/passwd` is looked for **inside the docroot**. The refusal is structural: no code path exists that produces a path outside the root from a lexical walk. | 404, if the docroot has no `etc/passwd` |
| `GET /x%00.png` | rule 5, the NUL | 400 |
| a symlink inside the root pointing outside it | not lexical at all — see below | 404 |

A sixth shape, not in the brief and worth as much as any of them:
**`GET /a%0d%0aSet-Cookie:%20x`**. Decoded, that segment carries CR and LF. A
raw CR/LF cannot survive the request reader (it splits on `\n` and strips a
trailing `\r`), but a percent-encoded one arrives intact — and this server echoes
a path into a `Location` header on the directory redirect (decision 9). That is
HTTP response splitting. Rule 5's control-byte refusal is the fix; Task 2 adds a
second, independent lock in `write_head`, which refuses to write any header
value containing a control byte and answers 500 instead. Two locks because one
of them is on the code path a future feature is most likely to route around.

**The symlink case is not lexical and cannot be.** The lexical walk guarantees
the *requested* path is under the root; it says nothing about where the
filesystem sends it. After the walk, `serve`:

- canonicalizes the docroot **once, at startup** (so a docroot that is itself a
  symlink — `dist -> releases/2026-08-15`, the normal deploy shape — works);
- joins the walked segments onto that canonical root;
- `std::fs::canonicalize`s the result, which resolves every symlink in it;
- requires the canonical result to `starts_with` the canonical root, and answers
  **404** when it does not;
- then requires the metadata to be a regular file or a directory — a fifo,
  socket or device node in the docroot is a 404, because opening a fifo blocks
  the task forever and that is a denial of service with no error message.

**The TOCTOU is real and is accepted, in writing.** Between `canonicalize` and
`File::open`, a local attacker who can create files in the docroot can swap a
path for a symlink. Closing it properly needs `openat2(RESOLVE_BENEATH)`, which
is Linux-only, `unsafe`, and unavailable on macOS — a tier-1 platform. The
accepted argument: an attacker who can write a symlink into the docroot can
already write an `index.html` into it, so the confidentiality boundary this
would defend was already gone. That argument holds for the docroot and **not**
for a docroot on a shared tmpdir; the `--help` text says so.

`starts_with` on `Path` compares **components**, not string prefixes, so
`/srv/www-secret` does not match a root of `/srv/www`. Write it as
`canonical.starts_with(&root)` on `Path` values and never as a `to_str` prefix
test; a test in Step 3.3 pins exactly that case.

### 6. The refusal taxonomy: 400, 401, 404, 405, 500, and nothing else

- **400** — the target itself is invalid: not starting with `/`, a malformed
  escape, a forbidden byte in a decoded segment, a `..` above root, or a request
  carrying a body (`content-length > 0`, or any `Transfer-Encoding` header,
  which this server never reads). Body text names the reason in one plain line.
- **401** — auth is configured and the request did not satisfy it, with
  `WWW-Authenticate: Basic realm="shep"`.
- **404** — resolved fine, and there is nothing to serve: no such path, a path
  that canonicalizes outside the root, a non-regular file, or a directory with
  no index and no listing enabled.
- **405** — a method other than `GET` or `HEAD`, with `Allow: GET, HEAD`.
- **500** — the file existed and could not be read, or a header value failed the
  control-byte check. Never carries the underlying error text to the client; it
  goes to the access log instead.

**Everything outside the root is a 404, never a 403.** One status for "not
here" and "not yours" means the server never confirms the existence of a file
the client is not allowed to have. The auth check is the deliberate exception —
401 has to be distinguishable, or no client can ever authenticate.

**Order matters and is pinned by a test**: auth is checked **before** path
resolution, so an unauthenticated client cannot use the difference between 400
and 404 to map the filesystem. A test asserts a traversal attempt against an
auth-protected server answers 401, not 400.

### 7. Basic auth: a creds file, `0600`, SHA-256 and `ring::constant_time`

`--auth <path>`. The file holds one line, `user:password`, trailing newline
optional. Empty file, no colon, or more than one non-empty line → `serve`
refuses to start, exit `InvalidConfig` (4), naming the file and the problem but
**never printing a line of its contents**.

**The file's mode is checked.** Group- or world-readable (`mode & 0o077 != 0`)
is a refusal at startup, the same posture as `$SHEP_HOME/run` at `0700` and the
`0600` `shep.toml` write. A credential the whole box can read is not a
credential.

**Constant-time comparison, done with `ring`.** `ring` is already in the
dependency tree — `grep -c '^name = "ring"' Cargo.lock` → 1, pulled in by
`tokio-rustls`, which shep-cli already depends on **unconditionally** for the
bark dog's TLS. Naming it directly in shep-cli's manifest therefore adds **zero
crates** and costs no cross-compile that is not already paid (the mingw
requirement for the Windows check exists today because of `ring`'s `cc`).
Confirm that with `cargo tree -p shep-cli | grep ring` before and after, and
record both.

```rust
use ring::digest::{SHA256, digest};
use ring::constant_time::verify_slices_are_equal;

fn credentials_match(presented: &[u8], expected: &[u8]) -> bool {
    // Digest first: `verify_slices_are_equal` returns Err immediately on a
    // length mismatch, so comparing the raw pair leaks the credential's
    // length. Two SHA-256 digests are always 32 bytes.
    verify_slices_are_equal(
        digest(&SHA256, presented).as_ref(),
        digest(&SHA256, expected).as_ref(),
    )
    .is_ok()
}
```

Hand-rolling the comparison was considered and rejected: an XOR-accumulate loop
is four lines and the compiler is permitted to optimise it into an early exit,
which is precisely the property being bought. `ring`'s version is audited and
already compiled into this binary.

**It is HTTP, and the credential is base64 on the wire.** `--help` says so, in
one sentence, next to the sentence about `--bind`. Basic auth on a loopback bind
protects against another process on the box; over a wider bind it protects
against nothing without TLS in front.

### 8. `serve` binds loopback by default, and widening it is loud

`--bind 127.0.0.1`, `--port 8080`. Spec §10 fixes the default; the metrics dog
already sets the precedent and its `MetricsConfig::bind` doc argues it: "this
dog will not widen its own exposure as a side effect of being enabled".

Widening takes an explicit `--bind 0.0.0.0` (or a routable address). When the
bind is not loopback, `shep serve` prints a stderr notice at registration time
naming the address and the docroot, and — if `--auth` was not given — says that
the files will be readable by anything that can reach the port. It is a notice,
not a refusal: serving a directory to a LAN is a legitimate thing to want, and a
tool that refuses it teaches people to reach for `python -m http.server`, which
has no auth at all.

### 9. Directory listing is off by default; the trailing-slash redirect is not optional

`--listing` enables it. Off by default, on the same reasoning as every other
exposure knob in this project (`--with-env`, `--allow-control`, the metrics
bind): a listing enumerates filenames, and filenames on an internal service are
information. With no index and no `--listing`, a directory is a 404. This is a
deliberate divergence from pm2's serve, which lists by default, and it is named
in the migration doc.

Three details that are easy to get wrong and are each a test:

- **A directory requested without a trailing slash gets a 301 to the same path
  with one.** Without it, every relative link in the served `index.html`
  resolves against the parent directory and the page is broken in a way that
  looks like a shep bug. The `Location` value is built from the **already
  resolved and re-encoded** path, never from the raw target — see decision 5's
  response-splitting note.
- **Filenames are HTML-escaped in the listing** (`&`, `<`, `>`, `"`, `'`). A file
  named `<script>alert(1)</script>` in a listed directory is stored XSS
  otherwise, and creating such a file is something a build tool can do by
  accident. The test creates exactly that filename.
- **The listing links are percent-encoded**, so a filename with a space or a `#`
  produces a link that works.

### 10. SPA fallback is gated on `Accept: text/html`

`--spa` makes a would-be 404 serve `<root>/index.html` with a **200**. Gated:
the fallback only fires when the request's `Accept` header contains `text/html`.

Ungated — pm2's behaviour — a missing `/assets/app.a1b2c3.js` returns HTML with
a 200, the browser refuses it as a module with a MIME error, and the developer
reads a message about a script type that has nothing to do with the missing
file. The gate costs one `contains` and makes missing assets 404 honestly while
deep links still work, because a navigation always sends `Accept: text/html`.

If `--spa` is given and `<root>/index.html` does not exist, `serve` refuses to
start (exit 4). A fallback to a file that is not there is a 500 on every 404.

### 11. `serve` runs as a managed sheep whose command line is `shep serve … --foreground`

`shep serve ./dist` does not bind anything. It resolves and canonicalizes the
docroot, refuses it early if it is missing or not a directory (exit 2), builds
an `AppConfig` whose `script` is `std::env::current_exe()` and whose `args` are
the same invocation plus `--foreground`, and sends it through the existing
`Request::Start` — the same path `shep start` uses, including
`connect_or_spawn_client`, because starting a sheep against a dead shepherd
means bringing one up first.

`--foreground` is **visible, not hidden**, unlike `daemon` and `dog`. Three
reasons: the resulting command line is readable in `shep describe` and means
exactly what it says; `shep serve ./dist --foreground` is a genuinely useful
thing to type when you want a server in this terminal and no shepherd at all;
and a hidden twin verb would need a name, and every name considered was either
a lie (`serve-worker`) or invented theme in a place the theme buys nothing.

The docroot is passed **canonicalized and absolute** in the sheep's args. A
relative path would resolve against the shepherd's cwd, not the operator's, and
that is a bug that only shows up after a reboot.

Name: `--name`, defaulting to the canonical docroot's file name, falling back to
`serve`. Fold: `--fold`, straight through. No `--instances`: one static server
does not need `SO_REUSEPORT` semantics explaining to it.

### 12. `dev` and `runtime` are one foreground engine with different options

Both are "boot a shepherd in this process, start a flock, stream its bleats to
stdout, and exit when nothing is online". Written once, in
`crates/shep-cli/src/commands/foreground.rs`, with an options struct. This is
not a premature abstraction: there are exactly two callers, both exist in this
phase, and the spec describes their shared behaviour in the same two words
("foreground", "auto-exit").

|  | `shep dev` | `shep runtime` |
|---|---|---|
| `$SHEP_HOME` | `$SHEP_DEV_HOME` else `~/.shep-dev` | `--home`/`$SHEP_HOME` as everywhere else |
| `watch` | forced `true` on every app | as the Flockfile says |
| restore the saved roll | no | no |
| PID-1 init split | never | when `std::process::id() == 1` |
| on exit | stop and delete the flock, then stop the shepherd | stop the shepherd; leave the roll alone |

The engine boots the supervisor with the existing `shep_daemon::boot` and then
**talks to it over its own unix socket like any other client**. Not through
`RunningDaemon::context()`, whose doc says in as many words that it is public
only for `tests/daemon_e2e.rs` and that "the CLI boots and calls `run`; it never
reaches inside". Going over the socket costs one connect on loopback-in-process
and buys: `shep flock` from a second terminal (or `docker exec`) works while
`runtime` is up, which is most of why people like this mode; the whole start
path is the one `shep start` already tests; and shutdown is `Request::KillDaemon`,
the same message `shep kill` sends.

`commands::daemon::run_daemon` is split into `boot_supervisor(paths, args) ->
Result<RunningDaemon, DaemonRunError>` and a `run_daemon` that calls it and then
`.run()`. Two lines moved; the foreground engine spawns `daemon.run()` as a task
and keeps the handle.

### 13. The exit-code collision: fail-fast becomes **11**, and a clean emptying is **0**

Spec §9 states the problem and hands it to this phase: "Code 2 is claimed by
clap for usage errors, which collides with the fail-fast code `runtime` is
specified to use below. `runtime` resolves that when it is built."

**Resolved as: a new code 11, `flock_empty`.** Not 2. A container orchestrator
reads the exit status to decide whether to restart, and a status that means both
"your flags were wrong" and "your app died" is a status it cannot act on. Codes
0–10 are all spoken for by spec §9's table, and the table's own rule is that
distinct causes get distinct codes, so the resolution is a new row rather than a
reused one. Task 12 adds that row to spec §9 and the paragraph above it goes
from naming an open question to naming the answer.

**And the emptying is not always a failure.** `runtime` exits:

- **0** when the flock emptied and no sheep is in `errored` — every app stopped
  cleanly, which is what a batch job in a container looks like, and what
  `autorestart = false` or a matching `stop_exit_codes` is *for*;
- **11** when any sheep ended `errored` — the restart budget was exhausted or a
  spawn failed, which is the fail-fast case the spec means and the case where an
  orchestrator should restart the container;
- **1** if the supervisor itself failed, **4** on an invalid Flockfile, and the
  ordinary taxonomy for everything else.

pm2-runtime exits non-zero unconditionally here. Following it would mean a
`shep runtime` running a one-shot migration job reports failure on success.

**The debounce is kept exactly as map.md records it: three consecutive polls,
two seconds apart.** Not ceremony — a sheep in `waiting-restart` between backoff
attempts is momentarily not online, and a single-sample exit would tear the
container down in the middle of a restart the supervisor was about to complete.
Six seconds of "nothing online" is a flock that is not coming back.

### 14. PID 1 gets its own process. The in-process waitpid loop cannot work

map.md sketches "PID-1 zombie reaping added (subreaper + WNOHANG loop)". **In
the supervisor's own process that is a bug, not a sketch to implement.**

`tokio::process` reaps a child by calling `waitpid(<that pid>, WNOHANG)` when
SIGCHLD fires. A blind `waitpid(-1, WNOHANG)` loop in the same process races it
and wins some of the time: the status is consumed by the reaper, tokio's
`child.wait()` then gets `ECHILD` and returns an `io::Error` instead of an exit
status, and the supervisor records an error for a sheep that exited 0. Spec §4
promises "Exit code + signal recorded exactly (owning-parent `waitpid`)". A
reaper in that process breaks that promise intermittently, which is the worst
way to break it.

The alternative that keeps one process — peek with `waitid(P_ALL, WNOWAIT)`,
reap only pids the supervisor does not own — needs the supervisor's live pid set
across a crate boundary, and livelocks whenever the peeked zombie is one tokio
has not got to yet.

**So `shep runtime` splits when it is PID 1**, which is the tini/dumb-init shape
and is correct by construction:

```
PID 1  shep runtime               (init: forwards signals, reaps everything)
  └─ PID 2  shep runtime … --supervise   (the shepherd, the flock, the bleats)
```

- The init spawns the child with `std::process::Command` — **not**
  `tokio::process`, so nothing but our own loop ever calls `waitpid`, and never
  calls `child.wait()`.
- Its argv is `std::env::args_os().skip(1)` plus `--supervise`, re-using
  `current_exe()`. That is identity-preserving: under `shep runtime x` the child
  is `shep runtime x --supervise`, under `shep-runtime x` it is
  `shep-runtime x --supervise`, and decision 2's alias rule does the rest.
- SIGTERM, SIGINT, SIGHUP and SIGQUIT are forwarded to the child's pid (not its
  group — the child owns its own flock's groups). **This is the reason an init
  is needed at all**: the kernel gives PID 1 no default dispositions, so a
  `docker stop` on a PID 1 with no handler is ignored until the 10-second SIGKILL.
- On SIGCHLD, and on a 1-second backstop tick, `waitpid(-1, WNOHANG)` in a loop
  until it reports `StillAlive` or `ECHILD`. Orphaned lambs reparented to PID 1
  are reaped here and nowhere else.
- When the reaped pid is the child's, the init exits with the child's code, or
  `128 + signal` if it died by one. It does **not** wait for remaining orphans:
  the container is going away and a stuck orphan must not hold the exit.
- `--supervise` also **disables the split unconditionally**, so a mis-detected
  PID cannot produce a fork bomb.

When `std::process::id() != 1` — a developer running `shep runtime` on a laptop,
and every test in CI — there is no split at all and the supervisor runs inline.
Same code, one branch, and the branch is the one thing the Linux-only test in
Task 10 exercises for real.

### 15. `dev` ignores `--home`, and says so

`shep dev`'s home is `$SHEP_DEV_HOME`, else `~/.shep-dev`. `--home` and
`$SHEP_HOME` are **ignored**, and passing `--home` explicitly prints a stderr
notice saying it was ignored and where the dev flock actually lives.

Two reasons. Isolation is the entire feature spec §9 names, and `--home` carries
`env = "SHEP_HOME"`, so an operator who exports `SHEP_HOME` for their real flock
would silently get a `shep dev` that shares it — the failure mode being a forced
`watch = true` written onto production apps. And the e2e tier needs a knob the
developer's own environment cannot collide with, or `cargo test` writes into
whoever's real `~/.shep-dev`.

Ignoring a flag the user passed is surprising, which is why the notice exists
rather than silence.

### 16. One access-log line per request, with non-printable bytes escaped (SEVERABLE)

`serve` writes one line to **stdout** per request: method, resolved path,
status, byte count, and the peer address. Because serve runs as a sheep, that is
its bleats — `shep bleats web` is the access log, with no new plumbing at all.

The escaping is not decoration. The raw target can contain any byte a client
sends, and an operator reading `shep bleats` in a terminal would otherwise be
handed ANSI escape sequences by a stranger. The logged path is the **resolved**
one where resolution succeeded, and where it failed it is the raw target with
every byte outside `0x20..=0x7e` rendered as `\xNN`.

If the phase is being trimmed, this is the piece to cut; nothing else depends on
it.

---

## Task order and dependencies

Twelve tasks. The survey that preceded this plan estimated 8 for `serve` and 13
for `dev`/`runtime`; re-derived from the spec and the code, the work buckets
into fewer, fatter tasks — the two verbs share one engine (decision 12), and
serve's five modules are five steps of one design rather than five independent
pieces.

```
1  library extraction + three bins        (blocks everything; land it first and fast)
2  http.rs moves up, gains write_head     (blocks 6)
3  serve::path — resolution and refusals  (pure; can run parallel to 2)
4  serve::mime + serve::listing           (pure; parallel to 3)
5  serve::auth — creds file, ring         (pure-ish; parallel to 3)
6  serve::worker — bind, accept, respond  (needs 2,3,4,5)
7  shep serve — the verb                  (needs 6)
8  ExitCode::FlockEmpty + the watcher     (parallel to 2-7)
9  foreground engine + shep runtime       (needs 1,8)
10 the PID-1 init split and reaper        (needs 9)
11 shep dev                               (needs 9)
12 docs, ledger, changelogs, CLAUDE.md    (last)
```

Tasks 3, 4, 5 and 8 are pure and independent of each other and of 1; if this
phase is run with parallel agents, that is the fan-out that exists. Task 1 is
not parallelisable with anything, because it moves the file they all land in.

---

## Task 1 — `crates/shep-cli` becomes a library with three thin binaries

**Files:** `crates/shep-cli/src/main.rs` → `crates/shep-cli/src/lib.rs`,
`crates/shep-cli/src/bin/shep.rs`, `crates/shep-cli/src/bin/shep-runtime.rs`,
`crates/shep-cli/src/bin/shep-dev.rs`, `crates/shep-cli/Cargo.toml`,
`crates/shep-cli/src/exit.rs`.

This task adds no behaviour. `shep <anything>` must do exactly what it did
before, and the e2e suite is the proof.

### Step 1.1 — baseline

```bash
grep -cF '[[bin]]' crates/shep-cli/Cargo.toml                     # 1
ls crates/shep-cli/src/lib.rs                                     # No such file or directory
grep -c "#\[test\]\|#\[tokio::test\]" crates/shep-cli/src/main.rs # 13
cargo test -p shep-cli --bins --all-features                      # record every line
cargo test -p shep-cli --lib  --all-features                      # runs NOTHING today, exits 0
```

Run both of those last two and read them. The `--lib` one printing `0 passed`
while exiting `0` is the trap this task inverts, and seeing it once now is what
makes the inversion obvious later.

### Step 1.2 — the move

```bash
git mv crates/shep-cli/src/main.rs crates/shep-cli/src/lib.rs
mkdir -p crates/shep-cli/src/bin
```

In `lib.rs`: keep every `mod` line, every `use`, every function and the whole
`#[cfg(test)] mod tests` exactly as they are. Four edits and no others:

1. `#![forbid(unsafe_code)]` stays at the top.
2. Delete `#[tokio::main] async fn main() { … }` and replace it with the three
   public entry points and the private helper below.
3. Add a crate-level `//!` doc that is written for **docs.rs**, because that is
   now where it renders. Three sentences: what the crate is, that the binary is
   `shep`, and that the API is three entry points because embedding shep is
   `shep-client`'s job.
4. Delete the paragraph in the old module doc claiming serve/dev/runtime are not
   built (`grep -c "not built"` goes `2 → 1`; Task 12 handles the rest of the
   staleness, but this sentence is falsified by this very phase and must not
   survive it).

```rust
/// The `shep` entry point. Parses this process's arguments and runs one verb.
///
/// Returns rather than exiting, so the caller's `main` owns the process exit —
/// which is also what lets the integration tier call this without taking the
/// test harness down with it.
#[must_use]
pub fn main() -> std::process::ExitCode {
    run_argv(std::env::args_os().collect())
}

/// The `shep-runtime` entry point: `shep runtime`, with the verb supplied.
#[must_use]
pub fn main_runtime() -> std::process::ExitCode {
    run_argv(alias_argv("runtime", std::env::args_os().collect()))
}

/// The `shep-dev` entry point: `shep dev`, with the verb supplied.
#[must_use]
pub fn main_dev() -> std::process::ExitCode {
    run_argv(alias_argv("dev", std::env::args_os().collect()))
}

/// Builds the argument vector an alias binary should be parsed as: `verb`
/// inserted after argv[0].
///
/// **Except for `daemon` and `dog`.** Both are hidden re-exec targets that the
/// supervisor spawns as `std::env::current_exe()` plus the verb —
/// `shep_daemon::dogs` for a built-in dog, `crate::launch::launch_command` for
/// the shepherd itself. Under an alias binary, `current_exe()` is
/// `shep-runtime`, so inserting a verb here would turn `shep-runtime dog
/// metrics` into `shep runtime dog metrics` and every dog in a container would
/// die at its first exec. Those two argument vectors are never typed by a
/// human; they are constructed in exactly the two places named above.
fn alias_argv(verb: &str, mut argv: Vec<OsString>) -> Vec<OsString> {
    let passthrough = matches!(
        argv.get(1).and_then(|arg| arg.to_str()),
        Some("daemon" | "dog")
    );
    if !passthrough {
        argv.insert(1, OsString::from(verb));
    }
    argv
}

/// Parses `argv` and runs it on a fresh multi-threaded runtime.
///
/// The runtime is built here rather than by `#[tokio::main]` on each entry
/// point, so the three of them share one construction and the `argv` seam
/// above stays testable without one.
fn run_argv(argv: Vec<OsString>) -> std::process::ExitCode {
    #[cfg(unix)]
    if std::env::var_os("SHEP_TERM_PANIC_PROBE").is_some() {
        lookout::term::probe_panic_for_test();
    }
    let cli = Cli::parse_from(argv);
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("shep: could not start an async runtime: {err}");
            return std::process::ExitCode::from(ExitCode::Failure as u8);
        }
    };
    std::process::ExitCode::from(runtime.block_on(run(cli)) as u8)
}
```

`Cli::parse_from`, not `Cli::parse`: `parse_from` is what makes `alias_argv`'s
output reach clap, and it is the same function `Cli::parse` calls with
`args_os()`.

**`std::process::ExitCode::from(u8)` is not `std::process::exit`.** The old
`main` called `std::process::exit(code as i32)`, which skips destructors and
buffer flushes. Returning an `ExitCode` runs them. That is a behaviour change,
it is the right one, and it is the one place in this task where "adds no
behaviour" is not literally true — write it in the task report. If any e2e test
turns flaky at this step, that ordering change is the first suspect.

### Step 1.3 — the three binaries

```rust
// crates/shep-cli/src/bin/shep.rs
//! The `shep` binary. Everything it does lives in the library beside it.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    shep_cli::main()
}
```

`shep-runtime.rs` and `shep-dev.rs` are the same three lines calling
`shep_cli::main_runtime()` and `shep_cli::main_dev()`.

### Step 1.4 — the manifest

```toml
[lib]
name = "shep_cli"
path = "src/lib.rs"

[[bin]]
name = "shep"
path = "src/bin/shep.rs"

# Container entrypoint aliases (spec §3). Each is three lines over the library
# above; the verb is supplied by the entry point it calls, so nothing here
# reads argv[0].
[[bin]]
name = "shep-runtime"
path = "src/bin/shep-runtime.rs"

[[bin]]
name = "shep-dev"
path = "src/bin/shep-dev.rs"
```

`cargo` would infer `src/bin/*.rs` without the explicit entries; they are
written out anyway, because `cargo publish` packaging and cargo-dist's artefact
list both read this table, and a reader asking "what does this crate install"
should find the answer here.

### Step 1.5 — RED then GREEN: the alias rule

Four tests, in `lib.rs`'s existing `mod tests`. All four run on every target
(they touch clap and `alias_argv`, both pure tier), which is what puts them
under the Windows cross-check.

```rust
    /// fails if an alias binary stops supplying its own verb.
    #[test]
    fn an_alias_supplies_its_verb() {
        let argv = alias_argv("runtime", vec!["shep-runtime".into(), "./Flockfile.toml".into()]);
        assert_eq!(argv, vec![
            OsString::from("shep-runtime"),
            OsString::from("runtime"),
            OsString::from("./Flockfile.toml"),
        ]);
    }

    /// fails if the alias with no arguments at all stops naming its verb —
    /// `shep-dev` on its own must be `shep dev`, not `shep`.
    #[test]
    fn an_alias_with_no_arguments_still_supplies_its_verb() {
        let argv = alias_argv("dev", vec!["shep-dev".into()]);
        assert_eq!(argv, vec![OsString::from("shep-dev"), OsString::from("dev")]);
    }

    /// fails if an alias binary rewrites the two hidden re-exec verbs.
    ///
    /// This is the container-killer: `shep_daemon::dogs` spawns a built-in dog
    /// as `current_exe() dog <name>`, and under `shep-runtime` that is this
    /// argument vector. A prepend here makes every dog exit with a clap usage
    /// error the moment `shep runtime` enables one.
    #[test]
    fn an_alias_passes_the_two_re_exec_verbs_through_untouched() {
        for verb in ["daemon", "dog"] {
            let argv = alias_argv(
                "runtime",
                vec!["shep-runtime".into(), verb.into(), "metrics".into()],
            );
            assert_eq!(argv[1], OsString::from(verb), "{verb} must not be rewritten");
            assert_eq!(argv.len(), 3, "{verb}: nothing may be inserted");
        }
    }

    /// fails if the pass-through is written as a prefix or contains test
    /// rather than an exact match — a sheep legitimately named `dogfood`
    /// must still reach `runtime`, not be mistaken for the `dog` re-exec.
    #[test]
    fn the_pass_through_matches_the_whole_argument_and_not_a_prefix() {
        let argv = alias_argv("runtime", vec!["shep-runtime".into(), "dogfood".into()]);
        assert_eq!(argv[1], OsString::from("runtime"));
        assert_eq!(argv[2], OsString::from("dogfood"));
    }
```

And one that proves the composed vector actually parses, which the four above do
not:

```rust
    /// fails if the alias vector is well-formed and still does not reach the
    /// verb — a `runtime` subcommand that took a required positional, say.
    #[test]
    fn the_alias_vector_parses_to_the_expected_command() {
        use clap::Parser;
        let argv = alias_argv("dog", vec!["shep-runtime".into(), "dog".into(), "metrics".into()]);
        let cli = Cli::try_parse_from(argv).expect("the passthrough vector must parse");
        assert!(matches!(cli.command, Commands::Dog(_)));
    }
```

That last one is written **now**, against `dog`, because `runtime` and `dev` do
not exist until Tasks 9 and 11. Task 9 adds its sibling for `runtime` and Task
11 for `dev`; both are named in those tasks' verification blocks.

### Step 1.6 — the e2e tier proves nothing regressed

`tests/cli_e2e.rs` and `tests/term_panic_order.rs` both use
`Command::cargo_bin("shep")`, which still resolves — the binary name did not
change. Run them unchanged:

```bash
cargo test -p shep-cli --test cli_e2e --all-features
cargo test -p shep-cli --test term_panic_order --all-features
```

Any failure here is this task's, not a pre-existing one; the baseline was green.

Add one e2e case, because the two alias binaries are otherwise untested as
binaries:

```rust
/// fails if `shep-dev` is not built, is not installed under that name, or does
/// not reach the `dev` verb. `--help` is used rather than a real run so the
/// test starts no shepherd and writes to no home.
#[test]
fn the_alias_binaries_exist_and_reach_their_own_verbs() {
    for (bin, verb) in [("shep-dev", "dev"), ("shep-runtime", "runtime")] {
        let output = Command::cargo_bin(bin)
            .unwrap_or_else(|err| panic!("{bin} must be a [[bin]] target: {err}"))
            .arg("--help")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(verb), "{bin} --help must be {verb}'s help:\n{text}");
    }
}
```

Written in Task 1 and **expected to fail until Tasks 9 and 11 land the verbs**.
Guard it in Task 1 with `#[ignore = "the dev and runtime verbs land in Tasks 9
and 11"]` and delete the attribute in Task 11 — an ignored test with a stated
reason is honest; a deleted one is a hole.

### Step 1.7 — the IR-20 comment that this task falsifies

`crates/shep-cli/src/exit.rs` says, above `ExitCode`:

> No `#[non_exhaustive]`: this is a binary crate, so there is no downstream
> matcher for it to protect …

After this task the crate is a library. The conclusion is unchanged — `ExitCode`
is not `pub` from `lib.rs`, so there is still no downstream matcher — but the
reason is now false as written. Rewrite it to say that, and check:

```bash
grep -c "this is a binary crate" crates/shep-cli/src/exit.rs   # 1 before, 0 after
grep -c "not exported from `lib.rs`" crates/shep-cli/src/exit.rs   # 0 before, 1 after
```

Then sweep for the same claim elsewhere:

```bash
grep -rn "binary crate\|\[\[bin\]\]-only" crates/shep-cli/src docs/idiomatic-rust.md
```

Read every hit and fix the ones that are now false. `docs/idiomatic-rust.md`'s
IR-20 text is one of them; Task 12 owns the docs, but note the hits here so that
task has a list rather than a search.

### Step 1.8 — MUTATION

Delete the `daemon | dog` pass-through from `alias_argv` (make it prepend
unconditionally). Expected:
`an_alias_passes_the_two_re_exec_verbs_through_untouched` fails on both verbs.
If it passes, the test is asserting nothing and the container-killer is
unguarded. Revert.

Second mutation, cheap and worth it: change `argv.insert(1, …)` to
`argv.push(…)`. Expected: `an_alias_supplies_its_verb` fails on ordering **and**
`the_alias_vector_parses_to_the_expected_command` still passes — which is the
point of having both, since a vector can parse and still be wrong.

### Step 1.9 — verification

```bash
grep -cF '[[bin]]' crates/shep-cli/Cargo.toml       # 1 → 3
grep -c '^pub ' crates/shep-cli/src/lib.rs          # 3, exactly
grep -c '^pub mod' crates/shep-cli/src/lib.rs       # 0 (grep exits 1)
ls crates/shep-cli/src/bin | wc -l                  # 3
cargo test -p shep-cli --lib --bins --all-features  # every test from the --bins baseline, +5
```

`grep -c '^pub '` printing exactly `3` is decision 1's whole guard, and its
limit is stated there: it counts declarations at column 0 and would not catch a
`pub` item nested inside a `mod` block that was itself made public. The
`^pub mod` check is the second half of that pair. Read the rendered docs once:

```bash
RUSTDOCFLAGS="-D warnings" cargo doc -p shep-cli --no-deps --all-features
```

and open `target/doc/shep_cli/index.html`. It must list three functions and no
modules. This is a human check; say in the report that you did it.

### Step 1.10 — gate

---

## Task 2 — `http.rs` moves to the crate root and learns to write a head

**Files:** `crates/shep-cli/src/dog/http.rs` → `crates/shep-cli/src/http.rs`,
`crates/shep-cli/src/lib.rs`, `crates/shep-cli/src/dog/mod.rs`,
`crates/shep-cli/src/dog/metrics/mod.rs`.

### Step 2.1 — baseline

```bash
grep -rn "dog::http\|dog/http.rs" crates | wc -l          # 6
grep -c "write_response" crates/shep-cli/src/dog/http.rs  # 5
grep -c "pub async fn" crates/shep-cli/src/dog/http.rs    # 2
cargo test -p shep-cli --lib --bins --all-features        # record
```

### Step 2.2 — the move

```bash
git mv crates/shep-cli/src/dog/http.rs crates/shep-cli/src/http.rs
```

`mod http;` moves from `dog/mod.rs` to `lib.rs`, beside `mod output;` — and it
is **not** `#[cfg(unix)]`. The module is pure `tokio::io` and compiles
everywhere; `dog/mod.rs` is where the unix gate lives and it stays there. Update
the six import sites. Update the module doc's opening sentence, which currently
says "The little HTTP a dog needs" and now has two consumers: it is the little
HTTP **this binary** needs, for the metrics dog and for `serve`, and the
paragraph explaining why it is hand-rolled rather than a crate gains one clause
naming Rin's 2026-08-15 ruling on `serve` alongside the existing reasoning.

### Step 2.3 — RED then GREEN: `write_head`

```rust
/// One extra response header: a name and a value, both already final.
pub struct Header<'a> {
    /// The header name, written exactly as given.
    pub name: &'a str,
    /// The value, written exactly as given — refused if it carries a control
    /// byte, see [`write_head`].
    pub value: &'a str,
}

/// Writes a response head and stops, leaving the body to the caller.
///
/// [`write_response`] is still the right function when the whole body is
/// already a `&[u8]` — the metrics dog's exposition is exactly that. This one
/// exists for `serve`, which streams a file it has not read: `content_length`
/// comes from the file's metadata and the caller copies the bytes afterwards.
///
/// Every response carries `Connection: close`, the same as [`write_response`],
/// so a caller that writes fewer bytes than it declared closes the connection
/// and the client sees a truncated response rather than a hang.
///
/// # Errors
/// - [`HttpError::Io`] — the write failed.
/// - [`HttpError::BadHeader`] — a header name or value carries a byte outside
///   the printable ASCII range. A `Location` built from a request path is the
///   case this exists for: a percent-encoded CRLF that reached this far would
///   otherwise split the response and let a client inject headers of its own.
///   The caller answers 500; nothing is written to the stream first, so the
///   refusal cannot itself produce a malformed response.
pub async fn write_head<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: u16,
    content_type: &str,
    content_length: u64,
    headers: &[Header<'_>],
) -> Result<(), HttpError>
```

`HttpError` gains one variant, `BadHeader { what: &'static str }` — a fixed
reason, never the offending bytes, matching the type's existing rule that its
`Debug` is safe to log because it never holds a header value. Its `Display`,
`source` and the doc on the enum all get their arms; the enum is private to the
crate now (decision 1), so IR-20's `#[non_exhaustive]` question does not arise —
say so in the variant's own comment rather than leaving a reader to wonder.

Tests, over `tokio::io::duplex` like every existing test in this file:

```rust
    /// fails if a header value carrying CRLF is written to the stream —
    /// response splitting, reachable from a percent-encoded path in a
    /// `Location`.
    #[tokio::test]
    async fn a_header_value_with_a_control_byte_is_refused_before_anything_is_written() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let err = write_head(
            &mut server,
            301,
            "text/html",
            0,
            &[Header { name: "Location", value: "/a\r\nSet-Cookie: x=1" }],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HttpError::BadHeader { .. }), "{err:?}");
        drop(server);
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        assert!(buf.is_empty(), "nothing may reach the stream: {buf:?}");
    }

    /// fails if the extra headers are dropped, or if the declared length stops
    /// matching what the caller was told to write.
    #[tokio::test]
    async fn a_head_carries_its_extra_headers_and_its_declared_length() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        write_head(
            &mut server,
            200,
            "text/css",
            42,
            &[Header { name: "X-Content-Type-Options", value: "nosniff" }],
        )
        .await
        .unwrap();
        drop(server);
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        let head = String::from_utf8(buf).unwrap();
        assert!(head.starts_with("HTTP/1.1 200 "), "{head:?}");
        assert!(head.contains("Content-Length: 42\r\n"), "{head:?}");
        assert!(head.contains("Content-Type: text/css\r\n"), "{head:?}");
        assert!(head.contains("X-Content-Type-Options: nosniff\r\n"), "{head:?}");
        assert!(head.contains("Connection: close\r\n"), "{head:?}");
        assert!(head.ends_with("\r\n\r\n"), "a head and nothing else: {head:?}");
    }
```

The empty-buffer assertion in the first test is the one that matters: a
`write_head` that validated *after* writing the status line would still return
the error and still have split the response.

### Step 2.4 — MUTATION

Change the control-byte check from `!(0x20..=0x7e).contains(&b)` to a check for
`b == b'\n'` only. Expected:
`a_header_value_with_a_control_byte_is_refused_before_anything_is_written`
**still passes**, because the injected value contains a `\n` — so also add the
bare-CR case to that test's table before mutating, and confirm the mutation
reddens it. This is dead-check shape 5 (the mutation that changes nothing) and
it was caught while writing this plan: a lone `\r` is enough to split a response
for some clients, and a `\n`-only check would have shipped.

### Step 2.5 — verification

```bash
grep -rn "dog::http" crates | wc -l               # 6 → 0
grep -rn "crate::http" crates/shep-cli/src | wc -l  # 0 → 2 or more
cargo test -p shep-cli --lib --bins --all-features  # baseline +2
```

### Step 2.6 — gate

---

## Task 3 — `serve::path`: resolution, and the six refusals

**Files:** `crates/shep-cli/src/serve/mod.rs` (new, `mod path;` and nothing
else yet), `crates/shep-cli/src/serve/path.rs` (new),
`crates/shep-cli/src/lib.rs` (`mod serve;`).

**This is the security core of the whole phase.** Read decision 5 before
starting. Nothing here touches the filesystem or the network; it is a pure
function and a table of tests.

`mod serve;` in `lib.rs` is **not** `#[cfg(unix)]` at this task — `path.rs` is
pure tier and belongs under the Windows cross-check, which is where the `\`
refusal earns its keep. Task 6 adds `#[cfg(unix)]` on the *worker* submodule
only.

### Step 3.1 — baseline

```bash
ls crates/shep-cli/src/serve                                    # No such file or directory
grep -rni "percent_decode\|percent-decode" crates | wc -l       # 0
cargo test -p shep-cli --lib --bins --all-features              # record
```

### Step 3.2 — RED first: the refusal table

Write these tests before the implementation. Each asserts an **exact** refusal,
and the first also asserts a positive control, so a server that refuses
everything cannot pass it.

```rust
/// Why a request target cannot be resolved. Every variant is a 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The target does not begin with `/`.
    NotAbsolute,
    /// A `%` escape that is not two hex digits.
    BadEscape,
    /// A decoded segment carries a byte a path segment may not: NUL, any
    /// other control byte, `/`, or `\`.
    ForbiddenByte,
    /// A decoded segment is not valid UTF-8.
    NotUtf8,
    /// A `..` with nothing left to pop — the target reaches above the root.
    AboveRoot,
}
```

```rust
    /// The five shapes this phase was asked to refuse by name, plus the
    /// response-splitting one, plus a positive control in the same test so a
    /// resolver that refuses everything cannot pass.
    #[test]
    fn the_traversal_shapes_are_each_refused_for_their_own_reason() {
        // positive control: an ordinary nested asset resolves, and `..` that
        // stays inside the root is allowed to pop.
        assert_eq!(resolve("/assets/app.css").unwrap(), vec!["assets", "app.css"]);
        assert_eq!(resolve("/assets/../index.html").unwrap(), vec!["index.html"]);
        assert_eq!(resolve("/").unwrap(), Vec::<String>::new());
        assert_eq!(resolve("/a//b/./c").unwrap(), vec!["a", "b", "c"]);

        assert_eq!(resolve("/../../etc/passwd"), Err(Refusal::AboveRoot));
        assert_eq!(resolve("/%2e%2e/%2e%2e/etc/passwd"), Err(Refusal::AboveRoot));
        assert_eq!(resolve("/..%2f..%2fetc/passwd"), Err(Refusal::ForbiddenByte));
        assert_eq!(resolve("/x%00.png"), Err(Refusal::ForbiddenByte));
        assert_eq!(resolve("/a%0d%0aSet-Cookie:%20x"), Err(Refusal::ForbiddenByte));
        assert_eq!(resolve("../etc/passwd"), Err(Refusal::NotAbsolute));
        assert_eq!(resolve("http://elsewhere/etc/passwd"), Err(Refusal::NotAbsolute));
        assert_eq!(resolve("/a%zz"), Err(Refusal::BadEscape));
        assert_eq!(resolve("/a%2"), Err(Refusal::BadEscape));
        assert_eq!(resolve("/a%ff%fe"), Err(Refusal::NotUtf8));
    }

    /// An absolute-looking path is NOT a refusal — it is resolved inside the
    /// root, which is the whole structural argument. `GET /etc/passwd` looks
    /// for `<root>/etc/passwd` and 404s; it never reaches `/etc/passwd`.
    #[test]
    fn an_absolute_looking_target_resolves_inside_the_root() {
        assert_eq!(resolve("/etc/passwd").unwrap(), vec!["etc", "passwd"]);
        assert_eq!(resolve("//etc/passwd").unwrap(), vec!["etc", "passwd"]);
    }

    /// fails if the query or fragment reaches the filesystem as part of a
    /// filename.
    #[test]
    fn the_query_and_fragment_are_cut_before_anything_else() {
        assert_eq!(resolve("/index.html?v=2").unwrap(), vec!["index.html"]);
        assert_eq!(resolve("/index.html#top").unwrap(), vec!["index.html"]);
        assert_eq!(resolve("/?../../etc/passwd").unwrap(), Vec::<String>::new());
    }

    /// fails if decoding runs before splitting. `%2f` must stay a byte inside
    /// one segment, never become a separator — and since `/` is forbidden in
    /// a segment, it is refused rather than silently renamed.
    #[test]
    fn decoding_happens_after_splitting_and_never_creates_a_separator() {
        assert_eq!(resolve("/a%2fb"), Err(Refusal::ForbiddenByte));
        // Double-encoded: decodes ONCE, to the literal three characters
        // `%2e%2e`, which is an ordinary (odd) filename and not a traversal.
        assert_eq!(resolve("/%252e%252e/x").unwrap(), vec!["%2e%2e", "x"]);
    }

    /// A backslash is a separator on Windows and this module compiles there.
    /// Refusing it costs a filename nobody has and closes a resolver that
    /// would be wrong the day someone builds the Windows tier.
    #[test]
    fn a_backslash_segment_is_refused_on_every_target() {
        assert_eq!(resolve("/a%5c..%5cetc"), Err(Refusal::ForbiddenByte));
    }
```

### Step 3.3 — GREEN: `resolve`, plus the root containment check

`resolve` is the walk from decision 5. The containment half is a second function
in the same module, because it is the same decision and its failure mode is the
same one:

```rust
/// Joins `segments` onto `root` and returns the path only if it is really
/// inside it.
///
/// `root` must already be canonical — `serve` canonicalizes it once at startup,
/// so a docroot that is itself a symlink (`dist -> releases/2026-08-15`, the
/// ordinary deploy shape) works, and the comparison below is against the place
/// the operator actually chose.
///
/// [`Path::starts_with`] compares **components**, not string prefixes: a root
/// of `/srv/www` does not contain `/srv/www-secret`, and a `to_str()` prefix
/// test would say it does.
///
/// # Errors
/// `None` when the path does not exist, cannot be canonicalized, or resolves —
/// through a symlink, which is the case this exists for — outside `root`. The
/// caller answers 404 to all three without distinguishing them: a server that
/// tells a client which of "missing" and "forbidden" applies is a server that
/// maps its own filesystem on request.
pub fn contain(root: &Path, segments: &[String]) -> Option<PathBuf> {
    let mut joined = root.to_path_buf();
    for segment in segments {
        joined.push(segment);
    }
    let canonical = std::fs::canonicalize(joined).ok()?;
    canonical.starts_with(root).then_some(canonical)
}
```

Tests for `contain` need a real temp tree, so they are `#[cfg(unix)]` (they
create a symlink) and live in the same file behind that gate:

```rust
    /// fails if a symlink inside the root that points outside it is served.
    /// The lexical walk cannot catch this one — the target has no `..` in it
    /// at all.
    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_the_root_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("www");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("ok.txt"), b"served").unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, b"not served").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("escape.txt")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();

        // positive control, in the same test: the ordinary neighbour works.
        assert!(contain(&root, &["ok.txt".to_string()]).is_some());
        assert!(contain(&root, &["escape.txt".to_string()]).is_none());
    }

    /// fails if containment is written as a string prefix. `/srv/www-secret`
    /// starts with the characters of `/srv/www` and is a different directory.
    #[cfg(unix)]
    #[test]
    fn a_sibling_whose_name_extends_the_roots_name_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("www")).unwrap();
        std::fs::create_dir(dir.path().join("www-secret")).unwrap();
        std::fs::write(dir.path().join("www-secret/x"), b"x").unwrap();
        let root = std::fs::canonicalize(dir.path().join("www")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("www-secret"),
            dir.path().join("www/link"),
        ).unwrap();
        assert!(contain(&root, &["link".to_string(), "x".to_string()]).is_none());
    }
```

### Step 3.4 — MUTATION

Two, run one at a time:

1. In the walk, replace the `AboveRoot` refusal with a clamp (`if stack.is_empty()
   { continue }`). Expected: the `AboveRoot` assertions in
   `the_traversal_shapes_are_each_refused_for_their_own_reason` fail. This is the
   single most common way this bug ships — clamping *is* safe against the
   lexical escape, which is exactly why it looks acceptable, and it silently
   turns `/../secret` into `/secret`, serving a file the client did not ask for.
   If the test still passes, the refusal is not being asserted, only the
   non-200-ness.
2. Change `canonical.starts_with(root)` to
   `canonical.to_string_lossy().starts_with(&*root.to_string_lossy())`. Expected:
   `a_sibling_whose_name_extends_the_roots_name_is_not_contained` fails and the
   symlink test still passes — which is why both exist.

### Step 3.5 — verification

```bash
grep -c "fn resolve" crates/shep-cli/src/serve/path.rs   # 1
cargo test -p shep-cli --lib --bins --all-features       # baseline +7
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The Windows check runs here rather than only at the merge: `path.rs` is the
pure-tier module this phase adds and the backslash refusal is a Windows claim.

### Step 3.6 — gate

---

## Task 4 — `serve::mime` and `serve::listing`

**Files:** `crates/shep-cli/src/serve/mime.rs`,
`crates/shep-cli/src/serve/listing.rs`. Both pure, both in the pure tier.

### Step 4.1 — baseline

```bash
grep -rni "text/css\|application/octet-stream" crates | wc -l   # 0
cargo test -p shep-cli --lib --bins --all-features               # record
```

### Step 4.2 — the table

```rust
/// Extension → content type. About twenty-five entries, ASCII-lowercased on
/// lookup, `application/octet-stream` for anything else.
///
/// A fixed table rather than `mime_guess`: this is a `match` over the
/// extensions a static site actually contains, and a dependency for it would
/// be a crate in the tree for a lookup, plus a second opinion about `.js`
/// nobody asked for.
///
/// `charset=utf-8` is on the text types deliberately. Without it a browser
/// falls back to a locale-dependent encoding and a UTF-8 page renders as
/// mojibake on somebody else's machine and not on yours.
const TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("htm", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("txt", "text/plain; charset=utf-8"),
    ("md", "text/markdown; charset=utf-8"),
    ("xml", "application/xml"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("wasm", "application/wasm"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
];
```

Tests: `.css` is `text/css`; an unknown extension and a file with no extension
are both `application/octet-stream`; `.HTML` and `.Html` resolve like `.html`;
`.tar.gz` uses the last extension only.

### Step 4.3 — the listing

```rust
/// Renders a directory listing as one HTML document.
///
/// `entries` are file names, already sorted, directories first. `prefix` is the
/// request path the listing is for, always ending in `/` — the caller redirects
/// a directory request without a trailing slash before it ever gets here.
///
/// Two escapes, and they are different escapes for different sinks. The name is
/// HTML-escaped for the text node (`&`, `<`, `>`, `"`, `'`), because a file
/// named `<script>alert(1)</script>` is a thing a build tool produces by
/// accident and a stored-XSS otherwise. The href is percent-encoded, because a
/// name with a space, a `#` or a `?` produces a link that goes somewhere else.
pub fn render(prefix: &str, entries: &[Entry]) -> String
```

Tests:

```rust
    /// fails if a filename reaches the text node unescaped.
    #[test]
    fn a_filename_that_is_html_is_escaped_in_the_text_and_encoded_in_the_href() {
        let html = render("/", &[Entry::file("<script>alert(1)</script>")]);
        assert!(!html.contains("<script>alert(1)"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)"), "{html}");
        assert!(html.contains("href=\"%3Cscript%3Ealert(1)"), "{html}");
    }

    /// fails if a name with a space or a `#` produces a broken link — the
    /// second half of the escaping pair, and the half that is about
    /// correctness rather than security.
    #[test]
    fn a_filename_with_a_space_or_a_hash_produces_a_link_that_resolves() {
        let html = render("/docs/", &[Entry::file("release notes #2.md")]);
        assert!(html.contains("href=\"release%20notes%20%232.md\""), "{html}");
    }
```

### Step 4.4 — MUTATION

Escape the href with the HTML escaper instead of the percent encoder (one
plausible copy-paste). Expected:
`a_filename_with_a_space_or_a_hash_produces_a_link_that_resolves` fails while
the XSS test still passes — the reason both exist rather than one combined test.

### Step 4.5 — verification and gate

```bash
cargo test -p shep-cli --lib --bins --all-features   # baseline +7
```

---

## Task 5 — `serve::auth`: the creds file and the constant-time check

**Files:** `crates/shep-cli/src/serve/auth.rs`, `crates/shep-cli/Cargo.toml`.

Read decision 7. The mode check and the digest are both load-bearing.

### Step 5.1 — baseline

```bash
grep -c '^name = "ring"' Cargo.lock                 # 1
grep -c "^ring" crates/shep-cli/Cargo.toml          # 0 (grep exits 1)
cargo tree -p shep-cli | grep -c "^.*ring v"        # record the number and the version
cargo test -p shep-cli --lib --bins --all-features  # record
```

### Step 5.2 — the manifest

```toml
# `serve`'s basic-auth comparison: `digest` for SHA-256 and `constant_time`
# for the comparison itself. Adds ZERO crates to the tree — `tokio-rustls`
# above already pulls this exact version in for the bark dog's TLS (Cargo.lock
# resolves one `ring`), and this crate already pays ring's `cc` build script on
# every cross-compile. Named directly rather than reached through
# `tokio_rustls::rustls`: rustls does not re-export ring's digest or its
# constant-time comparison, and a crate that uses a dependency says so.
ring = { version = "0.17", default-features = false }
```

`default-features = false` and confirm what that leaves — ring's `alloc` is on
by default and `digest` needs it. If the build fails without a feature, add the
one it names and write the reason in the entry rather than reverting to
defaults. Re-run `cargo tree -p shep-cli | grep "ring v"` and confirm the count
and version are unchanged; record both numbers in the report.

### Step 5.3 — RED then GREEN

```rust
/// One `user:password` pair, read from a file.
///
/// No `Debug` derive, and not a redacted one either — this type has no `Debug`
/// at all (IR-41's stronger form). There is no line of output anywhere in shep
/// where printing a credential is the right answer, so the way to be sure of
/// that is for the type not to be printable.
pub struct Credentials {
    expected: Vec<u8>,
}

/// Reads `path`, refusing a file the box can read.
///
/// # Errors
/// - the file is unreadable, empty, has no `:`, or holds more than one
///   non-empty line;
/// - its mode is group- or world-readable (`mode & 0o077 != 0`).
///
/// Every message names the path and the problem and **never a byte of the
/// contents** — a parse error that quotes the offending line is how a password
/// reaches a terminal and a log.
pub fn load(path: &Path) -> Result<Credentials, AuthError>;

/// Whether `header` — the raw `Authorization` value — satisfies these
/// credentials.
///
/// Compares SHA-256 digests through `ring::constant_time::verify_slices_are_equal`
/// rather than the raw pair: that function returns `Err` immediately on a
/// length mismatch, so comparing the credentials directly would leak their
/// length. Two digests are always 32 bytes.
pub fn satisfies(&self, header: Option<&str>) -> bool;
```

Tests:

```rust
    /// fails if any of the four rejection shapes is accepted.
    #[test]
    fn only_the_exact_pair_is_accepted() {
        let creds = Credentials::from_pair("alice", "s3cret");
        let ok = format!("Basic {}", base64("alice:s3cret"));
        assert!(creds.satisfies(Some(&ok)));
        assert!(!creds.satisfies(None));
        assert!(!creds.satisfies(Some(&format!("Basic {}", base64("alice:s3cres")))));
        assert!(!creds.satisfies(Some(&format!("Basic {}", base64("alicf:s3cret")))));
        assert!(!creds.satisfies(Some("Basic")), "no credentials at all");
        assert!(!creds.satisfies(Some(&format!("Bearer {}", base64("alice:s3cret")))),
                "the scheme is part of the check");
    }

    /// fails if a creds file the group or the world can read is accepted. A
    /// credential every account on the box can read is not a credential.
    #[cfg(unix)]
    #[test]
    fn a_group_readable_creds_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds");
        std::fs::write(&path, "alice:s3cret\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = load(&path).unwrap_err();
        assert!(matches!(err, AuthError::Mode { .. }), "{err:?}");
        // positive control: the same file at 0600 loads.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&path).is_ok());
    }

    /// fails if a failure message ever carries the file's contents.
    #[test]
    fn no_error_message_quotes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds");
        std::fs::write(&path, "no-colon-here-s3cret\n").unwrap();
        let message = load(&path).unwrap_err().to_string();
        assert!(!message.contains("s3cret"), "{message}");
        assert!(message.contains("creds"), "it must still name the file: {message}");
    }
```

base64 decoding is ~20 lines and is written here rather than pulled in; the
`Authorization` value is the only base64 in this crate. Refuse a payload that is
not valid base64, and cap it at 1 KiB before decoding.

### Step 5.4 — MUTATION

Replace `verify_slices_are_equal(digest(a), digest(b))` with
`verify_slices_are_equal(a, b)` — the plausible simplification. Expected: **the
tests all still pass.** That is the point: timing is not observable from a unit
test, and this mutation is here to prove that the guard against it is the code
review and the comment, not the suite. Say so in the task report and leave the
digest in. A second mutation that *does* redden: change `satisfies` to compare
with `==` after a `to_lowercase()` — `only_the_exact_pair_is_accepted` fails on
the `alicf` case.

### Step 5.5 — verification and gate

```bash
grep -c "verify_slices_are_equal" crates/shep-cli/src/serve/auth.rs   # 1
grep -c "derive(Debug" crates/shep-cli/src/serve/auth.rs              # 0 for Credentials
cargo test -p shep-cli --lib --bins --all-features                    # baseline +6
```

---

## Task 6 — `serve::worker`: bind, accept, answer

**Files:** `crates/shep-cli/src/serve/worker.rs`,
`crates/shep-cli/src/serve/mod.rs`, `crates/shep-cli/Cargo.toml`.

`worker.rs` is `#[cfg(unix)]` — it binds a listener and reads files, and the
Windows leg of `run` refuses every verb before it could be reached, exactly as
`lookout` and `whistle` already are. `path`, `mime`, `listing` and `auth` stay
pure.

### Step 6.1 — baseline and the manifest

```bash
grep -c 'features = \["rt-multi-thread", "macros", "signal", "net", "time", "sync", "io-util"\]' crates/shep-cli/Cargo.toml   # 1
cargo test -p shep-cli --lib --bins --all-features   # record
```

Add `"fs"` to that tokio feature list, with a comment: it is `serve`'s
`tokio::fs::File` and `tokio::io::copy`, and it adds **zero** crates — tokio's
`fs` feature is `spawn_blocking` over `std::fs` and pulls in no new dependency.
Confirm with `cargo tree -p shep-cli | wc -l` before and after and record both
numbers; a change there means the feature did pull something and the comment is
wrong.

### Step 6.2 — the config and the loop

```rust
/// What a running `serve` worker was told to do. Built by the verb (Task 7)
/// from its flags, and by nothing else.
pub struct ServeConfig {
    /// The docroot, already canonical and already known to be a directory.
    pub root: PathBuf,
    /// Where to listen. Loopback unless the operator said otherwise.
    pub bind: SocketAddr,
    /// Serve `<root>/index.html` for a would-be 404 that accepts HTML.
    pub spa: bool,
    /// Render a listing for a directory with no index.
    pub listing: bool,
    /// Credentials every request must satisfy, if any.
    pub auth: Option<Credentials>,
}
```

The loop is the metrics dog's, deliberately — bind, `SIGINT`/`SIGTERM` select,
one task per connection, `Connection: close`, no keep-alive. Copy its shape
rather than inventing a second one, including the SIGTERM handler and the reason
for it (a worker that only handles SIGINT rides the whole kill ladder to SIGKILL
on every `shep stop`, which is slow and looks like a hang).

Per connection, in this exact order — the order is decision 6 and is pinned by a
test:

1. `http::read_request` with a 5-second read timeout. On any error, close
   without a reply (there is no well-formed request to answer).
2. **auth** — `Some(creds)` and `!satisfies` → 401 with
   `WWW-Authenticate: Basic realm="shep"`. Before path resolution, so an
   unauthenticated client cannot tell 400 from 404.
3. method — anything but `GET`/`HEAD` → 405 with `Allow: GET, HEAD`.
4. body — a `content-length` above 0, or any `transfer-encoding` header → 400.
   This server never reads a body, and a request that carries one would leave
   bytes in the socket.
5. `path::resolve` → 400 on `Err`, with the refusal's own one-line reason.
6. `path::contain` → 404 on `None`.
7. metadata: a directory → step 8; a regular file → step 9; anything else → 404.
8. directory: the request path did not end in `/` → **301** to the same path
   with one. Otherwise `index.html` inside it → step 9; else `listing` → render;
   else 404.
9. file: `write_head` with the MIME type, the metadata length, and
   `X-Content-Type-Options: nosniff`; then, for `GET` only,
   `tokio::io::copy(&mut file.take(len), &mut stream)`.
10. any 404 with `spa` set, `GET`/`HEAD`, and an `Accept` containing
    `text/html` → serve `<root>/index.html` with **200**.
11. one access-log line to stdout (decision 16).

`file.take(len)` rather than a bare copy: `content-length` was taken from the
metadata, and a file that grows between the two would otherwise desync the
framing. A file that *shrank* still desyncs — fewer bytes than declared — and
`Connection: close` is what turns that into a client-visible truncation rather
than a hang. Say so in the code comment.

### Step 6.3 — RED then GREEN: the integration tests

These bind a real listener on `127.0.0.1:0` (never a fixed port — the metrics
dog's own test helper says why) and speak HTTP over a `TcpStream`. Reuse the
metrics dog's `RunningDog` shape: a struct holding the address and the
`JoinHandle`, aborting on drop.

Every one of them wraps its request/response exchange in
`tokio::time::timeout(Duration::from_secs(5), …)` around the **async** call — a
timeout around a synchronous call bounds nothing (dead-check shape 4).

```rust
/// The traversal table again, this time end to end over a real socket, so it
/// covers the handler's ordering and not only the resolver.
///
/// Each case asserts the exact status. `assert_ne!(status, 200)` would pass
/// against a server that 500s on everything, which is the shape a security
/// test fails in.
#[tokio::test]
async fn every_traversal_shape_is_refused_over_a_real_socket() {
    let tree = tempdir_with(&[("index.html", "<h1>home</h1>"), ("assets/app.css", "body{}")]);
    std::fs::write(tree.path().parent().unwrap().join("secret.txt"), "nope").unwrap();
    let server = serve_on_free_port(config(&tree)).await;

    // positive control first: the server serves.
    assert_eq!(get(server.addr(), "/index.html").await.status, 200);
    assert_eq!(get(server.addr(), "/assets/app.css").await.status, 200);

    for (target, want) in [
        ("/../secret.txt", 400),
        ("/../../etc/passwd", 400),
        ("/%2e%2e/secret.txt", 400),
        ("/..%2fsecret.txt", 400),
        ("/x%00.png", 400),
        ("/a%0d%0aSet-Cookie:%20x", 400),
        ("/etc/passwd", 404),
        ("/nope.txt", 404),
    ] {
        let response = get(server.addr(), target).await;
        assert_eq!(response.status, want, "{target} answered {response:?}");
        assert!(!response.body.contains("nope"), "{target} leaked the file");
    }
}
```

```rust
/// fails if a symlink out of the docroot is served over the socket — the
/// handler's use of `contain`, not the pure function's own test.
#[tokio::test]
async fn a_symlink_out_of_the_docroot_is_a_404_and_not_a_body() { … }

/// fails if auth is checked after path resolution. An unauthenticated client
/// must not be able to tell a refused traversal (400) from a missing file
/// (404) — that difference maps the filesystem.
#[tokio::test]
async fn an_unauthenticated_request_is_401_whatever_the_path_says() {
    let server = serve_on_free_port(config_with_auth(&tree)).await;
    for target in ["/index.html", "/../secret.txt", "/nope.txt"] {
        let response = get(server.addr(), target).await;
        assert_eq!(response.status, 401, "{target}");
        assert!(response.headers.contains_key("www-authenticate"), "{target}");
    }
    // positive control: with the credential, the same three answer 200/400/404.
    assert_eq!(get_auth(server.addr(), "/index.html").await.status, 200);
    assert_eq!(get_auth(server.addr(), "/../secret.txt").await.status, 400);
    assert_eq!(get_auth(server.addr(), "/nope.txt").await.status, 404);
}

/// fails if a POST is served, or if the 405 forgets to say what is allowed.
#[tokio::test]
async fn a_method_other_than_get_or_head_is_405_with_an_allow_header() { … }

/// fails if a HEAD grows a body, or loses its Content-Length.
#[tokio::test]
async fn a_head_carries_the_length_and_no_body() { … }

/// fails if a directory without a trailing slash is served in place, which
/// breaks every relative link in the page it serves.
#[tokio::test]
async fn a_directory_without_a_trailing_slash_redirects_to_one() {
    let response = get(server.addr(), "/docs").await;
    assert_eq!(response.status, 301);
    assert_eq!(response.headers["location"], "/docs/");
}

/// fails if listing is on by default. Off is the decision (decision 9); a
/// default that enumerates filenames is the kind of default nobody revisits.
#[tokio::test]
async fn a_directory_with_no_index_is_404_unless_listing_is_on() { … }

/// fails if the SPA fallback fires for an asset request. A missing
/// `/assets/app.js` must 404, not answer HTML with a 200 — the browser error
/// that produces names a script type and never the missing file.
#[tokio::test]
async fn the_spa_fallback_serves_index_for_navigations_and_404s_for_assets() {
    let server = serve_on_free_port(config_with_spa(&tree)).await;
    let nav = get_with_accept(server.addr(), "/deep/link", "text/html").await;
    assert_eq!(nav.status, 200);
    assert!(nav.body.contains("<h1>home</h1>"));
    let asset = get_with_accept(server.addr(), "/assets/missing.js", "*/*").await;
    assert_eq!(asset.status, 404);
}

/// fails if `nosniff` is dropped, or if the MIME table is not reaching the
/// response.
#[tokio::test]
async fn a_css_file_is_served_as_css_with_nosniff() { … }

/// fails if a body larger than any buffer is truncated or read whole into
/// memory. 2 MiB is enough to cross every buffer in this path and small
/// enough to write in a test.
#[tokio::test]
async fn a_file_larger_than_the_buffers_is_served_whole() { … }
```

### Step 6.4 — MUTATION

Three, one at a time:

1. Move the auth check to **after** path resolution. Expected:
   `an_unauthenticated_request_is_401_whatever_the_path_says` fails on
   `/../secret.txt` (400 instead of 401). If it passes, the ordering is not
   actually being asserted.
2. Delete `path::contain`'s call from the handler and join the segments onto the
   root directly. Expected:
   `a_symlink_out_of_the_docroot_is_a_404_and_not_a_body` fails **and the whole
   traversal table still passes**, because the lexical walk already refused
   every case in it. That asymmetry is the reason the symlink test exists as its
   own case.
3. Make the SPA fallback unconditional (drop the `Accept` gate). Expected:
   `the_spa_fallback_serves_index_for_navigations_and_404s_for_assets` fails on
   the asset half.

### Step 6.5 — verification and gate

```bash
cargo test -p shep-cli --lib --bins --all-features   # baseline +11
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

---

## Task 7 — `shep serve`, the verb

**Files:** `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/commands/serve.rs`,
`crates/shep-cli/src/lib.rs`, `crates/shep-cli/tests/cli_e2e.rs`.

### Step 7.1 — baseline

```bash
grep -c "Serve" crates/shep-cli/src/cli.rs           # 0 (grep exits 1)
grep -c "unreachable!" crates/shep-cli/src/lib.rs    # 3
cargo test -p shep-cli --lib --bins --all-features   # record
```

Three `unreachable!`s, and the one at the bottom of the main dispatch is the
one that matters: it is what keeps the early-dispatch block and the main
`match` from drifting, and the new arm has to be named in its list or the
compiler will not tell you.

### Step 7.2 — the clap surface

Every field type here is portable (decision: the Windows cross-check). `bind` is
`std::net::IpAddr`, not a `SocketAddr`, so `--bind` and `--port` stay separate
knobs and neither has to parse the other's half.

```rust
/// Arguments to `shep serve`.
#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Directory to serve
    pub root: PathBuf,
    /// Port to listen on
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
    /// Address to bind. Loopback unless you say otherwise — a wider bind
    /// publishes every file under the directory to anything that can reach
    /// the port.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: std::net::IpAddr,
    /// Name for this sheep (default: the directory's own name)
    #[arg(long)]
    pub name: Option<String>,
    /// Fold to place this sheep in
    #[arg(long)]
    pub fold: Option<String>,
    /// Serve index.html for paths that do not exist, for a single-page app.
    /// Only for requests that accept HTML, so a missing script still 404s.
    #[arg(long)]
    pub spa: bool,
    /// List a directory that has no index.html. Off by default: a listing
    /// publishes every filename under it.
    #[arg(long)]
    pub listing: bool,
    /// File holding one `user:password` line, mode 0600, required on every
    /// request. Sent over plain HTTP — base64, not encryption.
    #[arg(long)]
    pub auth: Option<PathBuf>,
    /// Serve in this terminal instead of registering a sheep.
    ///
    /// This is also how the registered sheep runs: the command line in
    /// `shep describe` is this one with the flag on the end.
    #[arg(long)]
    pub foreground: bool,
}
```

### Step 7.3 — the two halves

`commands::serve::serve` does the refusals once, for both halves, so
`--foreground` and the registered sheep cannot disagree about what is valid:

- `root` missing, or not a directory → `Usage` (2), naming the path.
- `--auth` file unreadable, malformed, or group/world-readable → `InvalidConfig`
  (4). Loaded here even in the registering half, so a bad creds file is an
  immediate error rather than a sheep that crash-loops.
- `--spa` with no `<root>/index.html` → `InvalidConfig` (4).
- bind not loopback → a stderr notice naming the address and the docroot, and,
  without `--auth`, saying the files will be readable by anything that can reach
  the port. Notice, not refusal (decision 8).

Then either:

- `--foreground` → build `ServeConfig` and run `worker::run` until signalled.
  Reached **before** the `$SHEP_HOME` gate is not required — a foreground serve
  does not need a shepherd, but it does no harm to resolve paths first, and
  routing it like `lookout` (unlocked streams, dispatched before the locked
  block) is what keeps a long-lived verb from holding a `StdoutLock` for its
  whole life. That comment in `run` is not decoration; a held guard wedged the
  daemon once already.
- otherwise → `AppConfig` with `script = current_exe()`, `args` = the same
  invocation with an absolute canonical root and `--foreground` appended, and
  `Request::Start` through `connect_or_spawn_client`.

```rust
/// The sheep's own command line, rebuilt from the flags rather than from
/// `std::env::args`.
///
/// Rebuilt, not forwarded: the operator's `shep serve ./dist` carries a
/// relative path that resolves against *their* cwd, and the shepherd spawns
/// from its own. The canonical root goes in, and every flag is written in one
/// canonical order, so `shep describe` shows the same line for the same
/// server however it was typed.
fn sheep_args(root: &Path, args: &ServeArgs) -> Vec<String>
```

### Step 7.4 — tests

Unit, in `commands/serve.rs`:

```rust
    /// fails if the registered command line loses a flag, or carries the
    /// operator's relative path instead of the canonical one.
    #[test]
    fn the_registered_command_line_is_absolute_and_carries_every_flag() {
        let args = ServeArgs { spa: true, listing: true, port: 9000, .. };
        let built = sheep_args(Path::new("/srv/www"), &args);
        assert_eq!(built[0], "serve");
        assert_eq!(built[1], "/srv/www");
        assert!(built.contains(&"--foreground".to_string()));
        assert!(built.contains(&"--spa".to_string()));
        assert!(built.contains(&"--listing".to_string()));
        assert!(built.windows(2).any(|w| w == ["--port", "9000"]));
    }

    /// fails if the rebuilt line does not parse back to the same flags — the
    /// half a string-equality test cannot see.
    #[test]
    fn the_registered_command_line_parses_back_to_the_same_arguments() {
        use clap::Parser;
        let built = sheep_args(Path::new("/srv/www"), &args);
        let mut argv = vec!["shep".to_string()];
        argv.extend(built);
        let cli = Cli::try_parse_from(argv).expect("the line shep registers must parse");
        let Commands::Serve(parsed) = cli.command else { panic!("expected serve") };
        assert!(parsed.foreground);
        assert!(parsed.spa);
        assert_eq!(parsed.port, 9000);
        assert_eq!(parsed.root, PathBuf::from("/srv/www"));
    }
```

That second test is the one that matters. The first pins a vector; this one
pins the **round trip**, which is where a flag rename or a positional reorder
actually breaks — and it fails loudly the day someone adds a required argument
to `ServeArgs` without updating the builder.

e2e, in `tests/cli_e2e.rs`, against a fresh `$SHEP_HOME`:

```rust
/// fails if `shep serve` does not register a sheep, or registers one that
/// cannot actually serve. The assertion is an HTTP GET against the port, not
/// a `shep flock` row — a row says the process is up, and up is not serving.
#[test]
fn serve_registers_a_sheep_that_answers_on_its_port() { … }

/// fails if a missing docroot registers a crash-looping sheep instead of
/// failing immediately.
#[test]
fn serve_refuses_a_docroot_that_is_not_a_directory() {
    // exit code 2, stderr names the path, and `shep flock` is still empty.
}
```

The first e2e case needs a free port; take one by binding `127.0.0.1:0`,
reading the address, and dropping the listener before passing the number to
`shep serve`. That has a race and the race is acceptable in a test; what is not
acceptable is a hard-coded port, which fails on somebody's machine for reasons
unrelated to the change.

### Step 7.5 — MUTATION

Drop `--foreground` from `sheep_args`. Expected:
`the_registered_command_line_is_absolute_and_carries_every_flag` fails, and
`serve_registers_a_sheep_that_answers_on_its_port` fails differently and much
more slowly — the sheep starts, registers a second sheep, and crash-loops. Both
failures are informative; note in the report which one you would rather have
found first.

### Step 7.6 — verification and gate

```bash
grep -c "Commands::Serve" crates/shep-cli/src/lib.rs   # 0 → 2 (the dispatch arm and the unreachable list)
cargo test -p shep-cli --lib --bins --all-features     # baseline +2
cargo test -p shep-cli --test cli_e2e --all-features   # baseline +2
```

---

## Task 8 — `ExitCode::FlockEmpty` and the empty-flock watcher

**Files:** `crates/shep-cli/src/exit.rs`,
`crates/shep-cli/src/commands/empty.rs`.

Read decision 13. This task is pure plus one polling loop, and it lands before
either verb so the exit-code question is settled in its own review.

### Step 8.1 — baseline

```bash
grep -c "FlockEmpty\|flock_empty" crates/shep-cli/src/exit.rs   # 0 (grep exits 1)
grep -c "DaemonAlreadyRunning = 10" crates/shep-cli/src/exit.rs # 1
grep -c "resolves that" docs/specs/shep-v1.md                   # 1
cargo test -p shep-cli --lib --bins --all-features               # record
```

### Step 8.2 — the code

```rust
    /// The flock emptied and something in it had failed.
    ///
    /// `runtime`'s fail-fast status: no sheep is online any more and at least
    /// one ended `errored` — its restart budget was exhausted, or it never
    /// spawned. An orchestrator reads this as "restart the container".
    ///
    /// **Spec §9 specified code 2 for this and flagged the collision rather
    /// than resolving it**: 2 is clap's usage code, so a container that exits
    /// 2 leaves an operator unable to tell a bad flag from a dead app. Codes
    /// 0-10 were all spoken for, so this is a new row rather than a reused
    /// one, and the spec's table now carries it.
    ///
    /// A flock that emptied *cleanly* — every sheep `stopped`, none `errored`
    /// — exits `Success` instead. A one-shot job in a container finishing its
    /// work is not a failure.
    FlockEmpty = 11,
```

Plus its `code_str` arm (`"flock_empty"`), and its entry in
`every_exit_code_has_its_own_machine_readable_spelling`'s array — that test
enumerates by hand, so a new variant with no entry is invisible to it.

The watcher:

```rust
/// What one poll of the flock says about whether the foreground engine should
/// still be running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    /// At least one sheep is online, starting, or waiting to restart.
    Busy,
    /// Nothing is running and nothing failed.
    EmptyClean,
    /// Nothing is running and at least one sheep is `errored`.
    EmptyFailed,
}

/// Reads one `ListFlock` answer into a [`Sample`].
///
/// **Dogs do not count.** A `ProcessInfo` with `dog: Some(_)` is a metrics or
/// bark process the shepherd started for itself; a flock whose only remaining
/// entry is the metrics dog is an empty flock, and counting the dog would keep
/// a container alive forever after its app died. This is the one line of this
/// function that is easy to leave out and impossible to notice without a test.
///
/// `waiting-restart` counts as busy: a sheep between backoff attempts is
/// coming back, and treating it as gone is what the debounce below exists to
/// prevent in the first place.
pub fn sample(flock: &[ProcessInfo]) -> Sample;

/// Three consecutive empty samples, two seconds apart, before the engine
/// gives up — map.md's recorded contract, and not ceremony: a single sample
/// catches the gap between a sheep exiting and its backoff restart, and a
/// container torn down in that gap is a container torn down mid-recovery.
const STRIKES: u8 = 3;
const INTERVAL: Duration = Duration::from_secs(2);
```

### Step 8.3 — RED then GREEN

```rust
    /// fails if a dog keeps a dead flock alive. `shep enable metrics` inside
    /// a container would otherwise mean the container never exits.
    #[test]
    fn a_flock_of_nothing_but_dogs_is_empty() {
        let flock = vec![dog_info("metrics", ProcStatus::Online)];
        assert_eq!(sample(&flock), Sample::EmptyClean);
    }

    /// fails if a sheep between backoff attempts reads as gone.
    #[test]
    fn a_sheep_waiting_to_restart_is_busy() {
        assert_eq!(sample(&[sheep_info("web", ProcStatus::WaitingRestart)]), Sample::Busy);
    }

    /// fails if a clean stop and a failure report the same thing — the whole
    /// of decision 13's exit-code split.
    #[test]
    fn an_errored_sheep_makes_an_empty_flock_a_failed_one() {
        assert_eq!(sample(&[sheep_info("web", ProcStatus::Stopped)]), Sample::EmptyClean);
        assert_eq!(sample(&[sheep_info("web", ProcStatus::Errored)]), Sample::EmptyFailed);
        assert_eq!(
            sample(&[sheep_info("a", ProcStatus::Stopped), sheep_info("b", ProcStatus::Errored)]),
            Sample::EmptyFailed
        );
    }

    /// fails if the debounce is dropped or miscounted. On a paused clock, so
    /// this measures the interval rather than waiting six real seconds
    /// (IR-46: the forcing mechanism is the test's own `advance`).
    #[tokio::test(start_paused = true)]
    async fn three_consecutive_empty_samples_are_needed_and_one_busy_one_resets_them() { … }
```

The last test drives the loop against a scripted sequence of samples rather than
a real client — the loop takes its readings through a small trait or an
`impl Fn() -> Sample`, so nothing here needs a socket. That seam is also what
keeps the debounce testable when Task 9 wires a real `Client` behind it.

### Step 8.4 — MUTATION

Change `STRIKES` to 1. Expected:
`three_consecutive_empty_samples_are_needed_and_one_busy_one_resets_them`
fails. Then change the dog filter to count dogs. Expected:
`a_flock_of_nothing_but_dogs_is_empty` fails. Run them one at a time; the second
is the one that would ship silently, because every other test in this file uses
sheep.

### Step 8.5 — verification and gate

```bash
grep -c "flock_empty" crates/shep-cli/src/exit.rs   # 0 → 1
cargo test -p shep-cli --lib --bins --all-features   # baseline +5
```

`every_rpc_error_code_maps_to_a_distinct_nonzero_exit_code` and
`every_exit_code_has_its_own_machine_readable_spelling` both still pass — the
second only if the new variant was added to its array, which is why it is called
out above.

---

## Task 9 — the foreground engine, and `shep runtime`

**Files:** `crates/shep-cli/src/commands/daemon.rs` (split `boot_supervisor`
out), `crates/shep-cli/src/commands/foreground.rs` (new),
`crates/shep-cli/src/commands/runtime.rs` (new),
`crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/lib.rs`.

Read decision 12. This task builds the engine and `runtime`'s inline path; Task
10 adds the PID-1 split on top and Task 11 adds `dev` beside it.

### Step 9.1 — baseline

```bash
grep -c "pub async fn run_daemon" crates/shep-cli/src/commands/daemon.rs   # 1
grep -c "fn boot_supervisor" crates/shep-cli/src/commands/daemon.rs        # 0 (grep exits 1)
grep -c "Runtime(" crates/shep-cli/src/cli.rs                              # 0 (grep exits 1)
cargo test -p shep-cli --lib --bins --all-features                          # record
cargo test -p shep-daemon --test daemon_e2e --all-features                  # record — this task
                                                                            # changes the daemon's
                                                                            # boot call site
```

### Step 9.2 — the split

`run_daemon`'s body up to and including `boot(...)` becomes:

```rust
/// Loads config, installs the log subscriber, and boots the supervisor —
/// everything [`run_daemon`] does except serve.
///
/// Split out for `commands::foreground`, which needs the booted daemon in
/// hand rather than a call that blocks until shutdown: it spawns `run()` as a
/// task and then talks to the same supervisor over its own socket, like any
/// other client. Nothing about the boot differs between the two callers, and
/// the split is what keeps that true.
pub async fn boot_supervisor(
    paths: ShepPaths,
    args: &DaemonArgs,
) -> Result<shep_daemon::boot::RunningDaemon, DaemonRunError>
```

and `run_daemon` becomes `boot_supervisor(paths, args).await?.run().await.map_err(DaemonRunError::Run)`.
No behaviour change; `daemon_e2e` and the `daemon` arm of `cli_e2e` are the
proof.

### Step 9.3 — the engine

```rust
/// What the foreground engine should do, for the two verbs that use it.
pub struct ForegroundOptions {
    /// Where this flock lives. `dev` computes its own; `runtime` takes the
    /// ordinary `$SHEP_HOME`.
    pub paths: ShepPaths,
    /// Apps to start once the shepherd is up.
    pub apps: Vec<AppConfig>,
    /// Stop and delete the flock on the way out. `dev` does; `runtime` does
    /// not — the container is going away and a delete would only slow the
    /// shutdown down.
    pub tidy_up: bool,
}

/// Boots a shepherd in this process, starts `apps`, streams their bleats to
/// stdout, and returns when nothing is online or a signal arrives.
///
/// The shepherd is reached **over its own socket**, not through
/// `RunningDaemon::context()`, whose own doc reserves that handle for
/// `tests/daemon_e2e.rs`. Three things come out of that: `shep flock` from a
/// second terminal (or `docker exec`) works while this is running, which is
/// most of the reason this mode exists; the start path is the one `shep start`
/// already covers; and shutdown is `Request::KillDaemon`, the message
/// `shep kill` sends.
///
/// Signals need no handler here. `shep_daemon::boot` installs SIGINT and
/// SIGTERM handlers before this function ever has a client, and they run the
/// flock's stop ladder and end `run()`. Installing a second set here would
/// race the first; instead the interrupt this passes to `bleats` completes when
/// **either** the supervisor task finishes **or** the flock has been empty for
/// three polls.
pub async fn run(options: ForegroundOptions) -> ExitCode
```

Order, and each step's failure exit code:

1. `boot_supervisor(paths, &DaemonArgs { no_restore: true, foreground: true })`
   — `no_restore` because a container and a dev session both start from their
   Flockfile, never from a roll somebody saved on this machine last week. On
   `Err` → `daemon_exit_code(&err)`, which already maps `Config` to 4 and
   `AlreadyRunning` to 10. **10 is the right answer here and is worth a
   sentence in the report**: two `shep runtime` in one container, or a
   `shep dev` while another is up, is exactly "another shepherd already holds
   this `$SHEP_HOME`".
2. `tokio::spawn(daemon.run())`, keeping the handle.
3. `Client::connect(&paths.socket)` — the listener is bound by the time `boot`
   returns, so this needs no retry. If it fails anyway, kill the task and exit
   `DaemonUnreachable` (5).
4. `Request::Start { apps }` with `START_DEADLINE`. A daemon-side failure maps
   through the existing `ExitCode::from(&RequestError)`.
5. `bleats::bleats_with_signal(&client, streams, fmt, quiet, &BleatsArgs {
   selector: "all", no_follow: false, err: false, out: false },
   until_empty_or_supervisor_exit)`. This is why that function takes an
   injectable interrupt future; nothing new is needed in the log plane.
6. If `tidy_up`: `Request::Stop` then `Request::Delete` over the same client,
   selector `all`.
7. `Request::KillDaemon`, then await the supervisor task.
8. Exit code: `Sample::EmptyFailed` → `FlockEmpty` (11); `Sample::EmptyClean` or
   a signal → `Success`; a supervisor task that returned `Err` → `Failure` (1),
   which wins over both.

**Streams are unlocked**, and this is the third verb in the file that needs the
comment in `run` explaining why: this one runs until the flock empties, and a
`StderrLock` held across that wedges the first record written from a tokio
worker thread — which, in this verb, is the supervisor's own logging, in the
same process. `daemon`, `bleats` and `lookout` are the existing three; runtime
and dev make five. Dispatch both before the locked block.

### Step 9.4 — `shep runtime`

```rust
/// Arguments to `shep runtime`.
#[derive(Debug, clap::Args)]
pub struct RuntimeArgs {
    /// Flockfile to run (default: discovered in the current directory)
    pub target: Option<String>,
    /// Run the supervisor in this process rather than splitting off an init.
    ///
    /// Set by the init half of a PID-1 split when it re-execs this binary,
    /// and never by a person. Also a safety catch: with this set the split
    /// cannot happen, so a mis-read pid can never produce a fork loop.
    #[arg(long, hide = true)]
    pub supervise: bool,
}
```

`target` resolves through the existing `lifecycle::resolve_target` when given,
and through `shep_core::config::flockfile::discover(&cwd)` when not. Nothing
discovered and no target → `Usage` (2) naming the ten filenames discovery looks
for.

**`$SHEP_HOME` in a container.** `resolve_paths` needs `--home`, `$SHEP_HOME`,
or `$HOME`, and a container often has none of them. `runtime` does not invent a
new default: it fails with `Usage` (2) and a message that names the flag and the
variable, and Task 12's Docker example sets `ENV SHEP_HOME=/shep`. Inventing
`/var/lib/shep` here would be a new filesystem convention created at 2am for one
verb.

### Step 9.5 — tests

```rust
    /// fails if `runtime` stops parsing, or if `--supervise` becomes visible.
    /// It is the init's own re-exec flag; a person typing it should not find
    /// it in `--help`.
    #[test]
    fn runtime_parses_and_its_supervise_flag_is_hidden() { … }

    /// The Task 1 sibling: the alias vector actually reaches this verb.
    #[test]
    fn the_runtime_alias_vector_parses_to_the_runtime_command() {
        let argv = alias_argv("runtime", vec!["shep-runtime".into(), "--supervise".into()]);
        let cli = Cli::try_parse_from(argv).unwrap();
        let Commands::Runtime(args) = cli.command else { panic!("expected runtime") };
        assert!(args.supervise);
    }
```

e2e, the one that matters, in `tests/cli_e2e.rs` against a fresh home:

```rust
/// fails if `shep runtime` does not exit on its own when the flock empties,
/// or exits with the wrong code for the reason it emptied.
///
/// Two runs of the same shape: one app that exits 0 with `autorestart = false`
/// (exit 0), and one that exits 1 with `max_restarts = 1` (exit 11). The
/// second is decision 13's whole contract.
///
/// Bounded by the harness's own wait on the child with a deadline — a
/// `runtime` that never exits must fail this test rather than hang the suite.
#[test]
fn runtime_exits_when_the_flock_empties_with_a_code_that_says_why() { … }
```

That test takes at least 6 seconds per run (three strikes at two seconds); it is
an integration test in a suite that already spends ~47 s in `cli_e2e`. Do not
shorten the debounce to make it fast — put the two runs in one test so the boot
cost is paid once.

### Step 9.6 — MUTATION

Change step 8's mapping so `Sample::EmptyClean` also returns `FlockEmpty`.
Expected: the exit-0 half of
`runtime_exits_when_the_flock_empties_with_a_code_that_says_why` fails. Then
change `no_restore: true` to `false`. Expected: nothing fails — no test covers
it, because a fresh `$SHEP_HOME` has no roll to restore. Note that gap in the
report rather than papering over it; the honest fix is one more e2e case that
`shep save`s a flock and then runs `runtime` with a different Flockfile, and it
is worth writing if the phase has room.

### Step 9.7 — verification and gate

```bash
grep -c "Commands::Runtime" crates/shep-cli/src/lib.rs   # 0 → 2
cargo test -p shep-cli --lib --bins --all-features        # baseline +2
cargo test -p shep-cli --test cli_e2e --all-features      # baseline +1
cargo test -p shep-daemon --test daemon_e2e --all-features # unchanged from baseline
```

---

## Task 10 — the PID-1 init split and the reaper

**Files:** `crates/shep-cli/src/commands/runtime.rs`,
`crates/shep-cli/src/commands/reap.rs` (new),
`crates/shep-cli/tests/reaper.rs` (new, Linux-only body).

Read decision 14 before writing a line. The reason this is its own task, after
`runtime` already works, is that the split is the piece most likely to be got
wrong and it must be reviewable against a `runtime` that is already green.

### Step 10.1 — baseline

```bash
grep -rn "set_child_subreaper" crates | wc -l                    # 0
grep -rn "waitpid" crates | wc -l                                # 2 (both in shep-client)
grep -c 'features = \["fs"\]' crates/shep-cli/Cargo.toml         # whatever Task 6 left; nix is
                                                                  # what matters here
grep -n 'nix = { workspace = true' crates/shep-cli/Cargo.toml    # the cfg(unix) entry
cargo test -p shep-cli --lib --bins --all-features                # record
```

`nix::sys::wait::waitpid` lives behind nix's `process` feature, which the
workspace entry already enables (`features = ["signal", "process", "user"]`), so
this task adds no feature. Confirm rather than assume — build once and read the
error if there is one.

### Step 10.2 — the pure half

The platform-arm trap this phase was warned about lands here: a `cfg(linux)` arm
that calls something not available on macOS compiles fine on this machine
because the arm is cfg-ed away. So the decision logic has **no `cfg` at all**:

```rust
/// What one `waitpid` return means to the init loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// The supervisor child exited. Carries the status to exit with.
    Supervisor(u8),
    /// Somebody else's orphan, reaped and forgotten.
    Orphan,
    /// Nothing is ready right now — stop looping and wait for the next
    /// SIGCHLD or tick.
    Nothing,
    /// There are no children at all. The supervisor is already accounted for
    /// by an earlier `Supervisor`; this is the loop's other exit.
    NoChildren,
}

/// Classifies one `waitpid(-1, WNOHANG)` result.
///
/// `cfg`-free and pure, deliberately: the platform arms around it are two
/// lines each, so a Linux-only method call cannot hide inside a branch this
/// machine never compiles.
///
/// The exit status follows the shell convention the container ecosystem reads:
/// a signalled child is `128 + signal`, so a supervisor killed by SIGKILL
/// reports 137 exactly as `docker inspect` expects.
pub fn classify(status: WaitOutcome, supervisor: i32) -> Reaped;
```

`WaitOutcome` is this module's own small enum (`Exited { pid, code }`,
`Signaled { pid, signal }`, `StillAlive`, `NoChildren`), constructed by the thin
`cfg(unix)` shim from `nix::sys::wait::WaitStatus`. That shim is the only
platform code in the file, and it is a `match` with no logic in it.

Tests over `classify` are exhaustive and need no processes:

```rust
    #[test]
    fn the_supervisor_is_told_apart_from_every_orphan() {
        assert_eq!(classify(WaitOutcome::Exited { pid: 7, code: 3 }, 7), Reaped::Supervisor(3));
        assert_eq!(classify(WaitOutcome::Exited { pid: 8, code: 3 }, 7), Reaped::Orphan);
    }

    /// fails if a signalled supervisor reports 0, or reports the raw signal
    /// number — `docker inspect` reads 128+n and an orchestrator restarting on
    /// a non-zero status would see a clean exit for a SIGKILL.
    #[test]
    fn a_signalled_supervisor_exits_128_plus_the_signal() {
        assert_eq!(classify(WaitOutcome::Signaled { pid: 7, signal: 9 }, 7), Reaped::Supervisor(137));
    }
```

### Step 10.3 — the init loop

```rust
/// Runs as PID 1: spawns the supervisor, forwards signals to it, and reaps
/// every process the kernel reparents here.
///
/// **`std::process::Command`, never `tokio::process`.** tokio reaps its own
/// children by calling `waitpid` on their pids when SIGCHLD fires; a blind
/// `waitpid(-1)` loop in the same process would race it and consume statuses
/// the supervisor needs — spec §4 promises exit code and signal are recorded
/// exactly. That race is the whole reason PID 1 is a separate process here and
/// not a loop inside the supervisor.
///
/// `child.wait()` is never called for the same reason: this loop is the only
/// thing in this process that waits.
///
/// The child's argv is this process's own, minus argv[0], plus `--supervise` —
/// so `shep runtime x` spawns `shep runtime x --supervise` and `shep-runtime x`
/// spawns `shep-runtime x --supervise`, and the alias keeps working (Task 1).
///
/// Forwarded signals go to the child's pid, not its process group: the child
/// owns its flock's groups and runs its own stop ladder over them.
///
/// It does not wait for orphans once the supervisor is gone. The container is
/// being torn down; a wedged orphan must not hold the exit open.
async fn run_init() -> ExitCode
```

The loop: a `tokio::select!` over `signal(SIGCHLD)`, the four forwarded signals,
and a 1-second `interval` as a backstop — signal delivery coalesces, and a tick
costs nothing next to a container that never exits because one SIGCHLD arrived
while another was being handled. On any of the three, drain with
`waitpid(Pid::from_raw(-1), WNOHANG)` until `StillAlive` or `NoChildren`.

**Nothing in this function may panic.** It is PID 1: a panic takes the container
down with no diagnostic. No `unwrap`, no `expect`, no indexing; every error is
logged to stderr and the loop continues, except a failure to spawn the child at
all, which exits non-zero immediately.

### Step 10.4 — the branch, and the test that reaches it

```rust
if args.supervise || std::process::id() != 1 {
    return foreground::run(options).await;
}
run_init().await
```

```rust
    /// fails if the split fires anywhere but PID 1, which would mean every
    /// developer running `shep runtime` on a laptop gets two processes and a
    /// re-exec.
    ///
    /// The test process is never PID 1, so this asserts the branch it actually
    /// takes rather than mocking the pid.
    #[test]
    fn the_init_split_does_not_fire_outside_pid_one() {
        assert_ne!(std::process::id(), 1, "a test harness is never PID 1");
        assert!(!should_split(&RuntimeArgs { supervise: false, .. }));
        assert!(!should_split(&RuntimeArgs { supervise: true, .. }));
    }
```

`should_split` is extracted precisely so this test can exist: it takes the pid
as a parameter, so the case that cannot be reached in a test can still be
asserted.

```rust
    #[test]
    fn a_pid_of_one_splits_unless_supervise_says_otherwise() {
        assert!(should_split(1, false));
        assert!(!should_split(1, true));
        assert!(!should_split(4242, false));
    }
```

### Step 10.5 — the Linux-only reaper test

`crates/shep-cli/tests/reaper.rs`. The whole point is to exercise the drain loop
against a **real orphan**, and getting one without being PID 1 needs Linux's
child-subreaper bit:

```rust
/// fails if the drain loop leaves a reparented orphan as a zombie — the
/// container's actual failure mode, a process table that fills up over days.
///
/// Linux only. `PR_SET_CHILD_SUBREAPER` makes this test process the reaper for
/// its own descendants, which is the only way to observe reparenting without
/// being PID 1 or having a PID namespace. macOS has no equivalent, so this
/// skips with a printed reason there rather than pretending to cover it.
#[cfg(target_os = "linux")]
#[test]
fn a_reparented_orphan_is_reaped() {
    nix::sys::prctl::set_child_subreaper(true).expect("linux supports this since 3.4");
    // A shell that forks a short-lived grandchild and exits immediately, so
    // the grandchild is reparented here.
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "(sleep 0.2; exit 3) & exit 0"])
        .spawn()
        .unwrap();
    let _ = child.wait();
    // Drain, with a bounded number of attempts — never `loop {}`.
    …
    assert!(reaped_an_orphan, "the grandchild must have been reaped here");
}
```

Confirm `nix::sys::prctl::set_child_subreaper` exists in nix 0.29 before writing
this against it (`cargo doc -p nix --open`, or read the crate source in
`~/.cargo/registry`). If it does not, the fallback is to skip this test with a
printed reason and say so in the report — **not** to write a version that passes
without observing a real reparent. This machine is macOS, so this test is CI's
to run; note in the report that you could not execute it locally.

### Step 10.6 — MUTATION

Change the drain to `waitpid(child_pid, WNOHANG)` — reap only our own child,
which is what a reader who has not read decision 14 would write. Expected:
`a_reparented_orphan_is_reaped` fails **on Linux only**, and every other test in
this phase still passes, on both platforms. That asymmetry is the finding:
without the Linux test, the mutation is invisible, and the plan would be
claiming coverage it does not have.

Second: delete the `--supervise` guard from `should_split`. Expected:
`a_pid_of_one_splits_unless_supervise_says_otherwise` fails. If it did not, PID
1 would re-exec itself forever.

### Step 10.7 — verification and gate

```bash
grep -c "tokio::process" crates/shep-cli/src/commands/reap.rs   # 0 (grep exits 1) — load-bearing
grep -c "unwrap()\|expect(" crates/shep-cli/src/commands/reap.rs # 0 outside `mod tests`
cargo test -p shep-cli --lib --bins --all-features                # baseline +5
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```

The `tokio::process` grep is the mechanical form of decision 14's whole
argument, which is why it is a check and not a comment.

---

## Task 11 — `shep dev`

**Files:** `crates/shep-cli/src/commands/dev.rs` (new),
`crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/lib.rs`,
`crates/shep-cli/tests/cli_e2e.rs`.

Read decision 15. Most of this task is the engine Task 9 built; what is new is
the home, the forced watch, and the tidy-up.

### Step 11.1 — baseline

```bash
grep -c "Dev(" crates/shep-cli/src/cli.rs                  # 0 (grep exits 1)
grep -rn "SHEP_DEV_HOME" crates | wc -l                    # 0
grep -c "shep-dev" docs/specs/shep-v1.md                   # 2
cargo test -p shep-cli --lib --bins --all-features          # record
```

### Step 11.2 — the code

```rust
/// Arguments to `shep dev`.
///
/// No `--home` of its own, and the global one does not apply — see
/// [`dev_home`].
#[derive(Debug, clap::Args)]
pub struct DevArgs {
    /// Script or Flockfile to run (default: discovered in this directory)
    pub target: Option<String>,
    /// Name for this sheep (script form only)
    #[arg(long)]
    pub name: Option<String>,
}
```

```rust
/// Where a dev flock lives: `$SHEP_DEV_HOME`, else `~/.shep-dev`.
///
/// **`--home` and `$SHEP_HOME` are ignored**, and passing `--home` explicitly
/// gets a stderr notice saying so. Isolation is the whole feature spec §9 names
/// for this verb, and `--home` carries `env = "SHEP_HOME"` — so an operator who
/// exports it for their real flock would otherwise get a `shep dev` that shares
/// it, and `dev`'s forced `watch = true` would be written onto their production
/// apps.
///
/// `$SHEP_DEV_HOME` is not in the spec. It exists because the e2e tier needs a
/// knob that a developer's own environment cannot collide with; without it,
/// `cargo test` writes into whoever's real `~/.shep-dev`.
fn dev_home(env: &impl Fn(&str) -> Option<String>, home_dir: &Path) -> ShepPaths
```

The verb:

1. Notice on stderr if `--home` was given.
2. Resolve `target` — a script path, a Flockfile, or discovery — through
   `lifecycle::resolve_target`, the same function `shep start` uses, so `shep dev
   ./server.js` and `shep start ./server.js` cannot disagree about what a target
   means.
3. **Force `watch = true` on every resolved app**, and say so on stderr once,
   naming the apps. Forcing silently would make a Flockfile's `watch = false`
   look ignored, and it is being ignored — deliberately, which is exactly the
   thing to say out loud.
4. `foreground::run(ForegroundOptions { paths: dev_home(...), apps, tidy_up: true })`.

`tidy_up: true` is the difference that matters in daily use: Ctrl-C out of a
`shep dev` leaves nothing running and no shepherd on `~/.shep-dev`. A dev mode
that leaks a supervisor is a dev mode people stop trusting.

### Step 11.3 — tests

```rust
    /// fails if dev starts honouring $SHEP_HOME, which would put a forced
    /// `watch = true` onto the operator's real flock.
    #[test]
    fn the_dev_home_ignores_shep_home_and_prefers_its_own_variable() {
        let env = |key: &str| match key {
            "SHEP_HOME" => Some("/srv/production".to_string()),
            _ => None,
        };
        let paths = dev_home(&env, Path::new("/home/rin"));
        assert_eq!(paths.home, Path::new("/home/rin/.shep-dev"));

        let env = |key: &str| match key {
            "SHEP_HOME" => Some("/srv/production".to_string()),
            "SHEP_DEV_HOME" => Some("/tmp/t1".to_string()),
            _ => None,
        };
        assert_eq!(dev_home(&env, Path::new("/home/rin")).home, Path::new("/tmp/t1"));
    }

    /// fails if watch is not forced, or if it is forced by rebuilding the
    /// AppConfig and dropping the rest of the Flockfile in the process.
    #[test]
    fn every_app_gets_watch_and_keeps_everything_else() {
        let mut apps = vec![AppConfig::minimal("web", "./server.js")];
        apps[0].watch = false;
        apps[0].max_memory = Some(MemSize::from_bytes(1024));
        force_watch(&mut apps);
        assert!(apps[0].watch);
        assert_eq!(apps[0].max_memory, Some(MemSize::from_bytes(1024)));
        assert_eq!(apps[0].name, "web");
    }
```

e2e:

```rust
/// fails if `shep dev` leaves a shepherd or a flock behind. Runs a script that
/// exits immediately with `autorestart = false`, so the auto-exit fires, then
/// asserts the dev home has no live socket and that `SHEP_DEV_HOME=<tmp> shep
/// flock` reports nothing.
///
/// `SHEP_DEV_HOME` points at a tempdir — without it this test would write into
/// the developer's own `~/.shep-dev`, which is decision 15's second reason for
/// the variable existing.
#[test]
fn dev_tidies_up_after_itself() { … }
```

And delete the `#[ignore]` from Task 1's
`the_alias_binaries_exist_and_reach_their_own_verbs`: both verbs exist now.

### Step 11.4 — MUTATION

Set `tidy_up: false` for dev. Expected: `dev_tidies_up_after_itself` fails on
the "flock is empty afterwards" assertion. Then make `dev_home` fall back to
`$SHEP_HOME`. Expected:
`the_dev_home_ignores_shep_home_and_prefers_its_own_variable` fails on its first
half — the half that protects a production flock.

### Step 11.5 — verification and gate

```bash
grep -c "Commands::Dev" crates/shep-cli/src/lib.rs        # 0 → 2
grep -c "ignore = " crates/shep-cli/tests/cli_e2e.rs      # 1 → 0 (0 before Task 1)
cargo test -p shep-cli --lib --bins --all-features         # baseline +2
cargo test -p shep-cli --test cli_e2e --all-features       # baseline +2 (dev's, plus the
                                                           # un-ignored alias case)
```

---

## Task 12 — docs, ledger, changelogs, and the two claims this phase falsifies

**Files:** `docs/specs/shep-v1.md`, `docs/specs/deferred.md`,
`docs/systematic-refactor/refactor-workspace/map.md`, `docs/migration.md`,
`docs/releasing.md`, `CLAUDE.md`, `crates/shep-cli/CHANGELOG.md`,
`crates/shep-cli/README.md`.

### Step 12.1 — baselines, all four of them checkable

```bash
grep -c "resolves that" docs/specs/shep-v1.md                    # 1
grep -cF 'one `[[bin]]`' docs/specs/deferred.md                  # 1
grep -c "axum" docs/specs/deferred.md                            # 1
grep -c "not built" crates/shep-cli/src/lib.rs                   # 1 (Task 1 removed the other)
grep -rn "shep-cli is \[\[bin\]\]-only\|--bins" CLAUDE.md | wc -l # 2
grep -c "Windows is 0%" CLAUDE.md                                 # 1
```

### Step 12.2 — spec §9

Two edits, both of which the spec asks for by name:

1. The exit-code table gains a row: `| 11 | flock empty | The foreground flock
   emptied with a sheep in \`errored\`. \`runtime\`'s fail-fast status. |`
2. The paragraph beginning "Code 2 is claimed by clap" stops saying "`runtime`
   resolves that when it is built" and says what the resolution was, in one
   sentence, with the reason (an orchestrator cannot act on a status that means
   both "bad flag" and "dead app"). Check: `grep -c "resolves that"` goes
   `1 → 0`.

Also correct §9's **serve** line: it names axum and tower-http, which Rin
overruled. Replace with the hand-rolled surface and a pointer to the reasoning,
and add the three flags the spec did not anticipate (`--listing` default-off,
`--spa`'s `Accept` gate, `--bind`), each in half a line. A spec that still names
a dependency the implementation deliberately does not have is the exact drift
`deferred.md` exists to stop hiding.

### Step 12.3 — the ledger

Delete the three entries this phase closes — **serve**, **dev / runtime**, and
the `[[bin]]` sentence inside the latter — and move what actually shipped into
the "Not deferred" section, with the divergences named:

- serve is hand-rolled, not axum/tower-http (Rin's ruling, 2026-08-15);
- directory listing is **off** by default, where pm2's is on;
- no range requests, no conditional requests, no hidden-file filtering, no
  `PM2_SERVE_*` compatibility — each a v1.1 candidate with one line of reason;
- exit code 11 exists and code 2 is clap's alone;
- `runtime` splits into an init process when it is PID 1, rather than reaping in
  the supervisor's own process.

Checks: `grep -c "axum" docs/specs/deferred.md` goes `1 → 0`;
`grep -cF 'one `[[bin]]`'` goes `1 → 0`.

### Step 12.4 — CLAUDE.md, and the rule this phase inverts

`CLAUDE.md` states the shep-cli test command as a fact about this repo, and
after Task 1 that fact is false: the crate has a library and `--bins` runs
almost nothing. Correct it to `cargo test -p shep-cli --lib --bins
--all-features`, and add one sentence saying the crate became a library in Phase
15 and why, so the next reader does not "fix" it back.

Also update the status paragraph: Phase 15 merged, the v1.0 CLI surface closed,
Windows still 0%.

### Step 12.5 — map.md drift entries

map.md is the design record and three of its entries are now wrong in ways worth
recording rather than rewriting:

- `serve.rs` says axum + tower-http and `PM2_SERVE_*` compat. Add a **Drift
  (Phase 15, recorded)** note: shipped as `serve/` with five modules, hand-rolled
  on `http.rs` (which moved up out of `dog/`), no env compat.
- `runtime.rs` says "auto-exit fail_count 3 / 2s / code 2" and "subreaper +
  WNOHANG loop". The debounce shipped exactly as written; the code is 11 and the
  reaper is a separate init process. Record both, and record **why** the
  in-process loop was rejected — that reasoning is the part a future reader
  needs, and it is the one thing map.md's sketch got wrong.
- `main.rs` says "multi-call binary (argv[0] dispatch)". Shipped as three
  `[[bin]]` targets, so argv[0] is never read; record it with decision 2's
  one-line reason.

### Step 12.6 — the rest

- `docs/migration.md`: a `pm2 serve` → `shep serve` section naming the listing
  default flip and the missing `PM2_SERVE_*` variables, and a `pm2-runtime` →
  `shep runtime` section with a Dockerfile that sets `ENV SHEP_HOME=/shep` and
  uses `ENTRYPOINT ["shep-runtime"]`.
- `docs/releasing.md`: the artefact list now has **three** binaries, not one.
  Whatever that file says about what `cargo install shep-cli` produces needs the
  other two named. Read it before editing — it was written the night before this
  plan and this is the one place a stale count is invisible until a release.
- `crates/shep-cli/CHANGELOG.md`: one entry per verb plus the library
  extraction, which is the entry that matters — it is the only user-visible
  packaging change in the phase.
- `crates/shep-cli/README.md` (it is the crate's `readme` and therefore its
  crates.io front page): the three binaries, and the one-line "embedding shep is
  `shep-client`, not this crate".
- **`SECURITY.md`**, if the repo has one by the time this runs (spec §10 names
  it, and Phase 14 may have added it): `serve`'s traversal posture, the accepted
  TOCTOU, the loopback default and the plain-HTTP basic auth all belong there in
  a paragraph.

### Step 12.7 — verification and the phase gate

Every grep above, plus the whole phase gate, plus both cross-checks, plus the
serial run and the benches gates that make it a phase merge rather than a task:

```bash
cargo test --workspace --all-features -- --test-threads=1
```

The serial run is not ceremony — it was red on `main` before Phase 5 and it
caught a real regression in Phase 6. This phase adds a verb that binds a port
and two that boot a supervisor; if anything in it depends on test parallelism
for a free port or a free `$SHEP_HOME`, this is the run that says so.

---

## What this phase deliberately does not build

Collected so the omissions are in one place, each with the reason and where it
is argued:

- **axum and tower-http.** Rin's ruling, decision 3. `Cargo.lock` still has
  neither after this phase, and that is a check in Task 12.
- **Range requests, conditional requests, ETags, compression, keep-alive, TLS,
  HTTP/2.** Not in spec §9's sentence about serve; decision 4. The visible cost
  is video seeking and a full re-read per request.
- **Hidden-file filtering.** A `.env` in a docroot the operator chose to publish
  is published; `--help` says so. Decision 4.
- **`PM2_SERVE_*` environment compatibility.** shep reads `SHEP_`-prefixed
  variables and nobody else's. Decision 4, and named in `docs/migration.md` so a
  migrating operator finds it.
- **A second embedding API.** shep-cli's library surface is three functions;
  embedding shep is `shep-client`'s job. Decision 1.
- **argv[0] sniffing.** Three real binaries know what they are at compile time.
  Decision 2.
- **An in-process zombie reaper.** It would race tokio's own child reaping and
  corrupt the exit statuses spec §4 promises are exact. Decision 14.
- **A `$SHEP_HOME` default for containers.** `runtime` fails with a message
  naming the flag rather than inventing `/var/lib/shep` at 2am. Task 9.
- **The `web.rs` HTTP interface** map.md lists next to `serve.rs`. It is not in
  spec §9's verb list and is not in this phase; if it is wanted, it is a v1.1
  item and the ledger should say so.

## Open decisions, and which of them are Rin's

Everything above is decided. Three things are worth her eye, and none of them
blocks the phase:

1. **Exit code 11's semantics** (decision 13). The plan makes a clean emptying
   exit 0 and only an `errored` one exit 11; pm2-runtime exits non-zero either
   way. If Rin wants "the flock emptied at all" to be a failure — for an
   orchestrator that should restart a container whatever the reason — that is a
   one-line change in Task 9's step 8 and one test. The plan takes the position
   that a one-shot job finishing is not a failure.
2. **Directory listing off by default** (decision 9). It is a deliberate
   divergence from pm2, argued from shep's own posture on every other exposure
   knob. If Rin would rather match pm2 for migration reasons, it is one default
   and one test.
3. **`serve`'s TOCTOU** (decision 5). Accepted in writing, with the argument
   that anyone who can plant a symlink in the docroot can plant an
   `index.html`. Closing it properly needs `openat2(RESOLVE_BENEATH)`, which is
   Linux-only and `unsafe`, in a crate that forbids `unsafe`. If she wants it
   closed on Linux anyway, that is a v1.1 item with a real cost.
