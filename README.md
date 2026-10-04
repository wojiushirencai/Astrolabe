# Astrolabe (星盘)

> **让 AI 编码助手秒懂你的超大代码库 —— 告别疯狂 Grep 烧 Token 与幻觉，快如闪电的确定性代码图谱 MCP 服务。**

Astrolabe
- **名字寓意**：星盘（古代天文学家和航海家在茫茫大海中观测天体、精准定位方位的精密仪器）。
- **技术双关**：**AST**rolabe 前缀融合了 **AST（抽象语法树）** —— 既象征拥有宏观俯瞰全库的天文级拓扑视野，又具备深入语法树叶子节点的微观精确度。
- **运行形态**：以 MCP (Model Context Protocol) 服务运行，原生适配 Claude Code、Cursor、Windsurf、Codex 等主流 AI 编程工具。

---

## 一、你是否也经历过这些 AI 编程的“至暗时刻”？

在几十万行代码的复杂项目里用 AI 编程助手（Claude Code / Cursor / Windsurf），你大概率遇到过这些抓狂场景：

1. **Token 碎钞机，全库盲搜卡到怀疑人生**：
   让 AI 改个需求或排查问题，它第一反应往往是全库疯狂执行 `grep`、`find` 和整文件全量 `Read`。几万甚至几十万 Token 瞬间蒸发，AI 苦转半天圈，最后还因为上下文窗口被无用代码撑爆而“截断失忆”，甚至开始胡说八道（产生代码幻觉）。
2. **MCP 工具配了也白配，AI 聊两句就开始摆烂**：
   明明在客户端里配置了专业工具，但长对话只要多聊几轮，AI 就会发生“注意力漂移”（Agent Drift），把专业工具抛到九霄云外，再次退化回低效的原始文本扫描模式。
3. **老牌分析工具笨重吃内存，机器风扇直接起飞**：
   传统的静态分析或代码图谱工具动辄基于重型 Node 或 Python 运行时，分析千级文件就要耗时十几秒，峰值内存动辄吃掉 3~4 GB，后台常驻甚至能把笔记本拖到卡死。

**Astrolabe 就是为了彻底终结这些痛苦而生的。** 它用 Rust 原生重写，构建了一套“确定性代码拓扑 + 按需精准分流”的高性能引擎，在 AI 思考前为它递上一张高清晰度的代码全景雷达地图。

---

## 二、硬核实测：比老工具快 10 倍，内存节省 99%

在包含 1028 个源文件的真实中型仓库（serena）上的同机实测对比：

| 引擎方案 | 首次全量索引耗时 | 缓存命中（二次索引） | 峰值内存占用 | 解析失败文件数 |
|---|---:|---:|---:|---:|
| **Astrolabe (Rust 原生)** | **1.0 秒** | **0.10 秒** | **84 MB** | **0 (全部精确解析)** |
| Serena (Python + LSP) | 5.7 秒 | — | 373 MB | — |
| OpenVisio (Node.js) | 12.6 秒 | — | 4566 MB (4.5 GB) | 205 个文件解析丢失 |

**通俗点评：**
- **千文件项目 1 秒建立全库拓扑**：Rust 原生系统级解析，文件未修改时二次命中缓存仅需 0.1 秒，AI 眨眼的功夫图谱就已就绪，彻底告别卡顿等待。
- **内存仅 84 MB，仅为老方案的 1/50**：彻底告别动辄数 GB 的内存巨兽，稳态内存甚至仅 31 MB，极度轻巧，后台常驻毫无负担。
- **零静默丢文件**：深入各大主流语言的真实构建规则解析，五大主流语言实现 100% 仓库内导入解析率，不搞“模糊匹配”、不静默吞掉报错，绝不用假结果误导 AI 删改有效代码。

---

## 三、它是怎么做到的？三大底层杀手锏（原理通俗解读）

Astrolabe 为什么能做到又快、又准、还省钱？核心在于三项工程设计：

