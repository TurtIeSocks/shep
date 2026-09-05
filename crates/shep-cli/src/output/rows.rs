//! Every rendered payload type in the binary, and the [`Render`] impl that
//! makes each one's table and JSON renderings one source of truth.
//!
//! They live here rather than under `commands/` because nothing here carries
//! a `cfg`, so a test on the Windows leg can name every one.

use std::collections::BTreeMap;

use serde::Serialize;
use shep_core::barks::{Bark, SinkOutcome};
use shep_core::protocol::{
    ActionOutcome, ActionReply, DogSource, ExitInfo, Lamb, LineOutcome, LineReply, ProcessInfo,
    SignalOutcome, SignalReply,
};
use shep_core::status::ProcStatus;

use crate::dog_index::AvailableDog;
use crate::style::Presentation;
use crate::vocabulary::{Reported, Role};

use super::Render;

/// `Vec<ProcessInfo>` for every verb whose reply carries one: `flock`,
/// `describe`, `fold`, `start`, `stop`, `restart`, `reopen`, `flush`.
///
/// A newtype for the orphan rule; `transparent`, so the JSON is a plain
/// array of `ProcessInfo`.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct FlockRows(pub Vec<ProcessInfo>);

impl Render for FlockRows {
    fn headers() -> &'static [&'static str] {
        &[
            "ID", "NAME", "STATUS", "PID", "RESTARTS", "EXIT", "CFG", "CPU", "MEM", "UPTIME",
            "FOLD", "SMIT",
        ]
    }

    /// One row per process, and the path `Bare` takes: `table_of` calls this
    /// directly when [`crate::style::StyleLevel::boxes`] is false, so the
    /// `web:0` suffix lives in [`plain_row`] as well as in
    /// [`Self::rows_for`].
    fn rows(&self) -> Vec<Vec<String>> {
        name_groups(&self.0)
            .flat_map(|group| {
                let slotted = group.len() > 1 && group.iter().all(|p| p.instance.is_some());
                group.iter().map(move |p| plain_row(p, slotted))
            })
            .collect()
    }

    /// [`Self::rows`], each cell painted by [`process_info_paint`]'s rule for
    /// its column, or [`group_paint`]'s for a header row.
    ///
    /// An app with several instances groups under one header row when
    /// [`crate::style::StyleLevel::boxes`] is true. `sort_flock` orders the
    /// listing by (name, instance, id), so instances are adjacent and one
    /// pass groups them. `Bare` never reaches this method; see [`Self::rows`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        /// Which payload a rendered row came from, for [`paint`]'s rule.
        enum RowSource<'a> {
            /// A slot row or a plain (ungrouped) row.
            Sheep(&'a ProcessInfo),
            /// A group's header row, plus its summed totals.
            Group(&'a [ProcessInfo], GroupTotals),
        }

        let mut out = Vec::with_capacity(self.0.len());
        let mut sources: Vec<RowSource<'_>> = Vec::with_capacity(self.0.len());
        for group in name_groups(&self.0) {
            // A slot nobody reported cannot be grouped or suffixed.
            let slotted = group.len() > 1 && group.iter().all(|p| p.instance.is_some());
            if slotted && presentation.level.boxes() {
                let totals = group_totals(group);
                out.push(group_row(group, &totals));
                sources.push(RowSource::Group(group, totals));
                for p in group {
                    out.push(slot_row(p));
                    sources.push(RowSource::Sheep(p));
                }
            } else {
                for p in group {
                    out.push(plain_row(p, slotted));
                    sources.push(RowSource::Sheep(p));
                }
            }
        }

        paint(
            out,
            Self::headers(),
            presentation,
            status_word,
            |header, _cell, index| match &sources[index] {
                RowSource::Sheep(p) => process_info_paint(header, p),
                RowSource::Group(g, totals) => group_paint(header, g, totals),
            },
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            "STATUS" => "status",
            "PID" => "pid",
            "RESTARTS" => "restarts",
            "EXIT" => "last_exit",
            // `cfg_cell` folds two fields into one cell; `overridden` rides
            // in `JSON_ONLY`.
            "CFG" => "pending",
            "CPU" => "cpu_percent",
            "MEM" => "memory_bytes",
            "UPTIME" => "uptime_ms",
            "FOLD" => "fold",
            "SMIT" => "smit",
            other => panic!("FlockRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // Absolute paths, wider than the rest of the table together.
        "out_file",
        "err_file",
        // Always `null`: every row here is a sheep.
        "dog",
        // Always `null`: only `Describe` walks for lambs.
        "lambs",
        // A handshake is a fact about a dog, and every row here is a sheep.
        "handshook",
        // A dog fact too: a shepherd gives up only on dogs.
        "dog_stale",
        // The table labels each slot instead; the JSON stays flat.
        "instance",
        // CFG's header maps to `pending`, so `overridden` rides here.
        "overridden",
    ];

    // Parallel to `headers()`. The rest survive in ascending order. CFG ties
    // with EXIT at `6` and yields first: `render_boxed_ex`'s `max_by_key`
    // takes the last of an equal pair, and CFG sits later in `headers()`.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 2, 4, 6, 6, 5, 3, 1, 7, 8];
}

/// Splits a listing into runs of one app's adjacent rows, keyed on NAME.
///
/// Sound only because `sort_flock` orders by (name, instance, id). The
/// `slotted` rule stays with each caller.
fn name_groups(items: &[ProcessInfo]) -> impl Iterator<Item = &[ProcessInfo]> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= items.len() {
            return None;
        }
        let name = items[at].name.as_str();
        let end = items[at..]
            .iter()
            .position(|p| p.name != name)
            .map_or(items.len(), |offset| at + offset);
        let group = &items[at..end];
        at = end;
        Some(group)
    })
}

/// An app's summed CPU/MEM/RESTARTS and its earliest UPTIME, shared by
/// [`group_row`] and [`group_paint`].
struct GroupTotals {
    /// Every slot's restarts, added up.
    restarts: u32,
    /// Every slot's CPU summed, `None` only when no slot reported one.
    cpu: Option<f32>,
    /// Every slot's memory summed, `None` under `cpu`'s rule.
    memory: Option<u64>,
    /// The shortest uptime across slots: time since the app was last
    /// disturbed.
    uptime_ms: u64,
}

fn group_totals(group: &[ProcessInfo]) -> GroupTotals {
    GroupTotals {
        restarts: group.iter().map(|p| p.restarts).sum(),
        cpu: group
            .iter()
            .filter_map(|p| p.cpu_percent)
            .fold(None, |acc, c| Some(acc.unwrap_or(0.0) + c)),
        memory: group
            .iter()
            .filter_map(|p| p.memory_bytes)
            .fold(None, |acc, m| Some(acc.unwrap_or(0) + m)),
        uptime_ms: group.iter().map(|p| p.uptime_ms).min().unwrap_or(0),
    }
}

/// The header above an app's instances: what the app costs, how many there
/// are, and the per-app facts FOLD and SMIT.
///
/// STATUS stays plain text here so [`group_paint`] can dress it through the
/// same [`Paint::Status`] path a sheep's cell takes.
fn group_row(group: &[ProcessInfo], totals: &GroupTotals) -> Vec<String> {
    let first = &group[0];
    vec![
        String::new(),
        format!("{} \u{d7}{}", first.name, group.len()),
        group_status(group),
        String::new(),
        totals.restarts.to_string(),
        String::new(),
        // Blank, not `-`, as ID/PID/EXIT are above: pending and overridden
        // are per-instance facts with no group-level answer.
        String::new(),
        totals
            .cpu
            .map_or_else(|| "-".to_string(), |c| format!("{c:.1}%")),
        totals
            .memory
            .map_or_else(|| "-".to_string(), super::human_bytes),
        super::human_duration(totals.uptime_ms),
        first.fold.clone().unwrap_or_else(|| "-".to_string()),
        first.smit.clone().unwrap_or_else(|| "-".to_owned()),
    ]
}

/// One instance under its group header. NAME carries only the `\u{21b3} :2`
/// marker, which teaches the `web:2` selector.
///
/// FOLD and SMIT are blank, not `-`: the group row above carries both.
fn slot_row(p: &ProcessInfo) -> Vec<String> {
    let slot = p
        .instance
        .map_or_else(String::new, |s| format!(" \u{21b3} :{s}"));
    vec![
        p.id.to_string(),
        slot,
        // Never a dog, but through `Reported` anyway so this cell has one
        // spelling.
        reported(p).word(),
        p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        p.restarts.to_string(),
        exit_cell(p.pid, p.last_exit),
        // A real per-instance fact, unlike FOLD/SMIT below: a load can park
        // a different set of fields on each slot.
        cfg_cell(p.pending.as_deref(), p.overridden.as_deref()),
        p.cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        p.memory_bytes
            .map_or_else(|| "-".to_string(), super::human_bytes),
        super::human_duration(p.uptime_ms),
        String::new(),
        String::new(),
    ]
}

/// One line per process: an app with one instance, a mixed group missing a
/// slot, or a flat style.
///
/// `slotted` earns the `web:0` suffix: more than one instance, every one
/// reporting its slot. Anything else leaves NAME alone.
fn plain_row(p: &ProcessInfo, slotted: bool) -> Vec<String> {
    let name = match (slotted, p.instance) {
        (true, Some(slot)) => format!("{}:{slot}", p.name),
        _ => p.name.clone(),
    };
    vec![
        p.id.to_string(),
        name,
        // `Reported`, not `p.status`: the plain path must say what the boxed
        // one does.
        reported(p).word(),
        p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        p.restarts.to_string(),
        exit_cell(p.pid, p.last_exit),
        cfg_cell(p.pending.as_deref(), p.overridden.as_deref()),
        p.cpu_percent
            .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
        p.memory_bytes
            .map_or_else(|| "-".to_string(), super::human_bytes),
        super::human_duration(p.uptime_ms),
        p.fold.clone().unwrap_or_else(|| "-".to_string()),
        p.smit.clone().unwrap_or_else(|| "-".to_owned()),
    ]
}

/// What one row's STATUS column reports: the lifecycle status, unless this
/// row is a dog whose process is up and which has never answered this
/// shepherd.
///
/// Keyed on `dog` as well as `handshook`, so the silence rule holds here
/// rather than resting on the daemon always sending a sheep `None`.
fn reported(p: &ProcessInfo) -> Reported {
    if p.dog.is_none() {
        return Reported::Live(p.status);
    }
    Reported::of(p.status, p.handshook)
}

/// The note one row owes a reader beyond its STATUS cell, or `None` when the
/// cell says everything.
///
/// Goes through [`reported`], so one guard decides both the word and the
/// note explaining it.
pub(crate) fn silence_note(p: &ProcessInfo) -> Option<String> {
    crate::vocabulary::silence_note(&p.name, reported(p), p.dog_stale)
}

