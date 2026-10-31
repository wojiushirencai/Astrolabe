#!/usr/bin/env node
/**
 * Merge Astrolabe's 7 catalog MCP tools into Cursor allowlists.
 *
 * Names must stay in sync with `CATALOG_TOOL_NAMES` in
 * crates/astrolabe-mcp/src/lib.rs.
 *
 * Usage:
 *   node scripts/setup-cursor-permissions.mjs
 *   node scripts/setup-cursor-permissions.mjs --cli
 *   node scripts/setup-cursor-permissions.mjs --dry-run
 *   node scripts/setup-cursor-permissions.mjs --home /tmp/fake-home
 */

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const SERVER_NAME = 'astrolabe';

/** Keep in sync with `astrolabe_mcp::CATALOG_TOOL_NAMES`. */
export const CATALOG_TOOL_NAMES = [
  'search_code',
  'find_symbol',
  'resolve_contx',
  'get_diag',
  'find_refs',
  'goto_def',
  'get_lang',
];

export const IDE_ALLOW_ENTRIES = CATALOG_TOOL_NAMES.map((name) => `${SERVER_NAME}:${name}`);
export const CLI_ALLOW_ENTRIES = CATALOG_TOOL_NAMES.map((name) => `Mcp(${SERVER_NAME}:${name})`);

const MCP_ALLOWLIST_WARNING = [
  '注意：定义 mcpAllowlist 会整表覆盖 IDE 内的 MCP allowlist；其它 MCP server 的工具请写进同一文件。',
  '此配置需要开启 Cursor Run Mode（Auto-review / Allowlist / Run Everything）才生效。',
].join('\n');

export function parseArgs(argv) {
  const opts = { dryRun: false, cli: false, home: null, help: false, yes: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dry-run') opts.dryRun = true;
    else if (arg === '--cli') opts.cli = true;
    else if (arg === '--yes' || arg === '-y') opts.yes = true;
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else if (arg === '--home') {
      const value = argv[i + 1];
      if (!value || value.startsWith('-')) {
        throw new Error('--home 需要一个目录路径');
      }
      opts.home = value;
      i += 1;
    } else {
      throw new Error(`未知参数: ${arg}`);
    }
  }
  return opts;
}

export function resolveHome(homeFlag) {
  if (homeFlag) return path.resolve(homeFlag);
  if (process.env.HOME) return process.env.HOME;
  if (process.env.USERPROFILE) return process.env.USERPROFILE;
  return os.homedir();
}

export function unionPreserve(existing, extras) {
  const seen = new Set();
  const out = [];
  for (const item of existing) {
    out.push(item);
    if (typeof item === 'string') seen.add(item);
  }
  let added = 0;
  for (const item of extras) {
    if (seen.has(item)) continue;
    seen.add(item);
    out.push(item);
    added += 1;
  }
  return { list: out, added };
}

