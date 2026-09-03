# Config overrides, slice 1: the apply engine

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shep start <file>` merges a Flockfile into a running flock additively, applies the fields that can change without disruption, parks the rest as pending, and shows the operator both.

**Architecture:** The Flockfile becomes a project template. A daemon-owned override store holds what the operator has changed since. A file load merges the two by key set rather than by value, so no three-way base is needed. Fields route by how the daemon reads them: some apply to the stored spec immediately, some reach the next spawn, and some park in a pending slot that `shep reload` promotes.

**Tech Stack:** Rust, edition 2024, MSRV 1.88. `serde`, `serde_json`, `tokio`, `tempfile`, `clap`. No new dependencies.

**Spec:** [docs/brainstorming/specs/2026-09-02-config-overrides-design.md](../../brainstorming/specs/2026-09-02-config-overrides-design.md)

## Global Constraints

- **Invoke the `shep-idiomatic-rust` skill before writing any Rust here.** Cite rules as `IR-<n>` in review.
- **Inner loop:** `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`. One cargo shape per task. Do not alternate `--workspace` and `-p`; the workspace shares one target-dir lock and switching shapes invalidates crates whose feature set changed.
- **For `shep`-crate tasks:** `cargo test -p shep --lib --bins --all-features -- --skip ::slow::`. The package is `shep`, not `shep-cli`; `-p shep-cli` runs zero tests and exits 0.
- **Every test's `await` needs a forcing mechanism**, not a hope: a timeout, a paused clock, or an explicit transition (IR-46).
- **Prove every test non-vacuous**: mutate what it protects, watch that specific test go red, restore. State in the commit that you did.
- **No em dashes or en dashes anywhere**, including doc comments and commit messages.
- **Never write an absolute home-directory path** into any committed file or commit message. Repo-relative only.
- **Every new public item needs docs and a deliberate `Debug` decision.** Anything carrying env or secrets gets a redacted `Debug` with an exact-string test (IR-41).
- `PROTOCOL_VERSION` **stays 2**. `SCHEMA_VERSION` **stays 1**. Neither moves in this plan.
- **Plan snippets describing existing code are the plan author's reading, not ground truth.** Where a step quotes an existing signature, grep for it and use what you find. Where they disagree, the code wins and you say so in your report.

---

## Task 1: Redact `SpawnSpec`'s Debug

**Files:**
- Modify: `crates/shep-daemon/src/runner.rs` (the `SpawnSpec` declaration, around line 1017)
- Test: same file, its existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces: nothing other tasks call. This is a standalone hardening fix that ships first because later tasks make env editable.

`SpawnSpec` sits on the exec boundary at `command.envs(&spec.env)` and derives a plain `Debug` over an unredacted `pub env: BTreeMap<String, String>`. Four sibling types that carry env already have a redacted `Debug` with an exact-string test: `AppConfig` (`crates/shep-core/src/config/app.rs:437`), `SavedApp`, `CarriedSheep`, `OsProber`. Copy `AppConfig`'s shape.

- [ ] **Step 1: Read the existing precedent**

Read `crates/shep-core/src/config/app.rs` around lines 437 to 444 for the manual `Debug`, and lines 681 to 691 for the exact-string test. Match both shapes.

- [ ] **Step 2: Write the failing test**

Add to `runner.rs`'s `mod tests`. Build a `SpawnSpec` however the surrounding tests already build one (grep for `SpawnSpec {` in that module and copy a constructor call, adding two env vars):

```rust
/// fails if `SpawnSpec`'s Debug ever prints an env VALUE. This type sits on
/// the exec boundary, so a `tracing` call that formats it would put every
/// secret an operator configured into the daemon's log (IR-41).
#[test]
fn debug_redacts_env_values() {
    let mut spec = /* the module's existing SpawnSpec constructor */;
    spec.env.insert("DATABASE_URL".to_string(), "postgres://user:hunter2@db".to_string());
    spec.env.insert("API_KEY".to_string(), "sk-live-abc".to_string());

    let rendered = format!("{spec:?}");
    assert!(!rendered.contains("hunter2"), "env value leaked: {rendered}");
    assert!(!rendered.contains("sk-live-abc"), "env value leaked: {rendered}");
    assert!(rendered.contains("<2 vars>"), "expected a redacted count: {rendered}");
}
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test -p shep-daemon --lib --all-features debug_redacts_env_values`
Expected: FAIL, the assertion on `hunter2` fires because the derived `Debug` prints the map.

- [ ] **Step 4: Replace the derive with a manual impl**

Remove `Debug` from `SpawnSpec`'s `#[derive(...)]` list, leaving the others. Add:

```rust
/// Redacted: `env` carries whatever the operator configured, and this type is
/// the one handed to `Command::envs` at exec. Every other env-carrying type in
/// the workspace redacts (IR-41), and this was the one that did not.
impl fmt::Debug for SpawnSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnSpec")
            .field("name", &self.name)
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .finish_non_exhaustive()
    }
}
```

Adjust the named fields to whatever `SpawnSpec` actually has. `finish_non_exhaustive` prints `..`, matching `AppConfig`'s own rendering. Add `use core::fmt;` if the module lacks it.

- [ ] **Step 5: Run the test and the module's suite**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`
Expected: PASS, including any existing test that formatted a `SpawnSpec`. If one asserted on the old derived output, update it and say so.

- [ ] **Step 6: Prove it non-vacuous**

Restore the `Debug` derive, run the new test alone, confirm it fails, then put the manual impl back.

- [ ] **Step 7: Commit**

```bash
git add crates/shep-daemon/src/runner.rs
git commit -m "fix(daemon): redact SpawnSpec's Debug, the one env-carrying type without it"
```

---

## Task 2: Validate `shep.toml` before a daemon reload acts

**Files:**
- Modify: `crates/shep-cli/src/commands/daemon.rs` (`reload_with_wait`, around line 586)
- Test: same file's `mod tests`, plus one end-to-end case in `crates/shep-cli/tests/cli_e2e.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: nothing other tasks call. Independent hardening, ships early.

`reload_with_wait` asks the daemon whether its flock can be carried and never asks whether `shep.toml` parses. The successor `execve`s, loads config, and on a semantic error `daemon_exit_code` maps `DaemonRunError::Config` to `ExitCode::InvalidConfig` and exits. The predecessor is already gone, so the flock keeps running with nothing supervising it.

`toml_edit` catches syntax, so `ShepToml::edit` cannot write an unparseable file. It does not catch a valid-TOML bad value such as `log_level = "verbose"` or `max_cron_sleep = "soon"`.

The precedent for the check is `crates/shep-cli/src/whistle/mod.rs:171`, which already runs `DaemonConfig::load` as a pre-flight. Read it before writing this.

- [ ] **Step 1: Write the failing unit test**

In `daemon.rs`'s `mod tests`. Grep the module for how existing tests build a `ShepPaths` over a `TempDir` and copy that.

```rust
/// fails if `reload` signals anything on a shep.toml that will not load. The
/// successor execs into a fresh boot_supervisor, so a bad value there exits
/// the daemon AFTER the predecessor is gone, leaving a running flock with no
/// shepherd. The value below is valid TOML and an invalid level.
#[tokio::test]
async fn reload_refuses_a_shep_toml_that_will_not_load() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("shep.toml"), "[daemon]\nlog_level = \"verbose\"\n").unwrap();
    let paths = /* the module's existing paths-over-tempdir helper */;

    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut streams = /* the module's existing Streams constructor over out/err */;

    let code = reload(&mut streams, &paths, crate::VersionGuard::default()).await;

    assert_eq!(code, ExitCode::InvalidConfig);
    let rendered = String::from_utf8(err).unwrap();
    assert!(rendered.contains("verbose"), "the refusal must name the bad value: {rendered}");
}
```

There is no daemon running in this test, so without the pre-flight `reload` reaches `Client::connect`, fails, and returns `DaemonUnreachable`. That is the failure you should see: the assertion on `InvalidConfig` fires.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p shep --lib --all-features reload_refuses_a_shep_toml`
Expected: FAIL with `InvalidConfig` versus `DaemonUnreachable`, which is the point: today nothing checks the config before dialling.

- [ ] **Step 3: Add the pre-flight**

At the very top of `reload_with_wait`, before `Client::connect`:

```rust
    // Before the connection, not after, and before either arm. The handover
    // arm `execve`s a successor that re-reads this file through
    // `boot_supervisor`; a value that fails to load there exits the successor
    // with the predecessor already gone, leaving the flock running with
    // nothing supervising it. `toml_edit` keeps `ShepToml::edit` from writing
    // a file that will not PARSE, so the gap this closes is a valid-TOML bad
    // VALUE. `whistle`'s own gate runs the same check for the same reason.
    if let Err(err) = read_daemon_config_source(paths)
        .map_err(DaemonRunError::from)
        .and_then(|source| {
            DaemonConfig::load(source.as_deref(), &|key| std::env::var(key).ok())
                .map_err(DaemonRunError::from)
        })
    {
        return streams.fail(ExitCode::InvalidConfig, &err.to_string());
    }
```

Check the real signatures of `read_daemon_config_source` and `DaemonConfig::load` in this module and in `crates/shep-core/src/config/daemon.rs` and adjust. `load` takes `Option<&str>` and an env closure. If the `From` impls do not exist, match on the two errors separately rather than adding conversions.

- [ ] **Step 4: Run the test**

Run: `cargo test -p shep --lib --all-features reload_refuses_a_shep_toml`
Expected: PASS.

- [ ] **Step 5: Add the end-to-end case**

In `crates/shep-cli/tests/cli_e2e.rs`, using the existing `write_shep_toml` helper at line 4653 and whatever harness starts a daemon and a sheep:

```rust
/// fails if a bad shep.toml can orphan a running flock. Starts a real daemon
/// and a real sheep, writes a value that parses as TOML and not as config,
/// runs `shep daemon reload`, then asserts the flock is still supervised: the
/// refusal must happen before anything is signalled.
#[test]
fn a_bad_shep_toml_refuses_the_reload_and_leaves_the_flock_supervised() {
    // start daemon + one sheep, assert it is Online
    // write_shep_toml(&home, "[daemon]\nmax_cron_sleep = \"soon\"\n")
    // run `shep daemon reload`, assert exit code 4 (InvalidConfig)
    // run `shep flock`, assert the sheep is still Online and the pid is unchanged
}
```

Fill this in against the harness the file already uses; copy the closest existing daemon-plus-sheep test.

- [ ] **Step 6: Run the e2e tier**

Run: `cargo test -p shep --test cli_e2e a_bad_shep_toml`
Expected: PASS.

- [ ] **Step 7: Prove both non-vacuous**

Comment out the pre-flight block, run both tests, confirm both go red, restore.

- [ ] **Step 8: Commit**

```bash
git add crates/shep-cli/src/commands/daemon.rs crates/shep-cli/tests/cli_e2e.rs
git commit -m "fix(cli): validate shep.toml before a daemon reload, not after the predecessor is gone"
```

---

## Task 3: Recover a Flockfile's declared key set

**Files:**
- Modify: `crates/shep-core/src/config/flockfile.rs`
- Test: same file's `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub struct DeclaredApp {
      pub config: AppConfig,
      pub declared: BTreeSet<String>,
      pub declared_env: BTreeSet<String>,
  }

  impl Flockfile {
      pub fn parse_declared(text: &str, format: FlockFormat) -> Result<Vec<DeclaredApp>, ParseError>;
  }
  ```
  Tasks 8 and 10 consume `DeclaredApp`.

`AppConfig` is `#[serde(deny_unknown_fields, default)]`, so after parsing, a document that declared four keys is indistinguishable from one that declared forty. The merge rule needs the literal key set, which means reading it from the document before it becomes a struct.

Deserialize once into `serde_json::Value` through the format's own deserializer, read each app table's keys, then deserialize that same value into `RawFlockfile`. One generic intermediate covers all four formats.

- [ ] **Step 1: Write the failing test**

```rust
/// fails if the declared key set is inferred from values rather than read
/// from the document. `autorestart = true` is also the DEFAULT, so a parser
/// that reports "fields that differ from Default" would miss it, and a later
/// file load would then overwrite an operator who had deliberately turned it
/// off.
#[test]
fn declared_reports_keys_the_document_wrote_even_at_their_default() {
    let text = r#"
[[app]]
name = "web"
script = "./srv"
autorestart = true
"#;
    let apps = Flockfile::parse_declared(text, FlockFormat::Toml).unwrap();
    assert_eq!(apps.len(), 1);
    let declared = &apps[0].declared;
    assert!(declared.contains("autorestart"), "declared: {declared:?}");
    assert!(declared.contains("name"));
    assert!(declared.contains("script"));
    assert!(!declared.contains("max_memory"), "a key nobody wrote is not declared");
    assert_eq!(declared.len(), 3);
}

/// fails if env keys are not reported separately. `env` is the only map of
/// user-supplied keys in AppConfig, so the merge treats it one level deeper
/// than every other field.
#[test]
fn declared_env_reports_the_keys_inside_the_env_table() {
    let text = r#"
[[app]]
name = "web"
script = "./srv"
env = { DB_HOST = "", NODE_ENV = "production" }
"#;
    let apps = Flockfile::parse_declared(text, FlockFormat::Toml).unwrap();
    assert_eq!(apps[0].declared_env.iter().collect::<Vec<_>>(), vec!["DB_HOST", "NODE_ENV"]);
    assert!(apps[0].declared.contains("env"));
}

/// fails if a format other than TOML loses the key set. All four go through
/// one generic intermediate, so a regression here means the intermediate was
/// bypassed for a format.
#[test]
fn declared_survives_every_parse_format() {
    let json = r#"{"app":[{"name":"web","script":"./srv","autorestart":true}]}"#;
    let apps = Flockfile::parse_declared(json, FlockFormat::Json).unwrap();
    assert!(apps[0].declared.contains("autorestart"));
}
```

Check the real names of `FlockFormat`'s variants and `Flockfile::parse`'s signature before writing these, and match them.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-core --lib --all-features declared_`
Expected: FAIL, `parse_declared` does not exist.

- [ ] **Step 3: Implement**

```rust
/// One app as the document declared it: the validated config, plus the keys
/// the document literally wrote.
///
/// The key set cannot be recovered from [`AppConfig`] afterwards.
/// `#[serde(default)]` gives every field a value, so a document naming four
/// keys deserializes identically to one naming forty. The merge in the daemon
/// keys on what a template CLAIMS rather than on what its values are, so the
/// claim has to be carried out of the parser.
// Serialize/Deserialize because this type travels inside
// `Request::ApplyConfig`. The key sets are the whole reason the request
// carries this rather than a bare `AppConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredApp {
    /// The app, validated the same way [`Flockfile::parse`] validates one
    pub config: AppConfig,
    /// Top-level keys this app's table wrote, whatever their values
    pub declared: BTreeSet<String>,
    /// Keys inside this app's `env` table. Empty when `env` was not declared.
    pub declared_env: BTreeSet<String>,
}
```

and on `Flockfile`:

```rust
    /// Parses `text` and reports, per app, which keys the document wrote.
    ///
    /// Deserializes once into a `serde_json::Value`, reads the key sets off
    /// it, then deserializes that same value into the document type. One
    /// intermediate rather than four, so a format cannot drift.
    ///
    /// # Errors
    ///
    /// Every error [`Flockfile::parse`] returns, for the same inputs.
    pub fn parse_declared(text: &str, format: FlockFormat) -> Result<Vec<DeclaredApp>, ParseError> {
        let value: serde_json::Value = /* per-format deserialize; see parse() */;
        let raw: RawFlockfile = serde_json::from_value(value.clone())
            .map_err(|err| /* the same error parse() produces */)?;

        let tables = value.get("app").and_then(serde_json::Value::as_array);
        raw.apps
            .into_iter()
            .enumerate()
            .map(|(index, config)| {
                let table = tables.and_then(|t| t.get(index)).and_then(serde_json::Value::as_object);
                let declared = table
                    .map(|t| t.keys().cloned().collect())
                    .unwrap_or_default();
                let declared_env = table
                    .and_then(|t| t.get("env"))
                    .and_then(serde_json::Value::as_object)
                    .map(|e| e.keys().cloned().collect())
                    .unwrap_or_default();
                Ok(DeclaredApp { config, declared, declared_env })
            })
            .collect()
    }
