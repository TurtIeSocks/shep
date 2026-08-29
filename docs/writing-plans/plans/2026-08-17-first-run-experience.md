# First-Run Experience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `shep` work on the first command a pm2 refugee types, and make
`shep --help` teach rather than leak.

**Architecture:** Five self-contained tasks in `crates/shep-cli` only. One new
module (`welcome.rs`), one new verb (`welcome`), one new shared function
(`ensure_home`) that both the dispatch gate and `startup` route through, and a
`help_template` on the existing `Cli` struct. No wire-protocol change, no
daemon change, no new dependency.

**Tech Stack:** Rust 2024, MSRV 1.88, clap 4.6.6 (derive), `std::io::IsTerminal`.

**Spec:** [docs/brainstorming/specs/2026-08-17-first-run-experience-design.md](../../brainstorming/specs/2026-08-17-first-run-experience-design.md)

## Global Constraints

- **Invoke the `shep-idiomatic-rust` skill before writing any Rust here.** Every
  new public item needs docs; every new error type needs a `# Errors` section;
  `core::error::Error`, not `std::error::Error`.
- **Inner loop:** `cargo test -p shep --lib --bins --all-features`. The
  `shep` crate is a library with three `[[bin]]` targets, so `--bins` alone
  runs almost nothing.
- **ONE cargo shape per task.** Do not alternate `--workspace` with
  `-p shep`. This plan is single-crate: use `-p shep` throughout.
- **Task gate, once per task:** `cargo fmt --all --check`, then
  `cargo clippy -p shep --all-targets --all-features -- -D warnings`, each
  from its own command with `$?` captured directly, never through a pipe.
- **Never `create_dir_all` followed by `set_permissions`.** Use
  `DirBuilder::mode(DIR_MODE)` so the directory is never wider than `0700`,
  not even briefly. Both existing call sites
  (`crates/shep-cli/src/launch.rs:53`, `crates/shep-daemon/src/boot.rs:99`)
  carry doc comments explaining the TOCTOU window; match them.
- **Terminal detection is a parameter, never a call inside the function.**
  `commands/daemon.rs:187`'s `ansi_enabled(stderr_is_terminal: bool, ..)` is
  the pattern: the caller does `std::io::stderr().is_terminal()`, the function
  takes the `bool`. Otherwise the suppression rules are untestable.
- **User-facing copy contains no em dashes.** Doc comments and this plan may;
  strings a user reads may not.
- **`--home` and `$SHEP_HOME` are the same thing** to this code. clap's
  `#[arg(long, global = true, env = "SHEP_HOME")]` on `GlobalArgs::home`
  already folds the variable into the flag, so `global.home.is_some()` means
  "the operator named a home explicitly", by either route.

---

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `crates/shep-cli/src/cli.rs` | The clap tree. Gains `Commands::Welcome`, a `help_template`, a `help_heading` on `--home`, and loses its leaked doc comment. | 1, 4, 5 |
| `crates/shep-cli/src/welcome.rs` | **New.** The art, the quick-start text, the render function, and the two print entry points. Nothing else knows the copy. | 3, 4 |
| `crates/shep-cli/src/lib.rs` | `ensure_home` and `HomeRefusal` live beside the existing `resolve_paths`; the dispatch gate and the `Startup` arm both route through them. | 2, 4 |
| `crates/shep-cli/src/commands/startup/mod.rs` | Loses its own home-existence check (line 235) now that the gate owns it. | 2 |
| `crates/shep-core/src/paths.rs` | Doc only: `ShepPaths`' claim that creation happens daemon-side stops being true. | 2 |

