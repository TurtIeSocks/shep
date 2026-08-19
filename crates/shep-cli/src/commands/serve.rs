//! `shep serve`: registers a static file server as a managed sheep, or —
//! with `--foreground` — runs the worker directly in this terminal.
//!
//! **One function does both halves** ([`serve`]), and does every refusal and
//! every notice before either one: `--foreground` and the registered sheep
//! must never disagree about what is valid, and the registered sheep is
//! itself this same binary re-invoked with `--foreground` appended
//! ([`sheep_args`]) — so the shepherd's own spawn of it runs straight back
//! through this function, re-deriving the same refusals and the same
//! notices against its own stderr, which is where `shep bleats` reads them
//! from (Phase 15 decision 8's own two-audience split).
//!
//! Dispatched from `lib.rs` before the shared `$SHEP_HOME`-gated, locked
//! block — the same early-dispatch spot `lookout` and `bleats` use, and for
//! the same reason: `--foreground` runs until signalled, and a `StdoutLock`
//! held for a process lifetime wedges the first off-thread write elsewhere
//! in the binary.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use shep_client::{Client, START_DEADLINE};
use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response};

use crate::cli::{Format, ServeArgs};
use crate::exit::ExitCode;
use crate::output::{FlockRows, Render, Streams, emit, emit_error, emit_notice, write_outcome};
use crate::serve::auth::{self, AuthError, Credentials};
use crate::serve::worker::{self, ServeConfig};

/// Why the shared refusals (Phase 15 decision, Step 7.3) stopped a
/// `shep serve` invocation before either half — registering or
/// `--foreground` — ever ran. Module-scoped per IR-18.
#[derive(Debug)]
enum ServeRefusal {
    /// `root` does not exist, or a component along the way is not itself a
    /// directory. Carries `std::fs::canonicalize`'s own error rather than
    /// re-deriving which case happened.
    RootUnresolvable {
        /// The path as the operator wrote it.
        root: PathBuf,
        /// The underlying IO failure.
        source: std::io::Error,
    },
    /// `root` resolved to a real path, but that path is not a directory —
    /// a file, say.
    RootNotADirectory {
        /// The resolved, canonical path.
        root: PathBuf,
    },
    /// `--auth` named a file [`auth::load`] refused, or that could not be
    /// canonicalized after loading fine.
    Auth(AuthError),
    /// `--spa` was given but `root` holds no `index.html` — a would-be 404
    /// this flag is supposed to answer with would have nothing to answer it
    /// with.
    MissingSpaIndex {
        /// The resolved, canonical docroot.
        root: PathBuf,
    },
}

impl std::fmt::Display for ServeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootUnresolvable { root, source } => write!(f, "{}: {source}", root.display()),
            Self::RootNotADirectory { root } => write!(f, "{}: not a directory", root.display()),
            Self::Auth(err) => write!(f, "{err}"),
            Self::MissingSpaIndex { root } => write!(
                f,
                "--spa was given but {} has no index.html",
                root.display()
            ),
        }
    }
}

impl core::error::Error for ServeRefusal {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::RootUnresolvable { source, .. } => Some(source),
            Self::RootNotADirectory { .. } | Self::MissingSpaIndex { .. } => None,
            Self::Auth(err) => Some(err),
        }
    }
}

/// The exit code a [`ServeRefusal`] reports — decision (Step 7.3): a bad
/// `root` is a usage error, a bad `--auth` file or a missing SPA index is a
/// config error.
fn refusal_exit_code(refusal: &ServeRefusal) -> ExitCode {
    match refusal {
        ServeRefusal::RootUnresolvable { .. } | ServeRefusal::RootNotADirectory { .. } => {
            ExitCode::Usage
        }
        ServeRefusal::Auth(_) | ServeRefusal::MissingSpaIndex { .. } => ExitCode::InvalidConfig,
    }
}

/// Renders `refusal` and returns the exit code it reports.
fn fail(streams: &mut Streams<'_>, fmt: Format, refusal: &ServeRefusal) -> ExitCode {
    let code = refusal_exit_code(refusal);
    let _ = emit_error(
        &mut *streams.err,
        fmt,
        code.code_str(),
        &refusal.to_string(),
    );
    code
}

/// Resolves and canonicalizes `root`, refusing it if it is missing or not a
/// directory (Phase 15 decision 11).
fn validate_root(root: &Path) -> Result<PathBuf, ServeRefusal> {
    let canonical =
        std::fs::canonicalize(root).map_err(|source| ServeRefusal::RootUnresolvable {
            root: root.to_path_buf(),
            source,
        })?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(ServeRefusal::RootNotADirectory { root: canonical })
    }
}

