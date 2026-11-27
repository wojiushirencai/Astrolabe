'use strict';

const fs = require('fs');
const path = require('path');
const {
  expandHome,
  readEnvDir,
  hookCommand,
  isAstrolabeHookCommand,
} = require('./cursor');

const REMIND_ARGS = ['hooks', 'remind', '--client=zcode'];
const REMIND_CMD = 'astrolabe hooks remind --client=zcode';

function resolveZcodeConfigDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return readEnvDir(env, ['ZCODE_CONFIG_DIR', 'ZCODE_HOME'], home) || path.join(home, '.zcode');
}

function detectZCode(home, env, { whichFn, existsFn }) {
  const evidence = [];
  const dir = resolveZcodeConfigDir(home, env);
  const hasCustom = Boolean(readEnvDir(env, ['ZCODE_CONFIG_DIR', 'ZCODE_HOME'], home));
  if (whichFn('zcode', env)) evidence.push('zcode on PATH');
  if (existsFn(dir)) evidence.push(hasCustom ? dir : '~/.zcode/');
  if (existsFn(path.join(dir, 'cli'))) {
    evidence.push(hasCustom ? path.join(dir, 'cli') : '~/.zcode/cli/');
  }
  for (const cand of [
    path.join(dir, 'cli', 'config.json'),
    path.join(dir, 'cli', 'setting.json'),
    ...(hasCustom && !existsFn(path.join(dir, 'cli'))
      ? [path.join(dir, 'config.json'), path.join(dir, 'setting.json')]
      : []),
  ]) {
    if (existsFn(cand)) {
      evidence.push(cand);
      break;
    }
  }
  return { detected: evidence.length > 0, evidence };
}

function resolveZcodeConfigPath(home, existsFn, env = process.env, configDir = null) {
  // Prefer documented ~/.zcode/cli/config.json (HOOKS.md + z.ai docs).
  // Some forks also use setting.json; only fall back when that alone exists.
  const dir = resolveZcodeConfigDir(home, env, configDir);
  const cliDir = path.join(dir, 'cli');
  const configJson = path.join(cliDir, 'config.json');
  const settingJson = path.join(cliDir, 'setting.json');
  if (existsFn(configJson)) return configJson;
  if (existsFn(settingJson)) return settingJson;
  const hasCustom = Boolean(
    expandHome(configDir, home) || readEnvDir(env, ['ZCODE_CONFIG_DIR', 'ZCODE_HOME'], home),
  );
  if (hasCustom && !existsFn(cliDir)) {
    const directConfig = path.join(dir, 'config.json');
    const directSetting = path.join(dir, 'setting.json');
    if (existsFn(directConfig)) return directConfig;
    if (existsFn(directSetting)) return directSetting;
  }
  return configJson;
}

function zcodeServerEntry(launch) {
  return {
    type: 'stdio',
    command: launch.command,
    args: launch.args,
  };
}

function preToolUseEntry(binPath = null, platform = process.platform) {
  return {
    matcher: '*',
    hooks: [
      {
        type: 'command',
        command: hookCommand(binPath, REMIND_ARGS, platform),
        timeout: 10,
      },
    ],
  };
}

function isAstrolabeRemindCommand(command, binPath = null, platform = process.platform) {
  return isAstrolabeHookCommand(command, {
    subcommands: ['remind'],
    binPath,
    platform,
  });
}

function isAstrolabeCleanupCommand(command, binPath = null, platform = process.platform) {
  return isAstrolabeHookCommand(command, {
    subcommands: ['cleanup'],
    binPath,
    platform,
  });
}

function entryIsSingleAstrolabeRemind(entry, binPath = null, platform = process.platform) {
  if (!entry || typeof entry !== 'object') return false;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  if (hooks.length !== 1) return false;
  return isAstrolabeRemindCommand(hooks[0] && hooks[0].command, binPath, platform);
}

/**
 * Strip cleanup hooks from a Stop entry; drop the entry if nothing remains.
 * Returns { entry | null, stripped }.
 */
function scrubStopEntry(entry, binPath = null, platform = process.platform) {
  if (!entry || typeof entry !== 'object') return { entry: null, stripped: false };
  if (!entryHasCleanup(entry, binPath, platform)) return { entry, stripped: false };
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  const kept = hooks.filter(
    (h) => !isAstrolabeCleanupCommand(h && h.command, binPath, platform),
  );
  if (!kept.length) return { entry: null, stripped: true };
  return { entry: { ...entry, hooks: kept }, stripped: true };
}

/**
 * True when a Stop (or similar) event entry hangs `astrolabe hooks cleanup`.
 * ZCode fires Stop at every turn end — cleanup there would wipe counters each turn.
 */
function entryHasCleanup(entry, binPath = null, platform = process.platform) {
  if (!entry || typeof entry !== 'object') return false;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  return hooks.some((h) => isAstrolabeCleanupCommand(h && h.command, binPath, platform));
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
  {
    create = true,
    withHooks = false,
    writeMcp = true,
    existsFn = fs.existsSync,
    binPath = null,
    platform = process.platform,
  } = {},
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

  let hadServer = false;
  if (writeMcp) {
    if (!doc.mcp || typeof doc.mcp !== 'object' || Array.isArray(doc.mcp)) {
      doc.mcp = {};
    }
    if (!doc.mcp.servers || typeof doc.mcp.servers !== 'object' || Array.isArray(doc.mcp.servers)) {
      doc.mcp.servers = {};
    }
    hadServer = Object.prototype.hasOwnProperty.call(doc.mcp.servers, 'astrolabe');
    doc.mcp.servers.astrolabe = zcodeServerEntry(launch);
  }

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
        const { entry: next, stripped } = scrubStopEntry(entry, binPath, platform);
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
    // Match any prior remind (wrong --client=claude-code, quoted, or .exe) and replace/dedupe.
    const next = preToolUseEntry(binPath, platform);
    let inserted = false;
    const nextList = [];
    for (const entry of doc.hooks.events.PreToolUse) {
      if (entryIsSingleAstrolabeRemind(entry, binPath, platform)) {
        if (!inserted) {
          nextList.push(next);
          inserted = true;
        }
        hooksReplaced = true;
        continue;
      }
      if (entry && typeof entry === 'object' && Array.isArray(entry.hooks)) {
        const filtered = entry.hooks.filter(
          (h) => !isAstrolabeRemindCommand(h && h.command, binPath, platform),
        );
        if (filtered.length !== entry.hooks.length) {
          hooksReplaced = true;
          if (filtered.length > 0) {
            nextList.push({ ...entry, hooks: filtered });
          }
          continue;
        }
      }
      nextList.push(entry);
    }
    if (!inserted) {
      nextList.push(next);
    }
    doc.hooks.events.PreToolUse = nextList;
  }

  return {
    filePath,
    doc,
    created,
    replaced: hadServer || hooksReplaced,
    entry: writeMcp && doc.mcp && doc.mcp.servers ? doc.mcp.servers.astrolabe : undefined,
    withHooks,
  };
}

module.exports = {
  REMIND_ARGS,
  REMIND_CMD,
  resolveZcodeConfigDir,
  detectZCode,
  resolveZcodeConfigPath,
  zcodeServerEntry,
  mergeZcodeConfigFile,
};
