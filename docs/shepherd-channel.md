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

The daemon also exports `SHEP_CHANNEL_VERSION`, currently `1`. It describes
the wire on fd 3 as this document defines it. shep cannot ask what your app
speaks, so this is not a negotiation — it is there so a defensive app can
notice a version it has never seen and say so, rather than failing to parse a
line with nothing to connect that failure to a protocol change.

## On Windows: a named pipe, not fd 3

**Everything above this heading describes the unix tier.** Windows has no
fd-3 inheritance a child can rely on — `cmd.exe` has no `<&3` redirection,
and the mechanism shep uses on unix (`command-fds`, a `pre_exec` crate) does
not exist there. So the descriptor half of the contract is replaced, and
only that half:

- The daemon exports **`SHEP_CHANNEL_PIPE`**, whose value is a named-pipe
  path like `\\.\pipe\shep-channel-1234-0-9f3c1a2b4d5e6f708192a3b4c5d6e7f8`.
  **Your app opens it itself**, for reading and writing, exactly as it would
  open any file. That open is the whole difference. Read the name out of
  the environment and do not try to reconstruct it: the trailing hex is
  random per spawn.

  **What that randomness does and does not buy, stated exactly, because a
  client author is entitled to the real model.** A named pipe created with
  the default security descriptor grants read to Everyone and restricts
  write, so the channel is readable by any local account. shep does not set
  a restrictive descriptor on it. The random name means the pipe cannot be
  *predicted*, which closes the attack where a hostile local account parks
  itself on a name it knew in advance. It does not close *watching*: the
  pipe namespace is enumerable by any unprivileged local user (measured on
  Windows 10, 190 pipes listed from a non-elevated session), so an account
  polling it can see the name appear and race the real app to connect. Treat
  the channel as carrying nothing you would not show another local account,
  and see [deferred.md](specs/deferred.md) for the DACL that would close
  this properly.
- `SHEP_CHANNEL_VERSION` is exported as before.
- **`SHEP_CHANNEL_FD` is deliberately NOT set on Windows.** Branch on which
  variable is present rather than on the platform: an app that finds
  `SHEP_CHANNEL_PIPE` should open it, one that finds `SHEP_CHANNEL_FD`
  should use that descriptor, and one that finds neither was not given a
  channel.

**The wire format below is unchanged** — same newline-delimited JSON, same
`ready`/`metric`/`action-reply` outbound and `shutdown`/`action` inbound
shapes, same correlation id. A named pipe opened in byte mode is a blocking
byte stream, so "read a line, parse it, act on it" still describes it
exactly.

**This matters more on Windows than on unix**, and it is worth being blunt
about why. Windows has no way to deliver anything SIGTERM-shaped to another
process, so `shep stop` on an app that has NOT opted into this channel waits
out the app's whole `kill_timeout` and then terminates it. **The channel is
the only graceful shutdown available on that platform.** Set
`shutdown_with_message = true`, read the pipe, and act on `shutdown`.

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
| `{"kind":"action-reply","action":"<name>","body":"<text>","id":<number>}` | Your answer to a triggered action. `action` names which one; `body` is free-form text and becomes what the operator sees. `id` is optional — echo the `id` from the `action` message you are answering and shep matches your reply to that exact request. |

### What you receive (daemon writes this)

| Message | Meaning |
|---|---|
| `{"kind":"shutdown"}` | Sent instead of a stop signal when `shutdown_with_message = true`. Treat it as your cue to shut down gracefully; the daemon still escalates to `SIGKILL` after `kill_timeout` if you take too long. |
| `{"kind":"action","name":"<name>","id":<number>}`, optionally with `"params":"<text>"` | An operator ran `shep trigger <selector> <name> [params]` against you. `params` is present only when the operator supplied one; `id` is always present — echo it on your reply. |

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

**Echo the `id`, and your reply is matched exactly.** Every `action` message
carries an `id` — an opaque number, unique for the life of the daemon. Put it
back on your `action-reply` as `id` and shep hands your answer to that exact
request. Do that and everything below stops applying to you.

**If you don't echo it, replies are matched by action name and by order.**
That is the fallback, and it is what every app written before `id` existed
gets. shep matches your `action-reply` to a waiting trigger by name, and if
you have two of the same action outstanding, by the order you wrote them. If
you reply to an action after its trigger has already timed out, that late
reply settles the debt for that one timeout rather than being handed to
whatever triggered the same action name next — but that protection covers
exactly one stray reply per timeout, and it has a sharp edge: while a debt is
outstanding, an unstamped reply to a *live* trigger of the same name is
consumed as the debt payment, and the live trigger reports `timed_out` even
though you answered it promptly. Echoing `id` is how you make that
impossible. Failing that, don't sit on a reply, and don't send more than one
per action you were asked.

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

## Everything you write here is also public on the bus

Every message you send on fd 3 — `ready`, `metric`, `action-reply` — is
republished on the daemon's event bus under `channel.*`
(`channel.ready`, `channel.metric`, `channel.action_reply`), as its own
topic alongside `process.*` and the log topics. Anyone subscribed to
`channel.*` sees it, not just the operator who happened to send the
`trigger` you were answering.

This is not a security warning — nothing on this wire is a credential,
which is exactly why the event carries your message whole rather than a
redacted stand-in for it. It is a reminder that a `body` you write for one
operator's terminal is also a `body` a dashboard, a log aggregator, or
another dog entirely might render. Put in it what you would be comfortable
with any subscriber seeing, not just the one who asked.

## Summary for the impatient

- Ask for a channel with `channel = true` (or get one for free from
  `wait_ready` / `shutdown_with_message`).
- Read `SHEP_CHANNEL_FD` on unix or `SHEP_CHANNEL_PIPE` on Windows (exactly
  one is ever set), open it, read/write newline-delimited JSON.
- A plain blocking read works — no event loop required.
- Reply to every `action` message you receive, even ones you don't
  recognize, and reply exactly once, promptly.
- Echo the `id` from the action on your reply — one field, and it is what
  makes a slow action's answer land on the right trigger.
- What you put in `body` is what the operator reads back.
