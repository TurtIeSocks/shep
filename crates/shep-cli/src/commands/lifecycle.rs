//! Lifecycle verbs: `start`, `stop`, `restart`, `delete`.
//!
//! Every verb here receives an already-connected [`Client`]. `start` alone
//! resolves a target into [`AppConfig`]s before anything reaches the wire;
//! [`resolve_target`] is that resolution, kept out of the RPC so it stays
//! pure.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::ValueEnum as _;
use shep_client::{Client, START_DEADLINE};
use shep_core::config::{
    AppConfig, DeclaredApp, FlockFormat, Flockfile, FlockfileError, ResetDepth,
};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{ProcessInfo, Request, Response, SelectorSpec, SheepApplied};
use shep_core::selector::ProcessSelector;

use crate::cli::Format;
use crate::cli::{ResetMode, SelectorArgs, StartArgs, StockArgs};
use crate::commands::bounded::{Bounded, run_bounded};
use crate::commands::dogs;
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{DeletedIds, FlockRows, Render, Streams, emit, emit_flock, write_outcome};

/// What [`resolve_target`] can fail with
///
/// Mapped to exit codes by [`target_exit_code`], not by a `From` impl.
/// `start`'s daemon-side failures are `shep_client`'s errors instead.
#[derive(Debug)]
pub enum TargetError {
    /// `target` was `-` and stdin was not valid UTF-8, or the read failed.
    Stdin(std::io::Error),
    /// The extension named a recognised Flockfile format, but the file
    /// could not be read.
    Read {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying IO failure.
        source: std::io::Error,
    },
    /// The source read fine but failed Flockfile validation.
    Flockfile(FlockfileError),
    /// `target` named nothing at any tier of `start`'s precedence: no sheep
    /// by id or name, no fold, no Flockfile, and no path on disk.
    Unresolvable {
        /// The raw target string.
        target: String,
    },
    /// `--flockfile` was given for a path whose extension names no format
    /// this can read.
    UnknownFlockfileFormat {
        /// The path as the operator wrote it.
        path: PathBuf,
    },
    /// A `.js` Flockfile could not be evaluated. `node_missing` separates
    /// "install node" from "your config threw", which exit differently.
    /// No `path` field: every `detail` already names it.
    Js {
        /// What went wrong, already phrased for the operator.
        detail: String,
        /// `true` when node itself was not found on `PATH`.
        node_missing: bool,
    },
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdin(err) => write!(f, "failed to read stdin: {err}"),
            Self::Read { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Flockfile(err) => write!(f, "{err}"),
            Self::Unresolvable { target } => write!(
                f,
                "{target} is not a sheep, a fold, `-`, a recognised Flockfile, or an \
                 existing path"
            ),
            Self::UnknownFlockfileFormat { path } => write!(
                f,
                "--flockfile needs a .toml, .yaml, .yml, .json, .json5 or .js file; {} is none of those",
                path.display()
            ),
            Self::Js { detail, .. } => f.write_str(detail),
        }
    }
}

impl core::error::Error for TargetError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Stdin(err) | Self::Read { source: err, .. } => Some(err),
            Self::Flockfile(err) => Some(err),
            Self::Unresolvable { .. } | Self::UnknownFlockfileFormat { .. } | Self::Js { .. } => {
                None
            }
        }
    }
}

impl From<FlockfileError> for TargetError {
    fn from(source: FlockfileError) -> Self {
        Self::Flockfile(source)
    }
}

/// `start`'s mapping from a resolution failure to the exit code that reports it
pub(crate) fn target_exit_code(err: &TargetError) -> ExitCode {
    match err {
        TargetError::Stdin(_) => ExitCode::Failure,
        TargetError::Read { .. } | TargetError::Unresolvable { .. } => ExitCode::Usage,
        TargetError::Flockfile(_) => ExitCode::InvalidConfig,
        TargetError::UnknownFlockfileFormat { .. } => ExitCode::Usage,
        TargetError::Js {
            node_missing: true, ..
        } => ExitCode::Failure,
        TargetError::Js {
            node_missing: false,
            ..
        } => ExitCode::InvalidConfig,
    }
}

/// How long node gets to hand back a `.js` Flockfile's JSON before shep
/// kills it
///
/// 30s, against ~60ms to require a small module and the couple of seconds a
/// large dependency tree costs on a cold filesystem.
const JS_EVAL_BUDGET: Duration = Duration::from_secs(30);

/// The filename [`evaluate_js_flockfile`] writes [`JS_BRIDGE_SCRIPT`] to
///
/// Plain ASCII with no spaces: it reaches node as a bare relative argument,
/// so nothing needing quoting reaches a command line.
const JS_BRIDGE_FILE: &str = "shep-flockfile-bridge.js";

/// The bridge run by [`evaluate_js_flockfile`], written to a file rather
/// than passed to `node -e`
///
/// A `node` resolving to a `.cmd` shim has `cmd.exe` re-parse a `-e`
/// argument, and this script contains `&&`. A file's contents cross no
/// parser. The path comes from the environment, so a Flockfile path
/// containing `'`, `\` or a newline has no string literal to escape.
const JS_BRIDGE_SCRIPT: &str = "try { \
     process.stdout.write(JSON.stringify(require(process.env.SHEP_FLOCKFILE_PATH))); \
 } catch (err) { \
     process.stderr.write('[bridge saw ' + String(process.env.SHEP_FLOCKFILE_PATH) + '] ' + (err && err.message ? String(err.message) : String(err))); \
     process.exitCode = 1; \
 }";

