#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const assert = require("assert");

const root = path.resolve(__dirname, "..");
const pkg = require(path.join(root, "package.json"));
const install = require(path.join(root, "scripts", "install.js"));
const zccacheContract = require(path.join(root, "scripts", "zccache-contract.js"));

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
  "contracts/zccache-integration-guardrails.v1.json",
  "contracts/zccache-runtime.v1.json",
  "scripts/install.js",
  "scripts/zccache-contract.js",
  "scripts/test-npm-package.js",
  "README.md",
  "LICENSE",
]);

const bin = fs.readFileSync(path.join(root, pkg.bin.soldr), "utf8");
assert(bin.startsWith("#!/usr/bin/env node"), "bin/soldr.js must have a node shebang");

// Linux: triple selection branches on libc. `platformTarget` accepts an
// explicit `libc` arg so tests don't depend on the runtime detector.
assert.strictEqual(
  install.platformTarget("linux", "x64", "gnu").triple,
  "x86_64-unknown-linux-gnu",
);
assert.strictEqual(
  install.platformTarget("linux", "x64", "musl").triple,
  "x86_64-unknown-linux-musl",
);
assert.strictEqual(
  install.platformTarget("linux", "arm64", "gnu").triple,
  "aarch64-unknown-linux-gnu",
);
assert.strictEqual(
  install.platformTarget("linux", "arm64", "musl").triple,
  "aarch64-unknown-linux-musl",
);
// Default (no libc arg) falls back to detectLibc; on most CI hosts that's
// gnu. We don't assert the triple here — just that the call resolves.
assert.ok(install.platformTarget("linux", "x64").triple.startsWith("x86_64-unknown-linux-"));
// An explicitly-unknown libc must take the runs-anywhere build, matching
// detectLibc's own unknown case rather than contradicting it.
assert.strictEqual(
  install.platformTarget("linux", "x64", null).triple,
  "x86_64-unknown-linux-musl",
);
assert.strictEqual(install.platformTarget("darwin", "x64").triple, "x86_64-apple-darwin");
assert.strictEqual(install.platformTarget("darwin", "arm64").triple, "aarch64-apple-darwin");
assert.strictEqual(install.platformTarget("win32", "x64").triple, "x86_64-pc-windows-msvc");
assert.strictEqual(install.platformTarget("win32", "arm64").triple, "aarch64-pc-windows-msvc");
assert.throws(() => install.platformTarget("freebsd", "x64"), /unsupported platform/);

// detectLibc must return null on non-Linux platforms so the platform key
// stays `<platform>-<arch>` rather than `<platform>-<arch>-gnu`.
assert.strictEqual(install.detectLibc("darwin"), null);
assert.strictEqual(install.detectLibc("win32"), null);
// On Linux it must always resolve to one of the two known families so
// `platformTarget` can build a well-formed key.
const linuxLibc = install.detectLibc("linux");
assert.ok(linuxLibc === "gnu" || linuxLibc === "musl", `unexpected libc: ${linuxLibc}`);

// detectLibc's branches, driven through the probe seam so they are
// exercised on every host rather than only on whichever libc CI runs.
const throwingProbe = () => {
  throw new Error("probe unavailable");
};
const NEW_ENOUGH = install.MIN_GLIBC_FOR_GNU;

// Numeric version comparison. A lexical compare ranks "2.9" above "2.39"
// and would wave through a host that cannot run the binary.
assert.strictEqual(install.compareVersions("2.9", "2.39"), -1);
assert.strictEqual(install.compareVersions("2.39", "2.9"), 1);
assert.strictEqual(install.compareVersions("2.39", "2.39"), 0);
assert.strictEqual(install.compareVersions("2.40", "2.39"), 1);
assert.strictEqual(install.compareVersions("2.2.5", "2.14"), -1);

// 1. A glibc new enough for the shipped artifact takes the gnu build.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({ glibcVersionRuntime: NEW_ENOUGH }),
    listLib: () => ["ld-linux-x86-64.so.2", "libc.so.6"],
  }),
  "gnu",
);

// 2. A glibc that is too old must NOT take the gnu build. This is the live
//    bug: Debian 12 reports 2.36, the published gnu binary requires
//    GLIBC_2.39, and it dies with "version `GLIBC_2.39' not found" while the
//    musl artifact from the same release runs fine.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({ glibcVersionRuntime: "2.36" }),
    listLib: () => ["ld-linux-x86-64.so.2", "libc.so.6"],
  }),
  "musl",
);

// 3. A musl SYSTEM wins over whatever Node was linked against. Node built
//    against glibc on alpine (`apk add nodejs-current`) reports a perfectly
//    good glibc version, so the filesystem probe has to be consulted FIRST --
//    otherwise this answers "gnu" and never looks at /lib at all.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({ glibcVersionRuntime: NEW_ENOUGH }),
    listLib: () => ["ld-musl-x86_64.so.1"],
  }),
  "musl",
);

