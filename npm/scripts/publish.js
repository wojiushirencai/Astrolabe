#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');
const { PLATFORMS } = require('../packages/astrolabe/lib/platforms');

const NPM_ROOT = path.resolve(__dirname, '..');
const DIST = path.join(NPM_ROOT, 'dist');
const MAIN = path.join(NPM_ROOT, 'packages', 'astrolabe', 'package.json');
const MAX_ATTEMPTS = 4;

function parseArgs(argv) {
  const opts = {
    dryRun: false,
    otp: process.env.NPM_OTP || null,
    tag: 'latest',
    skipExisting: true,
    access: 'public',
    packFirst: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dry-run') opts.dryRun = true;
    else if (arg === '--otp') opts.otp = argv[++i];
    else if (arg === '--tag') opts.tag = argv[++i];
    else if (arg === '--no-skip-existing') opts.skipExisting = false;
    else if (arg === '--access') opts.access = argv[++i];
    else if (arg === '--pack') opts.packFirst = true;
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
  console.error(`Publish platform packages first, then the main astrolabe package.

Usage:
  node scripts/publish.js [--dry-run] [--otp code] [--tag latest] [--pack]

Idempotent: a version already on the registry is skipped (re-run after a
partial failure). Platform packages MUST succeed before the main package
is published — npm will otherwise fail to install optionalDependencies.

Order:
  1. @astrolabe/<platform>  (all six)
  2. the main package (name taken from packages/astrolabe/package.json)
`);
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function runNpm(args, cwd) {
  const result = spawnSync('npm', args, { cwd, encoding: 'utf8' });
  return {
    status: result.status,
    stdout: result.stdout || '',
    stderr: result.stderr || '',
    error: result.error,
  };
}

function isPublished(name, version) {
  const result = runNpm(['view', `${name}@${version}`, 'version', '--silent'], NPM_ROOT);
  if (result.status !== 0) return false;
  return result.stdout.trim() === version;
}

function registryDescription(name, version) {
  const result = runNpm(['view', `${name}@${version}`, 'description', '--silent'], NPM_ROOT);
  if (result.status !== 0) return null;
  const value = result.stdout.trim();
  return value === '' || value === 'undefined' ? null : value;
}

/**
 * The unscoped `astrolabe` name on npm is occupied by an unrelated 2014
 * Protractor helper (which also published 0.1.0). Without this guard a
 * foreign package under the same name@version makes `isPublished` return
 * true and the main package is silently "skipped" while the run still
 * exits 0. Compare the registry description with ours instead: a mismatch
 * means the registry version is not ours, so fail loudly.
 */
function assertRegistryVersionIsOurs(dir, name, version) {
  const local = readJson(path.join(dir, 'package.json'));
  const ours = (local.description || '').trim();
  const theirs = registryDescription(name, version);
  if (theirs === null || theirs !== ours) {
    throw new Error(
      `${name}@${version} already exists on the registry but was not published by this project ` +
        `(registry description: ${JSON.stringify(theirs)}). ` +
        'Rename the package (e.g. astrolabe-mcp) or pick an unused version.',
    );
  }
}

function isRetryable(output) {
  return /E429|E502|E503|ETIMEDOUT|ECONNRESET|network|socket hang up|429 Too Many|503 Service/i.test(
    output,
  );
}

function isAlreadyPublished(output) {
  return /cannot publish over the previously published|EPUBLISHCONFLICT|409 Conflict/i.test(
    output,
  );
}

function sleepSync(ms) {
  spawnSync(process.execPath, ['-e', `setTimeout(() => {}, ${ms})`], {
    stdio: 'ignore',
  });
}

function publishDir(dir, name, version, opts) {
  if (opts.skipExisting && isPublished(name, version)) {
    assertRegistryVersionIsOurs(dir, name, version);
    console.error(`skip ${name}@${version}: already on registry`);
    return 'skipped';
  }

  const args = ['publish', '--access', opts.access, '--tag', opts.tag];
  if (opts.dryRun) args.push('--dry-run');
  if (opts.otp) args.push('--otp', opts.otp);

  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt += 1) {
    console.error(`publish ${name}@${version} (attempt ${attempt}/${MAX_ATTEMPTS})`);
    const result = runNpm(args, dir);
    const output = `${result.stdout}\n${result.stderr}`;
    if (result.status === 0) {
      console.error(`published ${name}@${version}`);
      return 'published';
    }
    if (isAlreadyPublished(output)) {
      console.error(`skip ${name}@${version}: already published (conflict)`);
      return 'skipped';
    }
    if (attempt < MAX_ATTEMPTS && isRetryable(output)) {
      const wait = 2000 * 2 ** (attempt - 1);
      console.error(`retryable failure, waiting ${wait}ms:\n${output}`);
      sleepSync(wait);
      continue;
    }
    throw new Error(`npm publish failed for ${name}@${version}:\n${output}`);
  }
  throw new Error(`npm publish failed for ${name}@${version} after retries`);
}

function assertVersions(version) {
  const optional = readJson(MAIN).optionalDependencies;
  const expectedKeys = PLATFORMS.map((platform) => platform.npmPackage).sort();
  const actualKeys = Object.keys(optional).sort();
  if (actualKeys.join(',') !== expectedKeys.join(',')) {
    throw new Error(
      `optionalDependencies keys [${actualKeys}] do not match platform packages [${expectedKeys}]`,
    );
  }
  for (const platform of PLATFORMS) {
    const pkg = readJson(path.join(NPM_ROOT, 'packages', platform.key, 'package.json'));
    if (pkg.version !== version) {
      throw new Error(
        `version mismatch: ${pkg.name} is ${pkg.version}, main package is ${version}`,
      );
    }
    if (pkg.name !== platform.npmPackage) {
      throw new Error(
        `package name mismatch: packages/${platform.key} declares ${pkg.name}, expected ${platform.npmPackage}`,
      );
    }
    if (optional[pkg.name] !== version) {
      throw new Error(
        `optionalDependencies[${pkg.name}] is ${optional[pkg.name]}, expected ${version}`,
      );
    }
  }
}

function assertBinaries() {
  for (const platform of PLATFORMS) {
    const bin = path.join(NPM_ROOT, 'packages', platform.key, platform.binaryName);
    if (!fs.existsSync(bin)) {
      throw new Error(`missing ${bin}; run fill-binaries.js --artifacts-dir … --require-all`);
    }
    const size = fs.statSync(bin).size;
    if (size < 100_000) {
      throw new Error(`${bin} is only ${size} bytes; refusing to publish a stub`);
    }
  }
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  const version = readJson(MAIN).version;
  assertVersions(version);
  assertBinaries();

  if (opts.packFirst) {
    const pack = spawnSync(process.execPath, [path.join(__dirname, 'pack-all.js'), '--require-all'], {
      cwd: NPM_ROOT,
      stdio: 'inherit',
    });
    if (pack.status !== 0) {
      throw new Error('pack-all --require-all failed');
    }
    console.error(`tarballs in ${DIST} (publish still uses package directories)`);
  }

  const results = [];
  for (const platform of PLATFORMS) {
    const dir = path.join(NPM_ROOT, 'packages', platform.key);
    results.push({
      name: platform.npmPackage,
      status: publishDir(dir, platform.npmPackage, version, opts),
    });
  }

  const missing = [];
  if (!opts.dryRun) {
    for (const platform of PLATFORMS) {
      if (!isPublished(platform.npmPackage, version)) {
        missing.push(platform.npmPackage);
      }
    }
  }
  if (missing.length) {
    throw new Error(
      `refusing to publish main package; platform packages not on registry: ${missing.join(', ')}`,
    );
  }

  const mainName = readJson(MAIN).name;
  results.push({
    name: mainName,
    status: publishDir(path.join(NPM_ROOT, 'packages', 'astrolabe'), mainName, version, opts),
  });

  console.error('\npublish summary:');
  for (const row of results) {
    console.error(`  ${row.name}: ${row.status}`);
  }
}

try {
  main();
} catch (error) {
  console.error(`[publish] ${error.message}`);
  process.exit(1);
}