/// The group's status: the shared word when every instance agrees, else a
/// count per state. Plain text either way; see [`group_row`].
fn group_status(group: &[ProcessInfo]) -> String {
    let first = group[0].status;
    if group.iter().all(|p| p.status == first) {
        return first.to_string();
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for p in group {
        *counts.entry(p.status.to_string()).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(status, n)| format!("{n} {status}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The treatment a group's header row wears: RESTARTS, CPU and MEM read
/// [`GroupTotals`], and STATUS colours only when every slot agrees.
///
/// ID is left plain, unlike in [`process_info_paint`]: a group row's ID cell
/// is empty.
fn group_paint(header: &str, group: &[ProcessInfo], totals: &GroupTotals) -> Paint {
    match header {
        "FOLD" => Paint::Role(Role::Ink3),
        // No `Reported::of`: a dog is never stocked to several instances,
        // so no group this branch can see has a handshake to report.
        "STATUS" => {
            let first = group[0].status;
            if group.iter().all(|p| p.status == first) {
                Paint::Status(Reported::Live(first))
            } else {
                Paint::Default
            }
        }
        "RESTARTS" => Paint::Role(restarts_role(totals.restarts)),
        "CPU" => Paint::Role(cpu_role(totals.cpu)),
        "MEM" => Paint::Role(mem_role(totals.memory)),
        _ => Paint::Default,
    }
}

/// One STATUS cell: the face, the word and the role, for every table with a
/// STATUS column. `vocabulary.rs` owns the faces and the roles.
///
/// `presentation.level.sheep()` decides whether a face appears,
/// `status_word` whether the word rides beside it. The whole cell takes one
/// [`crate::output::paint::style_for`] span, so
/// [`crate::output::width::visible_width`] has one boundary to discount.
fn status_cell(reported: Reported, presentation: Presentation, status_word: bool) -> String {
    let word = reported.word();
    let mut text = if presentation.level.sheep() {
        let face = reported.face();
        if status_word {
            format!("{face} {word}")
        } else {
            face.to_string()
        }
    } else {
        word
    };
    colour_cell(&mut text, reported.role(), presentation);
    text
}

/// What one cell should become, decided from its column's name.
#[derive(Debug, Clone, Copy)]
pub(super) enum Paint {
    /// Nothing of this cell's own. The `-` placeholder rule still applies.
    Default,
    /// Wrap the cell in this role's span.
    Role(Role),
    /// Replace the cell with [`status_cell`]. The only variant that changes
    /// content rather than only colour, so STATUS cannot be a role.
    Status(Reported),
}

/// Paints one table's cells, asking `paint_of` for each by column name.
///
/// The closure gets `(header, cell, index)` and no row: `index` addresses the
/// payload, never a sibling cell. `zip` stops at the shorter of row and
/// `headers`.
pub(super) fn paint<F>(
    mut rows: Vec<Vec<String>>,
    headers: &[&'static str],
    presentation: Presentation,
    status_word: bool,
    paint_of: F,
) -> Vec<Vec<String>>
where
    F: Fn(&str, &str, usize) -> Paint,
{
    for (index, row) in rows.iter_mut().enumerate() {
        for (cell, header) in row.iter_mut().zip(headers) {
            match paint_of(header, cell, index) {
                Paint::Status(reported) => {
                    *cell = status_cell(reported, presentation, status_word);
                }
                Paint::Role(role) => colour_cell(cell, role, presentation),
                Paint::Default => mute_a_dash(cell, presentation),
            }
        }
    }
    rows
}

/// The treatment every column read off a [`ProcessInfo`] wears, shared by
/// `FlockRows`, `DogRows` and `FlushedRows`.
///
/// A column absent from this match wears [`Paint::Default`]: NAME, UPTIME and
/// the two path columns have no state and no threshold.
fn process_info_paint(header: &str, p: &ProcessInfo) -> Paint {
    match header {
        // Chrome: stable labels, so they must not draw the eye.
        "ID" | "FOLD" => Paint::Role(Role::Ink3),
        // The one column reading two fields: `handshook` overrides `status`
        // for a dog that has never answered this shepherd.
        "STATUS" => Paint::Status(reported(p)),
        "RESTARTS" => Paint::Role(restarts_role(p.restarts)),
        "EXIT" => Paint::Role(exit_role(p.pid, p.last_exit)),
        "CPU" => Paint::Role(cpu_role(p.cpu_percent)),
        "MEM" => Paint::Role(mem_role(p.memory_bytes)),
        "SOURCE" => p
            .dog
            .as_ref()
            .map_or(Paint::Default, |source| Paint::Role(source_role(source))),
        // PID and SMIT reach the dash rule: a real value is plain, an
        // absent one is muted.
        _ => Paint::Default,
    }
}

/// [`Role`] for a SOURCE cell: the column answers "shep's own code, or
/// something else".
///
/// `built-in` is muted. Anything else, an adopted third-party binary or a
/// `DogSource` this client predates, takes `Role::Butter`: worth a glance,
/// never a fault.
fn source_role(source: &DogSource) -> Role {
    match source {
        DogSource::BuiltIn => Role::Ink3,
        _ => Role::Butter,
    }
}

/// The treatment the four dog-action rows wear over their shared columns
/// `NAME SOURCE SHEPHERD STATUS`.
///
/// Keyed off the rendered cell rather than the struct, unlike
/// [`process_info_paint`]: the four carry `source` and `status` as different
/// types. STATUS is coloured only when it names a status, since the field can
/// also hold a sentence saying why no shepherd answered.
fn dog_action_paint(header: &str, cell: &str) -> Paint {
    match header {
        "SOURCE" => match cell {
            "built-in" => Paint::Role(Role::Ink3),
            "-" => Paint::Default,
            _ => Paint::Role(Role::Butter),
        },
        // `Reported::Live`: these rows carry no `handshook` field.
        "STATUS" => status_named_by(cell).map_or(Paint::Default, |status| {
            Paint::Status(Reported::Live(status))
        }),
        _ => Paint::Default,
    }
}

/// [`Role`] for one OUTCOME cell, over the eleven kinds the three per-sheep
/// reply tables between them produce.
///
/// `Meadow` worked; `Ink3` has nothing to report, `skipped` being a reload
/// drainee and `not_running` a sheep with no live process; `Butter` is a gap
/// the operator can close; `Bark` failed. An unrecognised kind takes
/// `Butter`: this client is older than the daemon.
fn outcome_role(kind: &str) -> Role {
    match kind {
        "replied" | "delivered" | "sent" => Role::Meadow,
        "skipped" | "not_running" => Role::Ink3,
        "timed_out" | "failed" | "not_written" => Role::Bark,
        _ => Role::Butter,
    }
}

/// The treatment the three per-sheep reply tables wear over their shared
/// columns `ID NAME OUTCOME DETAIL`.
///
/// DETAIL is left plain: unbounded free text, present only when OUTCOME has
/// already said what happened.
fn reply_paint(header: &str, cell: &str) -> Paint {
    match header {
        "ID" => Paint::Role(Role::Ink3),
        "OUTCOME" => Paint::Role(outcome_role(cell)),
        _ => Paint::Default,
    }
}

/// The [`ProcStatus`] a free-text STATUS cell is naming, if it is naming one:
/// the dog-action rows carry `status` as a `String` that can also hold a
/// sentence.
///
/// Matched against each variant's own
/// [`fmt::Display`](std::fmt::Display), so it cannot drift from the rendering
/// it inverts.
fn status_named_by(text: &str) -> Option<ProcStatus> {
    const EVERY: [ProcStatus; 6] = [
        ProcStatus::Starting,
        ProcStatus::Online,
        ProcStatus::Stopping,
        ProcStatus::Stopped,
        ProcStatus::Errored,
        ProcStatus::WaitingRestart,
    ];
    EVERY.into_iter().find(|status| status.to_string() == text)
}

/// Colours a cell [`Role::Ink3`] when it holds the `-` placeholder: an absent
/// value must not compete with a real one. [`Paint::Default`] is what an impl
/// returns to ask for this rule.
pub(super) fn mute_a_dash(cell: &mut String, presentation: Presentation) {
    if cell == "-" {
        colour_cell(cell, Role::Ink3, presentation);
    }
}

/// Wraps `cell` in [`crate::output::paint::style_for`]'s span for `role`, or
/// leaves it untouched when `presentation.colour` is off. The one place
/// colour is applied, STATUS included through [`status_cell`].
pub(super) fn colour_cell(cell: &mut String, role: Role, presentation: Presentation) {
    if !presentation.colour {
        return;
    }
    let style = super::paint::style_for(role, presentation.deep_colour);
    *cell = format!("{style}{cell}{style:#}");
}

/// MEM's colour boundary, in bytes. 128 MiB separates the two footprints a
/// real flock shows side by side: a worker at a few megabytes, a service at
/// hundreds.
const MEM_ELEVATED_BYTES: u64 = 128 * 1024 * 1024;

/// [`Role`] for a MEM cell. `None` is [`Role::Ink3`], the colour every dash
/// gets; otherwise [`MEM_ELEVATED_BYTES`]'s two-tier ramp.
///
/// Two tiers, never [`Role::Bark`], which is reserved for faults. The ramp
/// answers "is this unusual for this flock"; `--format json` carries the
/// exact number.
fn mem_role(memory_bytes: Option<u64>) -> Role {
    match memory_bytes {
        None => Role::Ink3,
        Some(bytes) if bytes >= MEM_ELEVATED_BYTES => Role::Butter,
        Some(_) => Role::Meadow,
    }
}

/// CPU's colour boundary, in percent of one core. Sustained use at or above
/// this is unusual for a steady-state service. `Role::Bark` stays reserved
/// for a fault, never a busy sheep.
const CPU_ELEVATED_PERCENT: f32 = 50.0;

/// [`Role`] for a CPU cell. `None` and `0.0%` are both [`Role::Ink3`]:
/// neither is news. A busy sheep takes [`CPU_ELEVATED_PERCENT`]'s ramp.
fn cpu_role(cpu_percent: Option<f32>) -> Role {
    match cpu_percent {
        None => Role::Ink3,
        Some(cpu) if cpu <= 0.0 => Role::Ink3,
        Some(cpu) if cpu >= CPU_ELEVATED_PERCENT => Role::Butter,
        Some(_) => Role::Meadow,
    }
}

/// [`Role`] for a RESTARTS cell: `Role::Ink3` at zero, `Role::Butter` above
/// it. Never `Role::Bark`: a restart is a signal, not a fault.
const fn restarts_role(restarts: u32) -> Role {
    if restarts == 0 {
        Role::Ink3
    } else {
        Role::Butter
    }
}

/// [`Role`] for an EXIT cell, mirroring [`exit_cell`]'s branches rather than
/// parsing the rendered text back: a live process and a clean `0` exit both
/// take `Role::Ink3`. Only a nonzero code or a signal earns `Role::Bark`.
fn exit_role(pid: Option<u32>, last_exit: Option<ExitInfo>) -> Role {
    if pid.is_some() {
        return Role::Ink3;
    }
    match last_exit {
        Some(ExitInfo {
            code: Some(code), ..
        }) if code != 0 => Role::Bark,
        Some(ExitInfo {
            signal: Some(_), ..
        }) => Role::Bark,
        // A clean `0` exit, an exit the daemon could not characterize (both
        // fields `None`), or no exit recorded: none of the three is news.
        _ => Role::Ink3,
    }
}

/// The EXIT column's cell: the last exit's code or signal name for a sheep
/// that is not running, `-` otherwise.
///
/// Gated on `pid` rather than on `status`: `pid` is `None` for exactly the
/// statuses with no live process. `pub(crate)` so `lookout::view::flock`
/// shares the rule.
pub(crate) fn exit_cell(pid: Option<u32>, last_exit: Option<ExitInfo>) -> String {
    if pid.is_some() {
        return "-".to_string();
    }
    match last_exit {
        None => "-".to_string(),
        Some(ExitInfo {
            code: Some(code), ..
        }) => code.to_string(),
        Some(ExitInfo {
            signal: Some(signal),
            ..
        }) => signal_label(signal),
        // Both `None`: an exit the daemon could not characterize.
        Some(ExitInfo {
            code: None,
            signal: None,
        }) => "-".to_string(),
    }
}

/// The CFG column's cell: `!N` for N fields parked for the next spawn, `*N`
/// for an override with nothing parked, `-` for neither.
///
/// `pending` wins when both are non-empty: a parked field is one `shep
/// reload` away from taking effect. `shep describe` lists the names a cell
/// has no room for. `pub(crate)` so `lookout::view::flock` shares the rule.
pub(crate) fn cfg_cell(pending: Option<&[String]>, overridden: Option<&[String]>) -> String {
    match pending {
        Some(fields) if !fields.is_empty() => return format!("!{}", fields.len()),
        _ => {}
    }
    match overridden {
        Some(fields) if !fields.is_empty() => format!("*{}", fields.len()),
        _ => "-".to_string(),
    }
}

/// Renders a raw unix signal number as its canonical name (`SIGKILL`), or the
/// bare number when this platform's own signal table has none for it.
///
/// Resolving it here is sound because a client reaches a daemon only over a
/// local socket, so the `ProcessInfo` came from this same OS. Gated at the
/// item rather than in the body: `nix` is a unix-only dependency, so a
/// Windows build never links it.
#[cfg(unix)]
fn signal_label(raw: i32) -> String {
    nix::sys::signal::Signal::try_from(raw)
        .map_or_else(|_| raw.to_string(), |signal| signal.as_str().to_string())
}

/// Windows counterpart: no signal table to consult, so the bare number.
#[cfg(not(unix))]
fn signal_label(raw: i32) -> String {
    raw.to_string()
}

/// The dogs half of a flock listing: the `ProcessInfo`s whose `dog` marker
/// is set.
///
/// Every column the two tables share sits in the same order; each table's own
/// columns come last:
///
/// ```text
/// common:  ID  NAME  STATUS  PID  RESTARTS  EXIT  CPU  MEM  UPTIME
/// sheep:   ... + FOLD  SMIT
/// dogs:    ... + SOURCE
/// ```
///
/// `FOLD` and `SMIT` are impossible for a dog rather than empty: a dog
/// belongs to no fold, and a smit is a mark a dog paints on a sheep.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DogRows(pub Vec<ProcessInfo>);

/// `DogSource`'s table rendering, shared by every payload with a SOURCE
/// column. `DogSource` is `#[non_exhaustive]`, so a kind this client predates
/// renders `unknown`.
fn dog_source_label(source: &DogSource) -> &'static str {
    match source {
        DogSource::BuiltIn => "built-in",
        DogSource::Adopted { .. } => "adopted",
        _ => "unknown",
    }
}

impl Render for DogRows {
    fn headers() -> &'static [&'static str] {
        &[
            "ID", "NAME", "STATUS", "PID", "RESTARTS", "EXIT", "CPU", "MEM", "UPTIME", "SOURCE",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|p| {
                vec![
                    p.id.to_string(),
                    p.name.clone(),
                    // A dog whose process is up and which has never
                    // answered this shepherd reads `silent`, not `online`.
                    reported(p).word(),
                    p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                    p.restarts.to_string(),
                    exit_cell(p.pid, p.last_exit),
                    p.cpu_percent
                        .map_or_else(|| "-".to_string(), |cpu| format!("{cpu:.1}%")),
                    p.memory_bytes
                        .map_or_else(|| "-".to_string(), super::human_bytes),
                    super::human_duration(p.uptime_ms),
                    // Never the adopted path: too wide for a column. `None`
                    // is unreachable, since callers filter on
                    // `dog.is_some()`.
                    p.dog.as_ref().map_or("-".to_string(), |source| {
                        dog_source_label(source).to_string()
                    }),
                ]
            })
            .collect()
    }

    /// [`process_info_paint`], the same function `FlockRows` uses; SOURCE is
    /// the one column not shared with that table.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        paint(
            self.rows(),
            Self::headers(),
            presentation,
            status_word,
            |header, _cell, index| process_info_paint(header, &self.0[index]),
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            "STATUS" => "status",
            "PID" => "pid",
            "RESTARTS" => "restarts",
            "EXIT" => "last_exit",
            "CPU" => "cpu_percent",
            "MEM" => "memory_bytes",
            "UPTIME" => "uptime_ms",
            "SOURCE" => "dog",
            other => panic!("DogRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // A sheep concept: a dog is supervised, never grouped by fold.
        "fold",
        // Absolute paths, wider than the rest of the table together.
        "out_file",
        "err_file",
        // Always `null`: only `Describe` walks for lambs.
        "lambs",
        // A dog paints smits; nothing paints one on a dog.
        "smit",
        // It decides what STATUS says, so a column would say it twice.
        // `status` alone still reads `online` for a silent dog.
        "handshook",
        // Not derivable from `handshook`: a dog spawned a moment ago and one
        // this shepherd has given up on are both `handshook: false`.
        "dog_stale",
        // Always `Some(0)`: a dog is never stocked to N instances.
        "instance",
        // `Actor::apply_one` refuses a config entry naming a dog, so a load
        // can neither park nor override one.
        "pending",
        "overridden",
    ];

    // Parallel to `headers()`. The nine shared columns carry the numbers
    // `FlockRows` gives them, so both tables narrow in the same order.
    // SOURCE takes FOLD's `7`.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 2, 4, 6, 5, 3, 1, 7];
}

/// One sheep's lamb tree, as `describe`'s second table.
///
/// Not `#[serde(transparent)]`: this type's JSON is never read, since
/// `describe --format json` serializes the listing as [`FlockRows`] with its
/// own `lambs`. It exists to reach [`render_table`](super::render_table).
#[derive(Debug, Serialize)]
pub struct LambRows(pub Vec<Lamb>);

/// No colour: both columns are identity, and a lamb has no status, reading or
/// placeholder for one to carry.
impl Render for LambRows {
    fn headers() -> &'static [&'static str] {
        &["PID", "NAME"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|lamb| vec![lamb.pid.to_string(), lamb.name.clone()])
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "PID" => "pid",
            "NAME" => "name",
            other => panic!("LambRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()`. Two columns, both identity, so this never
    // narrows; spelled out so a later header does not inherit it by omission.
    const PRIORITIES: &'static [u8] = &[0, 0];
}

/// `shep enable <name>`: what the config edit and, if a shepherd is running,
/// the resulting `EnableDog` RPC did.
///
/// [`Self::shepherd_acted`] and [`Self::status`] are how a `--format json`
/// consumer tells the two outcomes apart.
#[derive(Debug, Serialize)]
pub struct DogEnabledRow {
    /// The dog's name.
    pub name: String,
    /// Where its binary comes from, read out of `shep.toml`:
    /// [`DogSource::Adopted`] for a name in `[daemon] adopted_dogs`,
    /// [`DogSource::BuiltIn`] otherwise.
    pub source: DogSource,
    /// Whether a shepherd was reached and asked to start the dog. `false`
    /// means only the config changed; `enable` never autostarts one.
    pub shepherd_acted: bool,
    /// The dog's resulting status: a real `ProcStatus` rendering
    /// (`"online"`, `"starting"`, ...) when a shepherd started it, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

// Shared scaffolding for the four dog-action tables. All four render one row
// of `["NAME", "SOURCE", "SHEPHERD", "STATUS"]` and share the JSON keys, the
// priorities and the paint dispatch; each resolves its own `source` to a
// label first.
struct DogActionRow<'a> {
    name: &'a str,
    source: &'a str,
    shepherd_acted: bool,
    status: &'a str,
}

impl DogActionRow<'_> {
    fn headers() -> &'static [&'static str] {
        &["NAME", "SOURCE", "SHEPHERD", "STATUS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.name.to_string(),
            self.source.to_string(),
            self.shepherd_acted.to_string(),
            self.status.to_string(),
        ]]
    }

    // The four dog-action rows' shared treatment; see `dog_action_paint`.
    fn rows_for(
        rows: Vec<Vec<String>>,
        presentation: Presentation,
        status_word: bool,
    ) -> Vec<Vec<String>> {
        paint(
            rows,
            Self::headers(),
            presentation,
            status_word,
            |header, cell, _index| dog_action_paint(header, cell),
        )
    }

    // Parallel to `headers()`. SOURCE drops before SHEPHERD, and a 4-column
    // table loses only one of the two.
    const PRIORITIES: &'static [u8] = &[0, 7, 6, 0];
}

// One JSON key rule for the four dog-action tables; the panic names the
// concrete type. A macro, not a shared fn: rustc's dead-code pass cannot see
// a use that occurs only inside another trait impl's body.
macro_rules! dog_action_json_key {
    ($caller:expr, $header:expr) => {{
        let caller: &'static str = $caller;
        let header: &str = $header;
        match header {
            "NAME" => "name",
            "SOURCE" => "source",
            "SHEPHERD" => "shepherd_acted",
            "STATUS" => "status",
            other => panic!("{caller}::headers() does not include {other:?}"),
        }
    }};
}

impl Render for DogEnabledRow {
    fn headers() -> &'static [&'static str] {
        DogActionRow::headers()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        DogActionRow {
            name: &self.name,
            source: dog_source_label(&self.source),
            shepherd_acted: self.shepherd_acted,
            status: &self.status,
        }
        .rows()
    }

    /// Shared with the other three dog-action rows; see
    /// [`DogActionRow::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        DogActionRow::rows_for(self.rows(), presentation, status_word)
    }

    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        dog_action_json_key!("DogEnabledRow", header)
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    const PRIORITIES: &'static [u8] = DogActionRow::PRIORITIES;
}

/// `shep disable <name>`: what the config edit and, if a shepherd is running,
/// the resulting `DisableDog` RPC did.
///
/// [`Self::source`] comes from the same `shep.toml` lookup
/// [`DogEnabledRow::source`] uses, never from the reply, which carries only
/// ids.
#[derive(Debug, Serialize)]
pub struct DogDisabledRow {
    /// The dog's name.
    pub name: String,
    /// Where its binary comes from; see this type's own doc.
    pub source: DogSource,
    /// Whether a shepherd was reached and asked to stop the dog.
    pub shepherd_acted: bool,
    /// The dog's resulting status: `"stopped"` when a shepherd acted, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

impl Render for DogDisabledRow {
    fn headers() -> &'static [&'static str] {
        DogActionRow::headers()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        DogActionRow {
            name: &self.name,
            source: dog_source_label(&self.source),
            shepherd_acted: self.shepherd_acted,
            status: &self.status,
        }
        .rows()
    }

