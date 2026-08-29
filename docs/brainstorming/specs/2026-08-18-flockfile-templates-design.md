# Flockfile templates — design

**Date:** 2026-08-18
**Status:** approved 2026-08-19; expanded the same day (see §8)
**Scope:** `shep` (the CLI crate) and one new verb. No wire change, no daemon
change.

## The ask

The maintainer, 2026-08-18: "we should have some subcommands for generating various
flock file templates / adding onto existing ones".

Two jobs, then: scaffold a Flockfile that does not exist yet, and add an app
to one that does.

## What already exists

Established from the code, not assumed. Full citations in
[the research notes](../research/2026-08-18-flockfile-templates-research.md).

- **A Flockfile is `{$schema?, app: [AppConfig...]}`.** Only `name` and
  `script` are genuinely required, and that is enforced by `normalize()`
  rather than by serde — so a document can deserialize and still be rejected.
- **Four parse formats**: TOML, YAML, JSON, JSON5. `.js` is deliberately
  excluded from discovery and from extension sniffing, reachable only through
  `--flockfile` and a node bridge.
- **Discovery is single-directory**, ten fixed filenames, TOML first.
- **`shep_toml.rs` is the write-conventions precedent**: a lock, an atomic
  stage-and-rename, and a refusal that does not clobber a file it could not
  parse. It is TOML-only and has no array-append pattern. Its git history
  carries two lessons this feature inherits directly, both from earlier today:
  a `.expect()` that panicked on a shape an operator can hand-write, and a
  refused write that still rewrote the file and changed its inode.
- **`shep import`'s `render.rs` is the closest existing generator**: TOML
  only, whole-file refuse-or-`--force`, no merge, and round-trip tested
  against the real parser.
- **Verbs are flat.** There is no nested subcommand anywhere in the CLI.
- **Nothing is specced or deferred** for this. Confirmed absent from
  `deferred.md`, `shep-v1.md`, `map.md` and `goals.md`.

## The trap this feature must avoid

`shep start <script>` with no Flockfile **canonicalizes the script to an
absolute path and sets `cwd` to the caller's directory**. That is right for
an ad-hoc start: it is what makes a relative path work from wherever you
typed it.

It is wrong for a generated file. A Flockfile is a thing you commit. Reusing
that code path would emit `script = "~/GitHub/zeus/server.js"` and
`cwd = "~/GitHub/zeus"` into a file that then only works on one
machine, and would differ from every hand-written Flockfile's
relative-script, no-`cwd` convention.

**The generator emits relative paths and omits `cwd`.** It resolves a script
only far enough to check the file exists and to relativize it against the
Flockfile's own directory.

## 1. One verb, flat: `shep init`

```
shep init                       scaffold a Flockfile here
shep init --template node       ... from a named template
shep init <script>              ... with one app already in it
shep init <script> --name api   ... and name it
```

When a Flockfile already exists in the directory, `shep init <script>`
**appends** rather than refusing. That is the "adding onto existing ones"
half, and it needs no second verb: the file's existence is the only thing
that distinguishes the two cases, and a user who runs `shep init` twice means
the second one as an addition.

`init` rather than something sheep-flavoured. `docs/terminology.md` keeps
straight verbs first-class, `init` is what a person types without reading
docs, and pm2 has `pm2 init`, so the muscle memory is already there. The
theme has plenty of room elsewhere; the verb that runs before you have a
flock is not the place to be cute.

### What it refuses

- **A Flockfile that exists and cannot be parsed.** Refuse, name the parse
  error, change nothing. Never rewrite a file whose contents we did not
  understand — that is exactly `shep_toml.rs`'s hard-won rule.
- **An app whose name is already in the file.** `normalize_all` already
  rejects duplicates; catching it here gives a better message than letting
  the daemon do it later.
- **`shep init` with no script, when the file already exists.** Nothing to
  add and nothing to create. Say so rather than silently succeeding.

`--force` replaces a whole existing Flockfile. It is the only destructive
path, so per `docs/terminology.md` its message stays plain and it says what
it is about to destroy.

## 2. Templates

`--template <kind>`, defaulting to `minimal`.

