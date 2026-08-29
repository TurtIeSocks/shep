//! Lifecycle verbs: `start`, `stop`, `restart`, `delete` — the first verbs a
//! user types, and the first to exercise the whole client stack end to end.
//!
//! Every verb here receives an already-connected [`Client`]; `main` decides
//! how it got one (`connect_or_spawn` for `start`, `Client::connect` for
//! everything else) before dispatching — see that module's own doc. `start`
//! alone resolves a target into [`AppConfig`]s before anything reaches the
//! wire; [`resolve_target`] is that resolution, kept pure and separate from
//! the RPC so it stays fast and hermetic to test.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shep_client::{Client, START_DEADLINE};
use shep_core::config::{AppConfig, FlockFormat, Flockfile, FlockfileError};
use shep_core::protocol::{ProcessInfo, Request, Response, SelectorSpec};
use shep_core::selector::ProcessSelector;

use crate::cli::Format;
use crate::cli::{SelectorArgs, StartArgs, StockArgs};
use crate::commands::bounded::{Bounded, run_bounded};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{DeletedIds, FlockRows, Render, Streams, emit, emit_flock, write_outcome};

/// What [`resolve_target`] can fail with. Module-scoped per IR-18, and named
/// for the function rather than the verb on purpose: `start`'s own
/// daemon-side failures are `shep_client::RequestError` and
/// `shep_client::spawn::SpawnError`, which `exit.rs` already converts. There
/// is no `impl From<&TargetError> for ExitCode` — the mapping is
/// [`target_exit_code`], a plain `match` inside this module, so `exit.rs`
/// stays owned entirely by the CLI skeleton task.
#[derive(Debug)]
pub enum TargetError {
    /// `target` was `-`, but `stdin` was not valid UTF-8 — or, in `start`
    /// itself, the real read of the process's stdin failed outright.
    Stdin(std::io::Error),
    /// `target`'s extension named a recognised Flockfile format, but the
    /// file itself could not be read.
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
        /// The raw target string, verbatim, so the reported message names
        /// exactly what was tried.
        target: String,
    },
    /// `--flockfile` was given for a path whose extension names no format
    /// this can read.
    UnknownFlockfileFormat {
        /// The path as the operator wrote it.
        path: PathBuf,
    },
    /// A `.js` Flockfile could not be evaluated. `node_missing` separates
    /// "install node" from "your config threw", because they are different
    /// problems with different fixes and different exit codes.
    ///
    /// Carries no separate `path` field: every `detail` string below is
    /// already built with the path baked in (decision 3's own sentences),
    /// so a second copy would be dead weight — literally, `cargo clippy -D
    /// warnings` flags an unread `path` field here, because
    /// `#[derive(Debug)]` does not count as a read for dead-code analysis.
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

/// `start`'s mapping from a resolution failure to the exit code that
/// reports it.
///
/// `pub(crate)`, not private: `commands::runtime` shares this mapping for
/// its own call to [`resolve_target`] rather than inventing a second one —
/// the same failure ought to mean the same exit code whichever verb hit it.
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
/// kills it.
///
/// 30s, against the ~60ms `node -e` spends requiring a small module on this
/// machine and the couple of seconds a large dependency tree costs on a cold
/// filesystem. It is not a performance dial: nothing waits on this except a
/// config that has already gone wrong, so it is set far enough out that no
/// honest module can reach it and near enough that an unattended `shep
/// start` still ends.
const JS_EVAL_BUDGET: Duration = Duration::from_secs(30);

/// The script handed to `node -e`. Wraps the `require` in its own
/// `try`/`catch` rather than letting an uncaught exception crash node and
/// relying on node's own crash-dump formatting — see this function's doc for
/// why.
/// The filename [`evaluate_js_flockfile`] writes [`JS_BRIDGE_SCRIPT`] to.
///
/// Deliberately plain ASCII with no spaces: it is passed to node as a bare
/// relative argument, and the whole point of the file is that nothing
/// needing quoting ever reaches a command line.
const JS_BRIDGE_FILE: &str = "shep-flockfile-bridge.js";

/// The bridge run by [`evaluate_js_flockfile`], written to a file rather
/// than passed to `node -e`.
///
/// **It was `node -e <script>` and that did not survive Windows CI.** The
/// symptom was node failing with `EISDIR: illegal operation on a directory,
/// lstat 'C:'`, unchanged across two different ways of handing it the path,
/// which is what ruled the path out as the cause: an identical message after
/// changing the mechanism means the mechanism was not what broke.
///
/// The remaining suspect is this script itself crossing a command line. It
/// contains `&&`, which is a `cmd.exe` operator, so a `node` that resolves
/// to a `.cmd` shim rather than a real `node.exe` would have cmd re-parse
/// and truncate it. That was never confirmed, because a machine with a real
/// `node.exe` does not reproduce it.
///
/// Writing the script to a file removes the question rather than answering
/// it. A file's contents cross no parser: not the MSVC C runtime's argument
/// escaping, not `cmd.exe`'s, not any shim's. The only argument left is
/// [`JS_BRIDGE_FILE`], a bare relative name with nothing in it to quote.
///
/// The path is still read from the environment and still never interpolated
/// into the source, so the injection argument holds: a Flockfile path
/// containing `'`, `\` or a newline cannot escape a string literal here,
/// because there is no string literal for it to escape.
const JS_BRIDGE_SCRIPT: &str = "try { \
     process.stdout.write(JSON.stringify(require(process.env.SHEP_FLOCKFILE_PATH))); \
 } catch (err) { \
     process.stderr.write('[bridge saw ' + String(process.env.SHEP_FLOCKFILE_PATH) + '] ' + (err && err.message ? String(err.message) : String(err))); \
     process.exitCode = 1; \
 }";

