# Design: config overrides and the settings surface

Status: designed 2026-09-02, not yet implemented. This is spec 1 of two. Spec 2
covers a secret store and is deliberately not designed here; the one thing this
spec owes it is a reserved token, named under Decisions.

shep is 0.1.x and guarantees no API. This pass breaks the wire
(`PROTOCOL_VERSION` 2 to 3) and adds two on-disk stores. It does not break the
Flockfile grammar, and the reasons for that are argued below rather than
assumed.

## The problem

An `AppConfig` change means stop and start. `deferred.md` records the shape of
it from 2026-08-30: an app running four instances, `instances = 5` edited into
the Flockfile, and no way to get the fifth without restarting the other four.
`handle_reload` and the restart path both say so in as many words, *"Nothing
here re-reads configuration."*

`Request::ConfigDrift` closed half of it. An edit that will not apply gets
reported instead of vanishing. Applying it was left open in the code:
*"Whether `start` should reconcile by default, or grow an `--update` flag, is
the maintainer's call and neither is taken here."*

Underneath that sits a second question the maintainer raised, which turns out
to be the larger one. If shep is going to apply config changes, and if
`shep lookout` is going to be where an operator makes them, then the Flockfile
on disk and the daemon's live state are two writers of the same value. The
answer taken here reframes what a Flockfile is, so that they never write the
same thing.

## What already exists

Established from the code, not assumed.

- **`AppConfig` has 40 fields and no source path.** The daemon has never known
  where a Flockfile lives. The CLI reads one, normalizes it, and sends
  `Request::Start { apps }`. Nothing on the entry records the file it came
  from.
- **`ProcessEntry.spec` is a single `ResolvedApp`** (`entry.rs:21`), and
  nothing else on the entry records what the running child was spawned from.
  Overwriting it erases the only account of what is running.
- **`shep stock` writes the stored spec and the muster roll, never a
  Flockfile.** `handle_scale` re-normalizes rather than mutating in place,
  spawns before it writes back so a partial scale cannot claim a count it did
  not reach, and `rpc.rs` records the result unconditionally because a
  `shep save` after a scale would otherwise freeze the old number.
- **`toml_edit` is already in the tree** and `ShepToml::edit` is a
  format-preserving, `flock(2)`-locked, `0600` atomic read-modify-write on
  `shep.toml`, driven by `shep enable`, `adopt`, `rehome` and `style`.
- **`AppConfig` is `#[serde(deny_unknown_fields, default)]`** (`app.rs:73`).
  Every field has a default, so after parsing, a document that declared four
  keys is indistinguishable from one that declared forty.
- **`env` is the only `BTreeMap<String, String>` in the struct.** `args`,
  `ignore_watch` and `watch_options` are ordered lists.
- **`template::render` runs over env values at spawn**, in the daemon, per
  instance (`assemble.rs:224`), and `template::validate` refuses an unknown
  `{{token}}` by name at config time.
- **`init.group` and `init.blurb` exist on every `AppConfig` field**, enforced
  by `scaffold.rs`'s `every_field_carries_a_group_and_a_blurb`, and drive the
  field ordering in what `shep init` writes. Four groups today: control (20),
  process (13), inputs (4), cron (2). shep-cli already enables shep-core's
  `schema` feature.
- **`--allow-control` gates lookout's three action keys**, from the flag or
  from `lookout.allow_control` in `$SHEP_HOME/kv.json`. `app.rs:48` states what
  it is: *"This is a fat-finger catch, not a security boundary."*
- **`shep.toml` is three configs with three apply paths.** `[style]` and
  `[interpreters]` are read by the CLI per invocation, and `[interpreters]`'
  own rustdoc says the daemon never reads it. `[whistle]` is read by
  `shep whistle` in its own process at startup. Only `[daemon]` and `[dog.*]`
  are the shepherd's.
- **`shep daemon reload` already re-reads `shep.toml`.** The successor is
  `execve`d with the same argv and re-enters `boot_supervisor`, which calls
  `DaemonConfig::load_layered` fresh (`daemon.rs:264`). No doc, test or comment
  in the repo says so.
