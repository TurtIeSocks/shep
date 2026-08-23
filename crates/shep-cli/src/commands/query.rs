//! Query verbs: `flock`, `describe`, `fold`, `ping` — the read-only half of
//! the verb set. None of them mutate the flock, and none of them autostart:
//! `main` hands every one of these an already-connected [`Client`], the
//! same contract `commands::lifecycle` documents on its own module.
//!
//! `describe` and `fold` share one shape (`Request::Describe` against a
//! [`SelectorSpec`]) — `fold` is a one-line specialisation of `describe`
//! that supplies `SelectorSpec::Fold` directly rather than parsing one from
//! a raw string, kept as a thin wrapper rather than a copy.
//!
//! `ping` deliberately does not ask the daemon for its own version and pid:
//! the handshake already answered that, in the
//! [`HelloAck`](shep_core::protocol::HelloAck) [`Client::daemon`] holds, so
//! asking again would be a wasted round trip for information already in
//! hand. It still issues `Request::Ping` itself, as the liveness check the
//! verb exists for.

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
    emit_error, emit_flock, emit_notice, write_outcome,
};

/// Sends `body`, renders whatever the daemon answers through [`emit`], and
/// maps every way that can go wrong to its exit code.
///
/// `extract` pulls the verb's own payload out of `Response`; `Response` is
/// `#[non_exhaustive]` (Global Constraints), so an answer `extract` does not
/// recognise — a variant this client predates, or simply the wrong one for
/// this verb — maps to [`ExitCode::Internal`] rather than being guessed at.
/// Every query verb uses the client's default deadline, so unlike
/// `commands::lifecycle`'s version of this helper there is no deadline
/// parameter to thread through.
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
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
                fmt,
                command,
                payload,
                streams.style,
            )),
            None => {
                let message = "the daemon answered with a response this client does not understand";
                let _ = emit_error(
                    &mut *streams.err,
                    fmt,
                    ExitCode::Internal.code_str(),
                    message,
                );
                ExitCode::Internal
            }
        },
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// `describe` and `fold`'s shared body: one `Request::Describe` against
/// `selector`, rendered through [`emit_described`] — the sheep table, then
/// each sheep's lamb tree beneath it. `command` is the verb name the output
/// envelope reports (`"describe"` or `"fold"`), which is why this takes it
/// as a parameter rather than hard-coding one.
///
/// Not routed through [`request_and_render`], for the same reason
/// [`flock`] is not: `emit_described` renders one `Vec<ProcessInfo>` into
/// two tables in table mode, which no single [`Render`] impl can express —
/// this is the small bespoke path this verb needs instead of widening that
/// helper into a second renderer.
async fn describe_selector(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    command: &str,
    selector: SelectorSpec,
) -> ExitCode {
    match client.request(Request::Describe { selector }).await {
        Ok(Response::Described(procs)) => write_outcome(emit_described(
            &mut *streams.out,
            fmt,
            command,
            procs,
            streams.style,
        )),
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// `shep flock` when no shepherd answers: the muster roll, marked stopped.
///
/// "Nothing is running" is a perfectly good answer to "what is running", and
/// turning it into `error[daemon_unreachable]` made looking at the flock a
/// dead end on exactly the machine where someone most needs to look -- one
/// that has just rebooted. The roll on disk holds everything needed to
/// answer, and `shep muster` is the way back, so both are stated.
///
/// The exit code stays [`ExitCode::DaemonUnreachable`] even though this
/// prints a successful-looking table, and that is the important part: a
/// monitoring script running `shep flock` must not read a dead supervisor as
/// a healthy empty flock. The output is for the human, the code is for the
/// script, and they are telling the truth about different things.
///
/// A missing or unreadable roll is not an error either -- a machine that has
/// never run `shep save` has nothing to show, which is itself the answer.
pub fn flock_from_roll(streams: &mut Streams<'_>, fmt: Format, paths: &ShepPaths) -> ExitCode {
    let saved = std::fs::read(&paths.snapshot)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FlockSnapshot>(&bytes).ok());

    let sheep: Vec<RolledSheep> = saved
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

    if fmt == Format::Table {
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
    // An empty roll renders no table at all in table form: bare column
    // headers over nothing read as a glitch, and the line above already said
    // there is nothing. JSON still gets the empty array, because a script
    // parsing this wants one shape, not two.
    if !(empty && fmt == Format::Table) {
        let _ = emit(
            &mut *streams.out,
            fmt,
            "flock",
            RolledSheepRows(sheep),
            streams.style,
        );
    }
    if fmt == Format::Table && !empty {
        let _ = writeln!(streams.err, "`shep muster` brings them back.");
    }
    ExitCode::DaemonUnreachable
}

/// Lists the whole flock: the sheep table, then the dogs table beneath it
/// whenever any dog is registered — [`emit_flock`]'s own job — with a
/// flourish ahead of the sheep table when the sheep have nothing else to
/// look at: none registered, or every one of them at rest. See
/// [`sheep_flourish`] for exactly what qualifies and why dogs are excluded
/// from the check.
///
/// The flourish is gated on `Format::Table` and
/// `streams.style.level.sheep()` and nothing else, matching every other
/// STATUS-column face — `--format json` and a piped `Format::Table` (which
/// `run_argv` already forces to [`crate::style::StyleLevel::Bare`], whose
/// `sheep()` is `false`) render exactly what they did before this flourish
/// existed.
///
/// Not routed through [`request_and_render`]: that helper renders exactly
/// one [`Render`] type per verb, through [`emit`]. A flock listing renders
/// through two tables built from one `Vec<ProcessInfo>`, which no single
/// `Render` impl can express — `emit_flock` is the small path this verb
/// needs instead of widening that helper into a second renderer, keeping
/// the same connect/request/extract/render shape every other query verb
/// here has.
pub async fn flock(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    match client.request(Request::ListFlock).await {
        Ok(Response::Flock(procs)) => {
            // Read before `procs` moves into `emit_flock`, and printed
            // ahead of the table it describes: "no sheep in the flock yet"
            // reads as the answer, with the table underneath as the
            // receipt, rather than a mascot bolted onto the end.
            let art = (fmt == Format::Table && streams.style.level.sheep())
                .then(|| sheep_flourish(&procs))
                .flatten();
            if let Some(art) = &art {
                let _ = write!(streams.out, "{art}");
            }
            write_outcome(emit_flock(
                &mut *streams.out,
                fmt,
                "flock",
                procs,
                streams.style,
            ))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// The flourish for one flock listing, or `None` when neither the
/// empty-flock nor the all-asleep state applies.
///
/// Dogs are excluded from both checks (`ProcessInfo::dog`), deliberately:
/// the flourish sits beside the sheep table, so it is a claim about the
/// sheep, and a metrics or bark dog left running answers a different
/// question than whether the flock is at rest — the dogs table beneath
/// already renders that fact on its own. A flock whose sheep are all
/// stopped but whose dog is still up is exactly the case a reader looking
/// at two tables, one with something alive in it, would find a cheerful
/// "all asleep" line wrong beside.
///
/// [`ProcStatus::Stopping`] does not count as asleep. It is reload's
/// transient for the instance being replaced, not rest (`ProcStatus`'s own
/// doc), so a flock with one sheep mid-shutdown does not flip this on
/// merely because nothing else is `Online`.
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

/// Lists the dogs, and nothing else: the same `Request::ListFlock` `flock`
/// sends, filtered to the entries carrying a `dog` marker and rendered as
/// [`DogRows`] through the ordinary [`request_and_render`] path every other
/// query verb here uses — `DogRows` is a [`Render`] impl like any other, so
/// no bespoke render path is needed the way `flock`'s own split is.
///
/// `args.filter`, when given, narrows further to a case-insensitive
/// substring match against each dog's own name — the one field a running
/// dog and a community-index entry actually share; [`available_dogs`] is
/// where `package`/`description` join it, per [`DogsArgs::filter`]'s own
/// doc.
///
/// Deliberately not [`emit_flock`]: that function's contract is a MIXED
/// listing, sheep table first: handing it a dogs-only `Vec<ProcessInfo>`
/// would still print the sheep table's header row for zero sheep, which is
/// exactly what this verb's own doc comment (`Commands::Dogs`, "and nothing
/// else") rules out. The two call sites still share one table renderer —
/// `render_table::<DogRows>`, reached here through `emit` and from inside
/// `emit_flock`'s own dogs section — so there is exactly one place that
/// knows how to lay out a dogs table.
pub async fn dogs(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &DogsArgs,
) -> ExitCode {
    let filter = args.filter.as_deref();
    request_and_render(
        client,
        streams,
        fmt,
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

/// Whether `filter` (already lower-cased at the one call site that needs
/// it) matches any of `haystacks`, case-insensitively. The one substring
/// rule backing [`DogsArgs::filter`]'s own doc comment, shared by [`dogs`]
/// (matching on name alone) and [`available_dogs`] (name, package and
/// description) so it lives in one place rather than two copies that could
/// drift apart on which field counts.
fn matches_filter(filter: &str, haystacks: &[&str]) -> bool {
    let filter = filter.to_lowercase();
    haystacks.iter().any(|h| h.to_lowercase().contains(&filter))
}

/// Lists the dogs published in the community index — `shep dogs
/// --available`. Reaches no [`Client`] and no [`Request`]: nothing here
/// touches the wire at all, which is the property `Commands::Dogs`'s own
/// guard arm in `main`'s dispatch (`lib.rs`) exists to preserve — this must
/// answer with no shepherd running.
///
/// `Format::Table` gets two affordances `--format json` does not, the same
/// kind of split [`flock`]'s own doc draws between its roll fallback and a
/// failed JSON request: a filter that narrows the listing to exactly one
/// dog prints that dog's detail view (its install and adopt commands)
/// instead of a one-row table, and a filter that matches nothing prints
/// `no dog matches "<filter>"` and still exits [`ExitCode::Success`] — an
/// empty search result is an answer, not a failure. `--format json` always
/// renders the plain array through the ordinary [`emit`] path, whatever its
/// length: every field either affordance would show is already on each row
/// there, via [`AvailableDog`]'s own `Serialize`.
///
/// # Errors reaching the operator
/// A failure to read or parse the index prints `reading the dog index from
/// {url}: {err}` and exits [`ExitCode::Failure`] — [`dog_index::IndexError`]
/// deliberately carries the URL on none of its variants but its own
/// `InsecureUrl` (that type's own doc says why), so naming it here is what
/// tells the operator which URL failed.
pub async fn available_dogs(streams: &mut Streams<'_>, fmt: Format, args: &DogsArgs) -> ExitCode {
    let url = dog_index::index_url();
    let index = match dog_index::fetch_index(&url).await {
        Ok(index) => index,
        Err(err) => {
            let message = format!("reading the dog index from {url}: {err}");
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Failure.code_str(),
                &message,
            );
            return ExitCode::Failure;
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

    let code = if fmt == Format::Table
        && matched.is_empty()
        && let Some(filter) = filter
    {
        let _ = writeln!(streams.out, "no dog matches {filter:?}");
        ExitCode::Success
    } else if fmt == Format::Table
        && let [only] = matched.as_slice()
    {
        write_outcome(render_detail(&mut *streams.out, only))
    } else {
        write_outcome(emit(
            &mut *streams.out,
            fmt,
            "dogs",
            AvailableDogRows(matched),
            streams.style,
        ))
    };

    note_index_costs(streams, fmt, skipped, sanitised);
    code
}

/// The clause both notices below end in, because both counts are
/// properties of the fetched document rather than of the search.
///
/// Without it, `shep dogs --available wombat` prints `1 entry contained
/// control characters` beside no rows at all, and the honest reading of
/// that — one of the entries you are looking at was hostile — is the wrong
/// one. Saying which set the number counts is cheaper than making the
/// number mean something else.
const INDEX_WIDE: &str = ", across the whole index rather than this listing";

/// Prints [`dog_index::Index::skipped`]/[`dog_index::Index::sanitised`] as
/// footer notices when either is non-zero — a reader who sees one has a
/// reason to go look at the index itself. Independent of the filter and of
/// how many rows matched: both counts describe the fetch, not the search,
/// so they are worth saying even alongside a detail view or a "no dog
/// matches" line, and [`INDEX_WIDE`] is what says so out loud.
fn note_index_costs(streams: &mut Streams<'_>, fmt: Format, skipped: usize, sanitised: usize) {
    if skipped > 0 {
        let _ = emit_notice(
            &mut *streams.err,
            fmt,
            "dogs_skipped",
            &format!(
                "{skipped} entr{} skipped{INDEX_WIDE}",
                if skipped == 1 { "y" } else { "ies" }
            ),
        );
    }
    if sanitised > 0 {
        let _ = emit_notice(
            &mut *streams.err,
            fmt,
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
/// operator needs to adopt it. Never reached from `--format json` — see
/// [`available_dogs`]'s own doc for why a machine consumer needs nothing
/// this adds.
///
/// # Errors
/// The underlying write failed.
fn render_detail(out: &mut dyn std::io::Write, dog: &AvailableDog) -> std::io::Result<()> {
    writeln!(out, "{} . {} . {}", dog.name, dog.package, dog.category)?;
    writeln!(out, "{}", dog.description)?;
    writeln!(out, "{} . {}", dog.license, dog.repo)?;
    writeln!(out)?;
    writeln!(out, "{}", install_line(&dog.source))?;
    writeln!(
        out,
        "{}",
        adopt_line(&dog.source, &dog.adopt_as, &dog.package)
    )
}

/// The `$ ...` line [`render_detail`] prints for how to build `source`'s
/// binary. [`DogSourceKind::Manual`] carries free-form prose instead of a
/// command — that variant's own doc says why — so it prints with no `$` at
/// all, just the two-space indent the command lines share.
fn install_line(source: &DogSourceKind) -> String {
    match source {
        DogSourceKind::CargoGit { url } => format!("  $ cargo install --git {url}"),
        DogSourceKind::GoInstall { module } => format!("  $ go install {module}@latest"),
        DogSourceKind::Manual { instructions } => format!("  {instructions}"),
    }
}

/// The `$ shep adopt ...` line [`render_detail`] prints, built from
/// `adopt_as` — never `name` or `package`. [`AvailableDog::adopt_as`]'s own
/// doc is why: a dog cannot learn the name it was adopted under, so the
/// wrong name here would ship a copy-pasteable command that silently
/// discards its entire `[dog.<name>]` config section. [`DogSourceKind::Manual`]
/// has no predictable install path to fill in, so its line names the
/// placeholder literally rather than guessing one.
fn adopt_line(source: &DogSourceKind, adopt_as: &str, package: &str) -> String {
    match source {
        DogSourceKind::CargoGit { .. } => {
            format!("  $ shep adopt {adopt_as} ~/.cargo/bin/{package}")
        }
        DogSourceKind::GoInstall { .. } => {
            format!("  $ shep adopt {adopt_as} $(go env GOPATH)/bin/{package}")
        }
        DogSourceKind::Manual { .. } => format!("  $ shep adopt {adopt_as} <path to the binary>"),
    }
}

/// Describes the sheep matching `args.selector` in detail.
pub async fn describe(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &SelectorArgs,
) -> ExitCode {
    // One pass per target, each rendered as its own detail view: `describe`
    // answers with a tree per sheep (its lambs), not a row, so merging several
    // into one payload would lose the per-sheep shape the verb exists for.
    let mut failure: Option<ExitCode> = None;
    for raw in &args.selectors {
        let selector = match parse_selector(streams, fmt, raw) {
            Ok(selector) => SelectorSpec::from(&selector),
            Err(code) => return code,
        };
        let code = describe_selector(client, streams, fmt, "describe", selector).await;
        if code != ExitCode::Success {
            failure = failure.or(Some(code));
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Lists one fold (spec §5 / §9): `Request::Describe { selector:
/// SelectorSpec::Fold(args.name) }`, delegating straight to
/// [`describe_selector`] rather than re-implementing the request/render
/// shape.
pub async fn fold(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &FoldArgs,
) -> ExitCode {
    describe_selector(
        client,
        streams,
        fmt,
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

    /// Every `envelopes.recv()` in this module is bounded by this timeout
    /// rather than left to run to completion — the same rule
    /// `commands::lifecycle`'s tests apply
    /// (`crates/shep-cli/src/commands/lifecycle.rs:613,652`) and this module
    /// originally skipped: a Task 9 reviewer mutated `ping` to render from
    /// `client.daemon()` without issuing the request, and the test meant to
    /// catch that hung past 90 seconds instead of failing. A test that fails
    /// by hanging gives CI a killed job, not a named assertion.
    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

    /// `flock` must send `Request::ListFlock` and nothing else — the same
    /// class of guard Task 8's reviewer found missing for `restart` and
    /// `delete` (mutating either to send `Request::Stop` left every test in
    /// that module green). An implementation that sent `Request::Describe`
    /// (e.g. copy-pasted from `describe`) fails this.
    #[tokio::test]
    async fn flock_asks_the_daemon_to_list_the_whole_flock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
        };
        let _ = flock(&client, &mut streams, Format::Table).await;
        let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
            .await
            .expect("flock must reach the wire; it hung instead of sending a request")
            .unwrap();
        assert_eq!(sent.body, Request::ListFlock);
    }

    /// The client-side parse is the point: `describe` must send a compiled
    /// `SelectorSpec` inside `Request::Describe`, not the raw string and not
    /// `SelectorSpec::All` regardless of input. Without this, a `describe`
    /// that always sent `SelectorSpec::All` would pass every other test in
    /// this module.
    #[tokio::test]
    async fn describe_sends_the_parsed_selector_in_its_compiled_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
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
            };
            let args = SelectorArgs {
                selectors: vec![input.into()],
            };
            let _ = describe(&client, &mut streams, Format::Table, &args).await;
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

    /// `"/[/"` is one of the only three inputs the selector grammar rejects
    /// (an unterminated regex character class). A `describe` that skipped
    /// the client-side parse would send it to the daemon and exit
    /// `NotFound` instead of failing locally.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
            };
            describe(
                &client,
                &mut streams,
                Format::Table,
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

    /// `fold <name>` must ask for exactly that fold, via `Request::Describe
    /// { selector: SelectorSpec::Fold(name) }` — not `describe`'s selector
    /// grammar, and not a wider `SelectorSpec::All`.
    #[tokio::test]
    async fn fold_asks_the_daemon_for_that_fold_and_nothing_wider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
        };
        let _ = fold(
            &client,
            &mut streams,
            Format::Table,
            &FoldArgs { name: "api".into() },
        )
        .await;
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

    /// Proves the `Response::Flock` arm inside `flock`'s own `extract`
    /// closure is wired to the right variant, not merely that some payload
    /// renders as `FlockRows` in isolation (`output/rows.rs`'s own tests
    /// already cover that) and not merely that the right request went out
    /// (`flock_asks_the_daemon_to_list_the_whole_flock`, above, covers
    /// that). `Response::Flock` and `Response::Described` both wrap a bare
    /// `Vec<ProcessInfo>`, so a match arm swapped between them compiles
    /// clean and passes every other test in this file — this is the one
    /// that fails: `FakeDaemon::reply_to_list` scripts a real
    /// `Response::Flock`, not the `Response::Pong` every other fake in this
    /// module hands back regardless of what was sent.
    #[tokio::test]
    async fn flock_response_round_trips_into_rendered_flock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_ack(&path, sample_ack()).await;
        daemon.reply_to_list(vec![sample_info()]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
            };
            flock(&client, &mut streams, Format::Json).await
        };

        assert_eq!(code, ExitCode::Success);
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["command"], "flock");
        assert_eq!(json["data"][0]["name"], "web");
    }

    /// Proves the `Response::Described` arm inside `describe_selector`'s
    /// `extract` closure is wired to the right variant, by the same
    /// reasoning as the test above — and, since `fold` delegates straight
    /// to `describe_selector` rather than duplicating it, this covers
    /// `fold`'s wiring too without a third fake-daemon round trip. Also
    /// covers Minor 5: `describe` and `fold` share this one code path and
    /// this one `command` string parameter, so an envelope with `"describe"`
    /// and `"fold"` swapped would otherwise go unasserted.
    #[tokio::test]
    async fn describe_response_round_trips_into_rendered_flock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_ack(&path, sample_ack()).await;
        daemon.reply_to_describe(vec![sample_info()]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
            };
            describe(
                &client,
                &mut streams,
                Format::Json,
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

    // --- Whole-branch review item 6: sheep_flourish had no test at all ----

    /// A sheep, `status` and nothing else pinned -- `sheep_flourish` never
    /// reads a field this doesn't set, and the id only has to be unique
    /// enough to keep two sheep from looking like the same one.
    fn sheep(id: u32, status: ProcStatus) -> ProcessInfo {
        ProcessInfo::builder(id, format!("s{id}"), status).build()
    }

    /// A registered dog -- `sheep_flourish` must never let one answer a
    /// question that is about the sheep half of the listing.
    fn dog(id: u32) -> ProcessInfo {
        ProcessInfo::builder(id, format!("d{id}"), ProcStatus::Online)
            .dog(Some(DogSource::BuiltIn))
            .build()
    }

    /// A listing with nothing registered gets the empty-flock flourish.
    #[test]
    fn sheep_flourish_fires_empty_flock_on_a_truly_empty_listing() {
        let art = sheep_flourish(&[]).expect("an empty listing must flourish");
        assert!(art.contains("no sheep in the flock yet"), "{art}");
    }

    /// A listing with dogs but no sheep is empty for this predicate's own
    /// purpose -- a dog is not a sheep, so a registry holding only dogs has
    /// exactly as much to say about the flock as one holding nothing at
    /// all.
    #[test]
    fn sheep_flourish_treats_dogs_only_as_an_empty_flock() {
        let art =
            sheep_flourish(&[dog(1), dog(2)]).expect("dogs alone must read as an empty flock");
        assert!(art.contains("no sheep in the flock yet"), "{art}");
    }

    /// Every sheep at rest gets the all-asleep flourish, naming the real
    /// sheep count -- dogs excluded from that count too.
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

    /// The exact case this module's own doc calls out: a dog still running
    /// beside an all-stopped flock must not block the "all asleep" claim --
    /// the flourish is about the sheep, and the dogs table beneath it
    /// already says the dog is up.
    #[test]
    fn a_live_dog_does_not_block_all_asleep() {
        let listing = [sheep(1, ProcStatus::Stopped), dog(2)];
        let art = sheep_flourish(&listing).expect("a live dog must not suppress all_asleep");
        assert!(art.contains("1 in the flock, all asleep"), "{art}");
    }

    /// A mixed flock -- some sheep up, some not -- has plenty to look at
    /// already, so neither flourish fires.
    #[test]
    fn sheep_flourish_is_silent_on_a_mixed_flock() {
        let listing = [sheep(1, ProcStatus::Online), sheep(2, ProcStatus::Stopped)];
        assert_eq!(
            sheep_flourish(&listing),
            None,
            "a mixed flock is not a flourish moment"
        );
    }

    /// `Stopping` -- reload's transient for the instance being replaced, not
    /// rest -- must not read as asleep merely because nothing is `Online`.
    /// A flourish claiming "all asleep" over a flock mid-reload would be
    /// wrong at the exact moment an operator is watching it happen.
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

    /// The gate `flock` actually applies -- `fmt == Format::Table &&
    /// streams.style.level.sheep()` -- pinned against a real call rather
    /// than trusted from its own doc comment. The daemon answers an empty
    /// flock on every one of these (`FakeDaemon`'s own default, unarmed),
    /// which is exactly the case `sheep_flourish` always fires on, so
    /// whether the art actually reaches `streams.out` is purely a function
    /// of the gate.
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
            let path = dir.path().join("s.sock");
            let (client, _daemon) = fake_client_on(&path).await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: Presentation::new(level, None, None, None, 80),
            };
            let _ = flock(&client, &mut streams, fmt).await;
            let printed = String::from_utf8_lossy(&out);
            assert_eq!(
                printed.contains("no sheep in the flock yet"),
                expect_art,
                "fmt={fmt:?} level={level:?}: {printed}"
            );
        }
    }
}
