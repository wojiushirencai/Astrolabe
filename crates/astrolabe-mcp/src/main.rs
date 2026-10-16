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
//! Keep the tool surface small. Eight tools ([`astrolabe_mcp::TOOL_COUNT`]):
//! `resolve_context`, `search_code`, `find_symbol`, `get_languages`,
//! `list_memories`, `find_references`, `goto_definition`, `get_diagnostics`.
//! `resolve_context` is the front door.
//!
//! Declare a generous `ttlMs` on `tools/list` so clients stop re-fetching the
//! tool definitions every session.

use astrolabe_mcp::{
    child_git_repos, count_root_files, is_cwd_sentinel, parse_launch, resolve_index_root,
    resolve_root, AstrolabeServer, Launch, ResolvedRoot, DEFAULT_MAX_ROOT_FILES, USAGE,
};
use rmcp::{service::serve_directly, RoleServer};
use std::path::Path;

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
        Launch::HooksRemind {
            client,
            deny_text_search,
            context,
        } => {
            std::process::exit(astrolabe_mcp::hooks::run_remind(
                &client,
                deny_text_search,
                context.as_deref(),
            ));
        }
        Launch::HooksCleanup => {
            std::process::exit(astrolabe_mcp::hooks::run_cleanup());
        }
        Launch::HooksGc => {
            std::process::exit(astrolabe_mcp::hooks::run_gc());
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
    let limit = max_root_files_from_env();
    let server = if is_cwd_sentinel(&requested) {
        let cwd = std::env::current_dir()?;
        match resolve_root(&cwd, limit) {
            ResolvedRoot::Repo(path) => {
                emit_root_telemetry(&path, "repo", limit);
                boot_single(path, context)
            }
            ResolvedRoot::Single(path) => {
                emit_root_telemetry(&path, "single", limit);
                boot_single(path, context)
            }
            ResolvedRoot::MultiProject { root, children } => {
                emit_root_telemetry(&root, "multi", limit);
                tracing::info!(
                    root = %root.display(),
                    children = children.len(),
                    "starting in multi-project dispatch mode"
                );
                for child in &children {
                    tracing::info!(child = %child.display(), "multi-project child");
                }
                AstrolabeServer::with_multi_project(root, children, context)
            }
            refuse @ ResolvedRoot::TooLarge { .. } => {
                let msg = refuse
                    .refuse_guidance()
                    .expect("TooLarge always has guidance");
                eprintln!("{msg}");
                std::process::exit(2);
            }
        }
    } else {
        let path = resolve_index_root(&requested)?;
        let nested = child_git_repos(&path);
        if !nested.is_empty() {
            tracing::warn!(
                root = %path.display(),
                nested_repos = nested.len(),
                "explicit root contains nested git repositories; indexing anyway \
                 (cd into a sub-repo, or set ASTROLABE_ROOT to force a specific root)"
            );
        }
        let mode =
            if path.join(".git").exists() || path.join(".serena").join("project.yml").is_file() {
                "repo"
            } else {
                "single"
            };
        emit_root_telemetry(&path, mode, limit);
        boot_single(path, context)
    };
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
    let signaled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal_flag = std::sync::Arc::clone(&signaled);
    let signal_task = tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("received shutdown signal");
        signal_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        cancellation.cancel();
    });

    let served = service.waiting().await;
    signal_task.abort();
    // Client closed stdin, the transport dropped, or SIGINT/SIGTERM: stop the
    // language servers properly (LSP shutdown + exit) so they can persist
    // their state (kotlin-lsp flushes its index), bounded so the client
    // never hangs on us.
    let deadline = astrolabe_core::lsp::pool::exit_deadline();
    let started = std::time::Instant::now();
    let _ = tokio::task::spawn_blocking(move || {
        astrolabe_core::lsp::pool::shutdown_all_pools(deadline)
    })
    .await;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "language servers stopped"
    );
    if signaled.load(std::sync::atomic::Ordering::SeqCst) {
        // The stdio reader is parked in a blocking read of a stdin that is
        // still open; runtime teardown would wait on it forever.
        std::process::exit(0);
    }
    served?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn boot_single(root: std::path::PathBuf, context: astrolabe_mcp::ClientContext) -> AstrolabeServer {
    tracing::info!(
        root = %root.display(),
        context = %context.name,
        structured = context.structured_or_auto_off(),
        "indexing repository"
    );
    AstrolabeServer::with_context(root, context)
}

fn max_root_files_from_env() -> u64 {
    std::env::var("ASTROLABE_MAX_ROOT_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_ROOT_FILES)
}

fn emit_root_telemetry(root: &Path, mode: &str, limit: u64) {
    let (files, truncated) = count_root_files(root, limit);
    let approx = if truncated {
        format!("{files}+")
    } else {
        files.to_string()
    };
    tracing::info!("root={} mode={} files≈{}", root.display(), mode, approx);
}