- **Nothing sends an app's env to a client.** `ProcessInfo` has no env field,
  `SheepDrift` reports names only, and none of whistle's nine tools can reach
  it in either direction.

## Three corrections to `deferred.md`'s field split

The three-way split in `deferred.md` was wrong in three places. Two change the
design.

- **`kill_signal` is G2, not G1.** Its ladder-mates `kill_timeout` and
  `graceful_timeout` are computed fresh off the live `slot.entry.spec.config()`
  in `claim_manual` every time a stop, restart, delete or drain fires.
  `kill_signal` is read inside `kill_process` from the `app: &AppConfig`
  parameter of the long-lived per-sheep task, whose `ResolvedApp` is moved in
  once at `spawn_sheep_task` and never refreshed. An edit reaches the next
  spawn, not the next kill.
- **`shutdown_with_message` is G3, not G1.** `assemble()` ORs it into whether
  fd 3 is opened for the child. That is the child's own fd table.
- **The seven `extras` fields apply live only if something re-arms them.**
  `max_memory`, `watch`, `ignore_watch`, `watch_delay`, `cron_restart`,
  `cron_timezone` and `liveness_probe` are all read through a fresh
  `entry.spec.config()` lookup, so the mechanism works. But `arm()` fires on a
  went-online transition and nothing else. A config write alone re-arms
  nothing, and those seven are G2 in practice unless the write triggers it.

Final split across 40 fields: G1 read at decision time, 19. G2 consumed at
spawn, 4. G3 baked into the child, 14. G4 structural, 3.

`autostart` is G2: its only read is in `restorable()`, consulted at muster or
boot. `fold` and `reuse_port` are G1, read fresh in `matching_ids` and
`ReloadMode::of` on every relevant command. `increment_var` is G4, read only by
`normalize.rs` to refuse it by name.

## Decisions

### 1. The Flockfile is a project template, not an operator's config

It is committed to git, it holds the known-working settings for running the
app, and shep never writes it. What the operator tunes afterwards lives in
shep. The systemd analogy is exact: a vendor unit plus drop-ins, with
`systemctl cat` to see which is which.

Taking the analogy means taking its reading tools too, which is decision 9.

### 2. The Flockfile is read when you name a file, never when you name a sheep

`shep start Flockfile.toml` reads it. `shep start koji`, `shep restart koji`
and `shep reload koji` never read any file, in any directory.

This is the whole of the security argument. A Flockfile arrives from the app's
repository. If a routine `shep start`, or a CI job, silently applied whatever
the template now said, a merged pull request would be a path into a running
flock: `user = "root"`, a changed `script`, a swapped `NODE_ENV`. Splitting on
the argument type closes that without a flag anyone can alias on by default.

### 3. A file load is additive, and two flags widen it

- No flag: append keys that are **not in the established set**. Never
  overwrite.
- `--reset`: put non-env process settings back to the template. Keep env and
  keep operator-added keys.
- `--reset-all`: put everything back to the template and delete operator-added
  keys.

**"Not in the established set" is the definition that matters**, and it must
not be implemented as "equals the default". Every `AppConfig` field always has
a value. Established means declared by a previous file load, or set as an
override. An operator who deliberately set `autorestart = false` must not have
a later template quietly set it back.

Recovering the declared key set costs one extra deserialize: the document into
`serde_json::Value`, read each app table's literal keys, then that same value
into `RawFlockfile`. One generic intermediate covers all four formats.
`drifted_fields` already round-trips `AppConfig` through `serde_json::Value`
and documents the `preserve_order` footgun that comes with it.

A template may add, never overwrite. It can still introduce `user = "root"` on
an app where nobody set one, which is inside the trust already extended to a
file whose `script` you execute, and is written down here rather than left to
be discovered.

`--reset` skipping env is a special case and the right one. Env is
operator-supplied data; the rest is operator-tuned policy. Resetting policy is
recoverable, resetting data takes the app's database away.

The original complaint is fixed by `--reset`, not by the default:
`instances = 5` edited into a template reaches a running app through
`shep start Flockfile.toml --reset`, which routes to the existing scale path
and leaves the running instances alone.

