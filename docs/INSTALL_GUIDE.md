# Astrolabe AI — install and client setup

Chinese version: [INSTALL_GUIDE.zh-CN.md](INSTALL_GUIDE.zh-CN.md). Last updated 2026-11-01 for v0.17.5+ (Windows stability #117/#119, MCP short names, required `setup`).

> **Name check.** This project is *Astrolabe AI* (GitHub: `wojiushirencai/Astrolabe`, binary: `astrolabe`). It is **not** the npm packages `astrolabe` (a 2014 Protractor page-object library) or `astrolabe-ai` (an unrelated resume tool), nor the other MCP servers that happen to be called "astrolabe". Install only from this repository's GitHub Releases or the `@zlnap/*` npm scope.

## 1. Get the binary

Official binaries: [GitHub Releases](https://github.com/wojiushirencai/Astrolabe/releases). Archives are named `astrolabe-<version>-<target>.tar.gz` (`.zip` on Windows); every release ships `SHA256SUMS`.

| Platform | Rust target | v0.17.0 archive size |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | 9,728,734 B |
| macOS Intel | `x86_64-apple-darwin` | 10,078,526 B |
| Linux x64 (glibc) | `x86_64-unknown-linux-gnu` | 10,238,301 B |
| Linux x64 (static, musl) | `x86_64-unknown-linux-musl` | 10,406,095 B |
| Linux arm64 (glibc) | `aarch64-unknown-linux-gnu` | 8,507,630 B |
| Windows x64 | `x86_64-pc-windows-msvc` (`.zip`) | 9,666,430 B |

Archive sizes from the [v0.17.0](https://github.com/wojiushirencai/Astrolabe/releases/tag/v0.17.0) GitHub Release assets.

### Option 1 — `install.sh` (macOS / Linux, verifies sha256)

```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
# pin a version / directory:
ASTROLABE_VERSION=0.17.0 ASTROLABE_INSTALL_DIR="$HOME/.local/bin" sh install.sh
```

The script downloads the archive for your platform and `SHA256SUMS`, checks the hash, and copies `astrolabe` into the install directory (default `~/.local/bin`). It does not edit your PATH or any config. Read it first if you prefer: [install.sh](../install.sh).

### Option 2 — manual download

```bash
V=0.17.0; T=aarch64-apple-darwin          # pick the target from the table
curl -fsSLO https://github.com/wojiushirencai/Astrolabe/releases/download/v$V/astrolabe-$V-$T.tar.gz
curl -fsSLO https://github.com/wojiushirencai/Astrolabe/releases/download/v$V/SHA256SUMS
grep "astrolabe-$V-$T.tar.gz" SHA256SUMS | sha256sum -c -      # macOS: shasum -a 256 -c -
tar -xzf astrolabe-$V-$T.tar.gz && install -m 0755 astrolabe ~/.local/bin/astrolabe
```

### Option 3 — Windows

```powershell
# Prefer npm (pulls the matching platform package):
npm i -g @zlnap/astrolabe
# Then REQUIRED — wire MCP (+ hooks so agents actually use the tools):
npx -y @zlnap/astrolabe setup --with-hooks
# or: astrolabe setup --with-hooks
```

Or download `astrolabe-<version>-x86_64-pc-windows-msvc.zip` from [Releases](https://github.com/wojiushirencai/Astrolabe/releases), compare `Get-FileHash -Algorithm SHA256` with `SHA256SUMS`, unzip `astrolabe.exe` to a fixed folder, add it to `PATH`, then still run `astrolabe setup --with-hooks`.

**Windows stability (tested on real machines, v0.17.4–v0.17.5):** `#117` normalizes `\?\` vs plain `C:\` for watcher membership / comparisons and strips `\?\` in status text; subdirectory `index_root` reuses a Ready parent index with a path filter (no second cold build). `#119` normalizes Windows `path_filter` (backslashes / absolute paths) to repo-relative `/` paths. Prefer omitting `index_root` or passing the **repo root**.

### Option 4 — npm (install alone is not enough)

The npm package is **`@zlnap/astrolabe`** (binary name stays `astrolabe`), with platform packages `@zlnap/<platform>`. **Published** under the personal npm scope `zlnap` (no org needed; the npm org `astrolabe-ai` belongs to someone else). Unscoped `astrolabe` / `astrolabe-ai` on npm are unrelated — do not use them.

```bash
npm i -g @zlnap/astrolabe          # or: npx -y @zlnap/astrolabe --version
# REQUIRED after every fresh install / upgrade:
npx -y @zlnap/astrolabe setup --with-hooks
```

**Why `setup` is required:** `npm i` only places the binary shim. Without `astrolabe setup`, no client MCP entry is written. Without hooks (`--with-hooks` where supported), many agents keep Grep/Read and never call Astrolabe tools in practice. `setup` detects Claude Code, Cursor, Codex, Windsurf, Cline, Roo, Continue, OpenCode, DeepSeek Harness, Codebuddy, Grok Build, Kimi Code, and ZCode and **merges** MCP config (it never wipes other servers).

### Option 5 — build from source (Rust 1.90+)

```bash
git clone https://github.com/wojiushirencai/Astrolabe.git && cd Astrolabe
cargo build --release --bin astrolabe      # target/release/astrolabe
```

Verify with `astrolabe --version`, then run `npx -y @zlnap/astrolabe setup --with-hooks` (or `astrolabe setup --with-hooks`) and restart your client so it picks up MCP + hooks.

## 2. Client configuration

### One-shot setup (required after install)

After installing the binary or npm package (or with `npx`), run:

```bash
npx -y @zlnap/astrolabe setup --with-hooks
# non-interactive:
npx -y @zlnap/astrolabe setup --yes --with-hooks
```

**Install without `setup` leaves you with a binary that clients do not call.** Hooks (`--with-hooks`) nudge agents away from Grep/Read toward Astrolabe; skip them only if you intentionally want MCP-only wiring.

The command detects Claude Code, Cursor, Codex, Windsurf, Cline, Roo Code, Continue, **OpenCode**, **DeepSeek Harness (DSH)**, **Codebuddy**, **Grok Build**, **Kimi Code**, and **ZCode** on this machine, lets you multi-select which ones to configure, and **merges** Astrolabe into each client's config (it never wipes other servers). Defaults to `npx -y @zlnap/astrolabe` with the right `--context=` flag when the client has one (Codebuddy / Grok / Kimi / ZCode use the mild `default` context and omit `--context`). Useful flags: `--yes` (all detected), `--clients=cursor,claude-code,opencode,dsh,codebuddy,grok,kimicode,zcode`, `--dry-run`, `--native` (use `astrolabe` on PATH), `--list`, `--with-hooks` (OpenCode guard plugin plus Cursor / Codebuddy / Grok / Kimi / ZCode hook recipes), `--no-cursor-permissions`, `--scope=global|project` (Cursor; default global), `--project <dir>` (Cursor project root; default: cwd's git root).

**MCP tool short names** (what `tools/list` shows): `resolve_contx`, `search_code`, `find_symbol`, `find_refs`, `goto_def`, `get_diag`, `get_lang`. Legacy long names remain aliases on `tools/call`.

- **Cursor (default `--scope=global`):** writes `~/.cursor/mcp.json` with the absolute native binary, `type: stdio`, and `env` containing `PATH`, `ASTROLABE_LSP_TIMEOUT=300`, plus `ASTROLABE_LSP_PYTHON` when `pyright-langserver` is found. Also runs `cursor-agent mcp enable astrolabe` in the current project directory as a best-effort step and removes a stale `astrolabe` entry from `mcp-disabled.json`. Cursor has no file-based global rules directory, so L1 guidance comes from the MCP instructions plus the global `~/.cursor/hooks.json` remind hook (enable with `--with-hooks`, absolute hook command, matcher `Shell|Read|Grep|astrolabe-.*`). Existing other `mcpServers` entries are preserved; re-running is idempotent.
- **Cursor project scope (`--scope=project`):** writes the three-piece setup for the git root / `--project`: `<project>/.cursor/mcp.json` (absolute binary, `cwd` = project root, same `env`), `<project>/.cursor/rules/astrolabe.mdc` (`alwaysApply: true`, generated from the same L1 source as the MCP instructions), and approves that directory with `cursor-agent mcp enable astrolabe`.
- **OpenCode:** merges into `~/.config/opencode/opencode.jsonc` (or `.json` / older `config.json`, or `~/.opencode/…` if that is what exists). Auto-detects V1 (`mcp.<name>` + `enabled`), V2 (`mcp.servers`), or Claude-like `mcpServers`. JSONC comments are preserved when the parent object already exists. Optional `--with-hooks` writes `~/.config/opencode/plugins/astrolabe-guard.ts`.
- **DeepSeek Harness:** installs vendored `astrolabe-dsh-guard` into `~/.dsh/profiles/**/node_modules` and merges a managed block into `~/.dsh/cordis.patch.yml` without touching the `orca-managed-dsh-hooks` block. DSH MCP servers are per-repo in the UI — setup does not write them.
- **Codebuddy:** MCP into `~/.codebuddy/.mcp.json` (or existing `mcp.json` / `~/.codebuddy.json`); optional `--with-hooks` merges CC-compatible hooks into `~/.codebuddy/settings.json` (PreToolUse + SessionEnd).
- **Grok Build:** MCP into `~/.grok/config.toml` as `[mcp_servers.astrolabe]` (Codex-shaped); optional `--with-hooks` writes `~/.grok/hooks/astrolabe.json`.
- **Kimi Code:** MCP into `~/.kimi-code/mcp.json`; optional `--with-hooks` appends managed `[[hooks]]` blocks in `~/.kimi-code/config.toml` (does not overwrite other hooks).
- **ZCode:** MCP into `~/.zcode/cli/config.json` → `mcp.servers.astrolabe`; optional `--with-hooks` sets `hooks.enabled: true` and writes `hooks.events.PreToolUse` (**never** writes `SessionEnd`; **scrubs** any existing `Stop` → cleanup so counters are not zeroed each turn).

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

`astrolabe setup` defaults to the global `~/.cursor/mcp.json`. Add `--scope=project` (and optionally `--project <dir>`) to write the project-level `.cursor/mcp.json` and `.cursor/rules/astrolabe.mdc` instead. Both keep other servers and use the absolute native binary:

```json
{
  "mcpServers": {
    "astrolabe": {
      "type": "stdio",
      "command": "/abs/path/to/node_modules/@zlnap/darwin-arm64/astrolabe",
      "args": [".", "--context=cursor"],
      "cwd": "/abs/path/to/project",
      "env": {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "ASTROLABE_LSP_TIMEOUT": "300",
        "ASTROLABE_LSP_PYTHON": "/abs/path/to/pyright-langserver"
      }
    }
  }
}
```

`cwd` and the `.cursor/rules` file are project-scope only. Cursor CLI approval is recorded per project in `~/.cursor/projects/<slug>/mcp-approvals.json`; a global MCP entry cannot pre-approve future projects. Default setup runs `cursor-agent mcp enable astrolabe` in the current project as a best-effort step, then tells you that new projects prompt for approval on first use (`--approve-mcps` for headless runs, or run the command manually in that project). `--scope=project` runs the same command for the target directory and verifies the approvals file. Editing the server entry changes the config hash and requires re-approval, so re-run setup after config changes. `setup` also removes `astrolabe` from `mcp-disabled.json`.

Cursor has no file-based global rules directory (User Rules live in the UI). Global L1 guidance therefore comes from the MCP `instructions` field plus the global `~/.cursor/hooks.json` remind hook, which `--with-hooks` registers with an absolute binary path and the Cursor-native `astrolabe-.*` matcher.

To skip Cursor's per-tool MCP approval for the seven read-only tools, merge **short** entries `astrolabe:<tool>` into `~/.cursor/permissions.json` → `mcpAllowlist` (do not write `astrolabe:*`; Cursor Run Mode must be enabled). `astrolabe setup` and `node scripts/setup-cursor-permissions.mjs` write short catalog names only and **migrate** any leftover legacy long names in the allowlist to short ones (`--dry-run` to preview, `--cli` for `~/.cursor/cli-config.json`). Allowlist tools: `search_code`, `find_symbol`, `resolve_contx`, `get_diag`, `find_refs`, `goto_def`, `get_lang`. Legacy long names remain **call-time** aliases on `tools/call` only (forgotten client configs still work).

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
- `find_refs`, `goto_def`, `get_diag` (legacy: `find_references` / `goto_definition` / `get_diagnostics`) need the language's server on `PATH` (otherwise they answer `unknown` with an install hint; servers are only downloaded if you opt in with `ASTROLABE_AUTO_INSTALL=on`, with mandatory checksums; `ASTROLABE_OFFLINE=1` forbids downloads). Override paths with `ASTROLABE_LSP_<LANG>`.
- Full variable table, language matrix and per-language install commands: [REFERENCE.zh-CN.md](REFERENCE.zh-CN.md) (Chinese) and the Chinese guide [INSTALL_GUIDE.zh-CN.md](INSTALL_GUIDE.zh-CN.md).
