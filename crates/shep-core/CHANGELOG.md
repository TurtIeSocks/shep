# Changelog

All notable changes to `shep-core` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against; entries below predate that and carry none.

## [Unreleased]

## [0.1.9] - 2026-08-28

### Fixed

- Never leave a connected pipe instance in the listener slot


## [0.1.8] - 2026-08-28


### Additions

- Add `Request::ConfigDrift`, `Response::Drifted`, `SheepDrift`, and
  `AppConfig::drifted_fields`. `ConfigDrift` asks which of a set of apps name
  a sheep the flock already has under a DIFFERENT config, and changes
  nothing. `Start` on an already-registered name adds instances rather than
  reconciling config, which `shep stock` depends on, so an operator who
  edited a Flockfile and re-ran `shep start` had the edit ignored with no
  error and no warning; this is what a caller asks in order to say so.
  `SheepDrift` carries the sheep's name and the names of the differing
  fields, never their values, because `AppConfig::env` holds secrets and the
  answer is printed at an operator (IR-41). `drifted_fields` compares through
  serde rather than field by field, so a field added to `AppConfig` is
  compared without a second edit, and sorts the result explicitly rather than
  leaning on `serde_json::Map` being a `BTreeMap`, which holds only while the
  additive `preserve_order` feature is off anywhere in the graph. Additive: `PROTOCOL_VERSION` stays **1**, a
  daemon that predates the request answers its existing "does not implement
  that request" error, and every previously pinned wire fixture is unchanged.

- Add `protocol::sort_flock`, the one ordering rule every operator-facing
  listing takes: by name, then by id. An id is assigned at registration, so
  ordering by it sorts the flock by an accident of history rather than by
  anything an operator is looking for, and it is not stable -- a `delete all`
  followed by a fresh start moved a real thirteen-app flock from ids 0-10 to
  11-21 with nothing about the apps having changed. The id keeps its other
  job: it is still how one instance of a clustered app is addressed. It stops
  being a sort key and stays an addressing key.

## [0.1.1] - 2026-08-26

### Additions

- Add `ExitInfo` and `ProcessInfo::last_exit` — why a sheep's process most
  recently stopped existing under this daemon: an exit code, a raw unix
  signal number, or `None` while it has never exited under this daemon (or
  the peer daemon predates the field). Answers the operator question a
  boot-looping sheep left unanswered before this: `flock`/`describe` could
  say a sheep was `errored` with a restart count, never why. `ExitInfo` is a
  small struct rather than two flat `Option<i32>` fields directly on
  `ProcessInfo`, so "never exited" and "exited by an unnamed signal" stay
  distinguishable — both flat fields would be `None` for either. `last_exit`
  is sticky across a respawn: it answers "why did it last stop", not "is it
  stopped right now" (`status`/`pid` already do that), so a sheep back
  `Online` after a crash keeps a true answer about the crash that restarted
  it until its next exit. Additive under `Option` on the same terms as every
  other field this struct has grown since Phase 3 — `PROTOCOL_VERSION` stays
  **1**, and a peer that predates the field neither sends nor expects the
  key.

## [0.1.0] - 2026-08-26

### Additions

- Add `DaemonConfig::load_layered` and `DaemonOverrides` — a third
  `file < SHEP_* env < flags` layer over the `file < env` `load` already did.
  `load` keeps its exact signature and semantics; `load_layered` validates
  exactly once, after all three layers are merged, so a good override can
  rescue a `shep.toml` whose own value is below the floor. `DaemonOverrides`
  is `#[non_exhaustive]` with a consuming-self builder, because it grows a
  field every time the hidden `daemon` subcommand grows a flag.
- Add `config::parse_daemon_bool` — the shared `1|0|true|false` grammar
  `SHEP_LOG_JSON` and `--log-json` both parse through, so the two never drift
  apart into different boolean spellings.
- Add a non-default `schema` feature: `JsonSchema` for the Flockfile
  document, `AppConfig`, `ProbeConfig`, `ProbeKind`, `MemSize`, `UpDuration`,
  and `config::flockfile_schema_json`, which renders the document schema as
  pretty-printed JSON. Off by default; `shep-cli` turns it on. The schema
  describes the deserializer, not the `normalize` step — `kill_signal` is an
  unconstrained string in the schema even though `normalize` narrows it to
  five signal names afterward.
