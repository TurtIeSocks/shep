//! Proves [`ProcessInfo::builder`] reaches every field from outside
//! shep-core, and that field assignment still works across the boundary.
//!
//! It does **not** prove `ProcessInfo` is `#[non_exhaustive]`, and the
//! filename deliberately does not claim otherwise. That attribute is
//! invisible inside the defining crate and this file's `assert`s run in a
//! separate crate but still only observe what compiles here — nothing in the
//! repository guards the attribute itself. The only thing that would is a
//! `trybuild` compile-fail pair asserting E0639, which Phase 10 declined as
//! a whole new test tier for one attribute.
//!
//! This file is a deliberate **exception** to IR-38, not an application of
//! it. IR-38 reads: "`tests/` dir = at most one compile-only file per crate
//! proving an external crate can implement the public trait (`todo!()`
//! bodies fine). Everything behavioral is co-located `#[cfg(test)]`." This
//! file has assertions and is therefore behavioral, so IR-38 does not permit
//! it. It earns the exception on the same grounds IR-38's own carve-out
//! rests on — the property needs a real crate boundary to observe, and
//! shep-core's `#[cfg(test)]` modules are inside the boundary. It is
//! shep-core's one `tests/` file and must stay the only one.

// `ProcStatus` lives at `shep_core::status` and is re-exported through the
// prelude, NOT through `protocol` — `protocol/mod.rs`'s `pub use` list does
// not name it. Two imports, deliberately, rather than one wrong one.
use shep_core::prelude::ProcStatus;
use shep_core::protocol::{DogSource, ExitInfo, Lamb, ProcessInfo};

#[test]
fn the_builder_reaches_every_field_from_outside_the_crate() {
    let info = ProcessInfo::builder(1, "web", ProcStatus::Online)
        .pid(Some(1))
        .restarts(1)
        .uptime_ms(1)
        .fold(Some("f".to_string()))
        .out_file(Some("o".to_string()))
        .err_file(Some("e".to_string()))
        .cpu_percent(Some(1.0))
        .memory_bytes(Some(1))
        .dog(Some(DogSource::BuiltIn))
        .lambs(Some(vec![Lamb::new(4243, "node")]))
        .last_exit(Some(ExitInfo {
            code: Some(1),
            signal: None,
        }))
        .build();

    // Every field, read back across the boundary. `dog` is set to a real
    // variant rather than `None` for the same reason Step 2.3's second
    // assertion exists: `None` is the default, so it proves nothing.
    assert_eq!(info.id, 1);
    assert_eq!(info.name, "web");
    assert_eq!(info.status, ProcStatus::Online);
    assert_eq!(info.pid, Some(1));
    assert_eq!(info.restarts, 1);
    assert_eq!(info.uptime_ms, 1);
    assert_eq!(info.fold.as_deref(), Some("f"));
    assert_eq!(info.out_file.as_deref(), Some("o"));
    assert_eq!(info.err_file.as_deref(), Some("e"));
    assert_eq!(info.cpu_percent, Some(1.0));
    assert_eq!(info.memory_bytes, Some(1));
    assert_eq!(info.dog, Some(DogSource::BuiltIn));
    assert_eq!(info.lambs, Some(vec![Lamb::new(4243, "node")]));
    assert_eq!(
        info.last_exit,
        Some(ExitInfo {
            code: Some(1),
            signal: None,
        })
    );

    // Field ASSIGNMENT is still legal across the boundary — the attribute
    // blocks construction, not mutation, and several call sites in shep-cli
    // and shep-daemon depend on that.
    let mut adjusted = info.clone();
    adjusted.pid = None;
    assert_eq!(adjusted.name, info.name);
    assert_eq!(adjusted.pid, None);
}
