# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

crates.io / npm 尚未发布；tag 与 release 记录见 GitHub Releases。当前 workspace 版本见 `[workspace.package]`。

## [Unreleased]

### Added

- **`astrolabe hooks gc` 与 remind 内置过期会话 GC。** 删除 `~/.astrolabe/hook_data/` 下超过 24 小时（`ASTROLABE_HOOK_GC_HOURS` 可调；`0` 关闭 remind 中的机会式 GC）未活动的会话目录（以目录树内文件最新 mtime 判定，当前会话永不删除）。`hooks remind` 借助 `~/.astrolabe/hook_gc.stamp` 节流，每小时至多扫描一次。用于无 `SessionEnd` 事件的宿主（ZCode）与会话异常退出的兜底清理。

### Fixed

- **ZCode 接入文档纠正**（Refs #30）。ZCode 仅支持 7 个 hook 事件且严格校验，配置 `SessionEnd` 会导致整段 hooks 加载失败；README 与 `docs/CLIENT_ALIGNMENT.md` 改为给出 `~/.zcode/cli/config.json`（Windows `%USERPROFILE%\.zcode\cli\config.json`）的正确结构（`enabled: true` + `events` 包装层，`PreToolUse` 调用 `astrolabe hooks remind --client=zcode`），明确不配 `SessionEnd`、不在 `Stop` 上挂 cleanup，清理交由过期 GC。

## [0.9.0] - 2026-10-08

### Changed

- **防漂移 Hook 切片读从"无限豁免"改为"连续额度制"，deny 文案收紧为直接切换指令。**
  - 带 `limit ≤ ASTROLABE_SLICE_READ_MAX`（默认 200）的 Read 切片读不再零计数完全放行（实测漂移会话出现过 259 次切片读零拦截，计数器被架空），改为连续额度制：连续 10 次（新环境变量 `ASTROLABE_SLICE_READ_THRESHOLD` 可调）**代码文件**切片读且期间无任何 Astrolabe 工具调用即触发新增的 `DenyKind::Slice` 提醒式拦截；非代码文件（未索引语言 / markdown 等）切片保持中立不计数（与全量读口径一致，把它们推向 search_code 没有意义）；任何 Astrolabe 工具调用（含 `search_code` / `get_diagnostics` 等非符号工具，新增 `ToolKind::AstrolabeNonSymbolic` 分类，只清零切片额度、不动 grep/read 计数）都会清零该额度；调用间隔超 2000 秒自动重置；`head -n N` / `tail -n N` / `sed -n 'A,Bp'` 等有界切片 shell 打印保持中立不计数。计数状态新增 `n_slice` / `last_slice_ts` 字段（serde 兼容旧 counter.json）。
  - Grep / Read / Mixed 三类 deny 的 reason 与 additionalContext 删除 "You can continue using ... the counter was reset" 软口径，替换为明确切换指令 "Stop using grep/read for code discovery. Call mcp__astrolabe__search_code or mcp__astrolabe__find_symbol now (read-only, safe)."；共用只读声明同步改为切片额度制准确口径（slice reads 仍可用，连续超阈值触发提醒，未索引语言逃生口保留）。

## [0.8.0] - 2026-10-07

### Added

