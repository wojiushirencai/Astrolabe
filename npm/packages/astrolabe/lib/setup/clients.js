'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const opencodeMod = require('./opencode');
const dshMod = require('./dsh');
const claudeCodeMod = require('./claude-code');
const codebuddyMod = require('./codebuddy');
const grokMod = require('./grok');
const kimicodeMod = require('./kimicode');
const zcodeMod = require('./zcode');
const cursorMod = require('./cursor');
const { expandHome, readEnvDir } = cursorMod;
const PACKAGE_NAME = '@zlnap/astrolabe';
const SERVER_NAME = 'astrolabe';

/** Keep in sync with crates/astrolabe-mcp/src/lib.rs CATALOG_TOOL_NAMES. */
const CATALOG_TOOL_NAMES = [
  'search_code',
  'find_symbol',
  'resolve_contx',
  'get_diag',
  'find_refs',
  'goto_def',
  'get_lang',
];

/**
 * Call-time aliases from crates/astrolabe-mcp/src/compat.rs TOOL_ALIASES.
 * Setup / allowlists advertise short catalog names only; these map legacy
 * allowlist entries onto short names. Server-side tools/call aliases stay.
 */
const LEGACY_TOOL_ALIASES = {
  resolve_context: 'resolve_contx',
  ast_resolve_contx: 'resolve_contx',
  ast_context: 'resolve_contx',
  get_diagnostics: 'get_diag',
  ast_get_diag: 'get_diag',
  ast_diag: 'get_diag',
  get_languages: 'get_lang',
  ast_get_lang: 'get_lang',
  ast_langs: 'get_lang',
  ast_lang: 'get_lang',
  find_references: 'find_refs',
  ast_find_ref: 'find_refs',
  ast_find_refs: 'find_refs',
  goto_definition: 'goto_def',
  ast_goto_def: 'goto_def',
  ast_def: 'goto_def',
  ast_search_code: 'search_code',
  ast_search: 'search_code',
  ast_find_symbol: 'find_symbol',
  ast_symbol: 'find_symbol',
};

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

/**
 * Rewrite Cursor IDE allowlist entries so Astrolabe tools use short catalog
 * names only. Legacy long / ast_* names are migrated in place; non-Astrolabe
 * entries are preserved. Then ensure every catalog short name is present.
 */
function migrateIdeAllowlist(existing, extras) {
  const seen = new Set();
  const list = [];
  const legacyNames = [];
  let migrated = 0;

  for (const item of existing) {
    if (typeof item !== 'string') {
      list.push(item);
      continue;
    }
    const colon = item.indexOf(':');
    if (colon <= 0) {
      if (!seen.has(item)) {
        seen.add(item);
        list.push(item);
      }
      continue;
    }
    const server = item.slice(0, colon);
    let tool = item.slice(colon + 1);
    if (server !== SERVER_NAME) {
      if (!seen.has(item)) {
        seen.add(item);
        list.push(item);
      }
      continue;
    }
    if (Object.prototype.hasOwnProperty.call(LEGACY_TOOL_ALIASES, tool)) {
      legacyNames.push(tool);
      tool = LEGACY_TOOL_ALIASES[tool];
      migrated += 1;
    }
    const next = `${SERVER_NAME}:${tool}`;
    if (seen.has(next)) continue;
    seen.add(next);
    list.push(next);
  }

  let added = 0;
  for (const item of extras) {
    if (seen.has(item)) continue;
    seen.add(item);
    list.push(item);
    added += 1;
  }
  return {
    list,
    added,
    migrated,
    legacyNames: [...new Set(legacyNames)],
  };
}

/**
 * Inspect an existing Cursor mcpAllowlist without writing.
 * status: missing | short-ok | needs-migrate | empty
 */
function checkCursorAllowlist(home, { env = process.env, configDir = null } = {}) {
  const permissionsPath = path.join(
    cursorMod.resolveCursorConfigDir(home, env, configDir),
    'permissions.json',
  );
  if (!exists(permissionsPath)) {
    return {
      filePath: permissionsPath,
      status: 'missing',
      legacyNames: [],
      shortPresent: 0,
      message: 'no permissions.json yet; setup will write short catalog names only',
    };
  }
  let doc;
  try {
    doc = JSON.parse(fs.readFileSync(permissionsPath, 'utf8').replace(/^\uFEFF/, ''));
  } catch (error) {
    return {
      filePath: permissionsPath,
      status: 'error',
      legacyNames: [],
      shortPresent: 0,
      message: `cannot parse permissions.json: ${error.message}`,
    };
  }
  const existing = Array.isArray(doc && doc.mcpAllowlist) ? doc.mcpAllowlist : [];
  const legacyNames = [];
  let shortPresent = 0;
  for (const item of existing) {
    if (typeof item !== 'string' || !item.startsWith(`${SERVER_NAME}:`)) continue;
    const tool = item.slice(SERVER_NAME.length + 1);
    if (Object.prototype.hasOwnProperty.call(LEGACY_TOOL_ALIASES, tool)) {
      legacyNames.push(tool);
    } else if (CATALOG_TOOL_NAMES.includes(tool)) {
      shortPresent += 1;
    }
  }
  const uniqueLegacy = [...new Set(legacyNames)];
  let status = 'short-ok';
  let message = `allowlist already uses short names (${shortPresent}/${CATALOG_TOOL_NAMES.length} catalog entries)`;
  if (uniqueLegacy.length > 0) {
    status = 'needs-migrate';
    message =
      `allowlist still lists legacy long names (${uniqueLegacy.join(', ')}); ` +
      'setup will rewrite them to short catalog names (server tools/call aliases stay)';
  } else if (shortPresent === 0 && existing.length === 0) {
    status = 'empty';
    message = 'mcpAllowlist is empty; setup will add short catalog names';
  } else if (shortPresent === 0) {
    status = 'empty';
    message = 'no Astrolabe short names in mcpAllowlist; setup will add them';
  }
  return { filePath: permissionsPath, status, legacyNames: uniqueLegacy, shortPresent, message };
}

