/*
 * The community dog index.
 *
 * `web/public/dogs.json` is the single source. It lives in `public/`, not
 * here in `src/data/`, because it has a second consumer this module cannot
 * satisfy: the published site serves it verbatim at `/dogs.json`, a stable
 * URL `shep dogs --available` reads (`crates/shep-cli/src/dog_index.rs`).
 * Every other data file on this site is a TypeScript module because only
 * the site itself reads it; this one has to be plain JSON so something
 * outside the Astro build can too.
 *
 * The file is an object, `{ "$schema": ..., "version": 1, "dogs": [...] }`,
 * not a bare top-level array of entries -- `unwrapIndex()` below is what
 * peels that open before `validate()` ever sees an entry, and it is a
 * breaking change against `shep` 0.1.0, which read the old bare-array shape
 * and nothing else. `$schema` gives a contributor's editor completion when
 * they add a row; a bare array could never carry that, or `version` beside
 * it. `version` is what lets a future, incompatible reshape of the wrapper
 * announce itself instead of silently parsing wrong -- `dog_index.rs`'s own
 * module doc has the fuller reasoning, shared by both readers of this file.
 *
 * This module is the gate: it validates that JSON at import time and throws
 * on the first bad entry, which fails `astro build` with the thrown message
 * printed. A malformed `dogs.json` cannot ship, in dev or in CI, without
 * anyone adding tooling for it. `web/public/dogs.schema.json` is a second,
 * looser copy of the same rules for a contributor's editor to check as they
 * type; `validate()` here is the one that actually decides.
 */
import raw from "../../public/dogs.json" with { type: "json" };

/**
 * The only wrapper `version` this build's `unwrapIndex()` accepts. Kept in
 * sync with `crates/shep-cli/src/dog_index.rs`'s own
 * `SUPPORTED_INDEX_VERSION` by hand -- `the schema and the wrapper version
 * agree` in `scripts/verify-dogs-index.ts` pins the two together the same
 * way this file's own category and source-kind lists are pinned against
 * that crate.
 */
export const SUPPORTED_INDEX_VERSION = 1;

/** The six categories a dog can be filed under, in the order the page groups them. */
export type DogCategory = "logs" | "metrics" | "alerts" | "health" | "deploy" | "other";

export const CATEGORIES: readonly DogCategory[] = [
  "logs",
  "metrics",
  "alerts",
  "health",
  "deploy",
  "other",
];

/**
 * Installable with `cargo install <package>` from crates.io. Carries no
 * fields of its own: the package name is already the entry's `package`, and
 * a source that could spell it a second way would eventually spell it
 * differently.
 */
export interface CargoSource {
  kind: "cargo";
  /**
   * The exact version to name, for a dog that needs one. cargo resolves `*`
   * by default and `*` never matches a pre-release, so an alpha that omits
   * this ships a command that cannot resolve it. Omit for a normal release.
   */
  version?: string;
}

/** Installable with `cargo install --git <url>`, for a dog not on crates.io. */
export interface CargoGitSource {
  kind: "cargo-git";
  url: string;
}

/** Installable with `go install <module>@latest`. */
export interface GoInstallSource {
  kind: "go-install";
  module: string;
}

/** No one-line installer; `instructions` is rendered as prose, not a command. */
export interface ManualSource {
  kind: "manual";
  instructions: string;
}

/**
 * How a dog is built, tagged by `kind`. Deliberately not a freeform string:
 * "how do I install this" and "what artifact would shep fetch" are two
 * different questions that happen to look like one field, and a tagged kind
 * stays machine-readable if `shep install` ever exists without asking every
 * past contributor to redo their entry.
 */
export type DogSource = CargoSource | CargoGitSource | GoInstallSource | ManualSource;

/** One listing in the community dog index. Every field is required. */
export interface Dog {
  /** The dog's own name, unique (case-insensitively) across the index. Displayed as `package (Name)`. */
  name: string;
  /** The crate or repository name; the real identity. */
  package: string;
  /**
   * The name this dog expects to be adopted under. A dog is given no argv
   * and cannot be told its own adopted name, so `shep adopt <name> <path>`
   * with the wrong `<name>` silently discards its whole `[dog.<name>]`
   * configuration. This field exists so the page's adopt line is correct by
   * construction instead of a guess.
   */
  adopt_as: string;
  /** One line describing what the dog does. */
  description: string;
  /** HTTPS URL of the dog's repository. */
  repo: string;
  /** SPDX license string. */
  license: string;
  category: DogCategory;
  source: DogSource;
}

