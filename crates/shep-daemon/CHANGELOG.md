# Changelog

All notable changes to `shep-daemon` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> PR references (`([#NN])`) start once the repository has a public remote to
> link against.

## [Unreleased]

### Fixes

- Name the sheep and the path in a failed spawn's message. The whole error
  an operator got was `process spawn failed: No such file or directory (os
  error 2)`, which on an eleven-app Flockfile said neither which app had
  failed nor which path had been tried. It now reads
  ``<name>: process spawn failed: <os error>; tried `<script>` in <cwd>``,
  with the `cwd` clause present only when the app sets one. The path is the `script`/`interpreter`
  and `cwd` as the Flockfile spells them rather than a resolution of the two,
  because that is where the edit has to be made. `shep stock`'s own partial-
  scale reply gains the path too and does not repeat the name, which it
  already opens with.

- Check every app in a `Start` batch before registering any of it, and
  register nothing if any of them fails. One app pointing at a `script` that
  does not exist used to register and start the apps ahead of it, fail on
  that one, and never reach the apps behind it, leaving a flock that matched
  neither the Flockfile nor its previous state. The error now names every app
  that failed and the path each was looked for at, rather than the first
  failure alone. Passwd resolution for `user`/`group` is hoisted into the
  same pass for the same reason.

  The refusal reaches only a `script` or `interpreter` containing a `/`. A
  path is a claim about the filesystem, which the daemon can settle and an
  operator can fix with a typo correction. A BARE command is a claim about an
  environment, and the environment that decides is the daemon's rather than
  the shell an operator tested in: a `shep startup` unit gives the shepherd
  whatever `PATH` launchd or systemd hands it, and shep's own fallback is
  `/usr/local/bin:/usr/bin:/bin`, so homebrew's `node` on Apple Silicon
  (`/opt/homebrew/bin`) and nvm's (under `$HOME`) both resolve in a terminal
  and neither resolves under the unit. A bare command that does not resolve
  is reported in the shepherd's log, naming the sheep and the program, and
  the batch goes ahead; that one app's spawn then fails exactly as it did
  before. Refusing there would keep a whole flock down at boot over one app's
  interpreter, which is worse than the partial registration being fixed.

  A filesystem error that is not "no such file" never refuses a batch either.
  `Path::exists` returns `false` on any `fs::metadata` error, so a permission
  error on an intermediate directory, an unsettled mount and a race all read
  identically to "absent"; the check now matches `ErrorKind::NotFound`
  specifically, and everything else is a suspicion the spawn reports as it
  always did.

- Keep the pre-registration refusal to the one caller it was written for.
  `do_start` is shared, so the check reached two callers that must not have
  it. A dog whose binary is missing was refused and left no trace, where it
  belongs in the dogs table as `Errored`: `spawn_fresh` registers that row on
  purpose, `shep dogs` renders it, and `dogs::spawn_dog_watch` subscribes to
  it, so an operator who enabled a broken dog needs to see it rather than
  find nothing. Worse, restoring a muster roll refused the WHOLE roll when
  one saved app's binary had gone missing, so a machine came back from a
  reboot with nothing running at all, unattended. Both now register each app
  on its own merits, via a `BatchPolicy` the call site states explicitly.
  All-or-nothing stays what `shep start` against a Flockfile does, which is
  the case the check exists for and the only one where an operator is holding
  a terminal.

  The policy governs every point `do_start` can stop at, which took three
  passes to get right: the pre-registration check, the spawn loop, and
  `user`/`group` resolution. A `PerApp` batch now survives an app whose user
  cannot be resolved, where one unresolvable name used to refuse an entire
  muster restore.

  Resolving credentials no longer builds a vector to zip against the apps.
  That pairing was safe only while a failure returned early; once a failure
  is SKIPPED, the two sequences drift, `zip` hands app 2 app 3's credentials,
  and the last app is dropped without a word. That is a privilege
  misassignment rather than a scheduling bug: the flock comes up looking
  correct with processes under identities nobody chose. Each app now carries
  its own credentials from the point of resolution, so there is no second
  sequence to keep in step.

  An app whose credentials fail is skipped rather than registered `Errored`,
  unlike an app whose spawn fails. Nothing can be assembled without an
  identity to assemble it for, and running it under the daemon's own identity
  instead is the outcome being ruled out. Both are named in the error.

  The policy governs the SPAWN loop as well as the pre-registration check.
  That loop returned on the first failure whatever the policy said, so a bad
  entry that was not last still took every app after it down: `a-good` came
  up, `b-bad` failed, and `c-good` was never registered at all. A muster roll
  is written from a `BTreeMap` and restored in that order, so this was a
  certainty whenever the broken name sorted first rather than a race. Under
  `PerApp` a failed spawn now records that app, leaves it `Errored` and
  visible, and moves to the next; the app's own remaining instances are
  skipped, since they share a binary and would only add identical wrecks to
  the listing. The error still names every app that failed, so the muster's
  existing "failed to spawn one or more apps" log keeps its meaning.

- Explain a bare program's spawn failure in the reply, not only in the log.
  When a `script` or `interpreter` with no `/` in it fails to spawn, the
  `SpawnFailed` message now carries "`node` is not on the shepherd's PATH
  (...)" in place of the bare "tried `node`" clause, so an operator at a
  terminal gets the diagnosis rather than only `No such file or directory`.
  No protocol change: `SpawnFailed` already carries free-form text. A PATH of
  more than four entries is summarised, because a daemon autostarted from a
  shell inherits that shell's PATH, which measured two kilobytes and buried
  the sentence that mattered.

- Every reply carrying a listing now comes back in the same order the flock
  listing does: by name, then by id. `ListFlock`, `Describe` and `Mustered`
  already grouped by name; `Start`, `Stop`, `Restart`, `Reload`, `Scale`,
  `Reopen`, `Flush`, `Trigger`, `Signal` and `SendLine` came back in whatever
  order they were assembled, so one session against one flock printed two
  different orders. Wire-observable, and so visible under `--format json` as
  well as in a table. `Delete` is not in that list and never was: it answers
  with ids alone, which the daemon already sorts.

- `snapshot_all` takes the shared rule rather than a finer one of its own. It
  sorted `(name, instance, id)`, which is more stable across a reload and
  which no listing that has crossed the wire can reproduce, since
  `ProcessInfo` carries no instance number. The two agreed until a reload
  churned an id and then diverged, so `ListFlock` could order a reloaded app
  differently from the `Restart` reply printed a second earlier -- the very
  inconsistency this change exists to end, one layer down. It now calls
  `sort_flock`, so the two cannot drift.

- A sheep restored from the muster roll comes back under its configured
  `user`/`group` rather than under the shepherd. `register_at_rest` records
  membership without resolving anything, and `ProcessEntry::credentials`
  spelled "nobody has looked this app up yet" and "this app asked for nobody"
  the same way, as `None`. A later `shep restart` read the second meaning and
  started the child as the daemon: no error, no warning, and nothing in `shep
  describe` to see it by. The field is now a `SpawnIdentity`, which tells the
  two apart, and every spawn path reaches a usable `Option<Credentials>`
  through one seam that resolves an unresolved entry instead of falling back.
  An app that resolved once is still settled for good, so a restart neither
  re-reads the passwd database nor changes a running app's identity underneath
  it.

### Additions

- Add `SupervisorError::CannotStart`, a `Start` batch refused before anything
  was registered. Separate from `SpawnFailed` because nothing was spawned,
  and an operator told "spawn failed" about a spawn that never happened is
  being pointed at the wrong place; the two also differ in what they leave
  behind, which is the part that matters operationally. It maps to the
  existing `RpcErrorCode::SpawnFailed` all the same, so exit 7 and every
  older client are unchanged: `RpcErrorCode` is versioned and a client
  predating a new code could not decode the reply at all.

