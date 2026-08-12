# Phase 9 — dogs: the contract, the metrics dog, and the bark dog

**Goal:** ship the plugin surface spec §8 promises, together with both dogs
that use it, so the contract is validated by two consumers before anything
external can bind to it.

**Why both dogs in one phase.** A plugin contract exercised by a single
consumer is not validated — the second consumer is what finds the wrong
abstraction. Metrics and bark are genuinely different: metrics polls and
serves, bark subscribes and writes state. Splitting them across phases would
mean the contract hardens after the first, and the second either bends to fit
or forces a rework that a later session is likely to resist rather than
perform. Building both at once removes that.

## The contract

**A dog is a process speaking the client wire protocol, supervised by the
daemon, marked as a dog.** Spec §8 already settled this; it means there is no
second protocol to design. `PROTOCOL_VERSION` covers a dog exactly as it
covers `shep flock`.

- **First-party dogs are argv branches of the same binary** — `shep dog
  metrics`, `shep dog bark` — the multi-call pattern the hidden `daemon`
  subcommand already uses.
- **Third-party dogs are any binary**, brought in with
  `shep adopt <name> --exec <path>` and dropped with `shep rehome <name>`,
  running at the daemon's own trust level. They get no sandboxing beyond that,
  which is the same trust a sheep already has, and is stated rather than
  implied.

`adopt` is deliberately not `enable --exec`. Turning on a dog that already
ships inside the binary and vetting a binary shep has never seen are different
acts with different failure modes — a missing path, a file that is not
executable, the wrong architecture — and one verb carrying both hides that.
`enable`/`disable` stay for first-party dogs; `adopt`/`rehome` bring an
outside one in and let it go. `enable --exec` survives as a hidden alias,
because it is what someone arriving from pm2 would try first.

### Configuration travels over the socket, never the environment

A dog inherits exactly one variable, `$SHEP_HOME`, which is how every client
already locates the socket. It connects, handshakes, and sends
`Request::DogConfig { name }`; the daemon replies with that dog's
`[dog.<name>]` section.

**The reason is secrets.** Bark's sinks are Discord and Slack webhook URLs.
The environment is readable from the process table on some systems, is
inherited by every child the dog spawns, and is captured into crash dumps.
`SECURITY.md` already has to disclose `flock.json` carrying cleartext env;
this design declines to widen that surface.

pm2's own answer was the opposite — `~/.pm2/module_conf.json` merged into the
module's environment by `extendExtraConfig` — and the trace of it records
three sharp edges: read-whole-file/write-whole-file with no locking, so
concurrent sets lose one; unset expressed as the literal string `'null'`, so a
value that legitimately is `"null"` cannot be written; and a prototype-
pollution fix in `splitKey`. None of those transfer to Rust unchanged, but
together they are a reason to choose deliberately rather than by precedent.

The reply is an **opaque blob the dog parses**, not a typed shep structure. A
third-party dog binds to the shape of its own section, not to our config
model, our file discovery, or our layering rules — so changing any of those
cannot break a dog nobody has seen.

The objection that a dog can then do nothing until it connects dissolves on
inspection: a dog's entire purpose is talking to the daemon. Metrics polls it,
bark subscribes to it. Neither has useful work before the connection exists.

## Lifecycle

`shep enable <name>` (or `shep adopt <name> --exec <path>` for an outside
binary) writes `[dog.<name>]` into `shep.toml` and records the
dog as enabled. `shep disable <name>` removes it and stops the process.
Enabled dogs are spawned when the daemon boots.

**`enable` starts the dog immediately when a daemon is running**, rather than
only arming it for the next boot — an operator who enables a dog and sees
nothing happen has been given a puzzle rather than a feature. With no daemon
running it writes the config and says so, and the dog comes up with the next
boot.

**A config change does not reach a running dog by itself.** The dog read its
section once, at connect. Changing `[dog.bark.rules]` and expecting live rules
would need either a push the contract does not have or a poll every dog would
have to implement. So the rule is stated rather than left to be discovered:
`shep disable <name> && shep enable <name>` re-reads it, and the docs say so.
Live config push is a v1.1 question, and one worth answering with two dogs'
worth of evidence about what actually changes at runtime.

