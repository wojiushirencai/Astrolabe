/**
 * Astrolabe drift guard for DeepSeek Harness — native hook plugin.
 *
 * DeepSeek Harness has no hook config file of its own: a "native hook" is an
 * ordinary Cordis plugin subscribing to the canonical interception events, and
 * the `dsh-hooks-claude-code` / `dsh-hooks-codex` packages are only translators
 * that map an external shell-hook protocol onto that same API. This plugin skips
 * the translation: it runs `astrolabe hooks remind` on `tools/pre-execute` and
 * maps the returned decision onto the harness's typed `PreToolDecision`.
 *
 * Astrolabe recognises its own MCP tools by substring, so a per-repository
 * server name such as `mcp__astrolabe-dict__find_symbol` already resets the
 * consecutive grep/read counter; no tool-name rewriting is needed here.
 *
 * @module astrolabe-dsh-guard
 */

import { mkdirSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

export const name = 'astrolabe-guard'

/** The command runner, the same seam the hook bridges use. */
export const inject = ['shell']

/**
 * Tools whose calls change Astrolabe's drift counter.
 *
 * `grep`, `read`, `glob` and shell text search accumulate the counter; every
 * Astrolabe tool call must also reach the hook so the counter resets — a denied
 * grep otherwise stays denied until the idle timeout. Unrelated tools are
 * skipped to avoid one process spawn per tool call.
 */
const RELEVANT = /^(grep|read|glob|bash|shell|rg)$/i

/** Astrolabe tool names as they appear after an MCP server prefix. */
const ASTROLABE_TOOL =
  /(?:^|__)(search_code|find_symbol|find_refs|find_references|goto_def|goto_definition|resolve_contx|resolve_context|get_diag|get_diagnostics|get_lang|get_languages)$/

/**
 * Whether this call can change Astrolabe's drift counter.
 * @param toolName - the harness tool name, e.g. `grep` or `mcp__astrolabe-dict__search_code`.
 * @returns true when the hook must observe the call.
 */
function relevant(toolName) {
  return RELEVANT.test(toolName) || (toolName.includes('astrolabe') && ASTROLABE_TOOL.test(toolName))
}

/**
 * Build the `PreToolUse` payload `astrolabe hooks remind` parses.
 * @param exec - the tool execution being gated.
 * @returns the payload object.
 */
function payloadOf(exec) {
  return {
    session_id: exec.agent?.session?.header?.id ?? '',
    transcript_path: '',
    cwd: exec.agent?.session?.header?.cwd ?? process.cwd(),
    hook_event_name: 'PreToolUse',
    tool_name: exec.name,
    tool_input: exec.arguments,
    tool_use_id: exec.callId,
  }
}

/**
 * Run `astrolabe hooks remind` and decode its decision.
 * @param shell - the harness shell executor.
 * @param spec - binary, client dialect, timeout, state home and the payload to pipe in.
 * @returns the decoded hook output, or undefined when it printed nothing usable.
 */
async function remind(shell, spec) {
  const request = {
    command: `${spec.binary} hooks remind --client=${spec.client}`,
    timeoutMs: spec.timeoutMs,
    stdin: JSON.stringify(spec.payload),
    // Astrolabe persists its consecutive-call counter under `$HOME/.astrolabe`.
    // This command runs inside the harness file sandbox, where the user's home
    // is readable but not writable, so the counter write would fail and the hook
    // would silently fail open. A writable state home keeps the counter alive
    // without widening the sandbox.
    env: { HOME: spec.stateHome },
    ...(spec.signal === undefined ? {} : { signal: spec.signal }),
  }
  const result = await (await shell.execute(shell.resolve(request))).result()
  const text = (result.stdout?.text ?? '').trim()
  if (text === '') return undefined
  try {
    return JSON.parse(text)
  } catch {
    // A malformed decision is not a blocking one: fail open.
    return undefined
  }
}

/**
 * Install the guard.
 * @param ctx - the plugin context, providing `shell` and the tool events.
 * @param config - plugin configuration (all optional).
 */
export function apply(ctx, config = {}) {
  const binary = typeof config.binary === 'string' && config.binary !== '' ? config.binary : 'astrolabe'
  const client = typeof config.client === 'string' && config.client !== '' ? config.client : 'claude-code'
  const timeoutMs = Number.isFinite(config.timeoutMs) && config.timeoutMs > 0 ? config.timeoutMs : 20000
  const stateHome = typeof config.stateDir === 'string' && config.stateDir !== ''
    ? config.stateDir
    : join(tmpdir(), 'astrolabe-dsh-guard')

  try {
    mkdirSync(stateHome, { recursive: true })
  } catch {
    // A state home we cannot create leaves the hook fail-open, never blocking.
  }

  ctx.on('tools/pre-execute', async (exec, next) => {
    if (!relevant(exec.name)) return next()

    let output
    try {
      output = await remind(ctx.shell, {
        binary,
        client,
        timeoutMs,
        stateHome,
        payload: payloadOf(exec),
        signal: exec.signal,
      })
    } catch {
      // A hook that cannot run must never block the turn — same rule as the
      // harness's own hook bridges.
      return next()
    }

    const decision = output?.hookSpecificOutput
    if (decision?.permissionDecision === 'deny') {
      return {
        kind: 'deny',
        reason: decision.permissionDecisionReason
          ?? 'Astrolabe: use the MCP code-navigation tools instead of grep/read',
      }
    }
    return next()
  })
}