### 1. 确定性图谱：不烧一分钱 Token 的全库高清雷达
- **人话解读**：看项目的宏观架构分层、查模块间的依赖关系、查改动一个文件会波及哪些上游代码……这些工作**根本不需要耗费昂贵的 LLM 算力去反复推理猜想**。
- **原理**：Astrolabe 直接用 Rust 原生快速扫描源码并解析 `import` / `use` 语句，结合各语言构建配置（Cargo / Go Module / Maven / tsconfig / pubspec 等）把模块路径精确映射成一张依赖拓扑图。查文件、查影响范围 1 秒返回精准结果，**一分钱 Token 都不用花**，而且保证 100% 确定，零模型幻觉。

### 2. 按需 LSP 分流：聪明干活、从不赖着不走的语言专家
- **人话解读**：平时的架构浏览和依赖梳理只走超轻量的图谱；只有遇到“跨文件安全重命名”、“精确函数跳转”、“编译器语法诊断”这些真正需要编译器级精准分析的硬骨头时，Astrolabe 才会在后台悄悄启动对应的语言服务器（LSP）。
- **原理**：一旦这几个重型任务完成，语言服务器闲置超过 5 分钟就会被**自动回收释放**，绝不长期常驻霸占机器内存。兼顾了“编译器级精确”与“极致轻量”。

### 3. 防漂移三级火箭与 Hook 拦截：专治 AI 的“健忘与偷懒”
- **人话解读**：AI 像个容易健忘的学生，聊久了就经常偷懒，放弃专业工具重新开始盲目 grep 和全文裸读代码。
- **原理**：Astrolabe 设计了三层递进约束体系：
  - **L1 握手注入**：会话建立第一秒就将当前仓库绝对根目录和工具使用军规刻进上下文；
  - **L2 行为指导**：提供标准化系统指令与工具守则（`initial_instructions`），明确禁止盲目大面积读代码；
  - **PreToolUse 门禁 Hook**：在底层的工具调用拦截层设卡 —— 一旦抓到 AI 连续 3 次无脑 grep 或连续 3 次大段裸读源码，**直接敲黑板打断并报错拦截**，逼迫 AI 回归星盘图谱工具；同时对特定小范围的精准行阅读智能放行。

---

## 四、10 秒极速上手（傻瓜化配置）

无论是 Claude Code 还是 Cursor，只需两步即可完成接入：

### 第一步：获取二进制文件（约 11 MB 单文件，无运行时依赖）

#### 选项 A：开箱即用二进制（推荐，支持 macOS / Windows）
Astrolabe 提供了已完成本地原生编译的 Release 产物（位于本仓库 `dist/release-binaries/`，亦可在 GitHub Releases 中下载）：

- **macOS（Universal 通用二进制，支持 M1/M2/M3/M4 及 Intel）**：
  ```bash
  # 复制到系统路径并赋予执行权限
  mkdir -p ~/.cargo/bin /usr/local/bin
  cp dist/release-binaries/astrolabe-macos-universal ~/.cargo/bin/astrolabe
  chmod +x ~/.cargo/bin/astrolabe
  xattr -d com.apple.quarantine ~/.cargo/bin/astrolabe 2>/dev/null || true
  ln -sf ~/.cargo/bin/astrolabe /usr/local/bin/astrolabe
  
  # 验证安装
  astrolabe -h
  ```
- **Windows (x64)**：
  解压 `dist/release-binaries/astrolabe-windows-x64.zip`，将 `astrolabe-windows-x64.exe` 放置在如 `C:\Tools\astrolabe\astrolabe.exe`，并将其添加到系统环境变量 `PATH` 中。

#### 选项 B：从源码编译（需 Rust 1.90+）
```bash
git clone <repo-url> astrolabe && cd astrolabe
cargo build --release
# 产物即为：target/release/astrolabe（单文件约 11 MB）
```

#### 选项 C：npx 免安装运行（Node 环境）
```bash
npx -y astrolabe /path/to/your/repo
```

> **详细平台支持与配置说明**：请参阅完整文档 [docs/INSTALL_GUIDE.md](docs/INSTALL_GUIDE.md)。

---

### 第二步：配置 AI 编程客户端（复制粘贴即用）

#### 1. Claude Code 配置
在全局配置 `~/.claude.json` 或项目级根目录 `.mcp.json` 中添加：

