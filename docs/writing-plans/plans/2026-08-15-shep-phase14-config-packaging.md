# Phase 14 — config and packaging

The four config-and-packaging items spec §5 and §11 name as v1.0 and that
`docs/specs/deferred.md` still lists as unbuilt: `.js` Flockfiles, the
schemars JSON-schema export, the daemon-config **flags** layer, and openrc +
BSD `rc.d` startup units.

Written against `5c61105` (`docs(web): write the deploy guide and pin the Node
version`). Phase 13 (whistle) was executing while this was written, so the tip
has moved. See "Baseline" below for how to establish yours.

## Why these four, and why together

They are the last of the *configuration surface*. Everything else in the v1.0
build queue is either a UI over the daemon (lookout 12b), a transport
(whistle), or a platform tier (Windows). These four are all "how does shep get
told what to do, and how does the machine get told to run shep" — one subject,
one review, one set of docs to reconcile at the end.

Two of them are also each other's prerequisite in a way that is easy to miss:
the flags layer is what finally forces the `DaemonConfig` proof-token question
Phase 10 wrote down and deferred, and runtime init detection is what makes an
openrc renderer *reachable at all*. Both are decided below rather than left
open.

---

## Baseline

**Do not pin a SHA and do not trust a test count from this document.** Phase
13 was in flight when this was written and the Phase 12b plan had to be
corrected for exactly this. The tip moves under you.

At the time of writing: **1219 passed / 0 failed / 4 ignored, across 17 result
lines**, plus whatever whistle added. Treat that as a shape.

Establish your own baseline before Task 1, two commands:

```bash
git merge-base --is-ancestor 5c61105 HEAD; echo "ancestor=$?"   # expect 0
```

`ancestor=0` means this plan's tree is in your history. `ancestor=1` means
someone rebased and you should re-derive every baseline grep below before
trusting one of them.

```bash
cargo test --workspace --all-features
```

Write down what it prints. That is your baseline; every task states a delta
against it, and `failed` must stay `0` on every result line the whole way.

---

## Global constraints

- MSRV 1.88, edition 2024, `MIT OR Apache-2.0`
- `#![forbid(unsafe_code)]` in shep-core, shep-client and shep-cli; unsafe only
  in `shep-daemon/src/sys.rs` with per-block `// SAFETY:`
