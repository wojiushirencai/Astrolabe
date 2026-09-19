# Astrolabe 架构

本文记录**有实测支撑的决策**，而不是模块清单。后来人若把这些选择当成口味问题随手推翻，会把已经量过的失败再做一遍。

读者是要改这个仓库的工程师。每个「为什么」后面都跟数字；数字来自源码注释、`README.md` 和 `docs/PERFORMANCE.md`，口径不同的已标明。

## 1. 它解决什么问题

Astrolabe 是给编码智能体用的**确定性代码图谱**，以 MCP server 提供查询。文件级问题走 import 图，符号级精确操作走语言服务器（Phase 2 已实现：`lsp/` + MCP 的 `find_references` / `goto_definition` / `get_diagnostics` / `plan_rename`；Phase 3 gated `apply_rename`）。解析失败和不确定性必须出现在结果里，不能用空列表假装「仓库里没有」。

同一仓库（serena，1028 个文件）上 README 记录的对照：

| 系统 | 峰值内存 | 索引耗时 | 符号数 | 解析失败 |
|---|---:|---:|---:|---:|
| Astrolabe 首次 | 84 MB | 1.0 s | 10051 | 0 |
| Astrolabe 缓存命中 | 31 MB | 0.10 s | 同上 | 0 |
| OpenVisio（Node） | 4566 MB | 12.6 s | 4979 | 205 |
| Serena（Python + LSP） | 373 MB | 5.7 s | — | — |

回归门禁用的不是这张表。`docs/PERFORMANCE.md` 测的是独立 worker 里的 `index_repo()`，serena 稳态中位约 **30 MB / 90–160 ms / 10051 符号**。差额来自进程边界和冷/热启动，不是「又快了十倍」的产品承诺。改性能数字之前先读那份文档。

## 2. 四条设计规则

`astrolabe-core` 的 `lib.rs` 把先前系统里量出来的失败收成四条。改核心路径时对照它们，而不是另发明一套「先做完再补错误处理」。

1. **解析失败不得静默丢失。** OpenVisio 的 `catch {}` 在真实仓库上丢掉约 20% 的文件且不通知调用方（`parse/mod.rs` 记的是 205 / 1026）。失败必须进入 `IndexReport`。
2. **解析器资源显式释放。** WASM 实现按文件分配 Parser、不释放 Tree，1200 文件仓库峰值 RSS 到 5 GB；两个文件的仓库仍要 1.4 GB。这里用按语言复用的 `ParserPool`，`Tree` 用完即丢。
3. **缓存必须有字节上限。** 按条目数封顶说明不了内存。`FileCache` 用 weigher 按字节计权；进程稳态上限是产品承诺，不是内部调参。
4. **说不出就标 `unknown`，不要猜。** 「没找到」和「没查」必须是不同类型。见 §5。

另外两条测量过、写在别的模块里，同等效力：

- **超时必须真的打断解析。** 用同步 parse 对赛一个定时器，定时器只会在 parse 结束后响。先前系统因此打出 913 条「超时」日志，文件其实已经解析成功。本仓库走 tree-sitter 的 progress/cancellation。
- **写存储必须有批次上限。** 先前系统把逐条同步写换成无界异步队列，索引快 5.7×，稳态 RSS 从 807 MB 涨到 4049 MB。2048 条一批的事务把速度保住、RSS 停在 883 MB。`store::WRITE_BATCH = 2048`。

## 3. 核心架构决策

### 3.1 按查询粒度分层，而且是静态路由

**测量（对标语言服务器）：**

| 问题类型 | 方法 | 召回 |
|---|---|---|
| 文件级影响 / 依赖 | import 图 | **100%** |
| 符号级引用（名字匹配调用图） | 按标识符连边 | TypeScript **66%**，Python **18%** |

所以：

