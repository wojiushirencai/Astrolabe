'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { spawnSync } = require('node:child_process');

const argsMod = require('./setup/args');
const clientsMod = require('./setup/clients');
const { CLIENTS, CATALOG_TOOL_NAMES } = clientsMod;

const BIN = path.join(__dirname, '..', 'bin', 'astrolabe.js');

function tmpHome() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'astrolabe-setup-'));
}

test('parseArgs accepts yes/clients/dry-run/native', () => {
  const opts = argsMod.parseArgs([
    '--yes',
    '--clients=cursor,codex',
    '--dry-run',
    '--native',
    '--home',
    '/tmp/h',
  ]);
  assert.equal(opts.yes, true);
  assert.deepEqual(opts.clients, ['cursor', 'codex']);
  assert.equal(opts.dryRun, true);
  assert.equal(opts.native, true);
  assert.equal(opts.home, '/tmp/h');
});

test('resolveLaunch defaults to npx @zlnap/astrolabe', () => {
  assert.deepEqual(clientsMod.resolveLaunch({ context: 'cursor' }), {
    command: 'npx',
    args: ['-y', '@zlnap/astrolabe', '.', '--context=cursor'],
  });
  assert.deepEqual(clientsMod.resolveLaunch({ context: null, native: true }), {
    command: 'astrolabe',
    args: ['.'],
  });
});

test('mergeJsonMcpFile preserves other servers', () => {
  const home = tmpHome();
  try {
    const filePath = path.join(home, 'mcp.json');
    fs.writeFileSync(
      filePath,
      JSON.stringify({ mcpServers: { other: { command: 'x', args: [] } } }, null, 2),
    );
    const entry = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.', '--context=cursor'] };
    const result = clientsMod.mergeJsonMcpFile(filePath, entry);
    assert.equal(result.created, false);
    assert.equal(result.doc.mcpServers.other.command, 'x');
    assert.deepEqual(result.doc.mcpServers.astrolabe, entry);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('mergeCodexToml appends and replaces astrolabe table only', () => {
  const home = tmpHome();
  try {
    const filePath = path.join(home, 'config.toml');
    fs.writeFileSync(
      filePath,
      'model = "x"\n\n[mcp_servers.other]\ncommand = "o"\nargs = ["1"]\n',
    );
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.', '--context=codex'] };
    const first = clientsMod.mergeCodexToml(filePath, launch);
    assert.match(first.text, /\[mcp_servers\.astrolabe\]/);
    assert.match(first.text, /\[mcp_servers\.other\]/);
    assert.match(first.text, /model = "x"/);

    fs.writeFileSync(filePath, first.text);
    const second = clientsMod.mergeCodexToml(filePath, {
      command: 'astrolabe',
      args: ['.', '--context=codex'],
    });
    assert.equal(second.replaced, true);
    assert.match(second.text, /command = "astrolabe"/);
    assert.equal((second.text.match(/\[mcp_servers\.astrolabe\]/g) || []).length, 1);
    assert.match(second.text, /\[mcp_servers\.other\]/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('CLIENTS catalog covers documented agents', () => {
  const ids = CLIENTS.map((c) => c.id).sort();
  assert.deepEqual(ids, [
    'claude-code',
    'cline',
    'codex',
    'continue',
    'cursor',
    'roo',
    'windsurf',
  ]);
  assert.equal(CATALOG_TOOL_NAMES.length, 7);
});

test('setup --list and --dry-run via bin shim', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.cursor', 'mcp.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'keep' } } }, null, 2),
    );
    const list = spawnSync(process.execPath, [BIN, 'setup', '--list', '--home', home], {
      encoding: 'utf8',
    });
    assert.equal(list.status, 0, list.stderr);
    assert.match(list.stdout, /cursor\s+detected/);

    const dry = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--clients=cursor', '--dry-run', '--home', home, '--no-cursor-permissions'],
      { encoding: 'utf8' },
    );
    assert.equal(dry.status, 0, dry.stderr + dry.stdout);
    assert.match(dry.stdout, /\[dry-run\]/);
    const doc = JSON.parse(fs.readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8'));
    assert.equal(doc.mcpServers.keep.command, 'keep');
    assert.equal(doc.mcpServers.astrolabe, undefined);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('setup --yes writes cursor mcp.json and merges allowlist', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.cursor', 'mcp.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'keep' } } }, null, 2),
    );
    const result = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--clients=cursor', '--home', home],
      { encoding: 'utf8' },
    );
    assert.equal(result.status, 0, result.stderr + result.stdout);
    const mcp = JSON.parse(fs.readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8'));
    assert.equal(mcp.mcpServers.keep.command, 'keep');
    assert.equal(mcp.mcpServers.astrolabe.command, 'npx');
    assert.deepEqual(mcp.mcpServers.astrolabe.args, [
      '-y',
      '@zlnap/astrolabe',
      '.',
      '--context=cursor',
    ]);
    const perms = JSON.parse(
      fs.readFileSync(path.join(home, '.cursor', 'permissions.json'), 'utf8'),
    );
    assert.ok(perms.mcpAllowlist.includes('astrolabe:search_code'));
    assert.equal(perms.mcpAllowlist.length, 7);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('detect uses PATH and home markers', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.codex'), { recursive: true });
    const env = { ...process.env, PATH: '/nonexistent/bin', HOME: home };
    const codex = CLIENTS.find((c) => c.id === 'codex').detect(home, env);
    assert.equal(codex.detected, true);
    const windsurf = CLIENTS.find((c) => c.id === 'windsurf').detect(home, env);
    assert.equal(windsurf.detected, false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
