# Dog Prerequisites Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the six shep-side changes a supervised dog needs before `shep-deploy` can be finished: `rehome` keeps the operator's settings, dogs get an on-remove hook, sheep get smits, the reload response carries its own deadline, the exit table records 12 and 13, and `shep-client`'s missing reconnect gets a ruling rather than a surprise.

**Architecture:** Five of the six are independent and only one is a wire change of any size. `rehome` and the on-remove hook are CLI-only, because `shep.toml` is CLI-only. Smits add one request, one `ProcessInfo` field and one column. The reload deadline adds one field to a response shep already computes. The exit rows are two constants and a table. The reconnect item ends in documentation and one small affordance, and this plan argues why rather than adding a retry loop.

**Tech Stack:** Rust 2024, MSRV 1.88. **No new dependencies.** `toml_edit`, `tokio`, `serde` and `insta` are all already in the workspace.

**Where the requirements come from.** There is no brainstorming spec for this plan. The source is `shep-deploy`'s own two ledgers and its design spec, all outside this repository:

- `/Users/rin/GitHub/shep-deploy/.superpowers/sdd/2026-08-26-deploy-engine/progress.md`
- `/Users/rin/GitHub/shep-deploy/.superpowers/sdd/2026-08-26-deploy-poll-loop/progress.md`
- `/Users/rin/GitHub/shep-deploy/docs/brainstorming/specs/2026-08-26-deploy-dog-design.md`, and its Task 12 in `docs/writing-plans/plans/2026-08-26-deploy-poll-loop.md:4185`

Read the poll-loop ledger's tail and the design spec's "Smits" and "Documentation shep owes" sections before Task 1. Everything below was established there, usually by measurement after something went wrong.

**Reading those files is allowed and expected. `/Users/rin/GitHub/pm2` is not.** See the clean-room constraint below.

## Global Constraints

- **`docs/idiomatic-rust.md`'s rules (IR-1..IR-45).** Invoke the `shep-idiomatic-rust` skill before writing any Rust here. `core::error::Error`, never `std::error::Error`. `# Errors` sections on fallible public functions. `# Panics` with `#[track_caller]`. A deliberate `Debug` decision on every new public item, redacted for anything carrying env or secrets, with an exact-string test (IR-41).
- **No new dependencies.**
- **No em dashes or en dashes** in anything a person reads, `///` comments included, since clap renders those into `--help` and `web/scripts/generate-cli-reference.sh` renders that into the docs site. Existing tests pin this (`crates/shep-cli/src/cli.rs:1415` and its neighbours).
- **Clean-room rule, non-negotiable:** never open, read or reference `/Users/rin/GitHub/pm2`. Reading `/Users/rin/GitHub/shep-deploy` is fine and is where this plan's requirements live.
- **One cargo shape per task.** The workspace shares one target-dir build lock, so concurrent runs block rather than parallelise. Run each gate as its own command with `$?` read directly, never through a pipe: in zsh a pipeline's `$?` is the last command's and `${PIPESTATUS[0]}` is empty.
- **The inner loop is `cargo test -p shep --lib --all-features`** for CLI work and `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::` for daemon work. Do not run bare `--workspace` while iterating. `shep` is a library with three thin `[[bin]]` targets and every unit test lives in the library, so `--lib` reaches them all; add `--bins` only when a task changes a binary's own `main`.
- **The docs trigger is live for every task except 2.** Each of the others changes something an operator types or sees: a help string, an exit code, a JSON field, a table column, or a contract a dog author reads. `web/` is published and part of the public surface. Task 9 is the single sweep that discharges it, and no task before it may claim to be finished on the strength of a green Rust gate alone.
- **`crates/shep-cli/src/cli.rs`, `src/lib.rs`, `src/commands/init.rs`, `src/commands/runtime.rs` and `tests/cli_e2e.rs` carry Rin's own uncommitted work** in the main checkout as of 2026-08-27. This plan runs in a separate worktree, so that work is not present here, but if any task is ever run in the main checkout instead: stage by name, never `git add -A`, and **never run `git checkout` on those files**.
- **`shep-deploy` appears in this repository as a systemd fixture name**, in `crates/shep-cli/src/commands/startup/` and `src/output/rows.rs`. It is an unrelated coincidence. Grepping for `shep-deploy` to find this work will find those instead.

## Verified facts, measured rather than assumed

Established 2026-08-27 by reading this tree at `2ea4226` (v0.1.3). Use these; do not re-derive them.

**On `rehome` and `shep.toml`:**

- **The daemon never writes `shep.toml`.** All config mutation is CLI-side (`crates/shep-daemon/src/lib.rs:244` says the daemon only reads it). `rehome`'s RPC is byte-identical to `disable`'s: both send `Request::DisableDog { name }`. The entire difference between the two verbs is `rehome_dog` vs `disable_dog` in `crates/shep-cli/src/commands/shep_toml.rs`.
- **`ShepToml` wraps `toml_edit::DocumentMut`** (`shep_toml.rs:104`), so formatting, comments and key order survive an edit. Not removing a table is therefore genuinely a no-op, not a reserialization. `disable_dog` already demonstrates this and is pinned at `shep_toml.rs:758`.
- **The `[dog.<name>]` deletion is three lines**, `shep_toml.rs:367-369`, at the end of `rehome_dog` (`shep_toml.rs:355`).
- **`ShepToml::edit` takes an exclusive `flock`** on a sibling `shep.toml.lock` (`shep_toml.rs:204`) and saves through a `0600` tempfile plus `rename(2)` (`shep_toml.rs:461`).
- **`commands/dogs.rs`'s module doc states the ordering invariant**: config first, then the daemon, for `enable`/`disable`/`rehome` (`dogs.rs:12-17`). Task 3 deliberately puts one step in front of both and must say why in that same doc.
- **`rehome` has no e2e test.** `crates/shep-cli/tests/cli_e2e.rs` covers `adopt` four times and never invokes `rehome`.

**On the dog process contract:**

- **A supervised adopted dog gets no argv at all** and exactly one environment variable, `SHEP_HOME` (`crates/shep-daemon/src/dogs.rs:147-155`, pinned by `a_dogs_child_environment_carries_shep_home_and_no_configuration` at `dogs.rs:340`). A built-in dog gets `<current_exe> dog <name>`.
- **`shep <name> [args]` is a second invocation mode**, CLI-side, argv passed through untouched plus `SHEP_HOME` (`crates/shep-cli/src/lib.rs:379`). The on-remove hook is a **third**: shepherd-initiated, one-shot, and the first one shep invents an argv for. **Three sites say a shepherd-started dog gets no argv** and all three stop being the whole truth in Task 3: `docs/dogs.md:223`, `docs/dogs.md:245`, and `web/src/pages/docs/dogs.astro:270`.
- **There is no lifecycle hook mechanism of any kind**, no dog manifest and no capability declaration. Grepped exhaustively for `on-adopt`, `on_adopt`, `on-remove`, `hook`, `dog.toml`, `manifest`, `capabilit`. Every `hook` hit is a panic hook or a webhook URL.
- **`vet_binary` is the precedent for spawning a dog binary out of band** (`crates/shep-cli/src/commands/dogs.rs:356-429`): `Command::new(&canonical).env("SHEP_HOME", home)` with all three stdio handles set explicitly, then kill and wait. Its own comment says why stdio is null'd there: a candidate that writes on its way up would scribble over the operator's terminal mid-vet.
- **`macos_deferred_exec_failure` is the precedent for a bounded wait on a spawned dog** (`dogs.rs:534`), a synchronous `try_wait` poll loop with `PROBE_BUDGET = 50ms` (`dogs.rs:506`) and `PROBE_POLL_INTERVAL = 500us` (`dogs.rs:510`).
- **A dog's stdout and stderr are not read by shep.** They go through the ordinary sheep log pump to `$SHEP_HOME/logs/<name>-<instance>-{out,err}.log`. The hook's output is the first dog output shep itself ever reads.
- **`shep-log-rotate` refuses an unknown argument** with a usage message on stderr and a nonzero exit (`/Users/rin/GitHub/shep-log-rotate/src/main.rs:114-126`). It is a real adopted dog that implements no hook, and it is the compatibility case Task 2 must not break.

**On notices, sanitising and `--quiet`:**

- **`emit_notice` already runs `crate::terminal_safe::sanitise`** over its message (`crates/shep-cli/src/output/mod.rs:655`), in both `Format::Json` and `Format::Table`. Anything routed through a notice is sanitised for free.
- **`Streams::note` writes a notice to stdout, `Streams::aside` writes one to stderr** (`output/mod.rs:163` and `:181`). `aside` is what `adopt` already uses for its group-writable warning (`dogs.rs:583`), which is the exact shape the hook's report has: something worth knowing about a run that is not the answer the operator asked for.
- **`--quiet` does NOT govern the notice stream today.** It is a global flag (`cli.rs:134`) plumbed by hand into `bleats`, `dev` and `runtime` only, and its own help text says so: "Currently narrows `bleats`' own notices". `shep-deploy`'s prerequisite list assumes `--quiet` already governs any notice. **It does not.** Task 3 makes that true for the hook report specifically and updates the flag's help text, which is itself a docs trigger.
- **`terminal_safe::sanitise` is the defence; `output::width::sanitize_cell` is not.** `sanitize_cell` deliberately *keeps* a well-formed CSI sequence, because shep's own colouring is made of them (`crates/shep-cli/src/terminal_safe.rs:42-47` says this in as many words). Any string a third party wrote must pass through `sanitise` first. **This applies to smits and nothing in `shep-deploy`'s spec says so** -- see Task 6.

**On the wire:**

- **`PROTOCOL_VERSION` stays 1 for an additive field.** The precedent is `ProcessInfo::last_exit`, added 2026-08-19 and recorded in `crates/shep-core/CHANGELOG.md:14-32`: "Additive under `Option` on the same terms as every other field this struct has grown since Phase 3 -- `PROTOCOL_VERSION` stays **1**, and a peer that predates the field neither sends nor expects the key."
- **Wire-additive and Rust-additive are different questions.** A `#[serde(default)]` field keeps the wire compatible; whether the Rust change is breaking depends on the variant's shape. Task 7 turns on this and carries the one decision in this plan that is Rin's.
- **Changelogs are hand-written**, Keep a Changelog form, one per crate, `release-plz` does not generate them (`release-plz.toml`, `changelog_update = false`). All four real crates share one version through `[workspace.package]`.
- **`shep-deploy` consumes `shep-client` from crates.io.** Nothing here unblocks its Task 12 until this work is released. Task 10 is that step.

## What each task changes, and why the boundaries fall where they do

| Task | Item | Crate | Wire | Docs trigger |
|---|---|---|---|---|
| 1 | `rehome` keeps `[dog.<name>]` | shep (cli) | no | yes |
| 2 | The hook runner | shep (cli) | no | no |
| 3 | `rehome` runs the hook | shep (cli) | no | yes |
| 4 | Exit rows 12 and 13 | shep (cli) | no | yes |
| 5 | The reload response carries its deadline | shep-core, shep-daemon | **yes** | yes |
| 6 | Smits on the wire and in the daemon | shep-core, shep-daemon | **yes** | no |
| 7 | The SMIT column in `shep flock` | shep (cli) | no | yes |
| 8 | The `shep-client` reconnect ruling | shep-client | no | yes |
| 9 | The docs sweep | web/, docs/ | no | it IS the trigger |
| 10 | Changelogs and the release | all | no | no |

**Tasks 1 through 8 are independent of each other** except for two edges: 3 consumes 2, and 7 consumes 6. Take the parallel legs. Tasks 9 and 10 are sequencing: 9 discharges the docs trigger for all of them at once, because five separate `web/` edits landing one per task would each need their own `astro build` and would conflict in the same three files.

**Where a fake would be too kind, once, up front.** `shep-deploy` shipped seventeen brief defects across two plans, and the two worst survived unit testing because the test doubles were more polite than a real daemon. The doubles that lied were: an `is_alive` that accepted `Starting`, so a process stuck there passed a readiness check; and an instant-turnover fake, where a real shepherd reports the old generation still `Online` while a swap is in flight. Both produced a green suite over code that could not work. Every task below names its own version of that risk in a **"Where a fake would be too kind"** block, and several call for an e2e test against a real binary because no double can answer the question.

---

### Task 1: `rehome` forgets the adoption, not the settings

