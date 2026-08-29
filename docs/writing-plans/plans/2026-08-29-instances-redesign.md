# Instances Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the instance slot on the wire, group multi-instance apps everywhere an operator looks, and replace `increment_var` with `{{instance}}` templating.

**Architecture:** One new wire field (`ProcessInfo.instance: Option<u32>`) unlocks everything else: a `name:slot` selector, a grouped flock table, two lookout row kinds, and slot-labelled log lines. Separately, a small template module in shep-core is validated by `normalize` and applied by `assemble`, which is the single seam every spawn path already passes through.

**Tech Stack:** Rust 2024, MSRV 1.88. shep-core (protocol, config, selector), shep-daemon (supervisor, assemble), shep (the CLI, package name `shep`, directory `crates/shep-cli`).

**Spec:** [docs/brainstorming/specs/2026-08-29-instances-design.md](../../brainstorming/specs/2026-08-29-instances-design.md). decisions are cited as D1..D11 throughout. Read it alongside this plan.

## Global Constraints

- **Clean-room rule.** Never open `~/GitHub/pm2`. Work from the spec and this plan only.
- **Inner loop, daemon:** `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`
- **Inner loop, CLI:** `cargo test -p shep --lib --bins --all-features -- --skip ::slow::`. The package is `shep`, not `shep-cli`; `-p shep-cli` runs zero tests and exits 0.
- **Inner loop, core:** `cargo test -p shep-core --lib --all-features`
- **One cargo shape per task.** Do not alternate `--workspace` with `-p <crate>` inside a task; the workspace shares one target-dir build lock and switching shapes invalidates caches.
- **Task gate** (run once per task, when the task is otherwise done, one command at a time):
  ```
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
  ```
- **No em or en dash in any user-facing string.** `normalize.rs` already has a test asserting this (`crates/shep-core/src/config/normalize.rs:764-772`): `assert!(!rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'))`. Every new error message follows it.
- **Every new public item needs a doc comment**, and every fallible public function needs an `# Errors` section. `RUSTDOCFLAGS="-D warnings"` enforces the first.
- **Error-message tests use `assert!(rendered.contains(...), "{rendered}")`**, never `assert_eq!` on a whole `Display` string. There is no exact-string precedent for `NormalizeError` in the codebase.
- **Invoke the `shep-idiomatic-rust` skill** before writing Rust here. Cite rules as `IR-<n>` in review.
- **Commit per task**, conventional style.

## File Structure

Created:

- `crates/shep-core/src/config/template.rs`: the `{{instance}}` / `{{name}}` grammar. Validation (used by `normalize`) and rendering (used by `assemble`) live together so the two can never disagree about what a token is.

Modified:

- `crates/shep-core/src/protocol/request.rs`: `ProcessInfo.instance`, its builder setter, `sort_flock`, `SelectorSpec::Instance`.
- `crates/shep-core/src/selector.rs`: `ProcessSelector::Instance`, parse, `matches`, both conversions.
- `crates/shep-core/src/config/normalize.rs`: colon ban, reserved env vars, template validation, the D8 refusal, the `increment_var` rejection.
- `crates/shep-core/src/config/app.rs`: remove `increment_var` from the live field set.
- `crates/shep-daemon/src/assemble.rs`: `SHEP_NAME`, template rendering across env, args and both log paths.
- `crates/shep-daemon/src/rpc.rs`, `crates/shep-daemon/src/supervisor.rs`: fill `instance` on every row; `matches` call sites.
- `crates/shep-cli/src/output/rows.rs`: grouped `FlockRows`.
- `crates/shep-cli/src/commands/bleats.rs`: backlog dedup, slot labels.
- `crates/shep-cli/src/lookout/app.rs` and `view/`: two row kinds.
- `crates/shep-cli/src/commands/import/`: pm2 `instance_var` to an env entry.

## Task Order and Dependencies

```
Task 1  (bleats dedup)          independent, lands first
Task 2  (ProcessInfo.instance)  foundation for 4, 9, 10, 11
Task 3  (colon ban)             independent
Task 4  (name:slot selector)    needs 2 and 3
Task 5  (SHEP_NAME, reserved)   independent
Task 6  (template module)       needs 5
Task 7  (log path templating)   needs 6
Task 8  (increment_var removal) needs 6
Task 9  (grouped flock table)   needs 2
Task 10 (bleats slot labels)    needs 1 and 2
Task 11 (lookout row kinds)     needs 2 and 9
Task 12 (docs, schema, reference) needs all
```

Tasks 3, 5 and 9 can run alongside 2 without conflict. Tasks 1 and 10 touch the same file and must be sequential.

---

### Task 1: Bleats reads each distinct path once

Implements D10. Independent of every other task: land it first.

**Files:**
- Modify `crates/shep-cli/src/commands/bleats.rs:312-395` (`tail_log_files`)
- Test same file, `mod tests`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: nothing other tasks rely on. Task 10 edits the same function afterwards.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/shep-cli/src/commands/bleats.rs`. The existing `info` helper builds distinct paths per id, so this test builds its own rows sharing one path.

```rust
#[test]
fn instances_sharing_one_log_file_are_read_once_not_once_each() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shared = dir.path().join("talker-out.log");
    std::fs::write(&shared, "line one\nline two\n").expect("write");

    let shared_path = shared.to_string_lossy().to_string();
    let mut cache = HashMap::new();
    for id in 0..2u32 {
        cache.insert(
            id,
            ProcessInfo::builder(id, "talker", ProcStatus::Online)
                .out_file(Some(shared_path.clone()))
                .err_file(Some(shared_path.clone()))
                .build(),
        );
    }

    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams::test(&mut out, &mut err);
    let args = bleats_args("talker", true, false, true);
    let selector = ProcessSelector::parse("talker").expect("selector");

    tail_log_files(&mut streams, false, &cache, &selector, &args);

    let printed = String::from_utf8(out).expect("utf8");
    assert_eq!(
        printed.matches("line one").count(),
        1,
        "one file, one read, however many instances point at it:\n{printed}"
    );
}
```

Check the `Streams` test constructor's real name before running: grep `impl Streams` in `crates/shep-cli/src/output/mod.rs` and use whatever the neighbouring bleats tests use. If `tempfile` is not already a dev-dependency of the `shep` package, use the same temp-directory helper the other file-reading tests in this module use rather than adding a dependency.

- [ ] **Step 2: Run the test and watch it fail**

```bash
cargo test -p shep --lib --all-features instances_sharing_one_log_file
```

Expected: FAIL, with the count at 2 rather than 1.

- [ ] **Step 3: Dedup the reads**

In `tail_log_files`, after the `matched.sort_unstable_by(...)` line and before the `for info in matched` loop, add the two sets. Then guard each arm.

```rust
    // One file, one read. Several instances can resolve to one path: every
    // `merge_logs` app does, and so does any app that set `out_file`
    // explicitly. Reading per row printed the file once per instance.
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut seen_notices: HashSet<String> = HashSet::new();
```

Inside the `for (stream_name, path, show)` loop, replace the `match path` arms:

```rust
            match path {
                None => {
                    // No path to key on, since the missing field is why this
                    // fires. The message already names the pair that varies.
                    let message =
                        format!("{name}: the daemon did not report a {stream_name} log path");
                    if seen_notices.insert(message.clone()) {
                        write_notice(streams, quiet, "log_path_unknown", &message);
                    }
                }
                Some(path) => {
                    if !seen_paths.insert(path.to_string()) {
                        continue;
                    }
                    match read_tail(Path::new(path), args.lines) {
                        // ... existing Ok and Err arms unchanged
                    }
                }
            }
```

Add `use std::collections::HashSet;` to the module's imports if it is not already there.

- [ ] **Step 4: Run the test and watch it pass**

```bash
cargo test -p shep --lib --all-features instances_sharing_one_log_file
```

Expected: PASS.

- [ ] **Step 5: Run the module's other bleats tests**

```bash
cargo test -p shep --lib --bins --all-features -- --skip ::slow:: bleats
```

Expected: PASS. If a pre-existing test asserted the duplicated output, it was pinning the bug: update it and say so in the commit body.

- [ ] **Step 6: Run the task gate, then commit**

```bash
git add crates/shep-cli/src/commands/bleats.rs
git commit -m "fix(bleats): read each log path once, not once per instance"
```

The commit body should carry the measured numbers from the spec: a 938-line file printed 1876 lines before the fix.

---

### Task 2: ProcessInfo carries the instance slot

Implements D2. Foundation for tasks 4, 9, 10 and 11.

**Files:**
- Modify `crates/shep-core/src/protocol/request.rs:604-688` (struct), `:738-758` (builder seed), `:781+` (setter), `:690-722` (`sort_flock`)
- Modify `crates/shep-daemon/src/rpc.rs`, `crates/shep-daemon/src/supervisor.rs`: every `ProcessInfo::builder(...)` call that has a `ProcessEntry` in hand
- Test `crates/shep-core/src/protocol/request.rs` `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: `ProcessInfo.instance: Option<u32>`; `ProcessInfoBuilder::instance(Option<u32>) -> Self`. Tasks 4, 9, 10 and 11 all read the field.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn an_instance_slot_survives_a_round_trip_and_defaults_to_absent() {
    let with = ProcessInfo::builder(1, "web", ProcStatus::Online)
        .instance(Some(2))
        .build();
    assert_eq!(with.instance, Some(2));

    let without = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
    assert_eq!(
        without.instance, None,
        "a row nobody set a slot on says so, rather than claiming slot 0"
    );
}

