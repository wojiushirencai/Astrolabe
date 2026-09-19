# 设计笔记：是否把 `goto_definition` 暴露为 MCP 工具

**状态：已实现。** core `goto_definition` 已接 `SymbolAt` + `guard_cold_index`；MCP 精确层已挂工具（参数/渲染对齐 `find_references`）。下文保留原设计推理，供对照实现。

历史现状（实现前）：`astrolabe-core::lsp::queries::goto_definition` 已有查询层；MCP 精确层原先只挂 `find_references` / `get_diagnostics` / `plan_rename`。

---

## 1. 为什么暴露 / 为什么不暴露

### 赞成暴露

| 理由 | 说明 |
|---|---|
| 问题类型不同 | `find_symbol` 是图谱名字子串匹配（`syntactic`），答的是「索引里有哪些叫这个的符号」。`goto_definition` 是语言服务器从**使用点**解析绑定（`exact`），答的是「这个标识符实际指向哪」。静态路由按问题类型分流（见 `ARCHITECTURE.md` §3.1），二者不可互换。 |
| 避免错误回退 | 没有精确定义工具时，智能体只会反复打 `find_symbol` / `search_code`，把同名 overload、shadow、重导出当成定义。这和「图谱不行再猜 LSP」一样危险，只是方向反了。 |
| core 已付成本 | 查询层、假服务器单测、仓外 note 已在 `lsp/queries.rs`。MCP 侧主要是参数壳 + `LiveBackend` + 渲染，与 `find_references` 同构。 |
| Serena 对齐 | Serena 一类工具面向「从引用跳到声明」。Astrolabe 刻意用 `Precise`/`Unknown` 压过 Serena 常见的冷索引 `[]`；缺定义工具会让迁移智能体继续依赖弱的名字匹配。 |

### 反对 / 需克制的点

| 理由 | 说明 |
|---|---|
| **工具目录 token** | 每多一个工具，`tools/list` 与系统提示都变长。当前 11 个已够用；再加必须证明「智能体否则会答错」，而不是「IDE 里也有所以 MCP 也要有」。 |
| 与 `find_symbol` 表面重叠 | 名字都像「找定义」。文档与 tool description 必须写清：符号表 vs 绑定解析；置信度 `syntactic` vs `exact`。 |
| 与 `find_references` 内部重叠 | 按名查引用时，`resolve_named_site` 已会调 `server.definition` 锚定声明再搜引用。智能体**不能**用「引用列表第一条」代替定义工具——引用含使用点，且空/冷索引语义不同。 |
| 参数形态未对齐 | core 的 `goto_definition` 目前只收 `Position`；MCP 的 `find_references` 收 `symbol` + 可选 `path`（`SymbolAt::Name`）。直接挂 MCP 要么扩 core，要么强制行列——见 §2。 |

### 建议结论

**建议暴露**，但作为精确层第 4 个工具，而不是图谱工具：

1. description 明确写「LSP 绑定解析；不要用 `find_symbol` 代替」。
2. 结果形状与 `find_references` 对齐（`index_root` / `confidence` / 锚点 / budget）。
3. 补上 core 里尚未套上的冷索引守卫（见 §3）——否则空 `exact` 会让智能体以为「没有定义」。
4. 若不做冷索引与 `Unknown` 规则，宁可继续不暴露。

不建议的替代：在 `find_symbol` 里「有 LSP 就升级成 exact」。那会破坏静态路由，并把两种召回混进同一工具名。

---

## 2. 建议的 MCP 参数与结果形状

对齐 `find_references` 的渲染契约（`precise_tools::run_find_references` + `AstrolabeServer::finish`）：

### 参数（草案）

```text
goto_definition
  path: string          # 必填。仓库相对路径；用于选语言服务器（与 LiveBackend 一致）
  symbol: string        # 与 line/character 二选一（见下）
  line: number          # 可选。0-based LSP 行（与 core Position 一致）
  character: number     # 可选。0-based UTF-16 列
  budget_tokens: number # 默认 2500，与其余精确工具相同
```

