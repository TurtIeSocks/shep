# shep-channel

Speak the shepherd channel for [shep](https://github.com/shep-pm/shep), a
process manager written in Rust: signal readiness, emit a metric, answer a
custom action, over the newline-JSON descriptor shep hands a supervised
process on fd 3. `docs/shepherd-channel.md` in the shep repository is the
contract this crate implements.

This crate currently holds the wire's message shapes only —
`ChildMessage`, `ShepherdMessage`, and `CHANNEL_VERSION` — re-exported by
`shep-core` so a `BusEvent::Channel` and an app writing on fd 3 agree on one
type. `shep-core` depends on this crate with `default-features = false` for
exactly those types and nothing else.

The `client` feature, on by default, is where discovering the descriptor,
framing the JSON, and answering messages an app does not handle itself will
live. An app that wants to speak this wire from Rust depends on this crate
with default features; `shep-core` is the one consumer that turns them off.

shep is pre-release. Anything public here can change before 1.0.

## License

MIT OR Apache-2.0, at your option.
