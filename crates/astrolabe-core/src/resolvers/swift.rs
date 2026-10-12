//! Swift (`.swift`) import resolution.
//!
//! A Swift `import` names a *module* (a SwiftPM target or an Xcode target), not
//! a file, so resolution maps the module name to its source directory and picks
//! one file in it to stand for the module (the graph is file-level).
//!
//! # `detect`
//!
//! Every `Package.swift` (and `Package@swift-X.Y.swift` variant; the plain
//! manifest is read first and wins) is scanned for target declarations —
//! `.target`, `.executableTarget`, `.testTarget`, `.macro`, `.plugin` — taking
//! the target's own `name:` and optional `path:` arguments (top level of the
//! call only, so `.product(name: ...)` inside `dependencies:` is ignored).
//! The default path is `Sources/<name>`, or `Tests/<name>` for test targets,
//! relative to the manifest's directory. Each target with an existing
//! directory becomes a [`ModuleUnit`].
//!
//! # `resolve`
//!
//! `import Foo`, `@testable import Foo`, `import struct Foo.Bar` → module `Foo`
//! (first path segment). Directory: the registered module, else (no manifest
//! or Xcode project) a `Sources/Foo` or top-level `Foo` directory. Within it,
//! `Foo.swift` wins, then the smallest `.swift` file directly in the directory,
//! then the smallest one below it. `Foundation`, `XCTest` and other external
//! modules resolve to `None`, which is correct.

use crate::types::{FileIndex, Language, ModuleResolver, ModuleUnit, ProjectMeta, RelPath};

pub struct SwiftResolver;

impl ModuleResolver for SwiftResolver {
    fn language(&self) -> Language {
        Language::Swift
    }

    fn detect(&self, files: &FileIndex) -> ProjectMeta {
        let mut manifests: Vec<&RelPath> = files
            .iter()
            .filter(|p| is_manifest(p.file_name()))
            .collect();
        // Plain `Package.swift` before version-specific variants, shallow first.
        manifests.sort_by_key(|p| {
            (
                p.as_str().matches('/').count(),
                p.file_name() != "Package.swift",
                p.as_str().to_owned(),
            )
        });
        let mut meta = ProjectMeta::default();
        for manifest in manifests {
            let Some(text) = files.read(manifest.as_str()) else {
                continue;
            };
            let base = manifest.dir();
            for (name, path, is_test) in parse_manifest(&text) {
                let rel = path.unwrap_or_else(|| {
                    format!("{}/{name}", if is_test { "Tests" } else { "Sources" })
                });
                let Some(dir) = crate::types::join_rel(base, &rel) else {
                    continue;
                };
                if !files.has_dir(&dir) || meta.modules.iter().any(|m| m.name == name) {
                    continue;
                }
                meta.modules.push(ModuleUnit { name, dir });
            }
            if !meta.source_roots.iter().any(|r| r == base) {
                meta.source_roots.push(base.to_owned());
            }
        }
        meta
    }

