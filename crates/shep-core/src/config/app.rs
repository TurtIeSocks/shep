//! Per-app configuration schema — one sheep's Flockfile entry

use core::fmt;

use std::collections::BTreeMap;

// use schemars::generate
use serde::{Deserialize, Serialize};

use crate::values::{MemSize, UpDuration};

/// How a health probe checks a sheep
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    /// HTTP GET must return 2xx
    Http,
    /// TCP connect must succeed
    Tcp,
    /// Command must exit 0
    Exec,
}

/// Readiness/liveness probe configuration (spec §7)
// wire format: changing field names/defaults is a breaking change
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    /// Probe mechanism
    pub kind: ProbeKind,
    /// URL (http), `host:port` (tcp), or command line (exec)
    pub target: String,
    /// Time between probes (default 10s)
    #[serde(default = "default_probe_interval")]
    pub interval: UpDuration,
    /// Per-probe timeout (default 5s)
    #[serde(default = "default_probe_timeout")]
    pub timeout: UpDuration,
    /// Consecutive failures before the probe reports unhealthy (default 3)
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
}

fn default_probe_interval() -> UpDuration {
    UpDuration::from_millis(10_000)
}
fn default_probe_timeout() -> UpDuration {
    UpDuration::from_millis(5_000)
}
fn default_failure_threshold() -> u32 {
    3
}

