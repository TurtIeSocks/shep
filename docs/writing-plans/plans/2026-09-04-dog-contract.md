# Dog contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A dog can describe its own config to shep, and shep can tell a running dog its config changed. Release 2 of three.

**Architecture:** Two flag names and one grammar move into `shep-core` so both sides of the probe read one definition. `shep-macros` is a new published crate holding only a derive. `shep-client` gains `dogs::probe`, which answers both flags. `shep-cli`'s adopt asks the second one beside where it already asks the first. Both built-in dogs get a schema, `BusEvent` gains a variant, and bark subscribes to it.

**Tech Stack:** Rust 2024, MSRV 1.88. `schemars` 1.2.2 (already workspace-pinned), `syn` and `quote` for the derive.

**Spec:** `docs/brainstorming/specs/2026-09-03-dog-config-design.md`, decisions 4 through 8 plus 6b. Read it first. Two sections were corrected on this branch before planning (`62c174a`, `debe38a`) and the corrections are load-bearing.

## Global Constraints

- **Clean-room rule, non-negotiable:** never open, read, or port source from any pm2 checkout on this machine.
- **Invoke the `shep-idiomatic-rust` skill before writing or reviewing any Rust.** Cite rules as `IR-<n>`.
- **No em dashes or en dashes anywhere**, including code comments and commit messages. Three rounds on the previous branch lost time to this.
- **Never write a real person's name, a personal email, or any absolute home-directory path** into a committed file or a commit message. Repo-relative paths only.
- **Every new public item needs a doc comment** with `# Errors` on anything fallible, and a deliberate `Debug` decision (IR-41).
- **`#[non_exhaustive]` needs a reason comment** (IR-20), and the reason may not claim wire tolerance. It buys source compatibility only; serde gets nothing from it, measured on the previous branch.
- **`#![forbid(unsafe_code)]` is live** in shep-core, shep-client and shep-cli. The new crate gets it too.
- **Prove every new test non-vacuous** by mutation, and **grep the file after each mutation patch to confirm it applied** before believing a green run.
- **A comment that stops being true is a defect.**
- **ONE cargo shape:** `cargo test --workspace --lib --bins --all-features -- --skip ::slow::` while iterating.
- **Task gate, once, at the end:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`, then `cargo build --release`, `./web/scripts/generate-cli-reference.sh`, and from `web/`: `npx astro build` and `npx astro check`. Each from its own command with `$?` captured directly, never through a pipe.
- **Worktree:** `.claude/worktrees/dog-contract`, branch `feat/dog-contract`, cut from `03bea9e`.

---

## What is verified and what is not

Read at `debe38a`. Line numbers drift; grep.

- `VERSION_FLAG = "--version"` is a private const at `crates/shep-cli/src/commands/dogs.rs:868`, and `SHEP_PROTOCOL_KEY = "shep-protocol"` at `:874`.
- `DogVersion` at `:917` carries `version: String` and `protocol: Option<u32>`, with `None` meaning unknown rather than faulty.
- The probe spawns with `env_clear()`, `probe_env()`, `SHEP_HOME`, `SHEP_DOG_NAME`, null stdin, piped stdout, null stderr (around `:817`).
- `shep-core` already has `schemars` as an optional dep behind a `schema` feature; `shep-cli` turns it on, `shep-client` does not.
- `shep-client` depends on `shep-core` and re-exports it.
- `BusEvent` (`crates/shep-core/src/protocol/events.rs:86`) has six variants: `Process`, `LogOut`, `LogErr`, `Channel`, `Dropped`, `DaemonShutdown`. Its topic string is a match on the variant.
- `MetricsConfig` (`crates/shep-cli/src/dog/metrics/mod.rs:41`) has one field, `bind: SocketAddr`. `BarkConfig` (`crates/shep-cli/src/dog/bark/mod.rs:57`) has `sinks`, `rules`, `poll`, `history_bytes`, `sink_timeout`. Both derive `Deserialize` with `deny_unknown_fields, default` and neither derives `JsonSchema` yet.
- `migrate_dog_sections` is called at `crates/shep-cli/src/commands/daemon.rs:298` and `:718`.

**Which of those two call sites can publish, and why it matters.** `:298` is inside `boot_supervisor`, in the daemon process, where the bus exists. `:718` is the reload pre-flight, which runs in the operator's short-lived CLI process with no bus. The only other writer of `dogs.toml` is `forget_dog_section`, called from `rehome` at `commands/dogs.rs:1649`, also CLI-side. That one needs no publish at all: rehome removes the dog, so there is no subscriber left to tell. Do not add a request variant to let a CLI write reach the bus.

**Not verified, and the implementer must check:** whether `Bus` has a publish method other than `publish_log`, what `probe_env()` returns, and whether `shep-macros` needs anything in `release-plz.toml` to be released alongside the others.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/shep-core/src/dogs.rs` (new) | both flag names, the `shep-protocol:` grammar and its parser, the secret marker key |
| `crates/shep-macros/` (new crate) | the `DogConfig` derive and `#[shep(secret)]`, nothing else |
| `crates/shep-client/src/dogs.rs` (new) | `probe`, and the re-export of the derive |
| `crates/shep-cli/src/commands/dogs.rs` | a second probe beside the first, sharing its vetting |
| `crates/shep-cli/src/dog/{metrics,bark}/mod.rs` | `JsonSchema` on both config types, and bark's subscription |
| `crates/shep-core/src/protocol/events.rs` | the seventh `BusEvent` variant |
| `crates/shep-daemon/src/` | publishing it |
| `docs/dogs.md`, `web/` | the contract a dog author reads |

