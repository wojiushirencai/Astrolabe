'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const opencodeMod = require('./opencode');
const dshMod = require('./dsh');
const PACKAGE_NAME = '@zlnap/astrolabe';
const SERVER_NAME = 'astrolabe';

/** Keep in sync with crates/astrolabe-mcp/src/lib.rs CATALOG_TOOL_NAMES. */
const CATALOG_TOOL_NAMES = [
  'search_code',
  'find_symbol',
  'resolve_context',
  'get_diagnostics',
  'find_references',
  'goto_definition',
  'get_languages',
];

function which(cmd, env = process.env) {
  const pathVar = env.PATH || env.Path || '';
  const parts = pathVar.split(path.delimiter).filter(Boolean);
  const exts =
    process.platform === 'win32'
      ? (env.PATHEXT || '.EXE;.CMD;.BAT;.COM').split(';').filter(Boolean)
      : [''];
  for (const dir of parts) {
    for (const ext of exts) {
      const candidate = path.join(dir, cmd + (ext && !cmd.toLowerCase().endsWith(ext.toLowerCase()) ? ext : ''));
      try {
        if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) return candidate;
      } catch {
        // ignore
      }
    }
  }
  return null;
}

function exists(p) {
  try {
    return fs.existsSync(p);
  } catch {
    return false;
  }
}

function appSupport(...parts) {
  if (process.platform === 'darwin') {
    return path.join(os.homedir(), 'Library', 'Application Support', ...parts);
  }
  if (process.platform === 'win32') {
    const base = process.env.APPDATA || path.join(os.homedir(), 'AppData', 'Roaming');
    return path.join(base, ...parts);
  }
  return path.join(os.homedir(), '.config', ...parts);
}

function resolveLaunch({ context, native, commandOverride }) {
  if (commandOverride) {
    const args = ['.'];
    if (context) args.push(`--context=${context}`);
    return { command: commandOverride, args };
  }
  if (native) {
    const args = ['.'];
    if (context) args.push(`--context=${context}`);
    return { command: 'astrolabe', args };
  }
  const args = ['-y', PACKAGE_NAME, '.'];
  if (context) args.push(`--context=${context}`);
  return { command: 'npx', args };
}

function jsonServerEntry(launch, { typed = false } = {}) {
  const entry = { command: launch.command, args: launch.args };
  if (typed) entry.type = 'stdio';
  return entry;
}

function mergeJsonMcpFile(filePath, entry, { create = true } = {}) {
  let doc = {};
  let created = false;
  if (exists(filePath)) {
    const raw = fs.readFileSync(filePath, 'utf8').replace(/^\uFEFF/, '');
    doc = JSON.parse(raw);
    if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) {
      throw new Error(`${filePath} root must be a JSON object`);
    }
  } else if (!create) {
    return { skipped: true, reason: `missing ${filePath} (will not create)` };
  } else {
    created = true;
  }
  if (!doc.mcpServers || typeof doc.mcpServers !== 'object' || Array.isArray(doc.mcpServers)) {
    doc.mcpServers = {};
  }
  const had = Object.prototype.hasOwnProperty.call(doc.mcpServers, SERVER_NAME);
  doc.mcpServers[SERVER_NAME] = entry;
  return { doc, created, replaced: had, filePath };
}

function mergeClaudeJson(filePath, entry) {
  // ~/.claude.json is a large user settings file; only touch mcpServers.astrolabe.
  return mergeJsonMcpFile(filePath, entry, { create: true });
}

/**
 * Append or replace [mcp_servers.astrolabe] in Codex config.toml without a
 * TOML dependency. Preserves the rest of the file byte-for-byte aside from
 * the astrolabe table.
 */
