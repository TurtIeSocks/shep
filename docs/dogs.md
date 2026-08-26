# Dogs — the shepherd's own plugins

A dog is a process the shepherd supervises for its own sake, not for
yours: it watches the flock rather than being part of it. This is the
practical guide to turning one on, configuring it, and — if you want to
write your own — the contract it has to speak. The behavior contract
itself is pinned in spec [§8](specs/shep-v1.md#8-dogs-plugins);
`docs/shepherd-channel.md` is its sibling for the OTHER kind of process a
shep app can be, and the two are worth telling apart up front: a sheep
that opens the shepherd channel talks to the daemon over fd 3 in newline
JSON. A dog talks to the daemon the way `shep` itself does — as a client,
over the Unix socket.

## What a dog is

A dog is a process speaking the client wire protocol (spec
[§6](specs/shep-v1.md#6-wire-protocol-v1--protocol-version-1)), connected
to `$SHEP_HOME/run/shep.sock` and handshaking exactly as `shep flock` or
`shep describe` would. `PROTOCOL_VERSION` covers a dog exactly as it
covers every other client: a version mismatch is a typed error at
handshake, not silence.

Two dogs ship inside the `shep` binary — `metrics` and `bark` — reached
through the hidden `shep dog <name>` re-exec target, the same shape
`shep daemon` already is. Neither is something you run directly; the
shepherd spawns it.

Underneath, a dog is not a second kind of supervised process. It is an
ordinary sheep with a marker on its entry saying where it came from
(built-in, or an adopted binary). The kill ladder, the backoff curve, the
restart budget, what `Errored` means — none of it branches on whether the
entry is a dog. A dog that restarts is still a dog when it comes back; a
dog whose binary cannot be spawned still shows up in `shep dogs` as
`Errored`, the same way a broken sheep would. `shep flock` prints the
flock as two tables — sheep, then dogs — and `shep dogs` prints the
second one alone. A wildcard selector (`stop all`, `reload all`, `/regex/`)
never touches a dog; naming it exactly always does, which is what makes
`shep disable bark` reach a dog while a sweep of everything else leaves it
running.

## Turning one on

```
shep enable metrics
shep enable bark
shep dogs
shep disable bark
```

`shep enable <name>` writes `$SHEP_HOME/shep.toml` first, and only then —
if a shepherd is reachable — asks it to start the dog. Run it against a
live shepherd and the dog starts now. Run it with nothing listening and
`enable` still exits successfully: the config says the dog belongs, and
the next shepherd that boots brings it up. Neither `enable` nor `disable`
starts a shepherd on your behalf to act on its own edit — that would be a
bigger side effect than a config change should cause, and `shep muster` is
the one verb in this CLI that autostarts anything.

`shep disable <name>` is the mirror: it stops the dog if one is running,
and removes it from the boot list either way.

Running two of these at once is safe. A provisioning script that
backgrounds `shep adopt` and `shep enable` together has each of them take
an exclusive lock on `shep.toml` for its whole read-edit-write, so the
second waits its turn instead of writing back a document it read before
the first one's edit landed.

## Configuration

A dog's settings live under `[dog.<name>]` in `shep.toml`, and they never
travel through the dog's environment. Instead the dog connects to the
socket and asks for its own section over the wire
(`Request::DogConfig`) — the daemon reads the file fresh for every such
request rather than serving a copy it cached at boot.

**Editing `[dog.<name>]` in `shep.toml` does not reach a dog that is
already running.** A running dog asked for its section once, at startup,
and nothing pushes it a new one. `shep disable <name> && shep enable
<name>` is what re-reads it: that stop/start cycle is a fresh process
asking a fresh question, and the daemon answers off the file as it stands
at that moment.

The reason the section rides the socket instead of the environment is the
same reason it applies to every dog and not just the ones with obvious
secrets: an environment variable is readable from the process table,
inherited by every child the dog spawns, and captured into a crash dump.
`[dog.bark.sinks]` routinely holds a webhook URL, and a webhook URL is a
bearer credential — keeping the whole section off the environment means
nobody has to remember which dog's config is sensitive.

## The metrics dog

`shep enable metrics` serves Prometheus exposition text over plain HTTP,
bound to `127.0.0.1:9615` by default:

```toml
[dog.metrics]
bind = "127.0.0.1:9615"
```

Nothing here accumulates between scrapes — every request rebuilds the
reading from a fresh `ListFlock` and a fresh host sample, so there is no
stale-cache window and no state a slow scraper could hold open.

| Metric | Meaning |
|---|---|
| `shep_sheep_cpu_percent` | Tree CPU as a percentage of one core, over the last sampling window. |
| `shep_sheep_memory_bytes` | Tree resident set size in bytes. |
| `shep_sheep_restart_total` | Restart count since registration. |
| `shep_sheep_uptime_seconds` | Seconds since this sheep's last successful start. |
| `shep_sheep_status` | 1 for the status this sheep is currently in, 0 for every other status. |
| `shep_dog_up` | 1 when this dog is online, 0 otherwise. |
| `shep_daemon_up` | Always 1: the scrape reached the shepherd. |
| `shep_daemon_pid` | The shepherd's own pid, so a restart is visible as a step change. |
| `shep_host_memory_total_bytes` | Total physical memory on the host. |
| `shep_host_memory_used_bytes` | Memory in use on the host, as the platform reports it. |
| `shep_host_processes` | Number of processes running on the host, the flock included. |
| `shep_host_uptime_seconds` | Seconds since the host booted. |

A reference Grafana dashboard built against this exact metric set ships
in `assets/grafana/`.

The default bind is loopback on purpose, not an oversight a wider one
would fix: every series above carries a sheep's name as a label, and on
plenty of hosts a sheep's name is the name of an internal service.
Widening `bind` to `0.0.0.0` (or any non-loopback address) is one config
line away when you want it, but the metrics dog will not make that
decision for you by shipping wide and asking you to lock it down.

## The bark dog

`shep enable bark` subscribes to the shepherd's event bus and watches for
the things worth paging someone about, delivering to named webhook sinks:

```toml
[dog.bark.sinks]
oncall = { kind = "discord", url = "https://discord.com/api/webhooks/..." }
audit = { kind = "json", url = "https://example.internal/hook" }

[[dog.bark.rules]]
on = "gave_up"
sinks = ["oncall", "audit"]

[[dog.bark.rules]]
on = "restart_rate"
restarts = 5
within = "2m"
sinks = ["oncall"]
```

Leave `[dog.bark.rules]` out entirely and the bark dog does not stay
silent — one rule is built in by default, firing on every configured sink
whenever a sheep reaches `Errored`. That is deliberate: it is the alert
that must not be missed, keyed to the shepherd's own decision that it has
given up on a sheep rather than to a threshold the bark dog chose for
itself, so it needs no opt-in.

A second restart-related rule exists and is opt-in for the opposite
reason: `restart_rate` (`N` restarts within a window) catches the early
warning — a sheep flapping but not yet `Errored` — which is exactly the
kind of thing that pages someone at 3am for a blip if an operator did not
choose the threshold themselves. Two rules, two different relationships
to configuration, because they answer two different questions: "did the
shepherd give up" needs none, "is this restarting too often for my taste"
needs yours.

Every rule debounces **per subject**, five minutes by default, never
globally — a global debounce would mean the second sheep to go down
during an incident goes silent, and that is usually the incident's most
interesting fact.

**The bus drops events under load, and the bark dog is built to survive
that rather than pretend it doesn't happen.** `tokio::sync::broadcast`
discards what a lagging subscriber cannot keep up with; on top of
listening to the bus, the bark dog polls the flock's current state on a
timer (30s by default) and evaluates the same rules against that
snapshot. A dropped frame triggers an immediate poll instead of waiting
for the next scheduled one. The two routes share one debounce record per
subject, so an `Errored` seen by both the bus and the very next poll
fires once, not twice.

Every fired alert is appended to `$SHEP_HOME/barks.jsonl`, a byte-capped
ring (1MiB by default, `history_bytes`) that evicts the oldest record
first and is the data `shep barks` reads. Two different processes append
to it — the bark dog when a rule fires, and the shepherd itself when an
enabled dog exhausts its own restart budget (below) — so it is written
through an advisory file lock, never truncated in place.

## Writing your own

Third-party extension is `shep adopt <path> [--name <name>]`:

```
shep adopt ./bin/my-watchdog --name watchdog
shep enable watchdog
shep rehome watchdog
```

`--name` is optional. Leave it off and the dog is named after its own
file stem with a leading `shep-` stripped, the way `cargo` strips
`cargo-` from its own external subcommands — `shep adopt
~/.cargo/bin/shep-log-rotate` alone names itself `log-rotate`. The path
itself can be given as-is, with a leading `~/`, or as a bare name already
on `$PATH` (which is where `cargo install`/`go install` put it).

`adopt` vets the binary once, at the moment you run it — not again at
every later boot or every `enable`. It refuses a path that does not
exist, is not a file, has no execute bit for anyone, or is world-writable
(the binary itself or its containing directory); it also refuses a name
that already belongs to a built-in verb or alias, since a dog by that
name could never be reached. It warns rather than refuses on a merely
group-writable path. Passing all of that, it actually spawns the binary
with no arguments and kills it immediately, because the only honest way
to know whether this kernel can exec a file is to ask this kernel. What
gets recorded in `shep.toml` is the canonicalized, absolute path — never
whatever relative path you typed — because the daemon may resolve it
again after a reboot from a different working directory than the one
`adopt` ran from.

Once adopted, `shep <name> [args...]` runs the dog directly — the same
`git foo` runs `git-foo` precedent, resolved against adopted dogs only
(never a `$PATH` scan). It's a second invocation mode, distinct from the
one the shepherd itself uses: a dog the shepherd starts gets no argv and
one environment variable (`$SHEP_HOME`, below); a dog you name on the
command line gets whatever you typed after it, passed straight through,
plus that same `$SHEP_HOME`. A built-in verb or alias always wins over
a same-named dog.

`rehome <name>` is `disable`'s counterpart for a third-party dog: it stops
it if running and forgets the registration in `shep.toml` entirely, rather
than leaving it disabled-but-known the way plain `disable` would.

The wire a third-party dog speaks is the same client protocol
[§6](specs/shep-v1.md#6-wire-protocol-v1--protocol-version-1) pins for
every other client of this daemon: connect to the Unix socket at
`$SHEP_HOME/run/shep.sock`, send `Hello`, wait for `HelloAck`, then send
`Request::DogConfig { name }` to fetch your own `[dog.<name>]` section as
opaque text — parse it however your own config shape wants. The one
variable an adopted dog inherits from the shepherd's environment is
`$SHEP_HOME`, which is how it finds that socket in the first place; no
`[dog.<name>]` value ever rides along beside it, for the reason given
above.

**Knowing which name to ask for is your problem, and getting it wrong is
silent.** A dog is given no argv and that one variable, so nothing tells it
what the operator typed at `shep adopt`. And a `DogConfig` for a name nobody
adopted comes back as the empty string -- exactly what a registered dog with
no section gets, because a dog with no configuration is the ordinary case
rather than a fault. So a one-character mismatch between the name you expect
and the name the operator used discards their entire section for you, uses
every default instead, and prints nothing on either side. Hardcoding a name
and documenting it works, as long as you say so loudly. Working it out is
better: your process knows its own pid, `ListFlock` reports a pid per entry,
and the entry that is marked a dog and carries your pid is you -- its `name`
is the key your section lives under. `shep-log-rotate` does the second and
falls back to the first out loud.

**Never send `Request::Flush` as part of rotating anything.** Its name reads
like settling a buffer and it does the opposite of what a rotator wants: it
flushes what is pending and then **truncates** the recorded paths. That is
`shep flush`, an operator deliberately emptying logs, and reaching for it
before a rename on the intuition that "flush" means "make sure it is written"
deletes the lines you were about to rotate. The rename-then-`Reopen` shape
below is the one with no such hole.

There is no sandbox here, and it would be a mistake to assume one: **an
adopted dog runs at the shepherd's own trust level, with no isolation
beyond it.** That is the same trust an ordinary sheep in your Flockfile
already has, so adopting a dog does not grant anything beyond what any
process your own config already starts could do. It is an honest
comparison, not a hand-wave: you were already trusting whatever you point
a Flockfile's `script` at, and a third-party dog is held to no higher bar
than that.

## When a dog dies

If an enabled dog exhausts its own restart budget and lands on `Errored`,
the shepherd writes a record of that straight into `barks.jsonl` itself,
independent of whether the bark dog is even running. This matters for one
specific reason: the bark dog has no webhook code of its own for
reporting its own death, by design, so if the bark dog is the one that
died, nothing is going to page anyone about it over Discord or Slack.
What you get instead is a local record — `shep barks` after the fact
shows the moment alerting stopped rather than leaving a gap you have to
infer — and the metrics dog's `shep_dog_up` gauge dropping to `0` for
that dog's name, visible to anything already scraping it.

Nothing watches across dogs, and that absence is not an oversight: the
supervisor that restarts a crashed dog has no idea what a "dog" is beyond
the marker on its entry, by design, so a dog cannot be supervised
differently than a sheep, and no dog is positioned to watch another dog
the way the shepherd watches all of them. If you want to know a dog went
down, `shep barks` and the metrics endpoint are the two places that say
so — nothing pushes it further than that on its own.
