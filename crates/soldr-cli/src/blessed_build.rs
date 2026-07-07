//! soldr#1012 PR 5 — blessed cross-compile sysroot preparation for
//! `soldr build --target <T>`.
//!
//! When `Commands::Build` (soldr#1013) detects a canonical cross-
//! compile target, this module materializes the per-target sysroot
//! from the soldr-toolchain catalogue, sets the env vars cargo + cc-rs
//! + linker tools need, and installs the clang shim ahead of the
//! system clang on PATH.
//!
//! ## Target coverage
//!
//! * `*-pc-windows-msvc` — xwin-cache + clang shim (sidesteps the
//!   ring 0.17.14:563 oversight described in [`crate::fetch::xwin_cache`]
//!   and the `soldr-clang-shim` binary doc)
//! * `*-apple-darwin` — soldr's existing apple-sdk fetcher already
//!   handles this via `soldr prepare`; the blessed `soldr build` path
//!   just calls into [`crate::fetch::apple_sdk::ensure_apple_sdk`]
//!
//! Other targets (linux musl, linux gnu) get no sysroot prep — they
//! work out-of-the-box with the host cargo + zigbuild flow.
//!
//! ## Opt-out
//!
//! `SOLDR_USE_LEGACY_XWIN=1` (or `SOLDR_USE_LEGACY_ZIGBUILD=1`)
//! suppresses the blessed prep for that toolchain family and falls
//! through to the cargo-xwin / cargo-zigbuild path. Mirrors the
//! `SOLDR_USE_LEGACY_*` env-var contract documented in soldr#1010
//! Phase 8.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Env var that opts out of the blessed xwin-cache materialization
/// and falls through to cargo-xwin's own live download for win-msvc
/// targets. Set to any non-empty value to trigger.
pub const USE_LEGACY_XWIN_ENV_VAR: &str = "SOLDR_USE_LEGACY_XWIN";

/// Env var that opts out of the blessed zigbuild prep flow. Reserved
/// for future #1012 work; today's zigbuild path is unchanged.
pub const USE_LEGACY_ZIGBUILD_ENV_VAR: &str = "SOLDR_USE_LEGACY_ZIGBUILD";

/// What the blessed-build prep accomplished, returned to the caller
/// so `Commands::Build` can log + set the resulting env vars on the
/// child cargo invocation.
#[derive(Debug, Clone, Default)]
pub struct BlessedPrep {
    /// Path to `XWIN_CACHE_DIR` value (for `*-pc-windows-msvc`).
    pub xwin_cache_dir: Option<PathBuf>,
    /// Path to `SDKROOT` value (for `*-apple-darwin`).
    pub sdkroot: Option<PathBuf>,
    /// Directory to prepend to `PATH` so the soldr-clang-shim wins
    /// when cc-rs (or ring) does a bare `which("clang")` lookup.
    pub shim_path_dir: Option<PathBuf>,
    /// Additional directories to prepend to `PATH` on the child cargo
    /// invocation (after `shim_path_dir`). Used for the managed
    /// cmake/ninja `bin/` dirs so CMake's Ninja generator resolves the
    /// managed ninja instead of whatever the system PATH serves.
    pub path_dirs: Vec<PathBuf>,
    /// Per-target env-var tuples to set on the child cargo
    /// invocation: `[(NAME, VALUE), ...]`. Includes `CC_<t>`,
    /// `CXX_<t>`, `AR_<t>`, `CARGO_TARGET_<T>_LINKER` for MSVC
    /// targets.
    pub env: Vec<(String, String)>,
    /// Extra cargo CLI args to inject into the `soldr build` cargo
    /// invocation. Used for target-scoped Cargo config that has no
    /// environment-variable equivalent, such as build-script overrides
    /// for crates with `links = "..."`
    pub cargo_args: Vec<String>,
}

