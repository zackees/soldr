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

use soldr_cache::save::{load, save, LoadOptions, SaveOptions, DEFAULT_ZSTD_LEVEL};

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
        cache_dir: &cache,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
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
        cache_dir: &cache_restored,
        workspace: Some(&ws),
        threads: None,
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
