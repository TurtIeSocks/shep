# Design: shepherd-channel client libraries for Go, JavaScript, Python and Rust

Status: designed, not built. Covers the 0.1.0 surface only. The dog client
is scoped here and deliberately held back to 0.2.0; see "What 0.1.0 leaves
out".

## The problem

An app that wants to talk back to the shepherd hand-rolls the framing today.
`docs/shepherd-channel.md` is a good contract and it still asks every author
to do the same six things: find the descriptor, notice which of two
environment variables is set, split a byte stream on newlines, parse each
line, echo a correlation id, and reply to action names it does not
recognise. None of that is app logic.

The examples in this repo prove the gap rather than close it.
`examples/polyglot/` ships an HTTP server in Node, one in Python and one in
Go, and not one of the three opens a channel. The only things in the tree
that speak fd 3 are daemon tests, and they speak it through `/bin/sh` and
`cmd.exe`.

The last rule in the contract is the one a library exists to enforce:

> **Reply even to an action name you don't recognize.** [...] From the
> daemon's side, an app that is thinking hard about a slow action and an app
> that has no idea what it was just asked are indistinguishable.

An app author can forget that. A library cannot.

## What the wire actually is

Pinned so the rest of this document can lean on it.
`crates/shep-core/src/protocol/channel.rs:48` sets `CHANNEL_VERSION` to `"1"`.
Five message shapes total: `Ready`, `Metric`, `ActionReply` going up,
`Shutdown` and `Action` coming down.

`crates/shep-daemon/src/tokio_runner.rs:931` exports `SHEP_CHANNEL_FD=3` on
unix, `:889` exports `SHEP_CHANNEL_PIPE` on Windows, and both export
`SHEP_CHANNEL_VERSION`. Exactly one of the first two is ever set, so a
library branches on presence rather than on platform.

Two daemon behaviours a library can rely on, both at
`tokio_runner.rs:2171`. A malformed line is logged and skipped, so bad JSON
from one app does not close its own channel. And the child's end of the
socketpair has `O_NONBLOCK` cleared on purpose, documented at `:918`, so a
plain blocking read is the intended usage rather than a workaround.

## Decisions

**D1. Two layers per library, not one.** A low-level channel with `open`,
`recv` and `send` and no threads, and a handler layer built on top of it.
The handler layer is where the auto-reply guarantee lives, so it is the
documented default; the low layer exists for an app that already owns an
event loop and wants the channel inside it. One code path, not two: the
handler layer is a consumer of the low layer.

**D2. Synchronous first. Async is additive later.** Rust gets a background
thread and no tokio in its dependency tree. Python gets a
`threading.Thread`. Node and Go keep the one model each of them has. A
`tokio` cargo feature and a `shep_pm.channel.aio` submodule can both be
added in a later minor version without breaking anything, and the contract
already says a plain blocking read works.

**D3. No channel is a no-op handle, not a null.** `serve()` always returns
something usable. With no channel every call does nothing, so an app needs
no branching at its emit sites. It writes one line to stderr, once, and only
when `SHEP_NAME` is set. That variable is injected unconditionally
(`crates/shep-daemon/src/assemble.rs:230`) and cannot be set by hand
(`crates/shep-core/src/config/normalize.rs:297`), which makes it a reliable
signal for "running under shep". Running under shep with no channel is a
probable config mistake and worth a line. Running outside shep is not, and
stays silent. `is_active()` is there for anyone who wants to branch anyway.

**D4. Metrics are droppable. Readiness and replies are not.** One writer
thread behind a bounded queue, 1024 messages by default. `metric` never
blocks and never fails; on a full queue it drops the oldest and increments a
counter the app can read. `ready` and action replies block until queued,
because a lost `ready` hangs the `wait_ready` gate and a lost reply costs an
operator the full `action_timeout`. The daemon logs metrics at debug level
and nothing else reads them yet, so dropping one costs nothing today.

**D5. A shutdown with no handler warns and does nothing.** The library never
terminates an app that did not ask it to. It names the missing handler and
lets the existing `kill_timeout` escalation take over, which is what already
happens without the library. Auto-exit was considered and rejected: a
library ending a process on its own judgement skips whatever cleanup the app
would have run. Re-raising `SIGTERM` was also considered and rejected,
because Windows has no `SIGTERM` and Windows is the platform where the
channel is the only graceful stop available.