/// Prepare the blessed sysroot for `target`. Returns `Ok(prep)` on
/// success; `Ok(BlessedPrep::default())` if `target` doesn't need
/// any prep (or if the caller opted into legacy via env vars);
/// `Err(_)` only on actual fetch / extract failures.
///
/// Caller is responsible for applying `prep.env` and prepending
/// `prep.shim_path_dir` to `PATH` on the child cargo invocation.
pub async fn prepare(paths: &SoldrPaths, target_triple: &str) -> Result<BlessedPrep, SoldrError> {
    let mut prep = BlessedPrep::default();

    // ----------------------------- Windows MSVC ------------------------------
    if target_triple.ends_with("-pc-windows-msvc") && !legacy_xwin_opt_out() {
        // Install the clang shim + set cc-rs env vars FIRST,
        // independent of xwin-cache state. The shim is what actually
        // fixes ring's hardcoded compiler override (build.rs:563);
        // xwin-cache is only an optimization that lets cargo-xwin
        // short-circuit its live MSVC download. Even when the
        // catalogue row for a target arch isn't yet ingested (arm64
        // today, until soldr-toolchain PR #30's recipe gets dispatched
        // + ingested), the shim alone makes ring compile correctly —
        // cargo-xwin's live download still works for the SDK itself.
        let target_u = target_triple.replace('-', "_");
        let target_u_upper = target_u.to_uppercase();

        let shim_dir = install_clang_shim(paths)?;
        prep.shim_path_dir = Some(shim_dir);

        // Put the managed LLVM binutils (clang-cl / lld-link / llvm-lib,
        // from zackees/clang-tool-chain-bins) on PATH so the AR /
        // linker picks below resolve without a host `apt install llvm`.
        let _ = ensure_llvm_on_path(paths, &mut prep, target_triple).await;

        prep.env
            .push((format!("CC_{target_u}"), "clang".to_string()));
        prep.env
            .push((format!("CXX_{target_u}"), "clang".to_string()));
        prep.env
            .push((format!("AR_{target_u}"), "llvm-lib".to_string()));
        prep.env.push((
            format!("CARGO_TARGET_{target_u_upper}_LINKER"),
            "lld-link".to_string(),
        ));

        // Now try to materialize xwin-cache too. If the catalogue row
        // for this arch is ingested, we set XWIN_CACHE_DIR so cargo-
        // xwin transparently uses our cache instead of triggering a
        // fresh live download, AND we inject the `/imsvc` include flags
        // + `/LIBPATH:` linker args via `CFLAGS_<t>` / `CXXFLAGS_<t>` /
        // `CARGO_TARGET_<T>_RUSTFLAGS` so the blessed path works
        // natively without cargo-xwin in the loop (soldr#1036 — make
        // soldr build a true cargo-xwin replacement, not just a wrapper
        // that delegates to it). If the catalogue row isn't ingested,
        // fall through — the shim alone is enough for the win-arm64
        // ring fix; cargo-xwin's live download still produces a working
        // SDK.
        match crate::fetch::xwin_cache::ensure_xwin_cache(paths, target_triple).await {
            Ok(cache_dir) => {
                prep.xwin_cache_dir = Some(cache_dir.clone());
                prep.env.push((
                    crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR.to_string(),
                    cache_dir.to_string_lossy().into_owned(),
                ));

                // Native cflags + rustflags injection (soldr#1036).
                // cc-rs reads `CFLAGS_<target>` / `CXXFLAGS_<target>`
                // and forwards them to the compiler invocation. The
                // soldr-clang-shim routes `clang` → `clang-cl` for
                // *-pc-windows-msvc, and clang-cl natively accepts
                // `/imsvc <path>` MSVC-style include flags.
                let cflags = xwin_msvc_cflags(&cache_dir);
                if !cflags.is_empty() {
                    prep.env
                        .push((format!("CFLAGS_{target_u}"), cflags.clone()));
                    prep.env.push((format!("CXXFLAGS_{target_u}"), cflags));
                }

                // Linker args via cargo's per-target RUSTFLAGS env var.
                // Each /LIBPATH: entry becomes a `-C link-arg=...` so
                // rustc passes it through to lld-link. Whitespace-
                // separated rustflags is cargo's documented contract.
                let link_args = xwin_msvc_link_args(&cache_dir, target_triple);
                if !link_args.is_empty() {
                    prep.env.push((
                        format!("CARGO_TARGET_{target_u_upper}_RUSTFLAGS"),
                        link_args,
                    ));
                }
            }
            Err(e) => {
                eprintln!("soldr build: catalogue xwin-cache unavailable for {target_triple}: {e}");
                eprintln!(
                    "soldr build: continuing without XWIN_CACHE_DIR — \
                     cargo-xwin's live download will produce the SDK \
                     (clang shim is still active for ring's compile fix)"
                );
            }
        }
    }

    // ------------------------------ Apple Darwin -----------------------------
    if target_triple.ends_with("-apple-darwin") && !legacy_zigbuild_opt_out() {
        // soldr is a bootstrapping tool: it must provision the cross
        // toolchain itself, never lean on a host `apt install clang lld
        // llvm`. The darwin arm sets AR=llvm-ar / RANLIB=llvm-ranlib /
        // CC=clang / linker=clang -fuse-ld=lld — put the managed LLVM
        // binutils (from zackees/clang-tool-chain-bins) on PATH so those
        // resolve on a bare Linux runner. (msvc gets LLVM via its
        // downstream `cargo xwin` subcommand; darwin dispatches to plain
        // cargo, so without this it never gets llvm-ar — soldr#1309.)
        let managed_llvm_available = ensure_llvm_on_path(paths, &mut prep, target_triple).await;

        // Apple SDK fetch is the same code path `soldr prepare` uses,
        // so this is reuse rather than new logic.
        match crate::fetch::apple_sdk::ensure_apple_sdk(paths).await {
            Ok(sdk) => {
                let sdk_str = sdk.to_string_lossy().into_owned();
                prep.sdkroot = Some(sdk.clone());
                prep.env.push(("SDKROOT".to_string(), sdk_str.clone()));

                // Surface the COMPLETE Apple SDK to every C/C++ build
                // script in the dep tree by overriding cc-rs's per-
                // target compiler picks. Without this, anything that
                // probes platform headers hits zig's minimal bundled
                // darwin sysroot instead of the real SDK and fails the
                // cross-compile. With it, every C/C++ dep — ring,
                // zstd-sys, libsqlite3-sys, libz-ng-sys, bzip2-sys,
                // lzma-sys, libmimalloc-sys — sees the same headers a
                // native macOS build would. Pattern mirrors the
                // Windows MSVC arm above (lines 84-110).
                //
                // Linker: use clang as the driver with `-fuse-ld=lld`.
                // lld knows Mach-O; clang knows how to pass the right
                // -L / -F flags to satisfy darwin framework references
                // (Security, CoreFoundation, etc.) from the SDK's
                // System/Library/Frameworks/.
                let target_u = target_triple.replace('-', "_");
                let target_u_upper = target_u.to_uppercase();
                let clang_arch_target = if target_triple.starts_with("aarch64") {
                    "arm64-apple-darwin"
                } else {
                    "x86_64-apple-darwin"
                };
                let clang_base =
                    format!("clang --target={clang_arch_target} -isysroot {sdk_str} -mmacosx-version-min=11.0");
                let clangxx_base = format!(
                    "clang++ --target={clang_arch_target} -isysroot {sdk_str} -mmacosx-version-min=11.0 -stdlib=libc++"
                );
                // `-fuse-ld=lld` is critical for the C/C++ flags too:
                // cmake-based -sys crates (libz-ng-sys, etc.) test the
                // compiler with a tiny .c + link round-trip. Without it
                // they invoke clang → /usr/bin/ld which is the host's
                // Linux ld and can't produce Mach-O, so the cmake
                // configure step panics with "linker command failed".
                // Adding it to CFLAGS/CXXFLAGS routes the test link
                // through lld which knows Mach-O. The `--target=` flag
                // is also needed in CFLAGS so the compiler test
                // actually targets darwin (not the linux host).
                let lld_flag = if managed_llvm_available {
                    " -fuse-ld=lld"
                } else {
                    ""
                };
                let cflags = format!(
                    "--target={clang_arch_target} -isysroot {sdk_str} \
                     -mmacosx-version-min=11.0{lld_flag}"
                );
                let cxxflags = format!(
                    "--target={clang_arch_target} -isysroot {sdk_str} \
                     -mmacosx-version-min=11.0 -stdlib=libc++{lld_flag}"
                );
                let mut rustflags = format!(
                    "-C link-arg=--target={clang_arch_target} \
                     -C link-arg=-isysroot \
                     -C link-arg={sdk_str} \
                     -C link-arg=-mmacosx-version-min=11.0"
                );
                if managed_llvm_available {
                    rustflags.push_str(" -C link-arg=-fuse-ld=lld");
                }
                let (ar_tool, ranlib_tool) = if managed_llvm_available {
                    ("llvm-ar", "llvm-ranlib")
                } else {
                    ("ar", "ranlib")
                };
                prep.env
                    .push((format!("CC_{target_u}"), clang_base.clone()));
                prep.env.push((format!("CXX_{target_u}"), clangxx_base));
                prep.env
                    .push((format!("AR_{target_u}"), ar_tool.to_string()));
                prep.env
                    .push((format!("RANLIB_{target_u}"), ranlib_tool.to_string()));
                prep.env.push((format!("CFLAGS_{target_u}"), cflags));
                prep.env.push((format!("CXXFLAGS_{target_u}"), cxxflags));
                prep.env.push((
                    format!("CARGO_TARGET_{target_u_upper}_LINKER"),
                    "clang".to_string(),
                ));
                prep.env.push((
                    format!("CARGO_TARGET_{target_u_upper}_RUSTFLAGS"),
                    rustflags,
                ));
            }
            Err(e) => {
                eprintln!("soldr build: apple SDK unavailable for {target_triple}: {e}");
                // Don't hard-fail; zigbuild may still locate an SDK
                // via cargo-zigbuild's own mechanism.
            }
        }
    }

    // -------------------------- *-sys library overrides ----------------------
    // soldr#1064 Phase B. For each registered *-sys library, try to
    // materialize the catalogue sysroot for the active target. On
    // success, push the crate-specific override env vars so the *-sys
    // build.rs skips its vendored compile. On error (not yet ingested,
    // unsupported triple, network failure), log + fall through — the
    // crate's vendored compile path still works.
    if !legacy_vendored_sys_opt_out() {
        inject_sys_library_overrides(paths, target_triple, &mut prep).await;
    }

    // ----------------------- managed cmake + ninja ---------------------------
    // Host-side tooling, independent of the compile target: cmake-based
    // *-sys build scripts (libz-ng-sys, zstd-sys, ...) must run a
    // known-good cmake with a sane generator, not whatever the system
    // PATH resolves. See inject_cmake_tooling for the full contract.
    inject_cmake_tooling(paths, &mut prep).await;

    Ok(prep)
}