```

Read `Flockfile::parse` first and reuse its per-format deserialization and its normalization step exactly. `parse_declared` must validate identically; the only difference is the extra key sets. If `parse` normalizes, this does too.

Export `DeclaredApp` from `crates/shep-core/src/config/mod.rs` and from the crate root wherever `Flockfile` is exported.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p shep-core --lib --all-features declared_`
Expected: PASS.

- [ ] **Step 5: Prove non-vacuous**

Change `declared` to be computed as "fields differing from `AppConfig::default()`". `declared_reports_keys_the_document_wrote_even_at_their_default` must go red. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-core/src/config/flockfile.rs crates/shep-core/src/config/mod.rs
git commit -m "feat(core): report which keys a Flockfile document actually declared"
```

---

## Task 4: The field classification table

**Files:**
- Create: `crates/shep-core/src/config/apply.rs`
- Modify: `crates/shep-core/src/config/mod.rs`
- Test: in the new file

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub enum ApplyGroup { Live, NextSpawn, NeedsRespawn, Structural }
  pub fn apply_group(field: &str) -> ApplyGroup;

  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "snake_case")]
  #[non_exhaustive]
  pub enum ResetDepth { None, Settings, All }
  ```
  Task 8 routes on `apply_group`. `ResetDepth` lives here, in shep-core,
  rather than in the daemon, because it travels inside
  `Request::ApplyConfig`.

