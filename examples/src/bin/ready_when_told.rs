//! Exits `0` once a sentinel file exists, and non-zero otherwise.
//!
//! Not the supervised app itself — the `target` an `exec` readiness probe
//! runs. There is no everyday real-world example of an exec probe the way
//! there is for HTTP or TCP, since most readiness is a socket coming up; this
//! is the minimal program that makes one demonstrable: create the sentinel
//! file by hand (`touch <path>`) and watch the next poll flip the sheep to
//! ready.
//!
//! # Usage
//!
//! ```text
//! ready_when_told <sentinel-path>
//! ```

#![forbid(unsafe_code)]

use std::path::Path;

fn main() {
    let raw = std::env::args()
        .nth(1)
        .unwrap_or_else(|| panic!("usage: ready_when_told <sentinel-path>"));

    if Path::new(&raw).exists() {
        std::process::exit(0);
    }
    eprintln!("ready_when_told: {raw} does not exist yet");
    std::process::exit(1);
}
