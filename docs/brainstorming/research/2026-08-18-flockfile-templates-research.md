# Research: Flockfile scaffolding / "add an app to an existing Flockfile" verbs

Research only, for the maintainer. No pm2 source was opened; everything below comes from
this repo (`shep`, branch `main`) and its own docs.

## 1. What a Flockfile is today

**Document shape.** A Flockfile deserializes into `RawFlockfile`
(`crates/shep-core/src/config/flockfile.rs:35-51`), a private, strict
(`#[serde(deny_unknown_fields)]`) struct with exactly two fields:

- `$schema: Option<String>` (line 49) — an editor hint, read and discarded,
  never validated.
- `app: Vec<AppConfig>` (line 51, `#[serde(default, rename = "app")]`) — the
  list of sheep, under the TOML key `app` (`[[app]]` tables). The comment at
  lines 23-26 records a **forward-compat decision**: the top level is locked
  to exactly `$schema` and `app` on purpose, so a typo'd top-level key fails
  loudly instead of being silently ignored.

`RawFlockfile` is validated (non-empty `app`) and converted into the public
`Flockfile { pub apps: Vec<AppConfig> }` (lines 17-21) by
`Flockfile::parse` (lines 158-186). An empty `app: []` is a hard error,
`FlockfileError::NoApps` (line 183, 300-310).

**Per-app fields — `AppConfig`** (`crates/shep-core/src/config/app.rs:73-231`).
`#[serde(deny_unknown_fields, default)]` (line 72) — every field has a
default via `impl Default for AppConfig` (lines 232-278), so nothing is
serde-`required`; **but** `normalize()` (see below) rejects an empty `name`
or `script` at validation time, so those two are the only fields a real
Flockfile must set. Full field list with the shipped defaults (from the
`Default` impl, lines 232-278):

| field | type | default |
|---|---|---|
| `name` | `String` | `""` (rejected empty by `normalize`) |
| `script` | `String` | `""` (rejected empty by `normalize`) |
| `args` | `Vec<String>` | `[]` |
| `cwd` | `Option<String>` | `None` |
| `interpreter` | `Option<String>` | `None` |
| `env` | `BTreeMap<String,String>` | `{}` |
| `instances` | `u32` | `1` |
| `autorestart` | `bool` | `true` |
| `autostart` | `bool` | `true` |
| `stop_exit_codes` | `Vec<i32>` | `[]` |
| `min_uptime` | `UpDuration` | `1000ms` |
| `max_restarts` | `u32` | `16` |
| `restart_delay` | `Option<UpDuration>` | `None` |
| `exp_backoff_restart_delay` | `Option<UpDuration>` | `None` |
| `kill_signal` | `Option<String>` | `None` (⇒ `SIGTERM`) |
| `kill_timeout` | `UpDuration` | `1600ms` |
| `shutdown_with_message` | `bool` | `false` |
| `listen_timeout` | `UpDuration` | `3000ms` |
| `graceful_timeout` | `UpDuration` | `8000ms` |
| `action_timeout` | `UpDuration` | `3000ms` |
| `max_memory` | `Option<MemSize>` | `None` |
| `watch` | `bool` | `false` |
| `ignore_watch` | `Vec<String>` | `[]` |
| `watch_delay` | `Option<UpDuration>` | `None` |
| `cron_restart` | `Option<String>` | `None` |
| `fold` | `Option<String>` | `None` |
| `user` / `group` | `Option<String>` | `None` |
| `out_file` / `err_file` | `Option<String>` | `None` |
| `merge_logs` | `bool` | `false` |
| `channel` | `bool` | `false` |
| `stdin` | `bool` | `false` |
| `wait_ready` | `bool` | `false` |
| `reuse_port` | `bool` | `false` |
| `readiness_probe` / `liveness_probe` | `Option<ProbeConfig>` | `None` |
| `watch_options` | `Vec<String>` | `[]` |
| `cron_timezone` | `Option<String>` | `None` |
| `increment_var` | `Option<String>` | `None` |

`AppConfig::minimal(name, script)` (`app.rs:279-288`) is the one
programmatic "just the required fields" constructor — everything else stays
at spec default. It's the same constructor a plain `shep start <script>`
uses (see §"design questions" below).

