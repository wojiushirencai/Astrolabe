//! Ruby (`.rb`, `Rakefile`, `Gemfile`) extraction hooks (lang-batch-8).
//!
//! Owned by the ruby worker; see `LANG-PLAN.md`. `parse/mod.rs` calls these
//! hooks for [`Language::Ruby`] only, so all language-specific post-processing
//! lives here and never in shared code.

use crate::types::{Language, SymbolKind};

#[allow(dead_code)]
const LANG: Language = Language::Ruby;

/// Final symbol name / kind for one `@definition.*` match. Default: unchanged.
pub(super) fn symbol_name(
    _definition: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    _source: &str,
) -> (String, SymbolKind) {
    (name.to_owned(), kind)
}

/// Whether a definition is visible outside its file. Default: everything is.
pub(super) fn exported(_name: &str, _node: tree_sitter::Node<'_>, _source: &str) -> bool {
    true
}
