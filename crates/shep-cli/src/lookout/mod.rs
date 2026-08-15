//! `shep lookout` (alias `dash`): the terminal dashboard.
//!
//! Phase 12a builds the shell and one pane, the flock table. The bleats feed,
//! the sheep detail pane and the host-usage strip are 12b —
//! `docs/lookout/README.md` says what those frames are for.
//!
//! **Two tasks, two channels.** The link task ([`link::run_link`]) owns the
//! connection: it subscribes, polls, repairs on a drop, climbs a bounded
//! reconnect ladder and freezes when it runs out. The UI loop ([`run_ui`])
//! owns the screen: it reads the keyboard, applies [`app::Msg`]s to
//! [`app::App`], and redraws. They talk over an `mpsc` in each direction —
//! `Msg`s out of the link, poll requests into it. Neither borrows the other,
//! which is what lets each be tested on its own.
//!
//! **This verb does not exit when the shepherd dies.** `bleats` does, and that
//! is right for a follow. A standing dashboard that vanished would take the
//! last known state of the flock with it, at the moment an operator most wants
//! to read it — Rin's ruling, and the reason [`link::RECONNECT_ATTEMPTS`]
//! exists.

pub mod app;
// `#[cfg(test)]`: every item in `frames` is read by tests and by the gallery
// writer, and by nothing else. `shep-cli` is `[[bin]]`-only, so `pub` exempts
// nothing from `dead_code` — see `output::table::render_table`, which carries
// an `#[allow(dead_code)]` and a comment saying exactly this.
#[cfg(test)]
pub mod frames;
pub mod input;
pub mod link;
pub mod source;
pub mod term;
pub mod theme;
pub mod view;

use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use shep_core::paths::ShepPaths;
use tokio::sync::mpsc;

use self::app::{App, Control, Effect, Msg};
// The trait, for the opening dial below. `BusEvent` and `KeyPress` are named
// only from the test module and are imported there, not here: an import used
// solely by `#[cfg(test)]` code warns in the ordinary build, and `-D warnings`
// makes that a task-gate failure.
use self::source::Shepherd;
use self::theme::Palette;
use crate::cli::{Format, LookoutArgs};
use crate::exit::ExitCode;
use crate::output::{self, Streams};

/// How often the uptime column is re-derived.
///
/// One second. Nothing on the wire changes on this tick — it exists so a
/// running sheep's UPTIME advances between the two-second polls instead of
/// stepping. Time-derived cells are the one thing a purely event-driven redraw
/// starves.
pub const HEARTBEAT: Duration = Duration::from_secs(1);

/// The floor on the gap between two draws.
///
/// ~30 frames a second. A `shep muster` of a large flock emits a `process.*`
/// event per sheep within a second or two; without this gate each one costs a
/// full render. The gate makes a burst of N events cost at most one draw per
/// 33ms rather than N draws, and it is armed only while something is actually
/// dirty, so an idle dashboard draws nothing at all.
pub const MIN_REDRAW: Duration = Duration::from_millis(33);