    /// Same treatment as [`DogEnabledRow::rows_for`]; see
    /// [`DogActionRow::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        DogActionRow::rows_for(self.rows(), presentation, status_word)
    }

    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        dog_action_json_key!("DogDisabledRow", header)
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    const PRIORITIES: &'static [u8] = DogActionRow::PRIORITIES;
}

/// `shep adopt <path> [--name <name>]`: what the config edit and, if a
/// shepherd is running, the resulting `EnableDog` RPC did.
///
/// [`Self::source`] is always [`DogSource::Adopted`]: this verb vetted the
/// path itself, so it looks nothing up.
#[derive(Debug, Serialize)]
pub struct DogAdoptedRow {
    /// The dog's name.
    pub name: String,
    /// Always [`DogSource::Adopted`], carrying the vetted, canonicalized
    /// path `adopt` just recorded.
    pub source: DogSource,
    /// Whether a shepherd was reached and asked to start the dog. `false`
    /// means only the config changed; no verb here autostarts one.
    pub shepherd_acted: bool,
    /// The dog's resulting status: a real `ProcStatus` rendering
    /// (`"online"`, `"starting"`, ...) when a shepherd started it, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

impl Render for DogAdoptedRow {
    fn headers() -> &'static [&'static str] {
        DogActionRow::headers()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        DogActionRow {
            name: &self.name,
            source: dog_source_label(&self.source),
            shepherd_acted: self.shepherd_acted,
            status: &self.status,
        }
        .rows()
    }

    /// Same treatment as [`DogEnabledRow::rows_for`]; see
    /// [`DogActionRow::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        DogActionRow::rows_for(self.rows(), presentation, status_word)
    }

    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        dog_action_json_key!("DogAdoptedRow", header)
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    const PRIORITIES: &'static [u8] = DogActionRow::PRIORITIES;
}

/// `shep rehome <name>`: what the config edit and, if a shepherd is running,
/// the resulting `DisableDog` RPC did.
///
/// [`Self::source`] is an [`Option`] because `rehome` reports what it forgot,
/// and it still runs for a name `shep.toml` never had an entry for.
#[derive(Debug, Serialize)]
pub struct DogRehomedRow {
    /// The dog's name.
    pub name: String,
    /// Where its binary came from, read before this verb forgot it. See
    /// this type's own doc for what `None` means.
    pub source: Option<DogSource>,
    /// Whether a shepherd was reached and asked to stop the dog.
    pub shepherd_acted: bool,
    /// The dog's resulting status: `"stopped"` when a shepherd acted, or a
    /// sentence explaining why not when none answered.
    pub status: String,
}

impl Render for DogRehomedRow {
    fn headers() -> &'static [&'static str] {
        DogActionRow::headers()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        // `-` for `None`, as `DogRows::rows` renders it.
        let source_label = self.source.as_ref().map_or_else(
            || "-".to_string(),
            |source| dog_source_label(source).to_string(),
        );
        DogActionRow {
            name: &self.name,
            source: &source_label,
            shepherd_acted: self.shepherd_acted,
            status: &self.status,
        }
        .rows()
    }

    /// Same treatment as [`DogEnabledRow::rows_for`]; see
    /// [`DogActionRow::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        DogActionRow::rows_for(self.rows(), presentation, status_word)
    }

    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        dog_action_json_key!("DogRehomedRow", header)
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    const PRIORITIES: &'static [u8] = DogActionRow::PRIORITIES;
}

/// `Response::Flushed(Vec<ProcessInfo>)`: the sheep a `shep flush` matched,
/// rendered by the files it emptied rather than by their lifecycle.
///
/// Serializes exactly as [`FlockRows`] does, over the same
/// `Vec<ProcessInfo>`, so only the table differs. `out_file`/`err_file` are
/// free-form config taken verbatim, so a mistyped one empties something that
/// is not a log.
///
/// One row per sheep: several can share a log path, and the daemon truncates
/// each distinct path once.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct FlushedRows(pub Vec<ProcessInfo>);

impl Render for FlushedRows {
    fn headers() -> &'static [&'static str] {
        &["ID", "NAME", "OUT_FILE", "ERR_FILE"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|p| {
                vec![
                    p.id.to_string(),
                    p.name.clone(),
                    // `-`: a peer daemon predating the field, never a sheep
                    // with no log file.
                    p.out_file.clone().unwrap_or_else(|| "-".to_string()),
                    p.err_file.clone().unwrap_or_else(|| "-".to_string()),
                ]
            })
            .collect()
    }

    /// [`process_info_paint`] again: ID muted, NAME plain, and both path
    /// columns left to the dash rule. A real path is the subject of this
    /// table rather than a reading about it.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        paint(
            self.rows(),
            Self::headers(),
            presentation,
            status_word,
            |header, _cell, index| process_info_paint(header, &self.0[index]),
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            "OUT_FILE" => "out_file",
            "ERR_FILE" => "err_file",
            other => panic!("FlushedRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // A sheep's lifecycle and resource use, which a flush neither reads
        // nor changes. They stay in the JSON so a consumer switching on the
        // envelope's `command` does not find the record shape switching too.
        "status",
        "pid",
        "restarts",
        "uptime_ms",
        "fold",
        "cpu_percent",
        "memory_bytes",
        // Every row is a sheep: no `dog`, no handshake, and nothing for a
        // shepherd to give up on.
        "dog",
        "handshook",
        "dog_stale",
        // Always `null`: only `Describe` walks for lambs.
        "lambs",
        // Nothing a flush reads or changes, and a column each would push
        // OUT_FILE/ERR_FILE off the side of a terminal.
        "last_exit",
        "smit",
        "instance",
        "pending",
        "overridden",
    ];

    // Parallel to `headers()`. ERR_FILE survives one round longer than
    // OUT_FILE: a crash is read from stderr first.
    const PRIORITIES: &'static [u8] = &[0, 0, 7, 6];
}

/// One of the shepherd's own log files, and what `shep flush --daemon` made
/// of it.
///
/// Its own payload rather than a `ProcessInfo`: these files belong to no
/// sheep, and the CLI empties them itself without asking the daemon.
#[derive(Debug, Serialize)]
pub struct EmptiedFile {
    /// Which of the shepherd's streams this file takes: `stdout` or `stderr`.
    pub stream: &'static str,
    /// The file's absolute path, as this invocation resolved `$SHEP_HOME`.
    pub file: String,
    /// `emptied` when the file was truncated, `absent` when there was no
    /// such file: already empty, and not created just to say so.
    pub result: &'static str,
}

/// `shep flush --daemon`: one row per file the shepherd logs into.
///
/// `transparent` so the JSON is a plain array, as every list payload here is.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct EmptiedFiles(pub Vec<EmptiedFile>);

impl Render for EmptiedFiles {
    fn headers() -> &'static [&'static str] {
        &["STREAM", "FILE", "RESULT"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|f| vec![f.stream.to_string(), f.file.clone(), f.result.to_string()])
            .collect()
    }

    /// RESULT alone. `absent` is muted rather than marked: no file to
    /// truncate is the state `flush` was asked to produce, not a failure.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let rows = self.rows();
        paint(
            rows,
            Self::headers(),
            presentation,
            status_word,
            |header, cell, _index| match (header, cell) {
                ("RESULT", "emptied") => Paint::Role(Role::Meadow),
                ("RESULT", _) => Paint::Role(Role::Ink3),
                _ => Paint::Default,
            },
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "STREAM" => "stream",
            "FILE" => "file",
            "RESULT" => "result",
            other => panic!("EmptiedFiles::headers() does not include {other:?}"),
        }
    }

    // Every field is a column: a verb that emptied a file and would not say
    // which one has reported nothing.
    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()`. Three columns is `render_boxed`'s own floor,
    // so this never narrows.
    const PRIORITIES: &'static [u8] = &[0, 6, 0];
}

/// `Response::Deleted(Vec<u32>)`: the ids that were removed.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DeletedIds(pub Vec<u32>);

/// No colour, and not the muted ID every other table gives that column: here
/// the ID is the only column and is the content, so muting it would fade the
/// whole table.
impl Render for DeletedIds {
    fn headers() -> &'static [&'static str] {
        &["ID"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0.iter().map(|id| vec![id.to_string()]).collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            other => panic!("DeletedIds::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // One column, and it is the row's whole identity.
    const PRIORITIES: &'static [u8] = &[0];
}

/// `kill`: what teardown actually achieved.
#[derive(Debug, Serialize)]
pub struct KillRow {
    /// Daemon pid at the moment of kill, read before the connection dropped.
    pub pid: u32,
    /// Whether the daemon removed its own socket file before exiting.
    pub socket_removed: bool,
}

impl Render for KillRow {
    fn headers() -> &'static [&'static str] {
        &["PID", "SOCKET_REMOVED"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.pid.to_string(), self.socket_removed.to_string()]]
    }

    /// SOCKET_REMOVED alone: `false` means the socket file outlived the
    /// daemon and the next boot has to contend with it. `Butter` and not
    /// `Bark`: a leftover to clear is no crash.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let removed = self.socket_removed;
        paint(
            self.rows(),
            Self::headers(),
            presentation,
            status_word,
            |header, _cell, _index| match header {
                "SOCKET_REMOVED" if removed => Paint::Role(Role::Meadow),
                "SOCKET_REMOVED" => Paint::Role(Role::Butter),
                _ => Paint::Default,
            },
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "PID" => "pid",
            "SOCKET_REMOVED" => "socket_removed",
            other => panic!("KillRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Two columns, both the point of the report.
    const PRIORITIES: &'static [u8] = &[0, 0];
}

/// One sheep as the muster roll remembers it, for `shep flock` when no
/// shepherd is running.
///
/// `status` is always `"stopped"`: a roll records what was registered, and
/// with no shepherd answering, nothing from it is up.
#[derive(Debug, Serialize)]
pub struct RolledSheep {
    /// The sheep's name, as saved.
    pub name: String,
    /// How many instances were running when the roll was written.
    pub instances: u32,
    /// Always `"stopped"`.
    pub status: &'static str,
}

/// Every sheep in a muster roll.
#[derive(Debug, Serialize)]
pub struct RolledSheepRows(pub Vec<RolledSheep>);

/// No colour, including on STATUS, the one unpainted STATUS column here:
/// every row carries the same literal `stopped`, and a colour identical on
/// every row distinguishes nothing.
impl Render for RolledSheepRows {
    fn headers() -> &'static [&'static str] {
        &["NAME", "INSTANCES", "STATUS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|s| vec![s.name.clone(), s.instances.to_string(), s.status.to_owned()])
            .collect()
    }

    /// # Panics
    /// If `header` is not one of [`Self::headers`]'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "INSTANCES" => "instances",
            "STATUS" => "status",
            other => panic!("RolledSheepRows has no column {other}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()`. Three columns never narrows.
    const PRIORITIES: &'static [u8] = &[0, 6, 0];
}

/// `Response::RollSaved`: where the muster roll landed, and what it recorded.
///
/// Every field is a column, for [`EmptiedFiles`]' reason.
#[derive(Debug, Serialize)]
pub struct SavedRollRow {
    /// The roll's path, exactly as the daemon reported it.
    pub file: String,
    /// How many apps that roll records.
    pub apps: u32,
}

/// No colour: a path and a count are the report itself rather than a reading
/// about it, with no state, threshold or outcome.
impl Render for SavedRollRow {
    fn headers() -> &'static [&'static str] {
        &["FILE", "APPS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.file.clone(), self.apps.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "FILE" => "file",
            "APPS" => "apps",
            other => panic!("SavedRollRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Two columns, both the point of the report.
    const PRIORITIES: &'static [u8] = &[0, 0];
}

/// One app `shep import` read out of a pm2 dump.
///
/// Never from a wire `Response`: this verb asks the daemon nothing. A `true`
/// `REUSE_PORT` is work for the operator, since shep binds nothing and the
/// app must set `SO_REUSEPORT` itself.
#[derive(Debug, Serialize)]
pub struct ImportRow {
    /// The app's name, which is also the key its instance rows were grouped by.
    pub name: String,
    /// The script the app runs.
    pub script: String,
    /// How many instances of it the dump recorded running.
    pub instances: u32,
    /// Whether the app has to set `SO_REUSEPORT` itself (pm2 cluster mode).
    pub reuse_port: bool,
}

/// `shep import`: one row per app the dump was collapsed into.
///
/// `transparent` so the JSON is a plain array.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct ImportRows(pub Vec<ImportRow>);

impl Render for ImportRows {
    fn headers() -> &'static [&'static str] {
        &["NAME", "SCRIPT", "INSTANCES", "REUSE_PORT"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|row| {
                vec![
                    row.name.clone(),
                    row.script.clone(),
                    row.instances.to_string(),
                    row.reuse_port.to_string(),
                ]
            })
            .collect()
    }

    /// REUSE_PORT alone, and only when `true`: work for the operator, so the
    /// same `Butter` a restart count above zero takes.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let rows = self.rows();
        paint(
            rows,
            Self::headers(),
            presentation,
            status_word,
            |header, cell, _index| match (header, cell) {
                ("REUSE_PORT", "true") => Paint::Role(Role::Butter),
                _ => Paint::Default,
            },
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "SCRIPT" => "script",
            "INSTANCES" => "instances",
            "REUSE_PORT" => "reuse_port",
            other => panic!("ImportRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()`. Only SCRIPT, an unbounded path, is ever
    // actually lost.
    const PRIORITIES: &'static [u8] = &[0, 8, 7, 6];
}

/// One step `shep startup` or `shep unstartup` took.
///
/// Never from a wire `Response`: neither verb asks the shepherd anything.
#[derive(Debug, Serialize)]
pub struct StartupStep {
    /// What was done: `wrote`, `removed`, `ran`.
    pub action: &'static str,
    /// The file or command it was done to.
    pub target: String,
    /// `ok`, `absent`, or the failure in one line. `absent` is an
    /// `unstartup` that found no unit to remove, not a failure.
    pub result: String,
}

/// `shep startup`/`shep unstartup`: one row per step, in the order taken.
///
/// Every step is reported even when an earlier one failed: a half-installed
/// unit needs every row to say which half.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct StartupSteps(pub Vec<StartupStep>);

impl Render for StartupSteps {
    fn headers() -> &'static [&'static str] {
        &["ACTION", "TARGET", "RESULT"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|step| {
                vec![
                    step.action.to_string(),
                    step.target.clone(),
                    step.result.clone(),
                ]
            })
            .collect()
    }

    /// RESULT alone. `absent` is muted rather than marked as a failure;
    /// anything but `ok` or `absent` is the failure line, so it takes `Bark`.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let rows = self.rows();
        paint(
            rows,
            Self::headers(),
            presentation,
            status_word,
            |header, cell, _index| match (header, cell) {
                ("RESULT", "ok") => Paint::Role(Role::Meadow),
                ("RESULT", "absent") => Paint::Role(Role::Ink3),
                ("RESULT", _) => Paint::Role(Role::Bark),
                _ => Paint::Default,
            },
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ACTION" => "action",
            "TARGET" => "target",
            "RESULT" => "result",
            other => panic!("StartupSteps::headers() does not include {other:?}"),
        }
    }

    // Every field is a column, for `EmptiedFiles`' reason.
    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()`. Three columns never narrows.
    const PRIORITIES: &'static [u8] = &[6, 0, 0];
}

/// `Response::Triggered(Vec<ActionReply>)`: one row per matched sheep, each
/// carrying what happened when the daemon tried to deliver `shep trigger`'s
/// action to it.
///
/// A newtype for the orphan rule; `transparent`, so `--format json` carries
/// each reply as the daemon sent it, `body` untruncated and with its newlines
/// intact. The table cannot; see [`Self::rows`].
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct TriggeredRows(pub Vec<ActionReply>);

/// A `Replied` body longer than this many `char`s is truncated in the table,
/// never in JSON. 80 leaves room for ID/NAME/OUTCOME on an ordinary terminal;
/// `render_table` cannot wrap.
const TRIGGER_BODY_PREVIEW_CHARS: usize = 80;

// Shared scaffolding for the three per-sheep reply tables. All three render
// `["ID", "NAME", "OUTCOME", "DETAIL"]` and share the JSON keys, the
// priorities and the paint dispatch; each verb has its own
// `describe_*_outcome` for the `(OUTCOME, DETAIL)` pair.
struct ReplyRows;

impl ReplyRows {
    fn headers() -> &'static [&'static str] {
        &["ID", "NAME", "OUTCOME", "DETAIL"]
    }

    fn row(id: u32, name: &str, outcome: &str, detail: String) -> Vec<String> {
        vec![
            id.to_string(),
            name.to_string(),
            outcome.to_string(),
            detail,
        ]
    }

    fn rows_for(
        rows: Vec<Vec<String>>,
        presentation: Presentation,
        status_word: bool,
    ) -> Vec<Vec<String>> {
        paint(
            rows,
            Self::headers(),
            presentation,
            status_word,
            |header, cell, _index| reply_paint(header, cell),
        )
    }

    // Parallel to `headers()`. DETAIL is the sole extra, and dropping it is
    // the only narrowing these tables do.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 6];
}

// One JSON key rule for the three per-sheep reply tables; the panic names the
// concrete type. A macro, not a shared fn: rustc's dead-code pass cannot see
// a use that occurs only inside another trait impl's body.
macro_rules! reply_rows_json_key {
    ($caller:expr, $header:expr) => {{
        let caller: &'static str = $caller;
        let header: &str = $header;
        match header {
            "ID" => "id",
            "NAME" => "name",
            // Both columns read the one `outcome` object, so both sit in
            // `assert_no_drift`'s `formatted` list.
            "OUTCOME" | "DETAIL" => "outcome",
            other => panic!("{caller}::headers() does not include {other:?}"),
        }
    }};
}

impl Render for TriggeredRows {
    fn headers() -> &'static [&'static str] {
        ReplyRows::headers()
    }

    /// One row per matched sheep. `OUTCOME` is [`ActionOutcome`]'s `kind`
    /// tag; `DETAIL` is where the four variants differ, via
    /// [`describe_outcome`].
    ///
    /// A `Replied` body is capped by [`preview_body`]: `render_table` writes
    /// exactly one line per row, so a multi-line body would desync the table.
    /// `--format json` is untouched.
    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|reply| {
                let (outcome, detail) = describe_outcome(&reply.outcome);
                ReplyRows::row(reply.id, &reply.name, outcome, detail)
            })
            .collect()
    }

    /// Shared with the other two per-sheep reply tables; see
    /// [`ReplyRows::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        ReplyRows::rows_for(self.rows(), presentation, status_word)
    }

    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        reply_rows_json_key!("TriggeredRows", header)
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    const PRIORITIES: &'static [u8] = ReplyRows::PRIORITIES;
}