- `PROTOCOL_VERSION` stays **1**. Nothing in this phase touches the wire — if
  a task makes you reach for a new `Request`/`Response`/`BusEvent` variant,
  stop, you have taken a wrong turn. (`AppConfig` *is* on the wire: adding a
  field to it is a wire change and needs a pinned fixture. Task 3 adds a field
  to `RawFlockfile`, which is **not** `AppConfig` and is not on the wire. Read
  that task's own note before you touch it.)
- **IR-20**: a `pub` error enum in a library crate (shep-core, shep-daemon,
  shep-client) carries `#[non_exhaustive]` with a rationale in its own terms,
  or documents why not; a `pub` error enum in shep-cli does not, because the
  crate is `[[bin]]`-only. Either way the comment is mandatory. Task 4 applies
  the same attribute to a `pub` **struct**, and the reasoning it has to write
  down is not the same reasoning — read that task.
- **IR-46**: every `await` in a test needs a forcing mechanism the test itself
  sets. This phase adds very little async, but Task 1 adds a *blocking
  subprocess wait* with no bound at all, which is IR-46's failure shape in
  synchronous clothing. That is a deliberate, documented decision — see
  decision 3 — not an oversight to replicate elsewhere.
- The fast loop is `cargo test -p shep-daemon --lib --all-features -- --skip
  ::slow::`. **shep-cli is `[[bin]]`-only, so it needs `--bins`, never
  `--lib`** — `--lib` silently runs nothing and reports success. Most of this
  phase is in shep-cli, so this is the trap that will actually bite.
- The task gate is fmt, clippy `-D warnings`, `cargo test --workspace
  --all-features`, `RUSTDOCFLAGS="-D warnings" cargo doc`; **one cargo command
  at a time**, `$?` captured directly, never through a pipe (in zsh a
  pipeline's `$?` is the last command's).
- Terminology: the daemon is **the shepherd** and only that; one managed
  process is **a sheep**; the plural is always **the flock**. Destructive
  operations and error text stay plain — no theme in a refusal.

### The exact commands

One cargo command per invocation, `$?` read directly:

```bash
cargo test -p shep-core   --lib  --all-features
cargo test -p shep-daemon --lib  --all-features -- --skip ::slow::
cargo test -p shep-client --lib  --all-features
cargo test -p shep-cli    --bins --all-features            # NOT --lib
cargo test -p shep-cli    --test cli_e2e --all-features
cargo test -p shep-daemon --test daemon_e2e --all-features
cargo test -p shep-daemon --test real_runner --all-features
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

The Windows one needs `brew install mingw-w64`. It matters more than usual
this phase: Task 6 rewrites `current_init`'s `cfg` arms, and a `cfg` mistake
there is invisible on macOS.

### Every check in this plan states its baseline

Every plan on this project has shipped verification steps that could not fail.
The catalogue so far: a grep whose pattern missed because the real text had
backticks; a glob whose no-match case errors under zsh instead of printing
nothing; an expectation that was already true at HEAD; a "terminal too small"
message longer than the terminal it complained about; a `tokio::time::timeout`
wrapped around a synchronous call.

So: **every non-cargo check below prints its baseline at HEAD first**, and
uses `grep -c` / `find … | wc -l` rather than a bare glob. Run the baseline
command before you make the change. If it does not print what this plan says
it prints, **stop and say so** — the check is broken, not the tree.

Baselines, all re-run at `5c61105` on this machine, all exactly as printed:

```bash
grep -c "schemars" crates/shep-core/Cargo.toml                    # 0 (grep exits 1)
grep -rn --include="*.rs" "JsonSchema" crates | wc -l             # 0
find assets -maxdepth 1 -name '*.json' | wc -l                    # 0
grep -c "the directory does not exist" docs/specs/deferred.md     # 1
grep -c "^\[features\]" crates/shep-core/Cargo.toml               # 0 (grep exits 1)
grep -rn "JSON.stringify(require" crates | wc -l                  # 0
grep -c "Js," crates/shep-core/src/config/flockfile.rs            # 0 (grep exits 1)
grep -rn "rc.subr" crates | wc -l                                 # 0
grep -rni "openrc" crates | wc -l                                 # 4
grep -c "fn validate" crates/shep-core/src/config/daemon.rs       # 0 (grep exits 1)
grep -c "^#\[non_exhaustive\]" crates/shep-core/src/config/daemon.rs      # 1
grep -c "allow(dead_code)" crates/shep-cli/src/commands/startup/unit.rs   # 2
grep -c "a fifth backend" crates/shep-core/src/config/flockfile.rs        # 1
grep -rn "openrc and the BSD rc.d scripts" crates/shep-cli/src | wc -l    # 1
git ls-files assets | wc -l                                       # 2
grep -c "schemars" Cargo.lock                                     # 5
```

**The `^#\[non_exhaustive\]` anchor is load-bearing — do not drop it.** The
unanchored pattern prints `2` today, because `DaemonConfigError`'s doc comment
quotes the attribute in its own rationale, and every rationale this phase
requires would inflate it further. Anchoring to column 0 counts attributes and
only attributes: `1` now, `3` after Task 4.

**`--include="*.rs"` on the `JsonSchema` grep is load-bearing.** Unscoped it
prints `1` at HEAD, not `0`: `crates/shep-cli/Cargo.toml:151` mentions
`#[derive(JsonSchema)]` in the comment explaining why `schemars` is named
directly rather than reached through `rmcp::schemars`. The check wants to know
whether any *Rust source* derives it, which is `0` now and non-zero after Task
2. This plan's own first draft stated `0` for the unscoped form and was wrong.

**The `crates/shep-cli/src` scope on the openrc grep is load-bearing too.**
Unscoped it prints `2`; the second hit is `crates/shep-cli/CHANGELOG.md:535`,
a historical record of a past release that must **not** be deleted. Scoping to
`src` is what makes the check mean "the code no longer claims openrc is
missing" rather than "the project has never mentioned openrc".

Note the four that exit `1`. `grep -c` printing `0` and exiting non-zero is
fine at a prompt and **fatal in a `set -e` script**, which is how one of the
dead checks in an earlier phase came to be dead. If you script these, append
`|| true`.

The `grep -rni "openrc" crates` baseline of **4** is the one to watch: two of
those four are CHANGELOG lines and two are `// openrc is deferred` comments
that Tasks 6–7 must delete. A post-task count that is still 4 means you added
the renderer and left the comment claiming it does not exist.

#### Three shapes a dead check takes, all found in earlier plans

1. **The pattern that cannot match the real text.** Source text with
   backticks, or wrapped across a line break, defeats a naive grep. Before
   writing a grep, `grep -n` the surrounding words and read what is actually
   there.
2. **The expectation already true at HEAD.** If the baseline command prints
   the post-change value, the check verifies nothing. Every baseline above is
   printed for this reason.
3. **The bound that is not a bound.** `tokio::time::timeout` around a
   synchronous call bounds nothing; nor does a harness process timeout, which
   fails the whole binary and names no test.

---

## Design decisions made here, not deferred

Eleven of them. Six are rulings the tasks below depend on; five are the small
calls that would otherwise get made badly at 2am.

### 1. `.js` is opt-in by FLAG, not by extension. Rin's ruling, plus the reason her ruling alone does not settle it

**Rin's ruling, binding:** build `.js` support, but never implicitly. A `.js`
Flockfile is used only when named explicitly on the command line, and
directory discovery never picks one up. Her reason: shelling out to node to
evaluate a config file is arbitrary code execution, and `cd` into a cloned
repo followed by `shep start` must not run someone else's JavaScript.

The ten-name `DISCOVERY_ORDER` in
`crates/shep-core/src/config/flockfile.rs:167-178` therefore **stays exactly as
it is**. Task 1 adds a test that pins it at ten names and pins that none of
them ends in `.js`.

That much is Rin's. What her ruling does not settle, and what this plan must:
"named explicitly on the command line" is ambiguous, and the obvious reading
of it is **wrong**.

The obvious reading is "make `FlockFormat::from_path` recognise `.js`", so
that `shep start ecosystem.config.js` routes to the node bridge. That breaks
the single most common thing anyone types at this program:

```rust
// crates/shep-cli/src/commands/lifecycle.rs:436-448, today, passing
#[test]
fn any_other_existing_path_becomes_one_minimal_app_named_for_its_stem() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.js");
    std::fs::write(&path, "").unwrap();
    let apps = resolve_target(path.to_str().unwrap(), None, b"").unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "server");
    assert_eq!(apps[0].script, path.to_str().unwrap());
}
```

`shep start server.js` means *run this script*. It has meant that since Phase
3, it is what a pm2 user's fingers already type, and the fixture in that test
is literally named `server.js`. Routing `.js` to the config parser by
extension would turn "start my server" into "evaluate my server as a config,
find no `app` key, and refuse" — and would do it to the one file extension
where the collision is guaranteed rather than hypothetical.

**Ruling: a new boolean flag on `shep start`, `--flockfile`.** It means "read
TARGET as a Flockfile rather than as a script". Explicit in the strongest
sense Rin asked for: the operator has typed a word that means *evaluate this*.

```
shep start ./ecosystem.config.js --flockfile
```

Resolution order in `resolve_target` becomes, and do not widen it:

1. `target == "-"` — stdin is Flockfile JSON. Unchanged. `--flockfile` is
   ignored here; stdin is already a Flockfile by construction.
2. **`--flockfile` set** — resolve by extension: a format
   `FlockFormat::from_path` recognises parses as it does today; `.js` goes
   through the node bridge; **anything else is a refusal naming the
   extensions**.
3. Extension recognised, no flag — parse. Unchanged.
4. Any other existing path — one `AppConfig::minimal`. Unchanged; `.js`
   without the flag still lands here, which is the whole point.
5. Nothing matched — `Unresolvable`, naming the target. Unchanged.

With the flag absent, every branch behaves exactly as it does at HEAD. That
is the property Task 1's mutation checks.

**Where the flag does NOT go:** not on `restart`, not on `reload`, not on
`import`. `start` is the only verb that takes a target.

### 2. What `.js` actually reads — and it is not a pm2 ecosystem file

This is the part that will be got wrong if it is not written down.

The framing "so a pm2 user's `ecosystem.config.js` can be read" is the
aspiration. It is not what this builds, and it cannot be, for a reason you can
check in thirty seconds:

- The Flockfile document key is `app`. pm2's ecosystem key is `apps`.
- `RawFlockfile` is `#[serde(deny_unknown_fields)]`
  (`flockfile.rs:27-32`), deliberately, so a typo'd key fails loudly.
- `AppConfig` is `#[serde(deny_unknown_fields, default)]` (`app.rs:67-69`)
  and its field names are sheep-native. Spec §5: "sheep-native names, no pm2
  aliases". `exec_mode`, `max_memory_restart`, `error_file`, `env_production`
  are all unknown fields.

So a real `ecosystem.config.js`, handed to the bridge, gets rejected at the
first key. **This is correct and should stay that way.** What the bridge
builds is a *JavaScript-authored Flockfile*: a `.js` module whose export has
the Flockfile shape.

```js
// flockfile.js — what this feature actually reads
module.exports = {
  app: [
    { name: "web", script: "./srv", instances: Number(process.env.WEB_N ?? 2) },
  ],
};
```

The value is real — computed config, a shared base spread across
environments, `process.env` at read time — it is just not "point shep at pm2's
file and go".

**Do not add a pm2-shape special case.** Serde's own message is already the
right answer:

```
invalid JSON Flockfile: unknown field `apps`, expected `app` at line 1 column 8
```

It names the wrong key and the right one. A hand-rolled "this looks like a pm2
ecosystem file" branch would be a second grammar to maintain for a sentence
serde already writes. `shep import` remains the pm2 path, and it reads
`~/.pm2/dump.pm2` only — Task 9 makes `docs/migration.md` say both things in
one place.

### 3. The node failure taxonomy — including the one Rin asked about by name

Rin asked specifically: what is the error when someone points `shep start` at
a `.js` file and node is not installed, because that is the common case for a
Rust user. Four failure modes, four different sentences, three different exit
codes:

| what happened | detected as | exit | the sentence |
|---|---|---|---|
| node not on `PATH` | `Command::output()` → `ErrorKind::NotFound` | **1** `Failure` | `reading a .js Flockfile runs it through node, and node was not found on PATH; install node, or convert <path> to a .toml Flockfile` |
| node ran, exited non-zero | `!status.success()` | **4** `InvalidConfig` | `node could not evaluate <path>: <last line of node's stderr>` |
| node exited 0, stdout is not JSON | `Flockfile::parse` → `Json` | **4** `InvalidConfig` | the existing `invalid JSON Flockfile: …` |
| node spawn failed some other way | any other `io::Error` | **1** `Failure` | `could not run node for <path>: <io error>` |

`Failure` (1) for the missing interpreter is the deliberate call. It is not
`Usage` (2) — the operator's command line was well-formed. It is not
`InvalidConfig` (4) — the config was never read, so nothing about it is known
to be invalid. `ExitCode::Failure`'s own doc says "an error with no more
specific code", and this is one. **No new `ExitCode` variant.** The taxonomy
is a CLI contract; growing it for one case that fits an existing variant is
how a taxonomy stops meaning anything.

The missing-node sentence names the escape hatch (`convert to .toml`) because
for a Rust user that is the actual fix, and a message that only says "install
node" reads as shep demanding a Node toolchain it does not otherwise need.

**Two mechanics that are part of the decision, not implementation detail:**

*The path never enters the JavaScript source.* Spec §5 writes the invocation
as `node -p 'JSON.stringify(require(p))'`. Interpolating a path into that
string is an injection surface: a path containing `'`, `\`, or a newline
escapes the literal, and the whole premise here is that we are already running
the file's own code, so adding a *second* way to get code in is gratuitous.
Pass the path as an argument instead:

```
node -p "JSON.stringify(require(process.argv[1]))" <absolute path>
```

Under `-p` / `-e`, node puts the first user argument at `process.argv[1]`
(there is no script path to occupy that slot). The path must be **absolute**:
`require("ecosystem.config.js")` with no `./` is a *package* specifier and
resolves against `node_modules`, not the cwd. Canonicalize before spawning,
and report a canonicalize failure as `TargetError::Read`.

*stdin is `/dev/null`, stdout and stderr are captured.* A config module that
reads stdin must not eat the operator's terminal; node's stderr is captured so
the second row of the table above can quote it.

**One bound this deliberately does NOT have.** A `.js` module that never
returns — one that starts a server at require time — hangs `shep start`
forever. There is no timeout. Adding one means a reaper thread in a
`#![forbid(unsafe_code)]` crate for a case where the process is in the
foreground, attached to the operator's terminal, and interruptible with
Ctrl-C. Record it in `deferred.md` as known debt (Task 9) rather than
pretending it is handled. This is IR-46's shape and it is being accepted
knowingly, once, in the one place where a human is watching.

### 4. `.js` lives entirely in shep-cli. shep-core never spawns a process

No `FlockFormat::Js`. No fifth `FlockfileError` variant. The bridge is a
function in `crates/shep-cli/src/commands/lifecycle.rs` that produces a
`String` and hands it to the existing `Flockfile::parse(&s, FlockFormat::Json)`.

This is not a new decision — `flockfile.rs`'s own module doc already says it:

> Parsing is strict serde — no code execution; `.js` configs are the CLI's job
> (it shells out to node and feeds the resulting JSON through
> [`FlockFormat::Json`]).

Keeping it that way means **shep-core, the library every other crate and every
out-of-tree consumer depends on, can never execute a config file.** That is a
property worth having in one sentence in `SECURITY.md`, and Task 1 adds it.

One consequence: the `#[non_exhaustive]` rationale on `FlockfileError`
currently predicts the opposite —

```rust
/// `#[non_exhaustive]`: a fifth backend is the named next step for this type
/// — `deferred.md` lists `.js` Flockfiles — and it brings its own rejection
/// reason with it, which must not be a breaking change for a consumer
/// matching on this enum (IR-20).
```

— and after Task 1 that prediction is false. The attribute stays (IR-20's
library-crate default), but the comment must be rewritten to a rationale that
is still true. `grep -c "a fifth backend"` going `1 → 0` is Task 1's check.

### 5. schemars: derive in shep-core behind a feature, generate through a hidden verb, commit the artefact, catch drift with `include_str!`

The situation changed since `deferred.md` was written. Verified against the
tree at `5c61105`:

- `Cargo.lock` carries exactly one `schemars`, **1.2.2**, pulled in by
  `rmcp`'s server feature for whistle.
- The root `Cargo.toml` declares it as a workspace dependency
  (`schemars = { version = "1.2.2", default-features = false, features =
  ["derive", "std"] }`), pinned exactly, with a comment explaining that the
  derive expands to absolute `schemars::` paths so the crate has to be
  nameable rather than reached through `rmcp::schemars`.
- `crates/shep-cli/Cargo.toml` has `schemars.workspace = true`.
- `crates/shep-core/Cargo.toml` has **no** `schemars` and no `[features]`
  table at all.

So this is now a derive decision, not a dependency decision — with one
exception: `AppConfig` lives in shep-core, and shep-core does not have the
crate.

**Ruling: optional dependency on shep-core behind a non-default `schema`
feature; shep-cli turns it on.**

Not unconditional, because shep-core is a published library and every
out-of-tree consumer would then compile `schemars` + `schemars_derive` (a proc
macro) for a JSON Schema they may not want; and shep-daemon, which is the
crate with the single-digit-MB idle-RSS goal, has no use for it. One line in
shep-cli's manifest is the whole cost of the gate. `--all-features`, which
every gate command in this plan already uses, turns it on everywhere it
matters.

**Where the schema lives: `assets/flockfile.schema.json`, committed.**

`deferred.md:113-114` says "no schema ships in `assets/` (the directory does
not exist)". **That parenthetical is now stale** — `assets/grafana/` was added
for the metrics dog and `git ls-files assets | wc -l` prints `2`. The
directory exists, it is tracked, and it already holds exactly this kind of
artefact: a generated JSON file whose consumer is a tool outside the Rust
build (Grafana there, an editor here). Precedent settles the location; Task 9
fixes the ledger sentence.

Root-level `assets/`, not `crates/shep-core/assets/`, for the same reason: it
does not ship in the crates.io tarball and does not need to. Its consumer
points an editor at a repo path or a release URL.

**How it is generated: a hidden `shep schema` verb.** Prints the schema to
stdout, hidden the way `daemon` and `dog` already are. Regeneration is
`cargo run -p shep-cli -- schema > assets/flockfile.schema.json`. It also
gives a user who has the binary but not the repo a way to get the schema
without cloning, which is a real benefit and the reason it is a verb rather
than test-only code.

**How drift is caught: `include_str!` plus a co-located test.**

```rust
// crates/shep-cli/src/commands/schema.rs
const COMMITTED: &str = include_str!("../../../../assets/flockfile.schema.json");
```

`include_str!` makes the committed file a **compile-time input**. Delete it
and the crate does not build; edit `AppConfig` and the co-located test fails
with a diff and the exact regeneration command. A committed schema nobody
regenerates is a lie with a filename — this is the mechanism that makes it
impossible to keep the lie, because the check is not a CI job someone can
forget to add, it is `cargo test` on the crate every task in every phase
already runs.

Path arithmetic, since it is easy to get wrong:
`crates/shep-cli/src/commands/` → `../` src → `../../` shep-cli →
`../../../` crates → `../../../../` repo root. Four.

**The rule the schema follows: it describes the DESERIALIZER, not the
normalizer.** `AppConfig::kill_signal` is `Option<String>`; `normalize` is
what refuses a name outside `SIGTERM|SIGINT|SIGQUIT|SIGUSR2`. The schema says
`"type": "string"` and stops there. Emitting an `enum` would make the schema
describe a validation step that happens in a different crate at a different
time, and the moment those two diverge the schema is wrong in a way no test
can catch. One sentence in `schema.rs`'s module doc, and a test that pins
`kill_signal` as an unconstrained string.

**`MemSize` and `UpDuration` need hand-written `JsonSchema` impls.** Both are
newtypes with manual `Serialize`/`Deserialize` that go to and from *strings*
(`values.rs:101-113`, `values.rs:248-260`). `#[derive(JsonSchema)]` would
describe the inner `u64` / `Duration` and be flatly wrong. The impls emit
`{"type": "string", "pattern": …}` with the pattern lifted from each type's
own `FromStr` doc — `^\d+(G|M|K)?$` for `MemSize` (`values.rs:53`),
`^\d+(h|m|s)?$` for `UpDuration` (`values.rs:200`). **Lift them from the doc
comment, do not retype them from this plan**, and pin each with a pair of
tests: a string the pattern accepts parses, a string it rejects fails to
parse. A pattern that agrees with itself and disagrees with `FromStr` is the
failure mode.

One side effect to know about before it surprises you: schemars' derive reads
`///` doc comments into `description`. That is a feature — `AppConfig`'s field
docs become hover text in the operator's editor, which is the best return this
whole task has. It also means **editing a doc comment on `AppConfig` fails the
drift test** until the schema is regenerated. That is correct behaviour, and
Task 2's test failure message says so explicitly so the next person does not
think they broke something.

### 6. `$schema` becomes a recognised, ignored top-level key

A JSON or JSON5 Flockfile cannot carry `"$schema": "…"` today: `RawFlockfile`
denies unknown fields, so the one line every JSON-schema-aware editor looks
for is a hard parse error. Without it, the artefact Task 2 commits is usable
only by TOML users through taplo's `#:schema` comment directive — and the
comment is invisible to serde, so TOML already works.

`flockfile.rs:22-26` anticipates exactly this:

> the top level is locked to the `app` key on purpose — a typo'd key must fail
> loudly. A future schema key (e.g. `version`) gets added HERE explicitly

So: add `$schema: Option<String>` to `RawFlockfile`, read and discarded.

**This is the one task in the phase that is cleanly severable.** Cutting it
costs JSON/JSON5-family editor completion and nothing else; TOML is the
preferred format and is unaffected. It is Task 3, standing alone, so it can be
dropped without touching a line of any other task.

Note the boundary carefully: the field goes on **`RawFlockfile`**, the
private document wrapper. It does **not** go on `AppConfig`, which is on the
wire. Nothing about `PROTOCOL_VERSION` or the pinned fixtures changes.

### 7. `DaemonConfig` and the proof-token question — the ruling

`deferred.md:181-196` records the question and names this phase's flags layer
as the thing that forces it:

> `ResolvedApp` keeps its `config` private so that holding one proves it went
> through `normalize`. `DaemonConfig` does not: its `daemon` and `dog` fields
> are `pub`, and the one validation it performs — the `max_cron_sleep` floor —
> happens inline inside `DaemonConfig::load` rather than in a `validate` step a
> hand-built value would also have to pass. […] What would force it: any
> production path that assembles a `DaemonConfig` from something other than a
> file — the daemon-config flags layer, for instance.

**Ruling: `DaemonConfig` does NOT become a proof token in the `ResolvedApp`
sense. It gets `#[non_exhaustive]` and an extracted `validate`, which buys the
same guarantee at a fraction of the cost.**

The reasoning, four parts:

**(a) The property `ResolvedApp` protects is a property of *travel*, and
`DaemonConfig` does not travel.** A `ResolvedApp` is handed to the supervisor,
which must be able to trust that normalization happened somewhere it cannot
see. A `DaemonConfig` is built and consumed within a few lines: `run_daemon`
(`commands/daemon.rs:225`) loads one and immediately renders it into
`BootOptions`; `dogs.rs:229` loads one to read a single `[dog.<name>]` table;
`whistle/gate.rs:94` loads one to read a single boolean. The daemon never
holds a `DaemonConfig` — it holds a `BootOptions`. There is no consumer that
*receives* one from elsewhere and must trust it.

**(b) `#[non_exhaustive]` gets the actual guarantee for free.** Outside
shep-core, `#[non_exhaustive]` makes `DaemonConfig { … }` and
`DaemonConfig { …, ..Default::default() }` both fail to compile. Since
`load`/`load_layered` and `Default` become the only ways to obtain one from
another crate, and both validate, *holding a `DaemonConfig` outside shep-core
already proves it was validated* — which is the proof-token property, with the
fields still readable. Verified constructible-today count: `grep -rn
"DaemonConfig" crates | grep -v shep-core/src/config/daemon.rs` finds no
struct literal anywhere. **The change is zero-diff at every call site.**

**(c) Privatising the fields costs a great deal and protects one `u64`.** The
invariant is a single floor on a single `Option<UpDuration>`. Making the
fields private means accessors for `log_json`, `log_level`, `socket`,
`enabled_dogs`, `adopted_dogs`, `max_cron_sleep`, `whistle.allow_control`, and
`dog` — the last of which is a `BTreeMap<String, toml::Table>` read by
`shep_toml.rs` and `dogs.rs` across two crates. Thirty-odd getters to guard
one floor is the wrong trade, and Readability > KISS > DRY says so plainly.

**(d) The `validate` extraction is the half that is actually load-bearing,
and it is required regardless.** See decision 8.

Write the reasoning into `DaemonConfig`'s own doc comment as the
`#[non_exhaustive]` rationale, in the type's own terms, and note there that
this is a **struct** rationale and not IR-20's error-enum default — IR-20 names
`ProcessInfo` as the one wire struct carrying the attribute, and this is a
second, different case (a validated-construction gate, not wire growth). Then
strike the `deferred.md` entry and replace it with the resolution (Task 9).

Add `crates/shep-core/tests/` — **no.** `process_info_builder_from_outside_the_crate.rs`
says in its own header: "It is shep-core's one `tests/` file and must stay the
only one." Honour it. The `#[non_exhaustive]` attribute is unobservable from
inside the defining crate and nothing in the repository guards it — exactly as
that file already admits for `ProcessInfo`. Task 4 states that gap rather than
inventing a `trybuild` tier for it, and does not pretend to a test it does not
have.

### 8. Validation happens ONCE, after all three layers. The flags layer must be able to rescue a broken file

`daemon.rs:232-239` already carries the reasoning for the layer below:

> Validating each layer as it is read instead would make a good
> `SHEP_MAX_CRON_SLEEP` unable to rescue a broken `shep.toml`, which is not
> what "file < env" means.

The same argument applies with equal force to flags: `--max-cron-sleep 5m`
must be able to boot a shepherd whose `shep.toml` says `max_cron_sleep = "0"`.
That is what `file < env < flags` means.

Which forces the API shape:

```rust
impl DaemonConfig {
    /// file < env, validated. Unchanged for every existing caller.
    pub fn load(file_source: Option<&str>, env: &dyn Fn(&str) -> Option<String>)
        -> Result<Self, DaemonConfigError>
    { Self::load_layered(file_source, env, &DaemonOverrides::new()) }

    /// file < env < flags, validated exactly once, at the end.
    pub fn load_layered(
        file_source: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
        overrides: &DaemonOverrides,
    ) -> Result<Self, DaemonConfigError> { … }
}
```

`load` keeps its exact signature and semantics, so `dogs.rs`, `gate.rs` and
some twenty test call sites are untouched. The floor check moves out of `load`
into a private `fn validate(&self, key: &'static str) -> Result<(),
DaemonConfigError>`, called from exactly one place at the bottom of
`load_layered`. `grep -c "fn validate" crates/shep-core/src/config/daemon.rs`
goes `0 → 1`.

The provenance key gains a third value. `DaemonConfigError::BelowMinimum`'s
`key: &'static str` is already documented as "the key the user actually set";
it becomes one of `"max_cron_sleep"`, `"SHEP_MAX_CRON_SLEEP"`,
`"--max-cron-sleep"`. The `Display` is unchanged and reads correctly with all
three: `invalid value \`500\` for --max-cron-sleep: must be at least 1s`.

`DaemonOverrides` is `#[non_exhaustive]` with a consuming-self builder — the
`ProcessInfo::builder` shape this codebase already uses, chosen because
`#[non_exhaustive]` rules out struct literals and functional update from
outside, so it needs *some* constructor and the precedent already exists:

```rust
DaemonOverrides::new()
    .log_json(Some(true))
    .log_level(None)
    .socket(None)
    .max_cron_sleep(Some(UpDuration::from_millis(300_000)))
```

Its `#[non_exhaustive]` rationale is honest and specific: this type grows a
field every time the `daemon` subcommand grows a flag, and that is anticipated
by construction.

### 9. The flags go on `DaemonArgs`, not on `GlobalArgs`

`shep.toml` configures **the shepherd**. The only invocation that runs a
shepherd is the hidden `daemon` subcommand. Putting `--log-level` on
`GlobalArgs` would offer it on `shep flock`, where it configures nothing.

The four flags mirror the four `SHEP_*` variables `load` already reads,
one for one:

| flag | env | `[daemon]` key |
|---|---|---|
| `--log-json[=BOOL]` | `SHEP_LOG_JSON` | `log_json` |
| `--log-level <LEVEL>` | `SHEP_LOG_LEVEL` | `log_level` |
| `--socket <PATH>` | `SHEP_SOCKET` | `socket` |
| `--max-cron-sleep <DUR>` | `SHEP_MAX_CRON_SLEEP` | `max_cron_sleep` |

Nothing else. `enabled_dogs` and `adopted_dogs` are written by `shep enable` /
`shep adopt` and have no env layer either; a flag for them would be a fourth
way to set the same thing.

`--log-json` needs three states — set true, set false, not mentioned — so it
is `Option<bool>` with `num_args = 0..=1, default_missing_value = "true"`,
giving `--log-json`, `--log-json=false`, and absence.

**Its value parser is the existing env grammar, not clap's.** `SHEP_LOG_JSON`
accepts exactly `1|0|true|false` (`daemon.rs:212-217`). clap's
`BoolishValueParser` also takes `yes|no|y|n|on|off` — wider. Widening an input
grammar is a named top drift risk on this project. So shep-core exports one
`pub fn parse_bool_value(v: &str) -> Option<bool>` over those four spellings,
the env arm calls it, and the CLI's `value_parser` calls it. One grammar, two
callers, DRY where DRY is about meaning.

This is also the flag layer's most useful real form. Spec §13's flagship
scenario runs the shepherd from an init unit, and that unit's `ExecStart` can
now say what it wants without a config file:

```
ExecStart=/usr/local/bin/shep daemon --foreground --log-level info --log-json
```

### 10. Init selection becomes a runtime probe on Linux, a compile-time fact everywhere else, and an operator override always

`unit.rs:39-43` states the current design and its own expiry date:

> Linux is systemd, macOS is launchd; there is no runtime detection because
> there is nothing else either target could be, and openrc/rc.d are named as
> deferred

The first clause stops being true the moment openrc exists. **Runtime
detection is a prerequisite for openrc and separable from BSD**, and the
asymmetry is the whole answer to Rin's question:

- On **Linux**, systemd and openrc share one target triple. `target_os` cannot
  distinguish them, so without a runtime probe an openrc renderer would have
  no way to ever be selected. Detection *is* the openrc feature.
- On **FreeBSD** and **OpenBSD**, `target_os = "freebsd"` / `"openbsd"`
  already determines rc.d uniquely. There is no second init in play. The BSD
  renderers need new `cfg` arms and no detection at all.

So Task 6 (detection) gates Task 7 (openrc) and does not gate Task 8 (BSD).

The Linux probe, in order:

1. `/run/systemd/system` is a directory → **systemd**. This is exactly what
   `sd_booted(3)` checks, and it is the one probe with an upstream contract
   behind it.
2. `/run/openrc/softlevel` exists, or `/run/openrc` is a directory →
   **openrc**.
3. Neither → refuse, naming both paths that were probed.

**This is a behaviour change on Linux and it can bite.** Today every Linux
build gets `Init::Systemd` unconditionally; a container with no
`/run/systemd/system` currently gets a systemd unit written into it happily.
After Task 6 it gets a refusal. That refusal is the *correct* answer — a unit
in a container with no init to read it does nothing — but it is still a case
that worked before and does not after, so it needs an escape hatch and a
changelog line.

**The escape hatch: `--init <systemd|openrc|launchd|freebsd-rc|openbsd-rc>` on
`StartupArgs`**, honoured by both `startup` and `unstartup`. `unstartup`
matters as much as `startup`: without the same flag, an operator who installed
a unit under one init and then changed init systems could not remove it.

The override is accepted verbatim on any target. Rendering is pure `format!`
and cannot fail; a wrong choice surfaces as a named failed row when the enable
step cannot find `systemctl`/`rc-update`/`rcctl`/`service`, which is a better
diagnosis than a compile-time refusal could give. It also makes every renderer
reachable on the machine you are actually sitting at, which is the only reason
the systemd unit has ever been tested at all.

Consequence to not forget: `Init`'s variants are currently annotated
`#[cfg_attr(not(target_os = "linux"), allow(dead_code))]` and the macOS
equivalent, because only one is constructed per target. With `--init`, all of
them are constructible everywhere. **Both `allow(dead_code)` attributes must
be deleted** — `grep -c "allow(dead_code)" crates/shep-cli/src/commands/startup/unit.rs`
goes `2 → 0`, and if it does not, clippy `-D warnings` is being lied to.

### 11. Two BSD renderers, not one. And what openrc does about readiness instead of pretending

**Two, and here is the shape of the difference.** FreeBSD and OpenBSD rc
scripts share a silhouette and agree on almost nothing concrete:

| | FreeBSD | OpenBSD |
|---|---|---|
| script path | `/usr/local/etc/rc.d/shep_<user>` | `/etc/rc.d/shep_<user>` |
| sourced framework | `/etc/rc.subr` | `/etc/rc.d/rc.subr` |
| the vocabulary | `name`, `rcvar`, `command`, `command_args`, `${name}_user`, `${name}_chdir`, `${name}_env`, `load_rc_config`, `run_rc_command` | `daemon`, `daemon_flags`, `daemon_user`, `daemon_execdir`, `rc_bg`, `rc_reload`, `rc_cmd` |
| enable | `sysrc shep_<user>_enable=YES` | `rcctl enable shep_<user>` |
| start | `service shep_<user> start` | `rcctl start shep_<user>` |
| env for the child | `${name}_env` | set inside `rc_start` |

A single renderer with five conditionals would be longer than two `format!`
blocks and would read worse. This is incidental similarity of *shape*, not
repetition of *meaning* — the case CLAUDE.md's DRY rule explicitly carves out.

**The username trap, which is real and which both BSD renderers must refuse.**
`rcvar` and `rcctl` service names become *shell variable names*
(`shep_<user>_enable`, `shep_<user>_flags`). A username containing `-` or `.`
— `web-app` and `deploy.svc` are both legal — produces `shep_web-app_enable`,
which is not a valid `sh` variable, and the resulting script fails at
`load_rc_config` with a syntax error naming a line number rather than a user.
So the BSD renderers refuse a user not matching `^[A-Za-z_][A-Za-z0-9_]*$`,
before writing anything, with a plain sentence naming the constraint and the
user. systemd and openrc name *files* and are unaffected; do not add the check
there. This refusal is Task 8's most valuable test, because it is the one
failure that would otherwise be discovered by a stranger on a machine none of
us has.

**Unit file mode is per-init.** `UNIT_MODE = 0o644` is right for a systemd
unit and a launchd plist, which are *read* by their init system. An openrc
init script and a BSD rc.d script are **executed**, so they need `0o755`.
`UNIT_MODE` becomes `fn unit_mode(init: Init) -> u32`, pinned for all five
variants. Shipping an openrc script at 0644 is a failure that surfaces at the
next reboot, which is the worst time.

**openrc and readiness — the honest answer.**

openrc has no `sd_notify` analogue. There is no protocol by which a supervised
process tells openrc it is ready; `supervise-daemon` considers the service
started the instant the process is spawned. Whatever `Type=notify` buys on
systemd is simply not available, and the openrc script must not pretend
otherwise.

What it does instead: **`start_post()` polls the shepherd's own control
socket**, and the script says in a comment that this is what it is doing and
why.

That poll is not a consolation prize — it happens to be *exactly as strong* as
`READY=1`, and the reason is in `boot.rs`'s own step order:

- step 2 binds the socket (`boot.rs:597`),
- step 4 restores the muster roll and spawns the dogs (`boot.rs:670-676`),
- step 5 reports readiness,
- and `RpcServer::new(listener, ctx)` — the thing that *accepts* on that
  listener — is constructed in `run`, after `boot` has returned.

So a connection lands in the backlog immediately but **no request is answered
until after the restore and the dogs are up**. The first answered `shep flock`
proves the same milestone `READY=1` proves, one step later. Write that
reasoning into the script as a comment; it is the kind of claim that gets
"simplified" away by someone who assumes the poll is a guess.

Bound the loop (60 × 1s) and return non-zero on timeout, so a shepherd that
fails to boot reports as a failed service rather than a started one.

**FreeBSD** gets the same treatment through `start_postcmd`, which is standard
`rc.subr`.

**OpenBSD does not poll.** OpenBSD's `rc.subr` gives `rc_pre` (before start)
and `rc_post` (after *stop*); there is no documented post-start hook, and this
plan is not going to invent one from memory for a platform nobody here can
run. The OpenBSD script's header comment says plainly that the service is
reported started as soon as the shepherd process is spawned, that the flock
may still be coming back, and that `shep flock` is the check. Task 8 pins that
comment with an exact-string test, because unpinned generated prose is how
Phase 12a shipped two false captions.

**Say this out loud in the docs: none of the three new scripts has been run on
its own operating system.** No FreeBSD, OpenBSD, or openrc host exists in this
project's CI or on this machine. They are pure `format!` with exact-string
tests — the same tier the systemd unit has always had, since it is likewise
only ever *rendered* on a Mac. That is a real and adequate tier for text, and
it is not a claim that the scripts work. `docs/releasing.md` gets one sentence
saying so, and no doc anywhere says "supported on FreeBSD" until somebody
reports back from one.

---

## Task order and dependencies

```
Task 1  .js Flockfile                       ── independent
Task 2  schemars derive + assets + verb     ── independent
Task 3  $schema key                          ── after 2 (it is only useful once a schema exists); SEVERABLE
Task 4  DaemonConfig: non_exhaustive, validate, DaemonOverrides   ── independent
Task 5  the daemon flags layer               ── after 4
Task 6  runtime init detection + --init      ── independent
Task 7  openrc renderer                      ── after 6
Task 8  FreeBSD + OpenBSD rc.d renderers     ── after 6
Task 9  docs, ledger, changelogs             ── last, after everything
```

Tasks 1, 2, 4 and 6 are genuinely independent and touch disjoint files; they
can go in parallel. 3, 5, 7 and 8 each follow one of them. 9 is last because
it reconciles claims the other eight make.

---

## Task 1 — `.js` Flockfiles behind `--flockfile`

**Files:** `crates/shep-cli/src/cli.rs`,
`crates/shep-cli/src/commands/lifecycle.rs`,
`crates/shep-core/src/config/flockfile.rs` (comments and one test only),
`SECURITY.md`.

### Step 1.1 — baseline

```bash
grep -rn "JSON.stringify(require" crates | wc -l              # 0
grep -c "a fifth backend" crates/shep-core/src/config/flockfile.rs   # 1
grep -c "flockfile" crates/shep-cli/src/cli.rs || true        # 0
cargo test -p shep-cli --bins --all-features
```

Record the shep-cli count.

`shep start ./x.js` **today**, with `x.js` present, starts `x.js` as a script
named `x`. That is the behaviour every step below must leave intact.

### Step 1.2 — RED: the parse surface

Add to `crates/shep-cli/src/cli.rs`, in `StartArgs`:

```rust
    /// Read TARGET as a Flockfile rather than as a script path.
    ///
    /// Required for a `.js` Flockfile and the only way to reach one: shep
    /// reads a `.js` config by running it through node, which is arbitrary
    /// code execution, so it never happens because a file merely has that
    /// extension. Without this flag `shep start server.js` starts
    /// `server.js` as a script, which is what it has always meant.
    #[arg(long)]
    pub flockfile: bool,
```

Tests in `cli.rs`'s existing `mod tests` (three, each of which fails now with
clap's `unexpected argument '--flockfile' found`):

```rust
    #[test]
    fn start_takes_a_flockfile_flag_and_defaults_it_off() {
        use clap::Parser;
        let plain = Cli::try_parse_from(["shep", "start", "srv.js"]).unwrap();
        let flagged = Cli::try_parse_from(["shep", "start", "srv.js", "--flockfile"]).unwrap();
        match (plain.command, flagged.command) {
            (Commands::Start(a), Commands::Start(b)) => {
                assert!(!a.flockfile, "absent means script form");
                assert!(b.flockfile);
            }
            other => panic!("expected two Start commands, got {other:?}"),
        }
    }
```

Every existing construction of `StartArgs` in `lifecycle.rs`'s tests (the
`start_args` helper, around line 405) needs `flockfile: false` — that is the
compile error that proves the field landed.

