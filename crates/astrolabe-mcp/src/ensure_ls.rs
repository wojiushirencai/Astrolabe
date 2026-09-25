//! MCP tool `ensure_language_server`: session-gated language-server install UX.
//!
//! Without `confirm_install` (default): return a needs_install / Ready / AST-only
//! plan and **do not** download.
//! With `confirm_install=true`: call [`LanguageServerInstaller::install`].
//!
//! [`StubInstaller`] plans from discovery and, on confirm, calls
//! [`astrolabe_core::lsp::ensure_installed`] when a catalog spec exists;
//! otherwise returns TODO/`NotImplemented` (RFC-P0 §2 prompt contract).

use astrolabe_core::lsp::install::{
    parse_language_name, InstallError, InstallPlan, LanguageServerInstaller, LspStatus,
    StubInstaller, VersionPolicy,
};
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

fn default_budget() -> usize {
    2_500
}

fn default_confirm() -> bool {
    false
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct EnsureLanguageServerParams {
    #[schemars(
        description = "Language to ensure a server for (python/go/java/rust/typescript/tsx/javascript/c/cpp/objc/objcpp/swift/php/vue). Case-insensitive."
    )]
    pub language: String,
    /// When false (default): return an install plan only — never download.
    /// When true: call the core installer (user must have agreed this session).
    #[serde(default = "default_confirm")]
    #[schemars(
        description = "Must be true to download/install. Default false returns needs_install plan only (session-gated; ask the user first)."
    )]
    pub confirm_install: bool,
    #[serde(default)]
    #[schemars(
        description = "Optional version pin. Omit for latest (RFC default). Pinned installs must not float."
    )]
    pub version: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

/// Run ensure_language_server against `installer` (usually [`StubInstaller`]).
pub(crate) fn run_ensure_language_server(
    params: EnsureLanguageServerParams,
    installer: &dyn LanguageServerInstaller,
) -> CallToolResult {
    let language = match parse_language_name(&params.language) {
        Ok(lang) => lang,
        Err(err) => {
            return text_result(
                format!(
                    "confidence: unknown\n{err}\n\
                     Supported: python, go, java, rust, typescript, tsx, javascript,                      c, cpp, objc, objcpp, swift, php, vue.\n                     get_languages reports Ready / needs_install / AST-only per language.\n"
                ),
                json!({
                    "confidence": "unknown",
                    "status": "unknown_language",
                    "confirm_install": params.confirm_install,
                    "budget_tokens": params.budget_tokens,
                }),
            );
        }
    };

    let plan = match installer.plan(language, params.version.as_deref()) {
        Ok(plan) => plan,
        Err(err) => {
            return text_result(
                format!("confidence: unknown\n{err}\n"),
                json!({
                    "confidence": "unknown",
                    "status": "error",
                    "confirm_install": params.confirm_install,
                    "budget_tokens": params.budget_tokens,
                }),
            );
        }
    };

    if !params.confirm_install {
        return plan_only_result(&plan, params.budget_tokens);
    }

    // confirm_install=true: invoke installer (stub may return NotImplemented).
    match installer.install(&plan) {
        Ok(record) => {
            let body = format!(
                "confidence: exact\n\
                 ensure_language_server: installed / ready\n\
                 language: {}\n\
                 server: {}\n\
                 version: {}\n\
                 command: {}\n\
                 note: {}\n",
                record.language.name(),
                record.server_name,
                record.version,
                record
                    .command
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(none)".into()),
                record.note,
            );
            text_result(
                body,
                json!({
                    "confidence": "exact",
                    "status": "ready",
                    "language": record.language.name(),
                    "server_name": record.server_name,
                    "version": record.version,
                    "confirm_install": true,
                    "budget_tokens": params.budget_tokens,
                }),
            )
        }
        Err(InstallError::NotImplemented(server, policy)) => {
            let body = format!(
                "confidence: unknown\n\
                 ensure_language_server: confirm_install=true accepted, but core installer \
                 is not merged yet (TODO).\n\
                 language: {}\n\
                 server: {server}\n\
                 version_policy: {policy}\n\
                 approximate_size: {}\n\
                 no download was performed.\n\
                 install_hint: {}\n\
                 next_step: install manually via the hint, or retry after the installer \
                 module lands; then re-run the precise tool.\n",
                plan.language.name(),
                plan.approximate_size_bytes
                    .map(|n| format!("~{n} bytes"))
                    .unwrap_or_else(|| "unknown".into()),
                plan.install_hint,
            );
            text_result(
                body,
                json!({
                    "confidence": "unknown",
                    "status": "stub_not_implemented",
                    "language": plan.language.name(),
                    "server_name": server,
                    "version_policy": policy,
                    "confirm_install": true,
                    "budget_tokens": params.budget_tokens,
                }),
            )
        }
        Err(err) => text_result(
            format!("confidence: unknown\nensure_language_server failed: {err}\n"),
            json!({
                "confidence": "unknown",
                "status": "error",
                "language": plan.language.name(),
                "confirm_install": true,
                "budget_tokens": params.budget_tokens,
                "error": err.to_string(),
            }),
        ),
    }
}

