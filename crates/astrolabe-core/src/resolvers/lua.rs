//! Lua (`.lua`) import resolution (lang-batch-8 stub).
//!
//! Owned by the lua worker; see `LANG-PLAN.md`. Until implemented, every
//! import is [`None`] (unresolved), which is the safe default.

use crate::types::{FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

pub struct LuaResolver;

impl ModuleResolver for LuaResolver {
    fn language(&self) -> Language {
        Language::Lua
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
