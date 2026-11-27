#!/usr/bin/env node
'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const readline = require('readline');
const { spawn } = require('child_process');

const processGroups = new WeakSet();

function parseArgs(argv) {
  const opts = { bin: null, root: process.cwd(), timeoutMs: 15_000 };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--bin') opts.bin = argv[++i];
    else if (arg === '--root') opts.root = argv[++i];
    else if (arg === '--timeout-ms') opts.timeoutMs = Number(argv[++i]);
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (!opts.bin || !opts.root) {
    throw new Error('usage: node scripts/verify-install.js --bin <astrolabe> [--root <repo>]');
  }
  if (!Number.isSafeInteger(opts.timeoutMs) || opts.timeoutMs <= 0) {
    throw new Error('--timeout-ms must be a positive integer');
  }
  return opts;
}

// One reader per child: notifications and out-of-order responses must not be
// mistaken for the response to the most recently written request.
class RpcClient {
  constructor(child) {
    this.child = child;
    this.pending = new Map();
    this.nextId = 0;
    this.failure = null;
    this.rl = readline.createInterface({ input: child.stdout });
    this.onError = (error) => this.fail(error);
    this.onClose = () => this.fail(new Error('JSON-RPC stdout closed (EOF)'));
    this.onExit = (code, signal) => this.fail(new Error(`server exited: code=${code} signal=${signal}`));
    this.onLine = (line) => {
      if (!line.trim()) return;
      let message;
      try {
        message = JSON.parse(line);
      } catch (error) {
        this.fail(new Error(`invalid JSON-RPC: ${error.message}: ${line}`));
        return;
      }
      if (!message || typeof message !== 'object' || Array.isArray(message)) {
        this.fail(new Error('invalid JSON-RPC: expected an object'));
        return;
      }
      const request = this.pending.get(message.id);
      if (!request) return; // notification or response to an expired request
      request.finish(null, message);
    };
    this.rl.on('error', this.onError);
    this.rl.on('line', this.onLine);
    this.rl.on('close', this.onClose);
    child.on('error', this.onError);
    child.on('exit', this.onExit);
    child.stdin.on('error', this.onError);
    child.stdout.on('error', this.onError);
  }

  fail(error) {
    this.failure ||= error;
    for (const request of this.pending.values()) request.finish(this.failure);
  }