#[test]
fn a_reply_from_a_daemon_without_the_field_deserializes_as_absent() {
    // The skew case the Option exists for: an older shepherd's JSON has no
    // `instance` key at all.
    let json = r#"{"id":1,"name":"web","status":"Online","pid":null,
        "restarts":0,"uptime_ms":0,"fold":null,"out_file":null,
        "err_file":null,"cpu_percent":null,"memory_bytes":null,"dog":null,
        "lambs":null,"last_exit":null,"smit":null}"#;
    let info: ProcessInfo = serde_json::from_str(json).expect("older reply still parses");
    assert_eq!(info.instance, None);
}

#[test]
fn sort_flock_orders_by_slot_before_id() {
    // A reload gave slot 0 a fresh, higher id. Slot order must still win.
    let mut listing = vec![
        ProcessInfo::builder(9, "web", ProcStatus::Online)
            .instance(Some(0))
            .build(),
        ProcessInfo::builder(2, "web", ProcStatus::Online)
            .instance(Some(1))
            .build(),
    ];
    sort_flock(&mut listing);
    assert_eq!(
        listing.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![9, 2],
        "slot 0 leads even though its id is higher"
    );
}

#[test]
fn sort_flock_falls_back_to_id_when_no_row_carries_a_slot() {
    let mut listing = vec![
        ProcessInfo::builder(5, "web", ProcStatus::Online).build(),
        ProcessInfo::builder(3, "web", ProcStatus::Online).build(),
    ];
    sort_flock(&mut listing);
    assert_eq!(
        listing.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![3, 5],
        "an older daemon's listing sorts exactly as it does today"
    );
}
```

The JSON literal must match the struct's real field set at the time you write it. If a field has been added since this plan, add it to the literal; the point of the test is the ABSENT `instance` key, not the presence of the others.

- [ ] **Step 2: Run the tests and watch them fail**

```bash
cargo test -p shep-core --lib --all-features instance
```

Expected: FAIL to compile, "no method named `instance`".

- [ ] **Step 3: Add the field, the setter and the sort**

In the struct, after `smit`:

```rust
    /// Which instance slot of its app this sheep occupies, counting from 0.
    ///
    /// `None` when the peer daemon predates the field, the same skew rule
    /// [`Self::out_file`] documents for itself. Deliberately not a bare
    /// `u32` defaulted to 0: an app stocked to four instances would then
    /// report four rows all claiming slot 0, which is the silently-wrong
    /// zero [`Self::dog`] warns against. A reader that finds `None` should
    /// render exactly what it rendered before this field existed.
    pub instance: Option<u32>,
```

Add `instance: None` to the builder's seed literal in `ProcessInfo::builder`, and the setter beside the others:

```rust
    /// Sets the instance slot; `None` when the peer daemon predates the field.
    pub fn instance(mut self, instance: Option<u32>) -> Self {
        self.info.instance = instance;
        self
    }
```

Replace `sort_flock`'s body:

```rust
pub fn sort_flock(listing: &mut [ProcessInfo]) {
    listing.sort_unstable_by(|a, b| {
        (a.name.as_str(), a.instance, a.id).cmp(&(b.name.as_str(), b.instance, b.id))
    });
}
```

Update `sort_flock`'s doc: the paragraph beginning "A richer `(name, instance, id)` order would be more stable" argued for this order and ruled it out because `ProcessInfo` carried no instance number. Rewrite it to say the field now exists, that the order is taken, and that a listing of all-`None` rows collapses to `(name, id)`.

- [ ] **Step 4: Fill the field wherever the daemon builds a row**

Find the sites:

```bash
rg 'ProcessInfo::builder' crates/shep-daemon/src
```

Every site that has a `ProcessEntry` in hand gains `.instance(Some(entry.instance))`. A site that builds a row for something with no slot leaves the setter off. Do not add `#[serde(default)]` anywhere; `Option` already deserializes an absent key as `None`.

- [ ] **Step 5: Run the tests and watch them pass**

```bash
cargo test -p shep-core --lib --all-features instance
```

Expected: PASS, all four.

- [ ] **Step 6: Run the daemon suite**

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

Expected: PASS. Some tests may assert on flock ordering; a failure here is real signal about the new sort, not noise to suppress.

- [ ] **Step 6b: Bump the JSON envelope version**

`SCHEMA_VERSION` in `crates/shep-cli/src/output/mod.rs` describes the shape a `--format json` consumer parses, and every flock row now carries a field it did not before. Bump it once here; task 10's `BleatLine.instance` rides the same bump rather than taking a second.

```bash
rg 'SCHEMA_VERSION' crates/shep-cli/src/output/mod.rs
```

If a test pins the version number, update it in the same commit.

- [ ] **Step 7: Run the task gate, then commit**

```bash
git add crates/shep-core/src/protocol/request.rs crates/shep-daemon/src crates/shep-cli/src/output/mod.rs
git commit -m "feat(protocol): put the instance slot on the wire"
```

---

### Task 3: Names may not contain a colon

Implements the first half of D3. Independent.

**Files:**
- Modify `crates/shep-core/src/config/normalize.rs:276-278` (the check), `:474-476` (the variant doc), `:603-608` (the Display arm)
- Test same file, `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: a guarantee task 4 depends on, that no sheep name contains `:`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn a_colon_in_a_name_is_refused_because_it_is_the_instance_separator() {
    let err = normalize(AppConfig::minimal("web:2", "./srv")).unwrap_err();
    assert_eq!(err, NormalizeError::InvalidName("web:2".to_string()));

    let rendered = err.to_string();
    assert!(rendered.contains(':'), "says which character: {rendered}");
    assert!(
        !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
        "no em or en dash in copy a user reads: {rendered}"
    );

    assert!(normalize(AppConfig::minimal("web-2", "./srv")).is_ok());
}
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep-core --lib --all-features a_colon_in_a_name
```

Expected: FAIL, the name normalizes fine today.

- [ ] **Step 3: Add the colon to the refused set**

```rust
    if app.name.contains(['/', '\\', ':']) || app.name == "." || app.name == ".." {
        return Err(NormalizeError::InvalidName(app.name));
    }
```

Update the `InvalidName` Display arm to name the colon and say why, without a dash:

```rust
            Self::InvalidName(n) => {
                write!(
                    f,
                    "sheep name `{n}` may not contain a path separator or a colon, or be `.` or `..`"
                )
            }
```

