'use strict';

const fs = require('fs');
const path = require('path');
const { parseJsonc, upsertJsoncProperty } = require('./jsonc');
const { expandHome, readEnvDir } = require('./cursor');

function resolveOpenCodeConfigDir(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  const envDir = readEnvDir(env, ['OPENCODE_CONFIG_DIR'], home);
  if (envDir) return envDir;
  const xdgConfig = readEnvDir(env, ['XDG_CONFIG_HOME'], home);
  if (xdgConfig) return path.join(xdgConfig, 'opencode');
  return path.join(home, '.config', 'opencode');
}

function resolveOpenCodePluginPath(home, env = process.env, configDir = null) {
  return path.join(resolveOpenCodeConfigDir(home, env, configDir), 'plugins', 'astrolabe-guard.ts');
}

function resolveOpenCodeConfigPath(home, existsFn, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) {
    const candidates = [
      path.join(explicit, 'opencode.jsonc'),
      path.join(explicit, 'opencode.json'),
      path.join(explicit, 'config.json'),
    ];
    for (const cand of candidates) {
      if (existsFn(cand)) return cand;
    }
    return candidates[0];
  }
  const envConfig = readEnvDir(env, ['OPENCODE_CONFIG'], home);
  if (envConfig) return envConfig;

  const envDir = readEnvDir(env, ['OPENCODE_CONFIG_DIR'], home);
  if (envDir) {
    const candidates = [
      path.join(envDir, 'opencode.jsonc'),
      path.join(envDir, 'opencode.json'),
      path.join(envDir, 'config.json'),
    ];
    for (const cand of candidates) {
      if (existsFn(cand)) return cand;
    }
    return candidates[0];
  }

  const xdgConfig = readEnvDir(env, ['XDG_CONFIG_HOME'], home);
  const primaryDir = xdgConfig ? path.join(xdgConfig, 'opencode') : path.join(home, '.config', 'opencode');
  const candidates = [
    path.join(primaryDir, 'opencode.jsonc'),
    path.join(primaryDir, 'opencode.json'),
    path.join(primaryDir, 'config.json'),
  ];
  if (xdgConfig) {
    candidates.push(
      path.join(home, '.config', 'opencode', 'opencode.jsonc'),
      path.join(home, '.config', 'opencode', 'opencode.json'),
      path.join(home, '.config', 'opencode', 'config.json'),
    );
  }
  candidates.push(
    path.join(home, '.opencode', 'opencode.jsonc'),
    path.join(home, '.opencode', 'opencode.json'),
    path.join(home, '.opencode', 'config.json'),
  );
  for (const cand of candidates) {
    if (existsFn(cand)) return cand;
  }
  // Default: new-style user config (JSONC).
  return path.join(primaryDir, 'opencode.jsonc');
}

/**
 * @returns {'v1'|'v2'|'mcpServers'}
 */
function detectOpenCodeMcpFormat(doc) {
  if (!doc || typeof doc !== 'object' || Array.isArray(doc)) return 'v1';
  if (doc.mcpServers && typeof doc.mcpServers === 'object' && !Array.isArray(doc.mcpServers)) {
    return 'mcpServers';
  }
  if (doc.mcp && typeof doc.mcp === 'object' && !Array.isArray(doc.mcp)) {
    if (doc.mcp.servers && typeof doc.mcp.servers === 'object' && !Array.isArray(doc.mcp.servers)) {
      return 'v2';
    }
    return 'v1';
  }
  // New file: prefer V1 (OpenCode 1.x / docs.opencode.ai classic shape).
  return 'v1';
}

function openCodeServerEntry(launch, format) {
  const command = [launch.command, ...launch.args];
  if (format === 'mcpServers') {
    return { command: launch.command, args: launch.args };
  }
  if (format === 'v2') {
    return { type: 'local', command };
  }
  // v1
  return { type: 'local', command, enabled: true };
}

