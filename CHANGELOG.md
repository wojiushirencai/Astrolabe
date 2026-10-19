# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

crates.io / npm 尚未发布；tag 与 release 记录见 GitHub Releases。当前 workspace 版本见 `[workspace.package]`。

## [Unreleased]

## [0.14.0] - 2026-10-19

自 v0.13.0 以来的 PR：#76、#77（文档与分发）、#78、#79（审计修复两批）、#80（`get_languages` 覆盖重做、`resolve_context` 分页）、#81（第 4 批）、#82（第 5 批）。工具目录仍为 7 个，`tools/list` 7,185 B（Linux 实测，上限测试 7,500 B）。`EXTRACTOR_VERSION` 2 → 5：升级后首次启动会重新解析一遍。完整说明与 A/B 数据见 GitHub Release。

### 行为变化（升级时请留意）

- `find_references` / `goto_definition` 输出坐标改为 1 起始（列按 UTF-16）；入参 `line`/`character` 仍为 0 起始。
- `find_references` 的 `path` 在 schema 中必填；引用为 Unknown 或定义有歧义时不再生成改名预览。
- 7 个工具新增可选 `root`；全部声明 `readOnlyHint=true`（Claude Code 可同一轮并行执行）。
- `search_code`：“N 处命中”改为“N 行命中”；`skip_comments` 默认只跳过注释，字符串内部标 `[字符串]`；`count_only` / `aggregate` 不参与会话去重。
- `resolve_context`：去掉静默截断，改为按相关度分页（新增 `offset`、`path_filter`、`dedup`）。
- `get_languages`：有语法错误的文件留在索引里；超过 10 个时详情写入 `<仓库>/.astrolabe/coverage-report.md`。
- 从超过 2 万文件的仓库子目录启动时只索引该子目录；孤立 `\r` 换行按多行处理；C/C++ `<x.h>` 不再连到同名本地文件。

### 修复（摘要）

- 文件监听与新鲜度：已发送未应用批次、ignore 规则对齐、文本类文件新增后不发布、全量扫描阻塞监听线程、轮询模式频繁触发全量扫描、冷构建不可取消。
- 编码：短 GBK 误判成 EUC-KR；短日文/韩文被强制解成 GB18030（常用字表改为自统计，不含 LGPL/MPL 数据）。
- 解析：C# `#if` 分支与 `#:`、VB `Imports` 头部、C/C++ 宏改写重试、TS 变体修饰符与 `export type *`、serilog `[method]`、JSX 属性裸 `&`；部分解析结果进缓存。
- 遮蔽：Lua/Ruby/PHP/C#/Vue/shell/JS 等注释与字符串（含 Ruby `%` 字面量与 heredoc、PHP heredoc/nowdoc、C# 原始字符串）。
- LSP：进程组/Job Object 清理、stdin 独立写线程、clangd 自动安装不再阻塞、Vue 桥接只启动一次、崩溃后重启并重发、相对路径编译库改写、Roslyn 就绪信号、`staleRequestSupport`。
- 索引：缓存清理、损坏自愈（`index.redb.bad-<时间戳>`）、增量复用 resolver 与 C# 命名空间边、Java/Kotlin 静默截断。

### 新增

- C/C++ `#include` 解析器、PHP composer 解析器、C/C++ 头文件与源文件配对（跳转定义与 `resolve_context`）。
- Go 隐式接口实现关系；`find_symbol` / `find_references` 遇到接口时列出实现方。
- `find_symbol`：`query` 为文件路径（或 `*` + `path_filter`）时返回带行号的大纲；`depth` 展开类型成员；零命中且多子词时按子词兜底排序；新增 `kind=variable`。
- `find_references` 分页（`offset`），`goto_definition` 列出全部声明。
- 死代码清理（`rewrite/`、`churn.rs` 等，去掉 4 个 `oxc_*` 依赖）；`scripts/cjk-freq/gen_cjk_freq.py`。

### 分发与文档（无二进制行为变更）

