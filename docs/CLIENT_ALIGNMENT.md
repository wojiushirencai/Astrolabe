# 客户端适配对齐：Serena vs Astrolabe

只读对照笔记。反映 **本机磁盘现状**（`crates/astrolabe-mcp/`）。不改 Rust 源码。

## 状态总表

| 项 | 状态 | 落点 |
|---|---|---|
| project root：`.git` **或** `.serena/project.yml` | **已落地** | `root.rs`（就近单次向上；显式路径不 walk） |
| 内置 context YAML | **已落地** | `contexts/{default,claude-code,cursor,codex}.yml`（+ template） |
| `context.rs` / `cli.rs` | **已落地** | `--context` / `ASTROLABE_CONTEXT`；`--help` 列出四名 |
| structured 输出策略 | **已落地** | context 默认 + `ASTROLABE_STRUCTURED` 覆盖；始终非空 `content[0].text` |
| `excluded_tools` → `tools/list` retain | **已落地**（已测） | `server.rs::listed_tools`；内置 yml 均为 `[]` |
| CHANGELOG / ARCHITECTURE Phase 2 | **已更新** | LSP + MCP `find_references` / `get_diagnostics` / `plan_rename`；无 `apply_rename`；`goto_definition` core 有、MCP 未暴露 |
| `openai_tool_compatible` schema sanitize | **已落地** | `openai_schema.rs`；`listed_tools` 在 excluded 之后对 `codex`/`oaicompat*` sanitize |
| 内置 yml 填 `excluded_tools` | **暂空** | 等与客户端重复的文件/shell 工具出现 |
| `astrolabe context list` 子命令 | **未做** | `--help` 已列名；无 list/create/edit |
| README：`--context` / `ASTROLABE_CONTEXT` | **已更新** | 环境变量表有 `ASTROLABE_CONTEXT`；Codex/Claude Code 示例用 `args` + `--context=…`；Cursor 为 `["."]` + 可选 `--context=cursor` |
| MCP `goto_definition` | **未暴露** | core `lsp/queries` 有 |
| `apply_rename` / 安全写入层 | **未做** | 仅 `plan_rename` |
| 用户级 context 目录约定 | **未做** | 已支持路径加载，无 `~/.astrolabe/contexts` |
| 同步回 Mac 主仓 | **待做** | 本 box 改动需另推/同步 |
| prompt / mode / `single_project` | **有意不做** | 薄适配层；防漂移 hooks 已单独在 `hooks.rs` 落地 |
| `codebuddy` 客户端对齐 | **已支持** | hook 枚举/classify（与 CC 同构）/输出格式已支持；context 走 `default` 温和级 |
| `grok` 客户端对齐 | **已支持** | hook 枚举/classify（含 `grokbuild`/`grok-build` 别名）/输出格式（扁平 `{"decision":"deny","reason":...}`）已支持；context 走 `default` 温和级 |
| `cursor` CLI hook 对齐 | **已支持** | hook 枚举（含 `cursor-agent` 等别名）/原生扁平 deny（`permission`/`agent_message`）/`conversation_id` fallback 已支持；context 保持 `cursor` |
| `kimicode` 客户端对齐 | **已支持** | hook 枚举（含 `kimi-code`、`kimi` 等别名）/CC 同构三字段 deny 输出；context 走 `default` 温和级 |
| `zcode` 客户端对齐 | **已支持** | hook 枚举（含 `z-code`、`zai` 等别名）/CC 同构三字段 deny 输出；context 走 `default` 温和级 |
| `opencode` 客户端对齐 | **已支持** | hook 枚举/扁平 `decision: deny` 输出/TS 插件桥接拦截已支持；context 走 `default` 温和级 |

## 1. context 列表 + `structured_tool_output`

| | Serena | Astrolabe（现状） |
|---|---|---|
| 形态 | YAML context（+ 独立 mode）；含 prompt、`single_project`、tool overrides | 薄 YAML：`name` / `structured_tool_output` / `excluded_tools` / `notes`；**无** prompt/mode |
| 内置 | 十余个（`desktop-app`、`claude-code`、`codex`、`grok`、`ide`、`oaicompat-agent`…） | 四个：`default`、`claude-code`、`cursor`、`codex`（`crates/astrolabe-mcp/contexts/*.yml` 编译期嵌入） |
| 选择 | `--context`；用户目录可覆盖 | `--context` / `ASTROLABE_CONTEXT`（CLI 优先）；可传自定义 `.yml` 路径 |
| 列表 | `serena context list`（+ create/edit/delete） | **无** list 子命令；`--help` 列四名；另有 `ClientContext::builtin_names()` |
| structured | context 字段，`null`=auto；`claude-code.yml` 显式 `false` | bool/`null`；默认 off；`ASTROLABE_STRUCTURED` 可覆盖 |

