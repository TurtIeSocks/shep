# whistle — the flock, over MCP

`shep whistle` is an MCP server, spoken over stdio, that hands a model the
same flock a person reaches with `shep flock`, `shep stop`, `shep restart`.
Spec [§8](../specs/shep-v1.md#8-dogs-plugins) names nine tools; this is the
practical guide to running it, what its gate does and does not buy, and the
one question it cannot answer for you afterward.

## The nine tools

Listed, described and pinned in [`tools.md`](tools.md) — generated straight
from the two live routers `whistle/read.rs` and `whistle/control.rs` build,
not a second hand-typed copy beside them. Read it there rather than here: a
second list is the thing this generation exists to prevent.

Five read the flock and are always registered. Four act on it — `start_sheep`,
`stop_sheep`, `restart_sheep`, `reload_sheep` — and exist only when the gate
below is open.

## Turning control on

```toml
# $SHEP_HOME/shep.toml
[whistle]
allow_control = true
```

Restart whistle for the edit to take effect — the running shepherd never
reads this section itself; whistle reads its own copy of `shep.toml` once, at
startup. With the gate shut, the four control tools are absent from
`tools/list` entirely, not present and refusing: a model cannot be tempted by
a tool it cannot see.

A worked launcher config, for an MCP host that spawns its own servers:

```jsonc
{
  "mcpServers": {
    "shep": {
      "command": "shep",
      "args": ["whistle"]
    }
  }
}
```

## What the gate is, and is not

**`allow_control` is a fat-finger catch, not a security boundary.** whistle
runs as whoever launched it, at that person's uid, and that uid can already
run `shep stop`, `shep delete`, or `rm -rf`. Turning the gate on does not
hand out any capability the launcher didn't already have.

What it does buy is narrower and real: `tail_bleats` returns text a sheep
wrote to its own logs, verbatim, into a model's context. With the gate shut,
that context has no tool in it that can act on what it just read — a sheep
that logs an attacker's input logs an attacker's instructions, and those
instructions land next to a tool list with nothing on it but `list_flock`,
`describe_sheep`, `get_metrics`, `tail_bleats` and `list_barks`. With the
gate open, a log line can reach `stop_sheep`. That's the specific thing the
default of `false` is for.

There is no `--allow-control` flag, and the reason is legibility, not
containment — a boolean in a file has a diff and an mtime an operator can
audit; a flag lives in whatever process's argv and is invisible to `shep`
between runs. That argument does not extend to claiming the flag would reach
somewhere the file cannot: `shep whistle --home <dir>` and
`SHEP_HOME=<dir> shep whistle` both already choose which `shep.toml` gets
read, so the launcher is the boundary in argv, environment and file alike —
restating the same point from the other side, not a second one.

`start_sheep` is narrower than `shep start` on purpose, gate or no gate: it
takes the name of a sheep already in the flock and can never register a new
process. A `start_sheep` shaped like `shep start` — a script path handed to a
model — would be arbitrary code execution as the operator, and no config
setting makes that acceptable.

## What the shepherd cannot tell you

A `restart_sheep` call from a model and a `shep restart` typed by a person
arrive at the daemon identically — the same `CommandOrigin::Operator`, the
same bus event, the same log line, the same bark. There is no wire field that
tells them apart. An operator asking "who restarted `api` at 3am" will not
find the answer in the shepherd's own records if whistle was in the loop;
attribution stops at the socket.