---

### Task 1: one home for the strings both sides parse

**Files:**
- Create: `crates/shep-core/src/dogs.rs`
- Modify: `crates/shep-core/src/lib.rs`, `crates/shep-cli/src/commands/dogs.rs`

**Interfaces:**
- Produces: `shep_core::dogs::{VERSION_FLAG, SCHEMA_FLAG, SHEP_PROTOCOL_KEY, SECRET_KEY}` and a parser for the `--version` answer returning the same shape `DogVersion` holds today.

Behaviour-neutral. `shep-cli` stops owning the private consts and reads shep-core's, and its existing tests must pass unchanged.

- [ ] **Step 1: Move the strings, and the grammar with them**

The point is not the constants, it is that the `shep-protocol:` line's PARSER moves too. Today shep-cli parses an answer that dog authors hand-type from a docs snippet, and a typo reads as "protocol unknown" with nothing saying so. One definition is what lets Task 3 emit exactly what Task 4 reads.

Keep `DogVersion` where it is if it carries CLI-only concerns; move only the parse. Decide which and say why in the module doc.

- [ ] **Step 2: Run the suite**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS with no test edited. If a test needs changing, the move was not neutral: stop and report which.

- [ ] **Step 3: Commit**

---

### Task 2: `shep-macros`

**Files:**
- Create: `crates/shep-macros/{Cargo.toml,src/lib.rs}`
- Modify: root `Cargo.toml` members, `release-plz.toml` if needed

**Interfaces:**
- Produces: `#[derive(DogConfig)]` and the `#[shep(secret)]` field attribute, expanding to the schemars extension keyed on `shep_core::dogs::SECRET_KEY`'s value.

**Why a whole crate for one derive**, per decision 6: `x-shep-secret` is a string shep parses and a dog author would otherwise hand-type. Transpose two letters and it compiles, the schema validates, the field is not marked, and lookout paints a webhook credential on screen. schemars takes a string literal for the extension key, so no exported const can go there and no lint can catch it. The macro is the only thing that turns that into a compile error.

- [ ] **Step 1: Write the failing test**

Proc-macro crates cannot test their own expansion from inside. Put the test where the derive is used, in `shep-client` under Task 3, and note here that Task 2 ships with its compile-time behaviour unproven until then. Say so in the report rather than claiming coverage.

