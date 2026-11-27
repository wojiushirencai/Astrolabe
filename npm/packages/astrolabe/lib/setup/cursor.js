'use strict';

const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');
const { createHash } = require('crypto');

const SERVER_NAME = 'astrolabe';
const DEFAULT_LSP_TIMEOUT = '300';
const L1_PLACEHOLDER = '{{INDEX_ROOT}}';

/**
 * Last-resort L1 text when the package was copied outside the monorepo and
 * `prepack` never materialized templates/cursor-l1.md. The canonical source
 * remains crates/astrolabe-mcp/contexts/cursor-l1.md; this keeps
 * `setup --scope=project` from crashing in a standalone checkout.
 */
const FALLBACK_L1_TEMPLATE = `CRITICAL: Prefer Astrolabe over Grep/Read for code. Tools: resolve_contx, search_code, find_symbol, find_refs, goto_def, get_diag, get_lang. All are read-only: batch independent calls in parallel in one turn.

Current index root index_root: {{INDEX_ROOT}}. The index_root in results is that path; if it is not the repo you are editing, do not Read those paths.

Cursor exposes these MCP tools as \`astrolabe-resolve_contx\`, \`astrolabe-search_code\`, \`astrolabe-find_symbol\`, \`astrolabe-find_refs\`, \`astrolabe-goto_def\`, \`astrolabe-get_diag\`, and \`astrolabe-get_lang\`. Prefer them over the built-in Grep/Read/Glob for code discovery, references, and navigation; use the built-ins for non-code files and when Astrolabe is unavailable.

When the last line says “Fully listed”, the listing is complete — answer from it; do not re-check with grep or re-query with other words. When the task is unclear, start with resolve_contx. After edits, if build/tests already pass, skip get_diag. Source files in other languages are not indexed — use Grep/Read.`;

/**
 * Cursor's hook matcher is a regex over the internal tool name. Cursor CLI
 * names MCP tools `astrolabe-find_symbol` (hyphen, server prefix); the
 * legacy `mcp__astrolabe__*` spelling is kept for older Cursor builds.
 */
const REMIND_MATCHER = 'Shell|Read|Grep|astrolabe-.*|mcp__astrolabe__.*';
const REMIND_ARGS = ['hooks', 'remind', '--client=cursor'];
const CLEANUP_ARGS = ['hooks', 'cleanup'];

function expandHome(value, home = os.homedir()) {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  if (!trimmed) return null;
  if (trimmed === '~') return path.resolve(home);
  if (trimmed.startsWith('~/') || trimmed.startsWith('~\\')) {
    const rel = trimmed.slice(2).replace(/^[/\\]+/, '');
    return rel ? path.resolve(home, rel) : path.resolve(home);
  }
  return path.resolve(trimmed);
}

function readEnvDir(env, names, home = os.homedir()) {
  if (!env || typeof env !== 'object') return null;
  for (const name of names) {
    const raw = env[name];
    if (typeof raw === 'string' && raw.trim()) {
      return expandHome(raw, home);
    }
  }
  return null;
}

function resolveCursorConfigDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return readEnvDir(env, ['CURSOR_CONFIG_DIR', 'CURSOR_HOME'], home) || path.join(home, '.cursor');
}

function resolveCursorHooksPath(home, env = process.env, configDir = null) {
  return path.join(resolveCursorConfigDir(home, env, configDir), 'hooks.json');
}