**客户端实测硬规则（两边一致）：**

- **Claude Code**：不会解包 `structuredContent`，而是用它**替换** `content[0].text` → 默认关闭 structured；残缺 metadata 会让模型以为索引为空。
- **Cursor**：纯 `structuredContent`、无文本会被静默丢弃 → 必须保证 `content[0]` 非空文本。
- **Astrolabe**：始终先发非空文本（首行 `index_root:`）；`cursor`/`codex` 开 structured 时附带完整 `text`+`root`，禁止只发 stub。

## 2. project-from-cwd 规则

Serena `--project-from-cwd`：从进程 cwd **单次向上**找最近边界——`.serena/project.yml` **或** `.git`（目录或 worktree/submodule 指针）；就近获胜，嵌套 worktree 不被祖先劫持。找不到 → 项目 **inactive**。

Astrolabe（`root.rs`，已对齐标记）：

- `.` / 省略 → `find_project_root`（`.git` **或** `.serena/project.yml`），存**绝对路径**。
- 显式路径 → 只 canonicalize，**不**向上走（≈ Serena `--project`）。
- **差异**：无标记时 Serena 保持 inactive；Astrolabe **回退 canonicalize(cwd) + warn**。

## 3. `openai_tool_compatible` / Codex 怪癖

Serena（`mcp.py`）：context ∈ `{chatgpt, codex, oaicompat-agent}` 时 `_sanitize_for_openai_tools`：

- `integer` → `number`（+ `multipleOf: 1`）
- 去掉 union 里的 `null`
- 尽量折叠仅差 integer/number 的 `oneOf`/`anyOf`

另：Codex App **不在项目目录启动**会话（需手动 activate）；Codex CLI 常用 `--project-from-cwd`；另有 hooks（Serena 侧，Astrolabe 无）。

Astrolabe：**已实现**（`openai_schema::sanitize_for_openai_tools`）。`codex`（及 `oaicompat*`）在 `listed_tools` 中、`excluded_tools` retain **之后**重写 `inputSchema`；领域类型（`usize` 等）不变。

## 4. `excluded_tools` 模式

Serena：context / mode / project / global 多层 `ToolInclusionDefinition`。CLI agent context 常排除与客户端重复的文件/shell 工具。

Astrolabe：**解析 + `tools/list` retain 已落地**（`server.rs` 单测 `excluded_tools_are_omitted_from_catalog`）。内置四个 yml 均为 `[]`——图谱工具与客户端内置重叠少，暂时够用。无 mode 层、无 `fixed_tools`。**缺口**是「按客户端填非空列表」，不是 enforcement。

## 5. 异常 / 空结果：Precise·Unknown vs Serena `[]`

| 场景 | Serena | Astrolabe |
|---|---|---|
| 冷 LSP / 缺服务器 / 忙 | 常返回空列表 JSON（`[]`），语义像「零命中」 | `Precise<T>` + `Confidence::Unknown` + 非空 `note` |
| 权威空命中 | 空列表 | `Exact` + 空 `value` 才表示「确实没有」 |
| 索引未就绪 | （依赖 LSP 状态） | 「索引正在构建… confidence: unknown」，**不**伪装成空图 |
| 改名等写操作 | 可直接改 | `plan_rename` 只出计划；低于安全置信度不自动落盘 |

动机：冷 `rust-analyzer` 对 references 回 `[]` 而非错误；当成权威「无引用」会让智能体删活代码。

## 6. 推荐下一步（重写：已完 vs 剩余）

### 已完成（勿再当缺口）

