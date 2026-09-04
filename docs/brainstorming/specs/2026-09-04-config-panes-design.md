# Design: the sheep and dog config panes

Status: designed 2026-09-04, not implemented.

This builds the half of
[the config overrides design](2026-09-02-config-overrides-design.md)'s
decision 11 that
[the lookout settings screen](2026-09-04-lookout-settings-design.md) left out,
its decision 12, and decision 9 of
[the dog config design](2026-09-03-dog-config-design.md). Three panes end up
sharing one renderer, which is why they are one spec rather than two.

A bare "decision 11" or "decision 12" here means the overrides spec's. A bare
"decision 9" means the dog spec's. This spec's own decisions are always
written "decision 4 below".

## The problem

`shep lookout` can edit `shep.toml` and nothing else. An operator who wants to
change a sheep's restart budget or a dog's webhook edits a file by hand, which
is what decision 11 and decision 9 both exist to remove.

Two panes are missing, and they look similar enough that building them apart
would produce two of everything.

## What already exists

**The settings screen, from #124.** `s` opens it, `--allow-control` gates
editing, and it writes `$SHEP_HOME/shep.toml` directly through
`ShepToml::try_edit`. Six scalars and a per-dog toggle.

It is hardcoded to that file. `SettingField` is a six-variant enum,
`Settings::rows` returns those six as a literal list, and
`view/settings.rs`'s `scalar_cell` matches on the enum. There is no row model
underneath it to point at different data.

**The Flockfile JSON Schema.** `crates/shep-core/assets/flockfile.schema.json`
is generated from `AppConfig` and printed by the hidden `shep schema`. Every
one of its 39 fields carries a hand-written `init` extension:

```json
"init": { "group": "control", "blurb": "...", "example": "..." }
```

Counted from the exported file: `control` 20, `process` 13, `inputs` 4,
`cron` 2, ungrouped 0. `increment_var` is in the struct and not in the schema,
because `normalize` refuses it.

**A dog's JSON Schema.** From #122. A dog answers `--schema` on its binary,
`shep adopt` asks and records the answer, and `shep-macros` marks a credential
field with `x-shep-secret`. Field descriptions come from schemars, which takes
them from doc comments.

**`apply_group`.** `crates/shep-core/src/config/apply.rs` assigns every sheep
field to `Live`, `NextSpawn`, `NeedsRespawn` or `Structural`. The table is
hand-written and was measured against read sites rather than guessed from
field names.

**The bus topic.** `config.dog.<name>`, from #122. Bark subscribes to its own.
Today the only thing that publishes it is the boot that migrates a section out
of `shep.toml`, at `crates/shep-daemon/src/bus.rs:162`.

## Decisions

### 1. One renderer, three schema sources

`SettingField` becomes a generic field model. Both new panes render a JSON
Schema into a form, and so does the settings screen once it is moved over.

This is not an abstraction invented to share code. A JSON Schema **is** a
field list with types, defaults and descriptions, which is exactly what a
form needs, and both new panes already have one. The settings screen is the
odd one out and joins them.

Sources:

| pane | schema | help text | sections |
| --- | --- | --- | --- |
| sheep | `flockfile.schema.json` | `init.blurb` | `init.group` |
| dog | the dog's own `--schema` | `description` | none, decision 3 |
| `shep.toml` | hand-built from the six scalars | as today | as today |

The settings screen keeps its behaviour exactly. Its snapshots are the
regression test for the move: a generalisation that changes a rendered frame
has changed something it should not have.

### 2. The sheep pane sections by `init.group`

Four sections, in the schema's own order: `process`, `inputs`, `control`,
`cron`. The groups were assigned by hand for `shep init` and they already
answer "what kind of thing is this", so a second grouping invented here would
be a second thing to keep in step.

Every exported field carries one, so there is no fallback section. A field
added without a group is a bug in `AppConfig`, and the renderer says so
rather than inventing a home for it.

### 3. The dog pane is flat, in the schema's declaration order

No sections. A dog's config is small (metrics has 1 field, bark has 5), and
sections over five rows are ceremony.

Nothing is added to `shep-macros` for this. A dog author who wants sections
can write the `schemars(extend(...))` by hand, exactly as `AppConfig` does,
and the renderer will honour it because it reads the same key. That door
stays open without a new attribute, its validation, its docs, and a contract
change every dog author has to adopt.

### 4. A sheep row carries what the change costs. A dog row does not.

