//! Spawn assembly: pure functions that build [`SpawnSpec`] from app config.
//!
//! The assembler takes a validated `ResolvedApp` and produces a fully-resolved
//! [`SpawnSpec`] ready for [`ProcessRunner::spawn`](crate::runner::ProcessRunner::spawn).
//! No I/O here — all defaults, env vars, and paths are pre-resolved by the
//! daemon before assembler is called (environment comes in via `ResolvedApp`).
//!
//! Public for its two out-of-crate readers and nothing else: `tests/real_runner.rs`
//! calls [`assemble`] to build a spec it then spawns for real, and
//! [`instance_slots`]'s doc example is compiled as its own crate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use shep_core::config::ResolvedApp;
use shep_core::paths::ShepPaths;

use crate::privilege::Credentials;
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

/// The env every spawned child starts from, before the app's own `env` map
/// is folded on top (app config always wins on conflict).
///
/// `tokio_runner.rs` calls `env_clear()` then `envs(&spec.env)` — the child
/// sees exactly this map and nothing else. Without a `PATH` in it, a bare
/// program/interpreter name (anything with no `/`: `node`, `python3`, `sh`,
/// a PATH-relative script) can never be found by exec; this is reading the
/// DAEMON'S OWN env once (not a file, not the child's), so it stays a pure
/// function of process state, not a filesystem/network IO the module doc's
/// "no I/O" note is warning about.
fn base_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    // A PRESENT-but-EMPTY PATH ("PATH=" in the daemon's own env — a
    // misconfigured launcher, `env -i PATH= shep-daemon`, ...) is treated
    // the same as an ABSENT one: `Ok("")` would otherwise slip through
    // `unwrap_or_else` untouched (that only catches the `Err` case) and
    // reproduce this exact task's ENOENT bug, since an empty PATH resolves
    // a bare program against the current directory, not a real search.
    let path = std::env::var("PATH")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_string());
    env.insert("PATH".to_string(), path);
    for key in ["HOME", "USER", "LANG", "TZ"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    env
}

