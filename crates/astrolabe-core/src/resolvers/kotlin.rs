//! Kotlin (`.kt`, `.kts`) import resolution (lang-batch-8 stub).
//!
//! Owned by the kotlin worker; see `LANG-PLAN.md`. Until implemented, every
//! import is [`None`] (unresolved), which is the safe default.

use crate::types::{FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

pub struct KotlinResolver;

impl ModuleResolver for KotlinResolver {
    fn language(&self) -> Language {
        Language::Kotlin
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
