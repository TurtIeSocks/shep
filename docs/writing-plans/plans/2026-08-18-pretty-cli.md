# Pretty CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Box-drawn, coloured, sheep-bearing output for `shep`, with one
`shep style` dial to turn it down, and piped output byte-identical to today.

**Architecture:** Seven tasks in `crates/shep-cli` only. One new
platform-neutral module (`vocabulary.rs`) owns the shared role names, status
mapping and face glyphs; `output/table.rs` grows a boxed renderer that
measures visible width and drops columns by priority; `Streams` carries the
resolved style level to every command that already receives it.

**Tech Stack:** Rust 2024, MSRV 1.88, `anstyle` for styling, `crossterm` for
terminal width (unix only), `insta` for per-level snapshots, `proptest` for
the width invariant.

**Spec:** [docs/brainstorming/specs/2026-08-18-pretty-cli-design.md](../../brainstorming/specs/2026-08-18-pretty-cli-design.md)

## Global Constraints

- **Invoke the `shep-idiomatic-rust` skill before writing any Rust here.**
  Every new public item needs docs; `core::error::Error`, not `std`; every
  `Result`-returning pub fn needs `# Errors`.
- **Inner loop:** `cargo test -p shep --lib --bins --all-features`.
- **ONE cargo shape per task.** This plan is single-crate: `-p shep`
  throughout. The final verification is the only `--workspace` command.
- **Task gate, once per task:** `cargo fmt --all --check`, then
  `cargo clippy -p shep --all-targets --all-features -- -D warnings`, each
  from its own command with `$?` captured directly, never through a pipe.
- **Piped output and `--format json` are byte-identical to today.** No boxes,
  no colour, no faces, no adaptation. `cli_e2e` must pass UNCHANGED; it
  asserts exact stdout and is the real test of this rule.
- **User-facing copy contains no em dashes.** Doc comments may; strings a
  user reads may not.
- **A face or a status-to-role mapping defined outside `vocabulary.rs` is a
  review defect.** `theme.rs` and `output/` bind roles to colour types; they
  do not decide the vocabulary.
- **Never measure a styled string's length.** Padding is computed on plain
  text; style is applied after. This is the bug the whole plan guards.

---

## Verified facts

Checked against the manifests and cfg gates, not assumed. Three claims in the
spec's first draft were wrong in exactly this way.

| what | reality | consequence |
|---|---|---|
| `anstyle` | workspace table only, NOT in `shep-cli` | Task 1 adds it |
| `proptest` | workspace table + `shep-daemon` dev-deps, NOT `shep-cli` | Task 3 adds it |
| `crossterm` | in `shep-cli`, inside its `cfg(unix)` block | width has no source off-unix; the 80 fallback is unconditional |
| `insta` | already a `shep-cli` dev-dep | use it for per-level snapshots |
| `mod lookout` | `#[cfg(unix)]` (`lib.rs:42`) | `theme.rs` cannot host anything `output/` needs |
| `mod output` | unconditional | must compile on Windows |
| `emit()` | 38 call sites | Task 5's mechanical change |
| `render_table()` | 10 call sites, all inside `output/` | Task 4 |

## File Structure

| File | Responsibility | Task |
|---|---|---|
| `crates/shep-cli/src/vocabulary.rs` | **New.** Role names, status→role, face glyphs. Platform-neutral, no colour types. | 2 |
| `crates/shep-cli/src/output/width.rs` | **New.** Visible width of a possibly-styled string. | 3 |
| `crates/shep-cli/src/output/table.rs` | Boxed renderer, column priority, footer. | 4 |
| `crates/shep-cli/src/output/mod.rs` | `Streams` carries the style; `emit` reads it. | 5 |
| `crates/shep-cli/src/style.rs` | **New.** `StyleLevel`, resolution, `shep style`. | 1 |
| `crates/shep-cli/src/lookout/theme.rs` | Binds roles to ratatui colours; stops deciding them. | 2 |
| `crates/shep-cli/src/flourish.rs` | **New.** Sheep for empty/all-stopped/muster. | 6 |

Task order matters: **2 before 4** (the renderer needs the vocabulary),
**3 before 4** (the renderer needs width), **1 before 5** (Streams needs the
type), **5 before 6** (flourishes need the level).

---

### Task 1: `StyleLevel` and the `shep style` verb

**Files:**
- Create: `crates/shep-cli/src/style.rs`
- Modify: `crates/shep-cli/Cargo.toml` (add `anstyle.workspace = true`)
- Modify: `crates/shep-cli/src/cli.rs` (add `Commands::Style`)
- Modify: `crates/shep-core/src/config/daemon.rs` (a `[style]` section)

**Interfaces:**
- Produces:
  - `enum StyleLevel { Full, Plain, Bare }` — `Copy`, `PartialEq`, `Eq`, `Debug`
  - `enum StyleSource { Flag, Env, Config, Default }`
  - `fn resolve(flag: Option<StyleLevel>, env: Option<&str>, config: Option<StyleLevel>) -> (StyleLevel, StyleSource)`
  - `impl StyleLevel { fn sheep(self) -> bool; fn boxes(self) -> bool; fn colour(self) -> bool }`

- [ ] **Step 1: Write the failing test**