The spec's classification, with its three corrections. `Live` is the spec's G1, `NextSpawn` is G2, `NeedsRespawn` is G3, `Structural` is G4.

- [ ] **Step 1: Write the failing test**

```rust
/// fails if any AppConfig field is missing from the table. A field added to
/// the struct without a group would route as its default and either apply
/// live when it cannot, or need a restart when it does not.
#[test]
fn every_appconfig_field_has_a_group() {
    let serde_json::Value::Object(fields) = serde_json::to_value(AppConfig::default()).unwrap()
    else {
        panic!("AppConfig must serialize as an object");
    };
    let missing: Vec<&String> = fields.keys().filter(|k| !is_classified(k)).collect();
    assert!(missing.is_empty(), "unclassified AppConfig fields: {missing:?}");
}

/// fails if kill_signal is classified Live. It is read from the per-sheep
/// task's frozen ResolvedApp, moved in once at spawn_sheep_task and never
/// refreshed, so an edit reaches the next spawn and not the next kill. Its
/// ladder-mates kill_timeout and graceful_timeout ARE read fresh, in
/// claim_manual, which is why this one looks like it belongs with them.
#[test]
fn kill_signal_reaches_the_next_spawn_not_the_next_kill() {
    assert_eq!(apply_group("kill_signal"), ApplyGroup::NextSpawn);
    assert_eq!(apply_group("kill_timeout"), ApplyGroup::Live);
    assert_eq!(apply_group("graceful_timeout"), ApplyGroup::Live);
}

/// fails if shutdown_with_message is classified anything but NeedsRespawn.
/// assemble() ORs it into whether fd 3 is opened for the child, which is the
/// child's own fd table and cannot change under a running process.
#[test]
fn shutdown_with_message_is_baked_into_the_child() {
    assert_eq!(apply_group("shutdown_with_message"), ApplyGroup::NeedsRespawn);
}

/// fails if the split drifts from what the spec recorded.
#[test]
fn the_split_is_nineteen_four_fourteen_three() {
    let serde_json::Value::Object(fields) = serde_json::to_value(AppConfig::default()).unwrap()
    else {
        panic!("AppConfig must serialize as an object");
    };
    let count = |want: ApplyGroup| fields.keys().filter(|k| apply_group(k) == want).count();
    assert_eq!(count(ApplyGroup::Live), 19, "Live");
    assert_eq!(count(ApplyGroup::NextSpawn), 4, "NextSpawn");
    assert_eq!(count(ApplyGroup::NeedsRespawn), 14, "NeedsRespawn");
    assert_eq!(count(ApplyGroup::Structural), 3, "Structural");
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-core --lib --all-features apply_group`
Expected: FAIL, the module does not exist.

- [ ] **Step 3: Implement the table**

```rust
//! How each `AppConfig` field reaches a running sheep.
//!
//! Four answers, and the difference between them is where the daemon READS
//! the field, not what the field means. A value read fresh at each decision
//! can be swapped under a running process with no disruption; one baked into
//! the child at exec cannot change until that process is replaced.
//!
//! The three entries most likely to look wrong carry their reasoning at their
//! own arm below. All three were measured against the read sites rather than
//! inferred from the field's name.

use crate::config::AppConfig;

/// Where a field's new value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyGroup {
    /// Read fresh at each decision, so a write to the stored spec is enough.
    Live,
    /// Read when a process spawns, so a write reaches the next one.
    NextSpawn,
    /// Held by the running child, so that instance must be replaced.
    NeedsRespawn,
    /// Identity or flock shape, not a runtime knob.
    Structural,
}

/// The group `field` belongs to.
///
/// An unknown name answers [`ApplyGroup::NeedsRespawn`], the most
/// conservative of the four: a field this table has not been taught about
/// gets a restart rather than a silent claim that it applied.
/// `every_appconfig_field_has_a_group` keeps that arm unreachable for real
/// fields.
#[must_use]
pub fn apply_group(field: &str) -> ApplyGroup {
    match field {
        // Read by `brain::decide` when a sheep exits.
        "autorestart" | "max_restarts" | "min_uptime" | "restart_delay"
        | "exp_backoff_restart_delay" | "stop_exit_codes"
        // Read by `claim_manual` when a kill ladder runs.
        | "kill_timeout" | "graceful_timeout"
        // Read fresh when extras arms a worker. These need a re-arm to take
        // effect; see `ExtrasRegistry::rearm_name`.
        | "max_memory" | "watch" | "ignore_watch" | "watch_delay" | "watch_options"
        | "cron_restart" | "cron_timezone" | "liveness_probe"
        // Read fresh per command.
        | "fold" | "reuse_port" => ApplyGroup::Live,

        // `kill_signal` is NOT Live, despite its two ladder-mates above. It
        // is read inside `kill_process` from the `app: &AppConfig` parameter
        // of the long-lived per-sheep task, whose `ResolvedApp` is moved in
        // once at `spawn_sheep_task` and never refreshed.
        "kill_signal"
        | "listen_timeout" | "readiness_probe"
        // Read once at muster or boot, by `restorable()`.
        | "autostart" => ApplyGroup::NextSpawn,

        // Baked into the child at exec: argv, cwd, environment, credentials,
        // the fd table, the log paths it is already writing to.
        "script" | "args" | "cwd" | "interpreter" | "env" | "user" | "group"
        | "out_file" | "err_file" | "merge_logs" | "channel" | "stdin" | "wait_ready"
        // `shutdown_with_message` belongs here rather than with the kill
        // ladder: `assemble()` ORs it into whether fd 3 is opened, and that
        // is the child's own fd table.
        | "shutdown_with_message" => ApplyGroup::NeedsRespawn,

        "name" | "instances"
        // Read only by `normalize` to refuse it by name.
        | "increment_var" => ApplyGroup::Structural,

        _ => ApplyGroup::NeedsRespawn,
    }
}

/// Whether `field` is named explicitly above, as opposed to reaching the
/// conservative fallback. Test-facing.
#[must_use]
pub fn is_classified(field: &str) -> bool {
    // Implement by listing the names once and reusing that list in
    // `apply_group`, or by a second exhaustive match. Do not let the two
    // drift; `every_appconfig_field_has_a_group` only catches a field missing
    // from BOTH.
}
```

Write `is_classified` against a single `const` slice of every named field, and have `apply_group` fall through to `NeedsRespawn` only for names absent from it. Two sources of truth here is exactly the drift the test cannot catch. If the counts in `the_split_is_nineteen_four_fourteen_three` do not come out, the table is wrong and not the test: recount against the field list rather than editing the expected numbers, and report the discrepancy.

Add `ResetDepth` in the same module:

```rust
/// How much of a Flockfile load overwrites what the operator has set since.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResetDepth {
    /// Append keys nobody established. Overwrite nothing. The default,
    /// because a Flockfile arrives from the app's own repository.
    #[default]
    None,
    /// Put non-`env` settings back to the template, keeping `env`. `env` is
    /// operator-supplied data while the rest is operator-tuned policy:
    /// resetting policy is recoverable, resetting data takes the app's
    /// database away.
    Settings,
    /// Put everything back to the template, `env` included.
    All,
}
```

**What shipped is wider than the design spec's wording, and the difference is
the maintainer's to settle.** Spec section 3 says `--reset` keeps
operator-added keys; `merge_declared` puts EVERY non-`env` key in scope at
this depth, undeclared ones included, and spends the override on each. An
undeclared key therefore goes back to the value a fresh start of this same
file would have registered, which for a key nothing supplies is the compiled
default, and no non-`env` override survives a `--reset`. Only `env` survives
it, and `All` removes that too. This text describes the code, because the code
is what ships; whether the code should narrow to the spec is an open question,
not something a doc edit decides.

