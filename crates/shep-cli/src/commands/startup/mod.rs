//! `shep startup`/`unstartup`: installing and removing the init unit that
//! starts the shepherd at boot. [`mod@unit`] renders a systemd unit or a
//! launchd plist from a [`unit::UnitSpec`] — pure `format!`, no filesystem
//! or process access. This module resolves a real `UnitSpec`, decides
//! whether this process may install it, and writes, enables, disables or
//! removes it.
//!
//! # Privilege
//!
//! shep never escalates. There is no `sudo`, no setuid, and no re-exec
//! through a helper: [`startup`] reads `geteuid()` once and hands the answer
//! down as a [`Privilege`] value, and an [`install`] or [`remove`] that is
//! given [`Privilege::Unprivileged`] prints the exact command an operator
//! can paste and exits non-zero — non-zero so a script notices rather than
//! believing a unit was installed.

pub(crate) mod unit;

use std::path::{Path, PathBuf};

use unit::UnitSpec;

use crate::cli::{Format, Init, StartupArgs};
use crate::exit::ExitCode;
use crate::output::{
    StartupStep, StartupSteps, Streams, emit, emit_error, emit_notice, write_outcome,
};

/// `$SHEP_HOME`'s own directory name under a user's home, mirroring
/// `ShepPaths::resolve`'s `home_dir.join(".shep")`. A literal there and a
/// literal here: shep-core exports the default as behaviour rather than as a
/// constant, and inventing a public one to share would widen that crate's
/// surface for one call site.
const DEFAULT_HOME_DIR: &str = ".shep";

/// The mode a generated unit is created with.
///
/// A systemd unit and a launchd plist are **read** by their init system:
/// 0644. An openrc script and a BSD rc.d script are **executed**: 0755.
/// Shipping an openrc script at 0644 fails at the next reboot, which is the
/// worst possible time to find out.
pub(crate) const fn unit_mode(init: Init) -> u32 {
    match init {
        Init::Systemd | Init::Launchd => 0o644,
        Init::Openrc | Init::FreebsdRc | Init::OpenbsdRc => 0o755,
    }
}

/// Where a generated unit for `init` is written, for `user`.
///
/// Systemd and launchd keep calling their own existing formatters —
/// `unit::systemd_unit_path`/`unit::launchd_plist_path` — rather than
/// restating their format strings here. The other three name a file
/// `unstartup` has to be able to find under any init an operator names with
/// `--init`, which is why this is a function of `Init` alone rather than
/// something `plan` only ever calls for the detected one.
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
/// `rcvar` and `rcctl` turn the service name into **shell variable names**
/// (`shep_<user>_enable`, `shep_<user>_flags`). A username containing `-` or
/// `.` — `web-app` and `deploy.svc` are both legal on both systems —
/// produces `shep_web-app_enable`, which is not a valid `sh` variable, and
/// the script then fails at `load_rc_config` with a syntax error naming a
/// line number rather than a user.
///
/// systemd and openrc name *files*, not variables, and are unaffected. Do
/// not add this check there.
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
    /// `$SUDO_USER`, resolved once in [`plan`] the same way [`Privilege`]
    /// is: a value [`install`] reads rather than an environment lookup of
    /// its own, so a test can drive the sanitised-`PATH` warning below
    /// without `std::env::set_var` — `unsafe` in edition 2024, and this
    /// crate is `#![forbid(unsafe_code)]`.
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
/// it names nothing the unit carries the **target user's** own
/// `<passwd home>/.shep`, never this process's `$HOME` — under `sudo` that
/// is root's, and a unit built from it restores nothing after a reboot.
pub fn startup(
    streams: &mut Streams<'_>,
    fmt: Format,
    explicit_home: Option<&Path>,
    args: &StartupArgs,
) -> ExitCode {
    match plan(explicit_home, args) {
        Ok(plan) => install(streams, fmt, &plan, privilege()),
        Err(refusal) => refuse(streams, fmt, refusal.code, &refusal.message),
    }
}

