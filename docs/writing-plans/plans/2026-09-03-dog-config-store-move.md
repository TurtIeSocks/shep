# Dog config store move Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move a dog's `[dog.<name>]` settings out of `shep.toml` into a hand-editable `$SHEP_HOME/dogs.toml`, migrating any existing sections on first boot, without the dog-facing wire changing at all.

**Architecture:** A new `DogsConfig` in shep-core parses the new file and is the only place its shape is decided, mirroring what `DaemonConfig` does for `shep.toml`. `ShepToml` grows one mutator that removes and returns the `[dog.*]` sections. The daemon's own entry point runs the migration once per boot before the supervisor starts, and `dog_section` reads the new file while still answering with byte-identical TOML.

**Tech Stack:** Rust 2024, MSRV 1.88. `toml` for parsing, `toml_edit` for the format-preserving rewrite, `serde` for the derive. No new dependencies.

**Spec:** `docs/brainstorming/specs/2026-09-03-dog-config-design.md` (decisions 1, 2 and 3 only; decisions 4 through 9 are releases 2 and 3 and are out of scope for this plan)

## Global Constraints

- **Clean-room rule, non-negotiable:** never open, read, or port source from the pm2 checkout. Work from this plan and the spec.
- **Invoke the `shep-idiomatic-rust` skill before writing or reviewing any Rust.** Cite rules as `IR-<n>` in review.
- **No em dashes or en dashes anywhere**, including code comments, doc comments, commit messages and PR bodies. Use a comma, colon, period or parentheses.
- **Never write the maintainer's real name, personal email, or any absolute home-directory path** into a committed file or a commit message. Repo-relative paths only.
- **Every new public item needs a doc comment and a deliberate `Debug` decision (IR-41).** Anything carrying dog config values is redacted, with an exact-string test, because `[dog.bark.sinks]` holds webhook credentials.
- **`#![forbid(unsafe_code)]` is live in shep-core and shep-cli.** Nothing here needs unsafe.
- **Every test's `await` needs a forcing mechanism, not a sleep (IR-46).**
- **Prove every new test non-vacuous:** mutate what it protects, watch that specific test go red, restore. Say in the report which mutation you used.
- **ONE cargo shape for every task in this plan:** `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`. This change crosses three crates, so `-p` would need three invocations and each switch invalidates the others' features. Do not "also run the targeted crate tests to catch failures early": that is the churn this rule exists to prevent.
- **Task gate, once, when the whole plan is done:** `cargo fmt --all --check`, then `cargo clippy --workspace --all-targets --all-features -- -D warnings`, then `cargo test --workspace --all-features`, then `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`. Each from its own command with `$?` captured directly, never through a pipe: in zsh a pipeline's `$?` belongs to the last command.
- **Worktree:** `.claude/worktrees/dog-config`, branch `feat/dog-config`. Run everything from there.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/shep-core/src/config/dogs.rs` (new) | `DogsConfig`, `DogsConfigError`. The only place `dogs.toml`'s shape is decided. Redacted `Debug`. |
| `crates/shep-core/src/config/mod.rs` | Export the new module. |
| `crates/shep-core/src/paths.rs` | `ShepPaths::dogs_config`, `home.join("dogs.toml")`. |
| `crates/shep-cli/src/commands/shep_toml.rs` | `ShepToml::take_dog_sections`, which removes `[dog.*]` and hands the tables back. |
| `crates/shep-cli/src/commands/dog_migration.rs` (new) | The boot migration, and only that. Its own file because it is the piece most likely to need reading later, and `daemon.rs` is a command runner rather than a home for one-time upgrades. |
| `crates/shep-cli/src/commands/daemon.rs` | Call the migration from `run_daemon` before the supervisor boots. |
| `crates/shep-daemon/src/dogs.rs` | `dog_section` reads `DogsConfig` instead of `DaemonConfig`. |
| `crates/shep-daemon/src/rpc.rs` | The context field it reads from. |
| `crates/shep-daemon/src/boot.rs` | Populate that field from `paths.dogs_config`. |
| `docs/dogs.md`, `web/`, `CLAUDE.md` | The operator-facing account. |

`DaemonConfig::dog` and `RawDaemonConfig::dog` are deliberately **not** removed. Deleting either would stop every existing `shep.toml` with a `[dog.bark]` section from parsing, and `deny_unknown_fields` would turn that into a refused boot with the flock left unsupervised. Removing them is a separate breaking change with its own deprecation window, named in the spec's Out of scope.

---

### Task 1: `DogsConfig` and the path

**Files:**
- Create: `crates/shep-core/src/config/dogs.rs`
- Modify: `crates/shep-core/src/config/mod.rs`
- Modify: `crates/shep-core/src/paths.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `shep_core::config::dogs::{DogsConfig, DogsConfigError}`. `DogsConfig` has one public field, `pub dog: BTreeMap<String, toml::Table>`, keyed by dog name with no prefix. `DogsConfig::load(source: Option<&str>) -> Result<Self, DogsConfigError>`. `ShepPaths::dogs_config: PathBuf`.