- **防漂移 Hook 客户端全面扩展（Cursor CLI、Grok Build、Kimi Code、ZCode、OpenCode）。**
  - **Cursor CLI 适配**：新增 `Client::Cursor`（别名 `cursor` / `cursor-agent` / `cursor_agent` / `cursor-cli`），原生输出 Cursor 扁平 deny 格式（`{"permission":"deny","user_message":...,"agent_message":...}`），长引导警告注入 `agent_message` 直达模型；PreToolUse payload 会话识别支持 `conversation_id` / `conversationId` fallback，无缝对接 Cursor 原生 `preToolUse` 事件。
  - **Grok Build 配方与别名**：完善 Grok Build（`xai-org/grok-build`）规范支持（别名 `grok` / `grokbuild` / `grok-build`），输出一等公民扁平格式 `{"decision":"deny","reason":...}`，提供 `~/.grok/hooks/astrolabe.json` 配方、项目级 `/hooks-trust` 授信说明及工具 matcher 双命名建议。
  - **Kimi Code CLI 适配**：新增 `Client::KimiCode`（别名 `kimicode` / `kimi-code` / `kimi_code` / `kimi`），协议与 Claude Code 同构，工具识别覆盖 `Read` / `Grep` / `Bash`，输出三字段 `hookSpecificOutput` 结构化决策。
  - **ZCode 适配**：新增 `Client::ZCode`（别名 `zcode` / `z-code` / `zai`），协议与 Claude Code 同构，输出三字段 `hookSpecificOutput` 结构化决策。
  - **OpenCode 插件桥接**：新增 `Client::OpenCode`（别名 `opencode`），输出扁平 `{"decision":"deny","reason":...}`；提供 `.opencode/plugins/astrolabe-guard.ts` TS 插件模板，通过 `tool.execute.before` 钩子拦截并抛出错误。

## [0.7.0] - 2026-10-07

### Fixed

- **防漂移 Hook 计数按子代理 `agent_id` 分片，修复并发子代理互相清零/消音。**
  - PreToolUse payload 携带 `agent_id` / `agentId`（并发子 Agent 场景才有，主线程不含）时，计数状态落盘到 `~/.astrolabe/hook_data/<session_id>/<agent_id>/counter.json`，各子代理独立计数；此前所有子代理共享同一 counter，任一子代理调用符号工具即全局清零、任一 deny 的静默窗消音全部子代理，3 次阈值在并发下永远无法稳定触发。
  - 分片名不合法（路径穿越、超长、非法字符等）时回退 FNV-1a 64 位哈希分量（`ag-<hex>`），防路径穿越；字段缺失或类型不符时静默回退会话级平面路径，尽力而为不报错。`SessionEnd` 清理仍按会话目录整体递归删除（含子代理分片）。

### Changed

- **deny 后静默窗默认从 120 秒缩短为 15 秒**（`ASTROLABE_DENY_SILENCE_SECS` 可覆盖），降低单一 deny 误消音后续合法排查的时长。

## [0.6.0] - 2026-10-05

### Added

- **C# (.cs), C# script (.csx), Razor and cshtml, and Visual Basic (.vb) syntactic graph.**
  - C# (`.cs`) and C# script (`.csx`).
  - Razor and cshtml: C# extracted from `@code` and `@using`. No Razor language server.
  - Visual Basic (`.vb`).
  - Optional Roslyn via `ASTROLABE_LSP_CSHARP` (`dotnet` plus `Microsoft.CodeAnalysis.LanguageServer.dll`, no auto-download).

### Known Limitations

- Official Roslyn packages do not ship Visual Basic assemblies, so VB go-to-definition was not verified.
- No Windows test environment.

## [0.5.0] - 2026-10-03

### Added

