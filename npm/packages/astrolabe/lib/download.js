'use strict';

const crypto = require('crypto');
const fs = require('fs');
const http = require('http');
const https = require('https');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

const { archiveName } = require('./platforms');

const DEFAULT_HOST = 'https://gitee.com';
const DEFAULT_REPO = 'adam_1986/astrolabe';
const MAX_REDIRECTS = 10;
const REQUEST_TIMEOUT_MS = 60_000;

function cacheRoot() {
  if (process.env.ASTROLABE_CACHE_DIR) {
    return process.env.ASTROLABE_CACHE_DIR;
  }
  return path.join(os.homedir(), '.astrolabe', 'bin');
}

function releaseRepo() {
  return process.env.ASTROLABE_RELEASE_REPO || DEFAULT_REPO;
}

function releaseHost() {
  return (process.env.ASTROLABE_RELEASE_HOST || DEFAULT_HOST).replace(/\/$/, '');
}

/// Human-facing releases page for `version`, used in error messages.
function releasesTagUrl(version) {
  return `${releaseHost()}/${releaseRepo()}/releases/tag/v${version}`;
}

function releasesBaseUrl(version) {
  if (process.env.ASTROLABE_RELEASES_BASE_URL) {
    return process.env.ASTROLABE_RELEASES_BASE_URL.replace(/\/$/, '');
  }
  return `${releaseHost()}/${releaseRepo()}/releases/download/v${version}`;
}

function requestHeaders(url, isRedirect = false) {
  const headers = {
    Accept: 'application/octet-stream',
    'User-Agent': `astrolabe-npm/${require('../package.json').version}`,
  };
  if (isRedirect) {
    return headers;
  }
  let host;
  try {
    host = new URL(url).host.toLowerCase();
  } catch {
    return headers;
  }

  // Never attach credentials to external redirect targets (AWS S3, Aliyun OSS, CDNs)
  if (
    host.endsWith('.amazonaws.com') ||
    host.endsWith('.aliyuncs.com') ||
    host.endsWith('.githubusercontent.com')
  ) {
    return headers;
  }

  let relHost = '';
  try {
    const raw = process.env.ASTROLABE_RELEASES_BASE_URL || releaseHost();
    relHost = new URL(raw.includes('://') ? raw : `https://${raw}`).host.toLowerCase();
  } catch {
    relHost = '';
  }

  const isMatchingReleaseHost = relHost && host === relHost;
  const isDirectReleaseEndpoint =
    host === 'github.com' ||
    host === 'api.github.com' ||
    host === 'gitee.com' ||
    host === 'api.gitee.com';

  if (!isMatchingReleaseHost && !isDirectReleaseEndpoint) {
    return headers;
  }

  const token = releaseTokenFor(url);
  if (token) {
    headers.Authorization = `Bearer ${token}`;
  }
  return headers;
}

/// Only send a credential to the host it was issued for.
///
/// `GITHUB_TOKEN` is set in most CI environments, and releases now live on
/// Gitee: attaching it unconditionally would hand a GitHub credential to an
/// unrelated host on every install. `ASTROLABE_RELEASE_TOKEN` is explicit
/// and goes to whichever host the user pointed the download at.
function releaseTokenFor(url) {
  let host;
  try {
    host = new URL(url).host.toLowerCase();
  } catch {
    return undefined;
  }

  if (
    host.endsWith('.amazonaws.com') ||
    host.endsWith('.aliyuncs.com') ||
    host.endsWith('.githubusercontent.com')
  ) {
    return undefined;
  }

  if (process.env.ASTROLABE_RELEASE_TOKEN) {
    return process.env.ASTROLABE_RELEASE_TOKEN;
  }
  if (host === 'github.com' || host.endsWith('.github.com')) {
    return process.env.ASTROLABE_GITHUB_TOKEN || process.env.GITHUB_TOKEN;
  }
  return undefined;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function downloadToFile(url, dest, redirectsLeft = MAX_REDIRECTS, isRedirect = false) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith('https:') ? https : http;
    const req = client.get(url, { headers: requestHeaders(url, isRedirect) }, (res) => {
      const status = res.statusCode || 0;
      if (status >= 300 && status < 400 && res.headers.location) {
        res.resume();
        if (redirectsLeft <= 0) {
          reject(new Error(`too many redirects fetching ${url}`));
          return;
        }
        const next = new URL(res.headers.location, url).toString();
        downloadToFile(next, dest, redirectsLeft - 1, true).then(resolve, reject);
        return;
      }
      if (status !== 200) {
        res.resume();
        reject(new Error(`HTTP ${status} fetching ${url}`));
        return;
      }
      const out = fs.createWriteStream(dest);
      res.pipe(out);
      out.on('finish', () => out.close(() => resolve(dest)));
      out.on('error', reject);
    });
    req.setTimeout(REQUEST_TIMEOUT_MS, () => {
      req.destroy(new Error(`timed out fetching ${url}`));
    });
    req.on('error', reject);
  });
}

function sha256File(file) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash('sha256');
    const stream = fs.createReadStream(file);
    stream.on('data', (chunk) => hash.update(chunk));
    stream.on('end', () => resolve(hash.digest('hex')));
    stream.on('error', reject);
  });
}