**Files:**
- Modify: `crates/shep-cli/src/commands/shep_toml.rs` (`rehome_dog` at `:355`, its doc at `:350`, the test at `:804` and its doc at `:800`, the assertion message at `:778`)
- Modify: `crates/shep-cli/src/commands/dogs.rs` (module doc `:2`, `rehome`'s doc `:828`, `rehome_after_config`'s doc `:847`, `disable`'s inline comment `:196`, the test at `:1672` and its assertion at `:1695`)
- Modify: `crates/shep-cli/src/cli.rs` (`Disable`'s help `:293-298`, `Rehome`'s help `:315-321`)
- Modify: `crates/shep-cli/src/output/rows.rs` (`DogRehomedRow`'s doc `:624`)
- Modify: `crates/shep-core/src/config/daemon.rs` (`adopted_dogs`'s field doc `:27-28`)
- Test: the same files' own `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `ShepToml::rehome_dog` keeps its exact signature, `pub fn rehome_dog(&mut self, name: &str)`. Only its behaviour and its contract change. No other task depends on this one.

**Rin approved this on 2026-08-26.** The settings under `[dog.<name>]` are the operator's, not the dog's. Destroying them makes re-adopting the same dog a from-scratch reconfiguration, which is exactly the argument `disable_dog`'s own doc already makes for `disable` (`shep_toml.rs:274-277`: "an operator who disables a dog to restart it must not lose the configuration they wrote for it"). The change here is extending that sentence to `rehome`, not inventing a new one.

**The remaining difference between `disable` and `rehome` is still real and still worth the two verbs.** `disable` leaves the dog in `adopted_dogs`, so shep still knows where its binary lives. `rehome` forgets that. After this task, re-adopting means running `shep adopt <path>` again and getting your configuration back, rather than running it again and starting from an empty table.

**Where a fake would be too kind.** A test that calls `adopt_dog` and then `rehome_dog` proves only that an **empty** `[dog.<name>]` table survives, because `adopt_dog` creates the table and never puts anything in it. That is not the property. The property is that an operator's own hand-written settings survive, so the fixture has to be a `shep.toml` written by hand, carrying a value **and a comment** -- comments are half the reason this file goes through `toml_edit` at all, and a rewrite that preserved keys while dropping comments would pass a keys-only assertion.

- [ ] **Step 1: Write the failing test**

In `crates/shep-cli/src/commands/shep_toml.rs`'s `mod tests`.

```rust
    /// fails if `rehome_dog` takes the operator's own settings with it.
    /// The `[dog.<name>]` table is theirs, not the dog's: deleting it makes
    /// re-adopting the same dog a from-scratch reconfiguration, which is
    /// the exact argument `disable_dog`'s own doc already makes for
    /// `disable`. The fixture is hand-written rather than built by
    /// `adopt_dog` on purpose -- `adopt_dog` creates an EMPTY table, and an
    /// empty table surviving proves nothing about a populated one.
    #[test]
    fn rehoming_a_dog_keeps_the_settings_the_operator_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep.toml");
        std::fs::write(
            &path,
            "[daemon]\n\
             enabled_dogs = [\"otel\"]\n\
             \n\
             [daemon.adopted_dogs]\n\
             otel = \"/usr/local/bin/shep-otel\"\n\
             \n\
             # the collector this box reports to\n\
             [dog.otel]\n\
             endpoint = \"https://otel.example.com:4317\"\n",
        )
        .unwrap();

        ShepToml::edit(&path, |doc| doc.rehome_dog("otel")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        // The adoption is forgotten.
        let cfg = DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(cfg.daemon.enabled_dogs.is_empty(), "{written}");
        assert!(!cfg.daemon.adopted_dogs.contains_key("otel"), "{written}");

        // The settings are not.
        assert!(cfg.dog.contains_key("otel"), "{written}");
        assert!(
            written.contains("endpoint = \"https://otel.example.com:4317\""),
            "the value the operator wrote must survive verbatim: {written}"
        );
        assert!(
            written.contains("# the collector this box reports to"),
            "their comment must survive too, which is half of why this file \
             goes through toml_edit: {written}"
        );
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep --lib --all-features -- rehoming_a_dog_keeps_the_settings
```
Expected: FAIL. The `cfg.dog.contains_key("otel")` assertion is the first to go, because `rehome_dog`'s last block removed the table.

- [ ] **Step 3: Delete the three lines**

In `crates/shep-cli/src/commands/shep_toml.rs`, remove the final block of `rehome_dog` (currently `:367-369`):

```rust
        if let Some(dog) = self.doc.get_mut("dog").and_then(Item::as_table_mut) {
            dog.remove(name);
        }
```

The function keeps its `disable_dog` call and its `adopted_dogs.remove(name)`. Nothing else in it changes.

- [ ] **Step 4: Rewrite `rehome_dog`'s contract**

The doc at `shep_toml.rs:350-354` currently says the table is removed. Replace it:

```rust
    /// Forgets `name`'s ADOPTION: out of `enabled_dogs`, out of
    /// `adopted_dogs`. `[dog.<name>]` is left exactly as it was.
    ///
    /// Those settings are the operator's, not the dog's, and taking them
    /// with the adoption made re-adopting the same dog a from-scratch
    /// reconfiguration. [`Self::disable_dog`] already makes this argument
    /// for `disable`; the only reason `rehome` did not follow it was that
    /// "forget the dog entirely" was read as covering the operator's own
    /// file. Changed 2026-08-27, on Rin's approval.
    ///
    /// What still separates this from [`Self::disable_dog`]: `disable`
    /// leaves the binary's path in `adopted_dogs`, so shep still knows
    /// where the dog lives. This forgets it, so coming back means
    /// `shep adopt <path>` again -- which now finds the configuration
    /// waiting rather than an empty table.
```

- [ ] **Step 5: Rewrite the two tests whose names encode the old claim**

Both are now red and both are **claims**, not incidental assertions, so the names change with the bodies.

`shep_toml.rs:800-829`, `rehoming_a_dog_forgets_it_entirely` becomes:

```rust
    /// fails if `rehome_dog` leaves the ADOPTION behind: `[daemon]
    /// adopted_dogs` or `enabled_dogs`. `[dog.<name>]` is deliberately not
    /// checked here -- that it SURVIVES is
    /// `rehoming_a_dog_keeps_the_settings_the_operator_wrote`'s job, and
    /// asserting the same fact twice in opposite directions is how the two
    /// drift apart.
    #[test]
    fn rehoming_a_dog_forgets_the_adoption() {
```

Its body keeps the `adopt_dog` setup and the two `daemon` assertions, and **loses** `assert!(!cfg.dog.contains_key("otel"));` at `:828`.

`crates/shep-cli/src/commands/dogs.rs:1666-1699`, `rehome_forgets_everything_disable_deliberately_keeps` becomes:

```rust
    /// fails if `rehome` stops forgetting the adoption, or starts forgetting
    /// the configuration again. The two verbs differ over `adopted_dogs`
    /// only: `disable` keeps it, `rehome` drops it, and NEITHER touches
    /// `[dog.<name>]`.
    #[tokio::test]
    async fn rehome_forgets_the_adoption_and_both_verbs_keep_the_settings() {
```

Its `:1695-1698` assertion inverts:

```rust
        assert!(
            cfg.dog.contains_key("otel"),
            "rehome must keep [dog.otel]: those settings are the operator's, \
             and rehome forgets the adoption, not the configuration: {written}"
        );
```

- [ ] **Step 6: Fix every other site that states the old behaviour**

Nine sites, and **fix all nine in this task**. A correction that reaches one site and misses others has happened five separate times across these two projects; the fix is not more care, it is grepping before committing.

| File:line | What it says now | What it must say |
|---|---|---|
| `crates/shep-cli/src/cli.rs:315-321` | "removes it from `[daemon] enabled_dogs`, `[daemon] adopted_dogs`, and its own `[dog.<name>]` table" and "`shep disable` stops a dog without forgetting its configuration; `rehome` is the verb that forgets it" | drop `[dog.<name>]` from the list; the contrast becomes `disable` keeps the dog registered, `rehome` forgets the adoption, and **neither** touches the configuration |
| `crates/shep-cli/src/cli.rs:293-298` | `Disable`'s "`shep rehome` is the verb that forgets a dog entirely" | "`shep rehome` is the verb that forgets the adoption" |
| `crates/shep-cli/src/commands/dogs.rs:2` | module doc, "register or forget a third-party one" | fine as-is, but read it |
| `crates/shep-cli/src/commands/dogs.rs:828` | "stops an adopted dog and forgets it entirely" | "stops an adopted dog and forgets its adoption" |
| `crates/shep-cli/src/commands/dogs.rs:847-853` | "it also erases the registration `disable` leaves alone" | true and stays true; confirm the surrounding sentence does not imply the table |
| `crates/shep-cli/src/commands/dogs.rs:196-198` | `disable`'s inline "that is the difference between `disable` and `rehome`" | narrow it to the adoption |
| `crates/shep-cli/src/output/rows.rs:624-637` | `DogRehomedRow`, "`rehome` reports what it FORGOT" | still true, and `source` is still the right thing to report; check the doc does not name the table |
| `crates/shep-core/src/config/daemon.rs:27-28` | "`shep adopt` writes this; `shep rehome` removes it" | true of `adopted_dogs`, which is the field this doc is on. Read it and leave it. |
| `crates/shep-cli/src/commands/shep_toml.rs:778` | assertion message "disable stops a dog; rehome is what forgets it" | "rehome is what forgets the adoption" |

Docs outside `crates/` (`docs/dogs.md:229`, `docs/terminology.md:28`, `docs/specs/shep-v1.md:286`, `web/src/pages/docs/dogs.astro:251`, `README.md:104`, and the generated CLI reference) are **Task 9's**, deliberately, so the site builds once rather than five times.

- [ ] **Step 7: Verify, including the mutation**

```bash
cargo test -p shep --lib --all-features -- rehom
```
Expected: PASS, three tests.

Then the mutation. **Use `cp` plus a checksum, never `git checkout`** -- the code under test is uncommitted at this point, so `git checkout` would destroy the task's own work rather than the mutation. This exact accident cost a whole task's edits on `shep-deploy`.

```bash
cp crates/shep-cli/src/commands/shep_toml.rs /tmp/shep_toml.rs.bak
shasum /tmp/shep_toml.rs.bak
```

| mutation | expected |
|---|---|
| restore the three deleted lines in `rehome_dog` | `rehoming_a_dog_keeps_the_settings_the_operator_wrote` red on `cfg.dog.contains_key` |
| make `rehome_dog` stop calling `disable_dog` | `rehoming_a_dog_forgets_the_adoption` red on `enabled_dogs.is_empty()` |
| make `rehome_dog` skip `adopted_dogs.remove(name)` | `rehome_forgets_the_adoption_and_both_verbs_keep_the_settings` red |

Restore with `cp /tmp/shep_toml.rs.bak crates/shep-cli/src/commands/shep_toml.rs` and re-check the shasum.

- [ ] **Step 8: Commit**

```bash
git add crates/shep-cli/src/commands/shep_toml.rs crates/shep-cli/src/commands/dogs.rs \
        crates/shep-cli/src/cli.rs crates/shep-cli/src/output/rows.rs \
        crates/shep-core/src/config/daemon.rs
git commit -F- <<'EOF'
fix(cli): rehome forgets the adoption, not the operator's settings

`rehome` removed the dog from `enabled_dogs` and `adopted_dogs` AND deleted
its `[dog.<name>]` table. That table is the operator's own file, not the
dog's: the keys in it were typed by hand, often with comments beside them,
and taking them with the adoption made re-adopting the same dog a
from-scratch reconfiguration of something that was already configured.

`disable_dog`'s own doc has made this argument since it was written: "an
operator who disables a dog to restart it must not lose the configuration
they wrote for it". Nothing about that reasoning is specific to `disable`.
The only reason `rehome` did not follow it is that "forget the dog
entirely" got read as covering a file shep does not own.

The two verbs still differ, and the difference is still worth having.
`disable` leaves the binary's path in `adopted_dogs`, so shep knows where
the dog lives and the next `enable` brings it straight back. `rehome`
forgets that, so coming back means `shep adopt <path>` again -- which now
finds the configuration waiting instead of an empty table.

Approved by Rin 2026-08-26. Surfaced by shep-deploy, whose `[dog.deploy]`
section holds an interval and a retention count an operator chose, and
which could not survive a rehome-and-re-adopt cycle.

The new test's fixture is hand-written rather than built by `adopt_dog`,
because `adopt_dog` creates an EMPTY table and an empty table surviving
proves nothing about a populated one. It asserts on the value AND on the
comment beside it: comments are half of why this file goes through
`toml_edit` rather than parse-and-reserialize, and a rewrite that kept
every key while dropping every comment would pass a keys-only check.

Two test names changed with it. Both encoded the old claim in the name
itself, and a test called `rehoming_a_dog_forgets_it_entirely` asserting
that it does not is worse than no test.

Docs outside crates/ (docs/dogs.md, terminology.md, shep-v1.md, the site,
and the generated CLI reference) land in this branch's docs sweep, so the
Astro build runs once rather than once per task.
EOF
```

---

### Task 2: The hook runner

**Files:**
- Create: `crates/shep-cli/src/commands/hook.rs`
- Modify: `crates/shep-cli/src/commands/mod.rs` (declare the module), `crates/shep-cli/Cargo.toml` (one tokio feature)
- Test: `crates/shep-cli/src/commands/hook.rs`'s own `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces, all `pub(crate)`:
  ```rust
  pub(crate) const ON_REMOVE_ARGV: &str = "on-remove";
  pub(crate) const ON_REMOVE_BUDGET: Duration = Duration::from_secs(10);
  pub(crate) const ON_REMOVE_OUTPUT_CAP: usize = 4096;

  #[derive(Debug, PartialEq, Eq)]
  pub(crate) enum HookOutcome {
      Done,
      Refused { status: i32 },
      TimedOut { after: Duration },
      Signalled { signal: i32 },
      WillNotExec { reason: String },
  }

  #[derive(Debug, PartialEq, Eq)]
  pub(crate) struct HookRun {
      pub(crate) outcome: HookOutcome,
      pub(crate) output: String,
      pub(crate) truncated: bool,
  }

  pub(crate) async fn run_on_remove(exec: &Path, home: &Path) -> HookRun;
  ```

**Why a new module rather than more of `commands/dogs.rs`.** That file is already 1,926 lines and holds four operator verbs plus `vet_binary`'s whole security argument. The hook is a different responsibility -- shep invoking a dog, bounded, and reading what it wrote -- and it is the only place in the CLI that reads a child's output at all. The precedent for this split is `fetch.rs` next to `http.rs`: a new module whose doc names its sibling and says why they are separate. Do the same here, pointing at `commands/dogs.rs` for the verbs and at `src/dog/mod.rs` for the opposite direction (what a dog uses to talk to shep, not what shep uses to talk to a dog).

**`Refused` is not a failure, and the name says so.** A dog that implements no hook exits nonzero on an argument it does not recognise. `shep-log-rotate` does exactly this today (`/Users/rin/GitHub/shep-log-rotate/src/main.rs:114`), and it is adopted on real machines. Every dog that exists predates this hook, so **the common case for a long time will be `Refused`**, and shep must treat it as ordinary rather than as something gone wrong. There is no registry, no manifest and no capability flag to consult first; running it and reading the answer is the whole of the discovery mechanism.

**Where a fake would be too kind.** Three places, and all three are why this task ends with a real-binary test rather than a mocked one:

1. **A double that returns canned output cannot deadlock.** If this is implemented with `Stdio::piped()` plus a `try_wait` poll loop and no reader, a hook that writes more than one pipe buffer blocks in `write`, never exits, and gets killed at the budget with its output lost. A test whose fake dog prints twelve bytes will never show it. **The test must include a dog that prints more than 64 KiB.**
2. **A double cannot exit by signal.** `Signalled` is reachable in production (a hook killed by the operator's own `SIGINT` on the terminal group) and unreachable in any hand-written stub.
3. **A double cannot be slow in a way `start_paused` respects.** A timeout test needs a real child that really sleeps, or a paused clock that the real child does not observe. Use a real child and a real short budget in the test, injected rather than the 10s constant.

- [ ] **Step 1: Enable tokio's `process` feature and prove it costs nothing**

`crates/shep-cli/Cargo.toml:105` currently reads:

```toml
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "signal", "net", "time", "sync", "io-util", "fs"] }
```

Add `"process"`, with a comment in the same style as the ones already around it saying what it is for (`commands::hook` spawning a dog's on-remove hook under a timeout) and that it adds no crates because `signal` already pulls the same transitive set on unix.

**Then measure that claim rather than believing it:**

```bash
cargo tree -p shep --all-features --prefix none | sort -u > /tmp/tree-after.txt
git stash && cargo tree -p shep --all-features --prefix none | sort -u > /tmp/tree-before.txt && git stash pop
diff /tmp/tree-before.txt /tmp/tree-after.txt
```

Expected: **no difference**. If crates appear, **stop and report what they are** rather than proceeding: "no new dependencies" is a global constraint, and the fallback is to redirect the child's stdio to a file under `$SHEP_HOME/run/` and poll `try_wait` with `std::process`, which needs no feature at all. Do not silently take the fallback; say the measurement disagreed.

Why `tokio::process` and not `std::process` with a poll loop, given `vet_binary` uses the latter: `vet_binary` sets all three stdio handles to `null` and never reads a byte, so it cannot deadlock. This does read, and `tokio::process::Command::output()` under `tokio::time::timeout` drains both pipes concurrently, which is the whole problem solved. Doing it with `std::process` means two reader threads or a temp file.

- [ ] **Step 2: Write the failing tests**

In `crates/shep-cli/src/commands/hook.rs`. The fixtures are real shell scripts written into a `tempfile::tempdir`, because every property here is about a real child process.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    /// Writes an executable `#!/bin/sh` script and hands back its path.
    fn dog(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).expect("create");
        write!(file, "#!/bin/sh\n{body}\n").expect("write");
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// fails if a hook that succeeds is not reported as having succeeded, or
    /// if its report is not carried back. The report is the ONLY thing an
    /// operator sees about a removal that moved their sheep, so losing it is
    /// losing the whole point of the hook.
    #[tokio::test]
    async fn a_hook_that_succeeds_reports_done_and_carries_its_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = dog(dir.path(), "good", "echo 'web put back at /home/rin/ReactMap'");
        let run = run_with_budget(&exec, dir.path(), Duration::from_secs(5)).await;
        assert_eq!(run.outcome, HookOutcome::Done);
        assert_eq!(run.output.trim(), "web put back at /home/rin/ReactMap");
        assert!(!run.truncated);
    }

    /// fails if a dog that does not implement the hook is treated as a
    /// failure. EVERY dog that exists predates this hook, and the ones that
    /// parse their argv refuse an unknown one with a usage message and a
    /// nonzero exit -- `shep-log-rotate` does exactly this. That is the
    /// ORDINARY case, not an error, and `rehome` must not report it as one.
    #[tokio::test]
    async fn a_dog_that_does_not_implement_the_hook_is_refused_not_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = dog(
            dir.path(),
            "rotate",
            "echo 'shep-log-rotate takes no options.' >&2\nexit 2",
        );
        let run = run_with_budget(&exec, dir.path(), Duration::from_secs(5)).await;
        assert_eq!(run.outcome, HookOutcome::Refused { status: 2 });
        assert!(
            run.output.contains("takes no options"),
            "its refusal is still worth showing: {}",
            run.output
        );
    }

    /// fails if the hook can hold `rehome` open indefinitely. An operator
    /// asking to remove something is entitled to have it removed, and a dog
    /// that hangs must not be able to prevent that.
    #[tokio::test]
    async fn a_hook_that_hangs_is_killed_at_its_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = dog(dir.path(), "hangs", "sleep 300");
        let budget = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let run = run_with_budget(&exec, dir.path(), budget).await;
        assert_eq!(run.outcome, HookOutcome::TimedOut { after: budget });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it must be killed, not merely abandoned: {:?}",
            started.elapsed()
        );
    }

    /// fails if a chatty hook can deadlock. With `Stdio::piped()` and no
    /// concurrent reader, a child writing past one pipe buffer blocks in
    /// `write`, never exits, and is killed at the budget with everything it
    /// said thrown away -- reported as a hang by a dog that did its job.
    /// 64 KiB is comfortably past the 64 KiB pipe buffer on both platforms
    /// shep ships to.
    #[tokio::test]
    async fn a_hook_that_writes_more_than_a_pipe_buffer_does_not_deadlock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = dog(
            dir.path(),
            "chatty",
            "i=0\nwhile [ $i -lt 2000 ]; do \
             echo 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; \
             i=$((i+1)); done",
        );
        let run = run_with_budget(&exec, dir.path(), Duration::from_secs(10)).await;
        assert_eq!(
            run.outcome,
            HookOutcome::Done,
            "a chatty hook that exits 0 must be reported as Done, not TimedOut"
        );
        assert!(run.truncated, "and its output must be capped");
        assert!(run.output.len() <= ON_REMOVE_OUTPUT_CAP);
    }

    /// fails if a hostile or careless hook can drive the operator's
    /// terminal. This output is written by a third party and printed to a
    /// person, which is exactly what `terminal_safe::sanitise` exists for.
    /// `output::width::sanitize_cell` is NOT a substitute: it deliberately
    /// KEEPS a well-formed CSI sequence, because shep's own colouring is
    /// made of them.
    #[tokio::test]
    async fn a_hook_cannot_drive_the_operators_terminal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = dog(dir.path(), "hostile", "printf '\\033[2Jcleared your screen'");
        let run = run_with_budget(&exec, dir.path(), Duration::from_secs(5)).await;
        assert!(
            !run.output.contains('\u{1b}'),
            "no escape may survive: {:?}",
            run.output
        );
        assert!(run.output.contains("cleared your screen"), "{}", run.output);
    }

    /// fails if the hook stops getting `$SHEP_HOME`. It is the ONE variable
    /// a dog inherits and the only way it finds the control socket, so a
    /// hook without it cannot reach the shepherd it is being asked to put
    /// sheep back through.
    #[tokio::test]
    async fn the_hook_is_given_shep_home_and_the_on_remove_argument() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = dog(dir.path(), "echoes", "echo \"$1 at $SHEP_HOME\"");
        let run = run_with_budget(&exec, dir.path(), Duration::from_secs(5)).await;
        assert_eq!(
            run.output.trim(),
            format!("on-remove at {}", dir.path().display())
        );
    }

    /// fails if a binary that cannot run at all is reported as a refusal.
    /// The two are different: a refusal means the dog answered, and this
    /// means it never started, which is worth different words.
    #[tokio::test]
    async fn a_binary_that_will_not_exec_says_so_rather_than_refusing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let run =
            run_with_budget(&dir.path().join("nothing-here"), dir.path(), Duration::from_secs(5))
                .await;
        assert!(
            matches!(run.outcome, HookOutcome::WillNotExec { .. }),
            "{:?}",
            run.outcome
        );
    }
}
```

- [ ] **Step 3: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features -- commands::hook
```
Expected: FAIL to compile, "cannot find function `run_with_budget`".

- [ ] **Step 4: Write the module**

`run_on_remove` is `run_with_budget(exec, home, ON_REMOVE_BUDGET)`. `run_with_budget` is what the tests call, so the budget is injectable and no test waits ten seconds.

```rust
pub(crate) async fn run_with_budget(exec: &Path, home: &Path, budget: Duration) -> HookRun {
    let mut command = tokio::process::Command::new(exec);
    command
        .arg(ON_REMOVE_ARGV)
        // The one variable a dog inherits, and how it finds the socket.
        // `env_clear` is deliberately NOT used here, for the reason
        // `vet_binary` gives at commands/dogs.rs:394: a real dog runs with
        // the daemon's own filtered environment, and a hook run under
        // stricter conditions than the dog ever sees would fail for a
        // binary that works.
        .env("SHEP_HOME", home)
        // Closed, not inherited: this runs while an operator is sitting at
        // a terminal, and a hook that read from it would steal their keys.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return HookRun {
                outcome: HookOutcome::WillNotExec { reason: err.to_string() },
                output: String::new(),
                truncated: false,
            };
        }
    };

    // `wait_with_output` drains both pipes CONCURRENTLY with the wait. A
    // `try_wait` loop that did not read would deadlock the moment the hook
    // wrote past one pipe buffer, and would then report a working dog as a
    // hang. `kill_on_drop` above is what makes the timeout arm a kill
    // rather than an abandonment.
    let Ok(finished) = tokio::time::timeout(budget, child.wait_with_output()).await else {
        return HookRun {
            outcome: HookOutcome::TimedOut { after: budget },
            output: String::new(),
            truncated: false,
        };
    };
    ...
}
```

The rest, spelled out rather than left to judgement:

- On `Err` from `wait_with_output`, return `WillNotExec { reason }`.
- Merge `stdout` then `stderr`, in that order, with `String::from_utf8_lossy`. Both matter: a dog's report goes to stdout and its refusal goes to stderr, and `shep-log-rotate` proves the second is the one an operator most needs when nothing else explains the exit.
- Truncate to `ON_REMOVE_OUTPUT_CAP` bytes **on a `char_indices` boundary**, not `&s[..cap]`, which panics mid-codepoint. Set `truncated`.
- Then `crate::terminal_safe::sanitise` the result and keep only the `String`. Sanitise **after** truncation, so the cap is on what the dog wrote rather than on what survived.
- Outcome from the status: `code() == Some(0)` is `Done`; `code() == Some(n)` is `Refused { status: n }`; `None` means a signal, so `Signalled { signal: status.signal().unwrap_or(0) }` behind `use std::os::unix::process::ExitStatusExt`.

Every public item needs a `# Errors`-free doc (nothing here returns `Result`) and `HookOutcome` needs a deliberate `Debug`: it carries a path-free reason string and no environment, so a derived `Debug` is correct. Say that in one line rather than leaving it implied (IR-41).

- [ ] **Step 5: Run the tests**

```bash
cargo test -p shep --lib --all-features -- commands::hook
```
Expected: PASS, seven tests.

- [ ] **Step 6: Mutation-check**

`cp crates/shep-cli/src/commands/hook.rs /tmp/hook.rs.bak && shasum /tmp/hook.rs.bak` first. **Never `git checkout`**: this file is uncommitted, so `git checkout` would delete the task instead of the mutation.

