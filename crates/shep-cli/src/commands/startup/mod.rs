//! `shep startup`/`unstartup`: installs and removes the init unit that
//! starts the shepherd at boot. [`mod@unit`] renders a unit from a
//! [`unit::UnitSpec`] with no filesystem or process access; this module
//! resolves a real `UnitSpec`, decides whether this process may install
//! it, and writes, enables, disables or removes it.
//!
//! # Privilege
//!
//! shep never escalates: no `sudo`, no setuid, no re-exec through a
//! helper. [`startup`] reads `geteuid()` once as a [`Privilege`] value; an
//! [`install`] or [`remove`] given [`Privilege::Unprivileged`] prints the
//! command an operator can paste and exits non-zero.

pub(crate) mod unit;

use std::path::{Path, PathBuf};

use unit::UnitSpec;

use crate::cli::{Init, StartupArgs};
use crate::exit::ExitCode;
use crate::output::{StartupStep, StartupSteps, Streams, emit, write_outcome};

/// `$SHEP_HOME`'s own directory name under a user's home, mirroring
/// `ShepPaths::resolve`'s `home_dir.join(".shep")`. A literal there and a
/// literal here: shep-core exports the default as behaviour rather than as a
/// constant, and inventing a public one to share would widen that crate's
/// surface for one call site.
const DEFAULT_HOME_DIR: &str = ".shep";

/// The mode a generated unit is created with.
///
/// systemd and launchd units are read, not executed: 0644. openrc and BSD
/// rc.d scripts are executed: 0755.
pub(crate) const fn unit_mode(init: Init) -> u32 {
    match init {
        Init::Systemd | Init::Launchd => 0o644,
        Init::Openrc | Init::FreebsdRc | Init::OpenbsdRc => 0o755,
    }
}

/// Where a generated unit for `init` is written, for `user`.
///
/// Systemd and launchd delegate to `unit::systemd_unit_path`/
/// `unit::launchd_plist_path`. Takes `Init` explicitly, not just the
/// detected one, so `unstartup` can find the file under whatever `--init`
/// names.
pub(crate) fn unit_path_for(init: Init, user: &str) -> PathBuf {
    match init {
        Init::Systemd => unit::systemd_unit_path(user),
        Init::Launchd => unit::launchd_plist_path(user),
        Init::Openrc => PathBuf::from(format!("/etc/init.d/shep-{user}")),
        Init::FreebsdRc => PathBuf::from(format!("/usr/local/etc/rc.d/shep_{user}")),
        Init::OpenbsdRc => PathBuf::from(format!("/etc/rc.d/shep_{user}")),
    }
}

/// Whether `user` can appear in a BSD rc script's variable names.
///
/// `rcvar` and `rcctl` turn the service name into shell variable names
/// (`shep_<user>_enable`, `shep_<user>_flags`). A `-` or `.` in `user`
/// produces an invalid `sh` identifier, and the script then fails at
/// `load_rc_config` with a syntax error naming a line number, not a user.
///
/// systemd and openrc name files, not variables, and are unaffected.
pub(crate) fn is_rc_safe_user(user: &str) -> bool {
    let mut chars = user.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A step that did what it was asked.
const OK: &str = "ok";

/// A step that found nothing to do, and that being the state it was asked to
/// produce. Only `unstartup` reaches it.
const ABSENT: &str = "absent";

/// Whether this process can install a system unit.
///
/// A value rather than a `geteuid()` call inside [`install`], because a test
/// cannot become root and one that skipped when unprivileged would never run
/// anywhere. [`startup`] reads `geteuid()` once and passes the answer down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Privilege {
    /// `geteuid() == 0`: a write under `/etc` or `/Library` will be allowed.
    Root,
    /// Anything else, which for this verb means: explain, do not attempt.
    Unprivileged,
}

/// Everything resolved before any privilege is needed: the unit to render,
/// where it goes, and the command to print if this process cannot install it
/// (built from `spec`'s own `exec`, `user` and `home`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartupPlan {
    /// Which init system this build writes for.
    pub init: Init,
    /// What the unit carries.
    pub spec: UnitSpec,
    /// Where the rendered unit goes.
    pub unit_path: PathBuf,
    /// The launchd label, unused on systemd.
    pub label: String,
    /// `$SUDO_USER`, resolved once in [`plan`]: a value [`install`] reads
    /// rather than an env lookup of its own, so a test can drive the
    /// sanitised-`PATH` warning without `std::env::set_var`, which is
    /// `unsafe` under `#![forbid(unsafe_code)]`.
    pub sudo_user: Option<String>,
}