What Task 3's test must pin: a struct with a `#[shep(secret)]` field produces a schema whose field carries the extension, and a struct without one does not.

- [ ] **Step 2: Write the crate**

`[lib] proc-macro = true`. Dependencies: `syn`, `quote`, `proc-macro2`. Add each to the workspace dependency table rather than pinning locally, matching how every other dependency here is declared.

Publishing metadata matters and is permanent: `description`, `license` matching the workspace (`MIT OR Apache-2.0`), `repository`, and `categories`/`keywords` if the siblings carry them. Copy the shape from `crates/shep-client/Cargo.toml` rather than inventing one. Check `release-plz.toml` for whether a new member needs an entry to be released, and say what you found.

- [ ] **Step 3: Confirm it builds and commit**

---

### Task 3: `shep_client::dogs::probe`

**Files:**
- Create: `crates/shep-client/src/dogs.rs`
- Modify: `crates/shep-client/{Cargo.toml,src/lib.rs}`

**Interfaces:**
- Consumes: Task 1's constants, Task 2's derive.
- Produces: `shep_client::dogs::probe::<T>()`, and `shep_client::dogs::DogConfig` re-exporting the derive.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_secret_field_carries_the_marker_and_a_plain_one_does_not() {
    // The whole reason the derive exists. Both halves in one test: a schema
    // where every field is marked would pass a test that only checked the
    // secret one.
}

