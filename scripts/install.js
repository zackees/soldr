#!/usr/bin/env node
"use strict";

const childProcess = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const https = require("https");
const os = require("os");
const path = require("path");
const zccacheContract = require("./zccache-contract");

const PACKAGE_ROOT = path.resolve(__dirname, "..");
const PACKAGE_JSON = require(path.join(PACKAGE_ROOT, "package.json"));

// Every release ships a single .tar.zst per target that bundles soldr
// alongside its matching-target zccache trio (zccache, zccache-daemon,
// zccache-fp), same-target crgx, and same-target cargo-chef. One fetch
// installs everything, and `bin/soldr.js` wires SOLDR_ZCCACHE_LOCAL_DIR,
// SOLDR_CRGX_LOCAL_DIR, and SOLDR_CARGO_CHEF_LOCAL_DIR
// to the install dir so soldr's runtime resolver finds the sibling binaries
// without going through the managed-download path.
const ARCHIVE_EXT = zccacheContract.ARCHIVE_EXT;

const TARGETS = {
  "linux-x64-gnu": { triple: "x86_64-unknown-linux-gnu", binary: "soldr" },
  "linux-x64-musl": { triple: "x86_64-unknown-linux-musl", binary: "soldr" },
  "linux-arm64-gnu": { triple: "aarch64-unknown-linux-gnu", binary: "soldr" },
  "linux-arm64-musl": { triple: "aarch64-unknown-linux-musl", binary: "soldr" },
  "darwin-x64": { triple: "x86_64-apple-darwin", binary: "soldr" },
  "darwin-arm64": { triple: "aarch64-apple-darwin", binary: "soldr" },
  "win32-x64": { triple: "x86_64-pc-windows-msvc", binary: "soldr.exe" },
  "win32-arm64": { triple: "aarch64-pc-windows-msvc", binary: "soldr.exe" },
};

// Files we expect to find at the root of every extracted release
// archive. Names line up with what `release-auto.yml`'s
// `Fetch matched zccache release`, `Stage soldr binary`, and
// `Build crgx from pinned source`, and `Build cargo-chef from pinned
// source` steps drop into `dist/package/`
// before the tar.zst is built. `.exe` suffix is appended at install
// time based on `target.binary`.
const BUNDLED_BINARIES = zccacheContract.RELEASE_BUNDLED_BINARIES;

// Detect whether the running Linux uses musl or glibc. Three layered probes:
//   1. process.report.header.glibcVersionRuntime is the documented Node
//      surface for runtime glibc version — present on glibc, absent /
//      empty on musl. Same approach used by @swc/core, @napi-rs/*, etc.
//   2. Filesystem check for `/lib/ld-musl-*.so.1` covers cases where the
//      Node binary itself is glibc (e.g. someone running glibc Node on
//      alpine via apk add nodejs-current) but the system is musl — the
//      soldr binary we download must match the SYSTEM libc, not Node's.
//   3. Final fallback: assume glibc. The mismatch will surface
//      immediately at runtime as "soldr: not found" or
//      "soldr: error while loading shared libraries", which is louder
//      than silently downloading the wrong tarball.
function detectLibc(platform = process.platform) {
  if (platform !== "linux") {
    return null;
  }
  try {
    const header = process.report && process.report.getReport && process.report.getReport().header;
    if (header && typeof header.glibcVersionRuntime === "string" && header.glibcVersionRuntime.length > 0) {
      return "gnu";
    }
    if (header && Object.prototype.hasOwnProperty.call(header, "glibcVersionRuntime")) {
      // Field present but empty / null → Node was built against musl.
      return "musl";
    }
  } catch (err) {
    // process.report can throw on locked-down environments; fall through.
  }
  try {
    const entries = fs.readdirSync("/lib");
    if (entries.some((name) => /^ld-musl-.+\.so\.1$/.test(name))) {
      return "musl";
    }
  } catch (err) {
    // /lib may not be readable in heavily sandboxed containers; fall through.
  }
  return "gnu";
}

