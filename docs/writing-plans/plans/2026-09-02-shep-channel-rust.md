# The shepherd-channel client libraries, plan 1 of 5: the Rust crate

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` to implement this task-by-task. Steps use `- [ ]` for tracking.

**Goal:** Ship `shep-channel`, a crate an app depends on to speak the shepherd channel without hand-rolling the framing, and make it the single definition of that wire for this workspace.

**Architecture:** A leaf crate holding the two wire enums, which `shep-core` then depends on and re-exports rather than defining. On top of the types, two layers: a raw `Channel` with `open`/`recv`/`send` and no threads, and a `serve()` handler layer that owns a reader thread and a writer thread and guarantees the reply rule the contract asks apps for. No tokio.

**Tech Stack:** Rust 1.88, edition 2024, serde, serde_json. Standard library threads and synchronisation, no async runtime, no new workspace dependency.

**Spec:** `docs/brainstorming/specs/2026-09-02-shep-client-libraries-design.md`. This plan implements D1 to D8 and D10, plus the Testing and Windows sections.

**Base:** cut from `origin/main` after the spec commits (`d065f21`, `c33d044`).

## Where this plan sits

Five plans. This is the first, and every later one consumes what it produces.

| plan | repo | produces |
|---|---|---|
| **1, this one** | `shep-pm/shep` | `crates/shep-channel`, the fixture corpus, one definition of the wire |
| 2 | `shep-pm/shep-go` | `channel` package, reads the fixtures |
| 3 | `shep-pm/shep-js` | `@shep-pm/channel` and `@shep-pm/cli` |
| 4 | `shep-pm/shep-py` | `shep-pm` and `shep-cli` |
| 5 | `shep-pm/shep` | the generator and the cross-repo propagation workflow |

Plans 2 through 4 cannot start until Task 2 below lands, because the fixture corpus is the thing they are written against.

## Global constraints

- MSRV 1.88, edition 2024. `version`, `edition`, `rust-version`, `repository` and `license` all come from `[workspace.package]`.
- `[lints] workspace = true` in the new crate. That turns on `missing_docs = "deny"`, `missing_debug_implementations = "deny"`, `missing_errors_doc = "deny"` and `undocumented_unsafe_blocks = "deny"`. Every public item needs a doc comment, every `Result`-returning public item needs a `# Errors` section, and every type needs a deliberate `Debug`.
- The `shep-idiomatic-rust` skill fronts IR-1 to IR-46 and must be invoked before writing any Rust here. IR-11, IR-20, IR-22, IR-23, IR-26 and IR-41 all bite in this plan.
- **`shep-channel` cannot carry `#![forbid(unsafe_code)]`.** There is no safe constructor from a raw descriptor. It carries `#![deny(unsafe_code)]` and exactly one `#[allow(unsafe_code)]` site with a `// SAFETY:` comment, the rule `crates/shep-daemon/src/sys.rs` already follows.
- `CHANNEL_VERSION` stays `"1"`. Nothing in this plan changes a wire byte. `PROTOCOL_VERSION` is untouched and unrelated.
- No new workspace dependency. serde and serde_json are already in `[workspace.dependencies]`.
- Adding a crate to `[workspace.dependencies]` needs a literal `version = "0.1.25"` beside the `path`, because `cargo publish` strips the path. The workspace manifest says so at `Cargo.toml:27`; a release bump touches both places.
- Inner loop for this crate: `cargo test -p shep-channel --lib --all-features`. One cargo shape per task. Do not alternate `-p` with `--workspace` inside a task; the workspace shares one target-dir lock and switching shapes re-resolves features and rebuilds.
- The task gate at the end of each task is the four commands in `CLAUDE.md`, run one at a time with `$?` captured directly rather than through a pipe.

## Plan snippets about existing code are approximations

Every code block below that shows code **being added** is a specification: write it as given unless it does not compile, and say so if it does not. Every block that describes code **already in the tree** was read on 2026-09-02 and may have moved. Grep for it rather than trusting the line number, and report the difference instead of quietly working around it.

## File structure

```
crates/shep-channel/
  Cargo.toml          new
  README.md           new, required by `readme = "README.md"`
  fixtures/*.json     new, Task 2, the corpus plans 2-4 read
  src/
    lib.rs            crate docs, serve(), the Shepherd handle
    wire.rs           ChildMessage, ShepherdMessage, CHANNEL_VERSION. No feature gate.
    endpoint.rs       descriptor discovery and the platform transport. Feature "client".
    outbox.rs         the bounded queue and its drop policy. Feature "client".
    session.rs        the read loop and action dispatch, generic over BufRead. Feature "client".

crates/shep-core/
  Cargo.toml                   modify: depend on shep-channel, default-features = false
  src/protocol/channel.rs      delete, replaced by a re-export
  src/protocol/mod.rs          modify: re-export from shep-channel

Cargo.toml            modify: members, [workspace.dependencies]
release-plz.toml      modify: add shep-channel to the shep version group
docs/shepherd-channel.md  modify: name the crate
```

`session.rs` is generic over `BufRead` and `Write` on purpose. That is what makes the read loop, the dispatch and the drop policy testable on every platform, including the Windows leg where a named pipe cannot be faked with the standard library alone.

---

## Task 1: Extract the wire types into a leaf crate

The workspace ends this task building and passing exactly as it does now, with the two enums defined in one new place instead of in `shep-core`.

**Files:**
- Create: `crates/shep-channel/Cargo.toml`
- Create: `crates/shep-channel/src/lib.rs`
- Create: `crates/shep-channel/src/wire.rs`
- Create: `crates/shep-channel/README.md`
- Delete: `crates/shep-core/src/protocol/channel.rs`
- Modify: `crates/shep-core/src/protocol/mod.rs`
- Modify: `crates/shep-core/Cargo.toml`
- Modify: `Cargo.toml`

**Interfaces:**
- Produces: `shep_channel::{ChildMessage, ShepherdMessage, CHANNEL_VERSION}`. `ChildMessage::{Ready, Metric { name: String, value: f64 }, ActionReply { action: String, body: String, id: Option<u64> }}`. `ShepherdMessage::{Shutdown, Action { name: String, params: Option<String>, id: u64 }}`. Both derive `Debug, Clone, PartialEq, Serialize, Deserialize`, both are deliberately NOT `#[non_exhaustive]`.
- Produces: `shep_core::protocol::{ChildMessage, ShepherdMessage, CHANNEL_VERSION}` continue to resolve, so the 14 files naming them do not change.

- [ ] **Step 1: Read the file being moved, and the reason it is not `#[non_exhaustive]`**

Read `crates/shep-core/src/protocol/channel.rs` end to end before touching anything. Its module doc argues that both enums are deliberately exhaustive, unlike everything else under `protocol`, because a new variant is a change every app speaking the wire must be told about out of band. That argument survives the move and the doc comment moves with it, edited only where it says "this lives in shep-core".

- [ ] **Step 2: Create the crate manifest**

`crates/shep-channel/Cargo.toml`:

```toml
[package]
name = "shep-channel"
description = "Client for the shep shepherd channel: readiness, metrics and custom actions over the descriptor shep hands a supervised process"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
repository.workspace = true
license.workspace = true
readme = "README.md"
documentation = "https://docs.rs/shep-channel"
keywords = ["shep", "process-manager", "ipc", "readiness", "metrics"]
categories = ["api-bindings", "os"]

[package.metadata.docs.rs]
all-features = true

[features]
# On by default. The client: descriptor discovery, the reader and writer
# threads, and the handler layer. `shep-core` takes this crate with
# `default-features = false` because it wants the wire types and nothing
# else; an app wants the whole thing and says nothing.
default = ["client"]
client = ["dep:serde_json"]

[dependencies]
serde.workspace = true
serde_json = { workspace = true, optional = true }

[lints]
workspace = true
```

- [ ] **Step 3: Move the types**

`git mv crates/shep-core/src/protocol/channel.rs crates/shep-channel/src/wire.rs`, then edit the module doc: drop the "Why this lives in shep-core" section and replace it with the paragraph below, keeping the `#[non_exhaustive]` argument intact.

```rust
//! # Why these two enums are not `#[non_exhaustive]`
//!
//! There is no handshake on the shepherd channel and no version to
//! negotiate. `CHANNEL_VERSION` is a stamp rather than a negotiation, so a
//! new variant here is a change every app speaking this wire has to be told
//! about out of band. Leaving them exhaustive means the compiler names every
//! site that has to decide something, which is exactly the review a change
//! on this wire deserves.
```

- [ ] **Step 4: Write the crate root**

`crates/shep-channel/src/lib.rs`, for this task only. Later tasks add to it.

```rust
//! Speak the shep shepherd channel: signal readiness, emit a metric, answer
//! an action.
//!
//! An app supervised by shep can be handed a descriptor carrying
//! newline-delimited JSON in both directions. This crate finds that
//! descriptor, frames the JSON, and answers the messages an app does not
//! handle itself. `docs/shepherd-channel.md` in the shep repository is the
//! contract this implements.
//!
//! Doing nothing is the normal case: an app whose operator never asked for a
//! channel gets a handle whose every call is a no-op.