/// Evaluates a `.js` Flockfile through node and returns its JSON.
///
/// The script is written to a file and run as `node <file>` from that
/// file's own directory; see [`JS_BRIDGE_SCRIPT`] for why it is not
/// `node -e`.
///
/// The path is passed in the **environment**, as `SHEP_FLOCKFILE_PATH`, and
/// never interpolated into the JavaScript source: a path containing `'`,
/// `\` or a newline would otherwise escape the string literal, and adding a
/// second way to inject code into a file whose own code we are already
/// about to run is gratuitous.
///
/// It used to be an argument, read back as `process.argv[1]`. That is
/// correct on unix and against a plain `node.exe`, and it broke on the
/// Windows CI runner: node reached `require` with `C:` and failed with
/// `EISDIR: illegal operation on a directory, lstat 'C:'`, so the path was
/// re-parsed somewhere between this `Command` and node's own argv. A `.cmd`
/// shim earlier on `PATH` than the real binary is the likeliest culprit,
/// since `cmd.exe` does not use the argument-escaping convention
/// `std::process::Command` writes for. The fix does not depend on settling
/// which layer did it: an environment variable is not a command line, so
/// nothing in between can re-split it.
///
/// The path must be absolute — `require("x.js")` with no leading `./` is a
/// *package* specifier and resolves against `node_modules`, not the cwd.
///
/// stdin is `/dev/null` so a config module that reads stdin cannot eat the
/// operator's terminal; stdout and stderr are captured so node's own message
/// can be quoted back.
///
/// **Runs `-e` with an in-script `try`/`catch`, not `-p` bare.** Letting
/// `require` throw uncaught and scraping node's own crash-dump formatting
/// was tried first and does not work on a current node: verified empirically
/// against v26.5.0, the crash dump always ends with a trailing `Node.js
/// vX.Y.Z` banner line, so "the last non-blank line of stderr" — the
/// extraction this module used to use — quotes the banner, or a stack frame,
/// never the message. `err.message` is written to stderr ourselves instead,
/// which is what makes the sentence below actually name the failure. This
/// stays inside the same mechanic the design calls for (`-p` / `-e` both put
/// the path in the environment, never in the source), so it is a narrower
/// implementation choice, not a different design.
///
/// **`budget` bounds the whole evaluation**, and node is killed the moment it
/// runs out. What has to happen inside it is node EXITING, not `require`
/// returning: a module that leaves a server listening or a timer armed can
/// assign `module.exports` and return while node's event loop stays alive, so
/// this used to hang until somebody pressed Ctrl-C. That is a fair answer at
/// a terminal and no answer at all for the CI job or provisioning script
/// running `shep start` with nobody watching. [`run_bounded`] is the
/// mechanism; [`JS_EVAL_BUDGET`] is what every caller passes.
///
/// The `node_missing` sentence is pinned by `cli_e2e`'s
/// `a_js_flockfile_without_node_says_so_and_says_what_to_do`, not a unit
/// test: producing it needs a `PATH` without node, and a unit test would
/// have to mutate its own process via `std::env::set_var`, which is
/// `unsafe` in edition 2024 inside a crate that forbids unsafe. The e2e
/// tier runs shep as a subprocess, and `Command::env` sets the child's
/// environment alone.
///
/// `docs/migration.md` still quotes this sentence for an operator without
/// node installed, and that quote is still kept in step by hand, so update it
/// in the same commit if this `format!` changes.
///
/// # Errors
///
/// - [`TargetError::Read`] — the path could not be canonicalized.
/// - [`TargetError::Js`] with `node_missing` — node is not on `PATH`.
/// - [`TargetError::Js`] — node ran and failed, could not be spawned, was
///   still running when `budget` ran out, or exited leaving a process of its
///   own holding the output shep was reading.
fn evaluate_js_flockfile(path: &Path, budget: Duration) -> Result<String, TargetError> {
    let absolute = std::fs::canonicalize(path).map_err(|source| TargetError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    // The bridge is written to a file and run as `node loader.js` from that
    // file's own directory, so the only argument node ever receives is a
    // bare relative filename: no path, no quotes, no shell metacharacters.
    // See [`JS_BRIDGE_SCRIPT`] for what this is defending against.
    //
    // `scratch` stays bound until this function returns, because dropping a
    // `TempDir` deletes it: node has to still be able to read the loader for
    // as long as `run_bounded` is waiting on it.
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

/// Resolves `target` into the [`AppConfig`]s `start` should register, in
/// this fixed order — do not widen it (spec fidelity; input-format widening
/// is a top drift risk):
///
/// 1. `target == "-"` — `stdin` is Flockfile JSON. `as_flockfile` is ignored
///    here; stdin is already a Flockfile by construction.
/// 2. `as_flockfile` is set — read by extension: a format
///    [`FlockFormat::from_path`] recognises parses as it does today; `.js`
///    goes through the node bridge ([`evaluate_js_flockfile`]); anything
///    else is [`TargetError::UnknownFlockfileFormat`], naming the
///    extensions accepted.
/// 3. `target`'s extension is one [`FlockFormat::from_path`] recognises —
///    read and [`Flockfile::parse`] it in that format.
/// 4. Any other existing path — one [`AppConfig::minimal`], named `name` if
///    given, else the path's file stem, with `target` itself as the script.
/// 5. Nothing matched — [`TargetError::Unresolvable`], naming `target`.
///
/// With `as_flockfile` false, every branch behaves exactly as it did before
/// this parameter existed — that property is what Task 1's mutations check.
///
/// `stdin` is bytes the caller already read, never read here — `start`
/// reads the real process stdin only when `target == "-"` and hands the
/// result in, which is what keeps this function pure and hermetically
/// testable.
///
/// # Errors
///
/// - [`TargetError::Stdin`] — `target` is `-` and `stdin` is not valid UTF-8.
/// - [`TargetError::Read`] — `target`'s extension names a recognised format,
///   but the file could not be read.
/// - [`TargetError::Flockfile`] — the source read fine but failed Flockfile
///   validation.
/// - [`TargetError::UnknownFlockfileFormat`] — `as_flockfile` is set and
///   `target`'s extension names no readable format.
/// - [`TargetError::Js`] — `as_flockfile` is set, `target` is a `.js` file,
///   and node could not be run, could not evaluate it, or was still at it
///   after [`JS_EVAL_BUDGET`].
/// - [`TargetError::Unresolvable`] — `target` matched none of the above.
pub fn resolve_target(
    target: &str,
    name: Option<&str>,
    stdin: &[u8],
    as_flockfile: bool,
) -> Result<Vec<AppConfig>, TargetError> {
    let path = Path::new(target);
    match (target, FlockFormat::from_path(path)) {
        ("-", _) => {
            let source = String::from_utf8(stdin.to_vec()).map_err(|_utf8_error| {
                TargetError::Stdin(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stdin is not UTF-8",
                ))
            })?;
            Ok(Flockfile::parse(&source, FlockFormat::Json)?.apps)
        }
        (_, format) if as_flockfile => match format {
            Some(format) => {
                let source = std::fs::read_to_string(path).map_err(|source| TargetError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                let flockfile = Flockfile::parse(&source, format)?;
                Ok(default_cwd_to_flockfile_dir(flockfile.apps, path))
            }
            None if path.extension().and_then(|e| e.to_str()) == Some("js") => {
                let json = evaluate_js_flockfile(path, JS_EVAL_BUDGET)?;
                let flockfile = Flockfile::parse(&json, FlockFormat::Json)?;
                Ok(default_cwd_to_flockfile_dir(flockfile.apps, path))
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
            let flockfile = Flockfile::parse(&source, format)?;
            Ok(default_cwd_to_flockfile_dir(flockfile.apps, path))
        }
        // Absolutised against the CLI's cwd, which is the cwd the `exists`
        // check just above was answered from. Without this the check and the
        // use disagree: the CLI confirms `./bin/thing` is there, the daemon
        // resolves the same string against ITS cwd -- wherever it happened
        // to be spawned -- and reports `No such file or directory` for a
        // path the operator can see with their own eyes. The check passing
        // is what makes that error baffling.
        _ if path.exists() => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(target);
            // Only a RELATIVE path is rewritten. An absolute one is left
            // exactly as typed: `canonicalize` also resolves symlinks, so on
            // macOS it turns the `/var/...` an operator wrote into
            // `/private/var/...`, and a listing that echoes back a path
            // nobody typed is the same species of surprise this is fixing.
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
            // Where the operator ran `shep start`, not where the shepherd
            // happens to live. An unset `cwd` leaves the child inheriting the
            // daemon's directory, which is invisible from the command line
            // and is whatever the shepherd was spawned from -- so a service
            // that reads a config file beside itself breaks in a quieter way
            // than a missing binary does. `normalize` already refuses `watch`
            // for this exact reason (`WatchWithoutCwd`: defaulting to the
            // daemon's cwd "risks watching the whole filesystem under a
            // systemd unit"); this generalises that caution to every app
            // started by path.
            app.cwd = std::env::current_dir()
                .ok()
                .map(|dir| dir.to_string_lossy().into_owned());
            Ok(vec![app])
        }
        _ => Err(TargetError::Unresolvable {
            target: target.to_string(),
        }),
    }
}

/// Sends `body` with `deadline` (`None` defers to the client's own default),
/// renders whatever the daemon answers through [`render_outcome`], and maps
/// every way that can go wrong to its exit code.
///
/// `stock` is the only caller. It renders the flock afterwards for the same
/// reason every other verb in this file does -- see [`render_outcome`] -- and
/// its `extract` still names the narrow payload, because that is what
/// `--format json` gets.
///
/// `extract` pulls the verb's own payload out of `Response`; `Response` is
/// `#[non_exhaustive]` (Global Constraints), so an answer `extract` does not
/// recognise — a variant this client predates, or simply the wrong one for
/// this verb — maps to [`ExitCode::Internal`] rather than panicking.
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

/// Parses every selector the invocation named, refusing on the first bad one.
///
/// All-or-nothing on purpose: a typo in the third target should not be
/// discovered after the first two have already been acted on.
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

/// Sends one request per selector and collects what each returned.
///
/// The CLI loops rather than the wire carrying a list, which is a deliberate
/// trade recorded here because it is invisible from the command line: the
/// selectors are not applied atomically. `shep stop a b c` where `b` matches
/// nothing still stops `a` and `c`, and says what happened to `b`. Swapping
/// this for a batched request later changes no command and breaks no script,
/// because the CLI surface does not encode which it is.
///
/// Every selector is attempted -- stopping at the first failure would leave
/// the operator guessing which of their targets were touched -- and the
/// returned code is the FIRST failure, so a partial success is never reported
/// as a whole one. Errors are rendered as they happen, so the operator sees
/// which target failed rather than a count.
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

/// Which sheep in `flock` a `start` target names, under the precedence
/// [`start_one`] documents.
///
/// # The two tiers this function is
///
/// A `Name` token is tried as an exact sheep name first and as a FOLD name
/// second, which is what makes `shep start backed` reach a fold called
/// `backed` without the `fold:` prefix. Every other selector form means one
/// thing already and is matched as itself.
///
/// # Dogs
///
/// A wildcard passes a dog by; an exact name or id reaches it. The same rule
/// [`ProcessSelector::is_exact`] states for the daemon's own matching, and for
/// the same reason: a dog is a process an operator installed, not a member of
/// the flock `all` means. The fold fallback counts as a wildcard even though
/// it came from a `Name` token, because the operator named a group rather than
/// a process.
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
        ProcessSelector::Id(wanted) => flock
            .iter()
            .filter(|info| info.id == *wanted)
            .cloned()
            .collect(),
        other => sheep_only(flock, &|info| {
            other.matches(&info.name, info.id, info.fold.as_deref())
        }),
    }
}

