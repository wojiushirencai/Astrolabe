# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

crates.io / npm 尚未发布；tag 与 release 记录见 GitHub Releases。当前 workspace 版本见 `[workspace.package]`。

## [Unreleased]

## [0.4.0] - 2026-09-30

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