| mutation | expected |
|---|---|
| swap `wait_with_output()` for a `try_wait` poll loop that never reads | `a_hook_that_writes_more_than_a_pipe_buffer_does_not_deadlock` red, reporting `TimedOut` |
| drop `.kill_on_drop(true)` | `a_hook_that_hangs_is_killed_at_its_budget` red on the elapsed assertion, not on the outcome |
| drop the `terminal_safe::sanitise` call | `a_hook_cannot_drive_the_operators_terminal` red |
| map a nonzero status to `WillNotExec` instead of `Refused` | `a_dog_that_does_not_implement_the_hook_is_refused_not_failed` red |
| drop `.env("SHEP_HOME", home)` | `the_hook_is_given_shep_home_and_the_on_remove_argument` red |
| truncate with `&output[..cap]` | the chatty test panics rather than failing, which is still red; note it, because a panic and a failure read differently in a log |

- [ ] **Step 7: Commit**

```bash
git add crates/shep-cli/src/commands/hook.rs crates/shep-cli/src/commands/mod.rs crates/shep-cli/Cargo.toml
git commit -F- <<'EOF'
feat(cli): run a dog's on-remove hook, bounded and readable

The runner only. Nothing calls it yet; `rehome` picks it up in the next
commit, deliberately split so the process behaviour has its own gate.

A dog that takes a sheep over owes the operator a way back. shep-deploy
re-registers a sheep with its `cwd` under `$SHEP_HOME`, which is a path the
operator has no reason to know about, so removing that dog without telling
it leaves an app running from somewhere its owner will not look. The hook
is the moment the dog gets to put things back, and it runs once, before
shep forgets anything, with the shepherd still up.

Three properties this is built around, and each has a test that dies
without it.

It cannot deadlock. `wait_with_output` drains both pipes concurrently with
the wait; a `try_wait` loop that did not read would block a hook writing
past one pipe buffer, kill it at the budget, and report a working dog as a
hang. The test writes 100 KB through a real child.

It cannot hold `rehome` open. Ten seconds, then `kill_on_drop` ends it. An
operator asking to remove something is entitled to have it removed, and a
dog arguing about its own uninstallation is not a state shep should be able
to reach.

It cannot drive the terminal. The output is written by a third party and
printed to a person, so it goes through `terminal_safe::sanitise`. Note
that `output::width::sanitize_cell` would NOT have done: that one
deliberately keeps a well-formed CSI sequence, because shep's own colouring
is made of them.

`Refused` is named as it is because it is not a failure. Every dog that
exists predates this hook, and one that parses its argv answers an unknown
argument with a usage message and a nonzero exit -- `shep-log-rotate` does
exactly that today, on real machines. There is no manifest and no
capability flag to consult first, so running it and reading the answer is
the whole of the discovery mechanism, and the ordinary answer for a long
while will be "I do not implement this".

tokio's `process` feature is new here and was measured rather than assumed:
`cargo tree` is identical before and after, because `signal` already pulls
the same transitive set on unix.
EOF
```

---

### Task 3: `rehome` runs the hook

**Files:**
- Modify: `crates/shep-cli/src/commands/dogs.rs` (module doc `:12-25`, `rehome` `:829-845`)
- Modify: `crates/shep-cli/src/output/rows.rs` (`DogRehomedRow` `:638-687`, its `Render` impl, and the `PRIORITIES` pin at `:2821`)
- Modify: `crates/shep-cli/src/lib.rs` (the `Commands::Rehome` dispatch at `:1318`, to pass `quiet`)
- Modify: `crates/shep-cli/src/cli.rs` (`--quiet`'s help text at `:128-134`)
- Test: `crates/shep-cli/src/commands/dogs.rs`'s `mod tests`, and `crates/shep-cli/tests/cli_e2e.rs`

**Interfaces:**
- Consumes: Task 2's `commands::hook::{run_on_remove, HookOutcome, HookRun}`.
- Produces: `dogs::rehome` grows one parameter, `quiet: bool`:
  ```rust
  pub async fn rehome(
      streams: &mut Streams<'_>,
      paths: &ShepPaths,
      name: &str,
      quiet: bool,
  ) -> ExitCode;
  ```
  and `DogRehomedRow` grows one field, `hook: Option<String>`.

**The order changes, and `commands/dogs.rs`'s module doc must say why.** That doc states the invariant at `:12-17`: config first, then the daemon, for `enable`/`disable`/`rehome`, so that a failed RPC still leaves the config saying what the operator asked for. The hook goes **in front of both**, which is a third position rather than a violation of that rule:

| step | why it must be here |
|---|---|
| 1. run the hook | the dog is still adopted, still running, and the shepherd is still up. All three are things the next two steps take away, and the hook needs all three: `shep-deploy`'s hook connects to the socket and issues `Delete` and `Start` for every sheep it moved. |
| 2. edit `shep.toml` | unchanged, and still ahead of the RPC for the reason the doc already gives |
| 3. `Request::DisableDog` | unchanged |

**The hook's failure never changes the exit code.** `rehome` exits exactly as it did before: `Success` on the config write, whatever the shepherd said, and a hook that refused, hung, or could not be run is reported and stepped over. This is the same reasoning the dog's own side already reaches from the other direction: an operator asking to remove something is entitled to have it removed, and a nonzero exit here would be shep arguing on a dog's behalf about its own uninstallation.

**`--quiet` does not do what `shep-deploy`'s prerequisite list assumes.** That list says the hook's output should ride "the existing notice stream so `--quiet` governs it". Today `--quiet` is a global flag plumbed by hand into three commands and its own help text says so: "Currently narrows `bleats`' own notices". Notices are not gated by it anywhere. This task makes the sentence true for one more command and updates the help text to match, which is why `cli.rs` is in the file list and why this task carries a docs trigger.

**Where a fake would be too kind.** The whole value of this task is ordering, and ordering is invisible to a unit test that stubs the hook: a stub returns instantly and cannot observe that `shep.toml` was already rewritten. **The ordering must be pinned by a real dog binary that inspects the world it was run in**, which is an e2e test, not a unit test. `rehome` has no e2e test at all today, so this task adds the first one. The unit tier still earns its place for the rendering and the `--quiet` gate; it just cannot answer the question this task exists for.

- [ ] **Step 1: Write the failing e2e test**

In `crates/shep-cli/tests/cli_e2e.rs`, following the four existing `adopt` tests (`:5908`, `:5936`, `:6006`, `:6055`) for the harness shape.

```rust
/// fails if `rehome` runs the hook after it has already forgotten the dog.
/// The hook exists to let a dog put things back, and everything it needs to
/// do that -- its own registration, its `[dog.<name>]` settings, a running
/// shepherd -- is something the two steps after it take away. A stubbed
/// hook cannot catch this: it has no world to look at. This one reads
/// `shep.toml` from inside the hook and reports what it found.
#[test]
fn the_on_remove_hook_runs_before_anything_is_forgotten() {
    let home = tempfile::tempdir().expect("tempdir");
    let bin = tempfile::tempdir().expect("tempdir");

    // A dog that answers `on-remove` by reporting whether it can still see
    // its own adoption and its own settings.
    let exec = bin.path().join("shep-nosy");
    std::fs::write(
        &exec,
        "#!/bin/sh\n\
         if [ \"$1\" != on-remove ]; then sleep 3600; fi\n\
         if grep -q 'nosy' \"$SHEP_HOME/shep.toml\"; then \
           echo 'still adopted'; else echo 'ALREADY FORGOTTEN'; fi\n\
         if grep -q 'colour = \"blue\"' \"$SHEP_HOME/shep.toml\"; then \
           echo 'settings intact'; else echo 'SETTINGS GONE'; fi\n",
    )
    .expect("write");
    // 0o755, via std::os::unix::fs::PermissionsExt, as the adopt tests do.

    // adopt, then hand-write a setting into [dog.nosy].
    // ... shep --home <home> adopt <exec> ...
    // ... append `colour = "blue"` under [dog.nosy] ...

    let output = shep(&home).args(["rehome", "nosy"]).output().expect("run");

    let printed = String::from_utf8_lossy(&output.stdout)
        + String::from_utf8_lossy(&output.stderr);
    assert!(printed.contains("still adopted"), "{printed}");
    assert!(printed.contains("settings intact"), "{printed}");
    assert!(!printed.contains("ALREADY FORGOTTEN"), "{printed}");
    assert_eq!(output.status.code(), Some(0), "{printed}");
}

/// fails if a dog that does not implement the hook makes `rehome` fail, or
/// go quiet about it. Every dog that exists predates this hook. The removal
/// must still happen, still exit 0, and still say what it heard.
#[test]
fn rehoming_a_dog_with_no_hook_still_removes_it_and_exits_zero() {
    // A dog whose script is `if [ "$1" = on-remove ]; then
    //   echo 'takes no options' >&2; exit 2; fi; sleep 3600`
    // adopt it, rehome it, then assert:
    //   - status 0
    //   - "takes no options" reached the operator
    //   - `shep.toml` no longer carries the adoption
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --test cli_e2e --all-features -- on_remove rehoming_a_dog_with_no_hook
```
Expected: FAIL. Nothing runs a hook, so the first prints neither line and the second never sees `takes no options`.

- [ ] **Step 3: Wire it into `rehome`**

`crates/shep-cli/src/commands/dogs.rs:829`. The hook runs first, off a path read **before** the edit:

```rust
pub async fn rehome(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    name: &str,
    quiet: bool,
) -> ExitCode {
    // FIRST, ahead of both the config edit and the RPC, and deliberately
    // out of step with this module's own config-first rule. The hook needs
    // the dog still adopted, still running, and a shepherd still up; steps
    // two and three take all three away. See the module doc.
    //
    // Read read-only rather than inside the `edit` closure below: this must
    // not hold the `shep.toml` lock across a ten-second child process, or a
    // hanging hook would block every other shep invocation on this home.
    let hook = match ShepToml::adopted_dog_path_readonly(&paths.daemon_config, name) {
        Ok(Some(exec)) => Some(hook::run_on_remove(&exec, &paths.home).await),
        // A built-in dog, or a name never adopted. Nothing to run.
        _ => None,
    };
    ...
}
```

Then the existing body, unchanged. `hook` is rendered afterwards, so a hook that hung has already been killed before `shep.toml` is touched.

**The lock point is not optional.** `ShepToml::edit` holds an exclusive `flock` on `shep.toml.lock` for the whole closure (`shep_toml.rs:204`). Running a ten-second child inside it would make one wedged dog block every concurrent `shep enable`, `shep disable` and `shep adopt` on that home for ten seconds. `adopted_dog_path_readonly` (`shep_toml.rs:343`) exists and takes no lock, which is why it is the right read here.

- [ ] **Step 4: Render the report**

Two halves, and they go to different places on purpose.

The **operator-facing** half rides `Streams::aside`, gated on `quiet`. `aside` is stderr, which is what `adopt` already uses for its group-writable warning (`dogs.rs:583`) and for the same reason: it is worth knowing about the run without being the answer that was asked for, and keeping it off stdout is what lets `shep rehome nosy --format json | jq` keep working.

```rust
const HOOK_NOTICE: &str = "on_remove";

fn tell(streams: &mut Streams<'_>, quiet: bool, name: &str, run: &HookRun) {
    if quiet {
        return;
    }
    let said = match &run.outcome {
        HookOutcome::Done if run.output.trim().is_empty() => {
            format!("{name}'s on-remove hook ran and said nothing")
        }
        HookOutcome::Done => format!("{name}'s on-remove hook said: {}", run.output.trim()),
        // NOT an error, and the wording has to carry that. Every dog that
        // exists predates this hook.
        HookOutcome::Refused { status } => format!(
            "{name} does not implement an on-remove hook (exit {status}). \
             Nothing was put back; {name} is still removed. It said: {}",
            run.output.trim()
        ),
        HookOutcome::TimedOut { after } => format!(
            "{name}'s on-remove hook was still running after {}s and was stopped. \
             {name} is still removed, and whatever the hook had not finished \
             was not finished.",
            after.as_secs()
        ),
        HookOutcome::Signalled { signal } => format!(
            "{name}'s on-remove hook was killed by signal {signal}. \
             {name} is still removed."
        ),
        HookOutcome::WillNotExec { reason } => format!(
            "{name}'s on-remove hook could not be run ({reason}). \
             {name} is still removed."
        ),
    };
    let said = if run.truncated {
        format!("{said} (output truncated)")
    } else {
        said
    };
    streams.aside(HOOK_NOTICE, &said);
}
```

The **machine-facing** half is a new `DogRehomedRow` field, `hook: Option<String>`, carrying the same sentence. `Render` requires `headers`, `rows`, `json_key_for` and `JSON_ONLY` to stay in lockstep, and `assert_no_drift` (`rows.rs:1883`) will fail loudly if they do not. **Put `hook` in `JSON_ONLY`**: it is a paragraph, and a paragraph in a table cell is what the adaptive renderer is worst at. `PRIORITIES` is unchanged, and the pin at `rows.rs:2821` should stay green as a result.

- [ ] **Step 5: Plumb `quiet` and correct its help text**

`crates/shep-cli/src/lib.rs:1318` passes `cli.global.quiet` through. `crates/shep-cli/src/cli.rs:128-131` currently reads:

> Currently narrows `bleats`' own notices (a dropped-events count, a daemon-shutdown notice, ...): diagnostics distinct from a sheep's own line or a real error, both of which still print regardless.

It becomes a two-command list: `bleats`' own notices, and `rehome`'s report of what a dog's on-remove hook said. Keep the closing clause; it is still true.

- [ ] **Step 6: Write the unit tests the e2e tier cannot cover cheaply**

In `crates/shep-cli/src/commands/dogs.rs`'s `mod tests`, built on
`rehome_with_no_shepherd_writes_the_config_and_exits_zero`'s shape
(`dogs.rs:1729-1747`): a `tempfile::tempdir`, `ShepPaths::resolve`, a seeded
`ShepToml`, and the module's existing `streams(&mut out, &mut err)` helper.
All three run with no shepherd, which `rehome` already tolerates, so the
only thing under test is the hook and its report.

```rust
    /// Adopts a real script as `nosy` under `paths`, so `rehome` has
    /// something to run. `body` is the script's `on-remove` behaviour.
    fn adopt_a_hook(paths: &ShepPaths, dir: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let exec = dir.join("shep-nosy");
        std::fs::write(&exec, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();
        ShepToml::edit(&paths.daemon_config, |seed| seed.adopt_dog("nosy", &exec)).unwrap();
    }

    /// fails if `--quiet` stops governing the hook report. It is exactly
    /// the "non-essential output" the flag's own help text describes, and
    /// shep-deploy's prerequisite list was written assuming `--quiet`
    /// already covered any notice. It did not; this is what makes that
    /// sentence true.
    ///
    /// The second half is the half that could rot: `--quiet` must silence
    /// the hook report and nothing else, so the removal itself still has to
    /// land and still has to exit 0.
    #[tokio::test]
    async fn quiet_silences_the_hook_report_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        adopt_a_hook(&paths, dir.path(), "echo 'web put back at /home/rin/ReactMap'");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(&mut streams(&mut out, &mut err), &paths, "nosy", true).await;

        let printed = String::from_utf8(out).unwrap() + &String::from_utf8(err).unwrap();
        assert!(
            !printed.contains("put back"),
            "--quiet must silence the hook report: {printed}"
        );
        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        let cfg = shep_core::config::DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(
            !cfg.daemon.adopted_dogs.contains_key("nosy"),
            "and it must silence the report ONLY, not the removal: {written}"
        );
    }

    /// fails if a hook that refused is reported in words an operator would
    /// read as a failure. `Refused` is the ordinary answer from every dog
    /// written before this hook existed -- `shep-log-rotate` answers an
    /// unknown argument with a usage message and exit 2 -- and telling
    /// somebody their removal went wrong when it went exactly right is
    /// worse than saying nothing at all.
    #[tokio::test]
    async fn a_refusal_is_reported_as_not_implemented_not_as_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        adopt_a_hook(&paths, dir.path(), "echo 'nosy takes no options.' >&2\nexit 2");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = rehome(&mut streams(&mut out, &mut err), &paths, "nosy", false).await;

        assert_eq!(code, ExitCode::Success, "a refusal is not a failed rehome");
        let printed = String::from_utf8(err).unwrap();
        assert!(
            printed.contains("does not implement an on-remove hook"),
            "the words matter as much as the exit code: {printed}"
        );
        assert!(
            printed.contains("is still removed"),
            "and it must say the removal happened anyway: {printed}"
        );
        assert!(
            printed.contains("takes no options"),
            "what the dog actually said is still worth showing: {printed}"
        );
    }

    /// fails if the hook's report leaks into stdout, which would break
    /// `shep rehome <name> --format json | jq` for every dog that prints
    /// anything. `adopt`'s group-writable notice already sets this
    /// precedent with the same reasoning, and `Streams::aside`'s own doc
    /// spells the rule out.
    #[tokio::test]
    async fn the_hook_report_goes_to_stderr_not_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        adopt_a_hook(&paths, dir.path(), "echo 'a line the operator should see'");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = rehome(&mut streams(&mut out, &mut err), &paths, "nosy", false).await;

        let on_out = String::from_utf8(out).unwrap();
        let on_err = String::from_utf8(err).unwrap();
        assert!(!on_out.contains("a line the operator should see"), "{on_out}");
        assert!(on_err.contains("a line the operator should see"), "{on_err}");
        // stdout still carries the command's own answer, which is the half
        // a pipe is reading for.
        assert!(on_out.contains("nosy"), "{on_out}");
    }
```

- [ ] **Step 7: Verify**

```bash
cargo test -p shep --lib --all-features -- rehom
```
Expected: PASS.

```bash
cargo test -p shep --test cli_e2e --all-features -- rehom on_remove
```
Expected: PASS, two new tests.

Then the mutations, after `cp crates/shep-cli/src/commands/dogs.rs /tmp/dogs.rs.bak && shasum /tmp/dogs.rs.bak`:

| mutation | expected |
|---|---|
| move the hook call below the `ShepToml::edit` block | `the_on_remove_hook_runs_before_anything_is_forgotten` red, printing `ALREADY FORGOTTEN` |
| move the hook call inside the `edit` closure | the ordering test still passes and nothing catches it. **This is a known gap.** The lock-holding hazard is argued in Step 3 and is not test-covered; say so in the report rather than inventing a test that would need two concurrent `shep` processes racing a ten-second child. |
| turn a `Refused` outcome into an early `streams.fail(...)` | `rehoming_a_dog_with_no_hook_still_removes_it_and_exits_zero` red on the status |
| ignore `quiet` in `tell` | `quiet_silences_the_hook_report_and_nothing_else` red |
| route the report through `streams.note` instead of `aside` | `the_hook_report_goes_to_stderr_not_stdout` red |

- [ ] **Step 8: Commit**

```bash
git add crates/shep-cli/src/commands/dogs.rs crates/shep-cli/src/output/rows.rs \
        crates/shep-cli/src/lib.rs crates/shep-cli/src/cli.rs crates/shep-cli/tests/cli_e2e.rs
git commit -F- <<'EOF'
feat(cli): rehome runs the dog's on-remove hook first

A dog that takes a sheep over owes the operator a way back, and until now
there was no moment at which it could give one. shep-deploy re-registers a
sheep with its `cwd` under `$SHEP_HOME`; rehome it and the app keeps
running from a path its owner has no reason to know about, while the
directory they think it lives in sits untouched. The dog has had a working
`on-remove` for a while. Nothing called it.

The hook runs BEFORE the config edit and before the DisableDog RPC, which
is a third position rather than a break with this module's config-first
rule, and the module doc now says so. The hook needs the dog still
adopted, still running, and a shepherd still up: it connects to the socket
and issues Delete and Start for every sheep it moved. Steps two and three
take all three of those away.

It is read through `adopted_dog_path_readonly`, deliberately, rather than
from inside the `edit` closure. `ShepToml::edit` holds an exclusive flock
on `shep.toml.lock` for the whole closure, so a ten-second child inside it
would let one wedged dog block every concurrent enable, disable and adopt
on that home.

