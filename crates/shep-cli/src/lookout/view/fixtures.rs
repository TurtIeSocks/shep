//! Fixtures the pane test modules share.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::text::Line;
use shep_client::RequestError;
use shep_core::config::AppConfig;
use shep_core::protocol::{BusEvent, DogSource, Lamb, ProcessInfo, Response, SheepConfigView};
use shep_core::status::ProcStatus;

use super::super::app::{
    ActionVerb, App, Control, KeyPress, LambWalk, Msg, RowKey, Sent, SettingsRow,
};
use super::super::source::HostSample;
use super::super::tail::{Stream, Tail, TailLine};
use super::super::theme::Palette;
use crate::commands::settings::{DogView, ScalarView, SettingField, SettingsSnapshot};
use crate::style::StyleSource;

/// No colour at all: the palette every fixture uses unless the test is about
/// colour.
pub fn plain() -> Palette {
    Palette::detect(None, None, None)
}

/// The 256-colour palette, for the two tests that assert on a specific
/// foreground.
pub fn coloured() -> Palette {
    Palette::detect(None, Some(OsStr::new("xterm-256color")), None)
}

/// A dashboard with `flock` listed and nothing else applied.
pub fn app_with(flock: Vec<ProcessInfo>, palette: Palette) -> App {
    let t0 = Instant::now();
    let mut app = App::new(
        palette,
        Control::ReadOnly,
        "/home/ada/.shep".to_string(),
        t0,
    );
    app.update(Msg::Snapshot {
        rows: flock,
        at: t0,
    });
    app
}

/// `count` online sheep, the first `with_readings` of which report cpu and
/// memory. The rest report neither, which is the case the `-` assertions need.
pub fn flock_of(count: u32, with_readings: u32) -> Vec<ProcessInfo> {
    (0..count)
        .map(|id| {
            let reports = id < with_readings;
            ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online)
                .pid(Some(48_000 + id))
                .uptime_ms(4_512_000)
                .cpu_percent(reports.then_some(3.5))
                .memory_bytes(reports.then_some(182 << 20))
                .out_file(Some(format!("/home/ada/.shep/logs/sheep-{id}-out.log")))
                .err_file(Some(format!("/home/ada/.shep/logs/sheep-{id}-err.log")))
                .build()
        })
        .collect()
}

/// One plausible host reading: the same numbers the gallery's scenes use, so
/// a failure here and a frame under review name the same figures.
pub fn sample() -> HostSample {
    HostSample {
        load: (2.31, 4.10, 3.88),
        cores: Some(10),
        memory_total_bytes: 32 << 30,
        memory_used_bytes: 12 * (1 << 30) + (410 << 20),
        uptime_seconds: 6 * 86_400 + 3 * 3_600,
    }
}

/// A dashboard that has had one host sample applied.
pub fn with_host(sample: HostSample, flock: Vec<ProcessInfo>) -> App {
    let mut app = app_with(flock, plain());
    app.update(Msg::Host {
        sample: Some(sample),
    });
    app
}

/// A dashboard with no host reading. The two ways of having none are not the
/// same state: `unsupported: true` applies `Msg::Host { sample: None }`, the
/// signal a `sysinfo` that does not support the platform produces, and the
/// strip says so. `unsupported: false` applies no `Msg::Host` at all, the
/// state before the first heartbeat, and the strip says `not read yet`
/// instead.
pub fn with_host_none(flock: Vec<ProcessInfo>, unsupported: bool) -> App {
    let mut app = app_with(flock, plain());
    if unsupported {
        app.update(Msg::Host { sample: None });
    }
    app
}

/// A dashboard with a flock of three sheep, the first one selected, and
/// `tail` applied as this refresh's feed.
pub fn with_feed(tail: Tail) -> App {
    let mut app = app_with(flock_of(3, 0), plain());
    app.update(Msg::Bleats { tail });
    app
}