- Add `ProcessRunner::preflight` and `Preflight`, what a runner can tell
  about a `SpawnSpec` before anything is spawned. Three verdicts, because a
  caller registering a batch has three different things to do with the
  answer: `Unknown` ("nothing knowable in advance", never "this will work"),
  `Impossible` (a certainty, and the only one a caller may refuse a whole
  batch over), and `Doubtful` (report it and carry on). `preflight` is
  defaulted to `Unknown`, so an out-of-tree implementor is unaffected and a
  runner that never touches the filesystem gives the honest answer.
  `TokioRunner` answers `Impossible` for a `/`-containing path that is not on
  the filesystem, `Doubtful` for a bare command missing from the `PATH` the
  child will actually be given, and `Unknown` for everything else, including
  a relative path with no `cwd` and any filesystem error other than
  `NotFound`. Existence only, never the executable bit.

- Answer `Request::ConfigDrift`: report which of a set of apps name a
  registered sheep whose stored config differs, and in which fields. Reads
  the flock and changes nothing -- it registers, spawns and records nothing,
  so it is answered during a shutdown rather than refused the way `Start` is.
  The incoming configs are re-normalized first, on the same untrusted-peer
  rule `Start` follows plus one of its own: an unnormalized config would
  report every default it did not spell out as a difference from the
  normalized copy the flock stores.

- `privilege::SpawnIdentity` — the type behind the fix above.
- `fake::ScriptedRunner::spawned_as` and `spawn_count`, behind `test-fakes`.
  The fake becomes nobody, so recording the credentials a spawn was asked for
  is the only way a test can assert the identity it carried rather than merely
  that it happened.

## [0.1.0] - 2026-08-26

### Additions

- Record every sheep's last exit outcome and carry it on `ProcessInfo`. The
  supervisor already had it -- it decides restart policy against
  `stop_exit_codes` -- and discarded it, so no operator surface could say why
  a sheep died. Set on every path through `handle_exited`, which is the one
  place a process under a registered id stops existing, and therefore covers
  an operator stop, a delete, a reload's drainee, a crash loop and shutdown
  alike. Cleared when a respawn fails to spawn at all, because nothing exited
  there and a stale code would read as a fresh crash.

- `MemorySampler::identify` (defaulted) and `StatsState::lambs_of` — a
  sheep's parent-pid descendants, walked on demand and carried by `Describe`
  only.
- `Request::SendLine` — one line to a sheep's stdin, per-sheep outcome,
  bounded at two seconds.
- `SpawnSpec::stdin` and `ProcIo::to_stdin` — an opt-in pipe on a sheep's
  stdin, with a per-line acknowledgement.
- `Request::Scale` — set an app's instance count. Scale-up takes the lowest
  free slots, scale-down releases the highest, and the new count is written
  back to the muster roll.
- `RunningProcess::signal_process` (defaulted, so it is additive for an
  out-of-tree implementor) and `Request::Signal` — one signal to one sheep's
  own process, not its group.
- Every shepherd-channel message a sheep writes is forwarded to the bus,
  including an `action-reply` no trigger is waiting for.
- `ShepherdMessage::Action` carries `id`; `ChildMessage::ActionReply`
  accepts an optional `id` echo. Additive — an app that ignores both is
  matched by name and order exactly as before.
- `SHEP_CHANNEL_VERSION` is exported to every child with a channel;
  `channel::CHANNEL_VERSION` is its value.
- Record, in `barks.jsonl`, an enabled dog that exhausts its restart budget —
  the shepherd's own trail for the one alert it cannot deliver itself. It has
  no sinks and no webhook code by design, so a dead bark dog can raise no
  webhook alert about its own death; what it can guarantee is a local record,
  so an operator reading `shep barks` after an outage finds the moment
  alerting stopped rather than a gap they have to infer. Written by a bus
  watcher (`dogs::spawn_dog_watch`), not a branch inside the supervisor —
  supervision stays blind to dog-ness, and this only answers who should see
  an event that already happened. Fires on a dog's `Errored` only, never on
  an `Exit` it survives or a sheep's own `Errored`, which is bark's record to
  write. A lagging watcher logs the drop at `warn!` and does not attempt to
  recover it; polling for what the bus already dropped would be building a
  second bark dog inside the shepherd.

- Start every `[daemon] enabled_dogs` dog when the daemon boots, strictly
  after the muster restore and strictly before the daemon reports itself
  ready. Both halves of that placement are load-bearing: after the restore,
  because a metrics dog started first would answer for an empty flock for
  the whole restore window, and a bark dog would raise a `process.start`
  alert for every sheep the roll brings back; before readiness, because
  `Type=notify` going green is meant to mean the whole daemon — flock and
  dogs alike — is up, the same reasoning that already put the restore
  itself inside that promise.

  A dog that will not start never fails the boot — the flock comes up and
  the daemon serves regardless, with a `warn!` naming the dog. That covers
  a binary this build cannot spawn, a spawn failure the OS reports, AND the
  case `EnableDog`'s own handler already guards: `start_dog` is idempotent
  by name, so a dog enabled under a name a sheep already holds comes back
  `Ok` over the sheep rather than starting anything, and reporting that as
  a success would be a false one, exactly as it would be over the socket.

- Answer the three dog verbs. `DogConfig` hands a dog its own `[dog.<name>]`
  section as TOML text, `EnableDog` starts one, and `DisableDog` stops and
  deregisters it through the same `delete` a sheep goes through — kill
  ladder, graceful timeout, deregistration — rather than a second way to end
  a supervised process.

  A dog's configuration travels over the socket, never in its environment.
  The child inherits `$SHEP_HOME` and nothing else it did not already need in
  order to exec; it connects to the socket that names, handshakes, and asks
  for its section. A bark sink is a webhook URL with a bearer token in it,
  and the environment is readable from the process table on some systems,
  inherited by every child a dog spawns, and captured into crash dumps. The
  reply is opaque text the dog parses rather than a shep type, so a
  third-party dog is bound to the shape of its own section and not to this
  project's config model, file discovery or layering rules — changing any of
  those cannot then break a dog nobody has seen.

  The section is read from disk on every request rather than served from a
  copy taken at boot. One reader can never be stale, and it is what makes
  `shep disable X && shep enable X` pick up an edited section. A missing
  file, or a file with no such section, answers with the empty string: a dog
  with no configuration is the ordinary case, not a fault.

  Enabling a dog under a name a sheep already holds is refused rather than
  answered. Starting one is idempotent by name, so what comes back is
  whatever already holds the name; an unmarked entry means no dog started and
  none can while the name is taken, and reporting it would claim a success
  that never happened.

- Start a dog. `SupervisorHandle::start_dog` registers one through the same
  spawn path a sheep takes, and writes onto its entry where the dog came
  from. The marker rides the entry rather than a registry of its own, which
  is what makes a restart, a memory-limit respawn, a cron occurrence, a
  watch-triggered restart and a reload all keep it without any of them
  knowing dogs exist — a reload reads it off the instance it is replacing,
  and everything else mutates the entry in place. It lands on a dog that
  failed to spawn too: a binary that is not there has to be visible as
  `errored` in the dogs table, not as a sheep nobody started.

  Starting a dog is idempotent by name. `shep enable` runs against a daemon
  that may already have the dog, and a second live process under one name
  would mean two connections, two metrics listeners on one port and two
  copies of every bark; the dog already registered is reported as it stands
  instead. A dog is refused outright once a graceful shutdown has begun, the
  same rule `start`, `restart` and `reload` follow — the shutdown's kill list
  is fixed when it runs, so a child registered after it is one nothing would
  kill.

- Keep a dog out of what a wildcard selector sweeps. `stop all`, `reload all`,
  `delete all`, `describe all` and a `/regex/` or `fold:` sweep now pass every
  dog by, while a selector that names one — `shep restart bark`, or its id —
  still reaches it. `flock` is the deliberate exception: it is the single
  registry both the flock table and the dogs table are rendered from, so
  filtering there would leave the second one with nothing to show.
  A dog is a process an operator installed rather than a member of the flock
  `all` means, and an operator sweeping the flock does not expect to take the
  metrics plumbing down with it. Selection is now answered in one place for
  every verb that resolves a selector against the registry, so the reach of
  `stop`, `reload`, `reopen`, `flush` and `trigger` cannot drift apart; a
  by-product is that a multi-match `stop`/`restart` emits its events in id
  order rather than in whatever order the registry happened to yield.

