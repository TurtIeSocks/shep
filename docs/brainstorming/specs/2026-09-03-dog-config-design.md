# Design: dog config, and making it editable

Status: designed 2026-09-03, not yet implemented.

This moves a dog's settings out of `shep.toml`, gives a dog a way to describe
its own config, and gives lookout enough to render an editor for it. The store
move is breaking for operators and is deliberately sequenced first, before
anything that depends on it.

## The problem

A dog's settings live under `[dog.<name>]` in `shep.toml` and shep has no idea
what any of it means. `DaemonConfig` carries `pub dog: BTreeMap<String,
toml::Table>`, parsed only because `RawDaemonConfig` denies unknown top-level
keys and an unregistered section would otherwise be a hard boot error. One read
site, `dog_section`, re-reads the file per request and passes the table through
as TOML text.

So shep sees the keys an operator wrote and their values, and nothing else. Not
the fields a dog defaults internally, not their types, not whether a key is
even valid for that dog, and not whether a value is a bearer credential.
`[dog.bark.sinks]` routinely holds a webhook URL, which is why the whole
section is treated as sensitive rather than any part of it.

That is the reason decision 11 of the overrides spec excluded `[dog.<name>]`
from lookout: "a third-party dog's opaque schema". The exclusion was correct
and this spec removes the thing that made it necessary.

Two smaller consequences fall out of the same gap. An operator editing
`[dog.metrics]` gets no hint of what else could be set, because a dog that
defaults everything has an empty section. And a change never reaches a running
dog, so `docs/dogs.md` documents `shep disable <name> && shep enable <name>` as
the way to apply one.

## What already exists

More than expected, which is why little of this is breaking.

`shep adopt` already spawns a dog binary, asks it a question, and tolerates
silence. It runs the binary with `--version`, reads two lines, and a dog that
does not answer is adopted with its protocol unknown rather than refused. The
pattern for asking a stranger's binary something is settled.

The bus already pushes. `Request::Subscribe` takes topic globs and `shep
bleats` uses it today. No dog subscribes yet, but nothing in the wire has to
change for one to start.

A dog can already restart itself. `hello.dog_name` is bookkeeping (it records
who named themselves, and narrates the reconnect transition) and gates no
requests, so a dog connection can issue `Request::Restart` exactly like the CLI
can.

`ShepToml` already does careful writes to `shep.toml`: `toml_edit` so comments
and key order survive, an advisory lock on a sibling file, a staged temp file,
`fsync`, atomic rename. `shep style`, `shep enable` and `shep adopt` all go
through it, so shep writing an operator's config file is established practice
rather than a new liberty.

schemars is already a workspace dependency at 1.2.2, and field-level
`#[schemars(extend(...))]` works in that version.

## Decisions

### 1. Dog config leaves `shep.toml` for its own file

`$SHEP_HOME/dogs.toml`, holding what `[dog.*]` holds today with the `dog.`
prefix dropped: `[metrics]`, `[bark.sinks]`.

Decision 11's objection was never to dog config being in a file. It was to
lookout writing into free-form maps inside the daemon's own hand-written
config, which is the same reason decision 8 of that spec put shared env in its
own store. A separate file answers the objection without taking anything away.

Hand-editable, and deliberately not a locked shep-owned store like
`overrides.json`. A dog's config is authored intent, not derived state, and an
operator on a box with only a shell has to be able to set `bind =
"0.0.0.0:9615"` without waiting for a dashboard. shep tolerates a human having
edited it badly exactly as it does for `shep.toml`.

### 2. Migrate on first boot, then strike the section

`RawDaemonConfig` keeps its `dog` field, so an old `shep.toml` never fails to
parse. This is not optional politeness: delete that field and every existing
file with a `[dog.bark]` section stops parsing, the daemon refuses to boot, and
the flock goes unsupervised at upgrade time.

On boot, any `[dog.*]` section present is moved into `dogs.toml` and struck
from `shep.toml` in one `ShepToml::edit`, and shep reports what moved. One
transition, one source of truth after it, no window where both files hold a
value for the same key.

After that boot the `dog` field is a recognized key that is always empty.
Removing it from the struct is a later breaking change with its own
deprecation window, not part of this.

### 3. The dog-facing wire does not change

`Response::DogSection` keeps carrying the section as TOML text in
`DogSectionToml`, and `dog_section` just reads a different file. Every dog ever
written keeps working, so the breakage stays operator-side: one file, migrated
automatically.

