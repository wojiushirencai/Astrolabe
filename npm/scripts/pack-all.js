#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');
const { PLATFORMS } = require('../packages/astrolabe/lib/platforms');

const NPM_ROOT = path.resolve(__dirname, '..');
const DIST = path.join(NPM_ROOT, 'dist');

function parseArgs(argv) {
  const opts = { requireAll: false };
  for (const arg of argv) {
    if (arg === '--require-all') opts.requireAll = true;
    else if (arg === '--help' || arg === '-h') {
      console.error('Usage: node scripts/pack-all.js [--require-all]');
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return opts;
}

function run(cmd, args, cwd) {
  const result = spawnSync(cmd, args, { cwd, encoding: 'utf8' });
  if (result.status !== 0) {
    throw new Error(
      `${cmd} ${args.join(' ')} failed in ${cwd}:\n${result.stderr || result.stdout}`,
    );
  }
  return result.stdout.trim();
}

function binaryPath(platform) {
  return path.join(NPM_ROOT, 'packages', platform.key, platform.binaryName);
}

function packOne(dir, label) {
  const output = run('npm', ['pack', '--pack-destination', DIST], dir);
  const line = output.split(/\r?\n/).filter(Boolean).pop();
  const tarball = path.join(DIST, line);
  if (!fs.existsSync(tarball)) {
    throw new Error(`npm pack did not produce ${tarball} (output: ${output})`);
  }
  const listing = run('tar', ['-tvf', tarball], NPM_ROOT);
  console.error(`\n=== packed ${label} -> ${path.basename(tarball)} ===`);
  console.error(listing);
  return tarball;
}

function main() {
  const opts = parseArgs(process.argv.slice(2));
  fs.mkdirSync(DIST, { recursive: true });
  const packed = [];

  for (const platform of PLATFORMS) {
    const bin = binaryPath(platform);
    if (!fs.existsSync(bin)) {
      if (opts.requireAll) {
        throw new Error(`missing ${bin}; run fill-binaries.js first`);
      }
      console.error(`skip pack ${platform.npmPackage}: no binary at ${bin}`);
      continue;
    }
    packed.push(
      packOne(path.join(NPM_ROOT, 'packages', platform.key), platform.npmPackage),
    );
  }

  packed.push(packOne(path.join(NPM_ROOT, 'packages', 'astrolabe'), 'astrolabe'));
  console.error(`\npacked ${packed.length} tarball(s) into ${DIST}`);
  for (const file of packed) console.error(`  ${file}`);
}

try {
  main();
} catch (error) {
  console.error(`[pack-all] ${error.message}`);
  process.exit(1);
}
