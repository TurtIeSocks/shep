//! Rendering the five init-system units shep can start at boot: a systemd
//! unit, a launchd plist, an openrc script, and the FreeBSD and OpenBSD
//! `rc.d` scripts.
//!
//! Every renderer is pure `format!` over a [`UnitSpec`]: no filesystem
//! access, no environment reads, nothing that could fail. Resolving a real
//! `UnitSpec` and writing the result to disk is the parent module's job.
//!
//! Every unit's argv is `<exec> daemon --foreground`, never `shep muster`:
//! an init system supervises the process it starts, and `muster` is a client
//! that exits immediately. The daemon restores the roll at boot on its own.

use std::ffi::OsString;
use std::path::PathBuf;

use super::shell_quote;

/// Everything a generated init unit carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnitSpec {
    /// The user the daemon runs as.
    pub user: String,
    /// This binary's own resolved path.
    pub exec: PathBuf,
    /// `$SHEP_HOME` the daemon is given.
    pub home: PathBuf,
    /// `PATH` captured from the invoking environment, so an interpreter under
    /// `~/.bun` or `~/.cargo` is still findable after a reboot.
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

/// `io.github.turtiesocks.shep.<user>`: the launchd label, the plist's own
/// filename stem, and what `launchctl bootout system/<label>` names.
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

/// Escapes one systemd `Environment=` value: doubles every `%`, which systemd
/// otherwise expands silently as a specifier (`%h`, `%t`, ...).
///
/// The caller wraps the whole `KEY=value` in `"..."`, so a value containing a
/// space needs nothing further here.
fn systemd_environment_value(value: &str) -> String {
    value.replace('%', "%%")
}

/// Escapes plist string content: `&` first, so it cannot re-escape the
/// entities this function just produced, then `<` and `>`.
fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Renders the openrc init script.
///
/// `supervise-daemon` (openrc >= 0.21) supervises the foreground process
/// like systemd, instead of tracking a pidfile.
///
/// openrc has no `sd_notify` analogue: `start_post` polls `shep flock`
/// instead, since the daemon binds its control socket before restoring
/// the muster roll but does not accept until after. `flock` routes
/// through `connect_client`, which never spawns a shepherd of its own.
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

/// Renders the FreeBSD `rc.d` script, `/usr/local/etc/rc.d/shep_<user>`.
///
/// `start_postcmd` polls the shepherd's own control socket, since
/// `daemon(8)` reports the service started as soon as it forks, before
/// the muster restore runs. `spec.user` must already pass
/// [`super::is_rc_safe_user`]: `name`, `rcvar` and `shep_<user>_*` are
/// shell identifiers, interpolated raw; any other single-shell value
/// uses [`sh_double_quoted`] alone.
///
/// `${name}_env` and `command_args` are re-read by a second shell or
/// program (`env(1)`'s word-split, `daemon(8)`'s argv): `home`, `path`
/// and `exec` there are `sh_double_quoted(shell_quote(value))` instead.
pub(crate) fn freebsd_rc_script(spec: &UnitSpec) -> String {
    let user = &spec.user;
    let exec = sh_double_quoted(&spec.exec.display().to_string());
    let exec_arg = sh_double_quoted(&shell_quote(&spec.exec.display().to_string()));
    let home = sh_double_quoted(&spec.home.display().to_string());
    let working_dir = sh_double_quoted(&spec.working_dir.display().to_string());
    let home_env = sh_double_quoted(&shell_quote(&spec.home.display().to_string()));
    let path_env = sh_double_quoted(&shell_quote(&spec.path.to_string_lossy()));
    format!(
        "#!/bin/sh\n\
         #\n\
         # PROVIDE: shep_{user}\n\
         # REQUIRE: LOGIN NETWORKING\n\
         # KEYWORD: shutdown\n\
         #\n\
         # Enable with: sysrc shep_{user}_enable=YES\n\
         \n\
         . /etc/rc.subr\n\
         \n\
         name=\"shep_{user}\"\n\
         rcvar=\"shep_{user}_enable\"\n\
         : ${{shep_{user}_enable:=\"NO\"}}\n\
         \n\
         shep_{user}_user=\"{user}\"\n\
         shep_{user}_chdir=\"{working_dir}\"\n\
         shep_{user}_env=\"SHEP_HOME={home_env} PATH={path_env}\"\n\
         \n\
         pidfile=\"/var/run/shep_{user}.pid\"\n\
         command=\"/usr/sbin/daemon\"\n\
         command_args=\"-P ${{pidfile}} -r -f {exec_arg} daemon --foreground\"\n\
         \n\
         start_postcmd=\"shep_{user}_poststart\"\n\
         \n\
         # rc.subr reports the service started as soon as daemon(8) has forked, which\n\
         # is before the shepherd has finished restoring the muster roll. This waits\n\
         # for the shepherd to answer on its own control socket, which is the same\n\
         # milestone systemd's READY=1 reports.\n\
         shep_{user}_poststart()\n\
         {{\n\
         \t_waited=0\n\
         \twhile [ ${{_waited}} -lt 60 ]; do\n\
         \t\tif \"{exec}\" --home \"{home}\" flock >/dev/null 2>&1; then\n\
         \t\t\treturn 0\n\
         \t\tfi\n\
         \t\tsleep 1\n\
         \t\t_waited=$((_waited + 1))\n\
         \tdone\n\
         \techo \"shep did not answer on its control socket within 60s\" >&2\n\
         \treturn 1\n\
         }}\n\
         \n\
         load_rc_config $name\n\
         run_rc_command \"$1\"\n"
    )
}

