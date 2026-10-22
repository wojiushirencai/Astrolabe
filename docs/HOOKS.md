# 防漂移 Hook（Anti-Drift Hooks）

> **状态说明（2026-10-18）**：本文档描述的是**机制**（拦截规则、各客户端配置）。这套 Hook **没有做过效果基准**：我们的 A/B 实测（见 [bench/](../bench/README.md)）里 Astrolabe 组没有启用 Hook，因此没有数据证明它能提高工具采用率或降低成本。它是可选功能，不属于项目的卖点。需要它时请自行评估；如果你做了对照实验，欢迎提 issue。

## 1. 它做什么

在支持 PreToolUse 钩子的客户端里，`astrolabe hooks remind` 会统计模型连续使用 grep / 整文件 Read 的次数，超过阈值时返回 deny 并提示改用 Astrolabe 工具。会话状态存在 `~/.astrolabe/hook_data/`。

## 2. Claude Code 配方

可选：将以下配置**合并进**（不要覆盖已有的 `hooks` 条目）项目 `.claude/settings.json`（或全局 `~/.claude/settings.json`）：
```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "astrolabe hooks remind --client=claude-code"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "astrolabe hooks cleanup"
          }
        ]
      }
    ]
  }
}
```

## 3. 其它客户端


Astrolabe 防漂移 Hook 层已内置对 **Codebuddy** 与 **Grok** 客户端的识别与拦截输出：
- `--client=codebuddy`：走与 Claude Code 同构的工具名识别规则（覆盖 `grep`、`read`、`search_for_pattern`、`read_file` 及 shell 常见命令），输出兼容 CC 的 `hookSpecificOutput` 结构化决策；
- `--client=grok`（支持 `grokbuild`、`grok-build` 别名）：识别 `grep` 与 `read_file` 及 shell 命令，输出 Grok 规范的扁平 JSON 格式（`{"decision":"deny","reason":...}`）；
- **Context 行为**：Codebuddy 与 Grok 目前均使用 `default` 温和级上下文（无特殊 schema 限制或工具过滤，无需指定单独 `--context` 参数）。

> **Hook 配方示例（适用于宿主支持 CC 兼容 hooks 的情况）**：
>
> 若要对该客户端**硬禁用** Grep/Glob/shell 文本搜索，在 mcp.json 的 Astrolabe `env` 与 hooks 命令两侧同时打开开关，例如：
>
> ```json
> {
>   "mcpServers": {
>     "astrolabe": {
>       "command": "astrolabe",
>       "args": ["--context=claude-code"],
>       "env": {
>         "ASTROLABE_DENY_TEXT_SEARCH": "true"
>       }
>     }
>   }
> }
> ```
>
> ```json
> "command": "astrolabe hooks remind --client=claude-code --deny-text-search"
> ```
>
> `--deny-text-search` 是跨平台写法（Windows cmd/PowerShell 不支持 `VAR=1 cmd` 前缀语法，请勿在 hooks 命令里使用）。
>
> 或把 `deny_text_search: true` 写进自定义 context YAML，hooks 用 `--context=/path/to.yml`（优先级：CLI `--deny-text-search` > 环境变量 > context YAML > 默认 off）。
>
> 若所使用的 Codebuddy 或 Grok 宿主环境支持 Claude Code 兼容的 hooks 配置语法，可沿用与上方 CC 相同的 settings 配方，只需将 PreToolUse 命令中的 `--client` 替换为对应值即可：
>
> *Codebuddy 示例*：
> ```json
> {
>   "hooks": {
>     "PreToolUse": [
>       {
>         "matcher": "",
>         "hooks": [
>           {
>             "type": "command",
>             "command": "astrolabe hooks remind --client=codebuddy"
>           }
>         ]
>       }
>     ],
>     "SessionEnd": [
>       {
>         "matcher": "",
>         "hooks": [
>           {
>             "type": "command",
>             "command": "astrolabe hooks cleanup"
>           }
>         ]
>       }
>     ]
>   }
> }
> ```
>
> *Grok 示例*：
> 将 command 替换为 `astrolabe hooks remind --client=grok` 即可。若宿主为其它自定义 hook 触发机制，只需将 PreToolUse payload（包含 `session_id` 与 `tool_name` 等 JSON 字段）经 stdin 送入该命令，并读取 stdout 返回的扁平 deny 决策。

