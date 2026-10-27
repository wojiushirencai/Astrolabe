# Astrolabe AI（星盘）

**Astrolabe AI 是给 Claude Code、Cursor、Codex 用的本地代码检索与导航 MCP 服务：快速全文检索、符号查找、引用导航，并支持“每个目录有多少处调用”这类按目录统计。**
它是一个 Rust 单二进制（`astrolabe`），用 tree-sitter 建索引，需要时才启动语言服务器。

[English](README.md) · [安装指南](docs/INSTALL_GUIDE.zh-CN.md) · [FAQ](docs/FAQ.md) · [评测脚本](bench/README.md) · [更新日志](CHANGELOG.md)

> **同名消歧**：不是 npm 上的 `astrolabe`（2014 年的 Protractor 页面对象库）或 `astrolabe-ai`（无关的简历工具），也不是其它叫 “astrolabe” 的 MCP 服务。本项目在 `github.com/wojiushirencai/Astrolabe`，二进制名是 `astrolabe`。

## 事实卡（截至 2026-10-27）

| | |
|---|---|
| 最新版本 | [v0.17.2](https://github.com/wojiushirencai/Astrolabe/releases/tag/v0.17.2)，2026-10-27 发布 |
| 暴露的工具 | 7 个 MCP 工具：`resolve_context`、`search_code`、`find_symbol`、`find_references`、`goto_definition`、`get_diagnostics`、`get_languages` |
| `tools/list` 体积 | 紧凑 JSON 约 3.6 KB（v0.16.0 Linux 实测 3,607 B；macOS 未重测） |
| 二进制 | 单文件、无运行时；Release 压缩包 7.4–9.1 MB，Linux x64 解压后约 50 MB |
| 平台 | macOS arm64/x64、Linux x64（glibc/musl）、Linux arm64、Windows x64 |
| 语言 | 18 行语言矩阵，含 Python、Go、Rust、Java、TS/JS、C/C++、Swift、Objective-C、PHP、Vue、Dart、C#、VB.NET、Kotlin、Shell、Ruby、Lua、Protobuf（[详情](docs/REFERENCE.zh-CN.md)） |
| 需要 API Key / 联网吗 | 不需要。索引与检索本身不调用 LLM（读取结果的模型仍会消耗 token） |
| 已写明配置的客户端 | Claude Code、Cursor、Codex、Windsurf/Cline/Roo Code/Continue、OpenCode、DeepSeek Harness、Codebuddy、Grok Build、Kimi Code、ZCode |
| 许可证 | MIT OR Apache-2.0 |
| 测试 | `cargo test --workspace -- --list` 列出 1,117 个测试用例（2026-10-18） |

## 三步上手

**1. 安装**（macOS / Linux，会校验 Release 的 sha256）：

```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
```

其它方式（手动下载、Windows、源码编译）见 [安装指南](docs/INSTALL_GUIDE.zh-CN.md)。npm 包：`npx -y @zlnap/astrolabe`（已发布）；配置客户端：`npx -y @zlnap/astrolabe setup`。

**2. 接入客户端。** Claude Code：

```bash
npx -y @zlnap/astrolabe setup   # 探测本机 agent 并合并写入 MCP 配置
# 或手动：
claude mcp add --scope user astrolabe -- npx -y @zlnap/astrolabe . --context=claude-code
```

Cursor（`.cursor/mcp.json`）：`{"mcpServers":{"astrolabe":{"command":"astrolabe","args":[".","--context=cursor"]}}}`。Codex、Windsurf、OpenCode、DSH、Codebuddy、Grok、Kimi、ZCode 等见[安装指南](docs/INSTALL_GUIDE.zh-CN.md)。

**3. 验证。** `astrolabe --version` 输出版本；在客户端里让它调用 `get_languages` 或 `search_code`。添加后请重启客户端会话。

## 七个工具

| 工具 | 用途 |
|---|---|
| `search_code` | 字面量/正则检索，支持路径过滤；结果分组并标注定义/使用/注释；末尾有可核验的汇总（如“23/23 处，无截断”）。统计：`aggregate=top\|dir\|file`（按顶层目录/目录/文件给出命中数和文件数）、`count_only`、`skip_comments`、`exclude`（如 `_test.go`、`vendor`）。 |
| `resolve_context` | 给一句任务描述，按相关度分页返回相关文件与符号（`offset` 翻页、`path_filter` 缩小范围），页头写明共匹配多少、剩余在哪些目录，并提示下一步该用哪个工具。 |
| `find_symbol` | 按名字或子串找函数、类、类型。 |
| `find_references` | 经语言服务器的编译器级引用；`dry_run_rename=新名` 只读列出每个文件要改的位置，不写盘。 |
| `goto_definition` | 经语言服务器跳到真实定义。 |
| `get_diagnostics` | 文件的语法与类型错误。 |
| `get_languages` | 语言、文件数、代码行数、语言服务器状态，以及索引覆盖情况（见下）。 |

`tools/list` 只声明模型实际会用的参数（小约 3.3 KB，这部分每轮 API 都会重发），其中包含每个工具上可选的按次 `index_root`（绝对路径或相对 cwd 的目录；省略则沿用进程启动时的索引根）。其余参数——`regex`、`exclude`、`count_only`、`context_lines`、`group`、`dedup`、`kind`、`depth`、`substring`、`dry_run_rename`、`character`、`severity`、`offset`、`page`、`budget_tokens`，以及非多仓工作区下的 `root`——照常接受。设 `ASTROLABE_FULL_SCHEMA=1` 可全部声明。

工具描述与返回文本为**纯英文**（schema 文案、结果正文、hook 提醒），避免中英双语让模型歧义。

**索引覆盖。** 支持语言的文件全部进入索引，包括有语法或编译错误的文件：能解析的符号会保留，出错区域里的符号可能缺失。`get_languages` 报告：
- 未进入索引的文件：按原因计数（超过大小上限、读取失败、解析超时）；
- 有语法错误的文件，按类别分别计数：
  - **其他语法错误**，标注为 `has syntax errors, index may be incomplete (symbols may be missing)`（有语法错误，索引可能不完整）；
  - **预处理/宏导致**（只用于 C、C++、Objective-C），标注为 `macro/preprocessor syntax errors, symbols mostly intact`（预处理/宏导致的语法错误，符号基本完整）：错误紧挨着预处理指令或宏写法，比如 `#if` 切开声明、`LUAI_FUNC int f()`、`TEST_CASE("x")`、`NAMESPACE_BEGIN`、`foreach(x) {`。没有构建配置，任何语法解析器都展开不了宏，但错误恢复会保住符号名，只是出错位置附近的符号范围可能不准。这是启发式判断，拿不准时归为其他语法错误。

  不超过 10 个时逐个列出（路径、类别标注、首个错误的行号和该行片段）；超过 10 个时只给各类数量和一份 Markdown 报告的绝对路径（`<仓库>/.astrolabe/coverage-report.md`，该目录自带 `.gitignore`，不可写时退到系统临时目录），报告里每个文件有路径、语言、类别和首个错误，其他语法错误排在前面，每节最多 1000 条；
- 不支持语言的文件：只按扩展名汇总（前 8 种，其余合并为一项），不列具体文件。

其它工具只在存在这些缺口时，在结果末尾附一行简短**英文**提示，指向 `get_languages`。

## 证据：测了什么，没测什么

以下数字都来自我们自己的配对 A/B 实测：**A** = 不接 Astrolabe 的 Claude Code，**C** = 同一任务接上 Astrolabe。花费是 Claude Code 的 `total_cost_usd`，时间是 `duration_ms`。只用了一个模型：`gemini-3.8-flash-high`，经 Claude Code 与一个兼容 Anthropic 接口的代理调用（所以美元数按该代理的计价）。所有组都禁用了 WebSearch/WebFetch。报告摘要见 [bench/results/](bench/results/)，方法见 [bench/README.md](bench/README.md)。

**v0.13.0 同时开跑的配对：15 个任务，95 对。** 其中 11 个任务各 5 对，4 个任务（N5、N8、N4、E3b）各 10 对。每一对的 A 和 C 在同一时刻起跑。

| | A（不接） | C（Astrolabe v0.13.0） | C − A，配对，95% bootstrap 区间 |
|---|---|---|---|
| 花费，每次均值 | $2.01 | $1.51 | **−$0.49 [−0.73, −0.28]** |
| 时间，每次均值 | 440 s | 357 s | **−83 s [−129, −41]** |
| 轮数，均值 | 34.9 | 28.1 | −6.8 [−9.2, −4.3] |
| 答对，主口径 | 93/95 | 95/95 | +2.1 个百分点 [0.0, +5.3] |
| 答对，严格口径 | 93/95 | 86/95 | −7.4 个百分点 [−14.7, −1.1] |

按任务等权看结论相近：花费 −$0.45 [−0.90, −0.15]，时间 −76 s [−153, −21]；15 个任务里 C 平均更省钱的有 11 个。剔除与机器冻结重叠的配对后变化不大（87 对：花费 −$0.45，时间 −87 s）。

*主口径与严格口径（N5）：* N5 的标准答案把 2 处注释里的调用也算进去（model=15/44，total=321）。`skip_comments=true` 时 Astrolabe 返回 15/43、319，这也是合理答案。所以 N5 的主口径两种都接受；严格口径（只认标准答案）仍然计算并并列报告。严格口径的差距全部来自 N5（C 严格 1/10，主口径 10/10）。N5 主口径是看到这个结果之后改的，两个数字都公开，方便你判断。

**收益在哪里。** 集中在聚合/发现型任务：N1（花费 C − A −$3.10 [−4.88, −1.32]，5 对）、N5 −$0.64 [−1.22, −0.02]、N8 −$0.91 [−1.48, −0.31]（各 10 对）。本轮新增的 11 个任务里，其余 10 个的花费区间都含 0；简单定位/小任务（H2、H3、H6、H7、E2、N6、N7）基本持平。N4、E3b（私有仓库）平均更便宜但不显著，且这两个任务里模型没用 `aggregate` 参数。每轮耗时没有差别（+0.6 s [−0.3, +1.4]），省时来自轮数减少。

**这些数据没有证明的事：**
- **不能**说普遍有收益。15 个任务是我们自己设计的，只有一个模型，每个任务只有 5 或 10 对，单任务区间很宽。平均数只描述这些任务。
- 更早的 v0.12.0 矩阵（75 格、15 个任务、5 轮、三组同时起跑）里，Astrolabe 比不用**更贵**：花费 C − A = **+$0.244 [+0.035, +0.467]**；时间 +53 s [−8, +119]、答对率 −4.0 个百分点 [−10.7, +2.7]，区间都含 0。其中 40 个对照格多花 +$0.294 [+0.085, +0.540]。新结果用的是 v0.13.0，改动了 `search_code` 和工具列表，两次不是同一个产品。
- 15 个任务里有 6 个用私有或改过的代码（N4、E3b、H4、H5、E1、N7），别人**无法复现**；公开仓库的任务可以重跑。我们用的参考快照是本地副本，如何核对你的检出是否一致见 [bench/README.md](bench/README.md)。
- 每轮耗时会随时段漂移，所以时间只有同时开跑的配对才可比。

复现公开部分：`bash bench/fetch_corpora.sh && bash bench/run_pairs.sh --tasks N5,N8 --pairs 10`（会产生真实 API 费用：40 对约 $214，55 对约 $121），然后 `python3 bench/report.py`。

## 什么时候有用，什么时候没用

- **实测有用：** 按目录/模块统计或列出出现次数并带排除条件（N5、N8），以及一个大范围发现任务（N1）：同一个模型比用 shell 脚本做更少轮、更省钱。
- **没有证明有用：** 简单定位或小改动任务（H2、H3、H6、H7、E2、N6、N7 基本持平，区间含 0）；v0.12.0 矩阵里 40 个通用对照格接 Astrolabe 反而更贵；批量改文件流程（N4、E3b）平均更便宜但不显著。
- **功能重叠：** Claude Code 可通过自带的 LSP 插件提供跳转定义与查引用（[文档](https://code.claude.com/docs/en/plugins/code-intelligence)），与我们的 `goto_definition`、`find_references` 重叠。
- **采用率没有保证：** 模型有时会忽略 MCP 工具。v0.12.0 时模型在 N5 的 0/5、N8 的 1/5 里用了 Astrolabe；v0.13.0 在 15 个任务共 95 次里用了 92 次。这是对这个模型和这些任务的观察，不是承诺。

## 相关项目

[Serena](https://github.com/oraios/serena)、[codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp)、[codegraph](https://github.com/colbymchenry/codegraph)、[claude-context](https://github.com/zilliztech/claude-context)、Sourcegraph MCP Server，以及 Claude Code 内置的 LSP 插件。我们没有拿 Astrolabe 和其中任何一个做过基准对比；旧版 README 里与 Serena、OpenVisio 的索引耗时对比已连同口径限制归档在 [docs/PERFORMANCE.md](docs/PERFORMANCE.md)。

## 更多文档

[安装与客户端](docs/INSTALL_GUIDE.zh-CN.md) · [FAQ](docs/FAQ.md) · [参考手册：配置、环境变量、语言矩阵、质量门禁](docs/REFERENCE.zh-CN.md) · [架构](docs/ARCHITECTURE.md) · [性能口径](docs/PERFORMANCE.md) · [可选防漂移 Hook（效果未做基准）](docs/HOOKS.md) · [发版流程](.github/RELEASING.md) · [llms.txt](llms.txt)

源码编译：`cargo build --release --bin astrolabe`（Rust 1.90+）；测试：`cargo test --workspace`。

## 许可证

MIT OR Apache-2.0，见 [NOTICE](NOTICE)。设计受 [Serena](https://github.com/oraios/serena) 与 [OpenVisio](https://github.com/syntaxPriest/openvisio-oss) 启发，详见 [参考手册](docs/REFERENCE.zh-CN.md#五致谢与技术渊源)。
