# Astrolabe AI（星盘）

**Astrolabe AI 是给 Claude Code、Cursor、Codex 用的本地代码检索与导航 MCP 服务：快速全文检索、符号查找、引用导航，并支持“每个目录有多少处调用”这类按目录统计。**
它是一个 Rust 单二进制（`astrolabe`），用 tree-sitter 建索引，需要时才启动语言服务器。

[English](README.md) · [安装指南](docs/INSTALL_GUIDE.zh-CN.md) · [FAQ](docs/FAQ.md) · [评测脚本](bench/README.md) · [更新日志](CHANGELOG.md)

> **同名消歧**：不是 npm 上的 `astrolabe`（2014 年的 Protractor 页面对象库）或 `astrolabe-ai`（无关的简历工具），也不是其它叫 “astrolabe” 的 MCP 服务。本项目在 `github.com/wojiushirencai/Astrolabe`，二进制名是 `astrolabe`。

## 事实卡（截至 2026-10-18）

| | |
|---|---|
| 最新版本 | [v0.13.0](https://github.com/wojiushirencai/Astrolabe/releases/tag/v0.13.0)，2026-10-18 发布 |
| 暴露的工具 | 7 个 MCP 工具：`resolve_context`、`search_code`、`find_symbol`、`find_references`、`goto_definition`、`get_diagnostics`、`get_languages` |
| `tools/list` 体积 | 紧凑 JSON 约 7.6 KB（Linux 实测 7,584 B；macOS 实测 8,001 B，差异尚未核对） |
| 二进制 | 单文件、无运行时；Release 压缩包 7.4–9.1 MB，Linux x64 解压后约 50 MB |
| 平台 | macOS arm64/x64、Linux x64（glibc/musl）、Linux arm64、Windows x64 |
| 语言 | 17 行语言矩阵，含 Python、Go、Rust、Java、TS/JS、C/C++、Swift、Objective-C、PHP、Vue、Dart、C#、VB.NET、Kotlin、Shell、Ruby、Lua（[详情](docs/REFERENCE.zh-CN.md)） |
| 需要 API Key / 联网吗 | 不需要。索引与检索本身不调用 LLM（读取结果的模型仍会消耗 token） |
| 已写明配置的客户端 | Claude Code、Cursor、Codex、Windsurf/Cline/Roo Code/Continue、Codebuddy、Grok Build、Kimi Code、ZCode、OpenCode |
| 许可证 | MIT OR Apache-2.0 |
| 测试 | `cargo test --workspace -- --list` 列出 1,117 个测试用例（2026-10-18） |

## 三步上手

**1. 安装**（macOS / Linux，会校验 Release 的 sha256）：

```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
```

其它方式（手动下载、Windows、源码编译）见 [安装指南](docs/INSTALL_GUIDE.zh-CN.md)。npm 包 `@astrolabe-ai/astrolabe` 已准备好，但**尚未发布**。

**2. 接入客户端。** Claude Code：

```bash
claude mcp add --scope user astrolabe -- astrolabe . --context=claude-code
```

Cursor（`.cursor/mcp.json`）：`{"mcpServers":{"astrolabe":{"command":"astrolabe","args":[".","--context=cursor"]}}}`。Codex、Windsurf 等见[安装指南](docs/INSTALL_GUIDE.zh-CN.md)。

**3. 验证。** `astrolabe --version` 输出版本；在客户端里让它调用 `get_languages` 或 `search_code`。添加后请重启客户端会话。

## 七个工具

| 工具 | 用途 |
|---|---|
| `search_code` | 字面量/正则检索，支持路径过滤；结果分组并标注定义/使用/注释；末尾有可核验的汇总（如“23/23 处，无截断”）。统计：`aggregate=top\|dir\|file`（按顶层目录/目录/文件给出命中数和文件数）、`count_only`、`skip_comments`、`exclude`（如 `_test.go`、`vendor`）。 |
| `resolve_context` | 给一句任务描述，聚合相关文件与代码骨架，并提示下一步该用哪个工具。 |
| `find_symbol` | 按名字或子串找函数、类、类型。 |
| `find_references` | 经语言服务器的编译器级引用；`dry_run_rename=新名` 只读列出每个文件要改的位置，不写盘。 |
| `goto_definition` | 经语言服务器跳到真实定义。 |
| `get_diagnostics` | 文件的语法与类型错误。 |
| `get_languages` | 语言、文件数、代码行数、语言服务器状态。 |

## 证据：测了什么，没测什么

以下数字都来自我们自己的配对 A/B 实测：**A** = 不接 Astrolabe 的 Claude Code，**C** = 同一任务接上 Astrolabe。花费是 Claude Code 的 `total_cost_usd`，时间是 `duration_ms`。只用了一个模型：`gemini-3.8-flash-high`，经 Claude Code 与一个兼容 Anthropic 接口的代理调用（所以美元数按该代理的计价）。所有组都禁用了 WebSearch/WebFetch。报告 `REPORT_V013E` 与 `REPORT_V012_BASELINE` 的摘要见 [bench/README.md](bench/README.md)。

**v0.13.0 同时开跑的配对（40 对，4 个任务，每个 10 对）。** 每一对的 A 和 C 在同一时刻起跑。

| | A（不接） | C（Astrolabe v0.13.0） | C − A，配对，95% bootstrap 区间 |
|---|---|---|---|
| 花费，每次均值 | $3.00 | $2.35 | **−$0.65 [−0.98, −0.31]** |
| 时间，每次均值 | 600 s | 492 s | **−108 s [−178, −37]** |
| 轮数，均值 | 50.3 | 40.0 | −10.3 [−14.4, −6.2] |
| 答对，N5 宽口径 | 38/40 | 40/40 | |
| 答对，N5 严格口径 | 38/40 | 31/40 | 见下 |

分任务（花费，C − A）：N5 −$0.64 [−1.22, −0.02]；N8 −$0.91 [−1.48, −0.31]；N4 −$0.51 [−1.20, +0.21]；E3b −$0.52 [−1.22, +0.18]。后两项区间含 0；而且 N4、E3b 里模型根本没用新的 `aggregate` 参数，不能把这两项的差值归功于它们。

*N5 严格口径：* N5 的标准答案把 2 处注释里的调用也算进去（model=15/44，total=321）。`skip_comments=true` 时 Astrolabe 返回 15/43、319，这也是合理答案，但严格检查判错。评分器同时支持两种口径（[bench/](bench/README.md)），我们两个都报。

**这些数据没有证明的事：**
- 只覆盖我们选的 4 个计数/定位/编辑任务。其余 11 个任务**还没有** v0.13.0 对 A 的同时开跑数据。
- 更早的 v0.12.0 矩阵（75 格、15 个任务、5 轮、三组同时起跑）里，Astrolabe 比不用**更贵**：花费 C − A = **+$0.244 [+0.035, +0.467]**；时间 +53 s [−8, +119]、答对率 −4.0 个百分点 [−10.7, +2.7]，区间都含 0。其中 40 个对照格多花 +$0.294 [+0.085, +0.540]。差距主要来自 4 个任务（E3b、N8、H1、N4；是看了数据后挑的，只能当假设），去掉它们后 C − A 为 +$0.03 [−0.14, +0.21]。
- 一个模型、一套评测脚本、样本小（每任务 10 对）。每轮耗时会随时段漂移，所以时间只有同时开跑的配对才可比。
- 四个主要任务里，N5、N8 在公开仓库（QuantumNous/new-api）上；N4、E3b 用的是私有仓库，**别人无法复现**。我们用的参考快照是本地副本，如何核对你的检出是否一致见 [bench/README.md](bench/README.md)。

复现公开部分：`bash bench/fetch_corpora.sh && bash bench/run_pairs.sh --tasks N5,N8 --pairs 10`（会产生真实 API 费用：我们的 40 对约 $214），然后 `python3 bench/report.py`。

## 什么时候有用，什么时候没用

- **实测有用：** 按目录/模块统计或列出出现次数并带排除条件（N5、N8）：同一个模型比用 shell 脚本做更少轮、更省钱。
- **没有证明有用：** v0.12.0 矩阵里 40 个通用对照格（含 cobra、express 这类小型公开仓库）接 Astrolabe 反而更贵；批量改文件流程（N4、E3b）平均更便宜但不显著。
- **功能重叠：** Claude Code 可通过自带的 LSP 插件提供跳转定义与查引用（[文档](https://code.claude.com/docs/en/plugins/code-intelligence)），与我们的 `goto_definition`、`find_references` 重叠。
- **采用率没有保证：** 模型有时会忽略 MCP 工具。v0.12.0 时模型在 N5 的 0/5、N8 的 1/5 里用了 Astrolabe；v0.13.0 改动后这两个任务 20/20 都用了。这是对这些任务的观察，不是承诺。

## 相关项目

[Serena](https://github.com/oraios/serena)、[codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp)、[codegraph](https://github.com/colbymchenry/codegraph)、[claude-context](https://github.com/zilliztech/claude-context)、Sourcegraph MCP Server，以及 Claude Code 内置的 LSP 插件。我们没有拿 Astrolabe 和其中任何一个做过基准对比；旧版 README 里与 Serena、OpenVisio 的索引耗时对比已连同口径限制归档在 [docs/PERFORMANCE.md](docs/PERFORMANCE.md)。

## 更多文档

[安装与客户端](docs/INSTALL_GUIDE.zh-CN.md) · [FAQ](docs/FAQ.md) · [参考手册：配置、环境变量、语言矩阵、质量门禁](docs/REFERENCE.zh-CN.md) · [架构](docs/ARCHITECTURE.md) · [性能口径](docs/PERFORMANCE.md) · [可选防漂移 Hook（效果未做基准）](docs/HOOKS.md) · [发版流程](.github/RELEASING.md) · [llms.txt](llms.txt)

源码编译：`cargo build --release --bin astrolabe`（Rust 1.90+）；测试：`cargo test --workspace`。

## 许可证

MIT OR Apache-2.0，见 [NOTICE](NOTICE)。设计受 [Serena](https://github.com/oraios/serena) 与 [OpenVisio](https://github.com/openvisio/openvisio) 启发，详见 [参考手册](docs/REFERENCE.zh-CN.md#五致谢与技术渊源)。