/// Renders the OpenBSD `rc.d` script, `/etc/rc.d/shep_<user>`.
///
/// Like [`freebsd_rc_script`], `spec.user` must already pass
/// [`super::is_rc_safe_user`] and is interpolated raw as an identifier.
/// OpenBSD's `rc.subr(8)` has no post-start hook: `rc_pre` runs before
/// `start`, `rc_post` after *stop*; `shep --home <home> flock` is the
/// manual readiness check instead.
///
/// The environment is passed as `VAR=value` prefixes inside the string
/// `rc_exec` hands to `su -fl ... -c "..."`, not exported: `su -l`
/// discards the environment. `su`'s spawned shell reads `home`, `path`
/// and `exec` a second time: `sh_double_quoted(shell_quote(value))`.
pub(crate) fn openbsd_rc_script(spec: &UnitSpec) -> String {
    let user = &spec.user;
    let exec = sh_double_quoted(&shell_quote(&spec.exec.display().to_string()));
    let working_dir = sh_double_quoted(&spec.working_dir.display().to_string());
    let home_display = spec.home.display();
    let home_env = sh_double_quoted(&shell_quote(&spec.home.display().to_string()));
    let path_env = sh_double_quoted(&shell_quote(&spec.path.to_string_lossy()));
    format!(
        "#!/bin/ksh\n\
         #\n\
         # shep process manager for {user}\n\
         #\n\
         # Enable with: rcctl enable shep_{user} && rcctl start shep_{user}\n\
         #\n\
         # OpenBSD's rc.subr has no post-start hook: rc_pre runs before the daemon\n\
         # starts and rc_post runs after it stops. So this script reports the service\n\
         # started as soon as the shepherd process is spawned, which is BEFORE the\n\
         # muster restore has finished — the flock may still be coming back. There is\n\
         # no readiness protocol here and this script does not pretend to one. Check\n\
         # with: shep --home {home_display} flock\n\
         \n\
         daemon=\"{exec}\"\n\
         daemon_flags=\"daemon --foreground\"\n\
         daemon_user=\"{user}\"\n\
         daemon_execdir=\"{working_dir}\"\n\
         \n\
         . /etc/rc.d/rc.subr\n\
         \n\
         rc_bg=YES\n\
         rc_reload=NO\n\
         \n\
         # su(1)'s -l flag, which rc_exec always passes, discards the environment\n\
         # rc.subr itself ran in — so exporting SHEP_HOME/PATH above would never reach\n\
         # the daemon. Prefixing them onto the string rc_exec hands to su is the only\n\
         # way they survive. Single-quoted for the shell that evaluates that string,\n\
         # then escaped again for the double-quoted context here.\n\
         rc_start() {{\n\
         \trc_exec \"SHEP_HOME={home_env} PATH={path_env} ${{daemon}} ${{daemon_flags}}\"\n\
         }}\n\
         \n\
         rc_cmd $1\n"
    )
}