    fn resolve(
        &self,
        _from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath> {
        let module = spec.trim().split('.').next()?.trim();
        if module.is_empty() {
            return None;
        }
        let dir = meta
            .modules
            .iter()
            .find(|m| m.name == module)
            .map(|m| m.dir.clone())
            .or_else(|| {
                [format!("Sources/{module}"), module.to_owned()]
                    .into_iter()
                    .find(|d| files.has_dir(d))
            })?;
        representative(&dir, module, files)
    }
}

fn is_manifest(name: &str) -> bool {
    name == "Package.swift" || (name.starts_with("Package@swift-") && name.ends_with(".swift"))
}

fn representative(dir: &str, module: &str, files: &FileIndex) -> Option<RelPath> {
    let is_swift = |p: &&RelPath| p.extension() == Some("swift");
    let direct = files.entries(dir);
    if let Some(p) = direct
        .iter()
        .filter(is_swift)
        .find(|p| p.file_name() == format!("{module}.swift"))
    {
        return Some(p.clone());
    }
    if let Some(p) = direct.iter().filter(is_swift).min() {
        return Some(p.clone());
    }
    let prefix = format!("{dir}/");
    files
        .iter()
        .filter(is_swift)
        .filter(|p| p.as_str().starts_with(&prefix))
        .min()
        .cloned()
}

const TARGET_CALLS: &[(&str, bool)] = &[
    (".target(", false),
    (".executableTarget(", false),
    (".testTarget(", true),
    (".macro(", false),
    (".plugin(", false),
];

/// `(name, path, is_test)` for every target declared in a manifest.
fn parse_manifest(text: &str) -> Vec<(String, Option<String>, bool)> {
    let text = strip_comments(text);
    let mut out = Vec::new();
    for &(call, is_test) in TARGET_CALLS {
        let mut from = 0;
        while let Some(pos) = text[from..].find(call) {
            let open = from + pos + call.len();
            from = open;
            let args = top_level_args(&text[open..]);
            let name = args
                .iter()
                .find_map(|(k, v)| (k == "name").then(|| v.clone()));
            let path = args
                .iter()
                .find_map(|(k, v)| (k == "path").then(|| v.clone()));
            if let Some(name) = name {
                out.push((name, path, is_test));
            }
        }
    }
    out
}

/// `label: "string"` pairs at depth 0 of an argument list starting right after
/// the opening parenthesis.
fn top_level_args(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    let mut label_start = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            b',' if depth == 0 => label_start = i + 1,
            b'"' => {
                let end = s[i + 1..].find('"').map_or(bytes.len(), |e| i + 1 + e);
                if depth == 0 {
                    let head = s[label_start..i].trim();
                    if let Some(label) = head.strip_suffix(':') {
                        out.push((label.trim().to_owned(), s[i + 1..end].to_owned()));
                    }
                }
                i = end;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|l| match l.find("//") {
            Some(i) if !l[..i].contains('"') => &l[..i],
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture() -> (PathBuf, FileIndex) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/swift");
        let mut out = Vec::new();
        fn walk(dir: &Path, root: &Path, out: &mut Vec<RelPath>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, root, out);
                } else {
                    let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                    out.push(RelPath::new(&rel));
                }
            }
        }
        walk(&root, &root, &mut out);
        (root.clone(), FileIndex::new(root, out))
    }

    fn resolve(spec: &str, from: &str) -> Option<String> {
        let (_, files) = fixture();
        let meta = SwiftResolver.detect(&files);
        SwiftResolver
            .resolve(&RelPath::new(from), spec, &files, &meta)
            .map(|p| p.as_str().to_owned())
    }

    #[test]
    fn swift_manifest_targets() {
        let (_, files) = fixture();
        let meta = SwiftResolver.detect(&files);
        let mods: Vec<(&str, &str)> = meta
            .modules
            .iter()
            .map(|m| (m.name.as_str(), m.dir.as_str()))
            .collect();
        for want in [
            ("Kit", "Sources/Kit"),
            ("Util", "Sources/Util"),
            ("Lib", "Custom/Lib"),
            ("App", "Sources/App"),
            ("KitTests", "Tests/KitTests"),
        ] {
            assert!(mods.contains(&want), "{want:?} not in {mods:?}");
        }
        // `.product(name: "Dep")` inside dependencies is not a target.
        assert!(!mods.iter().any(|m| m.0 == "Dep"));
    }

    #[test]
    fn swift_imports_resolve_to_module_files() {
        let from = "Sources/App/main.swift";
        assert_eq!(
            resolve("Kit", from).as_deref(),
            Some("Sources/Kit/Kit.swift")
        );
        assert_eq!(
            resolve("Util", from).as_deref(),
            Some("Sources/Util/Util.swift")
        );
        assert_eq!(
            resolve("Lib", from).as_deref(),
            Some("Custom/Lib/LibThing.swift")
        );
        assert_eq!(
            resolve("Kit.Session", from).as_deref(),
            Some("Sources/Kit/Kit.swift")
        );
        assert_eq!(resolve("Foundation", from), None);
        assert_eq!(resolve("XCTest", from), None);
    }

    #[test]
    fn swift_manifest_parser_handles_paths_and_comments() {
        let m = r#"
        // .target(name: "Commented")
        .target(name: "A", dependencies: [.product(name: "X", package: "x")], path: "Src/A"),
        .testTarget(name: "ATests", dependencies: ["A"])
        "#;
        let got = parse_manifest(m);
        assert!(got.contains(&("A".into(), Some("Src/A".into()), false)));
        assert!(got.contains(&("ATests".into(), None, true)));
        assert!(!got.iter().any(|g| g.0 == "Commented" || g.0 == "X"));
    }
}