- **Dart（.dart）语言全链路代码图谱与精确语义支持。**
  - **Tree-sitter 语法解析**：集成 `tree-sitter-dart 0.2`（ABI 兼容 host 0.27，141 个真实文件验证 0 ERROR 语法节点）。抽取 class、enum、mixin、function、getter、setter 等核心符号；识别 `_` 前缀私有成员命名约定并准确设置 `is_exported` 状态；完整捕获 `library_import`、`export`、`part` 及 `part of` 导入导出关系。
  - **Pubspec 与依赖解析器（Resolver）**：实现针对 `pubspec.yaml` 的零依赖行级解析（列 0 严格锚定 `name:`，彻底规避依赖块内的嵌套 `name:` 陷阱），建立包名到 `lib/` 源码目录的精确映射；解析 `package:<pkg>/...`、相对路径与 `part` 指令；将 `dart:` SDK 库与外部第三方包识别并返回 `None`（排除于仓库内图谱）；支持包含多个 `pubspec.yaml` 的 monorepo 结构，同名包冲突按路径深度取最浅者；在扫描（scan）、文件监听（watch）与解析器排除（resolver excludes）三层防御式硬排除 `.dart_tool/`。
  - **按需 LSP 与 dart language-server 调度**：接入随 Dart / Flutter SDK 自带的 `dart language-server`（prompt-only 交互提示，无需独立下载分发矩阵）；支持环境变量 `ASTROLABE_LSP_DART` 覆盖可执行文件路径；LSP `initialize` 请求中将 `rootUri` 显式置 `null` 并保留 `workspaceFolders`（规避 oraios/serena#2045 在 monorepo 下触发全盘重复分析与死循环空转）；注入 `initializationOptions` 四大配置项（`onlyAnalyzeProjectsWithOpenFiles: false`, `closingLabels: false`, `outline: false`, `flutterOutline: false`）；挂载 `$/analyzerStatus` 与 `experimental/serverStatus` 就绪通知映射（实测 Dart 3.13 走标准 `$/progress`，映射机制作为老版本 analyzer 兜底）；集成测试完成真实 definition 跳转验证。
  - **重写支持与语法验证**：维护包含 67 个 Dart 关键字的 `DART_KEYWORDS`（字母序 binary_search 快速检索），重命名标识符允许 `$` 符号，开启 rename 后的 verify re-parse 语法校验。
  - **实测性能与准确率**：在真实大型混合仓库 AI-con（Flutter + Go monorepo）中实现仓库内 Dart import 100% 精确解析；204 个 `.dart` 文件、1900 个符号在 885ms 内完成冷索引，峰值内存仅 93MB。

- **显式语言声明与未覆盖诊断机制。**
  - 工具元数据（description）全面声明当前支持的语言清单，帮助宿主客户端与模型在规划阶段明确能力边界。
  - `search_code`、`find_symbol` 零命中以及 `get_dependents` 目标未找到时，主动返回“目标可能属于未支持语言”的显式诊断提示，消除模型误判“代码不存在”的歧义与空转。
  - `get_languages` 扩展输出当前已支持（supported）的语言清单及未索引（unindexed）代码文件统计。

### Changed

- **防漂移 Hook 拦截口径收敛为支持语言集。**
  - PreToolUse Hook 的全量源码文件 Read 拦截依据从 58 种扩展名全集收敛为 Astrolabe 实际支持的 28 项语言扩展名（`SUPPORTED_CODE_EXTENSIONS`），避免对未支持语言（如 `.kt`、`.scala`、`.rb`）的常规阅读产生误拦截；原 58 项 `KNOWN_CODE_EXTENSIONS` 完整保留作为未索引语言的统计分析口径。
  - `grep` 与 `mixed` 的 deny 拦截提示文案中增加“未支持语言例外”说明（若目标文件属于 Astrolabe 未索引语言，grep 为正确工具，可忽略此提醒）。

- **持久化存储向后兼容。**
  - 存储层在枚举末尾追加 `Language::Dart` 映射值（`u8 = 14`），严格保证既有 Redb 索引的向前与向后二进制兼容。旧版本二进制读取包含 Dart 文件的新索引库时，语言标签安全退化为 `None`，文件节点与元数据正常保留。

### Known Limitations

- **调用链分析（trace_calls）暂未开放**：Dart 语法层面的调用边（call edges）在 v1 中返回空结果，计划后续版本支持。
- **构建生成代码未排除**：由 build runner 生成的 `.g.dart`、`.freezed.dart` 等文件当前仍会进入索引，等待后续 glob 排除机制支持。
- **冷启动 Settle 窗口与首查询兜底**：`dart language-server` 冷启动耗时（实测约 3.2s）略超当前统一的 3.0s LSP settle 静默窗口，首次查询由 `didOpen` 触发的优先即时分析机制兜底保障正确性。
- **测试语料特定场景**：在包含多个 example 子项目的复杂语料 `flutter/packages` 中，同名包嵌套场景下的 import 解析率为 99.66%，待后续语料专项跟进。

