# Releasing shep

**0.1.0 published on 2026-08-26.** All five crates are on crates.io and
`cargo install shep` works. Much of what follows was written before that and
describes the first release specifically; it is kept because the reasoning
about publish order and version choice still holds.

**Releasing is now one act: merge the release pull request.** release-plz
opens it, and merging it tags the commit, creates the GitHub release, and
uploads. There is no tag to push by hand and no local `cargo publish`.

## Publish order

Four of the five crates form a chain, so they go up in dependency order. Read
off the manifests:

| Crate | Depends on |
|---|---|
| `shep-core` | nothing in the workspace |
| `shep-client` | `shep-core` |
| `shep-daemon` | `shep-core` |
| `shep` | `shep-core`, `shep-daemon`, `shep-client` |
| `shep-cli` | nothing — the redirect placeholder, no code, no deps |

So: **shep-core, then shep-client and shep-daemon in either order, then
shep.** `shep-client` and `shep-daemon` do not know about each other.
`shep-cli` has no workspace dependency in either direction, so it can go up
whenever — first, last, or wherever `cargo publish --workspace` schedules it.

You do not have to drive that by hand. `cargo publish --workspace` computes
the order itself and, unlike five separate `-p` runs, resolves the
inter-member dependencies against the local workspace instead of demanding
they already be on the index. That is the one-command form, and it is what
the sequence below uses.

## Version: `0.1.0`, and what the alpha train was for

A crates.io version is permanent. Yanking hides a version from resolution but
never frees the number, so a version can be spent exactly once and never
corrected. That is why the first publish went out as `0.1.0` rather
than `0.1.0`.

That reasoning was half right, and it is worth keeping the half that held. A
workspace's first publish is where packaging faults surface: a readme path
that does not resolve, a docs.rs build that fails on a unix-only dependency,
an inter-crate version requirement that does not match what actually went up.
Those are unfixable in place. On an alpha train the fix is `-alpha.2` and
costs nothing; on `0.1.0` it is `0.1.1` plus a yank, on four crates, in
public. Treating the first publish as a rehearsal is sound and it is what
this project did.

The other half did not hold. The argument was that a pre-release cannot be
picked up by accident, since semver excludes it from ordinary matching. True,
and close to worthless here, because `0.x` already carries that meaning: the
semver spec says major version zero is for initial development and anything
may change at any time. The suffix stacked a second instability signal on a
version that already said it.

What it charged for that, measured on 2026-08-26 rather than guessed:

```
$ cargo install shep
error: could not find `shep` in registry `crates-io` with version `*`
```

`cargo install` resolves `*` when given no version, and `*` never matches a
pre-release. Note the asymmetry, because it decides how much this matters:
`cargo add shep-client` copes fine, writing the exact `"0.1.0"`
requirement itself. Only `install` breaks. For a library that would be a
footnote. shep is a CLI whose primary distribution channel is
`cargo install`, so it was the headline command in the README, in
getting-started, and on the landing page, each needing a version flag and a
paragraph explaining cargo's resolution rules.

So `0.1.0` it is, and the alpha stays on the registry unyanked. It works when
named explicitly, and yanking is for releases that are broken or unsafe
rather than superseded. Publishing `0.1.0` needs no yank in any case:
`0.1.0` sorts above `0.1.0`, so it is an ordinary forward bump.

Keep the rehearsal habit for future risky publishes. Drop the assumption that
a pre-release suffix is free, because on a binary crate it is not.

### The version bump touches two places

`[workspace.package] version` is one of them. The other is
`[workspace.dependencies]`, where `shep-core`, `shep-daemon` and `shep-client`
each carry a literal `version = "0.1.0"` beside their `path`. Cargo strips the
path at publish time and substitutes that literal, and there is no
`version.workspace = true` shorthand inside a dependency entry, so the two
have to be edited together.

Getting this wrong is a silent failure with a loud symptom. If the package
version and the dependency literals disagree, the published `shep-client`
asks for a `shep-core` that the version actually uploaded does not satisfy.
The pre-release train made this especially easy to hit, since `^0.1.0`
excludes every `0.1.0-alpha.*` by semver, but any mismatch does it.
`shep-core` publishes fine and `shep-client` fails to resolve, at the point
where three of four crates are already permanent.