function mergeCodexToml(filePath, launch) {
  const block = [
    `[mcp_servers.${SERVER_NAME}]`,
    `command = ${tomlString(launch.command)}`,
    `args = ${tomlStringArray(launch.args)}`,
    '',
  ].join('\n');

  let created = false;
  let text = '';
  if (exists(filePath)) {
    text = fs.readFileSync(filePath, 'utf8');
  } else {
    created = true;
    text = '';
  }

  const headerRe = /^\[mcp_servers\.astrolabe\]\s*$/m;
  const match = headerRe.exec(text);
  let next;
  let replaced = false;
  if (match) {
    replaced = true;
    const start = match.index;
    const after = text.slice(start + match[0].length);
    const nextHeader = after.search(/^\[/m);
    const end = nextHeader === -1 ? text.length : start + match[0].length + nextHeader;
    const before = text.slice(0, start).replace(/\s*$/, '\n\n');
    const rest = text.slice(end).replace(/^\s*/, '');
    next = `${before}${block}${rest ? `\n${rest}` : ''}`;
  } else {
    const trimmed = text.replace(/\s*$/, '');
    next = trimmed ? `${trimmed}\n\n${block}` : block;
  }
  return { text: next, created, replaced, filePath };
}

function tomlString(value) {
  return JSON.stringify(String(value));
}

function tomlStringArray(values) {
  return `[${values.map((v) => tomlString(v)).join(', ')}]`;
}

function backupFile(filePath) {
  if (!exists(filePath)) return null;
  const real = fs.realpathSync(filePath);
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const backup = `${real}.bak-${stamp}`;
  fs.copyFileSync(real, backup);
  return backup;
}

function writeTextAtomic(filePath, contents) {
  let dest = filePath;
  let mode = null;
  if (exists(filePath)) {
    dest = fs.realpathSync(filePath);
    mode = fs.statSync(dest).mode & 0o7777;
  }
  const dir = path.dirname(dest);
  fs.mkdirSync(dir, { recursive: true });
  const tmp = path.join(dir, `.${path.basename(dest)}.${process.pid}.${Date.now()}.tmp`);
  fs.writeFileSync(tmp, contents.endsWith('\n') ? contents : `${contents}\n`, {
    encoding: 'utf8',
    mode: mode ?? 0o600,
  });
  if (mode !== null) {
    try {
      fs.chmodSync(tmp, mode);
    } catch {
      // best effort
    }
  }
  fs.renameSync(tmp, dest);
}

function writeJsonAtomic(filePath, value) {
  writeTextAtomic(filePath, `${JSON.stringify(value, null, 2)}\n`);
}

function mergeCursorPermissions(home, { dryRun = false, yes = false } = {}) {
  const permissionsPath = path.join(home, '.cursor', 'permissions.json');
  const extras = CATALOG_TOOL_NAMES.map((name) => `${SERVER_NAME}:${name}`);
  let doc = {};
  let created = false;
  if (exists(permissionsPath)) {
    doc = JSON.parse(fs.readFileSync(permissionsPath, 'utf8').replace(/^\uFEFF/, ''));
    if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) {
      throw new Error(`${permissionsPath} root must be a JSON object`);
    }
  } else {
    created = true;
  }
  const existing = Array.isArray(doc.mcpAllowlist) ? doc.mcpAllowlist : [];
  const seen = new Set(existing.filter((x) => typeof x === 'string'));
  const list = existing.slice();
  let added = 0;
  for (const item of extras) {
    if (seen.has(item)) continue;
    seen.add(item);
    list.push(item);
    added += 1;
  }
  doc.mcpAllowlist = list;
  return {
    filePath: permissionsPath,
    doc,
    created,
    added,
    total: list.length,
    dryRun,
    yes,
  };
}

