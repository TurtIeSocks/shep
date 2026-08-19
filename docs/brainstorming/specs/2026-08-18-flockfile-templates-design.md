# Flockfile templates — design

**Date:** 2026-08-18
**Status:** draft, awaiting Rin's review — nothing implemented
**Scope:** `shep` (the CLI crate) and one new verb. No wire change, no daemon
change.

## The ask

Rin, 2026-08-18: "we should have some subcommands for generating various
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
that code path would emit `script = "/Users/rin/GitHub/zeus/server.js"` and
`cwd = "/Users/rin/GitHub/zeus"` into a file that then only works on one
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

Recorded because they are judgement calls made on Rin's behalf while she was
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

## 6. Open questions for Rin

1. **Does `shep init` with no arguments scaffold an empty `app = []`, or
   refuse and ask for a script?** An empty Flockfile is valid and gives
   something to edit; refusing is more honest about needing input. Leaning
   toward emitting the commented skeleton, since editing beats remembering.
2. **Should `shep init` offer to write the interpreter mapping** from task
   #47 into `shep.toml` at the same time, or stay strictly a Flockfile verb?
3. **Is `static` in scope?** It scaffolds `shep serve` rather than a user
   process, which is a slightly different thing wearing the same shape.