/// Whether `target` carries a marker that makes it unmistakably a selector,
/// and so what to say when it matched nothing.
///
/// Only the message differs. `start` still falls through to the Flockfile and
/// path tiers for every token, because a marker being present does not make a
/// file of that name stop existing -- `/srv/app/` parses as a `/regex/` and is
/// also a real directory somebody might type. What this decides is whether
/// `shep start fold:typo` reports "no sheep is in a fold called typo" or the
/// baffling "fold:typo is not a sheep, a fold, `-`, a recognised Flockfile, or
/// an existing path" that sent an operator looking for a file by that name.
fn selector_miss(
    target: &str,
    selector: &ProcessSelector,
    flock: &[ProcessInfo],
) -> Option<String> {
    match selector {
        // Phrased off the SHEEP count, never off `flock.is_empty()`.
        // `flock_matches` passes a dog by for every wildcard, so `all`
        // matching nothing means there are no sheep -- which is not the same
        // as an empty flock, and saying "the flock is empty" while
        // `shep flock` prints dog rows on the same machine is a plain
        // contradiction an operator would have to reconcile themselves.
        ProcessSelector::All if flock.iter().any(|info| info.dog.is_some()) => Some(
            "no sheep in the flock; there is nothing to start. The dogs listed \
             by `shep dogs` are not sheep and `all` never reaches them"
                .to_string(),
        ),
        ProcessSelector::All => Some("the flock is empty; there is nothing to start".to_string()),
        ProcessSelector::Fold(fold) => Some(format!("no sheep is in a fold called {fold}")),
        ProcessSelector::Regex(_) => Some(format!("no sheep matched {target}")),
        // A bare name or id carries no marker, so it may equally have been
        // meant as a filename. The unresolvable message names every tier.
        ProcessSelector::Name(_) | ProcessSelector::Id(_) => None,
    }
}

/// Whether `target` could name a sheep or a fold at all.
///
/// A sheep name may not contain a path separator and may not be `.` or `..`
/// (`shep_core::config::normalize`), so a token carrying one cannot be a
/// sheep however the flock is configured. Making that a rule rather than a
/// coincidence is what gives an operator whose fold shares a name with a file
/// a way to say which they meant: `./backed` is the file, always.
///
/// Applied to the `Name` form only. `/regex/`, `fold:`, `all` and a glob are
/// markers rather than names, and `/web/` legitimately contains slashes.
fn is_reachable_as_a_name(selector: &ProcessSelector) -> bool {
    match selector {
        ProcessSelector::Name(name) => !name.contains(['/', '\\']) && name != "." && name != "..",
        _ => true,
    }
}

/// Renders what a lifecycle verb should leave on the operator's screen: the
/// rows it touched under `--format json`, and the WHOLE flock as a table
/// otherwise.
///
/// # Why the whole flock, and why only in a table
///
/// The question an operator has after starting one app is almost never "did
/// that one app start" -- the exit code already answered that -- it is
/// "what does the flock look like now", which is the question `shep flock`
/// exists for and which a one-row table cannot answer. Every lifecycle verb
/// here prints exactly what `shep flock` would print, so the answer is the
/// same shape whichever verb asked it.
///
/// `--format json` keeps the narrow payload, deliberately, and this is the
/// one place the two surfaces diverge on purpose. A script that runs
/// `shep stop web --format json` asked about `web` and wants the rows for
/// `web`; widening its `data` to the whole flock would break every consumer
/// reading `data[0]` to learn what it just stopped, for no gain -- a script
/// that wants the flock has `shep flock --format json`. So the rule is: the
/// human surface answers "what now", the machine surface answers "what did I
/// just touch".
///
/// # The cost
///
/// One extra `ListFlock` round trip per lifecycle verb in table form. The
/// reply a lifecycle verb gets back carries only the rows it touched, and
/// widening those responses would be a wire change -- a bigger decision than
/// this behaviour needs, and one that would still leave `delete` unable to
/// describe a flock it had just removed rows from. A fresh listing needs no
/// protocol change and is the same request `shep flock` already makes.
///
/// # Dogs
///
/// In table form only: routed through [`emit_flock`], not [`emit`], so a dog
/// renders through the dogs table with its `SOURCE` column rather than
/// through the sheep table with an ID, a face, and a `FOLD` and `SMIT` it can
/// never fill, and `shep restart log-rotate` draws the same dog the same way
/// regardless of which verb asked. `--format json` gives a dog's row no such
/// treatment -- it goes through [`emit`] like any other row in `narrow`.
///
/// # Errors
///
/// Never returns a listing failure as this verb's failure. The verb already
/// did its work and already reported its own outcome; failing the command
/// because the receipt could not be fetched would turn a successful restart
/// into a non-zero exit. An unreachable or unrecognised listing prints
/// nothing extra and reports success, the same call [`flock_now`] makes for
/// its own reasons.
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

/// Starts one or more sheep, resolved from `args.target` — see
/// [`resolve_target`].
///
/// Sends `Request::Start` with `START_DEADLINE` rather than the client's
/// default: a cold spawn plus a readiness probe routinely outruns 5
/// seconds, and a client-side abandonment there would report failure for a
/// sheep that came up fine.
/// `shep start <name>` against a sheep the flock already has.
///
/// A sheep that is already up is reported and left alone. Restarting a live
/// service because someone typed `start` would be a genuinely bad surprise --
/// `restart` is right there and says what it does -- so this refuses to be
/// clever about it.
///
/// Anything not up is started. The wire has no start-by-name: `Request::Start`
/// carries configs to register, and the sheep is already registered. `Restart`
/// is the request that takes a selector and brings a stopped sheep up, which
/// is what `shep restart <name>` has always done to one.
///
/// A respawn that fails to spawn still answers `Response::Restarted` with an
/// `Ok` -- the daemon's aggregation reply for `Restart` has no per-id error
/// slot (`shep-daemon/src/supervisor.rs`'s `respawn`, `Err` arm), so a
/// failed spawn comes back as an ordinary `errored` row rather than an RPC
/// error. `shep start <path>`'s own `Request::Start` does not share that
/// gap (`do_start` returns `Err(SpawnFailed)` straight from the same
/// failure), which is what let `shep start <name>` against a sheep that
/// cannot spawn exit 0 and print nothing, while `shep start <path>` against
/// the identical broken script reported `error[spawn_failed]` -- reproduced
/// live and fixed here, since this is the one place that turns a `Restart`
/// answer into `start`'s own exit code.
fn is_live(info: &ProcessInfo) -> bool {
    use shep_core::status::ProcStatus;
    matches!(
        info.status,
        ProcStatus::Online | ProcStatus::Starting | ProcStatus::Stopping
    )
}

/// Brings every sheep in `matched` up, and reports the ones that were already
/// up rather than replacing them.
///
/// `selector` is the operator's own token when the match came from the
/// selector tier, and `None` when it came from a path or Flockfile that
/// resolved to apps the flock already has. The distinction is the difference
/// between a suggestion that works and one that does not: `shep restart
/// fold:backed` is a command, and `shep restart ./rotom.sh` is not, because
/// `restart` takes selectors and a path is not one. So the token is quoted
/// back whenever there is one, and a path or Flockfile falls back to listing
/// the names.
///
/// The already-up set gets ONE notice however large it is. `shep start all`
/// against a healthy thirteen-app flock would otherwise print thirteen
/// notices saying the same thing.
///
/// # Respawns are issued per ROW, by id
///
/// They used to be issued per NAME, deduped, because `Request::Restart` with
/// a name selector reaches every instance of a clustered app in one request
/// and a fold holding a four-instance app would otherwise send four. The
/// saving was real and the widening it bought was not worth it: a name
/// selector reaches every instance whether or not this function matched them,
/// so collapsing to names hands the daemon a WIDER set than the one the
/// operator's own selector picked out. Two ways that bit:
///
/// - `shep start 0` against ten instances named `zam` restarted all ten.
///   `Id` is the only selector form that can name a subset of one name's
///   rows, so it was the only form the collapse could widen -- which is
///   exactly the form an operator reaches for to name ONE of a clustered
///   app's instances.
/// - Worse, and for every selector form: the `live`/`asleep` partition above
///   promises not to replace a sheep that is already up, and a name selector
///   walked straight back over it. One online instance among nine stopped
///   ones was restarted by the request meant for the other nine.
///
/// So the id of each row is what goes on the wire, and no dedup is needed --
/// a row is one instance and appears once. The cost is the round trip the
/// old shape was saving, one per instance rather than one per name.
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
            // The operator's own token, not the sheep's name: `shep start 0`
            // asked about one instance, and answering it with `shep restart
            // zam` would suggest a command that replaces all ten.
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

    // Every row is attempted and the FIRST failure is what the verb returns,
    // the discipline `request_each` states for the selector-taking verbs:
    // stopping at the first failure leaves the operator guessing which of the
    // rest were touched. This returned early, so one app in a fold failing to
    // spawn abandoned the fold's remaining apps in silence.
    let mut failure: Option<ExitCode> = None;
    for sheep in asleep {
        let code = resume(client, streams, sheep, started).await;
        if code != ExitCode::Success {
            failure = failure.or(Some(code));
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Every distinct name in `infos`, in the order they first appear.
///
/// Feeds the already-running notice and nothing else -- never the respawn
/// targets, which must stay per-row and per-id; see [`resume_all`]'s own
/// doc for why. A notice reads as a list of apps, so collapsing to names is
/// right for this and only this.
///
/// Order-INDEPENDENT: it compares against every name kept so far, not
/// against the previous one, so a caller handing over rows in a Flockfile's
/// own order gets the same answer as one handing over the daemon's sorted
/// listing.
///
/// A `Vec` scan rather than a `HashSet`: this runs over the sheep one
/// selector matched, which is tens of rows, and it has to preserve first-seen
/// order for the notice it feeds.
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
    // See this function's own doc: an `Ok` reply can still carry a sheep
    // that came back `errored`, and that is a `start` failure by any
    // definition an operator would recognise. Reported and returned WITHOUT
    // extending `started` -- `shep start <path>`'s own failure leaves
    // `started` empty too (its `Request::Start` never reaches the `Ok` arm
    // `request_each` collects from), and `start`'s caller only prints a
    // table when `started` is non-empty. Populating it here would print a
    // table on a path that is supposed to fail exactly like the by-path one
    // does: an error on stderr and nothing on stdout.
    if any_restart_failed(&procs) {
        // Named by id as well as by name, because this reports one ROW:
        // without the id, four instances of one app failing would print
        // four identical messages, naming a sheep the operator cannot tell
        // apart. The id is also what makes the `bleats` suggestion
        // reach the instance that actually failed.
        let (name, id) = (&sheep.name, sheep.id);
        let message = format!(
            "{name} (id {id}) could not be started; see `shep bleats {id}` or its log files for why"
        );
        return streams.fail(ExitCode::SpawnFailed, &message);
    }
    started.extend(procs);
    failure.unwrap_or(ExitCode::Success)
}

/// Whether any row in a `Request::Restart` reply came back `errored` -- see
/// [`resume`]'s own doc for why that can happen inside an `Ok` reply.
fn any_restart_failed(procs: &[shep_core::protocol::ProcessInfo]) -> bool {
    procs
        .iter()
        .any(|info| info.status == shep_core::status::ProcStatus::Errored)
}

/// Gives every app that set no `cwd` the Flockfile's own directory.
///
/// A Flockfile is a file you commit. Before this, an app with a relative
/// `script` and no `cwd` resolved that script against the DAEMON's working
/// directory -- whatever directory the shepherd happened to be autostarted
/// from -- so the same committed file worked on the machine where that was
/// right and failed on the next one, with an error naming neither cause.
/// Measured 2026-08-19 with three distinct directories; `deferred.md` carries
/// the evidence. The maintainer's call was to default the cwd rather than resolve the
/// script alone, because the rule then fits in one sentence an operator can
/// read.
///
/// Absolute, via `canonicalize`, because the daemon resolves a relative cwd
/// against itself and would land back where this started.
///
/// Silent no-op if the path cannot be canonicalised: the caller is about to
/// fail on reading the file anyway, and inventing a directory here would
/// bury that error under a stranger one. An app that sets its own `cwd`
/// keeps it -- this only fills a blank.
fn default_cwd_to_flockfile_dir(apps: Vec<AppConfig>, flockfile: &Path) -> Vec<AppConfig> {
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
            if app.cwd.is_none() {
                app.cwd = Some(dir.clone());
            }
            app
        })
        .collect()
}

