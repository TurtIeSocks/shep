# Deferred — what is not in v1.0, and why

The single list. Spec §2's "v1.1 committed" section names six items
deferred by design; everything else below is named as v1.0 scope in the
spec (§2, §3, §5, §6, §8, §9) but is not built as of the 2026-08-12
spec↔implementation audit (`feat/phase8-cutover` at `fc3679e`, 883 tests
passing locally, 1 ignored). A spec section is a plan, not a shipped-state
claim — drift between the two is what this file exists to stop hiding.
Linked from spec §2.

## Scope decision, 2026-08-12: everything below §2's six cuts ships in v1

The maintainer's call, after the five v1.1 audits came back: *"we should probably fix
everything in v1. We're not in a rush to release this to the public. We want
a hot looking app right off the bat if we have to compete with well
established apps like pm2 and other rust attempts."*

So this file now holds two different kinds of thing, and the section headings
say which is which. The six items under "Committed to v1.1+ by design" are
still deferred — they are scope cuts the spec argues for. Everything under
"Named as v1.0 in spec §2/§9, not yet built" is a **build queue**, in this
order:

1. **The audit debt** — what the five 2026-08-12 audits turned up. Real bugs
   first (`kill_signal` accepts a typo and then sends the wrong signal
   forever; an on-time `ActionReply` can be matched to the wrong request),
   then the wire and config asymmetries, then the tooling and doc staleness.
2. **The rest of the v1.0 surface** — serve, dev/runtime, `.js` Flockfile,
   schemars, the daemon-config flags layer, and openrc + BSD rc.d.
3. ~~**The Windows functional tier — last**~~ (the maintainer, 2026-08-12). **Superseded
   2026-08-15: Windows is out of v1 entirely** and moved to the v1.1+ section
   below. The estimate that was "mostly guesswork" has since been made, and it
   is what changed the decision — see
   [windows-estimate.md](windows-estimate.md).

**Dogs** (spec §8) was originally queued first and has since shipped, on
`feat/phase9-dogs`; see "Not deferred" below for what landed. **whistle**
(spec §8, §13) has since shipped too, on Phase 13; same section.

Ordering is not priority. Windows was last because its estimate was the
weakest; now that the estimate exists, it is out of v1 rather than at the end
of it.

## Committed to v1.1+ by design (spec §2)

Five deliberate scope cuts, not oversights — spec §2 carries the reasoning.
There were six until 2026-08-26, when the Windows tier stopped being one
of them: it shipped, and its entry moved to "Not deferred" below.

- HTTP/SSE MCP transport (whistle ships stdio-only first)
- cgroup v2 enforcement (`enforce = "kernel"`) — `LimitEnforcer`'s polling
  impl is the v1.0 tier
- `@shep/io` npm shim (built on demand)
- vcs metadata (`vcs` feature, off by default)
- `shep web` JSON status endpoint. Resolved, 2026-08-13: the metrics dog
  does not cover this — it serves Prometheus exposition text for a
  scraper, and `shep web` was a hand-fetched JSON payload for a
  dashboard, an incompatible shape for an incompatible consumer. This
  stays its own deferred item rather than being folded into the dog.

## Named as v1.0 in spec §2/§9, not yet built

Schedule rather than design is what leaves these open. Where a phase has
landed part of a spec section, the entry names the part still missing rather
than the whole section. See `docs/systematic-refactor/refactor-workspace/`
for what phase is next.

**OTLP export (metrics dog)** (spec §8) — the metrics dog serves
Prometheus exposition only; no `otel` cargo feature exists in
`crates/shep-cli/Cargo.toml`.

## Known debt, recorded rather than built

### The Windows shepherd channel has no DACL, and the random name is not one

`tokio_runner.rs` creates the channel pipe with the DEFAULT security
descriptor, which on Windows grants read to Everyone and restricts write, and
the `accept` that follows authenticates nobody. The 128 random bits in the
name were added against that, and they close less than the comment there
originally claimed.

**Prediction versus observation.** The nonce means a hostile local account
cannot park itself on a name it worked out in advance. It does not stop that
account WATCHING: the pipe namespace enumerates to any unprivileged local
user. Measured on Windows 10, from a non-elevated PowerShell,
`[System.IO.Directory]::GetFiles("//./pipe/")` returned 190 names. An account
polling that in a loop sees `shep-channel-<pid>-<n>-<nonce>` appear in the
window between `Listener::bind` and the child's own open, and can connect
first. What it gets is daemon-to-child frames plus a `wait_ready` sheep that
never goes online, so: disclosure of whatever an app puts on its channel, and
a local denial of service against startup.

