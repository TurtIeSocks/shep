# shep 🐑

[![Crates.io Version](https://img.shields.io/crates/v/shep.svg)](https://crates.io/crates/shep)
[![docs.rs](https://img.shields.io/docsrs/shep)](https://docs.rs/shep)
[![License](https://img.shields.io/crates/l/shep.svg)](https://github.com/shep-pm/shep#license)
[![MSRV](https://img.shields.io/crates/msrv/shep.svg)](https://crates.io/crates/shep)
[![CI](https://github.com/shep-pm/shep/actions/workflows/test.yml/badge.svg)](https://github.com/shep-pm/shep/actions/workflows/test.yml)

A process manager written in Rust. One binary runs a daemon called the
shepherd, which keeps a flock of your long-running processes alive, restarts
them when they die, captures what they print, and says plainly when something
is wrong.

![shep start, stop and restart, each printing the whole flock](assets/hero.svg)

Every command answers with the whole flock, not just the sheep you touched.
The face in the STATUS column is the fastest thing on the page to read:
`(o.o)` online, `(o~o)` starting, `(>_<)` waiting to restart, `(-.-)` stopped,
`(x.x)` errored.

> Status: `0.1.24`, and pre-1.0 means anything can still change. macOS, Linux
> and Windows. The Windows tier is the newest of the three, and the three
> things it will not do are under [Windows](#windows) below.

## Install

```bash
cargo install shep
shep welcome
```

Homebrew, on macOS and Linux:

```bash
brew install shep-pm/shep/shep
```

Scoop, on Windows:

```powershell
scoop bucket add shep https://github.com/shep-pm/scoop-shep
scoop install shep
```

Or take a prebuilt archive off any
[release](https://github.com/shep-pm/shep/releases). Seven targets, each with
its own `.sha256` beside it and all of them in one `SHA256SUMS`.

Notes worth having before you pick:

- The Homebrew formula compiles the tree, so the first install is minutes
  rather than seconds. Scoop takes the published binary.
- The formula ships no `brew services` definition. `shep startup` installs
  shep's own launchd job, and running both would leave two pointed at one
  shepherd.
- On Windows `shep.exe` needs the Visual C++ redistributable, which Windows
  does not carry. If it will not start, `scoop bucket add extras` and then
  `scoop install extras/vcredist2022`. The bucket has to be added first, since
  a fresh Scoop has only `main`.
- Every channel installs all three binaries, `shep`, `shep-runtime` and
  `shep-dev`, but only `shep` is put on your PATH by the Windows packages.

## Coming from pm2

shep is a clean-room reimplementation of pm2's feature list, and `shep import`
turns whatever `pm2 save` last wrote into a Flockfile. It reads `--from`, or
`~/.pm2/dump.pm2`, and starts nothing.

The difference worth switching for is that shep tells you the truth about what
it did. `shep reload` does not claim zero-downtime, because shep never binds
your app's listening socket and so cannot promise it. A refusal names the
sheep, the path it tried, and what to change. A command that touched one
sheep still shows you the other eleven, because the question you actually
had was whether anything else moved.

Where the sheep vocabulary would cost clarity it gets dropped. `kill` is
called `kill`, errors are plain technical English, and every themed verb has a
straight alias: `flock` is also `ls`, `bleats` is also `logs`.

## A first flock

A Flockfile describes what should be running:

```toml
[[app]]
name = "api"
script = "./bin/api"

[[app]]
name = "worker"
script = "./bin/worker"
```

```console
$ shep start ./Flockfile.toml
┌────┬────────┬──────────────┬───────┬──────────┬──────┬─────┬────────┬────────┬──────┬──────┐
│ ID │ NAME   │ STATUS       │ PID   │ RESTARTS │ EXIT │ CPU │ MEM    │ UPTIME │ FOLD │ SMIT │
├────┼────────┼──────────────┼───────┼──────────┼──────┼─────┼────────┼────────┼──────┼──────┤
│ 0  │ api    │ (o.o) online │ 30115 │ 0        │ -    │ -   │ 640.0K │ 0s     │ -    │ -    │
│ 1  │ worker │ (o.o) online │ 30116 │ 0        │ -    │ -   │ 1.2M   │ 0s     │ -    │ -    │
└────┴────────┴──────────────┴───────┴──────────┴──────┴─────┴────────┴────────┴──────┴──────┘
```

`shep save` writes that down, and `shep startup` installs the service that
brings it back after a reboot.

## Following output

```console
$ shep bleats --no-follow --lines 4
api | api listening on :8080
worker | worker ready
```

`bleats` follows by default, so drop `--no-follow` to keep watching.
A sheep that already crashed has said everything it is going to say, so
`--lines` prints that history before following rather than showing you an
empty screen.

## Reloading

```console
$ shep reload api
┌────┬────────┬────────────────┬───────┬──────────┬──────┬──────┬──────┬────────┬──────┬──────┐
│ ID │ NAME   │ STATUS         │ PID   │ RESTARTS │ EXIT │ CPU  │ MEM  │ UPTIME │ FOLD │ SMIT │
├────┼────────┼────────────────┼───────┼──────────┼──────┼──────┼──────┼────────┼──────┼──────┤
│ 0  │ api    │ (-.-) stopping │ 30115 │ 0        │ -    │ 0.0% │ 3.1M │ 2m 24s │ -    │ -    │
│ 3  │ api    │ (o~o) starting │ 31342 │ 0        │ -    │ -    │ -    │ 0s     │ -    │ -    │
│ 1  │ worker │ (o.o) online   │ 30116 │ 0        │ -    │ 0.0% │ 3.1M │ 2m 24s │ -    │ -    │
└────┴────────┴────────────────┴───────┴──────────┴──────┴──────┴──────┴────────┴──────┴──────┘
```

Two `api` rows, because the new instance is up before the old one goes down.
shep spawns, waits for readiness, drains, then reaps.

That overlap is not zero-downtime on its own, and `reload --help` says so.
shep binds its own control socket and nothing else, never your app's
listener, so both instances want the same port unless your app sets
`SO_REUSEPORT` on it. Without that the second one takes `EADDRINUSE`.

An app with a `readiness_probe` is reloaded the other way round: the old
instance drains first, then the new one starts in its place. One row, and a
gap while it happens. A probe asks an address, and an address cannot say
which of two instances answered it. Run both at once and the outgoing one
answers for the incoming one, so shep would call a release ready that never
bound anything. `reuse_port = true` says your app really does share the
socket, and buys the overlap back.

## Watching it

```bash
shep lookout
```

![the lookout dashboard: flock table, host strip, sheep detail and bleats feed](assets/lookout.svg)

Four panes over the same selection. `j`/`k` moves, `/` filters, `q` quits.
Read-only unless you pass `--allow-control`, and each action key arms a
confirm rather than acting on the keypress that pressed it.

## Letting an agent look

`shep whistle` speaks the Model Context Protocol over stdio, so an agent host
can ask about your flock. It writes nothing else to stdout, because stdout is
the wire.

| tool | mutates | destructive | gate |
|---|---|---|---|
| `list_flock` | no | | always |
| `describe_sheep` | no | | always |
| `tail_bleats` | no | | always |
| `get_metrics` | no | | always |
| `list_barks` | no | | always |
| `start_sheep` | yes | no | `allow_control` |
| `reload_sheep` | yes | no | `allow_control` |
| `stop_sheep` | yes | yes | `allow_control` |
| `restart_sheep` | yes | yes | `allow_control` |

The four that act exist only when `[whistle] allow_control = true` in
`shep.toml`. That gate is about legibility rather than containment: whistle
runs as you, so anything it could do you can already do by hand. A boolean in
a config file has a diff and an mtime somebody can audit.

## Dogs

A dog is a plugin process the shepherd supervises alongside your flock.

- [shep-log-rotate](https://github.com/shep-pm/shep-log-rotate) rotates
  and compresses bleat logs.
- [shep-deploy](https://github.com/shep-pm/shep-deploy) redeploys a sheep
  when a watched git branch moves.

`shep dogs` lists them, `shep adopt` takes one on.

## Everything else

<details>
<summary>Every verb, grouped as <code>shep --help</code> groups them</summary>

```text
Run things       start serve stop restart reload delete stock
See what's up    flock describe bleats lookout fold barks
Survive reboots  save muster startup unstartup
Talk to a sheep  trigger signal whisper
The shepherd     ping kill reopen flush set get unset
Dogs and agents  dogs enable disable adopt rehome whistle
Foreground runs  runtime dev
Coming from pm2  import
Help             welcome init help completions style
```

</details>

Full documentation, including a generated reference for every flag of every
verb, is at [shep-pm.com](https://shep-pm.com).

## Windows

The day-to-day loop works: `start`, `stop`, `restart`, `reload`, `flock`,
`describe`, `bleats`, `delete`, `save`/`muster`, `lookout`, `whistle`, and the
dogs. shep talks over a named pipe instead of a unix socket, and each sheep
sits in a job object instead of a process group.

Three limits are real, and none of them is going to be papered over:

- `shep stop` has no polite signal to send. Windows offers nothing
  SIGTERM-shaped that can be delivered to an arbitrary process, so a sheep
  that has not opted into the shepherd channel gets its full `kill_timeout`
  and is then terminated. Set `shutdown_with_message = true` and read the
  channel if your app needs a clean shutdown. Same path as unix, except the
  handle arrives as `$SHEP_CHANNEL_PIPE` instead of fd 3.
- `shep startup` is not built. Boot-time supervision on Windows means a
  Service Control Manager service, which is a different program shape rather
  than a fifth unit template. Run `shep start` in your own session, or wrap
  `shep runtime` in NSSM or WinSW.

- `user` and `group` in a Flockfile are refused, permanently. Dropping
  privilege on Windows needs a logon session or a primary-token privilege,
  which is a different and security-sensitive feature rather than a
  different call.

Smaller things differ too, and they are listed in
[docs/specs/deferred.md](docs/specs/deferred.md): most `shep signal` names
have no Windows delivery, and `$SHEP_HOME` inherits its parent's ACL rather
than being narrowed the way `0700` narrows it on unix.

## Building

```bash
git clone https://github.com/shep-pm/shep
cd shep
cargo build --release
cargo test --workspace --all-features
```

MSRV 1.88, edition 2024. `shep-core`, `shep-client` and `shep` are
`#![forbid(unsafe_code)]`. `shep-daemon` denies it crate-wide and permits it in
two files: `sys.rs`, for adopting a descriptor the daemon inherited, and
`sys_windows.rs`, for the job object that holds a sheep and its lambs. That
is eight sites on unix and ten on Windows, each with its own
`// SAFETY:` note, and the whole of the workspace's unsafe surface.

## License

MIT or Apache-2.0, at your option.
