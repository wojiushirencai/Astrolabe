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

function entryHasRemind(entry) {
  if (!entry || typeof entry !== 'object') return false;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  return hooks.some(
    (h) =>
      h &&
      typeof h.command === 'string' &&
      h.command.includes('astrolabe hooks remind --client=zcode'),
  );
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
 * Strip SessionEnd entirely. On Stop, remove only `astrolabe hooks cleanup`
 * commands (and drop empty entries). Never invents a Stop event.
 */
function sanitizeZcodeHookEvents(events) {
  if (!events || typeof events !== 'object' || Array.isArray(events)) return;
  if (Object.prototype.hasOwnProperty.call(events, 'SessionEnd')) {
    delete events.SessionEnd;
  }
  if (Array.isArray(events.Stop)) {
    const next = [];
    for (const entry of events.Stop) {
      if (!entry || typeof entry !== 'object') continue;
      if (!Array.isArray(entry.hooks)) {
        next.push(entry);
        continue;
      }
      const hooks = entry.hooks.filter(
        (h) =>
          !(
            h &&
            typeof h.command === 'string' &&
            h.command.includes('astrolabe hooks cleanup')
          ),
      );
      if (hooks.length === 0) continue;
      next.push({ ...entry, hooks });
    }
    if (next.length === 0) {
      delete events.Stop;
    } else {
      events.Stop = next;
    }
  }
}

/**
 * Merge mcp.servers.astrolabe and (optional) hooks.events.PreToolUse into
 * ~/.zcode/cli/config.json. Never writes SessionEnd; never hangs cleanup on Stop
 * (Stop is turn-end, not session-end — use `astrolabe hooks gc` instead).
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
    // Hard rules for hosts without SessionEnd (ZCode): never SessionEnd;
    // never hang cleanup on Stop (turn-end would wipe drift counters every turn).
    sanitizeZcodeHookEvents(doc.hooks.events);
    if (!Array.isArray(doc.hooks.events.PreToolUse)) {
      doc.hooks.events.PreToolUse = [];
    }
    const idx = doc.hooks.events.PreToolUse.findIndex(entryHasRemind);
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
  entryHasCleanup,
  sanitizeZcodeHookEvents,
};
