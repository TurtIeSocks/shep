# CLI Plumbing Ergonomics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the repetition around `emit_error`, `emit_notice` and the pure-conversion `map_err` sites, with no change to a single byte shep prints or to any exit code.

**Architecture:** Three independent moves. `fmt` joins `Streams`, which already carries `style` for the same reason. `Streams` grows `fail` and `note` methods so a call site names a code and a message and nothing else. And 26 error variants that wrap a source and carry nothing else get `From` impls, so their `map_err` becomes a bare `?`.

**Tech Stack:** Rust 2024, MSRV 1.88. **No new dependencies**, which is half the point.

**Spec:** [docs/brainstorming/specs/2026-08-23-cli-plumbing-ergonomics-design.md](../../brainstorming/specs/2026-08-23-cli-plumbing-ergonomics-design.md). Read it before Task 1; it carries the reasoning, including why this is not macros.

## Global Constraints

- **Nothing shep prints may change.** Same code string, same message text, same shape in `--format table` and `--format json`. Task 1 pins this with snapshots before anything moves, and those snapshots must be byte-identical at the end of every later task.
- **No exit code may change**, including the ones reached only when a write fails.
- **No new dependency.** Not `thiserror`, not `anyhow`, not a proc-macro crate. IR-18 permits `anyhow` only inside `shep`, and IR-19 already specifies manual `Display`.
- **`docs/idiomatic-rust.md`'s rules (IR-1..IR-45).** Invoke the `shep-idiomatic-rust` skill before writing any Rust here. `# Errors` sections on fallible public functions, `core::error::Error`, and a deliberate `Debug` decision on new public items.
- **No em dashes or en dashes** in anything a person reads, `///` comments included.
- **One cargo shape per task.** The workspace shares one target-dir lock. Run gates as their own command with `$?` read directly, never through a pipe: in zsh a pipeline's `$?` is the last command's.
- **Clean-room rule:** never open, read or reference `~/GitHub/pm2`.

## Verified facts, measured rather than assumed

Counted 2026-08-23. Use these; do not re-derive them.

- `emit_error` has **91** call sites, `emit_notice` **20**, and **every one** discards the `io::Result` with `let _ =`.
- **84** functions take `streams: &mut Streams`, and **all 84** also take `fmt: Format`.
- **Every** call passing a `Format` other than the ambient `fmt` is in test code (`status.rs`, `welcome.rs`, `selector.rs`, `bleats.rs`, `output/mod.rs`). No production caller overrides it.
- `Streams { .. }` is constructed **12** times in production, all in `lib.rs`, and **93** times in test code.
- `Streams` today is `{ out, err, style }` (`output/mod.rs:92`). `Format` is `Copy` (`cli.rs:163`).
- `emit`, `emit_error` and `emit_notice` all take `&mut dyn io::Write` plus `Format`, not a `Streams`. `lib.rs:1272` calls `emit_error` with **no `Streams` at all**, so the free functions must survive.
- **220** `map_err` sites across the three crates: **72** pure conversion, **148** carrying context.
- **26** of shep's own error variants cover **66** of the 72. Six convert into foreign types (`std::io::Error::other`, `serde::de::Error::custom`) where the orphan rule blocks a `From`.
- `insta` is a dev-dependency of `shep-cli` (`Cargo.toml:167`) and already used by `output/mod.rs`, `output/table.rs` and `lookout/frames.rs`.

---

### Task 1: Pin what shep prints, before touching anything