Register the module in `crates/shep-core/src/config/mod.rs` and re-export `ApplyGroup`, `apply_group` and `ResetDepth`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p shep-core --lib --all-features apply_group`
Expected: PASS, all four.

- [ ] **Step 5: Prove non-vacuous**

Move `kill_signal` into the `Live` arm. `kill_signal_reaches_the_next_spawn_not_the_next_kill` must go red and `the_split_is_nineteen_four_fourteen_three` must go red too. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-core/src/config/apply.rs crates/shep-core/src/config/mod.rs
git commit -m "feat(core): classify every AppConfig field by where the daemon reads it"
```

---

## Task 5: A force-replacing re-arm for a name group

**Files:**
- Modify: `crates/shep-daemon/src/extras.rs`
- Test: same file's `mod tests`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  ```rust
  impl ExtrasRegistry {
      pub fn rearm_name(
          &mut self,
          name: &str,
          entries: &[&ProcessEntry],
          prober: Arc<dyn Prober>,
          extras: &Extras,
          supervisor: &SupervisorHandle,
      );
  }
  ```
  Task 8 calls this after writing a Live field.

`ExtrasRegistry::arm` rebuilds the per-id workers but deliberately leaves a live name-group task alone:

```rust
if group.cron.as_ref().is_none_or(JoinHandle::is_finished) {
    group.cron = arm_cron(config, extras, supervisor);
}
```

and the same for watch. `a_replacement_arming_before_the_drainee_disarms_keeps_the_groups_own_tasks` pins that by task identity, so it is deliberate. `disarm` is no escape: it aborts the group only when the id leaving was the last armed member.

So six Live fields (`watch`, `ignore_watch`, `watch_delay`, `watch_options`, `cron_restart`, `cron_timezone`) need a method that replaces the group tasks unconditionally, acting on the name rather than one id.

- [ ] **Step 1: Read `arm` and `disarm` in full**

`crates/shep-daemon/src/extras.rs`, roughly lines 320 to 430, plus `NameExtras::abort` at 243 and `InstanceExtras::disarm` at 281. Read the existing test at 2532 so you understand what `rearm_name` must NOT break.

- [ ] **Step 2: Write the failing test**

These use the same helpers as `a_replacement_arming_before_the_drainee_disarms_keeps_the_groups_own_tasks`, which lives in this module and compares task identity through `abort_handle()`. Put them in the same `mod` as that test so the helpers are in scope.

```rust
/// fails if re-arming leaves the old group tasks running. `arm` deliberately
/// keeps a live cron or watch task, which is right for a reload's overlap and
/// wrong for a config change: those tasks read `watch`, `ignore_watch`,
/// `watch_delay`, `watch_options`, `cron_restart` and `cron_timezone` when
/// they are BUILT, so a task that survives keeps the old values forever.
#[tokio::test(start_paused = true)]
async fn rearm_name_replaces_a_live_group_task() {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let root = tempfile::tempdir().unwrap();
    let (handle, _rx, _fixture) = spawn_test_fixture();
    let rig = rig(DEFAULT_MAX_CRON_SLEEP);
    let mut registry = ExtrasRegistry::default();
    let app = app_with("web", |app| {
        app.cron_restart = Some("0 * * * *".to_string());
        app.watch = true;
        app.cwd = Some(root.path().display().to_string());
    });
    handle.start(vec![app.clone()]).await.unwrap();

    let entry = armed_entry(0, 0, 1000, app.clone(), &paths);
    registry.arm(&entry, idle_prober(), &rig.extras, &handle);
    tokio::task::yield_now().await;

    let before_cron = registry.groups["web"].cron.as_ref().unwrap().abort_handle();
    let before_watch = registry.groups["web"].watch.as_ref().unwrap().abort_handle();

    registry.rearm_name("web", &[&entry], idle_prober(), &rig.extras, &handle);
    tokio::task::yield_now().await;

    let after_cron = registry.groups["web"].cron.as_ref().unwrap().abort_handle();
    let after_watch = registry.groups["web"].watch.as_ref().unwrap().abort_handle();
    assert_ne!(before_cron.id(), after_cron.id(), "the cron worker survived a rearm");
    assert_ne!(before_watch.id(), after_watch.id(), "the watch task survived a rearm");
}

/// fails if rearm_name tears a group down without rebuilding it. An app left
/// with no watcher at all is worse than one left with a stale watcher.
#[tokio::test(start_paused = true)]
async fn rearm_name_leaves_the_group_armed() {
    // Same setup as above, then after rearm_name assert both handles are
    // present and `is_finished()` is false on each.
}

/// fails if rearm_name reaches into another app's group. The registry is
/// keyed by name and a rebuild stays inside one name.
#[tokio::test(start_paused = true)]
async fn rearm_name_leaves_another_apps_group_alone() {
    // Arm "web" and "worker", capture worker's watch abort_handle, call
    // rearm_name("web", ...), assert worker's handle id is unchanged.
}
```

Check `abort_handle()` and `id()` are what this module already compares by; the existing test at `extras.rs:2532` is the reference. If it compares differently, copy what it does.

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test -p shep-daemon --lib --all-features rearm_name`
Expected: FAIL, the method does not exist.

- [ ] **Step 4: Implement**

```rust
    /// Rebuilds everything armed for `name`, replacing live tasks rather than
    /// keeping them.
    ///
    /// [`Self::arm`] deliberately preserves a running cron or watch task, so
    /// that a reload's replacement instance arming before the drainee disarms
    /// does not tear down a watcher the drainee still needs. That is right for
    /// the transition it was written for and wrong for a config change: the
    /// four group-scoped fields (`watch`, `ignore_watch`, `watch_delay`,
    /// `watch_options`) and the two cron ones are read when the task is built,
    /// so a task that survives keeps the old values forever.
    ///
    /// Takes every entry of the name rather than one id, because the group is
    /// per-name: disarming a single instance of a multi-instance app leaves
    /// the group standing, by design.
    ///
    /// # What this loses
    ///
    /// The OS watch is torn down and rebuilt with a real gap and no rescan, so
    /// a file saved during it is missed. Same gap any watcher restart has.
    /// `stats.watch()` clears the pid's CPU baseline, so `shep flock` shows a
    /// blank CPU cell for one poll interval. Both are documented rather than
    /// closed; see the design spec.
    pub fn rearm_name(
        &mut self,
        name: &str,
        entries: &[&ProcessEntry],
        prober: Arc<dyn Prober>,
        extras: &Extras,
        supervisor: &SupervisorHandle,
    ) {
        // Abort the group's own tasks before rebuilding, which is the whole
        // difference from `arm`. Removing the entry rather than mutating it
        // means the rebuild below takes `arm`'s own "no task yet" path, so
        // there is one construction site rather than two.
        if let Some(group) = self.groups.remove(name) {
            group.abort();
        }
        for entry in entries {
            self.arm(entry, Arc::clone(&prober), extras, supervisor);
        }
    }
```

Check that `NameExtras::abort` consumes `self` (it does at line 243) and that `groups` is the field name. If `arm` needs `&ProcessEntry` and you hold `&&ProcessEntry`, deref.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`
Expected: PASS, and `a_replacement_arming_before_the_drainee_disarms_keeps_the_groups_own_tasks` still passes. If that one breaks you have changed `arm` rather than added a sibling.

- [ ] **Step 6: Prove non-vacuous**

Replace `rearm_name`'s body with a bare loop of `self.arm(...)`, dropping the group removal. `rearm_name_replaces_a_live_watch_task` must go red on its own. If it stays green the test is not reaching the group path and needs rewriting before you continue. Restore.

- [ ] **Step 7: Commit**

```bash
git add crates/shep-daemon/src/extras.rs
git commit -m "feat(daemon): rearm_name, a force-replacing sibling to arm for config changes"
```

---

## Task 6: An epoch so a stale liveness failure cannot restart a sheep

**Files:**
- Modify: `crates/shep-daemon/src/extras.rs` (liveness arming and its reporter)
- Modify: `crates/shep-daemon/src/supervisor.rs` (`handle_extra_restart`, line 7047, and the `Command::ExtraRestart` variant)
- Test: `extras.rs` and `supervisor.rs` test modules

**Interfaces:**
- Consumes: task 5's `rearm_name` exists, though this task does not call it.
- Produces: `Command::ExtraRestart` gains an `epoch: u64` field. Task 8 does not touch it.

`InstanceExtras::disarm` calls `liveness.abort()`, which does not await. A liveness task aborted mid-flight can still have already sent its failure. `handle_extra_restart` guards only on pid and status:

```rust
if slot.entry.pid != Some(pid) { return; }
if slot.entry.status != ProcStatus::Online { return; }
```

A config-only re-arm changes neither, so a stale failure passes both guards and restarts the sheep. That makes a config apply kill a process, which the design forbids outright.

`SheepSlot` already carries an `epoch: u64`. Check whether it advances on a re-arm; if it tracks something else, add a separate liveness epoch rather than overloading it, and say why in the code.

- [ ] **Step 1: Write the failing test**

```rust
/// fails if a liveness failure from an ABORTED probe can still restart a
/// sheep. A config-only re-arm changes neither the pid nor the status, so
/// handle_extra_restart's two guards both pass and the sheep is restarted by
/// a probe that no longer exists. A config apply must never kill a process.
#[tokio::test]
async fn a_stale_liveness_failure_from_a_replaced_probe_does_not_restart() {
    // Arm an entry with a liveness probe.
    // Capture the epoch it was armed at.
    // Re-arm it (the config changed), which advances the epoch.
    // Send Command::ExtraRestart with the OLD epoch, the CURRENT pid and
    // status Online.
    // Assert no restart happened: the entry's restarts count is unchanged and
    // its pid is the same.
}
```

Use the supervisor test module's existing helpers for building an actor and asserting restart counts. Grep for other `ExtraRestart` tests and copy their shape.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p shep-daemon --lib --all-features a_stale_liveness_failure`
Expected: FAIL to compile, because `ExtraRestart` has no `epoch` field yet. That compile failure is the red state; do not work around it.

