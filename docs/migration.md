# Moving a flock from pm2 to shep

This is for an operator moving a running pm2 install to shep: what
`shep import` carries across, what it does not, and the steps to take a
flock through import, save, and a reboot without losing it.

`shep import` reads exactly one file: `~/.pm2/dump.pm2` (or whatever path
`--from` names). It does not read `ecosystem.config.js` or any other pm2
config format, and it never touches pm2's own state — nothing under
`~/.pm2` is written or deleted by anything in this guide.

### If your config is a `.js` file

`shep start <path> --flockfile` reads a `.js` file by running it through
`node`, so a config you generate rather than write out longhand still
works. It has to export the Flockfile shape — an `app` array with
sheep-native field names — not pm2's. Point it at a real
`ecosystem.config.js` and shep refuses, naming the key it found and the key
it wanted.

node gets 30 seconds to hand the config back and exit, then shep kills it and
names the file. Exporting the config is not enough on its own: a module that
leaves a server listening or a timer armed keeps node's event loop alive, and
node then never exits. An unattended `shep start` ends in a refusal rather
than waiting for a terminal nobody is sitting at.

Without the `--flockfile` flag, `shep start server.js` still means what it
has always meant: start `server.js`. And if node is not installed, shep
says so and tells you the alternative: *reading a .js Flockfile runs it
through node, and node was not found on PATH; install node, or convert
`<path>` to a .toml Flockfile.*

This quote is kept in step with `evaluate_js_flockfile`'s `format!` in
`crates/shep-cli/src/commands/lifecycle.rs` by hand, not by a test: producing
the `node`-missing path needs a `PATH` without `node` on it, and
`std::env::set_var` is `unsafe` in edition 2024 in a crate that forbids
unsafe, so there is no automated way to exercise it. If that sentence
changes, update this quote in the same commit.

## 1. What comes across, and what does not

`shep import` reads pm2's dump — one row per running *instance* — and
collapses same-named rows back into one app each, mapped field by field:

| pm2 | shep |
|---|---|
| `name` | `name` (the grouping key) |
| `pm_cwd` | `cwd` |
| `pm_exec_path` | `script` |
| `args` | `args` |
| `exec_interpreter` (`"none"` → run directly) | `interpreter` (absent for `"none"`) |
| `autorestart` | `autorestart` |
| `restart_delay` (ms) | `restart_delay` |
| `merge_logs` | `merge_logs` |
| `max_memory_restart` (bytes) | `max_memory` |
| rows sharing a `name` | `instances` = the row count |
| `exec_mode == "cluster_mode"` | `reuse_port = true`, plus a note (below) |
| `NODE_APP_INSTANCE` present in a row's env | `increment_var`, plus a note (below) |

A dump row with no `pm_exec_path` is refused by name rather than imported
as a broken app — `shep import` names the row's index and what keys it did
find, and stops. Every app the importer does produce is run through the
same config validation the daemon applies to a Flockfile at `shep start`
time, so a rejected field fails at import, not three seconds into a
sheep's life after a reboot.

`max_memory_restart` maps straight across as `max_memory`, but what it
does under shep is not identical to what it did under pm2: shep enforces
the ceiling against the app's whole process tree (the sheep plus every
lamb it forked), not the root process alone. An app that stayed under a
memory limit by forking workers to do the heavy lifting will read
differently under shep, and that is deliberate — a limit read off the root
pid only is trivial to dodge by forking, and shep does not offer that gap.

Two things do not come across, and neither is a bug in the importer — they
are both named on stderr instead, so you find out at import time rather
than at the first restart:

**Cluster-mode socket sharing.** pm2's cluster master holds one listen
socket and hands connections out to its workers. shep has no such master —
it binds nothing on any app's behalf. Running N instances of an app on one
port only works if the app arranges the socket sharing itself, with
`SO_REUSEPORT` (Node's `reusePort: true` listen option, which needs
Node ≥ 22.12). Without that, every instance past the first hits
`EADDRINUSE` the moment it starts. `shep import` sets `reuse_port = true`
on every app it found running in pm2 cluster mode and names it on stderr,
but setting the Flockfile field is not the same as the app doing the work —
if the app was never written to call `reusePort: true`, cluster mode does
not work under shep no matter what the Flockfile says. Real fd-passing
parity (shep handing out a socket the way pm2's master did, with no
cooperation needed from the app) is a v1.2 target, not yet built — see
`docs/specs/shep-v1.md` §2, "Versioned scope".

**An inherited shell environment.** This is its own rule, below.

## 2. The env rule

pm2 flattens whatever shell started `pm2 start` into every app it runs.
Run `pm2 start` by hand over SSH and the app inherits your login shell's
`BUN_INSTALL`, `JAVA_HOME`, `PATH`, whatever else was set — none of which
pm2 asked for and none of which the app's `ecosystem.config.js` ever
declared. Import that dump literally and a Flockfile would carry a
snapshot of one login session, most of which the app never needed and
none of which a daemon started by systemd or launchd will ever have: an
init-started process has no login shell behind it at all.

The rule `shep import` applies:

- A key **declared** in the app's own `env_<name>` blocks is always
  written. Its value is taken from what the process actually ran with when
  the dump was made, if the dump has one, otherwise from the declared
  value itself.
- A key that is not declared is checked against two short, closed lists —
  the shell's own variables (`PATH`, `SHLVL`, `SSH_TTY`, `LANG`, and
  similar) and what pm2 itself injects (`PM2_HOME`, `pm_id`,
  `unique_id`, ...) — and dropped silently if it matches either. This
  is session and tooling noise, not the app's own configuration.
- Everything else is **named on stderr and left out of the Flockfile.**
  `shep import` does not guess. An unrecognized key might be something the
  app genuinely needs (a stray `DATABASE_URL` set by hand once and never
  written down) or might be more session noise the closed lists don't
  happen to know about — the importer cannot tell the two apart, and
  guessing wrong in either direction is worse than asking. You decide
  where it belongs: the Flockfile's `env`, the unit's environment (next
  section), or nowhere.

`NODE_APP_INSTANCE` is a special case of the same rule: it is not copied
into `env`, because the dump only ever holds instance 0's value, and
copying it would tell every instance it is instance 0. It becomes
`increment_var` instead, which is shep's own mechanism for the same job —
each instance gets that environment variable set to its own slot number
at spawn time, the way `SHEP_INSTANCE` already works for shep-native apps.

## 3. `PATH` is the unit's