/// Per-app configuration — one sheep's entry in a Flockfile
///
/// Field names are the Flockfile contract (sheep-native; pm2 spellings are
/// rejected — the importer translates them). Unknown fields are errors so
/// typos fail loudly at parse time.
///
/// # Example
/// ```
/// use shep_core::config::AppConfig;
///
/// let app: AppConfig = toml::from_str("name = \"web\"\nscript = \"./srv\"").unwrap();
/// assert!(app.autorestart); // spec default
/// ```
// wire format: changing field names/defaults is a breaking change
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields, default)]
pub struct AppConfig {
    /// Unique sheep name (required)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "my-first-sheep",
        "group": "process",
        "blurb": "A convenient and unique name for shep to display"
    })))]
    pub name: String,
    /// Executable or script path (required)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "./index.js",
        "group": "process",
        "blurb": "The script that shep should use to launch your app"
    })))]
    pub script: String,
    /// Arguments passed to the script
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "inputs",
        "blurb": "Arguments passed to the script, as a list"
    })))]
    pub args: Vec<String>,
    /// Working directory (default: daemon's cwd at spawn registration)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "/srv/app",
        "group": "process",
        "blurb": "Where the process runs. Without it, the daemon's own directory"
    })))]
    pub cwd: Option<String>,
    /// Interpreter override (`"none"` = run script directly)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "none",
        "group": "process",
        "blurb": "What runs the script. Set it to none to exec the file directly"
    })))]
    pub interpreter: Option<String>,
    /// Environment for the sheep (merged over the daemon's filtered env)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "{ NODE_ENV = 'production' }",
        "group": "inputs",
        "blurb": "Environment variables for this app, layered over the daemon's own"
    })))]
    pub env: BTreeMap<String, String>,
    /// Instance count ("cluster" = N fork instances; spec §4)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "process",
        "blurb": "How many copies of this app to run"
    })))]
    pub instances: u32,
    /// Restart on unexpected exit
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Restarts the process automatically when it exits unexpectedly"
    })))]
    pub autorestart: bool,
    /// Start when the daemon starts / on `shep muster`
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Start this app when the daemon starts, and on shep muster"
    })))]
    pub autostart: bool,
    /// Exit codes treated as clean stop (no restart)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Exit codes that mean a clean stop, so shep will not restart"
    })))]
    pub stop_exit_codes: Vec<i32>,
    /// Uptime below this marks an exit as unstable
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "An exit sooner than this counts as unstable"
    })))]
    pub min_uptime: UpDuration,
    /// Consecutive unstable exits before `errored`
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "How many unstable exits in a row before shep gives up"
    })))]
    pub max_restarts: u32,
    /// Fixed delay before every restart (alternative to backoff)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "3s",
        "group": "control",
        "blurb": "A fixed wait before every restart, instead of growing backoff"
    })))]
    pub restart_delay: Option<UpDuration>,
    /// Initial backoff delay; grows ×1.5 capped at 15s (spec §4)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "5s",
        "group": "control",
        "blurb": "Starting delay between restarts, growing each time it fails again"
    })))]
    pub exp_backoff_restart_delay: Option<UpDuration>,
    /// Stop signal, one of `SIGTERM`/`SIGINT`/`SIGQUIT`/`SIGUSR2` (the `SIG`
    /// prefix and the case are both optional). Unset means `SIGTERM`.
    ///
    /// A `String` rather than a [`KillSignal`](crate::config::KillSignal) so
    /// the Flockfile schema and this struct's wire form stay plain text;
    /// `normalize` is what refuses a name outside that set, the same split
    /// `cron_restart` and the watch globs already use.
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "SIGTERM",
        "group": "process",
        "blurb": "Which signal shep sends first when stopping this app"
    })))]
    pub kill_signal: Option<String>,
    /// Grace period between stop signal and SIGKILL
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "How long shep waits after the stop signal before SIGKILL"
    })))]
    pub kill_timeout: UpDuration,
    /// Send `{"kind":"shutdown"}` on the shepherd channel instead of a signal
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Ask the app to stop over the channel instead of signalling it"
    })))]
    pub shutdown_with_message: bool,
    /// Readiness fallback window when no ready signal/probe configured
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "How long to wait for readiness when nothing else reports it"
    })))]
    pub listen_timeout: UpDuration,
    /// Drain window for the old instance during reload
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "How long the old instance gets to drain during a reload"
    })))]
    pub graceful_timeout: UpDuration,
    /// How long a triggered action gets to answer on the shepherd channel
    /// before its row becomes `ActionOutcome::TimedOut`.
    ///
    /// Defaults to 3s — comfortably under the 5s an RPC caller gets when it
    /// sends no deadline of its own (`shep-client`'s `DEFAULT_DEADLINE`,
    /// mirrored daemon-side as `rpc`'s `DEFAULT_DEADLINE_MS`). The margin
    /// matters more than the number: push this past that budget and a caller
    /// using the plain default gives up with `DeadlineExceeded` before the
    /// daemon's own honest `TimedOut` row ever reaches it. A legitimately
    /// slow action (a cache flush, say) can still ask for longer, but its
    /// caller has to ask for a longer deadline in step —
    /// `Client::request_with_deadline`, the way `shep logs -f` already asks
    /// for `LOG_PLANE_DEADLINE` rather than the client's default. `normalize`
    /// refuses a value no caller could ever satisfy, however long a deadline
    /// it asks for; a value merely above the *default* budget is a caller's
    /// choice to widen its own deadline, not a config error this crate can
    /// see.
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "How long a triggered action has to answer before shep gives up"
    })))]
    pub action_timeout: UpDuration,
    /// Memory ceiling — polling enforcer restarts above this
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "512M",
        "group": "control",
        "blurb": "Restart the app if it climbs above this much memory"
    })))]
    pub max_memory: Option<MemSize>,
    /// Watch files and restart on change
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Restart when a file changes"
    })))]
    pub watch: bool,
    /// Watch ignore globs (defaults added daemon-side: dot-entries, node_modules)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Paths watch should skip, on top of dotfiles and node_modules"
    })))]
    pub ignore_watch: Vec<String>,
    /// Watch debounce window (default 500ms, applied daemon-side)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "500",
        "group": "control",
        "blurb": "How long to wait after a change before restarting"
    })))]
    pub watch_delay: Option<UpDuration>,
    /// Cron pattern for scheduled restarts (croner dialect)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "* * * * *",
        "group": "cron",
        "blurb": "Restart on a schedule, written as a cron pattern"
    })))]
    pub cron_restart: Option<String>,
    /// Fold (group) this sheep belongs to
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "backend",
        "group": "process",
        "blurb": "A fold to group this app with others, for commands that take one"
    })))]
    pub fold: Option<String>,
    /// Run as this user (unix)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "www-data",
        "group": "process",
        "blurb": "Run as this user, on unix"
    })))]
    pub user: Option<String>,
    /// Run as this group (unix)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "www-data",
        "group": "process",
        "blurb": "Run as this group, on unix"
    })))]
    pub group: Option<String>,
    /// Stdout log file (default: `$SHEP_HOME/logs/<name>-<instance>-out.log`; `merge_logs` collapses to `<name>-out.log`)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "/var/log/my-first-sheep/out.log",
        "group": "process",
        "blurb": "Where stdout goes. Defaults to a file under $SHEP_HOME/logs"
    })))]
    pub out_file: Option<String>,
    /// Stderr log file (default: `$SHEP_HOME/logs/<name>-<instance>-err.log`; `merge_logs` collapses to `<name>-err.log`)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "/var/log/my-first-sheep/err.log",
        "group": "process",
        "blurb": "Where stderr goes. Defaults to a file under $SHEP_HOME/logs"
    })))]
    pub err_file: Option<String>,
    /// Merge instance logs into one file pair
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "process",
        "blurb": "Put every instance's output in one pair of files"
    })))]
    pub merge_logs: bool,
    /// Open the shepherd channel on fd 3 for this app on its own, without
    /// needing `wait_ready` or `shutdown_with_message` to imply it.
    ///
    /// Defaults to `false`: a socketpair plus two pump tasks per sheep is
    /// real cost weighed against spec §14.11's single-digit-MB idle-RSS
    /// goal, so a channel is opened only when something asks for one.
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "inputs",
        "blurb": "Opens fd 3 so the app can talk to shep directly"
    })))]
    pub channel: bool,
    /// Open a pipe on this sheep's stdin, so `shep whisper` can write to it.
    ///
    /// Defaults to `false`, and the default is the decision rather than a
    /// convenience. Without it a sheep gets `/dev/null` on fd 0, which is what
    /// every sheep has had until now, and three things argue for keeping it
    /// that way unless an app asks otherwise:
    ///
    /// - Flipping it for the whole flock is a behaviour change to processes
    ///   nobody asked to change.
    /// - **Programs detect stdin.** A closed or null fd 0 is how a great many
    ///   programs decide they are non-interactive — no prompt, no pager, no
    ///   readline, no colour. Handing them a pipe silently moves them to the
    ///   other branch.
    /// - It costs a descriptor and a pump task per sheep for the whole life of
    ///   the process, against spec §14.11's single-digit-MB idle-RSS goal — the
    ///   same budget [`Self::channel`]'s own default is protecting.
    ///
    /// Unlike `channel`, nothing implies this: `wait_ready` and
    /// `shutdown_with_message` both need fd 3 and so turn `channel` on for you,
    /// while nothing in shep needs a sheep's stdin except an operator typing
    /// `shep whisper`. A sheep without it answers a `no_stdin` row and names
    /// this field.
    ///
    /// The pipe's write end lives as long as the sheep does, so the app sees
    /// EOF on stdin when the process is on its way out, never before.
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "inputs",
        "blurb": "Keeps stdin open so shep whisper can write to the process"
    })))]
    pub stdin: bool,
    /// Expect `{"kind":"ready"}` on the shepherd channel
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Wait for the app to say it is ready on the channel"
    })))]
    pub wait_ready: bool,
    /// Asserts that the app itself sets `SO_REUSEPORT` before it binds —
    /// shep binds nothing, so it cannot set the option on the app's behalf.
    /// The child process owns the mechanism (Node ≥22's `reusePort`, Go's
    /// `net.ListenConfig.Control`, nginx's `reuseport`); shep's contribution
    /// is permission for the old and new instance to overlap during reload,
    /// not the socket option itself.
    ///
    /// **This field is inert today.** shep never reads it: reload overlap
    /// already happens unconditionally, so setting it changes nothing and
    /// leaving it unset costs nothing. It is kept because `shep import`
    /// writes it for a cluster-mode pm2 app and `shep flock` displays it, so
    /// dropping it would silently discard a value out of an imported config.
    /// It becomes load-bearing the day shep gains a reload mode that does NOT
    /// overlap by default, which is when the permission it describes stops
    /// being free — see `docs/specs/deferred.md`.
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "process",
        "blurb": "Not built yet. Setting it is refused rather than quietly ignored"
    })))]
    pub reuse_port: bool,
    /// Readiness probe — gates reload's AwaitReady (spec §7)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": { "kind": "http", "target": "http://127.0.0.1:8080/ready" },
        "group": "control",
        "blurb": "A health check shep waits on before it treats a reload as finished"
    })))]
    pub readiness_probe: Option<ProbeConfig>,
    /// Liveness probe — failures feed the restart policy (spec §7)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": { "kind": "http", "target": "http://127.0.0.1:8080/healthz" },
        "group": "control",
        "blurb": "A health check that triggers a restart when it keeps failing"
    })))]
    pub liveness_probe: Option<ProbeConfig>,
    /// Watch include globs (empty = watch cwd)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "group": "control",
        "blurb": "Which paths to watch. Empty means the working directory"
    })))]
    pub watch_options: Vec<String>,
    /// Timezone for `cron_restart` (IANA name)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "US/Eastern",
        "group": "cron",
        "blurb": "Which timezone cron_restart is read in, as an IANA name"
    })))]
    pub cron_timezone: Option<String>,
    /// Env var receiving the instance slot (default `SHEP_INSTANCE`)
    #[cfg_attr(feature = "schema", schemars(extend("init" = {
        "example": "INSTANCE_ID",
        "group": "inputs",
        "blurb": "The env var each instance finds its own slot number in"
    })))]
    pub increment_var: Option<String>,
}

