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
    'codebuddy',
    'codex',
    'continue',
    'cursor',
    'deepseek-harness',
    'grok',
    'kimicode',
    'opencode',
    'roo',
    'windsurf',
    'zcode',
  ]);
  assert.equal(CATALOG_TOOL_NAMES.length, 7);
  assert.equal(clientsMod.canonicalClientId('kimi'), 'kimicode');
  assert.equal(clientsMod.canonicalClientId('zai'), 'zcode');
  assert.equal(clientsMod.canonicalClientId('grokbuild'), 'grok');
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


test('Codebuddy detect + MCP + withHooks settings merge', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.codebuddy'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.codebuddy', 'settings.json'),
      JSON.stringify(
        {
          hooks: {
            PreToolUse: [{ hooks: [{ type: 'command', command: 'echo keep' }] }],
          },
        },
        null,
        2,
      ),
    );
    const client = clientsMod.getClient('codebuddy');
    const det = client.detect(home, { PATH: '/nonexistent' });
    assert.equal(det.detected, true);
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    const plan = client.plan({ home, launch, withHooks: true });
    const applied = plan.apply();
    assert.equal(applied.skipped, undefined);
    applied.write();
    const mcp = JSON.parse(
      fs.readFileSync(path.join(home, '.codebuddy', '.mcp.json'), 'utf8'),
    );
    assert.equal(mcp.mcpServers.astrolabe.command, 'npx');
    assert.equal(mcp.mcpServers.astrolabe.type, 'stdio');
    const settings = JSON.parse(
      fs.readFileSync(path.join(home, '.codebuddy', 'settings.json'), 'utf8'),
    );
    assert.equal(settings.hooks.PreToolUse[0].hooks[0].command, 'echo keep');
    assert.ok(
      settings.hooks.PreToolUse.some((e) =>
        (e.hooks || []).some((h) => String(h.command).includes('--client=codebuddy')),
      ),
    );
    assert.ok(
      settings.hooks.SessionEnd.some((e) =>
        (e.hooks || []).some((h) => String(h.command).includes('hooks cleanup')),
      ),
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Grok MCP toml + withHooks astrolabe.json', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.grok'), { recursive: true });
    fs.writeFileSync(path.join(home, '.grok', 'config.toml'), 'model = "x"\n');
    const client = clientsMod.getClient('grok');
    assert.equal(client.detect(home, { PATH: '/nonexistent' }).detected, true);
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    const plan = client.plan({ home, launch, withHooks: true });
    const applied = plan.apply();
    applied.write();
    const toml = fs.readFileSync(path.join(home, '.grok', 'config.toml'), 'utf8');
    assert.match(toml, /model = "x"/);
    assert.match(toml, /\[mcp_servers\.astrolabe\]/);
    assert.match(toml, /command = "npx"/);
    const hooks = JSON.parse(
      fs.readFileSync(path.join(home, '.grok', 'hooks', 'astrolabe.json'), 'utf8'),
    );
    assert.ok(hooks.hooks.PreToolUse);
    assert.ok(hooks.hooks.SessionEnd);
    assert.match(
      hooks.hooks.PreToolUse[0].hooks[0].command,
      /--client=grok/,
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Kimi Code mcp.json + managed [[hooks]] toml', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.kimi-code'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.kimi-code', 'mcp.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'x', args: [] } } }, null, 2),
    );
    fs.writeFileSync(
      path.join(home, '.kimi-code', 'config.toml'),
      'default_model = "kimi"\n\n[[hooks]]\nevent = "Notification"\ncommand = "echo keep"\n',
    );
    const client = clientsMod.getClient('kimi');
    assert.equal(client.id, 'kimicode');
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    const plan = client.plan({ home, launch, withHooks: true });
    plan.apply().write();
    const mcp = JSON.parse(fs.readFileSync(path.join(home, '.kimi-code', 'mcp.json'), 'utf8'));
    assert.equal(mcp.mcpServers.keep.command, 'x');
    assert.equal(mcp.mcpServers.astrolabe.command, 'npx');
    const toml = fs.readFileSync(path.join(home, '.kimi-code', 'config.toml'), 'utf8');
    assert.match(toml, /default_model = "kimi"/);
    assert.match(toml, /echo keep/);
    assert.match(toml, /astrolabe-managed-kimi-hooks/);
    assert.match(toml, /--client=kimicode/);
    assert.match(toml, /event = "SessionEnd"/);
    // idempotent
    const again = clientsMod.mergeKimiHooksToml(path.join(home, '.kimi-code', 'config.toml'));
    assert.equal(again.replaced, true);
    assert.equal((again.text.match(/astrolabe-managed-kimi-hooks/g) || []).length, 2);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('ZCode mcp.servers + hooks.events without SessionEnd', () => {
  const home = tmpHome();
  try {
    const cli = path.join(home, '.zcode', 'cli');
    fs.mkdirSync(cli, { recursive: true });
    fs.writeFileSync(
      path.join(cli, 'config.json'),
      JSON.stringify(
        {
          mcp: { servers: { keep: { type: 'stdio', command: 'x', args: [] } } },
          hooks: {
            enabled: false,
            events: {
              SessionEnd: [{ hooks: [{ type: 'command', command: 'bad' }] }],
              // Misconfigured: cleanup on Stop would wipe counters every turn
              Stop: [
                {
                  hooks: [
                    { type: 'command', command: 'astrolabe hooks cleanup' },
                    { type: 'command', command: 'echo keep-stop' },
                  ],
                },
              ],
              PreToolUse: [{ hooks: [{ type: 'command', command: 'echo keep' }] }],
            },
          },
        },
        null,
        2,
      ),
    );
    const client = clientsMod.getClient('zai');
    assert.equal(client.id, 'zcode');
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    const plan = client.plan({ home, launch, withHooks: true });
    plan.apply().write();
    const doc = JSON.parse(fs.readFileSync(path.join(cli, 'config.json'), 'utf8'));
    assert.equal(doc.mcp.servers.keep.command, 'x');
    assert.equal(doc.mcp.servers.astrolabe.type, 'stdio');
    assert.equal(doc.mcp.servers.astrolabe.command, 'npx');
    assert.equal(doc.hooks.enabled, true);
    assert.equal(doc.hooks.events.SessionEnd, undefined);
    assert.ok(
      !JSON.stringify(doc.hooks).includes('SessionEnd'),
      'SessionEnd must not appear in ZCode hooks',
    );
    // Stop keeps non-cleanup hooks; cleanup command is stripped
    assert.ok(Array.isArray(doc.hooks.events.Stop));
    assert.equal(doc.hooks.events.Stop.length, 1);
    assert.equal(doc.hooks.events.Stop[0].hooks.length, 1);
    assert.equal(doc.hooks.events.Stop[0].hooks[0].command, 'echo keep-stop');
    assert.ok(
      !JSON.stringify(doc.hooks).includes('hooks cleanup'),
      'Stop must not hang astrolabe hooks cleanup',
    );
    assert.ok(
      doc.hooks.events.PreToolUse.some((e) =>
        (e.hooks || []).some((h) => String(h.command).includes('--client=zcode')),
      ),
    );
    assert.ok(
      doc.hooks.events.PreToolUse.some((e) =>
        (e.hooks || []).some((h) => h.command === 'echo keep'),
      ),
    );
    // MCP-only path does not force hooks.enabled
    const home2 = tmpHome();
    try {
      fs.mkdirSync(path.join(home2, '.zcode', 'cli'), { recursive: true });
      const p2 = clientsMod.getClient('zcode').plan({
        home: home2,
        launch,
        withHooks: false,
      });
      p2.apply().write();
      const d2 = JSON.parse(
        fs.readFileSync(path.join(home2, '.zcode', 'cli', 'config.json'), 'utf8'),
      );
      assert.ok(d2.mcp.servers.astrolabe);
      assert.equal(d2.hooks, undefined);
    } finally {
      fs.rmSync(home2, { recursive: true, force: true });
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('ZCode withHooks strips Stop cleanup but keeps non-cleanup Stop', () => {
  const home = tmpHome();
  try {
    const cli = path.join(home, '.zcode', 'cli');
    fs.mkdirSync(cli, { recursive: true });
    fs.writeFileSync(
      path.join(cli, 'config.json'),
      JSON.stringify(
        {
          hooks: {
            enabled: true,
            events: {
              Stop: [
                {
                  matcher: 'cleanup-only',
                  hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }],
                },
                {
                  matcher: 'other',
                  hooks: [{ type: 'command', command: 'echo turn-end' }],
                },
              ],
            },
          },
        },
        null,
        2,
      ),
    );
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    clientsMod
      .getClient('zcode')
      .plan({ home, launch, withHooks: true })
      .apply()
      .write();
    const doc = JSON.parse(fs.readFileSync(path.join(cli, 'config.json'), 'utf8'));
    assert.equal(doc.hooks.events.SessionEnd, undefined);
    assert.ok(Array.isArray(doc.hooks.events.Stop));
    assert.equal(doc.hooks.events.Stop.length, 1);
    assert.equal(doc.hooks.events.Stop[0].matcher, 'other');
    assert.equal(doc.hooks.events.Stop[0].hooks[0].command, 'echo turn-end');
    assert.ok(!JSON.stringify(doc.hooks).includes('hooks cleanup'));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('setup --clients aliases resolve for new clients', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.grok'), { recursive: true });
    const dry = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=grokbuild,kimi,zai,codebuddy',
        '--dry-run',
        '--home',
        home,
        '--no-cursor-permissions',
      ],
      { encoding: 'utf8' },
    );
    assert.equal(dry.status, 0, dry.stderr + dry.stdout);
    assert.match(dry.stdout, /Grok Build/);
    assert.match(dry.stdout, /Kimi Code/);
    assert.match(dry.stdout, /ZCode/);
    // codebuddy skipped (no ~/.codebuddy)
    assert.match(dry.stdout, /Codebuddy: SKIP|Codebuddy/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
