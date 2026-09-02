use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use crate::dispatch::{Dispatch, Outcome, run};
use crate::outbox::{DEFAULT_CAPACITY, Outbox};
use crate::{CHANNEL_VERSION, Channel, ChannelError, ChildMessage, VERSION_VAR, session};

/// What to tell an author running under shep with no channel.
const NO_CHANNEL_ADVICE: &str = "no channel on this process. Set `channel = true` \
     (or `wait_ready` / `shutdown_with_message`) on this app in the Flockfile to open one.";

/// What to tell an author whose app was asked to stop and registered nothing.
const UNHANDLED_SHUTDOWN_ADVICE: &str = "the shepherd sent shutdown and no on_shutdown handler is registered. This \
     process will be killed when kill_timeout expires. Register one to stop gracefully.";

/// Writes one line of advice to stderr, prefixed so it is attributable.
///
/// stderr rather than a log crate: this crate has no logging dependency and
/// an app's stderr is already where shep collects its bleats, so the author
/// reads this where they are already looking.
fn warn(message: &str) {
    eprintln!("shep-channel: {message}");
}

/// A handle on this process's shepherd channel.
///
/// Cheap to clone and safe to share: every method takes `&self`, so a
/// long-lived clone can sit in application state and emit from any thread.
/// With no channel, every method is a no-op, so nothing above this needs to
/// know whether the operator opted in.
#[derive(Clone, Debug)]
pub struct Shepherd(Arc<Inner>);

// Pins the "safe to share... from any thread" claim above at compile time,
// so a later field addition that quietly breaks it fails the build instead
// of the next reader's assumption.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Shepherd>();
};

#[derive(Debug)]
struct Inner {
    /// `None` when this process has no channel.
    outbox: Option<Arc<Outbox>>,
    dispatch: Arc<RwLock<Dispatch>>,
    version: Option<String>,
}

impl Shepherd {
    fn inert(version: Option<String>) -> Self {
        Self(Arc::new(Inner {
            outbox: None,
            dispatch: Arc::new(RwLock::new(Dispatch::default())),
            version,
        }))
    }

    /// Whether this process actually has a channel.
    ///
    /// Branching on this is optional: every method already does nothing
    /// without one.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.0.outbox.is_some()
    }

    /// The `SHEP_CHANNEL_VERSION` stamp, when the shepherd set one.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.0.version.as_deref()
    }

    /// How many metrics have been dropped because the shepherd was not
    /// keeping up. Always 0 without a channel.
    #[must_use]
    pub fn dropped_metrics(&self) -> u64 {
        self.0.outbox.as_ref().map_or(0, |outbox| outbox.dropped())
    }

    /// Registers a handler for one action name, replacing any handler
    /// already registered under it.
    ///
    /// The handler is called with the action's params (`None` when the
    /// operator triggered it with none) and then the action's own name,
    /// and returns the reply body sent back to the operator.
    ///
    /// Registering from another thread while [`serve`] is running is fine,
    /// and so is registering from inside a handler -- a `reload` action
    /// that swaps its own handlers is exactly what this is for. Either way
    /// it takes effect on the next message: the registry's lock is never
    /// held while a handler runs, so this never contends with dispatch.
    pub fn on_action<H>(&self, name: impl AsRef<str>, handler: H) -> &Self
    where
        H: Fn(Option<&str>, &str) -> String + Send + Sync + 'static,
    {
        self.0
            .dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_action(name.as_ref().to_string(), Box::new(handler));
        self
    }

    /// Registers the handler run when the shepherd asks this app to stop.
    ///
    /// Without one, a shutdown message warns and nothing else happens: this
    /// crate never ends a process on its own judgement.
    pub fn on_shutdown<H>(&self, handler: H) -> &Self
    where
        H: Fn() + Send + Sync + 'static,
    {
        self.0
            .dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_shutdown(Box::new(handler));
        self
    }

    /// Says this app is up. Blocks only until the message is queued.
    ///
    /// # Errors
    ///
    /// [`ChannelError::Closed`] when the shepherd has gone away. Without a
    /// channel this is `Ok(())`, because an app that was never given one
    /// has nothing to report and no failure to handle.
    pub fn ready(&self) -> Result<(), ChannelError> {
        match &self.0.outbox {
            Some(outbox) => outbox.push_blocking(ChildMessage::Ready),
            None => Ok(()),
        }
    }

    /// Records one metric sample. Never blocks and never fails.
    ///
    /// A sample may be dropped if the shepherd stops reading; see
    /// [`Shepherd::dropped_metrics`]. That trade is deliberate, so that no
    /// call on an app's hot path can park on a full socket.
    ///
    /// Takes `impl Into<String>` rather than `on_action`'s `impl AsRef<str>`:
    /// this runs per sample on a documented hot path, and an owned `String`
    /// the caller already has moves in for free instead of being copied
    /// again.
    pub fn metric(&self, name: impl Into<String>, value: f64) {
        if let Some(outbox) = &self.0.outbox {
            outbox.push_lossy(ChildMessage::Metric {
                name: name.into(),
                value,
            });
        }
    }
}