### 4. A load never kills a process and never prunes

G1 and G2 fields apply to the stored spec. G3 fields park as pending. An app in
the flock but absent from the file is reported, never deleted.

Pruning would need provenance the daemon does not have, so
`shep start ./a/Flockfile.toml` followed by `./b/Flockfile.toml` would have the
second wipe the first. The survey behind spec 2 found no comparable system that
hot-reloads a changed env into a running process either; every one of them
requires a restart.

### 5. G3 values park in a pending slot, and reload promotes them

`ProcessEntry` gains `pending: Option<ResolvedApp>`. `entry.spec` keeps
describing what is running. Promotion is `entry.spec = pending.take()`, done by
`shep reload` and `shep restart`, so both `decisions.md` entries saying reload
does not re-read config stay true exactly as written: reload still reads only
the stored spec.

Two consequences at the call sites:

- **Promotion resets credentials when `user` or `group` drifted.**
  `ProcessEntry::credentials` is documented as resolved once and reused, so a
  restart never changes a running app's identity underneath it. An operator
  changing `user` is asking for that specific thing, so promotion sets
  `SpawnIdentity::Unresolved` for those two fields only. It needs its own
  argument in the code, not a silent reset.
- **A G1 write must re-arm extras.** `disarm_instance` then `arm_instance`, or
  the seven fields in the third correction above never take effect until the
  next spawn.

A merged config that fails `normalize` refuses that app whole, with the error,
and the rest of the flock still applies. `instances` moving while `out_file`
stays stored can hit the multi-instance `out_file` rule, and a half-applied app
is worse than a refused one. Same partial-failure shape `handle_scale` takes.

### 6. `shep stock` is the first override, not an exception

Under decision 1 there is nothing to contain. `shep stock web 5` sets an
override on `instances`, the same way any other field is set, and its
write-back is the model rather than the anomaly.

### 7. `shep add` registers without starting

`shep add Flockfile.toml`, `shep add ./server`. Same targets `shep start`
takes, same load path, and the only difference is that nothing spawns.

It is here rather than in a later spec because the template model needs it. A
Flockfile shipping `env = { DB_HOST = "", DB_PASSWORD = "" }` is the pattern
decision 10 endorses, and without `add` the first thing an operator does is
`shep start Flockfile.toml`, which spawns a process with an empty database URL.
It crashes, `autorestart` spends the restart budget, and the operator has to
stop a flapping app before they can configure it. With `add` the sequence is
register, fill in, start.

The daemon side exists. `register_at_rest` registers `ProcStatus::Stopped`,
spawns nothing, and is idempotent by name; its only production caller today is
the muster restore of a sheep saved while stopped.
`register_without_spawning`'s doc anticipates this exactly: *"Adding a third
means deciding there, not here."* So the work is a request variant, an rpc arm
and a verb, not new supervisor machinery.

`--reset` and `--reset-all` apply, since the load path is shared. On an app
that is already running it appends config and leaves it running: refusing would
break re-running it after a template edit, and stopping it would break decision
4. An added app has `instances_running = 0`, so `restorable()` will not bring
it up after a reboot while it stays a registered member, which is already the
behaviour and needs no work.

Verb counts both move and stay different for the reason `CLAUDE.md` records:
40 generated to 41, 41 listed to 42, with `help` still the difference.

### 8. Two new stores, both daemon-owned

`$SHEP_HOME/overrides.json` holds override deltas plus each app's established
key set. `$SHEP_HOME/shared-env.json` holds shared env values. Both `0600`,
both written the way `flock.json` already is: staged, `fsync`ed, renamed.

Daemon-owned and RPC-written, not files the CLI edits. A TUI that edits and
applies live cannot route through "write a file, then run a command", and
`shep stock` already proves the shape.

Beside `flock.json` rather than inside it, because the lifecycles differ.
`flock.json` is a snapshot rebuilt from live state. The override store is
authored intent, and losing it loses the deployment.