/// [`TriggeredRows::rows`]'s per-outcome split: the `OUTCOME` label and the
/// `DETAIL` text.
///
/// `ActionOutcome` is `#[non_exhaustive]`; the wildcard arm renders a variant
/// this client predates as `unknown` with its `Debug` form.
fn describe_outcome(outcome: &ActionOutcome) -> (&'static str, String) {
    match outcome {
        ActionOutcome::Replied { body } => ("replied", preview_body(body)),
        // Names the config field: an operator learns it here or in `--help`.
        ActionOutcome::NoChannel => (
            "no_channel",
            "no shepherd channel — set channel = true, or wait_ready / \
             shutdown_with_message, which imply it"
                .to_string(),
        ),
        ActionOutcome::Skipped => (
            "skipped",
            "mid-reload — a fresh instance is replacing this one".to_string(),
        ),
        ActionOutcome::TimedOut => (
            "timed_out",
            "no reply within the app's own action_timeout".to_string(),
        ),
        other => ("unknown", format!("{other:?}")),
    }
}

/// Collapses a `Replied` body to one line, capped at
/// [`TRIGGER_BODY_PREVIEW_CHARS`] `char`s. Embedded `\n`/`\r` become the
/// two-character escapes, and a body the cap cuts off ends in `...`.
fn preview_body(body: &str) -> String {
    let mut preview = String::new();
    let mut truncated = false;
    for (seen, ch) in body.chars().enumerate() {
        if seen == TRIGGER_BODY_PREVIEW_CHARS {
            truncated = true;
            break;
        }
        match ch {
            '\n' => preview.push_str("\\n"),
            '\r' => preview.push_str("\\r"),
            other => preview.push(other),
        }
    }
    if truncated {
        preview.push_str("...");
    }
    preview
}

/// `Response::Signalled(Vec<SignalReply>)`: one row per matched sheep, each
/// carrying what happened when the shepherd tried to deliver `shep signal`'s
/// signal to it.
///
/// Shaped like [`TriggeredRows`]: the selector grammar makes a mixed flock
/// the normal case, so the outcome is per row.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct SignalledRows(pub Vec<SignalReply>);

impl Render for SignalledRows {
    fn headers() -> &'static [&'static str] {
        ReplyRows::headers()
    }

    /// One row per matched sheep. `OUTCOME` is [`SignalOutcome`]'s `kind`
    /// tag; `DETAIL` comes from [`describe_signal_outcome`].
    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|reply| {
                let (outcome, detail) = describe_signal_outcome(&reply.outcome);
                ReplyRows::row(reply.id, &reply.name, outcome, detail)
            })
            .collect()
    }

    /// Shared with the other two per-sheep reply tables; see
    /// [`ReplyRows::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        ReplyRows::rows_for(self.rows(), presentation, status_word)
    }

    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        reply_rows_json_key!("SignalledRows", header)
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    const PRIORITIES: &'static [u8] = ReplyRows::PRIORITIES;
}

/// [`SignalledRows::rows`]'s per-outcome split. `SignalOutcome` is
/// `#[non_exhaustive]`; the wildcard arm renders a variant this client
/// predates as `unknown` with its `Debug` form.
fn describe_signal_outcome(outcome: &SignalOutcome) -> (&'static str, String) {
    match outcome {
        SignalOutcome::Delivered => ("delivered", String::new()),
        SignalOutcome::NotRunning => ("not_running", "no live process to signal".to_string()),
        SignalOutcome::Failed { reason } => ("failed", reason.clone()),
        other => ("unknown", format!("{other:?}")),
    }
}

/// `Response::SentLine(Vec<LineReply>)`: one row per matched sheep, each
/// carrying what happened when the shepherd tried to write `shep whisper`'s
/// line to its stdin.
///
/// Shaped like [`TriggeredRows`]/[`SignalledRows`], for the same reason.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct SentLineRows(pub Vec<LineReply>);

impl Render for SentLineRows {
    fn headers() -> &'static [&'static str] {
        ReplyRows::headers()
    }

    /// One row per matched sheep. `OUTCOME` is [`LineOutcome`]'s `kind` tag;
    /// `DETAIL` comes from [`describe_line_outcome`].
    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|reply| {
                let (outcome, detail) = describe_line_outcome(&reply.outcome);
                ReplyRows::row(reply.id, &reply.name, outcome, detail)
            })
            .collect()
    }

    /// Shared with the other two per-sheep reply tables; see
    /// [`ReplyRows::rows_for`].
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        ReplyRows::rows_for(self.rows(), presentation, status_word)
    }

    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        reply_rows_json_key!("SentLineRows", header)
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    const PRIORITIES: &'static [u8] = ReplyRows::PRIORITIES;
}

/// [`SentLineRows::rows`]'s per-outcome split. `LineOutcome` is
/// `#[non_exhaustive]`; the wildcard arm renders a variant this client
/// predates as `unknown` with its `Debug` form.
fn describe_line_outcome(outcome: &LineOutcome) -> (&'static str, String) {
    match outcome {
        LineOutcome::Sent => ("sent", String::new()),
        // Names the config field, as `describe_outcome`'s `NoChannel` does.
        LineOutcome::NoStdin => ("no_stdin", "no stdin pipe — set stdin = true".to_string()),
        LineOutcome::NotWritten { reason } => ("not_written", reason.clone()),
        other => ("unknown", format!("{other:?}")),
    }
}

/// `Vec<Bark>`, `shep barks`' payload, newest last as it sits on disk and as
/// `--tail` counts from.
///
/// Never built from a `Response`: `barks` reads `barks.jsonl` directly, so
/// the history survives the shepherd.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct BarkRows(pub Vec<Bark>);

