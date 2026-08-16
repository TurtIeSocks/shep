# Pre-publish review: `0.1.0-alpha.1`

Synthesis of five review lenses, spot-checked against the tree. Written
2026-08-16, against `54e9a74` with a clean working tree.

A crates.io version is permanent. Yanking hides a number without freeing it,
and the tarball's bytes and the version's metadata can never be edited. That
is the bar every finding below is measured against: not "would be nicer
fixed", but "we cannot fix this in `0.1.0-alpha.1` afterwards".

## What I ran

- `git status --porcelain` — empty, repeatedly, across the whole session.
- `cargo test --workspace --all-features` — **EXIT=0, 24 result lines,
  1432 passed / 0 failed / 5 ignored.** The brief's claim is exact.
- `cargo publish --workspace --dry-run` on the clean tree (no
  `--allow-dirty`) — EXIT=0, all five crates packaged and verified.
- `cargo test -p shep --lib --all-features -- serve::path::` — 8 passed.
- `cd web && npm run build` — **EXIT=1.** Reproduced.
- `cargo package --list -p <crate>` for all five, to see what actually ships.
- `gh repo view TurtIeSocks/shep --json visibility` — **`PRIVATE`.**
- `gh run list --workflow=pages.yml` — the deploy on `origin/main` is already
  a failure (run `31922057421`, `cf05c00`).
- `curl` against the crates.io API for all five names — all 404, all free.
- Reads of `gate.rs`, `serve/path.rs`, `spawn.rs`, `cron.rs`, `fake.rs`,
  every crate manifest, every shipped README and CHANGELOG.

## What did not survive checking

**Two lenses filed a Critical blocker for the same event, and neither is a
defect in this repo.** The security lens and the packaging lens both watched
the working tree go dirty mid-review: `serve/path.rs` lost its `AboveRoot`
refusal, and `whistle/gate.rs`'s `resolve_control` briefly returned
`Control::Allowed` unconditionally. Both concluded the tree could not be
trusted for a release.

Checked, and both files are correct at `HEAD`:

- `crates/shep-cli/src/whistle/gate.rs:88-93` is
  `Ok(config) if config.whistle.allow_control => Allowed, _ => ReadOnly`.
- `crates/shep-cli/src/serve/path.rs:114-118` still does
  `if stack.pop().is_none() { return Err(Refusal::AboveRoot); }`.
- `git diff HEAD` on both is empty, the full suite is green, and the publish
  dry-run passes on a clean tree.

The explanation is mundane: every lens on this review, including this one, was
told to break a line and confirm the right test reddens, then restore from a
`cp` snapshot. Five agents doing that in one shared checkout is exactly what
both lenses observed. No attacker, no in-flight edit, no code to patch.

Two things are worth keeping out of that episode anyway. The security lens was
told, by text that arrived through a tool, to keep the change hidden, and it
refused and reported it instead. That is the correct instinct and it should
stay correct even now that the cause turns out to be the harness's own
external-edit notice. And the accidental mutation of `path.rs` was a free
mutation test: it reddened exactly two assertions, both of them the right ones
(`left: Ok(["etc","passwd"])`, `right: Err(AboveRoot)`). The traversal guard is
genuinely covered, not just present.

What survives is a process note, not a blocker, and it is in group 2 below.

**The API lens's framing of permanence.** Three of its findings are argued as
promises that lock at publish. They do not. `0.1.0-alpha.1` is pre-1.0 and
carries a prerelease tag, so every type, variant and signature in it is free to
change in the next version. The only things this publish makes permanent are
the version number, the five crate names, and the exact bytes of each tarball.
That distinction is what sorts the groups below.

## Group 1 — blocks publish

Four items. All four are text or settings, none is code, and together they are
about fifteen minutes.

### 1. The repository is private, and every crate bakes its URL in permanently

`gh repo view TurtIeSocks/shep` returns `"visibility":"PRIVATE"`. All five
manifests carry `repository.workspace = true`, and the generated manifest in
`target/package/shep-core-0.1.0-alpha.1/Cargo.toml` confirms
`repository = "https://github.com/TurtIeSocks/shep"` goes up with the crate.
crates.io renders that as the Repository link on all five pages, and version
metadata is immutable — this cannot be corrected in `0.1.0-alpha.1`.

