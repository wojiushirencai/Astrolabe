'use strict';

const path = require('path');
const os = require('os');
const clientsMod = require('./clients');
const {
  CLIENTS,
  getClient,
  canonicalClientId,
  resolveLaunch,
  mergeCursorPermissions,
  checkCursorAllowlist,
  backupFile,
  writeJsonAtomic,
} = clientsMod;
const { parseArgs, USAGE } = require('./args');
const { isTTY, confirm, selectClients } = require('./prompt');

function resolveHome(homeFlag) {
  if (homeFlag) return path.resolve(homeFlag);
  if (process.env.HOME) return process.env.HOME;
  if (process.env.USERPROFILE) return process.env.USERPROFILE;
  return os.homedir();
}

function detectAll(home) {
  return CLIENTS.map((client) => {
    const { detected, evidence } = client.detect(home);
    return {
      id: client.id,
      label: client.label,
      context: client.context,
      detected,
      evidence,
    };
  });
}

function printList(rows) {
  for (const row of rows) {
    const status = row.detected ? 'detected' : 'missing';
    const ev = row.evidence.length ? ` (${row.evidence.join('; ')})` : '';
    process.stdout.write(`${row.id.padEnd(18)} ${status}${ev}\n`);
  }
}

function chooseIds(rows, opts) {
  if (opts.clients && opts.clients.length) {
    const unknown = opts.clients.filter((id) => !getClient(id));
    if (unknown.length) {
      throw new Error(`unknown client id(s): ${unknown.join(', ')}`);
    }
    // Deduplicate aliases onto canonical ids (e.g. dsh → deepseek-harness).
    return [...new Set(opts.clients.map((id) => canonicalClientId(id)))];
  }
  if (opts.yes) {
    const detected = rows.filter((r) => r.detected).map((r) => r.id);
    if (!detected.length) {
      throw new Error('no clients detected; pass --clients=... or run interactively');
    }
    return detected;
  }
  if (!isTTY()) {
    throw new Error(
      'stdin is not a TTY; pass --yes (all detected) or --clients=claude-code,cursor,...',
    );
  }
  return selectClients(rows);
}

