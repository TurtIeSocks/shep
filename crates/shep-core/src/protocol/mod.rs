//! The client<->daemon wire protocol (version 2)
//!
//! Typed request/response enums + bus events. Framing lives in [`wire`];
//! every type here is snapshot-pinned — changing any serialized shape is a
//! protocol version bump recorded in the CHANGELOG.
//!
//! **What version 2 added:** the instance slot on [`ProcessInfo`]. A sheep
//! that is one of several instances of an app reports which slot it is, so
//! every listing can group an app's instances and roll their numbers up
//! rather than showing several rows that share a name and explain nothing.
//! A single-instance sheep reports `None`, exactly as version 1 had no field
//! to report at all.

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
    Response, RpcError, RpcErrorCode, SelectorSpec, SheepDrift, SignalOutcome, SignalReply, Smit,
    SmitError, sort_flock,
};
pub use wire::{MAX_FRAME_BYTES, WireError, codec, decode_frame, encode_frame};

/// Wire protocol version.
///
/// Evolution rule: ADDITIVE optional fields (new serde-defaulted `Option<T>`
/// fields, new variants behind `#[non_exhaustive]`) keep the version.
/// Removing, renaming, or retyping anything serialized bumps it, recorded in
/// the CHANGELOG. Byte fixtures in each protocol module pin the deserialize
/// direction.
pub const PROTOCOL_VERSION: u32 = 2;
