# The sheep and dog config panes: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shep lookout` gains a config pane for a sheep and one for a dog, both rendering a JSON Schema through one shared field model.

**Architecture:** #124's settings screen is hardcoded to `shep.toml` across roughly thirty sites. It gets generalised into a field model that reads a JSON Schema, and lookout gains the scrolling it has never had. The sheep pane then points that model at `flockfile.schema.json` and the dog pane at a dog's own `--schema`.

**Tech Stack:** Rust 1.88, edition 2024. ratatui, insta snapshots, schemars 1.2.2, serde_json.

**Spec:** [docs/brainstorming/specs/2026-09-04-config-panes-design.md](../../brainstorming/specs/2026-09-04-config-panes-design.md). Read it. Decisions are cited below as "decision 4", meaning that document's.

## Global Constraints

- Clean-room rule, non-negotiable: never open, read, or port source from any pm2 checkout on this machine.
- Invoke the `shep-idiomatic-rust` skill before writing or reviewing any Rust. Cite rules as `IR-<n>`.
- **Every commit subject is a conventional commit.** `type(scope): summary`, with `!` on the commit that breaks something, in the crate that breaks. `.githooks/commit-msg` and `.github/workflows/commits.yml` both enforce this. Accepted types: `feat fix perf refactor docs test ci chore style`. `revert` and `build` are refused.
- No em dashes or en dashes anywhere: prose, code comments, commit messages.
- Never write a real person's name, a personal email, or an absolute home-directory path into a committed file or a commit message. Repo-relative paths only.
- Every new public item needs docs and a deliberate `Debug` decision, redacted for anything carrying env or a secret, with an exact-string test (IR-41).
- Prove every new test non-vacuous by mutating what it protects, watching that one test go red, and greping the file to confirm the patch applied before restoring.
- **Snippets below that describe existing code are guesses.** Signatures were read on 2026-09-04 but may have moved. Grep before relying on one; if it differs, follow the code and say so in your report.

## Commands

Inner loop, and the only shape any task uses:

```
cargo test -p shep --lib --all-features -- --skip ::slow::
```

Task gate, each from its own invocation with `$?` captured directly, never through a pipe:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Tasks 7 through 9 additionally run the web half, because they change what an operator sees:

```
cargo build --release
./web/scripts/generate-cli-reference.sh
cd web && npx astro build
cd web && npx astro check
```

## The regression surface

Seven full-frame snapshots cover the settings screen. Any layout change anywhere in `content_lines` re-diffs all of them, which is what makes them a real gate:

| snapshot | what it pins |
| --- | --- |
| `settings_fresh` | every scalar sourced from the default |
| `settings_set` | sources mixed between `shep.toml` and the default |
| `settings_confirm` | an armed confirm, at width 180 |
| `settings_typing` | the free-text editor open on `socket` |
| `settings_dogs` | three dogs, each drifting differently |
| `settings_narrow` | 45x24, the middle tier of both column tables |
| `settings_at_a_comfortable_width` | 120x30 through `view::draw` |

**Tasks 1 through 3 must leave all seven byte-identical.** A diff means the generalisation changed behaviour, which is the one thing it must not do.

---

# Slice 1a: the field model and scrolling

Lands on its own. No new pane, no wire change, no user-visible behaviour. The seven snapshots are the whole proof.

### Task 1: the generic field model

**Files:**
- Create: `crates/shep-cli/src/lookout/field.rs`
- Test: same file, `mod tests`

**Interfaces produced:** the types every later task consumes.

- [ ] **Step 1: write the failing tests first.** A `FieldSet` built from a small hand-written JSON Schema yields the fields in schema order; a field carrying `init.group` reports it; one carrying `x-shep-secret` reports secret; a `type: boolean` field reports a boolean kind and a `$ref` to `UpDuration` reports a string kind.

- [ ] **Step 2: the model.**

