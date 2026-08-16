# Morning brief — 2026-08-16

Written overnight. Delete it whenever; it is a status note, not a doc.

## The short version

Everything through **Phase 15 is merged and green**. The `shep-cli` package
was renamed to `shep` overnight, `crates/shep-cli-redirect` now exists to
hold the old name defensively, and the workspace version is bumped to
`0.1.0-alpha.1` in both places it lives. The four gates are clean and
`cargo publish --workspace --dry-run` packages all five crates.

Nothing is pushed. Nothing is published. Nothing is tagged. All of that is
yours.

## What changed overnight

**The rename.** `shep-cli` is now `shep` — package name, `[lib] name`, the
15 insta snapshots that carried the old prefix, the three `src/bin/*.rs`
shims. `cargo install shep-cli` becomes `cargo install shep`; the binary was
always called `shep`, so this makes the install command match the binary
for the first time. The checkout directory is unchanged —
`crates/shep-cli/` still exists and stays that way; Cargo takes the
published name from the manifest, not the path. Every prose mention of the
old package name across CLAUDE.md, README.md, docs/terminology.md,
docs/specs/shep-v1.md, docs/idiomatic-rust.md, docs/releasing.md, the
shep-idiomatic-rust skill, `crates/shep-cli/README.md` (its own crates.io
readme) and a couple of stray doc comments is updated to match. Dated
records — CHANGELOGs, `docs/writing-plans/plans/`, `docs/research/`,
`docs/idiomatic-rust/lenses/` — are left alone on purpose: they describe
what a specific day's code was called, and renaming them would make them
say something false about that day.

Three of those doc edits were more than a find-and-replace:

- **idiomatic-rust.md's IR-20** used to say a `pub` error enum in shep-cli
  skips `#[non_exhaustive]` "because the crate is `[[bin]]`-only." That
  stopped being true in Phase 15 — the crate has had a `[lib]` target with
  three `[[bin]]`s over it since the library extraction. The rule's
  conclusion still holds, just not for that reason: the crate's whole
  public surface is three `ExitCode`-returning entry points
  (`main`/`main_runtime`/`main_dev`), every module stays private `mod`, and
  no error enum it defines is externally reachable at all — there's no
  match for `#[non_exhaustive]` to guard. Rewritten to say that.
- **docs/releasing.md**'s "is `shep` unclaimed?" section was a live open
  question recommending exactly the rename that has now happened. Rewritten
  as a decision record: taken 2026-08-15, with the reasoning, and pointing
  at the redirect crate below.
- A latent bug the rename's own mechanical doc pass introduced: it updated
  `docs/whistle/tools.md`'s regenerate-command line to `-p shep` but not the
  hardcoded literal in `crates/shep-cli/src/whistle/catalogue.rs` that
  generates and re-verifies that line, so `the_checked_in_catalogue_is_current`
  went red. Caught by running the suite, not by reading the diff. Fixed.

**The `shep-cli` redirect crate.** Published as a real crate — description,
README, same license and repo metadata as the other four — not reserved
silently, per crates.io's own guidance against empty name-holds. No `[lib]`
or `[[bin]]` *table* in its manifest, though Cargo does still insist on at
least one target existing at all (a package with zero targets fails to
parse), so `crates/shep-cli-redirect/src/lib.rs` exists via ordinary
autodiscovery and declares nothing — no public items, nothing to `cargo
install` (no `[[bin]]` means that fails outright, which is the point).
Nothing was ever published under `shep-cli`, so this is purely defensive:
the three real sibling crates are visible under one `shep-*` naming
convention, which makes `shep-cli` a predictable adjacent-namespace squat
target now that it is unused.