Task order matters: **1 before 5** (both edit attributes on `Cli`), and
**3 before 4** (task 4 calls task 3's functions). Tasks 2 and 3 are
independent of everything else.

---

### Task 1: Stop leaking the `bin_name` note into `--help`

The doc comment on `Cli` explains why `bin_name = "shep"` is load-bearing.
clap renders a doc comment as `long_about`, so that paragraph is the first
thing `shep --help` prints today, markdown bold and "Phase 15 Task 11"
included. The engineering is correct and must be kept; it just has to stop
being a doc comment.

**Files:**
- Modify: `crates/shep-cli/src/cli.rs:22-34` (the doc comment above `Cli`)
- Test: `crates/shep-cli/src/cli.rs` (in the existing `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: nothing. Behaviour-only change to rendered help.

- [ ] **Step 1: Write the failing test**

Add to `crates/shep-cli/src/cli.rs`'s existing `mod tests`:

```rust
    /// `--help` is the first thing a stranger reads. It rendered this
    /// crate's own reasoning about clap's `bin_name` for three phases,
    /// because clap turns a doc comment into `long_about` and nobody ran
    /// the command after writing the comment.
    #[test]
    fn the_top_level_help_carries_no_implementation_notes() {
        let help = Cli::command().render_long_help().to_string();
        for leak in ["bin_name", "Phase 15", "load-bearing", "argv[0]"] {
            assert!(
                !help.contains(leak),
                "`shep --help` still contains the internal note {leak:?}:\n{help}"
            );
        }
    }
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep --lib --all-features the_top_level_help_carries_no_implementation_notes
```

Expected: FAIL, with the assertion naming `"bin_name"` and printing the help
text that contains it.

- [ ] **Step 3: Demote the note to a non-doc comment**

Replace the doc comment at `crates/shep-cli/src/cli.rs:22-34` with:

```rust
/// The `shep` command line.
// `bin_name = "shep"` below is load-bearing, not decoration. Without it clap
// renders every `Usage:` line from `argv[0]` rather than from `name` — so
// `shep-runtime --help` prints `Usage: shep-runtime runtime ...` and
// `shep-dev --help` prints `Usage: shep-dev dev ...` (verified empirically:
// both alias binaries built and run with no override, Phase 15 Task 11).
// Pinned so every rendering of a verb's own usage line reads `shep <verb>`
// whichever of the three `[[bin]]` targets produced it — the alias binaries
// are convenience entrypoints for that one invocation, not commands in their
// own right.
//
// A `//` comment, not `///`, deliberately: clap renders a doc comment as
// `long_about`, so as a doc comment this paragraph was the opening of
// `shep --help`. See `the_top_level_help_carries_no_implementation_notes`.
#[derive(Debug, clap::Parser)]
```

The `#[command(...)]` attribute block below it is unchanged.

- [ ] **Step 4: Run it and watch it pass**

```bash
cargo test -p shep --lib --all-features the_top_level_help_carries_no_implementation_notes
```

Expected: PASS.

- [ ] **Step 5: Confirm the whole crate is still green, then gate**

```bash
cargo test -p shep --lib --bins --all-features
```

Expected: PASS, no failures.

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

Both expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/cli.rs
git commit -m "fix(shep): stop rendering the bin_name note as --help's description

clap turns a doc comment into \`long_about\`, so the paragraph explaining why
\`bin_name = \"shep\"\` is load-bearing has been the opening of \`shep --help\`
since Phase 15 — markdown bold, phase number and all. The reasoning is right
and stays, as a \`//\` comment that clap does not read.

A test now asserts the rendered long help contains none of \"bin_name\",
\"Phase 15\", \"load-bearing\" or \"argv[0]\"."
```

---

### Task 2: `ensure_home` — create the default, refuse a named one

Today `~/.shep` is created only on the way to a running daemon:
`launch.rs`'s `launch_command` pre-creates `logs/`, and `boot::init_dirs`
creates the rest inside the daemon. `shep startup` installs a unit without
starting anything, so it needs the home in advance and refuses instead
(`commands/startup/mod.rs:235`). This task makes the home appear for any
command, and makes an explicitly named missing home a refusal everywhere
rather than only in `startup`.

**Files:**
- Modify: `crates/shep-cli/src/lib.rs` (add `HomeRefusal` and `ensure_home`
  beside `resolve_paths` at line 203; change the gate at line 360 and the
  `Startup` arm at line 301)
- Modify: `crates/shep-cli/src/commands/startup/mod.rs:234-243` (delete the
  `is_dir` check now owned by the gate)
- Test: `crates/shep-cli/src/lib.rs` (in the existing `mod tests`)

**Interfaces:**
- Consumes: `resolve_paths(&GlobalArgs) -> Result<ShepPaths, ExitCode>`
  (existing, `crates/shep-cli/src/lib.rs:203`); `shep_daemon::boot::DIR_MODE`
  (`0700`, already `pub`).
- Produces, for Task 4:
  - `enum HomeRefusal { Unresolved, Missing(PathBuf), Io { path: PathBuf, source: std::io::Error } }`
  - `fn ensure_home(global: &GlobalArgs) -> Result<(ShepPaths, bool), HomeRefusal>`
    — the `bool` is **true when this call created the home**, which is what
    tells Task 4 to print the welcome.
  - `fn ensure_home_at(paths: ShepPaths, explicit: bool) -> Result<(ShepPaths, bool), HomeRefusal>`
    — the same rule with the environment already resolved away. This is the
    testable half; `ensure_home` is a two-line wrapper over it.
  - `impl HomeRefusal { fn code(&self) -> ExitCode; fn message(&self) -> String }`

The split exists because the tests must not mutate `$HOME`. `shep-cli` has no
`temp-env` dev-dependency (checked: it is in neither
`crates/shep-cli/Cargo.toml` nor the workspace table), and adding one to set a
process-global variable inside a test binary that runs its tests in parallel
would be the wrong fix anyway. `ShepPaths::resolve` already takes its
environment as a closure for exactly this reason; `ensure_home_at` follows
that idiom one layer up.

- [ ] **Step 1: Write the failing tests**

Add to `crates/shep-cli/src/lib.rs`'s existing `mod tests`:

```rust
    /// A `ShepPaths` rooted at `root`, standing in for whatever
    /// `resolve_paths` would have produced.
    fn paths_at(root: &std::path::Path) -> ShepPaths {
        let home = root.join(".shep").to_string_lossy().into_owned();
        let env = |key: &str| (key == "SHEP_HOME").then(|| home.clone());
        ShepPaths::resolve(&env, std::path::Path::new("/nonexistent"))
    }

    /// The transcript that started this: a fresh machine, the pm2 flow, and
    /// the very first command fails. `~/.shep` is a name shep chose, so
    /// shep may create it.
    #[test]
    fn a_missing_default_home_is_created_and_reported_as_new() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(root.path());

        let (paths, created) =
            ensure_home_at(paths, false).expect("a default home must be created");
        assert_eq!(paths.home, root.path().join(".shep"));
        assert!(created, "the first call must report that it created the home");
        assert!(paths.home.is_dir(), "the home must exist on disk afterwards");

        let (_, created_again) =
            ensure_home_at(paths_at(root.path()), false).expect("second call must succeed");
        assert!(!created_again, "a home that already existed is not newly created");
    }

    /// A path the operator typed is not a path shep may invent. The likeliest
    /// reason it is missing is a typo, and creating it would turn that typo
    /// into a second, empty, invisible flock.
    #[test]
    fn an_explicitly_named_missing_home_is_refused_and_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(&root.path().join("srv").join("typo"));
        let named = paths.home.clone();

        let refusal = ensure_home_at(paths, true).expect_err("a named missing home is refused");
        assert_eq!(refusal.code(), ExitCode::Usage);
        let message = refusal.message();
        assert!(
            message.contains(&named.display().to_string()),
            "the refusal must name the path it refused: {message}"
        );
        assert!(
            message.contains("~/.shep"),
            "the refusal must point at the default as the way out: {message}"
        );
        assert!(
            !named.exists(),
            "a refused path must be left on disk exactly as it was found"
        );
    }

    /// The mode is the reason this does not use `create_dir_all`: a
    /// create-then-chmod sequence leaves the directory world-readable for as
    /// long as the two syscalls are apart.
    #[cfg(unix)]
    #[test]
    fn a_created_home_is_owner_only_from_the_moment_it_exists() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let (paths, _) = ensure_home_at(paths_at(root.path()), false).unwrap();
        let mode = std::fs::metadata(&paths.home).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "a fresh $SHEP_HOME must be owner-only");
    }
```

`tempfile` is already a dev-dependency of this crate.

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features ensure_home
```

Expected: FAIL to compile, `cannot find function ensure_home_at in this scope`.

- [ ] **Step 3: Add `HomeRefusal` and `ensure_home`**

Insert into `crates/shep-cli/src/lib.rs` directly below `resolve_paths`
(which ends at line 217):

```rust
/// Why [`ensure_home`] would not hand back a layout.
///
/// Separate from a bare [`ExitCode`] because two of the three carry the path
/// they are talking about, and the operator cannot act on a refusal that
/// does not name it.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug)]
enum HomeRefusal {
    /// Neither `--home`, `$SHEP_HOME`, nor `$HOME` resolves a root.
    Unresolved,
    /// `--home`/`$SHEP_HOME` named a path that is not there. Never created:
    /// see [`ensure_home`]'s own doc for why.
    Missing(PathBuf),
    /// The default home could not be created.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[cfg_attr(windows, allow(dead_code))]
impl HomeRefusal {
    /// The process exit status this refusal ends the command with.
    ///
    /// `Internal` rather than `Usage` for the io case: the operator asked for
    /// something reasonable and shep failed to do it, which is not a usage
    /// error however it reads at the terminal.
    fn code(&self) -> ExitCode {
        match self {
            Self::Unresolved | Self::Missing(_) => ExitCode::Usage,
            Self::Io { .. } => ExitCode::Internal,
        }
    }

    /// The message the operator sees, already carrying its own remedy.
    fn message(&self) -> String {
        match self {
            Self::Unresolved => UNRESOLVED_HOME.to_owned(),
            Self::Missing(path) => format!(
                "no flock at {}\n  \
                 did you mean to drop --home? the default is ~/.shep\n  \
                 to set up a flock there deliberately:  mkdir -p {}",
                path.display(),
                path.display()
            ),
            Self::Io { path, source } => {
                format!("could not create {}: {source}", path.display())
            }
        }
    }
}

/// Resolves `$SHEP_HOME` and makes sure the directory is there, returning the
/// layout and whether this call is what created it.
///
/// The asymmetry between a default home and a named one is the whole point.
/// `~/.shep` is a name shep chose, so shep may conjure it; `/srv/api` is a
/// name the operator typed, and the likeliest reason it is not there is a
/// typo. Creating a typo'd path silently would leave a second, empty,
/// invisible flock behind, and the bug report that follows is "shep lost all
/// my processes" when the truth is "you are looking at a different flock".
///
/// Only the root is created here. `logs/`, `pids/` and `run/` stay
/// [`shep_daemon::boot::init_dirs`]' job, which runs on every boot and
/// re-tightens all of them; this function exists for the commands that need
/// the root before any daemon has ever started, `startup` above all.
///
/// # Errors
///
/// - [`HomeRefusal::Unresolved`] — nothing names a root to resolve against.
/// - [`HomeRefusal::Missing`] — `--home`/`$SHEP_HOME` named an absent path.
/// - [`HomeRefusal::Io`] — the default home could not be created.
#[cfg_attr(windows, allow(dead_code))]
fn ensure_home(global: &GlobalArgs) -> Result<(ShepPaths, bool), HomeRefusal> {
    let paths = resolve_paths(global).map_err(|_| HomeRefusal::Unresolved)?;
    ensure_home_at(paths, global.home.is_some())
}

/// [`ensure_home`] with the environment already resolved away.
///
/// Split out so the rule can be tested without mutating `$HOME`, which is
/// process-global and shared by every test in this binary.
/// `ShepPaths::resolve` takes its environment as a closure for the same
/// reason; this follows that idiom one layer up.
///
/// `explicit` is whether the operator named this home themselves, by either
/// `--home` or `$SHEP_HOME`. It is the only thing that decides whether an
/// absent directory is created or refused.
///
/// # Errors
///
/// - [`HomeRefusal::Missing`] — `explicit` and the directory is not there.
/// - [`HomeRefusal::Io`] — the directory could not be created.
#[cfg_attr(windows, allow(dead_code))]
fn ensure_home_at(paths: ShepPaths, explicit: bool) -> Result<(ShepPaths, bool), HomeRefusal> {
    if paths.home.is_dir() {
        return Ok((paths, false));
    }
    if explicit {
        return Err(HomeRefusal::Missing(paths.home));
    }

    // `.mode(DIR_MODE)` at creation rather than `create_dir_all` followed by
    // `set_permissions`, matching `launch.rs:53` and `boot.rs:99`: a
    // create-then-chmod sequence leaves a window in which the directory
    // exists at whatever the ambient umask allows, and on a shared machine
    // that window is enough for another user to open a handle that survives
    // the later chmod. Do not "simplify" this.
    #[cfg(unix)]
    let built = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(shep_daemon::boot::DIR_MODE)
            .create(&paths.home)
    };
    #[cfg(not(unix))]
    let built = std::fs::DirBuilder::new().recursive(true).create(&paths.home);

    match built {
        Ok(()) => Ok((paths, true)),
        Err(source) => Err(HomeRefusal::Io {
            path: paths.home,
            source,
        }),
    }
}
```

- [ ] **Step 4: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features ensure_home
```

Expected: PASS, 3 passed.

Also run the two neighbouring names, which exercise the same code path:

```bash
cargo test -p shep --lib --all-features home
```

Expected: PASS, no failures.

- [ ] **Step 5: Route the dispatch gate through it**

Replace `crates/shep-cli/src/lib.rs:360-366`:

```rust
    let paths = match resolve_paths(&cli.global) {
        Ok(paths) => paths,
        Err(code) => {
            emit_error_locked(fmt, code, UNRESOLVED_HOME);
            return code;
        }
    };
```

with:

```rust
    let (paths, home_is_new) = match ensure_home(&cli.global) {
        Ok(resolved) => resolved,
        Err(refusal) => {
            let code = refusal.code();
            emit_error_locked(fmt, code, &refusal.message());
            return code;
        }
    };
    // Bound in Task 4. Named now so the shape of the gate does not change
    // twice; `let _ =` keeps clippy quiet until then.
    let _ = home_is_new;
```

`emit_error_locked`'s third parameter is `&str`, and `refusal.message()`
returns `String`, so the `&` is required.

- [ ] **Step 6: Route `Startup` through it too, and delete its own check**

`Commands::Startup` bypasses the shared gate entirely
(`crates/shep-cli/src/lib.rs:301-309`), which is why the error the maintainer hit came
from `startup`'s own code rather than the gate. Replace that arm with:

```rust
        Commands::Startup(ref args) => {
            let (paths, home_is_new) = match ensure_home(&cli.global) {
                Ok(resolved) => resolved,
                Err(refusal) => {
                    let code = refusal.code();
                    emit_error_locked(fmt, code, &refusal.message());
                    return code;
                }
            };
            let _ = home_is_new; // bound in Task 4
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            return startup::startup(&mut streams, fmt, Some(paths.home.as_path()), args);
        }
```

`startup::startup`'s fourth parameter is already `Option<&Path>`, so passing
`Some(paths.home.as_path())` needs no signature change: the arm previously
passed `cli.global.home.as_deref()`, and the resolved home is the same value
whenever that was `Some`, plus the correct default whenever it was `None`.

Then delete the now-unreachable check at
`crates/shep-cli/src/commands/startup/mod.rs:234-243` — the whole
`if !plan.spec.home.is_dir() { return refuse(..) }` block. The gate has
already guaranteed the directory exists.

- [ ] **Step 7: Correct the `ShepPaths` doc this change makes untrue**

`crates/shep-core/src/paths.rs:10-11` currently reads:

```rust
/// All paths are derived from `$SHEP_HOME` (default `<home>/.shep`); nothing
/// here touches the filesystem — creation happens daemon-side.
```

The first clause stays true — this type still touches nothing. The second no
longer is: the CLI creates the root before any daemon exists. Replace those
two lines with:

```rust
/// All paths are derived from `$SHEP_HOME` (default `<home>/.shep`); nothing
/// here touches the filesystem. The root is created by the CLI's own
/// `ensure_home` for the commands that need it before a daemon exists
/// (`startup` above all), and everything under it by
/// `shep_daemon::boot::init_dirs` on each boot.
```

This is a one-crate-boundary hop for a doc comment only, and it is the kind of
stale claim this project has been bitten by: a reader trusting "creation
happens daemon-side" would go looking in the wrong crate.

- [ ] **Step 8: Run the crate's suite**

```bash
cargo test -p shep --lib --bins --all-features
```

Expected: PASS. If a `startup` test asserted the old
`"no directory at ... pass --home"` string, update it to assert the new
refusal instead — the message moved, the behaviour did not.

- [ ] **Step 9: Prove the original transcript is fixed**

```bash
cargo build -p shep --bin shep --all-features
```

```bash
SHEP_FAKE_HOME=$(mktemp -d) && HOME="$SHEP_FAKE_HOME" ./target/debug/shep startup --help >/dev/null && HOME="$SHEP_FAKE_HOME" ls -la "$SHEP_FAKE_HOME"
```

Expected: `.shep` present in the listing, mode `drwx------`. (`startup --help`
rather than `startup` so the check does not try to install an init unit.)

- [ ] **Step 10: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/lib.rs crates/shep-cli/src/commands/startup/mod.rs crates/shep-core/src/paths.rs
git commit -m "feat(shep): create the default \$SHEP_HOME, refuse a named one that is missing

\`~/.shep\` was only ever created on the way to a running daemon, so
\`shep startup\` — which installs a unit without starting anything — was the
one verb that needed it in advance and the only one that refused. The first
command a pm2 user types was the only one that failed.

\`ensure_home\` now creates the default home for any command and reports
whether it did. An explicitly named home is never created: the likeliest
reason a typed path is missing is a typo, and inventing it would leave a
second empty flock behind for the operator to lose their processes in.

\`startup\` bypassed the shared gate and carried its own check, which is why
its refusal read differently from everything else. It routes through the gate
now and the duplicate check is gone."
```

---

### Task 3: The welcome text

A module that owns the copy and nothing else, so the art has exactly one home
and one pinning test.

**Files:**
- Create: `crates/shep-cli/src/welcome.rs`
- Modify: `crates/shep-cli/src/lib.rs` (add `mod welcome;` beside the other
  module declarations)

**Interfaces:**
- Consumes: nothing.
- Produces, for Task 4: `pub(crate) fn render(home: &Path) -> String`.

- [ ] **Step 1: Write the failing test**

Create `crates/shep-cli/src/welcome.rs` containing only:

```rust
//! The first-run welcome: the art, the quick start, and the text around them.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Pinned whole, the way `docs/lookout/frames.txt` pins a rendered
    /// frame. Art drifts silently otherwise, and the one thing a welcome
    /// cannot afford is to look unmaintained.
    #[test]
    fn the_welcome_renders_exactly_this() {
        let rendered = render(Path::new("/home/ada/.shep"));
        let expected = format!(
            "\
      ,-~-.     ,-~-.     ,-~-.
     ( o.o )   ( o.o )   ( o.o )       shep {version}
      `-^-'     `-^-'     `-^-'        flock at /home/ada/.shep
       \" \"       \" \"       \" \"
    /\\  /\\
   ( o  o )--,   the shepherd keeps them running
    `--..--'  |
      |  |    '

Set up /home/ada/.shep. Logs, pids and the shepherd's socket live here.

Getting started
  shep start server.js    start it and keep it alive
  shep flock              see what's running
  shep bleats server      follow its output
  shep save               remember this flock across reboots
  shep startup            bring it back after a reboot

  shep welcome            show this again
",
            version = env!("CARGO_PKG_VERSION"),
        );
        assert_eq!(rendered, expected);
    }

    /// The path appears twice and both must follow `--home`, or a second
    /// flock's welcome would advertise the first flock's directory.
    #[test]
    fn the_home_path_is_substituted_everywhere_it_appears() {
        let rendered = render(Path::new("/srv/api"));
        assert_eq!(
            rendered.matches("/srv/api").count(),
            2,
            "both the art's caption and the prose line name the home:\n{rendered}"
        );
        assert!(!rendered.contains("~/.shep"), "no hardcoded default leaks through");
    }

    /// No em dashes in copy a user reads. Doc comments may have them; this
    /// may not.
    #[test]
    fn the_welcome_copy_has_no_em_dashes() {
        let rendered = render(Path::new("/home/ada/.shep"));
        assert!(!rendered.contains('\u{2014}'), "em dash in user-facing copy");
        assert!(!rendered.contains('\u{2013}'), "en dash in user-facing copy");
    }
}
```

Add `mod welcome;` to `crates/shep-cli/src/lib.rs` alongside the other `mod`
declarations.

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep --lib --all-features welcome
```

