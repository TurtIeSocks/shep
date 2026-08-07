//! Spawn assembly: pure functions that build [`SpawnSpec`] from app config.
//!
//! The assembler takes a validated `ResolvedApp` and produces a fully-resolved
//! [`SpawnSpec`] ready for [`ProcessRunner::spawn`](crate::runner::ProcessRunner::spawn).
//! No I/O here — all defaults, env vars, and paths are pre-resolved by the
//! daemon before assembler is called (environment comes in via `ResolvedApp`).

use std::path::PathBuf;

use shep_core::config::ResolvedApp;
use shep_core::paths::ShepPaths;

use crate::runner::SpawnSpec;

/// Finds the `count` lowest-free instance slot numbers from an existing set.
///
/// Used to allocate instance slots for clustered apps. Assumes `existing` is
/// sorted; returns a new vector of `count` distinct slots, smallest first,
/// none of which appear in `existing`.
///
/// # Examples
///
/// ```
/// use shep_daemon::assemble::instance_slots;
///
/// assert_eq!(instance_slots(&[], 3), vec![0, 1, 2]);
/// assert_eq!(instance_slots(&[0, 2], 2), vec![1, 3]);
/// ```
#[must_use]
pub fn instance_slots(existing: &[u32], count: u32) -> Vec<u32> {
    let mut result = Vec::with_capacity(count as usize);
    let mut candidate = 0u32;

    for _ in 0..count {
        while existing.contains(&candidate) || result.contains(&candidate) {
            candidate += 1;
        }
        result.push(candidate);
        candidate += 1;
    }

    result
}

