import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

import {
  CATALOG_TOOL_NAMES,
  CLI_ALLOW_ENTRIES,
  IDE_ALLOW_ENTRIES,
} from './setup-cursor-permissions.mjs';

const SCRIPT = fileURLToPath(new URL('./setup-cursor-permissions.mjs', import.meta.url));

function tmpHome() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'astrolabe-cursor-perm-'));
}

function run(home, extraArgs = []) {
  return spawnSync(process.execPath, [SCRIPT, '--home', home, ...extraArgs], {
    encoding: 'utf8',
  });
}

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

test('catalog names are the seven reserved tools', () => {
  assert.equal(CATALOG_TOOL_NAMES.length, 7);
  assert.deepEqual(new Set(CATALOG_TOOL_NAMES), new Set([
    'search_code',
    'find_symbol',
    'resolve_context',
    'get_diagnostics',
    'find_references',
    'goto_definition',
    'get_languages',
  ]));
  assert.equal(new Set(CATALOG_TOOL_NAMES).size, 7);
});

test('creates permissions.json when missing and --yes is given', () => {
  const home = tmpHome();
  try {
    const result = run(home, ['--yes']);
    assert.equal(result.status, 0, result.stderr);
    const filePath = path.join(home, '.cursor', 'permissions.json');
    assert.equal(fs.existsSync(filePath), true);
    const doc = readJson(filePath);
    assert.deepEqual(doc, { mcpAllowlist: IDE_ALLOW_ENTRIES });
    assert.match(result.stdout, /整表覆盖/);
    assert.match(result.stdout, /Run Mode/);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'cli-config.json')), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('second merge keeps old allowlist entries and does not touch terminalAllowlist', () => {
  const home = tmpHome();
  try {
    const filePath = path.join(home, '.cursor', 'permissions.json');
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    const original = {
      mcpAllowlist: ['other:foo', 'astrolabe:search_code'],
      terminalAllowlist: ['ls', 'git status'],
      autoRun: true,
    };
    fs.writeFileSync(filePath, `${JSON.stringify(original, null, 2)}\n`);

    const first = run(home);
    assert.equal(first.status, 0, first.stderr);
    const afterFirst = readJson(filePath);
    assert.deepEqual(afterFirst.terminalAllowlist, ['ls', 'git status']);
    assert.equal(afterFirst.autoRun, true);
    assert.equal(afterFirst.mcpAllowlist[0], 'other:foo');
    assert.equal(afterFirst.mcpAllowlist[1], 'astrolabe:search_code');
    for (const entry of IDE_ALLOW_ENTRIES) {
      assert.equal(afterFirst.mcpAllowlist.includes(entry), true, `missing ${entry}`);
    }
    assert.equal(
      afterFirst.mcpAllowlist.filter((item) => item === 'astrolabe:search_code').length,
      1,
    );

    const second = run(home);
    assert.equal(second.status, 0, second.stderr);
    const afterSecond = readJson(filePath);
    assert.deepEqual(afterSecond, afterFirst);
    assert.match(second.stdout, /新增 0 条/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('--dry-run prints the plan and does not write', () => {
  const home = tmpHome();
  try {
    const result = run(home, ['--dry-run']);
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /\[dry-run\]/);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'permissions.json')), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('invalid JSON exits 1 and leaves the file unchanged', () => {
  const home = tmpHome();
  try {
    const filePath = path.join(home, '.cursor', 'permissions.json');
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    const raw = '{ // comment\n  "mcpAllowlist": []\n}\n';
    fs.writeFileSync(filePath, raw);
    const result = run(home);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /标准 JSON/);
    assert.equal(fs.readFileSync(filePath, 'utf8'), raw);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('--cli skips missing cli-config.json and does not create it', () => {
  const home = tmpHome();
  try {
    const result = run(home, ['--cli', '--yes']);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'permissions.json')), true);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'cli-config.json')), false);
    assert.match(result.stdout, /已跳过 CLI allow 合并/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('--cli merges Mcp(astrolabe:<tool>) without changing approvalMode', () => {
  const home = tmpHome();
  try {
    const cliPath = path.join(home, '.cursor', 'cli-config.json');
    fs.mkdirSync(path.dirname(cliPath), { recursive: true });
    const original = {
      version: 1,
      approvalMode: 'allowlist',
      permissions: {
        allow: ['Shell(ls)'],
        deny: ['Shell(rm)'],
      },
      editor: { vimMode: false },
    };
    fs.writeFileSync(cliPath, `${JSON.stringify(original, null, 2)}\n`);

    const result = run(home, ['--cli', '--yes']);
    assert.equal(result.status, 0, result.stderr);
    const doc = readJson(cliPath);
    assert.equal(doc.approvalMode, 'allowlist');
    assert.equal(doc.version, 1);
    assert.deepEqual(doc.editor, { vimMode: false });
    assert.deepEqual(doc.permissions.deny, ['Shell(rm)']);
    assert.equal(doc.permissions.allow[0], 'Shell(ls)');
    for (const entry of CLI_ALLOW_ENTRIES) {
      assert.equal(doc.permissions.allow.includes(entry), true, `missing ${entry}`);
    }
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('missing permissions.json is not created without --yes (non-interactive)', () => {
  const home = tmpHome();
  try {
    const result = run(home);
    assert.equal(result.status, 1);
    assert.match(result.stdout, /整表覆盖/);
    assert.match(result.stderr, /--yes/);
    assert.equal(fs.existsSync(path.join(home, '.cursor', 'permissions.json')), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('existing file is backed up, keeps mode, and warning is printed before the write', () => {
  const home = tmpHome();
  try {
    const filePath = path.join(home, '.cursor', 'permissions.json');
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
    const original = `${JSON.stringify({ mcpAllowlist: ['other:foo'] }, null, 2)}\n`;
    fs.writeFileSync(filePath, original);
    fs.chmodSync(filePath, 0o640);
    const result = run(home);
    assert.equal(result.status, 0, result.stderr);
    const baks = fs.readdirSync(path.dirname(filePath)).filter((n) => n.includes('.bak-'));
    assert.equal(baks.length, 1);
    assert.equal(fs.readFileSync(path.join(path.dirname(filePath), baks[0]), 'utf8'), original);
    if (process.platform !== 'win32') {
      assert.equal(fs.statSync(filePath).mode & 0o777, 0o640);
    }
    assert.ok(result.stdout.indexOf('整表覆盖') < result.stdout.indexOf('已合并'));
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('symlinked permissions.json stays a symlink and the target is updated', { skip: process.platform === 'win32' }, () => {
  const home = tmpHome();
  try {
    const dotfiles = path.join(home, 'dotfiles');
    fs.mkdirSync(dotfiles, { recursive: true });
    const target = path.join(dotfiles, 'permissions.json');
    fs.writeFileSync(target, `${JSON.stringify({ mcpAllowlist: ['other:foo'] })}\n`);
    const linkPath = path.join(home, '.cursor', 'permissions.json');
    fs.mkdirSync(path.dirname(linkPath), { recursive: true });
    fs.symlinkSync(target, linkPath);
    const result = run(home);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(fs.lstatSync(linkPath).isSymbolicLink(), true);
    const doc = readJson(target);
    assert.equal(doc.mcpAllowlist[0], 'other:foo');
    assert.equal(doc.mcpAllowlist.includes('astrolabe:search_code'), true);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
