#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const {
  PLATFORMS,
  archiveName,
  detectPlatform,
  platformByKey,
} = require('../packages/astrolabe/lib/platforms');

const NPM_ROOT = path.resolve(__dirname, '..');
const REPO_ROOT = path.resolve(NPM_ROOT, '..');

function parseArgs(argv) {
  const opts = {
    binary: null,
    platform: null,
    artifactsDir: null,
    requireAll: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--binary') opts.binary = argv[++i];
    else if (arg === '--platform') opts.platform = argv[++i];
    else if (arg === '--artifacts-dir') opts.artifactsDir = argv[++i];
    else if (arg === '--require-all') opts.requireAll = true;
    else if (arg === '--help' || arg === '-h') {
      printHelp();
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return opts;
}

function printHelp() {
  console.error(`Fill platform-package directories with native binaries.

Usage:
  node scripts/fill-binaries.js --binary <path> [--platform darwin-arm64]
  node scripts/fill-binaries.js --artifacts-dir <dir>
  node scripts/fill-binaries.js            # default: repo target/release/astrolabe

Artifact layout (any of these, mixed):
  <dir>/astrolabe-<rustTarget>.tar.gz|.zip
  <dir>/<rustTarget>/astrolabe[.exe]
  <dir>/<rustTarget>/release/astrolabe[.exe]
  <dir>/astrolabe-<rustTarget>/astrolabe[.exe]

If SHA256SUMS is present next to archives, hashes are verified before extract.
`);
}

function packageDir(platform) {
  return path.join(NPM_ROOT, 'packages', platform.key);
}

function destBinary(platform) {
  return path.join(packageDir(platform), platform.binaryName);
}

function copyBinary(src, platform) {
  const dest = destBinary(platform);
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(src, dest);
  if (process.platform !== 'win32') {
    fs.chmodSync(dest, 0o755);
  }
  const stat = fs.statSync(dest);
  console.error(
    `filled ${platform.npmPackage} <- ${src} (${stat.size} bytes, mode ${(stat.mode & 0o777).toString(8)})`,
  );
  return dest;
}

function extractArchive(archivePath, destDir) {
  fs.mkdirSync(destDir, { recursive: true });
  const result = spawnSync('tar', ['-xf', archivePath, '-C', destDir], { encoding: 'utf8' });
  if (result.status !== 0) {
    throw new Error(
      `tar -xf ${archivePath} failed: ${result.stderr || result.stdout || result.error}`,
    );
  }
}

function findBinaryInTree(dir, binaryName) {
  const stack = [dir];
  while (stack.length) {
    const current = stack.pop();
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) stack.push(full);
      else if (entry.isFile() && (entry.name === binaryName || entry.name === 'astrolabe' || entry.name === 'astrolabe.exe')) {
        return full;
      }
    }
  }
  return null;
}

function sha256FileSync(file) {
  const crypto = require('crypto');
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

function verifyArchive(archivePath, artifactsDir) {
  const sumsPath = path.join(artifactsDir, 'SHA256SUMS');
  if (!fs.existsSync(sumsPath)) return;
  const { parseSha256Sums } = require('../packages/astrolabe/lib/download');
  const sums = parseSha256Sums(fs.readFileSync(sumsPath, 'utf8'));
  const expected = sums.get(path.basename(archivePath));
  if (!expected) {
    throw new Error(`SHA256SUMS has no entry for ${path.basename(archivePath)}`);
  }
  const actual = sha256FileSync(archivePath);
  if (actual !== expected) {
    throw new Error(
      `SHA256 mismatch for ${path.basename(archivePath)}: expected ${expected}, got ${actual}`,
    );
  }
}

function candidateUnpacked(artifactsDir, platform) {
  const names = [
    path.join(artifactsDir, platform.rustTarget, platform.binaryName),
    path.join(artifactsDir, platform.rustTarget, 'release', platform.binaryName),
    path.join(artifactsDir, `astrolabe-${platform.rustTarget}`, platform.binaryName),
    path.join(artifactsDir, platform.key, platform.binaryName),
  ];
  return names.find((candidate) => fs.existsSync(candidate)) || null;
}

function fillFromArtifacts(artifactsDir, requireAll) {
  let filled = 0;
  const tmpRoot = fs.mkdtempSync(path.join(require('os').tmpdir(), 'astrolabe-fill-'));
  try {
    for (const platform of PLATFORMS) {
      const unpacked = candidateUnpacked(artifactsDir, platform);
      if (unpacked) {
        copyBinary(unpacked, platform);
        filled += 1;
        continue;
      }
      const archivePath = path.join(artifactsDir, archiveName(platform));
      if (!fs.existsSync(archivePath)) {
        if (requireAll) {
          throw new Error(`missing artifact for ${platform.key}: ${archivePath}`);
        }
        console.error(`skip ${platform.key}: no artifact in ${artifactsDir}`);
        continue;
      }
      verifyArchive(archivePath, artifactsDir);
      const extractDir = path.join(tmpRoot, platform.key);
      extractArchive(archivePath, extractDir);
      const found = findBinaryInTree(extractDir, platform.binaryName);
      if (!found) {
        throw new Error(`${archivePath} did not contain ${platform.binaryName}`);
      }
      copyBinary(found, platform);
      filled += 1;
    }
  } finally {
    fs.rmSync(tmpRoot, { recursive: true, force: true });
  }
  if (filled === 0) {
    throw new Error(`no binaries found under ${artifactsDir}`);
  }
}

function defaultReleaseBinary() {
  const unix = path.join(REPO_ROOT, 'target', 'release', 'astrolabe');
  const win = path.join(REPO_ROOT, 'target', 'release', 'astrolabe.exe');
  if (fs.existsSync(unix)) return unix;
  if (fs.existsSync(win)) return win;
  return null;
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.artifactsDir) {
    fillFromArtifacts(path.resolve(opts.artifactsDir), opts.requireAll);
    return;
  }

  const binary = path.resolve(opts.binary || defaultReleaseBinary() || '');
  if (!opts.binary && !defaultReleaseBinary()) {
    throw new Error(
      'no --binary / --artifacts-dir given, and target/release/astrolabe is missing. Build with cargo build --release first.',
    );
  }
  if (!fs.existsSync(binary)) {
    throw new Error(`binary not found: ${binary}`);
  }

  const platform = opts.platform ? platformByKey(opts.platform) : detectPlatform();
  copyBinary(binary, platform);
}

try {
  main();
} catch (error) {
  console.error(`[fill-binaries] ${error.message}`);
  process.exit(1);
}