**Files:**
- Modify: `crates/shep-cli/src/output/mod.rs` (its existing `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: snapshot tests that later tasks must not change.

This task exists because Tasks 2 and 3 touch 91 emit sites and 84 signatures, which is exactly the diff where a behaviour change hides in the noise. Without a baseline, "the suite is green" only says nobody asserted on the thing that moved.

- [ ] **Step 1: Add the snapshots**

In `output/mod.rs`'s test module. Cover both formats and both emitters, and include one message with characters worth escaping, since JSON and table treat them differently.

```rust
#[test]
fn what_an_error_looks_like_on_the_wire() {
    for (fmt, name) in [(Format::Table, "table"), (Format::Json, "json")] {
        let mut out = Vec::new();
        emit_error(&mut out, fmt, ExitCode::Usage.code_str(), "no flock at /tmp/x").unwrap();
        insta::assert_snapshot!(format!("error_{name}"), String::from_utf8(out).unwrap());
    }
}

#[test]
fn what_a_notice_looks_like_on_the_wire() {
    for (fmt, name) in [(Format::Table, "table"), (Format::Json, "json")] {
        let mut out = Vec::new();
        emit_notice(&mut out, fmt, "init", "wrote /tmp/x/Flockfile.toml").unwrap();
        insta::assert_snapshot!(format!("notice_{name}"), String::from_utf8(out).unwrap());
    }
}

#[test]
fn an_error_message_with_awkward_bytes_survives_both_formats() {
    // Quotes and a backslash render differently in the two formats, so a
    // change to either path shows up here rather than in a caller's test.
    for (fmt, name) in [(Format::Table, "table"), (Format::Json, "json")] {
        let mut out = Vec::new();
        emit_error(&mut out, fmt, ExitCode::InvalidConfig.code_str(), r#"bad "quoted" \path"#).unwrap();
        insta::assert_snapshot!(format!("error_awkward_{name}"), String::from_utf8(out).unwrap());
    }
}
```

- [ ] **Step 2: Accept the snapshots and read them**

```bash
cargo insta test --accept -p shep --lib
```

If `cargo insta` is not installed, run the tests once and rename the `.snap.new` files by hand; do not add a dependency for this.

**Read each accepted snapshot before committing it.** A snapshot accepted without being read pins whatever the code does today, including a bug. These are short; read them.

- [ ] **Step 3: Verify**

```bash
cargo test -p shep --lib --all-features
```
Expected: PASS, six new snapshot assertions.

- [ ] **Step 4: Commit**

```bash
git add crates/shep-cli/src/output/mod.rs crates/shep-cli/src/output/snapshots/
git commit -m "test(cli): pin what an error and a notice look like on the wire"
```

---

### Task 2: `fmt` moves into `Streams`, and `Streams` grows `fail` and `note`

**Files:**
- Modify: `crates/shep-cli/src/output/mod.rs`, `crates/shep-cli/src/lib.rs`, and every file under `crates/shep-cli/src/commands/` and `crates/shep-cli/src/` that takes `streams` or calls an emitter.

**Interfaces:**
- Consumes: Task 1's snapshots, which must not change.
- Produces:
  - `Streams { out, err, style, fmt }`
  - `pub fn Streams::fail(&mut self, code: ExitCode, message: &str) -> ExitCode`
  - `pub fn Streams::note(&mut self, code: &str, message: &str)`

Moves 1 and 2 of the spec land together, deliberately. Doing them apart would mean touching all 84 signatures twice, and the second pass would rewrite the call sites the first pass had just finished editing.

- [ ] **Step 1: Add the field and the methods**

```rust
pub struct Streams<'a> {
    pub out: &'a mut dyn io::Write,
    pub err: &'a mut dyn io::Write,
    pub style: Presentation,
    /// How this invocation renders: a table for a person, or JSON for a
    /// script.
    ///
    /// Carried here for the reason `style` is, one field up: it reaches
    /// every command already, and all 84 functions that take a `Streams`
    /// also took a `Format` beside it. Nothing in production ever passed a
    /// different one, so nothing loses an override it was using.
    pub fmt: Format,
}

impl Streams<'_> {
    /// Prints `message` as an error, and hands back the code it printed.
    ///
    /// Returning the code is what lets a caller write
    /// `return streams.fail(ExitCode::Usage, &message)` rather than naming
    /// the code twice and risking the two drifting apart.
    ///
    /// The write's own failure is discarded, deliberately: a closed stderr
    /// must not change what shep exits with. That was the decision at all
    /// 91 call sites this replaces, and it is made once here instead.
    pub fn fail(&mut self, code: ExitCode, message: &str) -> ExitCode {
        let _ = emit_error(&mut *self.err, self.fmt, code.code_str(), message);
        code
    }

    /// Prints `message` as a notice, on stdout.
    ///
    /// Discards its write's failure for the same reason [`Self::fail`] does.
    pub fn note(&mut self, code: &str, message: &str) {
        let _ = emit_notice(&mut *self.out, self.fmt, code, message);
    }
}
```

**`emit`, `emit_error` and `emit_notice` keep their signatures.** They take a raw writer, and `lib.rs:1272` calls one with no `Streams` at all.

**Check `note`'s stream against the call sites before writing it.** Some notices go to stdout and some to stderr (`init`'s shadow warning goes to stderr deliberately). If both are real, `note` takes stdout and a second method or an explicit `emit_notice` call covers the stderr case. Do not silently move a notice from one stream to the other; that is a visible change.

- [ ] **Step 2: Add the field at the 12 production constructions**

All in `lib.rs`. Each already has `fmt` in scope.

- [ ] **Step 3: Drop `fmt: Format` from the 84 signatures, and fix the callers**

Mechanical. The compiler drives it: remove the parameter, then fix every error it reports. Work file by file and keep the crate compiling as often as you can.

- [ ] **Step 4: Move the call sites onto `fail` and `note`**

```rust
// before
let _ = emit_error(&mut *streams.err, fmt, ExitCode::Usage.code_str(), &message);
return ExitCode::Usage;