Update the variant's doc comment to carry both reasons: a path separator would escape the shep home, and a colon is the `name:slot` separator and is also illegal in a Windows filename, which a sheep name becomes part of.

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p shep-core --lib --all-features a_colon_in_a_name
```

Expected: PASS.

- [ ] **Step 5: Run the core suite, then the gate, then commit**

```bash
cargo test -p shep-core --lib --all-features
```

```bash
git add crates/shep-core/src/config/normalize.rs
git commit -m "feat(config): refuse a colon in a sheep name"
```

---

### Task 4: `name:slot` selects one instance

Implements the second half of D3. Needs tasks 2 and 3.

**Files:**
- Modify `crates/shep-core/src/selector.rs`: the enum, `parse`, `is_exact`, `matches`, the `From` impl, the `TryFrom` impl
- Modify `crates/shep-core/src/protocol/request.rs:37-48`: `SelectorSpec::Instance`
- Modify every `matches(` call site
- Test `crates/shep-core/src/selector.rs` `mod tests`

**Interfaces:**
- Consumes: `ProcessInfo.instance` from task 2; the colon ban from task 3.
- Produces: `ProcessSelector::Instance { name: String, slot: u32 }`, and `ProcessSelector::matches(&self, name: &str, id: u32, fold: Option<&str>, instance: Option<u32>) -> bool` (a fourth parameter, so every call site changes).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn an_instance_form_parses_and_matches_only_its_slot() {
    let sel = ProcessSelector::parse("web:2").expect("parses");
    assert!(matches!(
        &sel,
        ProcessSelector::Instance { name, slot } if name == "web" && *slot == 2
    ));
    assert!(sel.matches("web", 7, None, Some(2)));
    assert!(!sel.matches("web", 7, None, Some(1)));
    assert!(!sel.matches("api", 7, None, Some(2)));
    assert!(
        !sel.matches("web", 7, None, None),
        "an older daemon's row carries no slot, so it cannot be the one asked for"
    );
}

#[test]
fn an_instance_selector_names_one_entry_so_it_is_exact() {
    // The dog rule: an operator who named it reaches it, a wildcard does not.
    assert!(ProcessSelector::parse("metrics:0").expect("parses").is_exact());
}

#[test]
fn the_colon_forms_do_not_shadow_each_other() {
    assert!(matches!(
        ProcessSelector::parse("fold:web").expect("parses"),
        ProcessSelector::Fold(_)
    ));
    assert!(matches!(
        ProcessSelector::parse("web:2").expect("parses"),
        ProcessSelector::Instance { .. }
    ));
    // A trailing segment that is not a number is not a slot. Names cannot
    // hold a colon any more, so this is a name that will simply match nothing.
    assert!(matches!(
        ProcessSelector::parse("web:two").expect("parses"),
        ProcessSelector::Name(_)
    ));
    // A glob is still a glob: the glob test runs first.
    assert!(matches!(
        ProcessSelector::parse("web*:2").expect("parses"),
        ProcessSelector::Regex(_)
    ));
    // An id is still an id.
    assert!(matches!(
        ProcessSelector::parse("11").expect("parses"),
        ProcessSelector::Id(11)
    ));
}

#[test]
fn an_instance_selector_round_trips_through_the_wire_form() {
    let sel = ProcessSelector::parse("web:2").expect("parses");
    let spec = crate::protocol::SelectorSpec::from(&sel);
    assert_eq!(
        spec,
        crate::protocol::SelectorSpec::Instance {
            name: "web".to_string(),
            slot: 2
        }
    );
    let back = ProcessSelector::try_from(spec).expect("converts back");
    assert!(matches!(back, ProcessSelector::Instance { .. }));
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep-core --lib --all-features instance_form
```

Expected: FAIL to compile, no `Instance` variant.

- [ ] **Step 3: Add the variant and the parse arm**

In `crates/shep-core/src/selector.rs`, add to `ProcessSelector`:

```rust
    /// One instance of one app, written `name:slot` on the CLI
    Instance {
        /// The app name, which cannot itself contain a colon
        name: String,
        /// The instance slot, counting from 0
        slot: u32,
    },
```

In `parse`, after the glob arm and before the final `Ok(Self::Name(...))`:

```rust
        // Last, so every earlier form wins: `fold:` is a prefix test above,
        // a glob containing a colon was already turned into a regex, and an
        // all-digit input was already an id. A name cannot contain a colon
        // (`config::normalize` refuses one), so splitting on the last colon
        // cannot cut a name in half.
        if let Some((name, slot)) = input.rsplit_once(':')
            && !name.is_empty()
            && !slot.is_empty()
            && slot.bytes().all(|b| b.is_ascii_digit())
            && let Ok(slot) = slot.parse()
        {
            return Ok(Self::Instance {
                name: name.to_string(),
                slot,
            });
        }
```

Update the module's precedence doc at the top of the file to read:
`all` > `fold:<name>` > `/regex/` > all-digits id > glob > `name:slot` > name.

- [ ] **Step 4: Extend `is_exact` and `matches`**

```rust
    pub const fn is_exact(&self) -> bool {
        match self {
            Self::Id(_) | Self::Name(_) | Self::Instance { .. } => true,
            Self::All | Self::Regex(_) | Self::Fold(_) => false,
        }
    }

    pub fn matches(&self, name: &str, id: u32, fold: Option<&str>, instance: Option<u32>) -> bool {
        match self {
            Self::All => true,
            Self::Id(want) => *want == id,
            Self::Name(want) => want == name,
            Self::Regex(re) => re.is_match(name),
            Self::Fold(want) => fold == Some(want.as_str()),
            // `None` means the peer daemon predates the slot field, so this
            // row cannot be shown to be the one asked for. Refusing to match
            // is the safe direction: a restart reaches nothing rather than
            // reaching every instance of the name.
            Self::Instance { name: want, slot } => want == name && instance == Some(*slot),
        }
    }
```

Add a `# Panics`-free `Instance` arm to the `From<&ProcessSelector> for SelectorSpec` impl and to the `TryFrom<SelectorSpec>` impl, and add the wire variant:

```rust
    /// By app name and instance slot
    Instance {
        /// The app name
        name: String,
        /// The instance slot, counting from 0
        slot: u32,
    },
```

- [ ] **Step 5: Update every `matches` call site**

```bash
rg '\.matches\(' crates/
```

Each site gains a fourth argument. In the daemon, pass the entry's slot as `Some(entry.instance)`. In the CLI, pass `info.instance`. Do not pass `None` to silence a compile error: a site that cannot supply a slot needs a comment saying why.

- [ ] **Step 6: Run the tests and watch them pass**

```bash
cargo test -p shep-core --lib --all-features
```

Expected: PASS.

- [ ] **Step 7: Run the workspace suite, then the gate, then commit**

```bash
cargo test --workspace --all-features
```

```bash
git add crates/
git commit -m "feat(selector): address one instance as name:slot"
```

---

### Task 5: SHEP_INSTANCE and SHEP_NAME are injected and reserved

Implements D6.

**Files:**
- Modify `crates/shep-daemon/src/assemble.rs:200-210`
- Modify `crates/shep-core/src/config/normalize.rs`: a new variant, its Display arm, the check
- Test both files

**Interfaces:**
- Consumes: nothing.
- Produces: `NormalizeError::ReservedEnvVar { name: String, var: &'static str }`, and the guarantee that `SHEP_INSTANCE` and `SHEP_NAME` are always in a child's environment. Task 6 builds the template grammar on top.

- [ ] **Step 1: Write the failing tests**

In `crates/shep-daemon/src/assemble.rs` `mod tests`:

```rust
#[test]
fn every_child_learns_its_slot_and_its_name() {
    let app = normalize(AppConfig {
        name: "worker".to_string(),
        script: "bin/worker".to_string(),
        ..Default::default()
    })
    .unwrap();
    let spec = assemble(&app, 3, &test_paths(), None);
    assert_eq!(spec.env.get("SHEP_INSTANCE").map(String::as_str), Some("3"));
    assert_eq!(spec.env.get("SHEP_NAME").map(String::as_str), Some("worker"));
}
```

In `crates/shep-core/src/config/normalize.rs` `mod tests`:

```rust
#[test]
fn the_reserved_env_vars_are_refused_rather_than_overwritten() {
    for var in ["SHEP_INSTANCE", "SHEP_NAME"] {
        let mut app = AppConfig::minimal("web", "./srv");
        app.env.insert(var.to_string(), "mine".to_string());
        let err = normalize(app).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains(var), "names the variable: {rendered}");
        assert!(
            !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
            "no em or en dash in copy a user reads: {rendered}"
        );
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep-core --lib --all-features reserved_env
```

Expected: FAIL, normalize accepts the key today.

- [ ] **Step 3: Inject both, and refuse both**

In `assemble.rs`, replace the `slot_var` lines:

```rust
    // Always, and under fixed names. An app that wants the slot under its own
    // variable writes `MY_VAR = "{{instance}}"` in its env, which is one
    // mechanism instead of a dedicated config knob for a single value.
    env.insert("SHEP_INSTANCE".to_string(), instance.to_string());
    env.insert("SHEP_NAME".to_string(), name.clone());
```

In `normalize.rs`, add the variant:

```rust
    /// An app's `env` sets a variable shep injects itself. Carries the sheep
    /// name and the variable, so the error names the entry to edit.
    ReservedEnvVar {
        /// The sheep name
        name: String,
        /// The variable the app tried to set
        var: &'static str,
    },
```

Its Display arm:

```rust
            Self::ReservedEnvVar { name, var } => write!(
                f,
                "sheep `{name}` sets `{var}` in env, but shep injects it: use a different name, or `{{{{instance}}}}` in your own variable"
            ),
```

And the check, after the name checks and before `expand_paths`:

```rust
    for var in ["SHEP_INSTANCE", "SHEP_NAME"] {
        if app.env.contains_key(var) {
            return Err(NormalizeError::ReservedEnvVar {
                name: app.name.clone(),
                var,
            });
        }
    }
```

- [ ] **Step 4: Fix the test the old knob owned**

`crates/shep-daemon/src/assemble.rs`'s `env_custom_increment_var` asserts `SHEP_INSTANCE` is ABSENT when `increment_var` is set. That is now false by design. Task 8 deletes the field; for this task, update the test to assert both variables are present alongside the custom one, and leave the field itself alone.

- [ ] **Step 5: Run both suites and watch them pass**

```bash
cargo test -p shep-core --lib --all-features
```

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

Expected: PASS.

- [ ] **Step 6: Run the task gate, then commit**

```bash
git add crates/shep-core/src/config/normalize.rs crates/shep-daemon/src/assemble.rs
git commit -m "feat(config): always inject SHEP_INSTANCE and SHEP_NAME, and reserve them"
```

---

### Task 6: The `{{instance}}` and `{{name}}` grammar

Implements D7. Needs task 5.

**Files:**
- Create `crates/shep-core/src/config/template.rs`
- Modify `crates/shep-core/src/config/mod.rs`: declare and re-export
- Modify `crates/shep-core/src/config/normalize.rs`: validate env values and args
- Modify `crates/shep-daemon/src/assemble.rs`: render env values and args
- Test `template.rs`, `normalize.rs`, `assemble.rs`

**Interfaces:**
- Consumes: the reserved-variable rule from task 5.
- Produces:
  - `shep_core::config::template::validate(value: &str) -> Result<(), TemplateError>`
  - `shep_core::config::template::render(value: &str, name: &str, instance: u32) -> String`
  - `TemplateError::UnknownToken { token: String }`
  - `NormalizeError::BadTemplate { name: String, field: String, reason: String }`

  Task 7 calls `validate` and `render` on the two log-path fields.

- [ ] **Step 1: Write the failing tests for the grammar**

Create `crates/shep-core/src/config/template.rs` with only the test module first, so the failure is real:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_tokens_render() {
        assert_eq!(render("z-{{instance}}", "worker", 3), "z-3");
        assert_eq!(render("{{name}}-{{instance}}d", "worker", 3), "worker-3d");
        assert_eq!(render("91{{instance}}", "worker", 7), "917");
    }

    #[test]
    fn a_value_with_no_token_is_returned_unchanged() {
        // The collision case the doubled braces exist for: single braces are
        // ordinary content and must survive untouched.
        for value in [
            r#"{"ts":"%t","level":"%l"}"#,
            r#"{"a":{"b":1}}"#,
            "^[a-z]{2,3}$",
            "plain",
        ] {
            assert_eq!(render(value, "worker", 1), value, "unchanged: {value}");
            assert!(validate(value).is_ok(), "and accepted: {value}");
        }
    }

    #[test]
    fn an_unknown_token_is_refused_by_name() {
        let err = validate("z-{{instnace}}").unwrap_err();
        assert!(matches!(&err, TemplateError::UnknownToken { token } if token == "instnace"));
        let rendered = err.to_string();
        assert!(rendered.contains("instnace"), "names the typo: {rendered}");
        assert!(rendered.contains("instance"), "and what is valid: {rendered}");
        assert!(
            !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
            "no em or en dash in copy a user reads: {rendered}"
        );
    }

    #[test]
    fn doubling_escapes_a_literal_token() {
        assert_eq!(render("{{{{instance}}}}", "worker", 3), "{{instance}}");
        assert!(validate("{{{{ .Values.port }}}}").is_ok());
        assert_eq!(
            render("{{{{ .Values.port }}}}", "worker", 3),
            "{{ .Values.port }}",
            "a Helm template passes through for the tool that consumes it"
        );
    }

    #[test]
    fn an_unclosed_token_is_refused() {
        assert!(validate("z-{{instance").is_err());
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep-core --lib --all-features template
```

Expected: FAIL to compile, no `render`/`validate`.

- [ ] **Step 3: Write the module**

Above the test module in the same file:

```rust
//! The `{{instance}}` grammar for Flockfile values.
//!
//! Two tokens, `{{instance}}` and `{{name}}`, in env values, args, and the
//! two log-path fields. Anything else between doubled braces is refused by
//! name at config time, so a typo dies at `shep start` rather than reaching
//! a child process as a literal string.
//!
//! # Why doubled braces
//!
//! Single braces are ordinary content in the values this runs over: JSON
//! blobs, regex quantifiers such as `{2,3}`, and Go or Helm templates passed
//! through as args. Under a single-brace grammar with an unknown token
//! refused, `LOG_FORMAT = '{"ts":"%t"}'` would stop a working Flockfile from
//! starting. Doubled braces almost never appear by accident.
//!
//! # Escaping
//!
//! `{{{{` is a literal `{{` and `}}}}` is a literal `}}`, which is
//! `format!`'s own doubling rule one level up. A lone `}}` is ordinary text,
//! deliberately: `{"a":{"b":1}}` ends in one and must survive.

use core::fmt;

use alloc::string::{String, ToString};

/// The tokens this grammar knows, in the order an error lists them.
const TOKENS: &[&str] = &["instance", "name"];

/// A value that is not a valid template.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// A `{{...}}` naming something this grammar does not define
    UnknownToken {
        /// The token as the user wrote it, without the braces
        token: String,
    },
    /// A `{{` with no closing `}}`
    Unclosed,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownToken { token } => write!(
                f,
                "`{{{{{token}}}}}` is not a template token: valid tokens are {}",
                TOKENS
                    .iter()
                    .map(|t| alloc::format!("`{{{{{t}}}}}`"))
                    .collect::<alloc::vec::Vec<_>>()
                    .join(" and ")
            ),
            Self::Unclosed => f.write_str("a `{{` in this value is never closed by a `}}`"),
        }
    }
}

