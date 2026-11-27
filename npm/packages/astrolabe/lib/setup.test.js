'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { spawnSync } = require('node:child_process');

const argsMod = require('./setup/args');
const clientsMod = require('./setup/clients');
const claudeCodeMod = require('./setup/claude-code');
const cursorMod = require('./setup/cursor');
const runMod = require('./setup/run');
const codebuddyMod = require('./setup/codebuddy');
const grokMod = require('./setup/grok');
const kimicodeMod = require('./setup/kimicode');
const zcodeMod = require('./setup/zcode');
const opencodeMod = require('./setup/opencode');
const dshMod = require('./setup/dsh');
const { CLIENTS, CATALOG_TOOL_NAMES } = clientsMod;

const BIN = path.join(__dirname, '..', 'bin', 'astrolabe.js');

function tmpHome() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'astrolabe-setup-'));
}

function tmpProject(home) {
  const project = path.join(home, 'project');
  fs.mkdirSync(project, { recursive: true });
  return project;
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
  assert.equal(opts.scope, 'global');
  assert.equal(argsMod.parseArgs(['--scope=project']).scope, 'project');
  assert.throws(() => argsMod.parseArgs(['--scope=bogus']), /--scope must be global or project/);
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
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--dry-run',
        '--home',
        home,
        '--project',
        tmpProject(home),
        '--command',
        path.join(home, 'bin', 'astrolabe'),
        '--no-cursor-permissions',
      ],
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

test('setup --yes defaults to global cursor mcp.json and leaves the project untouched', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.cursor', 'mcp.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'keep' } } }, null, 2),
    );
    const project = tmpProject(home);
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const bin = path.join(home, 'bin', 'astrolabe');
    const result = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--home',
        home,
        '--project',
        project,
        '--command',
        bin,
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(result.status, 0, result.stderr + result.stdout);
    assert.match(result.stdout, /global config/);
    assert.match(result.stdout, /no file-based global rules directory/);

    const mcp = JSON.parse(fs.readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8'));
    assert.equal(mcp.mcpServers.keep.command, 'keep');
    assert.equal(mcp.mcpServers.astrolabe.command, bin);
    assert.deepEqual(mcp.mcpServers.astrolabe.args, ['.', '--context=cursor']);
    assert.equal(mcp.mcpServers.astrolabe.cwd, undefined);
    assert.equal(mcp.mcpServers.astrolabe.env.ASTROLABE_LSP_TIMEOUT, '300');
    assert.equal(typeof mcp.mcpServers.astrolabe.env.PATH, 'string');

    // Default scope must not create project-level config or rules.
    assert.equal(fs.existsSync(path.join(project, '.cursor', 'mcp.json')), false);
    assert.equal(fs.existsSync(path.join(project, '.cursor', 'rules', 'astrolabe.mdc')), false);

    const perms = JSON.parse(
      fs.readFileSync(path.join(home, '.cursor', 'permissions.json'), 'utf8'),
    );
    assert.ok(perms.mcpAllowlist.includes('astrolabe:search_code'));
    assert.equal(perms.mcpAllowlist.length, 7);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('setup --scope=project writes project config, rules and approves that directory', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    const project = tmpProject(home);
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const bin = path.join(home, 'bin', 'astrolabe');
    const result = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--scope=project',
        '--home',
        home,
        '--project',
        project,
        '--command',
        bin,
        '--no-cursor-permissions',
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(result.status, 0, result.stderr + result.stdout);

    const realProject = clientsMod.canonicalizePath(project);
    const projectMcp = JSON.parse(
      fs.readFileSync(path.join(project, '.cursor', 'mcp.json'), 'utf8'),
    );
    assert.equal(projectMcp.mcpServers.astrolabe.command, bin);
    assert.deepEqual(projectMcp.mcpServers.astrolabe.args, ['.', '--context=cursor']);
    assert.equal(projectMcp.mcpServers.astrolabe.cwd, realProject);
    assert.equal(projectMcp.mcpServers.astrolabe.env.ASTROLABE_LSP_TIMEOUT, '300');
    assert.equal(typeof projectMcp.mcpServers.astrolabe.env.PATH, 'string');

    const rules = fs.readFileSync(
      path.join(project, '.cursor', 'rules', 'astrolabe.mdc'),
      'utf8',
    );
    assert.match(rules, /^---\n/);
    assert.match(rules, /alwaysApply: true/);
    assert.match(rules, /astrolabe-find_symbol/);
    assert.ok(rules.includes(realProject), rules);

    // Project scope does not create the global fallback.
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'mcp.json')), false);
    assert.match(result.stdout, /cursor-agent not found on PATH/);
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

test('Cursor client context is always cursor (never claude-code)', () => {
  const client = clientsMod.getClient('cursor');
  assert.equal(client.context, 'cursor');
  const launch = clientsMod.resolveLaunch({ context: client.context });
  assert.ok(launch.args.includes('--context=cursor'));
  assert.ok(!launch.args.some((a) => String(a).includes('claude-code')));
});

test('Cursor --with-hooks writes hooks.json with --client=cursor and mcp matcher', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    // Bad prior config: wrong client + matcher missing mcp__astrolabe__.*
    fs.writeFileSync(
      path.join(home, '.cursor', 'hooks.json'),
      JSON.stringify(
        {
          version: 1,
          hooks: {
            preToolUse: [
              {
                command: 'astrolabe hooks remind --client=claude-code',
                matcher: 'Shell|Read|Grep',
              },
            ],
          },
        },
        null,
        2,
      ),
    );
    fs.writeFileSync(
      path.join(home, '.cursor', 'mcp.json'),
      JSON.stringify(
        {
          mcpServers: {
            astrolabe: {
              command: 'npx',
              args: ['-y', '@zlnap/astrolabe', '.', '--context=claude-code'],
            },
          },
        },
        null,
        2,
      ),
    );
    const project = tmpProject(home);
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const bin = path.join(home, 'bin', 'astrolabe');
    const result = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--with-hooks',
        '--home',
        home,
        '--project',
        project,
        '--command',
        bin,
        '--no-cursor-permissions',
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(result.status, 0, result.stderr + result.stdout);
    assert.equal(fs.existsSync(path.join(project, '.cursor', 'mcp.json')), false);
    assert.equal(fs.existsSync(path.join(project, '.cursor', 'rules', 'astrolabe.mdc')), false);

    const mcp = JSON.parse(fs.readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8'));
    assert.equal(mcp.mcpServers.astrolabe.command, bin);
    assert.deepEqual(mcp.mcpServers.astrolabe.args, ['.', '--context=cursor']);
    assert.ok(!JSON.stringify(mcp).includes('claude-code'));

    const hooks = JSON.parse(fs.readFileSync(path.join(home, '.cursor', 'hooks.json'), 'utf8'));
    assert.equal(hooks.version, 1);
    const remind = hooks.hooks.preToolUse.find((e) =>
      String(e.command || '').includes('astrolabe hooks remind'),
    );
    assert.ok(remind, 'preToolUse remind missing');
    assert.equal(remind.command, `${bin} hooks remind --client=cursor`);
    assert.equal(remind.matcher, clientsMod.CURSOR_REMIND_MATCHER);
    assert.match(remind.matcher, /astrolabe-\.\*/);
    assert.match(remind.matcher, /mcp__astrolabe__\.\*/);
    assert.ok(!remind.command.includes('claude-code'));
    const cleanup = hooks.hooks.sessionEnd.find((e) =>
      String(e.command || '').includes('hooks cleanup'),
    );
    assert.ok(cleanup, 'sessionEnd cleanup missing');
    // Idempotent: no duplicate reminds
    const remindCount = hooks.hooks.preToolUse.filter((e) =>
      String(e.command || '').includes('astrolabe hooks remind'),
    ).length;
    assert.equal(remindCount, 1);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('ZCode --with-hooks replaces wrong remind client and scrubs Stop cleanup', () => {
  const home = tmpHome();
  try {
    const cli = path.join(home, '.zcode', 'cli');
    fs.mkdirSync(cli, { recursive: true });
    fs.writeFileSync(
      path.join(cli, 'config.json'),
      JSON.stringify(
        {
          mcp: { servers: {} },
          hooks: {
            enabled: true,
            events: {
              PreToolUse: [
                {
                  matcher: '*',
                  hooks: [
                    {
                      type: 'command',
                      command: 'astrolabe hooks remind --client=claude-code',
                      timeout: 10,
                    },
                  ],
                },
              ],
              Stop: [
                {
                  matcher: '*',
                  hooks: [
                    {
                      type: 'command',
                      command: 'astrolabe hooks cleanup',
                      timeout: 10,
                    },
                    {
                      type: 'command',
                      command: 'echo keep-stop',
                    },
                  ],
                },
              ],
              SessionEnd: [
                {
                  hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }],
                },
              ],
            },
          },
        },
        null,
        2,
      ),
    );
    const client = clientsMod.getClient('zcode');
    assert.equal(client.context, null);
    const launch = clientsMod.resolveLaunch({ context: client.context });
    assert.ok(!launch.args.some((a) => String(a).includes('--context=')));
    const noPathEnv = { ...process.env, PATH: '/nonexistent' };
    const plan = client.plan({ home, launch, withHooks: true, env: noPathEnv });
    plan.apply().write();
    const doc = JSON.parse(fs.readFileSync(path.join(cli, 'config.json'), 'utf8'));
    assert.equal(doc.hooks.events.SessionEnd, undefined);
    assert.ok(!JSON.stringify(doc.hooks).includes('SessionEnd'));
    // Stop cleanup removed; non-cleanup Stop hook preserved
    assert.ok(doc.hooks.events.Stop, 'Stop should remain when non-cleanup hooks survive');
    const stopCmds = (doc.hooks.events.Stop || []).flatMap((e) =>
      (e.hooks || []).map((h) => h.command),
    );
    assert.ok(stopCmds.includes('echo keep-stop'));
    assert.ok(!stopCmds.some((c) => String(c).includes('hooks cleanup')));
    // Wrong client replaced
    const remindCmds = (doc.hooks.events.PreToolUse || []).flatMap((e) =>
      (e.hooks || []).map((h) => h.command),
    );
    assert.ok(remindCmds.includes('astrolabe hooks remind --client=zcode'));
    assert.ok(!remindCmds.some((c) => String(c).includes('--client=claude-code')));
    // Pure Stop-cleanup-only configs drop Stop entirely
    const home2 = tmpHome();
    try {
      const cli2 = path.join(home2, '.zcode', 'cli');
      fs.mkdirSync(cli2, { recursive: true });
      fs.writeFileSync(
        path.join(cli2, 'config.json'),
        JSON.stringify(
          {
            hooks: {
              events: {
                Stop: [
                  {
                    hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }],
                  },
                ],
              },
            },
          },
          null,
          2,
        ),
      );
      clientsMod
        .getClient('zcode')
        .plan({ home: home2, launch, withHooks: true, env: noPathEnv })
        .apply()
        .write();
      const d2 = JSON.parse(fs.readFileSync(path.join(cli2, 'config.json'), 'utf8'));
      assert.equal(d2.hooks.events.Stop, undefined);
      assert.ok(
        d2.hooks.events.PreToolUse.some((e) =>
          (e.hooks || []).some((h) => h.command === 'astrolabe hooks remind --client=zcode'),
        ),
      );
    } finally {
      fs.rmSync(home2, { recursive: true, force: true });
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('mergeCursorPermissions migrates legacy long names and check reports it', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.cursor', 'permissions.json'),
      JSON.stringify(
        {
          mcpAllowlist: [
            'other:keep',
            'astrolabe:resolve_context',
            'astrolabe:find_references',
            'astrolabe:search_code',
          ],
        },
        null,
        2,
      ),
    );
    const check = clientsMod.checkCursorAllowlist(home);
    assert.equal(check.status, 'needs-migrate');
    assert.ok(check.legacyNames.includes('resolve_context'));

    const plan = clientsMod.mergeCursorPermissions(home, { yes: true });
    assert.ok(plan.migrated >= 2);
    assert.equal(plan.doc.mcpAllowlist.includes('other:keep'), true);
    assert.equal(plan.doc.mcpAllowlist.includes('astrolabe:resolve_context'), false);
    assert.equal(plan.doc.mcpAllowlist.includes('astrolabe:resolve_contx'), true);
    assert.equal(plan.doc.mcpAllowlist.includes('astrolabe:find_refs'), true);
    assert.equal(plan.doc.mcpAllowlist.length, 1 + CATALOG_TOOL_NAMES.length);

    const result = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--home',
        home,
        '--project',
        tmpProject(home),
        '--command',
        path.join(home, 'bin', 'astrolabe'),
        '--dry-run',
      ],
      { encoding: 'utf8' },
    );
    assert.equal(result.status, 0, result.stderr + result.stdout);
    assert.match(result.stdout, /Cursor MCP allowlist check/);
    assert.match(result.stdout, /legacy long names/);
    assert.match(result.stdout, /short catalog names only/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Cursor project root/slug matches cursor-agent project dirs', () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    const realProject = clientsMod.canonicalizePath(project);
    assert.equal(clientsMod.cursorProjectSlug('/Users/a/Project/serena+visio'), 'Users-a-Project-serena-visio');
    assert.equal(clientsMod.cursorProjectSlug('C:\\Users\\a\\repo'), 'C-Users-a-repo');
    assert.equal(
      clientsMod.resolveCursorApprovalPath(home, project),
      path.join(
        home,
        '.cursor',
        'projects',
        clientsMod.cursorProjectSlug(realProject),
        'mcp-approvals.json',
      ),
    );
    assert.equal(
      clientsMod.resolveCursorProjectRoot(project, {
        spawnFn: () => ({ status: 1, stdout: '' }),
      }),
      realProject,
    );
    const gitTop = clientsMod.resolveCursorProjectRoot('/ignored', {
      spawnFn: () => ({ status: 0, stdout: `${project}\n` }),
    });
    assert.equal(gitTop, realProject);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Cursor server entry injects PATH/timeout and probes pyright', () => {
  const noPyright = clientsMod.detectPyright(
    { PATH: '', HOME: '/none' },
    { whichFn: () => null, existsFn: () => false, home: '/none', platform: 'linux' },
  );
  assert.equal(noPyright, null);

  const found = clientsMod.detectPyright(
    { PATH: '/x' },
    {
      whichFn: (cmd) => (cmd === 'pyright-langserver' ? '/x/pyright-langserver' : null),
      existsFn: () => false,
      home: '/h',
      platform: 'linux',
    },
  );
  assert.equal(found, '/x/pyright-langserver');

  const entry = clientsMod.cursorServerEntry({
    binPath: '/abs/astrolabe',
    projectRoot: '/proj',
    env: { PATH: '/bin:/usr/bin' },
    pyrightPath: found,
    includeCwd: true,
  });
  assert.equal(entry.command, '/abs/astrolabe');
  assert.deepEqual(entry.args, ['.', '--context=cursor']);
  assert.equal(entry.cwd, path.resolve('/proj'));
  assert.equal(entry.env.PATH, '/bin:/usr/bin');
  assert.equal(entry.env.ASTROLABE_LSP_TIMEOUT, '300');
  assert.equal(entry.env.ASTROLABE_LSP_PYTHON, '/x/pyright-langserver');

  const fallback = clientsMod.cursorServerEntry({
    binPath: '/abs/astrolabe',
    env: { PATH: '/bin' },
    pyrightPath: null,
    includeCwd: false,
  });
  assert.equal(fallback.cwd, undefined);
  assert.equal(fallback.env.ASTROLABE_LSP_PYTHON, undefined);
});

