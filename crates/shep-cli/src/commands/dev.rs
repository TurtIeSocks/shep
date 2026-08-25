//! `shep dev`: the isolated, throwaway sibling of `shep runtime`. Resolves a
//! target through the same `lifecycle::resolve_target` `start` uses, forces
//! `watch = true` onto every app it finds, and hands the result to
//! `commands::foreground`'s engine with `tidy_up: true` — a session that
//! stops and deletes its whole flock on the way out, on a home
//! `--home`/`$SHEP_HOME` cannot reach. Read decision 15 in Phase 15's plan
//! before touching this file.

use std::path::{Path, PathBuf};

use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;

use crate::cli::DevArgs;
use crate::commands::foreground::{self, ForegroundOptions};
use crate::commands::lifecycle::{resolve_target, target_exit_code};
use crate::commands::runtime::discovered_target;
use crate::exit::ExitCode;
use crate::output::{Streams, emit_notice};

/// Where a dev flock lives: `$SHEP_DEV_HOME`, else `~/.shep-dev`.
///
/// **`--home` and `$SHEP_HOME` are ignored** — [`dev`] prints a stderr
/// notice when `--home` was given, and this function never reads it.
/// Isolation is the whole feature spec §9 names for this verb, and
/// `cli::GlobalArgs::home` carries `env = "SHEP_HOME"` — so an operator who
/// exports it for their real flock would otherwise get a `shep dev` that
/// shares it, and `dev`'s forced `watch = true` would be written onto their
/// production apps.
///
/// `$SHEP_DEV_HOME` is not in the spec. It exists because the e2e tier
/// needs a knob that a developer's own environment cannot collide with;
/// without it, `cargo test` writes into whoever's real `~/.shep-dev`.
///
/// Reuses [`ShepPaths::resolve`] rather than re-deriving its per-field
/// layout by hand: the home this function computes is injected as that
/// resolver's own answer for `SHEP_HOME`, so `daemon_config`/`snapshot`/
/// `logs`/... stay in lock-step with whatever `ShepPaths::resolve` derives
/// for every other verb, with no second copy of that arithmetic to drift
/// out of sync.
fn dev_home(env: &impl Fn(&str) -> Option<String>, home_dir: &Path) -> ShepPaths {
    let home = env("SHEP_DEV_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.join(".shep-dev"));
    let inject = |key: &str| (key == "SHEP_HOME").then(|| home.to_string_lossy().into_owned());
    ShepPaths::resolve(&inject, home_dir)
}

/// Sets `watch = true` on every app, leaving everything else untouched —
/// applied in place rather than by rebuilding each [`AppConfig`], which
/// would silently drop any field this function does not know to copy.
fn force_watch(apps: &mut [AppConfig]) {
    for app in apps {
        app.watch = true;
    }
}

/// Fills a missing `cwd` with the directory containing that app's own
/// `script` — the daemon refuses `watch = true` with no `cwd` to arm
/// (`shep_core::config::normalize::NormalizeError::WatchWithoutCwd`), and
/// [`force_watch`] has just turned `watch` on for every app here. A
/// Flockfile's own `cwd` is left untouched; this only fills the gap a bare
/// script target leaves, since [`resolve_target`] never sets one for that
/// form. Falls back to this process's own current directory when the
/// script's parent cannot be resolved (a bare filename with no directory
/// component) — the closest available answer to "the directory `shep dev`
/// was run from," and still a real directory rather than nothing to watch.
fn default_watch_cwd(apps: &mut [AppConfig]) {
    for app in apps {
        if app.cwd.is_some() {
            continue;
        }
        let parent = Path::new(&app.script)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        app.cwd = parent.map(|p| p.to_string_lossy().into_owned());
    }
}

/// Runs `shep dev`.
///
/// `args.target`, when given, resolves through the same [`resolve_target`]
/// `start` uses — a script or a Flockfile, by extension. With no target,
/// [`discovered_target`] (shared with `shep runtime`, so the two verbs
/// cannot disagree about what "no target" discovers) looks in the current
/// directory for one of the ten conventional names.
///
/// `home_given` is `cli::GlobalArgs::home.is_some()` — true whenever
/// `--home` or its aliased `$SHEP_HOME` named anything, in which case this
/// prints a notice that `dev` ignores it before going on to compute
/// [`dev_home`] instead. `quiet` is `cli::GlobalArgs::quiet`, threaded
/// straight through to the engine's own `bleats` narration, same as
/// `runtime`.
pub async fn dev(
    streams: &mut Streams<'_>,
    quiet: bool,
    home_given: bool,
    args: &DevArgs,
) -> ExitCode {
    if home_given {
        let _ = emit_notice(
            &mut *streams.err,
            streams.fmt,
            "home_ignored",
            "shep dev ignores --home/$SHEP_HOME; isolation is the whole feature — set \
             $SHEP_DEV_HOME instead",
        );
    }

    let target = match &args.target {
        Some(target) => target.clone(),
        None => match discovered_target(streams) {
            Ok(target) => target,
            Err(code) => return code,
        },
    };

    let mut apps = match resolve_target(&target, args.name.as_deref(), &[], false) {
        Ok(apps) => apps,
        Err(err) => {
            let code = target_exit_code(&err);
            return streams.fail(code, &err.to_string());
        }
    };

    force_watch(&mut apps);
    default_watch_cwd(&mut apps);
    let names = apps
        .iter()
        .map(|app| app.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let _ = emit_notice(
        &mut *streams.err,
        streams.fmt,
        "watch_forced",
        &format!(
            "shep dev: forcing watch on {names} — each app's own `watch` setting is ignored here"
        ),
    );

    // Only `$SHEP_DEV_HOME` is read — `$SHEP_HOME` is deliberately excluded,
    // the same isolation `dev_home`'s own doc argues for.
    let env = |key: &str| {
        if key == "SHEP_DEV_HOME" {
            std::env::var("SHEP_DEV_HOME").ok()
        } else {
            None
        }
    };
    let home_dir = match (std::env::var_os("HOME"), env("SHEP_DEV_HOME")) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(_)) => PathBuf::new(),
        (None, None) => {
            let message = "neither $SHEP_DEV_HOME nor $HOME resolves a root directory for shep dev";
            return streams.fail(ExitCode::Usage, message);
        }
    };
    let paths = dev_home(&env, &home_dir);

    let options = ForegroundOptions {
        paths,
        apps,
        tidy_up: true,
    };
    foreground::run(streams, quiet, options).await
}