impl core::error::Error for TemplateError {}

/// Walks `value`, calling `on_token` for each token and `on_literal` for each
/// run of ordinary text.
///
/// One walker so [`validate`] and [`render`] can never disagree about what a
/// token is.
fn walk(
    value: &str,
    mut on_literal: impl FnMut(&str),
    mut on_token: impl FnMut(&str) -> Result<(), TemplateError>,
) -> Result<(), TemplateError> {
    let bytes = value.as_bytes();
    let mut at = 0;
    let mut literal_from = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"{{{{") {
            on_literal(&value[literal_from..at]);
            on_literal("{{");
            at += 4;
            literal_from = at;
        } else if bytes[at..].starts_with(b"}}}}") {
            on_literal(&value[literal_from..at]);
            on_literal("}}");
            at += 4;
            literal_from = at;
        } else if bytes[at..].starts_with(b"{{") {
            on_literal(&value[literal_from..at]);
            let rest = &value[at + 2..];
            let Some(end) = rest.find("}}") else {
                return Err(TemplateError::Unclosed);
            };
            on_token(&rest[..end])?;
            at += 2 + end + 2;
            literal_from = at;
        } else {
            at += 1;
        }
    }
    on_literal(&value[literal_from..]);
    Ok(())
}

/// Checks that every `{{...}}` in `value` names a token this grammar defines.
///
/// # Errors
///
/// - [`TemplateError::UnknownToken`]: a token this grammar does not define.
/// - [`TemplateError::Unclosed`]: a `{{` with no closing `}}`.
pub fn validate(value: &str) -> Result<(), TemplateError> {
    walk(
        value,
        |_| {},
        |token| {
            if TOKENS.contains(&token) {
                Ok(())
            } else {
                Err(TemplateError::UnknownToken {
                    token: token.to_string(),
                })
            }
        },
    )
}

/// Substitutes the tokens in `value`.
///
/// Call [`validate`] first: an unknown token here renders as nothing, because
/// `normalize` is the seam that refuses one and a value reaching this
/// function has already passed it.
#[must_use]
pub fn render(value: &str, name: &str, instance: u32) -> String {
    let mut out = String::with_capacity(value.len());
    let slot = instance.to_string();
    let _ = walk(
        value,
        |literal| out.push_str(literal),
        |token| {
            match token {
                "instance" => out.push_str(&slot),
                "name" => out.push_str(name),
                _ => {}
            }
            Ok(())
        },
    );
    out
}
```

Adjust the `alloc::` paths to whatever the crate actually uses; if shep-core is `std`, use plain `String`, `format!` and `Vec`. Check the top of a neighbouring module in `crates/shep-core/src/config/` and match it.

Declare the module in `crates/shep-core/src/config/mod.rs` beside its siblings, and re-export what `normalize` and the daemon need.

- [ ] **Step 4: Run the grammar tests and watch them pass**

```bash
cargo test -p shep-core --lib --all-features template
```

Expected: PASS, all five.

- [ ] **Step 5: Write the failing tests for validation and rendering**

In `normalize.rs` `mod tests`:

```rust
#[test]
fn a_typo_in_an_env_template_is_refused_and_names_the_field() {
    let mut app = AppConfig::minimal("web", "./srv");
    app.env
        .insert("WORKER".to_string(), "w-{{instnace}}".to_string());
    let err = normalize(app).unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("instnace"), "names the typo: {rendered}");
    assert!(rendered.contains("WORKER"), "and the field: {rendered}");
}