Shared env is its own store rather than a `shep.toml` section for two reasons:
lookout stays out of that file's free-form maps (decision 11), and spec 2 needs
somewhere it can encrypt.

### 9. Provenance and export

`shep describe` labels each field with where its value came from: the
Flockfile, an override, or the default. Without it, the question "why does prod
differ from staging" stops having a git diff for an answer, which is the cost
of decision 1 and the reason systemd ships `systemctl cat`.

`shep export <name>` writes a resolved Flockfile. A new machine otherwise gets
the template and none of the overrides. `commands/import/render.rs` already
renders a Flockfile through `toml::to_string`, so this is mostly wiring.

### 10. Shared env is referenced per key, through the existing template seam

```
{{shared:DB_HOST}}   this spec
{{secret:DB_HOST}}   reserved for spec 2, same seam
```

No `env_from` and no set membership. Kubernetes ships both bulk `envFrom` and
single `secretKeyRef`, and the single-key one is safer: an app draws what it
needs instead of inheriting a set it never asked for.

`template::render` becomes fallible and a spawn can refuse on an unresolvable
name. `TOKENS` grows, so a `{{shared:...}}` written before this ships is
already a config error naming the token rather than a literal reaching a child.

**A reference in the store does more for secret safety than encrypting the
store would.** With a literal, the value is copied into `flock.json` and the
handover blob. With a reference, those carry the reference and plaintext exists
in two places: the store, and the child's own environment.

A Flockfile shipping `env = { DB_HOST = "", DB_PASSWORD = "" }` is the intended
pattern, the `.env.example` convention. The keys are established at first load,
the operator fills them in lookout, and additive-append means no later load
touches them. `""` needs no special meaning.

### 11. lookout edits settings and overrides, both behind `--allow-control`

`shep.toml`: scalars and per-dog enable/disable toggles routed through the same
code `shep enable` and `shep disable` use. Excluded: `adopted_dogs`, because
`shep adopt` vets a candidate by probing `--version` and refusing a protocol
mismatch, and a raw edit walks past that; `[interpreters]`, a free-form
extension map; `[dog.<name>]`, a third-party dog's opaque schema.

The gate is a fat-finger catch, not an escalation fix. Anyone who can run
lookout can already edit `shep.toml` in an editor. It is there so a keystroke
in a dashboard someone is reading does not rewrite their config.

Apply paths, which differ per section and have to be labelled per field:

| Section | What a change needs |
| --- | --- |
| `[style]`, `[interpreters]` | nothing, the next command reads it |
| `[whistle]` | `shep whistle` restarted, not the daemon |
| `[daemon]`, `[dog.*]` | `shep daemon reload` |

`socket` is the exception inside that last row. A handover successor inherits
the predecessor's bound listener through `rehydrate` and never calls
`bind_socket`, so it computes the new path and discards it. A `socket` change
needs a full stop and start, and lookout has to say so rather than offering a
reload that will not move it.

Dogs across a handover are asymmetric for one reason: `do_start_dog`
short-circuits on a name collision. A name added to `enabled_dogs` starts, a
name removed keeps running and is never told to stop, and a changed
`adopted_dogs` path does not re-spawn. lookout reports this rather than
implying a reload settles it.

### 12. Env is write-only from lookout

lookout can set an env value and can see, per key, whether it is set and what
kind of value it holds. A `{{shared:DB_HOST}}` reference displays in full,
because a reference is not a secret. A literal displays as `<set>` and can be
replaced, never read back.

So no request returns env values, `ProcessInfo` keeps having no env field, and
whistle needs no new rule because there is nothing new for it to call. The cost
is that an operator who forgets what they typed has to overwrite it.

### 13. Panes are generated from the schema, not hand-written

lookout renders from `flockfile_schema_json()`: section headers from
`init.group`, per-field help text from `init.blurb`. Both already exist for
every field and are already test-enforced.

`control` holds 20 of the 40 fields and needs subdividing before it is a usable
page. That is an edit to `GROUP_ORDER` and to per-field `group` values, with
`every_field_carries_a_group_and_a_blurb` keeping it honest.