Nothing the hook does changes the exit code. It refused, it hung, it could
not be run: the dog is removed either way and the report says what
happened. An operator asking to remove something is entitled to have it
removed, and shep arguing on a dog's behalf about its own uninstallation is
not a state worth being able to reach.

The report goes to stderr through `Streams::aside`, the same channel and
the same argument as `adopt`'s group-writable warning: worth knowing about
the run, not the answer that was asked for, and keeping it off stdout is
what lets `--format json | jq` keep working. `--format json` gets it as a
`hook` field, JSON-only because it is a paragraph and a paragraph is what
the adaptive table renderer is worst at.

`--quiet` now governs it, which needed the flag plumbed into this verb and
its help text corrected. That text said "Currently narrows bleats' own
notices" and was accurate; the list is two commands long now.

The ordering is pinned by an e2e test with a real dog binary that greps
`shep.toml` from inside its own hook, because a stubbed hook has no world
to look at and cannot tell that the file was already rewritten. rehome had
no e2e coverage at all before this.
EOF
```

---

### Task 4: Exit codes 12 and 13, reserved rather than defined

**Files:**
- Modify: `crates/shep-cli/src/exit.rs` (module doc `:1-3`, a new const, a new test)
- Modify: `docs/specs/shep-v1.md` (§9's table at `:401-414`, the prose at `:416-426`, and the stale line at `:451`)
- Test: `crates/shep-cli/src/exit.rs`'s `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub(crate) const DOG_RESERVED_FROM: u8 = 12;` in `crates/shep-cli/src/exit.rs`. Nothing else depends on it.

**I am departing from how the ledger worded this, and the departure is the point of the task.** `shep-deploy`'s ledger says "shep's `exit.rs` and `shep-v1.md` section 9 now owe rows for BOTH 12 and 13". Section 9 does. `exit.rs` does **not**, and adding them there would be wrong:

- `ExitCode` is the taxonomy of exits **the shep process itself produces**. Every variant has a code path that returns it. `shep` has no path that means "the deploy rolled back", and adding a variant nothing constructs is dead surface that `every_exit_code_has_its_own_machine_readable_spelling` (`exit.rs:244`) would then have to list.
- What is actually at risk is a **collision**, and the collision is real because of the passthrough. `shep <dogname> [args]` propagates the dog's exit code verbatim (`crates/shep-cli/src/lib.rs:397`, `dog_exit_code`), so `shep deploy web` really does exit 12 on an operator's terminal. If shep ever gave 12 its own meaning, those two would be indistinguishable at the only place anybody reads them.
- So the useful artifact is a **reservation that cannot be taken by accident**: spec §9 records 12 and 13 and says the range belongs to dogs, and `exit.rs` carries a test that goes red the day somebody adds a twelfth shep code.

That is strictly stronger than two unused variants would have been. If Rin wants the variants anyway, it is a five-line change on top and this task is where it goes.

**This task also fixes a contradiction inside the spec's own section 9.** `docs/specs/shep-v1.md:451` still reads "(fail-fast exit code 2)" for `runtime`, while `:414` and `:422-426` of the same file give `runtime` code 11 and explain at length why it is not 2. Nobody has noticed because nothing cross-checks the table against anything. Fix it in the same commit; it is the same table.

**Where a fake would be too kind.** The existing suite pins almost nothing here. `every_rpc_error_code_maps_to_a_distinct_nonzero_exit_code` (`exit.rs:216`) pins distinctness, not numbers. `every_exit_code_has_its_own_machine_readable_spelling` (`exit.rs:244`) pins the shape of the strings and its own doc says the exact words are somebody else's job. **Exactly one test pins a number**: `the_already_running_exit_code_matches_the_clients_constant` (`exit.rs:280`), and it pins only 10. Codes 0, 1, 6, 8 and 11 are pinned nowhere at all, so a hand-edit of any discriminant passes the entire workspace. A new test asserting "no variant is 12 or 13" would inherit that weakness: it would pass just as well if every other number had shifted underneath it. **Pin the whole ladder**, not the ceiling.

- [ ] **Step 1: Write the failing test**

In `crates/shep-cli/src/exit.rs`'s `mod tests`.

```rust
    /// fails if any shep exit code reaches the range dogs own, and fails if
    /// the ladder below it moves at all.
    ///
    /// Both halves matter. `shep <dogname> [args]` passes an adopted dog's
    /// exit code straight through (`lib.rs`'s `dog_exit_code`), so
    /// `shep deploy web` exiting 12 is shep-deploy speaking, not shep. The
    /// day shep gives 12 its own meaning, the two become indistinguishable
    /// at the only place anybody reads them.
    ///
    /// The exact numbers are pinned here rather than just the ceiling
    /// because a ceiling-only check passes just as happily if everything
    /// under it shifted by one. Before this test, exactly one number in
    /// this enum was pinned anywhere in the workspace (10, by
    /// `the_already_running_exit_code_matches_the_clients_constant`), and
    /// 0, 1, 6, 8 and 11 were pinned nowhere.
    #[test]
    fn the_exit_ladder_is_pinned_and_stops_below_the_range_dogs_own() {
        let ladder = [
            (ExitCode::Success, 0),
            (ExitCode::Failure, 1),
            (ExitCode::Usage, 2),
            (ExitCode::NotFound, 3),
            (ExitCode::InvalidConfig, 4),
            (ExitCode::DaemonUnreachable, 5),
            (ExitCode::ProtocolMismatch, 6),
            (ExitCode::SpawnFailed, 7),
            (ExitCode::DeadlineExceeded, 8),
            (ExitCode::Internal, 9),
            (ExitCode::DaemonAlreadyRunning, 10),
            (ExitCode::FlockEmpty, 11),
        ];
        for (code, expected) in ladder {
            assert_eq!(
                code as u8, expected,
                "{code:?} moved; docs/specs/shep-v1.md section 9 is the source \
                 of truth and every restatement listed there has to move with it"
            );
            assert!(
                (code as u8) < DOG_RESERVED_FROM,
                "{code:?} reaches into the range reserved for dogs. \
                 shep-deploy uses 12 for a rolled-back deploy and 13 for a \
                 cutover that landed and could not tidy up, and \
                 `shep <dogname>` passes those straight through."
            );
        }
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep --lib --all-features -- the_exit_ladder_is_pinned
```
Expected: FAIL to compile, "cannot find value `DOG_RESERVED_FROM`".

- [ ] **Step 3: Add the reservation**

In `crates/shep-cli/src/exit.rs`, immediately after the `ExitCode` enum:

```rust
/// The first exit code shep will never assign to a reason of its own.
///
/// 12 and up belong to adopted dogs. This is not a courtesy: `shep
/// <dogname> [args]` runs a dog directly and hands its exit code back
/// unchanged (`crate::dog_exit_code`), so a dog's numbers surface on an
/// operator's terminal under the `shep` name and share one space with
/// shep's own.
///
/// Taken so far, both by `shep-deploy`:
/// - `12` a deploy was rejected and the previous release was put back
/// - `13` a first cutover landed and then could not tidy up: the sheep is
///   live on the new release, and instances the cutover could not delete
///   are still registered and running their pre-adoption spec
///
/// Neither is a variant of [`ExitCode`], deliberately: shep has no code
/// path that means either, and a variant nothing constructs is surface
/// without behaviour. What this const buys is that a future twelfth shep
/// code cannot take 12 quietly --
/// `the_exit_ladder_is_pinned_and_stops_below_the_range_dogs_own` fails
/// the moment one does.
///
/// `docs/specs/shep-v1.md` section 9 is the registry. A dog claiming a new
/// number records it there.
pub(crate) const DOG_RESERVED_FROM: u8 = 12;
```

Then extend the module doc at `exit.rs:1-3` with one sentence pointing at it, so a reader who opens this file to add a variant meets the constraint before the enum rather than after.

- [ ] **Step 4: Run the test**

```bash
cargo test -p shep --lib --all-features -- exit
```
Expected: PASS.

- [ ] **Step 5: Update spec §9**

`docs/specs/shep-v1.md`. Append two rows to the table at `:401-414`, keeping the existing column style (the Name column uses **spaces**, not the underscores `code_str` uses; do not "fix" that here, it is a separate question and this task should not smuggle it in):

```
| 12 | rolled back | Reserved for dogs. `shep-deploy`: the deploy was rejected and the previous release was put back. |
| 13 | stranded | Reserved for dogs. `shep-deploy`: a first cutover landed and then could not tidy up. |
```

Add a paragraph after the two that already follow the table (`:416-426`), in their voice:

> **12 and up are reserved for dogs.** `shep <dogname> [args]` runs an adopted dog directly and returns its exit code unchanged, so a dog's numbers arrive on an operator's terminal under the `shep` name and share this space. shep will not assign itself a code of 12 or higher; `crates/shep-cli/src/exit.rs`'s `DOG_RESERVED_FROM` and its test are what keep that true. A dog claiming a number records it in the table above.

And on the same pass, **fix `:451`**: it says `(fail-fast exit code 2)` for `runtime`, contradicting `:414` and the whole argument at `:422-426` in the same section. It is 11.

- [ ] **Step 6: Verify and commit**

```bash
cargo test -p shep --lib --all-features -- exit
```
Expected: PASS.

Mutation, after `cp crates/shep-cli/src/exit.rs /tmp/exit.rs.bak && shasum /tmp/exit.rs.bak`:

| mutation | expected |
|---|---|
| change `FlockEmpty = 11` to `= 12` | the new test red on **both** assertions, which is the point of pinning the ladder rather than only the ceiling |
| swap `ProtocolMismatch` and `SpawnFailed`'s numbers | the new test red; **before this task, nothing in the workspace catches this** |
| set `DOG_RESERVED_FROM = 14` | nothing goes red. Known and accepted: the const is a declaration, and the test that gives it teeth is the ladder. Say so in the report. |

```bash
git add crates/shep-cli/src/exit.rs docs/specs/shep-v1.md
git commit -F- <<'EOF'
docs(spec): reserve exit codes 12 and up for dogs, and pin the ladder

shep-deploy claims 12 for a deploy that rolled back and 13 for a cutover
that landed and could not tidy up. Rin chose both, on the reasoning that a
script has to tell three outcomes apart: it worked, it worked but needs
tidying, it failed. That matters most to the poll loop, which runs
unattended and would otherwise treat a landed deploy as a failure and retry
it.

They are RESERVED here, not defined. shep has no code path meaning either,
and an `ExitCode` variant nothing constructs is surface without behaviour.
What is actually at risk is a collision, and the collision is real: `shep
<dogname> [args]` passes an adopted dog's exit code straight through, so
`shep deploy web` exiting 12 happens on a real terminal under the `shep`
name. Giving 12 a shep meaning would make the two indistinguishable exactly
where somebody reads them.

The ladder is pinned as well as the ceiling, because a ceiling-only check
passes just as happily if everything under it shifted by one. That turned
out to matter more than expected: before this commit, exactly ONE number in
this enum was pinned anywhere in the workspace, 10, by the cross-crate
equality with shep-client's DAEMON_ALREADY_RUNNING. 0, 1, 6, 8 and 11 were
pinned nowhere, and swapping any two discriminants passed the whole suite.

Also fixes a contradiction inside section 9 itself. Line 451 said `runtime`
fail-fasts with exit code 2, while the table twelve lines above it and the
two paragraphs after it give it 11 and spend a paragraph explaining why it
is not 2. Nothing cross-checks that table against anything, which is how it
survived.
EOF
```

---

### Task 5: The reload response carries its own deadline

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs` (`ProcessInfo` `:411-482`, `ProcessInfoBuilder` `:536-613`)
- Modify: `crates/shep-daemon/src/entry.rs` (a field), `crates/shep-daemon/src/supervisor.rs` (`to_info` `:5068`, `arm_reload_deadline` `:3570`)
- Modify: `crates/shep-cli/src/whistle/facts.rs` (`SheepRow` `:58-90`, its `From` at `:131`)
- Modify: `crates/shep-core/src/protocol/snapshots/*.snap`, `crates/shep-cli/tests/fixtures/*.json`, `crates/shep-client/src/testing.rs`
- Test: `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/supervisor.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `ProcessInfo::reload_deadline_ms: Option<u64>`, plus the matching `ProcessInfoBuilder::reload_deadline_ms` setter. Task 7 does not use it; nothing else in this plan does.

**Read this section before writing any code. The ledger's premise for this item is wrong, and the correction changes the design.**

`shep-deploy`'s ledger records Rin's decision as: "shep RETURNS the reload deadline on the reload response... Additive field, so no `PROTOCOL_VERSION` bump." The second half does not survive contact with the response's actual shape.

`Response::Reloading(Vec<ProcessInfo>)` (`crates/shep-core/src/protocol/request.rs:872`) is a **tuple variant**, under `#[serde(tag = "kind", content = "data")]` (`request.rs:841-845`). On the wire it is:

```json
{ "kind": "reloading", "data": [] }
```

pinned at `crates/shep-core/src/protocol/snapshots/shep_core__protocol__request__tests__reply_wire_v1.snap:192-197`. Giving it a sibling field means `data` stops being an array and becomes an object. `crates/shep-core/src/protocol/mod.rs:25-33` classifies that exactly:

> Evolution rule: ADDITIVE optional fields (new serde-defaulted `Option<T>` fields, new variants behind `#[non_exhaustive]`) keep the version. Removing, renaming, or **retyping anything serialized** bumps it.

So the shape Rin was told about is a `PROTOCOL_VERSION` bump, and the handshake compares versions for **equality** (`crates/shep-daemon/src/server.rs:422`, `crates/shep-client/src/connection.rs:166`), so a bump means every published client stops talking to every published daemon. The `last_exit` precedent does not transfer: that was a new `Option` field on an existing struct, which is a different move.

**Three ways out. Take (c).**

| | shape | protocol version | what it costs |
|---|---|---|---|
| (a) | `Reloading { flock, deadline_ms }` | **bumps to 2** | every 0.1.x client and daemon stop interoperating. For one advisory number. |
| (b) | a new `Response::ReloadingWithDeadline` variant | stays 1 | the daemon must choose which to send, and an old client meeting the new variant fails to deserialize, so choosing needs a per-request opt-in, which is a `Request` change too. Two variants meaning one thing, forever. |
| (c) | `ProcessInfo::reload_deadline_ms: Option<u64>` | **stays 1** | one more field on a struct that is returned by more verbs than reload |

**(c) is not a workaround, it is the better answer, and it closes more of the coupling than (a) would.** `arm_reload_deadline` (`supervisor.rs:3570-3583`) computes the deadline **per replacement instance**, from that instance's own registered spec:

```rust
let deadline = app.listen_timeout.as_duration()
    + app.graceful_timeout.as_duration()
    + RELOAD_DEADLINE_SLACK;
```

`Response::Reloading` already carries one `ProcessInfo` per affected instance. So a per-`ProcessInfo` field hands the dog exactly what the daemon armed, one per instance, and it kills **all three** of the dog's inferred inputs at once rather than one of them:

| what shep-deploy infers today | fixed by (a) | fixed by (c) |
|---|---|---|
| the formula and the 5s slack, copied from `supervisor.rs:3581` | yes | yes |
| `listen_timeout` and `graceful_timeout`, read from the release's Flockfile, which may not be what shep registered | **no** | **yes** |
| the instance count, worked out from a live pid capture | no, still multiplies | **yes**, one entry per instance |

That third row is the one the `shep-deploy` ledger left open after Task 10's closeout ("the two timeouts still come from a file that may not be what shep registered"). A single scalar on the response would not have closed it, because it would still be one number for a set of instances the dog has to count.

**The field is meaningful outside a reload, which is what makes it honest on `ProcessInfo`.** It is not "the deadline of the reload you just asked for". It is "how long a swap of this instance is allowed to take", derivable from its registered spec at any moment. `ListFlock` and `Describe` may as well answer it too, and a dog deciding whether it can afford a reload before asking for one is a real use.

**For Rin.** The decision recorded in the ledger was made against a description of the wire that was wrong. (c) delivers what she asked for without the bump and closes two more inferred inputs; (a) is what she was actually described and costs a protocol break. **This plan implements (c). If she wants (a), the plan needs re-cutting around a `PROTOCOL_VERSION` bump, which is a much larger piece of work than this task.**

**Where a fake would be too kind.** A unit test that builds a `ProcessInfo` with a deadline and reads it back proves the field exists and nothing else. The property that matters is that **the number shep hands out equals the number shep armed**, and those are computed in two different functions. A test that hardcodes 16000 on both sides would stay green if `arm_reload_deadline` changed and `to_info` did not, which is the exact drift this field exists to remove and which the dog's own copied formula suffered for five review rounds. Derive both from one function and pin **that**.

- [ ] **Step 1: Write the failing tests**

In `crates/shep-core/src/protocol/request.rs`'s `mod tests`, alongside the sibling compat tests at `:1195`, `:1255` and `:1308`:

```rust
    /// fails if the new field breaks an older peer. A daemon that predates
    /// it sends no `reload_deadline_ms` key, and this must decode to `None`
    /// rather than erroring, which is what keeps `PROTOCOL_VERSION` at 1.
    /// Serde's derive special-cases a syntactically `Option<...>` field, so
    /// no `#[serde(default)]` is needed or wanted -- see
    /// `a_process_info_without_a_last_exit_key_still_deserializes`, whose
    /// doc is the empirical proof of that.
    #[test]
    fn a_process_info_without_a_reload_deadline_key_still_deserializes() {
        let json = r#"{"id":1,"name":"web","status":"online","pid":42,
                       "restarts":0,"uptime_ms":10,"fold":null,
                       "out_file":null,"err_file":null,"cpu_percent":null,
                       "memory_bytes":null,"dog":null,"lambs":null,
                       "last_exit":null}"#;
        let info: ProcessInfo = serde_json::from_str(json).expect("decode");
        assert_eq!(info.reload_deadline_ms, None);
    }
```

And in `crates/shep-daemon/src/supervisor.rs`'s `mod tests`, the one that actually matters:

```rust
    /// fails if the deadline shep REPORTS stops matching the deadline shep
    /// ARMS. Two different functions compute it -- `arm_reload_deadline`
    /// starts the watchdog, `to_info` answers the question -- and a change
    /// to one without the other hands a caller a number shep does not
    /// honour. That is precisely the drift this field exists to remove:
    /// shep-deploy carried a hand-copy of this formula for five review
    /// rounds, and the whole argument for returning it is that a copy
    /// cannot notice.
    ///
    /// Both sides are derived from `reload_deadline_for`, so this is a
    /// tautology unless one of them stops calling it -- which is exactly
    /// the failure being guarded.
    #[test]
    fn the_reported_reload_deadline_is_the_one_the_watchdog_arms() {
        // an app with NON-DEFAULT timeouts, so a hardcoded 16000 anywhere
        // fails rather than coincidentally matching
        let app = app_with(UpDuration::from_millis(1_500), UpDuration::from_millis(20_000));
        let entry = entry_for(&app);

        let armed = reload_deadline_for(entry.spec.config());
        let reported = to_info(&entry).reload_deadline_ms.expect("reported");

        assert_eq!(reported, u64::try_from(armed.as_millis()).expect("fits"));
        assert_eq!(reported, 1_500 + 20_000 + 5_000);
    }
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep-core --lib --all-features -- reload_deadline
```
Expected: FAIL to compile, no field `reload_deadline_ms`.

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow:: reload_deadline
```
Expected: FAIL to compile, no `reload_deadline_for`.

