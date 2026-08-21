# Community Dog Index Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a community dog directory: one JSON that the docs site renders as a page and also serves at a stable URL, so listing a dog is a pull request and the same file is machine-readable later.

**Architecture:** `web/public/dogs.json` is the single source. A validating data module imports it, refuses a malformed entry by throwing, and exports typed entries; a new Astro page imports that module and groups entries by category. Because the module throws, `astro build` fails on a bad entry with no extra tooling. A `node --test` script exercises the validator against deliberately bad fixtures, using Node's built-in runner and type stripping, so nothing is added to `package.json`'s dependencies.

**Tech Stack:** Astro 7, TypeScript 6 (strict), Node's built-in test runner. No new dependencies.

**Spec:** [docs/brainstorming/specs/2026-08-20-community-dog-index-design.md](../../brainstorming/specs/2026-08-20-community-dog-index-design.md). Read it before Task 1; it carries the reasoning this plan does not repeat.

## Global Constraints

- **No new npm dependencies.** The validator uses `node:test` and `node:assert/strict`, both built in. Adding a test framework for one module is the thing this plan is avoiding.
- **No em dashes or en dashes** (U+2014, U+2013) in any prose a reader sees: the page, the JSON's descriptions, error messages. Hyphens only. Check the bytes.
- **The page's own copy is public-facing prose.** Run the `humanizer` skill over it before committing: no rule-of-three stacking, no mechanical boldface, no promotional cadence.
- **Do not touch `crates/shep-cli/src/cli.rs` or `crates/shep-cli/src/commands/init.rs`.** Both carry Rin's own uncommitted work from a teaching session. This plan is `web/`-only and has no reason to go near them.
- **`web/` is published.** Every task ends with `cd web && npx astro build` passing. That is the project's docs hard trigger and the only gate that sees this work at all; nothing in the Rust suite touches `web/`.
- **Existence-only.** The page must never imply review, audit or endorsement. That wording is load-bearing, not decoration.

## Verified facts, measured rather than assumed

Established 2026-08-20 by running them. Use these; do not re-derive.

- **Astro CAN import JSON from `public/`.** `import dogs from "../../public/dogs.json"` resolves and renders. `public/` is special to the copy step, not to the module graph.
- **The same file is STILL served verbatim** at `/dogs.json` in `dist/`. One file, both uses, no copy step.
- **A thrown error in a page's frontmatter fails the build**, exit 1, with the message printed. That is the whole validation mechanism.
- **Astro excludes `_`-prefixed files from routing.** A probe page named `__probe.astro` builds nothing and looks like a broken import. Do not name anything with a leading underscore.
- **Node on this machine is v26.5.0**, runs `.ts` directly with type stripping and no flag, and `node --test` works.
- **`package.json` declares `"node": ">=22.12.0"`**, but type stripping is only default-on from **22.18**. Task 1 raises that floor; see its Step 6.
- **`web/scripts/verify-pagefind-index.mjs` already runs inside `npm run build`.** That is the precedent the new verify script follows.
- The docs sidebar is `web/src/data/docsNav.ts`. Entries carry `slug`, `label`, `built`, `source`, and optionally `spec`/`api`. `ReferencePills` requires at least a Source pill.

---

### Task 1: The data, the validator, and its tests

**Files:**
- Create: `web/public/dogs.json`, `web/src/data/dogs.ts`, `web/public/dogs.schema.json`, `web/scripts/verify-dogs-index.ts`
- Modify: `web/package.json`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `export type DogCategory = "logs" | "metrics" | "alerts" | "health" | "deploy" | "other"`
  - `export const CATEGORIES: readonly DogCategory[]` in display order
  - `export interface DogSource` (a tagged union: `cargo-git`, `go-install`, `manual`)
  - `export interface Dog { name; package; adopt_as; description; repo; license; category; source }`
  - `export function validate(raw: unknown): Dog[]`, which throws `Error` with a message naming the offending entry and field
  - `export const dogs: Dog[]`, the validated real index, which throws at import time if bad