```rust
/// One editable field, read off a JSON Schema.
pub struct Field {
    /// The property name, which is also the key a write carries.
    pub key: String,
    /// What the operator reads. `init.blurb` where one exists, else the
    /// schema's `description`, else the key.
    pub help: String,
    /// `init.group`, absent for a schema that assigns none (decision 3).
    pub group: Option<String>,
    /// What the widget is.
    pub kind: FieldKind,
    /// The schema's own `default`, rendered.
    pub default: Option<String>,
    /// `x-shep-secret`. Renders as `<set>`, never read back.
    pub secret: bool,
    /// Whether the pane may edit it at all (decision 5).
    pub editable: bool,
}

pub enum FieldKind {
    Bool,
    Integer { minimum: Option<i64> },
    Text,
    /// A closed set, from `oneOf` of `const`s or from an `enum`.
    Choice(Vec<String>),
    /// A map, which opens a sub-screen rather than editing in place.
    Map,
    /// Something the pane cannot render. Shown read-only with its JSON.
    Opaque,
}

pub struct FieldSet { /* fields in schema order, plus group order */ }
```

- [ ] **Step 3: `FieldSet::from_schema`.** Walk the schema's `properties`, resolving one level of `$ref` into `$defs` to reach a named type's `type`. `anyOf` of `[T, null]` is `T` with the field optional. Do not recurse further: a nested object is `FieldKind::Opaque`, per decision 3's scope.

- [ ] **Step 4: group ordering.** `shep-core`'s `scaffold.rs` already has `GROUP_ORDER` and a `grouped_order()` reading `props[name]["init"]["group"]`, plus a `blurb()` reading `init.blurb` that panics when a field has none. **Reuse rather than re-derive**, exporting from `shep-core` if they are private. Grep both before writing anything.

- [ ] **Step 5: correct a stale comment while you are there.** `crates/shep-core/src/config/scaffold.rs` around line 120 says "half of `AppConfig` is currently ungrouped". Measured against the exported schema on 2026-09-04: 39 of 39 carry a group, none are ungrouped. Fix the comment. Its own commit, `docs(core):`.

- [ ] **Step 6: run the inner loop, then commit.** `feat(lookout): a field model read off a JSON Schema`

### Task 2: scrolling

**Files:**
- Modify: `crates/shep-cli/src/lookout/app.rs` (the `Settings` struct and its cursor movement)
- Modify: `crates/shep-cli/src/lookout/view/settings.rs` (`draw_settings`)

**Consumes:** nothing. **Produces:** a viewport every pane inherits.

Decision 8b. Today `draw_settings` takes `area.height` lines off the front of `content_lines()` with no skip, `Settings` holds no offset, and the cursor clamps to `rows().len()` with no notion of what was drawn.

- [ ] **Step 1: the failing test.** Build a `Settings` with more rows than a short viewport fits, move the cursor to the last row, render, and assert the selected row appears in the output. It fails today because the cursor sits below the fold.

- [ ] **Step 2: an offset on the screen state.** Add a scroll offset beside `cursor` and `pending`. Keep it private with an accessor, matching how `cursor` is already exposed.

- [ ] **Step 3: scroll into view on every cursor move.** The move functions clamp the cursor already; they now also pull the offset so the cursor is inside `[offset, offset + height)`. The height has to reach them, so thread the last drawn height onto the state at draw time rather than guessing it.

- [ ] **Step 4: `draw_settings` skips.** `.skip(offset).take(height)`.

- [ ] **Step 5: say there is more.** A line reading how many rows are hidden, or a scrollbar column. Whatever it is, it must not change any of the seven snapshots at their current sizes, which all fit without scrolling. If it would, put it behind "only when the content overflows".

- [ ] **Step 6: prove the seven snapshots are byte-identical.** `cargo insta test` or the repo's equivalent, and `git diff --stat` on the snapshot directories showing zero changed files. **This is the acceptance criterion for the task.**

- [ ] **Step 7: commit.** `feat(lookout): a viewport, so a screen can hold more rows than the terminal`

### Task 3: the settings screen moves onto the model

**Files:**
- Modify: `crates/shep-cli/src/lookout/app.rs`, `crates/shep-cli/src/lookout/view/settings.rs`, `crates/shep-cli/src/commands/settings.rs`

**Consumes:** Task 1's `Field`/`FieldSet`, Task 2's viewport.

The surface to replace, counted on 2026-09-04. Verify the count before starting, since it is the task's own definition of done:

- `app.rs`: `next_candidate`, `current_value`, `source_of`, `text_seed`, `rows`, `confirm_text`, `confirm_text_for_edit`, `typed_text_of`
- `view/settings.rs`: `scalar_view`, `field_label`, `apply_cost`, `section_for`
- `commands/settings.rs`: `load_settings`, `set_field`, `unset_field`