- Answer `flock` and `describe` with each sheep's live CPU and memory, in the
  two `ProcessInfo` fields shep-core grew for them. The reading is taken when
  the request is served, not read off the last periodic tick, so memory is
  current rather than up to 15 s stale; CPU is the delta since that tick,
  which is what lets a listing report a rate without blocking for a second
  reading of its own. A sheep the daemon has not yet sampled once reports no
  CPU — a percentage invented from a 50 ms window is worse than an empty
  cell.

  Only those two verbs pay for it. The sample is a syscall walk over the
  host's whole process table, measured at 5.77 ms across 883 processes, and
  `start`/`stop`/`restart`/`reload`/`reopen`/`flush` answer with
  `ProcessInfo` rows nobody reads resource use from. It runs on the blocking
  pool rather than on a runtime worker, and a listing whose sample fails
  comes back without the numbers rather than failing outright.

- Name `fake::FIRST_SCRIPTED_PID` (behind `test-fakes`), the pid
  `ScriptedRunner` hands its first spawn. A fixture describing that proc's
  process table can say which pid it means instead of repeating the literal.

- Sample every sheep, and enforce only where a limit exists. The polling loop
  used to run for the ids `max_memory` armed it against and for nobody else,
  so an app that set no ceiling — the ordinary case — was never measured at
  all. Sampling is now its own concern (`limits::stats`): every sheep with a
  pid is watched from the moment it comes online, and a memory ceiling arms
  the enforcer on top of that rather than instead of it. Enforcement itself
  is untouched — same cadence, same self-disarm on breach, same backpressure
  on a full report channel.

  It costs no extra walk of the process table. The polling tick already built
  one index per pass; it now hands that index to the sampler before running
  the enforcement pass, so the 6.5 ms syscall walk still happens once every
  15 s however many sheep are in the flock.

  CPU is the reason the two halves have to share a tick. Resident memory is a
  level and can be read on demand, but the OS reports CPU as a counter, so a
  percentage needs two readings and the wall time between them. The tick
  records one baseline per sheep; an on-demand read subtracts against it and
  writes nothing back, which is what stops two listings a moment apart from
  dividing a near-zero counter delta by a near-zero window. A sheep that came
  online since the last tick has no baseline and reports no CPU at all, which
  is the honest answer for a window nobody has measured yet.

- Report readiness to an init system that supervises the shepherd directly:
  one `READY=1` datagram to whatever `$NOTIFY_SOCKET` names, sent as the
  **last** step of `boot`, after the muster restore has finished and the
  control plane is assembled. New unix-only `notify` module.

  The ordering is the whole feature. A unit that goes green when the process
  execs describes a flock that is not up yet, so a restore that hangs reads
  as a healthy service supervising nothing, and anything ordered after that
  unit starts against apps that do not exist. Reporting last turns the same
  hang into a failed start. This is deliberately the opposite of the
  readiness pipe filed further down, and the two answer different parents:
  that one tells a `shep` process that daemonized this one it may now exit,
  and is written the moment the socket binds so a slow muster cannot make it
  think the boot failed; this one decides when a unit goes green. Both may
  be set, but whichever is supervising, the other is not.

  No new dependency and no unsafe: `std` addresses both shapes systemd can
  hand a service — a filesystem path through `UnixDatagram::send_to`, and an
  `@`-prefixed abstract name through `SocketAddrExt::from_abstract_name` plus
  `UnixDatagram::send_to_addr`, stable since 1.70. Off Linux an `@` address is
  `NotifyError::Unsupported` rather than a path, because there is no abstract
  namespace to reach and writing into a file literally named `@…` would
  succeed while telling nobody anything. An **absent** `$NOTIFY_SOCKET` is
  the ordinary case — every interactive run, the daemon the CLI autostarts
  for itself, and macOS, whose launchd has no readiness protocol at all — and
  it is a silent no-op, never an error.

  A failed send is a `warn!` and the boot continues. The daemon is fully
  functional; what failed is the init system's knowledge of it, which
  systemd's own `TimeoutStartSec` reports honestly. Killing a working
  supervisor over one undeliverable datagram would leave the flock down after
  a reboot instead of merely unannounced.

  `notify` is public because `shep-cli` names `NOTIFY_SOCKET_ENV` — the
  environment read belongs where every `SHEP_*` override is already read, and
  what crosses the crate boundary is the resolved address (see Changes).
- Answer `Request::Muster` on the control socket, over the same restore the
  daemon already runs at boot. `snapshot::muster` is now that one
  implementation: `boot::restore_flock` is a line over it, and the request
  handler calls it and turns the names it hands back into a listing. The
  restore that runs unattended after a reboot is therefore the one an operator
  exercises by hand, rather than a second path nobody has driven. It returns
  names instead of a listing so the boot caller has nothing to report and no
  reason to try.

  An app the flock already has is left where it stands, and is still counted
  as restored. Boot never meets that case, since its flock is empty by
  construction; an operator meets it whenever a muster follows a partial
  restore, or simply runs twice. Starting such an app again is not the no-op
  it looks like — `instance_slots` allocates the lowest FREE slot, so a second
  start of a one-instance app leaves it running two and the next roll records
  the pair. Restarting it would drop live connections over a verb that never
  claimed to be `restart`, and refusing the whole muster over it would break
  the partial-restore case the verb is most useful for. The unit of the rule
  is the app, not the instance count.
- Answer `Request::SaveRoll` on the control socket: the daemon writes the
  muster roll immediately, bypassing the debounce the snapshot writer
  otherwise applies, and reports back the path it wrote and how many apps
  that roll recorded — the count taken from the roll actually persisted, not
  from the flock before it was filtered down to one entry per app. `Ok(None)`
  — the supervisor engine has already stopped — answers `RpcErrorCode::Internal`
  rather than a success carrying nothing: an operator running this verb wants
  the roll on disk before a reboot, and a reply that said "saved" for a write
  that never happened is exactly the failure the verb exists to rule out.
  `RpcContext::save_roll_now` is the new entry point, returning
  `Option<SavedRoll>`; `RpcContext::snapshot_now` becomes a one-line wrapper
  over it that discards the count, keeping its own signature and its
  engine-stopped `Ok(())` behaviour unchanged, since `boot::run`'s teardown
  depends on both.
- Answer `Request::Trigger` on the control socket: the daemon resolves the
  selector against the flock, puts the action on each matched sheep's
  shepherd channel, and answers with one id-sorted `ActionReply` row per
  match carrying what that app said back. An empty match is `NotFound`, as it
  is for every other selector-in verb. `selector_call` (the helper those verbs
  share) is typed to `Vec<ProcessInfo>` and cannot carry `ActionReply`'s reply
  body, so this is its own small dispatch path rather than a forced fit.

  The waits run alongside each other — never one after another — so a whole
  flock costs whichever matched sheep's own `action_timeout` is longest,
  never the sum of them, and the answer fires only once the last of them has
  ended. A sheep that cannot take the action is refused in its own
  row and the rest are still asked: `NoChannel` when nothing is receiving on
  its channel — read off the channel itself, never off `channel = true`, so
  there is no second copy of that fact to disagree — and `Skipped` for a
  reload drainee, since both halves of a swap answer to the app's name and an
  answer from the process being replaced is worse than no answer. Both
  refusals are decided before any wait is armed, which is what keeps a refused
  sheep from leaving a wait nothing will ever resolve.

  A trigger no matched sheep could take is still a success — every match was
  found, and every row says why nothing was delivered to it — and the daemon
  logs a warning naming the action, because a whole request that delivered
  nothing is usually one misconfiguration repeated across a flock rather than
  a per-sheep surprise.

  What an app on the receiving end needs to know — the wire shapes in both
  directions, why it should reply even to an action name it does not
  recognize, and how a reply is matched to its trigger with no correlation
  id on the wire — is `docs/shepherd-channel.md`, not this entry.