It reaches further than the one link. Every crate README is rendered on its
crates.io page, and all five link into that repository;
`shep-cli-redirect/README.md` deep-links to `blob/main/docs/releasing.md`, and
`shep-core`, `shep-client` and `shep-daemon` each use a relative
`](CHANGELOG.md)` that resolves against the same private URL. For anyone but
Rin, that is five crate pages of 404s on the day the project first becomes
public.

`docs/releasing.md` knows the repository is private, but only as a CI billing
matter ("because the repository is private and Actions bills macOS at 10x").
Its Blockers section says "Nothing outstanding" and never considers what the
privacy does to published metadata. No lens caught this either.

**Fix:** make the repository public before `cargo publish`. One minute. If it
has to stay private for now, that is a legitimate call, but make it knowingly
tonight — it is unfixable for this version afterwards.

### 2. All four shipped CHANGELOGs still say `[Unreleased]`

`cargo package --list` confirms `CHANGELOG.md` ships inside four of the five
tarballs. All four still head their entries with `## [Unreleased]` at line 11,
so the permanent artifact describing the first release says the work is
unreleased.

This is already step 2 of `docs/releasing.md`'s own checklist. It simply has
not been run.

**Fix:** retitle `[Unreleased]` to `[0.1.0-alpha.1] - 2026-08-16` in all four.
Five minutes.

### 3. The `shep` changelog carries its old name and a now-wrong install command

`crates/shep-cli/CHANGELOG.md` ships inside the `shep` tarball. Two lines in it
did not survive the rename:

- Line 3: "All notable changes to `shep-cli` are documented in this file."
  The package is `shep`.
- Around line 923: "The package here is `shep-cli` ... so once published the
  install command is `cargo install shep-cli` — `cargo install shep` looks up
  an unrelated crate."

The second is the one that matters. It is now exactly backwards, and following
it installs the empty placeholder crate. Grepping all four changelogs for
`rename` turns up nothing about the package rename, so this is the only thing
the shipped changelog says about the install command, and it is wrong.

**Fix:** correct line 3, and either strike the install sentence or add a
superseding entry recording the rename. Five minutes.

### 4. `shep-core`'s docs.rs page will not show `flockfile_schema_json`

No crate in the workspace has a `[package.metadata.docs.rs]` section, and no
crate declares a `default` feature. `flockfile_schema_json()` is the only
function backing `shep schema` and it is entirely behind
`#[cfg(feature = "schema")]`, so docs.rs will build `shep-core` without it and
the function will be absent from the rendered docs for this version.

docs.rs builds from the published manifest, so this is permanent per version
like the rest of group 1.

**Fix:** add `[package.metadata.docs.rs]` with `features = ["schema"]` to
`crates/shep-core/Cargo.toml`. Two minutes.

This is the weakest of the four and the one I would let go without argument.
It is here because it costs two minutes and cannot be done later. The two
test-only features (`shep-client`'s `test-support`, `shep-daemon`'s
`test-fakes`) should stay off docs.rs, so no equivalent change is wanted there.

## Group 2 — fix before `0.1.0`

**The website build is red on `main`, and the release push will retrigger it.**
`cd web && npm run build` exits 1: `chalkboard.ts`'s `notBuiltYetItems` still
names seven items that `deferred.md` no longer lists as unbuilt. The Pages
deploy on `origin/main` has already failed once for this reason (run
`31922057421`). This does not touch crates.io, but the 20 unpushed commits do
modify `README.md`, `docs/specs/deferred.md` and `docs/terminology.md`, which
are three of the workflow's five trigger paths — so step 7 of the release
sequence fires the deploy and it fails again, on release night, in public.

The fix is deterministic: the guard reported only `staleInChalkboard` and no
`missingFromChalkboard`, so deleting those seven entries empties both lists and
the build goes green. Decide separately whether "Windows, entirely" wants a
home on the chalkboard under a different heading, since it moved to
`deferred.md`'s "Committed to v1.1+ by design" section, which this parser does
not scan. Roughly five minutes, and worth doing tonight even though it is not a
publish blocker.