```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "/usr/local/bin/astrolabe",
      "args": [".", "--context=claude-code"],
      "env": {}
    }
  }
}
```
*(Windows 用户将 command 替换为实际路径，如 `"C:/Tools/astrolabe/astrolabe.exe"`)*

> **注入防漂移 Hook（强烈推荐）**：
> 将以下配置写入项目 `.claude/settings.json`（或全局 `~/.claude/settings.json`），即可开启自动防偷懒拦截：
> ```json
> {
>   "hooks": {
>     "PreToolUse": [
>       {
>         "matcher": "",
>         "hooks": [
>           {
>             "type": "command",
>             "command": "astrolabe hooks remind --client=claude-code"
>           }
>         ]
>       }
>     ],
>     "SessionEnd": [
>       {
>         "matcher": "",
>         "hooks": [
>           {
>             "type": "command",
>             "command": "astrolabe hooks cleanup"
>           }
>         ]
>       }
>     ]
>   }
> }
> ```

#### 2. Cursor 配置
在项目根目录 `.cursor/mcp.json` 中添加配置：

```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "/usr/local/bin/astrolabe",
      "args": [".", "--context=cursor"]
    }
  }
}
```

#### 3. Codex / Windsurf 等通用客户端
- **Codex**：在 `~/.codex/config.toml` 中配置 `args = [".", "--context=codex"]`。
- **通用客户端**：使用标准输入输出（stdio），日志统一走 stderr（stdout 为 JSON-RPC 通道），完全符合标准 MCP 规范。

---

## 五、AI 助手可以用它做什么？（核心工具速查）

Astrolabe 向 AI 暴露了精炼而强悍的工具集，按职责清晰划分为两层：

### 1. 图谱层工具（常驻秒级响应，0 外部依赖）

| 工具名称 | 核心参数 | 它能帮 AI 解决什么？（大白话解读） |
|---|---|---|
| `resolve_context` | `task_description` | **【核心主入口】** 丢给它一句“我要接微信支付”，它自动把相关代码骨架、依赖邻域与核心文件聚合整理好，AI 接单立刻能干活！ |
| `get_repo_skeleton` | — | **全库骨架鸟瞰**：按重要性高低列出核心文件与公开符号，一眼看穿项目全貌。 |
| `get_group_graph` | `depth` | **模块架构大图**：按目录把细碎文件聚合成顶层组件图，搞清“系统分几大模块、模块间怎么调用”。 |
| `get_neighborhood` | `target`, `depth`, `direction` | **改动影响面评估**：双向探索 1~3 跳依赖，改动这个文件到底会波及哪些上游和下游？ |
| `get_dependents` | `target`, `direction` | **依赖精准反查**：查谁在导入/使用这个文件（准确率 100%）。 |
| `get_hotspots` | — | **找代码承重墙**：定位全库中被引用最多、改动风险最高的核心枢纽文件。 |
| `find_symbol` | `query` | **快速定位符号**：按名字或子串秒搜函数、类或类型定义，直接返回 `文件:行号` 锚点。 |
| `search_code` | `query`, `path_filter` | **确定性代码全文检索**：带正则与路径过滤的精准文本查找。 |
| `trace_calls` | `symbol` | **调用链分析**：快速追踪函数的上下游调用关系。 |
| `get_languages` | — | **代码库盘点**：统计语言种类、文件数与代码行数，并标注 LSP 状态 Ready / needs_install / AST-only。 |

### 2. 精确层工具（按需拉起语言服务器，精确到编译器级别）

| 工具名称 | 核心参数 | 它能帮 AI 解决什么？（大白话解读） |
|---|---|---|
| `find_references` | `symbol`, `path` | **编译器级引用查找**：找到全库中所有调用该符号的准确位置，绝不漏报。 |
| `goto_definition` | `path`, `symbol` | **精准定义跳转**：利用 LSP 语义绑定直达真实定义处，而非靠名字瞎猜。 |
| `get_diagnostics` | `path` | **改后即时体检**：获取当前文件的语法报错与类型错误，确认写完的代码是否成立。 |
| `get_symbol_info` | `path`, `symbol` | **符号语义信息**：LSP hover（文档串 / 类型 / 签名），不必整文件阅读。 |
| `ensure_language_server` | `language`, `confirm_install` | **会话确认安装语言服务器**：默认只返回 needs_install 计划（latest，不下载）；用户同意后再 `confirm_install=true` 调用安装器。精确工具 Unavailable 时先问用户再调此工具。 |
| `plan_rename` | `symbol`, `new_name`, `path` | **重命名预演**：在安全修改前先列出改动清单供审查，绝不贸然写盘。 |
| `apply_rename` | `symbol`, `new_name`, `path` | **安全批量重命名**：真正执行跨文件改名；写后自动重新编译解析，一旦报错立即安全回滚。 |

