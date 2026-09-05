//! Benches the two costs `MEMORY_POLL_INTERVAL`
//! (`shep-daemon/src/limits/mod.rs`) budgets against.
//!
//! `tree_rss` sums a deterministic synthetic tree (see
//! [`synthetic_process_tree`]). Cost scales with the input table, not
//! the flock subtree summed: it indexes every entry first. In
//! production, that table is the host's process list.
//!
//! `SysinfoSampler::sample` walks the real `/proc` (or platform
//! equivalent). Cost scales with host process count, so it is not
//! deterministic: recorded, not asserted. CI runs with `-- --test`,
//! executing each benchmark once and asserting nothing.

use std::collections::VecDeque;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use shep_daemon::limits::sample::{MemorySampler, ProcessRss, SysinfoSampler, tree_rss};

// Direct children per process before the next node in insertion order
// starts its own. Approximates a real supervised tree, between a flat
// generation and a single chain.
const BRANCHING_FACTOR: u32 = 4;

// Builds a deterministic `total`-process tree rooted at pid 1,
// [`BRANCHING_FACTOR`]-wide breadth first.
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
