'use strict';

const fs = require('fs');
const path = require('path');
const { expandHome, readEnvDir, hookCommand } = require('./cursor');

const MANAGED_BEGIN = '# >>> astrolabe-managed-kimi-hooks >>>';
const MANAGED_END = '# <<< astrolabe-managed-kimi-hooks <<<';

const REMIND_ARGS = ['hooks', 'remind', '--client=kimicode'];
const CLEANUP_ARGS = ['hooks', 'cleanup'];

function renderKimiManagedBlock(binPath = null, platform = process.platform) {
  const remindCmd = hookCommand(binPath, REMIND_ARGS, platform);
  const cleanupCmd = hookCommand(binPath, CLEANUP_ARGS, platform);
  return `${MANAGED_BEGIN}
[[hooks]]
event = "PreToolUse"
matcher = "Read|Grep|Bash|mcp__astrolabe__.*"
command = ${JSON.stringify(remindCmd)}

[[hooks]]
event = "SessionEnd"
command = ${JSON.stringify(cleanupCmd)}
${MANAGED_END}
`;
}

const MANAGED_BLOCK = renderKimiManagedBlock();

function resolveKimiConfigDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return (
    readEnvDir(env, ['KIMI_CONFIG_DIR', 'KIMI_HOME'], home) || path.join(home, '.kimi-code')
  );
}

function detectKimiCode(home, env, { whichFn, existsFn }) {
  const evidence = [];
  const dir = resolveKimiConfigDir(home, env);
  const hasCustom = Boolean(readEnvDir(env, ['KIMI_CONFIG_DIR', 'KIMI_HOME'], home));
  if (whichFn('kimi', env)) evidence.push('kimi on PATH');
  if (existsFn(dir)) evidence.push(hasCustom ? dir : '~/.kimi-code/');
  if (existsFn(path.join(dir, 'config.toml'))) {
    evidence.push(hasCustom ? path.join(dir, 'config.toml') : '~/.kimi-code/config.toml');
  }
  if (existsFn(path.join(dir, 'mcp.json'))) {
    evidence.push(hasCustom ? path.join(dir, 'mcp.json') : '~/.kimi-code/mcp.json');
  }
  return { detected: evidence.length > 0, evidence };
}

function resolveKimiMcpPath(home, env = process.env, configDir = null) {
  return path.join(resolveKimiConfigDir(home, env, configDir), 'mcp.json');
}

function resolveKimiConfigToml(home, env = process.env, configDir = null) {
  return path.join(resolveKimiConfigDir(home, env, configDir), 'config.toml');
}

/**
 * Merge managed [[hooks]] into ~/.kimi-code/config.toml without touching other
 * tables or user hook blocks (same marker strategy as DSH cordis patch).
 */
function mergeKimiHooksToml(
  filePath,
  { create = true, existsFn = fs.existsSync, binPath = null, platform = process.platform } = {},
) {
  let created = false;
  let text = '';
  if (existsFn(filePath)) {
    text = fs.readFileSync(filePath, 'utf8');
  } else if (!create) {
    return { skipped: true, reason: `missing ${filePath} (will not create)` };
  } else {
    created = true;
    text = '';
  }

  const block = renderKimiManagedBlock(binPath, platform);
  const begin = text.indexOf(MANAGED_BEGIN);
  const end = text.indexOf(MANAGED_END);
  let next;
  let replaced = false;
  if (begin >= 0 && end > begin) {
    replaced = true;
    const afterEnd = end + MANAGED_END.length;
    let tail = text.slice(afterEnd);
    if (tail.startsWith('\n')) tail = tail.slice(1);
    next = `${text.slice(0, begin)}${block}${tail}`;
  } else if (begin >= 0 || end >= 0) {
    throw new Error(
      `${filePath} has a broken astrolabe-managed-kimi-hooks marker; fix or remove it, then re-run setup`,
    );
  } else {
    const trimmed = text.replace(/\s*$/, '');
    next = trimmed ? `${trimmed}\n\n${block}` : `${block}`;
  }
  if (!next.endsWith('\n')) next += '\n';
  return {
    filePath,
    text: next,
    created,
    replaced,
    entryPreview: block.trim(),
  };
}

module.exports = {
  MANAGED_BEGIN,
  MANAGED_END,
  MANAGED_BLOCK,
  renderKimiManagedBlock,
  resolveKimiConfigDir,
  detectKimiCode,
  resolveKimiMcpPath,
  resolveKimiConfigToml,
  mergeKimiHooksToml,
};