---

## 六、进阶配置与运行细节

### 1. 仓库定位逻辑
启动时按以下顺序判定要索引的仓库根目录：
1. **显式指定优先**：CLI 位置参数（如 `astrolabe /path/to/repo`）或环境变量 `ASTROLABE_ROOT`。用户明确给出的路径直接采用，不做任何拦截；目录里装了多个子项目时，最多提醒一句，不替用户做主。
2. **默认 `.` 时，先向上找**：从当前目录一路向上找最近的 `.git`（支持普通目录、worktree、submodule）或 `.serena/project.yml`，找到即为仓库根——哪怕你是在仓库深处的子目录里启动，也能正确解析到仓库根。
3. **向上找不到，再看脚下**：比如在一个普通的非 git 目录里启动。此时检查当前目录的**直接子目录**里有没有 `.git`，也就是“一个目录装了多个项目”的情况——命中则进入多项目模式（见下一节）。
4. **都不沾边，数数把关**：子目录里也没有 `.git` 时，清点当前目录的文件数量。不超过 2 万个，当作普通目录正常索引；超过 2 万个则**拒绝启动**，并打印两条出路：进入具体项目目录再启动，或用 `ASTROLABE_ROOT` 显式指定。上限可用 `ASTROLABE_MAX_ROOT_FILES` 调整。

**为什么要立这些规矩？** 很多人把项目集中放在一个“收纳目录”里（比如 ~/Projects 装了十几个仓库）。旧版本在这种目录里启动，会把几十万个文件当成一个巨型仓库硬啃，内存和 CPU 直接被打爆——真实案例：一个 35 万文件的目录吃掉了 5 GB 内存。新规则兜住底线：**无论在哪个目录启动，都不会失控**。

> **安全机制**：每条工具输出第一行均包含 `index_root: /abs/path`。当路径与当前正在编辑的仓库不匹配时，智能体会明确收到提示，绝不会张冠李戴。传入不存在的目录会直接报错退出，不会静默索引错地方。

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
- `readonly`：**只读安全模式**（排除唯一写盘工具 `apply_rename`，保留仅供审查预演的 `plan_rename`，适用于安全审计、演示账号或只读审查代理）。

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
| `ASTROLABE_SLICE_READ_MAX` | 切片精读最大行数：带 `limit ≤ N` 的 Read 调用视为合法精读，不计入滥用 | `200` |
| `ASTROLABE_READ_THRESHOLD` | 连续全文件 Read 调用 deny 阈值（达到该次数触发拦截） | `3` |
| `ASTROLABE_DENY_SILENCE_SECS` | deny 后静默窗口秒数（窗口内 hook 放行，不累计计数） | `120` |
| `RUST_LOG` | 日志级别（`error` / `warn` / `info` / `debug`） | `info` |