/** Quote a binary path so hook commands survive spaces and shell metacharacters. */
function quoteCommandPath(binPath, platform = process.platform) {
  if (!binPath) return 'astrolabe';
  if (platform === 'win32') {
    if (binPath.includes('"')) {
      throw new Error(`Invalid Windows binary path (contains double quote): ${binPath}`);
    }
    return /^[A-Za-z0-9_./:\\-]+$/.test(binPath) ? binPath : `"${binPath}"`;
  }
  if (/^[A-Za-z0-9_./:@%+,\-]+$/.test(binPath)) return binPath;
  return `'${binPath.replace(/'/g, `'\\''`)}'`;
}

function hookCommand(binPath, args, platform = process.platform) {
  return [quoteCommandPath(binPath, platform), ...args].join(' ');
}

function remindEntry(binPath, platform = process.platform) {
  return {
    command: hookCommand(binPath, REMIND_ARGS, platform),
    matcher: REMIND_MATCHER,
  };
}

function cleanupEntry(binPath, platform = process.platform) {
  return {
    command: hookCommand(binPath, CLEANUP_ARGS, platform),
  };
}

function entryCommand(entry) {
  if (!entry || typeof entry !== 'object') return '';
  if (typeof entry.command === 'string') return entry.command;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  for (const h of hooks) {
    if (h && typeof h.command === 'string') return h.command;
  }
  return '';
}

/**
 * Unquote a single shell executable token from a hook command. Supports
 * Windows double-quoted paths, unquoted paths (preserving backslashes), and
 * POSIX adjacent single-quoted / escaped-quote segments such as
 * `'/Users/o'\''neill/bin/astrolabe'`. Returns null if the token contains
 * unquoted whitespace, shell operators, or malformed quoting.
 */
function unquoteHookExecutable(executable) {
  if (typeof executable !== 'string' || !executable) return null;
  if (/^"([^"]*)"$/.test(executable)) {
    return executable.slice(1, -1);
  }
  if (/^[^\s"';&|<>`]+$/.test(executable)) {
    return executable;
  }
  const partRe = /'([^']*)'|\\(.)|([^\s'"\\;&|<>`]+)/gy;
  let out = '';
  let matched = 0;
  let match;
  while ((match = partRe.exec(executable)) !== null) {
    matched = partRe.lastIndex;
    if (match[1] !== undefined) out += match[1];
    else if (match[2] !== undefined) out += match[2];
    else out += match[3];
  }
  return matched === executable.length ? out : null;
}

function isAstrolabeHookCommand(
  command,
  {
    subcommands = ['session-start', 'remind', 'cleanup'],
    binPath = null,
    platform = process.platform,
  } = {},
) {
  if (typeof command !== 'string') return false;
  const match = command.match(/^\s*(.*?)\s+hooks\s+(session-start|remind|cleanup)(?:\s|$)/);
  if (!match) return false;
  const allowed = Array.isArray(subcommands) ? subcommands : [subcommands];
  if (!allowed.includes(match[2])) return false;
  const executable = match[1];
  // Explicit --command paths may have arbitrary names. Otherwise only claim
  // the actual Astrolabe executable, not another program with similar args.
  const unquoted = unquoteHookExecutable(executable);
  if (binPath && (executable === quoteCommandPath(binPath, platform) || unquoted === binPath)) {
    return true;
  }
  if (!unquoted) return false;
  return /^(?:.*[/\\])?astrolabe(?:\.(?:exe|cmd|bat))?$/i.test(unquoted);
}

function isAstrolabeHook(entry, subcommand, binPath, platform = process.platform) {
  // Never replace a legacy group containing unrelated hooks as a whole.
  if (entry && !entry.command && Array.isArray(entry.hooks) && entry.hooks.length !== 1) {
    return false;
  }
  return isAstrolabeHookCommand(entryCommand(entry), {
    subcommands: [subcommand],
    binPath,
    platform,
  });
}

/**
 * Merge ~/.cursor/hooks.json (Cursor CLI native format: camelCase events,
 * flat command entries). Enforces --client=cursor, an absolute binary path,
 * and the documented matcher that includes Cursor's `astrolabe-*` MCP names
 * so counter resets work.
 */
