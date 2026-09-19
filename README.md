# Astrolabe

面向编码智能体的确定性代码图谱引擎，以 MCP server 形式提供服务。

Astrolabe
   - 寓意：星盘（古代天文学家和航海家用来观测天体、确定方位的精密仪器）。
   - 双关趣味：ASTrolabe 正好前缀带有 AST（抽象语法树），既象征天文级的全库拓扑，又契合语法树语义。
   - CLI 手感：astrolabe view / astrolabe mcp

它把仓库结构、文件依赖和符号信息整理成可查询的图，并且**明确暴露解析失败与不确定性**——不用"没有结果"掩盖索引缺口，也不用模糊匹配冒充精确答案。

同一仓库（serena，1028 个文件）上的实测对照：

| | 峰值内存 | 索引耗时 | 符号数 | 解析失败 |
|---|---:|---:|---:|---:|
| **Astrolabe**（首次） | **84 MB** | **1.0s** | **10051** | **0** |
| **Astrolabe**（缓存命中） | **31 MB** | **0.10s** | 同上 | **0** |
| OpenVisio（Node） | 4566 MB | 12.6s | 4979 | 205 |
| Serena（Python + LSP） | 373 MB | 5.7s | — | — |

首次索引要加载 Tree-sitter 语法并解析每个文件；之后未变更的文件按内容哈希跳过解析，内存和耗时都降到三分之一以下。回归门禁盯的是缓存命中态（见 `docs/PERFORMANCE.md`），因为冷启动会随系统页缓存抖动，不适合做阈值。

## 架构

按查询粒度分层，依据是实测而非偏好：**文件级 import 图对语言服务器的召回率是 100%，而名字匹配的符号级调用图只有 66%（TypeScript）到 18%（Python）**。所以两类问题必须分流。

- **文件级走图谱**：扫描源码、解析 import/use、按各语言的构建配置把模块路径映射到仓库文件，在图上完成依赖、邻域和影响分析。确定性，无 LLM，无向量检索。
- **符号级走语言服务器**：精确引用、定义、诊断、重命名计划交给 LSP（Phase 2 已实现：`find_references` / `goto_definition` / `get_diagnostics` / `plan_rename`；gated `apply_rename`）。
- **资源治理**：解析器资源显式释放，缓存有界。

每条结果都带置信度标注，`exact`（编译器支持）/ `scoped`（构建配置解析）/ `syntactic`（名字匹配，可能漏报或过报）/ `unknown`（无法确定）。让智能体能区分"没找到"和"不确定"。

## 安装

> 💡 **快速安装与各 AI 编程工具配置**：详见 [docs/INSTALL_GUIDE.md](docs/INSTALL_GUIDE.md)，已提供 macOS (Apple Silicon / Intel / Universal 通用二进制) 与 Windows x64 的预编译发布二进制包、校验和文件以及 Claude Code / Cursor / Windsurf 一键配置提示词模板。

### 方式一：从源码构建（当前唯一可用）

需要 Rust 1.90+。

```bash
git clone <repo-url> astrolabe && cd astrolabe
cargo build --release
# 产物：target/release/astrolabe（约 11 MB 单文件，无运行时依赖）
```

可选安装到 PATH：

```bash
cargo install --path crates/astrolabe-mcp
```

### 方式二：npm（发布后可用，给没有 Rust 工具链的用户）

```bash
npx -y astrolabe /path/to/repo
```

包已经做好并在本机验证过完整链路（`npm pack` → 安装 → MCP 握手返回 8 个工具 → 退出码与信号透传），只差推到 registry。结构是业界通行的那套（esbuild / Biome / SWC 同款）：主包 `astrolabe` 是零依赖 Node shim，六个 `@astrolabe/<platform>` 平台包走 `optionalDependencies`，npm 只装匹配当前 `os`/`cpu`/`libc` 的那一个。

