# lookout settings screen implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shep lookout` gains a settings screen, opened with `s`, that reads and writes `$SHEP_HOME/shep.toml`: six scalars and a per-dog toggle, editing behind the existing `--allow-control` gate and reading outside it.

**Architecture:** The reducer stays pure. Two new effects (`LoadSettings`, `WriteSetting`) are performed in `run_ui` on `spawn_blocking`, and a dog toggle chains a third step through the existing `Effect::Send`. The screen reads presence out of the `toml_edit` document rather than through `DaemonConfig`, because `DaemonConfig::load` destroys the difference between an absent key and one written to its default. `DaemonConfig` keeps two jobs: the effective value when a key is absent, and validation inside `try_edit`.

**Tech Stack:** Rust 2024, MSRV 1.88. `ratatui` for the screen, `toml_edit` through the existing `ShepToml`, `insta` for frame snapshots, `tokio` for `spawn_blocking`.

**Spec:** [docs/brainstorming/specs/2026-09-04-lookout-settings-design.md](../../brainstorming/specs/2026-09-04-lookout-settings-design.md). Read it before task 1. It carries the reasoning this plan only summarises, and its eleven decisions are cited by number throughout.

## Global constraints

Every task's requirements implicitly include all of these.

- **Clean-room rule, non-negotiable.** Never open, read, or port source from `~/GitHub/pm2`. Nothing in this feature has a pm2 ancestor.
- **Invoke the `shep-idiomatic-rust` skill before writing or reviewing any Rust in this repository.** Cite rules as `IR-<n>` in review.
- **No em dashes and no en dashes anywhere.** Not in code, comments, docs, commit messages, operator-facing strings or the PR body. Existing strings that carry one are out of scope and stay as they are.
- **Operator-facing prose goes through the project's voice review before it lands.** That covers every string this feature prints, the docs page, and the PR body. Not commit messages, which stay long and detailed.
- **Never write the maintainer's real name, personal email, or any absolute home-directory path** into a committed file, a commit message, or a PR body. Repo-relative paths only.
- **Any snippet in this plan that quotes existing code is a guess.** Grep the real file and follow what is there, not what is written here. Signatures for code this plan introduces are binding; quotations of code that already exists are not.
- **Every new test must be proved non-vacuous.** Mutate the thing it protects, watch that test go red, and **verify the mutation actually applied** before trusting the red. `cargo fmt` has rewrapped a line a patch was matching, leaving the patch a silent no-op and three green runs looking like evidence.
- **No sleeps in tests (IR-46).** Every wait has a forcing mechanism. Expiry rides synthesised `Instant`s through `Msg::Tick`, the way `a_confirm_expires_after_ten_seconds_of_ticks` already does.
- **One cargo shape for the whole task, while iterating:**
  ```
  cargo test --workspace --lib --bins --all-features -- --skip ::slow::
  ```
  Do not alternate with `-p`. Each switch invalidates feature resolution and rebuilds the world.
- **Every new public item needs docs and a deliberate `Debug` decision.** Decision 11 of the spec establishes that nothing on this screen is sensitive, so no redacted `Debug` is owed. Say so at the call site rather than leaving it silent.
- **Commit at the end of every task.** One commit per task, conventional-commit subject.

---

## File structure

| File | Responsibility |
| --- | --- |
| `crates/shep-cli/src/lookout/input.rs` | keymap only. Gains `s` and `space`; the four `Filter*` presses become `Text*`. |
| `crates/shep-cli/src/commands/shep_toml.rs` | the document. Gains typed readers, setters and two unsetters for the six scalars, plus a lock-free `read_only` constructor. Knows nothing about validity. |
| `crates/shep-cli/src/commands/settings.rs` | **new.** Owns `SettingsSnapshot`, `SettingField`, `SettingEdit`, `load_settings` and `apply_setting`. The one place that pairs the document with `DaemonConfig`'s validation. |
| `crates/shep-cli/src/commands/dogs.rs` | gains `enable_in_config` and `disable_in_config`, extracted from `enable` and `disable`. Reporting stays where it is. |
| `crates/shep-cli/src/lookout/app.rs` | reducer state and transitions: `App.settings`, the new `Msg`, `Effect` and `Sent` variants. |
| `crates/shep-cli/src/lookout/view/settings.rs` | **new.** Renders the screen. Holds no state. |
| `crates/shep-cli/src/lookout/view/mod.rs` | dispatches `draw` to the screen or the dashboard. |
| `crates/shep-cli/src/lookout/mod.rs` | performs the two new effects on `spawn_blocking`. |
| `crates/shep-cli/src/lookout/frames.rs` | gallery scenes for the screen. |
| `web/src/pages/docs/lookout.astro` | the `s` key and the screen, for operators. |

## Dependency graph

Tasks 1 and 2 are independent and dispatch together. Task 4 reads back through task 2's `read_only` and `enabled_dog_names`, so it is not independent of it, and 3 and 4 form the second parallel leg.

```
1 (rename) ──────────────────────┐
2 (ShepToml) ─┬─> 3 (settings) ──┴─> 5 (open/close) ─> 6 (render) ─┬─> 7 (scalars) ─> 8 (editor) ─┬─> 10 (frames) ─> 11 (docs)
              └─> 4 (dogs) ──────────────────────────────────────────────────────────────────────┘
```

---

### Task 1: Rename `KeyPress::Filter*` to `Text*`

The settings editor needs the identical text keymap, and a variant named for the filter box would name a destination the keymap cannot see. Mechanical, no behaviour change, and it lands first because it touches shipped code and the keymap's own test.

