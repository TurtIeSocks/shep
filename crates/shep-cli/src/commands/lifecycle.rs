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
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::{SelectorArgs, StartArgs, StockArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{DeletedIds, FlockRows, Render, Streams, emit, write_outcome};

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

/// Rewrites a canonicalized path into one node's `require` can resolve.
///
/// On Windows `std::fs::canonicalize` returns an extended-length path, so a
/// flockfile at `C:\tmp\flock.js` comes back as `\\?\C:\tmp\flock.js`. Node's
/// module resolver does not understand the `\\?\` prefix: it reads the leading
/// `\\` as a UNC share, walks off the front of the path, and fails with
/// ``EISDIR: illegal operation on a directory, lstat 'C:'``. That error names
/// `C:` and never the flockfile, which is what made it read for two rounds of
/// CI like an argument-quoting fault rather than a path-shape one.
///
/// Only the `\\?\C:\` form is unwrapped, because that is the only shape
/// `canonicalize` produces for a local file. A verbatim UNC path
/// (`\\?\UNC\server\share`) would need the same treatment and does not get it:
/// no Windows host here can mount a share to test the branch against, and an
/// unexercised guess is worth less than a documented gap.
#[cfg(windows)]
fn node_readable_path(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return std::borrow::Cow::Borrowed(path);
    };
    let Prefix::VerbatimDisk(letter) = prefix.kind() else {
        return std::borrow::Cow::Borrowed(path);
    };

    let mut rebuilt = std::path::PathBuf::from(format!("{}:\\", char::from(letter)));
    rebuilt.extend(components.filter(|part| !matches!(part, Component::RootDir)));
    std::borrow::Cow::Owned(rebuilt)
}

