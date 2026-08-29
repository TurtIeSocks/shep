# The community dog index: design

**Date:** 2026-08-20
**Status:** participate-mode design, both sections approved by the maintainer
**Scope:** the index data and the page that renders it. Not `shep install`.

## The ask

The maintainer, 2026-08-20: the docs site should carry a community list of dogs, held as
a JSON the Astro app reads at build time and turns into a page. Two payoffs
she named: contributing a dog becomes a pull request, and the same file is an
index a future `shep` command could read.

This came out of building `shep-log-rotate`, the first fully external dog, and
sits next to the deferred dog-manifest question in `docs/specs/deferred.md`.

## Scope, decided before anything else

Three things were on the table and they have very different trust profiles:

| | What shep takes on | New trust |
|---|---|---|
| index + page | nothing; a JSON the site renders | none |
| dog manifest | shep parses a file a dog author wrote | moderate |
| `shep install` | shep downloads and runs a third-party binary | large |

**Only the first is in scope.** The index page alone is most of the value and
costs nothing: a reader browses the page, copies an install line, and runs
`shep adopt`. That works today with no new shep code and no new trust surface.
The index being machine-readable is free, because it is a JSON either way.

`shep install` changes the character of the thing. shep would fetch a binary
and hand it to `adopt`, which vets by **executing** the candidate with the
operator's inherited environment (`crates/shep-cli/src/commands/dogs.rs:384`,
and see `deferred.md`). Today that risk is bounded by the operator having
chosen to install the thing themselves. An index makes shep the chooser, at
which point checksums, pinning and signatures stop being polish and become
the work. Deferred deliberately, not rejected.

## What a listing claims

**That the dog exists. Nothing more.** An entry means: a real repository, an
open licence, and it is actually a dog. It is explicitly not a security or
quality claim, and the page says so where a reader is about to act on it.

Chosen over a reviewed list because review is unbounded: every pull request
would be a code review of a stranger's process-supervisor plugin, and it
would have to be repeated on every version the author pushes, which the index
cannot see. Existence-only can be tightened later; a promise of review cannot
be walked back.

## 1. The data

**One file, `web/public/dogs.json`.** The page imports it at build time and
the published site serves it at `/dogs.json`. No copy step, no generator, and
the stable URL is what a future `shep` command would read.

`public/` rather than `src/data/` deliberately. The site's existing data lives
in `src/data/` as TypeScript modules, which is right for data only the site
consumes. This file has a second consumer that cannot import a `.ts` module.

### The entry

```json
{
  "name": "Spot",
  "package": "shep-log-rotate",
  "adopt_as": "log-rotate",
  "description": "Rotates grown log files and asks the shepherd to reopen them.",
  "repo": "https://github.com/TurtIeSocks/shep-log-rotate",
  "license": "MIT OR Apache-2.0",
  "category": "logs",
  "source": { "kind": "cargo-git", "url": "https://github.com/TurtIeSocks/shep-log-rotate" }
}
```

Every field is required. Eight of them, each doing work:

- **`name`** -- the dog's own name, unique across the index, enforced at build.
  Displayed as `shep-log-rotate (Spot)`. Unique rather than decorative because
  it costs one validation today and keeps a human handle available later:
  `shep install spot` reads better than `shep install shep-log-rotate`, and a
  name people remember is the kind of thing that makes an ecosystem feel like
  one.
- **`package`** -- the crate or repository name, the real identity.
- **`adopt_as`** -- the name the dog expects to be adopted under. See below;
  this is the field that is not obvious and matters most.
- **`description`** -- one line, what it does.
- **`repo`** -- HTTPS URL. There is no `author` field: the repo URL already
  carries it, and a field duplicating another is a field that goes stale.
- **`license`** -- SPDX string. Part of the listing bar, and useful to
  somebody deciding whether to adopt.
- **`category`** -- one of the enum below.
- **`source`** -- tagged, see below.

### Why `adopt_as` exists

A dog is given no argv and one environment variable, so it cannot be told the
name it was adopted under, and that name is the `[dog.<name>]` key its
configuration lives under. `DogConfig` for a name nobody adopted returns the
empty string, which is exactly what a registered dog with no section returns.
So adopting a dog under the wrong name silently discards the operator's entire
configuration for it, with no error from either side. Recorded in
`deferred.md` as a shep-side gap.

The index is the natural place to close it. Showing `shep-log-rotate (Spot)`
invites somebody to type `shep adopt Spot ./shep-log-rotate`, which would
break the dog quietly. Carrying `adopt_as` makes the page's install line
correct by construction rather than by the reader's guess, and turns the list
from a directory into an answer to a real failure mode.

### Categories

A fixed enum, so the page groups cleanly and a typo fails the build rather
than creating a section of one:

`logs` -- `metrics` -- `alerts` -- `health` -- `deploy` -- `other`

Plain nouns deliberately. `docs/terminology.md`'s rule is that the theme never
costs clarity, and somebody scanning for a log rotator should not have to
decode a pun to find one. `other` exists so nothing is unlistable.

### `source`, and the trap in it

```json
{ "kind": "cargo-git",  "url": "https://github.com/..." }
{ "kind": "go-install", "module": "github.com/..." }
{ "kind": "manual",     "instructions": "..." }
```

Tagged rather than a freeform string, and the reason is a distinction that is
easy to miss: **"how do I install this" and "what artifact would shep fetch"
look like one field and are two.** If `shep install` ever exists, the sane
version downloads a binary it can checksum, not runs `cargo install`, which
means invoking a compiler and arbitrary build scripts over the network on the
operator's behalf.

