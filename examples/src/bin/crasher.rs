//! Exits non-zero after a configurable delay, for watching restart policy,
//! backoff and `max_restarts`.
//!
//! # Usage
//!
//! ```text
//! crasher <exit-code> <delay-seconds>
//! ```

#![forbid(unsafe_code)]

fn main() {
    let mut args = std::env::args().skip(1);
    let exit_code = parse_exit_code(args.next().as_deref());
    let delay = parse_delay(args.next().as_deref());

    println!(
        "crasher pid={} will exit {exit_code} after {delay}s",
        std::process::id()
    );
    std::thread::sleep(std::time::Duration::from_secs(delay));
    println!("crasher: exiting {exit_code}");
    std::process::exit(exit_code);
}

/// Parses the exit code this program dies with.
///
/// # Panics
///
/// Panics if no argument was given or it does not parse as an `i32`.
fn parse_exit_code(raw: Option<&str>) -> i32 {
    let raw = raw.unwrap_or_else(|| panic!("usage: crasher <exit-code> <delay-seconds>"));
    raw.parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid exit code: {error}"))
}

/// Parses the delay before exit, in whole seconds.
///
/// # Panics
///
/// Panics if no argument was given or it does not parse as a `u64`.
fn parse_delay(raw: Option<&str>) -> u64 {
    let raw = raw.unwrap_or_else(|| panic!("usage: crasher <exit-code> <delay-seconds>"));
    raw.parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid delay in seconds: {error}"))
}