**Why it is not fixed here.** A restrictive descriptor means
`ServerOptions::create_with_security_attributes_raw`, which takes a raw
pointer to a `SECURITY_ATTRIBUTES` this code would have to build, and
`shep-core` is `#![forbid(unsafe_code)]`. The transport seam would have to
grow a way to pass a descriptor down, and the descriptor itself would have to
be built behind `shep_daemon::sys_windows`, which is the only place on that
platform allowed to write `unsafe`. That is a real design change to the seam
rather than a one-line fix, which is why it is written down rather than
squeezed into the Windows tier's own pull request.

**Severity, honestly: this is recorded, not queued.** It needs an attacker
already running code as a DIFFERENT account on the same machine, winning a
millisecond race, against a Windows install running shep. The maintainer's read, and it
is the right one: a shep-on-Windows user who also has a hostile local account
on the same box is close to a nonexistent population, and the fix costs a
redesign of the transport seam plus new unsafe FFI. Do not spend that on
this. It is written down because the docs used to imply a random name was a
security boundary, and a false claim in published docs is worth correcting
whatever the exploit likelihood. The claim is fixed. The DACL is a someday.


### What the reload-readiness fix does NOT cover -- open, 2026-08-28

Three residuals, recorded because each was a deliberate stopping point rather
than an oversight.

**The post-drain probe is exact only for a single-instance app.** An
overlapping reload of a probed app (`reuse_port = true`) asks its probe a
second time once the drainee is reaped, because `SO_REUSEPORT` means the kernel
may hand either instance a connection and even a late probe can be answered by
the one on its way out. With one process left, an answer proves that process
answered. With a CLUSTER, a reload replaces one instance at a time, so the
surviving old instances are still in the group and can still answer for a bad
replacement until the last swap. Closing it needs a per-instance identity in
the probe response — the app naming which process it is — which is exactly what
`wait_ready` already provides and is why the code points a clustered app at the
channel instead of growing a second mechanism.

**A replacement that fails its readiness check keeps its process and loses its
lifecycle extras.** It is left registered and `Starting` rather than killed,
because with the drainee gone, killing it would empty the instance slot
outright. But extras are armed at `went_online`, so an instance parked this way
has no liveness loop and nothing will restart it on its own; the operator has
to act on the `process.reload_abandoned` event. Arming a liveness loop against
a process that is not `Online` is a wider change than this fix wanted —
`handle_extra_restart` guards on that status for four separate callers.

**A deploy tool's patience is sized off `listen_timeout + graceful_timeout`,
and a `reuse_port` reload now costs one more `listen_timeout` than that.**
shep-deploy derives its verify budget that way (`deploy.rs::budget`), so for
the one combination of `reuse_port = true` AND a probe, the post-drain check
can outlast the budget and roll back a healthy release. A false rollback rather
than a false success, which is the right direction to fail, but it is a real
interaction and the fix belongs on shep-deploy's side of the line.

### `ProcessInfo` fuses four concerns behind one discriminator

Identity and lifecycle (`id`, `name`, `status`, `pid`, `restarts`,
`uptime_ms`, `fold`), log paths (`out_file`, `err_file`), resource stats
(`cpu_percent`, `memory_bytes`) and dog provenance (`dog`) all ride in one
struct, and a dog's row leaves several of them meaningless.

Deferred on the wire audit's own recommendation: do not split speculatively.
What would force it is the `lambs` field — the moment a row carries a process
tree, the question of what a `FlockMember` is stops being cosmetic. Phase 10
made that field cheap to add (`ProcessInfo` is `#[non_exhaustive]` with a
builder), which is deliberately the opposite of forcing the split early.

### `check_log_ancestry`'s TOCTOU window, and the Linux syscall that would close it

`check_log_ancestry` verifies a log path's ancestry and `open_log_path` then
opens it, with no atomic tie between the two. The realistic local-multiuser
attack is caught — a loose or wrong-owned ancestor is refused, and
`O_NOFOLLOW` refuses a symlink standing at the final component — but an
attacker who can rearrange a directory between the check and the open still
wins that race.

The design, written down so it does not have to be rediscovered:

- Linux fast path: `nix::fcntl::openat2` (available under the `fs` feature this
  crate already enables) with `ResolveFlag::RESOLVE_NO_SYMLINKS`, opening
  relative to a directory fd for the log directory.
- The `RawFd` it returns is adopted into a `File` with `FromRawFd`, which is
  `unsafe`, so the wrapper lives in `shep-daemon/src/sys.rs` with a per-block
  `// SAFETY:` (IR-22/23) and nothing else in the crate touches the raw fd.
- Fallback ladder: `ENOSYS` (kernel < 5.6) and `EPERM` (seccomp filters that
  do not allow the syscall) both fall through to today's
  check-then-`O_NOFOLLOW`-open path, which stays as the portable
  implementation and remains the only path on macOS.

Not built in Phase 10 because it is new `unsafe` on a Linux-only path that this
project cannot execute a test for from a macOS development machine — the exact
shape of debt the platform audit's "never been compiled" finding exists to
complain about. What would force it: a Linux box in the regular test loop, or a
threat model that includes an attacker with write access to a log directory's
parent.

### `shep signal` cannot reach a sheep's lambs, on purpose

`signal` delivers to the sheep's own pid. An operator who wants a whole
process tree to get a `SIGHUP` — the nginx-worker shape — has no verb for it:
`stop` signals the group but also runs a kill ladder behind it, and there is
no group-wide nudge.

Deferred rather than built because the two are genuinely different asks and
one flag on `signal` (`--group`) would make the safe reading the non-default
one. What would force it: an app class where the sheep is a supervisor that
does not forward signals to its own workers, which is a real shape and simply
has not come up here yet.

### `UpDuration`'s grammar tops out at hours, which only bites outside shep

Found 2026-08-20 while building `shep-log-rotate`. Its `max_age` setting is a
log-retention window, so the natural spelling is `7d`. The grammar is
`^\d+(ms|h|m|s)?$` (`crates/shep-core/src/values.rs`), so `7d` is refused and a
week has to be written `168h`. A month is `720h`.

**For shep itself the grammar is right and should not change on this
argument.** Every duration shep owns is a lifecycle timer -- `min_uptime`,
`kill_timeout`, the backoff curve -- and nobody sets a kill timeout in days.
A day unit would be dead weight on every one of them. That is presumably why
it is not there, and it is a good reason.

What is new is that **dogs have durations shep does not**, and retention is
the obvious one: any rotator, any archiver, any bark-history trimmer wants
days. A dog that follows the ecosystem's spellings, as `shep-log-rotate`
deliberately does, inherits a grammar that was scoped for a different kind of
value. The alternative is a dog inventing its own duration parser, which is
the thing the ecosystem rule exists to prevent.

**The two halves carry different risk, and only one of them is breaking:**

- **Parsing `d` is purely additive.** `"7d"` errors today, so nothing that
  works now would change. `UpDuration` is `u64` milliseconds, so a day is
  `86_400_000` and even `49_710d` fits without overflow.
- **Rendering `d` is a wire-visible change.** `Display` currently walks
  hours, then minutes, then seconds, so a `d` arm placed before the hours arm
  would make `min_uptime = "24h"` round-trip as `"1d"`. That changes what
  `shep describe` and `--format json` print for configs nobody edited, and
  the type's own comment says "changing this is a breaking change (string
  form in `AppConfig`)".

So parse-only is available cheaply and asymmetrically, if it is wanted. Not
picked here: the grammar is the maintainer's, it is a wire decision, and the exercise's
job was to find the friction rather than resolve it.

### A third-party dog has no way to ship its own defaults

Raised 2026-08-20 while designing `shep-log-rotate`, the first fully external
dog. `shep adopt <name> <path>` vets, registers, enables and starts in one
command, and then the operator has an adopted dog with no `[dog.<name>]`
section and nothing telling them what its knobs are. The README is the only
answer today.

The maintainer asked whether a dog's repo could ship a `Flockfile.toml` that `shep adopt`
reads. It cannot: `RawFlockfile` is `deny_unknown_fields` over exactly
`$schema` and `app`, so a Flockfile cannot mention a dog. That is a
file-ownership line rather than an oversight. A Flockfile is the operator's
file, committed in a service's repo, describing what to supervise. `shep.toml`
is the daemon's, one per machine, holding dogs, style and interpreters. A dog's
registration belongs to the machine.