impl Render for BarkRows {
    fn headers() -> &'static [&'static str] {
        &["WHEN", "RULE", "SUBJECT", "MESSAGE", "SINKS"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|b| {
                vec![
                    super::local_timestamp(b.at_ms),
                    b.rule.clone(),
                    b.subject.clone(),
                    b.message.clone(),
                    sinks_cell(&b.sinks),
                ]
            })
            .collect()
    }

    /// SINKS alone: `Meadow` when every sink took the bark, `Bark` when the
    /// cell carries [`sinks_cell`]'s `(failed)`, the dash rule when there
    /// were no sinks.
    ///
    /// WHEN stays plain although ID elsewhere is chrome: a timestamp is what
    /// an operator scans an alert feed by.
    fn rows_for(&self, presentation: Presentation, status_word: bool) -> Vec<Vec<String>> {
        let rows = self.rows();
        paint(
            rows,
            Self::headers(),
            presentation,
            status_word,
            |header, cell, _index| match (header, cell) {
                ("SINKS", "-") => Paint::Default,
                ("SINKS", sinks) if sinks.contains("(failed)") => Paint::Role(Role::Bark),
                ("SINKS", _) => Paint::Role(Role::Meadow),
                _ => Paint::Default,
            },
        )
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "WHEN" => "at_ms",
            "RULE" => "rule",
            "SUBJECT" => "subject",
            "MESSAGE" => "message",
            "SINKS" => "sinks",
            other => panic!("BarkRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Parallel to `headers()`. MESSAGE, unbounded free text, drops before
    // SINKS; both can be lost.
    const PRIORITIES: &'static [u8] = &[0, 0, 0, 7, 6];
}

/// Renders one [`Bark::sinks`] list for the `SINKS` column: `ops` for a
/// delivered sink, `ops(failed)` for a refused one. Never the sink's own
/// error text, which can quote a webhook's HTTP response; `--format json`
/// carries that in full.
///
/// `-` for an empty list, which per [`Bark::sinks`] means the shepherd wrote
/// the record itself.
fn sinks_cell(sinks: &[SinkOutcome]) -> String {
    if sinks.is_empty() {
        return "-".to_string();
    }
    sinks
        .iter()
        .map(|outcome| {
            if outcome.error.is_some() {
                format!("{}(failed)", outcome.sink)
            } else {
                outcome.sink.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One row of `shep get`'s whole-store listing.
///
/// A named-field struct rather than a `(String, String)`: a tuple serializes
/// to a JSON array, and a consumer should read `key`/`value` by name.
#[derive(Debug, Serialize)]
pub struct KvEntry {
    /// The key, exactly as stored; [`shep_core::kv`]'s grammar has already
    /// validated it.
    pub key: String,
    /// Its value.
    pub value: String,
}

/// `shep get`'s whole-store listing (bare `shep get`), or one key's own entry
/// (`shep get <key>`).
///
/// `transparent`, so the JSON is a plain array of [`KvEntry`] objects rather
/// than the map a consumer would have to special-case. Never from a
/// `Response`: the store never touches the wire.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct KvRows(pub Vec<KvEntry>);

/// No colour: a key and a value are operator data, and shep has no opinion
/// about either.
impl Render for KvRows {
    fn headers() -> &'static [&'static str] {
        &["KEY", "VALUE"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|entry| vec![entry.key.clone(), entry.value.clone()])
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "KEY" => "key",
            "VALUE" => "value",
            other => panic!("KvRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // Two columns: a key with no value is not a row at all.
    const PRIORITIES: &'static [u8] = &[0, 0];
}

/// `shep dogs --available`'s community-index listing.
///
/// Never from a `Response`: the community index never touches the daemon
/// wire. `dog_index` is the security boundary that sanitises every string
/// [`AvailableDog`] carries, so this impl clones fields through unescaped.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct AvailableDogRows(pub Vec<AvailableDog>);

/// No colour: a catalogue of dogs an operator could adopt, with no signal
/// column for chrome such as CATEGORY to recede behind.
impl Render for AvailableDogRows {
    fn headers() -> &'static [&'static str] {
        &["NAME", "PACKAGE", "CATEGORY", "DESCRIPTION"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|dog| {
                vec![
                    dog.name.clone(),
                    dog.package.clone(),
                    dog.category.clone(),
                    dog.description.clone(),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "NAME" => "name",
            "PACKAGE" => "package",
            "CATEGORY" => "category",
            "DESCRIPTION" => "description",
            other => panic!("AvailableDogRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // The name an adopt line is built from; a table has no room to say
        // how it differs from NAME.
        "adopt_as",
        // Long and rarely glanced at in a row; the detail view is where an
        // operator reads them.
        "repo", "license",
        // The tagged `DogSourceKind` the detail view's install line is built
        // from.
        "source",
    ];

    // NAME and PACKAGE identify a row, so both sit at the floor. DESCRIPTION,
    // the unbounded free-text field, drops before CATEGORY.
    const PRIORITIES: &'static [u8] = &[0, 0, 6, 7];
}

/// `shep unset`'s own report: how many keys the store lost.
///
/// A count rather than the keys: `shep_core::kv::clear` never materializes
/// the set it empties, so a single key and `--all` share one shape.
#[derive(Debug, Serialize)]
pub struct KvUnsetRow {
    /// How many keys were removed: always `1` for a single-key `unset`, which
    /// exits [`crate::exit::ExitCode::NotFound`] when the key was absent, and
    /// `shep_core::kv::clear`'s own count for `--all`.
    pub removed: u32,
}

/// No colour, for [`DeletedIds`]' reason: one column, which is also the whole
/// content. `removed` is a count rather than an outcome, and `0` reads as
/// `0`.
impl Render for KvUnsetRow {
    fn headers() -> &'static [&'static str] {
        &["REMOVED"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.removed.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "REMOVED" => "removed",
            other => panic!("KvUnsetRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];

    // One column, and it is the row's whole identity.
    const PRIORITIES: &'static [u8] = &[0];
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeSet;

    use shep_core::status::ProcStatus;

    use super::*;

    pub(crate) fn sample_info(id: u32, name: &str, uptime_ms: u64) -> ProcessInfo {
        // Every `Option` field `Some`: `assert_no_drift` skips a `null`, so a
        // field left empty here is a column it stops watching. `dog` is the
        // exception, since every row here is a sheep.
        ProcessInfo::builder(id, name, ProcStatus::Online)
            .pid(Some(1000 + id))
            .restarts(id)
            .uptime_ms(uptime_ms)
            .fold(Some("backend".to_string()))
            .out_file(Some(format!("/logs/{name}-0-out.log")))
            .err_file(Some(format!("/logs/{name}-0-err.log")))
            // Not a round number of MiB, so `human_bytes` renders "48.1M".
            .cpu_percent(Some(12.5))
            .memory_bytes(Some(50_462_720))
            // Populated for the "every `Option` field `Some`" reason above;
            // every row here is running, so the cell reads `-` regardless.
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            // The literal a real dog paints.
            .smit(Some("\u{25b2} main@a1b2c3".to_string()))
            // `cfg_cell` shows `pending` over `overridden` when both are set,
            // so this fixture cannot exercise `overridden`'s cell text; it is
            // `JSON_ONLY` anyway.
            .pending(Some(vec!["env".to_string()]))
            .overridden(Some(vec!["cwd".to_string()]))
            .build()
    }

    /// Three fully-populated sheep, shared by every test in this module and
    /// by `output`'s own envelope/emit tests.
    pub(crate) fn sample_flock() -> FlockRows {
        FlockRows(vec![
            sample_info(1, "web", 60_000),
            sample_info(2, "worker", 120_000),
            sample_info(3, "cron", 30_000),
        ])
    }

    pub(crate) fn info_with_uptime_ms(uptime_ms: u64) -> ProcessInfo {
        sample_info(1, "web", uptime_ms)
    }

    /// A dog-shaped `ProcessInfo`: `sample_info` with `dog` set to `source`.
    pub(crate) fn dog_info(name: &str, source: DogSource) -> ProcessInfo {
        let mut info = sample_info(1, name, 60_000);
        info.dog = Some(source);
        info
    }

    /// The anti-drift gate, once per payload type with JSON object keys
    /// (`DeletedIds` has none; see its own test below).
    ///
    /// Checks a fully-populated value's JSON keys against `headers()` after
    /// `json_key_for`, every row's cell count against `headers().len()`, and
    /// each non-`formatted` cell against its own JSON value, which is what
    /// catches two same-arity cells swapped.
    ///
    /// `formatted` lists headers whose cell is a human rendering rather than
    /// the field's raw value.
    fn assert_no_drift<T: Render>(
        value: &T,
        first_record: fn(&serde_json::Value) -> &serde_json::Value,
        formatted: &[&str],
    ) {
        let json = serde_json::to_value(value).unwrap();
        let record = first_record(&json);
        let keys: BTreeSet<&str> = record
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();

        let covered: BTreeSet<&str> = T::headers()
            .iter()
            .map(|h| T::json_key_for(h))
            .chain(T::JSON_ONLY.iter().copied())
            .collect();

        assert_eq!(
            keys, covered,
            "a serialized field is a column, or it is in JSON_ONLY with a reason — never neither"
        );

        let rows = value.rows();
        for row in &rows {
            assert_eq!(
                row.len(),
                T::headers().len(),
                "a row has {} cells but headers() has {} — a dropped or added cell changes no \
                 row *count*, so table_and_json_report_the_same_record_count would miss it",
                row.len(),
                T::headers().len(),
            );
        }

        let Some(row) = rows.first() else {
            return;
        };
        for (i, header) in T::headers().iter().enumerate() {
            if formatted.contains(header) {
                continue;
            }
            let key = T::json_key_for(header);
            let expected = match &record[key] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                // A `None`-carrying fixture is skipped, not a failure.
                serde_json::Value::Null => continue,
                other => panic!(
                    "{header} ({key}) serialized to {other:?}; teach this match how to \
                     stringify it, or add {header} to `formatted`"
                ),
            };
            assert_eq!(
                row[i], expected,
                "{header} cell does not match its own JSON field {key:?} — swapped or \
                 substituted with a neighbouring column?"
            );
        }
    }

    #[test]
    fn flock_rows_do_not_drift() {
        // UPTIME/CPU/MEM are formatted, EXIT's JSON value is a nested object,
        // and CFG is a summary of two fields.
        assert_no_drift(
            &sample_flock(),
            |j| &j[0],
            &["UPTIME", "CPU", "MEM", "EXIT", "CFG"],
        );
    }

    /// `sample_flock` cannot exercise this: every row there is running, so
    /// its cell is always `-`.
    #[test]
    fn the_exit_column_shows_the_last_exit_only_for_a_sheep_that_is_not_running() {
        let headers = FlockRows::headers();
        let at = |cells: &[String], h: &str| {
            cells[headers.iter().position(|x| *x == h).unwrap()].clone()
        };

        // Never exited: no pid, no `last_exit`.
        let never_run = ProcessInfo::builder(1, "fresh", ProcStatus::Stopped).build();
        // Exited with a code: no pid, `last_exit` carries one.
        let crashed = ProcessInfo::builder(2, "crashed", ProcStatus::Errored)
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();
        // Killed by a signal: no pid, `last_exit` carries one.
        let killed = ProcessInfo::builder(3, "killed", ProcStatus::Stopped)
            .last_exit(Some(ExitInfo {
                code: None,
                signal: Some(9),
            }))
            .build();
        // Running again after a past exit: `last_exit` is sticky across a
        // respawn, but a live pid leaves this column nothing to say.
        let running_again = ProcessInfo::builder(4, "recovered", ProcStatus::Online)
            .pid(Some(4242))
            .last_exit(Some(ExitInfo {
                code: Some(1),
                signal: None,
            }))
            .build();

        let rows = FlockRows(vec![never_run, crashed, killed, running_again]).rows();
        assert_eq!(at(&rows[0], "EXIT"), "-");
        assert_eq!(at(&rows[1], "EXIT"), "1");
        #[cfg(unix)]
        assert_eq!(at(&rows[2], "EXIT"), "SIGKILL");
        // `signal_label`'s Windows arm is a bare number.
        #[cfg(not(unix))]
        assert_eq!(at(&rows[2], "EXIT"), "9");
        assert_eq!(at(&rows[3], "EXIT"), "-");
    }

    /// A pending field an operator cannot see is a silent divergence.
    #[test]
    fn the_cfg_cell_marks_a_sheep_with_pending_config() {
        let mut info = sample_info(1, "web", 60_000);
        info.pending = Some(vec!["env".to_string()]);
        assert_eq!(
            cfg_cell(info.pending.as_deref(), info.overridden.as_deref()),
            "!1"
        );

        let clean = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
        assert_eq!(
            cfg_cell(clean.pending.as_deref(), clean.overridden.as_deref()),
            "-"
        );
    }

    #[test]
    fn lamb_rows_do_not_drift() {
        assert_no_drift(
            &LambRows(vec![Lamb::new(4243, "node"), Lamb::new(4244, "sh")]),
            |j| &j[0],
            &[],
        );
    }

    /// A path is wider than every other column combined and would push
    /// UPTIME off a terminal. It is still one `--format json` away.
    #[test]
    fn the_source_column_names_a_kind_and_leaves_the_path_to_json() {
        let rows = DogRows(vec![
            dog_info("metrics", DogSource::BuiltIn),
            dog_info(
                "otel",
                DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
            ),
        ]);
        let headers = DogRows::headers();
        let at = |cells: &[String], h: &str| {
            cells[headers.iter().position(|x| *x == h).unwrap()].clone()
        };
        assert_eq!(at(&rows.rows()[0], "SOURCE"), "built-in");
        assert_eq!(at(&rows.rows()[1], "SOURCE"), "adopted");

        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json[1]["dog"]["path"], "/usr/local/bin/shep-otel");
    }

    /// `SOURCE`'s JSON value is the tagged `DogSource` object; `EXIT`'s is
    /// nested too.
    #[test]
    fn dog_rows_do_not_drift() {
        assert_no_drift(
            &DogRows(vec![dog_info("metrics", DogSource::BuiltIn)]),
            |j| &j[0],
            &["UPTIME", "CPU", "MEM", "SOURCE", "EXIT"],
        );
    }

    /// `SOURCE` is `formatted` for the reason `dog_rows_do_not_drift` gives.
    #[test]
    fn dog_enabled_row_does_not_drift() {
        assert_no_drift(
            &DogEnabledRow {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
                shepherd_acted: true,
                status: "online".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// The `disable` sibling of `dog_enabled_row_does_not_drift`.
    #[test]
    fn dog_disabled_row_does_not_drift() {
        assert_no_drift(
            &DogDisabledRow {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
                shepherd_acted: false,
                status: "not running; will not start with the next shepherd".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// The `adopt` sibling of `dog_enabled_row_does_not_drift`.
    #[test]
    fn dog_adopted_row_does_not_drift() {
        assert_no_drift(
            &DogAdoptedRow {
                name: "otel".to_string(),
                source: DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                },
                shepherd_acted: true,
                status: "online".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// The `rehome` sibling, once with a recorded source and once with
    /// `None`, which passes through `assert_no_drift`'s `Value::Null` branch.
    #[test]
    fn dog_rehomed_row_does_not_drift_with_or_without_a_source() {
        assert_no_drift(
            &DogRehomedRow {
                name: "otel".to_string(),
                source: Some(DogSource::Adopted {
                    path: "/usr/local/bin/shep-otel".to_string(),
                }),
                shepherd_acted: true,
                status: "stopped".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
        assert_no_drift(
            &DogRehomedRow {
                name: "ghost".to_string(),
                source: None,
                shepherd_acted: false,
                status: "not running; will not start with the next shepherd".to_string(),
            },
            |j| j,
            &["SOURCE"],
        );
    }

    /// A zero is a claim, "this sheep is using no CPU", and the daemon says
    /// `None` precisely when it cannot make that claim.
    #[test]
    fn a_sheep_with_no_reading_renders_a_dash_not_a_zero() {
        let mut info = sample_info(1, "web", 60_000);
        info.cpu_percent = None;
        info.memory_bytes = None;
        let rows = FlockRows(vec![info]);
        let cells = &rows.rows()[0];
        let headers = FlockRows::headers();
        let cpu = cells[headers.iter().position(|h| *h == "CPU").unwrap()].clone();
        let mem = cells[headers.iter().position(|h| *h == "MEM").unwrap()].clone();
        assert_eq!(cpu, "-");
        assert_eq!(mem, "-");
    }

    /// Boxes on, so `rows_for` takes the grouping branch, and `NO_COLOR` set
    /// so cells compare as literal text.
    fn full_presentation() -> Presentation {
        use crate::style::StyleLevel;
        Presentation::new(
            StyleLevel::Full,
            Some(std::ffi::OsStr::new("1")),
            None,
            None,
            200,
        )
    }

    /// Boxes off, so `rows_for` takes the flat, suffixed branch instead.
    fn bare_presentation() -> Presentation {
        use crate::style::StyleLevel;
        Presentation::new(StyleLevel::Bare, None, None, None, 200)
    }

    #[test]
    fn a_single_instance_app_is_untouched_by_grouping() {
        let rows = FlockRows(vec![
            ProcessInfo::builder(4, "api", ProcStatus::Online)
                .instance(Some(0))
                .build(),
        ]);
        let rendered = rows.rows_for(full_presentation(), true);
        assert_eq!(rendered.len(), 1, "no group row for one instance");
        assert_eq!(rendered[0][1], "api", "and no suffix");
    }

    #[test]
    fn a_multi_instance_app_gets_a_group_row_then_its_slots() {
        let rows = FlockRows(
            (0..3)
                .map(|slot| {
                    ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                        .instance(Some(slot))
                        .build()
                })
                .collect(),
        );
        let rendered = rows.rows_for(full_presentation(), true);
        assert_eq!(rendered.len(), 4, "one group row plus three slots");
        assert_eq!(rendered[0][0], "", "the group row has no id");
        assert!(rendered[0][1].contains("web"), "{:?}", rendered[0]);
        assert!(
            rendered[0][1].contains('3'),
            "and the count: {:?}",
            rendered[0]
        );
        assert_eq!(rendered[1][0], "1", "slot rows keep their ids");
    }

    /// `BTreeMap` keys on the status word, so the order is alphabetical.
    #[test]
    fn a_mixed_group_says_so_rather_than_picking_a_winner() {
        let rows = FlockRows(vec![
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .instance(Some(0))
                .build(),
            ProcessInfo::builder(2, "web", ProcStatus::Stopped)
                .instance(Some(1))
                .build(),
            ProcessInfo::builder(3, "web", ProcStatus::Online)
                .instance(Some(2))
                .build(),
        ]);
        let rendered = rows.rows_for(full_presentation(), true);
        assert_eq!(rendered[0][2], "2 online, 1 stopped");
    }

    /// The slots are listed oldest first, so the shortest is last.
    #[test]
    fn a_group_uptime_is_the_shortest_of_its_slots() {
        let rows = FlockRows(
            [9_000_000_u64, 4_512_000, 300_000]
                .into_iter()
                .enumerate()
                .map(|(slot, uptime_ms)| {
                    let slot = u32::try_from(slot).unwrap();
                    ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                        .instance(Some(slot))
                        .uptime_ms(uptime_ms)
                        .build()
                })
                .collect(),
        );
        let rendered = rows.rows_for(full_presentation(), true);
        assert_eq!(rendered[0][9], "5m", "300_000ms, the shortest of the three");
    }

    /// A zero is a claim, and the sum of nothing is no claim: the fold starts
    /// at `None` rather than `0`.
    #[test]
    fn a_group_with_no_readings_shows_a_dash_not_a_zero() {
        let rows = FlockRows(
            (0..2)
                .map(|slot| {
                    ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                        .instance(Some(slot))
                        .cpu_percent(None)
                        .memory_bytes(None)
                        .build()
                })
                .collect(),
        );
        let rendered = rows.rows_for(full_presentation(), true);
        assert_eq!(rendered[0][7], "-", "cpu");
        assert_eq!(rendered[0][8], "-", "mem");

        // One live reading among absent ones is still a claim: the fold
        // leaves `None` only when no slot reported.
        let mixed = FlockRows(vec![
            ProcessInfo::builder(1, "web", ProcStatus::Online)
                .instance(Some(0))
                .cpu_percent(Some(2.5))
                .memory_bytes(Some(64 << 20))
                .build(),
            ProcessInfo::builder(2, "web", ProcStatus::Online)
                .instance(Some(1))
                .cpu_percent(None)
                .memory_bytes(None)
                .build(),
        ]);
        let rendered = mixed.rows_for(full_presentation(), true);
        assert_eq!(rendered[0][7], "2.5%", "cpu");
        assert_eq!(rendered[0][8], "64.0M", "mem");
    }

    /// The two rollups are not shared code, so cells are compared across the
    /// surfaces. Both are anchored at one instant, so the lookout's live
    /// uptime is the reported one.
    #[test]
    fn the_flock_table_and_the_lookout_roll_a_group_up_the_same_way() {
        use std::time::Instant;

        use crate::lookout::app::{App, Control, Msg, RowKey};
        use crate::lookout::theme::Palette;
        use crate::lookout::view::flock::{columns_for, key_line};

        // Every slot differs in every summed field, so a rollup reading one
        // member cannot coincide with the sum.
        let flock: Vec<ProcessInfo> = [
            (0_u32, 0_u32, 3.4_f32, 182_u64 << 20, 4_512_000_u64),
            (1, 2, 2.9, 178 << 20, 300_000),
            (2, 1, 3.1, 180 << 20, 9_000_000),
        ]
        .into_iter()
        .map(|(slot, restarts, cpu, memory, uptime_ms)| {
            ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                .instance(Some(slot))
                .pid(Some(48_400 + slot))
                .restarts(restarts)
                .cpu_percent(Some(cpu))
                .memory_bytes(Some(memory))
                .uptime_ms(uptime_ms)
                .build()
        })
        .collect();

        let t0 = Instant::now();
        let mut app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        app.update(Msg::Snapshot {
            rows: flock.clone(),
            at: t0,
        });

        // The summing rules themselves, ahead of any rendering.
        let table_totals = group_totals(&flock);
        let dashboard_totals = app.group_totals("web");
        assert_eq!(dashboard_totals.count, flock.len());
        assert_eq!(dashboard_totals.restarts, table_totals.restarts, "restarts");
        assert_eq!(dashboard_totals.cpu, table_totals.cpu, "cpu");
        assert_eq!(dashboard_totals.memory, table_totals.memory, "memory");
        assert_eq!(
            dashboard_totals.uptime_ms,
            Some(table_totals.uptime_ms),
            "uptime"
        );
        assert_eq!(
            app.group_status_text("web"),
            group_status(&flock),
            "a uniform group's status word"
        );

        // The mixed case has a format to disagree about, not one word.
        let mut mixed = flock.clone();
        mixed[1].status = ProcStatus::Stopped;
        let mut mixed_app = App::new(
            Palette::detect(None, None, None),
            Control::ReadOnly,
            "/home/ada/.shep".to_string(),
            t0,
        );
        mixed_app.update(Msg::Snapshot {
            rows: mixed.clone(),
            at: t0,
        });
        assert_eq!(
            mixed_app.group_status_text("web"),
            group_status(&mixed),
            "a mixed group's per-state counts"
        );

        let table = FlockRows(flock).rows_for(full_presentation(), true);
        let header = &table[0];
        let dashboard = key_line(
            &app,
            &RowKey::Group("web".to_string()),
            columns_for(200),
            200,
        )
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();

        // Then the rendered cells. FOLD and SMIT are per-app facts, not sums;
        // STATUS carries a face here and not in the dashboard.
        for column in ["NAME", "RESTARTS", "CPU", "MEM", "UPTIME"] {
            let at = FlockRows::headers()
                .iter()
                .position(|header| *header == column)
                .expect("the column is in the table");
            let cell = header[at].trim();
            assert!(
                dashboard.contains(cell),
                "`shep flock` rolls {column} up to {cell:?} and `shep lookout` \
                 does not agree: {dashboard:?}"
            );
        }
    }

    #[test]
    fn a_flat_style_suffixes_the_name_instead_of_grouping() {
        let rows = FlockRows(
            (0..2)
                .map(|slot| {
                    ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                        .instance(Some(slot))
                        .build()
                })
                .collect(),
        );
        let rendered = rows.rows_for(bare_presentation(), true);
        assert_eq!(rendered.len(), 2, "one line per process, still greppable");
        assert_eq!(rendered[0][1], "web:0");
        assert_eq!(rendered[1][1], "web:1");
    }

    /// The same suffix through `render_table`, which calls `Self::rows`
    /// directly. The test above asserts on `rows_for` and cannot reach it.
    #[test]
    fn the_bare_path_reaches_the_suffix_through_rows_not_rows_for() {
        let rows = FlockRows(
            (0..2)
                .map(|slot| {
                    ProcessInfo::builder(slot + 1, "web", ProcStatus::Online)
                        .instance(Some(slot))
                        .build()
                })
                .collect(),
        );
        let rendered = crate::output::render_table(&rows);
        assert!(rendered.contains("web:0"), "{rendered}");
        assert!(rendered.contains("web:1"), "{rendered}");
    }

    #[test]
    fn a_row_from_an_older_daemon_renders_exactly_as_it_did_before() {
        let rows = FlockRows(vec![
            ProcessInfo::builder(1, "web", ProcStatus::Online).build(),
            ProcessInfo::builder(2, "web", ProcStatus::Online).build(),
        ]);
        let rendered = rows.rows_for(full_presentation(), true);
        assert_eq!(rendered.len(), 2, "no slots, so no grouping");
        assert_eq!(rendered[0][1], "web", "and no suffix");
    }

    /// The lifecycle keys are off the table only because
    /// [`FlushedRows::JSON_ONLY`] names them.
    #[test]
    fn flushed_rows_do_not_drift() {
        assert_no_drift(&FlushedRows(sample_flock().0), |j| &j[0], &[]);
    }

    /// A `--format json` parser must not need a case keyed on the envelope's
    /// `command`.
    #[test]
    fn a_flush_serializes_the_same_record_the_other_flock_verbs_do() {
        let flock = serde_json::to_value(sample_flock()).unwrap();
        let flushed = serde_json::to_value(FlushedRows(sample_flock().0)).unwrap();
        assert_eq!(
            flock, flushed,
            "the table may differ between these two verbs; the JSON payload may not"
        );
    }

    #[test]
    fn emptied_files_do_not_drift() {
        assert_no_drift(
            &EmptiedFiles(vec![
                EmptiedFile {
                    stream: "stdout",
                    file: "/home/x/.shep/logs/shepd.out.log".to_string(),
                    result: "emptied",
                },
                EmptiedFile {
                    stream: "stderr",
                    file: "/home/x/.shep/logs/shepd.err.log".to_string(),
                    result: "absent",
                },
            ]),
            |j| &j[0],
            &[],
        );
    }

    #[test]
    fn kill_row_does_not_drift() {
        assert_no_drift(
            &KillRow {
                pid: 4242,
                socket_removed: true,
            },
            |j| j,
            &[],
        );
    }

    #[test]
    fn saved_roll_row_does_not_drift() {
        let row = SavedRollRow {
            file: "/home/ada/.shep/flock.json".to_string(),
            apps: 9,
        };
        assert_no_drift(&row, |json| json, &[]);
    }

    #[test]
    fn import_rows_do_not_drift() {
        assert_no_drift(
            &ImportRows(vec![
                ImportRow {
                    name: "api".to_string(),
                    script: "/srv/api/dist/server.js".to_string(),
                    instances: 2,
                    reuse_port: true,
                },
                ImportRow {
                    name: "worker".to_string(),
                    script: "/srv/worker/dist/worker.js".to_string(),
                    instances: 1,
                    reuse_port: false,
                },
            ]),
            |j| &j[0],
            &[],
        );
    }

    /// The two rows cover both shapes the payload carries: a file that was
    /// written, and a command that was run and failed.
    #[test]
    fn startup_steps_do_not_drift() {
        assert_no_drift(
            &StartupSteps(vec![
                StartupStep {
                    action: "wrote",
                    target: "/etc/systemd/system/shep-deploy.service".to_string(),
                    result: "ok".to_string(),
                },
                StartupStep {
                    action: "ran",
                    target: "systemctl enable --now shep-deploy.service".to_string(),
                    result: "Failed to enable unit: Unit file is masked.".to_string(),
                },
            ]),
            |j| &j[0],
            &[],
        );
    }

    /// `DeletedIds` serializes as a bare array, so `assert_no_drift` has no
    /// object keys to compare. This is its drift coverage instead.
    #[test]
    fn deleted_ids_rows_match_their_own_json_values() {
        let ids = DeletedIds(vec![10, 20, 30]);
        let json = serde_json::to_value(&ids).unwrap();
        let array = json.as_array().unwrap();
        let rows = ids.rows();

        assert_eq!(rows.len(), array.len());
        for (row, value) in rows.iter().zip(array) {
            assert_eq!(row.len(), 1, "DeletedIds::headers() has exactly one column");
            assert_eq!(row[0], value.to_string());
        }
    }

    #[test]
    fn table_and_json_report_the_same_record_count() {
        let rows = sample_flock(); // three sheep
        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 3);
        assert_eq!(
            rows.rows().len(),
            3,
            "the two renderings must never disagree on how many records exist"
        );

        let ids = DeletedIds(vec![1, 2, 3, 4]);
        assert_eq!(
            serde_json::to_value(&ids)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(ids.rows().len(), 4);
    }

    fn sample_replies() -> TriggeredRows {
        TriggeredRows(vec![
            ActionReply {
                id: 1,
                name: "web".to_string(),
                outcome: ActionOutcome::Replied {
                    body: "pong".to_string(),
                },
            },
            ActionReply {
                id: 2,
                name: "worker".to_string(),
                outcome: ActionOutcome::NoChannel,
            },
        ])
    }

    /// OUTCOME and DETAIL both derive from `outcome`, a nested object, so
    /// both sit in `assert_no_drift`'s `formatted` list. Its key and
    /// cell-count checks still run.
    #[test]
    fn triggered_rows_do_not_drift() {
        assert_no_drift(&sample_replies(), |j| &j[0], &["OUTCOME", "DETAIL"]);
    }

    #[test]
    fn triggered_rows_render_id_name_and_outcome_kind() {
        let rows = sample_replies().rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "1");
        assert_eq!(rows[0][1], "web");
        assert_eq!(rows[0][2], "replied");
        assert_eq!(rows[1][0], "2");
        assert_eq!(rows[1][1], "worker");
        assert_eq!(rows[1][2], "no_channel");
    }

    /// An operator reading a `no_channel` row must find the config field
    /// that would have avoided it in the row itself, not only in `--help`.
    #[test]
    fn a_no_channel_detail_names_the_config_field() {
        let rows = sample_replies().rows();
        let detail = &rows[1][3];
        assert!(
            detail.contains("channel = true"),
            "a no_channel row must name the field that opens one: {detail}"
        );
        assert!(
            detail.contains("wait_ready") && detail.contains("shutdown_with_message"),
            "and the two fields that imply it: {detail}"
        );
    }

    #[test]
    fn skipped_and_timed_out_details_say_why() {
        let skipped = describe_outcome(&ActionOutcome::Skipped).1;
        assert!(skipped.to_lowercase().contains("reload"), "{skipped}");

        let timed_out = describe_outcome(&ActionOutcome::TimedOut).1;
        assert!(
            timed_out.to_lowercase().contains("action_timeout"),
            "{timed_out}"
        );
    }

    #[test]
    fn a_short_single_line_body_previews_unchanged() {
        assert_eq!(preview_body("pong"), "pong");
    }

    /// [`preview_body`]'s `seen == TRIGGER_BODY_PREVIEW_CHARS` check fires
    /// one character late, so only a body past the cap is truncated.
    #[test]
    fn a_body_exactly_at_the_cap_is_not_truncated() {
        let exact = "x".repeat(TRIGGER_BODY_PREVIEW_CHARS);
        assert_eq!(preview_body(&exact), exact);
    }

    #[test]
    fn a_body_past_the_cap_is_truncated_with_a_trailing_marker() {
        let over = "x".repeat(TRIGGER_BODY_PREVIEW_CHARS + 1);
        let preview = preview_body(&over);
        let expected = "x".repeat(TRIGGER_BODY_PREVIEW_CHARS) + "...";
        assert_eq!(preview, expected);
    }

    /// A multi-line body would otherwise split a table row across output
    /// lines (`TriggeredRows::rows`).
    #[test]
    fn embedded_newlines_and_carriage_returns_are_escaped_not_literal() {
        let preview = preview_body("line one\nline two\r\nline three");
        assert!(!preview.contains('\n'));
        assert!(!preview.contains('\r'));
        assert!(preview.contains("\\n"));
        assert!(preview.contains("\\r"));
    }

    /// Fails if truncation or escaping leaks into `Serialize` instead of
    /// staying in [`TriggeredRows::rows`].
    #[test]
    fn json_carries_the_real_body_the_table_cannot() {
        let long_body = format!(
            "{}\nsecond line",
            "x".repeat(TRIGGER_BODY_PREVIEW_CHARS * 2)
        );
        let replies = TriggeredRows(vec![ActionReply {
            id: 1,
            name: "web".to_string(),
            outcome: ActionOutcome::Replied {
                body: long_body.clone(),
            },
        }]);
        let json = serde_json::to_value(&replies).unwrap();
        assert_eq!(json[0]["outcome"]["body"], long_body);

        let table_cell = &replies.rows()[0][3];
        assert_ne!(
            *table_cell, long_body,
            "the table cell must be the collapsed preview, not the real body"
        );
    }

    fn sample_signal_replies() -> SignalledRows {
        SignalledRows(vec![
            SignalReply {
                id: 1,
                name: "web".to_string(),
                outcome: SignalOutcome::Delivered,
            },
            SignalReply {
                id: 2,
                name: "worker".to_string(),
                outcome: SignalOutcome::NotRunning,
            },
        ])
    }

    /// OUTCOME and DETAIL both derive from `outcome`, a nested JSON object
    /// rather than a scalar, as in `triggered_rows_do_not_drift`.
    #[test]
    fn signalled_rows_do_not_drift() {
        assert_no_drift(&sample_signal_replies(), |j| &j[0], &["OUTCOME", "DETAIL"]);
    }

    #[test]
    fn signalled_rows_render_id_name_and_outcome_kind() {
        let rows = sample_signal_replies().rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "1");
        assert_eq!(rows[0][1], "web");
        assert_eq!(rows[0][2], "delivered");
        assert_eq!(rows[1][0], "2");
        assert_eq!(rows[1][1], "worker");
        assert_eq!(rows[1][2], "not_running");
    }

    #[test]
    fn a_failed_signal_details_the_kernels_reason() {
        let rows = SignalledRows(vec![SignalReply {
            id: 1,
            name: "web".to_string(),
            outcome: SignalOutcome::Failed {
                reason: "No such process".to_string(),
            },
        }])
        .rows();
        assert_eq!(rows[0][2], "failed");
        assert_eq!(rows[0][3], "No such process");
    }

    fn sample_line_replies() -> SentLineRows {
        SentLineRows(vec![
            LineReply {
                id: 1,
                name: "repl".to_string(),
                outcome: LineOutcome::Sent,
            },
            LineReply {
                id: 2,
                name: "worker".to_string(),
                outcome: LineOutcome::NoStdin,
            },
        ])
    }

    /// OUTCOME and DETAIL both derive from `outcome`, a nested JSON object
    /// rather than a scalar, as in `triggered_rows_do_not_drift`.
    #[test]
    fn sent_line_rows_do_not_drift() {
        assert_no_drift(&sample_line_replies(), |j| &j[0], &["OUTCOME", "DETAIL"]);
    }

    #[test]
    fn sent_line_rows_render_id_name_and_outcome_kind() {
        let rows = sample_line_replies().rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "1");
        assert_eq!(rows[0][1], "repl");
        assert_eq!(rows[0][2], "sent");
        assert_eq!(rows[1][0], "2");
        assert_eq!(rows[1][1], "worker");
        assert_eq!(rows[1][2], "no_stdin");
    }

    /// The `whisper` sibling of `a_no_channel_detail_names_the_config_field`.
    #[test]
    fn a_no_stdin_detail_names_the_config_field() {
        let rows = sample_line_replies().rows();
        let detail = &rows[1][3];
        assert!(
            detail.contains("stdin = true"),
            "a no_stdin row must name the field that opens one: {detail}"
        );
    }

    #[test]
    fn a_not_written_line_details_the_reason() {
        let rows = SentLineRows(vec![LineReply {
            id: 1,
            name: "repl".to_string(),
            outcome: LineOutcome::NotWritten {
                reason: "pipe is full".to_string(),
            },
        }])
        .rows();
        assert_eq!(rows[0][2], "not_written");
        assert_eq!(rows[0][3], "pipe is full");
    }

    /// One bark delivered to a live sink and one the shepherd wrote itself
    /// with no sinks, shared by every test below.
    fn sample_barks() -> BarkRows {
        BarkRows(vec![
            Bark {
                at_ms: 1_700_000_000_000,
                rule: "restart-storm".to_string(),
                subject: "web".to_string(),
                message: "3 restarts in 60s".to_string(),
                sinks: vec![SinkOutcome {
                    sink: "ops".to_string(),
                    error: None,
                }],
            },
            Bark {
                at_ms: 1_700_000_060_000,
                rule: "daemon".to_string(),
                subject: "worker".to_string(),
                message: "restart budget exhausted".to_string(),
                sinks: vec![],
            },
        ])
    }

    /// `WHEN` and `SINKS` are both human renderings of their own JSON field,
    /// so both sit in `formatted`.
    #[test]
    fn bark_rows_do_not_drift() {
        assert_no_drift(&sample_barks(), |j| &j[0], &["WHEN", "SINKS"]);
    }

    /// `sinks_cell`'s coverage: delivered, refused, and a shepherd-authored
    /// bark with no sinks at all.
    #[test]
    fn sinks_render_delivered_failed_and_empty() {
        let delivered = Bark {
            sinks: vec![SinkOutcome {
                sink: "ops".to_string(),
                error: None,
            }],
            ..sample_barks().0[0].clone()
        };
        assert_eq!(sinks_cell(&delivered.sinks), "ops");

        let failed = Bark {
            sinks: vec![SinkOutcome {
                sink: "ops".to_string(),
                error: Some("connection refused".to_string()),
            }],
            ..sample_barks().0[0].clone()
        };
        assert_eq!(sinks_cell(&failed.sinks), "ops(failed)");

        assert_eq!(sinks_cell(&[]), "-");
    }

    /// A comma-separated list, each sink carrying its own label.
    #[test]
    fn multiple_sinks_each_carry_their_own_outcome() {
        let sinks = vec![
            SinkOutcome {
                sink: "ops".to_string(),
                error: None,
            },
            SinkOutcome {
                sink: "oncall".to_string(),
                error: Some("timed out".to_string()),
            },
        ];
        assert_eq!(sinks_cell(&sinks), "ops, oncall(failed)");
    }

    /// The cell carries no more than the sink's name plus `(failed)`, never
    /// the error string, which can quote a webhook's HTTP response.
    #[test]
    fn a_failed_sinks_error_text_never_reaches_the_cell() {
        let sinks = vec![SinkOutcome {
            sink: "ops".to_string(),
            error: Some("HTTP 401 from discord.com/api/webhooks/...".to_string()),
        }];
        let cell = sinks_cell(&sinks);
        assert_eq!(cell, "ops(failed)");
        assert!(
            !cell.contains("401") && !cell.contains("discord"),
            "the error text must stay out of the table cell: {cell}"
        );
    }

    /// `shep barks` is newest-last, matching the file on disk.
    #[test]
    fn bark_rows_stay_in_the_order_they_were_given() {
        let rows = sample_barks().rows();
        assert_eq!(rows[0][2], "web", "the older bark stays first");
        assert_eq!(rows[1][2], "worker", "the newer bark stays last");
    }

    /// Neither column is a rendering of anything else, so `formatted` is
    /// empty.
    #[test]
    fn kv_rows_do_not_drift() {
        let rows = KvRows(vec![KvEntry {
            key: "bark.cooldown".to_string(),
            value: "30s".to_string(),
        }]);
        assert_no_drift(&rows, |j| &j[0], &[]);
    }

    #[test]
    fn kv_unset_row_does_not_drift() {
        assert_no_drift(&KvUnsetRow { removed: 2 }, |j| j, &[]);
    }

    /// The live index's single entry (`web/public/dogs.json`).
    fn sample_available_dog() -> AvailableDog {
        AvailableDog {
            name: "Spot".to_string(),
            package: "shep-log-rotate".to_string(),
            adopt_as: "log-rotate".to_string(),
            description: "Rotates grown log files and asks the shepherd to reopen them."
                .to_string(),
            repo: "https://github.com/shep-pm/shep-log-rotate".to_string(),
            license: "MIT OR Apache-2.0".to_string(),
            category: "logs".to_string(),
            source: crate::dog_index::DogSourceKind::CargoGit {
                url: "https://github.com/shep-pm/shep-log-rotate".to_string(),
            },
        }
    }

    /// `adopt_as`/`repo`/`license`/`source` serialize but are covered by
    /// `JSON_ONLY` rather than a column. The four real columns are plain
    /// strings, so `formatted` is empty.
    #[test]
    fn available_dog_rows_do_not_drift() {
        assert_no_drift(
            &AvailableDogRows(vec![sample_available_dog()]),
            |j| &j[0],
            &[],
        );
    }

    // --- PRIORITIES ------------------------------------------------------

    /// One `Render` impl's own check. `headers()` and `PRIORITIES` are
    /// hand-edited parallel arrays, so a header inserted without its priority
    /// shifts every later one onto the wrong column. `floor` is this type's
    /// intended never-drop set.
    fn assert_priorities_match_headers<T: Render>(floor: &[&str]) {
        let headers = T::headers();
        let priorities = T::PRIORITIES;
        assert_eq!(
            headers.len(),
            priorities.len(),
            "{}: headers() has {} columns but PRIORITIES has {} — they must move together",
            std::any::type_name::<T>(),
            headers.len(),
            priorities.len(),
        );
        let actual_floor: Vec<&str> = headers
            .iter()
            .zip(priorities)
            .filter(|&(_, &p)| p == 0)
            .map(|(&h, _)| h)
            .collect();
        assert_eq!(
            actual_floor,
            floor,
            "{}: the columns at priority 0 do not match this type's own intended floor",
            std::any::type_name::<T>(),
        );
    }

    /// The anti-drift gate for [`Render::PRIORITIES`] across every payload
    /// type this crate defines, so a table added later without a real array
    /// fails here instead of shipping the trait's all-zero default.
    #[test]
    fn priorities_line_up_with_headers_for_every_render_impl() {
        assert_priorities_match_headers::<FlockRows>(&["ID", "NAME", "STATUS"]);
        assert_priorities_match_headers::<DogRows>(&["ID", "NAME", "STATUS"]);
        assert_priorities_match_headers::<LambRows>(&["PID", "NAME"]);
        assert_priorities_match_headers::<DogEnabledRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<DogDisabledRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<DogAdoptedRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<DogRehomedRow>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<FlushedRows>(&["ID", "NAME"]);
        assert_priorities_match_headers::<EmptiedFiles>(&["STREAM", "RESULT"]);
        assert_priorities_match_headers::<DeletedIds>(&["ID"]);
        assert_priorities_match_headers::<KillRow>(&["PID", "SOCKET_REMOVED"]);
        assert_priorities_match_headers::<RolledSheepRows>(&["NAME", "STATUS"]);
        assert_priorities_match_headers::<SavedRollRow>(&["FILE", "APPS"]);
        assert_priorities_match_headers::<ImportRows>(&["NAME"]);
        assert_priorities_match_headers::<StartupSteps>(&["TARGET", "RESULT"]);
        assert_priorities_match_headers::<TriggeredRows>(&["ID", "NAME", "OUTCOME"]);
        assert_priorities_match_headers::<SignalledRows>(&["ID", "NAME", "OUTCOME"]);
        assert_priorities_match_headers::<SentLineRows>(&["ID", "NAME", "OUTCOME"]);
        assert_priorities_match_headers::<BarkRows>(&["WHEN", "RULE", "SUBJECT"]);
        assert_priorities_match_headers::<KvRows>(&["KEY", "VALUE"]);
        assert_priorities_match_headers::<KvUnsetRow>(&["REMOVED"]);
        assert_priorities_match_headers::<AvailableDogRows>(&["NAME", "PACKAGE"]);
    }

    /// The floor-set check cannot see two non-floor columns trading numbers.
    /// The flock listing's drop order is the one the spec states outright.
    #[test]
    fn the_flock_listing_drops_its_columns_in_the_documented_order() {
        let mut ranked: Vec<(&str, u8)> = FlockRows::headers()
            .iter()
            .copied()
            .zip(FlockRows::PRIORITIES.iter().copied())
            .collect();
        ranked.sort_by_key(|&(_, priority)| priority);

        let order: Vec<&str> = ranked.iter().map(|&(header, _)| header).collect();
        assert_eq!(
            order,
            vec![
                // The three that identify a sheep, and so never drop.
                "ID", "NAME", "STATUS", //
                // Then in the order they survive as the terminal narrows;
                // the give-up order is this reversed.
                "UPTIME", "PID", "MEM", "RESTARTS", "CPU", "EXIT", "CFG", "FOLD", "SMIT",
            ],
            "the flock listing's drop order changed; if that is deliberate, \
             change this test and say why in the commit"
        );
    }

    // --- Colour: MEM/CPU/RESTARTS/EXIT/ID/FOLD/placeholder roles ----------

    /// The boundary is inclusive on the `Butter` side.
    #[test]
    fn mem_role_ramps_at_its_documented_boundary() {
        assert_eq!(mem_role(None), Role::Ink3);
        assert_eq!(mem_role(Some(MEM_ELEVATED_BYTES - 1)), Role::Meadow);
        assert_eq!(mem_role(Some(MEM_ELEVATED_BYTES)), Role::Butter);
        // A light app and a heavy one must land on opposite sides.
        assert_eq!(mem_role(Some(3_800_000)), Role::Meadow, "3.8M is light");
        assert_eq!(mem_role(Some(800_000_000)), Role::Butter, "800M is heavy");
    }

    /// Idle (`0.0%`) stays `Ink3`; the boundary is inclusive on the `Butter`
    /// side.
    #[test]
    fn cpu_role_ramps_at_its_documented_boundary() {
        assert_eq!(cpu_role(None), Role::Ink3);
        assert_eq!(cpu_role(Some(0.0)), Role::Ink3);
        assert_eq!(cpu_role(Some(0.1)), Role::Meadow);
        assert_eq!(cpu_role(Some(CPU_ELEVATED_PERCENT - 0.1)), Role::Meadow);
        assert_eq!(cpu_role(Some(CPU_ELEVATED_PERCENT)), Role::Butter);
        assert_eq!(cpu_role(Some(99.0)), Role::Butter);
    }

    #[test]
    fn restarts_role_is_ink3_only_at_exactly_zero() {
        assert_eq!(restarts_role(0), Role::Ink3);
        assert_eq!(restarts_role(1), Role::Butter);
        assert_eq!(restarts_role(u32::MAX), Role::Butter);
    }

    /// A still-running sheep, a clean `0` and an uncharacterised exit are all
    /// `Ink3`.
    #[test]
    fn exit_role_is_bark_only_for_a_genuine_failure() {
        // Still running, over a `last_exit` that is still recorded.
        assert_eq!(
            exit_role(
                Some(1234),
                Some(ExitInfo {
                    code: Some(1),
                    signal: None
                })
            ),
            Role::Ink3
        );
        // Not running, no exit ever recorded.
        assert_eq!(exit_role(None, None), Role::Ink3);
        // Not running, a clean exit.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: Some(0),
                    signal: None
                })
            ),
            Role::Ink3
        );
        // Not running, the daemon could not characterize the exit.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: None,
                    signal: None
                })
            ),
            Role::Ink3
        );
        // Not running, a genuine nonzero exit code.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: Some(1),
                    signal: None
                })
            ),
            Role::Bark
        );
        // Not running, killed by a signal.
        assert_eq!(
            exit_role(
                None,
                Some(ExitInfo {
                    code: None,
                    signal: Some(9)
                })
            ),
            Role::Bark
        );
    }

    // --- Colour: the seven tables that are not the flock listing ---------
    //
    // Each assertion compares the exact painted string: a check for the mere
    // presence of an escape byte passes on a cell painted the wrong role.

    /// The 256-colour presentation these cases render at.
    fn coloured() -> Presentation {
        use crate::style::StyleLevel;
        Presentation::new(
            StyleLevel::Full,
            None,
            Some(std::ffi::OsStr::new("xterm-256color")),
            None,
            200,
        )
    }

    /// `text` as `colour_cell` would paint it for `role`.
    fn painted(text: &str, role: Role) -> String {
        let mut cell = text.to_string();
        colour_cell(&mut cell, role, coloured());
        cell
    }

    /// One dog whose readings land on a known side of every ramp: four
    /// restarts, idle CPU, and 3 MiB, below the MEM boundary.
    fn sample_dog(status: ProcStatus, pid: Option<u32>) -> ProcessInfo {
        ProcessInfo::builder(9, "log-rotate", status)
            .pid(pid)
            .restarts(4)
            .uptime_ms(41_000)
            .cpu_percent(pid.map(|_| 0.0))
            .memory_bytes(pid.map(|_| 3 * 1024 * 1024))
            .dog(Some(DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            }))
            .build()
    }

    #[test]
    fn the_sheep_and_dog_tables_share_a_column_order() {
        let sheep = FlockRows::headers();
        let dogs = DogRows::headers();

        // CFG is a sheep concept: a dog is never loaded from a Flockfile a
        // config load can park or override, so it is filtered out before the
        // shared prefix is compared.
        let sheep_without_cfg: Vec<&str> = sheep.iter().copied().filter(|h| *h != "CFG").collect();
        assert_eq!(
            sheep.iter().position(|h| *h == "CFG"),
            sheep.iter().position(|h| *h == "EXIT").map(|at| at + 1),
            "CFG sits directly after EXIT in the sheep table"
        );

        let common = [
            "ID", "NAME", "STATUS", "PID", "RESTARTS", "EXIT", "CPU", "MEM", "UPTIME",
        ];
        assert_eq!(
            &sheep_without_cfg[..common.len()],
            &common,
            "the sheep table leads with them, CFG aside"
        );
        assert_eq!(&dogs[..common.len()], &common, "and so does the dogs table");

        assert_eq!(
            &sheep_without_cfg[common.len()..],
            &["FOLD", "SMIT"],
            "the sheep table's own"
        );
        assert_eq!(&dogs[common.len()..], &["SOURCE"], "the dogs table's own");

        // FOLD and SMIT are impossible for a dog rather than empty: a dog
        // belongs to no fold, and a smit is a mark a dog paints on a sheep.
        assert!(
            DogRows::JSON_ONLY.contains(&"fold") && DogRows::JSON_ONLY.contains(&"smit"),
            "both still ride the JSON, with a reason recorded beside them"
        );
    }

    /// Compares painted cells, not roles, so it also catches a table reading
    /// the right rule off the wrong column.
    #[test]
    fn a_shared_column_is_painted_the_same_in_both_tables() {
        let mut as_sheep = sample_dog(ProcStatus::Online, Some(14_110));
        as_sheep.dog = None;
        let sheep = FlockRows(vec![as_sheep]).rows_for(coloured(), true);
        let dogs =
            DogRows(vec![sample_dog(ProcStatus::Online, Some(14_110))]).rows_for(coloured(), true);

        for (index, header) in FlockRows::headers().iter().enumerate() {
            let Some(there) = DogRows::headers().iter().position(|h| h == header) else {
                continue;
            };
            assert_eq!(
                sheep[0][index], dogs[0][there],
                "{header} renders differently in the two tables"
            );
        }
    }

    /// Driven through [`paint`] over a reversed header list, so every column
    /// sits somewhere it never sits in life.
    #[test]
    fn a_columns_colour_follows_its_name_and_not_its_position() {
        let dog = sample_dog(ProcStatus::Online, Some(14_110));
        let forwards = DogRows::headers();
        let backwards: Vec<&'static str> = forwards.iter().copied().rev().collect();

        let mut cells: Vec<String> = DogRows(vec![dog.clone()]).rows().remove(0);
        cells.reverse();
        let painted_rows = paint(vec![cells], &backwards, coloured(), true, |header, _, _| {
            process_info_paint(header, &dog)
        });

        let at = |name: &str| backwards.iter().position(|h| *h == name).unwrap();
        // Every one of these indices differs from the column's real one.
        assert_eq!(painted_rows[0][at("ID")], painted("9", Role::Ink3));
        assert_eq!(painted_rows[0][at("RESTARTS")], painted("4", Role::Butter));
        assert_eq!(painted_rows[0][at("MEM")], painted("3.0M", Role::Meadow));
        assert_eq!(painted_rows[0][at("CPU")], painted("0.0%", Role::Ink3));
        assert_eq!(
            painted_rows[0][at("SOURCE")],
            painted("adopted", Role::Butter)
        );
        assert_eq!(
            painted_rows[0][at("STATUS")],
            painted("(o.o) online", Role::Meadow)
        );
        assert_eq!(painted_rows[0][at("NAME")], "log-rotate", "still plain");
        assert_eq!(painted_rows[0][at("UPTIME")], "41s", "still plain");
    }

    /// The same reversed-header proof, pointed at every painter that is not
    /// [`process_info_paint`]: `dog_action_paint`, `reply_paint` and the four
    /// inline closures.
    #[test]
    fn every_painter_follows_the_column_name_and_not_the_position() {
        /// Paints `row` through `T`'s headers reversed, and hands the cells
        /// back in their original order.
        fn reversed<T: Render>(row: Vec<String>, paint_of: fn(&str, &str) -> Paint) -> Vec<String> {
            let backwards: Vec<&'static str> = T::headers().iter().copied().rev().collect();
            let mut cells = row;
            cells.reverse();
            let mut painted = paint(
                vec![cells],
                &backwards,
                coloured(),
                true,
                |header, cell, _index| paint_of(header, cell),
            )
            .remove(0);
            painted.reverse();
            painted
        }
        let at =
            |headers: &[&'static str], name: &str| headers.iter().position(|h| *h == name).unwrap();

        // --- the four dog-action rows, through `dog_action_paint` ---------
        let adopted = DogAdoptedRow {
            name: "log-rotate".to_string(),
            source: DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            },
            shepherd_acted: true,
            status: "online".to_string(),
        };
        let cells = reversed::<DogAdoptedRow>(adopted.rows().remove(0), dog_action_paint);
        let h = DogAdoptedRow::headers();
        assert_eq!(
            cells[at(h, "SOURCE")],
            painted("adopted", Role::Butter),
            "SOURCE decided from SOURCE, wherever it sits"
        );
        assert_eq!(
            cells[at(h, "STATUS")],
            painted("(o.o) online", Role::Meadow),
            "STATUS decided from STATUS"
        );
        assert_eq!(cells[at(h, "NAME")], "log-rotate", "NAME untouched");
        assert_eq!(cells[at(h, "SHEPHERD")], "true", "SHEPHERD untouched");

        // --- the three reply tables, through `reply_paint` ----------------
        let reply = TriggeredRows(vec![ActionReply {
            id: 0,
            name: "web".to_string(),
            outcome: ActionOutcome::TimedOut,
        }]);
        let cells = reversed::<TriggeredRows>(reply.rows().remove(0), reply_paint);
        let h = TriggeredRows::headers();
        assert_eq!(cells[at(h, "ID")], painted("0", Role::Ink3));
        assert_eq!(cells[at(h, "OUTCOME")], painted("timed_out", Role::Bark));
        assert_eq!(
            cells[at(h, "DETAIL")],
            "no reply within the app's own action_timeout",
            "DETAIL untouched, and never mistaken for the OUTCOME beside it"
        );

        // --- the inline closures -----------------------------------------
        let emptied = EmptiedFiles(vec![EmptiedFile {
            stream: "stdout",
            file: "/logs/shepd.out.log".to_string(),
            result: "emptied",
        }])
        .rows_for(coloured(), true);
        assert_eq!(
            emptied[0][at(EmptiedFiles::headers(), "RESULT")],
            painted("emptied", Role::Meadow)
        );

        let steps = StartupSteps(vec![StartupStep {
            action: "ran",
            target: "launchctl load".to_string(),
            result: "permission denied".to_string(),
        }])
        .rows_for(coloured(), true);
        assert_eq!(
            steps[0][at(StartupSteps::headers(), "RESULT")],
            painted("permission denied", Role::Bark),
            "an unrecognised RESULT is the failure line"
        );
    }

    /// `unknown` is `Butter` and never `Bark`: a client older than its daemon
    /// usually has a perfectly healthy dog.
    #[test]
    fn source_draws_the_trust_line_and_never_paints_a_working_dog_red() {
        assert_eq!(source_role(&DogSource::BuiltIn), Role::Ink3);
        assert_eq!(
            source_role(&DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string()
            }),
            Role::Butter
        );
        assert_ne!(
            source_role(&DogSource::BuiltIn),
            source_role(&DogSource::Adopted {
                path: "/x".to_string()
            }),
            "shep's own code and a third-party binary must not look the same"
        );
    }

    /// All eleven kinds the three verbs produce.
    #[test]
    fn an_outcome_lands_in_the_tier_its_kind_calls_for() {
        for worked in ["replied", "delivered", "sent"] {
            assert_eq!(outcome_role(worked), Role::Meadow, "{worked}");
        }
        for quiet in ["skipped", "not_running"] {
            assert_eq!(outcome_role(quiet), Role::Ink3, "{quiet}");
        }
        for failed in ["timed_out", "failed", "not_written"] {
            assert_eq!(outcome_role(failed), Role::Bark, "{failed}");
        }
        for gap in ["no_channel", "no_stdin"] {
            assert_eq!(outcome_role(gap), Role::Butter, "{gap}");
        }
        assert_eq!(
            outcome_role("unknown"),
            Role::Butter,
            "a kind this client predates is a version gap, not a fault"
        );
    }

    /// Driven through a real `TriggeredRows`, so it covers the wiring as well
    /// as the tiers.
    #[test]
    fn a_reply_table_colours_its_outcome_and_leaves_its_detail_alone() {
        let rows = TriggeredRows(vec![
            ActionReply {
                id: 0,
                name: "web".to_string(),
                outcome: ActionOutcome::Replied {
                    body: "swept 3".to_string(),
                },
            },
            ActionReply {
                id: 1,
                name: "api".to_string(),
                outcome: ActionOutcome::TimedOut,
            },
        ])
        .rows_for(coloured(), true);

        assert_eq!(rows[0][0], painted("0", Role::Ink3), "ID is chrome");
        assert_eq!(rows[0][1], "web", "NAME is plain");
        assert_eq!(rows[0][2], painted("replied", Role::Meadow));
        assert_eq!(rows[0][3], "swept 3", "DETAIL carries no colour");
        assert_eq!(rows[1][2], painted("timed_out", Role::Bark));
        assert_eq!(
            rows[1][3], "no reply within the app's own action_timeout",
            "and neither does a failure's DETAIL"
        );
    }

    /// fails if the `-` placeholder rule stops reaching a column whose own
    /// rule declined to paint it: `BarkRows` returns [`Paint::Default`] for a
    /// SINKS cell holding `-`, and `Paint::Default` carries the rule.
    #[test]
    fn a_placeholder_falls_back_to_the_shared_rule() {
        let rows = BarkRows(vec![Bark {
            at_ms: 0,
            rule: "restart-storm".to_string(),
            subject: "web".to_string(),
            message: "restarted 5 times".to_string(),
            sinks: Vec::new(),
        }])
        .rows_for(coloured(), true);
        assert_eq!(rows[0][4], painted("-", Role::Ink3), "no sinks reads as -");
    }

    #[test]
    fn a_bark_whose_sink_refused_is_marked() {
        let bark = |error: Option<String>| Bark {
            at_ms: 0,
            rule: "restart-storm".to_string(),
            subject: "web".to_string(),
            message: "restarted 5 times".to_string(),
            sinks: vec![SinkOutcome {
                sink: "ops".to_string(),
                error,
            }],
        };
        let delivered = BarkRows(vec![bark(None)]).rows_for(coloured(), true);
        assert_eq!(delivered[0][4], painted("ops", Role::Meadow));

        let refused =
            BarkRows(vec![bark(Some("connection refused".to_string()))]).rows_for(coloured(), true);
        assert_eq!(refused[0][4], painted("ops(failed)", Role::Bark));
    }

    #[test]
    fn the_dogs_table_is_coloured_by_the_flock_tables_own_rules() {
        let rows =
            DogRows(vec![sample_dog(ProcStatus::Online, Some(14_110))]).rows_for(coloured(), true);
        let row = &rows[0];

        // Cell by cell, in the sheep table's order.
        assert_eq!(row[0], painted("9", Role::Ink3), "ID is chrome");
        assert_eq!(row[1], "log-rotate", "NAME is plain, as in the flock table");
        assert_eq!(
            row[2],
            painted("(o.o) online", Role::Meadow),
            "STATUS takes the face and the role, from vocabulary.rs"
        );
        assert_eq!(row[3], "14110", "a real PID is left plain");
        assert_eq!(row[4], painted("4", Role::Butter), "RESTARTS above zero");
        assert_eq!(row[5], painted("-", Role::Ink3), "EXIT: still running");
        assert_eq!(row[6], painted("0.0%", Role::Ink3), "idle CPU is not news");
        assert_eq!(row[7], painted("3.0M", Role::Meadow), "MEM below the ramp");
        assert_eq!(row[8], "41s", "UPTIME is plain, as in the flock table");
        assert_eq!(
            row[9],
            painted("adopted", Role::Butter),
            "SOURCE carries the trust distinction, and sits last"
        );
    }

    /// The case above cannot reach the placeholder branch: a running dog has
    /// a real PID, CPU and MEM.
    #[test]
    fn a_stopped_dogs_placeholders_are_muted() {
        let rows = DogRows(vec![sample_dog(ProcStatus::Stopped, None)]).rows_for(coloured(), true);
        let row = &rows[0];

        assert_eq!(row[2], painted("(-.-) stopped", Role::Ink3));
        assert_eq!(row[3], painted("-", Role::Ink3), "PID");
        assert_eq!(row[6], painted("-", Role::Ink3), "CPU");
        assert_eq!(row[7], painted("-", Role::Ink3), "MEM");
    }

    /// `DogEnabledRow::status` can carry a sentence in place of a status
    /// rendering.
    #[test]
    fn a_dog_action_row_colours_a_status_and_never_a_sentence() {
        let acted = DogEnabledRow {
            name: "log-rotate".to_string(),
            source: DogSource::Adopted {
                path: "/usr/local/bin/shep-log-rotate".to_string(),
            },
            shepherd_acted: true,
            status: "online".to_string(),
        };
        let row = &acted.rows_for(coloured(), true)[0];
        assert_eq!(
            row[1],
            painted("adopted", Role::Butter),
            "SOURCE says this is not shep's own code"
        );
        assert_eq!(row[3], painted("(o.o) online", Role::Meadow));

        let sentence = "no shepherd running; the config was written";
        let unacted = DogEnabledRow {
            name: "log-rotate".to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: sentence.to_string(),
        };
        let row = &unacted.rows_for(coloured(), true)[0];
        assert_eq!(row[3], sentence, "a sentence is left exactly as it was");
    }

    /// `false` is worth knowing, but the STATUS cell beside it already says
    /// so in a whole sentence.
    #[test]
    fn a_dog_action_row_leaves_the_name_and_the_shepherd_column_plain() {
        let row = &DogDisabledRow {
            name: "log-rotate".to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: "no shepherd running".to_string(),
        }
        .rows_for(coloured(), true)[0];
        assert_eq!(row[0], "log-rotate");
        assert_eq!(row[2], "false");
    }

    /// `rehome` is the only one of the four whose SOURCE can be absent.
    #[test]
    fn a_rehomed_row_with_nothing_to_forget_still_mutes_its_source() {
        let row = &DogRehomedRow {
            name: "metrics".to_string(),
            source: None,
            shepherd_acted: true,
            status: "stopped".to_string(),
        }
        .rows_for(coloured(), true)[0];
        assert_eq!(row[1], painted("-", Role::Ink3));
        assert_eq!(row[3], painted("(-.-) stopped", Role::Ink3));
    }

    /// A path is this table's subject, so only the `-` a peer daemon
    /// predating the field produces is muted.
    #[test]
    fn a_flushed_row_mutes_its_id_and_its_dash_and_leaves_a_path_alone() {
        let mut without = sample_info(1, "cron", 0);
        without.out_file = None;
        without.err_file = None;
        let rows =
            FlushedRows(vec![sample_info(0, "web", 60_000), without]).rows_for(coloured(), true);

        assert_eq!(rows[0][0], painted("0", Role::Ink3), "ID is chrome");
        assert_eq!(rows[0][1], "web", "NAME is plain");
        assert_eq!(
            rows[0][2], "/logs/web-0-out.log",
            "a real path carries no colour"
        );
        assert_eq!(rows[1][2], painted("-", Role::Ink3), "the placeholder does");
        assert_eq!(rows[1][3], painted("-", Role::Ink3));
    }

    #[test]
    fn lamb_rows_carry_no_colour_at_all() {
        let rows = LambRows(vec![Lamb::new(48_302, "node")]).rows_for(coloured(), true);
        assert_eq!(rows[0], vec!["48302".to_string(), "node".to_string()]);
    }

    /// A variant missing from `status_named_by`'s list renders plain instead
    /// of failing to compile. Driven off `Display`, so it also fails if the
    /// two disagree.
    #[test]
    fn every_status_is_recognised_by_its_own_rendering() {
        for status in [
            ProcStatus::Starting,
            ProcStatus::Online,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
        ] {
            assert_eq!(
                status_named_by(&status.to_string()),
                Some(status),
                "{status} is not recognised by its own rendering"
            );
        }
        assert_eq!(
            status_named_by("no shepherd running"),
            None,
            "and a sentence is not mistaken for one"
        );
    }

    /// Through `rows_for`, since this is about which cells get touched rather
    /// than about a threshold.
    #[test]
    fn chrome_and_placeholder_columns_are_coloured_and_nothing_else_is() {
        use crate::style::{Presentation, StyleLevel};

        let presentation = Presentation::new(
            StyleLevel::Full,
            None,
            Some(std::ffi::OsStr::new("xterm-256color")),
            None,
            200,
        );
        // One row with a real PID and no fold, one with neither.
        let mut running = sample_info(0, "web", 60_000);
        running.fold = None;
        let mut stopped = sample_info(1, "cron", 0);
        stopped.pid = None;
        stopped.fold = None;
        let flock = FlockRows(vec![running, stopped]);

        let rows = flock.rows_for(presentation, true);

        // ID: chrome, always coloured.
        assert!(rows[0][0].contains('\u{1b}'), "{:?}", rows[0][0]);
        assert!(rows[1][0].contains('\u{1b}'), "{:?}", rows[1][0]);
        // PID: a real value is left plain; the placeholder is coloured.
        assert!(!rows[0][3].contains('\u{1b}'), "{:?}", rows[0][3]);
        assert!(rows[1][3].contains('\u{1b}'), "{:?}", rows[1][3]);
        // FOLD: chrome, always coloured, `-` here on both rows.
        assert!(rows[0][10].contains('\u{1b}'), "{:?}", rows[0][10]);
        assert!(rows[1][10].contains('\u{1b}'), "{:?}", rows[1][10]);
    }

    // --- A dog that has never answered the shepherd ----------------------
    //
    // `ProcessInfo::status` reports whether a process is alive; `handshook`
    // is the fact the STATUS column adds to it.

    /// The `Presentation` for one style, at a width nothing drops at.
    fn styled(level: crate::style::StyleLevel) -> Presentation {
        Presentation::new(
            level,
            None,
            Some(std::ffi::OsStr::new("xterm-256color")),
            None,
            200,
        )
    }

    /// `sample_dog`, plus what this shepherd knows about its handshake.
    fn dog_with_contact(handshook: Option<bool>) -> ProcessInfo {
        let mut dog = sample_dog(ProcStatus::Online, Some(208_341));
        dog.handshook = handshook;
        dog
    }

    /// The cell under `header` in `T`'s only row.
    fn cell_of<T: Render>(row: &[String], header: &str) -> String {
        row[T::headers().iter().position(|h| *h == header).unwrap()].clone()
    }

    /// The process is alive, so `status` is not wrong; it answers a different
    /// question than the operator's.
    #[test]
    fn a_dog_that_has_never_answered_the_shepherd_does_not_read_as_online() {
        let rows = DogRows(vec![dog_with_contact(Some(false))]).rows();
        assert_eq!(cell_of::<DogRows>(&rows[0], "STATUS"), "silent");
    }

    /// `full` carries its own face, and `bare` never reaches `rows_for`, so
    /// its cell carries no escape.
    #[test]
    fn a_silent_dog_reads_the_same_in_all_three_styles() {
        use crate::style::StyleLevel;
        let dogs = DogRows(vec![dog_with_contact(Some(false))]);

        let full = dogs.rows_for(styled(StyleLevel::Full), true);
        assert_eq!(
            cell_of::<DogRows>(&full[0], "STATUS"),
            painted("(?_?) silent", Role::Butter)
        );

        let plain = dogs.rows_for(styled(StyleLevel::Plain), true);
        assert_eq!(
            cell_of::<DogRows>(&plain[0], "STATUS"),
            painted("silent", Role::Butter)
        );

        let bare = dogs.rows();
        let cell = cell_of::<DogRows>(&bare[0], "STATUS");
        assert_eq!(cell, "silent");
        assert!(!cell.contains('\u{1b}'), "bare carries no escape: {cell:?}");
    }

    /// The whole row is compared: a guard keyed on the wrong field could
    /// leave STATUS right and move something else.
    #[test]
    fn a_dog_that_has_answered_renders_exactly_as_before() {
        use crate::style::StyleLevel;
        let mut before = sample_dog(ProcStatus::Online, Some(208_341));
        before.handshook = None;
        let talking = dog_with_contact(Some(true));

        assert_eq!(
            DogRows(vec![talking.clone()]).rows(),
            DogRows(vec![before.clone()]).rows()
        );
        assert_eq!(
            DogRows(vec![talking]).rows_for(styled(StyleLevel::Full), true),
            DogRows(vec![before]).rows_for(styled(StyleLevel::Full), true)
        );
    }

    /// `None` means "no handshake fact to report", never "never handshaken".
    #[test]
    fn a_dog_from_a_shepherd_predating_the_field_reads_as_it_always_did() {
        use crate::style::StyleLevel;
        let rows = DogRows(vec![dog_with_contact(None)]).rows();
        assert_eq!(cell_of::<DogRows>(&rows[0], "STATUS"), "online");

        let full = DogRows(vec![dog_with_contact(None)]).rows_for(styled(StyleLevel::Full), true);
        assert_eq!(
            cell_of::<DogRows>(&full[0], "STATUS"),
            painted("(o.o) online", Role::Meadow)
        );
    }

    /// A sheep's `handshook` is always `None`. Driven through a sheep
    /// carrying `Some(false)` too, which the daemon never sends.
    #[test]
    fn a_sheep_never_reads_as_silent() {
        use crate::style::StyleLevel;
        let sheep = sample_info(1, "web", 60_000);
        assert_eq!(sheep.handshook, None, "the daemon sends nothing here");
        let rows = FlockRows(vec![sheep.clone()]).rows();
        assert_eq!(cell_of::<FlockRows>(&rows[0], "STATUS"), "online");

        let mut impossible = sheep;
        impossible.handshook = Some(false);
        let full = FlockRows(vec![impossible]).rows_for(styled(StyleLevel::Full), true);
        assert_eq!(
            cell_of::<FlockRows>(&full[0], "STATUS"),
            painted("(o.o) online", Role::Meadow),
            "the sheep table has no dogs in it, and no silence rule either"
        );
    }

    /// `Row::reported` is the lookout's own copy, not shared code, so every
    /// axis that decides the answer is driven together: `dog`, `handshook`
    /// and every `ProcStatus`.
    #[test]
    fn the_flock_table_and_the_lookout_read_a_dogs_silence_the_same_way() {
        use crate::lookout::app::Row;

        let statuses = [
            ProcStatus::Starting,
            ProcStatus::Online,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
        ];
        let handshooks = [None, Some(false), Some(true)];
        let dogs = [None, Some(DogSource::BuiltIn)];

        for dog in &dogs {
            for &handshook in &handshooks {
                for &status in &statuses {
                    let info = ProcessInfo::builder(9, "log-rotate", status)
                        .dog(dog.clone())
                        .handshook(handshook)
                        .build();

                    let table = reported(&info);
                    let dashboard = Row {
                        info: info.clone(),
                        anchor: std::time::Instant::now(),
                    }
                    .reported();

                    assert_eq!(
                        table, dashboard,
                        "dog={dog:?} handshook={handshook:?} status={status:?}"
                    );
                }
            }
        }
    }

    /// `online` is the one word a silence contradicts: the rest already say
    /// the relationship is not established.
    #[test]
    fn only_online_is_overridden_by_a_silence() {
        for status in [
            ProcStatus::Starting,
            ProcStatus::Stopping,
            ProcStatus::Stopped,
            ProcStatus::Errored,
            ProcStatus::WaitingRestart,
        ] {
            let mut dog = sample_dog(status, Some(208_341));
            dog.handshook = Some(false);
            let rows = DogRows(vec![dog]).rows();
            assert_eq!(
                cell_of::<DogRows>(&rows[0], "STATUS"),
                status.to_string(),
                "{status} says what it says without help"
            );
        }
    }

    /// `status` alone still reads `online` for a silent dog.
    #[test]
    fn the_json_form_carries_the_handshake_fact() {
        let json = serde_json::to_value(DogRows(vec![
            dog_with_contact(Some(false)),
            dog_with_contact(Some(true)),
            dog_with_contact(None),
        ]))
        .unwrap();
        assert_eq!(json[0]["handshook"], serde_json::json!(false));
        assert_eq!(json[0]["status"], "online");
        assert_eq!(json[1]["handshook"], serde_json::json!(true));
        assert_eq!(json[2]["handshook"], serde_json::Value::Null);
    }
}