This task is inert: nothing reads either yet. That is deliberate, so the type and the path can be reviewed on their own.

- [ ] **Step 1: Write the failing tests**

Add to a `#[cfg(test)] mod tests` at the bottom of `crates/shep-core/src/config/dogs.rs`:

```rust
#[test]
fn a_missing_file_loads_as_an_empty_map() {
    let config = DogsConfig::load(None).expect("None is not an error");
    assert!(config.dog.is_empty());
}

#[test]
fn sections_are_keyed_by_name_with_no_prefix() {
    let source = "[metrics]\nbind = \"127.0.0.1:9615\"\n\n[bark.sinks]\noncall = { kind = \"discord\" }\n";
    let config = DogsConfig::load(Some(source)).expect("valid TOML");
    assert_eq!(config.dog.keys().collect::<Vec<_>>(), vec!["bark", "metrics"]);
    assert_eq!(
        config.dog["metrics"]["bind"].as_str(),
        Some("127.0.0.1:9615")
    );
}

#[test]
fn invalid_toml_is_a_named_error() {
    let err = DogsConfig::load(Some("[metrics")).expect_err("unterminated table header");
    assert!(matches!(err, DogsConfigError::Toml(_)));
}

// IR-41: this map routinely holds webhook URLs with a bearer token in the
// path. `Debug` is the one thing between such a token and any future
// `tracing::debug!("{config:?}")`, so the exact string is pinned rather
// than the shape.
#[test]
fn debug_redacts_every_dog_section() {
    let source = "[bark.sinks]\noncall = { url = \"https://discord.com/api/webhooks/SECRET\" }\n";
    let config = DogsConfig::load(Some(source)).expect("valid TOML");
    assert_eq!(format!("{config:?}"), "DogsConfig { dog: <1 tables> }");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: FAIL, `unresolved module or unlinked crate 'dogs'` or `cannot find type DogsConfig`.

- [ ] **Step 3: Write the module**

`crates/shep-core/src/config/dogs.rs`:

```rust
//! `$SHEP_HOME/dogs.toml`, a dog's own settings.
//!
//! One table per dog, keyed by the name the dog was registered under, with
//! no prefix: `[metrics]` here is what `[dog.metrics]` was in `shep.toml`
//! before the move. The daemon serves a section verbatim over the socket as
//! `Response::DogSection` and never interprets it, so this type parses
//! exactly far enough to find the right table and no further.
//!
//! Hand-editable on purpose, and deliberately not a locked shep-owned store
//! like `overrides.json`. A dog's config is authored intent rather than
//! derived state, and an operator on a box with only a shell has to be able
//! to set one without a dashboard.

use core::fmt;
use std::collections::BTreeMap;

/// Every `[<dog>]` table in `dogs.toml`
///
/// `Debug` is redacted (IR-41): a dog section routinely carries a webhook
/// URL with a bearer token in it, and this type exists to be logged near
/// the boot path.
#[derive(Clone, Default, PartialEq)]
pub struct DogsConfig {
    /// Raw `[<name>]` tables keyed by dog name
    pub dog: BTreeMap<String, toml::Table>,
}