/// Passes the path through: only Windows' `canonicalize` prefixes its output.
///
/// See the Windows sibling for what this exists to undo.
#[cfg(not(windows))]
fn node_readable_path(path: &Path) -> std::borrow::Cow<'_, Path> {
    std::borrow::Cow::Borrowed(path)
}

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
/// **There is no timeout.** A module that never returns — one that starts a
/// server at require time — hangs here. The process is in the foreground and
/// interruptible; adding a bound means a reaper thread in a crate that
/// forbids unsafe code. Recorded in `docs/specs/deferred.md`.
///
/// The `node_missing` sentence IS pinned, as of Phase 17, by `cli_e2e`'s
/// `a_js_flockfile_without_node_says_so_and_says_what_to_do`. This doc used
/// to say it could not be, on the grounds that producing it needs a `PATH`
/// without node and `std::env::set_var` is `unsafe` in edition 2024 inside a
/// crate that forbids unsafe. That holds for a unit test, which would have to
/// mutate its own process; the e2e tier runs shep as a subprocess, and
/// `Command::env` sets the child's environment alone.
///
/// `docs/migration.md` still quotes this sentence for an operator without
/// node installed, and that quote is still kept in step by hand, so update it
/// in the same commit if this `format!` changes.
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
    // The bridge is written to a file and run as `node loader.js` from that
    // file's own directory, so the only argument node ever receives is a
    // bare relative filename: no path, no quotes, no shell metacharacters.
    // See [`JS_BRIDGE_SCRIPT`] for what this is defending against.
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
    let output = std::process::Command::new("node")
        .arg(JS_BRIDGE_FILE)
        .current_dir(scratch.path())
        .env(
            "SHEP_FLOCKFILE_PATH",
            node_readable_path(&absolute).as_os_str(),
        )
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
                let json = evaluate_js_flockfile(path)?;
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
                    .map(|abs| abs.to_string_lossy().into_owned())
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
async fn resume(
    client: &Client,
    streams: &mut Streams<'_>,
    existing: &shep_core::protocol::ProcessInfo,
    started: &mut Vec<shep_core::protocol::ProcessInfo>,
) -> ExitCode {
    use shep_core::status::ProcStatus;

    if matches!(
        existing.status,
        ProcStatus::Online | ProcStatus::Starting | ProcStatus::Stopping
    ) {
        let message = format!(
            "{} is already {}; `shep restart {}` replaces it.",
            existing.name, existing.status, existing.name
        );
        streams.aside("start", &message);
        return ExitCode::Success;
    }
    let (procs, failure) = request_each(
        client,
        streams,
        &[SelectorSpec::Name(existing.name.clone())],
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
        let message = format!(
            "{} could not be started; see `shep bleats {}` or its log files for why",
            existing.name, existing.name
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
/// the evidence. Rin's call was to default the cwd rather than resolve the
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
/// `apps`, in the precedence Rin fixed for task 47: shep.toml, then a
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
        if !started.is_empty() {
            let wrote = write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                "start",
                FlockRows(started),
                streams.style,
            ));
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
    if !started.is_empty() {
        let wrote = write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "start",
            FlockRows(started),
            streams.style,
        ));
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
    // A target naming a sheep the flock already has is that sheep, not a new
    // one. Checked before the path arms below, or `shep start zeus-auth` on a
    // registered-but-stopped sheep is read as a filename, fails to resolve,
    // and reports that nothing by that name is on disk -- while the sheep sits
    // in the listing.
    //
    // Only when the target is not itself a readable target: an explicit path
    // or Flockfile still means what it says, so `shep start ./server.js` is
    // never diverted by a coincidence of names.
    let mut listing: Option<Vec<shep_core::protocol::ProcessInfo>> = None;
    if let Some(name) = target {
        let is_path_like = name == "-" || args.flockfile || Path::new(name).exists();
        if !is_path_like {
            let flock = flock_now(client).await;
            if let Some(existing) = flock.iter().find(|info| info.name == name) {
                return resume(client, streams, existing, started).await;
            }
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

    let apps = match resolve_target(target, args.name.as_deref(), &stdin, args.flockfile) {
        Ok(apps) => apps,
        Err(err) => return fail_target(streams, &err),
    };
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
    let mut resumed = Vec::new();
    let mut fresh = Vec::new();
    for app in apps {
        match flock.iter().find(|info| info.name == app.name) {
            Some(existing) => resumed.push(existing.clone()),
            None => fresh.push(app),
        }
    }
    for existing in &resumed {
        let code = resume(client, streams, existing, started).await;
        if code != ExitCode::Success {
            return code;
        }
    }
    if fresh.is_empty() {
        return ExitCode::Success;
    }
    let mut apps = fresh;

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
    if !procs.is_empty() {
        let wrote = write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "stop",
            FlockRows(procs),
            streams.style,
        ));
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
    if !procs.is_empty() && failed.is_empty() {
        let wrote = write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "restart",
            FlockRows(procs),
            streams.style,
        ));
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
    if !procs.is_empty() {
        let wrote = write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "reload",
            FlockRows(procs),
            streams.style,
        ));
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
    if !ids.is_empty() {
        let wrote = write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "delete",
            DeletedIds(ids),
            streams.style,
        ));
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
    /// Pins the prefix strip directly, because the end-to-end `.js` cases
    /// cannot prove it. Node resolves a `\\?\` path correctly on some
    /// versions and not others, so those cases passed on this machine while
    /// failing on the CI runner, both before the strip existed and after.
    /// Asserting on the rewritten path is the part that holds either way.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_prefix_is_stripped_before_node_sees_the_path() {
        let rewritten = super::node_readable_path(std::path::Path::new(r"\\?\C:\tmp\flock.js"));
        assert_eq!(
            rewritten.as_os_str(),
            std::ffi::OsStr::new(r"C:\tmp\flock.js"),
            "node reads the leading `\\\\` as a UNC share and lstats `C:`, so \
             the verbatim prefix must not reach it"
        );

        let plain = std::path::Path::new(r"C:\tmp\flock.js");
        assert_eq!(
            super::node_readable_path(plain).as_os_str(),
            plain.as_os_str(),
            "a path with no verbatim prefix must pass through untouched"
        );
    }

    /// Guards the assumption the strip rests on: that `canonicalize` really
    /// does hand back a prefixed path here, and that the rewrite clears it.
    /// If a future Windows or std stops adding the prefix, this stays green
    /// and the strip becomes a no-op rather than a wrong answer.
    #[cfg(windows)]
    #[test]
    fn a_real_canonicalized_flockfile_comes_back_free_of_the_prefix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let flockfile = dir.path().join("flock.js");
        std::fs::write(&flockfile, "module.exports = { apps: [] };").expect("write flockfile");

        let canonical = std::fs::canonicalize(&flockfile).expect("canonicalize");
        let rewritten = super::node_readable_path(&canonical);
        let shown = rewritten.display().to_string();

        assert!(
            !shown.starts_with(r"\\?\"),
            "the path handed to node still carries a verbatim prefix: {shown}"
        );
        assert!(
            std::path::Path::new(&shown).is_file(),
            "stripping the prefix must not break the path: {shown}"
        );
    }

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
    /// crosses. Rin hit this from `~`: `shep start ./GitHub/zeus/...` where
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

        let expected = std::fs::canonicalize(dir.path()).unwrap();
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
        // One request, and it is the flock lookup: a target is a sheep's name
        // before it is a filename, so `start` has to ask before it can say
        // the target resolves to nothing. What must never reach the daemon is
        // a `Start` carrying an app built from an unresolvable target.
        let asked = envelopes
            .try_recv()
            .expect("the flock is consulted before the target is read as a path");
        assert_eq!(asked.body, Request::ListFlock);
        assert!(
            envelopes.try_recv().is_err(),
            "and nothing else: an unresolvable target must not become a Start"
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
}
