# Changelog

All notable changes to `shep-core` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against; entries below predate that and carry none.

## [Unreleased]

### Additions

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

### Fixes

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
