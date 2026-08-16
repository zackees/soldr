#!/usr/bin/env python3
"""Exercise the catalogue-backed GNU lifecycle without a host or Zig fallback.

This runs on Linux CI after Soldr itself has been built.  Its temporary
fixture makes cc-rs compile C and C++, invokes the CMake binary exported by
``soldr prepare --github-env``, probes zlib through pkg-config, and verifies
the produced ELF.  Nothing is retained after the process exits.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import subprocess
import sys
import tempfile
from pathlib import Path

TARGETS = {
    "x86_64-unknown-linux-gnu": ("x86_64", "Advanced Micro Devices X86-64"),
    "aarch64-unknown-linux-gnu": ("aarch64", "AArch64"),
}
VALID_ENV = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
OUTER_SOLDR_ENV = (
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "SOLDR_BROKER_SERVICE",
    "SOLDR_INTERNAL_DAEMON_EXE",
)


def fresh_checkout_env(source: dict[str, str] | None = None) -> dict[str, str]:
    """Drop setup-soldr state before exercising the checkout-built binary."""
    env = os.environ.copy() if source is None else source.copy()
    for name in OUTER_SOLDR_ENV:
        env.pop(name, None)
    return env


def run(
    args: list[str], *, env: dict[str, str] | None = None, cwd: Path | None = None
) -> str:
    completed = subprocess.run(
        args,
        check=False,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if completed.returncode:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {' '.join(args)}\n{completed.stdout}"
        )
    return completed.stdout


def stop_soldr_broker(soldr: str, env: dict[str, str]) -> None:
    # Routes are path-derived (soldr#2479), so this checkout-built binary's
    # broker is the isolated one; no program name is needed to reach it.
    try:
        subprocess.run(
            [soldr, "broker", "stop"],
            env=env,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"warning: best-effort isolated broker stop failed: {exc}", flush=True)


def read_github_env(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        key, separator, value = raw.partition("=")
        if separator and VALID_ENV.fullmatch(key):
            values[key] = value
    return values


def require_no_zig(text: str, context: str) -> None:
    lowered = text.lower()
    for forbidden in ("zig", "cargo-zigbuild", "ziglang"):
        if forbidden in lowered:
            raise RuntimeError(f"{context} unexpectedly references {forbidden}: {text}")


def assert_plan(soldr: str, target: str, env: dict[str, str]) -> None:
    payload = json.loads(run([soldr, "env", "--target", target, "--json"], env=env))
    plan = payload["target_plan"]
    rendered = json.dumps(plan, sort_keys=True)
    require_no_zig(rendered, "GNU target plan")
    if plan["toolchain"]["family"] != "linux-gnu":
        raise RuntimeError(f"unexpected GNU toolchain family: {rendered}")
    if plan["platform"]["provider"] != "soldr-toolchain":
        raise RuntimeError(f"GNU plan lost catalogue provider: {rendered}")
    if not plan["cache_identity"].startswith("gnu-linux-toolchain/"):
        raise RuntimeError(f"GNU plan lost deterministic cache identity: {rendered}")


def prepare(
    soldr: str,
    target: str,
    env_file: Path,
    *,
    env: dict[str, str] | None = None,
    save: Path | None = None,
    restore: Path | None = None,
) -> tuple[dict[str, str], dict[str, str]]:
    """Run `soldr prepare`; return `(env_to_build_with, vars_soldr_wrote)`.

    Deliberately two values. The first is the full environment a build needs,
    which necessarily inherits the ambient one. The second is only what soldr
    set, and is the only thing the no-Zig assertion may inspect.
    """
    command = [soldr, "prepare", "--target", target, "--github-env", str(env_file)]
    if save is not None:
        command.extend(["--save", str(save)])
    if restore is not None:
        command.extend(["--restore", str(restore)])
    output = run(command, env=env)
    require_no_zig(output, "GNU preparation")
    managed = read_github_env(env_file)
    prepared_env = os.environ.copy() if env is None else env.copy()
    prepared_env.update(managed)
    return prepared_env, managed


def assert_managed_environment(
    env: dict[str, str], target: str, managed: dict[str, str]
) -> tuple[Path, Path]:
    suffix = target.replace("-", "_")
    upper = suffix.upper()
    root = Path(env["SOLDR_GNU_LINUX_TOOLCHAIN_ROOT"])
    sysroot = Path(env["SOLDR_GNU_LINUX_SYSROOT"])
    if not root.is_dir() or not sysroot.is_dir() or root not in sysroot.parents:
        raise RuntimeError(f"invalid managed GNU root/sysroot: {root} / {sysroot}")
    required = {
        f"CC_{suffix}": "gcc",
        f"CXX_{suffix}": "g++",
        f"AR_{suffix}": "ar",
        f"RANLIB_{suffix}": "ranlib",
        f"CARGO_TARGET_{upper}_LINKER": "gcc",
        "CMAKE_C_COMPILER": "gcc",
        "CMAKE_CXX_COMPILER": "g++",
        "CMAKE_AR": "ar",
        "CMAKE_RANLIB": "ranlib",
        "CMAKE_LINKER": "ld",
    }
    for key, tool in required.items():
        value = Path(env[key])
        if (
            not value.is_file()
            or root not in value.parents
            or not value.name.endswith(tool)
        ):
            raise RuntimeError(f"{key} is not a managed GNU {tool}: {value}")
        require_no_zig(str(value), key)
        run([str(value), "--version"], env=env)
    host_arch = (
        platform.machine()
        .lower()
        .replace("amd64", "x86_64")
        .replace("arm64", "aarch64")
    )
    is_native_target = host_arch == TARGETS[target][0]
    for alias, source in (
        ("CC", "CMAKE_C_COMPILER"),
        ("CXX", "CMAKE_CXX_COMPILER"),
        ("AR", "CMAKE_AR"),
        ("RANLIB", "CMAKE_RANLIB"),
    ):
        if is_native_target and env.get(alias) != env[source]:
            raise RuntimeError(
                f"external CMake alias {alias} did not preserve {source}"
            )
        if not is_native_target and env.get(alias) == env[source]:
            raise RuntimeError(
                f"cross-target alias {alias} leaked the managed target compiler"
            )
    for key in ("CMAKE_SYSROOT", "PKG_CONFIG_SYSROOT_DIR"):
        if Path(env[key]) != sysroot:
            raise RuntimeError(f"{key} does not select the managed sysroot: {env[key]}")
    if str(sysroot) not in env["PKG_CONFIG_LIBDIR"]:
        raise RuntimeError("pkg-config did not receive the managed sysroot")
    if "--sysroot=" + str(sysroot) not in env[f"CFLAGS_{suffix}"]:
        raise RuntimeError("C compiler flags did not carry the managed sysroot")
    # Only the variables soldr wrote. Scanning the whole environment matched
    # any ambient value containing "zig" -- including GITHUB_HEAD_REF, so a
    # branch named `...-xwin-zig-optin` failed this lane with "prepared GNU
    # environment unexpectedly references zig: /bin/bash", naming an
    # unrelated value because the message prints the whole blob.
    require_no_zig("\n".join(managed.values()), "prepared GNU environment")
    return root, sysroot


def write_fixture(root: Path) -> None:
    (root / "src").mkdir(parents=True)
    (root / "native").mkdir()
    (root / "cmake-probe").mkdir()
    (root / "Cargo.toml").write_text(
        """[package]
