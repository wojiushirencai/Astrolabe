'use strict';

const fs = require('fs');
const path = require('path');
const { expandHome, readEnvDir } = require('./cursor');

const MANAGED_BEGIN = '# >>> astrolabe-managed-dsh-guard >>>';
const MANAGED_END = '# <<< astrolabe-managed-dsh-guard <<<';

function resolveDshHome(home, env = process.env, configDir = null) {
  const explicit = expandHome(configDir, home);
  if (explicit) return explicit;
  return readEnvDir(env, ['DSH_HOME'], home) || path.join(home, '.dsh');
}

function resolveDshPatchPath(home, env = process.env, configDir = null) {
  return path.join(resolveDshHome(home, env, configDir), 'cordis.patch.yml');
}

function statIsDir(dirPath) {
  try {
    return fs.statSync(dirPath).isDirectory();
  } catch {
    return false;
  }
}

function pathExistsOrSymlink(filePath, existsFn = fs.existsSync) {
  if (existsFn(filePath)) return true;
  try {
    fs.lstatSync(filePath);
    return true;
  } catch {
    return false;
  }
}

/**
 * Return existing astrolabe-dsh-guard install directories only (never invents
 * non-existent directories).
 */
function findExistingDshPluginInstallDirs(
  home,
  existsFn = fs.existsSync,
  env = process.env,
  configDir = null,
) {
  const dshHome = resolveDshHome(home, env, configDir);
  const profiles = path.join(dshHome, 'profiles');
  const dirs = [];
  const hoistedTarget = path.join(profiles, 'node_modules', 'astrolabe-dsh-guard');
  if (pathExistsOrSymlink(hoistedTarget, existsFn)) {
    dirs.push(hoistedTarget);
  }
  if (existsFn(profiles)) {
    let entries = [];
    try {
      entries = fs.readdirSync(profiles, { withFileTypes: true });
    } catch {
      entries = [];
    }
    for (const ent of entries) {
      if (
        !ent.isDirectory() &&
        !(ent.isSymbolicLink() && statIsDir(path.join(profiles, ent.name)))
      ) {
        continue;
      }
      if (ent.name === 'node_modules') continue;
      const target = path.join(profiles, ent.name, 'node_modules', 'astrolabe-dsh-guard');
      if (pathExistsOrSymlink(target, existsFn) && !dirs.includes(target)) {
        dirs.push(target);
      }
    }
  }
  return dirs;
}

/**
 * Strip the astrolabe-managed-dsh-guard block from ~/.dsh/cordis.patch.yml
 * while 100% preserving orca-managed-dsh-hooks and all other content.
 */
function stripDshCordisPatch(filePath, { existsFn = fs.existsSync } = {}) {
  if (!existsFn(filePath)) {
    return { skipped: true, reason: `missing ${filePath}` };
  }
  const text = fs.readFileSync(filePath, 'utf8');
  const begin = text.indexOf(MANAGED_BEGIN);
  const end = text.indexOf(MANAGED_END);
  if (begin < 0 && end < 0) {
    return { skipped: true, reason: `no astrolabe-managed-dsh-guard block in ${filePath}` };
  }
  if (begin < 0 || end < 0 || end < begin) {
    throw new Error(
      `${filePath} has a broken astrolabe-managed-dsh-guard marker; fix or remove it, then re-run setup`,
    );
  }

  const before = text.slice(0, begin).replace(/\s+$/, '');
  let after = text.slice(end + MANAGED_END.length).replace(/^(?:[ \t]*\r?\n)+/, '');
  if (after.trim() === '') after = '';
  let next;
  if (before && after) {
    next = `${before}\n\n${after}`;
    if (!next.endsWith('\n')) next += '\n';
  } else if (before) {
    next = `${before}\n`;
  } else if (after) {
    next = after.endsWith('\n') ? after : `${after}\n`;
  } else {
    next = '';
  }

  return { filePath, text: next, created: false, replaced: true, removed: true };
}

function detectDsh(home, env = process.env, { existsFn = fs.existsSync } = {}) {
  const evidence = [];
  const dshHome = resolveDshHome(home, env);
  const hasCustom = Boolean(readEnvDir(env, ['DSH_HOME'], home));
  const patchPath = resolveDshPatchPath(home, env);
  if (existsFn(patchPath)) {
    try {
      const text = fs.readFileSync(patchPath, 'utf8');
      if (
        text.includes(MANAGED_BEGIN) ||
        text.includes(MANAGED_END) ||
        text.includes('astrolabe-managed-dsh-guard')
      ) {
        evidence.push(hasCustom ? patchPath : '~/.dsh/cordis.patch.yml');
      }
    } catch {
      // Ignore unreadable patch file
    }
  }
  for (const dir of findExistingDshPluginInstallDirs(home, existsFn, env)) {
    evidence.push(dir);
  }
  return { detected: evidence.length > 0, evidence };
}

module.exports = {
  MANAGED_BEGIN,
  MANAGED_END,
  resolveDshHome,
  resolveDshPatchPath,
  findExistingDshPluginInstallDirs,
  detectDsh,
  stripDshCordisPatch,
};