**D6. An unrecognised `SHEP_CHANNEL_VERSION` warns and proceeds.** Refusing
would break every app on the day shep ships an additive `2`. The stamp is
not a negotiation and the library should not pretend it is one.

**D7. `serve()` is a process singleton.** Only one owner of fd 3 can exist,
so a second call returns the first handle and warns rather than
double-owning the descriptor.

**D8. Handler failure is a reply, not a silence.** A handler that throws,
panics or rejects produces `action handler failed: <message>` rather than
nothing. The id is echoed on every reply the library sends, including this
one, so a slow or failed action still lands on the trigger that asked for it.

**D9. One repo per language, several packages inside it.** Not one repo per
library. The JavaScript side alone needs a channel package, a CLI shim and
seven per-platform binary packages, which would be nine repos under a
per-library rule.

**D10. Rust folds into `shep-pm/shep` rather than a fourth repo.**
`crates/shep-core/src/protocol/channel.rs` imports serde and nothing else
from the crate, so it extracts cleanly as a leaf. `shep-channel` becomes
that leaf with `default = ["client"]`, so `shep-core` takes it with
`default-features = false` and gets the two enums without the threads, the
handlers or the descriptor handling. One spelling of the wire exists
permanently, with no generator involved. This inversion was rejected earlier
for a good reason that stops applying here: as separate repos it would have
made shep's release wait on another repo's publish. Inside one workspace it
is one version group and one release PR. A `shep-rs` repo would also be a
strange name for the thing `shep` already is.

**D11. Typed action names are deferred.** Measured and set aside rather than
dropped, so the work is not repeated. The relevant constraint is that shep
keeps no registry of action names, so `shep trigger web anything` is legal
and arrives verbatim; a type can constrain what an app registers but never
what arrives. Ceilings measured on 2026-09-02: TypeScript reaches
compiler-checked exhaustiveness with `serve<Action>()` plus a
`Record<Action, Handler>` form (a typo is `TS2345`, a missing action is
`TS2739`). Rust reaches typo-caught with `impl AsRef<str>` and no trait of
ours. Go reaches typo-caught with a defined `type Action string`, and
generics are unavailable regardless because a Go method cannot carry a type
parameter. Python reaches typo-caught with a `str` enum under a checker.

## The shared contract

Same concepts in the same order in all four, spelled each ecosystem's way.

| concept | Rust | Go | Node | Python |
|---|---|---|---|---|
| open | `shep_channel::serve()` | `channel.Serve()` | `serve()` | `shep_pm.channel.serve()` |
| ready | `ch.ready()?` | `ch.Ready() error` | `ch.ready()` | `ch.ready()` |
| metric | `ch.metric("rps", 42.0)` | `ch.Metric("rps", 42.0)` | `ch.metric("rps", 42)` | `ch.metric("rps", 42.0)` |
| action | `ch.on_action("gc", f)` | `ch.OnAction("gc", f)` | `ch.onAction("gc", f)` | `@ch.action("gc")` |
| shutdown | `ch.on_shutdown(f)` | `ch.OnShutdown(f)` | `ch.onShutdown(f)` | `@ch.shutdown` |
| live? | `ch.is_active()` | `ch.Active()` | `ch.active` | `ch.active` |
| drops | `ch.dropped_metrics()` | `ch.DroppedMetrics()` | `ch.droppedMetrics` | `ch.dropped_metrics` |
| stamp | `ch.version()` | `ch.Version()` | `ch.version` | `ch.version` |

An action handler receives the params and the action name, and returns the
reply body. The library supplies the reply in three cases the contract calls
out: an unregistered name gets `unknown action: <name>`, a failed handler
gets `action handler failed: <message>`, and every reply echoes the id.

```rust
let ch = shep_channel::serve();
ch.on_action("gc", |params, _name| format!("collected, params={params:?}"));
ch.on_shutdown(move || { let _ = stop_tx.send(()); });
ch.ready()?;
ch.metric("rps", 4200.0);
```

## Windows is a different door, not a different wire