/// The flock as it stands, for deciding whether a target names a sheep that
/// already exists.
///
/// A name is unique across a flock, so a target naming one can never have
/// meant "add another" -- there is no room for a second. Fetched ONCE per
/// invocation and matched locally: a Flockfile naming ten apps asks ten
/// questions of one listing, not for ten listings.
///
/// An unreachable or unexpected answer yields an empty flock rather than an
/// error. Failing `start` because the listing could not be read would trade a
/// working command for a defensive one, and the `Start` below reports its own
/// failures.
async fn flock_now(client: &Client) -> Vec<shep_core::protocol::ProcessInfo> {
    match client.request(Request::ListFlock).await {
        Ok(Response::Flock(procs)) => procs,
        _ => Vec::new(),
    }
}

/// Says so when a Flockfile app the flock already has is registered under a
/// different config, naming the sheep and the fields that differ.
///
/// The defect this exists for: an operator edits `cwd` on two apps, re-runs
/// `shep start`, and both keep running the old one. `Request::Start` on a
/// name the flock already has adds instances rather than reconciling config,
/// which is what `shep stock` depends on, so the edit is simply not applied.
/// It was also not reported, which is the half being fixed: the edit
/// vanished without a word and the apps crash-looped against a path that no
/// longer applied.
///
/// Reports rather than applies. Whether `start` should reconcile by default,
/// or grow an `--update` flag, is the maintainer's call and neither is taken here; a
/// running flock changing its cwd or argv underneath an operator would be a
/// worse surprise than the one being fixed.
///
/// Field NAMES only, never values: this reaches an operator's terminal and
/// `env` carries secrets, so a differing `env` says `env` and stops (IR-41).
/// [`AppConfig::drifted_fields`] is where that rule is enforced.
///
/// Silent on an unreachable daemon, an unexpected answer, or a daemon too
/// old to know the request, the same call [`flock_now`] makes: failing a
/// `start` over a warning it could not compute would trade a working command
/// for a defensive one.
///
/// One request for the whole invocation, not one per app. The rows beside
/// each app go unread: drift is a comparison between two CONFIGS, and every
/// instance of a clustered app shares one.
async fn report_config_drift(
    client: &Client,
    streams: &mut Streams<'_>,
    resumed: &[(AppConfig, Vec<shep_core::protocol::ProcessInfo>)],
) {
    if resumed.is_empty() {
        return;
    }
    let apps = resumed.iter().map(|(app, _)| app.clone()).collect();
    let Ok(Response::Drifted(drifted)) = client.request(Request::ConfigDrift { apps }).await else {
        return;
    };
    for drift in drifted {
        let name = &drift.name;
        let fields = drift.fields.join(", ");
        let message = format!(
            "{name} is registered with a different config ({fields}). `shep start` \
             adds instances to a sheep the flock already has; it does not apply \
             config edits, so the edit is not in effect. To apply it: `shep \
             delete {name}`, then start again."
        );
        streams.aside("start", &message);
    }
}

/// `shep.toml`'s `[interpreters]` entry for `script`'s own extension, if it
/// has one and the map names it.
///
/// `Path::extension` already answers "no extension" the way this needs to:
/// a dotfile with nothing before its one dot (`.bashrc`) has none, so a
/// `[interpreters]` entry keyed `""` (nobody would write one, but nothing
/// stops them) can never match here either.
fn mapped_interpreter(script: &str, interpreters: &BTreeMap<String, String>) -> Option<String> {
    let extension = Path::new(script).extension()?.to_str()?;
    interpreters.get(extension).cloned()
}

/// Folds `shep.toml`'s `[interpreters]` mapping and `--interpreter` onto
/// `apps`, in the precedence the maintainer fixed for task 47: shep.toml, then a
/// Flockfile's own `interpreter` field, then the flag -- last one to touch
/// an app wins.
///
/// The Flockfile layer needs no code of its own here: [`resolve_target`]
/// already left an app's `interpreter` at whatever its source said, `None`
/// for anything that named none, so only filling `None` slots from
/// `interpreters` is what makes an app's own explicit value (including the
/// literal `"none"`, which is `Some("none")`, not `None`) outrank the map.
/// `flag`, when given, then overwrites every app unconditionally -- the top
/// layer, matching `--cwd`/`--fold` immediately above this function's own
/// call site.
fn apply_interpreters(
    apps: &mut [AppConfig],
    interpreters: &BTreeMap<String, String>,
    flag: Option<&str>,
) {
    if !interpreters.is_empty() {
        for app in apps.iter_mut() {
            if app.interpreter.is_none()
                && let Some(mapped) = mapped_interpreter(&app.script, interpreters)
            {
                app.interpreter = Some(mapped);
            }
        }
    }
    if let Some(interpreter) = flag {
        for app in apps.iter_mut() {
            app.interpreter = Some(interpreter.to_string());
        }
    }
}