/// Disables and removes the unit [`startup`] installed.
///
/// Resolves its plan with no explicit home: a removal is addressed by the
/// unit's path and label, both of which come from the target user alone, and
/// nothing here reads the `$SHEP_HOME` the unit happens to carry.
pub fn unstartup(streams: &mut Streams<'_>, fmt: Format, args: &StartupArgs) -> ExitCode {
    match plan(None, args) {
        Ok(plan) => remove(streams, fmt, &plan, privilege()),
        Err(refusal) => refuse(streams, fmt, refusal.code, &refusal.message),
    }
}

/// The user a generated unit runs the daemon as: `--user` when given, else
/// `$SUDO_USER`, else the invoking user.
///
/// `$SUDO_USER` beats the invoking user rather than the other way round
/// because under `sudo shep startup` the invoking user IS root: a unit
/// resolved from it would supervise root's flock while the operator's stayed
/// down, and would look correct doing it.
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
/// `$HOME`: `sudo` resets that to root's, so a unit built from it would carry
/// `/root/.shep` and restore nothing after a reboot — silently, and months
/// later.
pub(crate) fn target_home(explicit: Option<&Path>, user_home: &Path) -> PathBuf {
    explicit.map_or_else(|| user_home.join(DEFAULT_HOME_DIR), Path::to_path_buf)
}

