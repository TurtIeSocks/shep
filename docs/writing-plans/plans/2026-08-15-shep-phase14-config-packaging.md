# Phase 14 — config and packaging

The four config-and-packaging items spec §5 and §11 name as v1.0 and that
`docs/specs/deferred.md` still lists as unbuilt: `.js` Flockfiles, the
schemars JSON-schema export, the daemon-config **flags** layer, and openrc +
BSD `rc.d` startup units.

Revised 2026-08-15 after an adversarial review found 25 problems, four of
them Critical. Three of the four were failures of *reasoning*, not of
measurement, and the repairs are written up where the reasoning lives rather
than patched at the point of use:

- **The `#[non_exhaustive]` proof-token ruling was false as argued** and is
  re-derived from scratch in decision 7, to a different and smaller
  conclusion.
- **The committed schema described one app while being named and wired as the
  schema for a document.** Decision 5 now generates the document schema, from
  the parser's own type.
- **`include_str!` reaching out of the package root would have broken
  `cargo publish`.** Decision 5 moves the artefact inside a package and moves
  the guard with it.
- **`Init` on `StartupArgs` would have broken the Windows cross-check.**
  Decision 10 puts the type where `cli.rs` can name it on every target.

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
13 (whistle) was in flight while this was written and landed commits under the
review that produced this revision — one of the original plan's sixteen
baselines went stale mid-review, in exactly that way. The tip moves under you.

Establish your own baseline before Task 1. Ancestry, not equality:

```bash
git log --oneline -1 --format=%H -- docs/specs/deferred.md   # record it
git merge-base --is-ancestor "$(git log -1 --format=%H -- docs/writing-plans/plans/2026-08-15-shep-phase14-config-packaging.md)" HEAD; echo "ancestor=$?"
```

`ancestor=0` means the commit that wrote this plan is in your history and the
greps below were derived against a tree yours descends from. `ancestor=1`
means someone rebased: **re-derive every grep in this document before trusting
one of them**, and say so in the task report.

```bash
cargo test --workspace --all-features
```

Write down what it prints. That is your baseline; every task states a delta
against it as an *enumeration of named tests*, not as a number to check off,
and `failed` must stay `0` on every result line the whole way. At the time of
writing the whole-workspace run was around 1200 passing across 17 result
lines. Treat that as a shape. Two earlier briefs on this project shipped a
stale figure as if it were a checksum.

### Baselines, re-derived at revision time

All re-run against the working tree on this machine, all exactly as printed.
Where the original plan cited a `file:line`, this revision cites a **grep
anchor** instead — Phase 13 moved `daemon.rs` under the last review, and every
line number in the first draft that was checked had drifted.

```bash
grep -c "schemars" crates/shep-core/Cargo.toml                        # 0 (grep exits 1)
grep -c '^name = "schemars"$' Cargo.lock                              # 1
grep -rn --include="*.rs" "JsonSchema" crates/shep-core | wc -l       # 0
grep -c "^\[features\]" crates/shep-core/Cargo.toml                   # 0 (grep exits 1)
git ls-files crates/shep-core/assets | wc -l                          # 0
grep -c "the directory does not exist" docs/specs/deferred.md         # 1
grep -rn "JSON.stringify(require" crates | wc -l                      # 0
grep -c "Js," crates/shep-core/src/config/flockfile.rs                # 0 (grep exits 1)
grep -rn "rc.subr" crates | wc -l                                     # 0
grep -rni "openrc" crates | wc -l                                     # 4
grep -c "fn validate" crates/shep-core/src/config/daemon.rs           # 0 (grep exits 1)
grep -c "^#\[non_exhaustive\]" crates/shep-core/src/config/daemon.rs  # 1
grep -c "allow(dead_code)" crates/shep-cli/src/commands/startup/unit.rs   # 2
grep -c "a fifth backend" crates/shep-core/src/config/flockfile.rs        # 1
grep -rn "openrc and the BSD rc.d scripts" crates/shep-cli/src | wc -l    # 1
grep -rn "openrc/rc.d are named as deferred" crates | wc -l               # 1
grep -c "UNIT_MODE" crates/shep-cli/src/commands/startup/mod.rs           # 4
```

**Three of these carry a scope that is load-bearing. Do not drop it.**

`^#\[non_exhaustive\]` anchored to column 0: the unanchored pattern prints `2`
today, because `DaemonConfigError`'s doc comment quotes the attribute in its
own rationale, and every rationale this phase adds would inflate it further.
Anchored, it counts attributes and only attributes: `1` now, `3` after Task 4.

`crates/shep-core` on the `JsonSchema` grep. **The original plan scoped this to
`crates` and expected `0`; that is now wrong and is the one baseline that went
stale during review.** Phase 13 landed `crates/shep-cli/src/whistle/facts.rs`,
which does `use schemars::JsonSchema;` and derives it on five payload twins,
so the workspace-scoped count is `12` and is not a signal for this phase. The
question Task 2 asks is whether *shep-core* derives it, which is `0` now and
non-zero after Task 2. The original plan also carried a paragraph explaining
that `--include="*.rs"` was what made the number `0` rather than `1` (a comment
in `crates/shep-cli/Cargo.toml` mentions the derive); that paragraph is
obsolete under the new scope and has been deleted rather than corrected.

`crates/shep-cli/src` on the openrc grep. Unscoped it prints `2`; the second
hit is a line in `crates/shep-cli/CHANGELOG.md`, a historical record of a past
release that must **not** be deleted. Scoping to `src` is what makes the check
mean "the code no longer claims openrc is missing" rather than "the project has
never mentioned openrc".

`grep -c '^name = "schemars"$' Cargo.lock` replaces the original plan's
`grep -c "schemars" Cargo.lock  # 5`. **That check could not pass.** The
unanchored count is 5 today — one dependency entry under `rmcp`, the
`name = "schemars"` package, schemars' own dep on `schemars_derive`, the
`name = "schemars_derive"` package, and one dependency entry under `shep-cli`
— and Task 2 adds a sixth by construction, because shep-core's own dependency
list gains a `"schemars",` line. The plan then told the implementer to stop and
reconcile a version conflict that does not exist. The invariant actually meant
is "exactly one schemars *package* is resolved", which is the anchored form:
`1` before, `1` after. Record the unanchored delta (`5 → 6`) as an observation,
not as a gate.

The `grep -rni "openrc" crates` baseline of **4** is still the one to watch:
two of those four are CHANGELOG lines and two are `// openrc is deferred`
comments that Task 6 must delete. A count still at 4 after Task 6 means the
comment claiming openrc does not exist survived the change that made it exist.

Note the several greps that exit `1`. `grep -c` printing `0` and exiting
non-zero is fine at a prompt and **fatal in a `set -e` script**, which is how
one of the dead checks in an earlier phase came to be dead. If you script
these, append `|| true`.

#### Three shapes a dead check takes, all found in earlier plans — and the two the review found in this one

1. **The pattern that cannot match the real text.** Source text with
   backticks, or wrapped across a line break, defeats a naive grep. Before
   writing a grep, `grep -n` the surrounding words and read what is actually
   there.
2. **The expectation already true at HEAD.** If the baseline command prints
   the post-change value, the check verifies nothing. Every baseline above is
   printed for this reason. The review found two of these in the original
   Task 7 — both of its `1 → 0` greps are satisfied by Task 6, so they are
   moved to Task 6's verification block below and re-baselined at `0` in
   Task 7.
3. **The bound that is not a bound.** `tokio::time::timeout` around a
   synchronous call bounds nothing; nor does a harness process timeout, which
   fails the whole binary and names no test.
4. **The check that must false-alarm.** A count whose expected value is
   arithmetically impossible after the change it guards — the `Cargo.lock`
   grep above. The implementer stops on a phantom, and having stopped once on
   a phantom, stops trusting the next one.
5. **The mutation that changes nothing.** Two match arms that do the same
   thing for the inputs the tests use; swapping them is not a mutation. The
   review found one of these guarding this phase's headline security decision
   — see Task 1's mutation block, rewritten.

---

## What this phase adds beyond spec, and why

Four additions the spec does not name. Each is argued in full below; they are
collected here because the "what this phase deliberately does not build"
section at the end lists only omissions, and a reader deserves the additions
in the same place.

- **`shep start --flockfile`** — spec §5 names `.js` config via node but not a
  flag to reach it. Rin's ruling (never implicitly) plus the `server.js`
  collision force one; decision 1.
- **A hidden `shep schema` verb** — spec §5 says the schema "ships in assets"
  and does not say how it gets there. A verb is how the artefact is
  regenerated, and it hands the schema to someone who has the binary and not
  the repo; decision 5.
- **`$schema` as a recognised, ignored key** — appears nowhere in the spec.
  **Severable**: cutting it costs JSON/JSON5 editor completion and nothing
  else. It is Task 3, standing alone; decision 6.
- **`shep startup --init`** — spec §11 names four init systems and does not
  ask for an operator override. Decision 10's own behaviour change (a Linux
  container with no `/run/systemd/system` is now refused) is what requires
  one; it is also the only way this machine renders a systemd or rc.d script
  at all.

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
  the same attribute to a `pub` **struct**, where it buys something much
  narrower than the original draft of this plan claimed — read decision 7.
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

Task 1 has one extra form, because three of its tests need a second runtime
and would otherwise skip themselves into silence:

```bash
SHEP_REQUIRE_NODE=1 cargo test -p shep-cli --bins --all-features
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

The Windows one needs `brew install mingw-w64`. **It matters more than usual
this phase, and the original draft of this plan would have broken it.**
`crates/shep-cli/src/cli.rs` carries no `cfg` at all — its only `#[cfg]`
occurrences are a doc note and its own `#[cfg(test)]` module — while
`mod commands` in `main.rs` is `#[cfg(unix)]`. Any type `cli.rs` names must
therefore exist on Windows. Decision 10 is where that constraint is discharged;
Task 6's step 6.2 restates it at the point of use.

Two second-order effects of this phase on that check, both expected, both to be
**measured and recorded rather than assumed**:

- Task 2 makes `schemars` an optional dependency of shep-core. The
  workspace-wide check runs `--all-features`, so the `schema` feature is on and
  schemars compiles for Windows even though `shep-cli`'s own `schemars` sits in
  a `[target.'cfg(unix)'.dependencies]` table precisely to keep it out. Nothing
  in schemars' tree builds C or needs cmake, so this costs time, not
  correctness. Record the wall-clock before and after Task 2. The last recorded
  green run of this check was 8.42s.
- Task 6 moves `Init` into `cli.rs`, which is what makes the check pass at all.

**A gap this plan cannot close, stated rather than papered over.** Task 6
rewrites `current_init`'s `cfg` arms, and the Linux arm lives in **shep-cli**.
The Linux cross-check in the gate above is `-p shep-daemon`, deliberately:
shep-cli's tree carries `ring`, whose build script runs `cc`, so
`cargo check -p shep-cli --target x86_64-unknown-linux-gnu` needs a Linux cross
C toolchain this machine does not have. Attempt it anyway and report what
happens; if it fails inside `ring`'s build script, that is the known toolchain
gap, not a defect in the change. Task 6 is written so that the Linux arm's
*logic* does not depend on that check — the ordering lives in a pure function
with its own tests, and the `cfg` arm is two lines that call it.

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

The ten-name `DISCOVERY_ORDER` in `crates/shep-core/src/config/flockfile.rs`
therefore **stays exactly as it is** (find it with
`grep -n "const DISCOVERY_ORDER" crates/shep-core/src/config/flockfile.rs` —
the first draft of this plan cited a line range that was already ten lines
out). Task 1 adds a test that pins it at ten names and pins that none of them
ends in `.js`.

That much is Rin's. What her ruling does not settle, and what this plan must:
"named explicitly on the command line" is ambiguous, and the obvious reading
of it is **wrong**.

The obvious reading is "make `FlockFormat::from_path` recognise `.js`", so
that `shep start ecosystem.config.js` routes to the node bridge. That breaks
the single most common thing anyone types at this program:

```rust
// crates/shep-cli/src/commands/lifecycle.rs, today, passing.
// Find it with: grep -n "any_other_existing_path_becomes_one_minimal_app" \
//   crates/shep-cli/src/commands/lifecycle.rs
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
is the property Task 1's mutations check.

**Where the flag does NOT go:** not on `restart`, not on `reload`, not on
`import`. `start` is the only verb that takes a target.

### 2. What `.js` actually reads — and it is not a pm2 ecosystem file

This is the part that will be got wrong if it is not written down.

The framing "so a pm2 user's `ecosystem.config.js` can be read" is the
aspiration. It is not what this builds, and it cannot be, for a reason you can
check in thirty seconds:

- The Flockfile document key is `app`. pm2's ecosystem key is `apps`.
- `RawFlockfile` is `#[serde(deny_unknown_fields)]`, deliberately, so a
  typo'd key fails loudly. The forward-compat comment directly above it says
  so in its own words.
- `AppConfig` is `#[serde(deny_unknown_fields, default)]` and its field names
  are sheep-native. Spec §5: "sheep-native names, no pm2 aliases".
  `exec_mode`, `max_memory_restart`, `error_file`, `env_production` are all
  unknown fields.

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

(After Task 3, serde's expected-field list grows a second entry and the
message becomes `expected one of \`$schema\`, \`app\``. The test asserts on
the presence of `apps` and `app`, not on the whole sentence, so it survives
either way — but read the real message once and confirm.)

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

A fifth case, not a node failure at all: `--flockfile` on a path whose
extension names no readable format is `Usage` (2), because the command line is
the thing that is wrong.

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

