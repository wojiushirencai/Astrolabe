'use strict';

const fs = require('fs');
const path = require('path');

const REMIND_CMD = 'astrolabe hooks remind --client=cursor';
const CLEANUP_CMD = 'astrolabe hooks cleanup';
/** Matcher must include mcp__astrolabe__.* so successful Astrolabe calls reset counters. */
const REMIND_MATCHER = 'Shell|Read|Grep|mcp__astrolabe__.*';

function resolveCursorHooksPath(home) {
  return path.join(home, '.cursor', 'hooks.json');
}

function remindEntry() {
  return {
    command: REMIND_CMD,
    matcher: REMIND_MATCHER,
  };
}

function cleanupEntry() {
  return {
    command: CLEANUP_CMD,
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

function isAstrolabeRemind(entry) {
  return entryCommand(entry).includes('astrolabe hooks remind');
}

function isAstrolabeCleanup(entry) {
  return entryCommand(entry).includes('astrolabe hooks cleanup');
}

/**
 * Merge ~/.cursor/hooks.json (Cursor CLI native format: camelCase events,
 * flat command entries). Enforces --client=cursor and the documented matcher
 * that includes mcp__astrolabe__.* so counter resets work.
 */
function mergeCursorHooksFile(filePath, { create = true, existsFn = fs.existsSync } = {}) {
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

  if (doc.version == null) doc.version = 1;
  if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) {
    doc.hooks = {};
  }
  const hooks = doc.hooks;
  let replaced = false;

  if (!Array.isArray(hooks.preToolUse)) hooks.preToolUse = [];
  const remindIdx = hooks.preToolUse.findIndex(isAstrolabeRemind);
  const nextRemind = remindEntry();
  if (remindIdx >= 0) {
    hooks.preToolUse[remindIdx] = nextRemind;
    replaced = true;
  } else {
    hooks.preToolUse.push(nextRemind);
  }

  if (!Array.isArray(hooks.sessionEnd)) hooks.sessionEnd = [];
  const cleanupIdx = hooks.sessionEnd.findIndex(isAstrolabeCleanup);
  const nextCleanup = cleanupEntry();
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

module.exports = {
  REMIND_CMD,
  CLEANUP_CMD,
  REMIND_MATCHER,
  resolveCursorHooksPath,
  mergeCursorHooksFile,
};