Expected: FAIL to compile, `cannot find function render in this scope`.

- [ ] **Step 3: Write the module**

Insert above the `mod tests` block in `crates/shep-cli/src/welcome.rs`:

```rust
use std::path::Path;

/// The art, with `{version}` and `{home}` substituted at render time.
///
/// Original work. Deliberately about a third the height of pm2's banner:
/// the point is to be seen once and not resented.
const ART: &str = "\
      ,-~-.     ,-~-.     ,-~-.
     ( o.o )   ( o.o )   ( o.o )       shep {version}
      `-^-'     `-^-'     `-^-'        flock at {home}
       \" \"       \" \"       \" \"
    /\\  /\\
   ( o  o )--,   the shepherd keeps them running
    `--..--'  |
      |  |    '
";

/// The five commands that get someone from nothing to a process that
/// survives a reboot.
///
/// Deliberately absent: `--home`, `fold`, a link, and anything about dogs or
/// the whistle. Those are `shep --help`'s job. A welcome that lists
/// everything teaches nothing.
const QUICK_START: &str = "\
Getting started
  shep start server.js    start it and keep it alive
  shep flock              see what's running
  shep bleats server      follow its output
  shep save               remember this flock across reboots
  shep startup            bring it back after a reboot

  shep welcome            show this again
