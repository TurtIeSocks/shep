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
against once in the file. B is not caused by this design and is fixed
separately. It is recorded here because the investigation found it.

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

New field, `instance: u32`, set at every construction site, with a builder
setter. Additive for JSON consumers, and the output envelope's
`SCHEMA_VERSION` bumps.

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

`plain`, `bare` and JSON stay one line per process so they stay greppable. In
those styles the NAME cell becomes `web:2`, and only for apps with more than
one instance. JSON rows gain `instance` as a field rather than a suffix.

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

## Out of scope

**Defect B, the bleats duplication.** Confirmed above. It predates this work,
it reaches any shared log path including every `merge_logs = true` app, and it
wants a dedup on the path plus a regression test. Its own fix and its own
commit.

**`instances = 0` meaning "one per CPU".** pm2 expanded `0` and negative
counts to a CPU count, but shep refuses zero outright
(`NormalizeError::ZeroInstances`). Not restored here. If it comes back it
should be an explicit `instances = "cpus"` rather than an overloaded integer.

**Labelling bleats lines by slot.** The current prefix is the name alone, so
two instances are indistinguishable in the output even when their files are
separate. Worth doing, and it belongs with defect B's fix rather than here.

## Testing

- normalize: the colon ban, both reserved variables, an unknown token, the
  doubling escape, and D8's refusal in each of its three escape hatches.
- selector: `name:slot` against `fold:`, a digit id, a glob and a bare name,
  and the precedence between them.
- assemble: substitution in env, args and both log paths, and `{{name}}`.
- renderer: exact-string tests for the group row, the suffix in the flat
  styles, and the single-instance case being untouched.
- lookout: both row kinds, reseat across a poll, and the blast-radius confirm.
- import: pm2 `instance_var` converting to an env entry.
- e2e: a real multi-instance node app, since that is what found defects A
  and B.

## Docs

The docs trigger applies, since this changes what an operator types and sees.
Regenerate the CLI reference and the Flockfile schema, then read
`from-pm2.astro`, `first-flockfile.astro`, `migration.md`, `output.astro`,
`lookout.astro` and `json-output.astro`. Add the `instance` entry to
`terminology.md`.

## Migration

| break | who notices | what they do |
|---|---|---|
| `increment_var` removed | a Flockfile using it | set `env.YOUR_VAR = "{{instance}}"`, as the error says |
| `:` refused in names | a sheep named with one | rename, and note it was already broken on Windows |
| explicit `out_file` with `instances > 1` | a Flockfile that silently merged | add `{{instance}}` to the path, or `merge_logs = true` |
| `SelectorSpec` variant added | an old daemon meeting a new CLI | restart the daemon |
| `ProcessInfo.instance` added | a JSON consumer | additive, `SCHEMA_VERSION` bumps |