function mergeCursorPermissions(
  home,
  { dryRun = false, yes = false, env = process.env, configDir = null } = {},
) {
  const permissionsPath = path.join(
    cursorMod.resolveCursorConfigDir(home, env, configDir),
    'permissions.json',
  );
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
  const { list, added, migrated, legacyNames } = migrateIdeAllowlist(existing, extras);
  doc.mcpAllowlist = list;
  return {
    filePath: permissionsPath,
    doc,
    created,
    added,
    migrated,
    legacyNames,
    total: list.length,
    dryRun,
    yes,
  };
}

function resolveHookBinPath({ binPath = null, launch = null, env = process.env } = {}) {
  if (binPath) return binPath;
  if (launch && launch.command && launch.command !== 'npx' && launch.command !== 'astrolabe') {
    return launch.command;
  }
  return which('astrolabe', env);
}

function normalizeComparablePath(filePath, platform = process.platform) {
  const abs = path.resolve(filePath);
  const pathImpl = platform === 'win32' ? path.win32 : path;
  let resolved = abs;
  try {
    resolved = fs.realpathSync(abs);
  } catch {
    let cur = abs;
    const tail = [];
    for (;;) {
      const parent = pathImpl.dirname(cur);
      if (parent === cur) {
        resolved = abs;
        break;
      }
      tail.unshift(pathImpl.basename(cur));
      cur = parent;
      try {
        const realParent = fs.realpathSync(cur);
        resolved = pathImpl.join(realParent, ...tail);
        break;
      } catch {
        // keep walking up to the nearest existing ancestor
      }
    }
  }
  return platform === 'win32' ? resolved.toLowerCase() : resolved;
}

function sameComparablePath(a, b, platform = process.platform) {
  if (!a || !b) return false;
  return normalizeComparablePath(a, platform) === normalizeComparablePath(b, platform);
}

function normalizeExplicitHooks(hooksConfigPaths, home) {
  if (!Array.isArray(hooksConfigPaths) || hooksConfigPaths.length === 0) return [];
  const seen = new Set();
  const out = [];
  for (const raw of hooksConfigPaths) {
    const expanded = expandHome(raw, home);
    if (!expanded) continue;
    const key = normalizeComparablePath(expanded);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(expanded);
  }
  return out;
}

function resolveHooksPaths(hooksConfigPaths, defaultPath, home) {
  const normalized = normalizeExplicitHooks(hooksConfigPaths, home);
  return normalized.length > 0 ? normalized : [defaultPath];
}

function assertNoHooksConfigOverride(clientId, hooksConfigPaths) {
  if (
    Array.isArray(hooksConfigPaths) &&
    hooksConfigPaths.some((p) => typeof p === 'string' && p.trim().length > 0)
  ) {
    throw new Error(
      `--hooks-config/--settings is not supported for MCP-only client "${clientId}"`,
    );
  }
}

function assertDistinctMcpAndHooksPaths(clientId, mcpFilePath, hooksPaths) {
  if (!mcpFilePath || !Array.isArray(hooksPaths)) return;
  for (const hp of hooksPaths) {
    if (hp && sameComparablePath(mcpFilePath, hp)) {
      throw new Error(
        `${clientId}: --mcp-config (${mcpFilePath}) and --hooks-config/--settings (${hp}) must not point to the same file`,
      );
    }
  }
}

function resolveCodexDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return readEnvDir(env, ['CODEX_HOME'], home) || path.join(home, '.codex');
}

function resolveWindsurfDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  const windsurfDir = readEnvDir(env, ['WINDSURF_CONFIG_DIR'], home);
  if (windsurfDir) return windsurfDir;
  const codeiumDir = readEnvDir(env, ['CODEIUM_CONFIG_DIR'], home);
  if (codeiumDir) {
    if (
      exists(path.join(codeiumDir, 'windsurf')) &&
      !exists(path.join(codeiumDir, 'mcp_config.json'))
    ) {
      return path.join(codeiumDir, 'windsurf');
    }
    return codeiumDir;
  }
  return path.join(home, '.codeium', 'windsurf');
}

function resolveContinueDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return readEnvDir(env, ['CONTINUE_GLOBAL_DIR'], home) || path.join(home, '.continue');
}

function vscodeExtensionSettingsPath(
  home,
  env = process.env,
  configDir = null,
  extId,
  fileName,
  platform = process.platform,
) {
  const explicit = expandHome(configDir, home);
  const envDir = !explicit ? readEnvDir(env, ['VSCODE_USER_DATA_DIR'], home) : null;
  const customRoot = explicit || envDir;
  if (customRoot) {
    if (exists(path.join(customRoot, 'globalStorage')) && !exists(path.join(customRoot, 'User'))) {
      return path.join(customRoot, 'globalStorage', extId, 'settings', fileName);
    }
    return path.join(customRoot, 'User', 'globalStorage', extId, 'settings', fileName);
  }
  if (platform === 'darwin') {
    return path.join(
      home,
      'Library',
      'Application Support',
      'Code',
      'User',
      'globalStorage',
      extId,
      'settings',
      fileName,
    );
  }
  if (platform === 'win32') {
    const base = (env && env.APPDATA) || path.join(home, 'AppData', 'Roaming');
    return path.join(base, 'Code', 'User', 'globalStorage', extId, 'settings', fileName);
  }
  const xdgConfig = readEnvDir(env, ['XDG_CONFIG_HOME'], home);
  const base = xdgConfig || path.join(home, '.config');
  return path.join(base, 'Code', 'User', 'globalStorage', extId, 'settings', fileName);
}

function clineSettingsPath(
  home,
  env = process.env,
  configDir = null,
  platform = process.platform,
) {
  return vscodeExtensionSettingsPath(
    home,
    env,
    configDir,
    'saoudrizwan.claude-dev',
    'cline_mcp_settings.json',
    platform,
  );
}

