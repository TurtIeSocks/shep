# shep-daemon

The supervision engine of [shep](https://github.com/TurtIeSocks/shep), a
process manager written in Rust. This crate is the shepherd: the part that
spawns your processes, watches them, and restarts them when they die.

What it does:

- Runs N instances per app with restart policies, exponential backoff, a
  restart budget, and a `min_uptime` so a crash loop is not mistaken for a
  healthy start.
- Stops gracefully and escalates to `SIGKILL` on a timeout you set, killing
  the whole process group so a sheep's lambs go with her.
- Captures stdout and stderr per instance, with flush and reopen for when an
  external rotator has moved the files underneath you.
- Restarts on file changes, on a cron schedule, and when a process tree
  crosses a memory ceiling.
- Serves the RPC protocol over a unix socket, checks peer credentials, and
  publishes an event bus that clients subscribe to by topic glob.
- Supervises dogs, the plugin processes that answer for metrics and webhook
  alerts.

The pure tier compiles everywhere. Signal delivery, file-descriptor passing
and the socket server are unix only, so on Windows this crate builds but
supervises nothing.

You probably want the `shep` binary rather than this crate directly. It is
published as its own crate so that the parts stay separable and so that the
supervision engine can be embedded by something that is not our CLI.

shep is pre-release, and anything public here can change before 1.0. The
[CHANGELOG](CHANGELOG.md) records what moved.

## License

MIT OR Apache-2.0, at your option.
