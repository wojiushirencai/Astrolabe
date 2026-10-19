# Astrolabe AI 参考手册（中文）

本页收纳原 README 中的详细内容：工具速查、进阶配置、环境变量、语言矩阵、质量契约、开发命令。英文概览见 [README.md](../README.md)，安装见 [INSTALL_GUIDE.md](INSTALL_GUIDE.md)，Hook 见 [HOOKS.md](HOOKS.md)，性能口径见 [PERFORMANCE.md](PERFORMANCE.md)。

## 一、工具速查（7 个）

Astrolabe 向 AI 固定暴露 7 个工具（v0.12.0 起；此前的调用图、依赖图、热点、重命名、记忆等 15 个入口已移除，无兼容层）：

| 工具名称 | 核心参数 | 它能帮 AI 解决什么？（大白话解读） |
|---|---|---|
| `resolve_context` | `task_description`，可选 `offset`、`path_filter` | 丢给它一句任务描述，按相关度分页返回相关文件与符号：任务里写出的标识符（camelCase/snake_case）或完整路径所在文件排最前，其调用方和同名测试文件其次；小仓库（≤60 个文件）第 1 页列出全部文件。页头写明共匹配多少、本页范围，页尾说明剩余文件主要在哪些目录，由模型决定翻页还是缩小范围；翻页期间索引变化会提示从 offset=0 重查。结果末尾一行提示下一步该用哪个工具。 |
| `search_code` | `query`, `path_filter`, `offset`, `group`, `aggregate`, `exclude`, `skip_comments`, `count_only` | **确定性代码全文检索**：带正则与路径过滤；少量命中（≤5 处）自带所在函数体；跨文件命中自动分组并标注 `[定义]/[使用]/[加载]/[注释]/[断言]/[引用]` 角色（业务代码在前、测试在后；命中不多时每行全部展开）；每次结果末尾有一行“汇总”（总命中数、文件数、是否完整）；同会话已输出的函数体折叠为带签名的指针，重复查询返回首次结果的摘要（计数、最多命中的文件、首处与签名）；宽查询（>60 处）按目录汇总计数并预览最相关命中；零命中给出相近名称建议；结果被截断时给出 `total` 和 `next_offset`。`aggregate=top|dir|file` 按顶层目录/目录/文件统计，每行“N 处 / M 个文件”并给总计（统计、计数每个目录有多少处调用）；`count_only` 只返回总数；`skip_comments=true` 跳过注释与字符串里的命中；`exclude` 为 glob 黑名单（如 `["vendor","_test.go","*_test.go"]`），三者可组合；命中多时结果末尾提示这些用法；汇总行写“23/23 处，无截断”；文档命中单列“文档”组。 |
| `find_symbol` | `query` | **快速定位符号**：按名字或子串秒搜函数、类或类型，可直接带出定义体。 |
| `find_references` | `symbol`, `path`, `dry_run_rename` | **编译器级引用查找**：找到全库中所有调用该符号的准确位置；传 `dry_run_rename=新名` 得到按文件的改名清单（只读，不写盘），用于批量改名。 |
| `goto_definition` | `path`, `symbol` | **精准定义跳转**：利用 LSP 语义绑定直达真实定义处。 |
| `get_diagnostics` | `path` | **改后即时体检**：获取当前文件的语法报错与类型错误。 |
| `get_languages` | — | **代码库盘点**：统计语言种类、文件数与代码行数，并标注 LSP 状态；报告索引覆盖：未进索引的文件按原因计数，有语法错误的文件仍在索引里，按两类计数：其他语法错误（索引可能不完整，符号可能缺失），以及 C/C++/Objective-C 里预处理/宏导致的语法错误（启发式判断，符号基本完整）；≤10 个时逐个列出类别和首个错误的行号与该行片段，>10 个时给出 Markdown 报告 `<仓库>/.astrolabe/coverage-report.md` 的绝对路径（含类别列，其他语法错误在前），不支持语言的文件只按扩展名汇总。 |


## 二、进阶配置与运行细节