两个坑已经处理：npm 11 以下的 lockfile bug 会漏装平台包，所以 shim 带一条从 GitHub Releases 下载的自愈回退，**校验 SHA256，哈希不符拒绝执行**；可执行位来自 `bin` 字段而非 postinstall chmod，避开 pnpm 10+ 默认拦截 lifecycle 脚本的问题。发布顺序是先六个平台包、再主包，`npm/scripts/publish.js` 已实现且中途失败可重跑。

### 方式三及以后：尚未就绪

| 渠道 | 命令 | 前置条件 |
|---|---|---|
| crates.io | `cargo install astrolabe-mcp` | 先发 `astrolabe-core` 再发 `astrolabe-mcp`，需要真实仓库 URL 和 token |
| GitHub Releases | 下载二进制 | 流水线已就绪（`.github/workflows/release.yml`），推 `v*` tag 即触发 |

## 配置

### 指定索引哪个仓库

优先级：位置参数 → `ASTROLABE_ROOT` 环境变量 → `.`（从进程 cwd 探测）。

对齐 Serena 的 Claude Code 接入：

- **`.` / 省略参数**（Serena 的 `--project-from-cwd`）：从 MCP 进程 cwd 向上找最近的项目边界——`.git`（目录或 worktree/submodule 指针文件）或 `.serena/project.yml`（单次扫描，就近获胜；同级任一标记即可）。存**绝对路径**。嵌套仓库/worktree 就近获胜，不会被祖先的 `.git` 或 `.serena/project.yml` 劫持。找不到标记时回退为 canonicalize(cwd)，并打 warn（Serena 会让项目保持 inactive，Astrolabe 仍索引 spawn 目录）。
- **显式路径**（Serena 的 `--project`）：canonicalize 该目录，**不**向上走。

```bash
astrolabe /path/to/repo          # 显式路径，不向上探测
ASTROLABE_ROOT=/path/to/repo astrolabe
cd /path/to/repo && astrolabe    # 等价于 astrolabe .
```

每条工具结果第一行是 `index_root: /abs/path`。路径和当前仓库对不上时，不要用这些命中去读文件。

传入不存在的目录会直接报错退出，不会静默索引错地方。全局 MCP 配置请继续写 `args: ["."]`，不要把某个仓库的绝对路径写进全局配置。

### Claude Code

`.mcp.json`（项目级）或 `~/.claude.json`（全局）。建议加 `--context=claude-code`（也可设 `ASTROLABE_CONTEXT=claude-code`）：

```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "/absolute/path/to/astrolabe",
      "args": [".", "--context=claude-code"],
      "env": {}
    }
  }
}
```

Claude Code 不会解包 `structuredContent`，而是用它**替换** `content[0].text`。因此默认**不发送** `structuredContent`（与 Serena 的 `structured_tool_output: false` 相同）。若只附带 `{confidence, budget_tokens}` 这类残缺 metadata，模型会以为索引是空的。

需要 structured output 的客户端可设 `ASTROLABE_STRUCTURED=1`；开启后该字段必须包含完整 `text` 和绝对 `root`，不能只发 metadata。

### Cursor

`.cursor/mcp.json`。`args` 用 `["."]`；可选再加 `--context=cursor`（或 `ASTROLABE_CONTEXT=cursor`）：

```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "/absolute/path/to/astrolabe",
      "args": ["."]
    }
  }
}
```

可选：`"args": [".", "--context=cursor"]`。

Cursor 对纯 `structuredContent` 的工具响应会静默丢弃。Astrolabe 默认只返回文本，且保证 `content[0]` 非空，因此不受该问题影响。

### Codex

`~/.codex/config.toml`。用 `args = [".", "--context=codex"]`，不要把某个仓库的绝对路径写进全局配置（Codex App 的 cwd 怪癖见 `docs/CLIENT_ALIGNMENT.md`）：

```toml
[mcp_servers.astrolabe]
command = "/absolute/path/to/astrolabe"
args = [".", "--context=codex"]
```

### 通用 MCP 客户端

stdio 传输，日志走 stderr（stdout 是 JSON-RPC 通道）。协议版本 `2026-07-28`，兼容 `2025-11-25` 的 `initialize` 握手，也接受无握手直连。

### 环境变量