### Step 1.3 — GREEN: the node bridge

In `crates/shep-cli/src/commands/lifecycle.rs`. Two new `TargetError`
variants:

```rust
    /// `--flockfile` was given for a path whose extension names no format
    /// this can read.
    UnknownFlockfileFormat {
        /// The path as the operator wrote it.
        path: PathBuf,
    },
    /// A `.js` Flockfile could not be evaluated. `node_missing` separates
    /// "install node" from "your config threw", because they are different
    /// problems with different fixes and different exit codes.
    Js {
        /// The path that was being read.
        path: PathBuf,
        /// What went wrong, already phrased for the operator.
        detail: String,
        /// `true` when node itself was not found on `PATH`.
        node_missing: bool,
    },
```

`TargetError` is in shep-cli, a `[[bin]]`-only crate, so it carries no
`#[non_exhaustive]` — IR-20's own carve-out, and the existing enum already
follows it.

`Display`:

```rust
            Self::UnknownFlockfileFormat { path } => write!(
                f,
                "--flockfile needs a .toml, .yaml, .yml, .json, .json5 or .js file; {} is none of those",
                path.display()
            ),
            Self::Js { detail, .. } => f.write_str(detail),
```

`target_exit_code` — decision 3's table, encoded:

```rust
        TargetError::UnknownFlockfileFormat { .. } => ExitCode::Usage,
        TargetError::Js { node_missing: true, .. } => ExitCode::Failure,
        TargetError::Js { node_missing: false, .. } => ExitCode::InvalidConfig,
```

`source()` returns `None` for both new variants — neither wraps a live error
object; `Js` has already flattened node's stderr into a sentence.

The bridge itself:

```rust
/// Evaluates a `.js` Flockfile through node and returns its JSON.
///
/// The path is passed as an **argument**, never interpolated into the
/// JavaScript source: a path containing `'`, `\` or a newline would
/// otherwise escape the string literal, and adding a second way to inject
/// code into a file whose own code we are already about to run is
/// gratuitous. Under `-p`, node puts the first user argument at
/// `process.argv[1]`.
///
/// The path must be absolute — `require("x.js")` with no leading `./` is a
/// *package* specifier and resolves against `node_modules`, not the cwd.
///
/// stdin is `/dev/null` so a config module that reads stdin cannot eat the
/// operator's terminal; stdout and stderr are captured so node's own message
/// can be quoted back.
///
/// **There is no timeout.** A module that never returns — one that starts a
/// server at require time — hangs here. The process is in the foreground and
/// interruptible; adding a bound means a reaper thread in a crate that
/// forbids unsafe code. Recorded in `docs/specs/deferred.md`.
///
/// # Errors
///
/// - [`TargetError::Read`] — the path could not be canonicalized.
/// - [`TargetError::Js`] with `node_missing` — node is not on `PATH`.
/// - [`TargetError::Js`] — node ran and failed, or could not be spawned.
fn evaluate_js_flockfile(path: &Path) -> Result<String, TargetError> {
    let absolute = std::fs::canonicalize(path).map_err(|source| TargetError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let output = std::process::Command::new("node")
        .arg("-p")
        .arg("JSON.stringify(require(process.argv[1]))")
        .arg(&absolute)
        .stdin(std::process::Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(TargetError::Js {
                path: path.to_path_buf(),
                detail: format!(
                    "reading a .js Flockfile runs it through node, and node was not found on PATH; \
                     install node, or convert {} to a .toml Flockfile",
                    path.display()
                ),
                node_missing: true,
            });
        }
        Err(err) => {
            return Err(TargetError::Js {
                path: path.to_path_buf(),
                detail: format!("could not run node for {}: {err}", path.display()),
                node_missing: false,
            });
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("node exited non-zero and said nothing");
        return Err(TargetError::Js {
            path: path.to_path_buf(),
            detail: format!("node could not evaluate {}: {reason}", path.display()),
            node_missing: false,
        });
    }
    String::from_utf8(output.stdout).map_err(|_utf8_error| TargetError::Js {
        path: path.to_path_buf(),
        detail: format!("node printed non-UTF-8 output for {}", path.display()),
        node_missing: false,
    })
}
```

`resolve_target` gains one parameter and one arm. **The arm goes between the
`-` arm and the recognised-extension arm**, and nowhere else — that ordering
is the whole of decision 1:

```rust
pub fn resolve_target(
    target: &str,
    name: Option<&str>,
    stdin: &[u8],
    as_flockfile: bool,
) -> Result<Vec<AppConfig>, TargetError> {
    let path = Path::new(target);
    match (target, FlockFormat::from_path(path)) {
        ("-", _) => { /* unchanged */ }
        (_, format) if as_flockfile => match format {
            Some(format) => {
                let source = std::fs::read_to_string(path).map_err(|source| TargetError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                Flockfile::parse(&source, format)
                    .map(|flockfile| flockfile.apps)
                    .map_err(TargetError::Flockfile)
            }
            None if path.extension().and_then(|e| e.to_str()) == Some("js") => {
                let json = evaluate_js_flockfile(path)?;
                Flockfile::parse(&json, FlockFormat::Json)
                    .map(|flockfile| flockfile.apps)
                    .map_err(TargetError::Flockfile)
            }
            None => Err(TargetError::UnknownFlockfileFormat {
                path: path.to_path_buf(),
            }),
        },
        (_, Some(format)) => { /* unchanged */ }
        _ if path.exists() => { /* unchanged */ }
        _ => { /* unchanged */ }
    }
}
```

Update the function's doc comment to list five branches instead of four, and
keep the "do not widen it" sentence.

`start` passes `args.flockfile` at `lifecycle.rs:225`.

### Step 1.4 — tests

Co-located in `lifecycle.rs`'s `mod tests`. Six, and each one states what it
would catch:

```rust
    /// fails if `.js` is ever routed to the node bridge without the flag —
    /// the regression that would break `shep start server.js` for every
    /// user who has ever typed it. Deliberately a sibling of
    /// `any_other_existing_path_becomes_one_minimal_app_named_for_its_stem`.
    #[test]
    fn a_js_file_without_the_flag_is_still_a_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.js");
        std::fs::write(&path, "throw new Error('this must never be evaluated')").unwrap();
        let apps = resolve_target(path.to_str().unwrap(), None, b"", false).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "server");
        assert_eq!(apps[0].script, path.to_str().unwrap());
    }

    /// fails if `--flockfile` changes how a recognised extension is read.
    #[test]
    fn the_flag_does_not_change_a_toml_flockfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.toml");
        std::fs::write(&path, "[[app]]\nname = \"web\"\nscript = \"./srv\"\n").unwrap();
        let with = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap();
        let without = resolve_target(path.to_str().unwrap(), None, b"", false).unwrap();
        assert_eq!(with, without);
    }

    /// fails if an unreadable extension under the flag falls through to the
    /// script arm instead of refusing — which would silently start the
    /// operator's config file as a program.
    #[test]
    fn the_flag_refuses_an_extension_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.ini");
        std::fs::write(&path, "").unwrap();
        let err = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap_err();
        assert!(matches!(err, TargetError::UnknownFlockfileFormat { .. }));
        assert_eq!(target_exit_code(&err), ExitCode::Usage);
    }
```

Then the node cases. **These require node and must skip cleanly without it**
— but a test that silently passes when it skipped is dead, so the skip prints
and the harness still runs the other three:

```rust
    /// Returns `true` when node is on PATH. The `.js` cases below are the
    /// only tests in the workspace that need a second runtime, and a machine
    /// without node must not fail the suite — but it must SAY it skipped,
    /// because a silent skip is how a broken bridge ships green.
    fn node_available() -> bool {
        let ok = std::process::Command::new("node")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("SKIPPED: node is not on PATH; the .js Flockfile cases did not run");
        }
        ok
    }

    #[test]
    fn a_js_flockfile_under_the_flag_is_evaluated() {
        if !node_available() { return; }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.js");
        std::fs::write(
            &path,
            "module.exports = { app: [{ name: \"web\", script: \"./srv\" }] };",
        ).unwrap();
        let apps = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "web");
    }

    /// fails if a throwing config is reported as anything but InvalidConfig,
    /// or if node's own message is dropped on the floor.
    #[test]
    fn a_js_flockfile_that_throws_is_an_invalid_config_quoting_node() {
        if !node_available() { return; }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.js");
        std::fs::write(&path, "throw new Error('sheep dip empty');").unwrap();
        let err = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap_err();
        assert_eq!(target_exit_code(&err), ExitCode::InvalidConfig);
        assert!(err.to_string().contains("sheep dip empty"), "got: {err}");
    }

    /// fails if a pm2 ecosystem file is accepted, or if the refusal stops
    /// naming the key the operator has to change. Decision 2: this feature
    /// reads a Flockfile-shaped .js, and serde's own message is the answer.
    #[test]
    fn a_pm2_ecosystem_shape_is_refused_naming_the_right_key() {
        if !node_available() { return; }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ecosystem.config.js");
        std::fs::write(
            &path,
            "module.exports = { apps: [{ name: \"web\", script: \"./srv\" }] };",
        ).unwrap();
        let err = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap_err();
        assert_eq!(target_exit_code(&err), ExitCode::InvalidConfig);
        let msg = err.to_string();
        assert!(msg.contains("apps"), "must name what was written: {msg}");
        assert!(msg.contains("app"), "must name what was expected: {msg}");
    }
```

The missing-node path has **no test**, and pretending otherwise would be
worse than admitting it: producing it requires a `PATH` without node, and
`std::env::set_var` is `unsafe` in edition 2024 in a crate that forbids
unsafe. Say so in a comment above `evaluate_js_flockfile`, the way
`lifecycle.rs:411-420` already admits its stdin gap. The sentence itself is
still pinned — Task 9 puts it in `docs/migration.md` and greps for it.

### Step 1.5 — GREEN: shep-core's stale prediction, and the discovery pin

In `crates/shep-core/src/config/flockfile.rs`, rewrite `FlockfileError`'s
`#[non_exhaustive]` rationale. It currently predicts a fifth backend that
decision 4 rules out:

```rust
/// `#[non_exhaustive]`: shep-core is a library crate, so an out-of-tree
/// consumer can match this exhaustively and a new variant would break them
/// with no version bump to say so (IR-20). Growth is anticipated per
/// backend, not per format: `.js` Flockfiles do NOT appear here, because
/// shep-core never executes anything — the node bridge lives in shep-cli
/// (`commands::lifecycle`) and feeds its output back through
/// [`FlockFormat::Json`], which is what this module's own doc promises.
```

And pin the discovery order, which is Rin's ruling made mechanical:

```rust
    /// fails if a `.js` name is ever added to the discovery order. Rin's
    /// ruling, 2026-08-15: a `.js` Flockfile is read only when named
    /// explicitly on the command line, because reading one runs node on it,
    /// and `cd` into a cloned repo followed by `shep start` must not execute
    /// a stranger's JavaScript. Discovery is the path with no operator in
    /// the loop, so it is the path that must never reach node.
    #[test]
    fn discovery_never_names_a_js_file_and_stays_ten_names() {
        assert_eq!(DISCOVERY_ORDER.len(), 10);
        for name in DISCOVERY_ORDER {
            assert!(
                !name.ends_with(".js"),
                "{name} would let `shep start` execute a repo's JavaScript"
            );
            assert!(FlockFormat::from_path(Path::new(name)).is_some());
        }
    }
```

### Step 1.6 — SECURITY.md

One paragraph under the existing preconditions/properties structure (IR-42):

> **Config files are data, with one opt-in exception.** `shep-core` parses
> every Flockfile format with strict serde and executes nothing; it does not
> spawn a process on any path. A `.js` Flockfile is the exception and is
> reached only through `shep start <path> --flockfile`, which runs the file
> through `node`. Directory discovery never selects a `.js` file, so entering
> a directory and running `shep start` cannot execute code that directory
> contains.

### Step 1.7 — verify

```bash
grep -rn "JSON.stringify(require" crates | wc -l                     # 0 -> 1
grep -c "a fifth backend" crates/shep-core/src/config/flockfile.rs || true   # 1 -> 0
cargo test -p shep-core --lib --all-features
cargo test -p shep-cli --bins --all-features
```

Expect +1 in shep-core (the discovery pin), +7 in shep-cli (three flag/parse,
three-or-four resolve, one cli.rs parse). On a machine without node, three of
those pass by returning early and print `SKIPPED:` — check the output actually
contains it, because that is the difference between "node is absent" and "the
skip helper is broken".

### Step 1.8 — MUTATION

In `resolve_target`, move the `as_flockfile` arm **below** the
`(_, Some(format))` arm.

Expected: `the_flag_refuses_an_extension_it_cannot_read` and both `.js` cases
fail, because a `.js` path with `FlockFormat::from_path == None` now falls
through to the script arm. Blast radius 1 (shep-cli lib tests). Revert.

**Second mutation:** delete the `as_flockfile` guard entirely so the arm always
runs. Expected: `a_js_file_without_the_flag_is_still_a_script` fails, and so
does `any_other_existing_path_becomes_one_minimal_app_named_for_its_stem` — the
pre-existing test whose fixture is `server.js`. Two failures, one of them a
test nobody wrote for this feature, is the confirmation that decision 1 is
guarding something real. Revert.

### Step 1.9 — gate

Full task gate, one command at a time.

---

## Task 2 — schemars derive, `assets/flockfile.schema.json`, and `shep schema`

**Files:** `crates/shep-core/Cargo.toml`, `crates/shep-core/src/config/app.rs`,
`crates/shep-core/src/values.rs`, `crates/shep-cli/Cargo.toml`,
`crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/commands/schema.rs` (new),
`crates/shep-cli/src/commands/mod.rs`, `crates/shep-cli/src/main.rs`,
`assets/flockfile.schema.json` (new).

### Step 2.1 — baseline

```bash
grep -c "schemars" crates/shep-core/Cargo.toml || true    # 0
grep -c "^\[features\]" crates/shep-core/Cargo.toml || true  # 0
grep -rn --include="*.rs" "JsonSchema" crates | wc -l     # 0
find assets -maxdepth 1 -name '*.json' | wc -l            # 0
git ls-files assets | wc -l                               # 2
grep -c "schemars" Cargo.lock                             # 5
```

The `Cargo.lock` count of **5** is the one that matters: it must still be 5
afterwards. schemars is already in the tree via rmcp; this task must add
**zero** packages. If it becomes 6 or more, two schemars majors are resolved,
which means two `JsonSchema` traits and a derive that does not satisfy the one
the consumer wants — stop and reconcile the version.

### Step 2.2 — GREEN: the feature and the derives

`crates/shep-core/Cargo.toml`:

```toml
[features]
# Off by default. `JsonSchema` derives for the Flockfile schema
# (`assets/flockfile.schema.json`), which `shep schema` prints. shep-core is a
# published library and shep-daemon has an idle-RSS budget; neither should
# compile `schemars` plus its proc macro for an artefact only the CLI emits.
# shep-cli turns it on; `--all-features` turns it on in every gate command.
schema = ["dep:schemars"]

[dependencies]
# … existing …
# Adds no package to the tree: `rmcp`'s server feature already resolves
# schemars 1.2.2 for whistle, workspace-pinned exactly. Verified with
# `grep -c schemars Cargo.lock` == 5 before and after.
schemars = { workspace = true, optional = true }
```

On `AppConfig`, `ProbeConfig`, `ProbeKind` in `app.rs`:

```rust
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
```

Placed **below** the existing `#[derive(...)]` and above `#[serde(...)]`, and
leaving the `// wire format: changing field names/defaults is a breaking
change` comments where they are.

`MemSize` and `UpDuration` get **hand-written** impls in `values.rs` — the
derive would describe the newtype's inner integer and be flatly wrong, since
both serialize as strings:

```rust
/// String-shaped, matching this type's `Serialize`/`Deserialize`, which go
/// through `Display`/`FromStr` rather than the wrapped `u64`. A derive here
/// would emit `{"type":"integer"}` and describe a wire form that does not
/// exist.
///
/// The pattern is `FromStr`'s own grammar, lifted from its doc comment
/// above. If you change one, change the other — the paired tests below are
/// what catch it.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for MemSize {
    fn schema_name() -> alloc::borrow::Cow<'static, str> { "MemSize".into() }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": r"^\d+(G|M|K)?$",
            "description": "A byte quantity: digits, optionally suffixed G, M or K (binary units).",
        })
    }
}
```

and the same shape for `UpDuration` with `^\d+(h|m|s)?$` and "A duration:
digits, optionally suffixed h, m or s. Plain digits are milliseconds."

**Confirm the `schemars` 1.2.2 API before writing these** — `schema_name`'s
return type and `json_schema!`'s availability are 1.x-era details and this
plan is not the authority on them. `cargo doc -p schemars --open`, or read
`~/.cargo/registry/src/*/schemars-1.2.2/src/lib.rs`. Phase 12a's Step 1.4 did
exactly this for ratatui and it is why that task had no API surprises.

Paired tests in `values.rs`, one per type — these are what stop the pattern
and `FromStr` drifting apart:

```rust
    /// fails if the schema pattern and `FromStr` disagree. A pattern that is
    /// merely self-consistent is worthless; it has to agree with the parser
    /// the schema claims to describe.
    #[cfg(feature = "schema")]
    #[test]
    fn the_schema_pattern_agrees_with_from_str() {
        let schema = serde_json::to_value(schemars::schema_for!(MemSize)).unwrap();
        let pattern = schema["pattern"].as_str().unwrap();
        let re = regex::Regex::new(pattern).unwrap();
        for accepted in ["512M", "1G", "4096", "7K"] {
            assert!(re.is_match(accepted), "pattern rejects {accepted}");
            assert!(accepted.parse::<MemSize>().is_ok(), "FromStr rejects {accepted}");
        }
        for rejected in ["512MB", "512m", "1.5G", "", "M"] {
            assert!(!re.is_match(rejected), "pattern accepts {rejected}");
            assert!(rejected.parse::<MemSize>().is_err(), "FromStr accepts {rejected}");
        }
    }
```

`regex` is already a shep-core dependency, so this adds nothing.

### Step 2.3 — GREEN: the verb and the drift check

