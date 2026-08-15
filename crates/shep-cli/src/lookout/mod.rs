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
pub mod tail;
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
    let _ = run_ui(
        app,
        terminal,
        events,
        msg_rx,
        poll_tx,
        source::LocalReader::new(),
    )
    .await;
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
///
/// **This phase adds no arm to the `select!` above.** The host sample rides
/// arm 4, the heartbeat: sampling memory and a load average costs
/// microseconds and no process-table walk, so it rides along for free rather
/// than re-deriving the arm-retirement reasoning this doc just walked
/// through. The feed rides the redraw gate below instead of an arm at all —
/// `Effect::RefreshFeed` sets `feed_dirty`, and the read happens immediately
/// before the frame that shows its result, coalescing a held key's burst of
/// moved selections into one read per [`MIN_REDRAW`] window rather than one
/// per keypress. See the phase plan's design decisions 3 and 11.
pub async fn run_ui<B: Backend, S, L>(
    mut app: App,
    mut terminal: Terminal<B>,
    events: S,
    mut msgs: mpsc::Receiver<Msg>,
    polls: mpsc::Sender<()>,
    mut local: L,
) -> Terminal<B>
where
    S: Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
    L: source::Local,
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
    // Set by `Effect::RefreshFeed` and cleared once the coalesced read below
    // has run. See the doc above this function for why this is a flag rather
    // than an `mpsc` arm.
    let mut feed_dirty = false;
    // `Option`, not `Instant::now() - MIN_REDRAW`: subtracting from a fresh
    // `Instant` is a panic on a platform whose monotonic clock starts near
    // zero, and "has never drawn" is what the first iteration actually means.
    let mut last_draw: Option<Instant> = None;

    loop {
        // One gate, read once, so the feed cannot be refreshed on a frame
        // that is not about to be drawn — and, more importantly, is
        // refreshed BEFORE the frame that shows it rather than after.
        let may_draw = last_draw.is_none_or(|at| at.elapsed() >= MIN_REDRAW);
        if feed_dirty && may_draw {
            // The paths are cloned out before `app` is borrowed mutably.
            let (out, err) = app.selected_row().map_or((None, None), |row| {
                (row.info.out_file.clone(), row.info.err_file.clone())
            });
            let tail = local.tail(out.as_deref().map(Path::new), err.as_deref().map(Path::new));
            // `let _`: `Msg::Bleats` returns `Effect::None` by construction —
            // see its arm in the reducer — and acting on a returned effect
            // here would be the one place this design could recurse.
            let _ = app.update(Msg::Bleats { tail });
            feed_dirty = false;
            dirty = true;
        }
        if dirty && may_draw {
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
            _ = heartbeat.tick() => {
                // The host sample rides this arm rather than adding a fifth
                // one. The `biased` select's arm-retirement reasoning is the
                // subtlest thing in this module and this phase deliberately
                // does not re-derive it; sampling memory and a load average
                // is microseconds and no process-table walk.
                //
                // Sampled unconditionally, and REFUSED by the reducer once
                // the link is lost — one enforcement point, the same
                // division `Msg::Tick` and the uptime clock already use.
                // `let _`: `Msg::Host` returns `Effect::None` by
                // construction.
                let _ = app.update(Msg::Host { sample: local.host() });
                Some(Msg::Tick { now: Instant::now() })
            }
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
            // Not the read. `Effect::RefreshFeed` arrives once per moved
            // selection, and ordinary terminals deliver a held `j` as
            // twenty to thirty Press events a second — so doing the I/O
            // here would put a synchronous 128 KiB read behind every
            // repeat, on the task that also owns the redraw. Coalesced onto
            // `MIN_REDRAW` above instead, which is the same gate the draw
            // already uses.
            Effect::RefreshFeed => {
                feed_dirty = true;
                dirty = true;
            }
            Effect::None => dirty = true,
        }
    }

    terminal
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use futures_util::stream;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shep_core::protocol::{BusEvent, ProcessInfo};
    use shep_core::status::ProcStatus;

    use crate::lookout::app::{App, Control, KeyPress};
    use crate::lookout::source::{HostSample, Local};
    use crate::lookout::tail::Tail;
    use crate::lookout::theme::Palette;

    /// A `Local` that touches no disk: a fixed sample, a fixed tail, and a
    /// count of each call. `Arc`, because `run_ui` takes the reader by value
    /// and only gives the terminal back.
    #[derive(Clone, Default)]
    struct FakeLocal {
        sample: Option<HostSample>,
        hosts: Arc<AtomicUsize>,
        tails: Arc<AtomicUsize>,
    }

    impl Local for FakeLocal {
        fn host(&mut self) -> Option<HostSample> {
            self.hosts.fetch_add(1, Ordering::Relaxed);
            self.sample
        }

        fn tail(&mut self, _out: Option<&Path>, _err: Option<&Path>) -> Tail {
            self.tails.fetch_add(1, Ordering::Relaxed);
            Tail::default()
        }
    }

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
            run_ui(app, terminal, keys, msg_rx, poll_tx, FakeLocal::default()),
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
            run_ui(
                app,
                terminal,
                stream::empty(),
                msg_rx,
                poll_tx,
                FakeLocal::default(),
            ),
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

    /// fails if **nothing ever produces a `Msg::Host`.** The strip renders
    /// from `App::host`, the reducer arm was written in Task 4, and every
    /// pane test and every gallery frame injects the message directly — so a
    /// heartbeat that still yielded only `Msg::Tick` leaves the shipped
    /// binary drawing `host  not read yet` forever with nothing red anywhere
    /// on the suite. That is what the first draft of this plan shipped, and
    /// it is invisible to every other check in the phase.
    ///
    /// Asserted on the READER rather than on a frame, because at this task
    /// the strip is not on screen yet — `draw` does not call it until
    /// Task 8, and Task 8 adds `a_heartbeat_puts_the_host_strip_on_the_frame`
    /// for the other half. What is testable here is the call, and the
    /// missing call is the bug.
    ///
    /// IR-46: bounded — a `timeout` around a genuinely async loop, and a quit
    /// queued on a timer, so it cannot hang.
    #[tokio::test(start_paused = true)]
    async fn the_heartbeat_asks_the_local_reader_for_a_host_sample() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let local = FakeLocal::default();
        let hosts = Arc::clone(&local.hosts);

        // After the 1-second heartbeat, so the tick lands first.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            let _ = msg_tx.send(Msg::Key(KeyPress::Quit)).await;
        });

        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(app, terminal, stream::empty(), msg_rx, poll_tx, local),
        )
        .await
        .expect("the loop left within ten seconds");

        assert!(
            hosts.load(Ordering::Relaxed) >= 1,
            "the heartbeat fired and never sampled the host"
        );
    }

    /// fails if a burst of keypresses costs one two-file read per key.
    ///
    /// `input::map_key` drops `KeyEventKind::Repeat`, but ordinary terminals
    /// deliver auto-repeat as a stream of Press events — so a held `j` on a
    /// long flock is twenty to thirty moved selections a second, and an
    /// uncoalesced `Effect::RefreshFeed` would put a synchronous 128 KiB
    /// read behind every one of them, on the task that also owns the
    /// redraw. The read is coalesced onto the `MIN_REDRAW` gate for that
    /// reason, so a burst costs one read rather than twenty-one.
    ///
    /// `assert_eq!(1)` and not `<= 2`: the exact number is the property. One
    /// snapshot and twenty moves arrive with no time between them, so
    /// nothing is read until the clock passes `MIN_REDRAW`, and then it is
    /// read once.
    ///
    /// **Not `start_paused`, and that is a deviation from the plan's draft
    /// worth stating rather than silently carrying.** `MIN_REDRAW`'s gate
    /// reads [`std::time::Instant`] — real wall-clock, deliberately, so
    /// `App`'s clock model stays usable outside a tokio runtime at all
    /// (`App`'s own doc: "No clock. Every `Instant` arrives on the
    /// message"). A *paused* tokio clock auto-advances `tokio::time::sleep`
    /// in virtual time with no matching real delay — measured on this
    /// machine, a virtual 1.5s sleep under `start_paused` completes in
    /// ~30µs of real time — so a version of this test on a paused clock
    /// could never see `MIN_REDRAW` actually elapse and would fail no
    /// matter how the coalescing is implemented: not flaky, deterministically
    /// red, confirmed over five runs. So this test runs the real clock and
    /// waits out a real (short) multiple of `MIN_REDRAW` instead of a virtual
    /// 1.5s, which is what the property under test — a real wall-clock gate
    /// — actually needs.
    ///
    /// IR-46: bounded by a real, short sleep and a real, generous timeout —
    /// this cannot hang.
    #[tokio::test]
    async fn a_burst_of_selection_moves_costs_one_read_and_not_one_per_key() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let local = FakeLocal::default();
        let tails = Arc::clone(&local.tails);

        let at = Instant::now();
        msg_tx
            .send(Msg::Snapshot {
                rows: (0..8)
                    .map(|id| {
                        ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
                    })
                    .collect(),
                at,
            })
            .await
            .unwrap();
        for _ in 0..20 {
            msg_tx.send(Msg::Key(KeyPress::SelectDown)).await.unwrap();
        }
        tokio::spawn(async move {
            // A nudge, not the quit: the redraw gate is read once per loop
            // iteration, right before the (possibly blocking) receive, so
            // real time elapsing WHILE the loop is blocked waiting for the
            // next message is never observed by that check — only the next
            // iteration's check sees it. `Msg::Resize` (any message would
            // do; this one touches neither the feed nor the selection)
            // wakes the loop once real time has cleared `MIN_REDRAW`, so
            // the burst's still-pending `feed_dirty` is read on THIS
            // iteration rather than staying stale until `Quit` breaks the
            // loop before the gate is ever re-checked.
            tokio::time::sleep(MIN_REDRAW * 3).await;
            let _ = msg_tx.send(Msg::Resize).await;
            tokio::time::sleep(MIN_REDRAW).await;
            let _ = msg_tx.send(Msg::Key(KeyPress::Quit)).await;
        });

        let app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/tmp/shep".to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            run_ui(app, terminal, stream::empty(), msg_rx, poll_tx, local),
        )
        .await
        .expect("the loop left within five seconds");

        assert_eq!(
            tails.load(Ordering::Relaxed),
            1,
            "a snapshot and twenty selection moves must coalesce into one read"
        );
    }
}