/// A refusal that never got as far as a step: the exit code to return, and
/// the one line explaining it.
#[derive(Debug)]
struct Refusal {
    code: ExitCode,
    message: String,
}

/// Installs the init unit that starts the shepherd at boot.
///
/// `explicit_home` is `--home`/`$SHEP_HOME` as clap already folded it. When
/// it names nothing the unit carries the target user's own
/// `<passwd home>/.shep`, never this process's `$HOME`: under `sudo` that
/// is root's, and a unit built from it restores nothing after a reboot.
pub fn startup(
    streams: &mut Streams<'_>,
    explicit_home: Option<&Path>,
    args: &StartupArgs,
) -> ExitCode {
    match plan(explicit_home, args) {
        Ok(plan) => install(streams, &plan, privilege()),
        Err(refusal) => refuse(streams, refusal.code, &refusal.message),
    }
}

/// Disables and removes the unit [`startup`] installed.
///
/// Resolves its plan with no explicit home: a removal is addressed by the
/// unit's path and label, both of which come from the target user alone, and
/// nothing here reads the `$SHEP_HOME` the unit happens to carry.
pub fn unstartup(streams: &mut Streams<'_>, args: &StartupArgs) -> ExitCode {
    match plan(None, args) {
        Ok(plan) => remove(streams, &plan, privilege()),
        Err(refusal) => refuse(streams, refusal.code, &refusal.message),
    }
}

/// The user a generated unit runs the daemon as: `--user` when given, else
/// `$SUDO_USER`, else the invoking user.
///
/// Under `sudo shep startup` the invoking user is root, so `$SUDO_USER`
/// beats it: otherwise the unit would supervise root's flock while the
/// operator's stayed down.
pub(crate) fn target_user(
    explicit: Option<&str>,
    sudo_user: Option<&str>,
    invoking: &str,
) -> String {
    explicit.or(sudo_user).unwrap_or(invoking).to_string()
}

/// The `$SHEP_HOME` a generated unit carries: an explicit `--home`/`$SHEP_HOME`
/// when given, else the target user's own `<passwd home>/.shep`.
///
/// `user_home` is the target user's passwd home, never this process's
/// `$HOME`: `sudo` resets that to root's, so a unit built from it would
/// carry `/root/.shep` and restore nothing after a reboot.
pub(crate) fn target_home(explicit: Option<&Path>, user_home: &Path) -> PathBuf {
    explicit.map_or_else(|| user_home.join(DEFAULT_HOME_DIR), Path::to_path_buf)
}

/// Writes and enables the unit, or prints the command that would.
///
/// Refused, in order, before anything is written: a `$SHEP_HOME` that is
/// not a directory ([`ExitCode::Usage`]); a unit that already exists
/// ([`ExitCode::Usage`], naming `unstartup`, since rewriting a loaded
/// unit's file would leave it disagreeing with the running service); then
/// [`Privilege::Unprivileged`], which prints the resolved `sudo` command
/// and exits [`ExitCode::Failure`].
///
/// If [`plan`] saw `$SUDO_USER` set, [`secure_path_warning`] also warns:
/// `sudo` typically replaces `PATH` with its own `secure_path`, and shep
/// has no login `PATH` left to compare against.
pub(crate) fn install(
    streams: &mut Streams<'_>,
    plan: &StartupPlan,
    privilege: Privilege,
) -> ExitCode {
    if !plan.spec.home.is_dir() {
        return refuse(
            streams,
            ExitCode::Usage,
            &format!(
                "no directory at {}; pass --home with the $SHEP_HOME this unit should carry",
                plan.spec.home.display()
            ),
        );
    }
    if plan.unit_path.exists() {
        return refuse(
            streams,
            ExitCode::Usage,
            &format!(
                "{} already exists; shep unstartup removes it first",
                plan.unit_path.display()
            ),
        );
    }
    if privilege == Privilege::Unprivileged {
        return refuse(
            streams,
            ExitCode::Failure,
            &format!(
                "installing the unit needs root; run: sudo {} startup --user {} --home {}",
                shell_quote(&plan.spec.exec.display().to_string()),
                shell_quote(&plan.spec.user),
                shell_quote(&plan.spec.home.display().to_string()),
            ),
        );
    }
    if let Some(message) = secure_path_warning(plan.sudo_user.as_deref(), &plan.spec) {
        streams.aside("secure_path", &message);
    }

    let mut steps = vec![write_unit(plan)];
    match plan.init {
        Init::Systemd => {
            steps.push(run_step("systemctl", &["daemon-reload"]));
            steps.push(run_step(
                "systemctl",
                &["enable", "--now", &unit_file_name(plan)],
            ));
        }
        Init::Launchd => steps.push(run_step(
            "launchctl",
            &["bootstrap", "system", &plan.unit_path.display().to_string()],
        )),
        Init::Openrc => {
            steps.push(run_step(
                "rc-update",
                &["add", &unit_file_name(plan), "default"],
            ));
            steps.push(run_step("rc-service", &[&unit_file_name(plan), "start"]));
        }
        Init::FreebsdRc => {
            steps.push(run_step(
                "sysrc",
                &[&format!("{}_enable=YES", unit_file_name(plan))],
            ));
            steps.push(run_step("service", &[&unit_file_name(plan), "start"]));
        }
        Init::OpenbsdRc => {
            steps.push(run_step("rcctl", &["enable", &unit_file_name(plan)]));
            steps.push(run_step("rcctl", &["start", &unit_file_name(plan)]));
        }
    }
    report(streams, "startup", steps)
}

