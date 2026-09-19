'use strict';

const fs = require('fs');
const os = require('os');

/**
 * Single source of truth for npm platform packages and GitHub Release artifacts.
 *
 * Release contract (consumed by the GitHub Releases pipeline):
 *   tag:        v${version}          e.g. v0.1.0
 *   unix:       astrolabe-<rustTarget>.tar.gz
 *   windows:    astrolabe-<rustTarget>.zip
 *   checksums:  SHA256SUMS           GNU coreutils format (`<hex>  <filename>`)
 *   archive root contains the binary named `astrolabe` or `astrolabe.exe`.
 */
const PLATFORMS = [
  {
    key: 'darwin-arm64',
    npmPackage: '@astrolabe/darwin-arm64',
    rustTarget: 'aarch64-apple-darwin',
    npmOs: 'darwin',
    npmCpu: 'arm64',
    npmLibc: null,
    archiveExt: '.tar.gz',
    binaryName: 'astrolabe',
  },
  {
    key: 'darwin-x64',
    npmPackage: '@astrolabe/darwin-x64',
    rustTarget: 'x86_64-apple-darwin',
    npmOs: 'darwin',
    npmCpu: 'x64',
    npmLibc: null,
    archiveExt: '.tar.gz',
    binaryName: 'astrolabe',
  },
  {
    key: 'linux-x64-gnu',
    npmPackage: '@astrolabe/linux-x64-gnu',
    rustTarget: 'x86_64-unknown-linux-gnu',
    npmOs: 'linux',
    npmCpu: 'x64',
    npmLibc: 'glibc',
    archiveExt: '.tar.gz',
    binaryName: 'astrolabe',
  },
  {
    key: 'linux-x64-musl',
    npmPackage: '@astrolabe/linux-x64-musl',
    rustTarget: 'x86_64-unknown-linux-musl',
    npmOs: 'linux',
    npmCpu: 'x64',
    npmLibc: 'musl',
    archiveExt: '.tar.gz',
    binaryName: 'astrolabe',
  },
  {
    key: 'linux-arm64-gnu',
    npmPackage: '@astrolabe/linux-arm64-gnu',
    rustTarget: 'aarch64-unknown-linux-gnu',
    npmOs: 'linux',
    npmCpu: 'arm64',
    npmLibc: 'glibc',
    archiveExt: '.tar.gz',
    binaryName: 'astrolabe',
  },
  {
    key: 'win32-x64-msvc',
    npmPackage: '@astrolabe/win32-x64-msvc',
    rustTarget: 'x86_64-pc-windows-msvc',
    npmOs: 'win32',
    npmCpu: 'x64',
    npmLibc: null,
    archiveExt: '.zip',
    binaryName: 'astrolabe.exe',
  },
];

function archiveName(platform) {
  return `astrolabe-${platform.rustTarget}${platform.archiveExt}`;
}

function isMuslLinux() {
  if (process.platform !== 'linux') return false;

  try {
    const ldd = fs.readFileSync('/usr/bin/ldd', 'utf8');
    if (ldd.includes('musl')) return true;
  } catch {
    // no /usr/bin/ldd
  }

  try {
    if (fs.existsSync('/etc/alpine-release')) return true;
  } catch {
    // ignore
  }

  try {
    if (typeof process.report?.getReport === 'function') {
      const report = process.report.getReport();
      if (report && typeof report === 'object') {
        const shared = report.sharedObjects || [];
        if (shared.some((entry) => /libc\.musl-/.test(entry))) return true;
        const glibc = report.header && report.header.glibcVersionRuntime;
        if (glibc) return false;
      }
    }
  } catch {
    // ignore
  }

  return false;
}

function detectPlatform() {
  const npmOs = process.platform;
  const npmCpu = os.arch();
  const npmLibc = npmOs === 'linux' ? (isMuslLinux() ? 'musl' : 'glibc') : null;

  const match = PLATFORMS.find((platform) => {
    if (platform.npmOs !== npmOs) return false;
    if (platform.npmCpu !== npmCpu) return false;
    if (platform.npmLibc && platform.npmLibc !== npmLibc) return false;
    return true;
  });

  if (!match) {
    const host = `${npmOs}-${npmCpu}${npmLibc ? `-${npmLibc}` : ''}`;
    const supported = PLATFORMS.map((platform) => platform.key).join(', ');
    throw new Error(
      `astrolabe has no prebuilt binary for ${host}. Supported platforms: ${supported}`,
    );
  }

  return match;
}

function platformByKey(key) {
  const match = PLATFORMS.find((platform) => platform.key === key);
  if (!match) {
    throw new Error(`unknown platform key: ${key}`);
  }
  return match;
}

function platformByRustTarget(rustTarget) {
  return PLATFORMS.find((platform) => platform.rustTarget === rustTarget) || null;
}

module.exports = {
  PLATFORMS,
  archiveName,
  detectPlatform,
  isMuslLinux,
  platformByKey,
  platformByRustTarget,
};