function rooSettingsPath(
  home,
  env = process.env,
  configDir = null,
  platform = process.platform,
) {
  return vscodeExtensionSettingsPath(
    home,
    env,
    configDir,
    'rooveterinaryinc.roo-cline',
    'mcp_settings.json',
    platform,
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
      const customDir = claudeCodeMod.resolveClaudeConfigDir(home, env);
      const mcpPath = claudeCodeMod.resolveClaudeMcpPath(home, env);
      const dirPath = customDir || path.join(home, '.claude');
      if (which('claude', env)) evidence.push('claude on PATH');
      if (exists(mcpPath)) evidence.push(customDir ? mcpPath : '~/.claude.json');
      if (exists(dirPath)) evidence.push(customDir ? dirPath : '~/.claude/');
      return { detected: evidence.length > 0, evidence };
    },
    plan({
      home,
      launch,
      withHooks = false,
      alwaysLoad = true,
      binPath = null,
      env = process.env,
      configDir = null,
      mcpConfigPath = null,
      hooksConfigPaths = null,
    }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const explicitHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const effectiveWithHooks = Boolean(withHooks || explicitHooks.length > 0);
      const filePath = explicitMcp || claudeCodeMod.resolveClaudeMcpPath(home, env, explicitConfigDir);
      const defaultSettingsPath = claudeCodeMod.resolveClaudeSettingsPath(home, env, explicitConfigDir);
      const settingsPaths = resolveHooksPaths(hooksConfigPaths, defaultSettingsPath, home);
      if (effectiveWithHooks) {
        assertDistinctMcpAndHooksPaths('claude-code', filePath, settingsPaths);
      }
      const entry = jsonServerEntry(launch, { typed: true });
      if (alwaysLoad !== false) {
        entry.alwaysLoad = true;
      }
      const hookBinPath = resolveHookBinPath({ binPath, launch, env });
      const steps = [];
      steps.push({
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const result = mergeClaudeJson(filePath, entry);
          if (result.skipped) return result;
          return { ...result, entry, write: () => writeJsonAtomic(filePath, result.doc) };
        },
      });
      if (effectiveWithHooks) {
        for (const settingsPath of settingsPaths) {
          steps.push({
            kind: 'claude-code-hooks',
            filePath: settingsPath,
            summary: `merge SessionStart/PreToolUse/SessionEnd into ${settingsPath}`,
            apply() {
              const result = claudeCodeMod.mergeClaudeCodeHooksSettings(settingsPath, {
                create: true,
                existsFn: exists,
                binPath: hookBinPath,
              });
              if (result.skipped) return result;
              return { ...result, write: () => writeJsonAtomic(settingsPath, result.doc) };
            },
          });
        }
      }
      return {
        kind: effectiveWithHooks ? 'claude-code' : 'json-mcp',
        filePath,
        entry,
        summary:
          steps[0].summary +
          (effectiveWithHooks ? `; hooks → ${settingsPaths.join(', ')}` : ''),
        steps,
        apply() {
          const results = [];
          for (const step of steps) {
            const applied = step.apply();
            if (!applied.skipped && !applied.write && applied.doc) {
              const fp = applied.filePath;
              applied.write = () => writeJsonAtomic(fp, applied.doc);
            }
            results.push(applied);
          }
          const primary = results.find((r) => !r.skipped) || results[0];
          return {
            ...primary,
            entry,
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
    id: 'cursor',
    label: 'Cursor',
    context: 'cursor',
    detect(home, env = process.env) {
      const evidence = [];
      const cursorDir = cursorMod.resolveCursorConfigDir(home, env);
      const hasCustom = Boolean(readEnvDir(env, ['CURSOR_CONFIG_DIR', 'CURSOR_HOME'], home));
      if (which('cursor', env)) evidence.push('cursor on PATH');
      if (exists(cursorDir)) evidence.push(hasCustom ? cursorDir : '~/.cursor/');
      if (process.platform === 'darwin' && exists('/Applications/Cursor.app')) {
        evidence.push('/Applications/Cursor.app');
      }
      return { detected: evidence.length > 0, evidence };
    },
    plan({
      home,
      launch,
      withHooks = false,
      projectRoot = null,
      cursorBinPath = null,
      env = process.env,
      pyrightPath = null,
      detected = true,
      scope = 'global',
      cursorCommandExplicit = false,
      configDir = null,
      mcpConfigPath = null,
      hooksConfigPaths = null,
    }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const explicitHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const effectiveWithHooks = Boolean(withHooks || explicitHooks.length > 0);
      const cursorDir = cursorMod.resolveCursorConfigDir(home, env, explicitConfigDir);
      const globalMcpPath = explicitMcp || path.join(cursorDir, 'mcp.json');
      const defaultHooksPath = cursorMod.resolveCursorHooksPath(home, env, explicitConfigDir);
      const hooksPaths = resolveHooksPaths(hooksConfigPaths, defaultHooksPath, home);
      const projectScope = scope === 'project';
      if (effectiveWithHooks) {
        const earlyMcpPath =
          explicitMcp ||
          (projectScope
            ? projectRoot
              ? path.join(cursorMod.canonicalizePath(projectRoot), '.cursor', 'mcp.json')
              : null
            : globalMcpPath);
        if (earlyMcpPath) {
          assertDistinctMcpAndHooksPaths('cursor', earlyMcpPath, hooksPaths);
        }
      }
      if (!detected) {
        return {
          kind: 'cursor',
          filePath: globalMcpPath,
          summary: 'Cursor not detected; skipping Cursor config, rules and approvals',
          apply() {
            return {
              skipped: true,
              reason: 'Cursor not detected (install Cursor or cursor-agent, then re-run setup)',
            };
          },
        };
      }
      const root = projectRoot
        ? cursorMod.canonicalizePath(projectRoot)
        : cursorMod.resolveProjectRoot(process.cwd());
      const projectMcpPath = explicitMcp || path.join(root, '.cursor', 'mcp.json');
      const primaryPath = projectScope ? projectMcpPath : globalMcpPath;
      if (effectiveWithHooks) {
        assertDistinctMcpAndHooksPaths('cursor', primaryPath, hooksPaths);
      }
      // Absolute native binary: a bare `astrolabe`/`npx` command depends on the
      // client's PATH and silently fails on hooks and on stdio spawn.
      const binPath =
        cursorBinPath || (cursorCommandExplicit ? null : which('astrolabe', env));
      if (!binPath) {
        return {
          kind: 'cursor',
          filePath: globalMcpPath,
          summary: 'Cursor skipped: could not resolve an absolute astrolabe binary',
          apply() {
            return {
              skipped: true,
              reason: 'could not resolve an absolute astrolabe binary (pass --command=/abs/path/astrolabe)',
            };
          },
        };
      }
      const rulesPath = path.join(root, '.cursor', 'rules', 'astrolabe.mdc');
      const projectEntry = cursorMod.cursorServerEntry({
        binPath,
        projectRoot: root,
        env,
        pyrightPath,
        includeCwd: true,
      });
      const globalEntry = cursorMod.cursorServerEntry({
        binPath,
        env,
        pyrightPath,
        includeCwd: false,
      });
      const steps = [];
      if (projectScope) {
        steps.push({
          kind: 'json-mcp',
          filePath: projectMcpPath,
          summary: `merge mcpServers.astrolabe into ${projectMcpPath} (absolute binary, cwd ${root}, env PATH/ASTROLABE_LSP_TIMEOUT)`,
          apply() {
            const result = mergeJsonMcpFile(projectMcpPath, projectEntry, { create: true });
            return {
              ...result,
              entry: projectEntry,
              write: () => writeJsonAtomic(projectMcpPath, result.doc),
            };
          },
        });
        steps.push({
          kind: 'cursor-rules',
          filePath: rulesPath,
          summary: `write ${rulesPath} (frontmatter alwaysApply: true; body from the shared MCP L1 source)`,
          apply() {
            const text = cursorMod.renderCursorRules({
              template: cursorMod.loadL1Template(),
              projectRoot: root,
            });
            const result = cursorMod.mergeRulesFile(rulesPath, text, { existsFn: exists });
            return { ...result, write: () => writeTextAtomic(rulesPath, result.text) };
          },
        });
      } else {
        steps.push({
          kind: 'json-mcp',
          filePath: globalMcpPath,
          summary: `merge mcpServers.astrolabe into ${globalMcpPath} (absolute binary, env PATH/ASTROLABE_LSP_TIMEOUT)`,
          apply() {
            const result = mergeJsonMcpFile(globalMcpPath, globalEntry, { create: true });
            return {
              ...result,
              entry: globalEntry,
              write: () => writeJsonAtomic(globalMcpPath, result.doc),
            };
          },
        });
      }
      if (effectiveWithHooks) {
        for (const hooksPath of hooksPaths) {
          steps.push({
            kind: 'cursor-hooks',
            filePath: hooksPath,
            summary: `merge preToolUse/sessionEnd into ${hooksPath} (--client=cursor, absolute hook command)`,
            apply() {
              const result = cursorMod.mergeCursorHooksFile(hooksPath, {
                create: true,
                existsFn: exists,
                binPath,
              });
              if (result.skipped) return result;
              return { ...result, write: () => writeJsonAtomic(hooksPath, result.doc) };
            },
          });
        }
      }
      const primaryEntry = projectScope ? projectEntry : globalEntry;
      return {
        kind: 'cursor',
        filePath: primaryPath,
        entry: primaryEntry,
        summary: projectScope
          ? `project ${projectMcpPath} (absolute binary, cwd=project root, env)` +
            `; rules ${rulesPath} (explicit --scope=project)` +
            (effectiveWithHooks
              ? `; hooks → ${hooksPaths.join(', ')} (--client=cursor, matcher incl. astrolabe-.*)`
              : '')
          : `global ${globalMcpPath} (absolute binary, env)` +
            (effectiveWithHooks
              ? `; hooks → ${hooksPaths.join(', ')} (--client=cursor, matcher incl. astrolabe-.*)`
              : ''),
        steps,
        apply() {
          const results = [];
          for (const step of steps) {
            results.push(step.apply());
          }
          const primary = results.find((r) => !r.skipped) || results[0];
          return {
            ...primary,
            entry: primaryEntry,
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
    id: 'codex',
    label: 'Codex',
    context: 'codex',
    detect(home, env = process.env) {
      const evidence = [];
      const dir = resolveCodexDir(home, env);
      const hasCustom = Boolean(readEnvDir(env, ['CODEX_HOME'], home));
      if (which('codex', env)) evidence.push('codex on PATH');
      if (exists(dir)) evidence.push(hasCustom ? dir : '~/.codex/');
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch, env = process.env, configDir = null, mcpConfigPath = null, hooksConfigPaths = null }) {
      assertNoHooksConfigOverride('codex', hooksConfigPaths);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const filePath = explicitMcp || path.join(resolveCodexDir(home, env, configDir), 'config.toml');
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
      const dir = resolveWindsurfDir(home, env);
      const hasCustom = Boolean(
        readEnvDir(env, ['WINDSURF_CONFIG_DIR', 'CODEIUM_CONFIG_DIR'], home),
      );
      if (which('windsurf', env)) evidence.push('windsurf on PATH');
      if (exists(dir)) evidence.push(hasCustom ? dir : '~/.codeium/windsurf/');
      if (process.platform === 'darwin' && exists('/Applications/Windsurf.app')) {
        evidence.push('/Applications/Windsurf.app');
      }
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch, env = process.env, configDir = null, mcpConfigPath = null, hooksConfigPaths = null }) {
      assertNoHooksConfigOverride('windsurf', hooksConfigPaths);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const filePath =
        explicitMcp || path.join(resolveWindsurfDir(home, env, configDir), 'mcp_config.json');
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
    detect(home, env = process.env) {
      const evidence = [];
      const settings = clineSettingsPath(home, env);
      if (exists(settings)) evidence.push(settings);
      else if (exists(path.dirname(settings))) evidence.push(path.dirname(settings));
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch, env = process.env, configDir = null, mcpConfigPath = null, hooksConfigPaths = null }) {
      assertNoHooksConfigOverride('cline', hooksConfigPaths);
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const filePath = explicitMcp || clineSettingsPath(home, env, explicitConfigDir);
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          // Only create when the extension settings dir already exists (or explicit override).
          const create =
            exists(path.dirname(filePath)) ||
            exists(filePath) ||
            Boolean(explicitConfigDir || explicitMcp);
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
    detect(home, env = process.env) {
      const evidence = [];
      const settings = rooSettingsPath(home, env);
      if (exists(settings)) evidence.push(settings);
      else if (exists(path.dirname(settings))) evidence.push(path.dirname(settings));
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch, env = process.env, configDir = null, mcpConfigPath = null, hooksConfigPaths = null }) {
      assertNoHooksConfigOverride('roo', hooksConfigPaths);
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const filePath = explicitMcp || rooSettingsPath(home, env, explicitConfigDir);
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const create =
            exists(path.dirname(filePath)) ||
            exists(filePath) ||
            Boolean(explicitConfigDir || explicitMcp);
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
    detect(home, env = process.env) {
      const evidence = [];
      const dir = resolveContinueDir(home, env);
      const hasCustom = Boolean(readEnvDir(env, ['CONTINUE_GLOBAL_DIR'], home));
      if (exists(dir)) evidence.push(hasCustom ? dir : '~/.continue/');
      return { detected: evidence.length > 0, evidence };
    },
    plan({ home, launch, env = process.env, configDir = null, mcpConfigPath = null, hooksConfigPaths = null }) {
      assertNoHooksConfigOverride('continue', hooksConfigPaths);
      // Continue's primary config is YAML; MCP servers for the IDE extension
      // commonly live in ~/.continue/mcpServers/astrolabe.yaml or mcp.json.
      // We write the documented JSON mcpServers shape to ~/.continue/mcp.json
      // (merge-only) which matches INSTALL_GUIDE examples.
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const dir = resolveContinueDir(home, env, explicitConfigDir);
      const filePath = explicitMcp || path.join(dir, 'mcp.json');
      const entry = jsonServerEntry(launch);
      return {
        kind: 'json-mcp',
        filePath,
        entry,
        summary: `merge mcpServers.astrolabe into ${filePath}`,
        apply() {
          const create = exists(dir) || exists(filePath) || Boolean(explicitConfigDir || explicitMcp);
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
    plan({
      home,
      launch,
      withHooks = false,
      binPath = null,
      env = process.env,
      configDir = null,
      mcpConfigPath = null,
      hooksConfigPaths = null,
    }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const explicitHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const effectiveWithHooks = Boolean(withHooks || explicitHooks.length > 0);
      const filePath = explicitMcp || opencodeMod.resolveOpenCodeConfigPath(home, exists, env, explicitConfigDir);
      const hookBinPath = resolveHookBinPath({ binPath, launch, env });
      const pluginPaths = resolveHooksPaths(
        hooksConfigPaths,
        opencodeMod.resolveOpenCodePluginPath(home, env, explicitConfigDir),
        home,
      );
      if (effectiveWithHooks) {
        assertDistinctMcpAndHooksPaths('opencode', filePath, pluginPaths);
      }
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
      if (effectiveWithHooks) {
        for (const pluginFilePath of pluginPaths) {
          steps.push(
            opencodeMod.planOpenCodeGuardPlugin(home, exists, {
              binPath: hookBinPath,
              pluginFilePath,
              env,
              configDir: explicitConfigDir,
            }),
          );
        }
      }
      // Primary plan surface remains the MCP file; hooks are optional extras applied by run.js
      return {
        kind: 'opencode',
        filePath,
        summary:
          mcpPlan.summary +
          (effectiveWithHooks ? `; optional guard plugin → ${pluginPaths.join(', ')}` : ''),
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
    plan({ home, env = process.env, configDir = null, mcpConfigPath = null, hooksConfigPaths = null }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      if (explicitMcp) {
        throw new Error(
          '--mcp-config is not supported for deepseek-harness (MCP servers are configured per-repo in the DSH UI)',
        );
      }
      const dshHome = dshMod.resolveDshHome(home, env, explicitConfigDir);
      const defaultPatchPath = dshMod.resolveDshPatchPath(home, env, explicitConfigDir);
      const patchPaths = resolveHooksPaths(hooksConfigPaths, defaultPatchPath, home);
      const patchPath = patchPaths[0];
      const source = dshMod.resolveDshPluginSource(exists);
      const installDirs = dshMod.resolveDshPluginInstallDirs(home, exists, env, explicitConfigDir);
      const explicitHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const hasExplicitOverride = Boolean(explicitConfigDir || explicitHooks.length > 0);
      return {
        kind: 'dsh-guard',
        filePath: patchPath,
        summary:
          `install astrolabe-dsh-guard into DSH profile node_modules and merge ${patchPaths.join(', ')}` +
          ' (MCP servers are per-repo in the DSH UI — setup does not write them)',
        apply() {
          if (!source) {
            return {
              skipped: true,
              reason:
                'dsh-plugin sources not found (expected vendor/astrolabe-dsh-guard next to setup or repo dsh-plugin/)',
            };
          }
          const patches = [];
          for (const pp of patchPaths) {
            const patch = dshMod.mergeDshCordisPatch(pp, {
              create: exists(dshHome) || exists(pp) || hasExplicitOverride,
              existsFn: exists,
            });
            if (patch.skipped && patchPaths.length === 1) return patch;
            patches.push(patch);
          }
          const primary = patches.find((p) => !p.skipped) || patches[0];
          const results = patches.map((p) => ({
            ...p,
            write: p.skipped ? null : () => writeTextAtomic(p.filePath, p.text),
          }));
          return {
            filePath: primary.filePath,
            created: primary.created,
            replaced: primary.replaced,
            text: primary.text,
            source,
            installDirs,
            results: patchPaths.length > 1 ? results : undefined,
            entryPreview: dshMod.MANAGED_BLOCK.trim(),
            write: () => {
              for (const dest of installDirs) {
                fs.rmSync(dest, { recursive: true, force: true });
                dshMod.copyDirRecursive(source, dest);
              }
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
    id: 'codebuddy',
    label: 'Codebuddy',
    aliases: [],
    context: null,
    detect(home, env = process.env) {
      return codebuddyMod.detectCodebuddy(home, env, { whichFn: which, existsFn: exists });
    },
    plan({
      home,
      launch,
      withHooks = false,
      binPath = null,
      env = process.env,
      configDir = null,
      mcpConfigPath = null,
      hooksConfigPaths = null,
    }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const explicitHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const effectiveWithHooks = Boolean(withHooks || explicitHooks.length > 0);
      const dir = codebuddyMod.resolveCodebuddyConfigDir(home, env, explicitConfigDir);
      const hasCustomDir = Boolean(
        explicitConfigDir ||
          readEnvDir(env, ['CODEBUDDY_CONFIG_DIR', 'CODEBUDDY_HOME'], home),
      );
      const mcpPath = explicitMcp || codebuddyMod.resolveCodebuddyMcpPath(home, exists, env, explicitConfigDir);
      const settingsPaths = resolveHooksPaths(
        hooksConfigPaths,
        codebuddyMod.resolveCodebuddySettingsPath(home, env, explicitConfigDir),
        home,
      );
      if (effectiveWithHooks) {
        assertDistinctMcpAndHooksPaths('codebuddy', mcpPath, settingsPaths);
      }
      const hookBinPath = resolveHookBinPath({ binPath, launch, env });
      const entry = jsonServerEntry(launch, { typed: true });
      const steps = [];
      steps.push({
        kind: 'json-mcp',
        filePath: mcpPath,
        summary: `merge mcpServers.astrolabe into ${mcpPath}`,
        apply() {
          const create =
            exists(dir) ||
            exists(mcpPath) ||
            (!hasCustomDir && exists(path.join(home, '.codebuddy.json'))) ||
            hasCustomDir ||
            Boolean(explicitMcp);
          const result = mergeJsonMcpFile(mcpPath, entry, { create });
          if (result.skipped) return result;
          return { ...result, write: () => writeJsonAtomic(mcpPath, result.doc) };
        },
      });
      if (effectiveWithHooks) {
        const hasExplicitHooks = Boolean(hasCustomDir || explicitHooks.length > 0);
        for (const settingsPath of settingsPaths) {
          steps.push({
            kind: 'codebuddy-hooks',
            filePath: settingsPath,
            summary: `merge PreToolUse/SessionEnd into ${settingsPath}`,
            apply() {
              const create = exists(dir) || exists(settingsPath) || hasExplicitHooks;
              const result = codebuddyMod.mergeCodebuddyHooksSettings(settingsPath, {
                create,
                existsFn: exists,
                binPath: hookBinPath,
              });
              if (result.skipped) return result;
              return { ...result, write: () => writeJsonAtomic(settingsPath, result.doc) };
            },
          });
        }
      }
      return {
        kind: 'codebuddy',
        filePath: mcpPath,
        summary:
          steps[0].summary +
          (effectiveWithHooks ? `; hooks → ${settingsPaths.join(', ')}` : ''),
        steps,
        apply() {
          const results = [];
          for (const step of steps) {
            const applied = step.apply();
            if (!applied.skipped && !applied.write && applied.doc) {
              const fp = applied.filePath;
              applied.write = () => writeJsonAtomic(fp, applied.doc);
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
    id: 'grok',
    label: 'Grok Build',
    aliases: ['grokbuild', 'grok-build'],
    context: null,
    detect(home, env = process.env) {
      return grokMod.detectGrok(home, env, { whichFn: which, existsFn: exists });
    },
    plan({
      home,
      launch,
      withHooks = false,
      binPath = null,
      env = process.env,
      configDir = null,
      mcpConfigPath = null,
      hooksConfigPaths = null,
    }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const explicitHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const effectiveWithHooks = Boolean(withHooks || explicitHooks.length > 0);
      const dir = grokMod.resolveGrokConfigDir(home, env, explicitConfigDir);
      const hasCustomDir = Boolean(
        explicitConfigDir ||
          readEnvDir(env, ['GROK_CONFIG_DIR', 'GROK_HOME'], home),
      );
      const tomlPath = explicitMcp || grokMod.resolveGrokConfigToml(home, env, explicitConfigDir);
      const hooksPaths = resolveHooksPaths(
        hooksConfigPaths,
        grokMod.resolveGrokHooksPath(home, env, explicitConfigDir),
        home,
      );
      if (effectiveWithHooks) {
        assertDistinctMcpAndHooksPaths('grok', tomlPath, hooksPaths);
      }
      const hookBinPath = resolveHookBinPath({ binPath, launch, env });
      const steps = [];
      steps.push({
        kind: 'grok-toml',
        filePath: tomlPath,
        summary: `merge [mcp_servers.astrolabe] into ${tomlPath}`,
        apply() {
          const create = exists(dir) || exists(tomlPath) || hasCustomDir || Boolean(explicitMcp);
          if (!create) {
            return { skipped: true, reason: `missing ${tomlPath} (will not create)` };
          }
          const result = mergeCodexToml(tomlPath, launch);
          return { ...result, write: () => writeTextAtomic(tomlPath, result.text) };
        },
      });
      if (effectiveWithHooks) {
        const hasExplicitHooks = Boolean(hasCustomDir || explicitHooks.length > 0);
        for (const hooksPath of hooksPaths) {
          steps.push({
            kind: 'grok-hooks',
            filePath: hooksPath,
            summary: `write ${hooksPath}`,
            apply() {
              const create = exists(dir) || exists(hooksPath) || hasExplicitHooks;
              const result = grokMod.mergeGrokHooksFile(hooksPath, {
                create,
                existsFn: exists,
                binPath: hookBinPath,
              });
              if (result.skipped) return result;
              return { ...result, write: () => writeJsonAtomic(hooksPath, result.doc) };
            },
          });
        }
      }
      return {
        kind: 'grok',
        filePath: tomlPath,
        summary:
          steps[0].summary +
          (effectiveWithHooks ? `; hooks → ${hooksPaths.join(', ')}` : ''),
        steps,
        apply() {
          const results = [];
          for (const step of steps) {
            const applied = step.apply();
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
    id: 'kimicode',
    label: 'Kimi Code',
    aliases: ['kimi-code', 'kimi_code', 'kimi'],
    context: null,
    detect(home, env = process.env) {
      return kimicodeMod.detectKimiCode(home, env, { whichFn: which, existsFn: exists });
    },
    plan({
      home,
      launch,
      withHooks = false,
      binPath = null,
      env = process.env,
      configDir = null,
      mcpConfigPath = null,
      hooksConfigPaths = null,
    }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const explicitHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const effectiveWithHooks = Boolean(withHooks || explicitHooks.length > 0);
      const dir = kimicodeMod.resolveKimiConfigDir(home, env, explicitConfigDir);
      const hasCustomDir = Boolean(
        explicitConfigDir ||
          readEnvDir(env, ['KIMI_CONFIG_DIR', 'KIMI_HOME'], home),
      );
      const mcpPath = explicitMcp || kimicodeMod.resolveKimiMcpPath(home, env, explicitConfigDir);
      const tomlPaths = resolveHooksPaths(
        hooksConfigPaths,
        kimicodeMod.resolveKimiConfigToml(home, env, explicitConfigDir),
        home,
      );
      if (effectiveWithHooks) {
        assertDistinctMcpAndHooksPaths('kimicode', mcpPath, tomlPaths);
      }
      const hookBinPath = resolveHookBinPath({ binPath, launch, env });
      const entry = jsonServerEntry(launch);
      const steps = [];
      steps.push({
        kind: 'json-mcp',
        filePath: mcpPath,
        summary: `merge mcpServers.astrolabe into ${mcpPath}`,
        apply() {
          const create = exists(dir) || exists(mcpPath) || hasCustomDir || Boolean(explicitMcp);
          const result = mergeJsonMcpFile(mcpPath, entry, { create });
          if (result.skipped) return result;
          return { ...result, write: () => writeJsonAtomic(mcpPath, result.doc) };
        },
      });
      if (effectiveWithHooks) {
        const hasExplicitHooks = Boolean(hasCustomDir || explicitHooks.length > 0);
        for (const tomlPath of tomlPaths) {
          steps.push({
            kind: 'kimi-hooks',
            filePath: tomlPath,
            summary: `merge [[hooks]] into ${tomlPath}`,
            apply() {
              const create = exists(dir) || exists(tomlPath) || hasExplicitHooks;
              const result = kimicodeMod.mergeKimiHooksToml(tomlPath, {
                create,
                existsFn: exists,
                binPath: hookBinPath,
              });
              if (result.skipped) return result;
              return { ...result, write: () => writeTextAtomic(tomlPath, result.text) };
            },
          });
        }
      }
      return {
        kind: 'kimicode',
        filePath: mcpPath,
        summary:
          steps[0].summary +
          (effectiveWithHooks ? `; hooks → ${tomlPaths.join(', ')}` : ''),
        steps,
        apply() {
          const results = [];
          for (const step of steps) {
            results.push(step.apply());
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
    id: 'zcode',
    label: 'ZCode',
    aliases: ['z-code', 'zai'],
    context: null,
    detect(home, env = process.env) {
      return zcodeMod.detectZCode(home, env, { whichFn: which, existsFn: exists });
    },
    plan({
      home,
      launch,
      withHooks = false,
      binPath = null,
      env = process.env,
      configDir = null,
      mcpConfigPath = null,
      hooksConfigPaths = null,
    }) {
      const explicitConfigDir = expandHome(configDir, home);
      const explicitMcp = expandHome(mcpConfigPath, home);
      const normalizedHooks = normalizeExplicitHooks(hooksConfigPaths, home);
      const explicitHooks = normalizedHooks.length > 0 ? normalizedHooks : null;
      const effectiveWithHooks = Boolean(withHooks || explicitHooks);
      const dir = zcodeMod.resolveZcodeConfigDir(home, env, explicitConfigDir);
      const hasCustomDir = Boolean(
        explicitConfigDir ||
          readEnvDir(env, ['ZCODE_CONFIG_DIR', 'ZCODE_HOME'], home),
      );
      const hookBinPath = resolveHookBinPath({ binPath, launch, env });

      const stepSpecs = [];
      if (!explicitMcp && !explicitHooks) {
        stepSpecs.push({
          filePath: zcodeMod.resolveZcodeConfigPath(home, exists, env, explicitConfigDir),
          writeMcp: true,
          withHooks: effectiveWithHooks,
        });
      } else if (explicitMcp && !explicitHooks) {
        stepSpecs.push({
          filePath: explicitMcp,
          writeMcp: true,
          withHooks: effectiveWithHooks,
        });
      } else if (!explicitMcp && explicitHooks) {
        for (const fp of explicitHooks) {
          stepSpecs.push({
            filePath: fp,
            writeMcp: true,
            withHooks: true,
          });
        }
      } else {
        const includesMcp = explicitHooks.some((fp) => sameComparablePath(fp, explicitMcp));
        stepSpecs.push({
          filePath: explicitMcp,
          writeMcp: true,
          withHooks: includesMcp,
        });
        for (const fp of explicitHooks) {
          if (sameComparablePath(fp, explicitMcp)) continue;
          stepSpecs.push({
            filePath: fp,
            writeMcp: false,
            withHooks: true,
          });
        }
      }

      const hasExplicitOverride = Boolean(hasCustomDir || explicitMcp || explicitHooks);
      const primaryPath = stepSpecs[0].filePath;
      const steps = stepSpecs.map((spec) => ({
        kind: 'zcode',
        filePath: spec.filePath,
        apply() {
          const create =
            exists(dir) ||
            exists(path.join(dir, 'cli')) ||
            exists(spec.filePath) ||
            hasExplicitOverride;
          const result = zcodeMod.mergeZcodeConfigFile(spec.filePath, launch, {
            create,
            withHooks: spec.withHooks,
            writeMcp: spec.writeMcp,
            existsFn: exists,
            binPath: hookBinPath,
          });
          if (result.skipped) return result;
          return { ...result, write: () => writeJsonAtomic(spec.filePath, result.doc) };
        },
      }));

      return {
        kind: 'zcode',
        filePath: primaryPath,
        summary:
          `merge mcp.servers.astrolabe into ${primaryPath}` +
          (effectiveWithHooks
            ? ` (+ hooks.events.PreToolUse${explicitHooks ? ` → ${explicitHooks.join(', ')}` : ''}; no SessionEnd; scrub Stop cleanup)`
            : ''),
        steps,
        apply() {
          if (steps.length === 1) {
            return steps[0].apply();
          }
          const results = steps.map((s) => s.apply());
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

];

function getClient(id) {
  return (
    CLIENTS.find(
      (c) => c && (c.id === id || (Array.isArray(c.aliases) && c.aliases.includes(id))),
    ) || null
  );
}

function canonicalClientId(id) {
  const c = getClient(id);
  return c ? c.id : id;
}

module.exports = {
  CLIENTS,
  CATALOG_TOOL_NAMES,
  LEGACY_TOOL_ALIASES,
  PACKAGE_NAME,
  SERVER_NAME,
  migrateIdeAllowlist,
  checkCursorAllowlist,
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
  clineSettingsPath,
  rooSettingsPath,
  normalizeComparablePath,
  sameComparablePath,
  // OpenCode / DSH helpers (tests)
  mergeOpenCodeMcpFile: opencodeMod.mergeOpenCodeMcpFile,
  detectOpenCodeMcpFormat: opencodeMod.detectOpenCodeMcpFormat,
  resolveOpenCodeConfigDir: opencodeMod.resolveOpenCodeConfigDir,
  resolveOpenCodePluginPath: opencodeMod.resolveOpenCodePluginPath,
  resolveOpenCodeConfigPath: opencodeMod.resolveOpenCodeConfigPath,
  mergeDshCordisPatch: dshMod.mergeDshCordisPatch,
  resolveDshHome: dshMod.resolveDshHome,
  resolveDshPatchPath: dshMod.resolveDshPatchPath,
  resolveDshPluginSource: dshMod.resolveDshPluginSource,
  resolveDshPluginInstallDirs: dshMod.resolveDshPluginInstallDirs,
  OPENCODE_GUARD_TS: opencodeMod.OPENCODE_GUARD_TS,
  renderOpenCodeGuardTs: opencodeMod.renderOpenCodeGuardTs,
  parseJsonc: require('./jsonc').parseJsonc,
  // Claude Code / Codebuddy / Grok / Kimi / ZCode / Cursor helpers (tests)
  resolveClaudeConfigDir: claudeCodeMod.resolveClaudeConfigDir,
  resolveClaudeMcpPath: claudeCodeMod.resolveClaudeMcpPath,
  resolveClaudeSettingsPath: claudeCodeMod.resolveClaudeSettingsPath,
  mergeClaudeCodeHooksSettings: claudeCodeMod.mergeClaudeCodeHooksSettings,
  CLAUDE_CODE_REMIND_MATCHER: claudeCodeMod.REMIND_MATCHER,
  resolveCodebuddyConfigDir: codebuddyMod.resolveCodebuddyConfigDir,
  resolveCodebuddyMcpPath: codebuddyMod.resolveCodebuddyMcpPath,
  resolveCodebuddySettingsPath: codebuddyMod.resolveCodebuddySettingsPath,
  mergeCodebuddyHooksSettings: codebuddyMod.mergeCodebuddyHooksSettings,
  resolveGrokConfigDir: grokMod.resolveGrokConfigDir,
  resolveGrokConfigToml: grokMod.resolveGrokConfigToml,
  resolveGrokHooksPath: grokMod.resolveGrokHooksPath,
  mergeGrokHooksFile: grokMod.mergeGrokHooksFile,
  resolveKimiConfigDir: kimicodeMod.resolveKimiConfigDir,
  resolveKimiMcpPath: kimicodeMod.resolveKimiMcpPath,
  resolveKimiConfigToml: kimicodeMod.resolveKimiConfigToml,
  mergeKimiHooksToml: kimicodeMod.mergeKimiHooksToml,
  resolveZcodeConfigDir: zcodeMod.resolveZcodeConfigDir,
  resolveZcodeConfigPath: zcodeMod.resolveZcodeConfigPath,
  mergeZcodeConfigFile: zcodeMod.mergeZcodeConfigFile,
  resolveCursorConfigDir: cursorMod.resolveCursorConfigDir,
  resolveCursorHooksPath: cursorMod.resolveCursorHooksPath,
  mergeCursorHooksFile: cursorMod.mergeCursorHooksFile,
  CURSOR_REMIND_MATCHER: cursorMod.REMIND_MATCHER,
  resolveCursorProjectRoot: cursorMod.resolveProjectRoot,
  canonicalizePath: cursorMod.canonicalizePath,
  cursorProjectSlug: cursorMod.projectSlug,
  resolveCursorApprovalPath: cursorMod.resolveApprovalPath,
  resolveCursorDisabledPath: cursorMod.resolveDisabledPath,
  detectPyright: cursorMod.detectPyright,
  cursorServerEntry: cursorMod.cursorServerEntry,
  buildCursorEnv: cursorMod.buildCursorEnv,
  renderCursorRules: cursorMod.renderCursorRules,
  loadCursorL1Template: cursorMod.loadL1Template,
  mergeCursorRulesFile: cursorMod.mergeRulesFile,
  checkCursorApprovalFile: cursorMod.checkApprovalFile,
  removeAstrolabeFromCursorDisabled: cursorMod.removeAstrolabeFromDisabled,
  enableCursorApproval: cursorMod.enableCursorApproval,
};
