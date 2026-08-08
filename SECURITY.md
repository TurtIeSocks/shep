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
reachable by other uids invalidates them.

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
include env values or tokens. Log files inherit the permissions of
`$SHEP_HOME`; they are not independently access-controlled.

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