**定位规则（与 `find_references` 的 Name/Position 对称）：**

- 若同时给出 `line` + `character` → `SymbolAt::Position`（或直接调现有 `goto_definition`）。
- 否则若给出非空 `symbol` → 在 `path` 内取**第一个标识符出现**（复用 `first_identifier_position` / `resolve_named_site` 已有逻辑），再 `definition`。
- `path` 缺失或无法推断语言 → `Unknown` + 现有 `needs_path(...)` 文案，不猜语言、不并行拉多个服务器。
- `symbol` 与行列都缺 / `symbol` 空白 → `Unknown`，非空 note；`isError` 仍为 false。

实现前建议：**把 core `goto_definition` 的签名扩成与 `find_references` 相同的 `SymbolAt`**，避免 MCP 与 queries 两套定位逻辑。扩签名是小改；行为单测已有 Name 路径可参考。

### 文本结果

```text
index_root: /abs/repo
confidence: Exact
@src/lib.rs:12  col 4
…
```

- 首行 `index_root:` 由 `finish` 统一加（与图谱/精确工具相同）。
- `confidence:` 行 + 可选 note；命中用 `@path:line` 锚点（行号 **1-based**，与 `render_location` 一致）+ `col`（0-based UTF-16）。
- 空且 `Exact`：明确写「未找到定义」，**不要**附 `search_code` 误报提示（与 `empty_exact_references_are_not_unknown` 同口径）。
- 空且 `Unknown`：preamble + 安装提示；可建议 `find_symbol`（句法）或 `search_code`（字面），并声明二者不能代替绑定解析。

### `structuredContent`（仅 context 开启时）

与 `find_references` 同形，避免 Claude Code 被残缺 metadata 骗成「空索引」：

```json
{
  "confidence": "exact" | "unknown" | …,
  "budget_tokens": 2500,
  "matches": 1
}
```

默认 context 仍可不发 structured；发则必须连同完整文本（见 `CLIENT_ALIGNMENT.md`）。

### 多定义与仓外

- 多个 in-repo 定义（如接口/实现）：全部返回，按「查询文件优先，再 path/position」排序去重（已有 `finalize_hits`）。
- 全部在仓外（stdlib/deps）：`Exact` + 空列表 + note 点名省略项——**不是** `Unknown`。智能体应读 note，而不是当成「符号不存在」。

---

## 3. 冷索引 / `Unknown` 规则

`find_references` 已有 `guard_cold_index`：服务器 `is_busy()` 且结果为空 → 降为 `Unknown`，中文 note 说明「索引未完成，空结果不可信」。动机：冷 `rust-analyzer` 对 references 回 `[]` 而非 error；标成 `exact` 会让智能体删活代码。

**缺口：** 当前 `goto_definition` **没有**调用 `guard_cold_index`，只 `finalize_hits`。暴露 MCP 前必须补上，规则与引用对齐：

| 场景 | confidence | value | note |
|---|---|---|---|
| 无服务器 / 启动失败 / 超时 / 崩溃 / 协议错误 | `unknown` | `[]` | 原错误 + 安装/重试提示 |
| 服务器 busy 且定义为空 | `unknown` | `[]` | 冷索引不可信（与 references 同文案或略改「定义」） |
| 服务器就绪且定义为空 | `exact` | `[]` | 可写「未找到定义」；**禁止**当成失败 |
| 非空命中（即使 busy） | `exact` | 命中 | 命中真实；busy 时可不升 note，或可选 note「可能仍不完整」——与 references「非空保持 as-is」一致 |
| 仅仓外命中 | `exact` | `[]` | note 列出省略的外部路径 |

原则（与 `ARCHITECTURE.md` §3.4、`CLIENT_ALIGNMENT.md` §5 一致）：

