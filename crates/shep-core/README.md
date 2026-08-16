# shep-core

The shared tier of [shep](https://github.com/TurtIeSocks/shep), a process
manager written in Rust. One binary runs a daemon called the shepherd, which
keeps a flock of your long-running processes alive.

This crate holds what the other three agree on:

- `config`: the Flockfile reader. TOML, YAML, JSON and JSON5, discovered by
  searching ten filenames in a fixed order. Unknown fields are a parse error
  rather than a shrug. Durations and sizes have a strict grammar, so `512M`
  and `30s` parse while `512MB` and `1.5G` do not.
- `protocol`: the request and response types the CLI and the daemon exchange,
  plus the length-delimited codec that frames them and the bus events a
  subscriber receives.
- `barks`: the append-only alert ledger at `$SHEP_HOME/barks.jsonl`, written
  under a file lock and capped by evicting whole lines oldest first.
- `kv`: the flat key/value store at `$SHEP_HOME/kv.json`, behind the same lock
  discipline.

It has no process control and opens no sockets. Supervision lives in
`shep-daemon`, talking to it lives in `shep-client`, and the `shep` binary
itself is built by the crate of the same name.

shep is pre-release. Anything public here can change before 1.0, and the
crate's [CHANGELOG](CHANGELOG.md) records what moved.

## License

MIT OR Apache-2.0, at your option.
