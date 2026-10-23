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
    'deepseek-harness',
    'opencode',
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

test('parseArgs accepts --with-hooks', () => {
  const opts = argsMod.parseArgs(['--with-hooks', '--clients=dsh,opencode']);
  assert.equal(opts.withHooks, true);
  assert.deepEqual(opts.clients, ['dsh', 'opencode']);
  assert.equal(clientsMod.getClient('dsh').id, 'deepseek-harness');
  assert.equal(clientsMod.canonicalClientId('dsh'), 'deepseek-harness');
});

test('OpenCode V1 merge preserves JSONC comments and siblings', () => {
  const home = tmpHome();
  try {
    const dir = path.join(home, '.config', 'opencode');
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(
      path.join(dir, 'opencode.jsonc'),
      `{
  // keep-comment
  "mcp": {
    "serena": { "type": "local", "command": ["echo"], "enabled": true }
  }
}
`,
    );
    const r = clientsMod.mergeOpenCodeMcpFile(path.join(dir, 'opencode.jsonc'), {
      command: 'npx',
      args: ['-y', '@zlnap/astrolabe', '.'],
    });
    assert.equal(r.format, 'v1');
    assert.match(r.text, /keep-comment/);
    assert.match(r.text, /"serena"/);
    assert.match(r.text, /"astrolabe"/);
    assert.match(r.text, /"enabled"\s*:\s*true/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('OpenCode V2 merge writes mcp.servers without enabled', () => {
  const home = tmpHome();
  try {
    const file = path.join(home, 'opencode.jsonc');
    fs.writeFileSync(
      file,
      `{
  "mcp": {
    "servers": {
      "other": { "type": "local", "command": ["x"] }
    }
  }
}
`,
    );
    const r = clientsMod.mergeOpenCodeMcpFile(file, {
      command: 'npx',
      args: ['-y', '@zlnap/astrolabe', '.'],
    });
    assert.equal(r.format, 'v2');
    const doc = clientsMod.parseJsonc(r.text);
    assert.ok(doc.mcp.servers.astrolabe);
    assert.equal(doc.mcp.servers.astrolabe.enabled, undefined);
    assert.ok(doc.mcp.servers.other);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('OpenCode resolve prefers existing ~/.opencode/config.json', () => {
  const home = tmpHome();
  try {
    const old = path.join(home, '.opencode', 'config.json');
    fs.mkdirSync(path.dirname(old), { recursive: true });
    fs.writeFileSync(old, '{}\n');
    assert.equal(clientsMod.resolveOpenCodeConfigPath(home, fs.existsSync), old);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('OpenCode detect uses config dir markers', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.config', 'opencode'), { recursive: true });
    const d = clientsMod.getClient('opencode').detect(home, { PATH: '/nonexistent' });
    assert.equal(d.detected, true);
    assert.ok(d.evidence.some((e) => e.includes('.config/opencode')));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('DSH cordis patch keeps orca-managed block', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.dsh'), { recursive: true });
    const patch = path.join(home, '.dsh', 'cordis.patch.yml');
    fs.writeFileSync(
      patch,
      `# >>> orca-managed-dsh-hooks (managed by Orca; do not edit) >>>
- insert:
    - id: orca-agent-hooks
      name: '@deepseek-ai/dsh-hooks-claude-code'
# <<< orca-managed-dsh-hooks <<<
`,
    );
    const r = clientsMod.mergeDshCordisPatch(patch);
    assert.match(r.text, /orca-managed-dsh-hooks/);
    assert.match(r.text, /astrolabe-managed-dsh-guard/);
    assert.match(r.text, /astrolabe-dsh-guard/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('DSH plan copies vendor plugin and writes patch', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.dsh', 'profiles', 'node_modules'), { recursive: true });
    const client = clientsMod.getClient('deepseek-harness');
    const plan = client.plan({
      home,
      launch: { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] },
    });
    const applied = plan.apply();
    assert.equal(applied.skipped, undefined);
    applied.write();
    assert.ok(
      fs.existsSync(
        path.join(home, '.dsh', 'profiles', 'node_modules', 'astrolabe-dsh-guard', 'package.json'),
      ),
    );
    const text = fs.readFileSync(path.join(home, '.dsh', 'cordis.patch.yml'), 'utf8');
    assert.match(text, /astrolabe-managed-dsh-guard/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