function mergeCursorHooksFile(
  filePath,
  { create = true, existsFn = fs.existsSync, binPath = null, platform = process.platform } = {},
) {
  let created = false;
  let doc = {};
  if (existsFn(filePath)) {
    const raw = fs.readFileSync(filePath, 'utf8').replace(/^﻿/, '');
    doc = JSON.parse(raw);
    if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) {
      throw new Error(`${filePath} root must be a JSON object`);
    }
  } else if (!create) {
    return { skipped: true, reason: `missing ${filePath} (will not create)` };
  } else {
    created = true;
  }

  if (doc.version == null) doc.version = 1;
  if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) {
    doc.hooks = {};
  }
  const hooks = doc.hooks;
  let replaced = false;

  // Collapse only standalone entries owned by Astrolabe. Mixed legacy groups
  // remain untouched, so unrelated hooks are never silently removed.
  function migrate(entries, subcommand, replacement) {
    const out = [];
    let inserted = false;
    for (const entry of Array.isArray(entries) ? entries : []) {
      if (!isAstrolabeHook(entry, subcommand, binPath, platform)) {
        out.push(entry);
      } else {
        replaced = true;
        if (!inserted) out.push(replacement);
        inserted = true;
      }
    }
    if (!inserted) out.push(replacement);
    return out;
  }
  const nextRemind = remindEntry(binPath, platform);
  const nextCleanup = cleanupEntry(binPath, platform);
  hooks.preToolUse = migrate(hooks.preToolUse, 'remind', nextRemind);
  hooks.sessionEnd = migrate(hooks.sessionEnd, 'cleanup', nextCleanup);

  return {
    filePath,
    doc,
    created,
    replaced,
    entryPreview: JSON.stringify(
      { version: doc.version, hooks: { preToolUse: [nextRemind], sessionEnd: [nextCleanup] } },
      null,
      2,
    ),
  };
}

function gitTopLevel(dir, spawnFn) {
  try {
    const result = spawnFn('git', ['-C', dir, 'rev-parse', '--show-toplevel'], {
      encoding: 'utf8',
      timeout: 5000,
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });
    if (result && result.status === 0) {
      const top = String(result.stdout || '').trim();
      if (top) return path.resolve(top);
    }
  } catch {
    // git is optional; fall back to the .git walk below.
  }
  return null;
}

/**
 * Cursor-agent resolves the project root with `git rev-parse
 * --show-toplevel`, then falls back to cwd. Project-level `.cursor/mcp.json`,
 * rules and approvals must use that same root or the approval hash points at
 * a different project.
 */
/**
 * macOS exposes `/tmp` and `/var` through symlinks. Cursor/agent and Node's
 * `process.cwd()` report the physical path, so approvals and project-level
 * config must use the same form or the slug (and approval hash) miss.
 */
function canonicalizePath(input) {
  const abs = path.resolve(input);
  try {
    return fs.realpathSync.native(abs);
  } catch {
    try {
      return fs.realpathSync(abs);
    } catch {
      return abs;
    }
  }
}

function resolveProjectRoot(startDir, { spawnFn = spawnSync } = {}) {
  const start = canonicalizePath(startDir || process.cwd());
  const top = gitTopLevel(start, spawnFn);
  if (top) return canonicalizePath(top);
  let dir = start;
  for (;;) {
    if (fs.existsSync(path.join(dir, '.git'))) return canonicalizePath(dir);
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return start;
}

/** Same slug Cursor uses under ~/.cursor/projects (non-alphanumerics -> -). */
function projectSlug(projectRoot) {
  return String(projectRoot)
    .replace(/[^a-zA-Z0-9]/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-+|-+$/g, '');
}

function resolveCursorProjectDir(home, projectRoot, { env = process.env, configDir = null } = {}) {
  return path.join(
    resolveCursorConfigDir(home, env, configDir),
    'projects',
    projectSlug(canonicalizePath(projectRoot)),
  );
}

function resolveApprovalPath(home, projectRoot, opts) {
  return path.join(resolveCursorProjectDir(home, projectRoot, opts), 'mcp-approvals.json');
}

function resolveDisabledPath(home, projectRoot, opts) {
  return path.join(resolveCursorProjectDir(home, projectRoot, opts), 'mcp-disabled.json');
}

function whichOnPath(cmd, env = process.env) {
  const pathVar = env.PATH || env.Path || '';
  const exts =
    process.platform === 'win32'
      ? (env.PATHEXT || '.EXE;.CMD;.BAT;.COM').split(';').filter(Boolean)
      : [''];
  for (const dir of pathVar.split(path.delimiter).filter(Boolean)) {
    for (const ext of exts) {
      const suffix = ext && !cmd.toLowerCase().endsWith(ext.toLowerCase()) ? ext : '';
      const candidate = path.join(dir, `${cmd}${suffix}`);
      try {
        if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) return candidate;
      } catch {
        // keep looking
      }
    }
  }
  return null;
}