// 4. Real musl Node omits the property entirely (verified on node:22-alpine:
//    hasOwnProperty is false, not present-and-empty).
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({}),
    listLib: () => ["ld-musl-x86_64.so.1"],
  }),
  "musl",
);

// 5. Both probes unavailable (heavily sandboxed container) -> musl, the only
//    artifact that runs without knowing anything about the host.
assert.strictEqual(
  install.detectLibc("linux", { readHeader: throwingProbe, listLib: throwingProbe }),
  "musl",
);

// 6. A glibc host whose version cannot be read is also unknown: /lib says
//    glibc but carries no version, so the floor cannot be confirmed.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: throwingProbe,
    listLib: () => ["ld-linux-x86-64.so.2", "libc.so.6"],
  }),
  "musl",
);

// The probe seam must not leak to other platforms: a non-Linux host still
// short-circuits to null before any probe runs.
assert.strictEqual(
  install.detectLibc("darwin", { readHeader: throwingProbe, listLib: throwingProbe }),
  null,
);

// MIN_GLIBC_FOR_GNU must track the ceiling release-auto.yml enforces on the
// gnu artifacts. If the release build is fixed to link a 2.17 baseline and
// that ceiling drops, the installer must follow it down -- otherwise every
// glibc host below 2.39 keeps being sent to musl long after gnu would work.
const releaseWorkflow = fs.readFileSync(
  path.join(root, ".github", "workflows", "release-auto.yml"),
  "utf8",
);
const ceilingMatch = releaseWorkflow.match(/--max-glibc\s+([0-9][0-9.]*)/);
assert(ceilingMatch, "release-auto.yml must pass --max-glibc to verify_glibc_baseline.py");
assert.strictEqual(
  install.MIN_GLIBC_FOR_GNU,
  ceilingMatch[1],
  `install.js MIN_GLIBC_FOR_GNU (${install.MIN_GLIBC_FOR_GNU}) must match the ` +
    `--max-glibc ceiling in release-auto.yml (${ceilingMatch[1]})`,
);

assert.strictEqual(
  install.checksumFor(
    "abc123  soldr-v0.7.29-x86_64-unknown-linux-gnu.tar.zst\n",
    "soldr-v0.7.29-x86_64-unknown-linux-gnu.tar.zst",
  ),
  "abc123",
);

assert.strictEqual(install.ARCHIVE_EXT, "tar.zst");
assert.deepStrictEqual(
  install.BUNDLED_BINARIES,
  zccacheContract.RELEASE_BUNDLED_BINARIES,
);
// zccache is embedded into soldr/soldr-daemon; no standalone zccache
// binaries are bundled or downloaded.
assert.deepStrictEqual(zccacheContract.ZCCACHE_BUNDLED_BINARIES, []);
assert.strictEqual(zccacheContract.CRGX_BUNDLED_BINARY, "crgx");
assert.strictEqual(zccacheContract.CARGO_CHEF_BUNDLED_BINARY, "cargo-chef");
assert.deepStrictEqual(
  zccacheContract.soldrDebugInfoEntries({
    soldr: {
      debug_info: [{ name: "soldr.pdb", sha256: "0".repeat(64), format: "pdb" }],
    },
  }),
  [{ name: "soldr.pdb", sha256: "0".repeat(64), format: "pdb" }],
);
assert.ok(
  fs.existsSync(path.join(root, "contracts", "zccache-integration-guardrails.v1.json")),
  "npm package must include the zccache integration guardrail contract",
);

// Every TARGETS entry must drop the `archive` field — the combined
// archive format is fixed (.tar.zst) so the per-target field is dead
// data. Catch a regression early if anyone re-adds it.
for (const [key, target] of Object.entries(install.TARGETS || {})) {
  assert.ok(
    typeof target.triple === "string" && target.triple.length > 0,
    `TARGETS[${key}].triple must be a non-empty string`,
  );
  assert.ok(
    typeof target.binary === "string" && target.binary.length > 0,
    `TARGETS[${key}].binary must be a non-empty string`,
  );
  assert.strictEqual(
    target.archive,
    undefined,
    `TARGETS[${key}].archive must be removed — archive format is fixed at .tar.zst`,
  );
}

// BUNDLED_BINARIES must include soldr, soldr-daemon, crgx, and
// cargo-chef. It must not require standalone zccache binaries.
assert.ok(
  Array.isArray(install.BUNDLED_BINARIES),
  "install.BUNDLED_BINARIES must be exported as an array",
);
for (const required of [
  "soldr",
  "soldr-daemon",
  "crgx",
  "cargo-chef",
]) {
  assert.ok(
    install.BUNDLED_BINARIES.includes(required),
    `BUNDLED_BINARIES must include "${required}" (got ${JSON.stringify(install.BUNDLED_BINARIES)})`,
  );
}

console.log("npm package and PyPI version checks passed");
