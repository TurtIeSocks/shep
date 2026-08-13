//! `shep enable`/`shep disable`: the operator verbs that turn a registered
//! dog on and off.
//!
//! Neither takes an already-connected [`Client`], unlike every verb in
//! `commands::lifecycle`/`commands::query`: both must still do useful work
//! against a `$SHEP_HOME` with no shepherd running at all, so `main`
//! dispatches them straight off the resolved [`ShepPaths`] rather than
//! through `connect_client`, and each one attempts its own connection here,
//! tolerating a failure to reach one.
//!
//! **The order is config first, then the daemon.** [`ShepToml::save`] runs
//! before either verb ever tries the socket: if the RPC that follows fails
//! or never gets attempted, the config still says what the operator asked
//! for, and the next boot brings it up — which is the state the operator
//! actually wanted. The reverse order would leave a dog running (or
//! stopped) that no boot restores.
//!
//! **Neither verb autostarts a shepherd** — decision 11. `enable` against
//! no running daemon writes the config, reports that the dog will come up
//! with the next shepherd, and exits [`ExitCode::Success`]. Autostarting a
//! whole supervisor as a side effect of a config edit would be a surprise
//! out of proportion to the ask; `shep muster` is the one verb that
//! autostarts, and it says so in its own help text.

use shep_client::Client;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{DogSource, Request, Response};

use crate::cli::Format;
use crate::commands::shep_toml::{ShepToml, ShepTomlError};
use crate::exit::ExitCode;
use crate::output::{DogDisabledRow, DogEnabledRow, Streams, emit, emit_error, write_outcome};

/// [`DogEnabledRow::status`] when `enable` wrote the config but no shepherd
/// answered — decision 11: `enable` never autostarts one to act on its own
/// edit, so this IS the success outcome, not a partial one.
const NO_SHEPHERD_ENABLE_STATUS: &str = "will start with the next shepherd";

/// [`DogDisabledRow::status`] when `disable` wrote the config but no
/// shepherd answered — the mirror of [`NO_SHEPHERD_ENABLE_STATUS`].
const NO_SHEPHERD_DISABLE_STATUS: &str = "not running; will not start with the next shepherd";

/// [`DogDisabledRow::status`] when a shepherd stopped the dog.
const DISABLED_STATUS: &str = "stopped";

/// Renders `err` and returns the exit code a config-write failure reports.
///
/// [`ShepTomlError::Parse`] is a config-validation failure — the same
/// category [`ExitCode::InvalidConfig`] names for a bad Flockfile
/// (`commands::lifecycle::target_exit_code`) — while
/// [`ShepTomlError::Io`] has no more specific code than
/// [`ExitCode::Failure`].
fn fail_config(streams: &mut Streams<'_>, fmt: Format, err: &ShepTomlError) -> ExitCode {
    let code = match err {
        ShepTomlError::Io { .. } => ExitCode::Failure,
        ShepTomlError::Parse { .. } => ExitCode::InvalidConfig,
    };
    let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
    code
}