/// Loads and canonicalizes `path` as `--auth`'s creds file, if given.
///
/// Canonicalizing here — not deferred to [`sheep_args`]'s caller — is what
/// [`sheep_args`]'s own doc comment calls out: the registering half must
/// hand a relative `--auth` no further than this point, because a relative
/// path baked into the registered sheep's command line resolves against the
/// shepherd's cwd, not the operator's, and produces a sheep that validates
/// clean at registration and crash-loops on its first restart.
///
/// # Errors
/// [`ServeRefusal::Auth`] if the file cannot be loaded or, having loaded,
/// cannot be canonicalized.
fn validate_auth(path: &Path) -> Result<(PathBuf, Credentials), ServeRefusal> {
    let credentials = auth::load(path).map_err(ServeRefusal::Auth)?;
    let canonical = std::fs::canonicalize(path).map_err(|source| {
        ServeRefusal::Auth(AuthError::Io {
            path: path.to_path_buf(),
            source,
        })
    })?;
    Ok((canonical, credentials))
}

/// The compensating control for allowing `--bind` wider than loopback
/// (Phase 15 decision 8). `None` when `bind` is loopback; otherwise a
/// stderr notice naming the address and the docroot it is about to expose,
/// and — only when no `--auth` was set — spelling out that its files will
/// be readable by anything that can reach the port.
fn exposure_notice(bind: IpAddr, auth: bool, root: &Path) -> Option<String> {
    if bind.is_loopback() {
        return None;
    }
    let root = root.display();
    Some(if auth {
        format!(
            "shep serve: bound to {bind}, reachable from beyond this host — {root} is exposed \
             to anything that can reach the port"
        )
    } else {
        format!(
            "shep serve: bound to {bind}, reachable from beyond this host, with no --auth set — \
             {root}'s files will be readable by anything that can reach the port"
        )
    })
}

/// The second, independent compensating control (Phase 15 decision 8's
/// addendum): a stderr notice naming the check-then-open race
/// `--follow-symlinks` reopens. `None` when the flag is off. Independent of
/// [`exposure_notice`] on purpose — a fully loopback serve with the flag on
/// must still get this notice, and a wide bind without the flag says
/// nothing about symlinks.
fn follow_symlinks_notice(follow_symlinks: bool) -> Option<String> {
    if !follow_symlinks {
        return None;
    }
    Some(
        "shep serve: --follow-symlinks reopens the check-then-open race (TOCTOU) the default \
         per-component walk closes — a symlink under the docroot can now point anywhere this \
         process can read"
            .to_string(),
    )
}

/// Writes one of this verb's own notices to `streams.err`, through
/// [`emit_notice`] rather than [`emit_error`] — a notice's code is not part
/// of [`ExitCode`]'s taxonomy, and a clean run reaching `--foreground`'s
/// worker or a green registration can still emit one on its way there.
fn print_notice(streams: &mut Streams<'_>, fmt: Format, code: &str, message: &str) {
    let _ = emit_notice(&mut *streams.err, fmt, code, message);
}

/// The sheep's own command line, rebuilt from the flags rather than from
/// `std::env::args`.
///
/// Rebuilt, not forwarded: the operator's `shep serve ./dist` carries a
/// relative path that resolves against *their* cwd, and the shepherd spawns
/// from its own. The canonical root goes in, and every flag is written in
/// one canonical order, so `shep describe` shows the same line for the same
/// server however it was typed.
///
/// **`root` and `auth` both arrive already canonical — the caller
/// canonicalizes both**, in [`validate_root`] and [`validate_auth`], before
/// building this line. `shep serve ./dist --auth ./creds` validates the
/// file successfully in the registering half — so the operator sees no
/// error at all — and then, if `--auth` were forwarded uncanonicalized,
/// would register a sheep that resolves `./creds` against the shepherd's
/// cwd, does not find it, and crash-loops. A relative docroot produces a
/// 404; a relative creds path produces a server that never starts, after a
/// green registration. This function only ever emits the paths it is
/// handed.
///
/// **`--name` and `--fold` are deliberately NOT in the output.** They are
/// registration-time facts — which sheep this is and which fold it joins —
/// and mean nothing to the foreground worker that receives this line.
///
/// **`--follow-symlinks`, by contrast, is a worker-time fact and IS in the
/// output when set**, the same as `--spa`, `--listing` and `--hidden`: the
/// foreground process is the one that calls `fs::contain`, so it is the one
/// that has to know. A sheep registered with the flag on and restarted by
/// the shepherd must come back up still following symlinks — dropping it
/// here would be the same silent downgrade `--bind`'s round-trip test
/// already guards against, on a security-relevant flag instead of a
/// networking one.
fn sheep_args(root: &Path, auth: Option<&Path>, args: &ServeArgs) -> Vec<String> {
    let mut out = vec!["serve".to_string(), root.display().to_string()];
    out.push("--port".to_string());
    out.push(args.port.to_string());
    out.push("--bind".to_string());
    out.push(args.bind.to_string());
    if args.spa {
        out.push("--spa".to_string());
    }
    if args.listing {
        out.push("--listing".to_string());
    }
    if args.hidden {
        out.push("--hidden".to_string());
    }
    if args.follow_symlinks {
        out.push("--follow-symlinks".to_string());
    }
    if let Some(auth) = auth {
        out.push("--auth".to_string());
        out.push(auth.display().to_string());
    }
    out.push("--foreground".to_string());
    out
}