function platformTarget(platform = process.platform, arch = process.arch, libc = detectLibc(platform)) {
  const key =
    platform === "linux"
      ? `${platform}-${arch}-${libc || "gnu"}`
      : `${platform}-${arch}`;
  const target = TARGETS[key];
  if (!target) {
    throw new Error(`unsupported platform for soldr npm package: ${key}`);
  }
  return target;
}

function releaseBaseUrl(version) {
  const override = process.env.SOLDR_NPM_RELEASE_BASE_URL;
  if (override) {
    return override.replace(/\/+$/, "");
  }
  return `https://github.com/zackees/soldr/releases/download/v${version}`;
}

function download(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith("https:") ? https : http;
    const request = client.get(
      url,
      {
        headers: {
          "User-Agent": `soldr-npm/${PACKAGE_JSON.version}`,
        },
      },
      (response) => {
        if (
          response.statusCode >= 300 &&
          response.statusCode < 400 &&
          response.headers.location
        ) {
          response.resume();
          if (redirects >= 5) {
            reject(new Error(`too many redirects while downloading ${url}`));
            return;
          }
          resolve(download(new URL(response.headers.location, url).toString(), redirects + 1));
          return;
        }

        if (response.statusCode !== 200) {
          response.resume();
          reject(new Error(`download failed for ${url}: HTTP ${response.statusCode}`));
          return;
        }

        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => resolve(Buffer.concat(chunks)));
      },
    );
    request.on("error", reject);
  });
}

function checksumFor(checksumsText, filename) {
  for (const line of checksumsText.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const [hash, name] = trimmed.split(/\s+/, 2);
    if (name === filename) {
      return hash.toLowerCase();
    }
  }
  throw new Error(`checksum entry not found for ${filename}`);
}

