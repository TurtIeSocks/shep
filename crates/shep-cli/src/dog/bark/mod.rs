//! `shep dog bark`: the webhook-alert dog.
//!
//! [`sinks`] is Task 19 — the Discord, Slack and plain-JSON webhook
//! destinations one fired [`shep_core::barks::Bark`] can be delivered to,
//! plus the pure body renderer and the async delivery function every later
//! task in this module calls. Two pieces still to land in this directory:
//! reconciling the shepherd's own bus against `barks.jsonl` (Task 20 — the
//! bus drops events for a lagging subscriber, so bark polls rather than
//! trusting the stream to be complete), and this dog's own `run`
//! entrypoint (Task 21 — the target [`super::run_dog`]'s `"bark"` arm
//! reaches once it stops being a stub).

pub mod sinks;
