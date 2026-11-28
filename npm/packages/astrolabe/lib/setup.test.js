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

test('parseArgs accepts --with-hooks (deprecated) and --remove-hooks', () => {
  const opts = argsMod.parseArgs(['--with-hooks', '--clients=dsh,opencode']);
  assert.equal(opts.deprecatedWithHooks, true);
  assert.equal(opts.withHooks, false);
  assert.equal(opts.removeHooks, false);
  assert.deepEqual(opts.clients, ['dsh', 'opencode']);
  assert.equal(clientsMod.getClient('dsh').id, 'deepseek-harness');
  assert.equal(clientsMod.canonicalClientId('dsh'), 'deepseek-harness');

  const rmOpts = argsMod.parseArgs(['--remove-hooks']);
  assert.equal(rmOpts.removeHooks, true);
  assert.equal(rmOpts.withHooks, false);
  assert.equal(rmOpts.deprecatedWithHooks, false);
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

test('DSH cordis patch keeps orca-managed block when stripping astrolabe-managed-dsh-guard', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.dsh'), { recursive: true });
    const patch = path.join(home, '.dsh', 'cordis.patch.yml');
    const orcaBlock = `# >>> orca-managed-dsh-hooks (managed by Orca; do not edit) >>>
- insert:
    - id: orca-agent-hooks
      name: '@deepseek-ai/dsh-hooks-claude-code'
# <<< orca-managed-dsh-hooks <<<`;
    const astrolabeBlock = `# >>> astrolabe-managed-dsh-guard >>>
- insert:
    - id: astrolabe-dsh-guard
      name: 'astrolabe-dsh-guard'
# <<< astrolabe-managed-dsh-guard <<<`;
    fs.writeFileSync(patch, `${orcaBlock}\n\n${astrolabeBlock}\n`);
    const r = clientsMod.stripDshCordisPatch(patch);
    assert.equal(r.skipped, undefined);
    assert.equal(r.replaced, true);
    assert.equal(r.removed, true);
    assert.match(r.text, /orca-managed-dsh-hooks/);
    assert.match(r.text, /orca-agent-hooks/);
    assert.doesNotMatch(r.text, /astrolabe-managed-dsh-guard/);
    assert.doesNotMatch(r.text, /astrolabe-dsh-guard/);

    fs.writeFileSync(patch, r.text);
    const second = clientsMod.stripDshCordisPatch(patch);
    assert.equal(second.skipped, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('DSH detect and plan clean legacy astrolabe-dsh-guard plugin and cordis patch', () => {
  const home = tmpHome();
  try {
    const nodeModules = path.join(home, '.dsh', 'profiles', 'node_modules');
    fs.mkdirSync(nodeModules, { recursive: true });
    const client = clientsMod.getClient('deepseek-harness');
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };

    // Plain .dsh/profiles/node_modules without astrolabe-dsh-guard or patch block is not detected, and plan skips cleanly.
    assert.equal(client.detect(home, { PATH: '/nonexistent' }).detected, false);
    const emptyApplied = client.plan({ home, launch }).apply();
    assert.equal(emptyApplied.skipped, true);

    // Detected when astrolabe-dsh-guard dir exists
    const pluginDir = path.join(nodeModules, 'astrolabe-dsh-guard');
    fs.mkdirSync(pluginDir, { recursive: true });
    fs.writeFileSync(path.join(pluginDir, 'package.json'), JSON.stringify({ name: 'astrolabe-dsh-guard' }));
    assert.equal(client.detect(home, { PATH: '/nonexistent' }).detected, true);

    // Also detected & cleaned when cordis.patch.yml contains astrolabe-managed-dsh-guard block
    const patchPath = path.join(home, '.dsh', 'cordis.patch.yml');
    fs.writeFileSync(
      patchPath,
      `# >>> orca-managed-dsh-hooks >>>\n- insert:\n    - id: orca-agent-hooks\n# <<< orca-managed-dsh-hooks <<<\n\n# >>> astrolabe-managed-dsh-guard >>>\n- insert:\n    - id: astrolabe-dsh-guard\n# <<< astrolabe-managed-dsh-guard <<<\n`,
    );
    assert.equal(client.detect(home, { PATH: '/nonexistent' }).detected, true);

    const applied = client.plan({ home, launch }).apply();
    assert.equal(applied.skipped, undefined);
    applied.write();

    assert.equal(fs.existsSync(pluginDir), false);
    const text = fs.readFileSync(patchPath, 'utf8');
    assert.match(text, /orca-managed-dsh-hooks/);
    assert.doesNotMatch(text, /astrolabe-managed-dsh-guard/);

    // Subsequent detect/plan skips cleanly
    assert.equal(client.detect(home, { PATH: '/nonexistent' }).detected, false);
    assert.equal(client.plan({ home, launch }).apply().skipped, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});


test('Codebuddy detect + MCP + automatic legacy hooks cleanup', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.codebuddy'), { recursive: true });
    const settingsPath = path.join(home, '.codebuddy', 'settings.json');
    fs.writeFileSync(
      settingsPath,
      JSON.stringify(
        {
          hooks: {
            PreToolUse: [
              { hooks: [{ type: 'command', command: 'echo keep' }] },
              { hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=codebuddy' }] },
            ],
            SessionEnd: [
              { hooks: [{ type: 'command', command: '/usr/local/bin/astrolabe hooks cleanup' }] },
            ],
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
    const plan = client.plan({ home, launch });
    const applied = plan.apply();
    assert.equal(applied.skipped, undefined);
    applied.write();
    const mcp = JSON.parse(
      fs.readFileSync(path.join(home, '.codebuddy', '.mcp.json'), 'utf8'),
    );
    assert.equal(mcp.mcpServers.astrolabe.command, 'npx');
    assert.equal(mcp.mcpServers.astrolabe.type, 'stdio');
    const settings = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(settings.hooks.PreToolUse.length, 1);
    assert.equal(settings.hooks.PreToolUse[0].hooks[0].command, 'echo keep');
    assert.equal(settings.hooks.SessionEnd, undefined);
    assert.equal(clientsMod.stripCodebuddyHooksSettings(settingsPath).skipped, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Grok MCP toml + automatic cleanup of legacy astrolabe.json', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.grok', 'hooks'), { recursive: true });
    fs.writeFileSync(path.join(home, '.grok', 'config.toml'), 'model = "x"\n');
    const legacyAstrolabe = path.join(home, '.grok', 'hooks', 'astrolabe.json');
    const siblingAiCon = path.join(home, '.grok', 'hooks', 'ai-con.json');
    fs.writeFileSync(
      legacyAstrolabe,
      JSON.stringify({
        hooks: {
          PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=grok' }] }],
          SessionEnd: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }],
        },
      }),
    );
    const siblingContent = JSON.stringify({ hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'ai-con hook-proxy' }] }] } });
    fs.writeFileSync(siblingAiCon, siblingContent);

    const client = clientsMod.getClient('grok');
    assert.equal(client.detect(home, { PATH: '/nonexistent' }).detected, true);
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    const plan = client.plan({ home, launch });
    const applied = plan.apply();
    applied.write();

    const toml = fs.readFileSync(path.join(home, '.grok', 'config.toml'), 'utf8');
    assert.match(toml, /model = "x"/);
    assert.match(toml, /\[mcp_servers\.astrolabe\]/);
    assert.match(toml, /command = "npx"/);
    assert.equal(fs.existsSync(legacyAstrolabe), false);
    assert.equal(fs.readFileSync(siblingAiCon, 'utf8'), siblingContent);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Kimi Code mcp.json + automatic cleanup of managed [[hooks]] toml', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.kimi-code'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.kimi-code', 'mcp.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'x', args: [] } } }, null, 2),
    );
    const tomlPath = path.join(home, '.kimi-code', 'config.toml');
    fs.writeFileSync(
      tomlPath,
      [
        'default_model = "kimi"',
        '',
        '[[hooks]]',
        'event = "Notification"',
        'command = "echo keep"',
        '',
        '# >>> astrolabe-managed-kimi-hooks >>>',
        '[[hooks]]',
        'event = "PreToolUse"',
        'command = "astrolabe hooks remind --client=kimicode"',
        '[[hooks]]',
        'event = "SessionEnd"',
        'command = "astrolabe hooks cleanup"',
        '# <<< astrolabe-managed-kimi-hooks <<<',
        '',
      ].join('\n'),
    );
    const client = clientsMod.getClient('kimi');
    assert.equal(client.id, 'kimicode');
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    const plan = client.plan({ home, launch });
    plan.apply().write();
    const mcp = JSON.parse(fs.readFileSync(path.join(home, '.kimi-code', 'mcp.json'), 'utf8'));
    assert.equal(mcp.mcpServers.keep.command, 'x');
    assert.equal(mcp.mcpServers.astrolabe.command, 'npx');
    const toml = fs.readFileSync(tomlPath, 'utf8');
    assert.match(toml, /default_model = "kimi"/);
    assert.match(toml, /echo keep/);
    assert.doesNotMatch(toml, /astrolabe-managed-kimi-hooks/);
    assert.doesNotMatch(toml, /--client=kimicode/);
    // idempotent
    const again = clientsMod.stripKimiHooksToml(tomlPath);
    assert.equal(again.skipped, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('ZCode mcp.servers + automatic stripping of Astrolabe hooks while keeping third-party hooks', () => {
  const home = tmpHome();
  try {
    const cli = path.join(home, '.zcode', 'cli');
    fs.mkdirSync(cli, { recursive: true });
    const configPath = path.join(cli, 'config.json');
    fs.writeFileSync(
      configPath,
      JSON.stringify(
        {
          mcp: { servers: { keep: { type: 'stdio', command: 'x', args: [] } } },
          hooks: {
            enabled: true,
            events: {
              SessionEnd: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }],
              Stop: [
                {
                  hooks: [
                    { type: 'command', command: 'astrolabe hooks cleanup' },
                    { type: 'command', command: 'echo keep-stop' },
                  ],
                },
              ],
              PreToolUse: [
                { hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=zcode' }] },
                { hooks: [{ type: 'command', command: 'echo keep' }] },
              ],
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
    const plan = client.plan({ home, launch });
    plan.apply().write();
    const doc = JSON.parse(fs.readFileSync(configPath, 'utf8'));
    assert.equal(doc.mcp.servers.keep.command, 'x');
    assert.equal(doc.mcp.servers.astrolabe.type, 'stdio');
    assert.equal(doc.mcp.servers.astrolabe.command, 'npx');
    assert.equal(doc.hooks.enabled, true);
    assert.equal(doc.hooks.events.SessionEnd, undefined);
    assert.ok(Array.isArray(doc.hooks.events.Stop));
    assert.equal(doc.hooks.events.Stop.length, 1);
    assert.equal(doc.hooks.events.Stop[0].hooks[0].command, 'echo keep-stop');
    assert.equal(doc.hooks.events.PreToolUse.length, 1);
    assert.equal(doc.hooks.events.PreToolUse[0].hooks[0].command, 'echo keep');
    assert.ok(!JSON.stringify(doc.hooks).includes('astrolabe'));

    // When only { enabled: true } + Astrolabe hooks existed, delete empty doc.hooks shell
    const home2 = tmpHome();
    try {
      const cli2 = path.join(home2, '.zcode', 'cli');
      fs.mkdirSync(cli2, { recursive: true });
      const cfg2 = path.join(cli2, 'config.json');
      fs.writeFileSync(
        cfg2,
        JSON.stringify(
          {
            hooks: {
              enabled: true,
              events: {
                PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=zcode' }] }],
                Stop: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }],
                SessionEnd: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }],
              },
            },
          },
          null,
          2,
        ),
      );
      const merged2 = clientsMod.mergeZcodeConfigFile(cfg2, launch);
      assert.equal(merged2.hooksRemoved, true);
      assert.ok(merged2.doc.mcp.servers.astrolabe);
      assert.equal(merged2.doc.hooks, undefined);
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

test('Cursor --with-hooks warns deprecated, writes mcp.json with --context=cursor, and strips legacy hooks.json', () => {
  const home = tmpHome();
  try {
    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    // Bad prior config: wrong client hooks + mcp.json with wrong context
    const hooksPath = path.join(home, '.cursor', 'hooks.json');
    fs.writeFileSync(
      hooksPath,
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
            sessionEnd: [
              {
                command: 'astrolabe hooks cleanup',
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
    assert.match(result.stderr, /--with-hooks is deprecated/);
    assert.equal(fs.existsSync(path.join(project, '.cursor', 'mcp.json')), false);
    assert.equal(fs.existsSync(path.join(project, '.cursor', 'rules', 'astrolabe.mdc')), false);

    const mcp = JSON.parse(fs.readFileSync(path.join(home, '.cursor', 'mcp.json'), 'utf8'));
    assert.equal(mcp.mcpServers.astrolabe.command, bin);
    assert.deepEqual(mcp.mcpServers.astrolabe.args, ['.', '--context=cursor']);
    assert.ok(!JSON.stringify(mcp).includes('claude-code'));

    // hooks.json backed up and deleted since no third-party hooks remained
    assert.equal(fs.existsSync(hooksPath), false);
    const backups = fs
      .readdirSync(path.join(home, '.cursor'))
      .filter((name) => name.startsWith('hooks.json.bak-'));
    assert.equal(backups.length, 1);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('ZCode mergeZcodeConfigFile strips all Astrolabe hooks including wrong client and Stop cleanup', () => {
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
                    {
                      type: 'command',
                      command: 'echo keep-pre',
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
    const plan = client.plan({ home, launch, env: noPathEnv });
    plan.apply().write();
    const doc = JSON.parse(fs.readFileSync(path.join(cli, 'config.json'), 'utf8'));
    assert.ok(doc.mcp.servers.astrolabe);
    assert.equal(doc.hooks.events.SessionEnd, undefined);
    // Stop cleanup & PreToolUse remind removed; non-Astrolabe hooks preserved
    const stopCmds = (doc.hooks.events.Stop || []).flatMap((e) =>
      (e.hooks || []).map((h) => h.command),
    );
    assert.deepEqual(stopCmds, ['echo keep-stop']);
    const preCmds = (doc.hooks.events.PreToolUse || []).flatMap((e) =>
      (e.hooks || []).map((h) => h.command),
    );
    assert.deepEqual(preCmds, ['echo keep-pre']);
    assert.ok(!JSON.stringify(doc.hooks).includes('astrolabe'));

    // Pure Astrolabe-only hooks configs delete empty doc.hooks shell completely
    const home2 = tmpHome();
    try {
      const cli2 = path.join(home2, '.zcode', 'cli');
      fs.mkdirSync(cli2, { recursive: true });
      fs.writeFileSync(
        path.join(cli2, 'config.json'),
        JSON.stringify(
          {
            hooks: {
              enabled: true,
              events: {
                PreToolUse: [
                  {
                    hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }],
                  },
                ],
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
        .plan({ home: home2, launch, env: noPathEnv })
        .apply()
        .write();
      const d2 = JSON.parse(fs.readFileSync(path.join(cli2, 'config.json'), 'utf8'));
      assert.ok(d2.mcp.servers.astrolabe);
      assert.equal(d2.hooks, undefined);
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
      assert.equal(fs.existsSync(path.join(home, '.cursor', 'hooks.json')), false);

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
      assert.equal(fs.existsSync(path.join(home, '.cursor', 'hooks.json')), false);
      assert.equal(fs.existsSync(path.join(project, '.cursor', 'hooks.json')), false);
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

test('setup backs up legacy hooks.json before deleting and is idempotent on re-run', () => {
  const home = tmpHome();
  try {
    const cursorDir = path.join(home, '.cursor');
    fs.mkdirSync(cursorDir, { recursive: true });
    const hooksPath = path.join(cursorDir, 'hooks.json');
    fs.writeFileSync(
      hooksPath,
      JSON.stringify(
        {
          version: 1,
          hooks: {
            preToolUse: [{ command: 'astrolabe hooks remind --client=cursor' }],
            sessionEnd: [{ command: 'astrolabe hooks cleanup' }],
          },
        },
        null,
        2,
      ),
    );
    const project = tmpProject(home);
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const env = { ...process.env, HOME: home, PATH: emptyPath };
    const args = [
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
    ];
    const first = spawnSync(process.execPath, args, { encoding: 'utf8', env });
    assert.equal(first.status, 0, first.stderr + first.stdout);
    assert.equal(fs.existsSync(hooksPath), false);
    const backupsAfterFirst = fs
      .readdirSync(cursorDir)
      .filter((name) => name.startsWith('hooks.json.bak-'));
    assert.equal(
      backupsAfterFirst.length,
      1,
      `expected 1 hooks.json backup, got ${backupsAfterFirst.join(', ')}`,
    );

    const second = spawnSync(process.execPath, args, { encoding: 'utf8', env });
    assert.equal(second.status, 0, second.stderr + second.stdout);
    assert.equal(fs.existsSync(hooksPath), false);
    const backupsAfterSecond = fs
      .readdirSync(cursorDir)
      .filter((name) => name.startsWith('hooks.json.bak-'));
    assert.equal(
      backupsAfterSecond.length,
      1,
      `second run must not create extra hooks.json backups, got ${backupsAfterSecond.join(', ')}`,
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
  assert.match(template, /Route by intent/);
  assert.match(template, /astrolabe-find_symbol/);
  assert.match(template, /\{\{INDEX_ROOT\}\}/);
});

test('FALLBACK_L1_TEMPLATE stays byte-for-byte in sync with crates/astrolabe-mcp/contexts/cursor-l1.md', () => {
  const canonicalPath = path.resolve(
    __dirname,
    '../../../../crates/astrolabe-mcp/contexts/cursor-l1.md',
  );
  const canonical = fs.readFileSync(canonicalPath, 'utf8');
  assert.equal(cursorMod.FALLBACK_L1_TEMPLATE, canonical);
  assert.equal(clientsMod.loadCursorL1Template({ existsFn: () => false }), canonical);
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

test('Claude Code --with-hooks warns deprecated, writes .claude.json, strips legacy hooks idempotently, and creates backups', () => {
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
              { matcher: '', hooks: [{ type: 'command', command: 'astrolabe hooks session-start' }] },
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
              { matcher: '', hooks: [{ type: 'command', command: '/old/astrolabe hooks cleanup' }] },
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
    assert.match(run1.stderr, /--with-hooks is deprecated/);

    const claudeDoc = JSON.parse(fs.readFileSync(claudeJsonPath, 'utf8'));
    assert.equal(claudeDoc.mcpServers.keep.command, 'keep');
    assert.equal(claudeDoc.mcpServers.astrolabe.command, bin1);
    assert.equal(claudeDoc.mcpServers.astrolabe.alwaysLoad, true);

    const s1 = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(s1.model, 'sonnet');
    assert.deepEqual(s1.permissions, { allow: ['Bash(git status)'] });
    assert.equal(s1.hooks.PostToolUse[0].hooks[0].command, 'echo post-keep');
    assert.equal(s1.hooks.SessionStart.length, 1);
    assert.equal(s1.hooks.SessionStart[0].hooks[0].command, 'echo session-keep');
    assert.equal(s1.hooks.PreToolUse.length, 1);
    assert.equal(s1.hooks.PreToolUse[0].hooks[0].command, 'echo pre-keep');
    assert.equal(s1.hooks.SessionEnd.length, 1);
    assert.equal(s1.hooks.SessionEnd[0].hooks[0].command, 'echo end-keep');

    // Backups created for both existing files
    assert.ok(fs.readdirSync(home).some((f) => f.startsWith('.claude.json.bak-')));
    const settingsBackups1 = fs
      .readdirSync(claudeDir)
      .filter((f) => f.startsWith('settings.json.bak-'));
    assert.equal(settingsBackups1.length, 1);

    // Second run is idempotent and does not create extra settings.json backups
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
    const settingsBackups2 = fs
      .readdirSync(claudeDir)
      .filter((f) => f.startsWith('settings.json.bak-'));
    assert.equal(settingsBackups2.length, 1);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Claude Code plan never creates missing settings.json and strips legacy hooks with or without astrolabe on PATH', () => {
  const home = tmpHome();
  try {
    const fakeBin = path.join(home, 'fakebin');
    fs.mkdirSync(fakeBin, { recursive: true });
    const binName = process.platform === 'win32' ? 'astrolabe.exe' : 'astrolabe';
    const onPathBin = path.join(fakeBin, binName);
    fs.writeFileSync(onPathBin, '');

    const client = clientsMod.getClient('claude-code');
    const launch = clientsMod.resolveLaunch({ context: client.context });

    // Never creates settings.json when absent
    const appliedNoSettings = client
      .plan({ home, launch, env: { ...process.env, PATH: fakeBin } })
      .apply();
    appliedNoSettings.write();
    const settingsPath = clientsMod.resolveClaudeSettingsPath(home);
    assert.equal(fs.existsSync(settingsPath), false);

    // When settings.json exists with PATH-resolved & bare astrolabe legacy hooks, strips both
    fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
    fs.writeFileSync(
      settingsPath,
      JSON.stringify({
        model: 'opus',
        hooks: {
          SessionStart: [{ matcher: '', hooks: [{ type: 'command', command: `${onPathBin} hooks session-start` }] }],
          PreToolUse: [{ matcher: '*', hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }] }],
          SessionEnd: [{ matcher: '', hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }],
        },
      }),
    );

    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const appliedStrip = client
      .plan({ home, launch, env: { ...process.env, PATH: emptyPath } })
      .apply();
    appliedStrip.write();

    const after = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(after.model, 'opus');
    assert.equal(after.hooks, undefined);
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

test('npx environment without astrolabe on PATH warns --with-hooks deprecated, never creates settings.json, and strips existing legacy hooks even when resolveBinary fails', async () => {
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const cacheDir = path.join(home, 'npx-cache');
    fs.mkdirSync(cacheDir, { recursive: true });
    const binName = process.platform === 'win32' ? 'astrolabe.exe' : 'astrolabe';
    const packagedBin = path.join(cacheDir, binName);
    fs.writeFileSync(packagedBin, '');

    const settingsPath = clientsMod.resolveClaudeSettingsPath(home);
    fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
    const quotedPackaged = cursorMod.quoteCommandPath(packagedBin);
    fs.writeFileSync(
      settingsPath,
      JSON.stringify({
        hooks: {
          SessionStart: [{ matcher: '', hooks: [{ type: 'command', command: `${quotedPackaged} hooks session-start` }] }],
          PreToolUse: [{ matcher: '*', hooks: [{ type: 'command', command: 'npx -y @zlnap/astrolabe hooks remind --client=claude-code' }] }],
          SessionEnd: [{ matcher: '', hooks: [{ type: 'command', command: `${quotedPackaged} hooks cleanup` }] }],
        },
      }),
    );

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
    assert.match(res.stderr, /--with-hooks is deprecated/);

    const claudeDoc = JSON.parse(fs.readFileSync(path.join(home, '.claude.json'), 'utf8'));
    assert.equal(claudeDoc.mcpServers.astrolabe.command, 'npx');
    assert.deepEqual(claudeDoc.mcpServers.astrolabe.args, [
      '-y',
      '@zlnap/astrolabe',
      '.',
      '--context=claude-code',
    ]);
    const strippedSettings = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
    assert.equal(strippedSettings.hooks, undefined);

    // Without settings.json and with resolveBinary throwing: never creates settings.json; strips existing npx hooks if present.
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
      assert.equal(fs.existsSync(clientsMod.resolveClaudeSettingsPath(home2)), false);
      const doc2 = JSON.parse(fs.readFileSync(path.join(home2, '.claude.json'), 'utf8'));
      assert.equal(doc2.mcpServers.astrolabe.command, 'npx');
      assert.equal(doc2.mcpServers.astrolabe.alwaysLoad, true);
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

test('Codebuddy, Grok, Kimi Code, ZCode, and OpenCode strip POSIX-quoted, Windows cmd.exe, .cmd, bare, gc, and npx legacy hooks while keeping third-party hooks', () => {
  const home = tmpHome();
  try {
    const posixBin = "/Users/o'neill/My Bin/astrolabe";
    const posixQuoted = "'/Users/o'\\''neill/My Bin/astrolabe'";
    const winBin = 'C:\\Tools(x86)\\A&B\\astrolabe.exe';
    const winQuoted = `"${winBin}"`;

    // 1. Codebuddy: strip POSIX-quoted, Windows .exe, .cmd, bare astrolabe, gc, npx while keeping third-party & mixed-group
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
        { type: 'command', command: `${posixQuoted} hooks gc` },
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
              { matcher: '', hooks: [{ type: 'command', command: `${winQuoted} hooks remind --client=codebuddy` }] },
              { matcher: '', hooks: [{ type: 'command', command: 'npx -y @zlnap/astrolabe hooks remind --client=codebuddy' }] },
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
    const cb1 = codebuddyMod.stripCodebuddyHooksSettings(cbSettings, {
      binPath: posixBin,
      platform: 'darwin',
    });
    assert.equal(cb1.replaced, true);
    assert.equal(cb1.doc.hooks.PreToolUse.length, 2);
    assert.deepEqual(cb1.doc.hooks.PreToolUse[0], cbThirdParty);
    assert.deepEqual(cb1.doc.hooks.PreToolUse[1], {
      matcher: 'Read',
      hooks: [{ type: 'command', command: 'echo keep-mixed' }],
    });
    assert.equal(cb1.doc.hooks.SessionEnd.length, 1);
    assert.deepEqual(cb1.doc.hooks.SessionEnd[0], {
      matcher: '',
      hooks: [{ type: 'command', command: 'echo keep-end' }],
    });
    fs.writeFileSync(cbSettings, JSON.stringify(cb1.doc, null, 2));
    assert.equal(codebuddyMod.stripCodebuddyHooksSettings(cbSettings).skipped, true);

    // 2. Grok: strip POSIX/Windows/.cmd/bare/gc/npx hooks; preserve customKey/third-party, or delete when empty
    const grokFile = path.join(home, 'grok-astrolabe.json');
    fs.writeFileSync(
      grokFile,
      JSON.stringify(
        {
          customKey: 'keep',
          hooks: {
            PreToolUse: [
              { hooks: [{ type: 'command', command: `${posixQuoted} hooks remind --client=grok` }] },
              { hooks: [{ type: 'command', command: 'npx -y @zlnap/astrolabe@0.17.12 hooks gc' }] },
              { hooks: [{ type: 'command', command: 'echo keep-grok' }] },
            ],
            SessionEnd: [
              { hooks: [{ type: 'command', command: `${winQuoted} hooks cleanup` }] },
            ],
          },
        },
        null,
        2,
      ),
    );
    const g1 = grokMod.stripGrokHooksFile(grokFile, { binPath: winBin, platform: 'win32' });
    assert.equal(g1.deleted, false);
    assert.equal(g1.doc.customKey, 'keep');
    assert.equal(g1.doc.hooks.PreToolUse.length, 1);
    assert.equal(g1.doc.hooks.PreToolUse[0].hooks[0].command, 'echo keep-grok');
    assert.equal(g1.doc.hooks.SessionEnd, undefined);
    fs.writeFileSync(grokFile, JSON.stringify(g1.doc, null, 2));
    assert.equal(grokMod.stripGrokHooksFile(grokFile).skipped, true);

    const grokOnlyFile = path.join(home, 'grok-only.json');
    fs.writeFileSync(
      grokOnlyFile,
      JSON.stringify({
        hooks: {
          PreToolUse: [{ hooks: [{ type: 'command', command: '"C:\\bin\\astrolabe.cmd" hooks remind --client=grok' }] }],
        },
      }),
    );
    const gOnly = grokMod.stripGrokHooksFile(grokOnlyFile);
    assert.equal(gOnly.deleted, true);

    // 3. Kimi Code: strip managed block with Windows/POSIX/npx commands while preserving default_model and user [[hooks]]
    const kimiToml = path.join(home, 'kimi-config.toml');
    fs.writeFileSync(
      kimiToml,
      [
        'default_model = "kimi"',
        '',
        '[[hooks]]',
        'event = "Notification"',
        'command = "echo keep-kimi"',
        '',
        kimicodeMod.MANAGED_BEGIN,
        '[[hooks]]',
        'event = "PreToolUse"',
        `command = ${JSON.stringify(`${winQuoted} hooks remind --client=kimicode`)}`,
        '[[hooks]]',
        'event = "SessionEnd"',
        `command = ${JSON.stringify(`${posixQuoted} hooks cleanup`)}`,
        kimicodeMod.MANAGED_END,
        '',
      ].join('\n'),
    );
    const kStrip = kimicodeMod.stripKimiHooksToml(kimiToml);
    assert.equal(kStrip.replaced, true);
    assert.equal(kStrip.removed, true);
    assert.match(kStrip.text, /default_model = "kimi"/);
    assert.match(kStrip.text, /echo keep-kimi/);
    assert.doesNotMatch(kStrip.text, /astrolabe-managed-kimi-hooks/);
    fs.writeFileSync(kimiToml, kStrip.text);
    assert.equal(kimicodeMod.stripKimiHooksToml(kimiToml).skipped, true);

    // 4. ZCode: strip quoted/.exe/.cmd/npx/gc/wrong-client + mixed group + Stop cleanup, write MCP
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
                    { type: 'command', command: 'npx -y astrolabe hooks gc' },
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
      binPath: posixBin,
      platform: 'darwin',
    });
    assert.equal(z1.hooksRemoved, true);
    assert.ok(z1.doc.mcp.servers.astrolabe);
    assert.equal(z1.doc.hooks.events.PreToolUse.length, 1);
    assert.deepEqual(z1.doc.hooks.events.PreToolUse[0].hooks, [
      { type: 'command', command: 'echo keep-zcode-mixed' },
    ]);
    assert.deepEqual(z1.doc.hooks.events.Stop[0].hooks, [
      { type: 'command', command: 'echo keep-zcode-stop' },
    ]);
    fs.writeFileSync(zcodeFile, JSON.stringify(z1.doc, null, 2));
    const z2 = zcodeMod.mergeZcodeConfigFile(zcodeFile, launch, { writeMcp: false });
    assert.equal(z2.skipped, true);

    // 5. OpenCode: stripOpenCodeGuardPluginFile removes legacy guard plugin and skips non-guard plugin
    const ocGuard = path.join(home, 'astrolabe-guard.ts');
    fs.writeFileSync(
      ocGuard,
      `const ASTROLABE_BIN = ${JSON.stringify(posixBin)};\nexport const AstrolabeGuard = async () => ({ "tool.execute.before": () => {} });\n`,
    );
    const ocRes = opencodeMod.stripOpenCodeGuardPluginFile(ocGuard);
    assert.equal(ocRes.deleted, true);

    const ocCustom = path.join(home, 'custom-plugin.ts');
    fs.writeFileSync(ocCustom, 'export const CustomPlugin = async () => ({});\n');
    const ocCustomRes = opencodeMod.stripOpenCodeGuardPluginFile(ocCustom);
    assert.equal(ocCustomRes.skipped, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('run --with-hooks prints deprecation note, configures MCPs, and cleans pre-existing legacy hooks across 5 clients without creating missing hook files', () => {
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

    // First run with NO pre-existing hook files: configures MCPs without creating hook files
    const res1 = spawnSync(
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
    assert.equal(res1.status, 0, res1.stderr + res1.stdout);
    assert.match(res1.stderr, /--with-hooks is deprecated/);

    assert.equal(fs.existsSync(path.join(home, '.codebuddy', '.mcp.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.codebuddy', 'settings.json')), false);
    assert.equal(fs.existsSync(path.join(home, '.grok', 'config.toml')), true);
    assert.equal(fs.existsSync(path.join(home, '.grok', 'hooks', 'astrolabe.json')), false);
    assert.equal(fs.existsSync(path.join(home, '.kimi-code', 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.kimi-code', 'config.toml')), false);
    assert.equal(fs.existsSync(path.join(home, '.zcode', 'cli', 'config.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.config', 'opencode', 'opencode.jsonc')), true);
    assert.equal(
      fs.existsSync(path.join(home, '.config', 'opencode', 'plugins', 'astrolabe-guard.ts')),
      false,
    );

    // Seed pre-existing legacy hooks/plugins across all 5 clients and re-run
    const quoted = cursorMod.quoteCommandPath(packagedBin);
    fs.writeFileSync(
      path.join(home, '.codebuddy', 'settings.json'),
      JSON.stringify({
        hooks: {
          PreToolUse: [{ hooks: [{ type: 'command', command: `${quoted} hooks remind --client=codebuddy` }] }],
        },
      }),
    );
    fs.mkdirSync(path.join(home, '.grok', 'hooks'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.grok', 'hooks', 'astrolabe.json'),
      JSON.stringify({
        hooks: {
          PreToolUse: [{ hooks: [{ type: 'command', command: `${quoted} hooks remind --client=grok` }] }],
        },
      }),
    );
    fs.writeFileSync(
      path.join(home, '.kimi-code', 'config.toml'),
      `default_model = "kimi"\n\n${kimicodeMod.MANAGED_BEGIN}\n[[hooks]]\nevent = "PreToolUse"\ncommand = ${JSON.stringify(`${quoted} hooks remind --client=kimicode`)}\n${kimicodeMod.MANAGED_END}\n`,
    );
    fs.writeFileSync(
      path.join(home, '.zcode', 'cli', 'config.json'),
      JSON.stringify({
        hooks: {
          enabled: true,
          events: {
            PreToolUse: [{ hooks: [{ type: 'command', command: `${quoted} hooks remind --client=zcode` }] }],
          },
        },
      }),
    );
    fs.mkdirSync(path.join(home, '.config', 'opencode', 'plugins'), { recursive: true });
    fs.writeFileSync(
      path.join(home, '.config', 'opencode', 'plugins', 'astrolabe-guard.ts'),
      `const ASTROLABE_BIN = ${JSON.stringify(packagedBin)};\nexport const AstrolabeGuard = async () => ({});\n`,
    );

    const res2 = spawnSync(
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
    assert.equal(res2.status, 0, res2.stderr + res2.stdout);

    const cb = JSON.parse(fs.readFileSync(path.join(home, '.codebuddy', 'settings.json'), 'utf8'));
    assert.equal(cb.hooks, undefined);
    assert.equal(fs.existsSync(path.join(home, '.grok', 'hooks', 'astrolabe.json')), false);
    const kimi = fs.readFileSync(path.join(home, '.kimi-code', 'config.toml'), 'utf8');
    assert.match(kimi, /default_model = "kimi"/);
    assert.doesNotMatch(kimi, /astrolabe-managed-kimi-hooks/);
    const zcode = JSON.parse(fs.readFileSync(path.join(home, '.zcode', 'cli', 'config.json'), 'utf8'));
    assert.ok(zcode.mcp.servers.astrolabe);
    assert.equal(zcode.hooks, undefined);
    assert.equal(
      fs.existsSync(path.join(home, '.config', 'opencode', 'plugins', 'astrolabe-guard.ts')),
      false,
    );
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
    const claudeSettings = path.join(claudeDir, 'settings.json');
    fs.writeFileSync(
      claudeSettings,
      JSON.stringify({ hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }] }] } }),
    );
    const claudeEnv = { PATH: '/nonexistent', CLAUDE_CONFIG_DIR: '  ~/custom-claude  ' };
    const cc = clientsMod.getClient('claude-code');
    assert.equal(cc.detect(home, claudeEnv).detected, true);
    const ccPlan = cc.plan({ home, launch, env: claudeEnv }).apply();
    ccPlan.write();
    assert.equal(fs.existsSync(path.join(claudeDir, '.claude.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.claude.json')), false);
    assert.equal(JSON.parse(fs.readFileSync(claudeSettings, 'utf8')).hooks, undefined);

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
    const cursorHooks = path.join(cursorDir, 'hooks.json');
    fs.writeFileSync(
      cursorHooks,
      JSON.stringify({ version: 1, hooks: { preToolUse: [{ command: 'astrolabe hooks remind --client=cursor' }] } }),
    );
    const cursorEnv = { PATH: '/nonexistent', CURSOR_CONFIG_DIR: cursorDir };
    const cursor = clientsMod.getClient('cursor');
    assert.equal(cursor.detect(home, cursorEnv).detected, true);
    const cursorApplied = cursor
      .plan({
        home,
        launch,
        cursorBinPath: '/abs/astrolabe',
        env: cursorEnv,
        detected: true,
      })
      .apply();
    cursorApplied.write();
    assert.equal(fs.existsSync(path.join(cursorDir, 'mcp.json')), true);
    assert.equal(fs.existsSync(cursorHooks), false);
    const perms = clientsMod.mergeCursorPermissions(home, { env: cursorEnv });
    assert.equal(perms.filePath, path.join(cursorDir, 'permissions.json'));
    assert.equal(
      cursorMod.resolveApprovalPath(home, home, { env: cursorEnv }).startsWith(cursorDir),
      true,
    );

    // OpenCode: XDG_CONFIG_HOME and OPENCODE_CONFIG
    const xdgDir = path.join(home, 'xdg-cfg');
    fs.mkdirSync(path.join(xdgDir, 'opencode', 'plugins'), { recursive: true });
    const xdgPlugin = path.join(xdgDir, 'opencode', 'plugins', 'astrolabe-guard.ts');
    fs.writeFileSync(xdgPlugin, 'export const AstrolabeGuard = async () => ({});\n');
    const ocXdgEnv = { PATH: '/nonexistent', XDG_CONFIG_HOME: xdgDir };
    const oc = clientsMod.getClient('opencode');
    assert.equal(oc.detect(home, ocXdgEnv).detected, true);
    const ocXdgApplied = oc.plan({ home, launch, env: ocXdgEnv }).apply();
    assert.equal(ocXdgApplied.results[0].filePath, path.join(xdgDir, 'opencode', 'opencode.jsonc'));
    assert.equal(ocXdgApplied.results[1].filePath, xdgPlugin);
    assert.equal(ocXdgApplied.results[1].deleted, true);

    const ocFile = path.join(home, 'oc-single', 'my-opencode.json');
    fs.mkdirSync(path.dirname(ocFile), { recursive: true });
    fs.writeFileSync(ocFile, '{}\n');
    const ocSingleEnv = { PATH: '/nonexistent', OPENCODE_CONFIG: ocFile };
    assert.equal(oc.detect(home, ocSingleEnv).detected, true);
    const ocSinglePlan = oc.plan({ home, launch, env: ocSingleEnv });
    assert.equal(
      ocSinglePlan.steps[1].filePath,
      path.join(home, '.config', 'opencode', 'plugins', 'astrolabe-guard.ts'),
    );
    const ocSingleApplied = ocSinglePlan.apply();
    assert.equal(ocSingleApplied.results[0].filePath, ocFile);

    // Codebuddy: CODEBUDDY_HOME
    const cbDir = path.join(home, 'custom-cb');
    fs.mkdirSync(cbDir, { recursive: true });
    const cbSettings = path.join(cbDir, 'settings.json');
    fs.writeFileSync(
      cbSettings,
      JSON.stringify({ hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=codebuddy' }] }] } }),
    );
    const cbEnv = { PATH: '/nonexistent', CODEBUDDY_HOME: cbDir };
    const cb = clientsMod.getClient('codebuddy');
    assert.equal(cb.detect(home, cbEnv).detected, true);
    const cbApplied = cb.plan({ home, launch, env: cbEnv }).apply();
    assert.equal(cbApplied.results[0].filePath, path.join(cbDir, '.mcp.json'));
    assert.equal(cbApplied.results[1].filePath, cbSettings);
    assert.equal(cbApplied.results[1].removed, true);

    // Grok: GROK_HOME
    const grokDir = path.join(home, 'custom-grok');
    fs.mkdirSync(path.join(grokDir, 'hooks'), { recursive: true });
    const grokHooks = path.join(grokDir, 'hooks', 'astrolabe.json');
    fs.writeFileSync(
      grokHooks,
      JSON.stringify({ hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=grok' }] }] } }),
    );
    const grokEnv = { PATH: '/nonexistent', GROK_HOME: grokDir };
    const grok = clientsMod.getClient('grok');
    assert.equal(grok.detect(home, grokEnv).detected, true);
    const grokApplied = grok.plan({ home, launch, env: grokEnv }).apply();
    assert.equal(grokApplied.results[0].filePath, path.join(grokDir, 'config.toml'));
    assert.equal(grokApplied.results[1].filePath, grokHooks);
    assert.equal(grokApplied.results[1].deleted, true);

    // Kimi Code: KIMI_CONFIG_DIR
    const kimiDir = path.join(home, 'custom-kimi');
    fs.mkdirSync(kimiDir, { recursive: true });
    const kimiToml = path.join(kimiDir, 'config.toml');
    fs.writeFileSync(
      kimiToml,
      `${kimicodeMod.MANAGED_BEGIN}\n[[hooks]]\nevent = "PreToolUse"\ncommand = "astrolabe hooks remind --client=kimicode"\n${kimicodeMod.MANAGED_END}\n`,
    );
    const kimiEnv = { PATH: '/nonexistent', KIMI_CONFIG_DIR: kimiDir };
    const kimi = clientsMod.getClient('kimicode');
    assert.equal(kimi.detect(home, kimiEnv).detected, true);
    const kimiApplied = kimi.plan({ home, launch, env: kimiEnv }).apply();
    assert.equal(kimiApplied.results[0].filePath, path.join(kimiDir, 'mcp.json'));
    assert.equal(kimiApplied.results[1].filePath, kimiToml);
    assert.equal(kimiApplied.results[1].removed, true);

    // ZCode: ZCODE_HOME (both cli/config.json and direct config.json without cli/)
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
    const dshClient = clientsMod.getClient('deepseek-harness');
    assert.equal(dshClient.detect(home, dshEnv).detected, false);
    assert.equal(
      dshMod.resolveDshPatchPath(home, dshEnv),
      path.join(dshDir, 'cordis.patch.yml'),
    );
    const dshGuardDir = path.join(dshDir, 'profiles', 'node_modules', 'astrolabe-dsh-guard');
    fs.mkdirSync(dshGuardDir, { recursive: true });
    fs.writeFileSync(
      path.join(dshDir, 'cordis.patch.yml'),
      `${dshMod.MANAGED_BEGIN}\n- insert:\n    - id: astrolabe-dsh-guard\n${dshMod.MANAGED_END}\n`,
    );
    assert.equal(dshClient.detect(home, dshEnv).detected, true);
    const dshApplied = dshClient.plan({ home, launch, env: dshEnv }).apply();
    assert.equal(dshApplied.skipped, undefined);
    dshApplied.write();
    assert.equal(fs.existsSync(dshGuardDir), false);
    assert.equal(fs.readFileSync(path.join(dshDir, 'cordis.patch.yml'), 'utf8'), '');
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
    // 1. Claude Code multi-profile --settings + --mcp-config writes MCP, strips Astrolabe hooks from all profiles, and creates backups
    const customMcp = path.join(home, 'profiles', 'claude-work.json');
    const s1 = path.join(home, 'profiles', 'settings.json');
    const s2 = path.join(home, 'profiles', 'settings.ccm.json');
    fs.mkdirSync(path.dirname(customMcp), { recursive: true });
    fs.writeFileSync(customMcp, JSON.stringify({ theme: 'dark' }, null, 2));
    fs.writeFileSync(
      s1,
      JSON.stringify(
        {
          model: 'sonnet',
          hooks: {
            SessionStart: [
              { matcher: '', hooks: [{ type: 'command', command: 'astrolabe hooks session-start' }] },
            ],
          },
        },
        null,
        2,
      ),
    );
    fs.writeFileSync(
      s2,
      JSON.stringify(
        {
          hooks: {
            PreToolUse: [
              { matcher: 'Bash', hooks: [{ type: 'command', command: 'echo keep-ccm' }] },
              {
                matcher: 'Read',
                hooks: [{ type: 'command', command: '/old/astrolabe hooks remind --client=claude-code' }],
              },
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
    assert.equal(s1Out.hooks, undefined);
    const s2Out = JSON.parse(fs.readFileSync(s2, 'utf8'));
    assert.equal(s2Out.hooks.PreToolUse.length, 1);
    assert.equal(s2Out.hooks.PreToolUse[0].hooks[0].command, 'echo keep-ccm');
    const profileFiles = fs.readdirSync(path.dirname(customMcp));
    assert.ok(profileFiles.some((f) => f.startsWith('claude-work.json.bak-')));
    assert.ok(profileFiles.some((f) => f.startsWith('settings.json.bak-')));
    assert.ok(profileFiles.some((f) => f.startsWith('settings.ccm.json.bak-')));

    // 2. Cursor NOT installed under ~/.cursor still lands via --clients=cursor --config-dir=... without creating hooks.json
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
    assert.match(curRes.stderr, /--with-hooks is deprecated/);
    assert.equal(fs.existsSync(path.join(customCursorDir, 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(customCursorDir, 'hooks.json')), false);
    assert.equal(fs.existsSync(path.join(customCursorDir, 'permissions.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.cursor')), false);

    // 3. ZCode custom setting.json + single/dual file overrides write MCP and strip legacy hooks
    const zSettingDir = path.join(home, 'zcode-custom');
    fs.mkdirSync(zSettingDir, { recursive: true });
    const zSettingFile = path.join(zSettingDir, 'setting.json');
    fs.writeFileSync(
      zSettingFile,
      JSON.stringify(
        {
          theme: 'light',
          hooks: {
            enabled: true,
            events: {
              PreToolUse: [
                { hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=zcode' }] },
              ],
            },
          },
        },
        null,
        2,
      ),
    );
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
    assert.equal(zSettingOut.hooks, undefined);

    // ZCode --hooks-config only cleans legacy hooks from specified file while writing MCP to default ZCode config
    const zHooksOnly = path.join(home, 'zcode-hooks-only.json');
    fs.writeFileSync(
      zHooksOnly,
      JSON.stringify({
        keepOther: true,
        hooks: {
          enabled: true,
          events: {
            PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=zcode' }] }],
          },
        },
      }),
    );
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
    assert.equal(zHooksOnlyDoc.keepOther, true);
    assert.equal(zHooksOnlyDoc.mcp, undefined);
    assert.equal(zHooksOnlyDoc.hooks, undefined);
    const zDefaultDoc = JSON.parse(
      fs.readFileSync(path.join(home, '.zcode', 'cli', 'config.json'), 'utf8'),
    );
    assert.equal(zDefaultDoc.mcp.servers.astrolabe.command, '/abs/astrolabe');

    // ZCode both --mcp-config and --hooks-config writes MCP to --mcp-config and strips legacy hooks from --hooks-config
    const zSplitMcp = path.join(home, 'zcode-split-mcp.json');
    const zSplitHooks = path.join(home, 'zcode-split-hooks.json');
    fs.writeFileSync(
      zSplitHooks,
      JSON.stringify({
        hooks: {
          enabled: true,
          events: {
            PreToolUse: [
              { hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=zcode' }] },
              { hooks: [{ type: 'command', command: 'echo keep-zcode-split' }] },
            ],
          },
        },
      }),
    );
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
    assert.equal(zSplitHooksDoc.hooks.events.PreToolUse.length, 1);
    assert.equal(zSplitHooksDoc.hooks.events.PreToolUse[0].hooks[0].command, 'echo keep-zcode-split');

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
        cursorBinPath: '/abs/astrolabe',
        projectRoot: home,
        configDir: '   ',
        mcpConfigPath: '   ',
        hooksConfigPaths: ['   ', ''],
        env: { PATH: '/nonexistent' },
      });
      for (const step of plan.steps) {
        assert.ok(step.filePath && step.filePath.trim() === step.filePath);
        assert.notEqual(step.filePath, path.resolve('   '));
        assert.notEqual(step.filePath, process.cwd());
        if (String(step.kind).includes('cleanup')) {
          assert.equal(step.apply().skipped, true);
        }
      }
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

    // ZCode merges symlinked MCP + hooks paths into one single write (writes MCP + strips legacy hooks)
    const zcodeFile = path.join(home, 'zcode.json');
    const zcodeLink = path.join(home, 'zcode-link.json');
    fs.writeFileSync(
      zcodeFile,
      JSON.stringify({
        hooks: {
          enabled: true,
          events: {
            PreToolUse: [
              { hooks: [{ type: 'command', command: '/abs/astrolabe hooks remind --client=zcode' }] },
              { hooks: [{ type: 'command', command: 'echo keep-zcode' }] },
            ],
          },
        },
      }),
    );
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
    assert.equal(zDoc.hooks.events.PreToolUse.length, 1);
    assert.equal(zDoc.hooks.events.PreToolUse[0].hooks[0].command, 'echo keep-zcode');
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

test('Regression 5: non-existent custom dirs do not skip MCP, never create hook files, and cursor.approvalEnv forwards CURSOR_CONFIG_DIR', () => {
  const home = tmpHome();
  try {
    const launch = { command: '/abs/astrolabe', args: ['.'] };

    // 1. Cursor with detected: true and missing ~/.cursor creates mcp.json without creating hooks.json
    const cursorApplied = clientsMod.getClient('cursor').plan({
      home,
      launch,
      detected: true,
      cursorBinPath: '/abs/astrolabe',
      projectRoot: home,
      env: { PATH: '/nonexistent' },
    }).apply();
    cursorApplied.write();
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'hooks.json')), false);

    // 2. codebuddy / grok / kimicode / zcode with non-existent env dirs create MCP files without creating hook files
    const cbEnvDir = path.join(home, 'new-cb-env');
    const cbApplied = clientsMod.getClient('codebuddy').plan({
      home,
      launch,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', CODEBUDDY_CONFIG_DIR: cbEnvDir },
    }).apply();
    assert.equal(Boolean(cbApplied.results[0].skipped), false);
    assert.equal(cbApplied.results[1].skipped, true);
    cbApplied.write();
    assert.equal(fs.existsSync(path.join(cbEnvDir, '.mcp.json')), true);
    assert.equal(fs.existsSync(path.join(cbEnvDir, 'settings.json')), false);

    const grokEnvDir = path.join(home, 'new-grok-env');
    const grokApplied = clientsMod.getClient('grok').plan({
      home,
      launch,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', GROK_HOME: grokEnvDir },
    }).apply();
    assert.equal(Boolean(grokApplied.results[0].skipped), false);
    assert.equal(grokApplied.results[1].skipped, true);
    grokApplied.write();
    assert.equal(fs.existsSync(path.join(grokEnvDir, 'config.toml')), true);
    assert.equal(fs.existsSync(path.join(grokEnvDir, 'hooks', 'astrolabe.json')), false);

    const kimiEnvDir = path.join(home, 'new-kimi-env');
    const kimiApplied = clientsMod.getClient('kimicode').plan({
      home,
      launch,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', KIMI_HOME: kimiEnvDir },
    }).apply();
    assert.equal(Boolean(kimiApplied.results[0].skipped), false);
    assert.equal(kimiApplied.results[1].skipped, true);
    kimiApplied.write();
    assert.equal(fs.existsSync(path.join(kimiEnvDir, 'mcp.json')), true);
    assert.equal(fs.existsSync(path.join(kimiEnvDir, 'config.toml')), false);

    const zcodeEnvDir = path.join(home, 'new-zcode-env');
    const zcodeApplied = clientsMod.getClient('zcode').plan({
      home,
      launch,
      binPath: '/abs/astrolabe',
      env: { PATH: '/nonexistent', ZCODE_CONFIG_DIR: zcodeEnvDir },
    }).apply();
    assert.equal(Boolean(zcodeApplied.skipped), false);
    zcodeApplied.write();
    assert.equal(fs.existsSync(path.join(zcodeEnvDir, 'cli', 'config.json')), true);
    assert.equal(
      JSON.parse(fs.readFileSync(path.join(zcodeEnvDir, 'cli', 'config.json'), 'utf8')).hooks,
      undefined,
    );

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

test('Regression: isAstrolabeHookCommand handles Windows npx.cmd, win32 case-insensitive binPath, and strict npx positional/package args', () => {
  assert.equal(
    cursorMod.isAstrolabeHookCommand('C:\\nodejs\\npx.cmd -y @zlnap/astrolabe hooks remind', {
      platform: 'win32',
    }),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('C:\\nodejs\\npx.cmd -y @zlnap/astrolabe hooks remind'),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx -y @other/pkg --config=/tmp/astrolabe hooks cleanup'),
    false,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx -p @zlnap/astrolabe@0.5.0 runner hooks remind'),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx --package @zlnap/astrolabe runner hooks remind'),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx --package=@zlnap/astrolabe runner hooks cleanup'),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx -p @other/pkg astrolabe hooks cleanup'),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx -- -p @zlnap/astrolabe hooks remind'),
    false,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('c:\\tools\\custom.exe hooks cleanup', {
      binPath: 'C:\\Tools\\Custom.exe',
      platform: 'win32',
    }),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('"C:\\Tools\\Custom.exe" hooks cleanup', {
      binPath: 'c:\\tools\\custom.exe',
      platform: 'win32',
    }),
    true,
  );
});

test('Regression: unrelated empty event arrays are preserved when stripping hooks in Claude Code, Cursor, and Codebuddy', () => {
  const home = tmpHome();
  try {
    const ccFile = path.join(home, 'cc-settings.json');
    fs.writeFileSync(
      ccFile,
      JSON.stringify({
        hooks: {
          PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }] }],
          PostToolUse: [],
        },
      }),
    );
    const ccRes = claudeCodeMod.stripClaudeCodeHooksSettings(ccFile);
    assert.equal(ccRes.skipped, undefined);
    assert.deepEqual(ccRes.doc.hooks, { PostToolUse: [] });

    const curFile = path.join(home, 'cursor-hooks.json');
    fs.writeFileSync(
      curFile,
      JSON.stringify({
        version: 1,
        hooks: {
          preToolUse: [{ command: 'astrolabe hooks remind --client=cursor' }],
          afterFileEdit: [],
        },
      }),
    );
    const curRes = cursorMod.stripCursorHooksFile(curFile);
    assert.equal(curRes.skipped, undefined);
    assert.equal(curRes.deleted, false);
    assert.deepEqual(curRes.doc.hooks, { afterFileEdit: [] });

    const cbFile = path.join(home, 'cb-settings.json');
    fs.writeFileSync(
      cbFile,
      JSON.stringify({
        hooks: {
          SessionEnd: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }],
          PostToolUse: [],
        },
      }),
    );
    const cbRes = codebuddyMod.stripCodebuddyHooksSettings(cbFile);
    assert.equal(cbRes.skipped, undefined);
    assert.deepEqual(cbRes.doc.hooks, { PostToolUse: [] });
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression: discoverClaudeSettingsPaths follows symlinks to regular files and read-only warning prints valid command', () => {
  const home = tmpHome();
  try {
    const claudeDir = path.join(home, '.claude');
    fs.mkdirSync(claudeDir, { recursive: true });
    const targetFile = path.join(home, 'real-profile.json');
    fs.writeFileSync(targetFile, JSON.stringify({ hooks: {} }));
    const linkProfile = path.join(claudeDir, 'settings-linked.json');
    fs.symlinkSync(targetFile, linkProfile);
    const danglingProfile = path.join(claudeDir, 'settings-dangling.json');
    fs.symlinkSync(path.join(home, 'does-not-exist.json'), danglingProfile);

    const discovered = claudeCodeMod.discoverClaudeSettingsPaths(home);
    assert.equal(discovered.includes(linkProfile), true);
    assert.equal(discovered.includes(danglingProfile), false);

    const project = tmpProject(home);
    const projSettings = path.join(project, '.claude', 'settings.json');
    fs.mkdirSync(path.dirname(projSettings), { recursive: true });
    fs.writeFileSync(
      projSettings,
      JSON.stringify({
        hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind' }] }] },
      }),
    );
    const applied = clientsMod
      .getClient('claude-code')
      .plan({
        home,
        launch: { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.', '--context=claude-code'] },
        projectRoot: project,
        explicitProject: true,
        removeHooks: true,
        env: { PATH: '/nonexistent' },
      })
      .apply();
    assert.equal(applied.warnings.length, 1);
    assert.equal(
      applied.warnings[0].includes(
        `Run: astrolabe setup --clients=claude-code --settings ${JSON.stringify(projSettings)} --yes`,
      ),
      true,
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression: --hooks-config cleans BOTH default hook file AND extra file, and cursor --remove-hooks --command matches custom binary', () => {
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });

    // Cursor: default ~/.cursor/hooks.json + extra hooks file, with custom non-PATH --command
    const cursorDir = path.join(home, '.cursor');
    fs.mkdirSync(cursorDir, { recursive: true });
    const defaultCursorHooks = path.join(cursorDir, 'hooks.json');
    const extraCursorHooks = path.join(home, 'extra-cursor-hooks.json');
    fs.writeFileSync(
      defaultCursorHooks,
      JSON.stringify({
        version: 1,
        hooks: { preToolUse: [{ command: 'custom-astro hooks remind --client=cursor' }] },
      }),
    );
    fs.writeFileSync(
      extraCursorHooks,
      JSON.stringify({
        version: 1,
        hooks: { sessionEnd: [{ command: 'custom-astro hooks cleanup' }] },
      }),
    );
    const curRes = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--remove-hooks',
        '--clients=cursor',
        '--home',
        home,
        '--command',
        'custom-astro',
        '--hooks-config',
        extraCursorHooks,
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(curRes.status, 0, curRes.stderr + curRes.stdout);
    assert.equal(fs.existsSync(defaultCursorHooks), false);
    assert.equal(fs.existsSync(extraCursorHooks), false);

    // Codebuddy, Grok, Kimi, OpenCode, DSH: default + extra cleaned together
    const cbDir = path.join(home, '.codebuddy');
    fs.mkdirSync(cbDir, { recursive: true });
    const defaultCb = path.join(cbDir, 'settings.json');
    const extraCb = path.join(home, 'extra-cb.json');
    const cbPayload = JSON.stringify({
      hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=codebuddy' }] }] },
    });
    fs.writeFileSync(defaultCb, cbPayload);
    fs.writeFileSync(extraCb, cbPayload);
    const cbRes = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--clients=codebuddy', '--home', home, '--hooks-config', extraCb],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(cbRes.status, 0, cbRes.stderr + cbRes.stdout);
    assert.equal(JSON.parse(fs.readFileSync(defaultCb, 'utf8')).hooks, undefined);
    assert.equal(JSON.parse(fs.readFileSync(extraCb, 'utf8')).hooks, undefined);

    const grokHooksDir = path.join(home, '.grok', 'hooks');
    fs.mkdirSync(grokHooksDir, { recursive: true });
    const defaultGrok = path.join(grokHooksDir, 'astrolabe.json');
    const extraGrok = path.join(home, 'extra-grok.json');
    const grokPayload = JSON.stringify({
      hooks: { SessionEnd: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }] },
    });
    fs.writeFileSync(defaultGrok, grokPayload);
    fs.writeFileSync(extraGrok, grokPayload);
    const grokRes = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--clients=grok', '--home', home, '--hooks-config', extraGrok],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(grokRes.status, 0, grokRes.stderr + grokRes.stdout);
    assert.equal(fs.existsSync(defaultGrok), false);
    assert.equal(fs.existsSync(extraGrok), false);

    const kimiDir = path.join(home, '.kimi-code');
    fs.mkdirSync(kimiDir, { recursive: true });
    const defaultKimi = path.join(kimiDir, 'config.toml');
    const extraKimi = path.join(home, 'extra-kimi.toml');
    const kimiPayload = `${kimicodeMod.MANAGED_BEGIN}\n[[hooks]]\nevent = "SessionEnd"\n${kimicodeMod.MANAGED_END}   `;
    fs.writeFileSync(defaultKimi, kimiPayload);
    fs.writeFileSync(extraKimi, kimiPayload);
    const kimiRes = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--clients=kimicode', '--home', home, '--hooks-config', extraKimi],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(kimiRes.status, 0, kimiRes.stderr + kimiRes.stdout);
    assert.equal(fs.readFileSync(defaultKimi, 'utf8'), '');
    assert.equal(fs.readFileSync(extraKimi, 'utf8'), '');

    const ocPluginsDir = path.join(home, '.config', 'opencode', 'plugins');
    fs.mkdirSync(ocPluginsDir, { recursive: true });
    const defaultOc = path.join(ocPluginsDir, 'astrolabe-guard.ts');
    const extraOc = path.join(home, 'extra-oc-guard.ts');
    fs.writeFileSync(defaultOc, 'export const AstrolabeGuard = async () => ({});\n');
    fs.writeFileSync(extraOc, 'export const AstrolabeGuard = async () => ({});\n');
    const ocRes = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--clients=opencode', '--home', home, '--hooks-config', extraOc],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(ocRes.status, 0, ocRes.stderr + ocRes.stdout);
    assert.equal(fs.existsSync(defaultOc), false);
    assert.equal(fs.existsSync(extraOc), false);

    const dshDir = path.join(home, '.dsh');
    fs.mkdirSync(dshDir, { recursive: true });
    const defaultDsh = path.join(dshDir, 'cordis.patch.yml');
    const extraDsh = path.join(home, 'extra-cordis.patch.yml');
    const dshPayload = `${dshMod.MANAGED_BEGIN}\n- insert: []\n${dshMod.MANAGED_END}   `;
    fs.writeFileSync(defaultDsh, dshPayload);
    fs.writeFileSync(extraDsh, dshPayload);
    const dshRes = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--clients=dsh', '--home', home, '--hooks-config', extraDsh],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(dshRes.status, 0, dshRes.stderr + dshRes.stdout);
    assert.equal(fs.readFileSync(defaultDsh, 'utf8'), '');
    assert.equal(fs.readFileSync(extraDsh, 'utf8'), '');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression: DSH detects and removes a dangling astrolabe-dsh-guard symlink (and symlinked profile dirs)', () => {
  const home = tmpHome();
  try {
    const hoistedModules = path.join(home, '.dsh', 'profiles', 'node_modules');
    fs.mkdirSync(hoistedModules, { recursive: true });
    const danglingHoisted = path.join(hoistedModules, 'astrolabe-dsh-guard');
    fs.symlinkSync(path.join(home, 'missing-plugin-target'), danglingHoisted);

    const realProfile = path.join(home, 'external-profile');
    fs.mkdirSync(path.join(realProfile, 'node_modules'), { recursive: true });
    const danglingProfilePlugin = path.join(realProfile, 'node_modules', 'astrolabe-dsh-guard');
    fs.symlinkSync(path.join(home, 'missing-profile-plugin'), danglingProfilePlugin);
    fs.symlinkSync(realProfile, path.join(home, '.dsh', 'profiles', 'work'));

    const client = clientsMod.getClient('deepseek-harness');
    const det = client.detect(home);
    assert.equal(det.detected, true);
    assert.equal(det.evidence.includes(danglingHoisted), true);
    assert.equal(
      det.evidence.includes(path.join(home, '.dsh', 'profiles', 'work', 'node_modules', 'astrolabe-dsh-guard')),
      true,
    );

    const applied = client.plan({ home }).apply();
    assert.equal(applied.skipped, undefined);
    applied.write();

    assert.throws(() => fs.lstatSync(danglingHoisted));
    assert.throws(() => fs.lstatSync(danglingProfilePlugin));
    assert.equal(client.detect(home).detected, false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression: ZCode preserves explicit { enabled: false } while deleting { enabled: true } empty shell', () => {
  const home = tmpHome();
  try {
    const launch = { command: 'npx', args: ['-y', '@zlnap/astrolabe', '.'] };
    const disabledFile = path.join(home, 'zcode-disabled.json');
    fs.writeFileSync(
      disabledFile,
      JSON.stringify({
        hooks: {
          enabled: false,
          events: {
            PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=zcode' }] }],
          },
        },
      }),
    );
    const disabledRes = zcodeMod.mergeZcodeConfigFile(disabledFile, launch);
    assert.equal(disabledRes.hooksRemoved, true);
    assert.deepEqual(disabledRes.doc.hooks, { enabled: false });

    const enabledFile = path.join(home, 'zcode-enabled.json');
    fs.writeFileSync(
      enabledFile,
      JSON.stringify({
        hooks: {
          enabled: true,
          events: {
            PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=zcode' }] }],
          },
        },
      }),
    );
    const enabledRes = zcodeMod.mergeZcodeConfigFile(enabledFile, launch);
    assert.equal(enabledRes.hooksRemoved, true);
    assert.equal(enabledRes.doc.hooks, undefined);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression: OpenCode skips third-party @opencode-ai/plugin mentioning hooks + opencode without astrolabe/AstrolabeGuard', () => {
  const home = tmpHome();
  try {
    const pluginFile = path.join(home, 'third-party-plugin.ts');
    fs.writeFileSync(
      pluginFile,
      [
        "import type { Plugin } from '@opencode-ai/plugin';",
        'export const CustomHooksPlugin: Plugin = async () => ({',
        '  // custom hooks for opencode workflow',
        "  'tool.execute.before': async () => {},",
        '});',
        '',
      ].join('\n'),
    );
    const skippedRes = opencodeMod.stripOpenCodeGuardPluginFile(pluginFile);
    assert.equal(skippedRes.skipped, true);

    const legacyNamedResFile = path.join(home, 'legacy-astrolabe-plugin.ts');
    fs.writeFileSync(
      legacyNamedResFile,
      [
        "import type { Plugin } from '@opencode-ai/plugin';",
        'export const LegacyPlugin: Plugin = async () => ({',
        '  // opencode hooks invoking astrolabe',
        "  'tool.execute.before': async () => {},",
        '});',
        '',
      ].join('\n'),
    );
    const removedRes = opencodeMod.stripOpenCodeGuardPluginFile(legacyNamedResFile);
    assert.equal(removedRes.deleted, true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Regression: interactive TTY run skips Apply confirmation when all actions are skipped and no hook_data exists', async () => {
  const home = tmpHome();
  const origInTTY = Object.getOwnPropertyDescriptor(process.stdin, 'isTTY');
  const origOutTTY = Object.getOwnPropertyDescriptor(process.stdout, 'isTTY');
  const origWrite = process.stdout.write;
  let captured = '';
  try {
    Object.defineProperty(process.stdin, 'isTTY', { value: true, configurable: true });
    Object.defineProperty(process.stdout, 'isTTY', { value: true, configurable: true });
    process.stdout.write = (chunk, ...rest) => {
      captured += String(chunk);
      if (typeof rest[rest.length - 1] === 'function') rest[rest.length - 1]();
      return true;
    };
    const code = await runMod.run(['--remove-hooks', '--clients=codex', '--home', home]);
    assert.equal(code, 0);
    assert.equal(captured.includes('Apply these changes?'), false);
    assert.match(captured, /Nothing to apply; no files changed\./);
  } finally {
    process.stdout.write = origWrite;
    if (origInTTY) Object.defineProperty(process.stdin, 'isTTY', origInTTY);
    else delete process.stdin.isTTY;
    if (origOutTTY) Object.defineProperty(process.stdout, 'isTTY', origOutTTY);
    else delete process.stdout.isTTY;
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('Full 8-client upgrade (setup --yes) strips all legacy hooks/plugins + hook_data, updates MCPs, preserves third-party hooks, and is idempotent', () => {
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const packagedBin = path.join(home, 'pkg', process.platform === 'win32' ? 'astrolabe.exe' : 'astrolabe');
    fs.mkdirSync(path.dirname(packagedBin), { recursive: true });
    fs.writeFileSync(packagedBin, '');

    // 1. Claude Code: .claude.json + settings.json (with mixed entry.hooks) + settings-ccm.json + settings-local.json
    const claudeDir = path.join(home, '.claude');
    fs.mkdirSync(claudeDir, { recursive: true });
    fs.writeFileSync(
      path.join(home, '.claude.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'keep', args: [] } } }, null, 2),
    );
    fs.writeFileSync(
      path.join(claudeDir, 'settings.json'),
      JSON.stringify({
        model: 'sonnet',
        hooks: {
          SessionStart: [
            { matcher: '', hooks: [{ type: 'command', command: 'astrolabe hooks session-start' }] },
          ],
          PreToolUse: [
            {
              matcher: '*',
              hooks: [
                { type: 'command', command: 'ai-con hook-proxy --agent claude' },
                { type: 'command', command: 'astrolabe hooks remind --client=claude-code' },
              ],
            },
          ],
          SessionEnd: [
            { matcher: '', hooks: [{ type: 'command', command: 'npx -y @zlnap/astrolabe hooks cleanup' }] },
          ],
        },
      }, null, 2),
    );
    fs.writeFileSync(
      path.join(claudeDir, 'settings-ccm.json'),
      JSON.stringify({
        hooks: {
          PreToolUse: [
            { matcher: 'Bash', hooks: [{ type: 'command', command: 'echo keep-ccm' }] },
            { matcher: '*', hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }] },
          ],
        },
      }, null, 2),
    );
    fs.writeFileSync(
      path.join(claudeDir, 'settings-local.json'),
      JSON.stringify({
        hooks: {
          PreToolUse: [
            { matcher: '*', hooks: [{ type: 'command', command: 'astrolabe hooks gc' }] },
          ],
        },
      }, null, 2),
    );

    // 2. Cursor: .cursor/mcp.json + .cursor/hooks.json
    const cursorDir = path.join(home, '.cursor');
    fs.mkdirSync(cursorDir, { recursive: true });
    fs.writeFileSync(
      path.join(cursorDir, 'mcp.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'keep' } } }, null, 2),
    );
    fs.writeFileSync(
      path.join(cursorDir, 'hooks.json'),
      JSON.stringify({
        version: 1,
        hooks: {
          preToolUse: [{ command: 'astrolabe hooks remind --client=cursor' }],
          sessionEnd: [{ command: 'astrolabe hooks cleanup' }],
        },
      }, null, 2),
    );

    // 3. Codebuddy: .codebuddy/.mcp.json + settings.json
    const cbDir = path.join(home, '.codebuddy');
    fs.mkdirSync(cbDir, { recursive: true });
    fs.writeFileSync(
      path.join(cbDir, 'settings.json'),
      JSON.stringify({
        hooks: {
          PreToolUse: [
            { hooks: [{ type: 'command', command: 'echo keep-cb' }] },
            { hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=codebuddy' }] },
          ],
        },
      }, null, 2),
    );

    // 4. Grok: .grok/config.toml + hooks/astrolabe.json + sibling hooks/ai-con.json
    const grokDir = path.join(home, '.grok');
    fs.mkdirSync(path.join(grokDir, 'hooks'), { recursive: true });
    fs.writeFileSync(path.join(grokDir, 'config.toml'), 'model = "grok-3"\n');
    fs.writeFileSync(
      path.join(grokDir, 'hooks', 'astrolabe.json'),
      JSON.stringify({
        hooks: {
          PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=grok' }] }],
        },
      }),
    );
    const aiConGrok = JSON.stringify({ hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'ai-con hook-proxy' }] }] } });
    fs.writeFileSync(path.join(grokDir, 'hooks', 'ai-con.json'), aiConGrok);

    // 5. Kimi Code: .kimi-code/mcp.json + config.toml
    const kimiDir = path.join(home, '.kimi-code');
    fs.mkdirSync(kimiDir, { recursive: true });
    fs.writeFileSync(
      path.join(kimiDir, 'mcp.json'),
      JSON.stringify({ mcpServers: { keep: { command: 'k' } } }),
    );
    fs.writeFileSync(
      path.join(kimiDir, 'config.toml'),
      [
        'default_model = "kimi"',
        '',
        '[[hooks]]',
        'event = "Notification"',
        'command = "echo keep-kimi"',
        '',
        kimicodeMod.MANAGED_BEGIN,
        '[[hooks]]',
        'event = "PreToolUse"',
        'command = "astrolabe hooks remind --client=kimicode"',
        kimicodeMod.MANAGED_END,
        '',
      ].join('\n'),
    );

    // 6. ZCode: .zcode/cli/config.json
    const zcodeDir = path.join(home, '.zcode', 'cli');
    fs.mkdirSync(zcodeDir, { recursive: true });
    fs.writeFileSync(
      path.join(zcodeDir, 'config.json'),
      JSON.stringify({
        hooks: {
          enabled: true,
          events: {
            PreToolUse: [
              {
                hooks: [
                  { type: 'command', command: 'astrolabe hooks remind --client=zcode' },
                  { type: 'command', command: 'echo keep-zcode' },
                ],
              },
            ],
            Stop: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }],
          },
        },
      }, null, 2),
    );

    // 7. OpenCode: .config/opencode/opencode.jsonc + plugins/astrolabe-guard.ts + plugins/other.ts
    const ocDir = path.join(home, '.config', 'opencode');
    const ocPluginsDir = path.join(ocDir, 'plugins');
    fs.mkdirSync(ocPluginsDir, { recursive: true });
    fs.writeFileSync(path.join(ocDir, 'opencode.jsonc'), '{\n  // oc-comment\n  "mcp": {}\n}\n');
    fs.writeFileSync(
      path.join(ocPluginsDir, 'astrolabe-guard.ts'),
      'export const AstrolabeGuard = async () => ({});\n',
    );
    fs.writeFileSync(path.join(ocPluginsDir, 'other.ts'), 'export const Other = async () => ({});\n');

    // 8. DSH: .dsh/cordis.patch.yml (orca-managed-dsh-hooks + orca-status + astrolabe-managed-dsh-guard) + node_modules/astrolabe-dsh-guard
    const dshDir = path.join(home, '.dsh');
    const dshGuardDir = path.join(dshDir, 'profiles', 'node_modules', 'astrolabe-dsh-guard');
    fs.mkdirSync(dshGuardDir, { recursive: true });
    fs.writeFileSync(path.join(dshGuardDir, 'package.json'), '{"name":"astrolabe-dsh-guard"}');
    fs.writeFileSync(
      path.join(dshDir, 'cordis.patch.yml'),
      [
        '# >>> orca-managed-dsh-hooks >>>',
        '- insert:',
        '    - id: orca-agent-hooks',
        '      command: orca-status',
        '# <<< orca-managed-dsh-hooks <<<',
        '',
        dshMod.MANAGED_BEGIN,
        '- insert:',
        '    - id: astrolabe-dsh-guard',
        dshMod.MANAGED_END,
        '',
      ].join('\n'),
    );

    // 9. ~/.astrolabe/hook_data
    const hookDataDir = path.join(home, '.astrolabe', 'hook_data');
    fs.mkdirSync(hookDataDir, { recursive: true });
    fs.writeFileSync(path.join(hookDataDir, 'session.json'), '{"count":3}');

    const env = {
      ...process.env,
      HOME: home,
      PATH: emptyPath,
      ASTROLABE_BINARY_PATH: packagedBin,
    };

    const run1 = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--home', home, '--no-cursor-permissions'],
      { encoding: 'utf8', env },
    );
    assert.equal(run1.status, 0, run1.stderr + run1.stdout);

    // Verify hook_data removed
    assert.equal(fs.existsSync(hookDataDir), false);

    // Verify Claude Code MCP + 3 settings files cleaned + mixed ai-con preserved
    const claudeJson = JSON.parse(fs.readFileSync(path.join(home, '.claude.json'), 'utf8'));
    assert.ok(claudeJson.mcpServers.astrolabe);
    assert.equal(claudeJson.mcpServers.keep.command, 'keep');
    const sMain = JSON.parse(fs.readFileSync(path.join(claudeDir, 'settings.json'), 'utf8'));
    assert.equal(sMain.model, 'sonnet');
    assert.equal(sMain.hooks.SessionStart, undefined);
    assert.equal(sMain.hooks.SessionEnd, undefined);
    assert.deepEqual(sMain.hooks.PreToolUse[0].hooks, [
      { type: 'command', command: 'ai-con hook-proxy --agent claude' },
    ]);
    const sCcm = JSON.parse(fs.readFileSync(path.join(claudeDir, 'settings-ccm.json'), 'utf8'));
    assert.equal(sCcm.hooks.PreToolUse.length, 1);
    assert.equal(sCcm.hooks.PreToolUse[0].hooks[0].command, 'echo keep-ccm');
    const sLocal = JSON.parse(fs.readFileSync(path.join(claudeDir, 'settings-local.json'), 'utf8'));
    assert.equal(sLocal.hooks, undefined);

    // Verify Cursor MCP updated & hooks.json removed
    const cursorMcp = JSON.parse(fs.readFileSync(path.join(cursorDir, 'mcp.json'), 'utf8'));
    assert.equal(cursorMcp.mcpServers.astrolabe.command, packagedBin);
    assert.equal(fs.existsSync(path.join(cursorDir, 'hooks.json')), false);

    // Verify Codebuddy
    const cbMcp = JSON.parse(fs.readFileSync(path.join(cbDir, '.mcp.json'), 'utf8'));
    assert.ok(cbMcp.mcpServers.astrolabe);
    const cbSettings = JSON.parse(fs.readFileSync(path.join(cbDir, 'settings.json'), 'utf8'));
    assert.equal(cbSettings.hooks.PreToolUse.length, 1);
    assert.equal(cbSettings.hooks.PreToolUse[0].hooks[0].command, 'echo keep-cb');

    // Verify Grok
    assert.match(fs.readFileSync(path.join(grokDir, 'config.toml'), 'utf8'), /\[mcp_servers\.astrolabe\]/);
    assert.equal(fs.existsSync(path.join(grokDir, 'hooks', 'astrolabe.json')), false);
    assert.equal(fs.readFileSync(path.join(grokDir, 'hooks', 'ai-con.json'), 'utf8'), aiConGrok);

    // Verify Kimi Code
    const kimiMcp = JSON.parse(fs.readFileSync(path.join(kimiDir, 'mcp.json'), 'utf8'));
    assert.ok(kimiMcp.mcpServers.astrolabe);
    const kimiToml = fs.readFileSync(path.join(kimiDir, 'config.toml'), 'utf8');
    assert.match(kimiToml, /default_model = "kimi"/);
    assert.match(kimiToml, /echo keep-kimi/);
    assert.doesNotMatch(kimiToml, /astrolabe-managed-kimi-hooks/);

    // Verify ZCode
    const zcodeDoc = JSON.parse(fs.readFileSync(path.join(zcodeDir, 'config.json'), 'utf8'));
    assert.ok(zcodeDoc.mcp.servers.astrolabe);
    assert.equal(zcodeDoc.hooks.events.Stop, undefined);
    assert.equal(zcodeDoc.hooks.events.PreToolUse[0].hooks.length, 1);
    assert.equal(zcodeDoc.hooks.events.PreToolUse[0].hooks[0].command, 'echo keep-zcode');

    // Verify OpenCode
    assert.match(fs.readFileSync(path.join(ocDir, 'opencode.jsonc'), 'utf8'), /"astrolabe"/);
    assert.equal(fs.existsSync(path.join(ocPluginsDir, 'astrolabe-guard.ts')), false);
    assert.equal(fs.existsSync(path.join(ocPluginsDir, 'other.ts')), true);

    // Verify DSH
    assert.equal(fs.existsSync(dshGuardDir), false);
    const dshPatch = fs.readFileSync(path.join(dshDir, 'cordis.patch.yml'), 'utf8');
    assert.match(dshPatch, /orca-managed-dsh-hooks/);
    assert.match(dshPatch, /orca-status/);
    assert.doesNotMatch(dshPatch, /astrolabe-managed-dsh-guard/);

    // Verify idempotency on second run: no additional backups for hook files, and --remove-hooks skips everything with 0 backups
    const snapshotHookBackups = () => ({
      claude: fs.readdirSync(claudeDir).filter((f) => f.includes('.bak-')).length,
      cursor: fs.readdirSync(cursorDir).filter((f) => f.startsWith('hooks.json.bak-')).length,
      cb: fs.readdirSync(cbDir).filter((f) => f.startsWith('settings.json.bak-')).length,
      kimi: fs.readdirSync(kimiDir).filter((f) => f.startsWith('config.toml.bak-')).length,
      dsh: fs.readdirSync(dshDir).filter((f) => f.startsWith('cordis.patch.yml.bak-')).length,
    });
    const backupsBefore = snapshotHookBackups();

    const run2 = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--home', home, '--no-cursor-permissions'],
      { encoding: 'utf8', env },
    );
    assert.equal(run2.status, 0, run2.stderr + run2.stdout);
    assert.deepEqual(snapshotHookBackups(), backupsBefore);
    assert.equal(fs.existsSync(hookDataDir), false);

    const rm2 = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--home', home],
      { encoding: 'utf8', env },
    );
    assert.equal(rm2.status, 0, rm2.stderr + rm2.stdout);
    assert.deepEqual(snapshotHookBackups(), backupsBefore);
    assert.match(rm2.stdout, /SKIP/);

    // Non-guard astrolabe-guard.ts in OpenCode must NOT be deleted
    const homeNonGuard = tmpHome();
    try {
      const ngPlugins = path.join(homeNonGuard, '.config', 'opencode', 'plugins');
      fs.mkdirSync(ngPlugins, { recursive: true });
      const nonGuardFile = path.join(ngPlugins, 'astrolabe-guard.ts');
      const nonGuardSrc = 'export const CustomHelper = async () => ({ ok: true });\n';
      fs.writeFileSync(nonGuardFile, nonGuardSrc);
      const ngRes = spawnSync(
        process.execPath,
        [BIN, 'setup', '--yes', '--clients=opencode', '--home', homeNonGuard],
        { encoding: 'utf8', env: { ...process.env, HOME: homeNonGuard, PATH: emptyPath } },
      );
      assert.equal(ngRes.status, 0, ngRes.stderr + ngRes.stdout);
      assert.equal(fs.existsSync(nonGuardFile), true);
      assert.equal(fs.readFileSync(nonGuardFile, 'utf8'), nonGuardSrc);
    } finally {
      fs.rmSync(homeNonGuard, { recursive: true, force: true });
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('isAstrolabeHookCommand edge cases (gc, npx, Windows .exe/.cmd/.bat, POSIX single-quote, custom --command, strict non-matching)', () => {
  const shouldMatch = [
    ['astrolabe hooks gc', undefined, undefined],
    ["'/abs/astrolabe' hooks gc", undefined, 'darwin'],
    ['npx -y @zlnap/astrolabe hooks remind', undefined, undefined],
    ['npx --yes @zlnap/astrolabe@0.17.12 hooks session-start', undefined, undefined],
    ['npx -y astrolabe hooks cleanup', undefined, undefined],
    ['astrolabe.exe hooks remind', undefined, 'win32'],
    ['"C:\\Tools(x86)\\A&B\\astrolabe.cmd" hooks cleanup', undefined, 'win32'],
    ['C:\\bin\\astrolabe.bat hooks gc', undefined, 'win32'],
    ['npx.cmd -y @zlnap/astrolabe hooks remind', undefined, 'win32'],
    ["'/Users/o'\\''neill/bin/astrolabe' hooks remind --client=claude-code", undefined, 'darwin'],
    ['/custom/my-tool hooks remind', '/custom/my-tool', 'darwin'],
    ['"/custom dir/my-tool" hooks cleanup', '/custom dir/my-tool', 'win32'],
  ];
  for (const [cmd, binPath, platform] of shouldMatch) {
    assert.equal(
      cursorMod.isAstrolabeHookCommand(cmd, { binPath, platform }),
      true,
      `cursorMod should match: ${cmd}`,
    );
    assert.equal(
      claudeCodeMod.isAstrolabeHookCommand(cmd, null, binPath, platform),
      true,
      `claudeCodeMod should match: ${cmd}`,
    );
  }

  const shouldNotMatch = [
    'ai-con hook-proxy --agent claude',
    'orca-status',
    'npx other-pkg hooks cleanup',
    'npx -y @other/other-pkg hooks cleanup',
    'echo astrolabe hooks remind',
    '/opt/other-astrolabe hooks remind',
    'astrolabe hooks unknown-subcommand',
  ];
  for (const cmd of shouldNotMatch) {
    assert.equal(
      cursorMod.isAstrolabeHookCommand(cmd, { binPath: '/custom/my-tool' }),
      false,
      `cursorMod must NOT match: ${cmd}`,
    );
    assert.equal(
      claudeCodeMod.isAstrolabeHookCommand(cmd, null, '/custom/my-tool'),
      false,
      `claudeCodeMod must NOT match: ${cmd}`,
    );
  }
});

test('--remove-hooks pure cleanup mode leaves MCP/permissions/rules untouched, handles hook_data-only, and supports --scope=project', () => {
  // 1. Pure cleanup mode across clients without astrolabe on PATH
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });

    // Existing MCP configs + Cursor permissions/rules should remain byte-for-byte untouched; missing MCP configs must NOT be created
    fs.mkdirSync(path.join(home, '.claude'), { recursive: true });
    const claudeJson = path.join(home, '.claude.json');
    const claudeJsonRaw = JSON.stringify({ mcpServers: { keep: { command: 'k' } } }, null, 2);
    fs.writeFileSync(claudeJson, claudeJsonRaw);
    fs.writeFileSync(
      path.join(home, '.claude', 'settings.json'),
      JSON.stringify({
        hooks: {
          PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }] }],
        },
      }),
    );

    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    const cursorMcp = path.join(home, '.cursor', 'mcp.json');
    const cursorMcpRaw = JSON.stringify({ mcpServers: { keep: { command: 'k' } } }, null, 2);
    fs.writeFileSync(cursorMcp, cursorMcpRaw);
    const cursorPerms = path.join(home, '.cursor', 'permissions.json');
    const cursorPermsRaw = JSON.stringify({ mcpAllowlist: ['keep:tool'] });
    fs.writeFileSync(cursorPerms, cursorPermsRaw);
    fs.writeFileSync(
      path.join(home, '.cursor', 'hooks.json'),
      JSON.stringify({
        version: 1,
        hooks: { preToolUse: [{ command: 'astrolabe hooks remind --client=cursor' }] },
      }),
    );

    fs.mkdirSync(path.join(home, '.codex'), { recursive: true });
    const codexToml = path.join(home, '.codex', 'config.toml');
    const codexTomlRaw = 'model = "o3"\n';
    fs.writeFileSync(codexToml, codexTomlRaw);

    const hookDataDir = path.join(home, '.astrolabe', 'hook_data');
    fs.mkdirSync(hookDataDir, { recursive: true });
    fs.writeFileSync(path.join(hookDataDir, 's.json'), '{}');

    const res = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--home', home],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(res.status, 0, res.stderr + res.stdout);
    assert.equal(fs.readFileSync(claudeJson, 'utf8'), claudeJsonRaw);
    assert.equal(fs.readFileSync(cursorMcp, 'utf8'), cursorMcpRaw);
    assert.equal(fs.readFileSync(cursorPerms, 'utf8'), cursorPermsRaw);
    assert.equal(fs.readFileSync(codexToml, 'utf8'), codexTomlRaw);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'hooks.json')), false);
    assert.equal(JSON.parse(fs.readFileSync(path.join(home, '.claude', 'settings.json'), 'utf8')).hooks, undefined);
    assert.equal(fs.existsSync(hookDataDir), false);
    assert.match(res.stdout, /MCP server configurations were left untouched/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }

  // 2. Works when NO clients are detected and only ~/.astrolabe/hook_data exists
  const homeOnlyData = tmpHome();
  try {
    const emptyPath = path.join(homeOnlyData, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const hookDataDir = path.join(homeOnlyData, '.astrolabe', 'hook_data');
    fs.mkdirSync(hookDataDir, { recursive: true });
    fs.writeFileSync(path.join(hookDataDir, 'state.json'), '{}');

    const res = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--home', homeOnlyData],
      { encoding: 'utf8', env: { ...process.env, HOME: homeOnlyData, PATH: emptyPath } },
    );
    assert.equal(res.status, 0, res.stderr + res.stdout);
    assert.equal(fs.existsSync(hookDataDir), false);
    assert.match(res.stdout, /Removed .*hook_data/);
  } finally {
    fs.rmSync(homeOnlyData, { recursive: true, force: true });
  }

  // 3. Project-scoped --remove-hooks --scope=project --project <dir>
  const homeProj = tmpHome();
  try {
    const emptyPath = path.join(homeProj, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const project = tmpProject(homeProj);

    // Seed project hooks/plugins
    fs.mkdirSync(path.join(project, '.cursor'), { recursive: true });
    fs.mkdirSync(path.join(project, '.cursor', 'rules'), { recursive: true });
    const projRules = path.join(project, '.cursor', 'rules', 'astrolabe.mdc');
    fs.writeFileSync(projRules, 'keep-rules');
    const projCursorHooks = path.join(project, '.cursor', 'hooks.json');
    fs.writeFileSync(
      projCursorHooks,
      JSON.stringify({
        version: 1,
        hooks: { preToolUse: [{ command: 'astrolabe hooks remind --client=cursor' }] },
      }),
    );

    fs.mkdirSync(path.join(project, '.grok', 'hooks'), { recursive: true });
    const projGrokHooks = path.join(project, '.grok', 'hooks', 'astrolabe.json');
    fs.writeFileSync(
      projGrokHooks,
      JSON.stringify({
        hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=grok' }] }] },
      }),
    );

    fs.mkdirSync(path.join(project, '.opencode', 'plugins'), { recursive: true });
    const projOcPlugin = path.join(project, '.opencode', 'plugins', 'astrolabe-guard.ts');
    fs.writeFileSync(projOcPlugin, 'export const AstrolabeGuard = async () => ({});\n');

    fs.mkdirSync(path.join(project, '.claude'), { recursive: true });
    const projClaudeSettings = path.join(project, '.claude', 'settings.json');
    const projClaudeLocal = path.join(project, '.claude', 'settings.local.json');
    const projClaudeRaw1 = JSON.stringify({
      hooks: { PreToolUse: [{ hooks: [{ type: 'command', command: 'astrolabe hooks remind --client=claude-code' }] }] },
    });
    const projClaudeRaw2 = JSON.stringify({
      hooks: { SessionEnd: [{ hooks: [{ type: 'command', command: 'astrolabe hooks cleanup' }] }] },
    });
    fs.writeFileSync(projClaudeSettings, projClaudeRaw1);
    fs.writeFileSync(projClaudeLocal, projClaudeRaw2);

    const res = spawnSync(
      process.execPath,
      [
        BIN,
        'setup',
        '--yes',
        '--remove-hooks',
        '--scope=project',
        '--clients=cursor,grok,opencode,claude-code',
        '--home',
        homeProj,
        '--project',
        project,
      ],
      { encoding: 'utf8', env: { ...process.env, HOME: homeProj, PATH: emptyPath } },
    );
    assert.equal(res.status, 0, res.stderr + res.stdout);

    assert.equal(fs.existsSync(projCursorHooks), false);
    assert.equal(fs.existsSync(projGrokHooks), false);
    assert.equal(fs.existsSync(projOcPlugin), false);
    assert.equal(fs.readFileSync(projRules, 'utf8'), 'keep-rules');

    // Claude project settings files are read-only checked and NOT modified
    assert.equal(fs.readFileSync(projClaudeSettings, 'utf8'), projClaudeRaw1);
    assert.equal(fs.readFileSync(projClaudeLocal, 'utf8'), projClaudeRaw2);
    assert.match(res.stdout, /Warning: .*settings\.json contains legacy Astrolabe hooks .*read-only check; not modified/);
    assert.match(res.stdout, /Warning: .*settings\.local\.json contains legacy Astrolabe hooks .*read-only check; not modified/);
  } finally {
    fs.rmSync(homeProj, { recursive: true, force: true });
  }
});

test('--dry-run (normal and --remove-hooks) previews changes without modifying files, removing hook_data, or creating backups', () => {
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const packagedBin = path.join(home, 'pkg', process.platform === 'win32' ? 'astrolabe.exe' : 'astrolabe');
    fs.mkdirSync(path.dirname(packagedBin), { recursive: true });
    fs.writeFileSync(packagedBin, '');

    fs.mkdirSync(path.join(home, '.cursor'), { recursive: true });
    const cursorHooks = path.join(home, '.cursor', 'hooks.json');
    const cursorHooksRaw = JSON.stringify({
      version: 1,
      hooks: { preToolUse: [{ command: 'astrolabe hooks remind --client=cursor' }] },
    });
    fs.writeFileSync(cursorHooks, cursorHooksRaw);

    fs.mkdirSync(path.join(home, '.claude'), { recursive: true });
    const claudeSettings = path.join(home, '.claude', 'settings.json');
    const claudeSettingsRaw = JSON.stringify({
      hooks: { SessionStart: [{ hooks: [{ type: 'command', command: 'astrolabe hooks session-start' }] }] },
    });
    fs.writeFileSync(claudeSettings, claudeSettingsRaw);

    const hookDataDir = path.join(home, '.astrolabe', 'hook_data');
    fs.mkdirSync(hookDataDir, { recursive: true });
    fs.writeFileSync(path.join(hookDataDir, 's.json'), '{}');

    const env = { ...process.env, HOME: home, PATH: emptyPath, ASTROLABE_BINARY_PATH: packagedBin };

    for (const extraArgs of [[], ['--remove-hooks']]) {
      const res = spawnSync(
        process.execPath,
        [BIN, 'setup', '--yes', '--dry-run', ...extraArgs, '--home', home, '--no-cursor-permissions'],
        { encoding: 'utf8', env },
      );
      assert.equal(res.status, 0, res.stderr + res.stdout);
      assert.match(res.stdout, /\[dry-run\] No files written\./);
      assert.match(res.stdout, /Hook data cache: remove/);
      assert.equal(fs.readFileSync(cursorHooks, 'utf8'), cursorHooksRaw);
      assert.equal(fs.readFileSync(claudeSettings, 'utf8'), claudeSettingsRaw);
      assert.equal(fs.existsSync(hookDataDir), true);
      assert.equal(fs.existsSync(path.join(home, '.cursor', 'mcp.json')), false);
      assert.equal(fs.existsSync(path.join(home, '.claude.json')), false);
      assert.equal(fs.readdirSync(path.join(home, '.cursor')).some((f) => f.includes('.bak-')), false);
      assert.equal(fs.readdirSync(path.join(home, '.claude')).some((f) => f.includes('.bak-')), false);
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('USAGE and Next steps contain no --with-hooks recommendations', () => {
  // USAGE only documents --with-hooks as a deprecated no-op, never recommending it
  const withHooksLine = argsMod.USAGE.split('\n').find((line) => line.includes('--with-hooks')) || '';
  assert.match(withHooksLine, /Deprecated no-op/i);
  assert.doesNotMatch(argsMod.USAGE, /re-run.*--with-hooks|enable.*--with-hooks|install.*--with-hooks/i);

  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    fs.mkdirSync(path.join(home, '.claude'), { recursive: true });
    fs.mkdirSync(path.join(home, '.codebuddy'), { recursive: true });
    fs.mkdirSync(path.join(home, '.grok'), { recursive: true });
    fs.mkdirSync(path.join(home, '.kimi-code'), { recursive: true });
    fs.mkdirSync(path.join(home, '.zcode', 'cli'), { recursive: true });
    fs.mkdirSync(path.join(home, '.config', 'opencode'), { recursive: true });

    const normalRes = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--home', home],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(normalRes.status, 0, normalRes.stderr + normalRes.stdout);
    const normalNext = normalRes.stdout.slice(normalRes.stdout.indexOf('Done. Next steps:'));
    assert.ok(normalNext.startsWith('Done. Next steps:'));
    assert.equal(normalNext.includes('--with-hooks'), false);

    const rmRes = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--home', home],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(rmRes.status, 0, rmRes.stderr + rmRes.stdout);
    const rmNext = rmRes.stdout.slice(rmRes.stdout.indexOf('Done. Next steps:'));
    assert.ok(rmNext.startsWith('Done. Next steps:'));
    assert.equal(rmNext.includes('--with-hooks'), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('isAstrolabeHookCommand recognizes leading env/KEY=VAL prefixes and npx @scope rules without matching third-party hooks', () => {
  // [M1] Legacy hook commands with leading env var assignments and optional env binary
  assert.equal(
    cursorMod.isAstrolabeHookCommand(
      'ASTROLABE_DENY_TEXT_SEARCH=true astrolabe hooks remind --client=claude-code',
    ),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand(
      "ASTROLABE_HOOK_CURSOR_MODE=enforce '/usr/local/bin/astrolabe' hooks remind --client=cursor",
    ),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand(
      'env ASTROLABE_HOOK_MODE=advisory astrolabe hooks remind --client=claude-code',
    ),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand(
      '/usr/bin/env FOO="a b" BAR=\'c d\' npx -y @zlnap/astrolabe@latest hooks remind',
    ),
    true,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand(
      "FOO=1 '/custom path/my-agent' hooks remind",
      { binPath: '/custom path/my-agent', platform: 'darwin' },
    ),
    true,
  );

  // [N3] Scoped npx packages: only @zlnap/astrolabe is matched
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx @other-org/astrolabe hooks remind'),
    false,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('npx -y @zlnap/astrolabe@latest hooks remind'),
    true,
  );

  // Third-party hooks with env prefixes must NEVER match
  assert.equal(
    cursorMod.isAstrolabeHookCommand('FOO=1 /usr/local/bin/ai-con hook-proxy'),
    false,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('FOO=1 /usr/local/bin/ai-con hooks remind'),
    false,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('env FOO=1 /opt/other-astrolabe hooks remind'),
    false,
  );
  assert.equal(
    cursorMod.isAstrolabeHookCommand('env FOO=1 hooks remind'),
    false,
  );

  // End-to-end stripping preserves third-party env-prefixed hooks
  const home = tmpHome();
  try {
    const claudeSettings = path.join(home, 'settings.json');
    fs.writeFileSync(
      claudeSettings,
      JSON.stringify(
        {
          hooks: {
            PreToolUse: [
              {
                matcher: 'Grep',
                hooks: [
                  {
                    type: 'command',
                    command:
                      'ASTROLABE_DENY_TEXT_SEARCH=true astrolabe hooks remind --client=claude-code',
                  },
                ],
              },
              {
                matcher: 'Bash',
                hooks: [
                  {
                    type: 'command',
                    command: 'env ASTROLABE_HOOK_MODE=advisory astrolabe hooks remind --client=claude-code',
                  },
                ],
              },
              {
                matcher: '*',
                hooks: [{ type: 'command', command: 'FOO=1 /usr/local/bin/ai-con hook-proxy' }],
              },
            ],
          },
        },
        null,
        2,
      ),
    );
    const claudeRes = claudeCodeMod.stripClaudeCodeHooksSettings(claudeSettings);
    assert.equal(claudeRes.removed, true);
    assert.equal(claudeRes.doc.hooks.PreToolUse.length, 1);
    assert.equal(
      claudeRes.doc.hooks.PreToolUse[0].hooks[0].command,
      'FOO=1 /usr/local/bin/ai-con hook-proxy',
    );

    const cursorHooks = path.join(home, 'hooks.json');
    fs.writeFileSync(
      cursorHooks,
      JSON.stringify(
        {
          version: 1,
          hooks: {
            preToolUse: [
              {
                command:
                  "ASTROLABE_HOOK_CURSOR_MODE=enforce '/usr/local/bin/astrolabe' hooks remind --client=cursor",
              },
              {
                command: 'FOO=1 /usr/local/bin/ai-con hook-proxy',
              },
            ],
          },
        },
        null,
        2,
      ),
    );
    const cursorRes = cursorMod.stripCursorHooksFile(cursorHooks);
    assert.equal(cursorRes.deleted, false);
    assert.equal(cursorRes.doc.hooks.preToolUse.length, 1);
    assert.equal(cursorRes.doc.hooks.preToolUse[0].command, 'FOO=1 /usr/local/bin/ai-con hook-proxy');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('mergeCodebuddyHooksSettings is removed and hook_data symlinks are unlinked safely', () => {
  assert.equal(codebuddyMod.mergeCodebuddyHooksSettings, undefined);
  assert.equal(clientsMod.mergeCodebuddyHooksSettings, undefined);

  if (process.platform === 'win32') return;
  const home = tmpHome();
  try {
    const emptyPath = path.join(home, 'empty-path');
    fs.mkdirSync(emptyPath, { recursive: true });
    const targetDir = path.join(home, 'real-target');
    fs.mkdirSync(targetDir, { recursive: true });
    const sentinel = path.join(targetDir, 'keep.txt');
    fs.writeFileSync(sentinel, 'do-not-delete');

    const astrolabeDir = path.join(home, '.astrolabe');
    fs.mkdirSync(astrolabeDir, { recursive: true });
    const hookDataLink = path.join(astrolabeDir, 'hook_data');
    fs.symlinkSync(targetDir, hookDataLink);

    const res = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--home', home],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(res.status, 0, res.stderr + res.stdout);
    assert.throws(() => fs.lstatSync(hookDataLink), /ENOENT/);
    assert.equal(fs.existsSync(targetDir), true);
    assert.equal(fs.readFileSync(sentinel, 'utf8'), 'do-not-delete');

    // Dangling symlink is also detected and unlinked cleanly
    fs.symlinkSync(path.join(home, 'does-not-exist'), hookDataLink);
    const resDangling = spawnSync(
      process.execPath,
      [BIN, 'setup', '--yes', '--remove-hooks', '--home', home],
      { encoding: 'utf8', env: { ...process.env, HOME: home, PATH: emptyPath } },
    );
    assert.equal(resDangling.status, 0, resDangling.stderr + resDangling.stdout);
    assert.throws(() => fs.lstatSync(hookDataLink), /ENOENT/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