The five lines to change in the root `Cargo.toml`:

```toml
[workspace.package]
version = "0.1.0"

[workspace.dependencies]
shep-core = { path = "crates/shep-core", version = "0.1.0" }
shep-daemon = { path = "crates/shep-daemon", version = "0.1.0" }
shep-client = { path = "crates/shep-client", version = "0.1.0" }
```

If this drifts again on the next bump, `cargo-release` mechanises it.

## Tag: `v0.1.0` was manual; release-plz's default is one tag per package

`v0.1.0` was tagged by hand, in the sequence below, before release-plz owned
any of this. The reasoning that follows describes the target shape for that
manual tag, and it does not describe what release-plz, as configured, does.

`release-plz.toml` sets no `git_tag_name` or `git_tag_enable`, so each
release-enabled package gets release-plz's own default for a multi-package
workspace: `{{ package }}-v{{ version }}`. `shep-core`, `shep-client`,
`shep-daemon` and `shep` share a `version_group`, so the version number
agrees across all four, but the tags do not collapse into one:
`shep-core-v0.1.1`, `shep-client-v0.1.1`, `shep-daemon-v0.1.1`,
`shep-v0.1.1`. `shep-cli` carries `release = false` and gets neither a tag
nor a release.

A single shared tag is still possible: release-plz's own single-tag recipe
disables `git_tag_enable` workspace-wide and re-enables it, named
`v{{ version }}`, on one representative package. It is not configured here,
so it is not what the next release does. Whether to add that configuration
is Rin's call, not this fix's to make for her.

All five crates share a single workspace version, and the four with code are
released together. `shep-cli`, the redirect placeholder, carries
`release = false` and is versioned along with the rest without ever being
published by release-plz. So one annotated tag on the release commit would be
the honest shape, if configured. Per-package tags are what a workspace whose members version
independently would want; adopting that scheme here, as the default already
does, produces four tags that can only ever hold the same number.

Keep the `v` prefix, in either shape. It is what GitHub's release UI and most
changelog tooling expect.

## The sequence

**Every release after the first happens on its own.** release-plz opens the
pull request, queues it to merge behind `main`'s required checks, and the
resulting push tags, releases and uploads. Nothing below is something you
run.

What follows is how 0.1.0 was done by hand, kept because the checks in it are
the ones release-plz now performs for you, and because knowing what they were
is what lets you tell whether it did them. Run it from a clean `main` with
everything committed, one cargo command at a time, since the workspace shares
one target-dir build lock.

```bash
# 1. Bump both places, per the section above, then reconcile the lockfile.
$EDITOR Cargo.toml
cargo check --workspace --all-features

# 2. Move each crate's [Unreleased] section to [0.1.0] with today's
#    date, in all four real crates' CHANGELOG.md files. shep-cli (the
#    redirect) ships no code and keeps no CHANGELOG — nothing to log.
$EDITOR crates/*/CHANGELOG.md

# 3. The task gate, one command at a time, $? read directly and never
#    through a pipe.
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# 4. Rehearse the whole publish. Resolves inter-member deps locally, so it
#    exercises all five rather than stopping at the first unpublished one.
cargo publish --workspace --dry-run

# 5. Commit the release, push it, and let CI run the gate on it.
git add -A
git commit -m "chore(release): 0.1.0"
git push origin main

# 6. Once that is green, tag the pushed commit and push the tag.
git tag -a v0.1.0 -m "shep 0.1.0"
git push origin v0.1.0
```

**Merging the release pull request is the irreversible step.** There is no
tag to push and no local `cargo publish` in the sequence.

Two workflow files, one job each. `release-plz-pr.yml` maintains a pull
request that bumps `[workspace.package]` and writes the changelogs from the
conventional commits since the last release, on every push to `main`, then
queues that pull request to merge itself. `release-plz-release.yml` runs on
the resulting push and does nothing unless a manifest names a version that is
not on crates.io. It tags, creates the GitHub release, and uploads in
dependency order.