- 文件级问题（依赖、邻域、影响面、热文件、任务相关骨架）**只走图谱**。
- 「这个符号实际被谁引用 / 能否安全改名」**只走语言服务器**。
- `trace_calls` 暴露的是名字匹配调用边，置信度固定为 `syntactic`，调用方必须核验。

**为什么不能「图谱不行再回退 LSP」。** 图谱几乎不会以 Err 失败。名字匹配总会连上一些边，只是 TypeScript 漏三分之一、Python 漏五分之四。动态回退看到的是「有结果」，察觉不到这种沉默的不完整。错的引用一旦变成编辑，代价在用户的编译器里，不在我们的测试里。

因此路由键是**问题类型**，在查图之前就定死，而不是看返回值再决定。`lsp/mod.rs` 把这写成约束：缺语言服务器是可报告状态（`Confidence::Unknown`），不是偷偷换成名字匹配再标成精确。

当前实现状态：文件级已经落地。符号级 LSP 已接线：按需启动、空闲回收，MCP 走 `LiveBackend`。路由仍是按问题类型静态分流，不得改成「先图后 LSP」的动态回退。

### 3.2 不常驻语言服务器

Serena 上四次全库引用搜索，语言服务器子进程从 **186 MB 涨到 719 MB**，之后不回落。增长的 **94% 在宿主进程外**，宿主自己的内存上限看不见它。

后果：

- 服务器按需启动，空闲或超预算即回收（`LanguageServer::memory_bytes` 就是给池子用的）。
- 图谱工具不得阻塞在 LSP 启动上。真实服务器启动加索引是秒级；文件级查询必须在这期间仍然可答。
- 这不是「以后再加常驻缓存」的待办。常驻是已经量过的失败模式。

### 3.3 导入解析的全部成本就是读构建配置

五语言、3012 条仓库内 import（语法提取口径，见 §8）。按条数加权，先前方案大约 **15%**（12%×1577 + 0%×31 + 0%×984 + 23%×209 + 100%×211 ≈ 448 / 3012）。读完构建配置后目标是 **100%**。

| 语言 | 语料 | 仓库内 import | 先前 | 目标 | 先前失败在哪 |
|---|---|---:|---:|---:|---|
| Python | serena | 1577 | 12% | 100% | 只从仓库根找；serena 是 src-layout，`solidlsp` 在 `src/` |
| Go | gin | 31 | 0% | 100% | 拿完整模块路径去对仓库相对路径，永远对不上 |
| Java | gson | 984 | 0% | 100% | `com.google.gson.Gson` 按根目录拼，文件实际在 `*/src/main/java` |
| Rust | ripgrep | 209 | 23% | 100% | 连字符 crate 名、`crate::` 锚点、类型段回退三条全错 |
| TypeScript | openvisio-oss | 211 | 100% | 100% | 相对路径本来就能做；还要补 tsconfig `paths`、workspace 包 |

每门语言的成本都具体，而且已经付过：

- Python：`pyproject.toml` / `setup.cfg` / `setup.py` 的 package-dir；**仓库根必须作为最后一个 source root**，否则 serena 约 190 / 1577 条（测试、脚本）会丢。
- Go：每个 `go.mod` 的 `module` 前缀；import 指向包目录，图是文件级，所以要在目录里挑一个 `.go` 文件当代表。
- Java：gson 语料上有 12 个 Maven 惯例源根；再加 pom / Gradle `srcDirs` 和 package 声明回退。
- Rust：workspace 成员、连字符→下划线、`crate`/`self`/`super`、末段从类型回退到模块文件、`#[path]`。
- TypeScript：相对路径 + 扩展名推断、最近 `tsconfig` 的 `paths`/`baseUrl`、workspace `package.json`。

`detect` 可以读盘，`resolve` 必须纯、且不碰磁盘。构建配置进 `ProjectMeta` 一次，之后全是索引查找。

`None` 表示「不在本仓库」（标准库、第三方），这是正确答案，不是缺口。只有「看起来像仓库内」却解析失败的说明才进 `IndexReport::unresolved_imports`。

