#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const readline = require('readline');
const { spawn } = require('child_process');

function parseArgs(argv) {
  const opts = { bin: null, root: process.cwd(), timeoutMs: 15_000 };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--bin') opts.bin = argv[++i];
    else if (arg === '--root') opts.root = argv[++i];
    else if (arg === '--timeout-ms') opts.timeoutMs = Number(argv[++i]);
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (!opts.bin) throw new Error('usage: node scripts/verify-install.js --bin <astrolabe> [--root <repo>]');
  return opts;
}

function rpc(child, id, method, params) {
  return new Promise((resolve, reject) => {
    const rl = readline.createInterface({ input: child.stdout });
    const payload = JSON.stringify({
      jsonrpc: '2.0',
      id,
      method,
      ...(params !== undefined ? { params } : {}),
    });
    const onLine = (line) => {
      if (!line.trim()) return;
      rl.off('line', onLine);
      try {
        resolve(JSON.parse(line));
      } catch (error) {
        reject(new Error(`invalid JSON-RPC: ${error.message}: ${line}`));
      }
    };
    rl.on('line', onLine);
    child.stdin.write(`${payload}\n`);
  });
}

function waitExit(child, timeoutMs) {
  return new Promise((resolve, reject) => {
    if (child.exitCode != null || child.signalCode != null) {
      resolve({ code: child.exitCode, signal: child.signalCode });
      return;
    }
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error('timed out waiting for process exit'));
    }, timeoutMs);
    child.once('exit', (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal });
    });
  });
}

async function toolsList(bin, root, timeoutMs) {
  const child = spawn(bin, [root], {
    stdio: ['pipe', 'pipe', 'inherit'],
    env: { ...process.env, RUST_LOG: 'warn' },
  });
  const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
  try {
    const response = await rpc(child, 1, 'tools/list');
    if (response.error) {
      throw new Error(`tools/list error: ${JSON.stringify(response.error)}`);
    }
    const tools = (response.result && response.result.tools) || [];
    return { tools, ttlMs: response.result && response.result.ttlMs };
  } finally {
    clearTimeout(timer);
    child.kill('SIGTERM');
    await waitExit(child, 5_000).catch(() => {
      child.kill('SIGKILL');
    });
  }
}

async function badRootExit(bin) {
  const missing = path.join(fs.mkdtempSync(path.join(require('os').tmpdir(), 'astrolabe-missing-')), 'no-such-dir');
  const child = spawn(bin, [missing], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  const chunks = [];
  child.stderr.on('data', (buf) => chunks.push(buf));
  const result = await waitExit(child, 10_000);
  return {
    code: result.code,
    signal: result.signal,
    stderr: Buffer.concat(chunks).toString('utf8'),
  };
}

async function signalForward(bin, root) {
  // Keep stdin open so the MCP server does not exit on EOF before we signal.
  const child = spawn(bin, [root], {
    stdio: ['pipe', 'ignore', 'inherit'],
    env: { ...process.env, RUST_LOG: 'warn' },
  });
  const exited = waitExit(child, 10_000);
  await new Promise((resolve) => setTimeout(resolve, 400));
  child.kill('SIGTERM');
  return exited;
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const bin = path.resolve(opts.bin);
  const root = path.resolve(opts.root);
  if (!fs.existsSync(bin)) throw new Error(`bin not found: ${bin}`);
  if (!fs.statSync(root).isDirectory()) throw new Error(`root is not a directory: ${root}`);

  const catalog = await toolsList(bin, root, opts.timeoutMs);
  const names = catalog.tools.map((tool) => tool.name);
  console.log(`tools/list count=${catalog.tools.length} ttlMs=${catalog.ttlMs}`);
  console.log(`tools: ${names.join(', ')}`);
  if (catalog.tools.length < 8) {
    throw new Error(`expected at least 8 tools, got ${catalog.tools.length}`);
  }

  const bad = await badRootExit(bin);
  console.log(`missing-dir exit code=${bad.code} signal=${bad.signal}`);
  console.log(`missing-dir stderr: ${bad.stderr.trim()}`);
  if (bad.code === 0 || bad.code === null) {
    throw new Error(`expected non-zero exit for a missing directory, got code=${bad.code} signal=${bad.signal}`);
  }

  if (process.platform !== 'win32') {
    const sig = await signalForward(bin, root);
    console.log(`SIGTERM to shim -> code=${sig.code} signal=${sig.signal}`);
    const ok = sig.signal === 'SIGTERM' || sig.code === 143;
    if (!ok) {
      throw new Error(
        `expected SIGTERM (or exit 143) after killing the shim, got code=${sig.code} signal=${sig.signal}`,
      );
    }
  }

  console.log('verify-install: ok');
}

main().catch((error) => {
  console.error(`[verify-install] ${error.message}`);
  process.exit(1);
});