";

/// Renders the welcome for one flock.
///
/// `home` appears twice — once as the art's caption, once in the prose line
/// under it — so a `--home` render names the flock it is actually about.
pub(crate) fn render(home: &Path) -> String {
    let home = home.display().to_string();
    let art = ART
        .replace("{version}", env!("CARGO_PKG_VERSION"))
        .replace("{home}", &home);
    format!("{art}\nSet up {home}. Logs, pids and the shepherd's socket live here.\n\n{QUICK_START}")
}
```

- [ ] **Step 4: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features welcome
```

Expected: PASS, 3 passed.

- [ ] **Step 5: Look at it**

```bash
cargo test -p shep --lib --all-features welcome::tests::the_welcome_renders_exactly_this -- --nocapture
```

Then read the rendered art once with your own eyes. An exact-string test
proves the code matches the test; it cannot tell you the sheep look like
sheep.

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/welcome.rs crates/shep-cli/src/lib.rs
git commit -m "feat(shep): add the first-run welcome text

One module owns the copy, so the art has a single home and a single pinning
test — the same discipline docs/lookout/frames.txt applies to a rendered
frame. Three tests: the whole thing exactly, the home path substituted in
both places it appears, and no em dashes in copy a user reads.

Nothing calls it yet."
```

---

### Task 4: `shep welcome`, and printing it on first run

**Files:**
- Modify: `crates/shep-cli/src/cli.rs` (add `Commands::Welcome`)
- Modify: `crates/shep-cli/src/welcome.rs` (add the two print entry points)
- Modify: `crates/shep-cli/src/lib.rs` (bind the `home_is_new` values Task 2
  left as `let _ =`, and dispatch the new verb)

**Interfaces:**
- Consumes: `welcome::render(&Path) -> String` (Task 3);
  `ensure_home(&GlobalArgs) -> Result<(ShepPaths, bool), HomeRefusal>` (Task 2);
  `output::emit<T: Render>(out, fmt, command, data) -> io::Result<()>`
  (existing, `crates/shep-cli/src/output/mod.rs:146`).
- Produces: nothing later tasks depend on.

- [ ] **Step 1: Write the failing tests**

Add to `crates/shep-cli/src/welcome.rs`'s `mod tests`:

```rust
    use crate::cli::Format;
    use crate::output::Streams;

    fn drain(f: impl FnOnce(&mut Streams<'_>)) -> (String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        f(&mut streams);
        (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    /// A fresh machine is exactly where a provisioning script runs first, so
    /// the side-effect welcome goes to stderr and leaves stdout for the
    /// command the operator actually ran.
    #[test]
    fn the_first_run_welcome_goes_to_stderr() {
        let (out, err) = drain(|s| on_first_run(s, Format::Table, Path::new("/home/ada/.shep"), true));
        assert!(out.is_empty(), "stdout must stay clean: {out}");
        assert!(err.contains("Getting started"), "stderr must carry it: {err}");
    }

    /// `shep start server.js | jq` on a cold box must not have a sheep in
    /// its input.
    #[test]
    fn the_first_run_welcome_is_suppressed_for_json_and_for_pipes() {
        let (_, json) = drain(|s| on_first_run(s, Format::Json, Path::new("/x"), true));
        assert!(json.is_empty(), "--format json must suppress it: {json}");

        let (_, piped) = drain(|s| on_first_run(s, Format::Table, Path::new("/x"), false));
        assert!(piped.is_empty(), "a non-terminal stderr must suppress it: {piped}");
    }

    /// Asked for by name, it is the command's output rather than a
    /// diagnostic, so it goes to stdout and no terminal check applies.
    #[test]
    fn the_welcome_verb_prints_to_stdout_even_when_piped() {
        let (out, err) = drain(|s| {
            welcome(s, Format::Table, Path::new("/home/ada/.shep"));
        });
        assert!(out.contains("Getting started"), "stdout must carry it: {out}");
        assert!(err.is_empty(), "nothing belongs on stderr here: {err}");
    }

    /// Every other verb answers `--format json` with an envelope. This one
    /// does too, rather than printing nothing and looking broken.
    #[test]
    fn the_welcome_verb_answers_json_with_an_envelope() {
        let (out, _) = drain(|s| {
            welcome(s, Format::Json, Path::new("/home/ada/.shep"));
        });
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["command"], "welcome");
        assert!(
            parsed["data"]["text"].as_str().unwrap().contains("Getting started"),
            "the envelope carries the text: {out}"
        );
    }
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features welcome
```

Expected: FAIL to compile, `cannot find function on_first_run`.

- [ ] **Step 3: Add the two entry points**

Append to `crates/shep-cli/src/welcome.rs`, above `mod tests`:

```rust
use std::io::Write;

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{OutputEnvelope, SCHEMA_VERSION, Streams};

/// The welcome as one JSON field, so `--format json` gets an envelope like
/// every other verb rather than silence.
///
/// Not a [`crate::output::Render`] impl: that trait is for tabular data and
/// requires `headers()` and `rows()`, which free-form text has neither of.
/// The envelope is built directly instead, from the same
/// [`OutputEnvelope`] and [`SCHEMA_VERSION`] every other verb goes through,
/// so the schema stays consistent without pretending this is a table.
#[derive(Debug, serde::Serialize)]
struct WelcomeData {
    text: String,
}

/// Prints the welcome as a side effect of whichever command created the home.
///
/// Suppressed under `--format json` and when stderr is not a terminal: a cold
/// machine is exactly where a provisioning script runs first, and a banner in
/// the middle of `shep start server.js | jq` is a bug. The home is still
/// created when the text is suppressed — suppression governs the output,
/// never the side effect.
///
/// `stderr_is_terminal` is a parameter rather than an `IsTerminal` call in
/// here for the same reason `commands::daemon::ansi_enabled` takes one: a
/// test writing into a `Vec` cannot otherwise reach this branch.
pub(crate) fn on_first_run(
    streams: &mut Streams<'_>,
    fmt: Format,
    home: &Path,
    stderr_is_terminal: bool,
) {
    if fmt == Format::Json || !stderr_is_terminal {
        return;
    }
    let _ = write!(streams.err, "{}", render(home));
}

/// `shep welcome`: the same text, asked for by name.
///
/// stdout rather than stderr, and no terminal check, because here the
/// welcome *is* the command's output. An explicit invocation outranks the
/// side-effect path, so a `shep welcome` that also happens to create the home
/// prints once, here.
pub(crate) fn welcome(streams: &mut Streams<'_>, fmt: Format, home: &Path) -> ExitCode {
    let text = render(home);
    let wrote = match fmt {
        Format::Table => write!(streams.out, "{text}"),
        Format::Json => {
            let envelope = OutputEnvelope {
                schema_version: SCHEMA_VERSION,
                command: "welcome",
                data: WelcomeData { text },
            };
            serde_json::to_writer(&mut *streams.out, &envelope)
                .map_err(std::io::Error::other)
                .and_then(|()| writeln!(streams.out))
        }
    };
    match wrote {
        Ok(()) => ExitCode::Success,
        Err(_) => ExitCode::Internal,
    }
}
```

- [ ] **Step 4: Add the verb**

In `crates/shep-cli/src/cli.rs`, add to the `Commands` enum, placed after
`Completions` so the declaration order matches the help group it lands in:

```rust
    /// Print the welcome: the sheep, and the five commands worth knowing
    Welcome,
```

- [ ] **Step 5: Dispatch it, and bind the first-run print**

In `crates/shep-cli/src/lib.rs`, replace the `let _ = home_is_new;` line that
Task 2 left in the shared gate with:

```rust
    if home_is_new {
        let mut err = std::io::stderr();
        let mut sink = std::io::sink();
        let mut streams = Streams {
            out: &mut sink,
            err: &mut err,
        };
        welcome::on_first_run(
            &mut streams,
            fmt,
            &paths.home,
            std::io::stderr().is_terminal(),
        );
    }
```

Do the same in the `Startup` arm, replacing its `let _ = home_is_new;` — but
place the call *before* `startup::startup` runs, so the welcome precedes the
unit-installed line rather than following it.

Then add the dispatch arm. `Welcome` needs `paths`, so it goes with the
post-gate arms rather than the early `Completions`/`Schema` group:

```rust
        Commands::Welcome => {
            let mut out = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            return welcome::welcome(&mut streams, fmt, &paths.home);
        }
```

Guard against printing twice: when `shep welcome` is itself the command that
created the home, the gate's `on_first_run` would fire and then the verb
would print again. Suppress the gate's copy for this one verb:

```rust
    if home_is_new && !matches!(cli.command, Commands::Welcome) {
```

Add `use std::io::IsTerminal;` to `lib.rs`'s imports.

- [ ] **Step 6: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --bins --all-features
```

Expected: PASS, including the four new welcome tests.

- [ ] **Step 7: Drive it by hand, both ways**

```bash
cargo build -p shep --bin shep --all-features
```

```bash
H=$(mktemp -d) && HOME="$H" ./target/debug/shep welcome
```

Expected: the art and quick start on stdout, naming `$H/.shep`.

```bash
H=$(mktemp -d) && HOME="$H" ./target/debug/shep welcome --format json | head -c 120
```

Expected: a JSON envelope beginning `{"schema_version"` and containing
`"command":"welcome"`.

```bash
H=$(mktemp -d) && HOME="$H" ./target/debug/shep flock 2>/dev/null
```

Expected: the flock output alone. The welcome went to stderr and was
discarded, proving stdout stays clean for a pipe.

- [ ] **Step 8: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/cli.rs crates/shep-cli/src/welcome.rs crates/shep-cli/src/lib.rs
git commit -m "feat(shep): print the welcome on first run, and add \`shep welcome\`

Fires once per \$SHEP_HOME, on whichever command creates it, and never again
for that home. \`shep welcome\` reprints on demand.

Three suppression rules, all protecting the same case: never under
\`--format json\`, never when stderr is not a terminal, and stderr rather than
stdout when it is a side effect. A provisioning script running
\`shep start server.js | jq\` on a cold machine gets clean output; a human at
a terminal still sees the sheep. The home is created either way — suppression
governs the text, not the side effect.

\`shep welcome\` itself prints to stdout with no terminal check, because there
the welcome is the command's output rather than a diagnostic, and answers
\`--format json\` with an envelope like every other verb."
```

---

### Task 5: `shep --help` — getting started, grouped verbs, demoted `--home`

**Files:**
- Modify: `crates/shep-cli/src/cli.rs` (the `#[command(..)]` block on `Cli`,
  and `GlobalArgs::home`)
- Test: `crates/shep-cli/src/cli.rs` (in the existing `mod tests`)

**Interfaces:**
- Consumes: Task 1's `//`-comment change to the same struct. **Run Task 1
  first**; both edit attributes on `Cli`.
- Produces: nothing.

clap 4.6 cannot group subcommands — `#[command(help_heading = ..)]` on a
subcommand variant does not compile, verified against 4.6.6. The command
section is therefore hand-written in a `help_template`, and a test keeps it
honest. `help_heading` on a *global argument* does work, and is what demotes
`--home`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/shep-cli/src/cli.rs`'s `mod tests`:

```rust
    /// Every verb is filed under exactly one heading, and every name under a
    /// heading is a real verb. A hand-written list rots the first time
    /// somebody adds a command; this is what stops it, the same way
    /// `docs/whistle/tools.md`'s catalogue test stops that list rotting.
    #[test]
    fn every_visible_verb_appears_in_exactly_one_help_group() {
        let command = Cli::command();
        let visible: Vec<String> = command
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .map(|s| s.get_name().to_string())
            .collect();

        let help = command.clone().render_long_help().to_string();
        let groups: Vec<&str> = HELP_GROUPS.iter().flat_map(|(_, verbs)| verbs.iter().copied()).collect();

        for verb in &visible {
            let filed = groups.iter().filter(|g| *g == verb).count();
            assert_eq!(
                filed, 1,
                "`{verb}` appears in {filed} help groups; it must appear in exactly one"
            );
            assert!(help.contains(verb.as_str()), "`{verb}` is missing from rendered help");
        }
        for name in &groups {
            assert!(
                visible.iter().any(|v| v == name),
                "help group names `{name}`, which is not a visible verb"
            );
        }
    }

    /// `--home` is plumbing, not a choice. It was the first global option
    /// anyone read, above `--format`.
    #[test]
    fn home_is_demoted_out_of_the_headline_options() {
        let help = Cli::command().render_long_help().to_string();
        let less_common = help.find("Less common").expect("a `Less common` heading");
        let home = help.find("--home").expect("--home still documented");
        assert!(home > less_common, "--home must sit under `Less common`:\n{help}");
    }

    /// The five commands that get someone to a reboot-surviving process.
    #[test]
    fn the_help_opens_with_a_worked_example() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Getting started"), "no getting-started block:\n{help}");
        assert!(help.contains("shep start server.js"), "no worked example:\n{help}");
    }
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features help
```

Expected: FAIL to compile, `cannot find value HELP_GROUPS in this scope`.

- [ ] **Step 3: Declare the groups**

Add to `crates/shep-cli/src/cli.rs`, above the `Cli` struct:

```rust
/// The verb groups `HELP_TEMPLATE` renders, and the source of truth the
/// drift test checks the real command tree against.
///
/// clap 4.6 has no subcommand grouping — `#[command(help_heading = ..)]` on a
/// subcommand variant does not compile — so the section is hand-written and
/// this table is what keeps it from rotting. Add a verb without filing it
/// here and `every_visible_verb_appears_in_exactly_one_help_group` fails.
const HELP_GROUPS: &[(&str, &[&str])] = &[
    ("Run things", &["start", "serve", "stop", "restart", "reload", "delete", "stock"]),
    ("See what's up", &["flock", "describe", "bleats", "lookout", "fold", "barks"]),
    ("Survive reboots", &["save", "muster", "startup", "unstartup"]),
    ("Talk to a sheep", &["trigger", "signal", "whisper"]),
    ("The shepherd", &["ping", "kill", "reopen", "flush", "set", "get", "unset"]),
    ("Dogs and agents", &["dogs", "enable", "disable", "adopt", "rehome", "whistle"]),
    ("Foreground runs", &["runtime", "dev"]),
    ("Coming from pm2", &["import"]),
    ("Help", &["welcome", "help", "completions"]),
];

/// `--help`'s shape. `{all-args}` still comes from clap, so the options
/// section and `--home`'s `Less common` heading stay generated.
const HELP_TEMPLATE: &str = "\
{about}

{usage-heading} {usage}

Getting started
  shep start server.js    start it and keep it alive
  shep flock              see what's running
  shep bleats server      follow its output
  shep save               remember this flock across reboots
  shep startup            bring it back after a reboot

Run things       start serve stop restart reload delete stock
See what's up    flock describe bleats lookout fold barks
Survive reboots  save muster startup unstartup
Talk to a sheep  trigger signal whisper
The shepherd     ping kill reopen flush set get unset
Dogs and agents  dogs enable disable adopt rehome whistle
Foreground runs  runtime dev
Coming from pm2  import
Help             welcome help completions

{all-args}{after-help}";
```

The template's group lines and `HELP_GROUPS` say the same thing twice, which
is the one duplication this design accepts: clap needs a literal string and
the test needs structured data. A fourth test guards the seam.

- [ ] **Step 4: Add the seam test**

```rust
    /// The template is a literal and `HELP_GROUPS` is structured data, so
    /// they can disagree. They may not.
    #[test]
    fn the_help_template_and_the_group_table_agree() {
        for (heading, verbs) in HELP_GROUPS {
            let line = HELP_TEMPLATE
                .lines()
                .find(|l| l.starts_with(heading))
                .unwrap_or_else(|| panic!("`{heading}` is missing from HELP_TEMPLATE"));
            for verb in *verbs {
                assert!(
                    line.split_whitespace().any(|w| w == *verb),
                    "`{verb}` is in HELP_GROUPS under `{heading}` but not on that line: {line}"
                );
            }
        }
    }
```

- [ ] **Step 5: Wire the template and demote `--home`**

In the `#[command(..)]` block on `Cli`, add `help_template = HELP_TEMPLATE`
and an `after_help`:

```rust
#[command(
    name = "shep",
    bin_name = "shep",
    version,
    about = "A process manager for your flock",
    propagate_version = true,
    help_template = HELP_TEMPLATE,
    after_help = "Run `shep help <command>` for one command, or `shep welcome` for the tour."
)]
```

Then change `GlobalArgs::home` (`crates/shep-cli/src/cli.rs:47-49`):

```rust
    /// Talk to a different shepherd
    ///
    /// Mostly plumbing: `shep dev` sessions, a system-wide flock, tests. You
    /// almost certainly want the default, ~/.shep.
    #[arg(long, global = true, env = "SHEP_HOME", help_heading = "Less common")]
    pub home: Option<PathBuf>,
```

- [ ] **Step 6: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features help
```

Expected: PASS, 4 passed.

- [ ] **Step 7: Read the real thing**

```bash
cargo build -p shep --bin shep --all-features && ./target/debug/shep --help
```

Expected: the about line, usage, the getting-started block, nine group lines,
the options section with `--home` under `Less common`, and the after-help
line. Confirm no verb is missing and nothing reads as an implementation note.

- [ ] **Step 8: Confirm the alias binaries still say `shep`**

Task 1 kept `bin_name` for a reason; this is the check that it still holds.

```bash
cargo build -p shep --bins --all-features && ./target/debug/shep-dev --help | grep -i usage
```

Expected: a usage line naming `shep dev`, not `shep-dev dev`.

- [ ] **Step 9: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/cli.rs
git commit -m "feat(shep): teach \`shep --help\` instead of listing 34 verbs alphabetically

A worked example at the top — the five commands that get someone from nothing
to a process that survives a reboot — then the verbs filed under task-shaped
headings rather than one alphabetical wall.

clap 4.6 cannot group subcommands (\`help_heading\` on a subcommand variant
does not compile, checked against 4.6.6), so the section is hand-written in a
\`help_template\`. Two tests keep it honest: one enumerates the real command
tree and fails if any visible verb is unfiled or any filed name is not a real
verb, the other pins the literal template against the same table.

\`--home\` moves under a \`Less common\` heading and its help says what it is
for. It is the daemon's data-root, not a choice: \`shep dev\` sessions, a
system-wide flock, tests. \`fold\` — the feature that actually answers \"how do
I organise this\" — now appears under \"See what's up\" instead of at verb 18
of an alphabetical list."
```

---

## Final verification

After Task 5, once:

```bash
cargo test --workspace --all-features
```

Expected: PASS. This is the only workspace-shaped command in the plan; it
runs once, at the end, because `shep-daemon`'s and `shep-core`'s suites
cannot be affected by a CLI-only change but the integration tier can.

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Expected: exit 0. Every new item in this plan carries docs; this is what
proves it.

## Self-review notes

Checked against the spec, 2026-08-17:

- **Spec §1 (home creation)** → Task 2, all three rows of its table.
- **Spec §1 (refusal text)** → Task 2 Step 3, `HomeRefusal::message`.
- **Spec §1 (no creation command)** → nothing to build; the refusal points at
  `mkdir -p`.
- **Spec §2 (when it fires, `shep welcome`, double-print case)** → Task 4
  Steps 4-5, including the `!matches!(cli.command, Commands::Welcome)` guard.
- **Spec §2 (three suppression rules)** → Task 4 Step 1, two tests.
- **Spec §2 (the text)** → Task 3, pinned exactly.
- **Spec §3 (leaked note)** → Task 1.
- **Spec §3 (layout, drift test)** → Task 5.
- **Spec §4 (testing)** → distributed; the `shep startup` regression is Task 2
  Step 8.

One spec refinement made while planning, worth the maintainer's eye at review: the spec
says the welcome is suppressed under `--format json`, written with the
side-effect case in mind. Task 4 makes `shep welcome --format json` emit a
normal envelope instead of nothing, since a verb invoked by name that prints
nothing reads as broken. The side-effect path still suppresses under JSON
exactly as specified.