### Cursor / Kimi / ZCode / OpenCode / Grok Build

#### 1. Cursor CLI

Cursor CLI 支持原生 hooks 配置，亦具备对 Claude Code 配置的兼容能力：
- **配置文件**：放置于 `~/.cursor/hooks.json`（全局）或当前项目根目录 `.cursor/hooks.json`。事件名使用小驼峰（`preToolUse`、`sessionEnd`）。另外，Cursor 也会自动加载 `.claude/settings.json`（第三方兼容模式，PreToolUse 自动映射，但注意该模式不映射 `additionalContext`）。
- **工具识别与输出格式**：内置工具名包括 `Shell`、`Read`、`Write`、`Grep`、`Task`；Astrolabe `--client=cursor`（别名：`cursor-agent`、`cursor_agent`、`cursor-cli`）输出 Cursor 原生扁平格式 `{"permission":"deny","user_message":...,"agent_message":...}`，长引导文案直接注入 `agent_message` 送达模型。
- **matcher 必须包含 Astrolabe 的 MCP 工具**：remind 要看到 `mcp__astrolabe__*` 调用才能在你用了符号工具后清零 grep/read 计数；matcher 只写 `Shell|Read|Grep` 会让计数只增不清，已在正确使用 Astrolabe 的会话里仍可能被拦。`mcp__astrolabe__.*` 这一写法（Cursor 与 Grok 的 MCP 工具名格式）**未在真实宿主上验证**，若你的宿主不识别，请改用 `*`（ZCode 一节同理），非相关工具由 Astrolabe 自行放行。
- **会话识别兼容**：Cursor 原生 preToolUse 事件仅传递 `conversation_id`，Astrolabe 内部会话提取逻辑已原生兼容 `conversation_id` / `conversationId` fallback。

**完整配方（`.cursor/hooks.json`）**：
```json
{
  "version": 1,
  "hooks": {
    "preToolUse": [
      {
        "command": "astrolabe hooks remind --client=cursor",
        "matcher": "Shell|Read|Grep|mcp__astrolabe__.*"
      }
    ],
    "sessionEnd": [
      {
        "command": "astrolabe hooks cleanup"
      }
    ]
  }
}
```

#### 2. Grok Build

针对 `xai-org/grok-build`（CLI 名 `grok`）：
- **配置文件**：放置于 `~/.grok/hooks/astrolabe.json`（全局）或项目级 `.grok/hooks/astrolabe.json`。
- **项目级授信提示**：若使用项目级 hooks 配置，必须在 Grok CLI 中执行 `/hooks-trust` 完成授信后方可生效。
- **匹配与决策**：Grok 内置工具使用 snake_case（`run_terminal_command`、`read_file`、`grep`），matcher 建议同时编写 PascalCase 别名与 snake_case，并加上 `mcp__astrolabe__.*`：`"Bash|Read|Grep|run_terminal_command|read_file|grep|mcp__astrolabe__.*"`。Astrolabe 原生兼容驼峰 payload（`sessionId`、`toolName`、`toolInput`），并通过 `--client=grok` 输出 Grok 一等公民原生扁平决策 `{"decision":"deny","reason":...}`；退出码遵循规范（2 代表阻断、1 代表 fail-open 放行）。

**完整配方（`~/.grok/hooks/astrolabe.json`）**：
```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|Read|Grep|run_terminal_command|read_file|grep|mcp__astrolabe__.*",
        "hooks": [
          {
            "type": "command",
            "command": "astrolabe hooks remind --client=grok",
            "timeout": 10
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "astrolabe hooks cleanup",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
```

#### 3. Kimi Code CLI 与 ZCode