#[test]
fn the_version_answer_parses_with_shep_cli_s_own_parser() {
    // What `probe` prints for `--version` must be what Task 1's parser
    // reads. These are the two ends of the grammar that was hand-typed
    // from a docs snippet, so pin the round trip rather than the format.
}
```

- [ ] **Step 2: Add the feature and the dependency**

`schema` feature, on by default, per decision 6b. `schemars` optional, `shep-macros` a normal dependency. The feature name matches shep-core's own for the same crate.

Work out what `probe` does when the feature is off. Decision 4 says a dog that answers nothing is recorded as having no schema and refused nothing, so not answering is legal, and the design has a hole this shape already. Whether that means a second entry point, a `cfg` inside one, or a different signature is yours to decide; argue it at the call site.

- [ ] **Step 3: Write `probe`**

It answers `--version` and `--schema` and returns for anything else, so a dog calls it as its first line and carries on. Print the `--version` answer using Task 1's constants rather than a literal.

- [ ] **Step 4: Run, prove non-vacuous, commit**

Mutation: remove the extension from the derive's expansion and confirm the marker test goes red.

---

### Task 4: adopt asks the second flag

**Files:**
- Modify: `crates/shep-cli/src/commands/dogs.rs`

**Interfaces:**
- Consumes: Task 1's constants.
- Produces: a schema alongside `DogVersion` from the same vetting path.

- [ ] **Step 1: Write the failing tests**

One test per shape decision 4 names, because they are four different outcomes and a single "handles bad input" test would pass while three of them were wrong: a dog that answers valid JSON Schema, one that exits non-zero, one that prints nothing, and one that prints invalid JSON. The last three are all recorded as no schema, and only the invalid JSON earns a warning at adopt.

- [ ] **Step 2: Add the probe**

Reuse the spawn shape the `--version` probe already has, including `env_clear()`, `probe_env()`, the null stdin and the null stderr. This executes an untrusted third-party binary; it belongs next to the vetting that already governs that (execute bit, world-writable refusal, canonicalized path, spawn and kill) and must not grow a second, looser path.

Per decision 7 the schema is never stored. If you find yourself adding a field to `shep.toml` or `dogs.toml` to cache it, stop: a stale schema mislabels which field is a credential, which is worse than a stale version number.

- [ ] **Step 3: Run, prove non-vacuous, commit**

---

### Task 5: the topic, and bark listening on it

**Files:**
- Modify: `crates/shep-core/src/protocol/events.rs`, `crates/shep-daemon/src/`, `crates/shep-cli/src/dog/bark/mod.rs`

**Interfaces:**
- Produces: a seventh `BusEvent` variant whose topic is `config.dog.<name>`, published from the daemon-side migration only.

**The spec denied this variant was needed and was wrong**, corrected in `debe38a`. A topic is derived from a variant and the six that exist have nowhere to carry a config change. The variant is additive, so `PROTOCOL_VERSION` does not move: an older peer never subscribed to a topic it does not know.

- [ ] **Step 1: Write the failing tests**

That the variant's topic renders as `config.dog.<name>` for a given name, that a `Topic` pattern of `config.*` matches it and `log.*` does not, and that the daemon-side migration publishes exactly once per dog it moved.

- [ ] **Step 2: Add the variant and publish it**

Publish from `daemon.rs:298` only. `:718` is the reload pre-flight in a CLI process with no bus, and `rehome`'s `forget_dog_section` removes the dog so there is no subscriber left to tell. Both are deliberate; say so in a comment at each, because the next reader will wonder why one of three writers publishes.

- [ ] **Step 3: Make bark subscribe**

bark re-asks with `Request::DogConfig` and swaps its sinks and rules in place, which decision 8 says it can do because they are pure data with no OS resource attached. metrics is NOT in scope here: its one key is a listening socket and rebinding is real work.

If bark restarts itself for any reason, it must say so in its own log. The restart count cannot tell that apart from a crash loop, and that column is what an operator reads as instability.

- [ ] **Step 4: Run, prove non-vacuous, commit**

---

### Task 6: schemas on both built-in dogs, and the contract an author reads

**Files:**
- Modify: `crates/shep-cli/src/dog/{metrics,bark}/mod.rs`, `docs/dogs.md`, `web/src/pages/docs/dogs.astro`

- [ ] **Step 1: Derive `JsonSchema` on both config types**

`MetricsConfig` has one field. `BarkConfig` has five, and `sinks` is a map of a type carrying a webhook URL, which is the field the whole secret marker exists for. Mark it.

Both types derive `Deserialize` with `deny_unknown_fields, default`. Check what those do to a generated schema before assuming the result is right, and pin the marked field with an exact-string test.

- [ ] **Step 2: Write the contract**

`docs/dogs.md` is where a dog author learns this. It gains `--schema`, `probe`, the derive, the secret marker, and the bus topic. Keep the existing paragraph explaining why a dog's section travels over the socket rather than through the environment; it is still true and it is the best thing on that page.

Say plainly that answering is optional and what a dog that stays silent loses, which is release 3's pane.

- [ ] **Step 3: Run the full gate**

Including `astro check`, which CI does not run and which catches a prop a build accepts.

---

## Self-Review

**Spec coverage.** Decision 4 is Tasks 3 and 4. Decision 5 is Task 1 and Task 3. Decision 6 and 6b are Tasks 2 and 3. Decision 7 is Task 4 step 2's warning against caching. Decision 8 is Task 5. The built-in schemas and bark's subscription are Tasks 5 and 6.

**Left open for the implementer, deliberately.** What `probe` does with the `schema` feature off (Task 3 step 2). Whether `DogVersion` moves or only its parser (Task 1 step 1). Whether `release-plz.toml` needs an entry for a new member (Task 2 step 2). Each is flagged at its step, because on the previous branch three defects came from a plan asserting things about existing code its author had not read.

**Placeholder scan.** No TBDs. Test bodies in Tasks 3 to 5 are named and their purpose stated but not written out: the fixtures depend on signatures Task 1 and Task 2 produce, and a plan inventing them would send an implementer to reconcile the difference.

**Type consistency.** `shep_core::dogs::{VERSION_FLAG, SCHEMA_FLAG, SHEP_PROTOCOL_KEY, SECRET_KEY}` is the spelling throughout. `shep_client::dogs::probe` and `shep_client::dogs::DogConfig` are what a dog author touches; `shep-macros` is never named in a dog's own source.
