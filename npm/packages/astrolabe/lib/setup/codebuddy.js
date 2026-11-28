'use strict';

const fs = require('fs');
const path = require('path');
const {
  expandHome,
  readEnvDir,
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

function stripCodebuddyHooksSettings(
  filePath,
  { existsFn = fs.existsSync, binPath = null, platform = process.platform } = {},
) {
  if (!existsFn(filePath)) {
    return { skipped: true, reason: `missing ${filePath}` };
  }
  const text = fs.readFileSync(filePath, 'utf8');
  const raw = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
  const doc = JSON.parse(raw);
  if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) {
    throw new Error(`${filePath} root must be a JSON object`);
  }
  if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) {
    return { skipped: true, reason: `no legacy Astrolabe hooks in ${filePath}` };
  }

  let changed = false;
  for (const event of Object.keys(doc.hooks)) {
    const entries = doc.hooks[event];
    if (!Array.isArray(entries)) continue;
    let eventChanged = false;
    const nextList = [];
    for (const entry of entries) {
      if (entry && typeof entry === 'object' && Array.isArray(entry.hooks)) {
        const remaining = entry.hooks.filter(
          (h) =>
            !isAstrolabeHookCommand(typeof h === 'string' ? h : h && h.command, {
              binPath,
              platform,
            }),
        );
        if (remaining.length !== entry.hooks.length) {
          changed = true;
          eventChanged = true;
          if (remaining.length > 0) {
            nextList.push({ ...entry, hooks: remaining });
          }
          continue;
        }
      } else if (
        entry &&
        typeof entry === 'object' &&
        typeof entry.command === 'string' &&
        isAstrolabeHookCommand(entry.command, { binPath, platform })
      ) {
        changed = true;
        eventChanged = true;
        continue;
      }
      nextList.push(entry);
    }
    if (eventChanged) {
      if (nextList.length === 0) {
        delete doc.hooks[event];
      } else {
        doc.hooks[event] = nextList;
      }
    }
  }

  if (!changed) {
    return { skipped: true, reason: `no legacy Astrolabe hooks in ${filePath}` };
  }

  if (Object.keys(doc.hooks).length === 0) {
    delete doc.hooks;
  }

  return { filePath, doc, created: false, replaced: true, removed: true };
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
  stripCodebuddyHooksSettings,
};
