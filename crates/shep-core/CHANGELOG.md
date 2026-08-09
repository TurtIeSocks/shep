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
- Add `MemSize` and `UpDuration` config value newtypes, parsing the strict
  Flockfile grammars `^\d+(G|M|K)?$` and `^\d+(h|m|s)?$`.
- Add `ProcStatus` with stable wire strings for the process lifecycle states.
- Add `ShepPaths` to resolve the `$SHEP_HOME` runtime directory layout.
- Add `AppConfig` with the v1 Flockfile field set, plus `normalize` and
  `normalize_all`, which produce a proof-token `ResolvedApp`.
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

### Fixes

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