- **安装修复**：新增 `install.sh`（下载对应平台 Release 归档并校验 sha256，不改 PATH/配置）；README/安装指南删除不存在的 `dist/release-binaries/` 说明，并补 `claude mcp add` 一行命令（已实测语法）。
- **npm 重命名**：未加 scope 的 `astrolabe`、`astrolabe-ai` 均被他人占用，包名改为 `@astrolabe-ai/astrolabe` 与 `@astrolabe-ai/<platform>`（二进制名仍是 `astrolabe`）；修正 npm 回退下载（原指向错误仓库与 Gitee、归档名缺版本号）；新增手动触发的 npm 发布工作流模板 `docs/distribution/npm-publish.workflow.yml`（未启用：需维护者复制到 `.github/workflows/`；需要 `NPM_TOKEN` 与 npm org `astrolabe-ai`，**尚未发布**）。
- **README 重写**：英文为主（`README.zh-CN.md` 同步），首屏事实卡、3 步安装、证据与局限、适合/不适合；删除无出处或过期的说法（对 Serena/OpenVisio 的“10 倍/99%”、“不烧一分钱 Token”节、11 MB、190+ 测试等）；防漂移 Hook 不再作为卖点（`docs/HOOKS.md`，注明效果未做基准）；详细内容移到 `docs/`。
- **评测脚本开源**：`bench/`（配对 A/B 脚本、任务定义、含 N5/N8 两种口径的评分器、复现说明；不含密钥/代理地址/私有仓库任务）。
- 新增 `llms.txt`、`docs/FAQ.md`、`docs/distribution/`（渠道提交材料）、`glama.json`、Claude Code 插件市场清单（`.claude-plugin/`、`claude-plugin/`）。

## [0.13.0] - 2026-10-18

基于 v0.12.0 实测分析（模型不信任 `search_code` 结果、反复重查，聚合/批量任务不用 Astrolabe）的收口改进；工具目录仍为 7 个，`tools/list` 约 8 KB 以内。

### 变更

- **`search_code` 收口信号**：注释行标 `[注释]`，声明行标 `[定义]`，其余代码仍 `[使用]`；去掉“文本启发式，需核对”；命中不超过 40 处时 `group` 全部展开；每次结果末尾一行“汇总：N 处命中 / M 个文件，已全部列出（结果完整）”或“未列全（不完整）”并给出续查方式；“另有 N 处”改为明确的“本文件还有 N 处未列出”。
- **`search_code` 计数与排除**：新增 `aggregate=dir|file`、`count_only`、`exclude`（glob 列表，裸名如 `vendor` 也匹配其下所有路径）。
- **`resolve_context`** 结果末尾一行提示下一步该用的工具（按任务措辞选 `find_symbol` / `search_code` / `find_references`）。
- **`find_references` 改名预览**：新增 `dry_run_rename=新名`，输出按文件的逐处替换清单（只读，不写盘；置信度非 exact 时提示核对）。未新增工具。
- **去重指针带摘要**：同一 `search_code` 查询重复时返回首次结果摘要（命中数、文件数、最多命中的文件、首处位置与所在签名），减少换词重查。

### 新增（聚合类任务采用率改进）

- **`aggregate=top`**：按顶层目录汇总；`top`/`dir`/`file` 每行同时给出“N 处 / M 个文件”和总计（含顶层目录数、目录数）。
- **`skip_comments`**：跳过注释（行注释、块注释，Rust 嵌套块注释）与字符串字面量里的命中，识别 Go/Rust/Python/JS/TS/Dart/Java/C 系/Kotlin/C#/Ruby/Shell 等；查询词本身含引号时字符串不跳过；文档类 text-only 命中（md/rst/txt）一并跳过并报告跳过数量。可与 `aggregate`、`exclude`、`count_only` 组合。
- **`exclude`** 支持 `_test.go`、`.min.js` 这类按文件名后缀的写法，以及 `*_test.go`、`vendor`。
- **工具描述首句与服务器 instructions** 改为“统计/计数每个目录有多少处调用”措辞，直指 `aggregate=top|dir|file`。
- **命中多（>40 处）时**结果末尾也给“下一步”提示（统计用 `aggregate`，改名用 `find_references dry_run_rename`）。
- **“已全部列出”可核验**：改为“汇总：23/23 处命中 / M 个文件，无截断”；未列全写“已列 K/N 处，有截断”。
- **分组结果把文档（`docs/`、md/rst/txt）从“生产代码”拆出**，单列“文档”组，命中行标 `[文档]`。

