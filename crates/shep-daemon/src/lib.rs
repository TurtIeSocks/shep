//! The daemon: a process-supervision engine plus the control plane that
//! exposes it over a unix socket
//!
//! Every command that reaches the flock goes through one
//! [`SupervisorHandle`](supervisor::SupervisorHandle). Pure decision logic
//! (brain, backoff, entry assembly) is IO-free, so it tests deterministically
//! under a paused tokio clock. `RpcServer` exposes the engine to a CLI client
//! over `$SHEP_HOME/run/shep.sock`, and [`boot`] assembles both into one
//! running daemon. The CLI re-executes itself with a hidden `daemon`
//! subcommand to daemonize.
//!
//! ## Module taxonomy
//!
//! A linked name below is public; a name in plain backticks is crate-private
//! and has no rendered page to link to. The split is the crate's API
//! boundary: the two commented blocks of module declarations say which
//! consumer holds each public one open.
//!
//! ##### Engine
//!
//! Process-lifecycle decision logic and the actor that runs it.
//!
//! - `brain`: restart decision tree given exit outcome, uptime, and budget
//! - `backoff`: restart delay per the spec's exponential backoff rule
//! - [`assemble`]: process env, log paths, and spawn spec assembly
//! - `entry`: process lifecycle state, restart budget, reload state machine
//! - [`runner`]: [`ProcessRunner`](runner::ProcessRunner) spawn seam, two impls
//! - `fake`: deterministic scripted runner, absent from a default-features build
//! - `kill`: the kill ladder, portable and generic over the process handle
//! - [`supervisor`]: the actor: owns entries, spawns per-sheep tasks, routes commands
//! - [`channel`]: shepherd channel codec (child↔daemon messages, newline-JSON)
//! - `cron`: the `Clock` seam and the `cron_restart` worker
//! - [`limits`]: the [`MemorySampler`](limits::sample::MemorySampler) seam over
//!   a sheep's process tree, and the enforcer that reports a `max_memory` breach
//! - [`probes`]: the [`Prober`](probes::Prober) seam and the liveness loop that
//!   reports a sheep unhealthy after `failure_threshold` consecutive failures;
//!   `os::OsProber` is the hand-rolled HTTP/TCP/exec implementation
//! - `watch`: the `WatchSource` seam over notify's debounced events
//! - `extras`: arms the four subsystems above while a sheep is online, and
//!   turns a memory breach or a liveness failure into a guarded restart
//!
//! ##### Plane
//!
//! The control plane a CLI client talks to: event bus, request dispatch, the
//! socket, the persisted muster roll, and the boot sequence wiring it all.
//!
//! - `bus`: the event bus: topic-glob filtering, per-subscriber forwarder tasks
//! - [`rpc`]: verb routing onto
//!   [`SupervisorHandle`](supervisor::SupervisorHandle), typed errors, deadlines
//! - [`dogs`]: what a dog is spawned as ([`dog_app`](dogs::dog_app)) and the
//!   `[<name>]` section, read from `dogs.toml`, served back to it over the
//!   socket ([`dog_section`](dogs::dog_section))
//! - `server`: the connection layer: peer-cred auth, handshake, subscriptions
//! - [`snapshot`]: the muster roll: debounced atomic `flock.json` writes, restore
//! - [`boot`]: `0700` layout dirs, pidfile, socket bind with stale-socket
//!   recovery, the readiness pipe, signal handlers, ordered teardown (unix only)
//!
//! ##### Platform
//!
//! Platform glue underneath both tiers above.
//!
//! - `sys`: adopting an inherited descriptor, this crate's only unsafe surface
//!   on unix (unix only)
//! - [`privilege`]: `user`/`group` config to numeric uid/gid, one portable
//!   `resolve()` over a unix impl and a non-unix stub that refuses outright
//! - [`notify`]: one `READY=1` datagram to `$NOTIFY_SOCKET`, sent by [`boot`]
//!   once the muster restore has finished (unix only)
//! - [`tokio_runner`]: real [`ProcessRunner`](runner::ProcessRunner) over
//!   `tokio::process` (unix only)
//!
//! # Quick start
//!
//! Builds a supervisor engine with a scripted fake runner, registers one app,
//! and lists the live processes. Needs `--all-features` (`test-fakes`).
//!
//! ```no_run
//! # #[cfg(feature = "test-fakes")]
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use shep_daemon::fake::{ProcScript, ScriptedRunner};
//! use shep_daemon::supervisor::spawn_supervisor;
//! use shep_core::config::AppConfig;
//! use shep_core::config::normalize;
//! use shep_core::paths::ShepPaths;
//! use std::path::Path;
//!
//! // Create a fake runner that spawns one process never exiting
//! let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
//!
//! // Set up temporary paths for this example
//! let paths = ShepPaths::resolve(&|_| None, Path::new("/tmp/shep-example"));
//!
//! // Create the event bus every subscriber reads
//! let events = shep_daemon::new_bus();
//!
//! // Spawn the supervisor actor
//! let handle = spawn_supervisor(runner, paths, events);
//!
//! // Build one app config and normalize it
//! let app = AppConfig::minimal("web", "./server");
//! let resolved = normalize(app)?;
//!
//! // Start the app (creates one instance)
//! let infos = handle.start(vec![resolved]).await?;
//! println!("Started: {} instance(s)", infos.len());
//!
//! // List all registered processes
//! let list = handle.list().await;
//! for info in &list {
//!     println!("  ID {} ({}): {:?}", info.id, info.name, info.status);
//! }
//!
//! // Gracefully shut down all processes
//! handle.shutdown().await;
//!
//! Ok(())
//! # }
//! # #[tokio::main]
//! # async fn main() {
//! #     #[cfg(feature = "test-fakes")]
//! #     example().await.ok();
//! # }
//! ```
//!
//! ## Reference
//!
//! [`ProcessRunner`](runner::ProcessRunner) spawns a child process and returns
//! a [`RunningProcess`](runner::RunningProcess) handle plus a
//! [`ProcIo`](runner::ProcIo) bundle with channels for logs and shepherd
//! messages. [`spawn_supervisor`](supervisor::spawn_supervisor) wires these
//! together into the core actor loop.
//!
//! # Quick start
//!
//! Boots a full daemon through [`boot`] on a temporary `$SHEP_HOME` with the
//! same scripted fake runner, then round-trips one `Ping` over the wire codec
//! `server` speaks. Needs `--all-features` on a unix target.
//!
//! ```no_run
//! # #[cfg(all(unix, feature = "test-fakes"))]
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use shep_daemon::boot::{BootOptions, boot};
//! use shep_daemon::fake::ScriptedRunner;
//! use shep_core::paths::ShepPaths;
//! use shep_core::protocol::{
//!     Envelope, Hello, HelloReply, PROTOCOL_VERSION, Request, ServerFrame, codec, decode_frame,
//!     encode_frame,
//! };
//! use tokio::net::UnixStream;
//! use tokio_util::codec::Framed;
//! use futures_util::{SinkExt, StreamExt};
//!
//! // A throwaway $SHEP_HOME: `boot` creates its 0700 layout inside it.
//! let paths = ShepPaths::resolve(&|_| None, std::path::Path::new("/tmp/shep-daemon-example"));
//!
//! // Boot with the scripted fake runner — no real children, just the plane.
//! let daemon = boot(ScriptedRunner::new(vec![]), paths, BootOptions::default()).await?;
//! let socket = daemon.socket().to_path_buf();
//! tokio::spawn(daemon.run());
//!
//! // Connect and speak the wire protocol directly: Hello, then Ping.
//! let stream = UnixStream::connect(&socket).await?;
//! let mut frames = Framed::new(stream, codec());
//! frames
//!     .send(encode_frame(&Hello {
//!         client_version: "0.1.0".to_string(),
//!         protocol: PROTOCOL_VERSION,
//!         // Only a dog names one; see `Hello::dog_name`.
//!         dog_name: None,
//!     })?)
//!     .await?;
//! let ack: HelloReply = decode_frame(&frames.next().await.unwrap()?)?;
//! let ack = ack.expect("the daemon must ack our protocol");
//! println!("daemon pid: {}", ack.pid);
//!
//! frames
//!     .send(encode_frame(&Envelope {
//!         id: 1,
//!         deadline_ms: Some(1_000),
//!         body: Request::Ping,
//!     })?)
//!     .await?;
//! let frame: ServerFrame = decode_frame(&frames.next().await.unwrap()?)?;
//! println!("reply: {frame:?}");
//!
//! Ok(())
//! # }
//! # #[tokio::main]
//! # async fn main() {
//! #     #[cfg(all(unix, feature = "test-fakes"))]
//! #     example().await.ok();
//! # }
//! ```

