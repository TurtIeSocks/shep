//! The daemon: a process-supervision engine plus the control plane that
//! exposes it over a unix socket
//!
//! Owned by the daemon: spawns processes via [`ProcessRunner`](runner::ProcessRunner),
//! orchestrates restart policy, tracks logs and shepherd-channel traffic, and
//! gates lifecycle commands (start, stop, restart, delete, shutdown) through a
//! single [`SupervisorHandle`](supervisor::SupervisorHandle). Pure decision logic
//! (brain, backoff, entry assembly) is IO-free for deterministic testing under a
//! paused tokio clock. `RpcServer` exposes that engine to a CLI client over
//! `$SHEP_HOME/run/shep.sock`, and [`boot`] assembles both into one running
//! daemon. The OS tier (real spawning, signal delivery, the kill ladder, the
//! socket itself) is unix-only. Production daemon embeds this crate; the CLI
//! re-executes itself with a hidden `daemon` subcommand to daemonize.
//!
//! ## Module taxonomy
//!
//! A linked name below is public; a name in plain backticks is crate-private
//! and has no rendered page to link to. The split is the crate's API boundary,
//! not a formatting accident — see the two commented blocks of module
//! declarations for which consumer holds each public one open.
//!
//! ##### Engine
//!
//! Process-lifecycle decision logic and the actor that runs it (2a).
//!
//! - `brain`: restart decision tree given exit outcome, uptime, and budget
//! - `backoff`: restart delay computation per the spec's exponential backoff rule
//! - [`assemble`]: process env, log paths, and spawn spec assembly
//! - `entry`: process lifecycle state, restart budget, reload state machine
//! - [`runner`]: [`ProcessRunner`](runner::ProcessRunner) spawn seam with two impls
//! - `fake`: deterministic scripted [`ProcessRunner`](runner::ProcessRunner) (test-only, or test-fakes feature — not linked here since it is absent from a default-features doc build)
//! - `kill`: kill ladder — SIGTERM, SIGKILL escalation (portable, generic over [`RunningProcess`](runner::RunningProcess))
//! - [`supervisor`]: the actor — owns registered entries, spawns per-sheep tasks, routes commands
//! - [`channel`]: shepherd channel codec (child↔daemon messages, newline-JSON)
//! - `cron`: the `Clock` seam and the worker that restarts a name-group on
//!   its `cron_restart` schedule
//! - [`limits`]: the [`MemorySampler`](limits::sample::MemorySampler) seam
//!   over a sheep's process tree, and the polling enforcer that consumes it —
//!   reports a breach once a sheep's tree exceeds its `max_memory`
//! - [`probes`]: the [`Prober`](probes::Prober) seam and the liveness probe
//!   loop — reports a sheep's health once `failure_threshold` consecutive
//!   probes have failed; `os::OsProber` is the concrete hand-rolled
//!   HTTP/TCP/exec implementation
//! - `watch`: the `WatchSource` OS seam — bridges notify's debounced
//!   filesystem events onto a tokio channel
//! - `extras`: the `ExtrasRegistry` that arms the four subsystems above when
//!   a sheep goes online and disarms them when it goes terminal, plus the
//!   reporting task that turns a memory breach or a liveness failure into a
//!   guarded restart
//!
//! ##### Plane
//!
//! The control plane a CLI client talks to (2b): event bus, request dispatch,
//! the socket itself, the persisted muster roll, and the boot sequence that
//! wires all of the above (plus the engine above) into one daemon.
//!
//! - `bus`: the daemon-wide event bus — topic-glob filtering, per-subscriber forwarder tasks
//! - [`rpc`]: request dispatch — verb routing onto [`SupervisorHandle`](supervisor::SupervisorHandle), typed errors, per-call deadlines
//! - `server`: the unix-socket connection layer — peer-cred auth, handshake, subscriptions (unix-only)
//! - [`snapshot`]: the muster roll — debounced atomic `flock.json` writes, restart-survival restore
//! - [`boot`]: daemon boot — `0700` layout dirs, pidfile, socket bind with stale-socket
//!   recovery, the readiness pipe, signal handlers, and the ordered teardown sequence (unix-only)
//!
//! ##### Platform
//!
//! Unix-specific glue underneath both tiers above; nothing here is reachable
//! from a portable (non-unix) build except [`privilege`]'s refuse-outright stub.
//!
//! - [`sys`]: adopting an inherited descriptor — this crate's only `unsafe fn`, and its
//!   only unsafe surface, full stop (IR-22): [`boot`] receives an already-adopted
//!   [`std::fs::File`] and calls it not at all (`sys`'s own doc has the full
//!   test-call-site accounting) (unix-only)
//! - [`privilege`]: `user`/`group` config -> numeric uid/gid, one portable `resolve()`
//!   signature over a real unix impl and a refuse-outright non-unix stub
//! - [`tokio_runner`]: real [`ProcessRunner`](runner::ProcessRunner) over `tokio::process` (unix-only)
//!
//! # Quick start
//!
//! This example builds a supervisor engine with a scripted fake runner,
//! registers one app, and lists the live processes.
//! Compile with `--all-features` (the example requires `test-fakes`).
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
//! // Create a broadcast channel for events
//! let (events, _rx) = tokio::sync::broadcast::channel(64);
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
//! a [`RunningProcess`](runner::RunningProcess) handle plus a [`ProcIo`](runner::ProcIo)
//! bundle with channels for logs and shepherd messages. The fake runner
//! (`ScriptedRunner`, in the test-only `fake` module) drives deterministic tests;
//! [`spawn_supervisor`](supervisor::spawn_supervisor) wires these together into
//! the core actor loop.
//!
//! # Quick start
//!
//! This second example boots a full daemon — layout, socket, and RPC server,
//! via [`boot`] — on a temporary `$SHEP_HOME`, using the same scripted fake
//! runner as the example above so it stays hermetic (no real child
//! processes), then connects a raw client to the control socket and
//! round-trips one `Ping` over the same wire codec `server` itself speaks.
//! Compile with `--all-features` on a unix target (`test-fakes` for the fake
//! runner, plus the unix-only [`boot`]/`server` modules this example needs).
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