**Files:**
- Modify: `crates/shep-cli/src/lookout/input.rs`
- Modify: `crates/shep-cli/src/lookout/app.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `KeyPress::TextChar(char)`, `KeyPress::TextBackspace`, `KeyPress::TextApply`, `KeyPress::TextAbandon`. `KeyPress::FilterStart` keeps its name: it opens the filter box specifically and is not shared with the settings editor.

- [ ] **Step 1: Rename the four variants and every use**

In `input.rs`, the `InputMode::Text` arm returns the new names. In `app.rs`, the enum declaration, the doc comments on each variant, and every `match` arm follow.

Rewrite each variant's doc so it describes the keymap rather than the filter. For example, `TextChar`'s becomes "One printable character typed into whichever text field is open." The reducer, not the keymap, decides which that is, which is the same division `KeyPress::Escape`'s own doc already argues for.

- [ ] **Step 2: Run the suite and confirm it is green**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

Expected: PASS, with no test count change. This task adds no test because it adds no behaviour; `every_bound_key_resolves_to_its_press` in `input.rs` already covers the mapping and now covers it under the new names.

- [ ] **Step 3: Prove the existing test still binds**

Change `KeyCode::Backspace` in `input.rs`'s text arm to return `KeyPress::TextApply`. Confirm the patch applied by re-reading the line. Run the suite and confirm `every_bound_key_resolves_to_its_press` fails. Revert.

- [ ] **Step 4: Commit**

```bash
git add crates/shep-cli/src/lookout/input.rs crates/shep-cli/src/lookout/app.rs
git commit -m "refactor(lookout): the text keymap is named for text, not for the filter"
```

---

### Task 2: `ShepToml` gains typed access to the six scalars

The document layer only. Nothing here knows whether a value is legal; that is task 3's job and `DaemonConfig`'s.

**Files:**
- Modify: `crates/shep-cli/src/commands/shep_toml.rs`

**Interfaces:**
- Consumes: nothing.
- Produces, all on `ShepToml`:
  ```rust
  pub fn read_only(path: &Path) -> Result<Self, ShepTomlError>

  pub fn daemon_log_json(&self) -> Option<bool>
  pub fn daemon_log_level(&self) -> Option<String>
  pub fn daemon_socket(&self) -> Option<PathBuf>
  pub fn daemon_max_cron_sleep(&self) -> Option<String>
  pub fn whistle_allow_control(&self) -> Option<bool>
  pub fn style_level(&self) -> Option<String>

  pub fn set_daemon_log_json(&mut self, value: bool) -> Result<(), ShepTomlError>
  pub fn set_daemon_log_level(&mut self, value: &str) -> Result<(), ShepTomlError>
  pub fn set_daemon_socket(&mut self, value: &Path) -> Result<(), ShepTomlError>
  pub fn set_daemon_max_cron_sleep(&mut self, value: &str) -> Result<(), ShepTomlError>
  pub fn set_whistle_allow_control(&mut self, value: bool) -> Result<(), ShepTomlError>

  pub fn unset_daemon_socket(&mut self)
  pub fn unset_daemon_max_cron_sleep(&mut self)

  pub fn enabled_dog_names(&self) -> Vec<String>
  ```
  Every reader returns `None` for an absent key, which is the distinction the whole screen rests on. `set_style_level` and `adopted_dog_names` already exist and are not re-declared.

  The setters return `Result` for the reason `set_style_level` already does: an operator's hand-written `daemon = "loud"` is a key of the wrong shape, and `ShepTomlError::WrongShape` is what says so instead of clobbering it. The two unsetters cannot meet that case, because removing a key from a table that is not a table is already a no-op, so they return nothing.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_document_with_no_daemon_section_reads_every_scalar_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "[interpreters]\njs = \"node\"\n").unwrap();

    let cfg = ShepToml::read_only(&path).unwrap();
    assert_eq!(cfg.daemon_log_json(), None);
    assert_eq!(cfg.daemon_log_level(), None);
    assert_eq!(cfg.daemon_socket(), None);
    assert_eq!(cfg.daemon_max_cron_sleep(), None);
    assert_eq!(cfg.whistle_allow_control(), None);
    assert_eq!(cfg.style_level(), None);
}

/// The distinction the screen rests on: a key written to its own default is
/// not the same fact as a key nobody wrote, and `DaemonConfig::load` cannot
/// tell them apart because every section is `serde(default)`.
#[test]
fn a_scalar_written_to_its_default_still_reads_as_present() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "[daemon]\nlog_level = \"warn\"\nlog_json = false\n").unwrap();

    let cfg = ShepToml::read_only(&path).unwrap();
    assert_eq!(cfg.daemon_log_level().as_deref(), Some("warn"));
    assert_eq!(cfg.daemon_log_json(), Some(false));
}

#[test]
fn a_missing_file_reads_as_an_empty_document_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");

    let cfg = ShepToml::read_only(&path).unwrap();
    assert_eq!(cfg.daemon_log_level(), None);
    assert!(!path.exists(), "a read must never create the file");
}

#[test]
fn setting_a_scalar_keeps_the_comments_and_the_keys_around_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(
        &path,
        "# keep me\n[daemon]\nenabled_dogs = [\"metrics\"]\n\n[style]\nlevel = \"full\"\n",
    )
    .unwrap();

    ShepToml::try_edit(&path, |cfg| cfg.set_daemon_log_level("debug")).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# keep me\n"), "got: {text}");
    assert!(text.contains("enabled_dogs = [\"metrics\"]"), "got: {text}");
    assert!(text.contains("level = \"full\""), "got: {text}");
    assert!(text.contains("log_level = \"debug\""), "got: {text}");
}

#[test]
fn unsetting_removes_the_key_and_leaves_its_neighbours() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(
        &path,
        "[daemon]\nlog_level = \"debug\"\nmax_cron_sleep = \"30s\"\n",
    )
    .unwrap();

    ShepToml::edit(&path, ShepToml::unset_daemon_max_cron_sleep).unwrap();

    let cfg = ShepToml::read_only(&path).unwrap();
    assert_eq!(cfg.daemon_max_cron_sleep(), None);
    assert_eq!(cfg.daemon_log_level().as_deref(), Some("debug"));
}

#[test]
fn a_daemon_key_of_the_wrong_shape_is_refused_rather_than_clobbered() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "daemon = \"loud\"\n").unwrap();

    // `try_edit`, not `edit`. `edit` runs `save` whatever the closure
    // returned, so a refusal through it would still stage and rename a
    // byte-identical copy, landing a fresh inode on a file the refusal never
    // touched. That is the failure `try_edit` exists for, and it is why
    // every setter on this screen goes through it.
    let refusal: Result<(), ShepTomlError> =
        ShepToml::try_edit(&path, |cfg| cfg.set_daemon_log_json(true));

    assert!(matches!(
        refusal,
        Err(ShepTomlError::WrongShape { key: "daemon", .. })
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "daemon = \"loud\"\n");
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow:: shep_toml
```

Expected: FAIL, on unresolved methods.

- [ ] **Step 3: Implement**

Model the readers on `adopted_dog_names`, which already walks `doc.get("daemon").and_then(Item::as_table)`. Model `read_only` on `adopted_dog_path_readonly`: `Ok(Self::open(path)?)`, taking no lock, and carry that function's own argument in the doc comment, that `save`'s rename is atomic so a concurrent writer is observed before or after it and never torn.

Model the setters on `set_style_level`, which is already the shape: `entry(section).or_insert_with(Table)`, refuse with `WrongShape` if it is not a table, then `insert`. Factor the section lookup rather than writing it five times.

The two unsetters call `remove` on the section table if it is one, and do nothing if it is not.

`daemon_max_cron_sleep` returns the raw string as written, not a parsed `UpDuration`. The screen shows what the file says, and parsing it here would put a second opinion about the grammar next to `DaemonConfig`'s.

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

Expected: PASS.

- [ ] **Step 5: Prove each test binds**

