//! Shell / Bash (`.sh`, `.bash`, shebang) import resolution (lang-batch-8 stub).
//!
//! Owned by the shell worker; see `LANG-PLAN.md`. Until implemented, every
//! import is [`None`] (unresolved), which is the safe default.

use crate::types::{FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

pub struct ShellResolver;

impl ModuleResolver for ShellResolver {
    fn language(&self) -> Language {
        Language::Shell
    }

    fn detect(&self, _files: &FileIndex) -> ProjectMeta {
        ProjectMeta::default()
    }

    fn resolve(
        &self,
        _from: &RelPath,
        _spec: &str,
        _files: &FileIndex,
        _meta: &ProjectMeta,
    ) -> Option<RelPath> {
        None
    }
}