/// Opens this process's channel and starts serving it.
///
/// Always returns a usable handle. With no channel every call on it is a
/// no-op, so an app needs no branch at its emit sites; one line goes to
/// stderr in that case, and only when `SHEP_NAME` says this process is
/// running under shep at all.
///
/// A process singleton: the channel is one descriptor and can be owned once.
/// A second call returns the same handle.
#[must_use]
pub fn serve() -> Shepherd {
    static SHEPHERD: OnceLock<Shepherd> = OnceLock::new();
    static CALLS: AtomicU32 = AtomicU32::new(0);

    let shepherd = SHEPHERD.get_or_init(start);
    if CALLS.fetch_add(1, Ordering::Relaxed) == 1 {
        warn(
            "serve() called more than once; returning the first handle. \
              The channel is one descriptor and cannot be opened twice.",
        );
    }
    shepherd.clone()
}

/// Drives the writer side: drains `outbox` and writes each message, until
/// the transport fails or the outbox closes. Free function over `BufRead`/
/// `Write` rather than an inline thread closure so it can be driven over a
/// `Vec<u8>` in a test with no thread, no socket, and no descriptor -- the
/// same reason `session`'s own functions are generic (see its module doc).
pub(crate) fn writer_loop<W: Write>(writer: &mut W, outbox: &Outbox) {
    while let Some(message) = outbox.pop() {
        if session::write_message(writer, &message).is_err() {
            break;
        }
    }
    outbox.close();
}

