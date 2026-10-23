'use strict';

const fs = require('fs');
const path = require('path');

const REMIND_CMD = 'astrolabe hooks remind --client=codebuddy';
const CLEANUP_CMD = 'astrolabe hooks cleanup';

function resolveCodebuddyMcpPath(home, existsFn) {
  const candidates = [
    path.join(home, '.codebuddy', '.mcp.json'),
    path.join(home, '.codebuddy', 'mcp.json'),
    path.join(home, '.codebuddy.json'),
  ];
  for (const cand of candidates) {
    if (existsFn(cand)) return cand;
  }
  return candidates[0];
}

function detectCodebuddy(home, env, { whichFn, existsFn }) {
  const evidence = [];
  if (whichFn('codebuddy', env)) evidence.push('codebuddy on PATH');
  if (existsFn(path.join(home, '.codebuddy'))) evidence.push('~/.codebuddy/');
  if (existsFn(path.join(home, '.codebuddy.json'))) evidence.push('~/.codebuddy.json');
  return { detected: evidence.length > 0, evidence };
}

function commandHookEntry(command) {
  return {
    matcher: '',
    hooks: [{ type: 'command', command }],
  };
}

function entryHasAstrolabeCommand(entry, needle) {
  if (!entry || typeof entry !== 'object') return false;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  return hooks.some(
    (h) => h && typeof h.command === 'string' && h.command.includes(needle),
  );
}

/**
 * Merge CC-compatible PreToolUse + SessionEnd into ~/.codebuddy/settings.json
 * without clobbering other hook entries.
 */
function mergeCodebuddyHooksSettings(filePath, { create = true, existsFn = fs.existsSync } = {}) {
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

  if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) {
    doc.hooks = {};
  }
  const hooks = doc.hooks;
  let replaced = false;

  for (const [event, command, needle] of [
    ['PreToolUse', REMIND_CMD, 'astrolabe hooks remind --client=codebuddy'],
    ['SessionEnd', CLEANUP_CMD, 'astrolabe hooks cleanup'],
  ]) {
    if (!Array.isArray(hooks[event])) hooks[event] = [];
    const idx = hooks[event].findIndex((e) => entryHasAstrolabeCommand(e, needle));
    const next = commandHookEntry(command);
    if (idx >= 0) {
      hooks[event][idx] = next;
      replaced = true;
    } else {
      hooks[event].push(next);
    }
  }

  return { filePath, doc, created, replaced };
}

module.exports = {
  REMIND_CMD,
  CLEANUP_CMD,
  resolveCodebuddyMcpPath,
  detectCodebuddy,
  mergeCodebuddyHooksSettings,
};