`crates/shep-cli/Cargo.toml`: `shep-core = { workspace = true, features = ["schema"] }`.
Check the workspace dependency table's existing shape first — it is
`shep-core.workspace = true` today, and the features form has to keep whatever
the workspace entry sets.

`crates/shep-cli/src/cli.rs`, in `Commands`:

```rust
    /// Print the Flockfile JSON Schema. Hidden: the schema is committed at
    /// `assets/flockfile.schema.json`, and this is how it is regenerated.
    #[command(hide = true)]
    Schema,
```

`crates/shep-cli/src/commands/schema.rs`:

```rust
//! `shep schema`: the Flockfile JSON Schema, and the guard that keeps the
//! committed copy honest.
//!
//! The schema describes the **deserializer**, not the normalizer.
//! `AppConfig::kill_signal` is `Option<String>` here and stays a plain string
//! in the schema, even though `shep_core::config::normalize` accepts only
//! four spellings: the schema's job is to describe what serde will parse, and
//! a schema that described a validation step running in another crate at
//! another time would be wrong the moment those two diverged, in a way no
//! test could catch.
//!
//! [`COMMITTED`] is `include_str!`, deliberately. That makes
//! `assets/flockfile.schema.json` a compile-time input: delete it and this
//! crate does not build, change `AppConfig` and this module's own test fails
//! with the command that fixes it. A committed schema nobody regenerates is
//! a lie with a filename, and the only reliable guard is one that runs in
//! `cargo test` rather than in a CI job somebody can forget.

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::Streams;

/// The committed schema. Four `../` reaches the repository root from
/// `crates/shep-cli/src/commands/`.
const COMMITTED: &str = include_str!("../../../../assets/flockfile.schema.json");

/// How to regenerate the committed copy. Named in the drift test's own
/// failure message, so a red test is self-service.
const REGENERATE: &str = "cargo run -p shep-cli -- schema > assets/flockfile.schema.json";

/// Renders the Flockfile JSON Schema, pretty-printed with a trailing
/// newline so the committed file is a well-formed text file.
///
/// # Panics
///
/// Never in practice: `schemars` produces a `serde_json::Value` tree, which
/// `to_string_pretty` cannot fail on. `#[track_caller]` so a future change
/// that makes it fallible reports the caller (IR-24).
#[track_caller]
#[must_use]
pub fn flockfile_schema() -> String {
    let schema = schemars::schema_for!(shep_core::config::AppConfig);
    let mut rendered = serde_json::to_string_pretty(&schema)
        .expect("a schemars Schema always serializes");
    rendered.push('\n');
    rendered
}

/// Prints the schema. Always succeeds.
pub fn schema(streams: &mut Streams<'_>, _fmt: Format) -> ExitCode { … }
```

`--format json` is deliberately ignored: the output *is* JSON, and wrapping a
schema in the CLI's envelope would produce a file no editor could read. Say
that in the function's doc.

Co-located tests:

```rust
    /// fails whenever `AppConfig` changes and the committed schema does not.
    /// That includes a doc-comment edit: schemars reads `///` into
    /// `description`, which is the point — those become hover text in the
    /// operator's editor — so a docs-only change is a real schema change and
    /// regenerating is the correct response, not a sign anything broke.
    #[test]
    fn the_committed_schema_is_current() {
        let fresh = flockfile_schema();
        assert_eq!(
            fresh, COMMITTED,
            "assets/flockfile.schema.json is stale. Regenerate it:\n    {REGENERATE}\n\
             A doc-comment edit on AppConfig counts; schemars puts doc comments \
             into `description`."
        );
    }

    /// fails if the schema starts describing `normalize`'s grammar instead of
    /// serde's. The four signal names belong to a validation step in another
    /// crate; a schema that listed them would be describing something it
    /// cannot see.
    #[test]
    fn kill_signal_stays_an_unconstrained_string() {
        let schema: serde_json::Value = serde_json::from_str(&flockfile_schema()).unwrap();
        let field = &schema["properties"]["kill_signal"];
        assert!(
            field.to_string().contains("string"),
            "kill_signal should be a string: {field}"
        );
        assert!(
            field["enum"].is_null() && field["oneOf"].is_null(),
            "kill_signal must not enumerate normalize's grammar: {field}"
        );
    }

    /// fails if MemSize or UpDuration reverts to a derive and starts
    /// describing its inner integer.
    #[test]
    fn duration_and_memory_fields_are_string_shaped() {
        let schema: serde_json::Value = serde_json::from_str(&flockfile_schema()).unwrap();
        for field in ["min_uptime", "kill_timeout", "max_memory"] {
            let rendered = schema["properties"][field].to_string();
            assert!(
                rendered.contains("string"),
                "{field} must be string-shaped on the wire, got {rendered}"
            );
            assert!(
                !rendered.contains("integer"),
                "{field} looks like a derive on the newtype's inner value: {rendered}"
            );
        }
    }
```

Beware `$ref`: schemars may emit `{"$ref": "#/$defs/UpDuration"}` for a field
and put the string shape in `$defs`. Check the generated file before writing
the last two assertions and follow the ref if that is what it does. **Run
`flockfile_schema()` and read the output before finalising any assertion about
its shape** — that is the difference between a check and a guess.

### Step 2.4 — the artefact

Bootstrap order matters: `include_str!` will not compile without the file, so
create it empty first, build, then generate.

```bash
: > assets/flockfile.schema.json
cargo run -p shep-cli -- schema > assets/flockfile.schema.json
```

Then read it. Confirm `properties.name`, `properties.script`, that
`description` strings from the doc comments are present, and that
`additionalProperties` is `false` (`deny_unknown_fields` should produce that;
if it does not, say so rather than hand-editing the file).

`git add assets/flockfile.schema.json` — and check `git ls-files assets | wc -l`
goes `2 → 3`. `assets/` has no `.gitignore` but confirm rather than assume.

### Step 2.5 — verify

```bash
grep -c "schemars" Cargo.lock                     # still 5
find assets -maxdepth 1 -name '*.json' | wc -l    # 0 -> 1
git ls-files assets | wc -l                       # 2 -> 3
cargo test -p shep-core --lib --all-features
cargo test -p shep-cli --bins --all-features
```

### Step 2.6 — MUTATION

Add a field to `AppConfig`:

```rust
    /// Temporary mutation probe — remove.
    pub mutation_probe: Option<String>,
```

Expected: `the_committed_schema_is_current` fails and its message names the
regeneration command. If it passes, `include_str!` is reading a different
file than you think, or the test compares something other than the whole
string. Revert.

**Second mutation:** change `MemSize`'s `JsonSchema` pattern to
`^\d+(G|M|K|T)?$`. Expected: `the_schema_pattern_agrees_with_from_str` fails
on the `FromStr`-rejects side, *and* `the_committed_schema_is_current` fails.
Two independent failures for one edit is the property that makes the pattern
check worth having. Revert.

### Step 2.7 — gate

---

## Task 3 — `$schema` accepted and ignored (SEVERABLE)

If the phase is being trimmed, this is the task to cut. Cutting it costs
JSON/JSON5 editor completion and nothing else — TOML users reach the schema
through taplo's `#:schema` comment directive, which serde never sees.

**Files:** `crates/shep-core/src/config/flockfile.rs`, `docs/terminology.md`
(no) — just the one file, plus Task 9's docs.

### Step 3.1 — baseline

```bash
grep -c 'rename = "\$schema"' crates/shep-core/src/config/flockfile.rs || true   # 0
```

Today, `Flockfile::parse(r#"{"$schema":"x","app":[…]}"#, FlockFormat::Json)`
returns `Err(FlockfileError::Json("unknown field `$schema`, expected `app`…"))`.
Confirm that by writing the RED test first and watching it fail with that
message.

### Step 3.2 — RED then GREEN

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFlockfile {
    /// The editor's schema hint, read and discarded.
    ///
    /// This is the "future schema key" the comment above anticipated, added
    /// HERE explicitly rather than by relaxing `deny_unknown_fields`: a
    /// typo'd key must still fail loudly, and exactly one more key is now
    /// legal. shep does not validate against the named schema and makes no
    /// promise about it — it is a hint for the operator's editor, which is
    /// the only consumer that ever reads it.
    ///
    /// TOML Flockfiles do not need it: taplo's `#:schema <url>` directive is
    /// a comment, invisible to serde. JSON and JSON5 have no comment an
    /// editor agrees to look in, which is why this field exists at all.
    #[serde(default, rename = "$schema")]
    schema: Option<String>,
    #[serde(default, rename = "app")]
    apps: Vec<AppConfig>,
}
```

`schema` is read and dropped; add `let _ = raw.schema;` or bind it as
`_schema` — whichever keeps clippy quiet without an `#[allow]`.

**This is `RawFlockfile`, not `AppConfig`.** `RawFlockfile` is private, is not
on the wire, and has no pinned fixture. `PROTOCOL_VERSION` stays 1 and no
fixture changes. If you find yourself editing a `.snap` file, you have edited
the wrong struct.

Tests:

```rust
    #[test]
    fn a_schema_key_is_accepted_and_ignored() {
        let src = r#"{ "$schema": "./flockfile.schema.json",
                       "app": [{ "name": "web", "script": "./srv" }] }"#;
        let flock = Flockfile::parse(src, FlockFormat::Json).unwrap();
        assert_eq!(flock.apps.len(), 1);
    }

    /// fails if the new field is implemented by relaxing
    /// `deny_unknown_fields` instead of naming one more key — which would
    /// silently accept every typo the document lock exists to catch.
    #[test]
    fn one_more_key_is_legal_and_no_others_are() {
        let src = r#"{ "schema": "x", "app": [{ "name": "w", "script": "./s" }] }"#;
        assert!(
            matches!(Flockfile::parse(src, FlockFormat::Json), Err(FlockfileError::Json(_))),
            "bare `schema` (no $) must still be an unknown field"
        );
    }

    #[test]
    fn a_toml_flockfile_takes_the_key_too() {
        let src = "\"$schema\" = \"./flockfile.schema.json\"\n\
                   [[app]]\nname = \"web\"\nscript = \"./srv\"\n";
        assert_eq!(Flockfile::parse(src, FlockFormat::Toml).unwrap().apps.len(), 1);
    }
```

### Step 3.3 — MUTATION

Delete `deny_unknown_fields` from `RawFlockfile`. Expected:
`one_more_key_is_legal_and_no_others_are` fails. If it passes, that test is
asserting nothing and the document lock has quietly been given away. Revert.

### Step 3.4 — gate

---

## Task 4 — `DaemonConfig`: `#[non_exhaustive]`, extracted `validate`, `DaemonOverrides`

**Files:** `crates/shep-core/src/config/daemon.rs`,
`crates/shep-core/src/config/mod.rs`.

No CLI changes. Task 5 wires it up; this task lands the shep-core half so the
two can be reviewed separately.

### Step 4.1 — baseline

```bash
grep -c "fn validate" crates/shep-core/src/config/daemon.rs || true    # 0
grep -c "^#\[non_exhaustive\]" crates/shep-core/src/config/daemon.rs   # 1 (DaemonConfigError)
grep -c "DaemonOverrides" crates/shep-core/src/config/daemon.rs || true # 0
grep -rn "DaemonConfig" crates | grep -v "shep-core/src/config/daemon.rs" | grep -c "DaemonConfig {" || true   # 0
cargo test -p shep-core --lib --all-features
```

That last one is the ruling's load-bearing fact: **nothing outside shep-core
constructs a `DaemonConfig` by struct literal**, so `#[non_exhaustive]` is a
zero-diff change at every call site. If it prints anything but `0`, decision 7
needs revisiting before you go further.

### Step 4.2 — GREEN: the attribute and its rationale

```rust
/// Parsed daemon configuration with raw per-dog sections
///
/// Dog sections stay untyped here: each dog deserializes its own
/// `[dog.<name>]` table so dog config schemas live with the dog code.
///
/// `#[non_exhaustive]` here is a **construction gate, not wire growth** — a
/// different reason from IR-20's error-enum default and from `ProcessInfo`'s.
/// Outside this crate the attribute makes both `DaemonConfig { … }` and
/// `DaemonConfig { …, ..Default::default() }` fail to compile, so
/// [`Self::load`], [`Self::load_layered`] and `Default` are the only ways to
/// obtain one — and the first two validate. Holding a `DaemonConfig` outside
/// shep-core therefore proves it was validated, which is the property
/// `ResolvedApp` gets from private fields, at none of the cost: the fields
/// stay `pub` and readable, so `shep_toml`, `dogs` and `whistle::gate` need
/// no accessors for a `BTreeMap<String, toml::Table>` they legitimately
/// read.
///
/// It is deliberately NOT the full `ResolvedApp` treatment. `ResolvedApp`
/// protects a property of *travel* — the supervisor receives one and must
/// trust normalization it cannot see. A `DaemonConfig` never travels: every
/// one of the three production sites loads and consumes it within a few
/// lines, and the daemon holds a `BootOptions`, not this. `docs/specs/deferred.md`
/// asked the question and named this phase's flags layer as what would force
/// it; this is the answer.
///
/// Nothing in the repository guards the attribute itself — it is invisible
/// inside the defining crate, and observing it needs a `trybuild`
/// compile-fail tier this project has declined once already (see
/// `tests/process_info_builder_from_outside_the_crate.rs`, which admits the
/// same gap for `ProcessInfo` and is required to stay shep-core's only
/// `tests/` file).
#[non_exhaustive]
#[derive(Clone, Default, PartialEq)]
pub struct DaemonConfig { … }
```

### Step 4.3 — GREEN: `validate` out of `load`, and `load_layered`

Move the floor check out of `load` into:

```rust
    /// Checks every invariant a `DaemonConfig` carries, whatever layers
    /// produced it.
    ///
    /// One call site, at the bottom of [`Self::load_layered`], and that is
    /// the point: validating per layer would stop a good `--max-cron-sleep`
    /// from rescuing a broken `shep.toml`, which is not what
    /// `file < env < flags` means. The same reasoning the env layer already
    /// carries, extended one layer up.
    ///
    /// `key` is provenance — the spelling the operator actually set, so the
    /// refusal names the thing they can edit.
    ///
    /// # Errors
    ///
    /// - [`DaemonConfigError::BelowMinimum`] — `max_cron_sleep` is under the
    ///   floor that keeps the cron loop from spinning.
    fn validate(&self, key: &'static str) -> Result<(), DaemonConfigError> {
        if let Some(value) = self.daemon.max_cron_sleep
            && value < MIN_CRON_SLEEP
        {
            return Err(DaemonConfigError::BelowMinimum { key, value, min: MIN_CRON_SLEEP });
        }
        Ok(())
    }
```

`load` keeps its exact signature and delegates:

```rust
    pub fn load(
        file_source: Option<&str>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, DaemonConfigError> {
        Self::load_layered(file_source, env, &DaemonOverrides::new())
    }
```

`load_layered` is `load`'s current body with the floor check replaced by an
`overrides` pass and one `self.validate(key)?` at the end:

```rust
        if let Some(value) = overrides.log_json { cfg.daemon.log_json = value; }
        if let Some(value) = overrides.log_level { cfg.daemon.log_level = value; }
        if let Some(value) = &overrides.socket { cfg.daemon.socket = Some(value.clone()); }
        if let Some(value) = overrides.max_cron_sleep {
            cfg.daemon.max_cron_sleep = Some(value);
            max_cron_sleep_key = "--max-cron-sleep";
        }
        cfg.validate(max_cron_sleep_key)?;
        Ok(cfg)
```

Extend the existing provenance comment above `max_cron_sleep_key` to name the
third layer rather than leaving it describing two.

### Step 4.4 — GREEN: `DaemonOverrides` and the shared bool grammar

```rust
/// The CLI-flag layer of `file < env < flags` (spec §5).
///
/// Every field is `Option`: `None` means the flag was absent and the layer
/// below wins. Nothing here validates — [`DaemonConfig::validate`] runs once,
/// after all three layers, so a flag can rescue a file the layer below would
/// have rejected.
///
/// `#[non_exhaustive]` because this type grows a field every time the hidden
/// `daemon` subcommand grows a flag; that is anticipated by construction, not
/// hypothetical (IR-20's spirit, applied to an input struct). Build one with
/// [`Self::new`] and the chained setters — the consuming-self shape
/// `ProcessInfo::builder` already uses in this workspace.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonOverrides {
    /// `--log-json`
    pub log_json: Option<bool>,
    /// `--log-level`
    pub log_level: Option<LogLevel>,
    /// `--socket`
    pub socket: Option<PathBuf>,
    /// `--max-cron-sleep`
    pub max_cron_sleep: Option<UpDuration>,
}

impl DaemonOverrides {
    /// An empty layer — every flag absent.
    #[must_use]
    pub fn new() -> Self { Self::default() }
    /// Sets the `--log-json` override.
    #[must_use]
    pub fn log_json(mut self, value: Option<bool>) -> Self { self.log_json = value; self }
    // … log_level, socket, max_cron_sleep, same shape
}
```

`Debug` is derived rather than redacted: four values, none of them a secret —
a socket path and a log level are already visible in `ps`. Say so, because
IR-41 makes a derived `Debug` a decision that has to be written down.

And the shared bool grammar:

```rust
/// The four spellings a boolean takes in shep's daemon config, in the
/// environment and on the command line alike: `1`, `0`, `true`, `false`.
///
/// One function so the two layers cannot drift. clap's own
/// `BoolishValueParser` additionally accepts `yes`/`no`/`y`/`n`/`on`/`off`;
/// using it would widen the grammar on the flag side only, and widening an
/// input grammar beyond spec is a named drift risk on this project.
#[must_use]
pub fn parse_bool_value(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}
```

`load`'s `SHEP_LOG_JSON` arm calls it. Export `DaemonOverrides` and
`parse_bool_value` from `config/mod.rs` alongside `DaemonConfig`.

### Step 4.5 — tests

```rust
    /// fails if validation moves back into a per-layer position — the flags
    /// layer must be able to rescue a file the layer below would reject,
    /// which is what `file < env < flags` means. Same rule the env layer's
    /// own comment already states.
    #[test]
    fn a_flag_rescues_a_below_floor_file_value() {
        let cfg = DaemonConfig::load_layered(
            Some("[daemon]\nmax_cron_sleep = \"500\"\n"),
            &no_env,
            &DaemonOverrides::new().max_cron_sleep(Some(UpDuration::from_millis(300_000))),
        )
        .unwrap();
        assert_eq!(cfg.daemon.max_cron_sleep, Some(UpDuration::from_millis(300_000)));
    }

    /// fails if a below-floor FLAG is accepted, or if the refusal names the
    /// TOML key the operator did not set.
    #[test]
    fn a_below_floor_flag_is_refused_naming_the_flag() {
        let err = DaemonConfig::load_layered(
            None,
            &no_env,
            &DaemonOverrides::new().max_cron_sleep(Some(UpDuration::from_millis(500))),
        )
        .unwrap_err();
        assert_eq!(
            err,
            DaemonConfigError::BelowMinimum {
                key: "--max-cron-sleep",
                value: UpDuration::from_millis(500),
                min: MIN_CRON_SLEEP,
            }
        );
        assert!(err.to_string().contains("--max-cron-sleep"), "got: {err}");
    }

    /// fails if a flag stops beating the env layer.
    #[test]
    fn a_flag_beats_the_environment() {
        let env = |k: &str| (k == "SHEP_LOG_LEVEL").then(|| "trace".to_string());
        let cfg = DaemonConfig::load_layered(
            Some("[daemon]\nlog_level = \"error\"\n"),
            &env,
            &DaemonOverrides::new().log_level(Some(LogLevel::Info)),
        )
        .unwrap();
        assert_eq!(cfg.daemon.log_level, LogLevel::Info);
    }

    /// fails if an absent flag overwrites the layer below with a default —
    /// the classic `Option`-flattening bug, and the one a `bool` field
    /// instead of `Option<bool>` would guarantee.
    #[test]
    fn an_absent_flag_leaves_every_lower_layer_alone() {
        let src = "[daemon]\nlog_json = true\nlog_level = \"debug\"\nsocket = \"/tmp/s.sock\"\n";
        let layered = DaemonConfig::load_layered(Some(src), &no_env, &DaemonOverrides::new()).unwrap();
        let plain = DaemonConfig::load(Some(src), &no_env).unwrap();
        assert_eq!(layered, plain);
    }

    #[test]
    fn the_bool_grammar_is_exactly_four_spellings() {
        assert_eq!(parse_bool_value("1"), Some(true));
        assert_eq!(parse_bool_value("0"), Some(false));
        assert_eq!(parse_bool_value("true"), Some(true));
        assert_eq!(parse_bool_value("false"), Some(false));
        for wider in ["yes", "no", "on", "off", "TRUE", "y"] {
            assert_eq!(parse_bool_value(wider), None, "{wider} must not be a boolean here");
        }
    }
```

### Step 4.6 — verify

```bash
grep -c "fn validate" crates/shep-core/src/config/daemon.rs           # 0 -> 1
grep -c "^#\[non_exhaustive\]" crates/shep-core/src/config/daemon.rs  # 1 -> 3
cargo test -p shep-core --lib --all-features                     # +5
cargo test -p shep-cli --bins --all-features                     # unchanged; load's signature did not move
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::  # unchanged
```

`shep-cli` and `shep-daemon` being **unchanged** is the check that `load` kept
its contract. If either moves, `load_layered` was not a pure extension.

### Step 4.7 — MUTATION

Move `self.validate(key)?` from the bottom of `load_layered` to immediately
after the file parse. Expected: `a_flag_rescues_a_below_floor_file_value`
fails, and so does the pre-existing env-rescue test, if there is one. Revert.

**Second mutation:** change `DaemonOverrides::log_json` to `bool`. Expected:
`an_absent_flag_leaves_every_lower_layer_alone` fails, because an absent flag
now writes `false` over a file that said `true`. Revert.

### Step 4.8 — gate

---

## Task 5 — the daemon flags layer, wired

**Files:** `crates/shep-cli/src/cli.rs`,
`crates/shep-cli/src/commands/daemon.rs`.

### Step 5.1 — baseline

```bash
grep -c "pub no_restore\|pub foreground" crates/shep-cli/src/cli.rs   # 2
grep -c "load_layered" crates/shep-cli/src/commands/daemon.rs || true # 0
cargo test -p shep-cli --bins --all-features
```

`shep daemon --log-level info` today prints clap's
`unexpected argument '--log-level' found` and exits 2. Confirm before you
start; it is the RED state for the parse tests.

### Step 5.2 — RED then GREEN: `DaemonArgs`

```rust
/// Arguments to the hidden `shep daemon` subcommand.
///
/// The last four are the CLI-flag layer of spec §5's `file < env < flags`,
/// one per `SHEP_*` variable `DaemonConfig::load` already reads. They live
/// here rather than on `GlobalArgs` because they configure **the shepherd**,
/// and this is the only invocation that runs one — `--log-level` on
/// `shep flock` would configure nothing.
///
/// Their real audience is an init unit's `ExecStart`, which can now say
/// `shep daemon --foreground --log-level info` without a config file.
#[derive(Debug, clap::Args)]
pub struct DaemonArgs {
    /// Boot without restoring the saved muster roll
    #[arg(long)]
    pub no_restore: bool,
    /// Run supervised by an init system: do not expect to have been
    /// daemonized, and report readiness once the flock is back
    #[arg(long)]
    pub foreground: bool,
    /// Emit the shepherd's own logs as JSON lines (overrides shep.toml and
    /// SHEP_LOG_JSON). Accepts 1, 0, true, false; bare means true.
    #[arg(long, value_name = "BOOL", num_args = 0..=1,
          default_missing_value = "true", value_parser = bool_flag)]
    pub log_json: Option<bool>,
    /// Lowest severity of the shepherd's own records that reaches its log
    #[arg(long, value_name = "LEVEL", value_parser = log_level_flag)]
    pub log_level: Option<shep_core::config::LogLevel>,
    /// Control-socket path override
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Longest a cron worker sleeps before re-deriving its next occurrence
    #[arg(long, value_name = "DURATION", value_parser = duration_flag)]
    pub max_cron_sleep: Option<shep_core::values::UpDuration>,
}

