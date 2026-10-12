//! Ruby (`.rb`, `Rakefile`, `Gemfile`) import resolution (lang-batch-8).
//!
//! The parser hands over the raw path of `require` / `require_relative` /
//! `load` / `autoload` without saying which call it came from, so:
//!
//! * `./x`, `../x` — relative to the requiring file (only).
//! * otherwise, in order: the `$LOAD_PATH` roots Bundler / gemspecs set up
//!   (every `lib/` directory, the one enclosing the requiring file first),
//!   then the requiring file's directory (`require_relative "helper"`), then
//!   the repo root (`require "spec/support/x"` with `-I .`).
//!
//! `.rb` is appended unless present. Gems (`require "json"`) are `None`.

use crate::types::{join_rel, FileIndex, Language, ModuleResolver, ProjectMeta, RelPath};

pub struct RubyResolver;

impl ModuleResolver for RubyResolver {
    fn language(&self) -> Language {
        Language::Ruby
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut roots: Vec<String> = Vec::new();
        for p in files.iter() {
            if !p.as_str().ends_with(".rb") {
                continue;
            }
            let dir = p.dir();
            // Innermost `lib` ancestor of this file.
            let mut acc = String::new();
            let mut found: Option<String> = None;
            for seg in dir.split('/').filter(|s| !s.is_empty()) {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(seg);
                if seg == "lib" {
                    found = Some(acc.clone());
                }
            }
            if let Some(r) = found {
                if !roots.contains(&r) {
                    roots.push(r);
                }
            }
        }
        // Shallowest first: `lib` before `vendor/x/lib`.
        roots.sort_by_key(|r| (r.matches('/').count(), r.clone()));
        ProjectMeta {
            source_roots: roots,
            ..Default::default()
        }
    }

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let spec = spec.trim();
        if spec.is_empty() || spec.contains("#{") {
            return None;
        }
        let spec = spec.strip_suffix(".rb").unwrap_or(spec);
        let probe = |base: &str| -> Option<RelPath> {
            let joined = join_rel(base, &format!("{spec}.rb"))?;
            files.contains(&joined).then(|| RelPath::new(&joined))
        };
        if spec.starts_with("./") || spec.starts_with("../") {
            return probe(from.dir());
        }
        if spec.starts_with('/') {
            return None;
        }
        // Enclosing lib root first, then the others.
        let from_s = from.as_str();
        let enclosing = meta
            .source_roots
            .iter()
            .filter(|r| from_s.starts_with(&format!("{r}/")))
            .max_by_key(|r| r.len());
        if let Some(hit) = enclosing.and_then(|r| probe(r)) {
            return Some(hit);
        }
        for r in &meta.source_roots {
            if let Some(hit) = probe(r) {
                return Some(hit);
            }
        }
        probe(from.dir()).or_else(|| probe(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> FileIndex {
        FileIndex::new(
            "/nonexistent",
            [
                "lib/sinatra.rb",
                "lib/sinatra/base.rb",
                "lib/sinatra/main.rb",
                "rack-protection/lib/rack/protection.rb",
                "rack-protection/lib/rack/protection/base.rb",
                "test/test_helper.rb",
                "test/base_test.rb",
            ]
            .map(RelPath::new),
        )
    }

    fn r(from: &str, spec: &str) -> Option<String> {
        let files = idx();
        let meta = RubyResolver.detect(&files);
        RubyResolver
            .resolve(&RelPath::new(from), spec, &files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn load_path_and_relative() {
        assert_eq!(
            r("lib/sinatra/main.rb", "sinatra/base").as_deref(),
            Some("lib/sinatra/base.rb")
        );
        assert_eq!(
            r("lib/sinatra/main.rb", "./base").as_deref(),
            Some("lib/sinatra/base.rb")
        );
        assert_eq!(
            r("test/base_test.rb", "test_helper").as_deref(),
            Some("test/test_helper.rb")
        );
        assert_eq!(
            r("test/base_test.rb", "../lib/sinatra.rb").as_deref(),
            Some("lib/sinatra.rb")
        );
        assert_eq!(
            r(
                "rack-protection/lib/rack/protection.rb",
                "rack/protection/base"
            )
            .as_deref(),
            Some("rack-protection/lib/rack/protection/base.rb")
        );
        assert_eq!(r("test/base_test.rb", "json"), None);
        assert_eq!(r("lib/sinatra.rb", "#{x}/y"), None);
    }
}
