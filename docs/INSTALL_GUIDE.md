# Astrolabe 多平台安装与 AI 工具配置指南

本指南专为开发者及 AI 编程工具（Claude Code、Cursor、Windsurf、Codex、OpenCode 等）自动化配置而设计。您可以直接把本文档或配置 JSON 喂给 AI 工具完成一键部署。

---

## 一、二进制产物清单

Astrolabe 已完成本地 Native Release 构建，产物位于仓库 `dist/release-binaries/` 及 `target/` 目录：

| 平台 / 架构 | 二进制文件路径 | 打包归档文件 | 适用系统 |
|---|---|---|---|
| **macOS (Apple Silicon)** | `dist/release-binaries/astrolabe-macos-arm64` | `astrolabe-macos-arm64.tar.gz` | M1 / M2 / M3 / M4 Mac |
| **macOS (Intel x86_64)** | `dist/release-binaries/astrolabe-macos-x86_64` | `astrolabe-macos-x86_64.tar.gz` | Intel 处理器 Mac |
| **macOS (通用二进制 Universal)** | `dist/release-binaries/astrolabe-macos-universal` | `astrolabe-macos-universal.tar.gz` | 所有 Mac 系统（同时含 arm64 + x86_64） |
| **Windows (x64)** | `dist/release-binaries/astrolabe-windows-x64.exe` | `astrolabe-windows-x64.zip` | Windows 10 / 11 / Server 64位 |

> 注：校验和文件位于 `dist/release-binaries/SHA256SUMS`。

---

## 二、安装与放置步骤

### 1. macOS 安装（推荐放入系统 PATH）

在 macOS 终端中执行以下命令（以 Apple Silicon 为例，Intel 用户请替换为对应文件名）：

```bash
# 创建本地 bin 目录（如果不存在）
mkdir -p ~/.cargo/bin /usr/local/bin

# 复制二进制（macOS 建议赋予执行权限并清除 Gatekeeper 隔离属性）
cp dist/release-binaries/astrolabe-macos-universal ~/.cargo/bin/astrolabe
chmod +x ~/.cargo/bin/astrolabe
xattr -d com.apple.quarantine ~/.cargo/bin/astrolabe 2>/dev/null || true

# 建立全局软链接（方便全局直接执行 astrolabe）
ln -sf ~/.cargo/bin/astrolabe /usr/local/bin/astrolabe

# 验证安装
astrolabe -h
```

### 2. Windows 安装

1. 解压 `astrolabe-windows-x64.zip`；
2. 将 `astrolabe-windows-x64.exe` 重命名为 `astrolabe.exe`；
3. 将其放置在固定目录，如：`C:\Users\<用户名>\.cargo\bin\astrolabe.exe` 或 `C:\Tools\astrolabe\astrolabe.exe`；
4. （可选）将该目录加入 Windows 系统的用户环境变量 `PATH`。

---

## 三、各 AI 编程工具 MCP 配置配方

MCP 客户端通过标准输入输出（stdio）启动 Astrolabe。Astrolabe 支持 21 个图谱与语言智能工具。

### 1. Claude Code 配置

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

## 四、直接给 AI 工具执行的一键安装提示词模板

如果您正在打开新的 Claude Code / Cursor / Windsurf 窗口，可以直接把下面这段话复制发给 AI：

> **提示词模板（可直接复制）：**
> 
> 请帮我把 Astrolabe 代码图谱 MCP 服务配置到当前环境：
> 1. 如果是 macOS，检查 `/usr/local/bin/astrolabe` 是否存在；若不存在，从 Astrolabe 仓库根目录下的 `dist/release-binaries/astrolabe-macos-universal`（即 `<仓库路径>/dist/release-binaries/astrolabe-macos-universal`，如 `/path/to/Astrolabe/dist/release-binaries/astrolabe-macos-universal`）复制到 `~/.cargo/bin/astrolabe` 并建立软链接 `/usr/local/bin/astrolabe`，赋予可执行权限；
> 2. 如果是 Windows，请使用仓库内的 `dist/release-binaries/astrolabe-windows-x64.exe`（如 `C:/path/to/Astrolabe/dist/release-binaries/astrolabe-windows-x64.exe`）；
> 3. 请将以下 MCP 配置注入到全局或本项目的 MCP 配置文件中（如果是 Claude Code 写入 `~/.claude.json` 的 `mcpServers`）：
>    ```json
>    {
>      "astrolabe": {
>        "type": "stdio",
>        "command": "astrolabe",
>        "args": [".", "--context=claude-code"]
>      }
>    }
>    ```
> 4. 执行 `astrolabe -h` 验证配置生效。

---

## 五、高级配置与环境变量

| 环境变量 | 默认值 | 说明 |
|---|---|---|
| `ASTROLABE_CACHE_MB` | `256` | 内存文件缓存上限（MB），设为 `0` 关闭内存缓存 |
| `ASTROLABE_WATCH_SECS` | 未设置 | 后台文件监听模式：未设置使用原生事件（空闲 0% CPU）；设为 `0` 关闭监听；正整数 `N` 强制 `N` 秒轮询兜底 |
| `ASTROLABE_LSP_TIMEOUT` | `300` | LSP 语言服务器单次请求最大超时时间（秒） |
| `ASTROLABE_CONTEXT` | `default` | 客户端上下文预设（`claude-code`, `cursor`, `codex`, `readonly` 等） |