| 变量 | 作用 | 默认 |
|---|---|---|
| `ASTROLABE_ROOT` | 索引根目录（位置参数优先级更高；`.` 会从 cwd 向上找 `.git` 或 `.serena/project.yml`） | `.`（从 cwd 探测） |
| `ASTROLABE_CONTEXT` | 客户端上下文名（`default`/`claude-code`/`cursor`/`codex`）或自定义 `.yml` 路径；`--context` 优先 | `default` |
| `ASTROLABE_STRUCTURED` | 显式覆盖 context 的 structured 开关：`1`/`true` 开，`0`/`false` 关；未设置则用 context | 未设置（跟 context；`default`/`claude-code` 为关） |
| `ASTROLABE_LSP_TIMEOUT` | 单次 LSP 请求超时（秒）；对齐 Serena 默认 300 | `300` |
| `ASTROLABE_CACHE_MB` | 文件缓存字节预算，`0` 表示不缓存 | 256 |
| `ASTROLABE_WATCH_SECS` | 后台文件监听模式：未设置使用原生事件（空闲 0% CPU）；`0` 关闭监听；正整数 `N` 强制 `N` 秒轮询兜底 | 未设置（原生事件优先） |
| `RUST_LOG` | 日志级别（`error`/`warn`/`info`/`debug`） | `info` |

跑验收测试时还可以用 `ASTROLABE_CORPUS_PYTHON` 和 `ASTROLABE_CORPUS_TYPESCRIPT` 指定语料位置，见"验收契约"。

### 客户端上下文（`--context`）

薄适配层，对齐 Serena 的 context 概念，**不含** prompt/mode。内置五个 YAML（编译期嵌入）：`default`、`claude-code`、`cursor`、`codex`、`readonly`，路径 `crates/astrolabe-mcp/contexts/`。

```bash
astrolabe --context=claude-code .
ASTROLABE_CONTEXT=cursor astrolabe .
astrolabe --context=/path/to/mine.yml .
```

**structured 优先级：** `ASTROLABE_STRUCTURED`（若设为真/假）→ context 的 `structured_tool_output`（`null`=auto，目前等同关闭）→ 默认关闭。因此 `claude-code` 强制关 structured，除非显式 `ASTROLABE_STRUCTURED=1`。

**只读模式（`--context=readonly`）：** 排除唯一写盘工具 `apply_rename`，其余 13 个工具全部保留（`plan_rename` 仅渲染计划不写盘，保留）。适用于只读代理、审计环境、演示账号等不希望出现不可逆操作的接入方。配置第二实例即可与主实例并存：

```json
{ "mcpServers": { "astrolabe-readonly": { "command": "/Users/you/.cargo/bin/astrolabe", "args": [".", "--context=readonly"] } } }
```

注意：MCP 工具目录按 server 实例区分，与 Claude Code 的子代理类型（如 Explore）无关——子代理共享主会话的 MCP 连接；只读约束由实例的 context 决定。

**如何新增客户端上下文：** 复制 `contexts/context.template.yml` 为 `<name>.yml`，填 `name` / `structured_tool_output` / 可选 `excluded_tools` / `notes`，在 `src/context.rs` 的 `BUILTINS` 加一行 `include_str!`，并在本表或上文点名。也可用 `--context=/path/to/custom.yml` 做一次性自定义，无需改代码。

### 索引缓存与增量更新

解析结果按内容哈希持久化到被索引仓库的 `.astrolabe/` 下，未变更的文件在二次索引时跳过 Tree-sitter。实测 serena（1028 文件）冷启动 1.06s、热启动 0.13s，**约 8 倍**。该目录会自动写入一份自我忽略的 `.gitignore`，不会出现在你的 `git status` 里。

服务启动后持续监听文件变更，检测到改动就在后台重建图谱。**重建期间旧索引继续服务**——不会因为你保存了一个文件就让所有工具调用失败一两秒；重建失败也只记日志并保留旧图，一次坏编辑不会把可用索引换成错误。

### 语言服务器的生命周期

精确层的语言服务器**按需启动、空闲回收**，不常驻。这套策略的每个阈值都来自实测，改动前请先看清代价：