One mutation per test, each reverted before the next. Make `daemon_log_json` return `Some(false)` for an absent key and confirm the absence test reddens. Make a setter write into `[style]` instead of `[daemon]` and confirm the comment-preservation test reddens. Make `unset_daemon_max_cron_sleep` a no-op and confirm the unset test reddens. Re-read each patched line before running, to confirm the patch applied.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/commands/shep_toml.rs
git commit -m "feat(cli): ShepToml can read and write shep.toml's six scalars"
```

---

### Task 3: `commands/settings.rs`, the one place that pairs the document with validation

`shep_toml.rs`'s own module doc says `DaemonConfig::load` is "the one place the SHAPE of the file is decided" and that `shep_toml` "only ever adds or removes the handful of keys each verb owns". So validation does not go there. This module is where the two meet.

**Files:**
- Create: `crates/shep-cli/src/commands/settings.rs`
- Modify: `crates/shep-cli/src/commands/mod.rs` (declare the module)

**Interfaces:**
- Consumes: task 2's `ShepToml` readers, setters and unsetters.
- Produces:
  ```rust
  // No new source enum. `crate::style::StyleSource` already has exactly
  // `Flag`, `Env`, `Config`, `Default`, and "which layer decided" is one
  // concept, so this reuses it rather than declaring a twin. Its `Display`
  // spells `Flag` and `Env` as `--style` and `$SHEP_STYLE`, which are
  // style-specific, and that is correct here because only `style_level`
  // ever produces those two variants: the shepherd's own env and flags are
  // invisible to this process, so a `[daemon]` field is only ever `Config`
  // or `Default`.
  //
  // `Config` is not a claim that the shepherd is using the value. It says
  // the key is in the file. See the spec, "Two things decision 11 does not
  // cover".
  use crate::style::StyleSource;

  /// One scalar as the screen shows it.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct ScalarView {
      /// Already rendered for display, defaults resolved.
      pub value: String,
      pub source: StyleSource,
  }

  /// Which scalar an edit names.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SettingField {
      LogLevel, LogJson, Socket, MaxCronSleep, AllowControl, StyleLevel,
  }

  /// One edit, ready to apply.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum SettingEdit {
      Set { field: SettingField, value: String },
      /// Only `Socket` and `MaxCronSleep` reach this: `style_level` is owned
      /// by `shep style`, which cannot clear it, and the other three are not
      /// optional (spec decision 5).
      Unset { field: SettingField },
  }

  /// Everything the screen reads off disk in one go.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct SettingsSnapshot {
      pub log_level: ScalarView,
      pub log_json: ScalarView,
      pub socket: ScalarView,
      pub max_cron_sleep: ScalarView,
      pub allow_control: ScalarView,
      pub style_level: ScalarView,
      /// Every candidate dog: `BUILT_IN_DOGS` plus every `adopted_dogs` key,
      /// sorted, deduplicated.
      pub dogs: Vec<DogView>,
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct DogView {
      pub name: String,
      pub enabled: bool,
      /// `None` for a built-in dog; the adopted binary's path otherwise.
      pub adopted_path: Option<PathBuf>,
  }

  #[derive(Debug)]
  pub enum SettingError {
      Config(ShepTomlError),
      /// `DaemonConfig::load` refused the document this edit would have
      /// written. Carries the loader's own message.
      Invalid(String),
  }

  /// Reads the snapshot. Takes no lock.
  pub fn load_settings(
      path: &Path,
      socket_default: &Path,
      style: (StyleLevel, StyleSource),
  ) -> Result<SettingsSnapshot, ShepTomlError>

  /// Applies one edit under the config lock, validating before it saves.
  pub fn apply_setting(path: &Path, edit: &SettingEdit) -> Result<(), SettingError>
  ```
  `load_settings` takes the already-resolved style pair, which is exactly what `lib.rs`'s `resolve_style` returns (`fn resolve_style(global: &GlobalArgs) -> (style::StyleLevel, style::StyleSource)`, `lib.rs:533`). `Streams.style` is a `Presentation` and carries the level without the source, so it is not enough on its own.

  Plumbing, decided rather than left open: `lookout()` gains a `style: (StyleLevel, StyleSource)` parameter, passed from the dispatch site that already computes it, and `App` holds the pair beside `palette`. One parameter and one field, with no second call to `style::resolve` that could disagree with the first.

  `socket_default` is `paths.socket`, the socket this lookout is connected over, so `socket`'s default row is the live answer by construction rather than a recomputed guess.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_fresh_home_reads_every_scalar_as_the_default() {
    // What `scaffold_first_run_interpreters` actually leaves behind, and the
    // state most operators open this screen in.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    ShepToml::edit(&path, ShepToml::write_starter_interpreters).unwrap();

    let snap = load_settings(&path, &socket_fixture(), style_fixture()).unwrap();
    assert_eq!(snap.log_level.source, StyleSource::Default);
    assert_eq!(snap.log_level.value, "warn");
    assert_eq!(snap.log_json.source, StyleSource::Default);
    assert_eq!(snap.allow_control.source, StyleSource::Default);
    assert_eq!(snap.max_cron_sleep.source, StyleSource::Default);
}

#[test]
fn a_declared_scalar_reads_as_config_even_at_its_default_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "[daemon]\nlog_level = \"warn\"\n").unwrap();

    let snap = load_settings(&path, &socket_fixture(), style_fixture()).unwrap();
    assert_eq!(snap.log_level.value, "warn");
    assert_eq!(
        snap.log_level.source,
        StyleSource::Config,
        "a key written to its own default is still a key someone wrote"
    );
}

#[test]
fn a_value_the_loader_refuses_leaves_the_file_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    let before = "# mine\n[daemon]\nlog_level = \"debug\"\n";
    std::fs::write(&path, before).unwrap();
    let inode_before = std::fs::metadata(&path).unwrap().ino();

    let refusal = apply_setting(
        &path,
        &SettingEdit::Set {
            field: SettingField::MaxCronSleep,
            value: "500ms".into(),
        },
    );

    assert!(matches!(refusal, Err(SettingError::Invalid(_))));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&path).unwrap().ino(),
        inode_before,
        "a refusal must not stage and rename, which is what try_edit buys"
    );
}

#[test]
fn the_refusal_carries_the_loaders_own_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "").unwrap();

    let Err(SettingError::Invalid(message)) = apply_setting(
        &path,
        &SettingEdit::Set {
            field: SettingField::MaxCronSleep,
            value: "500ms".into(),
        },
    ) else {
        panic!("a value under the floor must be refused");
    };
    assert!(
        message.contains("max_cron_sleep"),
        "the operator has to be told which key: {message}"
    );
}

#[test]
fn unsetting_an_optional_field_returns_it_to_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "[daemon]\nmax_cron_sleep = \"30s\"\n").unwrap();

    apply_setting(&path, &SettingEdit::Unset { field: SettingField::MaxCronSleep }).unwrap();

    let snap = load_settings(&path, &socket_fixture(), style_fixture()).unwrap();
    assert_eq!(snap.max_cron_sleep.source, StyleSource::Default);
}

#[test]
fn every_built_in_dog_is_a_candidate_even_when_nothing_is_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "").unwrap();

    let snap = load_settings(&path, &socket_fixture(), style_fixture()).unwrap();
    let names: Vec<&str> = snap.dogs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["bark", "metrics"]);
    assert!(snap.dogs.iter().all(|d| !d.enabled));
}

#[test]
fn an_adopted_dog_joins_the_candidates_and_carries_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(
        &path,
        "[daemon]\nenabled_dogs = [\"otel\"]\n\n[daemon.adopted_dogs]\notel = \"/usr/local/bin/shep-otel\"\n",
    )
    .unwrap();

    let snap = load_settings(&path, &socket_fixture(), style_fixture()).unwrap();
    let otel = snap.dogs.iter().find(|d| d.name == "otel").unwrap();
    assert!(otel.enabled);
    assert_eq!(otel.adopted_path.as_deref(), Some(Path::new("/usr/local/bin/shep-otel")));
    let metrics = snap.dogs.iter().find(|d| d.name == "metrics").unwrap();
    assert_eq!(metrics.adopted_path, None, "a built-in dog has no path");
}
```

`style_fixture()` returns a `(StyleLevel, StyleSource)` pair with the level resolved from the config layer; `socket_fixture()` returns a `&Path` standing in for `paths.socket`. Write both once at the top of the test module.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow:: settings
```

Expected: FAIL, module not found.

- [ ] **Step 3: Implement**

`load_settings` opens through `ShepToml::read_only`, asks each task-2 reader for its `Option`, and pairs `Some` with `StyleSource::Config` and `None` with `StyleSource::Default` plus the compiled default rendered as a string. Take the defaults from `DaemonSection::default()` and `WhistleSection::default()` rather than typing literals, so a changed default cannot leave this module lying.

`socket`'s default is the resolved socket path, not an empty cell: pass it in from `ShepPaths` and render that. It is the socket this lookout is connected over, so it is the live answer by construction.

`style_level`'s source comes straight from the pair that was passed in. Every `[daemon]` and `[whistle]` field is `Config` or `Default` and nothing else, because this process cannot see the layers that would make it `Env` or `Flag`.

`apply_setting` goes through `ShepToml::try_edit`. Inside the closure: call the task-2 setter or unsetter, then render the document with `to_string()`, then `DaemonConfig::load(Some(&rendered), &|_| None)`. A loader `Err` becomes `SettingError::Invalid(err.to_string())` and returns from the closure, which is what leaves the file untouched. The env layer is `&|_| None` for the reason `reload_with_wait` already gives at its own pre-flight: this process's env is not the shepherd's, so layering it would refuse a file the shepherd would have survived and pass one it would not.

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

Expected: PASS.

- [ ] **Step 5: Prove each test binds**

Make `load_settings` report `Config` unconditionally and confirm the fresh-home test reddens. Drop the `DaemonConfig::load` call from `apply_setting` and confirm both refusal tests redden. Make the dog list skip `BUILT_IN_DOGS` and confirm the candidates test reddens. Re-read each patched line before running.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/commands/settings.rs crates/shep-cli/src/commands/mod.rs
git commit -m "feat(cli): a settings snapshot that keeps absent apart from defaulted"
```

---

### Task 4: Extract the dog toggle's decision from its reporting

`shep enable`'s file half holds a real decision: which `DogSource` a name resolves to, whether it names a dog at all, and the mutation. Its daemon half writes rows to stdout, which lookout does not have. Cut at that seam so lookout reuses the decision and brings its own reporting.

**Files:**
- Modify: `crates/shep-cli/src/commands/dogs.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub(crate) fn enable_in_config(path: &Path, name: &str) -> Result<DogSource, EnableRefusal>
  pub(crate) fn disable_in_config(path: &Path, name: &str) -> Result<DogSource, ShepTomlError>
  ```
  `EnableRefusal` already exists in this module and keeps both variants.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn enable_in_config_writes_the_name_and_reports_a_built_in_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");

    let source = enable_in_config(&path, "metrics").unwrap();

    assert!(matches!(source, DogSource::BuiltIn));
    assert!(ShepToml::read_only(&path).unwrap().enabled_dog_names().contains(&"metrics".to_string()));
}

#[test]
fn enable_in_config_refuses_a_name_that_is_no_dog_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, "").unwrap();

    let refusal = enable_in_config(&path, "nonsense");

    assert!(matches!(refusal, Err(EnableRefusal::UnknownDog { .. })));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "",
        "a refused enable leaves shep.toml untouched"
    );
}

#[test]
fn disable_in_config_removes_the_name_and_keeps_the_adoption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shep.toml");
    std::fs::write(
        &path,
        "[daemon]\nenabled_dogs = [\"otel\"]\n\n[daemon.adopted_dogs]\notel = \"/usr/local/bin/shep-otel\"\n",
    )
    .unwrap();

    let source = disable_in_config(&path, "otel").unwrap();

    assert!(matches!(source, DogSource::Adopted { .. }));
    let cfg = ShepToml::read_only(&path).unwrap();
    assert!(cfg.enabled_dog_names().is_empty());
    assert_eq!(
        cfg.adopted_dog_names(),
        vec!["otel".to_string()],
        "disable is not rehome, so the adoption survives"
    );
}
```

`DogSource` is `BuiltIn` or `Adopted { path: String }` (`shep-core/src/protocol/request.rs:587`). Note `path` is a `String` there, while `ShepToml::adopted_dog_path` hands back a `PathBuf`; `DogView.adopted_path` follows `ShepToml`, since that is where it is read.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow:: dogs
```

