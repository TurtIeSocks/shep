//! Compile-only proof (IR-38) that a crate outside `shep-daemon` can
//! implement the pure-tier seam traits this crate exports.
//!
//! Every method body below is `todo!()` and the one `#[test]` in this file
//! never calls one — the proof is that this file compiles against the real
//! trait definitions, not that these fakes behave like anything real.
//! `daemon_e2e.rs` and `real_runner.rs` are this crate's two *behavioral*
//! integration tiers, each carrying its own IR-38 deviation note; this is
//! the crate's one actual compile-only file and is unaffected by either.
//!
//! No `#![cfg(unix)]`: every trait named here is pure tier, and gating this
//! file would drop the proof from the Windows CI leg — the leg most likely
//! to break it.

use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use shep_core::config::ProbeTarget;
use shep_core::values::MemSize;
use shep_daemon::limits::LimitEnforcer;
use shep_daemon::limits::sample::{MemorySampler, ProcessRss};
use shep_daemon::probes::{ProbeFailure, Prober};

/// An external crate's `MemorySampler`.
struct ExternalSampler;

impl MemorySampler for ExternalSampler {
    fn sample(&self) -> Vec<ProcessRss> {
        todo!()
    }
}

/// An external crate's `LimitEnforcer`.
struct ExternalEnforcer;

impl LimitEnforcer for ExternalEnforcer {
    fn arm(&self, _id: u32, _root_pid: u32, _limit: MemSize) {
        todo!()
    }

    fn disarm(&self, _id: u32) {
        todo!()
    }
}

/// An external crate's `Prober`.
struct ExternalProber;

impl Prober for ExternalProber {
    fn probe<'a>(
        &'a self,
        _target: &'a ProbeTarget,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProbeFailure>> + Send + 'a>> {
        todo!()
    }
}

#[test]
fn external_types_satisfy_the_traits() {
    let sampler = ExternalSampler;
    let enforcer = ExternalEnforcer;
    let prober = ExternalProber;
    let _: &dyn MemorySampler = &sampler;
    let _: &dyn LimitEnforcer = &enforcer;
    let _: &dyn Prober = &prober;
}