In `crates/shep-cli/src/style.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// First hit wins, and the source is reported: an operator whose env var
    /// and config file disagree needs to know which one won.
    #[test]
    fn the_flag_beats_the_env_beats_the_config_beats_the_default() {
        assert_eq!(
            resolve(Some(StyleLevel::Bare), Some("full"), Some(StyleLevel::Plain)),
            (StyleLevel::Bare, StyleSource::Flag)
        );
        assert_eq!(
            resolve(None, Some("bare"), Some(StyleLevel::Full)),
            (StyleLevel::Bare, StyleSource::Env)
        );
        assert_eq!(
            resolve(None, None, Some(StyleLevel::Plain)),
            (StyleLevel::Plain, StyleSource::Config)
        );
        assert_eq!(resolve(None, None, None), (StyleLevel::Full, StyleSource::Default));
    }

    /// An unreadable `$SHEP_STYLE` falls through rather than failing every
    /// command: a typo in a shell profile must not make shep unusable.
    #[test]
    fn an_unparseable_env_value_falls_through_to_the_next_source() {
        assert_eq!(
            resolve(None, Some("shiny"), Some(StyleLevel::Bare)),
            (StyleLevel::Bare, StyleSource::Config)
        );
    }

    /// The three levels are three answers to three questions, and `bare` is
    /// exactly what a pipe gets.
    #[test]
    fn each_level_answers_all_three_questions() {
        assert_eq!(
            (StyleLevel::Full.sheep(), StyleLevel::Full.boxes(), StyleLevel::Full.colour()),
            (true, true, true)
        );
        assert_eq!(
            (StyleLevel::Plain.sheep(), StyleLevel::Plain.boxes(), StyleLevel::Plain.colour()),
            (false, true, true)
        );
        assert_eq!(
            (StyleLevel::Bare.sheep(), StyleLevel::Bare.boxes(), StyleLevel::Bare.colour()),
            (false, false, false)
        );
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep --lib --all-features style::
```

Expected: FAIL to compile, `cannot find type StyleLevel in this scope`.

- [ ] **Step 3: Write the module**

Above the test module in `crates/shep-cli/src/style.rs`:

```rust
//! How much shep dresses up its output, and where that decision came from.

use std::fmt;

/// How much shep dresses up its output.
///
/// One dial rather than three switches. Colour, boxes and sheep are not
/// independent tastes in practice: someone who wants the sheep gone usually
/// wants a calmer table, and someone who wants today's output wants all of
/// it gone. `NO_COLOR` remains orthogonal because it is a cross-ecosystem
/// convention about colour alone, not about layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StyleLevel {
    /// Sheep, boxes and colour.
    Full,
    /// Boxes and colour, no sheep.
    Plain,
    /// Exactly what shep printed before any of this, and exactly what a pipe
    /// gets.
    Bare,
}

impl StyleLevel {
    /// Whether sheep appear at all.
    pub(crate) const fn sheep(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether tables are box-drawn.
    pub(crate) const fn boxes(self) -> bool {
        matches!(self, Self::Full | Self::Plain)
    }

    /// Whether anything is coloured. `NO_COLOR` can still veto this; it
    /// cannot enable it.
    pub(crate) const fn colour(self) -> bool {
        matches!(self, Self::Full | Self::Plain)
    }

    /// Parses one of the three level names, case-insensitively.
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "plain" => Some(Self::Plain),
            "bare" => Some(Self::Bare),
            _ => None,
        }
    }
}

impl fmt::Display for StyleLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full => "full",
            Self::Plain => "plain",
            Self::Bare => "bare",
        })
    }
}

/// Which layer decided the level in force.
///
/// Reported by `shep style` because the failure this prevents is an operator
/// editing `shep.toml` and seeing nothing change, with `$SHEP_STYLE` set in a
/// shell profile they have forgotten about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StyleSource {
    /// `--style` on this invocation.
    Flag,
    /// `$SHEP_STYLE`.
    Env,
    /// `[style] level` in `shep.toml`.
    Config,
    /// Nothing said otherwise.
    Default,
}

impl fmt::Display for StyleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Flag => "--style",
            Self::Env => "$SHEP_STYLE",
            Self::Config => "shep.toml",
            Self::Default => "the default",
        })
    }
}

/// Picks the level in force, and says which layer picked it.
///
/// An unparseable `$SHEP_STYLE` falls through to the next source rather than
/// failing: a typo in a shell profile must not make every shep command
/// unusable, and the level is a preference rather than a correctness input.
pub(crate) fn resolve(
    flag: Option<StyleLevel>,
    env: Option<&str>,
    config: Option<StyleLevel>,
) -> (StyleLevel, StyleSource) {
    if let Some(level) = flag {
        return (level, StyleSource::Flag);
    }
    if let Some(level) = env.and_then(StyleLevel::parse) {
        return (level, StyleSource::Env);
    }
    if let Some(level) = config {
        return (level, StyleSource::Config);
    }
    (StyleLevel::Full, StyleSource::Default)
}
```

Add `mod style;` to `crates/shep-cli/src/lib.rs` beside the other module
declarations, and `anstyle.workspace = true` to `crates/shep-cli/Cargo.toml`
under `[dependencies]`.

- [ ] **Step 4: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features style::
```

Expected: PASS, 3 passed.

- [ ] **Step 5: Add the `[style]` section to the config**

In `crates/shep-core/src/config/daemon.rs`, beside `WhistleSection`:

```rust
/// The `[style]` section: how much the CLI dresses up its output.
///
/// Read by the CLI only. The daemon has no opinion about how anyone likes
/// their tables, and parses this solely so an unknown key is not an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StyleSection {
    /// `full`, `plain` or `bare`. Absent means the CLI decides.
    pub level: Option<String>,
}
```

Add `pub style: StyleSection,` to `DaemonConfig` beside `whistle`.

- [ ] **Step 6: Add the verb**

In `crates/shep-cli/src/cli.rs`, after `Commands::Welcome`:

```rust
    /// Show or set how much shep dresses up its output
    ///
    /// `full` is sheep, boxes and colour; `plain` drops the sheep; `bare` is
    /// plain text. With no level, prints the one in force and where it came
    /// from.
    Style(StyleArgs),