Each library branches on which environment variable is present rather than
on the platform, because exactly one is ever set. `SHEP_CHANNEL_FD` means
take that descriptor. `SHEP_CHANNEL_PIPE` means open that path like any
other file. Neither means there is no channel, which is D3's case.

The wire above the door is identical, so everything else in this document
applies unchanged. It matters more on Windows than on unix: there is no way
to deliver anything SIGTERM-shaped there, so `shutdown_with_message` is the
only graceful stop an app can get, and D5's warning is the difference
between an author noticing that and not.

The pipe name carries a random suffix per spawn, so a library must read it
from the environment and never reconstruct it.

## Testing

**Fixtures are the shared floor.** Every library's suite decodes and
re-encodes the same corpus of JSON lines and asserts byte equality where the
daemon's own tests assert it. That is what makes four independent
implementations agree before the generator exists to make them agree by
construction.

**Above the fixtures, each library is driven against a real socketpair**
with a fake shepherd on the other end, not against a mocked transport. The
probe written during design already does this in all four languages and
becomes the basis of the harness. A test that never opens a descriptor
cannot catch the class of bug that matters here, which is a library that
parses correctly and never reads.

**Every wait needs a forcing mechanism.** No test may depend on a handler
running "soon". A test asserts on an explicit transition: a reply arriving on
the fake shepherd's read end, a queue counter moving, a bounded timeout that
fails the test rather than hanging it. Rust pauses the clock where it can
(IR-33); the other three use an explicit deadline and treat expiry as
failure.

**Non-vacuity is checked, not assumed.** Before a test is trusted, the thing
it protects is mutated and that specific test is watched going red, then
restored. This is cheap and it is not ceremony: the auto-reply guarantee in
D8 and the drop policy in D4 are both the kind of behaviour a test can
appear to cover while asserting nothing.

## Verification carried out before the design was fixed

Every language was driven against a real `AF_UNIX` socketpair handed to a
real child on fd 3, with the daemon's own message shapes on the wire. All
four received an `action` carrying `params` and an `id` and returned a
correctly stamped `action-reply`.

| language | mechanism |
|---|---|
| Rust | `UnixStream::from_raw_fd(3)`, `try_clone` for the writer half |
| Go | `net.FileConn(os.NewFile(3, ...))` |
| Node | `new net.Socket({ fd: 3 })` |
| Python | `socket.socket(fileno=3)` |

One result changes the Rust crate's header. There is no safe constructor
from a raw descriptor, so `shep-channel` cannot carry
`#![forbid(unsafe_code)]` like `shep-core`, `shep-client` and `shep`. It
gets `#![deny(unsafe_code)]` and exactly one `#[allow]` site with a
`// SAFETY:` comment, which is the rule `crates/shep-daemon/src/sys.rs`
already follows (IR-22, IR-23).

## Package names

Both obvious names are taken by unrelated projects, checked 2026-09-02. npm
`shep` is an AWS API Gateway framework, 52 versions, last published
2022-06-26. PyPI `shep` is "Multi-state key stores using bit masks", 0.3.6.

| registry | name | note |
|---|---|---|
| crates.io | `shep-channel` | free; `shep-dog` also free, reserved for 0.2.0 |
| npm | `@shep-pm/channel`, `@shep-pm/cli` | the scope sidesteps the squatter and suits a multi-package repo |
| PyPI | `shep-pm` | library, universal wheel; import root `shep_pm` avoids colliding with the bit-mask package |
| PyPI | `shep-cli` | the binary, platform wheels; matches the crates.io placeholder of the same name |
| Go | `github.com/shep-pm/shep-go/channel` | no registry to dodge |

`pip install shep-pm[cli]` pulls the binary through an extra, so one command
gets both halves without forcing the pure-Python library to carry platform
tags.

## Repo layout