1. ~~`root.rs` 双标记~~ — `.git` 或 `.serena/project.yml`，就近获胜。
2. ~~内置 context + `context.rs`/`cli.rs`~~ — 四个 yml；`--context` / env；`--help` 列名。
3. ~~structured 策略 + 非空文本~~ — Claude Code 默认关；cursor/codex 开且带完整 payload。
4. ~~`excluded_tools` catalog 过滤~~ — retain 已测；yml 填值另议。
5. ~~CHANGELOG / ARCHITECTURE Phase 2 表述~~ — LSP 三工具接线；明确无 `apply_rename`、MCP 无 `goto_definition`。
6. ~~`openai_tool_compatible` schema sanitize~~ — `openai_schema.rs`；codex/`oaicompat*` 的 `listed_tools`。
7. ~~**Codebuddy / Grok hook 接线**~~ — `hooks.rs` 中 client 枚举、classify 与对应 deny 决策输出已落地；context 走 `default`。

### 仍缺（按优先级）

1. ~~**`openai_tool_compatible` schema sanitize**~~ — 已落地，见「已完成」与 §7。
2. ~~**README 补齐**~~ — 环境变量表已有 `ASTROLABE_CONTEXT`；Codex/Claude Code 用 `args = [".", "--context=…"]`；Cursor `["."]` + 可选 `--context=cursor`。
3. **可选：`astrolabe context list`** — `--help` 已够用时可缓做；用户级 `~/.astrolabe/contexts` 覆盖同理。
4. **按需填 `excluded_tools`** — 仅当增加与客户端重复的文件/shell 类工具时。
5. **MCP `goto_definition`** — core 已有，未暴露为工具。
6. **`apply_rename` / 写后重解析 / 回滚** — Phase 3；无确认步骤前不要挂 apply。
7. **同步回 Mac 主仓** — box 上的 context/root/docs 改动需另推或拷回。

### 勿优先

prompt/mode 系统、hooks、`single_project` 整套——Astrolabe 刻意保持薄适配层。

## 7. Codex / openai_tool_compatible（调查笔记）

2026-09-11：对照 Serena `_sanitize_for_openai_tools`，并对 Astrolabe schemars 1.2 输出做了 dump。**sanitize 已接线**（同日落地）。

### Serena 做什么

- 触发：`context.name ∈ {chatgpt, codex, oaicompat-agent}` → `openai_tool_compatible=True`。
- 递归：`integer`→`number`+`multipleOf:1`；type 数组去 `null`；整型-only enum→number；折叠「类型+null」与 sanitize 后相同的 oneOf/anyOf。
- 回归：openai 兼容 schema 里每个 property 都有 `type`。

### Astrolabe 现行 schema 形状

| 形状 | 出现位置 | Codex/OpenAI 风险 |
|---|---|---|
| `"type": "integer"` + `format: "uint"` + `minimum: 0` | **每个工具**的 `budget_tokens: usize` | **高** |
| `"type": ["string", "null"]` | 若干 `Option<String>` 参数 | **中** |
| `boolean` / `string` / 省略可选 | 多数 | **低** |
| `oneOf` / `anyOf` / 整型 enum | **当前无** | 暂无 |

结论：缺口曾是 **inputSchema 含 `integer` 与 null union**；现已在 `codex`/`oaicompat*` 的 `list_tools` 出口 sanitize（领域 `usize` 等类型未改）。

### 建议实现

1. ~~**优先**：纯函数 `sanitize_for_openai_tools`~~ — `crates/astrolabe-mcp/src/openai_schema.rs`；`ClientContext::openai_tool_compatible`（`codex` / `oaicompat*`）。
2. **不推荐**：把 `budget_tokens` 改成 `f64` 污染类型（仍未改）。
3. ~~**测试**~~ — `openai_schema` 单测 + `codex_list_tools_sanitizes_integer_and_null_unions`。
4. ~~**文档**~~ — `codex.yml` notes 已更新；README 仍可继续补 `--context=codex` 示例。

```text
listed_tools:
  tools = router.list_all()
  filter excluded_tools          # 已落地
  if context.openai_tool_compatible:  # 已落地
      sanitize each input_schema
  sort + return
```

## 8. Codebuddy / Grok 对齐现状

- **Codebuddy**：hook 层枚举 `Client::Codebuddy`，走与 Claude Code 同构的工具名识别规则（`grep` / `read` / `search_for_pattern` / `read_file` 及 shell 兜底），输出 CC 格式结构化 deny（`hookSpecificOutput`）；MCP context 走 `default` 温和级（无额外 schema 修改或工具过滤）。
- **Grok**：hook 层枚举 `Client::Grok`（支持 `grokbuild`、`grok-build` 别名），识别 `grep` 与 `read_file` 及 shell 命令，输出扁平 `{"decision":"deny","reason":...}` 格式；MCP context 走 `default` 温和级。

