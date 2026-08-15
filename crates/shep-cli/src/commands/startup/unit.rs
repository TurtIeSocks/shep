//! Rendering a systemd unit and a launchd plist for the daemon.
//!
//! Both renderers are pure `format!` over a [`UnitSpec`]: no filesystem
//! access, no environment reads, nothing that could fail. Resolving a real
//! `UnitSpec` — reading `$PATH`, this binary's own path, the target user —
//! and writing the result to disk is the parent module's.
//!
//! `ExecStart` names the daemon itself (`<exec> daemon --foreground`), not
//! `shep muster`. Under `Type=notify` systemd supervises the process it
//! starts, so `ExecStart=shep muster` would have systemd supervising a
//! client that talks to a daemon and exits immediately — the restore still
//! happens, because the daemon restores the roll at boot on its own
//! (decision 14). `--foreground` is on both renderers' argv for the same
//! reason: launchd has no readiness protocol, so `$NOTIFY_SOCKET` is unset
//! there and `shep_daemon::notify::notify_ready` reports `Ok(false)`, but
//! the flag stays so both platforms invoke the daemon through the same
//! documented entry point rather than one of them depending on the bare
//! hidden `daemon` verb's private contract (decision 15).

use std::ffi::OsString;
use std::path::PathBuf;

/// Everything a generated init unit carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnitSpec {
    /// The user the daemon runs as.
    pub user: String,
    /// This binary's own resolved path.
    pub exec: PathBuf,
    /// `$SHEP_HOME` the daemon is given.
    pub home: PathBuf,
    /// `PATH` captured from the invoking environment — the mechanism that
    /// makes an interpreter installed under `~/.bun` or `~/.cargo` findable
    /// after a reboot.
    pub path: OsString,
    /// The daemon's working directory.
    pub working_dir: PathBuf,
}

/// Renders the systemd unit, `Type=notify`.
pub(crate) fn systemd_unit(spec: &UnitSpec) -> String {
    let home = systemd_environment_value(&spec.home.display().to_string());
    let path = systemd_environment_value(&spec.path.to_string_lossy());
    format!(
        "[Unit]\n\
         Description=shep process manager for {user}\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=notify\n\
         NotifyAccess=main\n\
         User={user}\n\
         WorkingDirectory={working_dir}\n\
         Environment=\"SHEP_HOME={home}\"\n\
         Environment=\"PATH={path}\"\n\
         ExecStart={exec} daemon --foreground\n\
         ExecReload={exec} reload all\n\
         ExecStop={exec} kill\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        user = spec.user,
        working_dir = spec.working_dir.display(),
        exec = spec.exec.display(),
    )
}

