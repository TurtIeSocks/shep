# Distributing shep

**`cargo install shep` is still the only way to install shep.** Every channel
below is a thin wrapper around a download URL, so none of them can start until
that URL exists. That one workflow is most of the work; the manifests on top
of it are small.

`.github/workflows/release-artifacts.yml` is that workflow, and as of
2026-08-29 it has run against a real release and FAILED. Seven of its eight
target legs built. `aarch64-unknown-linux-musl` did not: it runs natively on
`ubuntu-24.04-arm`, and something asked rustup for
`stable-x86_64-unknown-linux-gnu`, which an ARM runner refuses with
"toolchain may not be able to run on this system". Because that leg failed,
the Homebrew, Scoop, SHA256SUMS, Chocolatey, deb and WinGet jobs downstream of
it were all skipped.

So shep 0.1.13 is on crates.io carrying no binaries and no package-manager
artifacts, and the sentence above stays true for a reason nobody intended.
Fix that leg before reading the rest of this file as a plan.

Everything downstream of it is written too, and every publishing step is
switched off. Nothing here reaches a package manager, a second repository or
a third-party account until somebody sets a repository variable. The next
section is the whole list.

## Turning a channel on

Each publishing job is gated on a repository variable. Setting the variable
is the act of turning the channel on, and for the three that push somewhere
else the variable is the destination, so nothing here hardcodes an owner or a
repository name.

| Channel | Variable | Secret | Also needs |
|---|---|---|---|
| `.deb` | `PUBLISH_DEB=true` | none | nothing |
| Homebrew | `HOMEBREW_TAP_REPO=<owner>/homebrew-shep` | `HOMEBREW_TAP_TOKEN` | the tap repository |
| Scoop | `SCOOP_BUCKET_REPO=<owner>/scoop-shep` | `SCOOP_BUCKET_TOKEN` | the bucket repository |
| Chocolatey | `PUBLISH_CHOCOLATEY=true` | `CHOCO_API_KEY` | an icon |
| WinGet | `WINGET_IDENTIFIER=<Publisher>.shep` | `WINGET_TOKEN` | a fork, one manual submission |

The secrets are not all the same kind of thing. `HOMEBREW_TAP_TOKEN` and
`SCOOP_BUCKET_TOKEN` are GitHub tokens with contents write on the repository
they push to, and they exist because the default `GITHUB_TOKEN` cannot reach
another repository. `WINGET_TOKEN` is a GitHub token too, but scoped
`public_repo` against a fork of microsoft/winget-pkgs, since that channel
opens a pull request rather than pushing. `CHOCO_API_KEY` is not a GitHub
credential at all: it is an API key from a chocolatey.org account.

**The `.deb` is the cheapest one to turn on** and the only one that needs
nothing outside this repository. It is `dpkg -i` rather than `apt install`,
for the reason the apt section below gives.

Three of the five are waiting on decisions rather than on work:

- Homebrew and Scoop both want a repository that does not exist. Naming it is
  the open question, and `github.com/shep` is taken by a dormant account
  created in 2008 with no public repositories, so an organisation called
  `shep` is not available.
- WinGet's `PackageIdentifier` embeds a publisher name, so it waits on the
  same decision. Its first version also has to be submitted to
  microsoft/winget-pkgs by hand: the release action checks that the package
  already exists and refuses otherwise.
- Chocolatey wants artwork. `iconUrl` is validator Guideline CPMR0033 rather
  than a Requirement, so the package is approvable without one, and CPMR0076
  forbids a raw GitHub URL once there is one.

## The prerequisite: `release-artifacts.yml`

A workflow that builds a matrix, archives it, checksums it, and attaches the
result to the release release-plz already creates.

Four decisions get frozen the moment it runs, because each one is copied
into every downstream manifest afterwards. Getting them wrong means
renaming a published URL later.

1. **Tag scheme `shep-v{version}`.** release-plz sets no `git_tag_name`, so
   it uses `{{package}}-v{{version}}`. Confirmed on the repository: the
   newest tag is `shep-v0.1.12`. Every Homebrew `url`, Scoop `url`, WinGet
   `InstallerUrl`, Chocolatey `$url64` and `curl -LO` line embeds this
   string.