function clineSettingsPath(home) {
  if (process.platform === 'darwin') {
    return path.join(
      home,
      'Library',
      'Application Support',
      'Code',
      'User',
      'globalStorage',
      'saoudrizwan.claude-dev',
      'settings',
      'cline_mcp_settings.json',
    );
  }
  if (process.platform === 'win32') {
    const base = process.env.APPDATA || path.join(home, 'AppData', 'Roaming');
    return path.join(
      base,
      'Code',
      'User',
      'globalStorage',
      'saoudrizwan.claude-dev',
      'settings',
      'cline_mcp_settings.json',
    );
  }
  return path.join(
    home,
    '.config',
    'Code',
    'User',
    'globalStorage',
    'saoudrizwan.claude-dev',
    'settings',
    'cline_mcp_settings.json',
  );
}

function rooSettingsPath(home) {
  if (process.platform === 'darwin') {
    return path.join(
      home,
      'Library',
      'Application Support',
      'Code',
      'User',
      'globalStorage',
      'rooveterinaryinc.roo-cline',
      'settings',
      'mcp_settings.json',
    );
  }
  if (process.platform === 'win32') {
    const base = process.env.APPDATA || path.join(home, 'AppData', 'Roaming');
    return path.join(
      base,
      'Code',
      'User',
      'globalStorage',
      'rooveterinaryinc.roo-cline',
      'settings',
      'mcp_settings.json',
    );
  }
  return path.join(
    home,
    '.config',
    'Code',
    'User',
    'globalStorage',
    'rooveterinaryinc.roo-cline',
    'settings',
    'mcp_settings.json',
  );
}

/**
 * @typedef {object} ClientDef
 * @property {string} id
 * @property {string} label
 * @property {string|null} context
 * @property {(home: string, env?: NodeJS.ProcessEnv) => { detected: boolean, evidence: string[] }} detect
 * @property {(ctx: object) => object} plan
 */