/// `shep enable <name>`: writes the config, and starts the dog if a
/// shepherd is running.
pub async fn enable(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode {
    let mut cfg = match ShepToml::open(&paths.daemon_config) {
        Ok(cfg) => cfg,
        Err(err) => return fail_config(streams, fmt, &err),
    };
    cfg.enable_dog(name);
    if let Err(err) = cfg.save() {
        return fail_config(streams, fmt, &err);
    }
    let client = Client::connect(&paths.socket).await.ok();
    enable_after_config(streams, fmt, name, client.as_ref()).await
}

/// `enable`'s daemon half, split out from [`enable`] so a test can drive it
/// against a [`shep_client::testing`] fake without racing a second, real
/// connection to the same socket the fake's own fixture already opened —
/// [`crate::commands::lifecycle::resolve_target`] is split out of `start`
/// for the same reason: hermetic testability of the part that has a seam.
///
/// `client: None` is [`enable`]'s own `Client::connect(..).ok()` — every way
/// a connection can fail is folded into "no shepherd running" here, matching
/// decision 11: this verb does not distinguish a stale socket file from a
/// genuinely absent daemon, because a provisioning script configuring a host
/// before starting anything must not have to.
async fn enable_after_config(
    streams: &mut Streams<'_>,
    fmt: Format,
    name: &str,
    client: Option<&Client>,
) -> ExitCode {
    let Some(client) = client else {
        let row = DogEnabledRow {
            name: name.to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: NO_SHEPHERD_ENABLE_STATUS.to_string(),
        };
        return write_outcome(emit(&mut *streams.out, fmt, "enable", row));
    };
    // Always `BuiltIn`: `shep adopt` (a later verb) is the one that carries
    // a path. An `EnableDog` reaching a name a sheep already holds comes
    // back as `RpcErrorCode::InvalidConfig` with the daemon's own message
    // naming the collision (`shep-daemon/src/rpc.rs`'s `EnableDog` arm) —
    // the `Err` arm below surfaces that message verbatim rather than a bare
    // code, which is already the clear report an operator needs.
    let request = Request::EnableDog {
        name: name.to_string(),
        source: DogSource::BuiltIn,
    };
    match client.request(request).await {
        Ok(Response::DogStarted(info)) => {
            let row = DogEnabledRow {
                name: name.to_string(),
                source: DogSource::BuiltIn,
                shepherd_acted: true,
                status: info.status.to_string(),
            };
            write_outcome(emit(&mut *streams.out, fmt, "enable", row))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// `shep disable <name>`: removes it from the config, and stops it if a
/// shepherd is running.
pub async fn disable(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    name: &str,
) -> ExitCode {
    let mut cfg = match ShepToml::open(&paths.daemon_config) {
        Ok(cfg) => cfg,
        Err(err) => return fail_config(streams, fmt, &err),
    };
    cfg.disable_dog(name);
    if let Err(err) = cfg.save() {
        return fail_config(streams, fmt, &err);
    }
    let client = Client::connect(&paths.socket).await.ok();
    disable_after_config(streams, fmt, name, client.as_ref()).await
}

/// `disable`'s daemon half — see [`enable_after_config`]'s own doc for why
/// this split exists and what `client: None` means.
async fn disable_after_config(
    streams: &mut Streams<'_>,
    fmt: Format,
    name: &str,
    client: Option<&Client>,
) -> ExitCode {
    let Some(client) = client else {
        let row = DogDisabledRow {
            name: name.to_string(),
            source: DogSource::BuiltIn,
            shepherd_acted: false,
            status: NO_SHEPHERD_DISABLE_STATUS.to_string(),
        };
        return write_outcome(emit(&mut *streams.out, fmt, "disable", row));
    };
    // `Response::Deleted`, the same reply `Delete` gives — `DisableDog`'s
    // own doc (`shep-core/src/protocol/request.rs`) says disabling
    // deregisters exactly as `Delete` does, so this reuses that reply
    // rather than inventing a shape of its own.
    match client
        .request(Request::DisableDog {
            name: name.to_string(),
        })
        .await
    {
        Ok(Response::Deleted(_ids)) => {
            let row = DogDisabledRow {
                name: name.to_string(),
                source: DogSource::BuiltIn,
                shepherd_acted: true,
                status: DISABLED_STATUS.to_string(),
            };
            write_outcome(emit(&mut *streams.out, fmt, "disable", row))
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    use super::*;

    fn streams<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Streams<'a> {
        Streams { out, err }
    }

    /// fails if `enable` sends anything but `EnableDog` with the name it was
    /// given and a `BuiltIn` source — the class of bug that left `restart`
    /// and `delete` sending `Request::Stop` with every test green.
    #[tokio::test]
    async fn enable_asks_the_shepherd_to_start_that_dog_as_a_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = enable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "metrics",
            Some(&client),
        )
        .await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.body,
            Request::EnableDog {
                name: "metrics".to_string(),
                source: DogSource::BuiltIn,
            }
        );
    }

    /// fails if a `shep enable` with no shepherd running is reported as a
    /// failure. The config edit is the part the operator asked for, and it
    /// landed; the dog comes up with the next boot. A non-zero exit here
    /// would make `shep enable` unusable in a provisioning script that
    /// configures a host before starting anything.
    #[tokio::test]
    async fn enable_with_no_shepherd_writes_the_config_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            "metrics",
        )
        .await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        assert!(
            written.contains("metrics"),
            "the config edit must still land: {written}"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("next shepherd"),
            "the operator needs to know the dog is not running yet: {text}"
        );
    }

    /// The name-collision guard `shep-daemon/src/rpc.rs`'s `EnableDog` arm
    /// carries (Task 6): `start_dog` is idempotent by name, so an unmarked
    /// entry coming back means a sheep already holds `name`, and the daemon
    /// refuses with `InvalidConfig` naming the collision. This pins that the
    /// operator sees that message verbatim on stderr, not a bare code —
    /// this verb sits directly on top of that guard and must not swallow it.
    #[tokio::test]
    async fn enable_reports_a_name_collision_with_the_daemons_own_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let message =
            "a sheep is already registered as `bark`; rename it or give the dog another name";
        let (client, _daemon) =
            fake_client_replying_err(&path, RpcErrorCode::InvalidConfig, message).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = enable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "bark",
            Some(&client),
        )
        .await;

        assert_eq!(code, ExitCode::InvalidConfig);
        let text = String::from_utf8(err).unwrap();
        assert!(
            text.contains(message),
            "the daemon's own message must reach the operator: {text}"
        );
    }

    /// The `disable` sibling of
    /// `enable_asks_the_shepherd_to_start_that_dog_as_a_built_in`: fails if
    /// `disable` sends anything but `DisableDog` with the name it was given.
    #[tokio::test]
    async fn disable_asks_the_shepherd_to_stop_that_dog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = disable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "bark",
            Some(&client),
        )
        .await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.body,
            Request::DisableDog {
                name: "bark".to_string(),
            }
        );
    }

    /// The `disable` sibling of
    /// `enable_with_no_shepherd_writes_the_config_and_exits_zero`.
    #[tokio::test]
    async fn disable_with_no_shepherd_writes_the_config_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(&|_| None, dir.path());
        let mut seed = ShepToml::open(&paths.daemon_config).unwrap();
        seed.enable_dog("bark");
        seed.save().unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = disable(
            &mut streams(&mut out, &mut err),
            Format::Table,
            &paths,
            "bark",
        )
        .await;

        assert_eq!(code, ExitCode::Success);
        let written = std::fs::read_to_string(&paths.daemon_config).unwrap();
        let cfg = shep_core::config::DaemonConfig::load(Some(&written), &|_| None).unwrap();
        assert!(
            cfg.daemon.enabled_dogs.is_empty(),
            "disable must remove the name from enabled_dogs: {written}"
        );
    }

    /// `disable` reused `Delete`'s own selector path (Task 6's `rpc.rs`
    /// doc), so a dog not currently registered answers `NotFound` exactly as
    /// `shep stop` would for a selector matching nothing — the config edit
    /// still lands (`disable_with_no_shepherd_writes_the_config_and_exits_zero`
    /// pins that half); this pins that the daemon's own report still reaches
    /// the operator rather than being swallowed as a false success.
    #[tokio::test]
    async fn disable_of_a_dog_the_shepherd_does_not_have_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, _daemon) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = disable_after_config(
            &mut streams(&mut out, &mut err),
            Format::Table,
            "ghost",
            Some(&client),
        )
        .await;
        assert_eq!(code, ExitCode::NotFound);
    }
}
