//! Shared foundation of the shep workspace: app/daemon configuration,
//! filesystem paths, process selectors, typed errors, and the client↔daemon
//! wire protocol (requests, responses, bus events, framing).
//!
//! Every other crate in the workspace depends on this one; it depends on no
//! sibling. Module-by-module design: `docs/systematic-refactor/refactor-workspace/map.md`.

pub mod values;