#[test]
fn a_typo_in_an_arg_template_is_refused_too() {
    let mut app = AppConfig::minimal("web", "./srv");
    app.args = vec!["--port".to_string(), "91{{slot}}".to_string()];
    let err = normalize(app).unwrap_err();
    assert!(err.to_string().contains("slot"), "{err}");
}
```

In `assemble.rs` `mod tests`:

```rust
#[test]
fn templates_render_per_instance_in_env_and_args() {
    let mut config = AppConfig {
        name: "z-worker".to_string(),
        script: "bin/worker".to_string(),
        instances: 4,
        args: vec!["--metrics-port".to_string(), "91{{instance}}".to_string()],
        ..Default::default()
    };
    config
        .env
        .insert("Z_WORKER_ID".to_string(), "z-{{instance}}".to_string());
    config
        .env
        .insert("Z_DEVICE_ID".to_string(), "{{name}}-{{instance}}d".to_string());

    let app = normalize(config).unwrap();
    let spec = assemble(&app, 2, &test_paths(), None);

    assert_eq!(spec.env.get("Z_WORKER_ID").map(String::as_str), Some("z-2"));
    assert_eq!(
        spec.env.get("Z_DEVICE_ID").map(String::as_str),
        Some("z-worker-2d")
    );
    assert!(spec.args.contains(&"912".to_string()), "{:?}", spec.args);
}
```

- [ ] **Step 6: Run them and watch them fail**

```bash
cargo test -p shep-core --lib --all-features env_template
```

Expected: FAIL, nothing validates yet.

- [ ] **Step 7: Wire validation into normalize and rendering into assemble**

Add the `NormalizeError` variant:

```rust
    /// A value carries a `{{...}}` that is not a template token. Carries the
    /// sheep name, which field held it, and the rejection rendered.
    BadTemplate {
        /// The sheep name
        name: String,
        /// Which field, for example `env.WORKER` or `args[1]`
        field: String,
        /// The [`crate::config::template::TemplateError`], rendered, so this
        /// variant does not have to restate the grammar's own copy
        reason: String,
    },
```

Its Display arm:

```rust
            Self::BadTemplate { name, field, reason } => {
                write!(f, "sheep `{name}`, {field}: {reason}")
            }
```

In `normalize`, after the reserved-variable check, validate every env value and every arg, naming the field in the error. In `assemble`, render each env value and each arg through `template::render(value, &name, instance)` as it is inserted.

Note the ordering inside `assemble`: `base_env()` first, then the app's own env rendered on top, then the two reserved variables last, so a template can never shadow them.

- [ ] **Step 8: Run both suites and watch them pass**

```bash
cargo test -p shep-core --lib --all-features
```

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

Expected: PASS.

- [ ] **Step 9: Run the task gate, then commit**

```bash
git add crates/shep-core/src crates/shep-daemon/src/assemble.rs
git commit -m "feat(config): substitute {{instance}} and {{name}} in env and args"
```

---

### Task 7: Log paths are templated, and a silent merge is refused

Implements D8. Needs task 6.

**Files:**
- Modify `crates/shep-daemon/src/assemble.rs:213-230`
- Modify `crates/shep-core/src/config/normalize.rs`
- Test both

**Interfaces:**
- Consumes: `template::validate` and `template::render` from task 6.
- Produces: `NormalizeError::SharedLogPath { name: String, field: String }`.

- [ ] **Step 1: Write the failing tests**

In `normalize.rs` `mod tests`:

```rust
#[test]
fn an_explicit_log_path_shared_by_every_instance_is_refused() {
    let mut app = AppConfig::minimal("web", "./srv");
    app.instances = 3;
    app.out_file = Some("/var/log/web.log".to_string());
    let err = normalize(app).unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("out_file"), "names the field: {rendered}");
    assert!(
        rendered.contains("{{instance}}") && rendered.contains("merge_logs"),
        "and both ways out: {rendered}"
    );
    assert!(
        !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
        "no em or en dash in copy a user reads: {rendered}"
    );
}

#[test]
fn the_three_ways_out_of_the_shared_log_refusal_all_work() {
    // A slot in the path.
    let mut templated = AppConfig::minimal("web", "./srv");
    templated.instances = 3;
    templated.out_file = Some("/var/log/web-{{instance}}.log".to_string());
    assert!(normalize(templated).is_ok());

    // Asking for the merge on purpose.
    let mut merged = AppConfig::minimal("web", "./srv");
    merged.instances = 3;
    merged.out_file = Some("/var/log/web.log".to_string());
    merged.merge_logs = true;
    assert!(normalize(merged).is_ok());

    // One instance cannot collide with itself.
    let mut single = AppConfig::minimal("web", "./srv");
    single.out_file = Some("/var/log/web.log".to_string());
    assert!(normalize(single).is_ok());
}

