//! Proves [`ProcessInfo::builder`] reaches every field from outside
//! shep-core, and that field assignment works across the boundary.
//!
//! Does not prove `ProcessInfo` is `#[non_exhaustive]`. Nothing in the
//! repository guards that attribute; this file only observes what
//! compiles here.
//!
//! shep-core's one `tests/` file, and it must stay the only one. The
//! property needs a real crate boundary, which `#[cfg(test)]` modules
//! inside the crate cannot provide.

// `ProcStatus` is re-exported through the prelude, not through
// `protocol`; `protocol/mod.rs`'s `pub use` list does not name it.
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

    // `dog` is set to a real variant rather than `None`, which is the
    // default and would prove nothing.
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

    // Field assignment is still legal across the boundary: the
    // attribute blocks construction, not mutation, and call sites in
    // shep-cli and shep-daemon depend on that.
    let mut adjusted = info.clone();
    adjusted.pid = None;
    assert_eq!(adjusted.name, info.name);
    assert_eq!(adjusted.pid, None);
}