/// Env var that opts out of the managed cmake/ninja injection and
/// leaves cmake resolution to the system PATH. Set to any non-empty
/// value (other than `"0"`) to trigger. Mirrors the
/// `SOLDR_USE_LEGACY_*` env-var contract.
pub const USE_SYSTEM_CMAKE_ENV_VAR: &str = "SOLDR_USE_SYSTEM_CMAKE";

fn system_cmake_opt_out() -> bool {
    std::env::var(USE_SYSTEM_CMAKE_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Inject the managed CMake + Ninja tooling env onto the prep.
///
/// What gets set (all only on success — every failure logs and falls
/// through to system cmake, stub-until-ingested style):
///
/// * `CMAKE=<bundle>/bin/cmake` — the cmake-rs crate (used by every
///   cmake-based *-sys build script) resolves the cmake binary from
///   this env var before falling back to PATH.
/// * `CMAKE_GENERATOR=Ninja` — cmake-rs forwards this as `-G`;
///   only set when the managed ninja materialized. Ninja is immune
///   to the "MSYS Makefiles + mangled MSVC flags" failure mode that
///   a stray GNU make on PATH produces, and it needs no VS generator
///   registry probing.
/// * `prep.path_dirs` gets both bundles' `bin/` dirs — CMake's Ninja
///   generator discovers `ninja` via PATH (there is no env-var
///   equivalent of the `CMAKE_MAKE_PROGRAM` cache variable).
///
/// Skips (with no log noise) when the caller already set `CMAKE` or
/// `CMAKE_GENERATOR` — explicit user env always wins — and when
/// [`USE_SYSTEM_CMAKE_ENV_VAR`] opts out.
pub async fn inject_cmake_tooling(paths: &SoldrPaths, prep: &mut BlessedPrep) {
    if system_cmake_opt_out() {
        return;
    }
    // Respect explicit user config: a pre-set CMAKE or CMAKE_GENERATOR
    // means someone already made this decision.
    for user_var in ["CMAKE", "CMAKE_GENERATOR"] {
        if std::env::var_os(user_var).is_some_and(|v| !v.is_empty()) {
            return;
        }
    }

    let host = crate::fetch::cmake_tools::current_host_triple();

    let cmake_root = match crate::fetch::cmake_tools::ensure_cmake_bundle(paths, host).await {
        Ok(root) => root,
        Err(e) => {
            log_cmake_unavailable("cmake", host, &e);
            return;
        }
    };
    let cmake_bin = crate::fetch::cmake_tools::cmake_exe(&cmake_root);
    if !cmake_bin.is_file() {
        eprintln!(
            "soldr build: managed cmake bundle at {} has no {} — \
             falling back to system cmake",
            cmake_root.display(),
            cmake_bin.display()
        );
        return;
    }

    prep.env.push((
        "CMAKE".to_string(),
        cmake_bin.to_string_lossy().into_owned(),
    ));
    prep.path_dirs.push(cmake_root.join("bin"));

    // Ninja is the generator upgrade, but the managed cmake alone is
    // already a win — don't tie the two together. Without ninja we
    // leave the generator choice to cmake-rs (Visual Studio on MSVC
    // hosts, Unix Makefiles elsewhere).
    //
    // Acquisition ladder (mirrors the maturin provisioning ladder):
    // catalogue bundle first, then the PyPI `ninja` wheel provisioned
    // into a manual uv-managed isolated env. The fallback covers
    // hosts whose ninja bundle is missing or not yet ingested (e.g.
    // musl hosts — the uv bundle exists there, ninja's doesn't).
    let ninja_bin = match crate::fetch::cmake_tools::ensure_ninja_bundle(paths, host).await {
        Ok(ninja_root) => {
            let bin = crate::fetch::cmake_tools::ninja_exe(&ninja_root);
            if bin.is_file() {
                Some(bin)
            } else {
                eprintln!(
                    "soldr build: managed ninja bundle at {} has no {} — \
                     trying the uv-provisioned ninja fallback",
                    ninja_root.display(),
                    bin.display()
                );
                ninja_via_uv_fallback(paths).await
            }
        }
        Err(e) => {
            log_cmake_unavailable("ninja", host, &e);
            ninja_via_uv_fallback(paths).await
        }
    };
    if let Some(ninja_bin) = ninja_bin {
        prep.env
            .push(("CMAKE_GENERATOR".to_string(), "Ninja".to_string()));
        if let Some(dir) = ninja_bin.parent() {
            prep.path_dirs.push(dir.to_path_buf());
        }
        // #1257 fallout guard: a target/ tree cached from BEFORE the
        // managed-Ninja switch (restored CI caches, existing dev
        // checkouts) holds cmake build dirs configured with a
        // different generator ("Unix Makefiles" / "Visual Studio").
        // cmake-rs re-runs configure in the same `-B` dir (it emits
        // rerun-if-env-changed for CMAKE_GENERATOR, so the build
        // script re-fires the moment we inject Ninja) and CMake
        // hard-errors "Does not match the generator used previously".
        // Sweep those dirs away up front — they would error anyway,
        // and a deleted build dir just means a fresh configure.
        // Observed live on the aarch64/x86_64-apple-darwin CI lanes
        // (run 28574946937).
        sweep_mismatched_cmake_build_dirs(&cargo_target_root(), "Ninja");
    }
}

/// Resolve the cargo target root the upcoming child build will use:
/// `CARGO_TARGET_DIR` when set, else `./target` under the cwd.
fn cargo_target_root() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"))
}

/// Delete every cmake-rs build dir under `target_root` whose cached
/// generator differs from `generator`. Layouts scanned (both the host
/// and the per-triple trees, any profile name):
///
/// ```text
/// target/<profile>/build/<crate-hash>/out/build/CMakeCache.txt
/// target/<triple>/<profile>/build/<crate-hash>/out/build/CMakeCache.txt
/// ```
///
/// Only the `out/build` dir is removed — cargo's fingerprint for the
/// build script is invalidated by the CMAKE_GENERATOR env change
/// anyway, so the script re-runs and configures the fresh dir.
fn sweep_mismatched_cmake_build_dirs(target_root: &std::path::Path, generator: &str) {
    let needle = format!("CMAKE_GENERATOR:INTERNAL={generator}");
    let mut roots: Vec<PathBuf> = Vec::new();
    let Ok(level1) = std::fs::read_dir(target_root) else {
        return;
    };
    for entry in level1.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        // `p` is either a profile dir (has build/) or a triple dir
        // (whose children are profile dirs).
        roots.push(p.clone());
        if let Ok(level2) = std::fs::read_dir(&p) {
            roots.extend(
                level2
                    .flatten()
                    .map(|e| e.path())
                    .filter(|c| c.is_dir() && c.join("build").is_dir()),
            );
        }
    }

    for profile_dir in roots {
        let build_dir = profile_dir.join("build");
        let Ok(crates) = std::fs::read_dir(&build_dir) else {
            continue;
        };
        for crate_dir in crates.flatten() {
            let out_build = crate_dir.path().join("out").join("build");
            let cache = out_build.join("CMakeCache.txt");
            let Ok(contents) = std::fs::read_to_string(&cache) else {
                continue;
            };
            let mismatch = contents
                .lines()
                .any(|l| l.starts_with("CMAKE_GENERATOR:INTERNAL=") && l.trim() != needle);
            if mismatch {
                eprintln!(
                    "soldr build: sweeping stale cmake build dir (generator != {generator}): {}",
                    out_build.display()
                );
                let _ = std::fs::remove_dir_all(&out_build);
            }
        }
    }
}