/// Runs the dashboard, and returns the [`ExitCode`] to exit with.
///
/// Three refusals, all of them before a single escape byte is written:
///
/// - [`ExitCode::Usage`] when stdout is not a terminal.
/// - [`ExitCode::DaemonUnreachable`] (or [`ExitCode::ProtocolMismatch`]) when
///   the FIRST connection fails — see [`source::LinkError::exit_code`]. A
///   shepherd that was never running is not the case Rin's retry-then-freeze
///   ruling is about, and lookout refuses it exactly as `shep flock` would.
/// - [`ExitCode::Failure`] when the terminal could not be put into raw mode.
///
/// After that it does not exit on its own at all: the ladder and the freeze
/// take over, and the operator quits.
pub async fn lookout(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &LookoutArgs,
) -> ExitCode {
    // A TUI piped into a file is a usage error, not a rendering mode: the
    // alternative is writing alternate-screen escapes into somebody's log.
    // This is also what makes the refusal testable — `assert_cmd` captures
    // stdout, so the e2e case gets this path for free.
    if !std::io::stdout().is_terminal() {
        let _ = output::emit_error(
            &mut *streams.err,
            fmt,
            ExitCode::Usage.code_str(),
            "lookout needs a terminal; stdout is not one",
        );
        return ExitCode::Usage;
    }

    // The FIRST dial, and it happens HERE — before the palette, before the
    // panic hook, before raw mode, and before anything has been drawn. A
    // shepherd that was never running gets the same refusal `shep flock` gets:
    // one error envelope on stderr and exit 5. Rin's "lookout never exits on
    // its own" is about a shepherd that dies *underneath* a running dashboard;
    // opening the alternate screen to spend eight seconds reconnecting to
    // something that was never there, announcing a death that never happened
    // and then exiting `Success`, is a different thing and a worse one.
    //
    // Everything after this point is the running-dashboard case, which is the
    // one the ladder is for.
    let mut shepherd = source::UnixShepherd::new(&paths.socket);
    let opened = match shepherd.link().await {
        Ok(opened) => opened,
        Err(err) => {
            let code = err.exit_code();
            let _ = output::emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            return code;
        }
    };

    let palette = Palette::detect(
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
        std::env::var_os("COLORTERM").as_deref(),
    );
    let control = resolve_control(args.allow_control, &paths.kv);
    let app = App::new(
        palette,
        control,
        paths.home.to_string_lossy().into_owned(),
        Instant::now(),
    );

    // Hook first, then the guard, then raw mode, then the alternate screen —
    // nothing that can panic in between. See `term`'s own module doc.
    term::install_panic_hook();
    // ARMED BEFORE `enter()`, deliberately. `enter` turns raw mode on and then
    // enters the alternate screen; if the second step fails, an earlier draft
    // of this function returned `Err` with raw mode still on and no guard yet
    // in existence, leaving the operator's shell with no echo and no line
    // editing — the failure design decision 7 calls the worst a TUI can have,
    // reached through the error path of the very function that prevents it.
    // `restore()` is documented idempotent and safe outside raw mode, so
    // arming it early costs nothing on the paths that never enter.
    let _guard = term::RestoreGuard::new();
    let out = match term::enter() {
        Ok(out) => out,
        Err(err) => {
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Failure.code_str(),
                &format!("could not put the terminal into raw mode: {err}"),
            );
            return ExitCode::Failure;
        }
    };

    let terminal = match Terminal::new(CrosstermBackend::new(out)) {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Failure.code_str(),
                &format!("could not open the terminal: {err}"),
            );
            return ExitCode::Failure;
        }
    };

    let (msg_tx, msg_rx) = mpsc::channel(1024);
    let (poll_tx, poll_rx) = mpsc::channel(8);
    // The connection opened above is handed straight in, so the link task
    // never dials for its first one.
    let link = tokio::spawn(link::run_link(
        shepherd,
        opened,
        msg_tx,
        poll_rx,
        link::FLOCK_POLL,
    ));

    let events = crossterm::event::EventStream::new();
    let _ = run_ui(app, terminal, events, msg_rx, poll_tx).await;
    link.abort();
    ExitCode::Success
}

/// Whether this lookout may act, from the flag or from the KV store.
///
/// The flag wins. The store is `$SHEP_HOME/kv.json` —
/// `shep set lookout.allow_control true` — rather than a new `[lookout]`
/// section in `shep.toml`, because this gate is the operator's own and not the
/// shepherd's: lookout runs as the operator, and the shepherd cannot tell one
/// of its keypresses from a `shep stop`. `[whistle] allow_control` in
/// `$SHEP_HOME/shep.toml` is daemon-side for the opposite reason — its
/// control tools act for a client nobody is watching.
///
/// A store that cannot be read is read as "no". A dashboard that failed open on
/// an unreadable file would be a gate that disappears exactly when something is
/// wrong with the machine.
#[must_use]
pub fn resolve_control(flag: bool, kv: &Path) -> Control {
    if flag {
        return Control::Allowed;
    }
    match shep_core::kv::get(kv, "lookout.allow_control") {
        Ok(Some(value)) if value == "true" => Control::Allowed,
        _ => Control::ReadOnly,
    }
}

