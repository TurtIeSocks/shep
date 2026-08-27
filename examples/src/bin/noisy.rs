//! Writes to both stdout and stderr at a configurable rate, for watching the
//! log plane: `shep bleats`, and the two files each sheep's output lands in.
//!
//! # Usage
//!
//! ```text
//! noisy <lines-per-second>
//! ```

#![forbid(unsafe_code)]

use std::time::Duration;

fn main() {
    let rate = parse_rate(std::env::args().nth(1).as_deref());
    let interval = Duration::from_millis(1000 / u64::from(rate));

    println!(
        "noisy pid={} writing {rate} lines/s to stdout and stderr",
        std::process::id()
    );

    let mut count: u64 = 0;
    loop {
        std::thread::sleep(interval);
        count += 1;
        if count.is_multiple_of(2) {
            println!("noisy: stdout line {count}");
        } else {
            eprintln!("noisy: stderr line {count}");
        }
    }
}

/// Parses the line rate, in lines per second.
///
/// # Panics
///
/// Panics if no argument was given, it does not parse as a `u32`, or it is
/// zero.
fn parse_rate(raw: Option<&str>) -> u32 {
    let raw = raw.unwrap_or_else(|| panic!("usage: noisy <lines-per-second>"));
    let rate: u32 = raw
        .parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid rate: {error}"));
    assert!(rate > 0, "lines-per-second must be greater than zero");
    rate
}
