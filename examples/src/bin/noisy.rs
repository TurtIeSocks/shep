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
    let interval = pace(rate);

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

/// Turns a rate into the sleep between lines.
///
/// `Duration::from_secs_f64`, not integer millisecond division: above
/// 1000/s, integer division floors to zero and the loop busy-spins.
/// Near `u32::MAX`, the computed interval itself can round to zero.
/// The check runs on the interval rather than a numeric threshold.
///
/// # Panics
///
/// Panics if `rate` is high enough that the resulting interval is zero.
fn pace(rate: u32) -> Duration {
    let interval = Duration::from_secs_f64(1.0 / f64::from(rate));
    assert!(
        !interval.is_zero(),
        "{rate} lines-per-second is too high to pace"
    );
    interval
}