**So a release needs no human step at all now.** Land an ordinary pull
request on `main` and the version follows it out, once `main`'s required
checks pass on the release pull request. The two things that can stop it are
deliberate: a release pull request that bumps no version is left open and red
rather than merged, and the `crates-io` environment will hold the publish if a
required reviewer is ever configured there.

The token lives in that workflow's `crates-io` environment, so a laptop never
needs one. That environment is also where a required reviewer goes if this
ever wants a human gate between merging and spending a version number.

**This used to be split**, with a hand-written `release.yml` publishing behind
a `v*` tag, and the split was wrong. `release-plz.toml` records why at length.
The short version: merging a pull request titled "chore: release vX.Y.Z" is a
more deliberate act than typing a `git push` of a tag, and the split's only
visible effect was that merging the release PR did nothing and surprised the
person who merged it.

**The gate moved out of the workflow and into a branch ruleset.** Until
2026-08-27 the publish job triggered on `test.yml`'s `workflow_run` rather
than on `push`, so that it could require a green run against the exact commit
it was about to publish. `main` now carries a ruleset requiring `lint`,
`docs`, `typos`, the `test` matrix, `features`, `musl` and `minimal-versions`,
which means an untested commit cannot reach `main` for the publish job to find.
The gate had nothing left to catch, so it went, and the workflow matches
zendriver-rs's shape.

Read that as a dependency, not a simplification: delete the ruleset and
nothing checks anything before an upload. `slow`, `coverage`, `bench`,
`privileged` and `windows-gnu` are deliberately not required. `slow` is the
serial timing tier and has been the whole of CI's red for four consecutive
runs; requiring a job that fails on a contended runner would block merges
without telling anyone anything.

**The release pull request is squashed, not merged.** release-plz 0.3.160,
which the pinned action runs, substitutes the pull request's head commit for
the commit it was handed when that commit is a merge commit, so `cargo publish`
would package a tree that never landed on `main`. A squash leaves nothing for
that lookup to find. The old workflow carried a step that refused merge
commits outright; squashing removes the case instead of detecting it.

If CI is unavailable and the publish has to happen from a laptop, the token
goes in `CARGO_REGISTRY_TOKEN` and the command is `cargo publish --workspace`,
after the same rehearsal. Ordering is cargo's problem, not yours. To go crate
by crate and watch each land instead:

```bash
cargo publish -p shep-cli
cargo publish -p shep-core
cargo publish -p shep-client
cargo publish -p shep-daemon
cargo publish -p shep
```

`shep-cli` has no dependency ordering to respect — it is listed first only so
it is out of the way early, not because it must go before anything else. Each
of the last three real crates waits for the previous one to appear in the
index. Recent cargo polls for that on its own; if a run fails with `no
matching package named 'shep-core' found` immediately after `shep-core` went
up, wait a minute and rerun the one that failed.

Afterwards, the install line becomes:

```bash
cargo install shep --version 0.1.0
```

`shep` carries three `[[bin]]` targets since Phase 15's library
extraction, not one — this single command installs all three binaries:
`shep`, plus the two container-entrypoint aliases `shep-runtime` and
`shep-dev`.

The README's status block, badges, and "Try it" section already carry this
command as of 2026-08-16, ahead of the actual publish, per Rin's call that
the docs should describe the install rather than wait for it. Nothing to
edit there at publish time; just confirm the badges resolve once the crates
are up.

## `shep-log-rotate` publishes after this, not alongside it

`github.com/TurtIeSocks/shep-log-rotate` is the first external dog and it
depends on `shep-client`. Until `shep-client` is on the index, that crate
cannot be published at all, so the order across the two repositories is:
everything here first, then the dog.

Its manifest is already prepared for the swap. `shep-client` is named there
with **both** a version and a git URL, so cargo builds it from git today while
`cargo package` still finds the version requirement it demands. Once
`shep-client` is up, the git and branch keys come out of that one line and
nothing else changes.

Two things worth knowing before doing that:

