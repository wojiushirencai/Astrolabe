# Astrolabe AI 安装与客户端配置指南（中文）

英文版见 [INSTALL_GUIDE.md](INSTALL_GUIDE.md)。最后更新 2026-10-25（v0.17.0）。

本指南覆盖安装与各客户端的 MCP 配置。

---

## 一、获取二进制

唯一的官方二进制来源是 [GitHub Releases](https://github.com/wojiushirencai/Astrolabe/releases)。归档命名：`astrolabe-<版本>-<target>.tar.gz`（Windows 为 `.zip`），每个版本附 `SHA256SUMS`。

| 平台 | target | 示例（v0.17.0，压缩包大小） |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | 9,728,734 B |
| macOS Intel | `x86_64-apple-darwin` | 10,078,526 B |
| Linux x64（glibc） | `x86_64-unknown-linux-gnu` | 10,238,301 B |
| Linux x64（静态，musl） | `x86_64-unknown-linux-musl` | 10,406,095 B |
| Linux arm64 | `aarch64-unknown-linux-gnu` | 8,507,630 B |
| Windows x64 | `x86_64-pc-windows-msvc`（`.zip`） | 9,666,430 B |

压缩包大小取自 [v0.17.0](https://github.com/wojiushirencai/Astrolabe/releases/tag/v0.17.0) Release 资源。

## 二、安装

### 方式 1：install.sh（macOS / Linux，会校验 sha256）

```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
# 指定版本 / 目录：
ASTROLABE_VERSION=0.17.0 ASTROLABE_INSTALL_DIR="$HOME/.local/bin" sh install.sh
```

脚本只做：下载对应平台归档和 `SHA256SUMS` → 校验 → 把 `astrolabe` 放到安装目录。不修改 PATH 或任何配置文件。

### 方式 2：手动下载

```bash
V=0.17.0; T=aarch64-apple-darwin   # 按上表选择 target
curl -fsSLO https://github.com/wojiushirencai/Astrolabe/releases/download/v$V/astrolabe-$V-$T.tar.gz
curl -fsSLO https://github.com/wojiushirencai/Astrolabe/releases/download/v$V/SHA256SUMS
grep "astrolabe-$V-$T.tar.gz" SHA256SUMS | sha256sum -c -     # macOS 用 shasum -a 256 -c -
tar -xzf astrolabe-$V-$T.tar.gz && install -m 0755 astrolabe ~/.local/bin/astrolabe
```

### 方式 3：Windows

下载 `astrolabe-<版本>-x86_64-pc-windows-msvc.zip`，用 `Get-FileHash -Algorithm SHA256` 对照 `SHA256SUMS`，解压出 `astrolabe.exe` 放到固定目录并加入 `PATH`。（Windows 路径未在本次改动中实机验证。）

### 方式 4：npm

npm 包名为 **`@zlnap/astrolabe`**（二进制名仍是 `astrolabe`），平台包为 `@zlnap/<platform>`，已在 npm 个人 scope `zlnap` **发布**（[v0.17.0](https://github.com/wojiushirencai/Astrolabe/releases/tag/v0.17.0)；无需建 org；npm org `astrolabe-ai` 已被他人占用）。安装：`npx -y @zlnap/astrolabe .`，再 `npx -y @zlnap/astrolabe setup` 接入客户端。注意：npm 上未加 scope 的 `astrolabe` 和 `astrolabe-ai` 是与本项目无关的他人的包，请不要用它们。

### 方式 5：从源码编译（Rust 1.90+）

```bash
git clone https://github.com/wojiushirencai/Astrolabe.git && cd Astrolabe
cargo build --release --bin astrolabe     # 产物 target/release/astrolabe
```

验证：`astrolabe --version`。

---

## 三、各 AI 编程工具 MCP 配置配方

### 一键配置（推荐）

安装 npm 包后（或直接用 `npx`）运行：

```bash
npx -y @zlnap/astrolabe setup
```

会探测本机的 Claude Code、Cursor、Codex、Windsurf、Cline、Roo Code、Continue、**OpenCode**、**DeepSeek Harness（DSH）**、**Codebuddy**、**Grok Build**、**Kimi Code**、**ZCode**，交互多选要写入的客户端，并把 Astrolabe **合并**进各自配置（不会清空其它 server）。默认启动命令为 `npx -y @zlnap/astrolabe`，有 `--context=` 的客户端会带上（上述四者走 `default` 温和级，不写 `--context`）。常用参数：`--yes`、`--clients=cursor,claude-code,opencode,dsh,codebuddy,grok,kimicode,zcode`、`--dry-run`、`--native`、`--list`、`--with-hooks`（安装 OpenCode 守卫插件，以及 Cursor / Codebuddy / Grok / Kimi / ZCode 的 hooks 配方）、`--no-cursor-permissions`。

- **Cursor：** MCP 写入 `~/.cursor/mcp.json`（`--context=cursor`）；可选 `--with-hooks` 写入 `~/.cursor/hooks.json`（`preToolUse` → `astrolabe hooks remind --client=cursor`，matcher 含 `mcp__astrolabe__.*`；`sessionEnd` cleanup）。
- **OpenCode：** 合并进 `~/.config/opencode/opencode.jsonc`（或已有的 `.json` / 旧版 `config.json`，或 `~/.opencode/…`）。自动识别 V1（`mcp.<name>` + `enabled`）、V2（`mcp.servers`）或类 Claude 的 `mcpServers`；在父对象已存在时尽量保留 JSONC 注释。可选 `--with-hooks` 写入 `~/.config/opencode/plugins/astrolabe-guard.ts`。
- **DeepSeek Harness：** 把自带的 `astrolabe-dsh-guard` 装进 `~/.dsh/profiles/**/node_modules`，并合并写入 `~/.dsh/cordis.patch.yml` 的托管块（不碰 `orca-managed-dsh-hooks`）。DSH 的 MCP 是按仓库在 UI 里配的，setup 不会代写。
- **Codebuddy：** MCP 写入 `~/.codebuddy/.mcp.json`（若已有 `mcp.json` / `~/.codebuddy.json` 则合并进优先已存在的那个）；可选 `--with-hooks` 合并进 `~/.codebuddy/settings.json` 的 CC 兼容 `hooks`（PreToolUse + SessionEnd）。
- **Grok Build：** MCP 合并进 `~/.grok/config.toml` 的 `[mcp_servers.astrolabe]`（与 Codex 同形）；可选 `--with-hooks` 写入 `~/.grok/hooks/astrolabe.json`。
- **Kimi Code：** MCP 合并进 `~/.kimi-code/mcp.json`；可选 `--with-hooks` 在 `~/.kimi-code/config.toml` 追加托管 `[[hooks]]` 块（不覆盖其它 hooks）。
- **ZCode：** 合并进 `~/.zcode/cli/config.json` 的 `mcp.servers.astrolabe`；可选 `--with-hooks` 设置 `hooks.enabled: true` 并写入 `hooks.events.PreToolUse`（**绝不**写 `SessionEnd`；并**清掉**已有的 `Stop` → cleanup，避免每轮清零计数）。

若只用 cargo 安装：`astrolabe setup` 在本机有 Node 时会通过 `npx` 跑同一套流程。


MCP 客户端通过标准输入输出（stdio）启动 Astrolabe。Astrolabe 暴露 7 个工具（见 [参考手册](REFERENCE.zh-CN.md)）。

### 1. Claude Code 配置

#### 一行命令（已用 Claude Code 2.1.251 实测）

```bash
claude mcp add --scope user astrolabe -- astrolabe . --context=claude-code   # 所有项目
claude mcp add --scope project astrolabe -- astrolabe . --context=claude-code # 写入项目 .mcp.json
```

`--` 之后是启动命令及参数；若 `astrolabe` 不在 PATH，请写绝对路径。

#### 方式 A：全局配置（对所有项目生效，推荐）
编辑 `~/.claude.json`，在 `mcpServers` 对象中增加：

**macOS 平台：**
```json
{
  "mcpServers": {
    "astrolabe": {
      "type": "stdio",
      "command": "/usr/local/bin/astrolabe",
      "args": [
        ".",
        "--context=claude-code"
      ]
    }
  }
}
```

**Windows 平台（注意路径反斜杠转义或使用正斜杠）：**
```json
{
  "mcpServers": {
    "astrolabe": {
      "type": "stdio",
      "command": "C:/Tools/astrolabe/astrolabe.exe",
      "args": [
        ".",
        "--context=claude-code"
      ]
    }
  }
}
```

#### 方式 B：单项目配置
在项目根目录下创建 `.mcp.json`：
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

---

### 2. Cursor 配置

打开 Cursor 的设置 -> Features -> MCP Servers，或编辑 `~/.cursor/mcp.json`：

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
*(Windows 请把 command 改为绝对路径 `C:\\Tools\\astrolabe\\astrolabe.exe`)*

#### 免 Cursor 原生 MCP 审批

`mcp.json` 没有 `trusted` 字段。要把下面 7 个只读工具免去 Cursor 原生 MCP 审批，需要写入用户级 `~/.cursor/permissions.json` 的 `mcpAllowlist`（格式 `server:tool`，其中 server 名必须与 mcp.json 的 key 一致，即 `astrolabe`）：

- `search_code`
- `find_symbol`
- `resolve_context`
- `get_diagnostics`
- `find_references`
- `goto_definition`
- `get_languages`

不要写 `astrolabe:*` 或 `*:*`。

定义 `mcpAllowlist` 会**整表覆盖** IDE 里的 MCP allowlist，因此必须合并、不能覆盖已有条目；其它 MCP server 的工具也请写进同一文件。该 allowlist 需要开启 Cursor **Run Mode**（Auto-review / Allowlist / Run Everything）才生效。

推荐在仓库根目录运行安装脚本（会 merge 写入，已存在的条目不会被删除；支持 `--dry-run` 预览、`--home` 覆盖 HOME）：

```bash
node scripts/setup-cursor-permissions.mjs
```

示例（脚本会在文件不存在时创建，已存在时只 union 进 `mcpAllowlist`，不修改 `terminalAllowlist` / `autoRun`）：

```json
{
  "mcpAllowlist": [
    "astrolabe:search_code",
    "astrolabe:find_symbol",
    "astrolabe:resolve_context",
    "astrolabe:get_diagnostics",
    "astrolabe:find_references",
    "astrolabe:goto_definition",
    "astrolabe:get_languages"
  ]
}
```

可选：同时把 Cursor CLI 的 `~/.cursor/cli-config.json` 里 `permissions.allow` 补上缺失的 `Mcp(astrolabe:<tool>)`（不改 `approvalMode` 和其它字段）。若该文件不存在则跳过并提示，不会新建整个 cli-config：

```bash
node scripts/setup-cursor-permissions.mjs --cli
```

---

### 3. Windsurf / VS Code (Cline / Roo-Code / Continue) 配置

在对应插件的 MCP 设置（如 `cline_mcp_settings.json` 或 `mcp.json`）中添加：

```json
{
  "mcpServers": {
    "astrolabe": {
      "command": "astrolabe",
      "args": [
        "."
      ]
    }
  }
}
```

---

### 3b. Codex

`~/.codex/config.toml`（Codex 的 `[mcp_servers.<name>]` 格式；**本机未安装 Codex，此示例按 Codex 文档格式编写，未实机验证**）：

```toml
[mcp_servers.astrolabe]
command = "astrolabe"
args = [".", "--context=codex"]
```

### 4. Codebuddy / Grok / Kimi Code / ZCode 等 CLI 配置与 Hook

一键配置见上文 `astrolabe setup`（含 `--with-hooks`）。手动路径摘要：

| 客户端 | MCP 配置 | Hooks（可选） |
|---|---|---|
| Codebuddy | `~/.codebuddy/.mcp.json`（`mcpServers`） | `~/.codebuddy/settings.json` → `hooks` |
| Grok Build | `~/.grok/config.toml` → `[mcp_servers.astrolabe]` | `~/.grok/hooks/astrolabe.json` |
| Kimi Code | `~/.kimi-code/mcp.json`（`mcpServers`） | `~/.kimi-code/config.toml` → `[[hooks]]` |
| ZCode | `~/.zcode/cli/config.json` → `mcp.servers` | 同文件 `hooks.enabled` + `hooks.events`（无 SessionEnd） |

- **MCP 服务接入**：作为标准 stdio MCP 启动，无需指定特殊 context 参数（默认回退 `default` 温和级）。JSON `mcpServers` 示例：
  ```json
  {
    "mcpServers": {
      "astrolabe": {
        "command": "astrolabe",
        "args": ["."]
      }
    }
  }
  ```
- **防漂移 Hook 接入（可选，效果未做基准，见 [HOOKS.md](HOOKS.md)）**：`--client=codebuddy` / `grok`（别名 `grokbuild`、`grok-build`）/ `kimicode`（别名 `kimi` 等）/ `zcode`（别名 `zai` 等）。

---

## 四、直接给 AI 工具执行的安装提示词模板

> 请帮我安装并配置 Astrolabe（本地代码检索 MCP 服务）：
> 1. 运行 `curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh`，确认输出 `sha256 OK`；
> 2. Claude Code：运行 `claude mcp add --scope user astrolabe -- <astrolabe 的绝对路径> . --context=claude-code`；
> 3. 若当前是 Cursor：在仓库根目录运行 `node scripts/setup-cursor-permissions.mjs`，并确认 Run Mode 已开启，不要写 `astrolabe:*`；
> 4. 执行 `astrolabe --version`，并重启客户端会话让 MCP 生效。

---

## 五、高级配置与环境变量

| 环境变量 | 默认值 | 说明 |
|---|---|---|
| `ASTROLABE_CACHE_MB` | `256` | 内存文件缓存上限（MB），设为 `0` 关闭内存缓存 |
| `ASTROLABE_WATCH_SECS` | 未设置 | 后台文件监听模式：未设置使用原生事件（空闲 0% CPU）；设为 `0` 关闭监听；正整数 `N` 强制 `N` 秒轮询兜底 |
| `ASTROLABE_LSP_TIMEOUT` | `300` | LSP 语言服务器单次请求最大超时时间（秒） |
| `ASTROLABE_CONTEXT` | `default` | 客户端上下文预设（`claude-code`, `cursor`, `codex`, `readonly` 等；codebuddy 与 grok 走 `default` 温和级） |
| `ASTROLABE_SLICE_READ_MAX` | `200` | 切片精读最大行数：带 `limit ≤ N` 的 Read 调用视为切片读 |
| `ASTROLABE_SLICE_READ_THRESHOLD` | `10` | 连续切片读提醒阈值：连续 N 次切片读且期间无任何 Astrolabe 工具调用时触发 deny（任何 Astrolabe 工具调用清零该额度） |
| `ASTROLABE_READ_THRESHOLD` | `3` | 连续全文件 Read 调用 deny 阈值（达到该次数触发拦截） |
| `ASTROLABE_GREP_THRESHOLD` | `3` | 连续 Grep/shell 文本搜索 deny 阈值（soft 模式） |
| `ASTROLABE_DENY_TEXT_SEARCH` | 关闭 | 硬禁用宿主 Grep/Glob/shell 文本搜索（`true`/`1`/`yes`/`on`）；首次即 deny。多 Astrolabe 进程可在各自 mcp.json `env` 中分别配置；hooks 命令需同步设置或加 `--deny-text-search` / `--context=` |
| `ASTROLABE_DENY_SILENCE_SECS` | `15` | deny 后静默窗口秒数（窗口内 hook 放行，不累计计数） |

### 语言服务器（可选）

符号索引、`find_symbol`、文件图谱只靠内置 tree-sitter，不需要任何语言服务器。`find_references` / `goto_definition` / `get_diagnostics` / 改名需要对应的语言服务器在 `PATH` 上；找不到时这些工具返回 `unknown` 并给出安装提示，不会静默降级。也可用 `ASTROLABE_LSP_<LANG>` 指定可执行文件路径。Astrolabe 不会自动下载。

| 语言 | 扩展名 | 语言服务器（优先 → 回退） | 安装 | 覆盖变量 |
|---|---|---|---|---|
| Swift | `.swift` | `sourcekit-lsp` | macOS：`xcode-select --install`；Linux / Windows：安装 [Swift 工具链](https://www.swift.org/install/) | `ASTROLABE_LSP_SWIFT` |
| Objective-C / ObjC++ | `.m`, `.mm`, ObjC `.h` | `clangd` → `sourcekit-lsp` | `brew install llvm`（Linux：`apt-get install clangd`）；或 `xcode-select --install` | `ASTROLABE_LSP_OBJC`, `ASTROLABE_LSP_OBJCPP` |
| Vue | `.vue` | `vue-language-server --stdio` | `npm i -g @vue/language-server` | `ASTROLABE_LSP_VUE` |
| Kotlin | `.kt`, `.kts` | `kotlin-lsp` → `kotlin-language-server` | `brew install JetBrains/utils/kotlin-lsp`（预览版）或 [Releases](https://github.com/Kotlin/kotlin-lsp/releases)；回退 `brew install kotlin-language-server` | `ASTROLABE_LSP_KOTLIN` |
| Shell / Bash | `.sh`, `.bash`, sh/bash shebang 脚本 | `bash-language-server start` | `npm i -g bash-language-server`（可选 `shellcheck`） | `ASTROLABE_LSP_SHELL` |
| Ruby | `.rb`, `.rake`, `.gemspec`, `Rakefile` / `Gemfile` 等 | `ruby-lsp` → `solargraph stdio` | `gem install ruby-lsp`（或 `gem install solargraph`） | `ASTROLABE_LSP_RUBY` |
| Lua | `.lua`, lua shebang 脚本 | `lua-language-server` | `brew install lua-language-server` 或 [Releases](https://github.com/LuaLS/lua-language-server/releases) | `ASTROLABE_LSP_LUA` |

**Kotlin（kotlin-lsp）补充说明**

- 启动器：新版独立包用 `bin/intellij-server` 启动（旧的 `kotlin-lsp.sh` 已弃用）。Astrolabe 会在 PATH、Homebrew `libexec`、`/opt`、`~/.local/share`、`%LOCALAPPDATA%\Programs`、VS Code / Cursor 扩展目录里查找；`ASTROLABE_LSP_KOTLIN` 可以直接指向解压目录。
- 缓存：每个仓库用独立的 `--system-path`（默认 `~/.astrolabe/kotlin-lsp/<仓库名>-<哈希>`，可用 `ASTROLABE_KOTLIN_SYSTEM_PATH` 覆盖）。首次 Gradle 导入成功后，Astrolabe 会导出 `workspace.json`；之后重启时只要构建文件（`settings.gradle*`、`build.gradle*`、`gradle.properties`、`libs.versions.toml`、`pom.xml`）没变，就用 `json` 导入器在几秒内恢复项目模型，不再重跑 Gradle 同步。
- `ASTROLABE_KOTLIN_BUILD_TOOL`：强制指定导入方式（`gradle` / `maven` / `json` / `*`；空字符串表示不导入）。
- 导入期间和 kotlin-lsp 返回空结果时（例如 Kotlin 多平台 / Android 源集没有被导入），`goto_definition` / `find_references` / 悬停会给出 tree-sitter 语法索引的结果，标为 `Syntactic` 并附说明，不会把空结果标成 `Exact`。


其余语言（Python、Go、Rust、Java、TS/JS、C/C++、PHP、Dart、C#/VB）见 [参考手册](REFERENCE.zh-CN.md) 的语言矩阵。

Hook 的读源码拦截按 41 个受支持扩展名判断（含上表的 `.kt` / `.kts` / `.sh` / `.bash` / `.rb` / `.rake` / `.gemspec` / `.lua` / `.swift` / `.m` / `.mm` / `.vue`）；不在表里的语言正常读取，不拦截。

### 客户端与 Hook 支持矩阵

| 客户端 (`--client`) | 工具识别规则 | Deny 决策格式 | 上下文预设 (`--context`) | 状态与说明 |
|---|---|---|---|---|
| `claude-code` | CC 规范 `grep` / `read` / shell 命令 | 结构化 `hookSpecificOutput` | `claude-code` | 官方支持 |
| `codebuddy` | CC 同构工具名识别（`grep`、`read`、`read_file` 等） | 结构化 `hookSpecificOutput` | `default`（温和级） | 内置支持（适用于支持 CC 兼容 hooks 的宿主） |
| `grok` | `grep` / `read_file` / shell 命令 | 扁平 `{"decision":"deny","reason":...}` | `default`（温和级） | 内置支持（别名：`grokbuild`、`grok-build`） |
| `cursor` | 原生 `Shell`/`Read`/`Grep`/`Task` 及 shell 命令 | 原生扁平 `{"permission":"deny",...}` | `default`（MCP 可选 `cursor`） | 内置支持（Cursor CLI hooks.json，别名 cursor-agent/cursor_agent/cursor-cli；兼容 conversation_id） |
| `kimicode` | CC 同构识别（`Read` / `Grep` / `Bash` 及 shell 命令） | 结构化 `hookSpecificOutput` | `default`（温和级） | 内置支持（别名：kimi-code/kimi_code/kimi） |
| `zcode` | CC 同构识别（`Read` / `Grep` / `Bash` 及 shell 命令） | 结构化 `hookSpecificOutput` | `default`（温和级） | 内置支持（别名：z-code/zai） |
| `opencode` | 宿主传入 tool 名称及 shell 命令 | 扁平 `{"decision":"deny","reason":...}` | `default`（温和级） | 插件桥接（.opencode/plugins/astrolabe-guard.ts TS 插件拦截） |
| `codex` | shell grep/read 命令 | 结构化 `permissionDecision` | `codex` | 官方支持 |
| `vscode` / `other` | 通用子串包含匹配 | 结构化 `hookSpecificOutput` | `default` | 通用回退 |