test('Cursor rules render from the shared MCP L1 template', () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    const template = clientsMod.loadCursorL1Template();
    assert.match(template, /\{\{INDEX_ROOT\}\}/);
    const text = clientsMod.renderCursorRules({ template, projectRoot: project });
    assert.match(text, /^---\n/);
    assert.match(text, /description: .*Astrolabe/);
    assert.match(text, /alwaysApply: true/);
    assert.match(text, /astrolabe-find_symbol/);
    assert.ok(text.includes(clientsMod.canonicalizePath(project)), text);
    assert.ok(!text.includes('{{INDEX_ROOT}}'), text);

    const rulesPath = path.join(project, '.cursor', 'rules', 'astrolabe.mdc');
    const first = clientsMod.mergeCursorRulesFile(rulesPath, text, { existsFn: () => false });
    assert.equal(first.created, true);
    fs.mkdirSync(path.dirname(rulesPath), { recursive: true });
    fs.writeFileSync(rulesPath, text);
    const second = clientsMod.mergeCursorRulesFile(rulesPath, text, { existsFn: fs.existsSync });
    assert.equal(second.created, false);
    assert.equal(second.unchanged, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Cursor approvals and disabled lists are read/merged idempotently', () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    const approvalPath = clientsMod.resolveCursorApprovalPath(home, project);
    fs.mkdirSync(path.dirname(approvalPath), { recursive: true });
    const server = { command: '/bin/astrolabe', args: ['.'], env: {} };
    const key = cursorMod.approvalKey(project, server);
    fs.writeFileSync(approvalPath, JSON.stringify(['other-1', key]));
    const approval = clientsMod.checkCursorApprovalFile(approvalPath, { projectRoot: project, server });
    assert.equal(approval.exists, true);
    assert.equal(approval.approved, true);

    fs.writeFileSync(approvalPath, JSON.stringify(['other-1']));
    assert.equal(clientsMod.checkCursorApprovalFile(approvalPath).approved, false);

    const disabledPath = clientsMod.resolveCursorDisabledPath(home, project);
    fs.mkdirSync(path.dirname(disabledPath), { recursive: true });
    fs.writeFileSync(disabledPath, JSON.stringify(['astrolabe', 'astrolabe-abc123', 'astrolabe-deadbeef', 'other']));
    const first = clientsMod.removeAstrolabeFromCursorDisabled(disabledPath);
    assert.equal(first.changed, true);
    assert.deepEqual(first.doc, ['other']);
    fs.writeFileSync(disabledPath, JSON.stringify(first.doc));
    const second = clientsMod.removeAstrolabeFromCursorDisabled(disabledPath);
    assert.equal(second.changed, false);
    assert.deepEqual(second.doc, ['other']);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Cursor hooks use absolute command and astrolabe-* matcher idempotently', () => {
  const home = tmpHome();
  try {
    const hooksPath = clientsMod.resolveCursorHooksPath(home);
    fs.mkdirSync(path.dirname(hooksPath), { recursive: true });
    fs.writeFileSync(
      hooksPath,
      JSON.stringify(
        {
          version: 1,
          hooks: {
            preToolUse: [
              { command: 'astrolabe hooks remind --client=cursor', matcher: 'Shell|Read|Grep' },
              { command: 'echo keep', matcher: 'Read' },
            ],
            sessionEnd: [{ command: 'astrolabe hooks cleanup' }],
          },
        },
        null,
        2,
      ),
    );
    const bin = path.join(home, 'bin', 'astrolabe');
    const first = clientsMod.mergeCursorHooksFile(hooksPath, {
      create: true,
      existsFn: fs.existsSync,
      binPath: bin,
    });
    assert.equal(first.replaced, true);
    assert.equal(first.doc.hooks.preToolUse[0].command, `${bin} hooks remind --client=cursor`);
    assert.equal(first.doc.hooks.preToolUse[0].matcher, clientsMod.CURSOR_REMIND_MATCHER);
    assert.match(first.doc.hooks.preToolUse[0].matcher, /astrolabe-\.\*/);
    assert.equal(first.doc.hooks.preToolUse.length, 2);
    assert.equal(first.doc.hooks.preToolUse[1].command, 'echo keep');
    assert.equal(first.doc.hooks.sessionEnd[0].command, `${bin} hooks cleanup`);

    fs.writeFileSync(hooksPath, JSON.stringify(first.doc, null, 2));
    const second = clientsMod.mergeCursorHooksFile(hooksPath, {
      create: true,
      existsFn: fs.existsSync,
      binPath: bin,
    });
    assert.equal(
      second.doc.hooks.preToolUse.filter((e) =>
        String(e.command || '').includes('astrolabe hooks remind'),
      ).length,
      1,
    );
    assert.equal(
      second.doc.hooks.sessionEnd.filter((e) =>
        String(e.command || '').includes('astrolabe hooks cleanup'),
      ).length,
      1,
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test(
  'setup approves cursor-agent MCP and preserves other servers (idempotent)',
  { skip: process.platform === 'win32' },
  () => {
    const home = tmpHome();
    try {
      fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
      fs.writeFileSync(
        path.join(home, '.cursor', 'mcp.json'),
        JSON.stringify({ mcpServers: { keep: { command: 'keep' } } }, null, 2),
      );
      const project = tmpProject(home);
      const fakeBin = path.join(home, 'fakebin');
      fs.mkdirSync(fakeBin, { recursive: true });
      const fakeAgent = path.join(fakeBin, 'cursor-agent');
      fs.writeFileSync(
        fakeAgent,
        [
          '#!/usr/bin/env node',
          "'use strict';",
          "const fs = require('fs');",
          "const path = require('path');",
          "const slug = process.cwd().replace(/[^a-zA-Z0-9]/g, '-').replace(/-+/g, '-').replace(/^-+|-+$/g, '');",
          "const dir = path.join(process.env.HOME, '.cursor', 'projects', slug);",
          "fs.mkdirSync(dir, { recursive: true });",
          "const server = JSON.parse(fs.readFileSync(path.join(process.cwd(), '.cursor', 'mcp.json'), 'utf8')).mcpServers.astrolabe;",
          "const normalized = { command: server.command, args: server.args, env: server.env, cwd: server.cwd, startupTimeoutSec: server.startupTimeoutSec };",
          "const key = 'astrolabe-' + require('crypto').createHash('sha256').update(JSON.stringify({ path: process.cwd(), server: normalized })).digest('hex').slice(0, 16);",
          "fs.writeFileSync(path.join(dir, 'mcp-approvals.json'), JSON.stringify([key]));",
          "process.exit(0);",
          '',
        ].join('\n'),
      );
      fs.chmodSync(fakeAgent, 0o755);
      const bin = path.join(home, 'bin', 'astrolabe');
      const env = {
        ...process.env,
        HOME: home,
        PATH: `${fakeBin}${path.delimiter}${process.env.PATH || ''}`,
      };
      const args = [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--scope=project',
        '--with-hooks',
        '--home',
        home,
        '--project',
        project,
        '--command',
        bin,
        '--no-cursor-permissions',
      ];
      const result = spawnSync(process.execPath, args, { encoding: 'utf8', env });
      assert.equal(result.status, 0, result.stderr + result.stdout);
      assert.match(result.stdout, /Cursor CLI approval: approved astrolabe/);

      const approvalPath = clientsMod.resolveCursorApprovalPath(home, project);
      const server = JSON.parse(fs.readFileSync(path.join(project, '.cursor', 'mcp.json'), 'utf8')).mcpServers.astrolabe;
      const approval = clientsMod.checkCursorApprovalFile(approvalPath, { projectRoot: project, server });
      assert.equal(approval.approved, true);
      assert.ok(approval.entries.includes(cursorMod.approvalKey(project, server)));

      const globalDoc = JSON.parse(
        fs.readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8'),
      );
      assert.equal(globalDoc.mcpServers.keep.command, 'keep');
      assert.equal(globalDoc.mcpServers.astrolabe, undefined);

      const projectDoc = JSON.parse(
        fs.readFileSync(path.join(project, '.cursor', 'mcp.json'), 'utf8'),
      );
      assert.equal(projectDoc.mcpServers.astrolabe.cwd, clientsMod.canonicalizePath(project));

      const rerun = spawnSync(process.execPath, args, { encoding: 'utf8', env });
      assert.equal(rerun.status, 0, rerun.stderr + rerun.stdout);
      const hooks = JSON.parse(
        fs.readFileSync(path.join(home, '.cursor', 'hooks.json'), 'utf8'),
      );
      assert.equal(
        hooks.hooks.preToolUse.filter((e) =>
          String(e.command || '').includes('astrolabe hooks remind'),
        ).length,
        1,
      );
    } finally {
      fs.rmSync(home, { recursive: true, force: true });
    }
  },
);

test(
  'setup without cursor-agent removes astrolabe from mcp-disabled and prints guidance',
  { skip: process.platform === 'win32' },
  () => {
    const home = tmpHome();
    try {
      fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
      const project = tmpProject(home);
      const disabledPath = clientsMod.resolveCursorDisabledPath(home, project);
      fs.mkdirSync(path.dirname(disabledPath), { recursive: true });
      fs.writeFileSync(disabledPath, JSON.stringify(['astrolabe', 'astrolabe-abc123', 'astrolabe-deadbeef', 'other']));
      const emptyPath = path.join(home, 'empty-path');
      fs.mkdirSync(emptyPath, { recursive: true });
      const env = { ...process.env, HOME: home, PATH: emptyPath };
      const result = spawnSync(
        process.execPath,
        [
          BIN,
          'setup',
          '--yes',
          '--clients=cursor',
          '--home',
          home,
          '--project',
          project,
          '--command',
          path.join(home, 'bin', 'astrolabe'),
          '--no-cursor-permissions',
        ],
        { encoding: 'utf8', env },
      );
      assert.equal(result.status, 0, result.stderr + result.stdout);
      assert.match(result.stdout, /cursor-agent not found on PATH/);
      assert.match(result.stdout, /cursor-agent mcp enable astrolabe/);
      assert.deepEqual(JSON.parse(fs.readFileSync(disabledPath, 'utf8')), ['other']);
    } finally {
      fs.rmSync(home, { recursive: true, force: true });
    }
  },
);

test('Cursor hooks dedupe .exe/space/custom-binary commands on re-run', () => {
  const home = tmpHome();
  try {
    const hooksPath = clientsMod.resolveCursorHooksPath(home);
    fs.mkdirSync(path.dirname(hooksPath), { recursive: true });
    fs.writeFileSync(
      hooksPath,
      JSON.stringify(
        {
          version: 1,
          hooks: {
            preToolUse: [
              {
                command:
                  '"C:\\Program Files\\Astrolabe\\astrolabe.exe" hooks remind --client=cursor',
                matcher: 'Shell|Read|Grep',
              },
              { command: 'echo keep', matcher: 'Read' },
            ],
            sessionEnd: [{ command: 'C:\\Tools\\astrolabe.exe hooks cleanup' }],
          },
        },
        null,
        2,
      ),
    );
    const bin = 'C:\\Program Files\\Astrolabe\\astrolabe.exe';
    const first = clientsMod.mergeCursorHooksFile(hooksPath, {
      create: true,
      existsFn: fs.existsSync,
      binPath: bin,
    });
    assert.equal(first.replaced, true);
    assert.equal(
      first.doc.hooks.preToolUse.filter((e) =>
        /hooks\s+remind/.test(String(e.command || '')),
      ).length,
      1,
    );
    assert.equal(
      first.doc.hooks.sessionEnd.filter((e) =>
        /hooks\s+cleanup/.test(String(e.command || '')),
      ).length,
      1,
    );
    assert.equal(first.doc.hooks.preToolUse[0].matcher, clientsMod.CURSOR_REMIND_MATCHER);
    assert.ok(first.doc.hooks.preToolUse[0].command.includes(bin));

    fs.writeFileSync(hooksPath, JSON.stringify(first.doc, null, 2));
    const second = clientsMod.mergeCursorHooksFile(hooksPath, {
      create: true,
      existsFn: fs.existsSync,
      binPath: 'C:\\Tools\\custom-name.exe',
    });
    assert.equal(
      second.doc.hooks.preToolUse.filter((e) =>
        /hooks\s+remind/.test(String(e.command || '')),
      ).length,
      1,
      'second run must replace, not append',
    );
    assert.ok(second.doc.hooks.preToolUse[0].command.includes('custom-name.exe'));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('--command bare name resolves through PATH instead of cwd', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    const project = tmpProject(home);
    const fakeBin = path.join(home, 'fakebin');
    fs.mkdirSync(fakeBin, { recursive: true });
    const fakeAstrolabe = path.join(fakeBin, 'astrolabe');
    fs.writeFileSync(fakeAstrolabe, '#!/bin/sh\nexit 0\n');
    fs.chmodSync(fakeAstrolabe, 0o755);
    const env = { ...process.env, HOME: home, PATH: fakeBin };
    const result = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--home',
        home,
        '--project',
        project,
        '--command',
        'astrolabe',
        '--no-cursor-permissions',
      ],
      { encoding: 'utf8', env },
    );
    assert.equal(result.status, 0, result.stderr + result.stdout);
    const mcp = JSON.parse(fs.readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8'));
    assert.equal(mcp.mcpServers.astrolabe.command, fakeAstrolabe);
    assert.ok(path.isAbsolute(mcp.mcpServers.astrolabe.command));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('skipped cursor action does not run approval or permissions merge', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    const project = tmpProject(home);
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const env = { ...process.env, HOME: home, PATH: emptyPath };
    const result = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--home',
        home,
        '--project',
        project,
        '--command',
        'definitely-not-on-path',
      ],
      { encoding: 'utf8', env },
    );
    assert.equal(result.status, 0, result.stderr + result.stdout);
    assert.match(result.stdout, /SKIP/);
    assert.doesNotMatch(result.stdout, /Cursor CLI approval/);
    assert.doesNotMatch(result.stdout, /Cursor MCP allowlist check/);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'permissions.json')), false);
    assert.equal(
      fs.existsSync(clientsMod.resolveCursorApprovalPath(home, project)),
      false,
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('setup backs up hooks.json on a re-run with --with-hooks', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    const project = tmpProject(home);
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const env = { ...process.env, HOME: home, PATH: emptyPath };
    const args = [
      BIN,
      'setup',
      '--yes',
      '--clients=cursor',
      '--with-hooks',
      '--home',
      home,
      '--project',
      project,
      '--command',
      path.join(home, 'bin', 'astrolabe'),
      '--no-cursor-permissions',
    ];
    const first = spawnSync(process.execPath, args, { encoding: 'utf8', env });
    assert.equal(first.status, 0, first.stderr + first.stdout);
    const second = spawnSync(process.execPath, args, { encoding: 'utf8', env });
    assert.equal(second.status, 0, second.stderr + second.stdout);
    const backups = fs
      .readdirSync(path.join(home, '.cursor'))
      .filter((name) => name.startsWith('hooks.json.bak-'));
    assert.ok(backups.length >= 1, `expected hooks.json backup, got ${backups.join(', ')}`);
    const hooks = JSON.parse(
      fs.readFileSync(path.join(home, '.cursor', 'hooks.json'), 'utf8'),
    );
    assert.equal(
      hooks.hooks.preToolUse.filter((e) =>
        /hooks\s+remind/.test(String(e.command || '')),
      ).length,
      1,
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('enableCursorApproval sets timeout/stdio and shells Windows .cmd', () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    let captured = null;
    const spawnFn = (cmd, args, opts) => {
      captured = { cmd, args, opts };
      return { status: 0, stdout: '', stderr: '', signal: null };
    };
    const result = clientsMod.enableCursorApproval({
      home,
      projectRoot: project,
      env: { PATH: 'C:\\tools' },
      whichFn: () => 'C:\\Program Files\\Cursor\\cursor-agent.cmd',
      spawnFn,
      platform: 'win32',
    });
    assert.equal(result.available, true);
    assert.equal(captured.opts.timeout, 15000);
    assert.deepEqual(captured.opts.stdio, ['ignore', 'pipe', 'pipe']);
    assert.equal(captured.opts.shell, true);
    assert.deepEqual(captured.args, []);
    assert.match(captured.cmd, /^"C:\\Program Files\\Cursor\\cursor-agent\.cmd" mcp enable astrolabe$/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('loadL1Template falls back when the packaged template is absent', () => {
  const template = clientsMod.loadCursorL1Template({ existsFn: () => false });
  assert.match(template, /CRITICAL/);
  assert.match(template, /astrolabe-find_symbol/);
  assert.match(template, /\{\{INDEX_ROOT\}\}/);
});


test('Cursor approval requires the current hash, not a stale or bare server entry', () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    const file = path.join(home, 'approvals.json');
    const server = { type: 'stdio', command: '/bin/astrolabe', args: ['.'], env: { PATH: '/bin' } };
    const options = { projectRoot: project, server };
    const key = cursorMod.approvalKey(project, server);
    fs.writeFileSync(file, JSON.stringify(['astrolabe', 'astrolabe-old', 'other-1']));
    assert.equal(cursorMod.checkApprovalFile(file, options).approved, false);
    fs.writeFileSync(file, JSON.stringify([key]));
    assert.equal(cursorMod.checkApprovalFile(file, options).approved, true);
    assert.equal(cursorMod.checkApprovalFile(file).approved, false);
    for (const changed of [
      { ...server, command: '/new/astrolabe' },
      { ...server, args: ['.', '--context=cursor'] },
      { ...server, env: { PATH: '/new/bin' } },
    ]) {
      assert.equal(cursorMod.checkApprovalFile(file, { ...options, server: changed }).approved, false);
    }
    assert.equal(cursorMod.checkApprovalFile(file, {
      ...options, projectRoot: path.join(project, 'other'),
    }).approved, false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('failed or timed out enable never reports approval, even with a matching hash', () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    const server = { command: '/bin/astrolabe', args: ['.'], env: {} };
    fs.mkdirSync(path.join(project, '.cursor'));
    fs.writeFileSync(path.join(project, '.cursor', 'mcp.json'), JSON.stringify({ mcpServers: { astrolabe: server } }));
    const file = cursorMod.resolveApprovalPath(home, project);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, JSON.stringify([cursorMod.approvalKey(project, server)]));
    for (const result of [
      { status: 1 },
      { status: null, error: new Error('ETIMEDOUT'), signal: 'SIGTERM' },
      { status: 0, error: new Error('spawn failed') },
    ]) {
      const approval = cursorMod.enableCursorApproval({
        home, projectRoot: project, whichFn: () => '/fake/cursor-agent', spawnFn: () => result,
      });
      assert.equal(approval.approval.approved, false);
    }
    assert.equal(cursorMod.enableCursorApproval({
      home, projectRoot: project, whichFn: () => '/fake/cursor-agent', spawnFn: () => ({ status: 0 }),
    }).approval.approved, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Cursor hook migration preserves third-party subcommands and recognizes explicit custom binaries', () => {
  const home = tmpHome();
  try {
    const file = path.join(home, 'hooks.json');
    const thirdParty = [
      { command: '/opt/other-agent hooks remind --client=cursor' },
      { command: 'echo astrolabe hooks remind' },
      { command: 'echo /path/astrolabe hooks remind' },
      { command: '/opt/other-astrolabe hooks remind' },
      { hooks: [{ command: 'astrolabe hooks remind' }, { command: 'echo keep' }] },
    ];
    const cleanup = { command: '/opt/other-agent hooks cleanup' };
    fs.writeFileSync(file, JSON.stringify({ hooks: {
      preToolUse: [...thirdParty, { command: "'/old path/astrolabe' hooks remind --client=claude-code" }],
      sessionEnd: [cleanup, { command: 'astrolabe hooks cleanup' }],
    } }));
    const binPath = '/custom path/my-agent';
    const first = cursorMod.mergeCursorHooksFile(file, { binPath });
    assert.deepEqual(first.doc.hooks.preToolUse.slice(0, thirdParty.length), thirdParty);
    assert.deepEqual(first.doc.hooks.sessionEnd[0], cleanup);
    assert.equal(first.doc.hooks.preToolUse.length, thirdParty.length + 1);
    assert.equal(first.doc.hooks.sessionEnd.length, 2);
    fs.writeFileSync(file, JSON.stringify(first.doc));
    const second = cursorMod.mergeCursorHooksFile(file, { binPath });
    assert.deepEqual(second.doc, first.doc);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('project setup does not print approved after a failed enable with a stale hash', { skip: process.platform === 'win32' }, () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'));
    const project = tmpProject(home);
    const approvalPath = cursorMod.resolveApprovalPath(home, project);
    fs.mkdirSync(path.dirname(approvalPath), { recursive: true });
    fs.writeFileSync(approvalPath, JSON.stringify(['astrolabe-stale']));
    const fakeBin = path.join(home, 'fakebin');
    fs.mkdirSync(fakeBin);
    fs.writeFileSync(path.join(fakeBin, 'cursor-agent'), '#!/bin/sh\nexit 1\n', { mode: 0o755 });
    const result = spawnSync(process.execPath, [
      BIN, 'setup', '--yes', '--clients=cursor', '--scope=project', '--home', home,
      '--project', project, '--command', path.join(home, 'astrolabe'), '--no-cursor-permissions',
    ], { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: fakeBin } });
    assert.equal(result.status, 0, result.stderr + result.stdout);
    assert.match(result.stdout, /exited 1/);
    assert.doesNotMatch(result.stdout, /approval: approved|was approved/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});


test('disabled cleanup preserves other server names and non-string values', () => {
  const home = tmpHome();
  try {
    const file = path.join(home, 'disabled.json');
    fs.writeFileSync(file, JSON.stringify(['astrolabe', 'astrolabe-a1b2c3', 'astrolabe-helper', 'other-abc123', 3]));
    assert.deepEqual(cursorMod.removeAstrolabeFromDisabled(file).doc,
      ['astrolabe-helper', 'other-abc123', 3]);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('parseArgs defaults alwaysLoad to true and supports --no-always-load', () => {
  assert.equal(argsMod.parseArgs([]).alwaysLoad, true);
  assert.equal(argsMod.parseArgs(['--no-always-load']).alwaysLoad, false);
  assert.equal(argsMod.parseArgs(['--no-always-load', '--always-load']).alwaysLoad, true);
  assert.match(argsMod.USAGE, /--no-always-load/);
});

test('Claude Code defaults to alwaysLoad: true and --no-always-load omits/removes it', () => {
  const home = tmpHome();
  try {
    const claudeJsonPath = path.join(home, '.claude.json');
    fs.writeFileSync(
      claudeJsonPath,
      JSON.stringify(
        {
          numStartups: 5,
          mcpServers: {
            other: { type: 'stdio', command: 'other-mcp', args: [] },
          },
        },
        null,
        2,
      ),
    );
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });

    const first = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--clients=claude-code', '--home', home],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(first.status, 0, first.stderr + first.stdout);

    const doc1 = JSON.parse(fs.readFileSync(claudeJsonPath, 'utf8'));
    assert.equal(doc1.numStartups, 5);
    assert.equal(doc1.mcpServers.other.command, 'other-mcp');
    assert.equal(doc1.mcpServers.astrolabe.type, 'stdio');
    assert.equal(doc1.mcpServers.astrolabe.command, 'npx');
    assert.deepEqual(doc1.mcpServers.astrolabe.args, [
      '-y',
      '@zlnap/astrolabe',
      '.',
      '--context=claude-code',
    ]);
    assert.equal(doc1.mcpServers.astrolabe.alwaysLoad, true);
    assert.equal(fs.existsSync(path.join(home, '.claude', 'settings.json')), false);

    // Re-run with --no-always-load: removes alwaysLoad from existing entry
    const second = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--clients=claude-code', '--no-always-load', '--home', home],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(second.status, 0, second.stderr + second.stdout);

    const doc2 = JSON.parse(fs.readFileSync(claudeJsonPath, 'utf8'));
    assert.equal(doc2.mcpServers.other.command, 'other-mcp');
    assert.equal(doc2.mcpServers.astrolabe.alwaysLoad, undefined);
    assert.equal(Object.hasOwn(doc2.mcpServers.astrolabe, 'alwaysLoad'), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Claude Code --with-hooks merges ~/.claude/settings.json idempotently, preserves user hooks, updates paths, and creates backups', () => {
  const home = tmpHome();
  try {
    const claudeDir = path.join(home, '.claude');
    fs.mkdirSync(claudeDir, { recursive: true });
    const settingsPath = path.join(claudeDir, 'settings.json');
    const claudeJsonPath = path.join(home, '.claude.json');
    fs.writeFileSync(
      claudeJsonPath,
      JSON.stringify({ mcpServers: { keep: { command: 'keep', args: [] } } }, null, 2),
    );
    fs.writeFileSync(
      settingsPath,
      JSON.stringify(
        {
          model: 'sonnet',
          permissions: { allow: ['Bash(git status)'] },
          hooks: {
            SessionStart: [
              { matcher: 'startup', hooks: [{ type: 'command', command: 'echo session-keep' }] },
            ],
            PreToolUse: [
              { matcher: 'Bash', hooks: [{ type: 'command', command: 'echo pre-keep' }] },
              {
                matcher: 'Grep|Read',
                hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }],
              },
            ],
            PostToolUse: [
              { matcher: 'Edit', hooks: [{ type: 'command', command: 'echo post-keep' }] },
            ],
            SessionEnd: [
              { matcher: '', hooks: [{ type: 'command', command: 'echo end-keep' }] },
            ],
          },
        },
        null,
        2,
      ),
    );

    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const bin1 = path.join(home, 'bin', 'astrolabe');

    const run1 = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=claude-code',
        '--with-hooks',
        '--home',
        home,
        '--command',
        bin1,
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(run1.status, 0, run1.stderr + run1.stdout);

    const claudeDoc = JSON.parse(fs.readFileSync(claudeJsonPath, 'utf8'));
    assert.equal(claudeDoc.mcpServers.keep.command, 'keep');
    assert.equal(claudeDoc.mcpServers.astrolabe.command, bin1);
    assert.equal(claudeDoc.mcpServers.astrolabe.alwaysLoad, true);

    const s1 = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(s1.model, 'sonnet');
    assert.deepEqual(s1.permissions, { allow: ['Bash(git status)'] });
    assert.equal(s1.hooks.PostToolUse[0].hooks[0].command, 'echo post-keep');

    assert.equal(s1.hooks.SessionStart.length, 2);
    assert.equal(s1.hooks.SessionStart[0].hooks[0].command, 'echo session-keep');
    assert.deepEqual(s1.hooks.SessionStart[1], {
      matcher: '',
      hooks: [{ type: 'command', command: `${bin1} hooks session-start` }],
    });

    assert.equal(s1.hooks.PreToolUse.length, 2);
    assert.equal(s1.hooks.PreToolUse[0].hooks[0].command, 'echo pre-keep');
    assert.deepEqual(s1.hooks.PreToolUse[1], {
      matcher: clientsMod.CLAUDE_CODE_REMIND_MATCHER,
      hooks: [{ type: 'command', command: `${bin1} hooks remind --client=claude-code` }],
    });
    assert.match(s1.hooks.PreToolUse[1].matcher, /Grep/);
    assert.match(s1.hooks.PreToolUse[1].matcher, /Read/);
    assert.match(s1.hooks.PreToolUse[1].matcher, /Bash/);
    assert.match(s1.hooks.PreToolUse[1].matcher, /Glob/);
    assert.match(s1.hooks.PreToolUse[1].matcher, /mcp__astrolabe__\.\*/);

    assert.equal(s1.hooks.SessionEnd.length, 2);
    assert.equal(s1.hooks.SessionEnd[0].hooks[0].command, 'echo end-keep');
    assert.deepEqual(s1.hooks.SessionEnd[1], {
      matcher: '',
      hooks: [{ type: 'command', command: `${bin1} hooks cleanup` }],
    });

    // Backups created for both existing files
    assert.ok(fs.readdirSync(home).some((f) => f.startsWith('.claude.json.bak-')));
    assert.ok(fs.readdirSync(claudeDir).some((f) => f.startsWith('settings.json.bak-')));

    // Second run with the same command is completely idempotent
    const run2 = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=claude-code',
        '--with-hooks',
        '--home',
        home,
        '--command',
        bin1,
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(run2.status, 0, run2.stderr + run2.stdout);
    const s2 = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.deepEqual(s2, s1);

    // Third run with a new command path (including spaces) updates existing hooks in place
    const bin2 = path.join(home, 'bin dir', 'astrolabe');
    const run3 = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=claude-code',
        '--with-hooks',
        '--home',
        home,
        '--command',
        bin2,
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(run3.status, 0, run3.stderr + run3.stdout);
    const s3 = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    const quotedBin2 = process.platform === 'win32' ? `"${bin2}"` : `'${bin2}'`;
    assert.equal(s3.hooks.SessionStart.length, 2);
    assert.equal(s3.hooks.SessionStart[1].hooks[0].command, `${quotedBin2} hooks session-start`);
    assert.equal(s3.hooks.PreToolUse.length, 2);
    assert.equal(
      s3.hooks.PreToolUse[1].hooks[0].command,
      `${quotedBin2} hooks remind --client=claude-code`,
    );
    assert.equal(s3.hooks.SessionEnd.length, 2);
    assert.equal(s3.hooks.SessionEnd[1].hooks[0].command, `${quotedBin2} hooks cleanup`);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Claude Code --with-hooks resolves astrolabe on PATH when --command is omitted and falls back to bare astrolabe', () => {
  const home = tmpHome();
  try {
    const fakeBin = path.join(home, 'fakebin');
    fs.mkdirSync(fakeBin, { recursive: true });
    const binName = process.platform === 'win32' ? 'astrolabe.exe' : 'astrolabe';
    const onPathBin = path.join(fakeBin, binName);
    fs.writeFileSync(onPathBin, '');

    const client = clientsMod.getClient('claude-code');
    const launch = clientsMod.resolveLaunch({ context: client.context });

    // With astrolabe on PATH -> absolute path in hooks
    const applied1 = client
      .plan({ home, launch, withHooks: true, env: { ...process.env, PATH: fakeBin } })
      .apply();
    applied1.write();

    const settingsPath = clientsMod.resolveClaudeSettingsPath(home);
    const s1 = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(s1.hooks.SessionStart[0].hooks[0].command, `${onPathBin} hooks session-start`);
    assert.equal(
      s1.hooks.PreToolUse[0].hooks[0].command,
      `${onPathBin} hooks remind --client=claude-code`,
    );
    assert.equal(s1.hooks.SessionEnd[0].hooks[0].command, `${onPathBin} hooks cleanup`);

    // Without astrolabe on PATH -> falls back to bare 'astrolabe' and replaces in place
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const applied2 = client
      .plan({ home, launch, withHooks: true, env: { ...process.env, PATH: emptyPath } })
      .apply();
    applied2.write();

    const s2 = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(s2.hooks.SessionStart.length, 1);
    assert.equal(s2.hooks.SessionStart[0].hooks[0].command, 'astrolabe hooks session-start');
    assert.equal(s2.hooks.PreToolUse.length, 1);
    assert.equal(
      s2.hooks.PreToolUse[0].hooks[0].command,
      'astrolabe hooks remind --client=claude-code',
    );
    assert.equal(s2.hooks.SessionEnd.length, 1);
    assert.equal(s2.hooks.SessionEnd[0].hooks[0].command, 'astrolabe hooks cleanup');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Claude Code hook merge handles multi-hook groups, duplicates, third-party hooks, BOM, and --dry-run', () => {
  const home = tmpHome();
  try {
    const claudeDir = path.join(home, '.claude');
    fs.mkdirSync(claudeDir, { recursive: true });
    const settingsPath = path.join(claudeDir, 'settings.json');
    const thirdParty = {
      matcher: '*',
      hooks: [{ type: 'command', command: '/opt/other-agent hooks remind --client=claude-code' }],
    };
    const mixedGroup = {
      matcher: '',
      hooks: [
        { type: 'command', command: 'echo keep-grouped' },
        { type: 'command', command: 'astrolabe hooks cleanup' },
      ],
    };
    fs.writeFileSync(
      settingsPath,
      `${String.fromCharCode(0xfeff)}${JSON.stringify(
        {
          hooks: {
            SessionStart: [
              { matcher: '', hooks: [{ type: 'command', command: '/old/astrolabe hooks session-start' }] },
              { matcher: '', hooks: [{ type: 'command', command: 'astrolabe hooks session-start' }] },
            ],
            PreToolUse: [
              thirdParty,
              { matcher: 'Read', hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }] },
            ],
            SessionEnd: [mixedGroup],
          },
        },
        null,
        2,
      )}`,
    );

    const dry = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=claude-code',
        '--with-hooks',
        '--dry-run',
        '--home',
        home,
        '--command',
        '/new/astrolabe',
      ],
      { encoding: 'utf8' },
    );
    assert.equal(dry.status, 0, dry.stderr + dry.stdout);
    assert.match(dry.stdout, /\[dry-run\]/);
    assert.equal(fs.existsSync(path.join(home, '.claude.json')), false);
    assert.match(fs.readFileSync(settingsPath, 'utf8'), /\/old\/astrolabe/);

    const merged = clientsMod.mergeClaudeCodeHooksSettings(settingsPath, {
      binPath: '/new/astrolabe',
    });
    assert.equal(merged.replaced, true);
    assert.equal(merged.doc.hooks.SessionStart.length, 1);
    assert.equal(
      merged.doc.hooks.SessionStart[0].hooks[0].command,
      '/new/astrolabe hooks session-start',
    );
    assert.equal(merged.doc.hooks.PreToolUse.length, 2);
    assert.deepEqual(merged.doc.hooks.PreToolUse[0], thirdParty);
    assert.equal(
      merged.doc.hooks.PreToolUse[1].hooks[0].command,
      '/new/astrolabe hooks remind --client=claude-code',
    );
    assert.equal(merged.doc.hooks.SessionEnd.length, 2);
    assert.deepEqual(merged.doc.hooks.SessionEnd[0], {
      matcher: '',
      hooks: [{ type: 'command', command: 'echo keep-grouped' }],
    });
    assert.equal(
      merged.doc.hooks.SessionEnd[1].hooks[0].command,
      '/new/astrolabe hooks cleanup',
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('npx environment without astrolabe on PATH resolves packaged binary for claude-code --with-hooks and falls back when resolveBinary fails', async () => {
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const cacheDir = path.join(home, 'npx-cache');
    fs.mkdirSync(cacheDir, { recursive: true });
    const binName = process.platform === 'win32' ? 'astrolabe.exe' : 'astrolabe';
    const packagedBin = path.join(cacheDir, binName);
    fs.writeFileSync(packagedBin, '');

    const res = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--clients=claude-code', '--with-hooks', '--home', home],
      {
        encoding: 'utf8',
        env: {
          ...process.env,
          HOME: home,
          PATH: emptyPath,
          ASTROLABE_BINARY_PATH: packagedBin,
        },
      },
    );
    assert.equal(res.status, 0, res.stderr + res.stdout);

    const claudeDoc = JSON.parse(fs.readFileSync(path.join(home, '.claude.json'), 'utf8'));
    assert.equal(claudeDoc.mcpServers.astrolabe.command, 'npx');
    assert.deepEqual(claudeDoc.mcpServers.astrolabe.args, [
      '-y',
      '@zlnap/astrolabe',
      '.',
      '--context=claude-code',
    ]);

    const quotedPackaged = cursorMod.quoteCommandPath(packagedBin);
    const settingsPath = clientsMod.resolveClaudeSettingsPath(home);
    const settings = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(settings.hooks.SessionStart[0].hooks[0].command, `${quotedPackaged} hooks session-start`);
    assert.equal(
      settings.hooks.PreToolUse[0].hooks[0].command,
      `${quotedPackaged} hooks remind --client=claude-code`,
    );
    assert.equal(settings.hooks.SessionEnd[0].hooks[0].command, `${quotedPackaged} hooks cleanup`);

    // Fallback: when resolveBinary() throws and PATH has no astrolabe, fall back to bare 'astrolabe'.
    const home2 = tmpHome();
    const resolvePath = require.resolve('./resolve');
    const prevResolveCache = require.cache[resolvePath];
    const prevEnv = {
      PATH: process.env.PATH,
      Path: process.env.Path,
      HOME: process.env.HOME,
      ASTROLABE_BINARY_PATH: process.env.ASTROLABE_BINARY_PATH,
    };
    try {
      const emptyPath2 = path.join(home2, 'empty-path');
      fs.mkdirSync(emptyPath2, { recursive: true });
      process.env.PATH = emptyPath2;
      process.env.Path = emptyPath2;
      process.env.HOME = home2;
      delete process.env.ASTROLABE_BINARY_PATH;
      require.cache[resolvePath] = {
        id: resolvePath,
        filename: resolvePath,
        loaded: true,
        exports: {
          resolveBinary: async () => {
            throw new Error('packaged binary missing');
          },
        },
      };
      const code = await runMod.run([
        '--yes',
        '--clients=claude-code',
        '--with-hooks',
        '--home',
        home2,
      ]);
      assert.equal(code, 0);
      const fallbackSettings = JSON.parse(
        fs.readFileSync(clientsMod.resolveClaudeSettingsPath(home2), 'utf8'),
      );
      assert.equal(fallbackSettings.hooks.SessionStart[0].hooks[0].command, 'astrolabe hooks session-start');
      assert.equal(
        fallbackSettings.hooks.PreToolUse[0].hooks[0].command,
        'astrolabe hooks remind --client=claude-code',
      );
      assert.equal(fallbackSettings.hooks.SessionEnd[0].hooks[0].command, 'astrolabe hooks cleanup');
    } finally {
      if (prevResolveCache) require.cache[resolvePath] = prevResolveCache;
      else delete require.cache[resolvePath];
      for (const key of Object.keys(prevEnv)) {
        if (prevEnv[key] === undefined) delete process.env[key];
        else process.env[key] = prevEnv[key];
      }
      fs.rmSync(home2, { recursive: true, force: true });
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("Unix single-quoted hook paths (/Users/o'neill/bin/astrolabe) are recognized, deduplicated, and migrated idempotently", () => {
  const home = tmpHome();
  try {
    const bin1 = "/Users/o'neill/bin/astrolabe";
    const bin2 = '/usr/local/bin/astrolabe';
    const quoted1 = "'/Users/o'\\''neill/bin/astrolabe'";
    assert.equal(cursorMod.quoteCommandPath(bin1, 'darwin'), quoted1);
    assert.equal(cursorMod.unquoteHookExecutable(quoted1), bin1);
    assert.equal(
      claudeCodeMod.isAstrolabeHookCommand(
        `${quoted1} hooks remind --client=claude-code`,
        'remind',
        bin2,
        'darwin',
      ),
      true,
    );
    assert.equal(
      claudeCodeMod.isAstrolabeHookCommand(
        `echo ${quoted1} hooks remind --client=claude-code`,
        'remind',
        bin2,
        'darwin',
      ),
      false,
    );

    // Claude Code: write with single-quote path, re-run idempotently, then migrate to bin2
    const claudeSettings = path.join(home, '.claude', 'settings.json');
    fs.mkdirSync(path.dirname(claudeSettings), { recursive: true });
    const thirdPartyClaude = {
      matcher: 'Bash',
      hooks: [{ type: 'command', command: `echo ${quoted1} hooks remind` }],
    };
    fs.writeFileSync(
      claudeSettings,
      JSON.stringify({ hooks: { PreToolUse: [thirdPartyClaude] } }, null, 2),
    );

    const c1 = claudeCodeMod.mergeClaudeCodeHooksSettings(claudeSettings, {
      binPath: bin1,
      platform: 'darwin',
    });
    fs.writeFileSync(claudeSettings, JSON.stringify(c1.doc, null, 2));
    assert.equal(c1.doc.hooks.SessionStart[0].hooks[0].command, `${quoted1} hooks session-start`);
    assert.equal(
      c1.doc.hooks.PreToolUse[1].hooks[0].command,
      `${quoted1} hooks remind --client=claude-code`,
    );
    assert.equal(c1.doc.hooks.SessionEnd[0].hooks[0].command, `${quoted1} hooks cleanup`);

    const c2 = claudeCodeMod.mergeClaudeCodeHooksSettings(claudeSettings, {
      binPath: bin1,
      platform: 'darwin',
    });
    assert.deepEqual(c2.doc, c1.doc);

    const c3 = claudeCodeMod.mergeClaudeCodeHooksSettings(claudeSettings, {
      binPath: bin2,
      platform: 'darwin',
    });
    assert.equal(c3.replaced, true);
    assert.equal(c3.doc.hooks.SessionStart.length, 1);
    assert.equal(c3.doc.hooks.SessionStart[0].hooks[0].command, `${bin2} hooks session-start`);
    assert.equal(c3.doc.hooks.PreToolUse.length, 2);
    assert.deepEqual(c3.doc.hooks.PreToolUse[0], thirdPartyClaude);
    assert.equal(
      c3.doc.hooks.PreToolUse[1].hooks[0].command,
      `${bin2} hooks remind --client=claude-code`,
    );
    assert.equal(c3.doc.hooks.SessionEnd.length, 1);
    assert.equal(c3.doc.hooks.SessionEnd[0].hooks[0].command, `${bin2} hooks cleanup`);

    // Cursor: write with single-quote path, re-run idempotently, then migrate to bin2
    const cursorHooks = path.join(home, '.cursor', 'hooks.json');
    fs.mkdirSync(path.dirname(cursorHooks), { recursive: true });
    const thirdPartyCursor = { command: `echo ${quoted1} hooks remind` };
    fs.writeFileSync(
      cursorHooks,
      JSON.stringify({ version: 1, hooks: { preToolUse: [thirdPartyCursor] } }, null, 2),
    );

    const k1 = cursorMod.mergeCursorHooksFile(cursorHooks, {
      binPath: bin1,
      platform: 'darwin',
    });
    fs.writeFileSync(cursorHooks, JSON.stringify(k1.doc, null, 2));
    assert.equal(
      k1.doc.hooks.preToolUse[1].command,
      `${quoted1} hooks remind --client=cursor`,
    );
    assert.equal(k1.doc.hooks.sessionEnd[0].command, `${quoted1} hooks cleanup`);

    const k2 = cursorMod.mergeCursorHooksFile(cursorHooks, {
      binPath: bin1,
      platform: 'darwin',
    });
    assert.deepEqual(k2.doc, k1.doc);

    const k3 = cursorMod.mergeCursorHooksFile(cursorHooks, {
      binPath: bin2,
      platform: 'darwin',
    });
    assert.equal(k3.replaced, true);
    assert.equal(k3.doc.hooks.preToolUse.length, 2);
    assert.deepEqual(k3.doc.hooks.preToolUse[0], thirdPartyCursor);
    assert.equal(k3.doc.hooks.preToolUse[1].command, `${bin2} hooks remind --client=cursor`);
    assert.equal(k3.doc.hooks.sessionEnd.length, 1);
    assert.equal(k3.doc.hooks.sessionEnd[0].command, `${bin2} hooks cleanup`);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Windows cmd.exe metacharacter paths (C:\\Tools(x86)\\A&B\\astrolabe.exe) are double-quoted and recognized idempotently', () => {
  const home = tmpHome();
  try {
    const safeWin = 'C:\\Tools\\astrolabe.exe';
    assert.equal(cursorMod.quoteCommandPath(safeWin, 'win32'), safeWin);

    const metaWin = 'C:\\Tools(x86)\\A&B\\astrolabe.exe';
    assert.equal(cursorMod.quoteCommandPath(metaWin, 'win32'), `"${metaWin}"`);
    assert.equal(
      cursorMod.quoteCommandPath('C:\\a^b\\c%d\\e!f\\astrolabe.exe', 'win32'),
      '"C:\\a^b\\c%d\\e!f\\astrolabe.exe"',
    );
    assert.equal(
      cursorMod.quoteCommandPath('C:\\Users\\测试\\astrolabe.exe', 'win32'),
      '"C:\\Users\\测试\\astrolabe.exe"',
    );
    assert.throws(
      () => cursorMod.quoteCommandPath('C:\\bad"path\\astrolabe.exe', 'win32'),
      /double quote/i,
    );

    // Cursor hooks: write, re-run idempotently, and migrate to safeWin
    const cursorHooks = path.join(home, '.cursor', 'hooks.json');
    fs.mkdirSync(path.dirname(cursorHooks), { recursive: true });
    const k1 = cursorMod.mergeCursorHooksFile(cursorHooks, {
      binPath: metaWin,
      platform: 'win32',
    });
    fs.writeFileSync(cursorHooks, JSON.stringify(k1.doc, null, 2));
    assert.equal(
      k1.doc.hooks.preToolUse[0].command,
      `"${metaWin}" hooks remind --client=cursor`,
    );
    assert.equal(k1.doc.hooks.sessionEnd[0].command, `"${metaWin}" hooks cleanup`);

    const k2 = cursorMod.mergeCursorHooksFile(cursorHooks, {
      binPath: metaWin,
      platform: 'win32',
    });
    assert.equal(k2.replaced, true);
    assert.deepEqual(k2.doc, k1.doc);

    const k3 = cursorMod.mergeCursorHooksFile(cursorHooks, {
      binPath: safeWin,
      platform: 'win32',
    });
    assert.equal(k3.replaced, true);
    assert.equal(k3.doc.hooks.preToolUse.length, 1);
    assert.equal(k3.doc.hooks.preToolUse[0].command, `${safeWin} hooks remind --client=cursor`);
    assert.equal(k3.doc.hooks.sessionEnd.length, 1);
    assert.equal(k3.doc.hooks.sessionEnd[0].command, `${safeWin} hooks cleanup`);

    // Claude Code hooks: write, re-run idempotently, and migrate to safeWin
    const claudeSettings = path.join(home, '.claude', 'settings.json');
    fs.mkdirSync(path.dirname(claudeSettings), { recursive: true });
    const c1 = claudeCodeMod.mergeClaudeCodeHooksSettings(claudeSettings, {
      binPath: metaWin,
      platform: 'win32',
    });
    fs.writeFileSync(claudeSettings, JSON.stringify(c1.doc, null, 2));
    assert.equal(c1.doc.hooks.SessionStart[0].hooks[0].command, `"${metaWin}" hooks session-start`);
    assert.equal(
      c1.doc.hooks.PreToolUse[0].hooks[0].command,
      `"${metaWin}" hooks remind --client=claude-code`,
    );
    assert.equal(c1.doc.hooks.SessionEnd[0].hooks[0].command, `"${metaWin}" hooks cleanup`);

    const c2 = claudeCodeMod.mergeClaudeCodeHooksSettings(claudeSettings, {
      binPath: metaWin,
      platform: 'win32',
    });
    assert.equal(c2.replaced, true);
    assert.deepEqual(c2.doc, c1.doc);

    const c3 = claudeCodeMod.mergeClaudeCodeHooksSettings(claudeSettings, {
      binPath: safeWin,
      platform: 'win32',
    });
    assert.equal(c3.replaced, true);
    assert.equal(c3.doc.hooks.SessionStart.length, 1);
    assert.equal(c3.doc.hooks.SessionStart[0].hooks[0].command, `${safeWin} hooks session-start`);
    assert.equal(c3.doc.hooks.PreToolUse.length, 1);
    assert.equal(
      c3.doc.hooks.PreToolUse[0].hooks[0].command,
      `${safeWin} hooks remind --client=claude-code`,
    );
    assert.equal(c3.doc.hooks.SessionEnd.length, 1);
    assert.equal(c3.doc.hooks.SessionEnd[0].hooks[0].command, `${safeWin} hooks cleanup`);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Codebuddy, Grok, Kimi Code, ZCode, and OpenCode hooks align absolute binPath, quoting, migration, and idempotency', () => {
  const home = tmpHome();
  try {
    const posixBin = "/Users/o'neill/My Bin/astrolabe";
    const posixQuoted = "'/Users/o'\\''neill/My Bin/astrolabe'";
    const winBin = 'C:\\Tools(x86)\\A&B\\astrolabe.exe';
    const winQuoted = `"${winBin}"`;

    // 1. Codebuddy: migrate bare + duplicate + mixed group, POSIX single-quote & Windows .exe, idempotent
    const cbSettings = path.join(home, 'codebuddy-settings.json');
    const cbThirdParty = {
      matcher: 'Bash',
      hooks: [{ type: 'command', command: 'echo keep-cb' }],
    };
    const cbMixed = {
      matcher: 'Read',
      hooks: [
        { type: 'command', command: 'echo keep-mixed' },
        { type: 'command', command: 'astrolabe hooks remind --client=codebuddy' },
      ],
    };
    fs.writeFileSync(
      cbSettings,
      JSON.stringify(
        {
          hooks: {
            PreToolUse: [
              cbThirdParty,
              cbMixed,
              { matcher: '', hooks: [{ type: 'command', command: '/old/astrolabe.exe hooks remind --client=codebuddy' }] },
            ],
            SessionEnd: [
              { matcher: '', hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] },
              {
                matcher: '',
                hooks: [
                  { type: 'command', command: 'echo keep-end' },
                  { type: 'command', command: '"/old/my bin/astrolabe.cmd" hooks cleanup' },
                ],
              },
            ],
          },
        },
        null,
        2,
      ),
    );
    const cb1 = codebuddyMod.mergeCodebuddyHooksSettings(cbSettings, {
      binPath: posixBin,
      platform: 'darwin',
    });
    assert.equal(cb1.replaced, true);
    assert.equal(cb1.doc.hooks.PreToolUse.length, 3);
    assert.deepEqual(cb1.doc.hooks.PreToolUse[0], cbThirdParty);
    assert.deepEqual(cb1.doc.hooks.PreToolUse[1], {
      matcher: 'Read',
      hooks: [{ type: 'command', command: 'echo keep-mixed' }],
    });
    assert.equal(
      cb1.doc.hooks.PreToolUse[2].hooks[0].command,
      `${posixQuoted} hooks remind --client=codebuddy`,
    );
    assert.equal(cb1.doc.hooks.SessionEnd.length, 2);
    assert.equal(cb1.doc.hooks.SessionEnd[0].hooks[0].command, `${posixQuoted} hooks cleanup`);
    assert.deepEqual(cb1.doc.hooks.SessionEnd[1], {
      matcher: '',
      hooks: [{ type: 'command', command: 'echo keep-end' }],
    });
    fs.writeFileSync(cbSettings, JSON.stringify(cb1.doc, null, 2));
    const cb2 = codebuddyMod.mergeCodebuddyHooksSettings(cbSettings, {
      binPath: posixBin,
      platform: 'darwin',
    });
    assert.deepEqual(cb2.doc, cb1.doc);

    const cbWin = codebuddyMod.mergeCodebuddyHooksSettings(cbSettings, {
      binPath: winBin,
      platform: 'win32',
    });
    assert.equal(cbWin.replaced, true);
    assert.equal(
      cbWin.doc.hooks.PreToolUse[2].hooks[0].command,
      `${winQuoted} hooks remind --client=codebuddy`,
    );
    assert.equal(cbWin.doc.hooks.SessionEnd[0].hooks[0].command, `${winQuoted} hooks cleanup`);

    // 2. Grok: POSIX & Windows quoted absolute paths + idempotency + preserve top-level keys
    const grokFile = path.join(home, 'grok-astrolabe.json');
    fs.writeFileSync(grokFile, JSON.stringify({ customKey: 'keep', hooks: {} }, null, 2));
    const g1 = grokMod.mergeGrokHooksFile(grokFile, { binPath: posixBin, platform: 'darwin' });
    assert.equal(g1.doc.customKey, 'keep');
    assert.equal(
      g1.doc.hooks.PreToolUse[0].hooks[0].command,
      `${posixQuoted} hooks remind --client=grok`,
    );
    assert.equal(g1.doc.hooks.SessionEnd[0].hooks[0].command, `${posixQuoted} hooks cleanup`);
    fs.writeFileSync(grokFile, JSON.stringify(g1.doc, null, 2));
    const g2 = grokMod.mergeGrokHooksFile(grokFile, { binPath: winBin, platform: 'win32' });
    assert.equal(
      g2.doc.hooks.PreToolUse[0].hooks[0].command,
      `${winQuoted} hooks remind --client=grok`,
    );
    assert.equal(g2.doc.hooks.SessionEnd[0].hooks[0].command, `${winQuoted} hooks cleanup`);
    fs.writeFileSync(grokFile, JSON.stringify(g2.doc, null, 2));
    const g3 = grokMod.mergeGrokHooksFile(grokFile, { binPath: winBin, platform: 'win32' });
    assert.deepEqual(g3.doc, g2.doc);

    // 3. Kimi Code: TOML basic string escaping for Windows backslash + quotes, migration, idempotent
    const kimiToml = path.join(home, 'kimi-config.toml');
    fs.writeFileSync(
      kimiToml,
      `default_model = "kimi"\n\n${kimicodeMod.renderKimiManagedBlock()}\n`,
    );
    const kWin = kimicodeMod.mergeKimiHooksToml(kimiToml, {
      binPath: winBin,
      platform: 'win32',
    });
    assert.equal(kWin.replaced, true);
    const kMatches = [...kWin.text.matchAll(/^command = (".*")$/gm)];
    assert.equal(kMatches.length, 2);
    // Verify the TOML basic strings unescape back to exact Windows cmd command strings.
    assert.equal(JSON.parse(kMatches[0][1]), `${winQuoted} hooks remind --client=kimicode`);
    assert.equal(JSON.parse(kMatches[1][1]), `${winQuoted} hooks cleanup`);
    fs.writeFileSync(kimiToml, kWin.text);
    const kIdemp = kimicodeMod.mergeKimiHooksToml(kimiToml, {
      binPath: winBin,
      platform: 'win32',
    });
    assert.equal(kIdemp.text, kWin.text);

    const kPosix = kimicodeMod.mergeKimiHooksToml(kimiToml, {
      binPath: posixBin,
      platform: 'darwin',
    });
    const kPosixMatches = [...kPosix.text.matchAll(/^command = (".*")$/gm)];
    assert.equal(JSON.parse(kPosixMatches[0][1]), `${posixQuoted} hooks remind --client=kimicode`);

    // 4. ZCode: migrate quoted/.exe/wrong-client/duplicates + mixed group + Stop cleanup scrub
    const zcodeFile = path.join(home, 'zcode-config.json');
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    fs.writeFileSync(
      zcodeFile,
      JSON.stringify(
        {
          mcp: { servers: {} },
          hooks: {
            enabled: true,
            events: {
              PreToolUse: [
                {
                  matcher: '*',
                  hooks: [{ type: 'command', command: `${posixQuoted} hooks remind --client=claude-code` }],
                },
                {
                  matcher: '*',
                  hooks: [
                    { type: 'command', command: 'echo keep-zcode-mixed' },
                    { type: 'command', command: `${winQuoted} hooks remind --client=zcode` },
                  ],
                },
              ],
              Stop: [
                {
                  matcher: '*',
                  hooks: [
                    { type: 'command', command: `${winQuoted} hooks cleanup` },
                    { type: 'command', command: 'echo keep-zcode-stop' },
                  ],
                },
              ],
            },
          },
        },
        null,
        2,
      ),
    );
    const z1 = zcodeMod.mergeZcodeConfigFile(zcodeFile, launch, {
      withHooks: true,
      binPath: posixBin,
      platform: 'darwin',
    });
    assert.equal(z1.replaced, true);
    assert.equal(z1.doc.hooks.events.PreToolUse.length, 2);
    assert.equal(
      z1.doc.hooks.events.PreToolUse[0].hooks[0].command,
      `${posixQuoted} hooks remind --client=zcode`,
    );
    assert.deepEqual(z1.doc.hooks.events.PreToolUse[1].hooks, [
      { type: 'command', command: 'echo keep-zcode-mixed' },
    ]);
    assert.deepEqual(z1.doc.hooks.events.Stop[0].hooks, [
      { type: 'command', command: 'echo keep-zcode-stop' },
    ]);
    fs.writeFileSync(zcodeFile, JSON.stringify(z1.doc, null, 2));
    const z2 = zcodeMod.mergeZcodeConfigFile(zcodeFile, launch, {
      withHooks: true,
      binPath: posixBin,
      platform: 'darwin',
    });
    assert.deepEqual(z2.doc, z1.doc);

    // 5. OpenCode: ASTROLABE_BIN in Bun.spawnSync argv array uses raw un-shell-quoted JSON string
    const ocPosix = opencodeMod.planOpenCodeGuardPlugin(home, fs.existsSync, {
      binPath: posixBin,
    }).apply();
    const posixLiteral = ocPosix.text.match(/^const ASTROLABE_BIN = (.*);$/m);
    assert.ok(posixLiteral, 'ASTROLABE_BIN declaration should exist');
    assert.equal(JSON.parse(posixLiteral[1]), posixBin);
    assert.match(ocPosix.text, /Bun\.spawnSync\(\[ASTROLABE_BIN, "hooks", "remind", "--client=opencode"\]/);
    assert.ok(!ocPosix.text.includes("'\\''"), 'OpenCode argv must not contain POSIX shell quotes');

    const ocWin = opencodeMod.planOpenCodeGuardPlugin(home, fs.existsSync, {
      binPath: winBin,
    }).apply();
    const winLiteral = ocWin.text.match(/^const ASTROLABE_BIN = (.*);$/m);
    assert.ok(winLiteral);
    assert.equal(JSON.parse(winLiteral[1]), winBin);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('run --with-hooks resolves ASTROLABE_BINARY_PATH for codebuddy/grok/kimicode/zcode/opencode when PATH has no astrolabe', () => {
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const packagedBin = path.join(home, 'pkg dir', process.platform === 'win32' ? 'astrolabe.exe' : 'astrolabe');
    fs.mkdirSync(path.dirname(packagedBin), { recursive: true });
    fs.writeFileSync(packagedBin, '');

    fs.mkdirSync(path.join(home, '.codebuddy'), { recursive: true });
    fs.mkdirSync(path.join(home, '.grok'), { recursive: true });
    fs.mkdirSync(path.join(home, '.kimi-code'), { recursive: true });
    fs.mkdirSync(path.join(home, '.zcode', 'cli'), { recursive: true });
    fs.mkdirSync(path.join(home, '.config', 'opencode'), { recursive: true });

    const res = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=codebuddy,grok,kimicode,zcode,opencode',
        '--with-hooks',
        '--home',
        home,
      ],
      {
        encoding: 'utf8',
        env: {
          ...process.env,
          HOME: home,
          PATH: emptyPath,
          ASTROLABE_BINARY_PATH: packagedBin,
        },
      },
    );
    assert.equal(res.status, 0, res.stderr + res.stdout);

    const quoted = cursorMod.quoteCommandPath(packagedBin);
    const cb = JSON.parse(fs.readFileSync(path.join(home, '.codebuddy', 'settings.json'), 'utf8'));
    assert.equal(cb.hooks.PreToolUse[0].hooks[0].command, `${quoted} hooks remind --client=codebuddy`);
    const grok = JSON.parse(fs.readFileSync(path.join(home, '.grok', 'hooks', 'astrolabe.json'), 'utf8'));
    assert.equal(grok.hooks.PreToolUse[0].hooks[0].command, `${quoted} hooks remind --client=grok`);
    const kimi = fs.readFileSync(path.join(home, '.kimi-code', 'config.toml'), 'utf8');
    assert.ok(kimi.includes(JSON.stringify(`${quoted} hooks remind --client=kimicode`)));
    const zcode = JSON.parse(fs.readFileSync(path.join(home, '.zcode', 'cli', 'config.json'), 'utf8'));
    assert.equal(zcode.hooks.events.PreToolUse[0].hooks[0].command, `${quoted} hooks remind --client=zcode`);
    const ocPlugin = fs.readFileSync(
      path.join(home, '.config', 'opencode', 'plugins', 'astrolabe-guard.ts'),
      'utf8',
    );
    assert.ok(ocPlugin.includes(`const ASTROLABE_BIN = ${JSON.stringify(packagedBin)};`));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Standard harness env vars override detect() and plan() across all clients with trim and ~ expansion', () => {
  const home = tmpHome();
  try {
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };

    // Claude Code: CLAUDE_CONFIG_DIR (with whitespace + ~/)
    const claudeDir = path.join(home, 'custom-claude');
    fs.mkdirSync(claudeDir, { recursive: true });
    const claudeEnv = { PATH: '/nonexistent', CLAUDE_CONFIG_DIR: '  ~/custom-claude  ' };
    const cc = clientsMod.getClient('claude-code');
    assert.equal(cc.detect(home, claudeEnv).detected, true);
    const ccPlan = cc.plan({ home, launch, withHooks: true, env: claudeEnv }).apply();
    ccPlan.write();
    assert.equal(fs.existsSync(path.join(claudeDir, '.claude.json')), true);
    assert.equal(fs.existsSync(path.join(claudeDir, 'settings.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.claude.json')), false);

    // Codex: CODEX_HOME
    const codexDir = path.join(home, 'custom-codex');
    fs.mkdirSync(codexDir, { recursive: true });
    const codexEnv = { PATH: '/nonexistent', CODEX_HOME: codexDir };
    const codex = clientsMod.getClient('codex');
    assert.equal(codex.detect(home, codexEnv).detected, true);
    const codexApplied = codex.plan({ home, launch, env: codexEnv }).apply();
    assert.equal(codexApplied.filePath, path.join(codexDir, 'config.toml'));

    // Cursor: CURSOR_CONFIG_DIR
    const cursorDir = path.join(home, 'custom-cursor');
    fs.mkdirSync(cursorDir, { recursive: true });
    const cursorEnv = { PATH: '/nonexistent', CURSOR_CONFIG_DIR: cursorDir };
    const cursor = clientsMod.getClient('cursor');
    assert.equal(cursor.detect(home, cursorEnv).detected, true);
    const cursorApplied = cursor
      .plan({
        home,
        launch,
        withHooks: true,
        cursorBinPath: '/abs/astrolabe',
        env: cursorEnv,
        detected: true,
      })
      .apply();
    cursorApplied.write();
    assert.equal(fs.existsSync(path.join(cursorDir, 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(cursorDir, 'hooks.json')), true);
    const perms = clientsMod.mergeCursorPermissions(home, { env: cursorEnv });
    assert.equal(perms.filePath, path.join(cursorDir, 'permissions.json'));
    assert.equal(
      cursorMod.resolveApprovalPath(home, home, { env: cursorEnv }).startsWith(cursorDir),
      true,
    );

    // OpenCode: XDG_CONFIG_HOME and OPENCODE_CONFIG
    const xdgDir = path.join(home, 'xdg-cfg');
    fs.mkdirSync(path.join(xdgDir, 'opencode'), { recursive: true });
    const ocXdgEnv = { PATH: '/nonexistent', XDG_CONFIG_HOME: xdgDir };
    const oc = clientsMod.getClient('opencode');
    assert.equal(oc.detect(home, ocXdgEnv).detected, true);
    const ocXdgApplied = oc.plan({ home, launch, withHooks: true, env: ocXdgEnv }).apply();
    assert.equal(ocXdgApplied.results[0].filePath, path.join(xdgDir, 'opencode', 'opencode.jsonc'));
    assert.equal(
      ocXdgApplied.results[1].filePath,
      path.join(xdgDir, 'opencode', 'plugins', 'astrolabe-guard.ts'),
    );

    const ocFile = path.join(home, 'oc-single', 'my-opencode.json');
    fs.mkdirSync(path.dirname(ocFile), { recursive: true });
    fs.writeFileSync(ocFile, '{}\n');
    const ocSingleEnv = { PATH: '/nonexistent', OPENCODE_CONFIG: ocFile };
    assert.equal(oc.detect(home, ocSingleEnv).detected, true);
    const ocSingleApplied = oc.plan({ home, launch, withHooks: true, env: ocSingleEnv }).apply();
    assert.equal(ocSingleApplied.results[0].filePath, ocFile);
    assert.equal(
      ocSingleApplied.results[1].filePath,
      path.join(home, '.config', 'opencode', 'plugins', 'astrolabe-guard.ts'),
    );

    // Codebuddy: CODEBUDDY_HOME
    const cbDir = path.join(home, 'custom-cb');
    fs.mkdirSync(cbDir, { recursive: true });
    const cbEnv = { PATH: '/nonexistent', CODEBUDDY_HOME: cbDir };
    const cb = clientsMod.getClient('codebuddy');
    assert.equal(cb.detect(home, cbEnv).detected, true);
    const cbApplied = cb.plan({ home, launch, withHooks: true, env: cbEnv }).apply();
    assert.equal(cbApplied.results[0].filePath, path.join(cbDir, '.mcp.json'));
    assert.equal(cbApplied.results[1].filePath, path.join(cbDir, 'settings.json'));

    // Grok: GROK_HOME
    const grokDir = path.join(home, 'custom-grok');
    fs.mkdirSync(grokDir, { recursive: true });
    const grokEnv = { PATH: '/nonexistent', GROK_HOME: grokDir };
    const grok = clientsMod.getClient('grok');
    assert.equal(grok.detect(home, grokEnv).detected, true);
    const grokApplied = grok.plan({ home, launch, withHooks: true, env: grokEnv }).apply();
    assert.equal(grokApplied.results[0].filePath, path.join(grokDir, 'config.toml'));
    assert.equal(grokApplied.results[1].filePath, path.join(grokDir, 'hooks', 'astrolabe.json'));

    // Kimi Code: KIMI_CONFIG_DIR
    const kimiDir = path.join(home, 'custom-kimi');
    fs.mkdirSync(kimiDir, { recursive: true });
    const kimiEnv = { PATH: '/nonexistent', KIMI_CONFIG_DIR: kimiDir };
    const kimi = clientsMod.getClient('kimicode');
    assert.equal(kimi.detect(home, kimiEnv).detected, true);
    const kimiApplied = kimi.plan({ home, launch, withHooks: true, env: kimiEnv }).apply();
    assert.equal(kimiApplied.results[0].filePath, path.join(kimiDir, 'mcp.json'));
    assert.equal(kimiApplied.results[1].filePath, path.join(kimiDir, 'config.toml'));

    // ZCode: ZCODE_HOME (both cli/config.json and direct setting.json without cli/)
    const zcodeDir = path.join(home, 'custom-zcode');
    fs.mkdirSync(path.join(zcodeDir, 'cli'), { recursive: true });
    const zcodeEnv = { PATH: '/nonexistent', ZCODE_HOME: zcodeDir };
    const zcode = clientsMod.getClient('zcode');
    assert.equal(zcode.detect(home, zcodeEnv).detected, true);
    assert.equal(
      zcode.plan({ home, launch, env: zcodeEnv }).apply().filePath,
      path.join(zcodeDir, 'cli', 'config.json'),
    );

    const zcodeFlatDir = path.join(home, 'flat-zcode');
    fs.mkdirSync(zcodeFlatDir, { recursive: true });
    fs.writeFileSync(path.join(zcodeFlatDir, 'config.json'), '{}\n');
    const zcodeFlatEnv = { PATH: '/nonexistent', ZCODE_HOME: zcodeFlatDir };
    assert.equal(zcode.detect(home, zcodeFlatEnv).detected, true);
    assert.equal(
      zcode.plan({ home, launch, env: zcodeFlatEnv }).apply().filePath,
      path.join(zcodeFlatDir, 'config.json'),
    );

    // Cline & Roo: VSCODE_USER_DATA_DIR + Linux XDG_CONFIG_HOME
    const vsUserData = path.join(home, 'vscode-portable');
    const clineSettingsDir = path.join(
      vsUserData,
      'User',
      'globalStorage',
      'saoudrizwan.claude-dev',
      'settings',
    );
    const rooSettingsDir = path.join(
      vsUserData,
      'User',
      'globalStorage',
      'rooveterinaryinc.roo-cline',
      'settings',
    );
    fs.mkdirSync(clineSettingsDir, { recursive: true });
    fs.mkdirSync(rooSettingsDir, { recursive: true });
    const vsEnv = { PATH: '/nonexistent', VSCODE_USER_DATA_DIR: vsUserData };
    assert.equal(clientsMod.getClient('cline').detect(home, vsEnv).detected, true);
    assert.equal(
      clientsMod.getClient('cline').plan({ home, launch, env: vsEnv }).apply().filePath,
      path.join(clineSettingsDir, 'cline_mcp_settings.json'),
    );
    assert.equal(clientsMod.getClient('roo').detect(home, vsEnv).detected, true);
    assert.equal(
      clientsMod.getClient('roo').plan({ home, launch, env: vsEnv }).apply().filePath,
      path.join(rooSettingsDir, 'mcp_settings.json'),
    );
    assert.equal(
      clientsMod.clineSettingsPath(home, { XDG_CONFIG_HOME: xdgDir }, null, 'linux'),
      path.join(xdgDir, 'Code', 'User', 'globalStorage', 'saoudrizwan.claude-dev', 'settings', 'cline_mcp_settings.json'),
    );
    assert.equal(
      clientsMod.rooSettingsPath(home, { XDG_CONFIG_HOME: xdgDir }, null, 'linux'),
      path.join(xdgDir, 'Code', 'User', 'globalStorage', 'rooveterinaryinc.roo-cline', 'settings', 'mcp_settings.json'),
    );

    // Continue: CONTINUE_GLOBAL_DIR
    const contDir = path.join(home, 'custom-continue');
    fs.mkdirSync(contDir, { recursive: true });
    const contEnv = { PATH: '/nonexistent', CONTINUE_GLOBAL_DIR: contDir };
    const cont = clientsMod.getClient('continue');
    assert.equal(cont.detect(home, contEnv).detected, true);
    assert.equal(
      cont.plan({ home, launch, env: contEnv }).apply().filePath,
      path.join(contDir, 'mcp.json'),
    );

    // Windsurf: WINDSURF_CONFIG_DIR; DSH: DSH_HOME
    const wsDir = path.join(home, 'custom-windsurf');
    fs.mkdirSync(wsDir, { recursive: true });
    const wsEnv = { PATH: '/nonexistent', WINDSURF_CONFIG_DIR: wsDir };
    assert.equal(clientsMod.getClient('windsurf').detect(home, wsEnv).detected, true);
    assert.equal(
      clientsMod.getClient('windsurf').plan({ home, launch, env: wsEnv }).apply().filePath,
      path.join(wsDir, 'mcp_config.json'),
    );
    const dshDir = path.join(home, 'custom-dsh');
    fs.mkdirSync(dshDir, { recursive: true });
    const dshEnv = { PATH: '/nonexistent', DSH_HOME: dshDir };
    assert.equal(clientsMod.getClient('deepseek-harness').detect(home, dshEnv).detected, true);
    assert.equal(
      dshMod.resolveDshPatchPath(home, dshEnv),
      path.join(dshDir, 'cordis.patch.yml'),
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Explicit CLI overrides (--config-dir, --mcp-config, repeatable --hooks-config/--settings) and safety guardrails', async () => {
  const parsed = argsMod.parseArgs([
    '--yes',
    '--clients=claude-code',
    '--config-dir',
    '/tmp/cc',
    '--mcp-config=/tmp/mcp.json',
    '--settings',
    '/tmp/s1.json',
    '--settings=/tmp/s2.ccm.json',
    '--hooks-config',
    '/tmp/s3.json',
  ]);
  assert.equal(parsed.configDir, '/tmp/cc');
  assert.equal(parsed.mcpConfig, '/tmp/mcp.json');
  assert.deepEqual(parsed.hooksConfig, ['/tmp/s1.json', '/tmp/s2.ccm.json', '/tmp/s3.json']);

  const home = tmpHome();
  try {
    // 1. Claude Code multi-profile --settings + --mcp-config writes and backs up all existing files
    const customMcp = path.join(home, 'profiles', 'claude-work.json');
    const s1 = path.join(home, 'profiles', 'settings.json');
    const s2 = path.join(home, 'profiles', 'settings.ccm.json');
    fs.mkdirSync(path.dirname(customMcp), { recursive: true });
    fs.writeFileSync(customMcp, JSON.stringify({ theme: 'dark' }, null, 2));
    fs.writeFileSync(s1, JSON.stringify({ model: 'sonnet' }, null, 2));
    fs.writeFileSync(
      s2,
      JSON.stringify(
        {
          hooks: {
            PreToolUse: [
              { matcher: 'Bash', hooks: [{ type: 'command', command: 'echo keep-ccm' }] },
            ],
          },
        },
        null,
        2,
      ),
    );
    const ccRes = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=claude-code',
        '--home',
        home,
        '--command',
        '/abs/astrolabe',
        '--mcp-config',
        customMcp,
        '--settings',
        s1,
        '--settings',
        s2,
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home } },
    );
    assert.equal(ccRes.status, 0, ccRes.stderr + ccRes.stdout);
    const mcpOut = JSON.parse(fs.readFileSync(customMcp, 'utf8'));
    assert.equal(mcpOut.theme, 'dark');
    assert.equal(mcpOut.mcpServers.astrolabe.command, '/abs/astrolabe');
    const s1Out = JSON.parse(fs.readFileSync(s1, 'utf8'));
    assert.equal(s1Out.model, 'sonnet');
    assert.equal(s1Out.hooks.SessionStart[0].hooks[0].command, '/abs/astrolabe hooks session-start');
    const s2Out = JSON.parse(fs.readFileSync(s2, 'utf8'));
    assert.equal(s2Out.hooks.PreToolUse.length, 2);
    assert.equal(s2Out.hooks.PreToolUse[0].hooks[0].command, 'echo keep-ccm');
    assert.equal(
      s2Out.hooks.PreToolUse[1].hooks[0].command,
      '/abs/astrolabe hooks remind --client=claude-code',
    );
    const profileFiles = fs.readdirSync(path.dirname(customMcp));
    assert.ok(profileFiles.some((f) => f.startsWith('claude-work.json.bak-')));
    assert.ok(profileFiles.some((f) => f.startsWith('settings.json.bak-')));
    assert.ok(profileFiles.some((f) => f.startsWith('settings.ccm.json.bak-')));

    // 2. Cursor NOT installed under ~/.cursor still lands via --clients=cursor --config-dir=...
    const customCursorDir = path.join(home, 'portable-cursor');
    assert.equal(fs.existsSync(path.join(home, '.cursor')), false);
    const curRes = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=cursor',
        '--with-hooks',
        '--home',
        home,
        '--config-dir',
        customCursorDir,
        '--command',
        '/abs/astrolabe',
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: '/nonexistent' } },
    );
    assert.equal(curRes.status, 0, curRes.stderr + curRes.stdout);
    assert.equal(fs.existsSync(path.join(customCursorDir, 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(customCursorDir, 'hooks.json')), true);
    assert.equal(fs.existsSync(path.join(customCursorDir, 'permissions.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.cursor')), false);

    // 3. ZCode custom setting.json + single/dual file overrides
    const zSettingDir = path.join(home, 'zcode-custom');
    fs.mkdirSync(zSettingDir, { recursive: true });
    const zSettingFile = path.join(zSettingDir, 'setting.json');
    fs.writeFileSync(zSettingFile, JSON.stringify({ theme: 'light' }, null, 2));
    const zDirRes = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=zcode',
        '--with-hooks',
        '--home',
        home,
        '--config-dir',
        zSettingDir,
        '--command',
        '/abs/astrolabe',
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home } },
    );
    assert.equal(zDirRes.status, 0, zDirRes.stderr + zDirRes.stdout);
    const zSettingOut = JSON.parse(fs.readFileSync(zSettingFile, 'utf8'));
    assert.equal(zSettingOut.theme, 'light');
    assert.equal(zSettingOut.mcp.servers.astrolabe.command, '/abs/astrolabe');
    assert.equal(
      zSettingOut.hooks.events.PreToolUse[0].hooks[0].command,
      '/abs/astrolabe hooks remind --client=zcode',
    );

    // ZCode --hooks-config only writes both MCP + hooks to specified file
    const zHooksOnly = path.join(home, 'zcode-hooks-only.json');
    const zHooksRes = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=zcode',
        '--home',
        home,
        '--hooks-config',
        zHooksOnly,
        '--command',
        '/abs/astrolabe',
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home } },
    );
    assert.equal(zHooksRes.status, 0, zHooksRes.stderr + zHooksRes.stdout);
    const zHooksOnlyDoc = JSON.parse(fs.readFileSync(zHooksOnly, 'utf8'));
    assert.equal(zHooksOnlyDoc.mcp.servers.astrolabe.command, '/abs/astrolabe');
    assert.ok(zHooksOnlyDoc.hooks.events.PreToolUse);

    // ZCode both --mcp-config and --hooks-config writes each to its respective file
    const zSplitMcp = path.join(home, 'zcode-split-mcp.json');
    const zSplitHooks = path.join(home, 'zcode-split-hooks.json');
    const zSplitRes = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--clients=zcode',
        '--home',
        home,
        '--mcp-config',
        zSplitMcp,
        '--hooks-config',
        zSplitHooks,
        '--command',
        '/abs/astrolabe',
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home } },
    );
    assert.equal(zSplitRes.status, 0, zSplitRes.stderr + zSplitRes.stdout);
    const zSplitMcpDoc = JSON.parse(fs.readFileSync(zSplitMcp, 'utf8'));
    const zSplitHooksDoc = JSON.parse(fs.readFileSync(zSplitHooks, 'utf8'));
    assert.equal(zSplitMcpDoc.mcp.servers.astrolabe.command, '/abs/astrolabe');
    assert.equal(zSplitMcpDoc.hooks, undefined);
    assert.equal(zSplitHooksDoc.mcp, undefined);
    assert.equal(
      zSplitHooksDoc.hooks.events.PreToolUse[0].hooks[0].command,
      '/abs/astrolabe hooks remind --client=zcode',
    );

    // 4. Guardrails: missing --clients, multiple --clients, or --hooks-config on MCP-only clients
    await assert.rejects(
      () => runMod.run(['--yes', '--home', home, '--config-dir', '/tmp/x']),
      /require --clients=<single-client-id>/,
    );
    await assert.rejects(
      () => runMod.run(['--yes', '--home', home, '--clients=claude-code,cursor', '--config-dir', '/tmp/x']),
      /require a single client/,
    );
    for (const mcpOnly of ['codex', 'cline', 'roo', 'windsurf', 'continue']) {
      await assert.rejects(
        () => runMod.run(['--yes', '--home', home, `--clients=${mcpOnly}`, '--hooks-config', '/tmp/h.json']),
        /not supported for MCP-only client/,
      );
    }
    const cliGuardFail = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--home', home, '--mcp-config', '/tmp/m.json'],
      { encoding: 'utf8' },
    );
    assert.equal(cliGuardFail.status, 1);
    assert.match(cliGuardFail.stderr, /require --clients=<single-client-id>/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression 1: expandHome strips leading slashes/backslashes after ~/ or ~\\', () => {
  const home = path.resolve('/tmp/astro-home');
  assert.equal(cursorMod.expandHome('~//foo', home), path.join(home, 'foo'));
  assert.equal(cursorMod.expandHome('~///etc/hosts', home), path.join(home, 'etc', 'hosts'));
  assert.equal(cursorMod.expandHome('~\\\\foo', home), path.join(home, 'foo'));
  assert.equal(cursorMod.expandHome('~//', home), home);
  assert.equal(cursorMod.expandHome('~/\\foo', home), path.join(home, 'foo'));
  assert.equal(cursorMod.expandHome('~\\//foo', home), path.join(home, 'foo'));
  assert.equal(cursorMod.expandHome('~\\', home), home);
});

test('Regression 2: parseArgs rejects whitespace path flags and clients ignore whitespace hooks/configDir', () => {
  const dirFlags = [
    ['--home', /--home needs a directory/],
    ['--project', /--project needs a directory/],
    ['--config-dir', /--config-dir needs a directory/],
  ];
  for (const [flag, pattern] of dirFlags) {
    assert.throws(() => argsMod.parseArgs([flag, '   ']), pattern);
    assert.throws(() => argsMod.parseArgs([`${flag}=   `]), pattern);
    assert.throws(() => argsMod.parseArgs([flag]), pattern);
    assert.throws(() => argsMod.parseArgs([flag, '--yes']), pattern);
    assert.throws(() => argsMod.parseArgs([flag, '  --yes']), pattern);
  }
  const fileFlags = [
    ['--mcp-config', /--mcp-config needs a file path/],
    ['--hooks-config', /--hooks-config needs a file path/],
    ['--settings', /--settings needs a file path/],
  ];
  for (const [flag, pattern] of fileFlags) {
    assert.throws(() => argsMod.parseArgs([flag, '   ']), pattern);
    assert.throws(() => argsMod.parseArgs([`${flag}=   `]), pattern);
    assert.throws(() => argsMod.parseArgs([flag]), pattern);
    assert.throws(() => argsMod.parseArgs([flag, '-x']), pattern);
    assert.throws(() => argsMod.parseArgs([flag, '  -x']), pattern);
  }

  const trimmed = argsMod.parseArgs([
    '--home', '  /tmp/h  ',
    '--project=  /tmp/p  ',
    '--config-dir', '  /tmp/c  ',
    '--mcp-config=  /tmp/m.json  ',
    '--hooks-config', '  /tmp/h1.json  ',
    '--settings=  /tmp/h2.json  ',
  ]);
  assert.equal(trimmed.home, '/tmp/h');
  assert.equal(trimmed.project, '/tmp/p');
  assert.equal(trimmed.configDir, '/tmp/c');
  assert.equal(trimmed.mcpConfig, '/tmp/m.json');
  assert.deepEqual(trimmed.hooksConfig, ['/tmp/h1.json', '/tmp/h2.json']);

  const home = tmpHome();
  try {
    const launch = { command: '/abs/astrolabe', args: ['.'] };
    for (const id of ['claude-code', 'cursor', 'opencode', 'codebuddy', 'grok', 'kimicode', 'zcode']) {
      const c = clientsMod.getClient(id);
      const plan = c.plan({
        home,
        launch,
        withHooks: false,
        cursorBinPath: '/abs/astrolabe',
        projectRoot: home,
        configDir: '   ',
        mcpConfigPath: '   ',
        hooksConfigPaths: ['   ', ''],
        env: { PATH: '/nonexistent' },
      });
      assert.equal(
        plan.steps.some((s) => String(s.kind).includes('hooks') || String(s.kind).includes('guard')),
        false,
        `${id} should not treat whitespace hooksConfigPaths as withHooks`,
      );
      if (id === 'zcode') {
        assert.equal(plan.filePath, path.join(home, '.zcode', 'cli', 'config.json'));
      }
    }
    const dshPlan = clientsMod.getClient('deepseek-harness').plan({
      home,
      launch,
      configDir: '   ',
      mcpConfigPath: '   ',
      hooksConfigPaths: ['   '],
      env: { PATH: '/nonexistent' },
    });
    assert.equal(dshPlan.filePath, path.join(home, '.dsh', 'cordis.patch.yml'));
    assert.equal(dshPlan.apply().skipped, true);
    for (const mcpOnly of ['codex', 'windsurf', 'cline', 'roo', 'continue']) {
      assert.doesNotThrow(() =>
        clientsMod.getClient(mcpOnly).plan({
          home,
          launch,
          configDir: '   ',
          mcpConfigPath: '   ',
          hooksConfigPaths: ['   ', ''],
          env: { PATH: '/nonexistent' },
        }),
      );
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression 3: sameComparablePath blocks non-single-file MCP/hooks collisions and merges zcode symlinks', () => {
  const home = tmpHome();
  try {
    const realFile = path.join(home, 'shared.json');
    const linkFile = path.join(home, 'shared-link.json');
    fs.writeFileSync(realFile, '{}\n');
    fs.symlinkSync(realFile, linkFile);

    const linkedDir = path.join(home, 'linked-dir');
    const realDir = path.join(home, 'real-dir');
    fs.mkdirSync(realDir, { recursive: true });
    fs.symlinkSync(realDir, linkedDir);

    assert.equal(clientsMod.sameComparablePath(realFile, linkFile), true);
    assert.equal(
      clientsMod.sameComparablePath(
        path.join(realDir, 'nested', 'new.json'),
        path.join(linkedDir, 'nested', 'new.json'),
      ),
      true,
    );
    assert.equal(
      clientsMod.sameComparablePath('/tmp/CaseTest.json', '/tmp/casetest.json', 'win32'),
      true,
    );

    const launch = { command: '/abs/astrolabe', args: ['.'] };
    for (const id of ['claude-code', 'codebuddy', 'cursor', 'opencode', 'grok', 'kimicode']) {
      const client = clientsMod.getClient(id);
      assert.throws(
        () =>
          client.plan({
            home,
            launch,
            withHooks: true,
            cursorBinPath: '/abs/astrolabe',
            projectRoot: home,
            mcpConfigPath: realFile,
            hooksConfigPaths: [realFile],
            env: { PATH: '/nonexistent' },
          }),
        /must not point to the same file/,
      );
      assert.throws(
        () =>
          client.plan({
            home,
            launch,
            withHooks: true,
            cursorBinPath: '/abs/astrolabe',
            projectRoot: home,
            mcpConfigPath: realFile,
            hooksConfigPaths: [linkFile],
            env: { PATH: '/nonexistent' },
          }),
        /must not point to the same file/,
      );
    }

    // Cursor also blocks collision when cursorBinPath is null, detected is false, or scope is project
    const cursor = clientsMod.getClient('cursor');
    assert.throws(
      () =>
        cursor.plan({
          home,
          launch,
          withHooks: true,
          detected: true,
          cursorBinPath: null,
          cursorCommandExplicit: true,
          scope: 'global',
          mcpConfigPath: realFile,
          hooksConfigPaths: [linkFile],
          env: { PATH: '/nonexistent' },
        }),
      /must not point to the same file/,
    );
    assert.throws(
      () =>
        cursor.plan({
          home,
          launch,
          withHooks: true,
          detected: false,
          scope: 'global',
          mcpConfigPath: realFile,
          hooksConfigPaths: [linkFile],
          env: { PATH: '/nonexistent' },
        }),
      /must not point to the same file/,
    );
    assert.throws(
      () =>
        cursor.plan({
          home,
          launch,
          withHooks: true,
          detected: true,
          cursorBinPath: null,
          cursorCommandExplicit: true,
          scope: 'project',
          projectRoot: home,
          hooksConfigPaths: [path.join(home, '.cursor', 'mcp.json')],
          env: { PATH: '/nonexistent' },
        }),
      /must not point to the same file/,
    );

    // ZCode merges symlinked MCP + hooks paths into one single write
    const zcodeFile = path.join(home, 'zcode.json');
    const zcodeLink = path.join(home, 'zcode-link.json');
    fs.writeFileSync(zcodeFile, '{}\n');
    fs.symlinkSync(zcodeFile, zcodeLink);
    const zcodePlan = clientsMod.getClient('zcode').plan({
      home,
      launch,
      binPath: '/abs/astrolabe',
      mcpConfigPath: zcodeFile,
      hooksConfigPaths: [zcodeLink],
      env: { PATH: '/nonexistent' },
    });
    assert.equal(zcodePlan.steps.length, 1);
    const zApplied = zcodePlan.apply();
    zApplied.write();
    const zDoc = JSON.parse(fs.readFileSync(zcodeFile, 'utf8'));
    assert.equal(zDoc.mcp.servers.astrolabe.command, '/abs/astrolabe');
    assert.equal(zDoc.hooks.enabled, true);
    assert.equal(
      zDoc.hooks.events.PreToolUse[0].hooks[0].command,
      '/abs/astrolabe hooks remind --client=zcode',
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression 4: OPENCODE_CONFIG only overrides config file path without polluting plugins dir', () => {
  const home = tmpHome();
  try {
    const customFile = path.join(home, 'elsewhere', 'my-opencode.json');
    assert.equal(
      opencodeMod.resolveOpenCodeConfigDir(home, { OPENCODE_CONFIG: customFile }),
      path.join(home, '.config', 'opencode'),
    );
    assert.equal(
      opencodeMod.resolveOpenCodePluginPath(home, { OPENCODE_CONFIG: customFile }),
      path.join(home, '.config', 'opencode', 'plugins', 'astrolabe-guard.ts'),
    );
    assert.equal(
      opencodeMod.resolveOpenCodeConfigPath(home, () => false, { OPENCODE_CONFIG: customFile }),
      customFile,
    );

    const xdgDir = path.join(home, 'xdg');
    assert.equal(
      opencodeMod.resolveOpenCodePluginPath(home, {
        OPENCODE_CONFIG: customFile,
        XDG_CONFIG_HOME: xdgDir,
      }),
      path.join(xdgDir, 'opencode', 'plugins', 'astrolabe-guard.ts'),
    );

    const ocDir = path.join(home, 'oc-dir');
    assert.equal(
      opencodeMod.resolveOpenCodePluginPath(home, {
        OPENCODE_CONFIG: customFile,
        OPENCODE_CONFIG_DIR: ocDir,
        XDG_CONFIG_HOME: xdgDir,
      }),
      path.join(ocDir, 'plugins', 'astrolabe-guard.ts'),
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression 5: non-existent custom dirs do not skip and cursor.approvalEnv forwards CURSOR_CONFIG_DIR', () => {
  const home = tmpHome();
  try {
    const launch = { command: '/abs/astrolabe', args: ['.'] };

    // 1. Cursor with detected: true and missing ~/.cursor still creates hooks.json
    const cursorApplied = clientsMod.getClient('cursor').plan({
      home,
      launch,
      withHooks: true,
      detected: true,
      cursorBinPath: '/abs/astrolabe',
      projectRoot: home,
      env: { PATH: '/nonexistent' },
    }).apply();
    cursorApplied.write();
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'hooks.json')), true);

    // 2. codebuddy / grok / kimicode / zcode with non-existent env dirs create files instead of skipping
    const cbEnvDir = path.join(home, 'new-cb-env');
    const cbApplied = clientsMod.getClient('codebuddy').plan({
      home,
      launch,
      withHooks: true,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', CODEBUDDY_CONFIG_DIR: cbEnvDir },
    }).apply();
    assert.equal(cbApplied.results.every((r) => !r.skipped), true);
    cbApplied.write();
    assert.equal(fs.existsSync(path.join(cbEnvDir, '.mcp.json')), true);
    assert.equal(fs.existsSync(path.join(cbEnvDir, 'settings.json')), true);

    const grokEnvDir = path.join(home, 'new-grok-env');
    const grokApplied = clientsMod.getClient('grok').plan({
      home,
      launch,
      withHooks: true,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', GROK_HOME: grokEnvDir },
    }).apply();
    assert.equal(grokApplied.results.every((r) => !r.skipped), true);
    grokApplied.write();
    assert.equal(fs.existsSync(path.join(grokEnvDir, 'config.toml')), true);
    assert.equal(fs.existsSync(path.join(grokEnvDir, 'hooks', 'astrolabe.json')), true);

    const kimiEnvDir = path.join(home, 'new-kimi-env');
    const kimiApplied = clientsMod.getClient('kimicode').plan({
      home,
      launch,
      withHooks: true,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', KIMI_HOME: kimiEnvDir },
    }).apply();
    assert.equal(kimiApplied.results.every((r) => !r.skipped), true);
    kimiApplied.write();
    assert.equal(fs.existsSync(path.join(kimiEnvDir, 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(kimiEnvDir, 'config.toml')), true);

    const zcodeEnvDir = path.join(home, 'new-zcode-env');
    const zcodeApplied = clientsMod.getClient('zcode').plan({
      home,
      launch,
      withHooks: true,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', ZCODE_CONFIG_DIR: zcodeEnvDir },
    }).apply();
    assert.equal(Boolean(zcodeApplied.skipped), false);
    zcodeApplied.write();
    assert.equal(fs.existsSync(path.join(zcodeEnvDir, 'cli', 'config.json')), true);

    // 3. cursorMod.approvalEnv and enableCursorApproval forward CURSOR_CONFIG_DIR
    assert.equal(
      cursorMod.approvalEnv({}, home, '~/cli-cursor').CURSOR_CONFIG_DIR,
      path.join(home, 'cli-cursor'),
    );
    assert.equal(
      cursorMod.approvalEnv({ CURSOR_HOME: '~/env-cursor-home' }, home).CURSOR_CONFIG_DIR,
      path.join(home, 'env-cursor-home'),
    );
    assert.equal(
      cursorMod.approvalEnv({ CURSOR_CONFIG_DIR: '~/env-cursor-cfg' }, home).CURSOR_CONFIG_DIR,
      path.join(home, 'env-cursor-cfg'),
    );
    assert.equal(
      Object.prototype.hasOwnProperty.call(
        cursorMod.approvalEnv({ CURSOR_HOME: '   ' }, home, '   '),
        'CURSOR_CONFIG_DIR',
      ),
      false,
    );

    let capturedEnv = null;
    cursorMod.enableCursorApproval({
      home,
      projectRoot: home,
      env: { CURSOR_HOME: '~/forwarded-cursor' },
      whichFn: () => '/fake/cursor-agent',
      spawnFn: (_cmd, _args, opts) => {
        capturedEnv = opts.env;
        return { status: 0, stdout: '', stderr: '' };
      },
      existsFn: () => false,
    });
    assert.equal(capturedEnv.CURSOR_CONFIG_DIR, path.join(home, 'forwarded-cursor'));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});



test('Cursor effective approval normalization matches client field order and optional fields', { skip: process.platform === 'win32' }, () => {
  const fixtures = [
    [{ command: '/bin/astrolabe' }, 'astrolabe-1a322119af84646d'],
    [{ type: 'stdio', command: '/bin/astrolabe', args: ['.'], env: { Z: 'last', A: 'first' },
      cwd: '/repo', startupTimeoutSec: 300 }, 'astrolabe-1ef03ba32e577c6d'],
    [{ command: '${BIN}', args: ['${ROOT:-/repo}'], env: { BIN: '/bin/astrolabe', Z: '${env:EXTERNAL}' },
      envFile: 'ignored' }, 'astrolabe-35958e626891138f'],
  ];
  for (const [server, key] of fixtures) {
    assert.equal(cursorMod.approvalKey('/repo', server, 'astrolabe', { EXTERNAL: 'value' }), key);
  }
  const bare = { command: '/bin/astrolabe' };
  assert.notEqual(cursorMod.approvalKey('/repo', bare), cursorMod.approvalKey('/repo', { ...bare, args: [] }));
  assert.notEqual(cursorMod.approvalKey('/repo', bare), cursorMod.approvalKey('/repo', { ...bare, cwd: '/repo' }));
  assert.equal(cursorMod.normalizeCursorServer({ command: 'x', env: null }), null);
  assert.equal(cursorMod.normalizeCursorServer({ command: 'x', type: 'http' }), null);
});

test('Cursor hook migration collapses owned duplicates and preserves mixed groups', () => {
  const home = tmpHome();
  try {
    const file = path.join(home, 'hooks.json');
    const mixed = { hooks: [{ command: 'astrolabe hooks remind' }, { command: 'echo keep' }] };
    const third = { command: '/third-party hooks remind' };
    fs.writeFileSync(file, JSON.stringify({ hooks: {
      preToolUse: [{ command: '/old/astrolabe hooks remind', matcher: 'mcp__astrolabe__.*' }, third,
        { command: 'astrolabe hooks remind' }, mixed],
      sessionEnd: [{ command: 'astrolabe hooks cleanup' }, { command: '/old/astrolabe hooks cleanup' }],
    } }));
    const result = cursorMod.mergeCursorHooksFile(file, { binPath: '/stable/astrolabe' });
    assert.equal(result.doc.hooks.preToolUse.length, 3);
    assert.deepEqual(result.doc.hooks.preToolUse.slice(1), [third, mixed]);
    assert.match(result.doc.hooks.preToolUse[0].matcher, /astrolabe-\.\*/);
    assert.match(result.doc.hooks.preToolUse[0].matcher, /mcp__astrolabe__/);
    assert.equal(result.doc.hooks.sessionEnd.length, 1);
    fs.writeFileSync(file, JSON.stringify(result.doc));
    assert.deepEqual(cursorMod.mergeCursorHooksFile(file, { binPath: '/stable/astrolabe' }).doc, result.doc);
  } finally { fs.rmSync(home, { recursive: true, force: true }); }
});

test('--check-config is read-only, selects project over global, and does not print env', () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    fs.mkdirSync(path.join(home, '.cursor'));
    fs.mkdirSync(path.join(project, '.cursor'));
    const projectFile = path.join(project, '.cursor/mcp.json');
    const globalFile = path.join(home, '.cursor/mcp.json');
    fs.writeFileSync(globalFile, JSON.stringify({ mcpServers: { astrolabe: { command: '/new/astrolabe' } } }));
    fs.writeFileSync(projectFile, JSON.stringify({ mcpServers: { astrolabe: {
      command: '/old/astrolabe', args: ['.', '--context=cursor'], env: { PRIVATE_FIXTURE: 'must-not-be-printed' },
    } } }));
    const before = fs.readFileSync(projectFile, 'utf8');
    const result = spawnSync(process.execPath, [BIN, 'setup', '--check-config', '--home', home,
      '--project', project], { encoding: 'utf8', env: { ...process.env, PATH: '', HOME: home } });
    assert.equal(result.status, 0, result.stderr);
    const report = JSON.parse(result.stdout);
    assert.equal(report.source, 'project');
    assert.equal(report.command, '/old/astrolabe');
    assert.equal(report.shadowsGlobal, true);
    assert.equal(report.approval, 'current-config-not-approved');
    assert.doesNotMatch(result.stdout, /must-not-be-printed|PRIVATE_FIXTURE/);
    assert.equal(fs.readFileSync(projectFile, 'utf8'), before);
    assert.deepEqual(fs.readdirSync(path.join(home, '.cursor')), ['mcp.json']);
    fs.writeFileSync(projectFile, '{broken');
    const invalid = cursorMod.inspectCursorConfig(home, project);
    assert.equal(invalid.source, 'project');
    assert.equal(invalid.valid, false);
    assert.equal(invalid.command, null);
  } finally { fs.rmSync(home, { recursive: true, force: true }); }
});

test('global setup warns when an existing project registration shadows it', { skip: process.platform === 'win32' }, () => {
  const home = tmpHome();
  try {
    const project = tmpProject(home);
    fs.mkdirSync(path.join(home, '.cursor'));
    fs.mkdirSync(path.join(project, '.cursor'));
    const file = path.join(project, '.cursor/mcp.json');
    const old = JSON.stringify({ mcpServers: { astrolabe: { command: '/old/astrolabe' } } });
    fs.writeFileSync(file, old);
    const result = spawnSync(process.execPath, [BIN, 'setup', '--yes', '--clients=cursor', '--scope=global',
      '--home', home, '--project', project, '--command=/new/astrolabe', '--no-cursor-permissions'],
      { encoding: 'utf8', env: { ...process.env, PATH: '', HOME: home } });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /project configuration .* overrides the global Astrolabe entry/);
    assert.equal(fs.readFileSync(file, 'utf8'), old);
  } finally { fs.rmSync(home, { recursive: true, force: true }); }
});
