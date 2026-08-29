# Design: instances, second pass

Status: approved design, not yet planned. Supersedes `increment_var`.

shep is 0.1.x and guarantees no API, so this pass breaks the wire, the
Flockfile grammar and the name grammar where breaking them buys something.
Every break is listed under Migration below.

## The problem

Three complaints, and two defects found while checking them.

**Instances are invisible on the wire.** The daemon knows which slot a sheep
occupies (`ProcessEntry.instance`, `crates/shep-daemon/src/entry.rs:16`) and
never sends it. `ProcessInfo` says so outright at
`crates/shep-daemon/src/supervisor.rs:1397`. The only way a client can
recover a slot today is to string-parse the log filename, which one test
helper does and nothing else.

**Everything renders flat.** Measured against a real two-instance app on
2026-08-29:

```
ID  NAME    STATUS  PID    RESTARTS  EXIT  CPU   MEM    UPTIME  FOLD  SMIT
0   talker  online  49013  0         -     0.1%  40.8M  2m 21s  -     -
1   talker  online  49014  0         -     0.1%  41.4M  2m 21s  -     -
```

Two rows, one name, nothing saying they are one app. `shep describe talker`
prints the same thing. The daemon already thinks in apps rather than
instances for two of its verbs, and the table cannot show it: `Scale` and
`SetSmit` both take a name, and the smit doc at
`crates/shep-core/src/protocol/request.rs:302` explains why, that a smit
belongs to a sheep rather than to one of its instances.

**`increment_var` is a knob that renames one variable.** `SHEP_INSTANCE` is
already the default name at `crates/shep-daemon/src/assemble.rs:207`, so the
field's whole job is to call it something else. An app wanting two derived
values, a worker id and a device id, cannot have them.

The two defects, both about log paths:

**A. An explicit `out_file` silently merges.** The `-<instance>-` infix lives
only on the default path (`assemble.rs:213-226`), so `out_file =
"/var/log/web.log"` with `instances = 3` gives every instance one file. That
is `merge_logs` behaviour without `merge_logs`, and there is no way to ask
for anything else.

**B. `shep bleats` prints one copy per instance of any shared log file.**
`tail_log_files` loops over matched rows and calls `read_tail` per row with no
dedup on the path (`crates/shep-cli/src/commands/bleats.rs:331-347`).
Confirmed on the same two-instance app, `merge_logs = true`:

```
$ wc -l < $SHEP_HOME/logs/talker-out.log
938
$ shep bleats talker --no-follow --lines 5000 | wc -l
1876
```

Exactly twice the file, and `slot=0 line=1` appears twice in the output
against once in the file. B predates this design rather than following from
it, so D10 fixes it in its own commit ahead of everything else.

The same run showed a third, smaller thing. The prefix is `talker |` for both
instances, so nothing in the output says which one spoke. D11 covers it.

## Decisions

### D1. `instances` keeps its name, and lambs are left alone

A lamb is a child process of one sheep. An instance is a sibling copy of a
sheep. Reusing the word would mean an instance-lamb has lambs of its own, and
the describe tree would stop being a process tree.

Lambs are also more load-bearing than they look: `Lamb` is a wire type
(`crates/shep-core/src/protocol/request.rs:480`), filled on describe
(`crates/shep-daemon/src/rpc.rs:568`), rendered in the describe table, the
lookout detail pane and the whistle facts, and covered by roughly thirty
tests across eight files and eight docs pages.

`terminology.md` gains an `instance` entry that says what it is not, because
lamb is the neighbouring word.

### D2. `ProcessInfo` carries the slot

New field, `instance: Option<u32>`, with a builder setter, set wherever the
daemon builds a row from a `ProcessEntry`.

`Option` rather than a bare `u32`, because that is the house rule for this
exact situation and five fields already follow it. `None` means the peer
daemon predates the field, which is how `out_file`, `cpu_percent`, `dog`,
`last_exit` and `smit` all read their own absence. A bare `u32` would need
`#[serde(default)]` to survive an old daemon's reply and would then report
every row as slot 0, which is the silently-wrong zero the `dog` field's doc
warns against.