## [0.4.1] - 2026-10-01

### Fixed

- **`search_code` 遵循调用方预算，消除误卸载与假死截断。**
  - **动态资源阈值（遵循模型意图）**：MCP resource 卸载阈值由写死 4000 改为动态计算 `max(基线4000, 钳制后budget_tokens)`，并增加 128 tokens 的 SLACK 双保险；预先扣除头部开销（35 tokens），彻底根治调用方加大 `budget_tokens`（如 5000）后反而因超过 4000 被降级为 resource 链接的问题。
  - **单行超长安全切片（UTF-8 边界安全）**：单行代码超长（>250 字符，如压缩的 `min.js` / 单行 JSON）时，以匹配项为中心截取前后各约 90 字符的安全窗口，并对匹配段长度进行封顶，避免单个压缩单行吃光几千 token 预算或导致后续匹配被全弃；定位基于原串的 `regex::Match`，杜绝 Unicode 小写变换造成的字节偏移错位 panic。
  - **前瞻匹配摘要**：当工具结果确需转存为 MCP 资源时，在首个文本块中展示前 5 行有界前瞻摘要（≤200 tokens），让模型无需调 `resources/read` 即可获知匹配概貌。
  - **零展示明确诊断**：当 `shown=0 && omitted>0` 时，如实提示首条匹配单项超过预算，指导收紧 `path_filter` 或调整参数。
  - **`trace_calls` 树结构预算防护**：对深层树形调用链渲染增加 `budget_tokens` 截断与省略提示。
  - **环境变量旋钮**：支持通过 `ASTROLABE_RESOURCE_THRESHOLD`（基线覆盖）与 `ASTROLABE_MAX_BUDGET_TOKENS`（硬上限覆盖，默认 32000）按需调整。

### Changed

- **根探测护栏：任何目录启动都不会失控。** cwd 哨兵的判定链改为：向上找 `.git`/`.serena`（优先，嵌套仓不受影响）→ 直接子目录含 `.git` 即进入多项目调度模式 → 非 git 目录按文件数把关（≤20000 索引，超限拒绝启动并给出 `ASTROLABE_ROOT` / cd 指引）。显式指定的根跳过拦截。此前在一个 35 万文件的"项目收纳目录"启动会把整个目录当巨型仓库索引（实测 5 GB 内存 + 持续高 CPU）；该场景现在要么调度、要么拒绝。`ASTROLABE_MAX_ROOT_FILES` 可调。

- **Watcher 改为定向 diff。** 事件先按排除目录前缀过滤（`.astrolabe`/`target`/`node_modules` 等——写自己的缓存不再唤醒自己扫全树）；唤醒后只对事件路径做增量核对（目录事件展开子树），单次变动路径数超 `ASTROLABE_STORM_PATHS`（默认 5000，如 git checkout 大分支）才退回整库核对。安全兜底扫描从固定 600s 改为默认 1800s（`ASTROLABE_SAFETY_SECS`，0 关闭），且仅在事件通道声明不可信（溢出/全脏）时缩短到 60s 快扫；多进程相位 jitter 保留。

- **冷建期间编辑不再丢失。** watcher 注册与基线登记提前到冷建之前，建图期间到达的变更合并后在就绪时一次性增量补账（旧顺序会把这批编辑折进基线，导致新索引"出生即陈旧"）。

### Added

