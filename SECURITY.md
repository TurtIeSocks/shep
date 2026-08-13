# Security Policy

## Disclaimer

shep is a community project, maintained on a reasonable-effort basis. It
cannot provide legally-binding guarantees of security.

## Security premises

IF the runtime directory (`$SHEP_HOME/run/`) is created with mode 0700, the
daemon runs unprivileged (no setuid, no elevated capabilities), and the
control socket is a Unix domain socket authenticated by same-uid peer
credentials (`SO_PEERCRED` / `getpeereid`), THEN:

- No other local user can observe or control the flock. A process running as
  a different uid cannot connect to the socket, read its directory listing
  (0700 excludes it), or otherwise reach the daemon's RPC surface.
- Environment variable values are redacted by default everywhere a sheep's
  config is rendered back to a caller: `shep describe` output, RPC responses
  over the wire, and any `Debug` implementation on a type that carries them.
  Opt-in flags (e.g. `--with-env`) exist to reveal them explicitly; the
  default is always redacted.
- Tokens, webhook URLs, and other secret-carrying config values are never
  written to logs, `Debug` output, or RPC responses in plaintext.

These properties hold only while the preconditions hold. A daemon started as
root, a runtime directory created with looser permissions, or a socket
reachable by other uids invalidates them. Running as root is not entirely
unhandled: the log plane refuses paths a privileged shepherd should not be
writing below (see Log files). Nothing it does there restores the premises
above.

## Per-component

### Control socket

The daemon binds the RPC socket at `$SHEP_HOME/run/shep.sock`. The
`run/` directory is created with mode 0700 so only the owning user can open
it. On connect, the daemon checks the peer's uid via `SO_PEERCRED` (Linux) or
`getpeereid` (macOS/BSD) and refuses connections from any other uid. Windows
uses a per-user named pipe with equivalent access-control semantics. A stale
socket file from a crashed daemon is probe-connected before being unlinked,
so a live daemon's socket is never clobbered by a second instance.

### Spawned-process environment

Each managed sheep inherits an environment built from its Flockfile `env`
table plus daemon-injected variables (`SHEP_INSTANCE`, `SHEP_CHANNEL_FD`,
and similar). These values are held in daemon memory and passed to the
child process at spawn time. They are redacted in any output the daemon
sends back over the RPC socket or writes to its own logs, per the premises
above. The daemon does not scrub or validate what the child process itself
does with its environment once spawned — that is outside the daemon's
control boundary.

### Log files

Bleats (stdout/stderr capture, `$SHEP_HOME/logs/`) contain whatever the
managed process writes to its own standard streams. If a sheep prints a
secret to stdout, that secret lands in its log file — the daemon does not
scan or redact application output. Daemon-authored log lines (startup,
lifecycle events, RPC errors) follow the redaction rule above and do not
include env values or tokens.

Inside the default layout, a log file is not independently access-controlled.
It sits in `$SHEP_HOME/logs/`, which is created `0700` at `mkdir` time, and
that directory is the whole of its protection; the file's own mode is
whatever the umask allows.

Outside the layout, no such directory stands between the file and anyone
else. An app's `out_file`/`err_file` are free-form config taken verbatim,
pointing a sheep's logs at `/var/log/myapp.log` is supported, and shep
neither moves such a path into the layout nor tightens a directory it did not
create. A log file under a directory anyone can write to is readable, and
replaceable, by anyone who can write there. Naming a path outside
`$SHEP_HOME/logs/` means taking that directory's access control instead of
shep's.

What shep does guarantee about a log path, wherever it points:

- **Every directory it creates on the way is created `0700`**, with the mode
  asked for at `mkdir` time rather than chmod'd afterwards, so the directory
  never exists at the umask's wider mode. A directory that already existed is
  left exactly as it was, permissions included.
- **It will not follow a symlink standing at the log path itself.** Both
  openers pass `O_NOFOLLOW` (the log pump's appending handle, and the one
  `shep flush` opens to truncate), so a symlink planted where the log file
  was going to be fails the open instead of redirecting the write. The
  symlink and its target are left alone, and the failure names the path and
  says the word symlink, since an operator may have put that symlink there on
  purpose.
- **It will not open a log file below an ancestry another local user could
  redirect, when the daemon is running as root.** An ancestor counts as
  loose when it is owned by neither the daemon's uid nor root, or when it is
  a world-writable directory. Components are judged as themselves, so a
  symlinked intermediate directory is caught by its own owner.

What that does not cover:

- `O_NOFOLLOW` guards only the final path component. A symlinked parent
  directory still resolves. The ancestry check above is what covers that
  case, and only for a privileged daemon.
- The ancestry check is a check, not an atomic resolve, so a TOCTOU window
  remains between it and the open. Closing it needs
  `openat2(RESOLVE_NO_SYMLINKS)`, which is Linux-only, and macOS is a tier-1
  platform here.
- **An unprivileged daemon warns rather than refuses.** A loose ancestry is
  an escalation only when the daemon is privileged; a shepherd running as an
  ordinary user that logs into a shared directory has handed nobody anything
  they could not already do as that user. It is a footgun, and shep says so
  once per path at the default log level, but it is not blocked. Refusing
  would break logging to `/tmp` as yourself, which is legitimate.
- **Dropping privileges does not move any of this.** An app's `user`/`group`
  changes what the child runs as, and the child never sees its log file. Log
  I/O is the daemon's, on the far side of a pipe, and happens with the
  daemon's own privileges regardless.

`shep flush` empties exactly the paths the Flockfile named, so a mistyped
`out_file` makes it truncate that file. Its table names every path it emptied
for that reason. The shepherd's own `shepd.out.log`/`shepd.err.log` are not
reachable by any selector; `shep flush --daemon` is the only thing that
empties those.