- **Its lockfile pins a shep commit, not a version.** Whoever swaps the line
  should rebuild and re-run the dog's own suite against the published crate
  rather than assuming the git build proved it. The published tarball excludes
  files a git checkout has, and that difference is exactly what a git
  dependency cannot test.
- **The version is `0.1.0`**, matching this workspace, so the number
  does not promise more than an alpha.

## What is a blocker and what is not

The point of this section is that the decision in the morning is informed. A
`0.1.0` promises a working thing on macOS and Linux that is not
finished. It does not promise a stable API, Windows, or a complete v1.0
surface.

### Blockers

**Nothing outstanding, and this is no longer a prediction.** `0.1.0-alpha.1`
published on 2026-08-26: all five crates went up, in cargo's own order,
within four seconds. The list below is what held, not what somebody hoped
would hold.

- The task gate is green: `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets --all-features -- -D warnings`, and `cargo test --workspace
  --all-features` at 1716 passed / 0 failed.
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

**CI runs automatically now.** `push` to `main` and `pull_request` trigger the
full workflow as of 2026-08-16; before that it was `workflow_dispatch` only,
kept off while the repository was private and Actions billed macOS at 10x.
The repository is public and standard runners are free, so that arithmetic is
history rather than a live constraint. The
weekly `schedule` row stays off for the same billing reason, since a full
19-job run against an unchanged tree spends the expensive part of the file to
learn nothing. The gates it runs are the same ones that run locally, and they
are green.

**Windows is zero.** Every verb prints `shep does not yet support Windows` and
exits 1. That is a documented state, not a broken build: the workspace
cross-compiles for the target, and the README, the crate descriptions and the
`shep-daemon` readme all say plainly that supervision is unix only. An alpha
is allowed to have an unsupported platform. A `1.0.0` is not.

**One v1.0 spec item is still unbuilt.** `shep serve`, `shep dev` and `shep
runtime` shipped in Phase 15, and lookout's search/filter, its action keys,
and lambs in its detail pane shipped in Phase 16; none of them are among the
remainder anymore. What is named in [specs/deferred.md](specs/deferred.md)
is down to one thing: OTLP export on the metrics dog, and it does not change
the behaviour of what does exist. It is why the version is an alpha. `.js`
Flockfiles, the schemars schema, the CLI-flag config layer, and openrc/BSD
`rc.d` units shipped in Phase 14, see the two paragraphs below for what that
means for this release, specifically.

**The three new init scripts are rendered and pinned by exact-string tests,
and have not been executed on FreeBSD, OpenBSD, or an openrc host.** No such
host exists in this project. Nothing claims support for those platforms
until somebody reports back from one; `shep startup` will happily render a
script for an init system nobody here has ever booted it under.

**`crates/shep-core/assets/flockfile.schema.json` is a committed, generated
artefact that ships in shep-core's tarball**, because
`crates/shep-core/src/config/flockfile.rs` `include_str!`s it — that is why
it lives inside the package directory rather than at the repository root.
Regenerate it with `cargo run -p shep -- schema >
crates/shep-core/assets/flockfile.schema.json` before a release if
`AppConfig` changed, though the drift test in `flockfile.rs` will have told
you already: `cargo test -p shep-core` fails first.

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

## The `shep-cli` package was renamed to `shep`

Decided and done 2026-08-15. Checked that day: `shep`, `shep-core`,
`shep-daemon`, `shep-client` and `shep-cli` were all free on crates.io.

The binary was already called `shep` while the crate that built it was named
`shep-cli`, so users would have installed it as `cargo install shep-cli` and
then run `shep`. Renamed the package (not the directory — the checkout still
keeps `crates/shep-cli/`, since Cargo takes the published name from the
manifest, not the path) to `shep` so the install command and the binary name
match: `cargo install shep`, then run `shep`.

That leaves `shep-cli` unclaimed and sitting one character off `shep`'s own
namespace — an obvious squat target once the other three crates are visible
under a `shep-*` naming convention. `shep-cli` is published as a real
redirect crate (see `crates/shep-cli-redirect/`) for exactly that reason:
nothing was ever published under the old name, so the redirect is purely
defensive, not a migration path anyone needs to follow.

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
