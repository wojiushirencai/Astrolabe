//! Astrolabe MCP server (stdio).
//!
//! Non-negotiable output rule, from testing four clients: **always return
//! `content[0]` as text.** A result carrying only `structuredContent` is
//! silently dropped by Cursor. Claude Code does the opposite: it *replaces*
//! `content[0].text` with `structuredContent`, so a stub `{confidence,
//! budget_tokens}` hides the real hits. Default / `claude-code` context is
//! therefore text-only, with `index_root:` on the first line. `--context=cursor`
//! or `ASTROLABE_STRUCTURED=1` attach the full text plus `root` for clients
//! that actually unpack it. Oversized results go out as a `resource_link`
//! plus a short text summary.
//!
//! Keep the tool surface small. A measured finding from the field: one strong
//! entry-point tool guides an agent better than a menu of narrow ones — fewer
//! mis-selections and less context spent per session. Twenty-one tools
//! ([`astrolabe_mcp::TOOL_COUNT`]): ten graph-level, six language-server-backed
//! (`find_references`, `goto_definition`, `get_diagnostics`, `get_symbol_info`,
//! `plan_rename`, `apply_rename`), `initial_instructions`, and four memory
//! tools. `resolve_context` is the front door. `apply_rename` is gated
//! (auto-applicable or `force=true`) and irreversible.
//!
//! Declare a generous `ttlMs` on `tools/list` so clients stop re-fetching the
//! tool definitions every session.

use astrolabe_mcp::{parse_launch, resolve_index_root, AstrolabeServer, Launch, USAGE};
use rmcp::{service::serve_directly, RoleServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    // Root precedence: positional argument, then ASTROLABE_ROOT, then cwd.
    // The argument comes first because `astrolabe /path/to/repo` is what
    // people type; silently ignoring it would index the wrong tree with no
    // hint, which is exactly the failure mode this project refuses elsewhere.
    //
    // `.` (what MCP configs pass) is not taken literally. Like Serena's
    // `--project-from-cwd`, it walks up from the client spawn cwd looking
    // for `.git` or `.serena/project.yml` and stores the absolute path. An
    // explicit directory is canonicalized as-is and is not replaced by an
    // ancestor marker.
    let launch = parse_launch(std::env::args_os())?;
    let (requested, context) = match launch {
        Launch::Help => {
            print!("{USAGE}");
            return Ok(());
        }
        Launch::Version => {
            println!("astrolabe {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Launch::HooksRemind { client } => {
            std::process::exit(astrolabe_mcp::hooks::run_remind(&client));
        }
        Launch::HooksCleanup => {
            std::process::exit(astrolabe_mcp::hooks::run_cleanup());
        }
        Launch::PrintCcSystemPromptOverride => {
            print!(
                "{}",
                astrolabe_mcp::instructions::cc_system_prompt_override()
            );
            return Ok(());
        }
        Launch::Run {
            requested_root,
            context,
        } => (requested_root, context),
    };
    let root = resolve_index_root(&requested)?;
    tracing::info!(
        root = %root.display(),
        context = %context.name,
        structured = context.structured_or_auto_off(),
        "indexing repository"
    );
    let server = AstrolabeServer::with_context(root, context);
    // Indexing is spawn_blocking, so it never stalls stdio. Start it before the
    // accept loop so handshake-less clients can call tools as soon as the graph
    // is ready rather than waiting for a handshake that may never arrive.
    server.start_indexing();
    // MCP spec 2026-07-28 removed the required `initialize` handshake.
    // `ServiceExt::serve` waits for `initialize` (or a first request carrying
    // complete per-request `_meta`) before entering the request loop. A bare
    // `tools/list` therefore never reaches our handler — rmcp either replies
    // with an opaque `_meta` error and then exits, or a client that swallows
    // that error shows an empty catalog. `serve_directly` accepts requests
    // immediately. Classic `initialize` is still handled in the request loop.
    let service = serve_directly::<RoleServer, _, _, _, _>(server, rmcp::transport::stdio(), None);
    let cancellation = service.cancellation_token();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("received shutdown signal");
            cancellation.cancel();
        }
    });

    service.waiting().await?;
    signal_task.abort();
    Ok(())
}