function formatEntryPreview(planResult) {
  if (planResult.entryPreview) return String(planResult.entryPreview).trimEnd();
  if (planResult.entry) {
    return JSON.stringify({ astrolabe: planResult.entry }, null, 2);
  }
  if (planResult.doc && planResult.doc.mcpServers) {
    return JSON.stringify(
      { mcpServers: { astrolabe: planResult.doc.mcpServers.astrolabe } },
      null,
      2,
    );
  }
  if (planResult.text) {
    const match = planResult.text.match(/\[mcp_servers\.astrolabe\][\s\S]*?(?=\n\[|$)/);
    if (match) return match[0].trimEnd();
    const managed = planResult.text.match(
      /# >>> astrolabe-managed-dsh-guard >>>[\s\S]*?# <<< astrolabe-managed-dsh-guard <<</,
    );
    if (managed) return managed[0].trimEnd();
    // OpenCode: show a short snippet around "astrolabe"
    const idx = planResult.text.indexOf('"astrolabe"');
    if (idx >= 0) {
      const start = Math.max(0, planResult.text.lastIndexOf('\n', idx) - 80);
      return planResult.text.slice(start, Math.min(planResult.text.length, idx + 220)).trimEnd();
    }
    return planResult.text.trimEnd();
  }
  return '';
}

function resolveCommandBinary(command, env = process.env) {
  if (!command) return null;
  // Absolute path or an explicit relative path: resolve against cwd.
  if (path.isAbsolute(command)) return path.resolve(command);
  if (command.includes('/') || command.includes('\\')) return path.resolve(command);
  // Bare command name: resolve through PATH, otherwise `--command=astrolabe`
  // would become <cwd>/astrolabe, which is not absolute and has no separator.
  const found = clientsMod.which(command, env);
  return found ? path.resolve(found) : null;
}

/**
 * Back up every existing target a multi-step plan will rewrite (project/global
 * mcp.json, rules, hooks.json), not just the primary file. New files have
 * nothing to back up.
 */
function backupAppliedFiles(applied) {
  const targets = [
    { filePath: applied.filePath, created: applied.created, skipped: applied.skipped },
    ...(Array.isArray(applied.results) ? applied.results : []).map((step) => ({
      filePath: step.filePath,
      created: step.created,
      skipped: step.skipped,
    })),
  ];
  const seen = new Set();
  const backups = [];
  for (const target of targets) {
    if (!target.filePath || target.skipped || target.created || seen.has(target.filePath)) {
      continue;
    }
    seen.add(target.filePath);
    const backup = backupFile(target.filePath);
    if (backup) backups.push(backup);
  }
  return backups;
}

function run(argv = process.argv.slice(2)) {
  return runAsync(argv);
}

async function runAsync(argv = process.argv.slice(2)) {
  const opts = parseArgs(argv);
  if (opts.help) {
    process.stdout.write(USAGE);
    return 0;
  }

  const home = resolveHome(opts.home);
  const rows = detectAll(home);
  const detectedById = new Map(rows.map((row) => [row.id, row.detected]));

  if (opts.list) {
    printList(rows);
    return 0;
  }

  let selectedIds;
  try {
    selectedIds = chooseIds(rows, opts);
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    return 1;
  }
  if (!selectedIds.length) {
    process.stdout.write('Nothing selected; no files changed.\n');
    return 0;
  }

  const cursorSelected = selectedIds.includes('cursor');
  const cursorDetected = cursorSelected && detectedById.get('cursor');
  let projectRoot = null;
  let cursorBinPath = null;
  let cursorBinError = null;
  let pyrightPath = null;
  if (cursorDetected) {
    projectRoot = clientsMod.resolveCursorProjectRoot(
      opts.project ? path.resolve(opts.project) : process.cwd(),
    );
    pyrightPath = clientsMod.detectPyright(process.env, {
      whichFn: clientsMod.which,
      home,
    });
    if (opts.command) {
      cursorBinPath = resolveCommandBinary(opts.command, process.env);
      if (!cursorBinPath) {
        cursorBinError =
          `--command ${opts.command} not found (absolute path or PATH lookup failed)`;
      }
    } else if (opts.native) {
      // --native promises "astrolabe on PATH"; prefer that over downloading.
      cursorBinPath = resolveCommandBinary('astrolabe', process.env);
      if (!cursorBinPath) {
        try {
          const { resolveBinary } = require('../resolve');
          cursorBinPath = path.resolve(await resolveBinary());
        } catch (error) {
          cursorBinError = error.message || String(error);
        }
      }
    } else {
      try {
        const { resolveBinary } = require('../resolve');
        cursorBinPath = path.resolve(await resolveBinary());
      } catch (error) {
        cursorBinError = error.message || String(error);
      }
    }
    if (!cursorBinPath && !opts.command) {
      cursorBinPath = resolveCommandBinary('astrolabe', process.env);
    }
    if (!cursorBinPath) {
      process.stderr.write(
        `[astrolabe setup] could not resolve the native astrolabe binary: ` +
          `${cursorBinError || 'not found'}\n` +
          '  Cursor needs an absolute binary path; skipping Cursor setup.\n' +
          '  Fix: npm i -g @zlnap/astrolabe (or pass --command=/abs/path/astrolabe)\n',
      );
    }
  }

  const actions = [];
  for (const id of selectedIds) {
    const client = getClient(id);
    const launch = resolveLaunch({
      context: client.context,
      native: opts.native,
      commandOverride: opts.command,
    });
    const plan = client.plan({
      home,
      launch,
      withHooks: opts.withHooks,
      projectRoot,
      cursorBinPath,
      pyrightPath,
      env: process.env,
      detected: detectedById.get(id),
      scope: opts.scope,
      cursorCommandExplicit: Boolean(opts.command),
    });
    const applied = plan.apply();
    actions.push({
      id,
      label: client.label,
      summary: plan.summary,
      launch,
      applied,
    });
  }

  const cursorAction = actions.find((action) => action.id === 'cursor');
  const cursorApplied = Boolean(cursorAction && !cursorAction.applied.skipped);

  let permissionsPlan = null;
  let allowlistCheck = null;
  if (opts.cursorPermissions && cursorApplied) {
    allowlistCheck = checkCursorAllowlist(home);
    process.stdout.write(`\nCursor MCP allowlist check: ${allowlistCheck.message}\n`);
    permissionsPlan = mergeCursorPermissions(home, {
      dryRun: opts.dryRun,
      yes: opts.yes,
    });
  }

  process.stdout.write('\nPlan:\n');
  for (const action of actions) {
    if (action.applied.skipped) {
      process.stdout.write(`  - ${action.label}: SKIP ${action.applied.reason}\n`);
      continue;
    }
    const op = action.applied.created ? 'create' : action.applied.replaced ? 'update' : 'write';
    process.stdout.write(`  - ${action.label}: ${op} ${action.applied.filePath}\n`);
    if (action.applied.installDirs) {
      for (const dir of action.applied.installDirs) {
        process.stdout.write(`      plugin → ${dir}\n`);
      }
    }
    if (Array.isArray(action.applied.results)) {
      for (const step of action.applied.results) {
        if (!step || step.skipped) continue;
        if (step.filePath && step.filePath !== action.applied.filePath) {
          const sop = step.created ? 'create' : step.replaced ? 'update' : 'write';
          process.stdout.write(`      ${sop} ${step.filePath}\n`);
        }
      }
    }
    if (action.id !== 'deepseek-harness') {
      const launchCommand =
        action.id === 'cursor' && action.applied.entry
          ? action.applied.entry
          : action.launch;
      process.stdout.write(
        `      command: ${launchCommand.command} ${(launchCommand.args || []).join(' ')}\n`,
      );
    }
    const preview = formatEntryPreview(action.applied);
    if (preview) {
      for (const line of preview.split('\n')) {
        process.stdout.write(`      ${line}\n`);
      }
    }
  }
  if (permissionsPlan) {
    const migrateBit =
      permissionsPlan.migrated > 0
        ? `, migrate ${permissionsPlan.migrated} legacy name(s)→short`
        : '';
    process.stdout.write(
      `  - Cursor permissions: merge ${permissionsPlan.added} allowlist entries${migrateBit} into ${permissionsPlan.filePath}` +
        `${permissionsPlan.created ? ' (new file)' : ''}\n`,
    );
    process.stdout.write(
      '      default: short catalog names only; tools/call still accepts legacy aliases\n',
    );
  }

  if (opts.dryRun) {
    process.stdout.write('\n[dry-run] No files written.\n');
    return 0;
  }

  if (!opts.yes && isTTY()) {
    if (!confirm('Apply these changes?', { defaultYes: false })) {
      process.stdout.write('Cancelled; no files written.\n');
      return 1;
    }
  } else if (!opts.yes && !isTTY()) {
    process.stderr.write('Refusing to write without --yes in non-interactive mode.\n');
    return 1;
  }

  const summary = [];
  for (const action of actions) {
    if (action.applied.skipped) {
      summary.push({ id: action.id, status: 'skipped', reason: action.applied.reason });
      continue;
    }
    const backups = backupAppliedFiles(action.applied);
    action.applied.write();
    summary.push({
      id: action.id,
      status: 'written',
      file: action.applied.filePath,
      backup: backups[0] || null,
      backups,
    });
    process.stdout.write(
      `Wrote ${action.applied.filePath}${backups.length ? ` (backup ${backups.join(', ')})` : ''}\n`,
    );
  }

  let cursorApproval = null;
  if (cursorDetected && projectRoot && cursorApplied) {
    cursorApproval = clientsMod.enableCursorApproval({
      home,
      projectRoot,
      env: process.env,
      whichFn: clientsMod.which,
    });
    if (cursorApproval.disabled && cursorApproval.disabled.changed && cursorApproval.disabled.doc) {
      const backup = backupFile(cursorApproval.disabled.filePath);
      writeJsonAtomic(cursorApproval.disabled.filePath, cursorApproval.disabled.doc);
      process.stdout.write(
        `Removed astrolabe from ${cursorApproval.disabled.filePath}` +
          `${backup ? ` (backup ${backup})` : ''}\n`,
      );
    }
    const approvalHint =
      '  Global MCP registration cannot pre-approve future projects: Cursor asks on the ' +
      'first use of each project name. Headless runs can pass --approve-mcps; otherwise ' +
      'run `cursor-agent mcp enable astrolabe` in that project.\n';
    if (!cursorApproval.available) {
      process.stdout.write(
        '\nCursor CLI approval: cursor-agent not found on PATH.\n' +
          `  Run: cursor-agent mcp enable astrolabe   (cwd ${projectRoot})\n` +
          approvalHint,
      );
    } else if (opts.scope === 'project') {
      if (cursorApproval.status !== 0 || cursorApproval.error || cursorApproval.signal ||
          !cursorApproval.approval.approved) {
        process.stdout.write(
          `\nCursor CLI approval: \`cursor-agent mcp enable astrolabe\` exited ${cursorApproval.status}` +
            `${cursorApproval.stderr ? `: ${cursorApproval.stderr}` : ''}\n` +
            `  Run it manually in ${projectRoot}.\n` +
            '  Cursor CLI silently hides unapproved project MCP servers from the model.\n',
        );
      } else {
        process.stdout.write(
          `\nCursor CLI approval: approved astrolabe for ${projectRoot} (${cursorApproval.approvalPath})\n`,
        );
        summary.push({
          id: 'cursor-approval',
          status: 'written',
          file: cursorApproval.approvalPath,
          backup: null,
        });
      }
    } else {
      process.stdout.write(
        `\nCursor CLI: ran \`cursor-agent mcp enable astrolabe\` in ${projectRoot} ` +
          `(best effort, exit ${cursorApproval.status}).\n` +
          approvalHint,
      );
    }
  }

  if (permissionsPlan) {
    if (permissionsPlan.created && !opts.yes && isTTY()) {
      process.stdout.write(
        `\nNote: defining mcpAllowlist replaces the IDE MCP allowlist table. ` +
          `Other MCP tools must be listed in the same file. Cursor Run Mode must be on.\n`,
      );
      if (!confirm(`Create ${permissionsPlan.filePath}?`, { defaultYes: false })) {
        process.stdout.write('Skipped Cursor permissions.json\n');
        permissionsPlan = null;
      }
    }
    if (permissionsPlan) {
      const backup = backupFile(permissionsPlan.filePath);
      writeJsonAtomic(permissionsPlan.filePath, permissionsPlan.doc);
      const migrateNote =
        permissionsPlan.migrated > 0
          ? `, migrated ${permissionsPlan.migrated} legacy→short`
          : '';
      process.stdout.write(
        `Merged Cursor allowlist ${permissionsPlan.filePath} ` +
          `(+${permissionsPlan.added}${migrateNote}, total ${permissionsPlan.total})` +
          `${backup ? `; backup ${backup}` : ''}\n`,
      );
      if (permissionsPlan.migrated > 0) {
        process.stdout.write(
          `  Rewrote legacy allowlist names (${permissionsPlan.legacyNames.join(', ')}) ` +
            'to short catalog names; server tools/call aliases remain.\n',
        );
      }
      summary.push({
        id: 'cursor-permissions',
        status: 'written',
        file: permissionsPlan.filePath,
        backup,
      });
    }
  }

  process.stdout.write('\nDone. Next steps:\n');
  process.stdout.write('  1. Restart each configured agent / IDE so it reloads MCP servers.\n');
  process.stdout.write('  2. Verify: npx -y @zlnap/astrolabe --version\n');
  if (selectedIds.includes('claude-code')) {
    process.stdout.write('  3. Claude Code: claude mcp get astrolabe\n');
  }
  if (selectedIds.includes('cursor')) {
    if (cursorDetected) {
      if (opts.scope === 'project') {
        process.stdout.write(
          `  3. Cursor: project config ${path.join(projectRoot, '.cursor', 'mcp.json')} + ` +
            `${path.join(projectRoot, '.cursor', 'rules', 'astrolabe.mdc')}; this directory ` +
            'requires successful `cursor-agent mcp enable astrolabe` approval before use.\n',
        );
      } else {
        process.stdout.write(
          `  3. Cursor: global config ${path.join(home, '.cursor', 'mcp.json')}. Cursor has no ` +
            'file-based global rules directory; the L1 guidance comes from the MCP instructions ' +
            'and the global ~/.cursor/hooks.json remind hook (enable with --with-hooks). ' +
            'New projects still prompt for MCP approval on first use.\n',
        );
      }
    } else {
      process.stdout.write(
        '  3. Cursor: not detected; skipped config, rules and MCP approval.\n',
      );
    }
    if (opts.withHooks) {
      process.stdout.write(
        '     Hooks: ~/.cursor/hooks.json uses an absolute binary path and --client=cursor ' +
          '(matcher includes astrolabe-.*).\n',
      );
    } else {
      process.stdout.write(
        '     Tip: re-run with --with-hooks to write ~/.cursor/hooks.json (absolute path, --client=cursor).\n',
      );
    }
  }
  if (selectedIds.includes('opencode')) {
    process.stdout.write(
      '  3. OpenCode: restart opencode; MCP is under mcp / mcp.servers in ~/.config/opencode/.\n',
    );
    if (!opts.withHooks) {
      process.stdout.write(
        '     Tip: re-run with --with-hooks to install plugins/astrolabe-guard.ts\n',
      );
    }
  }
  if (selectedIds.includes('deepseek-harness')) {
    process.stdout.write(
      '  3. DSH: native guard is in cordis.patch.yml; add the Astrolabe MCP server per repo in the DSH UI.\n',
    );
  }
  if (selectedIds.includes('codebuddy')) {
    process.stdout.write(
      '  3. Codebuddy: MCP in ~/.codebuddy/.mcp.json; restart codebuddy. Tip: --with-hooks merges ~/.codebuddy/settings.json.\n',
    );
  }
  if (selectedIds.includes('grok')) {
    process.stdout.write(
      '  3. Grok Build: MCP in ~/.grok/config.toml ([mcp_servers.astrolabe]); tip: --with-hooks writes ~/.grok/hooks/astrolabe.json.\n',
    );
  }
  if (selectedIds.includes('kimicode')) {
    process.stdout.write(
      '  3. Kimi Code: MCP in ~/.kimi-code/mcp.json; tip: --with-hooks merges [[hooks]] into config.toml.\n',
    );
  }
  if (selectedIds.includes('zcode')) {
    process.stdout.write(
      '  3. ZCode: MCP under mcp.servers in ~/.zcode/cli/config.json; tip: --with-hooks adds hooks.events.PreToolUse (--client=zcode; no SessionEnd; scrub Stop cleanup).\n',
    );
  }
  return summary.some((row) => row.status === 'written') || summary.every((row) => row.status === 'skipped')
    ? 0
    : 0;
}

module.exports = { run, detectAll, resolveHome };