/// Escapes a value that lands inside a double-quoted shell assignment:
/// `"`, `$`, `` ` `` and `\` get a backslash.
///
/// Distinct from [`super::shell_quote`]: that produces a standalone
/// single-quoted word for a human to paste, this escapes content already
/// inside double quotes. Where a value is re-evaluated by a second shell
/// (OpenBSD's `rc_start` string, FreeBSD's `${name}_env`), the two
/// compose, innermost first: `sh_double_quoted(shell_quote(value))`.
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

    /// Byte for byte, not `.contains`: a substring match can't catch a
    /// stray line, wrong section order, or a missing blank line between
    /// sections. Every value here round-trips with no escaping needed, so
    /// this also pins the unescaped happy path.
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

    /// Byte for byte, same rationale as the systemd exact-match test above:
    /// `.contains` cannot see a swapped tag order or a missing sibling key.
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
    /// format strings, but `systemctl enable` and `launchctl bootout`
    /// address a unit by exactly these, so they get the same exact-match
    /// treatment as the two renderers above.
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
    /// `Path::exists` rather than shelling out to `which`: one fewer
    /// process, no dependence on the test's own `$PATH`.
    fn which_systemd_analyze() -> Result<PathBuf, ()> {
        for candidate in ["/usr/bin/systemd-analyze", "/bin/systemd-analyze"] {
            let path = Path::new(candidate);
            if path.exists() {
                return Ok(path.to_path_buf());
            }
        }
        Err(())
    }

    /// systemd's own parser is the only thing that can say the generated
    /// unit is well-formed; every assertion above is only our opinion of
    /// it. Skips, loudly, when the tool is absent (an ordinary macOS
    /// state); runs on the Linux CI leg.
    ///
    /// Builds its own spec rather than [`spec`]'s: `verify` resolves
    /// `ExecStart`/`ExecReload`/`ExecStop` against the real filesystem and
    /// rejects a unit whose command is not an existing, executable file.
    /// This process's own executable always exists and is executable, so
    /// it stands in for `spec()`'s fixture path.
    #[test]
    fn systemd_analyze_accepts_the_generated_unit() {
        let Ok(analyze) = which_systemd_analyze() else {
            eprintln!("skipping: systemd-analyze is not on this machine");
            return;
        };
        let mut unit_spec = spec();
        unit_spec.exec = std::env::current_exe().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shep-deploy.service");
        std::fs::write(&path, systemd_unit(&unit_spec)).unwrap();
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

    /// Guards the rendered script's own explanation of why polling stands
    /// in for `READY=1`: prose in a generated artefact can go stale
    /// silently otherwise.
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

    /// openrc derives defaults from `name`/`RC_SVCNAME`; a constant `name`
    /// would make two users on one host collide.
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

    /// The two must agree, or `sysrc shep_<user>_enable=YES` sets a
    /// variable nothing reads, and the service never starts at boot.
    #[test]
    fn the_freebsd_rcvar_matches_the_script_name() {
        let rendered = freebsd_rc_script(&spec());
        assert!(rendered.contains(r#"name="shep_deploy""#), "{rendered}");
        assert!(
            rendered.contains(r#"rcvar="shep_deploy_enable""#),
            "{rendered}"
        );
        assert!(rendered.contains("PROVIDE: shep_deploy"), "{rendered}");
        assert_eq!(
            unit_path_for(Init::FreebsdRc, "deploy"),
            PathBuf::from("/usr/local/etc/rc.d/shep_deploy")
        );
    }

    /// OpenBSD's `rc.subr` has no post-start hook.
    #[test]
    fn the_openbsd_script_admits_it_has_no_readiness_gate() {
        let rendered = openbsd_rc_script(&spec());
        assert!(rendered.contains("no post-start hook"), "{rendered}");
        assert!(rendered.contains("BEFORE the"), "{rendered}");
        assert!(
            !rendered.contains("start_post"),
            "OpenBSD has no such hook: {rendered}"
        );
        assert!(
            !rendered.contains("READY=1"),
            "that is systemd's, not this: {rendered}"
        );
    }

    /// A shepherd started without `SHEP_HOME` falls back to root's
    /// `~/.shep` and restores nothing.
    #[test]
    fn both_bsd_scripts_carry_shep_home_and_path() {
        for rendered in [freebsd_rc_script(&spec()), openbsd_rc_script(&spec())] {
            assert!(rendered.contains("SHEP_HOME="), "{rendered}");
            assert!(rendered.contains("PATH="), "{rendered}");
        }
    }

    /// fails if a metacharacter in `home` escapes the quoting in the
    /// FreeBSD script. Same class as the openrc test above; this one is
    /// worse in the `${name}_env` line, where `rc.subr` word-splits the
    /// result into arguments for `env(1)`.
    #[test]
    fn the_freebsd_script_quotes_shell_metacharacters() {
        let mut s = spec();
        s.home = PathBuf::from(r#"/tmp/we"ird/$HOME/`x`/back\slash"#);
        let rendered = freebsd_rc_script(&s);
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

    /// fails if a metacharacter in `home` escapes the quoting in the
    /// OpenBSD script. Worse than the openrc case: OpenBSD hands its whole
    /// interpolated string to a shell for evaluation via `su -c`.
    #[test]
    fn the_openbsd_script_quotes_shell_metacharacters() {
        let mut s = spec();
        s.home = PathBuf::from(r#"/tmp/we"ird/$HOME/`x`/back\slash"#);
        let rendered = openbsd_rc_script(&s);
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

    /// fails if a `PATH` containing a space becomes two environment entries.
    /// `${name}_env` is a space-separated list rc.subr word-splits into
    /// arguments for `env(1)`, and capturing a real `PATH` is the whole
    /// reason that field exists.
    #[test]
    fn a_path_with_a_space_stays_one_freebsd_env_entry() {
        let mut s = spec();
        s.path = OsString::from("/opt/my tools/bin:/usr/bin");
        let rendered = freebsd_rc_script(&s);
        assert!(
            rendered.contains("PATH='/opt/my tools/bin:/usr/bin'"),
            "the space must stay inside the single-quoted word: {rendered}"
        );
    }

    /// fails if an `exec` path containing a space splits into two argv
    /// elements when `rc.subr` later expands `$command_args` unquoted into
    /// `daemon(8)`'s argv. Without the two-context escape, `daemon(8)`
    /// would be handed `/opt/my` and `tools/shep` as separate arguments and
    /// would exec the wrong (nonexistent) file at the next reboot.
    #[test]
    fn a_path_with_a_space_stays_one_freebsd_command_args_word() {
        let mut s = spec();
        s.exec = PathBuf::from("/opt/my tools/shep");
        let rendered = freebsd_rc_script(&s);
        assert!(
            rendered.contains("-f '/opt/my tools/shep' daemon --foreground"),
            "the space must stay inside the single-quoted word: {rendered}"
        );
        assert!(
            rendered.contains("if \"/opt/my tools/shep\" --home"),
            "start_postcmd's own direct invocation stays single-escaped: {rendered}"
        );
    }

    /// fails if an `exec` path containing a space splits into two words once
    /// `${daemon}` is interpolated unquoted into the string `rc_exec` hands
    /// to `su -c`, which spawns a second shell that word-splits it.
    #[test]
    fn a_path_with_a_space_stays_one_openbsd_daemon_word() {
        let mut s = spec();
        s.exec = PathBuf::from("/opt/my tools/shep");
        let rendered = openbsd_rc_script(&s);
        assert!(
            rendered.contains("daemon=\"'/opt/my tools/shep'\""),
            "the space must stay inside the single-quoted word: {rendered}"
        );
    }

    /// Byte-for-byte, the same tier `the_systemd_unit_matches_the_spec_exactly`
    /// and `the_launchd_plist_matches_the_spec_exactly` already set. The
    /// `.contains` tests above each guard one claim; this one guards the
    /// whole artefact, which is the only kind of test a file nobody can run
    /// on its own OS can have.
    #[test]
    fn the_openbsd_script_matches_the_spec_exactly() {
        let rendered = openbsd_rc_script(&spec());
        assert_eq!(
            rendered,
            "#!/bin/ksh\n\
             #\n\
             # shep process manager for deploy\n\
             #\n\
             # Enable with: rcctl enable shep_deploy && rcctl start shep_deploy\n\
             #\n\
             # OpenBSD's rc.subr has no post-start hook: rc_pre runs before the daemon\n\
             # starts and rc_post runs after it stops. So this script reports the service\n\
             # started as soon as the shepherd process is spawned, which is BEFORE the\n\
             # muster restore has finished — the flock may still be coming back. There is\n\
             # no readiness protocol here and this script does not pretend to one. Check\n\
             # with: shep --home /home/deploy/.shep flock\n\
             \n\
             daemon=\"/usr/local/bin/shep\"\n\
             daemon_flags=\"daemon --foreground\"\n\
             daemon_user=\"deploy\"\n\
             daemon_execdir=\"/home/deploy\"\n\
             \n\
             . /etc/rc.d/rc.subr\n\
             \n\
             rc_bg=YES\n\
             rc_reload=NO\n\
             \n\
             # su(1)'s -l flag, which rc_exec always passes, discards the environment\n\
             # rc.subr itself ran in — so exporting SHEP_HOME/PATH above would never reach\n\
             # the daemon. Prefixing them onto the string rc_exec hands to su is the only\n\
             # way they survive. Single-quoted for the shell that evaluates that string,\n\
             # then escaped again for the double-quoted context here.\n\
             rc_start() {\n\
             \trc_exec \"SHEP_HOME=/home/deploy/.shep PATH=/home/deploy/.bun/bin:/usr/local/bin:/usr/bin:/bin ${daemon} ${daemon_flags}\"\n\
             }\n\
             \n\
             rc_cmd $1\n",
            "{rendered}"
        );
    }
}