| 行为 | 取值 | 依据 |
|---|---|---|
| 空闲回收 | 5 分钟 | 一次问答的自然间隔内保持热态，跨任务则释放 |
| 稳态内存上限 | 2 GiB | rust-analyzer 建索引时峰值超过 1 GiB，更低的上限会在它产出任何结果前就回收掉它 |
| 硬上限 | 4 GiB | 索引期也不豁免，防止失控进程拖垮机器 |
| 就绪等待 | 最多 120 秒 | 向智能体返回"稍后重试"只是白跑一趟，它除了重试什么都做不了 |

两个必须知道的实测结论：

**索引期豁免稳态上限。** 建索引是瞬时峰值，在那里回收等于保证服务器永远跨不过去——实测 rust-analyzer 冲到 1165 MB 后被 1024 MB 的上限在 4.6 秒时杀掉，一个查询都没答上来。硬上限不豁免。

**"就绪"要求持续安静，不是某一刻不忙。** rust-analyzer 在一个 10 行 crate 上启动时，活跃进度令牌集合会在 0.18s 到 1.57s 之间**反复清空 11 次**才真正索引完。单次采样必然落进某个空档，把冷服务器判成就绪。判定条件是连续安静 1 秒。

冷索引返回的空结果会被标成 `unknown` 而非 `exact`。语言服务器在索引未完成时用 `[]` 而不是错误来应答，把它当权威答案就是在告诉智能体"这个符号没有引用"——而智能体会据此删掉活代码。同理，`ContentModified`（LSP `-32801`）按协议规定重试，不作为失败上报。

## 工具

十一个工具，以 `resolve_context` 为主入口。工具数量刻意保持精简——一个强入口比一堆窄工具更能引导智能体，误选更少、每会话占用的上下文也更少。

分两层：**图谱层**八个工具常驻可用，零外部依赖；**精确层**三个工具按需拉起语言服务器，只在图谱给不出确定答案时才付这笔代价。

| 工具 | 必填参数 | 可选参数 | 用途 |
|---|---|---|---|
| `resolve_context` | `task_description` | — | 主入口。给一段任务描述，返回相关骨架与邻域 |
| `get_repo_skeleton` | — | — | 按 import 中心性排序的仓库骨架与公开符号 |
| `find_symbol` | `query` | — | 按名称或子串定位符号，返回签名与 `path:line` 锚点 |
| `search_code` | `query` | `regex` `path_filter` | 全文检索，返回匹配行与锚点 |
| `get_dependents` | `target` | `direction` | 文件级依赖与影响面（对 LSP 召回率 100%） |
| `trace_calls` | `symbol` | `direction` | 调用链追踪（`syntactic` 置信度，需人工核验） |
| `get_hotspots` | — | — | import 中心性高的承重文件 |
| `get_languages` | — | — | 语言、文件数、代码行数统计 |

精确层（按需启动语言服务器，未安装时返回安装提示而非空结果）：

| 工具 | 必填参数 | 可选参数 | 用途 |
|---|---|---|---|
| `find_references` | `symbol` | `path` | 精确引用查询（`exact` 置信度） |
| `goto_definition` | `path` | `symbol` 或 `line`+`character` | 精确定义跳转（LSP 绑定解析；不要用 `find_symbol` 代替） |
| `get_diagnostics` | `path` | — | 类型错误与诊断 |
| `plan_rename` | `symbol` `new_name` | `path` | 重命名计划，只产出不落盘 |
| `apply_rename` | `symbol` `new_name` | `path` `force` | 应用重命名（不可逆）；默认仅 exact/scoped，否则需 `force=true`；写后重解析，失败回滚 |

`path` 决定启动哪个语言服务器，缺省时工具会说明需要补什么，而不是猜一个。

所有工具接受 `budget_tokens`，按排名截断并说明省略了多少项。`tools/list` 带 24 小时 `ttlMs`，客户端可缓存工具定义。

索引在后台异步构建，构建期间调用工具会返回明确的"索引正在构建"提示而非空结果。serena 规模（1028 文件）约 1 秒。

