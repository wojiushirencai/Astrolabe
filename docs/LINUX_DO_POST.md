# 【开源/硬核实测】别再让 Claude Code 盲目 Read 烧光 Token 了：我们用 Rust 重构了代码图谱引擎 Astrolabe（1.0s / 84MB，零 LLM 消耗，机械级防漂移）

**标签**：`#开发调优` `#AI编程` `#Rust` `#ClaudeCode` `#Cursor` `#开源项目`

各位 Linux.do 的佬友们好！

今天想和大家分享一个我们在日常使用 **Claude Code / Cursor / Windsurf** 等 AI 编程智能体时，被逼出来的开源项目 —— **Astrolabe（星盘）**。

如果你日常深度依赖 AI 辅助写代码，面对中大型代码仓库（几百到几千个文件），你大概率遇到过这些让人血压升高的场景：

1. **盲目 grep 与无脑 Read 烧光 Token**：Agent 只要接手一个稍微复杂的任务，就开始满世界 `grep`、`rg`，然后动辄把几千行的源文件全量 `Read` 进上下文。几轮对话下来，几十万 Token 瞬间蒸发，账单飞涨不说，上下文窗口被无意义代码淹没后，模型推理能力断崖式“降智”；
2. **严重的 Agent 漂移（无视 MCP）**：明明在客户端配齐了各种强大的 MCP 工具，但随着会话变长，模型产生了“MCP 遗忘症”，漂移回使用内置 bash/grep 的惰性模式；
3. **现有图谱方案“傻大粗”**：有的工具基于 Node/WASM，大仓库动辄吃掉 4~5GB 内存甚至直接 OOM 崩溃；有的基于 Python+LSP，冷启动极慢、常驻后台内存持续泄漏。

为了彻底解决这三个顽疾，我们用 **100% 纯 Rust** 重新打造了面向 AI 智能体的确定性代码图谱引擎 —— **Astrolabe**。

---

## 一、真实压测：硬核实测数据对比

极客社区不搞虚的，先看同一测试基准（以真实的 Serena 仓库为例，包含 1028 个文件、上万个符号）下的实测资源与耗时对比：

| 引擎 / 方案 | 技术栈 | 首次索引耗时 | 缓存命中耗时 | 峰值内存 (RSS) | 提取符号数 | 解析失败文件数 | 仓库内 Import 召回率 |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **Astrolabe (本项目)** | **纯 Rust + Tree-sitter** | **1.0 s** | **0.10 s** | **84 MB** *(稳态 31MB)* | **10,051** | **0** | **100%** |
| **OpenVisio** | Node.js + WASM | 12.6 s | — | 4,566 MB *(~4.5GB)* | 4,979 | 205 (静默丢弃) | ~15% |
| **Serena** | Python + LSP 常驻 | 5.7 s | — | 373 MB *(随查询飙升至700MB+)* | — | — | ~12% (仅根目录) |

### 数据背后的血泪教训：
- **4.5GB vs 84MB**：先前方案按文件分配 Tree-sitter Parser 且未能显式释放语法树，导致内存暴涨到 4.5GB 甚至撑爆容器；Astrolabe 采用全局语言级 `ParserPool`，Tree 资源用完即丢，搭配有界内存缓存（`FileCache`），内存死死锁在两位数 MB。
- **205 个静默失败 vs 0 失败**：旧方案大量采用 `catch {}` 吞掉异常，导致 20% 的源文件被悄悄丢弃，AI 拿着残缺的图谱瞎猜；Astrolabe 对任何解析异常与不确定性显式暴露，拒绝掩耳盗铃。
- **100% 导入召回**：通过深入适配五门语言构建规范（Python src-layout/PEP 420、Go module、Java Maven 惯例源根、Rust workspace/crate、TS paths），把仓库内 import 解析成功率从前人的 ~15% 彻底推到了 **100%**。

---

## 二、核心亮点：它是如何帮 Claude / Cursor 省钱提效的？

### 1. 确定性代码图谱：零 LLM 调用、零 Token 消耗
- Astrolabe 建图 **不调用任何外部 LLM、不做向量 Embedding 检索**。
- 它完全依赖本地 Tree-sitter 语法树解析与构建配置拓扑运算。
- 启动即在后台完成全局拓扑图构建，AI 发起 `resolve_context`、`get_dependents`、`get_repo_skeleton` 查询时，直接返回**精确的文件依赖链、架构热点与 `path:line` 符号锚点**。
- **结果**：AI 不再需要盲目 Read 上百个文件来“脑补”项目架构，只读精准相关的骨架切片，单次探索节省 80% 以上的上下文 Token。

### 2. 动静分流架构：图谱常驻 + 按需 LSP，绝不让后台内存泄漏
我们经过实测发现了一个残酷的事实：
> **文件级 import 图对语言服务器的召回率是 100%，而名字匹配的符号级调用图召回率只有 18% (Python) ~ 66% (TypeScript)。**

因此，Astrolabe 采用了**静态路由分流设计**：
- **宏观文件级查询（依赖、邻域、影响面、承重热点）**：走 100% 确定性的本地图谱，毫秒级响应；
- **微观符号级操作（精准定义跳转、查找精确引用、重命名计划）**：按需拉起对应的 LSP（如 rust-analyzer, pyright, gopls, ts_ls, jdtls）；
- **5 分钟空闲回收**：用完后 5 分钟无请求自动 Kill 掉语言服务器进程，打破 Python/Node 方案中 LSP 驻留导致内存从 180MB 一路飙到 700MB+ 的魔咒。
- **置信度是一等公民**：所有工具输出明确附带 `exact`（编译器支持）、`scoped`（构建解析）、`syntactic`（词法匹配）或 `unknown`（未安装或无法确定），从不把模糊推断假装成真理。