Expected: FAIL, on unresolved functions.

- [ ] **Step 3: Extract**

Move the body of `enable`'s `ShepToml::try_edit` call into `enable_in_config`, unchanged, and have `enable` call it. Same for `disable` and its `ShepToml::edit` call. No behaviour changes: the closure, the refusal, the lock and the order are all as they were. Carry over the comment explaining why the check lives inside the closure rather than before the call, because that reasoning belongs to the extracted function now.

- [ ] **Step 4: Run the suite**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

Expected: PASS, including every existing `dogs.rs` test unchanged. If any existing test needed editing, the extraction changed behaviour and is wrong.

- [ ] **Step 5: Prove the new tests bind**

Make `enable_in_config` skip the `BUILT_IN_DOGS` check and confirm the refusal test reddens. Make `disable_in_config` call `rehome_dog` instead and confirm the adoption test reddens. Re-read each patched line before running.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/commands/dogs.rs
git commit -m "refactor(cli): a dog toggle's config decision, apart from its reporting"
```

---

### Task 5: The screen opens and closes, read-only

State and transitions only. Nothing renders yet and nothing writes yet.

**Files:**
- Modify: `crates/shep-cli/src/lookout/app.rs`
- Modify: `crates/shep-cli/src/lookout/input.rs`
- Modify: `crates/shep-cli/src/lookout/mod.rs`

**Interfaces:**
- Consumes: task 3's `SettingsSnapshot`, `SettingField`; task 1's `KeyPress::Text*`.
- Produces:
  ```rust
  // input.rs, normal-mode arms
  KeyPress::Settings   // `s`
  KeyPress::Cycle      // `space`

  // app.rs
  pub enum Msg { /* ... */ Settings { result: Result<SettingsSnapshot, String> } }
  pub enum Effect { /* ... */ LoadSettings }

  /// The settings screen's own state. `None` on `App` is the dashboard.
  #[derive(Debug, Clone)]
  pub struct Settings { /* private */ }

  /// One row the cursor can sit on.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SettingsRow {
      Scalar(SettingField),
      /// Index into the snapshot's own dog list.
      Dog(usize),
  }

  impl Settings {
      pub fn snapshot(&self) -> &SettingsSnapshot;
      pub fn rows(&self) -> Vec<SettingsRow>;
      pub fn cursor(&self) -> Option<SettingsRow>;
  }

  impl App {
      pub fn settings(&self) -> Option<&Settings>;
  }
  ```
  `Msg::Settings` carries `Result<_, String>` rather than the error type, because the reducer holds no error types from `commands` and a notice needs a rendered sentence anyway. `run_ui` does the `to_string()`.

- [ ] **Step 1: Write the failing tests**

```rust
/// `s` asks for the read rather than opening on stale or empty state: the
/// file can have changed since the last look, and an empty screen while the
/// read is in flight is a screen that lies for one frame.
#[test]
fn s_asks_for_the_file_before_the_screen_opens() {
    let mut app = fixtures::full_app();
    assert_eq!(app.update(Msg::Key(KeyPress::Settings)), Effect::LoadSettings);
    assert!(app.settings().is_none(), "nothing opens until the read lands");
}

#[test]
fn the_screen_opens_when_the_read_lands() {
    let mut app = fixtures::full_app();
    let _ = app.update(Msg::Key(KeyPress::Settings));
    let _ = app.update(Msg::Settings { result: Ok(fixtures::settings_snapshot()) });
    assert!(app.settings().is_some());
}

#[test]
fn a_read_that_failed_says_so_and_leaves_the_dashboard_up() {
    let mut app = fixtures::full_app();
    let _ = app.update(Msg::Key(KeyPress::Settings));
    let _ = app.update(Msg::Settings { result: Err("no such file".into()) });
    assert!(app.settings().is_none());
    let notice = app.notice().expect("a failed read has to say so");
    assert!(notice.is_grave());
    assert!(notice.to_string().contains("no such file"));
}

#[test]
fn s_closes_the_screen_again() {
    let mut app = fixtures::app_in_settings();
    let _ = app.update(Msg::Key(KeyPress::Settings));
    assert!(app.settings().is_none());
}

/// The one arm of the `Escape` cascade this screen swaps. From the dashboard
/// with no filter, `Esc` quits; from here it must not.
#[test]
fn escape_closes_the_screen_and_never_quits() {
    let mut app = fixtures::app_in_settings();
    assert_eq!(app.update(Msg::Key(KeyPress::Escape)), Effect::None);
    assert!(app.settings().is_none());
}

#[test]
fn the_flock_cursor_and_the_filter_survive_the_swap() {
    let mut app = fixtures::full_app();
    let _ = app.update(Msg::Key(KeyPress::FilterStart));
    for c in "web".chars() {
        let _ = app.update(Msg::Key(KeyPress::TextChar(c)));
    }
    let _ = app.update(Msg::Key(KeyPress::TextApply));
    // `selected()` hands back an owned `Option<RowKey>` (app.rs:1434), so
    // there is nothing to clone.
    let selected = app.selected();
    let filter = app.filter().to_string();

    let _ = app.update(Msg::Key(KeyPress::Settings));
    let _ = app.update(Msg::Settings { result: Ok(fixtures::settings_snapshot()) });
    let _ = app.update(Msg::Key(KeyPress::Settings));

    assert_eq!(app.selected(), selected);
    assert_eq!(app.filter(), filter);
}

#[test]
fn the_settings_cursor_starts_at_the_first_row_on_every_open() {
    let mut app = fixtures::app_in_settings();
    let _ = app.update(Msg::Key(KeyPress::SelectDown));
    let _ = app.update(Msg::Key(KeyPress::SelectDown));
    let _ = app.update(Msg::Key(KeyPress::Settings));
    let _ = app.update(Msg::Key(KeyPress::Settings));
    let _ = app.update(Msg::Settings { result: Ok(fixtures::settings_snapshot()) });

    let first = app.settings().unwrap().rows()[0];
    assert_eq!(app.settings().unwrap().cursor(), Some(first));
}

#[test]
fn the_cursor_moves_through_the_scalars_and_into_the_dogs() {
    let mut app = fixtures::app_in_settings();
    let rows = app.settings().unwrap().rows();
    for _ in 0..rows.len() - 1 {
        let _ = app.update(Msg::Key(KeyPress::SelectDown));
    }
    assert_eq!(app.settings().unwrap().cursor(), Some(*rows.last().unwrap()));
    // and it stops rather than wrapping, the way the flock table does
    let _ = app.update(Msg::Key(KeyPress::SelectDown));
    assert_eq!(app.settings().unwrap().cursor(), Some(*rows.last().unwrap()));
}

#[test]
fn an_action_key_from_the_dashboard_is_unreachable_while_the_screen_is_up() {
    let mut app = fixtures::app_in_settings_with_control();
    // `x` is the stop key on the dashboard. In here it is not an action at all.
    let _ = app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
    // The accessor is `App::action()` (app.rs:1662).
    assert!(app.action().is_none(), "no sheep confirm can arm from here");
}

#[test]
fn a_read_only_lookout_opens_the_screen_and_refuses_the_edit_key() {
    let mut app = fixtures::app_in_settings(); // Control::ReadOnly
    assert!(app.settings().is_some(), "reading shep.toml is not gated");
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let notice = app.notice().expect("the refusal has to say why");
    assert!(notice.is_grave());
}
```

The existing refusal is `"read-only: actions need --allow-control"` (`app.rs:1033`). Reuse it verbatim rather than writing a second sentence for the same fact.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow:: lookout
```

Expected: FAIL, on missing variants and missing fixtures.

- [ ] **Step 3: Implement**

`input.rs` gains two normal-mode arms: `KeyCode::Char('s') => Some(KeyPress::Settings)` and `KeyCode::Char(' ') => Some(KeyPress::Cycle)`. No third `InputMode`: the keymap emits both unconditionally and the reducer decides, which is the division `KeyPress::Escape`'s own doc already argues for. Extend `every_bound_key_resolves_to_its_press` to cover both.

