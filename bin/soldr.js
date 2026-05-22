#!/usr/bin/env node
"use strict";

const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");

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
// Soldr's `fetch_zccache_with_paths` checks SOLDR_ZCCACHE_LOCAL_DIR
// ahead of the managed-download chain, so this turns the bundled
// binaries into the active zccache automatically — no manual env
// setup, no managed fetch over the network. Users who explicitly set
// the env var themselves keep their override.
const zccacheExt = process.platform === "win32" ? ".exe" : "";
const childEnv = { ...process.env };
if (
  !childEnv.SOLDR_ZCCACHE_LOCAL_DIR &&
  fs.existsSync(path.join(nativeDir, `zccache${zccacheExt}`)) &&
  fs.existsSync(path.join(nativeDir, `zccache-daemon${zccacheExt}`)) &&
  fs.existsSync(path.join(nativeDir, `zccache-fp${zccacheExt}`))
) {
  childEnv.SOLDR_ZCCACHE_LOCAL_DIR = nativeDir;
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
