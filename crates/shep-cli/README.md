# shep-cli

The `shep` binary. [shep](https://github.com/TurtIeSocks/shep) is a process
manager written in Rust: one binary runs a daemon called the shepherd, which
keeps a flock of your long-running processes alive, captures what they print,
and says plainly when something is wrong.

It runs on macOS and Linux. On Windows every command prints `shep does not yet
support Windows` and exits 1, which is a real answer but not a useful one.

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
$ shep ls
ID  NAME    STATUS  PID   RESTARTS  CPU    MEM    UPTIME  FOLD
1   web     online  1001  1         12.5%  48.1M  1m      backend
2   worker  online  1002  2         12.5%  48.1M  2m      backend
```

`shep bleats` follows the logs, and `shep logs` is the same command for people
who prefer the boring word. Every themed verb has a straight alias that works
forever. Everything renders as `--format json` too, under a versioned
envelope, so you can pipe it somewhere without scraping columns.

`shep import` reads a real pm2 dump and writes a Flockfile out of it.

The repository README has the full lexicon, what works today, and what is not
built yet. shep is pre-release: no tagged release, and several v1.0 items are
still missing.

## License

MIT OR Apache-2.0, at your option.
