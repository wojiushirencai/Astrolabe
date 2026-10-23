# Astrolabe AI — install and client setup

Chinese version: [INSTALL_GUIDE.zh-CN.md](INSTALL_GUIDE.zh-CN.md). Last updated 2026-10-18 for v0.13.0.

> **Name check.** This project is *Astrolabe AI* (GitHub: `wojiushirencai/Astrolabe`, binary: `astrolabe`). It is **not** the npm packages `astrolabe` (a 2014 Protractor page-object library) or `astrolabe-ai` (an unrelated resume tool), nor the other MCP servers that happen to be called "astrolabe". Install only from this repository's GitHub Releases or the `@zlnap/*` npm scope.

## 1. Get the binary

Official binaries: [GitHub Releases](https://github.com/wojiushirencai/Astrolabe/releases). Archives are named `astrolabe-<version>-<target>.tar.gz` (`.zip` on Windows); every release ships `SHA256SUMS`.

| Platform | Rust target | v0.13.0 archive size |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | 8,570,892 B |
| macOS Intel | `x86_64-apple-darwin` | 8,791,798 B |
| Linux x64 (glibc) | `x86_64-unknown-linux-gnu` | 8,956,755 B |
| Linux x64 (static, musl) | `x86_64-unknown-linux-musl` | 9,113,635 B |
| Linux arm64 (glibc) | `aarch64-unknown-linux-gnu` | 7,351,404 B |
| Windows x64 | `x86_64-pc-windows-msvc` (`.zip`) | 8,478,933 B |

The unpacked Linux x64 binary is about 50 MB (49,718,560 B, measured 2026-10-18).

### Option 1 — `install.sh` (macOS / Linux, verifies sha256)

```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
# pin a version / directory:
ASTROLABE_VERSION=0.13.0 ASTROLABE_INSTALL_DIR="$HOME/.local/bin" sh install.sh
```

The script downloads the archive for your platform and `SHA256SUMS`, checks the hash, and copies `astrolabe` into the install directory (default `~/.local/bin`). It does not edit your PATH or any config. Read it first if you prefer: [install.sh](../install.sh).

### Option 2 — manual download

```bash
V=0.13.0; T=aarch64-apple-darwin          # pick the target from the table
curl -fsSLO https://github.com/wojiushirencai/Astrolabe/releases/download/v$V/astrolabe-$V-$T.tar.gz
curl -fsSLO https://github.com/wojiushirencai/Astrolabe/releases/download/v$V/SHA256SUMS
grep "astrolabe-$V-$T.tar.gz" SHA256SUMS | sha256sum -c -      # macOS: shasum -a 256 -c -
tar -xzf astrolabe-$V-$T.tar.gz && install -m 0755 astrolabe ~/.local/bin/astrolabe
```

### Option 3 — Windows

Download `astrolabe-<version>-x86_64-pc-windows-msvc.zip`, compare `Get-FileHash -Algorithm SHA256` with `SHA256SUMS`, unzip `astrolabe.exe` to a fixed folder and add it to `PATH`. (The Windows steps were not exercised in this change.)

### Option 4 — npm

The npm package is **`@zlnap/astrolabe`** (binary name stays `astrolabe`), with platform packages `@zlnap/<platform>`. Published under the personal npm scope `zlnap` (no org needed; the npm org `astrolabe-ai` belongs to someone else); Install: `npx -y @zlnap/astrolabe .` then `npx -y @zlnap/astrolabe setup` to wire MCP clients. (Optional CI publish workflow template: `docs/distribution/npm-publish.workflow.yml`.)

### Option 5 — build from source (Rust 1.90+)

```bash
git clone https://github.com/wojiushirencai/Astrolabe.git && cd Astrolabe
cargo build --release --bin astrolabe      # target/release/astrolabe
```

Verify with `astrolabe --version`, then restart your client so it picks up the new MCP server.

## 2. Client configuration

### One-shot setup (recommended)

After installing the npm package (or with `npx`), run:

```bash
npx -y @zlnap/astrolabe setup
```

The command detects Claude Code, Cursor, Codex, Windsurf, Cline, Roo Code, Continue, **OpenCode**, **DeepSeek Harness (DSH)**, **Codebuddy**, **Grok Build**, **Kimi Code**, and **ZCode** on this machine, lets you multi-select which ones to configure, and **merges** Astrolabe into each client's config (it never wipes other servers). Defaults to `npx -y @zlnap/astrolabe` with the right `--context=` flag when the client has one (Codebuddy / Grok / Kimi / ZCode use the mild `default` context and omit `--context`). Useful flags: `--yes` (all detected), `--clients=cursor,claude-code,opencode,dsh,codebuddy,grok,kimicode,zcode`, `--dry-run`, `--native` (use `astrolabe` on PATH), `--list`, `--with-hooks` (OpenCode guard plugin plus Codebuddy / Grok / Kimi / ZCode hook recipes), `--no-cursor-permissions`.

- **OpenCode:** merges into `~/.config/opencode/opencode.jsonc` (or `.json` / older `config.json`, or `~/.opencode/…` if that is what exists). Auto-detects V1 (`mcp.<name>` + `enabled`), V2 (`mcp.servers`), or Claude-like `mcpServers`. JSONC comments are preserved when the parent object already exists. Optional `--with-hooks` writes `~/.config/opencode/plugins/astrolabe-guard.ts`.
- **DeepSeek Harness:** installs vendored `astrolabe-dsh-guard` into `~/.dsh/profiles/**/node_modules` and merges a managed block into `~/.dsh/cordis.patch.yml` without touching the `orca-managed-dsh-hooks` block. DSH MCP servers are per-repo in the UI — setup does not write them.
- **Codebuddy:** MCP into `~/.codebuddy/.mcp.json` (or existing `mcp.json` / `~/.codebuddy.json`); optional `--with-hooks` merges CC-compatible hooks into `~/.codebuddy/settings.json` (PreToolUse + SessionEnd).
- **Grok Build:** MCP into `~/.grok/config.toml` as `[mcp_servers.astrolabe]` (Codex-shaped); optional `--with-hooks` writes `~/.grok/hooks/astrolabe.json`.
- **Kimi Code:** MCP into `~/.kimi-code/mcp.json`; optional `--with-hooks` appends managed `[[hooks]]` blocks in `~/.kimi-code/config.toml` (does not overwrite other hooks).
- **ZCode:** MCP into `~/.zcode/cli/config.json` → `mcp.servers.astrolabe`; optional `--with-hooks` sets `hooks.enabled: true` and writes `hooks.events.PreToolUse` (**never** writes `SessionEnd`, and does not hang cleanup on `Stop`).

Cargo-only installs: `astrolabe setup` re-execs the same flow via `npx` when Node is available.


Astrolabe is a stdio MCP server: `astrolabe . --context=<client>`. `.` means "index the repo containing the working directory". Use an absolute path for `command` if `astrolabe` is not on your PATH.

### Claude Code

One command (syntax checked against Claude Code 2.1.251 `claude mcp add --help`, and the resulting config read back with `claude mcp get`):

```bash
claude mcp add --scope user astrolabe -- astrolabe . --context=claude-code      # all projects
claude mcp add --scope project astrolabe -- astrolabe . --context=claude-code   # writes ./.mcp.json
```

Equivalent JSON (`~/.claude.json` or project `.mcp.json`):

```json
{
  "mcpServers": {
    "astrolabe": { "type": "stdio", "command": "astrolabe", "args": [".", "--context=claude-code"] }
  }
}
```

### Cursor

`~/.cursor/mcp.json` or `.cursor/mcp.json`:

```json
{ "mcpServers": { "astrolabe": { "command": "astrolabe", "args": [".", "--context=cursor"] } } }
```

To skip Cursor's per-tool MCP approval for the seven read-only tools, merge entries `astrolabe:<tool>` into `~/.cursor/permissions.json` → `mcpAllowlist` (do not write `astrolabe:*`; Cursor Run Mode must be enabled). `node scripts/setup-cursor-permissions.mjs` does the merge safely (`--dry-run` to preview, `--cli` for `~/.cursor/cli-config.json`). Tools: `search_code`, `find_symbol`, `resolve_context`, `get_diagnostics`, `find_references`, `goto_definition`, `get_languages`.

### Codex

`~/.codex/config.toml` (standard Codex `[mcp_servers.*]` format; **Codex is not installed on the machine used for this change, so this was not run**):

```toml
[mcp_servers.astrolabe]
command = "astrolabe"
args = [".", "--context=codex"]
```

### Windsurf / Cline / Roo Code / Continue

```json
{ "mcpServers": { "astrolabe": { "command": "astrolabe", "args": ["."] } } }
```

### Codebuddy / Grok Build / Kimi Code / ZCode / OpenCode

One-shot setup above covers these. Manual path summary:

| Client | MCP config | Hooks (optional; effect not benchmarked — see [HOOKS.md](HOOKS.md)) |
|---|---|---|
| Codebuddy | `~/.codebuddy/.mcp.json` (`mcpServers`) | `~/.codebuddy/settings.json` → `hooks` |
| Grok Build | `~/.grok/config.toml` → `[mcp_servers.astrolabe]` | `~/.grok/hooks/astrolabe.json` |
| Kimi Code | `~/.kimi-code/mcp.json` (`mcpServers`) | `~/.kimi-code/config.toml` → `[[hooks]]` |
| ZCode | `~/.zcode/cli/config.json` → `mcp.servers` | same file: `hooks.enabled` + `hooks.events` (no `SessionEnd`) |
| OpenCode | `~/.config/opencode/opencode.jsonc` (V1/V2) | `--with-hooks` → `plugins/astrolabe-guard.ts` |

Stdio server args are `["."]` with the mild `default` context (no `--context` unless the client needs one).

## 3. Environment variables and language servers

- `find_symbol`, `search_code` and the file graph use built-in tree-sitter and need no language server.
- `find_references`, `goto_definition`, `get_diagnostics` need the language's server on `PATH` (otherwise they answer `unknown` with an install hint; servers are only downloaded if you opt in with `ASTROLABE_AUTO_INSTALL=on`, with mandatory checksums; `ASTROLABE_OFFLINE=1` forbids downloads). Override paths with `ASTROLABE_LSP_<LANG>`.
- Full variable table, language matrix and per-language install commands: [REFERENCE.zh-CN.md](REFERENCE.zh-CN.md) (Chinese) and the Chinese guide [INSTALL_GUIDE.zh-CN.md](INSTALL_GUIDE.zh-CN.md).
