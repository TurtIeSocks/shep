# The shepherd channel — a contract for app authors

This is for anyone writing an app that runs under shep and wants to talk
back: send readiness, custom metrics, or answer a `shep trigger`. It is
language-agnostic — the channel is a plain file descriptor carrying JSON
text, nothing Rust-specific about it.

The wire shapes themselves are pinned in spec
[§7](specs/shep-v1.md#7-readiness--health) and
[§9](specs/shep-v1.md#9-cli-surface-sheep-native); this doc is the practical
walkthrough of the same contract, written for the app side rather than the
daemon side.

## Getting a channel

shep opens the channel — a socketpair, one end handed to your process as an
extra file descriptor — only when your app's Flockfile config asks for one.
Three fields open it, and any one of them is enough:

- `channel = true` — ask for it directly, with no other behavior implied.
- `wait_ready = true` — implies a channel, because it needs one to receive
  your `ready` message.
- `shutdown_with_message = true` — also implies a channel, for the same
  reason in the other direction.

Leave all three unset and your app gets no fd 3 at all. This is opt-in on
purpose: a channel is a socketpair plus two pump tasks running for the life
of the process, and shep would rather not pay that for an app that never
uses it.

**The file descriptor is fd 3**, and the daemon also exports its number as
the `SHEP_CHANNEL_FD` environment variable — read that instead of hardcoding
`3` if you want to be robust to it changing later.

**The descriptor is a normal blocking file descriptor.** A plain `read()`
on it parks your process until the daemon has something to say, exactly as
reading any other pipe would. You do not need an event loop, non-blocking
I/O, or polling to use it — a shell script doing `read -r line <&3` works.

## The wire format

Newline-delimited JSON, one complete message per line, in both directions.
Nothing else rides on this descriptor — no framing header, no length
prefix, just `{...}\n{...}\n...`. Read a line, parse it as JSON, act on it;
build a JSON object, append `\n`, write it.

### What you send (daemon reads this)

| Message | Meaning |
|---|---|
| `{"kind":"ready"}` | You are up and ready to serve. Only meaningful if `wait_ready = true`; the daemon is otherwise not waiting for it. |
| `{"kind":"metric","name":"<name>","value":<number>}` | A custom metric sample. Currently logged by the daemon at debug level and nothing more — no dog reads it yet. |
| `{"kind":"action-reply","action":"<name>","body":"<text>"}` | Your answer to a triggered action. `action` names which one; `body` is free-form text and becomes what the operator sees. |

### What you receive (daemon writes this)

| Message | Meaning |
|---|---|
| `{"kind":"shutdown"}` | Sent instead of a stop signal when `shutdown_with_message = true`. Treat it as your cue to shut down gracefully; the daemon still escalates to `SIGKILL` after `kill_timeout` if you take too long. |
| `{"kind":"action","name":"<name>"}` or `{"kind":"action","name":"<name>","params":"<text>"}` | An operator ran `shep trigger <selector> <name> [params]` against you. `params` is present only when the operator supplied one; absent otherwise — do not assume the key is always there. |

## Custom actions — the part most worth reading closely

`shep trigger <selector> <action> [params]` is how an operator reaches a
running app directly, for whatever your app defines an "action" to mean:
force a GC, dump internal state, flip a log level, whatever you want a
running process to be able to do without restarting it.

**The action name is entirely yours. shep never validates it, never keeps
a registry of known actions, and never inspects `params`.** Whatever the
operator typed after `shep trigger <selector>` is sent to you verbatim, for
you to recognize or refuse. There is no way to ask the daemon "what actions
does this app support" — that documentation lives with your app, not with
shep.

**Reply even to an action name you don't recognize.** This is the one rule
that actually matters for a good operator experience. From the daemon's
side, an app that is thinking hard about a slow action and an app that has
no idea what it was just asked are indistinguishable — both are silence.
The only thing that tells them apart is `action_timeout` (configurable per
app, default 3s, capped at 58s) running out. If you silently ignore
messages you don't understand, an operator who fat-fingers an action name
waits out the full timeout for nothing. Send back something like:

```json
{"kind":"action-reply","action":"reload-config","body":"unknown action: reload-config"}
```

and they find out immediately instead.

**The reply body is what the operator actually sees.** `shep trigger`
prints one row per matched sheep; a `Replied` row's `DETAIL` column is your
`body`, and `--format json` carries it whole and untouched — full length,
real newlines, byte-for-byte what you sent. The table view is the only
place it is ever altered, and only for display: capped at 80 characters
with a trailing `...` when cut, and embedded `\n`/`\r` shown as the two-
character escapes `\n`/`\r` so one long or multi-line reply cannot desync
the table's columns. Neither limit exists on the wire or in JSON output —
they are rendering choices in the CLI, not something shep asks you to
respect when you write `body`.

**Replies are matched by action name and by order, not by a request id.**
The channel carries no correlation id — adding one now would be a silent
wire break for every app already speaking it, since there is no version
field on fd 3 to negotiate a change through. So the daemon matches your
`action-reply` to a waiting trigger by name, and if you have two of the
same action outstanding, by the order you wrote them: reply once, promptly,
in the order you read the actions off the channel, and this is a non-issue.
If you reply to an action after its trigger has already timed out, that
late reply is treated as settling the debt for that one timeout rather than
being handed to whatever triggered the same action name next — but that
protection covers exactly one stray reply per timeout, not a general
inbox. The practical guidance is the same either way: don't sit on a reply,
and don't send more than one per action you were asked.

**No shepherd channel is not a silent failure.** An app configured with
none of `channel`/`wait_ready`/`shutdown_with_message` still gets a row
back from `shep trigger` — `no_channel`, naming exactly which config field
would have opened one. A reload drainee (the old instance mid-swap-out)
gets `skipped` instead of a wait, because an answer from the process on its
way out would be worse than none. Neither of these costs the operator a
timeout; both are refused immediately.

**Multi-argument `params` has no defined quoting today.** `params` is one
opaque string — whatever text followed the action name on the `shep
trigger` command line, passed to you exactly as given. If your action needs
more than one value, you own the grammar: put your own delimiter or your
own JSON inside that one string. shep will never parse it for you, and
there is currently no shep-level convention for how a multi-value `params`
string should be split — that is a decision your app makes for its own
actions, documented wherever you document them.

## Summary for the impatient

- Ask for a channel with `channel = true` (or get one for free from
  `wait_ready` / `shutdown_with_message`).
- Read `SHEP_CHANNEL_FD`, open that fd, read/write newline-delimited JSON.
- A plain blocking read works — no event loop required.
- Reply to every `action` message you receive, even ones you don't
  recognize, and reply exactly once, promptly.
- What you put in `body` is what the operator reads back.