`apply_group` is per field, so every sheep row can say whether a change takes
effect now (`Live`), at the next start (`NextSpawn`), or by killing the
running child (`NeedsRespawn`). A `NeedsRespawn` edit arms a confirm naming
what dies, the way lookout's `x`, `R` and `L` keys already do.

A dog row carries nothing, because shep does not know. The dog spec put a
live-versus-needs-restart axis in its schema explicitly out of scope, so
shep publishes the topic and the dog decides for itself. The pane says that
once, at the foot of the screen, rather than per row.

**The asymmetry is the honest answer, not a gap to paper over.** Inventing a
cost for a dog field would mean guessing on behalf of code shep did not
write.

### 5. Structural sheep fields are read-only, and the daemon already agrees

`name` cannot drift, because the app was found by it. `increment_var` is
refused by `normalize` and is not in the schema. `instances` is the only
structural field that reaches the apply path, and a plain load never reshapes
a flock, so it produces a note rather than a scale.

So the pane renders all three read-only and points at the verb that owns
them. This is not a safety measure the pane invents; it is what
`handle_apply_config` does already, made visible.

### 6. The daemon writes a dog's section, not lookout

A new request. lookout sends the edit, the daemon writes `dogs.toml` and
publishes `config.dog.<name>`.

**The daemon has no lock for that file and this decision has to buy one.** An
earlier revision said "under the lock it already holds", which was wrong.
`ConfigLock` and `create_config_file` are `shep-cli`-private, all three of
today's writers are in the CLI, and `DogsConfig`'s own doc calls the file
"deliberately not a locked shep-owned store like `overrides.json`". The
daemon reads it and nothing more. `shep-core`'s `overrides.rs` already has a
sibling-lockfile scheme that both sides can use, so the work is moving to
that rather than writing a third one.

It needs no actor. `RpcContext` already carries `events: Bus` and
`dogs_config: PathBuf`, both in scope where the dispatch match runs, and
`dogs.toml` is not supervisor state the way `overrides.json` is.

This breaks #124's precedent deliberately. lookout writes `shep.toml` itself
and that is fine, because nothing subscribes to `shep.toml`. A dog's section
has a subscriber, and the only publisher is daemon-side, so a direct write
would leave a running dog reading stale config with nothing to tell it. That
would leave #122's re-read mechanism with a single boot-time producer and no
way for an operator to trigger it.

### 6b. A sheep edit needs one new request too, for the read

`Request::ApplyConfig` already exists and already carries the four-way
classification, so the write side is done. The read side is not, and this was
missed when decision 6 was first written.

Nothing on the wire returns a sheep's config values. `Response::Described`
carries `Vec<ProcessInfo>`, whose only config fields are `pending` and
`overridden`, and both are documented as names only, never values, under
IR-41 because `env` can hold a secret. So a pane can learn which fields an
operator has changed and can write a change, and cannot show what
`max_restarts` is currently set to.

A second request answers with the sheep's effective `AppConfig`, `env`
replaced by its key names alone. Every other field is operator-supplied
policy that the pane is about to let them edit, so withholding a value would
make the pane unusable while protecting nothing.

Decision 12 is untouched by this: env VALUES still never cross the wire, and
the pane still writes env without reading it back. Both new requests ride the
one `PROTOCOL_VERSION` bump decision 7 already asks for.

### 7. `PROTOCOL_VERSION` moves 3 to 4

The new request is additive, so by the six precedents in shep-core's
changelog it would not need a bump. Those precedents have now been tested and
found wanting: `ApplyConfig` took that route, and a newer CLI against an older
daemon passed the handshake, sent the request, and had the connection dropped
on an envelope the daemon could not decode. `shep start <Flockfile>` failed on
a dead client rather than on a named version refusal, and
`getting-started.astro` carries a restart-the-shepherd note because of it.

A bump costs every operator one `shep daemon reload` after upgrading and buys
a refusal that names both numbers and the remedy. The last two releases
already asked for that restart.

`SCHEMA_VERSION` does not move. No output envelope changes shape.

### 8. `e` opens the pane for the selected row

One key, on whatever is selected where the cursor is: a sheep in the flock
table, a dog on the settings screen's dog rows. It reads as an action on a
selection, which is what `x`, `R` and `L` already are.

Taken keys at the time of writing: `/ G L R W c g j k q r s x z`.

### 8b. The panes need scrolling, which lookout does not have

`draw_settings` renders `content_lines()` and takes `area.height` off the
front. There is no `skip`, no scroll offset on `Settings`, and no
scroll-into-view when the cursor moves: the cursor clamps to `rows().len()`
and has no notion of which rows were drawn.