## 防漂移：hooks 与 system prompt override

即使有 instructions 军规，模型在长会话中仍会因上下文增长或对内置工具的偏置而遗忘约定，漂移回使用 grep / read 的惰性模式（agent drift）。Astrolabe 对齐 Serena 的防漂移体系（context 军规 + remind hooks + system prompt override），在工具调用层做硬性计数拦截与主动引导。

### Claude Code 配置

将以下 hooks 配置写入项目 `.claude/settings.json`（仅当前项目生效）或全局 `~/.claude/settings.json`：

```json
{
    "hooks": {
        "PreToolUse": [
            {
                "matcher": "",
                "hooks": [
                    {
                        "type": "command",
                        "command": "astrolabe hooks remind --client=claude-code"
                    }
                ]
            }
        ],
        "SessionEnd": [
            {
                "matcher": "",
                "hooks": [
                    {
                        "type": "command",
                        "command": "astrolabe hooks cleanup"
                    }
                ]
            }
        ]
    }
}
```

### System Prompt 整体替换（Claude Code 专用）

Claude Code 启动时支持整体替换系统提示词，彻底消除默认内置工具的偏好影响：

```shell
claude --system-prompt="$(astrolabe print-cc-system-prompt-override)"
```

也可将 `astrolabe print-cc-system-prompt-override` 的内容写入项目 `CLAUDE.md`，但 Serena 的实测结论表明，其效果显著弱于 `--system-prompt` 整体替换。

### 行为机制

`astrolabe hooks remind` 挂载在 PreToolUse 阶段，按会话做硬性计数治理：

- **Deny 触发条件**：在无 Astrolabe 符号工具调用的情况下，连续 3 次 grep、连续 3 次读代码文件（基于 58 种常见源码扩展名过滤），或连续 4 次混合调用（grep 与 read 累积），hook 将拦截并输出 deny 决策 JSON。
- **计数重置**：任意 Astrolabe 符号工具调用（如 `resolve_context`、`find_symbol`、`find_references` 等）会立即清空连续计数；两次同类调用间隔超时（grep/read > 1000s，混合 > 2000s）亦会重置。
- **静默窗口**：触发 deny 后进入 120 秒静默期。静默窗口内 hook 变为 no-op（放行且不递增计数），避免打断必要的连续排查。
- **状态存储与清理**：会话状态持久化于 `~/.astrolabe/hook_data/<session_id>/counter.json`；会话结束时 `SessionEnd` 触发 `astrolabe hooks cleanup` 自动清理对应会话目录。

### Codex 与 Grok 支持

- **Codex**：使用 `--client=codex` 切换适配 Codex 的 hook 输出格式。需在 `~/.codex/config.toml` 中开启 `[features] codex_hooks = true`，并在 `~/.codex/hooks.json` 中配置 PreToolUse（`matcher: "Bash"`，命令为 `astrolabe hooks remind --client=codex`）与 SessionEnd。
- **Grok**：使用 `--client=grok` 适配 Grok 的 hook 规范，支持针对 shell 管道命令（如 grep、rg、cat 等）的参数提取与模式判断。

## 验收契约

`scripts/verify.sh` 是质量门禁，任一项不达标返回非零退出码。

**导入解析**（五语言 2985 条仓库内 import）。分母对应 `scripts/fetch-corpora.sh` 钉死的语料版本，这是唯一可复现的基准：

| 语言 | 语料 | 钉死版本 | 仓库内 import | 旧方案 | 实测 |
|---|---|---|---:|---:|---:|
| Python | serena | `701e7c84` | 1575 | 12% | **100%** |
| Go | gin | `v1.12.0` | 31 | 0% | **100%** |
| Java | gson | `gson-parent-2.14.0` | 971 | 0% | **100%** |
| Rust | ripgrep | `15.2.0` | 197 | 23% | **100%** |
| TypeScript | openvisio-oss | `bdb1d2a3` | 211 | 100% | **100%** |

