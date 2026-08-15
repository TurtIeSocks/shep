# Releasing shep

Nothing has ever been published. There are no git tags, no crates.io
presence, and no install path. This file is the checklist for changing that,
written so the first release is a decision rather than a discovery.

Everything below was worked out and dry-run on 2026-08-15 against `main`.
No tag was created and nothing was uploaded.

## Publish order

The four crates form a chain, so they go up in dependency order. Read off the
manifests:

| Crate | Depends on |
|---|---|
| `shep-core` | nothing in the workspace |
| `shep-client` | `shep-core` |
| `shep-daemon` | `shep-core` |
| `shep-cli` | `shep-core`, `shep-daemon`, `shep-client` |

So: **shep-core, then shep-client and shep-daemon in either order, then
shep-cli.** `shep-client` and `shep-daemon` do not know about each other.

You do not have to drive that by hand. `cargo publish --workspace` computes
the order itself and, unlike four separate `-p` runs, resolves the
inter-member dependencies against the local workspace instead of demanding
they already be on the index. That is the one-command form, and it is what
the sequence below uses.

## Version: publish `0.1.0-alpha.1`, not `0.1.0`

A crates.io version is permanent. Yanking hides a version from resolution but
never frees the number, so `0.1.0` can be spent exactly once and never
corrected. That matters more than usual here for two reasons.

The first is that shep is genuinely pre-release, and the README already says
so. Windows is zero rather than partial, lookout has one of its four panes,
whistle and `shep serve` and `shep dev` do not exist, and CI has never run on
its own. A `0.1.0` on crates.io is a normal release under semver, and cargo
resolves it for anyone who writes `shep-core = "0.1"`. A pre-release version
is excluded from that matching by the semver spec, so `0.1.0-alpha.1` cannot
be picked up by accident. Nobody ends up depending on this by writing a
version requirement that looks ordinary.

The second is that a workspace's first publish is where packaging faults
surface: a readme path that does not resolve, a docs.rs build that fails on a
unix-only dependency, an inter-crate version requirement that does not match
what actually went up. Those are unfixable in place. On an alpha train the fix
is `0.1.0-alpha.2` and costs nothing. On `0.1.0` the fix is `0.1.1` plus a
yank, on four crates, in public.

The cost is one line of friction: `cargo install shep-cli` does not select a
pre-release, so the install command carries an explicit version until the
first non-alpha release. That is a fair price for keeping `0.1.0` in reserve
for the release that is actually complete.

### The version bump touches two places

`[workspace.package] version` is one of them. The other is
`[workspace.dependencies]`, where `shep-core`, `shep-daemon` and `shep-client`
each carry a literal `version = "0.1.0"` beside their `path`. Cargo strips the
path at publish time and substitutes that literal, and there is no
`version.workspace = true` shorthand inside a dependency entry, so the two
have to be edited together.

Getting this wrong is a silent failure with a loud symptom. If the package
version becomes `0.1.0-alpha.1` while the dependency literals stay `"0.1.0"`,
then the published `shep-client` asks for a `shep-core` matching `^0.1.0`,
which by semver excludes every `0.1.0-alpha.*`. `shep-core` publishes fine and
`shep-client` fails to resolve, at the point where three of four crates are
already permanent.

The five lines to change in the root `Cargo.toml`:

```toml
[workspace.package]
version = "0.1.0-alpha.1"

[workspace.dependencies]
shep-core = { path = "crates/shep-core", version = "0.1.0-alpha.1" }
shep-daemon = { path = "crates/shep-daemon", version = "0.1.0-alpha.1" }
shep-client = { path = "crates/shep-client", version = "0.1.0-alpha.1" }
```

If this drifts again on the next bump, `cargo-release` mechanises it.

## Tag: one tag, `v0.1.0-alpha.1`

The four crates share a single workspace version and are released together, so
one annotated tag on the release commit is the honest shape. Per-crate tags
(`shep-core-v0.1.0-alpha.1`) are for workspaces whose members version
independently, and adopting that scheme here would create four tags that can
only ever hold the same number.

Keep the `v` prefix. It is what GitHub's release UI and most changelog tooling
expect, and it is what the repo will be stuck with.

## The sequence

Run it from a clean `main` with everything committed. One cargo command at a
time: the workspace shares one target-dir build lock.

```bash
# 1. Bump both places, per the section above, then reconcile the lockfile.
$EDITOR Cargo.toml
cargo check --workspace --all-features

# 2. Move each crate's [Unreleased] section to [0.1.0-alpha.1] with today's
#    date, in all four CHANGELOG.md files.
$EDITOR crates/*/CHANGELOG.md

# 3. The task gate, one command at a time, $? read directly and never
#    through a pipe.
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# 4. Rehearse the whole publish. Resolves inter-member deps locally, so it
#    exercises all four rather than stopping at the first unpublished one.
cargo publish --workspace --dry-run

# 5. Commit and tag.
git add -A
git commit -m "chore(release): 0.1.0-alpha.1"
git tag -a v0.1.0-alpha.1 -m "shep 0.1.0-alpha.1"

# 6. Publish. Ordering is cargo's problem, not yours.
cargo publish --workspace

# 7. Push the commit and the tag.
git push origin main
git push origin v0.1.0-alpha.1
```

