'use strict';

const fs = require('fs');
const path = require('path');
const { expandHome, readEnvDir } = require('./cursor');

const MANAGED_BEGIN = '# >>> astrolabe-managed-kimi-hooks >>>';
const MANAGED_END = '# <<< astrolabe-managed-kimi-hooks <<<';

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
 * Strip the astrolabe-managed-kimi-hooks block from ~/.kimi-code/config.toml
 * while preserving all other TOML tables and user [[hooks]] blocks.
 */
function stripKimiHooksToml(filePath, { existsFn = fs.existsSync } = {}) {
  if (!existsFn(filePath)) {
    return { skipped: true, reason: `missing ${filePath}` };
  }
  const text = fs.readFileSync(filePath, 'utf8');
  const begin = text.indexOf(MANAGED_BEGIN);
  const end = text.indexOf(MANAGED_END);
  if (begin < 0 && end < 0) {
    return { skipped: true, reason: `no astrolabe-managed-kimi-hooks block in ${filePath}` };
  }
  if (begin < 0 || end < 0 || end < begin) {
    throw new Error(
      `${filePath} has a broken astrolabe-managed-kimi-hooks marker; fix or remove it, then re-run setup`,
    );
  }

  const before = text.slice(0, begin).replace(/\s+$/, '');
  let after = text.slice(end + MANAGED_END.length).replace(/^(?:[ \t]*\r?\n)+/, '');
  if (after.trim() === '') after = '';
  let next;
  if (before && after) {
    next = `${before}\n\n${after}`;
    if (!next.endsWith('\n')) next += '\n';
  } else if (before) {
    next = `${before}\n`;
  } else if (after) {
    next = after.endsWith('\n') ? after : `${after}\n`;
  } else {
    next = '';
  }

  return {
    filePath,
    text: next,
    created: false,
    replaced: true,
    removed: true,
  };
}

module.exports = {
  MANAGED_BEGIN,
  MANAGED_END,
  resolveKimiConfigDir,
  detectKimiCode,
  resolveKimiMcpPath,
  resolveKimiConfigToml,
  stripKimiHooksToml,
};