/// The notice [`install`] prints when `$SUDO_USER` was set: `None` if it
/// was not, else a message naming `sudo_user` and showing `spec.path` in
/// full, so the operator can check it against that user's login `PATH`.
///
/// A pure function of the two values, not an environment read of its own:
/// this crate is `#![forbid(unsafe_code)]`, so a test cannot call
/// `std::env::set_var` to establish an ambient `$SUDO_USER` for [`plan`]
/// to read.
fn secure_path_warning(sudo_user: Option<&str>, spec: &UnitSpec) -> Option<String> {
    let sudo_user = sudo_user?;
    Some(format!(
        "sudo may have replaced PATH with its own secure_path before shep ever saw it \
         (SUDO_USER={sudo_user}); the unit now carries PATH={}; compare it against \
         {sudo_user}'s login PATH, and if a directory such as ~/.bun/bin or ~/.cargo/bin \
         is missing, run shep unstartup then sudo --preserve-env=PATH shep startup to \
         carry it through instead",
        spec.path.to_string_lossy(),
    ))
}

/// Disables and removes the unit, or prints the command that would.
///
/// A missing unit is a success carrying one `absent` row; that check runs
/// before the privilege gate, so `shep unstartup` on a host that never ran
/// `startup` answers without demanding root. Otherwise this needs root:
/// without it, prints `sudo <exec> unstartup --user <user>` (no `--home`,
/// since removal is addressed by path and label alone) and exits
/// [`ExitCode::Failure`].
pub(crate) fn remove(
    streams: &mut Streams<'_>,
    plan: &StartupPlan,
    privilege: Privilege,
) -> ExitCode {
    if !plan.unit_path.exists() {
        return report(
            streams,
            "unstartup",
            vec![StartupStep {
                action: "removed",
                target: plan.unit_path.display().to_string(),
                result: ABSENT.to_string(),
            }],
        );
    }
    if privilege == Privilege::Unprivileged {
        return refuse(
            streams,
            ExitCode::Failure,
            &format!(
                "removing the unit needs root; run: sudo {} unstartup --user {}",
                shell_quote(&plan.spec.exec.display().to_string()),
                shell_quote(&plan.spec.user),
            ),
        );
    }

    let mut steps = Vec::new();
    match plan.init {
        Init::Systemd => {
            steps.push(run_step(
                "systemctl",
                &["disable", "--now", &unit_file_name(plan)],
            ));
            steps.push(remove_unit(plan));
            steps.push(run_step("systemctl", &["daemon-reload"]));
        }
        Init::Launchd => {
            steps.push(run_step(
                "launchctl",
                &["bootout", &format!("system/{}", plan.label)],
            ));
            steps.push(remove_unit(plan));
        }
        Init::Openrc => {
            steps.push(run_step("rc-service", &[&unit_file_name(plan), "stop"]));
            steps.push(run_step(
                "rc-update",
                &["del", &unit_file_name(plan), "default"],
            ));
            steps.push(remove_unit(plan));
        }
        Init::FreebsdRc => {
            steps.push(run_step("service", &[&unit_file_name(plan), "stop"]));
            steps.push(run_step(
                "sysrc",
                &["-x", &format!("{}_enable", unit_file_name(plan))],
            ));
            steps.push(remove_unit(plan));
        }
        Init::OpenbsdRc => {
            steps.push(run_step("rcctl", &["stop", &unit_file_name(plan)]));
            steps.push(run_step("rcctl", &["disable", &unit_file_name(plan)]));
            steps.push(remove_unit(plan));
        }
    }
    report(streams, "unstartup", steps)
}