name = "gnu-toolchain-e2e"
version = "0.1.0"
edition = "2021"
build = "build.rs"

[build-dependencies]
cc = "1"
pkg-config = "0.3"
""",
        encoding="utf-8",
    )
    (root / "rust-toolchain.toml").write_text(
        '[toolchain]\nchannel = "1.95.0"\n', encoding="utf-8"
    )
    (root / "src" / "main.rs").write_text(
        'extern "C" { fn cc_probe() -> i32; }\nfn main() { assert_eq!(unsafe { cc_probe() }, 7); }\n',
        encoding="utf-8",
    )
    (root / "native" / "probe.c").write_text(
        "int cc_probe(void) { return 7; }\n", encoding="utf-8"
    )
    (root / "native" / "probe.cc").write_text(
        'extern "C" int cxx_probe(void) { return 11; }\n', encoding="utf-8"
    )
    (root / "cmake-probe" / "CMakeLists.txt").write_text(
        """cmake_minimum_required(VERSION 3.20)
project(gnu_toolchain_probe LANGUAGES C CXX)
add_library(cmake_probe STATIC probe.c probe.cc)
install(TARGETS cmake_probe ARCHIVE DESTINATION lib)
""",
        encoding="utf-8",
    )
    (root / "cmake-probe" / "probe.c").write_text(
        "int cmake_c_probe(void) { return 1; }\n", encoding="utf-8"
    )
    (root / "cmake-probe" / "probe.cc").write_text(
        "int cmake_cxx_probe(void) { return 2; }\n", encoding="utf-8"
    )
    (root / "build.rs").write_text(
        """use std::{env, path::Path, process::Command};

fn cmake(args: &[String]) {
    let status = Command::new(env::var("CMAKE").expect("managed CMAKE"))
        .args(args)
        .status()
        .expect("run managed CMake");
    assert!(status.success(), "managed CMake failed");
}

fn cmake_definition(name: &str) -> String {
    format!("-D{name}={}", env::var(name).unwrap_or_else(|_| panic!("managed {name}")))
}

