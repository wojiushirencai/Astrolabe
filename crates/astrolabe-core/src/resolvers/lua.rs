//! Lua module resolution (lang-batch-8).
//!
//! `require("a.b")` follows the default `package.path` templates relative to
//! a set of source roots: `<root>/a/b.lua`, then `<root>/a/b/init.lua`.
//! `dofile("x.lua")` / `loadfile("x.lua")` specifiers end in `.lua` and are
//! tried as plain paths (repo root, then the importing file's directory).
//!
//! Source roots, in priority order:
//!   * the repo root,
//!   * conventional roots that exist: `lua/` (Neovim plugins, runtimepath),
//!     `src/`, `lib/`, `script/` (LuaLS), `kong/..`-style roots are the repo
//!     root itself,
//!   * rockspec `build.modules` entries (`["kong.foo"] = "kong/foo.lua"`),
//!     which map a module name straight to a file,
//!   * finally the importing file's ancestor directories, innermost first —
//!     projects that set `package.path` to their own directory
//!     (`test/`, `spec/` helpers, LuaLS `script/`).
//!
//! External modules (`require "cjson"`, `require "vim.lsp"`) yield `None`.

use crate::types::{FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath};

pub struct LuaResolver;

const CONVENTIONAL_ROOTS: [&str; 4] = ["lua", "src", "lib", "script"];

impl ModuleResolver for LuaResolver {
    fn language(&self) -> Language {
        Language::Lua
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut meta = ProjectMeta::default();
        meta.source_roots.push(String::new());
        for root in CONVENTIONAL_ROOTS {
            if files.has_dir(root) {
                meta.source_roots.push(root.to_owned());
            }
        }
        for path in files.iter() {
            if path.extension() == Some("rockspec") && !path.as_str().contains('/') {
                if let Some(text) = files.read(path.as_str()) {
                    rockspec_modules(&text, files, &mut meta.modules);
                }
            }
        }
        meta.modules.sort_by(|a, b| a.name.cmp(&b.name));
        meta.modules.dedup_by(|a, b| a.name == b.name);
        meta
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if let Some(stem) = spec.strip_suffix(".lua") {
            // dofile / loadfile: a path, not a module name.
            let stem = stem.trim_start_matches("./");
            let from_dir = from.dir();
            return [String::new(), from_dir.to_owned()]
                .iter()
                .filter_map(|base| crate::types::join_rel(base, &format!("{stem}.lua")))
                .find(|p| files.contains(p))
                .map(RelPath::new);
        }
        if spec.contains(['/', '\\', ' ']) {
            return None;
        }
        if let Ok(i) = meta.modules.binary_search_by(|m| m.name.as_str().cmp(spec)) {
            if files.contains(&meta.modules[i].dir) {
                return Some(RelPath::new(&meta.modules[i].dir));
            }
        }
        let rel = spec.replace('.', "/");
        let probe = |root: &str| -> Option<RelPath> {
            let base = if root.is_empty() {
                rel.clone()
            } else {
                format!("{root}/{rel}")
            };
            [format!("{base}.lua"), format!("{base}/init.lua")]
                .into_iter()
                .find(|p| files.contains(p))
                .map(RelPath::new)
        };
        if let Some(hit) = meta.source_roots.iter().find_map(|r| probe(r)) {
            return Some(hit);
        }
        // Ancestor directories of the importing file, innermost first.
        let mut dir = from.dir();
        while !dir.is_empty() {
            if let Some(hit) = probe(dir) {
                return Some(hit);
            }
            dir = match dir.rfind('/') {
                Some(i) => &dir[..i],
                None => "",
            };
        }
        None
    }
}

/// `["kong.api"] = "kong/api/init.lua",` entries of a rockspec `build.modules`.
fn rockspec_modules(text: &str, files: &FileIndex, out: &mut Vec<ModuleUnit>) {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("[\"") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once("\"]") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim().trim_end_matches(',').trim();
        let Some(path) = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        else {
            continue;
        };
        if path.ends_with(".lua") && files.contains(path) {
            out.push(ModuleUnit {
                name: name.to_owned(),
                dir: path.to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::new("/nonexistent", paths.iter().map(RelPath::new))
    }

    fn resolve(files: &FileIndex, from: &str, spec: &str) -> Option<String> {
        let meta = LuaResolver.detect(files);
        LuaResolver
            .resolve(&RelPath::new(from), spec, files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn dotted_module_maps_to_file_then_init() {
        let files = index(&["kong/db/init.lua", "kong/tools/utils.lua", "kong/init.lua"]);
        let r = |s| resolve(&files, "kong/init.lua", s);
        assert_eq!(
            r("kong.tools.utils").as_deref(),
            Some("kong/tools/utils.lua")
        );
        assert_eq!(r("kong.db").as_deref(), Some("kong/db/init.lua"));
        assert_eq!(r("kong").as_deref(), Some("kong/init.lua"));
        assert_eq!(r("cjson.safe"), None);
    }

    #[test]
    fn neovim_lua_root() {
        let files = index(&[
            "lua/lspconfig/util.lua",
            "lua/lspconfig.lua",
            "plugin/x.lua",
        ]);
        assert_eq!(
            resolve(&files, "plugin/x.lua", "lspconfig.util").as_deref(),
            Some("lua/lspconfig/util.lua")
        );
        assert_eq!(
            resolve(&files, "plugin/x.lua", "lspconfig").as_deref(),
            Some("lua/lspconfig.lua")
        );
        assert_eq!(resolve(&files, "plugin/x.lua", "vim.lsp"), None);
    }

    #[test]
    fn ancestor_directory_fallback() {
        let files = index(&["tools/gen/a.lua", "tools/gen/helpers/b.lua"]);
        assert_eq!(
            resolve(&files, "tools/gen/a.lua", "helpers.b").as_deref(),
            Some("tools/gen/helpers/b.lua")
        );
    }

    #[test]
    fn dofile_paths() {
        let files = index(&["conf.lua", "spec/helpers.lua", "spec/a_spec.lua"]);
        assert_eq!(
            resolve(&files, "spec/a_spec.lua", "conf.lua").as_deref(),
            Some("conf.lua")
        );
        assert_eq!(
            resolve(&files, "spec/a_spec.lua", "helpers.lua").as_deref(),
            Some("spec/helpers.lua")
        );
    }

    #[test]
    fn rockspec_module_table() {
        let mut out = Vec::new();
        let files = index(&["kong/api/init.lua"]);
        rockspec_modules(
            "build = { modules = {\n  [\"kong.api\"] = \"kong/api/init.lua\",\n  [\"x\"] = \"missing.lua\",\n} }",
            &files,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "kong.api");
    }
}