2. **Archive name `shep-{target}.tar.gz`**, `.zip` on Windows.
   `taiki-e/upload-rust-binary-action` defaults its archive name to
   `$bin-$target`, so a `bin:` list of three would emit three archives per
   target. Set `archive: shep-$target` explicitly.
3. **All three `[[bin]]` targets in every archive.** `shep`,
   `shep-runtime`, `shep-dev`, matching what `cargo install shep` already
   puts on a machine. Downstream packages decide separately which ones to
   expose.
4. **Seven targets.**

| Target | Runner | Toolchain note |
|---|---|---|
| `aarch64-apple-darwin` | macos-latest | native |
| `x86_64-apple-darwin` | macos-latest | cross, Xcode clang covers it |
| `x86_64-unknown-linux-gnu` | ubuntu-latest | native |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm | native |
| `x86_64-unknown-linux-musl` | ubuntu-latest | `apt-get install musl-tools` |
| `aarch64-unknown-linux-musl` | ubuntu-24.04-arm | native, so no cross `cc` |
| `x86_64-pc-windows-msvc` | windows-latest | native |

`ring` runs a `cc` build script on every target, which is why the two arm
legs build natively on `ubuntu-24.04-arm` rather than cross-compiling from
x86_64. `test.yml` already proves six of these seven build, and the musl
leg there runs a full `cargo test`.

**Use msvc for Windows, never gnu.** `x86_64-pc-windows-gnu` in this
repository is `cargo check` only, cross-compiled from ubuntu, and has never
executed a shep binary anywhere. msvc is the target `test.yml`'s
windows-latest legs run `cargo test` against.

The license files are already handled and this document previously said
otherwise. Each published crate directory carries `LICENSE-MIT` and
`LICENSE-APACHE` as symlinks to the two at the repository root, added
2026-08-19 in `1c2fe2f`, and cargo dereferences them into the tarball.
Verified by unpacking `shep-0.1.12.crate`: both are there. The `.deb` can
take them from the crate directory like everything else.

Also in the same change: `actions/attest-build-provenance`. Sigstore
keyless, no key to store, no rotation, and both Homebrew and Chocolatey
care about checksums anyway.

Expect these builds to be slower than CI's. `[profile.release]` in the
workspace manifest sets `lto = "thin"` and `codegen-units = 1`, which is
the right trade for a shipped binary and the wrong one for a fast job.

## The trigger, and the four releases

`RELEASE_PLZ_TOKEN` is a real PAT and `.github/workflows/release-plz-release.yml`
passes it directly, with no `secrets.GITHUB_TOKEN` fallback. That matters:
GitHub suppresses downstream workflow triggers for events created by the
default token, silently, to stop recursive chains. Because the PAT is in
play, `on: release: [published]` genuinely fires here. Without it the only
option would be `workflow_run` against the workflow named `Release`.

**One publish creates four tags and four GitHub releases.** The four crates
share `version_group = "shep"`, so 0.1.12 produced `shep-v0.1.12`,
`shep-core-v0.1.12`, `shep-client-v0.1.12` and `shep-daemon-v0.1.12`,
stamped within 42 seconds of each other. A `release: published` workflow
with no filter runs four times, three of them for crates that ship no
binaries.

`startsWith(github.event.release.tag_name, 'shep-v')` matches exactly once,
but only by coincidence: the siblings happen to start `shep-c` and
`shep-d`. A fifth crate named `shep-vault` would break it silently. Prefer
checking that the tag matches the `shep` package's own version.

## Why not cargo-dist

dist is maintained and there is a documented release-plz integration for
it, which needs `git_release_enable = false` so dist owns the GitHub
release instead.

Handing release ownership away is the argument against. `release-plz.toml`
and the two release workflows carry a long record of getting that boundary
wrong and fixing it, most recently on 2026-08-27. dist also adds a second
generated CI surface that has to be regenerated and kept in sync, which is
the same drift class `CLAUDE.md` already documents for the generated CLI
reference and the Flockfile schema.