- Add the reload state machine: the supervisor can replace each instance of an
  app with a fresh one, one instance at a time, so the app has a window in
  which it can stay reachable across the swap. A replacement registers under a
  **new id in the drainee's instance slot**, so an app deriving its port from
  `SHEP_INSTANCE` binds the same one; both entries coexist until the drainee
  exits, and the drainee's registration is removed with it rather than left
  behind as a dead row. The old instance is marked `stopping` before the
  replacement is spawned, which gives `ProcStatus::Stopping` its first writer
  and keeps a one-instance app from ever counting as two.

  An instance that is no longer replaceable when its turn comes is skipped and
  the reload carries on to the rest: one that is not `online`, and one already
  on its way out under a `stop`, a `restart` or a memory breach that claimed
  it before the reload arrived. The second kind still reads `online` — a kill
  ladder does not change the status while it runs — but a swap against it
  cannot survive, because the exit that ladder is about to produce would
  abandon the reload it was accepted into.

  **This is an overlap, not zero downtime, and the difference is the
  application's to close.** The old listener's accept backlog is reset when it
  closes — on both tier-1 platforms — so whatever was queued and not yet
  accepted is lost unless the app stops accepting, drains, and exits inside
  `graceful_timeout`. An app that ignores its stop signal until shep's
  `SIGKILL` drops that backlog on every single reload, and nothing shep does
  prevents it. **What that costs depends on the platform**, now measured
  rather than reasoned: Linux load-balances new connections across every
  listener sharing the port, so the instance being replaced keeps taking about
  half of them right up until it closes and a reload of a defiant app loses 5
  to 8 connections in every ~260; macOS gives every new connection to the last
  socket to bind, so the same app is handed nothing from the moment its
  replacement is up and the same reload loses none. Draining costs zero on
  both. Linux is where this is worth an operator's attention.

  Readiness is always gated for a replacement, even for an app that configures
  neither `wait_ready` nor `readiness_probe` — the heuristic wait exists for
  exactly this caller. A replacement that does not become ready inside
  `listen_timeout` **abandons the reload**: the instance being replaced goes
  back to serving, the instances the reload had not reached yet are left
  alone, and the replacement is killed through the stop ladder and
  deregistered. Abandoning protects the instance that can still serve, so it
  only happens while there is one — a replacement whose deadline elapses
  after the instance it was replacing has already gone on its own is taken
  `online` anyway, since killing it too would empty the instance slot
  outright. The drain itself runs under `graceful_timeout` (default
  8000ms) rather than `kill_timeout` (default 1600ms), which gives
  `graceful_timeout` its first reader in the daemon — `kill_timeout` already
  bounded every other stop.

  **Every swap is bounded by a deadline of the daemon's own**, five seconds
  past its two timeouts back to back (`listen_timeout` + `graceful_timeout`),
  and gives up when it expires. Without one, a reload could only ever end on a
  message from somewhere else — a readiness task's result, or a sheep's exit —
  and the kill ladder's wait after `SIGKILL` is unbounded, so a single
  instance wedged in uninterruptible sleep left the app answering `<name> is
  already being reloaded` until the daemon was restarted, and took `shep
  reload all` down with it because that refusal is whole-selector. Giving up
  early is cheap enough to make the margin this tight: an abandonment never
  ends an instance that is serving. Before the swap commits it puts the
  instance being replaced back and takes the replacement down, exactly as a
  readiness timeout does; after it, the replacement is the app's live instance
  and is left alone, and only the rest of the reload is lost.

  **A cron occurrence and a change under a watched tree are held off both
  halves of a swap that has not committed.** Both restart an app on the
  daemon's own initiative, and one landing on the instance being replaced — or
  on its replacement — abandons the reload and turns the deploy into the
  ordinary hard restart the overlap exists to avoid. For an app with `watch =
  true`, the one most likely to be reloaded at all, that was any save inside
  the readiness window. **The held-off trigger is dropped, not deferred**, and
  that is the price of holding the overlap: a save landing inside the window
  came after the replacement was spawned, so the replacement is not carrying
  it and nothing re-fires it, and that one instance keeps serving the older
  code until something else restarts it. A missed cron occurrence was never
  replayed either.

  A memory breach and a liveness failure never needed the hold. Both are
  refused against anything that is not `online`, which a drainee stops being
  before its replacement is spawned, and a replacement arms neither of them
  until it goes `online` itself. The hold ends at the commit rather than at
  the end of the reload: from there the replacement is the app's live
  instance, and a trigger against it gets the restart it would get an hour
  later, while the drainee is by then held by the drain's own claim on it.
  Instances of the app the reload has not reached yet are not half of any swap
  and are restarted as usual, and an operator's own `stop`/`restart`/`delete`
  still reaches either half and still wins — a reload is not a lock on the
  app.

  **Both halves of a swap write to one pair of log files**, because a sheep's
  log paths are derived from its name and its instance and the two entries
  share an instance slot. Every app is therefore a shared-log-path app for as
  long as a swap lasts, which until now took a `merge_logs` or an explicit
  `out_file` to arrange. `shep flush` already drew its barrier around the file
  rather than around the selector and needed nothing; **`shep reopen` now
  reaches every pump writing to a path it is rotating** instead of only the
  sheep the selector matched. Without that, an external rotator renaming a
  file mid-reload left the drainee appending to the renamed inode — the
  archive going on growing after the rotation meant to close it, while the
  recreated path took only the replacement's lines, and the `postrotate`
  stanza that waited for a zero exit was told the opposite. The same gap was
  open, and is now closed, for `shep reopen <one id>` against any app whose
  instances share a path. The reply is unchanged and still names the sheep the
  selector reached and no others; a failure, however, can now name a sheep the
  operator did not, which is the honest report of a shared file that could not
  be reopened.

  **The verb is answered on the control socket.** `Request::Reload` comes
  back as `Response::Reloading` the moment the reload is *accepted*, before
  the first replacement is spawned, carrying the matched sheep as they stood
  at that moment. That is forced rather than chosen: one instance costs a
  readiness wait plus a drain in the worst case, a client's budget is capped
  at 60s, and expiring a budget bounds the reply and not the actor's work —
  so a reply that waited for the swaps would routinely be abandoned while the
  reload it asked for went on running. Both refusals — a selector that
  reached an app already reloading, and a reload arriving after a shutdown
  has begun — answer `RpcErrorCode::Internal`, since that code set is
  versioned and neither refusal has one of its own. An app already reloading
  is the one an operator can act on, so its reply carries the
  `SupervisorError`'s own message, which names the app.

  **The swaps report themselves on the bus**, which an early reply makes the
  only account of them there is. Each swap puts a `process.reload` on the
  instance being replaced *before* its replacement's `process.start` — a
  second `start` in an instance slot that already holds a live entry explains
  nothing on its own — and a `process.reloaded` on the replacement once the
  instance it drained is gone, so the event means "the swap is over" rather
  than "the new one is up". **`process.reloaded` is owed to a replacement that
  is still serving**, not merely to one still registered: a replacement that
  goes down inside the drain window keeps its row in the flock, and announcing
  a swap off that row would name a process that is not there. A reload that
  gives up sends `process.reload_abandoned` instead, naming whichever instance
  the abandonment left holding the slot — the instance it gave up on
  replacing, which is the app's live one wherever going back to serving is
  still true, or the replacement itself where that is what went down. Read the
  status on the event rather than assuming. Every way a swap can fail reaches
  it: a replacement that could not be spawned at all, one that did not become
  ready inside `listen_timeout`, one that exited before it was ready — with or
  without the instance it was replacing still there — one that exited after
  taking the slot over but before the instance it replaced was gone, and an
  operator's own command reaching the instance being replaced while the swap
  was still abandonable. The one case that reports nothing is the one with
  nothing left to name: a replacement deleted outright while the instance it
  replaced was still draining, which is a warning in the daemon's log and no
  event. An instance the reload passed
  over — not `online` when its turn came, or already on its way out under
  something else — also produces none of the three, because no swap was ever
  attempted against it.
