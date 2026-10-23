'use strict';

const fs = require('fs');
const path = require('path');

const MANAGED_BEGIN = '# >>> astrolabe-managed-dsh-guard >>>';
const MANAGED_END = '# <<< astrolabe-managed-dsh-guard <<<';

const MANAGED_BLOCK = `${MANAGED_BEGIN}
- insert:
    - id: astrolabe-guard
      name: 'astrolabe-dsh-guard'
      config:
        client: claude-code
${MANAGED_END}
`;

function resolveDshPluginSource(existsFn, startDir = __dirname) {
  const vendor = path.join(startDir, 'vendor', 'astrolabe-dsh-guard');
  if (existsFn(path.join(vendor, 'package.json'))) return vendor;

  let dir = startDir;
  for (let i = 0; i < 10; i += 1) {
    const cand = path.join(dir, 'dsh-plugin');
    if (existsFn(path.join(cand, 'package.json'))) return cand;
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

/**
 * Prefer hoisted ~/.dsh/profiles/node_modules (seen on macOS installs), else
 * each profiles/<name>/node_modules, else create the hoisted path.
 */
function resolveDshPluginInstallDirs(home, existsFn) {
  const profiles = path.join(home, '.dsh', 'profiles');
  const dirs = [];
  const hoisted = path.join(profiles, 'node_modules');
  if (existsFn(hoisted) || existsFn(profiles)) {
    dirs.push(path.join(hoisted, 'astrolabe-dsh-guard'));
  }
  if (existsFn(profiles)) {
    let entries = [];
    try {
      entries = fs.readdirSync(profiles, { withFileTypes: true });
    } catch {
      entries = [];
    }
    for (const ent of entries) {
      if (!ent.isDirectory()) continue;
      if (ent.name === 'node_modules') continue;
      const nm = path.join(profiles, ent.name, 'node_modules');
      // Only install into a profile that already has node_modules or a package.json
      // (active profile), so we do not invent empty profile trees.
      if (existsFn(nm) || existsFn(path.join(profiles, ent.name, 'package.json'))) {
        const target = path.join(nm, 'astrolabe-dsh-guard');
        if (!dirs.includes(target)) dirs.push(target);
      }
    }
  }
  if (!dirs.length) {
    dirs.push(path.join(home, '.dsh', 'profiles', 'node_modules', 'astrolabe-dsh-guard'));
  }
  // Dedupe while preferring hoisted first
  return [...new Set(dirs)];
}

function copyDirRecursive(src, dest) {
  fs.mkdirSync(dest, { recursive: true });
  for (const ent of fs.readdirSync(src, { withFileTypes: true })) {
    const from = path.join(src, ent.name);
    const to = path.join(dest, ent.name);
    if (ent.isDirectory()) copyDirRecursive(from, to);
    else fs.copyFileSync(from, to);
  }
}

/**
 * Merge the managed astrolabe-guard insert into ~/.dsh/cordis.patch.yml without
 * touching the orca-managed-dsh-hooks block (or any other content).
 */
function mergeDshCordisPatch(filePath, { create = true, existsFn = fs.existsSync } = {}) {
  let created = false;
  let text = '';
  if (existsFn(filePath)) {
    text = fs.readFileSync(filePath, 'utf8');
  } else if (!create) {
    return { skipped: true, reason: `missing ${filePath} (will not create)` };
  } else {
    created = true;
    text = '';
  }

  const begin = text.indexOf(MANAGED_BEGIN);
  const end = text.indexOf(MANAGED_END);
  let next;
  let replaced = false;
  if (begin >= 0 && end > begin) {
    replaced = true;
    const afterEnd = end + MANAGED_END.length;
    let tail = text.slice(afterEnd);
    if (tail.startsWith('\n')) tail = tail.slice(1);
    next = `${text.slice(0, begin)}${MANAGED_BLOCK}${tail}`;
  } else if (begin >= 0 || end >= 0) {
    throw new Error(
      `${filePath} has a broken astrolabe-managed-dsh-guard marker; fix or remove it, then re-run setup`,
    );
  } else {
    const trimmed = text.replace(/\s*$/, '');
    next = trimmed ? `${trimmed}\n\n${MANAGED_BLOCK}` : `${MANAGED_BLOCK}`;
  }
  if (!next.endsWith('\n')) next += '\n';
  return { filePath, text: next, created, replaced };
}

function detectDsh(home, env, { whichFn, existsFn }) {
  const evidence = [];
  if (whichFn('dsh', env)) evidence.push('dsh on PATH');
  if (existsFn(path.join(home, '.dsh'))) evidence.push('~/.dsh/');
  if (existsFn(path.join(home, '.dsh', 'cordis.patch.yml'))) {
    evidence.push('~/.dsh/cordis.patch.yml');
  }
  return { detected: evidence.length > 0, evidence };
}

module.exports = {
  MANAGED_BEGIN,
  MANAGED_END,
  MANAGED_BLOCK,
  resolveDshPluginSource,
  resolveDshPluginInstallDirs,
  copyDirRecursive,
  mergeDshCordisPatch,
  detectDsh,
};