### 依赖与文档

- `zip` 8.6 → 9（修复 RUSTSEC-2026-0367/0368/0369，cargo-deny 门禁）。
- README 工具表改为固定 7 个并更新 `search_code` 说明（#72）。

## [0.12.0] - 2026-10-17

破坏性变更：MCP 工具目录固定为 7 个，删除 15 个入口。完整发版说明与评测数据见 GitHub Release。

### 1、具体是什么问题？如何解决的。

1. **22 个工具的目录体积大、边缘工具几乎无人调用**
   - 解决：目录固定为 7 个：`resolve_context`、`search_code`、`find_symbol`、`find_references`、`goto_definition`、`get_diagnostics`、`get_languages`。删除 `trace_calls`、`get_repo_skeleton`、`get_dependents`、`get_neighborhood`、`get_group_graph`、`get_hotspots`、`get_symbol_info`、`plan_rename`、`apply_rename`、`read_memory`、`write_memory`、`delete_memory`、`list_memories`、`initial_instructions`、`ensure_language_server`，不设兼容层；`excluded_tools` 中已删工具名被忽略。（#70）
2. **`search_code` 命中后模型还要反复 Read**
   - 解决：命中自带上下文（自适应展开函数体）；会话去重指针保留签名；宽查询预览最相关命中；零命中给相近名称。（#66、#67）
3. **谁断言/加载某字面量难以一眼看出；去重计数不准**
   - 解决：`group` 分组并标注角色标签，宽命中自动分组；去重只记真正发出的函数体；下线旧版 read-guard hook。（#68）
4. **模型绕过索引用 grep/rg/Glob 扫盘**
   - 解决：新增 `deny_text_search` 开关（`--deny-text-search`、`ASTROLABE_DENY_TEXT_SEARCH`、context YAML），默认关闭。（#69）

### 2、新增支持

- `search_code` 自带函数体、`group` 分组与角色标签、零命中近似名建议。
- `deny_text_search` 硬拦截开关（按进程配置）。

### 3、前后对比

| 项目 | v0.11.0 | 精简测试版 (c5) | 本版 7 工具 |
|---|---|---|---|
| tools/list 工具数 | 22 | 5 | 7 |
| 目录管理机制 | 动态过滤与全量保留 | `slim_catalog` 裁剪 | 固定 7 个 |

评测（gemini-3.8-flash-high，12 任务 × 3 轮，A/B/C 同时运行，每组 36 次；C 为 #68 的 5 工具精简版）：

| 组别 | 答对数 | 费用中位数 | 费用合计 | 耗时中位数 | Bash grep 调用 | Read 调用 |
|---|---|---|---|---|---|---|
| A 完全不开 | 34/36 | $1.22 | $56.58 | 178s | 272 | 376 |
| B v0.11.0 | 36/36 | $1.20 | $58.26 | 196s | 118 | 347 |
| C 精简版 | 36/36 | $0.99 | $46.82 | 192s | 96 | 305 |

## [0.11.0] - 2026-10-13

### 1、具体是什么问题？如何解决的。

1. **Kotlin 首次查询要等约 20 秒才有回答**
   - 原因：第一次查询要先同步启动 kotlin-lsp（JVM 启动约 5 秒），再等它就绪，最长 15 秒。
   - 解决：kotlin-lsp 改到后台线程启动，查询不再等它；首次等待上限从 15 秒降到 2 秒，可用 `ASTROLABE_KOTLIN_READY_WAIT_MS` 调整。启动和导入期间的查询直接给出 tree-sitter 语法结果，标为 Syntactic，并注明“仍在导入”。（#63）
2. **Kotlin 空结果被标成 Exact；每次重启都要重跑 Gradle 同步**
   - 解决：空结果改为回退到语法结果，不再标 Exact。kotlin-lsp 按仓库缓存系统目录，并复用导出的项目模型；构建文件没变时，重启后约 12 秒就有精确结果。（#62）