- **多项目目录调度模式。** 父目录含多个 git 子仓时不再建总索引：按工具调用的路径/文本提示路由到具体子仓，用到哪个才为哪个建索引，store 与 leader 锁都落在子仓目录；同时活跃子仓上限 3（`ASTROLABE_RESIDENT_ROOTS`），闲置 300s 自动释放 watcher/锁/语言服务器（`ASTROLABE_IDLE_EVICT_SECS`）；提示歧义时返回子仓清单让模型自选。

- **查询时新鲜度屏障。** 完整性敏感工具与 `apply_rename` 系列在作答前核对索引新鲜度：个别疑点文件当场增量补修（≤`ASTROLABE_BARRIER_MAX`，默认 64）；疑点超限则后台重建并在结果尾部如实标注 staleness；水位过期无疑点时主动请求一次校验。事件漏发不再表现为"静默缺引用"。

- **溢出加固。** watcher 事件通道 1024 → 65536，接收侧合并去重路径而非堆积事件对象；溢出置不信任标志并触发快扫兜底。

- **scan 契约 API 与嗅探缓存。** `path_admission`（单路径 gitignore 链重放，与全量扫描结论一致性有性质测试）、`count_files`（早停计数）、binary 嗅探按 (path, mtime, len) 缓存，权威扫描不再逐文件重读 8KB。

- **README 新规则章节。** 仓库定位判定链、多项目模式玩法、索引新鲜度机制与人话版环境变量速查表（新增 8 个旋钮）全部写入 README 第六节。

## [0.3.0] - 2026-09-29

### Changed

- **MCP 监视重建改为增量应用。** Watcher `ChangeSet` 先按语言过滤（`added`/`modified` 仅保留 `Language::from_path` 可识别的源文件；`removed` 全保留），再合并节流窗口内的全部变更，一次 `IncrementalIndex::apply_changeset`；无 Ready 快照时退回 `IncrementalIndex::build`。失败仍保留旧图。两次重建默认最小间隔 2s（`ASTROLABE_REINDEX_MS`，`0` 关闭等待仍 drain+merge）。The previous full `index_repo` on every save is gone.

- **Leader 门控持久化。** 同仓多进程用 `IndexerLock`（`.astrolabe/indexer.lock`）选举：leader `IndexOptions.persist = true` 写 `index.redb`；follower 纯内存增量、不碰 store。leader 退出后 OS 释放 flock，follower 下个周期 `try_acquire` 可晋升。`try_acquire` 失败记 warn、按 follower 处理。

### Added

- **`.astrolabe` 扫描排除与安全扫描 jitter。** `scan` 硬排除 `.astrolabe/`（避免把解析缓存扫进图）。`WatchConfig::safety_interval` 默认 600s（`Duration::ZERO` 关闭），`safety_jitter` 默认开启，多进程全树安全扫描错峰。

- **`ASTROLABE_PARSE_CACHE_MB`。** 可选，覆盖增量索引内存 LRU（默认 256 MiB），风格对齐 `ASTROLABE_CACHE_MB`。

## [0.2.0] - 2026-09-26

### Added

- **C / C++ tree-sitter + clangd discovery (P0).** `.c`, `.h` map to `Language::C` with `tree-sitter-c`; `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh` map to `Language::Cpp` with `tree-sitter-cpp`. Embedded symbols, imports, and calls queries. LSP discovery probes `clangd` (`--background-index`) then `ccls`. Include paths capture `#include "..."` and `<...>`. Override via `ASTROLABE_LSP_C` / `ASTROLABE_LSP_CPP`.

- **Apple ObjC / ObjCpp / Swift discovery (P0).** Recognize `.m` (`Language::ObjC`), `.mm` (`Language::ObjCpp`), and `.swift` (`Language::Swift`). Discovery probes `sourcekit-lsp` first (Xcode / Command Line Tools). ObjC / ObjCpp fall back to `clangd` when SourceKit is absent. Override via `ASTROLABE_LSP_SWIFT` / `ASTROLABE_LSP_OBJC` / `ASTROLABE_LSP_OBJCPP`.