- [ ] **Step 3: Thread the epoch**

Add `epoch: u64` to `Command::ExtraRestart`. Have the liveness arming capture the epoch it was armed at and include it in every failure it reports. In `handle_extra_restart`, add a third guard alongside the two that exist:

```rust
        // A third guard, and the two above cannot stand in for it. A probe
        // replaced because its CONFIG changed leaves the pid and the status
        // exactly as they were, so a failure already in flight from the
        // aborted task passes both. `liveness.abort()` does not await, so
        // that in-flight failure is a real case and not a theoretical one.
        // Without this, changing any config field on a sheep with a liveness
        // probe could restart it, and a config apply must never kill a
        // process.
        if slot.liveness_epoch != epoch {
            tracing::debug!(
                id,
                pid,
                epoch,
                current = slot.liveness_epoch,
                "extra restart dropped: the reporting probe has been replaced"
            );
            return;
        }
```

Advance `liveness_epoch` wherever an instance's liveness is armed, so every arm gets a fresh one.

- [ ] **Step 4: Run the test and the suite**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`
Expected: PASS. Existing `ExtraRestart` tests need the new field; update their construction and nothing else about them.

- [ ] **Step 5: Prove non-vacuous**

Delete the third guard. The new test must go red while the other two guards' tests stay green. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-daemon/src/extras.rs crates/shep-daemon/src/supervisor.rs
git commit -m "fix(daemon): an epoch, so a replaced liveness probe cannot restart the sheep it left"
```

---

## Task 7: The override store

**Files:**
- Create: `crates/shep-core/src/overrides.rs`
- Modify: `crates/shep-core/src/lib.rs`, `crates/shep-core/src/paths.rs`
- Test: in the new file

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub struct AppOverrides {
      pub fields: serde_json::Map<String, serde_json::Value>,
      pub declared: BTreeSet<String>,
      pub declared_env: BTreeSet<String>,
  }
  pub const OVERRIDES_VERSION: u32 = 1;
  pub fn all(path: &Path) -> Result<BTreeMap<String, AppOverrides>, OverridesError>;
  pub fn get(path: &Path, name: &str) -> Result<Option<AppOverrides>, OverridesError>;
  pub fn put(path: &Path, name: &str, value: &AppOverrides) -> Result<(), OverridesError>;
  pub fn remove(path: &Path, name: &str) -> Result<bool, OverridesError>;
  ```
  Task 8 reads and writes through these.

`ShepPaths` gains `overrides: PathBuf`, deriving to `$SHEP_HOME/overrides.json`.

**Which precedent to copy.** `crates/shep-core/src/kv.rs`, not `snapshot::write_atomic`. `snapshot`'s no-lock shape is safe only because the daemon is its sole writer and coordinates internally. This store is written by the daemon today and by CLI verbs later, so it needs `kv.rs`'s sibling `.lock` file with `flock(2)` on unix and `share_mode(0)` on Windows. Copy `KvLock`'s shape; it is private to its module, so copy rather than reuse.

`fields` holds env values, so `AppOverrides` needs a redacted `Debug` (IR-41).

- [ ] **Step 1: Read `kv.rs` end to end**

Its module doc explains why the lock exists, its `KvLock` shows the two-platform dance, `create_kv_file` sets `0600` at creation through `tempfile::Builder::permissions`, and its `two_concurrent_writers` test at the bottom is what proves the lock works.

- [ ] **Step 2: Write the failing tests**

```rust
/// fails if a written override does not come back.
#[test]
fn put_then_get_round_trips() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("overrides.json");
    let mut fields = serde_json::Map::new();
    fields.insert("max_memory".to_string(), serde_json::json!("512M"));
    let value = AppOverrides {
        fields,
        declared: ["name", "script"].iter().map(|s| s.to_string()).collect(),
        declared_env: BTreeSet::new(),
    };
    put(&path, "web", &value).unwrap();
    assert_eq!(get(&path, "web").unwrap().as_ref(), Some(&value));
}

/// fails if a missing store is an error. A fresh $SHEP_HOME has no overrides
/// and that is the normal state, not a fault.
#[test]
fn a_missing_store_reads_as_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    assert!(all(&dir.path().join("overrides.json")).unwrap().is_empty());
}

/// fails if the store is readable by anyone but its owner. It holds env
/// values, which is what flock.json's own owner-only test exists for.
#[cfg(unix)]
#[test]
fn the_store_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("overrides.json");
    put(&path, "web", &AppOverrides::default()).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
}

/// fails if Debug prints an env value. This store is where an operator's
/// secrets will live (IR-41).
#[test]
fn debug_redacts_override_values() {
    let mut fields = serde_json::Map::new();
    fields.insert("env".to_string(), serde_json::json!({"DATABASE_URL": "postgres://hunter2"}));
    let value = AppOverrides { fields, ..AppOverrides::default() };
    let rendered = format!("{value:?}");
    assert!(!rendered.contains("hunter2"), "leaked: {rendered}");
}

/// fails if a store written by a NEWER shep is silently rewritten by an older
/// one, which would drop every field this binary does not know.
#[test]
fn a_future_version_refuses_without_clobbering() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("overrides.json");
    std::fs::write(&path, r#"{"version":99,"apps":{}}"#).unwrap();
    assert!(matches!(get(&path, "web"), Err(OverridesError::FutureVersion(99))));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"version":99,"apps":{}}"#);
}

/// fails if two concurrent writers lose each other's work. This is what the
/// lock is for; kv.rs's own version of this test is the model.
#[test]
fn two_concurrent_writers_lose_nothing() {
    // Spawn two threads, each putting 50 distinct app names, join both,
    // assert all 100 are present.
}
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test -p shep-core --lib --all-features overrides`
Expected: FAIL, the module does not exist.

- [ ] **Step 4: Implement**

Mirror `kv.rs` structurally: an on-disk `OverridesFile { version: u32, apps: BTreeMap<String, AppOverrides> }`, a private `read_file`/`write_file` pair, a lock acquired at the top of every public function, and a `#[non_exhaustive]` error enum wrapping `std::io::Error` and `serde_json::Error` directly rather than stringifying them.

`AppOverrides` derives `Clone, Default, PartialEq, Eq, Serialize, Deserialize` and gets a manual `Debug`:

```rust
/// Redacted: `fields` can hold an `env` map, and this store is the primary
/// place an operator's secrets live (IR-41).
impl fmt::Debug for AppOverrides {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppOverrides")
            .field("fields", &format_args!("<{} fields>", self.fields.len()))
            .field("declared", &self.declared)
            .field("declared_env", &self.declared_env)
            .finish()
    }
}
```

`declared` and `declared_env` are key names, never values, so they print in full.