- **Kimi Code CLI**（Moonshot，CLI 命令 `kimi`，仓库 `MoonshotAI/kimi-code`）：
  - 配置文件位于 `~/.kimi-code/config.toml` 的 `[[hooks]]` 配置块（支持 `PreToolUse`、`PostToolUse` 与 `matcher`）；
  - 协议与 Claude Code 同构（通过 stdin 输入 JSON：包含 `session_id`、`tool_name`、`tool_input`；deny 拦截输出 `hookSpecificOutput` 结构化决策或 exit 2 阻断）；内置工具识别覆盖 `Read`、`Grep`、`Bash`；
  - PreToolUse 命令参数使用 `--client=kimicode`（别名：`kimi-code`、`kimi_code`、`kimi`）；
  - *示意配置（注：TOML 具体键名未经逐字核对，以下为示意级示例，请以官方文档为准）*：
    ```toml
    # ~/.kimi-code/config.toml 示意配置（具体键名以官方文档为准）
    [[hooks]]
    event = "PreToolUse"
    matcher = "Read|Grep|Bash|mcp__astrolabe__.*"
    command = "astrolabe hooks remind --client=kimicode"

    [[hooks]]
    event = "SessionEnd"
    command = "astrolabe hooks cleanup"
    ```
- **ZCode**（智谱 Z.ai，仓库 `zai-org/ZCode`）：
  - **事件集与 Claude Code 不同**：ZCode 仅支持 `SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PermissionRequest`、`PostToolUse`、`PostToolUseFailure`、`Stop` 七个事件，且对事件名做严格校验——**配置中出现 `SessionEnd` 等未知事件会导致整段 hooks 加载失败**（PreToolUse 也随之失效，见 issue #30）。
  - **配置文件**：用户级 `~/.zcode/cli/config.json`（Windows：`%USERPROFILE%\.zcode\cli\config.json`）的 `hooks` 字段。注意必须有 `"enabled": true`，且事件写在 `events` 包装层内（与 CC 的 `hooks.PreToolUse` 直挂不同）。ZCode 读到的 `~/.claude/settings.json` 等 CC 旧配置默认是禁用、只读的，不能依赖。
  - **不要**把 `astrolabe hooks cleanup` 挂到 `Stop` 上：`Stop` 每轮回答结束都会触发，会把防漂移计数每轮清零。ZCode 无 `SessionEnd`，状态目录由 `astrolabe hooks remind` 内置的过期 GC 自动清理（超过 24 小时未活动的会话，可用 `ASTROLABE_HOOK_GC_HOURS` 调整），也可手动执行 `astrolabe hooks gc`。
  - 配置示例（合并进已有 `config.json`，保留其它字段）：
    ```json
    {
      "hooks": {
        "enabled": true,
        "events": {
          "PreToolUse": [
            {
              "matcher": "*",
              "hooks": [
                {
                  "type": "command",
                  "command": "astrolabe hooks remind --client=zcode",
                  "timeout": 10
                }
              ]
            }
          ]
        }
      }
    }
    ```
  - `--client=zcode`（别名：`z-code`、`zai`）：已对照 ZCode 3.14.1 源码核实，stdin 为 CC 同构 payload（`session_id`、`hook_event_name`、`tool_name`、`tool_input`、`tool_use_id`、`cwd`、`transcript_path` 等，另附 camelCase 副本）；内置工具名 `Read`（`file_path`/`offset`/`limit`）、`Grep`、`Glob`、`Bash`（`command`），MCP 工具名为 `mcp__<server>__<tool>`；stdout JSON 的 `hookSpecificOutput.permissionDecision: "deny"` 生效，exit 2 亦视为 deny（以 stderr 为原因），其它非零退出码视为 hook 执行失败。matcher 使用 `*`：remind 需要同时看到 `mcp__astrolabe__*` 调用才能清零计数，非相关工具由 Astrolabe 自行放行。

#### 4. OpenCode

针对 `anomalyco/opencode`（`opencode.ai`）：
- **机制说明**：OpenCode 宿主本身未提供预置 hooks 命令体系，但提供了灵活的 TypeScript 插件扩展能力。通过在 `tool.execute.before` 钩子中抛出 `Error` 即可实现工具调用拦截。
- `--client=opencode` 输出扁平 `{"decision":"deny","reason":...}`，正好供 TypeScript 桥接插件解析。
- **桥接插件模板（`.opencode/plugins/astrolabe-guard.ts`）**：
```typescript
import type { Plugin } from "@opencode-ai/plugin";

export const AstrolabeGuard: Plugin = async () => {
  return {
    "tool.execute.before": async (input, output) => {
      const payload = JSON.stringify({
        session_id: input.sessionID,
        tool_name: input.tool,
        tool_input: output.args ?? {},
      });
      const proc = Bun.spawnSync(["astrolabe", "hooks", "remind", "--client=opencode"], {
        stdin: payload, stdout: "pipe", stderr: "pipe",
      });
      const stdout = new TextDecoder().decode(proc.stdout).trim();
      if (stdout) {
        const decision = JSON.parse(stdout);
        if (decision.decision === "deny") {
          throw new Error(decision.reason);
        }
      }
    },
  };
};
export default AstrolabeGuard;
```