- [ ] **Step 1: Write `web/public/dogs.json` with the one real dog**

```json
[
  {
    "name": "Spot",
    "package": "shep-log-rotate",
    "adopt_as": "log-rotate",
    "description": "Rotates grown log files and asks the shepherd to reopen them.",
    "repo": "https://github.com/TurtIeSocks/shep-log-rotate",
    "license": "MIT OR Apache-2.0",
    "category": "logs",
    "source": {
      "kind": "cargo-git",
      "url": "https://github.com/TurtIeSocks/shep-log-rotate"
    }
  }
]
```

A top-level array, not an object with a `dogs` key. There is one kind of thing in this file and a wrapper would earn nothing.

- [ ] **Step 2: Write the failing tests in `web/scripts/verify-dogs-index.ts`**

Every refusal in the spec gets a case, and each asserts the message names the offending entry so a contributor is told which of forty entries is wrong.

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { validate, dogs, CATEGORIES } from "../src/data/dogs.ts";

/** A valid entry, cloned and broken per case. */
function good(): Record<string, unknown> {
  return {
    name: "Rex",
    package: "shep-example",
    adopt_as: "example",
    description: "Does an example thing.",
    repo: "https://github.com/someone/shep-example",
    license: "MIT",
    category: "other",
    source: { kind: "cargo-git", url: "https://github.com/someone/shep-example" },
  };
}

test("the real index passes its own validator", () => {
  assert.ok(dogs.length >= 1, "the index should not be empty");
});

test("a duplicate dog name is refused, naming both entries", () => {
  const a = good();
  const b = { ...good(), package: "shep-other" };
  assert.throws(() => validate([a, b]), (err: Error) => {
    assert.match(err.message, /Rex/);
    assert.match(err.message, /shep-example/);
    assert.match(err.message, /shep-other/);
    return true;
  });
});

test("an unknown category is refused and lists the ones that exist", () => {
  assert.throws(() => validate([{ ...good(), category: "logz" }]), (err: Error) => {
    assert.match(err.message, /logz/);
    for (const category of CATEGORIES) assert.match(err.message, new RegExp(category));
    return true;
  });
});

test("a missing required field names the field and the entry", () => {
  const entry = good();
  delete entry.adopt_as;
  assert.throws(() => validate([entry]), (err: Error) => {
    assert.match(err.message, /adopt_as/);
    assert.match(err.message, /shep-example/);
    return true;
  });
});

test("an empty required field is refused like a missing one", () => {
  assert.throws(() => validate([{ ...good(), description: "   " }]), /description/);
});

test("a repo that is not https is refused", () => {
  assert.throws(() => validate([{ ...good(), repo: "http://github.com/a/b" }]), /https/);
});

test("an unknown source kind is refused and lists the three that exist", () => {
  assert.throws(() => validate([{ ...good(), source: { kind: "brew", formula: "x" } }]), (err: Error) => {
    assert.match(err.message, /brew/);
    assert.match(err.message, /cargo-git/);
    assert.match(err.message, /go-install/);
    assert.match(err.message, /manual/);
    return true;
  });
});

test("a manual source with no instructions is refused", () => {
  assert.throws(() => validate([{ ...good(), source: { kind: "manual", instructions: "" } }]), /instructions/);
});

test("a cargo-git source with no url is refused", () => {
  assert.throws(() => validate([{ ...good(), source: { kind: "cargo-git" } }]), /url/);
});

test("the top level must be an array", () => {
  assert.throws(() => validate({ dogs: [good()] }), /array/);
});

test("no entry carries an em dash or an en dash", () => {
  for (const dog of dogs) {
    assert.doesNotMatch(dog.description, /—|–/, `${dog.package}'s description`);
  }
});
```

- [ ] **Step 3: Run them to watch them fail**

```bash
cd /Users/rin/GitHub/pm2-rs/web && node --test scripts/verify-dogs-index.ts
```
Expected: FAIL, `../src/data/dogs.ts` does not exist.

- [ ] **Step 4: Implement `web/src/data/dogs.ts`**

Shape it as: the types and `CATEGORIES` first, then `validate`, then `export const dogs = validate(raw)` where `raw` is the imported JSON. Validate entry by entry, and put the entry's `package` (or its index, when `package` is itself missing) into every message, because a validator that fails without saying which entry is wrong is barely better than none.

The module needs a file-header comment saying what it is and why the JSON lives in `public/`: one file with two consumers, the page that imports it and the URL the site serves.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cd /Users/rin/GitHub/pm2-rs/web && node --test scripts/verify-dogs-index.ts
```
Expected: PASS, 11 tests.