Spawning is the **ordinary** path: the same runner, kill ladder, restart
budget and log pumps a sheep gets. A marker on the process entry keeps dogs
out of `shep flock` unless `--all` is passed, and **badges them when they are
shown**, so a dog in a listing is never mistaken for one of the operator's own
processes. `shep dogs` lists them directly.

This is deliberately a marker rather than a second registry. Duplicating
supervision would mean teaching every feature since Phase 2 — reload, watch,
cron, limits, the log plane, the muster roll — about a second population, or
excluding it from each. The risk this accepts is a `dog` flag accumulating
special cases until it is a second system wearing the first one's clothes;
**if `dog` starts appearing in match arms across `supervisor.rs`, that is the
signal the choice was wrong**, and it is worth saying so now while it is
cheap to notice.

## The metrics dog

Prometheus exposition on `127.0.0.1:9615`, configurable. **Loopback by
default; binding wider is explicit.**

Per sheep: cpu, memory, `restart_total`, status, uptime — all already carried
by `ProcessInfo`, which gained cpu and memory in the cutover phase. Plus
daemon self-metrics, host metrics, and **a health gauge per enabled dog**.

Reference Grafana dashboard JSON ships in `assets/grafana/`, the directory
spec §12 promises and which does not yet exist.

## The bark dog

**Subscribes `process.*`, and polls `Request::Flock` as reconciliation.** The
bus is a tokio broadcast channel: a lagging subscriber has events *dropped*,
not queued. For `shep bleats` that is a cosmetic notice; for alerting it is a
missed page. The subscription makes bark fast, the poll makes it correct.

**Bark reads `ProcessInfo.restarts` — the daemon's own count — rather than
tallying bus events.** A private tally would drift from the number the daemon
acts on, and the operator would be told a different story from the one the
supervisor believes.

**Sinks** are named entries under `[dog.bark.sinks]`: Discord webhook, Slack
webhook, and a generic JSON POST with a templated body. **Rules** live under
`[dog.bark.rules]`; each names event kinds or a condition, carries its own
debounce, and **routes to one or more named sinks**.

Restart-loop detection is **two rule kinds, not one threshold**:

- **"the daemon gave up"** — keyed to budget exhaustion, which is already an
  event. On by default, nothing to tune, cannot disagree with the daemon, and
  is the alert that must not be missed: the app is down and staying down.
- **an early warning** — a tunable "N restarts in M seconds" that fires while
  the app is still coming back. Opt-in, because it is the one that pages at
  3am for a blip, and the threshold should be one the operator chose.

Fired alerts append to `$SHEP_HOME/barks.jsonl`, a **size-capped ring**
(oldest out), which is the data source for `shep barks` and later for the
whistle's `list_barks`.

## When a dog dies

A dog is supervised, so a crash is restarted and a crash-loop exhausts its
budget and lands `Errored`. At that point shep has stopped alerting and the
thing that would say so is the thing that is down.

Two answers, together:

- **The daemon records it.** When an enabled dog exhausts its budget the
  daemon appends to `barks.jsonl` and logs loudly. It cannot *deliver* the
  alert — it has no sinks and no webhook code — but there is always a local
  trail.
- **Metrics exposes dog health**, so alerting that lives outside shep can page
  on it. "Is the monitoring up" is the one question monitoring cannot answer
  about itself, which is why the answer belongs outside.

**Explicitly not cross-dog watching.** Two dogs observing each other reads as
rigorous and adds a failure mode without adding an independent observer — it
fails hardest when both go down together, which is the most likely way they go
down at all.

## Testing

- **Sinks are HTTP**, so a local test server. Never a real webhook.
- **`barks.jsonl`'s eviction is tested, not just its append.** A ring whose cap
  is never reached in a test is an append-only file with extra code.
- **The reconciliation path needs a test where the bus actually drops events**
  and the poll catches up. That is the property bark exists for, and it will
  not happen by accident — a test that merely subscribes and sees events
  proves the fast path, which was never the risk.
- Dog supervision reuses tested machinery; what is new is the marker, the
  listing filter, `DogConfig`, and each dog's own logic.

## Assumptions

- `Request::DogConfig` returns an opaque blob; a dog parses its own shape.
- Metrics binds loopback only unless configured otherwise.
- `barks.jsonl` caps by size with oldest-out, matching the log plane's model.
- Third-party dogs run at the daemon's trust level with no added sandboxing.
- The dog marker is a field on the existing entry, not a separate registry.
