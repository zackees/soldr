#!/usr/bin/env node
"use strict";

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

const PACKAGE_ROOT = path.resolve(__dirname, "..");
const CONTRACT_PATH = path.join(PACKAGE_ROOT, "contracts", "zccache-runtime.v1.json");
const CONTRACT = JSON.parse(fs.readFileSync(CONTRACT_PATH, "utf8"));

const ARCHIVE_EXT = CONTRACT.release_archive.extension;
const MANIFEST_NAME = CONTRACT.release_archive.manifest_name;
const MANIFEST_MIN_SCHEMA_VERSION = CONTRACT.release_archive.manifest_min_schema_version;
const RELEASE_BUNDLED_BINARIES = Object.freeze([...CONTRACT.release_archive.required_binaries]);
const ZCCACHE_BUNDLED_BINARIES = Object.freeze([...CONTRACT.zccache.required_binaries]);
const CRGX_BUNDLED_BINARY = CONTRACT.crgx.required_binaries[0];
const CARGO_CHEF_BUNDLED_BINARY = CONTRACT.cargo_chef.required_binaries[0];

function binaryName(baseName, platform = process.platform) {
  return `${baseName}${platform === "win32" ? ".exe" : ""}`;
}

function releaseBinaryNames(platform = process.platform) {
  return RELEASE_BUNDLED_BINARIES.map((baseName) => binaryName(baseName, platform));
}

function zccacheTargetForSoldrTarget(soldrTarget) {
  if (!soldrTarget.includes("-unknown-linux-")) {
    return soldrTarget;
  }
  const arch = soldrTarget.split("-unknown-linux-", 1)[0];
  return `${arch}-unknown-linux-${CONTRACT.release_archive.linux_zccache_target_libc}`;
}

function sha256File(filePath) {
  return crypto.createHash("sha256").update(fs.readFileSync(filePath)).digest("hex");
}

function collectManifestBinaries(manifest) {
  const binaries = new Map();
  if (manifest.soldr && manifest.soldr.binary && manifest.soldr.sha256) {
    binaries.set(manifest.soldr.binary, manifest.soldr.sha256);
  }
  const zccacheBinaries = (manifest.zccache && manifest.zccache.binaries) || [];
  for (const entry of zccacheBinaries) {
    if (entry && entry.name && entry.sha256) {
      binaries.set(entry.name, entry.sha256);
    }
  }
  if (manifest.crgx && manifest.crgx.binary && manifest.crgx.sha256) {
    binaries.set(manifest.crgx.binary, manifest.crgx.sha256);
  }
  if (manifest.cargo_chef && manifest.cargo_chef.binary && manifest.cargo_chef.sha256) {
    binaries.set(manifest.cargo_chef.binary, manifest.cargo_chef.sha256);
  }
  return binaries;
}

function validateReleaseManifest(manifest, options) {
  const { soldrTarget, platform = process.platform, findFile } = options;
  if (!Number.isInteger(manifest.schema_version) || manifest.schema_version < MANIFEST_MIN_SCHEMA_VERSION) {
    throw new Error(`release manifest schema_version must be >= ${MANIFEST_MIN_SCHEMA_VERSION}`);
  }
  if (!manifest.archive || manifest.archive.format !== ARCHIVE_EXT) {
    throw new Error(`release manifest archive.format must be ${ARCHIVE_EXT}`);
  }
  if (!manifest.soldr || manifest.soldr.target !== soldrTarget) {
    throw new Error(`release manifest soldr.target must be ${soldrTarget}`);
  }
  const expectedZccacheTarget = zccacheTargetForSoldrTarget(soldrTarget);
  if (!manifest.zccache || manifest.zccache.target !== expectedZccacheTarget) {
    throw new Error(`release manifest zccache.target must be ${expectedZccacheTarget}`);
  }
  if (!manifest.crgx || manifest.crgx.target !== soldrTarget) {
    throw new Error(`release manifest crgx.target must be ${soldrTarget}`);
  }
  if (!manifest.cargo_chef || manifest.cargo_chef.target !== soldrTarget) {
    throw new Error(`release manifest cargo_chef.target must be ${soldrTarget}`);
  }

  const expectedNames = releaseBinaryNames(platform);
  const manifestBinaries = collectManifestBinaries(manifest);
  for (const name of expectedNames) {
    if (!manifestBinaries.has(name)) {
      throw new Error(`release manifest is missing bundled binary record for ${name}`);
    }
    const expectedSha = String(manifestBinaries.get(name)).toLowerCase();
    if (!/^[0-9a-f]{64}$/.test(expectedSha)) {
      throw new Error(`release manifest sha256 for ${name} is not lowercase hex`);
    }
    const filePath = findFile(name);
    const actualSha = sha256File(filePath);
    if (actualSha !== expectedSha) {
      throw new Error(`release manifest sha256 mismatch for ${name}: expected ${expectedSha}, got ${actualSha}`);
    }
  }
}

module.exports = {
  ARCHIVE_EXT,
  CONTRACT,
  CONTRACT_PATH,
  CARGO_CHEF_BUNDLED_BINARY,
  CRGX_BUNDLED_BINARY,
  MANIFEST_NAME,
  RELEASE_BUNDLED_BINARIES,
  ZCCACHE_BUNDLED_BINARIES,
  binaryName,
  releaseBinaryNames,
  validateReleaseManifest,
  zccacheTargetForSoldrTarget,
};
