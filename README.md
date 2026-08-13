# shep 🐑

A process manager written in Rust. One binary runs a daemon called **the
shepherd**, which keeps a **flock** of your long-running processes alive.
It restarts them when they die, captures what they print, and says plainly
when something is wrong.

The vocabulary is not decoration. A process manager gives you a lot of nouns
to keep straight, and sheep are a better mnemonic than "target group" or
"managed unit". Where the joke would cost clarity it gets dropped: `kill` is
called `kill`, error messages are written in plain technical English, and
every themed verb has a straight alias that works forever.

> **Status: pre-release, and honest about it.** shep is not on crates.io, has
> no tagged release, and has no install script. You build it from source. It
> runs on macOS and Linux. On Windows every command prints `shep does not yet
> support Windows` and exits 1, which is a real answer but not a useful one.
> The [roadmap](#whats-not-built-yet) below says what is missing.

## Try it

```bash
git clone https://github.com/TurtIeSocks/shep.git
cd shep
cargo build --release
./target/release/shep --help
```

Write a `Flockfile.toml`. Two fields is a complete one:

```toml
[[app]]
name = "web"
script = "./server"
```

Then start it. You never launch the daemon yourself: `shep start` notices
nothing is listening and re-execs itself in the background.

```bash
shep start Flockfile.toml
shep ls
```

```
ID  NAME    STATUS  PID   RESTARTS  CPU    MEM    UPTIME  FOLD
1   web     online  1001  1         12.5%  48.1M  1m      backend
2   worker  online  1002  2         12.5%  48.1M  2m      backend
3   cron    online  1003  3         12.5%  48.1M  30s     backend
```

`shep bleats` follows the logs. `shep bleats --no-follow` prints the tail and
exits. If you prefer the boring word, `shep logs` is the same command and
always will be.

Everything renders as `--format json` too, under a versioned envelope, so you
can pipe it somewhere without scraping columns:

```json
{
  "schema_version": 1,
  "command": "flock",
  "data": [
    { "id": 1, "name": "web", "status": "online", "pid": 1001, "restarts": 1,
      "uptime_ms": 60000, "fold": "backend", "cpu_percent": 12.5,
      "memory_bytes": 50462720, "dog": null,
      "out_file": "/logs/web-0-out.log", "err_file": "/logs/web-0-err.log" }
  ]
}
```

## The lexicon

The whole vocabulary, and whether it exists yet.

| shep says | Means | Where you meet it | Built? |
|---|---|---|---|
| the shepherd | the daemon | log messages, docs | yes |
| the flock | every managed process, as a set | `shep flock` (aliases `list`, `ls`) | yes |
| a sheep | one managed process (singular only) | `shep describe <name>` | yes |
| a fold | a namespace or group | `shep fold backend`, `fold =` in config | yes |
| Flockfile | the app config file | `Flockfile.toml` / `.yaml` / `.json` / `.json5` | yes |
| bleats | logs | `shep bleats` (alias `logs`) | yes |
| muster | bring a saved flock back | `shep save`, then `shep muster` | yes |
| the shepherd channel | a private pipe on fd 3 between daemon and app | `channel = true`, `shep trigger` | yes |
| a lamb | a child process of a sheep | tree-kill today, `describe` tree view later | partly |
| a dog | a plugin process the shepherd supervises | `shep enable metrics`, `shep dogs` | yes |
| a bark | a webhook alert | `[dog.bark.sinks]` config, `shep barks` | yes |
| the whistle | the MCP interface agents talk to | `shep whistle` | no |
| the lookout | the terminal dashboard | `shep lookout` (alias `dash`) | no |
| adopt / rehome | register or drop a third-party dog | `shep adopt <name> <path>` | yes |
| that'll do | graceful stop, after the real herding command | `shep thatlldo` | yes |

Sheepdogs and sheep were separate ideas from the start, so "dog" never means
the daemon. The shepherd is the shepherd. Dogs are plugins that work for it.

## What works today

**Supervision.** N instances per app, restart policies with exponential
backoff and a restart budget, `min_uptime` so a crash loop is not mistaken for
a healthy start, graceful stop escalating to `SIGKILL` on a timeout you set,
and process-group tree kill so a sheep's lambs go with her.

**Config.** `Flockfile.toml`, `.yaml`, `.json`, or `.json5`, discovered by
searching ten filenames in a fixed order. Unknown fields are a parse error
rather than a shrug, so a typo tells you at load instead of at 3am. Durations
and sizes have a deliberately strict grammar: `512M` and `30s` parse, `512MB`
and `1.5G` and `30S` do not.

**Restarts you didn't ask for by hand.** File watching with ignore globs,
cron schedules (five-field, plus `@daily` and friends), and a memory ceiling
that restarts a sheep when her process tree crosses it.

**Logs.** Per-instance stdout and stderr files, follow and tail, `flush` to
empty them, and `reopen` for when an external rotator has renamed the files
underneath you.

**Reload.** `shep reload` replaces instances one at a time. It is not
zero-downtime and the command's own `--help` says so: shep binds no sockets,
so an overlap only works if your app sets `SO_REUSEPORT` itself. Measured on
Linux, an app that drains its listener loses nothing, and an app that does not
drops a handful of in-flight connections when the old listener closes.

**Custom actions.** Set `channel = true` and your app gets a pipe on fd 3.
`shep trigger web reload-config` sends a named action down it and prints what
each instance answered, or says exactly why it could not answer:

```
ID  NAME  OUTCOME     DETAIL
1   web   replied     reloaded 4 routes
2   api   no_channel  no shepherd channel — set channel = true, or wait_ready / …
```

**Surviving reboots.** `shep save` writes a muster roll. `shep startup`
installs a systemd unit (`Type=notify`) or a launchd plist that runs
`shep muster` at boot. It never escalates its own privileges: without root it
prints the exact command you should run and exits non-zero.

**Dogs.** `shep enable metrics` turns on a Prometheus endpoint at
`127.0.0.1:9615`; `shep enable bark` watches the flock and posts alerts to
Discord, Slack, or a JSON endpoint you name under `[dog.bark.sinks]`. Both
ship inside the binary. `shep adopt <name> <path>` runs anyone else's
binary the same way — vetted once when you adopt it, and served its own
`[dog.<name>]` config over the same socket `shep` itself talks to rather
than through its environment, so a webhook credential never ends up in a
process listing or a crash dump. [docs/dogs.md](docs/dogs.md) is the guide.

**Coming from pm2.** `shep import` reads a real `dump.pm2` and writes a
Flockfile. It starts nothing, names every clustered app on stderr because
cluster mode does not survive the trip unchanged, and refuses to silently
swallow env keys it cannot place. [docs/migration.md](docs/migration.md) is
the walkthrough.

## What's not built yet

Not started: the lookout TUI, the whistle MCP server, `shep serve`,
`shep dev` and `shep runtime`, `scale` / `signal` / `sendline`, the
key-value store, `.js` Flockfiles, openrc and BSD `rc.d` units, and lambs
in `describe`'s output.

Windows is last, and it is genuinely zero rather than partial. The crate
compiles and its unit tests pass there in CI, which is not the same thing as
working.

[docs/specs/deferred.md](docs/specs/deferred.md) is the full list, including
the six things deliberately held back past 1.0.

## How it is built

**Clean-room.** shep takes pm2's feature list as a target and nothing else.
pm2's source was read exactly once, by a dedicated tracing phase whose only
output was a set of behavior specs. Everything since has been written from
those specs, and the rule that implementation never opens that source is
written into the repo's own contributor instructions. The bugs the trace found
are recorded too, so that nobody reimplements them by accident.

**One binary.** There is no `shepd`. Daemonizing means the `shep` binary
re-execs itself with a hidden subcommand, detaches, and reports readiness back
over a pipe once its socket is bound. Four crates build it: `shep-core`
(types, config, wire protocol), `shep-daemon` (the supervision engine),
`shep-client` (async client), and `shep-cli` (the binary).

**Tested by trying to break it.** 1030 tests, and every task ends with a
mutation pass: break a line on purpose, confirm a test goes red, put the line
back. It keeps turning up tests that could not fail, which is the reason to
do it.

**Small things done on purpose.** The CPU column prints `-` rather than `0.0%`
when a reading is unavailable, because a confident zero is worse than an
obvious blank. Three of the four crates are `#![forbid(unsafe_code)]` and the
daemon is `#![deny(...)]`, which leaves exactly one file in the workspace
containing an `unsafe` block. Anything holding an environment or a secret gets
a redacted `Debug`, with a test pinning the exact string it prints.

## Docs

- [docs/terminology.md](docs/terminology.md): the lexicon and the rules for using it
- [docs/migration.md](docs/migration.md): coming from pm2
- [docs/shepherd-channel.md](docs/shepherd-channel.md): the fd-3 protocol, for app authors
- [docs/dogs.md](docs/dogs.md): the metrics and bark dogs, and writing your own
- [docs/specs/shep-v1.md](docs/specs/shep-v1.md): the behavior contract
- [docs/specs/deferred.md](docs/specs/deferred.md): what is not built, and the order it lands in
- [docs/idiomatic-rust.md](docs/idiomatic-rust.md): the 45 house style rules

## Building

Rust 1.88 or newer, edition 2024.

```bash
cargo build --release
cargo test --workspace --all-features
```

## License

MIT OR Apache-2.0, at your option.