- Accept and ignore `$schema` at the Flockfile top level — a JSON/JSON5
  editor's own convention for pointing at a schema file, read by
  `RawFlockfile` and discarded. Does not touch `AppConfig`, which is on the
  wire; `PROTOCOL_VERSION` is unaffected.
- Add `signals::OperatorSignal`, the nine signals `shep signal` may name;
  `Request::Signal` / `Response::Signalled` / `SignalReply` / `SignalOutcome`.
- Add `Request::Scale` / `Response::Scaled` — set an app's instance count,
  the stocking rate; the CLI verb is `shep stock` (alias `scale`).
- Add `AppConfig::stdin` (default `false`) — pipe a sheep's stdin so `shep
  whisper` (alias `sendline`) can reach it; `Request::SendLine` /
  `Response::SentLine` / `LineReply` / `LineOutcome`.
- Add `Lamb` and `ProcessInfo::lambs` — a sheep's process-tree members,
  populated by `Describe` only. `None` means the reply did not walk for them,
  never that there are none.
- Add `kv` — the file-locked key/value store at `$SHEP_HOME/kv.json` (spec
  §5), and `ShepPaths::kv`.
- Add `BusEvent::Channel` and the `channel.ready` / `channel.metric` /
  `channel.action_reply` topics (spec §6).
- Add `protocol::ProcessInfoBuilder`.
- Add `config::KillSignal`, the four-signal grammar `kill_signal` accepts.
- Add `barks` module: `Bark`, `SinkOutcome`, `append`, `read`, and
  `DEFAULT_MAX_BYTES` — the `barks.jsonl` ring both the bark dog (a rule
  fired) and the shepherd itself (an enabled dog gave up) append to, and
  `shep barks` will read. One JSON object per line, because a
  line-delimited format is the one shape where a writer dying mid-append
  costs the reader one record rather than the file. `append` keeps the
  ring under a byte cap by evicting whole lines oldest-first, rewriting
  the survivors plus the new record to a sibling temp file and `rename`ing
  it over the original — the same atomic-replace shape
  `shep-daemon::snapshot::write_atomic` uses, so an interrupted rewrite
  never leaves a truncated line behind. A single record bigger than the
  whole cap is written anyway, over cap, rather than dropped — the
  alternative silently loses the alert that was too interesting to fit.
  `read` is the forgiving half: a line that will not parse — a partial
  write from a dead writer, or a record from a future shep — costs that
  one record, not the whole history, because this file is read during an
  incident and refusing it over one bad line is the wrong failure mode.
  Lives in shep-core, not shep-daemon, because it has two writers in two
  different processes and neither is the other's crate; one implementation
  of the cap keeps them from evicting differently. `Bark`'s `Debug` is
  derived, not redacted: it carries a rule name, a subject and a message,
  all shep's own prose, and `SinkOutcome` names a sink by its
  `[dog.bark.sinks]` config key, never by its webhook URL or token — the
  file itself is still created owner-only (unix `0600`) as a matter of
  posture, not because it holds a secret today.

- Add `ProcessSelector::is_exact`, which answers whether a selector names one
  entry the caller already knew of — by its name or its id — rather than
  sweeping whatever matches. `Regex` and `Fold` are wildcards by this
  measure even when the pattern is a literal that can only ever match one
  name: what the split turns on is whether the operator named the thing, not
  how many things the selector reaches. The daemon uses it to keep a dog out
  of what `all` and a `/regex/` sweep touch while leaving `shep restart bark`
  able to reach it.

- Add `DaemonSection::adopted_dogs`, a `BTreeMap<String, PathBuf>` recording
  where each adopted dog's binary lives, keyed by dog name (`shep adopt`
  writes an entry; `shep rehome` removes it). A name in `enabled_dogs` with
  no entry here is a built-in dog — an argv branch of the shep binary
  itself — and that presence-or-absence is the whole of the distinction.
  The path deliberately lives in `[daemon.adopted_dogs]` rather than as a
  shep-owned key inside `[dog.<name>]`: that table is the dog's own opaque
  configuration, parsed by the dog and not by shep, and a shep-owned key
  planted inside it would collide with a third-party dog's own schema.
  `PathBuf`, not `String`: unlike `DogSource::Adopted`'s wire form, this
  value never crosses the wire — it is read straight off `shep.toml` and
  handed to a spawn, which is a path, not serialized text.
- Add `Request::DogConfig`, `Request::EnableDog`, `Request::DisableDog`, and
  `Response::DogSection`, `Response::DogStarted` — the wire verbs a dog and an
  operator use to fetch a dog's config, start one, and stop one. Additive
  under `#[non_exhaustive]` on the same terms as `Reopen` above —
  `PROTOCOL_VERSION` stays **1**, and an older daemon fails to decode the verb
  rather than answering it.

  `DogConfig` carries a `name: String`, not a `SelectorSpec`: a dog's name is
  a config key, selecting one `[dog.<name>]` table and one `enabled_dogs`
  entry, not a set of processes, so a selector here would invite `shep enable
  all`, which has no meaning. `DogSection` answers with the section rendered
  back to TOML text rather than a typed structure — a config value the daemon
  never has reason to parse for the dog, and a third-party dog stays bound to
  the shape of its own section rather than to shep's config model, file
  discovery, or layering rules. That text is wrapped in `DogSectionToml`, a
  `#[serde(transparent)]` newtype over `String` whose `Debug` prints a byte
  count instead of the section: a `[dog.<name>]` table is where a webhook
  credential lives, and keeping it off the process table is the whole reason
  the section travels over the socket rather than through the child's
  environment. A derived `Debug` would have handed it back on the first
  `tracing::debug!` of a reply. The newtype is transparent to serde, so the
  wire bytes are those of a bare string and the pinned fixtures are unchanged;
  it `Deref`s to `str` for reading. `EnableDog` carries the same `name` plus a
  `DogSource` naming where the dog's binary comes from, and answers
  `DogStarted(ProcessInfo)` — a bare `ProcessInfo`, not a `Vec`, the only
  `Response` variant shaped that way: enabling starts exactly one dog, and a
  one-element list would invite a reader to wonder when it holds two.
  `DisableDog` answers the existing `Response::Deleted(Vec<u32>)` rather than
  a variant of its own: disabling deregisters exactly as `Delete` does, so
  this is the same fact and not a coincidence of shape.