/// The name a registered sheep gets when `--name` is absent: the canonical
/// docroot's own file name, falling back to `serve` when it has none (`/`,
/// or a root the platform gives no basename to).
fn default_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "serve".to_string())
}

/// `shep serve`'s entry point, reached from `lib.rs`'s early dispatch —
/// before the shared `$SHEP_HOME`-gated, locked block, the same spot
/// `lookout` and `bleats` are, and for the same reason: `--foreground` runs
/// until signalled.
///
/// Does every refusal and every notice once, for both halves, so
/// `--foreground` and the registered sheep can never disagree about what is
/// valid — see this module's own doc.
pub async fn serve(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &ServeArgs,
) -> ExitCode {
    let root = match validate_root(&args.root) {
        Ok(root) => root,
        Err(refusal) => return fail(streams, fmt, &refusal),
    };

    let auth = match args.auth.as_deref() {
        Some(path) => match validate_auth(path) {
            Ok(auth) => Some(auth),
            Err(refusal) => return fail(streams, fmt, &refusal),
        },
        None => None,
    };

    if args.spa && !root.join("index.html").is_file() {
        return fail(streams, fmt, &ServeRefusal::MissingSpaIndex { root });
    }

    if let Some(notice) = exposure_notice(args.bind, auth.is_some(), &root) {
        print_notice(streams, fmt, "exposure", &notice);
    }
    if let Some(notice) = follow_symlinks_notice(args.follow_symlinks) {
        print_notice(streams, fmt, "follow_symlinks", &notice);
    }

    if args.foreground {
        let cfg = ServeConfig {
            root,
            bind: SocketAddr::new(args.bind, args.port),
            spa: args.spa,
            listing: args.listing,
            hidden: args.hidden,
            auth: auth.map(|(_, credentials)| credentials),
            follow_symlinks: args.follow_symlinks,
            connection_deadline: worker::CONNECTION_DEADLINE,
        };
        return worker::run(cfg).await;
    }

    register(
        streams,
        fmt,
        paths,
        &root,
        auth.as_ref().map(|(path, _)| path.as_path()),
        args,
    )
    .await
}

