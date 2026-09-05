//! `shep lookout` (alias `dash`): the terminal dashboard.
//!
//! Four panes, one screen: the flock table, a sheep detail pane
//! ([`view::detail`]) and a bleats feed ([`view::bleats`]) under the selected
//! row, and a host-usage strip ([`view::host`]) above. A narrow or short
//! terminal drops panes before columns; [`view::panes_for`] is the tier table.
//!
//! Two tasks: the link task ([`link::run_link`]) owns the connection, the UI
//! loop ([`run_ui`]) owns the screen, and they talk over an `mpsc` each way.
//! Neither borrows the other. A dead shepherd freezes the dashboard rather
//! than ending it; see [`link::RECONNECT_ATTEMPTS`].

pub mod app;
pub mod field;
// `#[cfg(test)]`: every item in `frames` is read by tests and by the gallery
// writer, and by nothing else. `pub` exempts nothing from `dead_code` here,
// since `mod lookout` in `lib.rs` is private rather than `pub mod`.
#[cfg(test)]
pub mod frames;
pub mod input;
pub mod link;
pub mod pane;
pub mod source;
pub mod tail;
pub mod term;
pub mod theme;
pub mod view;
pub mod viewport;

use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use futures_util::{Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use shep_core::paths::ShepPaths;
use tokio::sync::mpsc;

use self::app::{App, Control, Effect, Msg, RowKey, Sent};
use self::source::Shepherd;
use self::theme::Palette;
use crate::cli::LookoutArgs;
use crate::exit::ExitCode;
use crate::output::Streams;
use crate::style::{StyleLevel, StyleSource};

/// How often the uptime column is re-derived.
///
/// One second. Nothing on the wire changes on this tick: it exists so a
/// running sheep's UPTIME advances between the two-second polls instead of
/// stepping.
pub const HEARTBEAT: Duration = Duration::from_secs(1);

/// The floor on the gap between two draws.
///
/// ~30 frames a second. A `shep muster` of a large flock emits a `process.*`
/// event per sheep, and this makes a burst of N events cost one draw per 33ms
/// rather than N draws. Armed only while something is dirty, so an idle
/// dashboard draws nothing at all.
pub const MIN_REDRAW: Duration = Duration::from_millis(33);

/// Runs the dashboard, and returns the [`ExitCode`] to exit with.
///
/// Four refusals, all before a single escape byte is written:
/// [`ExitCode::Usage`] when stdout is not a terminal;
/// [`ExitCode::DaemonUnreachable`] or [`ExitCode::ProtocolMismatch`] when the
/// first connection fails; [`ExitCode::VersionSkew`] when it succeeds against
/// a different crate version; [`ExitCode::Failure`] when the terminal cannot
/// be put into raw mode. After that it never exits on its own.
///
/// `style` is `run_argv`'s already-resolved pair, handed to `App::set_style`
/// so the settings screen reports the layer that actually won.
pub async fn lookout(
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    args: &LookoutArgs,
    style: (StyleLevel, StyleSource),
) -> ExitCode {
    // A TUI piped into a file is a usage error, not a rendering mode: the
    // alternative is writing alternate-screen escapes into somebody's log.
    if !std::io::stdout().is_terminal() {
        return streams.fail(
            ExitCode::Usage,
            "lookout needs a terminal; stdout is not one",
        );
    }

    // The first dial, before the palette, the panic hook, raw mode, or
    // anything drawn: a shepherd that was never running gets the same refusal
    // `shep flock` gets. Everything after this point is the running-dashboard
    // case, which is the one the ladder is for.
    let mut shepherd = source::UnixShepherd::new(&paths.socket);
    let opened = match shepherd.link().await {
        Ok(opened) => opened,
        Err(err) => {
            let code = err.exit_code();
            return streams.fail(code, &err.to_string());
        }
    };

    // `lookout` drives the daemon for as long as the dashboard stays open, so
    // it can never be one of `RECOVERY_VERBS`. A reconnect on the ladder is
    // not re-checked; a shepherd cannot downgrade itself mid-run.
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

    // Hook first, then the guard, then raw mode, then the alternate screen,
    // with nothing that can panic in between.
    term::install_panic_hook();
    // Armed before `enter()`: `enter` turns raw mode on and then enters the
    // alternate screen, so a failure in the second step would otherwise leave
    // the operator's shell with no echo and no line editing. `restore()` is
    // idempotent and safe outside raw mode.
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
        paths.home.clone(),
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
/// The flag wins. The store is `$SHEP_HOME/kv.json`
/// (`shep set lookout.allow_control true`) rather than a `shep.toml` section,
/// because this gate is the operator's own: lookout runs as the operator, and
/// the shepherd cannot tell one of its keypresses from a `shep stop`.
///
/// A store that cannot be read is read as "no": failing open would drop the
/// gate exactly when something is wrong with the machine.
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
/// Generic over the backend and the key source, so a test drives it with a
/// `TestBackend` and a finite `Stream`, and gets the terminal back.
///
/// Five `biased` arms: `SIGTERM`, the keyboard, the link, the settings
/// screen's finished file I/O, the heartbeat. An exhausted source is `Ready`
/// forever, so every arm above the heartbeat is disabled once it runs dry;
/// arm 4's is live, since an empty `FuturesUnordered` fills again. The redraw
/// runs after the `select!`, gated on `dirty` and [`MIN_REDRAW`]; the feed
/// and the lamb fetch ride that same gate. Nine arguments, hence the
/// `#[allow]`.
#[allow(clippy::too_many_arguments)]
pub async fn run_ui<B: Backend, S, L>(
    mut app: App,
    mut terminal: Terminal<B>,
    events: S,
    mut msgs: mpsc::Receiver<Msg>,
    polls: mpsc::Sender<()>,
    requests: mpsc::Sender<self::app::Sent>,
    // `$SHEP_HOME` as this invocation resolved it. Only `Effect::LoadDogPane`
    // reads it: `commands::dogs::ask` sets `SHEP_HOME` for the probed
    // candidate, so a home other than `--home`'s could point a schema probe
    // at the live daemon's socket instead.
    home: std::path::PathBuf,
    // The settings screen's own read target. Two owned `PathBuf`s rather than
    // `&ShepPaths`, so a test can hand this loop an arbitrary pair, and cloned
    // into each `spawn_blocking` closure that outlives this stack frame.
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

    // Set once each, when their source runs dry.
    let mut keys_done = false;
    let mut link_done = false;

    let mut dirty = true;
    // Set by `Effect::RefreshFeed`, cleared once the coalesced read has run.
    let mut feed_dirty = false;
    // Set by `Effect::RefreshSelected` and by `Effect::PollNow`, cleared once
    // the coalesced request below has gone out.
    let mut lambs_dirty = false;
    // The settings screen's file I/O, in flight. Each entry resolves to the
    // `Msg` its result belongs in. A set, not a slot: a second write can be
    // raised while the first still runs. Dropping the set cancels nothing, so
    // a quit leaves no half-written file.
    let mut inflight: FuturesUnordered<BoxFuture<'static, Msg>> = FuturesUnordered::new();
    // `Option`, not `Instant::now() - MIN_REDRAW`: subtracting from a fresh
    // `Instant` can panic, and "has never drawn" is what this means.
    let mut last_draw: Option<Instant> = None;

    loop {
        // One gate, read once, so the feed is refreshed before the frame that
        // shows it and never on a frame that is not about to be drawn.
        let may_draw = last_draw.is_none_or(|at| at.elapsed() >= MIN_REDRAW);
        if feed_dirty && may_draw {
            // Nothing selected means an empty flock, and the pane's header
            // already says so. `tail::read`'s `(None, None)` early return is
            // for a different case, a selected sheep whose shepherd predates
            // the `out_file`/`err_file` fields.
            let tail = match app.selected_row() {
                None => tail::Tail::default(),
                Some(row) => {
                    // Cloned out before `app` is borrowed mutably.
                    let (out, err) = (row.info.out_file.clone(), row.info.err_file.clone());
                    local.tail(out.as_deref().map(Path::new), err.as_deref().map(Path::new))
                }
            };
            // `let _`: `Msg::Bleats` returns `Effect::None` by construction,
            // and acting on an effect here is where this could recurse.
            let _ = app.update(Msg::Bleats { tail });
            feed_dirty = false;
            dirty = true;
        }
        if lambs_dirty && may_draw {
            // Read here and not in the reducer: `run_ui` knows the terminal
            // and `App` does not, and a terminal too short to draw the detail
            // pane must not pay for a process-table walk it cannot show. A
            // size that cannot be read counts as too short.
            let height = terminal.size().map_or(0, |size| size.height);
            if view::panes_for(height).detail
                && let Some(RowKey::Sheep(id)) = app.selected()
            {
                // `try_send`, for `Effect::PollNow`'s reason: a full channel
                // means a request is already queued, and a dropped lamb fetch
                // reads as "not read yet".
                let _ = requests.try_send(Sent::Lambs { id });
            }
            lambs_dirty = false;
        }
        if dirty && may_draw {
            // Told before the draw it is about to feed, not after: a
            // scrolled screen's cursor must never land on a row the
            // terminal that size implies could not have shown.
            if let Ok(size) = terminal.size() {
                let area = Rect::new(0, 0, size.width, size.height);
                app.note_body_rows(view::body_rows(area));
            }
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
                    // complete, rather than spinning the loop.
                    None => std::future::pending().await,
                }
            } => break,
            event = events.next(), if !keys_done => match event {
                Some(Ok(crossterm::event::Event::Resize(..))) => Some(Msg::Resize),
                Some(Ok(event)) => input::map_key(&event, app.mode()).map(Msg::Key),
                // A key source that has ended, or has started erroring. Both
                // conditions are permanent, and both retire this arm: one
                // that keeps completing immediately, above the link and the
                // heartbeat, freezes the display and spins the process.
                Some(Err(_)) | None => {
                    keys_done = true;
                    None
                }
            },
            msg = msgs.recv(), if !link_done => match msg {
                Some(msg) => Some(msg),
                // Every sender dropped: the link task ended without freezing,
                // which only happens if it was aborted. Keep the last frame
                // up, and retire this arm.
                None => {
                    link_done = true;
                    None
                }
            },
            // `FuturesUnordered::next` is cancel-safe: the futures live in
            // the set, not in the future this arm polls, so losing a
            // `select!` race loses no progress. The precondition is not
            // optional: an empty set is `Ready(None)` on every poll.
            done = inflight.next(), if !inflight.is_empty() => done,
            _ = heartbeat.tick() => {
                // The host sample rides this arm rather than adding one of
                // its own: memory and a load average cost microseconds and no
                // process-table walk. Sampled unconditionally and refused by
                // the reducer once the link is lost, one enforcement point.
                let _ = app.update(Msg::Host { sample: local.host() });
                Some(Msg::Tick { now: Instant::now() })
            }
        };

        // Nothing to apply, and not a spin risk: an unbound keypress needs a
        // fresh keystroke, and each source retires once, ever.
        let Some(msg) = msg else { continue };

        match app.update(msg) {
            Effect::Quit => break,
            Effect::PollNow => {
                // `try_send`, not `send`: a full poll channel means a repair
                // is already queued, and blocking the UI on it would stall the
                // screen. A closed one means the link ended, which the reducer
                // handles by refusing `r` once the link is `Lost`.
                let _ = polls.try_send(());
                // `r` means "tell me again", so it refreshes the panes too.
                lambs_dirty = true;
                dirty = true;
            }
            // Not the read. A held `j` reaches an ordinary terminal as twenty
            // to thirty Press events a second, so a synchronous 128 KiB read
            // and a `Describe` here would sit behind every repeat, on the task
            // that also owns the redraw. Coalesced onto `MIN_REDRAW` instead.
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
                // would stall the screen. A failure goes back to the reducer,
                // which is already showing an in-flight line about it.
                if let Err(err) = requests.try_send(sent) {
                    let (mpsc::error::TrySendError::Full(sent)
                    | mpsc::error::TrySendError::Closed(sent)) = err;
                    // `let _`: `Msg::Unsent` returns `Effect::None` by
                    // construction.
                    let _ = app.update(Msg::Unsent { sent });
                }
                dirty = true;
            }
            // Off this task: `spawn_blocking` even though the read takes no
            // lock, and pushed into `inflight` rather than awaited, as
            // `Effect::WriteSetting` does. The style is `app.style()`, already
            // resolved by `run_argv`, so the STYLE LEVEL row agrees.
            Effect::LoadSettings => {
                let path = daemon_config.clone();
                let socket_default = socket_default.clone();
                let style = app.style();
                let handle = tokio::task::spawn_blocking(move || {
                    crate::commands::settings::load_settings(&path, &socket_default, style)
                });
                // Pushed, not awaited. The `Msg` is built inside the wrapper
                // so the arm that drains `inflight` stays one line.
                inflight.push(Box::pin(async move {
                    let result = handle
                        .await
                        .map_err(|err| err.to_string())
                        .and_then(|inner| inner.map_err(|err| err.to_string()));
                    Msg::Settings { result }
                }));
                dirty = true;
            }
            // `apply_setting` takes `ShepToml::try_edit`'s lock, which blocks
            // with no deadline, so the handle goes into `inflight` rather than
            // being awaited here. `_authority` is a proof carried by the
            // effect, not a value this arm reads.
            Effect::WriteSetting(edit, _authority) => {
                let path = daemon_config.clone();
                let for_msg = edit.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    crate::commands::settings::apply_setting(&path, &edit)
                });
                inflight.push(Box::pin(async move {
                    let result = handle
                        .await
                        .map_err(|err| err.to_string())
                        .and_then(|inner| inner.map_err(|err| err.to_string()));
                    Msg::SettingWritten {
                        edit: for_msg,
                        result,
                    }
                }));
                dirty = true;
            }
            // Off this task: an adopted dog's schema probe spawns its own
            // binary and can block up to `VERSION_BUDGET`, which would
            // freeze the redraw and bus drain if awaited inline. `Silent`
            // and `Unreadable` both mean no pane here.
            Effect::LoadDogPane { name, adopted_path } => {
                let home = home.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    let schema = match (crate::dog::builtin_schema(&name), adopted_path.as_deref())
                    {
                        (Some(schema), _) => Some(schema),
                        (None, Some(path)) => {
                            match crate::commands::dogs::ask_schema(
                                path,
                                &home,
                                &name,
                                crate::commands::dogs::VERSION_BUDGET,
                            ) {
                                crate::commands::dogs::DogSchema::Published(schema) => Some(schema),
                                crate::commands::dogs::DogSchema::Silent
                                | crate::commands::dogs::DogSchema::Unreadable => None,
                            }
                        }
                        (None, None) => None,
                    };
                    let result = schema.ok_or_else(|| {
                        format!("{name} publishes no schema; edit dogs.toml with $EDITOR")
                    });
                    Msg::DogPane {
                        name,
                        adopted_path,
                        result,
                    }
                });
                inflight.push(Box::pin(async move {
                    // A `spawn_blocking` that panicked is the one case this
                    // has no `Msg` for, and it is reported as the same
                    // refusal rather than dropped: a keystroke that produces
                    // silence reads as a key that is not bound.
                    handle.await.unwrap_or_else(|err| Msg::DogPane {
                        name: String::new(),
                        adopted_path: None,
                        result: Err(format!("the schema probe failed: {err}")),
                    })
                }));
                dirty = true;
            }
            // `dogs::enable_in_config`/`dogs::disable_in_config` take
            // `ShepToml`'s own lock, which blocks with no deadline, so this
            // arm does not wait for it either. `_authority` is dropped as in
            // `Effect::WriteSetting`.
            Effect::WriteDog(edit, _authority) => {
                let path = daemon_config.clone();
                let for_msg = edit.clone();
                let handle = tokio::task::spawn_blocking(move || {
                    if edit.enable {
                        crate::commands::dogs::enable_in_config(&path, &edit.name)
                            .map_err(|err| enable_refusal_message(&err, &edit.name))
                    } else {
                        crate::commands::dogs::disable_in_config(&path, &edit.name)
                            .map_err(|err| err.to_string())
                    }
                });
                // `Msg::DogWritten` answers with `Effect::Send` on the `Ok`
                // arm, and reaches this loop through `inflight` like any other
                // message, so the daemon half of a dog toggle goes out exactly
                // as an ordinary key's `Effect::Send` does.
                inflight.push(Box::pin(async move {
                    let result = handle
                        .await
                        .map_err(|err| err.to_string())
                        .and_then(|inner| inner);
                    Msg::DogWritten {
                        edit: for_msg,
                        result,
                    }
                }));
                dirty = true;
            }
            Effect::None => dirty = true,
        }
    }

    terminal
}