impl fmt::Debug for DogsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DogsConfig")
            .field("dog", &format_args!("<{} tables>", self.dog.len()))
            .finish()
    }
}

impl DogsConfig {
    /// Parses `dogs.toml`, or answers empty when there is no file
    ///
    /// # Errors
    ///
    /// - [`DogsConfigError::Toml`] when `source` is not valid TOML.
    pub fn load(source: Option<&str>) -> Result<Self, DogsConfigError> {
        let Some(source) = source else {
            return Ok(Self::default());
        };
        let dog = toml::from_str(source).map_err(DogsConfigError::Toml)?;
        Ok(Self { dog })
    }
}

/// Why `dogs.toml` could not be read
#[derive(Debug)]
#[non_exhaustive]
pub enum DogsConfigError {
    /// The file is not valid TOML
    Toml(toml::de::Error),
}

impl fmt::Display for DogsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(err) => write!(f, "invalid TOML in dogs.toml: {err}"),
        }
    }
}

impl core::error::Error for DogsConfigError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Toml(err) => Some(err),
        }
    }
}
```

`std::collections::BTreeMap` matches `crates/shep-core/src/config/daemon.rs:8`, which is the neighbouring module. Checked, not guessed.

`#[non_exhaustive]` on the error carries a reason comment per IR-20: add `// One variant today. `#[non_exhaustive]` so a second reading failure (a permissions error, once this type learns to open the file itself) is additive rather than breaking.` directly above the attribute.

- [ ] **Step 4: Export it**

In `crates/shep-core/src/config/mod.rs`, add `pub mod dogs;` between `pub mod daemon;` (line 7) and `pub mod flockfile;` (line 8), and `pub use dogs::{DogsConfig, DogsConfigError};` immediately after `pub use daemon::{...}` on line 19. Every neighbouring module is re-exported that way; checked.

- [ ] **Step 5: Add the path**

In `crates/shep-core/src/paths.rs`, add the field to `ShepPaths` next to `daemon_config`:

```rust
    /// A dog's own settings: `dogs.toml`
    ///
    /// Separate from [`Self::daemon_config`] rather than a section inside
    /// it, so lookout can write a dog's config without writing into the
    /// daemon's own hand-authored file.
    pub dogs_config: PathBuf,
```

and in the constructor, beside `daemon_config: home.join("shep.toml"),`:

```rust
            dogs_config: home.join("dogs.toml"),
```

Then extend the existing path assertion test in that file (the one asserting `p.daemon_config`) with the matching `dogs_config` line, following whatever home root that test already uses.

- [ ] **Step 6: Run the tests**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS, including the four new tests and the extended path test.

- [ ] **Step 7: Prove they are not vacuous**

Change `<{} tables>` to `{:?}` in the `Debug` impl and confirm `debug_redacts_every_dog_section` fails with the webhook URL in the output. Restore. Report the mutation.

- [ ] **Step 8: Commit**

```bash
git add crates/shep-core/src/config/dogs.rs crates/shep-core/src/config/mod.rs crates/shep-core/src/paths.rs
git commit -m "feat(core): dogs.toml gets a type and a path"
```

---

### Task 2: `ShepToml::take_dog_sections`

**Files:**
- Modify: `crates/shep-cli/src/commands/shep_toml.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `ShepToml::take_dog_sections(&mut self) -> BTreeMap<String, toml::Table>`. Removes the whole `[dog]` table from the document and returns what was under it, keyed by dog name. Returns an empty map when there was no `[dog]` table, and in that case leaves the document byte-identical.

Still inert: nothing calls it until Task 3. Keeping it its own task means a reviewer can reject the rewrite semantics without rejecting the migration around them.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `crates/shep-cli/src/commands/shep_toml.rs`, following the fixture style already used there for `enable_dog` and `adopt_dog`:

```rust
#[test]
fn taking_dog_sections_returns_them_keyed_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shep.toml");
    std::fs::write(
        &path,
        "[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\nbind = \"127.0.0.1:9615\"\n\n[dog.bark.sinks]\noncall = { kind = \"discord\" }\n",
    )
    .expect("write");

    let taken = ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

    assert_eq!(taken.keys().collect::<Vec<_>>(), vec!["bark", "metrics"]);
    assert_eq!(taken["metrics"]["bind"].as_str(), Some("127.0.0.1:9615"));
}