/// Renders the launchd plist. `KeepAlive`/`SuccessfulExit=false` is
/// launchd's `Restart=on-failure`; launchd has no `ExecReload` equivalent,
/// so a reload goes through `shep reload all` same as any other client.
pub(crate) fn launchd_plist(spec: &UnitSpec) -> String {
    let label = xml_text(&launchd_label(&spec.user));
    let exec = xml_text(&spec.exec.display().to_string());
    let user = xml_text(&spec.user);
    let working_dir = xml_text(&spec.working_dir.display().to_string());
    let home = xml_text(&spec.home.display().to_string());
    let path = xml_text(&spec.path.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key><string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{exec}</string>\n\
         \t\t<string>daemon</string>\n\
         \t\t<string>--foreground</string>\n\
         \t</array>\n\
         \t<key>UserName</key><string>{user}</string>\n\
         \t<key>WorkingDirectory</key><string>{working_dir}</string>\n\
         \t<key>EnvironmentVariables</key>\n\
         \t<dict>\n\
         \t\t<key>SHEP_HOME</key><string>{home}</string>\n\
         \t\t<key>PATH</key><string>{path}</string>\n\
         \t</dict>\n\
         \t<key>RunAtLoad</key><true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<dict><key>SuccessfulExit</key><false/></dict>\n\
         \t<key>StandardOutPath</key><string>{home}/logs/shepd.out.log</string>\n\
         \t<key>StandardErrorPath</key><string>{home}/logs/shepd.err.log</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// `/etc/systemd/system/shep-<user>.service`.
pub(crate) fn systemd_unit_path(user: &str) -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/shep-{user}.service"))
}

/// `io.github.turtiesocks.shep.<user>` — the launchd label, also the plist's
/// own filename stem via [`launchd_plist_path`] and the job label
/// `launchctl bootout system/<label>` names.
pub(crate) fn launchd_label(user: &str) -> String {
    format!("io.github.turtiesocks.shep.{user}")
}

/// `/Library/LaunchDaemons/<label>.plist`.
pub(crate) fn launchd_plist_path(user: &str) -> PathBuf {
    PathBuf::from(format!(
        "/Library/LaunchDaemons/{}.plist",
        launchd_label(user)
    ))
}

/// Escapes one systemd `Environment=` value: doubles every `%`, which
/// systemd otherwise expands as a specifier (`%h`, `%t`, ...) — a real
/// captured `PATH` can contain one by coincidence (`/pct%dir/bin` is a
/// legal POSIX path), and the expansion is silent rather than a parse
/// error. The caller wraps the whole `KEY=value` assignment in `"..."`, so
/// a value containing a space needs nothing further from this function.
fn systemd_environment_value(value: &str) -> String {
    value.replace('%', "%%")
}

/// Escapes plist string content: `&` first (so it cannot re-escape the
/// entities this function just produced), then `<` and `>`. All three are
/// XML metacharacters that end the current element early — a raw `&` in a
/// path (legal in a POSIX filename) makes the whole plist unparseable, and
/// launchd's own refusal names the file, not the character.
fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Renders the openrc init script.
///
/// `supervise-daemon` (openrc >= 0.21) rather than `start-stop-daemon`,
/// because it supervises the foreground process the way systemd does rather
/// than daemonizing and tracking a pidfile — and `shep daemon --foreground`
/// is the entry point both other renderers already use.
///
/// **openrc has no `sd_notify` analogue.** Nothing tells openrc the shepherd
/// is ready; `supervise-daemon` marks the service started the instant the
/// process is spawned, which is before the muster restore has finished. The
/// `start_post` poll below is what closes that gap, and it is not a
/// consolation prize: it proves exactly what `READY=1` proves. `boot` binds
/// the control socket at step 2, restores the roll and starts the dogs at
/// step 4, and `RpcServer` — the thing that *accepts* on that listener — is
/// constructed afterwards, in `run`. So a connection lands in the backlog
/// immediately but **no request is answered until after the restore**, and
/// the first answered `shep flock` is the same milestone, one step later.
/// `shep flock` also routes through `connect_client`, which never spawns, so
/// the poll cannot start a shepherd of its own.
/// Do not "simplify" the poll away on the assumption that it is a guess.
///
/// Every interpolated value goes through [`sh_double_quoted`]: these land
/// inside double-quoted assignments in a script that runs as root at boot.
pub(crate) fn openrc_script(spec: &UnitSpec) -> String {
    let user = sh_double_quoted(&spec.user);
    let exec = sh_double_quoted(&spec.exec.display().to_string());
    let home = sh_double_quoted(&spec.home.display().to_string());
    let path = sh_double_quoted(&spec.path.to_string_lossy());
    let working_dir = sh_double_quoted(&spec.working_dir.display().to_string());
    format!(
        "#!/sbin/openrc-run\n\
         # shep process manager for {user}\n\
         #\n\
         # openrc has no sd_notify analogue, so the readiness gap systemd's\n\
         # Type=notify closes is closed here by start_post asking the shepherd\n\
         # itself. The first answered request proves the muster restore finished:\n\
         # shep binds its control socket before the restore but does not accept on\n\
         # it until after.\n\
         \n\
         name=\"shep-{user}\"\n\
         description=\"shep process manager for {user}\"\n\
         supervisor=\"supervise-daemon\"\n\
         command=\"{exec}\"\n\
         command_args=\"daemon --foreground\"\n\
         command_user=\"{user}\"\n\
         directory=\"{working_dir}\"\n\
         pidfile=\"/run/shep-{user}.pid\"\n\
         respawn_delay=5\n\
         output_log=\"{home}/logs/shepd.out.log\"\n\
         error_log=\"{home}/logs/shepd.err.log\"\n\
         \n\
         export SHEP_HOME=\"{home}\"\n\
         export PATH=\"{path}\"\n\
         \n\
         depend() {{\n\
         \tneed net\n\
         }}\n\
         \n\
         # start_post runs as root, and the control socket lives in a 0700 $SHEP_HOME\n\
         # owned by {user}. Root bypasses that, so the poll works; it looks like a\n\
         # permission bug only until you have thought it through.\n\
         start_post() {{\n\
         \tlocal waited=0\n\
         \twhile [ \"${{waited}}\" -lt 60 ]; do\n\
         \t\tif \"{exec}\" --home \"{home}\" flock >/dev/null 2>&1; then\n\
         \t\t\treturn 0\n\
         \t\tfi\n\
         \t\tsleep 1\n\
         \t\twaited=$((waited + 1))\n\
         \tdone\n\
         \teerror \"shep did not answer on its control socket within 60s\"\n\
         \treturn 1\n\
         }}\n"
    )
}

/// Escapes a value that lands INSIDE a double-quoted shell assignment:
/// `"`, `$`, `` ` `` and `\` get a backslash.
///
/// Not the same function as [`super::shell_quote`], and the two must not be
/// folded together: `shell_quote` produces a standalone single-quoted *word*
/// for a human to paste into a terminal, while this escapes *content* that
/// is already inside double quotes. Where a value will additionally be
/// re-evaluated by a shell — OpenBSD's `rc_start` string, FreeBSD's
/// `${name}_env` — the two compose, innermost first:
/// `sh_double_quoted(shell_quote(value))`.
fn sh_double_quoted(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '"' | '$' | '`' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cli::Init;
    use crate::commands::startup::unit_path_for;

    fn spec() -> UnitSpec {
        UnitSpec {
            user: "deploy".to_string(),
            exec: PathBuf::from("/usr/local/bin/shep"),
            home: PathBuf::from("/home/deploy/.shep"),
            path: OsString::from("/home/deploy/.bun/bin:/usr/local/bin:/usr/bin:/bin"),
            working_dir: PathBuf::from("/home/deploy"),
        }
    }

    /// The systemd unit, byte for byte, exactly as the brief specifies it —
    /// the strongest check available: a `.contains` assertion proves a
    /// substring survived, not that nothing else drifted (an extra stray
    /// line, wrong section order, a missing blank line between sections).
    /// Every value below round-trips through the real formatter with no
    /// escaping needed, so this also pins the unescaped happy path.
    #[test]
    fn the_systemd_unit_matches_the_spec_exactly() {
        let unit = systemd_unit(&spec());
        assert_eq!(
            unit,
            "[Unit]\n\
             Description=shep process manager for deploy\n\
             After=network.target\n\
             \n\
             [Service]\n\
             Type=notify\n\
             NotifyAccess=main\n\
             User=deploy\n\
             WorkingDirectory=/home/deploy\n\
             Environment=\"SHEP_HOME=/home/deploy/.shep\"\n\
             Environment=\"PATH=/home/deploy/.bun/bin:/usr/local/bin:/usr/bin:/bin\"\n\
             ExecStart=/usr/local/bin/shep daemon --foreground\n\
             ExecReload=/usr/local/bin/shep reload all\n\
             ExecStop=/usr/local/bin/shep kill\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            "{unit}"
        );
    }

    /// The launchd plist, byte for byte, exactly as the brief specifies it —
    /// same rationale as the systemd exact-match test above: a `.contains`
    /// check cannot see a swapped tag order or a missing sibling key.
    #[test]
    fn the_launchd_plist_matches_the_spec_exactly() {
        let plist = launchd_plist(&spec());
        assert_eq!(
            plist,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \t<key>Label</key><string>io.github.turtiesocks.shep.deploy</string>\n\
             \t<key>ProgramArguments</key>\n\
             \t<array>\n\
             \t\t<string>/usr/local/bin/shep</string>\n\
             \t\t<string>daemon</string>\n\
             \t\t<string>--foreground</string>\n\
             \t</array>\n\
             \t<key>UserName</key><string>deploy</string>\n\
             \t<key>WorkingDirectory</key><string>/home/deploy</string>\n\
             \t<key>EnvironmentVariables</key>\n\
             \t<dict>\n\
             \t\t<key>SHEP_HOME</key><string>/home/deploy/.shep</string>\n\
             \t\t<key>PATH</key><string>/home/deploy/.bun/bin:/usr/local/bin:/usr/bin:/bin</string>\n\
             \t</dict>\n\
             \t<key>RunAtLoad</key><true/>\n\
             \t<key>KeepAlive</key>\n\
             \t<dict><key>SuccessfulExit</key><false/></dict>\n\
             \t<key>StandardOutPath</key><string>/home/deploy/.shep/logs/shepd.out.log</string>\n\
             \t<key>StandardErrorPath</key><string>/home/deploy/.shep/logs/shepd.err.log</string>\n\
             </dict>\n\
             </plist>\n",
            "{plist}"
        );
    }

    /// fails if any of the four ExecStart/Reload/Stop/Type lines drifts.
    /// Each is load-bearing: Type=notify is what makes the unit go green on
    /// a restored flock, and an ExecStart naming `muster` would have systemd
    /// supervising a client that exits immediately.
    #[test]
    fn the_systemd_unit_carries_the_four_lines_that_matter() {
        let unit = systemd_unit(&spec());
        assert!(unit.contains("Type=notify"), "{unit}");
        assert!(
            unit.contains("ExecStart=/usr/local/bin/shep daemon --foreground"),
            "{unit}"
        );
        assert!(
            unit.contains("ExecReload=/usr/local/bin/shep reload all"),
            "{unit}"
        );
        assert!(unit.contains("ExecStop=/usr/local/bin/shep kill"), "{unit}");
        assert!(unit.contains("WantedBy=multi-user.target"), "{unit}");
    }

    /// fails if an Environment value stops being quoted, or a `%` stops
    /// being escaped. A PATH with a space silently truncates at the space;
    /// a `%` is a systemd specifier and expands to something else entirely.
    /// Both are reachable from a real captured PATH, and neither is visible
    /// until an interpreter is not found after a reboot.
    #[test]
    fn environment_values_are_quoted_and_specifier_escaped() {
        let mut spec = spec();
        spec.path = OsString::from("/opt/my tools/bin:/usr/bin:/pct%dir/bin");
        let unit = systemd_unit(&spec);
        assert!(
            unit.contains(r#"Environment="PATH=/opt/my tools/bin:/usr/bin:/pct%%dir/bin""#),
            "{unit}"
        );
    }

    /// fails if plist values stop being XML-escaped. A `&` in a path makes
    /// the whole plist unparseable, and launchd's refusal names the file
    /// rather than the character.
    #[test]
    fn plist_values_are_xml_escaped() {
        let mut spec = spec();
        spec.home = PathBuf::from("/home/r&d/.shep");
        let plist = launchd_plist(&spec);
        assert!(
            plist.contains("<string>/home/r&amp;d/.shep</string>"),
            "{plist}"
        );
        assert!(
            !plist.contains("r&d"),
            "a raw ampersand makes the plist unparseable"
        );
    }

    /// `systemd_unit_path`/`launchd_label`/`launchd_plist_path` are simple
    /// format strings, but the verb that installs and removes a unit
    /// addresses it by exactly these three — `systemctl enable` by the
    /// file's name, `launchctl bootout` by the label — so they get the same
    /// exact-match treatment as the two renderers above.
    #[test]
    fn the_install_paths_and_label_match_the_spec_exactly() {
        assert_eq!(
            systemd_unit_path("deploy"),
            PathBuf::from("/etc/systemd/system/shep-deploy.service")
        );
        assert_eq!(launchd_label("deploy"), "io.github.turtiesocks.shep.deploy");
        assert_eq!(
            launchd_plist_path("deploy"),
            PathBuf::from("/Library/LaunchDaemons/io.github.turtiesocks.shep.deploy.plist")
        );
    }

    /// Probes `systemd-analyze`'s two conventional install paths with
    /// `Path::exists` rather than shelling out to `which` — one fewer
    /// process, and no dependence on the test's own `$PATH`.
    fn which_systemd_analyze() -> Result<PathBuf, ()> {
        for candidate in ["/usr/bin/systemd-analyze", "/bin/systemd-analyze"] {
            let path = Path::new(candidate);
            if path.exists() {
                return Ok(path.to_path_buf());
            }
        }
        Err(())
    }

    /// fails if `systemd-analyze verify` rejects the generated unit —
    /// systemd's own parser is the only thing that can say the unit is
    /// well-formed, and every assertion above is our opinion of it.
    ///
    /// Skips, loudly, where the tool does not exist: this is a macOS
    /// development machine's ordinary state, and a test that failed there
    /// would be disabled rather than fixed. On the Linux CI leg it runs.
    #[test]
    fn systemd_analyze_accepts_the_generated_unit() {
        let Ok(analyze) = which_systemd_analyze() else {
            eprintln!("skipping: systemd-analyze is not on this machine");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep-deploy.service");
        std::fs::write(&path, systemd_unit(&spec())).unwrap();
        let out = std::process::Command::new(analyze)
            .arg("verify")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "systemd-analyze verify rejected the unit:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// fails if the readiness poll is ever dropped or unbounded. openrc's
    /// only honest answer to Type=notify.
    #[test]
    fn the_openrc_script_polls_for_readiness_and_bounds_the_wait() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains("start_post()"));
        assert!(rendered.contains("-lt 60"), "the poll must be bounded");
        assert!(rendered.contains("flock >/dev/null"));
        assert!(
            rendered.contains("return 1"),
            "a timeout must fail the service"
        );
    }

    /// fails if the comment explaining WHY the poll is equivalent to
    /// READY=1 is deleted. Phase 12a shipped two false captions in a
    /// generated artefact because only one of them was pinned; generated
    /// prose that makes a claim gets a test.
    #[test]
    fn the_openrc_script_says_why_it_polls() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains("openrc has no sd_notify analogue"));
        assert!(rendered.contains("binds its control socket before the restore"));
    }

    /// fails if a metacharacter in a path escapes the double quotes.
    #[test]
    fn the_openrc_script_quotes_shell_metacharacters() {
        let mut s = spec();
        s.home = PathBuf::from(r#"/tmp/we"ird/$HOME/`x`/back\slash"#);
        let rendered = openrc_script(&s);
        assert!(
            rendered.contains(r#"we\"ird"#),
            "a quote must be escaped: {rendered}"
        );
        assert!(
            rendered.contains(r"\$HOME"),
            "a dollar must be escaped: {rendered}"
        );
        assert!(
            rendered.contains(r"\`x\`"),
            "a backtick must be escaped: {rendered}"
        );
        assert!(
            rendered.contains(r"back\\slash"),
            "a backslash must be escaped: {rendered}"
        );
    }

    /// fails if the service name stops matching the file name — openrc
    /// derives defaults from `name`/`RC_SVCNAME`, and a constant `name`
    /// would make two users on one host collide while owning distinct files.
    #[test]
    fn the_openrc_name_is_per_user_and_matches_the_file() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains(r#"name="shep-deploy""#), "{rendered}");
        assert_eq!(
            unit_path_for(Init::Openrc, "deploy"),
            PathBuf::from("/etc/init.d/shep-deploy")
        );
    }

    #[test]
    fn the_openrc_script_is_the_same_entry_point_as_the_other_two() {
        let rendered = openrc_script(&spec());
        assert!(rendered.contains(r#"command_args="daemon --foreground""#));
    }
}