## 9. Cursor CLI / Grok Build / Kimi Code / ZCode / OpenCode 对齐现状

- **Cursor CLI**：
  - 枚举：`Client::Cursor`（别名 `cursor` / `cursor-agent` / `cursor_agent` / `cursor-cli`）；
  - 工具分类：识别原生工具名 `Shell`、`Read`、`Write`、`Grep`、`Task` 及通用 shell 命令；
  - 输出格式：输出 Cursor 原生扁平 JSON `{"permission":"deny","user_message":...,"agent_message":...}`，长引导文案进 `agent_message` 直达模型；
  - 会话兼容：原生 hook payload 仅带 `conversation_id`，会话提取逻辑已支持 `conversation_id` / `conversationId` fallback；
  - Context：MCP 模式可配置 `--context=cursor`，通用/hook 走 `default`；
  - 接线方式：配置 `~/.cursor/hooks.json` 或项目 `.cursor/hooks.json`，在 `preToolUse` 执行 `astrolabe hooks remind --client=cursor`，在 `sessionEnd` 执行 `astrolabe hooks cleanup`。
- **Grok Build**：
  - 枚举：`Client::Grok`（别名 `grok` / `grokbuild` / `grok-build`）；
  - 工具分类：snake_case 工具名（`run_terminal_command` / `read_file` / `grep`）与 PascalCase 别名；
  - 输出格式：输出 Grok 一等公民原生扁平 JSON `{"decision":"deny","reason":...}`，退出码 2=阻断、1=放行；
  - Context：走 `default` 温和级；
  - 接线方式：配置 `~/.grok/hooks/astrolabe.json`（全局）或 `.grok/hooks/astrolabe.json`（项目级需 `/hooks-trust` 授信），PreToolUse 执行 `astrolabe hooks remind --client=grok`。
- **Kimi Code CLI**：
  - 枚举：`Client::KimiCode`（别名 `kimicode` / `kimi-code` / `kimi_code` / `kimi`）；
  - 工具分类：走与 Claude Code 同构的工具名识别规则（覆盖 `Read` / `Grep` / `Bash` 及 shell 命令）；
  - 输出格式：输出与 Claude Code 同构的 `hookSpecificOutput` 结构化决策（三字段）或 exit 2 阻断；
  - Context：走 `default` 温和级；
  - 接线方式：在 `~/.kimi-code/config.toml` 中配置 `[[hooks]]`（示意级，具体键名以官方文档为准），stdin 传入 JSON payload。
- **ZCode**：
  - 枚举：`Client::ZCode`（别名 `zcode` / `z-code` / `zai`）；
  - 工具分类：走与 Claude Code 同构的工具名识别规则；
  - 输出格式：输出与 Claude Code 同构的 `hookSpecificOutput` 结构化决策；
  - Context：走 `default` 温和级；
  - 接线方式：配置宿主 hooks 执行 `astrolabe hooks remind --client=zcode`，stdin 传入 CC 同构 payload。
- **OpenCode**：
  - 枚举：`Client::OpenCode`（别名 `opencode`）；
  - 工具分类：识别宿主传入的 tool 名称及 shell 命令；
  - 输出格式：输出扁平 `{"decision":"deny","reason":...}` 决策格式；
  - Context：走 `default` 温和级；
  - 接线方式：宿主无原生 hooks 命令体系，在 `.opencode/plugins/astrolabe-guard.ts` 中编写 TS 插件挂载 `tool.execute.before` 钩子，通过 `Bun.spawnSync` 调用 `astrolabe hooks remind --client=opencode`，遇到 `decision === "deny"` 时抛出 `Error(reason)` 实现拦截。

## 参考路径

- Serena：`src/serena/config/context_mode.py`、`mcp.py`、`cli.py`、`resources/config/contexts/*.yml`
- Astrolabe：`crates/astrolabe-mcp/src/{context,root,cli,server,main,precise_tools}.rs`、`crates/astrolabe-mcp/contexts/*.yml`、`docs/ARCHITECTURE.md`、`CHANGELOG.md`