Step 6 is the irreversible one. Everything before it can be redone.

If you would rather go crate by crate and watch each land, the same thing
spelled out:

```bash
cargo publish -p shep-core
cargo publish -p shep-client
cargo publish -p shep-daemon
cargo publish -p shep-cli
```

Each of the last three waits for the previous crate to appear in the index.
Recent cargo polls for that on its own; if a run fails with `no matching
package named 'shep-core' found` immediately after `shep-core` went up, wait a
minute and rerun the one that failed.

Afterwards, the install line becomes:

```bash
cargo install shep-cli --version 0.1.0-alpha.1
```

Add it to the README's status block at that point, replacing "has no install
script".

## What is a blocker and what is not

The point of this section is that the decision in the morning is informed. A
`0.1.0-alpha.1` promises a working thing on macOS and Linux that is not
finished. It does not promise a stable API, Windows, or a complete v1.0
surface.

### Blockers

**Nothing outstanding.** The four gates below all pass on `main` as of
2026-08-15, and the packaging rehearsal is clean. What follows is what has to
stay true, not work left to do.

- The task gate is green: `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets --all-features -- -D warnings`, and `cargo test --workspace
  --all-features` at 1219 passed / 0 failed / 4 ignored.
- Every crate has `description`, `readme`, `keywords` and `categories`, and
  every category is a real crates.io slug. A category the registry does not
  recognise is rejected at upload, after earlier crates in the chain have
  already gone up permanently.
- The version bump reaches both `[workspace.package]` and the three
  `[workspace.dependencies]` literals. See the section above for why this one
  is a blocker and not a nicety.
- `cargo publish --workspace --dry-run` is clean at the version you are about
  to publish, not at the version you dry-ran last week.

### Not blockers

**CI has never run automatically.** The workflow is `workflow_dispatch` only,
because the repository is private and Actions bills macOS at 10x. The gates it
would run are the gates that run locally, and they are green. Turning the
triggers on is the right move when the repository goes public, and it is not a
precondition for an alpha.

**Windows is zero.** Every verb prints `shep does not yet support Windows` and
exits 1. That is a documented state, not a broken build: the workspace
cross-compiles for the target, and the README, the crate descriptions and the
`shep-daemon` readme all say plainly that supervision is unix only. An alpha
is allowed to have an unsupported platform. A `1.0.0` is not.

**Several v1.0 spec items are unbuilt.** whistle, `shep serve`, `shep dev` and
`shep runtime`, `.js` Flockfiles, the schemars schema, the CLI-flag config
layer, openrc and BSD `rc.d` units, and three of lookout's four panes. All of
them are named in [specs/deferred.md](specs/deferred.md), and none of them
change the behaviour of what does exist. They are why the version is an alpha.

**Docs.rs has never built these crates.** It cannot until they are published,
so this is unknowable in advance rather than skippable. `RUSTDOCFLAGS="-D
warnings" cargo doc --workspace --no-deps --all-features` locally is the best
available proxy and it passes. Check the docs.rs build status for each crate
after publishing; a failure there is fixable in the next alpha.

**No LICENSE file ships inside the `.crate` archives.** `LICENSE-MIT` and
`LICENSE-APACHE` sit at the repository root, outside every package directory,
and cargo only reaches outside for `readme` and `license-file`. The
`license = "MIT OR Apache-2.0"` field is what crates.io renders and what
tooling reads, so this is cosmetic. Copying or symlinking the two files into
each crate directory would fix it whenever it starts to bother you.

## The `shep` crate name is unclaimed

Checked 2026-08-15: `shep`, `shep-core`, `shep-daemon`, `shep-client` and
`shep-cli` are all free on crates.io.

`shep` being free is the interesting one, because the binary is already called
`shep` while the crate that builds it is `shep-cli`. Users would install it as
`cargo install shep-cli` and then run `shep`. Renaming the crate to `shep`
would make those match, and it is a rename of the package name only, not of
the local directory. Whichever way that goes, the name is worth taking in the
first publish rather than after somebody else notices it is available.

## What the rehearsal found

`cargo package --list` was read for all four crates. Every file in every
archive is source, tests, fixtures, insta snapshots, the changelog or the
readme. Nothing needed an `exclude`.

The 800K of design PNGs under `docs/shep-design/`, the `benches/` tree, the
`web/` directory and the pm2 migration fixtures at the repository root are all
outside the four package directories, and cargo packages only what lives under
the crate it is building. They were never in danger of shipping.

The largest archive is `shep-daemon` at 1.9 MiB uncompressed, 520 KiB
compressed, which is `supervisor.rs` and `boot.rs` being large source files.
That is well inside the 10 MiB registry limit.