/// The UI loop.
///
/// Generic over the backend and over the key source, so a test drives the whole
/// thing with a `TestBackend` and a finite `Stream` — no terminal, no socket.
/// Returns the terminal so that test can read the last frame.
///
/// Four arms, `biased` in this order:
///
/// 1. **`SIGTERM`.** In raw mode Ctrl-C is a key event, not a signal, so this
///    arm is not about the operator — it is a session teardown or a `kill`.
///    Breaking the loop lets the `RestoreGuard` in the caller run, which is the
///    difference between a tidy exit and a broken terminal.
/// 2. **The keyboard**, for latency: a keypress that waited behind a burst of
///    bus events feels like a hung program.
/// 3. **The link's messages.**
/// 4. **The heartbeat**, which advances the uptime column and nothing else.
///
/// **`biased` makes an exhausted arm a hazard, not a detail.** A `Stream` that
/// has ended returns `Poll::Ready(None)` immediately and forever, and an `mpsc`
/// whose senders are all dropped does the same — so an arm that has run dry,
/// sitting above another one in a biased select, wins every iteration and
/// starves everything below it for the life of the loop. Concretely: a
/// persistent stdin read error would freeze the display while the link and the
/// heartbeat never got polled, and a key source that ended would spin this loop
/// at full tilt forever. So arms 2 and 3 each carry a **branch precondition**
/// (`, if !keys_done` / `, if !link_done`) that takes them out of the running
/// for good once their source is done. Arm 4 is unconditional, so the `select!`
/// always has an enabled branch. Arm 1 needs the same idea in its other form —
/// `std::future::pending()` — because a missing signal handler is not a
/// condition that changes.
///
/// The redraw is not an arm: it happens after the `select!`, gated on `dirty`
/// and on [`MIN_REDRAW`] having elapsed.
pub async fn run_ui<B: Backend, S>(
    mut app: App,
    mut terminal: Terminal<B>,
    events: S,
    mut msgs: mpsc::Receiver<Msg>,
    polls: mpsc::Sender<()>,
) -> Terminal<B>
where
    S: Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
{
    let mut events = events;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    // Set once each, when their source runs dry. See this function's doc.
    let mut keys_done = false;
    let mut link_done = false;

    let mut dirty = true;
    // `Option`, not `Instant::now() - MIN_REDRAW`: subtracting from a fresh
    // `Instant` is a panic on a platform whose monotonic clock starts near
    // zero, and "has never drawn" is what the first iteration actually means.
    let mut last_draw: Option<Instant> = None;

    loop {
        if dirty && last_draw.is_none_or(|at| at.elapsed() >= MIN_REDRAW) {
            let _ = terminal.draw(|frame| view::draw(&app, frame));
            dirty = false;
            last_draw = Some(Instant::now());
        }

        let msg = tokio::select! {
            biased;
            () = async {
                match sigterm.as_mut() {
                    Some(signal) => {
                        signal.recv().await;
                    }
                    // No handler could be installed; this arm must then never
                    // complete, rather than completing immediately and
                    // spinning the loop.
                    None => std::future::pending().await,
                }
            } => break,
            event = events.next(), if !keys_done => match event {
                Some(Ok(crossterm::event::Event::Resize(..))) => Some(Msg::Resize),
                Some(Ok(event)) => input::map_key(&event).map(Msg::Key),
                // A key source that has ENDED, or has started erroring. The
                // real one does neither in ordinary use; a test's ends on its
                // last scripted key, and a stdin whose descriptor has gone bad
                // errors on every poll rather than once. Both conditions are
                // permanent, and both retire this arm rather than being
                // shrugged off: an arm that keeps completing immediately,
                // above the link and the heartbeat in a `biased` select,
                // freezes the display and spins the process at full tilt. The
                // dashboard keeps running on the other arms; the operator
                // quits with a signal.
                Some(Err(_)) | None => {
                    keys_done = true;
                    None
                }
            },
            msg = msgs.recv(), if !link_done => match msg {
                Some(msg) => Some(msg),
                // Every sender dropped: the link task ended without freezing,
                // which only happens if it was aborted. Keep the last frame up
                // — and retire this arm, because a closed `mpsc` is `Ready`
                // forever and would otherwise starve the heartbeat below it.
                None => {
                    link_done = true;
                    None
                }
            },
            _ = heartbeat.tick() => Some(Msg::Tick { now: Instant::now() }),
        };

        // Nothing to apply. Not a spin risk, and it is worth naming why there
        // are exactly three ways to get here: an unbound keypress (which needs
        // a fresh keystroke), and each of the two sources retiring (once each,
        // ever). Nothing on this path can repeat without something new
        // happening, so there is no sleep and no yield.
        let Some(msg) = msg else { continue };

        match app.update(msg) {
            Effect::Quit => break,
            Effect::PollNow => {
                // `try_send`, not `send`: a full poll channel means a repair is
                // already queued, and blocking the UI on it would stall the
                // screen for exactly as long as the shepherd is slow. A CLOSED
                // one means the link task has ended — which the reducer
                // already accounts for, by refusing `r` outright once the link
                // is `Lost` rather than letting a request vanish here.
                let _ = polls.try_send(());
                dirty = true;
            }
            // Task 6 replaces this with the read. Until then the reducer's
            // request is honoured by redrawing and nothing else — which is
            // correct, because there is no feed on screen yet to refresh.
            Effect::RefreshFeed => dirty = true,
            Effect::None => dirty = true,
        }
    }

    terminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shep_core::protocol::BusEvent;

    use crate::lookout::app::{App, Control, KeyPress};
    use crate::lookout::theme::Palette;

    /// fails if the loop stops drawing, or stops leaving on `q`. This is the
    /// whole loop under test with no terminal and no socket: a `TestBackend`
    /// for the screen and a finite `Stream` for the keyboard.
    ///
    /// IR-46: bounded, because a loop that never sees its quit key would hang
    /// the suite rather than fail it.
    #[tokio::test(start_paused = true)]
    async fn the_loop_draws_and_quits_on_a_keypress() {
        let (msg_tx, msg_rx) = mpsc::channel(16);
        let (poll_tx, _poll_rx) = mpsc::channel(1);
        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let keys = stream::iter(vec![Ok(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ))]);

        drop(msg_tx);
        let done = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, keys, msg_rx, poll_tx),
        )
        .await;
        let terminal = done.expect("the loop left on `q` within ten seconds");
        let frame = crate::lookout::frames::render_text(terminal.backend().buffer());
        assert!(frame.contains("shep lookout"), "it drew at least once");
    }

    /// fails if the loop stops asking for a poll when the reducer says to. The
    /// `Effect::PollNow` a drop produces has to reach the link task, or the
    /// repair the link task exists for never happens.
    ///
    /// **This test is also the starvation pin**, and it is the reason
    /// `stream::empty()` is the key source rather than an accident of
    /// convenience. An empty stream is `Poll::Ready(None)` on its first poll
    /// and on every poll after it, so in a `biased` select with the keyboard
    /// above the message channel, an implementation that did not retire the
    /// keyboard arm would win that arm forever and never read either message
    /// queued below — the loop would spin until the `timeout` fired and this
    /// test would fail on its `expect`. It cannot pass by accident: the two
    /// messages it sends are on the arm that has to be reachable.
    #[tokio::test(start_paused = true)]
    async fn a_drop_forwards_a_poll_request_to_the_link_task() {
        let (msg_tx, msg_rx) = mpsc::channel(16);
        let (poll_tx, mut poll_rx) = mpsc::channel(4);
        msg_tx
            .send(Msg::Event(BusEvent::Dropped { count: 4 }))
            .await
            .unwrap();
        msg_tx.send(Msg::Key(KeyPress::Quit)).await.unwrap();
        drop(msg_tx);

        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, stream::empty(), msg_rx, poll_tx),
        )
        .await
        .expect("the loop left within ten seconds");

        assert_eq!(poll_rx.try_recv(), Ok(()), "the poll request was forwarded");
    }

    /// fails if the KV store stops being a source for the gate. The flag has to
    /// win, and the store has to work — `shep set lookout.allow_control true`
    /// is the whole point of not adding a config section for one bool.
    #[test]
    fn the_flag_wins_over_the_store_and_the_store_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let kv = dir.path().join("kv.json");
        assert_eq!(resolve_control(false, &kv), Control::ReadOnly);

        shep_core::kv::set(&kv, "lookout.allow_control", "true").unwrap();
        assert_eq!(resolve_control(false, &kv), Control::Allowed);
        assert_eq!(resolve_control(true, &kv), Control::Allowed);

        shep_core::kv::set(&kv, "lookout.allow_control", "false").unwrap();
        assert_eq!(resolve_control(false, &kv), Control::ReadOnly);
        assert_eq!(
            resolve_control(true, &kv),
            Control::Allowed,
            "the flag wins over a store that says no"
        );
    }
}