### 5. 索引缓存与增量更新
- **哈希增量缓存**：解析结果按内容哈希持久化于被索引仓库下的 `.astrolabe/` 目录（自动写入 `.gitignore`，不污染版本控制）。未变更的文件二次索引时跳过语法解析，冷启动 1.0 秒、热启动 0.1 秒。
- **只补改动，不掀屋顶**：保存文件后只重新解析改动的那几个文件、只更新受影响的部分，不再整库重建；改的是文档、日志这类不参与索引的文件时，连重建都不会触发。
- **多开不打架**：同一个仓库开多个 AI 会话（多个 Astrolabe 进程）时，多个进程里只有一位“账房先生”负责写磁盘缓存，其他人只用自己的内存副本——互不抢锁，也不重复干活。
- **答案出场前的自动体检**：每次查询返回前，顺带核对结果涉及的文件是不是最新版本。个别文件过期就当场补修，快到无感；大面积过期时不装没事，会在结果里如实标注“结果可能过期”，并触发后台补建。当场补修的文件数上限由 `ASTROLABE_BARRIER_MAX` 控制（默认 64）。
- **漏网之鱼的兜底**：极少数情况下，操作系统的文件通知可能漏发。Astrolabe 会低频巡检补漏（默认每 30 分钟一次，检测到异常会自动加大巡检频率），间隔由 `ASTROLABE_SAFETY_SECS` 调整。
- **启动期间改文件不丢失**：先登记文件清单、再建图。建图期间保存的文件，会在建完后自动补进索引，不会出现“刚启动时改的代码查不到”。
- **重建期间不断服务、坏编辑不瘫痪**：后台自动监听文件变更，改动时平滑更新图谱；重建期间旧索引继续对外服务，保存文件不会中断工具调用。一次语法错误的坏编辑只记日志并保留旧图，绝不使服务瘫痪。

### 6. 语言服务器的生命周期管理
精确层调用的语言服务器完全实现智能化按需调度：
- **空闲 5 分钟自动释放**：单次问答中保持热态，跨任务后自动回收，绝不占用系统常驻资源。
- **内存防护上限**：稳态内存上限设为 2 GiB，硬上限 4 GiB，防止失控的语言服务器拖垮整机。
- **真就绪判定**：监控语言服务器启动后必须连续安静 1 秒无待处理任务，才正式标记为就绪，彻底杜绝冷索引期返回虚假空结果导致误删代码的问题。

### 7. 语言支持与 LSP 矩阵

Astrolabe 深度适配主流语言构建系统与语言服务器，实现无 LLM 消耗的文件级确定性图谱与按需编译级精确分析：

| 语言 | 扩展名 | LSP 语言服务器 | 环境变量覆盖 | 备注 |
|---|---|---|---|---|
| **Python** | `.py`, `.pyi` | `pyright` | `ASTROLABE_LSP_PYTHON` | `npm i -g pyright`；支持 `pyproject.toml`、PEP 420 命名空间包 |
| **Go** | `.go` | `gopls` | `ASTROLABE_LSP_GO` | `go install golang.org/x/tools/gopls@latest`；支持 `go.mod`、`go.work` |
| **Rust** | `.rs` | `rust-analyzer` | `ASTROLABE_LSP_RUST` | `rustup component add rust-analyzer`；支持 Cargo workspace |
| **Java** | `.java` | `jdtls` | `ASTROLABE_LSP_JAVA` | `brew install jdtls`；支持 Maven、Gradle 源根解析 |
| **TypeScript / JS** | `.ts`, `.tsx`, `.js`, `.jsx`, `.mts`, `.cts`, `.mjs`, `.cjs` | `typescript-language-server` | `ASTROLABE_LSP_TYPESCRIPT` | 支持 `tsconfig.json` paths、Node workspace `package.json` |
| **C / C++** | `.c`, `.h`, `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`, `.ipp` | `clangd` / `ccls` | `ASTROLABE_LSP_C`, `ASTROLABE_LSP_CXX` | `clangd --background-index`；支持 `#include` 路径解析 |
| **Swift / ObjC** | `.swift`, `.m`, `.mm` | `sourcekit-lsp` / `clangd` | `ASTROLABE_LSP_SWIFT`, `ASTROLABE_LSP_OBJC`, `ASTROLABE_LSP_OBJCPP` | macOS Command Line Tools 自带 `sourcekit-lsp`；ObjC 可回退 `clangd` |
| **PHP** | `.php` | `intelephense` / `phpactor` | `ASTROLABE_LSP_PHP` | `npm i -g intelephense`；支持 include/require 路径解析 |
| **Vue** | `.vue` | `vue-language-server` (Volar) | `ASTROLABE_LSP_VUE` | SFC 内嵌 `<script>` / `<script setup>` 提取与语义解析 |
| **Dart** | `.dart` | `dart language-server` | `ASTROLABE_LSP_DART` | 随 Dart / Flutter SDK 自带；`pubspec.yaml` 行级解析与多 pubspec monorepo 支持；`rootUri` 置 `null` 规避重复分析 |
| **C#** | `.cs`, `.csx`, `.razor`, `.cshtml` | `Microsoft.CodeAnalysis.LanguageServer`（可选 Roslyn） | `ASTROLABE_LSP_CSHARP` | `.cs` / `.csx` / `.razor` / `.cshtml` 同为 C#（不另建语言变体，没有 Razor 语言）。`.csx` 用 C# 语法（含顶层语句与 local function）。`.razor` / `.cshtml` 在 tree-sitter 之前抽出 C#（`@code`、`@{ }`、行首 `@using` / `@inherits`；标记不当成 C# 符号；换行保留，列号不映射）。没有工程时 Roslyn 加载不了松散 `.csx`，这类脚本只走语法索引。命名空间 using 扇出到每个声明文件（不进 PageRank），`_ViewImports.cshtml` 的 `@using` 走同一解析（仅仓内命名空间；`System` / NuGet 为 None）。`ProjectReference` 记为 Import。Roslyn 为 `dotnet <dll> --stdio`，需要 .NET 10 runtime（工程目标框架可更旧），不静默下载，不是 OmniSharp / csharp-ls。Razor 没有语言服务器：现有启动器只认 Roslyn 的 `solution/open` / `project/open`，不是 Razor 协议，因此没有 `ASTROLABE_LSP_RAZOR`。 |
| **Visual Basic .NET** | `.vb` | 同一个 `Microsoft.CodeAnalysis.LanguageServer` | `ASTROLABE_LSP_CSHARP`（与 C# 共用，无第二个变量） | 只索引 `.vb`（不含 `.bas` / `.frm`）。`Imports` 命名空间扇出到每个声明文件（Namespace，不进 PageRank）；类型 `Imports` 仅在恰好一个仓内目标时记为 Import。`.vbproj` 的 `ProjectReference` 与 C# 一样记在项目元数据上，工程文件本身不是图节点，不伪造边。`.sln` / `.vbproj` 走同一 `dotnet <dll> --stdio`。 |

