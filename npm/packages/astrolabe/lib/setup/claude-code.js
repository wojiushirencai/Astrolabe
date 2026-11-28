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
  const subcommands = Array.isArray(subcommand)
    ? subcommand
    : subcommand
      ? [subcommand]
      : ['session-start', 'remind', 'cleanup', 'gc'];
  return matchAstrolabeHookCommand(command, {
    subcommands,
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

function isStatFile(filePath) {
  try {
    return fs.statSync(filePath).isFile();
  } catch {
    return false;
  }
}

function discoverClaudeSettingsPaths(
  home,
  { existsFn = fs.existsSync, env = process.env, configDir = null } = {},
) {
  const primary = resolveClaudeSettingsPath(home, env, configDir);
  const dir = path.dirname(primary);
  const discovered = [];
  if (existsFn(dir)) {
    try {
      const entries = fs.readdirSync(dir, { withFileTypes: true });
      const names = entries
        .filter(
          (entry) =>
            /^settings-.*\.json$/i.test(entry.name) &&
            (entry.isFile() ||
              (entry.isSymbolicLink() && isStatFile(path.join(dir, entry.name)))),
        )
        .map((entry) => entry.name)
        .sort();
      for (const name of names) {
        discovered.push(path.join(dir, name));
      }
    } catch {
      // Ignore unreadable directories during discovery.
    }
  }
  return Array.from(new Set([primary, ...discovered]));
}

function stripClaudeCodeHooksSettings(
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
            !isAstrolabeHookCommand(
              typeof h === 'string' ? h : h && h.command,
              null,
              binPath,
              platform,
            ),
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
        isAstrolabeHookCommand(entry.command, null, binPath, platform)
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

function inspectClaudeProjectHooks(
  projectRoot,
  { existsFn = fs.existsSync, binPath = null, platform = process.platform } = {},
) {
  if (!projectRoot) return [];
  const candidates = [
    path.join(projectRoot, '.claude', 'settings.json'),
    path.join(projectRoot, '.claude', 'settings.local.json'),
  ];
  const findings = [];
  for (const filePath of candidates) {
    if (!existsFn(filePath)) continue;
    let doc;
    try {
      const text = fs.readFileSync(filePath, 'utf8');
      const raw = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
      doc = JSON.parse(raw);
    } catch {
      continue;
    }
    if (!doc || typeof doc !== 'object' || Array.isArray(doc)) continue;
    if (!doc.hooks || typeof doc.hooks !== 'object' || Array.isArray(doc.hooks)) continue;
    const events = [];
    for (const event of Object.keys(doc.hooks)) {
      const entries = doc.hooks[event];
      if (!Array.isArray(entries)) continue;
      const found = entries.some((entry) => {
        if (!entry || typeof entry !== 'object') return false;
        if (
          typeof entry.command === 'string' &&
          isAstrolabeHookCommand(entry.command, null, binPath, platform)
        ) {
          return true;
        }
        if (Array.isArray(entry.hooks)) {
          return entry.hooks.some((h) =>
            isAstrolabeHookCommand(
              typeof h === 'string' ? h : h && h.command,
              null,
              binPath,
              platform,
            ),
          );
        }
        return false;
      });
      if (found) events.push(event);
    }
    if (events.length > 0) {
      findings.push({ filePath, events });
    }
  }
  return findings;
}

module.exports = {
  SESSION_START_ARGS,
  REMIND_ARGS,
  CLEANUP_ARGS,
  REMIND_MATCHER,
  resolveClaudeConfigDir,
  resolveClaudeMcpPath,
  resolveClaudeSettingsPath,
  discoverClaudeSettingsPaths,
  sessionStartEntry,
  remindEntry,
  cleanupEntry,
  isAstrolabeHookCommand,
  mergeClaudeCodeHooksSettings,
  stripClaudeCodeHooksSettings,
  inspectClaudeProjectHooks,
};