### 1. 仓库定位逻辑
启动时按以下顺序判定要索引的仓库根目录：
1. **显式指定优先**：CLI 位置参数（如 `astrolabe /path/to/repo`）或环境变量 `ASTROLABE_ROOT`。用户明确给出的路径直接采用，不做任何拦截；目录里装了多个子项目时，最多提醒一句，不替用户做主。
2. **默认 `.` 时，先向上找**：从当前目录一路向上找最近的 `.git`（支持普通目录、worktree、submodule）或 `.serena/project.yml`，找到即为仓库根——哪怕你是在仓库深处的子目录里启动，也能正确解析到仓库根。
3. **向上找不到，再看脚下**：比如在一个普通的非 git 目录里启动。此时检查当前目录的**直接子目录**里有没有 `.git`，也就是“一个目录装了多个项目”的情况——命中则进入多项目模式（见下一节）。
4. **都不沾边，数数把关**：子目录里也没有 `.git` 时，清点当前目录的文件数量。不超过 2 万个，当作普通目录正常索引；超过 2 万个则**拒绝启动**，并打印两条出路：进入具体项目目录再启动，或用 `ASTROLABE_ROOT` 显式指定。上限可用 `ASTROLABE_MAX_ROOT_FILES` 调整。

**为什么要立这些规矩？** 很多人把项目集中放在一个“收纳目录”里（比如 ~/Projects 装了十几个仓库）。旧版本在这种目录里启动，会把整个目录当成一个巨型仓库去索引。开发过程中的一次内部观察是：一个约 35 万文件的目录占用了约 5 GB 内存（无公开记录，仅作背景说明）。现在启动时会先判断仓库根，文件数超过上限（默认 2 万）则拒绝启动并提示。

> **安全机制**：每条工具输出第一行均包含 `index_root: /abs/path`。当路径与当前正在编辑的仓库不匹配时，智能体会明确收到提示，会明确收到提示。传入不存在的目录会直接报错退出，不会静默索引错地方。

### 2. 多项目目录：用到哪个子项目，就索引哪个
检测到当前目录装着多个 git 子项目时（上一节的第 3 步），Astrolabe 绝不建“大而全”的总索引：
- **启动零负担**：不为父目录建任何总索引，启动瞬间完成。
- **用到才建，建在子项目自己家里**：AI 的提问或操作涉及到某个子项目时，才临时为**那个子项目**建索引；索引缓存文件也放在子项目自己的目录里，父目录不会堆积任何索引数据。
- **来得快，让得也快**：同一时刻最多 3 个子项目保持“活跃”（最久没用的会让位）；一个子项目 5 分钟没人用，就自动释放干净——文件监听、缓存、语言服务器全部停掉。下次再用，秒级热身恢复，不必从头建起。
- **指代不清就问，绝不猜**：AI 的提问同时提到两个子项目、说不清要哪个时，工具会返回子项目清单让 AI 自己挑。

两个上限/时长可调：`ASTROLABE_RESIDENT_ROOTS`（同时活跃的子项目数，默认 3）、`ASTROLABE_IDLE_EVICT_SECS`（闲置多少秒后自动释放，默认 300）。

### 3. 客户端上下文（`--context`）
内置 5 种客户端上下文模式：
- `default`：通用默认模式。
- `claude-code`：专为 Claude Code 优化，关闭破坏性的残缺 `structuredContent`，只返回安全完整的文本流。
- `cursor`：专为 Cursor 优化，保证 `content[0]` 始终为有效文本，避免被 Cursor 静默丢弃。
- `codex`：适配 Codex 的 cwd 与参数特性。
- `readonly`：**只读安全模式**（目录工具均为只读，适用于安全审计、演示账号或只读审查代理）。

### 4. 环境变量速查表

按主题聚拢排列：仓库定位与多项目 → 客户端与输出 → 索引更新与缓存 → 语言服务器与拦截 → 日志。