```

And the args struct beside the others:

```rust
/// Arguments to `shep style`.
#[derive(Debug, clap::Args)]
pub struct StyleArgs {
    /// `full`, `plain`, or `bare`. Omit to show the level in force.
    pub level: Option<String>,
}
```

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/style.rs crates/shep-cli/src/lib.rs crates/shep-cli/src/cli.rs crates/shep-cli/Cargo.toml crates/shep-core/src/config/daemon.rs
git commit -m "feat(shep): add StyleLevel and the shep style verb

One dial with three levels rather than three switches for colour, boxes and
sheep: they are not independent tastes in practice. NO_COLOR stays
orthogonal, because it is a convention about colour alone.

An unparseable \$SHEP_STYLE falls through to the next source rather than
failing. A typo in a shell profile must not make every shep command
unusable, and the level is a preference, not a correctness input.

\`shep style\` with no argument reports the source as well as the level: the
failure this prevents is editing shep.toml, seeing nothing change, and not
knowing a forgotten \$SHEP_STYLE is winning."
```

---

### Task 2: `vocabulary.rs` — one source of truth for faces and roles

**Files:**
- Create: `crates/shep-cli/src/vocabulary.rs`
- Modify: `crates/shep-cli/src/lookout/theme.rs` (bind roles instead of deciding them)

**Interfaces:**
- Produces:
  - `enum Role { Meadow, Butter, Bark, Ink3 }` — `Copy`, `PartialEq`, `Eq`, `Debug`
  - `fn role_of(status: ProcStatus) -> Role`
  - `fn face(status: ProcStatus) -> &'static str`

Platform-neutral by necessity: `mod lookout` is `#[cfg(unix)]` while
`mod output` is not, so anything both need cannot live in `theme.rs`.

- [ ] **Step 1: Write the failing test**

In `crates/shep-cli/src/vocabulary.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Every status has a face and a role. A `match` would catch a missing
    /// arm at compile time; this catches a face that is empty or the wrong
    /// width, which compiles fine and looks broken.
    #[test]
    fn every_status_has_a_five_column_face() {
        for status in [
            ProcStatus::Online,
            ProcStatus::Starting,
            ProcStatus::WaitingRestart,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
        ] {
            let face = face(status);
            assert_eq!(
                face.chars().count(),
                5,
                "{status} face {face:?} must be 5 columns; the table budget assumes it"
            );
            assert!(face.is_ascii(), "{status} face {face:?} must be ASCII: an emoji is \
                 double-width, inconsistently so, and cannot take a colour");
        }
    }

    /// The mapping is the one `lookout` already shipped. Changing it here
    /// changes both renderings, which is the point of this module existing.
    #[test]
    fn the_roles_match_what_lookout_already_showed() {
        assert_eq!(role_of(ProcStatus::Online), Role::Meadow);
        assert_eq!(role_of(ProcStatus::Starting), Role::Butter);
        assert_eq!(role_of(ProcStatus::WaitingRestart), Role::Butter);
        assert_eq!(role_of(ProcStatus::Errored), Role::Bark);
        assert_eq!(role_of(ProcStatus::Stopping), Role::Ink3);
        assert_eq!(role_of(ProcStatus::Stopped), Role::Ink3);
    }

    /// A sleeping sheep and a startled one must not look the same at a
    /// glance, or the face carries nothing the colour did not.
    #[test]
    fn the_faces_are_distinct_from_one_another() {
        let faces = [
            face(ProcStatus::Online),
            face(ProcStatus::Starting),
            face(ProcStatus::Stopped),
            face(ProcStatus::Errored),
        ];
        let mut seen = faces;
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), faces.len(), "each state needs its own face: {faces:?}");
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep --lib --all-features vocabulary::
```

Expected: FAIL to compile, `cannot find function face in this scope`.

- [ ] **Step 3: Write the module**

```rust
//! What the two flock renderings share: role names, the status mapping, and
//! the faces.
//!
//! `shep flock` renders a table through `output/`, and `shep lookout`
//! renders one through ratatui. They must agree about what `online` looks
//! like, and they cannot share code: their colour types come from different
//! crates, and `mod lookout` is `#[cfg(unix)]` while `mod output` is not.
//!
//! So this module owns the vocabulary and neither renderer decides any of
//! it. Each binds [`Role`] to its own colour type -- `theme.rs` to ratatui's
//! `Color`, `output/` to `anstyle::Style`. A face or a mapping decided
//! anywhere but here is a review defect.

use shep_core::status::ProcStatus;

/// A colour role, named for the meadow rather than for the colour, so the
/// 256-colour and 16-colour tiers can differ without renaming anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Healthy: a sheep that is up.
    Meadow,
    /// In between: coming up, or waiting to.
    Butter,
    /// Wrong: errored.
    Bark,
    /// Quiet: stopped, stopping, and every muted chrome element.
    Ink3,
}

/// Which role a status wears.
///
/// Lifted verbatim from `lookout/theme.rs`'s own `status()`, which shipped
/// first. Both renderers now read this, so they agree by construction rather
/// than by two people remembering the same thing.
pub(crate) const fn role_of(status: ProcStatus) -> Role {
    match status {
        ProcStatus::Online => Role::Meadow,
        ProcStatus::Starting | ProcStatus::WaitingRestart => Role::Butter,
        ProcStatus::Errored => Role::Bark,
        ProcStatus::Stopping | ProcStatus::Stopped => Role::Ink3,
    }
}