**Deferred deliberately.** The near-term answer costs shep nothing: a dog
prints its own commented defaults (`the-dog --print-config >> shep.toml`),
which is the same shape `shep init` gives a Flockfile, and it puts the
documentation of a dog's options where its defaults already live.

**The trust question is what any real design has to answer.** A `Dogfile.toml`
that `adopt` reads means shep parsing a file the dog's author wrote and merging
it into the operator's `shep.toml`. That is a larger step than "run this
binary", and `adopt`'s existing vetting ritual exists because this boundary is
taken seriously. Worth revisiting if a dog ecosystem appears; not worth
pre-building for one dog.

### `shep install` does not exist, and a scanner is not what would make it safe

Asked by the maintainer on 2026-08-26, the day the first external dog published.

**What shep offers today is discovery, not installation.** `shep dogs
--available` reads the community index and prints two copy-pasteable commands
per entry: `cargo install <package>`, then `shep adopt <name> <path>`. shep
runs the second and vets the binary before registering it. The operator runs
the first. Nothing in shep fetches, builds, or executes a stranger's code.

**The split is deliberate, and `dog_index.rs`'s module header already says
why.** That index is built from pull requests by strangers, and every string
in it is printed to a terminal, so the whole module is a security boundary:
it sanitises escapes because a `description` carrying `\u{1b}[2J` clears the
operator's screen, and because shep emits colour of its own, a well-placed
escape can imitate shep's own output with the reader unable to tell an
entry's bytes from shep's. An index whose strings cannot be trusted to
**print** is not one that should be trusted to **execute**. `shep install`
would turn "somebody added a row to a table" into "somebody chose what builds
and runs on this machine", which is a far larger step than the sanitiser
guards.

**The door is left open on purpose.** `DogSourceKind` is a tagged enum rather
than a freeform string exactly so this can be added later without asking every
past contributor to rewrite their entry: "how do I install this" and "what
artifact would shep fetch" are two questions that look like one field. Adding
the `cargo` kind on 2026-08-26 cost almost nothing, which is that decision
paying out.

**The obvious next move is a scanner, and it is the wrong one.** The tooling
is real and worth knowing. `cargo-audit` and `cargo-deny` check a dependency
tree against published advisories. `cargo-vet` and `cargo-crev` record that a
*human* reviewed a specific version, and `cargo-vet` can import Mozilla's and
Google's audit sets rather than starting from nothing. OpenSSF Scorecard
grades repository practice. Socket does behavioural analysis and reached
general availability for Rust in 2026, though its deep behavioural tier
remains JavaScript and Python only, Cargo sits in a shallower tier, and git
and path dependencies are unsupported.

**None of that helps here, because a dog's legitimate job is the suspicious
behaviour.** `shep-log-rotate` renames files, deletes files, writes archives,
and holds a socket to a privileged daemon. A hostile dog does the same things.
The capability profile of a benign dog and a malicious one is identical by
design. Behavioural analysis earns its keep when a capability contradicts a
stated purpose, a JSON parser that opens a network socket being the classic
case. A process-supervisor plugin that spawns processes and writes files
presents no contradiction to detect, so the scanner flags every real dog and
clears a hostile one that stays inside the same envelope.

**A verdict would be worse than no command at all.** `shep audit <dog>`
printing "no issues found" manufactures confidence about a question that is
not decidable in general, and people would install things they would
otherwise think twice about. A green check on an unanswerable question is a
liability, not a feature.

**What would have to be true first, in rough order of signal per unit of
work:**

1. **Provenance.** Does the crate come from the repository its index entry
   claims? crates.io supports trusted publishing and attestations, so this is
   a fact rather than a judgement, and it closes the likeliest real attack: a
   typosquat or a hijacked publish that has nothing to do with the repository
   anyone reviewed.
2. **A recorded human audit.** `cargo-vet` exists for precisely this and its
   importable audit sets mean shep would not be starting a web of trust from
   zero.
3. **Confinement rather than inspection.** The one worth most. shep already
   resolves `user`/`group` for sheep; running dogs with reduced privilege
   bounds the damage whatever the code turns out to do. Analysis is advisory,
   confinement is enforcement, and an hour spent on a scanner is an hour not
   spent on the control that actually holds.
4. **Pinned versions, and reading the diff on update.** Most real supply-chain
   attacks arrive as an update to something already trusted, not as a first
   install.

