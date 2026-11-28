# Astrolabe AI 安装与客户端配置指南（中文）

英文版见 [INSTALL_GUIDE.md](INSTALL_GUIDE.md)。最后更新 2026-11-01（v0.17.5+：Windows 稳定性 #117/#119、MCP 短名、安装后必须 `setup`）。

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

```powershell
# 推荐 npm（自动拉对应平台包）：
npm i -g @zlnap/astrolabe
# 然后必须跑 setup（写入 MCP 配置）：
npx -y @zlnap/astrolabe setup
# 或：astrolabe setup
```

也可从 [Releases](https://github.com/wojiushirencai/Astrolabe/releases) 下载 `astrolabe-<版本>-x86_64-pc-windows-msvc.zip`，用 `Get-FileHash -Algorithm SHA256` 对照 `SHA256SUMS`，解压 `astrolabe.exe` 到固定目录并加入 `PATH`，然后同样执行 `astrolabe setup`。

**Windows 稳定性（真机验证，v0.17.4–v0.17.5）：** #117 统一 `\?\` 与普通 `C:\`（watcher 归属 / 比较），状态文案去掉 `\?\`；子目录 `index_root` 复用已 Ready 的父索引并带 path_filter（不再另开冷索引）。#119 把 Windows 的 `path_filter`（反斜杠 / 绝对路径）归一成仓库相对 `/` 路径。尽量不传 `index_root`，或只传**仓库根**。

### 方式 4：npm（只安装不够）

npm 包名为 **`@zlnap/astrolabe`**（二进制名仍是 `astrolabe`），平台包为 `@zlnap/<platform>`，已在 npm 个人 scope `zlnap` **发布**（无需建 org；npm org `astrolabe-ai` 已被他人占用）。注意：npm 上未加 scope 的 `astrolabe` 和 `astrolabe-ai` 是与本项目无关的他人的包，请不要用它们。

```bash
npm i -g @zlnap/astrolabe          # 或：npx -y @zlnap/astrolabe --version
# 每次新装 / 升级后都必须：
npx -y @zlnap/astrolabe setup
```

**为什么一定要 `setup`：** `npm i` 只放下二进制 shim。不跑 `astrolabe setup`，就不会写入任何客户端的 MCP 配置。从旧版升级时，重新运行 `astrolabe setup`（或 `astrolabe setup --remove-hooks`）会自动清理遗留的 Astrolabe hooks，并保留你的其它第三方 hooks。`setup` 会探测 Claude Code、Cursor、Codex、Windsurf、Cline、Roo、Continue、OpenCode、DeepSeek Harness、Codebuddy、Grok Build、Kimi Code、ZCode，并把 Astrolabe **合并**进各自配置（不会清空其它 server）。

### 方式 5：从源码编译（Rust 1.90+）

```bash
git clone https://github.com/wojiushirencai/Astrolabe.git && cd Astrolabe
cargo build --release --bin astrolabe     # 产物 target/release/astrolabe
```

验证：`astrolabe --version`，然后执行 `npx -y @zlnap/astrolabe setup`（或 `astrolabe setup`），再重启客户端以加载 MCP。

---

## 三、各 AI 编程工具 MCP 配置配方

### 一键配置（安装后必做）

装好二进制或 npm 包后（或直接用 `npx`）运行：

```bash
npx -y @zlnap/astrolabe setup
# 非交互：
npx -y @zlnap/astrolabe setup --yes
```

**只安装不跑 `setup`，等于客户端根本不会调用 Astrolabe。** 防漂移 Hook 已移除，现通过内置意图路由提示词引导调用；旧版升级后运行 `setup`（或 `astrolabe setup --remove-hooks`）会自动清理遗留的 Astrolabe hooks，且不触碰第三方 hooks（详见 [HOOKS.md](HOOKS.md)）。

会探测本机的 Claude Code、Cursor、Codex、Windsurf、Cline、Roo Code、Continue、**OpenCode**、**DeepSeek Harness（DSH）**、**Codebuddy**、**Grok Build**、**Kimi Code**、**ZCode**，交互多选要写入的客户端，并把 Astrolabe **合并**进各自配置（不会清空其它 server）。默认启动命令为 `npx -y @zlnap/astrolabe`，有 `--context=` 的客户端会带上（上述四者走 `default` 温和级，不写 `--context`）。常用参数：`--yes`、`--clients=cursor,claude-code,opencode,dsh,codebuddy,grok,kimicode,zcode`、`--dry-run`、`--native`、`--list`、`--check-config`（只读检查 Cursor 实际生效的配置和审批，0.17.10 及更早版本没有这个参数）、`--remove-hooks`（仅清理遗留的 Astrolabe hooks，保留其它第三方 hooks）、`--no-cursor-permissions`、`--scope=global|project`（Cursor，默认 global）、`--project <dir>`（Cursor 项目根，默认取 cwd 的 git 根）。

**MCP 工具短名**（`tools/list` 声明）：`resolve_contx`、`search_code`、`find_symbol`、`find_refs`、`goto_def`、`get_diag`、`get_lang`。旧长名在 `tools/call` 上仍是别名。

- **Cursor（默认 `--scope=global`）：** 写 `~/.cursor/mcp.json`，command 用原生二进制绝对路径，`type: stdio`，`env` 带 `PATH`、`ASTROLABE_LSP_TIMEOUT=300`，探测到 `pyright-langserver` 时写 `ASTROLABE_LSP_PYTHON`。同时在当前项目目录尽力执行一次 `cursor-agent mcp enable astrolabe`，并把 `astrolabe` 从 `mcp-disabled.json` 移除。Cursor 没有基于文件的全局 rules 目录，L1 引导来自 MCP instructions。保留其它 `mcpServers`，可重复执行。
- **Cursor 项目级（`--scope=project`）：** 对 git 根 / `--project` 写三件套：`<project>/.cursor/mcp.json`（绝对路径、`cwd` 固定项目根、同样的 `env`）、`<project>/.cursor/rules/astrolabe.mdc`（`alwaysApply: true`，正文与 MCP L1 instructions 同源），并对该目录执行 `cursor-agent mcp enable astrolabe` 完成审批。
- **OpenCode：** 合并进 `~/.config/opencode/opencode.jsonc`（或已有的 `.json` / 旧版 `config.json`，或 `~/.opencode/…`）。自动识别 V1（`mcp.<name>` + `enabled`）、V2（`mcp.servers`）或类 Claude 的 `mcpServers`；在父对象已存在时尽量保留 JSONC 注释。
- **DeepSeek Harness：** DSH 的 MCP 是按仓库在 UI 里配的，setup 不会代写，并会从 `~/.dsh/cordis.patch.yml` 清理遗留的 `astrolabe-dsh-guard` 块（不碰 `orca-managed-dsh-hooks`）。
- **Codebuddy：** MCP 写入 `~/.codebuddy/.mcp.json`（若已有 `mcp.json` / `~/.codebuddy.json` 则合并进优先已存在的那个）。
- **Grok Build：** MCP 合并进 `~/.grok/config.toml` 的 `[mcp_servers.astrolabe]`（与 Codex 同形）。
- **Kimi Code：** MCP 合并进 `~/.kimi-code/mcp.json`。
- **ZCode：** 合并进 `~/.zcode/cli/config.json` 的 `mcp.servers.astrolabe`。

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

`astrolabe setup` 默认写全局 `~/.cursor/mcp.json`；加 `--scope=project`（可配 `--project <dir>`）则写项目级 `.cursor/mcp.json` 与 `.cursor/rules/astrolabe.mdc`。两种方式都使用原生二进制绝对路径并保留其它 server：

```json
{
  "mcpServers": {
    "astrolabe": {
      "type": "stdio",
      "command": "/绝对路径/node_modules/@zlnap/darwin-arm64/astrolabe",
      "args": [".", "--context=cursor"],
      "cwd": "/绝对路径/项目根",
      "env": {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "ASTROLABE_LSP_TIMEOUT": "300",
        "ASTROLABE_LSP_PYTHON": "/绝对路径/pyright-langserver"
      }
    }
  }
}
```
`cwd` 与 `.cursor/rules` 仅项目级写入。Cursor CLI 的审批按项目记录在 `~/.cursor/projects/<slug>/mcp-approvals.json`，全局配置无法预批准未来项目：默认 setup 会对当前项目尽力执行一次 `cursor-agent mcp enable astrolabe`，并在输出中提示新项目首次使用会弹审批（无头场景用 `--approve-mcps`，或在目标项目手动执行该命令）。`--scope=project` 会对目标目录执行并校验审批文件。修改 server 条目会改变配置哈希、需要重新审批，因此改完配置要重跑 setup。

**先看实际生效的是哪份。** 包含本次修复的版本提供下面的只读检查（0.17.10 及更早版本还没有这个参数）：

```bash
astrolabe setup --check-config --project "/path/to/my-project"
```

它只读取配置，列出实际使用的文件、Astrolabe 命令和参数、是否覆盖全局注册，以及当前配置的审批／禁用状态；不安装程序、不下载、不改文件，也不代替你批准 MCP。不会显示配置里的 `env`。这里只检查标准的全局和项目文件，企业托管或会话覆盖仍需在客户端里确认。`current-config-not-approved` 是“没找到当前配置对应的审批”，不是“已经证明这个会话不能用”。检查后再决定更新哪一份，不要只看终端里的版本号。

setup 会更新它识别到的启动路径与工具免审批名单（短名 allowlist），清理遗留的 Astrolabe hooks（含第三方命令的混合分组不动），并尝试审批当前项目、清理 Astrolabe 的禁用条目。

Cursor 没有基于文件的全局 rules 目录（User Rules 在 UI 中维护）。全局链路的 L1 引导由 MCP `instructions` 提供。

#### 免 Cursor 原生 MCP 审批

`mcp.json` 没有 `trusted` 字段。要把下面 7 个只读工具免去 Cursor 原生 MCP 审批，需要写入用户级 `~/.cursor/permissions.json` 的 `mcpAllowlist`（格式 `server:tool`，其中 server 名必须与 mcp.json 的 key 一致，即 `astrolabe`）：

- `search_code`
- `find_symbol`
- `resolve_contx`
- `get_diag`
- `find_refs`
- `goto_def`
- `get_lang`

allowlist **只写短名**。旧长名（`resolve_context` / `get_diagnostics` / `find_references` / `goto_definition` / `get_languages` 等）留在服务端 `tools/call` 别名里；`astrolabe setup` / `setup-cursor-permissions.mjs` 重跑时会检查并把 allowlist 里残留的旧长名改成短名。

不要写 `astrolabe:*` 或 `*:*`。

定义 `mcpAllowlist` 会**整表覆盖** IDE 里的 MCP allowlist，因此必须合并、不能覆盖已有条目；其它 MCP server 的工具也请写进同一文件。该 allowlist 需要开启 Cursor **Run Mode**（Auto-review / Allowlist / Run Everything）才生效。

推荐在仓库根目录运行安装脚本（会 merge 短名、迁移旧长名；非 Astrolabe 条目保留；支持 `--dry-run` 预览、`--home` 覆盖 HOME）：

```bash
node scripts/setup-cursor-permissions.mjs
```

示例（脚本会在文件不存在时创建；已存在时写入短名并迁移旧长名，不修改 `terminalAllowlist` / `autoRun`）：

```json
{
  "mcpAllowlist": [
    "astrolabe:search_code",
    "astrolabe:find_symbol",
    "astrolabe:resolve_contx",
    "astrolabe:get_diag",
    "astrolabe:find_refs",
    "astrolabe:goto_def",
    "astrolabe:get_lang"
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

### 4. Codebuddy / Grok / Kimi Code / ZCode 等 CLI 配置

一键配置见上文 `astrolabe setup`。手动路径摘要：

| 客户端 | MCP 配置 |
|---|---|
| Codebuddy | `~/.codebuddy/.mcp.json`（`mcpServers`） |
| Grok Build | `~/.grok/config.toml` → `[mcp_servers.astrolabe]` |
| Kimi Code | `~/.kimi-code/mcp.json`（`mcpServers`） |
| ZCode | `~/.zcode/cli/config.json` → `mcp.servers` |

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
- **旧版 Hook 升级清理**：从旧版升级时运行 `astrolabe setup --remove-hooks`（或 `astrolabe setup`）即可清理历史遗留的 Astrolabe hooks 并保留第三方 hooks，详见 [HOOKS.md](HOOKS.md)。

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

> **旧版 Hook 升级清理**：防漂移 Hook 及 `ASTROLABE_SLICE_READ_*` / `ASTROLABE_READ_THRESHOLD` / `ASTROLABE_GREP_THRESHOLD` / `ASTROLABE_DENY_*` / `ASTROLABE_HOOK_*` 环境变量已移除；从旧版升级时运行 `astrolabe setup`（或 `astrolabe setup --remove-hooks`）即可清理遗留 hooks，详见 [HOOKS.md](HOOKS.md)。

### 语言服务器（可选）

符号索引、`find_symbol`、文件图谱只靠内置 tree-sitter，不需要任何语言服务器。`find_refs` / `goto_def` / `get_diag`（旧长名仍可用）/ 改名需要对应的语言服务器在 `PATH` 上；找不到时这些工具返回 `unknown` 并给出安装提示，不会静默降级。也可用 `ASTROLABE_LSP_<LANG>` 指定可执行文件路径。Astrolabe 不会自动下载。

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
- 导入期间和 kotlin-lsp 返回空结果时（例如 Kotlin 多平台 / Android 源集没有被导入），`goto_def` / `find_refs` / 悬停会给出 tree-sitter 语法索引的结果，标为 `Syntactic` 并附说明，不会把空结果标成 `Exact`。


其余语言（Python、Go、Rust、Java、TS/JS、C/C++、PHP、Dart、C#/VB）见 [参考手册](REFERENCE.zh-CN.md) 的语言矩阵。