function assertObject(value, label) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${label} 的根必须是 JSON 对象`);
  }
}

export function readJsonObject(filePath) {
  const raw = fs.readFileSync(filePath, 'utf8').replace(/^\uFEFF/, '');
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    throw new Error(
      `无法将 ${filePath} 解析为标准 JSON（不支持 JSONC 注释）。文件未修改。${error.message}`,
    );
  }
  assertObject(parsed, filePath);
  return parsed;
}

export function mergePermissionsDoc(doc, extras = IDE_ALLOW_ENTRIES) {
  assertObject(doc, 'permissions.json');
  const existing = Object.prototype.hasOwnProperty.call(doc, 'mcpAllowlist')
    ? doc.mcpAllowlist
    : [];
  if (!Array.isArray(existing)) {
    throw new Error('permissions.json 的 mcpAllowlist 必须是数组');
  }
  const { list, added } = unionPreserve(existing, extras);
  doc.mcpAllowlist = list;
  return { doc, added, total: list.length };
}

export function mergeCliConfigDoc(doc, extras = CLI_ALLOW_ENTRIES) {
  assertObject(doc, 'cli-config.json');
  if (doc.permissions == null) {
    doc.permissions = {};
  } else if (typeof doc.permissions !== 'object' || Array.isArray(doc.permissions)) {
    throw new Error('cli-config.json 的 permissions 必须是对象');
  }
  const existing = Object.prototype.hasOwnProperty.call(doc.permissions, 'allow')
    ? doc.permissions.allow
    : [];
  if (!Array.isArray(existing)) {
    throw new Error('cli-config.json 的 permissions.allow 必须是数组');
  }
  const { list, added } = unionPreserve(existing, extras);
  doc.permissions.allow = list;
  return { doc, added, total: list.length };
}

function formatJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

/**
 * Back up `filePath` (if it exists) next to itself as `<name>.bak-<timestamp>`,
 * keeping the original mode. Returns the backup path or null.
 */
export function backupFile(filePath) {
  if (!fs.existsSync(filePath)) return null;
  const real = fs.realpathSync(filePath);
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const backup = `${real}.bak-${stamp}`;
  fs.copyFileSync(real, backup);
  try {
    fs.chmodSync(backup, fs.statSync(real).mode & 0o7777);
  } catch {
    // best effort (e.g. Windows)
  }
  return backup;
}

/**
 * Write JSON atomically. Resolves symlinks first so a dotfiles-managed link is
 * not replaced by a regular file, and keeps the existing file's permission bits.
 */
export function writeJsonAtomic(filePath, value) {
  let dest = filePath;
  let mode = null;
  if (fs.existsSync(filePath)) {
    dest = fs.realpathSync(filePath);
    mode = fs.statSync(dest).mode & 0o7777;
  }
  const dir = path.dirname(dest);
  fs.mkdirSync(dir, { recursive: true });
  const tmp = path.join(dir, `.${path.basename(dest)}.${process.pid}.${Date.now()}.tmp`);
  fs.writeFileSync(tmp, formatJson(value), { encoding: 'utf8', mode: mode ?? 0o600 });
  if (mode !== null) {
    try {
      fs.chmodSync(tmp, mode);
    } catch {
      // best effort
    }
  }
  try {
    fs.renameSync(tmp, dest);
  } catch (error) {
    if (process.platform === 'win32' && (error.code === 'EPERM' || error.code === 'EEXIST')) {
      // Never delete the destination first: copy over it so there is no window
      // in which the file is missing.
      try {
        fs.copyFileSync(tmp, dest);
      } finally {
        fs.rmSync(tmp, { force: true });
      }
      return;
    }
    try {
      fs.rmSync(tmp, { force: true });
    } catch {
      // ignore cleanup failure
    }
    throw error;
  }
}

/** Ask a yes/no question on the terminal. Non-interactive stdin counts as "no". */
function confirmInteractive(question) {
  if (!process.stdin.isTTY) return false;
  process.stdout.write(`${question} [y/N] `);
  const buf = Buffer.alloc(256);
  let n = 0;
  try {
    n = fs.readSync(0, buf, 0, buf.length, null);
  } catch {
    return false;
  }
  return /^y(es)?$/i.test(buf.toString('utf8', 0, n).trim());
}

function printHelp() {
  process.stdout.write(`Merge Astrolabe 的 7 个 MCP 工具到 Cursor allowlist。

用法:
  node scripts/setup-cursor-permissions.mjs [--cli] [--dry-run] [--home <dir>]

  --cli       同时 merge ~/.cursor/cli-config.json 的 permissions.allow
              （文件不存在则跳过，不会新建整个 cli-config）
  --dry-run   只打印将要写入的内容，不改文件
  --yes, -y   permissions.json 不存在时直接新建（否则交互确认，非交互环境会拒绝新建）
  --home      覆盖 HOME（测试用）

