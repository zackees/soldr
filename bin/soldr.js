#!/usr/bin/env node
"use strict";

const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");
const zccacheContract = require("../scripts/zccache-contract");

const binaryName = process.platform === "win32" ? "soldr.exe" : "soldr";
const nativeDir = path.join(__dirname, "native");
const binaryPath = path.join(nativeDir, binaryName);

if (!fs.existsSync(binaryPath)) {
  console.error(
    [
      "soldr: native binary is missing from the npm package install.",
      `expected: ${binaryPath}`,
      "Try reinstalling the package, or run `npm rebuild soldr`.",
    ].join("\n"),
  );
  process.exit(1);
}

// Wire the bundled zccache trio (zccache, zccache-daemon, zccache-fp,
// shipped alongside soldr in `bin/native/` since the combined-archive
// release format landed) into soldr's local-zccache resolution path.
// Soldr's ZccacheResolver checks SOLDR_ZCCACHE_LOCAL_DIR ahead of the
// pinned and managed-download chain, so this turns the bundled
// binaries into the active zccache automatically — no manual env
// setup, no managed fetch over the network. Users who explicitly set
// the env var themselves keep their override.
const exeExt = process.platform === "win32" ? ".exe" : "";
const childEnv = { ...process.env };
const zccacheLocalDirEnv = zccacheContract.CONTRACT.zccache.local_dir_env;
if (
  !childEnv[zccacheLocalDirEnv] &&
  zccacheContract.ZCCACHE_BUNDLED_BINARIES.every((baseName) =>
    fs.existsSync(path.join(nativeDir, `${baseName}${exeExt}`)),
  )
) {
  childEnv[zccacheLocalDirEnv] = nativeDir;
}

// Same wiring for the bundled crgx binary (shipped alongside soldr
// since the crgx-bundling follow-up to the combined-archive release
// format). soldr's `fetch_tool_with_paths` honors
// SOLDR_CRGX_LOCAL_DIR ahead of the GitHub Releases / crates.io
// fetch chain, so `soldr crgx ...` runs the bundled binary with no
// network round trip. Caller-set overrides win.
const crgxLocalDirEnv = zccacheContract.CONTRACT.crgx.local_dir_env;
if (
  !childEnv[crgxLocalDirEnv] &&
  fs.existsSync(path.join(nativeDir, `${zccacheContract.CRGX_BUNDLED_BINARY}${exeExt}`))
) {
  childEnv[crgxLocalDirEnv] = nativeDir;
}

// Same wiring for bundled cargo-chef. `soldr cook` invokes
// `soldr cargo chef ...`, whose resolver checks
// SOLDR_CARGO_CHEF_LOCAL_DIR before GitHub Releases. This avoids a
// live upstream lookup on targets cargo-chef does not publish, such as
// macOS arm64.
const cargoChefLocalDirEnv = zccacheContract.CONTRACT.cargo_chef.local_dir_env;
if (
  !childEnv[cargoChefLocalDirEnv] &&
  fs.existsSync(path.join(nativeDir, `${zccacheContract.CARGO_CHEF_BUNDLED_BINARY}${exeExt}`))
) {
  childEnv[cargoChefLocalDirEnv] = nativeDir;
}

const child = childProcess.spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env: childEnv,
  windowsHide: false,
});

child.on("error", (error) => {
  console.error(`soldr: failed to launch ${binaryPath}: ${error.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code === null ? 1 : code);
});
