//! Per-known-agent, per-MCP-session memory of the last unambiguous symbol → path[:line].
//!
//! Agents often call `find_symbol` / `goto_definition`, then `find_references`
//! without `path`. Ambiguous names (e.g. many `Init`s) then get a weak unique-
//! definition guess or an opaque "needs path" error. Remembering the last
//! single-hit resolution in this process lets `find_references` inherit a path
//! when the schema still leaves it optional.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Cap so a long session cannot grow without bound.
const MAX_ENTRIES: usize = 64;

/// One remembered anchor for an exact symbol name under a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentSymbolAnchor {
    pub path: String,
    /// 0-based line when known (definition / use site).
    pub line: Option<u32>,
}

/// Session-local map keyed by `agent\0root\0symbol`. Unknown agents never store anchors.
#[derive(Debug, Default)]
pub(crate) struct RecentSymbols {
    inner: Mutex<HashMap<String, RecentSymbolAnchor>>,
}

fn key(agent: &str, root: &Path, symbol: &str) -> String {
    format!(
        "{}\0{}\0{}",
        agent,
        astrolabe_core::paths::path_norm_key(root),
        symbol
    )
}

impl RecentSymbols {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Remember only when the caller has a single unambiguous hit.
    pub(crate) fn remember(
        &self,
        agent: &str,
        root: &Path,
        symbol: &str,
        path: &str,
        line: Option<u32>,
    ) {
        let symbol = symbol.trim();
        let path = path.trim();
        if agent.is_empty() || symbol.is_empty() || path.is_empty() {
            return;
        }
        let k = key(agent, root, symbol);
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_ENTRIES && !map.contains_key(&k) {
            // Drop an arbitrary entry; order is not part of the contract.
            if let Some(evict) = map.keys().next().cloned() {
                map.remove(&evict);
            }
        }
        map.insert(
            k,
            RecentSymbolAnchor {
                path: path.to_string(),
                line,
            },
        );
    }

    pub(crate) fn forget(&self, agent: &str, root: &Path, symbol: &str) {
        if !agent.is_empty() {
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key(agent, root, symbol.trim()));
        }
    }

    pub(crate) fn recall(
        &self,
        agent: &str,
        root: &Path,
        symbol: &str,
    ) -> Option<RecentSymbolAnchor> {
        let symbol = symbol.trim();
        if agent.is_empty() || symbol.is_empty() {
            return None;
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key(agent, root, symbol))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn remember_and_recall_round_trip() {
        let store = RecentSymbols::new();
        let root = PathBuf::from("/tmp/repo");
        assert!(store.recall("agent-a", &root, "Init").is_none());
        store.remember("agent-a", &root, "Init", "pkg/a.go", Some(10));
        let hit = store.recall("agent-a", &root, "Init").expect("remembered");
        assert_eq!(hit.path, "pkg/a.go");
        assert_eq!(hit.line, Some(10));
        // Other roots / names stay separate.
        assert!(store
            .recall("agent-a", &PathBuf::from("/tmp/other"), "Init")
            .is_none());
        assert!(store.recall("agent-a", &root, "Other").is_none());
    }

    #[test]
    fn empty_symbol_or_path_is_ignored() {
        let store = RecentSymbols::new();
        let root = PathBuf::from("/tmp/repo");
        store.remember("agent-a", &root, "  ", "a.go", None);
        store.remember("agent-a", &root, "Init", "", None);
        assert!(store.recall("agent-a", &root, "Init").is_none());
        assert!(store.recall("agent-a", &root, "  ").is_none());
    }

    #[test]
    fn windows_verbatim_plain_and_slash_roots_share_cache_key() {
        let store = RecentSymbols::new();
        let verbatim = Path::new(r"\\?\C:\repo");
        let plain = Path::new(r"C:\repo");
        let lower_slash = Path::new("c:/repo/");

        assert_eq!(
            key("agent-a", verbatim, "Init"),
            key("agent-a", plain, "Init")
        );
        assert_eq!(
            key("agent-a", verbatim, "Init"),
            key("agent-a", lower_slash, "Init")
        );
        assert_ne!(
            key("agent-a", verbatim, "Init"),
            key("agent-b", plain, "Init")
        );

        store.remember("agent-a", verbatim, "Init", "src/lib.rs", Some(42));
        let hit_plain = store
            .recall("agent-a", plain, "Init")
            .expect("recall via plain");
        assert_eq!(hit_plain.path, "src/lib.rs");
        assert_eq!(hit_plain.line, Some(42));
        assert!(store.recall("agent-b", plain, "Init").is_none());

        let hit_slash = store
            .recall("agent-a", lower_slash, "Init")
            .expect("recall via lower slash");
        assert_eq!(hit_slash.path, "src/lib.rs");
        assert_eq!(hit_slash.line, Some(42));
    }
}