## 4. 拦截与放行规则

模型有时不选用 MCP 工具而直接 `grep` / `read`（我们自己的实测里，v0.12.0 对 N5、N8 的采用率是 0/5；这是观察，不是 Hook 的效果数据）。Astrolabe 提供了可选的拦截机制：

### 核心拦截与放行规则
- **拦截（Deny）触发条件**：在没有调用 Astrolabe 符号工具的情况下，只要检测到 AI 连续 3 次无脑 grep（可用 `ASTROLABE_GREP_THRESHOLD` 调整）、连续 3 次裸读源码文件（基于 41 种支持语言源码后缀过滤，可通过 `ASTROLABE_READ_THRESHOLD` 调整；未支持语言常规读取不予拦截）、连续 4 次混合调用，或连续 10 次切片读（可通过 `ASTROLABE_SLICE_READ_THRESHOLD` 调整，期间无任何 Astrolabe 工具调用），Hook 会返回 deny，deny 文案直接给出切换指令（"Stop using grep/read for code discovery. Call mcp__astrolabe__search_code or mcp__astrolabe__find_symbol now (read-only, safe)."）。
- **硬禁用文本搜索（可选）**：设置 `ASTROLABE_DENY_TEXT_SEARCH=true`（或 hooks 命令加 `--deny-text-search`，或上下文 YAML `deny_text_search: true`）后，**首次**宿主 Grep / Glob / shell 文本搜索（`grep`/`egrep`/`fgrep`/`zgrep`/`rg`/`ag`/`ack`/`git grep`，含管道）即 deny，引导改用 `search_code` / `find_symbol` / `find_references` / `goto_definition`。默认关闭，不影响原 soft 阈值行为。适合同一机器上多个 Astrolabe MCP 进程服务不同宿主时按进程开关。注意：mcp.json 的 `env` 只作用于 MCP 进程本身；hooks 命令需同样带上该环境变量、CLI 开关，或 `--context=` 指向含 `deny_text_search: true` 的 YAML。
- **切片精读额度制**：带有 `limit ≤ 200`（可通过 `ASTROLABE_SLICE_READ_MAX` 调整）的 Read 切片调用仍可正常使用，但不再无限豁免——连续 10 次切片读且期间无任何 Astrolabe 工具调用时触发提醒式拦截，任何 Astrolabe 工具调用（含 `search_code` 等非符号工具）都会清零该额度。`head -n N` / `tail -n N` / `sed -n 'A,Bp'` 等有界切片 shell 命令仍完全放行、不计数。
- **计数重置**：AI 调用任意 Astrolabe 符号工具（如 `resolve_context`、`find_symbol` 等）时，全部连续计数立即清零；`search_code` 等非符号 Astrolabe 工具调用只清零切片读额度（不影响 grep/read 计数）；若连续调用间隔超过设定时间，亦自动重置。
- **智能放行窗口**：触发 Deny 后进入 15 秒静默宽容窗口（可通过 `ASTROLABE_DENY_SILENCE_SECS` 调整）。在此窗口内 Hook 放行且不累加计数，避免在特定需要连续细查的合法场景中打断正常排查。
- **状态存储与清理**：会话状态持久化于 `~/.astrolabe/hook_data/<session_id>/counter.json`；子代理调用时（payload 携带 `agent_id` / `agentId` 字段）按 `~/.astrolabe/hook_data/<session_id>/<agent_id>/counter.json` 分片，并发子代理各自独立计数、互不清零互不消音；会话结束时通过 `SessionEnd` 自动清理（含子代理分片目录）；无 `SessionEnd` 的宿主（如 ZCode）或会话异常退出时，`hooks remind` 每小时至多一次机会式清理超过 24 小时未活动的会话目录（`ASTROLABE_HOOK_GC_HOURS` 调整，`0` 关闭），亦可手动 `astrolabe hooks gc`，不留垃圾。

