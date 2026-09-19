//! File discovery: walk the repo and decide what enters the index.
//!
//! Owns: gitignore semantics (root + nested, negation), a universal exclude
//! list (`.git`, `node_modules`, `target`, `build`, `dist`, `__pycache__`,
//! `.venv`), a max file size, binary detection, and content hashing for
//! incremental reindex.
//!
//! Use the `ignore` crate for the walk — it is ripgrep's, handles nested
//! gitignore correctly, and parallelizes. Do not hand-roll glob matching.
//!
//! Report skipped files by reason. The prior art printed only aggregate
//! counts, which hid a 20% parse-failure rate on a real repo.
//!
//! ## Gitignore audit vs. a single walk
//!
//! `ignore` never yields excluded entries, so listing every gitignore skip
//! requires a second walk (rules off, then on). [`ScanOptions::audit_ignored`]
//! is **off by default**: everyday indexing only needs the admitted set, and
//! the extra walk roughly doubles scan cost. Turn it on when diagnosing
//! "why wasn't this file indexed?".
//!
//! ## Build-declared excludes
//!
//! [`crate::types::ProjectMeta::excludes`] is not known until after `detect`,
//! which itself needs a [`FileIndex`] from this module. Do not fold those
//! directories into the first walk. Call [`apply_excludes`] on the
//! [`ScanResult`] (in-memory, no second disk walk) and then [`to_index`].

use crate::types::{FileIndex, Language, RelPath};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};
use memchr::memchr;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Take};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const BINARY_SNIFF_BYTES: u64 = 8 * 1024;
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    ".openvisio",
    ".serena",
];

/// Why a discovered file did not enter the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Ignored,
    TooLarge,
    Binary,
    ReadError,
}

#[derive(Debug, Default, Clone)]
pub struct ScanResult {
    pub files: Vec<RelPath>,
    pub skipped: Vec<(RelPath, SkipReason)>,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub max_file_bytes: u64,
    pub extra_excludes: Vec<String>,
    pub respect_gitignore: bool,
    /// Record gitignore exclusions in [`ScanResult::skipped`].
    ///
    /// Off by default. Daily indexing does not need the gitignore skip list,
    /// and producing it costs a second parallel walk (`ignore` does not
    /// callback excluded entries). Enable this when answering "why wasn't
    /// my file indexed?". Size, binary, read-error, and `extra_excludes`
    /// skips are always recorded, regardless of this flag.
    pub audit_ignored: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_file_bytes: 1_500_000,
            extra_excludes: Vec::new(),
            respect_gitignore: true,
            audit_ignored: false,
        }
    }
}

pub fn scan(root: &Path, opts: &ScanOptions) -> ScanResult {
    let root = root.to_path_buf();
    let excludes = Arc::new(ExcludeMatcher::new(&opts.extra_excludes));

    // `ignore` does not yield ignored entries. The audit trail therefore
    // needs a rules-off walk plus a rules-on walk; skip the extra walk
    // unless the caller asked for it.
    let audit_gitignore = opts.audit_ignored && opts.respect_gitignore;
    let candidates = collect_paths(
        &root,
        !audit_gitignore && opts.respect_gitignore,
        Arc::clone(&excludes),
    );
    let admitted = if audit_gitignore {
        Some(
            collect_paths(&root, true, Arc::clone(&excludes))
                .into_iter()
                .collect::<BTreeSet<_>>(),
        )
    } else {
        None
    };

    let mut result = ScanResult::default();
    for path in candidates {
        let rel = RelPath::new(path_to_rel(&root, &path));

        let gitignored = admitted
            .as_ref()
            .is_some_and(|admitted| !admitted.contains(&path));
        if gitignored || excludes.matches_file(&root, &path) {
            result.skipped.push((rel, SkipReason::Ignored));
            continue;
        }

        let metadata = match path.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(_) => {
                result.skipped.push((rel, SkipReason::ReadError));
                continue;
            }
        };
        if metadata.len() > opts.max_file_bytes {
            result.skipped.push((rel, SkipReason::TooLarge));
            continue;
        }

        match looks_binary(&path, Language::from_path(&rel).is_some()) {
            Ok(true) => result.skipped.push((rel, SkipReason::Binary)),
            Ok(false) => result.files.push(rel),
            Err(_) => result.skipped.push((rel, SkipReason::ReadError)),
        }
    }

    result.files.sort();
    result.skipped.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