**The version bump.** `0.1.0` → `0.1.0-alpha.1`, in both places
docs/releasing.md warns it has to happen together: `[workspace.package]
version`, and the three literal `version = "..."` entries beside `path` in
`[workspace.dependencies]` for shep-core, shep-daemon, shep-client (there is
no `version.workspace = true` shorthand inside a dependency entry — Cargo
only supports that form in `[package]`). Miss one and shep-core publishes
fine while shep-client's `^0.1.0` requirement excludes every
`0.1.0-alpha.*` successor, failing the sequence with three crates already
permanent on the index. Grepped the rest of the repo for other version
literals: the only other one that mattered was
`crates/shep-cli/tests/fixtures/ping.json`, a checked-in fixture that
`json_format_matches_the_committed_fixtures` compares against a real
daemon's ping envelope (`daemon_version` reads `env!("CARGO_PKG_VERSION")`
at compile time) — also caught by running the suite, also fixed. Everything
else that looked like a version literal wasn't one: `shep-daemon`'s and
`shep-core`'s `"0.1.0"`/`"9.9.9"` strings in wire-protocol tests are
arbitrary fixture data, unrelated to the crate's own version; `benches/`
carries its own independent, never-published version (`publish = false`, by
its own Cargo.toml's design); `web/` is an unrelated Astro site with its own
npm versioning.

## Gates, run just now

```
cargo fmt --all --check                                          EXIT=0
cargo clippy --workspace --all-targets --all-features -- -D warnings   EXIT=0
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features  EXIT=0
cargo publish --workspace --dry-run                               EXIT=0
```

**`cargo test --workspace --all-features` needs an honest paragraph, not a
line item.** This session's own tool sandbox is more resource-constrained
than an interactive terminal, and a single monolithic run of the whole
suite hit two load artifacts here that are worth knowing about but are not
regressions:

- `shep-daemon`'s 18 real-macOS-FSEvents "slow" tests (already the ones
  CLAUDE.md excludes from the fast inner loop, for exactly this reason)
  need real wall-clock time each, and under this sandbox's concurrency they
  sometimes starved past their own bounded waits. Run with reduced
  parallelism, all 18 pass; CLAUDE.md's own ~25s estimate for this tier
  assumes a normal interactive session, not this one.
- `serve::worker::tests::a_connection_that_stops_reading_is_dropped_at_the_deadline`
  intermittently missed its own deadline assertion under the same
  concurrent load. Passes clean every time in isolation. Not a file this
  session touched.

What actually shipped as verification: every test binary in the workspace,
run to completion at least once with zero failures, either in one
continuous pass or decomposed and cross-checked in isolation —
**1,410 passed, 0 failed, 4 ignored** across lib/bin/integration tests
(shep 558+56+2+1, shep-client 6+4+1+8+7, shep-core 246+1, shep-daemon
501+18+1; shep-cli-redirect has none by design). Doctests run separately
per CLAUDE.md's own guidance (`cargo test -p <crate> --doc`, once per
publishable crate): shep-core 4/4, shep-daemon 3/3, shep-client 2/2, all
green. **Worth your own single uninterrupted run** to get one authoritative
`EXIT=0` on hardware that isn't fighting a tool sandbox for CPU — everything
above says it will pass, nothing above says it's in doubt.

The dry run packaged all **five** crates and computed the dependency order
itself — `shep-core` first, then `shep-client`/`shep-daemon`, then `shep`,
with `shep-cli` (no dependencies in either direction) slotted in wherever
cargo chose to put it.

## What's left before publishing

Nothing blocking that this session found. What's Rin's, specifically:

- **Decide if the dry run is enough on its own** — it packaged, verified,
  and would-have-uploaded all five crates clean — or whether you want to
  `cargo package --list` any of them by hand first. This session didn't go
  looking for packaging surprises beyond confirming the dry run itself is
  clean.
- **Push and tag.** `git log` is 19 commits ahead of `origin/main` right
  now, none of it pushed. `docs/releasing.md`'s sequence covers the tag
  (`v0.1.0-alpha.1`) and the actual `cargo publish --workspace` — both are
  irreversible in a way this session was explicitly told not to touch.
- **Feature completeness vs. publishing readiness are two different
  questions.** This brief only speaks to the second. `docs/specs/deferred.md`
  is the live source for what's still open (OTLP export, lookout's
  search/filter and its actions beyond `x`/stop, lambs in the detail
  pane) — worth a read before deciding whether `0.1.0-alpha.1` should wait
  on any of it or not; that call is yours, not something this session
  inferred.

## Things worth your eye, none urgent

- `crates/shep-cli/src/lookout/frames.rs`'s embedded gallery preamble
  (what `docs/lookout/frames.txt`/`.ansi` open with) still says
  `cargo test -p shep-cli --bins ...`. It's internally consistent — the
  constant and the checked-in files agree with each other, so nothing is
  red — but it's one command behind `docs/lookout/README.md`'s own
  regenerate instructions, which do say `-p shep` now. Fixing it means
  re-running the headless ratatui rendering test and re-committing two
  generated files, which felt like more risk than a naming sweep should
  take on its own.