3. **Linux 和 macOS 上 Swift/ObjC 的 find_references 返回空**
   - 解决：sourcekit-lsp 开启后台索引；clangd 用合成的编译数据库；仍为空时回退到 tree-sitter，并标明 Syntactic。（#59）
4. **Volar 3.x 下 Vue 没有语言服务结果**
   - 解决：新增 Volar 3.x 桥接，接到 typescript-language-server 加 `@vue/typescript-plugin`；Volar 2.x 继续走 hybridMode=false。另外修正了 typescript@5 的 tsserver 路径。（#61、#58）
5. **zsh 顶层变量被语法错误吞掉；一次定义多个名字的函数定位不准**
   - 解决：从出错节点里恢复文件级变量；多名函数按行分别定位。（#60、#57）
6. **导入解析漏项**
   - Kotlin 多行 `@file` 注解、Shell 的 `\.` source、Ruby `require_relative` 以及 spec/test 加载路径，这些情况的导入之前会漏掉，现已修复。（#55）
7. **客户端断开或收到 SIGTERM 时，语言服务器没有被正常关闭**
   - 原因：Astrolabe 收到断开后 0.03 秒就退出，语言服务器的 `shutdown`/`exit` 根本没发出去，语言服务器只能靠 stdin 断开自己退出，没人看管（gopls 会多活约 3 秒）。原来的 SIGINT 路径还会卡在读 stdin 上。
   - 解决：退出前先把所有语言服务器并行关闭（`shutdown` 最多等 2 秒，再发 `exit`），整个过程最多 12 秒（`ASTROLABE_EXIT_TIMEOUT_MS`）。kotlin-lsp 退出后保存索引需要 4.7–5.8 秒，现在给它 10 秒宽限（`ASTROLABE_KOTLIN_EXIT_GRACE_MS`），其他语言 500 ms。SIGTERM 也走这套流程。（#64）
8. **其他**
   - find_symbol 支持按偏移分页。
   - 无扩展名脚本的 shebang 探测结果会缓存。（#53）
   - Vue 缓存按路径加盐。（#54）

### 2、新增了哪些支持（sql、xml、toml等）？如何支持的

- **8 门新语言的符号索引**：Swift、ObjC、ObjC++、Vue、Kotlin、Bash/Shell（含 zsh、.bats）、Ruby、Lua。
  - 实现：tree-sitter 查询加各语言的解析钩子；查询编译结果按语言缓存，冷索引快 5–20 倍；无扩展名文件用 shebang 识别。
  - ObjC++（.mm）先做掩码改写，再用两套语法分别解析。（#51、#56）
- **这些语言的语言服务器**：sourcekit-lsp、clangd、Volar（2.x/3.x）、kotlin-lsp（另有 kotlin-language-server 作为兜底）。（#58、#59、#61、#62）
- **召回门**：CI 召回门从 7 个语料扩到 15 个，每门新语言再加一个大型真实仓库（ObjC++ 也有一个）：
  - Alamofire、SDWebImage、MMKV、kotlinx.coroutines、rubocop、nvm、lua-language-server、element-plus。
  - 新增的 8 个语料在本机共跑约 23 秒。（#63）

### 3、调整后整体的覆盖和性能情况