/**
 * Merge Astrolabe into an OpenCode config file, preserving JSONC comments when
 * the parent `mcp` / `mcp.servers` object already exists.
 */
function mergeOpenCodeMcpFile(filePath, launch, { create = true, existsFn = fs.existsSync } = {}) {
  let created = false;
  let raw = '';
  let format = 'v1';
  if (existsFn(filePath)) {
    raw = fs.readFileSync(filePath, 'utf8');
    try {
      format = detectOpenCodeMcpFormat(parseJsonc(raw));
    } catch (err) {
      throw new Error(`${filePath} is not valid JSON/JSONC: ${err.message}`);
    }
  } else if (!create) {
    return { skipped: true, reason: `missing ${filePath} (will not create)` };
  } else {
    created = true;
    raw = '{\n  "$schema": "https://opencode.ai/config.json"\n}\n';
  }

  const entry = openCodeServerEntry(launch, format);
  let parentPath;
  if (format === 'mcpServers') parentPath = ['mcpServers'];
  else if (format === 'v2') parentPath = ['mcp', 'servers'];
  else parentPath = ['mcp'];

  const result = upsertJsoncProperty(raw, parentPath, 'astrolabe', entry, { indent: 2 });
  return {
    filePath,
    text: result.text,
    created,
    replaced: result.replaced,
    format,
    entry,
    commentsPreserved: result.commentsPreserved !== false,
    write: null, // filled by caller with writeTextAtomic
  };
}

function detectOpenCode(home, env, { whichFn, existsFn }) {
  const evidence = [];
  if (whichFn('opencode', env)) evidence.push('opencode on PATH');
  const envConfig = readEnvDir(env, ['OPENCODE_CONFIG'], home);
  if (envConfig && existsFn(envConfig)) evidence.push(envConfig);
  const envDir = readEnvDir(env, ['OPENCODE_CONFIG_DIR'], home);
  if (envDir && existsFn(envDir)) evidence.push(envDir);
  const xdgConfig = readEnvDir(env, ['XDG_CONFIG_HOME'], home);
  if (xdgConfig && existsFn(path.join(xdgConfig, 'opencode'))) {
    evidence.push(path.join(xdgConfig, 'opencode'));
  }
  if (existsFn(path.join(home, '.config', 'opencode'))) evidence.push('~/.config/opencode/');
  if (existsFn(path.join(home, '.opencode'))) evidence.push('~/.opencode/');
  return { detected: evidence.length > 0, evidence };
}

function stripOpenCodeGuardPluginFile(filePath, { existsFn = fs.existsSync } = {}) {
  if (!existsFn(filePath)) {
    return { skipped: true, reason: `missing ${filePath}` };
  }
  const content = fs.readFileSync(filePath, 'utf8');
  const isAstrolabePlugin =
    content.includes('AstrolabeGuard') ||
    (content.includes('hooks') && content.includes('opencode') && /astrolabe/i.test(content));
  if (!isAstrolabePlugin) {
    return { skipped: true, reason: `${filePath} does not look like an Astrolabe guard plugin` };
  }
  return { filePath, deleted: true, created: false, replaced: true };
}

function stripOpenCodeGuardPlugin(
  home,
  existsFn = fs.existsSync,
  { pluginFilePath = null, env = process.env, configDir = null } = {},
) {
  const resolveExists = existsFn || fs.existsSync;
  const filePath = pluginFilePath
    ? expandHome(pluginFilePath, home)
    : resolveOpenCodePluginPath(home, env, configDir);
  return stripOpenCodeGuardPluginFile(filePath, { existsFn: resolveExists });
}

module.exports = {
  resolveOpenCodeConfigDir,
  resolveOpenCodePluginPath,
  resolveOpenCodeConfigPath,
  detectOpenCodeMcpFormat,
  openCodeServerEntry,
  mergeOpenCodeMcpFile,
  detectOpenCode,
  stripOpenCodeGuardPluginFile,
  stripOpenCodeGuardPlugin,
};