For seven targets CI already covers, `taiki-e/upload-rust-binary-action` is
ordinary YAML with no new config file and no ownership change. Revisit dist
once a tap and a Chocolatey package are both real, because its automatic
formula push and MSI generation start paying for themselves there.

## Homebrew

A tap, not homebrew-core. Homebrew's Acceptable Formulae page asks for 75
stars, 30 forks or 30 watchers, and a project at least 30 days old. This
repository was created 2026-08-08 and had 4 stars, 0 forks and 0 watchers
on 2026-08-29. The age floor clears on 2026-09-07; the notability one does
not clear on its own.

The "it is already on crates.io" objection does not apply. Homebrew's
language-specific formulae policy excludes libraries, and welcomes
command-line applications.

So: `shep-pm/homebrew-shep`, giving `brew install shep-pm/shep/shep`,
with `Formula/shep.rb`.

The formula is written and lives at `packaging/homebrew/shep.rb`, which is
its source of truth. `release-artifacts.yml`'s `homebrew` job rewrites the
two version lines and pushes the result to the tap on each release, and
`test.yml`'s `formula` job runs `brew style` over it. Three things are still
needed, all of them outside this repository: create the tap repository, put
the current formula in it as `Formula/shep.rb`, and give this repository a
`HOMEBREW_TAP_TOKEN` secret holding a token with contents write on the tap.
Then set `HOMEBREW_TAP_REPO` to its `<owner>/<name>`.

A separate repository is not strictly required. The `homebrew-` prefix is
only hardcoded for the one-argument form of `brew tap`; the two-argument form
takes any URL, and Homebrew looks for formulae in `Formula/`,
`HomebrewFormula/` or the tap root. So this repository could be its own tap.
It should not be, for two measured reasons. `brew tap` does a full clone with
no `--depth`, so every user would fetch a 9 MB Rust workspace and refetch it
on every `brew update` to learn whether a 70-line file moved. And this
repository's `main` ruleset requires a pull request and status checks, with
bypass only for the Admin role, so the release job would have to open a pull
request somebody merges every time.

The formula pulls the crate tarball from `static.crates.io` rather than a
GitHub tag archive. crates.io publishes each version's sha256 as its
`checksum`, so the bump job reads the hash from the API instead of
downloading anything, and the crate tarball is immutable by contract where a
GitHub archive is generated on the fly. The bump job polls that API rather
than reading it once: release-plz creates the GitHub release and uploads to
crates.io in the same run with no ordering between them, so the version can
lag the event that starts the job by a few seconds.

**Start with a build-from-source formula** even though the artifacts exist
by then. Bumping one is a single sha256 recomputation, which
`dawidd6/action-homebrew-bump-formula` handles cleanly. A binary formula
carries seven url and sha256 pairs and `brew bump-formula-pr` is much worse
at that. Switch after two or three clean releases.

The cost of that choice is real: every `brew install shep` compiles the
whole tree, `ring` and tokio and ratatui and rmcp included. Minutes, not
seconds.

Formula details that are easy to get wrong:

- `license any_of: ["Apache-2.0", "MIT"]` for `MIT OR Apache-2.0`.
- No `--bin` flag. `std_cargo_args` installs all three `[[bin]]` targets,
  and supplies `--locked`, which works because the crate tarball carries
  `Cargo.lock`.
- `generate_completions_from_executable(bin/"shep", "completions")`. shep's
  form is `shep completions <shell>`, which is the helper's default shape,
  so no `shell_parameter_format` override. shep writes that verb's status
  line to stderr, so the generated scripts stay clean.
- `brew style` has to see the file at a `Formula/` path. Linting it in
  place reports a `Style/FrozenStringLiteralComment` offence that no
  formula in a real tap has, because Homebrew's rubocop config keys several
  cops off the path. The CI job copies before it lints.

**No `service do` block.** shep installs its own launchd plist through
`shep startup`, and `brew services` would install a competing one for the
same daemon. Say so in `caveats` instead.