| kind | what it emits |
|---|---|
| `minimal` | `name` and `script`, nothing else |
| `node` | a Node service: script, `instances`, `watch` off, a restart budget |
| `python` | the same shape, with the interpreter question handled |
| `binary` | a compiled executable, no interpreter |
| `static` | `shep serve` in front of a docroot |
| `cron` | a scheduled job, `autorestart` off |

Every template is a real `AppConfig`, and **every emitted field carries a
comment saying what it does**. A scaffolded file is the most-read
documentation this project has, because it is read at the moment someone is
deciding whether to keep using shep.

**`python` and `node` depend on the interpreter decision** already recorded
as task #47 (declare a mapping once, shep applies it, never guesses). These
templates are the natural home for that mapping's first appearance, and the
two features should land in either order but be designed together. If #47
ships first, the templates emit the mapping; if this ships first, they emit
an explicit interpreter per app and #47 generalizes it.

## 3. Format

**Emit TOML.** Discovery is TOML-first, `shep_toml.rs` and `render.rs` are
both TOML-only, and — decisively — there is no comment-preserving YAML or
JSON5 editor anywhere in the tree, and YAML serialization is not even
feature-enabled.

`--format` may select JSON or JSON5 **for creation only**. Appending to a
non-TOML Flockfile is refused with a message that says why, rather than
supported by round-tripping through serde and silently destroying the
operator's comments and key order. That refusal is honest; a lossy append
would not be.

`.js` Flockfiles are out of scope in both directions. They are a code path,
not a document, and nothing in the tree discusses generating one.

Emit a `$schema` line pointing at the checked-in schema, so an editor gives
completion on the file it just wrote.

## 4. Testing

- **Round-trip every template through the real parser.** `render.rs` already
  does this and it is the right pattern: every template must parse AND
  normalize cleanly, so a template can never ship in a shape the daemon would
  reject. This is the test that matters most.
- **Append preserves comments, key order, and every unrelated table**, tested
  against a deliberately hostile Flockfile — the same test shape that caught
  problems in `shep style`'s writer earlier today.
- **A refused write leaves the file untouched, by inode and mode**, not
  merely by content. Content equality is exactly what hid that bug the first
  time.
- **Generated paths are relative**, asserted directly, because the trap above
  is silent and only shows up on a second machine.
- **The two verb-surface invariants**: the new verb appears in exactly one
  help group, and the top-level `--help` still carries no em dash, no en dash
  and no intra-doc-link syntax.
- **A generated Flockfile actually starts.** One e2e test: `shep init`, then
  `shep start --flockfile`, then assert the sheep comes up. A template that
  parses but does not run is worse than no template.

## 5. Assumptions

Recorded because they are judgement calls made on the maintainer's behalf while she was
away, not requirements she stated:

1. **One verb, not two.** The file's existence distinguishes create from
   append, so a second verb would carry no information.
2. **`init` rather than a sheep name.** Straight verbs stay first-class, and
   this one runs before anyone has learned the vocabulary.
3. **TOML only for append.** Driven by what exists: no comment-preserving
   editor for the other formats. A lossy append is worse than a refusal.
4. **Six templates.** Chosen to cover what shep can supervise rather than to
   be exhaustive. `static` and `cron` earn their place by exercising features
   a new user would not otherwise discover.
5. **Comments in emitted files.** Costs bytes, buys the only documentation
   read at the deciding moment.
6. **`--force` only replaces whole files.** There is no forced append,
   because the failure modes of append are refusals that mean something.

## 6. Answered by the maintainer, 2026-08-19

1. **Bare `shep init` emits a skeleton, and a fuller one than proposed.** Not
   an empty `app = []`: a Flockfile carrying a commented-out `[[app]]` AND a
   commented-out `[dog.<name>]`, each showing its full set of options.

   **This makes the scaffolded file the reference documentation**, which is
   the point, and creates the one real risk in this feature: a hand-written
   full-options block drifts from the parser the first time a field is added
   or renamed, and a stale reference is worse than none because people trust
   it. Mitigation, which the plan must carry: the skeleton is checked against
   `crates/shep-core/assets/flockfile.schema.json` — itself generated from
   the parser's own document type via schemars — by a test asserting that
   every key the skeleton mentions exists in the schema, and that every
   documented option in the schema appears in the skeleton. Adding a field to
   the parser then fails that test until the skeleton catches up.

2. **`shep init` also offers to write the interpreter mapping.** Paired with
   the answer to the first point of §7 below: both halves of the fresh-install
   problem get solved in the same place rather than each solving half.