function pyrightCandidateDirs(home, env, platform) {
  const dirs = [
    path.join(home, '.local', 'bin'),
    path.join(home, 'miniconda3', 'bin'),
    path.join(home, 'anaconda3', 'bin'),
    path.join(home, '.npm-global', 'bin'),
  ];
  if (platform === 'win32') {
    const appData = env.APPDATA || path.join(home, 'AppData', 'Roaming');
    dirs.push(path.join(appData, 'npm'));
  } else {
    dirs.push('/opt/homebrew/bin', '/usr/local/bin', '/usr/bin');
  }
  return dirs;
}

/**
 * Locate `pyright-langserver` on PATH, then in the usual Python/npm install
 * locations. Returns null when absent; setup then omits ASTROLABE_LSP_PYTHON
 * and the server falls back to its normal discovery.
 */
function detectPyright(
  env = process.env,
  { whichFn = whichOnPath, existsFn = fs.existsSync, home = os.homedir(), platform = process.platform } = {},
) {
  const preset = env.ASTROLABE_LSP_PYTHON;
  if (preset && existsFn(preset)) return preset;

  const onPath = whichFn('pyright-langserver', env);
  if (onPath) return onPath;

  const names =
    platform === 'win32'
      ? ['pyright-langserver.cmd', 'pyright-langserver.exe', 'pyright-langserver']
      : ['pyright-langserver'];
  for (const dir of pyrightCandidateDirs(home, env, platform)) {
    for (const name of names) {
      const candidate = path.join(dir, name);
      if (existsFn(candidate)) return candidate;
    }
  }
  return null;
}

function buildCursorEnv(env = process.env, { pyrightPath = null } = {}) {
  const out = {
    PATH: env.PATH || env.Path || '',
    ASTROLABE_LSP_TIMEOUT: DEFAULT_LSP_TIMEOUT,
  };
  if (pyrightPath) out.ASTROLABE_LSP_PYTHON = pyrightPath;
  return out;
}

function cursorServerEntry({
  binPath,
  projectRoot = null,
  env = process.env,
  pyrightPath = null,
  includeCwd = false,
} = {}) {
  const entry = {
    type: 'stdio',
    command: binPath,
    args: ['.', '--context=cursor'],
    env: buildCursorEnv(env, { pyrightPath }),
  };
  if (includeCwd && projectRoot) entry.cwd = projectRoot;
  return entry;
}

function resolveL1TemplatePath({ existsFn = fs.existsSync } = {}) {
  const candidates = [];
  if (process.env.ASTROLABE_CURSOR_L1) {
    candidates.push(path.resolve(process.env.ASTROLABE_CURSOR_L1));
  }
  // Packed npm tarball (prepack copies the crate source here).
  candidates.push(path.join(__dirname, 'templates', 'cursor-l1.md'));
  // Source tree / tests.
  candidates.push(
    path.resolve(__dirname, '../../../../../crates/astrolabe-mcp/contexts/cursor-l1.md'),
  );
  return candidates.find((candidate) => existsFn(candidate)) || null;
}

function loadL1Template({ existsFn = fs.existsSync, readFileFn = fs.readFileSync } = {}) {
  const templatePath = resolveL1TemplatePath({ existsFn });
  if (!templatePath) return FALLBACK_L1_TEMPLATE;
  return readFileFn(templatePath, 'utf8');
}

function renderCursorRules({ template, projectRoot }) {
  const root = canonicalizePath(projectRoot);
  const body = String(template).split(L1_PLACEHOLDER).join(root).trim();
  const frontmatter = [
    '---',
    'description: Astrolabe MCP code intelligence for Cursor. Prefer astrolabe-* tools over Grep/Read for code discovery, references, and navigation.',
    'alwaysApply: true',
    '---',
    '',
    '',
  ].join('\n');
  return `${frontmatter}${body}\n`;
}

function mergeRulesFile(filePath, text, { existsFn = fs.existsSync } = {}) {
  const existed = existsFn(filePath);
  let previous = null;
  if (existed) previous = fs.readFileSync(filePath, 'utf8').replace(/^﻿/, '');
  return {
    filePath,
    text,
    created: !existed,
    replaced: existed && previous !== text,
    unchanged: existed && previous === text,
  };
}