/// The sheep wearing that status.
///
/// Five columns each, ASCII, and mutually distinct -- all three pinned by
/// this module's tests. Five because the table's column budget assumes it;
/// ASCII because an emoji is double-width (inconsistently across terminals)
/// and cannot take a foreground colour, which would make the width maths
/// guesswork; distinct because a face that only differs by colour tells a
/// `NO_COLOR` reader nothing.
pub(crate) const fn face(status: ProcStatus) -> &'static str {
    match status {
        ProcStatus::Online => "(o.o)",
        ProcStatus::Starting | ProcStatus::WaitingRestart => "(o~o)",
        ProcStatus::Stopping | ProcStatus::Stopped => "(-.-)",
        ProcStatus::Errored => "(x.x)",
    }
}
```

Add `mod vocabulary;` to `crates/shep-cli/src/lib.rs`.

- [ ] **Step 4: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features vocabulary::
```

Expected: PASS, 3 passed.

- [ ] **Step 5: Make `theme.rs` bind rather than decide**

Replace `status()`'s body in `crates/shep-cli/src/lookout/theme.rs:103-111`:

```rust
    pub fn status(self, status: ProcStatus) -> Style {
        // The mapping lives in `crate::vocabulary`, so the CLI's table and
        // this pane cannot drift. This method is now the ratatui BINDING of
        // it, and nothing more.
        Self::fg(match crate::vocabulary::role_of(status) {
            crate::vocabulary::Role::Meadow => self.meadow,
            crate::vocabulary::Role::Butter => self.butter,
            crate::vocabulary::Role::Bark => self.bark,
            crate::vocabulary::Role::Ink3 => self.ink3,
        })
    }
```

- [ ] **Step 6: Confirm the TUI is unchanged**

```bash
cargo test -p shep --lib --all-features lookout::
```

Expected: PASS with no failures. `theme.rs`'s existing tests assert the
status colours directly; they must still pass, which is what proves the
refactor changed no behaviour.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/vocabulary.rs crates/shep-cli/src/lib.rs crates/shep-cli/src/lookout/theme.rs
git commit -m "feat(shep): one vocabulary for both flock renderings

\`shep flock\` and \`shep lookout\` both show a flock and must agree about what
online looks like. They cannot share code: their colour types come from
different crates, and \`mod lookout\` is cfg(unix) while \`mod output\` is not.

So \`vocabulary.rs\` owns the role names, the status mapping and the faces,
and each renderer binds roles to its own colour type. \`theme.rs\`'s status()
is now that binding and nothing more; its existing colour assertions still
pass, which is what proves the refactor changed no behaviour.

Faces are five ASCII columns and mutually distinct, all three pinned. Five
because the column budget assumes it, ASCII because an emoji is double-width
and cannot take a colour, distinct because a face differing only by colour
tells a NO_COLOR reader nothing."
```

---

### Task 3: visible width

**Files:**
- Create: `crates/shep-cli/src/output/width.rs`
- Modify: `crates/shep-cli/Cargo.toml` (add `proptest.workspace = true` to `[dev-dependencies]`)

**Interfaces:**
- Produces: `fn visible_width(s: &str) -> usize`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this module exists for: a styled cell is 15 bytes and 5
    /// columns, and padding it by `len()` pushes every later border right.
    #[test]
    fn a_styled_string_measures_its_visible_width_not_its_bytes() {
        let styled = "\u{1b}[32m(o.o)\u{1b}[0m";
        assert_eq!(styled.len(), 14, "the raw string really is longer");
        assert_eq!(visible_width(styled), 5);
        assert_eq!(visible_width("(o.o)"), 5);
    }

    /// Several escapes in one cell, and one at each end.
    #[test]
    fn every_escape_in_a_string_is_discounted() {
        assert_eq!(visible_width("\u{1b}[1m\u{1b}[32mup\u{1b}[0m"), 2);
        assert_eq!(visible_width("\u{1b}[0m"), 0);
        assert_eq!(visible_width(""), 0);
    }

    /// Non-ASCII names are real: a table that miscounts them misaligns for
    /// the people least able to work around it.
    #[test]
    fn non_ascii_text_counts_characters() {
        assert_eq!(visible_width("café"), 4);
        assert_eq!(visible_width("日本"), 2, "counted as chars, not bytes");
    }
}
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p shep --lib --all-features width::
```

Expected: FAIL to compile, `cannot find function visible_width`.

- [ ] **Step 3: Write the module**

```rust
//! How wide a string looks, as opposed to how long it is.

/// Columns `s` occupies once ANSI escapes are discounted.
///
/// A styled cell is `\x1b[32m(o.o)\x1b[0m`: 14 bytes, 5 columns. Padding it
/// by `len()` or even by `chars().count()` pushes every border after it to
/// the right, and the table looks broken in a way that is hard to attribute.
/// Three hand-drawn mockups during this feature's design made exactly this
/// mistake.
///
/// Counts characters rather than grapheme clusters or east-asian width.
/// That is a deliberate floor, not an oversight: shep names are operator-
/// chosen identifiers, the alternative is a `unicode-width` dependency for
/// a case nobody has hit, and the property test in `table.rs` will catch it
/// the moment someone does.
pub(crate) fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // A CSI sequence runs to its final byte in @..~; anything else
            // after ESC is a two-character sequence. Both are zero-width.
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}
```

Add `mod width;` to `crates/shep-cli/src/output/mod.rs`, and
`proptest.workspace = true` to `crates/shep-cli/Cargo.toml`'s
`[dev-dependencies]`.

- [ ] **Step 4: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features width::
```