**Raw vs. normalized.** `AppConfig` (the parsed/raw form) is turned into
`ResolvedApp` — "a proof token: constructing one is only possible through
`normalize`" (`crates/shep-core/src/config/normalize.rs:1-4, 60-73`) —
by `normalize()` (lines 115-233) and `normalize_all()` (lines 294-303, which
additionally rejects duplicate names, `NormalizeError::DuplicateName`).
`normalize` is where real validation happens — a generator writing a
Flockfile must satisfy this, not just `AppConfig`'s serde shape:

- `NormalizeError::MissingName` (line 117) / `MissingScript` (line 122) —
  empty `name`/`script`.
- `InvalidName` (line 119) — `name` contains `/`, `\`, or is `.`/`..`.
- `ZeroInstances` (line 126) — `instances == 0`.
- `InvalidCron` / `InvalidTimezone` — `cron_restart`/`cron_timezone` parse
  failures (lines 128-143).
- `InvalidProbe` (line 257) / `ZeroFailureThreshold` (line 265) /
  `IntervalBelowMinimum` (line 279) — probe validation, delegated to
  `validate_probe` (lines 249-291).
- `ZeroMaxMemory` (line 160).
- `InvalidKillSignal` (line 173) — a `kill_signal` string `KillSignal::parse`
  rejects.
- `ActionTimeoutTooLong` (line 186) — `action_timeout >= 58_000ms`
  (`MAX_ACTION_TIMEOUT`, line 55).
- `WatchWithoutCwd` (line 197) — `watch = true` with no `cwd` set.
- `ZeroWatchDelay` (line 208).
- `InvalidWatchGlob` (line 232) — a `watch_options`/`ignore_watch` pattern
  `globset` won't compile, delegated to `validate_watch_globs`
  (lines 226-238).

The full `NormalizeError` enum definition (variant order matches the above)
is at `normalize.rs:312-...`, e.g. `ZeroInstances` line 321, `InvalidCron`
line 325, `InvalidTimezone` line 332, `InvalidProbe` line 340,
`ZeroFailureThreshold` line 348, `IntervalBelowMinimum` line 357,
`ZeroMaxMemory` line 369, `ActionTimeoutTooLong` line 377,
`InvalidKillSignal` line 387, `WatchWithoutCwd` line 395, `ZeroWatchDelay`
line 401, `InvalidWatchGlob` line 407.

A generator therefore only strictly needs to emit `name` and `script`; every
other field is optional and defaults exactly as the table above says — the
same defaults `AppConfig::minimal` produces and the same defaults
`shep start <script>` (no Flockfile) registers.

## 2. Formats supported

Four parse formats, `FlockFormat` (`crates/shep-core/src/config/flockfile.rs:120-131`):
`Toml`, `Yaml`, `Json`, `Json5`, dispatched by extension in
`FlockFormat::from_path` (lines 133-144): `.toml` → Toml, `.yaml`/`.yml` →
Yaml, `.json` → Json, `.json5` → Json5, anything else (including `.js`) →
`None`.

**`.js` is deliberately excluded from `FlockFormat`.** shep-core "never
executes anything" (module doc, lines 1-6); a `.js` Flockfile is handled
entirely in `shep-cli`, by shelling out to `node -e` and feeding the
resulting JSON back through `FlockFormat::Json`
(`crates/shep-cli/src/commands/lifecycle.rs:146-236`,
`evaluate_js_flockfile`). It is reachable **only** via `shep start --flockfile
some.js`, never by discovery and never by extension alone — enforced by
`resolve_target`'s dispatch order (`lifecycle.rs:277-338`, see §3) and locked
in by two tests: `discovery_never_names_a_js_file_and_stays_ten_names`
(`flockfile.rs:459-472`, asserts none of the 10 discovery names end in
`.js`) and the doc comment on `StartArgs::flockfile`
(`crates/shep-cli/src/cli.rs:577-585`: "Required for a `.js` Flockfile and
the only way to reach one... Without this flag `shep start server.js` starts
`server.js` as a script").

**JSON Schema.** Generated from `RawFlockfile` (not `AppConfig` alone —
`AppConfig`-only would reject every real `{"app": [...]}` document), via
`schemars::schema_for!` in `flockfile_schema_json()`
(`flockfile.rs:107-114`), behind the non-default `schema` feature on
shep-core (`crates/shep-core/Cargo.toml:23-32`, `schema = ["dep:schemars"]`,
off by default). Printed by the hidden `shep schema` verb
(`crates/shep-cli/src/cli.rs:547-551`, `Commands::Schema`;
`crates/shep-cli/src/commands/schema.rs:20-22`, ignores `--format`
deliberately since the output already is JSON). The committed copy at
`crates/shep-core/assets/flockfile.schema.json` is drift-guarded by
`the_committed_schema_is_current`
(`flockfile.rs:580-589`, `#[cfg(feature = "schema")]`, compares
`flockfile_schema_json()` byte-for-byte against `include_str!`'d
`COMMITTED`, line 78) — this only runs under `--all-features`/`--features
schema`, not a bare `cargo test -p shep-core`. Regeneration command, named in
the test failure: `cargo run -p shep-cli -- schema >
crates/shep-core/assets/flockfile.schema.json` (`flockfile.rs:84-86`,
`REGENERATE`). The schema describes the **deserializer** shape, not
`normalize`'s narrower validation (e.g. `kill_signal` is an unconstrained
string in the schema even though only 4 spellings survive `normalize`) —
documented at `flockfile.rs:96-100` and in `deferred-history.md`'s schemars
entry.

