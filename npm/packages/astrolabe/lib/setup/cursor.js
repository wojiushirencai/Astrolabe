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

function resolveCursorHooksPath(home) {
  return path.join(home, '.cursor', 'hooks.json');
}

/** Quote a binary path so hook commands survive spaces (both platforms). */
function quoteCommandPath(binPath) {
  if (!binPath) return 'astrolabe';
  if (process.platform === 'win32') {
    return /\s/.test(binPath) ? `"${binPath}"` : binPath;
  }
  if (/^[A-Za-z0-9_./:@%+,\-]+$/.test(binPath)) return binPath;
  return `'${binPath.replace(/'/g, `'\\''`)}'`;
}

function hookCommand(binPath, args) {
  return [quoteCommandPath(binPath), ...args].join(' ');
}

function remindEntry(binPath) {
  return {
    command: hookCommand(binPath, REMIND_ARGS),
    matcher: REMIND_MATCHER,
  };
}

function cleanupEntry(binPath) {
  return {
    command: hookCommand(binPath, CLEANUP_ARGS),
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

function isAstrolabeHook(entry, subcommand, binPath) {
  // Never replace a legacy group containing unrelated hooks as a whole.
  if (entry && !entry.command && Array.isArray(entry.hooks) && entry.hooks.length !== 1) {
    return false;
  }
  const command = entryCommand(entry);
  const match = command.match(/^\s*(.*?)\s+hooks\s+(remind|cleanup)(?:\s|$)/);
  if (!match || match[2] !== subcommand) return false;
  const executable = match[1];
  // Explicit --command paths may have arbitrary names. Otherwise only claim
  // the actual Astrolabe executable, not another program with similar args.
  if (binPath && executable === quoteCommandPath(binPath)) return true;
  if (!/^(?:"[^"]*"|'[^']*'|[^\s"';&|<>`]+)$/.test(executable)) return false;
  const unquoted = executable.replace(/^(?:"([^"]*)"|'([^']*)')$/, (_, a, b) => a || b);
  return /^(?:.*[/\\])?astrolabe(?:\.(?:exe|cmd|bat))?$/i.test(unquoted);
}

/**
 * Merge ~/.cursor/hooks.json (Cursor CLI native format: camelCase events,
 * flat command entries). Enforces --client=cursor, an absolute binary path,
 * and the documented matcher that includes Cursor's `astrolabe-*` MCP names
 * so counter resets work.
 */
function mergeCursorHooksFile(
  filePath,
  { create = true, existsFn = fs.existsSync, binPath = null } = {},
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

  if (!Array.isArray(hooks.preToolUse)) hooks.preToolUse = [];
  const remindIdx = hooks.preToolUse.findIndex((entry) => isAstrolabeHook(entry, 'remind', binPath));
  const nextRemind = remindEntry(binPath);
  if (remindIdx >= 0) {
    hooks.preToolUse[remindIdx] = nextRemind;
    replaced = true;
  } else {
    hooks.preToolUse.push(nextRemind);
  }

  if (!Array.isArray(hooks.sessionEnd)) hooks.sessionEnd = [];
  const cleanupIdx = hooks.sessionEnd.findIndex((entry) => isAstrolabeHook(entry, 'cleanup', binPath));
  const nextCleanup = cleanupEntry(binPath);
  if (cleanupIdx >= 0) {
    hooks.sessionEnd[cleanupIdx] = nextCleanup;
    replaced = true;
  } else {
    hooks.sessionEnd.push(nextCleanup);
  }

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

function resolveCursorProjectDir(home, projectRoot) {
  return path.join(home, '.cursor', 'projects', projectSlug(canonicalizePath(projectRoot)));
}

function resolveApprovalPath(home, projectRoot) {
  return path.join(resolveCursorProjectDir(home, projectRoot), 'mcp-approvals.json');
}

function resolveDisabledPath(home, projectRoot) {
  return path.join(resolveCursorProjectDir(home, projectRoot), 'mcp-disabled.json');
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

/** Cursor hashes the project root and its normalized stdio server config. */
function approvalKey(projectRoot, server, serverName = SERVER_NAME) {
  if (!projectRoot || !server || typeof server.command !== 'string') return null;
  // Cursor's stdio schema strips fields such as type/cwd before hashing.
  const normalized = { command: server.command, args: server.args || [], env: server.env || {} };
  const hash = createHash('sha256')
    .update(JSON.stringify({ path: canonicalizePath(projectRoot), server: normalized }))
    .digest('hex').slice(0, 16);
  return `${serverName}-${hash}`;
}

function checkApprovalFile(filePath, { projectRoot, server, serverName = SERVER_NAME } = {}) {
  const entries = readJsonArray(filePath);
  const expectedKey = approvalKey(projectRoot, server, serverName);
  const approved = Boolean(expectedKey && entries && entries.includes(expectedKey));
  return { filePath, exists: entries !== null, approved, expectedKey, entries: entries || [] };
}

function currentServer(home, projectRoot) {
  // Project entries override global entries, as in Cursor. Invalid project
  // configuration must not turn a global approval into a successful check.
  for (const filePath of [
    path.join(projectRoot, '.cursor', 'mcp.json'),
    path.join(home, '.cursor', 'mcp.json'),
  ]) {
    try {
      const doc = JSON.parse(fs.readFileSync(filePath, 'utf8').replace(/^\uFEFF/, ''));
      if (doc.mcpServers && Object.hasOwn(doc.mcpServers, SERVER_NAME)) {
        return doc.mcpServers[SERVER_NAME];
      }
    } catch (error) {
      if (error.code !== 'ENOENT') return null;
    }
  }
  return null;
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

function approvalEnv(env, home) {
  const out = { ...env };
  // cursor-agent reads approvals relative to the user's home; mirror --home
  // so tests (and portable installs) write into the same tree setup targets.
  if (home) {
    out.HOME = home;
    if (process.platform === 'win32') out.USERPROFILE = home;
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
  whichFn = whichOnPath,
  spawnFn = spawnSync,
  existsFn = fs.existsSync,
  platform = process.platform,
} = {}) {
  const root = canonicalizePath(projectRoot);
  const approvalPath = resolveApprovalPath(home, root);
  const disabledPath = resolveDisabledPath(home, root);

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
      approval: checkApprovalFile(approvalPath, { projectRoot: root, server: currentServer(home, root) }),
      disabled,
      root,
      approvalPath,
    };
  }

  const result = spawnCursorAgent(
    spawnFn,
    agent,
    ['mcp', 'enable', SERVER_NAME],
    { cwd: root, env: approvalEnv(env, home) },
    platform,
  );
  const approval = checkApprovalFile(approvalPath, {
    projectRoot: root, server: currentServer(home, root),
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
  resolveCursorHooksPath,
  quoteCommandPath,
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
  approvalKey,
  checkApprovalFile,
  removeAstrolabeFromDisabled,
  enableCursorApproval,
};
