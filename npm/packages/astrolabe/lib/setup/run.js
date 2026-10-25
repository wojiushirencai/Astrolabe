'use strict';

const path = require('path');
const os = require('os');
const {
  CLIENTS,
  getClient,
  canonicalClientId,
  resolveLaunch,
  mergeCursorPermissions,
  backupFile,
  writeJsonAtomic,
} = require('./clients');
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

function run(argv = process.argv.slice(2)) {
  const opts = parseArgs(argv);
  if (opts.help) {
    process.stdout.write(USAGE);
    return 0;
  }

  const home = resolveHome(opts.home);
  const rows = detectAll(home);

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

  const actions = [];
  for (const id of selectedIds) {
    const client = getClient(id);
    const launch = resolveLaunch({
      context: client.context,
      native: opts.native,
      commandOverride: opts.command,
    });
    const plan = client.plan({ home, launch, withHooks: opts.withHooks });
    const applied = plan.apply();
    actions.push({
      id,
      label: client.label,
      summary: plan.summary,
      launch,
      applied,
    });
  }

  let permissionsPlan = null;
  if (opts.cursorPermissions && selectedIds.includes('cursor')) {
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
      process.stdout.write(`      command: ${action.launch.command} ${action.launch.args.join(' ')}\n`);
    }
    const preview = formatEntryPreview(action.applied);
    if (preview) {
      for (const line of preview.split('\n')) {
        process.stdout.write(`      ${line}\n`);
      }
    }
  }
  if (permissionsPlan) {
    process.stdout.write(
      `  - Cursor permissions: merge ${permissionsPlan.added} allowlist entries into ${permissionsPlan.filePath}` +
        `${permissionsPlan.created ? ' (new file)' : ''}\n`,
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
    const backup = backupFile(action.applied.filePath);
    action.applied.write();
    summary.push({
      id: action.id,
      status: 'written',
      file: action.applied.filePath,
      backup,
    });
    process.stdout.write(
      `Wrote ${action.applied.filePath}${backup ? ` (backup ${backup})` : ''}\n`,
    );
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
      process.stdout.write(
        `Merged Cursor allowlist ${permissionsPlan.filePath} ` +
          `(+${permissionsPlan.added}, total ${permissionsPlan.total})` +
          `${backup ? `; backup ${backup}` : ''}\n`,
      );
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
    process.stdout.write(
      '  3. Cursor: open Settings → MCP and confirm astrolabe is listed; enable Run Mode for allowlist.\n',
    );
    if (opts.withHooks) {
      process.stdout.write(
        '     Hooks: ~/.cursor/hooks.json uses --client=cursor (matcher includes mcp__astrolabe__.*).\n',
      );
    } else {
      process.stdout.write(
        '     Tip: re-run with --with-hooks to write ~/.cursor/hooks.json (--client=cursor).\n',
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
