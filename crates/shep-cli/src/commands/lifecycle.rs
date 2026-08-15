//! Lifecycle verbs: `start`, `stop`, `restart`, `delete` — the first verbs a
//! user types, and the first to exercise the whole client stack end to end.
//!
//! Every verb here receives an already-connected [`Client`]; `main` decides
//! how it got one (`connect_or_spawn` for `start`, `Client::connect` for
//! everything else) before dispatching — see that module's own doc. `start`
//! alone resolves a target into [`AppConfig`]s before anything reaches the
//! wire; [`resolve_target`] is that resolution, kept pure and separate from
//! the RPC so it stays fast and hermetic to test.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shep_client::{Client, START_DEADLINE};
use shep_core::config::{AppConfig, FlockFormat, Flockfile, FlockfileError};
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::{Format, SelectorArgs, StartArgs, StockArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{DeletedIds, FlockRows, Render, Streams, emit, emit_error, write_outcome};

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
    /// `target` was not `-`, had no recognised Flockfile extension, and did
    /// not name an existing path.
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
                "{target} is not `-`, a recognised Flockfile, or an existing path"
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

/// `start`'s mapping from a resolution failure to the exit code that
/// reports it.
fn target_exit_code(err: &TargetError) -> ExitCode {
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

/// The script handed to `node -e`. Wraps the `require` in its own
/// `try`/`catch` rather than letting an uncaught exception crash node and
/// relying on node's own crash-dump formatting — see this function's doc for
/// why. `process.argv[1]` is the absolute path, per `-e`'s argv layout
/// (identical to `-p`'s).
const JS_BRIDGE_SCRIPT: &str = "try { \
     process.stdout.write(JSON.stringify(require(process.argv[1]))); \
 } catch (err) { \
     process.stderr.write(err && err.message ? String(err.message) : String(err)); \
     process.exitCode = 1; \
 }";

/// Evaluates a `.js` Flockfile through node and returns its JSON.
///
/// The path is passed as an **argument**, never interpolated into the
/// JavaScript source: a path containing `'`, `\` or a newline would
/// otherwise escape the string literal, and adding a second way to inject
/// code into a file whose own code we are already about to run is
/// gratuitous. Under `-p` / `-e`, node puts the first user argument at
/// `process.argv[1]`.
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
/// the path at `process.argv[1]`, never in the source), so it is a narrower
/// implementation choice, not a different design.
///
/// **There is no timeout.** A module that never returns — one that starts a
/// server at require time — hangs here. The process is in the foreground and
/// interruptible; adding a bound means a reaper thread in a crate that
/// forbids unsafe code. Recorded in `docs/specs/deferred.md`.
///
/// **The `node_missing` sentence has no unit test**, for the same reason the
/// stdin gap a few functions up admits its own: producing it needs a `PATH`
/// without node, and `std::env::set_var` is `unsafe` in edition 2024 in a
/// crate that forbids unsafe. It is pinned in `docs/migration.md` instead,
/// by two greps in Task 9.
///
/// # Errors
///
/// - [`TargetError::Read`] — the path could not be canonicalized.
/// - [`TargetError::Js`] with `node_missing` — node is not on `PATH`.
/// - [`TargetError::Js`] — node ran and failed, or could not be spawned.
fn evaluate_js_flockfile(path: &Path) -> Result<String, TargetError> {
    let absolute = std::fs::canonicalize(path).map_err(|source| TargetError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let output = std::process::Command::new("node")
        .arg("-e")
        .arg(JS_BRIDGE_SCRIPT)
        .arg(&absolute)
        .stdin(std::process::Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
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
///   and node could not be run or could not evaluate it.
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
            Flockfile::parse(&source, FlockFormat::Json)
                .map(|flockfile| flockfile.apps)
                .map_err(TargetError::Flockfile)
        }
        (_, format) if as_flockfile => match format {
            Some(format) => {
                let source = std::fs::read_to_string(path).map_err(|source| TargetError::Read {
                    path: path.to_path_buf(),
                    source,
                })?;
                Flockfile::parse(&source, format)
                    .map(|flockfile| flockfile.apps)
                    .map_err(TargetError::Flockfile)
            }
            None if path.extension().and_then(|e| e.to_str()) == Some("js") => {
                let json = evaluate_js_flockfile(path)?;
                Flockfile::parse(&json, FlockFormat::Json)
                    .map(|flockfile| flockfile.apps)
                    .map_err(TargetError::Flockfile)
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
            Flockfile::parse(&source, format)
                .map(|flockfile| flockfile.apps)
                .map_err(TargetError::Flockfile)
        }
        _ if path.exists() => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(target);
            Ok(vec![AppConfig::minimal(name.unwrap_or(stem), target)])
        }
        _ => Err(TargetError::Unresolvable {
            target: target.to_string(),
        }),
    }
}

/// Sends `body` with `deadline` (`None` defers to the client's own default),
/// renders whatever the daemon answers through [`emit`], and maps every way
/// that can go wrong to its exit code.
///
/// `extract` pulls the verb's own payload out of `Response`; `Response` is
/// `#[non_exhaustive]` (Global Constraints), so an answer `extract` does not
/// recognise — a variant this client predates, or simply the wrong one for
/// this verb — maps to [`ExitCode::Internal`] rather than panicking.
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
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
            Some(payload) => write_outcome(emit(&mut *streams.out, fmt, command, payload)),
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

/// Renders `err` and returns the exit code `start` reports it as.
fn fail_target(streams: &mut Streams<'_>, fmt: Format, err: &TargetError) -> ExitCode {
    let code = target_exit_code(err);
    let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
    code
}

/// Starts one or more sheep, resolved from `args.target` — see
/// [`resolve_target`].
///
/// Sends `Request::Start` with `START_DEADLINE` rather than the client's
/// default: a cold spawn plus a readiness probe routinely outruns 5
/// seconds, and a client-side abandonment there would report failure for a
/// sheep that came up fine.
pub async fn start(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &StartArgs,
) -> ExitCode {
    let stdin = if args.target == "-" {
        let mut buf = Vec::new();
        if let Err(source) = std::io::stdin().lock().read_to_end(&mut buf) {
            return fail_target(streams, fmt, &TargetError::Stdin(source));
        }
        buf
    } else {
        Vec::new()
    };

    let mut apps = match resolve_target(&args.target, args.name.as_deref(), &stdin, args.flockfile)
    {
        Ok(apps) => apps,
        Err(err) => return fail_target(streams, fmt, &err),
    };
    if let Some(fold) = &args.fold {
        for app in &mut apps {
            app.fold = Some(fold.clone());
        }
    }

    request_and_render(
        client,
        streams,
        fmt,
        "start",
        Request::Start { apps },
        Some(START_DEADLINE),
        |response| match response {
            Response::Started(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Stops the sheep matching `args.selector`.
pub async fn stop(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &SelectorArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
        fmt,
        "stop",
        Request::Stop { selector },
        None,
        |response| match response {
            Response::Stopped(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Restarts the sheep matching `args.selector`.
pub async fn restart(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &SelectorArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
        fmt,
        "restart",
        Request::Restart { selector },
        None,
        |response| match response {
            Response::Restarted(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
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
pub async fn reload(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &SelectorArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
        fmt,
        "reload",
        Request::Reload { selector },
        None,
        |response| match response {
            Response::Reloading(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Deletes (stops and deregisters) the sheep matching `args.selector`.
pub async fn delete(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &SelectorArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
        fmt,
        "delete",
        Request::Delete { selector },
        None,
        |response| match response {
            Response::Deleted(ids) => Some(DeletedIds(ids)),
            _ => None,
        },
    )
    .await
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
pub async fn stock(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &StockArgs,
) -> ExitCode {
    request_and_render(
        client,
        streams,
        fmt,
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
    use shep_client::DEFAULT_DEADLINE;
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    fn start_args(target: &str) -> StartArgs {
        StartArgs {
            target: target.to_string(),
            name: None,
            fold: None,
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
    #[tokio::test]
    async fn a_target_that_matches_nothing_is_a_usage_error_naming_what_was_tried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            start(
                &client,
                &mut streams,
                Format::Table,
                &start_args("./does-not-exist"),
            )
            .await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "an unresolvable target must not reach the daemon"
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
        let path = dir.path().join("s.sock");
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
                };
                let args = SelectorArgs {
                    selector: input.into(),
                };
                let expected_body = match verb {
                    Verb::Stop => Request::Stop { selector: expected },
                    Verb::Restart => Request::Restart { selector: expected },
                    Verb::Reload => Request::Reload { selector: expected },
                    Verb::Delete => Request::Delete { selector: expected },
                };
                let _ = match verb {
                    Verb::Stop => stop(&client, &mut streams, Format::Table, &args).await,
                    Verb::Restart => restart(&client, &mut streams, Format::Table, &args).await,
                    Verb::Reload => reload(&client, &mut streams, Format::Table, &args).await,
                    Verb::Delete => delete(&client, &mut streams, Format::Table, &args).await,
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
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            stop(
                &client,
                &mut streams,
                Format::Table,
                &SelectorArgs {
                    selector: "/[/".into(),
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
        let path = dir.path().join("s.sock");
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let code = stop(
            &client,
            &mut streams,
            Format::Table,
            &SelectorArgs {
                selector: "ghost".into(),
            },
        )
        .await;
        assert_eq!(code, ExitCode::NotFound);
    }

    /// Bounded by `timeout` rather than left to run to completion: `start`
    /// returns early with `ExitCode::Usage` whenever `resolve_target` fails
    /// — before any request is built — so a regression that reintroduced
    /// an early return here would otherwise hang this test forever on
    /// `envelopes.recv()`. The reviewer measured exactly that with the
    /// fixture missing: the test ran past 60 seconds before SIGALRM killed
    /// it at 75s (exit 142), reporting a killed CI job rather than a named
    /// assertion. Also asserts `sent.body`, not only `sent.deadline_ms` —
    /// previously only the deadline was pinned, so a `start` that sent the
    /// wrong request with the right deadline would still have passed.
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
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let srv = dir.path().join("srv");
        std::fs::write(&srv, "").unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };

        let _ = start(
            &client,
            &mut streams,
            Format::Table,
            &start_args(srv.to_str().unwrap()),
        )
        .await;

        let sent = tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
            .await
            .expect("start must reach the wire; it hung instead of sending a request")
            .unwrap();
        assert_eq!(
            sent.deadline_ms,
            Some(u64::try_from(START_DEADLINE.as_millis()).unwrap())
        );
        assert_eq!(
            sent.body,
            Request::Start {
                apps: vec![AppConfig::minimal("srv", srv.to_str().unwrap())]
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
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let srv = dir.path().join("srv");
        std::fs::write(&srv, "").unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let mut args = start_args(srv.to_str().unwrap());
        args.fold = Some("backend".to_string());

        let _ = start(&client, &mut streams, Format::Table, &args).await;

        let sent = tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
            .await
            .expect("start must reach the wire with a --fold target")
            .unwrap();
        match sent.body {
            Request::Start { apps } => {
                assert_eq!(apps.len(), 1);
                assert_eq!(apps[0].fold.as_deref(), Some("backend"));
            }
            other => panic!("expected Request::Start, got {other:?}"),
        }
    }

    /// fails if the envelope carries anything but the name and the count. `stock`
    /// is the one verb here that does NOT parse a selector, and a copy-pasted
    /// `parse_selector` would turn `web` into `SelectorSpec::Name("web")` and send
    /// a frame the daemon has no arm for.
    #[tokio::test]
    async fn the_request_carries_the_app_name_and_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = stock(
            &client,
            &mut streams,
            Format::Table,
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
        let path = dir.path().join("s.sock");
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
            };
            stock(
                &client,
                &mut streams,
                Format::Table,
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
}