/// PyPI-ninja-via-uv fallback rung. Returns the ninja exe on success;
/// logs + returns `None` on failure (the caller keeps cmake's default
/// generator — never fatal).
async fn ninja_via_uv_fallback(paths: &SoldrPaths) -> Option<PathBuf> {
    match crate::fetch::uv_env::provision_ninja_via_uv(paths).await {
        Ok(ninja) => Some(ninja),
        Err(e) => {
            eprintln!("soldr build: uv-provisioned ninja fallback unavailable: {e}");
            eprintln!("soldr build: keeping cmake's default generator");
            None
        }
    }
}

fn log_cmake_unavailable(tool: &str, host: &str, err: &SoldrError) {
    eprintln!("soldr build: managed {tool} unavailable for host {host}: {err}");
    eprintln!("soldr build: continuing with system {tool} from PATH");
}

/// Env var that opts out of the entire `*-sys` catalogue-substitution
/// path and falls through to each crate's vendored compile. Set to any
/// non-empty value (other than `"0"`) to trigger. Mirrors the
/// `SOLDR_USE_LEGACY_{XWIN,ZIGBUILD}` env-var contract.
pub const USE_LEGACY_VENDORED_SYS_ENV_VAR: &str = "SOLDR_USE_LEGACY_VENDORED_SYS";