#[test]
fn an_escaped_template_in_a_log_path_does_not_satisfy_the_refusal() {
    // `{{{{instance}}}}` spells the token but renders to one literal path
    // for every instance, so a substring check would wave it through.
    let mut app = AppConfig::minimal("web", "./srv");
    app.instances = 3;
    app.out_file = Some("/var/log/web-{{{{instance}}}}.log".to_string());
    assert!(normalize(app).is_err());
}
```

In `assemble.rs` `mod tests`:

```rust
#[test]
fn a_templated_log_path_renders_per_instance() {
    let app = normalize(AppConfig {
        name: "web".to_string(),
        script: "./srv".to_string(),
        instances: 3,
        out_file: Some("/var/log/web-{{instance}}.log".to_string()),
        ..Default::default()
    })
    .unwrap();
    let spec = assemble(&app, 2, &test_paths(), None);
    assert_eq!(spec.out_file, PathBuf::from("/var/log/web-2.log"));
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep-core --lib --all-features shared_log
```

Expected: FAIL, normalize accepts the shared path today.

- [ ] **Step 3: Add the refusal and the rendering**

The variant:

```rust
    /// An explicit log path has no `{{instance}}` in it, the app runs more
    /// than one instance, and `merge_logs` is off, so every instance would
    /// write to one file without having asked to.
    SharedLogPath {
        /// The sheep name
        name: String,
        /// `out_file` or `err_file`
        field: &'static str,
    },
```

Its Display arm:

```rust
            Self::SharedLogPath { name, field } => write!(
                f,
                "sheep `{name}` runs several instances and sets `{field}` to one path: put `{{{{instance}}}}` in it, or set `merge_logs = true` to share it on purpose"
            ),
```

The check, after the template validation from task 6 (so a malformed template is reported as malformed rather than as shared):

```rust
    if app.instances > 1 && !app.merge_logs {
        for (field, path) in [("out_file", &app.out_file), ("err_file", &app.err_file)] {
            // Rendered rather than searched for a substring: an escaped
            // `{{{{instance}}}}` contains the token's spelling but renders to
            // one literal path for every instance, which is exactly the
            // collision this refuses. Two slots that render alike collide.
            if let Some(path) = path
                && template::render(path, &app.name, 0) == template::render(path, &app.name, 1)
            {
                return Err(NormalizeError::SharedLogPath {
                    name: app.name.clone(),
                    field,
                });
            }
        }
    }
```

In `assemble.rs`, render the explicit paths:

```rust
    let out_file = if let Some(ref explicit) = config.out_file {
        PathBuf::from(template::render(explicit, &name, instance))
    } else {
        paths.logs.join(format!("{}out.log", log_stem))
    };
```

and the same for `err_file`. Validate both fields in task 6's validation loop as well, so a typo in a path is caught with the others.

- [ ] **Step 4: Run both suites and watch them pass**

```bash
cargo test -p shep-core --lib --all-features
```

```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```

Expected: PASS.

- [ ] **Step 5: Run the task gate, then commit**

```bash
git add crates/shep-core/src/config/normalize.rs crates/shep-daemon/src/assemble.rs
git commit -m "feat(config): template log paths, and refuse an unasked-for merge"
```

---

### Task 8: increment_var is removed

Implements D9. Needs task 6.

**Files:**
- Modify `crates/shep-core/src/config/app.rs:427-433` (field), `:491` (Default)
- Modify `crates/shep-core/src/config/normalize.rs`: the rejection
- Modify `crates/shep-daemon/src/assemble.rs`: drop the read and the old test
- Modify `crates/shep-cli/src/commands/import/convert.rs:198-206`, `render.rs:41-42,86-87`
- Modify `crates/shep-daemon/src/supervisor.rs:4255,6793`: doc prose only
- Test `normalize.rs`, `convert.rs`

**Interfaces:**
- Consumes: the template grammar from task 6.
- Produces: nothing other tasks rely on.

- [ ] **Step 1: Write the failing tests**

In `normalize.rs` `mod tests`:

```rust
#[test]
fn increment_var_is_refused_and_says_what_replaced_it() {
    let mut app = AppConfig::minimal("web", "./srv");
    app.increment_var = Some("WORKER_ID".to_string());
    let err = normalize(app).unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("increment_var"), "{rendered}");
    assert!(rendered.contains("WORKER_ID"), "keeps their name: {rendered}");
    assert!(rendered.contains("{{instance}}"), "and the fix: {rendered}");
    assert!(
        !rendered.contains('\u{2014}') && !rendered.contains('\u{2013}'),
        "no em or en dash in copy a user reads: {rendered}"
    );
}
```

In `crates/shep-cli/src/commands/import/convert.rs`, replace `the_pm2_instance_variable_becomes_increment_var_and_never_a_value`:

```rust
/// pm2's instance variable becomes an env entry holding the template, never
/// a value. Copying instance 0's number in would pin every worker to 0.
#[test]
fn the_pm2_instance_variable_becomes_an_env_template_and_never_a_value() {
    let imported = imported();
    let api = &imported.apps[0];
    assert_eq!(
        api.env.get("NODE_APP_INSTANCE").map(String::as_str),
        Some("{{instance}}")
    );
    assert!(imported.notes.contains(&ImportNote::InstanceVar {
        app: "api".to_string(),
        var: "NODE_APP_INSTANCE".to_string(),
    }));
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep-core --lib --all-features increment_var
```

Expected: FAIL, the field normalizes fine today.

- [ ] **Step 3: Turn the field into a rejection**

Keep the field in `AppConfig`, so `deny_unknown_fields` does not produce a serde error naming no fix, but mark it plainly:

```rust
    /// Removed. Set your own variable to `{{instance}}` in `env` instead.
    ///
    /// Kept only so `normalize` can reject it with that instruction: a
    /// `deny_unknown_fields` serde error would name no replacement. Remove
    /// in 0.2.
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub increment_var: Option<String>,
```

The `NormalizeError` variant:

```rust
    /// `increment_var` was removed in favour of `{{instance}}` templating.
    /// Carries the variable the app named, so the error can show the exact
    /// line to write instead.
    IncrementVarRemoved {
        /// The sheep name
        name: String,
        /// The variable the app asked for
        var: String,
    },
```

Its Display arm:

```rust
            Self::IncrementVarRemoved { name, var } => write!(
                f,
                "sheep `{name}` sets `increment_var`, which was removed: write `{var} = \"{{{{instance}}}}\"` under `[app.env]` instead"
            ),
```

The check, early in `normalize`, beside the other config-shape refusals.

- [ ] **Step 4: Drop the runtime read and update import**

In `assemble.rs`, delete the `slot_var` read (task 5 already replaced what it did) and delete `env_custom_increment_var`, whose behaviour no longer exists.

In `convert.rs`, write the env entry rather than the field:

```rust
    if let Some(var) = app_env.instance_var {
        // The template, not the value: pm2 reported instance 0's number, and
        // copying it in would tell every worker it is worker 0.
        app.env.insert(var.clone(), "{{instance}}".to_string());
        notes.push(ImportNote::InstanceVar {
            app: name.to_string(),
            var,
        });
    }
```

In `render.rs`, delete the `increment_var` field from `Rendered` and its line in `From<&AppConfig>`. The env map is already rendered, so the new entry emits with no further change.

Check `ImportNote::InstanceVar`'s own rendered text and update it if it names `increment_var`.

- [ ] **Step 5: Update the doc prose**

`crates/shep-daemon/src/supervisor.rs:4255` and `:6793` mention `increment_var` in comments. Rewrite both to name `SHEP_INSTANCE` and the template.

```bash
rg 'increment_var' crates/
```

Expected after this step: only the field, its doc, the rejection, and their tests.

- [ ] **Step 6: Run the workspace suite and watch it pass**

```bash
cargo test --workspace --all-features
```

Expected: PASS.

- [ ] **Step 7: Run the task gate, then commit**

```bash
git add crates/
git commit -m "feat(config)!: remove increment_var in favour of {{instance}}"
```

---

### Task 9: The flock table groups multi-instance apps

Implements D4. Needs task 2.

**Files:**
- Modify `crates/shep-cli/src/output/rows.rs:38-78` (`FlockRows`)
- Test same file

**Interfaces:**
- Consumes: `ProcessInfo.instance` from task 2.
- Produces: nothing other tasks call directly. Task 11 reuses the rollup rules by eye, not by import.

Background the implementer needs: `Render::rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>>` is the presentation-aware hook, defaulting to `rows()`. A row is a `Vec<String>`, so a group row is simply another row in the returned vector: no new mechanism is needed. `PRIORITIES` is index-parallel to `headers()`, where `0` means never drop. `RolledSheepRows` at `rows.rs:1541` is the existing precedent for a grouped renderer.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_single_instance_app_is_untouched_by_grouping() {
    let rows = FlockRows(vec![
        ProcessInfo::builder(4, "api", ProcStatus::Online)
            .instance(Some(0))
            .build(),
    ]);
    let rendered = rows.rows_for(full_presentation(), true);
    assert_eq!(rendered.len(), 1, "no group row for one instance");
    assert_eq!(rendered[0][1], "api", "and no suffix");
}

#[test]
fn a_multi_instance_app_gets_a_group_row_then_its_slots() {
    let rows = FlockRows(
        (0..3)
            .map(|slot| {
                ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                    .instance(Some(slot))
                    .build()
            })
            .collect(),
    );
    let rendered = rows.rows_for(full_presentation(), true);
    assert_eq!(rendered.len(), 4, "one group row plus three slots");
    assert_eq!(rendered[0][0], "", "the group row has no id");
    assert!(rendered[0][1].contains("web"), "{:?}", rendered[0]);
    assert!(rendered[0][1].contains('3'), "and the count: {:?}", rendered[0]);
    assert_eq!(rendered[1][0], "1", "slot rows keep their ids");
}

#[test]
fn a_mixed_group_says_so_rather_than_picking_a_winner() {
    let rows = FlockRows(vec![
        ProcessInfo::builder(1, "web", ProcStatus::Online)
            .instance(Some(0))
            .build(),
        ProcessInfo::builder(2, "web", ProcStatus::Stopped)
            .instance(Some(1))
            .build(),
    ]);
    let rendered = rows.rows_for(full_presentation(), true);
    let status = &rendered[0][2];
    assert!(status.contains('1'), "counts each state: {status}");
}

#[test]
fn a_flat_style_suffixes_the_name_instead_of_grouping() {
    let rows = FlockRows(
        (0..2)
            .map(|slot| {
                ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                    .instance(Some(slot))
                    .build()
            })
            .collect(),
    );
    let rendered = rows.rows_for(bare_presentation(), true);
    assert_eq!(rendered.len(), 2, "one line per process, still greppable");
    assert_eq!(rendered[0][1], "web:0");
    assert_eq!(rendered[1][1], "web:1");
}

#[test]
fn a_row_from_an_older_daemon_renders_exactly_as_it_did_before() {
    let rows = FlockRows(vec![
        ProcessInfo::builder(1, "web", ProcStatus::Online).build(),
        ProcessInfo::builder(2, "web", ProcStatus::Online).build(),
    ]);
    let rendered = rows.rows_for(full_presentation(), true);
    assert_eq!(rendered.len(), 2, "no slots, so no grouping");
    assert_eq!(rendered[0][1], "web", "and no suffix");
}
```

Write `full_presentation()` and `bare_presentation()` as small test helpers building a `Presentation` at `StyleLevel::Full` and `StyleLevel::Bare`. Check `crates/shep-cli/src/style.rs:162-171` for the field set and follow whatever the neighbouring row tests already do.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features group_row
```

Expected: FAIL, `rows_for` is not overridden so every app renders flat.

- [ ] **Step 3: Implement `rows_for` on FlockRows**

Leave `rows()` exactly as it is: it is what `Format::Json` serializes through, and D4 keeps JSON flat. Everything here is the `rows_for` override.

```rust
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        // The listing arrives sorted by (name, instance, id), so an app's
        // instances are already adjacent and one pass can group them.
        let mut out = Vec::with_capacity(self.0.len());
        let mut at = 0;
        while at < self.0.len() {
            let name = self.0[at].name.as_str();
            let end = self.0[at..]
                .iter()
                .position(|p| p.name != name)
                .map_or(self.0.len(), |offset| at + offset);
            let group = &self.0[at..end];
            // A slot nobody reported cannot be grouped or suffixed: an older
            // shepherd's listing renders exactly as it did before the field.
            let slotted = group.len() > 1 && group.iter().all(|p| p.instance.is_some());
            if slotted && presentation.level.boxes() {
                out.push(group_row(group, presentation, status_word));
                out.extend(group.iter().map(|p| slot_row(p, presentation, status_word)));
            } else {
                out.extend(
                    group
                        .iter()
                        .map(|p| plain_row(p, slotted, presentation, status_word)),
                );
            }
            at = end;
        }
        out
    }