// Internal tier: nothing outside this crate's own `src` names these, so they
// are not API. A dog is a separate process that speaks the protocol rather
// than linking this crate, so what a dog author builds against is
// `shep-core`; this crate's supervision internals are not part of that
// contract. Widening one back to `pub` is a deliberate API decision, and the
// module's own header says what would justify it where the question is live.
pub(crate) mod backoff;
pub(crate) mod brain;
pub(crate) mod bus;
pub(crate) mod cron;
pub(crate) mod entry;
pub(crate) mod extras;
pub(crate) mod kill;
pub(crate) mod watch;

// Reachable tier: each of these is named from outside this crate's `src` —
// by `shep-cli`, by an integration test under `tests/`, by the bench crate,
// or by a doc example (which rustdoc compiles as its own crate). Every
// module here carries a note in its own header saying which consumer holds
// it open.
pub mod assemble;
pub mod channel;
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

// Unix-only: built on `std::os::unix::fs::PermissionsExt` (directory mode
// bits) and `tokio::net::UnixListener`. Doc lives inside boot.rs's own
// `//!` header (not here) — see the comment on `server` below for why an
// outer `///` here would be the wrong place for it.
//
// Reachable tier: `shep-cli`'s hidden `daemon` subcommand calls `boot` and
// `RunningDaemon::run`, and `launch.rs` reads `DIR_MODE`.
#[cfg(unix)]
pub mod boot;

// Unix-only: `std::os::unix::io::{FromRawFd, RawFd}` and this crate's whole
// unsafe surface (adopting the readiness pipe's inherited descriptor —
// this module's own definition plus its own test-only call sites, and
// nothing outside this file; see sys.rs's own doc for the full accounting)
// have no portable equivalent. Doc lives inside sys.rs's own `//!` header
// (IR-24's rationale essay), not here — same reasoning as `server`'s note
// below.
//
// Reachable tier, and the one entry here whose consumer is not written yet:
// `boot` deliberately does not call `adopt_fd` (that is what removed the
// fd-recycling hazard sys.rs's scenario (c) describes), so the ordering
// precondition can only be discharged by a caller that runs before any
// runtime exists — `shep-cli`'s `main`. `pub(crate)` compiles but leaves the
// whole module dead outside its own tests, which is a worse signal than
// this note: it would read as unused code rather than as a seam waiting on
// its documented caller.
#[cfg(unix)]
pub mod sys;

/// Real [`ProcessRunner`](runner::ProcessRunner) over actual OS processes.
///
/// Unix-only: it's built on `nix` (process-group signals) and `command-fds`
/// (fd-3 passing), both `#[cfg(unix)]` deps (see this crate's `Cargo.toml`).
/// The pure tier above (types, traits, the scripted fake) compiles on every
/// platform; only this OS tier is gated out on Windows.
///
/// Public because `shep-cli` hands [`TokioRunner`](tokio_runner::TokioRunner)
/// to [`boot`], and `tests/real_runner.rs` drives it against real children.
#[cfg(unix)]
pub mod tokio_runner;

// Unix-only for the same reason as `tokio_runner` above: it is built on
// `tokio::net`'s unix-socket types. Task 4's `rpc` dispatcher (and
// everything it calls) stays portable; this module is the thing that
// actually opens a socket. Doc lives inside server.rs's own `//!` header
// (not here) — an outer `///` doc on this declaration would merge with
// that inner doc and rustdoc would resolve the WHOLE merged block's
// intra-doc links against this file's scope, breaking every bare
// same-module link inside server.rs's own header (confirmed by a minimal
// repro during Task 5's docs gate).
//
// Internal tier: `boot` constructs and runs the server; nothing outside this
// crate names it.
#[cfg(unix)]
pub(crate) mod server;

/// Deterministic scripted [`ProcessRunner`](runner::ProcessRunner), reused by
/// this crate's own tests and, behind `test-fakes`, by any other crate's.
///
/// Public because both doc examples above build one, and rustdoc compiles a
/// doc example as its own crate — not because anything ships against it.
#[cfg(any(test, feature = "test-fakes"))]
pub mod fake;
