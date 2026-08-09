// IR-33: one crate-root fixture module. Every test mod from Task 3 onward
// (and the harness in Tasks 4-5) shares this one `test_paths` helper instead
// of hand-rolling its own.
use std::sync::Arc;

use shep_core::paths::ShepPaths;
use tokio::sync::{broadcast, watch};

use crate::fake::{ProcScript, ScriptedRunner};
use crate::rpc::RpcContext;
use crate::snapshot::FlockRegistry;
use crate::supervisor::spawn_supervisor;

// `FD_REUSE_LOCK` lived here until 2026-08-08. It serialized the tests
// that close a real descriptor and then re-probe that same number, to
// stop them racing the kernel's lowest-available-fd allocation.
//
// It was removed because it could not work. A mutex only excludes the
// tests that TAKE it; every other test in the binary stayed free to open
// a file and be handed the just-closed number, after which `adopt_fd`'s
// `F_GETFD` probe legitimately succeeds and the adoption double-closes
// somebody else's descriptor. That is not hypothetical: it was
// reproduced WITH the lock in place, as `fatal runtime error: IO Safety
// violation: owned file descriptor already closed`, once in 25 saturated
// `--workspace --all-features` runs, taking the whole lib test binary
// down with SIGABRT.
//
// The fix is structural, not exclusive: `sys.rs`'s probe now parks on a
// high fd number (`F_DUPFD`), which the lowest-free allocation policy
// will not hand back while lower numbers remain free. See
// `a_closed_descriptor_is_refused_instead_of_adopted`.

// WHY a shallow home: later tasks bind a UDS under `run/`, and sun_path
// caps a socket path near 104 bytes. Using the tempdir root as
// $SHEP_HOME (no extra nesting) keeps every test in this crate under the
// limit on macOS, whose temp paths are already long.
pub(crate) fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
    let home = dir.path().to_path_buf();
    ShepPaths::resolve(
        &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
        std::path::Path::new("/nonexistent"),
    )
}

// IR-33: `rpc.rs`'s dispatch tests (Task 4) and the connection-server's
// tests (Task 5) need the exact same fixture — one factory, not two.
pub(crate) struct Harness {
    pub(crate) ctx: RpcContext,
    // Kept alive only: dropping the tempdir would remove the paths `ctx`
    // still points at.
    _dir: tempfile::TempDir,
    // Kept alive only: dropping the sender's last receiver would turn
    // every future `events.send()` into a silent no-op.
    _events_rx: broadcast::Receiver<shep_core::protocol::BusEvent>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
}

/// Builds one supervisor engine (a [`ScriptedRunner`] replaying `scripts`)
/// plus a fresh [`RpcContext`] wired to it.
pub(crate) fn harness(scripts: Vec<ProcScript>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = test_paths(&dir);
    let (events, events_rx) = broadcast::channel(256);
    let supervisor = spawn_supervisor(ScriptedRunner::new(scripts), paths.clone(), events.clone());
    let (shutdown, shutdown_rx) = watch::channel(false);
    Harness {
        ctx: RpcContext {
            supervisor,
            events,
            registry: FlockRegistry::new(),
            snapshot_path: paths.snapshot.clone(),
            daemon_version: "0.1.0".to_string(),
            pid: 4242,
            shutdown: Arc::new(shutdown),
        },
        _dir: dir,
        _events_rx: events_rx,
        shutdown_rx,
    }
}