#[cfg(test)]
mod tests {
    use shep_core::values::MemSize;

    use super::*;

    /// fails if dev starts honouring $SHEP_HOME, which would put a forced
    /// `watch = true` onto the operator's real flock.
    #[test]
    fn the_dev_home_ignores_shep_home_and_prefers_its_own_variable() {
        let env = |key: &str| match key {
            "SHEP_HOME" => Some("/srv/production".to_string()),
            _ => None,
        };
        let paths = dev_home(&env, Path::new("/home/rin"));
        assert_eq!(paths.home, Path::new("/home/rin/.shep-dev"));

        let env = |key: &str| match key {
            "SHEP_HOME" => Some("/srv/production".to_string()),
            "SHEP_DEV_HOME" => Some("/tmp/t1".to_string()),
            _ => None,
        };
        assert_eq!(
            dev_home(&env, Path::new("/home/rin")).home,
            Path::new("/tmp/t1")
        );
    }

    /// fails if watch is not forced, or if it is forced by rebuilding the
    /// AppConfig and dropping the rest of the Flockfile in the process.
    #[test]
    fn every_app_gets_watch_and_keeps_everything_else() {
        let mut apps = vec![AppConfig::minimal("web", "./server.js")];
        apps[0].watch = false;
        apps[0].max_memory = Some(MemSize::from_bytes(1024));
        force_watch(&mut apps);
        assert!(apps[0].watch);
        assert_eq!(apps[0].max_memory, Some(MemSize::from_bytes(1024)));
        assert_eq!(apps[0].name, "web");
    }

    /// fails if a script-form target's missing `cwd` is not filled — the
    /// exact gap `force_watch` opens for `resolve_target`'s bare-script
    /// branch, which never sets one, and the daemon rejects `watch = true`
    /// without.
    #[test]
    fn a_missing_cwd_defaults_to_the_scripts_own_directory() {
        let mut apps = vec![AppConfig::minimal("web", "/srv/app/server.js")];
        default_watch_cwd(&mut apps);
        assert_eq!(apps[0].cwd.as_deref(), Some("/srv/app"));
    }

    /// fails if a Flockfile's own `cwd` is overwritten rather than left
    /// alone.
    #[test]
    fn an_explicit_cwd_is_left_untouched() {
        let mut apps = vec![AppConfig::minimal("web", "./server.js")];
        apps[0].cwd = Some("/srv/explicit".to_string());
        default_watch_cwd(&mut apps);
        assert_eq!(apps[0].cwd.as_deref(), Some("/srv/explicit"));
    }
}
