//! Whether a shepherd is answering, and where this flock lives.
//!
//! One probe, two readers: `shep ping` renders it as the command's whole
//! output, and the quiet verbs (`shep` with no verb, `welcome`, `help`,
//! `completions`) print a one-line form to stderr, since those four
//! produce no flock data of their own.
//!
//! Absent from every other verb: a command that succeeded already told
//! you the shepherd is up.

use std::path::PathBuf;

use shep_client::Client;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response};

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{OutputEnvelope, SCHEMA_VERSION, Streams};

/// What a shepherd said about itself when it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Online {
    /// Daemon crate version, off the handshake.
    pub(crate) version: String,
    /// Daemon pid, off the same handshake.
    pub(crate) pid: u32,
}

/// A shepherd's liveness plus the paths that identify this flock.
///
/// The paths are carried rather than looked up again because `HelloAck` has
/// no room for them: it holds `daemon_version`, `protocol` and `pid` and
/// nothing else, so the socket path cannot be recovered from a handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShepherdStatus {
    /// `Some` when a shepherd answered the handshake.
    pub(crate) online: Option<Online>,
    /// `$SHEP_HOME` for this invocation.
    pub(crate) home: PathBuf,
    /// The control socket this probe tried.
    pub(crate) socket: PathBuf,
}

impl ShepherdStatus {
    /// Asks the socket whether anyone is home, and makes them answer.
    ///
    /// Never fails: "nothing answered" is an answer, not an error.
    ///
    /// Reports online only after the connect, the handshake, and a real
    /// [`Request::Ping`] round-trip all succeed: a daemon can hold a
    /// listening socket and complete a handshake while wedged past the
    /// point of serving anything.
    ///
    /// `version` and `pid` come off the handshake, not the reply, since
    /// `Response::Pong` carries neither.
    pub(crate) async fn probe(paths: &ShepPaths) -> Self {
        let online = match Client::connect(&paths.socket).await {
            Ok(client) => {
                let ack = client.daemon();
                let version = ack.daemon_version.clone();
                let pid = ack.pid;
                match client.request(Request::Ping).await {
                    Ok(Response::Pong) => Some(Online { version, pid }),
                    // Answered with something else, or not at all: a socket
                    // that talks but does not serve is not online.
                    _ => None,
                }
            }
            Err(_) => None,
        };
        Self {
            online,
            home: paths.home.clone(),
            socket: paths.socket.clone(),
        }
    }
}

/// One line for the quiet verbs, naming the home so the flock's location
/// is discoverable without provoking an error message to learn it.
///
/// Only the four verbs that produce no flock data of their own get this:
/// `shep` with no verb, `welcome`, `help`, `completions`.
pub(crate) fn one_line(status: &ShepherdStatus) -> String {
    match &status.online {
        Some(Online { pid, .. }) => format!(
            "shepherd online (pid {pid}), flock at {}",
            status.home.display()
        ),
        None => format!(
            "no shepherd running, flock at {}. `shep start` brings one up.",
            status.home.display()
        ),
    }
}

/// `shep ping`'s JSON payload.
///
/// Its own type rather than a `rows::` entry with a `Render` impl: `Render`
/// is a *table* trait wanting `headers()` and `rows()`, and one record with
/// two long paths in it renders as an unreadable five-column line. The table
/// form below is written by hand for the same reason `describe` has its own.
#[derive(Debug, serde::Serialize)]
struct PingStatus {
    /// `"online"` or `"offline"`.
    shepherd: &'static str,
    /// Daemon crate version off the handshake; `None` when offline.
    daemon_version: Option<String>,
    /// Daemon pid off the same handshake; `None` when offline.
    pid: Option<u32>,
    /// `$SHEP_HOME` for this invocation.
    home: String,
    /// The control socket that was tried.
    socket: String,
}

