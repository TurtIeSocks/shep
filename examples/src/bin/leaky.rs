//! Allocates on a timer and holds the memory, so a Flockfile's `max_memory`
//! fires.
//!
//! Each tick allocates a chunk, writes to every page in it (an untouched
//! allocation may never become resident, and `max_memory` is enforced against
//! resident memory), and keeps it — nothing here is ever freed until the
//! process is restarted.
//!
//! # Usage
//!
//! ```text
//! leaky <mb-per-tick> <interval-ms>
//! ```

#![forbid(unsafe_code)]

use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let mb_per_tick = parse_mb(args.next().as_deref());
    let interval = parse_interval(args.next().as_deref());

    println!(
        "leaky pid={} allocating {mb_per_tick}MB every {}ms",
        std::process::id(),
        interval.as_millis()
    );

    let mut held = Vec::new();
    let mut total_mb: u64 = 0;
    loop {
        std::thread::sleep(interval);

        let mut chunk = vec![0_u8; mb_per_tick as usize * 1024 * 1024];
        // Touch every page so the allocation is actually resident, not just
        // reserved address space the OS never backs with physical memory.
        for byte in chunk.iter_mut().step_by(4096) {
            *byte = 1;
        }
        held.push(chunk);

        total_mb += u64::from(mb_per_tick);
        println!("leaky: holding ~{total_mb}MB");
    }
}

/// Parses the per-tick allocation size, in megabytes.
///
/// # Panics
///
/// Panics if no argument was given, it does not parse as a `u32`, or it is
/// zero.
fn parse_mb(raw: Option<&str>) -> u32 {
    let raw = raw.unwrap_or_else(|| panic!("usage: leaky <mb-per-tick> <interval-ms>"));
    let mb: u32 = raw
        .parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid megabyte count: {error}"));
    assert!(mb > 0, "mb-per-tick must be greater than zero");
    mb
}

/// Parses the tick interval, in milliseconds.
///
/// # Panics
///
/// Panics if no argument was given or it does not parse as a `u64`.
fn parse_interval(raw: Option<&str>) -> Duration {
    let raw = raw.unwrap_or_else(|| panic!("usage: leaky <mb-per-tick> <interval-ms>"));
    let ms: u64 = raw
        .parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid interval in ms: {error}"));
    Duration::from_millis(ms)
}