```

The three row builders, beside `FlockRows`:

```rust
/// The header above an app's instances: what the app costs, and how many of
/// it there are. Per-app facts live here rather than being repeated down
/// every slot row, which is what `fold` and `smit` already are.
fn group_row(
    group: &[ProcessInfo],
    presentation: Presentation,
    status_word: bool,
) -> Vec<String> {
    let first = &group[0];
    let restarts: u32 = group.iter().map(|p| p.restarts).sum();
    let cpu: Option<f32> = group
        .iter()
        .filter_map(|p| p.cpu_percent)
        .fold(None, |acc, c| Some(acc.unwrap_or(0.0) + c));
    let memory: Option<u64> = group
        .iter()
        .filter_map(|p| p.memory_bytes)
        .fold(None, |acc, m| Some(acc.unwrap_or(0) + m));
    // The minimum, so this reads as time since the app was last disturbed
    // rather than as the age of its luckiest instance.
    let uptime = group.iter().map(|p| p.uptime_ms).min().unwrap_or(0);
    vec![
        String::new(),
        format!("{} \u{d7}{}", first.name, group.len()),
        group_status(group, presentation, status_word),
        String::new(),
        restarts.to_string(),
        String::new(),
        cpu.map_or_else(|| "-".to_string(), |c| format!("{c:.1}%")),
        memory.map_or_else(|| "-".to_string(), super::human_bytes),
        super::human_duration(uptime),
        first.fold.clone().unwrap_or_else(|| "-".to_string()),
        first.smit.clone().unwrap_or_else(|| "-".to_owned()),
    ]
}

/// One instance under its group header. `\u{21b3} :2` teaches the `web:2`
/// selector by sitting under the name the header already printed.
fn slot_row(p: &ProcessInfo, presentation: Presentation, status_word: bool) -> Vec<String> {
    let slot = p.instance.map_or_else(String::new, |s| format!(" \u{21b3} :{s}"));
    vec![
        p.id.to_string(),
        slot,
        status_cell(p.status, presentation, status_word),
        p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        p.restarts.to_string(),
        exit_cell(p.pid, p.last_exit),
        p.cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        p.memory_bytes
            .map_or_else(|| "-".to_string(), super::human_bytes),
        super::human_duration(p.uptime_ms),
        // Blank, not "-": the group row above carries both, and repeating a
        // per-app fact down every slot row is noise.
        String::new(),
        String::new(),
    ]
}

/// One line per process, for the flat styles and for any listing that cannot
/// be grouped. `slotted` is true when this app has more than one instance and
/// every row reported its slot, which is the only case that earns a suffix.
fn plain_row(
    p: &ProcessInfo,
    slotted: bool,
    presentation: Presentation,
    status_word: bool,
) -> Vec<String> {
    let name = match (slotted, p.instance) {
        (true, Some(slot)) => format!("{}:{}", p.name, slot),
        _ => p.name.clone(),
    };
    vec![
        p.id.to_string(),
        name,
        status_cell(p.status, presentation, status_word),
        p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        p.restarts.to_string(),
        exit_cell(p.pid, p.last_exit),
        p.cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        p.memory_bytes
            .map_or_else(|| "-".to_string(), super::human_bytes),
        super::human_duration(p.uptime_ms),
        p.fold.clone().unwrap_or_else(|| "-".to_string()),
        p.smit.clone().unwrap_or_else(|| "-".to_owned()),
    ]
}