#![doc(test(attr(deny(warnings))))]
#![deny(unsafe_code)]

// Internal tier: nothing outside this crate's own `src` names these. A dog is
// a separate process speaking the protocol rather than linking this crate, so
// a dog author builds against `shep-core`. Widening one back to `pub` is an
// API decision.
pub(crate) mod backoff;
pub(crate) mod brain;
pub(crate) mod bus;
// The bus surface a caller of `supervisor::spawn_supervisor` needs, and no
// more: the module stays crate-private so its forwarders and topic
// bookkeeping never become API.
pub use bus::{Bus, SharedEvent, new_bus};
pub(crate) mod cron;
pub(crate) mod entry;
pub(crate) mod extras;
// Unix only: `fcntl`, `execve` and raw descriptor numbers have no Windows
// equivalent, and `Arm::for_daemon` already returns the stop arm there.
#[cfg(unix)]
pub(crate) mod handover;
pub(crate) mod kill;
pub(crate) mod watch;

// Reachable tier: each of these is named from outside this crate's `src`, by
// `shep-cli`, an integration test, the bench crate, or a doc example. Every
// module here carries a note in its own header saying which consumer holds it
// open.
pub mod assemble;
pub mod channel;
// A `dogs::DogSpec` says which dogs to run and where their binaries come
// from, an answer only `shep.toml` holds, and every `shep.toml` read in this
// project happens in `shep-cli`. So the assembling caller is out of crate by
// construction.
pub mod dogs;
pub mod limits;
pub mod privilege;
pub mod probes;
pub mod rpc;
pub mod runner;
pub mod snapshot;
pub mod supervisor;

