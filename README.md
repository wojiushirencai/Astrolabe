# Astrolabe AI

**Astrolabe AI is a local MCP server that gives Claude Code, Cursor and Codex fast code search, symbol lookup and reference navigation, with directory-level counts for questions like "how many call sites per module".**
It is a single Rust binary (`astrolabe`) that indexes your repo with tree-sitter and starts language servers on demand.

[简体中文](README.zh-CN.md) · [Install guide](docs/INSTALL_GUIDE.md) · [FAQ](docs/FAQ.md) · [Benchmark harness](bench/README.md) · [Changelog](CHANGELOG.md)

> **Not to be confused with** the npm packages `astrolabe` (a 2014 Protractor page-object library) and `astrolabe-ai` (an unrelated resume tool), or other MCP servers named "astrolabe". This project lives at `github.com/wojiushirencai/Astrolabe`; the binary is `astrolabe`.

## Fact card (as of 2026-10-18)

| | |
|---|---|
| Latest release | [v0.13.0](https://github.com/wojiushirencai/Astrolabe/releases/tag/v0.13.0), published 2026-10-18 |
| What it exposes | 7 MCP tools: `resolve_context`, `search_code`, `find_symbol`, `find_references`, `goto_definition`, `get_diagnostics`, `get_languages` |
| `tools/list` size | about 7.6 KB compact JSON (7,584 B measured on Linux; 8,001 B measured on macOS, difference not yet reconciled) |
| Binary | one executable, no runtime; release archives are 7.4–9.1 MB, the unpacked Linux x64 binary is about 50 MB |
| Platforms | macOS arm64/x64, Linux x64 (glibc/musl), Linux arm64, Windows x64 |
| Languages | 17 matrix rows incl. Python, Go, Rust, Java, TypeScript/JavaScript, C/C++, Swift, Objective-C, PHP, Vue, Dart, C#, VB.NET, Kotlin, Shell, Ruby, Lua ([details](docs/REFERENCE.zh-CN.md)) |
| Needs an API key / network? | No. Indexing and search do not call an LLM. (The model that reads the results still spends tokens.) |
| Clients documented | Claude Code, Cursor, Codex, Windsurf/Cline/Roo Code/Continue, Codebuddy, Grok Build, Kimi Code, ZCode, OpenCode |
| License | MIT OR Apache-2.0 |
| Tests | `cargo test --workspace -- --list` lists 1,117 test cases (2026-10-18) |

## Quick start (3 steps)

**1. Install** (macOS / Linux; verifies the release sha256):

```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
```

Other ways (manual download, Windows, build from source): [docs/INSTALL_GUIDE.md](docs/INSTALL_GUIDE.md). An npm package `@astrolabe-ai/astrolabe` is prepared but **not published yet**.

**2. Connect your client.** Claude Code:

```bash
claude mcp add --scope user astrolabe -- astrolabe . --context=claude-code
```

Cursor (`.cursor/mcp.json`): `{"mcpServers":{"astrolabe":{"command":"astrolabe","args":[".","--context=cursor"]}}}`.
Codex, Windsurf and others: [docs/INSTALL_GUIDE.md](docs/INSTALL_GUIDE.md#2-client-configuration).

**3. Verify.** `astrolabe --version` prints the version; in your client, ask it to call `get_languages` or `search_code`. Restart the client session after adding the server.

## The seven tools

| Tool | Use it for |
|---|---|
| `search_code` | Literal/regex search with path filters. Groups hits and labels each as definition / use / comment; ends with a verifiable summary ("23/23 hits, no truncation"). Counting: `aggregate=top\|dir\|file` (hits and files per top-level dir / dir / file), `count_only`, `skip_comments`, `exclude` (e.g. `_test.go`, `vendor`). |
| `resolve_context` | Give it a task description; it gathers relevant files and code skeletons and hints which tool to use next. |
| `find_symbol` | Find a function, class or type by name or substring. |
| `find_references` | Compiler-grade references via the language server; `dry_run_rename=<new name>` lists per-file edit locations without writing anything. |
| `goto_definition` | Jump to the real definition through the language server. |
| `get_diagnostics` | Syntax and type errors for a file. |
| `get_languages` | Languages, file counts, lines of code, language-server status. |

## Evidence: what we measured, and what we did not

All numbers below come from our own paired A/B runs: **A** = Claude Code without Astrolabe, **C** = the same task with Astrolabe connected. Cost is Claude Code's `total_cost_usd`, time is `duration_ms`. One model was used: `gemini-3.8-flash-high` through Claude Code and an Anthropic-compatible proxy (so USD figures use that proxy's pricing). WebSearch/WebFetch were disabled in every arm. Reports: `REPORT_V013E` and `REPORT_V012_BASELINE` (summarized in [bench/README.md](bench/README.md)).

**Simultaneous pairs on v0.13.0 (40 pairs, 4 tasks, 10 pairs each).** Each pair started A and C at the same instant.

| | A (no Astrolabe) | C (Astrolabe v0.13.0) | C − A, paired, 95% bootstrap CI |
|---|---|---|---|
| Cost, mean per run | $3.00 | $2.35 | **−$0.65 [−0.98, −0.31]** |
| Time, mean per run | 600 s | 492 s | **−108 s [−178, −37]** |
| Turns, mean | 50.3 | 40.0 | −10.3 [−14.4, −6.2] |
| Correct, lenient N5 scoring | 38/40 | 40/40 | |
| Correct, strict N5 scoring | 38/40 | 31/40 | see below |

Per task (cost, C − A): N5 −$0.64 [−1.22, −0.02]; N8 −$0.91 [−1.48, −0.31]; N4 −$0.51 [−1.20, +0.21]; E3b −$0.52 [−1.22, +0.18]. The last two intervals include zero, and in N4/E3b the model never used the new `aggregate` options, so those two gains cannot be credited to them.

*Strict N5 scoring:* the reference answer for N5 counts 2 call sites that sit inside comments (model=15/44, total=321). With `skip_comments=true` Astrolabe returns 15/43 and 319, which is also a defensible answer but fails the strict check. The harness scores both conventions ([bench/](bench/README.md)); we report both.

**What this does not show.**
- It covers 4 tasks that we chose because they are counting/locating/editing work. On the other 11 tasks there is **no simultaneous v0.13.0-vs-A run** yet.
- The older v0.12.0 matrix (75 cells, 15 tasks, 5 rounds, triplets started together) found Astrolabe **more expensive**, not cheaper, than not using it: cost C − A = **+$0.244 [+0.035, +0.467]**; time +53 s [−8, +119] and correctness −4.0 points [−10.7, +2.7] both include zero. On its 40 control cells the extra cost was +$0.294 [+0.085, +0.540]. Most of that gap came from four tasks (E3b, N8, H1, N4; picked after seeing the data, so treat it as a hypothesis): without them C − A was +$0.03 [−0.14, +0.21].
- One model, one harness, small samples (10 pairs per task). Latency per turn drifts during the day, so only simultaneous pairs are comparable for time.
- Of the four headline tasks, N5 and N8 run on a public repo (QuantumNous/new-api). N4 and E3b ran on private repos and **cannot be reproduced** by others. The reference snapshot we used was a local copy; see [bench/README.md](bench/README.md) for how to check your checkout against it.

Reproduce the public part: `bash bench/fetch_corpora.sh && bash bench/run_pairs.sh --tasks N5,N8 --pairs 10` (real API spend: our 40-pair run cost about $214), then `python3 bench/report.py`.

## When it helps, when it does not

- **Measured to help:** counting or listing occurrences per directory/module with exclusions (N5, N8): fewer turns and lower cost than the same model doing it with shell scripts.
- **Not shown to help:** the 40 general-purpose control cells of the v0.12.0 matrix (small public repos such as cobra and express among them) cost more with Astrolabe; batch edit workflows (N4, E3b) were cheaper on average but not significantly.
- **Overlap:** Claude Code can provide go-to-definition and find-references through its own LSP plugins ([docs](https://code.claude.com/docs/en/plugins/code-intelligence)); our `goto_definition` / `find_references` overlap with that.
- **Adoption is not guaranteed:** models sometimes ignore MCP tools. In v0.12.0 the model used Astrolabe in 0/5 runs of N5 and 1/5 of N8; after the v0.13.0 changes it did so in 20/20 runs of those two tasks. That is an observation about these tasks, not a promise.

## Related projects

[Serena](https://github.com/oraios/serena) (LSP-based coding toolkit), [codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp), [codegraph](https://github.com/colbymchenry/codegraph), [claude-context](https://github.com/zilliztech/claude-context), the Sourcegraph MCP server, and Claude Code's built-in LSP plugins. We have not benchmarked Astrolabe against any of them; indexing-time numbers that older versions of this README published against Serena and OpenVisio are archived with their caveats in [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## More documentation

[Install & clients](docs/INSTALL_GUIDE.md) · [FAQ](docs/FAQ.md) · [Reference (Chinese): config, env vars, language matrix, quality gates](docs/REFERENCE.zh-CN.md) · [Architecture](docs/ARCHITECTURE.md) · [Performance method](docs/PERFORMANCE.md) · [Optional anti-drift hooks (effect not benchmarked)](docs/HOOKS.md) · [Releasing](.github/RELEASING.md) · [llms.txt](llms.txt)

Build from source: `cargo build --release --bin astrolabe` (Rust 1.90+). Tests: `cargo test --workspace`.

## License

MIT OR Apache-2.0, see [NOTICE](NOTICE). Astrolabe was inspired by [Serena](https://github.com/oraios/serena) and [OpenVisio](https://github.com/openvisio/openvisio); details in [docs/REFERENCE.zh-CN.md](docs/REFERENCE.zh-CN.md#五致谢与技术渊源).