The newtype's manual `Debug` and its exact-string test survive untouched, which
matters more than the format seam it creates against a schema that is JSON.

### 4. A dog publishes its schema, and answering is optional

`--schema` on the binary, printing JSON Schema on stdout. Same shape as the
`--version` probe adopt already runs: spawn, read, kill.

Asked of the binary rather than over the socket because the dog most in need of
configuring is the one that is disabled or has never started, and it has no
connection to answer on. Configure then enable is the sequence an operator
wants.

A dog that answers nothing is recorded as having no schema and is refused
nothing, matching `--version`. A dog that answers invalid JSON gets one warning
at adopt and is otherwise treated as silence, because a dog with a broken
`--schema` may still scrape perfectly well.

This is a new expectation on dog authors, which is why it belongs in the dog
contract now rather than after more dogs exist.

### 5. shep owns the probe, and the strings both sides parse

One entry point on the dog side, handling every probe rather than one function
per flag:

```rust
fn main() {
    shep_client::dogs::probe::<MyDogConfig>();  // answers --version and --schema
    // ...normal startup
}
```

`--version`'s answer format is parsed by shep and is currently hand-typed in
every dog from a snippet in the docs. A typo there reads as "protocol unknown"
and nothing says so. `--schema` would create the same bug on day one, so both
move behind one call that shep can extend later without every dog needing an
edit.

Three homes, and no crate depends on another's half:

| what | where |
| --- | --- |
| flag names, the `shep-protocol:` line's grammar, the secret marker key | `shep-core` |
| `dogs::probe`, the dog side | `shep-client`, which already re-exports `shep_core` |
| spawning the binary and reading the answer | `shep-cli`, beside the vetting adopt already does |

The asker stays in `shep-cli` deliberately. It is the code that executes an
untrusted third-party binary, and it belongs next to the rules that go with
that (execute bit, world-writable refusal, canonicalized path, spawn and kill).
Putting it in `shep-client` would make every dog compile it and would split
adopt's vetting across two crates.

### 6. The secret marker is a derive macro, not a documented extension

```rust
#[derive(Deserialize, DogConfig)]
struct Sink {
    kind: SinkKind,
    #[shep(secret)]
    url: String,
}
```

schemars can express this without shep shipping anything:
`#[schemars(extend("x-shep-secret" = true))]` works at field level in 1.2. On
ergonomics that is twenty-three characters and not worth a proc-macro crate.

The reason to ship one anyway is that `x-shep-secret` is a string shep parses,
hand-typed by the author. Transpose two of its letters and it compiles, the
schema validates, the field is not marked, and lookout paints a webhook
credential on screen. Nothing fails and nothing warns. It cannot be linted either, because
schemars takes a string literal for the extension key, so a shep-exported const
cannot go in that position.

That is the same bug class decision 5 removes for `--version`, and the marker
is the one field where getting it wrong has a security consequence rather than
a cosmetic one.

### 7. The schema is asked fresh, never stored

`docs/dogs.md` already refuses to store a dog's protocol version, on the
grounds that `cargo install` replaces a binary with nothing watching, so a
stored copy "would be wrong exactly when it mattered". A schema is the same
claim about the same file and gets the same rule.

A stale schema is worse than a stale version number, because it mislabels which
field is a credential. The cost is a process spawn when a pane opens, which is
a keystroke rather than a loop.

### 8. A change reaches a running dog over the bus, and the dog decides

shep publishes on `config.dog.<name>` when a dog's config changes. A dog that
subscribed re-asks with `Request::DogConfig` and does whatever suits it: swap
values in place, rebind a listener, or send `Request::Restart` on itself.

shep says that it changed and nothing about what it means. Putting the live
versus needs-restart axis in the schema would have made lookout able to show
dogs the same pending marker sheep get, at the cost of shep knowing what a
third-party dog's fields mean, which every other decision here refuses.

The two built-in dogs need no contract at all, since shep owns their source.
They are not the same case: bark's config is sinks and rules, pure data with no
OS resource attached, swappable in place. `[dog.metrics]` has exactly one key,
`bind`, which is a listening socket, so metrics has real work to do on a change
even though its process never has to exit.

Published on writes shep made, and on a rescan at `shep daemon reload`. Not on
a file watcher: a hand edit takes effect the way an edit to `shep.toml` does
today, on a path a human deliberately triggered.

