//! Query verbs: `flock`, `describe`, `fold`, `ping`. None mutate the flock,
//! and none autostart: `main` hands each one an already-connected [`Client`].
//!
//! `describe` and `fold` share one shape (`Request::Describe` against a
//! [`SelectorSpec`]); `fold` supplies `SelectorSpec::Fold` directly.
//!
//! `ping` does not ask the daemon for its version and pid: the handshake
//! already answered that, in the
//! [`HelloAck`](shep_core::protocol::HelloAck) [`Client::daemon`] holds. It
//! still issues `Request::Ping` as the liveness check.

use shep_client::Client;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{ProcessInfo, Request, Response, SelectorSpec};
use shep_core::status::ProcStatus;
use shep_daemon::snapshot::FlockSnapshot;

use crate::cli::{DogsArgs, FoldArgs, Format, SelectorArgs};
use crate::commands::selector::parse_selector;
use crate::dog_index::{self, AvailableDog, DogSourceKind};
use crate::exit::ExitCode;
use crate::flourish;
use crate::output::{
    AvailableDogRows, DogRows, Render, RolledSheep, RolledSheepRows, Streams, emit, emit_described,
    emit_flock, write_outcome,
};

/// Sends `body`, renders whatever the daemon answers through [`emit`], and
/// maps every way that can go wrong to its exit code.
///
/// `extract` pulls the verb's own payload out of `Response`, which is
/// `#[non_exhaustive]`: an answer it does not recognise maps to
/// [`ExitCode::Internal`] rather than being guessed at. Every query verb uses
/// the client's default deadline, so there is no deadline parameter.
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    command: &str,
    body: Request,
    extract: F,
) -> ExitCode
where
    T: Render,
    F: FnOnce(Response) -> Option<T>,
{
    match client.request(body).await {
        Ok(response) => match extract(response) {
            Some(payload) => write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                command,
                payload,
                streams.style,
            )),
            None => {
                let message = "the daemon answered with a response this client does not understand";
                streams.fail(ExitCode::Internal, message)
            }
        },
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// `describe` and `fold`'s shared body: one `Request::Describe` against
/// `selector`, rendered through [`emit_described`] as the sheep table and
/// each sheep's lamb tree beneath it. `command` is the verb name the output
/// envelope reports.
///
/// Not routed through [`request_and_render`]: `emit_described` renders one
/// `Vec<ProcessInfo>` into two tables, which no single [`Render`] impl can
/// express.
async fn describe_selector(
    client: &Client,
    streams: &mut Streams<'_>,
    command: &str,
    selector: SelectorSpec,
) -> ExitCode {
    match client.request(Request::Describe { selector }).await {
        Ok(Response::Described(procs)) => write_outcome(emit_described(
            &mut *streams.out,
            streams.fmt,
            command,
            procs,
            streams.style,
        )),
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// `shep flock` when no shepherd answers: the muster roll, marked stopped
///
/// The exit code stays [`ExitCode::DaemonUnreachable`] even though the table
/// looks successful: a monitoring script must not read a dead supervisor as
/// a healthy empty flock. A missing or unreadable roll is not an error.
pub fn flock_from_roll(streams: &mut Streams<'_>, paths: &ShepPaths) -> ExitCode {
    let saved = std::fs::read(&paths.snapshot)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FlockSnapshot>(&bytes).ok());

    let mut sheep: Vec<RolledSheep> = saved
        .map(|roll| {
            roll.apps
                .into_iter()
                .map(|entry| RolledSheep {
                    name: entry.app.name.clone(),
                    instances: entry.instances_running,
                    status: "stopped",
                })
                .collect()
        })
        .unwrap_or_default();
    // The roll's stored order is neither meaningful nor stable. Name is the
    // whole key: one entry per app, so there is no tie to break.
    sheep.sort_unstable_by(|a, b| a.name.cmp(&b.name));

    if streams.fmt == Format::Table {
        let _ = writeln!(
            streams.err,
            "no shepherd running. {}",
            if sheep.is_empty() {
                "nothing in the saved roll either.".to_owned()
            } else {
                format!(
                    "{} in the saved roll at {}:",
                    match sheep.len() {
                        1 => "1 sheep".to_owned(),
                        n => format!("{n} sheep"),
                    },
                    paths.snapshot.display()
                )
            }
        );
    }
    let empty = sheep.is_empty();
    // No table for an empty roll: bare headers over nothing read as a glitch.
    // JSON still gets the empty array, because a script wants one shape.
    if !(empty && streams.fmt == Format::Table) {
        let _ = emit(
            &mut *streams.out,
            streams.fmt,
            "flock",
            RolledSheepRows(sheep),
            streams.style,
        );
    }
    if streams.fmt == Format::Table && !empty {
        let _ = writeln!(streams.err, "`shep muster` brings them back.");
    }
    ExitCode::DaemonUnreachable
}

/// Lists the whole flock: the sheep table, then the dogs table beneath it
/// whenever any dog is registered, with a [`sheep_flourish`] above
///
/// The flourish is gated on `Format::Table` and `streams.style.level.sheep()`
/// and nothing else. Not routed through [`request_and_render`], which renders
/// one [`Render`] type per verb rather than two tables from one
/// `Vec<ProcessInfo>`.
pub async fn flock(client: &Client, streams: &mut Streams<'_>) -> ExitCode {
    match client.request(Request::ListFlock).await {
        Ok(Response::Flock(procs)) => {
            // Read before `procs` moves into `emit_flock`.
            let art = (streams.fmt == Format::Table && streams.style.level.sheep())
                .then(|| sheep_flourish(&procs))
                .flatten();
            if let Some(art) = &art {
                let _ = write!(streams.out, "{art}");
            }
            write_outcome(emit_flock(
                &mut *streams.out,
                streams.fmt,
                "flock",
                procs,
                streams.style,
            ))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// The flourish for one flock listing, or `None` when neither the
/// empty-flock nor the all-asleep state applies
///
/// Dogs are excluded from both checks: the flourish sits beside the sheep
/// table and is a claim about the sheep. [`ProcStatus::Stopping`] does not
/// count as asleep, being reload's transient rather than rest.
fn sheep_flourish(listing: &[ProcessInfo]) -> Option<String> {
    let sheep: Vec<&ProcessInfo> = listing.iter().filter(|p| p.dog.is_none()).collect();
    if sheep.is_empty() {
        return Some(flourish::empty_flock());
    }
    sheep
        .iter()
        .all(|p| p.status == ProcStatus::Stopped)
        .then(|| flourish::all_asleep(sheep.len()))
}

/// Lists the dogs and nothing else: the same `Request::ListFlock` [`flock`]
/// sends, filtered to the entries carrying a `dog` marker
///
/// `args.filter` is a case-insensitive substring match against the dog's
/// name, the one field a running dog and a community-index entry share.
/// Not [`emit_flock`], which would print the sheep table's header row over
/// a dogs-only listing.
pub async fn dogs(client: &Client, streams: &mut Streams<'_>, args: &DogsArgs) -> ExitCode {
    let filter = args.filter.as_deref();
    request_and_render(
        client,
        streams,
        "dogs",
        Request::ListFlock,
        |response| match response {
            Response::Flock(procs) => Some(DogRows(
                procs
                    .into_iter()
                    .filter(|p| p.dog.is_some())
                    .filter(|p| filter.is_none_or(|f| matches_filter(f, &[&p.name])))
                    .collect(),
            )),
            _ => None,
        },
    )
    .await
}

/// Whether `filter` matches any of `haystacks`, case-insensitively. Shared
/// by [`dogs`] (name alone) and [`available_dogs`] (name, package and
/// description).
fn matches_filter(filter: &str, haystacks: &[&str]) -> bool {
    let filter = filter.to_lowercase();
    haystacks.iter().any(|h| h.to_lowercase().contains(&filter))
}

/// Lists the dogs published in the community index: `shep dogs --available`
///
/// Reaches no [`Client`], so it answers with no shepherd running. Under
/// `Format::Table` a filter matching one dog prints its detail view, and a
/// filter matching nothing prints `no dog matches "<filter>"` and still
/// exits [`ExitCode::Success`]. `--format json` always renders the array.
///
/// # Errors reaching the operator
/// A failure to read or parse the index names the URL and exits
/// [`ExitCode::Failure`]: [`dog_index::IndexError`] carries it on one
/// variant only.
pub async fn available_dogs(streams: &mut Streams<'_>, args: &DogsArgs) -> ExitCode {
    let url = dog_index::index_url();
    let index = match dog_index::fetch_index(&url).await {
        Ok(index) => index,
        Err(err) => {
            let message = format!("reading the dog index from {url}: {err}");
            return streams.fail(ExitCode::Failure, &message);
        }
    };
    let (skipped, sanitised) = (index.skipped, index.sanitised);

    let filter = args.filter.as_deref();
    let matched: Vec<AvailableDog> = index
        .dogs
        .into_iter()
        .filter(|dog| {
            filter.is_none_or(|f| matches_filter(f, &[&dog.name, &dog.package, &dog.description]))
        })
        .collect();

    let code = if streams.fmt == Format::Table
        && matched.is_empty()
        && let Some(filter) = filter
    {
        let _ = writeln!(streams.out, "no dog matches {filter:?}");
        ExitCode::Success
    } else if streams.fmt == Format::Table
        && let [only] = matched.as_slice()
    {
        write_outcome(render_detail(&mut *streams.out, only))
    } else {
        write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "dogs",
            AvailableDogRows(matched),
            streams.style,
        ))
    };

    note_index_costs(streams, skipped, sanitised);
    code
}

/// The clause both notices below end in: the counts describe the fetched
/// document, not the filtered listing they are printed beside.
const INDEX_WIDE: &str = ", across the whole index rather than this listing";

/// Prints [`dog_index::Index::skipped`]/[`dog_index::Index::sanitised`] as
/// footer notices when either is non-zero, whatever the filter matched.
fn note_index_costs(streams: &mut Streams<'_>, skipped: usize, sanitised: usize) {
    if skipped > 0 {
        streams.aside(
            "dogs_skipped",
            &format!(
                "{skipped} entr{} skipped{INDEX_WIDE}",
                if skipped == 1 { "y" } else { "ies" }
            ),
        );
    }
    if sanitised > 0 {
        streams.aside(
            "dogs_sanitised",
            &format!(
                "{sanitised} entr{} contained control characters{INDEX_WIDE}",
                if sanitised == 1 { "y" } else { "ies" }
            ),
        );
    }
}

/// The lone-match affordance [`available_dogs`] prints for `Format::Table`:
/// full detail on one dog, ending in the two copy-pasteable commands an
/// operator needs to adopt it. Never reached from `--format json`.
///
/// # Errors
/// The underlying write failed.
fn render_detail(out: &mut dyn std::io::Write, dog: &AvailableDog) -> std::io::Result<()> {
    writeln!(out, "{} . {} . {}", dog.name, dog.package, dog.category)?;
    writeln!(out, "{}", dog.description)?;
    writeln!(out, "{} . {}", dog.license, dog.repo)?;
    writeln!(out)?;
    writeln!(out, "{}", install_line(&dog.source, &dog.package))?;
    writeln!(
        out,
        "{}",
        adopt_line(&dog.source, &dog.adopt_as, &dog.package)
    )
}

/// The `$ ...` line [`render_detail`] prints for how to build `source`'s
/// binary. [`DogSourceKind::Manual`] carries prose instead of a command, so
/// it prints with no `$`, just the two-space indent the command lines share.
fn install_line(source: &DogSourceKind, package: &str) -> String {
    match source {
        DogSourceKind::Cargo {
            version: Some(version),
        } => {
            format!("  $ cargo install {package} --version {version}")
        }
        DogSourceKind::Cargo { version: None } => format!("  $ cargo install {package}"),
        DogSourceKind::CargoGit { url } => format!("  $ cargo install --git {url}"),
        DogSourceKind::GoInstall { module } => format!("  $ go install {module}@latest"),
        DogSourceKind::Manual { instructions } => format!("  {instructions}"),
    }
}

/// The `$ shep adopt ...` line [`render_detail`] prints
///
/// Built from `adopt_as`, never `name` or `package`: a wrong name ships a
/// command that silently discards the dog's whole config section.
/// [`DogSourceKind::Manual`] has no predictable install path, so its line
/// names the placeholder literally. `--name` is always spelled, since
/// nothing enforces the naming convention on a user-contributed `package`.
fn adopt_line(source: &DogSourceKind, adopt_as: &str, package: &str) -> String {
    match source {
        DogSourceKind::Cargo { .. } | DogSourceKind::CargoGit { .. } => {
            format!("  $ shep adopt ~/.cargo/bin/{package} --name {adopt_as}")
        }
        DogSourceKind::GoInstall { .. } => {
            format!("  $ shep adopt $(go env GOPATH)/bin/{package} --name {adopt_as}")
        }
        DogSourceKind::Manual { .. } => {
            format!("  $ shep adopt <path to the binary> --name {adopt_as}")
        }
    }
}

/// Describes the sheep matching `args.selector` in detail.
pub async fn describe(client: &Client, streams: &mut Streams<'_>, args: &SelectorArgs) -> ExitCode {
    // One pass per target, each its own detail view: `describe` answers with
    // a tree per sheep, so merging them would lose that shape.
    let mut failure: Option<ExitCode> = None;
    for raw in &args.selectors {
        let selector = match parse_selector(streams, raw) {
            Ok(selector) => SelectorSpec::from(&selector),
            Err(code) => return code,
        };
        let code = describe_selector(client, streams, "describe", selector).await;
        if code != ExitCode::Success {
            failure = failure.or(Some(code));
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Lists one fold: `Request::Describe` with `SelectorSpec::Fold(args.name)`,
/// delegating to [`describe_selector`].
pub async fn fold(client: &Client, streams: &mut Streams<'_>, args: &FoldArgs) -> ExitCode {
    describe_selector(
        client,
        streams,
        "fold",
        SelectorSpec::Fold(args.name.clone()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use shep_client::testing::{
        fake_client_capturing_envelopes, fake_client_on, fake_client_with_ack, sample_ack,
        sample_info,
    };
    use shep_core::protocol::DogSource;

    use super::*;

    /// Bounds every `envelopes.recv()` here: a verb that never reaches the
    /// wire must fail by assertion, not by hanging the job.
    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn flock_asks_the_daemon_to_list_the_whole_flock() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let _ = flock(&client, &mut streams).await;
        let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
            .await
            .expect("flock must reach the wire; it hung instead of sending a request")
            .unwrap();
        assert_eq!(sent.body, Request::ListFlock);
    }

    #[tokio::test]
    async fn describe_sends_the_parsed_selector_in_its_compiled_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;

        for (input, expected) in [
            ("all", SelectorSpec::All),
            ("7", SelectorSpec::Id(7)),
            ("web", SelectorSpec::Name("web".into())),
            ("/^web-/", SelectorSpec::Regex("^web-".into())),
            ("fold:api", SelectorSpec::Fold("api".into())),
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            let args = SelectorArgs {
                selectors: vec![input.into()],
            };
            let _ = describe(&client, &mut streams, &args).await;
            let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("describe({input}) must reach the wire; it hung instead of sending a request")
                })
                .unwrap();
            assert_eq!(
                sent.body,
                Request::Describe { selector: expected },
                "{input}"
            );
        }
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects:
    /// an unterminated regex character class.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            describe(
                &client,
                &mut streams,
                &SelectorArgs {
                    selectors: vec!["/[/".into()],
                },
            )
            .await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally"
        );
    }

    #[tokio::test]
    async fn fold_asks_the_daemon_for_that_fold_and_nothing_wider() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let _ = fold(&client, &mut streams, &FoldArgs { name: "api".into() }).await;
        let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
            .await
            .expect("fold must reach the wire; it hung instead of sending a request")
            .unwrap();
        assert_eq!(
            sent.body,
            Request::Describe {
                selector: SelectorSpec::Fold("api".into())
            }
        );
    }

    /// `Response::Flock` and `Response::Described` both wrap a bare
    /// `Vec<ProcessInfo>`, so an arm swapped between them compiles clean.
    /// `reply_to_list` scripts a real `Response::Flock` to catch that.
    #[tokio::test]
    async fn flock_response_round_trips_into_rendered_flock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_ack(&path, sample_ack()).await;
        daemon.reply_to_list(vec![sample_info()]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Json,
            };
            flock(&client, &mut streams).await
        };

        assert_eq!(code, ExitCode::Success);
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["command"], "flock");
        assert_eq!(json["data"][0]["name"], "web");
    }

    #[tokio::test]
    async fn describe_response_round_trips_into_rendered_flock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_ack(&path, sample_ack()).await;
        daemon.reply_to_describe(vec![sample_info()]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Json,
            };
            describe(
                &client,
                &mut streams,
                &SelectorArgs {
                    selectors: vec!["all".into()],
                },
            )
            .await
        };

        assert_eq!(code, ExitCode::Success);
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["command"], "describe");
        assert_eq!(json["data"][0]["name"], "web");
    }

    // --- sheep_flourish ---

    /// A sheep with `status` pinned and nothing else.
    fn sheep(id: u32, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(id, format!("s{id}"), status).build()
    }

    /// A registered dog, which `sheep_flourish` must never count as a sheep.
    fn dog(id: u32) -> ProcessInfo {
        ProcessInfo::builder(id, format!("d{id}"), ProcStatus::Online)
            .dog(Some(DogSource::BuiltIn))
            .build()
    }

    #[test]
    fn sheep_flourish_fires_empty_flock_on_a_truly_empty_listing() {
        let art = sheep_flourish(&[]).expect("an empty listing must flourish");
        assert!(art.contains("no sheep in the flock yet"), "{art}");
    }

    #[test]
    fn sheep_flourish_treats_dogs_only_as_an_empty_flock() {
        let art =
            sheep_flourish(&[dog(1), dog(2)]).expect("dogs alone must read as an empty flock");
        assert!(art.contains("no sheep in the flock yet"), "{art}");
    }

    /// The count in the flourish excludes dogs too.
    #[test]
    fn sheep_flourish_fires_all_asleep_when_every_sheep_is_stopped() {
        let listing = [
            sheep(1, ProcStatus::Stopped),
            sheep(2, ProcStatus::Stopped),
            dog(3),
        ];
        let art = sheep_flourish(&listing).expect("an all-stopped flock must flourish");
        assert!(art.contains("2 in the flock, all asleep"), "{art}");
    }

    #[test]
    fn a_live_dog_does_not_block_all_asleep() {
        let listing = [sheep(1, ProcStatus::Stopped), dog(2)];
        let art = sheep_flourish(&listing).expect("a live dog must not suppress all_asleep");
        assert!(art.contains("1 in the flock, all asleep"), "{art}");
    }

    #[test]
    fn sheep_flourish_is_silent_on_a_mixed_flock() {
        let listing = [sheep(1, ProcStatus::Online), sheep(2, ProcStatus::Stopped)];
        assert_eq!(
            sheep_flourish(&listing),
            None,
            "a mixed flock is not a flourish moment"
        );
    }

    /// `Stopping` is reload's transient for the instance being replaced, so
    /// a flock mid-reload must not read as asleep.
    #[test]
    fn stopping_does_not_count_as_asleep() {
        let listing = [
            sheep(1, ProcStatus::Stopping),
            sheep(2, ProcStatus::Stopping),
        ];
        assert_eq!(
            sheep_flourish(&listing),
            None,
            "Stopping is a transient, not rest"
        );
    }

    /// The daemon answers an empty flock on every case here, which
    /// `sheep_flourish` always fires on, so only the gate decides.
    #[tokio::test]
    async fn the_flourish_only_prints_under_table_format_and_a_sheep_drawing_level() {
        use crate::style::{Presentation, StyleLevel};

        for (fmt, level, expect_art) in [
            (Format::Table, StyleLevel::Full, true),
            (Format::Json, StyleLevel::Full, false),
            (Format::Table, StyleLevel::Plain, false),
            (Format::Table, StyleLevel::Bare, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = shep_client::testing::control_address(dir.path());
            let (client, _daemon) = fake_client_on(&path).await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: Presentation::new(level, None, None, None, 80),
                fmt,
            };
            let _ = flock(&client, &mut streams).await;
            let printed = String::from_utf8_lossy(&out);
            assert_eq!(
                printed.contains("no sheep in the flock yet"),
                expect_art,
                "fmt={fmt:?} level={level:?}: {printed}"
            );
        }
    }
}