### 5. schemars: derive on the parser's own document type, generate through a hidden verb, commit the artefact INSIDE a package, catch drift with `include_str!`

The situation changed since `deferred.md` was written. Verified against the
tree:

- `Cargo.lock` carries exactly one `schemars`, **1.2.2**, pulled in by
  `rmcp`'s server feature for whistle.
- The root `Cargo.toml` declares it as a workspace dependency
  (`schemars = { version = "1.2.2", default-features = false, features =
  ["derive", "std"] }`), pinned exactly, with a comment explaining that the
  derive expands to absolute `schemars::` paths so the crate has to be
  nameable rather than reached through `rmcp::schemars`.
- `crates/shep-cli/Cargo.toml` has `schemars.workspace = true` — **inside its
  `[target.'cfg(unix)'.dependencies]` table**, beside `rmcp`, because
  `src/whistle/` is unix-only and the manifest comment says building an MCP
  stack into a Windows binary that cannot use it would slow the Windows
  cross-check.
- `crates/shep-core/Cargo.toml` has **no** `schemars` and no `[features]`
  table at all.

So this is now a derive decision, not a dependency decision — with one
exception: the types the schema describes live in shep-core, and shep-core
does not have the crate.

#### (a) What the schema describes: the DOCUMENT, not one app

The original draft made `flockfile_schema()` be `schema_for!(AppConfig)` and
committed the result as `flockfile.schema.json`. **That artefact would reject
every real Flockfile.** `schema_for!(AppConfig)` emits an object whose
properties are `name`, `script`, `args`, … with `required: ["name","script"]`
and, from `deny_unknown_fields`, `additionalProperties: false`. A Flockfile is
`{"$schema": "…", "app": [ … ]}`. Point an editor at the one and hand it the
other and every key is unknown and every required key is missing — the exact
inverse of the feature's purpose, and none of the draft's three tests could
have caught it, because all three asserted against an AppConfig-shaped tree.

Spec §5's "Schema = `AppConfig` in shep-core" names the *field set*; the same
sentence says the artefact is "for editor completion", and what an editor
completes is the document.

**Ruling: the schema is generated from `RawFlockfile` — the private type serde
actually deserializes the document into — and shep-core is what generates it.**

```rust
// crates/shep-core/src/config/flockfile.rs
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Flockfile"))]
#[serde(deny_unknown_fields)]
struct RawFlockfile { … }
```

Three things fall out of that choice, and they are the reason for it:

1. **There is no twin to drift.** A hand-written `FlockfileSchema` mirror of
   `RawFlockfile` would be a second declaration of the document grammar, and
   the first field added to one and not the other is a schema that lies. Here
   the schema is generated from the *only* declaration of that grammar — the
   one serde uses.
2. **Task 3 becomes a content change instead of a coherence problem.** Adding
   `$schema` to `RawFlockfile` puts `$schema` in the emitted schema
   automatically. Cut Task 3 and the schema simply does not list it, and does
   not accept it — still exactly agreeing with the parser.
3. `AppConfig` still carries the derive and still supplies every field
   description; it lands in `$defs` and the document's `app` array `$ref`s it.

`RawFlockfile` is private, which is fine: `schema_for!` is invoked *inside*
shep-core, and what crosses the crate boundary is a `String`.

#### (b) Where it lives: `crates/shep-core/assets/flockfile.schema.json`

The original draft put it at the repository root, reasoning that "it does not
ship in the crates.io tarball and does not need to". **That reasoning is
incompatible with the drift guard the same decision chose.** `include_str!`
makes the file a compile-time input; `cargo package` packs only files under
the package directory; and shep-cli IS published — `docs/releasing.md`,
written the night before this plan, carries `cargo publish --workspace`,
`cargo publish -p shep-cli` and `cargo install shep-cli`. A root-relative
`include_str!("../../../../assets/…")` compiles here and fails for everyone
who installs from crates.io, and the first anyone would learn of it is the
first publish.

So the artefact goes inside the package that owns the type it describes, and
the guard goes with it. Neither manifest declares `include`/`exclude`
(verified), so every tracked file under the package directory is packed — no
manifest change is needed beyond adding the file to git. Task 9 confirms with
`cargo publish --workspace --dry-run`, which is the check this defect breaks.

Root `assets/` is left alone: it holds `assets/grafana/`, for the metrics dog,
and `git ls-files assets` stays at 2.

#### (c) The feature gate, and what it actually buys

**Ruling: optional dependency on shep-core behind a non-default `schema`
feature; shep-cli turns it on.**

The honest accounting, corrected from the draft: **inside this workspace the
gate is inert.** Cargo unifies features across the dependency graph, so
`cargo build --workspace`, every `--all-features` gate command, and the
shipped `shep` binary all compile shep-core *with* schemars, and shep-daemon
links that shep-core. The draft's claim that shep-daemon "has no use for it"
and its single-digit-MB idle-RSS budget are not what this buys — a derive
costs compile time and binary size, not resident memory, and shep-daemon gets
the derive anyway in every build that matters.

What the gate genuinely buys: an **out-of-tree** consumer of the published
`shep-core` does not compile `schemars` plus its proc macro for a JSON Schema
they may not want, and a standalone `cargo build -p shep-daemon` does not
either. One line in shep-cli's manifest is the whole cost. State it that way
in the manifest comment; do not restate the RSS argument.

#### (d) How it is generated: a hidden `shep schema` verb

Prints the schema to stdout, hidden the way `daemon` and `dog` already are.
Regeneration is

```
cargo run -p shep-cli -- schema > crates/shep-core/assets/flockfile.schema.json
```

It also gives a user who has the binary but not the repo a way to get the
schema without cloning, which is a real benefit and the reason it is a verb
rather than test-only code.

**The rendering lives in shep-core, not in the verb.** shep-core exports

```rust
#[cfg(feature = "schema")]
pub fn flockfile_schema_json() -> String
```

which pretty-prints and appends the trailing newline, and `shep schema` writes
its return value verbatim. One renderer means the verb and the drift test
cannot disagree about whitespace — a mismatch that would otherwise show up as
a permanently red test nobody can regenerate away.

#### (e) How drift is caught: `include_str!` plus a co-located test, in shep-core

```rust
// crates/shep-core/src/config/flockfile.rs
#[cfg(feature = "schema")]
const COMMITTED: &str = include_str!("../../assets/flockfile.schema.json");
```

Path arithmetic: `crates/shep-core/src/config/` → `../` src → `../../`
shep-core. Two, not four, and the destination is inside the package.

`include_str!` makes the committed file a **compile-time input**. Delete it
and the crate does not build; edit `AppConfig` and the co-located test fails
with a diff and the exact regeneration command. A committed schema nobody
regenerates is a lie with a filename — this is the mechanism that makes it
impossible to keep the lie, because the check is not a CI job someone can
forget to add, it is `cargo test` on shep-core, which every task in every
phase already runs. Putting it in shep-core rather than shep-cli also means it
runs on every target rather than only where `mod commands` compiles.

**The rule the schema follows: it describes the DESERIALIZER, not the
normalizer.** `AppConfig::kill_signal` is `Option<String>`; `normalize` is
what refuses a name outside `SIGTERM|SIGINT|SIGQUIT|SIGUSR2`. The schema says
`"type": "string"` and stops there. Emitting an `enum` would make the schema
describe a validation step that happens in a different module at a different
time, and the moment those two diverge the schema is wrong in a way no test
can catch. One sentence in the module doc, and a test that pins `kill_signal`
as an unconstrained string.

**`MemSize` and `UpDuration` need hand-written `JsonSchema` impls.** Both are
newtypes with manual `Serialize`/`Deserialize` that go to and from *strings*.
`#[derive(JsonSchema)]` would describe the inner `u64` / `Duration` and be
flatly wrong. The impls emit `{"type": "string", "pattern": …}` with the
pattern lifted from each type's own `FromStr` doc — `^\d+(G|M|K)?$` for
`MemSize`, `^\d+(h|m|s)?$` for `UpDuration`. **Lift them from the doc comment,
do not retype them from this plan**, and pin each with a test that runs both
the pattern and `FromStr` over an accept list and a reject list. A pattern
that agrees with itself and disagrees with `FromStr` is the failure mode; the
reject list therefore has to contain a string that *would* be accepted by a
plausibly-wrong pattern, which the draft's did not — see Task 2's mutation.

`schema_name` returns **`std::borrow::Cow<'static, str>`**. The draft wrote
`alloc::borrow::Cow`, which does not resolve: shep-core has no
`extern crate alloc` anywhere in the workspace and `alloc` is not in the extern
prelude for a std crate in edition 2024. schemars' own documentation examples
use `std::borrow::Cow`, and the rest of shep-core uses `std::` paths for
`PathBuf` and `BTreeMap`; `core::` is reserved here for the paths shep-core
already uses it for (`core::fmt`, `core::error::Error`).

Both impls provide `schema_name`, so schemars places them in `$defs` and
`$ref`s them from the fields that use them. **Every assertion about the shape
of a field must therefore resolve `$ref` before asserting.** Task 2 supplies a
resolver helper and, more importantly, orders its steps so the artefact is
generated and read *before* any shape assertion is written.

One side effect to know about before it surprises you: schemars' derive reads
`///` doc comments into `description`. That is a feature — `AppConfig`'s field
docs become hover text in the operator's editor, which is the best return this
whole task has. It also means **editing a doc comment on `AppConfig` fails the
drift test** until the schema is regenerated. That is correct behaviour, it is
Task 2's sharpest mutation, and the test's failure message says so explicitly
so the next person does not think they broke something.

### 6. `$schema` becomes a recognised, ignored top-level key

A JSON or JSON5 Flockfile cannot carry `"$schema": "…"` today: `RawFlockfile`
denies unknown fields, so the one line every JSON-schema-aware editor looks
for is a hard parse error. Without it, the artefact Task 2 commits is usable
only by TOML users through taplo's `#:schema` comment directive — and the
comment is invisible to serde, so TOML already works.

`flockfile.rs`'s forward-compat comment anticipates exactly this:

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

One coupling to Task 2, and it is a feature rather than a snag: because the
schema is generated from `RawFlockfile` itself (decision 5), adding this field
**changes the committed schema**, and Task 2's drift test goes red until the
artefact is regenerated. Task 3 therefore ends with the regeneration command,
and that red test is the proof the guard works.

### 7. `DaemonConfig` and the proof-token question — the ruling, re-derived

`docs/specs/deferred.md` records the question and names this phase's flags
layer as the thing that forces it (find it with
`grep -n "is not a proof token" docs/specs/deferred.md`):

> `ResolvedApp` keeps its `config` private so that holding one proves it went
> through `normalize`. `DaemonConfig` does not: its `daemon` and `dog` fields
> are `pub`, and the one validation it performs — the `max_cron_sleep` floor —
> happens inline inside `DaemonConfig::load` rather than in a `validate` step a
> hand-built value would also have to pass. […] What would force it: any
> production path that assembles a `DaemonConfig` from something other than a
> file — the daemon-config flags layer, for instance.

**The first draft of this plan answered it wrongly, and the wrong answer was
scheduled to be written into a published library's rustdoc.** It claimed that
`#[non_exhaustive]` makes `load`/`load_layered`/`Default` the only ways to
obtain a `DaemonConfig` from another crate, and therefore that "holding a
`DaemonConfig` outside shep-core proves it was validated". That is false.
`#[non_exhaustive]` on a struct blocks struct literals and functional-update
syntax from outside the defining crate. **It does not block field mutation.**
`DaemonConfig` derives `Default`, its `daemon` field is `pub`, and
`DaemonSection` is a plain `pub struct` with `pub` fields and no attribute of
its own, so:

```rust
let mut cfg = shep_core::config::DaemonConfig::default();
cfg.daemon.max_cron_sleep = Some(UpDuration::from_millis(1));   // compiles
```

walks straight past the gate. No struct literal required. A false
architectural rationale is worse than an absent one, because it is what the
next reader trusts instead of re-deriving it.

So, from scratch.

**(a) What property is actually wanted?** `ResolvedApp` protects a property of
*travel*: the supervisor receives one from somewhere it cannot see and must be
able to trust that `normalize` ran. The question is whether `DaemonConfig` has
a consumer in that position. It does not. Every production site loads one and
consumes it within a few lines:

| site | what it does with it |
|---|---|
| `commands/daemon.rs`'s `run_daemon` | loads one, renders it straight into `BootOptions` |
| `shep-daemon`'s `dogs.rs` | loads one to read a single `[dog.<name>]` table |
| `shep-cli`'s `whistle/gate.rs` | loads one to read a single boolean |

(Re-derive that list at task time: `grep -rn "DaemonConfig::load" crates`.)

The daemon never holds a `DaemonConfig` — it holds a `BootOptions`. Nothing
receives one from elsewhere and has to trust it. **The property `ResolvedApp`
needs is a property `DaemonConfig` has no consumer for.**

**(b) What would it cost to have it anyway?** Two ways to get it. Privatising
the fields means accessors for `log_json`, `log_level`, `socket`,
`enabled_dogs`, `adopted_dogs`, `max_cron_sleep`, `whistle.allow_control` and
`dog` — the last a `BTreeMap<String, toml::Table>` legitimately read across two
crates. Thirty-odd getters to guard one floor on one `Option<UpDuration>`.
Narrowing to just the invariant — `#[non_exhaustive]` on `DaemonSection` too,
plus a private `max_cron_sleep` with an accessor — is much cheaper, and is the
honest option if the property is wanted. It is still a permanent API cost and
a permanent asymmetry (one private field among seven public ones) paid to
guard a floor whose only effect is that a cron worker wakes more often than
intended.

