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
// gnu BINARIES. If the release build is fixed to link a 2.17 baseline and that
// ceiling drops, the installer must follow it down -- otherwise every glibc
// host below 2.39 keeps being sent to musl long after gnu would work.
//
// Anchored on the script name rather than on any `--max-glibc`. release-auto
// now passes that flag twice: 2.39 to verify_glibc_baseline.py for the
// binaries, and 2.17 to verify_wheel_glibc.py for the wheel contents. A bare
// match takes whichever appears first, so this was correct only by the order
// the steps happen to sit in. Reorder them and the lockstep would demand the
// installer drop to 2.17, routing glibc 2.17-2.38 hosts to a binary that needs
// 2.39 -- the bug #2081 fixed, reintroduced by its own guard.
function glibcCeilingsFor(workflowText, scriptName) {
  const found = [];
  const lines = workflowText.split(/\r?\n/);
  for (let i = 0; i < lines.length; i += 1) {
    if (!lines[i].includes(scriptName)) {
      continue;
    }
    // The invocation may be split across continuation lines, so look at the
    // matching line and the few that follow it.
    const window = lines.slice(i, i + 4).join("\n");
    const match = window.match(/--max-glibc\s+([0-9][0-9.]*)/);
    if (match) {
      found.push(match[1]);
    }
  }
  return found;
}

function glibcCeilingFor(workflowText, scriptName) {
  const found = glibcCeilingsFor(workflowText, scriptName);
  if (found.length === 0) {
    return null;
  }
  // release-auto invokes verify_glibc_baseline.py twice -- once pre-staging on
  // the built binary, once post-staging across the whole bundle. Returning the
  // first would let the two drift apart with the installer silently following
  // only one of them, which is the same order-dependence this function was
  // written to remove.
  const distinct = [...new Set(found)];
  assert.strictEqual(
    distinct.length,
    1,
    `release-auto.yml passes conflicting --max-glibc values to ${scriptName}: ` +
      `${found.join(", ")}. They gate the same artifacts and must agree.`,
  );
  return distinct[0];
}

// Pin the anchoring itself: with the wheel invocation first, the binary
// ceiling must still resolve to the binary ceiling.
{
  const reordered = [
    "python3 .github/scripts/verify_wheel_glibc.py --max-glibc 2.17 dist/*.whl",
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 soldr",
  ].join("\n");
  assert.strictEqual(
    glibcCeilingFor(reordered, "verify_glibc_baseline.py"),
    "2.39",
    "the binary ceiling must not be confused with the wheel ceiling",
  );
  assert.strictEqual(
    glibcCeilingFor(reordered, "verify_wheel_glibc.py"),
    "2.17",
    "the wheel ceiling must resolve independently",
  );
  assert.strictEqual(glibcCeilingFor(reordered, "not_a_script.py"), null);

  // Two invocations of the same script that agree resolve to that value...
  const agreeing = [
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 soldr",
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 bundle",
  ].join("\n");
  assert.strictEqual(glibcCeilingFor(agreeing, "verify_glibc_baseline.py"), "2.39");

  // ...and two that disagree are a hard error rather than a silent pick.
  const conflicting = [
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.17 soldr",
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 bundle",
  ].join("\n");
  assert.throws(
    () => glibcCeilingFor(conflicting, "verify_glibc_baseline.py"),
    /conflicting --max-glibc/,
    "two ceilings for the same script must not be silently reconciled",
  );
}

const releaseWorkflow = fs.readFileSync(
  path.join(root, ".github", "workflows", "release-auto.yml"),
  "utf8",
);
const binaryCeiling = glibcCeilingFor(releaseWorkflow, "verify_glibc_baseline.py");
assert(
  binaryCeiling,
  "release-auto.yml must pass --max-glibc to verify_glibc_baseline.py",
);
assert.strictEqual(
  install.MIN_GLIBC_FOR_GNU,
  binaryCeiling,
  `install.js MIN_GLIBC_FOR_GNU (${install.MIN_GLIBC_FOR_GNU}) must match the ` +
    `--max-glibc ceiling verify_glibc_baseline.py enforces in release-auto.yml ` +
    `(${binaryCeiling})`,
);

assert.strictEqual(
  install.checksumFor(
    "abc123  soldr-v0.7.29-x86_64-unknown-linux-gnu.tar.zst\n",
    "soldr-v0.7.29-x86_64-unknown-linux-gnu.tar.zst",
  ),
  "abc123",
);

// The integrity check for everything this package installs
// (docs/TRUST_BOUNDARIES.md). `checksumFor` was covered but the comparison
// that uses it was inline in install() and untested, so deleting the
// mismatch branch would have disabled verification with every test green.
const crypto = require("crypto");
const ARCHIVE_BYTES = Buffer.from("pretend this is a tar.zst");
const ARCHIVE_NAME = "soldr-v9.9.9-x86_64-unknown-linux-musl.tar.zst";
const ARCHIVE_SHA = crypto.createHash("sha256").update(ARCHIVE_BYTES).digest("hex");
const SUMS = `${ARCHIVE_SHA}  ${ARCHIVE_NAME}
`;

// A matching digest returns it rather than throwing.
assert.strictEqual(
  install.verifyArchiveChecksum(ARCHIVE_BYTES, SUMS, ARCHIVE_NAME),
  ARCHIVE_SHA,
);

// A tampered archive must throw. This is the case that had no coverage.
assert.throws(
  () =>
    install.verifyArchiveChecksum(
      Buffer.concat([ARCHIVE_BYTES, Buffer.from("tampered")]),
      SUMS,
      ARCHIVE_NAME,
    ),
  /checksum mismatch/,
  "a tampered archive must be rejected",
);

// A digest for a DIFFERENT file must not be accepted for this one: the
// lookup is by exact filename, so a sums file listing only some other
// artifact fails closed rather than matching the first line it sees.
assert.throws(
  () =>
    install.verifyArchiveChecksum(
      ARCHIVE_BYTES,
      `${ARCHIVE_SHA}  some-other-artifact.tar.zst
`,
      ARCHIVE_NAME,
    ),
  /checksum entry not found/,
  "a sums file without this artifact must be rejected",
);

// An empty sums file is not a free pass.
assert.throws(
  () => install.verifyArchiveChecksum(ARCHIVE_BYTES, "", ARCHIVE_NAME),
  /checksum entry not found/,
  "an empty checksums file must be rejected",
);

// Real SHA256SUMS.txt files use two spaces and may carry CRLF; both must
// parse. Verified against the published v0.8.29 file, which is
// `<hash>  <name>` with 16 entries covering every release asset.
assert.strictEqual(
  install.verifyArchiveChecksum(
    ARCHIVE_BYTES,
    `deadbeef  unrelated.whl
${ARCHIVE_SHA}  ${ARCHIVE_NAME}
`,
    ARCHIVE_NAME,
  ),
  ARCHIVE_SHA,
);

// Digests are compared case-insensitively on the manifest side: an
// uppercase entry names the same bytes.
assert.strictEqual(
  install.verifyArchiveChecksum(
    ARCHIVE_BYTES,
    `${ARCHIVE_SHA.toUpperCase()}  ${ARCHIVE_NAME}
`,
    ARCHIVE_NAME,
  ),
  ARCHIVE_SHA,
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
