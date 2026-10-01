//! soldr#3520: the managed clang drives a Linux link only when it can see the
//! host's C runtime; otherwise the host's wrapper clang (NixOS cc-wrapper)
//! does.

use super::{managed_clang_finds_host_runtime, prefer_host_wrapper_clang};
use std::path::{Path, PathBuf};

/// A fake clang whose `-print-file-name=<f>` answers like a real one: the
/// absolute path when `<f>` is under `lib_dir`, else the bare name.
fn fake_clang(dir: &Path, name: &str, lib_dir: &Path) -> PathBuf {
    let clang = dir.join(name);
    std::fs::write(
        &clang,
        format!(
            "#!/bin/sh\nf=\"${{1#-print-file-name=}}\"\n\
             if [ -e '{lib}'/\"$f\" ]; then echo '{lib}'/\"$f\"; else echo \"$f\"; fi\n",
            lib = lib_dir.display()
        ),
    )
    .unwrap();
    crate::platform::fs::permissions::make_executable(&clang).unwrap();
    clang
}

#[test]
fn a_wrapper_only_libgcc_s_makes_the_system_clang_drive_the_link() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    // NixOS shape: the runtime lives only in a store path the managed
    // (unwrapped) clang does not search; only the wrapper knows it.
    let fhs = temp.path().join("usr/lib");
    let store = temp.path().join("nix/store/hash-gcc-15.3.0-lib/lib");
    std::fs::create_dir_all(&fhs).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    for file in ["Scrt1.o", "libgcc_s.so"] {
        std::fs::write(store.join(file), b"").unwrap();
    }
    let managed = fake_clang(temp.path(), "managed-clang", &fhs);
    let system = PathBuf::from("/run/current-system/sw/bin/clang");

    assert!(!managed_clang_finds_host_runtime(&managed));
    assert_eq!(
        prefer_host_wrapper_clang(
            managed.clone(),
            Some(system.clone()),
            true,
            managed_clang_finds_host_runtime
        ),
        system,
        "a managed clang blind to libgcc_s must yield to the host wrapper"
    );

    // Only libgcc_s is wrapper-only: still the wrapper.
    std::fs::write(fhs.join("Scrt1.o"), b"").unwrap();
    assert!(!managed_clang_finds_host_runtime(&managed));

    // An FHS host (Debian, Ubuntu, Fedora): the managed clang keeps the link.
    std::fs::write(fhs.join("libgcc_s.so"), b"").unwrap();
    assert!(managed_clang_finds_host_runtime(&managed));
    assert_eq!(
        prefer_host_wrapper_clang(
            managed.clone(),
            Some(system.clone()),
            true,
            managed_clang_finds_host_runtime
        ),
        managed
    );
}

#[test]
fn the_managed_clang_stays_off_linux_without_a_system_clang_or_when_unprobeable() {
    let managed = PathBuf::from("/managed/clang");
    let system = PathBuf::from("/usr/bin/clang");
    let blind = |_: &Path| false;
    assert_eq!(
        prefer_host_wrapper_clang(managed.clone(), Some(system.clone()), false, blind),
        managed,
        "Windows/macOS hosts keep the managed clang"
    );
    assert_eq!(
        prefer_host_wrapper_clang(managed.clone(), None, true, blind),
        managed,
        "no usable system clang: nothing better to use"
    );
    assert!(
        managed_clang_finds_host_runtime(Path::new("/nonexistent/soldr-3520/clang")),
        "a probe that cannot run must not change the driver"
    );
}
