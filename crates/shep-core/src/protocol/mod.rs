//! The client<->daemon wire protocol (version 3)
//!
//! Typed request/response enums + bus events. Framing lives in [`wire`];
//! every type here is snapshot-pinned — changing any serialized shape is a
//! protocol version bump recorded in the CHANGELOG.
//!
//! **What version 2 added:** the instance slot on [`ProcessInfo`]. A sheep
//! that is one of several instances of an app reports which slot it is, so
//! every listing can group an app's instances and roll their numbers up
//! rather than showing several rows that share a name and explain nothing.
//! A sheep reports its own slot, counting from 0, so a single-instance app
//! reports `Some(0)`. `None` means the peer daemon predates the field, and a
//! reader that finds it should render exactly what it rendered before this
//! field existed.
//!
//! **What version 3 changed:** `ResetDepth::Settings` was renamed to
//! `ResetDepth::Policy` (and gained `File`/`Env` siblings). Unlike every
//! bump before it except `SelectorSpec::Instance`'s, this one is not an
//! addition: `"settings"` was the wire spelling of a `Request::ApplyConfig`
//! already shipping, and a rename removes a string an older daemon already
//! decoded rather than adding one it never saw. Without the bump, a CLI
//! built after this change sends `"policy"` for what used to be `--reset`,
//! and a daemon that has not restarted since the upgrade cannot decode it:
//! the connection ends on an envelope it cannot read instead of a named
//! refusal at the handshake. Restart the shepherd after upgrading to this
//! version, the same as the last bump asked.
//!
//! **Two sets of tests carry a version in their name and they assert
//! opposite things.** The `*_wire_v3` snapshots pin the shape this crate
//! serializes TODAY, so they follow [`PROTOCOL_VERSION`] and get renamed
//! whenever it moves. The `v1_*_fixture_still_deserializes` tests pin a
//! literal payload captured from a version 1 peer and assert it STILL
//! decodes, so their name records where the bytes came from and never
//! moves; renaming one would erase the compatibility claim it exists to
//! make.

pub mod channel;
pub mod events;
pub mod frame;
pub mod request;
/// Frame encoding shared by daemon and client
pub mod wire;

pub use channel::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
pub use events::{BusEvent, ProcessEventKind};
pub use frame::ServerFrame;
pub use request::{
    ActionOutcome, ActionReply, DogSectionToml, DogSource, Envelope, ExitInfo, Hello, HelloAck,
    HelloReply, Lamb, LineOutcome, LineReply, ProcessInfo, ProcessInfoBuilder, Reply, Request,
    Response, RpcError, RpcErrorCode, SelectorSpec, SheepApplied, SheepConfigView, SheepDrift,
    SignalOutcome, SignalReply, Smit, SmitError, sort_flock,
};
pub use wire::{MAX_FRAME_BYTES, WireError, codec, decode_frame, encode_frame};

/// Wire protocol version.
///
/// Evolution rule: ADDITIVE optional fields (new serde-defaulted `Option<T>`
/// fields, new variants behind `#[non_exhaustive]`) keep the version.
/// Removing, renaming, or retyping anything serialized bumps it, recorded in
/// the CHANGELOG. Byte fixtures in each protocol module pin the deserialize
/// direction.
pub const PROTOCOL_VERSION: u32 = 3;
