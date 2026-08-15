# Morning brief — 2026-08-15

Written overnight. Delete it whenever; it is a status note, not a doc.

## The short version

The **website is finished and ready to deploy**. The **workspace is ready to
publish**, and publishing is one command. **Phase 13 (whistle) is built, green
and reviewed** — the whistle MCP server works. **Phase 12b (lookout's last
three panes) is building as you sleep.** Phase 14 has a finished plan and has
not started, and Phase 15 — the last unplanned phase — now has one too.

Nothing is pushed. Nothing is published. Nothing is tagged. All of that is
yours.

## What you can do first thing

### Deploy the site

Cloudflare Pages, and `web/README.md` has the first-time steps. The short of it:
point it at this repo, root directory `web`, build command `npm run build`,
output `dist`, Node version from `.nvmrc`.

It is Cloudflare rather than GitHub Pages for two real reasons, not taste. The
repo is private and Cloudflare builds private repos on the free plan, where the
GitHub Pages equivalent is a paid feature. And the site's internal links are
root-relative, so a GitHub Pages project subpath would 404 seventeen of them.

Three pages exist: the landing scene, the docs (Getting started and Terminology
written, eight honest stubs), and the design-language reference. 13 routes,
build green, light and dark.

### Publish the crates

`docs/releasing.md` is the checklist. Two things to read before you run anything.

**The version lives in two places.** `[workspace.package] version`, and three
literals inside `[workspace.dependencies]`. There is no `version.workspace =
true` shorthand inside a dependency entry. Bump one without the other and
`shep-core` publishes fine while `shep-client` requires a version that by semver
excludes every `0.1.0-alpha.*`, failing three crates partway into a sequence you
cannot undo.

**The recommendation is `0.1.0-alpha.1`, not `0.1.0`.** A crates.io version is
permanent; yanking hides a number without freeing it. A first workspace publish
is where packaging faults surface, and on an alpha the fix costs nothing.

`cargo publish --workspace --dry-run` passes today and packages all four.

### One decision waiting for you

`shep`, `shep-core`, `shep-client`, `shep-daemon` and `shep-cli` are **all
unclaimed** on crates.io as of last night. The binary is already called `shep`
while the crate that builds it is `shep-cli`, so renaming that package would
make `cargo install shep` work instead of `cargo install shep-cli`. Names are
first-come. Worth claiming `shep` either way.

## Where the code got to

**Phase 13 (whistle) is done.** Eleven tasks, all nine MCP tools, the verb, the
catalogue and an e2e tier. `cargo test --workspace --all-features` is EXIT=0 at
1256 passed / 0 failed / 5 ignored. Its review returned six findings and all six
are fixed; I re-ran the suite myself afterwards, because the agent that fixed
them was cut off before it could.

One of those findings is worth thirty seconds of your time, because it was a
claim rather than a bug. `shep whistle --help` used to tell you the control gate
deliberately has no flag, since "a flag would let the same edit that adds this
server open the gate". That was false: `--home` is a global flag reading
`$SHEP_HOME`, so `shep whistle --home /tmp/open` is exactly that one line and it
opens the gate. The phase's own test proves it. Nothing about the gate changed —
it was never a security boundary, and whistle runs as you — but the help text
now says the true reason, which is that a config file is auditable where a
per-invocation setting is not.

**Phase 12b is running right now** — the bleats feed, the sheep detail pane and
the host-usage strip, ten tasks. Check `.superpowers/sdd/progress.md` for where
it got to.

**Phase 14** (`.js` Flockfiles, the schema export, the daemon flags layer,
openrc and BSD units) has a plan through an adversarial read and a revision, and
has not started.

**Phase 15** (`serve`, `dev`, `runtime`) is now planned — written, reviewed
against 27 findings, revised, re-swept, tightened. It is last because it
extracts shep-cli into a library with three thin binaries, which nothing else
needs. Windows is after that, by your earlier call.

### One Phase 15 decision I would like you to look at

Closing a real race in `shep serve` costs something an operator will notice.
To refuse a symlink attack without re-checking the path on every request, the
resolver refuses **any** symlink component — not only ones that escape the
docroot. So `dist/current -> ../releases/2026-08-15`, and a symlinked
`assets/`, now 404 where pm2's serve would serve them, and the operator gets no
explanation with the 404.

It is written down as a deliberate cost in the plan, the ledger and
`migration.md`. But if you would rather keep the layout working and accept the
race, that is your call and it is cheaper to make now than after Phase 15 runs.

## Things worth your eye, none urgent

- The landing hero's sheep and dog cluster at 375px is legible but not elegant.
  Improving it means redesigning the scenery composition, which felt bigger than
  a mobile pass.
- The two BSD init scripts Phase 14 plans **cannot be executed by anyone on this
  project** — there is no FreeBSD, OpenBSD or openrc host here or in CI. They
  will ship as text with exact-string tests, the same tier the systemd unit has
  always had on a Mac. No doc will claim BSD support until someone reports back.
- CI still has never run automatically. It is dispatch-only because the repo is
  private and Actions bills macOS at ten times. Worth turning on when the repo
  goes public.