## 3. Discovery

Ten fixed names, in order, `DISCOVERY_ORDER`
(`crates/shep-core/src/config/flockfile.rs:267-278`):

```
Flockfile.toml, Flockfile.yaml, Flockfile.yml, Flockfile.json, Flockfile.json5,
flockfile.toml, flockfile.yaml, flockfile.yml, flockfile.json, flockfile.json5
```

`discover(dir: &Path)` (lines 281-286) returns the first of these that
`is_file()` in **one directory only** — no ancestor/upward search. Confirmed
by its only two call sites, both passing the CLI's own `current_dir()`
directly with no walk-up: bare `shep start` with no targets
(`crates/shep-cli/src/lib.rs:986-989`) and `shep runtime`
(`crates/shep-cli/src/commands/runtime.rs:95`, and `shep dev` shares the
same foreground engine per the module's own doc). No `.js` name is in the
list — enforced by the discovery test cited in §2.

## 4. What already writes config files — `ShepToml`

`crates/shep-cli/src/commands/shep_toml.rs` is a comment-and-key-order
-preserving editor for `$SHEP_HOME/shep.toml`, built on `toml_edit::DocumentMut`
rather than round-tripping through plain `toml::Table`
(module doc, lines 1-27: "a `shep enable` that reformatted it... would be a
reason not to run `shep enable`"). Conventions worth carrying into a
Flockfile writer:

- **One writer type, one entry point.** `ShepToml::edit` (lines 109-113) and
  `ShepToml::try_edit` (lines 141-149, for a closure that can itself refuse)
  are the *only* way to reach a write: `open_locked` → read → closure →
  `save()`. Read and write are never separate public steps — "a caller that
  could read, think, and then write would be the lost update this type takes
  a lock to prevent" (lines 55-58).
- **Exclusive lock across the whole read-modify-write**, `ConfigLock`
  (lines 468-... , acquired in `open_locked`, lines 161-178) held via
  `flock(2)` on a sibling `shep.toml.lock`, motivated explicitly by a real
  prior bug: two writers racing on `barks.jsonl` "silently lost half of each
  other's records" before it grew the same lock (lines 96-102).
- **Missing file ⇒ empty document, not an error**; a file that exists but
  fails to parse is **refused, not overwritten** (`open`, lines 187-206) —
  "it may hold every knob a daemon boots with... there is no undo for losing
  it to a typo'd verb" (lines 51-54).
- **Atomic write**: staged in a `0600` sibling temp file
  (`create_config_file`, lines 446-455, `CONFIG_FILE_MODE = 0o600`, line 46),
  `fsync`'d, then `rename`'d over the target (`save`, lines 364-378) — "a
  crash, a signal or an `ENOSPC` between the truncate and the write" is the
  failure mode plain `std::fs::write` (`O_TRUNC`) exposes (lines 347-354).

**Two lessons from its own recent history**, both named explicitly in doc
comments and confirmed in `git log --follow`:

1. `b123c75 fix(shep): set_style_level reports rather than panics on a scalar
   \`style\` key` — the four earlier setters (`enable_dog`/`disable_dog`/
   `adopt_dog`/`dog_table_mut`, lines 219-224, 253-259, 393-410) all use
   `.entry(..).or_insert_with(..).as_table_mut().expect(..)`, sound only
   because nothing else in the file ever writes those keys as anything but a
   table. `set_style_level`'s key (`style`) **can** be hand-written by an
   operator as `style = "full"` (a scalar, not `[style]`), so the same
   `.expect()` shape was reachable from operator-controlled input — "exactly
   the panicking-constructor shape IR-21 rules out" (lines 317-328). Fixed to
   return `ShepTomlError::WrongShape` instead (lines 329-343). The four
   sibling setters still carry the old panicking shape and are noted as "a
   tracked follow-up, not this fix's scope" (line 328) — worth knowing if a
   Flockfile writer reuses this idiom for any key an operator could
   plausibly hand-write into an unexpected shape.
2. `d023465 fix(shep): a refused style write must not rewrite shep.toml` — a
   `try_edit` whose closure returned `Err` still, before the fix, staged a
   byte-identical file and `rename`'d it over the original: same bytes, new
   inode, mode force-reset to `0600` even on a file the edit never actually
   touched (test doc, `shep_toml.rs:853-860`; assertions at lines 896-907
   check same-inode *and* unchanged mode, not just unchanged bytes). The
   general lesson: "refused" must mean the file is byte- **and inode**-
   identical, not merely content-identical after a stage/rename round trip.

`ShepToml` is TOML-only and is a single-document editor (one file,
`$SHEP_HOME/shep.toml`) — it has no notion of "one of N formats" or of
appending a new entity to an array-of-tables the way a Flockfile's `[[app]]`
list would need.

## 5. Closest existing generator — `shep import`

`crates/shep-cli/src/commands/import/` converts a pm2 `dump.pm2` (JSON) into
a brand-new Flockfile. Pipeline: `dump::parse` → `env` (splits env into
Flockfile-safe vs. left-out) → `convert::convert` → `render::flockfile`.

**`render.rs`** (`crates/shep-cli/src/commands/import/render.rs`) is the
closest thing to a generator in the tree today:

- Serializes a **purpose-built projection**, `Rendered`
  (lines 19-53), not `AppConfig` directly — "an importer that serialized
  it directly would write every one of [~40] fields... burying the handful
  an operator actually needs to read" (module doc, lines 1-7). Only 12
  fields are ever considered for output (`name`, `script`, `args`, `cwd`,
  `interpreter`, `instances`, `autorestart`, `restart_delay`, `max_memory`,
  `merge_logs`, `reuse_port`, `increment_var`, `env`), and every one of them
  is `#[serde(skip_serializing_if = ...)]`'d against its spec default (lines
  23-52), so an imported app that only differs by `script`/`name` renders as
  just those two lines (proven by the `defaults_are_left_out` test, lines
  153-160: `AppConfig::minimal("web", "./srv")` renders to exactly
  `[[app]]\nname = "web"\nscript = "./srv"`).
- Uses **plain `toml::to_string`** (`flockfile` fn, lines 117-122), not
  `toml_edit` — this is a from-scratch document, never an edit of an
  existing one, so there is no comment/key-order to preserve.
- `Doc { #[serde(rename = "app")] apps: Vec<Rendered> }` (lines 101-105) —
  explicit rename to the `app` key `RawFlockfile` requires, called out as an
  easy mistake ("`Flockfile`'s own public field is named `apps`... but the
  document key `Flockfile::parse` requires is `app`", lines 95-100).
- **Round-trip tested against the real parser**:
  `flockfile_round_trips_through_the_real_parser` (lines 139-147) renders,
  then re-parses with `Flockfile::parse` + `FlockFormat::Toml`, and asserts
  equality with the apps that went in — "fails if the renderer emits a
  Flockfile shep cannot read back" (lines 132-138). This pattern (render →
  reparse → assert-equal) is the right acceptance test for any new
  Flockfile-emitting code.
- Newtypes (`MemSize`, `UpDuration`) render in their string form
  (`"512M"`, `"5s"`), verified by `newtype_values_render_in_their_string_form`
  (lines 168-176), not as raw integers — a raw integer would fail `normalize`
  or fail to reparse.

**`import`'s own write path** (`crates/shep-cli/src/commands/import/mod.rs`,
lines 22-45 module/fn doc, 130-152 the actual write):