// after
return streams.fail(ExitCode::Usage, &message);
```

Where a call site emits an error and then returns a **different** code, leave it explicit rather than bending it. Say in your report if you find any; that would be a bug worth naming, not a shape to preserve.

- [ ] **Step 5: Add the field at the 93 test constructions**

Each test names the format it exercises. Where a test previously passed `Format::Table` to a function, that value moves to its `Streams`.

- [ ] **Step 6: Verify, and read the snapshots**

```bash
cargo test -p shep --lib --all-features
```
```bash
cargo test -p shep --test cli_e2e --all-features
```

**Task 1's snapshots must be unchanged.** A `.snap.new` file appearing is a bug in this task, never an update to accept. If one appears, stop and report what differs.

Known red and NOT this task's: `cli_e2e` has 5 failing `shep_init_*` tests on `main` too, from in-flight work.

- [ ] **Step 7: Commit**

```bash
git add -- crates/shep-cli/
git commit -m "refactor(cli): fmt lives in Streams, and Streams can fail and note"
```

---

### Task 3: `From` impls for the 26 source-only variants

**Files:**
- Modify: the modules owning the 26 variants, across `shep-cli`, `shep-core` and `shep-daemon`.

**Interfaces:**
- Consumes: nothing from Tasks 1 or 2. Orderable before them if that suits.
- Produces: `impl From<Source> for TheError` for each of the 26, and the matching `map_err` sites become `?`.

The 26, covering 66 sites:

```
BarkError::Encode      BarkError::Io          BootError::ReadyWrite  BootError::Snapshot
ConnError::Auth        ConnError::Decode      ConnError::Encode      ConnError::Frame
DaemonRunError::Boot   DaemonRunError::Config DaemonRunError::Run     DogError::NoBinary
DogRunError::Connect   DogRunError::Request   FetchError::Transport  HttpError::Io
IndexError::Fetch      KvError::Decode        KvError::Io            NotifyError::Io
RulesError::InsecureSink                      ServeRefusal::Auth      SinkError::Transport
SnapshotError::Encode  SnapshotError::Io      TargetError::Flockfile
```

- [ ] **Step 1: Confirm each one really carries nothing else**

Before writing a single `From`, open each variant and check it holds the source and no other field. **This is the whole safety property of the task.** The list above was produced by a regex over call sites, and a variant that gained a second field since would be wrong to convert.

Report any that do not qualify, and skip them.

- [ ] **Step 2: Write the impls**

```rust
impl From<std::io::Error> for KvError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}
```

No derive, no crate, no attribute. Three lines each.

- [ ] **Step 3: Replace the `map_err` at those sites with `?`**

```rust
// before
let _lock = KvLock::acquire(path).map_err(KvError::Io)?;
// after
let _lock = KvLock::acquire(path)?;
```

**Leave every other `map_err` alone.** The 148 context-carrying sites are the feature: `TargetError::Read { source, path }` exists so the error names the file, and a `From` is exactly what would let somebody `?` past that later. If a site looks convertible but its variant has a second field, it is not convertible.

- [ ] **Step 4: Verify**

```bash
cargo test --workspace --all-features
```
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Watch for clippy's `needless_question_mark` or a `From` that makes an existing conversion ambiguous.

- [ ] **Step 5: Commit**

One commit, or one per crate if the diff reads better split.

```bash
git commit -m "refactor: From impls for the error variants that wrap only a source"
```

---

## Final verification

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

Each as its own command with `$?` read directly.

No `web/` step: this changes no verb, no flag and no printed text, so the docs trigger does not fire. **If the generated CLI reference does move, something in this refactor changed what an operator sees, and that is a bug in it.** Run the generator once at the end and confirm `git diff` on it is empty.

**The one check that matters most:** Task 1's snapshots, unchanged after all three tasks. Everything else here is a mechanical edit that the compiler supervises; those six files are the only thing standing between this refactor and a silent change to what shep prints.