---

## 七、防漂移拦截机制详解 (Anti-Drift Hooks)

即使有 System Prompt 军规约束，大模型在上下文拉长后依然可能出现注意力衰退（Agent Drift），退化为盲目使用 `grep` / `read`。Astrolabe 提供了硬核的门禁拦截机制：

### 核心拦截与放行规则
- **拦截（Deny）触发条件**：在没有调用 Astrolabe 符号工具的情况下，只要检测到 AI 连续 3 次无脑 grep、连续 3 次裸读源码文件（基于 28 种支持语言源码后缀过滤，可通过 `ASTROLABE_READ_THRESHOLD` 调整；未支持语言常规读取不予拦截），或连续 4 次混合调用，Hook 将坚决拦截并输出引导警告。
- **切片精读放行**：带有 `limit ≤ 200`（可通过 `ASTROLABE_SLICE_READ_MAX` 调整）的 Read 调用视为合法精读，不计入滥用计数。同样，`head -n N` / `tail -n N` / `sed -n 'A,Bp'` 等有界切片命令亦放行。
- **计数重置**：AI 只要调用任意 Astrolabe 工具（如 `resolve_context`、`find_symbol` 等），连续计数立即清零；若连续调用间隔超过设定时间，亦自动重置。
- **智能放行窗口**：触发 Deny 后进入 120 秒静默宽容窗口（可通过 `ASTROLABE_DENY_SILENCE_SECS` 调整）。在此窗口内 Hook 放行且不累加计数，避免在特定需要连续细查的合法场景中打断正常排查。
- **状态存储与清理**：会话状态持久化于 `~/.astrolabe/hook_data/<session_id>/counter.json`；会话结束时通过 `SessionEnd` 自动清理，不留垃圾。

---

## 八、质量契约：五大主流语言 100% 真实解析验收

质量不是口号，必须经得起真实复杂开源仓库的考验。Astrolabe 包含严苛的自动化质量验收门禁（`scripts/verify.sh`）：

### 1. 真实开源项目内部 Import 解析率（2985 条依赖语句基准）

| 语言 | 测试语料项目 | 锁定 Commit / Tag | 仓库内真实 Import 数 | 旧版方案解析率 | Astrolabe 实测解析率 |
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

---

## 九、本地开发与构建命令

