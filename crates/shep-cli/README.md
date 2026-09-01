# shep

The `shep` binary. [shep](https://github.com/shep-pm/shep) is a process
manager written in Rust: one binary runs a daemon called the shepherd, which
keeps a flock of your long-running processes alive, captures what they print,
and says plainly when something is wrong.

It runs on macOS, Linux and Windows. The Windows tier is the newest of the
three and refuses three things deliberately: no graceful signal outside the
shepherd channel, no `shep startup`, and no `user`/`group`. The repository
README has the reasoning for each.

Write a `Flockfile.toml`. Two fields is a complete one:

```toml
[[app]]
name = "web"
script = "./server"
```

Then start it. You never launch the daemon yourself, because `shep start`
notices nothing is listening and re-execs itself in the background.

```console
$ shep start Flockfile.toml
$ shep ls --style bare
ID  NAME    STATUS  PID   RESTARTS  EXIT  CPU    MEM    UPTIME  FOLD     SMIT
1   web     online  1001  1         -     12.5%  48.1M  1m      backend  -
2   worker  online  1002  2         -     12.5%  48.1M  2m      backend  -
```

`shep bleats` follows the logs, and `shep logs` is the same command for people
who prefer the boring word. Every themed verb has a straight alias that works
forever. Everything renders as `--format json` too, under a versioned
envelope, so you can pipe it somewhere without scraping columns.

`shep import` reads a real pm2 dump and writes a Flockfile out of it.

This crate builds three binaries, not one: `shep` itself, plus
`shep-runtime` and `shep-dev` — thin wrappers that prepend `runtime` and
`dev` before parsing, for use as a container `ENTRYPOINT` (`shep runtime` and
`shep dev` work identically through the `shep` binary; the aliases exist so a
container image needs no shell to supply the verb). `cargo install shep`
installs all three.

Embedding shep in another program is [`shep-client`](https://crates.io/crates/shep-client)'s
job, not this crate's — `shep` exposes nothing beyond its three `main*`
entry points, each of which owns the whole process (exit code, `argv`, signal
handling) the way a CLI is expected to.

The repository README has the full lexicon, what works today, and what is not
built yet. shep is pre-1.0, so anything can still change, and several v1.0
items are still missing.

## License

MIT OR Apache-2.0, at your option.