`app.rs` gains `settings: Option<Settings>` on `App`, initialised `None` in `new`. `on_key` grows a branch ahead of the ordinary dispatch, after the text-mode branch: when `self.settings.is_some()`, route to `on_settings_key`, which owns `Settings`, `Escape`, `SelectUp`/`SelectDown`/`SelectFirst`/`SelectLast`, `Cycle`, `Confirm` and `Refresh`, and ignores everything else. `Quit` still falls through, for the reason the armed-confirm branch already carves it out.

Cursor movement clamps rather than wrapping, matching the flock table. Store the cursor as a `usize` index into `rows()` and resolve it through `cursor()`, so a shorter dog list after a refresh cannot point past the end; clamp on every read.

`mod.rs`'s effect loop gains an arm:

```rust
Effect::LoadSettings => {
    let path = paths.daemon_config.clone();
    let socket_default = paths.socket.clone();
    let style = /* the resolved style this lookout already holds */;
    let result = tokio::task::spawn_blocking(move || {
        commands::settings::load_settings(&path, &socket_default, style)
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|inner| inner.map_err(|err| err.to_string()));
    let _ = app.update(Msg::Settings { result });
    dirty = true;
}
```

`load_settings` takes three arguments, not two: the config path, the socket path a document that declares none falls back to, and the already-resolved style. The style goes by value, since it is a `(StyleLevel, StyleSource)` pair of two `Copy` fields.

`spawn_blocking` even though the read takes no lock: the rule is "no file I/O on the redraw task", which is cheaper to hold than a judgement per call site. Say so in a comment.

What shipped does not `.await` the handle in the arm, as the sample above does. Awaiting it there parks the UI task until the I/O lands, which is the freeze `spawn_blocking` was reached for in the first place; the handle goes into a `FuturesUnordered` the `select!` drains instead. See the commit that fixed it for the full argument.

Add the fixtures `settings_snapshot()`, `app_in_settings()` and `app_in_settings_with_control()` to `view/fixtures.rs`, in the shape the ones already there use.

- [ ] **Step 4: Run them and watch them pass**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

Expected: PASS.

- [ ] **Step 5: Prove each test binds**

Make `Escape` in the settings branch return `Effect::Quit` and confirm that test reddens. Have the open path keep the previous cursor and confirm the reset test reddens. Clear `self.filter` when the screen opens and confirm the survival test reddens. Re-read each patched line before running.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/lookout/
git commit -m "feat(lookout): s opens a settings screen, read-only for now"
```

---

### Task 6: The screen renders

**Files:**
- Create: `crates/shep-cli/src/lookout/view/settings.rs`
- Modify: `crates/shep-cli/src/lookout/view/mod.rs`

**Interfaces:**
- Consumes: task 5's `App::settings`, `Settings::rows`, `Settings::cursor`.
- Produces: `pub fn draw_settings(app: &App, settings: &Settings, area: Rect, buffer: &mut Buffer)`, plus `pub fn columns_for(width: u16) -> &'static [DogColumn]` for the dogs table's drop tiers.

- [ ] **Step 1: Write the failing test**

```rust
/// The whole screen at a comfortable width. The snapshot is the assertion:
/// it pins the section order, the SOURCE column's wording and the fact that
/// a fresh home reads `the default` all the way down.
#[test]
fn settings_at_a_comfortable_width() {
    let app = fixtures::app_in_settings();
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| super::draw(&app, frame)).unwrap();
    insta::assert_snapshot!(render_text(terminal.backend().buffer()));
}

/// SOURCE is the widest column and the first to go, the same reasoning
/// `flock::TIERS` gives for SMIT.
#[test]
fn the_dogs_source_column_drops_before_the_rest() {
    assert!(columns_for(120).contains(&DogColumn::Source));
    assert!(!columns_for(60).contains(&DogColumn::Source));
    assert!(
        columns_for(60).contains(&DogColumn::Running),
        "RUNNING is the diagnostic half and outlives SOURCE"
    );
}
```

- [ ] **Step 2: Run and watch it fail**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow:: settings
```

Expected: FAIL, module not found. The snapshot test will want `cargo insta accept` once the render is right; read the frame before accepting it.

- [ ] **Step 3: Implement**

`view/mod.rs`'s `draw` keeps the title line and the status bar and branches on `app.settings()`: `Some` draws the screen into the body, `None` draws today's dashboard. The `MIN_TERM_WIDTH`/`MIN_HEIGHT` refusal stays ahead of the branch and covers both.

`settings.rs` renders, in order: `[daemon]` with its four rows, `[whistle]` with one, `[style]` with one, then `[dogs]` with its caption and table. Each scalar row is name, value, source, and the apply cost. Section headers use the palette's muted style; the cursor row is marked `>` exactly as the flock table marks its selection.

The dogs caption is `space arms, Enter applies; a dog needs no reload`. Do not write a caption that says space applies: this screen arms and confirms like every other action in lookout.

This task renders the dogs table from `SettingsSnapshot` alone: NAME, IN FILE and SOURCE. The RUNNING column is the join against the live flock and belongs to task 9, which declares `dog_rows` for it. Leave the column out here rather than rendering an empty one.

Model the drop tiers on `view/flock.rs`'s `TIERS` and `columns_for`, including the doc explaining the drop order. SOURCE goes first because it is the widest and an adopted path can be long; then IN FILE, because RUNNING is the half that answers "is it up".

Values render defaults resolved, never blank. `socket` shows the resolved path.

- [ ] **Step 4: Run and watch it pass**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

Expected: PASS.

- [ ] **Step 5: Prove the test binds**

Make `load_settings`'s fixture report `Config` for every field and confirm the snapshot reddens with `the default` replaced. Swap two tiers in `columns_for` and confirm the tier test reddens. Re-read each patched line before running.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/lookout/view/
git commit -m "feat(lookout): the settings screen renders, with a source per scalar"
```

---

### Task 7: Editing the four closed scalars

`space` arms a candidate and re-arms on each press; `Enter` applies. The confirm names the field's own apply cost, and for `[daemon]` it names the two layers lookout cannot see.

**Files:**
- Modify: `crates/shep-cli/src/lookout/app.rs`
- Modify: `crates/shep-cli/src/lookout/view/settings.rs`
- Modify: `crates/shep-cli/src/lookout/mod.rs`

**Interfaces:**
- Consumes: task 3's `SettingEdit`, `apply_setting`; task 5's `Settings`.
- Produces:
  ```rust
  pub enum Msg { /* ... */ SettingWritten { edit: SettingEdit, result: Result<(), String> } }
  pub enum Effect { /* ... */ WriteSetting(SettingEdit) }

  impl Settings {
      /// The armed candidate and its prompt, or `None`.
      pub fn pending(&self) -> Option<SettingsPrompt<'_>>;
  }

  pub struct SettingsPrompt<'a> {
      pub text: &'a str,
      /// False while it is a question, true once it has gone out.
      pub sent: bool,
  }
  ```

**Confirm strings, verbatim.** These have already been through the project's voice review; use them as written and do not paraphrase.

| Field | Prompt |
| --- | --- |
| `log_level` | `set log_level to debug? needs shep daemon reload, and will not apply if the shepherd was booted with SHEP_LOG_LEVEL or --log-level` |
| `log_json` | `set log_json to true? needs shep daemon reload, and will not apply if the shepherd was booted with SHEP_LOG_JSON or --log-json` |
| `allow_control` | `turn whistle control tools on? needs shep whistle restarted` |
| `style level` | `set style level to plain? the next command reads it` |

The value in each is the candidate, so `debug`, `true` and `plain` above are examples rather than constants.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn space_arms_a_candidate_without_changing_the_row() {
    let mut app = fixtures::app_in_settings_with_control();
    let before = app.settings().unwrap().snapshot().log_level.value.clone();

    assert_eq!(app.update(Msg::Key(KeyPress::Cycle)), Effect::None);

    assert_eq!(
        app.settings().unwrap().snapshot().log_level.value,
        before,
        "arming is a question, so the row still shows what the file says"
    );
    assert!(app.settings().unwrap().pending().is_some());
}

/// Six log levels and one cycle key. Without re-arming, the fourth is
/// unreachable without cancelling in between.
#[test]
fn space_advances_the_candidate_rather_than_needing_a_cancel() {
    let mut app = fixtures::app_in_settings_with_control();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let first = app.settings().unwrap().pending().unwrap().text.to_string();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let second = app.settings().unwrap().pending().unwrap().text.to_string();
    assert_ne!(first, second);
}

#[test]
fn the_daemon_confirm_names_both_layers_lookout_cannot_see() {
    let mut app = fixtures::app_in_settings_with_control();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let text = app.settings().unwrap().pending().unwrap().text.to_string();
    assert!(text.contains("shep daemon reload"), "got: {text}");
    assert!(text.contains("SHEP_LOG_LEVEL"), "got: {text}");
    assert!(text.contains("--log-level"), "got: {text}");
}

