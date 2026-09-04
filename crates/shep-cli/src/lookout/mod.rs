//! `shep lookout` (alias `dash`): the terminal dashboard.
//!
//! Four panes, one screen: the flock table stays the spine, and a selected
//! row now grows a sheep detail pane ([`view::detail`]) and a bleats feed
//! ([`view::bleats`]) beneath it, with a host-usage strip
//! ([`view::host`]) above. A narrow or short terminal drops panes before it
//! drops columns — [`view::panes_for`] is the tier table. `/` opens a name
//! filter in the status bar ([`app::App::on_key`]'s text-mode arm) that
//! narrows the table to matching rows in place, without touching what the
//! link task fetches. `source::TOPICS` has the argument for why the feed
//! re-reads the selected sheep's log files on every refresh instead of
//! subscribing to `log.*`. `docs/lookout/README.md` says what the rendered
//! frames are for.
//!
//! **Two tasks, three channels.** The link task ([`link::run_link`]) owns the
//! connection: it subscribes, polls, repairs on a drop, climbs a bounded
//! reconnect ladder and freezes when it runs out. The UI loop ([`run_ui`])
//! owns the screen: it reads the keyboard, applies [`app::Msg`]s to
//! [`app::App`], and redraws. They talk over an `mpsc` each way — `Msg`s out
//! of the link, and into it: poll requests, and one-shot [`app::Sent`]
//! requests the dashboard asks the link to send on this connection. Neither
//! task borrows the other, which is what lets each be tested on its own.
//!
//! **This verb does not exit when the shepherd dies.** `bleats` does, and that
//! is right for a follow. A standing dashboard that vanished would take the
//! last known state of the flock with it, at the moment an operator most wants
//! to read it — the maintainer's ruling, and the reason [`link::RECONNECT_ATTEMPTS`]
//! exists.

pub mod app;
// `#[cfg(test)]`: every item in `frames` is read by tests and by the gallery
// writer, and by nothing else. The package has a `[lib]` target, but `pub`
// here still exempts nothing from `dead_code`: `mod
// lookout` in `lib.rs` is private, not `pub mod`, and `lib.rs`'s own doc
// comment states the crate's whole public API as three entry points, every
// other item private — so `pub mod frames` nested inside a private module is
// invisible outside this crate regardless of the keyword. Reachability turns
// on module privacy, not on whether a `[lib]` target exists. See
// `output::table::render_table`, which carries an `#[allow(dead_code)]` and a
// comment saying the same thing from the other direction.
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

