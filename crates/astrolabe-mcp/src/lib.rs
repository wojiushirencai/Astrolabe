//! Astrolabe MCP protocol adapter.
//!
//! Every tool response is constructed in `server` and starts with a non-empty
//! text content block. Structured content is off by default (`default` and
//! `claude-code` contexts): Claude Code substitutes `structuredContent` for
//! `content[0].text` and will hide the real hits if that field is only
//! `{confidence, budget_tokens}`. Named contexts (`--context` /
//! `ASTROLABE_CONTEXT`) and `ASTROLABE_STRUCTURED` can turn it on.

mod cli;
mod context;
mod ensure_ls;
// hooks / instructions 对 bin（main.rs 的子命令分发）公开：hook 协议入口
// 不经 ServerHandler，直接调用模块函数。
pub mod hooks;
mod index;
pub mod instructions;
mod live_backend;
mod memory;
mod openai_schema;
mod precise_tools;
mod root;
mod server;

pub use cli::{parse_launch, Launch, USAGE};
pub use context::{resolve_context_name, ClientContext};
pub use root::{find_project_root, resolve_index_root};
pub use server::AstrolabeServer;

pub const TOOL_CATALOG_TTL_MS: u64 = 86_400_000;
/// Graph tools (resolve_context/skeleton/hotspots/languages/dependents/
/// find_symbol/search_code/trace_calls plus `get_neighborhood` and
/// `get_group_graph`), the language-server-backed ones (`find_references`/
/// `goto_definition`/`get_diagnostics`/`get_symbol_info`/`plan_rename`/
/// `apply_rename`), the `initial_instructions` bootstrap tool, the four
/// memory tools, and `ensure_language_server` (session-gated install UX).
pub const TOOL_COUNT: usize = 10 + PRECISE_TOOL_COUNT + 1 + 4 + 1;
/// Language-server-backed tools defined in `precise_tools`, mounted on the server.
pub const PRECISE_TOOL_COUNT: usize = 6;