- Output path: `args.out`, else `./Flockfile.toml` (lines 133-136) — always
  TOML, never format-selectable.
- **Refuses to overwrite** an existing file unless `--force` (lines 137-144,
  `ExitCode::Usage`) — this is the existing precedent for "don't clobber an
  operator's file," but note it's whole-file refuse-or-clobber, **not**
  append/merge. `import` has **no notion of adding to an existing
  Flockfile** at all.
- `--dry-run` prints the rendered Flockfile to stdout with no envelope and
  writes nothing, explicitly so `shep import --dry-run > Flockfile.toml`
  produces a file shep can read back (lines 33-38, 129-131, and the
  `dry_run_prints_a_parseable_flockfile_and_writes_nothing` test at lines
  274-303).
- Plain `std::fs::write` (line 145) — **not** the atomic stage-then-rename
  pattern `ShepToml::save` uses. A new Flockfile writer that wants the same
  crash-safety `shep_toml.rs` earns itself would need to add that; `import`
  doesn't have it today.
- `ImportArgs` (`crates/shep-cli/src/cli.rs:942-955`): `--from`, `--out`,
  `--dry-run`, `--force` — a plausible flag shape to mirror for new verbs.

## 6. CLI verb conventions

**Flat verbs, no nesting.** `Commands` (`crates/shep-cli/src/cli.rs:201-551`)
is one `clap::Subcommand` enum; every verb (`Start`, `Stop`, `Import`, etc.)
is a **top-level, flat** variant — there is no example anywhere in this tree
of a nested subcommand (`shep config import`-style). `import` itself is
`Import(ImportArgs)` at line 491, a flat top-level verb. Any scaffolding verb
should follow the same flat pattern (e.g. `shep <verb>`, not `shep flock
init`).