| 项目 | 改动前 | 改动后 |
|---|---|---|
| Kotlin 首次启动：首个可用回答（okhttp） | 249 s（v0.10.2）；20.7 s（#62 之后） | **0.2 s**（Syntactic） |
| Kotlin 重启：首个可用回答 | 13.3 s | **0.2 s** |
| Kotlin 重启：首个 Exact | 14.3 s | **10.8 s** |
| Kotlin 首次启动：首个 Exact（含 Gradle 同步） | 343 s（#62 之前）；426 s（#62 之后） | 412–450 s（受 Gradle 同步限制，基本不变） |
| 召回门语料数 | 7 | **15** |
| 客户端断开：go/gin 退出 | 0.03 s（gopls 自己拖到 2.96 s 才退） | **1.07 s**（先关好 gopls，不留残余进程） |
| 客户端断开：okhttp（kotlin-lsp）退出 | 0.03 s（JVM 0.48 s 后才自己退） | **0.51 s**（先关好 kotlin-lsp） |
| 已有语言冷索引：zod / Polly / fmt | 5.28 s / 8.29 s / 1.77 s | **0.76 s / 1.27 s / 0.76 s** |
| Swift 召回（Alamofire / argument-parser） | 0% / 0% | **100% / 100%** |
| ObjC 召回（SDWebImage / AFNetworking） | 1.11% / 0.41% | **100% / 100%** |
| ObjC++ 召回（MMKV） | 2.62% | **100%** |
| Kotlin 召回（kotlinx.coroutines / okhttp） | 0% / 0% | **100% / 100%** |
| Ruby 召回（rubocop / sinatra） | 0% / 0% | **100% / 100%** |
| Shell 召回（nvm / bats-core） | 0% / 0% | **100% / 100%** |
| Lua 召回（lua-language-server / nvim-lspconfig） | 0% / 0% | **100% / 100%** |
| Vue 召回（element-plus / vue-vben-admin） | 94.69% / 90.09% | **99.96% / 99.98%** |
| find_symbol 200 名抽样（全部新语料） | 0–183/200 | **200/200** |
| 冷索引：element-plus / rubocop | 19.3 s / 23.6 s | **1.0 s / 1.8 s** |

### 已知问题
- okhttp 这类“多平台 + Android 库”模块：即使装了 Android SDK，kotlin-lsp 的导入也不会带上 commonJvmAndroid 源码目录，所以这类模块的跳转定义和找引用只能给 Syntactic 结果；悬停仍是 Exact。普通 Android 模块装了 SDK 之后可以拿到源码目录。
- element-plus（Vue）召回率 99.96%：漏掉的 2 条是 `docs/examples/table-v2` 里的局部变量 `i`。

## [0.10.2] - 2026-10-12

### 1、具体是什么问题？如何解决的。

- **Dart 构造函数和多类声明没有被抽取**（#47）。问题：无名构造函数、const / factory / 重定向构造函数没有挂在类名下（provider 漏掉 8 个定义）；字段、顶层变量/常量、枚举值、extension、extension type、typedef、运算符，以及 `class D = B with M;` 都没有被抽取。解决：补充 Dart 的 symbols.scm 查询；构造函数按类名记录（命名构造函数同时按自身名字记录）；运算符命名为 `operator +` 的形式。
- **TS 成员没有被抽取**（#48）。问题：类型别名里的对象成员（`type X = { a: T }`）、enum 成员，以及以字符串/数字命名的方法、字段、接口成员都没有被抽取。解决：补充 typescript 的 symbols.scm 规则，并加入 `ts_inline_object_member` 过滤（断言和参数里的内联匿名类型字面量有意不收录）。
- **search_code 没有分页**（#49）。问题：命中数很多时只能截断（zod 第一页只覆盖 57.9%）。解决：新增 `offset` 参数；截断时正文给出总数、本页范围（“第 X–Y 条，共 N 条”）和 `next_offset`；结构化输出带 total / offset / next_offset；结果顺序稳定（先代码命中，再 `[text-only]` 命中）；loop guard 的签名包含 offset，翻页不会被误拦。

### 2、新增了哪些支持（sql、xml、toml等）？如何支持的

本版本没有新增可搜索的文件类型（sql、xml、toml 等文本文件的搜索已在 v0.10.1 支持），新增的是符号抽取和分页：

- **Dart**（#47，通过 symbols.scm 查询实现）：构造函数挂在类名下，字段、顶层变量/常量、枚举值、extension / extension type、typedef、运算符、mixin application 类（`class D = B with M;`）。
- **TypeScript**（#48，通过 symbols.scm 规则加 `ts_inline_object_member` 过滤实现）：类型别名成员、enum 成员、以字符串/数字命名的成员。
- **search_code 分页**（#49）：通过 `offset` 参数和返回的 `next_offset` 翻页。

### 3、调整后整体的覆盖和性能情况

基线为 v0.10.1。

**全部声明的抽取召回率**

