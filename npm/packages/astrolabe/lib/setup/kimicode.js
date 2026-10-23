'use strict';

const fs = require('fs');
const path = require('path');

const MANAGED_BEGIN = '# >>> astrolabe-managed-kimi-hooks >>>';
const MANAGED_END = '# <<< astrolabe-managed-kimi-hooks <<<';

const MANAGED_BLOCK = `${MANAGED_BEGIN}
[[hooks]]
event = "PreToolUse"
matcher = "Read|Grep|Bash|mcp__astrolabe__.*"
command = "astrolabe hooks remind --client=kimicode"

[[hooks]]
event = "SessionEnd"
command = "astrolabe hooks cleanup"
${MANAGED_END}
`;

function detectKimiCode(home, env, { whichFn, existsFn }) {
  const evidence = [];
  if (whichFn('kimi', env)) evidence.push('kimi on PATH');
  if (existsFn(path.join(home, '.kimi-code'))) evidence.push('~/.kimi-code/');
  if (existsFn(path.join(home, '.kimi-code', 'config.toml'))) {
    evidence.push('~/.kimi-code/config.toml');
  }
  if (existsFn(path.join(home, '.kimi-code', 'mcp.json'))) {
    evidence.push('~/.kimi-code/mcp.json');
  }
  return { detected: evidence.length > 0, evidence };
}

function resolveKimiMcpPath(home) {
  return path.join(home, '.kimi-code', 'mcp.json');
}

function resolveKimiConfigToml(home) {
  return path.join(home, '.kimi-code', 'config.toml');
}

/**
 * Merge managed [[hooks]] into ~/.kimi-code/config.toml without touching other
 * tables or user hook blocks (same marker strategy as DSH cordis patch).
 */
function mergeKimiHooksToml(filePath, { create = true, existsFn = fs.existsSync } = {}) {
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

  const begin = text.indexOf(MANAGED_BEGIN);
  const end = text.indexOf(MANAGED_END);
  let next;
  let replaced = false;
  if (begin >= 0 && end > begin) {
    replaced = true;
    const afterEnd = end + MANAGED_END.length;
    let tail = text.slice(afterEnd);
    if (tail.startsWith('\n')) tail = tail.slice(1);
    next = `${text.slice(0, begin)}${MANAGED_BLOCK}${tail}`;
  } else if (begin >= 0 || end >= 0) {
    throw new Error(
      `${filePath} has a broken astrolabe-managed-kimi-hooks marker; fix or remove it, then re-run setup`,
    );
  } else {
    const trimmed = text.replace(/\s*$/, '');
    next = trimmed ? `${trimmed}\n\n${MANAGED_BLOCK}` : `${MANAGED_BLOCK}`;
  }
  if (!next.endsWith('\n')) next += '\n';
  return {
    filePath,
    text: next,
    created,
    replaced,
    entryPreview: MANAGED_BLOCK.trim(),
  };
}

module.exports = {
  MANAGED_BEGIN,
  MANAGED_END,
  MANAGED_BLOCK,
  detectKimiCode,
  resolveKimiMcpPath,
  resolveKimiConfigToml,
  mergeKimiHooksToml,
};