### 3.4 置信度是一等公民

```text
exact  >  scoped  >  syntactic  >  unknown
```

| 值 | 含义 | 典型来源 | 智能体应如何对待 |
|---|---|---|---|
| `exact` | 编译器或语言服务器，或磁盘字面匹配 | LSP 引用；`search_code` 扫到的源码行 | 可以当事实 |
| `scoped` | 构建配置 + 词法/模块结构 | import 边；`get_dependents` | 文件级可信；不是类型检查 |
| `syntactic` | 名字匹配，可漏报也可过报 | 调用边、`find_symbol`、`trace_calls` | 必须用锚点核验 |
| `unknown` | 没查或查不了 | 未安装 `gopls` | **禁止**当成「零引用」 |

`Precise<T>` 存在的唯一理由：空 `Vec` 在 `exact` 和 `unknown` 下意义相反。改名计划低于 `scoped` 不得自动落盘（`RewritePlan::is_auto_applicable`）。把置信度从结果里拿掉，等于把 66% / 18% 的调用图伪装成精确答案。

### 3.5 整图快照不作为跨会话增量基础

`FileId` 是当前解析文件集上的稠密序号：解析结果按路径排序后 `FileId(i)`。增删一个文件，后面所有 id 都会错位。边的 `from`/`to` 是裸 `u32`，import 边连文件、调用边连符号，解释完全依赖当次图。

因此：

- **持久化的是「按内容哈希的解析结果」**，不是带 `FileId` 的整图。键是 `language:sha`（FNV-1a），未改动的文件跳过 tree-sitter。
- `Store::put_files` / `put_symbols` / `put_edges` 存在，但 `index_repo` 的热路径不写它们。冷启动按 gopls 的教训也不该反序列化整图——那一处架构变化给 gopls 省回 53–83% 内存。
- 跨会话增量的单位是文件内容，不是 id。二次索引仍走完整的 resolve + 建图；省下的是 parse。

若有人把「把 CodeGraph 整份 mmap 回来」当成优化，先解释 id 在文件集变化后如何保持稳定。现在没有这套稳定 id。

## 4. 数据流

```text
                    ASTROLABE_WATCH_SECS（默认原生事件，0 关闭，N 轮询秒数）
                    ┌──────────────────────────────────┐
                    │  Watcher: 原生事件驱动 + 快照 diff  │
                    │  故障自动降级 10s 轮询 / 600s 兜底 │
                    └────────────┬─────────────────────┘
                                 │ ChangeSet
                                 ▼
根目录 ──► scan ──► FileIndex ──► ResolverSet::detect ──► apply_excludes
              │                      │
              │                      │ ProjectMeta（源根 / crate / paths）
              ▼                      ▼
         并行 parse ◄── Store 解析缓存（.astrolabe/index.redb，按 language:sha）
              │         ParserPool（每语言一把 Parser）
              │
              ▼
         按路径排序，分配 FileId / SymbolId
              │
              ├── resolve import ──► EdgeKind::Import, Confidence::Scoped
              └── 可选 call_edges ──► EdgeKind::Call,  Confidence::Syntactic
                                 │
                                 ▼
                            CodeGraph
                                 │
              ┌──────────────────┼──────────────────┐
              ▼                  ▼                  ▼
     静态 PageRank         rank_for_task        MCP 工具
     （可缓存）            （任务个性化）         FileCache（search_code 热路径）
```

要点：