#[test]
fn the_whistle_confirm_names_a_whistle_restart_and_not_a_reload() {
    let mut app = fixtures::app_in_settings_on(SettingField::AllowControl);
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let text = app.settings().unwrap().pending().unwrap().text.to_string();
    assert!(text.contains("shep whistle restarted"), "got: {text}");
    assert!(!text.contains("daemon reload"), "a whistle key needs no reload: {text}");
}

#[test]
fn the_style_confirm_promises_nothing_beyond_the_next_command() {
    let mut app = fixtures::app_in_settings_on(SettingField::StyleLevel);
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let text = app.settings().unwrap().pending().unwrap().text.to_string();
    assert!(text.contains("the next command reads it"), "got: {text}");
}

#[test]
fn enter_sends_the_armed_edit() {
    let mut app = fixtures::app_in_settings_with_control();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let effect = app.update(Msg::Key(KeyPress::Confirm));
    assert!(matches!(effect, Effect::WriteSetting(SettingEdit::Set { field: SettingField::LogLevel, .. })));
    assert!(app.settings().unwrap().pending().unwrap().sent);
}

#[test]
fn a_written_edit_updates_the_row_and_its_source() {
    let mut app = fixtures::app_in_settings_with_control();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
        panic!("Enter must send");
    };
    let _ = app.update(Msg::SettingWritten { edit, result: Ok(()) });

    assert_eq!(app.settings().unwrap().snapshot().log_level.source, StyleSource::Config);
    assert!(app.settings().unwrap().pending().is_none());
}

#[test]
fn a_refused_write_says_why_and_leaves_the_row_alone() {
    let mut app = fixtures::app_in_settings_with_control();
    let before = app.settings().unwrap().snapshot().log_level.clone();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
        panic!("Enter must send");
    };
    let _ = app.update(Msg::SettingWritten {
        edit,
        result: Err("max_cron_sleep is 500ms, below the 1s floor".into()),
    });

    assert_eq!(app.settings().unwrap().snapshot().log_level, before);
    let notice = app.notice().unwrap();
    assert!(notice.is_grave());
    assert!(notice.to_string().contains("below the 1s floor"));
}

/// The divergence from the sheep confirm, which `disarm_on_link_change`
/// clears. A settings edit is local file I/O over a file that is not stale.
#[test]
fn a_lost_link_leaves_a_scalar_confirm_armed() {
    let mut app = fixtures::app_in_settings_with_control();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let _ = app.update(Msg::Frozen { at_local: "12:00:00".into() });
    assert!(
        app.settings().unwrap().pending().is_some(),
        "a scalar never leaves the machine, so a dead shepherd is irrelevant to it"
    );
}

/// And it still expires, off the raw tick rather than `self.now`, which
/// stops advancing once the link is lost.
#[test]
fn a_settings_confirm_expires_on_a_frozen_dashboard() {
    let (mut app, start) = fixtures::app_in_settings_at();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let _ = app.update(Msg::Frozen { at_local: "12:00:00".into() });
    let _ = app.update(Msg::Tick { now: start + CONFIRM_EXPIRY });
    assert!(app.settings().unwrap().pending().is_none());
}

#[test]
fn escape_cancels_the_confirm_before_it_closes_the_screen() {
    let mut app = fixtures::app_in_settings_with_control();
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let _ = app.update(Msg::Key(KeyPress::Escape));
    assert!(app.settings().unwrap().pending().is_none());
    assert!(app.settings().is_some(), "the first Esc cancels, it does not close");
    let _ = app.update(Msg::Key(KeyPress::Escape));
    assert!(app.settings().is_none());
}
```

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Implement**

Add `Pending` inside `Settings`, as one field rather than several `Option`s so that typing, armed and sent cannot overlap:

```rust
enum Pending {
    /// Only `Socket` and `MaxCronSleep` reach this. Task 8.
    Typing { field: SettingField, buffer: String },
    Armed { edit: SettingEdit, at: Instant },
    Sent { edit: SettingEdit },
}
```

`Cycle` on a scalar row builds the next candidate and stores `Armed`, resetting `at` each press. `Confirm` on `Armed` moves to `Sent` and returns `Effect::WriteSetting`. `Msg::SettingWritten` clears `Pending` and, on `Ok`, updates that field's `ScalarView` in place to the written value with `StyleSource::Config`; on `Err` it leaves the view alone and raises a grave `Notice`.

Expiry: extend the `Msg::Tick` arm. The existing sheep-action expiry sits inside `if !matches!(self.link, Link::Lost { .. })`. The settings expiry goes **outside** that guard and compares against the tick's own `now`, not `self.now`. Comment why: the sheep confirm freezes because everything it describes is stale, and a settings edit describes a file that is not.

`mod.rs` gains the write arm, mirroring task 5's:

```rust
Effect::WriteSetting(edit) => {
    let path = paths.daemon_config.clone();
    let for_msg = edit.clone();
    let result = tokio::task::spawn_blocking(move || {
        commands::settings::apply_setting(&path, &edit)
    })
    .await
    .map_err(|err| err.to_string())
    .and_then(|inner| inner.map_err(|err| err.to_string()));
    let _ = app.update(Msg::SettingWritten { edit: for_msg, result });
    dirty = true;
}
```

`spawn_blocking` is load-bearing here rather than a convention: `ConfigLock::acquire` blocks with no deadline, so a concurrent `shep adopt` on the UI task would freeze the redraw, the tick and the bus drain together. Say that in the comment.

The view grows a prompt line under the table, in the shape the status bar already uses for a sheep confirm.

- [ ] **Step 4: Run them and watch them pass**

- [ ] **Step 5: Prove each test binds**

Move the settings expiry inside the link guard and confirm the frozen-expiry test reddens. Have `Cycle` mutate the row rather than arm and confirm the first test reddens. Drop `SHEP_LOG_LEVEL` from the daemon prompt and confirm that test reddens. Re-read each patched line before running.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/lookout/
git commit -m "feat(lookout): the four closed scalars arm, confirm and write"
```

---

### Task 8: The text editor, and unsetting the two optional fields

**Files:**
- Modify: `crates/shep-cli/src/lookout/app.rs`
- Modify: `crates/shep-cli/src/lookout/view/settings.rs`

**Interfaces:**
- Consumes: task 7's `Pending`, task 1's `KeyPress::Text*`.
- Produces: `Settings::typing() -> Option<(&SettingField, &str)>` for the view.

**Confirm strings, verbatim:**

| Edit | Prompt |
| --- | --- |
| set `socket` | `set socket to /run/shep/shep.sock? needs the shepherd stopped and started; a reload will not move it, and it will not apply if the shepherd was booted with SHEP_SOCKET or --socket` |
| set `max_cron_sleep` | `set max_cron_sleep to 30s? needs shep daemon reload, and will not apply if the shepherd was booted with SHEP_MAX_CRON_SLEEP or --max-cron-sleep` |
| unset `socket` | `unset socket? it goes back to the default under $SHEP_HOME, and needs the shepherd stopped and started` |
| unset `max_cron_sleep` | `unset max_cron_sleep? it goes back to the daemon's own default, and needs shep daemon reload` |

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn enter_on_a_text_row_opens_the_editor_seeded_with_the_current_value() {
    let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
    let _ = app.update(Msg::Key(KeyPress::Confirm));
    let (field, buffer) = app.settings().unwrap().typing().expect("the editor opens");
    assert_eq!(*field, SettingField::MaxCronSleep);
    assert_eq!(buffer, "30s");
}

#[test]
fn typing_then_enter_arms_rather_than_writing() {
    let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
    let _ = app.update(Msg::Key(KeyPress::Confirm));
    for _ in 0..3 {
        let _ = app.update(Msg::Key(KeyPress::TextBackspace));
    }
    for c in "45s".chars() {
        let _ = app.update(Msg::Key(KeyPress::TextChar(c)));
    }
    assert_eq!(app.update(Msg::Key(KeyPress::TextApply)), Effect::None);
    let prompt = app.settings().unwrap().pending().unwrap();
    assert!(!prompt.sent, "the editor arms; a second Enter is what sends");
    assert!(prompt.text.contains("45s"), "got: {}", prompt.text);
    assert!(prompt.text.contains("SHEP_MAX_CRON_SLEEP"), "got: {}", prompt.text);
}