```
shep-pm/shep                       exists
  crates/shep-channel/             new, leaf: serde and serde_json only
  crates/shep-core/                depends on it, re-exports the two enums
  docs/shepherd-channel/fixtures/  new, generated from the real serde impls
  xtask wire-export                new, built last

shep-pm/shep-js                    npm workspace
  packages/channel/                @shep-pm/channel
  packages/cli/                    @shep-pm/cli plus 7 optional platform packages
  packages/dog/                    0.2.0
  fixtures/                        vendored

shep-pm/shep-py
  src/shep_pm/channel/             PyPI shep-pm
  packaging/cli/                   PyPI shep-cli
  src/shep_pm/dog/                 0.2.0
  fixtures/                        vendored

shep-pm/shep-go                    one module, one tag, standard library only
  channel/
  dog/                             0.2.0
  fixtures/                        vendored
```

Each new repo mirrors what `shep` already carries: dual MIT and Apache-2.0,
`SECURITY.md`, `.coderabbit.yaml`, `_typos.toml`, and a test matrix across
macOS, Linux and Windows.

## The CLI shims

Go gets none. There is no mechanism to `go install` a binary built from
Rust, so `shep-go` is the channel and later the dog, and nothing else.

JavaScript and Python both get one, and neither uses an install script. The
current pattern is per-platform packages plus a resolver, which works in
organisations that disable install scripts and does not add an
install-time-execution surface to audit. Both mirror the seven targets
`.github/workflows/release-artifacts.yml` already builds: `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-pc-windows-msvc`.

## Release

| repo | tool | trigger | auth |
|---|---|---|---|
| shep | release-plz, exists | merge the release PR | crates.io token, exists |
| shep-js | release-please | merge the release PR | npm token, provenance via OIDC |
| shep-py | release-please | merge the release PR | PyPI trusted publishing, no token |
| shep-go | release-please | merge the release PR, tag only | none, the module proxy pulls the tag |

release-please across the three non-Rust repos is chosen for one reason: it
gives the same operator gesture release-plz already gives this workspace,
where the release is the merge. One habit, four repos.

PyPI trusted publishing removes the token outright. npm has since added its
own; whether it covers scoped packages is unconfirmed, so plan on a granular
token and drop it if the check at setup says otherwise.

## The generator

Built after the four libraries exist, so its acceptance test can be that its
first run produces a zero diff against files written by hand. If it does not,
one of the two is wrong and that is worth knowing before anything immutable
is published.

```
shep-core's real serde impls
        |
        v
docs/shepherd-channel/fixtures/*.json, plus a wire file per language
        |  shep's own CI fails if the committed copy is stale
        v
opens a pull request in shep-js, shep-py and shep-go when the bytes change
        |
        v
that repo's CI runs its suite against the new fixtures and goes red
until the library is updated
```

The redness is the enforcement. A generated pull request that nobody acts on
leaves a repo failing, which is visible; a vendored copy that silently drifts
is not.

Rust needs none of this, because D10 leaves one definition rather than two.

**The credential is a maintainer action, not an implementation step.** A
fine-grained PAT or a GitHub App installed on `shep-pm` with contents and
pull-requests write on the three library repos, held as a secret in `shep`.
The workflow opens pull requests rather than pushing, so a wire change is
visible in three public repos before it lands.

## What 0.1.0 leaves out

**The dog client.** The two jobs are not the same size. The channel is five
variants across 244 lines and `SHEP_CHANNEL_VERSION` has never moved. A dog
speaks the client wire protocol: 76 `Request` variants across 3057 lines in
`crates/shep-core/src/protocol/request.rs`, plus a handshake, a codec, two
transports, and the `--version` contract Phase 4 added. `PROTOCOL_VERSION`
is already at 2 and a mismatch is refused at the handshake, so that wire has
broken once and will again.

Publishing it later costs almost nothing, because the repos are laid out for
it now. Publishing it at 0.1.0 would commit 76 immutable shapes per language
through a generator that has never run in anger. The channel is the right
thing to exercise the pipeline on.

**Async variants**, per D2. **Typed action names**, per D11.

## Open questions for the maintainer

1. Whether third-party dogs in languages other than Rust are wanted at all.
   Every published dog client is a constraint on future `PROTOCOL_VERSION`
   bumps, and that is a maintenance commitment rather than a technical one.
2. Whether `shep-py` should publish one distribution with platform wheels
   (the ruff and uv shape) instead of the two-distribution split above. The
   split keeps the pure-Python library installable anywhere; one
   distribution is a simpler install line.