```bash
cargo build                                          # 构建开发版本
cargo test --workspace                               # 运行全部单元与集成测试（190+ 项）
bash scripts/verify.sh                               # 运行真实开源项目语料验收
cargo run --release --example index_bench -- <repo>  # 测试任意指定仓库的索引耗时与内存
```

---

## 十、致敬与技术渊源 (Acknowledgements & Lineage)

Astrolabe 站在开源代码智能体探索的坚实肩膀之上。在架构设计与关键功能实现中，我们深度汲取并由衷致敬 **[Serena](https://github.com/oraios/serena)** 与 **[OpenVisio](https://github.com/openvisio/openvisio)** 两个优秀项目的工程实践与理论思想：

### 汲取自 Serena (Oraios AI) 的工程智慧
- **三级提示词范式（Tiered Instructions L1–L3）**：学习 Serena 层次分明的 Agent 引导思路 —— 在握手阶段确立项目根与边界（L1），在会话中通过 `initial_instructions` 注入严密的行为纪律（L2），在工具元数据中给出精准描述（L3），有效约束大模型遵循工具使用准则。
- **多客户端兼容性对齐（Client Alignment）**：吸纳 Serena 在异构环境下的适配策略，包括支持 `.git` 与 `.serena/project.yml` 双标记就近项目探测（彻底杜绝子仓库与 worktree 越界），以及对 Claude Code 和 Cursor 各自 output 缺陷的精细处理。
- **PreToolUse 防漂移拦截体系**：继承 Serena 主动对抗 Agent 遗忘偏置的治理思路，通过在 PreToolUse 阶段对高频盲目 grep / read 进行硬性计数拦截，主动将偏离轨道的 AI 重新拉回高效的图谱工具轨道。

### 汲取自 OpenVisio (OpenVisio contributors) 的算法思想
- **目录级架构聚合图（`get_group_graph`）**：借鉴 OpenVisio 将微观文件依赖折叠为顶层目录或宏观模块组件视图的思想，清晰解答“系统分几大模块、模块间如何交互”的宏观问题。
- **双向多跳邻域探索（`get_neighborhood`）**：对齐 OpenVisio 的依赖子图切片能力，沿导入边进行 1~3 跳双向 BFS 扩展，帮助智能体以极少 Token 精准圈定改动的影响半径。
- **中心性拓扑热点探测（`get_hotspots`）**：借鉴 OpenVisio 基于图拓扑度数与中心度发现系统“承重墙”的算法，迅速标定系统中改动风险最高的关键枢纽。

### Astrolabe 的系统级重写与实质飞跃
在继承上述前沿思想的同时，Astrolabe 针对传统工具在长周期生产环境下的性能瓶颈与脆弱性，**采用 Rust 进行了全面的系统级工程重写**，实现了质的跨越：
1. **速度大幅飞跃**：将以往脚本语言动辄十几秒的索引时间压缩至 **首次 1.0 秒**，哈希缓存命中时更是低至 **0.10 秒**，彻底消除 MCP 导致 AI 陷入长时间等待的停顿感。
2. **内存压减 99%**：利用显式资源释放的 `ParserPool` 与受限字节计权 LRU 缓存，将曾经高达 4.5 GB 的内存峰值压低至 **84 MB**，稳态常驻仅 **31 MB**，彻底消除语言服务器内存泄漏隐患。
3. **消除静默失败**：摒弃传统正则粗暴扫描或用 `try-catch` 静默忽略解析失败的做法，深入语言构建语义，实现五大语言主流项目 **100% 导入解析率**，保证每一条返回给 AI 的结果都有据可查。
4. **单二进制极致分发**：彻底摆脱复杂的 Python 虚拟环境或 Node 运行时依赖，打包为仅 **约 11 MB 的独立二进制文件**，开箱即用，坚若磐石。

---

## 🔗 友情链接

* [LINUX DO](https://linux.do) —— 真诚分享、友好讨论的技术社区，本项目的交流与反馈也发布于此

---

## 十一、开源许可证

本项目采用 **MIT OR Apache-2.0** 双重开源授权。完整的开源合规说明与上游许可证保留信息，请参阅根目录 [NOTICE](NOTICE) 文件。