**If a command is ever built, it reports facts and never a verdict.**
Something nearer "14 dependencies, 3 with build scripts, published from the
repository it claims, no human audits on record, 2 known advisories" than a
tick or a cross, and it does not gate `shep install`, because gating on an
undecidable question is how the false confidence gets manufactured in the
first place.


### `cmd` on Windows cannot carry a quoted argument, and shep cannot fix it

A Flockfile app whose `script` is `cmd` and whose `args` contain double
quotes will fail to start on Windows. `std::process::Command`'s escaping and
`cmd`'s own parsing disagree, so the quotes arrive mangled and the command
never runs as written.

The workaround is to put the commands in a `.cmd` file and point `script` at
that. A `.cmd` file's contents go through no argument escaping at all.

Not shep's bug and not fixable in shep: the only lever is
`CommandExt::raw_arg`, which would mean shep guessing at quoting on the app
author's behalf. Recorded here because an operator who hits it has no way to
tell it apart from a shep bug.

The full postmortem, including the wrong diagnosis it took to get here, is in
[deferred-history.md](deferred-history.md).

### A config edit reaches nothing, and the warning about it is wrong

Found 2026-08-30, from the maintainer's question: an app running four
instances, `instances = 5` edited into the Flockfile, and no way to get the
fifth without restarting the other four.

For `instances` alone there is a way. `shep stock web 5` fills the lowest free
slot and leaves 0 through 3 running, writing the new count onto the stored
spec and into the muster roll. For every other field there is nothing.
`handle_reload` and the restart path both say so in as many words --
*"Nothing here re-reads configuration."* The only route is `shep delete`
followed by `shep start`, which restarts every instance.

`Request::ConfigDrift` closed half of this: an edit that will not apply is
reported rather than vanishing without a word. Applying it was left open
deliberately, in the code -- *"Whether `start` should reconcile by default, or
grow an `--update` flag, is the maintainer's call and neither is taken here."*

**The fields split three ways, and the first group is larger than "a config
change needs a restart" suggests.**

- **Read at decision time, so nothing need be restarted.** `autorestart`,
  `max_restarts`, `min_uptime`, `restart_delay`, `exp_backoff_restart_delay`
  and `stop_exit_codes` are read by `brain::decide` when a sheep exits;
  `kill_signal`, `kill_timeout` and `graceful_timeout` when a kill ladder
  runs; `max_memory`, `cron_restart`, `cron_timezone`, `watch` and the
  liveness probe when `extras` arms a worker, which it already does through
  `arm_instance`/`disarm_instance`. A write-back takes effect at the next such
  decision with no disruption at all.
- **Consumed at spawn, so they reach the next process rather than the running
  one.** `listen_timeout` and `readiness_probe`.
- **Baked into the child, so one instance swap each.** `script`, `args`,
  `cwd`, `interpreter`, `env`, `user`, `group`, `out_file`, `err_file`,
  `merge_logs`, `channel`, `stdin`, `wait_ready`.

**`shep stock` already proves every mechanism a wider verb needs**: normalize
before write, write-back onto the stored spec, partial-failure handling, and
muster-roll persistence. `AppConfig::drifted_fields` already computes which
fields moved. What is missing is the routing between the three groups.

**One part of this is a bug rather than a gap.** The drift warning tells the
operator that "`shep start` adds instances to a sheep the flock already has".
True of the daemon's `Request::Start`, false of the `shep start` an operator
types: the CLI sorts apps into resumed and fresh, and only fresh ones reach
that request. The sentence describes something no terminal can produce.

### The handover blob's compatibility tests never load an old blob

Raised in review on #84, 2026-08-31, and agreed rather than argued away.

Eight cases are named `a_blob_written_before_<field>_was_carried_still_loads`
-- stdin, the channel, `pending_delete`, the manual marker, a swap in flight,
`ready_failed`, the restart deadline, and (since phase 3 task 4) the dog
marker. Every one of them builds a blob from today's `Handover`, serialises
it, removes one key, and loads the result.

That proves the thing each field's carry actually needs: an absent key loads
as `None` rather than refusing, so a successor boots against a predecessor
that never wrote the field. It is not nothing.

**What it cannot prove is that a blob written by an older binary still
parses**, because it round-trips through today's serialiser. Rename a field
and every one of these follows the rename and keeps passing, while a real
blob on disk from v0.1.18 fails. IR-35 asks for exactly the missing half:
"committed byte-fixtures from the previous protocol version that must still
deserialize".