/// Evaluates a `.js` Flockfile through node and returns its JSON
///
/// `SHEP_FLOCKFILE_PATH` carries the path, absolute: `require("x.js")`
/// without `./` resolves against `node_modules`. `budget` bounds node
/// exiting, not `require` returning. `docs/migration.md` quotes the
/// `node_missing` sentence by hand.
///
/// # Errors
/// - [`TargetError::Read`] if the path could not be canonicalized.
/// - [`TargetError::Js`] with `node_missing` if node is not on `PATH`.
/// - [`TargetError::Js`] if node failed, could not be spawned, ran past
///   `budget`, or left a process holding the output.
fn evaluate_js_flockfile(path: &Path, budget: Duration) -> Result<String, TargetError> {
    let absolute = std::fs::canonicalize(path).map_err(|source| TargetError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    // Bound until this function returns: dropping a `TempDir` deletes the
    // loader node is still reading.
    let scratch = tempfile::Builder::new()
        .prefix("shep-js-bridge")
        .tempdir()
        .map_err(|source| TargetError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let loader = scratch.path().join(JS_BRIDGE_FILE);
    std::fs::write(&loader, JS_BRIDGE_SCRIPT).map_err(|source| TargetError::Read {
        path: loader.clone(),
        source,
    })?;
    let mut command = std::process::Command::new("node");
    command
        .arg(JS_BRIDGE_FILE)
        .current_dir(scratch.path())
        .env(
            "SHEP_FLOCKFILE_PATH",
            shep_core::paths::strip_verbatim_prefix(&absolute).as_os_str(),
        )
        .stdin(std::process::Stdio::null());
    let output = match run_bounded(&mut command, budget) {
        Ok(Bounded::Exited(output)) => output,
        Ok(Bounded::Killed) => {
            return Err(TargetError::Js {
                detail: format!(
                    "node was still running {} after {}s, so shep killed it; a Flockfile \
                     module has to export its config and let node exit, and one that leaves a \
                     server listening or a timer armed does not",
                    path.display(),
                    budget.as_secs_f32()
                ),
                node_missing: false,
            });
        }
        Ok(Bounded::OutputHeldOpen) => {
            return Err(TargetError::Js {
                detail: format!(
                    "node finished with {} within {}s, but a process it left behind still \
                     holds the output shep was reading, so shep gave up on it; a Flockfile \
                     module must not leave a child of its own on node's stdout or stderr",
                    path.display(),
                    budget.as_secs_f32()
                ),
                node_missing: false,
            });
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(TargetError::Js {
                detail: format!(
                    "reading a .js Flockfile runs it through node, and node was not found on PATH; \
                     install node, or convert {} to a .toml Flockfile",
                    path.display()
                ),
                node_missing: true,
            });
        }
        Err(err) => {
            return Err(TargetError::Js {
                detail: format!("could not run node for {}: {err}", path.display()),
                node_missing: false,
            });
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("node exited non-zero and said nothing");
        return Err(TargetError::Js {
            detail: format!("node could not evaluate {}: {reason}", path.display()),
            node_missing: false,
        });
    }
    String::from_utf8(output.stdout).map_err(|_utf8_error| TargetError::Js {
        detail: format!("node printed non-UTF-8 output for {}", path.display()),
        node_missing: false,
    })
}

/// Resolves `target` into the [`AppConfig`]s `start` should register
///
/// Fixed precedence, do not widen: `-` is Flockfile JSON on `stdin`, then
/// `as_flockfile` or a recognised extension, then any path that exists.
///
/// # Errors
/// - [`TargetError::Stdin`] if `target` is `-` and `stdin` is not UTF-8.
/// - [`TargetError::Read`] if the file could not be read.
/// - [`TargetError::Flockfile`] if the source failed Flockfile validation.
/// - [`TargetError::UnknownFlockfileFormat`] under `as_flockfile`, bad extension.
/// - [`TargetError::Js`] if node failed or ran past [`JS_EVAL_BUDGET`].
/// - [`TargetError::Unresolvable`] if `target` matched none of the above.
pub fn resolve_target(
    target: &str,
    name: Option<&str>,
    stdin: &[u8],
    as_flockfile: bool,
) -> Result<Vec<AppConfig>, TargetError> {
    Ok(resolve_target_declared(target, name, stdin, as_flockfile)?
        .into_iter()
        .map(|declared| declared.config)
        .collect())
}

/// [`resolve_target`], keeping the keys each app's document literally wrote
///
/// A key set answers what a template claims, and only a document makes a
/// claim: the Flockfile tiers go through [`Flockfile::parse_declared`], and
/// a bare script path reports an empty set. A defaulted `cwd` is not
/// declared, nor are `--cwd`, `--fold` and `--interpreter`: they change what
/// a fresh app is registered with and claim nothing about an app the flock
/// already has.
///
/// # Errors
/// Every error [`resolve_target`] returns, for the same inputs.
fn resolve_target_declared(
    target: &str,
    name: Option<&str>,
    stdin: &[u8],
    as_flockfile: bool,
) -> Result<Vec<DeclaredApp>, TargetError> {
    let path = Path::new(target);
    match (target, FlockFormat::from_path(path)) {
        ("-", _) => {
            let source = String::from_utf8(stdin.to_vec()).map_err(|_utf8_error| {
                TargetError::Stdin(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stdin is not UTF-8",
                ))
            })?;
            Ok(Flockfile::parse_declared(&source, FlockFormat::Json)?)
        }
        (_, format) if as_flockfile => match format {
            Some(format) => {
                let source = std::fs::read_to_string(path).map_err(|source| TargetError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                let apps = Flockfile::parse_declared(&source, format)?;
                Ok(default_cwd_to_flockfile_dir(apps, path))
            }
            None if path.extension().and_then(|e| e.to_str()) == Some("js") => {
                let json = evaluate_js_flockfile(path, JS_EVAL_BUDGET)?;
                let apps = Flockfile::parse_declared(&json, FlockFormat::Json)?;
                Ok(default_cwd_to_flockfile_dir(apps, path))
            }
            None => Err(TargetError::UnknownFlockfileFormat {
                path: path.to_path_buf(),
            }),
        },
        (_, Some(format)) => {
            let source = std::fs::read_to_string(path).map_err(|source| TargetError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            let apps = Flockfile::parse_declared(&source, format)?;
            Ok(default_cwd_to_flockfile_dir(apps, path))
        }
        // Absolutised against the CLI's cwd, which is where `exists` is
        // answered. The daemon would otherwise resolve it against its own.
        _ if path.exists() => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(target);
            // Relative only: `canonicalize` resolves symlinks, so an
            // absolute `/var/...` comes back as `/private/var/...` on macOS.
            let script = if path.is_absolute() {
                target.to_string()
            } else {
                std::fs::canonicalize(path)
                    .map(|abs| {
                        shep_core::paths::strip_verbatim_prefix(&abs)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_else(|_| target.to_string())
            };
            let mut app = AppConfig::minimal(name.unwrap_or(stem), &script);
            // Where the operator ran `shep start`: an unset `cwd` leaves
            // the child inheriting whatever the shepherd was spawned from.
            app.cwd = std::env::current_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned());
            // Empty key sets: the command line is not a template, so a load
            // has nothing to apply to a sheep the flock already has.
            Ok(vec![DeclaredApp {
                config: app,
                declared: BTreeSet::new(),
                declared_env: BTreeSet::new(),
            }])
        }
        _ => Err(TargetError::Unresolvable {
            target: target.to_string(),
        }),
    }
}

/// Sends `body`, renders the answer through [`render_outcome`], and maps
/// every failure to its exit code
///
/// `None` for `deadline` defers to the client's default. An answer `extract`
/// does not recognise maps to [`ExitCode::Internal`].
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    command: &str,
    body: Request,
    deadline: Option<Duration>,
    extract: F,
) -> ExitCode
where
    T: Render,
    F: FnOnce(Response) -> Option<T>,
{
    match client.request_with_deadline(body, deadline).await {
        Ok(response) => match extract(response) {
            Some(payload) => render_outcome(client, streams, command, payload).await,
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

/// Parses every selector the invocation named, refusing on the first bad one
///
/// All-or-nothing: a typo in the third target must not be discovered after
/// the first two were acted on.
fn parse_selectors(
    streams: &mut Streams<'_>,
    raw: &[String],
) -> Result<Vec<SelectorSpec>, ExitCode> {
    let mut parsed = Vec::with_capacity(raw.len());
    for one in raw {
        parsed.push(SelectorSpec::from(&parse_selector(streams, one)?));
    }
    Ok(parsed)
}

/// Sends one request per selector and collects what each returned
///
/// Not atomic: `shep stop a b c` where `b` matches nothing still stops `a`
/// and `c`. Every selector is attempted, errors are rendered as they
/// happen, and the returned code is the first failure.
async fn request_each<I, B, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    selectors: &[SelectorSpec],
    deadline: Option<Duration>,
    body: B,
    extract: F,
) -> (Vec<I>, Option<ExitCode>)
where
    B: Fn(SelectorSpec) -> Request,
    F: Fn(Response) -> Option<Vec<I>>,
{
    let mut collected = Vec::new();
    let mut failure: Option<ExitCode> = None;

    for selector in selectors {
        match client
            .request_with_deadline(body(selector.clone()), deadline)
            .await
        {
            Ok(response) => match extract(response) {
                Some(mut rows) => collected.append(&mut rows),
                None => {
                    let message =
                        "the daemon answered with a response this client does not understand";
                    failure = failure.or(Some(streams.fail(ExitCode::Internal, message)));
                }
            },
            Err(err) => {
                let code = ExitCode::from(&err);
                failure = failure.or(Some(streams.fail(code, &err.to_string())));
            }
        }
    }
    (collected, failure)
}

/// Which sheep in `flock` a `start` target names
///
/// A `Name` token is tried as an exact sheep name first and as a fold name
/// second, so `shep start backed` reaches a fold called `backed`.
///
/// A wildcard passes a dog by and an exact selector reaches it, keyed off
/// [`ProcessSelector::is_exact`] so a form like `name:slot` cannot land on
/// the wrong side. The fold fallback counts as a wildcard.
fn flock_matches(selector: &ProcessSelector, flock: &[ProcessInfo]) -> Vec<ProcessInfo> {
    let sheep_only = |flock: &[ProcessInfo], keep: &dyn Fn(&ProcessInfo) -> bool| {
        flock
            .iter()
            .filter(|info| info.dog.is_none())
            .filter(|info| keep(info))
            .cloned()
            .collect::<Vec<ProcessInfo>>()
    };
    match selector {
        ProcessSelector::Name(wanted) => {
            let named: Vec<ProcessInfo> = flock
                .iter()
                .filter(|info| &info.name == wanted)
                .cloned()
                .collect();
            if !named.is_empty() {
                return named;
            }
            sheep_only(flock, &|info| info.fold.as_deref() == Some(wanted.as_str()))
        }
        exact if exact.is_exact() => flock
            .iter()
            .filter(|info| exact.matches(&info.name, info.id, info.fold.as_deref(), info.instance))
            .cloned()
            .collect(),
        wildcard => sheep_only(flock, &|info| {
            wildcard.matches(&info.name, info.id, info.fold.as_deref(), info.instance)
        }),
    }
}

/// Whether `target` carries a marker that makes it unmistakably a selector,
/// and so what to say when it matched nothing
///
/// Only the message differs. `start` still falls through to the Flockfile
/// and path tiers for every token: `/srv/app/` parses as a `/regex/` and is
/// also a directory somebody might have.
fn selector_miss(
    target: &str,
    selector: &ProcessSelector,
    flock: &[ProcessInfo],
) -> Option<String> {
    match selector {
        // Phrased off the sheep count, not `flock.is_empty()`: a wildcard
        // passes dogs by, so `all` matching nothing means no sheep.
        ProcessSelector::All if flock.iter().any(|info| info.dog.is_some()) => Some(
            "no sheep in the flock; there is nothing to start. The dogs listed \
             by `shep dogs` are not sheep and `all` never reaches them"
                .to_string(),
        ),
        ProcessSelector::All => Some("the flock is empty; there is nothing to start".to_string()),
        ProcessSelector::Fold(fold) => Some(format!("no sheep is in a fold called {fold}")),
        ProcessSelector::Regex(_) => Some(format!("no sheep matched {target}")),
        // A colon is not a path character, so `name:slot` is a marker the
        // same way `fold:` is, not a filename that could exist instead.
        ProcessSelector::Instance { name, slot } => {
            Some(format!("no instance {slot} of {name} is registered"))
        }
        // A bare name or id carries no marker and may have meant a
        // filename, so the unresolvable message naming every tier stands.
        ProcessSelector::Name(_) | ProcessSelector::Id(_) => None,
    }
}

/// Whether `target` could name a sheep or a fold at all
///
/// A sheep name may not contain a path separator and may not be `.` or `..`
/// (`shep_core::config::normalize`), so `./backed` is always the file.
/// Applied to the `Name` form only: `/web/` is a regex full of slashes.
fn is_reachable_as_a_name(selector: &ProcessSelector) -> bool {
    match selector {
        ProcessSelector::Name(name) => !name.contains(['/', '\\']) && name != "." && name != "..",
        _ => true,
    }
}

/// Renders what a lifecycle verb leaves on screen: the rows it touched
/// under `--format json`, the whole flock as a table otherwise
///
/// The table costs one extra `ListFlock` round trip. `--format json` keeps
/// the narrow payload, so a script reads `data[0]` to learn what it touched.
///
/// In table form a dog goes through [`emit_flock`], not [`emit`], so it
/// renders in the dogs table with its `SOURCE` column.
///
/// # Errors
/// Never returns a listing failure as this verb's failure: an unreachable or
/// unrecognised listing prints nothing extra and reports success.
async fn render_outcome<T: Render>(
    client: &Client,
    streams: &mut Streams<'_>,
    command: &str,
    narrow: T,
) -> ExitCode {
    if streams.fmt == Format::Json {
        return write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            command,
            narrow,
            streams.style,
        ));
    }
    let listing = flock_now(client).await;
    write_outcome(emit_flock(
        &mut *streams.out,
        streams.fmt,
        command,
        listing,
        streams.style,
    ))
}

/// Renders `err` and returns the exit code `start` reports it as.
fn fail_target(streams: &mut Streams<'_>, err: &TargetError) -> ExitCode {
    let code = target_exit_code(err);
    streams.fail(code, &err.to_string())
}

/// Whether `info` names a sheep `start` must leave alone rather than bring up
fn is_live(info: &ProcessInfo) -> bool {
    use shep_core::status::ProcStatus;
    matches!(
        info.status,
        ProcStatus::Online | ProcStatus::Starting | ProcStatus::Stopping
    )
}

/// Brings every sheep in `matched` up, and reports the ones that were already
/// up rather than replacing them
///
/// `selector` is the operator's own token, `None` when the match came from
/// a path or Flockfile. Only a token can be quoted back as a remedy, so a
/// path falls back to listing the names. One notice for the whole
/// already-up set.
///
/// Respawns go out per row, by id: a name selector reaches every instance
/// the name has, which would widen `shep start 0` to the whole app and walk
/// back over the `live`/`asleep` partition.
async fn resume_all(
    client: &Client,
    streams: &mut Streams<'_>,
    selector: Option<&str>,
    matched: &[ProcessInfo],
    started: &mut Vec<ProcessInfo>,
) -> ExitCode {
    let (live, asleep): (Vec<&ProcessInfo>, Vec<&ProcessInfo>) =
        matched.iter().partition(|info| is_live(info));

    match live.as_slice() {
        [] => {}
        [one] => {
            // The operator's own token, not the sheep's name: `shep restart
            // zam` for a `shep start 0` would replace every instance.
            let retype = selector.unwrap_or(one.name.as_str());
            let message = format!(
                "{} is already {}; `shep restart {retype}` replaces it.",
                one.name, one.status
            );
            streams.aside("start", &message);
        }
        several => {
            let names: Vec<&str> = unique_names(several);
            let retype = selector.map_or_else(|| names.join(" "), str::to_string);
            let message = format!(
                "{} are already running; `shep restart {retype}` replaces them.",
                names.join(", ")
            );
            streams.aside("start", &message);
        }
    }

    // Every row is attempted and the first failure is what the verb
    // returns, as `request_each` does for the selector-taking verbs.
    let mut failure: Option<ExitCode> = None;
    for sheep in asleep {
        let code = resume(client, streams, sheep, started).await;
        if code != ExitCode::Success {
            failure = failure.or(Some(code));
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Reports the sheep `shep add` found already registered, and changes nothing
///
/// Always [`ExitCode::Success`]: a deploy script running `shep add
/// Flockfile.toml` twice must not fail the second time. No remedy is named,
/// since this arm holds both stopped and running rows.
fn already_registered(streams: &mut Streams<'_>, matched: &[ProcessInfo]) -> ExitCode {
    let refs: Vec<&ProcessInfo> = matched.iter().collect();
    let names = unique_names(&refs);
    let message = match names.as_slice() {
        [] => return ExitCode::Success,
        [one] => format!("{one} is already registered; nothing to add."),
        several => format!(
            "{} are already registered; nothing to add.",
            several.join(", ")
        ),
    };
    streams.aside("add", &message);
    ExitCode::Success
}

/// Every distinct name in `infos`, in the order they first appear
///
/// Feeds the already-running notice, never the respawn targets, which stay
/// per-row and per-id; see [`resume_all`]. Compares against every name kept
/// so far, not the previous one, so an unsorted listing gives the same
/// answer as a sorted one.
fn unique_names<'a>(infos: &[&'a ProcessInfo]) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::with_capacity(infos.len());
    for info in infos {
        if !names.contains(&info.name.as_str()) {
            names.push(&info.name);
        }
    }
    names
}

async fn resume(
    client: &Client,
    streams: &mut Streams<'_>,
    sheep: &ProcessInfo,
    started: &mut Vec<shep_core::protocol::ProcessInfo>,
) -> ExitCode {
    let (procs, failure) = request_each(
        client,
        streams,
        &[SelectorSpec::Id(sheep.id)],
        None,
        |selector| Request::Restart { selector },
        |response| match response {
            Response::Restarted(procs) => Some(procs),
            _ => None,
        },
    )
    .await;
    // The `Restart` reply has no per-id error slot, so an `Ok` can carry an
    // `errored` sheep, which is a `start` failure. Returned without
    // extending `started`, so a failing verb leaves stdout empty.
    if any_restart_failed(&procs) {
        // By id as well as by name: this reports one row, and four failing
        // instances would otherwise print four identical messages.
        let (name, id) = (&sheep.name, sheep.id);
        let message = format!(
            "{name} (id {id}) could not be started; see `shep bleats {id}` or its log files for why"
        );
        return streams.fail(ExitCode::SpawnFailed, &message);
    }
    started.extend(procs);
    failure.unwrap_or(ExitCode::Success)
}

/// Whether any row in a `Request::Restart` reply came back `errored`
///
/// The reply has no per-id error slot, so a failed respawn arrives inside
/// an `Ok`.
fn any_restart_failed(procs: &[shep_core::protocol::ProcessInfo]) -> bool {
    procs
        .iter()
        .any(|info| info.status == shep_core::status::ProcStatus::Errored)
}

/// Gives every app that set no `cwd` the Flockfile's own directory
///
/// Fills the field without adding `cwd` to the declared key set: the
/// document did not ask for this directory, so a load must not establish it
/// on a sheep the flock already has and overwrite a `--cwd` set since.
///
/// Absolute, via `canonicalize`, because the daemon resolves a relative cwd
/// against its own. A path that cannot be canonicalised is a silent no-op,
/// and an app that sets its own `cwd` keeps it.
fn default_cwd_to_flockfile_dir(apps: Vec<DeclaredApp>, flockfile: &Path) -> Vec<DeclaredApp> {
    let Some(dir) = std::fs::canonicalize(flockfile)
        .ok()
        .and_then(|abs| abs.parent().map(Path::to_path_buf))
        .map(|dir| shep_core::paths::strip_verbatim_prefix(&dir).into_owned())
    else {
        return apps;
    };
    let dir = dir.to_string_lossy().into_owned();
    apps.into_iter()
        .map(|mut app| {
            if app.config.cwd.is_none() {
                app.config.cwd = Some(dir.clone());
            }
            app
        })
        .collect()
}

/// The flock as it stands, for deciding whether a target names a sheep that
/// already exists
///
/// A name is unique across a flock, so a target naming one can never have
/// meant "add another". Fetched once per invocation and matched locally.
/// An unreachable or unexpected answer yields an empty flock; the `Start`
/// that follows reports its own failures.
async fn flock_now(client: &Client) -> Vec<shep_core::protocol::ProcessInfo> {
    match client.request(Request::ListFlock).await {
        Ok(Response::Flock(procs)) => procs,
        _ => Vec::new(),
    }
}

/// Merges the Flockfile's declaration into each app the flock already has,
/// and tells the operator what that did
///
/// `Request::Start` on a name the flock already has adds instances rather
/// than reconciling config, so this is the request that applies an edited
/// file to a running app.
///
/// Additive: a Flockfile arrives from the app's own repository, so a load
/// appends what nobody has established and leaves alone what an operator
/// set since. `--reset` widens that. Only apps whose document declared
/// something are sent, so `shep start ./thing` applies nothing. Field names
/// only, never values, since `env` carries secrets.
async fn apply_declared(
    client: &Client,
    streams: &mut Streams<'_>,
    declared: &[DeclaredApp],
    reset: ResetDepth,
    mode: Load,
) -> ExitCode {
    let apps: Vec<DeclaredApp> = declared
        .iter()
        .filter(|app| !app.declared.is_empty())
        .cloned()
        .collect();
    if apps.is_empty() {
        return ExitCode::Success;
    }
    let report = match client
        .request_with_deadline(Request::ApplyConfig { apps, reset }, Some(START_DEADLINE))
        .await
    {
        Ok(Response::Applied(report)) => report,
        // `Response` is `#[non_exhaustive]`, so an answer this client does
        // not recognise is a daemon-side fault rather than a bad file.
        Ok(_other) => {
            let message = "the daemon answered the config load with a response this client does \
                 not understand; the Flockfile's edits are not in effect";
            return streams.fail(ExitCode::Internal, message);
        }
        // The code the class of failure already has: an unreachable daemon
        // is `DaemonUnreachable`, an expired deadline `DeadlineExceeded`, a
        // daemon-side refusal whatever its `RpcErrorCode` maps to.
        Err(err) => {
            let message = format!(
                "the Flockfile's edits could not be applied, so they are not in effect: {err}"
            );
            return streams.fail(ExitCode::from(&err), &message);
        }
    };
    let mut failure: Option<ExitCode> = None;
    for sheep in report {
        // An app with nothing to say prints nothing: a deploy re-runs the
        // same unchanged Flockfile every time.
        let Some(message) = applied_line(&sheep) else {
            continue;
        };
        if sheep.refused.is_some() {
            // Config the operator declared did not land, so exiting 0 would
            // tell `shep start F.toml && deploy` to carry on. Every app is
            // reported before the code is returned.
            failure = failure.or(Some(streams.fail(REFUSED_EXIT, &message)));
        } else {
            streams.aside(mode.verb(), &message);
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// What a per-app refusal inside an otherwise successful load exits with
///
/// One code for every refusal: [`SheepApplied::refused`] carries a sentence
/// and nothing machine-readable, so the class cannot be recovered without
/// matching on the daemon's prose.
const REFUSED_EXIT: ExitCode = ExitCode::InvalidConfig;

/// The first of two codes to have failed, `Success` when neither did
///
/// `start` works in order, so a later failure overwriting an earlier one
/// would leave the operator reading the symptom instead of the cause.
fn first_failure(earlier: ExitCode, later: ExitCode) -> ExitCode {
    if earlier == ExitCode::Success {
        later
    } else {
        earlier
    }
}

/// One line for what a load did to one app, or `None` when it did nothing
///
/// A pending field always travels with the verb that promotes it. The
/// clause is a gerund, `waiting on`, which agrees with the singular and
/// plural subjects `join(", ")` can produce.
fn applied_line(sheep: &SheepApplied) -> Option<String> {
    let name = &sheep.name;
    let mut parts = Vec::new();
    if !sheep.applied.is_empty() {
        parts.push(format!("applied {}", sheep.applied.join(", ")));
    }
    if !sheep.pending.is_empty() {
        parts.push(format!(
            "{} waiting on the next spawn (`shep reload {name}` promotes them)",
            sheep.pending.join(", ")
        ));
    }
    if let Some(refused) = &sheep.refused {
        parts.push(refused.clone());
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("{name}: {}", parts.join("; ")))
}

/// `shep.toml`'s `[interpreters]` entry for `script`'s own extension, if it
/// has one and the map names it
///
/// `Path::extension` reads a dotfile like `.bashrc` as extensionless, so an
/// entry keyed `""` can never match here.
fn mapped_interpreter(script: &str, interpreters: &BTreeMap<String, String>) -> Option<String> {
    let extension = Path::new(script).extension()?.to_str()?;
    interpreters.get(extension).cloned()
}

/// Folds `shep.toml`'s `[interpreters]` mapping and `--interpreter` onto
/// `apps`
///
/// Precedence: `shep.toml`, then a Flockfile's own `interpreter` field,
/// then the flag. Filling only `None` slots from `interpreters` is what
/// makes an app's own value outrank the map, the literal `"none"` included.
/// `flag` then overwrites every app.
fn apply_interpreters(
    apps: &mut [DeclaredApp],
    interpreters: &BTreeMap<String, String>,
    flag: Option<&str>,
) {
    if !interpreters.is_empty() {
        for app in apps.iter_mut() {
            if app.config.interpreter.is_none()
                && let Some(mapped) = mapped_interpreter(&app.config.script, interpreters)
            {
                app.config.interpreter = Some(mapped);
            }
        }
    }
    if let Some(interpreter) = flag {
        for app in apps.iter_mut() {
            app.config.interpreter = Some(interpreter.to_string());
        }
    }
}

/// The `--reset=<mode>` the operator typed, `None` when they typed nothing
///
/// Carries the mode so the two arms that refuse a reset quote back what was
/// written. A bare `--reset` is a usage error on its own.
fn reset_flag(args: &StartArgs) -> Option<String> {
    args.reset.map(|mode| {
        let name = mode
            .to_possible_value()
            .expect("every ResetMode variant has a possible value")
            .get_name()
            .to_string();
        format!("--reset={name}")
    })
}

fn reset_depth(args: &StartArgs) -> ResetDepth {
    args.reset.map_or(ResetDepth::None, ResetMode::to_depth)
}

/// Which of the two verbs that read a Flockfile is running
///
/// They share targets, resolution, merge and refusals; the difference is
/// whether anything is spawned at the end. One code path, so a document
/// cannot register differently depending on which verb read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Load {
    /// `shep start`: register what the flock does not have, and bring up what
    /// it does.
    Start,
    /// `shep add`: register what the flock does not have, and stop there.
    Add,
}

impl Load {
    /// The verb's own name, for a notice's code and for the `--format json`
    /// envelope's `command`.
    fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Add => "add",
        }
    }
}

/// Registers what `args` resolves to and brings all of it up.
pub async fn start(
    client: &Client,
    streams: &mut Streams<'_>,
    args: &StartArgs,
    discovered: Option<&Path>,
    interpreters: &BTreeMap<String, String>,
) -> ExitCode {
    load(client, streams, args, discovered, interpreters, Load::Start).await
}

/// Registers what `args` resolves to and starts none of it
///
/// The app lands registered and stopped, so a template shipping `env = {
/// DB_HOST = "" }` can be filled in before it spawns. The order is
/// register, fill in, start.
///
/// An app the flock already has is merged into and left as it is, running
/// or not, so re-running this after editing a template cannot stop a
/// service.
pub async fn add(
    client: &Client,
    streams: &mut Streams<'_>,
    args: &StartArgs,
    discovered: Option<&Path>,
    interpreters: &BTreeMap<String, String>,
) -> ExitCode {
    load(client, streams, args, discovered, interpreters, Load::Add).await
}

/// The body [`start`] and [`add`] share; see [`Load`] for what they do not
async fn load(
    client: &Client,
    streams: &mut Streams<'_>,
    args: &StartArgs,
    discovered: Option<&Path>,
    interpreters: &BTreeMap<String, String>,
    mode: Load,
) -> ExitCode {
    // `--name` renames the sheep a target becomes, and a name is unique to
    // one sheep, so it cannot mean anything across several targets.
    if args.name.is_some() && args.targets.len() > 1 {
        let message = "--name takes one target: a name belongs to one sheep";
        return streams.fail(ExitCode::Usage, message);
    }
    if args.targets.is_empty() {
        let mut started = Vec::new();
        let code = load_one(
            client,
            streams,
            args,
            None,
            discovered,
            interpreters,
            &mut started,
            mode,
        )
        .await;
        // Printed whenever the verb succeeded, not only when it touched a
        // row. A failing verb leaves stdout empty, crate-wide.
        if code == ExitCode::Success {
            let wrote = render_outcome(client, streams, mode.verb(), FlockRows(started)).await;
            if wrote != ExitCode::Success {
                return wrote;
            }
        }
        return code;
    }
    // In turn, not atomically: if the second target fails the first is
    // already up. The exit code is the first failure.
    let mut failure: Option<ExitCode> = None;
    let mut started = Vec::new();
    for target in &args.targets {
        let code = load_one(
            client,
            streams,
            args,
            Some(target),
            discovered,
            interpreters,
            &mut started,
            mode,
        )
        .await;
        if code != ExitCode::Success {
            failure = failure.or(Some(code));
        }
    }
    // One table for the whole invocation, keyed on the outcome alone and
    // never on `started` being non-empty: every row is attempted, so a
    // partly-failed fold ends non-empty, and a table there would sit beside
    // an error envelope under `--format json`.
    if failure.is_none() {
        let wrote = render_outcome(client, streams, mode.verb(), FlockRows(started)).await;
        if wrote != ExitCode::Success {
            return wrote;
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// One target's worth of [`load`]
///
/// `interpreters` is read once by the caller rather than per target.
#[allow(clippy::too_many_arguments)]
async fn load_one(
    client: &Client,
    streams: &mut Streams<'_>,
    args: &StartArgs,
    target: Option<&str>,
    discovered: Option<&Path>,
    interpreters: &BTreeMap<String, String>,
    started: &mut Vec<shep_core::protocol::ProcessInfo>,
    mode: Load,
) -> ExitCode {
    // Everything from here to `resolve_target` is the precedence in
    // `StartArgs::targets`' own help: a sheep by id or name, then a fold,
    // then a Flockfile, then a path. Each tier claims the token only if it
    // resolves there. `-` and `--flockfile` skip the flock entirely.
    let mut listing: Option<Vec<ProcessInfo>> = None;
    let mut missed: Option<String> = None;
    if let Some(token) = target
        && token != "-"
        && !args.flockfile
    {
        // Parsed client-side, as every selector-taking verb does, so a
        // malformed one is a local usage error rather than a round trip.
        let selector = match parse_selector(streams, token) {
            Ok(selector) => selector,
            Err(code) => return code,
        };
        if is_reachable_as_a_name(&selector) {
            let flock = flock_now(client).await;
            let matched = flock_matches(&selector, &flock);
            if !matched.is_empty() {
                // Returns rather than falling through: a token that resolved
                // to a sheep reads no file, so `shep start web` cannot apply
                // whatever Flockfile sits in the operator's directory. A
                // reset flag is refused: there is no template to reset to.
                if let Some(flag) = reset_flag(args) {
                    let message = format!(
                        "{flag} needs a Flockfile to reset to; {token} names a sheep, not a file"
                    );
                    return streams.fail(ExitCode::Usage, &message);
                }
                return match mode {
                    Load::Start => {
                        resume_all(client, streams, Some(token), &matched, started).await
                    }
                    // The sheep is registered, which is all `add` was asked
                    // for. Said anyway, so a printed table is explained.
                    Load::Add => already_registered(streams, &matched),
                };
            }
            // Held for the failure path below rather than reported here: the
            // token may still name a Flockfile or a path.
            missed = selector_miss(token, &selector, &flock);
            listing = Some(flock);
        }
    }

    let discovered = discovered.map(|p| p.to_string_lossy().into_owned());
    let target: &str = match (target, discovered.as_deref()) {
        (Some(target), _) => target,
        (None, Some(found)) => found,
        (None, None) => {
            let message = "no target and no Flockfile in this directory";
            return streams.fail(ExitCode::Usage, message);
        }
    };

    let stdin = if target == "-" {
        let mut buf = Vec::new();
        if let Err(source) = std::io::stdin().lock().read_to_end(&mut buf) {
            return fail_target(streams, &TargetError::Stdin(source));
        }
        buf
    } else {
        Vec::new()
    };

    let mut apps =
        match resolve_target_declared(target, args.name.as_deref(), &stdin, args.flockfile) {
            Ok(apps) => apps,
            // The token named nothing anywhere. One written unmistakably as
            // a selector is reported as one, at exit 3, matching every other
            // verb's answer for a selector that matched nothing.
            Err(TargetError::Unresolvable { .. }) if missed.is_some() => {
                let message = missed.unwrap_or_default();
                return streams.fail(ExitCode::NotFound, &message);
            }
            Err(err) => return fail_target(streams, &err),
        };

    // The other target that supplies no template: a bare script path gets
    // an empty declared set, so a reset would exit 0 having reset nothing.
    // Tested on the declared set, not the token's shape, since that is what
    // a reset acts on. Every Flockfile app declares at least `script`.
    if let Some(flag) = reset_flag(args)
        && apps.iter().all(|app| app.declared.is_empty())
    {
        let message =
            format!("{flag} needs a Flockfile to reset to; {target} is a script, not a Flockfile");
        return streams.fail(ExitCode::Usage, &message);
    }

    if let Some(fold) = &args.fold {
        for app in &mut apps {
            app.config.fold = Some(fold.clone());
        }
    }
    // After the per-app defaults above, so an explicit flag wins over both
    // the script form's default and a Flockfile's own value.
    if let Some(cwd) = &args.cwd {
        for app in &mut apps {
            app.config.cwd = Some(cwd.clone());
        }
    }
    apply_interpreters(&mut apps, interpreters, args.interpreter.as_deref());
    // The bare-name rule again, now the target is a set of named apps: a
    // name the flock already has is that sheep. Without it `shep start
    // ./thing` spawns a second copy of a one-instance app.
    let flock = match listing {
        Some(flock) => flock,
        None => flock_now(client).await,
    };
    // Every row the name has, not the first: respawns go out per row, so one
    // row standing in for a clustered app leaves the other instances down.
    let mut resumed: Vec<(DeclaredApp, Vec<ProcessInfo>)> = Vec::new();
    let mut fresh = Vec::new();
    for app in apps {
        let rows: Vec<ProcessInfo> = flock
            .iter()
            .filter(|info| info.name == app.config.name)
            .cloned()
            .collect();
        if rows.is_empty() {
            fresh.push(app);
        } else {
            resumed.push((app, rows));
        }
    }
    // Before the resumes below, since a `NeedsRespawn` field parks for the
    // next spawn and that is the resume immediately below. The code is
    // carried rather than returned: a refused field must not stop the flock
    // coming back up. The merge runs under `add` too, only the resume does not.
    let declared: Vec<DeclaredApp> = resumed.iter().map(|(app, _)| app.clone()).collect();
    let applied = apply_declared(client, streams, &declared, reset_depth(args), mode).await;
    if !resumed.is_empty() && mode == Load::Start {
        // `None`, not the operator's token: this arm reached the flock
        // through a Flockfile or a path, so there is no selector to quote.
        let existing: Vec<ProcessInfo> = resumed
            .iter()
            .flat_map(|(_, rows)| rows.iter().cloned())
            .collect();
        let code = resume_all(client, streams, None, &existing, started).await;
        if code != ExitCode::Success {
            return first_failure(applied, code);
        }
    }
    if fresh.is_empty() {
        return applied;
    }
    let apps: Vec<AppConfig> = fresh.iter().map(|app| app.config.clone()).collect();

    let (procs, failure) = request_each(
        client,
        streams, // Both requests carry the apps, so this "selector" is a
        // placeholder the body closure ignores.
        &[SelectorSpec::All],
        // `START_DEADLINE`: a cold spawn plus a readiness probe routinely
        // outruns the client's default 5s, and on `Add` a single-threaded
        // actor behind a batch of cold spawns does too.
        Some(START_DEADLINE),
        |_| match mode {
            Load::Start => Request::Start { apps: apps.clone() },
            Load::Add => Request::Add { apps: apps.clone() },
        },
        |response| match response {
            Response::Started(procs) | Response::Added(procs) => Some(procs),
            _ => None,
        },
    )
    .await;
    // Establishes what a fresh app's file declared: `Start` and `Add` write
    // nothing to the override store. `ResetDepth::None` whatever flag was
    // passed, since a reset would drop the record this call writes. Only
    // apps the daemon registered: a failed spawn is not in `procs`.
    let registered: BTreeSet<&str> = procs.iter().map(|info| info.name.as_str()).collect();
    let established: Vec<DeclaredApp> = fresh
        .into_iter()
        .filter(|app| registered.contains(app.config.name.as_str()))
        .collect();
    let recorded = apply_declared(client, streams, &established, ResetDepth::None, mode).await;
    started.extend(procs);
    first_failure(
        applied,
        first_failure(failure.unwrap_or(ExitCode::Success), recorded),
    )
}

/// Stops the sheep matching `args.selector`.
pub async fn stop(client: &Client, streams: &mut Streams<'_>, args: &SelectorArgs) -> ExitCode {
    let selectors = match parse_selectors(streams, &args.selectors) {
        Ok(selectors) => selectors,
        Err(code) => return code,
    };
    let (procs, failure) = request_each(
        client,
        streams,
        &selectors,
        None,
        |selector| Request::Stop { selector },
        |response| match response {
            Response::Stopped(procs) => Some(procs),
            _ => None,
        },
    )
    .await;
    // Printed whenever the verb did its work; see `load`.
    if !procs.is_empty() || failure.is_none() {
        let wrote = render_outcome(client, streams, "stop", FlockRows(procs)).await;
        if wrote != ExitCode::Success {
            return wrote;
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Restarts the sheep matching `args.selector`
///
/// `paths` is here so a restart that names a dog is warned about before the
/// request goes out. See [`dogs::warn_of_a_dog_a_restart_would_break`].
pub async fn restart(
    client: &Client,
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    args: &SelectorArgs,
) -> ExitCode {
    restart_within(client, streams, paths, args, dogs::VERSION_BUDGET).await
}

/// [`restart`], against a caller-chosen budget for the dog probe below
///
/// A timed-out probe answers unknown and unknown is silent, so at the
/// production budget a busy machine turns a test's subject into thin air.
pub async fn restart_within(
    client: &Client,
    streams: &mut Streams<'_>,
    paths: &ShepPaths,
    args: &SelectorArgs,
    probe: Duration,
) -> ExitCode {
    let selectors = match parse_selectors(streams, &args.selectors) {
        Ok(selectors) => selectors,
        Err(code) => return code,
    };
    // Before `request_each`: a warning that arrives after the restart is a
    // description of a state the operator is already in.
    dogs::warn_of_a_dog_a_restart_would_break(streams, paths, &selectors, probe);
    let (procs, failure) = request_each(
        client,
        streams,
        &selectors,
        None,
        |selector| Request::Restart { selector },
        |response| match response {
            Response::Restarted(procs) => Some(procs),
            _ => None,
        },
    )
    .await;
    // Named before `procs` is moved into the table below. The `Restart`
    // reply has no per-id error slot, so a failed respawn reaches here as an
    // ordinary `errored` row inside an `Ok`.
    let failed: Vec<String> = procs
        .iter()
        .filter(|info| info.status == shep_core::status::ProcStatus::Errored)
        .map(|info| info.name.clone())
        .collect();

    // Stdout stays empty on a failure, as every verb's failure path does.
    // The cost is that a `restart all` where one of ten fails lists none of
    // the nine that came back.
    if (!procs.is_empty() || failure.is_none()) && failed.is_empty() {
        let wrote = render_outcome(client, streams, "restart", FlockRows(procs)).await;
        if wrote != ExitCode::Success {
            return wrote;
        }
    }

    if !failed.is_empty() {
        let names = failed.join(", ");
        let message = format!(
            "{names} did not come back up; see `shep bleats {}` or its log files for why",
            failed[0]
        );
        return streams.fail(ExitCode::SpawnFailed, &message);
    }

    failure.unwrap_or(ExitCode::Success)
}

/// Reloads the sheep matching `args.selector`, replacing each instance with
/// a fresh one so the app has a window in which it can hand over
///
/// The client's default deadline, as `stop`/`restart`/`delete` use: the
/// daemon answers when the reload is accepted, not when the swaps finish.
/// The rows printed are the flock as it stood at acceptance.
pub async fn reload(client: &Client, streams: &mut Streams<'_>, args: &SelectorArgs) -> ExitCode {
    let selectors = match parse_selectors(streams, &args.selectors) {
        Ok(selectors) => selectors,
        Err(code) => return code,
    };
    let (procs, failure) = request_each(
        client,
        streams,
        &selectors,
        None,
        |selector| Request::Reload { selector },
        |response| match response {
            Response::Reloading(procs) => Some(procs),
            _ => None,
        },
    )
    .await;
    // Printed whenever the verb did its work; see `load`.
    if !procs.is_empty() || failure.is_none() {
        let wrote = render_outcome(client, streams, "reload", FlockRows(procs)).await;
        if wrote != ExitCode::Success {
            return wrote;
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Deletes (stops and deregisters) the sheep matching `args.selector`.
pub async fn delete(client: &Client, streams: &mut Streams<'_>, args: &SelectorArgs) -> ExitCode {
    let selectors = match parse_selectors(streams, &args.selectors) {
        Ok(selectors) => selectors,
        Err(code) => return code,
    };
    let (ids, failure) = request_each(
        client,
        streams,
        &selectors,
        None,
        |selector| Request::Delete { selector },
        |response| match response {
            Response::Deleted(ids) => Some(ids),
            _ => None,
        },
    )
    .await;
    // Printed whenever the verb did its work; see `load`. The aside below is
    // guarded separately: there is nothing to name when nothing was deleted.
    if !ids.is_empty() || failure.is_none() {
        // The listing this prints does not hold what was deleted, so the
        // ids go to stderr. Ids and not names: ids are all
        // `Response::Deleted` carries.
        let count = ids.len();
        let listed = ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<String>>()
            .join(", ");
        if count > 0 && streams.fmt != Format::Json {
            let message = match count {
                1 => format!("deleted 1 sheep, id {listed}"),
                n => format!("deleted {n} sheep, ids {listed}"),
            };
            streams.aside("delete", &message);
        }
        let wrote = render_outcome(client, streams, "delete", DeletedIds(ids)).await;
        if wrote != ExitCode::Success {
            return wrote;
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Sets `args.name`'s instance count (the stocking rate), and renders the
/// instances that remain
///
/// No `parse_selector` call, unlike every other verb here: `stock` takes a
/// name. `START_DEADLINE` rather than the client's default, since a stock-up
/// spawns processes.
pub async fn stock(client: &Client, streams: &mut Streams<'_>, args: &StockArgs) -> ExitCode {
    request_and_render(
        client,
        streams,
        "stock",
        Request::Scale {
            name: args.name.clone(),
            count: args.count,
        },
        Some(START_DEADLINE),
        |response| match response {
            Response::Scaled(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cli::Format;
    use shep_client::DEFAULT_DEADLINE;
    use shep_client::testing::{
        fake_client_answering, fake_client_capturing_envelopes, fake_client_replying_err,
    };
    use shep_core::protocol::RpcErrorCode;

    #[tokio::test]
    async fn a_discovered_flockfile_is_started_when_no_target_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"demo\"\nscript = \"/bin/sleep\"\n",
        )
        .unwrap();

        let sock = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&sock).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = StartArgs {
            targets: Vec::new(),
            name: None,
            fold: None,
            cwd: None,
            interpreter: None,
            flockfile: false,
            reset: None,
        };
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            let _ = start(
                &client,
                &mut streams,
                &args,
                Some(flockfile.as_path()),
                &BTreeMap::new(),
            )
            .await;
        }

        let sent = next_start(&mut envelopes).await;
        match sent.body {
            Request::Start { apps } => {
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].name, "demo");
            }
            other => panic!("expected a Start request, got {other:?}"),
        }
    }

    #[test]
    fn a_flockfile_app_without_a_cwd_runs_where_the_flockfile_lives() {
        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"web\"\nscript = \"./sub/server\"\n",
        )
        .unwrap();

        let apps = resolve_target(flockfile.to_str().unwrap(), None, &[], false)
            .expect("the Flockfile parses");

        // Canonical and without the verbatim prefix, the shape the app is
        // given.
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let expected = shep_core::paths::strip_verbatim_prefix(&canonical).into_owned();
        assert_eq!(
            apps[0].cwd.as_deref(),
            Some(expected.to_string_lossy().as_ref()),
            "the app runs where its Flockfile lives"
        );
    }

    #[test]
    fn a_flockfile_app_that_sets_its_own_cwd_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"web\"\nscript = \"./server\"\ncwd = \"/srv/elsewhere\"\n",
        )
        .unwrap();

        let apps = resolve_target(flockfile.to_str().unwrap(), None, &[], false)
            .expect("the Flockfile parses");

        assert_eq!(apps[0].cwd.as_deref(), Some("/srv/elsewhere"));
    }

    #[test]
    fn a_relative_script_is_resolved_against_the_callers_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("bin");
        std::fs::create_dir_all(&nested).unwrap();
        let script = nested.join("thing");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();

        // From the tempdir, the way the CLI resolves from where the
        // operator stands.
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let apps = resolve_target("./bin/thing", None, &[], false);
        std::env::set_current_dir(previous).unwrap();

        let apps = apps.expect("a script that exists must resolve");
        assert_eq!(apps.len(), 1);
        let sent = &apps[0].script;
        assert!(
            std::path::Path::new(sent).is_absolute(),
            "the daemon resolves against its own cwd, so what crosses must be \
             absolute: {sent}"
        );
        assert!(
            std::path::Path::new(sent).exists(),
            "and it must still name the real file: {sent}"
        );
        assert_eq!(apps[0].name, "thing", "the name still comes from the stem");
    }

    /// The first envelope that is not a `ListFlock`.
    async fn next_start(
        envelopes: &mut tokio::sync::mpsc::Receiver<shep_core::protocol::Envelope>,
    ) -> shep_core::protocol::Envelope {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
                .await
                .expect("start must reach the wire; it hung instead of sending a request")
                .unwrap();
            if envelope.body != Request::ListFlock {
                return envelope;
            }
        }
    }

    fn start_args(target: &str) -> StartArgs {
        StartArgs {
            targets: vec![target.to_string()],
            name: None,
            fold: None,
            cwd: None,
            interpreter: None,
            flockfile: false,
            reset: None,
        }
    }

    /// Covers `resolve_target`'s pure `-` arm. The real `std::io::stdin()`
    /// read inside `start` has no injection seam and is untested.
    #[test]
    fn a_dash_target_reads_a_flockfile_from_stdin_as_json() {
        // `app`, not `apps`: the wire key is renamed and unknown keys are a
        // hard error.
        let apps = resolve_target(
            "-",
            None,
            br#"{"app":[{"name":"web","script":"./srv"}]}"#,
            false,
        )
        .unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "web");
    }

    #[test]
    fn a_recognised_extension_parses_as_a_flockfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.toml");
        std::fs::write(&path, "[[app]]\nname = \"web\"\nscript = \"./srv\"\n").unwrap();
        let apps = resolve_target(path.to_str().unwrap(), None, b"", false).unwrap();
        assert_eq!(apps[0].name, "web");
    }

    #[test]
    fn any_other_existing_path_becomes_one_minimal_app_named_for_its_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.js");
        std::fs::write(&path, "").unwrap();
        let apps = resolve_target(path.to_str().unwrap(), None, b"", false).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "server");
        assert_eq!(apps[0].script, path.to_str().unwrap());
    }

    #[test]
    fn an_explicit_name_overrides_the_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.js");
        std::fs::write(&path, "").unwrap();
        let apps = resolve_target(path.to_str().unwrap(), Some("api"), b"", false).unwrap();
        assert_eq!(apps[0].name, "api");
    }

    #[test]
    fn a_js_file_without_the_flag_is_still_a_script() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.js");
        std::fs::write(&path, "throw new Error('this must never be evaluated')").unwrap();
        let apps = resolve_target(path.to_str().unwrap(), None, b"", false).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "server");
        assert_eq!(apps[0].script, path.to_str().unwrap());
    }

    #[test]
    fn the_flag_does_not_change_a_toml_flockfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.toml");
        std::fs::write(&path, "[[app]]\nname = \"web\"\nscript = \"./srv\"\n").unwrap();
        let with = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap();
        let without = resolve_target(path.to_str().unwrap(), None, b"", false).unwrap();
        assert_eq!(with, without);
    }

    /// Falling through to the script arm would start the operator's config
    /// file as a program.
    #[test]
    fn the_flag_refuses_an_extension_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.ini");
        std::fs::write(&path, "").unwrap();
        let err = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap_err();
        assert!(matches!(err, TargetError::UnknownFlockfileFormat { .. }));
        assert_eq!(target_exit_code(&err), ExitCode::Usage);
    }

    /// Returns `true` when node is on PATH, so a machine without node does
    /// not fail the suite
    ///
    /// `SHEP_REQUIRE_NODE` turns a missing node into a failure. The
    /// `eprintln!` alone is invisible under a passing test.
    fn node_available() -> bool {
        let ok = std::process::Command::new("node")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(
            ok || std::env::var_os("SHEP_REQUIRE_NODE").is_none(),
            "SHEP_REQUIRE_NODE is set but node is not usable on PATH"
        );
        if !ok {
            eprintln!("SKIPPED: node is not on PATH; the .js Flockfile cases did not run");
        }
        ok
    }

    #[test]
    fn a_js_flockfile_under_the_flag_is_evaluated() {
        if !node_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.js");
        std::fs::write(
            &path,
            "module.exports = { app: [{ name: \"web\", script: \"./srv\" }] };",
        )
        .unwrap();
        let apps = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "web");
    }

    #[test]
    fn a_js_flockfile_that_throws_is_an_invalid_config_quoting_node() {
        if !node_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.js");
        std::fs::write(&path, "throw new Error('sheep dip empty');").unwrap();
        let err = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap_err();
        assert_eq!(target_exit_code(&err), ExitCode::InvalidConfig);
        assert!(err.to_string().contains("sheep dip empty"), "got: {err}");
    }

    /// `setInterval` keeps node's event loop alive after `module.exports` is
    /// assigned. 200ms rather than [`JS_EVAL_BUDGET`]: what this pins is that
    /// the bound is enforced, not what the shipped bound is.
    #[test]
    fn a_js_flockfile_that_keeps_node_alive_is_killed_and_says_why() {
        if !node_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.js");
        std::fs::write(
            &path,
            "setInterval(() => {}, 1000); module.exports = { app: [] };",
        )
        .unwrap();

        let started = std::time::Instant::now();
        let err = evaluate_js_flockfile(&path, Duration::from_millis(200)).unwrap_err();

        assert_eq!(target_exit_code(&err), ExitCode::InvalidConfig);
        assert!(err.to_string().contains("still running"), "got: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "node was waited out rather than killed, in {:?}",
            started.elapsed()
        );
    }

    /// The refusal has to name the key the operator changes; serde's own
    /// message is the answer.
    #[test]
    fn a_pm2_ecosystem_shape_is_refused_naming_the_right_key() {
        if !node_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ecosystem.config.js");
        std::fs::write(
            &path,
            "module.exports = { apps: [{ name: \"web\", script: \"./srv\" }] };",
        )
        .unwrap();
        let err = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap_err();
        assert_eq!(target_exit_code(&err), ExitCode::InvalidConfig);
        let msg = err.to_string();
        assert!(msg.contains("apps"), "must name what was written: {msg}");
        assert!(msg.contains("app"), "must name what was expected: {msg}");
    }

    /// Armed through `fake_client_on`: the flock has to answer for the
    /// lookup to find anything, and only that fixture lets a test arm the
    /// reply.
    #[tokio::test]
    async fn a_target_naming_a_stopped_sheep_is_acted_on_not_resolved_as_a_path() {
        use shep_client::testing::fake_client_on;
        use shep_core::status::ProcStatus;

        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_on(&path).await;
        daemon.reply_to_list(vec![
            shep_core::protocol::ProcessInfo::builder(7, "zeus-auth", ProcStatus::Stopped).build(),
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            start(
                &client,
                &mut streams,
                &start_args("zeus-auth"),
                None,
                &BTreeMap::new(),
            )
            .await
        };

        // The path arms would have refused: no file by that name exists here.
        assert_ne!(
            code,
            ExitCode::Usage,
            "a known name must not fall through to the path arms: {}",
            String::from_utf8_lossy(&err)
        );
        assert!(
            !String::from_utf8_lossy(&err).contains("zeus-auth\" does not"),
            "and must not be reported as an unresolvable target"
        );
    }

    /// The flock a `render_outcome` test hands the fake: two sheep the verb
    /// did not touch, one it did, and a dog
    fn a_flock_with_a_dog() -> Vec<shep_core::protocol::ProcessInfo> {
        use shep_core::protocol::{DogSource, ProcessInfo};
        use shep_core::status::ProcStatus;
        vec![
            ProcessInfo::builder(0, "golbat", ProcStatus::Online).build(),
            ProcessInfo::builder(1, "koji", ProcStatus::Stopped).build(),
            ProcessInfo::builder(2, "rotom", ProcStatus::Online).build(),
            ProcessInfo::builder(3, "log-rotate", ProcStatus::Online)
                .dog(Some(DogSource::Adopted {
                    path: "/usr/local/bin/shep-log-rotate".to_string(),
                }))
                .build(),
        ]
    }

    /// The narrow payload is koji alone and the armed listing has four
    /// entries, so the two differ by row count as well as by content.
    #[tokio::test]
    async fn a_lifecycle_verb_renders_the_whole_flock_as_a_table() {
        use shep_client::testing::fake_client_on;
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let dir = tempfile::tempdir().unwrap();
        let address = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_on(&address).await;
        daemon.reply_to_list(a_flock_with_a_dog());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let touched = vec![ProcessInfo::builder(1, "koji", ProcStatus::Stopped).build()];
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            render_outcome(&client, &mut streams, "stop", FlockRows(touched)).await
        };

        assert_eq!(code, ExitCode::Success);
        let printed = String::from_utf8(out).unwrap();
        // Split at the caption rather than filtered: the two tables have
        // different columns, so a name read from the wrong one is a
        // different field.
        let (sheep, dogs) = printed
            .split_once("\nDogs\n")
            .unwrap_or_else(|| panic!("the dogs table needs its own caption: {printed}"));
        let sheep_names: Vec<&str> = sheep
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|word| *word != "NAME")
            .collect();
        assert_eq!(
            sheep_names,
            vec!["golbat", "koji", "rotom"],
            "every sheep, not only the one that was stopped, and no dog among \
             them: {printed}"
        );
        // Column 1, not 0: the dogs table leads with ID.
        let dog_names: Vec<&str> = dogs
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|word| *word != "NAME")
            .collect();
        assert_eq!(
            dog_names,
            vec!["log-rotate"],
            "the dog renders through the dogs table: {printed}"
        );
        assert!(
            dogs.contains("SOURCE") && dogs.contains("adopted"),
            "with the SOURCE column the sheep table has not: {printed}"
        );
    }

    /// A script reads `data[0]` to learn what it stopped, so four rows break
    /// it silently. `list_flock_count` proves the JSON path fetches no
    /// listing rather than fetching one and discarding it.
    #[tokio::test]
    async fn the_json_surface_keeps_the_rows_the_verb_touched() {
        use shep_client::testing::fake_client_on;
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let dir = tempfile::tempdir().unwrap();
        let address = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_on(&address).await;
        daemon.reply_to_list(a_flock_with_a_dog());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let touched = vec![ProcessInfo::builder(1, "koji", ProcStatus::Stopped).build()];
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Json,
            };
            render_outcome(&client, &mut streams, "stop", FlockRows(touched)).await
        };

        assert_eq!(code, ExitCode::Success);
        let envelope: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let names: Vec<&str> = envelope["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["koji"],
            "only what the verb touched: {envelope}"
        );
        assert_eq!(
            daemon.list_flock_count(),
            0,
            "and no listing was fetched to build it"
        );
    }

    /// A flock in which every tier of `start`'s precedence finds something
    /// different: `golbat` and `koji` are in the fold `backed`, `rotom` is in
    /// no fold, `log-rotate` is a dog, and `backed` is a fold and not a
    /// sheep.
    fn a_foldable_flock() -> Vec<shep_core::protocol::ProcessInfo> {
        use shep_core::protocol::{DogSource, ProcessInfo};
        use shep_core::status::ProcStatus;
        vec![
            ProcessInfo::builder(0, "golbat", ProcStatus::Stopped)
                .fold(Some("backed".to_string()))
                .build(),
            ProcessInfo::builder(1, "koji", ProcStatus::Stopped)
                .fold(Some("backed".to_string()))
                .build(),
            ProcessInfo::builder(2, "rotom", ProcStatus::Stopped).build(),
            ProcessInfo::builder(3, "log-rotate", ProcStatus::Online)
                .dog(Some(DogSource::BuiltIn))
                .build(),
        ]
    }

    /// The names `flock_matches` picks for `target`.
    fn matched_names(target: &str) -> Vec<String> {
        let selector = ProcessSelector::parse(target).expect("the fixture uses valid selectors");
        flock_matches(&selector, &a_foldable_flock())
            .into_iter()
            .map(|info| info.name)
            .collect()
    }

    /// The precedence `StartArgs::targets`' own help states, one case per
    /// tier.
    #[test]
    fn a_start_target_walks_the_precedence() {
        assert_eq!(
            matched_names("koji"),
            vec!["koji"],
            "tier 1: a sheep by name"
        );
        assert_eq!(matched_names("1"), vec!["koji"], "tier 1: a sheep by id");
        assert_eq!(
            matched_names("fold:backed"),
            vec!["golbat", "koji"],
            "tier 2: a fold, named as one"
        );
        assert_eq!(
            matched_names("backed"),
            vec!["golbat", "koji"],
            "tier 2: the same fold, named bare"
        );
        assert!(
            matched_names("nosuchthing").is_empty(),
            "and a token that is none of those falls through to the file tiers"
        );
    }

    /// The only fixture that can tell the two apart:
    /// `a_start_target_walks_the_precedence`'s `backed` is a fold and not a
    /// sheep, so it would pass under either order.
    #[test]
    fn a_sheep_outranks_a_fold_of_the_same_name() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let flock = vec![
            ProcessInfo::builder(0, "backed", ProcStatus::Stopped).build(),
            ProcessInfo::builder(1, "koji", ProcStatus::Stopped)
                .fold(Some("backed".to_string()))
                .build(),
        ];
        let selector = ProcessSelector::parse("backed").unwrap();
        let names: Vec<String> = flock_matches(&selector, &flock)
            .into_iter()
            .map(|info| info.name)
            .collect();
        assert_eq!(names, vec!["backed"], "the sheep, not the fold it names");
    }

    /// A dog is a process an operator installed, not a member of the flock
    /// `all` means.
    #[test]
    fn a_wildcard_passes_a_dog_by_and_an_exact_name_reaches_it() {
        assert_eq!(
            matched_names("all"),
            vec!["golbat", "koji", "rotom"],
            "no dog in the sweep"
        );
        assert_eq!(
            matched_names("log-rotate"),
            vec!["log-rotate"],
            "but naming it outright reaches it"
        );
    }

    /// `ProcessSelector::is_exact` counts `Instance` as exact. Enumerating
    /// `Name` and `Id` instead would send `metrics:0` through the
    /// dog-filtering tier.
    #[test]
    fn an_instance_selector_reaches_a_dog() {
        use shep_core::protocol::{DogSource, ProcessInfo};
        use shep_core::status::ProcStatus;

        let flock = vec![
            ProcessInfo::builder(0, "metrics", ProcStatus::Online)
                .dog(Some(DogSource::BuiltIn))
                .instance(Some(0))
                .build(),
            ProcessInfo::builder(1, "metrics", ProcStatus::Online)
                .dog(Some(DogSource::BuiltIn))
                .instance(Some(1))
                .build(),
        ];
        let selector = ProcessSelector::parse("metrics:0").unwrap();
        let ids: Vec<u32> = flock_matches(&selector, &flock)
            .into_iter()
            .map(|info| info.id)
            .collect();
        assert_eq!(ids, vec![0], "the named slot, dog or not");
    }

    /// The escape hatch for a fold that shares a name with a file in the
    /// current directory. `/web/` is full of slashes and is a regex, so the
    /// rule is on the parsed form rather than on the raw token.
    #[test]
    fn a_token_with_a_path_separator_is_never_a_name() {
        let path = ProcessSelector::parse("./backed").unwrap();
        assert!(
            !is_reachable_as_a_name(&path),
            "./backed can only be a file"
        );
        let bare = ProcessSelector::parse("backed").unwrap();
        assert!(is_reachable_as_a_name(&bare), "backed may be either");
        let regex = ProcessSelector::parse("/web/").unwrap();
        assert!(
            is_reachable_as_a_name(&regex),
            "a regex is not a name, so the separator rule does not apply to it"
        );
    }

    /// A bare name or id carries no marker and may have meant a filename, so
    /// it keeps the message that names every tier.
    #[test]
    fn a_selector_that_matched_nothing_is_reported_as_a_selector() {
        let miss = |target: &str, flock: &[shep_core::protocol::ProcessInfo]| {
            selector_miss(target, &ProcessSelector::parse(target).unwrap(), flock)
        };
        let empty: [shep_core::protocol::ProcessInfo; 0] = [];

        assert_eq!(
            miss("fold:typo", &empty).as_deref(),
            Some("no sheep is in a fold called typo")
        );
        assert_eq!(
            miss("zz-*", &empty).as_deref(),
            Some("no sheep matched zz-*")
        );
        assert_eq!(
            miss("all", &empty).as_deref(),
            Some("the flock is empty; there is nothing to start")
        );
        assert_eq!(
            miss("koji", &empty),
            None,
            "a bare name may still be a file, so the unresolvable message stands"
        );
        assert_eq!(miss("11", &empty), None, "and so may a bare id");
    }

    /// The fixture is `web`, `api`, `web`: sorted, the two `web` rows would
    /// be adjacent and comparing against the previous name alone would pass.
    /// First-seen order is asserted too, since the notice reads as a list.
    #[test]
    fn unique_names_drops_a_duplicate_that_is_not_adjacent() {
        use shep_core::status::ProcStatus;

        let rows = [
            ProcessInfo::builder(0, "web", ProcStatus::Stopped).build(),
            ProcessInfo::builder(1, "api", ProcStatus::Stopped).build(),
            ProcessInfo::builder(2, "web", ProcStatus::Stopped).build(),
        ];
        let borrowed: Vec<&ProcessInfo> = rows.iter().collect();
        assert_eq!(
            unique_names(&borrowed),
            vec!["web", "api"],
            "one entry per name, in the order each was first seen"
        );
    }

    /// Both halves in one case: a build that always says "no sheep" and one
    /// that always says "empty" each pass half of it.
    #[test]
    fn an_all_that_matched_nothing_counts_sheep_and_not_dogs() {
        use shep_core::protocol::{DogSource, ProcessInfo};
        use shep_core::status::ProcStatus;

        let all = ProcessSelector::parse("all").unwrap();
        let dogs_only = [ProcessInfo::builder(0, "log-rotate", ProcStatus::Online)
            .dog(Some(DogSource::BuiltIn))
            .build()];

        let said = selector_miss("all", &all, &dogs_only).expect("a miss is reported");
        assert!(
            said.starts_with("no sheep in the flock"),
            "a flock holding only dogs is not empty: {said}"
        );
        assert!(
            said.contains("`shep dogs`"),
            "and it says where the rows an operator can see came from: {said}"
        );

        let empty: [ProcessInfo; 0] = [];
        assert_eq!(
            selector_miss("all", &all, &empty).as_deref(),
            Some("the flock is empty; there is nothing to start"),
            "with nothing registered at all, empty is the honest word"
        );
    }

    /// Driven through the verb rather than `selector_miss`: the mapping from
    /// "matched nothing" to an exit code lives in `load_one`.
    #[tokio::test]
    async fn a_start_on_an_empty_fold_exits_not_found_without_a_start_request() {
        use shep_client::testing::fake_client_on;

        let dir = tempfile::tempdir().unwrap();
        let address = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_on(&address).await;
        daemon.reply_to_list(a_foldable_flock());

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            start(
                &client,
                &mut streams,
                &start_args("fold:typo"),
                None,
                &BTreeMap::new(),
            )
            .await
        };

        assert_eq!(code, ExitCode::NotFound);
        let said = String::from_utf8(err).unwrap();
        assert!(
            said.contains("no sheep is in a fold called typo"),
            "the refusal names the fold, not a file: {said}"
        );
        assert!(
            !said.contains("existing path"),
            "and never mentions a path nobody asked about: {said}"
        );
        assert!(out.is_empty(), "stdout stays empty on a failure");
    }

    /// Someone who typed `start` did not ask for their live service to be
    /// replaced.
    #[tokio::test]
    async fn a_target_naming_a_running_sheep_leaves_it_alone() {
        use shep_client::testing::fake_client_on;
        use shep_core::status::ProcStatus;

        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_on(&path).await;
        daemon.reply_to_list(vec![
            shep_core::protocol::ProcessInfo::builder(7, "zeus-auth", ProcStatus::Online).build(),
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            start(
                &client,
                &mut streams,
                &start_args("zeus-auth"),
                None,
                &BTreeMap::new(),
            )
            .await
        };

        assert_eq!(code, ExitCode::Success);
        let said = String::from_utf8_lossy(&err);
        assert!(said.contains("already"), "the operator is told: {said}");
        assert!(
            said.contains("shep restart zeus-auth"),
            "and pointed at the verb that would replace it: {said}"
        );
    }

    /// Three instances of one clustered app, stopped unless named in `online`
    ///
    /// The shape `a_foldable_flock` cannot show: there a name selector and a
    /// row selector pick the same set.
    fn a_clustered_flock(online: &[u32]) -> Vec<ProcessInfo> {
        use shep_core::status::ProcStatus;
        (0..3)
            .map(|id| {
                let status = if online.contains(&id) {
                    ProcStatus::Online
                } else {
                    ProcStatus::Stopped
                };
                ProcessInfo::builder(id, "zam", status).build()
            })
            .collect()
    }

    /// A fake that answers a `start` invocation end to end
    ///
    /// `failing` names the ids to answer as `errored`, which is how a spawn
    /// failure reaches this verb.
    fn a_daemon_for(
        flock: Vec<ProcessInfo>,
        failing: &'static [u32],
    ) -> impl Fn(&Request) -> Response + Send + 'static {
        use shep_core::status::ProcStatus;
        move |request| match request {
            Request::ListFlock => Response::Flock(flock.clone()),
            // One entry per app named, as `Response::Applied` promises.
            // Every entry is a no-op: these fixtures are about respawn
            // selectors.
            Request::ApplyConfig { apps, .. } => Response::Applied(
                apps.iter()
                    .map(|app| {
                        SheepApplied::new(app.config.name.clone(), Vec::new(), Vec::new(), None)
                    })
                    .collect(),
            ),
            Request::Restart { selector } => {
                let SelectorSpec::Id(id) = selector else {
                    // Never reached by a correct build. Answered rather than
                    // panicked, so the assertion naming the bug is the one
                    // that fails.
                    return Response::Restarted(Vec::new());
                };
                let status = if failing.contains(id) {
                    ProcStatus::Errored
                } else {
                    ProcStatus::Online
                };
                Response::Restarted(vec![ProcessInfo::builder(*id, "zam", status).build()])
            }
            _ => Response::Pong,
        }
    }

    /// Every selector `start` sent inside a `Request::Restart`, in order.
    fn respawns(
        envelopes: &mut tokio::sync::mpsc::UnboundedReceiver<shep_core::protocol::Envelope>,
    ) -> Vec<SelectorSpec> {
        let mut sent = Vec::new();
        while let Ok(envelope) = envelopes.try_recv() {
            if let Request::Restart { selector } = envelope.body {
                sent.push(selector);
            }
        }
        sent
    }

    /// Runs `start` against `daemon` and hands back the code, stdout and
    /// stderr, in that order.
    async fn start_against(client: &Client, target: &str) -> (ExitCode, String, String) {
        start_against_with_args(client, &start_args(target)).await
    }

    async fn start_against_with_args(
        client: &Client,
        args: &StartArgs,
    ) -> (ExitCode, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            start(client, &mut streams, args, None, &BTreeMap::new()).await
        };
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// Collapsing matched rows to distinct names would widen the request:
    /// `Id` is the only selector form that names a subset of one name's rows.
    #[tokio::test]
    async fn a_start_by_id_respawns_that_row_and_no_other() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[]), &[])).await;

        let (code, _, _) = start_against(&client, "0").await;

        assert_eq!(code, ExitCode::Success);
        assert_eq!(
            respawns(&mut envelopes),
            vec![SelectorSpec::Id(0)],
            "one respawn, for the row the operator named"
        );
    }

    /// Asserts on the declared key set as well as the name, because that is
    /// what a merge keys on: an empty set puts the same envelope on the wire
    /// and applies nothing.
    #[tokio::test]
    async fn a_flockfile_load_applies_its_declared_keys_to_an_app_the_flock_has() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"./srv\"\nmax_restarts = 99\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0, 1, 2]), &[])).await;

        let (code, _printed, _said) = start_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(code, ExitCode::Success);
        let mut sent = Vec::new();
        while let Ok(envelope) = envelopes.try_recv() {
            if let Request::ApplyConfig { apps, reset } = envelope.body {
                sent.push((apps, reset));
            }
        }
        assert_eq!(sent.len(), 1, "one request for the whole invocation");
        let (apps, reset) = &sent[0];
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].config.name, "zam");
        assert!(
            apps[0].declared.contains("max_restarts"),
            "the edited key is declared: {:?}",
            apps[0].declared
        );
        assert_eq!(
            *reset,
            ResetDepth::None,
            "additive by default; --reset is something the operator types"
        );
    }

    /// `reset_depth` is the only place that mapping happens.
    #[tokio::test]
    async fn reset_modes_choose_the_apply_config_depth_on_the_wire() {
        use shep_client::testing::fake_client_answering;

        async fn sent_depth(reset: Option<ResetMode>) -> ResetDepth {
            let dir = tempfile::tempdir().unwrap();
            let flockfile = dir.path().join("Flockfile.toml");
            std::fs::write(
                &flockfile,
                "[[app]]\nname = \"zam\"\nscript = \"./srv\"\nmax_restarts = 99\n",
            )
            .unwrap();
            let path = shep_client::testing::control_address(dir.path());
            let (client, mut envelopes) =
                fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0, 1, 2]), &[]))
                    .await;
            let mut args = start_args(flockfile.to_str().unwrap());
            args.reset = reset;
            let (code, _printed, _said) = start_against_with_args(&client, &args).await;
            assert_eq!(code, ExitCode::Success);
            let mut sent = Vec::new();
            while let Ok(envelope) = envelopes.try_recv() {
                if let Request::ApplyConfig { reset, .. } = envelope.body {
                    sent.push(reset);
                }
            }
            assert_eq!(sent.len(), 1, "one request for the whole invocation");
            sent[0]
        }

        assert_eq!(sent_depth(None).await, ResetDepth::None);
        assert_eq!(sent_depth(Some(ResetMode::File)).await, ResetDepth::File);
        assert_eq!(
            sent_depth(Some(ResetMode::Policy)).await,
            ResetDepth::Policy
        );
        assert_eq!(sent_depth(Some(ResetMode::Env)).await, ResetDepth::Env);
        assert_eq!(sent_depth(Some(ResetMode::All)).await, ResetDepth::All);
    }

    /// There is no file to reset to, so the flag is meaningless.
    #[tokio::test]
    async fn a_reset_flag_on_a_name_target_is_refused() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0, 1, 2]), &[])).await;

        let mut args = start_args("zam");
        args.reset = Some(ResetMode::Env);
        let (code, printed, said) = start_against_with_args(&client, &args).await;

        assert_eq!(code, ExitCode::Usage);
        assert!(printed.is_empty(), "a refusal prints no data envelope");
        assert!(
            said.contains("--reset=env") && said.contains("zam"),
            "the refusal must echo the mode the operator actually typed, \
             not a bare --reset that is now its own usage error: {said}"
        );
    }

    /// The command line is not a template, so the flag would otherwise exit
    /// 0 having reset nothing.
    #[tokio::test]
    async fn a_reset_flag_on_a_bare_script_target_is_refused() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("zam");
        std::fs::write(&script, "#!/bin/sh\nsleep 1\n").unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0, 1, 2]), &[])).await;

        let mut args = start_args(script.to_str().unwrap());
        args.reset = Some(ResetMode::All);
        let (code, printed, said) = start_against_with_args(&client, &args).await;

        assert_eq!(code, ExitCode::Usage);
        assert!(printed.is_empty(), "a refusal prints no data envelope");
        assert!(
            said.contains("--reset=all") && said.contains("zam"),
            "the refusal must echo the mode the operator actually typed, \
             not a bare --reset that is now its own usage error: {said}"
        );
        assert!(
            applies(&mut envelopes).is_empty(),
            "a refused reset sends no load"
        );
    }

    /// The command line supplied every value, including a `cwd` of wherever
    /// the operator stood, so applying it would move a running app's
    /// directory.
    #[tokio::test]
    async fn a_bare_script_target_applies_nothing() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("zam");
        std::fs::write(&script, "#!/bin/sh\nsleep 1\n").unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0, 1, 2]), &[])).await;

        let (code, _printed, _said) = start_against(&client, script.to_str().unwrap()).await;

        assert_eq!(code, ExitCode::Success);
        assert!(
            applies(&mut envelopes).is_empty(),
            "a script path declares nothing, so there is nothing to apply"
        );
    }

    /// A list of names nobody can act on is a report nobody can use.
    #[tokio::test]
    async fn a_load_names_the_verb_that_promotes_what_is_pending() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"./srv\"\nmax_restarts = 99\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let flock = a_clustered_flock(&[0, 1, 2]);
        let (client, _envelopes) = fake_client_answering(&path, move |request| match request {
            Request::ListFlock => Response::Flock(flock.clone()),
            Request::ApplyConfig { .. } => Response::Applied(vec![SheepApplied::new(
                "zam",
                vec!["max_restarts".to_string()],
                vec!["env".to_string()],
                None,
            )]),
            _ => Response::Pong,
        })
        .await;

        let (code, _printed, said) = start_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(code, ExitCode::Success);
        assert!(
            said.contains("applied max_restarts"),
            "what landed is named: {said}"
        );
        assert!(
            said.contains("shep reload zam"),
            "and what promotes the rest: {said}"
        );
    }

    /// Both apps are in one reply: one refusal among successes still fails
    /// the verb, and the app that did apply is still reported.
    #[tokio::test]
    async fn a_load_that_refused_an_app_exits_non_zero_and_still_reports_the_rest() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"./srv\"\nmax_restarts = 99\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let flock = a_clustered_flock(&[0, 1, 2]);
        let (client, _envelopes) = fake_client_answering(&path, move |request| match request {
            Request::ListFlock => Response::Flock(flock.clone()),
            Request::ApplyConfig { .. } => Response::Applied(vec![
                SheepApplied::new("zam", vec!["max_restarts".to_string()], Vec::new(), None),
                SheepApplied::new(
                    "api",
                    Vec::new(),
                    Vec::new(),
                    Some("instances: this load never reshapes a flock".to_string()),
                ),
            ]),
            _ => Response::Pong,
        })
        .await;

        let (code, printed, said) = start_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(
            code,
            ExitCode::InvalidConfig,
            "a refused load is a failed load: {said}"
        );
        assert!(
            said.contains("never reshapes a flock"),
            "the refusal reaches the operator: {said}"
        );
        assert!(
            said.contains("applied max_restarts"),
            "and so does what did land, beside it: {said}"
        );
        assert!(
            printed.is_empty(),
            "a failed verb leaves stdout empty, so `--format json` never \
             carries a data envelope beside an error one: {printed}"
        );
    }

    /// A load whose answer this client cannot read means the whole file went
    /// nowhere, and a daemon-side fault takes its own class's code rather
    /// than the refusal's.
    #[tokio::test]
    async fn a_load_that_failed_for_another_reason_exits_with_its_own_class() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"./srv\"\nmax_restarts = 99\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let flock = a_clustered_flock(&[0, 1, 2]);
        // `Pong` to an `ApplyConfig`. `Response` is `#[non_exhaustive]`, so
        // this is also the shape a newer daemon produces.
        let (client, _envelopes) = fake_client_answering(&path, move |request| match request {
            Request::ListFlock => Response::Flock(flock.clone()),
            _ => Response::Pong,
        })
        .await;

        let (code, _printed, said) = start_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(
            code,
            ExitCode::Internal,
            "its own class, not the refusal code: {said}"
        );
        assert!(
            said.contains("not in effect"),
            "and the operator is told the edits went nowhere: {said}"
        );
    }

    #[test]
    fn a_load_that_did_nothing_to_an_app_says_nothing_about_it() {
        let quiet = SheepApplied::new("zam", Vec::new(), Vec::new(), None);
        assert_eq!(applied_line(&quiet), None);

        let refused = SheepApplied::new(
            "zam",
            Vec::new(),
            Vec::new(),
            Some("zam is not registered".to_string()),
        );
        assert_eq!(
            applied_line(&refused).as_deref(),
            Some("zam: zam is not registered")
        );
    }

    /// Every `Request::ApplyConfig` `start` sent, in order.
    fn applies(
        envelopes: &mut tokio::sync::mpsc::UnboundedReceiver<shep_core::protocol::Envelope>,
    ) -> Vec<Vec<String>> {
        let mut sent = Vec::new();
        while let Ok(envelope) = envelopes.try_recv() {
            if let Request::ApplyConfig { apps, .. } = envelope.body {
                sent.push(apps.into_iter().map(|app| app.config.name).collect());
            }
        }
        sent
    }

    /// `Flockfile::parse_declared` deserializes the source a second time
    /// into a `serde_json::Value`. One case per format, each carrying a
    /// nested table and a list, the two shapes a generic pass is most likely
    /// to handle differently from a typed one.
    #[test]
    fn every_format_survives_the_second_deserialize_resolve_target_now_runs() {
        let dir = tempfile::tempdir().unwrap();
        let sources = [
            (
                "Flockfile.toml",
                "[[app]]\nname = \"web\"\nscript = \"./srv\"\nargs = [\"-p\", \"80\"]\n\
                 [app.env]\nMODE = \"live\"\n",
            ),
            (
                "Flockfile.yaml",
                "app:\n  - name: web\n    script: ./srv\n    args: ['-p', '80']\n\
                 \n    env:\n      MODE: live\n",
            ),
            (
                "Flockfile.json",
                "{\"app\":[{\"name\":\"web\",\"script\":\"./srv\",\
                 \"args\":[\"-p\",\"80\"],\"env\":{\"MODE\":\"live\"}}]}",
            ),
            (
                "Flockfile.json5",
                "{app:[{name:'web',script:'./srv',args:['-p','80'],env:{MODE:'live'}}]}",
            ),
        ];
        for (filename, source) in sources {
            let path = dir.path().join(filename);
            std::fs::write(&path, source).unwrap();
            let target = path.to_str().unwrap();

            // The validating pass alone, the baseline the second must not
            // narrow.
            let format = FlockFormat::from_path(&path).expect("a recognised extension");
            let first = Flockfile::parse(source, format).expect("the validating pass accepts it");

            let apps = resolve_target(target, None, b"", false)
                .unwrap_or_else(|err| panic!("{filename} was refused after the first pass: {err}"));
            assert_eq!(
                apps.len(),
                first.apps.len(),
                "{filename}: both passes see the same apps"
            );
            assert_eq!(apps[0].name, "web", "{filename}");
            assert_eq!(
                apps[0].args,
                vec!["-p".to_string(), "80".to_string()],
                "{filename}"
            );
            assert_eq!(
                apps[0].env.get("MODE").map(String::as_str),
                Some("live"),
                "{filename}: a nested table survives both passes"
            );
        }
    }

    /// A bare start already reads the file to decide what to run, so it
    /// extends no trust the invocation had not extended already. The only
    /// case here that reaches the apply through `discovered`.
    #[tokio::test]
    async fn a_discovered_flockfile_applies_to_an_app_the_flock_already_has() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"./srv\"\nmax_restarts = 99\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0, 1, 2]), &[])).await;

        let args = StartArgs {
            targets: Vec::new(),
            name: None,
            fold: None,
            cwd: None,
            interpreter: None,
            flockfile: false,
            reset: None,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            start(
                &client,
                &mut streams,
                &args,
                Some(flockfile.as_path()),
                &BTreeMap::new(),
            )
            .await
        };

        assert_eq!(code, ExitCode::Success);
        assert_eq!(
            applies(&mut envelopes),
            vec![vec!["zam".to_string()]],
            "a discovered load applies exactly as an explicit one does"
        );
    }

    /// A Flockfile arrives from an app's own repository, so a load is
    /// something the operator asked for by naming a file. The Flockfile here
    /// declares the same app under a different script, so a build that read
    /// it would have something to send.
    #[tokio::test]
    async fn start_by_name_sends_no_apply_config_even_with_a_flockfile_present() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"./from-the-repository\"\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        // Every instance online, so the only requests this invocation can
        // produce are the listings.
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0, 1, 2]), &[])).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            start(
                &client,
                &mut streams,
                &start_args("zam"),
                Some(&flockfile),
                &BTreeMap::new(),
            )
            .await
        };

        assert_eq!(code, ExitCode::Success, "the name resolved to the flock");
        assert!(
            applies(&mut envelopes).is_empty(),
            "a name target applies nothing"
        );
    }

    /// `resume_all` partitions matched rows into live and asleep. Sending a
    /// name walks back over the row it just set aside, no `Id` needed.
    #[tokio::test]
    async fn a_start_never_respawns_a_row_that_was_already_up() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0]), &[])).await;

        let (code, printed, said) = start_against(&client, "all").await;

        assert_eq!(code, ExitCode::Success);
        let _ = printed;
        assert_eq!(
            respawns(&mut envelopes),
            vec![SelectorSpec::Id(1), SelectorSpec::Id(2)],
            "the two that were down, and not the one that was up"
        );
        assert!(said.contains("already"), "the live one is reported: {said}");
    }

    /// `shep restart zam` for a `shep start 0` replaces every instance. Only
    /// a path or Flockfile target falls back to the name.
    #[tokio::test]
    async fn the_already_up_notice_quotes_the_operators_own_token() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[0]), &[])).await;

        let (code, _, said) = start_against(&client, "0").await;

        assert_eq!(code, ExitCode::Success);
        assert!(
            said.contains("`shep restart 0`"),
            "the one row the operator named: {said}"
        );
        assert!(
            !said.contains("`shep restart zam`"),
            "never the name, which would replace every instance of it: {said}"
        );
    }

    #[tokio::test]
    async fn a_row_that_cannot_spawn_does_not_abandon_the_rows_after_it() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_for(a_clustered_flock(&[]), &[0])).await;

        let (code, printed, said) = start_against(&client, "all").await;

        assert_eq!(code, ExitCode::SpawnFailed, "the first failure is the code");
        assert_eq!(
            respawns(&mut envelopes),
            vec![
                SelectorSpec::Id(0),
                SelectorSpec::Id(1),
                SelectorSpec::Id(2)
            ],
            "every row is attempted, not just the ones before the failure"
        );
        assert!(
            said.contains("id 0"),
            "and the failure names the row, not just the app: {said}"
        );
        assert!(
            printed.is_empty(),
            "a failed verb leaves stdout empty even though rows 1 and 2 came \
             up, the rule `cli_e2e`'s assert_json_error pins crate-wide: \
             {printed}"
        );
    }

    /// Both halves: three rows respawned, and none skipped because the
    /// representative row happened to be the live one.
    #[tokio::test]
    async fn a_flockfile_naming_a_clustered_app_resumes_every_instance() {
        use shep_client::testing::fake_client_answering;

        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("flock.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"./zam\"\ninstances = 3\n",
        )
        .unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&socket, a_daemon_for(a_clustered_flock(&[]), &[])).await;

        let (code, _, _) = start_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(code, ExitCode::Success);
        assert_eq!(
            respawns(&mut envelopes),
            vec![
                SelectorSpec::Id(0),
                SelectorSpec::Id(1),
                SelectorSpec::Id(2)
            ],
            "every row the name has, not the first one"
        );
    }

    #[tokio::test]
    async fn a_target_that_matches_nothing_is_a_usage_error_naming_what_was_tried() {
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
            start(
                &client,
                &mut streams,
                &start_args("./does-not-exist"),
                None,
                &BTreeMap::new(),
            )
            .await
        };
        assert_eq!(code, ExitCode::Usage);
        // Nothing reaches the daemon: `./does-not-exist` carries a path
        // separator, so `start` skips the flock lookup. A target with no
        // separator does ask.
        assert!(
            envelopes.try_recv().is_err(),
            "a target that can only be a path costs no round trip, and an \
             unresolvable one must never become a Start"
        );
        assert!(String::from_utf8(err).unwrap().contains("./does-not-exist"));
    }

    /// Every selector-taking verb must send a compiled `SelectorSpec` inside
    /// its own `Request` variant
    ///
    /// The whole `sent.body` is asserted, so a verb sending the wrong request
    /// kind is caught. Also pins that all four pass `deadline: None`, visible
    /// on the wire as `DEFAULT_DEADLINE`.
    #[tokio::test]
    async fn a_selector_reaches_the_wire_in_its_compiled_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        // No `shep.toml` here, so `restart`'s dog check finds nothing
        // adopted and spawns nothing.
        let paths = ShepPaths::resolve(&|_| None, dir.path());

        #[derive(Clone, Copy, Debug)]
        enum Verb {
            Stop,
            Restart,
            Reload,
            Delete,
        }

        for verb in [Verb::Stop, Verb::Restart, Verb::Reload, Verb::Delete] {
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
                let expected_body = match verb {
                    Verb::Stop => Request::Stop { selector: expected },
                    Verb::Restart => Request::Restart { selector: expected },
                    Verb::Reload => Request::Reload { selector: expected },
                    Verb::Delete => Request::Delete { selector: expected },
                };
                let _ = match verb {
                    Verb::Stop => stop(&client, &mut streams, &args).await,
                    Verb::Restart => restart(&client, &mut streams, &paths, &args).await,
                    Verb::Reload => reload(&client, &mut streams, &args).await,
                    Verb::Delete => delete(&client, &mut streams, &args).await,
                };
                let sent = envelopes.recv().await.unwrap();
                assert_eq!(sent.body, expected_body, "verb={verb:?} input={input}");
                // `request_with_deadline` fills in `DEFAULT_DEADLINE`, so
                // an envelope carrying exactly that is the signal that the
                // call site passed `None`.
                assert_eq!(
                    sent.deadline_ms,
                    Some(u64::try_from(DEFAULT_DEADLINE.as_millis()).unwrap()),
                    "verb={verb:?} input={input} must defer to the client's default deadline"
                );
            }
        }
    }

    /// A `$SHEP_HOME`, a dog binary answering `--version` with `protocol`,
    /// and a `shep.toml` that has adopted it under `name`
    ///
    /// Written through `ShepToml::adopt_dog`, so a test dog is recorded the
    /// way `shep adopt` records a real one.
    fn adopted_dog(dir: &Path, name: &str, answer: &str) -> ShepPaths {
        let paths = ShepPaths::resolve(&|_| None, dir);
        let binary = dir.join(format!("shep-{name}"));
        std::fs::write(&binary, format!("#!/bin/sh\n{answer}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        crate::commands::shep_toml::ShepToml::edit(&paths.daemon_config, |cfg| {
            cfg.adopt_dog(name, &binary);
        })
        .unwrap();
        paths
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    /// A verb skipping the client-side parse would send it and exit
    /// `NotFound`.
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
            stop(
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
    async fn a_not_found_reply_exits_not_found_rather_than_being_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let code = stop(
            &client,
            &mut streams,
            &SelectorArgs {
                selectors: vec!["ghost".into()],
            },
        )
        .await;
        assert_eq!(code, ExitCode::NotFound);
    }

    /// Bounded by `timeout`: `start` returns early whenever `resolve_target`
    /// fails, before any request is built, so a regression would hang on
    /// `envelopes.recv()`. A `.toml` name would not substitute for the
    /// fixture, since that extension routes into `Flockfile::parse`.
    #[tokio::test]
    async fn start_asks_for_the_longer_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let srv = dir.path().join("srv");
        std::fs::write(&srv, "").unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };

        let _ = start(
            &client,
            &mut streams,
            &start_args(srv.to_str().unwrap()),
            None,
            &BTreeMap::new(),
        )
        .await;

        let sent = next_start(&mut envelopes).await;
        assert_eq!(
            sent.deadline_ms,
            Some(u64::try_from(START_DEADLINE.as_millis()).unwrap())
        );
        // `cwd` comes along: a script started by path runs where the
        // operator stood. The redacted `Debug` does not print it, so a
        // mismatch reads as two identical-looking values.
        let mut expected = AppConfig::minimal("srv", srv.to_str().unwrap());
        expected.cwd = std::env::current_dir()
            .ok()
            .map(|dir| dir.to_string_lossy().into_owned());
        assert_eq!(
            sent.body,
            Request::Start {
                apps: vec![expected]
            }
        );
    }

    /// The fold has to land on the `AppConfig` that reaches the wire:
    /// deleting the `if let Some(fold)` loop leaves every other test here
    /// green.
    #[tokio::test]
    async fn a_fold_flag_lands_on_the_resolved_app() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let srv = dir.path().join("srv");
        std::fs::write(&srv, "").unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let mut args = start_args(srv.to_str().unwrap());
        args.fold = Some("backend".to_string());

        let _ = start(&client, &mut streams, &args, None, &BTreeMap::new()).await;

        let sent = next_start(&mut envelopes).await;
        match sent.body {
            Request::Start { apps } => {
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].fold.as_deref(), Some("backend"));
            }
            other => panic!("expected Request::Start, got {other:?}"),
        }
    }

    /// Matched with the dot stripped, absent for a name with no extension,
    /// and absent for a dotfile whose one dot leads rather than separates.
    #[test]
    fn mapped_interpreter_reads_the_extension_without_its_dot() {
        let mut interpreters = BTreeMap::new();
        interpreters.insert("js".to_string(), "node".to_string());

        assert_eq!(
            mapped_interpreter("server.js", &interpreters),
            Some("node".to_string())
        );
        assert_eq!(mapped_interpreter("server", &interpreters), None);
        assert_eq!(mapped_interpreter(".bashrc", &interpreters), None);
        assert_eq!(mapped_interpreter("server.py", &interpreters), None);
    }

    /// Layer 1. Without it the quick start `welcome.rs` that `--help`
    /// advertises fails with `spawn_failed`.
    #[tokio::test]
    async fn a_shep_toml_mapping_fills_an_unset_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let srv = dir.path().join("srv.js");
        std::fs::write(&srv, "").unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let mut interpreters = BTreeMap::new();
        interpreters.insert("js".to_string(), "node".to_string());

        let _ = start(
            &client,
            &mut streams,
            &start_args(srv.to_str().unwrap()),
            None,
            &interpreters,
        )
        .await;

        let sent = next_start(&mut envelopes).await;
        match sent.body {
            Request::Start { apps } => {
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].interpreter.as_deref(), Some("node"));
            }
            other => panic!("expected Request::Start, got {other:?}"),
        }
    }

    /// Layer 2: the mapping is a fallback, not a policy.
    #[tokio::test]
    async fn a_flockfile_interpreter_outranks_the_shep_toml_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"demo\"\nscript = \"server.js\"\ninterpreter = \"bun\"\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let mut interpreters = BTreeMap::new();
        interpreters.insert("js".to_string(), "node".to_string());

        let _ = start(
            &client,
            &mut streams,
            &start_args(flockfile.to_str().unwrap()),
            None,
            &interpreters,
        )
        .await;

        let sent = next_start(&mut envelopes).await;
        match sent.body {
            Request::Start { apps } => {
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].interpreter.as_deref(), Some("bun"));
            }
            other => panic!("expected Request::Start, got {other:?}"),
        }
    }

    /// Layer 3, the one-off override typed on the command line.
    #[tokio::test]
    async fn the_interpreter_flag_outranks_a_flockfiles_own_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"demo\"\nscript = \"server.js\"\ninterpreter = \"bun\"\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let mut args = start_args(flockfile.to_str().unwrap());
        args.interpreter = Some("deno".to_string());
        let mut interpreters = BTreeMap::new();
        interpreters.insert("js".to_string(), "node".to_string());

        let _ = start(&client, &mut streams, &args, None, &interpreters).await;

        let sent = next_start(&mut envelopes).await;
        match sent.body {
            Request::Start { apps } => {
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].interpreter.as_deref(), Some("deno"));
            }
            other => panic!("expected Request::Start, got {other:?}"),
        }
    }

    #[test]
    fn any_restart_failed_is_true_only_for_an_errored_row() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let online = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
        let errored = ProcessInfo::builder(2, "worker", ProcStatus::Errored).build();
        assert!(!any_restart_failed(std::slice::from_ref(&online)));
        assert!(any_restart_failed(&[online, errored]));
    }

    /// `stock` parses no selector, and a copy-pasted `parse_selector` would
    /// send a `SelectorSpec::Name("web")` frame the daemon has no arm for.
    #[tokio::test]
    async fn the_request_carries_the_app_name_and_the_count() {
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
        let _ = stock(
            &client,
            &mut streams,
            &StockArgs {
                name: "web".to_string(),
                count: 4,
            },
        )
        .await;

        let envelope = envelopes.recv().await.unwrap();
        assert_eq!(
            envelope.body,
            Request::Scale {
                name: "web".to_string(),
                count: 4,
            }
        );
    }

    /// A count of 0 is the shape an operator will type, and it has to come
    /// back as exit 4 with the daemon's own sentence.
    #[tokio::test]
    async fn an_invalid_stock_exits_invalid_config_and_prints_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _served) = fake_client_replying_err(
            &path,
            RpcErrorCode::InvalidConfig,
            "an app runs at least one instance; use `shep delete web` to remove it",
        )
        .await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            stock(
                &client,
                &mut streams,
                &StockArgs {
                    name: "web".to_string(),
                    count: 1,
                },
            )
            .await
        };
        assert_eq!(code, ExitCode::InvalidConfig);
        assert!(
            String::from_utf8(err).unwrap().contains("shep delete web"),
            "the daemon's own sentence has to reach the operator"
        );
    }

    /// A daemon that registers what it is asked to: an `Add` answers one
    /// `Stopped` row per app, a `Start` one `Online` row
    ///
    /// A `Start` is answered rather than refused, so a build that sent one
    /// from `shep add` fails on the assertion naming the request.
    fn a_daemon_that_registers(
        flock: Vec<ProcessInfo>,
    ) -> impl Fn(&Request) -> Response + Send + 'static {
        use shep_core::status::ProcStatus;
        let rows = |apps: &[AppConfig], status: ProcStatus| -> Vec<ProcessInfo> {
            apps.iter()
                .enumerate()
                .map(|(i, app)| {
                    ProcessInfo::builder(u32::try_from(i).unwrap(), &app.name, status).build()
                })
                .collect()
        };
        move |request| match request {
            Request::ListFlock => Response::Flock(flock.clone()),
            Request::ApplyConfig { apps, .. } => Response::Applied(
                apps.iter()
                    .map(|app| {
                        SheepApplied::new(app.config.name.clone(), Vec::new(), Vec::new(), None)
                    })
                    .collect(),
            ),
            Request::Add { apps } => Response::Added(rows(apps, ProcStatus::Stopped)),
            Request::Start { apps } => Response::Started(rows(apps, ProcStatus::Online)),
            Request::Restart { .. } => Response::Restarted(Vec::new()),
            _ => Response::Pong,
        }
    }

    /// Every request body an invocation put on the wire, in order
    ///
    /// One drain rather than a helper per request kind: a channel can only be
    /// emptied once.
    fn sent(
        envelopes: &mut tokio::sync::mpsc::UnboundedReceiver<shep_core::protocol::Envelope>,
    ) -> Vec<Request> {
        let mut bodies = Vec::new();
        while let Ok(envelope) = envelopes.try_recv() {
            bodies.push(envelope.body);
        }
        bodies
    }

    /// Runs `shep add` against `target` and hands back its code and streams.
    async fn add_against(client: &Client, target: &str) -> (ExitCode, String, String) {
        let args = start_args(target);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            add(client, &mut streams, &args, None, &BTreeMap::new()).await
        };
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// `add` and `start` are one code path, so this guards a mode that
    /// leaked rather than a missing feature.
    #[tokio::test]
    async fn add_registers_a_fresh_app_and_sends_no_start() {
        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"demo\"\nscript = \"/bin/sleep\"\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_that_registers(Vec::new())).await;

        let (code, _out, _err) = add_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(code, ExitCode::Success);
        let sent = sent(&mut envelopes);
        let registered: Vec<&str> = sent
            .iter()
            .filter_map(|request| match request {
                Request::Add { apps } => Some(apps[0].name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(registered, vec!["demo"], "one registration, for the app");
        assert!(
            !sent.iter().any(|r| matches!(r, Request::Start { .. })),
            "nothing was started"
        );
    }

    /// `add` exists so an operator can fill in the empty `env` keys a
    /// template shipped, and a key nothing established is one the next load
    /// overwrites.
    #[tokio::test]
    async fn add_establishes_the_keys_the_template_declared() {
        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"demo\"\nscript = \"/bin/sleep\"\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_that_registers(Vec::new())).await;

        let (code, _out, _err) = add_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(code, ExitCode::Success);
        let sent = sent(&mut envelopes);
        let add_at = sent
            .iter()
            .position(|r| matches!(r, Request::Add { .. }))
            .expect("the app was registered");
        let apply_at = sent
            .iter()
            .position(|r| matches!(r, Request::ApplyConfig { .. }))
            .expect("its declared keys were established");
        assert!(
            add_at < apply_at,
            "the app is registered before its keys are established"
        );
    }

    /// Re-running a template after editing it is ordinary, so the file's new
    /// keys merge in and the running child survives it.
    #[tokio::test]
    async fn add_merges_into_a_running_app_without_replacing_it() {
        let dir = tempfile::tempdir().unwrap();
        let flockfile = dir.path().join("Flockfile.toml");
        std::fs::write(
            &flockfile,
            "[[app]]\nname = \"zam\"\nscript = \"/bin/sleep\"\n",
        )
        .unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_that_registers(a_clustered_flock(&[0]))).await;

        let (code, _out, _err) = add_against(&client, flockfile.to_str().unwrap()).await;

        assert_eq!(code, ExitCode::Success);
        let sent = sent(&mut envelopes);
        assert!(
            sent.iter()
                .any(|r| matches!(r, Request::ApplyConfig { .. })),
            "the template was merged in"
        );
        assert!(
            !sent.iter().any(|r| matches!(r, Request::Restart { .. })),
            "and nothing was replaced"
        );
        assert!(
            !sent.iter().any(|r| matches!(r, Request::Add { .. })),
            "the flock already has it, so there was nothing to register"
        );
    }

    /// A name target reads no Flockfile, and `add` sits behind that same
    /// boundary. Nothing is left for the verb to do but say so.
    #[tokio::test]
    async fn add_by_name_registers_nothing_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) =
            fake_client_answering(&path, a_daemon_that_registers(a_clustered_flock(&[0]))).await;

        let (code, _out, said) = add_against(&client, "zam").await;

        assert_eq!(code, ExitCode::Success);
        let sent = sent(&mut envelopes);
        assert!(
            !sent.iter().any(|r| matches!(r, Request::Add { .. })),
            "the flock already has it"
        );
        assert!(
            !sent
                .iter()
                .any(|r| matches!(r, Request::ApplyConfig { .. })),
            "a name target reads no file, so there is nothing to apply"
        );
        assert!(
            said.contains("already registered"),
            "the operator is told why nothing happened: {said}"
        );
    }

    /// Wall-clock tests, skipped by every CI job but the serial `slow` one
    ///
    /// The `restart` cases fork and exec a dog's binary inside a budget. A
    /// probe that runs out of budget answers unknown, and unknown is silent,
    /// so contention turns their subject into thin air. A `/bin/sh` probe
    /// takes single-digit milliseconds idle and over a second at
    /// `--test-threads=64`. The node case needs a real node to start and
    /// exit inside the budget, which is a claim about the machine.
    /// The node case climbs a ladder of budgets: the pipe wait spends the
    /// whole budget, so one number cannot be both cheap and enough. See
    /// `BUDGETS`.
    mod slow {
        /// A probe budget no contention can exhaust
        ///
        /// Not a claim about how long a probe should take: far enough above
        /// this suite's contention that timing stops being a variable.
        const PROBE_BUDGET: Duration = Duration::from_secs(30);

        use super::*;

        /// A shepherd that restarts whatever it is asked to and lists an
        /// empty flock afterwards
        ///
        /// Enough for `restart` to reach the end, so a test asserting an
        /// empty stderr is asserting about the dog check.
        #[cfg(unix)]
        fn answering_a_restart(request: &Request) -> Response {
            match request {
                Request::Restart { .. } => Response::Restarted(Vec::new()),
                _ => Response::Flock(Vec::new()),
            }
        }

        /// The answer a dog gives when its binary was built against a
        /// protocol this shep does not speak
        #[cfg(unix)]
        fn stale_answer() -> String {
            format!(
                "echo 'shep-log-rotate 0.1.3'\necho 'shep-protocol: {}'",
                shep_client::PROTOCOL_VERSION + 1
            )
        }

        /// The ordering is read off stderr rather than off a clock: the
        /// shepherd refuses the restart, so its refusal lands on the same
        /// stream as the warning and the two offsets say which ran first.
        // Unix only because of the fixture: `adopted_dog` writes a
        // `#!/bin/sh` script, so on Windows the probe answers unknown and the
        // three `restarts_in_silence` tests would pass vacuously.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_dog_whose_disk_binary_cannot_connect_is_warned_about_before_the_restart() {
            let dir = tempfile::tempdir().unwrap();
            let paths = adopted_dog(dir.path(), "log-rotate", &stale_answer());
            let sock = shep_client::testing::control_address(dir.path());
            let (client, _daemon) =
                fake_client_replying_err(&sock, RpcErrorCode::NotFound, "no such sheep").await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                let args = SelectorArgs {
                    selectors: vec!["log-rotate".into()],
                };
                let _ = restart_within(&client, &mut streams, &paths, &args, PROBE_BUDGET).await;
            }

            let text = String::from_utf8(err).unwrap();
            let warning = text
                .find("dog_binary_skew")
                .unwrap_or_else(|| panic!("the restart must warn first: {text}"));
            let refusal = text
                .find("no such sheep")
                .unwrap_or_else(|| panic!("the restart must still be attempted: {text}"));
            assert!(
                warning < refusal,
                "the warning must reach the operator before the restart does: {text}"
            );
        }

        /// The operator knows which of the two fixes they meant, so the
        /// message names both and tells them what the restart just did.
        #[cfg(unix)]
        #[tokio::test]
        async fn the_restart_warning_names_both_numbers_and_both_ways_out() {
            let dir = tempfile::tempdir().unwrap();
            let paths = adopted_dog(dir.path(), "log-rotate", &stale_answer());
            let sock = shep_client::testing::control_address(dir.path());
            let (client, _envelopes) = fake_client_answering(&sock, answering_a_restart).await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                let args = SelectorArgs {
                    selectors: vec!["log-rotate".into()],
                };
                let _ = restart_within(&client, &mut streams, &paths, &args, PROBE_BUDGET).await;
            }

            let text = String::from_utf8(err).unwrap();
            let disk = (shep_client::PROTOCOL_VERSION + 1).to_string();
            let shep = shep_client::PROTOCOL_VERSION.to_string();
            assert!(
                text.contains(&disk) && text.contains(&shep),
                "the warning names both numbers: {text}"
            );
            assert!(
                text.contains("Run a shep that speaks") && text.contains("reinstall the dog"),
                "the warning names both ways out, and picks neither: {text}"
            );
            assert!(
                text.contains("log-rotate"),
                "the warning names the dog it is about: {text}"
            );
        }

        /// Every restart of every working flock is in this case, so a line
        /// here is a line an operator learns to skip.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_dog_whose_disk_binary_is_current_restarts_in_silence() {
            let dir = tempfile::tempdir().unwrap();
            let answer = format!(
                "echo 'shep-log-rotate 0.1.3'\necho 'shep-protocol: {}'",
                shep_client::PROTOCOL_VERSION
            );
            let paths = adopted_dog(dir.path(), "log-rotate", &answer);
            let sock = shep_client::testing::control_address(dir.path());
            let (client, mut envelopes) = fake_client_answering(&sock, answering_a_restart).await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                let args = SelectorArgs {
                    selectors: vec!["log-rotate".into()],
                };
                let _ = restart_within(&client, &mut streams, &paths, &args, PROBE_BUDGET).await;
            }

            assert_eq!(
                String::from_utf8(err).unwrap(),
                "",
                "a healthy dog restarts with nothing said about it"
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
                    .await
                    .expect("restart must reach the wire; it hung instead of sending a request")
                    .unwrap()
                    .body,
                Request::Restart {
                    selector: SelectorSpec::Name("log-rotate".into()),
                },
                "and it is still restarted"
            );
        }

        /// `docs/dogs.md` promises that not answering is never held against
        /// a dog. Unknown is not stale: it is the state `adopt` records.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_dog_that_answers_nothing_restarts_in_silence() {
            let dir = tempfile::tempdir().unwrap();
            let paths = adopted_dog(
                dir.path(),
                "log-rotate",
                "echo 'shep-log-rotate does not understand --version' >&2\nexit 2",
            );
            let sock = shep_client::testing::control_address(dir.path());
            let (client, mut envelopes) = fake_client_answering(&sock, answering_a_restart).await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                let args = SelectorArgs {
                    selectors: vec!["log-rotate".into()],
                };
                let _ = restart_within(&client, &mut streams, &paths, &args, PROBE_BUDGET).await;
            }

            assert_eq!(
                String::from_utf8(err).unwrap(),
                "",
                "a dog that does not answer is not a dog that is stale"
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
                    .await
                    .expect("restart must reach the wire; it hung instead of sending a request")
                    .unwrap()
                    .body,
                Request::Restart {
                    selector: SelectorSpec::Name("log-rotate".into()),
                },
                "and it is still restarted"
            );
        }

        /// The second shape of unknown, reaching a different line from the
        /// dog that answers nothing at all: there is an answer here, it just
        /// does not say.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_dog_that_names_no_protocol_restarts_in_silence() {
            let dir = tempfile::tempdir().unwrap();
            let paths = adopted_dog(dir.path(), "log-rotate", "echo 'shep-log-rotate 0.1.3'");
            let sock = shep_client::testing::control_address(dir.path());
            let (client, mut envelopes) = fake_client_answering(&sock, answering_a_restart).await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                let args = SelectorArgs {
                    selectors: vec!["log-rotate".into()],
                };
                let _ = restart_within(&client, &mut streams, &paths, &args, PROBE_BUDGET).await;
            }

            assert_eq!(
                String::from_utf8(err).unwrap(),
                "",
                "an unstated protocol is unknown, and unknown is not stale"
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
                    .await
                    .expect("restart must reach the wire; it hung instead of sending a request")
                    .unwrap()
                    .body,
                Request::Restart {
                    selector: SelectorSpec::Name("log-rotate".into()),
                },
                "and it is still restarted"
            );
        }

        /// A built-in dog's binary is the shepherd's own, so there is no
        /// second thing to drift. The config here has a stale adopted dog in
        /// it too, so the silence is about this name rather than about an
        /// empty `adopted_dogs`.
        #[cfg(unix)]
        #[tokio::test]
        async fn a_built_in_dog_is_never_asked_what_its_binary_speaks() {
            let dir = tempfile::tempdir().unwrap();
            let paths = adopted_dog(dir.path(), "log-rotate", &stale_answer());
            crate::commands::shep_toml::ShepToml::edit(&paths.daemon_config, |cfg| {
                cfg.enable_dog("metrics");
            })
            .unwrap();
            let sock = shep_client::testing::control_address(dir.path());
            let (client, mut envelopes) = fake_client_answering(&sock, answering_a_restart).await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                let args = SelectorArgs {
                    selectors: vec!["metrics".into()],
                };
                let _ = restart_within(&client, &mut streams, &paths, &args, PROBE_BUDGET).await;
            }

            assert_eq!(
                String::from_utf8(err).unwrap(),
                "",
                "a built-in dog has no binary of its own to be stale"
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
                    .await
                    .expect("restart must reach the wire; it hung instead of sending a request")
                    .unwrap()
                    .body,
                Request::Restart {
                    selector: SelectorSpec::Name("metrics".into()),
                },
                "and it is still restarted"
            );
        }

        /// A module that exports its config and leaves a detached node
        /// holding the pipes shep reads. The holder outlives `budget`
        /// threefold, or the reads would finish and the case pass for the
        /// wrong reason.
        fn held_pipe_module(budget: Duration) -> String {
            format!(
                "require('child_process')\
                 .spawn(process.execPath, ['-e', 'setTimeout(()=>{{}},{})'], \
                 {{ detached: true, stdio: 'inherit' }})\
                 .unref(); \
                 module.exports = {{ app: [] }};",
                (budget * 3).as_millis()
            )
        }

        /// node itself exits here: `detached` plus `unref` takes the child
        /// off node's event loop, and `stdio: inherit` hands it the pipes
        /// shep is reading, so only the reads run out of budget.
        #[test]
        fn a_js_flockfile_leaving_a_process_on_the_pipe_says_that_instead() {
            if !node_available() {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("flock.js");

            // A ladder, not one number: `run_bounded` waits for node and then
            // spends the rest on the held pipe, so the budget is this case's
            // runtime. A `Killed` verdict is read as a slow machine and retried
            // with more room; the last rung falls through.
            const BUDGETS: [Duration; 3] = [
                Duration::from_secs(5),
                Duration::from_secs(20),
                Duration::from_secs(80),
            ];

            for (attempt, budget) in BUDGETS.iter().copied().enumerate() {
                std::fs::write(&path, held_pipe_module(budget)).unwrap();
                let err = evaluate_js_flockfile(&path, budget).unwrap_err();
                let message = err.to_string();

                // node never got as far as exiting, so there was nothing
                // holding a pipe yet to say anything about. Give it more
                // room. The last rung falls through instead, so a machine
                // that cannot do it in 80s fails with the message it earned.
                if message.contains("still running") && attempt + 1 < BUDGETS.len() {
                    continue;
                }

                assert_eq!(target_exit_code(&err), ExitCode::InvalidConfig);
                assert!(
                    message.contains("left behind still holds the output"),
                    "got: {message}"
                );
                assert!(
                    !message.contains("killed"),
                    "node exited on its own, so nothing was killed: {message}"
                );
                return;
            }
        }
    }
}