fn main() {
    cc::Build::new().file("native/probe.c").compile("cc_probe");
    cc::Build::new().cpp(true).file("native/probe.cc").compile("cxx_probe");
    pkg_config::Config::new().cargo_metadata(false).probe("zlib-ng").expect("managed pkg-config zlib-ng");
    let out = env::var("OUT_DIR").expect("OUT_DIR");
    let install = Path::new(&out).join("cmake-install");
    let build = Path::new(&out).join("cmake-build");
    cmake(&[
        "-S".into(),
        "cmake-probe".into(),
        "-B".into(),
        build.display().to_string(),
        "-DCMAKE_BUILD_TYPE=Release".into(),
        format!("-DCMAKE_INSTALL_PREFIX={}", install.display()),
        cmake_definition("CMAKE_C_COMPILER"),
        cmake_definition("CMAKE_CXX_COMPILER"),
        cmake_definition("CMAKE_AR"),
        cmake_definition("CMAKE_RANLIB"),
        cmake_definition("CMAKE_LINKER"),
        cmake_definition("CMAKE_SYSROOT"),
    ]);
    cmake(&[
        "--build".into(),
        build.display().to_string(),
        "--target".into(),
        "install".into(),
    ]);
    println!("cargo:rustc-link-search=native={}", install.join("lib").display());
    println!("cargo:rustc-link-lib=static=cmake_probe");
}
""",
        encoding="utf-8",
    )


def build_fixture(soldr: str, target: str, env: dict[str, str], work: Path) -> Path:
    fixture = work / "fixture"
    write_fixture(fixture)
    env = env.copy()
    env["CARGO_TARGET_DIR"] = str(work / "target")
    output = run(
        [
            soldr,
            "build",
            "--release",
            "--target",
            target,
            "--manifest-path",
            str(fixture / "Cargo.toml"),
        ],
        env=env,
        cwd=fixture,
    )
    require_no_zig(output, "mixed-language GNU build")
    binary = work / "target" / target / "release" / "gnu-toolchain-e2e"
    if not binary.is_file():
        raise RuntimeError(f"expected fixture binary was not produced: {binary}")
    return binary


def verify_artifact(
    repo: Path, target: str, root: Path, binary: Path, env: dict[str, str]
) -> None:
    _, machine = TARGETS[target]
    readelf = next(root.glob("bin/*-readelf"), None)
    if readelf is None:
        raise RuntimeError(f"managed readelf is missing under {root}")
    header = run([str(readelf), "-h", str(binary)], env=env)
    if machine not in header:
        raise RuntimeError(f"unexpected ELF machine for {target}: {header}")
    run(
        [
            sys.executable,
            str(repo / ".github/scripts/verify_glibc_baseline.py"),
            "--max-glibc",
            "2.17",
            str(binary),
        ],
        env=env,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--soldr", required=True)
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--repo", type=Path, required=True)
    args = parser.parse_args()
    soldr = str(Path(args.soldr).resolve())
    checkout_env = fresh_checkout_env()
    try:
        assert_plan(soldr, args.target, checkout_env)
        with tempfile.TemporaryDirectory(
            prefix=f"soldr-gnu-e2e-{TARGETS[args.target][0]}-"
        ) as raw:
            work = Path(raw)
            archive = work / "prepared.tar.zst"
            source_env, source_managed = prepare(
                soldr,
                args.target,
                work / "source-github.env",
                env=checkout_env,
                save=archive,
            )
            source_root, _ = assert_managed_environment(
                source_env, args.target, source_managed
            )
            # Warm Cargo's fixture dependencies before proving that the restored
            # toolchain itself is enough when Soldr is forbidden from networking.
            build_fixture(soldr, args.target, source_env, work / "source-build")

            restored_env = fresh_checkout_env()
            restored_env["SOLDR_CACHE_DIR"] = str(work / "restored-soldr")
            restored_env["SOLDR_TEST_NO_NETWORK"] = "1"
            env, managed = prepare(
                soldr,
                args.target,
                work / "restored-github.env",
                env=restored_env,
                restore=archive,
            )
            root, _ = assert_managed_environment(env, args.target, managed)
            if root == source_root:
                raise RuntimeError(
                    "prepare archive did not restore into the clean Soldr root"
                )
            binary = build_fixture(soldr, args.target, env, work / "restored-build")
            verify_artifact(args.repo, args.target, root, binary, env)
            # Stop the daemon that owns the restored root BEFORE
            # TemporaryDirectory.__exit__ rmtrees it (soldr#2521 B2): a live
            # daemon still writing under restored-soldr made teardown die
            # with "Directory not empty". The `finally` below stops the
            # checkout root's broker, which is a different root, and runs
            # only after cleanup has already been attempted.
            stop_soldr_broker(soldr, restored_env)
    finally:
        stop_soldr_broker(soldr, checkout_env)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