/// clap value parser over shep's own four boolean spellings — NOT clap's
/// `BoolishValueParser`, which also takes yes/no/y/n/on/off and would widen
/// the grammar on the flag side only.
fn bool_flag(value: &str) -> Result<bool, String> {
    shep_core::config::parse_bool_value(value)
        .ok_or_else(|| format!("expected one of 1, 0, true, false; got `{value}`"))
}

/// clap value parser over [`LogLevel::from_name`] — the same lowercase-only
/// grammar `SHEP_LOG_LEVEL` accepts.
fn log_level_flag(value: &str) -> Result<shep_core::config::LogLevel, String> { … }

/// clap value parser over `UpDuration`'s `FromStr`.
fn duration_flag(value: &str) -> Result<shep_core::values::UpDuration, String> { … }
```

Parse tests in `cli.rs`, three states for `--log-json` and one grammar
refusal each:

```rust
    #[test]
    fn log_json_has_three_states() {
        use clap::Parser;
        let cases = [
            (vec!["shep", "daemon"], None),
            (vec!["shep", "daemon", "--log-json"], Some(true)),
            (vec!["shep", "daemon", "--log-json=false"], Some(false)),
            (vec!["shep", "daemon", "--log-json=1"], Some(true)),
        ];
        for (argv, expected) in cases {
            match Cli::try_parse_from(&argv).unwrap().command {
                Commands::Daemon(args) => assert_eq!(args.log_json, expected, "{argv:?}"),
                other => panic!("expected Daemon, got {other:?}"),
            }
        }
    }

    /// fails if the flag grammar widens past the env grammar — the exact
    /// drift `parse_bool_value` exists to prevent.
    #[test]
    fn the_flag_bool_grammar_matches_the_env_grammar() {
        use clap::Parser;
        for wider in ["--log-json=yes", "--log-json=on", "--log-json=TRUE"] {
            assert!(
                Cli::try_parse_from(["shep", "daemon", wider]).is_err(),
                "{wider} must not parse"
            );
        }
    }
```

### Step 5.3 — GREEN: `run_daemon`

`commands/daemon.rs:223-226`:

```rust
    let overrides = DaemonOverrides::new()
        .log_json(args.log_json)
        .log_level(args.log_level)
        .socket(args.socket.clone())
        .max_cron_sleep(args.max_cron_sleep);
    let config = DaemonConfig::load_layered(file_source.as_deref(), &env, &overrides)
        .map_err(DaemonRunError::Config)?;
```

Extend `run_daemon`'s doc: the `SHEP_*` paragraph currently reads as if the
environment is the top layer, and after this it is not.

A co-located test proving the whole chain rather than the pieces:

```rust
    /// fails if `run_daemon` builds its overrides from the wrong fields, or
    /// drops one. Drives the config assembly, not the boot — booting a real
    /// shepherd is `daemon_e2e`'s job.
    #[test]
    fn every_daemon_flag_reaches_the_config() {
        let args = DaemonArgs {
            no_restore: false,
            foreground: false,
            log_json: Some(true),
            log_level: Some(LogLevel::Trace),
            socket: Some(PathBuf::from("/tmp/flag.sock")),
            max_cron_sleep: Some(UpDuration::from_millis(120_000)),
        };
        let overrides = daemon_overrides(&args);   // extract the builder into a fn to test it
        let cfg = DaemonConfig::load_layered(
            Some("[daemon]\nlog_json = false\nlog_level = \"error\"\nsocket = \"/tmp/file.sock\"\n"),
            &|_| None,
            &overrides,
        )
        .unwrap();
        assert!(cfg.daemon.log_json);
        assert_eq!(cfg.daemon.log_level, LogLevel::Trace);
        assert_eq!(cfg.daemon.socket, Some(PathBuf::from("/tmp/flag.sock")));
        assert_eq!(cfg.daemon.max_cron_sleep, Some(UpDuration::from_millis(120_000)));
    }
```

Extracting `fn daemon_overrides(args: &DaemonArgs) -> DaemonOverrides` is what
makes this testable without booting; do that rather than inlining the builder
in `run_daemon`.

### Step 5.4 — verify

```bash
grep -c "load_layered" crates/shep-cli/src/commands/daemon.rs   # 0 -> 1
cargo test -p shep-cli --bins --all-features                    # +3
```

### Step 5.5 — MUTATION

Swap `.log_level(args.log_level)` for `.log_level(None)` in
`daemon_overrides`. Expected: `every_daemon_flag_reaches_the_config` fails on
the `log_level` assertion, and nothing else does — which is exactly why that
test asserts all four fields rather than sampling one. Revert.

### Step 5.6 — gate

---

## Task 6 — runtime init detection and `--init`

**Files:** `crates/shep-cli/src/cli.rs`,
`crates/shep-cli/src/commands/startup/mod.rs`,
`crates/shep-cli/src/commands/startup/unit.rs`.

This task adds **no new renderer**. It makes `Init` selectable, which is what
Tasks 7 and 8 need, and it is separately reviewable for exactly that reason.

### Step 6.1 — baseline

```bash
grep -c "allow(dead_code)" crates/shep-cli/src/commands/startup/unit.rs   # 2
grep -c "target_os" crates/shep-cli/src/commands/startup/mod.rs           # 3
grep -c "init" crates/shep-cli/src/cli.rs                                 # record it
cargo test -p shep-cli --bins --all-features
```

`shep startup --init systemd` today prints clap's
`unexpected argument '--init' found`, exit 2.

### Step 6.2 — GREEN: three more `Init` variants

In `unit.rs`, extend `Init` to five variants and **delete both
`#[cfg_attr(…, allow(dead_code))]` attributes** — with `--init`, every variant
is constructible on every target, so the narrowing is now a lie that would
suppress a real warning:

```rust
/// Which init system a unit is written for.
///
/// Five variants, all constructible on every target: `--init` lets an
/// operator name one directly, which is also what lets a macOS machine
/// exercise the systemd, openrc and rc.d renderers at all. Selection without
/// the flag is [`super::current_init`] — a runtime probe on Linux, where
/// systemd and openrc share one target triple, and a compile-time fact
/// everywhere else, where nothing else the target could be exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub(crate) enum Init {
    /// Linux + systemd: a unit file, `Type=notify`.
    Systemd,
    /// Linux + openrc: an `openrc-run` script. No readiness protocol — see
    /// [`super::unit::openrc_script`].
    Openrc,
    /// macOS: a `LaunchDaemon` plist.
    Launchd,
    /// FreeBSD: an `/etc/rc.subr` script under `/usr/local/etc/rc.d`.
    FreebsdRc,
    /// OpenBSD: an `/etc/rc.d/rc.subr` script under `/etc/rc.d`.
    OpenbsdRc,
}
```

