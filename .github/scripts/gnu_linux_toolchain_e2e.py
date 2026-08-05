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


def assert_plan(soldr: str, target: str) -> None:
    payload = json.loads(run([soldr, "env", "--target", target, "--json"]))
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
) -> dict[str, str]:
    command = [soldr, "prepare", "--target", target, "--github-env", str(env_file)]
    if save is not None:
        command.extend(["--save", str(save)])
    if restore is not None:
        command.extend(["--restore", str(restore)])
    output = run(command, env=env)
    require_no_zig(output, "GNU preparation")
    prepared_env = os.environ.copy() if env is None else env.copy()
    prepared_env.update(read_github_env(env_file))
    return prepared_env


def assert_managed_environment(env: dict[str, str], target: str) -> tuple[Path, Path]:
    prefix, _ = TARGETS[target]
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
    for alias, source in (
        ("CC", "CMAKE_C_COMPILER"),
        ("CXX", "CMAKE_CXX_COMPILER"),
        ("AR", "CMAKE_AR"),
        ("RANLIB", "CMAKE_RANLIB"),
    ):
        if env.get(alias) != env[source]:
            raise RuntimeError(
                f"external CMake alias {alias} did not preserve {source}"
            )
    for key in ("CMAKE_SYSROOT", "PKG_CONFIG_SYSROOT_DIR"):
        if Path(env[key]) != sysroot:
            raise RuntimeError(f"{key} does not select the managed sysroot: {env[key]}")
    if str(sysroot) not in env["PKG_CONFIG_LIBDIR"]:
        raise RuntimeError("pkg-config did not receive the managed sysroot")
    if "--sysroot=" + str(sysroot) not in env[f"CFLAGS_{suffix}"]:
        raise RuntimeError("C compiler flags did not carry the managed sysroot")
    require_no_zig("\n".join(env.values()), "prepared GNU environment")
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
        '[toolchain]\nchannel = "1.94.1"\n', encoding="utf-8"
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

fn cmake(args: &[&str]) {
    let status = Command::new(env::var("CMAKE").expect("managed CMAKE"))
        .args(args)
        .status()
        .expect("run managed CMake");
    assert!(status.success(), "managed CMake failed");
}

fn main() {
    cc::Build::new().file("native/probe.c").compile("cc_probe");
    cc::Build::new().cpp(true).file("native/probe.cc").compile("cxx_probe");
    pkg_config::Config::new().cargo_metadata(false).probe("zlib-ng").expect("managed pkg-config zlib-ng");
    let out = env::var("OUT_DIR").expect("OUT_DIR");
    let install = Path::new(&out).join("cmake-install");
    let build = Path::new(&out).join("cmake-build");
    cmake(&["-S", "cmake-probe", "-B", build.to_str().unwrap(), "-DCMAKE_BUILD_TYPE=Release", &format!("-DCMAKE_INSTALL_PREFIX={}", install.display())]);
    cmake(&["--build", build.to_str().unwrap(), "--target", "install"]);
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
    assert_plan(args.soldr, args.target)
    with tempfile.TemporaryDirectory(
        prefix=f"soldr-gnu-e2e-{TARGETS[args.target][0]}-"
    ) as raw:
        work = Path(raw)
        archive = work / "prepared.tar.zst"
        source_env = prepare(
            args.soldr, args.target, work / "source-github.env", save=archive
        )
        source_root, _ = assert_managed_environment(source_env, args.target)
        # Warm Cargo's fixture dependencies before proving that the restored
        # toolchain itself is enough when Soldr is forbidden from networking.
        build_fixture(args.soldr, args.target, source_env, work / "source-build")

        restored_env = os.environ.copy()
        restored_env["SOLDR_CACHE_DIR"] = str(work / "restored-soldr")
        restored_env["SOLDR_TEST_NO_NETWORK"] = "1"
        env = prepare(
            args.soldr,
            args.target,
            work / "restored-github.env",
            env=restored_env,
            restore=archive,
        )
        root, _ = assert_managed_environment(env, args.target)
        if root == source_root:
            raise RuntimeError(
                "prepare archive did not restore into the clean Soldr root"
            )
        binary = build_fixture(args.soldr, args.target, env, work / "restored-build")
        verify_artifact(args.repo, args.target, root, binary, env)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