/// Like [`with_feed`], but selects sheep `id` first, for the tests that need
/// the header to name a specific sheep.
pub fn with_feed_and_selection(tail: Tail, id: u32) -> App {
    let mut app = app_with(flock_of(3, 0), plain());
    for _ in 0..id {
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    app.update(Msg::Bleats { tail });
    app
}

/// Like [`with_feed`], but with an explicit palette, for the one test that
/// asserts on a specific foreground colour.
pub fn with_feed_and_palette(tail: Tail, palette: Palette) -> App {
    let mut app = app_with(flock_of(3, 0), palette);
    app.update(Msg::Bleats { tail });
    app
}

/// A dashboard with an empty flock: nothing is selected, so the feed's own
/// "no sheep is selected" line is what renders.
pub fn with_no_selection() -> App {
    app_with(Vec::new(), plain())
}

/// A two-sheep dashboard with the selection walked onto `info`, on row 1
/// rather than row 0, so a `selected_row()` that fell back to the first row
/// would be caught. Both properties are asserted below rather than assumed.
pub fn with_selection(info: ProcessInfo) -> App {
    with_selection_and_palette(info, plain())
}

/// The same, at a given palette.
///
/// The decoy is named `!decoy` rather than `decoy`: the table reads by name,
/// and `!` sorts below every ASCII letter and digit, so the decoy is row 0
/// whatever the sheep under test is called.
pub fn with_selection_and_palette(info: ProcessInfo, palette: Palette) -> App {
    assert!(
        info.id > 0,
        "the decoy takes id 0, so the sheep under test cannot"
    );
    let wanted = info.id;
    let decoy = ProcessInfo::builder(0, "!decoy", ProcStatus::Online).build();
    let mut app = app_with(vec![decoy, info], palette);
    app.update(Msg::Key(KeyPress::SelectDown));
    assert_eq!(
        app.selected(),
        Some(RowKey::Sheep(wanted)),
        "the sheep under test must end up selected, and on row 1: the mutation \
         this fixture exists to catch reads row 0 instead"
    );
    app
}

/// A sheep whose listing carries lambs.
///
/// `ListFlock` never populates this field, so this fixture cannot occur live:
/// the pane must not mention lambs even when handed some.
pub fn sheep_with_lambs() -> ProcessInfo {
    ProcessInfo::builder(9, "gateway", ProcStatus::Online)
        .pid(Some(48_301))
        .lambs(Some(vec![
            Lamb::new(48_302, "node"),
            Lamb::new(48_303, "sh"),
        ]))
        .build()
}

/// [`with_selection`] over [`sheep_with_lambs`] (id 9), with one lamb reading
/// applied for that sheep.
pub fn with_lamb_reading(walk: LambWalk) -> App {
    with_lamb_reading_for(9, walk)
}

/// The same, with the reading pinned to `id` instead, so a test can hand the
/// pane a reading that belongs to a different sheep.
pub fn with_lamb_reading_for(id: u32, walk: LambWalk) -> App {
    let mut app = with_selection(sheep_with_lambs());
    app.update(Msg::Replied {
        sent: Sent::Lambs { id },
        result: reply_for(id, &walk),
    });
    app
}

/// [`with_lamb_reading`] plus the `Instant` the dashboard started at, for the
/// one test that needs to tick the clock forward itself.
pub fn app_with_lamb_reading_at(walk: LambWalk) -> (App, Instant) {
    let t0 = Instant::now();
    let mut app = with_selection(sheep_with_lambs());
    app.update(Msg::Tick { now: t0 });
    app.update(Msg::Replied {
        sent: Sent::Lambs { id: 9 },
        result: reply_for(9, &walk),
    });
    (app, t0)
}

/// The reply that makes the reducer record `walk`. There is no way to set a
/// `LambWalk` directly and there should not be: a fixture that reached past
/// `on_lambs` would stop testing the mapping this pane depends on.
///
/// `Failed` is produced by an `Err` rather than by an unrecognised `Ok`,
/// because the two are the same state and `Err` is the one an operator
/// actually meets.
fn reply_for(id: u32, walk: &LambWalk) -> Result<Response, RequestError> {
    let lambs = match walk {
        LambWalk::Failed => return Err(RequestError::Closed),
        LambWalk::NotWalked => None,
        LambWalk::Walked(lambs) => Some(lambs.clone()),
    };
    Ok(Response::Described(vec![
        ProcessInfo::builder(id, "gateway", ProcStatus::Online)
            .pid(Some(48_301))
            .lambs(lambs)
            .build(),
    ]))
}

/// The pane's lamb line alone, for the tests that compare two renderings of
/// it. Panics if the pane has none, so a regression that dropped the line
/// entirely cannot pass by comparing two absences.
pub fn lamb_line_of(app: &App) -> String {
    render_all(&super::detail::detail_lines(app, 200))
        .lines()
        .find(|line| line.starts_with("lambs  "))
        .map(str::to_string)
        .expect("the pane has a lamb line")
}

/// A dashboard with twelve sheep and a full bleats feed, for the checks that
/// need every pane to have more than it can show.
pub fn full_app() -> App {
    let mut app = app_with(flock_of(12, 12), plain());
    app.update(Msg::Bleats {
        tail: Tail {
            lines: (0..10)
                .map(|n| line(Stream::Out, &format!("line-{n}")))
                .collect(),
            missed_lines: 0,
            missed_bytes: 0,
            read_bytes: 1_024,
            note: None,
        },
    });
    app
}

/// One tail line, tagged with the stream it came from.
pub fn line(stream: Stream, text: &str) -> TailLine {
    TailLine {
        stream,
        text: text.to_string(),
    }
}

/// One rendered line, styles discarded.
pub fn rendered(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Several rendered lines, newline-joined. Newline-joined and not
/// concatenated, so an assertion can anchor on a line boundary.
pub fn render_all(lines: &[Line<'static>]) -> String {
    lines.iter().map(rendered).collect::<Vec<_>>().join("\n")
}

/// Four sheep at ids 1..=4 named `web`, `api`, `web-worker`, `cron`, with
/// `query` typed into the filter box and applied. An empty `query` leaves the
/// dashboard unfiltered, which is what the "nothing changed" assertions need.
///
/// Two of the four contain `web`, with `api` between them, so a fixture that
/// stepped over hidden rows would show up as a wrong count rather than as a
/// passing test.
pub fn filtered_app(query: &str) -> App {
    filtered_app_of(named_flock(), query)
}

/// [`filtered_app`] over an explicit flock, for the empty-flock mirror.
pub fn filtered_app_of(flock: Vec<ProcessInfo>, query: &str) -> App {
    let mut app = app_with(flock, plain());
    if !query.is_empty() {
        app.update(Msg::Key(KeyPress::FilterStart));
        for typed in query.chars() {
            app.update(Msg::Key(KeyPress::TextChar(typed)));
        }
        app.update(Msg::Key(KeyPress::TextApply));
    }
    app
}

/// The same four sheep with `query` half-typed and the box still open: no
/// `TextApply`, which is the whole difference from [`filtered_app`].
pub fn editing_app(query: &str) -> App {
    let mut app = app_with(named_flock(), plain());
    app.update(Msg::Key(KeyPress::FilterStart));
    for typed in query.chars() {
        app.update(Msg::Key(KeyPress::TextChar(typed)));
    }
    app
}

/// The four named sheep the filter fixtures share. `flock_of` names its sheep
/// `sheep-0`..`sheep-N`, which every query would match or miss together.
fn named_flock() -> Vec<ProcessInfo> {
    [(1, "web"), (2, "api"), (3, "web-worker"), (4, "cron")]
        .into_iter()
        .map(|(id, name)| {
            ProcessInfo::builder(id, name, ProcStatus::Online)
                .pid(Some(48_000 + id))
                .uptime_ms(4_512_000)
                .build()
        })
        .collect()
}

/// [`filtered_app`]'s four sheep with the gate open and the cursor on `api`
/// at id 2, which is the sheep every action assertion in this file names.
///
/// The cursor is walked to `api` rather than moved a fixed number of rows:
/// the table reads by name, so which row `api` occupies depends on the
/// other sheep's names, not on its id.
pub fn allowed_app() -> App {
    let mut app = app_with(named_flock(), plain());
    app.set_control_for_tests(Control::Allowed);
    for _ in 0..named_flock().len() {
        if app.selected() == Some(RowKey::Sheep(2)) {
            break;
        }
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    assert_eq!(
        app.selected(),
        Some(RowKey::Sheep(2)),
        "the cursor must end up on api"
    );
    app
}

/// [`allowed_app`] with `verb` armed and nothing sent.
pub fn armed_app(verb: ActionVerb) -> App {
    let mut app = allowed_app();
    app.update(Msg::Key(KeyPress::Action(verb)));
    app
}

/// [`armed_app`] confirmed: the request is out and the reply has not landed.
pub fn acting_app(verb: ActionVerb) -> App {
    let mut app = armed_app(verb);
    app.update(Msg::Key(KeyPress::Confirm));
    app
}

/// An armed confirm with a filter applied and a notice standing, so the bar
/// has something in all three slots at once.
///
/// Order matters: the notice must be raised after arming, since `on_key`'s
/// normal branch clears it, and `Msg::Event` never passes through `on_key`.
pub fn armed_app_with_a_filter_and_a_notice() -> App {
    let mut app = filtered_app("api");
    app.set_control_for_tests(Control::Allowed);
    app.update(Msg::Key(KeyPress::Action(ActionVerb::Stop)));
    app.update(Msg::Event(BusEvent::Dropped { count: 3 }));
    app
}

/// A plausible settings snapshot: every scalar rendered as if `shep.toml`
/// declared it, and two candidate dogs, one enabled, for the tests that
/// need a screen with real rows rather than a fresh home's all-default one.
pub fn settings_snapshot() -> SettingsSnapshot {
    let config = |value: &str| ScalarView {
        value: value.to_string(),
        source: StyleSource::Config,
    };
    SettingsSnapshot {
        log_level: config("warn"),
        log_json: config("false"),
        socket: config("/home/ada/.shep/run/shep.sock"),
        max_cron_sleep: config("30s"),
        allow_control: config("false"),
        style_level: config("full"),
        // The document declares it, so the file and the resolved value
        // agree.
        style_level_in_file: Some("full".to_string()),
        dogs: vec![
            DogView {
                name: "bark".to_string(),
                enabled: false,
                adopted_path: None,
            },
            DogView {
                name: "metrics".to_string(),
                enabled: true,
                adopted_path: None,
            },
        ],
    }
}

/// A dashboard with the settings screen already open on
/// [`settings_snapshot`], the gate closed ([`Control::ReadOnly`]).
pub fn app_in_settings() -> App {
    let mut app = app_with(flock_of(3, 0), plain());
    app.update(Msg::Key(KeyPress::Settings));
    app.update(Msg::Settings {
        result: Ok(settings_snapshot()),
    });
    app
}

/// [`app_in_settings_with_control`] with the cursor already moved onto
/// `field`'s row, by real `SelectDown` keypresses rather than poking the
/// cursor index directly.
pub fn app_in_settings_on(field: SettingField) -> App {
    let mut app = app_in_settings_with_control();
    let target = app
        .settings()
        .unwrap()
        .rows()
        .iter()
        .position(|row| *row == SettingsRow::Scalar(field))
        .expect("field is one of the six scalar rows Settings::rows always carries");
    for _ in 0..target {
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    app
}

/// [`app_in_settings_with_control`], but built from a caller-visible `t0`
/// rather than [`Instant::now`] read inside this function: an expiry test
/// needs to hand `Msg::Tick` an instant it can do arithmetic against.
pub fn app_in_settings_at() -> (App, Instant) {
    let t0 = Instant::now();
    let mut app = App::new(plain(), Control::Allowed, "/home/ada/.shep".to_string(), t0);
    app.update(Msg::Key(KeyPress::Settings));
    app.update(Msg::Settings {
        result: Ok(settings_snapshot()),
    });
    (app, t0)
}

/// The settings screen with `[style] level` SHADOWED: the document says
/// `full`, `source` is the layer that outranked it, and the level in force
/// is `bare`. The cursor sits on the style row, moved there by real
/// keypresses the same way [`app_in_settings_on`] moves it.
///
/// The one state where a scalar's value in force and its value on disk
/// disagree. Every other field's layers belong to the shepherd's process,
/// where lookout can see neither.
pub fn app_in_settings_with_shadowed_style(source: StyleSource) -> App {
    let mut app = app_in_settings_on(SettingField::StyleLevel);
    let mut snapshot = settings_snapshot();
    snapshot.style_level = ScalarView {
        value: "bare".to_string(),
        source,
    };
    snapshot.style_level_in_file = Some("full".to_string());
    app.update(Msg::Settings {
        result: Ok(snapshot),
    });
    app
}

/// [`app_in_settings`] with the control gate open, for the one test that
/// proves an action key stays unreachable even when actions would otherwise
/// be permitted.
pub fn app_in_settings_with_control() -> App {
    let mut app = app_in_settings();
    app.set_control_for_tests(Control::Allowed);
    app
}

/// [`settings_snapshot`]'s own scalars, with `dogs` replaced, for the
/// dogs-table tests, which need particular names, `enabled` bits, and a
/// matching or mismatching flock.
fn settings_snapshot_with_dogs(dogs: Vec<DogView>) -> SettingsSnapshot {
    SettingsSnapshot {
        dogs,
        ..settings_snapshot()
    }
}

/// `otel` runs online while the file disables it: a removed name still
/// running. `ledger` is enabled in the file and absent from the flock: a dog
/// that failed to start. Exercises [`super::settings::dog_rows`]'s join, not
/// the toggle.
pub fn app_in_settings_with_dog_drift() -> App {
    let flock = vec![
        ProcessInfo::builder(90, "otel", ProcStatus::Online)
            .pid(Some(90_000))
            .dog(Some(DogSource::BuiltIn))
            .build(),
    ];
    let mut app = app_with(flock, plain());
    app.update(Msg::Key(KeyPress::Settings));
    app.update(Msg::Settings {
        result: Ok(settings_snapshot_with_dogs(vec![
            // Real paths, not `None`: `otel` and `ledger` are adopted dogs,
            // and every value in `[daemon] adopted_dogs` is a path.
            DogView {
                name: "otel".to_string(),
                enabled: false,
                adopted_path: Some(PathBuf::from("/usr/local/bin/shep-otel")),
            },
            DogView {
                name: "ledger".to_string(),
                enabled: true,
                adopted_path: Some(PathBuf::from("/opt/ledger/bin/dog")),
            },
        ])),
    });
    app
}

/// `bark` is up but has never completed a handshake
/// (`handshook: Some(false)`), so [`super::settings::dog_rows`] must read it
/// `silent`, not `online`, the same correction [`crate::vocabulary::Reported`]
/// makes for the flock table.
pub fn app_in_settings_with_silent_dog() -> App {
    let flock = vec![
        ProcessInfo::builder(91, "bark", ProcStatus::Online)
            .pid(Some(91_000))
            .dog(Some(DogSource::BuiltIn))
            .handshook(Some(false))
            .build(),
    ];
    let mut app = app_with(flock, plain());
    app.update(Msg::Key(KeyPress::Settings));
    app.update(Msg::Settings {
        result: Ok(settings_snapshot_with_dogs(vec![DogView {
            name: "bark".to_string(),
            enabled: true,
            adopted_path: None,
        }])),
    });
    app
}

/// Two candidate dogs for the toggle tests: `metrics` disabled, `otel`
/// enabled, so a test can pick whichever direction it means to arm.
fn settings_snapshot_for_toggle_tests() -> SettingsSnapshot {
    settings_snapshot_with_dogs(vec![
        DogView {
            name: "metrics".to_string(),
            enabled: false,
            adopted_path: None,
        },
        DogView {
            name: "otel".to_string(),
            enabled: true,
            adopted_path: Some(PathBuf::from("/usr/local/bin/shep-otel")),
        },
    ])
}

/// A dashboard with the settings screen open on
/// [`settings_snapshot_for_toggle_tests`], the control gate open, and the
/// cursor moved onto `name`'s dog row by real `SelectDown` keypresses.
///
/// The six scalar rows always sort first in `Settings::rows`, so the dog at
/// index `i` of [`settings_snapshot_for_toggle_tests`]'s own list sits at
/// row `6 + i`.
pub fn app_in_settings_on_dog(name: &str) -> App {
    let mut app = app_with(flock_of(3, 0), plain());
    app.set_control_for_tests(Control::Allowed);
    app.update(Msg::Key(KeyPress::Settings));
    app.update(Msg::Settings {
        result: Ok(settings_snapshot_for_toggle_tests()),
    });
    let dog_index = settings_snapshot_for_toggle_tests()
        .dogs
        .iter()
        .position(|dog| dog.name == name)
        .expect("name is one of settings_snapshot_for_toggle_tests's own dogs");
    for _ in 0..(6 + dog_index) {
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    app
}

/// [`app_in_settings_on_dog`], named for the test that means to start on an
/// already-enabled dog: same fixture, same mechanism, a name that reads
/// what the test is asserting on without checking the dogs list.
pub fn app_in_settings_on_enabled_dog(name: &str) -> App {
    app_in_settings_on_dog(name)
}

/// One sheep's config as the shepherd would answer it: `web`, with two
/// fields an operator has overridden, one parked until a respawn, and two
/// env keys whose values the view never carries.
pub fn sheep_config_view() -> SheepConfigView {
    let mut config = AppConfig {
        name: "web".to_string(),
        script: "./srv".to_string(),
        args: vec!["--port".to_string(), "8080".to_string()],
        max_restarts: 32,
        instances: 3,
        ..AppConfig::default()
    };
    config
        .env
        .insert("DB_HOST".to_string(), "db.internal".to_string());
    config
        .env
        .insert("LOG_LEVEL".to_string(), "debug".to_string());
    SheepConfigView::new(
        config,
        vec!["max_restarts".to_string(), "instances".to_string()],
        vec!["kill_signal".to_string()],
    )
}

/// The bark dog's `[bark]` section as `Request::DogConfig` would answer it:
/// a comment, two scalars, and a sink holding a webhook credential.
///
/// The comment is load-bearing rather than decoration: a write goes out as
/// the WHOLE section, so a pane that re-rendered it from the parsed values
/// would delete this line on the operator's own keystroke.
pub fn dog_section() -> String {
    "# how often\npoll = \"60s\"\nhistory_bytes = 4096\n\n[sinks.ops]\nkind = \"slack\"\nurl = \"https://hooks.example/x\"\n"
        .to_string()
}

/// A dashboard with the bark dog's config pane open, opened the way the
/// event loop opens it: `e` on the settings screen's own dog row, then the
/// schema its binary answered with, then the shepherd's section. The
/// control gate is open, so the pane can write.
///
/// bark is [`settings_snapshot`]'s first dog, and the six scalar rows
/// always sort ahead of the dogs, so row 6 is its row.
pub fn app_in_dog_pane() -> App {
    let mut app = app_with(flock_of(3, 0), plain());
    app.set_control_for_tests(Control::Allowed);
    app.update(Msg::Key(KeyPress::Settings));
    app.update(Msg::Settings {
        result: Ok(settings_snapshot()),
    });
    for _ in 0..6 {
        app.update(Msg::Key(KeyPress::SelectDown));
    }
    app.update(Msg::Key(KeyPress::Edit));
    app.update(Msg::DogPane {
        name: "bark".to_string(),
        adopted_path: None,
        result: Ok(crate::dog::builtin_schema("bark").expect("bark is a built-in")),
    });
    app.update(Msg::Replied {
        sent: Sent::DogSection {
            name: "bark".to_string(),
        },
        result: Ok(Response::DogSection {
            toml: dog_section().into(),
        }),
    });
    app
}

/// [`app_in_sheep_pane`] with the control gate open: the pane can write.
///
/// The gate is set BEFORE the pane opens, so nothing about how it opened
/// depends on it, which is what makes a read-only refusal and a permitted
/// write comparable frames.
pub fn app_in_sheep_pane_with_control() -> App {
    let mut app = with_selection(
        ProcessInfo::builder(9, "web", ProcStatus::Online)
            .pid(Some(48_000))
            .build(),
    );
    app.set_control_for_tests(Control::Allowed);
    app.update(Msg::Key(KeyPress::Edit));
    app.update(Msg::Replied {
        sent: Sent::SheepConfig {
            name: "web".to_string(),
        },
        result: Ok(Response::SheepConfig(Box::new(sheep_config_view()))),
    });
    app
}

/// A dashboard with `web` selected and its config pane open, opened the way
/// the event loop opens it: `e`, then the shepherd's own reply.
pub fn app_in_sheep_pane() -> App {
    let mut app = with_selection(
        ProcessInfo::builder(9, "web", ProcStatus::Online)
            .pid(Some(48_000))
            .build(),
    );
    app.update(Msg::Key(KeyPress::Edit));
    app.update(Msg::Replied {
        sent: Sent::SheepConfig {
            name: "web".to_string(),
        },
        result: Ok(Response::SheepConfig(Box::new(sheep_config_view()))),
    });
    app
}