#![doc(test(attr(deny(warnings))))]
// Not `forbid`: taking the inherited descriptor needs one `unsafe` block,
// because the standard library has no safe constructor from a raw
// descriptor. That single site carries its own `// SAFETY:` and the
// workspace denies `undocumented_unsafe_blocks`, so it cannot lose it.
#![deny(unsafe_code)]

mod wire;

pub use wire::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
```

- [ ] **Step 5: Point shep-core at it**

In `crates/shep-core/Cargo.toml`, under `[dependencies]`:

```toml
# The shepherd channel's wire types, and their only definition. This crate
# re-exports them because `BusEvent::Channel` carries a `ChildMessage`
# verbatim to every bus subscriber, so the message a bus event holds has to
# be the same type an app writing on fd 3 constructs. `default-features =
# false` takes the types without the client: nothing in the daemon opens fd 3
# from the app side.
shep-channel = { workspace = true, default-features = false }
```

In `crates/shep-core/src/protocol/mod.rs`, replace `pub mod channel;` and its `pub use` line with:

```rust
pub use shep_channel::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
```

In the workspace `Cargo.toml`, add `"crates/shep-channel"` to `members` and this to `[workspace.dependencies]`, beside the other three path entries:

```toml
shep-channel = { path = "crates/shep-channel", version = "0.1.25" }
```

- [ ] **Step 6: Verify nothing else moved**

Run: `cargo test -p shep-core --lib --all-features`
Expected: PASS, including the five wire fixture tests that moved with the file (`ready_wire_fixture_round_trips`, `metric_wire_fixture_round_trips`, `an_action_reply_without_an_id_round_trips`, `an_action_reply_with_an_echoed_id_round_trips`, `shutdown_wire_fixture_round_trips`, `an_action_carries_its_id_with_or_without_params`).

Then, once, to prove the 14 files naming these types still compile:

Run: `cargo test --workspace --all-features`
Expected: PASS, no change in count except the tests that moved crate.

If `shep_core::protocol::channel::` is named anywhere by path rather than through the re-export, the compiler says so. Fix those call sites to use the re-export rather than re-adding the module.

- [ ] **Step 7: Prove the extraction is not vacuous**

Change `CHANNEL_VERSION` in `crates/shep-channel/src/wire.rs` from `"1"` to `"2"`. Run `cargo test -p shep-core --lib --all-features` and confirm a test fails, which proves shep-core is reading the new crate rather than a stale copy. Restore `"1"`.

- [ ] **Step 8: Commit**

```bash
git add crates/shep-channel crates/shep-core Cargo.toml
git commit -m "refactor(core,channel): move the shepherd-channel wire into its own crate"
```

---

## Task 2: The fixture corpus

The bytes plans 2 through 4 are written against, generated from the real serde impls rather than typed by hand, with a test that fails when the committed copy goes stale.

**Files:**
- Create: `crates/shep-channel/fixtures/*.json`
- Create: `crates/shep-channel/tests/fixtures.rs`
- Modify: `crates/shep-channel/Cargo.toml`

**Interfaces:**
- Consumes: `shep_channel::{ChildMessage, ShepherdMessage}` from Task 1.
- Produces: `crates/shep-channel/fixtures/`, one file per case, each holding a single line of JSON with no trailing newline. Plans 2 to 4 read this directory verbatim.

- [ ] **Step 1: Write the failing test**

`crates/shep-channel/tests/fixtures.rs`:

```rust
//! The fixture corpus, and the test that keeps it honest.
//!
//! Each case is serialized from this crate's own types and compared byte for
//! byte with the committed file, then read back and compared with the value.
//! Both directions, because the Go, JavaScript and Python libraries are
//! written against these bytes and a drift in either direction would reach
//! them silently.
//!
//! Regenerate with `SHEP_CHANNEL_BLESS=1 cargo test -p shep-channel --test fixtures`.

use std::fs;
use std::path::PathBuf;

use shep_channel::{ChildMessage, ShepherdMessage};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn check(name: &str, encoded: String) {
    let path = fixtures_dir().join(format!("{name}.json"));
    if std::env::var_os("SHEP_CHANNEL_BLESS").is_some() {
        fs::create_dir_all(fixtures_dir()).expect("create fixtures dir");
        fs::write(&path, &encoded).expect("write fixture");
        return;
    }
    let committed = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("{}: {error}. Run with SHEP_CHANNEL_BLESS=1 to create it.", path.display())
    });
    assert_eq!(
        committed, encoded,
        "{} is stale. Three other libraries are written against these bytes.",
        path.display()
    );
}

#[test]
fn child_messages_match_their_fixtures() {
    let cases: Vec<(&str, ChildMessage)> = vec![
        ("child-ready", ChildMessage::Ready),
        ("child-metric", ChildMessage::Metric { name: "rps".into(), value: 42.0 }),
        ("child-action-reply", ChildMessage::ActionReply {
            action: "gc".into(), body: "ok".into(), id: None,
        }),
        ("child-action-reply-id", ChildMessage::ActionReply {
            action: "gc".into(), body: "ok".into(), id: Some(7),
        }),
    ];
    for (name, value) in cases {
        let encoded = serde_json::to_string(&value).expect("encode");
        check(name, encoded.clone());
        let decoded: ChildMessage = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, value, "{name} does not survive a round trip");
    }
}

#[test]
fn shepherd_messages_match_their_fixtures() {
    let cases: Vec<(&str, ShepherdMessage)> = vec![
        ("shepherd-shutdown", ShepherdMessage::Shutdown),
        ("shepherd-action", ShepherdMessage::Action {
            name: "gc".into(), params: None, id: 7,
        }),
        ("shepherd-action-params", ShepherdMessage::Action {
            name: "set-log-level".into(), params: Some("debug".into()), id: 8,
        }),
    ];
    for (name, value) in cases {
        let encoded = serde_json::to_string(&value).expect("encode");
        check(name, encoded.clone());
        let decoded: ShepherdMessage = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, value, "{name} does not survive a round trip");
    }
}

/// fails if an `action-reply` carrying no `id` stops decoding. Every app
/// written before the correlation id existed sends this shape, and the
/// daemon's name-and-order fallback exists for exactly it.
#[test]
fn an_action_reply_without_an_id_still_decodes() {
    let decoded: ChildMessage =
        serde_json::from_str(r#"{"kind":"action-reply","action":"gc","body":"ok"}"#)
            .expect("decode");
    assert_eq!(
        decoded,
        ChildMessage::ActionReply { action: "gc".into(), body: "ok".into(), id: None }
    );
}
```

Add to `crates/shep-channel/Cargo.toml`:

```toml
[dev-dependencies]
serde_json.workspace = true
```

- [ ] **Step 2: Run it to watch it fail**

Run: `cargo test -p shep-channel --test fixtures`
Expected: FAIL, four panics naming a missing `fixtures/*.json` and telling you to bless.

- [ ] **Step 3: Generate the corpus**

Run: `SHEP_CHANNEL_BLESS=1 cargo test -p shep-channel --test fixtures`
Expected: PASS.

Then read every generated file. They must be exactly these seven, one line each, no trailing newline:

```
child-ready.json           {"kind":"ready"}
child-metric.json          {"kind":"metric","name":"rps","value":42.0}
child-action-reply.json    {"kind":"action-reply","action":"gc","body":"ok"}
child-action-reply-id.json {"kind":"action-reply","action":"gc","body":"ok","id":7}
shepherd-shutdown.json     {"kind":"shutdown"}
shepherd-action.json       {"kind":"action","name":"gc","id":7}
shepherd-action-params.json {"kind":"action","name":"set-log-level","params":"debug","id":8}
```

If any file differs from that, stop. Either the types drifted or the fixture names did, and both matter to three other repositories.

- [ ] **Step 4: Run again without blessing**

Run: `cargo test -p shep-channel --test fixtures`
Expected: PASS, now comparing against the committed files.

- [ ] **Step 5: Prove the staleness check is not vacuous**

Edit `fixtures/child-metric.json`, changing `42.0` to `43.0`. Run the test and confirm `child_messages_match_their_fixtures` fails with the stale message. Restore the file with `git checkout -- crates/shep-channel/fixtures/child-metric.json`.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-channel/fixtures crates/shep-channel/tests/fixtures.rs crates/shep-channel/Cargo.toml
git commit -m "test(channel): generate the wire fixture corpus from the real serde impls"
```

---

## Task 3: Framing and the descriptor, as separable pieces

The read and write logic goes in a module generic over `BufRead` and `Write`, so it is fully covered on every platform including the Windows leg where a named pipe cannot be faked with the standard library. The platform-specific part shrinks to one function that produces a transport.

**Files:**
- Create: `crates/shep-channel/src/session.rs`
- Create: `crates/shep-channel/src/endpoint.rs`
- Modify: `crates/shep-channel/src/lib.rs`

**Interfaces:**
- Consumes: `wire::{ChildMessage, ShepherdMessage}` from Task 1.
- Produces: `shep_channel::{Channel, ChannelError, Endpoint}`. `Channel::open() -> Result<Option<Channel>, ChannelError>`, `Channel::recv(&mut self) -> Result<Option<ShepherdMessage>, ChannelError>`, `Channel::send(&mut self, &ChildMessage) -> Result<(), ChannelError>`, `Channel::version(&self) -> Option<&str>`.
- Produces, crate-internal: `session::read_message<R: BufRead>`, `session::write_message<W: Write>`, `endpoint::Transport`.

- [ ] **Step 1: Write the failing framing tests**

`crates/shep-channel/src/session.rs`:

```rust
//! Reading and writing one newline-delimited JSON message.
//!
//! Generic over `BufRead` and `Write` rather than over the transport,
//! because that is what lets these tests run on a platform where the real
//! transport cannot be constructed without a live shepherd.

use std::io::{BufRead, Write};

use crate::{ChannelError, ChildMessage, ShepherdMessage};

/// Reads one message. `Ok(None)` is end of stream.
pub(crate) fn read_message<R: BufRead>(
    reader: &mut R,
) -> Result<Option<ShepherdMessage>, ChannelError> {
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(ChannelError::Io)? == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end_matches(['\n', '\r']);
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|error| ChannelError::Malformed(error.to_string()))
}

/// Writes one message and its newline, then flushes.
pub(crate) fn write_message<W: Write>(
    writer: &mut W,
    message: &ChildMessage,
) -> Result<(), ChannelError> {
    let mut line = serde_json::to_vec(message).map_err(|e| ChannelError::Malformed(e.to_string()))?;
    line.push(b'\n');
    writer.write_all(&line).map_err(ChannelError::Io)?;
    writer.flush().map_err(ChannelError::Io)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn reads_two_messages_from_one_buffer() {
        let mut reader = Cursor::new(
            "{\"kind\":\"shutdown\"}\n{\"kind\":\"action\",\"name\":\"gc\",\"id\":7}\n".as_bytes(),
        );
        assert_eq!(read_message(&mut reader).unwrap(), Some(ShepherdMessage::Shutdown));
        assert_eq!(
            read_message(&mut reader).unwrap(),
            Some(ShepherdMessage::Action { name: "gc".into(), params: None, id: 7 })
        );
        assert_eq!(read_message(&mut reader).unwrap(), None);
    }

    /// fails if a `\r\n` line ending reaches serde. The Windows transport is
    /// a byte-mode pipe and an app on the far side may well write one.
    #[test]
    fn a_carriage_return_before_the_newline_is_tolerated() {
        let mut reader = Cursor::new("{\"kind\":\"shutdown\"}\r\n".as_bytes());
        assert_eq!(read_message(&mut reader).unwrap(), Some(ShepherdMessage::Shutdown));
    }

    /// fails if a malformed line ends the stream instead of being one
    /// recoverable error. The daemon skips a bad frame and keeps reading
    /// (`tokio_runner.rs`, the channel pumps); this side must be able to do
    /// the same or the two halves disagree about what a bad line costs.
    #[test]
    fn a_malformed_line_is_recoverable() {
        let mut reader = Cursor::new("not json\n{\"kind\":\"shutdown\"}\n".as_bytes());
        assert!(matches!(read_message(&mut reader), Err(ChannelError::Malformed(_))));
        assert_eq!(read_message(&mut reader).unwrap(), Some(ShepherdMessage::Shutdown));
    }

    #[test]
    fn writes_one_line_per_message_with_a_trailing_newline() {
        let mut out = Vec::new();
        write_message(&mut out, &ChildMessage::Ready).unwrap();
        write_message(&mut out, &ChildMessage::Metric { name: "rps".into(), value: 42.0 })
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"kind\":\"ready\"}\n{\"kind\":\"metric\",\"name\":\"rps\",\"value\":42.0}\n"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p shep-channel --lib --all-features`
Expected: FAIL to compile, `ChannelError` is not defined yet.

- [ ] **Step 3: Add the error type and the endpoint**

In `crates/shep-channel/src/lib.rs`:

```rust
/// Anything that can go wrong on the shepherd channel.
///
/// `#[non_exhaustive]` per IR-20: this is on the peer-facing surface and the
/// channel will grow reasons to fail.
#[non_exhaustive]
#[derive(Debug)]
pub enum ChannelError {
    /// The transport failed. Carries the underlying error.
    Io(std::io::Error),
    /// One frame could not be encoded or decoded. Carries serde's message.
    ///
    /// Recoverable: the frame is lost and the next call resumes at the next
    /// line, which is what the daemon does with a bad frame in the other
    /// direction.
    Malformed(String),
    /// The environment names a channel this platform cannot open, for
    /// example `SHEP_CHANNEL_PIPE` on unix. Carries the variable and value.
    Unusable(String),
    /// The writer has stopped and the message was not queued.
    Closed,
}

impl core::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "shepherd channel I/O failed: {error}"),
            Self::Malformed(message) => write!(f, "malformed shepherd-channel frame: {message}"),
            Self::Unusable(what) => write!(f, "unusable shepherd channel: {what}"),
            Self::Closed => f.write_str("the shepherd channel is closed"),
        }
    }
}

impl core::error::Error for ChannelError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
```

`crates/shep-channel/src/endpoint.rs`:

```rust
//! Finding the channel this process was given, and opening it.
//!
//! Branch on which environment variable is present, never on the platform.
//! The daemon sets exactly one of them and never both: `SHEP_CHANNEL_FD` on
//! unix, `SHEP_CHANNEL_PIPE` on Windows. Neither means no channel was opened
//! for this process, which is the ordinary case.

use std::path::PathBuf;

use crate::ChannelError;

/// The descriptor number variable, set on unix only.
pub const FD_VAR: &str = "SHEP_CHANNEL_FD";
/// The named pipe variable, set on Windows only.
pub const PIPE_VAR: &str = "SHEP_CHANNEL_PIPE";
/// The wire version stamp, set on both platforms whenever a channel exists.
pub const VERSION_VAR: &str = "SHEP_CHANNEL_VERSION";

/// Where this process's channel is, if it has one.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// An inherited descriptor number, from `SHEP_CHANNEL_FD`.
    Descriptor(i32),
    /// A named pipe path, from `SHEP_CHANNEL_PIPE`.
    Pipe(PathBuf),
    /// Neither variable is set. Not an error.
    Absent,
}

/// Reads the environment and says where the channel is.
///
/// # Errors
///
/// [`ChannelError::Unusable`] when `SHEP_CHANNEL_FD` is set to something
/// that is not a descriptor number. That is a broken environment rather
/// than an absent channel, and saying so beats silently doing nothing.
pub fn discover() -> Result<Endpoint, ChannelError> {
    if let Some(raw) = std::env::var_os(FD_VAR) {
        let text = raw.to_string_lossy().into_owned();
        return text
            .trim()
            .parse::<i32>()
            .map(Endpoint::Descriptor)
            .map_err(|_| ChannelError::Unusable(format!("{FD_VAR}={text}")));
    }
    if let Some(raw) = std::env::var_os(PIPE_VAR) {
        return Ok(Endpoint::Pipe(PathBuf::from(raw)));
    }
    Ok(Endpoint::Absent)
}

/// The duplex this platform carries the channel on.
#[cfg(unix)]
pub(crate) type Transport = std::os::unix::net::UnixStream;
/// The duplex this platform carries the channel on.
#[cfg(windows)]
pub(crate) type Transport = std::fs::File;

/// Opens the endpoint, returning the transport and a clone for the writer.
///
/// # Errors
///
/// - [`ChannelError::Unusable`] when the endpoint names a mechanism this
///   platform does not have.
/// - [`ChannelError::Io`] when the pipe cannot be opened or the descriptor
///   cannot be cloned.
pub(crate) fn connect(endpoint: &Endpoint) -> Result<(Transport, Transport), ChannelError> {
    let transport = match endpoint {
        #[cfg(unix)]
        Endpoint::Descriptor(fd) => {
            use std::os::fd::FromRawFd as _;
            // SAFETY: the shepherd hands this process exactly one descriptor
            // for the channel and names its number in `SHEP_CHANNEL_FD`.
            // Nothing else in this crate touches that number, and `serve` is
            // a process singleton, so this constructor runs at most once per
            // descriptor and takes sole ownership of it. A number that is not
            // ours produces an error on first use rather than undefined
            // behaviour, because the standard library's socket calls check.
            #[allow(unsafe_code)]
            unsafe {
                Transport::from_raw_fd(*fd)
            }
        }
        #[cfg(windows)]
        Endpoint::Pipe(path) => std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(ChannelError::Io)?,
        #[cfg(unix)]
        Endpoint::Pipe(path) => {
            return Err(ChannelError::Unusable(format!(
                "{PIPE_VAR}={} names a Windows named pipe and this is not Windows",
                path.display()
            )));
        }
        #[cfg(windows)]
        Endpoint::Descriptor(fd) => {
            return Err(ChannelError::Unusable(format!(
                "{FD_VAR}={fd} names an inherited descriptor and Windows does not inherit one"
            )));
        }
        Endpoint::Absent => {
            return Err(ChannelError::Unusable(
                "no channel: neither variable is set".to_string(),
            ));
        }
    };
    let writer = transport.try_clone().map_err(ChannelError::Io)?;
    Ok((transport, writer))
}
```

- [ ] **Step 4: Add `Channel` over those pieces**

In `crates/shep-channel/src/lib.rs`:

```rust
/// The channel with no threads: you own the loop.
///
/// [`serve`] is the other road and the documented default, because it
/// answers the messages you did not register a handler for. Reach for this
/// when your app already has an event loop and wants the channel inside it.
#[derive(Debug)]
pub struct Channel {
    reader: std::io::BufReader<endpoint::Transport>,
    writer: endpoint::Transport,
    version: Option<String>,
}

impl Channel {
    /// Opens this process's channel, or `Ok(None)` when it has none.
    ///
    /// # Errors
    ///
    /// - [`ChannelError::Unusable`] when the environment names a channel
    ///   that cannot be opened here.
    /// - [`ChannelError::Io`] when the transport cannot be opened.
    pub fn open() -> Result<Option<Self>, ChannelError> {
        let found = endpoint::discover()?;
        if found == endpoint::Endpoint::Absent {
            return Ok(None);
        }
        let (reader, writer) = endpoint::connect(&found)?;
        Ok(Some(Self {
            reader: std::io::BufReader::new(reader),
            writer,
            version: std::env::var(endpoint::VERSION_VAR).ok(),
        }))
    }

    /// Reads one message. `Ok(None)` is the shepherd closing its end.
    ///
    /// # Errors
    ///
    /// - [`ChannelError::Malformed`] for one unparseable line. Recoverable:
    ///   call again to resume at the next line.
    /// - [`ChannelError::Io`] when the transport fails.
    pub fn recv(&mut self) -> Result<Option<ShepherdMessage>, ChannelError> {
        session::read_message(&mut self.reader)
    }

    /// Writes one message and flushes it.
    ///
    /// # Errors
    ///
    /// - [`ChannelError::Io`] when the transport fails.
    /// - [`ChannelError::Malformed`] when the message cannot be encoded.
    pub fn send(&mut self, message: &ChildMessage) -> Result<(), ChannelError> {
        session::write_message(&mut self.writer, message)
    }

    /// The `SHEP_CHANNEL_VERSION` stamp, when the shepherd set one.
    ///
    /// A stamp, not a negotiation: the shepherd cannot ask what this app
    /// speaks. It is here so an app can notice a wire it has never seen.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}
```

Add this to the crate root. **The gates are load-bearing, not decoration:** `shep-core` takes this crate with `default-features = false`, so anything reaching for `serde_json` has to be behind `client` or shep-core stops compiling.

```rust
#[cfg(feature = "client")]
mod endpoint;
#[cfg(feature = "client")]
mod session;

#[cfg(feature = "client")]
pub use endpoint::{Endpoint, FD_VAR, PIPE_VAR, VERSION_VAR};
```

`ChannelError` and `Channel` from Steps 3 and 4 take the same `#[cfg(feature = "client")]`. Every module Tasks 4, 5 and 6 add (`outbox`, `dispatch`) and everything they export takes it too. Only `wire` is ungated.

Verify the gate with the one command that actually exercises it:

Run: `cargo check -p shep-channel --no-default-features`
Expected: EXIT=0. If it fails on a missing `serde_json`, a gate is missing.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p shep-channel --lib --all-features`
Expected: PASS, four session tests.

- [ ] **Step 6: Add the unix round trip over a real socketpair**

Append to `session.rs`'s test module:

```rust
    /// fails if `Channel` cannot drive a real duplex. The generic tests
    /// above prove the framing; this proves the type wired to a socket.
    #[cfg(unix)]
    #[test]
    fn a_channel_over_a_socketpair_round_trips() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::os::unix::net::UnixStream;

        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let mut channel = crate::Channel {
            reader: BufReader::new(ours.try_clone().expect("clone")),
            writer: ours,
            version: Some("1".to_string()),
        };
        let mut shepherd = BufReader::new(theirs.try_clone().expect("clone"));
        let mut shepherd_writer = theirs;

        shepherd_writer
            .write_all(b"{\"kind\":\"action\",\"name\":\"gc\",\"id\":7}\n")
            .expect("write");
        assert_eq!(
            channel.recv().expect("recv"),
            Some(ShepherdMessage::Action { name: "gc".into(), params: None, id: 7 })
        );

        channel
            .send(&ChildMessage::ActionReply {
                action: "gc".into(),
                body: "ok".into(),
                id: Some(7),
            })
            .expect("send");
        let mut back = String::new();
        shepherd.read_line(&mut back).expect("read");
        assert_eq!(back, "{\"kind\":\"action-reply\",\"action\":\"gc\",\"body\":\"ok\",\"id\":7}\n");
    }
```

That test names `Channel`'s private fields, so it has to live in this crate. It reads a fixed number of lines with no waiting, so there is nothing to hang on.

- [ ] **Step 7: Prove the framing tests are not vacuous**

Remove the `.trim_end_matches(['\n', '\r'])` in `read_message` and run the suite. `a_carriage_return_before_the_newline_is_tolerated` must fail. Restore it. Then delete the `line.push(b'\n')` in `write_message` and confirm `writes_one_line_per_message_with_a_trailing_newline` fails. Restore.

- [ ] **Step 8: Commit**

```bash
git add crates/shep-channel/src
git commit -m "feat(channel): frame the wire and find the descriptor"
```

---

## Task 4: The outbox and its split drop policy

D4: a metric never blocks the app and may be dropped; readiness and replies block until queued because losing either is visible to an operator. Pure logic, no I/O, so it is covered identically on every platform.

**Files:**
- Create: `crates/shep-channel/src/outbox.rs`
- Modify: `crates/shep-channel/src/lib.rs`

**Interfaces:**
- Produces, crate-internal: `outbox::Outbox` with `new(capacity)`, `push_lossy(ChildMessage)`, `push_blocking(ChildMessage) -> Result<(), ChannelError>`, `pop() -> Option<ChildMessage>`, `close()`, `dropped() -> u64`.

- [ ] **Step 1: Write the failing tests**

`crates/shep-channel/src/outbox.rs`:

```rust
//! The queue between the app's threads and the one thread that writes.
//!
//! Two push policies, because two kinds of message have different costs when
//! they go missing. A dropped metric costs nothing today: the shepherd logs
//! metrics at debug level and no dog reads them. A dropped `ready` hangs a
//! `wait_ready` gate, and a dropped reply costs an operator the whole
//! `action_timeout`. So metrics are lossy and never block the caller, and
//! everything else waits for room.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, PoisonError};

use crate::{ChannelError, ChildMessage};

/// How many messages may wait for the writer before the policy applies.
pub(crate) const DEFAULT_CAPACITY: usize = 1024;

#[derive(Debug)]
struct Inner {
    queue: VecDeque<ChildMessage>,
    dropped: u64,
    closed: bool,
}

/// The bounded queue the writer thread drains.
#[derive(Debug)]
pub(crate) struct Outbox {
    inner: Mutex<Inner>,
    capacity: usize,
    /// Signalled when a message is queued, or the outbox closes.
    queued: Condvar,
    /// Signalled when a message leaves, or the outbox closes.
    drained: Condvar,
}

impl Outbox {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner { queue: VecDeque::new(), dropped: 0, closed: false }),
            capacity,
            queued: Condvar::new(),
            drained: Condvar::new(),
        }
    }

    /// Queues a message that may be dropped. Never blocks, never fails.
    ///
    /// On a full queue the oldest waiting message is discarded and counted.
    /// Oldest rather than newest: a metric's value is a sample, and the
    /// newer sample is the one worth keeping.
    pub(crate) fn push_lossy(&self, message: ChildMessage) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.closed {
            return;
        }
        if inner.queue.len() >= self.capacity {
            inner.queue.pop_front();
            inner.dropped = inner.dropped.saturating_add(1);
        }
        inner.queue.push_back(message);
        self.queued.notify_one();
    }

    /// Queues a message that must not be lost, waiting for room.
    ///
    /// # Errors
    ///
    /// [`ChannelError::Closed`] when the outbox closes while waiting, which
    /// is the shepherd having gone away.
    pub(crate) fn push_blocking(&self, message: ChildMessage) -> Result<(), ChannelError> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        while !inner.closed && inner.queue.len() >= self.capacity {
            inner = self.drained.wait(inner).unwrap_or_else(PoisonError::into_inner);
        }
        if inner.closed {
            return Err(ChannelError::Closed);
        }
        inner.queue.push_back(message);
        self.queued.notify_one();
        Ok(())
    }

    /// Takes the next message, waiting for one. `None` once closed and empty.
    pub(crate) fn pop(&self) -> Option<ChildMessage> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        while inner.queue.is_empty() && !inner.closed {
            inner = self.queued.wait(inner).unwrap_or_else(PoisonError::into_inner);
        }
        let taken = inner.queue.pop_front();
        if taken.is_some() {
            self.drained.notify_one();
        }
        taken
    }

    /// Releases every waiter. Idempotent.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.closed = true;
        drop(inner);
        self.queued.notify_all();
        self.drained.notify_all();
    }

    /// How many messages `push_lossy` has discarded.
    pub(crate) fn dropped(&self) -> u64 {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).dropped
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// Every wait in this module's tests is bounded by this. A working
    /// outbox answers in microseconds; this is slack for a loaded runner,
    /// not an expected duration.
    const DEADLINE: Duration = Duration::from_secs(5);

    fn metric(value: f64) -> ChildMessage {
        ChildMessage::Metric { name: "rps".into(), value }
    }

    #[test]
    fn a_full_outbox_drops_the_oldest_metric_and_counts_it() {
        let outbox = Outbox::new(2);
        outbox.push_lossy(metric(1.0));
        outbox.push_lossy(metric(2.0));
        outbox.push_lossy(metric(3.0));

        assert_eq!(outbox.dropped(), 1);
        assert_eq!(outbox.pop(), Some(metric(2.0)));
        assert_eq!(outbox.pop(), Some(metric(3.0)));
    }

    /// fails if `push_blocking` returns while the queue is full. The forcing
    /// mechanism is the channel: the pusher reports only after it returns,
    /// so a `recv_timeout` that times out proves it is still waiting, and
    /// the `pop` that follows is the explicit transition that releases it.
    #[test]
    fn a_must_deliver_push_waits_for_room_and_then_proceeds() {
        let outbox = Arc::new(Outbox::new(1));
        outbox.push_blocking(ChildMessage::Ready).expect("first fits");

        let (tx, rx) = mpsc::channel();
        let pusher = Arc::clone(&outbox);
        let handle = std::thread::spawn(move || {
            let outcome = pusher.push_blocking(ChildMessage::Ready);
            tx.send(outcome).expect("report");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "push_blocking returned while the outbox was full"
        );

        assert_eq!(outbox.pop(), Some(ChildMessage::Ready));
        rx.recv_timeout(DEADLINE).expect("pusher did not proceed").expect("push after room");
        handle.join().expect("pusher panicked");
    }

    /// fails if closing leaves a blocked pusher parked. Without this an app
    /// whose shepherd went away hangs on `ready()` forever.
    #[test]
    fn closing_releases_a_blocked_push_with_an_error() {
        let outbox = Arc::new(Outbox::new(1));
        outbox.push_blocking(ChildMessage::Ready).expect("first fits");

        let (tx, rx) = mpsc::channel();
        let pusher = Arc::clone(&outbox);
        let handle = std::thread::spawn(move || {
            tx.send(pusher.push_blocking(ChildMessage::Ready)).expect("report");
        });

        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "returned too early");
        outbox.close();

        let outcome = rx.recv_timeout(DEADLINE).expect("still parked after close");
        assert!(matches!(outcome, Err(ChannelError::Closed)));
        handle.join().expect("pusher panicked");
    }

    /// fails if `pop` parks forever on a closed empty outbox, which would
    /// leave the writer thread unjoinable at shutdown.
    #[test]
    fn pop_returns_none_once_closed_and_empty() {
        let outbox = Outbox::new(4);
        outbox.close();
        assert_eq!(outbox.pop(), None);
    }

    /// fails if a lossy push on a closed outbox panics or queues. An app
    /// emitting metrics past shutdown is ordinary, not an error.
    #[test]
    fn a_lossy_push_after_close_is_ignored() {
        let outbox = Outbox::new(4);
        outbox.close();
        outbox.push_lossy(metric(1.0));
        assert_eq!(outbox.pop(), None);
        assert_eq!(outbox.dropped(), 0);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p shep-channel --lib --all-features`
Expected: FAIL to compile, `mod outbox` is not declared.

- [ ] **Step 3: Declare the module**

Add this to `crates/shep-channel/src/lib.rs`:

```rust
#[cfg(feature = "client")]
mod outbox;
```

The gate is load-bearing. `shep-core` takes this crate with
`default-features = false` to get the wire types alone, so a module reaching
for `serde_json` outside the `client` feature stops shep-core compiling.
Confirm with `cargo check -p shep-channel --no-default-features`, which must
exit 0.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p shep-channel --lib --all-features`
Expected: PASS, five outbox tests.

- [ ] **Step 5: Prove the drop policy test is not vacuous**

In `push_lossy`, change `inner.queue.pop_front()` to `inner.queue.pop_back()`. Run the suite: `a_full_outbox_drops_the_oldest_metric_and_counts_it` must fail on the popped values rather than on the count, which is what proves it asserts on which message survived and not merely on how many did. Restore.

Then remove the `self.drained.notify_all()` from `close`. `closing_releases_a_blocked_push_with_an_error` must fail by timing out at `DEADLINE` rather than hanging the suite. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-channel/src/outbox.rs crates/shep-channel/src/lib.rs
git commit -m "feat(channel): bound the outbound queue, drop metrics before readiness"
```

---

## Task 5: The reply rule, as pure logic

D8 is the whole reason this crate exists: an action name nobody registered, and a handler that panics, must both produce a reply rather than silence. Silence is indistinguishable from a slow handler and costs the operator the full `action_timeout`. No threads in this task, so every case is a plain function call.

**Files:**
- Create: `crates/shep-channel/src/dispatch.rs`
- Modify: `crates/shep-channel/src/lib.rs`

**Interfaces:**
- Produces, crate-internal: `dispatch::Dispatch` with `register_action(&mut self, String, ActionHandler)`, `register_shutdown(&mut self, ShutdownHandler)`, `handle(&self, ShepherdMessage) -> Outcome`; `dispatch::Outcome::{Reply(ChildMessage), Handled, UnhandledShutdown}`.
- Produces, public: `shep_channel::{ActionHandler, ShutdownHandler}` type aliases, so a caller can name a boxed handler.

- [ ] **Step 1: Write the failing tests**

`crates/shep-channel/src/dispatch.rs`:

```rust
//! Turning one shepherd message into the reply that has to go back.
//!
//! The contract asks an app to reply even to an action name it does not
//! recognise, because from the shepherd's side a slow handler and an app
//! that has no idea what it was asked are both silence, and only
//! `action_timeout` running out tells them apart. An app author can forget
//! that. This module cannot.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{ChildMessage, ShepherdMessage};

/// What an action handler is: params, then the action's own name, returning
/// the reply body the operator reads.
pub type ActionHandler = Box<dyn Fn(Option<&str>, &str) -> String + Send + Sync + 'static>;

/// What a shutdown handler is.
pub type ShutdownHandler = Box<dyn Fn() + Send + Sync + 'static>;

/// What handling one message produced.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Send this back.
    Reply(ChildMessage),
    /// A shutdown, and a handler ran.
    Handled,
    /// A shutdown, and no handler was registered.
    UnhandledShutdown,
}

/// The registered handlers.
#[derive(Default)]
pub(crate) struct Dispatch {
    actions: HashMap<String, ActionHandler>,
    shutdown: Option<ShutdownHandler>,
}

// Hand-written because a boxed closure is not `Debug` and the workspace
// denies `missing_debug_implementations`. Names what is registered, which is
// the only part worth seeing, and holds no user data (IR-41).
impl core::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut names: Vec<&str> = self.actions.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("Dispatch")
            .field("actions", &names)
            .field("shutdown", &self.shutdown.is_some())
            .finish()
    }
}

impl Dispatch {
    pub(crate) fn register_action(&mut self, name: String, handler: ActionHandler) {
        self.actions.insert(name, handler);
    }

    pub(crate) fn register_shutdown(&mut self, handler: ShutdownHandler) {
        self.shutdown = Some(handler);
    }

    pub(crate) fn handle(&self, message: ShepherdMessage) -> Outcome {
        match message {
            ShepherdMessage::Shutdown => match &self.shutdown {
                Some(handler) => {
                    handler();
                    Outcome::Handled
                }
                None => Outcome::UnhandledShutdown,
            },
            ShepherdMessage::Action { name, params, id } => {
                let body = match self.actions.get(&name) {
                    Some(handler) => {
                        match catch_unwind(AssertUnwindSafe(|| handler(params.as_deref(), &name))) {
                            Ok(body) => body,
                            Err(payload) => {
                                format!("action handler failed: {}", panic_text(&*payload))
                            }
                        }
                    }
                    None => format!("unknown action: {name}"),
                };
                Outcome::Reply(ChildMessage::ActionReply { action: name, body, id: Some(id) })
            }
        }
    }
}

fn panic_text(payload: &(dyn core::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "panicked with a non-string payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn action(name: &str, params: Option<&str>, id: u64) -> ShepherdMessage {
        ShepherdMessage::Action {
            name: name.to_string(),
            params: params.map(str::to_string),
            id,
        }
    }

    fn reply_of(outcome: Outcome) -> (String, String, Option<u64>) {
        match outcome {
            Outcome::Reply(ChildMessage::ActionReply { action, body, id }) => (action, body, id),
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn a_registered_action_gets_its_handler_and_echoes_the_id() {
        let mut dispatch = Dispatch::default();
        dispatch.register_action(
            "gc".to_string(),
            Box::new(|params, name| format!("{name} ran with {params:?}")),
        );

        let (action, body, id) = reply_of(dispatch.handle(action("gc", Some("now"), 7)));
        assert_eq!(action, "gc");
        assert_eq!(body, "gc ran with Some(\"now\")");
        assert_eq!(id, Some(7), "the id must be echoed or the reply races the timeout");
    }

    /// fails if an unregistered name produces silence. That silence is the
    /// exact failure the contract calls out: the operator waits out
    /// `action_timeout` for a typo.
    #[test]
    fn an_unregistered_action_still_gets_a_reply() {
        let dispatch = Dispatch::default();
        let (action, body, id) = reply_of(dispatch.handle(action("reload-config", None, 3)));
        assert_eq!(action, "reload-config");
        assert_eq!(body, "unknown action: reload-config");
        assert_eq!(id, Some(3));
    }

    /// fails if a panicking handler takes the reply down with it. An app
    /// that panics in one action should not cost the operator a timeout on
    /// top of the bug.
    #[test]
    fn a_panicking_handler_replies_with_the_panic_message() {
        let mut dispatch = Dispatch::default();
        dispatch.register_action("boom".to_string(), Box::new(|_, _| panic!("no such state")));

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = dispatch.handle(action("boom", None, 11));
        std::panic::set_hook(previous);

        let (action, body, id) = reply_of(outcome);
        assert_eq!(action, "boom");
        assert_eq!(body, "action handler failed: no such state");
        assert_eq!(id, Some(11));
    }

    #[test]
    fn a_shutdown_runs_its_handler_exactly_once() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let mut dispatch = Dispatch::default();
        dispatch.register_shutdown(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

        assert!(matches!(dispatch.handle(ShepherdMessage::Shutdown), Outcome::Handled));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// fails if an unhandled shutdown is silently swallowed. D5 says the
    /// library never stops the app itself, so the only thing standing
    /// between the author and a `kill_timeout` is that this case is
    /// distinguishable and gets a warning.
    #[test]
    fn a_shutdown_with_no_handler_is_reported_rather_than_ignored() {
        let dispatch = Dispatch::default();
        assert!(matches!(
            dispatch.handle(ShepherdMessage::Shutdown),
            Outcome::UnhandledShutdown
        ));
    }

    /// fails if `Debug` starts printing handler internals or stops naming
    /// what is registered (IR-41: the Debug is a decision, not a derive).
    #[test]
    fn debug_names_the_registered_actions_and_nothing_else() {
        let mut dispatch = Dispatch::default();
        dispatch.register_action("gc".to_string(), Box::new(|_, _| String::new()));
        dispatch.register_action("dump".to_string(), Box::new(|_, _| String::new()));
        assert_eq!(
            format!("{dispatch:?}"),
            "Dispatch { actions: [\"dump\", \"gc\"], shutdown: false }"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p shep-channel --lib --all-features`
Expected: FAIL to compile, `mod dispatch` is not declared.

- [ ] **Step 3: Declare the module and re-export the handler aliases**

In `crates/shep-channel/src/lib.rs`:

```rust
#[cfg(feature = "client")]
mod dispatch;

#[cfg(feature = "client")]
pub use dispatch::{ActionHandler, ShutdownHandler};
```

The gate is load-bearing: `shep-core` takes this crate with
`default-features = false` for the wire types alone, and anything outside the
`client` feature that reaches for `serde_json` stops shep-core compiling.
Confirm with `cargo check -p shep-channel --no-default-features`, which must
exit 0.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p shep-channel --lib --all-features`
Expected: PASS, six dispatch tests.

- [ ] **Step 5: Prove the reply rule is not vacuous**

Replace the `None` arm of the action lookup with `return Outcome::Handled`, which is what an app that silently ignores unknown names does. `an_unregistered_action_still_gets_a_reply` must fail. Restore.

Then remove the `catch_unwind` and call the handler directly. `a_panicking_handler_replies_with_the_panic_message` must fail by panicking rather than by asserting. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-channel/src/dispatch.rs crates/shep-channel/src/lib.rs
git commit -m "feat(channel): always reply, including to a name nobody registered"
```

---

## Task 6: `serve()`, the threads, and doing nothing well

D3, D6 and D7. The handle always exists, an app with no channel branches nowhere, and the warning fires only where it means something.

**Files:**
- Modify: `crates/shep-channel/src/lib.rs`
- Modify: `crates/shep-channel/src/endpoint.rs`

**Interfaces:**
- Consumes: `Channel` (Task 3), `Outbox` (Task 4), `Dispatch` (Task 5).
- Produces: `shep_channel::serve() -> Shepherd`. `Shepherd` is `Clone + Debug` and has `ready(&self) -> Result<(), ChannelError>`, `metric(&self, impl AsRef<str>, f64)`, `on_action<H: Fn(Option<&str>, &str) -> String + Send + Sync + 'static>(&self, impl AsRef<str>, H) -> &Self`, `on_shutdown<H: Fn() + Send + Sync + 'static>(&self, H) -> &Self`, `is_active(&self) -> bool`, `dropped_metrics(&self) -> u64`, `version(&self) -> Option<&str>`.

`on_action` and `on_shutdown` are generic over the closure and box it internally, so a caller writes `ch.on_action("gc", |params, _| ...)` with no `Box::new`. `ActionHandler` and `ShutdownHandler` stay public for anyone who wants to name a stored handler, and `Dispatch` keeps taking them boxed, because a `HashMap` needs one concrete type.

`on_action` takes `impl AsRef<str>` rather than `impl Into<String>` deliberately. D11 defers typed action names, and `AsRef<str>` is what a caller's own enum would implement, so taking it now costs nothing and means the deferred decision needs no signature change.

- [ ] **Step 1: Split the channel into halves**

Add to `Channel` in `lib.rs`:

```rust
    /// Takes the channel apart for the two threads that drive it.
    pub(crate) fn into_halves(
        self,
    ) -> (std::io::BufReader<endpoint::Transport>, endpoint::Transport, Option<String>) {
        (self.reader, self.writer, self.version)
    }
```

- [ ] **Step 2: Write the failing tests**

Append to `crates/shep-channel/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a handle with no channel refuses work. An app must be able
    /// to call every method without asking whether it has a channel, which
    /// is the whole of D3.
    #[test]
    fn an_inert_handle_accepts_everything_and_does_nothing() {
        let shepherd = Shepherd::inert(None);
        assert!(!shepherd.is_active());
        shepherd.on_action("gc", |_, _| "ok".to_string());
        shepherd.on_shutdown(|| {});
        shepherd.metric("rps", 42.0);
        shepherd.ready().expect("an inert ready is not an error");
        assert_eq!(shepherd.dropped_metrics(), 0);
        assert_eq!(shepherd.version(), None);
    }

    /// fails if the no-channel advice stops naming all three fields that
    /// would open one. An author reading this line is deciding which to set.
    #[test]
    fn the_no_channel_advice_names_every_field_that_opens_one() {
        for field in ["channel", "wait_ready", "shutdown_with_message"] {
            assert!(NO_CHANNEL_ADVICE.contains(field), "advice does not mention {field}");
        }
    }

    /// fails if the shutdown warning stops naming the method an author has
    /// to call. D5 makes this warning the only thing between a missing
    /// handler and a `kill_timeout`.
    #[test]
    fn the_unhandled_shutdown_warning_names_the_method_to_call() {
        assert!(UNHANDLED_SHUTDOWN_ADVICE.contains("on_shutdown"));
        assert!(UNHANDLED_SHUTDOWN_ADVICE.contains("kill_timeout"));
    }
}
```

- [ ] **Step 3: Implement the handle**

In `crates/shep-channel/src/lib.rs`:

```rust
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::dispatch::{Dispatch, Outcome};
use crate::outbox::{DEFAULT_CAPACITY, Outbox};

/// What to tell an author running under shep with no channel.
const NO_CHANNEL_ADVICE: &str = "no channel on this process. Set `channel = true` \
     (or `wait_ready` / `shutdown_with_message`) on this app in the Flockfile to open one.";

/// What to tell an author whose app was asked to stop and registered nothing.
const UNHANDLED_SHUTDOWN_ADVICE: &str =
    "the shepherd sent shutdown and no on_shutdown handler is registered. This \
     process will be killed when kill_timeout expires. Register one to stop gracefully.";

/// Writes one line of advice to stderr, prefixed so it is attributable.
///
/// stderr rather than a log crate: this crate has no logging dependency and
/// an app's stderr is already where shep collects its bleats, so the author
/// reads this where they are already looking.
fn warn(message: &str) {
    eprintln!("shep-channel: {message}");
}

/// A handle on this process's shepherd channel.
///
/// Cheap to clone and safe to share: every method takes `&self`, so a
/// long-lived clone can sit in application state and emit from any thread.
/// With no channel, every method is a no-op, so nothing above this needs to
/// know whether the operator opted in.
#[derive(Clone, Debug)]
pub struct Shepherd(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    /// `None` when this process has no channel.
    outbox: Option<Arc<Outbox>>,
    dispatch: Arc<RwLock<Dispatch>>,
    version: Option<String>,
}

impl Shepherd {
    fn inert(version: Option<String>) -> Self {
        Self(Arc::new(Inner {
            outbox: None,
            dispatch: Arc::new(RwLock::new(Dispatch::default())),
            version,
        }))
    }

    /// Whether this process actually has a channel.
    ///
    /// Branching on this is optional: every method already does nothing
    /// without one.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.0.outbox.is_some()
    }

    /// The `SHEP_CHANNEL_VERSION` stamp, when the shepherd set one.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.0.version.as_deref()
    }

    /// How many metrics have been dropped because the shepherd was not
    /// keeping up. Always 0 without a channel.
    #[must_use]
    pub fn dropped_metrics(&self) -> u64 {
        self.0.outbox.as_ref().map_or(0, |outbox| outbox.dropped())
    }

    /// Registers a handler for one action name, replacing any handler
    /// already registered under it.
    ///
    /// Registering after [`serve`] has started is fine and takes effect on
    /// the next message.
    pub fn on_action<H>(&self, name: impl AsRef<str>, handler: H) -> &Self
    where
        H: Fn(Option<&str>, &str) -> String + Send + Sync + 'static,
    {
        self.0
            .dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_action(name.as_ref().to_string(), Box::new(handler));
        self
    }

    /// Registers the handler run when the shepherd asks this app to stop.
    ///
    /// Without one, a shutdown message warns and nothing else happens: this
    /// crate never ends a process on its own judgement.
    pub fn on_shutdown<H>(&self, handler: H) -> &Self
    where
        H: Fn() + Send + Sync + 'static,
    {
        self.0
            .dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_shutdown(Box::new(handler));
        self
    }

    /// Says this app is up. Blocks only until the message is queued.
    ///
    /// # Errors
    ///
    /// [`ChannelError::Closed`] when the shepherd has gone away. Without a
    /// channel this is `Ok(())`, because an app that was never given one
    /// has nothing to report and no failure to handle.
    pub fn ready(&self) -> Result<(), ChannelError> {
        match &self.0.outbox {
            Some(outbox) => outbox.push_blocking(ChildMessage::Ready),
            None => Ok(()),
        }
    }

    /// Records one metric sample. Never blocks and never fails.
    ///
    /// A sample may be dropped if the shepherd stops reading; see
    /// [`Shepherd::dropped_metrics`]. That trade is deliberate, so that no
    /// call on an app's hot path can park on a full socket.
    pub fn metric(&self, name: impl AsRef<str>, value: f64) {
        if let Some(outbox) = &self.0.outbox {
            outbox.push_lossy(ChildMessage::Metric {
                name: name.as_ref().to_string(),
                value,
            });
        }
    }
}

/// Opens this process's channel and starts serving it.
///
/// Always returns a usable handle. With no channel every call on it is a
/// no-op, so an app needs no branch at its emit sites; one line goes to
/// stderr in that case, and only when `SHEP_NAME` says this process is
/// running under shep at all.
///
/// A process singleton: the channel is one descriptor and can be owned once.
/// A second call returns the same handle.
#[must_use]
pub fn serve() -> Shepherd {
    static SHEPHERD: OnceLock<Shepherd> = OnceLock::new();
    static CALLS: AtomicU32 = AtomicU32::new(0);

    let shepherd = SHEPHERD.get_or_init(start);
    if CALLS.fetch_add(1, Ordering::Relaxed) == 1 {
        warn("serve() called more than once; returning the first handle. \
              The channel is one descriptor and cannot be opened twice.");
    }
    shepherd.clone()
}

fn start() -> Shepherd {
    let channel = match Channel::open() {
        Ok(Some(channel)) => channel,
        Ok(None) => {
            if std::env::var_os("SHEP_NAME").is_some() {
                warn(NO_CHANNEL_ADVICE);
            }
            return Shepherd::inert(None);
        }
        Err(error) => {
            warn(&format!("{error}; continuing without a channel"));
            return Shepherd::inert(None);
        }
    };

    let (reader, mut writer, version) = channel.into_halves();
    if let Some(stamp) = &version {
        if stamp != CHANNEL_VERSION {
            warn(&format!(
                "the shepherd stamps {VERSION_VAR}={stamp} and this crate implements \
                 {CHANNEL_VERSION}; continuing, since a newer wire has so far only added \
                 fields an older reader ignores"
            ));
        }
    }

    let outbox = Arc::new(Outbox::new(DEFAULT_CAPACITY));
    let dispatch = Arc::new(RwLock::new(Dispatch::default()));

    let writing = Arc::clone(&outbox);
    std::thread::Builder::new()
        .name("shep-channel-writer".to_string())
        .spawn(move || {
            while let Some(message) = writing.pop() {
                if session::write_message(&mut writer, &message).is_err() {
                    break;
                }
            }
            writing.close();
        })
        .ok();

    let reading = Arc::clone(&outbox);
    let handlers = Arc::clone(&dispatch);
    std::thread::Builder::new()
        .name("shep-channel-reader".to_string())
        .spawn(move || {
            let mut reader = reader;
            let mut warned_malformed = false;
            loop {
                match session::read_message(&mut reader) {
                    Ok(Some(message)) => {
                        let outcome = handlers
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .handle(message);
                        match outcome {
                            Outcome::Reply(reply) => {
                                if reading.push_blocking(reply).is_err() {
                                    break;
                                }
                            }
                            Outcome::Handled => {}
                            Outcome::UnhandledShutdown => warn(UNHANDLED_SHUTDOWN_ADVICE),
                        }
                    }
                    Err(ChannelError::Malformed(message)) => {
                        if !warned_malformed {
                            warned_malformed = true;
                            warn(&format!("malformed frame from the shepherd: {message}"));
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            reading.close();
        })
        .ok();

    Shepherd(Arc::new(Inner { outbox: Some(outbox), dispatch, version }))
}
```

Note the reader thread runs handlers itself, so a slow handler delays the next message. That is deliberate and matches what the contract asks of an app ("reply exactly once, promptly"); `action_timeout` defaults to 3s, so a handler slower than that has already lost. Say so in the crate docs in Task 8.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p shep-channel --lib --all-features`
Expected: PASS.

- [ ] **Step 5: Prove the inert path is not vacuous**

Change `ready()`'s `None` arm from `Ok(())` to `Err(ChannelError::Closed)`. `an_inert_handle_accepts_everything_and_does_nothing` must fail. Restore.

- [ ] **Step 6: Run the task gate**

Four commands, one at a time, each with `$?` read directly:

```bash
cargo fmt --all --check
```
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
```bash
cargo test --workspace --all-features
```
```bash
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

- [ ] **Step 7: Commit**

```bash
git add crates/shep-channel/src
git commit -m "feat(channel): serve the channel, and do nothing well without one"
```

---

## Task 7: One real child on a real descriptor

Everything so far is covered without a process. This task proves the piece none of it reaches: that `serve()` finds fd 3 in a process the shepherd actually spawned.

**Files:**
- Create: `crates/shep-channel/examples/answers.rs`
- Create: `crates/shep-channel/tests/real_child.rs`
- Modify: `crates/shep-channel/Cargo.toml`

**Interfaces:**
- Consumes: `shep_channel::serve` from Task 6.

- [ ] **Step 1: Take the dependency the daemon already uses**

`command-fds` 0.3.3 is already in `[workspace.dependencies]` (checked 2026-09-02, `Cargo.toml:103`), and `tokio_runner.rs` maps the child's fd 3 with it. Use the same crate rather than a second mechanism.

In `crates/shep-channel/Cargo.toml`:

```toml
[target.'cfg(unix)'.dev-dependencies]
command-fds.workspace = true
```

Unix-only, because the test that needs it is unix-only and `command-fds` does unconditional unix fd work.

- [ ] **Step 2: Write the example app**

`crates/shep-channel/examples/answers.rs`:

```rust
//! A supervised app that answers on the shepherd channel.
//!
//! Run under shep with `channel = true` and `shep trigger <name> gc` reaches
//! the handler below. `tests/real_child.rs` drives this same binary through
//! a socketpair on fd 3, which is what proves the descriptor is found.

fn main() {
    let shepherd = shep_channel::serve();
    shepherd.on_action("gc", |params, _name| format!("collected, params={params:?}"));
    shepherd.on_shutdown(|| std::process::exit(0));
    shepherd.ready().expect("say ready");
    shepherd.metric("rps", 42.0);

    // Park. The reader thread is doing the work; the test kills this process.
    loop {
        std::thread::park();
    }
}
```

- [ ] **Step 3: Write the failing test**

`crates/shep-channel/tests/real_child.rs`:

```rust
//! Drives the `answers` example as a real child with a real fd 3.
//!
//! unix only: wiring an inherited descriptor is the unix half of the
//! contract. The Windows half is a named pipe the app opens by name, which
//! needs a live shepherd to create, so it is covered by the shep daemon's
//! own Windows tests rather than here. Everything above the descriptor is
//! already covered on both platforms by the generic tests in `session.rs`.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// How long the child gets to answer before the test calls it hung. A
/// working child answers in milliseconds; this is slack for a loaded runner.
const DEADLINE: Duration = Duration::from_secs(10);

#[test]
fn a_real_child_finds_fd_3_and_answers() {
    let (ours, theirs) = UnixStream::pair().expect("socketpair");
    // The child inherits a blocking descriptor, which is what the shepherd
    // hands a real app: `tokio_runner.rs` clears O_NONBLOCK deliberately so
    // a plain read parks rather than returning EAGAIN.
    theirs.set_nonblocking(false).expect("blocking");
    ours.set_read_timeout(Some(DEADLINE)).expect("deadline");

    let mut child = command_fds_spawn(env!("CARGO_BIN_EXE_answers"), theirs);

    let mut writer = ours.try_clone().expect("clone");
    let mut reader = BufReader::new(ours);

    let mut ready = String::new();
    reader.read_line(&mut ready).expect("child did not say ready within the deadline");
    assert_eq!(ready.trim_end(), "{\"kind\":\"ready\"}");

    let mut metric = String::new();
    reader.read_line(&mut metric).expect("no metric");
    assert_eq!(metric.trim_end(), "{\"kind\":\"metric\",\"name\":\"rps\",\"value\":42.0}");

    writer
        .write_all(b"{\"kind\":\"action\",\"name\":\"gc\",\"params\":\"now\",\"id\":7}\n")
        .expect("write");
    let mut reply = String::new();
    reader.read_line(&mut reply).expect("no reply");
    assert_eq!(
        reply.trim_end(),
        "{\"kind\":\"action-reply\",\"action\":\"gc\",\"body\":\"collected, params=Some(\\\"now\\\")\",\"id\":7}"
    );

    // The rule this crate exists for, against a real process.
    writer
        .write_all(b"{\"kind\":\"action\",\"name\":\"typo\",\"id\":8}\n")
        .expect("write");
    let mut unknown = String::new();
    reader.read_line(&mut unknown).expect("no reply to an unknown action");
    assert_eq!(
        unknown.trim_end(),
        "{\"kind\":\"action-reply\",\"action\":\"typo\",\"body\":\"unknown action: typo\",\"id\":8}"
    );

    child.kill().expect("kill");
    child.wait().expect("reap");
}
```

And the helper, in the same file. `FdMapping` takes an `OwnedFd` in 0.3.3, which a `UnixStream` converts into, so this needs no `unsafe` at all:

```rust
fn command_fds_spawn(exe: &str, theirs: UnixStream) -> std::process::Child {
    use std::os::fd::OwnedFd;

    use command_fds::{CommandFdExt as _, FdMapping};

    let mut command = std::process::Command::new(exe);
    command
        .env("SHEP_CHANNEL_FD", "3")
        .env("SHEP_CHANNEL_VERSION", "1")
        // Set so the child takes the D3 warning path only if it has no
        // channel, which it does. Without this the test would prove nothing
        // about a process running under shep.
        .env("SHEP_NAME", "answers");
    command
        .fd_mappings(vec![FdMapping { parent_fd: OwnedFd::from(theirs), child_fd: 3 }])
        .expect("map the socketpair to fd 3");
    command.spawn().expect("spawn the answers example")
}
```

Every read in this test is bounded by `set_read_timeout(DEADLINE)`, so a child that never answers fails the test rather than hanging the suite. There is no sleep anywhere in it.

- [ ] **Step 4: Run to verify it fails, then passes**

Run: `cargo test -p shep-channel --test real_child`
Expected: FAIL first, because the example does not exist until Step 2 is saved and the helper until Step 3. Then PASS.

- [ ] **Step 5: Prove it is not vacuous**

In `endpoint::discover`, return `Ok(Endpoint::Absent)` unconditionally. The test must fail on the first `read_line`, timing out at `DEADLINE` rather than hanging. Restore.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-channel/examples crates/shep-channel/tests/real_child.rs crates/shep-channel/Cargo.toml
git commit -m "test(channel): drive a real child on a real fd 3"
```

---

## Task 8: Publish it, and say it exists

**Files:**
- Create: `crates/shep-channel/README.md`
- Modify: `crates/shep-channel/src/lib.rs`
- Modify: `release-plz.toml`
- Modify: `docs/shepherd-channel.md`
- Possibly modify: `web/src/pages/docs/*.astro`

- [ ] **Step 1: Add the crate to the release**

In `release-plz.toml`, beside the other four entries:

```toml
[[package]]
name = "shep-channel"
version_group = "shep"
```

Every crate in this workspace shares one version through `[workspace.package]`, and the version group is what stops release-plz assigning this one a different number from the rest.

- [ ] **Step 2: Write the README**

`crates/shep-channel/README.md`. Match whatever badge and heading shape `crates/shep-client/README.md` already uses, and carry this body:

````markdown
# shep-channel

Speak the shep shepherd channel from a supervised app: signal readiness,
emit a metric, answer an action.

```rust
let shepherd = shep_channel::serve();

shepherd.on_action("gc", |params, _name| {
    format!("collected, params={params:?}")
});
shepherd.on_shutdown(|| server.graceful_stop());
shepherd.ready()?;
shepherd.metric("rps", 4200.0);
```

Ask for a channel in the Flockfile with `channel = true`, or get one from
`wait_ready` or `shutdown_with_message`.

Without one, every call above does nothing and the app runs unchanged, so
there is nothing to branch on. An action name you did not register still
gets a reply, which is the part an app is most likely to get wrong: to the
shepherd, silence from an app thinking hard and silence from an app that
never understood the question look the same, and only `action_timeout`
running out tells them apart.

The wire itself is documented in `docs/shepherd-channel.md` in the shep
repository. It is language agnostic, and there are Go, JavaScript and
Python libraries over the same contract.
````

The `?` in that block means the README is not a doctest, which is correct: it is a snippet, and `lib.rs` carries the one that compiles.

- [ ] **Step 3: Finish the crate docs**

The crate-level doc comment in `lib.rs` needs a worked example that compiles as a doctest, and three facts that are surprising:

- The reader thread runs handlers, so a slow handler delays the next message. `action_timeout` defaults to 3s.
- `metric` can drop; `ready` and replies cannot.
- A shutdown with no handler warns and does nothing; this crate never stops a process itself.

The doctest must not open a real channel. Use `serve()` and assert `is_active()` is false, which is true in a test process and is also the honest demonstration of D3.

- [ ] **Step 4: Point the contract at the crate**

In `docs/shepherd-channel.md`, add a short section after "Getting a channel" saying a Rust app can take `shep-channel` instead of framing this by hand, and that the Go, JavaScript and Python libraries follow. Do not restate the API; link it. The document's whole value is being the language-agnostic contract, so it must not turn into Rust documentation.

Then grep `web/` for the channel:

```bash
grep -rln "shepherd channel\|SHEP_CHANNEL" web/src/pages/
```

If a prose page covers the channel, add the same pointer there. If none does, say so and change nothing: this task adds no verb, flag or config key, so the generated CLI reference is untouched and `web/` needs no regeneration.

- [ ] **Step 5: Build the docs**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`
Expected: EXIT=0, and the doctest in Step 3 passes under `cargo test --doc -p shep-channel`.

If you touched `web/`:

```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

Both, because `astro build` does not typecheck and a wrong prop renders wrong while building clean.

- [ ] **Step 6: Run the full phase gate**

The four task-gate commands, then:

```bash
cargo test --workspace --all-features -- --test-threads=1
```

The serial run is not ceremony; it has caught a real regression in this workspace before.

- [ ] **Step 7: Commit**

```bash
git add crates/shep-channel/README.md crates/shep-channel/src/lib.rs release-plz.toml docs/shepherd-channel.md
git commit -m "docs(channel): publish the crate and point the contract at it"
```

---

## Before opening the pull request

- [ ] The two cross-checks from `CLAUDE.md`, once for this phase rather than once per task, each with its own `CARGO_TARGET_DIR` if you want the host cache left alone:

```bash
cargo check -p shep-daemon --all-targets --all-features --target x86_64-unknown-linux-gnu
```
```bash
cargo check --workspace --all-targets --all-features --target x86_64-pc-windows-gnu
```

The Windows one matters more than usual here. `endpoint.rs` has a `cfg(windows)` arm that a macOS `cargo test` never compiles, and this is the only local command that reads it. It needs `brew install mingw-w64`, because `ring`'s build script runs `cc` for the target.

- [ ] Read the CI result before calling the branch green. The local gate does not run Linux or Windows tests, and `.github/workflows/test.yml` is what does.

- [ ] `shep-channel` is a new crates.io name. The first publish is irreversible, so before the release PR merges, confirm the manifest is what should be permanent: `description`, `keywords`, `categories`, `documentation`, and `readme` all render on the crates.io page and none of them can be edited without a new version.