fn legacy_vendored_sys_opt_out() -> bool {
    std::env::var(USE_LEGACY_VENDORED_SYS_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Per-*-sys-crate catalogue substitution. Each block follows the same
/// shape: call the consumer module's `ensure_<lib>_sysroot`, and on
/// success push the override env vars. Failures are logged but never
/// fatal — the *-sys crate's vendored compile remains the safety net.
async fn inject_sys_library_overrides(
    paths: &SoldrPaths,
    target_triple: &str,
    prep: &mut BlessedPrep,
) {
    // All syslib env vars below are **target-scoped** via the
    // `PKG_CONFIG_PATH_<triple>` naming convention pkg-config-rs
    // honors. Unscoped `PKG_CONFIG_PATH` would leak into host
    // build-script compilations (cargo runs build scripts with the
    // same env it spawns cargo with), causing the host-side
    // build_script_build link step for soldr-cli to try linking
    // against the cross-target sysroot — e.g. `rust-lld: unable to
    // find library -lzstd_static` when the catalogue path points
    // at a Windows-format zstd dir but the host link runs on Linux.
    //
    // We also no longer set `ZSTD_SYS_USE_PKG_CONFIG=1` /
    // `LIBSQLITE3_SYS_USE_PKG_CONFIG=1` / `LZMA_API_STATIC=1`
    // unconditionally — those flags are NOT target-scoped, so
    // setting them globally forces every consumer (including the
    // host build-script side of the same crate) to attempt
    // pkg-config probing, which then matches against the leaked
    // unscoped `PKG_CONFIG_PATH` and re-introduces the same link
    // bug. The catalogue sysroot is still surfaced via the
    // target-scoped `PKG_CONFIG_PATH_<triple>`; -sys crates that
    // natively probe pkg-config-rs will find it for the cross
    // target without us having to flip a global feature flag.

    // zstd-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::zstd_sysroot::ensure_zstd_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("zstd", target_triple, &e),
    }

    // libsqlite3-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::sqlite_sysroot::ensure_sqlite_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("sqlite", target_triple, &e),
    }

    // libmimalloc-sys has `links = "mimalloc"` but no target-scoped
    // env hook. Use Cargo's build-script override config instead;
    // that skips libmimalloc-sys/build.rs entirely and supplies the
    // link metadata directly.
    match crate::fetch::mimalloc_sysroot::ensure_mimalloc_sysroot(paths, target_triple).await {
        Ok(sysroot) => add_mimalloc_build_script_override(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("mimalloc", target_triple, &e),
    }

    // libz-ng-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::zlib_ng_sysroot::ensure_zlib_ng_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("zlib-ng", target_triple, &e),
    }

    // lzma-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::lzma_sysroot::ensure_lzma_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("lzma", target_triple, &e),
    }

    // bzip2-sys → PKG_CONFIG_PATH_<triple>
    match crate::fetch::bzip2_sysroot::ensure_bzip2_sysroot(paths, target_triple).await {
        Ok(sysroot) => prepend_pkg_config_path_for_target(prep, target_triple, &sysroot),
        Err(e) => log_sys_unavailable("bzip2", target_triple, &e),
    }
}

/// Prepend `<sysroot>/lib/pkgconfig` to the **target-scoped**
/// `PKG_CONFIG_PATH_<triple>` env var (pkg-config-rs convention).
/// Unscoped `PKG_CONFIG_PATH` is deliberately NOT used — see the
/// rationale in `inject_sys_library_overrides`.
fn prepend_pkg_config_path_for_target(
    prep: &mut BlessedPrep,
    target_triple: &str,
    sysroot: &std::path::Path,
) {
    let var_name = format!("PKG_CONFIG_PATH_{target_triple}");
    let pkgconfig_dir = sysroot.join("lib").join("pkgconfig");
    let new_entry = pkgconfig_dir.to_string_lossy().into_owned();
    if let Some(existing) = prep.env.iter_mut().find(|(name, _)| name == &var_name) {
        let sep = if cfg!(windows) { ';' } else { ':' };
        existing.1 = format!("{new_entry}{sep}{}", existing.1);
    } else {
        prep.env.push((var_name, new_entry));
    }
}