function readJsonArray(filePath) {
  try {
    const raw = fs.readFileSync(filePath, 'utf8').replace(/^﻿/, '');
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter((item) => typeof item === 'string') : null;
  } catch {
    return null;
  }
}

/** Match Cursor CLI's effective stdio object, not the raw registration.
 * Optional fields stay absent; type/envFile/enabledTools are stripped. cwd and
 * startupTimeoutSec participate in approvals. Keep field order in sync with
 * Cursor's command/args/env/cwd/startupTimeoutSec normalization.
 */
function normalizeCursorServer(server, env = process.env) {
  if (!server || typeof server.command !== 'string' || (server.type !== undefined && server.type !== 'stdio')) return null;
  const rawEnv = server.env;
  if (rawEnv !== undefined && (!rawEnv || typeof rawEnv !== 'object' || Array.isArray(rawEnv) ||
      Object.values(rawEnv).some((v) => typeof v !== 'string'))) return null;
  const expand = (text, lookup) => text.replace(
    /\$\{(?:env:([A-Za-z_][A-Za-z0-9_]*)|([^:}]+)(?::-([^}]*))?)\}/g,
    (original, explicit, name, fallback) => lookup(explicit || name) ?? (explicit ? original : fallback ?? original),
  );
  const effectiveEnv = Object.fromEntries(Object.entries(rawEnv || {}).map(([key, value]) => [
    key, expand(value, (name) => name === key ? env[name] : rawEnv[name] ?? env[name]),
  ]));
  const lookup = (name) => effectiveEnv[name] ?? env[name];
  const out = { command: expand(server.command, lookup) };
  if (server.args !== undefined) {
    if (!Array.isArray(server.args) || server.args.some((v) => typeof v !== 'string')) return null;
    out.args = server.args.map((v) => expand(v, lookup));
  }
  if (rawEnv !== undefined) out.env = effectiveEnv;
  if (server.cwd !== undefined) {
    if (typeof server.cwd !== 'string') return null;
    out.cwd = expand(server.cwd, lookup);
  }
  if (server.startupTimeoutSec !== undefined && Number.isInteger(server.startupTimeoutSec) &&
      server.startupTimeoutSec >= 0) out.startupTimeoutSec = server.startupTimeoutSec;
  return out;
}

function approvalKey(projectRoot, server, serverName = SERVER_NAME, env = process.env) {
  const normalized = normalizeCursorServer(server, env);
  if (!projectRoot || !normalized) return null;
  const hash = createHash('sha256')
    .update(JSON.stringify({ path: canonicalizePath(projectRoot), server: normalized }))
    .digest('hex').slice(0, 16);
  return `${serverName}-${hash}`;
}

function checkApprovalFile(filePath, { projectRoot, server, serverName = SERVER_NAME, env = process.env } = {}) {
  const entries = readJsonArray(filePath);
  const expectedKey = approvalKey(projectRoot, server, serverName, env);
  const approved = Boolean(expectedKey && entries && entries.includes(expectedKey));
  return { filePath, exists: entries !== null, approved, expectedKey, entries: entries || [] };
}

function effectiveCursorConfig(home, projectRoot, { env = process.env, configDir = null, mcpConfigPath = null } = {}) {
  const projectPath = path.join(projectRoot, '.cursor', 'mcp.json');
  const globalPath = path.join(resolveCursorConfigDir(home, env, configDir), 'mcp.json');
  const read = (filePath) => {
    try {
      const doc = JSON.parse(fs.readFileSync(filePath, 'utf8').replace(/^\uFEFF/, ''));
      if (!doc || typeof doc !== 'object' || Array.isArray(doc)) return { error: 'invalid JSON object' };
      const found = doc.mcpServers && Object.hasOwn(doc.mcpServers, SERVER_NAME);
      return { found: Boolean(found), server: found ? doc.mcpServers[SERVER_NAME] : null };
    } catch (error) {
      return error.code === 'ENOENT' ? { found: false } : { error: 'unreadable or invalid JSON' };
    }
  };
  // Invalid project configuration must not turn a global approval into a successful check.
  if (mcpConfigPath) {
    const overridePath = expandHome(mcpConfigPath, home);
    const override = read(overridePath);
    if (override.error) {
      return { source: 'mcp-config', filePath: overridePath, error: override.error, server: null };
    }
    if (override.found) {
      const project = read(projectPath);
      const global = read(globalPath);
      return {
        source: 'mcp-config',
        filePath: overridePath,
        server: override.server,
        shadowsGlobal: Boolean(project.found || global.found),
      };
    }
  }
  const project = read(projectPath);
  const global = read(globalPath);
  if (project.error) return { source: 'project', filePath: projectPath, error: project.error, server: null };
  if (project.found) return {
    source: 'project', filePath: projectPath, server: project.server,
    shadowsGlobal: Boolean(global.found),
  };
  return {
    source: global.found ? 'global' : null,
    filePath: globalPath,
    server: global.server || null,
    ...(global.error ? { error: global.error } : {}),
  };
}