/// The group's status: the shared one when every instance agrees, else a
/// count per state, so a mixed group says what it is rather than picking a
/// winner an operator would then act on.
fn group_status(
    group: &[ProcessInfo],
    presentation: Presentation,
    status_word: bool,
) -> String {
    let first = group[0].status;
    if group.iter().all(|p| p.status == first) {
        return status_cell(first, presentation, status_word);
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in group {
        *counts.entry(p.status.to_string()).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(status, n)| format!("{n} {status}"))
        .collect::<Vec<_>>()
        .join(", ")
}
```

`presentation.level.boxes()` is the existing full-versus-flat test (`crates/shep-cli/src/style.rs`); confirm its name before use and swap in `sheep()` if `boxes()` turns out to gate something else. Add `use std::collections::BTreeMap;` if the module lacks it.

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test -p shep --lib --all-features group_row
```

Expected: PASS, all five.

- [ ] **Step 5: Check the column-drop interaction**

The name column is wider now. Confirm `PRIORITIES` still keeps NAME at `0` (never dropped), and run the table tests:

```bash
cargo test -p shep --lib --all-features -- --skip ::slow:: output::
```

Expected: PASS.

- [ ] **Step 6: Run the task gate, then commit**

```bash
git add crates/shep-cli/src/output/rows.rs
git commit -m "feat(flock): group an app's instances under one row"
```

---

### Task 10: Bleats labels a line with its slot

Implements D11. Needs tasks 1 and 2.

**Files:**
- Modify `crates/shep-cli/src/commands/bleats.rs:164-186` (`write_line`), `:312-395` (`tail_log_files`), the follow arm at `:410-425`
- Test same file

**Interfaces:**
- Consumes: the dedup from task 1; `ProcessInfo.instance` from task 2.
- Produces: nothing.

- [ ] **Step 1: Write the failing tests**

First a helper beside task 1's test, since all three tests build the same shape:

```rust
/// Builds a cache of `count` rows for one app, and returns it with the
/// printed backlog. `shared` puts every instance on one file, the way
/// `merge_logs` does.
fn backlog_of(dir: &Path, app: &str, count: u32, shared: bool) -> String {
    let mut cache = HashMap::new();
    for slot in 0..count {
        let stem = if shared {
            format!("{app}-out.log")
        } else {
            format!("{app}-{slot}-out.log")
        };
        let path = dir.join(&stem);
        std::fs::write(&path, format!("hello from {slot}\n")).expect("write");
        cache.insert(
            slot,
            ProcessInfo::builder(slot, app, ProcStatus::Online)
                .instance(Some(slot))
                .out_file(Some(path.to_string_lossy().to_string()))
                .build(),
        );
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = Streams::test(&mut out, &mut err);
    let args = bleats_args(app, true, false, true);
    let selector = ProcessSelector::parse(app).expect("selector");
    tail_log_files(&mut streams, false, &cache, &selector, &args);
    String::from_utf8(out).expect("utf8")
}
```

```rust
#[test]
fn a_multi_instance_app_labels_its_backlog_lines_with_the_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let printed = backlog_of(dir.path(), "web", 2, false);
    assert!(printed.contains("web:0 |"), "{printed}");
    assert!(printed.contains("web:1 |"), "{printed}");
}

#[test]
fn a_single_instance_app_keeps_the_bare_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let printed = backlog_of(dir.path(), "solo", 1, false);
    assert!(printed.contains("solo |"), "{printed}");
    assert!(
        !printed.contains("solo:0"),
        "no suffix for one instance: {printed}"
    );
}

#[test]
fn a_shared_backlog_file_is_labelled_with_the_app_not_a_slot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let printed = backlog_of(dir.path(), "talker", 2, true);
    assert!(printed.contains("talker |"), "{printed}");
    assert!(
        !printed.contains("talker:"),
        "one file holds both instances, and no line says which wrote it: {printed}"
    );
}
```

Use whatever temp-directory and `Streams` test constructors task 1 settled on rather than these names if they differ.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features labels
```

Expected: FAIL, every line prints the bare name.

- [ ] **Step 3: Thread the slot through**

`write_line` takes the slot rather than deriving it:

```rust
fn write_line(
    out: &mut dyn io::Write,
    fmt: Format,
    id: u32,
    name: &str,
    instance: Option<u32>,
    stream: &'static str,
    line: &str,
) -> io::Result<()> {
    match fmt {
        Format::Json => {
            let payload = BleatLine {
                schema_version: output::SCHEMA_VERSION,
                id,
                name,
                instance,
                stream,
                line,
            };
            serde_json::to_writer(&mut *out, &payload)?;
            writeln!(out)
        }
        Format::Table => match instance {
            Some(slot) => writeln!(out, "{name}:{slot} | {line}"),
            None => writeln!(out, "{name} | {line}"),
        },
    }
}
```

Add `instance: Option<u32>` to `BleatLine`.

Callers decide what to pass, and the two halves differ on purpose:

- **Backlog.** Pass `Some(slot)` only when the app has more than one instance registered in the cache AND this path belongs to one row. When the dedup from task 1 collapsed several rows onto one path, pass `None`: the file holds every instance's output and no line says who wrote it.
- **Follow.** The bus event carries the sheep id, so look the row up in the cache and pass its slot whenever the app has more than one instance, shared file or not.

Count instances per name over the whole cache, not over the matched set, so a selector cannot change how a line is labelled.

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test -p shep --lib --all-features labels
```

Expected: PASS, all three.

- [ ] **Step 5: Run the module's suite, the gate, then commit**

```bash
cargo test -p shep --lib --bins --all-features -- --skip ::slow:: bleats
```

```bash
git add crates/shep-cli/src/commands/bleats.rs
git commit -m "feat(bleats): label a line with its instance slot"
```

---

### Task 11: Lookout grows two row kinds

Implements D5. Needs tasks 2 and 9.

**Files:**
- Modify `crates/shep-cli/src/lookout/app.rs`: `visible_ids`, `reseat`, `selected`, `arm`, `confirm`, `Sent::request`
- Modify `crates/shep-cli/src/lookout/view/flock.rs`, `view/mod.rs`, `view/status.rs`, `view/detail.rs`
- Test `app.rs` `mod tests`

**Interfaces:**
- Consumes: `ProcessInfo.instance` from task 2; the rollup rules from task 9.
- Produces: nothing.

Background the implementer needs: selection is `selected: Option<u32>`, an id, and `visible_ids()` returns ids sorted by `(name, id)`. `Action { verb, id, name, at, stage }` is id-keyed and `Sent::request()` builds `SelectorSpec::Id(id)`. The confirm prompt is built in `view/status.rs:73-85`. The selected row is shown by a gutter marker (`flock::mark`), not a row style.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_multi_instance_app_shows_a_group_row_above_its_slots() {
    let mut app = allowed_with_instances();
    assert_eq!(
        app.visible_rows().len(),
        4,
        "three slots and the group row above them"
    );
    assert!(matches!(app.visible_rows()[0], RowKey::Group(ref n) if n == "web"));
}

#[test]
fn an_action_on_a_group_row_targets_the_whole_app_by_name() {
    let mut app = allowed_with_instances();
    app.select(RowKey::Group("web".to_string()));
    app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
    let sent = app.confirm_and_take_sent();
    assert_eq!(
        sent.request(),
        Request::Stop {
            selector: SelectorSpec::Name("web".to_string())
        }
    );
}

#[test]
fn a_group_confirm_states_how_many_processes_it_reaches() {
    let mut app = allowed_with_instances();
    app.select(RowKey::Group("web".to_string()));
    app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
    let prompt = status_line_text(&app);
    assert!(prompt.contains('3'), "names the blast radius: {prompt}");
}

#[test]
fn selection_survives_a_poll_on_both_row_kinds() {
    let mut app = allowed_with_instances();
    app.select(RowKey::Group("web".to_string()));
    app.update(Msg::Snapshot { rows: snapshot_rows(), at: Instant::now() });
    assert_eq!(app.selected(), Some(RowKey::Group("web".to_string())));
}
```

Write `allowed_with_instances()` on the model of the existing `allowed()` helper at `app.rs:1463-1519`, with three `web` rows carrying slots 0, 1 and 2. `confirm_and_take_sent` and `status_line_text` are small test helpers; if the existing tests already reach the `Effect::Send` payload some other way, follow that instead of adding helpers.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features lookout
```

Expected: FAIL to compile, no `RowKey`.

- [ ] **Step 3: Introduce the row key**

```rust
/// What the cursor can sit on: one sheep, or the header above an app's
/// instances.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowKey {
    /// One app's group header, carrying its name
    Group(String),
    /// One sheep, by id
    Sheep(u32),
}
```

`selected` becomes `Option<RowKey>`. `visible_ids` becomes `visible_rows() -> Vec<RowKey>`, emitting a `Group` before an app's slots when it has more than one row and every row carries a slot, and `Sheep` rows otherwise. Keep the existing filter behaviour: it is applied inside this function today and stays there. `reseat` works on the returned vector, unchanged in shape.

`Action` carries a `RowKey` rather than a bare id, and `Sent::request()` maps `Group(name)` to `SelectorSpec::Name(name)` and `Sheep(id)` to `SelectorSpec::Id(id)`.

- [ ] **Step 4: Update the confirm prompt**

In `view/status.rs`, a group action states the count:

```rust
    // A group row is the one place a keypress reaches several processes, so
    // the prompt says how many before the operator commits.
    format!(
        "{} all {count} instances of {name}? enter confirms, any other key cancels",
        action.verb.label()
    )
```

Leave the single-sheep prompt exactly as it is, including its `(id {})`.

- [ ] **Step 5: Update the view**

`view/mod.rs`'s render loop iterates rows rather than `Row` values; `flock::row_line` gains a group arm rendering the same cells task 9 defined; `detail.rs` shows the app-level summary for a group row and its existing fields for a sheep. The lamb fetch in `mod.rs:351-367` is keyed on `app.selected()` being a sheep: a group row fetches nothing.

- [ ] **Step 6: Run them and watch them pass**

```bash
cargo test -p shep --lib --all-features lookout
```

Expected: PASS, all four.

- [ ] **Step 7: Run the task gate, then commit**

```bash
git add crates/shep-cli/src/lookout
git commit -m "feat(lookout): give an app's instances a selectable group row"
```

---

### Task 12: Docs, schema and the CLI reference

Closes the docs trigger. Needs every task above.

**Files:**
- Modify `crates/shep-core/assets/flockfile.schema.json` (generated)
- Modify `web/src/data/cli-reference.generated.txt` (generated)
- Modify `web/src/pages/docs/from-pm2.astro`, `first-flockfile.astro`, `output.astro`, `lookout.astro`, `json-output.astro`, `getting-started.astro`, `examples.astro`, `folds.astro`, `cli.astro`
- Modify `docs/migration.md`, `docs/terminology.md`
- Modify `CLAUDE.md`: the instances paragraph

- [ ] **Step 1: Regenerate the schema**

```bash
cargo run --bin shep -- schema > crates/shep-core/assets/flockfile.schema.json
```

A drift test in `crates/shep-core/src/config/flockfile.rs` compares the committed asset against a fresh generation and fails loudly when they differ, so run the core suite afterwards.

- [ ] **Step 2: Regenerate the CLI reference**

```bash
cargo build --release
```

```bash
./web/scripts/generate-cli-reference.sh
```

`git diff` afterwards is the check. A stale copy fails no build, which is exactly why it drifts.

- [ ] **Step 3: Read the prose pages**

```bash
rg -l 'increment_var|instances|bleats' web/src/pages/docs/
```

Every hit is hand-written and no generator touches it. In particular: `from-pm2.astro` and `first-flockfile.astro` document `increment_var` and must now show the `{{instance}}` form; the pages showing bleats output must show the new prefix; `output.astro` must show the grouped table.

- [ ] **Step 4: Add the terminology entry**

`docs/terminology.md` gains an `instance` row saying what it is and what it is not, since lamb is the neighbouring word: an instance is a sibling copy of a sheep, a lamb is a child process of one.

- [ ] **Step 5: Build and typecheck the site**

```bash
cd web && npx astro build
```

```bash
cd web && npx astro check
```

Both. `check` is the one that catches a wrong prop: Astro does not typecheck during a build, so a page passing a component a prop it does not have builds clean and renders wrong.

- [ ] **Step 6: Update CLAUDE.md**

The status section describes `increment_var` and the instances behaviour. Rewrite that paragraph to describe the templating and the grouped display, and keep the verb-count paragraph accurate if any verb count moved (it should not have; this plan adds no verb).

- [ ] **Step 7: Run the full gate, then commit**

```bash
cargo test --workspace --all-features
```

```bash
git add crates web docs CLAUDE.md
git commit -m "docs: describe instance templating and the grouped flock table"
```

---

## Cross-cutting checks before the branch is done

- [ ] **The two cross-checks**, once, at the end rather than per task:

```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```

```bash
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The Windows one needs `brew install mingw-w64` because `ring`'s build script runs `cc`.

- [ ] **The serial run**, which was red on main before and caught a real regression in Phase 6:

```bash
cargo test --workspace --all-features -- --test-threads=1
```

- [ ] **Read the CI result before calling the branch green.** The local gate does not run Linux or Windows tests: a macOS `cargo test` never compiles a `cfg(windows)` item, and the windows-gnu cross-check is `cargo check`, which executes nothing.

- [ ] **End-to-end on a real flock.** The defects this plan fixes were found by running a real two-instance node app, not by reading code. Start one with `merge_logs`, one without, confirm `shep flock` groups them, `shep restart web:1` reaches one process, `shep bleats web` prints each line once with slot labels, and `shep lookout` selects a group row and confirms with a count.
