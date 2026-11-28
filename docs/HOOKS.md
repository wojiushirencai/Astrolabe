# 防漂移 Hook（Deprecated / Removed）

> **状态：已正式移除。** 本页保留用于说明移除原因与旧版升级迁移，避免外部链接失效。

## 为什么移除

Astrolabe 已从防漂移 PreToolUse Hook 全面转向内置的意图路由提示词：

- **实测无需 Hook 即可高采用**：95 对 A/B 基准实测（[bench/README.md](../bench/README.md)）表明，精简后的 7 工具目录配合内置意图路由提示词，在不启用任何 Hook 的情况下即可达到 **96.8%** 的工具采用率（92/95）。
- **不再打断正常编辑流**：移除 PreToolUse 拦截后，不再阻断原生的 `Read` + `Edit` 修改流程，也不会干扰模型的并发工具调用。

## 升级迁移指引

从旧版升级后，运行一次以下命令即可自动清理历史遗留的 Astrolabe hooks：

```bash
# 重新配置 MCP 并顺带清理旧版 Astrolabe hooks
astrolabe setup
# 或仅清理 hooks，不改动现有 MCP 配置
astrolabe setup --remove-hooks
```

上述命令会从以下客户端配置中剥离旧版 Astrolabe hooks / 守卫插件，且**不会触碰其它第三方 hooks**：

- Claude Code（含 `settings.json` 及 `settings-*.json`）
- Cursor
- Codebuddy
- Grok
- Kimi Code
- ZCode
- OpenCode
- DeepSeek Harness（DSH）

## 二进制向后兼容

`astrolabe hooks ...` CLI 子命令仍保留为静默返回退出码 `0` 的空操作（no-op）。即使旧客户端配置文件尚未清理，触发该命令也不会报错或阻断工具调用。
