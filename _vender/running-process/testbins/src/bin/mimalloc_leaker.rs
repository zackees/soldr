//! Allocates at a known frame, then writes a pprof heap profile (#792).
//!
//! Usage: `testbin-mimalloc-leaker <dump-path>`
//!
//! Prints one status line to stdout so the test can tell the outcomes apart
//! without inferring them from a missing file:
//!
//! - `DUMPED <path>`     — a pprof profile was written and is ready to decode.
//! - `PROFILING_OFF`     — the profiler refused to start.
//! - `NO_PROFILER <why>` — the dump call failed.
//!
//! Replaces the previous fixture. Two things changed with the allocator:
//!
//! 1. **No platform gate.** mimalloc-pprof is a Windows-first mimalloc fork,
//!    so it builds and profiles everywhere. The fixture this replaced was
//!    linux-gnu only, because its allocator had no MSVC build and no usable
//!    macOS profiler.
//! 2. **The output is already pprof.** The previous allocator emitted a
//!    bespoke text format the daemon had to parse and lower. `dump_proto_file`
//!    writes the protobuf directly, so no text format is in the path at all.

#[global_allocator]
static ALLOC: mimalloc_pprof::MiMalloc = mimalloc_pprof::MiMalloc;

/// How much to allocate and hold, in 4 KiB blocks.
///
/// Large enough to dominate incidental allocation in a fixture this small, so
/// the test can assert this frame is the top of the profile.
const BLOCKS: usize = 2000;

/// Sample every Nth byte of allocation.
///
/// This profiler is statistical, unlike the previous allocator's every-single-
/// allocation mode. A small rate keeps the leaking frame
/// unambiguous without making the test assert exact byte totals, which no
/// sampling profiler can promise.
const SAMPLE_RATE: usize = 4096;

/// Fixed so repeated runs sample the same way. A flaky profile would make a
/// dominance assertion intermittently wrong for reasons unrelated to the code
/// under test.
const SAMPLE_SEED: u64 = 0x5eed_1234;

/// The frame the test looks for. Never inlined: the point is that this name
/// anchors the dominant stack.
#[inline(never)]
fn leak_here(blocks: usize) -> Vec<Box<[u8; 4096]>> {
    let mut held = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        held.push(Box::new([7u8; 4096]));
    }
    held
}

fn main() {
    let path = match std::env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: testbin-mimalloc-leaker <dump-path>");
            std::process::exit(2);
        }
    };

    if !mimalloc_pprof::prof::start_seeded(SAMPLE_RATE, SAMPLE_SEED) {
        println!("PROFILING_OFF");
        return;
    }

    // Held across the dump: the profiler reports what is live, so releasing
    // this first would produce a profile that correctly shows nothing leaking.
    let held = leak_here(BLOCKS);

    match mimalloc_pprof::prof::dump_proto_file(std::path::Path::new(&path)) {
        Ok(()) => println!("DUMPED {path}"),
        Err(e) => println!("NO_PROFILER dump_proto_file failed: {e}"),
    }

    std::mem::drop(held);
    mimalloc_pprof::prof::stop();
}