- **scan → parse → resolve → graph → 查询** 是唯一建图路径。MCP 不另写一套抽取。
- `detect` 在首次 scan 之后，因为它要 `FileIndex`。构建系统声明的输出目录（Cargo `target`、tsconfig `outDir`）此时才知道，所以 `apply_excludes` 是内存过滤，不是二次走盘。
- **store** 在 parse 两侧：命中则跳过 tree-sitter；未命中则编码写回。打不开、只读、坏掉都只记日志，索引继续走内存。
- **cache**（`FileCache`）不参与建图。它是 MCP `search_code` 的读侧热层，按路径 + mtime，字节预算默认 256 MB（`ASTROLABE_CACHE_MB`）。README 记录：8 MB 预算下 RSS 停在约 45 MB 平台、16 轮查询不涨；1–2 MB 预算下缓存占用贴住上限（误差 <100 字节）。
- **watch** 在 MCP 第一次索引成功之后启动。重建期间**旧图继续服务**；重建失败丢弃新结果。一次坏编辑不能把可用索引换成错误。默认采用**操作系统原生事件驱动（`notify`）唤醒快照 diff**，空闲时 0% CPU 占用；遭遇平台通知故障或后端错误时自动平滑降级为 **10s 轮询兜底**，并辅以 600s 周期性安全网扫描。大批量事件经 500ms 防抖合并，重建侧自动清空积压，避免持续变更产生冗余重建风暴。环境变量 `ASTROLABE_WATCH_SECS=0` 可彻底关闭监视，指定正整数 `N` 可强制使用 `N` 秒纯轮询模式。

调用边开关：

- `IndexOptions::default().call_edges = false`。基准和库默认关掉。
- MCP 的 `index::build` 打开它，因为 `trace_calls` 需要这些边。打开后边数从约 1531 涨到 98154（README，约 64 倍），既漏也过报。这是产品上接受噪声、并用 `syntactic` 标出来，不是「默认就该开」。

## 5. 模块地图

Workspace：`astrolabe-core`（图谱库）← `astrolabe-mcp`（stdio MCP）。依赖只允许这个方向。

### 5.1 `astrolabe-core`

| 模块 | 职责 | 关键类型 | 依赖方向 |
|---|---|---|---|
| `types` | 跨工作流契约。视为冻结 | `RelPath`, `Language`, `FileId`, `SymbolId`, `Confidence`, `FileIndex`, `ProjectMeta`, `ModuleResolver`, `CodeFile` / `CodeSymbol` / `CodeEdge` | 被所有人依赖，不依赖本 crate 其他模块 |
| `scan` | gitignore、默认排除、大小、二进制嗅探 | `ScanOptions`, `ScanResult`, `SkipReason` | `types` |
| `parse` | tree-sitter、符号/import/call 抽取、Parser 池 | `ParserPool`, `ParsedFile`, `ParseError` | `types`；queries 内嵌 `.scm` |
| `parse::queries` | 每语言 symbols/imports/calls | `LanguageQueries` | 符号召回门槛 ≥90% |
| `resolvers` | 五语言模块路径 → 仓库文件 | `ResolverSet`；每语言一个无字段 unit struct | `types`；语言之间禁止互调 |
| `graph` | import 图、PageRank、任务排序、可达性 | `CodeGraph`, `Centrality` | 只依赖 `types` |
| `index` | 编排 scan→parse→resolve→graph | `IndexOptions`, `index_repo` | scan / parse / resolvers / store / graph |
| `store` | redb；解析缓存；整图表预留 | `Store`, `WRITE_BATCH`, `SCHEMA_VERSION` | 不进入查询热路径 |
| `cache` | 有界内存文件缓存 | `FileCache`, `CacheConfig` | 被 MCP 使用 |
| `watch` | 原生事件唤醒 + 防抖快照 diff + 轮询兜底 | `Watcher`, `ChangeSet`, `WatchHandle` | 复用 `scan`，保证监视集 = 索引集 |
| `budget` | 按排名截断工具输出 | `TokenBudget`, `truncate_ranked` | 估计是 `chars/4` |
| `render` | `path:line` 锚点、置信度附注 | `anchor`, `symbol_line`, `confidence_note` | 给智能体读，不是给人看的 UI |
| `lsp` | Phase 2 已实现：按需 LSP、精确查询 | `LanguageServer`, `Precise<T>`, `LspError` | 图谱问题不得进入此模块 |
| `rewrite` | Phase 3 计划层：先计划后写入、低于 scoped 不自动应用 | `RewritePlan`, `ScopeResolver`, `Evidence` | JS/TS 走 oxc；**哪些位点**由本模块决定。写入经 gated `apply_rename` 接到 MCP |