- [ ] **Step 1: build the `shep.toml` `FieldSet` by hand.** There is no JSON Schema for `shep.toml`, so this one is constructed in code from the six scalars, per decision 1's table. That is the point: the model is the common shape, not the schema.

- [ ] **Step 2: move the renderer.** `scalar_view`, `field_label` and `section_for` become reads off `Field`. `apply_cost` stays shep.toml-specific for now and is supplied by the caller, because a sheep's cost comes from `apply_group` and a dog has none (decision 4).

- [ ] **Step 3: move the state machine.** `rows` returns rows over the `FieldSet`. `next_candidate` reads `FieldKind::Choice`. `text_seed` reads `FieldKind::Text`.

- [ ] **Step 4: leave `Pending` alone.** lookout has two independent confirm mechanisms already, `Pending` for this screen and `Action`/`Stage` for the dashboard's `x`/`R`/`L`, sharing only `CONFIRM_EXPIRY`. Unifying them is real work and is NOT in this task. **Note it in your report** so the reviewer sees it was a decision rather than an oversight.

- [ ] **Step 5: the seven snapshots stay byte-identical.** Same acceptance criterion as Task 2, and the whole reason this slice exists.

- [ ] **Step 6: commit.** `refactor(lookout): the settings screen reads a field model`

---

# Slice 1b: the sheep pane

### Task 4: the wire, both variants, and the protocol bump

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs`, `crates/shep-core/src/protocol/mod.rs`
- Modify: `crates/shep-daemon/src/rpc.rs`
- Modify: the two wire snapshots under `crates/shep-core/src/protocol/snapshots/`

**Produces:** both requests the panes need. Both land here so the `PROTOCOL_VERSION` bump happens once.

**A missing handler does not fail to compile.** `Request` and `Response` are `#[non_exhaustive]` and `rpc.rs`'s dispatch ends in a wildcard answering "this daemon does not implement that request". The two wire snapshots are hand-written literal fixtures with no completeness check. So a variant with no arm silently answers an internal error at runtime, and nothing catches it.

- [ ] **Step 1: the sheep read variant.** Carries a sheep's name, answers with its effective `AppConfig` with `env` reduced to key names (decision 6b). Decide the reply type's shape and give it a redacted `Debug` with an exact-string test, since it carries a config.

- [ ] **Step 2: the dog write variant.** Carries a dog's name and the edit.

- [ ] **Step 3: handlers for both, and a test that each is reachable.** The test is the point: without one, the wildcard arm hides a missing handler.

- [ ] **Step 4: bump `PROTOCOL_VERSION` to 4** in `crates/shep-core/src/protocol/mod.rs`. Two tests pin the numeral rather than reading the constant and must move: `hello_handshake_shape` and `a_dogs_hello_names_the_dog_and_nothing_elses_does`, both in `request.rs`, around lines 3126 and 3144. **Several other files hardcode "protocol 1" and "protocol 2" deliberately, simulating an older daemon. Do not touch those.**

- [ ] **Step 5: add a row to each wire snapshot.**

- [ ] **Step 6: commit.** Two commits, and the bump is the breaking one: `feat(core)!: PROTOCOL_VERSION 4, for the two config-pane requests`

### Task 5: the sheep pane

**Files:**
- Create: `crates/shep-cli/src/lookout/view/sheep_config.rs` or equivalent
- Modify: `crates/shep-cli/src/lookout/app.rs`, `crates/shep-cli/src/lookout/input.rs`

- [ ] **Step 1: `e` opens it** on the selected flock-table row (decision 8). Taken keys as of 2026-09-04: `/ G L R W c g j k q r s x z`. `input.rs` is currently settings-agnostic, so check whether `e` needs a new `KeyPress` or reuses one.

- [ ] **Step 2: build the `FieldSet` from `flockfile_schema_json()`.** It is embedded via `include_str!` in `crates/shep-core/src/config/flockfile.rs`, so there is no disk read and no failure path for a missing file.

- [ ] **Step 3: four sections from `init.group`,** in `GROUP_ORDER`: process, inputs, control, cron. Every exported field carries one, so a field without a group is a bug: say so rather than inventing a section.

- [ ] **Step 4: the cost badge per row** from `apply_group` (decision 4), and Structural fields read-only (decision 5).

- [ ] **Step 5: env is one row that opens a sub-screen** (decision 9), write-only. A literal shows `<set>`, a `{{shared:X}}` reference shows in full.