Expected: PASS, 3 passed.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/output/width.rs crates/shep-cli/src/output/mod.rs crates/shep-cli/Cargo.toml
git commit -m "feat(shep): measure a cell's visible width, not its length

A styled cell is 14 bytes and 5 columns. Padding by len() pushes every border
after it right, and the result looks broken in a way that is hard to
attribute to its cause -- three hand-drawn mockups during this feature's
design made exactly this mistake.

Counts characters rather than graphemes or east-asian width, deliberately:
shep names are operator-chosen identifiers, the alternative is a new
dependency for a case nobody has hit, and the table's property test will
catch it the moment someone does."
```

---

### Task 4: the boxed renderer

**Files:**
- Modify: `crates/shep-cli/src/output/table.rs`

**Interfaces:**
- Consumes: `visible_width` (Task 3); `StyleLevel` (Task 1).
- Produces:
  - `fn render_boxed(headers: &[&str], rows: &[Vec<String>], priorities: &[u8], term_width: usize) -> String`

`priorities` is parallel to `headers`; 0 never drops. The floor is three
columns.

- [ ] **Step 1: Write the failing tests**

```rust
    /// The invariant the whole feature rests on. Any rows, any width, any
    /// mix of styled and plain cells: every line is the same visible width,
    /// and it fits.
    #[test]
    fn every_line_of_a_boxed_table_has_the_same_visible_width() {
        use proptest::prelude::*;

        proptest!(|(
            cells in proptest::collection::vec(
                proptest::collection::vec("[a-z(). -]{0,12}", 3..6), 0..5),
            term in 20usize..200,
        )| {
            let headers = ["ID", "NAME", "STATUS", "PID", "MEM"];
            let n = cells.first().map_or(3, Vec::len);
            let headers = &headers[..n];
            let priorities: Vec<u8> = (0..n).map(|i| u8::try_from(i).unwrap_or(u8::MAX)).collect();
            let out = render_boxed(headers, &cells, &priorities, term);

            let widths: Vec<usize> = out.lines().map(crate::output::width::visible_width).collect();
            if let Some(first) = widths.first() {
                prop_assert!(
                    widths.iter().all(|w| w == first),
                    "ragged table at term={term}: widths {widths:?}\n{out}"
                );
            }
        });
    }

    /// Columns drop by priority until the table fits, and the floor is the
    /// three that identify a sheep.
    #[test]
    fn columns_drop_by_priority_and_never_below_three() {
        let headers = ["ID", "NAME", "STATUS", "PID", "FOLD"];
        let rows = vec![vec![
            "0".into(),
            "zeus-auth".into(),
            "(o.o) online".into(),
            "24963".into(),
            "backend".into(),
        ]];
        let priorities = [0, 0, 0, 2, 6];

        let wide = render_boxed(&headers, &rows, &priorities, 200);
        assert!(wide.contains("FOLD"), "everything fits at 200:\n{wide}");

        let narrow = render_boxed(&headers, &rows, &priorities, 46);
        assert!(!narrow.contains("FOLD"), "FOLD drops first:\n{narrow}");
        assert!(narrow.contains("NAME"), "identity columns survive:\n{narrow}");
        assert!(narrow.contains("hidden"), "and the footer says so:\n{narrow}");

        let tiny = render_boxed(&headers, &rows, &priorities, 10);
        for keep in ["ID", "NAME", "STATUS"] {
            assert!(tiny.contains(keep), "{keep} is a floor column:\n{tiny}");
        }
    }

    /// A dropped column is named, so nothing vanishes silently.
    #[test]
    fn the_footer_names_every_column_it_hid() {
        let headers = ["ID", "NAME", "STATUS", "CPU", "FOLD"];
        let rows = vec![vec!["0".into(), "a".into(), "(o.o)".into(), "0%".into(), "b".into()]];
        let out = render_boxed(&headers, &rows, &[0, 0, 0, 5, 6], 30);
        let footer = out.lines().last().unwrap();
        assert!(footer.contains("CPU"), "{footer}");
        assert!(footer.contains("FOLD"), "{footer}");
        assert!(footer.contains("--format json"), "and the way to see them: {footer}");
    }
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features table::
```

Expected: FAIL to compile, `cannot find function render_boxed`.

- [ ] **Step 3: Write the renderer**

Append to `crates/shep-cli/src/output/table.rs`:

```rust
/// Columns that identify a sheep, and so are never dropped.
const FLOOR_COLUMNS: usize = 3;

