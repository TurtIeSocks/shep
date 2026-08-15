//! `shep whistle`: an MCP server over stdio, handing a model the flock.
//!
//! Scaffolding so far — Task 3 of Phase 13 lands [`gate`], the
//! `[whistle] allow_control` gate that decides whether the four control
//! tools exist. Task 4 lands [`shepherd`], the one-connection-per-call
//! transport every tool reaches the daemon through. Task 5 lands [`facts`],
//! the schema-carrying payload twins every tool returns. Task 6 lands
//! [`read`], the five read-only tools, and with them the minimal [`Whistle`]
//! below — just enough for `#[tool_router]` to have a `Self` to attach to.
//! The verb itself (`get_info`, the stdio serve loop, wiring `Whistle::new`
//! into `shep whistle`'s dispatch) is Task 8; nothing here is reachable from
//! `main` yet.

pub(crate) mod facts;
pub(crate) mod gate;
pub(crate) mod read;
pub(crate) mod shepherd;

use std::path::PathBuf;

use shepherd::Shepherd;

/// The MCP handler every tool in this module is a method on.
///
/// Minimal by design: `read.rs`'s five tools need a [`Shepherd`] to reach
/// the daemon through and a path to `barks.jsonl` for the one tool that
/// never does. Task 7's four control tools reach the daemon through the
/// same `shepherd` field — nothing new. Task 8 is what actually constructs
/// one (from `ShepPaths`, at `shep whistle`'s own startup), adds
/// `get_info`, and decides whether `control::control_router` is added on
/// top of `read::read_only_router`.
///
/// Not constructed outside this module's own tests yet, same reason as
/// [`shepherd::Shepherd`]: nothing calls `Whistle::new` from `main` until
/// Task 8. `#[allow(dead_code)]` on the inherent impl below says so
/// explicitly, same pattern as `shepherd::Shepherd` and `gate::Control`.
#[derive(Debug, Clone)]
pub(crate) struct Whistle {
    shepherd: Shepherd,
    /// `$SHEP_HOME/barks.jsonl` — `read::list_barks` reads this directly,
    /// no socket, which is the entire point of that tool (spec: it must
    /// work after the shepherd has crashed).
    barks_path: PathBuf,
}

#[allow(dead_code)]
impl Whistle {
    /// Wraps a [`Shepherd`] and a `barks.jsonl` path. Nothing else exists
    /// to configure yet.
    #[must_use]
    pub(crate) fn new(shepherd: Shepherd, barks_path: PathBuf) -> Self {
        Self {
            shepherd,
            barks_path,
        }
    }
}