A dog that restarts itself on a config change must say so in its own log. The
restart count cannot tell that apart from a crash loop, and that column is what
an operator reads as instability.

### 9. No schema means no pane

Lookout renders from the schema: field list, types, defaults, and descriptions,
which schemars takes from doc comments. A field marked `#[shep(secret)]`
renders as `<set>` and can be replaced, never read back, which is decision 12
of the overrides spec applied per field instead of per section. Behind
`--allow-control`, per decision 11. Writes go through `toml_edit`, so a
hand-written `dogs.toml` keeps its comments.

A dog with no schema gets no pane and needs no fallback, because decision 1
already provides one. A raw TOML buffer inside a TUI would be worse than
`$EDITOR`, and it would show bark's webhook URL in the clear for every dog that
has not adopted the contract yet, bark included until it does.

## Wire

`Request` and `Response` are unchanged. `Subscribe` and `DogConfig` already
exist and `config.dog.<name>` is a topic, not a variant. `PROTOCOL_VERSION`
stays at 2 and `SCHEMA_VERSION` stays at 1.

The new contract is entirely outside the socket: two flags on a binary and what
they print.

## Surfacing

`shep describe <dog>` gains the schema's field list where one exists, so the
information is reachable without lookout. `shep flock` gains nothing: it is for
critical state, and a dog's config is not that.

## Rollout

Three releases, because the breaking half is independent of everything else and
should ship small.

1. **The store move.** Decisions 1, 2, 3. No new features, nothing for a dog
   author to read. The change that touches every operator's file ships alone
   and is easy to revert.
2. **The contract.** Decisions 4, 5, 6, 7, 8, plus schemas on both built-in
   dogs and bark subscribing to its own topic.
3. **The pane.** Decision 9.

## Out of scope

- **Encrypting `dogs.toml`.** It holds webhook credentials and is plaintext,
  exactly as `shep.toml` is today. Spec 2 of the overrides work owns the
  encrypted store and this file should join it there rather than grow its own
  scheme.
- **A live versus needs-restart axis in the schema.** Argued at decision 8.
  Additive later if a dog author asks for it.
- **Removing `dog` from `RawDaemonConfig`.** Its own breaking change, after a
  deprecation window.
- **Validating a dog's config against its own schema.** shep has the schema and
  the values and could reject a bad edit, but the dog is the authority on its
  own config and a shep that disagreed would be wrong in the direction that
  breaks a working dog.

## Testing

- A `shep.toml` carrying `[dog.*]` parses on the new binary, migrates on boot,
  and the file afterwards has the section gone and every other section, comment
  and key order intact. Exact string.
- A `shep.toml` with no dog sections is not rewritten at all.
- A second boot after a migration writes nothing.
- `dog_section` serves byte-identical TOML from `dogs.toml` as it did from
  `shep.toml`, so the dog-facing contract is pinned rather than assumed.
- `probe` answers both flags and returns for anything else, including no
  arguments at all.
- A dog binary that exits non-zero, prints nothing, or prints invalid JSON on
  `--schema` is recorded as having no schema and refused nothing. One test per
  shape.
- A schema with a `#[shep(secret)]` field round-trips the marker, and a pane
  built from it renders `<set>` rather than the value. Exact string, since this
  is the assertion standing between a webhook token and a screen.
- Editing a dog's config publishes on `config.dog.<name>` exactly once.
- `shep daemon reload` rescans `dogs.toml` and publishes for a dog whose
  section changed, and not for one whose section did not.

Every await needs a forcing mechanism rather than a sleep, and every new test
gets proved non-vacuous by mutating what it protects and watching that test go
red.

## Docs

`docs/dogs.md` is the dog contract and needs the most of this: the new file,
the migration, `--schema`, `probe`, the secret marker, and the bus topic with
the self-restart logging rule. Its Configuration section currently documents
the disable-and-enable dance and that paragraph is replaced.

`web/src/pages/docs/` needs the store move on any page naming `[dog.<name>]`,
and the CLI reference regenerated if any verb's help text moves.

`CLAUDE.md` records that dog config no longer lives in `shep.toml`.

## Migration

Covered at decision 2. The operator does nothing: they upgrade, the daemon
boots, the sections move, and shep says what it did. An operator who never
opens `shep.toml` again never learns this happened, which is the goal.