`lsp/` 子模块（`discovery` / `transport` / `pool` / `queries` / `diagnostics` / `router`）已落地。`rewrite/` 的 `engine` 与 `scope_js`/`scope_ts` 能产出 `plan_rename`；MCP `apply_rename` 已 gated 接入（低于 scoped 默认不写；`force` 可强制）。约束不变：不常驻、不静默降级、计划与写入分离、写后重解析、失败回滚。

### 5.2 `astrolabe-mcp`

| 模块 | 职责 |
|---|---|
| `main` | 根目录优先级：位置参数 → `ASTROLABE_ROOT` → cwd；stdio；日志在 stderr |
| `lib` | `TOOL_COUNT = 13`（8 图谱 + 5 精确），`tools/list` 的 24h `ttlMs` |
| `index` | 对 `index_repo` 的薄封装；**打开 `call_edges`** |
| `server` | 十三个工具、索引状态机、watch 重建、`content[0]` 非空文本 |
| `precise_tools` | 精确层渲染：`find_references` / `goto_definition` / `get_diagnostics` / `plan_rename` / `apply_rename`（gated） |
| `live_backend` | 按需 `LspPool` + rewrite planner；缺服务器标 `unknown` |

工具与置信度：

| 工具 | 走哪一层 | 标注 |
|---|---|---|
| `resolve_context` | 任务个性化 PageRank + 邻域符号 | `scoped` |
| `get_repo_skeleton` / `get_hotspots` | 静态 import 中心性 | `scoped` |
| `get_dependents` | import 可达性 | `scoped` |
| `get_languages` | 图上的语言/LOC 计数 | `exact` |
| `search_code` | 磁盘源码字面匹配 | `exact` |
| `find_symbol` | 符号名子串 | `syntactic` |
| `trace_calls` | 名字匹配调用边 | `syntactic` |
| `find_references` | 语言服务器引用 | `exact`；缺服务器 `unknown` |
| `goto_definition` | 语言服务器定义（绑定解析） | `exact`；缺服务器 / 冷索引 `unknown` |
| `get_diagnostics` | 语言服务器诊断 | `exact`；缺服务器 `unknown` |
| `plan_rename` | rewrite 计划，不落盘 | 随证据；低于 `scoped` 不可自动应用 |
| `apply_rename` | 事务写盘 + 写后重解析；失败回滚 | 默认仅 `exact`/`scoped`；否则需 `force=true`（结果含警告）；不可逆 |

Cursor 会静默丢弃只有 `structuredContent`、没有文本的工具响应。每条响应的 `content[0]` 必须是非空文本。这是测过四个客户端之后的硬规则，不是格式偏好。

### 5.3 图算法（改排序前先读）

- PageRank：阻尼 0.85，**固定 40 轮**，顺序累加。不用收敛阈值，避免跨运行最后几 bit 翻转导致名次变化。
- 任务个性化借 aider 的权重：对话里出现的标识符约 10×，长而具体的名字再 10×，已在上下文中的文件约 50×。静态中心性可缓存；个性化是另一次带 restart 向量的 PageRank，不改缓存。

## 6. 已知边界

这些不是「待优化列表」。它们是测量过、目前故意停在这里的限制。

**调用边默认在库里关闭。** README：开启后边数 1531 → 98154（64 倍）。名字匹配既漏动态分派，也把同名符号连在一起。MCP 为了 `trace_calls` 打开它，但每条边和工具输出都标 `syntactic`。不要为了让调用图「看起来密」就去掉这层标注，也不要把这 64 倍写进与 `call_edges=false` 对打的性能基线。

