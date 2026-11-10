# @zlnap/astrolabe

Astrolabe AI：面向 AI 编码助手的本地代码检索与导航 MCP server（二进制名 `astrolabe`）。与 npm 上无关的 `astrolabe`、`astrolabe-ai` 包不是同一个项目。本包是**零依赖**的 Node shim：通过 `optionalDependencies` 安装当前平台的原生二进制，再把 stdin/stdout/stderr、退出码和信号原样交给它。

## Install + setup (setup is required)

```bash
npm i -g @zlnap/astrolabe
# REQUIRED — without setup there is no MCP wiring and no hooks,
# so agents usually will not actually use the tools:
npx -y @zlnap/astrolabe setup --with-hooks
# non-interactive:
npx -y @zlnap/astrolabe setup --yes --with-hooks
```

Or one-shot via npx (still run setup after first use / on each machine):

```bash
npx -y @zlnap/astrolabe --version
npx -y @zlnap/astrolabe setup --with-hooks
```

`setup` detects Claude Code, Cursor, Codex, Windsurf, Cline, Roo, Continue, OpenCode, DeepSeek Harness, Codebuddy, Grok Build, Kimi Code, and ZCode; **merges** MCP config (never wipes other servers). Cursor defaults to the global `~/.cursor/mcp.json` with the absolute native binary, `type: stdio`, and `env` (`PATH`, `ASTROLABE_LSP_TIMEOUT=300`, `ASTROLABE_LSP_PYTHON` when pyright is found); it also best-effort runs `cursor-agent mcp enable astrolabe` in the current project and removes a stale `astrolabe` entry from `mcp-disabled.json`. Cursor has no file-based global rules directory, so L1 guidance comes from the MCP instructions plus the global `~/.cursor/hooks.json` remind hook. Add `--scope=project` (optionally `--project <dir>`) to write `<project>/.cursor/mcp.json` (pinned `cwd`), `<project>/.cursor/rules/astrolabe.mdc` (`alwaysApply: true`), and approve that directory; global MCP entries cannot pre-approve new projects, so Cursor asks on first use (`--approve-mcps` for headless). `--with-hooks` installs OpenCode's guard plugin and hook recipes for Cursor / Codebuddy / Grok / Kimi / ZCode (Cursor: `~/.cursor/hooks.json` with an absolute command path, `--client=cursor`, matcher `astrolabe-.*`; ZCode: no `SessionEnd`, scrub cleanup off `Stop`). Flags: `--yes`, `--clients=…`, `--dry-run`, `--native`, `--list`, `--with-hooks`, `--no-cursor-permissions`, `--scope=global|project`, `--project <dir>`.

## MCP tools (short names)

`tools/list` advertises: `resolve_contx`, `search_code`, `find_symbol`, `find_refs`, `goto_def`, `get_diag`, `get_lang`.  
Legacy long names (`resolve_context`, `find_references`, `goto_definition`, `get_diagnostics`, `get_languages`) and older `ast_*` aliases still work on `tools/call`.

## Windows

v0.17.4–v0.17.5 fixed real Windows issues: `\\?\` path normalize + watcher membership (#117), subdirectory `index_root` reuse (#117), and `path_filter` backslash/absolute-path normalize (#119). Prefer omitting `index_root` or passing the repo root. After install on Windows:

```powershell
npm i -g @zlnap/astrolabe
npx -y @zlnap/astrolabe setup --with-hooks
```

## Run as MCP server

```bash
npx -y @zlnap/astrolabe /path/to/repo --context=cursor
```

Manual MCP example:

```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "npx",
      "args": ["-y", "@zlnap/astrolabe", ".", "--context=cursor"]
    }
  }
}
```

日志走 stderr，JSON-RPC 走 stdin/stdout，shim 不会缓冲或改写任何通道。

若平台包因 `--omit=optional`、`--ignore-scripts` 或 npm 11 以下的 lockfile bug 缺失，shim 会从 GitHub Releases 下载对应 target 的归档，校验 SHA256 后缓存到 `~/.astrolabe/bin/`。

Docs: https://github.com/wojiushirencai/Astrolabe