3. **`static` stays in scope.**

## 7. The interpreter decisions this depends on (task #47)

Answered at the same time, recorded here because the `node` and `python`
templates cannot be written without them:

1. **First run writes a starter mapping.** The mapping is opt-in, so without
   this a fresh install still cannot run the `shep start server.js` that
   `welcome.rs` and `--help` advertise in three places. shep is scaffolding
   `~/.shep/shep.toml` anyway; the mapping is visible and editable the moment
   it exists, which keeps it honest rather than magic.
2. **A per-invocation `--interpreter` override exists**, for one-offs.
3. **Precedence is `shep.toml` then Flockfile then flag, last wins.** The
   same `file < env < flags` layering `DaemonConfig` already uses, with the
   Flockfile between them because it is the more specific statement about a
   particular flock.
4. **the maintainer's own services did not depend on any of this.** `zeus-auth` is a
   compiled binary started as `shep start ./target/release/zeus-auth` from
   its own checkout, so the `spawn_failed` she reported on 2026-08-18 was
   entirely the relative-path resolution bug fixed in `6cf7124`. The
   interpreter gap is real and unrelated to it.


## 8. Expansion, decided by the maintainer 2026-08-19 while writing lesson 1

Reviewing her own first skeleton against the spec, the maintainer named scenarios the
one-axis design could not express:

> "Helping a user get comfortable with setting up shep/flockfiles. A quick way
> to create a new Flockfile. A convenient way to add a new app to an existing
> flockfile. Adding a new dog to an existing flockfile. Maybe users would want
> to augment their existing app entries on an existing flockfile with all of
> the options available."
>
> "We have quite a lot of options and sometimes all of them would be desirable
> and sometimes not. We wouldn't want to overwhelm a brand new user who hasn't
> used shep or pm2 with everything but a veteran user might want to add them
> all because they aren't 100% confident of the field names."

That identifies a **second axis the original design collapsed into the first**.
Verbosity is a property of the moment, not of the template: a newcomer and a
veteran want the SAME template at different depths. Forcing it into
`--template` yields `node` and `node-full` and `python` and `python-full`,
which is the combinatorial smell.

### The three axes

| axis | values |
|---|---|
| **target** | new file, add an app, add a dog, expand an existing entry |
| **depth** | curated default, or `--all` |
| **kind** | `--template`: minimal, node, python, binary, static, cron |

### Depth: two levels, default and `--all`

**A consequence that cuts against intuition and must not be forgotten: only
`--all` is machine-checkable.** The anti-drift test in §6 compares the
skeleton against the schemars-generated schema, which works precisely because
`--all` is meant to contain everything. The curated default is a human
judgement about what matters on day one; no test can tell anyone it has gone
stale. So the fuller level is the CHEAPER one to maintain and the friendly one
carries the ongoing cost. Reviewers of the default level are reviewing
editorial judgement, not correctness.

### Kind: all six stay

Offered a cut to three on the grounds that the interpreter mapping (task #47)
had erased the difference between `node`, `python` and `binary`, the maintainer kept all
six: `shep init --template node` is discoverable and reassuring in a way
`--template service` is not, and the redundancy buys familiarity.

They must then differ in SOMETHING or the help text lies. What legitimately
differs is the **example script path** each one shows -- `./index.js`,
`./main.py`, `./target/release/app` -- which is the teaching content, and the
one or two fields that shape actually implies. `static` and `cron` remain
structurally distinct as before.

Test matrix: 6 templates x 2 depths = 12 outputs, each round-tripped through
the real parser. Cheap, and the `--all` half is additionally schema-checked.

### Augment: in scope

`shep init --all` pointed at an app that already exists rewrites that entry to
carry a commented-out line for every option it has not set, preserving the
operator's own values, comments and key order.

This is the hardest piece in the feature and the only one that REWRITES rather
than appends, so `shep_toml.rs`'s conventions bind hardest here: the lock, the
atomic stage-and-rename, the refusal that does not clobber a file it could not
parse, and both hard-won lessons -- no `.expect()` on a shape an operator can
hand-write, and a refused write must not rewrite the file (assert on inode and
mode, not on bytes).

It is also the scenario that most directly serves the veteran who cannot
remember a field name, applied to the file they already have.