pub fn to_index(root: &Path, res: &ScanResult) -> FileIndex {
    FileIndex::new(root, res.files.iter().cloned())
}

/// True if `rel` is exactly `dir`, or a file/dir inside it.
///
/// Matching is by path segment, not string prefix: `target` matches
/// `target/foo.rs` but not `targets/foo.rs`. Empty strings and `"."` never
/// match, so a resolver that has nothing to declare cannot wipe the index.
pub fn path_under_excludes(rel: &RelPath, excludes: &[String]) -> bool {
    let rel = rel.as_str();
    excludes
        .iter()
        .any(|raw| normalize_exclude_dir(raw).is_some_and(|dir| is_under_dir(rel, &dir)))
}

/// Drop admitted files that live under repo-relative exclude directories.
///
/// This is the scan-side consumer of [`crate::types::ProjectMeta::excludes`].
/// `scan` must run before `detect`, so those directories are not known at
/// walk time. The orchestrator should:
///
/// ```ignore
/// let scanned = scan(root, opts);
/// let files = to_index(root, &scanned);
/// let resolvers = ResolverSet::detect(&files);
/// let mut excludes = Vec::new();
/// for lang in [
///     Language::Python,
///     Language::Go,
///     Language::Java,
///     Language::Rust,
///     Language::TypeScript,
/// ] {
///     if let Some(meta) = resolvers.meta_for(lang) {
///         excludes.extend(meta.excludes.iter().cloned());
///     }
/// }
/// let scanned = apply_excludes(scanned, &excludes);
/// let files = to_index(root, &scanned);
/// ```
///
/// In-memory only — no second disk walk. Files already in `skipped` are left
/// as they are; newly excluded files are recorded as [`SkipReason::Ignored`].
/// Prefer this over stuffing `ProjectMeta::excludes` into
/// [`ScanOptions::extra_excludes`]: glob matching there is filename-aware
/// and would treat `out` as "any directory named out", which is broader than
/// the repo-relative directories `detect` emits.
pub fn apply_excludes(mut result: ScanResult, excludes: &[String]) -> ScanResult {
    let dirs: Vec<String> = excludes
        .iter()
        .filter_map(|raw| normalize_exclude_dir(raw))
        .collect();
    if dirs.is_empty() {
        result.files.sort();
        result.skipped.sort_by(|a, b| a.0.cmp(&b.0));
        return result;
    }

    let mut kept = Vec::with_capacity(result.files.len());
    for path in std::mem::take(&mut result.files) {
        if dirs.iter().any(|dir| is_under_dir(path.as_str(), dir)) {
            result.skipped.push((path, SkipReason::Ignored));
        } else {
            kept.push(path);
        }
    }
    result.files = kept;
    result.files.sort();
    result.skipped.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

fn normalize_exclude_dir(raw: &str) -> Option<String> {
    let rel = RelPath::new(raw);
    let dir = rel.as_str().trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        None
    } else {
        Some(dir.to_string())
    }
}

fn is_under_dir(rel: &str, dir: &str) -> bool {
    rel == dir
        || (rel.len() > dir.len() && rel.as_bytes()[dir.len()] == b'/' && rel.starts_with(dir))
}

#[derive(Debug)]
struct ExcludeMatcher {
    globs: GlobSet,
    literals: Vec<String>,
}

