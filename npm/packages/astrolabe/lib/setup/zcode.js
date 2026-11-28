'use strict';

const fs = require('fs');
const path = require('path');
const {
  expandHome,
  readEnvDir,
  isAstrolabeHookCommand,
} = require('./cursor');

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

/**
 * Merge mcp.servers.astrolabe into ~/.zcode/cli/config.json and always strip
 * any legacy Astrolabe hooks from doc.hooks.events. Never installs hooks.
 */
function mergeZcodeConfigFile(
  filePath,
  launch,
  {
    create = true,
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
  } else if (!create || !writeMcp) {
    return { skipped: true, reason: `missing ${filePath}` };
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

  let hooksRemoved = false;
  if (doc.hooks && typeof doc.hooks === 'object' && !Array.isArray(doc.hooks)) {
    if (
      doc.hooks.events &&
      typeof doc.hooks.events === 'object' &&
      !Array.isArray(doc.hooks.events)
    ) {
      for (const [eventName, entries] of Object.entries(doc.hooks.events)) {
        if (!Array.isArray(entries)) continue;
        const nextEntries = [];
        let eventChanged = false;
        for (const entry of entries) {
          if (!entry || typeof entry !== 'object') {
            nextEntries.push(entry);
            continue;
          }
          if (Array.isArray(entry.hooks)) {
            const kept = entry.hooks.filter(
              (h) => !(h && typeof h === 'object' && isAstrolabeHookCommand(h.command, { binPath, platform })),
            );
            if (kept.length !== entry.hooks.length) {
              eventChanged = true;
              hooksRemoved = true;
              if (kept.length > 0) {
                nextEntries.push({ ...entry, hooks: kept });
              }
              continue;
            }
          } else if (
            typeof entry.command === 'string' &&
            isAstrolabeHookCommand(entry.command, { binPath, platform })
          ) {
            eventChanged = true;
            hooksRemoved = true;
            continue;
          }
          nextEntries.push(entry);
        }
        if (eventChanged) {
          if (nextEntries.length > 0) {
            doc.hooks.events[eventName] = nextEntries;
          } else {
            delete doc.hooks.events[eventName];
          }
        }
      }
      if (hooksRemoved && Object.keys(doc.hooks.events).length === 0) {
        delete doc.hooks.events;
      }
    }
    if (
      hooksRemoved &&
      !doc.hooks.events &&
      (doc.hooks.enabled === undefined || doc.hooks.enabled === true) &&
      Object.keys(doc.hooks).filter((k) => k !== 'enabled').length === 0
    ) {
      delete doc.hooks;
    }
  }

  if (!writeMcp && !hooksRemoved) {
    return { skipped: true, reason: `no legacy Astrolabe hooks in ${filePath}` };
  }

  return {
    filePath,
    doc,
    created,
    replaced: hadServer || hooksRemoved,
    hooksRemoved,
    entry: writeMcp && doc.mcp && doc.mcp.servers ? doc.mcp.servers.astrolabe : undefined,
  };
}

module.exports = {
  resolveZcodeConfigDir,
  detectZCode,
  resolveZcodeConfigPath,
  zcodeServerEntry,
  mergeZcodeConfigFile,
};