function run(command, args, options = {}) {
  const result = childProcess.spawnSync(command, args, {
    stdio: "inherit",
    ...options,
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with exit code ${result.status}`);
  }
}

function extractArchive(archivePath, destination) {
  // GNU tar 1.31+ and bsdtar (default on macOS / Windows) both support
  // `--zstd` for zstandard. If a host's tar predates that flag, fall
  // back to `--use-compress-program=unzstd` which only needs the
  // `unzstd` CLI on PATH — installed alongside `zstd` on every modern
  // package manager. As a last resort, `zstd` decompresses to a temp
  // .tar and we extract that explicitly.
  const attempts = [
    ["tar", ["--zstd", "-xf", archivePath, "-C", destination]],
    ["tar", ["--use-compress-program=unzstd", "-xf", archivePath, "-C", destination]],
  ];
  for (const [cmd, args] of attempts) {
    const result = childProcess.spawnSync(cmd, args, { stdio: "inherit" });
    if (!result.error && result.status === 0) {
      return;
    }
    if (result.error && result.error.code === "ENOENT") {
      throw result.error;
    }
    // status != 0 → fall through to the next strategy.
  }
  // Last resort: decompress to a sibling .tar, then untar.
  const intermediate = `${archivePath}.tar`;
  run("zstd", ["-d", "-o", intermediate, archivePath]);
  run("tar", ["-xf", intermediate, "-C", destination]);
  fs.rmSync(intermediate, { force: true });
}

function findExtractedBinary(root, binaryName) {
  const entries = fs.readdirSync(root, { withFileTypes: true });
  for (const entry of entries) {
    const candidate = path.join(root, entry.name);
    if (entry.isFile() && entry.name === binaryName) {
      return candidate;
    }
    if (entry.isDirectory()) {
      const nested = findExtractedBinary(candidate, binaryName);
      if (nested) {
        return nested;
      }
    }
  }
  return null;
}

async function install() {
  if (process.env.SOLDR_NPM_SKIP_DOWNLOAD) {
    console.log("soldr: skipping native binary download because SOLDR_NPM_SKIP_DOWNLOAD is set");
    return;
  }

  const version = PACKAGE_JSON.version;
  const target = platformTarget();
  const filename = `soldr-v${version}-${target.triple}.${ARCHIVE_EXT}`;
  const baseUrl = releaseBaseUrl(version);
  const archiveUrl = `${baseUrl}/${filename}`;
  const checksumUrl = `${baseUrl}/soldr-v${version}-SHA256SUMS.txt`;
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "soldr-npm-"));

  try {
    console.log(`soldr: downloading ${archiveUrl}`);
    const [archive, checksums] = await Promise.all([
      download(archiveUrl),
      download(checksumUrl).then((buffer) => buffer.toString("utf8")),
    ]);

    const expected = checksumFor(checksums, filename);
    const actual = crypto.createHash("sha256").update(archive).digest("hex");
    if (actual !== expected) {
      throw new Error(`checksum mismatch for ${filename}: expected ${expected}, got ${actual}`);
    }

    const archivePath = path.join(tmp, filename);
    const extractDir = path.join(tmp, "extract");
    fs.writeFileSync(archivePath, archive);
    fs.mkdirSync(extractDir, { recursive: true });
    extractArchive(archivePath, extractDir);

    const nativeDir = path.join(PACKAGE_ROOT, "bin", "native");
    fs.rmSync(nativeDir, { recursive: true, force: true });
    fs.mkdirSync(nativeDir, { recursive: true });

    // Copy every bundled binary so soldr's runtime resolver can find
    // its sibling zccache via SOLDR_ZCCACHE_LOCAL_DIR, crgx via
    // SOLDR_CRGX_LOCAL_DIR, and cargo-chef via SOLDR_CARGO_CHEF_LOCAL_DIR.
    // `bin/soldr.js` wires these env vars before exec. The archive
    // layout is flat: all bundled binaries live at the archive root.
    const binaryExt = target.binary.endsWith(".exe") ? ".exe" : "";
    const manifestSrc = findExtractedBinary(extractDir, zccacheContract.MANIFEST_NAME);
    if (!manifestSrc) {
      throw new Error(`release archive ${filename} did not contain ${zccacheContract.MANIFEST_NAME}`);
    }
    const manifest = JSON.parse(fs.readFileSync(manifestSrc, "utf8"));
    zccacheContract.validateReleaseManifest(manifest, {
      soldrTarget: target.triple,
      platform: process.platform,
      findFile: (name) => {
        const filePath = findExtractedBinary(extractDir, name);
        if (!filePath) {
          throw new Error(`release archive ${filename} did not contain ${name}`);
        }
        return filePath;
      },
    });
    for (const baseName of BUNDLED_BINARIES) {
      const fileName = `${baseName}${binaryExt}`;
      const src = findExtractedBinary(extractDir, fileName);
      if (!src) {
        throw new Error(`release archive ${filename} did not contain ${fileName}`);
      }
      const dst = path.join(nativeDir, fileName);
      fs.copyFileSync(src, dst);
      if (process.platform !== "win32") {
        fs.chmodSync(dst, 0o755);
      }
    }
    for (const entry of zccacheContract.soldrDebugInfoEntries(manifest)) {
      const src = findExtractedBinary(extractDir, entry.name);
      if (!src) {
        throw new Error(`release archive ${filename} did not contain ${entry.name}`);
      }
      fs.copyFileSync(src, path.join(nativeDir, entry.name));
    }

    // Drop manifest.json alongside the binaries so downstream tooling
    // (and humans reading `bin/native/`) can introspect provenance —
    // soldr / zccache versions, target triples, build commit, sha256s.
    fs.copyFileSync(manifestSrc, path.join(nativeDir, zccacheContract.MANIFEST_NAME));

    console.log(
      `soldr: installed ${target.triple} (soldr + zccache trio + crgx + cargo-chef) into ${nativeDir}`,
    );
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

if (require.main === module) {
  install().catch((error) => {
    console.error(`soldr: npm install failed: ${error.message}`);
    process.exit(1);
  });
}

module.exports = {
  ARCHIVE_EXT,
  BUNDLED_BINARIES,
  TARGETS,
  checksumFor,
  detectLibc,
  platformTarget,
  releaseBaseUrl,
};