Add `overrides: PathBuf` to `ShepPaths` and set it in every construction site. Grep for `ShepPaths {` before assuming there is only one.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p shep-core --lib --all-features overrides`
Expected: PASS, all six.

- [ ] **Step 6: Prove the lock non-vacuous**

Replace the lock acquisition with a no-op. `two_concurrent_writers_lose_nothing` must go red. It may need several runs to fail; if it never does, raise the writer count until it does, then restore the lock and keep the higher count.

- [ ] **Step 7: Commit**

```bash
git add crates/shep-core/src/overrides.rs crates/shep-core/src/lib.rs crates/shep-core/src/paths.rs
git commit -m "feat(core): the override store, locked and owner-only like the KV store"
```

---

## Task 8: `Command::ApplyConfig`, the merge and the routing

**Files:**
- Modify: `crates/shep-daemon/src/supervisor.rs`
- Modify: `crates/shep-daemon/src/entry.rs` (`ProcessEntry` gains `pending`)
- Test: `supervisor.rs`'s `mod tests`

**Interfaces:**
- Consumes: `ApplyGroup`/`apply_group` (task 4), `AppOverrides` and its store functions (task 7), `rearm_name` (task 5), `DeclaredApp` (task 3).
- Produces:
  ```rust
  pub(crate) struct Applied {
      pub(crate) name: String,
      pub(crate) applied: Vec<String>,
      pub(crate) pending: Vec<String>,
      pub(crate) refused: Option<String>,
      /// The merged, normalized app. `rpc.rs` hands this to
      /// `FlockRegistry::record`, the way the `Scale` arm hands it
      /// `Scaled::app`, so a reboot comes up on the applied config.
      pub(crate) app: ResolvedApp,
  }

  impl SupervisorHandle {
      pub(crate) async fn apply_config(
          &self,
          apps: Vec<DeclaredApp>,
          reset: ResetDepth,
      ) -> Result<Vec<Applied>, SupervisorError>;
  }
  ```
  Task 9 promotes `pending`, task 10 puts `apply_config` on the wire, and
  task 12 surfaces both lists.

This is the largest task. Read `handle_scale` (`supervisor.rs:4677`) end to end first: it is the model for a command that mutates slots and persists, including its partial-failure handling and its deliberate spawn-before-write-back ordering.

**The merge, per app, per field:**

1. If the Flockfile declares the key, its value wins and the override for that key is deleted.
2. Otherwise if overridden, the override's value.
3. Otherwise the default.

Under `ResetDepth::None` this runs only for keys not in the established set (`declared` from a previous load, union the override keys). Under `Settings` it runs for every non-`env` key. Under `All` it runs for everything and the override store entry is removed.

`env` merges one level deeper: the Flockfile's declared env keys win, override-only env keys survive.

- [ ] **Step 1: Add `pending` to `ProcessEntry`**

```rust
    /// The config a file load left for this sheep's next spawn.
    ///
    /// `None` for every sheep outside the window between a load that changed
    /// a `NeedsRespawn` field and the restart that picks it up. `spec` keeps
    /// describing what the running child was spawned from, which is the only
    /// account of that anywhere; overwriting it would erase it.
    pub pending: Option<ResolvedApp>,
```

Set it to `None` in every `ProcessEntry` construction site.

- [ ] **Step 2: Write the failing tests**

```rust
/// fails if a later file load overwrites a key the operator set. Additive is
/// the default precisely because a Flockfile arrives from the app's own
/// repository, so a merged pull request must not be able to change a running
/// flock's config.
#[tokio::test]
async fn a_file_load_does_not_overwrite_an_established_key() {
    // Start "web" with max_restarts = 10.
    // Apply an override setting max_restarts = 3.
    // apply_config with a DeclaredApp declaring max_restarts = 99.
    // Assert the stored config still says 3.
}

/// fails if a key absent from the established set is not appended. This is
/// what makes a template update reach an app at all.
#[tokio::test]
async fn a_file_load_appends_a_key_nobody_had_established() {
    // Start "web" declaring only name and script.
    // apply_config with a DeclaredApp also declaring max_memory = "512M".
    // Assert the stored config now carries it and it is reported in `applied`.
}

/// fails if a Live field does not take effect on the stored spec.
#[tokio::test]
async fn a_live_field_lands_on_the_stored_spec() {
    // apply_config changing max_restarts, assert entry.spec.config() reflects
    // it and `applied` names it.
}

/// fails if a NeedsRespawn field touches the running child rather than
/// parking. A load must never kill a process.
#[tokio::test]
async fn a_needs_respawn_field_parks_as_pending_and_leaves_the_child_alone() {
    // Start "web", capture its pid.
    // apply_config changing `env`.
    // Assert entry.pid is unchanged, entry.spec.config().env is unchanged,
    // entry.pending is Some and its env carries the new value, and the reply
    // names "env" under `pending`.
}

/// fails if a merged config that cannot normalize is half-applied. An app
/// whose merge is invalid refuses whole; the rest of the flock still applies.
#[tokio::test]
async fn an_unnormalizable_merge_refuses_one_app_and_applies_the_others() {
    // Two apps. Make the first's merge invalid (instances = 2 with an
    // explicit out_file carrying no {{instance}} and no merge_logs).
    // Assert the first's Applied carries `refused` with the normalize error,
    // its stored config is untouched, and the second applied normally.
}

/// fails if an app the file no longer mentions is deleted. A load never
/// prunes: the daemon has no record of which Flockfile an app came from, so
/// `shep start ./a/Flockfile.toml` followed by `./b/Flockfile.toml` would
/// have the second wipe the first's flock.
#[tokio::test]
async fn an_app_absent_from_the_file_is_left_running() {
    // Start "web" and "worker".
    // apply_config with a DeclaredApp for "web" only.
    // Assert "worker" is still registered, still Online, same pid, and that
    // no Applied entry claims to have touched it.
}

/// fails if the reset depths do not differ. --reset restores every non-env
/// setting, declared or not, and leaves env; --reset-all restores env too and
/// drops the override record.
#[tokio::test]
async fn reset_depths_differ_on_env_and_on_extras() {
    // Establish name, script, max_restarts from a file.
    // Override max_restarts, add an env key nobody declared, add a
    // never-declared field.
    // ResetDepth::Settings: max_restarts back to the file's, the env key
    // survives, the never-declared field goes back to what a fresh start off
    // this file would give it and its override is spent.
    // ResetDepth::All: env key gone, added field gone.
}
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test -p shep-daemon --lib --all-features apply_config`
Expected: FAIL to compile.

- [ ] **Step 4: Implement**

Add `Command::ApplyConfig { apps: Vec<DeclaredApp>, reset: ResetDepth, reply: oneshot::Sender<Result<Vec<Applied>, SupervisorError>> }`, its `SupervisorHandle::apply_config` (copy `config_drift`'s send-and-await shape verbatim), its arm in the command match (not rejected while `shutting_down`, following `ConfigDrift`'s reasoning: it spawns nothing), and `handle_apply_config`.

Per app, in this order:

1. Find the slots for the name. Absent means report and skip, never register.
2. Load the app's `AppOverrides`.
3. Build the merged `AppConfig` per the rules above.
4. `normalize` it. `Err` sets `refused` and moves to the next app, touching nothing.
5. Compute `drifted_fields(stored, merged)` and partition by `apply_group`.
6. `Structural` `instances`: route through the existing scale path.
7. `Live` and `NextSpawn`: write onto every slot's `entry.spec`.
8. If any `Live` field was in the extras set, call `rearm_name` with every entry of the name.
9. `NeedsRespawn`: set `entry.pending` on every slot.
10. Write the updated `AppOverrides` back.
11. Return the names of each group.

Then in the rpc layer, `ctx.registry.record(...)` with the merged app, exactly as the `Scale` arm does at `rpc.rs:396`, so a reboot comes up on the applied config.

- [ ] **Step 5: Run the suite**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`
Expected: PASS.

- [ ] **Step 6: Prove the two that matter non-vacuous**

Delete the `rearm_name` call: any test asserting a `watch` change took effect must go red. Delete the `pending` write and apply `NeedsRespawn` straight onto `entry.spec`: `a_needs_respawn_field_parks_as_pending_and_leaves_the_child_alone` must go red. Restore both.

- [ ] **Step 7: Commit**

```bash
git add crates/shep-daemon/src/supervisor.rs crates/shep-daemon/src/entry.rs
git commit -m "feat(daemon): apply a Flockfile onto a running flock additively, without killing anything"
```

---

## Task 9: Promote pending config on reload and restart

**Files:**
- Modify: `crates/shep-daemon/src/supervisor.rs` (the reload path and the restart path)
- Test: same file's `mod tests`

**Interfaces:**
- Consumes: `ProcessEntry::pending` (task 8).
- Produces: nothing new. This is what makes task 8's pending slot reachable.

Task 8 parks `NeedsRespawn` fields in `entry.pending`. Without this task nothing ever reads it, so the slice would ship config an operator can see and never apply.

