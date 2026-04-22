#!/usr/bin/env node
"use strict";

const childProcess = require("child_process");
const fs = require("fs");
const path = require("path");

const binaryName = process.platform === "win32" ? "soldr.exe" : "soldr";
const binaryPath = path.join(__dirname, "native", binaryName);

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

const child = childProcess.spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
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