/// Writes and enables the unit, or prints the command that would.
///
/// Every refusal is decided before anything is written or run, and in this
/// order:
///
/// 1. A `$SHEP_HOME` that is not a directory is [`ExitCode::Usage`], naming
///    the path and `--home`. The overwhelmingly likely cause is the sudo
///    trap [`target_home`] describes, and a unit pointing at a home that is
///    not there is a reboot that restores nothing.
/// 2. A unit that already exists is [`ExitCode::Usage`] too, naming the path
///    and `unstartup`. An operator who edited the unit in place has to be
///    told rather than have the edits replaced — and on both init systems
///    rewriting the file does not change the service already loaded, so an
///    overwrite would leave the file and the running unit disagreeing.
///    `unstartup` then `startup` closes that gap; a `--force` flag would not.
/// 3. [`Privilege::Unprivileged`] prints the fully resolved
///    `sudo <exec> startup --user <user> --home <home>` and exits
///    [`ExitCode::Failure`] — non-zero so a script notices. shep never
///    escalates on its own.
///
/// Past that point the unit is going to be written, and if [`plan`] saw
/// `$SUDO_USER` set, [`secure_path_warning`] prints a notice naming the
/// `PATH` about to go into it — `sudo` on most distributions replaces
/// `PATH` with its own `secure_path` before shep ever runs, and that
/// substitution happens before shep is exec'd, so this is a "may have"
/// rather than a refusal: shep has no login `PATH` left to compare against,
/// only the operator does.
///
/// The privilege is a parameter rather than a `geteuid()` call in here for
/// the reason [`Privilege`]'s own doc gives, and `plan.unit_path` is a field
/// rather than a call to `unit::systemd_unit_path` for the matching one: a
/// test points it into a temporary directory.
pub(crate) fn install(
    streams: &mut Streams<'_>,
    fmt: Format,
    plan: &StartupPlan,
    privilege: Privilege,
) -> ExitCode {
    if !plan.spec.home.is_dir() {
        return refuse(
            streams,
            fmt,
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
            fmt,
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
            fmt,
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
        let _ = emit_notice(&mut *streams.err, fmt, "secure_path", &message);
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
    report(streams, fmt, "startup", steps)
}

/// The notice [`install`] prints when `$SUDO_USER` was set: `None` if it
/// was not, else a message naming `sudo_user` and showing `spec.path` in
/// full, so the operator can check it against that user's login `PATH`
/// without a second lookup.
///
/// A pure function of the two values rather than an environment read of its
/// own, matching [`target_user`]/[`target_home`]'s own shape: this crate is
/// `#![forbid(unsafe_code)]` (`cli.rs`), so nothing in its test suite can
/// call the `unsafe`-in-edition-2024 `std::env::set_var` to establish an
/// ambient `$SUDO_USER` and watch [`plan`] read it. Driving this function
/// directly with a resolved `sudo_user` is the next best thing, and the
/// thing that actually matters: the text the operator reads.
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
/// A unit that is not there is a success carrying one `absent` row, matching
/// `flush --daemon`'s treatment of a log file that is not there — and that
/// check runs BEFORE the privilege gate, so `shep unstartup` on a machine
/// that never ran `startup` answers plainly instead of demanding root to
/// discover there is nothing to do. Everything past it needs root, and
/// without it this prints `sudo <exec> unstartup --user <user>` and exits
/// [`ExitCode::Failure`]; the printed command carries no `--home`, since a
/// removal is addressed by the unit's path and label alone.
pub(crate) fn remove(
    streams: &mut Streams<'_>,
    fmt: Format,
    plan: &StartupPlan,
    privilege: Privilege,
) -> ExitCode {
    if !plan.unit_path.exists() {
        return report(
            streams,
            fmt,
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
            fmt,
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
    report(streams, fmt, "unstartup", steps)
}

/// Renders the unit and writes it at [`unit_mode`], as one step.
///
/// The mode is requested at `open` time rather than set afterwards, matching
/// `launch::launch_command`'s own discipline: a create-then-chmod sequence
/// leaves the file readable at whatever the ambient umask allowed until the
/// chmod lands, and this one is being written into a directory every user on
/// the machine can reach.
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
            // `open`'s mode is still masked by the caller's umask, and a
            // root shell with `umask 077` would leave a unit only root can
            // read — `systemctl cat shep-<user>`, run by the operator the
            // unit is FOR, could not. Setting it afterwards makes the
            // shipped mode deterministic; it acts on the open descriptor,
            // so there is no path to race, and it only ever widens from a
            // mode that was already no wider than this one.
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

/// `shep-<user>.service`, read back off the path the plan already resolved
/// rather than formatted a second time — `systemctl enable` wants the unit's
/// name, and two spellings of it could drift apart.
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
fn report(
    streams: &mut Streams<'_>,
    fmt: Format,
    command: &str,
    steps: Vec<StartupStep>,
) -> ExitCode {
    let failed = steps
        .iter()
        .any(|step| step.result != OK && step.result != ABSENT);
    let written = write_outcome(emit(
        &mut *streams.out,
        fmt,
        command,
        StartupSteps(steps),
        streams.style,
    ));
    if failed { ExitCode::Failure } else { written }
}

/// Quotes one word of a printed command so the line can be pasted rather
/// than read and repaired.
///
/// A `$SHEP_HOME` with a space in it is a legal path, and an unquoted one
/// would become two arguments — the operator would paste a command that
/// installs a unit carrying half the path they meant.
///
/// `pub(crate)` rather than private: [`unit::freebsd_rc_script`] and
/// [`unit::openbsd_rc_script`] reuse it as the single-quote former their own
/// doc comments describe, for a value that will be re-evaluated by a nested
/// shell — a different job from `unit`'s own double-quote escaper, and the
/// two compose there.
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
fn refuse(streams: &mut Streams<'_>, fmt: Format, code: ExitCode, message: &str) -> ExitCode {
    let _ = emit_error(&mut *streams.err, fmt, code.code_str(), message);
    code
}

/// Which init a Linux host running these two probes is on.
///
/// A pure function so the ORDER is testable on a machine that is not Linux.
/// systemd wins a tie: `/run/systemd/system` is exactly what `sd_booted(3)`
/// checks and is the only probe here with an upstream contract behind it,
/// and a host with both present is a host running systemd with openrc
/// leftovers rather than the other way round.
///
/// [`current_init`]'s Linux arm is the only non-test caller, and that arm is
/// `#[cfg]`-ed away on every other target — narrowed the same way the old
/// `Init` variants were before this task moved them, rather than
/// blanket-allowed.
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
/// Linux is a **runtime** probe: systemd and openrc share one target triple,
/// so `target_os` cannot tell them apart, and until this existed a Linux host
/// running openrc was silently written a systemd unit whose failure surfaced
/// only when `systemctl` turned out not to exist. The ordering lives in
/// [`linux_init`], which is compiled and tested everywhere; this function is
/// the two filesystem reads that feed it.
///
/// Every other target is a compile-time fact: there is nothing else macOS,
/// FreeBSD or OpenBSD could be.
///
/// **This is stricter than what it replaces.** A Linux container with no
/// `/run/systemd/system` used to get a systemd unit written into it and now
/// gets a refusal. That is the right answer — a unit with no init to read it
/// does nothing — but it is a case that worked before, so `--init` exists to
/// override this entirely.
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
            // Captured from THIS invocation, which is what makes an
            // interpreter installed under `~/.bun` or `~/.cargo` findable
            // after a reboot. An empty one is left empty rather than
            // guessed at: a unit carrying a PATH shep invented would fail
            // somewhere else entirely.
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

/// The target user's passwd home — the unit's working directory, and the
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

    /// fails if `$SUDO_USER` stops winning over the invoking user. Under
    /// `sudo shep startup` the invoking user IS root, so a resolution that
    /// ignored SUDO_USER would install a unit supervising root's flock
    /// while the operator's stayed down — and the unit would look correct.
    #[test]
    fn the_target_user_prefers_an_explicit_name_then_sudo_user() {
        assert_eq!(target_user(Some("deploy"), Some("rin"), "root"), "deploy");
        assert_eq!(target_user(None, Some("rin"), "root"), "rin");
        assert_eq!(target_user(None, None, "rin"), "rin");
    }

    /// fails if the home falls back to this process's `$HOME`. `sudo` resets
    /// HOME to root's, so a unit built from it carries /root/.shep and
    /// restores nothing after a reboot — the failure the whole gate exists
    /// to prevent, and one that surfaces months later.
    #[test]
    fn the_target_home_comes_from_the_target_user_not_the_invoker() {
        assert_eq!(
            target_home(None, Path::new("/home/rin")),
            Path::new("/home/rin/.shep")
        );
        assert_eq!(
            target_home(Some(Path::new("/srv/shep")), Path::new("/home/rin")),
            Path::new("/srv/shep")
        );
    }

    /// fails if the warning fires without `$SUDO_USER`, or if it fires with
    /// one and drops the name or the exact `PATH` about to be written. The
    /// first would warn an operator who never touched `sudo`; the second
    /// would leave the one who did with nothing to check the captured
    /// `PATH` against.
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
            secure_path_warning(Some("rin"), &spec).expect("$SUDO_USER was set, so this warns");
        assert!(message.contains("SUDO_USER=rin"), "{message}");
        assert!(
            message.contains("/usr/local/sbin:/usr/local/bin:/usr/bin:/bin"),
            "the full captured PATH must be readable without a second lookup: {message}"
        );
        assert!(
            message.contains("--preserve-env=PATH"),
            "the operator needs a way to get the PATH they meant: {message}"
        );
    }

    /// fails if an unprivileged startup exits 0, or prints a command the
    /// operator cannot paste. Exit 0 makes a script believe a unit was
    /// installed; a command missing --home re-runs the sudo trap the gate
    /// above exists to close.
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
                style: crate::style::StyleLevel::Bare,
            };
            install(
                &mut streams,
                Format::Table,
                &plan_for_test(&home),
                Privilege::Unprivileged,
            )
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

    /// fails if the secure-`PATH` warning is checked before the privilege
    /// gate rather than after it. Nothing is written on this path — there
    /// is no `PATH` yet to show — so a plan carrying `$SUDO_USER` must stay
    /// as silent about it as the plain refusal above.
    #[test]
    fn an_unprivileged_startup_under_sudo_still_prints_no_secure_path_warning() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let plan = StartupPlan {
            sudo_user: Some("rin".to_string()),
            ..plan_for_test(&home)
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::StyleLevel::Bare,
            };
            install(&mut streams, Format::Table, &plan, Privilege::Unprivileged);
        }
        let printed = String::from_utf8(err).unwrap();
        assert!(!printed.contains("secure_path"), "{printed}");
    }

    /// fails if a `$SHEP_HOME` that does not exist is accepted. That is what
    /// the sudo trap produces when nobody notices it, and the unit it yields
    /// is one that boots cleanly and restores an empty flock.
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
                style: crate::style::StyleLevel::Bare,
            };
            // Root, deliberately: an unprivileged run would refuse for the
            // other reason and this case would pass without ever exercising
            // the home check.
            install(
                &mut streams,
                Format::Table,
                &plan_for_test(&missing),
                Privilege::Root,
            )
        };
        assert_eq!(code, ExitCode::Usage);
        let printed = String::from_utf8(err).unwrap();
        assert!(printed.contains(missing.to_str().unwrap()), "{printed}");
        assert!(
            !plan_for_test(&missing).unit_path.exists(),
            "a refused startup writes no unit"
        );
    }

    /// fails if an existing unit is overwritten. An operator who edited the
    /// unit in place has to be told, not have the edits replaced — and on
    /// both init systems a rewritten file does not change the service that
    /// is already loaded, so an overwrite would leave the file and the
    /// running unit disagreeing.
    ///
    /// `Privilege::Root`, for `a_shep_home_that_does_not_exist_is_refused`'s
    /// reason: unprivileged would refuse for the other reason and this case
    /// would never reach the check.
    #[test]
    fn an_existing_unit_is_refused_rather_than_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".shep");
        std::fs::create_dir_all(&home).unwrap();
        let plan = StartupPlan {
            sudo_user: Some("rin".to_string()),
            ..plan_for_test(&home)
        };
        std::fs::write(&plan.unit_path, "# hand-edited\n").unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::StyleLevel::Bare,
            };
            install(&mut streams, Format::Table, &plan, Privilege::Root)
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

    /// fails if `unstartup` on a machine that never ran `startup` reports a
    /// failure instead of an `absent` row — the same treatment
    /// `flush --daemon` gives a log file that is not there.
    ///
    /// `Privilege::Unprivileged` deliberately: the absence check runs before
    /// the privilege gate, so this passes only while that order holds, and
    /// a `Privilege::Root` plan here would reach a real `systemctl` the
    /// moment somebody swapped the two.
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
                style: crate::style::StyleLevel::Bare,
            };
            remove(
                &mut streams,
                Format::Table,
                &plan_for_test(&home),
                Privilege::Unprivileged,
            )
        };
        assert_eq!(code, ExitCode::Success);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains(ABSENT), "{printed}");
    }

    /// fails if an unprivileged unstartup exits 0, prints a command the
    /// operator cannot paste, or removes the unit anyway. The last is the
    /// one that matters: this verb's whole job is destructive.
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
                style: crate::style::StyleLevel::Bare,
            };
            remove(&mut streams, Format::Table, &plan, Privilege::Unprivileged)
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

    /// fails if the unit is written with the wrong bytes or a mode other
    /// than 0644. A unit an init system cannot read is a boot that restores
    /// nothing; a world-writable one under `/etc` is worse than that.
    ///
    /// Drives `write_unit` and `remove_unit` directly rather than `install`:
    /// `install`'s privileged path runs `systemctl`, which no test in this
    /// phase may reach.
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
        // A literal, deliberately, not `unit_mode(Init::Systemd)`: a test
        // comparing the function's own return value against itself passes
        // for whatever that value is changed to, and a mutation to 0o666
        // went uncaught by exactly that until this line stopped naming it.
        assert_eq!(mode & 0o777, 0o644, "mode was {:o}", mode & 0o777);

        assert_eq!(remove_unit(&plan).result, OK);
        assert!(!plan.unit_path.exists());
        assert_eq!(
            remove_unit(&plan).result,
            ABSENT,
            "removing what is already gone is the state that was asked for"
        );
    }

    /// fails if a failed step stops failing the verb, or if a failure
    /// truncates the report. Both halves are the rule [`report`] exists for:
    /// a half-installed unit is worse than a fully-attempted one, so every
    /// step still runs and still prints, and the operator needs a non-zero
    /// exit to know which half they are holding.
    ///
    /// Drives `report` with hand-built rows rather than a real `install`:
    /// producing a genuinely failing step would mean running a real
    /// `systemctl`, which no test in this crate may.
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
                style: crate::style::StyleLevel::Bare,
            };
            report(&mut streams, Format::Table, "startup", steps)
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

    /// fails if a printed command stops being paste-able. A `$SHEP_HOME`
    /// with a space in it is legal and would otherwise become two arguments
    /// — the operator would paste a command that installs a unit carrying
    /// half the path they meant.
    #[test]
    fn a_printed_command_quotes_what_a_shell_would_split() {
        assert_eq!(shell_quote("/home/rin/.shep"), "/home/rin/.shep");
        assert_eq!(shell_quote("/opt/my shep"), "'/opt/my shep'");
        assert_eq!(shell_quote("rin's"), r"'rin'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    /// fails if a failed step reports something other than the first line
    /// its command complained with. systemd answers a refusal in several
    /// lines of its own advice and a row is one line; a step reporting an
    /// empty string would say a command failed and not say how.
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

    /// fails if a systemd unit or launchd plist stops being read-only, or an
    /// openrc/rc.d script stops being executable. A unit an init system
    /// cannot read is a boot that restores nothing; a script that is not
    /// executable fails at the next reboot, the worst possible time to
    /// find out.
    #[test]
    fn the_mode_is_read_only_for_units_and_executable_for_scripts() {
        assert_eq!(unit_mode(Init::Systemd), 0o644);
        assert_eq!(unit_mode(Init::Launchd), 0o644);
        assert_eq!(unit_mode(Init::Openrc), 0o755);
        assert_eq!(unit_mode(Init::FreebsdRc), 0o755);
        assert_eq!(unit_mode(Init::OpenbsdRc), 0o755);
    }

    /// fails if the probe order ever flips. systemd wins a tie because
    /// `/run/systemd/system` is the check `sd_booted(3)` makes; a host with
    /// both is a systemd host with openrc leftovers. Untestable as a
    /// filesystem probe on this machine — which is the whole reason the
    /// ordering is a pure function.
    #[test]
    fn systemd_wins_when_both_linux_probes_are_true() {
        assert_eq!(linux_init(true, true), Some(Init::Systemd));
        assert_eq!(linux_init(true, false), Some(Init::Systemd));
        assert_eq!(linux_init(false, true), Some(Init::Openrc));
        assert_eq!(linux_init(false, false), None);
    }

    /// fails if `--init` stops overriding detection — the escape hatch for a
    /// container with no /run/systemd/system, and the only way a macOS
    /// machine renders a systemd unit at all.
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

    /// fails if `--init` stops choosing the unit PATH — which is the half
    /// that matters for `unstartup`. A unit installed under one init has to
    /// be removable after the host has changed to another, and that is a
    /// claim about which file gets removed, not about which struct the two
    /// verbs share. (`Startup` and `Unstartup` both take `StartupArgs`, so a
    /// test that only checked that both parse `--init` could barely fail.)
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

    /// fails if a username `rcvar`/`rcctl` cannot turn into a shell variable
    /// stops being refused, or if a safe one is refused by mistake. `web-app`
    /// and `deploy.svc` are both legal usernames and both illegal shell
    /// variable fragments; a script built from one fails at
    /// `load_rc_config` naming a line number rather than the user.
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