use std::time::SystemTime;

/// The one real-time read shared across this crate: wall-clock milliseconds
/// since the Unix epoch, for [`BusEvent::Process::at_ms`](shep_core::protocol::BusEvent::Process)
/// and [`FlockSnapshot::saved_at_ms`](snapshot::FlockSnapshot::saved_at_ms).
/// Everything else in the engine uses the paused-clock-aware
/// `tokio::time::Instant`.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod testing;

// Reachable tier: `shep-cli`'s hidden `daemon` subcommand calls `boot` and
// `RunningDaemon::run`, and `launch.rs` reads `DIR_MODE`. Portable on both
// platforms; `DIR_MODE` alone is gated unix-only. Doc lives inside boot.rs's
// own `//!` header, for the reason `server`'s note below gives.
pub mod boot;

// Unix only: `FromRawFd`/`RawFd` and this crate's whole unsafe surface there.
// `pub` for a consumer not written yet: `boot` does not call `adopt_fd`, so
// the ordering precondition needs a caller running before any runtime exists,
// `shep-cli`'s `main`. Doc lives inside sys.rs's own `//!` header.
#[cfg(unix)]
pub mod sys;

// The Windows counterpart to `sys`, and this crate's only unsafe surface
// there: a job object per sheep, standing in for the unix process group.
// `pub` because `seal_std_handles`' caller lives in shep-cli; `Job` stays
// `pub(crate)`, so nothing outside this crate can name a job object.
#[cfg(windows)]
pub mod sys_windows;

// Unix only: `UnixDatagram`, plus Linux's abstract-namespace address. Doc
// lives inside notify.rs's own `//!` header. `shep-cli`'s hidden `daemon`
// subcommand names `NOTIFY_SOCKET_ENV`, since every environment read happens
// there; this crate receives the resolved address instead.
#[cfg(unix)]
pub mod notify;

/// Real [`ProcessRunner`](runner::ProcessRunner) over actual OS processes.
///
/// Unix only: built on `nix` process-group signals and `command-fds` fd-3
/// passing, both `#[cfg(unix)]` deps. Public because `shep-cli` hands
/// [`TokioRunner`](tokio_runner::TokioRunner) to [`boot`], and
/// `tests/real_runner.rs` drives it against real children.
pub mod tokio_runner;

// Portable: it names `shep_core::transport`, so one accept loop and
// connection state machine serve both platforms; the same-uid `peer_cred`
// check is `#[cfg(unix)]` inside. Doc lives inside server.rs's own `//!`
// header: an outer `///` here would merge with it and misresolve its links.
pub(crate) mod server;

/// Deterministic scripted [`ProcessRunner`](runner::ProcessRunner), reused by
/// this crate's own tests and, behind `test-fakes`, by any other crate's.
///
/// Public because both doc examples above build one, and rustdoc compiles a
/// doc example as its own crate.
#[cfg(any(test, feature = "test-fakes"))]
pub mod fake;