**One wrinkle stops this being a straight IR-35 application, and it needs
deciding rather than inferring.** The rule says fixtures from the PREVIOUS
protocol version, and the blob's `VERSION` has never moved from 1. Every
format worth pinning -- v0.1.18 through v0.1.21, and each phase's additions
inside them -- is version 1 with fewer optional fields. So the fixture policy
has to answer what a fixture is keyed to when the version does not move: a
release, a phase, or every shape that ever shipped.

Not done in #84 because it is not that PR's test. Covering only the newest
field would leave one case in a different style from seven siblings and barely
reduce the risk, since the exposure is the whole blob rather than any one
key.

### The bark dog still restarts once per reload -- open, 2026-08-31

Phase 3 carries every dog across the handover with no restart, and the
metrics dog is measured doing exactly that. **Bark is not**, and it is the
one thing G7 asks for that phase 3 did not deliver.

The mechanism, from task 1's measurement: bark's `EventStream` belongs to one
connection generation, so when that connection dies the stream ends,
`run_loop`'s `None` arm breaks the select loop, the dog exits 0, and
`autorestart` replaces it. Measured across two reloads: pid moving each time,
`restarts` 25 -> 26 -> 27, `online` after every one, while metrics held its
pid at `restarts 0`.

**What it costs, and what it does not.** The count is a false reading on the
one column an operator uses to decide whether a dog is unhealthy: twenty
reloads leave a perfectly healthy dog reporting `restarts 20`. It does NOT
risk an outage -- `install_adopted` gives every adopted entry a fresh
`RestartBudget`, so reloads cannot exhaust one -- and it is loud rather than
silent, which is the whole difference from the defect this phase was built
to fix. Bark also loses `rules::Rules`' per-subject debounce state across the
restart, so a sheep already alerted on can be alerted on twice.

**The fix is not "re-arm the stream inside the client".** Task 1 declined
that and the argument still holds: `ReconnectingClient::subscribe` re-arming
its own stream would silently swallow the gap between a connection dying and
the successor accepting a fresh `Subscribe`, and an event stream that hides a
gap is worse than one that ends.

**The fix belongs in bark, where the gap already has an answer.**
`run_loop`'s `Some(Err(dropped))` arm reconciles against `ListFlock` the
moment the bus reports a lag, on the reasoning that a drop carries no
information about what was lost and the only way to know is to ask the
shepherd what things look like now. A handover gap is the same class of loss
and deserves the same answer: re-subscribe, then reconcile. Nothing new is
invented; the state-based rules and the per-subject debounce are already
built for a subject seen twice by two routes.

**What stops it being small, and why it is its own task rather than a line
in phase 3 task 4:**

- `EventSource` needs a `resubscribe`, which means a production adapter
  holding the `ReconnectingClient` alongside the stream and its topics.
  `run_bark` moves that client into `ClientFlockSource` today.
- The adapter has to WAIT for the link to come back before it can subscribe:
  a `Subscribe` issued against a dead generation fails immediately with
  `Closed`. `ReconnectingClient` exposes `link()` as a reading, not a future
  to await, so this needs an API the type does not have.
- `LinkState::Refused` has to exit rather than retry, so G8's one restart
  from disk still applies to a bark dog that cannot speak this protocol.
- **And it needs a ruling on the ORPHANED dog, which is about every dog and
  not about bark.** Today bark exits when its shepherd goes away for any
  reason; a dog that re-subscribed instead would linger, and would attach
  itself to whatever shepherd next binds that socket -- beside that
  shepherd's
  own bark dog, double-alerting quietly. The metrics dog already has that
  hazard through `ReconnectingClient`'s own supervisor, which retries
  forever, and nobody has ruled on it.

The last of those is the reason this is deferred rather than squeezed in: the
question is what a dog does when its shepherd is gone, and answering it for
bark alone would leave two dogs answering it differently for the third time.

## Ideas, recorded but not designed

Not debt, not deferred spec surface, and not promised to anybody. Things worth
keeping because the reasoning behind them was expensive to arrive at and cheap
to lose.

### Per-sheep build and update scripts

The maintainer's, 2026-08-26, written down before it got lost:

> I would really love to add the ability for users to add build scripts to
> shep for each of their sheep. `shep build koji`,
> `shep build reactmap --restart`. Maybe even update scripts or git related
> scripts? `shep update koji` => git pull from cwd? It obviously doesn't make
> sense to mirror all of git's capabilities but just some basics of updating
> and building could go a loooong way in my experience. Do you know how many
> janky updating/deploying scripts I've written over the years?

