'use strict';

const fs = require('fs');
const path = require('path');
const {
  expandHome,
  readEnvDir,
  hookCommand,
  isAstrolabeHookCommand,
} = require('./cursor');

const REMIND_ARGS = ['hooks', 'remind', '--client=codebuddy'];
const CLEANUP_ARGS = ['hooks', 'cleanup'];
const REMIND_CMD = 'astrolabe hooks remind --client=codebuddy';
const CLEANUP_CMD = 'astrolabe hooks cleanup';

function resolveCodebuddyConfigDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return (
    readEnvDir(env, ['CODEBUDDY_CONFIG_DIR', 'CODEBUDDY_HOME'], home) ||
    path.join(home, '.codebuddy')
  );
}

function resolveCodebuddyMcpPath(home, existsFn, env = process.env, configDir = null) {
  const dir = resolveCodebuddyConfigDir(home, env, configDir);
  const hasCustom = Boolean(
    expandHome(configDir, home) ||
      readEnvDir(env, ['CODEBUDDY_CONFIG_DIR', 'CODEBUDDY_HOME'], home),
  );
  const candidates = [path.join(dir, '.mcp.json'), path.join(dir, 'mcp.json')];
  if (!hasCustom) {
    candidates.push(path.join(home, '.codebuddy.json'));
  }
  for (const cand of candidates) {
    if (existsFn(cand)) return cand;
  }
  return candidates[0];
}

function resolveCodebuddySettingsPath(home, env = process.env, configDir = null) {
  return path.join(resolveCodebuddyConfigDir(home, env, configDir), 'settings.json');
}

function detectCodebuddy(home, env, { whichFn, existsFn }) {
  const evidence = [];
  const dir = resolveCodebuddyConfigDir(home, env);
  const hasCustom = Boolean(readEnvDir(env, ['CODEBUDDY_CONFIG_DIR', 'CODEBUDDY_HOME'], home));
  if (whichFn('codebuddy', env)) evidence.push('codebuddy on PATH');
  if (existsFn(dir)) evidence.push(hasCustom ? dir : '~/.codebuddy/');
  if (!hasCustom && existsFn(path.join(home, '.codebuddy.json'))) {
    evidence.push('~/.codebuddy.json');
  }
  return { detected: evidence.length > 0, evidence };
}

function commandHookEntry(command) {
  return {
    matcher: '',
    hooks: [{ type: 'command', command }],
  };
}

function entryIsAstrolabeHook(entry, subcommand, binPath, platform = process.platform) {
  if (!entry || typeof entry !== 'object') return false;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  if (hooks.length !== 1) return false;
  return isAstrolabeHookCommand(hooks[0] && hooks[0].command, {
    subcommands: [subcommand],
    binPath,
    platform,
  });
}

/**
 * Merge CC-compatible PreToolUse + SessionEnd into ~/.codebuddy/settings.json
 * without clobbering other hook entries.
 */
function mergeCodebuddyHooksSettings(
  filePath,
  { create = true, existsFn = fs.existsSync, binPath = null, platform = process.platform } = {},
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

  if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) {
    doc.hooks = {};
  }
  const hooks = doc.hooks;
  let replaced = false;

  const nextRemind = commandHookEntry(hookCommand(binPath, REMIND_ARGS, platform));
  const nextCleanup = commandHookEntry(hookCommand(binPath, CLEANUP_ARGS, platform));

  for (const [event, subcommand, next] of [
    ['PreToolUse', 'remind', nextRemind],
    ['SessionEnd', 'cleanup', nextCleanup],
  ]) {
    if (!Array.isArray(hooks[event])) hooks[event] = [];
    let inserted = false;
    const nextList = [];
    for (const entry of hooks[event]) {
      if (entryIsAstrolabeHook(entry, subcommand, binPath, platform)) {
        if (!inserted) {
          nextList.push(next);
          inserted = true;
        }
        replaced = true;
        continue;
      }
      if (entry && typeof entry === 'object' && Array.isArray(entry.hooks)) {
        const filtered = entry.hooks.filter(
          (h) =>
            !isAstrolabeHookCommand(h && h.command, {
              subcommands: [subcommand],
              binPath,
              platform,
            }),
        );
        if (filtered.length !== entry.hooks.length) {
          replaced = true;
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
    hooks[event] = nextList;
  }

  return { filePath, doc, created, replaced };
}

module.exports = {
  REMIND_ARGS,
  CLEANUP_ARGS,
  REMIND_CMD,
  CLEANUP_CMD,
  resolveCodebuddyConfigDir,
  resolveCodebuddyMcpPath,
  resolveCodebuddySettingsPath,
  detectCodebuddy,
  mergeCodebuddyHooksSettings,
};