## apt

Two layers that get conflated, and only the first is cheap.

**Building the `.deb`** is done. `[package.metadata.deb]` lives in
`crates/shep-cli/Cargo.toml` and `release-artifacts.yml`'s `deb` job builds
both architectures, gated on `PUBLISH_DEB`.

That job does not compile anything. It downloads the musl archives the matrix
already uploaded, unpacks them where `cargo deb --no-build` looks, runs the
binary to generate the completions, and repacks. A second twenty-minute
release build becomes about thirty seconds. The arm64 leg runs on
`ubuntu-24.04-arm` for one reason: the completions are generated rather than
checked in, so the job has to be able to execute the binary it is packaging.

Verified locally against cargo-deb rather than written from the README: a
real package builds, and its nine assets land at `usr/bin/`,
`usr/share/doc/shep/` and the three completion directories Debian expects.
One correction fell out of that, worth recording because the README is wrong
about it. `license-file` takes a plain string, not the documented
`["path", lines]` pair, which fails to parse.

Build against **musl**, not glibc. `depends = "$auto"` writes whatever
glibc the runner linked against, `ubuntu-latest` is 24.04 (glibc 2.39), and
a `.deb` requiring 2.39 refuses to install on Debian 12 or Ubuntu 22.04.
Those are a large share of anyone who would type `apt install shep`. A
static musl build makes the question disappear. Two legs: amd64 on
ubuntu-latest, arm64 on ubuntu-24.04-arm.

