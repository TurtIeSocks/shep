//! `shep whistle`: an MCP server over stdio, handing a model the flock.
//!
//! Scaffolding only so far — Task 3 of Phase 13 lands [`gate`], the
//! `[whistle] allow_control` gate that decides whether the four control
//! tools exist. The verb itself (the `Whistle` handler, `get_info`, the
//! stdio serve loop) is Task 8; nothing here is reachable from `shep
//! whistle` yet.

pub(crate) mod gate;
