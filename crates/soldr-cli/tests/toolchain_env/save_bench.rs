//! Speed sanity check. Not a real criterion benchmark — just an
//! ignored test you opt into with `--ignored` that synthesizes a
//! workspace + cache of realistic size and asserts the round trip
//! completes within a generous wall-clock budget. The point is to
//! catch obvious regressions (a 10x slowdown from accidentally
//! disabling rayon, single-threaded zstd, etc.), not to publish
//! benchmark numbers.

use std::fs;
use std::path::Path;
use std::time::Instant;

use soldr_cli::cache_lib::save::{
    load, save, LoadOptions, SaveOptions, SaveProfile, DEFAULT_ZSTD_LEVEL,
};

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Build a workspace with `n_files` source files of `bytes_per_file`
/// pseudo-random bytes. Deterministic content (seeded by index) so
/// runs are reproducible.
fn synth_workspace(root: &Path, n_files: usize, bytes_per_file: usize) {
    for i in 0..n_files {
        let mut content = vec![0u8; bytes_per_file];
        let mut seed = (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
        for byte in &mut content {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (seed >> 33) as u8;
        }
        let depth = i % 5;
        let mut path = root.to_path_buf();
        for d in 0..depth {
            path = path.join(format!("d{d}"));
        }
        path = path.join(format!("file_{i:04}.rs"));
        write(&path, &content);
    }
    // Required boilerplate so walks pick it up as a "real" workspace.
    write(
        &root.join("Cargo.toml"),
        b"[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    );
    write(&root.join("Cargo.lock"), b"# lock\n");
}

fn synth_cache(root: &Path, total_mb: usize) {
    // 1 MB per file is realistic for compile-cache buckets. Each
    // chunk's content is pseudo-random (LCG seeded by file index) so
    // zstd can't crush it to nothing — the archive size will reflect
    // what a real artifact cache produces.
    let n = total_mb;
    let mut chunk = vec![0u8; 1024 * 1024];
    for i in 0..n {
        let mut seed = (i as u64).wrapping_add(0xCAFE_BABE_DEAD_BEEF);
        for byte in &mut chunk {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (seed >> 33) as u8;
        }
        let a = i % 256;
        let b = (i / 256) % 256;
        write(&root.join(format!("{a:02x}/{b:02x}/blob.bin")), &chunk);
    }
}

/// #1541 benchmark — NOT run by default (`--ignored` opt-in). Builds
/// two synthetic bundles ("mixed": 10K x 4KiB + 3 x 64MiB; "small":
/// 30K x 1KiB), then times save + load over 3 reps each, printing
/// wall-clock plus /proc/self/io logical read/write byte deltas
/// (Linux; zeros elsewhere). Used to quantify the duplicate-I/O
/// eliminations in save/load; asserts nothing beyond round-trip
/// success so it can never flake CI.
#[test]
#[ignore = "perf measurement; opt in with --ignored --nocapture"]
fn bench_save_load_io() {
    fn proc_io() -> (u64, u64) {
        // (rchar, wchar); zeros when unavailable (non-Linux).
        let Ok(text) = fs::read_to_string("/proc/self/io") else {
            return (0, 0);
        };
        let grab = |key: &str| -> u64 {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0)
        };
        (grab("rchar:"), grab("wchar:"))
    }

    let dir = tempfile::tempdir().unwrap();

    // Phase A ("mixed"): 10_000 x 4 KiB + 3 x 64 MiB — realistic bundle
    // shape where compression cost shares the stage with per-file work.
    let cache = dir.path().join("cache");
    let mut lcg: u64 = 0x1541_1541_1541_1541;
    let mut next = || {
        lcg = lcg
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        lcg
    };
    for bucket in 0..100u32 {
        for f in 0..100u32 {
            let mut body = Vec::with_capacity(4096);
            while body.len() < 4096 {
                body.extend_from_slice(&next().to_le_bytes());
                body.extend_from_slice(b"soldr-bench-padding-block-......");
            }
            body.truncate(4096);
            write(&cache.join(format!("b{bucket:03}/f{f:03}.bin")), &body);
        }
    }
    // 3 large files: 64 MiB each, low-compressibility.
    for l in 0..3u32 {
        let mut body = Vec::with_capacity(64 * 1024 * 1024);
        while body.len() < 64 * 1024 * 1024 {
            body.extend_from_slice(&next().to_le_bytes());
        }
        write(&cache.join(format!("large/blob{l}.bin")), &body);
    }

    // Phase B ("small"): 30_000 x 1 KiB compressible files — isolates the
    // per-file syscall overhead (stat/utimensat/mkdir) that the serial
    // post-load mtime pass and the extract workers pay per entry.
    let small_cache = dir.path().join("cache-small");
    for bucket in 0..150u32 {
        for f in 0..200u32 {
            let body = format!(
                "soldr-bench small payload bucket={bucket} file={f} {}",
                "x".repeat(960)
            );
            write(
                &small_cache.join(format!("s{bucket:03}/f{f:03}.d")),
                body.as_bytes(),
            );
        }
    }

    for (phase, cache) in [("mixed", &cache), ("small", &small_cache)] {
        for rep in 0..3u32 {
            let archive = dir.path().join(format!("bench-{phase}-{rep}.tar.zst"));
            let (r0, w0) = proc_io();
            let t0 = Instant::now();
            let sreport = save(&SaveOptions {
                workspace: None,
                cache_dir: Some(cache),
                out: &archive,
                zstd_level: DEFAULT_ZSTD_LEVEL,
                threads: None,
                mtimes_only: false,
                profile: SaveProfile::Full,
            })
            .expect("bench save ok");
            let save_ms = t0.elapsed().as_millis();
            let (r1, w1) = proc_io();

            let restore = dir.path().join(format!("restore-{phase}-{rep}"));
            let t1 = Instant::now();
            let lreport = load(&LoadOptions {
                archive: &archive,
                cache_dir: Some(&restore),
                workspace: None,
                threads: None,
                mtimes_only: false,
                profile_extract: false,
                auto_defender_exclude: false,
            })
            .expect("bench load ok");
            let load_ms = t1.elapsed().as_millis();
            let (r2, w2) = proc_io();

            println!(
                "BENCH phase={phase} rep={rep} save_ms={save_ms} save_rchar={} save_wchar={} load_ms={load_ms} load_rchar={} load_wchar={} cache_files={} restored={} archive_bytes={}",
                r1 - r0,
                w1 - w0,
                r2 - r1,
                w2 - w1,
                sreport.cache_files,
                lreport.cache_files_restored,
                sreport.archive_bytes,
            );
            fs::remove_dir_all(&restore).unwrap();
            fs::remove_file(&archive).unwrap();
        }
    }
}

#[test]
#[ignore = "perf sanity; opt in with --ignored"]
fn perf_roundtrip_realistic() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    let cache = dir.path().join("cache");
    let cache_restored = dir.path().join("cache_restored");
    let archive = dir.path().join("snap.tar.zst");

    let t0 = Instant::now();
    synth_workspace(&ws, 1000, 4 * 1024);
    synth_cache(&cache, 100);
    eprintln!("synth fixtures: {:?}", t0.elapsed());

    let t0 = Instant::now();
    let sreport = save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");
    let save_elapsed = t0.elapsed();
    eprintln!(
        "save: source_files={} cache_files={} archive={:.1} MB elapsed={:?}",
        sreport.source_files,
        sreport.cache_files,
        sreport.archive_bytes as f64 / 1_048_576.0,
        save_elapsed
    );

    let t0 = Instant::now();
    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache_restored),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");
    let load_elapsed = t0.elapsed();
    eprintln!(
        "load: cache_files_restored={} mtimes_applied={} elapsed={:?}",
        lreport.cache_files_restored, lreport.mtimes_applied, load_elapsed
    );

    // Sanity: every source file's mtime should restore because we
    // didn't touch the workspace between save and load.
    assert_eq!(lreport.mtimes_applied, sreport.source_files);

    // Generous budget. On a typical CI runner this whole loop should
    // finish well under 10 s; we assert under 30 s to allow for slow
    // shared CI workers.
    let total = save_elapsed + load_elapsed;
    assert!(
        total.as_secs() < 30,
        "save+load took {:?}, suspiciously slow",
        total
    );
}