Grouping the Rust structs instead was considered and refused.
`#[serde(deny_unknown_fields)]` is incompatible with `#[serde(flatten)]`,
because flatten needs a catch-all for keys it does not recognise. So nesting
the types means nesting the TOML, which breaks every existing Flockfile, the
published JSON Schema, `scaffold.rs`, `import/render.rs` and the docs. That is
a breaking release, and it is not `SCHEMA_VERSION`: that constant versions the
output envelope, not the Flockfile grammar.

Because the TUI never names a field, a later cleanup of `AppConfig`'s
pm2-shaped grammar does not mean rewriting it. There is no ordering dependency
between the two.

### 14. `shep daemon reload` validates before it acts

`reload_with_wait` asks the daemon whether its flock can be carried. It never
asks whether `shep.toml` parses. The successor `execve`s, loads config, and on
a semantic error (a bad `log_level`, an unparseable `max_cron_sleep`)
`daemon_exit_code` maps it to `ExitCode::InvalidConfig` and exits. The
predecessor is already gone, so the flock keeps running with nothing
supervising it.

`toml_edit` catches syntax, so `ShepToml::edit` cannot write a file that fails
to parse. It does not catch a valid-TOML bad value.

The fix is a `DaemonConfig::load` before anything is signalled, refusing with
the parse error. `whistle/mod.rs:171` already does exactly this check. One
pre-flight covers a hand-typed reload, lookout's apply, and an init system's
SIGHUP.

This is a live bug that predates the feature. It is fixed here because decision
11 adds a button that triggers it.

### 15. `SpawnSpec` gets a redacted Debug

`runner.rs:1017` derives plain `Debug` with an unredacted
`pub env: BTreeMap<String, String>`, and the type sits on the exec boundary at
`command.envs(&spec.env)`. Four sibling types that carry env all have a
redacted `Debug` with an exact-string test per IR-41: `AppConfig`, `SavedApp`,
`CarriedSheep`, `OsProber`.

Not a live leak today. No production `tracing` call formats it, and the only
`{spec:?}` in the tree is a test assertion over an empty env. It is the one
link in the chain missing the guard, and this spec makes env editable from a
TUI.

## Wire

```rust
struct DeclaredApp {
    config: AppConfig,
    declared: BTreeSet<String>,
    declared_env: BTreeSet<String>,
}

Request::ApplyConfig { apps: Vec<DeclaredApp>, reset: ResetDepth }
Request::SetOverride { name: String, patch: serde_json::Map<String, Value> }
Request::ClearOverride { name: String, fields: Vec<String> }
Request::Overrides { name: String }
```

A patch rather than one field per request: a TUI save commits several at once,
and each has to normalize against the others.

`Request::Overrides` answers with values for every field except `env`, which
comes back as per-key metadata only (set or unset, literal or reference, and
the reference text when it is one). Decision 12 is enforced by the response
type rather than by a rule someone has to remember.

`Request::ConfigDrift` is unchanged and stays read-only. Its doc already argues
for `&self` on the grounds that applying under a running sheep is the outcome
being ruled out, which was right for a request that reports. The applier is a
sibling.

`PROTOCOL_VERSION` 2 to 3. New variants an older daemon cannot deserialize, so
it refuses a newer client at the handshake and the operator restarts the daemon
after upgrading. Same shape and same remedy as `SelectorSpec::Instance`.

`SCHEMA_VERSION` stays 1. `ProcessInfo` gains `pending: Vec<String>` and
`overridden: Vec<String>`, both additive, both names only, because `env`
carries secrets and names-only is the rule `drifted_fields` already follows.

`ProcessEntry` gains `pending`, and `CarriedSheep` gains it too. Absent keys
load as `None`, matching the eight existing
`a_blob_written_before_<field>_was_carried_still_loads` tests.

## Surfacing

`shep flock` grows a marker column for an entry with pending or overridden
fields, following the EXIT column added 2026-08-19, and `shep flock`'s adaptive
column dropping handles the width. `shep describe` lists the field names and
their sources. lookout's flock table renders the same data, so it picks the
marker up without separate work.

## Out of scope

