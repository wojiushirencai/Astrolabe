# @zlnap/astrolabe

Astrolabe AI：面向 AI 编码助手的本地代码检索与导航 MCP server（二进制名 `astrolabe`）。与 npm 上无关的 `astrolabe`、`astrolabe-ai` 包不是同一个项目。本包是**零依赖**的 Node shim：通过 `optionalDependencies` 安装当前平台的原生二进制，再把 stdin/stdout/stderr、退出码和信号原样交给它。

```bash
npx -y @zlnap/astrolabe /path/to/repo
```

MCP 客户端（推荐一键配置）：

```bash
npx -y @zlnap/astrolabe setup
```

Detects Claude Code, Cursor, Codex, Windsurf, Cline, Roo, Continue, OpenCode, DeepSeek Harness, Codebuddy, Grok Build, Kimi Code, and ZCode. Merges MCP config (OpenCode V1/V2/jsonc; Grok `[mcp_servers.*]` TOML; ZCode `mcp.servers`; DSH installs the native guard plugin + `cordis.patch.yml` merge). Optional `--with-hooks` installs OpenCode's guard plugin and hook recipes for Codebuddy / Grok / Kimi / ZCode (ZCode: no `SessionEnd`, do not hang cleanup on `Stop`). Flags: `--yes`, `--clients=…`, `--dry-run`, `--with-hooks`, `--list`.

探测本机 Claude Code / Cursor / Codex 等，交互勾选后合并写入 MCP 配置（`--yes`、`--dry-run`、`--list` 可用）。手动配置示例：

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