#[test]
fn taking_dog_sections_leaves_every_other_section_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shep.toml");
    std::fs::write(
        &path,
        "# keep me\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\nbind = \"127.0.0.1:9615\"\n\n[style]\nlevel = \"full\"\n",
    )
    .expect("write");

    ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

    // Exact string: the whole reason this goes through `toml_edit` rather
    // than a `toml::Table` round-trip is that a comment or a reordered key
    // would be a reason not to run the upgrade.
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "# keep me\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[style]\nlevel = \"full\"\n"
    );
}

#[test]
fn taking_from_a_file_with_no_dog_sections_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shep.toml");
    let before = "[daemon]\nlog_level = \"info\"\n";
    std::fs::write(&path, before).expect("write");

    let taken = ShepToml::edit(&path, ShepToml::take_dog_sections).expect("edit");

    assert!(taken.is_empty());
    // Content identity, not proof that nothing was written: `edit` always
    // stages and renames, so the file has a new inode either way. Not
    // writing at all is the migration's job, and its own early return is
    // where that is tested.
    assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: FAIL, `no function or associated item named 'take_dog_sections'`.

- [ ] **Step 3: Write the method**

Add to `impl ShepToml`, next to the other dog mutators:

```rust
    /// Removes the whole `[dog]` table and hands back what was under it
    ///
    /// Keyed by dog name with the `dog.` prefix dropped, which is the shape
    /// `dogs.toml` wants. A document with no `[dog]` table yields an empty
    /// map and is left byte-identical, so a second call after a migration
    /// writes nothing.
    ///
    /// The one caller is the boot migration. This is not a general editing
    /// primitive: it takes everything, because a partial move would leave
    /// the same key readable from two files.
    pub fn take_dog_sections(&mut self) -> BTreeMap<String, toml::Table> {
        let Some(item) = self.doc.remove("dog") else {
            return BTreeMap::new();
        };
        let Some(table) = item.as_table() else {
            return BTreeMap::new();
        };
        table
            .iter()
            .filter_map(|(name, value)| {
                let section = value.as_table()?;
                let parsed = section.to_string().parse::<toml::Table>().ok()?;
                Some((name.to_string(), parsed))
            })
            .collect()
    }
```

If `self.doc` is not the field name on `ShepToml`, grep the struct and use whatever it is. `disable_dog` reads `self.doc.get_mut("daemon")`, so `doc` is the expected name.

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS.

- [ ] **Step 5: Prove they are not vacuous**

