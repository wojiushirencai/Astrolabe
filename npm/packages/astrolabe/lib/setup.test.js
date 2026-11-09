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
    const plan = client.plan({ home, launch, withHooks: true });
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
        .plan({ home: home2, launch, withHooks: true })
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
    fs.writeFileSync(approvalPath, JSON.stringify(['other-1', 'astrolabe-abc123']));
    const approval = clientsMod.checkCursorApprovalFile(approvalPath);
    assert.equal(approval.exists, true);
    assert.equal(approval.approved, true);

    fs.writeFileSync(approvalPath, JSON.stringify(['other-1']));
    assert.equal(clientsMod.checkCursorApprovalFile(approvalPath).approved, false);

    const disabledPath = clientsMod.resolveCursorDisabledPath(home, project);
    fs.mkdirSync(path.dirname(disabledPath), { recursive: true });
    fs.writeFileSync(disabledPath, JSON.stringify(['astrolabe', 'other']));
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
          "fs.writeFileSync(path.join(dir, 'mcp-approvals.json'), JSON.stringify(['astrolabe-testhash']));",
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
      const approval = clientsMod.checkCursorApprovalFile(approvalPath);
      assert.equal(approval.approved, true);
      assert.ok(approval.entries.includes('astrolabe-testhash'));

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
      fs.writeFileSync(disabledPath, JSON.stringify(['astrolabe', 'other']));
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
            sessionEnd: [{ command: 'C:\\Tools\\my-astrolabe.exe hooks cleanup' }],
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
