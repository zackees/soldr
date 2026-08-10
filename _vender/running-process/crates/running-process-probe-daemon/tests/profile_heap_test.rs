//! A real heap profile, produced by a real allocator (#792).
//!
//! # Why this test no longer parses anything
//!
//! The previous allocator emitted its own bespoke text format, so the daemon
//! carried a parser and a pprof lowering stage, and this test existed largely
//! to prove that parser could read what the allocator wrote. `mimalloc-pprof`
//! writes pprof protobuf directly, so both stages are gone: the fixture's
//! output *is* the wire format, and this test decodes it with `prost` the same
//! way any pprof consumer would.
//!
//! # Why the assertions are about dominance, not byte totals
//!
//! The old fixture could ask its allocator to sample every single allocation,
//! which made an exact-bytes assertion sound. This profiler is statistical by
//! design. Asserting a precise live-byte total against a sampler would be
//! flaky for reasons that have nothing to do with the code under test, so the
//! fixture seeds the sampler and the assertions are relative: the leaking call
//! site should carry far more sampled bytes than anything else.

use std::path::PathBuf;
use std::process::Command;

use prost::Message as _;
use running_process_probe_daemon::profile::pprof::Profile;

/// Locate a fixture binary built by `soldr cargo build -p testbins`.
fn testbin_path(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.\n\
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

/// Run the fixture and return the pprof profile it wrote.
///
/// `label` keys the dump path: these tests run as threads of one binary, so a
/// shared path would have them reading each other's half-written files.
fn capture_profile(label: &str) -> Option<Profile> {
    let fixture = testbin_path("testbin-mimalloc-leaker");
    let dir = std::env::temp_dir().join(format!("rp-heap-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dump = dir.join("heap.pb");

    let output = Command::new(&fixture)
        .arg(&dump)
        .output()
        .expect("run mimalloc fixture");

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(reason) = stdout.strip_prefix("NO_PROFILER ") {
        eprintln!("skipping: {reason}");
        return None;
    }
    if stdout == "PROFILING_OFF" {
        eprintln!("skipping: the allocator refused to start its profiler here");
        return None;
    }
    assert!(
        stdout.starts_with("DUMPED "),
        "fixture did not report a dump: stdout={stdout:?} stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let bytes =
        std::fs::read(&dump).unwrap_or_else(|e| panic!("read profile at {}: {e}", dump.display()));
    Some(Profile::decode(bytes.as_slice()).expect("fixture output must be a pprof profile"))
}

/// Every module named by a sample's stack.
///
/// Modules, not function names: this profile is deliberately unsymbolized —
/// it carries `location` and `mapping` tables but no `function` table at all,
/// because symbolization happens later and off-process. Asking it for
/// `leak_here` by name would be asking for something the format does not
/// contain here, so a stack is identified by the binaries it runs through.
fn stack_modules(profile: &Profile, location_ids: &[u64]) -> Vec<String> {
    let mut modules = Vec::new();
    for location_id in location_ids {
        let Some(location) = profile.location.iter().find(|l| l.id == *location_id) else {
            continue;
        };
        if let Some(mapping) = profile.mapping.iter().find(|m| m.id == location.mapping_id) {
            modules.push(profile.string_table[mapping.filename as usize].clone());
        }
    }
    modules
}

#[test]
fn the_fixture_emits_a_decodable_pprof_profile() {
    let Some(profile) = capture_profile("decodes") else {
        return;
    };

    assert!(
        !profile.sample.is_empty(),
        "a profile with no samples reads as `this program allocates nothing`"
    );
    assert!(
        !profile.sample_type.is_empty(),
        "pprof requires at least one sample type"
    );
    // The spec invariant, and 0 is also how every optional string field says
    // "unset".
    assert_eq!(profile.string_table[0], "");
}

#[test]
fn the_leaking_call_site_dominates_the_profile() {
    let Some(profile) = capture_profile("dominates") else {
        return;
    };

    // `inuse_space` — what is still held. The four types are
    // alloc_objects / alloc_space / inuse_objects / inuse_space, and a leak
    // hunt cares about the last.
    let live = profile
        .sample_type
        .iter()
        .position(|t| profile.string_table[t.r#type as usize] == "inuse_space")
        .expect("a heap profile should report inuse_space");

    let total: i64 = profile
        .sample
        .iter()
        .map(|s| s.value.get(live).copied().unwrap_or(0))
        .sum();
    assert!(total > 0, "the profile carries no live bytes to compare");

    let heaviest = profile
        .sample
        .iter()
        .max_by_key(|s| s.value.get(live).copied().unwrap_or(0))
        .expect("at least one sample");
    let heaviest_bytes = heaviest.value.get(live).copied().unwrap_or(0);

    // Dominance rather than an exact figure: the sampler is statistical, so
    // asserting a precise byte total would be flaky by construction. The
    // fixture holds ~8 MiB at one site against a few KiB of incidental
    // runtime allocation, so the margin is wide.
    assert!(
        heaviest_bytes * 2 > total,
        "the heaviest stack holds {heaviest_bytes} of {total} live bytes — the \
         fixture's own allocation should be the clear majority"
    );

    // And that dominant allocation should be attributed to the fixture, not
    // to a system library it happened to call through.
    let modules = stack_modules(&profile, &heaviest.location_id);
    assert!(
        modules
            .iter()
            .any(|m| m.contains("testbin-mimalloc-leaker")),
        "the heaviest stack does not run through the fixture binary: {modules:?}"
    );
}