fn add_mimalloc_build_script_override(
    prep: &mut BlessedPrep,
    target_triple: &str,
    sysroot: &std::path::Path,
) {
    let table = format!("target.{target_triple}.mimalloc");
    let lib_dir = sysroot.join("lib");
    let include_dir = sysroot.join("include");
    prep.cargo_args.extend([
        "--config".to_string(),
        format!("{table}.rustc-link-lib=[\"static=mimalloc\"]"),
        "--config".to_string(),
        format!(
            "{table}.rustc-link-search=[{}]",
            toml_string(&lib_dir.to_string_lossy())
        ),
        "--config".to_string(),
        format!(
            "{table}.metadata_include_dir={}",
            toml_string(&include_dir.to_string_lossy())
        ),
    ]);
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn log_sys_unavailable(lib_name: &str, target_triple: &str, err: &SoldrError) {
    eprintln!(
        "soldr build: catalogue {lib_name} sysroot unavailable for \
         {target_triple}: {err}"
    );
    eprintln!(
        "soldr build: continuing — {lib_name}-sys will compile from its \
         vendored C source as usual"
    );
}

fn legacy_xwin_opt_out() -> bool {
    std::env::var(USE_LEGACY_XWIN_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

fn legacy_zigbuild_opt_out() -> bool {
    std::env::var(USE_LEGACY_ZIGBUILD_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Install the soldr-clang-shim binary at `<paths.bin>/clang(.exe)`
/// and matching siblings (`clang++`). Idempotent — overwrites if
/// already present (cheap; the shim is ~few hundred KB).
///
/// Returns the directory the shim was installed into, ready for
/// PATH prepending.
/// Fetch the managed LLVM toolchain (clang / clang++ / clang-cl / lld /
/// ld.lld / lld-link / llvm-ar / llvm-lib / llvm-ranlib, from
/// `zackees/clang-tool-chain-bins`) and prepend its `bin/` to the child
/// cargo's PATH via `prep.path_dirs`.
///
/// This is what lets `soldr build --target {*-apple-darwin,
/// *-pc-windows-msvc}` run on a bare Linux runner with **no host
/// `apt install clang lld llvm`** — soldr is a bootstrapping tool, so it
/// uses the right toolchain rather than requiring the host to
/// pre-install it (soldr#1309).
///
/// Non-fatal on miss: logs and falls through so a host-provided LLVM
/// still works if the catalogue asset is unavailable for the host arch.
async fn ensure_llvm_on_path(
    paths: &SoldrPaths,
    prep: &mut BlessedPrep,
    target_triple: &str,
) -> bool {
    match crate::fetch::ensure_llvm_toolchain(paths).await {
        Ok(bin_dir) => {
            // Prepend so the managed clang/llvm-ar/lld win over any host
            // copies; the clang shim (msvc) is prepended even earlier by
            // the caller and only shadows `clang`/`clang++`, so clang-cl
            // / lld-link / llvm-lib still resolve here.
            prep.path_dirs.insert(0, bin_dir);
            true
        }
        Err(e) => {
            eprintln!("soldr build: managed LLVM toolchain unavailable for {target_triple}: {e}");
            eprintln!("soldr build: continuing — relying on any host clang/llvm/lld on PATH");
            false
        }
    }
}

fn install_clang_shim(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    paths.ensure_dirs()?;

    let shim_dir = paths.bin.join("clang-shim");
    std::fs::create_dir_all(&shim_dir)?;

    // The `soldr-clang-shim` binary lives next to the soldr binary
    // (both are produced by the same workspace build). On a soldr-
    // released install, it's at `<exe-dir>/soldr-clang-shim(.exe)`.
    let shim_src = locate_shim_binary()?;

    // soldr#1265 — preserve the source binary's mtime on the installed
    // shim. cc-rs uses the CC binary's mtime in some build-script paths;
    // without preservation every `soldr build` invocation looked like a
    // fresh compiler → cargo invalidated cc-rs-touched deps on darwin/
    // MSVC CI lanes. Preserving mtime keeps the shim's timestamp aligned
    // with the source (stable when bootstrap cache HITs).
    let src_mtime = std::fs::metadata(&shim_src)
        .and_then(|meta| meta.modified())
        .map_err(|e| {
            SoldrError::Other(format!(
                "failed to read source mtime for soldr-clang-shim: {e}"
            ))
        })?;

    for name in clang_shim_names() {
        let dst = shim_dir.join(&name);
        // Best-effort remove first; ignore errors (file may not exist).
        let _ = std::fs::remove_file(&dst);
        // Use std::fs::copy for cross-platform; symlinks would be
        // cleaner on POSIX but the implementation cost isn't worth
        // it for what's effectively a 0-cost-per-invocation file copy.
        std::fs::copy(&shim_src, &dst).map_err(|e| {
            SoldrError::Other(format!(
                "failed to install soldr-clang-shim at {}: {e}",
                dst.display()
            ))
        })?;

        // Preserve source mtime — see block comment above the loop.
        std::fs::File::options()
            .write(true)
            .open(&dst)
            .and_then(|f| f.set_modified(src_mtime))
            .map_err(|e| {
                SoldrError::Other(format!(
                    "failed to preserve mtime on installed soldr-clang-shim {}: {e}",
                    dst.display()
                ))
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dst)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dst, perms)?;
        }
    }

    Ok(shim_dir)
}

#[cfg(windows)]
fn clang_shim_names() -> Vec<String> {
    // Only `clang` + `clang++` — soldr-clang-shim's `from_argv0` accepts
    // those two basenames (plus `soldr-clang-shim` itself). DO NOT add
    // `clang-cl` here: the shim invokes `clang-cl` as its DOWNSTREAM (it
    // exists to route clang→clang-cl), and if `clang-cl` is also a
    // symlink to the shim then PATH resolution finds the shim's own
    // clang-cl symlink first and the shim re-invokes itself with
    // argv[0]=clang-cl → `unrecognized argv[0] basename`. See
    // ci/docker-aarch64-windows-msvc-cross/ + soldr#1033 followup.
    vec!["clang.exe".to_string(), "clang++.exe".to_string()]
}

#[cfg(not(windows))]
fn clang_shim_names() -> Vec<String> {
    // See the Windows branch for why clang-cl is NOT here.
    vec!["clang".to_string(), "clang++".to_string()]
}

fn locate_shim_binary() -> Result<PathBuf, SoldrError> {
    // Look next to the running `soldr` executable.
    let current_exe = std::env::current_exe()
        .map_err(|e| SoldrError::Other(format!("could not resolve current exe: {e}")))?;
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| SoldrError::Other("current exe has no parent directory".to_string()))?;

    let shim_name = if cfg!(windows) {
        "soldr-clang-shim.exe"
    } else {
        "soldr-clang-shim"
    };
    let candidate = exe_dir.join(shim_name);
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(SoldrError::Other(format!(
        "soldr-clang-shim binary not found at {}; expected next to the \
         running soldr exe. Reinstall soldr or rebuild from source.",
        candidate.display()
    )))
}

/// Build the MSVC-style include-flag string that cargo-xwin would
/// have injected for a child cargo invocation. Format:
/// `"/imsvc <crt/include> /imsvc <sdk/include/ucrt> ..."` — each
/// `/imsvc` is followed by the path as a separate token. clang-cl
/// natively recognizes `/imsvc <path>` as an MSVC-style include
/// directive (equivalent to clang's `-isystem <path>`).
///
/// Paths that don't exist on disk are silently skipped — defends
/// against catalogue-shape drift (e.g. a future xwin-cache that
/// stops shipping the winrt include tree shouldn't make the whole
/// CFLAGS injection error out).
fn xwin_msvc_cflags(cache_dir: &std::path::Path) -> String {
    let candidates = [
        cache_dir.join("crt").join("include"),
        cache_dir.join("sdk").join("include").join("ucrt"),
        cache_dir.join("sdk").join("include").join("um"),
        cache_dir.join("sdk").join("include").join("shared"),
        cache_dir.join("sdk").join("include").join("winrt"),
        cache_dir.join("sdk").join("include").join("cppwinrt"),
    ];
    candidates
        .iter()
        .filter(|p| p.is_dir())
        // No space between `/imsvc` and the path. clang-cl accepts
        // the `/imsvc<path>` joined form; the two-token `/imsvc <path>`
        // form gets mangled because cc-rs receives CFLAGS, splits on
        // whitespace, then passes each token as a separate argv entry
        // — and clang-cl ends up seeing `/imsvc <path>` as two
        // unrelated args (the path arg is treated as a positional
        // source file). soldr#1070 root cause.
        .map(|p| format!("/imsvc{}", p.display()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the rustc `-C link-arg=/LIBPATH:<path>` chain for the
/// xwin-cache so lld-link finds the MSVC import libs without
/// cargo-xwin being in the loop. Returns a whitespace-separated
/// rustflags string consumable via `CARGO_TARGET_<T>_RUSTFLAGS`.
///
/// The xwin tarball lays libs out per-arch as `crt/lib/<arch>/` and
/// `sdk/lib/{um,ucrt}/<arch>/` where `<arch>` is the MS arch name
/// (`x64`, `arm64`) — matching xwin's `--preserve-ms-arch-notation`
/// flag in the upstream recipe.
fn xwin_msvc_link_args(cache_dir: &std::path::Path, target_triple: &str) -> String {
    let arch = if target_triple.starts_with("aarch64-") {
        "arm64"
    } else if target_triple.starts_with("x86_64-") {
        "x64"
    } else {
        return String::new();
    };
    let candidates = [
        cache_dir.join("crt").join("lib").join(arch),
        cache_dir.join("sdk").join("lib").join("um").join(arch),
        cache_dir.join("sdk").join("lib").join("ucrt").join(arch),
    ];
    candidates
        .iter()
        .filter(|p| p.is_dir())
        .flat_map(|p| {
            vec![
                "-C".to_string(),
                format!("link-arg=/LIBPATH:{}", p.display()),
            ]
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that mutate process env vars. `std::env::set_var`
    /// / `remove_var` mutate global state, and cargo runs tests in
    /// parallel within a single process — without a barrier the tests
    /// race and intermittently fail (soldr#1267).
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    crate::timed_test!(opt_out_env_var_recognized, {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os(USE_LEGACY_XWIN_ENV_VAR);

        std::env::remove_var(USE_LEGACY_XWIN_ENV_VAR);
        assert!(!legacy_xwin_opt_out());

        std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, "1");
        assert!(legacy_xwin_opt_out());

        std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, "0");
        assert!(!legacy_xwin_opt_out(), "literal '0' must not opt in");

        std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, "");
        assert!(!legacy_xwin_opt_out(), "empty value must not opt in");

        match prev {
            Some(v) => std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, v),
            None => std::env::remove_var(USE_LEGACY_XWIN_ENV_VAR),
        }
    });

    crate::timed_test!(linux_targets_get_no_xwin_or_sdk_prep, {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // soldr#1064 Phase B: syslib catalogue overrides may populate
        // prep.env even on linux targets. The invariant we still want to
        // assert is "linux gets no Windows-xwin and no Apple-SDK prep" —
        // opt out of catalogue injection (and the managed cmake/ninja
        // injection, which is host-side and target-independent) to keep
        // this test hermetic.
        std::env::set_var(USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
        std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, "1");
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(prepare(&paths, "x86_64-unknown-linux-musl"));
        std::env::remove_var(USE_LEGACY_VENDORED_SYS_ENV_VAR);
        std::env::remove_var(USE_SYSTEM_CMAKE_ENV_VAR);
        let prep = result.expect("linux musl target should not error");
        assert!(prep.xwin_cache_dir.is_none());
        assert!(prep.sdkroot.is_none());
        assert!(prep.env.is_empty());
        assert!(prep.cargo_args.is_empty());
    });

    crate::timed_test!(cmake_generator_sweep_removes_only_mismatches, {
        // Simulate a pre-Ninja cached target tree: one Unix Makefiles
        // cache in the host-profile layout, one Ninja cache in the
        // per-triple layout, one Visual Studio cache in the per-triple
        // layout. Sweep for Ninja: the mismatched two vanish, the
        // Ninja one survives.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let target = tmp.path();

        let mk = |rel: &str, generator: &str| {
            let out_build = target.join(rel).join("out").join("build");
            std::fs::create_dir_all(&out_build).unwrap();
            std::fs::write(
                out_build.join("CMakeCache.txt"),
                format!("SOMEVAR:BOOL=ON\nCMAKE_GENERATOR:INTERNAL={generator}\n"),
            )
            .unwrap();
            out_build
        };

        let stale_host = mk("release/build/libz-ng-sys-aaa", "Unix Makefiles");
        let ninja_triple = mk(
            "aarch64-apple-darwin/release/build/libz-ng-sys-bbb",
            "Ninja",
        );
        let stale_triple = mk(
            "x86_64-pc-windows-msvc/debug/build/zstd-sys-ccc",
            "Visual Studio 16 2019",
        );

        sweep_mismatched_cmake_build_dirs(target, "Ninja");

        assert!(!stale_host.exists(), "Unix Makefiles dir must be swept");
        assert!(!stale_triple.exists(), "Visual Studio dir must be swept");
        assert!(ninja_triple.exists(), "matching Ninja dir must survive");
        assert!(
            ninja_triple.join("CMakeCache.txt").is_file(),
            "surviving cache intact"
        );
    });

    crate::timed_test!(cmake_generator_sweep_tolerates_missing_target, {
        // A nonexistent target root is a no-op, not an error.
        sweep_mismatched_cmake_build_dirs(std::path::Path::new("Z:/does/not/exist"), "Ninja");
    });

    crate::timed_test!(cmake_injection_respects_system_opt_out, {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let mut prep = BlessedPrep::default();

        std::env::set_var(USE_SYSTEM_CMAKE_ENV_VAR, "1");
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(inject_cmake_tooling(&paths, &mut prep));
        std::env::remove_var(USE_SYSTEM_CMAKE_ENV_VAR);

        assert!(prep.env.is_empty(), "opt-out must inject nothing");
        assert!(prep.path_dirs.is_empty());
    });

    crate::timed_test!(cmake_injection_defers_to_user_cmake_env, {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // A caller-provided CMAKE (or CMAKE_GENERATOR) means the user
        // already decided; the managed injection must stand down
        // entirely — no fetch attempt, no env, no PATH dirs.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();

        for user_var in ["CMAKE", "CMAKE_GENERATOR"] {
            let mut prep = BlessedPrep::default();
            std::env::set_var(user_var, "user-choice");
            rt.block_on(inject_cmake_tooling(&paths, &mut prep));
            std::env::remove_var(user_var);
            assert!(
                prep.env.is_empty(),
                "pre-set {user_var} must suppress injection"
            );
            assert!(prep.path_dirs.is_empty());
        }
    });

    crate::timed_test!(mimalloc_build_script_override_uses_target_config, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let sysroot = tmp.path().join("mimalloc sysroot");
        let mut prep = BlessedPrep::default();

        add_mimalloc_build_script_override(&mut prep, "x86_64-unknown-linux-gnu", &sysroot);

        assert_eq!(prep.cargo_args.len(), 6);
        assert_eq!(prep.cargo_args[0], "--config");
        assert_eq!(
            prep.cargo_args[1],
            "target.x86_64-unknown-linux-gnu.mimalloc.rustc-link-lib=[\"static=mimalloc\"]"
        );
        assert_eq!(prep.cargo_args[2], "--config");
        assert!(prep.cargo_args[3]
            .starts_with("target.x86_64-unknown-linux-gnu.mimalloc.rustc-link-search=[\""));
        assert!(prep.cargo_args[3].contains("mimalloc sysroot"));
        assert_eq!(prep.cargo_args[4], "--config");
        assert!(prep.cargo_args[5]
            .starts_with("target.x86_64-unknown-linux-gnu.mimalloc.metadata_include_dir=\""));
    });

    crate::timed_test!(xwin_cflags_emits_imsvc_for_present_dirs, {
        // soldr#1036: simulate an xwin-cache layout, confirm CFLAGS
        // string contains an `/imsvc <path>` entry for each present
        // include subtree (and skips absent ones).
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path();
        // Materialize crt/include + sdk/include/ucrt only; leave the
        // others absent to prove the filter works.
        std::fs::create_dir_all(root.join("crt").join("include")).unwrap();
        std::fs::create_dir_all(root.join("sdk").join("include").join("ucrt")).unwrap();

        let cflags = xwin_msvc_cflags(root);
        assert!(
            cflags.contains("/imsvc"),
            "cflags must contain /imsvc directive: {cflags}"
        );
        // Both materialized paths should appear, separated by `/imsvc`.
        let imsvc_count = cflags.matches("/imsvc").count();
        assert_eq!(
            imsvc_count, 2,
            "expected 2 /imsvc entries (one per present subtree), got: {cflags}"
        );
        // Absent winrt subtree must NOT have an entry.
        assert!(
            !cflags.contains("winrt"),
            "absent winrt subtree leaked into cflags: {cflags}"
        );
    });

    crate::timed_test!(xwin_cflags_empty_for_empty_cache, {
        // No subtrees present → empty cflags string. Caller can detect
        // this and skip the CFLAGS_<t> env var injection entirely.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let cflags = xwin_msvc_cflags(tmp.path());
        assert!(cflags.is_empty(), "expected empty cflags, got: {cflags:?}");
    });

    crate::timed_test!(xwin_link_args_picks_correct_arch_subdir, {
        // Confirm aarch64-pc-windows-msvc looks under `arm64/`,
        // x86_64-pc-windows-msvc looks under `x64/`. This is the
        // MS-arch-notation contract from xwin's
        // --preserve-ms-arch-notation flag in the upstream recipe.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path();
        for arch in ["arm64", "x64"] {
            std::fs::create_dir_all(root.join("crt").join("lib").join(arch)).unwrap();
            std::fs::create_dir_all(root.join("sdk").join("lib").join("um").join(arch)).unwrap();
            std::fs::create_dir_all(root.join("sdk").join("lib").join("ucrt").join(arch)).unwrap();
        }

        let aarch64 = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc");
        assert!(
            aarch64.contains("/arm64") || aarch64.contains("\\arm64"),
            "aarch64 must hit arm64 subdir: {aarch64}"
        );
        assert!(
            !aarch64.contains("/x64") && !aarch64.contains("\\x64"),
            "aarch64 link args leaked x64 path: {aarch64}"
        );

        let x86 = xwin_msvc_link_args(root, "x86_64-pc-windows-msvc");
        assert!(
            x86.contains("/x64") || x86.contains("\\x64"),
            "x86_64 must hit x64 subdir: {x86}"
        );
        assert!(
            !x86.contains("/arm64") && !x86.contains("\\arm64"),
            "x86_64 link args leaked arm64 path: {x86}"
        );
    });

    crate::timed_test!(xwin_link_args_unknown_arch_returns_empty, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        // Non-MSVC triple → empty link args.
        let out = xwin_msvc_link_args(tmp.path(), "x86_64-unknown-linux-gnu");
        assert!(
            out.is_empty(),
            "non-msvc triple must yield empty link args, got: {out:?}"
        );
    });

    crate::timed_test!(xwin_link_args_format_uses_c_link_arg_pairs, {
        // Each /LIBPATH: must be paired with a leading `-C` so rustc
        // parses them as link-args. Without the `-C` prefix the flag
        // would be passed as a plain rustc arg and silently dropped.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("crt").join("lib").join("arm64")).unwrap();

        let out = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc");
        // Count `-C` tokens vs `link-arg=/LIBPATH:` tokens; should be equal.
        let dash_c = out.split_whitespace().filter(|t| *t == "-C").count();
        let link_arg = out
            .split_whitespace()
            .filter(|t| t.starts_with("link-arg=/LIBPATH:"))
            .count();
        assert_eq!(
            dash_c, link_arg,
            "every link-arg must be preceded by -C: {out}"
        );
        assert!(dash_c >= 1, "expected at least one link-arg pair: {out}");
    });
}