| 环境变量 | 作用说明 | 默认值 |
|---|---|---|
| `ASTROLABE_ROOT` | 指定被索引仓库的根目录 | `.` (自动探测) |
| `ASTROLABE_MAX_ROOT_FILES` | 非 git 目录建索引的文件数上限，超过则拒绝启动并提示 | `20000` |
| `ASTROLABE_RESIDENT_ROOTS` | 多项目目录下同时保持活跃的子项目数上限 | `3` |
| `ASTROLABE_IDLE_EVICT_SECS` | 多项目模式下子项目闲置多少秒后自动释放 | `300` |
| `ASTROLABE_CONTEXT` | 指定客户端模式（`default` / `claude-code` / `cursor` / `codex` / `readonly`） | `default` |
| `ASTROLABE_STRUCTURED` | 是否开启结构化输出（`1` 开启，`0` 关闭） | 默认关闭 |
| `ASTROLABE_RESOURCE_THRESHOLD` | 工具结果超过该 token 估算值时卸载为 MCP resource（附前瞻摘要）；实际阈值为它与 `budget_tokens`（钳制后）的较大值 | `4000` |
| `ASTROLABE_MAX_BUDGET_TOKENS` | `budget_tokens` 硬上限：动态卸载阈值取 `max(阈值基线, budget)` 时防止异常大的 budget 把阈值顶穿 | `32000` |
| `ASTROLABE_WATCH_SECS` | 文件变更监听：未设置走原生系统事件（空闲 0% CPU）；`0` 关闭；`N` 走轮询兜底 | 未设置（原生事件优先） |
| `ASTROLABE_REINDEX_MS` | 两次索引更新之间的最小间隔毫秒数（0 关间隔） | `2000` |
| `ASTROLABE_STORM_PATHS` | 一次文件变动超过多少个路径时退回整库核对 | `5000` |
| `ASTROLABE_BARRIER_MAX` | 查询前“自动体检”当场补修的文件数上限 | `64` |
| `ASTROLABE_SAFETY_SECS` | 兜底巡检间隔秒数（0 关闭） | `1800` |
| `ASTROLABE_CACHE_MB` | 文件解析内存缓存大小预算（MB） | `256` |
| `ASTROLABE_PARSE_CACHE_MB` | 增量索引的内存缓存预算（MB） | `256` |
| `ASTROLABE_LSP_TIMEOUT` | 单次 LSP 语言服务器响应超时时间（秒） | `300` |
| `ASTROLABE_LSP_DART` | 覆盖 Dart 语言服务器（`dart language-server`）可执行文件路径 | 自动探测（随 Dart/Flutter SDK 自带） |
| `ASTROLABE_LSP_<LANG>` | 覆盖指定语言 LSP 服务器可执行文件路径（如 `ASTROLABE_LSP_PYTHON`, `ASTROLABE_LSP_RUST`, `ASTROLABE_LSP_C` 等） | 自动探测对应 LSP |
| `ASTROLABE_SEARCH_REPEAT_MAX` | 防循环：同一条 `search_code` / `find_symbol` 查询（查询串 + path_filter + 选项（含 budget_tokens、offset）完全相同）在窗口内最多执行几次，超出后不再重新搜索，直接返回“已执行过、请使用先前结果或换方法”的短提示（`0` 关闭） | `3` |
| `ASTROLABE_SEARCH_REPEAT_WINDOW_SECS` | 上述重复查询的滑动窗口秒数；`0` 关闭重复拦截 | `600` |
| `ASTROLABE_SEARCH_RATE_MAX` | 防循环：窗口内搜索工具（`search_code` + `find_symbol`）总调用次数超过该值后，在结果末尾追加循环警告（仍正常返回结果；`0` 关闭） | `300` |
| `ASTROLABE_SEARCH_RATE_WINDOW_SECS` | 上述总频率的滑动窗口秒数；`0` 关闭频率告警 | `600` |
| `ASTROLABE_SLICE_READ_MAX` | 切片精读最大行数：带 `limit ≤ N` 的 Read 调用视为切片读 | `200` |
| `ASTROLABE_SLICE_READ_THRESHOLD` | 连续切片读提醒阈值：连续 N 次切片读且期间无任何 Astrolabe 工具调用时触发 deny | `10` |
| `ASTROLABE_READ_THRESHOLD` | 连续全文件 Read 调用 deny 阈值（达到该次数触发拦截） | `3` |
| `ASTROLABE_GREP_THRESHOLD` | 连续 Grep/shell 文本搜索 deny 阈值（soft 模式；`1` ≈ 首次即拦） | `3` |
| `ASTROLABE_DENY_TEXT_SEARCH` | 硬禁用宿主 Grep / Glob / shell 文本搜索（`true`/`1`/`yes`/`on`）；开启后首次即 deny，覆盖 soft 阈值。多进程/多客户端可在各自 mcp.json `env` 中分别配置 | 关闭 |
| `ASTROLABE_DENY_SILENCE_SECS` | deny 后静默窗口秒数（窗口内 hook 放行，不累计计数） | `15` |
| `RUST_LOG` | 日志级别（`error` / `warn` / `info` / `debug`） | `info` |