- **PHP tree-sitter + LSP discovery (P0).** `.php` maps to `Language::Php` with
  `tree-sitter-php` (`LANGUAGE_PHP`) and embedded symbols/imports/calls queries.
  Discovery prefers `intelephense --stdio` (`npm i -g intelephense`; free tier
  works without a key; premium needs a purchased `INTELEPHENSE_LICENSE_KEY` —
  no pirated keys), then `phpactor language-server` (MIT, PHP 8.1+; composer
  global or phar). Override via `ASTROLABE_LSP_PHP`. Include/require paths
  capture `string_content` for both `string` and `encapsed_string`.

- **Vue SFC indexing + Volar discovery (P0).** `.vue` is a first-class
  `Language::Vue`. Indexing extracts `<script>` / `<script setup>` (JS/TS/TSX)
  into the shared parse pipeline — `tree-sitter-vue` is ABI-incompatible with
  tree-sitter 0.27, so embedding is intentional. LSP discovery probes
  `vue-language-server` (`npm i -g @vue/language-server`, override
  `ASTROLABE_LSP_VUE`). P0 is **single-process Volar only**; Serena-style
  dual-server / `@vue/typescript-plugin` coordination is an explicit follow-up,
  not a silent fallback.

- **Session-gated language-server install UX (MCP).** Precise tools that hit
  `Unavailable` now name the server and tell the model to ask the user, then
  call `ensure_language_server`. Without `confirm_install` the tool returns a
  `needs_install` plan (name, `version_policy=latest`, size if known) and does
  not download; with `confirm_install=true` it calls the core installer trait
  (currently a stub/`TODO` until the artifact installer merges).
  `get_languages` rows include `Ready` / `needs_install` / `AST-only`. L1/L2
  instructions and precise-tool descriptions document the gate.

### Fixed

- **`search_code` 字面/正则与截断声明。** 默认字面搜索（`regex=false`）；`A|B` 等正则语法必须显式 `regex=true`。`query` / `regex` / `path_filter` / `budget_tokens` 补齐 schema 说明。结果正文回显 `mode: literal|regex`，字面模式下 query 含 `|`、`.*`、`\b` 等元字符时给出警告；`search_code` / `find_symbol` 每条结果声明 `shown` / `omitted` / `truncated`（Claude Code 默认丢弃 structuredContent，故写入正文）。不做翻页：需要更多命中时加大 `budget_tokens` 或收紧 `path_filter`。

## [0.1.1] - 2026-09-22

首个 tag release（`v0.1.1`），内容与上方条目相同。

### Added