当前 serena 性能基线（调用边关）是 **1529** 条 import 边、10051 符号、0 解析失败。1531 / 1529 的差是测量窗口里的漂移，不是两种算法。

**整图不持久化。** 原因在 §3.5：`FileId` 随文件集分配。增量的正确粒度是内容哈希后的 `ParsedFile`。

**跨文件作用域分析只有 JS/TS 有成熟方案。** workspace 依赖了 oxc（parser + semantic）。`rewrite::scope_js` / `scope_ts` 是给这条路径留的。Python / Go / Java / Rust 没有同等可用的、许可证合适的进程内语义分析可塞进本 crate。那些语言的安全改名走 LSP（`Evidence::LanguageServer`），不要用名字匹配冒充 `ScopeBinding`。ast-grep 自己的文档写明它不做作用域、类型或数据流；**改哪些位点**不能委托给匹配器。

**符号召回仍有已知残差**（`parse/mod.rs`，不用来缩小分母）：

- Java 注解类型元素尚未进 query。
- 行扫描会把部分字段 / 嵌入类型 / 宏噪声算进 ground truth，解析器名称与扫描器不一致时计漏报。

这些残差已经包含在 README 的召回数字里（Python 100.00%、Rust 99.41%、Go 99.29%、Java 96.51%、TypeScript 93.97%）。门槛是 ≥90%。把 query 改「更少」如果打掉召回，先查漏报，不要把门槛改成 80%。

**验收测试的 Python 解析阈值在代码里是 0.95，契约是 100%。** `tests/import_resolution.rs` 里 Python 写的是 `threshold: 0.95`，其余四门是 `1.0`。产品与 README 的验收契约是五门都 100%。0.95 不是允许退步的空间。

**CI 默认不跑五语言验收。** `.github/workflows/ci.yml` 在 push/PR 上跑 fmt、clippy `-D warnings`、`cargo test --workspace`、`cargo deny`。`scripts/verify.sh` 需要 gitignore 掉的 `corpus/{go,java,rust}` 以及同级的 `serena` / `openvisio-oss`。workflow_dispatch 的 `corpus` job 只 clone Go/Java/Rust。改解析器却只看 GitHub 绿勾，不够。

**Phase 2 已接线；Phase 3 写入经 gated `apply_rename` 接到 MCP。** MCP 暴露 `find_references`、`goto_definition`、`get_diagnostics`、`plan_rename`、`apply_rename`（`TOOL_COUNT=13`）。默认低于 `scoped` 拒绝写盘；`force=true` 可强制。继续遵守：不常驻、不静默降级、计划与写入分离、写后重解析、失败回滚。

## 7. 动这些决策会付出什么

| 若你打算… | 先面对的数字 |
|---|---|
| 用名字匹配调用图回答「谁引用了这个符号」 | TS 召回 66%、Python 18%；错编辑 |
| 图谱无结果再回退 LSP | 图谱会给出不完整结果，回退条件永远不触发 |
| 把语言服务器常驻在 MCP 进程旁 | 四次全库搜索 186 → 719 MB，且在 RSS 上限盲区 |
| 不读 pyproject / go.mod / Maven / Cargo / tsconfig | 仓库内 import 从 ~15% 掉回先前方案 |
| 去掉置信度，只返回列表 | 空列表无法区分「零引用」和「没装 gopls」 |
| 把 `FileId` 图快照当增量基础 | 文件集一变，所有边的端点失效 |
| 解析失败改成 skip | 分母变小，出现 209/209 全绿但丢了一整个源文件的那种假阳性 |
| 无界写队列 / 无界符号缓存 | RSS 从百 MB 进 GB |
| 为让 CI 变绿而降低 100% / 90% 阈值 | 回归被定义成了成功 |