Change `self.doc.remove("dog")` to `self.doc.get("dog").cloned()` so the section is read but never struck, and confirm `taking_dog_sections_leaves_every_other_section_alone` fails on the exact string with `[dog.metrics]` still present. Restore. Report the mutation.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/commands/shep_toml.rs
git commit -m "feat(cli): ShepToml can take the dog sections out"
```

---

### Task 3: The migration, and the read that follows it

**Files:**
- Create: `crates/shep-cli/src/commands/dog_migration.rs`
- Modify: `crates/shep-cli/src/commands/mod.rs` (or wherever sibling command modules are declared, grep `mod shep_toml;` and follow)
- Modify: `crates/shep-cli/src/commands/daemon.rs:298-306` (`run_daemon`)
- Modify: `crates/shep-daemon/src/dogs.rs:337-354` (`dog_section`)
- Modify: `crates/shep-daemon/src/rpc.rs:69-74` and `:507`
- Modify: `crates/shep-daemon/src/boot.rs:1379`

**Interfaces:**
- Consumes: `DogsConfig` and `ShepPaths::dogs_config` from Task 1, `ShepToml::take_dog_sections` from Task 2.
- Produces: `migrate_dog_sections(paths: &ShepPaths) -> Result<Vec<String>, DogMigrationError>`, returning the names moved, empty when there was nothing to move.

**These two changes share one commit, deliberately.** They are the exception in the one-commit-per-item rule: a commit that migrates sections without switching the read leaves every dog reading an empty section, and a commit that switches the read without migrating does the same. Neither is a state anyone should be able to bisect onto.

- [ ] **Step 1: Write the failing tests**

In `crates/shep-cli/src/commands/dog_migration.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(shep_toml: &str) -> (tempfile::TempDir, ShepPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = ShepPaths::for_home(dir.path());
        std::fs::write(&paths.daemon_config, shep_toml).expect("write");
        (dir, paths)
    }

    #[test]
    fn sections_move_and_the_original_loses_them() {
        let (_dir, paths) = home_with(
            "# mine\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\nbind = \"127.0.0.1:9615\"\n",
        );

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert_eq!(moved, vec!["metrics".to_string()]);
        assert_eq!(
            std::fs::read_to_string(&paths.daemon_config).expect("read"),
            "# mine\n[daemon]\nenabled_dogs = [\"metrics\"]\n"
        );
        let written = std::fs::read_to_string(&paths.dogs_config).expect("read");
        let parsed = DogsConfig::load(Some(&written)).expect("valid");
        assert_eq!(parsed.dog["metrics"]["bind"].as_str(), Some("127.0.0.1:9615"));
    }

    #[test]
    fn a_file_with_no_dog_sections_is_not_rewritten() {
        let before = "[daemon]\nlog_level = \"info\"\n";
        let (_dir, paths) = home_with(before);

        let moved = migrate_dog_sections(&paths).expect("migrate");

        assert!(moved.is_empty());
        assert_eq!(std::fs::read_to_string(&paths.daemon_config).expect("read"), before);
        assert!(!paths.dogs_config.exists(), "no sections means no file");
    }

    #[test]
    fn a_second_boot_writes_nothing() {
        let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");
        migrate_dog_sections(&paths).expect("first");
        let after_first = std::fs::read_to_string(&paths.dogs_config).expect("read");

        let moved = migrate_dog_sections(&paths).expect("second");

        assert!(moved.is_empty());
        assert_eq!(std::fs::read_to_string(&paths.dogs_config).expect("read"), after_first);
    }

    // The one case that must never silently merge: an operator who already
    // hand-wrote dogs.toml and still has a stale section in shep.toml. Two
    // values for one key, and picking either would be shep guessing.
    #[test]
    fn an_existing_dogs_file_makes_the_migration_refuse() {
        let (_dir, paths) = home_with("[dog.metrics]\nbind = \"127.0.0.1:9615\"\n");
        std::fs::write(&paths.dogs_config, "[metrics]\nbind = \"0.0.0.0:9615\"\n").expect("write");

        let err = migrate_dog_sections(&paths).expect_err("both files hold metrics");

        assert!(matches!(err, DogMigrationError::WouldOverwrite { .. }));
        assert!(
            std::fs::read_to_string(&paths.daemon_config)
                .expect("read")
                .contains("[dog.metrics]"),
            "a refused migration strikes nothing"
        );
    }
}
```

If `ShepPaths` has no `for_home` constructor, grep `paths.rs` for how its tests build one against a temp directory and use that instead. Do not add a constructor for the test's convenience.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: FAIL, `cannot find function 'migrate_dog_sections'`.

- [ ] **Step 3: Write the migration**

`crates/shep-cli/src/commands/dog_migration.rs`:

```rust
//! Moving `[dog.<name>]` out of `shep.toml` and into `dogs.toml`, once.
//!
//! Runs at the top of every daemon boot and does nothing on all but the
//! first. `RawDaemonConfig` keeps its `dog` field so an un-migrated file
//! still parses: deleting it would turn `deny_unknown_fields` into a
//! refused boot for every operator carrying a dog section, with the flock
//! left unsupervised at exactly the moment nobody is watching.