/// Drives the reader side: reads one message at a time, resolves it against
/// `dispatch`, and runs the result -- with the registry's lock dropped
/// before that run, per this crate's `dispatch` module doc. `warn` is
/// injected rather than called directly so a test can assert on exactly
/// what was said and how many times, which is the only way to pin the
/// once-per-loop malformed-frame latch.
pub(crate) fn reader_loop<R: BufRead>(
    reader: &mut R,
    outbox: &Outbox,
    dispatch: &RwLock<Dispatch>,
    warn: &dyn Fn(&str),
) {
    let mut warned_malformed = false;
    loop {
        match session::read_message(reader) {
            Ok(Some(message)) => {
                let resolved = dispatch
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .resolve(message);
                let outcome = run(resolved);
                match outcome {
                    Outcome::Reply(reply) => {
                        if outbox.push_blocking(reply).is_err() {
                            break;
                        }
                    }
                    Outcome::Handled => {}
                    Outcome::UnhandledShutdown => warn(UNHANDLED_SHUTDOWN_ADVICE),
                    Outcome::ShutdownFailed(message) => {
                        warn(&format!("shutdown handler panicked: {message}"));
                    }
                }
            }
            Err(ChannelError::Malformed(message)) => {
                if !warned_malformed {
                    warned_malformed = true;
                    warn(&format!("malformed frame from the shepherd: {message}"));
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    outbox.close();
}

fn start() -> Shepherd {
    let channel = match Channel::open() {
        Ok(Some(channel)) => channel,
        Ok(None) => {
            if std::env::var_os("SHEP_NAME").is_some() {
                warn(NO_CHANNEL_ADVICE);
            }
            return Shepherd::inert(None);
        }
        Err(error) => {
            warn(&format!("{error}; continuing without a channel"));
            return Shepherd::inert(None);
        }
    };

    let (reader, mut writer, version) = channel.into_halves();
    if let Some(stamp) = &version
        && stamp != CHANNEL_VERSION
    {
        warn(&format!(
            "the shepherd stamps {VERSION_VAR}={stamp} and this crate implements \
             {CHANNEL_VERSION}; continuing, since a newer wire has so far only added \
             fields an older reader ignores"
        ));
    }

    let outbox = Arc::new(Outbox::new(DEFAULT_CAPACITY));
    let dispatch = Arc::new(RwLock::new(Dispatch::default()));

    let writing = Arc::clone(&outbox);
    let writer_spawn = std::thread::Builder::new()
        .name("shep-channel-writer".to_string())
        .spawn(move || writer_loop(&mut writer, &writing));
    if let Err(error) = writer_spawn {
        // Nothing will ever drain the outbox without this thread, so a
        // handle that reported `is_active()` true here would be worse than
        // no handle: `ready()` would queue into a channel nothing reads and
        // return `Ok(())`, and the operator's `wait_ready` gate would time
        // out with nothing anywhere saying why. Close the outbox first so
        // `ready()`/`metric()` see the failure honestly, then hand back an
        // inert handle -- still carrying the version stamp, since the
        // channel itself did open.
        warn(&format!(
            "failed to spawn the shep-channel writer thread: {error}; continuing without a channel"
        ));
        outbox.close();
        return Shepherd::inert(version);
    }

    let reading = Arc::clone(&outbox);
    let handlers = Arc::clone(&dispatch);
    let reader_spawn = std::thread::Builder::new()
        .name("shep-channel-reader".to_string())
        .spawn(move || {
            let mut reader = reader;
            reader_loop(&mut reader, &reading, &handlers, &warn);
        });
    if let Err(error) = reader_spawn {
        // The writer is still useful without this thread: `ready()` and
        // `metric()` still reach the shepherd. Only actions go unanswered,
        // since nothing is left to read `ShepherdMessage::Action` off the
        // wire and dispatch it -- warn and hand back a handle that still
        // does the two things it can, rather than tearing the writer down
        // over a failure that does not touch it.
        warn(&format!(
            "failed to spawn the shep-channel reader thread: {error}; readiness and metrics still work, but no action sent to this process will ever be answered"
        ));
    }

    Shepherd(Arc::new(Inner {
        outbox: Some(outbox),
        dispatch,
        version,
    }))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// Bounds this module's one test that could hang the calling thread
    /// forever on a real regression (the deadlock test below). A working
    /// reader answers in microseconds; this is slack for a loaded runner,
    /// not an expected duration -- the same convention `outbox`'s and
    /// `session`'s own tests use.
    const DEADLINE: Duration = Duration::from_secs(5);

    /// fails if a handle with no channel refuses work. An app must be able
    /// to call every method without asking whether it has a channel, which
    /// is the whole of D3.
    #[test]
    fn an_inert_handle_accepts_everything_and_does_nothing() {
        let shepherd = Shepherd::inert(None);
        assert!(!shepherd.is_active());
        shepherd.on_action("gc", |_, _| "ok".to_string());
        shepherd.on_shutdown(|| {});
        shepherd.metric("rps", 42.0);
        shepherd.ready().expect("an inert ready is not an error");
        assert_eq!(shepherd.dropped_metrics(), 0);
        assert_eq!(shepherd.version(), None);
    }

    /// fails if the no-channel advice stops naming all three fields that
    /// would open one. An author reading this line is deciding which to
    /// set. Asserts the exact `channel = true` clause, not the bare
    /// substring "channel" -- that substring also occurs in the advice's
    /// leading "no channel on this process" and would pass even with the
    /// whole `channel = true` clause deleted.
    #[test]
    fn the_no_channel_advice_names_every_field_that_opens_one() {
        for field in ["channel = true", "wait_ready", "shutdown_with_message"] {
            assert!(
                NO_CHANNEL_ADVICE.contains(field),
                "advice does not mention {field}"
            );
        }
    }

    /// fails if the shutdown warning stops naming the method an author has
    /// to call. D5 makes this warning the only thing between a missing
    /// handler and a `kill_timeout`.
    #[test]
    fn the_unhandled_shutdown_warning_names_the_method_to_call() {
        assert!(UNHANDLED_SHUTDOWN_ADVICE.contains("on_shutdown"));
        assert!(UNHANDLED_SHUTDOWN_ADVICE.contains("kill_timeout"));
    }

    /// fails if two malformed frames produce two warnings instead of one,
    /// or if a malformed frame ends the loop instead of being skipped. This
    /// is the test Finding A asked for by name: `start`'s two threads were
    /// previously inline closures with no test reaching them at all, so the
    /// once-per-loop latch and the daemon-matching "skip and keep reading"
    /// behavior were both unverified. Non-vacuity is proven separately,
    /// below this module (see the fix-round report).
    #[test]
    fn two_malformed_lines_warn_once_and_the_loop_keeps_going() {
        let mut reader =
            Cursor::new(b"not json\nalso not json\n{\"kind\":\"shutdown\"}\n".to_vec());
        let outbox = Outbox::new(4);
        let dispatch = RwLock::new(Dispatch::default());
        let shutdown_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&shutdown_hits);
        dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_shutdown(Box::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));

        let warnings: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let warn = |message: &str| warnings.lock().unwrap().push(message.to_string());

        reader_loop(&mut reader, &outbox, &dispatch, &warn);

        let collected = warnings.into_inner().unwrap();
        assert_eq!(
            collected.len(),
            1,
            "expected exactly one warning for two malformed lines, got {collected:?}"
        );
        assert_eq!(
            shutdown_hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the well-formed shutdown after the two bad lines was never reached"
        );
    }

    /// fails if end of stream leaves the outbox open, which would park the
    /// writer thread in `pop()` forever.
    #[test]
    fn end_of_stream_breaks_the_loop_and_closes_the_outbox() {
        let mut reader = Cursor::new(Vec::new());
        let outbox = Outbox::new(4);
        let dispatch = RwLock::new(Dispatch::default());
        let warn = |_: &str| {};

        reader_loop(&mut reader, &outbox, &dispatch, &warn);

        // Non-blocking probe for "closed" before the (otherwise
        // could-park-forever) pop() below: push_blocking on a closed
        // outbox returns Err(Closed) immediately rather than waiting, so a
        // dropped `outbox.close()` in reader_loop fails right here in
        // microseconds instead of hanging this test on an empty, open
        // queue with no bound.
        assert!(
            outbox.push_blocking(ChildMessage::Ready).is_err(),
            "reader_loop returned without closing the outbox"
        );
        assert_eq!(
            outbox.pop(),
            None,
            "outbox should read as closed-and-empty after EOF, not park a waiter"
        );
    }

    /// fails if an action's reply does not reach the outbox, or drops the
    /// id the daemon needs to match it to its trigger.
    #[test]
    fn an_actions_reply_reaches_the_outbox_carrying_its_id() {
        let mut reader = Cursor::new(b"{\"kind\":\"action\",\"name\":\"gc\",\"id\":7}\n".to_vec());
        let outbox = Outbox::new(4);
        let dispatch = RwLock::new(Dispatch::default());
        dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_action("gc".to_string(), Box::new(|_, _| "ok".to_string()));
        let warn = |_: &str| {};

        reader_loop(&mut reader, &outbox, &dispatch, &warn);

        match outbox.pop() {
            Some(ChildMessage::ActionReply { action, body, id }) => {
                assert_eq!(action, "gc");
                assert_eq!(body, "ok");
                assert_eq!(id, Some(7));
            }
            other => panic!("expected an action reply, got {other:?}"),
        }
    }

    /// fails if the writer stops after the first `pop()` once `close()` has
    /// been called, rather than draining what was already queued. A
    /// shutdown reply queued right before the shepherd goes away must still
    /// go out.
    #[test]
    fn the_writer_drains_what_is_already_queued_after_close() {
        let outbox = Outbox::new(4);
        outbox
            .push_blocking(ChildMessage::Ready)
            .expect("room for the first message");
        outbox
            .push_blocking(ChildMessage::Metric {
                name: "rps".to_string(),
                value: 1.0,
            })
            .expect("room for the second message");
        outbox.close();

        let mut written = Vec::new();
        writer_loop(&mut written, &outbox);

        let text = String::from_utf8(written).expect("valid utf8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "expected both already-queued messages to be written, got {lines:?}"
        );
        assert!(lines[0].contains("\"kind\":\"ready\""));
        assert!(lines[1].contains("\"kind\":\"metric\""));
    }

    /// fails if a handler that registers another handler deadlocks the
    /// reader on itself. Finding B: `Dispatch::resolve` clones the handler
    /// out and drops the registry's read guard before `run` calls into app
    /// code, so a `reload` action swapping its own handlers -- the obvious
    /// shape this would break on -- completes instead of parking the write
    /// lock `resolve` would otherwise still be holding on the same thread.
    ///
    /// Bounded like `outbox`'s own real-blocking-risk tests: a real
    /// deadlock here would hang the calling thread forever with no natural
    /// timeout, so `reader_loop` runs on its own thread and this fails fast
    /// on the deadline instead of hanging the whole suite.
    #[test]
    fn a_handler_that_registers_another_handler_does_not_deadlock_the_reader() {
        let shepherd = Shepherd::inert(None);
        let inner_shepherd = shepherd.clone();
        shepherd.on_action("reload", move |_, _| {
            inner_shepherd.on_action("late", |_, _| "late ok".to_string());
            "reloaded".to_string()
        });

        let outbox = Arc::new(Outbox::new(4));
        let dispatch = Arc::clone(&shepherd.0.dispatch);
        let outbox_thread = Arc::clone(&outbox);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = Cursor::new(
                b"{\"kind\":\"action\",\"name\":\"reload\",\"id\":1}\n\
                  {\"kind\":\"action\",\"name\":\"late\",\"id\":2}\n"
                    .to_vec(),
            );
            let warn = |_: &str| {};
            reader_loop(&mut reader, &outbox_thread, &dispatch, &warn);
            let _ = tx.send(());
        });

        rx.recv_timeout(DEADLINE)
            .expect("reader_loop deadlocked: a handler that registers a handler hung the reader");

        let first = outbox.pop().expect("the reload reply");
        let second = outbox
            .pop()
            .expect("the late reply, registered inside the reload handler");
        match (first, second) {
            (
                ChildMessage::ActionReply {
                    action: action1,
                    body: body1,
                    id: id1,
                },
                ChildMessage::ActionReply {
                    action: action2,
                    body: body2,
                    id: id2,
                },
            ) => {
                assert_eq!(action1, "reload");
                assert_eq!(body1, "reloaded");
                assert_eq!(id1, Some(1));
                assert_eq!(action2, "late");
                assert_eq!(body2, "late ok");
                assert_eq!(id2, Some(2));
            }
            other => panic!("expected two action replies, got {other:?}"),
        }
    }
}
