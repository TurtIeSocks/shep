//! Compile-only check that a crate outside `shep-daemon` can implement the
//! pure-tier seam traits this crate exports. Every method body is
//! `todo!()` and never runs; only compilation is proved.
//!
//! No `#![cfg(unix)]`: these traits are pure tier, so this compiles on
//! every platform.

use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use shep_core::config::ProbeTarget;
use shep_core::values::MemSize;
use shep_daemon::limits::LimitEnforcer;
use shep_daemon::limits::sample::{MemorySampler, ProcessRss};
use shep_daemon::probes::{ProbeFailure, Prober};

struct ExternalSampler;

impl MemorySampler for ExternalSampler {
    fn sample(&self) -> Vec<ProcessRss> {
        todo!()
    }
}

struct ExternalEnforcer;

impl LimitEnforcer for ExternalEnforcer {
    fn arm(&self, _id: u32, _root_pid: u32, _limit: MemSize) {
        todo!()
    }

    fn disarm(&self, _id: u32) {
        todo!()
    }
}

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