- **文件级代码图谱。** 扫描 → tree-sitter 解析 → 按构建配置解析 import → 建图。五门语言：Python、Go、Java、Rust、TypeScript/TSX/JavaScript。确定性：无 LLM、无向量；PageRank 固定 40 轮、阻尼 0.85。
- **导入解析（验收契约 100%）。** 分母 3012 条仓库内 import（语法提取口径）。读 `pyproject.toml` / `setup.cfg` / `setup.py`、`go.mod`/`go.work`、Maven/Gradle 源根、Cargo workspace、`tsconfig` `paths` 与 workspace 包。先前方案按条数加权约 15%。
- **符号抽取。** 每语言独立的 tree-sitter query（`symbols` / `imports` / `calls`）。符号召回门槛 ≥90%；README 记录 Python 100.00%、Rust 99.41%、Go 99.29%、Java 96.51%、TypeScript 93.97%。
- **置信度。** 每条边和工具输出携带 `exact | scoped | syntactic | unknown`。import 边为 `scoped`；名字匹配调用边为 `syntactic`。
- **MCP server（stdio）。** 十三个工具（`TOOL_COUNT=13`），以 `resolve_context` 为主入口。图谱层八个：`get_repo_skeleton`、`find_symbol`、`search_code`、`get_dependents`、`trace_calls`、`get_hotspots`、`get_languages`；精确层五个经 `LiveBackend` 按需拉起语言服务器：`find_references`、`goto_definition`、`get_diagnostics`、`plan_rename`、`apply_rename`（gated：仅 `exact`/`scoped` 或 `force=true` 才写盘；写后重解析，失败回滚；不可逆）。协议 `2026-07-28`，兼容无 `initialize` 握手；`tools/list` 带 24 小时 `ttlMs`。每个响应保证 `content[0]` 为非空文本（Cursor 会丢弃纯 `structuredContent`）。
- **解析缓存。** 按 `language` + 内容哈希写入被索引仓库的 `.astrolabe/index.redb`（redb，批次 ≤2048）。未改动文件跳过 tree-sitter。目录自带 `.gitignore`。缓存失败不阻断索引。
- **有界文件缓存。** moka，按字节 weigher + `time_to_idle`（默认 15 分钟）。`ASTROLABE_CACHE_MB` 默认 256，`0` 表示不缓存。
- **文件监视与后台重建。** 原生操作系统事件驱动（`notify`，空闲 0% CPU 占用）唤醒快照 diff，自动降级为 10s 轮询兜底；支持 500ms 防抖合并与重建侧排水，设 `ASTROLABE_WATCH_SECS=0` 可彻底关闭监听，指定 `N` 秒可强制纯轮询模式。重建期间旧索引继续服务；失败保留旧图。
- **资源治理。** 每语言一把 `Parser` 的池；解析预算 5 s 且真正打断；`IndexReport` 列出每一个解析失败和「看起来像仓库内」却未解析的 import。
- **索引性能门禁。** `index_bench` 在独立进程里测 `index_repo`；`scripts/bench.sh --ci` 对照 `crates/astrolabe-core/examples/baselines/*.json`。测量方法见 `docs/PERFORMANCE.md`。
- **发布骨架。** GitHub Release 六目标三元组（`.github/workflows/release.yml`）；npm 平台包布局与 SHA256 自愈回退（尚未推 registry）；`cargo deny` 许可证与 advisory 门禁。双许可 MIT OR Apache-2.0。
- **Phase 2 LSP。** `lsp/` 已实现（discovery、stdio transport、空闲回收池、queries、diagnostics）。MCP 经 `LiveBackend` 暴露 `find_references`、`goto_definition`、`get_diagnostics`、`plan_rename`、gated `apply_rename`。不常驻；缺服务器返回 `confidence: unknown` 与安装提示，不静默降级成名字匹配。

### 有意未做（0.1.0 缺口）

- **无独立确认协议的 rename apply。** `apply_rename` 已 gated（`is_auto_applicable` 或 `force=true`），但仍无 plan-id / 统一用户确认步骤；不可逆，调用方需自知。
- **调用边。** 库默认关闭。开启后边数约 1531 → 98154（64 倍），既漏报也过报。MCP 为 `trace_calls` 打开，并强制 `syntactic`。
- **整图快照持久化。** 只持久化解析结果。`FileId` 随当前文件集稠密分配，不能做跨会话增量的边表主键。
- **crates.io / npm 发布。** 版本号已是 `0.1.0`，尚未发版。

### 测量过、写进代码的限制（不是回归）

- 文件级 import 图对语言服务器召回 100%；名字匹配调用图 TypeScript 66%、Python 18%。因此文件级走图、符号级走 LSP，静态分流。
- 同类工具四次全库引用搜索让 LSP 子进程 186 MB → 719 MB，94% 的增长在宿主外。本项目不常驻语言服务器。
- OpenVisio 静默 `catch` 丢掉约 20% 文件（205/1026）；WASM 解析器泄漏到 5 GB RSS。本项目把这两条当成缺陷，而不是「以后再加日志」。

## [0.1.0] - 2026-09-19

开源初始版本（未单独打 tag；tag 与 `Cargo.toml` 版本必须一致，见 `.github/RELEASING.md`）。
