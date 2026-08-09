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
  defaults to 80, path to `/`; a bracketed IPv6 host such as `[::1]` is
  accepted with its brackets stripped), `host:port` for `Tcp`, and any
  non-empty command line for `Exec`. `https://` targets are rejected with
  their own error variant — the HTTP prober is a hand-rolled client with no
  TLS, and a probe that silently failed every poll would look exactly like a
  down app. `normalize` now validates `readiness_probe` and `liveness_probe`
  targets (discarding the parsed value; the daemon re-parses when it arms
  the probe), rejects an explicit `failure_threshold == 0` on either probe,
  and rejects `watch = true` with no `cwd` — there is no directory to watch,
  and defaulting to the daemon's own cwd risks recursively watching the
  whole filesystem under a systemd unit with no `WorkingDirectory=`.
- `normalize` now compiles every `watch_options` and `ignore_watch` pattern
  through globset — the engine the daemon's own watch filter uses — and
  rejects one it will not compile with `NormalizeError::InvalidWatchGlob`,
  naming the sheep, which of the two lists the pattern came from, the pattern
  as written and globset's reason. Both lists are checked whether or not
  `watch` is on. Previously a pattern such as `"["` was accepted, the sheep
  came up `online`, and the watch it configured simply did not exist.
- Add `CronSchedule`, validating `cron_restart` against croner's dialect and
  resolving `cron_timezone` against the IANA database, replacing the
  5-token-count stopgap. Accepts the seven vixie `@nickname` shorthands,
  expanded to five-field patterns before croner ever sees them: `@yearly`
  and `@annually` -> `0 0 1 1 *`, `@monthly` -> `0 0 1 * *`, `@weekly` ->
  `0 0 * * 0`, `@daily` and `@midnight` -> `0 0 * * *`, `@hourly` ->
  `0 * * * *`.
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
- `encode_frame` now returns `Bytes` instead of `BytesMut`: it takes
  ownership of the serialized `Vec`'s buffer instead of copying it.
- Swap the YAML backend from `serde_yml` to `serde-saphyr` (pure-Rust
  parser), keeping the crate's `forbid(unsafe_code)` guarantee.
- Rename `ConfigError` -> `NormalizeError`, matching the per-construction-site
  naming convention used by the crate's other error enums.