function currentServer(
  home,
  projectRoot,
  { env = process.env, configDir = null, mcpConfigPath = null } = {},
) {
  return effectiveCursorConfig(home, projectRoot, { env, configDir, mcpConfigPath }).server;
}

/** Read only: no binary resolution/download, subprocesses or approval writes.
 * Never return the server env (it can contain credentials).
 */
function inspectCursorConfig(home, projectRoot, env = process.env, { configDir = null, mcpConfigPath = null } = {}) {
  const root = canonicalizePath(projectRoot);
  const effective = effectiveCursorConfig(home, root, { env, configDir, mcpConfigPath });
  const normalized = normalizeCursorServer(effective.server, env);
  const approval = checkApprovalFile(resolveApprovalPath(home, root), {
    projectRoot: root, server: effective.server, env,
  });
  const disabled = readJsonArray(resolveDisabledPath(home, root));
  const disabledMatch = Boolean(disabled && (disabled.includes(SERVER_NAME) ||
    (approval.expectedKey && disabled.includes(approval.expectedKey))));
  return {
    projectRoot: root, source: effective.source, filePath: effective.filePath,
    shadowsGlobal: Boolean(effective.shadowsGlobal), valid: Boolean(normalized && !effective.error),
    command: normalized ? normalized.command : null, args: normalized ? normalized.args || [] : [],
    approval: approval.approved ? 'current-config-approved' : 'current-config-not-approved',
    disabled: disabledMatch,
    ...(effective.error ? { error: effective.error } : {}),
    note: 'Reads standard project/global MCP files only; managed/enterprise and session overrides are not inspected. Approval state is reported, never changed.',
  };
}

function removeAstrolabeFromDisabled(
  filePath,
  { existsFn = fs.existsSync, serverName = SERVER_NAME } = {},
) {
  if (!existsFn(filePath)) {
    return { filePath, skipped: true, changed: false, doc: null };
  }
  const raw = fs.readFileSync(filePath, 'utf8').replace(/^﻿/, '');
  const parsed = JSON.parse(raw);
  if (!Array.isArray(parsed)) {
    throw new Error(`${filePath} must contain a JSON array`);
  }
  const doc = parsed.filter((item) =>
    !(typeof item === 'string' && (item === serverName ||
      (item.startsWith(`${serverName}-`) && /^[a-f0-9]+$/i.test(item.slice(serverName.length + 1))))),
  );
  return { filePath, skipped: false, changed: doc.length !== parsed.length, doc };
}

function approvalEnv(env, home, configDir = null) {
  const out = { ...env };
  // cursor-agent reads approvals relative to the user's home; mirror --home
  // so tests (and portable installs) write into the same tree setup targets.
  if (home) {
    out.HOME = home;
    if (process.platform === 'win32') out.USERPROFILE = home;
  }
  const hasCustomConfigDir = Boolean(
    expandHome(configDir, home) || readEnvDir(env, ['CURSOR_CONFIG_DIR', 'CURSOR_HOME'], home),
  );
  if (hasCustomConfigDir) {
    out.CURSOR_CONFIG_DIR = resolveCursorConfigDir(home, env, configDir);
  }
  return out;
}