| 仓库 | 声明数 | 调整前 | 调整后 |
|---|---|---|---|
| provider (Dart) | 1,400 | 63.9% | 100% |
| riverpod (Dart) | 30,445 | 69.4% | 100% |
| dart-lang/http (Dart) | 10,099 | 47.2% | 100% |
| zod (TS) | 7,783 | 88.94% | 99.74% |
| trpc (TS) | 10,427 | 68.22% | 99.88% |

**通过 MCP 调用 find_symbol**

| 样本 | 调整前 | 调整后 |
|---|---|---|
| provider 209 个定义 | 96% | 100% |
| zod 抽样 50 个符号（141 处定义） | 119/141 (84%) | 126/141 (89%) |

zod 剩下的 15 处是内联匿名类型成员和基准写法引起的，是有意不收录的。

**TS 分类召回率（zod / trpc）**

| 类别 | 调整前 | 调整后 |
|---|---|---|
| 类型别名成员 | 0.9% / 0% | 100% / 100% |
| enum 成员 | 1% / 0% | 100% / 100% |
| 对象字面量方法（zod） | 73.7% | 100% |

**zod search_code（budget 32000）**

| | 调整前（只有第一页） | 调整后（按 next_offset 翻页） |
|---|---|---|
| 命中覆盖 | 7,459/12,879 (57.9%) | 12,874/12,879 (99.96%) |
| 页数 | 1 | 最多 4 页 |
| loop guard 拦截 | — | 0 次 |

**升级提示**：抽取器版本变了，升级后首次启动会重新解析所有文件。Dart 查询类名时，结果里现在也会有构造函数。

**已知仍存在的问题**：打包产物 `build/index.cjs` 中有 5 行没有搜到（正在排查）；find_symbol 还不支持分页；TS 的 find_references 在测试机上不可用。

## [0.10.1] - 2026-10-12

### 1、具体是什么问题？如何解决的。

- **loop guard 误拦正常重查**（#36）。问题：调大 `budget_tokens` 重试、改代码后复查、“索引构建中”的响应都被算作重复查询而被拦。解决：签名包含全部参数和索引代数（索引变化即重置），只统计成功执行的查询；被拦时返回上一次缓存的结果并标注 `blocked: true`。
- **LSP 文档不同步、结果误标、请求无超时**（#37）。问题：语言服务器一直用首次打开时的内容，收不到后续编辑；服务器自身建索引期间的部分结果被标为 Exact；单次请求没有超时（pyright 卡住超过 120 秒）。解决：每个文档记录版本号和哈希，发送 didChange / didClose / didChangeWatchedFiles；服务器建索引期间降级标注；单次查询 30 秒超时（`ASTROLABE_LSP_QUERY_TIMEOUT`），find_references 超时后回退到文本搜索。
- **find_symbol 排序与匹配问题**（#38）。问题：按路径排序导致真正的定义被截断；`Type.method` / `A::b` / `pkg.Func` 返回 0；`kind` 被忽略；Java/C# 锚点落在 `@注解` 行；path_filter 写错时静默返回 0。解决：按 精确 > 前缀 > 子串 排序；支持 `.` / `::` 分隔，并加入 Go receiver / impl 启发式；遵守 `kind`；锚点落在名字所在行；path_filter 无匹配时报错并给出最接近的路径。工具描述补充 C#/VB。
- **TS `outDir: "."` 排除整个源码树**（#39）。问题：zod 的 332 个文件被排除，却报告 “unindexed: none”。解决：只有当 outDir 是不含源码的独立目录时才排除，被排除的源码会如实报告。
- **索引覆盖遗漏**（#40）。问题：任意深度的 build/target/dist/obj 都被排除；非 UTF-8（GBK/Latin-1）文件被跳过；解析失败或文件过大时无提示；大写扩展名以及 .inl .tpp .cu .cppm .ixx 被忽略；.h 一律按 C 处理；Dart 命名构造函数、Go 局部常量缺失。解决：只在仓库/项目根目录排除构建目录；用 chardetng + encoding_rs 识别编码；给出 “N files not indexed” 提示；扩展名大小写不敏感并新增 C++ 扩展名；.h 按内容识别是否为 C++；补齐上述符号。
- **find_references 锚点错误**（#41）。问题：锚点取第一次文本出现的位置（注释/文档/import），Go 50 个符号中 16 个报错，Rust 返回空结果却标为 Exact（LspPool 32 处引用只找到 1 处）。解决：用 tree-sitter 定位到声明处，合并重载（最多 8 个），支持 line/character 参数，空结果或落在注释上时标为 Unknown，返回结构化的 returned/omitted/truncated。
- **search_code 只搜代码文件、空闲仓库频繁提示索引过期**（#42）。问题：search_code 只搜已索引的代码文件；未改动的仓库也一直提示 “index may be stale”（Linux 监听把 read/close 事件算作变更，导致反复重建，gson 30 秒约 7.4 万个事件）。解决：对所有扫描到的文本文件做 text-only 搜索（见第 2 节），`[text-only]` 命中排在代码命中之后，新增 `case_sensitive` 参数，跳过 lock 文件；丢弃只读类监听事件；barrier 2 秒超时时给出提示。
- **C# 符号抽取遗漏**（#43）。问题：漏掉字段、枚举成员、事件、索引器、运算符、转换运算符、析构函数、委托、嵌套局部函数。解决：补充 symbols.scm 查询和命名规则。
- **升级后沿用旧的符号抽取结果**（#44）。问题：单文件符号缓存只按语言和文件内容做键，升级后仍用旧版抽取结果，#38/#40/#43 的修复对老仓库不生效。解决：缓存键加入抽取器版本和程序版本。
- **测试补充**（#45）：按名字定位的 find_references 在服务器卡住时回退到文本搜索。