- [ ] **Step 6: a rendered frame test per width,** matching how `view/settings.rs` already tests itself, plus one at a height that forces scrolling.

- [ ] **Step 7: commit.** `feat(lookout): a config pane for a sheep`

### Task 6: the sheep pane writes

- [ ] **Step 1: an edit sends `Request::ApplyConfig`.** It already exists and already answers `Response::Applied(Vec<SheepApplied>)`, whose `applied`, `pending` and `refused` fields are what the pane reports.

- [ ] **Step 2: a `NeedsRespawn` edit arms a confirm naming what dies** (decision 4). Use the settings screen's `Pending`, not a third mechanism.

- [ ] **Step 3: test that a field in each of the four groups lands,** proven through `drifted_fields`, which returns the sorted names whose values differ.

- [ ] **Step 4: commit.** `feat(lookout): a sheep pane edit reaches the overrides store`

---

# Slice 2: the dog pane

### Task 7: a lock the daemon can use

**Files:**
- Move into `crates/shep-core/`: `ConfigLock` and `create_config_file`, today `shep-cli`-private in `crates/shep-cli/src/commands/shep_toml.rs`

Decision 6, corrected. The daemon holds no lock on `dogs.toml` and its only relationship to the file is read-only. `shep-core`'s `overrides.rs` already has a sibling-lockfile scheme that both sides can use.

- [ ] **Step 1: decide which way.** Either move `ConfigLock` into shep-core, or give `dogs.toml` the `overrides.rs` scheme. **Say which and why in your report.** Do not write a third implementation.

- [ ] **Step 2: keep the CLI's three writers working.** Migration, `shep rehome`'s forget half, and whatever else `git grep write_dogs_config` finds.

- [ ] **Step 3: the lock ordering survives.** When both `shep.toml` and `dogs.toml` locks are held, `shep.toml`'s is outer. `dog_migration.rs`'s header comment states this; do not invert it.

- [ ] **Step 4: commit.** `refactor(core): the config lock moves where both the daemon and the CLI can hold it`

### Task 8: the daemon writes a dog's section and publishes

- [ ] **Step 1: the handler writes `dogs.toml`.** In `rpc.rs`, not the actor: `RpcContext` already carries `events: Bus` and `dogs_config: PathBuf` in scope where dispatch runs, and `dogs.toml` is not supervisor state the way `overrides.json` is.

- [ ] **Step 2: it publishes `config.dog.<name>`** through `publish_dog_config_changed` in `crates/shep-daemon/src/bus.rs`.

- [ ] **Step 3: the file stays hand-editable.** `DogsConfig`'s doc calls it "deliberately not a locked shep-owned store", so a concurrent hand edit must not be clobbered silently. Read, modify and write under the lock.

- [ ] **Step 4: test that a subscriber receives the event,** not that the publish site was called. The dog contract's own `bark_subscribes_to_its_own_config_topic` is the shape to copy.

- [ ] **Step 5: commit.** `feat(daemon): a dog's section can be written over the wire`

### Task 9: the dog pane, and the docs

- [ ] **Step 1: `e` on a dog row** opens the pane, flat in schema order, no sections (decision 3).

- [ ] **Step 2: a dog with no schema gets no pane** (decision 10). The refusal names `$EDITOR` on `dogs.toml`.

- [ ] **Step 3: `x-shep-secret` renders `<set>`,** replaceable, never read back.

- [ ] **Step 4: no cost badge,** and one line at the foot saying the dog decides for itself (decision 4).

- [ ] **Step 5: the docs.** `web/src/pages/docs/lookout.astro` for both panes and `e`. `web/src/pages/docs/overrides.astro` for the sheep pane. `docs/dogs.md` and `web/src/pages/docs/dogs.astro` for what a dog author gets by publishing a schema. `getting-started.astro` for the protocol bump. Run the web half of the gate.

- [ ] **Step 6: commit.** `feat(lookout): a config pane for a dog`

---

## Out of scope, deliberately

- **Unifying `Pending` and `Action`/`Stage`.** lookout has two confirm mechanisms sharing only a constant. Task 3 leaves them alone and says so. Worth its own pass.
- **Recursing into nested objects.** `FieldKind::Opaque` covers them read-only. A dog with a nested config gets a pane that shows the field and will not edit it.
- **A live-versus-needs-restart axis for dog fields.** The dog spec owns that and left it out.
