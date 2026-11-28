'use strict';

const fs = require('fs');
const path = require('path');
const { expandHome, readEnvDir, isAstrolabeHookCommand } = require('./cursor');

function resolveGrokConfigDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return readEnvDir(env, ['GROK_CONFIG_DIR', 'GROK_HOME'], home) || path.join(home, '.grok');
}

function detectGrok(home, env, { whichFn, existsFn }) {
  const evidence = [];
  const dir = resolveGrokConfigDir(home, env);
  const hasCustom = Boolean(readEnvDir(env, ['GROK_CONFIG_DIR', 'GROK_HOME'], home));
  if (whichFn('grok', env)) evidence.push('grok on PATH');
  if (existsFn(dir)) evidence.push(hasCustom ? dir : '~/.grok/');
  if (existsFn(path.join(dir, 'config.toml'))) {
    evidence.push(hasCustom ? path.join(dir, 'config.toml') : '~/.grok/config.toml');
  }
  return { detected: evidence.length > 0, evidence };
}

function resolveGrokConfigToml(home, env = process.env, configDir = null) {
  return path.join(resolveGrokConfigDir(home, env, configDir), 'config.toml');
}

function resolveGrokHooksPath(home, env = process.env, configDir = null) {
  return path.join(resolveGrokConfigDir(home, env, configDir), 'hooks', 'astrolabe.json');
}

/**
 * Strip legacy Astrolabe hook commands from a Grok hooks JSON file while
 * preserving unrelated third-party hooks and top-level fields.
 */
function stripGrokHooksFile(
  filePath,
  { existsFn = fs.existsSync, binPath = null, platform = process.platform } = {},
) {
  if (!existsFn(filePath)) {
    return { skipped: true, reason: `missing ${filePath}` };
  }
  const raw = fs.readFileSync(filePath, 'utf8').replace(/^\uFEFF/, '');
  const doc = JSON.parse(raw);
  if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) {
    throw new Error(`${filePath} root must be a JSON object`);
  }
  if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) {
    return { skipped: true, reason: `no legacy Astrolabe hooks in ${filePath}` };
  }

  let changed = false;
  for (const [eventName, entries] of Object.entries(doc.hooks)) {
    if (!Array.isArray(entries)) continue;
    const nextEntries = [];
    let eventChanged = false;
    for (const entry of entries) {
      if (!entry || typeof entry !== 'object') {
        nextEntries.push(entry);
        continue;
      }
      if (Array.isArray(entry.hooks)) {
        const remaining = entry.hooks.filter(
          (h) => !(h && typeof h === 'object' && isAstrolabeHookCommand(h.command, { binPath, platform })),
        );
        if (remaining.length !== entry.hooks.length) {
          eventChanged = true;
          changed = true;
          if (remaining.length > 0) {
            nextEntries.push({ ...entry, hooks: remaining });
          }
          continue;
        }
      } else if (
        typeof entry.command === 'string' &&
        isAstrolabeHookCommand(entry.command, { binPath, platform })
      ) {
        eventChanged = true;
        changed = true;
        continue;
      }
      nextEntries.push(entry);
    }
    if (eventChanged) {
      if (nextEntries.length > 0) {
        doc.hooks[eventName] = nextEntries;
      } else {
        delete doc.hooks[eventName];
      }
    }
  }

  if (changed && Object.keys(doc.hooks).length === 0) {
    delete doc.hooks;
  }

  if (!changed) {
    return { skipped: true, reason: `no legacy Astrolabe hooks in ${filePath}` };
  }

  if (Object.keys(doc).length === 0) {
    return { filePath, deleted: true, created: false, replaced: true, doc: null };
  }

  return { filePath, doc, deleted: false, created: false, replaced: true };
}

module.exports = {
  resolveGrokConfigDir,
  detectGrok,
  resolveGrokConfigToml,
  resolveGrokHooksPath,
  stripGrokHooksFile,
};