  request(method, params, timeoutMs) {
    if (this.failure) return Promise.reject(this.failure);
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      const finish = (error, value) => {
        clearTimeout(timer);
        this.pending.delete(id);
        if (error) reject(error);
        else resolve(value);
      };
      const timer = setTimeout(() => {
        this.fail(new Error(`JSON-RPC ${method} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      this.pending.set(id, { finish });
      const payload = { jsonrpc: '2.0', id, method, ...(params !== undefined ? { params } : {}) };
      try {
        this.child.stdin.write(`${JSON.stringify(payload)}\n`, (error) => {
          if (error) this.fail(error);
        });
      } catch (error) {
        this.fail(error);
      }
    });
  }

  close() {
    this.fail(new Error('JSON-RPC client closed'));
    this.rl.off('line', this.onLine);
    this.rl.off('close', this.onClose);
    this.rl.close();
    this.rl.off('error', this.onError);
    this.child.off('error', this.onError);
    this.child.off('exit', this.onExit);
    this.child.stdin.off('error', this.onError);
    this.child.stdout.off('error', this.onError);
  }
}

function waitExit(child, timeoutMs) {
  if (child.exitCode != null || child.signalCode != null) {
    return Promise.resolve({ code: child.exitCode, signal: child.signalCode });
  }
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timer);
      child.off('exit', onExit);
      child.off('error', onError);
    };
    const onExit = (code, signal) => {
      cleanup();
      resolve({ code, signal });
    };
    const onError = (error) => { cleanup(); reject(error); };
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error(`timed out waiting for process exit after ${timeoutMs}ms`));
    }, timeoutMs);
    child.once('exit', onExit);
    child.once('error', onError);
  });
}

async function cleanupChild(child) {
  // A timed-out npm shim may still have a native child holding the pipes open.
  // POSIX children get their own process group, so forced cleanup reaps the
  // whole verification tree rather than orphaning the server/LSP descendants.
  if (processGroups.has(child) && child.pid != null) {
    try { process.kill(-child.pid, 'SIGKILL'); } catch (error) {
      // Darwin may report EPERM for a group whose last member has exited
      // but has not been reaped yet. Still kill/wait the owned child below.
      if (!['ESRCH', 'EPERM'].includes(error.code)) throw error;
    }
  }
  if (child.pid != null && child.exitCode == null && child.signalCode == null) {
    child.kill('SIGKILL');
    await waitExit(child, 5_000);
  }
  child.stdin?.destroy();
  child.stdout?.destroy();
  child.stderr?.destroy();
}

function startServer(bin, root, spawnImpl) {
  const detached = process.platform !== 'win32';
  const child = spawnImpl(bin, [root], {
    stdio: ['pipe', 'pipe', 'inherit'],
    env: { ...process.env, RUST_LOG: 'warn' },
    detached,
  });
  if (detached) processGroups.add(child);
  return child;
}

function resultOf(response, method) {
  if (response.error) throw new Error(`${method} error: ${JSON.stringify(response.error)}`);
  if (!response.result || typeof response.result !== 'object') {
    throw new Error(`${method} returned no result`);
  }
  return response.result;
}

async function toolsList(bin, root, timeoutMs, spawnImpl = spawn) {
  const child = startServer(bin, root, spawnImpl);
  const client = new RpcClient(child);
  try {
    const catalog = resultOf(await client.request('tools/list', undefined, timeoutMs), 'tools/list');
    if (!Array.isArray(catalog.tools)) throw new Error('tools/list returned no tools array');
    // EOF is a separate lifecycle contract from signal forwarding.
    child.stdin.end();
    const exit = await waitExit(child, timeoutMs);
    if (exit.code !== 0 || exit.signal !== null) {
      throw new Error(`expected graceful EOF exit 0, got code=${exit.code} signal=${exit.signal}`);
    }
    return catalog;
  } finally {
    try { await cleanupChild(child); } finally { client.close(); }
  }
}

async function badRootExit(bin, timeoutMs = 10_000, spawnImpl = spawn) {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'astrolabe-missing-'));
  let child;
  try {
    child = spawnImpl(bin, [path.join(temp, 'no-such-dir')], {
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    const chunks = [];
    child.stderr.on('data', (buf) => chunks.push(buf));
    const result = await waitExit(child, timeoutMs);
    return { ...result, stderr: Buffer.concat(chunks).toString('utf8') };
  } finally {
    try { if (child) await cleanupChild(child); } finally { fs.rmSync(temp, { recursive: true, force: true }); }
  }
}

async function waitReady(client, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (true) {
    const remaining = deadline - Date.now();
    if (remaining <= 0) throw new Error(`server did not become ready within ${timeoutMs}ms`);
    const result = resultOf(await client.request('tools/call', {
      name: 'get_lang', arguments: {},
    }, remaining), 'get_lang');
    const text = (result.content || []).filter((item) => item.type === 'text').map((item) => item.text).join('\n');
    if (!text.trim()) throw new Error(`get_lang readiness failed: ${JSON.stringify(result)}`);
    // Building is a retryable MCP isError result, not a startup failure.
    if (!/index is building|still building/i.test(text)) {
      if (result.isError) throw new Error(`get_lang readiness failed: ${JSON.stringify(result)}`);
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, Math.min(50, remaining)));
  }
}

async function signalForward(bin, root, timeoutMs = 15_000, spawnImpl = spawn) {
  const child = startServer(bin, root, spawnImpl);
  const client = new RpcClient(child);
  try {
    // A successful tool result (not a fixed startup sleep) proves that the
    // server is serving before SIGTERM. Keep stdin open throughout the check.
    await waitReady(client, timeoutMs);
    const exited = waitExit(child, timeoutMs);
    child.kill('SIGTERM');
    const result = await exited;
    if (result.code !== 0 || result.signal !== null) {
      throw new Error(`expected graceful SIGTERM exit 0, got code=${result.code} signal=${result.signal}`);
    }
    return result;
  } finally {
    try { await cleanupChild(child); } finally { client.close(); }
  }
}

async function main(argv = process.argv.slice(2)) {
  const opts = parseArgs(argv);
  const bin = path.resolve(opts.bin);
  const root = path.resolve(opts.root);
  if (!fs.existsSync(bin)) throw new Error(`bin not found: ${bin}`);
  if (!fs.statSync(root).isDirectory()) throw new Error(`root is not a directory: ${root}`);

  const catalog = await toolsList(bin, root, opts.timeoutMs);
  const names = catalog.tools.map((tool) => tool.name);
  console.log(`tools/list count=${catalog.tools.length} ttlMs=${catalog.ttlMs}`);
  console.log(`tools: ${names.join(', ')}`);
  const expected = ['find_refs', 'find_symbol', 'get_diag', 'get_lang', 'goto_def', 'resolve_contx', 'search_code'];
  if (expected.some((name) => !names.includes(name))) {
    throw new Error(`missing required tools: ${expected.filter((name) => !names.includes(name)).join(', ')}`);
  }

  const bad = await badRootExit(bin, opts.timeoutMs);
  console.log(`missing-dir exit code=${bad.code} signal=${bad.signal}`);
  console.log(`missing-dir stderr: ${bad.stderr.trim()}`);
  if (bad.code === 0 || bad.code === null) {
    throw new Error(`expected non-zero exit for a missing directory, got code=${bad.code} signal=${bad.signal}`);
  }

  if (process.platform !== 'win32') {
    const sig = await signalForward(bin, root, opts.timeoutMs);
    console.log(`SIGTERM to shim -> code=${sig.code} signal=${sig.signal}`);
    if (sig.code !== 0 || sig.signal !== null) {
      throw new Error(`expected graceful SIGTERM exit 0, got code=${sig.code} signal=${sig.signal}`);
    }
  }
  console.log('verify-install: ok');
}

if (require.main === module) {
  main().catch((error) => {
    console.error(`[verify-install] ${error.message}`);
    process.exitCode = 1;
  });
}
module.exports = { RpcClient, parseArgs, waitExit, toolsList, badRootExit, waitReady, signalForward, main };