Everything downstream degrades to today's behaviour when it sees `None`: no
grouping, no suffix, no slot in a bleats prefix, and `name:slot` matches
nothing. Against an old daemon the output is exactly what it is now.

Additive for JSON consumers, and the output envelope's `SCHEMA_VERSION` bumps.

**`sort_flock` becomes `(name, instance, id)`.** Its doc already argues for
this order and rules it out for one reason: "it is a rule no listing that has
crossed the wire could reproduce, since `ProcessInfo` carries no instance
number" (`crates/shep-core/src/protocol/request.rs:713-719`). This field is
that reason removed. The order it wants is more stable than `(name, id)`
wherever a reload has given a slot a fresh id, which is precisely when the
current rule reshuffles rows under an operator watching a two-second poll.
With `None` on every row the comparison collapses to `(name, id)`, so an old
daemon sorts exactly as it does today.

### D3. `name:slot` selects one instance, and names lose the colon

Parsed last, immediately before a bare name:

```
all  >  fold:<name>  >  /regex/  >  digits(id)  >  glob  >  name:slot  >  name
```

`fold:` is a prefix test and still wins first, so the two colon forms cannot
collide. `ProcessSelector::matches` takes the instance as a fourth argument.
`is_exact()` is true for the new variant, since the operator named one entry,
which keeps `shep restart metrics:0` reaching a dog.

Names may no longer contain `:`. This is a fix as much as a grammar change:
names go into log filenames, and NTFS reads `:` as the alternate-data-stream
separator, so a sheep called `web:2` cannot open a log file on Windows now.
The error names the character and suggests `-`.

Slot lifecycle is unchanged. `stop web:2` leaves slot 2 registered and
stopped. `delete web:2` frees it, and the next scale-up refills the lowest
free slot (`crates/shep-daemon/src/assemble.rs:36`).

Cost, taken deliberately: a new `SelectorSpec` variant is a protocol change an
older daemon cannot deserialize. Globs avoided this by compiling down to
`Regex` (`crates/shep-core/src/selector.rs:49-52`), but a slot is not part of
the name, so the same trick is unavailable.

### D4. `shep flock` groups multi-instance apps in the full style

A single-instance app is unchanged, no group row and no suffix:

```
 ID   NAME      STATUS      PID    RESTARTS   CPU     MEM      UPTIME   SMIT
 4    api       up          1201   0          0.3%    41 MB    2h 14m
      web ×3    up          -      2          5.1%    372 MB   41m      ▲ main@a1b2c3
 1     ↳ :0     up          1130   0          1.9%    124 MB   2h 14m
 2     ↳ :1     up          1131   0          1.7%    121 MB   2h 14m
 3     ↳ :2     up          1145   2          1.5%    127 MB   41m
```

`↳ :0` teaches the `web:0` selector by sitting under the name. A mixed group
rolls up honestly rather than picking a winner, `2 up, 1 down`.

| column | group row |
|---|---|
| ID | blank, there is no single id |
| NAME | `web ×3` |
| STATUS | the shared status, else `2 up, 1 down` |
| PID | blank |
| RESTARTS | sum |
| EXIT | blank, instance rows carry it |
| CPU, MEM | sum, which answers what the app costs |
| UPTIME | minimum, time since the last disruption |
| FOLD, SMIT | per-app already, so they sit here and leave the instance rows |

When a selector or filter matches only some instances, the count and the
rollups describe the rows actually listed, not the app's true size.

`bare` and JSON stay one line per process so they stay greppable. In `bare`
the NAME cell becomes `web:2`, and only for apps with more than one instance
where every row reports its slot. JSON rows gain `instance` as a field rather
than a suffix.

**`plain` groups too, alongside `full`.** This paragraph said otherwise until
the style dial was actually read. `StyleLevel::boxes()` and `colour()` both
already treat `Full` and `Plain` as one tier
(`crates/shep-cli/src/style.rs:58-70`), so the codebase's own idea of a
human-facing table is those two together, and `bare` is the machine tier.
Splitting Full from Plain for grouping alone would invent a third distinction
the dial does not otherwise make, to serve a sentence written before anyone
looked.