/// Renders the unit and writes it at [`unit_mode`], as one step.
///
/// Mode set at `open` time, not via a later chmod: a create-then-chmod
/// sequence would leave the file readable at the ambient umask until the
/// chmod lands, and this file lands in a directory every user can reach.
fn write_unit(plan: &StartupPlan) -> StartupStep {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let rendered = match plan.init {
        Init::Systemd => unit::systemd_unit(&plan.spec),
        Init::Launchd => unit::launchd_plist(&plan.spec),
        Init::Openrc => unit::openrc_script(&plan.spec),
        Init::FreebsdRc => unit::freebsd_rc_script(&plan.spec),
        Init::OpenbsdRc => unit::openbsd_rc_script(&plan.spec),
    };
    let mode = unit_mode(plan.init);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&plan.unit_path)
        .and_then(|mut file| {
            file.write_all(rendered.as_bytes())?;
            // `open`'s mode is masked by the umask; this chmod makes it
            // deterministic. Acts on the open fd, so there is no race, and
            // it only ever widens a mode already no wider than this one.
            file.set_permissions(std::fs::Permissions::from_mode(mode))
        });
    StartupStep {
        action: "wrote",
        target: plan.unit_path.display().to_string(),
        result: match written {
            Ok(()) => OK.to_string(),
            Err(err) => err.to_string(),
        },
    }
}

/// Removes the unit file, as one step. A file that is already gone is the
/// state this was asked to produce, so it reports [`ABSENT`] rather than the
/// `NotFound` it saw.
fn remove_unit(plan: &StartupPlan) -> StartupStep {
    StartupStep {
        action: "removed",
        target: plan.unit_path.display().to_string(),
        result: match std::fs::remove_file(&plan.unit_path) {
            Ok(()) => OK.to_string(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ABSENT.to_string(),
            Err(err) => err.to_string(),
        },
    }
}

/// Runs one init-system command and reports it as a step.
///
/// Never short-circuits the caller: a command that failed is a row like any
/// other, and [`report`] is what turns a failed row into a non-zero exit
/// after every remaining step has still been attempted.
fn run_step(program: &str, args: &[&str]) -> StartupStep {
    let target = format!("{program} {}", args.join(" "));
    let result = match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => OK.to_string(),
        Ok(output) => failure_line(&output),
        Err(err) => err.to_string(),
    };
    StartupStep {
        action: "ran",
        target,
        result,
    }
}

/// `shep-<user>.service`, read back off the path the plan already
/// resolved rather than formatted a second time: `systemctl enable` wants
/// the unit's name, and two spellings could drift apart.
fn unit_file_name(plan: &StartupPlan) -> String {
    plan.unit_path
        .file_name()
        .unwrap_or(plan.unit_path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Emits the steps and returns the code they earned.
///
/// A step that failed fails the verb, but only after every remaining step has
/// run: a half-installed unit is worse than a fully-attempted one, and the
/// operator needs every row to know which half.
fn report(streams: &mut Streams<'_>, command: &str, steps: Vec<StartupStep>) -> ExitCode {
    let failed = steps
        .iter()
        .any(|step| step.result != OK && step.result != ABSENT);
    let written = write_outcome(emit(
        &mut *streams.out,
        streams.fmt,
        command,
        StartupSteps(steps),
        streams.style,
    ));
    if failed { ExitCode::Failure } else { written }
}

/// Quotes one word of a printed command so the line can be pasted rather
/// than read and repaired.
///
/// A `$SHEP_HOME` with a space is a legal path; unquoted, it would become
/// two arguments and paste a command carrying half the path.
///
/// `pub(crate)`: [`unit::freebsd_rc_script`] and [`unit::openbsd_rc_script`]
/// reuse it as the single-quote former for a value re-evaluated by a
/// nested shell, distinct from `unit`'s own double-quote escaper.
pub(crate) fn shell_quote(word: &str) -> String {
    let safe = |b: &u8| b.is_ascii_alphanumeric() || b"_./:@%+=-".contains(b);
    if !word.is_empty() && word.as_bytes().iter().all(safe) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The one line a failed command is reported by: the first non-blank line of
/// its stderr, or its exit status when it failed without saying anything.
///
/// One line because a row is one line, and systemd answers a refusal with
/// several of its own advice.
fn failure_line(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(|| format!("failed: {}", output.status), ToString::to_string)
}

/// This process's own privilege, read once.
fn privilege() -> Privilege {
    if nix::unistd::geteuid().is_root() {
        Privilege::Root
    } else {
        Privilege::Unprivileged
    }
}

/// Writes one refusal to stderr and returns the code it earned.
fn refuse(streams: &mut Streams<'_>, code: ExitCode, message: &str) -> ExitCode {
    streams.fail(code, message)
}

/// Which init a Linux host running these two probes is on.
///
/// A pure function so the order is testable off Linux. systemd wins a
/// tie: `/run/systemd/system` is what `sd_booted(3)` checks, so both
/// present means openrc leftovers on a systemd host, not the reverse.
///
/// [`current_init`]'s Linux arm is the only non-test caller; `#[cfg]`-ed
/// away on every other target rather than blanket-`#[allow(dead_code)]`ed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const fn linux_init(systemd: bool, openrc: bool) -> Option<Init> {
    if systemd {
        Some(Init::Systemd)
    } else if openrc {
        Some(Init::Openrc)
    } else {
        None
    }
}

/// The init system this host is actually running, or `None` when it is one
/// shep has no renderer for.
///
/// Linux is a runtime probe: `target_os` cannot tell systemd and openrc
/// apart. The ordering lives in [`linux_init`]; this function is the two
/// filesystem reads that feed it. Every other target is a compile-time
/// fact.
///
/// A Linux container with no `/run/systemd/system` and no openrc is
/// refused rather than guessed at; `--init` overrides this entirely.
fn current_init() -> Option<Init> {
    #[cfg(target_os = "linux")]
    {
        linux_init(
            Path::new("/run/systemd/system").is_dir(),
            Path::new("/run/openrc/softlevel").exists() || Path::new("/run/openrc").is_dir(),
        )
    }
    #[cfg(target_os = "macos")]
    {
        Some(Init::Launchd)
    }
    #[cfg(target_os = "freebsd")]
    {
        Some(Init::FreebsdRc)
    }
    #[cfg(target_os = "openbsd")]
    {
        Some(Init::OpenbsdRc)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd"
    )))]
    {
        None
    }
}

