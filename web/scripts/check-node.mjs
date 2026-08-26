// Refuses a Node too old for the rest of the build, by name.
//
// Plain `.mjs` on purpose: it has to run on the very versions it exists to
// reject, and a `.ts` preflight cannot, which is the whole failure it guards
// against. `verify-dogs-index.ts` needs Node to strip types, which is default
// only from 22.18, and on 22.12 the build died with
// `ERR_UNKNOWN_FILE_EXTENSION: Unknown file extension ".ts"` and a stack trace
// into node:internal. Nothing in that names the cause.
//
// It also keeps `.nvmrc` and `engines` honest about each other. They
// disagreed once: `engines` said >=22.18.0 while `.nvmrc` pinned 22.12.0, and
// since CI resolves its Node from `.nvmrc`, the declared floor was a claim
// nothing enforced. Whichever a reader trusts, the other has to agree.
import { readFileSync } from "node:fs";

const root = new URL("..", import.meta.url);
const parse = (v) => v.trim().replace(/^v/, "").split(".").map(Number);
const lt = (a, b) => {
  for (let i = 0; i < 3; i++) if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) < (b[i] ?? 0);
  return false;
};

const engines = JSON.parse(readFileSync(new URL("package.json", root), "utf8")).engines.node;
const floor = parse(engines.replace(/^[^\d]*/, ""));
const nvmrc = parse(readFileSync(new URL(".nvmrc", root), "utf8"));
const running = parse(process.versions.node);

const fail = (msg) => {
  process.stderr.write(`web build: ${msg}\n`);
  process.exit(1);
};

if (lt(nvmrc, floor)) {
  fail(
    `.nvmrc pins ${nvmrc.join(".")} but package.json engines requires ${engines}. ` +
      `CI resolves its Node from .nvmrc, so the floor engines declares is not the one that runs. ` +
      `Raise .nvmrc.`,
  );
}
if (lt(running, floor)) {
  fail(
    `running Node ${running.join(".")}, but this build needs ${engines}. ` +
      `Below 22.18 Node will not strip types from a .ts file, and scripts/verify-dogs-index.ts ` +
      `fails with ERR_UNKNOWN_FILE_EXTENSION rather than anything that names the reason.`,
  );
}
