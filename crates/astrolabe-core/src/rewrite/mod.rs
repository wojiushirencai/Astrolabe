//! Safe structural rewriting.
//!
//! This is the capability nothing in the surveyed prior art provides: the
//! graph-based tools are read-only, and the one commercial implementation of
//! scope-aware cross-language rewriting keeps it behind an enterprise tier.
//! It is also the place where being wrong is most expensive — a missed
//! reference is a compile error the user discovers later, and a spurious one
//! silently corrupts unrelated code.
//!
//! Three rules follow from that:
//!
//! 1. **A plan is computed, reviewed, then applied.** Never edit during
//!    analysis. [`RewritePlan`] is inspectable and carries its own confidence.
//! 2. **Below `Confidence::Scoped`, do not apply automatically.** Return the
//!    candidate sites and say why they are uncertain. Name matching recalls
//!    66% on TypeScript and 18% on Python — applying that blindly would break
//!    one call site in three.
//! 3. **Verify after writing, and roll back on regression.** Re-parse every
//!    touched file; if a file that parsed cleanly before now has error nodes,
//!    the edit was wrong and the whole transaction reverts.
//!
//! `ast-grep` is the execution layer. Its own documentation is explicit that
//! it performs no scope, type, or dataflow analysis, so the decision of *which*
//! sites to change is made here, never delegated to the matcher.

use crate::types::{Confidence, RelPath};

pub mod engine;
pub mod scope_js;
pub mod scope_ts;
pub mod transaction;
pub mod verify;

/// Byte range within a file. Byte offsets (not UTF-16) because this is what
/// the rewriting layer slices strings with; converting once at the boundary
/// is safer than carrying two conventions through the edit path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

/// One site the rewrite would touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub path: RelPath,
    pub range: ByteRange,
    /// Text currently at `range`, kept so application can assert the file has
    /// not changed since planning.
    pub current: String,
    pub replacement: String,
}

/// Why a site is believed to refer to the symbol being renamed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// A language server resolved this reference.
    LanguageServer,
    /// A scope-aware resolver bound this use to the declaration.
    ScopeBinding,
    /// The identifier text matches. May be an unrelated symbol.
    NameMatch,
}

#[derive(Clone, Debug)]
pub struct RewritePlan {
    pub symbol: String,
    pub new_name: String,
    pub sites: Vec<Site>,
    pub evidence: Evidence,
    pub confidence: Confidence,
    /// Sites deliberately excluded, with reasons — comments, strings, or
    /// same-named symbols in other scopes. Surfacing these lets a reviewer
    /// catch an over-narrow plan, which is otherwise invisible.
    pub excluded: Vec<(Site, String)>,
}

impl RewritePlan {
    /// Whether this plan may be written to disk without human confirmation.
    ///
    /// Deliberately conservative: name-matched plans are returned for review
    /// rather than applied. An agent that wants them anyway must say so
    /// explicitly.
    pub fn is_auto_applicable(&self) -> bool {
        matches!(self.confidence, Confidence::Exact | Confidence::Scoped)
    }

    pub fn files(&self) -> Vec<RelPath> {
        let mut v: Vec<RelPath> = self.sites.iter().map(|s| s.path.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RewriteError {
    #[error("file changed since the plan was computed: {0}")]
    Stale(RelPath),
    #[error("rewrite would overlap itself in {0}")]
    Overlapping(RelPath),
    #[error("{0} no longer parses after the edit; rolled back")]
    VerificationFailed(RelPath),
    #[error("io error on {0}: {1}")]
    Io(RelPath, String),
    #[error("scope resolution unavailable for this language: {0}")]
    NoResolver(String),
}

/// Resolves which occurrences of a name actually bind to one declaration.
///
/// This is the part that makes a rewrite safe, and the part `ast-grep`
/// explicitly does not do.
pub trait ScopeResolver: Send + Sync {
    /// Every occurrence in `path` that binds to the declaration containing
    /// `at`, including the declaration itself. Shadowed uses of the same name
    /// must be excluded.
    fn bindings(
        &self,
        path: &RelPath,
        source: &str,
        at: ByteRange,
    ) -> Result<Vec<ByteRange>, RewriteError>;

    /// Confidence this resolver's answers carry.
    fn evidence(&self) -> Evidence;

    /// Whether the symbol at `at` might also be referenced from other files.
    ///
    /// A file-local resolver can see that a symbol is `pub`, exported, or
    /// module-level, but it cannot find the uses elsewhere. Without this
    /// signal a single-file plan looks complete, and renaming a `pub fn`
    /// while its callers keep the old name produces exactly the broken tree
    /// this module exists to prevent.
    ///
    /// Defaults to `true` — a resolver that cannot tell must not let the
    /// engine believe otherwise.
    fn may_span_files(&self, _path: &RelPath, _source: &str, _at: ByteRange) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(path: &str, start: usize, end: usize) -> Site {
        Site {
            path: RelPath::new(path),
            range: ByteRange { start, end },
            current: "old".into(),
            replacement: "new".into(),
        }
    }

    #[test]
    fn name_matched_plans_are_not_auto_applicable() {
        let plan = RewritePlan {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            sites: vec![site("a.ts", 0, 7)],
            evidence: Evidence::NameMatch,
            confidence: Confidence::Syntactic,
            excluded: Vec::new(),
        };
        assert!(
            !plan.is_auto_applicable(),
            "syntactic evidence must require review"
        );
    }

    #[test]
    fn files_are_deduplicated_and_sorted() {
        let plan = RewritePlan {
            symbol: "x".into(),
            new_name: "y".into(),
            sites: vec![site("b.ts", 0, 1), site("a.ts", 0, 1), site("b.ts", 5, 6)],
            evidence: Evidence::ScopeBinding,
            confidence: Confidence::Scoped,
            excluded: Vec::new(),
        };
        let files: Vec<String> = plan.files().iter().map(|p| p.to_string()).collect();
        assert_eq!(files, vec!["a.ts", "b.ts"]);
        assert!(plan.is_auto_applicable());
    }
}