工具列表: ${CATALOG_TOOL_NAMES.join(', ')}
`);
}

function loadOrCreatePermissions(filePath) {
  if (!fs.existsSync(filePath)) {
    return { ...mergePermissionsDoc({}), created: true };
  }
  return { ...mergePermissionsDoc(readJsonObject(filePath)), created: false };
}

function printPlan(label, filePath, doc) {
  process.stdout.write(`${label}${filePath}:\n${formatJson(doc)}`);
}

export function run(argv = process.argv.slice(2)) {
  const opts = parseArgs(argv);
  if (opts.help) {
    printHelp();
    return 0;
  }

  const home = resolveHome(opts.home);
  const cursorDir = path.join(home, '.cursor');
  const permissionsPath = path.join(cursorDir, 'permissions.json');
  const cliConfigPath = path.join(cursorDir, 'cli-config.json');

  const permissionsPlan = loadOrCreatePermissions(permissionsPath);

  let cliPlan = null;
  let cliSkipped = false;
  if (opts.cli) {
    if (!fs.existsSync(cliConfigPath)) {
      cliSkipped = true;
    } else {
      cliPlan = mergeCliConfigDoc(readJsonObject(cliConfigPath));
    }
  }

  if (opts.dryRun) {
    process.stdout.write('[dry-run] 将写入 ');
    printPlan('', permissionsPath, permissionsPlan.doc);
    if (opts.cli && cliSkipped) {
      process.stdout.write(
        `未找到 ${cliConfigPath}，已跳过 CLI allow 合并（不会新建该文件）。\n`,
      );
    } else if (cliPlan) {
      process.stdout.write('[dry-run] 将写入 ');
      printPlan('', cliConfigPath, cliPlan.doc);
    }
    process.stdout.write(`${MCP_ALLOWLIST_WARNING}\n`);
    return 0;
  }

  // 先提示，再写：mcpAllowlist 会整表覆盖 IDE 内的 MCP allowlist。
  process.stdout.write(`${MCP_ALLOWLIST_WARNING}\n`);
  if (permissionsPlan.created) {
    process.stdout.write(
      `${permissionsPath} 不存在。新建它会让 Cursor 只允许上面 ${IDE_ALLOW_ENTRIES.length} 个 astrolabe 工具，` +
        '你已在 IDE 里允许的其它 MCP 工具会被覆盖。\n',
    );
    if (!opts.yes && !confirmInteractive('确认新建？')) {
      process.stderr.write(
        '未新建 permissions.json（需要新建时请加 --yes，或先用 --dry-run 查看内容）。\n',
      );
      return 1;
    }
  }

  const permissionsBackup = backupFile(permissionsPath);
  if (permissionsBackup) {
    process.stdout.write(`已备份原文件到 ${permissionsBackup}\n`);
  }
  writeJsonAtomic(permissionsPath, permissionsPlan.doc);
  process.stdout.write(
    `已合并 ${permissionsPath}（新增 ${permissionsPlan.added} 条，现共 ${permissionsPlan.total} 条）。\n`,
  );

  if (opts.cli && cliSkipped) {
    process.stdout.write(
      `未找到 ${cliConfigPath}，已跳过 CLI allow 合并（不会新建该文件）。需要时请先在 Cursor CLI 里生成配置后再加 --cli。\n`,
    );
  } else if (cliPlan) {
    const cliBackup = backupFile(cliConfigPath);
    if (cliBackup) {
      process.stdout.write(`已备份原文件到 ${cliBackup}\n`);
    }
    writeJsonAtomic(cliConfigPath, cliPlan.doc);
    process.stdout.write(
      `已合并 ${cliConfigPath} 的 permissions.allow（新增 ${cliPlan.added} 条，现共 ${cliPlan.total} 条）。未改 approvalMode 和其它字段。\n`,
    );
  }
  return 0;
}

function isEntrypoint() {
  const entry = process.argv[1];
  if (!entry) return false;
  try {
    const thisFile = fs.realpathSync(fileURLToPath(import.meta.url));
    return fs.realpathSync(path.resolve(entry)) === thisFile;
  } catch {
    return pathToFileURL(path.resolve(entry)).href === import.meta.url;
  }
}

if (isEntrypoint()) {
  try {
    process.exitCode = run(process.argv.slice(2));
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  }
}
