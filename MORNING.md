# Morning brief — 2026-08-15

Written overnight. Delete it whenever; it is a status note, not a doc.

## The short version

The **website is finished and ready to deploy**. The **workspace is ready to
publish**, and publishing is one command. **Phase 13 (whistle) is about half
built.** Phases 12b and 14 have finished, reviewed plans and have not started.

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

Phase 13 (whistle, the MCP server) has its dependency, its config section, its
control gate, its shepherd bridge and its payload twins. The nine tools, the
verb, the catalogue and the e2e tier are not built yet.

Phases 12b (lookout's other three panes) and 14 (`.js` Flockfiles, the schema
export, the daemon flags layer, openrc and BSD units) have plans that have been
through an adversarial read and a revision. Neither has started.

Phase 15 (`serve`, `dev`, `runtime`) has not been planned. Windows is last, by
your earlier call.

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