### Rotating logs from outside shep

A rotator that renames or copy-truncates a log file has to tell the daemon,
or the pump keeps filling the inode it renamed away. Two forms do that, and
they are not equivalent. Prefer the command:

```
postrotate
    shep reopen all
endscript
```

`shep reopen` waits until every matched pump has closed both handles and
opened both paths again, and it reports what came of that: exit `0` means
nothing is still holding what the rotator renamed, and exit `9` names every
sheep and path that could not be opened again, on stderr.
That is the whole reason to spend a process on it. A sheep writing its stream
nowhere is exactly the failure a nightly rotation must not swallow, and a
rotator that checks the exit code catches it the same night.

`kill -USR2 <shepherd pid>` does the same work at the `all` selector and is
the fire-and-forget form: a signal carries no reply, so there is nothing to
wait on and nothing to check. It gives the operator nothing on failure. The
daemon logs the outcome instead, a failed reopen at `warn` so it is visible
at the default level, but a successful one at `info`, which the default
`log_level = "warn"` filters out. Confirming a signal-driven rotation worked
therefore means setting `log_level = "info"` and reading `shepd.err.log`. The
success line stays at `info` because a routine success is not a warning, and
promoting it would misreport its severity.

Either form leaves the sheep's own handle `O_APPEND`, so a `copytruncate`
rotator's next line lands at offset 0 rather than past a hole.

### Muster roll (`flock.json`)

The daemon persists its muster roll at `$SHEP_HOME/flock.json` so a restart
can restore the flock (`shep muster`, spec §9/§13.4). The roll stores each
registered app's config, including its `env` map, verbatim — the redaction
rule above covers output the daemon sends back over the RPC socket or writes
to its own logs, not this file. `flock.json` is created owner-only (`0600`)
and kept there across its atomic rename (see `server.rs`'s canonical
security writeup for the daemon-wide rundown of what this crate writes to
disk). Anyone who can read `$SHEP_HOME` — the daemon's own user, or root —
can read every secret held in any managed app's `env` table.

### Adopted dogs

IF an operator runs `shep adopt <name> <path>`, THEN:

- The binary is vetted **once, at adopt time** — existence, not-a-directory,
  an execute bit set for someone, no world-writable mode on the binary or
  its containing directory (refused outright, before any of it is run), a
  warning on the terminal if either is merely group-writable, and a real
  spawn (killed immediately) proving this kernel can actually exec it. None
  of that runs again at boot or at `shep enable` — the daemon spawns
  whatever `shep.toml` recorded, not whatever is sitting at that path by
  the time it does.
- The path `shep.toml` records, and the one the daemon later execs, is the
  **canonicalized** one, resolved once at adopt time rather than against
  the daemon's own working directory at spawn — which, after a reboot, may
  not be the directory `adopt` ran from.
- **The adopted binary then runs at the shepherd's own trust level, with no
  sandboxing beyond it.** State it rather than imply it: this is the same
  trust an ordinary sheep already has, and adopting a dog grants nothing
  beyond what any Flockfile entry already could. There is no seccomp
  profile, no namespace, no separate uid for a dog.
- A dog's `[dog.<name>]` config — a bark sink's webhook URL included —
  reaches it over the Unix socket, never through its environment, so it
  cannot appear in the process table, in a spawned child's own inherited
  environment, or in a crash dump. `barks.jsonl` keeps the same rule at
  rest: a fired alert records a sink by its `[dog.bark.sinks]` config key,
  never the URL or token behind it, and the file is created `0600` inside
  the already-`0700` `$SHEP_HOME`.
- Discord and Slack sinks are refused at config load if given a `http://`
  URL — the webhook token lives in the path, and both services only ever
  serve `https://`, so no working deployment loses anything to the refusal.
  `Sink::Json` is exempt: it can point at any operator-chosen endpoint,
  including a plaintext internal one, because shep has no way to tell a
  deliberate internal target from a mistake by looking at the URL alone.

These hold only for the binary `shep adopt` actually vetted. Replacing the
file at that same canonicalized path afterward — after adoption, before the
daemon next execs it — is a window nothing here closes: the path is what is
pinned, not a hash of what was read from it.

### Metrics and serve binds

The metrics dog's Prometheus exposition endpoint and the `shep serve` static
file server both bind `127.0.0.1` by default. Reaching either from another
host requires an explicit config change to bind a non-loopback address, at
which point the operator is responsible for any additional network
controls (firewalling, TLS termination, auth).

## Non-goals

- **Root can always read daemon memory.** shep's privilege model protects
  against other unprivileged local users, not against root on the same
  machine. A root-owned process can read the daemon's memory, its config
  files, and its socket regardless of any setting described here.
- **No network hardening claims pre-1.0.** Binding a shep-managed service
  (metrics, serve, or a supervised app) to a non-loopback address is the
  operator's decision and outside shep's threat model until 1.0. shep does
  not audit, filter, or rate-limit traffic to processes it supervises.
- **No protection against a compromised managed process.** A sheep that is
  itself compromised can do anything its own OS-level permissions allow,
  including reading files the daemon's user can read.

## Supported versions

Pre-1.0: only the latest published release receives security fixes. There is
no long-term-support branch yet.

## Reporting a vulnerability

Report security issues privately through
[GitHub Security Advisories](https://github.com/TurtIeSocks/shep/security/advisories/new)
for this repository. Do not open a public issue for a suspected
vulnerability.

This project is maintained on a reasonable-effort basis. Please allow at
least 90 days to investigate and prepare a fix before any public disclosure.