> 空 `Vec` 在 `exact` 与 `unknown` 下意义相反。缺查询能力是可报告状态，不是偷偷换成 `find_symbol`。

---

## 4. 测试计划

不写实现，只定回归门禁；风格跟 `precise_tools` / `lsp/queries` 现有测例。

### core（`lsp/queries.rs`）

1. **冷索引**：busy + 空 definition → `Unknown` + 非空 note（今日缺测，暴露前必补）。
2. **就绪空结果**：非 busy + `[]` → `Exact`。
3. **Name 定位**（若扩 `SymbolAt`）：首个标识符、UTF-16/CJK、部分匹配跳过——可复用 references 的 Name 测例结构。
4. **仓外过滤 / 去重排序**：已有 `goto_definition_normalizes_and_filters`；保持。

### MCP 单元（`precise_tools` + `LiveBackend` fake）

1. catalog：`PRECISE_TOOL_COUNT == 4`，名字含 `goto_definition`，仍无 `apply_rename`。
2. schema：`path` required；`budget_tokens` optional；`symbol` 与行列的约束在文档/校验里测到。
3. unwired / 缺 LSP：`confidence: Unknown`、安装提示、`content[0]` 非空、`isError != true`。
4. exact 命中：`@path:line` 锚点、structured `matches`。
5. exact 空：文案含「未找到」，不含「尚未接通」。
6. budget=0：仍非空文本 + 「因 budget_tokens 被省略」或仅 preamble。
7. 缺 `path`：`needs_path` 类 note。

### 集成 / smoke

- `stdio_smoke`：list 含新工具；一次 happy-path（有语料+假后端或跳过无 LSP 环境）。
- 若有 live LSP CI：与 `live_lsp.rs` 同门禁，定义查询抽一条已知符号即可。

### 文档门禁（接线 PR 同改）

- `README` 工具表、`CHANGELOG`、`ARCHITECTURE` Phase 2 句、`TOOL_COUNT` 注释。

---

## 5. 预估改动面

| 文件 | 改动 |
|---|---|
| `crates/astrolabe-core/src/lsp/queries.rs` | `goto_definition` 接 `guard_cold_index`；建议改为 `SymbolAt`；补冷索引/Name 测例 |
| `crates/astrolabe-mcp/src/precise_tools.rs` | `GotoDefinitionParams`、`run_goto_definition`、`#[tool]`、`PreciseCapability::definition`、测例与 catalog 断言 |
| `crates/astrolabe-mcp/src/live_backend.rs` | 实现 `definition`：选语言、`acquire`、调 core |
| `crates/astrolabe-mcp/src/server.rs` | 挂载 tool → `finish`（自动 `index_root`） |
| `crates/astrolabe-mcp/src/lib.rs` | `PRECISE_TOOL_COUNT: 3 → 4`（`TOOL_COUNT` 随之 12） |
| `crates/astrolabe-mcp/tests/stdio_smoke.rs` | list/可选调用 |
| `README.md` / `CHANGELOG.md` / `docs/ARCHITECTURE.md` | 工具表与「未暴露」表述 |

约 **6–8 个源文件 + 3 个文档**；无新 crate、无协议版本变更。不改图谱工具、不引入常驻 LSP、不加 `apply_rename`。

可选后续（本设计不要求）：

- context `excluded_tools` 示例排除 `goto_definition`（仅当某客户端自带定义跳转且目录 token 敏感时）。
- Codex schema sanitize 落地后，`line`/`character`/`budget_tokens` 的 `integer`→`number` 走同一出口（见 `CLIENT_ALIGNMENT.md`）。

---

## 6. 一句话决策摘要

> **暴露**：精确层只读工具，参数/结果对齐 `find_references`，与 `find_symbol` 按置信度与问题类型分工；**先**给 core 补冷索引 `Unknown`，再挂 MCP。为省 `tools/list` token 而继续隐藏可以，但不要把定义查询塞进 `find_symbol`。
