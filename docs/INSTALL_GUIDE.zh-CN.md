# Astrolabe AI 安装与客户端配置指南（中文）

英文版见 [INSTALL_GUIDE.md](INSTALL_GUIDE.md)。最后更新 2026-11-16。

第一次使用请从[安装](#二安装)开始；已经装过的用户请看[升级旧版本](#upgrade)。

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
# 然后必须跑 setup（加 --with-hooks，否则智能体往往不会真正调用工具）：
npx -y @zlnap/astrolabe setup --with-hooks
# 或：astrolabe setup --with-hooks
```

也可从 [Releases](https://github.com/wojiushirencai/Astrolabe/releases) 下载 `astrolabe-<版本>-x86_64-pc-windows-msvc.zip`，用 `Get-FileHash -Algorithm SHA256` 对照 `SHA256SUMS`，解压 `astrolabe.exe` 到固定目录并加入 `PATH`，然后同样执行 `astrolabe setup --with-hooks`。

**Windows 稳定性（真机验证，v0.17.4–v0.17.5）：** #117 统一 `\?\` 与普通 `C:\`（watcher 归属 / 比较），状态文案去掉 `\?\`；子目录 `index_root` 复用已 Ready 的父索引并带 path_filter（不再另开冷索引）。#119 把 Windows 的 `path_filter`（反斜杠 / 绝对路径）归一成仓库相对 `/` 路径。尽量不传 `index_root`，或只传**仓库根**。

### 方式 4：npm（只安装不够）

npm 包名为 **`@zlnap/astrolabe`**（二进制名仍是 `astrolabe`），平台包为 `@zlnap/<platform>`，已在 npm 个人 scope `zlnap` **发布**（无需建 org；npm org `astrolabe-ai` 已被他人占用）。注意：npm 上未加 scope 的 `astrolabe` 和 `astrolabe-ai` 是与本项目无关的他人的包，请不要用它们。

```bash
npm i -g @zlnap/astrolabe          # 或：npx -y @zlnap/astrolabe --version
# 首次安装后，运行 setup 接入客户端：
npx -y @zlnap/astrolabe setup --with-hooks
```

**为什么一定要 `setup`：** `npm i` 只放下二进制 shim。不跑 `astrolabe setup`，就不会写入任何客户端的 MCP 配置。不加 hooks（在支持的客户端上用 `--with-hooks`），很多智能体仍会用 Grep/Read，等于没用上 Astrolabe。`setup` 会探测 Claude Code、Cursor、Codex、Windsurf、Cline、Roo、Continue、OpenCode、DeepSeek Harness、Codebuddy、Grok Build、Kimi Code、ZCode，并把 Astrolabe **合并**进各自配置（不会清空其它 server）。

### 方式 5：从源码编译（Rust 1.90+）

```bash
git clone https://github.com/wojiushirencai/Astrolabe.git && cd Astrolabe
cargo build --release --bin astrolabe     # 产物 target/release/astrolabe
```

验证：`astrolabe --version`，然后执行 `npx -y @zlnap/astrolabe setup --with-hooks`（或 `astrolabe setup --with-hooks`），再重启客户端以加载 MCP + hooks。

---

<a id="upgrade"></a>

## 升级旧版本

**已经装过的用户从这里开始。** 升级不只是换一个可执行文件，还要让客户端用上它。重跑 `setup` 是为了同步配置，不代表每个补丁版都要手改 JSON。

### 1. 先确认原来怎么装、配置写在哪里

先备份准备改的 MCP 和 Hook 配置。尤其是自己改过启动命令、加过硬拦截参数的用户，先记下这些设置。`setup` 写入时也会备份已有文件，但不会替你判断哪一份旧配置还在使用。

Cursor 用户先看第 3 步的两种配置位置；不要为了升级就从项目里的配置改成全局配置。其它客户端按原来的安装方式更新即可。

### 2. 更新程序，检查版本

用 npm 安装的用户运行：

```bash
npm i -g @zlnap/astrolabe@latest
astrolabe --version
```

如果版本还是旧的，先检查终端的 `PATH` 是否指向另一份 Astrolabe。用安装脚本、手动下载或源码安装的用户，按本文对应方法更新原来的那份程序，不必为了升级再装一份 npm 版。下面的 `astrolabe setup` 也可用 `npx -y @zlnap/astrolabe@latest setup` 代替；要继续使用自己安装的二进制，可给 setup 加 `--command="/程序的绝对路径/astrolabe"`（Windows 用实际的 `.exe` 路径）。

### 3. 按客户端更新配置

**Cursor：先选对配置位置，两条命令选一条。**

- **全局配置**是 `~/.cursor/mcp.json`，供多个项目使用，也是 setup 的默认位置。在要使用 Astrolabe 的项目目录运行：

  ```bash
  astrolabe setup --clients=cursor --with-hooks
  ```

- **项目里的配置**是 `<项目>/.cursor/mcp.json`。如果以前把 Astrolabe 配在这里，就更新这里；把示例路径换成自己的项目根目录：

  ```bash
  astrolabe setup --clients=cursor --with-hooks --scope=project --project "/path/to/my-project"
  ```

同名的项目配置会覆盖全局配置。只更新全局文件，可能仍然启动项目里写的旧程序。setup 不会自动删除另一处的旧注册；只有你决定不再使用那份配置时，才在备份后移除其中的 Astrolabe 条目，不要删整个文件或其它 server。

setup 会更新它识别到的启动路径、工具免审批名单（短名 allowlist）和 Hook 命令，也会给 Cursor 的匹配规则补上 `astrolabe-.*`，并尝试审批当前项目、清理 Astrolabe 的禁用条目。审批认的是项目路径和当前启动配置（配置哈希）；修改启动命令或环境变量后可能要重新审批，全局配置也不能替所有项目预先批准。若 setup 提示自动审批失败，请在**实际要用的项目目录**运行：

```bash
cursor-agent mcp enable astrolabe
```

即使选择项目 MCP 配置，setup 写的 Cursor Hook 仍是全局 `~/.cursor/hooks.json`。项目里的 `.cursor/hooks.json`，以及 Cursor 也可能加载的 `.claude/settings.json`，不会因此一起迁移。若以前手工装过 Hook，请检查是否重复，只整理已确认不用的 Astrolabe 条目，保留其它工具的 Hook。

**Claude Code：setup 配 MCP，Hook 另行合并。**

```bash
astrolabe setup --clients=claude-code
```

当前即使加上 `--with-hooks`，Claude Code 的 setup 也只注册 MCP，不会安装 Hook。需要 Hook 时，按 [Claude Code Hook 配方](HOOKS.md#2-claude-code-配方)合并到原来的 settings 文件，保留第三方 Hook。原来手写在项目 `.mcp.json` 里的 MCP 条目，也不会被这条命令自动更新或清理，请另行核对。

**其它客户端：**运行 `astrolabe setup --with-hooks`，只选择自己使用的客户端；只想更新其中一个时，加 `--clients=客户端ID`，ID 见[下方配置说明](#三各-ai-编程工具-mcp-配置配方)。`--with-hooks` 只为已支持的客户端安装 Hook 或插件，不是所有客户端都支持。想先看改动，可在所选 setup 命令末尾加 `--dry-run`。

**自己改过 Hook 的用户，再核对一次。** setup 可能替换它识别到的 Astrolabe Hook 命令，自定义的 `--deny-text-search`、上下文路径或执行模式不保证保留。Cursor 默认 `auto` 在 MCP 未获批准、未启动或无法可靠探测时会放行并提醒；`advisory` 只提醒，`enforce` 才是不依赖 MCP 可用性的强制拦截。需要保留自定义行为时，按 [Hook 模式说明](HOOKS.md#1-cursor-cli)核对 **Hook 进程**的设置，不要只改 MCP 的环境变量。

### 4. 让客户端重新加载，再实际调用一次

更新磁盘上的程序不会替换已经运行的 MCP 进程。完成配置和审批后，按客户端的方式重连 MCP 或重启客户端，再开一个新会话，让它重新读取工具列表和 Hook。不要在别人的任务运行中强行结束进程。

先看 `astrolabe --version`，再在客户端里让 AI **实际调用 Astrolabe 的 `get_lang` 或 `search_code`**，确认能查到当前项目。如果仍看不到工具，先检查项目配置是否覆盖了全局配置，以及当前项目是否已批准；终端显示新版本不代表会话里也在运行新版本。

### 5. 不用例行删缓存

从 0.17.7／0.17.8／0.17.9 升到 0.17.10，不需要手动删除索引或重新建库。这几个版本的存储格式版本未变；需要时 Astrolabe 会自动重建不兼容的缓存。没有明确故障时，不要把清空缓存当成升级步骤。

---

## 三、各 AI 编程工具 MCP 配置配方

### 一键配置（安装后必做）

装好二进制或 npm 包后（或直接用 `npx`）运行：

```bash
npx -y @zlnap/astrolabe setup --with-hooks
# 非交互：
npx -y @zlnap/astrolabe setup --yes --with-hooks
```

**首次安装后要跑 `setup`，客户端才会得到 MCP 配置。** `--with-hooks` 会为支持的客户端安装防漂移 Hook 或插件；Claude Code 的 Hook 需要[另行手工合并](HOOKS.md#2-claude-code-配方)。已经装过的用户先看[升级步骤](#upgrade)，避免改错配置位置。

会探测本机的 Claude Code、Cursor、Codex、Windsurf、Cline、Roo Code、Continue、**OpenCode**、**DeepSeek Harness（DSH）**、**Codebuddy**、**Grok Build**、**Kimi Code**、**ZCode**，交互多选要写入的客户端，并把 Astrolabe **合并**进各自配置（不会清空其它 server）。默认启动命令为 `npx -y @zlnap/astrolabe`，有 `--context=` 的客户端会带上（上述四者走 `default` 温和级，不写 `--context`）。常用参数：`--yes`、`--clients=cursor,claude-code,opencode,dsh,codebuddy,grok,kimicode,zcode`、`--dry-run`、`--native`、`--list`、`--with-hooks`（安装 OpenCode 守卫插件，以及 Cursor / Codebuddy / Grok / Kimi / ZCode 的 hooks 配方）、`--no-cursor-permissions`、`--scope=global|project`（Cursor，默认 global）、`--project <dir>`（Cursor 项目根，默认取 cwd 的 git 根）。

**MCP 工具短名**（`tools/list` 声明）：`resolve_contx`、`search_code`、`find_symbol`、`find_refs`、`goto_def`、`get_diag`、`get_lang`。旧长名在 `tools/call` 上仍是别名。

- **Cursor（默认 `--scope=global`）：** 写 `~/.cursor/mcp.json`，command 用原生二进制绝对路径，`type: stdio`，`env` 带 `PATH`、`ASTROLABE_LSP_TIMEOUT=300`，探测到 `pyright-langserver` 时写 `ASTROLABE_LSP_PYTHON`。同时在当前项目目录尽力执行一次 `cursor-agent mcp enable astrolabe`，并把 `astrolabe` 从 `mcp-disabled.json` 移除。Cursor 没有基于文件的全局 rules 目录，L1 引导来自 MCP instructions 与全局 `~/.cursor/hooks.json` remind hook（用 `--with-hooks` 注册，命令为绝对路径，matcher `Shell|Read|Grep|astrolabe-.*`）。保留其它 `mcpServers`，可重复执行。
- **Cursor 项目级（`--scope=project`）：** 对 git 根 / `--project` 写三件套：`<project>/.cursor/mcp.json`（绝对路径、`cwd` 固定项目根、同样的 `env`）、`<project>/.cursor/rules/astrolabe.mdc`（`alwaysApply: true`，正文与 MCP L1 instructions 同源），并对该目录执行 `cursor-agent mcp enable astrolabe` 完成审批。
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

Cursor 没有基于文件的全局 rules 目录（User Rules 在 UI 中维护）。全局链路的 L1 引导由 MCP `instructions` 和全局 `~/.cursor/hooks.json` remind hook 共同提供；`--with-hooks` 会用绝对路径和 Cursor 原生 `astrolabe-.*` matcher 注册该 hook。

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

> 请帮我安装或升级 Astrolabe：
> 1. 先确认我用哪个客户端、是否装过 Astrolabe；已安装的按本文“升级旧版本”操作，不要另装一份或擅自更换全局／项目配置。修改前备份相关文件。
> 2. 首次用 npm 安装时，运行 `npm i -g @zlnap/astrolabe@latest`，再检查 `astrolabe --version`；如果我选择其它安装方式，就按本文对应方法安装。
> 3. **必须运行 setup 配置 MCP。** 用 `astrolabe setup --with-hooks` 只选择我使用的客户端；Cursor 先确认原配置位置，项目里的配置要用上面的 `--scope=project --project` 命令。不要覆盖整个配置文件，也不要删除其它 server 或 Hook。
> 4. Claude Code 的 setup 只注册 MCP；需要 Hook 时，按 [Claude Code 配方](HOOKS.md#2-claude-code-配方)另行合并。其它客户端也要检查 setup 是否支持其 Hook；保留并核对原有自定义参数。Cursor 自动审批失败时，在目标项目里运行 `cursor-agent mcp enable astrolabe`。
> 5. 提醒我重连 MCP／重启客户端并开新会话，不要中断其它正在运行的任务。最后实际调用 Astrolabe 的 `get_lang` 或 `search_code` 验证当前项目可用，不要只看版本号。

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
