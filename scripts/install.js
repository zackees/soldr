#!/usr/bin/env node
"use strict";

const childProcess = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const https = require("https");
const os = require("os");
const path = require("path");

const PACKAGE_ROOT = path.resolve(__dirname, "..");
const PACKAGE_JSON = require(path.join(PACKAGE_ROOT, "package.json"));

const TARGETS = {
  "linux-x64": {
    triple: "x86_64-unknown-linux-gnu",
    archive: "tar.gz",
    binary: "soldr",
  },
  "linux-arm64": {
    triple: "aarch64-unknown-linux-gnu",
    archive: "tar.gz",
    binary: "soldr",
  },
  "darwin-x64": {
    triple: "x86_64-apple-darwin",
    archive: "tar.gz",
    binary: "soldr",
  },
  "darwin-arm64": {
    triple: "aarch64-apple-darwin",
    archive: "tar.gz",
    binary: "soldr",
  },
  "win32-x64": {
    triple: "x86_64-pc-windows-msvc",
    archive: "zip",
    binary: "soldr.exe",
  },
  "win32-arm64": {
    triple: "aarch64-pc-windows-msvc",
    archive: "zip",
    binary: "soldr.exe",
  },
};

function platformTarget(platform = process.platform, arch = process.arch) {
  const target = TARGETS[`${platform}-${arch}`];
  if (!target) {
    throw new Error(`unsupported platform for soldr npm package: ${platform}-${arch}`);
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

function extractArchive(archivePath, archiveType, destination) {
  if (archiveType === "tar.gz") {
    run("tar", ["-xzf", archivePath, "-C", destination]);
    return;
  }

  if (archiveType === "zip" && process.platform === "win32") {
    run("powershell", [
      "-NoProfile",
      "-NonInteractive",
      "-ExecutionPolicy",
      "Bypass",
      "-Command",
      "Expand-Archive -LiteralPath $env:SOLDR_NPM_ARCHIVE -DestinationPath $env:SOLDR_NPM_EXTRACT -Force",
    ], {
      env: {
        ...process.env,
        SOLDR_NPM_ARCHIVE: archivePath,
        SOLDR_NPM_EXTRACT: destination,
      },
    });
    return;
  }

  run("tar", ["-xf", archivePath, "-C", destination]);
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
  const filename = `soldr-v${version}-${target.triple}.${target.archive}`;
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
    extractArchive(archivePath, target.archive, extractDir);

    const extracted = findExtractedBinary(extractDir, target.binary);
    if (!extracted) {
      throw new Error(`release archive did not contain ${target.binary}`);
    }

    const nativeDir = path.join(PACKAGE_ROOT, "bin", "native");
    fs.rmSync(nativeDir, { recursive: true, force: true });
    fs.mkdirSync(nativeDir, { recursive: true });

    const destination = path.join(nativeDir, target.binary);
    fs.copyFileSync(extracted, destination);
    if (process.platform !== "win32") {
      fs.chmodSync(destination, 0o755);
    }

    console.log(`soldr: installed ${target.triple} binary`);
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
  checksumFor,
  platformTarget,
  releaseBaseUrl,
};