/// Resolves everything either verb needs before privilege enters into it.
///
/// `$SUDO_USER` is read here rather than passed in, so [`target_user`] stays
/// a function three cases can be stated about; an empty one is treated as
/// unset, since that is what a shell that exported it without a value means.
fn plan(explicit_home: Option<&Path>, args: &StartupArgs) -> Result<StartupPlan, Refusal> {
    let Some(init) = args.init.or_else(current_init) else {
        return Err(Refusal {
            code: ExitCode::Failure,
            message: "could not tell which init system is running: neither \
                      /run/systemd/system nor /run/openrc is present. Name one \
                      with --init (systemd, openrc, launchd, freebsd-rc, openbsd-rc)"
                .to_string(),
        });
    };
    let exec = std::env::current_exe().map_err(|err| Refusal {
        code: ExitCode::Failure,
        message: format!("could not resolve this binary's own path: {err}"),
    })?;
    let sudo_user = std::env::var("SUDO_USER")
        .ok()
        .filter(|name| !name.is_empty());
    let user = target_user(
        args.user.as_deref(),
        sudo_user.as_deref(),
        &invoking_user()?,
    );
    if matches!(init, Init::FreebsdRc | Init::OpenbsdRc) && !is_rc_safe_user(&user) {
        return Err(Refusal {
            code: ExitCode::Usage,
            message: format!(
                "a BSD rc.d script turns the user name into a shell variable, so {user} \
                 cannot be used: it must start with a letter or underscore and contain \
                 only letters, digits and underscores. Pass --user with a name that does."
            ),
        });
    }
    let passwd_home = passwd_home(&user)?;
    let unit_path = unit_path_for(init, &user);
    Ok(StartupPlan {
        init,
        label: unit::launchd_label(&user),
        spec: UnitSpec {
            user,
            exec,
            home: target_home(explicit_home, &passwd_home),
            // Captured from this invocation so an interpreter under
            // `~/.bun` or `~/.cargo` stays findable after reboot. Left
            // empty rather than guessed at if unset.
            path: std::env::var_os("PATH").unwrap_or_default(),
            working_dir: passwd_home,
        },
        unit_path,
        sudo_user,
    })
}

/// This process's own user name, for [`target_user`]'s last fallback.
fn invoking_user() -> Result<String, Refusal> {
    let uid = nix::unistd::geteuid();
    match nix::unistd::User::from_uid(uid) {
        Ok(Some(user)) => Ok(user.name),
        Ok(None) => Err(Refusal {
            code: ExitCode::Failure,
            message: format!("no passwd entry for uid {uid}"),
        }),
        Err(errno) => Err(Refusal {
            code: ExitCode::Failure,
            message: format!("could not read this process's own passwd entry: {errno}"),
        }),
    }
}

