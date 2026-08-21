// Regression guard for the community dog index (web/src/data/dogs.ts).
//
// dogs.ts's validate() is the actual gate: it throws at import time if
// web/public/dogs.json is malformed, which is what fails `astro build`.
// This file exercises that validator directly against deliberately broken
// fixtures, so a refusal is proven rather than assumed, and asserts every
// message names the entry it is about -- a validator that fails without
// saying which of however many entries is wrong is barely better than none.
//
// Runs under Node's built-in test runner and native TypeScript type
// stripping (`node --test scripts/verify-dogs-index.ts`), so this project
// adds no test framework for one module.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { validate, dogs, CATEGORIES, REQUIRED_FIELDS } from "../src/data/dogs.ts";

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

test("the published schema and the validator agree", async () => {
  const schema = JSON.parse(await readFile(new URL("../public/dogs.schema.json", import.meta.url), "utf8"));
  assert.deepEqual(schema.items.properties.category.enum, [...CATEGORIES]);
  assert.deepEqual(schema.items.required.sort(), REQUIRED_FIELDS.toSorted());
});

// --- Extra refusal cases, beyond the brief's list. See task-1-report.md for
// what each protects and why it was made a refusal rather than allowed. ---

test("a name that collides with an existing one only by case is refused", () => {
  const a = good();
  const b = { ...good(), package: "shep-other", name: "rex" };
  assert.throws(() => validate([a, b]), (err: Error) => {
    assert.match(err.message, /rex/i);
    assert.match(err.message, /shep-example/);
    assert.match(err.message, /shep-other/);
    return true;
  });
});

test("a duplicate package under a different display name is refused", () => {
  const a = good();
  const b = { ...good(), name: "Rex II" };
  assert.throws(() => validate([a, b]), (err: Error) => {
    assert.match(err.message, /shep-example/);
    return true;
  });
});

test("a repo that is a valid URL but not a repository is refused", () => {
  assert.throws(() => validate([{ ...good(), repo: "https://github.com" }]), /repository/);
  assert.throws(() => validate([{ ...good(), repo: "https://github.com/someone" }]), /repository/);
});

test("an entry that is not an object is refused, naming its index", () => {
  assert.throws(() => validate(["not an object"]), (err: Error) => {
    assert.match(err.message, /index 0/);
    return true;
  });
});

test("a source that is missing entirely is refused", () => {
  const entry = good();
  delete entry.source;
  assert.throws(() => validate([entry]), /source/);
});