**`--format`** is a single global flag, `Format` (`Table`/`Json`,
`cli.rs:161-166`), folded into every subcommand via `GlobalArgs`'s
`#[command(flatten)]` (`cli.rs:112-115, 122-158`) — not per-verb.

**Exit codes** — `crates/shep-cli/src/exit.rs:16-82`, a fixed 12-value
`#[repr(u8)]` enum, each with a stable `code_str()`
(lines 92-108) used verbatim in `--format json`'s `error.code`:
`success(0)`, `failure(1)`, `usage(2)`, `not_found(3)`, `invalid_config(4)`,
`daemon_unreachable(5)`, `protocol_mismatch(6)`, `spawn_failed(7)`,
`deadline_exceeded(8)`, `internal(9)`, `daemon_already_running(10)`,
`flock_empty(11)`. A Flockfile-writing verb maps naturally onto
`Usage` (bad args, e.g. "file already exists, pass --force" as `import`
already does) and `InvalidConfig` (the assembled document fails to
parse/normalize, mirroring `import`'s use of `InvalidConfig` for a bad dump,
`import/mod.rs:73-83`).

**Two invariant tests any new visible verb must satisfy**
(`crates/shep-cli/src/cli.rs`):

1. `every_visible_verb_appears_in_exactly_one_help_group` (lines 1103-1128):
   every non-hidden subcommand name must appear in exactly one entry of the
   hand-written `HELP_GROUPS` table (lines 26-53) — a new verb needs a
   `HELP_GROUPS` entry (and per `the_help_template_and_the_group_table_agree`,
   lines 1132-1148, a matching line in the literal `HELP_TEMPLATE` string,
   lines 57-88).