/** @type {ClientDef[]} */
const CLIENTS = [
  {
    id: 'claude-code',
    label: 'Claude Code',
    context: 'claude-code',
    detect(home, env = process.env) {
      const evidence = [];
      if (which('claude', env)) evidence.push('claude on PATH');
      if (exists(path.join(home, '.claude.json'))) evidence.push('~/.claude.json');
      if (exists(path.join(home, '.claude'))) evidence.push('~/.claude/');
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch }) {
      const filePath = path.join(home, '.claude.json');
      const entry = jsonServerEntry(launch, { typed: true });
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const result = mergeClaudeJson(filePath, entry);
          if (result.skipped) return result;
          return { ...result, write: () => writeJsonAtomic(filePath, result.doc) };
        },
      };
    },
  },
  {
    id: 'cursor',
    label: 'Cursor',
    context: 'cursor',
    detect(home, env = process.env) {
      const evidence = [];
      if (which('cursor', env)) evidence.push('cursor on PATH');
      if (exists(path.join(home, '.cursor'))) evidence.push('~/.cursor/');
      if (process.platform === 'darwin' && exists('/Applications/Cursor.app')) {
        evidence.push('/Applications/Cursor.app');
      }
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch }) {
      const filePath = path.join(home, '.cursor', 'mcp.json');
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const result = mergeJsonMcpFile(filePath, entry, { create: true });
          return { ...result, write: () => writeJsonAtomic(filePath, result.doc) };
        },
      };
    },
  },
  {
    id: 'codex',
    label: 'Codex',
    context: 'codex',
    detect(home, env = process.env) {
      const evidence = [];
      if (which('codex', env)) evidence.push('codex on PATH');
      if (exists(path.join(home, '.codex'))) evidence.push('~/.codex/');
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch }) {
      const filePath = path.join(home, '.codex', 'config.toml');
      return {
        kind: 'codex-toml',
        filePath,
        summary: `merge [mcp_servers.astrolabe] into ${filePath}`,
        apply() {
          const result = mergeCodexToml(filePath, launch);
          return { ...result, write: () => writeTextAtomic(filePath, result.text) };
        },
      };
    },
  },
  {
    id: 'windsurf',
    label: 'Windsurf',
    context: null,
    detect(home, env = process.env) {
      const evidence = [];
      if (which('windsurf', env)) evidence.push('windsurf on PATH');
      if (exists(path.join(home, '.codeium', 'windsurf'))) evidence.push('~/.codeium/windsurf/');
      if (process.platform === 'darwin' && exists('/Applications/Windsurf.app')) {
        evidence.push('/Applications/Windsurf.app');
      }
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch }) {
      const filePath = path.join(home, '.codeium', 'windsurf', 'mcp_config.json');
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const result = mergeJsonMcpFile(filePath, entry, { create: true });
          return { ...result, write: () => writeJsonAtomic(filePath, result.doc) };
        },
      };
    },
  },
  {
    id: 'cline',
    label: 'Cline (VS Code)',
    context: null,
    detect(home) {
      const evidence = [];
      const settings = clineSettingsPath(home);
      if (exists(settings)) evidence.push(settings);
      else if (exists(path.dirname(settings))) evidence.push(path.dirname(settings));
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch }) {
      const filePath = clineSettingsPath(home);
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          // Only create when the extension settings dir already exists.
          const create = exists(path.dirname(filePath)) || exists(filePath);
          const result = mergeJsonMcpFile(filePath, entry, { create });
          if (result.skipped) return result;
          return { ...result, write: () => writeJsonAtomic(filePath, result.doc) };
        },
      };
    },
  },
  {
    id: 'roo',
    label: 'Roo Code (VS Code)',
    context: null,
    detect(home) {
      const evidence = [];
      const settings = rooSettingsPath(home);
      if (exists(settings)) evidence.push(settings);
      else if (exists(path.dirname(settings))) evidence.push(path.dirname(settings));
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch }) {
      const filePath = rooSettingsPath(home);
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const create = exists(path.dirname(filePath)) || exists(filePath);
          const result = mergeJsonMcpFile(filePath, entry, { create });
          if (result.skipped) return result;
          return { ...result, write: () => writeJsonAtomic(filePath, result.doc) };
        },
      };
    },
  },
  {
    id: 'continue',
    label: 'Continue',
    context: null,
    detect(home) {
      const evidence = [];
      if (exists(path.join(home, '.continue'))) evidence.push('~/.continue/');
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch }) {
      // Continue's primary config is YAML; MCP servers for the IDE extension
      // commonly live in ~/.continue/mcpServers/astrolabe.yaml or mcp.json.
      // We write the documented JSON mcpServers shape to ~/.continue/mcp.json
      // (merge-only) which matches INSTALL_GUIDE examples.
      const filePath = path.join(home, '.continue', 'mcp.json');
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const create = exists(path.join(home, '.continue')) || exists(filePath);
          const result = mergeJsonMcpFile(filePath, entry, { create });
          if (result.skipped) return result;
          return { ...result, write: () => writeJsonAtomic(filePath, result.doc) };
        },
      };
    },
  },
  {
    id: 'opencode',
    label: 'OpenCode',
    aliases: [],
    context: null,
    detect(home, env = process.env) {
      return opencodeMod.detectOpenCode(home, env, { whichFn: which, existsFn: exists });
    },
    plan({ home, launch, withHooks = false }) {
      const filePath = opencodeMod.resolveOpenCodeConfigPath(home, exists);
      const steps = [];
      const mcpPlan = {
        kind: 'opencode-mcp',
        filePath,
        summary: `merge mcp.astrolabe into ${filePath} (OpenCode V1/V2/mcpServers auto-detect)`,
        apply() {
          const result = opencodeMod.mergeOpenCodeMcpFile(filePath, launch, { create: true, existsFn: exists });
          if (result.skipped) return result;
          return {
            ...result,
            write: () => writeTextAtomic(filePath, result.text),
          };
        },
      };
      steps.push(mcpPlan);
      if (withHooks) {
        steps.push(opencodeMod.planOpenCodeGuardPlugin(home, exists));
      }
      // Primary plan surface remains the MCP file; hooks are optional extras applied by run.js
      return {
        kind: 'opencode',
        filePath,
        summary: mcpPlan.summary + (withHooks ? '; optional guard plugin' : ''),
        steps,
        apply() {
          const results = [];
          for (const step of steps) {
            const applied = step.apply();
            if (applied.skipped) {
              results.push(applied);
              continue;
            }
            if (!applied.write) {
              const fp = applied.filePath;
              const contents = applied.text;
              applied.write = () => writeTextAtomic(fp, contents);
            }
            results.push(applied);
          }
          const primary = results.find((r) => !r.skipped) || results[0];
          return {
            ...primary,
            results,
            write: () => {
              for (const r of results) {
                if (!r.skipped && r.write) r.write();
              }
            },
          };
        },
      };
    },
  },
  {
    id: 'deepseek-harness',
    label: 'DeepSeek Harness (DSH)',
    aliases: ['dsh'],
    context: null,
    detect(home, env = process.env) {
      return dshMod.detectDsh(home, env, { whichFn: which, existsFn: exists });
    },
    plan({ home }) {
      const patchPath = path.join(home, '.dsh', 'cordis.patch.yml');
      const source = dshMod.resolveDshPluginSource(exists);
      const installDirs = dshMod.resolveDshPluginInstallDirs(home, exists);
      return {
        kind: 'dsh-guard',
        filePath: patchPath,
        summary:
          `install astrolabe-dsh-guard into DSH profile node_modules and merge ${patchPath}` +
          ' (MCP servers are per-repo in the DSH UI — setup does not write them)',
        apply() {
          if (!source) {
            return {
              skipped: true,
              reason:
                'dsh-plugin sources not found (expected vendor/astrolabe-dsh-guard next to setup or repo dsh-plugin/)',
            };
          }
          const patch = dshMod.mergeDshCordisPatch(patchPath, {
            create: exists(path.join(home, '.dsh')) || exists(patchPath),
            existsFn: exists,
          });
          if (patch.skipped) return patch;
          return {
            filePath: patchPath,
            created: patch.created,
            replaced: patch.replaced,
            text: patch.text,
            source,
            installDirs,
            entryPreview: dshMod.MANAGED_BLOCK.trim(),
            write: () => {
              for (const dest of installDirs) {
                fs.rmSync(dest, { recursive: true, force: true });
                dshMod.copyDirRecursive(source, dest);
              }
              writeTextAtomic(patchPath, patch.text);
            },
          };
        },
      };
    },
  },
];

