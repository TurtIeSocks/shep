//! The client<->daemon wire protocol (version 1)
//!
//! Typed request/response enums + bus events. Framing lives in [`wire`];
//! every type here is snapshot-pinned — changing any serialized shape is a
//! protocol version bump recorded in the CHANGELOG.

pub mod events;
pub mod request;
// `pub mod wire;` is added by the framing task — declaring it here would
// break this task's gates (E0583: file not found).

pub use events::{BusEvent, ProcessEventKind};
pub use request::{
    Envelope, Hello, HelloAck, HelloReply, ProcessInfo, Reply, Request, Response, RpcError,
    RpcErrorCode, SelectorSpec,
};

/// Wire protocol version; bump on any breaking change to serialized shapes
pub const PROTOCOL_VERSION: u32 = 1;