pub async fn start(
    client: &Client,
    streams: &mut Streams<'_>,
    args: &StartArgs,
    discovered: Option<&Path>,
    interpreters: &BTreeMap<String, String>,
) -> ExitCode {
    // `--name` renames the sheep a target becomes, and a name is unique to
    // one sheep, so it cannot mean anything across several targets.
    if args.name.is_some() && args.targets.len() > 1 {
        let message = "--name takes one target: a name belongs to one sheep";
        return streams.fail(ExitCode::Usage, message);
    }
    if args.targets.is_empty() {
        let mut started = Vec::new();
        let code = start_one(
            client,
            streams,
            args,
            None,
            discovered,
            interpreters,
            &mut started,
        )
        .await;
        // Printed whenever the verb did its work, not only when it touched a row.
        // `shep start koji` against a koji that is already online starts nothing
        // and succeeds, and the flock is still the answer to what happens next --
        // before this it printed the notice and no table at all. A verb that
        // FAILED still leaves stdout empty, which is the discipline `cli_e2e`'s
        // `assert_json_error` pins crate-wide.
        if code == ExitCode::Success {
            let wrote = render_outcome(client, streams, "start", FlockRows(started)).await;
            if wrote != ExitCode::Success {
                return wrote;
            }
        }
        return code;
    }
    // In turn, not atomically: if the second target fails the first is
    // already up. The exit code is the first failure, so a partial success
    // is never reported as a whole one.
    let mut failure: Option<ExitCode> = None;
    let mut started = Vec::new();
    for target in &args.targets {
        let code = start_one(
            client,
            streams,
            args,
            Some(target),
            discovered,
            interpreters,
            &mut started,
        )
        .await;
        if code != ExitCode::Success {
            failure = failure.or(Some(code));
        }
    }
    // One table for the whole invocation, matching `stop` and `restart`. A
    // header per target would be the only place in the CLI where asking for
    // three things prints three tables.
    // Printed whenever the verb did its work, not only when it touched a row.
    // `shep start koji` against a koji that is already online starts nothing
    // and succeeds, and the flock is still the answer to what happens next --
    // before this it printed the notice and no table at all. A verb that
    // FAILED still leaves stdout empty, which is the discipline `cli_e2e`'s
    // `assert_json_error` pins crate-wide.
    //
    // Keyed on the outcome ALONE, never on `started` being non-empty. The two
    // agreed while a failure stopped the run, since nothing could follow one
    // and add a row: now that every row is attempted, a fold whose second app
    // fails and whose third comes up fine ends non-empty AND failed, and the
    // old condition printed a table on the error path. Under `--format json`
    // that is a data envelope beside an error envelope, which is two answers
    // to one question and what `cli_e2e`'s `assert_json_error` refuses.
    if failure.is_none() {
        let wrote = render_outcome(client, streams, "start", FlockRows(started)).await;
        if wrote != ExitCode::Success {
            return wrote;
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// One target's worth of [`start`].
///
/// Eight parameters, the same growth `lookout::frames::sheep`'s own
/// `#[allow(clippy::too_many_arguments)]` already accepted for itself:
/// `client`, `streams` and `fmt` are the RPC/rendering plumbing every verb
/// in this file threads through; `args`, `target` and `discovered` are the
/// three ways one invocation names what to start; `interpreters` is task
/// 47's `shep.toml` mapping, read once by the caller rather than
/// re-reading the file per target; and `started` is the caller's own
/// accumulator. None of the eight groups naturally into a struct without
/// inventing one used nowhere else, the same call that function's own doc
/// makes.
#[allow(clippy::too_many_arguments)]
async fn start_one(
    client: &Client,
    streams: &mut Streams<'_>,
    args: &StartArgs,
    target: Option<&str>,
    discovered: Option<&Path>,
    interpreters: &BTreeMap<String, String>,
    started: &mut Vec<shep_core::protocol::ProcessInfo>,
) -> ExitCode {
    // `args.target` is optional so bare `shep start` can mean "this
    // directory's Flockfile". The caller does the discovery, because the
    // no-target-and-no-Flockfile case never reaches here: it brings a
    // shepherd up and stops.
    //
    // Everything from here to `resolve_target` is the precedence in
    // `StartArgs::targets`' own help: a sheep by id or name, then a fold, then
    // a Flockfile, then a path. Each tier claims the token only if the token
    // actually resolves there, so a name the flock does not have still reaches
    // the file of that name.
    //
    // `-` and `--flockfile` skip the flock entirely. Both say outright that
    // the token is a source to read, and neither has ever meant anything else.
    let mut listing: Option<Vec<ProcessInfo>> = None;
    let mut missed: Option<String> = None;
    if let Some(token) = target
        && token != "-"
        && !args.flockfile
    {
        // Parsed client-side, exactly as every selector-taking verb does, so a
        // malformed one is a local usage error rather than a round trip. This
        // is also what makes `shep start fold:backed` and `shep start all`
        // work at all: `start` takes the same argument grammar as every
        // other lifecycle verb, so folds are actionable here too.
        let selector = match parse_selector(streams, token) {
            Ok(selector) => selector,
            Err(code) => return code,
        };
        if is_reachable_as_a_name(&selector) {
            let flock = flock_now(client).await;
            let matched = flock_matches(&selector, &flock);
            if !matched.is_empty() {
                // No `report_config_drift` on this path, and that is not an
                // omission: drift is a comparison between a config the
                // operator just supplied and the one the flock stores. A
                // selector supplies no config -- `shep start fold:backed`
                // names sheep, not a Flockfile -- so there is nothing to
                // compare and nothing that could have been silently ignored.
                return resume_all(client, streams, Some(token), &matched, started).await;
            }
            // Held for the failure path below rather than reported here: the
            // token may still name a Flockfile or a path, and only if it names
            // neither does the shape it was written in decide the message.
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

    let mut apps = match resolve_target(target, args.name.as_deref(), &stdin, args.flockfile) {
        Ok(apps) => apps,
        // The last tier refused too, so the token named nothing anywhere. A
        // token written unmistakably as a selector is reported as one -- `shep
        // start fold:typo` says no sheep is in a fold called typo, rather than
        // reporting that no file called `fold:typo` is on disk, which is the
        // answer to a question nobody asked. Exit 3, matching what every other
        // verb returns for a selector that matched nothing.
        Err(TargetError::Unresolvable { .. }) if missed.is_some() => {
            let message = missed.unwrap_or_default();
            return streams.fail(ExitCode::NotFound, &message);
        }
        Err(err) => return fail_target(streams, &err),
    };

    if let Some(fold) = &args.fold {
        for app in &mut apps {
            app.fold = Some(fold.clone());
        }
    }
    // After the per-app defaults above, so an explicit flag wins over both
    // the script form's "where you are" default and a Flockfile's own value.
    if let Some(cwd) = &args.cwd {
        for app in &mut apps {
            app.cwd = Some(cwd.clone());
        }
    }
    apply_interpreters(&mut apps, interpreters, args.interpreter.as_deref());
    // The same rule the bare-name lookup above applies, now that the target
    // has become a set of named apps: a name the flock already has is that
    // sheep. Without this, `shep start ./thing` against a running `thing`
    // spawned a SECOND copy of a one-instance app -- `instance_slots`
    // allocates the lowest free slot, so the two sat side by side as ids 0
    // and 1 under one name, and the next save persisted `instances_running: 2`
    // for an app configured for 1.
    let flock = match listing {
        Some(flock) => flock,
        None => flock_now(client).await,
    };
    // EVERY row the name has, not the first one. A clustered app is several
    // rows under one name, and taking one of them as the app's stand-in meant
    // a Flockfile naming a stopped four-instance app started instance 0 and
    // left the other three down. Worse when instance 0 was the live one: the
    // "already running" notice fired for the whole app and nothing started at
    // all. Invisible while respawns went out per name, because a name
    // selector reached the other three anyway; the moment they go out per
    // row, the representative is all this arm would act on.
    let mut resumed: Vec<(AppConfig, Vec<ProcessInfo>)> = Vec::new();
    let mut fresh = Vec::new();
    for app in apps {
        let rows: Vec<ProcessInfo> = flock
            .iter()
            .filter(|info| info.name == app.name)
            .cloned()
            .collect();
        if rows.is_empty() {
            fresh.push(app);
        } else {
            resumed.push((app, rows));
        }
    }
    // Before the resumes below, not after: an operator who edited the file
    // and gets a wall of restart output should read why none of it applied
    // at the top of the run rather than under it.
    report_config_drift(client, streams, &resumed).await;
    if !resumed.is_empty() {
        // `None`, not the operator's token: this arm reached the flock through
        // a Flockfile or a path, so there is no selector to quote back in the
        // "already running" notice. `shep restart ./rotom.sh` is not a
        // command. See `resume_all`'s own doc.
        let existing: Vec<ProcessInfo> = resumed
            .iter()
            .flat_map(|(_, rows)| rows.iter().cloned())
            .collect();
        let code = resume_all(client, streams, None, &existing, started).await;
        if code != ExitCode::Success {
            return code;
        }
    }
    if fresh.is_empty() {
        return ExitCode::Success;
    }
    let apps = fresh;

    let (procs, failure) = request_each(
        client,
        streams, // One request either way: `Start` carries the apps, so the "selector"
        // here is a placeholder the body closure ignores. Reusing the looping
        // helper keeps the collect-then-render shape in one place.
        &[SelectorSpec::All],
        Some(START_DEADLINE),
        |_| Request::Start { apps: apps.clone() },
        |response| match response {
            Response::Started(procs) => Some(procs),
            _ => None,
        },
    )
    .await;
    started.extend(procs);
    failure.unwrap_or(ExitCode::Success)
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
    // See `start`'s own note: printed whenever the verb did its work.
    if !procs.is_empty() || failure.is_none() {
        let wrote = render_outcome(client, streams, "stop", FlockRows(procs)).await;
        if wrote != ExitCode::Success {
            return wrote;
        }
    }
    failure.unwrap_or(ExitCode::Success)
}

/// Restarts the sheep matching `args.selector`.
pub async fn restart(client: &Client, streams: &mut Streams<'_>, args: &SelectorArgs) -> ExitCode {
    let selectors = match parse_selectors(streams, &args.selectors) {
        Ok(selectors) => selectors,
        Err(code) => return code,
    };
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
    // Named before `procs` is moved into the table below. A failed respawn
    // reaches here as an ordinary `errored` row inside an `Ok`, because the
    // daemon's aggregation reply for `Restart` has no per-id error slot
    // (`shep-daemon/src/supervisor.rs`'s `respawn`, `Err` arm) -- the same
    // gap that let `shep start <name>` exit 0 in silence.
    let failed: Vec<String> = procs
        .iter()
        .filter(|info| info.status == shep_core::status::ProcStatus::Errored)
        .map(|info| info.name.clone())
        .collect();

    // Stdout stays empty on a failure, exactly as `start`'s own failure path
    // and every other verb's do -- `cli_e2e`'s `assert_json_error` pins that
    // discipline crate-wide, and under `--format json` printing a data
    // envelope AND an error envelope would hand a consumer two answers to
    // one question. The cost is real and taken deliberately: a `restart all`
    // where one of ten fails no longer lists the nine that came back, and
    // the operator reads them from `shep flock`. Consistency across verbs is
    // worth more here than this verb keeping its table on the one path where
    // it has bad news to deliver.
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
/// a fresh one so the app has a window in which it can hand over.
///
/// Sends `Request::Reload` with the client's default deadline, exactly as
/// `stop`/`restart`/`delete` do, and for a reason particular to this verb: the
/// daemon answers as soon as the reload is ACCEPTED rather than when the swaps
/// finish (see `Response::Reloading`), so a longer deadline would buy nothing
/// — the answer is already back long before the first swap commits. The rows
/// printed are the flock as it stood at acceptance, and the swaps report
/// themselves on the bus.
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
    // See `start`'s own note: printed whenever the verb did its work.
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
    // See `start`'s own note: printed whenever the verb did its work. The
    // aside below is guarded separately -- there is nothing to name when
    // nothing was deleted.
    if !ids.is_empty() || failure.is_none() {
        // The listing this prints no longer holds what was deleted, so the
        // ids go to stderr rather than being lost. Named as ids and not as
        // names because ids are all `Response::Deleted` carries, and inventing
        // the names would need a second listing taken before the delete.
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
/// instances that remain.
///
/// No `parse_selector` call, unlike every other verb in this module: `stock`
/// takes a name. See [`StockArgs`]'s own doc for why.
///
/// Sends `Request::Scale` with `START_DEADLINE`, not the client's default: a
/// stock-up spawns processes, which is the same work `start` already asks
/// for the longer budget to cover.
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
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    /// Bare `shep start` with a Flockfile discovered by the caller must
    /// start that file. The no-target-and-no-Flockfile case never reaches
    /// here -- it brings a shepherd up and stops -- so the remaining hole
    /// this guards is a `start` that ignored the discovery and refused.
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

    /// The CLI answers `exists` from its own cwd and the daemon spawns from
    /// a different one, so a relative script has to be absolutised before it
    /// crosses. The maintainer hit this from `~`: `shep start ./GitHub/zeus/...` where
    /// the file plainly existed, refused with `No such file or directory`
    /// A Flockfile is a file you commit, so an app that names no `cwd` runs
    /// where the Flockfile lives -- not where the daemon happened to be
    /// started, which is invisible from the command line and effectively
    /// arbitrary.
    ///
    /// Before this, the same committed file worked on the machine where the
    /// shepherd was started in the right directory and failed on the next
    /// one with `No such file or directory`, naming neither cause.
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

        // The same shape the app is given: canonical, and on Windows without
        // the verbatim prefix, which the daemon would otherwise echo back in
        // `shep describe`.
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let expected = shep_core::paths::strip_verbatim_prefix(&canonical).into_owned();
        assert_eq!(
            apps[0].cwd.as_deref(),
            Some(expected.to_string_lossy().as_ref()),
            "the app runs where its Flockfile lives"
        );
    }

    /// Only a blank is filled. An app that states its own `cwd` keeps it,
    /// because the operator said something and shep is not entitled to
    /// overrule it.
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

    /// because the shepherd's cwd was a checkout elsewhere.
    #[test]
    fn a_relative_script_is_resolved_against_the_callers_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("bin");
        std::fs::create_dir_all(&nested).unwrap();
        let script = nested.join("thing");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();

        // Resolved from the tempdir, the way the CLI resolves from wherever
        // the operator is standing.
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

    /// The first `Start` on the wire, skipping the flock lookup every `start`
    /// now issues first.
    ///
    /// `start` consults the flock before treating a target as a path, because
    /// a name the flock already has is that sheep rather than a new one. That
    /// puts one `ListFlock` ahead of the request these tests are about.
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
        }
    }

    /// Covers `resolve_target`'s pure `-` arm only — `stdin` here is bytes
    /// already read, handed straight in. The real `std::io::stdin()` read
    /// inside `start` (this file's `if args.target == "-"` block) has no
    /// injection seam and is a documented gap, not tested: swapping the
    /// process's real fd 0 to feed it would race whatever other test
    /// `cargo test`'s multi-threaded runner happens to run concurrently,
    /// and reading the harness's own actual stdin blocks or EOFs depending
    /// on how the suite was invoked — neither is a real assertion, and
    /// inventing one that passes either way is worse than admitting the
    /// gap.
    #[test]
    fn a_dash_target_reads_a_flockfile_from_stdin_as_json() {
        // `app`, not `apps` — the wire key is renamed and unknown keys are a
        // hard error (flockfile.rs:23-32).
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

    /// fails if `.js` is ever routed to the node bridge without the flag —
    /// the regression that would break `shep start server.js` for every
    /// user who has ever typed it. Deliberately a sibling of
    /// `any_other_existing_path_becomes_one_minimal_app_named_for_its_stem`.
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

    /// fails if `--flockfile` changes how a recognised extension is read.
    #[test]
    fn the_flag_does_not_change_a_toml_flockfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.toml");
        std::fs::write(&path, "[[app]]\nname = \"web\"\nscript = \"./srv\"\n").unwrap();
        let with = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap();
        let without = resolve_target(path.to_str().unwrap(), None, b"", false).unwrap();
        assert_eq!(with, without);
    }

    /// fails if an unreadable extension under the flag falls through to the
    /// script arm instead of refusing — which would silently start the
    /// operator's config file as a program.
    #[test]
    fn the_flag_refuses_an_extension_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flock.ini");
        std::fs::write(&path, "").unwrap();
        let err = resolve_target(path.to_str().unwrap(), None, b"", true).unwrap_err();
        assert!(matches!(err, TargetError::UnknownFlockfileFormat { .. }));
        assert_eq!(target_exit_code(&err), ExitCode::Usage);
    }

    /// Returns `true` when node is on PATH. The `.js` cases below are the
    /// only tests in the workspace that need a second runtime, and a machine
    /// without node must not fail the suite.
    ///
    /// The `eprintln!` is not the guard: libtest captures the output of a
    /// test that PASSES and prints it only on failure, so a skip is
    /// invisible under a plain `cargo test` — which is exactly the host this
    /// helper exists for. `SHEP_REQUIRE_NODE=1` is the guard. Set it on any
    /// machine that has node (the task gate below does) and a broken helper
    /// is a panic rather than a green run over three tests that never ran.
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

    /// fails if a throwing config is reported as anything but InvalidConfig,
    /// or if node's own message is dropped on the floor.
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

    /// fails if a config module that keeps node alive hangs shep, which is
    /// exactly what one did until this budget existed. `setInterval` leaves a
    /// handle on node's event loop, so node stays running long after `require`
    /// returned and `module.exports` was assigned -- the same mechanism as the
    /// server-at-require-time shape `docs/specs/deferred.md` named, without
    /// binding a port to get it.
    ///
    /// The budget is 200ms rather than [`JS_EVAL_BUDGET`] so the test costs a
    /// fifth of a second; what it pins is that the bound is enforced and says
    /// so, not what the shipped bound is.
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

    /// fails if a pm2 ecosystem file is accepted, or if the refusal stops
    /// naming the key the operator has to change. Decision 2: this feature
    /// reads a Flockfile-shaped .js, and serde's own message is the answer.
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

    /// Drives the VERB, not `resolve_target`, so the assertion covers the
    /// mapping as well as the resolution — and proves nothing reached the
    /// wire. A `start` that shipped the unresolved string to the daemon and
    /// let it fail would return `NotFound` after a round trip and fail both
    /// assertions.
    /// `shep start zeus-auth` on a sheep the flock already has must act on
    /// that sheep, not look for a file called `zeus-auth`. A name is unique
    /// across a flock, so a target naming one can never have meant "add
    /// another" -- there is no room for a second.
    ///
    /// Armed through `fake_client_on` rather than the envelope-capturing
    /// fixture, because the flock has to ANSWER for the lookup to find
    /// anything, and only this one lets a test arm the reply.
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

        // The path arms would have refused: there is no file called
        // `zeus-auth` here. Anything other than a usage error means the
        // lookup found the sheep and acted on it instead.
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
    /// did not touch, one it did, and a dog.
    ///
    /// The names are chosen so the narrow payload and the full listing cannot
    /// be confused for one another: a test that asserted "the output mentions
    /// koji" would pass on the one-row table this change replaces, so every
    /// assertion below is on the exact SET of rows.
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

    /// fails if a lifecycle verb prints only the rows it touched.
    ///
    /// `shep start koji` printed a one-row table containing koji. The question
    /// after starting one app is "what does the flock look like now", which a
    /// one-row table cannot answer.
    ///
    /// The narrow payload handed in is koji ALONE, and the armed listing has
    /// four entries, so the two are distinguishable by row count as well as by
    /// content -- a build that kept rendering the narrow payload prints one
    /// row and fails on the first assertion rather than passing on a
    /// coincidence of names.
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
        // Split at the caption rather than filtering the whole output: the two
        // tables have different columns, so a NAME read from the wrong one is
        // a different field. This is also what proves the dog is in the SECOND
        // table and not merely somewhere on the page.
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
        // Column 1, not 0: the dogs table leads with ID now, exactly as the
        // sheep table does.
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

    /// fails if `--format json` is widened to the whole flock, or if it pays
    /// for a listing it does not render.
    ///
    /// The machine surface answers a different question from the human one. A
    /// script that runs `shep stop koji --format json` asked about koji and
    /// reads `data[0]` to learn what it stopped; handing it four rows, three
    /// of which it never asked about, breaks it silently.
    ///
    /// `list_flock_count` is the second half and is not decoration: it is what
    /// proves the JSON path SKIPS the listing rather than fetching one and
    /// discarding it. Without it a build that always asked and then chose what
    /// to print would pass.
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

    /// A flock in which every tier of `start`'s precedence has something to
    /// find, and each tier's answer is distinguishable from the others'.
    ///
    /// `golbat` and `koji` are in the fold `backed`; `rotom` is not in any
    /// fold; `log-rotate` is a dog. The name `backed` is a fold and NOT a
    /// sheep, which is the whole point: a build that only ever matched names
    /// finds nothing for it.
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

    /// The names `flock_matches` picks, so a case below reads as the rule it
    /// is about rather than as a fold of `ProcessInfo` fields.
    fn matched_names(target: &str) -> Vec<String> {
        let selector = ProcessSelector::parse(target).expect("the fixture uses valid selectors");
        flock_matches(&selector, &a_foldable_flock())
            .into_iter()
            .map(|info| info.name)
            .collect()
    }

    /// The precedence `StartArgs::targets`' own help states, one case per
    /// tier, driven through the function that decides it.
    ///
    /// `shep stop fold:backed` already worked and `shep start fold:backed`
    /// refused with "backed is not `-`, a recognised Flockfile, or an existing
    /// path", because `start` took a different argument grammar from every
    /// other lifecycle verb. Folds were actionable everywhere except the verb
    /// that creates things.
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

    /// fails if a name that is BOTH a sheep and a fold resolves to the fold.
    ///
    /// The order is name before fold, and this is the only fixture that can
    /// tell the two apart: `a_start_target_walks_the_precedence`'s `backed` is
    /// a fold and not a sheep, so it would pass under either order.
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

    /// fails if a wildcard sweeps up a dog.
    ///
    /// The same rule `ProcessSelector::is_exact` states for the daemon's own
    /// matching: a dog is a process an operator installed, not a member of the
    /// flock `all` means, so `shep start all` must pass it by while
    /// `shep start log-rotate` still reaches it. The fold fallback counts as a
    /// wildcard even though the token was a bare name, because the operator
    /// named a group.
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

    /// fails if a token carrying a path separator is tried as a sheep or a
    /// fold.
    ///
    /// This is the escape hatch, and it has to be a rule rather than a
    /// coincidence: somebody whose fold shares a name with a file in the
    /// current directory needs a way to say which they meant, and the
    /// precedence alone gives them none. A sheep name may never contain a path
    /// separator (`shep_core::config::normalize`), so `./backed` is always the
    /// file.
    ///
    /// `/web/` is in the same case for the opposite reason: it is full of
    /// slashes and is a regex, not a name, so the rule is on the PARSED form
    /// rather than on the raw token.
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

    /// fails if `shep start fold:typo` reports that no FILE called `fold:typo`
    /// is on disk.
    ///
    /// That was the error the maintainer actually hit, and it sent her looking for a file
    /// she had never asked about. A token written unmistakably as a selector
    /// is reported as one; a bare name or id carries no marker and may equally
    /// have been meant as a filename, so it keeps the message that names every
    /// tier.
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

    /// fails if a name repeated NON-ADJACENTLY produces two respawns.
    ///
    /// `unique_names` compared each name against the previous one only, which
    /// drops a duplicate solely when the two sit next to each other. That is
    /// true of a name-sorted listing and NOT true of `start_one`'s Flockfile
    /// and path arm, which builds its set from the resolved apps in the file's
    /// own order. Two instances of one app listed either side of a third then
    /// produced a second `Request::Restart` and restarted that app twice in
    /// one invocation.
    ///
    /// The fixture is `web`, `api`, `web` precisely because a sorted one
    /// cannot exhibit it: sorted, the two `web` rows are adjacent and the old
    /// code was already correct. First-seen order is asserted too, since the
    /// notice this feeds reads as a list.
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

    /// fails if `shep start all` calls a flock empty while it holds dogs.
    ///
    /// `flock_matches` passes a dog by for every wildcard, so `all` matching
    /// nothing means there are no SHEEP. Reading that as an empty flock made
    /// `shep start all` print "the flock is empty" on a machine where
    /// `shep flock` was printing dog rows at the same moment, which is a
    /// contradiction an operator has to reconcile on their own.
    ///
    /// Both halves in one case, because a build that always says "no sheep"
    /// and a build that always says "empty" each pass one of them.
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

    /// fails if `shep start fold:typo` exits anything but 3, or if it reaches
    /// the daemon with a `Start`.
    ///
    /// Driven through the VERB rather than through `selector_miss`, because
    /// the mapping from "matched nothing" to an exit code lives in `start_one`
    /// and a unit test of the message alone cannot see it. Exit 3 is what
    /// `shep stop fold:typo` already returns, which is the consistency this
    /// whole change is about.
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

    /// A sheep that is already up is reported, never restarted. Someone who
    /// typed `start` did not ask for their live service to be replaced, and
    /// `restart` is right there.
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

    /// Three instances of one clustered app, stopped unless named in
    /// `online`, the shape both respawn
    /// bugs needed and the shape `a_foldable_flock` cannot show: every sheep
    /// in it has a name of its own, so a selector naming a name and a
    /// selector naming a row pick the same set there.
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

    /// A fake that answers a `start` invocation end to end: the listing it
    /// begins with, the respawns it decides on, the drift check, and the
    /// second listing it renders from. `failing` names the ids to answer as
    /// `errored`, which is how a spawn failure reaches this verb.
    fn a_daemon_for(
        flock: Vec<ProcessInfo>,
        failing: &'static [u32],
    ) -> impl Fn(&Request) -> Response + Send + 'static {
        use shep_core::status::ProcStatus;
        move |request| match request {
            Request::ListFlock => Response::Flock(flock.clone()),
            Request::ConfigDrift { .. } => Response::Drifted(Vec::new()),
            Request::Restart { selector } => {
                let SelectorSpec::Id(id) = selector else {
                    // Never reached by a correct build, and asserted on
                    // directly below -- answered rather than panicked so the
                    // assertion that names the bug is the one that fails.
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
                client,
                &mut streams,
                &start_args(target),
                None,
                &BTreeMap::new(),
            )
            .await
        };
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    /// fails if `shep start 0` respawns anything but id 0.
    ///
    /// The maintainer's own flock: ten instances of `zam`, all stopped, and
    /// `shep start 0` brought all ten back. `resume_all` collapsed the rows
    /// it matched to their distinct NAMES before sending, and a name selector
    /// reaches every instance the name has. `Id` is the only selector form
    /// that can name a subset of one name's rows, so it was the only form the
    /// collapse could widen -- and it is exactly the form an operator reaches
    /// for to name one instance of a clustered app.
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

    /// fails if `start` respawns a sheep it has just reported as already up.
    ///
    /// The same collapse, and the worse half of it: this one needs no `Id`
    /// selector at all. `resume_all` partitions the rows it matched into live
    /// and asleep precisely so that a live one is left alone -- and then sent
    /// a NAME, which walks straight back over the row it had just set aside.
    /// `shep start all` against a clustered app with one instance up
    /// restarted that instance, which is the surprise `start` documents
    /// itself as refusing to be clever about.
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

    /// fails if the notice for a single live row suggests a command wider
    /// than the one the operator typed.
    ///
    /// `shep start 0` against a live instance answered "`shep restart zam`
    /// replaces it", which replaces all ten. The suggestion has to be the
    /// operator's own token whenever there is one; only a path or Flockfile
    /// target, which is not a selector and cannot be quoted back, falls back
    /// to the name.
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

    /// fails if one row failing to spawn abandons the rows after it.
    ///
    /// `request_each` states the discipline for every selector-taking verb --
    /// attempt them all, report the FIRST failure -- because stopping early
    /// leaves the operator guessing which of the rest were touched.
    /// `resume_all` returned on the first failure instead, so one app in a
    /// fold failing to come up left the fold's remaining apps down without a
    /// word about them.
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

    /// fails if a Flockfile naming a clustered app starts only one instance.
    ///
    /// This arm matched an app to the flock with a `find`, which takes the
    /// FIRST row of a name and calls it the app. Harmless while respawns went
    /// out per name -- the name reached the other instances anyway -- and a
    /// silent halving the moment they go out per row. Both halves are here:
    /// three rows respawned, and none of them skipped because the
    /// representative happened to be the live one.
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
        // NOTHING reaches the daemon. `./does-not-exist` carries a path
        // separator, and a sheep name may never contain one
        // (`shep_core::config::normalize`), so the token cannot be a sheep or
        // a fold and `start` skips the flock lookup rather than asking a
        // question whose answer it already knows. A target with no separator
        // does ask -- `a_target_naming_a_stopped_sheep_is_acted_on_not_
        // resolved_as_a_path` is the case that proves it.
        //
        // What must never reach the daemon either way is a `Start` carrying an
        // app built from an unresolvable target.
        assert!(
            envelopes.try_recv().is_err(),
            "a target that can only be a path costs no round trip, and an \
             unresolvable one must never become a Start"
        );
        assert!(String::from_utf8(err).unwrap().contains("./does-not-exist"));
    }

    /// The client-side parse is the point: every selector-taking verb must
    /// send a compiled `SelectorSpec` inside its OWN `Request` variant — not
    /// the raw string, not `SelectorSpec::All`, and not another verb's
    /// variant. Asserting the whole `sent.body` (not just the selector
    /// inside it) is what catches a verb sending the wrong request kind:
    /// the reviewer proved this reachable by mutating both `restart` and
    /// `delete` to send `Request::Stop { selector }` and getting 9 passed,
    /// 0 failed — because the previous version of this test only ever
    /// drove `stop`. Also pins that all four call `request_and_render`
    /// with `deadline: None` — visible on the wire as `DEFAULT_DEADLINE`,
    /// since `Client::request_with_deadline` never leaves `deadline_ms`
    /// unstated — and a verb that stopped doing so passed unnoticed before
    /// this.
    ///
    /// `reload` belongs in the same list and takes the same default despite
    /// being the slowest thing this CLI can ask for: the daemon answers it
    /// as soon as the reload is accepted, so the round trip is a short one
    /// and a `START_DEADLINE`-sized budget here would be a claim about the
    /// swaps that the reply never waits for anyway.
    #[tokio::test]
    async fn a_selector_reaches_the_wire_in_its_compiled_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;

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
                    Verb::Restart => restart(&client, &mut streams, &args).await,
                    Verb::Reload => reload(&client, &mut streams, &args).await,
                    Verb::Delete => delete(&client, &mut streams, &args).await,
                };
                let sent = envelopes.recv().await.unwrap();
                assert_eq!(sent.body, expected_body, "verb={verb:?} input={input}");
                // `request_and_render` is called with `deadline: None` for
                // every verb here — `Client::request_with_deadline` never
                // leaves that unstated on the wire — `request_with_deadline`
                // fills in `DEFAULT_DEADLINE` — so the wire-level
                // signal that the call site truly passed `None` (rather
                // than some other explicit `Some(_)`) is the envelope
                // carrying exactly that default.
                assert_eq!(
                    sent.deadline_ms,
                    Some(u64::try_from(DEFAULT_DEADLINE.as_millis()).unwrap()),
                    "verb={verb:?} input={input} must defer to the client's default deadline"
                );
            }
        }
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    /// A verb that skipped the client-side parse would send it and exit
    /// `NotFound` instead.
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

    /// Bounded by `timeout` rather than left to run to completion: `start`
    /// returns early with `ExitCode::Usage` whenever `resolve_target` fails
    /// — before any request is built — so a regression that reintroduced
    /// an early return here would otherwise hang this test forever on
    /// `envelopes.recv()`, reporting a killed CI job rather than a named
    /// assertion. Also asserts `sent.body`, not only `sent.deadline_ms` —
    /// a `start` that sent the wrong request with the right deadline must
    /// not pass.
    ///
    /// The fixture itself lives in this test's own tempdir rather than a
    /// tracked file at the crate root: a tracked fixture's absence is what
    /// caused the hang above, it depended on Cargo running test binaries
    /// with CWD == package root, and a `.toml`-named fixture would not
    /// substitute — that extension routes `resolve_target` into
    /// `Flockfile::parse`, a different branch entirely.
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
        // `cwd` comes along now: a script started by path runs where the
        // operator stood, not where the shepherd was spawned. The redacted
        // `Debug` does not print it, so a mismatch here reads as two
        // identical-looking values.
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

    /// `--fold` (the `if let Some(fold) = &args.fold` loop, this file's
    /// `start`) is spec'd behaviour with, until now, no guard: deleting
    /// that loop left every other test in this module green (9 passed).
    /// Assert the fold actually lands on the `AppConfig` that reaches the
    /// wire, not merely that `start` still exits `Success`.
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

    /// [`mapped_interpreter`]'s own extension grammar: matched with the
    /// dot stripped, absent for a name with no extension, and absent for a
    /// dotfile whose one dot leads rather than separates -- `Path::extension`
    /// already reads `.bashrc` as extensionless, so a `[interpreters]` entry
    /// keyed `""` (nobody would write one, but nothing stops them) still
    /// cannot fire from here.
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

    /// Task 47's precedence, layer 1: `shep.toml`'s `[interpreters]` mapping
    /// fills a script's interpreter when nothing else already named one --
    /// the exact gap that made `shep start server.js` fail with
    /// `spawn_failed` before this task, the quick start `welcome.rs` and
    /// `--help` both advertise.
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

    /// Task 47's precedence, layer 2: a Flockfile app's own `interpreter`
    /// outranks the mapping, since it is the more specific statement about
    /// this one app. Without this, an operator naming `bun` for one app in
    /// a Flockfile would find shep.toml's `js -> node` overruling it, which
    /// is exactly backwards for a mapping that is supposed to be a
    /// fallback, not a policy.
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

    /// Task 47's precedence, layer 3: `--interpreter` outranks both the
    /// mapping and a Flockfile's own field -- the top layer, for the
    /// one-off override an operator types on the command line.
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

    /// [`any_restart_failed`]'s own logic, isolated from the wire: `true`
    /// only when at least one row is `errored`, matching
    /// [`resume`]'s own doc for why an `Ok` reply can still mean failure.
    #[test]
    fn any_restart_failed_is_true_only_for_an_errored_row() {
        use shep_core::protocol::ProcessInfo;
        use shep_core::status::ProcStatus;

        let online = ProcessInfo::builder(1, "web", ProcStatus::Online).build();
        let errored = ProcessInfo::builder(2, "worker", ProcStatus::Errored).build();
        assert!(!any_restart_failed(std::slice::from_ref(&online)));
        assert!(any_restart_failed(&[online, errored]));
    }

    /// fails if the envelope carries anything but the name and the count. `stock`
    /// is the one verb here that does NOT parse a selector, and a copy-pasted
    /// `parse_selector` would turn `web` into `SelectorSpec::Name("web")` and send
    /// a frame the daemon has no arm for.
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

    /// fails if an `InvalidConfig` refusal is swallowed or remapped. A count of 0
    /// is the shape an operator will actually type, and it has to come back as
    /// exit 4 with the daemon's own sentence, not as a generic failure.
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

    /// Wall-clock tests, skipped by every CI job but the serial `slow` one.
    ///
    /// The case below needs a real node to START AND EXIT inside the budget,
    /// which is a claim about how fast the machine is rather than about shep.
    /// At 200ms it failed on four CI runners at once (arm, macos, musl and
    /// the coverage job) while passing every local run: node took longer than
    /// that to come up, so the run hit the deadline still running and shep
    /// reported the kill it really had performed. The tier exists for exactly
    /// this, and the budget here is 5s because a stalled read waits out
    /// whatever is left of it, so the budget IS what the test costs.
    mod slow {
        use super::*;

        /// fails if a module that leaves a process on node's stdout is
        /// reported as a module shep killed. node itself exits here:
        /// `detached` plus `unref` takes the child off node's event loop, and
        /// `stdio: inherit` hands it the pipes shep is reading, so the wait
        /// ends on its own and only the reads run out of budget.
        #[test]
        fn a_js_flockfile_leaving_a_process_on_the_pipe_says_that_instead() {
            if !node_available() {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("flock.js");
            std::fs::write(
                &path,
                "require('child_process')\
                 .spawn('sleep', ['30'], { detached: true, stdio: 'inherit' })\
                 .unref(); \
                 module.exports = { app: [] };",
            )
            .unwrap();

            let err = evaluate_js_flockfile(&path, Duration::from_secs(5)).unwrap_err();

            assert_eq!(target_exit_code(&err), ExitCode::InvalidConfig);
            let message = err.to_string();
            assert!(
                message.contains("left behind still holds the output"),
                "got: {message}"
            );
            assert!(
                !message.contains("killed"),
                "node exited on its own, so nothing was killed: {message}"
            );
        }
    }
}