impl ExcludeMatcher {
    fn new(patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut literals = Vec::new();
        for pattern in patterns {
            match Glob::new(pattern) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => literals.push(pattern.replace('\\', "/")),
            }
        }
        ExcludeMatcher {
            globs: builder
                .build()
                .expect("validated glob patterns must build successfully"),
            literals,
        }
    }

    fn matches_file(&self, root: &Path, path: &Path) -> bool {
        self.matches(root, path)
    }

    fn matches_dir(&self, root: &Path, path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| EXCLUDED_DIRS.contains(&name))
            || self.matches(root, path)
    }

    fn matches(&self, root: &Path, path: &Path) -> bool {
        let rel = path_to_rel(root, path);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        self.globs.is_match(&rel)
            || self.globs.is_match(name)
            || self
                .literals
                .iter()
                .any(|literal| literal == &rel || literal == name)
    }
}

fn collect_paths(
    root: &Path,
    respect_gitignore: bool,
    excludes: Arc<ExcludeMatcher>,
) -> Vec<PathBuf> {
    let root_for_filter = root.to_path_buf();
    let mut builder = WalkBuilder::new(root);
    builder.hidden(false).require_git(false);
    if !respect_gitignore {
        builder
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
    }
    builder.filter_entry(move |entry| !is_pruned_dir(entry, &root_for_filter, excludes.as_ref()));

    let paths = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&paths);
    builder.build_parallel().run(|| {
        let output = Arc::clone(&output);
        Box::new(move |entry| {
            if let Ok(entry) = entry {
                if entry.depth() > 0 && !entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    output
                        .lock()
                        .expect("scan result mutex poisoned")
                        .push(entry.into_path());
                }
            }
            WalkState::Continue
        })
    });
    drop(output);

    let mut paths = Arc::try_unwrap(paths)
        .expect("all parallel walker references must be dropped")
        .into_inner()
        .expect("scan result mutex poisoned");
    paths.sort();
    paths
}

fn is_pruned_dir(entry: &DirEntry, root: &Path, excludes: &ExcludeMatcher) -> bool {
    entry.depth() > 0
        && entry.file_type().is_some_and(|kind| kind.is_dir())
        && excludes.matches_dir(root, entry.path())
}