/// Debug implementation does not leak env values (IR-41)
impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("name", &self.name)
            .field("script", &self.script)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .finish_non_exhaustive()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            script: String::new(),
            args: Vec::new(),
            cwd: None,
            interpreter: None,
            env: BTreeMap::new(),
            instances: 1,
            autorestart: true,
            autostart: true,
            stop_exit_codes: Vec::new(),
            min_uptime: UpDuration::from_millis(1000),
            max_restarts: 16,
            restart_delay: None,
            exp_backoff_restart_delay: None,
            kill_signal: None,
            kill_timeout: UpDuration::from_millis(1600),
            shutdown_with_message: false,
            listen_timeout: UpDuration::from_millis(3000),
            graceful_timeout: UpDuration::from_millis(8000),
            action_timeout: UpDuration::from_millis(3000),
            max_memory: None,
            watch: false,
            ignore_watch: Vec::new(),
            watch_delay: None,
            cron_restart: None,
            fold: None,
            user: None,
            group: None,
            out_file: None,
            err_file: None,
            merge_logs: false,
            channel: false,
            stdin: false,
            wait_ready: false,
            reuse_port: false,
            readiness_probe: None,
            liveness_probe: None,
            watch_options: Vec::new(),
            cron_timezone: None,
            increment_var: None,
        }
    }
}

