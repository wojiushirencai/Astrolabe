'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { test } = require('node:test');

test('bench preserves failing worker status and fails the CI gate', {
  skip: process.platform === 'win32',
}, () => {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'astrolabe-bench-harness-'));
  try {
    let source = fs.readFileSync(path.join(__dirname, 'bench.sh'), 'utf8');
    // Replace only the compile step in a disposable copy. Never invoke cargo or
    // alter the real repository cache; the worker below is a tiny shell stub.
    const build = 'if ! cargo build --release -p astrolabe-core --example index_bench; then';
    assert.equal(source.split(build).length, 2, 'benchmark build step changed; update fixture');
    source = source.replace(build, 'if ! true; then');
    for (const directory of ['scripts', 'target/release/examples', 'corpus/go', 'crates/astrolabe-core/examples/baselines']) {
      fs.mkdirSync(path.join(temp, directory), { recursive: true });
    }
    fs.writeFileSync(path.join(temp, 'scripts/bench.sh'), source);
    fs.writeFileSync(path.join(temp, 'crates/astrolabe-core/examples/baselines/go.json'), '{}');
    fs.writeFileSync(path.join(temp, 'target/release/examples/index_bench'), '#!/bin/sh\nexit 42\n', { mode: 0o755 });
    const result = spawnSync('bash', [path.join(temp, 'scripts/bench.sh'), '--ci'], {
      encoding: 'utf8', timeout: 5_000,
    });
    assert.ifError(result.error);
    assert.equal(result.status, 1, result.stdout + result.stderr);
    assert.match(result.stderr, /index_bench exit 42;/);
    assert.doesNotMatch(result.stderr, /index_bench exit 0;/);
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
});