#[test]
fn an_empty_editor_arms_an_unset() {
    let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
    let _ = app.update(Msg::Key(KeyPress::Confirm));
    for _ in 0..8 {
        let _ = app.update(Msg::Key(KeyPress::TextBackspace));
    }
    let _ = app.update(Msg::Key(KeyPress::TextApply));
    let text = app.settings().unwrap().pending().unwrap().text.to_string();
    assert!(text.starts_with("unset max_cron_sleep?"), "got: {text}");
}

#[test]
fn the_socket_confirm_rules_out_the_reload_it_would_otherwise_imply() {
    let mut app = fixtures::app_in_settings_on(SettingField::Socket);
    let _ = app.update(Msg::Key(KeyPress::Confirm));
    let _ = app.update(Msg::Key(KeyPress::TextApply));
    let text = app.settings().unwrap().pending().unwrap().text.to_string();
    assert!(text.contains("stopped and started"), "got: {text}");
    assert!(text.contains("a reload will not move it"), "got: {text}");
}

/// A refusal is discovered under the lock, so it lands after the confirm.
/// The typed text has to survive it, or the operator retypes a path to fix
/// one character.
#[test]
fn a_refused_write_reopens_the_editor_with_the_text_intact() {
    let mut app = fixtures::app_in_settings_on(SettingField::MaxCronSleep);
    let _ = app.update(Msg::Key(KeyPress::Confirm));
    for _ in 0..3 {
        let _ = app.update(Msg::Key(KeyPress::TextBackspace));
    }
    for c in "500ms".chars() {
        let _ = app.update(Msg::Key(KeyPress::TextChar(c)));
    }
    let _ = app.update(Msg::Key(KeyPress::TextApply));
    let Effect::WriteSetting(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
        panic!("Enter must send");
    };
    let _ = app.update(Msg::SettingWritten {
        edit,
        result: Err("max_cron_sleep is 500ms, below the 1s floor".into()),
    });

    let (_, buffer) = app.settings().unwrap().typing().expect("the editor reopens");
    assert_eq!(buffer, "500ms");
    assert!(app.notice().unwrap().to_string().contains("below the 1s floor"));
}

#[test]
fn escape_abandons_the_editor_and_keeps_the_screen_open() {
    let mut app = fixtures::app_in_settings_on(SettingField::Socket);
    let _ = app.update(Msg::Key(KeyPress::Confirm));
    let _ = app.update(Msg::Key(KeyPress::TextAbandon));
    assert!(app.settings().unwrap().typing().is_none());
    assert!(app.settings().is_some());
}

