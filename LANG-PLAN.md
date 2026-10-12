# lang-batch-8 plan (branch `feat/lang-batch-8`, worktree `/workspace/astro-langs`)

Scope: Swift, ObjC, ObjC++, Vue, Kotlin, Shell/Bash, Ruby, Lua. Bar = same as C#/Dart:
full-declaration recall ~100% vs an independent tree-sitter ground truth, find_symbol 100%
on sampled names, coverage verification per language. ONE branch, ONE worktree, ONE target dir.

## Build rules (all workers)

- `export CARGO_TARGET_DIR=/workspace/astro-langs/target` (shared; sccache stays on; **never `cargo clean`**).
- Every cargo invocation goes through the lock:
  `flock /workspace/astro-langs/.cargo.lock cargo <build|test|clippy> ...`
  (`cargo fmt` does not need the lock). Hold the lock only for the cargo command.
  Prefer scoped tests: `cargo test -p astrolabe-core <lang>` then the full suite once before handing back.
- Do not commit; do not push; do not create branches. The lead commits per language after review.
- Edit **only** the files you own (below). If you need a shared-file change, put the exact
  diff in your report and the lead applies it.
- Before handing back: `cargo fmt --all`, clippy `-D warnings`, `cargo test --workspace` green.

## Scaffolding already in place (commit on this branch)

- Grammar crates (workspace `Cargo.toml`): tree-sitter-swift 0.7, tree-sitter-objc 3.0,
  tree-sitter-kotlin-ng 1.1, tree-sitter-bash 0.25, tree-sitter-ruby 0.23, tree-sitter-lua 0.5.
  All LanguageFn / tree-sitter-language 0.1 mode, load under tree-sitter 0.27, MIT, cargo-deny clean.
- `Language` new variants + storage tags (append-only): Kotlin=17, Shell=18, Ruby=19, Lua=20
  (ObjC=9, ObjCpp=10, Swift=11, Vue=13 already existed).
- Parser pool slots (`parse/mod.rs`, `rewrite/verify.rs`, `[Language; 20]`): ObjC=13, ObjCpp=14
  (ObjC grammar), Swift=15, Kotlin=16, Shell=17, Ruby=18, Lua=19. Vue stays dispatched to `parse/vue.rs`.
- Extension map: `.kt .kts` Kotlin; `.sh .bash` Shell; `.rb .rake .gemspec` + `Rakefile Gemfile Guardfile
  Podfile Fastfile Brewfile` Ruby; `.lua` Lua. Shebang (`from_path_and_source`, extension-less files only):
  sh/bash/dash/ksh → Shell, ruby → Ruby, lua/luajit → Lua.
- LSP: kotlin-lsp → kotlin-language-server; bash-language-server `start`; ruby-lsp → solargraph `stdio`;
  lua-language-server. Env overrides `ASTROLABE_LSP_{KOTLIN,SHELL,RUBY,LUA}`. LSP languageIds:
  kotlin / shellscript / ruby / lua. Swift (sourcekit-lsp), ObjC (sourcekit-lsp → clangd), Vue (vue-language-server) were already wired.
- Placeholder queries: first line contains `placeholder (lang-batch-8)`; while present, the
  capture-convention tests skip that language (compile test still runs). **Delete that line when
  real patterns land** — the full capture checks then apply.
- Resolver stubs return `None`; registered in `resolvers::all()`. ObjC++ imports route to the ObjC resolver.
- `hooks.rs` SUPPORTED_CODE_EXTENSIONS now includes kt kts sh bash rb rake gemspec lua (do not merge
  to main before those languages actually extract symbols).

## File ownership