/// Renders `rows` as a box-drawn table that fits `term_width`.
///
/// Columns are dropped by descending priority until the table fits, never
/// below [`FLOOR_COLUMNS`] -- a table that cannot say which sheep a row is
/// about has stopped being a table. What was dropped is named in a footer,
/// because a column that vanishes silently is worse than one that is
/// missing loudly.
///
/// Every width is computed with [`crate::output::width::visible_width`], so
/// a styled cell pads by what it shows rather than by what it stores. The
/// property test above is the real specification of this function.
pub(crate) fn render_boxed(
    headers: &[&str],
    rows: &[Vec<String>],
    priorities: &[u8],
    term_width: usize,
) -> String {
    let mut keep: Vec<usize> = (0..headers.len()).collect();
    let mut dropped: Vec<&str> = Vec::new();

    loop {
        let widths = column_widths(headers, rows, &keep);
        let total: usize = widths.iter().map(|w| w + 3).sum::<usize>() + 1;
        if total <= term_width || keep.len() <= FLOOR_COLUMNS {
            break;
        }
        let worst = keep
            .iter()
            .enumerate()
            .max_by_key(|(_, &col)| priorities.get(col).copied().unwrap_or(0))
            .map(|(at, _)| at);
        let Some(at) = worst else { break };
        if priorities.get(keep[at]).copied().unwrap_or(0) == 0 {
            break;
        }
        dropped.push(headers[keep[at]]);
        keep.remove(at);
    }

    let widths = column_widths(headers, rows, &keep);
    let rule = |left: &str, mid: &str, right: &str| {
        let mut line = String::from(left);
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                line.push_str(mid);
            }
            line.push_str(&"─".repeat(w + 2));
        }
        line.push_str(right);
        line.push('\n');
        line
    };

    let mut out = rule("┌", "┬", "┐");
    out.push_str(&boxed_row(
        &keep.iter().map(|&c| headers[c].to_string()).collect::<Vec<_>>(),
        &widths,
    ));
    out.push_str(&rule("├", "┼", "┤"));
    for row in rows {
        out.push_str(&boxed_row(
            &keep.iter().map(|&c| row.get(c).cloned().unwrap_or_default()).collect::<Vec<_>>(),
            &widths,
        ));
    }
    out.push_str(&rule("└", "┴", "┘"));

    if !dropped.is_empty() {
        dropped.sort_unstable();
        out.push_str(&format!(
            "  {} hidden. Widen the window, or use --format json.\n",
            dropped.join(", ")
        ));
    }
    out
}

/// The visible width each kept column needs.
fn column_widths(headers: &[&str], rows: &[Vec<String>], keep: &[usize]) -> Vec<usize> {
    keep.iter()
        .map(|&col| {
            let mut w = crate::output::width::visible_width(headers[col]);
            for row in rows {
                if let Some(cell) = row.get(col) {
                    w = w.max(crate::output::width::visible_width(cell));
                }
            }
            w
        })
        .collect()
}

/// One `│ a │ b │` row, padded on visible width.
fn boxed_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::from("│");
    for (cell, w) in cells.iter().zip(widths) {
        let pad = w.saturating_sub(crate::output::width::visible_width(cell));
        line.push(' ');
        line.push_str(cell);
        line.push_str(&" ".repeat(pad));
        line.push_str(" │");
    }
    line.push('\n');
    line
}
```

- [ ] **Step 4: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features table::
```

Expected: PASS. The property test runs 256 cases by default; a failure
prints the minimal shrunk input, which is the whole reason it is a property
test rather than three examples.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/output/table.rs
git commit -m "feat(shep): a box-drawn table that fits the terminal

Columns drop by descending priority until the table fits, never below the
three that identify a sheep -- a table that cannot say which sheep a row is
about has stopped being a table. What was dropped is named in a footer,
because a column that vanishes silently is worse than one missing loudly.

Every width is computed on visible width, so a styled cell pads by what it
shows rather than by what it stores. The property test is the real
specification: any rows, any terminal width, any mix of styled and plain
cells, every line the same width and inside the terminal."
```

---

### Task 5: thread the level through `Streams`

**Files:**
- Modify: `crates/shep-cli/src/output/mod.rs` (`Streams` gains `style`; `emit` reads it)
- Modify: every `emit(` call site (38) and every `Streams {` construction

**Interfaces:**
- Consumes: `StyleLevel` (Task 1), `render_boxed` (Task 4).
- Produces: `Streams { out, err, style }`; `emit` renders boxed when
  `style.boxes()`.

`Streams` is already threaded to every command, which is why it carries this
rather than a 38-parameter change or a global. The codebase's own idiom is
terminal-ness as a parameter, never a call inside the function
(`commands/daemon.rs:187`), and a global would break that.

- [ ] **Step 1: Add the field, defaulting to `Bare`**

In `crates/shep-cli/src/output/mod.rs`:

```rust
pub struct Streams<'a> {
    #[cfg_attr(windows, allow(dead_code))]
    pub out: &'a mut dyn io::Write,
    pub err: &'a mut dyn io::Write,
    /// How much this invocation dresses up its output.
    ///
    /// Carried here rather than passed to `emit` because `Streams` already
    /// reaches every command, and a global would break this crate's rule
    /// that presentation inputs are parameters.
    ///
    /// `Bare` is the default so that any construction which forgets to set
    /// it renders exactly what shep printed before this feature -- the safe
    /// direction to fail, and what a pipe wants anyway.
    pub style: crate::style::StyleLevel,
}
```

- [ ] **Step 2: Let the compiler find every construction**

```bash
cargo build -p shep --all-features 2>&1 | grep -c 'missing `style`'
```

Expected: a count of every `Streams { .. }` literal. Add
`style: crate::style::StyleLevel::Bare,` to each **test** construction, and
the resolved level to each production one in `lib.rs`.

- [ ] **Step 3: Make `emit` choose**

In `emit`'s `Format::Table` arm:

```rust
        Format::Table => {
            if style.boxes() {
                write!(out, "{}", render_boxed(
                    T::headers(),
                    &data.rows(),
                    T::PRIORITIES,
                    terminal_width(),
                ))
            } else {
                write!(out, "{}", render_table(&data))
            }
        }
```

`emit`'s signature gains `style: StyleLevel`, and the 38 call sites pass
`streams.style`. `Render` gains:

```rust
    /// Per-column drop priority, parallel to [`Self::headers`]. `0` never
    /// drops. A row type that does not care may return `&[]`, which the
    /// renderer reads as all-zero and so never drops anything.
    const PRIORITIES: &'static [u8] = &[];
```

- [ ] **Step 4: Terminal width, with an unconditional fallback**

In `crates/shep-cli/src/output/mod.rs`:

```rust
/// The terminal's width, or 80 when there is not one.
///
/// `crossterm` is a `shep-cli` dependency only inside its `cfg(unix)` block
/// -- deliberately, so a Windows build does not link a terminal stack it can
/// never use -- so the fallback is unconditional rather than an error path.
fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        crossterm::terminal::size().map_or(80, |(w, _)| usize::from(w))
    }
    #[cfg(not(unix))]
    {
        80
    }
}
```

- [ ] **Step 5: Resolve the level once, in `run_argv`**

In `crates/shep-cli/src/lib.rs`, after the parse and before dispatch: read
`--style`, `$SHEP_STYLE` and the config, call `style::resolve`, and force
`StyleLevel::Bare` when `!std::io::stdout().is_terminal()` or
`fmt == Format::Json`. That forcing is the hard rule from the spec, applied
in exactly one place.

- [ ] **Step 6: The hard rule's test**

```bash
cargo test -p shep --test cli_e2e --all-features
```

Expected: PASS, 56 passed, **unchanged**. `cli_e2e` asserts exact stdout and
runs shep as a subprocess with pipes, so a border or an escape reaching
piped output fails it. This is the test; no new one is needed.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/
git commit -m "feat(shep): carry the style level on Streams

Streams already reaches every command, so it carries the level rather than
38 call sites gaining a parameter or the crate gaining a global. A global
would break this crate's own rule that presentation inputs are parameters,
which is why ansi_enabled takes a bool instead of calling IsTerminal.

The level is forced to Bare in exactly one place -- when stdout is not a
terminal, or --format json is set -- so the hard rule lives at one seam
rather than being remembered at 38.

Bare is also the field's default, so a construction that forgets to set it
renders what shep printed before this feature. That is the safe direction to
fail.

cli_e2e passes unchanged, which is the actual test of the rule: it asserts
exact stdout through a pipe."
```

