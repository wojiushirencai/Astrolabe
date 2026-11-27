'use strict';

const USAGE = `Configure Astrolabe as an MCP server for local AI coding agents.

Usage:
  astrolabe setup [options]

Options:
  --yes, -y              Configure all detected clients without prompts
  --clients=a,b,c        Configure only these client ids (comma-separated)
  --list                 Print detection results and exit
  --check-config         Read-only Cursor effective config/approval check (no setup)
  --dry-run              Preview writes; do not modify files
  --native               Use command "astrolabe" on PATH instead of npx
  --command=<bin>        Override MCP command (default: npx, or astrolabe with --native)
  --config-dir <dir>     Override the target client's config root directory
                         (requires --clients=<single-id>; overrides env defaults)
  --mcp-config <file>    Override the target client's MCP config file path
                         (requires --clients=<single-id>)
  --hooks-config <file>  Override the target client's hooks/settings/plugin file path
  --settings <file>      Alias for --hooks-config; repeatable (e.g. multiple profiles)
                         (requires --clients=<single-id>)
  --with-hooks           Also install drift-guard hooks/plugins where supported
                         (OpenCode guard plugin; Claude Code/Cursor/Codebuddy/Grok/
                          Kimi/ZCode hook recipes; DSH always installs its native guard).
                         Claude Code writes ~/.claude/settings.json (--client=claude-code).
                         Cursor writes ~/.cursor/hooks.json (--client=cursor).
                         ZCode: PreToolUse only — never SessionEnd, never Stop cleanup
  --no-always-load       Omit alwaysLoad: true from Claude Code ~/.claude.json entry
  --always-load          Set alwaysLoad: true in Claude Code ~/.claude.json entry (default on)
  --no-cursor-permissions
                         Skip merging Cursor mcpAllowlist
  --cursor-permissions   Force Cursor allowlist merge when Cursor is selected (default on)
  --project <dir>        Project root for Cursor's best-effort approval and, with
                         --scope=project, for .cursor/mcp.json + rules
  --scope=global|project Cursor config scope (default: global ~/.cursor/mcp.json).
                         project writes <project>/.cursor/mcp.json (cwd pinned),
                         <project>/.cursor/rules/astrolabe.mdc and approves that directory
  --home <dir>           Override HOME (tests)
  -h, --help             Show this help

Client ids:
  claude-code, cursor, codex, windsurf, cline, roo, continue, opencode,
  deepseek-harness (alias: dsh), codebuddy, grok (aliases: grokbuild, grok-build),
  kimicode (aliases: kimi-code, kimi_code, kimi), zcode (aliases: z-code, zai)
`;

function parseArgs(argv) {
  const opts = {
    yes: false,
    list: false,
    dryRun: false,
    checkConfig: false,
    native: false,
    help: false,
    home: null,
    project: null,
    scope: 'global',
    clients: null,
    command: null,
    configDir: null,
    mcpConfig: null,
    hooksConfig: [],
    alwaysLoad: true,
    cursorPermissions: true,
    withHooks: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--yes' || arg === '-y') opts.yes = true;
    else if (arg === '--list') opts.list = true;
    else if (arg === '--check-config') opts.checkConfig = true;
    else if (arg === '--dry-run') opts.dryRun = true;
    else if (arg === '--native') opts.native = true;
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else if (arg === '--with-hooks') opts.withHooks = true;
    else if (arg === '--no-always-load') opts.alwaysLoad = false;
    else if (arg === '--always-load') opts.alwaysLoad = true;
    else if (arg === '--no-cursor-permissions') opts.cursorPermissions = false;
    else if (arg === '--cursor-permissions') opts.cursorPermissions = true;
    else if (arg === '--home') {
      const raw = argv[i + 1];
      const value = typeof raw === 'string' ? raw.trim() : '';
      if (!value || value.startsWith('-')) throw new Error('--home needs a directory');
      opts.home = value;
      i += 1;
    } else if (arg.startsWith('--home=')) {
      const value = arg.slice('--home='.length).trim();
      if (!value) throw new Error('--home needs a directory');
      opts.home = value;
    } else if (arg === '--project') {
      const raw = argv[i + 1];
      const value = typeof raw === 'string' ? raw.trim() : '';
      if (!value || value.startsWith('-')) throw new Error('--project needs a directory');
      opts.project = value;
      i += 1;
    } else if (arg.startsWith('--project=')) {
      const value = arg.slice('--project='.length).trim();
      if (!value) throw new Error('--project needs a directory');
      opts.project = value;
    } else if (arg === '--scope') {
      const value = argv[i + 1];
      if (value !== 'global' && value !== 'project') {
        throw new Error('--scope must be global or project');
      }
      opts.scope = value;
      i += 1;
    } else if (arg.startsWith('--scope=')) {
      const value = arg.slice('--scope='.length);
      if (value !== 'global' && value !== 'project') {
        throw new Error('--scope must be global or project');
      }
      opts.scope = value;
    } else if (arg.startsWith('--clients=')) {
      const raw = arg.slice('--clients='.length).trim();
      if (!raw) throw new Error('--clients needs a comma-separated list');
      opts.clients = raw.split(',').map((s) => s.trim()).filter(Boolean);
    } else if (arg === '--clients') {
      const value = argv[i + 1];
      if (!value || value.startsWith('-')) throw new Error('--clients needs a comma-separated list');
      opts.clients = value.split(',').map((s) => s.trim()).filter(Boolean);
      i += 1;
    } else if (arg.startsWith('--command=')) {
      opts.command = arg.slice('--command='.length);
      if (!opts.command) throw new Error('--command needs a value');
    } else if (arg === '--command') {
      const value = argv[i + 1];
      if (!value || value.startsWith('-')) throw new Error('--command needs a value');
      opts.command = value;
      i += 1;
    } else if (arg === '--config-dir') {
      const raw = argv[i + 1];
      const value = typeof raw === 'string' ? raw.trim() : '';
      if (!value || value.startsWith('-')) throw new Error('--config-dir needs a directory');
      opts.configDir = value;
      i += 1;
    } else if (arg.startsWith('--config-dir=')) {
      const value = arg.slice('--config-dir='.length).trim();
      if (!value) throw new Error('--config-dir needs a directory');
      opts.configDir = value;
    } else if (arg === '--mcp-config') {
      const raw = argv[i + 1];
      const value = typeof raw === 'string' ? raw.trim() : '';
      if (!value || value.startsWith('-')) throw new Error('--mcp-config needs a file path');
      opts.mcpConfig = value;
      i += 1;
    } else if (arg.startsWith('--mcp-config=')) {
      const value = arg.slice('--mcp-config='.length).trim();
      if (!value) throw new Error('--mcp-config needs a file path');
      opts.mcpConfig = value;
    } else if (arg === '--hooks-config' || arg === '--settings') {
      const raw = argv[i + 1];
      const value = typeof raw === 'string' ? raw.trim() : '';
      if (!value || value.startsWith('-')) throw new Error(`${arg} needs a file path`);
      opts.hooksConfig.push(value);
      i += 1;
    } else if (arg.startsWith('--hooks-config=')) {
      const value = arg.slice('--hooks-config='.length).trim();
      if (!value) throw new Error('--hooks-config needs a file path');
      opts.hooksConfig.push(value);
    } else if (arg.startsWith('--settings=')) {
      const value = arg.slice('--settings='.length).trim();
      if (!value) throw new Error('--settings needs a file path');
      opts.hooksConfig.push(value);
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return opts;
}

module.exports = { parseArgs, USAGE };