本机语料若不是从该脚本拉取的（比如直接 clone 了默认分支），分母会不同——上游 tag 之后的新文件会带进额外的 import。解析率仍应是 100%；**变的是分母，不是通过与否**。

分母按语法提取口径：注释与字符串字面量已掩码，PEP 420 命名空间包计入，Rust 的 `use` 按语句计数。早期表格里的 1557 / 91 / 205 来自行扫描，会把 docstring 里的示例 import 算作真实导入、又漏掉命名空间包，已废弃。

**符号召回**（门槛 ≥90%）。基线是对源码做掩码后的正则定义扫描，**不是语言服务器的 document symbols**——后者需要装齐五种语言服务器才能跑，做不了常规门禁。正则基线有已知噪音（泛型参数、类型名会被当成字段计入分母），所以未达 100% 的部分需逐条核查是真漏还是噪音，详见 `crates/astrolabe-core/src/parse/mod.rs` 的 `run_symbol_recall`：

Python 100.00% · Rust 99.41% · Go 99.29% · Java 96.51% · TypeScript 93.97%

两类验收的 ground truth 都独立于被测代码生成：先建分母再解析，解析失败仍计漏报，读取失败写入哨兵名占位。这一条来自教训——曾经有过 209/209 全绿却丢失一整个源文件的情况，那个文件被误判为二进制，它的三条 import 连同它一起从分母里消失了。**分母不能随被测代码的失败一起缩小。**

Go、Java、Rust 三份语料放在 `corpus/` 下（不入版本控制，需自行 clone 对应 tag）。Python 和 TypeScript 默认找 workspace 的同级目录 `serena` 和 `openvisio-oss`，可用 `ASTROLABE_CORPUS_PYTHON` / `ASTROLABE_CORPUS_TYPESCRIPT` 覆盖。

## 构建与测试

```bash
cargo build                                    # 构建
cargo test --workspace                         # 单元与集成测试（190 个）
bash scripts/verify.sh                         # 全部真实语料验收
cargo test -p astrolabe-core -- --ignored      # 语料测试并查看未解析样例
cargo run --release --example index_bench -- <repo>   # 索引一个仓库并打印开销
```

## 进度与已知缺口

文件级图谱层与符号级精确层（Phase 2）均已落地；Phase 3 写入经 gated `apply_rename` 接到 MCP。

| 能力 | 状态 |
|---|---|
| 五语言导入解析、符号提取、依赖图、任务相关排序 | 可用 |
| MCP server（13 工具：8 图谱 + 5 精确；置信度标注、token 预算） | 可用 |
| 索引持久化与增量重索引 | 可用。按内容哈希跳过未变更文件，热索引约 8 倍；文件监听自动触发重建 |
| 内存上界强制 | 可用。实测 8 MB 预算下 RSS 稳定在 45 MB 平台，16 轮查询不增长；1–2 MB 预算下缓存占用贴住上限（误差 <100 字节） |
| 符号级精确操作（引用、定义、诊断、重命名计划） | 可用。`find_references` / `goto_definition` / `get_diagnostics` / `plan_rename` 经 `LiveBackend` 按需拉起语言服务器；缺服务器返回 `unknown` 与安装提示，不静默降级。gated `apply_rename`（默认仅 exact/scoped 或 `force`） |
| 安全改写 | 计划层可用（`plan_rename` / JS·TS 作用域）；`apply_rename` 已 gated 接入（低于 scoped 默认拒绝；`force` 可强制）；写后重解析，失败回滚。不可逆 |

调用边默认关闭：开启后边数从 1531 涨到 98154（64 倍），名字匹配既漏报也严重过报，开启时每条边标注 `syntactic`。

## 致敬与技术渊源 (Acknowledgements & Lineage)