function quoteForWindowsCmd(value) {
  return `"${String(value).replace(/"/g, '""')}"`;
}

/**
 * Node >= 18.20/20.12 refuses to spawn `.cmd`/`.bat` without a shell
 * (CVE-2024-27980). cursor-agent installs as `cursor-agent.cmd` on Windows,
 * so route that case through cmd.exe with a quoted command line; everything
 * else spawns directly. The timeout prevents a first-run self-update from
 * hanging setup forever.
 */
function spawnCursorAgent(spawnFn, agent, args, { cwd, env }, platform = process.platform) {
  const options = {
    cwd,
    env,
    encoding: 'utf8',
    timeout: 15000,
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  };
  if (platform === 'win32' && /\.(?:cmd|bat)$/i.test(agent)) {
    return spawnFn(
      [quoteForWindowsCmd(agent), ...args].join(' '),
      [],
      { ...options, shell: true },
    );
  }
  return spawnFn(agent, args, options);
}

/**
 * Run `cursor-agent mcp enable astrolabe` from the project root. Cursor
 * silently skips unapproved project MCP servers, so this is the difference
 * between "tools exist" and "the model can see them".
 */
function enableCursorApproval({
  home,
  projectRoot,
  env = process.env,
  configDir = null,
  mcpConfigPath = null,
  whichFn = whichOnPath,
  spawnFn = spawnSync,
  existsFn = fs.existsSync,
  platform = process.platform,
} = {}) {
  const root = canonicalizePath(projectRoot);
  const approvalPath = resolveApprovalPath(home, root, { env, configDir });
  const disabledPath = resolveDisabledPath(home, root, { env, configDir });

  let disabled = null;
  try {
    disabled = removeAstrolabeFromDisabled(disabledPath, { existsFn });
  } catch (error) {
    disabled = { filePath: disabledPath, error: error.message, changed: false };
  }

  const agent = whichFn('cursor-agent', env);
  if (!agent) {
    return {
      available: false,
      approval: checkApprovalFile(approvalPath, {
        projectRoot: root,
        server: currentServer(home, root, { env, configDir, mcpConfigPath }),
        env,
      }),
      disabled,
      root,
      approvalPath,
    };
  }

  const result = spawnCursorAgent(
    spawnFn,
    agent,
    ['mcp', 'enable', SERVER_NAME],
    { cwd: root, env: approvalEnv(env, home, configDir) },
    platform,
  );
  const approval = checkApprovalFile(approvalPath, {
    projectRoot: root,
    server: currentServer(home, root, { env, configDir, mcpConfigPath }),
    env,
  });
  if (result.status !== 0 || result.error || result.signal) approval.approved = false;
  return {
    available: true,
    agent,
    root,
    approvalPath,
    approval,
    disabled,
    status: result.status,
    signal: result.signal || null,
    stdout: String(result.stdout || '').trim(),
    stderr: String(result.stderr || '').trim(),
    error: result.error ? result.error.message : null,
  };
}

module.exports = {
  SERVER_NAME,
  DEFAULT_LSP_TIMEOUT,
  L1_PLACEHOLDER,
  REMIND_MATCHER,
  REMIND_ARGS,
  CLEANUP_ARGS,
  FALLBACK_L1_TEMPLATE,
  spawnCursorAgent,
  expandHome,
  readEnvDir,
  resolveCursorConfigDir,
  resolveCursorHooksPath,
  quoteCommandPath,
  unquoteHookExecutable,
  isAstrolabeHookCommand,
  isAstrolabeHook,
  hookCommand,
  remindEntry,
  cleanupEntry,
  mergeCursorHooksFile,
  resolveProjectRoot,
  canonicalizePath,
  projectSlug,
  resolveCursorProjectDir,
  resolveApprovalPath,
  resolveDisabledPath,
  detectPyright,
  buildCursorEnv,
  cursorServerEntry,
  resolveL1TemplatePath,
  loadL1Template,
  renderCursorRules,
  mergeRulesFile,
  normalizeCursorServer,
  effectiveCursorConfig,
  inspectCursorConfig,
  approvalKey,
  checkApprovalFile,
  removeAstrolabeFromDisabled,
  approvalEnv,
  enableCursorApproval,
};