Promotion is `entry.spec = pending.take()`, done where a sheep is about to be replaced. `shep reload` and `shep restart` both spawn a new child, so both promote. Neither re-reads a file, so both `decisions.md` entries saying reload does not re-read config stay true exactly as written: reload still reads only the stored spec, and the stored spec is what changed.

**Credentials are the wrinkle.** `ProcessEntry::credentials` is documented as resolved once and reused, so a restart never changes a running app's identity underneath it. An operator who changed `user` or `group` is asking for precisely that, so promotion resets `credentials` to `SpawnIdentity::Unresolved` when either of those two is among the promoted fields, and only then.

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if a reload does not pick up config a file load parked. Without this
/// the pending slot is written and never read, so an operator can see a
/// pending field forever and has no way to apply it.
#[tokio::test]
async fn reload_promotes_pending_config() {
    // Start "web". apply_config changing `env` (a NeedsRespawn field).
    // Assert entry.pending is Some.
    // Reload "web".
    // Assert entry.spec.config().env carries the new value, entry.pending is
    // None, and the pid changed.
}

/// fails if a restart does not promote. Both verbs replace the child, so both
/// are chances to apply what is owed.
#[tokio::test]
async fn restart_promotes_pending_config() {
    // As above, through restart rather than reload.
}

/// fails if promoting a `user` change reuses the identity resolved at the
/// original start. `credentials` is resolved once precisely so a restart does
/// not change a running app's identity by accident; an operator editing
/// `user` is asking for it on purpose, and that is the one case that must
/// re-resolve.
#[tokio::test]
async fn promoting_a_user_change_re_resolves_credentials() {
    // Start "web" with no user. apply_config setting user.
    // Reload. Assert credentials went back to Unresolved before the spawn,
    // so the spawn path resolved it fresh.
}

/// fails if promotion resets credentials for a field that has nothing to do
/// with identity. Re-resolving on every promotion would mean a passwd lookup
/// per config change and would defeat the once-only rule for no reason.
#[tokio::test]
async fn promoting_an_unrelated_change_keeps_the_resolved_identity() {
    // Start "web" with a user that resolves. apply_config changing `args`.
    // Reload. Assert credentials is still the resolved value.
}
```

Grep `mod tests` in `supervisor.rs` for how existing reload tests drive a reload and read back an entry, and copy that shape.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep-daemon --lib --all-features promotes_pending`
Expected: FAIL, nothing reads `pending`.

- [ ] **Step 3: Implement**

Add one helper on the actor and call it from both paths:

```rust
    /// Moves a sheep's pending config onto its stored spec, if it has any.
    ///
    /// Called where a child is about to be replaced, which is the only moment
    /// a `NeedsRespawn` field can take effect. Nothing here re-reads a file:
    /// the pending config was put there by a file load that already happened,
    /// so `shep reload`'s documented promise not to re-parse a Flockfile is
    /// untouched.
    ///
    /// Returns the field names promoted, for the caller's report.
    fn promote_pending(&mut self, id: u32) -> Vec<String> {
        let Some(slot) = self.sheep.get_mut(&id) else {
            return Vec::new();
        };
        let Some(pending) = slot.entry.pending.take() else {
            return Vec::new();
        };
        let promoted = slot.entry.spec.config().drifted_fields(pending.config());

        // The one exception to `credentials` being resolved once. That rule
        // exists so a restart never changes a running app's identity by
        // accident; an operator who edited `user` or `group` is asking for
        // exactly that change, and reusing the old identity would silently
        // ignore them. Narrow on purpose: any other promoted field keeps the
        // resolved value, so an ordinary config change costs no passwd
        // lookup.
        if promoted.iter().any(|f| f == "user" || f == "group") {
            slot.entry.credentials = SpawnIdentity::Unresolved;
        }
        slot.entry.spec = pending;
        promoted
    }
```

Call it in the reload path and the restart path, before the spawn that replaces the child. Find both by grepping for where a replacement is spawned; `handle_reload` and `claim_manual`'s restart arm are the starting points.

- [ ] **Step 4: Run the suite**

Run: `cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`
Expected: PASS.

- [ ] **Step 5: Prove non-vacuous**

Delete the `user`/`group` condition so promotion always resets credentials. `promoting_an_unrelated_change_keeps_the_resolved_identity` must go red while the other three stay green. Then delete the whole `promote_pending` call from the reload path: `reload_promotes_pending_config` must go red. Restore both.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-daemon/src/supervisor.rs
git commit -m "feat(daemon): reload and restart promote pending config, re-resolving identity only when it changed"
```

---

## Task 10: The wire, and `shep start <file>`

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs`, `crates/shep-daemon/src/rpc.rs`, `crates/shep-cli/src/commands/lifecycle.rs`
- Test: each of those three modules

**Interfaces:**
- Consumes: `apply_config` (task 8), `DeclaredApp` (task 3).
- Produces:
  ```rust
  Request::ApplyConfig { apps: Vec<DeclaredApp>, reset: ResetDepth }
  Response::Applied(Vec<SheepApplied>)
  pub struct SheepApplied { pub name: String, pub applied: Vec<String>, pub pending: Vec<String>, pub refused: Option<String> }
  ```

`SheepApplied` mirrors `SheepDrift` (`request.rs:1232`): field names only, never values, because `env` carries secrets.

`PROTOCOL_VERSION` **stays 2**. The rule at `protocol/mod.rs:43` keeps the version for new variants behind `#[non_exhaustive]`, and `shep-core`'s CHANGELOG applies it repeatedly, `ConfigDrift` itself among them.

- [ ] **Step 1: Add the protocol types**

Mirror `SheepDrift` exactly, including its `#[non_exhaustive]`, its derives and a `new` constructor. Document that the vectors carry names and never values, citing IR-41 and `env` as the reason, the way `SheepDrift`'s own doc does.

- [ ] **Step 2: Add the rpc arm**

In `rpc.rs`'s `run`, **before** the `_ =>` catch-all at line 550:

```rust
        Request::ApplyConfig { apps, reset } => match ctx.supervisor.apply_config(apps, reset).await
        {
            Ok(applied) => {
                // Recorded unconditionally, on the same reasoning the `Scale`
                // arm above gives: an apply that reached the stored spec must
                // reach the roll too, or a reboot brings up config that was
                // never running. Refused apps carry their pre-merge app, so
                // recording them is a no-op rather than a special case.
                let recorded: Vec<ResolvedApp> =
                    applied.iter().map(|a| a.app.clone()).collect();
                ctx.registry.record(&recorded);
                reply(Ok(Response::Applied(
                    applied.into_iter().map(SheepApplied::from).collect(),
                )))
            }
            Err(err) => reply(Err(rpc_error(&err))),
        },
```

`Applied` (daemon-side, task 8) carries the merged `ResolvedApp`;
`SheepApplied` (wire-side) does not, because a config is not something a
client needs and `env` is in it. Write the `From<Applied> for SheepApplied`
conversion in `rpc.rs` and let it drop the app.

- [ ] **Step 3: Write the CLI test**

```rust
/// fails if `shep start <name>` reads a Flockfile. The file is a template
/// from the app's repository; a load must be something the operator asked
/// for by naming a file, never a side effect of starting a sheep by name.
#[tokio::test]
async fn start_by_name_sends_no_apply_config_even_with_a_flockfile_present() {
    // cwd containing a Flockfile.toml that declares "web" differently from
    // what the fake daemon holds.
    // Run start with a name selector against a capturing fake client.
    // Assert no Request::ApplyConfig was sent.
}
```

Use `shep_client::testing::fake_client_capturing_envelopes`, which `muster.rs`'s tests already use.

- [ ] **Step 4: Wire `shep start <file>`**

In `lifecycle.rs`, where `start` reads a Flockfile: parse with `Flockfile::parse_declared`, split apps into those the flock already has and those it does not, send `Request::Start` for the fresh ones as today, and `Request::ApplyConfig` for the known ones. Replace the existing `Request::ConfigDrift` warning call: the drift warning's sentence is documented in `deferred.md` as describing something no terminal can produce, so it goes rather than being kept alongside.

Render the reply: one line per app naming what applied, what is pending, and any refusal. A pending list must say what promotes it, `shep reload <name>`.

- [ ] **Step 5: Run all three tiers**

```bash
cargo test -p shep-core --lib --all-features
```
```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```
```bash
cargo test -p shep --lib --bins --all-features -- --skip ::slow::
```