**The last sentence is the evidence, and it is the strongest part.** "I keep
rewriting this by hand" beats any amount of design argument about whether a
feature is wanted. Anyone running a flock has written the pull-build-restart
script, and written it slightly differently each time.

**This revisits a documented non-goal, and the narrowing is the argument.**
[shep-v1.md](shep-v1.md) §1 cuts "deployment tooling" from v1, which is
`pm2 deploy`: host lists, revision directories, `ref` and `repo` per
environment, remote execution over SSH. What is described here is much
smaller. One machine, one checkout, the cwd a sheep already has, and two verbs
that stop well short of a deploy system. Reconsidering the cut is not the same
as reversing it, and a future design should say plainly which of pm2's deploy
surface it is still refusing.

**The sequencing is the actual feature, not the individual verbs.** `shep
build koji` on its own is a task runner with extra steps, and `just`, `make`
and npm scripts already exist. What replaces a janky script is pull, then
build, then restart, with the guarantee that a failed build does not restart
anything. The maintainer's own `--restart` flag already gestures at this. The
failure semantics are the whole value, so a design that ships the verbs
without deciding them has shipped the thin half.

Questions a design would have to answer, roughly in the order they bite:

- **Where do the scripts live.** The Flockfile is the natural home: per-app,
  committed beside the code, and it already carries `cwd`. `shep.toml` is
  daemon-scoped and would be the wrong shape for a per-sheep command.
- **Who executes them, the CLI or the shepherd.** This is the crux and it is
  the same trust question the `shep install` entry above works through. A
  shepherd that runs these is a long-lived privileged daemon executing
  arbitrary shell out of a config file. A CLI that runs them uses the
  operator's own shell and privileges, which is far easier to reason about,
  and costs nothing that matters while multi-host remains a non-goal.
- **Which directory.** Mostly already decided: Phase 17 resolved a
  Flockfile's relative `script` against the app's own directory rather than
  the daemon's cwd, and an app's `cwd` defaults to the Flockfile's directory.
  `shep build` should use the app's resolved `cwd` and inherit that ruling
  rather than inventing a second one.
- **What `shep update` actually promises.** `git pull` is where this stops
  being simple. A dirty worktree, a detached HEAD, the wrong branch,
  submodules, and authentication that depends on whose SSH keys are in scope
  are all ordinary states, not edge cases. A pull that fails halfway leaves a
  half-updated checkout that the operator then has to reason about anyway,
  which is the exact position the janky scripts leave them in. Refusing
  loudly on anything but a clean fast-forward is probably the honest v1 of
  this.
- **Whether there is a rollback.** The frightening part of a hand-rolled
  deploy script is usually that there is not one. shep should either offer
  something or say clearly that it does not, because an operator who assumes
  wrong finds out at the worst moment.

**If it turns out to be a thin wrapper, that is a reason not to build it.**
The value has to come from the integration: knowing the cwd, the environment,
the restart, and the ordering across a flock. A design that cannot point at
something `just` plus three lines of shell does not already do should stop
there.


### Shepherd-channel libraries for the languages apps are written in

The maintainer's, 2026-08-30, after asking whether an app could speak fd 3
using something that already exists. Wanted, in her own list: **node, go,
rust, python**.

**Today an app can, badly.** `shep_core::protocol::channel` exports
`ChildMessage`, `ShepherdMessage` and `CHANNEL_VERSION`, all serde-derived,
and shep-core is published. But it is a daemon's core rather than a client:
an app that wants two enums also gets toml, serde-saphyr, json5, regex,
tokio, croner, chrono, chrono-tz, globset, tempfile and nix. Hand-rolling
against [shepherd-channel.md](../shepherd-channel.md) is about forty lines
and the better trade, which is an odd thing to have to say about one's own
published crate. The other three languages have nothing at all.

**There is no example anywhere.** `examples/` holds seven Rust binaries and
four polyglot apps in Go, Node, Python and static HTML, and not one of them
speaks fd 3. The contract doc has two code blocks and one of them is JSON.
The wire is specified in prose and demonstrated nowhere, which is the real
barrier for anyone deciding whether to adopt it.