fn plan_only_result(plan: &InstallPlan, budget_tokens: usize) -> CallToolResult {
    let status = plan.status.token();
    let mut body = plan.render();
    body.push_str(
        "\nSession gate: installation is user-confirmed. Do not set confirm_install=true \
         until the user agrees in this session.\n",
    );
    text_result(
        body,
        json!({
            "confidence": "exact",
            "status": status,
            "language": plan.language.name(),
            "server_name": plan.server_name,
            "version_policy": plan.version_policy.label(),
            "approximate_size_bytes": plan.approximate_size_bytes,
            "confirm_install": false,
            "budget_tokens": budget_tokens,
            "next_step": match &plan.status {
                LspStatus::NeedsInstall { .. } => {
                    "ask_user_then_confirm_install"
                }
                LspStatus::Ready { .. } => "already_ready",
                LspStatus::AstOnly { .. } => "ast_only_no_install",
            },
        }),
    )
}

fn text_result(body: String, structured: Value) -> CallToolResult {
    let body = if body.trim().is_empty() {
        "ensure_language_server: empty result. confidence: unknown".to_string()
    } else {
        body
    };
    let mut result = CallToolResult::success(vec![ContentBlock::text(body)]);
    result.structured_content = Some(structured);
    result
}

/// Default process-PATH installer used by the MCP server.
pub(crate) fn default_installer() -> StubInstaller {
    StubInstaller::from_process()
}

#[allow(dead_code)]
pub(crate) fn version_policy_label(pin: Option<&str>) -> String {
    VersionPolicy::from_pin(pin).label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::lsp::discovery::Discovery;

    fn first_text(result: &CallToolResult) -> &str {
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|t| t.text.as_str())
            .unwrap_or("")
    }

    #[test]
    fn without_confirm_returns_needs_install_plan() {
        let installer = StubInstaller::new(Discovery::new(""));
        let result = run_ensure_language_server(
            EnsureLanguageServerParams {
                language: "go".into(),
                confirm_install: false,
                version: None,
                budget_tokens: 800,
            },
            &installer,
        );
        let text = first_text(&result);
        assert!(text.contains("needs_install"), "{text}");
        assert!(text.contains("no download performed"), "{text}");
        assert!(text.contains("gopls"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["status"],
            json!("needs_install")
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()["confirm_install"],
            json!(false)
        );
    }

    #[test]
    fn with_confirm_hits_stub_todo_without_download() {
        let installer = StubInstaller::new(Discovery::new(""));
        let result = run_ensure_language_server(
            EnsureLanguageServerParams {
                language: "rust".into(),
                confirm_install: true,
                version: None,
                budget_tokens: 800,
            },
            &installer,
        );
        let text = first_text(&result);
        assert!(text.contains("TODO") || text.contains("not merged"), "{text}");
        assert!(text.contains("no download"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["status"],
            json!("stub_not_implemented")
        );
    }
}
