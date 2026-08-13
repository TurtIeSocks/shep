//! `shep dog bark`: the webhook-alert dog.
//!
//! [`sinks`] is Task 19 — the Discord, Slack and plain-JSON webhook
//! destinations one fired [`shep_core::barks::Bark`] can be delivered to,
//! plus the pure body renderer and the async delivery function every later
//! task in this module calls. [`rules`] is Task 20 — [`rules::Rules`]
//! decides which bus events and which reconciliation-poll snapshots become
//! a [`rules::Firing`], and which are filtered out. One piece still to
//! land in this directory: this dog's own `run` entrypoint (Task 21 — the
//! target [`super::run_dog`]'s `"bark"` arm reaches once it stops being a
//! stub), which is what wires `rules` and `sinks` together into a running
//! dog.

pub mod rules;
pub mod sinks;
