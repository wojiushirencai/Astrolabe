# astrolabe-dsh-guard — Astrolabe drift guard for DeepSeek Harness

A **native DeepSeek Harness (DSH) hook plugin** that runs `astrolabe hooks remind`
on every relevant tool call and denies the call when Astrolabe says the agent has
drifted into repeated `grep`/`read` instead of using Astrolabe's MCP tools.

Unlike the Claude Code marketplace plugin in `../claude-plugin` (which only
registers the MCP server), this package is for **DSH hosts**, whose hook surface
is a Cordis plugin API rather than a `hooks.json` file.

## Why a native plugin

DSH's own design note is explicit:

> "native hooks" are not a package — a native hook is just an ordinary Cordis
> plugin subscribing to the canonical lifecycle events. … Anything a bridge can
> do, a plain plugin can do directly — more powerfully (no serialization
> boundary, full `ctx`, typed returns).

Running Astrolabe's hook through `@deepseek-ai/dsh-hooks-claude-code` also works,
but that path needs a third-party bridge package, a `hooks.json`, and a wrapper
script. This plugin needs none of them.

## Requirements

- `astrolabe` on `PATH` (see the repository README), or set `config.binary` to an
  absolute path.
- A DSH profile where the plugin can be loaded (see *Install* below).

## Install

```bash
# 1. put the package where the DSH profile resolves modules
cp -R dsh-plugin "$HOME/.dsh/profiles/desktop/node_modules/astrolabe-dsh-guard"
```

```yaml
# 2. ~/.dsh/cordis.patch.yml — insert the plugin
- insert:
    - id: astrolabe-guard
      name: 'astrolabe-dsh-guard'
      config:
        client: claude-code
```

DSH hot-reloads the patch file; no restart is required.

> The package declares `dsh.bundle`, so a DSH plugin manager that installs
> bundles can also take it (from a git URL or a local path) and manage it as a
> normal entry.

## Configuration

| Key | Default | Meaning |
|---|---|---|
| `binary` | `astrolabe` | Executable to run; an absolute path is safest |
| `client` | `claude-code` | `--client` dialect passed to `astrolabe hooks remind` |
| `timeoutMs` | `20000` | Per-call timeout for the hook process |
| `stateDir` | `<tmpdir>/astrolabe-dsh-guard` | `HOME` for the hook process; Astrolabe writes its counter under `$HOME/.astrolabe` |

## The sandbox detail that matters

DSH runs hook commands inside its file sandbox. The user's home is readable but
**not writable** there, so Astrolabe's counter write to `~/.astrolabe/hook_data/`
fails and the hook silently fails open — the guard appears to be installed but
never denies anything.

The plugin therefore points the hook process at a writable `HOME`
(`stateDir`) instead of widening the sandbox. Counters live under
`<tmpdir>/astrolabe-dsh-guard/.astrolabe/hook_data/<session_id>/counter.json`
and reset when the OS clears the temp directory.

## Behaviour

- Tools observed: `grep`, `read`, `glob`, `bash`/`shell`, `rg`, plus every
  Astrolabe MCP tool (so that using Astrolabe resets the counter).
- Default threshold: the 3rd consecutive `grep`/`read` without an intervening
  symbolic Astrolabe call is denied, with Astrolabe's own reason text forwarded
  to the model.
- Any failure (missing binary, timeout, unparsable output, sandbox fault) **fails
  open**: the tool call proceeds.

## Notes

- Astrolabe identifies its own tools by substring, so DSH's per-repository server
  names (`mcp__astrolabe-<repo>__find_symbol`) need no rewriting.
- Any Astrolabe catalog tool (including `search_code`) resets the
  grep/read/non_symbolic/slice counters on PreToolUse and clears the
  post-deny silence window. Prefer `find_symbol` /
  `resolve_context` / `find_references` / `goto_definition` for navigation;
  `search_code` is fine for recovery after a deny.

## Licence

MIT OR Apache-2.0, matching the rest of Astrolabe.
