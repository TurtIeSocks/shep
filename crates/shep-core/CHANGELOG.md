# Changelog

All notable changes to `shep-core` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against; entries below predate that and carry none.

## [Unreleased]

### Additions

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

### Fixes

- Reject JSON5 documents nested past depth 64 instead of stack-overflowing
  (an uncatchable `SIGABRT`) on deeply nested or malicious `Flockfile` input.
- Make the JSON5 depth guard comment-aware: `//` and `/* */` comments
  containing a quote character no longer desync the guard's string-tracking
  and disable it for the rest of the document.
- Stop `DaemonConfig`'s `Debug` implementation from leaking `[dog.*]` table
  values (e.g. webhook URLs); it now prints only the table count.

### Changes

- `encode_frame` now returns `Bytes` instead of `BytesMut`: it takes
  ownership of the serialized `Vec`'s buffer instead of copying it.
- Swap the YAML backend from `serde_yml` to `serde-saphyr` (pure-Rust
  parser), keeping the crate's `forbid(unsafe_code)` guarantee.
- Rename `ConfigError` -> `NormalizeError`, matching the per-construction-site
  naming convention used by the crate's other error enums.