- Add `ProcessInfo::dog` and `DogSource`, marking which flock entries are
  dogs (the daemon's own supervised utility processes — metrics, bark) rather
  than sheep, and naming where a marked one came from: `BuiltIn` for an argv
  branch of the shep binary itself, `Adopted { path }` for an operator's own
  binary run at the daemon's own trust level. `DogSource` is
  `#[non_exhaustive]`, so a future source needs no protocol bump, and `path`
  is a `String` rather than a `PathBuf` for the reason `ProcessInfo::out_file`
  is one: serde's `PathBuf` impl refuses a non-UTF-8 path outright, aborting
  the whole reply rather than degrading the one odd field. Additive under
  `#[non_exhaustive]` and an `Option` field, so `PROTOCOL_VERSION` stays
  **1** on the same terms as `out_file`/`cpu_percent` before it: a daemon
  built before dogs existed sends no `dog` key at all, and a current client
  decodes that as `None`.

  `None` deliberately covers both "this entry is a sheep" and "this peer
  predates the field" without needing to tell them apart, unlike
  `cpu_percent`'s three enumerated cases — a stale `cpu_percent` reading
  would be a claim about resource use, and there is no equivalent claim a
  missing `dog` marker could get wrong: a daemon that predates dogs truly has
  none, so "not a dog" is the correct answer under either reading.

- Add `ProcessInfo::cpu_percent` and `ProcessInfo::memory_bytes`, a sheep's
  live resource use as the daemon read it while answering. CPU is a
  percentage of one core over the window since the daemon's last periodic
  sample, memory a resident set size in bytes; both are summed over the
  sheep's whole process tree, so a sheep that forks lambs is reported as what
  it actually costs the machine. Both are `Option`, and all three of the
  cases that make one `None` render as unknown rather than as zero: the sheep
  is not running, it has been up for less than one sampling window and so has
  no honest CPU figure yet, or the peer daemon predates the fields.
  `PROTOCOL_VERSION` stays 1 for the reason `out_file`/`err_file` left it at
  1 — the fields are additive, they decode to `None` from pre-existing bytes
  (pinned by a committed byte fixture), and a peer that predates them ignores
  them.

- Add `ProbeTarget`, parsing a probe's `target` once, at config time, into the
  form its kind promises: `http://host[:port][/path]` for `Http` (port
  defaults to 80, path to `/`), `host:port` for `Tcp`, and any non-empty
  command line for `Exec`. Both authority-bearing kinds share one split, so a
  bracketed IPv6 host such as `[::1]` is accepted on either and carried with
  its brackets stripped. `https://` targets are rejected with their own error
  variant — the HTTP prober is a hand-rolled client with no TLS, and a probe
  that silently failed every poll would look exactly like a down app.
  `normalize` now validates `readiness_probe` and `liveness_probe` targets
  (discarding the parsed value; the daemon re-parses when it arms the probe),
  rejects an explicit `failure_threshold == 0`, and rejects `watch = true`
  with no `cwd`. A zero threshold is unhealthy before the first poll ever
  runs. For `watch`, there is no directory to watch, and defaulting to the
  daemon's own cwd risks recursively watching the whole filesystem under a
  systemd unit with no `WorkingDirectory=`.
- Reject the zero-valued knobs that turn a loop into a hot spin, each with
  its own error variant naming the app: `liveness_probe.interval` below one
  second (`IntervalBelowMinimum`), `max_memory` of `0` (`ZeroMaxMemory`),
  and `watch_delay` of `0` (`ZeroWatchDelay`). A sub-second liveness
  interval was previously clamped in silence, which the neighbouring
  `max_cron_sleep` knob already refused to do on the grounds that a clamp
  announces itself only in a log file nobody reads. A zero `watch_delay`
  reaches the debouncer, whose tick is `delay / 4`, and pegs a core per
  watched app. A zero `max_memory` is a ceiling every process exceeds, so
  the sheep restarts every poll forever. A `readiness_probe.interval` below
  the floor is still accepted: that poll is bounded by `listen_timeout`, so
  it cannot spin.
- Add `CronSchedule`, validating `cron_restart` against croner's dialect and
  resolving `cron_timezone` against the IANA database, replacing the
  5-token-count stopgap. Accepts the seven vixie `@nickname` shorthands,
  expanded to five-field patterns before croner ever sees them: `@yearly`
  and `@annually` -> `0 0 1 1 *`, `@monthly` -> `0 0 1 * *`, `@weekly` ->
  `0 0 * * 0`, `@daily` and `@midnight` -> `0 0 * * *`, `@hourly` ->
  `0 * * * *`.
- Add the `[daemon] max_cron_sleep` key: the longest a cron worker sleeps
  before re-deriving its next occurrence, which bounds how far a cron restart
  can drift after a laptop suspend or an NTP step. Defaults to 60s when unset,
  and `SHEP_MAX_CRON_SLEEP` overrides the file value. Values below one second
  are **rejected** rather than clamped — below that the loop stops scheduling
  and starts spinning, and a clamp would announce itself only in a detached
  daemon's log file. Mind the duration grammar, which is `UpDuration`'s and
  counts **milliseconds** when no unit is given: `max_cron_sleep = "60"` is
  sixty milliseconds and fails the floor, while `"60s"` is the minute most
  people mean.
- Add wire protocol v1 types — `Request`, `Response`, `Envelope`, `Reply`,
  `RpcError`, `Hello`/`HelloAck`, `BusEvent` — with pinned insta snapshots of
  their serialized form.
- Add `Request::Reopen` and `Response::Reopened`, asking a daemon to reopen
  the log files of every matched sheep after an external rotator has renamed
  them. Both enums are `#[non_exhaustive]` and no existing variant changes,
  so `PROTOCOL_VERSION` stays **1**: the committed v1 byte fixtures still
  deserialize. A new *variant* buys no graceful answer from an older daemon,
  though, and this one does not either: `Request` is internally tagged
  (`#[serde(tag = "kind")]`) with no `#[serde(other)]` catch-all, so a daemon
  whose `Request` predates the variant fails to decode the frame at all and
  ends the connection. The `Internal` wildcard in the daemon's dispatch fires
  only where its own `shep-core` already knows the variant — which, for a
  shipped daemon, means it implements it too. (The additive-*field* reasoning
  under `ProcessInfo::out_file` below is sound and simply does not extend to
  variants.) `Reopened` carries `ProcessInfo`s like `Stopped` and `Restarted`
  do — every matched sheep, including any that was not running and so had
  nothing to reopen.
- Add `Request::Flush` and `Response::Flushed`, asking a daemon to empty the
  log files of every matched sheep. Additive under `#[non_exhaustive]` on the
  same terms as `Reopen` above — `PROTOCOL_VERSION` stays **1**, and an older
  daemon fails to decode the verb rather than answering it. `Flush`
  carries a `SelectorSpec` with no default anywhere in the stack — the verb
  destroys log data, so the operator names its target. `Flushed` carries one
  `ProcessInfo` per matched SHEEP, not one per file emptied: several sheep
  can share a log path (`merge_logs`, or an explicit `out_file` on a
  multi-instance app) and the daemon truncates each distinct path once, but
  the selector names sheep and so does the answer.
- Add `Request::Reload` and `Response::Reloading`, asking a daemon to replace
  each matched sheep with a fresh instance of the same app, one instance of an
  app at a time. Additive under `#[non_exhaustive]` on the same terms as
  `Reopen` above — `PROTOCOL_VERSION` stays **1**, and an older daemon fails
  to decode the verb rather than answering it. `Reload` carries a
  `SelectorSpec` with no default anywhere in the stack, matching
  `stop`/`restart`/`delete`: the verb replaces running processes, so the
  operator names its target.

  `Reloading` is named for what it is. It is an **acceptance**, and the only
  reply in the enum carrying a flock listing that names one rather than
  finished work — `ShuttingDown` is an acceptance too and carries nothing.
  The reason is timing: one instance costs a
  readiness wait plus a drain in the worst case, so a clustered app outlasts
  any deadline a client is allowed to ask for, and a reply that waited would
  time out while the reload it asked for went on running. It carries the
  matched sheep as they stood when the reload was accepted — including any
  with nothing to replace, which are the no-op successes they look like — and
  the swaps themselves report on the bus.
- Add `ProcessEventKind::Reload`, `Reloaded` and `ReloadAbandoned`
  (`process.reload`, `process.reloaded`, `process.reload_abandoned`), the
  three ways a reload reports itself. `Reload` names the instance being
  replaced, `Reloaded` the replacement once the instance it drained is gone,
  and `ReloadAbandoned` whichever instance the abandonment left holding the
  slot — the one the reload gave up on replacing, still the app's live one, or
  the replacement itself where that is what went down. **A subscriber built
  before these variants cannot
  decode the frames and drops them**, and unlike a new `Request` variant that
  is not something it opted into: every topic here is `process.<something>`,
  which the `process.*` glob already matches. Accepted rather than worked
  around, because a reload's reply is an acceptance and the bus is therefore
  the only place its outcome is reported at all; reusing `Start`/`Stop` and
  leaving subscribers to infer a reload from a doubled name would make the
  outcome unreadable to a new client as well as an old one.
- Add `Request::Trigger` and `Response::Triggered`, asking a daemon to send a
  named action to every matched sheep over its shepherd channel and report
  what each app says back (`shep trigger <target> <action> [params]`).
  Additive under `#[non_exhaustive]` on the same terms as `Reopen` above —
  `PROTOCOL_VERSION` stays **1**, and an older daemon fails to decode the
  verb rather than answering it. `Trigger` carries a `SelectorSpec` with no
  default anywhere in the stack, matching `stop`/`restart`/`reload`/`delete`/
  `flush`. `action` is a free-form `String` the daemon never parses or
  validates, and `params` an `Option<String>` matching the shepherd channel's
  own `action` message.

  `Triggered` carries `Vec<ActionReply>`, not `Vec<ProcessInfo>`: a reply
  body has nowhere to live on `ProcessInfo`, and `EmptiedFile`
  (`shep-cli`'s own non-`ProcessInfo` row) is the precedent for a row built
  for what one verb needs rather than reused from the flock-listing shape.
  Each `ActionReply` carries the sheep's id and name plus a new
  `#[non_exhaustive]` `ActionOutcome`: `Replied { body }` when the app
  answered, `NoChannel` when the daemon had no shepherd channel to deliver
  over, `Skipped` for a reload drainee, or `TimedOut` when nothing came back
  before the app's action timeout. Per-row rather than a whole-request
  refusal, matching `Reopen`/`Flush`'s own precedent: spec §9's selector
  grammar (`all`, `/regex/`, `fold:`) makes a mixed flock the normal case,
  and a channel-less sheep in that mix should not cost every other sheep its
  answer.
- Add `Request::Muster` and `Response::Mustered`, asking a daemon to assemble
  the flock from the muster roll on disk (`shep muster`). Additive under
  `#[non_exhaustive]` on the same terms as `SaveRoll` below — `PROTOCOL_VERSION`
  stays **1**, and an older daemon fails to decode the verb rather than
  answering it. `Muster` carries no fields for the reason `SaveRoll` carries
  none: the roll describes a whole flock, so there is nothing to select.
  `Mustered` carries the same `Vec<ProcessInfo>` `Flock` does, and reports
  every sheep of every app the roll restored rather than only the ones that
  call spawned — assembling a flock that is already assembled starts nothing,
  and an empty listing there would be indistinguishable from an empty roll,
  which is the one outcome the reply exists to tell apart.
- Add `Request::SaveRoll` and `Response::RollSaved`, asking a daemon to write
  the muster roll now, bypassing the snapshot writer's debounce
  (`shep save`). Additive under `#[non_exhaustive]` on the same terms as
  `Reopen` above — `PROTOCOL_VERSION` stays **1**, and an older daemon fails
  to decode the verb rather than answering it. `SaveRoll` carries no fields:
  unlike `Stop`/`Restart`/`Trigger` and the rest, there is no flock to
  select — the roll always records the whole flock. `RollSaved` is the only
  struct-shaped `Response` variant, carrying the roll's absolute path (a
  `String`, not a `PathBuf`, for the reason `ProcessInfo::out_file`'s own
  comment gives) and how many apps it recorded (`apps: u32`, matching
  `SavedApp::instances_running`'s width in shep-daemon's `snapshot.rs`).
- Add `MemSize` and `UpDuration` config value newtypes, parsing the strict
  Flockfile grammars `^\d+(G|M|K)?$` and `^\d+(h|m|s)?$`.
- Add `ProcStatus` with stable wire strings for the process lifecycle states.
- Add `ShepPaths` to resolve the `$SHEP_HOME` runtime directory layout.
- Add `AppConfig` with the v1 Flockfile field set, plus `normalize` and
  `normalize_all`, which produce a proof-token `ResolvedApp`.
- Add `AppConfig::channel`, opening the shepherd channel on fd 3 for an app
  on its own rather than only as a side effect of `wait_ready` or
  `shutdown_with_message`. Defaults to `false`: a socketpair plus two pump
  tasks per sheep is real cost against spec §14.11's single-digit-MB
  idle-RSS goal, so the channel now opens only when something asks for one.
  `wait_ready` and `shutdown_with_message` keep opening it on their own,
  unaffected by the new field.
- Add `AppConfig::action_timeout`, how long a triggered action gets to answer
  before its row becomes `ActionOutcome::TimedOut`. Defaults to 3s, replacing
  the flock-wide constant the daemon used before this field existed, with the
  same value and the same reasoning: comfortably under the 5s an RPC caller
  gets by default, so the honest `TimedOut` row still reaches a caller who set
  no deadline of its own. `normalize` rejects a value at or above 58s — 2s
  under the daemon's own hard ceiling on any deadline a caller could ever be
  given, `MAX_DEADLINE_MS` — because past that line no caller, however long a
  deadline it asks for, could ever be given room to wait it out; a value
  merely above the 5s *default* is accepted, on the understanding that
  satisfying it is the caller's to arrange with a wider deadline of its own
  (`Client::request_with_deadline`, the way `shep logs -f` already asks for
  `LOG_PLANE_DEADLINE` rather than the client's default).
- Add `Flockfile` discovery (`discover`) and TOML/YAML/JSON/JSON5 parsing.
- Add `DaemonConfig`, parsing `shep.toml` with `SHEP_*` env-variable
  layering.
- Add `ProcessSelector` parsing and matching (`all`, id, name, `/regex/`,
  `fold:<name>`).
- Add the length-delimited JSON wire codec (`codec`, `encode_frame`,
  `decode_frame`) with a 16 MiB frame cap (`MAX_FRAME_BYTES`).
- Add `SelectorSpec` <-> `ProcessSelector` wire bridges (`TryFrom`/`From`) so
  selectors travel over RPC.
- Add `ProcessInfo::out_file` and `ProcessInfo::err_file`, the daemon's
  resolved log paths for a sheep. Readers can no longer derive these: an
  explicit `out_file`/`err_file` in an app's config may point anywhere, so
  guessing the `logs/<name>-<instance>-out.log` convention silently finds
  nothing for such a sheep. Both are `Option<String>` — a string because
  every other path on this wire is one and because serde refuses a non-UTF-8
  `PathBuf` outright, failing the whole reply rather than one field; optional
  because this addition does not bump `PROTOCOL_VERSION`, so a daemon
  predating the fields still connects and sends replies without them, where
  `None` means "peer too old to say", never "this sheep has no log file".
  `PROTOCOL_VERSION` stays 1: the fields are additive, they decode to `None`
  from pre-existing bytes (pinned by a committed byte fixture), and a peer
  that predates them ignores them.
- Add `[daemon] log_level`, overridable by `SHEP_LOG_LEVEL`, deciding how
  much of the daemon's own diagnostics is rendered. Its type is the new
  `LogLevel` — `off`, `error`, `warn`, `info`, `debug`, `trace`, lowercase
  and nothing else — and the default is `warn`, which is where the daemon's
  warn-and-continue arms live: a watch it could not arm, a cron pattern it
  could not parse, a memory ceiling a process tree crossed. `debug` fires
  per dropped restart and per child metric sample, so it is a firehose on a
  busy flock rather than a slightly noisier log. A name outside the six is a
  startup error (`DaemonConfigError::Toml` from the file,
  `DaemonConfigError::BadEnvValue` from the environment), never a silent
  fallback to the default. `DaemonSection` gains the field, so a struct
  literal naming every field must name this one too; `..Default::default()`
  is unaffected.
- `protocol::channel` — the shepherd channel's `ChildMessage`/`ShepherdMessage`
  shapes, moved here from shep-daemon so a bus event can carry one.
- Add `config::WhistleSection` and `DaemonConfig::whistle` — the `[whistle]`
  section, one key (`allow_control`, default `false`) gating whether `shep
  whistle`'s four control tools are offered. Lives in `shep.toml` and
  nowhere else: no flag, no `SHEP_*` variable, because the config file is
  what an operator can audit and `shep whistle`'s launcher writes its own
  argv. `Debug` is derived, not redacted (IR-41) — one boolean, no secret.

### Fixes

- `normalize` refuses a `kill_signal` shep cannot send instead of leaving the
  daemon to substitute SIGTERM at every stop.
- Give the workspace's path dependencies (`shep-core`, `shep-daemon`,
  `shep-client`) a version alongside their `path`, which `cargo publish`
  requires — it strips `path` from a dependency at publish time and refuses
  to publish anything left with no version to put there. One cosmetic side
  effect for this crate specifically: its `[target.'cfg(any())'.dependencies]`
  floor-pin block (see that block's own comment) publishes as real manifest
  entries, so crates.io and docs.rs list all six of `annotate-snippets`,
  `anstyle`, `encoding_rs_io`, `pest`, `quote` and `syn` as dependencies of
  `shep-core`, even though `cfg(any())` never matches and not one of them
  ever builds into it. `shep-daemon` and `shep-cli` publish the same way, for
  the two and the one floor pin their own blocks carry.
- Reject a `watch_options` or `ignore_watch` pattern globset will not compile,
  with `NormalizeError::InvalidWatchGlob`, naming the sheep, which of the two
  lists the pattern came from, the pattern as written and globset's reason.
  `normalize` compiles every pattern through globset — the engine the daemon's
  own watch filter uses — and checks both lists whether or not `watch` is on.
  Previously a pattern such as `"["` was accepted, the sheep came up `online`,
  and the watch it configured simply did not exist.
- Reject JSON5 documents nested past depth 64 instead of stack-overflowing
  (an uncatchable `SIGABRT`) on deeply nested or malicious `Flockfile` input.
- Make the JSON5 depth guard comment-aware: `//` and `/* */` comments
  containing a quote character no longer desync the guard's string-tracking
  and disable it for the rest of the document.
- Stop `DaemonConfig`'s `Debug` implementation from leaking `[dog.*]` table
  values (e.g. webhook URLs); it now prints only the table count.

### Changes

- `config::DaemonConfig` is `#[non_exhaustive]`. Outside shep-core it can no
  longer be built with a struct literal or `..Default::default()`. **A
  breaking change for any out-of-tree struct-literal construction.** It is
  for field growth — `DaemonConfig` gains a section roughly every phase — not
  for validation: the type is not a proof token and does not become one,
  fields stay `pub`, and holding one does not prove it was validated. The
  escape hatch if that ever needs to change is a public `validate`, not
  private fields.
- `protocol::ProcessInfo` is `#[non_exhaustive]` — construct it with
  `ProcessInfo::builder`. Fields remain `pub`; reading and assigning are
  unchanged. A future field is no longer a breaking change for downstream
  crates.
- `ProcessInfo` no longer derives `Eq`. `cpu_percent` is an `f32` and floats
  are only partially ordered, so the derive could not survive the field.
  `PartialEq` stays, which is everything `assert_eq!` and a `==` comparison
  need; what stops compiling downstream is a `HashSet<ProcessInfo>`, a
  `BTreeSet` of them, or a type that derives `Eq` and holds one. Nothing in
  this workspace does any of the three.

- `cron_restart` validation moves in both directions. Tighter: patterns the
  stopgap accepted purely on token count (e.g. `99 99 99 99 99`) now fail
  with croner's own reason, and croner's `L`, `W`, `#` and `?` extensions are
  newly rejected with the offending character named — six-field and
  seconds-bearing patterns were already rejected by the token count and stay
  rejected, but the error now says why instead of "not a 5-field pattern",
  and `@reboot` is rejected with a message about what it means rather than
  about field counts. Looser: `0 0 * JUL WED` and `0 0 * * MON-FRI` keep
  working, and the seven vixie nicknames above are now accepted where the
  token-count stopgap rejected them. `NormalizeError::InvalidCron` becomes a
  struct variant (`{ pattern, reason }`) instead of a bare tuple, and gains a
  sibling `InvalidTimezone { name }` for a `cron_timezone` that is not an
  IANA zone — validated even when `cron_restart` is absent.
- `DaemonConfigError` gains a `BelowMinimum { key, value, min }` variant, for
  the `max_cron_sleep` floor above. This is filed as a change rather than an
  addition because the enum carries no `#[non_exhaustive]`: any downstream
  `match` over it stops compiling until it handles the new variant.
- `encode_frame` now returns `Bytes` instead of `BytesMut`: it takes
  ownership of the serialized `Vec`'s buffer instead of copying it.
- Swap the YAML backend from `serde_yml` to `serde-saphyr` (pure-Rust
  parser), keeping the crate's `forbid(unsafe_code)` guarantee.
- Rename `ConfigError` -> `NormalizeError`, matching the per-construction-site
  naming convention used by the crate's other error enums.
- Rewrite `AppConfig::reuse_port`'s doc. It previously read "Bind listen
  sockets with SO_REUSEPORT", first person, as though shep does the
  binding — it doesn't. The child binds after `exec`, and a socket option
  has to be set before `bind()` by the process that binds, so `reuse_port
  = true` has only ever meant the operator asserting that the app sets the
  option itself (Node ≥22's `reusePort`, Go's `Control` hook, nginx's
  `reuseport`); shep's contribution is permission for the old and new
  instance to overlap during reload, not the mechanism. The doc now also
  names the failure mode: an app that does not set the option gets
  `EADDRINUSE` at the replacement spawn on every reload, undetectable in
  advance, and `SO_REUSEADDR` — which far more frameworks set by default —
  is not sufficient. No behavior changed; the field's meaning was always
  this, only the doc claimed otherwise.
- Wire fixtures now pin every `SelectorSpec` variant, every
  `ProcessEventKind`, and every `Response` tag. No wire string changed —
  this closes coverage, not behaviour.
- `FlockfileError`, `DaemonConfigError` and `BarkError` are `#[non_exhaustive]`.
  Match on them with a wildcard arm.
- `SelectorError`, `ParseMemSizeError`, `ParseUpDurationError` and `WireError`
  are `#[non_exhaustive]`. Match on them with a wildcard arm.