- [ ] **Step 6: Wire it into the build and raise the Node floor**

In `web/package.json`, add the verify step to `build` alongside the pagefind one, and raise `engines.node` from `>=22.12.0` to `>=22.18.0`:

```json
"engines": { "node": ">=22.18.0" },
"scripts": {
  "build": "node --test scripts/verify-dogs-index.ts && astro build && pagefind --site dist && node scripts/verify-pagefind-index.mjs"
}
```

The floor moves because Node runs `.ts` by stripping types, and that is only default-on from 22.18. Below it the verify script needs a flag, so the declared floor would be a lie. Put that reason in a comment where package.json allows one, or in the task's commit message if it does not.

The verify step runs **first**, before `astro build`, so a bad entry fails in a second rather than after a full site build.

- [ ] **Step 7: Write `web/public/dogs.schema.json`**

A JSON Schema (draft 2020-12) matching the validator exactly: the eight required fields, the category enum, and `source` as a `oneOf` over the three kinds with their own required fields. It goes in `public/` so it is served at `/dogs.schema.json` and a contributor can point an editor at it. Add `"$schema"` to nothing; the data file is an array and cannot carry one, so the README instruction is to configure the editor by path.

**This schema is a second copy of a rule.** The validator is the gate and the schema is the courtesy. Add a test asserting they agree on the required-field list and the category enum, so the copy cannot drift silently:

```ts
test("the published schema and the validator agree", async () => {
  const schema = JSON.parse(await readFile(new URL("../public/dogs.schema.json", import.meta.url), "utf8"));
  assert.deepEqual(schema.items.properties.category.enum, [...CATEGORIES]);
  assert.deepEqual(schema.items.required.sort(), REQUIRED_FIELDS.toSorted());
});
```

Export `REQUIRED_FIELDS` from `dogs.ts` for this.

- [ ] **Step 8: Verify the whole build**

```bash
cd /Users/rin/GitHub/pm2-rs/web && npm run build
```
Expected: exit 0, and `dist/dogs.json` plus `dist/dogs.schema.json` both present.

- [ ] **Step 9: Prove the gate has teeth**

Temporarily break `web/public/dogs.json` (change `"category": "logs"` to `"category": "logz"`), run `npm run build`, confirm it fails naming `logz`, then restore. **Report which mutation you ran and what it printed.** A validator nobody watched fail is not evidence.

- [ ] **Step 10: Commit**

```bash
git add web/public/dogs.json web/public/dogs.schema.json web/src/data/dogs.ts web/scripts/verify-dogs-index.ts web/package.json
git commit -m "feat(web): a validated community dog index"
```

---

### Task 2: The page, the navigation, and the way in

**Files:**
- Create: `web/src/pages/docs/community-dogs.astro`
- Modify: `web/src/data/docsNav.ts`, `web/src/pages/docs/dogs.astro`

**Interfaces:**
- Consumes: `dogs`, `CATEGORIES`, `Dog`, `DogCategory` from `../../data/dogs.ts` (Task 1).
- Produces: the page at `/docs/community-dogs`.

- [ ] **Step 1: Add the nav entry**

In `web/src/data/docsNav.ts`, immediately after the `dogs` entry in the same group:

```ts
{
  slug: "community-dogs",
  label: "Community dogs",
  built: true,
  source: "docs/dogs.md",
},
```

`source` reuses `docs/dogs.md` because the page has no doc of its own and `ReferencePills` requires at least a Source pill.

- [ ] **Step 2: Write the page**