impl AppConfig {
    /// A minimal config with spec defaults — the programmatic entry point
    #[must_use]
    pub fn minimal(name: &str, script: &str) -> Self {
        Self {
            name: name.to_string(),
            script: script.to_string(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::{MemSize, UpDuration};

    #[test]
    fn minimal_config_gets_spec_defaults() {
        let app = AppConfig::minimal("web", "./server");
        assert_eq!(app.name, "web");
        assert_eq!(app.script, "./server");
        assert!(app.autorestart);
        assert!(app.autostart);
        assert_eq!(app.instances, 1);
        assert_eq!(app.min_uptime, UpDuration::from_millis(1000));
        assert_eq!(app.max_restarts, 16);
        assert_eq!(app.kill_timeout, UpDuration::from_millis(1600));
        assert_eq!(app.listen_timeout, UpDuration::from_millis(3000));
        assert_eq!(app.graceful_timeout, UpDuration::from_millis(8000));
        assert_eq!(app.action_timeout, UpDuration::from_millis(3000));
        assert!(app.max_memory.is_none());
        assert!(app.fold.is_none());
        assert!(!app.channel);
    }

    /// fails if `stdin` defaults to anything but false. The default is the
    /// whole decision: piping stdin for every sheep would change how a great
    /// many programs behave (a closed stdin is how they decide they are
    /// non-interactive), and would hold a descriptor and a task per sheep for
    /// the life of the process.
    #[test]
    fn stdin_is_not_piped_unless_the_app_asks() {
        let app = AppConfig::minimal("web", "./srv");
        assert!(!app.stdin);
        let parsed: AppConfig = toml::from_str("name = \"web\"\nscript = \"./srv\"").unwrap();
        assert!(!parsed.stdin);
    }

    /// fails if the Flockfile key is spelled anything but `stdin`. It is a
    /// contract with every config file already written against it the moment
    /// this ships, and `deny_unknown_fields` means a rename is a hard parse
    /// failure for the operator rather than a silently ignored key.
    #[test]
    fn the_flockfile_key_is_stdin() {
        let parsed: AppConfig =
            toml::from_str("name = \"web\"\nscript = \"./srv\"\nstdin = true").unwrap();
        assert!(parsed.stdin);
    }

    #[test]
    fn toml_round_trip_with_newtypes() {
        let toml_src = r#"
name = "worker"
script = "python3"
args = ["job.py", "--fast"]
max_memory = "512M"
min_uptime = "5s"
fold = "backend"
env = { RUST_LOG = "info" }
"#;
        let app: AppConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(app.max_memory, Some("512M".parse::<MemSize>().unwrap()));
        assert_eq!(app.min_uptime, UpDuration::from_millis(5000));
        assert_eq!(app.fold.as_deref(), Some("backend"));
        assert_eq!(app.env.get("RUST_LOG").map(String::as_str), Some("info"));
        assert_eq!(app.args, vec!["job.py", "--fast"]);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = toml::from_str::<AppConfig>(
            "name = \"x\"\nscript = \"y\"\nmax_memory_restart = \"1G\"",
        )
        .unwrap_err();
        assert!(err.to_string().contains("max_memory_restart"), "{err}");
    }

    #[test]
    fn probe_config_parses_with_defaults() {
        let src = r#"
name = "api"
script = "./api"

[readiness_probe]
kind = "http"
target = "http://127.0.0.1:8080/healthz"
"#;
        let app: AppConfig = toml::from_str(src).unwrap();
        let probe = app.readiness_probe.unwrap();
        assert_eq!(probe.kind, ProbeKind::Http);
        assert_eq!(probe.target, "http://127.0.0.1:8080/healthz");
        assert_eq!(probe.interval, UpDuration::from_millis(10_000));
        assert_eq!(probe.timeout, UpDuration::from_millis(5_000));
        assert_eq!(probe.failure_threshold, 3);
        assert!(app.liveness_probe.is_none());
    }

    #[test]
    fn debug_redacts_env_values() {
        // IR-41: env may carry secrets; Debug output lands in daemon logs.
        // Exact string pinned so a lazy derive(Debug) refactor fails here.
        let mut app = AppConfig::minimal("web", "./srv");
        app.env
            .insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        app.env.insert("RUST_LOG".to_string(), "info".to_string());
        assert_eq!(
            format!("{app:?}"),
            "AppConfig { name: \"web\", script: \"./srv\", env: <2 vars>, .. }"
        );
    }
}
