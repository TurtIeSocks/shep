//! The client<->daemon wire protocol (version 1)
//!
//! Typed request/response enums + bus events. Framing lives in [`wire`];
//! every type here is snapshot-pinned — changing any serialized shape is a
//! protocol version bump recorded in the CHANGELOG.

pub mod events;
pub mod request;
/// Frame encoding shared by daemon and client
pub mod wire;

pub use events::{BusEvent, ProcessEventKind};
pub use request::{
    Envelope, Hello, HelloAck, HelloReply, ProcessInfo, Reply, Request, Response, RpcError,
    RpcErrorCode, SelectorSpec,
};
pub use wire::{WireError, codec, decode_frame, encode_frame};

/// Wire protocol version; bump on any breaking change to serialized shapes
pub const PROTOCOL_VERSION: u32 = 1;
