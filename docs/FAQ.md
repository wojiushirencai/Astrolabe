# Astrolabe AI — FAQ

Last updated 2026-10-18 (v0.13.0). Numbers are from our own runs; see [bench/README.md](../bench/README.md) for method and limits.

## What is Astrolabe AI?
A local MCP server (single Rust binary `astrolabe`) that gives AI coding agents such as Claude Code, Cursor and Codex code search, symbol lookup and reference navigation over your repository. It exposes 7 tools; `search_code` can also count hits per directory.

## Who is it for?
Developers who use an MCP-capable coding agent on a medium or large repository and often ask counting, locating or cross-file questions ("how many calls to X per module", "where is Y defined and who calls it").

## Who is it not for?
Not shown to help: general small tasks on small repos (in our v0.12.0 control cells Astrolabe cost more than not using it, +$0.294 [+0.085, +0.540] per run), and cases where your client's built-in LSP tools already do what you need. We have no evidence for benefits outside the tasks we measured.

## Is it faster or cheaper than not using it?
On 15 tasks of our own design, 95 simultaneous A/B pairs with v0.13.0 (5 pairs for 11 tasks, 10 for 4): cost −$0.49 per run [−0.73, −0.28] and time −83 s [−129, −41] versus the same model without Astrolabe, with correctness at least as high under the primary scoring (95/95 vs 93/95; strict scoring 86/95 vs 93/95, the gap is one task, N5, where excluding comments is a defensible answer). The gain is concentrated in aggregation/discovery tasks (N1, N5, N8); simple locating tasks were about even. One model only; this is not evidence of a general benefit. The older v0.12.0 matrix was not better (cost +$0.244 [+0.035, +0.467]). Details in the [README](../README.md#evidence-what-we-measured-and-what-we-did-not) and [bench/results/](../bench/results/).

## How is it different from Serena, codebase-memory-mcp, codegraph, claude-context or Claude Code's built-in LSP plugins?
All of them target the same problem (fewer blind greps for coding agents). We have **not** benchmarked Astrolabe against them, so we make no claim of being better. Factual differences we can state: Astrolabe is a single Rust binary with tree-sitter indexing, runs without an embedding model or API key, exposes only 7 tools, and has `search_code` aggregate counting (`aggregate=top|dir|file`, `skip_comments`, `exclude`). Claude Code's LSP plugins provide go-to-definition and find-references natively ([docs](https://code.claude.com/docs/en/plugins/code-intelligence)), which overlaps with our `goto_definition` / `find_references`.

## Is it the same as the npm package `astrolabe` or `astrolabe-ai`?
No. Those are unrelated third-party packages (a 2014 Protractor library; a resume tool). This project's npm packages are planned under the `@zlnap/` scope and are not published yet. Install from [GitHub Releases](https://github.com/wojiushirencai/Astrolabe/releases) or with `install.sh`.

## Does it send my code anywhere?
By reading the code: the server does not call an LLM and has no telemetry upload (the "root telemetry" function only writes one local log line). The only network code is the optional language-server installer, which is off by default (`ASTROLABE_AUTO_INSTALL=on` enables it, checksums are mandatory, `ASTROLABE_OFFLINE=1` blocks it). This is a code-reading statement, not an independent network audit. The model your client uses still receives whatever tool results it asks for.

## How do I install it?
```bash
curl -fsSL https://raw.githubusercontent.com/wojiushirencai/Astrolabe/main/install.sh | sh
claude mcp add --scope user astrolabe -- astrolabe . --context=claude-code
```
Other clients and options: [INSTALL_GUIDE.md](INSTALL_GUIDE.md).

## How do I verify the benefit on my own repo?
Use the paired harness in [bench/](../bench/README.md): it runs the same task with and without Astrolabe at the same moment and reports paired cost/time/turn differences with bootstrap intervals. It costs real API money; start with `--pairs 3`.

## Which languages are supported?
Python, Go, Rust, Java, TypeScript/JavaScript, C/C++, Swift, Objective-C/C++, PHP, Vue, Dart, C#, VB.NET, Kotlin, Shell, Ruby and Lua. tree-sitter covers symbol search for all of them; `find_references`, `goto_definition` and `get_diagnostics` need the language's server on PATH. Matrix: [REFERENCE.zh-CN.md](REFERENCE.zh-CN.md).

## What are the anti-drift hooks? Should I use them?
Optional PreToolUse hooks that deny repeated grep/whole-file reads. Their effect has **not** been benchmarked (our A/B runs did not use them), so they are not part of the recommended setup. See [HOOKS.md](HOOKS.md).

## Why did the model not call Astrolabe in my session?
Models sometimes prefer shell tools. In our v0.12.0 runs it used Astrolabe in 0/5 of N5 and 1/5 of N8; after v0.13.0's changes in 20/20 of those two tasks. If it ignores the tools, name them in your prompt (`search_code` with `aggregate=top`) or in your project instructions.