A freeform string is fine for a human today and useless to a machine tomorrow,
and it is a one-way door: it cannot be machine-read later without going back
to every contributor. Tagged kinds are machine-readable from day one without
shep acting on them, and adding a `release` kind carrying binary URLs and
checksums is additive rather than breaking.

`manual` exists so a dog written in something with no one-line installer is
still listable. The page renders its `instructions` as prose rather than a
command block, so nothing looks copy-pasteable that is not.

## 2. Validation

Build-time, failing the site build with a named message. The JSON is data, so
the build is the only thing that can keep it honest -- the same discipline the
generated CLI reference already gets.

Refusals:

- a duplicate `name`, naming both entries
- an unknown `category`, listing the six that exist
- a missing or empty required field, naming the field and the entry
- a `repo` that is not HTTPS
- a `source.kind` that is not one of the three, listing them
- a `manual` source with empty `instructions`

A `dogs.schema.json` sits beside the data so an editor validates an entry as a
contributor types it. shep already plays this trick with the schemars-exported
Flockfile schema. The build validator is the gate; the schema is the courtesy
that stops most pull requests being wrong before they are opened.

## 3. The page

`web/src/pages/docs/community-dogs.astro`, at `/docs/community-dogs`. Flat,
matching every other page's slug convention, with an entry in
`web/src/data/docsNav.ts` in the same group as `dogs`. `ReferencePills`
requires at least a Source pill, so the nav entry carries `source:
"docs/dogs.md"`.

Top to bottom:

1. **The disclaimer once, in a `Callout`, at the top.** Listed means a real
   repo, an open licence, and that it is actually a dog. Not reviewed, not
   audited, not endorsed. Adopting one runs it at the shepherd's trust level,
   which is the sentence `/docs/dogs` already makes, repeated where somebody
   is about to act on it.
2. **One section per category**, in the enum's order. A category with no
   entries is omitted rather than rendered empty.
3. **Each entry**: `package (Name)` as the heading, the description, the
   licence, a repo link, and two copy-pasteable lines -- the install command
   its `source.kind` implies, and the matching `shep adopt` line.

   Per kind, and the second line is where the detail matters:

   | `kind` | install line | adopt line |
   |---|---|---|
   | `cargo-git` | `cargo install --git <url>` | `shep adopt <adopt_as> ~/.cargo/bin/<package>` |
   | `go-install` | `go install <module>@latest` | `shep adopt <adopt_as> $(go env GOPATH)/bin/<package>` |
   | `manual` | the `instructions` as prose | `shep adopt <adopt_as> <path to the binary>` |

   **The install path is an assumption, not a fact**, and the page says so in
   one line rather than pretending: a crate's binary target need not share its
   package name, and `CARGO_INSTALL_ROOT` or `GOBIN` move the destination. The
   line is right for the common case and wrong quietly otherwise, so it is
   worth a sentence telling the reader to check where the installer actually
   put the file. `shep adopt` refuses a path that is not there, which makes a
   wrong guess loud rather than silent.
4. **Add yours**, at the bottom: edit `web/public/dogs.json`, open a pull
   request, and a link to the schema.

The page's own prose is public-facing copy and gets a `humanizer` pass before
it ships, per the global rule.

## 4. Contribution and removal

Contributing is a pull request editing one JSON file. No account, no registry,
no tooling to install.

An entry whose repository has vanished, or which turns hostile, comes off by
pull request or by the maintainer directly. The list makes no ongoing claim, which is
exactly why existence-only was the right bar: there is nothing to retract.

## 5. Deliberately deferred, and compatible with all of it

- **`source: { kind: "release", url, sha256, target }`** -- purely additive
  when shep can fetch binaries.
- **`shep dogs --available`** -- fetches `/dogs.json`, prints a table,
  installs nothing. Worth naming because it is the honest next step: near-zero
  trust cost, it exercises the machine-readable half, and it is a GET added to
  plumbing that already does TLS. `shep-cli` carries a hand-rolled HTTP/1.1
  client over `tokio-rustls` for the bark dog's webhooks, chosen over
  `reqwest` at +10 crates against +93. It is POST-only today.
- **`shep install`** -- needs the supply-chain design. Out of scope.
- **The dog manifest** -- still deferred, still recorded in `deferred.md`. The
  index arguably reduces the pressure for it: `adopt_as` and the defaults a
  dog documents with `--print-config` cover most of what a manifest was
  reaching for, without shep parsing a file a third party wrote.

## 6. Testing

- The validator has unit tests: one per refusal above, each asserting the
  message names the offending entry and field. A validator that fails without
  saying which of forty entries is wrong is not much better than none.
- A test that the real `dogs.json` passes its own validator, so a bad entry
  fails in CI rather than at somebody's next local build.
- `cd web && npx astro build` must pass. This change trips the project's docs
  hard trigger: a new page and a new build-time failure mode, neither of which
  the Rust gate can see.

## 7. Assumptions

Judgement calls made while drafting, all confirmed with the maintainer except where noted:

1. `web/public/dogs.json` rather than `src/data/`, because the file has a
   second consumer that cannot import a TypeScript module. Not raised
   separately; stated in Section A and approved.
2. Validation fails the build rather than warning. Same.
3. No `author` field; the repo URL carries it.
4. `manual` instructions render as prose, not a command block.
5. Empty categories are omitted rather than shown empty.
6. The nav entry reuses `docs/dogs.md` as its Source pill, since the page has
   no doc of its own and `ReferencePills` requires one.
7. The rendered adopt line assumes a crate's binary is named after its
   package and lands in the default install root. Stated on the page as an
   assumption rather than presented as fact, because it is wrong for a crate
   whose `[[bin]]` name differs and for anyone who has set
   `CARGO_INSTALL_ROOT` or `GOBIN`.