---

### Task 6: the sheep

**Files:**
- Create: `crates/shep-cli/src/flourish.rs`
- Modify: `crates/shep-cli/src/commands/query.rs` (empty and all-stopped flock)
- Modify: `crates/shep-cli/src/commands/muster.rs` (the milestone)

**Interfaces:**
- Consumes: `StyleLevel::sheep()` (Task 1).
- Produces:
  - `fn empty_flock() -> String`
  - `fn all_asleep(count: usize) -> String`
  - `fn mustered(count: usize) -> String`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Capped, so `shep muster` over forty processes does not paint a field.
    #[test]
    fn a_flourish_never_shows_more_than_five_sheep() {
        for n in [1, 2, 5, 6, 40, 400] {
            let art = mustered(n);
            let sheep = art.matches("(o.o)").count();
            assert!(sheep <= 5, "{n} sheep rendered {sheep} faces:\n{art}");
            assert!(sheep >= 1, "at least one, even for {n}:\n{art}");
        }
    }

    /// The empty state exists to answer "what now", so it must say.
    #[test]
    fn the_empty_flock_names_the_next_command() {
        let art = empty_flock();
        assert!(art.contains("shep start"), "{art}");
    }

    /// No em dashes in copy a user reads.
    #[test]
    fn the_flourishes_carry_no_em_dashes() {
        for art in [empty_flock(), all_asleep(3), mustered(2)] {
            assert!(!art.contains('\u{2014}'), "em dash in {art:?}");
            assert!(!art.contains('\u{2013}'), "en dash in {art:?}");
        }
    }

    /// Every line inside 80 columns, like the welcome.
    #[test]
    fn the_flourishes_fit_an_eighty_column_terminal() {
        for art in [empty_flock(), all_asleep(5), mustered(5)] {
            for line in art.lines() {
                assert!(line.chars().count() <= 80, "line too wide: {line:?}");
            }
        }
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p shep --lib --all-features flourish::
```

Expected: FAIL to compile, `cannot find function mustered`.

- [ ] **Step 3: Write the module**

Sheep are built from `vocabulary::face` so a face change reaches the art
too. Cap at five. No `\` line-continuation before an indented first line --
that strips the indentation and silently unaligns the leftmost sheep, which
happened to `welcome.rs` and no exact-string test could see it.

```rust
//! Sheep for the moments with nothing else to look at.
//!
//! Three places only: an empty flock, a flock entirely stopped, and
//! `shep muster`. Never on an error and never after a destructive verb --
//! `docs/terminology.md`'s rule is that the theme never costs clarity, and a
//! sheep beside `error[not_found]` makes a failure harder to read.

use shep_core::status::ProcStatus;

use crate::vocabulary::face;

/// The most sheep any flourish draws.
const MOST: usize = 5;

/// A row of `count` faces, capped, indented four columns.
fn row(status: ProcStatus, count: usize) -> String {
    let face = face(status);
    let n = count.clamp(1, MOST);
    let mut line = String::from("    ");
    for _ in 0..n {
        line.push_str(face);
    }
    line
}

/// Nothing registered yet.
pub(crate) fn empty_flock() -> String {
    format!(
        "\n{}   no sheep in the flock yet\n       `shep start <script>` adds one\n",
        row(ProcStatus::Stopped, 1)
    )
}

/// Registered, none running.
pub(crate) fn all_asleep(count: usize) -> String {
    format!(
        "\n{}   {count} in the flock, all asleep\n       `shep start <name>` wakes one\n",
        row(ProcStatus::Stopped, count)
    )
}

/// The flock is back.
pub(crate) fn mustered(count: usize) -> String {
    format!(
        "\n{}   {count} back on their feet\n",
        row(ProcStatus::Online, count)
    )
}
```

Add `mod flourish;` to `crates/shep-cli/src/lib.rs`.

- [ ] **Step 4: Run the tests and watch them pass**

```bash
cargo test -p shep --lib --all-features flourish::
```

Expected: PASS, 4 passed.

- [ ] **Step 5: Wire the three sites, gated on `style.sheep()`**

In `query.rs`'s flock rendering: when the listing is empty, print
`flourish::empty_flock()`; when it is non-empty and every entry is
`Stopped`, print `flourish::all_asleep(n)`. In `muster.rs`: after a
successful restore of `n > 0`, print `flourish::mustered(n)`. Each guarded
by `streams.style.sheep()`, each to `streams.out`, and none of them
replacing the table -- the flourish is in addition, not instead.

- [ ] **Step 6: Look at all three**

```bash
cargo build -p shep --bin shep --all-features
```

```bash
H=$(mktemp -d) && HOME="$H" ./target/debug/shep flock
```

Expected: the empty-flock sheep, then an empty table. **Read it.** An
exact-string test proves the code matches the test; it cannot tell you the
sheep look like sheep, which is the lesson `welcome.rs`'s unaligned leftmost
sheep taught.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/flourish.rs crates/shep-cli/src/lib.rs crates/shep-cli/src/commands/
git commit -m "feat(shep): sheep for the moments with nothing else to look at

Three places only: an empty flock, a flock entirely stopped, and shep muster.
Capped at five, so a muster over forty processes does not paint a field.

Never on an error and never after a destructive verb. docs/terminology.md
already sets that rule -- the theme never costs clarity -- and a sheep beside
error[not_found] makes a failure harder to read and reads as flippant to
someone debugging at 2am.

Built from vocabulary::face, so changing a face changes the art too."
```

---

### Task 7: pin every level

**Files:**
- Modify: `crates/shep-cli/src/output/table.rs` (snapshot tests)
- Create: `crates/shep-cli/src/snapshots/` (insta output)

- [ ] **Step 1: Write the snapshot tests**

```rust
    /// Each level pinned whole, the way docs/lookout/frames.txt pins a TUI
    /// frame. Art and layout drift silently otherwise.
    #[test]
    fn each_style_level_renders_exactly_this() {
        let headers = ["ID", "NAME", "STATUS", "PID"];
        let rows = vec![
            vec!["0".into(), "zeus-auth".into(), "(o.o) online".into(), "24963".into()],
            vec!["1".into(), "reactmap".into(), "(-.-) stopped".into(), "-".into()],
        ];
        insta::assert_snapshot!(
            "boxed_wide",
            render_boxed(&headers, &rows, &[0, 0, 0, 2], 120)
        );
        insta::assert_snapshot!(
            "boxed_narrow",
            render_boxed(&headers, &rows, &[0, 0, 0, 2], 34)
        );
    }
```

- [ ] **Step 2: Generate and review the snapshots**

```bash
cargo insta test -p shep --accept --lib
```

Then **read the two `.snap` files** before committing them. A snapshot
accepted without reading pins whatever the bug produced.

- [ ] **Step 3: Verify the whole suite**

```bash
cargo test -p shep --lib --bins --all-features
```

Expected: PASS.

- [ ] **Step 4: Gate and commit**

```bash
cargo fmt --all --check
```

```bash
cargo clippy -p shep --all-targets --all-features -- -D warnings
```

```bash
git add crates/shep-cli/src/
git commit -m "test(shep): pin the boxed table at two widths

The same discipline docs/lookout/frames.txt applies to a TUI frame. Layout
drifts silently otherwise, and a table is exactly the kind of output where a
one-column change is invisible in review and obvious in use.

Both snapshots were read before being accepted: an accepted snapshot pins
whatever the code produced, bug included."
```

---

## Final verification

Once, after Task 7:

```bash
cargo test --workspace --all-features
```

Expected: PASS. The only `--workspace` command in the plan.

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Expected: exit 0.

```bash
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

Expected: exit 0. This is the check that matters for this plan specifically:
`vocabulary.rs` and `output/` must compile without `mod lookout`, and
`terminal_width`'s fallback must be reachable off-unix. Needs
`brew install mingw-w64`.

## Self-review notes

Checked against the spec, 2026-08-18:

- **Spec §1 (renderer, visible width, priority, footer)** → Tasks 3 and 4.
- **Spec §1 (terminal width, 80 fallback)** → Task 5 Step 4.
- **Spec §2 (faces, roles, shared vocabulary)** → Task 2.
- **Spec §3 (levels, resolution order, `shep style`)** → Task 1.
- **Spec §3 (the hard rule)** → Task 5 Steps 5-6, enforced at one seam and
  tested by the existing `cli_e2e`.
- **Spec §4 (three sheep sites, cap, exclusions)** → Task 6.
- **Spec §5 (property test, snapshots, e2e, NO_COLOR, precedence)** → Tasks
  3, 4, 5, 7. **Gap found and closed:** the spec asks for a `NO_COLOR`-at-
  `full` test and the tasks did not have one; it belongs in Task 5, where
  the level is resolved, and is added to that task's Step 5 as part of
  resolution.
- **Spec §6 (the two-table seam)** → Task 2, with `theme.rs` reduced to a
  binding.

One deviation from the spec, made while planning: the spec's §2 said faces
live in `theme.rs`. They cannot -- `mod lookout` is `cfg(unix)` and
`mod output` is not -- so they live in `vocabulary.rs` and the spec was
corrected before this plan was written.