fn path_to_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A stray NUL inside a file with a known source extension should not cost us
/// the whole file. Measured on a real repository: `mcp/src/adapter.ts` carries
/// two NUL bytes in 8 KB of otherwise valid TypeScript, and treating it as
/// binary silently dropped its symbols and left three imports unresolvable
/// elsewhere. For source files we therefore require either invalid UTF-8 or a
/// meaningful density of NULs; everything else keeps the strict rule, which is
/// what actually protects us from images and object files.
fn looks_binary(path: &Path, known_source: bool) -> std::io::Result<bool> {
    let file = File::open(path)?;
    let mut reader: Take<File> = file.take(BINARY_SNIFF_BYTES);
    let mut header = Vec::with_capacity(BINARY_SNIFF_BYTES as usize);
    reader.read_to_end(&mut header)?;

    if !known_source {
        return Ok(memchr(0, &header).is_some());
    }
    if header.is_empty() {
        return Ok(false);
    }

    let mut nuls = 0usize;
    let mut rest = header.as_slice();
    while let Some(i) = memchr(0, rest) {
        nuls += 1;
        rest = &rest[i + 1..];
    }
    if nuls == 0 {
        return Ok(false);
    }
    // Truncating the sniff window can split a multi-byte character, so a
    // trailing invalid sequence is not evidence of a binary file.
    let text_like = match std::str::from_utf8(&header) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    Ok(!text_like || nuls * 1000 > header.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos();
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "astrolabe-scan-{}-{nonce}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            TestDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(path, contents).expect("write test file");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn has_skip(result: &ScanResult, path: &str, reason: SkipReason) -> bool {
        result.skipped.contains(&(RelPath::new(path), reason))
    }

    #[test]
    fn scan_honors_gitignore_nested_rules_and_negation() {
        let dir = TestDir::new();
        dir.write(".gitignore", "*.txt\n!keep.txt\n");
        dir.write("drop.txt", "ignored");
        dir.write("keep.txt", "kept");
        dir.write("src/.gitignore", "*.tmp\n!keep.tmp\n");
        dir.write("src/drop.tmp", "ignored");
        dir.write("src/keep.tmp", "kept");
        dir.write("src/main.rs", "fn main() {}");

        let result = scan(
            dir.path(),
            &ScanOptions {
                audit_ignored: true,
                ..ScanOptions::default()
            },
        );
        assert!(result.files.contains(&RelPath::new("keep.txt")));
        assert!(result.files.contains(&RelPath::new("src/keep.tmp")));
        assert!(result.files.contains(&RelPath::new("src/main.rs")));
        assert!(has_skip(&result, "drop.txt", SkipReason::Ignored));
        assert!(has_skip(&result, "src/drop.tmp", SkipReason::Ignored));
    }

    #[test]
    fn scan_prunes_default_excluded_directories() {
        let dir = TestDir::new();
        dir.write("src/main.rs", "fn main() {}");
        dir.write("target/generated.rs", "generated");
        dir.write("node_modules/pkg/index.js", "generated");
        dir.write(".serena/cache.txt", "cache");

        let result = scan(dir.path(), &ScanOptions::default());
        assert_eq!(result.files, vec![RelPath::new("src/main.rs")]);
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn scan_marks_large_files() {
        let dir = TestDir::new();
        dir.write("large.txt", b"12345");
        let opts = ScanOptions {
            max_file_bytes: 4,
            ..ScanOptions::default()
        };

        let result = scan(dir.path(), &opts);
        assert!(has_skip(&result, "large.txt", SkipReason::TooLarge));
    }

    #[test]
    fn scan_marks_binary_files() {
        let dir = TestDir::new();
        dir.write("binary.dat", b"text\0more");

        let result = scan(dir.path(), &ScanOptions::default());
        assert!(has_skip(&result, "binary.dat", SkipReason::Binary));
    }

    #[test]
    fn source_files_survive_a_stray_nul_but_not_a_dense_one() {
        let dir = TestDir::new();
        // Mirrors a real case: two NULs inside 8 KB of valid TypeScript.
        let mut sparse = b"export function f() { return 1 }\n".repeat(80);
        sparse[10] = 0;
        sparse[20] = 0;
        dir.write("sparse.ts", &sparse);
        // Dense NULs are still binary, whatever the extension claims.
        dir.write("dense.ts", b"\0\0\0\0".repeat(64));
        // Unknown extensions keep the strict rule.
        dir.write("blob.dat", b"text\0more");

        let result = scan(dir.path(), &ScanOptions::default());
        assert!(
            result.files.iter().any(|p| p.as_str() == "sparse.ts"),
            "a source file must not be lost to two stray bytes"
        );
        assert!(has_skip(&result, "dense.ts", SkipReason::Binary));
        assert!(has_skip(&result, "blob.dat", SkipReason::Binary));
    }

    #[test]
    fn scan_is_deterministic() {
        let dir = TestDir::new();
        for path in ["z.rs", "a.rs", "nested/y.rs", "nested/b.rs"] {
            dir.write(path, path);
        }

        let first = scan(dir.path(), &ScanOptions::default());
        let second = scan(dir.path(), &ScanOptions::default());
        assert_eq!(first.files, second.files);
        assert_eq!(first.skipped, second.skipped);
        assert!(first.files.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn audit_ignored_false_omits_gitignore_skips_but_keeps_file_set() {
        let dir = TestDir::new();
        dir.write(".gitignore", "*.txt\n!keep.txt\n");
        dir.write("drop.txt", "ignored");
        dir.write("keep.txt", "kept");
        dir.write("src/main.rs", "fn main() {}");
        dir.write("huge.rs", "x".repeat(64));
        dir.write("binary.dat", b"x\0y");

        let audit = ScanOptions {
            audit_ignored: true,
            max_file_bytes: 32,
            ..ScanOptions::default()
        };
        let fast = ScanOptions {
            audit_ignored: false,
            max_file_bytes: 32,
            ..ScanOptions::default()
        };
        assert!(
            !ScanOptions::default().audit_ignored,
            "daily indexing must not pay for a second walk"
        );

        let audited = scan(dir.path(), &audit);
        let fast = scan(dir.path(), &fast);

        assert_eq!(audited.files, fast.files);
        assert!(audited.files.contains(&RelPath::new("keep.txt")));
        assert!(audited.files.contains(&RelPath::new("src/main.rs")));
        assert!(!audited.files.contains(&RelPath::new("drop.txt")));
        assert!(!fast.files.contains(&RelPath::new("drop.txt")));

        assert!(has_skip(&audited, "drop.txt", SkipReason::Ignored));
        assert!(!fast.skipped.iter().any(|(p, _)| p.as_str() == "drop.txt"));
        assert!(!fast.skipped.iter().any(|(_, r)| *r == SkipReason::Ignored));

        assert!(has_skip(&fast, "huge.rs", SkipReason::TooLarge));
        assert!(has_skip(&audited, "huge.rs", SkipReason::TooLarge));
        assert!(has_skip(&fast, "binary.dat", SkipReason::Binary));
        assert!(has_skip(&audited, "binary.dat", SkipReason::Binary));
    }

    #[test]
    fn apply_excludes_filters_dirs_without_prefix_false_positive() {
        // `target/` is pruned during the walk, so the prefix rule is tested
        // on a synthetic result that still contains both `target` and `targets`.
        let synthetic = ScanResult {
            files: vec![
                RelPath::new("src/main.rs"),
                RelPath::new("target/generated.rs"),
                RelPath::new("targets/keep.rs"),
                RelPath::new("pkg/out/A.java"),
                RelPath::new("pkg/outgoing/B.java"),
            ],
            skipped: vec![(RelPath::new("skip.dat"), SkipReason::Binary)],
        };
        let filtered = apply_excludes(
            synthetic,
            &["target".to_string(), "pkg/out".to_string(), "".to_string()],
        );

        assert_eq!(
            filtered.files,
            vec![
                RelPath::new("pkg/outgoing/B.java"),
                RelPath::new("src/main.rs"),
                RelPath::new("targets/keep.rs"),
            ]
        );
        assert!(has_skip(
            &filtered,
            "target/generated.rs",
            SkipReason::Ignored
        ));
        assert!(has_skip(&filtered, "pkg/out/A.java", SkipReason::Ignored));
        assert!(!filtered
            .skipped
            .iter()
            .any(|(p, _)| p.as_str() == "targets/keep.rs"));
        assert!(!filtered
            .skipped
            .iter()
            .any(|(p, _)| p.as_str() == "pkg/outgoing/B.java"));
        assert!(has_skip(&filtered, "skip.dat", SkipReason::Binary));
        assert!(!path_under_excludes(
            &RelPath::new("targets/keep.rs"),
            &["target".to_string()]
        ));
        assert!(path_under_excludes(
            &RelPath::new("target/generated.rs"),
            &["target".to_string()]
        ));

        let dir = TestDir::new();
        dir.write("src/main.rs", "fn main() {}");
        dir.write("targets/keep.rs", "keep");
        dir.write("pkg/out/A.java", "class A {}");
        dir.write("pkg/outgoing/B.java", "class B {}");
        dir.write("target/generated.rs", "generated");

        let scanned = scan(dir.path(), &ScanOptions::default());
        assert!(scanned.files.contains(&RelPath::new("targets/keep.rs")));
        assert!(!scanned
            .files
            .iter()
            .any(|p| p.as_str() == "target/generated.rs"));

        let filtered = apply_excludes(scanned, &["target".to_string(), "pkg/out".to_string()]);
        assert!(filtered.files.contains(&RelPath::new("src/main.rs")));
        assert!(filtered.files.contains(&RelPath::new("targets/keep.rs")));
        assert!(filtered
            .files
            .contains(&RelPath::new("pkg/outgoing/B.java")));
        assert!(!filtered.files.contains(&RelPath::new("pkg/out/A.java")));
        assert!(has_skip(&filtered, "pkg/out/A.java", SkipReason::Ignored));
    }

    #[test]
    fn apply_excludes_and_fast_scan_are_deterministic() {
        let dir = TestDir::new();
        for path in ["z.rs", "a.rs", "out/y.rs", "out/b.rs", "targets/c.rs"] {
            dir.write(path, path);
        }

        let opts = ScanOptions::default();
        assert!(!opts.audit_ignored);
        let first = scan(dir.path(), &opts);
        let second = scan(dir.path(), &opts);
        assert_eq!(first.files, second.files);
        assert_eq!(first.skipped, second.skipped);
        assert!(first.files.windows(2).all(|pair| pair[0] <= pair[1]));

        let excludes = vec!["out".to_string()];
        let first = apply_excludes(first, &excludes);
        let second = apply_excludes(second, &excludes);
        assert_eq!(first.files, second.files);
        assert_eq!(first.skipped, second.skipped);
        assert!(first.files.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(first.skipped.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(
            first.files,
            vec![
                RelPath::new("a.rs"),
                RelPath::new("targets/c.rs"),
                RelPath::new("z.rs"),
            ]
        );
        assert!(has_skip(&first, "out/b.rs", SkipReason::Ignored));
        assert!(has_skip(&first, "out/y.rs", SkipReason::Ignored));
    }

    #[test]
    #[ignore]
    fn bench_audit_ignored_modes_on_corpus() {
        use std::time::{Duration, Instant};

        fn time_scan(
            root: &Path,
            opts: &ScanOptions,
            warmup: usize,
            iters: usize,
        ) -> (Duration, Duration, Duration, usize, usize) {
            for _ in 0..warmup {
                let _ = scan(root, opts);
            }
            let mut times = Vec::with_capacity(iters);
            let mut files = 0usize;
            let mut skipped = 0usize;
            for _ in 0..iters {
                let t = Instant::now();
                let r = scan(root, opts);
                times.push(t.elapsed());
                files = r.files.len();
                skipped = r.skipped.len();
            }
            times.sort();
            (
                times[0],
                times[times.len() / 2],
                times[times.len() - 1],
                files,
                skipped,
            )
        }

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let corpora = [
            ("corpus/java", manifest.join("../../corpus/java")),
            ("../serena", manifest.join("../../../serena")),
        ];

        let fast = ScanOptions::default();
        let audit = ScanOptions {
            audit_ignored: true,
            ..ScanOptions::default()
        };

        for (label, root) in corpora {
            assert!(root.is_dir(), "missing corpus {label}: {}", root.display());
            let (fast_min, fast_t, fast_max, fast_files, fast_skipped) =
                time_scan(&root, &fast, 2, 15);
            let (audit_min, audit_t, audit_max, audit_files, audit_skipped) =
                time_scan(&root, &audit, 2, 15);
            println!(
                "{label}: fast(audit_ignored=false) min/median/max {:?}/{:?}/{:?} files={fast_files} skipped={fast_skipped}; \
                 audit(audit_ignored=true) min/median/max {:?}/{:?}/{:?} files={audit_files} skipped={audit_skipped}; \
                 median ratio {:.2}x",
                fast_min,
                fast_t,
                fast_max,
                audit_min,
                audit_t,
                audit_max,
                audit_t.as_secs_f64() / fast_t.as_secs_f64().max(1e-9),
            );
        }
    }
}