- **Encryption of the shared store.** Spec 2. The research behind it found that
  a daemon which must decrypt unattended needs the key machine-reachable, so
  the honest claim is protection for a file that leaks or gets copied, not
  protection against a compromised host. That claim is worth having and worth
  stating precisely, which is why it gets its own threat model.
- **The `{{secret:...}}` resolver.** Token reserved here, resolver in spec 2.
- **Cleaning up `AppConfig`'s pm2-shaped grammar.** A breaking release of its
  own. Decision 13 removes the ordering dependency.
- **The handover blob left on disk after a failed adopt.** Deliberate today, as
  operator-readable evidence, and argued as no worse than `flock.json`. If spec
  2 tightens `flock.json`, this becomes the most exposed copy and the argument
  needs revisiting.
- **Log files outside `$SHEP_HOME` getting no forced mode.** Real, unrelated to
  this change, and belongs with spec 2's hardening.

## Testing

Every await needs a forcing mechanism rather than a sleep (IR-46), and every
test below gets proved non-vacuous by mutating what it protects and watching
that specific test go red.

The one that matters most is the extras re-arm. Set `watch = true` through an
override on a running sheep and assert the watcher fires. Delete the
`disarm_instance` and `arm_instance` pair and it must go red: without them the
write lands on `entry.spec` and nothing reads it until the next spawn, which is
the silent under-delivery this design exists to avoid.

- an established key is not overwritten by a later file load
- a key absent from the established set is appended by a later file load
- `--reset` restores process settings and leaves env alone
- `--reset-all` deletes operator-added keys
- a G3 override lands as pending and does not touch the running child
- `shep reload` promotes pending, and promoting a `user` change resets
  `credentials` to `Unresolved`
- a merged config that fails `normalize` refuses that app and applies the rest
- `shep start <name>` reads no file, with a Flockfile present in the working
  directory
- `Request::Overrides` returns no env value, exact-string
- `SpawnSpec`'s Debug, exact-string, matching its four siblings
- a `shep.toml` holding valid TOML with a bad value makes `shep daemon reload`
  refuse, with the flock still supervised afterwards
- a handover blob written without `pending` still loads

The daemon tier runs on the inner loop,
`cargo test -p shep-daemon --lib --all-features -- --skip ::slow::`. Nothing
here needs the slow tier unless a watch test ends up timing-sensitive, in which
case it belongs in a `mod slow` and in CI's serial job.

## Docs

`web/` is published and part of the deliverable, so the CLI reference gets
regenerated and both `astro build` and `astro check` run. `check` is the one
that catches a wrong prop; a build stays green on one.

- a new overrides page: the template model, the three load modes, provenance,
  export
- `first-flockfile.astro`: a Flockfile is a template, and the empty-value
  convention
- `getting-started.astro`: the upgrading section frames `shep daemon reload`
  entirely around the binary and says nothing about `shep.toml`
- `docs/dogs.md` and `dogs.astro`: a newly enabled dog does start on a
  successor, which is a stronger claim than what is there now
- `whistle.astro` and `docs/whistle/README.md`: `[whistle]` is not covered by
  a daemon reload
- `decisions.md`: entries for the decisions above, plus one saying explicitly
  that the two existing "reload does not re-read config" entries are about
  `shep reload <sheep>` and the Flockfile. That is the thing most likely to be
  misquoted into a wrong doc.
- `deferred.md`: close the config-edit entry, and record the three field-split
  corrections
- `CLAUDE.md`: `shep stock` is no longer the exception, and the reload
  paragraph needs the pre-flight

## Migration

- **`PROTOCOL_VERSION` 2 to 3.** An older daemon refuses a newer client at the
  handshake. Restart the daemon after upgrading.
- **No Flockfile grammar change.** Every existing Flockfile parses unchanged.
- **First load of an existing app establishes its current keys.** An operator
  upgrading has an app whose config came from a Flockfile, so the established
  set is what that file declared. No override store exists yet, and additive
  append is a no-op on the first run.
- **`shep stock` behaves identically** from the outside. It now writes an
  override rather than the spec directly, and `--reset-all` is the way to
  discard one.