**Three things a hand-roll gets wrong**, each named in `channel.rs`'s own
module doc: an app must reply to a `ShepherdMessage::Action` even when it
does not recognise the name; it should echo the `id` so the reply is matched
to its exact trigger, and the name-and-order fallback costs something when
it does not; and there is a `params` quoting gap. Those are what a library
encodes once and prose asks every author to get right separately. Windows is
a fourth, since fd 3 is a named pipe there and every client needs two arms.

**Sequencing, offered as a recommendation rather than a decision.** Working
examples first, because the repository already has an app in every one of
the four languages: Rust under `examples/src/bin/`, and Go, Node and Python
under `examples/polyglot/`. Teaching those four to speak fd 3 is a small
diff against files that already exist, and it gives every community app
something to copy. A library second, for whichever language earns one. The
four chosen are also the ones the surrounding ecosystem is written in, so
the examples serve the whole audience on their own.

## A readiness probe cannot verify a reload's replacement

Found on 2026-08-28 by deploying real repositories with shep-deploy against a
real shepherd, not by reading code. Two experiments, same broken release, same
app, differing only in whether the old instance was answering:

| old instance answering the probe | outcome |
|---|---|
| yes | the broken release was verified, recorded as deployed, exit 0, app down |
| no | the broken release was correctly refused and rolled back, exit 12 |

The second row is the machinery working. The first is the defect, and the
mechanism is exact. `spawn_replacement` derives `ReadinessSource::of(config)`,
which is `Probe` for an app with a `readiness_probe`. `await_ready`'s `Probe`
arm probes immediately, and says why in its own comment: "the first probe must
land at t=0, not after one `interval`". At t=0 the drainee is still listening
on the address the probe names, because draining is what `reload_ready_result`
does AFTER readiness resolves. So the old instance answers, the replacement is
declared `Ready`, the old is drained, and the address goes dead.

Timed, with a `listen_timeout` of ten seconds:

```text
t+0.0s  probe answers YES  status=online  pid=7349   the old release
t+1.0s  probe answers no   status=online  pid=7745   the new one, already Online
t+25.8s probe answers no   status=online  pid=7745
```

**This is not a bug in one line, it is a property of the arrangement.** An
address probe cannot say which of two processes answered it, and an overlapping
reload exists precisely to have two. `SO_REUSEPORT`, which is what makes the
overlap zero-downtime in the first place, makes it worse rather than better:
the kernel balances across both sockets, so even a probe that arrives late may
be answered by the instance on its way out.

What it costs downstream: `shep-deploy`'s `verify = "probed"` watches for a new
process reaching `Online`. The replacement gets there on the drainee's answer,
so it is marked `Ready` and then `Online` while never having served anything
itself, and the deploy that was waiting on exactly that signal records success.
The release is left in place, serving nothing. That crate's README now says
what its verification actually checks and points here.

**The workaround that exists today is `wait_ready = true`.** It selects
`ReadinessSource::Channel`, and the channel is per instance: `spawn_readiness_task`
creates a fresh `oneshot` per spawned id, so the drainee has no way to answer
the replacement's. An app that can write `{"kind":"ready"}` on its shepherd
channel therefore gets a reload gate that means what it says, today, with none
of the work below. An app that cannot is the case this entry is about.

### The options, none of them free

1. **Probe after the drain, not before.** Keeps the overlap, adds a second
   readiness check once the drainee is gone, and reports the reload failed if
   it does not pass. Does not restore the old instance, which is already gone
   by then, but it tells the truth, and a deploy dog with the previous release
   still on disk can act on it. The most honest of the three and the most work.
2. **Prefer the channel for a gated reload.** `wait_ready` is per process and
   cannot be answered by the drainee. This means telling operators that a probe
   does not gate a reload and `wait_ready` is what does, which is a real
   demotion of the feature most apps can actually use.
3. **Serialise: drain, then start, then probe.** Correct verification, no
   overlap, no zero-downtime reload. Not worth it.

Recommendation is 1, with the doc change from 2 alongside it, because even
after 1 the pre-drain probe pass remains meaningless and should stop being
described as verification.

Not built here because it changes what a reload promises, which is the maintainer's call
rather than a fix to apply on the way past.

## Closed entries live next door

Everything this file used to carry that is now FIXED, STALE, resolved or
rejected, plus the record of what shipped instead of being deferred, moved to
[deferred-history.md](deferred-history.md) on 2026-08-29. That is 1182 lines
against the 523 left here, and a reader had to get past all of it to reach
the 9 entries under "Known debt" that are actually still open.

This file answers "what is not built". That one answers "what was not built,
and what happened to it".