One at a time, each from its own command with `$?` captured directly. In zsh a pipeline's `$?` is the last command's.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-core/src/protocol/request.rs crates/shep-daemon/src/rpc.rs crates/shep-cli/src/commands/lifecycle.rs
git commit -m "feat: shep start <file> applies a template additively; shep start <name> reads nothing"
```

---

## Task 11: `--reset` and `--reset-all`

**Files:**
- Modify: `crates/shep-cli/src/cli.rs` (`StartArgs`), `crates/shep-cli/src/commands/lifecycle.rs`
- Test: both

**Interfaces:**
- Consumes: `ResetDepth` (task 4), `Request::ApplyConfig` (task 10).
- Produces: nothing new.

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if the two flags map to the same depth, or if either can be combined
/// with the other. They differ on two axes and a caller must pick one.
#[test]
fn the_reset_flags_are_mutually_exclusive_and_map_to_distinct_depths() {
    assert_eq!(depth_of(&parse_start(&["shep", "start", "F.toml"])), ResetDepth::None);
    assert_eq!(depth_of(&parse_start(&["shep", "start", "F.toml", "--reset"])), ResetDepth::Settings);
    assert_eq!(depth_of(&parse_start(&["shep", "start", "F.toml", "--reset-all"])), ResetDepth::All);
    assert!(Cli::try_parse_from(["shep", "start", "F.toml", "--reset", "--reset-all"]).is_err());
}

/// fails if a reset flag is accepted when the target is a NAME. There is no
/// file to reset to, so the flag is meaningless and silently doing nothing
/// would be worse than refusing.
#[test]
fn a_reset_flag_on_a_name_target_is_refused() {
    // assert the parse or the command refuses, and the message names the
    // reason.
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep --lib --all-features reset_flag`
Expected: FAIL, the flags do not exist.

- [ ] **Step 3: Add the flags**

On `StartArgs`, with `conflicts_with`:

```rust
    /// Put process settings back to what the Flockfile says, keeping env.
    /// Every setting goes back, declared or not.
    #[arg(long, conflicts_with = "reset_all")]
    pub reset: bool,
    /// Put everything back to what the Flockfile says, including env, and
    /// drop the override record.
    #[arg(long = "reset-all")]
    pub reset_all: bool,
```

Doc comments become `--help` text, so they carry the difference in what survives.

- [ ] **Step 4: Run the tests and the suite**

Run: `cargo test -p shep --lib --bins --all-features -- --skip ::slow::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shep-cli/src/cli.rs crates/shep-cli/src/commands/lifecycle.rs
git commit -m "feat(cli): --reset and --reset-all on a Flockfile load"
```

---

## Task 12: Surface pending and overridden

**Files:**
- Modify: `crates/shep-core/src/protocol/request.rs` (`ProcessInfo`), `crates/shep-daemon/src/supervisor.rs` (`to_info`), `crates/shep-cli/src/output/rows.rs`
- Test: each

**Interfaces:**
- Consumes: `ProcessEntry::pending` (task 8), the override store (task 7).
- Produces: `ProcessInfo.pending: Option<Vec<String>>` and `ProcessInfo.overridden: Option<Vec<String>>`.

Both additive under `Option`, so `PROTOCOL_VERSION` stays 2 and `SCHEMA_VERSION` stays 1. Names only, never values.

`FlockRows`' headers are at `rows.rs:42`. The EXIT column is the precedent for adding one; `exit_cell` at `rows.rs:914` is the cell-value function to copy in shape.

- [ ] **Step 1: Write the failing tests**

```rust
/// fails if a sheep with pending config is indistinguishable from one
/// without. A pending field an operator cannot see is a silent divergence,
/// which is worse than the problem this feature set out to fix.
#[test]
fn the_cfg_cell_marks_a_sheep_with_pending_config() {
    let mut info = sample_info();
    info.pending = Some(vec!["env".to_string()]);
    assert_eq!(cfg_cell(info.pending.as_deref(), info.overridden.as_deref()), "!1");

    let clean = sample_info();
    assert_eq!(cfg_cell(clean.pending.as_deref(), clean.overridden.as_deref()), "-");
}

/// fails if ProcessInfo ever carries an override VALUE. env values are
/// secrets and nothing sends them to a client today (IR-41).
#[test]
fn process_info_carries_names_and_never_values() {
    let json = serde_json::to_string(&{
        let mut info = sample_info();
        info.overridden = Some(vec!["env".to_string()]);
        info
    })
    .unwrap();
    assert!(json.contains("\"env\""));
    assert!(!json.contains("DATABASE_URL"));
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p shep --lib --all-features cfg_cell`
Expected: FAIL, the fields and the function do not exist.

- [ ] **Step 3: Add the fields and the column**

Add both fields to `ProcessInfo` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, documented as names only and why. Populate them in `to_info`. Add `"CFG"` to `FlockRows::headers()` and a `cfg_cell` next to `exit_cell`.

`shep flock`'s adaptive column dropping handles the width; check where the drop order is declared and put `CFG` at the priority the EXIT column sits at or lower.

- [ ] **Step 4: Add the describe section**

`shep describe <name>` lists the field names under a pending heading and an overridden heading, with the sentence naming `shep reload <name>` as what promotes pending.

- [ ] **Step 5: Run every tier**

```bash
cargo test -p shep-core --lib --all-features
```
```bash
cargo test -p shep-daemon --lib --all-features -- --skip ::slow::
```
```bash
cargo test -p shep --lib --bins --all-features -- --skip ::slow::
```

- [ ] **Step 6: Commit**

```bash
git add crates/shep-core/src/protocol/request.rs crates/shep-daemon/src/supervisor.rs crates/shep-cli/src/output/rows.rs
git commit -m "feat: a CFG column and a describe section, so pending config is visible"
```

---

## Task 13: Docs and the full gate

**Files:**
- Modify: `web/src/pages/docs/first-flockfile.astro`, `web/src/pages/docs/getting-started.astro`
- Create: `web/src/pages/docs/overrides.astro`, and its entry in `web/src/data/docsNav.ts`
- Modify: `docs/decisions.md`, `docs/specs/deferred.md`, `CLAUDE.md`
- Regenerate: `web/src/data/cli-reference.generated.txt`

**Interfaces:**
- Consumes: every task above.
- Produces: nothing.

`web/` is published and part of the deliverable. Two `--reset` flags, a changed `shep start`, a new column and a new `describe` section all count as things an operator types or sees.

- [ ] **Step 1: Write the overrides page**

Cover the template model, the three load modes and what each keeps, that a template may add and never overwrite, and that removing a line from a Flockfile stops removing the setting once it has been overridden, with `--reset-all` as the way out. Prose pages are hand-written and no generator touches them.

- [ ] **Step 2: Correct the two pages that are now wrong**

`first-flockfile.astro` calls a Flockfile an app's config; it is a template now. `getting-started.astro`'s upgrading section frames `shep daemon reload` entirely around the binary and says nothing about `shep.toml`, which it has always re-read.

- [ ] **Step 3: Record the decisions**

In `docs/decisions.md`, an entry per non-obvious call: why `arm` needed a force-replacing sibling, why the liveness epoch exists, why additive is the default, why `PROTOCOL_VERSION` did not move. Add one saying explicitly that the two existing "reload does not re-read config" entries are about `shep reload <sheep>` and the Flockfile, not `shep daemon reload` and `shep.toml`. That confusion is the thing most likely to produce a wrong doc later.

Close `deferred.md`'s config-edit entry, recording that it is fixed by `shep start <file> --reset` rather than by the default, and record the three field-split corrections there.

In `CLAUDE.md`, `shep stock` is no longer the exception and the reload paragraph needs the pre-flight.

- [ ] **Step 4: Regenerate the CLI reference**

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```

`git diff` afterwards is the check. A stale copy fails no build, which is exactly why it drifts.

- [ ] **Step 5: Build and check the site**

```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

Both. `check` is the one that catches a wrong prop; a page passing a component a prop it does not have builds clean and renders wrong.

- [ ] **Step 6: Run the full task gate**

Each from its own command, `$?` captured directly, one cargo command at a time:

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

- [ ] **Step 7: Commit**

```bash
git add web/ docs/ CLAUDE.md
git commit -m "docs: the override model, and the pages the apply engine made wrong"
```

---

## Out of this plan

- **`shep add`.** Plan 2. The supervisor side already exists as `register_at_rest`; the work is a request variant, an rpc arm and a verb module.
- **Shared env and `{{shared:...}}`.** Plan 2. Needs `template::render` to become fallible, which touches the spawn path.
- **Per-field provenance in `describe` and `shep export`.** Plan 2. Both read the same declared and override sets this plan builds.
- **lookout.** Plan 3. A UI over an API that exists after plan 2.
- **The secret store.** Spec 2, not yet written.