`PATH` is deliberately on the closed session-shell list above — it is
never written into an app's `env`, no matter how it got into the dump.
Instead, `shep startup` captures the `PATH` of the shell that ran it into
the systemd unit or launchd plist directly (`Environment="PATH=..."` /
the plist's `EnvironmentVariables` dict), the same environment every app
the daemon then spawns inherits.

This is the mechanism that makes an interpreter installed under
`~/.bun/bin` or `~/.cargo/bin` findable after a reboot. A daemon started
by systemd has no login shell and no `.bashrc`/`.profile` behind it —
without this capture, `PATH` at boot would be whatever minimal default the
init system starts with, and any app whose interpreter lives outside
`/usr/bin`/`/bin` would fail to spawn on the very first boot after the
migration, silently, until someone reboots the box again to find out why.
See the `sudo` trap in the troubleshooting section below — this is also
the one place the capture can go wrong, and `shep startup` warns about it
at install time rather than leaving it to surface at the next reboot.

## 4. The three commands

In the order you'd reach for them:

**`shep import`** turns pm2's dump into a Flockfile. It starts nothing —
no daemon connection, no socket, just a file read and a file write.
`shep import --dry-run` prints the Flockfile to stdout and writes nothing,
so `shep import --dry-run > Flockfile.toml` is a safe way to see the
result before committing to it. A normal run writes `./Flockfile.toml` (or
wherever `--out` names) and refuses to overwrite an existing file unless
you pass `--force`. Either way, every note — every cluster-mode app, every
dropped env key — goes to stderr, in both `--format table` and
`--format json`.

**`shep save`** writes the muster roll: a snapshot of the running flock
that a later `shep muster` (or a reboot, once `shep startup` is in place)
reads back. It takes no selector — the roll always records the whole
flock — and it talks only to an already-running daemon; it will not start
one on your behalf, because autostarting a daemon just to save an empty
flock would overwrite a good roll with an empty one. A save against a
daemon whose engine has already stopped fails loudly rather than writing
nothing and calling it success.

**`shep startup`** installs the unit that brings the shepherd — and the
flock `save` last recorded — back after a reboot. `shep unstartup` is its
undo. Both are covered in full in the rollback section below; the runbook
that follows walks through where `startup` sits in the whole sequence.

## 5. The runbook

This is the flagship scenario spec §13.4 describes: `shep import`,
`shep save`, and a reboot, on a Linux box. It needs an actual reboot, so it
cannot run in CI without a VM — this is what makes the outcome
checkable by hand instead of just assumed. Every step below names what to
check and what a failure looks like.

```
1.  shep import --dry-run           # read the Flockfile before it is written
2.  shep import                     # writes ./Flockfile.toml; starts nothing
3.  pm2 delete all && pm2 kill      # the one destructive step, and it is pm2's
4.  shep start ./Flockfile.toml     # the flock comes up under shep
5.  shep flock                      # every app online, CPU and MEM populated
6.  shep save                       # names the roll it wrote and the app count
7.  sudo shep startup --user <you>  # writes and enables the unit
8.  systemctl status shep-<you>     # active (running), and green
9.  reboot
10. systemctl status shep-<you>     # active (running) WITHOUT anyone logging in
11. shep flock                      # the same apps, new pids, uptime near zero
```

Step 3 is the only step that touches pm2, and the only irreversible one in
the list — everything before it only reads. Steps 1 and 2 leave pm2's own
flock running untouched; nothing is lost by stopping there to review the
generated Flockfile first.

Step 8's unit going `active (running)` means something specific here: the
generated unit is `Type=notify`, so systemd does not consider it started
until the daemon itself says so — and the daemon sends that signal only
once the muster restore from step 6's roll has finished, not the moment
the process execs. A green status at step 8 is therefore already evidence
the restore path works, before the reboot ever happens.

### Troubleshooting

**`activating (start)` then a timeout, at step 8 or step 10.** The daemon
came up but never reported readiness — either it never sent `READY=1`, or
the muster restore hung before reaching that point. `journalctl -u
shep-<you>` carries the daemon's own log records and is the first place to
look.

**`active (running)` at step 10, but an empty `shep flock` at step 11.**
The unit came up against the wrong `$SHEP_HOME` — it is supervising a
daemon, just not the one whose roll you saved. `systemctl cat shep-<you>`
shows the `SHEP_HOME` the unit actually carries; compare it against the
`$SHEP_HOME` step 6 saved into.

**The wrong `PATH` was captured, and an app fails to spawn only after a
reboot.** Step 7 runs as `sudo shep startup ...`, and `sudo` on most
distributions replaces `PATH` with its own `secure_path` before your
command ever runs — the `~/.bun` or `~/.cargo` entry the capture in
section 3 exists to preserve is exactly what `secure_path` tends to drop.
`shep startup` cannot tell a sanitized `PATH` from an untouched one after
the fact — the substitution happens before `shep` is even exec'd, so
there is nothing left in the environment to compare against — but it
knows when it is running under `sudo` (`$SUDO_USER` is set) and prints a
warning at step 7 itself naming the `PATH` about to go into the unit, so
you can catch a missing interpreter directory before the reboot rather
than after. If it is missing something, rerun as
`sudo --preserve-env=PATH shep startup ...` (after `shep unstartup`, since
`startup` refuses to overwrite the unit it just wrote) to carry your
login `PATH` through instead. `systemctl cat shep-<user>` still shows
what was actually written, at step 7 or any time after.

## 6. `pm2 serve` → `shep serve`

`shep serve <dir>` is a hand-rolled static file server, not axum +
tower-http, run as a managed sheep by default (add `--foreground` to run it
in the current terminal instead). Three defaults are flipped from pm2's own
`serve`, and each is a regression before it is a fix if you are not expecting
it:

- **Directory listing is off by default.** pm2 lists a directory that has no
  `index.html`; shep 404s it unless you pass `--listing`. A listing publishes
  every filename under the directory, and shep's posture is that the operator
  opts into that rather than discovering it later.
- **Dotfiles are refused by default.** pm2's `serve` publishes them; shep 404s
  any path with a dotfile component unless you pass `--hidden`. Serving a repo
  checkout with `shep serve .` would otherwise publish `.env` and the whole
  `.git` history. `--hidden` exists mainly for `.well-known/acme-challenge`.
- **Every symlink under the docroot is refused by default, not only one that
  leaves it.** A deploy layout like `dist/current -> ../releases/2026-08-15`
  or a symlinked `assets/` 404s unless you pass `--follow-symlinks`. Passing
  it reopens a check-then-open race that the default mode closes without a
  per-request `canonicalize` — that is the trade you are making, not an
  incidental cost, so reach for it deliberately for a layout that needs it,
  not out of habit carried over from pm2.

There is no `PM2_SERVE_*` environment compatibility — `shep serve` takes no
configuration from the environment at all. Pass `--port`, `--bind`, `--spa`,
`--auth <creds-file>` and the three flags above on the command line instead.

## 7. `pm2-runtime` → `shep runtime`

`shep-runtime` is the container entrypoint alias for `shep runtime`: a
foreground, no-daemon supervisor that reads a Flockfile, boots the flock
in-process, and auto-exits when it empties (exit 0 if every sheep stopped
clean, exit 11 if one ended in `errored` — see `docs/specs/shep-v1.md`'s exit
code table). At PID 1 it splits into a small init process first, so it can
reap re-parented orphans and forward SIGTERM/SIGINT/SIGHUP/SIGQUIT to the
supervisor — a container that skips this step accumulates zombies until
`docker stop` waits out the full grace period before SIGKILLing everything
mid-shutdown.

A minimal Dockerfile:

```dockerfile
FROM debian:bookworm-slim
COPY shep-runtime /usr/local/bin/shep-runtime
COPY Flockfile.toml /shep/Flockfile.toml
ENV SHEP_HOME=/shep
WORKDIR /shep
ENTRYPOINT ["shep-runtime"]
```

`SHEP_HOME` is not defaulted for you inside a container — an unset one fails
fast naming the flag, rather than inventing `/var/lib/shep` at 2am.

## 8. Rolling back

`shep unstartup` disables and removes the unit `shep startup` installed:
`sudo shep unstartup --user <you>`. Run unprivileged, it prints the same
kind of paste-able command `startup` does. A machine that never ran
`startup` at all reports the unit `absent` and exits successfully — there
is nothing left to guess at.

Nothing about this guide is destructive to pm2 itself, except the one line
in the runbook above that says so. `shep import` only ever reads
`dump.pm2` — pm2's own installation, its `~/.pm2` directory, and its
running flock (until you choose to stop it) are untouched by every command
here except `pm2 delete all && pm2 kill`, which is pm2's own command
against pm2's own state. Rolling back `shep startup`/`shep unstartup`
does not touch pm2 either way; pm2 is either still there because you never
ran the destructive step, or it is gone because you did, and nothing in
this guide brings it back.