#[test]
fn a_closed_scalar_has_no_editor() {
    let mut app = fixtures::app_in_settings_with_control(); // on log_level
    let _ = app.update(Msg::Key(KeyPress::Confirm));
    assert!(
        app.settings().unwrap().typing().is_none(),
        "log_level is a cycle, not a text field"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Implement**

`Confirm` on a `Socket` or `MaxCronSleep` row with no `Pending` sets `Pending::Typing` seeded with the row's current value and sets `self.mode = InputMode::Text`. The text keys route through the existing text branch in `on_key`, which now has two destinations: the filter box when `settings` is `None`, the editor when it is `Some`. That is the reducer deciding, which is why task 1 renamed the variants.

`TextApply` on an empty buffer builds `SettingEdit::Unset`; on a non-empty one, `SettingEdit::Set`. Either way it moves `Pending` to `Armed` and leaves `InputMode::Normal`.

`Msg::SettingWritten` with an `Err` whose edit named a text field returns `Pending` to `Typing` with the buffer the edit carried, and sets `InputMode::Text` again.

Do not trim the buffer. This repository does not widen an accepted input grammar without a basis in the spec, and the filter box already carries that rule explicitly.

- [ ] **Step 4: Run them and watch them pass**

- [ ] **Step 5: Prove each test binds**

Have the refusal clear the buffer and confirm the reopen test reddens. Make `TextApply` on an empty buffer arm a `Set` with an empty string and confirm the unset test reddens. Re-read each patched line before running.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/lookout/
git commit -m "feat(lookout): socket and max_cron_sleep get an editor, and can be unset"
```

---

### Task 9: The dogs section, and the two-step toggle

The file half and the daemon half are two acts. File first, then the request, which is `shep enable`'s own order.

**Files:**
- Modify: `crates/shep-cli/src/lookout/app.rs`
- Modify: `crates/shep-cli/src/lookout/view/settings.rs`
- Modify: `crates/shep-cli/src/lookout/mod.rs`

**Interfaces:**
- Consumes: task 4's `enable_in_config`/`disable_in_config`; task 6's `DogColumn`.
- Produces:
  ```rust
  pub enum Sent {
      /* ... */
      /// One dog's daemon half, after its file half landed. `source` is what
      /// the write returned, so the request cannot disagree with the file.
      Dog { name: String, enable: bool, source: DogSource },
  }
  ```
  `Sent::request()` gains its two arms: `EnableDog { name, source }` and `DisableDog { name }`. `PROTOCOL_VERSION` does not move; both requests already exist.

  Also produced, in `view/settings.rs`:
  ```rust
  /// One row of the dogs table, with the file and the live flock joined by
  /// name. Declared here rather than in task 6 because the join is this
  /// task's whole subject: task 6 renders IN FILE and SOURCE from the
  /// snapshot alone, and RUNNING is what this adds.
  pub struct DogRow {
      pub name: String,
      pub enabled: bool,
      /// The word the flock table would show, or `None` when no dog of this
      /// name is running.
      pub running: Option<String>,
      pub adopted_path: Option<PathBuf>,
  }

  pub fn dog_rows(app: &App, width: u16) -> Vec<DogRow>;
  ```

**Confirm strings, verbatim:**

| Edit | Prompt |
| --- | --- |
| enable | `enable metrics? it starts now, no reload` |
| disable | `disable otel? it stops now and is deregistered` |

`DisableDog`'s own doc is "Stop and deregister one dog", answering `Response::Deleted`. The prompt says deregister out loud rather than letting an operator find out afterwards.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn the_dogs_table_joins_the_file_against_the_running_flock() {
    // `otel` runs while the file has it disabled, which is what "a removed
    // name keeps running" looks like from the outside. `ledger` is enabled
    // and absent, which is a dog that failed to start.
    let app = fixtures::app_in_settings_with_dog_drift();
    let rows = view::settings::dog_rows(&app, 120);

    let otel = rows.iter().find(|r| r.name == "otel").unwrap();
    assert!(!otel.enabled);
    assert_eq!(otel.running.as_deref(), Some("online"));

    let ledger = rows.iter().find(|r| r.name == "ledger").unwrap();
    assert!(ledger.enabled);
    assert_eq!(ledger.running, None);
}

/// Phase 3b: a dog that never completed a handshake reads `silent`, not
/// `online`, and this table must not undo that.
#[test]
fn a_dog_that_never_handshook_reads_silent_here_too() {
    let app = fixtures::app_in_settings_with_silent_dog();
    let rows = view::settings::dog_rows(&app, 120);
    let bark = rows.iter().find(|r| r.name == "bark").unwrap();
    assert_eq!(bark.running.as_deref(), Some("silent"));
}

#[test]
fn arming_a_dog_names_the_live_apply_and_not_a_reload() {
    let mut app = fixtures::app_in_settings_on_dog("metrics");
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let text = app.settings().unwrap().pending().unwrap().text.to_string();
    assert!(text.contains("it starts now, no reload"), "got: {text}");
}

#[test]
fn disabling_says_it_deregisters() {
    let mut app = fixtures::app_in_settings_on_enabled_dog("otel");
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let text = app.settings().unwrap().pending().unwrap().text.to_string();
    assert!(text.contains("deregistered"), "got: {text}");
}

/// The chain: the write lands, and the reducer raises the daemon half. One
/// message still yields one effect.
#[test]
fn a_written_dog_toggle_raises_the_daemon_half() {
    let mut app = fixtures::app_in_settings_on_dog("metrics");
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let Effect::WriteDog(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
        panic!("Enter must send the file half first");
    };
    let effect = app.update(Msg::DogWritten {
        edit,
        result: Ok(DogSource::BuiltIn),
    });
    assert!(matches!(
        effect,
        Effect::Send(Sent::Dog { enable: true, .. })
    ));
}

#[test]
fn a_refused_file_half_never_reaches_the_shepherd() {
    let mut app = fixtures::app_in_settings_on_dog("metrics");
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    let Effect::WriteDog(edit) = app.update(Msg::Key(KeyPress::Confirm)) else {
        panic!("Enter must send the file half first");
    };
    let effect = app.update(Msg::DogWritten {
        edit,
        result: Err("permission denied".into()),
    });
    assert_eq!(effect, Effect::None, "a failed write must not ask the shepherd");
    assert!(app.notice().unwrap().is_grave());
}

/// The scalars never leave the machine; a dog's second half does.
#[test]
fn a_dog_toggle_refuses_while_the_link_is_gone() {
    let mut app = fixtures::app_in_settings_on_dog("metrics");
    let _ = app.update(Msg::Frozen { at_local: "12:00:00".into() });
    let effect = app.update(Msg::Key(KeyPress::Cycle));
    assert_eq!(effect, Effect::None);
    assert!(app.settings().unwrap().pending().is_none(), "nothing arms");
    assert!(app.notice().unwrap().is_grave());
}

#[test]
fn a_scalar_still_edits_while_the_link_is_gone() {
    let mut app = fixtures::app_in_settings_with_control(); // on log_level
    let _ = app.update(Msg::Frozen { at_local: "12:00:00".into() });
    let _ = app.update(Msg::Key(KeyPress::Cycle));
    assert!(
        app.settings().unwrap().pending().is_some(),
        "a scalar is local file I/O and needs no shepherd"
    );
}
```

The dog toggle gets its own `Effect::WriteDog` and `Msg::DogWritten` rather than reusing task 7's `WriteSetting`, because the two carry different payloads and different next steps: a scalar write ends in a notice, a dog write ends in a request. Grep `Sent` and `DogSource` for their real shapes before writing the chain.

- [ ] **Step 2: Run them and watch them fail**

- [ ] **Step 3: Implement**

The join: `App::all_rows` already carries every running dog, and `ProcessInfo.dog` is `Some` for one. The word comes from `Reported::of(status, handshook).word()` (`vocabulary.rs:117`), which is the one place that turns an `Online` status with `handshook: Some(false)` into `silent`. Call it. Writing a second mapping here would be a second opinion about the wire contract's own vocabulary.

`Cycle` on a dog row checks the link first and refuses with the existing `LINK_GONE` sentence when it is `Lost`, then arms. `Confirm` returns `Effect::WriteDog`. `Msg::DogWritten` with `Ok(source)` returns `Effect::Send(Sent::Dog { .. })`; with `Err` it raises a grave notice and returns `Effect::None`.

`mod.rs` gains the `WriteDog` arm, on `spawn_blocking`, calling `dogs::enable_in_config` or `dogs::disable_in_config`. The `Effect::Send` arm needs no change: `Sent::Dog` rides the existing request channel.

The RUNNING column repairs itself: the next `ListFlock` two seconds later carries the new state, so nothing here has to poke the snapshot.

- [ ] **Step 4: Run them and watch them pass**

- [ ] **Step 5: Prove each test binds**

Have `Msg::DogWritten`'s `Err` arm still return `Effect::Send` and confirm that test reddens. Drop the link check from the dog arm and confirm the refusal test reddens. Map a non-handshook dog to `online` and confirm the silent test reddens. Re-read each patched line before running.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/lookout/
git commit -m "feat(lookout): per-dog toggles, applying live through the shepherd"
```

---

### Task 10: Gallery frames

**Files:**
- Modify: `crates/shep-cli/src/lookout/frames.rs`
- Modify: `docs/lookout/frames.txt` (regenerated, not hand-edited)
- Modify: `docs/lookout/frames.ansi` (same)

- [ ] **Step 1: Add the scenes**

Five, appended to `Scene`, to `Scene::ALL`, and to `label`, `caption` and `size`:

| Scene | Shows |
| --- | --- |
| `SettingsFresh` | a `shep.toml` holding only `[interpreters]`, so every scalar reads `the default`. The state most operators open the screen in. |
| `SettingsSet` | some scalars declared, so `shep.toml` and `the default` sit side by side and the difference is visible. |
| `SettingsConfirm` | a `[daemon]` confirm armed, naming the variable and the flag it cannot see. |
| `SettingsTyping` | the `socket` editor open mid-path. |
| `SettingsDogs` | the dogs table with the drift: one running while disabled, one enabled and absent, one silent. |

`SettingsReadOnly` is deliberately not a sixth: `Refused` already covers the read-only refusal for the dashboard, and the settings screen's own read-only state is one caption line different. Add it only if the caption turns out to carry something the others do not.

- [ ] **Step 2: Run the suite and read every new snapshot before accepting it**

```bash
cargo test --workspace --lib --bins --all-features -- --skip ::slow::
```

Then `cargo insta accept` only after reading each pending snapshot. A snapshot accepted unread is a test that pins whatever the bug was.

- [ ] **Step 3: Regenerate the gallery**

```bash
cargo test -p shep --lib --all-features -- --ignored write_the_gallery
```

Then read the diff on `docs/lookout/frames.txt`. Extend its header prose to mention the settings screen, in the register the file already uses.

- [ ] **Step 4: Commit**

```bash
git add crates/shep-cli/src/lookout/frames.rs crates/shep-cli/src/lookout/snapshots/ docs/lookout/
git commit -m "docs(lookout): gallery frames for the settings screen"
```

---

### Task 11: Docs, and the gate

**Files:**
- Modify: `web/src/pages/docs/lookout.astro`
- Modify: `docs/specs/deferred.md` if it carries a line this closes
- Possibly modify: `web/src/pages/docs/cli/*` (generated, never hand-edited)

- [ ] **Step 1: Write the docs**

`lookout.astro` gains the `s` key in its key table and a section on the screen. It must state four things an operator cannot infer:

1. Reading is not gated; editing is.
2. `the default` against `shep.toml` in the SOURCE column, and what the difference means.
3. That a `[daemon]` edit can be shadowed by the shepherd's own `SHEP_*` env or boot flags, which lookout cannot see, and that `[style]` is the exception because those layers are lookout's own.
4. That `log_level`, `log_json` and `allow_control` can move from `the default` to `shep.toml` and not back from this screen, because `socket` and `max_cron_sleep` are the only optional ones (spec decision 5).

Run the project's voice review over the prose before committing it. Match the page's existing register rather than inventing one.

- [ ] **Step 2: Regenerate the CLI reference and check the diff**

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```

`git diff` afterwards is the check. This feature adds no flag and no verb, so an empty diff here is the expected result rather than a failure. A non-empty one means something drifted earlier and should be committed with a note saying so.

- [ ] **Step 3: Build and check the site**

```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

Both. `check` is the one CI does not run and the one that catches a component given a prop it does not have.

- [ ] **Step 4: The task gate**

Each from its own command, with `$?` captured directly and never through a pipe: in zsh a pipeline's `$?` belongs to the last command and `${PIPESTATUS[0]}` is empty.

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

- [ ] **Step 5: Commit**

```bash
git add web/ docs/
git commit -m "docs(web): the lookout settings screen, and what a change there costs"
```

---

## Self-review

Run against the spec before dispatching task 1.

- **Spec coverage.** Decision 1 is task 6 (`draw` branches rather than adding a pane tier). Decision 2 is task 7. Decision 3 is tasks 3 and 8. Decision 4 is tasks 2, 3 and 6. Decision 5 is tasks 2, 3 and 8. Decision 6 is tasks 7, 8 and 9's confirm strings. Decision 7 is task 11's docs, since lookout only names the command. Decision 8 is task 5's read-only test. Decision 9 is tasks 6 and 9. Decision 10 is task 4. Decision 11 is a note at the call sites, checked in review rather than by a test, because there is nothing to redact.
- **Types.** `SettingField`, `SettingEdit`, `SettingsSnapshot`, `ScalarView`, `DogView` and `SettingError` are defined in task 3 and used unchanged in 5 through 9. `SettingsRow` and `Settings` are task 5's. `Pending` is task 7's, extended by task 8. `Effect::WriteDog`, `Msg::DogWritten` and `Sent::Dog` are task 9's.
- **Loose ends, all closed before dispatch.** `resolve_style` returns `(StyleLevel, StyleSource)` at `lib.rs:533`; `SettingSource` is not declared at all; `StyleSource` already has the four variants; `DogSource` is `BuiltIn | Adopted { path: String }` at `request.rs:587`; the read-only refusal is `"read-only: actions need --allow-control"` at `app.rs:1033`; and `silent` comes from `Reported::of(..).word()` at `vocabulary.rs:117`. Nothing in this plan is left for an implementer to guess at.
- **`ino()` needs `std::os::unix::fs::MetadataExt`** in task 3's test module, and the import needs its own `#[cfg(unix)]`. `mod commands;` carries no gate at all: each site that reaches for a unix API gates itself. Assuming otherwise is what broke the Windows leg of CI on this branch.
