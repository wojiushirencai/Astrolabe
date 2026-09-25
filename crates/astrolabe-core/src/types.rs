//! Core types shared by every module. This file is the contract between the
//! parallel workstreams — treat it as frozen unless a change is agreed upon.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Repo-relative path, always normalized to forward slashes and never starting
/// with `./`. Constructing through [`RelPath::new`] guarantees both.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

impl RelPath {
    pub fn new(s: impl AsRef<str>) -> Self {
        let s = s.as_ref().replace('\\', "/");
        let s = s.strip_prefix("./").unwrap_or(&s);
        RelPath(s.trim_start_matches('/').to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parent directory, or `""` for a file at the repo root.
    pub fn dir(&self) -> &str {
        match self.0.rfind('/') {
            Some(i) => &self.0[..i],
            None => "",
        }
    }

    pub fn extension(&self) -> Option<&str> {
        let base = self.0.rsplit('/').next()?;
        base.rsplit_once('.').map(|(_, e)| e)
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Join repo-relative segments and collapse `.`/`..`, staying repo-relative.
/// Returns `None` if the result escapes the repo root.
pub fn join_rel(base: &str, rest: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for seg in base.split('/').chain(rest.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Language {
    Python,
    Go,
    Java,
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    /// C source / headers (`.c`, `.h`).
    C,
    /// C++ source / headers (`.cpp`, `.cc`, `.cxx`, `.hpp`, …).
    Cpp,
    /// Objective-C (`.m`).
    ObjC,
    /// Objective-C++ (`.mm`).
    ObjCpp,
    /// Swift (`.swift`).
    Swift,
    /// PHP (`.php`).
    Php,
    /// Vue single-file components (`.vue`).
    Vue,
}

impl Language {
    /// Language implied by a file extension, if any.
    pub fn from_path(p: &RelPath) -> Option<Self> {
        Some(match p.extension()? {
            "py" | "pyi" => Language::Python,
            "go" => Language::Go,
            "java" => Language::Java,
            "rs" => Language::Rust,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "ipp" => Language::Cpp,
            "m" => Language::ObjC,
            "mm" => Language::ObjCpp,
            "swift" => Language::Swift,
            "php" => Language::Php,
            "vue" => Language::Vue,
            _ => return None,
        })
    }

    /// Parse a canonical language name (`"python"`, `"c++"`, `"objc"`, …).
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "python" | "py" => Some(Language::Python),
            "go" | "golang" => Some(Language::Go),
            "java" => Some(Language::Java),
            "rust" | "rs" => Some(Language::Rust),
            "typescript" | "ts" => Some(Language::TypeScript),
            "tsx" => Some(Language::Tsx),
            "javascript" | "js" => Some(Language::JavaScript),
            "c" => Some(Language::C),
            "cpp" | "c++" | "cxx" => Some(Language::Cpp),
            "objc" | "objective-c" | "objectivec" => Some(Language::ObjC),
            "objcpp" | "objective-c++" | "objectivecpp" => Some(Language::ObjCpp),
            "swift" => Some(Language::Swift),
            "php" => Some(Language::Php),
            "vue" => Some(Language::Vue),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::Go => "go",
            Language::Java => "java",
            Language::Rust => "rust",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::ObjC => "objc",
            Language::ObjCpp => "objcpp",
            Language::Swift => "swift",
            Language::Php => "php",
            Language::Vue => "vue",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SymbolId(pub u32);

/// How much we trust a piece of derived information. Surfaced to the agent so
/// "not found" and "not sure" stay distinguishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// Backed by a compiler / language server.
    Exact,
    /// Backed by scope- and import-aware static resolution.
    Scoped,
    /// Backed by syntax and name matching only; may over- or under-report.
    Syntactic,
    /// We could not determine this. Never silently omit — say so.
    Unknown,
}

// ---------------------------------------------------------------- file index

/// The set of files in the repo, with the lookups resolvers need. Built once
/// per index run and shared immutably across languages.
#[derive(Debug, Default)]
pub struct FileIndex {
    paths: BTreeSet<RelPath>,
    by_dir: BTreeMap<String, Vec<RelPath>>,
    dirs: BTreeSet<String>,
    root: PathBuf,
}

impl FileIndex {
    pub fn new(root: impl Into<PathBuf>, files: impl IntoIterator<Item = RelPath>) -> Self {
        let mut idx = FileIndex {
            root: root.into(),
            ..Default::default()
        };
        for p in files {
            // Register every ancestor directory so `has_dir` is exact.
            let mut cur = p.dir();
            loop {
                idx.dirs.insert(cur.to_string());
                match cur.rfind('/') {
                    Some(i) => cur = &cur[..i],
                    None => break,
                }
            }
            idx.by_dir
                .entry(p.dir().to_string())
                .or_default()
                .push(p.clone());
            idx.paths.insert(p);
        }
        idx
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn contains(&self, p: &str) -> bool {
        self.paths.contains(&RelPath::new(p))
    }

    /// Files directly inside `dir` (not recursive). `""` means the repo root.
    pub fn entries(&self, dir: &str) -> &[RelPath] {
        self.by_dir.get(dir).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn has_dir(&self, dir: &str) -> bool {
        dir.is_empty() || self.dirs.contains(dir)
    }

    pub fn iter(&self) -> impl Iterator<Item = &RelPath> {
        self.paths.iter()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Read a repo file from disk. Intended for build config during `detect`,
    /// not for bulk source reading on the resolve hot path.
    pub fn read(&self, p: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(p)).ok()
    }
}

// ------------------------------------------------------------- project meta

/// A named unit that import statements can address: a Go module path, a Rust
/// crate name, a TS path alias. `name` is what appears in source, `dir` is the
/// repo-relative directory it maps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleUnit {
    pub name: String,
    pub dir: String,
}

/// An explicit source-path redirection declared in code rather than implied by
/// naming, such as Rust's `#[path = "other.rs"] mod name;`.
///
/// Collected during `detect` so that `resolve` stays free of disk access.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathOverride {
    /// File containing the declaration.
    pub declared_in: String,
    /// Module name as written at the declaration site.
    pub name: String,
    /// Repo-relative file the name actually refers to.
    pub target: String,
}

/// What a resolver learned from the project's build configuration. This is the
/// piece OpenVisio never had, and the reason its import graph was empty for
/// every language except TypeScript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectMeta {
    /// Directories that start a module namespace. Java: every
    /// `*/src/main/java`. Python: `src` when using src-layout, else `""`.
    pub source_roots: Vec<String>,
    /// Named units keyed by the prefix used in imports.
    pub modules: Vec<ModuleUnit>,
    /// Repo-relative directories the build declares as output and that must
    /// not be indexed, e.g. Cargo's `target` or Maven's `target`. Consumed by
    /// `scan` on the next pass; a resolver that has nothing to declare leaves
    /// this empty.
    pub excludes: Vec<String>,
    /// Declaration-site path redirections, sorted for determinism.
    pub overrides: Vec<PathOverride>,
}

impl ProjectMeta {
    /// Longest-prefix match over `modules`, so nested workspaces resolve to the
    /// innermost unit. Ties on name length are broken by the shortest `dir`,
    /// keeping the choice deterministic when several units share a name.
    pub fn module_for(&self, spec: &str) -> Option<&ModuleUnit> {
        self.modules
            .iter()
            .filter(|m| spec == m.name || spec.starts_with(&format!("{}/", m.name)))
            .min_by_key(|m| (std::cmp::Reverse(m.name.len()), m.dir.len(), m.dir.as_str()))
    }

    /// The redirection declared for `name` in `declared_in`, if any.
    pub fn override_for(&self, declared_in: &str, name: &str) -> Option<&PathOverride> {
        self.overrides
            .iter()
            .find(|o| o.declared_in == declared_in && o.name == name)
    }
}

/// Maps import specifiers to repo files for one language.
///
/// Contract:
/// - `detect` runs once per index and may read build config from disk.
/// - `resolve` runs per import statement, must be pure and allocation-light.
/// - Returning `None` means "not in this repo" (stdlib/third-party), which is
///   a correct answer, not a failure.
pub trait ModuleResolver: Send + Sync {
    fn language(&self) -> Language;

    fn detect(&self, files: &FileIndex) -> ProjectMeta;

    fn resolve(
        &self,
        from: &RelPath,
        spec: &str,
        files: &FileIndex,
        meta: &ProjectMeta,
    ) -> Option<RelPath>;
}

// ------------------------------------------------------------------- graph

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Interface,
    Struct,
    Enum,
    Trait,
    Type,
    Const,
    Module,
    /// Member of a type: a Java field, a Rust enum variant, a TypeScript
    /// interface property. Language servers report these in document symbols,
    /// so omitting them makes field-heavy classes look almost empty.
    Field,
    /// Named binding at module or file scope that is not a constant.
    Variable,
}

#[derive(Clone, Debug)]
pub struct CodeFile {
    pub id: FileId,
    pub path: RelPath,
    pub language: Option<Language>,
    pub loc: u32,
    /// Content hash, for incremental reindexing.
    pub sha: String,
}

#[derive(Clone, Debug)]
pub struct CodeSymbol {
    pub id: SymbolId,
    pub file: FileId,
    pub name: String,
    pub kind: SymbolKind,
    pub signature: String,
    /// 1-based, inclusive.
    pub start_line: u32,
    pub end_line: u32,
    pub exported: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// File imports file. Derived from build config + import statements.
    Import,
    /// Symbol calls symbol. Name-based, hence `Confidence::Syntactic`.
    Call,
    /// Type extends/implements type.
    Inherit,
}

#[derive(Clone, Debug)]
pub struct CodeEdge {
    pub from: u32,
    pub to: u32,
    pub kind: EdgeKind,
    pub weight: u32,
    pub confidence: Confidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_path_and_name_cover_p0_matrix() {
        assert_eq!(Language::from_path(&RelPath::new("a.c")), Some(Language::C));
        assert_eq!(Language::from_path(&RelPath::new("a.h")), Some(Language::C));
        assert_eq!(
            Language::from_path(&RelPath::new("a.cpp")),
            Some(Language::Cpp)
        );
        assert_eq!(
            Language::from_path(&RelPath::new("a.m")),
            Some(Language::ObjC)
        );
        assert_eq!(
            Language::from_path(&RelPath::new("a.mm")),
            Some(Language::ObjCpp)
        );
        assert_eq!(
            Language::from_path(&RelPath::new("a.swift")),
            Some(Language::Swift)
        );
        assert_eq!(
            Language::from_path(&RelPath::new("a.php")),
            Some(Language::Php)
        );
        assert_eq!(
            Language::from_path(&RelPath::new("a.vue")),
            Some(Language::Vue)
        );
        assert_eq!(Language::from_name("C++"), Some(Language::Cpp));
        assert_eq!(Language::from_name("objective-c"), Some(Language::ObjC));
        assert_eq!(Language::from_name("vue").unwrap().name(), "vue");
    }

    #[test]
    fn relpath_normalizes() {
        assert_eq!(RelPath::new("./a/b.rs").as_str(), "a/b.rs");
        assert_eq!(RelPath::new("a\\b.rs").as_str(), "a/b.rs");
        assert_eq!(RelPath::new("a/b.rs").dir(), "a");
        assert_eq!(RelPath::new("b.rs").dir(), "");
        assert_eq!(RelPath::new("a/b.rs").extension(), Some("rs"));
    }

    #[test]
    fn join_rel_collapses_and_guards() {
        assert_eq!(join_rel("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(join_rel("", "./x/y").as_deref(), Some("x/y"));
        assert_eq!(join_rel("a", "../../x"), None);
    }

    #[test]
    fn file_index_knows_dirs_and_entries() {
        let idx = FileIndex::new(
            "/tmp",
            [
                "src/main/java/com/x/A.java",
                "src/main/java/com/x/B.java",
                "go.mod",
            ]
            .map(RelPath::new),
        );
        assert!(idx.contains("go.mod"));
        assert!(idx.has_dir("src/main/java/com/x"));
        assert!(idx.has_dir("src/main"));
        assert!(!idx.has_dir("src/test"));
        assert_eq!(idx.entries("src/main/java/com/x").len(), 2);
        assert_eq!(idx.entries("").len(), 1);
    }

    #[test]
    fn module_for_prefers_longest_prefix() {
        let meta = ProjectMeta {
            modules: vec![
                ModuleUnit {
                    name: "a".into(),
                    dir: "a".into(),
                },
                ModuleUnit {
                    name: "a/b".into(),
                    dir: "a/b".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(meta.module_for("a/b/c").unwrap().name, "a/b");
        assert_eq!(meta.module_for("a/z").unwrap().name, "a");
        assert!(meta.module_for("zzz").is_none());
    }
}