- [ ] **Step 3: Add the field**

In `crates/shep-core/src/protocol/request.rs`, after `last_exit` (`:481`), following that field's declaration exactly: a plain `Option<u64>`, **no `#[serde(default)]`**.

```rust
    /// How long a reload of this instance is allowed to take, in whole
    /// milliseconds, or `None` from a daemon that predates the field.
    ///
    /// `listen_timeout + graceful_timeout + RELOAD_DEADLINE_SLACK`, from
    /// this instance's own registered spec. It is the deadline the
    /// supervisor really arms when it swaps this instance, not an estimate
    /// of one: both come from the same function.
    ///
    /// Per instance rather than per reload, because that is how the
    /// supervisor arms it and because instances are replaced one at a time.
    /// A caller waiting out a whole reload sums the entries it was given
    /// rather than multiplying one number by an instance count it had to
    /// work out for itself.
    ///
    /// Reported by every verb that returns a `ProcessInfo`, not only by a
    /// reload: it is a property of the instance's configuration, answerable
    /// at any moment, and a caller deciding whether it can afford a reload
    /// before asking for one is a real use.
    ///
    /// Milliseconds as `u64` is this wire's convention for a duration the
    /// protocol itself invents (`uptime_ms`, `Envelope::deadline_ms`);
    /// `UpDuration`'s string form is for durations mirrored out of
    /// `AppConfig`, which this is not.
    pub reload_deadline_ms: Option<u64>,
```

Update `ProcessInfo`'s type doc at `:398-410`, which enumerates the additive growth waves, to name this one as the sixth. Add the builder setter beside `last_exit`'s at `:603`, and the `build()` default at `:517`.

- [ ] **Step 4: Give the formula one home**

In `crates/shep-daemon/src/supervisor.rs`, extract from `arm_reload_deadline` (`:3573-3583`):

```rust
/// How long one swap of `app` is allowed to take.
///
/// The one home for this number. `arm_reload_deadline` starts a watchdog
/// on it and `to_info` reports it to a caller, and they must not be able
/// to disagree -- a caller told one budget and held to another is the
/// failure this whole field exists to remove.
pub(crate) fn reload_deadline_for(app: &AppConfig) -> Duration {
    app.listen_timeout.as_duration() + app.graceful_timeout.as_duration() + RELOAD_DEADLINE_SLACK
}
```

`arm_reload_deadline` calls it. `to_info` (`:5068`) calls it too:

```rust
        .reload_deadline_ms(u64::try_from(
            reload_deadline_for(entry.spec.config()).as_millis(),
        ).ok())
```

`try_from(...).ok()` rather than a saturating cast: neither timeout is clamped by `normalize` (verified -- `normalize.rs` bounds `action_timeout` and the liveness interval and nothing else, and `crates/shep-daemon/src/extras.rs:1872` sets a `listen_timeout` of `6h` in a test), so an operator can write a value large enough to overflow. `None` there means "shep cannot express this", which reads correctly at the caller and is better than a wrong number.

**`entry.rs` needs no new field.** The deadline is derived from `entry.spec.config()`, which `to_info` already holds. Do not cache it on `ProcessEntry`: a cached copy is a second place for it to live, which is the thing being fixed.

- [ ] **Step 5: Re-accept the wire snapshots, and READ them**

```bash
cargo insta test --accept -p shep-core
```

Then **open each changed `.snap` and read it** before staging. Three are expected to move: `reply_wire_v1.snap`, `request_wire_v1.snap`, `bus_event_wire_v1.snap` (`ProcessInfo` rides `BusEvent::Process`). What you are checking is that the ONLY change is a new `"reload_deadline_ms"` key. IR-35 calls a wire change a version bump plus a CHANGELOG entry, "never a silent snapshot re-accept", and reading them is what makes the re-accept not silent.

Also update, by hand: `crates/shep-cli/tests/fixtures/{flock,describe,start}.json`, `crates/shep-client/src/testing.rs`'s sample `ProcessInfo`, and `crates/shep-cli/src/whistle/facts.rs`'s `SheepRow` plus its `From<&ProcessInfo>` at `:131` (it derives `JsonSchema`, so the MCP tool schema moves too and `docs/whistle/tools.md` is regenerated in Task 9).

- [ ] **Step 6: Verify**

```bash
cargo test -p shep-core --lib --all-features
```
Expected: PASS.

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```
Expected: PASS.

Mutations, `cp` plus `shasum` first:

| mutation | expected |
|---|---|
| make `to_info` compute the sum inline instead of calling `reload_deadline_for` | nothing red **yet**; then change `RELOAD_DEADLINE_SLACK` to 6s and `the_reported_reload_deadline_is_the_one_the_watchdog_arms` goes red. **Run both halves**: the first alone proves nothing, and a report claiming the mutation was caught without the second step would be the claimed-evidence-that-does-not-reproduce failure this project has already had once. |
| drop `graceful_timeout` from `reload_deadline_for` | the deadline test red, and several existing reload tests too. Note the radius. |
| add `#[serde(default)]` to the new field | nothing red, and that is correct: it is redundant on an `Option`, not wrong. Do not add it; the tree has none anywhere and consistency is the reason. |
| have `to_info` report `Some(0)` on overflow instead of `None` | not covered. Add a case to the deadline test with `listen_timeout = "6h"` twice over if `u64` can be made to overflow; if it cannot in practice, say so plainly rather than shipping an untested arm. |

- [ ] **Step 7: Commit**

```bash
git add crates/shep-core/src/protocol/request.rs crates/shep-core/src/protocol/snapshots/ \
        crates/shep-daemon/src/supervisor.rs crates/shep-cli/src/whistle/facts.rs \
        crates/shep-cli/tests/fixtures/ crates/shep-client/src/testing.rs
git commit -F- <<'EOF'
feat(core,daemon): report each instance's reload deadline

shep arms a watchdog on every reload swap for `listen_timeout +
graceful_timeout + RELOAD_DEADLINE_SLACK` and has never told anyone the
number. A caller waiting out a reload therefore has to reconstruct it.
shep-deploy does exactly that: a hand-copy of the formula and the 5s
constant, in a crate that cannot see this source, with a comment naming the
file and line it was copied from. Nothing makes the two fail together, and
it took five review rounds to arrive at a formula that was right, because
the first four were wrong in ways only a real reload could show.

Returned per `ProcessInfo` rather than on the `Reloading` response, and the
shape is the whole design rather than a workaround.

`Response::Reloading(Vec<ProcessInfo>)` is a tuple variant under
`#[serde(tag, content)]`, so `data` is an array. Giving it a sibling field
turns that array into an object, which `protocol/mod.rs`'s own evolution
rule calls retyping something serialized: a PROTOCOL_VERSION bump. The
handshake compares versions for equality, so that would stop every
published client talking to every published daemon, in exchange for one
advisory number.

A field on `ProcessInfo` is additive under `Option` on the same terms as
`last_exit`, so the version stays 1 and a peer that predates the field
neither sends nor expects the key.

It also closes more than the response would have. The deadline is armed per
replacement instance, from that instance's own registered spec, and
`Reloading` already carries one `ProcessInfo` per affected instance. So a
caller gets what shep really armed, one per instance, and stops inferring
three separate things rather than one: the formula, the two timeouts (which
shep-deploy read from a release's Flockfile that may not be what shep
registered), and the instance count (which it worked out from a live pid
capture). Summing what it was handed needs none of them.

The formula now has one home, `reload_deadline_for`, called by both the
function that arms the watchdog and the function that reports it. A caller
told one budget and held to another is the failure this field exists to
remove, so the two must not be able to disagree, and the test derives both
sides from it with non-default timeouts so a hardcoded 16000 cannot pass by
coincidence.

Reported by every verb returning a ProcessInfo, not only by reload: it is a
property of the instance's configuration, answerable at any moment, and
deciding whether you can afford a reload before asking for one is a real
use.

`u64::try_from(...).ok()` rather than a saturating cast, because neither
timeout is clamped by `normalize` and an operator can write `6h`. None
reads as "shep cannot express this", which is true, where a saturated
number would be false.
EOF
```

---

### Task 6: Smits on the wire, and in the daemon

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs` (`Request`, `Response`, `ProcessInfo`, a new `Smit` type), `crates/shep-core/src/protocol/mod.rs` (re-export)
- Modify: `crates/shep-daemon/src/server.rs` (`handle_conn` `:342`, `converse` `:359`, `read_loop` `:381`), `crates/shep-daemon/src/rpc.rs` (`RpcContext` `:58`, `run` `:215`, a handler), `crates/shep-daemon/src/supervisor.rs` (`Command`, `handle_command`, `to_info` `:5068`)
- Modify: `crates/shep-core/src/protocol/snapshots/*.snap`, `crates/shep-cli/tests/fixtures/*.json`, `crates/shep-client/src/testing.rs`, `crates/shep-cli/src/whistle/facts.rs`
- Test: `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/supervisor.rs`, `crates/shep-daemon/tests/daemon_e2e.rs`

**Interfaces:**
- Consumes: nothing. Independent of Task 5, though both touch `ProcessInfo`; if they run in parallel expect a merge in `request.rs` and re-accept the snapshots once.
- Produces:
  ```rust
  // shep-core
  pub struct Smit(String);                       // validating newtype
  impl Smit { pub const MAX_CHARS: usize = 48; }
  impl core::str::FromStr for Smit { type Err = SmitError; }
  pub enum SmitError { TooLong { chars: usize }, Unprintable, Empty }

  Request::SetSmit { sheep: String, smit: Option<Smit> }   // None clears
  Response::SmitPainted(Vec<ProcessInfo>)
  ProcessInfo::smit: Option<String>
  ```
- Task 7 consumes `ProcessInfo::smit` and nothing else.

**A smit is not a shep concept and shep must not learn one.** shep stores a string and paints it. It does not know what `▲ main@a1b2c3` means, will never parse it, and has no opinion about its content beyond "it fits in a table cell and cannot drive a terminal". That is the whole reason this is a general mechanism rather than a deploy feature: `docs/terminology.md` gains a row, and the daemon gains no vocabulary.

**Keyed by sheep NAME, not by instance id.** `shep-deploy` publishes `set_smit(sheep, text)` and a sheep can run several instances. Storing one smit per `ProcessEntry` would mean fanning out at publish time and then keeping it in step as instances come and go, so an instance spawned five seconds after a publish would show nothing until the dog's next tick, thirty seconds later. A name-keyed map has none of that: every instance of a named sheep shows the same smit, including one spawned a moment ago.

**Ephemeral, and scoped to the CONNECTION that published it.** The design spec is explicit that persisting smits leaves `shep flock` showing a mark attributed to a dog that no longer exists, and that removing that orphan class must not cost "cleanup logic on every path that can stop a dog". Connection scope delivers exactly that with **one** cleanup site:

| how a dog stops | what happens to its socket | what happens to its smits |
|---|---|---|
| `shep disable`, `shep rehome` | daemon stops the process, socket closes | dropped |
| the dog crashes | socket closes | dropped |
| the daemon restarts | everything in memory goes | dropped |
| the dog reconnects deliberately | old socket closes | dropped, and republished on its next tick |

Every row is the same mechanism, and none of them is a code path anybody has to remember to edit. `ProcessEntry` is in-memory only with no `Serialize` (`crates/shep-daemon/src/entry.rs:16`) and the muster roll stores only `AppConfig` plus a running count (`crates/shep-daemon/src/snapshot.rs:62-78`), so nothing persists a smit even by accident.

**There is no connection identity in the daemon today, and this task adds the smallest one that works.** `RpcServer::serve` (`server.rs:136`) spawns `handle_conn` per connection with a cloned `RpcContext` and nothing distinguishing them. Mint a `ConnId(u64)` from an `AtomicU64` in `handle_conn` **after** `check_peer`, thread it through `converse` and `read_loop` into `rpc::run`, and forget that connection's smits in `handle_conn`'s tail. That tail is already the "runs on every path" block, and its existing comment says so in as many words -- it is where `drop(out_tx)` and the writer join live.

**Validated at ingress, refused rather than stripped.** A smit is written by a third party and printed into an operator's terminal. `crates/shep-cli/src/terminal_safe.rs:42-47` states the rule this repository already follows and why the table renderer is not a substitute: `output::width::sanitize_cell` deliberately **keeps** a well-formed CSI sequence, because shep's own colouring is made of them. So a smit carrying `\x1b[2J` would survive the renderer.

Refusing at the daemon rather than sanitising at the CLI, for three reasons:
1. **One place.** `ProcessInfo::smit` is read by `shep flock`, `shep describe`, `--format json`, the lookout TUI, the MCP tool schema and every `BusEvent::Process` subscriber. Sanitising at render means six places that each have to remember; refusing at ingress means the invariant holds for all of them by construction. That is the correction-reaching-one-site-and-missing-others failure, prevented rather than watched for.
2. **The publisher is a program.** A refusal it can see and fix beats silent mangling it cannot. `crates/shep-core/src/kv.rs` already sets this precedent for the same kind of value: `MAX_KEY_BYTES`, `MAX_VALUE_BYTES` and a key grammar at `kv.rs:183-187`, all refusals.
3. **It makes the renderer's job trivial.** If no smit can contain an ESC, `sanitize_cell` has nothing to keep and Task 7 needs no special case.

`Smit::MAX_CHARS = 48` counts **characters, not bytes and not display columns**, and the doc must say which and why: bytes would refuse a legitimate CJK smit at a third of its apparent length, and display columns are what the renderer measures rather than what the parser can cheaply promise. 48 is chosen against the reference smit `▲ main@a1b2c3` at 13, leaving room for a long branch name without letting one column dominate the table.

**Where a fake would be too kind.** The ephemerality is the whole feature, and **no unit test can observe it**: a supervisor test can call the forget path directly and prove the map empties, which is only a test of the function you already wrote. What must be shown is that **closing a real socket really reaches that function**, through `handle_conn`'s tail, through the reply path, through the actor's mailbox. That is `crates/shep-daemon/tests/daemon_e2e.rs`, with two real clients: one paints and disconnects, the other looks. Get this wrong and smits persist silently, which is the exact orphan class the design forbids and which every unit test in the file would happily call correct.

- [ ] **Step 1: Write the failing e2e test first**

In `crates/shep-daemon/tests/daemon_e2e.rs`, following `kill_daemon_shuts_the_flock_down_and_unlinks_the_socket` (`:1193`) for the harness shape.

```rust
/// fails if a smit outlives the connection that painted it.
///
/// This is the whole lifecycle decision and it cannot be unit-tested: a
/// supervisor test that calls the forget path proves only that the function
/// it just wrote does what it says. What has to hold is that CLOSING A REAL
/// SOCKET reaches it -- through `handle_conn`'s tail, the actor's mailbox,
/// and `to_info`. Persisting a smit means `shep flock` shows a mark
/// attributed to a dog that no longer exists, forever, with nothing to
/// clear it but a daemon restart.
#[tokio::test]
async fn a_smit_dies_with_the_connection_that_painted_it() {
    let daemon = start_daemon().await;
    start_sheep(&daemon, "web").await;
    const SMIT: &str = "\u{25b2} main@a1b2c3";

    // The observer outlives the painter, deliberately: the question is what
    // a DIFFERENT client sees before and after, so it must not be the
    // connection whose closing is under test.
    let looker = Client::connect(&daemon.socket).await.expect("connect");

    let painter = Client::connect(&daemon.socket).await.expect("connect");
    painter
        .request(Request::SetSmit {
            sheep: "web".to_owned(),
            smit: Some(SMIT.parse().expect("valid")),
        })
        .await
        .expect("paint");

    assert_eq!(
        smit_of(&looker, "web").await,
        Some(SMIT.to_owned()),
        "a smit must be visible to every client, not only its painter"
    );

    painter.close().expect("close");

    // The daemon notices asynchronously, so wait for the transition rather
    // than sampling once. A bare sleep here would be the flake this suite
    // has already paid for elsewhere.
    let gone = wait_until(Duration::from_secs(5), || async {
        smit_of(&looker, "web").await.is_none()
    })
    .await;
    assert!(gone, "the smit outlived the connection that painted it");
}

/// fails if a smit cannot drive an operator's terminal. The renderer is NOT
/// the guard: `output::width::sanitize_cell` deliberately keeps a
/// well-formed CSI sequence, because shep's own colouring is made of them.
/// The daemon refusing is what makes every downstream reader safe without
/// each of them remembering.
#[tokio::test]
async fn a_smit_carrying_an_escape_is_refused_at_the_daemon() {
    let daemon = start_daemon().await;
    start_sheep(&daemon, "web").await;
    let client = Client::connect(&daemon.socket).await.expect("connect");

    // Built past the `Smit` parser deliberately, so this tests the DAEMON's
    // refusal rather than the client's: a hand-rolled dog in another
    // language sends whatever it likes and never runs our parser.
    let refused = client.request(raw_set_smit("web", "\u{1b}[2Jgone")).await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(smit_of(&client, "web").await, None);
}
```

**The second test's comment is the load-bearing part.** A `Smit` newtype that validates in `FromStr` protects only callers who use the Rust client. The wire is a documented protocol (`docs/dogs.md:233-243` tells dog authors to speak it directly), so the daemon must validate what it decodes, not trust that it was constructed properly. Give `Smit` a `Deserialize` impl that goes through the same validation as `FromStr` rather than deriving it.

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep-daemon --test daemon_e2e --all-features -- smit
```
Expected: FAIL to compile, no `Request::SetSmit`.

- [ ] **Step 3: The `Smit` type in shep-core**

In `crates/shep-core/src/protocol/request.rs`, near `SelectorSpec` (`:37`). A newtype over `String`, `#[derive(Debug, Clone, PartialEq, Eq, Serialize)]` with a **hand-written** `Deserialize` that runs the same check as `FromStr`, and a `// wire format: changing this is a breaking change` comment matching `ExitInfo`'s at `:344` and `UpDuration`'s at `values.rs:189`.

The rule, stated once in the type's doc and enforced in one function:

- non-empty after trimming
- at most `MAX_CHARS = 48` **characters**
- no `char::is_control`, and no `\u{1b}` (which `is_control` already covers, but name it anyway, because it is the one an attacker reaches for and a reader should not have to know the classification to see it is handled)

Derive `Display`. `Debug` is derived and that is the deliberate decision (IR-41): a smit carries no environment and no secret, it is a string a dog chose to have painted in public, so redacting it would hide the thing an operator is debugging.

- [ ] **Step 4: The request, the response, and the field**

```rust
    /// Attach a short marker to `sheep` for `shep flock` to paint, or clear
    /// it with `None`.
    ///
    /// By NAME rather than a selector, for `Scale`'s reason
    /// (see its own doc above): a smit belongs to a sheep, not to one of
    /// its instances, and every instance of that name shows it -- including
    /// one spawned after the smit was painted.
    ///
    /// Held in memory and scoped to the connection that sent it. When that
    /// connection closes, for any reason, the smits it painted go with it.
    /// A publisher therefore republishes rather than publishing on change.
    ///
    /// shep does not parse it and has no opinion about what it means.
    SetSmit {
        /// Which sheep.
        sheep: String,
        /// The marker, or `None` to clear it.
        smit: Option<Smit>,
    },
```

`Response::SmitPainted(Vec<ProcessInfo>)`, its own variant rather than reusing one of the ten existing `Vec<ProcessInfo>` variants: the `Response` doc at `request.rs:832-840` says those exist separately precisely so a variant can diverge later without a protocol bump. Follow it.