/** The top-level required fields, exported so a test can check they match `dogs.schema.json`. */
export const REQUIRED_FIELDS: readonly string[] = [
  "name",
  "package",
  "adopt_as",
  "description",
  "repo",
  "license",
  "category",
  "source",
];

const SOURCE_KINDS: readonly DogSource["kind"][] = ["cargo", "cargo-git", "go-install", "manual"];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isDogCategory(value: string): value is DogCategory {
  return (CATEGORIES as readonly string[]).includes(value);
}

function isSourceKind(value: string): value is DogSource["kind"] {
  return (SOURCE_KINDS as readonly string[]).includes(value);
}

/**
 * The `#fragment` an entry is linked by, so a link can read
 * `/docs/community-dogs#bella` rather than naming the crate. Derived from
 * `name` and not stored, because a hand-written id is one more field to get
 * out of step with the name beside it.
 *
 * Slugging is lossy, so `validate` below keys its duplicate-name check on
 * this rather than on the name itself: "Bella" and "Bella!" are two names
 * and one anchor, and the second would silently scroll to the first.
 */
export function anchorOf(dog: Pick<Dog, "name">): string {
  return dog.name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/**
 * A short, stable name for an entry to put in an error message: its
 * `package` field when that is present and non-empty, or its array index
 * when `package` is itself the missing or malformed field. Every refusal in
 * this file routes through this so a contributor is told which of however
 * many entries is wrong, not just that one of them is.
 */
function entryLabel(entry: unknown, index: number): string {
  if (isRecord(entry) && typeof entry.package === "string" && entry.package.trim() !== "") {
    return `"${entry.package}"`;
  }
  return `at index ${index} (no "package" field)`;
}

function fail(label: string, message: string): never {
  throw new Error(`Dog ${label}: ${message}`);
}

function requireNonEmptyString(record: Record<string, unknown>, field: string, label: string): string {
  const value = record[field];
  if (typeof value !== "string" || value.trim() === "") {
    fail(label, `missing or empty "${field}"`);
  }
  return value;
}

/** A repo URL that at least has the shape of a repository, not just a domain or a user's profile page. */
function requireRepoUrl(repo: string, label: string): string {
  let parsed: URL;
  try {
    parsed = new URL(repo);
  } catch {
    fail(label, `"repo" is not a valid URL: "${repo}"`);
  }
  if (parsed.protocol !== "https:") {
    fail(label, `"repo" is not https: "${repo}". Use an https:// URL.`);
  }
  const segments = parsed.pathname.split("/").filter((segment) => segment !== "");
  if (segments.length < 2) {
    fail(
      label,
      `"repo" doesn't look like a repository: "${repo}". Expected an owner and a repo name in the path, not just a domain or a profile page.`,
    );
  }
  return repo;
}

function validateSource(source: unknown, label: string): DogSource {
  if (!isRecord(source)) {
    fail(label, `"source" must be an object`);
  }
  const kind = source.kind;
  if (typeof kind !== "string" || !isSourceKind(kind)) {
    fail(
      label,
      `unknown "source.kind": "${String(kind)}". Valid kinds: ${SOURCE_KINDS.join(", ")}.`,
    );
  }

  switch (kind) {
    case "cargo": {
      // Optional, but not lax: absent means a normal release, while present
      // and unusable is a malformed entry like any other here.
      const version = source.version;
      if (version === undefined) {
        return { kind };
      }
      if (typeof version !== "string" || version.trim() === "") {
        fail(label, `a "cargo" source with a "version" needs it non-empty`);
      }
      return { kind, version };
    }
    case "cargo-git": {
      const url = source.url;
      if (typeof url !== "string" || url.trim() === "") {
        fail(label, `a "cargo-git" source needs a non-empty "url"`);
      }
      return { kind, url };
    }
    case "go-install": {
      const module = source.module;
      if (typeof module !== "string" || module.trim() === "") {
        fail(label, `a "go-install" source needs a non-empty "module"`);
      }
      return { kind, module };
    }
    case "manual": {
      const instructions = source.instructions;
      if (typeof instructions !== "string" || instructions.trim() === "") {
        fail(label, `a "manual" source needs non-empty "instructions"`);
      }
      return { kind, instructions };
    }
  }
}

/**
 * Validates a raw JSON value as a community dog index, throwing an `Error`
 * naming the offending entry and field on the first problem found. Returns
 * the fully-typed entries when every one passes.
 *
 * @throws {Error} on the first entry, field, or index-level problem found.
 */
export function validate(raw: unknown): Dog[] {
  if (!Array.isArray(raw)) {
    throw new Error(
      `dogs.json must be a top-level array of dog entries, not ${isRecord(raw) ? "an object" : typeof raw}.`,
    );
  }

  const result: Dog[] = [];
  const namesSeen = new Map<string, string>();
  const packagesSeen = new Map<string, string>();

  raw.forEach((entry, index) => {
    const label = entryLabel(entry, index);

    if (!isRecord(entry)) {
      fail(label, "must be an object");
    }

    for (const field of REQUIRED_FIELDS) {
      if (!(field in entry)) {
        fail(label, `missing required field "${field}"`);
      }
    }

    const name = requireNonEmptyString(entry, "name", label);
    const packageName = requireNonEmptyString(entry, "package", label);
    const adoptAs = requireNonEmptyString(entry, "adopt_as", label);
    const description = requireNonEmptyString(entry, "description", label);
    const repo = requireNonEmptyString(entry, "repo", label);
    const license = requireNonEmptyString(entry, "license", label);
    const category = requireNonEmptyString(entry, "category", label);

    requireRepoUrl(repo, label);

    if (!isDogCategory(category)) {
      fail(
        label,
        `unknown "category": "${category}". Valid categories: ${CATEGORIES.join(", ")}.`,
      );
    }

    const source = validateSource(entry.source, label);

    const nameKey = anchorOf({ name });
    if (nameKey === "") {
      fail(label, `"name" "${name}" has no letters or digits, so it has no anchor to link it by`);
    }
    const priorNameLabel = namesSeen.get(nameKey);
    if (priorNameLabel !== undefined) {
      fail(label, `duplicate "name" "${name}" (already used by ${priorNameLabel})`);
    }
    namesSeen.set(nameKey, label);

    const priorPackageLabel = packagesSeen.get(packageName);
    if (priorPackageLabel !== undefined) {
      fail(label, `duplicate "package" "${packageName}" (already used by ${priorPackageLabel})`);
    }
    packagesSeen.set(packageName, label);

    result.push({
      name,
      package: packageName,
      adopt_as: adoptAs,
      description,
      repo,
      license,
      category,
      source,
    });
  });

  return result;
}

/**
 * Peels the `{ "$schema": ..., "version": 1, "dogs": [...] }` wrapper open
 * and hands `validate()` the bare array it has always taken. Kept separate
 * from `validate()` itself so every existing entry-level test keeps calling
 * `validate()` with a plain array, unaffected by the wrapper -- the entries
 * inside `dogs` did not change, so the tests that check them should not
 * have to.
 *
 * @throws {Error} if `raw` is not an object, names an unsupported
 * `version`, or has no `dogs` array.
 */
export function unwrapIndex(raw: unknown): unknown[] {
  if (!isRecord(raw)) {
    throw new Error(
      `dogs.json must be a top-level object with a "dogs" array, not ${Array.isArray(raw) ? "an array" : typeof raw}. ` +
        `shep 0.1.0's own index was a bare array; that shape is no longer accepted.`,
    );
  }
  if (raw.version !== SUPPORTED_INDEX_VERSION) {
    throw new Error(
      `dogs.json is version ${JSON.stringify(raw.version) ?? "unspecified"}, but this build only understands version ${SUPPORTED_INDEX_VERSION}.`,
    );
  }
  if (!Array.isArray(raw.dogs)) {
    throw new Error(`dogs.json's "dogs" field must be an array, not ${typeof raw.dogs}.`);
  }
  return raw.dogs;
}

/** The validated real index. Throws at import time if `dogs.json` is bad. */
export const dogs: Dog[] = validate(unwrapIndex(raw));
