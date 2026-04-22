#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const assert = require("assert");

const root = path.resolve(__dirname, "..");
const pkg = require(path.join(root, "package.json"));
const install = require(path.join(root, "scripts", "install.js"));

function tomlSection(toml, sectionName) {
  const header = `[${sectionName}]`;
  const lines = toml.split(/\r?\n/);
  const body = [];
  let found = false;

  for (const line of lines) {
    if (/^\s*\[.*\]\s*$/.test(line)) {
      if (found) {
        break;
      }
      found = line.trim() === header;
      continue;
    }

    if (found) {
      body.push(line);
    }
  }

  return found ? body.join("\n") : null;
}

const cargoToml = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
const cargoVersion = cargoToml.match(/\[workspace\.package\][\s\S]*?^version = "([^"]+)"/m);
assert(cargoVersion, "workspace package version not found in Cargo.toml");
assert.strictEqual(pkg.version, cargoVersion[1], "package.json version must match Cargo.toml");

const pyprojectToml = fs.readFileSync(path.join(root, "pyproject.toml"), "utf8");
const pyprojectProject = tomlSection(pyprojectToml, "project");
assert(pyprojectProject, "[project] section not found in pyproject.toml");

assert(
  !/^\s*version\s*=/.test(pyprojectProject),
  'pyproject.toml [project] must not hardcode version; PyPI must derive it from Cargo.toml',
);

const dynamicVersion = pyprojectProject.match(/^\s*dynamic\s*=\s*\[([^\]]*)\]\s*$/m);
assert(
  dynamicVersion,
  'pyproject.toml [project] must declare dynamic = ["version"] so PyPI derives from Cargo.toml',
);

const dynamicItems = [...dynamicVersion[1].matchAll(/"([^"]+)"|'([^']+)'/g)].map(
  (match) => match[1] || match[2],
);
assert(
  dynamicItems.includes("version"),
  'pyproject.toml [project] dynamic metadata must include "version"',
);

assert.strictEqual(pkg.name, "@zackees/soldr");
assert.strictEqual(pkg.license, "BSD-3-Clause");
assert.strictEqual(pkg.bin.soldr, "bin/soldr.js");
assert.strictEqual(pkg.repository.url, "git+https://github.com/zackees/soldr.git");
assert.deepStrictEqual(pkg.files, [
  "bin/soldr.js",
  "scripts/install.js",
  "scripts/test-npm-package.js",
  "README.md",
  "LICENSE",
]);

const bin = fs.readFileSync(path.join(root, pkg.bin.soldr), "utf8");
assert(bin.startsWith("#!/usr/bin/env node"), "bin/soldr.js must have a node shebang");

assert.strictEqual(install.platformTarget("linux", "x64").triple, "x86_64-unknown-linux-gnu");
assert.strictEqual(install.platformTarget("linux", "arm64").triple, "aarch64-unknown-linux-gnu");
assert.strictEqual(install.platformTarget("darwin", "x64").triple, "x86_64-apple-darwin");
assert.strictEqual(install.platformTarget("darwin", "arm64").triple, "aarch64-apple-darwin");
assert.strictEqual(install.platformTarget("win32", "x64").triple, "x86_64-pc-windows-msvc");
assert.strictEqual(install.platformTarget("win32", "arm64").triple, "aarch64-pc-windows-msvc");
assert.throws(() => install.platformTarget("freebsd", "x64"), /unsupported platform/);

assert.strictEqual(
  install.checksumFor("abc123  soldr-v0.7.5-x86_64-unknown-linux-gnu.tar.gz\n", "soldr-v0.7.5-x86_64-unknown-linux-gnu.tar.gz"),
  "abc123",
);

console.log("npm package and PyPI version checks passed");