/// The target user's passwd home: the unit's working directory, and the
/// root its `$SHEP_HOME` defaults under.
fn passwd_home(name: &str) -> Result<PathBuf, Refusal> {
    match nix::unistd::User::from_name(name) {
        Ok(Some(user)) => Ok(user.dir),
        Ok(None) => Err(Refusal {
            code: ExitCode::Usage,
            message: format!("no such user: {name}"),
        }),
        Err(errno) => Err(Refusal {
            code: ExitCode::Failure,
            message: format!("could not look up {name}: {errno}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::cli::Format;

    /// The unit path a `StartupPlan` built for a test points at, so `install`
    /// can be driven without writing into `/etc` or `/Library`. Every field
    /// but `home` is fixed; `home` is what each case varies.
    ///
    /// `with_file_name` rather than a path under `home`: the two cases that
    /// matter pass a `home` that does not exist, and a unit path inside it
    /// could not be written even by the run this is meant to prove writes
    /// nothing.
    fn plan_for_test(home: &Path) -> StartupPlan {
        StartupPlan {
            init: Init::Systemd,
            spec: UnitSpec {
                user: "deploy".to_string(),
                exec: PathBuf::from("/usr/local/bin/shep"),
                home: home.to_path_buf(),
                path: OsString::from("/usr/local/bin:/usr/bin:/bin"),
                working_dir: PathBuf::from("/home/deploy"),
            },
            unit_path: home.with_file_name("shep-deploy.service"),
            label: unit::launchd_label("deploy"),
            sudo_user: None,
        }
    }

    /// Under `sudo`, the invoking user is root; ignoring `$SUDO_USER` would
    /// install a unit supervising root's flock instead of the operator's.
    #[test]
    fn the_target_user_prefers_an_explicit_name_then_sudo_user() {
        assert_eq!(target_user(Some("deploy"), Some("ada"), "root"), "deploy");
        assert_eq!(target_user(None, Some("ada"), "root"), "ada");
        assert_eq!(target_user(None, None, "ada"), "ada");
    }

    /// `sudo` resets `$HOME` to root's; falling back to it would carry
    /// `/root/.shep` and restore nothing after a reboot.
    #[test]
    fn the_target_home_comes_from_the_target_user_not_the_invoker() {
        assert_eq!(
            target_home(None, Path::new("/home/ada")),
            Path::new("/home/ada/.shep")
        );
        assert_eq!(
            target_home(Some(Path::new("/srv/shep")), Path::new("/home/ada")),
            Path::new("/srv/shep")
        );
    }

    #[test]
    fn secure_path_warning_names_the_sudo_user_and_the_full_path_only_under_sudo() {
        let spec = UnitSpec {
            user: "deploy".to_string(),
            exec: PathBuf::from("/usr/local/bin/shep"),
            home: PathBuf::from("/home/deploy/.shep"),
            path: OsString::from("/usr/local/sbin:/usr/local/bin:/usr/bin:/bin"),
            working_dir: PathBuf::from("/home/deploy"),
        };
        assert_eq!(
            secure_path_warning(None, &spec),
            None,
            "no $SUDO_USER means shep was never run through sudo at all"
        );

        let message =
            secure_path_warning(Some("ada"), &spec).expect("$SUDO_USER was set, so this warns");
        assert!(message.contains("SUDO_USER=ada"), "{message}");
        assert!(
            message.contains("/usr/local/sbin:/usr/local/bin:/usr/bin:/bin"),
            "the full captured PATH must be readable without a second lookup: {message}"
        );
        assert!(
            message.contains("--preserve-env=PATH"),
            "the operator needs a way to get the PATH they meant: {message}"
        );
    }

    /// Exit 0 would make a script believe a unit was installed; a command
    /// missing `--home` re-runs the sudo trap [`target_home`] describes.
    #[test]
    fn an_unprivileged_startup_prints_the_command_and_exits_non_zero() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            install(&mut streams, &plan_for_test(&home), Privilege::Unprivileged)
        };
        assert_ne!(code, ExitCode::Success);
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains("sudo"), "{printed}");
        assert!(printed.contains("--home"), "{printed}");
        assert!(printed.contains(home.to_str().unwrap()), "{printed}");
        assert!(
            !plan_for_test(&home).unit_path.exists(),
            "an unprivileged startup writes no unit"
        );
    }

    /// The secure-`PATH` warning is checked after the privilege gate, not
    /// before: nothing is written on this path, so it must stay as silent
    /// as the plain refusal above.
    #[test]
    fn an_unprivileged_startup_under_sudo_still_prints_no_secure_path_warning() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let plan = StartupPlan {
            sudo_user: Some("ada".to_string()),
            ..plan_for_test(&home)
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            install(&mut streams, &plan, Privilege::Unprivileged);
        }
        let printed = String::from_utf8(err).unwrap();
        assert!(!printed.contains("secure_path"), "{printed}");
    }

    /// Accepting a missing `$SHEP_HOME` would yield a unit that boots
    /// cleanly and restores an empty flock.
    #[test]
    fn a_shep_home_that_does_not_exist_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            // Root: an unprivileged run would refuse for the other reason
            // first, never exercising the home check.
            install(&mut streams, &plan_for_test(&missing), Privilege::Root)
        };
        assert_eq!(code, ExitCode::Usage);
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains(missing.to_str().unwrap()), "{printed}");
        assert!(
            !plan_for_test(&missing).unit_path.exists(),
            "a refused startup writes no unit"
        );
    }

    /// Rewriting an existing unit's file would not change the service
    /// already loaded, leaving the file and the running unit disagreeing.
    ///
    /// `Privilege::Root`: unprivileged would refuse for the other reason
    /// first, never reaching this check.
    #[test]
    fn an_existing_unit_is_refused_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let plan = StartupPlan {
            sudo_user: Some("ada".to_string()),
            ..plan_for_test(&home)
        };
        std::fs::write(&plan.unit_path, "# hand-edited\n").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            install(&mut streams, &plan, Privilege::Root)
        };
        assert_eq!(code, ExitCode::Usage);
        let printed = String::from_utf8(err).unwrap();
        assert!(
            printed.contains(plan.unit_path.to_str().unwrap()),
            "{printed}"
        );
        assert!(printed.contains("unstartup"), "{printed}");
        assert!(
            !printed.contains("secure_path"),
            "a plan carrying $SUDO_USER still warns nothing on a refusal path \
             that wrote no unit: {printed}"
        );
        assert_eq!(
            std::fs::read_to_string(&plan.unit_path).unwrap(),
            "# hand-edited\n",
            "a refused startup leaves the operator's own file alone"
        );
    }

    /// `Privilege::Unprivileged`: the absence check runs before the
    /// privilege gate, so a `Privilege::Root` plan here would reach a real
    /// `systemctl`.
    #[test]
    fn an_absent_unit_is_an_absent_row_and_a_success() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            remove(&mut streams, &plan_for_test(&home), Privilege::Unprivileged)
        };
        assert_eq!(code, ExitCode::Success);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains(ABSENT), "{printed}");
    }

    /// The removal check matters most: this verb's whole job is destructive.
    #[test]
    fn an_unprivileged_unstartup_prints_the_command_and_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        let plan = plan_for_test(&home);
        std::fs::write(&plan.unit_path, "# installed\n").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            remove(&mut streams, &plan, Privilege::Unprivileged)
        };
        assert_ne!(code, ExitCode::Success);
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains("sudo"), "{printed}");
        assert!(printed.contains("unstartup"), "{printed}");
        assert!(
            plan.unit_path.exists(),
            "an unprivileged unstartup removes nothing"
        );
    }

    /// Drives `write_unit` and `remove_unit` directly rather than
    /// `install`: `install`'s privileged path runs `systemctl`, which no
    /// test here may reach.
    #[test]
    fn the_unit_is_written_at_0644_and_removed_again() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        let plan = plan_for_test(&home);

        let step = write_unit(&plan);
        assert_eq!(step.result, OK, "{step:?}");
        assert_eq!(
            std::fs::read_to_string(&plan.unit_path).unwrap(),
            unit::systemd_unit(&plan.spec)
        );
        let mode = std::fs::metadata(&plan.unit_path)
            .unwrap()
            .permissions()
            .mode();
        // A literal, not `unit_mode(Init::Systemd)`: comparing the
        // function's own return value to itself would pass no matter what
        // it returned.
        assert_eq!(mode & 0o777, 0o644, "mode was {:o}", mode & 0o777);

        assert_eq!(remove_unit(&plan).result, OK);
        assert!(!plan.unit_path.exists());
        assert_eq!(
            remove_unit(&plan).result,
            ABSENT,
            "removing what is already gone is the state that was asked for"
        );
    }

    /// Drives `report` with hand-built rows rather than a real `install`:
    /// producing a genuinely failing step would need a real `systemctl`,
    /// which no test in this crate may run.
    #[test]
    fn a_failed_step_fails_the_verb_and_still_prints_every_row() {
        let step = |action, target: &str, result: &str| StartupStep {
            action,
            target: target.to_string(),
            result: result.to_string(),
        };
        let steps = vec![
            step("wrote", "/etc/systemd/system/shep-deploy.service", OK),
            step(
                "ran",
                "systemctl daemon-reload",
                "Failed to reload: read-only file system",
            ),
            step("ran", "systemctl enable --now shep-deploy.service", OK),
        ];

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            report(&mut streams, "startup", steps)
        };
        assert_eq!(code, ExitCode::Failure);
        let printed = String::from_utf8(out).unwrap();
        for expected in [
            "daemon-reload",
            "read-only file system",
            "enable --now shep-deploy.service",
        ] {
            assert!(
                printed.contains(expected),
                "every step is reported, not only the ones before the failure: {printed}"
            );
        }
    }

    /// A `$SHEP_HOME` with a space is legal; unquoted it would split into
    /// two arguments.
    #[test]
    fn a_printed_command_quotes_what_a_shell_would_split() {
        assert_eq!(shell_quote("/home/ada/.shep"), "/home/ada/.shep");
        assert_eq!(shell_quote("/opt/my shep"), "'/opt/my shep'");
        assert_eq!(shell_quote("ada's"), r"'ada'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    /// systemd answers a refusal in several lines; a row is one line.
    #[test]
    fn a_failed_step_reports_one_line_and_never_an_empty_one() {
        use std::os::unix::process::ExitStatusExt as _;

        let failed = std::process::ExitStatus::from_raw(256);
        let output = |stderr: &str| std::process::Output {
            status: failed,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(
            failure_line(&output(
                "\nFailed to enable unit: Unit is masked.\nSee `journalctl`.\n"
            )),
            "Failed to enable unit: Unit is masked."
        );
        assert!(
            !failure_line(&output("")).is_empty(),
            "a command that failed silently still has to say so"
        );
    }

    #[test]
    fn the_mode_is_read_only_for_units_and_executable_for_scripts() {
        assert_eq!(unit_mode(Init::Systemd), 0o644);
        assert_eq!(unit_mode(Init::Launchd), 0o644);
        assert_eq!(unit_mode(Init::Openrc), 0o755);
        assert_eq!(unit_mode(Init::FreebsdRc), 0o755);
        assert_eq!(unit_mode(Init::OpenbsdRc), 0o755);
    }

    /// systemd wins a tie: `/run/systemd/system` is what `sd_booted(3)`
    /// checks, so both present means openrc leftovers on a systemd host.
    #[test]
    fn systemd_wins_when_both_linux_probes_are_true() {
        assert_eq!(linux_init(true, true), Some(Init::Systemd));
        assert_eq!(linux_init(true, false), Some(Init::Systemd));
        assert_eq!(linux_init(false, true), Some(Init::Openrc));
        assert_eq!(linux_init(false, false), None);
    }

    /// The escape hatch for a container with no `/run/systemd/system`, and
    /// the only way a macOS host renders a systemd unit.
    #[test]
    fn an_explicit_init_beats_detection() {
        use clap::Parser as _;

        use crate::cli::{Cli, Commands};

        let cli = Cli::try_parse_from(["shep", "startup", "--init", "openrc"]).unwrap();
        match cli.command {
            Commands::Startup(args) => assert_eq!(args.init, Some(Init::Openrc)),
            other => panic!("expected Startup, got {other:?}"),
        }
    }

    /// A unit installed under one init must be removable after the host
    /// changes to another, so this is about which file gets removed, not
    /// which struct the two verbs share.
    #[test]
    fn each_init_names_its_own_unit_path() {
        assert_eq!(
            unit_path_for(Init::Openrc, "deploy"),
            PathBuf::from("/etc/init.d/shep-deploy")
        );
        assert_eq!(
            unit_path_for(Init::FreebsdRc, "deploy"),
            PathBuf::from("/usr/local/etc/rc.d/shep_deploy")
        );
        assert_eq!(
            unit_path_for(Init::OpenbsdRc, "deploy"),
            PathBuf::from("/etc/rc.d/shep_deploy")
        );
        // systemd and launchd keep the paths they already had
        assert_eq!(
            unit_path_for(Init::Systemd, "deploy"),
            unit::systemd_unit_path("deploy")
        );
        assert_eq!(
            unit_path_for(Init::Launchd, "deploy"),
            unit::launchd_plist_path("deploy")
        );
    }

    /// `web-app` and `deploy.svc` are legal usernames and illegal shell
    /// variable fragments; a script built from one fails at
    /// `load_rc_config` naming a line number, not the user.
    #[test]
    fn a_user_name_that_cannot_be_a_shell_variable_is_refused() {
        for ok in ["deploy", "www", "_shep", "app2"] {
            assert!(is_rc_safe_user(ok), "{ok} should be accepted");
        }
        for bad in ["web-app", "deploy.svc", "2fast", "", "ünicode"] {
            assert!(!is_rc_safe_user(bad), "{bad} should be refused");
        }
    }
}