- Add `SupervisorError::ReloadInFlight`, carrying an app's name — a reload
  that reaches an app whose reload has not finished is refused whole rather
  than queued or partly accepted. **Breaking for anything matching
  exhaustively on `SupervisorError`**, which is not `#[non_exhaustive]` by
  deliberate choice.
- Add the cron-restart worker: one worker per name-group, restarting every
  instance of the name — stopped instances included — on its `cron_restart`
  schedule, the same reach the watch below has. The dialect is five-field
  standard cron in the app's `cron_timezone`, and the next
  occurrence is re-derived from the wall clock on every iteration rather than
  tracked across one long sleep, so a suspend or an NTP step costs at most one
  `max_cron_sleep` of drift. **A missed occurrence is not replayed**: a
  machine that slept through six hourly occurrences restarts once, at the
  next one, instead of firing six times in a burst on wake.
- Add the memory-limit enforcer: an app's `max_memory` ceiling is polled every
  15 seconds against the sheep's whole **process tree** — its own pid plus
  every lamb — and a breach restarts it. This deviates from pm2, which
  measures the root pid alone; an app that forks workers may therefore see
  restarts pm2 never gave it. A breach restart **resets the restart budget**,
  exactly as `shep restart` does — it does not merely skip its own increment,
  it also forgives every unstable exit counted before it — so a leaking app
  restarts indefinitely rather than reaching `errored`.
- Add liveness probes: `liveness_probe` polls a sheep over HTTP, TCP or a
  command on its `interval`, and restarts it once `failure_threshold`
  *consecutive* probes have failed. The HTTP client is hand-rolled and carries
  no TLS stack and no redirect following — a `3xx` is a failed probe, and an
  `https://` target is refused at config time by `shep-core` rather than
  failing every poll, since a probe that always fails is indistinguishable
  from an app that is down.
- Add the filesystem watch: an app with `watch = true` gets one watcher over
  its `cwd`, debounced by `watch_delay` (default 500ms), and a change under
  that tree restarts the app. A delivered path triggers when it matches the
  app's `watch_options` (or `**` when it names none) and does not match the
  ignores, so ignore always wins; dot-entries, `node_modules`, and shep's own
  `logs/` and `pids/` are in the ignores by default, and an app's
  `ignore_watch` extends those defaults rather than replacing them.
  `watch = true` without a `cwd` is refused at config time rather than arming
  nothing quietly — see `shep-core`'s entry for why defaulting to the daemon's
  own cwd was the worse of the two remaining options.

  **One thing escapes the globs entirely, and it is not a path.** When notify
  reports a *rescan* — it dropped events (an inotify queue overflow, an
  FSEvents `MustScanSubDirs`) and wants the tree re-read — the group restarts
  whatever either list says. A rescan means "unknown paths under here
  changed", not "this path changed", so no user pattern can be matched
  against it meaningfully; restarting is the conservative reading, and the
  alternative is a watch that goes quiet exactly when it knows least. It
  travels alongside the changed paths as notify's own flag rather than being
  inferred from them, because both available inferences are wrong: an empty
  path list is inotify's shape for a rescan and not macOS's, and a path equal
  to the watch root is macOS's shape for one *and* an ordinary event on that
  directory's own inode. A change reported at the watch root itself is
  therefore an ordinary event, filtered like any other — it changed nothing
  under the tree, so it restarts nothing.

  Two halves of the reach are worth stating together, because either alone
  misleads. **A triggering change restarts every instance of the name**,
  stopped instances included. **Stopping a sheep disarms its watch.** For a
  single-instance app that means total protection: `shep stop web` and no
  later save brings `web` back. For one instance of a multi-instance app it
  does not: `shep stop web-1` with `web-2` still running leaves the group's
  one watcher armed, so the next save restarts the whole name and `web-1`
  comes back up. Stop the group, or delete the instance.
- Add the extras registry that arms all four of the above when a sheep goes
  live and disarms them across every terminal transition, including the
  `Drop` that aborts every armed task when the supervisor itself goes away —
  covering both a graceful shutdown that never kills a `WaitingRestart` sheep
  and a panicking actor.
- Add the process-lifecycle engine: a `ProcessRunner` spawn seam with a real
  `tokio::process`-backed implementation (own process group, fd-3 shepherd
  channel, log capture) and a deterministic scripted fake for tests.
- Add spawn assembly (env, interpreter resolution, log paths), the restart
  brain (exit-outcome decision tree) and pinned-integer exponential backoff,
  and the kill ladder (message, signal, timeout, then `SIGKILL` on the whole
  process group).
- Add the supervisor actor: registers and spawns per-sheep tasks, routes
  `Start`/`Stop`/`Restart`/`Delete`/`Shutdown`, and resolves each app's
  `user`/`group` config to numeric uid/gid once per spawn (unix; refused
  outright elsewhere). Verified under a paused clock with a proptest over
  random command/exit interleavings (never two live pids per unit, restart
  count monotonic, always reaches steady state).
- Add the unix-socket control plane: `RpcServer` (same-uid peer-credential
  auth, a versioned handshake that refuses protocol skew with a typed error,
  per-call deadlines clamped server-side) and the portable `rpc::dispatch`
  it calls into, which never touches a socket or a byte.
- Add the daemon-wide event bus: `Subscribe` with server-side topic-glob
  filtering, a bounded per-subscriber queue that drops the oldest event and
  reports `Dropped { count }` rather than blocking the bus.
