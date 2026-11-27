'use strict';

const fs = require('fs');
const path = require('path');
const { expandHome, readEnvDir, hookCommand } = require('./cursor');

const REMIND_ARGS = ['hooks', 'remind', '--client=grok'];
const CLEANUP_ARGS = ['hooks', 'cleanup'];

function buildGrokHooksDoc(binPath = null, platform = process.platform) {
  return {
    hooks: {
      PreToolUse: [
        {
          matcher: 'Bash|Read|Grep|run_terminal_command|read_file|grep|mcp__astrolabe__.*',
          hooks: [
            {
              type: 'command',
              command: hookCommand(binPath, REMIND_ARGS, platform),
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
              command: hookCommand(binPath, CLEANUP_ARGS, platform),
              timeout: 10,
            },
          ],
        },
      ],
    },
  };
}

const GROK_HOOKS_DOC = buildGrokHooksDoc();

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
 * Write/replace the dedicated Astrolabe hooks file (merge root.hooks keys only
 * when an existing file already has unrelated top-level fields).
 */
function mergeGrokHooksFile(
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

  const hooksDoc = buildGrokHooksDoc(binPath, platform);
  const replaced = Boolean(doc.hooks);
  doc.hooks = hooksDoc.hooks;
  return { filePath, doc, created, replaced, entryPreview: JSON.stringify(hooksDoc, null, 2) };
}

module.exports = {
  GROK_HOOKS_DOC,
  buildGrokHooksDoc,
  resolveGrokConfigDir,
  detectGrok,
  resolveGrokConfigToml,
  resolveGrokHooksPath,
  mergeGrokHooksFile,
};
