//! The measurement `MEMORY_POLL_INTERVAL` (`shep-daemon/src/limits/mod.rs`)
//! is built on: how long one poll of the memory-limit enforcer actually
//! costs, split into its two independent halves.
//!
//! `tree_rss` benches the pure summation over a synthetic 500-process table
//! with a realistic tree shape (a root, four-wide branching, several
//! generations — see [`synthetic_process_tree`]). It is deterministic: same
//! input every run, `black_box` on the table and the root pid going in and
//! on the returned sum coming out, so the optimizer cannot fold the call
//! away. This half scales with **`table.len()`**, the size of the whole
//! input table: `tree_rss` builds three `HashMap`s over every entry in
//! `table` before it walks the tree (`limits/sample.rs`'s `tree_rss`), so
//! its cost is not confined to the flock subtree it sums. In production that
//! table is the host's entire process list — `limits/sample.rs`'s own comment
//! on the walk notes "the whole-machine table this feeds from can run to
//! hundreds of entries" — so `tree_rss`'s real-world cost driver is host
//! process count, the same driver `sysinfo_sampler` measures below, not an
//! independent axis. This fixture cannot separate the two: every one of its
//! 500 entries is a descendant of the benched root, so here `table.len()`
//! and flock size are equal by construction, and the number below is really
//! measuring table-size cost, not flock-size cost.
//!
//! `SysinfoSampler::sample()` benches the real `/proc` walk (or the
//! platform-equivalent syscalls sysinfo makes) against whatever machine runs
//! this bench. It is **not deterministic** — its cost scales with the
//! *host's* total process count, which is exactly the quantity
//! `MEMORY_POLL_INTERVAL` has to budget for, and which differs between a
//! laptop and a loaded CI runner. That non-determinism is why this bench's
//! output is a comment recording one measured run, not an assertion: there
//! is no "correct" number to assert against, only a number worth writing
//! down next to the constant it justifies.
//!
//! Run with `cargo bench --manifest-path benches/Cargo.toml`. CI runs
//! `cargo bench --manifest-path benches/Cargo.toml -- --test`, which
//! exercises each benchmark exactly once and asserts nothing — the point on
//! a shared runner is proving the harness still compiles and executes, not
//! collecting a timing.
//!
//! ## Measured
//!
//! 2026-08-12, Apple M4 Pro (aarch64-apple-darwin), macOS 26.2 (Darwin
//! 25.2.0), `cargo bench --manifest-path benches/Cargo.toml`, criterion
//! 0.7.0, release profile:
//!
//! - `tree_rss/500_process_tree`: 34.26 µs per call (100-sample estimate,
//!   [34.171, 34.368] µs) — this is `table.len() == 500` cost, not flock-size
//!   cost; see the driver discussion above.
//! - `sysinfo_sampler/sample_real_machine`: 6.51 ms per call (100-sample
//!   estimate, [6.3499, 6.6781] ms; host process count at measurement time:
//!   871, per `ps aux | wc -l`)
//!
//! Both moved when the sampler started reading CPU time alongside memory, and
//! both moves are accounted for: `tree_rss` was 22.98 µs when the index built
//! two maps per table and now builds three, and `SysinfoSampler::sample` was
//! 5.77 ms before its refresh asked for `.with_cpu()`. A poll still costs
//! single-digit milliseconds against a 15 s interval, so the reasoning at
//! `MEMORY_POLL_INTERVAL` is unchanged.
//!
//! Re-run and update this comment if the numbers drift enough to change the
//! reasoning at the constant they justify — this is a recorded observation,
//! not a regression gate.

use std::collections::VecDeque;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use shep_daemon::limits::sample::{MemorySampler, ProcessRss, SysinfoSampler, tree_rss};

// How many direct children each process gets before the next process in
// insertion order starts collecting its own. 4 keeps the 500-process table
// several generations deep rather than either a single flat generation
// (every process a direct child of the root, which would exercise only the
// `HashMap` lookups `tree_rss` builds) or a single deep chain (every process
// an only child, which would exercise only stack/loop depth). A real
// supervised tree — a sheep spawning a handful of workers, each spawning a
// handful more — sits between those two extremes, which is what this shape
// approximates.
const BRANCHING_FACTOR: u32 = 4;

// Builds a deterministic table of `total` processes rooted at pid 1, using
// [`BRANCHING_FACTOR`]-wide breadth-first assignment: the root gets the
// first few pids as children, each of those gets its own children next, and
// so on until `total` pids have been placed. Every run of this function
// produces byte-for-byte the same table, which is what makes the `tree_rss`
// benchmark below deterministic.
fn synthetic_process_tree(total: u32) -> Vec<ProcessRss> {
    const ROOT_BYTES: u64 = 20 * 1024 * 1024; // 20 MiB, a plausible root RSS
    const CHILD_BYTES: u64 = 512 * 1024; // 512 KiB, a plausible lamb RSS
    const ROOT_CPU_MS: u64 = 90_000; // 90 CPU-seconds, a plausible root total
    const CHILD_CPU_MS: u64 = 1_500; // 1.5 CPU-seconds, a plausible lamb total

    let mut table = Vec::with_capacity(total as usize);
    table.push(ProcessRss {
        pid: 1,
        parent: None,
        bytes: ROOT_BYTES,
        cpu_ms: ROOT_CPU_MS,
    });

    let mut next_pid = 2u32;
    let mut frontier: VecDeque<u32> = VecDeque::from([1u32]);
    while next_pid <= total {
        let parent = frontier
            .pop_front()
            .expect("frontier cannot empty before `total` pids are placed: every pushed pid is also queued as a future parent");
        for _ in 0..BRANCHING_FACTOR {
            if next_pid > total {
                break;
            }
            table.push(ProcessRss {
                pid: next_pid,
                parent: Some(parent),
                bytes: CHILD_BYTES,
                cpu_ms: CHILD_CPU_MS,
            });
            frontier.push_back(next_pid);
            next_pid += 1;
        }
    }
    table
}

fn bench_tree_rss(c: &mut Criterion) {
    const PROCESS_COUNT: u32 = 500;
    let table = synthetic_process_tree(PROCESS_COUNT);
    c.bench_function("tree_rss/500_process_tree", |b| {
        b.iter(|| tree_rss(black_box(&table), black_box(1)));
    });
}

fn bench_sysinfo_sample(c: &mut Criterion) {
    let sampler = SysinfoSampler::new();
    c.bench_function("sysinfo_sampler/sample_real_machine", |b| {
        b.iter(|| black_box(sampler.sample()));
    });
}

criterion_group!(benches, bench_tree_rss, bench_sysinfo_sample);
criterion_main!(benches);
