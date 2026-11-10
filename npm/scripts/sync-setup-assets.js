#!/usr/bin/env node
'use strict';

/**
 * Keep the npm package's Cursor L1 template byte-identical to the Rust
 * context source. The file is copied in before `npm pack` and removed after,
 * so the published tarball is self-contained without keeping a second,
 * hand-maintained copy under version control.
 *
 * Usage:
 *   node npm/scripts/sync-setup-assets.js          # copy source -> package
 *   node npm/scripts/sync-setup-assets.js --clean  # remove the generated copy
 */

const fs = require('fs');
const path = require('path');

const NPM_ROOT = path.resolve(__dirname, '..');
const REPO_ROOT = path.resolve(NPM_ROOT, '..');
const SOURCE = path.join(REPO_ROOT, 'crates', 'astrolabe-mcp', 'contexts', 'cursor-l1.md');
const TARGET_DIR = path.join(NPM_ROOT, 'packages', 'astrolabe', 'lib', 'setup', 'templates');
const TARGET = path.join(TARGET_DIR, 'cursor-l1.md');

function clean() {
  fs.rmSync(TARGET, { force: true });
  try {
    if (fs.readdirSync(TARGET_DIR).length === 0) fs.rmdirSync(TARGET_DIR);
  } catch {
    // directory already gone
  }
}

function copy() {
  if (!fs.existsSync(SOURCE)) {
    throw new Error(`missing shared Cursor L1 source: ${SOURCE}`);
  }
  fs.mkdirSync(TARGET_DIR, { recursive: true });
  fs.copyFileSync(SOURCE, TARGET);
  console.error(`[sync-setup-assets] ${SOURCE} -> ${TARGET}`);
}

try {
  if (process.argv.includes('--clean')) clean();
  else copy();
} catch (error) {
  console.error(`[sync-setup-assets] ${error.message || error}`);
  process.exit(1);
}