function parseSha256Sums(text) {
  const map = new Map();
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;

    let match = line.match(/^([0-9a-fA-F]{64})\s+\*?(.+)$/);
    if (match) {
      map.set(path.basename(match[2].replace(/^\.\//, '')), match[1].toLowerCase());
      continue;
    }
    match = line.match(/^SHA256 \((.+)\) = ([0-9a-fA-F]{64})$/);
    if (match) {
      map.set(path.basename(match[1]), match[2].toLowerCase());
    }
  }
  return map;
}

function extractArchive(archivePath, destDir) {
  fs.mkdirSync(destDir, { recursive: true });
  const result = spawnSync('tar', ['-xf', archivePath, '-C', destDir], {
    encoding: 'utf8',
  });
  if (result.error || result.status !== 0) {
    if (process.platform === 'win32' && archivePath.endsWith('.zip')) {
      // Pass paths via env vars so PowerShell never interpolates them inside
      // a double-quoted -Command string (paths can contain `$`, `` ` ``, `"`).
      const psResult = spawnSync(
        'powershell.exe',
        [
          '-NoProfile',
          '-NonInteractive',
          '-Command',
          'Expand-Archive -LiteralPath $env:ASTROLABE_ZIP_SRC -DestinationPath $env:ASTROLABE_ZIP_DST -Force',
        ],
        {
          encoding: 'utf8',
          env: {
            ...process.env,
            ASTROLABE_ZIP_SRC: archivePath,
            ASTROLABE_ZIP_DST: destDir,
          },
        },
      );
      if (psResult.error) {
        throw new Error(`failed to extract ${archivePath}: ${psResult.error.message}`);
      }
      if (psResult.status !== 0) {
        throw new Error(
          `failed to extract ${archivePath}: ${psResult.stderr || psResult.stdout || `exit ${psResult.status}`}`,
        );
      }
      return;
    }
    if (result.error) {
      throw new Error(`failed to extract ${archivePath}: ${result.error.message}`);
    }
    throw new Error(
      `failed to extract ${archivePath}: ${result.stderr || result.stdout || `exit ${result.status}`}`,
    );
  }
}

function findBinary(dir, binaryName) {
  const direct = path.join(dir, binaryName);
  if (fs.existsSync(direct) && fs.statSync(direct).isFile()) {
    return direct;
  }

  const stack = [dir];
  while (stack.length) {
    const current = stack.pop();
    let entries;
    try {
      entries = fs.readdirSync(current, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const entry of entries) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) {
        stack.push(full);
      } else if (entry.isFile() && (entry.name === binaryName || entry.name === 'astrolabe' || entry.name === 'astrolabe.exe')) {
        return full;
      }
    }
  }
  throw new Error(`archive did not contain ${binaryName}`);
}

function ensureExecutable(file) {
  if (process.platform === 'win32') return;
  const mode = fs.statSync(file).mode;
  if ((mode & 0o111) === 0) {
    fs.chmodSync(file, mode | 0o755);
  }
}

async function withLock(lockDir, fn) {
  fs.mkdirSync(lockDir, { recursive: true });
  const lockPath = path.join(lockDir, 'download.lock');
  const started = Date.now();
  for (;;) {
    try {
      const fd = fs.openSync(lockPath, 'wx');
      try {
        return await fn();
      } finally {
        fs.closeSync(fd);
        try {
          fs.unlinkSync(lockPath);
        } catch {
          // ignore
        }
      }
    } catch (error) {
      if (error.code !== 'EEXIST') throw error;
      if (Date.now() - started > 120_000) {
        throw new Error('timed out waiting for astrolabe binary download lock');
      }
      await sleep(200);
    }
  }
}

async function downloadRelease(platform, version) {
  const destDir = path.join(cacheRoot(), version, platform.key);
  const destBin = path.join(destDir, platform.binaryName);
  if (fs.existsSync(destBin)) {
    ensureExecutable(destBin);
    return destBin;
  }

  return withLock(destDir, async () => {
    if (fs.existsSync(destBin)) {
      ensureExecutable(destBin);
      return destBin;
    }

    const base = releasesBaseUrl(version);
    const archive = archiveName(platform);
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'astrolabe-dl-'));
    const sumsPath = path.join(tmpDir, 'SHA256SUMS');
    const archivePath = path.join(tmpDir, archive);
    const extractDir = path.join(tmpDir, 'extract');

    try {
      await downloadToFile(`${base}/SHA256SUMS`, sumsPath);
      const sums = parseSha256Sums(fs.readFileSync(sumsPath, 'utf8'));
      const expected = sums.get(archive);
      if (!expected) {
        throw new Error(`SHA256SUMS has no entry for ${archive}`);
      }

      await downloadToFile(`${base}/${archive}`, archivePath);
      const actual = await sha256File(archivePath);
      if (actual !== expected) {
        throw new Error(
          `SHA256 mismatch for ${archive}: expected ${expected}, got ${actual}`,
        );
      }

      extractArchive(archivePath, extractDir);
      const extracted = findBinary(extractDir, platform.binaryName);
      fs.copyFileSync(extracted, destBin);
      ensureExecutable(destBin);
      return destBin;
    } finally {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    }
  });
}

module.exports = {
  cacheRoot,
  downloadRelease,
  releasesBaseUrl,
  releasesTagUrl,
  releaseTokenFor,
  ensureExecutable,
  parseSha256Sums,
  sha256File,
};