That has never bitten because the settings screen is six scalars and a few
dogs, which fits any terminal. A sheep pane is 39 fields under four headers,
so a 30-line terminal shows about a quarter of it and the cursor walks off
the bottom onto rows that were never rendered.

So slice 1 builds a viewport: an offset on the screen state, scroll-into-view
on every cursor move, and a way to see there is more. The settings screen
inherits it and its snapshots must not move, which is the same gate decision
1 already leans on.

### 9. Env is write-only, and it is one row that opens a sub-screen

Decision 12 governs the behaviour and this decision only places it. `env` is
a map, not a scalar, so it cannot be a row like the others. It is a single
row in the `inputs` section that opens a key list.

Per key: a literal shows `<set>` and can be replaced, never read back. A
`{{shared:DB_HOST}}` reference shows in full, because a reference is not a
secret. No request returns env values and `ProcessInfo` gains no env field.

### 10. A dog with no schema gets no pane

Decision 9's rule, unchanged. `e` on such a dog says the dog publishes no
schema and names `$EDITOR` on `dogs.toml`. A raw TOML buffer inside a TUI
would be worse than an editor, and it would show a webhook URL in the clear
for every dog that has not adopted the contract.

## Wire

Two new `Request` variants: one carrying a sheep's name and answering with
its effective `AppConfig` with `env` reduced to key names (decision 6b), and
one carrying a dog's name and the edit.

**Adding a variant produces no compile error for a missing handler.**
`Request` and `Response` are both `#[non_exhaustive]` and `rpc.rs`'s dispatch
ends in a wildcard answering "this daemon does not implement that request",
so a variant with no arm silently answers an internal error at runtime. The
two wire snapshots (`request_wire_v3`, `reply_wire_v3`) are hand-written
literal fixtures with no completeness check, so they do not catch it either.
Both plans list the handler as its own step for that reason.

The bump moves two tests that pin the numeral rather than reading the
constant: `hello_handshake_shape` and
`a_dogs_hello_names_the_dog_and_nothing_elses_does`, both in `request.rs`.
Several other files hardcode "protocol 1" and "protocol 2" on purpose,
simulating an older daemon, and must not be touched. The daemon
writes, publishes `config.dog.<name>`, and answers with the same shape its
other config writes use.

`PROTOCOL_VERSION` 3 to 4 (decision 7). `SCHEMA_VERSION` unchanged.

## Rollout

Two slices, sheep first.

1. **The generic field model and the sheep pane.** Decisions 1, 2, 4, 5, 8,
   9. The settings screen moves onto the model in the same slice, because
   leaving it behind is the duplication this spec exists to avoid, and its
   snapshots are what prove the move was faithful.
2. **The dog pane.** Decisions 3, 6, 7, 10. Reuses the renderer and adds the
   wire request.

Sheep first because it builds the renderer against a schema whose shape is
known and checked in. The dog pane then points that renderer at a schema
shep did not write, which is the harder case and a poor place to discover the
model is wrong.

## Out of scope

- **A live-versus-needs-restart axis in a dog's schema.** The dog spec owns
  that call and left it out. Decision 4 lives with the consequence.
- **Editing `adopted_dogs`, `[interpreters]` or a dog's opaque section from
  the settings screen.** Decision 11 excluded all three and nothing here
  changes that. The dog pane reaches `[dog.<name>]` through a schema, which
  is the thing decision 11 said was missing.
- **Encrypting `dogs.toml`.** Spec 2 of the overrides work owns the encrypted
  store.
- **Reading an env value back.** Decision 12, and it is a rule rather than a
  limitation.

## Testing

- The settings screen's existing snapshots pass unchanged after decision 1's
  move. This is the load-bearing test of the whole slice.
- A rendered frame per pane, per width, as `view/settings.rs` already does.
- A sheep field in each of the four groups lands through `ApplyConfig` and
  comes back in `drifted_fields`.
- A `NeedsRespawn` edit arms a confirm and does not write until it is taken.
- A dog edit reaches `dogs.toml` and publishes `config.dog.<name>`, proven by
  a subscriber receiving it rather than by reading the publish site.
- A dog with no schema gets the refusal from decision 10.
- An older daemon refuses a protocol 4 client at the handshake, naming both
  numbers.

## Docs

`web/src/pages/docs/lookout.astro` for both panes and the `e` key.
`web/src/pages/docs/overrides.astro` for the sheep pane as a way to set an
override. `docs/dogs.md` and `web/src/pages/docs/dogs.astro` for what a dog
author gets by publishing a schema, which is now a pane rather than a
promise. `getting-started.astro` for the protocol bump.