/// Moves every `[dog.<name>]` section into `dogs.toml`, returning the names
/// moved
///
/// Empty when there was nothing to move, which is every boot after the
/// first. Writes `dogs.toml` before striking `shep.toml`, so a crash
/// between the two leaves the sections readable from the old file rather
/// than from neither.
///
/// # Errors
///
/// - [`DogMigrationError::WouldOverwrite`] when a name is present in both
///   files. Two values for one key is a question shep cannot answer, so it
///   refuses and changes nothing.
/// - [`DogMigrationError::Read`], [`DogMigrationError::Write`] and
///   [`DogMigrationError::Edit`] for the underlying I/O.
pub fn migrate_dog_sections(paths: &ShepPaths) -> Result<Vec<String>, DogMigrationError> {
    let existing_source = match std::fs::read_to_string(&paths.daemon_config) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(DogMigrationError::Read(err)),
    };
    // Cheap check first, and load-bearing rather than an optimisation.
    // `ShepToml::edit` and `try_edit` both stage a temp file and rename it
    // over the original whenever `save` runs, so opening the document at
    // all would give an untouched `shep.toml` a fresh inode, force
    // `CONFIG_FILE_MODE` on it, and replace a symlinked path with a plain
    // file. A boot with nothing to do must not open it.
    if !existing_source.contains("[dog.") && !existing_source.contains("[dog]") {
        return Ok(Vec::new());
    }

    let already = match std::fs::read_to_string(&paths.dogs_config) {
        Ok(source) => DogsConfig::load(Some(&source)).map_err(DogMigrationError::Parse)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DogsConfig::default(),
        Err(err) => return Err(DogMigrationError::Read(err)),
    };

    // `try_edit`, never `edit`. `edit` calls `save` unconditionally
    // (shep_toml.rs:169), so refusing from inside it would strike the
    // sections from `shep.toml` and only then fail, leaving them in
    // neither file. `try_edit`'s own `Err` skips `save` entirely and
    // leaves `path` exactly as it was found.
    ShepToml::try_edit(&paths.daemon_config, |doc| {
        let incoming = doc.take_dog_sections();
        if let Some(name) = incoming.keys().find(|name| already.dog.contains_key(*name)) {
            return Err(DogMigrationError::WouldOverwrite { name: name.clone() });
        }
        let mut moved: Vec<String> = incoming.keys().cloned().collect();
        moved.sort();

        let mut merged = already.dog.clone();
        merged.extend(incoming);
        let rendered = toml::to_string(&merged).map_err(DogMigrationError::Render)?;
        // Written before this closure returns, so `save` strikes the old
        // sections only once the new file already holds them. A crash
        // between the two leaves them readable from `shep.toml`, which is
        // the direction that loses nothing.
        std::fs::write(&paths.dogs_config, rendered).map_err(DogMigrationError::Write)?;
        Ok(moved)
    })
}
```

`try_edit` is generic as `E: From<ShepTomlError>`, so `DogMigrationError` needs a
`From<ShepTomlError>` impl rather than an `Edit` variant the caller maps into.
Give it a `Toml(ShepTomlError)` variant and that `From`.

**`ShepToml::edit` versus `try_edit` was checked before this plan shipped, and the answer is why the code above looks the way it does.** `edit` runs `doc.save()` unconditionally (shep_toml.rs:169); the module doc's "only when the closure actually produced a value to save" describes `try_edit`, whose `f` can return `Err`. An earlier draft of this task used `edit` for a read-only collision pass and would have struck the sections before refusing. Do not reintroduce it.

Write `DogMigrationError` as an enum with the variants the doc names, a `Display`, a `core::error::Error` with `source`, and `#[non_exhaustive]` carrying a reason comment (IR-20). Derived `Debug` is correct here and needs saying in a comment: the variants carry a dog name and an I/O error, never a section's contents, so there is nothing to redact.

- [ ] **Step 4: Wire it into the boot**

In `crates/shep-cli/src/commands/daemon.rs`, at the top of `run_daemon`, before `boot_supervisor`:

```rust
pub async fn run_daemon(paths: ShepPaths, args: &DaemonArgs) -> Result<(), DaemonRunError> {
    // Before the supervisor, because `dog_section` reads the new file from
    // the first request onward and a dog can connect as soon as the socket
    // is up. A boot after the first finds nothing and returns immediately.
    let moved = crate::commands::dog_migration::migrate_dog_sections(&paths)
        .map_err(DaemonRunError::DogMigration)?;
    if !moved.is_empty() {
        // Named individually: an operator who did not know this was coming
        // needs to be able to find where their config went.
        tracing::info!(
            dogs = %moved.join(", "),
            "moved dog config out of shep.toml and into dogs.toml"
        );
    }
    // A production daemon always keeps its final roll -- `shep muster` after
    // a reboot is the entire reason it exists.
    boot_supervisor(paths, args, false)
        .await?
        .run()
        .await
        .map_err(DaemonRunError::Run)
}
```

Add a `DogMigration(DogMigrationError)` variant to `DaemonRunError` with its `Display` arm and its `source`, and give it an exit code in `daemon_exit_code` matching whatever that function does for the other pre-boot failures. Match the file's existing `tracing` call style: if it uses `tracing::info!` with a message-first argument order, follow that.

- [ ] **Step 5: Switch the read**

In `crates/shep-daemon/src/dogs.rs`, `dog_section` currently loads `DaemonConfig` and reads `config.dog.get(name)`. Change it to `DogsConfig::load` and `config.dog.get(name)`, keeping the `NotFound` arm answering `Ok(String::new())` and keeping `toml::to_string(table)` as the return, so the bytes on the wire do not move. Update its doc comment, which names `shep.toml`.

In `crates/shep-daemon/src/rpc.rs`, rename the context field `daemon_config` to `dogs_config` (its doc comment at line 69 says "Where `DogConfig` reads a dog's `[dog.<name>]` section from", which needs rewriting for the new file and the new key shape), and update the call at line 507. Update the test at line 2603 that writes `[dog.bark]` into that path: it now writes `[bark]` into `dogs.toml`.

In `crates/shep-daemon/src/boot.rs:1379`, change `daemon_config: paths.daemon_config.clone()` to `dogs_config: paths.dogs_config.clone()`.

- [ ] **Step 6: Add the wire-unchanged test**

This is the assertion that decision 3 actually held. Put it beside the existing `dog_section` tests in `crates/shep-daemon/src/dogs.rs`:

```rust
#[test]
fn a_section_reaches_the_wire_exactly_as_it_did_from_shep_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dogs.toml");
    std::fs::write(&path, "[bark]\ndebounce = \"30s\"\n").expect("write");

    // Byte-for-byte what the old `[dog.bark]` read produced. The dog-facing
    // contract not moving is the whole of decision 3, so it is pinned as a
    // string rather than as a parse.
    assert_eq!(
        dog_section(&path, "bark").expect("section"),
        "debounce = \"30s\"\n"
    );
}

#[test]
fn a_dog_with_no_section_still_gets_an_empty_string() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dogs.toml");
    std::fs::write(&path, "[bark]\ndebounce = \"30s\"\n").expect("write");

    assert_eq!(dog_section(&path, "metrics").expect("section"), "");
}
```

If the existing `dog_section` tests assert a different exact string for the same input, use theirs: the point is that the value does not change, so the old expectation is the correct one.

- [ ] **Step 7: Run the tests**

Run: `cargo test --workspace --lib --bins --all-features -- --skip ::slow::`
Expected: PASS.

- [ ] **Step 8: Prove they are not vacuous**

Two mutations, because this task has two halves. Make `migrate_dog_sections` return `Ok(Vec::new())` before doing anything and confirm `sections_move_and_the_original_loses_them` fails. Then make `dog_section` return `Ok(String::new())` unconditionally and confirm `a_section_reaches_the_wire_exactly_as_it_did_from_shep_toml` fails. Restore both. Report both.

- [ ] **Step 9: Commit**

```bash
git add crates/shep-cli/src/commands/dog_migration.rs crates/shep-cli/src/commands/mod.rs crates/shep-cli/src/commands/daemon.rs crates/shep-daemon/src/dogs.rs crates/shep-daemon/src/rpc.rs crates/shep-daemon/src/boot.rs
git commit -m "feat: dog config moves to dogs.toml, migrated on boot"
```

---

### Task 4: The operator-facing account

**Files:**
- Modify: `docs/dogs.md` (the `## Configuration` section, around line 91)
- Modify: `web/src/pages/docs/*.astro` (grep for `[dog.` first)
- Modify: `CLAUDE.md`