`clap::ValueEnum` on a `pub(crate)` type inside `commands::startup::unit`,
referenced from `cli.rs` — check the visibility path compiles; if `cli.rs`
cannot name it, move `Init` up to a `pub(crate)` re-export rather than making
the module public.

### Step 6.3 — GREEN: the probe

In `mod.rs`, replace `current_init`:

```rust
/// The init system this host is actually running, or `None` when it is one
/// shep has no renderer for.
///
/// Linux is a **runtime** probe: systemd and openrc share one target triple,
/// so `target_os` cannot tell them apart, and until this existed a Linux host
/// running openrc was silently written a systemd unit whose failure surfaced
/// only when `systemctl` turned out not to exist. `/run/systemd/system` is
/// the check `sd_booted(3)` itself makes — the one probe here with an
/// upstream contract behind it.
///
/// Every other target is a compile-time fact: there is nothing else macOS,
/// FreeBSD or OpenBSD could be.
///
/// **This is stricter than what it replaces.** A Linux container with no
/// `/run/systemd/system` used to get a systemd unit written into it and now
/// gets a refusal. That is the right answer — a unit with no init to read it
/// does nothing — but it is a case that worked before, so `--init` exists to
/// override this entirely.
fn current_init() -> Option<Init> {
    #[cfg(target_os = "linux")]
    {
        if Path::new("/run/systemd/system").is_dir() {
            return Some(Init::Systemd);
        }
        if Path::new("/run/openrc/softlevel").exists() || Path::new("/run/openrc").is_dir() {
            return Some(Init::Openrc);
        }
        None
    }
    #[cfg(target_os = "macos")]
    { Some(Init::Launchd) }
    #[cfg(target_os = "freebsd")]
    { Some(Init::FreebsdRc) }
    #[cfg(target_os = "openbsd")]
    { Some(Init::OpenbsdRc) }
    #[cfg(not(any(target_os = "linux", target_os = "macos",
                  target_os = "freebsd", target_os = "openbsd")))]
    { None }
}
```

Note the Linux arm no longer returns a bare expression — it has early returns,
so `current_init` cannot stay `const fn`. Drop `const`.

`plan`'s refusal names both probes now, and clippy's `-D warnings` on an
unused `Path` import on non-Linux is a real risk — check the Linux
cross-check, not just the host build.

```rust
    let Some(init) = args.init.or_else(current_init) else {
        return Err(Refusal {
            code: ExitCode::Failure,
            message: "could not tell which init system is running: neither \
                      /run/systemd/system nor /run/openrc is present. Name one \
                      with --init (systemd, openrc, launchd, freebsd-rc, openbsd-rc)"
                .to_string(),
        });
    };
```

Plain, per the terminology rule — a refusal carries no theme.

### Step 6.4 — GREEN: `StartupArgs` and per-init paths

```rust
pub struct StartupArgs {
    /// The user the unit runs the shepherd as (default: $SUDO_USER, else the invoking user)
    #[arg(long)]
    pub user: Option<String>,
    /// Write a unit for this init system instead of the detected one.
    ///
    /// `unstartup` takes it too: a unit installed under one init has to be
    /// removable after the host has changed to another.
    #[arg(long, value_enum)]
    pub init: Option<Init>,
}
```

`unit_path` and the mode both become functions of `Init`:

```rust
/// The mode a generated unit is created with.
///
/// A systemd unit and a launchd plist are **read** by their init system:
/// 0644. An openrc script and a BSD rc.d script are **executed**: 0755.
/// Shipping an openrc script at 0644 fails at the next reboot, which is the
/// worst possible time to find out.
pub(crate) const fn unit_mode(init: Init) -> u32 {
    match init {
        Init::Systemd | Init::Launchd => 0o644,
        Init::Openrc | Init::FreebsdRc | Init::OpenbsdRc => 0o755,
    }
}
```

Tasks 7 and 8 supply the paths; this task adds the `match` arms with
`unimplemented!()` — **no.** Do not leave `unimplemented!()` in a shipped
tree. Land Task 6 with the three new variants mapping to a `Refusal` naming
"not yet built in this phase", and Tasks 7 and 8 replace those arms. That way
every intermediate state of the tree is a tree that runs.

### Step 6.5 — tests

```rust
    #[test]
    fn the_mode_is_read_only_for_units_and_executable_for_scripts() {
        assert_eq!(unit_mode(Init::Systemd), 0o644);
        assert_eq!(unit_mode(Init::Launchd), 0o644);
        assert_eq!(unit_mode(Init::Openrc), 0o755);
        assert_eq!(unit_mode(Init::FreebsdRc), 0o755);
        assert_eq!(unit_mode(Init::OpenbsdRc), 0o755);
    }

    /// fails if `--init` stops overriding detection — the escape hatch for a
    /// container with no /run/systemd/system, and the only way a macOS
    /// machine renders a systemd unit at all.
    #[test]
    fn an_explicit_init_beats_detection() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["shep", "startup", "--init", "openrc"]).unwrap();
        match cli.command {
            Commands::Startup(args) => assert_eq!(args.init, Some(Init::Openrc)),
            other => panic!("expected Startup, got {other:?}"),
        }
    }

    /// fails if unstartup loses the flag — without it a unit installed under
    /// one init cannot be removed after the host changes to another.
    #[test]
    fn unstartup_takes_the_init_flag_too() { … }
```

### Step 6.6 — verify

```bash
grep -c "allow(dead_code)" crates/shep-cli/src/commands/startup/unit.rs   # 2 -> 0
cargo test -p shep-cli --bins --all-features                              # +3
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace --all-targets --all-features --target x86_64-unknown-linux-gnu
```

The Linux cross-check is not optional here. The `cfg` block in `current_init`
has five arms and only one of them compiles on this machine.

### Step 6.7 — MUTATION

Reverse the probe order — openrc before systemd. This cannot be caught by a
test on macOS, and saying so is the point: **note it in the task report as an
untested branch**, and have the Linux cross-check confirm only that it
compiles. Do not invent a test that fakes `/run/systemd/system`; a test that
would need to create a path under `/run` is a test that does not run.

Do the mutation on something that *can* fail instead: change
`unit_mode(Init::Openrc)` to `0o644`. Expected:
`the_mode_is_read_only_for_units_and_executable_for_scripts` fails. Revert.

### Step 6.8 — gate

---

## Task 7 — the openrc renderer

**Files:** `crates/shep-cli/src/commands/startup/unit.rs`,
`crates/shep-cli/src/commands/startup/mod.rs`.

### Step 7.1 — baseline

```bash
grep -rni "openrc" crates | wc -l        # 4 before Task 6; re-run after Task 6 and record
grep -c "rc-update" crates/shep-cli/src/commands/startup/mod.rs || true   # 0
```

Two of the pre-Task-6 four are comments asserting openrc does not exist. They
must be gone by the end of this task.

### Step 7.2 — GREEN: the script

`/etc/init.d/shep-<user>`. openrc names *files*, not shell variables, so `-`
is fine and the systemd naming carries over.

```rust
/// Renders the openrc init script.
///
/// `supervise-daemon` (openrc >= 0.21) rather than `start-stop-daemon`,
/// because it supervises the foreground process the way systemd does rather
/// than daemonizing and tracking a pidfile — and `shep daemon --foreground`
/// is the entry point both other renderers already use.
///
/// **openrc has no `sd_notify` analogue.** Nothing tells openrc the shepherd
/// is ready; `supervise-daemon` marks the service started the instant the
/// process is spawned, which is before the muster restore has finished. The
/// `start_post` poll below is what closes that gap, and it is not a
/// consolation prize: it proves exactly what `READY=1` proves. `boot` binds
/// the control socket at step 2, restores the roll and starts the dogs at
/// step 4, and `RpcServer` — the thing that *accepts* on that listener — is
/// constructed afterwards, in `run`. So a connection lands in the backlog
/// immediately but **no request is answered until after the restore**, and
/// the first answered `shep flock` is the same milestone, one step later.
/// Do not "simplify" the poll away on the assumption that it is a guess.
pub(crate) fn openrc_script(spec: &UnitSpec) -> String { … }
```

The rendered text:

```sh
#!/sbin/openrc-run
# shep process manager for <user>
#
# openrc has no sd_notify analogue, so the readiness gap systemd's
# Type=notify closes is closed here by start_post asking the shepherd
# itself. The first answered request proves the muster restore finished:
# shep binds its control socket before the restore but does not accept on
# it until after.

name="shep"
description="shep process manager for <user>"
supervisor="supervise-daemon"
command="<exec>"
command_args="daemon --foreground"
command_user="<user>"
directory="<working_dir>"
pidfile="/run/shep-<user>.pid"
respawn_delay=5
output_log="<home>/logs/shepd.out.log"
error_log="<home>/logs/shepd.err.log"

export SHEP_HOME="<home>"
export PATH="<path>"

depend() {
	need net
}

start_post() {
	local waited=0
	while [ "${waited}" -lt 60 ]; do
		if "<exec>" --home "<home>" flock >/dev/null 2>&1; then
			return 0
		fi
		sleep 1
		waited=$((waited + 1))
	done
	eerror "shep did not answer on its control socket within 60s"
	return 1
}
```

`start_post` runs as root, and the socket lives in a 0700 `$SHEP_HOME` owned
by the target user — root bypasses that, so the poll works. Say so in a
comment inside the renderer, because it looks like a permission bug to a
reader who has not thought it through.

**Shell-quoting.** `<user>`, `<home>`, `<path>` and `<exec>` land inside
double quotes. A value containing `"`, `$`, `` ` `` or `\` breaks out —
exactly the class of bug `systemd_environment_value` and `xml_text` already
handle for the other two renderers. Add `sh_double_quoted(value: &str) ->
String` escaping those four with a backslash, use it on every interpolation,
and pin it with a test built from a path containing all four characters. This
is the openrc equivalent of the `%` and `&` escapes already in the file, and
it is the reviewer's first question.

### Step 7.3 — GREEN: install and remove

In `mod.rs`, the `Init::Openrc` arms:

```rust
        Init::Openrc => {
            steps.push(run_step("rc-update", &["add", &unit_file_name(plan), "default"]));
            steps.push(run_step("rc-service", &[&unit_file_name(plan), "start"]));
        }
```

and, in `remove`:

```rust
        Init::Openrc => {
            steps.push(run_step("rc-service", &[&unit_file_name(plan), "stop"]));
            steps.push(run_step("rc-update", &["del", &unit_file_name(plan), "default"]));
            steps.push(remove_unit(plan));
        }
