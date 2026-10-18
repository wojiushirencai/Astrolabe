# Distribution pack (ready to submit — nothing here has been submitted)

Prepared 2026-10-18 for v0.13.0. Every submission is a public action and needs the maintainer's go-ahead. Order matters: (1) publish npm, (2) Glama, (3) awesome-mcp-servers, (4) official registry, (5) the rest.

Honesty rule for all copy: use only the measured claims in the README (15 self-designed tasks, 95 simultaneous pairs, one model, cost −$0.49 / time −83 s, concentrated in aggregation/discovery tasks; v0.12.0 matrix not cheaper). No "faster than X" claims — we have not benchmarked against other servers.

## Shared copy

- **Name:** Astrolabe AI (binary `astrolabe`; repo `wojiushirencai/Astrolabe`)
- **Short (≤100 chars):** `Local MCP server for code search and navigation: search_code, find_symbol, find_references.`
- **One sentence:** Astrolabe AI is a local MCP server that gives Claude Code, Cursor and Codex fast code search, symbol lookup and reference navigation, with directory-level counts for questions like "how many call sites per module".
- **Long:** Single Rust binary; tree-sitter index; language servers started on demand for references/definition/diagnostics; 7 tools; no API key, no LLM calls in the server. Measured on 15 self-designed tasks with one model (95 simultaneous A/B pairs): cost −$0.49 per run [−0.73, −0.28], time −83 s [−129, −41], concentrated in aggregation/discovery tasks; not shown to help on simple tasks. Not related to the npm packages `astrolabe` / `astrolabe-ai`.
- **Install:** `curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh` · `claude mcp add --scope user astrolabe -- astrolabe . --context=claude-code`
- **Categories/tags:** code search, code navigation, developer tools, coding agents; mcp, mcp-server, claude-code, codex, cursor, tree-sitter, lsp, rust

## 1. npm (prerequisite for the official registry) — needs the maintainer

1. Create the npm org/scope `astrolabe-ai` (the unscoped names `astrolabe` and `astrolabe-ai` are taken by unrelated packages; checked 2026-10-18).
2. Add repository secret `NPM_TOKEN` (automation token with publish rights on the scope).
3. Copy `docs/distribution/npm-publish.workflow.yml` to `.github/workflows/npm-publish.yml` (not done in the PR: the token used to push it lacks the `workflow` scope), then run workflow **npm-publish** (`workflow_dispatch`) with `tag=v0.13.0`, first with `dry_run=true`, then `false`. The workflow takes binaries from the release tag and package sources from `source_ref` (default `main`, whose `npm/` version is also 0.13.0 after this PR), so v0.13.0 can be published without a new release. The v0.13.0 tag itself still has the old package names.

## 2. Glama

- Repo root `glama.json` is included (`maintainers: ["wojiushirencai"]`, format from Glama's published example).
- Submit/claim at https://glama.ai/mcp/servers (add the server by repository URL; sign in with GitHub and run "Claim ownership"). Glama may ask for a Dockerfile to run introspection; not prepared.

## 3. awesome-mcp-servers (punkpeye) — needs Glama listing first

Add to the **🤖 Coding Agents** section of README.md, alphabetical by `owner/name` (near the end of the section), via PR from a fork. Their legend: 🦀 Rust, 🏠 local, 🍎 🪟 🐧 OS.

```markdown
- [wojiushirencai/Astrolabe](https://github.com/wojiushirencai/Astrolabe) 🦀 🏠 🍎 🪟 🐧 - Local MCP server for code search and navigation (`search_code` with per-directory counts, `find_symbol`, `find_references`, `goto_definition`) for Claude Code, Cursor and Codex. Rust single binary, tree-sitter index, language servers on demand.
```

After Glama lists the server, insert the Glama score badge the way neighbouring entries do (`[![wojiushirencai/Astrolabe MCP server](https://glama.ai/mcp/servers/wojiushirencai/Astrolabe/badges/score.svg)](https://glama.ai/mcp/servers/wojiushirencai/Astrolabe)`). PR title: `Add wojiushirencai/Astrolabe`. (Their CONTRIBUTING says automated-agent PRs can opt in to fast-track with a `🤖🤖🤖` suffix; we recommend a human submits it.)

## 4. Official MCP registry

- File: `docs/distribution/server.json` (name `io.github.wojiushirencai/astrolabe`, 91-char description; validated against `https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json`).
- Requires the npm package to be published first and `mcpName` in the npm `package.json` (added: `io.github.wojiushirencai/astrolabe`).
- Steps (from the registry quickstart): install `mcp-publisher`; copy `server.json` to the directory you run it from; `mcp-publisher login github`; `mcp-publisher publish`. Verify: `curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.wojiushirencai/astrolabe"`.
- If you want to skip npm, the registry also supports MCPB bundles; we have not built one.

## 5. Smithery

Smithery publishes either a public HTTPS (streamable HTTP) URL or an **MCPB bundle** for local stdio servers (`smithery mcp publish ./server.mcpb -n wojiushirencai/astrolabe`). We have neither; Astrolabe is stdio-only. An MCPB bundle would need to be built per platform. Not prepared; consider after npm.

## 6. mcp.so

Web submission form (https://mcp.so — requirements not verified). Copy: name "Astrolabe AI", URL `https://github.com/wojiushirencai/Astrolabe`, one-sentence description and install lines above, category "Developer Tools".

## 7. Claude Code plugin marketplace (self-hosted, ready)

The repo now contains a marketplace (`.claude-plugin/marketplace.json`, name `astrolabe-ai`) listing one plugin (`claude-plugin/`, registers the MCP server `astrolabe`). Both pass `claude plugin validate`, and `claude plugin marketplace add` + `claude plugin install astrolabe@astrolabe-ai` succeeded in a scratch HOME (Claude Code 2.1.251). Users:

```
/plugin marketplace add wojiushirencai/Astrolabe
/plugin install astrolabe@astrolabe-ai
```

The plugin only registers the server; the `astrolabe` binary must already be on PATH (install.sh). Submitting to Anthropic's official directory is a separate process (https://code.claude.com/docs/en/plugins — "Submit to Anthropic's directory"); not done.

## 8. Other (optional)

crates.io (`cargo publish`, core before mcp; crate metadata still says "deterministic code-graph engine", update before publishing), Homebrew tap, Show HN / Reddit r/ClaudeAI / Linux.do / V2EX posts: use README numbers, not `docs/LINUX_DO_POST.md` (archived, outdated).