Astrolabe 站在开源代码智能体工具探索的肩膀上。在架构设计与关键功能实现中，深度汲取并致敬了 **[Serena](https://github.com/oraios/serena)** 与 **[OpenVisio](https://github.com/openvisio/openvisio)** 两个优秀上游项目的工程实践与理论思想：

### 借鉴 Serena (Oraios AI)
- **三级提示词架构（Tiered Instructions L1–L3）**：借鉴 Serena 的多层引导范式。L1 连接握手阶段注入硬核约束与绝对索引根，L2 通过 `initial_instructions` 暴露完整工具使用守则与严苛纪律（彻底压制模型滥用 grep / read 的偏置），L3 工具元数据按需说明，构建层层递进的 Agent 引导链。
- **客户端对齐机制（Client Alignment）**：吸纳 Serena 在多客户端复杂异构环境下的适配策略，包括双标记就近项目探测（`.git` 与 `.serena/project.yml` 协同边界发现，彻底防止嵌套仓库/worktree 越界与祖先劫持）与 `structuredContent` 针对性治理（规避 Claude Code 替换 `content[0].text` 以及 Cursor 静默丢弃非文本响应的问题）。
- **PreToolUse 防漂移 Hook 思想**：继承 Serena 主动对抗 Agent 遗忘与偏置（Agent Drift）的治理思想，在 PreToolUse 阶段以硬性调用计数（连续 grep/read 拦截、窗口静默、会话状态持久化与自动清理）硬阻断非理性的全库暴力扫描，纠正模型回归符号与图谱工具。

### 借鉴 OpenVisio (OpenVisio contributors)
- **文件夹架构聚合图（`toGroupGraph` / `get_group_graph`）**：借鉴 OpenVisio 将细粒度文件依赖折叠为顶层目录或宏观模块组件视图的思想，以目录为节点、聚合 import 为加权边，清晰回答代码库「分几大模块、模块间如何单向/双向依赖」的宏观架构问题。
- **双向 BFS 多跳邻域探索（`get_neighborhood`）**：对齐 OpenVisio 的依赖子图切片能力，沿 import 依赖边进行双向 BFS 扩展（支持出向 `dependencies`、入向 `dependents` 与双向 `both`，严格约束 1–3 跳），帮助智能体以极小 token 成本精准圈定改动文件的多跳影响半径。
- **中心性热点定位（`get_hotspots`）**：借鉴 OpenVisio 基于图拓扑度数与中心性发现代码承重墙的核心逻辑，快速为智能体标定系统中被高频引用、改动风险极高的核心枢纽文件。

### Astrolabe 的 Rust 系统级重写与跨越
虽然汲取了上述项目的算法与协议设计灵感，但 Astrolabe 并非简单的功能拼凑，而是针对智能体长期运行面临的性能瓶颈与脆弱性，使用 **Rust 进行了彻头彻尾的系统级重写与工程突破**：
- **速度飞跃**：将 Node/Python 动辄 12 秒以上的索引耗时大幅压缩至 **首次 1.0 秒**，哈希缓存命中时进一步压制至 **0.10 秒**，彻底消除 MCP 阻塞智能体思考的等待感。
- **内存极限收敛**：OpenVisio 在真实中型仓库上峰值内存高达 4566 MB（4.5 GB），Serena 亦占用近 400 MB 并伴随语言服务器内存无界泄漏；Astrolabe 引入基于显式释放的 `ParserPool` 与基于字节计权的受限 LRU 缓存，将首次峰值压低至 **84 MB**，稳态内存更是仅 **31 MB**（降幅超 99%）。
- **零静默失败（Zero Silent Failures）**：彻底扫除上游基于宽松正则或 `try-catch` 静默吞掉错误导致高达 20% 文件解析丢失的隐患。深入各语言构建系统（Maven/Gradle/Go Module/Cargo/tsconfig/pyproject 等）提取真实语义，在五大语言基准语料上实现 **100% 仓库内导入解析率**，严格标明置信度，绝不用假结果误导智能体改写活代码。
- **极致部署手感**：摒弃复杂的 Python venv、Node/npm 运行时依赖与庞大的重型环境，编译为约 **11 MB 零运行时依赖的纯单二进制可执行文件**，跨平台原生分发，真正实现即开即用、坚如磐石。

## 许可

本项目采用 MIT OR Apache-2.0 双重授权，完整开源合规与上游许可证保留说明详见根目录 [NOTICE](NOTICE) 文件。