/// Renders [`crate::commands::dogs::EnableRefusal`] for the settings screen's
/// own notice line.
///
/// `name` is this function's own argument, since `EnableRefusal::UnknownDog`
/// does not carry one, so the wrong name never lands in a sentence about
/// someone else's dog.
fn enable_refusal_message(err: &crate::commands::dogs::EnableRefusal, name: &str) -> String {
    use crate::commands::dogs::EnableRefusal;
    match err {
        EnableRefusal::Config(err) => err.to_string(),
        EnableRefusal::UnknownDog { .. } => {
            format!("{name} is not a dog shep knows about")
        }
    }
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
    /// count of each call. `Arc`, since `run_ui` takes the reader by value.
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

    /// The whole loop with no terminal and no socket: a `TestBackend` for the
    /// screen and a finite `Stream` for the keyboard. Bounded, so a loop that
    /// never sees its quit key fails rather than hangs the suite.
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
                PathBuf::from("/tmp/shep-lookout-tests"),
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

    /// The `Effect::PollNow` a drop produces has to reach the link task, or
    /// the repair the link task exists for never happens.
    ///
    /// Also the starvation pin: `stream::empty()` is `Poll::Ready(None)` on
    /// every poll, so an implementation that did not retire the keyboard arm
    /// would win that arm forever and never read either message queued below.
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
                PathBuf::from("/tmp/shep-lookout-tests"),
                PathBuf::from("/tmp/shep-lookout-tests/shep.toml"),
                PathBuf::from("/tmp/shep-lookout-tests/run/shep.sock"),
                FakeLocal::default(),
            ),
        )
        .await
        .expect("the loop left within ten seconds");

        assert_eq!(poll_rx.try_recv(), Ok(()), "the poll request was forwarded");
    }

    /// The flag has to win, and the store has to work: `shep set
    /// lookout.allow_control true` is why there is no config section for it.
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

    /// The strip renders from `App::host`, and every pane test and gallery
    /// frame injects the message directly, so a heartbeat that yielded only
    /// `Msg::Tick` would leave the shipped binary drawing `host  not read
    /// yet` with nothing red on the suite. Asserted on the reader rather than
    /// on a frame; `a_heartbeat_puts_the_host_strip_on_the_frame` is the
    /// other half.
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
                PathBuf::from("/tmp/shep-lookout-tests"),
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

    /// The end-to-end half of
    /// `the_heartbeat_asks_the_local_reader_for_a_host_sample`: a `Local` that
    /// reports a sample, one heartbeat, and the numbers on the rendered frame.
    ///
    /// Not `start_paused`: the redraw that carries the sample is gated on
    /// `MIN_REDRAW`, which reads real `std::time::Instant`, and a paused
    /// clock's virtual sleeps resolve in microseconds of real time, so the
    /// gate would never open. The heartbeat's first tick fires regardless of
    /// the clock, so the real wait below need only outlast `MIN_REDRAW`.
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
            // top of the next iteration, so real time elapsing while the loop
            // sits blocked in `select!` is never observed on its own.
            // `Msg::Resize` wakes it once real time has cleared `MIN_REDRAW`.
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
                PathBuf::from("/tmp/shep-lookout-tests"),
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

    /// `input::map_key` drops `KeyEventKind::Repeat`, but ordinary terminals
    /// deliver auto-repeat as Press events, so a held `j` is twenty to thirty
    /// moved selections a second and an uncoalesced `Effect::RefreshFeed`
    /// would put a synchronous 128 KiB read behind every one of them, on the
    /// task that also owns the redraw.
    ///
    /// `assert_eq!(1)` and not `<= 2`: the exact number is the property.
    ///
    /// Not `start_paused`, for the reason
    /// `a_heartbeat_puts_the_host_strip_on_the_frame` gives: `MIN_REDRAW`
    /// reads a real [`std::time::Instant`], which a virtual clock never
    /// advances.
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
            // iteration, right before the blocking receive, so real time
            // elapsing while the loop waits is only seen on the next one.
            // `Msg::Resize` wakes it after `MIN_REDRAW` has cleared.
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
                PathBuf::from("/tmp/shep-lookout-tests"),
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

    /// Ordinary terminals deliver auto-repeat as twenty to thirty Press
    /// events a second, each moving the selection; without the redraw gate
    /// this would be the fixed-clock process-table walk it exists to avoid,
    /// only faster. One request per redraw window, not one per key.
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
            // The same nudge-then-quit shape as the feed's own burst test:
            // the redraw gate is read once per loop iteration.
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
                PathBuf::from("/tmp/shep-lookout-tests"),
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

    /// `run_ui` knows the height; the reducer does not, and does not need to.
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
                PathBuf::from("/tmp/shep-lookout-tests"),
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

    /// The property, not the mechanism: with a write in flight and unable to
    /// finish, the loop still processes what comes after it.
    ///
    /// Held up by taking `shep.toml`'s own lock from a second file descriptor.
    /// `flock(2)` excludes per open file description, not per process, so
    /// `ShepToml::try_edit` blocks with no deadline and nothing to wake it.
    /// `q` after the confirm is what has to keep working. Unix only: the
    /// Windows arm of `ConfigLock` polls a `share_mode(0)` open instead.
    ///
    /// The clock is the real one: a paused clock auto-advances only while the
    /// runtime is idle, and a thread parked on a lock is not idle, so the
    /// timeout would never fire.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_loop_keeps_running_while_a_settings_write_is_stuck() {
        use nix::fcntl::{Flock, FlockArg};

        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("shep.toml");
        std::fs::write(&config, "[daemon]\nlog_level = \"info\"\n").unwrap();
        let socket_default = dir.path().join("run/shep.sock");

        // Held for the whole of `run_ui` below, and released only once it
        // has returned.
        let lock_file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.path().join("shep.toml.lock"))
            .unwrap();
        let held = Flock::lock(lock_file, FlockArg::LockExclusive).unwrap();

        let (msg_tx, msg_rx) = mpsc::channel(16);
        let (poll_tx, _poll_rx) = mpsc::channel(4);
        let (request_tx, _request_rx) = mpsc::channel(4);

        // The screen is opened by handing the reducer the read it would
        // otherwise have asked for, so the four messages below arrive in a
        // fixed order.
        let snapshot = crate::commands::settings::load_settings(
            &config,
            &socket_default,
            (StyleLevel::Full, StyleSource::Default),
        )
        .unwrap();
        msg_tx
            .send(Msg::Settings {
                result: Ok(snapshot),
            })
            .await
            .unwrap();
        // `space` arms the first row (`[daemon] log_level`), `Enter` sends
        // it, and `q` is the key that must still be answered.
        msg_tx.send(Msg::Key(KeyPress::Cycle)).await.unwrap();
        msg_tx.send(Msg::Key(KeyPress::Confirm)).await.unwrap();
        msg_tx.send(Msg::Key(KeyPress::Quit)).await.unwrap();
        drop(msg_tx);

        let app = App::new(
            Palette::detect(None, None, None),
            Control::Allowed,
            dir.path().display().to_string(),
            Instant::now(),
        );
        let terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            run_ui(
                app,
                terminal,
                stream::empty(),
                msg_rx,
                poll_tx,
                request_tx,
                dir.path().to_path_buf(),
                config.clone(),
                socket_default,
                FakeLocal::default(),
            ),
        )
        .await
        .expect("the loop answered `q` with a write still in flight");

        // The other half of the same property: the write was not cancelled by
        // the loop leaving. `spawn_blocking` runs its closure to completion
        // whatever happens to the handle, so releasing the lock here lets it
        // land. Polled, with a ceiling so a failure reports rather than hangs.
        drop(held);
        let mut written = false;
        for _ in 0..500 {
            if std::fs::read_to_string(&config).unwrap().contains("debug") {
                written = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(written, "the write that was in flight still landed");
    }
}