The suffix therefore lives in `FlockRows::rows()` rather than in `rows_for`.
`render_table`, the non-boxed path, calls `rows()` (`table.rs:37`), while
`table_of` reaches `rows_for` only when `boxes()` is true. Seventeen types
override `rows_for`, so moving that dispatch to serve one of them would change
all of them.

### D5. Lookout gains two row kinds

`Row` splits into `Group { name, rollup }` and `Sheep { id, info }`, and the
selection key becomes one or the other so `reseat`
(`crates/shep-cli/src/lookout/app.rs:1132`) still survives a poll.

Group rows are selectable, and an action key on one targets the whole app by
name. This is the only place in shep where one keypress reaches several
processes, so the confirm states the blast radius:

```
Stop all 3 instances of web?  [y/N]
```

The detail pane on a group row shows the app-level summary rather than one
process's fields. The bleats pane on a group row reads every instance's file
and labels lines by slot, capped, which costs one bounded read per instance
instead of one per pane.

### D6. `SHEP_INSTANCE` and `SHEP_NAME` are always injected and reserved

Both go into every child. Setting either in `[app.env]` is a normalize error
naming the variable, rather than a value the daemon quietly overwrites.

### D7. `{{instance}}` and `{{name}}` in env values and args

```toml
[[app]]
name = "z-worker"
instances = 4
args = ["--metrics-port", "91{{instance}}"]

[app.env]
Z_WORKER_ID = "z-{{instance}}"
Z_DEVICE_ID = "z-{{instance}}d"
```

Any other `{{...}}` is refused at normalize time, named, and listed against
the valid tokens, so `{{instnace}}` dies at `shep start` rather than reaching
the child as a literal.

Doubled braces, not single. Single braces appear constantly in real values,
in JSON blobs, regex quantifiers like `{2,3}`, and Go or Helm templates passed
through as args, and under strict refusal a collision is fatal rather than
silent. `LOG_FORMAT = '{"ts":"%t"}'` would stop a working Flockfile from
starting. Escaping is by doubling, `{{{{` and `}}}}`, which is `format!`'s own
rule one level up. A TOML basic string cannot carry `\{{` at all, so backslash
escaping was never available.

Validation and substitution split across the existing proof-token seam.
`normalize` checks the grammar, `assemble` performs the substitution: it is
pure, already holds both the name and the slot, and every spawn path goes
through it, so a fresh start, a restart, a reload and a scale-up all agree.

### D8. `out_file` and `err_file` are templated, and a silent merge is refused

The two fields join the substitution sites, which closes defect A. An explicit
path with `instances > 1`, no `{{instance}}` and no `merge_logs` is a
normalize error telling the operator to add one or the other. Refusing is what
shep does with `instances = 0` and with a name holding a path separator, and it
makes the templating load-bearing rather than decorative.

### D9. `increment_var` is removed, and says so for one release

Fifty-two references across twenty-three files. `shep import` converts pm2's
`instance_var: FOO` into `env.FOO = "{{instance}}"`, so migration loses
nothing.

The field stays in `AppConfig` for one release purely to be rejected with the
replacement spelled out. Without it, `deny_unknown_fields` produces a serde
error that names no fix. Remove in 0.2.

### D10. `shep bleats` reads each distinct path once

Defect B. `tail_log_files` iterates matched rows and calls `read_tail` per row
with no dedup, so a path shared by several instances is read once per
instance. Dedup the matched rows by resolved path before reading.

**The backlog only.** Following is not affected and needs no change: it
subscribes to `log.*` and reads `BusEvent::LogOut { id, line }`, which is
emitted per sheep, so two instances sharing a file still produce one event
each. My first reading of the repro said both halves duplicated, and that was
wrong. The 36 lines printed twice were the backlog running twice; everything
after it was two live processes each writing its own line.

`log_unreadable` names a path and so dedups with the reads.
`log_path_unknown` has no path to key on, since the field is missing, so it
dedups on the name and stream its message already carries.

