# astrolabe

面向编码智能体的确定性代码图谱 MCP server。本包是**零依赖**的 Node shim：通过 `optionalDependencies` 安装当前平台的原生二进制，再把 stdin/stdout/stderr、退出码和信号原样交给它。

```bash
npx -y astrolabe /path/to/repo
```

MCP 客户端：

```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "npx",
      "args": ["-y", "astrolabe", "."]
    }
  }
}
```

日志走 stderr，JSON-RPC 走 stdin/stdout，shim 不会缓冲或改写任何通道。

若平台包因 `--omit=optional`、`--ignore-scripts` 或 npm 11 以下的 lockfile bug 缺失，shim 会从 GitHub Releases 下载对应 target 的归档，校验 SHA256 后缓存到 `~/.astrolabe/bin/`。