### 5. 索引缓存与增量更新
- **哈希增量缓存**：解析结果按内容哈希持久化于被索引仓库下的 `.astrolabe/` 目录（自动写入 `.gitignore`，不污染版本控制）。未变更的文件二次索引时跳过语法解析，冷启动 1.0 秒、热启动 0.1 秒。
- **只补改动，不掀屋顶**：保存文件后只重新解析改动的那几个文件、只更新受影响的部分，不再整库重建；改的是文档、日志这类不参与索引的文件时，连重建都不会触发。
- **多开不打架**：同一个仓库开多个 AI 会话（多个 Astrolabe 进程）时，多个进程里只有一位“账房先生”负责写磁盘缓存，其他人只用自己的内存副本——互不抢锁，也不重复干活。
- **答案出场前的自动体检**：每次查询返回前，顺带核对结果涉及的文件是不是最新版本。个别文件过期就当场补修，快到无感；大面积过期时不装没事，会在结果里如实标注“结果可能过期”，并触发后台补建。当场补修的文件数上限由 `ASTROLABE_BARRIER_MAX` 控制（默认 64）。
- **漏网之鱼的兜底**：极少数情况下，操作系统的文件通知可能漏发。Astrolabe 会低频巡检补漏（默认每 30 分钟一次，检测到异常会自动加大巡检频率），间隔由 `ASTROLABE_SAFETY_SECS` 调整。
- **启动期间改文件不丢失**：先登记文件清单、再建图。建图期间保存的文件，会在建完后自动补进索引，不会出现“刚启动时改的代码查不到”。
- **重建期间不断服务、坏编辑不瘫痪**：后台自动监听文件变更，改动时平滑更新图谱；重建期间旧索引继续对外服务，保存文件不会中断工具调用。一次语法错误的坏编辑只记日志并保留旧图，不会因此退出。

### 6. 语言服务器的生命周期管理
精确层调用的语言服务器完全实现智能化按需调度：
- **空闲 5 分钟自动释放**：单次问答中保持热态，跨任务后自动回收，闲置后释放。
- **内存防护上限**：稳态内存上限设为 2 GiB，硬上限 4 GiB，防止失控的语言服务器拖垮整机。
- **真就绪判定**：监控语言服务器启动后必须连续安静 1 秒无待处理任务，才正式标记为就绪，以避免冷索引期返回空结果。

### 7. 语言支持与 LSP 矩阵

Astrolabe 适配以下语言的构建系统与语言服务器；索引与检索本身不调用 LLM：

