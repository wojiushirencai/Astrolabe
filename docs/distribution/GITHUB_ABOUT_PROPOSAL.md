# GitHub About proposal for wojiushirencai/Astrolabe (NOT applied — externally visible)

Prepared 2026-10-18. Needs the maintainer's OK; then run the commands below.

**Description (≤350 chars):**
`Local MCP server for code search and navigation (search_code, find_symbol, find_references, goto_definition) for Claude Code, Cursor and Codex. Rust single binary, tree-sitter index, language servers on demand.`

**Homepage:** `https://github.com/wojiushirencai/Astrolabe#readme` (or leave empty until npm exists; once published use `https://www.npmjs.com/package/@zlnap/astrolabe`).

**Topics (20):** `mcp` `mcp-server` `model-context-protocol` `code-search` `code-navigation` `code-intelligence` `claude-code` `codex` `cursor` `windsurf` `tree-sitter` `language-server` `lsp` `ai-coding` `coding-agent` `rust` `developer-tools` `static-analysis` `ripgrep-alternative` `token-efficiency`

Review notes: `token-efficiency` and `ripgrep-alternative` are aspirational; our evidence covers only 4 counting/locating/editing tasks, so you may prefer to drop them (replace with `opencode` and `kimi-code`).

**Commands (run only after approval):**
```bash
gh api -X PATCH repos/wojiushirencai/Astrolabe \
  -f description='Local MCP server for code search and navigation (search_code, find_symbol, find_references, goto_definition) for Claude Code, Cursor and Codex. Rust single binary, tree-sitter index, language servers on demand.' \
  -f homepage='https://github.com/wojiushirencai/Astrolabe#readme'
gh api -X PUT repos/wojiushirencai/Astrolabe/topics \
  -f 'names[]=mcp' -f 'names[]=mcp-server' -f 'names[]=model-context-protocol' -f 'names[]=code-search' \
  -f 'names[]=code-navigation' -f 'names[]=code-intelligence' -f 'names[]=claude-code' -f 'names[]=codex' \
  -f 'names[]=cursor' -f 'names[]=windsurf' -f 'names[]=tree-sitter' -f 'names[]=language-server' \
  -f 'names[]=lsp' -f 'names[]=ai-coding' -f 'names[]=coding-agent' -f 'names[]=rust' \
  -f 'names[]=developer-tools' -f 'names[]=static-analysis' -f 'names[]=ripgrep-alternative' -f 'names[]=token-efficiency'
```