**(c) The ruling.** `DaemonConfig` is **not** a proof token, does not become
one, and does not need to be. Fields stay `pub`. The contract is stated rather
than enforced: `load` and `load_layered` are the validating constructors, and
a caller that mutates a loaded config afterwards is out of contract. shep-core
does not detect that and, with public fields, cannot. Write exactly that into
the type's doc comment. Do not write anything that reads as a guarantee.

**(d) `#[non_exhaustive]` still goes on, for the reason it genuinely buys.**
Not validation — **field growth**. `DaemonConfig` has grown a section per
phase (`whistle` most recently) and will grow another; without the attribute
every one of those is a breaking change for an out-of-tree struct literal.
That is the ordinary IR-20 reasoning applied to a struct, it is true, and it
is checkable: `grep -rn "DaemonConfig" crates | grep "DaemonConfig {"` finds
no struct literal outside `daemon.rs`, so **the change is zero-diff at every
call site in this workspace**. It is still a breaking change for anyone
out-of-tree who does construct one, which is why Task 9 puts it under
`### Changed` in shep-core's changelog rather than burying it in an addition.

**(e) The escape hatch, named but not built.** If an out-of-tree caller ever
does need to mutate a loaded config and re-check it, the answer is to make
`validate` public — a one-line, non-breaking change — not to privatise the
fields. It is not made now because no such caller exists. Say that in
`deferred.md` so the next person does not re-derive the whole question.

**(f) The half that is actually load-bearing** is the `validate` extraction,
and it is required regardless of any of the above. See decision 8.

Nothing in the repository observes `#[non_exhaustive]` — it is invisible from
inside the defining crate, and seeing it needs a `trybuild` compile-fail tier
this project has declined once already. `crates/shep-core/tests/process_info_builder_from_outside_the_crate.rs`
admits the same gap for `ProcessInfo` and says in its own header that it is
shep-core's one `tests/` file and must stay the only one. Honour that: state
the gap, do not invent a second `tests/` file for it.

### 8. Validation happens ONCE, after all three layers. The flags layer must be able to rescue a broken file

`daemon.rs` already carries the reasoning for the layer below, in the comment
above `max_cron_sleep_key`:

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

Its `#[non_exhaustive]` rationale is honest and specific, and it is the same
kind as `DaemonConfig`'s rather than a proof-token claim: this type grows a
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
accepts exactly `1|0|true|false` (read the match arm in `load`). clap's
`BoolishValueParser` also takes `yes|no|y|n|on|off` — wider. Widening an input
grammar is a named top drift risk on this project. So shep-core exports one
function over those four spellings, the env arm calls it, and the CLI's
`value_parser` calls it. One grammar, two callers, DRY where DRY is about
meaning.

**Its name says whose grammar it is: `parse_daemon_bool`, not
`parse_bool_value`.** This is permanent public API on a published library
crate, added to serve one flag. `LogLevel::from_name` is the precedent for
exporting it at all, and the anti-widening argument is worth the surface — but
a name that reads as a general-purpose boolean parser is one an out-of-tree
consumer will adopt and then ask to be widened. The doc's first line names the
scope too: *the boolean grammar of `shep.toml` and the `SHEP_*` environment*.

This is also the flag layer's most useful real form. Spec §13's flagship
scenario runs the shepherd from an init unit, and that unit's `ExecStart` can
now say what it wants without a config file:

```
ExecStart=/usr/local/bin/shep daemon --foreground --log-level info --log-json
```

### 10. Init selection becomes a runtime probe on Linux, a compile-time fact everywhere else, and an operator override always

`unit.rs`'s `Init` doc states the current design and its own expiry date:

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

**The ordering is a pure function and is tested as one.** `current_init`'s
Linux arm reads the two paths and hands two booleans to
`fn linux_init(systemd: bool, openrc: bool) -> Option<Init>`, which is
compiled and tested on every target including this machine. That matters
because the `cfg(target_os = "linux")` arm itself is compiled by nothing here
— see the toolchain gap recorded under "The exact commands" — and because the
original draft admitted it could not mutate the ordering at all. Now it can:
swap the two arms of `linux_init` and a named test goes red.

**This is a behaviour change on Linux and it can bite.** Today every Linux
build gets `Init::Systemd` unconditionally; a container with no
`/run/systemd/system` currently gets a systemd unit written into it happily.
After Task 6 it gets a refusal. That refusal is the *correct* answer — a unit
in a container with no init to read it does nothing — but it is still a case
that worked before and does not after, so it needs an escape hatch and a
`### Changed` changelog line, not an addition.

**The escape hatch: `--init <systemd|openrc|launchd|freebsd-rc|openbsd-rc>` on
`StartupArgs`**, honoured by both `startup` and `unstartup`. `unstartup`
matters as much as `startup`: without the same flag, an operator who installed
a unit under one init and then changed init systems could not remove it — and
that is a claim about `plan()` picking the right `unit_path`, so that is what
Task 6 tests, rather than re-asserting that two verbs sharing one args struct
share its fields.

**Where `Init` lives: `crates/shep-cli/src/cli.rs`, beside `Format`.** Not
`commands::startup::unit`, where it is today. `mod commands` is `#[cfg(unix)]`
in `main.rs`; `cli.rs` carries no `cfg` at all and compiles on every target,
including under `--all-targets`, which also compiles its `#[cfg(test)]`
module. A `pub init: Option<Init>` field on `StartupArgs` naming a type from a
unix-only module makes `cargo check --workspace --all-targets --all-features
--target x86_64-pc-windows-gnu` fail to compile — a phase-gate command Phase
10 restored deliberately and Phase 12a kept green. `cli.rs` today imports
exactly one thing (`std::path::PathBuf`) and defines `Format` as its own
`clap::ValueEnum`; `Init` becomes the second, and `startup::mod` and
`startup::unit` do `use crate::cli::Init;`. That also disposes of the draft's
hedge about whether `cli.rs` could name a `pub(crate)` type inside
`commands::startup::unit` — it could, on unix, which is exactly why the
problem was easy to miss.

The same question was asked of every other type this phase puts in `cli.rs`,
and the rest are clean: Task 1 adds a `bool`; Task 2 adds a unit variant with
no payload; Task 5's `DaemonArgs` fields name `Option<bool>`,
`Option<PathBuf>`, `shep_core::config::LogLevel` and
`shep_core::values::UpDuration`, all of which come from shep-core, which is a
plain unconditional dependency of shep-cli and compiles for Windows today
(a recent commit gates `kv.rs`'s `PathBuf` import to `cfg(unix)` precisely to
keep it that way).

The override is accepted verbatim on any target. Rendering is pure `format!`
and cannot fail; a wrong choice surfaces as a named failed row when the enable
step cannot find `systemctl`/`rc-update`/`rcctl`/`service`, which is a better
diagnosis than a compile-time refusal could give. It also makes every renderer
reachable on the machine you are actually sitting at, which is the only reason
the systemd unit has ever been tested at all.

Consequence to not forget: `Init`'s variants are currently annotated
`#[cfg_attr(not(target_os = "linux"), allow(dead_code))]` and the macOS
equivalent, because only one is constructed per target. With `--init`, all of
them are constructible everywhere. **Both `allow(dead_code)` attributes are
deleted along with the move** — `grep -c "allow(dead_code)"
crates/shep-cli/src/commands/startup/unit.rs` goes `2 → 0`, and if it does
not, clippy `-D warnings` is being lied to.

**Task 6 owns which file, Tasks 7 and 8 own what goes in it.** The split is
deliberate and it is what keeps every intermediate tree a tree that runs:
Task 6 lands all five variants, all five unit *paths*, and the per-init
*mode*, and refuses the three variants whose renderer does not exist yet
through one named gate. Tasks 7 and 8 delete their entry from that gate and
supply the renderer and the enable/remove commands. See Task 6, step 6.4 —
the draft left the three other exhaustive matches on `Init` unspecified, and
two of them cannot return a `Refusal` because they do not return a `Result`.

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
| env for the child | `${name}_env` | inside the `rc_start` command string |

A single renderer with five conditionals would be longer than two `format!`
blocks and would read worse. This is incidental similarity of *shape*, not
repetition of *meaning* — the case CLAUDE.md's DRY rule explicitly carves out.

**Shell quoting: two escapers, and a rule for which one goes where.** Every
one of these three renderers interpolates operator-controlled strings — a
`$SHEP_HOME`, a captured `PATH`, an exec path — into a script that runs as
root at boot. The original draft added `sh_double_quoted` for openrc and then
left both BSD renderers escaping nothing at all, which is the more dangerous
half: OpenBSD hands its whole interpolated string to a shell for evaluation.

There are two contexts and they compose:

- **Value lands inside a double-quoted assignment and is not re-evaluated** —
  e.g. openrc's `command="<exec>"`. Escape `"`, `$`, `` ` `` and `\` with a
  backslash: that is the new `sh_double_quoted`.
- **Value lands inside a string some shell will later evaluate as a command
  line** — e.g. OpenBSD's `rc_start` argument, and FreeBSD's `${name}_env`,
  which `rc.subr` `eval`s. Single-quote the value first so word-splitting and
  expansion cannot touch it, then escape the result for the double-quoted
  context it sits in: `sh_double_quoted(shell_quote(value))`.

`shell_quote` already exists in `crates/shep-cli/src/commands/startup/mod.rs`
— it wraps a word in single quotes, escaping embedded `'` as `'\''`, and today
its only job is making the printed `sudo …` command pasteable. It is exactly
the single-quote former the second rule needs; promote it to `pub(crate)` and
reuse it rather than writing a third one. `sh_double_quoted`'s doc comment
must name `shell_quote` and say in one sentence why they are not the same
function — one produces a standalone word for a human to paste, the other
escapes content that is already inside a double-quoted assignment — or a
later reader folds them together and breaks one of the two callers.

The space case is not hypothetical and it is why the second rule exists at
all: FreeBSD's `${name}_env` is a **space-separated list**, and capturing a
real `PATH` from the invoking environment is the entire point of that field.
An unquoted `PATH` containing a space silently becomes two environment entries
rather than one value.

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

**Service naming: `shep-<user>` on openrc, `shep_<user>` on the BSDs, and both
match their own filename.** openrc names files, so `-` is legal and the
systemd naming carries over; the BSDs name variables, so `_`. The draft set
openrc's `name="shep"` — a constant — while installing at
`/etc/init.d/shep-<user>` and while FreeBSD used the per-user form. openrc
derives defaults from `name`/`RC_SVCNAME`, so two users on one host would
share a `name` while owning distinct service files. Per-user on all three.

**Unit file mode is per-init.** `UNIT_MODE = 0o644` is right for a systemd
unit and a launchd plist, which are *read* by their init system. An openrc
init script and a BSD rc.d script are **executed**, so they need `0o755`.
`UNIT_MODE` becomes `fn unit_mode(init: Init) -> u32`, pinned for all five
variants. Shipping an openrc script at 0644 is a failure that surfaces at the
next reboot, which is the worst time. Four sites reference the constant today
(`grep -c "UNIT_MODE" crates/shep-cli/src/commands/startup/mod.rs` prints 4)
and **one of them is an intra-doc link in `write_unit`'s doc comment** — miss
it and `RUSTDOCFLAGS="-D warnings" cargo doc` fails on a broken link rather
than the compiler telling you.

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

- step 2 binds the socket,
- step 4 restores the muster roll and spawns the dogs,
- step 5 reports readiness,
- and `RpcServer::new(listener, ctx)` — the thing that *accepts* on that
  listener — is constructed in `run`, after `boot` has returned.

