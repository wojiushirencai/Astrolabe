'use strict';

const assert = require('node:assert/strict');
const { test } = require('node:test');
const { spawn } = require('node:child_process');
const path = require('node:path');
const { EventEmitter } = require('node:events');
const { PassThrough } = require('node:stream');
const {
  RpcClient, parseArgs, waitExit, toolsList, badRootExit, signalForward,
} = require('./verify-install');

// A real child with no Rust build, external service or installed binary needed.
const FIXTURE = `
const readline = require('readline');
const mode = process.argv[1];
let ready = false;
const lines = readline.createInterface({ input: process.stdin });
const started = Date.now();
const send = (value) => process.stdout.write(JSON.stringify(value) + '\\n');
process.on('SIGTERM', () => {
  if (mode === 'term143') process.exit(143);
  if (mode === 'ignoreterm') return;
  process.exit(ready ? 0 : 42);
});
if (mode === 'exit') process.exit(7);
if (mode === 'eof') process.stdout.end();
if (mode === 'missing') {
  process.stderr.write('missing directory\\n');
  process.exit(2);
}
const requests = [];
lines.on('line', (line) => {
  if (mode === 'silent' || mode === 'eof') return;
  if (mode === 'invalid') { process.stdout.write('not JSON\\n'); return; }
  if (mode === 'scalar') { send(null); return; }
  const request = JSON.parse(line);
  if (mode === 'reverse') {
    requests.push(request);
    if (requests.length === 2) {
      send({ jsonrpc: '2.0', method: 'notifications/progress', params: {} });
      for (const r of requests.reverse()) send({ jsonrpc: '2.0', id: r.id, result: { method: r.method } });
    }
    return;
  }
  if (mode === 'rpcerror') {
    send({ jsonrpc: '2.0', id: request.id, error: { code: -32603, message: 'fixture failure' } });
    return;
  }
  let result;
  if (request.method === 'tools/list') {
    result = { tools: ['find_refs', 'find_symbol', 'get_diag', 'get_lang', 'goto_def', 'resolve_contx', 'search_code'].map(name => ({ name })) };
  } else {
    ready = mode !== 'building' && (mode !== 'delayed' || Date.now() - started >= 550);
    result = { content: [{ type: 'text', text: ready ? 'index_root: fixture; python' : 'Index is building in the background' }] };
    if (!ready || mode === 'toolerror') result.isError = true;
  }
  send({ jsonrpc: '2.0', method: 'notifications/progress', params: {} });
  send({ jsonrpc: '2.0', id: request.id, result });
});
lines.on('close', () => process.exit(mode === 'eofbad' ? 9 : 0));
`;

function fixtureSpawner(mode, shim = false) {
  const children = [];
  const spawnImpl = (_bin, _args, options) => {
    let args = ['-e', FIXTURE, mode];
    if (shim) {
      const run = path.resolve(__dirname, '../packages/astrolabe/lib/run.js');
      args = ['-e', `require(${JSON.stringify(run)}).runBinary(process.execPath, ${JSON.stringify(args)})`];
    }
    const child = spawn(process.execPath, args, options);
    children.push(child);
    return child;
  };
  return { children, spawnImpl };
}

function assertReaped(children) {
  for (const child of children) {
    assert.ok(child.exitCode !== null || child.signalCode !== null, `child ${child.pid} was not reaped`);
  }
}

test('arguments reject missing and invalid timeout values', () => {
  assert.equal(parseArgs(['--bin', 'astro', '--timeout-ms', '1']).timeoutMs, 1);
  for (const value of ['0', '-1', 'NaN', 'Infinity', '1.5', undefined]) {
    const args = ['--bin', 'astro', '--timeout-ms'];
    if (value !== undefined) args.push(value);
    assert.throws(() => parseArgs(args), /positive integer/);
  }
  assert.throws(() => parseArgs(['--bin']), /usage/);
  assert.throws(() => parseArgs(['--bin', 'astro', '--root']), /usage/);
});

test('one RPC reader correlates concurrent responses and ignores notifications', async () => {
  const fixture = fixtureSpawner('reverse');
  const child = fixture.spawnImpl('', [], { stdio: ['pipe', 'pipe', 'inherit'] });
  const client = new RpcClient(child);
  try {
    const [first, second] = await Promise.all([
      client.request('first', {}, 2_000), client.request('second', {}, 2_000),
    ]);
    assert.equal(first.result.method, 'first');
    assert.equal(second.result.method, 'second');
    assert.equal(client.pending.size, 0);
  } finally {
    child.stdin.end();
    await waitExit(child, 2_000);
    client.close();
  }
});