| 语言 | 扩展名 | LSP 语言服务器 | 环境变量覆盖 | 备注 |
|---|---|---|---|---|
| **Python** | `.py`, `.pyi` | `pyright` | `ASTROLABE_LSP_PYTHON` | `npm i -g pyright`；支持 `pyproject.toml`、PEP 420 命名空间包 |
| **Go** | `.go` | `gopls` | `ASTROLABE_LSP_GO` | `go install golang.org/x/tools/gopls@latest`；支持 `go.mod`、`go.work` |
| **Rust** | `.rs` | `rust-analyzer` | `ASTROLABE_LSP_RUST` | `rustup component add rust-analyzer`；支持 Cargo workspace |
| **Java** | `.java` | `jdtls` | `ASTROLABE_LSP_JAVA` | `brew install jdtls`；支持 Maven、Gradle 源根解析 |
| **TypeScript / JS** | `.ts`, `.tsx`, `.js`, `.jsx`, `.mts`, `.cts`, `.mjs`, `.cjs` | `typescript-language-server` | `ASTROLABE_LSP_TYPESCRIPT` | 支持 `tsconfig.json` paths、Node workspace `package.json` |
| **C / C++** | `.c`, `.h`, `.cpp`, `.cc`, `.cxx`, `.c++`, `.hpp`, `.hh`, `.hxx`, `.h++`, `.ipp`, `.inl`, `.tpp`, `.tcc`, `.cu`, `.cuh`, `.cppm`, `.ixx` | `clangd` / `ccls` | `ASTROLABE_LSP_C`, `ASTROLABE_LSP_CXX` | `clangd --background-index`；支持 `#include` 路径解析；`.h` 按内容判定：含 `@interface` / `#import` 等 ObjC 标记归 ObjC，含 C++ 构造归 C++，否则为 C；C++ 抽取含析构函数与嵌套命名空间 |
| **Swift** | `.swift` | `sourcekit-lsp` | `ASTROLABE_LSP_SWIFT` | `xcode-select --install`：macOS Command Line Tools 自带 `sourcekit-lsp`（Linux / Windows 需另装 [Swift 工具链](https://www.swift.org/install/)）。Astrolabe 以 `backgroundIndexing` 启动 sourcekit-lsp，SwiftPM 包无需事先 `swift build` 即可跨文件查引用；没有 `Package.swift`（纯 Xcode 工程）或代码处于本机不生效的 `#if` 分支时，`find_references` 退回 tree-sitter 检索，标为 `Syntactic`。tree-sitter 符号抽取：class / struct / enum / protocol / extension / actor / func / init / 属性 / typealias；带大段属性（如 `@Test(arguments:)`）的声明锚点落在名字所在行 |
| **Objective-C / ObjC++** | `.m`, `.mm`，以及判定为 ObjC 的 `.h` | `clangd`（回退 `sourcekit-lsp`） | `ASTROLABE_LSP_OBJC`, `ASTROLABE_LSP_OBJCPP` | `brew install llvm` / `apt-get install clangd`。仓库没有 `compile_commands.json` 时，Astrolabe 在 `.astrolabe/clangd/` 生成一份（`-x objective-c`、`-fobjc-arc`、头文件搜索路径；macOS 用 `xcrun` 的 SDK，Linux 用自带的 Foundation 桩头文件），ObjC 的 `.h` 也按 ObjC 交给 clangd。语言服务器没有索引时，`find_references` 退回 tree-sitter 检索，标为 `Syntactic`。tree-sitter 符号抽取：`@interface` / `@implementation` / `@protocol` / category / 方法（完整 selector）/ 属性。解析前把 Cocoa 宏（`NS_ASSUME_NONNULL_*`、弃用 / 可用性宏等）与 `#if` 分支改写成可解析形式（行号不变）；`.mm` 先把 range-for、`auto`、lambda、引用等 C++ 写法改写后走 ObjC 语法，再用 C++ 语法解析一遍合并结果。签名从原始源码截取。`RCT_ENUM_CONVERTER` 这类复杂宏内的声明仍可能漏 |
| **PHP** | `.php` | `intelephense` / `phpactor` | `ASTROLABE_LSP_PHP` | `npm i -g intelephense`；支持 include/require 路径解析 |
| **Vue** | `.vue` | `vue-language-server` (Volar) | `ASTROLABE_LSP_VUE` | `npm i -g @vue/language-server typescript-language-server typescript`。Volar 2.x 以 `hybridMode=false` 单进程运行；Volar 3.x 自动识别版本，由 Astrolabe 桥接 `typescript-language-server` + `@vue/typescript-plugin`（转发 `tsserver/request`），跳转/引用/hover 由带插件的 TS 服务回答。SFC 内嵌 `<script>` / `<script setup>` 走 TS/JS 语法抽取；另抽组件名、`defineProps` / `defineEmits` / `defineModel` 与选项式 API 的 props / emits |
| **Dart** | `.dart` | `dart language-server` | `ASTROLABE_LSP_DART` | 随 Dart / Flutter SDK 自带；`pubspec.yaml` 行级解析与多 pubspec monorepo 支持；`rootUri` 置 `null` 规避重复分析 |
| **C#** | `.cs`, `.csx`, `.razor`, `.cshtml` | `Microsoft.CodeAnalysis.LanguageServer`（可选 Roslyn） | `ASTROLABE_LSP_CSHARP` | `.cs` / `.csx` / `.razor` / `.cshtml` 同为 C#（不另建语言变体，没有 Razor 语言）。`.csx` 用 C# 语法（含顶层语句与 local function）。`.razor` / `.cshtml` 在 tree-sitter 之前抽出 C#（`@code`、`@{ }`、行首 `@using` / `@inherits`；标记不当成 C# 符号；换行保留，列号不映射）。没有工程时 Roslyn 加载不了松散 `.csx`，这类脚本只走语法索引。命名空间 using 扇出到每个声明文件（不进 PageRank），`_ViewImports.cshtml` 的 `@using` 走同一解析（仅仓内命名空间；`System` / NuGet 为 None）。`ProjectReference` 记为 Import。Roslyn 为 `dotnet <dll> --stdio`，需要 .NET 10 runtime（工程目标框架可更旧），不静默下载，不是 OmniSharp / csharp-ls。Razor 没有语言服务器：现有启动器只认 Roslyn 的 `solution/open` / `project/open`，不是 Razor 协议，因此没有 `ASTROLABE_LSP_RAZOR`。 |
| **Visual Basic .NET** | `.vb` | 同一个 `Microsoft.CodeAnalysis.LanguageServer` | `ASTROLABE_LSP_CSHARP`（与 C# 共用，无第二个变量） | 只索引 `.vb`（不含 `.bas` / `.frm`）。`Imports` 命名空间扇出到每个声明文件（Namespace，不进 PageRank）；类型 `Imports` 仅在恰好一个仓内目标时记为 Import。`.vbproj` 的 `ProjectReference` 与 C# 一样记在项目元数据上，工程文件本身不是图节点，不伪造边。`.sln` / `.vbproj` 走同一 `dotnet <dll> --stdio`。 |
| **Kotlin** | `.kt`, `.kts` | `kotlin-lsp`（回退 `kotlin-language-server`） | `ASTROLABE_LSP_KOTLIN` | `brew install JetBrains/utils/kotlin-lsp`（预览版，或从 [Releases](https://github.com/Kotlin/kotlin-lsp/releases) 下载）；回退 `brew install kotlin-language-server`。class / interface / object / companion / fun / 属性 / typealias / enum entry；import 按文件声明的 `package` 解析（包名不必与目录一致），支持与 Java 混写项目 |
| **Shell / Bash** | `.sh`, `.bash`，以及 `#!` 为 sh/bash/dash/ksh 的无后缀脚本 | `bash-language-server` | `ASTROLABE_LSP_SHELL` | `npm i -g bash-language-server`（可选 `shellcheck` 提供诊断）。函数定义（含 `a::b` 风格名字）与 `source` / `.` 相对路径；`.bats` 暂按纯文本；zsh 专有语法不在 bash 语法覆盖范围内 |
| **Ruby** | `.rb`, `.rake`, `.gemspec`，`Rakefile` / `Gemfile` 等，以及 ruby shebang 脚本 | `ruby-lsp`（回退 `solargraph`） | `ASTROLABE_LSP_RUBY` | `gem install ruby-lsp`（回退 `gem install solargraph`）。class / module / def / `def self.x` / 常量；`define_method`、`alias_method`、`attr_*` 与 `A::B` 嵌套命名；`require_relative` / `require`（经 `lib/`）解析 |
| **Lua** | `.lua`，以及 lua/luajit shebang 脚本 | `lua-language-server` | `ASTROLABE_LSP_LUA` | `brew install lua-language-server`（或从 [Releases](https://github.com/LuaLS/lua-language-server/releases) 下载）。`function a.b:c()`、`local function`、表字段函数与模块表；`require("a.b")` → `a/b.lua` / `a/b/init.lua` |

---


---

## 三、质量契约：真实开源仓库验收

以下门禁在 CI 的 `five-language corpus gate` 中运行。Astrolabe 在 CI 的 `five-language corpus gate` 里跑两道门禁：五门主流语言的 import 解析验收（`scripts/verify.sh`），以及 v0.11 新增七门语言的声明召回验收（`scripts/recall-gate/recall_gate.py`，由 `verify.sh` 构建 debug 二进制后调用）。所有语料都锁定 commit，浅克隆并缓存。

### 1. 真实开源项目内部 Import 解析率（2985 条依赖语句基准）

| 语言 | 测试语料项目 | 锁定 Commit / Tag | 仓库内真实 Import 数 | 早期实现解析率（版本未记录，未复核） | Astrolabe 实测解析率 |
|---|---|---|---:|---:|---:|
| **Python** | serena | `701e7c84` | 1575 | 12% | **100%** |
| **Go** | gin | `v1.12.0` | 31 | 0% | **100%** |
| **Java** | gson | `gson-parent-2.14.0` | 971 | 0% | **100%** |
| **Rust** | ripgrep | `15.2.0` | 197 | 23% | **100%** |
| **TypeScript** | openvisio-oss | `bdb1d2a3` | 211 | 100% | **100%** |

> **分母规则严谨性**：分母严格掩码剥离注释与字符串中的虚假样例，严格识别 PEP 420 命名空间包与各语言真实语法。分母独立计算，即使解析出错也不会缩小分母掩盖问题。

### 2. 符号召回率实测
在真实语料上对照严格语法定义的扫描基线：
- **Python**: 100.00%
- **Rust**: 99.41%
- **Go**: 99.29%
- **Java**: 96.51%
- **TypeScript**: 93.97%

### 3. 新增语言声明召回门禁（每门一个锁定语料）

启动真实 MCP 服务，对冻结的全部声明清单（`scripts/recall-gate/gt/*.json.gz`）逐个调用 `find_symbol` 精确查询，锚点落在声明所在文件、行号 −3 至 +20 范围内算命中；另抽 200 个名字，每个名字的所有定义都要找到。

| 语言 | 测试语料项目 | 锁定 Commit | 声明数 | v0.10.2 | 当前 | CI 门槛 |
|---|---|---|---:|---:|---:|---:|
| **Swift** | swift-argument-parser | `efd239f0` | 4132 | 0% | **100%** | ≥99.9%，200/200 |
| **Objective-C** | AFNetworking | `d9f589cc` | 1473 | 0.54% | **100%** | ≥99.9%，200/200 |
| **Kotlin** | okhttp | `42e5888d` | 11732 | 0% | **100%** | ≥99.9%，200/200 |
| **Ruby** | sinatra | `cb22afd7` | 1342 | 0% | **100%** | ≥99.9%，200/200 |
| **Shell** | bats-core | `0c92eb4c` | 463 | 0% | **100%** | ≥99.9%，200/200 |
| **Lua** | nvim-lspconfig | `6d27c288` | 3312 | 0% | **100%** | ≥99.9%，200/200 |
| **Vue** | vue-vben-admin | `4634655c` | 4620 | 90.2% | **99.98%** | ≥99.9%，200/200 |

> 更大语料上的结果（PR #51）：Alamofire、kotlinx.coroutines、rubocop、nvm、LuaLS、Kong、element-plus、vuejs/core、SDWebImage、MMKV、React Native 均在 99.14%–100%。Vue 唯一漏项是模块顶层 `for` 循环计数变量，本不算声明。

---


---

## 四、本地开发与构建命令

```bash
cargo build                                          # 构建开发版本
cargo test --workspace                               # 运行全部单元与集成测试（190+ 项）
bash scripts/fetch-corpora.sh                        # 拉取锁定版本的验收语料到 corpus/
bash scripts/verify.sh                               # 五语言 import 解析 + 七门新语言声明召回验收（RECALL_GATE=0 只跑前者）
cargo run --release --example index_bench -- <repo>  # 测试任意指定仓库的索引耗时与内存
```

---


---

## 五、致谢与技术渊源

Astrolabe 的设计受 **[Serena](https://github.com/oraios/serena)** 与 **[OpenVisio](https://github.com/syntaxPriest/openvisio-oss)** 启发（三级提示词范式、多客户端兼容性对齐、PreToolUse 拦截思路；目录级聚合、多跳邻域、中心性热点这几类算法思想，实现保留在 core 中、不再作为 MCP 工具暴露）。具体借鉴点见 [NOTICE](../NOTICE)。

本项目是独立的 Rust 实现；与上述项目的性能对比只在 [PERFORMANCE.md](PERFORMANCE.md) 的「历史对照」里保留，并注明口径限制。

## 六、许可证

MIT OR Apache-2.0，见 [NOTICE](../NOTICE)。
