# shep-channel

Speak the shep shepherd channel from a supervised app: signal readiness,
emit a metric, answer an action.

```rust
let shepherd = shep_channel::serve();

shepherd.on_action("gc", |params, _name| {
    format!("collected, params={params:?}")
});
shepherd.on_shutdown(|| server.graceful_stop());
shepherd.ready()?;
shepherd.metric("rps", 4200.0);
```

Ask for a channel in the Flockfile with `channel = true`, or get one from
`wait_ready` or `shutdown_with_message`.

Without one, every call above does nothing and the app runs unchanged, so
there is nothing to branch on. An action name you did not register still
gets a reply, which is the part an app is most likely to get wrong: to the
shepherd, silence from an app thinking hard and silence from an app that
never understood the question look the same, and only `action_timeout`
running out tells them apart.

The wire itself is documented in `docs/shepherd-channel.md` in the shep
repository. It is language agnostic, and there are Go, JavaScript and
Python libraries over the same contract.

shep is pre-release. Anything public here can change before 1.0.

## License

MIT OR Apache-2.0, at your option.