/// Assembles a [`SpawnSpec`] from a validated app config and instance slot.
///
/// Resolves the program/args from the interpreter config, merges env with the
/// instance slot var, computes log file paths respecting `merge_logs`, and sets
/// the shepherd-channel flag from `channel || wait_ready || shutdown_with_message`.
///
/// `credentials` is resolved by the caller (passwd/group lookups are real
/// I/O, so they stay out of this otherwise-pure function — see
/// `crate::privilege::resolve`) and threaded straight onto the spec.
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
///
/// # Stdin
///
/// `SpawnSpec::stdin` carries `config.stdin` straight through. Nothing else
/// turns it on: unlike `channel`, which `wait_ready` and
/// `shutdown_with_message` both imply, no other flag needs fd 0 — so a sheep
/// gets a piped stdin only when its own config asks for one.
#[must_use]
pub fn assemble(
    app: &ResolvedApp,
    instance: u32,
    paths: &ShepPaths,
    credentials: Option<Credentials>,
) -> SpawnSpec {
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

    // Environment: base env FIRST (PATH + a small inherited allowlist), then
    // the app's own env on top: env_clear() + envs(&spec.env) in
    // tokio_runner.rs means anything not seeded here is invisible to the
    // child (adversarial finding #1 — a bare interpreter/program spawned
    // with no PATH is ENOENT, not a slow failure).
    let mut env = base_env();
    env.extend(config.env.clone());
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

    // Shepherd channel: enabled by its own field, or implied by either
    // readiness flag — widening this gate must keep every existing term,
    // since dropping one silently stops opening fd 3 for whatever app relied
    // on it implying the channel.
    let channel = config.channel || config.wait_ready || config.shutdown_with_message;

    SpawnSpec {
        name,
        program,
        args,
        cwd,
        env,
        out_file,
        err_file,
        channel,
        stdin: config.stdin,
        credentials,
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

        let spec = assemble(&app, 1, &paths, None);

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

        let spec = assemble(&app, 5, &paths, None);

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

        let spec = assemble(&app, 0, &paths, None);

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

        let spec = assemble(&app, 0, &paths, None);

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

        let spec = assemble(&app, 0, &paths, None);

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

        let spec = assemble(&app, 2, &paths, None);

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

        let spec = assemble(&app, 1, &paths, None);

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

        let spec = assemble(&app, 0, &paths, None);

        assert_eq!(spec.out_file, PathBuf::from("/var/log/myapp.log"));
        // err_file still uses default
        assert_eq!(
            spec.err_file,
            PathBuf::from("/home/rin/.shep/logs/app-0-err.log")
        );
    }

    // fails if the gate drops the `channel` term from the disjunction
    #[test]
    fn channel_enabled_by_its_own_field() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app".to_string(),
            args: vec![],
            channel: true,
            wait_ready: false,
            shutdown_with_message: false,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths, None);

        assert!(spec.channel);
    }

    // fails if the gate drops the `wait_ready` term from the disjunction
    #[test]
    fn channel_enabled_by_wait_ready() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app".to_string(),
            args: vec![],
            channel: false,
            wait_ready: true,
            shutdown_with_message: false,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths, None);

        assert!(spec.channel);
    }

    // fails if the gate drops the `shutdown_with_message` term from the
    // disjunction
    #[test]
    fn channel_enabled_by_shutdown_with_message() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app".to_string(),
            args: vec![],
            channel: false,
            wait_ready: false,
            shutdown_with_message: true,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths, None);

        assert!(spec.channel);
    }

    // fails if any term is stuck open regardless of config (e.g. an
    // accidental `|| true`, or a term that defaults on) — every one of the
    // three flags is explicitly false here, so this is the counterpart the
    // three positive tests above can't cover on their own.
    #[test]
    fn channel_disabled_when_all_three_flags_are_false() {
        let app_config = AppConfig {
            name: "app".to_string(),
            script: "app".to_string(),
            args: vec![],
            channel: false,
            wait_ready: false,
            shutdown_with_message: false,
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let paths = test_paths();

        let spec = assemble(&app, 0, &paths, None);

        assert!(!spec.channel);
    }

    #[test]
    fn assembled_env_always_carries_a_path() {
        // tokio_runner.rs's env_clear() + envs(&spec.env) means this map IS
        // the child's whole env: no PATH here, and a bare interpreter name
        // (node, python3, sh, ...) can never be found by exec.
        let app_config = AppConfig {
            name: "web".to_string(),
            script: "app.js".to_string(),
            args: vec![],
            interpreter: Some("node".to_string()),
            ..Default::default()
        };
        let app = normalize(app_config).unwrap();
        let spec = assemble(&app, 0, &test_paths(), None);
        let path = spec
            .env
            .get("PATH")
            .expect("PATH must survive env_clear()+envs(&spec.env)");
        assert!(
            !path.is_empty(),
            "an empty PATH is exactly the ENOENT failure mode"
        );
    }

    #[test]
    fn an_explicit_app_path_overrides_the_seeded_default() {
        let mut app_config = AppConfig {
            name: "web".to_string(),
            script: "app.js".to_string(),
            args: vec![],
            interpreter: Some("node".to_string()),
            ..Default::default()
        };
        app_config
            .env
            .insert("PATH".to_string(), "/opt/custom/bin".to_string());
        let app = normalize(app_config).unwrap();
        let spec = assemble(&app, 0, &test_paths(), None);
        assert_eq!(
            spec.env.get("PATH").map(String::as_str),
            Some("/opt/custom/bin")
        );
    }

    /// fails if `stdin` does not reach the spec. It is the one field on the
    /// way to the runner whose default is "closed", so a spec assembled
    /// without it would silently give an opted-in app `/dev/null` and make
    /// every sendline row read `no_stdin` with nothing to point at.
    #[test]
    fn the_stdin_flag_reaches_the_spawn_spec() {
        let mut app = AppConfig::minimal("repl", "./repl");
        app.stdin = true;
        let spec = assemble(&normalize(app).unwrap(), 0, &test_paths(), None);
        assert!(spec.stdin);
    }

    /// fails if `stdin` is implied by something. `channel` is implied by
    /// `wait_ready` and `shutdown_with_message` because both need fd 3;
    /// nothing in shep needs fd 0, so nothing may turn it on behind the
    /// operator's back.
    #[test]
    fn nothing_else_turns_stdin_on() {
        let mut app = AppConfig::minimal("web", "./srv");
        app.channel = true;
        app.wait_ready = true;
        app.shutdown_with_message = true;
        let spec = assemble(&normalize(app).unwrap(), 0, &test_paths(), None);
        assert!(spec.channel, "the fixture should still open a channel");
        assert!(!spec.stdin);
    }
}
