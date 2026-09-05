//! `shep runtime`: resolves a Flockfile, then hands off to
//! `commands::foreground`'s engine with `tidy_up: false`, which leaves the
//! muster roll exactly as `runtime` found it. At PID 1 it splits into
//! `commands::reap`'s init loop first.

use std::path::PathBuf;

use shep_core::config::discover;
use shep_core::paths::ShepPaths;

use crate::cli::RuntimeArgs;
use crate::commands::foreground::{self, ForegroundOptions};
use crate::commands::lifecycle::{resolve_target, target_exit_code};
#[cfg(unix)]
use crate::commands::reap;
use crate::exit::ExitCode;
use crate::output::Streams;

/// The ten filenames [`discover`] looks for, in the order it looks
///
/// Spelled out because `shep_core::config::flockfile::DISCOVERY_ORDER` is
/// private.
const DISCOVERY_NAMES: &str = "Flockfile.toml, Flockfile.yaml, Flockfile.yml, Flockfile.json, \
     Flockfile.json5, flockfile.toml, flockfile.yaml, flockfile.yml, flockfile.json, \
     flockfile.json5";

/// Runs `shep runtime`
///
/// `args.target` resolves through the same [`resolve_target`] `start` uses.
/// With no target, [`discover`] looks in the current directory; finding
/// nothing is [`ExitCode::Usage`] naming the ten names.
pub async fn runtime(
    streams: &mut Streams<'_>,
    quiet: bool,
    paths: ShepPaths,
    args: &RuntimeArgs,
) -> ExitCode {
    let target = match &args.target {
        Some(target) => target.clone(),
        None => match discovered_target(streams) {
            Ok(target) => target,
            Err(code) => return code,
        },
    };

    let apps = match resolve_target(&target, None, &[], false) {
        Ok(apps) => apps,
        Err(err) => {
            let code = target_exit_code(&err);
            return streams.fail(code, &err.to_string());
        }
    };

    let options = ForegroundOptions {
        paths,
        apps,
        tidy_up: false,
    };

    // `SHEP_FORCE_INIT` is read here and nowhere else: it exists to make the
    // split reachable from a test harness. Unix only, because Windows has no
    // zombie state and no reparent-to-init rule, so there is nothing to reap.
    #[cfg(windows)]
    return foreground::run(streams, quiet, options).await;

    #[cfg(unix)]
    let forced = std::env::var_os("SHEP_FORCE_INIT").is_some();
    #[cfg(unix)]
    if !reap::should_split(std::process::id(), args.supervise, forced) {
        return foreground::run(streams, quiet, options).await;
    }
    // `Infallible` does not coerce to `ExitCode` as a bare tail expression,
    // so the empty match performs it. `run_init` never returns.
    #[cfg(unix)]
    match reap::run_init().await {}
}

/// Discovers a Flockfile in the current directory, or reports
/// [`ExitCode::Usage`] naming the ten filenames [`discover`] looked for.
///
/// Shared with `commands::dev`, so `shep dev ./` and `shep runtime ./`
/// cannot disagree about what "no target" discovers.
pub(crate) fn discovered_target(streams: &mut Streams<'_>) -> Result<String, ExitCode> {
    let cwd = get_cwd(streams)?;
    match discover(&cwd) {
        Some(path) => Ok(path.to_string_lossy().into_owned()),
        None => {
            let message = format!(
                "no Flockfile found in {} (looked for {DISCOVERY_NAMES})",
                cwd.display()
            );
            Err(streams.fail(ExitCode::Usage, &message))
        }
    }
}

pub(crate) fn get_cwd(streams: &mut Streams<'_>) -> Result<PathBuf, ExitCode> {
    std::env::current_dir().map_err(|err| {
        let message = format!("could not read the current directory: {err}");
        streams.fail(ExitCode::Usage, &message)
    })
}
