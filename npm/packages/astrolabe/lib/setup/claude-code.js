'use strict';

const fs = require('fs');
const path = require('path');
const {
  expandHome,
  readEnvDir,
  hookCommand,
  isAstrolabeHookCommand: matchAstrolabeHookCommand,
} = require('./cursor');

const SESSION_START_ARGS = ['hooks', 'session-start'];
const REMIND_ARGS = ['hooks', 'remind', '--client=claude-code'];
const CLEANUP_ARGS = ['hooks', 'cleanup'];
const REMIND_MATCHER = 'Grep|Read|Bash|Glob|mcp__astrolabe__.*';

function resolveClaudeConfigDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return readEnvDir(env, ['CLAUDE_CONFIG_DIR'], home);
}

function resolveClaudeMcpPath(home, env = process.env, configDir = null) {
  const customDir = resolveClaudeConfigDir(home, env, configDir);
  return path.join(customDir || home, '.claude.json');
}

function resolveClaudeSettingsPath(home, env = process.env, configDir = null) {
  const customDir = resolveClaudeConfigDir(home, env, configDir);
  if (customDir) return path.join(customDir, 'settings.json');
  return path.join(home, '.claude', 'settings.json');
}

function commandHookEntry(matcher, command) {
  return {
    matcher,
    hooks: [{ type: 'command', command }],
  };
}

function sessionStartEntry(binPath, platform = process.platform) {
  return commandHookEntry('', hookCommand(binPath, SESSION_START_ARGS, platform));
}

function remindEntry(binPath, platform = process.platform) {
  return commandHookEntry(REMIND_MATCHER, hookCommand(binPath, REMIND_ARGS, platform));
}

function cleanupEntry(binPath, platform = process.platform) {
  return commandHookEntry('', hookCommand(binPath, CLEANUP_ARGS, platform));
}

function isAstrolabeHookCommand(command, subcommand, binPath, platform = process.platform) {
  return matchAstrolabeHookCommand(command, {
    subcommands: subcommand ? [subcommand] : ['session-start', 'remind', 'cleanup'],
    binPath,
    platform,
  });
}

function entryIsAstrolabeHook(entry, subcommand, binPath, platform = process.platform) {
  if (!entry || typeof entry !== 'object') return false;
  const hooks = Array.isArray(entry.hooks) ? entry.hooks : [];
  // Never replace a group containing unrelated hooks as a whole.
  if (hooks.length !== 1) return false;
  return isAstrolabeHookCommand(hooks[0] && hooks[0].command, subcommand, binPath, platform);
}

/**
 * Merge Claude Code SessionStart + PreToolUse + SessionEnd into
 * ~/.claude/settings.json without clobbering existing settings or other hooks.
 */
function mergeClaudeCodeHooksSettings(
  filePath,
  { create = true, existsFn = fs.existsSync, binPath = null, platform = process.platform } = {},
) {
  let created = false;
  let doc = {};
  if (existsFn(filePath)) {
    const text = fs.readFileSync(filePath, 'utf8');
    const raw = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
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

  const nextSessionStart = sessionStartEntry(binPath, platform);
  const nextRemind = remindEntry(binPath, platform);
  const nextCleanup = cleanupEntry(binPath, platform);

  for (const [event, subcommand, next] of [
    ['SessionStart', 'session-start', nextSessionStart],
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
          (h) => !isAstrolabeHookCommand(h && h.command, subcommand, binPath, platform),
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

  return {
    filePath,
    doc,
    created,
    replaced,
    entryPreview: JSON.stringify(
      {
        hooks: {
          SessionStart: [nextSessionStart],
          PreToolUse: [nextRemind],
          SessionEnd: [nextCleanup],
        },
      },
      null,
      2,
    ),
  };
}

module.exports = {
  SESSION_START_ARGS,
  REMIND_ARGS,
  CLEANUP_ARGS,
  REMIND_MATCHER,
  resolveClaudeConfigDir,
  resolveClaudeMcpPath,
  resolveClaudeSettingsPath,
  sessionStartEntry,
  remindEntry,
  cleanupEntry,
  isAstrolabeHookCommand,
  mergeClaudeCodeHooksSettings,
};