/// Assembles a [`SpawnSpec`] from a validated app config and instance slot.
///
/// Resolves the program/args from the interpreter config, merges env with the
/// instance slot var, computes log file paths respecting `merge_logs`, and sets
/// the shepherd-channel flag from `wait_ready || shutdown_with_message`.
///
/// # Interpreter logic
///
/// - `interpreter = None` → runs the script directly as the program
/// - `interpreter = Some("none")` → runs the script directly (explicit override)
/// - `interpreter = Some(path)` → runs `path` with `[script, ...args]`
///
/// # Log paths
///
/// Default log paths are `logs/<name>-<instance>-out.log` and `-err.log`.
/// When `merge_logs = true`, they become `logs/<name>-out.log` and `-err.log`
/// (shared across all instances). Explicit `out_file`/`err_file` config
/// always win over defaults.
#[must_use]
pub fn assemble(app: &ResolvedApp, instance: u32, paths: &ShepPaths) -> SpawnSpec {
    let config = app.config();
    let name = config.name.clone();

    // Interpreter: resolve program and args
    let (program, args) = match &config.interpreter {
        None => {
            // Direct script execution
            (config.script.clone(), config.args.clone())
        }
        Some(interp) if interp == "none" => {
            // Explicit "none" means direct script execution
            (config.script.clone(), config.args.clone())
        }
        Some(interp) => {
            // Interpreter with script as first arg
            let mut interp_args = vec![config.script.clone()];
            interp_args.extend(config.args.iter().cloned());
            (interp.clone(), interp_args)
        }
    };

    // Environment: merge app env with instance slot var
    let mut env = config.env.clone();
    let slot_var = config.increment_var.as_deref().unwrap_or("SHEP_INSTANCE");
    env.insert(slot_var.to_string(), instance.to_string());

    // Working directory
    let cwd = config.cwd.as_ref().map(PathBuf::from);

    // Log paths: instance-suffixed by default, or merged
    let log_stem = if config.merge_logs {
        format!("{}-", name)
    } else {
        format!("{}-{}-", name, instance)
    };

    let out_file = if let Some(ref explicit) = config.out_file {
        PathBuf::from(explicit)
    } else {
        paths.logs.join(format!("{}out.log", log_stem))
    };

    let err_file = if let Some(ref explicit) = config.err_file {
        PathBuf::from(explicit)
    } else {
        paths.logs.join(format!("{}err.log", log_stem))
    };

    // Shepherd channel: enabled if wait_ready or shutdown_with_message
    let channel = config.wait_ready || config.shutdown_with_message;

    SpawnSpec {
        name,
        program,
        args,
        cwd,
        env,
        out_file,
        err_file,
        channel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::config::{AppConfig, normalize};

    fn test_paths() -> ShepPaths {
        ShepPaths {
            home: PathBuf::from("/home/rin/.shep"),
            daemon_config: PathBuf::from("/home/rin/.shep/shep.toml"),
            snapshot: PathBuf::from("/home/rin/.shep/flock.json"),
            logs: PathBuf::from("/home/rin/.shep/logs"),
            pids: PathBuf::from("/home/rin/.shep/pids"),
            run: PathBuf::from("/home/rin/.shep/run"),
            socket: PathBuf::from("/home/rin/.shep/run/shep.sock"),
            barks: PathBuf::from("/home/rin/.shep/barks.jsonl"),
        }
    }

    #[test]
    fn slots_empty_request() {
        let result = instance_slots(&[], 3);
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn slots_skip_occupied() {
        let result = instance_slots(&[0, 2], 2);
        assert_eq!(result, vec![1, 3]);
    }

    #[test]
    fn env_adds_shep_instance() {
        let app_config = AppConfig {
            name: "web".to_string(),
            script: "/usr/bin/python3".to_string(),
            args: vec!["app.py".to_string()],
            interpreter: Some("none".to_string()),
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 1, &paths);

        assert!(spec.env.contains_key("SHEP_INSTANCE"));
        assert_eq!(spec.env.get("SHEP_INSTANCE").map(|s| s.as_str()), Some("1"));
    }

    #[test]
    fn env_custom_increment_var() {
        let mut app_config = AppConfig {
            name: "worker".to_string(),
            script: "bin/worker".to_string(),
            args: vec![],
            ..Default::default()
        };
        app_config.increment_var = Some("WORKER_ID".to_string());

        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 5, &paths);

        assert!(!spec.env.contains_key("SHEP_INSTANCE"));
        assert_eq!(spec.env.get("WORKER_ID").map(|s| s.as_str()), Some("5"));
    }

    #[test]
    fn interpreter_none_runs_script_directly() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "/opt/bin/server".to_string(),
            args: vec!["--port".to_string(), "8080".to_string()],
            interpreter: None,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths);

        assert_eq!(spec.program, "/opt/bin/server");
        assert_eq!(spec.args, vec!["--port", "8080"]);
    }

    #[test]
    fn interpreter_explicit_none_runs_script_directly() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "server.py".to_string(),
            args: vec!["--verbose".to_string()],
            interpreter: Some("none".to_string()),
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths);

        assert_eq!(spec.program, "server.py");
        assert_eq!(spec.args, vec!["--verbose"]);
    }

    #[test]
    fn interpreter_path_prepends_script() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app.js".to_string(),
            args: vec!["--debug".to_string(), "true".to_string()],
            interpreter: Some("node".to_string()),
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths);

        assert_eq!(spec.program, "node");
        assert_eq!(spec.args, vec!["app.js", "--debug", "true"]);
    }

    #[test]
    fn merge_logs_false_uses_instance_suffix() {
        let app_config = AppConfig {
            name: "web".to_string(),
            script: "app".to_string(),
            args: vec![],
            merge_logs: false,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 2, &paths);

        assert_eq!(
            spec.out_file,
            PathBuf::from("/home/rin/.shep/logs/web-2-out.log")
        );
        assert_eq!(
            spec.err_file,
            PathBuf::from("/home/rin/.shep/logs/web-2-err.log")
        );
    }

    #[test]
    fn merge_logs_true_omits_instance_suffix() {
        let app_config = AppConfig {
            name: "api".to_string(),
            script: "api".to_string(),
            args: vec![],
            merge_logs: true,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 1, &paths);

        assert_eq!(
            spec.out_file,
            PathBuf::from("/home/rin/.shep/logs/api-out.log")
        );
        assert_eq!(
            spec.err_file,
            PathBuf::from("/home/rin/.shep/logs/api-err.log")
        );
    }

    #[test]
    fn explicit_out_file_wins() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app".to_string(),
            args: vec![],
            merge_logs: false,
            out_file: Some("/var/log/myapp.log".to_string()),
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths);

        assert_eq!(spec.out_file, PathBuf::from("/var/log/myapp.log"));
        // err_file still uses default
        assert_eq!(
            spec.err_file,
            PathBuf::from("/home/rin/.shep/logs/app-0-err.log")
        );
    }

    #[test]
    fn channel_enabled_by_wait_ready() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app".to_string(),
            args: vec![],
            wait_ready: true,
            shutdown_with_message: false,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths);

        assert!(spec.channel);
    }

    #[test]
    fn channel_enabled_by_shutdown_with_message() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app".to_string(),
            args: vec![],
            wait_ready: false,
            shutdown_with_message: true,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths);

        assert!(spec.channel);
    }
}