| Worker | Owns (only these) |
|---|---|
| swift | `crates/astrolabe-core/src/parse/swift.rs`, `parse/queries/swift/{symbols,imports,calls}.scm`, `resolvers/swift.rs`, `tests/fixtures/swift/**` (new) |
| objc (+objc++) | `parse/objc.rs`, `parse/queries/objc/*.scm`, `resolvers/objc.rs`, `tests/fixtures/objc/**`. Note: `.mm` uses the ObjC grammar; C++ constructs produce ERROR nodes — measure & report, propose a cpp-fallback only via lead. |
| vue | `parse/vue.rs` (existing script-peel extractor), `tests/fixtures/vue/**`. Vue reuses TS/JS queries — TS query changes go through the lead. Resolver stays TypeScript. |
| kotlin | `parse/kotlin.rs`, `parse/queries/kotlin/*.scm`, `resolvers/kotlin.rs` (Gradle source sets / package→dir, Java-resolver style), `tests/fixtures/kotlin/**` |
| shell | `parse/shell.rs`, `parse/queries/bash/*.scm`, `resolvers/shell.rs` (`source`/`.` relative paths), `tests/fixtures/shell/**` |
| ruby | `parse/ruby.rs`, `parse/queries/ruby/*.scm`, `resolvers/ruby.rs` (`require_relative`, `require` via `lib/`), `tests/fixtures/ruby/**` |
| lua | `parse/lua.rs`, `parse/queries/lua/*.scm`, `resolvers/lua.rs` (`require("a.b")` → `a/b.lua`, `a/b/init.lua`), `tests/fixtures/lua/**` |
| bench (optional 8th/9th) | `/workspace/recall-bench/lang8/**` only (GT scripts, corpora, results) |

Paths above are relative to `crates/astrolabe-core/src/` unless rooted. Hooks per language file:
`symbol_name(def_node, name, kind, src) -> (String, SymbolKind)` and `exported(name, node, src) -> bool`
(both called from `parse/mod.rs` for that language only). Unit tests go inside the owned file (`#[cfg(test)] mod tests`).

**Lead-only shared files** (send diffs, don't edit): `Cargo.toml`, `Cargo.lock`, `types.rs`, `store.rs`,
`parse/mod.rs`, `parse/queries/mod.rs`, `rewrite/verify.rs`, `rewrite/engine.rs` (keywords/comment
classify: Shell/Ruby currently use the Python `#` classifier, Lua uses C-like as a TODO, keywords empty),
`resolvers/mod.rs`, `lsp/*`, `index.rs`, `astrolabe-mcp/**`, README/docs, CI.

## Test / bench conventions

- Ground truth: independent Python script in the style of `/workspace/jcs/gt.py`
  (`tree_sitter_language_pack` parser, walk every node, collect declaration node types + name field),
  saved as `/workspace/recall-bench/lang8/gt_<lang>.py`. Compare against an Astrolabe symbol dump
  (file\tname) exactly like gt.py: report total GT, matched, recall %, and the top missed node types.
- Also run `/workspace/recall-bench/bench.py`-style find_symbol sampling (MCP, `mcp.py`) on 200 random GT names.
- Corpora go in `/workspace/recall-bench/corpora/<name>` (shallow clone, pin the commit in the report).
- Targets: full-declaration recall ≥99% (explain every residual), find_symbol sample 100%,
  zero parse panics, index time reported. Report before/after table (before = 0 for new languages).

Suggested corpora (pick 1-2):

| Lang | Primary | Secondary |
|---|---|---|
| Swift | Alamofire/Alamofire | apple/swift-argument-parser |
| ObjC | AFNetworking/AFNetworking | SDWebImage/SDWebImage |
| ObjC++ | facebook/react-native (`*.mm` under React/) | Tencent/MMKV (iOS `.mm`) |
| Vue | vuejs/core `packages-private/` + element-plus (`.vue`) | vbenjs/vue-vben-admin |
| Kotlin | square/okhttp (Kotlin sources) | JetBrains/kotlinx.coroutines |
| Shell | nvm-sh/nvm | ohmyzsh/ohmyzsh `tools/` + bats-core/bats-core |
| Ruby | sinatra/sinatra | rubocop/rubocop |
| Lua | kong/kong (subset) or neovim/nvim-lspconfig | LuaLS/lua-language-server `script/` |