/// `shep ping`'s own rendering, online or off.
///
/// A verb whose whole job is reporting liveness must not fail because
/// the answer is "down": it reports instead of erroring.
///
/// The exit code is unchanged: still [`ExitCode::DaemonUnreachable`] when
/// nothing answers, since `shep ping && echo up` is a real idiom.
pub(crate) fn render_ping(streams: &mut Streams<'_>, status: &ShepherdStatus) -> ExitCode {
    let online = status.online.as_ref();
    let payload = PingStatus {
        shepherd: if online.is_some() {
            "online"
        } else {
            "offline"
        },
        daemon_version: online.map(|o| o.version.clone()),
        pid: online.map(|o| o.pid),
        home: status.home.display().to_string(),
        socket: status.socket.display().to_string(),
    };

    let _ = match streams.fmt {
        Format::Table => {
            let mut lines = vec![format!("shepherd  {}", payload.shepherd)];
            if let Some(o) = online {
                lines.push(format!("version   {}", o.version));
                lines.push(format!("pid       {}", o.pid));
            }
            lines.push(format!("home      {}", payload.home));
            lines.push(format!("socket    {}", payload.socket));
            writeln!(streams.out, "{}", lines.join("\n"))
        }
        Format::Json => {
            let envelope = OutputEnvelope {
                schema_version: SCHEMA_VERSION,
                command: "ping",
                data: payload,
            };
            serde_json::to_writer(&mut *streams.out, &envelope)
                .map_err(std::io::Error::other)
                .and_then(|()| writeln!(streams.out))
        }
    };

    if online.is_some() {
        ExitCode::Success
    } else {
        ExitCode::DaemonUnreachable
    }
}

// unix only: the socket-backed case has not been run against the Windows
// named-pipe transport.
#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    fn at(online: Option<Online>) -> ShepherdStatus {
        ShepherdStatus {
            online,
            home: PathBuf::from("/home/ada/.shep"),
            socket: PathBuf::from("/home/ada/.shep/run/shep.sock"),
        }
    }

    /// The offline line has to carry the way out, because the reader seeing
    /// it is the reader who does not know what to type next.
    #[test]
    fn the_offline_line_names_the_home_and_the_way_out() {
        let line = one_line(&at(None));
        assert!(line.contains("no shepherd running"), "{line}");
        assert!(line.contains("/home/ada/.shep"), "{line}");
        assert!(line.contains("shep start"), "{line}");
    }

    /// Online, the pid is the useful part: it is what you reach for to know
    /// whether the thing you are looking at is the thing you started.
    #[test]
    fn the_online_line_names_the_pid_and_the_home() {
        let line = one_line(&at(Some(Online {
            version: "0.1.0-alpha.1".to_owned(),
            pid: 4823,
        })));
        assert!(line.contains("online"), "{line}");
        assert!(line.contains("4823"), "{line}");
        assert!(line.contains("/home/ada/.shep"), "{line}");
    }

    /// No em dashes in copy a user reads.
    #[test]
    fn the_status_lines_have_no_em_dashes() {
        for line in [
            one_line(&at(None)),
            one_line(&at(Some(Online {
                version: "0.1.0".to_owned(),
                pid: 1,
            }))),
        ] {
            assert!(!line.contains('\u{2014}'), "em dash in {line:?}");
            assert!(!line.contains('\u{2013}'), "en dash in {line:?}");
        }
    }

    /// A shepherd wedged past its own handshake is not online: `probe`
    /// connects and handshakes, then reports offline because the
    /// [`Request::Ping`] it sends is never answered.
    ///
    /// `handshook` is the load-bearing assertion. A handshake that failed
    /// would give the same `None`, and the liveness path would go untested
    /// while the test still passed.
    ///
    /// `start_paused` so the request deadline costs no wall clock.
    #[tokio::test(start_paused = true)]
    async fn a_socket_that_handshakes_but_never_answers_is_not_online() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (_fake, handshook) = shep_client::testing::fake_daemon_wedged_after_handshake(
            &path,
            shep_client::testing::sample_ack(),
        );

        let env =
            |key: &str| (key == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned());
        let mut paths = ShepPaths::resolve(&env, std::path::Path::new("/nonexistent"));
        paths.socket = path;

        let status = ShepherdStatus::probe(&paths).await;
        assert!(
            handshook.load(Ordering::SeqCst),
            "the handshake has to complete, or this covers a failed connect and not the ping"
        );
        assert_eq!(
            status.online, None,
            "a wedged socket is not an online shepherd"
        );

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            render_ping(&mut streams, &status)
        };
        assert_eq!(code, ExitCode::DaemonUnreachable);
        assert!(
            String::from_utf8(out).unwrap().contains("offline"),
            "the operator is told, not errored at"
        );
    }

    /// A probe against a socket nobody is listening on reports offline
    /// rather than erroring, which is the property every caller relies on.
    #[tokio::test]
    async fn a_probe_with_nothing_listening_reports_offline() {
        let dir = tempfile::tempdir().unwrap();
        let env =
            |key: &str| (key == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned());
        let paths = ShepPaths::resolve(&env, std::path::Path::new("/nonexistent"));

        let status = ShepherdStatus::probe(&paths).await;
        assert_eq!(status.online, None);
        assert_eq!(status.home, dir.path());
    }
}
