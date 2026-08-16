//! `shep runtime`: resolves a Flockfile, then either hands off to
//! `commands::foreground`'s engine with `tidy_up: false` — decision 12's own
//! table: a container going away has no reason to pay for a delete on the
//! way out, and leaves the muster roll exactly as `runtime` found it — or,
//! at PID 1, splits into `commands::reap`'s init loop first. Read decision
//! 14 in Phase 15's plan before touching the split itself.

use shep_core::config::discover;
use shep_core::paths::ShepPaths;

use crate::cli::{Format, RuntimeArgs};
use crate::commands::foreground::{self, ForegroundOptions};
use crate::commands::lifecycle::{resolve_target, target_exit_code};
use crate::commands::reap;
use crate::exit::ExitCode;
use crate::output::{Streams, emit_error};

/// The ten filenames [`discover`] looks for, in the order it looks. Named
/// here rather than imported: `shep_core::config::flockfile::DISCOVERY_ORDER`
/// is private, and this is the one caller that needs to name the list, in an
/// error message printed when discovery finds nothing.
const DISCOVERY_NAMES: &str = "Flockfile.toml, Flockfile.yaml, Flockfile.yml, Flockfile.json, \
     Flockfile.json5, flockfile.toml, flockfile.yaml, flockfile.yml, flockfile.json, \
     flockfile.json5";

/// Runs `shep runtime`.
///
/// `args.target`, when given, resolves through the same
/// [`resolve_target`] `start` uses — a script or a Flockfile, by extension.
/// With no target, [`discover`] looks in the current directory for one of
/// the ten conventional names; finding none is [`ExitCode::Usage`] (2)
/// naming them, same as an unresolvable `start` target.
///
/// Always dispatches with `tidy_up: false` — see this module's own doc.
/// `quiet` is `cli::GlobalArgs::quiet`, threaded straight through to the
/// engine's own `bleats` narration, same as every other verb that follows.
pub async fn runtime(
    streams: &mut Streams<'_>,
    fmt: Format,
    quiet: bool,
    paths: ShepPaths,
    args: &RuntimeArgs,
) -> ExitCode {
    let target = match &args.target {
        Some(target) => target.clone(),
        None => match discovered_target(streams, fmt) {
            Ok(target) => target,
            Err(code) => return code,
        },
    };

    let apps = match resolve_target(&target, None, &[], false) {
        Ok(apps) => apps,
        Err(err) => {
            let code = target_exit_code(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            return code;
        }
    };

    let options = ForegroundOptions {
        paths,
        apps,
        tidy_up: false,
    };

    // Read once, right here, and nowhere else in the crate: it exists only
    // to make the PID-1 split reachable from a test harness, following the
    // panic probe's shape in `lib.rs`'s `run_argv`. See decision 14.
    let forced = std::env::var_os("SHEP_FORCE_INIT").is_some();
    if !reap::should_split(std::process::id(), args.supervise, forced) {
        return foreground::run(streams, fmt, quiet, options).await;
    }
    // `Infallible` is an ordinary uninhabited enum, not the never type `!`,
    // so it does not coerce to `ExitCode` as a bare tail expression — the
    // empty match is what performs that coercion. `run_init` never returns.
    match reap::run_init().await {}
}

/// Discovers a Flockfile in the current directory, or reports
/// [`ExitCode::Usage`] naming the ten filenames [`discover`] looked for.
fn discovered_target(streams: &mut Streams<'_>, fmt: Format) -> Result<String, ExitCode> {
    let cwd = std::env::current_dir().map_err(|err| {
        let message = format!("could not read the current directory: {err}");
        let _ = emit_error(&mut *streams.err, fmt, ExitCode::Usage.code_str(), &message);
        ExitCode::Usage
    })?;
    match discover(&cwd) {
        Some(path) => Ok(path.to_string_lossy().into_owned()),
        None => {
            let message = format!(
                "no Flockfile found in {} (looked for {DISCOVERY_NAMES})",
                cwd.display()
            );
            let _ = emit_error(&mut *streams.err, fmt, ExitCode::Usage.code_str(), &message);
            Err(ExitCode::Usage)
        }
    }
}