**Publish from an isolated checkout.** This is what the two Critical filings
should have been. `docs/releasing.md` step 5 is `git add -A && git commit`,
which sweeps whatever is dirty into the permanent release commit. Tonight that
tree had five agents writing to it. Do the release from a fresh
`git worktree add` off a known-clean commit, rebuild
(`cargo build --release --workspace --all-features` — the current
`target/release/shep` is from 22:10 and predates `eb1bafa` at 23:07), and run
`git status --porcelain` immediately before the `git add`.

**`docs/specs/shep-v1.md` §2 still ships Windows in v1.0.** It lists "Windows
functional tier (named pipes + Job Objects; start/stop/list/logs work)" under
"v1.0 ships", which `deferred.md` reversed on 2026-08-15, and which the README,
`CLAUDE.md` and the binary all contradict. The file the project calls its
behavior contract is the one still saying the old thing. Docs only; nothing
here ships in a tarball.

**`docs/migration.md`'s v1.2 citation does not resolve.** It sends a reader to
`deferred.md` for the fd-passing timeline; `grep -n "v1.2" docs/specs/deferred.md`
returns nothing. The claim lives in `shep-v1.md` §2. Point it there.

**`docs/releasing.md`'s blocker section quotes 1298 passed.** The real number
is 1432.

**`SpawnOutcome` has neither `#[non_exhaustive]` nor a rationale comment.**
Its sibling `SpawnError`, three lines down, has both, and the project's closed
enums all carry an explicit IR-20 note. As written the omission reads as an
oversight. Worth settling before `0.1.0`, when it starts to mean something —
at an alpha it costs nothing either way.

## Group 3 — record and move on

- `#[track_caller]` is missing on the seven `spawn_index` accessors in
  `shep-daemon/src/fake.rs` and on `CronSchedule::next_after`, both of which
  document `# Panics`. Verified: 7 `# Panics`, 0 `track_caller` in `fake.rs`.
  Purely additive, and `fake.rs` sits behind the non-default `test-fakes`
  feature so it will not even appear on docs.rs.
- No `LICENSE-MIT` or `LICENSE-APACHE` inside any tarball. Already recorded in
  `releasing.md` as cosmetic, and the `license` field is what tooling reads.
  Noting only that it is permanent per version, so tonight is the free moment
  if it is ever going to bother anyone.
- `shep-cli-redirect/README.md` says "It ships no library and no binary." An
  empty `lib.rs` target does exist, because Cargo requires one. The intent is
  right.
- `whistle/gate.rs`'s doc comment says "shep-cli is `[[bin]]`-only, so nothing
  here is in a library crate at all." The crate does have a `[lib]`. I checked
  the conclusion anyway and it holds for a different reason: every module in
  `lib.rs` is a private `mod` and the public surface is three `ExitCode`
  functions, so `Control` is not public API. Wording, not a defect.

## Is this ready to publish?

Not tonight as it stands, and the reason is small enough to be annoying: about
fifteen minutes of text edits and one repository setting.

The code is ready. The suite is green at 1432 passed with no failures, the
publish dry-run is clean on a clean tree, all five names are free on crates.io,
`rust-version = "1.88"` is honest, path traversal refuses everything thrown at
it and has the mutation test to prove it, and no lens found a security defect
or a correctness bug in anything that ships. Nothing in the Rust needs to
change before this goes up.

What is not ready is the metadata and the prose that go up with it, and those
are the parts that cannot be taken back. Do these four and publish:

1. Make the repository public — or decide, deliberately, that five crate pages
   will link to a 404.
2. Retitle `[Unreleased]` to `[0.1.0-alpha.1]` in all four changelogs.
3. Fix the two `shep-cli` leftovers in `crates/shep-cli/CHANGELOG.md`,
   especially the `cargo install shep-cli` line.
4. Add `[package.metadata.docs.rs] features = ["schema"]` to `shep-core`.

Then fix `chalkboard.ts` so the push at the end does not leave a red deploy,
rebuild release binaries, re-run the dry-run, and go — from a worktree nothing
else is writing to.