use self::app::{App, Control, Effect, Msg, RowKey, Sent};
// The trait, for the opening dial below. `BusEvent` and `KeyPress` are named
// only from the test module and are imported there, not here: an import used
// solely by `#[cfg(test)]` code warns in the ordinary build, and `-D warnings`
// makes that a task-gate failure.
use self::source::Shepherd;
use self::theme::Palette;
use crate::cli::LookoutArgs;
use crate::exit::ExitCode;
use crate::output::Streams;
use crate::style::{StyleLevel, StyleSource};

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
/// Four refusals, all of them before a single escape byte is written:
///
/// - [`ExitCode::Usage`] when stdout is not a terminal.
/// - [`ExitCode::DaemonUnreachable`] (or [`ExitCode::ProtocolMismatch`]) when
///   the FIRST connection fails — see [`source::LinkError::exit_code`]. A
///   shepherd that was never running is not the case the maintainer's retry-then-freeze
///   ruling is about, and lookout refuses it exactly as `shep flock` would.
/// - [`ExitCode::VersionSkew`] when that connection succeeds but the
///   shepherd answering it is a different crate version — `lookout` is
///   never exempt, since it drives the daemon for as long as it is open.
/// - [`ExitCode::Failure`] when the terminal could not be put into raw mode.
///
/// After that it does not exit on its own at all: the ladder and the freeze
/// take over, and the operator quits.
///
/// `style` is `run_argv`'s own already-resolved `(StyleLevel, StyleSource)`
/// pair, the same one every other verb's rendering already uses. Handed
/// straight to `App::set_style` below, so the settings screen's own STYLE
/// LEVEL row reports the layer that actually won rather than resolving a
/// second, possibly disagreeing answer of its own.
pub async fn lookout(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    args: &LookoutArgs,
    style: (StyleLevel, StyleSource),
) -> ExitCode {
    // A TUI piped into a file is a usage error, not a rendering mode: the
    // alternative is writing alternate-screen escapes into somebody's log.
    // This is also what makes the refusal testable — `assert_cmd` captures
    // stdout, so the e2e case gets this path for free.
    if !std::io::stdout().is_terminal() {
        return streams.fail(
            ExitCode::Usage,
            "lookout needs a terminal; stdout is not one",
        );
    }

    // The FIRST dial, and it happens HERE — before the palette, before the
    // panic hook, before raw mode, and before anything has been drawn. A
    // shepherd that was never running gets the same refusal `shep flock` gets:
    // one error envelope on stderr and exit 5. The maintainer's "lookout never exits on
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
            return streams.fail(code, &err.to_string());
        }
    };

    // `lookout` drives the daemon for as long as the dashboard stays open —
    // polling, subscribing, and (with `--allow-control`) acting on it — so
    // it can never be one of `RECOVERY_VERBS`. Applied here, on the FIRST
    // dial, rather than inside `link` itself: see `ClientFlock::client`'s
    // own doc for why. A reconnect on the ladder is not re-checked; a
    // shepherd cannot downgrade itself mid-run.
    if let Err(code) =
        crate::refuse_version_skew(streams, opened.0.client(), crate::VersionGuard::Enforce)
    {
        return code;
    }

    let palette = Palette::detect(
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
        std::env::var_os("COLORTERM").as_deref(),
    );
    let control = resolve_control(args.allow_control, &paths.kv);
    let mut app = App::new(
        palette,
        control,
        paths.home.to_string_lossy().into_owned(),
        Instant::now(),
    );
    app.set_style(style);

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
            return streams.fail(
                ExitCode::Failure,
                &format!("could not put the terminal into raw mode: {err}"),
            );
        }
    };

    let terminal = match Terminal::new(CrosstermBackend::new(out)) {
        Ok(terminal) => terminal,
        Err(err) => {
            return streams.fail(
                ExitCode::Failure,
                &format!("could not open the terminal: {err}"),
            );
        }
    };

    let (msg_tx, msg_rx) = mpsc::channel(1024);
    let (poll_tx, poll_rx) = mpsc::channel(8);
    // Capacity 2: one action plus one lamb fetch is the most that can be
    // outstanding, because the reducer refuses a second action while one is in
    // flight and the lamb fetch is coalesced onto the redraw gate.
    let (request_tx, request_rx) = mpsc::channel(2);
    // The connection opened above is handed straight in, so the link task
    // never dials for its first one.
    let link = tokio::spawn(link::run_link(
        shepherd,
        opened,
        msg_tx,
        link::Channels {
            polls: poll_rx,
            requests: request_rx,
        },
        link::FLOCK_POLL,
    ));

    let events = crossterm::event::EventStream::new();
    let _ = run_ui(
        app,
        terminal,
        events,
        msg_rx,
        poll_tx,
        request_tx,
        paths.daemon_config.clone(),
        paths.socket.clone(),
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
/// through. The feed and the lambs both ride the redraw gate below instead of
/// an arm at all — `Effect::RefreshFeed` and `Effect::RefreshSelected` set
/// `feed_dirty` and/or `lambs_dirty`, and the work happens immediately before
/// the frame that shows its result, coalescing a held key's burst of moved
/// selections into one read and one `Describe` per [`MIN_REDRAW`] window
/// rather than one per keypress. See the phase plan's design decisions 3
/// and 11.
///
/// `#[allow(clippy::too_many_arguments)]`: this crosses eight with
/// `daemon_config`/`socket_default`, the settings screen's own read target.
/// Bundling every existing test call site's positional args into a struct
/// to duck the lint would touch all seven of them for a cosmetic reason;
/// `frames::sheep` already carries the same attribute for the same kind of
/// tradeoff, at eight.
#[allow(clippy::too_many_arguments)]
pub async fn run_ui<B: Backend, S, L>(
    mut app: App,
    mut terminal: Terminal<B>,
    events: S,
    mut msgs: mpsc::Receiver<Msg>,
    polls: mpsc::Sender<()>,
    requests: mpsc::Sender<self::app::Sent>,
    // The settings screen's own read target. Carried as two owned `PathBuf`s
    // rather than `&ShepPaths`, so a test can hand this loop an arbitrary
    // pair without constructing a real `ShepPaths` -- and cloned into
    // `Effect::LoadSettings`'s `spawn_blocking` closure each time the screen
    // opens, since that closure outlives this function's own stack frame.
    daemon_config: std::path::PathBuf,
    socket_default: std::path::PathBuf,
    mut local: L,
) -> Terminal<B>
where
    S: Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
    L: source::Local,
{
    let mut events = events;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sigterm = crate::shutdown::Terminate::install().ok();

    // Set once each, when their source runs dry. See this function's doc.
    let mut keys_done = false;
    let mut link_done = false;

    let mut dirty = true;
    // Set by `Effect::RefreshFeed` and cleared once the coalesced read below
    // has run. See the doc above this function for why this is a flag rather
    // than an `mpsc` arm.
    let mut feed_dirty = false;
    // Set by `Effect::RefreshSelected` and by `Effect::PollNow`, cleared once
    // the coalesced request below has gone out. A flag rather than an `mpsc`
    // arm, for the reason this function's doc gives about the `biased`
    // select's arm retirement.
    let mut lambs_dirty = false;
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
            // Nothing selected means an empty flock (`App::reseat` keeps the
            // selection on a real sheep whenever one exists), and the pane's
            // own header already says so ("bleats  no sheep is selected").
            // `tail::read`'s `(None, None)` early return exists for a
            // DIFFERENT case — a selected sheep whose shepherd predates the
            // `out_file`/`err_file` fields — and reusing it here would print
            // that sentence under a header naming a sheep that does not
            // exist. Skipping the read keeps the header the pane's one and
            // only sentence for this case, matching the table and detail
            // panes' own single-sentence pattern for the same state.
            let tail = match app.selected_row() {
                None => tail::Tail::default(),
                Some(row) => {
                    // The paths are cloned out before `app` is borrowed
                    // mutably.
                    let (out, err) = (row.info.out_file.clone(), row.info.err_file.clone());
                    local.tail(out.as_deref().map(Path::new), err.as_deref().map(Path::new))
                }
            };
            // `let _`: `Msg::Bleats` returns `Effect::None` by construction —
            // see its arm in the reducer — and acting on a returned effect
            // here would be the one place this design could recurse.
            let _ = app.update(Msg::Bleats { tail });
            feed_dirty = false;
            dirty = true;
        }
        if lambs_dirty && may_draw {
            // The height is read here and not in the reducer: `run_ui` knows
            // the terminal, `App` deliberately does not, and a terminal too
            // short to draw the detail pane must not pay for a process-table
            // walk it cannot show. A size that cannot be read is treated as
            // too short, which errs toward asking for nothing.
            let height = terminal.size().map_or(0, |size| size.height);
            if view::panes_for(height).detail
                && let Some(RowKey::Sheep(id)) = app.selected()
            {
                // `try_send`, for `Effect::PollNow`'s reason: a full
                // channel means a request is already queued, and a
                // dropped lamb fetch reads as "not read yet", which the
                // pane already knows how to say.
                let _ = requests.try_send(Sent::Lambs { id });
            }
            lambs_dirty = false;
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
                Some(Ok(event)) => input::map_key(&event, app.mode()).map(Msg::Key),
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
                // `r` means "tell me again", so it refreshes everything the
                // pane shows and not only the table.
                lambs_dirty = true;
                dirty = true;
            }
            // Not the read. `Effect::RefreshFeed` arrives once per snapshot,
            // and `Effect::RefreshSelected` arrives once per moved selection
            // — ordinary terminals deliver a held `j` as twenty to thirty
            // Press events a second, so doing the I/O here would put a
            // synchronous 128 KiB read (and a `Describe` request) behind
            // every repeat, on the task that also owns the redraw. Coalesced
            // onto `MIN_REDRAW` above instead, which is the same gate the
            // draw already uses.
            Effect::RefreshFeed => {
                feed_dirty = true;
                dirty = true;
            }
            Effect::RefreshSelected => {
                feed_dirty = true;
                lambs_dirty = true;
                dirty = true;
            }
            Effect::Send(sent) => {
                // `try_send`, not `send`: blocking the UI on a full channel
                // would stall the screen for as long as the shepherd is slow.
                // A failure comes straight back to the reducer rather than
                // being dropped, because the reducer is already showing an
                // in-flight line about it.
                if let Err(err) = requests.try_send(sent) {
                    let (mpsc::error::TrySendError::Full(sent)
                    | mpsc::error::TrySendError::Closed(sent)) = err;
                    // `let _`: `Msg::Unsent` returns `Effect::None` by
                    // construction, and acting on a returned effect here is
                    // the one place this design could recurse.
                    let _ = app.update(Msg::Unsent { sent });
                }
                dirty = true;
            }
            // The one arm that does file I/O off this task on purpose,
            // rather than as an oversight: `spawn_blocking` even though the
            // read takes no lock, because "no file I/O on the redraw task"
            // is cheaper to hold as a rule than to re-judge at every call
            // site.
            //
            // The style passed in is `app.style()`, not a fresh resolution:
            // `run_argv` already resolved `--style`/`$SHEP_STYLE`/
            // `shep.toml`'s `[style] level` once, before this loop started,
            // and handed the result to `App::set_style`. Reading it back
            // here rather than resolving again is what keeps the settings
            // screen's own STYLE LEVEL row agreeing with the rest of the
            // CLI about which layer won, flag included.
            Effect::LoadSettings => {
                let path = daemon_config.clone();
                let socket_default = socket_default.clone();
                let style = app.style();
                let result = tokio::task::spawn_blocking(move || {
                    crate::commands::settings::load_settings(&path, &socket_default, style)
                })
                .await
                .map_err(|err| err.to_string())
                .and_then(|inner| inner.map_err(|err| err.to_string()));
                // `let _`: `Msg::Settings` returns `Effect::None`
                // unconditionally, the same reason `Msg::Bleats`'s call site
                // above gives.
                let _ = app.update(Msg::Settings { result });
                dirty = true;
            }
            Effect::None => dirty = true,
        }
    }

    terminal
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use futures_util::stream;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shep_core::protocol::{BusEvent, ProcessInfo};
    use shep_core::status::ProcStatus;

    use crate::lookout::app::{App, Control, KeyPress, Sent};
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
        let (request_tx, _request_rx) = mpsc::channel(2);
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
            run_ui(
                app,
                terminal,
                keys,
                msg_rx,
                poll_tx,
                request_tx,
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
                FakeLocal::default(),
            ),
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
        let (request_tx, _request_rx) = mpsc::channel(2);
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
                request_tx,
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
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
        let (request_tx, _request_rx) = mpsc::channel(2);
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
            run_ui(
                app,
                terminal,
                stream::empty(),
                msg_rx,
                poll_tx,
                request_tx,
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
                local,
            ),
        )
        .await
        .expect("the loop left within ten seconds");

        assert!(
            hosts.load(Ordering::Relaxed) >= 1,
            "the heartbeat fired and never sampled the host"
        );
    }

    /// fails if a heartbeat does not put the host strip on the frame. The
    /// other half of `the_heartbeat_asks_the_local_reader_for_a_host_sample`
    /// above, which could only assert the call because the strip was not
    /// drawable yet. This is the end-to-end: a `Local` that reports a
    /// sample, one heartbeat, and the numbers on the rendered frame.
    ///
    /// It is the only check in the phase that would catch a heartbeat that
    /// sampled and a `draw` that ignored the result, or the reverse.
    ///
    /// **Not `start_paused`, for the same reason
    /// `a_burst_of_selection_moves_costs_one_read_and_not_one_per_key` isn't**:
    /// the redraw that would carry the sampled host onto the frame is gated
    /// on `MIN_REDRAW`, which reads real `std::time::Instant`, and a paused
    /// clock's virtual sleeps resolve in microseconds of real time — so the
    /// gate would never actually open and the loop would exit on its first,
    /// pre-heartbeat draw every time. This one measured the same way:
    /// deterministically red under `start_paused`, on every run. The
    /// heartbeat's first tick still fires immediately regardless of the
    /// clock (interval semantics, not this gate), so the real wait below only
    /// has to outlast `MIN_REDRAW`, not `HEARTBEAT`.
    ///
    /// IR-46: bounded by a real, short nudge-then-quit and a real, generous
    /// `timeout` — this cannot hang.
    #[tokio::test]
    async fn a_heartbeat_puts_the_host_strip_on_the_frame() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let (request_tx, _request_rx) = mpsc::channel(2);
        let local = FakeLocal {
            sample: Some(crate::lookout::view::fixtures::sample()),
            ..FakeLocal::default()
        };

        tokio::spawn(async move {
            // The nudge, not the quit: `may_draw` is only re-checked at the
            // top of the next loop iteration, so real time elapsing while
            // the loop sits blocked in `select!` is never observed on its
            // own — see `a_burst_of_selection_moves_costs_one_read_and_not_
            // one_per_key`'s own comment for the same mechanism. `Msg::Resize`
            // wakes the loop once real time has cleared `MIN_REDRAW`, which
            // is when the redraw carrying the already-sampled host actually
            // happens.
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
        let terminal = tokio::time::timeout(
            Duration::from_secs(10),
            run_ui(
                app,
                terminal,
                stream::empty(),
                msg_rx,
                poll_tx,
                request_tx,
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
                local,
            ),
        )
        .await
        .expect("the loop left within ten seconds");

        let frame = crate::lookout::frames::render_text(terminal.backend().buffer());
        assert!(
            frame.contains("host  load 2.31 4.10 3.88 / 10 cores"),
            "the strip drew the sample the heartbeat took: {frame}"
        );
        assert!(
            !frame.contains("not read yet"),
            "and not the pre-heartbeat sentence"
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
    /// **Not `start_paused`.** `MIN_REDRAW`'s gate
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
        let (request_tx, _request_rx) = mpsc::channel(2);
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
            run_ui(
                app,
                terminal,
                stream::empty(),
                msg_rx,
                poll_tx,
                request_tx,
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
                local,
            ),
        )
        .await
        .expect("the loop left within five seconds");

        assert_eq!(
            tails.load(Ordering::Relaxed),
            1,
            "a snapshot and twenty selection moves must coalesce into one read"
        );
    }

    /// fails if a held `j` fires one `Describe` per keypress. Ordinary
    /// terminals deliver auto-repeat as twenty to thirty Press events a
    /// second, and each one moves the selection; without the redraw gate this
    /// feature would be exactly the fixed-clock process-table walk it exists
    /// to avoid, only faster. One request per redraw window, not one per key.
    ///
    /// IR-46: bounded by a real, short sleep and a real, generous timeout.
    #[tokio::test]
    async fn a_burst_of_selection_moves_costs_one_lamb_request() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let (request_tx, mut request_rx) = mpsc::channel(2);
        let local = FakeLocal::default();

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
            // Same nudge-then-quit shape as the feed's own burst test, and
            // the same real-clock caveat: the redraw gate is read once per
            // loop iteration, so real time elapsing while the loop is
            // blocked in `select!` is never observed on its own.
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
            run_ui(
                app,
                terminal,
                stream::empty(),
                msg_rx,
                poll_tx,
                request_tx,
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
                local,
            ),
        )
        .await
        .expect("the loop left within five seconds");

        let mut asked = 0;
        while let Ok(sent) = request_rx.try_recv() {
            assert!(matches!(sent, Sent::Lambs { .. }));
            asked += 1;
        }
        assert_eq!(asked, 1, "twenty moves, one Describe");
    }

    /// fails if a terminal too short to draw the detail pane still pays for
    /// it. `run_ui` knows the height; the reducer does not, and does not need
    /// to.
    ///
    /// IR-46: bounded the same way.
    #[tokio::test]
    async fn no_lambs_are_requested_when_the_detail_pane_is_not_drawn() {
        let (msg_tx, msg_rx) = mpsc::channel(64);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let (request_tx, mut request_rx) = mpsc::channel(2);
        let local = FakeLocal::default();

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
        // The 18-row tier: `view::panes_for(20).detail` is false, so the
        // detail pane is not drawn even though the host strip and feed are.
        let terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            run_ui(
                app,
                terminal,
                stream::empty(),
                msg_rx,
                poll_tx,
                request_tx,
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
                local,
            ),
        )
        .await
        .expect("the loop left within five seconds");

        assert!(
            request_rx.try_recv().is_err(),
            "no lamb request when the detail pane is not drawn"
        );
    }
}
