# 代码走查报告

| 项目 | 内容 |
|------|------|
| 走查日期 | 2026-09-18 |
| 走查范围 | `3dcffad` → 工作区（含已提交与未提交变更） |
| 变更模式 | 接口新增 / 流程变更 / 配置变更 / MCP 客户端适配 / LSP 精确层 |
| 走查人员 | AI代码审查助手 |

---

## 一、走查范围

| 项目 | 内容 |
|------|------|
| 业务功能 | Astrolabe 近期 MCP 客户端对齐、LSP 精确工具（goto_definition / apply_rename 等）、hooks、store/watch、超时与安装文档等 |
| 变更文件 | **约 47 个文件**（含未跟踪脚本；纯文档默认未审） |
| 变更代码 | **+11602 / -247**（numstat 合计量级；完整 diff ~13479 行） |
| 涉及模块 | 后端 Rust（core + mcp）· 配置（Cargo / contexts yml）· 前端/脚本（npm download）· 文档（跳过） |

### 文件清单（摘自 Phase 1 numstat，大文件单独走查）

| 文件路径 | 变更类型 | 模块分类 | 变更说明 |
|---------|---------|---------|---------|
| crates/astrolabe-mcp/src/hooks.rs | Added/Modified | 后端 | hooks 大块新增 |
| crates/astrolabe-mcp/src/server.rs | Modified | 后端 | MCP server 工具注册与路由 |
| crates/astrolabe-mcp/src/precise_tools.rs | Modified | 后端 | LSP 精确工具 |
| crates/astrolabe-core/src/name_path.rs | Added | 后端 | name path |
| crates/astrolabe-core/src/lsp/queries.rs | Modified | 后端 | LSP queries / 超时 |
| crates/astrolabe-core/src/churn.rs | Added | 后端 | churn |
| crates/astrolabe-core/src/trace_tree.rs | Added | 后端 | trace tree |
| crates/astrolabe-core/src/neighborhood.rs | Added | 后端 | neighborhood |
| crates/astrolabe-core/src/watch.rs 等 | Modified | 后端 | store/watch/index/transport 等分批 |
| crates/astrolabe-mcp/src/{cli,context,root,memory,openai_schema}.rs 等 | Added/Modified | 后端 | 客户端上下文与 CLI |
| Cargo.toml / contexts/*.yml | Modified/Added | 配置 | 依赖与客户端上下文 |
| npm/packages/astrolabe/lib/download.js 等 | Modified | 前端/脚本 | 安装下载 |

大文件（>500 增行）独立 Agent：`hooks` / `server` / `precise_tools` / `queries` / `name_path` / `churn` / `neighborhood` / `trace_tree`。

纯 `.md` 文档按 skill 默认跳过（未点名）。

---

## 二、模块概览

### 2.1 模块功能说明

| 项目 | 说明 |
|------|------|
| 主要功能 | MCP 服务端与多客户端上下文对齐；LSP 精确层与超时；hooks / memory / rename 门控 |
| 核心类 | `server` / `precise_tools` / `hooks` / `queries` / `transport` / `root` / `context` |
| 关键方法 | `goto_definition` / `find_references` / `apply_rename` / root 解析 / schema sanitize |
| 数据库表 | 无（本地 redb/store，非 SQL） |

### 2.2 代码结构分析

```
Astrolabe
├── astrolabe-mcp
│   ├── server.rs / precise_tools.rs / hooks.rs
│   ├── cli.rs / context.rs / root.rs / openai_schema.rs
│   └── contexts/*.yml
├── astrolabe-core
│   ├── lsp/{queries,transport,pool,router,diagnostics}
│   ├── name_path / neighborhood / churn / trace_tree / watch / store
│   └── body / group_graph / index
└── npm download / Cargo 配置
```

### 2.3 调用链路

```
MCP client → astrolabe-mcp server → precise_tools / hooks
                → astrolabe-core LSP transport/queries
                → store / watch / graph helpers
```

---

## 三、优点

| # | 亮点 | 说明 |
|---|------|------|
| 1 | hooks 可测性与边界分流 | decide/classify 与 I/O 解耦；切片精读/写重定向边界清晰；deny 静默窗设计合理 |

---

## 四、问题列表

> Phase 2 审查结果到达后追加。分片：large×8 + backend_batch0–9 + config + frontend。

### 分片 hooks（crates/astrolabe-mcp/src/hooks.rs）

#### 优点
1. `decide` / `classify_tool` / `is_slice_read` 与 I/O 解耦，单测覆盖较好。
2. 切片精读、写重定向等边界分流明确。
3. deny 后静默窗 + burst 清零设计清晰。

#### 问题

| # | 等级 | 位置 | 描述 | 修复要点 |
|---|------|------|------|----------|
| H1 | 严重 | hooks.rs:193-195, 692-696 | session_id 未校验，可路径穿越写出/删除 | sanitize session_id |
| H2 | 严重 | hooks.rs:848-878 | 并行 PreToolUse 无锁，计数可丢失永不 deny | 文件锁 + 原子写 |
| H3 | 一般 | hooks.rs:209-216 | save_counter 先截断再写，崩溃可损坏状态 | tmp+rename |
| H4 | 一般 | hooks.rs:929-935 | cleanup 失败仍记成功 | 失败 return 2 |
| H5 | 一般 | hooks.rs 注释 vs 实现 | 注释与实现不一致 | 同步注释 |
| H6 | 建议 | hooks.rs:798-818 | 复合 shell 仅看首命令可绕过 | 拆分或文档边界 |
| H7 | 建议 | hooks.rs:58-68 | READ 含 gc 易误判 | 仅 PS 启用或全名 |

### 分片 backend_batch0（body.rs）

#### 优点
流式按行抽取、契约用例、前缀宽度与 debug_assert 分层合理。

#### 问题

| # | 等级 | 位置 | 描述 | 修复要点 |
|---|------|------|------|----------|
| B0-1 | 一般 | extract_lines 倒挂短路 | 倒挂时不 open，缺文件也 Ok("")，与缺文件必 Err 不一致 | 先 open/try_exists 或改文档 |
| B0-2 | 建议 | 文档 vs 实现 | 注释固定宽5，实现动态 width | 同步文档 |
| B0-3 | 建议 | start_line==0 | 非法锚点被洗成成功抽取 | Err 或 assert |
| B0-4 | 建议 | IO 错误 | 未带 file_abs 路径 | map_err 加 context |
| B0-5 | 建议 | 无体积上限 | 超大区间可塞爆上下文 | max_bytes/max_lines |
| B0-6 | 建议 | 测试临时目录 | 断言失败易泄漏 | TempDir/Drop Guard |

### 分片 neighborhood

#### 优点
BFS+visited 环安全；Import 边口径；确定性排序；边界测试较完整。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| N1 | 一般 | Both 为两次单向 BFS 并集，不能跨方向折返，与 OpenVisio 单次双向 BFS 不一致 | 改单次双向 BFS 或改文档 |
| N2 | 一般 | Both 建邻接表两遍 | 抽取复用 |
| N3 | 建议 | depth≤3 时「收敛到 dependencies」表述易误导 | 改文档 |
| N4 | 建议 | 缺 relation 字段 | 可选增加或标明精简 |


### 分片 queries（lsp/queries.rs）

#### 优点
cold index guard；失败路径明确；hover 解析与降级；测试覆盖关键路径。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| Q1 | 一般 | hover 锚定失败混入 references/search_code 提示 | hover 专用 degrade |
| Q2 | 一般 | 命名 hover 两次 LSP 往返，超时叠加 | 锚定失败回退首标识符 |
| Q3 | 一般 | goto_definition 错误仍用 references 文案 | degrade_definition |
| Q4 | 一般 | cold guard 覆盖仓外 definition 的 external note | 合并 note |
| Q5 | 一般 | 冷索引偏斜单一 definition 锚定后 Exact | 锚定前 cold 判断 |
| Q6 | 建议 | resolve_* 逻辑重复 | 抽取共享 |
| Q7 | 建议 | FakeServer::hover 恒 Null | 可脚本化返回 |


### 分片 backend_batch3（watch.rs）

#### 优点
事件+scan 分层；降级完整；confirm sticky；stop 时 flush pending。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| W1 | 严重 | spawn events 成功分支无条件 check 并丢弃，可吞 spawn 前变更 | 仅未 seeded 做 baseline，或发送 changeset |
| W2 | 一般 | event_loop stop 不 flush debounce 内变更 | 退出前再 check |
| W3 | 一般 | 首次全树 scan 阻塞 spawn 调用线程 | 移入后台线程 |
| W4 | 一般 | notify 无界 mpsc 事件风暴内存风险 | 有界 channel / dirty |
| W5 | 一般 | confirm 恢复可读可能误报 modified | settle 策略 + 断言 |
| W6 | 建议 | 文档仍写 Polling | 改为 Hybrid |
| W7 | 建议 | poll 默认 1s→10s 静默变慢 | changelog/文档 |
| W8 | 建议 | temporary_unreadable 测试弱 | 补断言 |

### 分片 server

#### 优点
finish 统一处理；find_symbol 附体预算；watcher/churn TTL。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| S1 | 严重 | find_symbol_by_path 忽略 include_body | 复用附体逻辑或明确不支持 |
| S2 | 一般 | depth serde default 实际 0 非文档 1 | default_fn=1 |
| S3 | 一般 | rustdoc/clippy 误挂 list_memories | 挪到 trace_calls_tree_body |
| S4 | 一般 | 未知 kind 静默不过滤 | 返回错误列出合法值 |
| S5 | 一般 | neighborhood/group_graph budget 不截断 | truncate_ranked |
| S6 | 一般 | write_memory 失败仍 success | text_error |
| S7 | 一般 | instructions/memories budget 不截断 | 截断或删参数 |
| S8 | 建议 | 首符号体可突破 budget | 与产品确认 |
| S9 | 建议 | Watch 默认 Events | changelog |
| S10 | 建议 | churn 无 singleflight | 互斥 |
| S11 | 建议 | bool env 大小写不全 | ascii_lowercase |


### 分片 backend_batch1

#### 优点
group_graph 纪律；MAX_SYNTACTIC_CALL_TARGETS；open_or_heal。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| B1-1 | 一般 | 跨组边按条+1 低估 weight | 累加 weight.max(1) |
| B1-2 | 建议 | New{n} 测例误导 | 对齐同名断言 |
| B1-3 | 建议 | 阈值跳过无日志 | debug/report |
| B1-4 | 建议 | group_of 重复 RelPath::dir | 复用 |
| B1-5 | 建议 | hover trait 无 default | 提供 default |


### 分片 backend_batch2

#### 优点
hover 贯通；超时 300s；ready_confirmed；Locked 重试。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| B2-1 | 一般 | ready_confirmed 粘住，再 indexing 不等 | progress 再出现时清零 |
| B2-2 | 一般 | diagnostics Timeout 改写成 Protocol | 保留原错误 |
| B2-3 | 一般 | 默认 300s 跃迁偏陡 | 文档/分层超时 |
| B2-4 | 建议 | timeout_from_env 非法静默 | warn |
| B2-5 | 建议 | 重试注释易误导 | 改注释 |
| B2-6 | 建议 | Fake hover 未记 calls | push hover |
| B2-7 | 建议 | documentChanges 资源操作静默空 | 解析或告警 |


### 分片 backend_batch4

#### 优点
手写 CLI 简洁；env 可注入；context 双写法；单测较全。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| B4-1 | 一般 | ASTROLABE_ROOT 空串不回落 `.` | trim 空视为未设 |
| B4-2 | 一般 | `--client=` 空/空白不拒绝 | 与 context 对齐 |
| B4-3 | 一般 | 子命令下 -h 体验差 | 分支处理 help |
| B4-4 | 建议 | context 空格/= 校验不一致 | 统一 |
| B4-5 | 建议 | 多余参数未附 USAGE | 统一 |
| B4-6 | 建议 | CONTEXT var 吞非 UTF-8 | var_os+校验 |
| B4-7 | 建议 | 缺边界测例 | 补测 |


### 分片 churn

#### 优点
降级清晰；路径边界匹配；超时杀进程；单测边界。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| C1 | 一般 | 两次 git 各 10s 最坏 20s | 共享 deadline |
| C2 | 一般 | 集成测试依赖本机 git 历史 | skip/fixture |
| C3 | 建议 | ts>now 仍计入窗 | clamp/跳过 |
| C4 | 建议 | 键未 RelPath 规范化 | 入表前规范化 |
| C5 | 建议 | 超时静默空表 | 日志/可配置 |

### 分片 name_path

#### 优点
注释清晰；parse_query；strictly_contains；chain_matches 跳层；主路径测试全。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| NP1 | 一般 | 绝对路径+同名嵌套贪心就近绑定误拒 | 外→内约束或继续匹配至链顶 |
| NP2 | 建议 | direct_children 注释与实现矛盾 | 改注释 |
| NP3 | 建议 | 排序文档缺 end_line/id | 同步文档 |
| NP4 | 建议 | 尾斜杠排除空容器 | 按 kind 或产品确认 |
| NP5 | 建议 | 大小写折叠策略不一致 | 统一 ascii |
| NP6 | 建议 | 缺同名嵌套回归测 | 补测 |


### 分片 trace_tree

#### 优点
路径级环检测；forest 预算；确定性输出；测试较全。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| TT1 | 一般 | max_nodes 静默截断无标记 | truncated/omitted 标志 |
| TT2 | 一般 | tree/forest 方向分发重复 | 抽 helper |
| TT3 | 建议 | trace_tree 无硬上限 | 默认 max_nodes |
| TT4 | 建议 | roots 重复未去重 | 去重或文档 |
| TT5 | 建议 | 缺 Both+预算截断测 | 补测 |


### 分片 backend_batch5

#### 优点
薄上下文；优先级清晰；structured 默认安全；内置 YAML + 测。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| B5-1 | 一般 | 未知列表键会 brick startup | 跳过未知列表 |
| B5-2 | 一般 | excluded_tools 重复键追加 | 遇键先 clear |
| B5-3 | 一般 | openai 兼容依赖 YAML name | 用 stem/选择器 |
| B5-4 | 一般 | 不接受 flow 序列 | 解析或文档 |
| B5-5 | 建议 | saw_structured 死代码 | 删除 |
| B5-6 | 建议 | 空 tool 名 / Tab 前缀 | 拒绝空名 |
| B5-7 | 建议 | 布尔同义词脆 | 扩展或文档 |

### 分片 precise_tools

#### 优点
apply_rename 重算计划门控；弱置信优先；force 默认拒写；goto 空参拒绝。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| PT1 | 一般 | 注释 PRECISE 仍写 5 | 与常量同步为 6 |
| PT2 | 一般 | 空参仍附 APPLY_REFUSED/force 指引 | 仅校验错误 |
| PT3 | 一般 | get_symbol_info budget 不截断 | truncate |
| PT4 | 一般 | symbol schema 可选运行时必填 | 改为必填 |
| PT5 | 一般 | 预读未走 resolve_dest 逃逸校验 | 复用校验 |
| PT6 | 建议 | 缺行为测 | 补测 |
| PT7 | 建议 | force+空 sites 仍 applied:true | 空站点不应用 |


### 分片 backend_batch7

#### 优点
白名单防穿越；tmp+rename；UTF-8 摘要；单测全。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| M1 | 一般 | Windows 覆写 rename 失败 | 先删再 rename / replace API |
| M2 | 一般 | content 无大小上限 | 设上限 |
| M3 | 建议 | 同名并发共用固定 tmp | pid/随机后缀 |
| M4 | 建议 | list/read/delete 吞真实 IO 错误 | Result/日志 |
| M5 | 建议 | 跟随 symlink | 拒绝 symlink |
| M6 | 建议 | 扩展名大小写漏列 | ascii lower |


### 分片 backend_batch8

#### 优点
integer→number；oneOf 改进；递归消毒；单测主路径。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| O1 | 一般 | oneOf/anyOf 折叠只抬 type 丢约束 | 合并完整子 schema |
| O2 | 一般 | 纯 number 也强制 multipleOf:1 | 仅 had_integer 时补 |
| O3 | 建议 | 键序不规范无法折叠 | sort_keys |
| O4 | 建议 | items 数组等未下钻 | 补 walk |
| O5 | 建议 | type 改写后重复 | 去重 |
| O6 | 建议 | 缺边界回归测 | 补测 |

### 分片 backend_batch6（instructions / lib / live_backend / main）

#### 优点
instructions 分层；TOOL_COUNT 公式；definition 入参分支；main 分流清晰。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| B6-1 | 一般 | main 注释仍写 Thirteen tools 与 lib 21 不符 | 按 TOOL_COUNT 改写 |
| B6-2 | 一般 | 无法识别语言时 note 误说缺 path | 区分缺 path vs 无 Language |
| B6-3 | 建议 | info 缺对称单测 | 补测 |
| B6-4 | 建议 | connection_instructions 未用 context_name | 文档或删参 |
| B6-5 | 建议 | apply_rename 不可逆 vs 回滚表述不一致 | 统一文案 |

### 分片 config（Cargo + contexts yml）

#### 优点
notify 声明一致；yml 契约对齐；structured 默认合理；无敏感默认。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| CF1 | 一般 | readonly 文案承诺硬排除写盘，实际仅 tools/list 过滤 | 改文案或调用路径闸门 |
| CF2 | 建议 | template 对 STRUCTURED 优先级易误解 | 改注释 |
| CF3 | 建议 | readonly notes 中英不一致 | 统一语言 |
| CF4 | 建议 | template 与生产同目录 | 移出/改名 |
| CF5 | 建议 | notify features 未收窄 | 收窄 features |

### 分片 backend_batch9（root.rs + stdio_smoke）

#### 优点
Serena 对齐语义；.git 目录/文件；smoke 更贴近真实启动。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| R1 | 一般 | smoke 断言文案与已建 .git 矛盾 | 改正文案 |
| R2 | 一般 | 就绪判定改成出现 python，空仓变慢失败 | 保留构建失败门闩 |
| R3 | 一般 | find_project_root 测可被祖先 git 误红 | 隔离 TMPDIR |
| R4 | 一般 | 缺 sentinel `.` 主路径单测 | set_current_dir 测 |
| R5 | 建议 | is_cwd_sentinel 匹配偏窄 | 规范化 |
| R6 | 建议 | canonicalize 失败静默 | warn 或 Err |
| R7 | 建议 | 测试清理 | TempDir |

### 分片 frontend（npm download / verify-install）

#### 优点
redirect 剥离 Authorization；host 收紧；Windows 解压在 SHA 后；verify 下限。

#### 问题

| # | 等级 | 描述 | 修复要点 |
|---|------|------|----------|
| F1 | 一般 | Expand-Archive 路径嵌双引号可被 PowerShell 展开 | env 传路径或单引号 |
| F2 | 建议 | 同域 302 也剥凭证 | 仅跨域剥离 |
| F3 | 建议 | 用户直连 S3 时不带 RELEASE_TOKEN | 名单策略调整 |
| F4 | 建议 | releaseTokenFor 与 requestHeaders 漂移 | 单一实现 |
| F5 | 建议 | 未限制 https→http 跳转 | 禁降级 |
| F6 | 建议 | tar 失败回退未清空 destDir | 先清空 |
| F7 | 建议 | verify 只比 >=8 | 工具名子集断言 |

---

## 五、问题统计（去重前按分片合计）

| 等级 | 数量 |
|------|------|
| 致命 | 0 |
| 严重 | 4 |
| 一般 | 54 |
| 建议 | 70 |

### 严重项一览（须优先处理）

| ID | 分片 | 摘要 |
|----|------|------|
| H1 | hooks | `session_id` 未校验，可路径穿越写出/删除 |
| H2 | hooks | 并行 PreToolUse 无锁，计数丢失可导致永不 deny |
| W1 | watch | `spawn` events 成功分支无条件 `check` 并丢弃，可吞 spawn 前变更 |
| S1 | server | `find_symbol_by_path` 忽略 `include_body`，与名称分支能力不一致 |

## 六、安全与兼容

- **路径/写盘**：hooks session_id、memory Windows 覆写、apply 预读逃逸校验不一致、readonly 仅 list 过滤非调用闸门。
- **客户端契约**：budget 参数多处不截断；depth/schema 默认值文档不符；错误文案混用 references/hover。
- **行为变更**：Watch 默认 Events、LSP 超时 300s、poll 默认变长——需 changelog 明示。

## 七、覆盖检查

本次主要为新模块/新工具面，非枚举扩展。结构性遗漏已记入严重/一般（如 `include_body` 路径分支、Both 邻域语义、cold guard 覆盖 external note）。

## 八、测试建议

1. hooks：恶意 `session_id`、并行 PreToolUse 计数。
2. watch：seeded 后 spawn 前变更是否仍发出。
3. find_symbol：路径查询 + `include_body=true`。
4. name_path：`/Dup/leaf` 同名嵌套绝对路径。
5. download.js：Windows 临时路径含 `$` 的 Expand-Archive。

## 九、总结

变更面大（MCP 客户端对齐、LSP 精确层、hooks、graph 辅助、安装下载），整体方向正确且测试意识强。无致命项；**4 项严重**集中在 hooks 安全/并发、watch 变更丢失、`find_symbol` 能力遗漏。建议先修严重项再合入，其余一般/建议可分批。

**结论：修复后可合并**（至少修完 4 项严重）。

---

## 十、修复落地（2026-09-18）

已并修复报告中的 **严重 4** 与 **一般** 项（建议项未批量改）。改动已回写本机工作树；`cargo check -p astrolabe-core -p astrolabe-mcp` 通过；相关单测抽样通过。

分片摘要见 `docs/archive/走查修复摘要/`。

**严重项状态**
- H1/H2 hooks session_id + 计数锁：已修
- W1 watch spawn 吞变更：已修
- S1 find_symbol_by_path include_body：已修

未提交；需要的话再说一声我帮你 commit。