/// Registers `root` as a sheep whose command line runs this same binary
/// again with `--foreground` appended ([`sheep_args`]), through the same
/// path `shep start` uses — `connect_or_spawn_client`, because starting a
/// sheep against a dead shepherd means bringing one up first.
async fn register(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    root: &Path,
    auth: Option<&Path>,
    args: &ServeArgs,
) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(source) => {
            let message = format!("could not resolve this binary's own path: {source}");
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Failure.code_str(),
                &message,
            );
            return ExitCode::Failure;
        }
    };

    let name = args.name.clone().unwrap_or_else(|| default_name(root));
    let mut app = AppConfig::minimal(&name, &exe.display().to_string());
    app.args = sheep_args(root, auth, args);
    app.fold.clone_from(&args.fold);

    let client = match crate::connect_or_spawn_client(streams, fmt, paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    request_and_render(
        &client,
        streams,
        fmt,
        "serve",
        Request::Start { apps: vec![app] },
        Some(START_DEADLINE),
        |response| match response {
            Response::Started(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Sends `body`, renders whatever the daemon answers through [`emit`], and
/// maps every way that can go wrong to its exit code.
///
/// A copy of `commands::lifecycle`'s own helper of the same name and shape
/// (also duplicated in `commands::logs`/`commands::query`) rather than a
/// shared one — this project's own precedent for a small, single-purpose
/// helper with more than one call site across `commands/`.
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    command: &str,
    body: Request,
    deadline: Option<Duration>,
    extract: F,
) -> ExitCode
where
    T: Render,
    F: FnOnce(Response) -> Option<T>,
{
    match client.request_with_deadline(body, deadline).await {
        Ok(response) => match extract(response) {
            Some(payload) => write_outcome(emit(
                &mut *streams.out,
                fmt,
                command,
                payload,
                streams.style,
            )),
            None => {
                let message = "the daemon answered with a response this client does not understand";
                let _ = emit_error(
                    &mut *streams.err,
                    fmt,
                    ExitCode::Internal.code_str(),
                    message,
                );
                ExitCode::Internal
            }
        },
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_args() -> ServeArgs {
        ServeArgs {
            root: PathBuf::from("./dist"),
            port: 9000,
            bind: "0.0.0.0".parse().unwrap(),
            name: Some("web".into()),
            fold: Some("prod".into()),
            spa: true,
            listing: true,
            hidden: true,
            follow_symlinks: true,
            auth: Some(PathBuf::from("./creds")),
            foreground: false,
        }
    }

    /// fails if the registered command line loses a flag, or carries the
    /// operator's relative path instead of the canonical one.
    ///
    /// Every field of `ServeArgs` is set to a non-default value here, so a
    /// flag `sheep_args` forgets shows up as an absence rather than as a
    /// default that happens to match.
    #[test]
    fn the_registered_command_line_is_absolute_and_carries_every_flag() {
        let args = full_args();
        let built = sheep_args(Path::new("/srv/www"), Some(Path::new("/srv/creds")), &args);
        assert_eq!(built[0], "serve");
        assert_eq!(built[1], "/srv/www");
        assert!(built.contains(&"--foreground".to_string()));
        assert!(built.contains(&"--spa".to_string()));
        assert!(built.contains(&"--listing".to_string()));
        assert!(built.contains(&"--hidden".to_string()));
        assert!(
            built.contains(&"--follow-symlinks".to_string()),
            "a sheep that quietly drops this on restart silently reopens the safe default"
        );
        assert!(built.windows(2).any(|w| w == ["--port", "9000"]));
        assert!(
            built.windows(2).any(|w| w == ["--bind", "0.0.0.0"]),
            "a sheep that quietly binds loopback is a silent downgrade"
        );
        assert!(
            built.windows(2).any(|w| w == ["--auth", "/srv/creds"]),
            "absolute, or the sheep crash-loops after a green registration"
        );
        assert!(
            !built.contains(&"--name".to_string()),
            "registration-time only"
        );
        assert!(
            !built.contains(&"--fold".to_string()),
            "registration-time only"
        );
    }

    /// fails if the rebuilt line does not parse back to the same flags — the
    /// half a string-equality test cannot see.
    ///
    /// Whole-struct equality, not field by field: a field added to
    /// `ServeArgs` without a matching arm in `sheep_args` fails this test by
    /// construction, which is the property the earlier field-by-field
    /// version claimed and did not have — it asserted four of ten fields
    /// and let `--bind` and `--auth` through silently.
    #[test]
    fn the_registered_command_line_parses_back_to_the_same_arguments() {
        use crate::cli::{Cli, Commands};
        use clap::Parser;

        let original = full_args();
        let built = sheep_args(
            Path::new("/srv/www"),
            Some(Path::new("/srv/creds")),
            &original,
        );
        let mut argv = vec!["shep".to_string()];
        argv.extend(built);
        let cli = Cli::try_parse_from(argv).expect("the line shep registers must parse");
        let Commands::Serve(parsed) = cli.command else {
            panic!("expected serve")
        };
        assert_eq!(
            parsed,
            ServeArgs {
                root: PathBuf::from("/srv/www"),
                auth: Some(PathBuf::from("/srv/creds")),
                foreground: true,
                // registration-time only, and absent from the line by design
                name: None,
                fold: None,
                ..original
            }
        );
    }

    /// fails if widening the bind stops being loud. The notice is the entire
    /// compensating control for allowing `--bind 0.0.0.0` (decision 8).
    #[test]
    fn a_non_loopback_bind_produces_a_notice_that_names_the_address() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(
            exposure_notice(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                false,
                Path::new("/srv/www")
            )
            .is_none()
        );
        let notice = exposure_notice("0.0.0.0".parse().unwrap(), false, Path::new("/srv/www"))
            .expect("a wider bind must say so");
        assert!(notice.contains("0.0.0.0"), "{notice}");
        assert!(notice.contains("/srv/www"), "{notice}");
        assert!(
            notice.contains("readable"),
            "no auth: say what that means: {notice}"
        );
        let with_auth = exposure_notice("0.0.0.0".parse().unwrap(), true, Path::new("/srv/www"))
            .expect("still a wider bind");
        assert!(!with_auth.contains("readable"), "{with_auth}");
    }

    /// fails if turning symlink-following on stops being loud, or if the
    /// notice reads as free rather than as a reopened race. Independent of
    /// `exposure_notice` on purpose (decision 8's addendum): a fully
    /// loopback serve with the flag on must still get this notice.
    #[test]
    fn follow_symlinks_produces_a_notice_that_names_the_race() {
        assert!(follow_symlinks_notice(false).is_none());
        let notice = follow_symlinks_notice(true).expect("the flag must say so");
        assert!(notice.contains("--follow-symlinks"), "{notice}");
        assert!(
            notice.contains("race") || notice.contains("TOCTOU"),
            "{notice}"
        );
    }
}