So a connection lands in the backlog immediately but **no request is answered
until after the restore and the dogs are up**. The first answered `shep flock`
proves the same milestone `READY=1` proves, one step later. (`shep flock` also
routes through `connect_client`, which never spawns, so the poll cannot
autostart a rogue shepherd.) Write that reasoning into the script as a
comment; it is the kind of claim that gets "simplified" away by someone who
assumes the poll is a guess, and Task 7 pins it with an exact-string test for
that reason.

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
Task 3  $schema key                          ── after 2 (it changes 2's artefact); SEVERABLE
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
node --version || echo "no node on this host"
```

Record the shep-cli count and whether node is present — the last line decides
whether three of this task's tests exercise anything.

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

One test in `cli.rs`'s existing `mod tests`, failing now with clap's
`unexpected argument '--flockfile' found`:

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
`start_args` helper — find it with `grep -n "fn start_args"`) needs
`flockfile: false`; that compile error is the proof the field landed.

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
/// **The `node_missing` sentence has no unit test**, for the same reason the
/// stdin gap a few functions up admits its own: producing it needs a `PATH`
/// without node, and `std::env::set_var` is `unsafe` in edition 2024 in a
/// crate that forbids unsafe. It is pinned in `docs/migration.md` instead,
/// by two greps in Task 9.
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
keep the "do not widen it" sentence. `start` passes `args.flockfile` at its
one `resolve_target` call site.

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
— but a test that silently passes when it skipped is dead, so the skip is
*forceable*:

```rust
    /// Returns `true` when node is on PATH. The `.js` cases below are the
    /// only tests in the workspace that need a second runtime, and a machine
    /// without node must not fail the suite.
    ///
    /// The `eprintln!` is not the guard: libtest captures the output of a
    /// test that PASSES and prints it only on failure, so a skip is
    /// invisible under a plain `cargo test` — which is exactly the host this
    /// helper exists for. `SHEP_REQUIRE_NODE=1` is the guard. Set it on any
    /// machine that has node (the task gate below does) and a broken helper
    /// is a panic rather than a green run over three tests that never ran.
    fn node_available() -> bool {
        let ok = std::process::Command::new("node")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(
            ok || std::env::var_os("SHEP_REQUIRE_NODE").is_none(),
            "SHEP_REQUIRE_NODE is set but node is not usable on PATH"
        );
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

The missing-node path has **no test**, and pretending otherwise would be worse
than admitting it — the reason is in `evaluate_js_flockfile`'s doc above, and
the sentence is pinned in `docs/migration.md` by Task 9 instead.

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

One paragraph under the existing preconditions/properties structure (IR-42).
**Say only what is true.** The draft's version claimed that "entering a
directory and running `shep start` cannot execute code that directory
contains", which describes a code path that does not exist:
`shep_core::config::discover` has no production caller anywhere
(`grep -rn "discover" crates --include="*.rs"` finds only the re-export and
its own tests), and `StartArgs.target` is a required positional, so a bare
`shep start` in a directory is a usage error rather than a discovery. In a
public security document that reads as a description of shipped behaviour.

> **Config files are data, with one opt-in exception.** `shep-core` parses
> every Flockfile format with strict serde and executes nothing; it does not
> spawn a process on any path. A `.js` Flockfile is the exception and is
> reached only through `shep start <path> --flockfile`, which runs the file
> through `node`. `shep start` always takes an explicit target, and
> shep-core's ten-name discovery order — reserved for a future no-argument
> form, and used by nothing today — contains no `.js` name, so it cannot
> route to node either.

The pinning test stays: it is guarding the right thing for the day discovery
is wired.

### Step 1.7 — verify

```bash
grep -rn "JSON.stringify(require" crates | wc -l                     # 0 -> 1
grep -c "a fifth backend" crates/shep-core/src/config/flockfile.rs || true   # 1 -> 0
cargo test -p shep-core --lib --all-features
SHEP_REQUIRE_NODE=1 cargo test -p shep-cli --bins --all-features
```

Deltas, as an enumeration rather than a number to tick:

- shep-core **+1**: `discovery_never_names_a_js_file_and_stays_ten_names`.
- shep-cli **+7**: `start_takes_a_flockfile_flag_and_defaults_it_off` (cli.rs)
  plus the six in `lifecycle.rs`.

On a host without node, drop `SHEP_REQUIRE_NODE=1`; three of the seven then
pass by returning early. To *see* the skip you need `-- --nocapture`, because
libtest prints a passing test's output nowhere else.

### Step 1.8 — MUTATION

In `resolve_target`, move the `as_flockfile` arm **below the `_ if
path.exists()` arm** — not below `(_, Some(format))`.

That distinction is the mutation. Below `(_, Some(format))` is a **no-op**:
for a recognised extension the two arms do the identical read-and-parse, and
for `.js` and for unrecognised extensions `FlockFormat::from_path` returns
`None`, so `(_, Some(format))` does not match and the flag arm still runs.
Every test would still pass. The draft shipped that version as its headline
mutation for its headline security decision.

Below `_ if path.exists()`, four tests fail, because an existing `.js` or
`.ini` file now becomes a minimal app before the flag is ever consulted:
`the_flag_refuses_an_extension_it_cannot_read`,
`a_js_flockfile_under_the_flag_is_evaluated`,
`a_js_flockfile_that_throws_is_an_invalid_config_quoting_node`,
`a_pm2_ecosystem_shape_is_refused_naming_the_right_key`. Blast radius 1
(shep-cli bins). Revert.

**Second mutation:** delete the `as_flockfile` guard entirely so the arm always
runs. Now every target except `-` goes through the flag arm, so the failures
are wider than the draft predicted: `a_js_file_without_the_flag_is_still_a_script`
fails; so does the pre-existing
`any_other_existing_path_becomes_one_minimal_app_named_for_its_stem`, whose
fixture is `server.js`; and so does every test that passes a plain script path
or a nonexistent one, because both now land in `UnknownFlockfileFormat`
instead of `AppConfig::minimal` and `Unresolvable`. Run with
`--no-fail-fast` and count them. A pre-existing test nobody wrote for this
feature going red is the confirmation that decision 1 is guarding something
real. Revert.

### Step 1.9 — gate

Full task gate, one command at a time.

---

## Task 2 — the Flockfile schema, `crates/shep-core/assets/flockfile.schema.json`, and `shep schema`

**Files:** `crates/shep-core/Cargo.toml`,
`crates/shep-core/src/config/flockfile.rs`,
`crates/shep-core/src/config/app.rs`, `crates/shep-core/src/config/mod.rs`,
`crates/shep-core/src/values.rs`, `crates/shep-core/assets/flockfile.schema.json` (new),
`crates/shep-cli/Cargo.toml`, `crates/shep-cli/src/cli.rs`,
`crates/shep-cli/src/commands/schema.rs` (new),
`crates/shep-cli/src/commands/mod.rs`, `crates/shep-cli/src/main.rs`.

### Step 2.1 — baseline

```bash
grep -c "schemars" crates/shep-core/Cargo.toml || true               # 0
grep -c "^\[features\]" crates/shep-core/Cargo.toml || true          # 0
grep -rn --include="*.rs" "JsonSchema" crates/shep-core | wc -l      # 0
git ls-files crates/shep-core/assets | wc -l                         # 0
grep -c '^name = "schemars"$' Cargo.lock                             # 1
grep -c "schemars" Cargo.lock                                        # 5 (record only)
```

The **anchored** lock count is the gate: exactly one `schemars` package is
resolved, and it must still be exactly one afterwards. Two schemars majors
would mean two `JsonSchema` traits and a derive that does not satisfy the one
the consumer wants — that is the thing worth stopping for. The unanchored
count is an *observation*: it goes `5 → 6` by construction, because shep-core's
own dependency list gains a `"schemars",` line. Record the new number; do not
gate on it.

Scope the `JsonSchema` grep to `crates/shep-core`. Workspace-wide it prints 12
today, from Phase 13's whistle payload twins in shep-cli, and is not a signal
for this task.

### Step 2.2 — GREEN: the feature and the derives

`crates/shep-core/Cargo.toml`:

```toml
[features]
# Off by default. `JsonSchema` derives for the Flockfile document schema
# (`assets/flockfile.schema.json`), which `shep schema` prints.
#
# Inside this workspace the gate is inert: cargo unifies features across the
# graph, so every `--all-features` gate command and the shipped `shep` binary
# compile this crate WITH schemars regardless. What it buys is out-of-tree —
# a consumer of the published shep-core does not compile a proc macro for a
# JSON Schema they may not want, and neither does a standalone
# `cargo build -p shep-daemon`.
schema = ["dep:schemars"]

[dependencies]
# … existing …
# Adds no PACKAGE to the tree: `rmcp`'s server feature already resolves
# schemars 1.2.2 for whistle, workspace-pinned exactly. Verified with
# `grep -c '^name = "schemars"$' Cargo.lock` == 1 before and after. It does
# add one dependency EDGE, so the unanchored `grep -c schemars Cargo.lock`
# goes 5 -> 6; that is expected, not a version conflict.
schemars = { workspace = true, optional = true }
```

On `AppConfig`, `ProbeConfig`, `ProbeKind` in `app.rs`:

```rust
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
```

Placed **below** the existing `#[derive(...)]` and above `#[serde(...)]`, and
leaving the `// wire format: changing field names/defaults is a breaking
change` comments where they are.

On `RawFlockfile` in `flockfile.rs` — this is the document type, and decision
5(a) is why the schema is generated from it rather than from `AppConfig`:

```rust
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
// `rename` sets `schema_name`, which schemars uses as the root schema's
// `title`. The type is called `RawFlockfile` because it is the pre-validation
// twin of `Flockfile`; the document an operator writes is a Flockfile, and
// that is what the title has to say.
#[cfg_attr(feature = "schema", schemars(rename = "Flockfile"))]
#[serde(deny_unknown_fields)]
struct RawFlockfile { … }
```

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
/// above. If you change one, change the other — the paired test below is
/// what catches it.
#[cfg(feature = "schema")]
impl schemars::JsonSchema for MemSize {
    fn schema_name() -> std::borrow::Cow<'static, str> { "MemSize".into() }
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

`std::borrow::Cow`, not `alloc::borrow::Cow`: shep-core has no
`extern crate alloc` and `alloc` is not in the extern prelude here, so the
`alloc::` path does not resolve. schemars' own doc examples use `std::`.

**Confirm the `schemars` 1.2.2 API before writing these.** Both facts were
checked when this revision was written — `fn schema_name() -> Cow<'static, str>`
in `src/lib.rs` and `json_schema!` exported from `src/macros.rs` — but check
them again against
`~/.cargo/registry/src/*/schemars-1.2.2/src/`, the same way Phase 12a's Step
1.4 checked ratatui, which is why that task had no API surprises.

Paired tests in `values.rs`, one per type:

```rust
    /// fails if the schema pattern and `FromStr` disagree. A pattern that is
    /// merely self-consistent is worthless; it has to agree with the parser
    /// the schema claims to describe. The reject list carries `512T` and
    /// `1P` for a specific reason: a widened suffix set is the way this
    /// pattern most plausibly goes wrong, and a reject list without a
    /// would-be-accepted suffix cannot catch it.
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
        for rejected in ["512MB", "512m", "1.5G", "", "M", "512T", "1P", "512g"] {
            assert!(!re.is_match(rejected), "pattern accepts {rejected}");
            assert!(rejected.parse::<MemSize>().is_err(), "FromStr accepts {rejected}");
        }
    }
```

`regex` is already a shep-core dependency, so this adds nothing. `schema_for!`
on a single type inlines that type at the root, so `schema["pattern"]` is the
right path here — unlike the document schema, where every field is a `$ref`.

### Step 2.3 — GREEN: the renderer and the drift guard, both in shep-core

In `flockfile.rs`, beside `RawFlockfile`:

```rust
/// The committed Flockfile JSON Schema.
///
/// `include_str!` deliberately: it makes the file a compile-time input, so
/// deleting it fails the build and changing `AppConfig` fails the test
/// below with the command that fixes it. A committed schema nobody
/// regenerates is a lie with a filename, and the only reliable guard is one
/// that runs in `cargo test` rather than in a CI job somebody can forget.
///
/// It lives INSIDE this package, not at the repository root. `cargo package`
/// packs only files under the package directory, and shep-core and shep-cli
/// are both published (`docs/releasing.md`), so a root-relative
/// `include_str!` would compile here and fail for everyone who runs
/// `cargo install shep-cli`.
#[cfg(feature = "schema")]
const COMMITTED: &str = include_str!("../../assets/flockfile.schema.json");

/// How to regenerate the committed copy. Named in the drift test's own
/// failure message, so a red test is self-service.
#[cfg(feature = "schema")]
const REGENERATE: &str =
    "cargo run -p shep-cli -- schema > crates/shep-core/assets/flockfile.schema.json";

/// Renders the Flockfile JSON Schema: the document grammar, pretty-printed
/// with a trailing newline so the committed file is a well-formed text file.
///
/// Generated from `RawFlockfile` — the type serde actually deserializes a
/// Flockfile into — so the schema and the parser cannot drift: they are the
/// same declaration. `AppConfig` supplies the per-app half and lands in
/// `$defs`.
///
/// The schema describes the **deserializer**, not the normalizer.
/// `AppConfig::kill_signal` is `Option<String>` here and stays a plain string
/// in the schema, even though `config::normalize` accepts only four
/// spellings: the schema's job is to describe what serde will parse, and a
/// schema that described a validation step running elsewhere at another time
/// would be wrong the moment those two diverged, in a way no test could
/// catch.
///
/// # Panics
///
/// Never in practice: schemars produces a `serde_json::Value` tree, which
/// `to_string_pretty` cannot fail on. `#[track_caller]` so a future change
/// that makes it fallible reports the caller (IR-24).
#[cfg(feature = "schema")]
#[track_caller]
#[must_use]
pub fn flockfile_schema_json() -> String {
    let schema = schemars::schema_for!(RawFlockfile);
    let mut rendered =
        serde_json::to_string_pretty(&schema).expect("a schemars Schema always serializes");
    rendered.push('\n');
    rendered
}
```

Export it from `config/mod.rs` under the same `#[cfg(feature = "schema")]`.

**One renderer, two consumers.** `shep schema` prints this function's return
value verbatim and the drift test compares it to `COMMITTED`. If the verb
formatted the schema itself, the two could disagree about whitespace and the
committed file would be permanently un-regeneratable.

### Step 2.4 — the artefact, generated BEFORE any assertion about its shape is written

Bootstrap order matters twice over: `include_str!` will not compile without
the file, and every assertion in step 2.5 is a claim about a tree nobody has
looked at yet.

```bash
mkdir -p crates/shep-core/assets
: > crates/shep-core/assets/flockfile.schema.json
cargo run -p shep-cli -- schema > crates/shep-core/assets/flockfile.schema.json
```

The real order is circular and has one seam, so state it: step 2.3 lands the
`include_str!`, which will not compile against a file that does not exist, so
**create the file empty first** — shep-core then compiles with `COMMITTED`
empty. Land step 2.6's verb next, run it into the file, and only then write
step 2.5's assertions. If you would rather not interleave the tasks, a
throwaway `#[test] fn dump() { println!("{}", flockfile_schema_json()) }` with
`-- --nocapture` produces the same bytes, since the renderer is shared.

Then **read it**, and write down what you find, because the next step's tests
are written against it rather than against a guess:

- the root has `properties.app`, an array whose `items` `$ref`s `AppConfig`;
- `$defs.AppConfig.properties.name` and `.script` exist and `required` names
  both;
- `additionalProperties` is `false` at the root and on `AppConfig`
  (`deny_unknown_fields` should produce that — if it does not, say so rather
  than hand-editing the file);
- `description` strings lifted from the doc comments are present;
- how `MemSize`/`UpDuration` fields are expressed: a bare `$ref`, or a
  `$ref` under `anyOf` beside `"null"` because the field is `Option`. **Both
  are plausible and the draft guessed.** Whatever it is, the resolver in
  step 2.5 follows it.

`git add crates/shep-core/assets/flockfile.schema.json`, then
`git ls-files crates/shep-core/assets | wc -l` goes `0 → 1`. Neither manifest
declares `include`/`exclude`, so nothing else is needed to make it ship —
Task 9's `cargo publish --workspace --dry-run` is the confirmation.

### Step 2.5 — tests, in shep-core, written against the file you just read

```rust
    /// Resolves a `$ref` into `$defs`, one hop, and returns the subschema.
    /// Everything with a `schema_name` is referenced rather than inlined, so
    /// an assertion that does not follow the ref is asserting about a
    /// `{"$ref": …}` object and passes or fails for the wrong reason.
    #[cfg(feature = "schema")]
    fn resolved<'a>(root: &'a serde_json::Value, node: &'a serde_json::Value)
        -> &'a serde_json::Value { … }

    /// fails whenever the Flockfile grammar changes and the committed schema
    /// does not. That includes a doc-comment edit: schemars reads `///` into
    /// `description`, which is the point — those become hover text in the
    /// operator's editor — so a docs-only change is a real schema change and
    /// regenerating is the correct response, not a sign anything broke.
    #[cfg(feature = "schema")]
    #[test]
    fn the_committed_schema_is_current() {
        assert_eq!(
            flockfile_schema_json(), COMMITTED,
            "crates/shep-core/assets/flockfile.schema.json is stale. Regenerate it:\n    {REGENERATE}\n\
             A doc-comment edit on AppConfig counts; schemars puts doc comments \
             into `description`."
        );
    }

    /// fails if the artefact goes back to describing ONE APP. The document is
    /// `{"app": [ … ]}`; a schema whose own `required` names `name` and
    /// `script` is an AppConfig schema under a Flockfile filename, and every
    /// real Flockfile would fail against it.
    #[cfg(feature = "schema")]
    #[test]
    fn the_schema_describes_a_document_not_one_app() {
        let schema: serde_json::Value =
            serde_json::from_str(&flockfile_schema_json()).unwrap();
        assert!(schema["properties"]["app"].is_object(), "{schema}");
        assert_eq!(schema["properties"]["app"]["type"], "array", "{schema}");
        assert!(schema["properties"]["name"].is_null(), "root must not be an app: {schema}");
        assert!(schema["$defs"]["AppConfig"].is_object(), "{schema}");
    }

    /// fails if the schema starts describing `normalize`'s grammar instead of
    /// serde's. The four signal names belong to a validation step elsewhere;
    /// a schema that listed them would be describing something it cannot see.
    #[cfg(feature = "schema")]
    #[test]
    fn kill_signal_stays_an_unconstrained_string() { … }

    /// fails if MemSize or UpDuration reverts to a derive and starts
    /// describing its inner integer. Follows the `$ref` — the fields are
    /// references into `$defs`, not inline schemas.
    #[cfg(feature = "schema")]
    #[test]
    fn duration_and_memory_fields_are_string_shaped() { … }
```

Fill in the last two from the file you read in step 2.4, resolving through
`$defs.AppConfig.properties.<field>` and then through `resolved`. **Do not
write the assertion first and check it afterwards** — the draft did that, and
its `schema["properties"]["min_uptime"]` path did not exist even before the
document-schema change.

### Step 2.6 — GREEN: the verb

`crates/shep-cli/Cargo.toml`: `shep-core = { workspace = true, features = ["schema"] }`.
Check the workspace dependency table's existing shape first — it is
`shep-core.workspace = true` today, and the features form has to keep whatever
the workspace entry sets. Note that shep-cli's own `schemars` sits in
`[target.'cfg(unix)'.dependencies]`; this task adds no second entry there,
because shep-cli never names schemars — it calls a shep-core function that
returns a `String`.

`crates/shep-cli/src/cli.rs`, in `Commands`:

```rust
    /// Print the Flockfile JSON Schema. Hidden: the schema is committed at
    /// `crates/shep-core/assets/flockfile.schema.json`, and this is how it is
    /// regenerated.
    #[command(hide = true)]
    Schema,
```

`crates/shep-cli/src/commands/schema.rs` is thin on purpose:

```rust
//! `shep schema`: prints the Flockfile JSON Schema.
//!
//! The schema itself, and the guard that keeps the committed copy honest,
//! live in shep-core beside the type they describe — see
//! `shep_core::config::flockfile_schema_json`. This module is the verb and
//! nothing else, so the string the operator gets and the string the drift
//! test compares are produced by one function.

/// Prints the schema. Always succeeds.
///
/// `--format json` is deliberately ignored: the output *is* JSON, and
/// wrapping a schema in the CLI's envelope would produce a file no editor
/// could read.
pub fn schema(streams: &mut Streams<'_>, _fmt: Format) -> ExitCode { … }
```

### Step 2.7 — verify

```bash
grep -c '^name = "schemars"$' Cargo.lock             # 1 -> 1
grep -c "schemars" Cargo.lock                        # 5 -> 6 (observation)
git ls-files crates/shep-core/assets | wc -l         # 0 -> 1
grep -rn --include="*.rs" "JsonSchema" crates/shep-core | wc -l   # 0 -> non-zero
cargo test -p shep-core --lib --all-features
cargo test -p shep-cli --bins --all-features
```

Deltas: shep-core **+6** — `the_schema_pattern_agrees_with_from_str` twice,
once per type, plus `the_committed_schema_is_current`,
`the_schema_describes_a_document_not_one_app`,
`kill_signal_stays_an_unconstrained_string` and
`duration_and_memory_fields_are_string_shaped`. shep-cli **+0** new tests
unless you add a parse test for the hidden verb; if you do, name it and say
so, because an unexplained delta is one nobody trusts twice.

Then record the Windows cross-check's wall-clock, since this task is what
changes it:

```bash
CARGO_TARGET_DIR=target/win cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

### Step 2.8 — MUTATION

**First:** edit one `///` line on any `AppConfig` field — add a word.
Expected: `the_committed_schema_is_current` fails and its message names the
regeneration command; **nothing else fails**, blast radius 1. That is the
sharp version of this mutation, because it also demonstrates the documented
doc-comment behaviour the test's own message warns about. Revert.

(Adding a *field* to `AppConfig` also fails it, but `AppConfig` is on the wire
and has pinned fixtures, so the radius is wider and the signal is muddier. If
you run that variant, predict the fixture failures too rather than being
surprised by them.)

**Second:** change `MemSize`'s `JsonSchema` pattern to `^\d+(G|M|K|T)?$`.
Expected: `the_schema_pattern_agrees_with_from_str` fails on `512T` — which is
in the reject list *for this mutation* — and `the_committed_schema_is_current`
fails as well. Two independent failures for one edit is the property that makes
the pattern check worth having. (The draft's reject list contained no `T`, so
this mutation fired only the drift test, which fires for any edit at all and
therefore proved nothing about the pattern.) Revert.

### Step 2.9 — gate

---

## Task 3 — `$schema` accepted and ignored (SEVERABLE)

If the phase is being trimmed, this is the task to cut. Cutting it costs
JSON/JSON5 editor completion and nothing else — TOML users reach the schema
through taplo's `#:schema` comment directive, which serde never sees.

**Files:** `crates/shep-core/src/config/flockfile.rs`,
`crates/shep-core/assets/flockfile.schema.json` (regenerated).

### Step 3.1 — baseline

```bash
grep -c 'rename = "\$schema"' crates/shep-core/src/config/flockfile.rs || true   # 0
cargo test -p shep-core --lib --all-features
```

Today, `Flockfile::parse(r#"{"$schema":"x","app":[…]}"#, FlockFormat::Json)`
returns `Err(FlockfileError::Json("unknown field `$schema`, expected `app`…"))`.
Confirm that by writing the RED test first and watching it fail with that
message.

### Step 3.2 — RED then GREEN

```rust
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Flockfile"))]
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

The field is never read, so `dead_code` fires under `-D warnings`. Destructure
it away in `parse` rather than reaching for an `#[allow]`:

```rust
let RawFlockfile { schema: _schema, apps } = raw;
```

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

### Step 3.3 — regenerate the artefact, and watch the guard work

Because the schema is generated from `RawFlockfile` itself (decision 5a),
adding this field changes it. `the_committed_schema_is_current` is red at this
point — **that is the guard doing its job, and seeing it red once is the
cheapest confirmation this phase offers that Task 2 built a real check.**

```bash
cargo test -p shep-core --lib --all-features   # the_committed_schema_is_current: FAILED
cargo run -p shep-cli -- schema > crates/shep-core/assets/flockfile.schema.json
cargo test -p shep-core --lib --all-features   # green, +3
```

Confirm the regenerated file now lists `$schema` under `properties` and still
carries `additionalProperties: false`.

### Step 3.4 — MUTATION

Delete `deny_unknown_fields` from `RawFlockfile`. Expected:
`one_more_key_is_legal_and_no_others_are` fails — and so does
`the_committed_schema_is_current`, because `additionalProperties: false`
disappears from the emitted schema. If the first one passes, that test is
asserting nothing and the document lock has quietly been given away. Revert.

### Step 3.5 — gate

---

## Task 4 — `DaemonConfig`: `#[non_exhaustive]`, extracted `validate`, `DaemonOverrides`

**Files:** `crates/shep-core/src/config/daemon.rs`,
`crates/shep-core/src/config/mod.rs`.

No CLI changes. Task 5 wires it up; this task lands the shep-core half so the
two can be reviewed separately.

**Read decision 7 before starting.** The attribute goes on for field growth,
not for validation, and the doc comment must not claim otherwise.

### Step 4.1 — baseline

```bash
grep -c "fn validate" crates/shep-core/src/config/daemon.rs || true    # 0
grep -c "^#\[non_exhaustive\]" crates/shep-core/src/config/daemon.rs   # 1 (DaemonConfigError)
grep -c "DaemonOverrides" crates/shep-core/src/config/daemon.rs || true # 0
grep -rn "DaemonConfig" crates | grep -v "shep-core/src/config/daemon.rs" | grep -c "DaemonConfig {" || true   # 0
cargo test -p shep-core --lib --all-features
```

That last one is what makes the attribute **zero-diff at every call site in
this workspace**: nothing outside `daemon.rs` constructs a `DaemonConfig` by
struct literal. If it prints anything but `0`, the change is not zero-diff and
the changelog line has to say which call site moved.

Note what it does **not** prove. It says nothing about field mutation, and
field mutation is what the original draft's ruling got wrong — see decision 7.

### Step 4.2 — GREEN: the attribute and its rationale

```rust
/// Parsed daemon configuration with raw per-dog sections
///
/// Dog sections stay untyped here: each dog deserializes its own
/// `[dog.<name>]` table so dog config schemas live with the dog code.
///
/// `#[non_exhaustive]`: this struct has grown a section per phase — `whistle`
/// most recently — and each one would otherwise be a breaking change for an
/// out-of-tree struct literal. That is IR-20's ordinary reasoning applied to
/// a struct. **It is not a validation gate**, and this type is deliberately
/// not the proof token [`crate::config::ResolvedApp`] is: the attribute blocks
/// struct literals and functional-update syntax from outside this crate, but
/// not field mutation, and [`Self::default`] followed by an assignment to
/// `daemon.max_cron_sleep` reaches an unvalidated value without a literal.
///
/// The contract is therefore stated, not enforced. [`Self::load`] and
/// [`Self::load_layered`] are the validating constructors; a caller that
/// mutates a loaded config afterwards is out of contract, and shep-core does
/// not detect it and, with public fields, cannot.
///
/// That is the right trade here because nothing ever *receives* one of these.
/// `ResolvedApp` protects a property of travel — the supervisor is handed one
/// and must trust normalization it cannot see. Every production site loads a
/// `DaemonConfig` and consumes it within a few lines (`run_daemon` renders it
/// straight into `BootOptions`; shep-daemon's `dogs` reads one
/// `[dog.<name>]` table; shep-cli's `whistle::gate` reads one boolean), and
/// the daemon holds a `BootOptions`, not this. Guarding the one
/// `max_cron_sleep` floor against a caller who is already out of contract
/// would cost accessors for every field of every section, including a
/// `BTreeMap<String, toml::Table>` two crates legitimately read. If an
/// out-of-tree caller ever does need to mutate and re-check, the answer is to
/// make `validate` public — one line, non-breaking — not to privatise the
/// fields. `docs/specs/deferred.md` records this as resolved.
///
/// Nothing in the repository observes the attribute itself: it is invisible
/// inside the defining crate, and seeing it needs a `trybuild` compile-fail
/// tier this project declined once already for `ProcessInfo` (see
/// `tests/process_info_builder_from_outside_the_crate.rs`, which admits the
/// same gap and is required to stay shep-core's only `tests/` file).
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
    /// Private. It guards construction, not mutation: a caller outside this
    /// crate can assign to a `pub` field afterwards and this never runs
    /// again. See the type's own doc for why that is accepted rather than
    /// closed.
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

`log_json`, `log_level` and `max_cron_sleep` are all `Copy` behind the
reference (`bool`, `LogLevel` derives `Copy`, `UpDuration` derives `Copy`), so
those three `if let`s read out of `&DaemonOverrides` without a clone; `socket`
is a `PathBuf` and is matched by reference and cloned. Verify the `Copy`
derives before you rely on it — three of them are one `#[derive]` line each.

Extend the existing provenance comment above `max_cron_sleep_key` to name the
third layer rather than leaving it describing two, and extend `load`'s
`# Errors` section, which currently says the floor applies to "file or
`SHEP_MAX_CRON_SLEEP`, whichever won".

### Step 4.4 — GREEN: `DaemonOverrides` and the shared bool grammar

```rust
/// The CLI-flag layer of `file < env < flags` (spec §5).
///
/// Every field is `Option`: `None` means the flag was absent and the layer
/// below wins. Nothing here validates — `DaemonConfig::validate` runs once,
/// after all three layers, so a flag can rescue a file the layer below would
/// have rejected.
///
/// `#[non_exhaustive]` because this type grows a field every time the hidden
/// `daemon` subcommand grows a flag; that is anticipated by construction, not
/// hypothetical — the same field-growth reasoning `DaemonConfig` carries, and
/// like it, not a claim that the value was validated. Build one with
/// [`Self::new`] and the chained setters — the consuming-self shape
/// `ProcessInfo::builder` already uses in this workspace, and the shape
/// `#[non_exhaustive]` requires, since it rules out struct literals and
/// functional update from outside.
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
/// The boolean grammar of `shep.toml` and the `SHEP_*` environment: `1`, `0`,
/// `true`, `false`, and nothing else.
///
/// One function so the file/env layer and the `--log-json` flag cannot drift.
/// clap's own `BoolishValueParser` additionally accepts
/// `yes`/`no`/`y`/`n`/`on`/`off`; using it would widen the grammar on the flag
/// side only, and widening an input grammar beyond spec is a named drift risk
/// on this project.
///
/// The name says whose grammar this is on purpose. It is not a general
/// boolean parser and must not be widened into one: the whole value of
/// exporting it is that there is exactly one answer to "what counts as true
/// in shep's daemon config".
#[must_use]
pub fn parse_daemon_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}
```

`load`'s `SHEP_LOG_JSON` arm calls it. Export `DaemonOverrides` and
`parse_daemon_bool` from `config/mod.rs` alongside `DaemonConfig`.

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
        assert_eq!(parse_daemon_bool("1"), Some(true));
        assert_eq!(parse_daemon_bool("0"), Some(false));
        assert_eq!(parse_daemon_bool("true"), Some(true));
        assert_eq!(parse_daemon_bool("false"), Some(false));
        for wider in ["yes", "no", "on", "off", "TRUE", "y"] {
            assert_eq!(parse_daemon_bool(wider), None, "{wider} must not be a boolean here");
        }
    }
```

`no_env` is the existing `fn no_env(_: &str) -> Option<String>` helper in
`daemon.rs`'s `mod tests`; keep passing it as `&no_env`, the form every other
test there already uses.

### Step 4.6 — verify

```bash
grep -c "fn validate" crates/shep-core/src/config/daemon.rs           # 0 -> 1
grep -c "^#\[non_exhaustive\]" crates/shep-core/src/config/daemon.rs  # 1 -> 3
cargo test -p shep-core --lib --all-features                     # +5, named above
cargo test -p shep-cli --bins --all-features                     # unchanged
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::  # unchanged
```

`shep-cli` and `shep-daemon` being **unchanged** is the check that `load` kept
its contract. If either moves, `load_layered` was not a pure extension.

### Step 4.7 — MUTATION

Move `self.validate(key)?` from the bottom of `load_layered` to immediately
after the file parse. Expected: **two** failures —
`a_flag_rescues_a_below_floor_file_value`, and the pre-existing
`env_max_cron_sleep_floor_check_runs_on_the_winner`, which sets a good file
value and a below-floor `SHEP_MAX_CRON_SLEEP` and expects a refusal naming the
env var; with validation moved above the env layer it never sees the env
value and returns `Ok`. (The draft hedged with "if there is one" — there is;
it is in `daemon.rs`'s `mod tests`.) Revert.

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
    shep_core::config::parse_daemon_bool(value)
        .ok_or_else(|| format!("expected one of 1, 0, true, false; got `{value}`"))
}

/// clap value parser over [`LogLevel::from_name`] — the same lowercase-only
/// grammar `SHEP_LOG_LEVEL` accepts.
fn log_level_flag(value: &str) -> Result<shep_core::config::LogLevel, String> { … }

/// clap value parser over `UpDuration`'s `FromStr`.
fn duration_flag(value: &str) -> Result<shep_core::values::UpDuration, String> { … }
```

All four types come from shep-core, which is an unconditional dependency of
shep-cli and compiles for `x86_64-pc-windows-gnu` today — that is the check
decision 10 is about, and this struct passes it.

Every existing construction of `DaemonArgs` in the workspace's tests gains the
four new fields; that compile error is the proof they landed. Find them with
`grep -rn "DaemonArgs {" crates`.

Parse tests in `cli.rs`:

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
    /// drift `parse_daemon_bool` exists to prevent.
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

At `run_daemon`'s `DaemonConfig::load` call (find it with
`grep -n "DaemonConfig::load" crates/shep-cli/src/commands/daemon.rs`):

```rust
    let overrides = daemon_overrides(args);
    let config = DaemonConfig::load_layered(file_source.as_deref(), &env, &overrides)
        .map_err(DaemonRunError::Config)?;
```

Extract `fn daemon_overrides(args: &DaemonArgs) -> DaemonOverrides` rather than
inlining the builder — that is what makes the chain testable without booting.

Extend `run_daemon`'s doc: the `SHEP_*` paragraph currently reads as if the
environment is the top layer, and after this it is not.

```rust
    /// fails if `run_daemon` builds its overrides from the wrong fields, or
    /// drops one. Drives the config assembly, not the boot — booting a real
    /// shepherd is `daemon_e2e`'s job. Asserts all four fields rather than
    /// sampling one, which is what makes the mutation below land on exactly
    /// one assertion.
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
        let cfg = DaemonConfig::load_layered(
            Some("[daemon]\nlog_json = false\nlog_level = \"error\"\nsocket = \"/tmp/file.sock\"\n"),
            &|_| None,
            &daemon_overrides(&args),
        )
        .unwrap();
        assert!(cfg.daemon.log_json);
        assert_eq!(cfg.daemon.log_level, LogLevel::Trace);
        assert_eq!(cfg.daemon.socket, Some(PathBuf::from("/tmp/flag.sock")));
        assert_eq!(cfg.daemon.max_cron_sleep, Some(UpDuration::from_millis(120_000)));
    }
```

### Step 5.4 — verify

```bash
grep -c "load_layered" crates/shep-cli/src/commands/daemon.rs   # 0 -> 1
cargo test -p shep-cli --bins --all-features                    # +3
```

The three: `log_json_has_three_states`,
`the_flag_bool_grammar_matches_the_env_grammar`,
`every_daemon_flag_reaches_the_config`.

### Step 5.5 — MUTATION

Swap `.log_level(args.log_level)` for `.log_level(None)` in
`daemon_overrides`. Expected: `every_daemon_flag_reaches_the_config` fails on
the `log_level` assertion — the config falls back to the file's `error` — and
nothing else does. Revert.

### Step 5.6 — gate

---

## Task 6 — runtime init detection and `--init`

**Files:** `crates/shep-cli/src/cli.rs`,
`crates/shep-cli/src/commands/startup/mod.rs`,
`crates/shep-cli/src/commands/startup/unit.rs`.

This task adds **no new renderer**. It makes `Init` selectable, gives every
variant its unit path and mode, and refuses the three whose renderer does not
exist yet — which is what Tasks 7 and 8 need, and it is separately reviewable
for exactly that reason.

**Read decision 10 before starting.** `Init` moves to `cli.rs`, and the reason
is the Windows cross-check.

### Step 6.1 — baseline

```bash
grep -c "allow(dead_code)" crates/shep-cli/src/commands/startup/unit.rs   # 2
grep -c "target_os" crates/shep-cli/src/commands/startup/mod.rs           # 3
grep -c "UNIT_MODE" crates/shep-cli/src/commands/startup/mod.rs           # 4
grep -c "const UNIT_MODE" crates/shep-cli/src/commands/startup/mod.rs     # 1
grep -c "init" crates/shep-cli/src/cli.rs                                 # record it
grep -rn "openrc/rc.d are named as deferred" crates | wc -l               # 1
grep -rn "openrc and the BSD rc.d scripts" crates/shep-cli/src | wc -l    # 1
cargo test -p shep-cli --bins --all-features
```

`shep startup --init systemd` today prints clap's
`unexpected argument '--init' found`, exit 2.

The last two greps are **Task 6's deltas, not Task 7's** — both strings live
inside doc comments this task rewrites wholesale, so by the time Task 7 begins
they are already `0`. The draft had them as Task 7 checks, where they could
not fail.

### Step 6.2 — GREEN: `Init`, five variants, in `cli.rs`

Delete the `Init` enum from `unit.rs` and put it in `cli.rs`, beside `Format`,
with both `#[cfg_attr(…, allow(dead_code))]` attributes gone. `unit.rs` and
`startup/mod.rs` then `use crate::cli::Init;`.

```rust
/// Which init system a unit is written for.
///
/// Five variants, all constructible on every target: `--init` lets an
/// operator name one directly, which is also what lets a macOS machine
/// exercise the systemd, openrc and rc.d renderers at all. Selection without
/// the flag is `commands::startup::current_init` — a runtime probe on Linux,
/// where systemd and openrc share one target triple, and a compile-time fact
/// everywhere else, where nothing else the target could be exists.
///
/// It lives in `cli.rs` rather than beside the renderers because `cli.rs`
/// compiles on **every** target while `mod commands` is `#[cfg(unix)]`. A
/// field on `StartupArgs` naming a type from a unix-only module breaks
/// `cargo check --workspace --all-targets --all-features --target
/// x86_64-pc-windows-gnu`, which is a phase-gate command. `Format` above is
/// the precedent: a `clap::ValueEnum` the parse surface owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum Init {
    /// Linux + systemd: a unit file, `Type=notify`.
    Systemd,
    /// Linux + openrc: an `openrc-run` script. No readiness protocol — see
    /// the renderer's own doc.
    Openrc,
    /// macOS: a `LaunchDaemon` plist.
    Launchd,
    /// FreeBSD: an `/etc/rc.subr` script under `/usr/local/etc/rc.d`.
    FreebsdRc,
    /// OpenBSD: an `/etc/rc.d/rc.subr` script under `/etc/rc.d`.
    OpenbsdRc,
}
```

`unit.rs`'s old `Init` doc named the two variants' `allow(dead_code)` narrowing
and said openrc/rc.d were deferred; both claims die with the move. That is
where `grep -rn "openrc/rc.d are named as deferred"` goes to `0`.

### Step 6.3 — GREEN: the probe, with its ordering in a pure function

In `mod.rs`:

```rust
/// Which init a Linux host running these two probes is on.
///
/// A pure function so the ORDER is testable on a machine that is not Linux.
/// systemd wins a tie: `/run/systemd/system` is exactly what `sd_booted(3)`
/// checks and is the only probe here with an upstream contract behind it,
/// and a host with both present is a host running systemd with openrc
/// leftovers rather than the other way round.
const fn linux_init(systemd: bool, openrc: bool) -> Option<Init> {
    if systemd {
        Some(Init::Systemd)
    } else if openrc {
        Some(Init::Openrc)
    } else {
        None
    }
}

/// The init system this host is actually running, or `None` when it is one
/// shep has no renderer for.
///
/// Linux is a **runtime** probe: systemd and openrc share one target triple,
/// so `target_os` cannot tell them apart, and until this existed a Linux host
/// running openrc was silently written a systemd unit whose failure surfaced
/// only when `systemctl` turned out not to exist. The ordering lives in
/// [`linux_init`], which is compiled and tested everywhere; this function is
/// the two filesystem reads that feed it.
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
        linux_init(
            Path::new("/run/systemd/system").is_dir(),
            Path::new("/run/openrc/softlevel").exists() || Path::new("/run/openrc").is_dir(),
        )
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

**`current_init` loses its `const`, and the reason is a trap worth naming.**
It is `const fn` today because both of its arms are literals. The Linux arm
now calls `Path::is_dir`, which is not a const operation — but on macOS that
arm is `#[cfg]`-ed away, so a `const fn` that cannot compile on Linux compiles
perfectly here and fails only on the target nothing on this machine builds.
Drop `const` in the same edit that adds the probe. `linux_init` stays `const
fn`: it is two booleans and a branch.

`plan`'s refusal names both probes:

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

There is **no unused-import risk on `Path`**, contrary to the draft's warning:
`plan(explicit_home: Option<&Path>, …)` uses it unconditionally in the same
file on every target. Do not go looking for that problem. The Linux
cross-check instruction below is real; that sentence was not.

### Step 6.4 — GREEN: `StartupArgs`, per-init paths, per-init mode, and one gate for the three unbuilt renderers

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

Every existing construction of `StartupArgs` in tests gains `init: None`.

`unit_path` and the mode both become functions of `Init`. **Task 6 owns both**
— which file a unit lives at and how it is chmodded is part of "which init",
and landing them here is what lets Tasks 7 and 8 be pure content:

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

`UNIT_MODE` disappears and four sites in `mod.rs` reference it today — the
`const` itself, the `.mode(UNIT_MODE)` call, the `set_permissions` call, and
**an intra-doc link in `write_unit`'s doc comment**. Miss the last one and
`RUSTDOCFLAGS="-D warnings" cargo doc` fails on a broken link rather than the
compiler telling you. (A fifth hit is a comment in a test explaining why it
uses a literal instead; leave it, but reread it — it may now want to name the
function.)

Paths, all five:

`plan()`'s `match init` on `unit_path` becomes one function,
`pub(crate) fn unit_path_for(init: Init, user: &str) -> PathBuf`, so the
mapping is testable without building a whole `StartupPlan` — which is what
step 6.5's `each_init_names_its_own_unit_path` needs. The two existing arms
keep the paths `systemd_unit_path` and `launchd_plist_path` already return;
`unit_path_for` calls them rather than restating the format strings.

| variant | path |
|---|---|
| `Systemd` | `/etc/systemd/system/shep-<user>.service` (unchanged) |
| `Openrc` | `/etc/init.d/shep-<user>` |
| `Launchd` | `/Library/LaunchDaemons/io.github.turtiesocks.shep.<user>.plist` (unchanged) |
| `FreebsdRc` | `/usr/local/etc/rc.d/shep_<user>` |
| `OpenbsdRc` | `/etc/rc.d/shep_<user>` |

**The three unbuilt renderers get ONE gate, in `plan()`.** The draft said to
land them "mapping to a `Refusal`" and ruled out `unimplemented!()`, but only
`plan()` returns `Result<_, Refusal>`: `write_unit`'s match returns a `String`,
and `install` and `remove` push into a `Vec<StartupStep>`. Naming a single
gate is what makes the other three matches' arms provably unreachable instead
of unspecified:

```rust
/// The renderer this init does not have yet, or `None` when it does.
///
/// Temporary, by construction: Task 7 removes `Openrc` from it and Task 8
/// removes the two BSDs, at which point this function and its test are
/// deleted. It exists so that `--init` can accept all five values in the
/// tree that adds the flag without any intermediate state of that tree being
/// able to write a file it cannot render.
pub(crate) const fn unbuilt_renderer(init: Init) -> Option<&'static str> {
    match init {
        Init::Systemd | Init::Launchd => None,
        Init::Openrc => Some("openrc"),
        Init::FreebsdRc => Some("freebsd-rc"),
        Init::OpenbsdRc => Some("openbsd-rc"),
    }
}
```

`plan()` calls it right after resolving `init` and returns
`Refusal { code: ExitCode::Usage, message: format!("shep cannot write a {name} unit yet") }`.
Every caller of `write_unit`, `install` and `remove` takes a `StartupPlan`,
and the only way to get one is `plan()`, so their new arms cannot run. Give
each one a comment saying exactly that and a harmless body — `write_unit`
returns `String::new()`; `install` and `remove` push one failed step naming
the init — and mark all three with the literal `TASK-7-8 REPLACES THIS ARM`, so
`grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src | wc -l` is a
number that must reach `0` by the end of Task 8. (`grep -rc … | wc -l` would
not do: `-rc` prints a line per file *searched*, including `:0`, so piping it
to `wc -l` counts files rather than matches — check shape 1, met in this
plan's own revision.)

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

    /// fails if the probe order ever flips. systemd wins a tie because
    /// `/run/systemd/system` is the check `sd_booted(3)` makes; a host with
    /// both is a systemd host with openrc leftovers. Untestable as a
    /// filesystem probe on this machine — which is the whole reason the
    /// ordering is a pure function.
    #[test]
    fn systemd_wins_when_both_linux_probes_are_true() {
        assert_eq!(linux_init(true, true), Some(Init::Systemd));
        assert_eq!(linux_init(true, false), Some(Init::Systemd));
        assert_eq!(linux_init(false, true), Some(Init::Openrc));
        assert_eq!(linux_init(false, false), None);
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

    /// fails if `--init` stops choosing the unit PATH — which is the half
    /// that matters for `unstartup`. A unit installed under one init has to
    /// be removable after the host has changed to another, and that is a
    /// claim about which file gets removed, not about which struct the two
    /// verbs share. (`Startup` and `Unstartup` both take `StartupArgs`, so a
    /// test that only checked that both parse `--init` could barely fail.)
    #[test]
    fn each_init_names_its_own_unit_path() {
        assert_eq!(unit_path_for(Init::Openrc, "deploy"),
                   PathBuf::from("/etc/init.d/shep-deploy"));
        assert_eq!(unit_path_for(Init::FreebsdRc, "deploy"),
                   PathBuf::from("/usr/local/etc/rc.d/shep_deploy"));
        assert_eq!(unit_path_for(Init::OpenbsdRc, "deploy"),
                   PathBuf::from("/etc/rc.d/shep_deploy"));
        // systemd and launchd keep the paths they already had
    }

    /// fails when Task 7 or 8 lands a renderer and forgets to remove its
    /// entry here — a `--init openrc` that refuses after openrc exists.
    #[test]
    fn only_the_unbuilt_renderers_are_refused() {
        assert_eq!(unbuilt_renderer(Init::Systemd), None);
        assert_eq!(unbuilt_renderer(Init::Launchd), None);
        assert!(unbuilt_renderer(Init::Openrc).is_some());
        assert!(unbuilt_renderer(Init::FreebsdRc).is_some());
        assert!(unbuilt_renderer(Init::OpenbsdRc).is_some());
    }
```

### Step 6.6 — verify

```bash
grep -c "allow(dead_code)" crates/shep-cli/src/commands/startup/unit.rs   # 2 -> 0
grep -c "const UNIT_MODE" crates/shep-cli/src/commands/startup/mod.rs     # 1 -> 0
grep -c "unit_mode(" crates/shep-cli/src/commands/startup/mod.rs          # 0 -> non-zero
grep -rn "openrc/rc.d are named as deferred" crates | wc -l               # 1 -> 0
grep -rn "openrc and the BSD rc.d scripts" crates/shep-cli/src | wc -l    # 1 -> 0
grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src | wc -l       # 0 -> 3
cargo test -p shep-cli --bins --all-features                              # +5, named above
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
CARGO_TARGET_DIR=target/win cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

**The Windows cross-check is not optional here and is the point of the `Init`
move.** Run it before you believe this task is done.

`cargo doc` is in this list rather than only in the gate because `UNIT_MODE`'s
intra-doc link is the failure this task most plausibly ships.

The Linux cross-check is where this task's coverage stops:
`cargo check -p shep-daemon --all-targets --all-features --target
x86_64-unknown-linux-gnu` compiles no shep-cli code at all, and the shep-cli
form needs a Linux cross C toolchain for `ring`. Try it, record what happens,
and note in the task report that `current_init`'s `cfg(target_os = "linux")`
arm was compiled by nothing on this machine — its two filesystem reads are as
far as the untested surface goes, because `linux_init` carries the logic and
has tests.

### Step 6.7 — MUTATION

Swap the two arms of `linux_init` so openrc wins a tie. Expected:
`systemd_wins_when_both_linux_probes_are_true` fails on the first two
assertions. Revert. **This is the mutation the draft said could not exist** —
it wrote that the probe order "cannot be caught by a test on macOS" and fell
back to mutating `unit_mode` instead. Extracting the ordering into a pure
function is what makes it catchable, and that is the reason for the
extraction.

**Second mutation:** change `unit_mode(Init::Openrc)` to `0o644`. Expected:
`the_mode_is_read_only_for_units_and_executable_for_scripts` fails. Revert.

**Third mutation:** remove `Init::Openrc` from `unbuilt_renderer` without
adding a renderer. Expected: `only_the_unbuilt_renderers_are_refused` fails.
That is the check that Task 7 cannot half-land. Revert.

### Step 6.8 — gate

---

## Task 7 — the openrc renderer

**Files:** `crates/shep-cli/src/commands/startup/unit.rs`,
`crates/shep-cli/src/commands/startup/mod.rs`.

### Step 7.1 — baseline

```bash
grep -rni "openrc" crates | wc -l                                        # re-run after Task 6, record
grep -rn "openrc/rc.d are named as deferred" crates | wc -l              # 0 (Task 6 did this)
grep -rn "openrc and the BSD rc.d scripts" crates/shep-cli/src | wc -l   # 0 (Task 6 did this)
grep -c "fn openrc_script" crates/shep-cli/src/commands/startup/unit.rs || true   # 0
grep -c "rc-update" crates/shep-cli/src/commands/startup/mod.rs || true          # 0
grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src | wc -l              # 3
cargo test -p shep-cli --bins --all-features
```

The two `0`s are re-baselined here on purpose: both strings live in doc
comments Task 6 rewrote, so as Task 7 checks they could only ever have been
already-true. Task 7's own deltas are the four below them.

### Step 7.2 — GREEN: the script

`/etc/init.d/shep-<user>`, the path Task 6 already returns. openrc names
*files*, not shell variables, so `-` is fine and the systemd naming carries
over — including into `name=`, which is per-user for the same reason the
FreeBSD one is.

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
/// `shep flock` also routes through `connect_client`, which never spawns, so
/// the poll cannot start a shepherd of its own.
/// Do not "simplify" the poll away on the assumption that it is a guess.
///
/// Every interpolated value goes through [`sh_double_quoted`]: these land
/// inside double-quoted assignments in a script that runs as root at boot.
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

name="shep-<user>"
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

# start_post runs as root, and the control socket lives in a 0700 $SHEP_HOME
# owned by <user>. Root bypasses that, so the poll works; it looks like a
# permission bug only until you have thought it through.
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

**Shell-quoting.** `<user>`, `<home>`, `<path>` and `<exec>` all land inside
double quotes. A value containing `"`, `$`, `` ` `` or `\` breaks out —
exactly the class of bug `systemd_environment_value` and `xml_text` already
handle for the other two renderers. Add:

```rust
/// Escapes a value that lands INSIDE a double-quoted shell assignment:
/// `"`, `$`, `` ` `` and `\` get a backslash.
///
/// Not the same function as [`super::shell_quote`], and the two must not be
/// folded together: `shell_quote` produces a standalone single-quoted *word*
/// for a human to paste into a terminal, while this escapes *content* that
/// is already inside double quotes. Where a value will additionally be
/// re-evaluated by a shell — OpenBSD's `rc_start` string, FreeBSD's
/// `${name}_env` — the two compose, innermost first:
/// `sh_double_quoted(shell_quote(value))`.
fn sh_double_quoted(value: &str) -> String { … }
```

Use it on every interpolation, and pin it with a test built from a path
containing all four characters. This is the openrc equivalent of the `%` and
`&` escapes already in the file, and it is the reviewer's first question.

### Step 7.3 — GREEN: install and remove

In `mod.rs`, replace the `TASK-7-8 REPLACES THIS ARM` openrc arms:

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

And remove `Init::Openrc` from `unbuilt_renderer`, updating
`only_the_unbuilt_renderers_are_refused` in the same edit — Task 6's third
mutation is the check that these two cannot come apart.

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
        assert!(rendered.contains(r"back\\slash"), "a backslash must be escaped: {rendered}");
    }

    /// fails if the service name stops matching the file name — openrc
    /// derives defaults from `name`/`RC_SVCNAME`, and a constant `name`
    /// would make two users on one host collide while owning distinct files.
    #[test]
    fn the_openrc_name_is_per_user_and_matches_the_file() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains(r#"name="shep-deploy""#), "{rendered}");
        assert_eq!(unit_path_for(Init::Openrc, "deploy"),
                   PathBuf::from("/etc/init.d/shep-deploy"));
    }

    #[test]
    fn the_openrc_script_is_the_same_entry_point_as_the_other_two() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains(r#"command_args="daemon --foreground""#));
    }
```

That is five, not four; count what you write.

### Step 7.5 — verify

```bash
grep -c "fn openrc_script" crates/shep-cli/src/commands/startup/unit.rs   # 0 -> 1
grep -c "rc-update" crates/shep-cli/src/commands/startup/mod.rs           # 0 -> 2
grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src | wc -l       # 3 -> 2
grep -rni "openrc" crates | wc -l                                         # record; it should be well up
cargo test -p shep-cli --bins --all-features   # +5
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

**Third mutation:** change `name=` back to the constant `"shep"`. Expected:
`the_openrc_name_is_per_user_and_matches_the_file` fails. Revert.

### Step 7.7 — gate

---

## Task 8 — FreeBSD and OpenBSD `rc.d` renderers

**Files:** `crates/shep-cli/src/commands/startup/unit.rs`,
`crates/shep-cli/src/commands/startup/mod.rs`.

**Read decision 11 before starting.** Two renderers, not one; the username
refusal; the quoting composition rule; and OpenBSD deliberately does not poll.

### Step 8.1 — baseline

```bash
grep -rn "rc.subr" crates | wc -l    # 0
grep -rn "rcvar" crates | wc -l      # 0
grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src | wc -l   # 2
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
shep_<user>_env="SHEP_HOME=<home:quoted> PATH=<path:quoted>"

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
		if "<exec>" --home "<home>" flock >/dev/null 2>&1; then
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

`<home:quoted>` and `<path:quoted>` mean `sh_double_quoted(shell_quote(v))`,
per decision 11: `${name}_env` is a **space-separated list** that `rc.subr`
`eval`s, and a `PATH` with a space in it would otherwise become two
environment entries rather than one value — and capturing a real `PATH` from
the invoking environment is the entire point of that field. Everything else
in the script is a plain double-quoted assignment and takes
`sh_double_quoted` alone.

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

# The environment goes inside the command string rather than being exported
# at the top of this script because rcexec runs the daemon through `su -l`,
# which this plan believes resets the environment. VERIFY against rc.subr(8):
# if su there preserves the environment, two `export` lines at the top are
# simpler and should replace this. Either way the values are single-quoted
# for the shell that evaluates this string and then escaped for the
# double-quoted context they sit in.
rc_start() {
	${rcexec} "SHEP_HOME=<home:quoted> PATH=<path:quoted> ${daemon} ${daemon_flags}"
}

rc_cmd $1
```

Install: `rcctl enable shep_<user>` then `rcctl start shep_<user>`.
Remove: `rcctl stop shep_<user>`, `rcctl disable shep_<user>`, remove file.

**Verify the framework vocabulary before you write either renderer.** This
plan states `start_postcmd` for FreeBSD's `rc.subr`, `rc_pre`/`rc_post` plus
the absence of a post-start hook for OpenBSD's, the `${name}_env`
word-splitting rule, and `su -l`'s treatment of the environment — all from
memory, on a machine that runs neither system. Check `rc.subr(8)` for each; the
online manual pages are authoritative. **If a fact here is wrong, fix the
script and say so in the task report** rather than shipping what this plan
guessed. The one thing that is not negotiable is the honesty rule: if OpenBSD
turns out to have a usable post-start hook, use it and delete the comment; if
it does not, the comment stays exactly as written.

Then remove `Init::FreebsdRc` and `Init::OpenbsdRc` from `unbuilt_renderer` —
which empties it, so **delete the function and
`only_the_unbuilt_renderers_are_refused` along with it**, and the three
`TASK-7-8 REPLACES THIS ARM` comments with them.

### Step 8.5 — tests

Exact-string, same tier as the others. Seven:

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
        assert_eq!(unit_path_for(Init::FreebsdRc, "deploy"),
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

    /// fails if a metacharacter in a path escapes the quoting in either BSD
    /// script. Same class as the openrc test; these two are worse, because
    /// OpenBSD hands its whole string to a shell to evaluate.
    #[test]
    fn the_freebsd_script_quotes_shell_metacharacters() { … }
    #[test]
    fn the_openbsd_script_quotes_shell_metacharacters() { … }

    /// fails if a PATH containing a space becomes two environment entries.
    /// `${name}_env` is a space-separated list, and capturing a real PATH is
    /// the whole reason that field exists.
    #[test]
    fn a_path_with_a_space_stays_one_freebsd_env_entry() { … }

    /// Byte-for-byte, the tier `the_systemd_unit_matches_the_spec_exactly`
    /// and `the_launchd_plist_matches_the_spec_exactly` already set. The
    /// `.contains` tests above each guard one claim; this one guards the
    /// whole artefact, which is the only kind of test a file nobody can run
    /// on its own OS can have.
    #[test]
    fn the_openbsd_script_matches_the_spec_exactly() { … }
```

### Step 8.6 — verify

```bash
grep -rn "rc.subr" crates | wc -l                                     # 0 -> 2 or more
grep -rn "TASK-7-8 REPLACES THIS ARM" crates/shep-cli/src | wc -l   # 2 -> 0
grep -rn "unbuilt_renderer" crates/shep-cli/src | wc -l             # non-zero -> 0
cargo test -p shep-cli --bins --all-features    # +8 new, -1 deleted (only_the_unbuilt_renderers_are_refused)
CARGO_TARGET_DIR=target/win cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

Net **+7** on the shep-cli count: eight added — `a_user_name_that_cannot_be_a_shell_variable_is_refused`
from step 8.2, plus the seven in step 8.5 — and one deleted with
`unbuilt_renderer`. The draft said `+6` while defining four tests; a delta the
implementer cannot reconcile is a delta they stop trusting, which is the
failure mode the whole baselines discipline exists to prevent — so reconcile
it here, out loud, and reconcile it again against what you actually wrote.

### Step 8.7 — MUTATION

Change `name="shep_<user>"` to `name="shep"` in the FreeBSD renderer while
leaving `rcvar` alone. Expected: `the_freebsd_rcvar_matches_the_script_name`
fails. This is the mutation that matters — it is the exact bug a copy-paste
from a single-instance rc script produces, and its symptom in the field is a
service that installs cleanly and never starts at boot. Revert.

**Second mutation:** delete the "no post-start hook" paragraph from the
OpenBSD script. Expected: `the_openbsd_script_admits_it_has_no_readiness_gate`
and `the_openbsd_script_matches_the_spec_exactly` both fail. Revert.

**Third mutation:** drop the inner `shell_quote` from the FreeBSD `${name}_env`
line, leaving only `sh_double_quoted`. Expected:
`a_path_with_a_space_stays_one_freebsd_env_entry` fails and the metacharacter
test still passes — which is the point: the two escapers guard different
things and one is not a substitute for the other. Revert.

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
grep -c "node was not found on PATH" docs/migration.md || true  # 0
grep -c "to a .toml Flockfile" docs/migration.md || true        # 0
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
2. **schemars** — becomes shipped. **Delete "(the directory does not exist)"**,
   which was about root `assets/` and is stale twice over: `assets/` has
   existed since the metrics dog, and the schema does not live there anyway.
   Name `crates/shep-core/assets/flockfile.schema.json`, `shep schema`, and
   the `include_str!` drift guard — including *why* it is inside the package
   (`cargo package` packs only the package directory, and both crates are
   published).
3. **Daemon-config flags layer** — becomes shipped, naming the four flags and
   the validate-once rule.
4. **openrc and BSD rc.d units** — becomes shipped, with three caveats stated
   rather than buried: runtime detection changes Linux behaviour in a
   container; `--init` is the override; **none of the three new scripts has
   been executed on its own operating system**.
5. **`DaemonConfig` is not a proof token** — moves from open question to
   resolution. Write decision 7's *re-derived* answer, not the first draft's:
   the type is not a proof token and does not become one, `#[non_exhaustive]`
   is on it for field growth rather than validation, the contract is stated
   and not enforced because nothing ever receives one from elsewhere, and the
   escape hatch if that ever changes is a public `validate`, not private
   fields. **Do not write that the attribute proves a config was validated.**
   It does not — `Default::default()` plus a field assignment walks past it,
   and shipping that sentence in a published crate's rustdoc is what this
   revision exists to prevent.

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

§5 also says the schema "ships in assets". It ships in
`crates/shep-core/assets/`, and it describes the **document** — `AppConfig` is
still the field set, and now sits in the schema's `$defs`. Amend that sentence
too rather than leaving a reader to discover the artefact is somewhere else.

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
> through node, and node was not found on PATH; install node, or convert
> `<path>` to a .toml Flockfile.*

Then pin **both clauses**, since this is the one sentence in the phase with no
unit test:

```bash
grep -c "node was not found on PATH" docs/migration.md                          # 0 -> 1
grep -c "node was not found on PATH" crates/shep-cli/src/commands/lifecycle.rs  # 1
grep -c "to a .toml Flockfile" docs/migration.md                                # 0 -> 1
grep -c "to a .toml Flockfile" crates/shep-cli/src/commands/lifecycle.rs        # 1
```

The draft pinned only the first clause. The second is the half that carries the
actual advice — the escape hatch decision 3 argues is the whole point of the
sentence — and it is also the half the two artefacts phrase differently (the
code interpolates a path, the doc writes `<path>`), so it is the half most
likely to drift. `to a .toml Flockfile` is the substring both share.

### Step 9.5 — `docs/releasing.md`

Two paragraphs, not one:

- The three new init scripts are rendered and pinned by exact-string tests,
  and have **not** been executed on FreeBSD, OpenBSD, or an openrc host.
  Nothing claims support for those platforms until somebody reports back from
  one.
- `crates/shep-core/assets/flockfile.schema.json` is a committed, generated
  artefact that ships in shep-core's tarball, because
  `crates/shep-core/src/config/flockfile.rs` `include_str!`s it. Regenerate it
  with `cargo run -p shep-cli -- schema > crates/shep-core/assets/flockfile.schema.json`
  before a release if `AppConfig` changed — though the drift test will have
  told you already.

Then run the check this whole location decision exists to satisfy:

```bash
cargo publish --workspace --dry-run
```

### Step 9.6 — CHANGELOGs

`crates/shep-core/CHANGELOG.md` under `## [Unreleased]`:

- `DaemonConfig::load_layered` and `DaemonOverrides` — the `file < env < flags`
  layer, validated once at the end so a flag can rescue a lower layer.
- `config::parse_daemon_bool` — the shared `1|0|true|false` grammar.
- `schema` feature: `JsonSchema` for the Flockfile document, `AppConfig`,
  `ProbeConfig`, `ProbeKind`, `MemSize`, `UpDuration`, and
  `config::flockfile_schema_json`. Off by default.
- `$schema` accepted and ignored at the Flockfile top level (if Task 3
  shipped; delete this line if it was cut).

Under a `### Changed` heading, not buried in an addition:

- `DaemonConfig` is `#[non_exhaustive]`: outside shep-core it can no longer be
  built with a struct literal or functional update. **A breaking change for
  any out-of-tree struct-literal construction.** Say what it is *for* — field
  growth — and do not say it makes the type validated; it does not.

`crates/shep-cli/CHANGELOG.md`:

- `shep start --flockfile`, and `.js` Flockfiles through node. Note explicitly
  that `shep start server.js` is unchanged.
- Hidden `shep schema`; the committed schema at
  `crates/shep-core/assets/flockfile.schema.json`.
- `shep daemon` takes `--log-json`, `--log-level`, `--socket`,
  `--max-cron-sleep`.
- `shep startup`/`unstartup` take `--init`.
- openrc, FreeBSD and OpenBSD renderers, with the "not executed on its own
  operating system" caveat.

Under `### Changed`:

- **A Linux host with no `/run/systemd/system` is now refused instead of being
  written a systemd unit** — the container case — and `--init systemd`
  restores the old behaviour. This is the phase's one user-visible regression
  and it gets its own line rather than a clause inside an addition.

Also correct `crates/shep-cli/CHANGELOG.md`'s existing lines only if they are
wrong about the *past*; a historical entry describing a past state stays.

### Step 9.7 — CLAUDE.md

The Status section names phases through 13. Add Phase 14 in the same register:
the four config-and-packaging items, and what `.js` refuses.

### Step 9.8 — verify

```bash
grep -c "the directory does not exist" docs/specs/deferred.md        # 1 -> 0
grep -c "openrc and BSD rc.d remain open" docs/specs/deferred.md     # 1 -> 0
grep -c "Deferred because making the fields private" docs/specs/deferred.md  # 1 -> 0
grep -c "node was not found on PATH" docs/migration.md               # 0 -> 1
grep -c "node was not found on PATH" crates/shep-cli/src/commands/lifecycle.rs  # 1
grep -c "to a .toml Flockfile" docs/migration.md                     # 0 -> 1
grep -c "to a .toml Flockfile" crates/shep-cli/src/commands/lifecycle.rs        # 1
grep -c "ecosystem.config.js" docs/migration.md                      # 2 -> 3
grep -rn "proves it was validated" crates docs/specs | wc -l          # 0, and it must stay 0
```

That last one is this revision's own guard: the sentence the first draft was
going to ship — that holding a `DaemonConfig` outside shep-core proves it was
validated — must not reach the code or the ledger. It is scoped to `crates`
and `docs/specs` deliberately: this plan quotes the sentence twice in order to
refute it, and a guard that matched its own refutation would be a check that
cannot pass.

### Step 9.9 — phase gate

The four task-gate commands, plus:

```bash
cargo test --workspace --all-features -- --test-threads=1
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
cargo publish --workspace --dry-run
```

The serial run is not ceremony — it was red on `main` before Phase 5 and
caught a real regression in Phase 6. The dry-run publish is new to this phase's
gate and belongs there permanently: it is the check that `include_str!`
reaching outside a package would have broken, and nothing else in the tree
would have caught it before the first real publish. Both benches gates too,
per CLAUDE.md's phase gate.

---

## What this phase deliberately does not build

Say these out loud in the phase report rather than leaving them as silence:

- **No pm2 `ecosystem.config.js` reader.** Decision 2. `shep import` stays the
  pm2 path, and it reads `~/.pm2/dump.pm2`.
- **No `.js` evaluation timeout.** Decision 3, recorded as debt.
- **No new `ExitCode` variant.** Decision 3.
- **No `FlockFormat::Js` and no fifth `FlockfileError` variant.** Decision 4 —
  shep-core never spawns a process.
- **No private fields on `DaemonConfig`, and no proof-token property.**
  Decision 7, re-derived. The type is not tamper-proof and does not need to
  be; the phase says so rather than claiming an attribute buys a guarantee it
  does not buy.
- **No `trybuild` compile-fail tier** to observe `#[non_exhaustive]`. Phase 10
  declined it for `ProcessInfo` and this phase declines it for the same reason;
  the gap is stated, not papered over.
- **No second `tests/` file in shep-core.** Its one file says it must stay the
  only one.
- **No test for the missing-node message.** Decision 3 and Task 1 — it needs a
  `PATH` without node and `set_var` is unsafe in edition 2024. Pinned in
  `docs/migration.md` by two greps instead.
- **No Linux-target compilation of `current_init`'s Linux arm on this
  machine.** The gate's Linux cross-check is `-p shep-daemon` because shep-cli
  needs a cross C toolchain for `ring`. The arm's *logic* is a pure function
  with tests; the two filesystem reads are not compiled here, and the task
  report says so.
- **No NetBSD or DragonFly rc.d.** Spec §11 names four init systems and these
  are not among them.
- **No CI runner for openrc or the BSDs.** The scripts are text with
  exact-string tests, which is the same tier the systemd unit has always had
  on a Mac, and no doc claims more.