**Interfaces:**
- Consumes: everything from Tasks 1 through 3.
- Produces: nothing code-facing.

`web/` is published and part of the public surface, so this is not optional upkeep.

- [ ] **Step 1: Find every mention**

```bash
grep -rn '\[dog\.' docs/ web/src/ CLAUDE.md README.md
```

Grep the word, not the phrase: a claim about dog config is rarely in only one place, and a wrapped line will not match a phrase search.

- [ ] **Step 2: Rewrite `docs/dogs.md`'s Configuration section**

Its current text says settings live under `[dog.<name>]` in `shep.toml`, and that editing one does not reach a running dog so `shep disable <name> && shep enable <name>` is what re-reads it. Both halves change: the file is now `$SHEP_HOME/dogs.toml` with the section keyed as `[<name>]`, and the disable-and-enable sentence stays true for this release but should say the file it is talking about.

Keep the paragraph explaining why the section rides the socket instead of the environment. It is still correct and it is the best thing on that page.

Add a short paragraph on the migration: that it happens once, on the first boot of a version carrying it, that shep says which dogs moved, and that a name present in both files makes it refuse rather than guess.

- [ ] **Step 3: Rewrite the web pages the grep found**

Same substance, matching each page's existing voice rather than pasting the markdown across.

- [ ] **Step 4: Update `CLAUDE.md`**

One paragraph recording that dog config no longer lives in `shep.toml`, that `RawDaemonConfig` keeps its `dog` field on purpose and why, and that the migration is in `crates/shep-cli/src/commands/dog_migration.rs`.

- [ ] **Step 5: Build the site**

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```
```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

Each from its own command with `$?` captured directly. `astro check` is the one that catches a wrong prop: a page passing a component a prop it does not have builds clean and renders wrong, which shipped once already on `/docs/output`.

`git diff` on `web/src/data/cli-reference.generated.txt` should be empty for this release, since no verb, flag or alias changed. If it is not empty, something drifted before this branch and the diff is worth reading rather than committing blind.

- [ ] **Step 6: Commit**

```bash
git add docs/dogs.md web/ CLAUDE.md
git commit -m "docs: dog config lives in dogs.toml now"
```

---

## Self-Review

**Spec coverage.** Decision 1 is Tasks 1 and 3. Decision 2 is Task 3, with `RawDaemonConfig::dog` deliberately retained and that argued in the File Structure table. Decision 3 is Task 3 step 6, pinned as an exact string rather than assumed. The spec's Testing section lists eight cases: the migration exact-string, the no-sections no-rewrite, the second-boot no-op, and the byte-identical `dog_section` are Tasks 2 and 3. The remaining four (`probe` answering both flags, the three bad-`--schema` shapes, the secret marker round-trip, and the bus topic publishing once) all belong to decisions 4 through 9 and are releases 2 and 3, correctly absent here. The spec's Docs section is Task 4.

**Additions the spec did not name.** The both-files collision refusal (`WouldOverwrite`) is not in the spec. It came out of writing Task 3: decision 1 makes `dogs.toml` hand-editable, so an operator can create one before upgrading, and a migration that merged silently would pick a winner between two values for the same key. Refusing is the only answer that does not guess. Worth folding back into the spec's decision 2 when this lands.

**Placeholder scan.** No TBDs. Two steps deliberately say "grep and follow" rather than naming an exact line: the `config/mod.rs` re-export style in Task 1 step 4, and `ShepToml::edit`'s write-on-empty-closure semantics in Task 3 step 3. Both are questions about existing code that a plan should not answer from memory, and both say what to do with either answer.

**Type consistency.** `DogsConfig::load(Option<&str>)` in Task 1 is what Task 3 calls. `take_dog_sections(&mut self) -> BTreeMap<String, toml::Table>` in Task 2 is what Task 3 consumes twice. `ShepPaths::dogs_config` from Task 1 is what Tasks 3 reads in three files. `dog_section(&Path, &str) -> Result<String, DogError>` keeps its existing signature throughout, which is what makes decision 3 true.