**Ship no systemd unit, and never call `shep startup` from `postinst`.**
That would write to a user's systemd state at `dpkg -i` time, before they
have a Flockfile, and it contradicts shep's own opt-in model. `shep
startup` already renders and installs the unit when the operator asks for
it.

**Hosting is the other 80%, and it stays deferred.** A `.deb` attached
to a GitHub release is not `apt install`. That needs a signed repository:
`Packages` and `Release` and `InRelease`, a GPG key, an HTTP host, and a
`.sources` file plus a keyring in `/etc/apt/keyrings` on every user's
machine. The recurring cost is not the CI job, it is key rotation, which
requires every existing user to act manually and cannot be automated away.
reprepro or aptly state also has to survive between runs or old versions
vanish from the index.

Document the `dpkg -i` path plainly instead, including what it does not
give: no `apt upgrade`. A repository that quietly goes stale is worse than
an honest download link.

## Windows: Scoop, then WinGet, then Chocolatey

Chocolatey is the one people name and the one to do last. Its packaging is
mechanical and its moderation queue is not: automated validation, a
VirusTotal scan that unsigned Rust binaries routinely trip, and human
review, measured in days to weeks on a first submission with no trusted
status.

Scoop is close to free. `packaging/scoop/shep.json` is written, and the
`scoop` job pushes it to whatever `SCOOP_BUCKET_REPO` names. No review, and
it is where a lot of the CLI audience on Windows actually looks. The manifest
carries `checkver` and `autoupdate` as well, so a bucket running Scoop's
excavator workflow could bump itself without this repository's help.

WinGet ships with current Windows and its review is lighter than
Chocolatey's for a well-formed submission. It is the one channel with nothing
under `packaging/`, because its manifests are generated per version rather
than maintained: the `winget` job runs the WinGet Releaser action against
`WINGET_IDENTIFIER`. Two things have to happen first. The identifier embeds a
publisher name, so it waits on the naming decision, and the first version has
to be submitted to microsoft/winget-pkgs by hand, since the action refuses
when the package is not already there. The action's default
`installers-regex` matches by installer extension and would find nothing in a
portable `.zip`, so the job sets its own.

The Chocolatey package is written and lives in `packaging/chocolatey/`:
`shep.nuspec`, the two scripts, `LICENSE.txt`, `VERIFICATION.txt` and the two
`.exe.ignore` files. `release-artifacts.yml`'s `chocolatey` job packs and
pushes it, and stays inert until the repository variable
`PUBLISH_CHOCOLATEY` is set to `true` and a `CHOCO_API_KEY` secret exists.
The parse check on its PowerShell runs in `test.yml`'s `packaging` job.

Two things are still open on it. The first is artwork: `iconUrl` is missing,
which is validator Guideline CPMR0033 rather than a Requirement, so the
package is approvable without it, and CPMR0076 forbids linking a raw GitHub
URL when one exists, so it has to go through a CDN like jsdelivr. The second
is the moderation queue itself, which cannot be shortened from here.

What the package already settles:

- **Shim `shep.exe` only.** Chocolatey auto-shims every `.exe` in the tools
  directory. `shep-runtime` and `shep-dev` are container entrypoints with
  no desktop use case. The `.exe.ignore` files must ship as package files
  in the `.nupkg`, not inside the downloaded zip, because
  `Install-ChocolateyZipPackage` unpacks into the tools directory and
  shimgen scans it afterwards. The filename match is case sensitive.
- `iconUrl` needs an image this repository does not have. `web/public/`
  carries `favicon.ico` and `favicon.svg` and no PNG.
- The uninstall script refuses while a shepherd is running rather than
  stopping the flock itself. `shep ping` is the probe, since it is the one
  verb that treats "no shepherd" as information rather than an error.
  Windows will not delete a running executable either way, so the
  alternative is not "uninstall quietly" but "uninstall half-fails with a
  confusing message".

**The package description has to name the Windows gaps.** `shep startup` is
not built there, because boot-time supervision on Windows means a Service
Control Manager service rather than a fifth unit template. `user` and
`group` refuse permanently. Windows has nothing SIGTERM-shaped that can be
delivered to an arbitrary process, so `shep stop` waits out the app's whole
`kill_timeout` before terminating it, unless the app opts into the
shepherd channel with `shutdown_with_message`. A Windows user installing a
process manager reaches for "run this at boot" first, and that is the one
thing it cannot do. `web/src/pages/docs/not-built.astro` already says all
of this and is the text to reuse.

## Order

0. Merge the Windows tier. Done, PR #16.
1. `release-artifacts.yml`. Done.
2. README and `web/` install docs. One pass, appended to as channels land.
3. `.deb` on the release. Written. Set `PUBLISH_DEB`.
4. Homebrew tap. Formula written, in `packaging/homebrew/`. Blocked on a
   repository name.
5. Scoop bucket. Manifest written, in `packaging/scoop/`. Blocked on the
   same name.
6. WinGet. Job written. Blocked on the same name, plus one manual
   submission.
7. Chocolatey. Package written, in `packaging/chocolatey/`. Blocked on
   artwork, then on moderation.

The engineering is done. What is left is four decisions and some clicking:
a name for wherever the tap and the bucket live, artwork, a WinGet first
submission, and the tokens. Chocolatey's moderation queue is the only part
measured in days rather than minutes, which is the whole argument for its
position.

## What each release costs afterwards

Merging the release pull request stays the only thing that triggers a
release. release-plz tags and publishes, the artifact workflow builds and
uploads, the `.deb` rides the same matrix, and the tap, Scoop and WinGet
bumps hang off the release event.

It is not the only manual act, though, because of the first of the two costs
below.

Two recurring costs are real:

**Chocolatey's VirusTotal scan reruns on every new binary hash.** A package
that was clean last release can pick up a fresh false positive from an
unrelated antivirus vendor's model update, so someone has to look each
time. Trusted status is granted after several clean versions and drops this
to watching an automated push.

**Nothing enforces the duplication.** After this work, "shep ships three
binaries named `shep`, `shep-runtime` and `shep-dev`" lives in
`crates/shep-cli/Cargo.toml`, the artifact workflow's `bin:` list, the
`.deb` assets array, Chocolatey's `.ignore` files and the Homebrew formula.
The target list lives in `test.yml` and in the artifact matrix. Same silent
drift as the generated CLI reference and the Flockfile schema, and the same
cheap mitigation: a header comment in `release-artifacts.yml` naming the
two upstream sources of truth. Add a fourth binary and four files need
editing, with nothing to tell you.