This lands as its own commit ahead of the rest. It is a live bug against
`merge_logs`, which is shipped and documented, and it is independent of every
other decision here.

### D11. Bleats labels a line with its slot

The table prefix is the name alone (`bleats.rs:184`), so two instances are
indistinguishable even when their files are separate. `id` is already passed
to `write_line` and used only by the JSON arm.

| case | table prefix | JSON `instance` |
|---|---|---|
| single-instance app | `web \|` | the slot |
| multi-instance, own file | `web:2 \|` | the slot |
| multi-instance, shared file, backlog | `web \|` | null |
| multi-instance, shared file, following | `web:2 \|` | the slot |

The last two rows differ because the two halves of `bleats` learn about a line
in different ways. The backlog reads a file, and a shared file holds every
instance's output interleaved with nothing in a line saying who wrote it, so
shep declines to guess. Following reads `BusEvent::LogOut { id, line }`, which
the daemon emits per sheep, so the writer is known even when the file is
shared. Pairs with D10: one read of the merged file, unattributed, then
attributed lines from the moment the subscription starts.

The slot appears whenever the app has more than one instance registered, not
merely more than one matched. A selector should not change how a line is
labelled, so `shep bleats web:0` still prints `web:0`. This differs on purpose
from D4's rollup rule, where the count describes the rows actually listed: a
table row summarises a listing, while a log prefix identifies a process.

## Out of scope

**`instances = 0` meaning "one per CPU".** pm2 expanded `0` and negative
counts to a CPU count, but shep refuses zero outright
(`NormalizeError::ZeroInstances`). Not restored here. If it comes back it
should be an explicit `instances = "cpus"` rather than an overloaded integer.

## Testing

- normalize: the colon ban, both reserved variables, an unknown token, the
  doubling escape, and D8's refusal in each of its three escape hatches.
- selector: `name:slot` against `fold:`, a digit id, a glob and a bare name,
  and the precedence between them.
- assemble: substitution in env, args and both log paths, and `{{name}}`.
- sort_flock: a reloaded slot keeping its position, and a listing of all
  `None` instances ordering exactly as `(name, id)` did.
- renderer: exact-string tests for the group row, the suffix in the flat
  styles, and the single-instance case being untouched.
- lookout: both row kinds, reseat across a poll, and the blast-radius confirm.
- import: pm2 `instance_var` converting to an env entry.
- bleats: a shared path read once rather than once per instance, with the
  line count pinned against the file's own, one notice per distinct path, and
  all three labelling cases from D11, including the null in the JSON arm.
- e2e: a real multi-instance node app, since that is what found defects A
  and B, and the `merge_logs` case specifically, since that is where B bites
  hardest.

## Docs

The docs trigger applies, since this changes what an operator types and sees.
Regenerate the CLI reference and the Flockfile schema, then read
`from-pm2.astro`, `first-flockfile.astro`, `migration.md`, `output.astro`,
`lookout.astro` and `json-output.astro`. D11 changes the bleats prefix and its
JSON object, so also check `getting-started.astro`, `examples.astro`,
`folds.astro` and `cli.astro`, which all show bleats output. Add the
`instance` entry to `terminology.md`.

## Migration

| break | who notices | what they do |
|---|---|---|
| `increment_var` removed | a Flockfile using it | set `env.YOUR_VAR = "{{instance}}"`, as the error says |
| `:` refused in names | a sheep named with one | rename, and note it was already broken on Windows |
| explicit `out_file` with `instances > 1` | a Flockfile that silently merged | add `{{instance}}` to the path, or `merge_logs = true` |
| `SelectorSpec` variant added | an old daemon meeting a new CLI | restart the daemon |
| `ProcessInfo.instance` added | a JSON consumer | additive, `SCHEMA_VERSION` bumps |
| bleats stops repeating a shared file | anyone parsing its output | they were reading duplicates, so nothing to do |
| bleats prefix gains `:slot` | a script splitting on `\|` | match the name loosely, or read the JSON arm's `instance` |