`web/src/pages/docs/community-dogs.astro`, following the structure every other docs page uses (`DocsLayout`, `ReferencePills`, `Callout`, `CodeBlock`). Read `dogs.astro` for the house pattern before writing.

Order, top to bottom:

1. A lede saying what the list is.
2. **The disclaimer, in a `Callout`, before any entry.** Listed means a real repo, an open licence, and that it is actually a dog. Not reviewed, not audited, not endorsed. Adopting one runs it at the shepherd's trust level, which is the same thing `/docs/dogs` says, repeated here because this is where somebody is about to act on it.
3. One section per category, **in `CATEGORIES` order, omitting any category with no entries** rather than rendering an empty heading.
4. Each entry: `package (Name)` as the heading, the description, the licence, a link to the repo, and two command blocks.
5. An "Add a dog" section at the end: edit `web/public/dogs.json`, open a pull request, and a link to `/dogs.schema.json`.

- [ ] **Step 3: Render the two command lines per source kind**

| `kind` | install line | adopt line |
|---|---|---|
| `cargo-git` | `cargo install --git <url>` | `shep adopt <adopt_as> ~/.cargo/bin/<package>` |
| `go-install` | `go install <module>@latest` | `shep adopt <adopt_as> $(go env GOPATH)/bin/<package>` |
| `manual` | the `instructions` rendered as **prose, not a command block** | `shep adopt <adopt_as> <path to the binary>` |

Nothing that is not runnable may look copy-pasteable, which is why `manual` gets prose.

**The install path is an assumption and the page must say so**, in one sentence near the first entry: a crate's binary target need not share its package name, and `CARGO_INSTALL_ROOT` or `GOBIN` move the destination, so check where the installer actually put the file. The saving grace is worth stating too: `shep adopt` refuses a path that is not there, so a wrong guess is loud. Contrast that with the adopt NAME being wrong, which is silent, and which is the entire reason `adopt_as` exists.

- [ ] **Step 4: Link it from the dogs page**

In `web/src/pages/docs/dogs.astro`, at the end of the "Writing your own" section, one sentence pointing at `/docs/community-dogs` for what other people have built. Do not restructure that page; it is 439 lines and this is a cross-reference, not a refactor.

- [ ] **Step 5: Humanize the page copy**

Run the `humanizer` skill over every sentence you wrote in Steps 2 to 4 before committing. The existing docs pages are the voice sample to match, not a blank page. Check the bytes for U+2014 and U+2013 afterwards.

- [ ] **Step 6: Build and look at it**

```bash
cd /Users/rin/GitHub/pm2-rs/web && npm run build
```
Expected: exit 0, 21 pages (20 today plus this one).

Then read the rendered output to confirm the entry actually rendered, rather than trusting the build:

```bash
grep -o "shep-log-rotate ([A-Za-z]*)" /Users/rin/GitHub/pm2-rs/web/dist/docs/community-dogs/index.html
```
Expected: `shep-log-rotate (Spot)`.

- [ ] **Step 7: Prove the empty-category rule**

Confirm the page shows only the `logs` section, since the index has one entry. Five empty headings would mean Step 2's omission rule was not implemented. Say what you saw.

- [ ] **Step 8: Commit**

```bash
git add web/src/pages/docs/community-dogs.astro web/src/data/docsNav.ts web/src/pages/docs/dogs.astro
git commit -m "feat(web): the community dogs page"
```

---

## Final verification

```bash
cd /Users/rin/GitHub/pm2-rs/web && npm run build
```
```bash
cd /Users/rin/GitHub/pm2-rs/web && npx astro check
```

Both as their own command with `$?` read directly, never through a pipe: in zsh a pipeline's `$?` is the last command's.

No Rust gate applies. Nothing in this plan touches a crate, and `cargo test` does not read `web/`, which is exactly why the docs trigger exists.

**One thing to confirm by eye before calling it done:** load `/docs/community-dogs` and check the disclaimer is above the fold and reads as a statement rather than boilerplate. The entire trust model of this page is one paragraph, and if it reads as noise it is not doing its job.