---


## DeepSeek Harness（原生插件）

DSH 没有自己的 `hooks.json`：它的"原生 hook"就是一个订阅 `tools/pre-execute` 等生命周期事件的 Cordis 插件，`dsh-hooks-claude-code` 只是把外部 shell hook 协议翻译到同一套 API 的中间层。因此 DSH 有两条接入路径：

### 方式一：原生插件（推荐）

用 `dsh-plugin/`（本仓库自带，包名 `astrolabe-dsh-guard`，声明 `dsh.bundle`）。它直接调用 `astrolabe hooks remind` 并把返回的 `permissionDecision` 映射成 DSH 的 `PreToolDecision`，不需要桥接包、外部 `hooks.json` 或包装脚本。

```yaml
# ~/.dsh/cordis.patch.yml
- insert:
    - id: astrolabe-guard
      name: 'astrolabe-dsh-guard'
      config:
        client: claude-code
```

### 方式二：经 `dsh-hooks-claude-code` 桥接

若已在用该桥接包，写一个 `PreToolUse` 的 `hooks.json` 指向 `astrolabe hooks remind --client=claude-code`，并在 `cordis.patch.yml` 里 `configPath` 指向它即可。

### DSH 上的两个必须注意点

1. **matcher 必须是 `*`，不能只写 `grep|read`。** 计数只在 hook 看到 Astrolabe 工具调用时才会清零；只把 grep/read 送进 hook 会让计数只增不清，正确使用 Astrolabe 的会话也会被永久拦截（直到空闲超时）。非相关工具由 Astrolabe 自行放行，开销可忽略。
2. **hook 进程需要可写的 `HOME`。** DSH 在文件 sandbox 里执行 hook 命令，用户家目录可读不可写，`~/.astrolabe/hook_data/` 的计数写入会失败并使 hook 静默 fail-open（表现为"装了但从不拦"）。请把 hook 进程的 `HOME` 指向可写目录（原生插件已默认指向 `<tmpdir>/astrolabe-dsh-guard`），不要把 sandbox 放宽来解决。

### 工具名

DSH 可为每个仓库注册一个 MCP server，工具名形如 `mcp__astrolabe-<repo>__find_symbol`。`is_astrolabe_tool` 按子串匹配（`astrolabe` + 目录里的工具名），因此**无需改写工具名**即可正确识别与清零。

------

## 输入异常与退出码（fail-open）

宿主语义里 exit 2 通常等于"拦截这次工具调用"，所以 `astrolabe hooks remind` / `cleanup` 遇到空 stdin、非 JSON、缺 `session_id`/`tool_name` 一律 **fail-open**：stderr 打一行提示并返回 0，不会拦住工具调用。`session_id` 含点、冒号、空格等字符时不拒绝，而是哈希成 `s-<fnv1a64>` 目录名（remind 与 cleanup 用同一映射）。只有"拦截"本身通过 stdout 的 deny JSON 表达。

## 硬禁用与命令识别说明

- `ASTROLABE_DENY_TEXT_SEARCH=true`（或 `--deny-text-search`）下 Grep/Glob 每次都拦，不受 `min_deny_interval_seconds` 静默窗约束；静默窗只作用于软模式。
- 命令识别按 shell 分词后只看"命令位置"的 token：支持 `cd x && grep`、`sudo grep`、`FOO=1 rg`、`bash -c "grep …"`、`grep.exe`/`rg.exe`、`findstr`、`Select-String`、`git [opts] grep`；`echo grep is great`、`git commit -m "fix grep bug"` 不会被当成搜索。软模式不把管道后段（`cat a | grep x`）当 grep，硬模式会拦。
- Windows 原生环境没有 `HOME` 时，状态目录回退到 `USERPROFILE`（再回退 `HOMEDRIVE`+`HOMEPATH`）。这一点只通过单元测试覆盖，**未在真实 Windows 上验证**。
- 计数文件锁使用操作系统文件锁（flock / LockFileEx），进程崩溃后自动释放，不会遗留需要手动清理的锁。