### 3. 独家 PreToolUse Hooks 机械级防漂移机制（治好 Agent 的 MCP 遗忘症）
这是我们在 Claude Code 深度实践中打磨出来的“杀手锏”功能：
- **机械级硬拦截**：挂载在 Claude Code 的 `PreToolUse` 阶段。当 Agent 在未调用 Astrolabe 符号工具的情况下：
  - 连续 3 次调用 `grep`
  - 连续 3 次全量 `Read` 代码文件
  - 连续 4 次混合无脑探索
  - 👉 **Hook 会直接截断并返回 Deny**，强制注入提示词，引导模型改用 Astrolabe 的 `search_code` / `find_symbol` / `resolve_context` 等低 Token 消耗工具。
- **优雅放行切片精读**：我们深刻理解开发者的排查需求，因此 hook 做了精细化放行 —— **只要 Agent 使用带有 `limit <= 120` 的 offset/limit 切片读取，完全视为合法精准排查，绝不计入滥用计数！**
- **120 秒静默保护**：拦截后自动给予 120 秒冷却期，绝不打断紧急的调试节奏。

---

## 三、开源传承与诚挚致谢

Astrolabe 的诞生并非空中楼阁，它站在了开源社区优秀先驱的肩膀上：
- **致敬 Serena ([Oraios AI](https://github.com/oraios/serena))**：感谢 Serena 团队在代码智能体工具协议、三级提示词工程（L1/L2/L3）以及防漂移机制上的深刻洞察。Astrolabe 继承并严谨对齐了 Serena 的 context 规范、项目探测逻辑与提示词治理哲学。
- **致敬 OpenVisio ([OpenVisio contributors](https://github.com/openvisio/openvisio))**：感谢 OpenVisio 在多语言代码可视化与拓扑依赖建模上的开拓性探索，为我们提供了宝贵的图谱构建灵感。

**为什么选择用 Rust 100% 重构？**
我们深爱前人探索出的方向，但在实际高强度工程落地中，Node.js 运行时的 GC 抖动、WASM 跨语言内存释放缺陷、Python 常驻进程的内存泄漏，让中大型项目的使用体验大打折扣。用 Rust 重写，不仅把常驻内存压到 30MB~80MB、耗时压进 1 秒，更让我们能够实现基于 Redb 的无锁持久化、原生 OS 文件监听（notify 空闲 0% CPU）以及坚固的进程沙箱控制。

---

## 四、极速开箱：一键运行与配置

Astrolabe 提供了极致的开箱体验，无论你是否有 Rust 环境均可零门槛使用。

### 1. npx 一键免安装运行（零依赖）
无需安装 Rust 工具链，直接使用 Node shim（自动匹配平台原生二进制，带 SHA256 校验）：
```bash
npx -y astrolabe .
```

### 2. 下载预编译二进制（GitHub Releases）
所有 Release 均附带跨平台二进制包与 `SHA256SUMS` 校验和文件：
- **macOS (Apple Silicon M1/M2/M3/M4)**: `astrolabe-macos-arm64.tar.gz`
- **macOS (Intel x86_64)**: `astrolabe-macos-x86_64.tar.gz`
- **macOS 通用二进制 (Universal)**: `astrolabe-macos-universal.tar.gz`
- **Windows (x64)**: `astrolabe-windows-x64.zip`

**macOS 快速安装（放入 PATH）：**
```bash
# 解压后将二进制移入 local bin
sudo cp astrolabe /usr/local/bin/astrolabe
sudo chmod +x /usr/local/bin/astrolabe
xattr -d com.apple.quarantine /usr/local/bin/astrolabe 2>/dev/null || true
```

---

### 3. AI 工具配置示例

#### ① Claude Code 全局配置
编辑 `~/.claude.json`，在 `mcpServers` 中增加：
```json
{
  "mcpServers": {
    "astrolabe": {
      "type": "stdio",
      "command": "astrolabe",
      "args": [
        ".",
        "--context=claude-code"
      ]
    }
  }
}
```

**启用防漂移 Hooks（写入 `~/.claude/settings.json`）：**
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

#### ② Cursor 配置
打开 Cursor 设置 -> `Features` -> `MCP Servers`，或编辑 `~/.cursor/mcp.json`：
```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "astrolabe",
      "args": [
        ".",
        "--context=cursor"
      ]
    }
  }
}
```
*(Windows 用户将 command 设为绝对路径，如 `C:/Tools/astrolabe/astrolabe.exe`)*

---

## 五、开源链接与协议

- **GitHub 仓库**：[https://github.com/wojiushirencai/Astrolabe](https://github.com/wojiushirencai/Astrolabe)
- **开源协议**：双开源协议 **MIT OR Apache-2.0**，对商业与个人二次开发均完全友好。

如果 Astrolabe 帮你在日常 Coding 中省下了一大笔 Token 账单，或者治好了你家 AI 的“降智遗忘症”，欢迎去 GitHub 点个 Star ⭐️！

非常期待各位佬友在评论区留下你们的实测体验、压测数据和吐槽建议，我们随时在线交流优化！
