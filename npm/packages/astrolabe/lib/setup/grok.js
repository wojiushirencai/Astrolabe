'use strict';

const fs = require('fs');
const path = require('path');

const GROK_HOOKS_DOC = {
  hooks: {
    PreToolUse: [
      {
        matcher: 'Bash|Read|Grep|run_terminal_command|read_file|grep|mcp__astrolabe__.*',
        hooks: [
          {
            type: 'command',
            command: 'astrolabe hooks remind --client=grok',
            timeout: 10,
          },
        ],
      },
    ],
    SessionEnd: [
      {
        matcher: '',
        hooks: [
          {
            type: 'command',
            command: 'astrolabe hooks cleanup',
            timeout: 10,
          },
        ],
      },
    ],
  },
};

function detectGrok(home, env, { whichFn, existsFn }) {
  const evidence = [];
  if (whichFn('grok', env)) evidence.push('grok on PATH');
  if (existsFn(path.join(home, '.grok'))) evidence.push('~/.grok/');
  if (existsFn(path.join(home, '.grok', 'config.toml'))) evidence.push('~/.grok/config.toml');
  return { detected: evidence.length > 0, evidence };
}

function resolveGrokConfigToml(home) {
  return path.join(home, '.grok', 'config.toml');
}

function resolveGrokHooksPath(home) {
  return path.join(home, '.grok', 'hooks', 'astrolabe.json');
}

/**
 * Write/replace the dedicated Astrolabe hooks file (merge root.hooks keys only
 * when an existing file already has unrelated top-level fields).
 */
function mergeGrokHooksFile(filePath, { create = true, existsFn = fs.existsSync } = {}) {
  let created = false;
  let doc = {};
  if (existsFn(filePath)) {
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

  const replaced = Boolean(doc.hooks);
  doc.hooks = GROK_HOOKS_DOC.hooks;
  return { filePath, doc, created, replaced, entryPreview: JSON.stringify(GROK_HOOKS_DOC, null, 2) };
}

module.exports = {
  GROK_HOOKS_DOC,
  detectGrok,
  resolveGrokConfigToml,
  resolveGrokHooksPath,
  mergeGrokHooksFile,
};