2. `the_top_level_help_has_no_dashes_or_doc_link_syntax`
   (lines 1211-1233): the rendered `--help` text must contain no em dash
   (`\u{2014}`), no en dash (`\u{2013}`), and no Rust intra-doc-link syntax
   (`` [` ``) — this fires off the verb's own `///` doc comment (which clap
   turns into help text), so a new verb's doc comment needs plain prose, no
   dashes, no `[\`Thing\`]` links. (There is also
   `the_top_level_help_carries_no_implementation_notes`, lines 1178-1198,
   guarding against leaking internal implementation words like "bin_name"
   into `--help`.)

## 7. Terminology fit

From `docs/terminology.md`: the lexicon is `shepherd` (daemon), `flock`
(the list, always plural), `sheep` (one process, singular only), `dog`
(plugin), `lamb` (child process), `fold` (group/namespace), **`Flockfile`**
(the config file itself, already named — line 21: "app config file...
Flockfile (`Flockfile.toml` / `.yaml` / `.json`)"). Usage rules (lines
39-50): straight verbs (`start`/`stop`/`list`) stay first-class aliases
forever (rule 1); destructive/precise operations and error text stay plain,
zero whimsy (rule 2); types may be themed only when self-evident, never
opaque (rule 3).

**No existing lexicon entry names a scaffolding/generation verb** — this is
genuinely open territory, not a naming gap in an otherwise-covered table.
Candidates that would fit the existing register: something built on
`Flockfile` itself (the term is already established and not overloaded)
rather than inventing new pastoral vocabulary — e.g. a verb literally named
around "flockfile" reads as in-register without stretching for a pun the way
forcing a new sheep/shepherd metaphor onto "generate a config file" might.
Rule 2 (destructive/plain) matters here specifically for the "add an app to
an existing Flockfile" verb if it can silently clobber or reorder — that
verb's *name* can be playful, but its refuse/overwrite messaging should stay
as plain as `import`'s "`{path} already exists; pass --force to overwrite
it`" (`import/mod.rs:138-141`). A pure "write me a fresh one" verb has lower
stakes than an "edit one I already have" verb, and the theme rules apply
more cautiously to the latter.

## 8. Already planned or deferred?

**Nothing.** Searched `docs/specs/deferred.md`, `docs/specs/shep-v1.md`, and
`docs/systematic-refactor/refactor-workspace/{map,goals}.md` for
`scaffold`/`template`/`init`/`generate`/`starter`: every hit is unrelated —
`docs/specs/shep-v1.md:479` and `deferred-history.md`'s `shep startup` /
`unstartup` entry are about the **systemd/openrc/launchd/rc.d unit
generators** (a different kind of "generate a file"),
`deferred-history.md`'s schemars entry is the Flockfile **JSON Schema**
(already covered in §2), and
`goals.md:16`/`map.md:875` are about **bark webhook payload templates**, an
unrelated feature. **This request is not specced, not deferred, and not
mentioned anywhere in the design docs** — it is new ground, which is itself
the most important finding here: there is no prior decision to reconcile
with, but also no existing rationale to lean on for format/verb-name calls.

## Design-relevant questions, answered from the code

**Default output format if a generator writes a Flockfile.** TOML.
Three independent facts point the same way: (1) `shep import` already
defaults to `./Flockfile.toml` and its renderer (`render.rs`) only ever
emits TOML via `toml::to_string`; (2) TOML is the *only* format with a
proven round-trip test in the tree (`flockfile_round_trips_through_the_real_parser`)
and the only one with a comment-preserving editor already built
(`toml_edit`, used by `shep_toml.rs`); (3) the dependency tree confirms this
isn't incidental — `serde-saphyr` (the YAML backend, `crates/shep-core/Cargo.toml:38`,
pinned in the root `Cargo.toml:35`) is declared with
`default-features = false, features = ["deserialize"]` **only** — YAML
*serialization* is not even enabled in this workspace today. JSON
(`serde_json`) and JSON5 (`json5` crate) can both serialize, but discovery
prefers TOML first among same-cased names (`DISCOVERY_ORDER`, TOML is index
0) and TOML is the only format anyone in this codebase has written a
generator for.

**What "adding onto an existing one" has to preserve, and whether toml_edit
transfers.** For a TOML Flockfile: yes, `toml_edit::DocumentMut` is exactly
the right tool and `shep_toml.rs` is the direct precedent to model an
appender on — comment- and key-order-preserving edits, a lock across
read-modify-write, atomic stage+rename, and (learn from its two bugs) no
panicking `.expect()` on a shape an operator could plausibly have
hand-written differently, and a refused/no-op edit must leave the file
byte- **and inode**-identical. Concretely, appending an app means pushing a
new `[[app]]` entry onto the document's `app` array-of-tables via
`toml_edit`, not re-serializing the whole parsed structure (which would
drop comments and reflow everything, the exact failure `shep_toml.rs`'s
module doc says makes a tool nobody wants to run). **For YAML/JSON/JSON5,
there is no equivalent in this dependency tree today** — no comment-
preserving YAML editor crate is present (only `serde-saphyr`, deserialize-only,
and it is not documented anywhere in this tree as supporting a `DocumentMut`-
style AST); JSON has no comments to preserve but also no existing
"insert into an array while leaving unrelated formatting alone" tool in the
tree (`serde_json::Value` round-trips data but not formatting); JSON5 (via
the `json5` crate) has comment syntax but the crate in this tree isn't used
anywhere for anything but read-back in tests. Supporting "add an app" for
every discoverable format is real, currently-unaddressed scope — TOML alone
is a much smaller, already-precedented lift.

**Existing notion of a template/starter config.** No CLI-generated one.
The only "starter Flockfile" in the repository is **prose**, in the docs
website: `web/src/pages/docs/first-flockfile.astro:136-138` hand-writes
`[[app]]\nname   = "web"\nscript = "./server"` (aligned `=` signs) inside a
`<CodeBlock>` for a human to copy — not produced by any verb, not tested
against the real parser, and not part of the `shep` binary at all.

**What `shep start <script>` registers with no Flockfile, and the trap for a
generator.** `lifecycle::resolve_target`
(`crates/shep-cli/src/commands/lifecycle.rs:277-338`) falls through to its
last matching arm (`_ if path.exists()`, lines 331-357) when `target` is a
plain existing path with no recognized Flockfile extension and no
`--flockfile` flag. It builds `AppConfig::minimal(name.unwrap_or(stem),
&script)` where:

- `name` is `--name` if given, else the path's file stem (line 332,
  `path.file_stem()`).
- `script` is **canonicalized** if the path was relative (lines 339-345,
  `std::fs::canonicalize`) — so `shep start ./server.js` registers an
  *absolute* script path, not `./server.js` verbatim. An absolute path
  typed as-is is left untouched (not re-canonicalized, to avoid macOS's
  `/var` → `/private/var` symlink surprise, lines 335-338).
- `app.cwd` is explicitly set to the operator's `current_dir()` (lines
  348-357) — **not left `None`**. The comment explains why at length: an
  unset `cwd` would leave the child inheriting the *daemon's* directory
  (invisible from the command line, wherever the shepherd happened to be
  spawned), which "breaks in a quieter way than a missing binary does."

**This is the trap the request called out.** A Flockfile is normally
hand-written with a *relative* `script` and no `cwd` at all (see the
`first-flockfile.astro` example above, and `import/render.rs`'s
`AppConfig::minimal("web", "./srv")` test fixture, which also leaves `cwd`
unset) — `normalize()` allows `cwd: None` freely; nothing requires it. If a
new "scaffold a Flockfile from a script" verb blindly reused this same
`resolve_target` code path (absolutize + set `cwd`), it would emit a
Flockfile pinned to the machine that generated it — wrong the moment it's
committed to a repo and run somewhere else, or even just moved. A generator
almost certainly wants the *Flockfile-native* convention (relative `script`,
`cwd` left `None` so it inherits wherever the shepherd is later pointed) —
i.e., closer to what a human would hand-write, and to what `import/render.rs`
already renders, than to what `shep start <script>` registers today. This
divergence should be a deliberate, stated design decision, not an accident
of reusing `resolve_target`.

## What I could not establish

- Whether the maintainer has an opinion on verb naming beyond what `docs/terminology.md`
  states generally — nothing in the tree names candidates for
  "scaffold"/"add app to Flockfile" verbs one way or the other; §7's
  candidate reasoning is inference from the existing rules, not a
  discovered decision.
- Whether JSON5's `json5` crate (0.4.1, in the lockfile) actually supports
  serialization well enough to round-trip comments back out — I confirmed
  it's declared and used for *parsing* only in this tree; I did not test its
  serialize path, since nothing in the codebase currently calls it.
- Whether an "add an app" verb should also handle the `.js` case at all
  (e.g. "append a `[[app]]`-equivalent entry to an `ecosystem.config.js`" is
  almost certainly out of scope given the deliberate node-execution
  boundary in §2, but I found nothing that rules it in or out explicitly —
  it's simply never discussed).