```

`run_step` never short-circuits (its own doc says so), so a `stop` on a
service that was not running is a failed row rather than an aborted removal —
matching how systemd's `disable --now` already behaves here.

Path: `openrc_script_path(user) -> PathBuf` = `/etc/init.d/shep-<user>`.

### Step 7.4 — tests

Exact-string, matching the shape `unit.rs`'s existing systemd and launchd
tests use. Four:

```rust
    /// fails if the readiness poll is ever dropped or unbounded. openrc's
    /// only honest answer to Type=notify.
    #[test]
    fn the_openrc_script_polls_for_readiness_and_bounds_the_wait() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains("start_post()"));
        assert!(rendered.contains("-lt 60"), "the poll must be bounded");
        assert!(rendered.contains("flock >/dev/null"));
        assert!(rendered.contains("return 1"), "a timeout must fail the service");
    }

    /// fails if the comment explaining WHY the poll is equivalent to
    /// READY=1 is deleted. Phase 12a shipped two false captions in a
    /// generated artefact because only one of them was pinned; generated
    /// prose that makes a claim gets a test.
    #[test]
    fn the_openrc_script_says_why_it_polls() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains("openrc has no sd_notify analogue"));
        assert!(rendered.contains("binds its control socket before the restore"));
    }

    /// fails if a metacharacter in a path escapes the double quotes.
    #[test]
    fn the_openrc_script_quotes_shell_metacharacters() {
        let mut s = spec();
        s.home = PathBuf::from(r#"/tmp/we"ird/$HOME/`x`/back\slash"#);
        let rendered = openrc_script(&s);
        assert!(rendered.contains(r#"\"eird"#), "a quote must be escaped: {rendered}");
        assert!(rendered.contains(r"\$HOME"), "a dollar must be escaped: {rendered}");
        assert!(rendered.contains(r"\`x\`"), "a backtick must be escaped: {rendered}");
    }

    #[test]
    fn the_openrc_script_is_the_same_entry_point_as_the_other_two() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains(r#"command_args="daemon --foreground""#));
    }
```

### Step 7.5 — verify

```bash
grep -rni "openrc" crates | wc -l    # up sharply; and the two "deferred" comments GONE:
grep -rn "openrc/rc.d are named as deferred" crates | wc -l              # 1 -> 0
grep -rn "openrc and the BSD rc.d scripts" crates/shep-cli/src | wc -l   # 1 -> 0
cargo test -p shep-cli --bins --all-features   # +4
```

The CHANGELOG hits stay; those are a historical record of a past release and
are not claims about the present tree.

### Step 7.6 — MUTATION

Delete the `start_post` block from the rendered string. Expected: both
readiness tests fail. Revert.

**Second mutation:** drop `sh_double_quoted` from the `home` interpolation.
Expected: `the_openrc_script_quotes_shell_metacharacters` fails. If it passes,
the fixture path does not actually contain the characters the test claims —
check the raw string literal, this is exactly the "pattern cannot match the
real text" shape.

### Step 7.7 — gate

---

## Task 8 — FreeBSD and OpenBSD `rc.d` renderers

**Files:** `crates/shep-cli/src/commands/startup/unit.rs`,
`crates/shep-cli/src/commands/startup/mod.rs`.

**Read decision 11 before starting.** Two renderers, not one; the username
refusal; and OpenBSD deliberately does not poll.

### Step 8.1 — baseline

```bash
grep -rn "rc.subr" crates | wc -l    # 0
grep -rn "rcvar" crates | wc -l      # 0
```

### Step 8.2 — the username refusal, first

This is the highest-value part of the task and it comes first so the
renderers can assume a valid name.

```rust
/// Whether `user` can appear in a BSD rc script's variable names.
///
/// `rcvar` and `rcctl` turn the service name into **shell variable names**
/// (`shep_<user>_enable`, `shep_<user>_flags`). A username containing `-` or
/// `.` — `web-app` and `deploy.svc` are both legal on both systems —
/// produces `shep_web-app_enable`, which is not a valid `sh` variable, and
/// the script then fails at `load_rc_config` with a syntax error naming a
/// line number rather than a user.
///
/// systemd and openrc name *files*, not variables, and are unaffected. Do
/// not add this check there.
pub(crate) fn is_rc_safe_user(user: &str) -> bool {
    let mut chars = user.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
```

In `plan`, after `target_user`:

```rust
    if matches!(init, Init::FreebsdRc | Init::OpenbsdRc) && !is_rc_safe_user(&user) {
        return Err(Refusal {
            code: ExitCode::Usage,
            message: format!(
                "a BSD rc.d script turns the user name into a shell variable, so {user} \
                 cannot be used: it must start with a letter or underscore and contain \
                 only letters, digits and underscores. Pass --user with a name that does."
            ),
        });
    }
```

Test it against the names that actually occur:

```rust
    #[test]
    fn a_user_name_that_cannot_be_a_shell_variable_is_refused() {
        for ok in ["deploy", "www", "_shep", "app2"] {
            assert!(is_rc_safe_user(ok), "{ok} should be accepted");
        }
        for bad in ["web-app", "deploy.svc", "2fast", "", "ünicode"] {
            assert!(!is_rc_safe_user(bad), "{bad} should be refused");
        }
    }
```

### Step 8.3 — GREEN: FreeBSD

`/usr/local/etc/rc.d/shep_<user>`, `name="shep_<user>"`,
`rcvar="shep_<user>_enable"`.

```sh
#!/bin/sh
#
# PROVIDE: shep_<user>
# REQUIRE: LOGIN NETWORKING
# KEYWORD: shutdown
#
# Enable with: sysrc shep_<user>_enable=YES

. /etc/rc.subr

name="shep_<user>"
rcvar="shep_<user>_enable"
: ${shep_<user>_enable:="NO"}

shep_<user>_user="<user>"
shep_<user>_chdir="<working_dir>"
shep_<user>_env="SHEP_HOME=<home> PATH=<path>"

pidfile="/var/run/shep_<user>.pid"
command="/usr/sbin/daemon"
command_args="-P ${pidfile} -r -f <exec> daemon --foreground"

start_postcmd="shep_<user>_poststart"

# rc.subr reports the service started as soon as daemon(8) has forked, which
# is before the shepherd has finished restoring the muster roll. This waits
# for the shepherd to answer on its own control socket, which is the same
# milestone systemd's READY=1 reports.
shep_<user>_poststart()
{
	_waited=0
	while [ ${_waited} -lt 60 ]; do
		if <exec> --home <home> flock >/dev/null 2>&1; then
			return 0
		fi
		sleep 1
		_waited=$((_waited + 1))
	done
	echo "shep did not answer on its control socket within 60s" >&2
	return 1
}

load_rc_config $name
run_rc_command "$1"
```

Install: `sysrc shep_<user>_enable=YES` then `service shep_<user> start`.
Remove: `service shep_<user> stop`, `sysrc -x shep_<user>_enable`, remove file.

### Step 8.4 — GREEN: OpenBSD

`/etc/rc.d/shep_<user>`.

```sh
#!/bin/ksh
#
# shep process manager for <user>
#
# Enable with: rcctl enable shep_<user> && rcctl start shep_<user>
#
# OpenBSD's rc.subr has no post-start hook: rc_pre runs before the daemon
# starts and rc_post runs after it stops. So this script reports the service
# started as soon as the shepherd process is spawned, which is BEFORE the
# muster restore has finished — the flock may still be coming back. There is
# no readiness protocol here and this script does not pretend to one. Check
# with: shep --home <home> flock

daemon="<exec>"
daemon_flags="daemon --foreground"
daemon_user="<user>"
daemon_execdir="<working_dir>"

. /etc/rc.d/rc.subr

rc_bg=YES
rc_reload=NO

rc_start() {
	${rcexec} "SHEP_HOME=<home> PATH=<path> ${daemon} ${daemon_flags}"
}

rc_cmd $1
```

Install: `rcctl enable shep_<user>` then `rcctl start shep_<user>`.
Remove: `rcctl stop shep_<user>`, `rcctl disable shep_<user>`, remove file.

**Verify the framework vocabulary before you write either renderer.** This
plan states `start_postcmd` for FreeBSD's `rc.subr` and `rc_pre`/`rc_post`
plus the absence of a post-start hook for OpenBSD's, from memory, on a machine
that runs neither. Check `rc.subr(8)` for each system — the online manual
pages are authoritative — and **if a fact here is wrong, fix the script and
say so in the task report** rather than shipping what this plan guessed. The
one thing that is not negotiable is the honesty rule: if OpenBSD turns out to
have a usable post-start hook, use it and delete the comment; if it does not,
the comment stays exactly as written.

### Step 8.5 — tests

Exact-string, same tier as the others. The load-bearing ones:

```rust
    /// fails if the FreeBSD rcvar stops matching the script name — the two
    /// have to agree or `sysrc shep_<user>_enable=YES` sets a variable
    /// nothing reads, and the service silently never starts at boot.
    #[test]
    fn the_freebsd_rcvar_matches_the_script_name() {
        let rendered = freebsd_rc_script(&spec());
        assert!(rendered.contains(r#"name="shep_deploy""#));
        assert!(rendered.contains(r#"rcvar="shep_deploy_enable""#));
        assert!(rendered.contains("PROVIDE: shep_deploy"));
        assert_eq!(freebsd_rc_path("deploy"),
                   PathBuf::from("/usr/local/etc/rc.d/shep_deploy"));
    }

    /// fails if the OpenBSD script grows a readiness claim it cannot back.
    /// Decision 11: OpenBSD's rc.subr has no post-start hook, the script says
    /// so plainly, and this is what stops that sentence being "tidied away".
    #[test]
    fn the_openbsd_script_admits_it_has_no_readiness_gate() {
        let rendered = openbsd_rc_script(&spec());
        assert!(rendered.contains("no post-start hook"));
        assert!(rendered.contains("BEFORE the"));
        assert!(!rendered.contains("start_post"), "OpenBSD has no such hook");
        assert!(!rendered.contains("READY=1"), "that is systemd's, not this");
    }

    /// fails if either BSD script forgets SHEP_HOME — a shepherd started
    /// without it uses root's ~/.shep and restores nothing, silently, and
    /// the operator finds out at the next reboot.
    #[test]
    fn both_bsd_scripts_carry_shep_home_and_path() {
        for rendered in [freebsd_rc_script(&spec()), openbsd_rc_script(&spec())] {
            assert!(rendered.contains("SHEP_HOME="), "{rendered}");
            assert!(rendered.contains("PATH="), "{rendered}");
        }
    }
```

### Step 8.6 — verify

```bash
grep -rn "rc.subr" crates | wc -l      # 0 -> 2 or more
cargo test -p shep-cli --bins --all-features    # +6
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

### Step 8.7 — MUTATION

Change `name="shep_<user>"` to `name="shep"` in the FreeBSD renderer while
leaving `rcvar` alone. Expected: `the_freebsd_rcvar_matches_the_script_name`
fails. This is the mutation that matters — it is the exact bug a copy-paste
from a single-instance rc script produces, and its symptom in the field is a
service that installs cleanly and never starts at boot. Revert.

**Second mutation:** delete the "no post-start hook" paragraph from the
OpenBSD script. Expected: `the_openbsd_script_admits_it_has_no_readiness_gate`
fails. Revert.

### Step 8.8 — gate

---

## Task 9 — docs, ledger, changelogs

Last, because it reconciles claims the other eight tasks make. Every edit here
is a **claim about the tree**, so every one of them gets a grep that could
have failed.

### Step 9.1 — baseline

```bash
grep -c "the directory does not exist" docs/specs/deferred.md   # 1
grep -c "ecosystem.config.js" docs/migration.md                 # 2
grep -c "openrc and BSD rc.d remain open" docs/specs/deferred.md # 1
grep -c "Deferred because making the fields private" docs/specs/deferred.md  # 1
grep -c "^## \[Unreleased\]" crates/shep-core/CHANGELOG.md      # 1
grep -c "^## \[Unreleased\]" crates/shep-cli/CHANGELOG.md       # 1
```

The `Deferred because making the fields private` string is what Step 9.8
checks disappeared. `is not a proof token` is **not** usable for that: it is
the section heading, it is `1` today, and it stays `1` after the section is
rewritten into a resolution — a check that cannot fail. That mistake was in
this plan's own first draft.

### Step 9.2 — `docs/specs/deferred.md`

Four entries move from "not yet built" to "Not deferred", and one known-debt
entry is resolved:

1. **`.js` Flockfile** — becomes shipped, with the ruling: explicit only,
   never by discovery, never by extension alone; reads a Flockfile-shaped
   `.js`, not a pm2 ecosystem file; `shep import` is still the pm2 path.
2. **schemars** — becomes shipped. **Delete "(the directory does not exist)"**;
   `assets/` has existed since the metrics dog. Name
   `assets/flockfile.schema.json`, `shep schema`, and the `include_str!`
   drift guard.
3. **Daemon-config flags layer** — becomes shipped, naming the four flags and
   the validate-once rule.
4. **openrc and BSD rc.d units** — becomes shipped, with three caveats stated
   rather than buried: runtime detection changes Linux behaviour in a
   container; `--init` is the override; **none of the three new scripts has
   been executed on its own operating system**.
5. **`DaemonConfig` is not a proof token** — moves from open question to
   resolution. Replace the entry with decision 7's ruling and its reasoning,
   including the sentence that nothing guards the attribute itself.

And two **new** known-debt entries, both honest admissions this phase creates:

- **A `.js` Flockfile has no evaluation timeout.** A module that never returns
  hangs `shep start`. Not built because a bound means a reaper thread in a
  crate that forbids unsafe code, for a case where the process is in the
  foreground and interruptible. What would force it: any path that evaluates a
  `.js` Flockfile unattended.
- **The missing-node error message has no test.** Producing it needs a `PATH`
  without node, and `std::env::set_var` is unsafe in edition 2024 in a crate
  that forbids unsafe. The sentence is pinned in `docs/migration.md` instead.

### Step 9.3 — `docs/specs/shep-v1.md`

§5's Flockfile paragraph currently implies `.js` is reachable like any other
format. Amend it in the style §13 and §9 already use — state the change and
the reason, so a later reader does not "restore" it:

> **Amended, Phase 14 (Rin's ruling).** `.js` is read only when named
> explicitly with `shep start <path> --flockfile`. Directory discovery never
> selects a `.js` file and the ten-name order is unchanged, because reading
> one runs `node` on it: entering a cloned repository and running `shep start`
> must not execute code that repository contains. The document it reads is
> Flockfile-shaped (`app`, sheep-native field names), not a pm2
> `ecosystem.config.js`; `shep import` remains the pm2 path.

§5's layering sentence and §11's init list both become true statements about
shipped code — check each still says something accurate rather than adding a
second claim beside it.

### Step 9.4 — `docs/migration.md`

Line 8 currently says `shep import` "does not read `ecosystem.config.js` or
any other pm2 config format". **That stays true and must not be softened.**
Add one short section after it, and this is the place the missing-node
sentence gets pinned:

> ### If your config is a `.js` file
>
> `shep start <path> --flockfile` reads a `.js` file by running it through
> `node`, so a config you generate rather than write out longhand still
> works. It has to export the Flockfile shape — an `app` array with
> sheep-native field names — not pm2's. Point it at a real
> `ecosystem.config.js` and shep refuses, naming the key it found and the key
> it wanted.
>
> Without the `--flockfile` flag, `shep start server.js` still means what it
> has always meant: start `server.js`. And if node is not installed, shep
> says so and tells you the alternative: *reading a .js Flockfile runs it
> through node, and node was not found on PATH; install node, or convert the
> file to a .toml Flockfile.*

Then pin it, since it is the one sentence in this phase with no unit test:

```bash
grep -c "node was not found on PATH" docs/migration.md                          # 1
grep -c "node was not found on PATH" crates/shep-cli/src/commands/lifecycle.rs  # 1
```

Two files, one sentence, and a doc that drifts from the code shows up as a
count of 1 and 0. Add both greps to this task's verification block.

### Step 9.5 — `docs/releasing.md`

One paragraph: the three new init scripts are rendered and pinned by
exact-string tests, and have not been executed on FreeBSD, OpenBSD, or an
openrc host. Nothing claims support for those platforms until somebody reports
back from one.

### Step 9.6 — CHANGELOGs

`crates/shep-core/CHANGELOG.md` under `## [Unreleased]`:

- `DaemonConfig::load_layered` and `DaemonOverrides` — the `file < env < flags`
  layer, validated once at the end so a flag can rescue a lower layer.
- `DaemonConfig` is `#[non_exhaustive]`: outside shep-core it can only be
  obtained from `load`/`load_layered`/`Default`. **A breaking change for any
  out-of-tree struct-literal construction** — say so plainly under a
  `### Changed` heading, not buried in an addition.
- `config::parse_bool_value` — the shared `1|0|true|false` grammar.
- `schema` feature: `JsonSchema` for `AppConfig`, `ProbeConfig`, `ProbeKind`,
  `MemSize`, `UpDuration`. Off by default.
- `$schema` accepted and ignored at the Flockfile top level (if Task 3
  shipped; delete this line if it was cut).

`crates/shep-cli/CHANGELOG.md`:

- `shep start --flockfile`, and `.js` Flockfiles through node. Note explicitly
  that `shep start server.js` is unchanged.
- Hidden `shep schema`; `assets/flockfile.schema.json`.
- `shep daemon` takes `--log-json`, `--log-level`, `--socket`,
  `--max-cron-sleep`.
- `shep startup`/`unstartup` take `--init`; init detection on Linux is now a
  runtime probe. Under `### Changed`: **a Linux host with no
  `/run/systemd/system` is now refused instead of being written a systemd
  unit** — the container case — and `--init systemd` restores the old
  behaviour.
- openrc, FreeBSD and OpenBSD renderers, with the "not executed on its own
  operating system" caveat.

Also correct `crates/shep-cli/CHANGELOG.md`'s existing lines only if they are
wrong about the *past*; a historical entry describing a past state stays.

### Step 9.7 — CLAUDE.md

The Status section names phases through 13. Add Phase 14 in the same register:
the four config-and-packaging items, and what `.js` refuses.

### Step 9.8 — verify

```bash
grep -c "the directory does not exist" docs/specs/deferred.md        # 1 -> 0
grep -c "openrc and BSD rc.d remain open" docs/specs/deferred.md     # 1 -> 0
grep -c "node was not found on PATH" docs/migration.md               # 0 -> 1
grep -c "node was not found on PATH" crates/shep-cli/src/commands/lifecycle.rs  # 1
grep -c "ecosystem.config.js" docs/migration.md                      # 2 -> 3
grep -c "Deferred because making the fields private" docs/specs/deferred.md  # 1 -> 0
```

### Step 9.9 — phase gate

The four task-gate commands, plus:

```bash
cargo test --workspace --all-features -- --test-threads=1
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The serial run is not ceremony — it was red on `main` before Phase 5 and
caught a real regression in Phase 6. Both benches gates too, per CLAUDE.md's
phase gate.

---

## What this phase deliberately does not build

Say these out loud in the phase report rather than leaving them as silence:

- **No pm2 `ecosystem.config.js` reader.** Decision 2. `shep import` stays the
  pm2 path, and it reads `~/.pm2/dump.pm2`.
- **No `.js` evaluation timeout.** Decision 3, recorded as debt.
- **No new `ExitCode` variant.** Decision 3.
- **No `FlockFormat::Js` and no fifth `FlockfileError` variant.** Decision 4 —
  shep-core never spawns a process.
- **No private fields on `DaemonConfig`.** Decision 7.
- **No `trybuild` compile-fail tier** to observe `#[non_exhaustive]`. Phase 10
  declined it for `ProcessInfo` and this phase declines it for the same reason;
  the gap is stated, not papered over.
- **No second `tests/` file in shep-core.** Its one file says it must stay the
  only one.
- **No NetBSD or DragonFly rc.d.** Spec §11 names four init systems and these
  are not among them.
- **No CI runner for openrc or the BSDs.** The scripts are text with
  exact-string tests, which is the same tier the systemd unit has always had
  on a Mac, and no doc claims more.