### 2、新增了哪些支持（sql、xml、toml等）？如何支持的

- **search_code 支持 text-only 搜索**（#42）：未进入代码索引的文本文件也能被搜到，包括 .md .sql .yaml/.yml .json .xml .toml .ini .cfg .conf .properties .env* .gradle(.kts) .kt Dockerfile Makefile .sh .proto .graphql .csv 以及模板文件等。
  - 实现方式：基于 ripgrep 的 grep-searcher / grep-regex 库，用 rayon 按文件并行搜索。
  - 规则：遵守 .gitignore；单文件上限 1.5MB；含 NUL 字节视为二进制并跳过；跳过 lock 文件（Cargo.lock、package-lock.json 等）；UTF-16 BOM 文件转码，GBK 文件有损解码。
- **其他新增**：非 UTF-8 源码可以建索引（#40）；新增 C++ 扩展名 .inl .tpp .cu .cppm .ixx（#40）；C# 成员类型（#43）；Dart 命名构造函数、Go 局部常量（#40）。

### 3、调整后整体的覆盖和性能情况

基线为 v0.10.0（`7094e4c`），调整后为 main `e53f3e6` 及之后。

**find_symbol 召回率（budget 2500 tokens，%）**

| 仓库 | 调整前 | 调整后 |
|---|---|---|
| Polly (C#) | 99 | 100 |
| astrolabe (Rust) | 100 | 100 |
| cobra (Go) | 96 | 100 |
| express (JS) | 100 | 100 |
| gson (Java) | 98 | 100 |
| provider (Dart) | 96 | 96 |
| requests (Python) | 100 | 100 |
| zod (TS) | 13 | 84 |

**search_code 召回率（budget 32000，%）**

| 仓库 | 调整前 | 调整后 |
|---|---|---|
| astrolabe | 85 | 85 |
| zod（字面） | 10 | 58 |
| zod（regex） | 10 | 97 |
| 其他仓库 | 100 | 100 |

regex 模式其余仓库不变（Go 的 51 是大小写敏感与基线大小写不敏感的口径差异）。zod 静默截断：39 → 1。

**find_references（50 个符号）**

| 仓库 | 调整前 | 调整后 |
|---|---|---|
| cobra (Go) | 34 成功 / 16 报错 | 50 成功 / 0 报错 |
| astrolabe (Rust) | 44 成功 / 6 空结果却标 Exact | 50 成功 / 0 空结果 |
| LspPool 引用数（rg 为 32） | 1 | 30 |

**C# 声明抽取召回率（%）**

| 仓库 | 调整前 | 调整后 |
|---|---|---|
| Polly | 88.3 | 100.0 |
| Newtonsoft.Json | 85.7 | 99.9 |
| Serilog | 84.9 | 99.9 |
| Java gson / jackson-core / spring-petclinic | 100 | 100 |

通过 MCP 调用 find_symbol 的同名冲突样本（Polly、Newtonsoft、gson、jackson-core 各 25 个）：100/100 定义都在结果中。

**未改动仓库的“索引可能过期”提示**

| 仓库 | 调整前 | 调整后 |
|---|---|---|
| gson | 每次查询 123 个疑似变更 | 0 |
| Polly | 约 975 | 0 |
| requests | 反复重建 | 0 |

**性能（text-only 搜索，9,510 个 .md 文件，91MB，8 核，5 次取中位数）**

| 查询 | 调整前 (ms) | 调整后 (ms) |
|---|---|---|
| xyznotfound | 94 | 29 |
| JsonReader | 117 | 36 |
| version | 101 | 29 |
| = | 181 | 149 |
| the | 140 | 91 |

同目录下 rg 命令行耗时 24–46ms。LSP 单次查询超时从 300 秒降到 30 秒。

**升级提示**：由于 #44，升级后首次启动会重新解析所有文件，耗时比平时长。

**已知仍存在的问题**：zod 仍有 22 个定义未召回；Dart provider 召回率 96%；TS 的 find_references 在测试机上为 0%（TS 服务器环境问题）；非常常见的名字会被 budget 截断，请用 path_filter 或调大 budget_tokens。

## [0.10.0] - 2026-10-11

### Added

- **`search_code` / `find_symbol` 服务端防循环守卫。** hook 不统计 Astrolabe 自身工具，曾出现子 Agent 48 分钟内调用 `search_code` 9,055 次、在 109 条查询间轮换的死循环。现在 MCP 服务端按连接记录查询签名（工具 + 查询串 + path_filter + 选项）：同一签名在 10 分钟窗口内执行超过 3 次后不再重新搜索，改为返回“已执行过、请使用先前结果或换方法”的短提示；窗口内搜索总次数超过 300 次时在结果末尾追加循环警告。阈值可用 `ASTROLABE_SEARCH_REPEAT_MAX` / `ASTROLABE_SEARCH_REPEAT_WINDOW_SECS` / `ASTROLABE_SEARCH_RATE_MAX` / `ASTROLABE_SEARCH_RATE_WINDOW_SECS` 调整，`0` 关闭对应检查（#34）。

- **`astrolabe hooks gc` 与 remind 内置过期会话 GC。** 删除 `~/.astrolabe/hook_data/` 下超过 24 小时（`ASTROLABE_HOOK_GC_HOURS` 可调；`0` 关闭 remind 中的机会式 GC）未活动的会话目录（以目录树内文件最新 mtime 判定，当前会话永不删除）。`hooks remind` 借助 `~/.astrolabe/hook_gc.stamp` 节流，每小时至多扫描一次。用于无 `SessionEnd` 事件的宿主（ZCode）与会话异常退出的兜底清理（#31）。

### Fixed

- **ZCode 接入文档纠正**（Refs #30）。ZCode 仅支持 7 个 hook 事件且严格校验，配置 `SessionEnd` 会导致整段 hooks 加载失败；README 与 `docs/CLIENT_ALIGNMENT.md` 改为给出 `~/.zcode/cli/config.json`（Windows `%USERPROFILE%\.zcode\cli\config.json`）的正确结构（`enabled: true` + `events` 包装层，`PreToolUse` 调用 `astrolabe hooks remind --client=zcode`），明确不配 `SessionEnd`、不在 `Stop` 上挂 cleanup，清理交由过期 GC（#31）。

- **冷启动索引竞态导致的偶发测试失败**（#32）。文件监听已启动但首次全量扫描未完成的空档内新建的文件可能永远不进索引（`cold_build_edit_is_visible_at_ready` 偶发失败）；现在先等待首次扫描完成（最多 60 秒）再开始建索引。

### Removed

- **README 友情链接一节**（#33）：移除 linux.do 友情链接（该节仅此一条，标题一并删除）。

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