test('catalog verification waits for a graceful EOF exit', async () => {
  const fixture = fixtureSpawner('normal');
  const result = await toolsList('', '.', 2_000, fixture.spawnImpl);
  assert.equal(result.tools.length, 7);
  assertReaped(fixture.children);
});

for (const [mode, expected] of [
  ['exit', /exited|EOF/],
  ['eof', /EOF/],
  ['invalid', /invalid JSON-RPC/],
  ['scalar', /expected an object/],
  ['silent', /timed out/],
  ['rpcerror', /fixture failure/],
  ['eofbad', /expected graceful EOF exit 0/],
]) {
  // On windows-latest CI, process.stdout.end() in a still-running node child
  // did not deliver EOF to the parent pipe, so the 'eof' fixture only timed
  // out there. The exit path is still covered by 'exit' on every platform.
  const opts = { skip: mode === 'eof' && process.platform === 'win32' };
  test(`catalog failure (${mode}) rejects and reaps the child`, opts, async () => {
    const fixture = fixtureSpawner(mode);
    await assert.rejects(toolsList('', '.', 500, fixture.spawnImpl), expected);
    assertReaped(fixture.children);
  });
}

test('spawn failure rejects instead of leaving an unresolved RPC', async () => {
  const spawnMissing = (_bin, _args, options) => spawn(
    `astrolabe-test-missing-command-${process.pid}`, [], options,
  );
  await assert.rejects(toolsList('', '.', 500, spawnMissing), /ENOENT/);
  await assert.rejects(badRootExit('', 500, spawnMissing), /ENOENT/);
});

test('missing-root verification observes nonzero exit and captures stderr', async () => {
  const fixture = fixtureSpawner('missing');
  const result = await badRootExit('', 2_000, fixture.spawnImpl);
  assert.equal(result.code, 2);
  assert.match(result.stderr, /missing directory/);
  assertReaped(fixture.children);
});

// Windows child.kill('SIGTERM') is forced termination, not the POSIX handler.
const unixOnly = { skip: process.platform === 'win32' };
test('SIGTERM waits for actual readiness beyond the old 400ms delay', unixOnly, async () => {
  const fixture = fixtureSpawner('delayed');
  const result = await signalForward('', '.', 2_000, fixture.spawnImpl);
  assert.deepEqual(result, { code: 0, signal: null });
  assertReaped(fixture.children);
});

test('legacy exit 143 is not accepted as graceful cleanup', unixOnly, async () => {
  const fixture = fixtureSpawner('term143');
  await assert.rejects(signalForward('', '.', 2_000, fixture.spawnImpl), /graceful SIGTERM exit 0/);
  assertReaped(fixture.children);
});

for (const [mode, expected] of [
  ['building', /did not become ready|timed out/],
  ['toolerror', /readiness failed/],
  ['ignoreterm', /waiting for process exit/],
]) {
  test(`signal check failure (${mode}) rejects and reaps`, unixOnly, async () => {
    const fixture = fixtureSpawner(mode);
    await assert.rejects(signalForward('', '.', 500, fixture.spawnImpl), expected);
    assertReaped(fixture.children);
  });
}

test('the real npm forwarding shim preserves graceful SIGTERM exit 0', unixOnly, async () => {
  const fixture = fixtureSpawner('normal', true);
  const result = await signalForward('', '.', 2_000, fixture.spawnImpl);
  assert.deepEqual(result, { code: 0, signal: null });
  assertReaped(fixture.children);
});

test('a stuck npm shim tree is cleaned up after the signal deadline', unixOnly, async () => {
  const fixture = fixtureSpawner('ignoreterm', true);
  await assert.rejects(signalForward('', '.', 500, fixture.spawnImpl), /waiting for process exit/);
  assertReaped(fixture.children);
  // Cleanup also closes the transport after the owned shim has been reaped.
  for (const child of fixture.children) {
    assert.equal(child.stdout.destroyed, true);
  }
});

for (const streamName of ['stdin', 'stdout']) {
  test(`RPC ${streamName} error rejects pending requests and detaches listeners`, async () => {
    const child = new EventEmitter();
    child.stdin = new PassThrough();
    child.stdout = new PassThrough();
    const client = new RpcClient(child);
    const pending = client.request('tools/list', {}, 500);
    child[streamName].emit('error', new Error(`${streamName} fixture failure`));
    await assert.rejects(pending, new RegExp(`${streamName} fixture failure`));
    assert.equal(client.pending.size, 0);
    client.close();
    assert.equal(child.listenerCount('exit'), 0);
    assert.equal(child.listenerCount('error'), 0);
    child.stdin.destroy();
    child.stdout.destroy();
  });
}
