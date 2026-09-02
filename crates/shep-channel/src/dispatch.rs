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
                Outcome::Reply(ChildMessage::ActionReply {
                    action: name,
                    body,
                    id: Some(id),
                })
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
        assert_eq!(
            id,
            Some(7),
            "the id must be echoed or the reply races the timeout"
        );
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

        assert!(matches!(
            dispatch.handle(ShepherdMessage::Shutdown),
            Outcome::Handled
        ));
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
