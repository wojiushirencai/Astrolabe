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
mod explain;
mod knobs;
mod session;
// hooks / instructions 对 bin（main.rs 的子命令分发）公开：hook 协议入口
// 不经 ServerHandler，直接调用模块函数。
pub mod hooks;
mod index;
pub mod instructions;
mod live_backend;
mod loop_guard;
mod openai_schema;
mod precise_tools;
mod reindex;
mod root;
pub mod roots;
mod server;

pub use cli::{parse_launch, Launch, USAGE};
pub use context::{resolve_context_name, ClientContext};
pub use root::{
    child_git_repos, count_root_files, find_project_root, is_cwd_sentinel, resolve_index_root,
    resolve_root, ResolvedRoot, DEFAULT_MAX_ROOT_FILES,
};
pub use server::AstrolabeServer;

pub const TOOL_CATALOG_TTL_MS: u64 = 86_400_000;
/// Catalog tools: `resolve_context`, `search_code`, `find_symbol`,
/// `get_languages`, plus the language-server-backed
/// `find_references` / `goto_definition` / `get_diagnostics`.
pub const TOOL_COUNT: usize = 4 + PRECISE_TOOL_COUNT;
/// Language-server-backed tools defined in `precise_tools`, mounted on the server.
pub const PRECISE_TOOL_COUNT: usize = 3;
