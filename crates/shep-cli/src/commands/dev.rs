//! `shep dev`: the isolated, throwaway sibling of `shep runtime`. Forces
//! `watch = true` onto every app it resolves and hands the result to
//! `commands::foreground`'s engine with `tidy_up: true`, on a home
//! `--home`/`$SHEP_HOME` cannot reach.

use std::path::{Path, PathBuf};

use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;

use crate::cli::DevArgs;
use crate::commands::foreground::{self, ForegroundOptions};
use crate::commands::lifecycle::{resolve_target, target_exit_code};
use crate::commands::runtime::discovered_target;
use crate::exit::ExitCode;
use crate::output::Streams;

/// Where a dev flock lives: `$SHEP_DEV_HOME`, else `~/.shep-dev`.
///
/// `--home` and `$SHEP_HOME` are ignored: sharing a real flock's home would
/// land the forced `watch = true` on production apps.
///
/// The home is injected as [`ShepPaths::resolve`]'s own answer for
/// `SHEP_HOME`, so every derived path matches the other verbs.
fn dev_home(env: &impl Fn(&str) -> Option<String>, home_dir: &Path) -> ShepPaths {
    let home = env("SHEP_DEV_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.join(".shep-dev"));
    let inject = |key: &str| (key == "SHEP_HOME").then(|| home.to_string_lossy().into_owned());
    ShepPaths::resolve(&inject, home_dir)
}

/// Sets `watch = true` on every app, in place: rebuilding each [`AppConfig`]
/// would silently drop any field this function does not know to copy.
fn force_watch(apps: &mut [AppConfig]) {
    for app in apps {
        app.watch = true;
    }
}

/// Fills a missing `cwd` with the directory containing that app's own
/// `script`: the daemon refuses `watch = true` with no `cwd` to arm, and
/// [`force_watch`] has just turned `watch` on for every app. Only a bare
/// script target leaves that gap. A script with no directory component falls
/// back to this process's current directory.
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
/// With no `args.target`, [`discovered_target`] looks in the current
/// directory for a conventional name.
///
/// `home_given` is true whenever `--home` or its aliased `$SHEP_HOME` named
/// anything; this then notices that `dev` uses [`dev_home`] instead.
pub async fn dev(
    streams: &mut Streams<'_>,
    quiet: bool,
    home_given: bool,
    args: &DevArgs,
) -> ExitCode {
    if home_given {
        streams.aside(
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
    streams.aside(
        "watch_forced",
        &format!(
            "shep dev: forcing watch on {names} — each app's own `watch` setting is ignored here"
        ),
    );

    // Only `$SHEP_DEV_HOME` is read; a real flock's home must not leak in.
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

    #[test]
    fn the_dev_home_ignores_shep_home_and_prefers_its_own_variable() {
        let env = |key: &str| match key {
            "SHEP_HOME" => Some("/srv/production".to_string()),
            _ => None,
        };
        let paths = dev_home(&env, Path::new("/home/ada"));
        assert_eq!(paths.home, Path::new("/home/ada/.shep-dev"));

        let env = |key: &str| match key {
            "SHEP_HOME" => Some("/srv/production".to_string()),
            "SHEP_DEV_HOME" => Some("/tmp/t1".to_string()),
            _ => None,
        };
        assert_eq!(
            dev_home(&env, Path::new("/home/ada")).home,
            Path::new("/tmp/t1")
        );
    }

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

    #[test]
    fn a_missing_cwd_defaults_to_the_scripts_own_directory() {
        let mut apps = vec![AppConfig::minimal("web", "/srv/app/server.js")];
        default_watch_cwd(&mut apps);
        assert_eq!(apps[0].cwd.as_deref(), Some("/srv/app"));
    }

    #[test]
    fn an_explicit_cwd_is_left_untouched() {
        let mut apps = vec![AppConfig::minimal("web", "./server.js")];
        apps[0].cwd = Some("/srv/explicit".to_string());
        default_watch_cwd(&mut apps);
        assert_eq!(apps[0].cwd.as_deref(), Some("/srv/explicit"));
    }
}
