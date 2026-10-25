'use strict';

const fs = require('fs');
const path = require('path');

const REMIND_CMD = 'astrolabe hooks remind --client=zcode';

function detectZCode(home, env, { whichFn, existsFn }) {
  const evidence = [];
  if (whichFn('zcode', env)) evidence.push('zcode on PATH');
  if (existsFn(path.join(home, '.zcode'))) evidence.push('~/.zcode/');
  if (existsFn(path.join(home, '.zcode', 'cli'))) evidence.push('~/.zcode/cli/');
  if (existsFn(path.join(home, '.zcode', 'cli', 'config.json'))) {
    evidence.push('~/.zcode/cli/config.json');
  }
  return { detected: evidence.length > 0, evidence };
}

function resolveZcodeConfigPath(home, existsFn) {
  // Prefer documented ~/.zcode/cli/config.json (HOOKS.md + z.ai docs).
  // Some forks also use setting.json; only fall back when that alone exists.
  const configJson = path.join(home, '.zcode', 'cli', 'config.json');
  const settingJson = path.join(home, '.zcode', 'cli', 'setting.json');
  if (existsFn(configJson)) return configJson;
  if (existsFn(settingJson)) return settingJson;
  return configJson;
}

function zcodeServerEntry(launch) {
  return {
    type: 'stdio',
    command: launch.command,
    args: launch.args,
  };
}

function preToolUseEntry() {
  return {
    matcher: '*',
    hooks: [
      {
        type: 'command',
        command: REMIND_CMD,
        timeout: 10,
      },
    ],
  };
}

function hookCommands(entry) {
  if (!entry || typeof entry !== 'object') return [];
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  return hooks
    .filter((h) => h && typeof h.command === 'string')
    .map((h) => h.command);
}

function entryHasAstrolabeRemind(entry) {
  return hookCommands(entry).some((cmd) => cmd.includes('astrolabe hooks remind'));
}

/**
 * Strip cleanup hooks from a Stop entry; drop the entry if nothing remains.
 * Returns { entry | null, stripped }.
 */
function scrubStopEntry(entry) {
  if (!entry || typeof entry !== 'object') return { entry: null, stripped: false };
  if (!entryHasCleanup(entry)) return { entry, stripped: false };
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  const kept = hooks.filter(
    (h) => !(h && typeof h.command === 'string' && h.command.includes('astrolabe hooks cleanup')),
  );
  if (!kept.length) return { entry: null, stripped: true };
  return { entry: { ...entry, hooks: kept }, stripped: true };
}

/**
 * True when a Stop (or similar) event entry hangs `astrolabe hooks cleanup`.
 * ZCode fires Stop at every turn end — cleanup there would wipe counters each turn.
 */
function entryHasCleanup(entry) {
  if (!entry || typeof entry !== 'object') return false;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  return hooks.some(
    (h) =>
      h &&
      typeof h.command === 'string' &&
      h.command.includes('astrolabe hooks cleanup'),
  );
}


/**
 * Merge mcp.servers.astrolabe and (optional) hooks.events.PreToolUse into
 * ~/.zcode/cli/config.json. Never writes SessionEnd; never hangs cleanup on Stop
 * (Stop is turn-end — use `astrolabe hooks gc`). Replaces any prior Astrolabe
 * remind (including wrong --client=claude-code).
 */
function mergeZcodeConfigFile(
  filePath,
  launch,
  { create = true, withHooks = false, existsFn = fs.existsSync } = {},
) {
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

  if (!doc.mcp || typeof doc.mcp !== 'object' || Array.isArray(doc.mcp)) {
    doc.mcp = {};
  }
  if (!doc.mcp.servers || typeof doc.mcp.servers !== 'object' || Array.isArray(doc.mcp.servers)) {
    doc.mcp.servers = {};
  }
  const hadServer = Object.prototype.hasOwnProperty.call(doc.mcp.servers, 'astrolabe');
  doc.mcp.servers.astrolabe = zcodeServerEntry(launch);

  let hooksReplaced = false;
  if (withHooks) {
    if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) {
      doc.hooks = {};
    }
    doc.hooks.enabled = true;
    if (!doc.hooks.events || typeof doc.hooks.events !== 'object' || Array.isArray(doc.hooks.events)) {
      doc.hooks.events = {};
    }
    // Hard rule: never SessionEnd (ZCode rejects unknown events).
    if (Object.prototype.hasOwnProperty.call(doc.hooks.events, 'SessionEnd')) {
      delete doc.hooks.events.SessionEnd;
      hooksReplaced = true;
    }
    // Hard rule: never hang cleanup on Stop (fires every turn; would zero counters).
    if (Array.isArray(doc.hooks.events.Stop)) {
      const scrubbed = [];
      for (const entry of doc.hooks.events.Stop) {
        const { entry: next, stripped } = scrubStopEntry(entry);
        if (stripped) hooksReplaced = true;
        if (next) scrubbed.push(next);
      }
      if (scrubbed.length) {
        doc.hooks.events.Stop = scrubbed;
      } else {
        delete doc.hooks.events.Stop;
      }
    }
    if (!Array.isArray(doc.hooks.events.PreToolUse)) {
      doc.hooks.events.PreToolUse = [];
    }
    // Match any prior remind (wrong --client=claude-code included) and replace.
    const idx = doc.hooks.events.PreToolUse.findIndex(entryHasAstrolabeRemind);
    const next = preToolUseEntry();
    if (idx >= 0) {
      doc.hooks.events.PreToolUse[idx] = next;
      hooksReplaced = true;
    } else {
      doc.hooks.events.PreToolUse.push(next);
    }
  }

  return {
    filePath,
    doc,
    created,
    replaced: hadServer || hooksReplaced,
    entry: doc.mcp.servers.astrolabe,
    withHooks,
  };
}

module.exports = {
  REMIND_CMD,
  detectZCode,
  resolveZcodeConfigPath,
  zcodeServerEntry,
  mergeZcodeConfigFile,
};
