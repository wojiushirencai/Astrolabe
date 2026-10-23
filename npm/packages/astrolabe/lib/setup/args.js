'use strict';

const USAGE = `Configure Astrolabe as an MCP server for local AI coding agents.

Usage:
  astrolabe setup [options]

Options:
  --yes, -y              Configure all detected clients without prompts
  --clients=a,b,c        Configure only these client ids (comma-separated)
  --list                 Print detection results and exit
  --dry-run              Preview writes; do not modify files
  --native               Use command "astrolabe" on PATH instead of npx
  --command=<bin>        Override MCP command (default: npx, or astrolabe with --native)
  --with-hooks           Also install drift-guard hooks/plugins where supported
                         (OpenCode guard plugin; Codebuddy/Grok/Kimi/ZCode hook recipes;
                          DSH always installs its native guard)
  --no-cursor-permissions
                         Skip merging Cursor mcpAllowlist
  --cursor-permissions   Force Cursor allowlist merge when Cursor is selected (default on)
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
    native: false,
    help: false,
    home: null,
    clients: null,
    command: null,
    cursorPermissions: true,
    withHooks: false,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--yes' || arg === '-y') opts.yes = true;
    else if (arg === '--list') opts.list = true;
    else if (arg === '--dry-run') opts.dryRun = true;
    else if (arg === '--native') opts.native = true;
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else if (arg === '--with-hooks') opts.withHooks = true;
    else if (arg === '--no-cursor-permissions') opts.cursorPermissions = false;
    else if (arg === '--cursor-permissions') opts.cursorPermissions = true;
    else if (arg === '--home') {
      const value = argv[i + 1];
      if (!value || value.startsWith('-')) throw new Error('--home needs a directory');
      opts.home = value;
      i += 1;
    } else if (arg.startsWith('--home=')) {
      opts.home = arg.slice('--home='.length);
      if (!opts.home) throw new Error('--home needs a directory');
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
    } else {
      throw new Error(`unknown argument: ${arg}`);
    }
  }
  return opts;
}

module.exports = { parseArgs, USAGE };