- Add the muster roll: debounced atomic `flock.json` writes (owner-only,
  `0600` — the one place this daemon persists an app's `env` to disk) and
  restart-survival restore that validates each entry independently instead
  of aborting the whole muster on one bad one.
- Add the daemon boot sequence: `0700` runtime layout (created at that mode
  directly, never chmod'ed after), an atomically-written pidfile, control-
  socket bind with stale-socket recovery, a readiness-pipe handshake for the
  CLI's `daemon` subcommand, SIGTERM/SIGINT/SIGQUIT graceful shutdown and
  SIGUSR2 log reopening (see below), and a load-bearing ordered teardown
  (roll saved before the flock is killed, or `shep muster` after a reboot
  restores nothing).
- The pure decision tiers (brain, backoff, assemble, entry, the `runner`
  trait and its fake) compile and test on every platform; the OS tier
  (real spawning, signals, the kill ladder, the socket itself) is unix-only.
- Report each sheep's resolved log paths on `ProcessInfo`. `ProcessEntry`
  now carries the `out_file`/`err_file` that `assemble` resolved for it,
  copied off the assembled `SpawnSpec` at registration rather than derived a
  second time, so the reported paths are by construction the ones the child
  is writing to — including when the app configured an explicit `out_file`
  pointing outside the log directory entirely.
- This crate's fifty-six `tracing` records now reach a reader. Nothing here
  installs a subscriber — that belongs to the binary, once per process, and a
  library that installed one would fail every test after the first — but the
  `shep` binary now does, at `warn` by default, so every warn-and-continue arm
  in this crate is output rather than a comment claiming output. The arms
  worth knowing about: `extras` reports a watch, a cron worker or a liveness
  probe it could not arm and lets the sheep come up `online` regardless;
  `supervisor`'s `Actor::handle_ready_result` reports a readiness deadline
  that elapsed, which is otherwise indistinguishable from a sheep that
  answered; and `boot`'s SIGUSR2 listener reports what a signal-driven reopen
  did, a signal having no reply channel to report it through. The count lives
  here and nowhere else: a copy of it in another crate's changelog goes stale
  on this crate's next commit, which is what happened to the one it replaces.
- Add `runner::LogCtl`, the request type a sheep's log pump takes mid-flight,
  and the first way anything has been able to reach the file handle that pump
  writes to. `Reopen` makes the pump flush, close and
  re-open both of a sheep's log files, then answer on a `oneshot`. That
  acknowledgement is the point of the shape rather than a nicety: a flag the
  pump would notice before its next write promises nothing about a sheep that
  has gone quiet, and an external rotator needs to know the swap has happened
  before it compresses or deletes what it renamed. The acknowledgement
  carries a `Result`: `Ok` means both old handles were flushed and closed AND
  both paths were opened again, while `runner::ReopenError` names the paths
  that could not be opened. Either answer clears the rotator to act on its
  rename, since the old handles are closed regardless — what the error adds
  is that the sheep has no file left to log that stream to. The child is not
  involved and never notices: it holds a pipe, and the daemon does the file
  I/O on the far side of it. Reaching a pump means holding the `ProcIo` field
  below.

  `Flush` is the second variant: it waits for every write already handed to
  the blocking pool to reach the file and keeps the handle, which is the
  half of `shep flush` that runs before anything is truncated. It answers
  with a `Result` too, where `LogFile::reopen` logs a flush failure and moves
  on because the handle it belongs to is being replaced by a working one.
  That result does not hold up the truncate — `poll_flush` drives the write
  already in flight to completion either way, so bytes it reports are bytes
  that errored, not bytes still racing anything — it changes the answer the
  operator gets, which is that a sheep could not write its log.
  `runner::FlushError` names the files either half of the verb could not
  deal with.
- Answer `Request::Reopen`: the supervisor keeps a clone of every running
  sheep's log-control sender and pushes a `LogCtl::Reopen` at every sheep
  writing to a path a matched sheep writes to, which is what makes
  `create`-mode rotation — rename the file, then ask — work at all. Until now
  the pump kept filling the renamed inode and the live path was never
  recreated, so `shep bleats --no-follow` printed nothing and exited 0 with
  no diagnostic; a restart was the only working reopen. The reach is keyed by
  the path rather than by the selector because what a rotator renamed is a
  file, and any writer left unasked goes on appending to the renamed inode —
  see the reload entry above for the case that forced the distinction. The
  reply stays selector-keyed and names the sheep the operator asked for and
  no others; a failure can name one they did not. That reply lands only once
  every pump reached has swapped both handles, so a `postrotate` stanza that
  waits for it knows nothing is still holding what it renamed. A matched
  sheep with no live pump is reported as a success rather than an error:
  there was nothing to reopen, which is not a failure worth failing `reopen
  all` over. A pump that answered and could not open a path again is the
  opposite case and fails the request (`SupervisorError::ReopenFailed`,
  `RpcErrorCode::Internal` on the wire), naming every such sheep and path —
  every pump is visited first, so one sheep whose log directory is gone
  neither stops the rest being reopened nor goes unreported. The
  acknowledgements are awaited on a task of their own and never inside the
  actor loop — an actor parked on one stops draining its mailbox, which
  stops the sheep task draining its logs, which stops the pump answering.
  Holding that clone costs the pump no life of its own: a pump ends when its
  `logs` receiver goes away as readily as when its last control sender does,
  and the sheep task lets go of both together. That is what retires the pump
  of a sheep whose child forked a lamb and left it holding the pipe — with
  neither stream ever reaching EOF, nothing else would.
- **SIGUSR2 now reopens every sheep's log files** — the same work
  `shep reopen all` does, reached without a socket. A signal carries no
  selector, so `all` is the only thing it can mean, and a `postrotate`
  stanza that would rather send a signal than run a client gets the same
  swap: every live pump closes both handles and opens both paths again.
  Installing the handler was already load-bearing on its own, because
  SIGUSR2's default disposition is to terminate — an unhandled `kill -USR2`
  kills the daemon instead of rotating it — and it is installed before the
  socket is bound, so there is no window where the daemon is reachable but
  the signal is still fatal. Two things the socket form gives that this one
  cannot: a signal has no reply, so the result is logged rather than
  reported and nothing can wait for the swap to finish; and it reaches the
  whole flock or nothing. The logged result is asymmetric on purpose — a
  failed reopen is a `warn` and so visible at the default level, while a
  successful one is an `info` the default `log_level = "warn"` filters out,
  since a routine success is not a warning. Confirming a signal-driven
  rotation worked therefore means running at `log_level = "info"`, which is
  why `SECURITY.md` recommends `shep reopen` in a `postrotate` stanza over
  `kill -USR2`: the command exits 9 naming the sheep and path, and the signal
  cannot report anything. A rotation that moved the log directory rather than
  the files is handled the same way it is for the socket form — by the pump,
  see the directory-mode entry below.
- Answer `Request::Flush`: every pump writing to a matched log path is sent a
  `LogCtl::Flush` and answers, and only then is each distinct recorded log
  path truncated. Both halves of that sentence are load-bearing. The flush
  comes first because `write_all` on a `tokio::fs::File` returns as soon as
  the real `write(2)` is queued, so a line already in flight would otherwise
  land at offset 0 of a file that had just been emptied — the one line that
  survives a flush, in the log its operator was told is empty. The barrier is
  drawn around the FILE and not around the selection, which is why a sheep
  the selector skipped is still flushed when it shares a path with one that
  matched: `shep flush 0` on a `merge_logs` app empties instance 1's live
  file, and an unflushed instance 1 is exactly the in-flight line above. The
  reply stays keyed by the selector — a row there means "a sheep you named",
  and what happened to the sibling is a fact about a path. And it is the
  RECORDED PATH that is truncated, never the inode the pump currently holds:
  after an external rotator's rename those name different files, and a flush
  that chased the handle would empty the archive and leave the live log
  untouched. Being path-based is also what lets a stopped sheep, which has no
  pump at all, be flushed — its logs are still readable with
  `shep bleats --no-follow`, so they are still worth emptying. Paths are
  deduplicated, so instances sharing one file under `merge_logs` truncate it
  once: one truncate empties the file for every `O_APPEND` handle open on it,
  and a second would only repeat work already done. A pump that could not
  land what it owed, or a path that could not be truncated, fails the request
  (`SupervisorError::FlushFailed`, `RpcErrorCode::Internal` on the wire)
  naming every such path — keyed by path rather than by sheep, since a shared
  path belongs to no single one. Every pump and path is visited first, so one
  unwritable file neither stops the rest being emptied nor goes unreported. A
  missing path is not a failure: a log file that is not there is
  already empty, and it is deliberately not created, which would otherwise
  leave a stray empty log wherever a rotator had just renamed one away. Like
  the reopen above, every await lives on a task of its own and never inside
  the actor loop.

### Fixes

- `shep serve`'s connection deadline is a config field rather than a constant
  read at the use site, so its test can wait one out on a real clock. The test
  previously paused the clock and raced tokio's auto-advance against live
  socket IO, failing about one run in three.

- A reply to a live trigger is no longer swallowed as a previous trigger's
  timeout debt when the app echoes the dispatch `id`.
- The stop ladder no longer clamps an unrecognized `kill_signal` to SIGTERM —
  `normalize` refuses it first.
- Let a child block on the shepherd channel. Every fd 3 handed to a child was
  non-blocking, and nothing meant it to be: `UnixStream::pair()` sets
  `O_NONBLOCK` on both ends for the sake of the daemon's own half, `into_std`
  leaves the flag exactly as it found it, and it then rode across the exec into
  the app. A child doing a plain blocking `read` on fd 3 got `EAGAIN` —
  "Resource temporarily unavailable" — rather than parking. What this broke is
  `shutdown_with_message`, which sends `{"kind":"shutdown"}` to a child that
  has been waiting since long before the message existed: that child never
  heard it. Runtimes with an event loop set their own descriptors non-blocking
  regardless and never noticed, which is how the flag survived this long; an
  app written to simply read did not. The daemon's end is a separate
  descriptor and keeps the flag it needs.
- Make `ScriptedRunner::spawn` (behind `test-fakes`) honour `spec.channel`
  instead of ignoring the whole spawn spec and wiring a live channel relay
  for every spawn regardless of what the config asked for. A `channel =
  false` spawn now drops both real channel ends immediately — the same
  shape `tokio_runner.rs`'s own `else` branch already has — with `FakeIo`'s
  `from_child_tx`/`to_child_rx` standing in already-closed rather than
  disappearing, so `io_handles` still never panics. Before this fix the fake
  and the real runner disagreed about the one fact Trigger (above) now
  treats as load-bearing: a `channel = false` sheep read as reachable under
  the fake and as `ActionOutcome::NoChannel` against a real child, so a test
  built only against the fake could not tell the two apart.
- Report an automatic restart as automatic. Every restart the daemon raised
  on its own — cron, watch, a memory breach or a liveness failure — emitted
  `BusEvent::Process { manually: true }`, whose documented meaning is "a
  user action caused it". A client using that flag to tell an operator's
  `shep restart` from the daemon acting alone was wrong on all four. This
  is a change on the wire, not only in the docs.
- Stop a watched sheep restarting forever on its own log writes. An app
  naming an explicit `out_file` or `err_file` under its own `cwd` put those
  files inside the tree its watch covered, so each startup line triggered
  the next restart. The default `**/logs/**` ignore never covered it: those
  globs are matched after the watch root is stripped, and the daemon's own
  log directory lies outside the app's `cwd` entirely. The assembled log
  paths are now derived into the watch's ignore set. The loop was
  self-sustaining and `max_restarts` could not stop it, because an
  automatic restart resets the restart budget.
- Send the kill ladder's graceful stop to the sheep's whole process group
  instead of its leader alone, so a wrapper script that forks a child without
  `exec`ing it (`thing & wait`) no longer leaves that child running, orphaned
  and untracked, once the wrapper exits on the signal. The escalated `SIGKILL`
  was already group-wide but only ran on timeout, which such a wrapper never
  reaches — it exits promptly. Lambs now also get a chance to shut down
  cleanly rather than only ever meeting `SIGKILL`.
- Create every runtime directory at `0700` directly via `DirBuilder::mode`
  instead of creating then `chmod`-ing, closing a TOCTOU window where a
  freshly created directory briefly sat at its umask-derived (potentially
  world-writable) mode. A sheep's log pump asks `mkdir` for the same mode
  when it opens or reopens a log file, so a rotation that moved the log
  DIRECTORY aside rather than the files gets it back at `0700` however the
  reopen was asked for — `shep reopen`, `SIGUSR2`, or the next spawn — rather
  than at whatever the umask allows. The pump is the only owner of that
  guarantee, which is also why an app whose `out_file` points outside the
  layout gets `0700` on any parent directory shep has to create for it.
- Adopt the CLI's inherited readiness descriptor as the first fd-touching
  statement in `boot`, before anything else opens or closes one of its own —
  closes an IO-safety hazard where a stale `SHEP_READY_FD` could land on a
  descriptor the daemon had since opened for itself (e.g. its own listener),
  closing it out from under `tokio` on drop.
- Stop a `Delete` racing a `Restart` from bypassing `pending_delete`: the
  caller was told a sheep was deleted while it kept respawning `Online` with
  a live control channel.
- Restart budget now errors at exactly `max_restarts`, not `max_restarts + 1`
  (spec §4).
- Let a `stop` or `delete` override an automatic restart that is already
  mid-kill-ladder. A memory breach, a liveness failure, a cron occurrence or a
  change under a watched tree claimed the sheep's next exit, so a `stop`
  arriving behind one was silently converted into a restart: the sheep came
  back up with `restarts: 1` and the `stop` caller was handed an `Online`
  snapshot of it. Two commands that each have an operator waiting on an answer
  still resolve first-command-wins, and an automatic restart still never
  displaces either. What a restart *does* is unchanged either way — an
  automatic one resets the restart budget exactly as `shep restart` does,
  whichever of the four raised it.

### Security and unsafe

- Refuse, under a shepherd running as root, to open a log file whose ancestry
  another local user could redirect — and warn about it, once per path, under
  any other. An ancestor is loose when it is owned by neither the daemon's own
  uid nor root, or when it is a world-writable directory. Ownership is the
  load-bearing half: it catches an intermediate component swapped for a
  symlink, which `O_NOFOLLOW` on the final component structurally cannot see,
  and it catches an ordinary `0755` directory owned by an app's own
  dropped-privilege `user`, which a write-bit test alone waves through. The
  split by uid is deliberate — a loose ancestry is an escalation only for a
  privileged daemon, and a developer logging to `/tmp` as themselves has
  handed nobody anything they could not already do, so refusing there would
  break a legitimate setup to no one's benefit. The sticky bit does not change
  the answer: it restricts unlinking and renaming entries you do not own, not
  creating new ones, and the attack plants a NEW entry at a path shep has not
  created yet. A TOCTOU window remains between the check and the open, and
  there is no portable way to close it while macOS is tier-1. The check costs
  one `lstat(2)` per path component (7.8 µs for a nine-component path,
  measured).
- Open every log file with `O_NOFOLLOW`, in both halves of the log plane:
  the pump's appending handle and the truncating one `shep flush` opens. An
  app's `out_file`/`err_file` are free-form config, so a log path can name a
  pre-existing directory shep neither created nor tightens — and there
  another local user could plant a symlink where the log file was going to
  be, have a root shepherd append the sheep's stdout through it, and have
  `shep flush` empty its target. Dropping privileges with `user`/`group`
  never helped, because log I/O never leaves the daemon, and the peer-cred
  check was never in the path, because the attacker never touches the socket.
  Both opens now fail instead, leaving the symlink and its target alone. The
  guard covers only the FINAL path component: a symlinked parent directory
  still resolves, and closing that needs `openat2(RESOLVE_NO_SYMLINKS)`,
  which is Linux-only and so out of scope while macOS is tier-1. `O_APPEND`
  rides alongside the new flag rather than being replaced by it — losing it
  brings back the sparse hole after every rotation. An operator whose log
  path legitimately IS a symlink is told so in those words, on the failure
  path each verb already has: `ELOOP`'s own wording ("too many levels of
  symbolic links") describes a loop they do not have.

### Changes

- Every listing comes back grouped by name: `Actor::snapshot_all` now sorts
  on `(name, instance, id)` instead of bare `id`. Sorting by id scattered a
  clustered app's instances across the table; sorting by name is what makes
  a four-instance app read as one thing at a glance. `instance` keeps a
  clustered app's slots in their own order once grouped, and `id` breaks the
  tie a reload creates, where a replacement takes the drainee's slot number
  with a fresh id.

  Applied once, in `snapshot_all`, because it is the single function every
  listing reply is built from — `ListFlock`, `Describe`, `Mustered`, and the
  muster roll's own `list_checked`. Sorting in the CLI instead would leave
  the metrics dog and bark reading a different order from the operator, and
  sorting in each verb would be four copies of one rule. Every other
  id-ordered reply (a `Reopen`, a `Flush`, a triggered action's rows) is
  unchanged — those build their own order off `matching_ids`, not off this
  function.

- `ProcessRss` gains a `cpu_ms: u64` field — accumulated CPU time in
  CPU-milliseconds, as the OS reports it, cumulative since the process
  started. **Breaking for anything outside this crate that implements
  `MemorySampler`**: the struct carries no `#[non_exhaustive]`, so every
  literal that builds one stops compiling until it names the new field.

  `SysinfoSampler` fills it from a refresh that now asks for CPU as well as
  memory. That flag is load-bearing and fails quietly without it: sysinfo
  populates the counter only under a CPU refresh and otherwise leaves it at
  zero, so a memory-only refresh yields a table of 900 processes with not one
  of them reporting any CPU time — and every percentage derived from it reads
  a plausible, wrong `0.0` rather than erroring.

- `BootOptions` gains a `notify_socket: Option<OsString>` field, carrying the
  address the readiness datagram above goes to; `None` — the ordinary case —
  reports nothing. Filed as a change rather than an addition for the same
  reason `max_cron_sleep` below is: the struct carries no
  `#[non_exhaustive]`, so any downstream literal naming every field stops
  compiling until it names this one (`..Default::default()` is unaffected).

  It carries the **resolved address** rather than a bool because a boot test
  could not otherwise observe the ordering it exists to guarantee:
  `std::env::set_var` is `unsafe` in edition 2024 and this crate is
  `#![deny(unsafe_code)]`, so no test here can establish an ambient
  `$NOTIFY_SOCKET` to watch against. The CLI reads the variable once, beside
  every `SHEP_*` override it already reads, and hands the value down.
- `ShepherdMessage::Action` gains a `params: Option<String>` field: the
  argument text an operator passes after an action name, handed to the child
  verbatim. The daemon never parses it, validates it, or holds a schema for
  it — an app that defines an action already has a grammar for that action's
  arguments, and a second grammar here would only be something for every app
  to either adopt or work around.

  **It is additive on a channel that has no version to bump.**
  `PROTOCOL_VERSION` governs the client↔daemon socket and not fd 3, so a
  shape change here reaches every app that speaks it with no handshake in
  which to negotiate one. `params` is skipped when it is `None` and reads
  back as `None` when absent, so `{"kind":"action","name":"gc"}` is
  byte-identical going out and unchanged coming in, and an app that ignores
  the key is unaffected. Committed fixtures pin both forms. Nothing sends a
  `params` yet — no verb reaches this message — and the field is here now
  because its cost rises the moment one deployed app or the `@shep/io` shim
  exists, which is the reasoning spec §9 now records.

  Filed as a change rather than an addition for the reason `ProcIo` below is:
  the variant carries no `#[non_exhaustive]`, so any
  `ShepherdMessage::Action` literal, or a pattern that names every field
  rather than ending in `..`, stops compiling until it names this one too.
- An app that configures `wait_ready` or a `readiness_probe` no longer reaches
  `online` at spawn. It holds at `starting` until the shepherd channel
  delivers `{"kind":"ready"}` or the first probe passes, whichever its config
  selects — `wait_ready` wins when both are set, since the channel is the app
  telling us directly and a probe is an outside guess at the same fact. Apps
  configuring neither are unaffected and still go `online` at spawn.

  No wire type changed, but the timing is visible to anything watching: a
  `shep flock` or `shep describe` issued right after `shep start` now reports
  `starting` for such an app, and the `online` transition arrives on the bus
  later than it used to. Scripts that started an app and immediately asserted
  `online` need to poll instead.

  On `listen_timeout` elapsing without a signal, the sheep goes `online`
  anyway: the daemon logs a warning, and that warning is the only thing
  telling a `starting` that ran long from one that answered — the status and
  the bus event are the same either way. Treating a slow start as a spawn
  failure would produce exactly the restart loop `max_restarts` exists to
  contain, out of an app that is slow rather than broken.
- `fake::ProcScript` (behind `test-fakes`) gains an `obeys_kill: bool` field
  and a `never_reports_its_exit()` constructor for a script whose `wait()`
  never resolves — the one child a kill ladder cannot end, wedged in
  uninterruptible sleep, where `SIGKILL` is delivered and the wait behind it
  never returns. Nothing else could put a test on what the supervisor does
  when a message it is waiting for never comes. Filed as a change rather than
  an addition for the same reason `BootOptions` below is: the struct carries
  no `#[non_exhaustive]`, so a downstream literal naming every field stops
  compiling until it names this one. Every existing constructor sets it
  `true`, which is what every real process does.
- `BootOptions` gains a `max_cron_sleep: Option<Duration>` field, carrying
  `[daemon] max_cron_sleep` from `shep.toml` to the cron workers; `None` means
  the crate-private default, applied by `boot` and nowhere else. Filed as a
  change rather than an addition because the struct carries no
  `#[non_exhaustive]`: any downstream struct literal that names every field
  stops compiling until it names this one too (`..Default::default()` is
  unaffected).
- `supervisor::Command` becomes `pub(crate)`, removing a public type. Same
  reasoning: it is `pub` in a `pub mod` and not `#[non_exhaustive]`, so every
  new subsystem's command was a semver break on a surface nobody consumes.
  `SupervisorHandle` is the only door into the actor, and nothing outside this
  crate names the enum.
- Most of this crate becomes `pub(crate)`, generalizing that one narrowing to
  the whole surface. The modules `backoff`, `brain`, `bus`, `cron`, `entry`,
  `extras`, `kill`, `server` and `watch` are no longer public at all, taking
  with them `Clock`/`SystemClock`/`spawn_cron_worker`, the entire `extras`
  surface, `WatchFilter`/`WatchSource`/`watch_tree` and their errors,
  `RpcServer`/`check_peer`/`daemon_uid`, `TopicFilter`/`spawn_forwarder`,
  `restart_delay`, `decide_on_exit`, `kill_process`, and `ProcessEntry` with
  its budget and reload types. Inside the modules that stay public, so do
  `MEMORY_POLL_INTERVAL`, `PollingEnforcer`, `LimitBreach`,
  `LivenessFailure`, `spawn_liveness_task`, `probes::os`, `probes::ready`
  (`ReadinessSource`, `Readiness`, `await_ready`), `privilege::resolve` and
  `PrivilegeError`, `SupervisorBuilder`, every `SupervisorHandle` method
  except `start`, `list` and `shutdown`,
  `dispatch`/`Outcome`/`budget` and both deadline constants,
  `RpcContext`'s fields, `FlockRegistry`, `write_atomic`, `restorable`,
  `SnapshotWriter` with both snapshot constants, and `boot`'s `init_dirs`,
  `read_pidfile`, `socket_path`, `bind_socket` and `DaemonReady`.

  The rule behind it: a dog is a separate process speaking the protocol, so
  what a dog author builds against is `shep-core`. Nothing needs to link this
  crate, and a `pub` item nobody links is not API — it is a semver
  obligation taken on by accident.

  What is left public is small, and each item now says in its own doc which
  consumer holds it open. `boot`, `tokio_runner` and `boot::DIR_MODE` are
  `shep-cli`'s; `runner`'s whole surface follows from `ProcessRunner` being
  the bound on `boot`. `limits::sample`, `LimitEnforcer` and `Prober` are held
  by the bench crate and by the external-implementor test that keeps those
  seams honest. `assemble`, `channel::ChildMessage`, `privilege::Credentials`,
  `snapshot::read`, `boot::pidfile` and `RunningDaemon::context` are held by
  integration tests, and `supervisor`'s remaining surface by the crate-root
  doc example, which rustdoc compiles as its own crate. `sys` and
  `READY_FD_ENV` stay public with no caller at all: both halves of the
  readiness handshake belong to a `shep-cli` `main` that is not written yet,
  and `adopt_fd`'s ordering precondition cannot be discharged from inside
  this crate.

  Doc links to the newly-private names became plain code spans rather than
  being deleted; in the crate-root taxonomy, a linked module name now means
  public and a backticked one means internal.
- `ProcIo` gains a `log_ctl: mpsc::Sender<LogCtl>` field: the control channel
  into a sheep's log pump, carrying the requests described under Additions
  above. Filed here rather than there because the struct carries no
  `#[non_exhaustive]`: any downstream `ProcIo` literal, or destructuring that
  names every field, stops compiling until it names this one too.

  Dropping the sender ends the pump, so a holder must keep it for as long as
  the child is alive. Ending the pump drops the read ends of the child's
  stdout and stderr along with it, and the child's next write to either then
  gets `EPIPE`/`SIGPIPE` — a dropped sender kills children, it does not
  merely stop collecting from them. A send that fails means the pump is
  already gone, which makes a reopen or a flush a no-op rather than an error.

  The real runner also spawns one pump task per sheep now instead of one per
  stream, so a single request covers both files and answers once.
- `DogError` is `#[non_exhaustive]`. Match on it with a wildcard arm.
- `NotifyError`, `BootError`, `SupervisorError`, `AuthError`, `ConnError`,
  `RunnerError`, `SysError`, `SnapshotError` and `BusError` are
  `#[non_exhaustive]`. Match on them with a wildcard arm.
- `channel::ChildMessage`/`ShepherdMessage` are re-exports of the shep-core
  types now. Same names, same wire, same imports.
