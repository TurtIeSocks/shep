# Phase 8 — the pm2 cutover set

**Goal:** make it possible to retire pm2 on a real machine. Four pieces:
`shep muster save`, `shep import`, `shep startup`/`unstartup`, and inline
CPU/memory in `shep flock`.

**Success criterion** is spec §13.4, and it is the one that matters:
`shep import && shep muster save && reboot` leaves the flock running, started
by the init system rather than by a login shell.

## What this phase is not

Deferred, unchanged, and listed in `docs/specs/deferred.md`: the dogs
subsystem and both dogs, the lookout TUI, whistle MCP, the Windows tier,
`serve`/`dev`/`runtime`, `scale`/`signal`/`sendline`, `enable`/`disable`, and
the KV store. The `ActionReply` correlation id gets its own small phase after
cutover — the cost asymmetry that made `params` urgent does not bite while no
external consumer of the shepherd channel exists.

## Two traps found in a real pm2 dump

A production dump (9 apps, 15 instance rows) was read during design. Both
findings shaped the work, and both are cutover blockers rather than polish.

**Cluster mode has no shep equivalent.** Two apps ran `exec_mode: cluster`
with 4 instances each — HTTP servers relying on pm2's cluster master holding
one listen socket and handing connections to workers. shep binds nothing;
four independent processes on one port is `EADDRINUSE`. The app itself must
set `SO_REUSEPORT` (Node's `reusePort: true`, which requires Node >= 22.12).
Real fd-passing parity is deferred to v1.2 by the spec. **`import` must say
this at import time**, naming each affected app, rather than letting it
surface as a bind failure at first start.

**pm2's dump flattens the invoking shell's environment into each app.** Of 31
env keys on one app, 24 were session artefacts (`SSH_TTY`, `XDG_SESSION_ID`,
`MOTD_SHOWN`, `LS_COLORS`), 4 were pm2-injected, and the *declared* env was a
single `NODE_ENV`. Importing that wholesale would pin a dead login session
into config that then survives reboots. It also exposes the second trap: apps
started by hand over SSH silently inherit things like `BUN_INSTALL` and
`JAVA_HOME`, and an init-started daemon has neither.

## `shep muster` / `shep muster save`

The debounced atomic snapshot writer and the boot-time restore both ship and
are tested; neither is reachable by an operator.

- `shep muster save` — `Request::MusterSave`. Writes the roll immediately,
  bypassing the debounce, and replies with the path written and the number of
  apps recorded. The reply matters: a save that silently does nothing is the
  failure mode this verb exists to rule out.
- `shep muster` — restores the roll, autostarting the daemon if it is not up.
  This is what the init unit runs, and pm2's `resurrect`.

## `shep import`

Reads `~/.pm2/dump.pm2`; `--from <path>` overrides. Writes a Flockfile and
**starts nothing**. `--dry-run` prints what it would write.

Dump-only by choice. It is JSON, so no `node` dependency, and it records what
is actually running rather than what was declared — including apps started ad
hoc. An `ecosystem.config.js` overlay was considered and rejected: reading it
faithfully means evaluating JavaScript, which is exactly why `.js` Flockfile
support is deferred, and the dump already holds the resolved truth
(`pm_exec_path` + `args` beats a `script: "bun run start"` string).

**Instance collapsing.** The dump is per-instance. Rows are grouped by `name`
and become one app with `instances = N`.

**Field mapping.**

| pm2 | shep |
|---|---|
| `name` | `name` |
| `pm_cwd` | `cwd` |
| `pm_exec_path` + `args` | script + args |
| `exec_interpreter` (`none` = exec directly) | interpreter |
| `autorestart`, `restart_delay`, `merge_logs` | same |
| `max_memory_restart` (bytes) | `max_memory` (`MemSize`) |
| `exec_mode: cluster` | `instances = N` + `reuse_port = true` |

**Env.** Declared env only. Inherited-shell and pm2-injected keys are dropped
by construction, not by heuristic. Any dropped key that does not match a known
session-junk pattern is **named in the output** so the operator decides
whether it belongs in the Flockfile, the unit, or nowhere. A heuristic that
silently guesses which inherited vars matter will eventually be wrong; this
way the guess is the operator's, with the evidence in front of them.

**Output must report**, not bury: how many apps were read and from where, each
clustered app and the `reusePort` requirement it carries, and every ambiguous
env key dropped.

## `shep startup` / `shep unstartup`

Detects systemd (Linux) or launchd (macOS) and renders a unit carrying shep's
resolved exec path, `SHEP_HOME`, the target user, and **`PATH` captured from
the invoking environment** — the mechanism that makes interpreters installed
under `~/.bun` or `~/.cargo` findable after a reboot.

systemd unit: `Type=notify`, `ExecStart` runs `shep muster` in the foreground,
`ExecReload=shep reload all`, `ExecStop=shep kill`, `Restart=on-failure`,
`WantedBy=multi-user.target`. macOS: a `LaunchDaemon` plist with the same
content.

Two new daemon behaviours fall out of this:

- **A foreground mode.** Under `Type=notify` the daemon must not self-daemonize
  — systemd owns the process. shep currently daemonizes by re-execing itself
  with a hidden `daemon` subcommand, so this is a deliberate second path, not
  an accident.
- **sd_notify.** An `AF_UNIX` `SOCK_DGRAM` `sendto` of `READY=1` to
  `$NOTIFY_SOCKET`, **after the muster restore completes**. No new dependency.
  The point is that the unit goes green when the flock exists rather than when
  the process execs — which makes "did it survive the reboot?" answerable, and
  turns a hung restore into a failed start instead of a green unit supervising
  nothing.

**Privilege.** Install and enable when running privileged; otherwise print
exactly the command to run and exit non-zero so a script notices. shep never
escalates on its own. `unstartup` disables and removes under the same rule.

## Inline CPU and memory

`sysinfo`, the sampler, and `tree_rss` (which sums the root process and every
descendant, so a clustered app reports like `pm2 ls`) all ship already. The
gap is that `LimitEnforcer::arm` runs only for apps that set `max_memory`, and
`ProcessInfo` carries no stats fields.

- **Split sampling from enforcement.** The 15s loop samples every sheep and
  keeps a per-pid baseline of CPU time and instant. Enforcement continues to
  run only where a limit exists.
- **Fresh on demand.** `flock` and `describe` take a live sample and compute
  CPU as a delta against the last *periodic* baseline. Memory is always
  current; CPU needs no blocking second reading.
- **The baseline is always the periodic sample, never a previous on-demand
  one.** Two `flock` calls a moment apart would otherwise divide by a
  near-zero window and report nonsense. This bounds the window to <= 15s and
  keeps it away from zero.
- **A sheep with no baseline yet reports `-`.** A process spawned since the
  last tick has no honest CPU number, and inventing one from a 50ms window is
  worse than an empty cell.
- `ProcessInfo` gains `cpu_percent` and `memory_bytes`, both `Option`,
  additive under `#[non_exhaustive]`, with the regenerated wire snapshot's
  delta verified to be only the addition.

## Testing

**Fixtures are synthesised**, modelled on a real dump's shape and never
derived from it. A real dump carries absolute paths, a live SSH session's
environment, and the layout of a production host; none of that belongs in a
repository.

Automated: import correctness against fixtures (instance collapsing, the field
mapping, env filtering, the cluster warning firing), unit and plist generation,
`shep muster save` writing when asked, the stats delta including the
no-baseline and rapid-call cases.

Manual, documented as a runbook in `migration.md`: the §13.4 scenario itself.
It needs a reboot, so it cannot run in CI without a VM. The runbook states
what to check after the reboot, so the criterion is falsifiable by hand rather
than assumed.

`systemd-analyze verify` runs against the generated unit where available.

## Assumptions

- The target host runs Node >= 22 for any clustered app, so `reusePort: true`
  is available. Established during design; on Node 20 those apps cannot come
  across at more than one instance.
- `~/.pm2/dump.pm2` exists, i.e. `pm2 save` has been run at least once.
- System-level init integration (systemd system unit, launchd LaunchDaemon)
  rather than user-level. User units avoid one root step and add another
  (`loginctl enable-linger`), plus a failure mode where the flock silently
  does not return after a reboot.