`ProcessInfo::smit: Option<String>` next to `last_exit`, plain `Option`, **no `#[serde(default)]`** (the tree has none; serde's derive special-cases `Option`, proved by the test at `request.rs:1255`). `String` rather than `Smit` on the read side: a client decoding a listing from a daemon that validated it should not have to re-run the parser, and `ProcessInfo` is a report rather than an input.

Add the compat test beside its siblings:

```rust
    /// fails if the new field breaks an older peer, on the same terms as
    /// `last_exit` and `lambs` before it. A daemon that predates smits
    /// sends no `smit` key, and this decoding to `None` rather than
    /// erroring is what keeps `PROTOCOL_VERSION` at 1.
    #[test]
    fn a_process_info_without_a_smit_key_still_deserializes() {
        let json = r#"{"id":1,"name":"web","status":"online","pid":42,
                       "restarts":0,"uptime_ms":10,"fold":null,
                       "out_file":null,"err_file":null,"cpu_percent":null,
                       "memory_bytes":null,"dog":null,"lambs":null,
                       "last_exit":null}"#;
        let info: ProcessInfo = serde_json::from_str(json).expect("decode");
        assert_eq!(info.smit, None);
    }

    /// fails if the daemon accepts a smit it should refuse. `Smit` must
    /// validate on the way IN, not only in `FromStr`: `docs/dogs.md` tells
    /// dog authors to speak this wire directly, so a dog written in another
    /// language never runs our parser.
    #[test]
    fn a_smit_is_validated_when_it_is_deserialized_not_only_when_parsed() {
        for bad in [
            r#""\u001b[2Jgone""#,                    // an escape, JSON-encoded
            r#""a\nb""#,                                 // a newline
            r#""""#,                                     // empty
            &format!(r#""{}""#, "x".repeat(Smit::MAX_CHARS + 1)), // too long
        ] {
            assert!(
                serde_json::from_str::<Smit>(bad).is_err(),
                "a daemon must refuse this on the wire: {bad}"
            );
        }
        assert!(serde_json::from_str::<Smit>(r#""▲ main@a1b2c3""#).is_ok());
    }
```

- [ ] **Step 5: Connection identity in the daemon**

`crates/shep-daemon/src/server.rs`:

```rust
/// Distinguishes one client connection from another, for the lifetime of
/// that connection and no longer.
///
/// Minted per accepted connection and never reused within a daemon's life.
/// The only thing scoped by it today is smits, whose whole lifecycle rule
/// is "they belong to the connection that painted them" -- which is what
/// makes them ephemeral without cleanup logic on every path that can stop
/// a dog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ConnId(u64);
```

Minted in `handle_conn` after `check_peer` (a `static NEXT: AtomicU64`, `fetch_add(1, Ordering::Relaxed)`), threaded through `converse` and `read_loop` into `rpc::run`, and forgotten in `handle_conn`'s tail:

```rust
    drop(out_tx);
    let _ = writer.await;
    // Beside the two lines above for the same reason their comment gives:
    // this block is on EVERY path out. A smit belongs to the connection
    // that painted it, and this is the one place that is true of.
    ctx.supervisor.forget_smits(conn).await;
    outcome
```

Note the ordering: after the writer join, so a client that painted and immediately read still sees its own smit in the reply it was already sent.

- [ ] **Step 6: Storage in the supervisor**

A `HashMap<String, (ConnId, String)>` on the `Actor`, keyed by sheep name. Two commands: `Command::SetSmit { conn, sheep, smit, reply }` and `Command::ForgetSmits { conn, reply }`, with `SupervisorHandle` methods beside `scale` (`supervisor.rs:794`) and following its oneshot shape exactly.

`SetSmit` with `Some` inserts `(conn, text)`, overwriting whatever was there. **A second dog can overwrite a first dog's smit, and that is deliberate**: one column, one string, last writer wins, and shep is not going to arbitrate between dogs. Say so in the command's doc so it is a decision rather than an accident. `SetSmit` with `None` removes the entry **only if the stored `ConnId` matches**, so one dog cannot clear another's.

`ForgetSmits` retains only entries whose `ConnId` differs.

`to_info` (`supervisor.rs:5068`) becomes `Actor::to_info(&self, entry)` so it can read the map, looking up `entry.spec.config().name`. **Expect a wide but entirely mechanical edit inside `supervisor.rs`**: it is the single conversion point and every listing and every `BusEvent::Process` goes through it, so the compiler finds all of them. Check first whether anything outside the `impl` calls it (`crates/shep-daemon/src/testing.rs` is the one to look at); if so, keep a free function taking the map and have the method delegate, rather than duplicating the body.

- [ ] **Step 7: The RPC arm**

`crates/shep-daemon/src/rpc.rs`, beside `Request::Scale`'s arm at `:349`. Refuse a `sheep` that names nothing with the same `RpcErrorCode` the other name-taking verbs use, so a dog painting a smit for a sheep that was deleted gets a clean answer rather than a silently-stored orphan.

- [ ] **Step 8: Verify**

```bash
cargo test -p shep-core --lib --all-features
```
Expected: PASS.

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```
Expected: PASS.

```bash
cargo test -p shep-daemon --test daemon_e2e --all-features -- smit
```
Expected: PASS, two tests.

Re-accept and **read** the three wire snapshots, as Task 5's Step 5 describes. Update `crates/shep-cli/tests/fixtures/*.json`, `crates/shep-client/src/testing.rs` and `whistle/facts.rs`.

Mutations, `cp` plus `shasum` first:

| mutation | expected |
|---|---|
| delete the `forget_smits` call from `handle_conn`'s tail | `a_smit_dies_with_the_connection_that_painted_it` red. **This is the mutation that matters**; if it does not turn red, the e2e test is not exercising what it claims and must be fixed before anything else in this task is believed. |
| make `ForgetSmits` clear the whole map instead of filtering by `ConnId` | not covered by the tests above. **Add a third e2e case**: two painters, one disconnects, the other's smit survives. Without it, connection scoping is indistinguishable from "any disconnect clears everything". |
| accept a control character in `Smit`'s validation | `a_smit_carrying_an_escape_is_refused_at_the_daemon` red |
| derive `Deserialize` on `Smit` instead of validating in it | the same test red, because the raw request bypasses `FromStr`. If it stays green, the raw helper is not raw. |
| let `SetSmit { smit: None }` clear regardless of `ConnId` | not covered. Add it to the two-painter case above. |

- [ ] **Step 9: Commit**

```bash
git add crates/shep-core/src/protocol/ crates/shep-daemon/src/ \
        crates/shep-cli/tests/fixtures/ crates/shep-client/src/testing.rs \
        crates/shep-cli/src/whistle/facts.rs crates/shep-daemon/tests/daemon_e2e.rs
git commit -F- <<'EOF'
feat(core,daemon): a dog may paint a smit on a sheep

Rin's requirement was that `shep flock` show which sheep are being watched
by a deploy dog. Building that into shep would mean shep's core learning
what a deployment is. This is the general mechanism instead: a dog attaches
a short string to a sheep and shep paints it without understanding it.
`smit` is the shepherding term for a paint mark identifying whose flock a
sheep belongs to, and unlike a brand it is deliberately temporary, which is
exactly what this is.

Keyed by sheep NAME, not instance id. A sheep can run several instances,
and one smit per entry would mean fanning out at publish time and then
keeping it in step as instances come and go -- an instance spawned five
seconds after a publish would show nothing until the publisher's next tick.

Ephemeral, and scoped to the CONNECTION that painted it. That is the
lifecycle decision and it buys the whole thing with one cleanup site.
Disable, rehome, crash, daemon restart and a deliberate reconnect all end a
socket, so all five drop the smits without anyone editing five code paths.
Persisting instead would leave `shep flock` showing a mark attributed to a
dog that no longer exists, and removing that orphan class is what would
have cost cleanup logic everywhere. Publishers republish rather than
publishing on change, and the wire doc says so.

The daemon had no connection identity at all, so this adds the smallest one
that works: a `ConnId` minted per accepted connection, threaded to the RPC
layer, forgotten in `handle_conn`'s tail. That tail was already the block
whose comment says it runs on every path out.

Validated at ingress and refused rather than sanitised. A smit is written
by a third party and printed to a person, and the table renderer is NOT a
guard: `output::width::sanitize_cell` deliberately keeps a well-formed CSI
sequence, because shep's own colouring is made of them. Refusing at the
daemon means `flock`, `describe`, `--format json`, the lookout, the MCP
schema and every bus subscriber are safe by construction instead of six
places each remembering. The publisher is a program, so a refusal it can
see beats mangling it cannot; `kv.rs` already refuses on the same terms for
the same kind of value.

`Smit` validates in `Deserialize`, not only in `FromStr`. The wire is a
documented protocol that dog authors are told to speak directly, so a dog
written in another language never runs our parser and the daemon has to
check what it decodes. The e2e test sends a raw frame past the newtype for
exactly that reason.

Last writer wins if two dogs paint the same sheep. One column, one string,
and shep is not going to arbitrate between dogs. A clear only takes effect
from the connection that painted it, so one dog cannot wipe another's.

The lifecycle is pinned end to end rather than in a unit test. A supervisor
test calling the forget path proves only that the function does what it
says; what had to be shown is that closing a real socket reaches it.
EOF
```

---

### Task 7: The SMIT column, at both widths

**Files:**
- Modify: `crates/shep-cli/src/output/rows.rs` (`FlockRows` `:41-157`, the drop-order test `:2855`)
- Modify: `crates/shep-cli/src/output/table.rs` (the fixture and its width note `:875-897`)
- Modify: `crates/shep-cli/src/lookout/view/flock.rs` (`Column` `:61`, `header` `:96`, `width` `:114`, the column sets `:137-195`, `TIERS` `:221`, `cell` `:362`)
- Modify: `crates/shep-cli/src/output/snapshots/*.snap`, `crates/shep-cli/src/lookout/snapshots/*.snap`, `docs/lookout/frames.txt`, `docs/lookout/frames.ansi`
- Test: the same files' `mod tests`

**Interfaces:**
- Consumes: Task 6's `ProcessInfo::smit: Option<String>`.
- Produces: nothing another task consumes.

**Rin's ruling, and the condition attached to it.** The smit is droppable on a narrow terminal. Her permission was conditional: dropping it is acceptable *because* it is seen regularly at full width. That carries a requirement the permission does not state outright, and this task exists to pin it: **it must never be dropped at full width, and must not be crowded out there by a later change.** Both ends get an exact-string test.

**Before placing it, a correction: two comments in this repository state the drop order backwards.** The renderer drops the **highest** priority number first (`table.rs:277-283`, `max_by_key` over `priorities`). `FlockRows::PRIORITIES` is `[0, 0, 0, 2, 4, 6, 5, 3, 1, 7]` against `["ID","NAME","STATUS","PID","RESTARTS","EXIT","CPU","MEM","UPTIME","FOLD"]`, so the real give-up order is **FOLD, EXIT, CPU, RESTARTS, MEM, PID, UPTIME**.

Three independent things agree with that and disagree with the comments:

- `columns_drop_by_priority_and_never_below_three` (`table.rs:689`) asserts FOLD is gone at width 46.
- `web/src/pages/docs/output.astro:88` shows a real 60-column render whose footer reads `CPU, EXIT, FOLD hidden.` -- the three highest numbers, printed alphabetically because `dropped.sort_unstable()` runs first (`table.rs:328`).
- The lookout's own tiers drop FOLD first and EXIT second (`lookout/view/flock.rs:221-230`).

What is wrong is only the wording. `rows.rs:137-157` and the test comment at `rows.rs:2866-2871` both introduce a list sorted by ascending priority as "in that dropping order" / "in the order they are given up". It is the reverse: ascending priority is the order they **survive** in, longest-lasting first. The neighbouring sentence at `:2869-2871` ("the ones answering 'is it healthy' outlast the ones answering 'which one is it'") describes the true behaviour correctly, which is presumably how this survived. Fix both wordings in this task; do not change a single number.

**So SMIT gets priority 8 and drops first of all.** "Among the first columns to yield" is Rin's phrase, and 8 is the literal reading of it. The supporting argument worth putting in the code comment: it is by far the widest column, so dropping it buys back the most room for one column lost.

**Corrected after review.** An earlier draft of this task offered a second argument, that SMIT is the only column whose content another command can recover by asking the deploy dog, where nothing but a wider terminal brings FOLD back. That is false, and `output.astro` disproves it on the same screen: `--format json` carries every field at any width, so every column is recoverable that way and none is special. The priority is unaffected; only the decorative reason goes. Do not reintroduce it.

**And it goes last in the header order**, after FOLD. The alternative worth naming: next to NAME would read better, since a smit is about which sheep this is. Against it: mid-table insertion moves every existing snapshot's layout more than the end does, and a column that is first to drop reads oddly in the middle of ones that outlast it. Ordered last, position and priority agree.

**The lookout is not optional here.** `the_full_column_set_matches_flock_rows_headers_exactly` (`lookout/view/flock.rs:456`) fails the moment `FlockRows::headers()` grows and `Column` does not. That test exists so the two surfaces cannot drift, and it will do its job. Note that the two surfaces already disagree on the order **after** the first two drops (the CLI gives up CPU third, the lookout gives up RESTARTS third) and that the lookout never drops UPTIME at all. Leave that alone; it predates this task and converging them is separate work.

**Where a fake would be too kind.** Three specific ways:

1. **A hand-built `ProcessInfo` with `smit: Some("x")` proves nothing about width.** The requirement is about a real smit at a real terminal size, so the fixtures use the real strings the real dog produces: `▲ main@a1b2c3` and `⏸ main@f6e5d4`, taken from `/Users/rin/GitHub/shep-deploy/src/smit.rs`.
2. **Do not assume how wide `▲` is.** `visible_width` measures through `unicode-width` (`output/width.rs:21`), and `▲` (U+25B2) and `⏸` (U+23F8) are exactly the kind of symbol whose East Asian Width is ambiguous or has moved between Unicode revisions. **Measure it in a test rather than reasoning about it**, and if the two marks measure differently, say so in the report: it does not break the column, which pads, but it means the two smit strings are not interchangeable in a width calculation.
3. **A test at one width cannot fail the way this must.** A single 120-column snapshot passes just as happily if SMIT had priority 1 and never dropped at all. Both ends, or neither.

- [ ] **Step 1: Measure the marks before writing anything else**

```rust
    /// Not a behaviour test: a MEASUREMENT, recorded so the two-width tests
    /// below rest on a number rather than an assumption. `unicode-width`
    /// classifies these two by East Asian Width, which is ambiguous for
    /// some symbols and has moved between Unicode revisions, so what shep
    /// thinks a smit occupies is worth writing down.
    #[test]
    fn how_wide_the_real_smits_actually_are() {
        assert_eq!(visible_width("\u{25b2} main@a1b2c3"), 13);
        assert_eq!(visible_width("\u{23f8} main@f6e5d4"), 13);
    }
```

**If either assertion fails, take the measured number and carry it into every width below rather than "fixing" the test.** Then say in the report what the real number is, because it changes the full-width threshold.

- [ ] **Step 2: Write the two failing width tests**

In `crates/shep-cli/src/output/table.rs`'s `mod tests`, beside the existing snapshot tests (`:920`, `:953`) and reusing `mixed_flock` (`:885`) extended to carry smits.

```rust
    /// fails if a smit is dropped at full width. Rin's permission to drop
    /// it on a narrow terminal was conditional on it being seen regularly
    /// at a wide one, so a later column that crowded it out here would
    /// reopen a decision that was already made. This is the half of that
    /// condition her permission does not state outright.
    #[test]
    fn a_smit_is_never_dropped_at_full_width() {
        let rendered = table_of(&mixed_flock_with_smits(), full_at(FULL_WIDTH));
        assert!(
            rendered.contains("\u{25b2} main@a1b2c3"),
            "the smit must survive a full-width render: {rendered}"
        );
        assert!(
            !rendered.contains("hidden. Widen the window"),
            "and nothing else may be dropped either, or FULL_WIDTH is wrong: {rendered}"
        );
        insta::assert_snapshot!(rendered);
    }

    /// fails if a smit stops yielding first on a narrow terminal. It is by
    /// far the widest column, so giving it up buys back the most room for
    /// one column lost.
    #[test]
    fn a_smit_is_the_first_column_dropped_when_the_window_narrows() {
        let rendered = table_of(&mixed_flock_with_smits(), full_at(FULL_WIDTH - 1));
        assert!(
            !rendered.contains("main@a1b2c3"),
            "the smit must be gone one column below full width: {rendered}"
        );
        assert!(
            rendered.contains("SMIT hidden.") || rendered.contains("SMIT, "),
            "and the footer must name it, so an operator knows to widen: {rendered}"
        );
        // FOLD outlasts it, which is the placement decision itself.
        assert!(rendered.contains("FOLD"), "{rendered}");
        insta::assert_snapshot!(rendered);
    }
```

`FULL_WIDTH` is a `const` in the test module, set to the narrowest width at which all eleven columns fit, computed from the fixture. **Derive it rather than hardcoding it**, then assert the derived number equals a literal, so the test says out loud when a later column changes it:

```rust
    /// The narrowest terminal that still shows every column, including the
    /// smit. Asserted rather than assumed: `table.rs`'s own note at :875
    /// records that adding EXIT cost 7 columns and forced the wide fixture
    /// from 80 to 90, and a later column will move this too. When it moves,
    /// that is a decision about Rin's full-width condition, not a number to
    /// quietly update.
    const FULL_WIDTH: usize = 106;
```

Getting 106 exactly right by hand is not the point and it may well be wrong: run the test, read what the renderer actually needed, and set it. Then check the two tests still say opposite things at `FULL_WIDTH` and `FULL_WIDTH - 1`.

- [ ] **Step 3: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features -- smit
```
Expected: FAIL to compile, `mixed_flock_with_smits` does not exist and `ProcessInfo` has no `smit` unless Task 6 has landed. **Task 7 cannot start before Task 6.**

- [ ] **Step 4: Add the column**

`crates/shep-cli/src/output/rows.rs`:

- `headers()` (`:43`) gains `"SMIT"` after `"FOLD"`.
- `rows()` (`:49`) gains `p.smit.clone().unwrap_or_else(|| "-".to_owned())`, matching FOLD's convention at `:72` exactly. A sheep no dog has painted shows `-`, which is the same answer the table already gives for "no fold" and "not running".
- `json_key_for` (`:103`) gains `"SMIT" => "smit"`.
- `PRIORITIES` (`:157`) becomes `&[0, 0, 0, 2, 4, 6, 5, 3, 1, 7, 8]`, with its comment rewritten to state the give-up order the right way round and to say why SMIT is 8.
- **Not** in `JSON_ONLY` and **not** in `assert_no_drift`'s `formatted` list (`rows.rs:1958`): the cell is the stored string verbatim, so the JSON value and the cell agree and the drift check should compare them.

Then the drop-order test at `:2855`. Its `ranked` list gains `"SMIT"` at the end, since ascending priority puts 8 last, and its comment gets the wording fix described above.

- [ ] **Step 5: The lookout**

`crates/shep-cli/src/lookout/view/flock.rs`: a `Column::Smit` variant, its `header()` arm (`"SMIT"`), its `width()` arm, its `cell()` arm reading `info.smit`, `Smit` at the end of `ALL`, and **a new tier above the current top one** so `ALL` is only chosen on a terminal wide enough for it:

```rust
const TIERS: &[(u16, &[Column])] = &[
    (117, ALL),        // the new one; ALL now includes Smit
    (101, NO_SMIT),    // what ALL used to be
    (89, NO_FOLD),
    ...
```

The exact threshold comes from `Column::Smit`'s `width()` plus the existing 101, the same arithmetic every other tier uses. `columns_drop_in_a_fixed_order_as_the_terminal_narrows` (`:426`) gains an assertion at the new width, and `the_full_column_set_matches_flock_rows_headers_exactly` (`:456`) should go green on its own once `ALL` matches.

- [ ] **Step 6: Re-accept the snapshots, and read every one**

```bash
cargo insta test --accept -p shep --lib
```

Expect movement in five `output/snapshots/*.snap` and the whole `lookout/snapshots/` set. **Read them.** `bare_pins_the_byte_identical_plain_table` is the one to read hardest: `bare` renders through `render_table`, not `render_boxed_ex` (`output/mod.rs:340-342`), so **no column is ever dropped at `bare` and no footer is ever printed**. The smit therefore always shows there, and `shep flock | cat` always carries it. That is correct and worth a sentence in the commit, because it means "droppable" is a property of the boxed renderer alone.

Then regenerate the TUI gallery, which is a separate `#[ignore]`d test (`lookout/frames.rs:1590`):

```bash
cargo test -p shep --lib --all-features -- write_the_gallery --ignored
```

and commit `docs/lookout/frames.txt` and `frames.ansi` with the rest.

- [ ] **Step 7: Verify**

```bash
cargo test -p shep --lib --all-features
```
Expected: PASS.

Mutations, `cp` plus `shasum` first:

| mutation | expected |
|---|---|
| change SMIT's priority from 8 to 1 | `a_smit_is_the_first_column_dropped_when_the_window_narrows` red. **This is the mutation that proves the pair is doing its job**: the full-width test alone stays green, which is exactly why one test would not have been enough. |
| change SMIT's priority from 8 to 0 (never drops) | the narrow test red; the full-width test still green |
| render `FULL_WIDTH - 1` in the full-width test too | both tests green, which is the failure mode the derived-and-asserted `FULL_WIDTH` const exists to make visible. Note it: the pair rests on the two widths genuinely differing, and nothing but that const says so. |
| drop `Column::Smit` from the lookout's `ALL` | `the_full_column_set_matches_flock_rows_headers_exactly` red |
| show `""` instead of `-` for an unpainted sheep | the full-width snapshot red |

- [ ] **Step 8: Commit**

```bash
git add crates/shep-cli/src/output/ crates/shep-cli/src/lookout/ docs/lookout/
git commit -F- <<'EOF'
feat(cli): paint a sheep's smit in the flock table

The reading half of the mechanism the daemon grew last commit. shep does
not know what `A main@a1b2c3` means and never will; it is a string a dog
attached and a column shep paints.

Priority 8, so it is the first column given up as a terminal narrows, and
placed last so position and priority agree. Rin ruled it droppable, with a
condition her permission did not state outright: dropping it on narrow is
acceptable BECAUSE it is seen regularly at full width, so it must never be
dropped at full width and must not be crowded out there later. Both ends
are pinned. One test would not have been enough -- a full-width test alone
passes just as happily with the priority set to 1, and the narrow test is
what fails.

One supporting reason it yields first, in the code comment: it is by far the
widest column, so dropping it buys back the most room for one column lost.

Also corrects two comments that stated the drop order backwards. The
renderer gives up the HIGHEST priority number first, so the real order is
FOLD, EXIT, CPU, RESTARTS, MEM, PID, UPTIME. Both `FlockRows::PRIORITIES`
and the drop-order test introduced an ascending-priority list as "in that
dropping order", when ascending priority is the order they SURVIVE in.
Three things already disagreed with the comments and agreed with each
other: the width-46 assertion in `columns_drop_by_priority_and_never_below_
three`, the real 60-column render published at web/src/pages/docs/output
.astro whose footer reads "CPU, EXIT, FOLD hidden", and the lookout's own
tiers. No number changed here; only the sentences.

Note that `bare` never drops anything: it renders through `render_table`,
not the boxed renderer, so `shep flock | cat` always carries the smit and
prints no footer. "Droppable" is a property of the box-drawn path alone.

The fixtures use the real strings the real dog produces rather than
invented ones, and a separate test records what `unicode-width` thinks
those two marks measure, so the width thresholds rest on a measurement
instead of on reasoning about East Asian Width.

The lookout moves with it, as its parity test requires. The two surfaces
still disagree about the order after the first two drops, and the lookout
still never gives up UPTIME; both predate this and are left alone.
EOF
```

---

### Task 8: `shep-client` reconnect, ruled on

**Files:**
- Modify: `crates/shep-client/src/client.rs` (`Client` `:115`, `RequestError::Closed`'s doc `:82`), `crates/shep-client/src/lib.rs` (crate doc `:1-15`, re-exports `:44-51`), `crates/shep-client/src/testing.rs` (four new fakes, see Step 1)
- Modify: `docs/dogs.md` ("Writing your own", after the wire paragraph at `:233-243`)
- Test: `crates/shep-client/tests/request_reply.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub enum Reconnected { SameDaemon, NewDaemon }
  impl Client {
      pub async fn reconnect(&mut self) -> Result<Reconnected, ConnectError>;
      pub async fn reconnect_within(&mut self, deadline: Duration)
          -> Result<Reconnected, ConnectError>;
  }
  ```

**The brief asked for a ruling, argued, not assumed. Here it is: this is a `shep-client` change AND a documented contract, and it is emphatically NOT a retry inside `request`.**

**Why "out of scope" is not available.** `docs/specs/shep-v1.md:239` already says, unqualified: "Client reconnect: backoff 100ms x1.5, cap 5s." The constants exist and cite that line in their own docs (`crates/shep-client/src/spawn.rs:27,30`), but they are spent entirely on the cold-start path -- reaching a daemon that is coming up, never re-reaching one that went away. So this is unfinished spec work, and the evidence that it is unfinished rather than deliberately dropped is that **two other places in this workspace went and built it themselves**:

- `lookout` has a full ladder in `crates/shep-cli/src/lookout/link.rs`, at 250ms **x2** capped at 4s over 5 attempts, **not** the spec's 100ms x1.5 capped at 5s. It also had to invent a `Shepherd` trait (`lookout/source.rs:107`) whose whole reason for existing, per its own doc, is that a reconnect has to rebuild the request path and the subscription together and the client crate offers no way to say that.
- `whistle` sidesteps it by connecting, sending one request and closing, every single call (`crates/shep-cli/src/whistle/shepherd.rs:70-85`).

Three answers to one question in one workspace, and the spec's answer is implemented in none of them.

**Why a transparent retry inside `request` is nonetheless wrong, and shep's own wire is what proves it.** Instance ids are minted by a running daemon and are not persisted: `ProcessEntry` has no `Serialize` (`crates/shep-daemon/src/entry.rs:16`) and the muster roll stores only `AppConfig` plus a running count (`crates/shep-daemon/src/snapshot.rs:62-78`). So an id means something only for one daemon's lifetime. `SelectorSpec::Id` (`crates/shep-core/src/protocol/request.rs:37-48`) puts ids on the wire, and **`shep-deploy` deletes the original instance by id during a cutover**, which is the single most destructive thing it does.

A `request` that silently re-dialled would take a `Delete { selector: Id(7) }` aimed at one daemon and land it on another, where 7 is a different process or none. Today that returns `RequestError::Closed` and the caller stops. That is a loud failure being converted into a quiet wrong answer, which is the precise shape of the two worst defects `shep-deploy` shipped: a double more forgiving than the real daemon, moved into production.

**What makes the safe version cheap: `HelloAck` already carries the daemon's pid.** `HelloAck { daemon_version, protocol, pid }`, so a reconnect can tell the caller whether it landed on the same process. That single fact turns "reconnect is dangerous for id-holding callers" into "reconnect tells id-holding callers whether to throw their ids away", which is a two-line check at the call site.

**The remedy for a supervised dog is to EXIT, not to reconnect, and that is worth saying plainly.** `[daemon] enabled_dogs` means "dogs to autostart with the daemon" (`crates/shep-core/src/config/daemon.rs:27`), so a daemon that comes back starts its own dog. A dog that survived the old daemon and reconnected to the new one would be a **second** copy, both polling, both deploying. For `shep-deploy` that means two processes racing over one `current` symlink. So the dog-side fix is one arm: on `RequestError::Closed`, print why and exit nonzero. `reconnect` is for the callers that deliberately outlive a daemon and hold no ids, which is `lookout` and third-party embedders.

**Explicitly NOT in this task:** converging `lookout` onto the spec's schedule, or onto `reconnect`. It works, it is tested (`link.rs:497`, `:587`), and its 5-attempt bound exists to reach a "frozen" UI state that a plain backoff has no concept of. Naming the divergence is this task's job; resolving it is not.

**For Rin, two things.** First, `reconnect(&mut self)` cannot be called through an `Arc<Client>`, and `crates/shep-client/src/client.rs:110-114` explicitly recommends sharing one `Client` behind an `Arc`. That is deliberate: `&mut self` forces a caller to hold exclusive access at the moment the connection's identity changes, which is exactly when a shared handle would be lying to its other users. The alternative is interior mutability, which slides back toward the transparent behaviour argued against above. Second, distinguishing daemons by pid has a residual: pids are reusable. Within a reconnect window it takes wrapping the whole pid space, so it is not a practical worry, and the stronger fix is a boot nonce as an additive `Option<u64>` on `HelloAck` (version stays 1, same terms as `last_exit`). **Not built here**; noted so the choice is visible rather than discovered later.

**Where a fake would be too kind.** Almost every fake in `crates/shep-client/src/testing.rs` accepts exactly **one** connection (`:43`, `:67`, `:101`, `:501`, `:917`), so a reconnect test written against the obvious fake would pass by connecting once and never proving a re-dial happened at all. `fake_daemon_accepting_repeatedly` (`:117`) is the one to use, and the test has to assert the **second** connection actually served a request, not merely that `reconnect` returned `Ok`.

- [ ] **Step 1: Write the failing tests**

In `crates/shep-client/tests/request_reply.rs`, beside `a_dropped_connection_fails_pending_requests_instead_of_hanging` (`:69`).

**Four helpers below do not exist yet and are yours to add** to `crates/shep-client/src/testing.rs`, behind the existing `test-support` feature. Only `fake_daemon_accepting_repeatedly` (`testing.rs:117`) is already there, and it is the only fake in that file that accepts more than one connection at all. The new ones:

| helper | what it does |
|---|---|
| `ack_with_pid(pid: u32) -> HelloAck` | a `HelloAck` with `daemon_version` from `CARGO_PKG_VERSION`, `protocol: PROTOCOL_VERSION`, and the given pid. Three of the tests differ only in this. |
| `fake_daemon_accepting_repeatedly_on(path, ack)` | the same fake, bound to a path the caller names, so a replacement daemon can take over one socket |
| `.drop_current_connection()` | closes the connection currently served, leaving the listener up. This is a daemon dropping one client, not going away. |
| `.stop_listening()` | closes the listener, leaving nothing to re-dial. This is the daemon going away. |

The two are separate on purpose: `reconnect` succeeding needs the first without the second, and `reconnect_within` exhausting a deadline needs both. Collapsing them into one "kill the fake" helper is how the reconnect test ends up unable to distinguish a re-dial from a connection that never closed.

```rust
/// fails if a handle cannot be brought back after its daemon went away.
/// Note the fake: almost every fake in `testing.rs` accepts exactly one
/// connection, so a test written against those would pass by connecting
/// once and never re-dialling. This one asserts the SECOND connection
/// served a request.
#[tokio::test]
async fn a_client_can_be_reconnected_after_its_connection_died() {
    let daemon = fake_daemon_accepting_repeatedly(ack_with_pid(4242));
    let mut client = Client::connect(&daemon.socket).await.expect("connect");
    daemon.drop_current_connection();

    assert_eq!(client.request(Request::Ping).await, Err(RequestError::Closed));
    client.reconnect().await.expect("reconnect");
    assert!(
        client.request(Request::Ping).await.is_ok(),
        "the handle must work again, which means a second connection really served this"
    );
}

/// fails if a caller cannot tell that its ids stopped meaning anything.
/// Instance ids are minted per daemon lifetime and are never persisted, so
/// `SelectorSpec::Id` aimed at a daemon that has been replaced addresses a
/// different process or none. shep-deploy deletes the original instance BY
/// ID during a cutover, which is the most destructive thing it does, so
/// "same daemon or not" is the fact that makes reconnect safe to use at
/// all.
#[tokio::test]
async fn a_reconnect_says_whether_it_landed_on_the_same_daemon() {
    let daemon = fake_daemon_accepting_repeatedly(ack_with_pid(4242));
    let mut client = Client::connect(&daemon.socket).await.expect("connect");
    daemon.drop_current_connection();
    assert_eq!(client.reconnect().await.expect("reconnect"), Reconnected::SameDaemon);

    // The same socket path, a different daemon behind it: what an operator
    // gets from `shep kill` followed by any command that autostarts one.
    daemon.stop_listening();
    let replacement = fake_daemon_accepting_repeatedly_on(&daemon.socket, ack_with_pid(5353));
    assert_eq!(client.reconnect().await.expect("reconnect"), Reconnected::NewDaemon);
    drop(replacement);
}

/// fails if reconnect stops honouring the schedule the spec names. Spec
/// section 6 says 100ms x1.5 capped at 5s, `spawn::BACKOFF_START` and
/// `BACKOFF_CAP` already hold those numbers and cite that line, and this
/// path must use THOSE rather than growing a third schedule -- the
/// workspace already has two (lookout's is 250ms x2 capped at 4s).
#[tokio::test(start_paused = true)]
async fn reconnect_within_retries_on_the_schedule_the_spec_names() {
    let daemon = fake_daemon_accepting_repeatedly(ack_with_pid(4242));
    let mut client = Client::connect(&daemon.socket).await.expect("connect");
    daemon.stop_listening();
    daemon.drop_current_connection();

    // Elapsed time, not attempt count: a change to the multiplier moves
    // the schedule without moving the count, and the schedule is what
    // spec section 6 pins.
    let started = tokio::time::Instant::now();
    let deadline = Duration::from_millis(700);
    let failed = client.reconnect_within(deadline).await;

    assert!(failed.is_err(), "nothing is listening: {failed:?}");
    // The schedule is 100, 150, 225, 337, so four sleeps sum to 812ms and
    // three to 475ms. `probe_until_ready` checks the deadline at the TOP of
    // its loop, so it wakes from the fourth sleep and gives up: elapsed
    // lands in [475, 812]. Assert the band rather than one number, because
    // the exact value depends on where the deadline check sits, and pin the
    // two constants separately so a change to EITHER is caught.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(475) && elapsed <= Duration::from_millis(812),
        "off the spec's schedule: {elapsed:?}"
    );
    assert_eq!(spawn::BACKOFF_START, Duration::from_millis(100));
    assert_eq!(spawn::BACKOFF_CAP, Duration::from_secs(5));
}
```

`start_paused = true` on the third: without it this waits real seconds. Six of `shep-deploy`'s tests shipped missing it and cost about a minute of wall clock each run. **And check the failure direction**: under a paused clock a test whose only await point is the sleep can hang rather than fail if the sleep is removed, which is how one of that project's mutations produced exit 124 and no verdict instead of a red test. Make sure the failing shape here is a failure, not a hang, and say which it is in the report.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep-client --test request_reply --features test-support -- reconnect
```
Expected: FAIL to compile, no method `reconnect`.

- [ ] **Step 3: Implement**

```rust
/// Whether a [`Client::reconnect`] landed on the daemon it left.
///
/// The distinction exists because instance ids do not survive a daemon.
/// They are minted by a running daemon and never persisted, and
/// `SelectorSpec::Id` puts them on the wire, so a request carrying an id
/// from before a replacement addresses a different process or none.
///
/// [`Self::SameDaemon`] means a caller's ids are still good.
/// [`Self::NewDaemon`] means every one of them must be discarded and
/// re-fetched before anything is addressed by id.
///
/// Told apart by the daemon's pid, from the handshake. Pids are reusable
/// in principle; within a reconnect window that needs the whole pid space
/// to wrap, so it is not a practical concern, and the stronger answer if
/// it ever becomes one is a boot nonce on `HelloAck` (additive, so
/// `PROTOCOL_VERSION` would stay 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reconnected { SameDaemon, NewDaemon }
```

`reconnect` re-dials `self.socket` once through `Connection::open` with `HANDSHAKE_TIMEOUT`, replaces `self.commands` and `self.ack`, and compares the new `ack.pid` with the old. The old actor's task ends on its own when the previous `commands` sender drops, which is `actor.rs:123`'s `None => break` arm.

`reconnect_within` loops the same re-dial to a deadline using `spawn::BACKOFF_START` and `spawn::BACKOFF_CAP` -- **the existing constants, imported, not new ones**. Follow `probe_until_ready`'s shape (`spawn.rs:294-352`) precisely: the deadline check at the top of the loop, the sleep before each re-probe, `backoff = backoff.mul_f64(1.5).min(cap)` after the sleep, and `ConnectError::ProtocolMismatch` short-circuiting rather than being retried.

Both need `# Errors` sections (IR-24). `Reconnected` derives `Debug` and that is the deliberate decision: it carries no data at all, let alone a secret.

- [ ] **Step 4: Write the contract down, in the two places a dog author looks**

The crate doc (`crates/shep-client/src/lib.rs:1-15`) says nothing about connection lifetime. `RequestError::Closed`'s doc (`client.rs:82-83`) is currently the only statement anywhere in the public surface that a daemon can die under a live handle, and it does not tell a caller what to do. Add, to the crate doc:

> **A `Client` is one connection, and it does not come back on its own.** When the daemon exits or crashes, the actor behind the handle ends, every pending request fails with [`RequestError::Closed`], and so does every later one. Nothing retries. Call [`Client::reconnect`] to bring the handle back, and read what it returns: instance ids do not survive a daemon, so landing on a new one means discarding every id you were holding.
>
> **A supervised dog should exit instead of reconnecting.** A daemon that comes back starts its own dogs from `[daemon] enabled_dogs`, so a dog that outlived the old daemon and re-dialled the new one is a second copy of itself, both running. Exiting non-zero is the correct response to losing your shepherd; the shepherd is what brings you back.

And in `docs/dogs.md`, after the wire paragraph at `:233-243`, the same point in that document's voice, since a dog author reading only that file must not have to infer it. This is the paragraph whose absence produced the failure: `shep-deploy` held one `Client` for its whole process lifetime and became a zombie that still fetched, built and swapped before failing.

- [ ] **Step 5: Verify and commit**

```bash
cargo test -p shep-client --all-features
```
Expected: PASS.

Mutations, `cp` plus `shasum` first:

| mutation | expected |
|---|---|
| have `reconnect` always return `SameDaemon` | `a_reconnect_says_whether_it_landed_on_the_same_daemon` red on the second half |
| have `reconnect` keep the old `ack` | the same test red |
| swap `BACKOFF_CAP` for a literal `Duration::from_secs(4)` (lookout's number) | `reconnect_within_retries_on_the_schedule_the_spec_names` red |
| use a fake that accepts one connection | `a_client_can_be_reconnected_after_its_connection_died` red at the second request. Worth running once to confirm the fake choice is load-bearing rather than incidental. |

```bash
git add crates/shep-client/src/ crates/shep-client/tests/ docs/dogs.md
git commit -F- <<'EOF'
feat(client): explicit reconnect, and say what a dead daemon means

A `Client` has always been one connection with no way back. The actor
breaks its loop when the socket dies, drains every pending request with
`Closed`, and every later request takes the same path because the command
channel's receiver is gone. Nothing said so anywhere in the public surface
except `RequestError::Closed`'s own doc, which describes the state without
telling a caller what to do about it.

That gap has a cost measured in a real dog: shep-deploy holds one Client
for its whole process lifetime, so once its shepherd went away it carried
on as a zombie, fetching, building and swapping a release before failing at
the first request that needed the daemon, and saying nothing about why.

This is not a new feature so much as unfinished spec work. Spec section 6
says "Client reconnect: backoff 100ms x1.5, cap 5s", unqualified.
BACKOFF_START and BACKOFF_CAP already exist and cite that line, spent
entirely on reaching a daemon that is coming UP rather than re-reaching one
that went away. Two other places built their own instead: lookout has a
full ladder at 250ms x2 capped at 4s and had to invent a Shepherd trait
because this crate offers no way to say "rebuild both halves of a
connection", and whistle connects, asks and closes on every single call.

It is NOT a retry inside `request`, and shep's own wire is why. Instance
ids are minted per daemon lifetime and never persisted, ProcessEntry has no
Serialize and the muster roll keeps only AppConfig plus a running count.
SelectorSpec::Id puts those ids on the wire, and shep-deploy deletes the
original instance BY ID during a cutover. A request that silently re-dialled
would carry `Delete { Id(7) }` to a daemon where 7 is a different process or
none, turning a loud failure into a quiet wrong answer. That is exactly the
shape of the two worst defects that project shipped: a double kinder than
the real daemon, moved into production.

So the caller opts in and is told what it landed on. HelloAck already
carries the daemon's pid, so distinguishing "same daemon, your ids are
good" from "new daemon, throw them away" costs nothing new on the wire.

The dog-side answer is documented as EXIT, not reconnect, and that is not a
hedge. `enabled_dogs` means dogs autostart with the daemon, so a daemon
that comes back starts its own; a dog that outlived the old one and
re-dialled would be a second copy, both polling and both deploying, racing
over one `current` symlink. Losing your shepherd is a reason to stop, and
the shepherd is what brings you back.

Left alone deliberately: lookout keeps its own ladder. It works, it is
tested, and its attempt bound exists to reach a frozen-UI state a plain
backoff has no concept of. Naming the divergence is this commit's job.
EOF
```

---

### Task 9: The docs sweep

**Files:** everything in the table below.

**Interfaces:**
- Consumes: Tasks 1 through 8, all of them. Ninth of the ten; only Task 10 follows it.
- Produces: nothing another task consumes.

**Why one sweep rather than a docs step inside each task.** Five of the eight tasks change what an operator types or sees, and three of them edit the same three `.astro` files. Landing them separately means three merge conflicts and five `astro build` runs. More importantly, `web/scripts/generate-cli-reference.sh` regenerates from a **release binary**, so it has to run after the last `--help` string has changed, not after each one.

**This is the hard trigger, and a green Rust gate does not discharge it.** `CLAUDE.md` states the rule and the reason: on 2026-08-19 the generated reference was two days stale, 919 lines of drift, and regenerating it surfaced a real regression nobody had noticed. Nothing in `cargo test` reads `web/`.

**Grep before assuming.** Every list below is what was found on 2026-08-27, and the point of the rule is that a hand-written page nothing generates can say anything. Re-grep rather than trusting this table to still be complete.

- [ ] **Step 1: The lexicon, which has one source and two renderings**

`README.md`'s "## The lexicon" table (`:85-110`) **is the source**: `web/src/data/docsLexicon.ts` parses it at build time and `terminology.astro` renders it, so one edit updates the README and the site together. Add a row in the existing four-column shape:

```
| a smit | a short mark a dog paints on a sheep (a badge) | the SMIT column in `shep flock` | yes |
```

`docs/terminology.md` is the canonical lexicon and needs the same concept in **its** shape, which is a different table (`| Concept | Conventional | shep says | Where it applies |`), with `badge` recorded as the plain alias. `smit` is the real shepherding term for a paint mark identifying whose flock a sheep belongs to, and unlike a brand it is deliberately temporary, which is what makes it the right word. `badge` was rejected as the primary because the README already carries seven shields.io badges and one word should not mean two things in one project. Rin approved `smit` on the precedent of `muster` and `thatlldo`.

While in `docs/terminology.md`, fix its `rehome` row at `:28` per Task 1.

- [ ] **Step 2: Every prose page, by task**

| Page | Line | Says now | Task |
|---|---|---|---|
| `docs/dogs.md` | `:229-231` | rehome "forgets the registration in `shep.toml` entirely" | 1 |
| `docs/dogs.md` | `:59-60`, `:74-80` | the disable/enable contrast, and "editing does not reach a running dog" | 1 |
| `docs/dogs.md` | `:196` | the `shep rehome watchdog` example block | 1 |
| `docs/dogs.md` | `:223`, `:245` | "a dog the shepherd starts gets no argv" -- now has a third invocation mode | 3 |
| `docs/dogs.md` | after `:243` | the wire paragraph; gains the dead-daemon contract | 8 |
| `docs/dogs.md` | `:189+` | "Writing your own" gains the on-remove hook: the argv, the 10s budget, that a nonzero exit is read as "not implemented" and is fine, that stdout and stderr both reach the operator, and that it runs before anything is forgotten | 3 |
| `docs/specs/shep-v1.md` | `:286`, `:324-333` | "`shep rehome <name>` forgets the registration" | 1 |
| `docs/specs/deferred.md` | `:864` | the dog verb list | 1 |
| `README.md` | `:104` | "adopt / rehome \| register or drop a third-party dog" | 1 |
| `SECURITY.md` | `:209` | the four config-writing verbs | 1 |
| `web/src/pages/docs/dogs.astro` | `:251-258` | rehome's `VerbSignature` | 1 |
| `web/src/pages/docs/dogs.astro` | `:73`, `:120-124` | disable's signature, and the "`disable && enable` re-reads it" paragraph | 1 |
| `web/src/pages/docs/dogs.astro` | `:270` | "a supervised dog gets no argv and one environment variable" | 3 |
| `web/src/pages/docs/dogs.astro` | `:231+` | "Writing your own" gains the hook, mirroring `docs/dogs.md` | 3 |
| `web/src/pages/docs/dogs.astro` | `:84-92` | the two-table `shep flock` sample | 7 |
| `web/src/pages/docs/cli.astro` | `:44` | the verb list | 1 |
| `web/src/pages/docs/from-pm2.astro` | `:74` | the pm2 mapping row | 1 |
| `web/src/pages/docs/containers.astro` | `:64-75` | "Exit codes an orchestrator can act on": 0 and 11. Gains 12 and 13 as **reserved for dogs**, and the rule that shep will not take 12 or above | 4 |
| `web/src/pages/docs/json-output.astro` | `:44-72` | the full `ProcessInfo` JSON body. Gains `reload_deadline_ms` and `smit` | 5, 6 |
| `web/src/pages/docs/json-output.astro` | `:129-147` | the prose about null fields; both new fields are nullable for the same reason | 5, 6 |
| `web/src/pages/docs/output.astro` | `:37-42` | the wide 120-column `shep flock` sample. **Gains a SMIT column**, and 120 may no longer be wide enough -- paste a real render, do not hand-edit | 7 |
| `web/src/pages/docs/output.astro` | `:78-104` | "Narrow windows", including a hand-pasted 60-column render with a literal `CPU, EXIT, FOLD hidden.` footer and the never-drop rule at `:100-104`. **The footer changes and the drop order gets written down here** | 7 |
| `web/src/pages/docs/output.astro` | `:113-115` | the `shep flock \| cat` bare sample. Note bare drops nothing, so the smit is always there | 7 |
| `web/src/pages/docs/getting-started.astro` | `:109` | a plain flock header row | 7 |
| `web/src/pages/docs/lookout.astro` | `:265` | "narrow widths drop columns in this order: FOLD, then RESTARTS and PID" | 7 |

**Every sample table above must be a paste of a real render, not a hand-edit.** `output.astro:88`'s footer is the reason: it is a literal string the renderer produces, and a hand-edited one that disagrees is a lie nothing checks.

- [ ] **Step 3: Fix the staleness this sweep walks past**

Three pages carry `shep flock` samples that are **already** wrong, from the EXIT column landing on 2026-08-19, and this task is editing those exact tables:

- `web/src/pages/docs/folds.astro:62` and `:98` -- header rows with no EXIT column.
- `web/src/pages/docs/lookout.astro:19`, `:58`, `:81`, `:111`, `:135`, `:143`, `:155` -- TUI header rows with no EXIT column.
- `web/src/pages/docs/lookout.astro:265` -- the prose drop order, which names FOLD then RESTARTS then PID and omits EXIT, which the tiers give up second (`lookout/view/flock.rs:221-230`).

**Its own commit**, and say in the message that it predates this branch. Leaving a sample missing one column while adding another to it would be absurd, and adding SMIT on top of a stale row would bake the staleness in.

- [ ] **Step 4: Regenerate what is generated**

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```
```bash
git diff --stat web/src/data/cli-reference.generated.txt
```

Expect movement from Task 1 (`rehome` and `disable` help), Task 3 (`--quiet`'s help) and nothing else. **Read the diff.** A stale copy fails no build, which is exactly why it drifts, and `git diff` is the whole of the check.

The MCP tool schema moves too, because `whistle/facts.rs`'s `SheepRow` derives `JsonSchema` and gained two fields:

```bash
# regenerate docs/whistle/tools.md by whatever `docs/whistle/README.md` names
```

- [ ] **Step 5: Build AND check the site**

```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

**Both, and `check` is the one that catches a wrong prop.** Astro does not typecheck during a build. Measured 2026-08-20: `/docs/output` shipped two `<Callout kind="note">` against a component whose prop is `variant`, so the rendered `div` lost its variant class and the label badge rendered empty, and `astro build` was green throughout. `astro check` reported both at `ts(2322)` the moment it ran. `/docs/output` is a page this task edits heavily.

- [ ] **Step 6: Commit, in four**

Separate commits, because they are four different asks and each should be revertable on its own: the lexicon, the prose corrections, the pre-existing staleness, and the regenerated artifacts.

---

### Task 10: Changelogs, and the release that unblocks the dog

**Files:**
- Modify: `crates/shep-core/CHANGELOG.md`, `crates/shep-client/CHANGELOG.md`, `crates/shep-daemon/CHANGELOG.md`, `crates/shep-cli/CHANGELOG.md`

**Interfaces:**
- Consumes: Tasks 1 through 9.
- Produces: the published `shep-client` that `shep-deploy`'s Task 12 waits on.

**The changelogs are hand-written and release-plz will not do it.** `release-plz.toml` sets `changelog_update = false`, with its own reasoning recorded there: generation inserted 576 lines in a second style next to entries covering the same work, and copied a commit-message typo into a tracked file that the `typos` job then failed on. All four crates share one version through `[workspace.package]`.

- [ ] **Step 1: Write the `[Unreleased]` entries**

Keep a Changelog form, matching the depth of the existing entries -- `crates/shep-core/CHANGELOG.md:12-32` (the `last_exit` entry) is the model, and it is long because it carries the reasoning rather than the diff.

| Crate | Entries |
|---|---|
| `shep-core` | `ProcessInfo::reload_deadline_ms` and `ProcessInfo::smit`, both additive under `Option`, **`PROTOCOL_VERSION` stays 1** and say so in the words the `last_exit` entry uses. `Smit`, `SmitError`, `Request::SetSmit`, `Response::SmitPainted`. State explicitly that the deadline went on `ProcessInfo` rather than on `Response::Reloading` **because** reshaping a tuple variant under `#[serde(tag, content)]` would have been a retype and a version bump. |
| `shep-daemon` | reports each instance's reload deadline; accepts and stores smits, connection-scoped and never persisted; `ConnId`. |
| `shep-client` | `Client::reconnect`, `Client::reconnect_within`, `Reconnected`. Under `Added`, and a note under `Changed` that the crate now documents a `Client` as one connection that does not come back on its own. `crates/shep-client/CHANGELOG.md:21-22` says everything in that surface is a stability surface, so a new public method is an entry of its own. |
| `shep-cli` (published as `shep`) | `rehome` keeps `[dog.<name>]` -- **under `Changed`, not `Added`**, because it changes documented shipped behaviour; the on-remove hook; the SMIT column; `--quiet` covering the hook report; exit codes 12 and up reserved for dogs. |

**`rehome`'s entry is the one to get right.** It is the only behaviour change in this branch that an existing operator could notice without asking for it, and what they notice is that something they expected to be deleted was not. Say what changed, say the settings are theirs, and say that re-adopting now finds them waiting.

- [ ] **Step 2: Run the full gate, once, at the branch level**

Each as its own command, `$?` read directly, never through a pipe.

```bash
cargo fmt --all --check
```
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
```bash
cargo test --workspace --all-features
```
```bash
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Then the two cross-checks, which this branch genuinely needs rather than merely owing: Task 6 edits `crates/shep-daemon/src/server.rs`, which is `cfg(unix)`-gated, and Task 2 adds a `tokio::process` call whose Windows arm nobody here compiles.

```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```
```bash
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

Give the second its own `CARGO_TARGET_DIR` so it does not invalidate the host cache. It needs `brew install mingw-w64`, because `ring`'s build script runs `cc`.

**Then read CI, and do not call the branch green before you have.** `.github/workflows/test.yml` runs on every push and pull request, its `ubuntu-latest` and `ubuntu-24.04-arm` legs run the suite on real Linux, and its serial `slow` job is where the timing-sensitive tests live. Three separate breakages on 2026-08-19 were visible only there. Task 2's hook tests spawn real children and Task 6's e2e test waits on a real socket closing; if either proves contention-sensitive on a shared runner, it belongs in the `slow` job's skip list rather than being made to pass by widening a timeout.

- [ ] **Step 3: Commit, then open the PR**

```bash
git add crates/*/CHANGELOG.md
git commit -m "docs(changelog): dog prerequisites"
```

The repository has been on pull requests since 0.1.0 was published; the push-to-main window is closed. The PR body is public-facing prose: run `humanizer` then `rin-voice` over it, use bullets, and do not hard-wrap the paragraphs, because GitHub renders comment fields with hard line breaks on and an 80-column wrap becomes a ragged third-width column.

- [ ] **Step 4: Release, because the dog is waiting on crates.io**

`shep-deploy` depends on `shep-client` **from crates.io**, so nothing in this branch unblocks its Task 12 until this is published. Releasing is one act: merge the release pull request release-plz opens. Publish order is `shep-core`, then `shep-client` and `shep-daemon`, then `shep` (`docs/releasing.md`).

Then tell `shep-deploy`: bump `shep-client`, and Task 12 is unblocked.

---

## What this plan does not do

Named so that none of them looks like an oversight later.

- **`shep disable` does not run the on-remove hook.** Only `rehome` does. `disable` keeps the adoption and the settings and is the verb an operator reaches for to restart a dog or pause it during an incident; putting every sheep back on a `disable` would make the pause destructive. `shep-deploy`'s own `poll.rs` names that exact case as one `watch = "manual"` serves.
- **`shep adopt` gets no matching on-adopt hook.** Nothing asks for one, and `adopt` already spawns the binary once during vetting, so a hook there would be the second spawn in one command.
- **`lookout` keeps its own reconnect ladder** at 250ms x2 capped at 4s, rather than converging on the spec's 100ms x1.5 capped at 5s that Task 8 wires up. Argued under Task 8.
- **`HelloAck` gets no boot nonce.** Task 8 distinguishes daemons by pid and documents the residual.
- **The `PROTOCOL_VERSION` stays 1** and no response variant is reshaped. Task 5 argues why, and that argument is the one place this plan departs from a decision already recorded in `shep-deploy`'s ledger.
- **`ExitCode` gains no variants.** Task 4 argues why reserving beats defining.
- **Nothing arbitrates between two dogs painting one sheep.** Last writer wins, and Task 6 says so on purpose.
- **The spec's Name column keeps its spaces** where `code_str` uses underscores (`not found` against `not_found`). Task 4 deliberately does not smuggle that in.
- **Windows is untouched**, as everywhere else. Task 2's `tokio::process` call sits in `commands/`, which is already `#[cfg(unix)]`-gated at `main.rs`.

## For Rin

Five things, in the order they would cost most to get wrong.

1. **The reload deadline cannot be an additive field on the reload response, and the ledger says it can.** `Response::Reloading(Vec<ProcessInfo>)` is a tuple variant under `#[serde(tag, content)]`, so giving it a sibling turns `data` from an array into an object, which shep's own evolution rule calls a retype and a `PROTOCOL_VERSION` bump. The handshake compares versions for equality, so a bump stops every published client talking to every published daemon. **Task 5 puts the deadline on `ProcessInfo` instead**, which is genuinely additive, keeps the version at 1, and closes two more inferred inputs than the response shape would have: the dog stops guessing the two timeouts AND stops working out its own instance count. If you want the response shape anyway, this plan needs re-cutting around a protocol bump.
2. **`shep`'s `ExitCode` should not gain variants for 12 and 13.** The ledger says `exit.rs` owes rows. Task 4 argues it owes a **reservation** instead: shep has no code path meaning either, and the collision that actually matters comes from `shep <dogname>` passing a dog's exit code through unchanged. A five-line change adds the variants on top if you disagree.
3. **`shep-deploy` expects `--quiet` to govern any notice. It does not.** The flag is plumbed by hand into three commands and its own help says "Currently narrows `bleats`' own notices". Task 3 makes it true for the hook report and corrects the help text.
4. **`Client::reconnect` takes `&mut self`, which means it cannot be called through the `Arc<Client>` the crate's own docs recommend.** Deliberate, argued under Task 8: exclusive access at the moment a connection's identity changes is the point. The alternative is interior mutability, which slides back toward the transparent reconnect that same task argues against.
5. **Three of shep's own documents were found stating something false, and this plan fixes them in passing.** `docs/specs/shep-v1.md:451` gives `runtime` exit code 2 where the same section's table gives 11; two comments in `output/rows.rs` state the column drop order backwards; and three `web/` pages still show `shep flock` without the EXIT column that shipped on 2026-08-19. None was caused by this work.

## Self-review

Checked against the six items in the brief.

| Item | Task | Note |
|---|---|---|
| 1. `rehome` stops deleting `[dog.<name>]` | 1 | Nine restatement sites in `crates/`, and the rest in Task 9's table |
| 2. On-remove lifecycle hook | 2, 3 | The runner and the wiring, split so the process behaviour has its own gate |
| 3. Smits (blocks `shep-deploy` Task 12) | 6, 7 | Two-width exact-string test in Task 7, Step 2 |
| 4. Reload response carries the deadline | 5 | Delivered on `ProcessInfo`, not the response. Argued; Rin's call |
| 5. Exit rows 12 and 13 | 4 | Reserved, not defined. Argued; Rin's call |
| 6. `shep-client` reconnect | 8 | Ruled: a client change plus a documented contract, not a transparent retry |

**Dependencies.** Task 7 consumes Task 6 and cannot start before it. Task 3 consumes Task 2. Everything else is independent: 1, 2, 4, 5, 6 and 8 can run in parallel, with 5 and 6 both editing `ProcessInfo` and `request.rs` (expect one merge, and re-accept the wire snapshots once rather than twice). Task 9 runs after all of them. Task 10 last.

**Every task states its mutation and its expected failure**, and four of them name a mutation that is **not** caught and say so rather than inventing a test: Task 3's hook-inside-the-lock, Task 4's `DOG_RESERVED_FROM` value, Task 5's overflow arm, and Task 7's two widths collapsing to one. Naming an uncovered mutation is worth more than a test that would pass either way.