function getClient(id) {
  return (
    CLIENTS.find((c) => c.id === id || (Array.isArray(c.aliases) && c.aliases.includes(id))) || null
  );
}

function canonicalClientId(id) {
  const c = getClient(id);
  return c ? c.id : id;
}

module.exports = {
  CLIENTS,
  CATALOG_TOOL_NAMES,
  PACKAGE_NAME,
  SERVER_NAME,
  which,
  resolveLaunch,
  jsonServerEntry,
  mergeJsonMcpFile,
  mergeCodexToml,
  mergeCursorPermissions,
  backupFile,
  writeJsonAtomic,
  writeTextAtomic,
  getClient,
  canonicalClientId,
  appSupport,
  // OpenCode / DSH helpers (tests)
  mergeOpenCodeMcpFile: opencodeMod.mergeOpenCodeMcpFile,
  detectOpenCodeMcpFormat: opencodeMod.detectOpenCodeMcpFormat,
  resolveOpenCodeConfigPath: opencodeMod.resolveOpenCodeConfigPath,
  mergeDshCordisPatch: dshMod.mergeDshCordisPatch,
  resolveDshPluginSource: dshMod.resolveDshPluginSource,
  resolveDshPluginInstallDirs: dshMod.resolveDshPluginInstallDirs,
  OPENCODE_GUARD_TS: opencodeMod.OPENCODE_GUARD_TS,
  parseJsonc: require('./jsonc').parseJsonc,
};
