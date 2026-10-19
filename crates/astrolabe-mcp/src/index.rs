//! Repository index held by the server.
//!
//! The build itself lives in `astrolabe_core::index` — this module only adds
//! the lookups the tool layer needs. Keeping one orchestration path means the
//! numbers the server reports are the same ones the acceptance suite measures.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::UNIX_EPOCH,
};

use astrolabe_core::cache::FileCache;
use astrolabe_core::graph::CodeGraph;
use astrolabe_core::index::IncrementalIndex;
use astrolabe_core::{CodeFile, FileId, IndexReport, RelPath};

use crate::reindex::mcp_index_options;

/// Serving snapshot. Production holds an [`IncrementalIndex`] so the watch
/// thread can `apply_changeset` without a full rescan. Unit tests that inject a
/// synthetic graph use [`RepoIndex::from_graph`].
pub(crate) struct RepoIndex {
    pub root: PathBuf,
    inner: InnerIndex,
    in_graph_paths: OnceLock<HashSet<String>>,
    names: OnceLock<NameIndex>,
    /// Markdown coverage report path, written lazily once per snapshot.
    coverage_report: OnceLock<Option<PathBuf>>,
    /// Process-unique stamp; every index replacement (edit, rebuild) gets a new one.
    generation: u64,
}

/// Lookup tables over `graph().symbols`, built once per index snapshot (the
/// snapshot is immutable, so they never go stale): lowercase names so
/// `find_symbol` does not allocate per symbol per query, an exact-name set,
/// a lowercase-name → positions map for exact lookups, and id → position.
pub(crate) struct NameIndex {
    pub lower: Vec<String>,
    pub exact: HashSet<String>,
    pub by_lower: std::collections::HashMap<String, Vec<u32>>,
    pub by_id: std::collections::HashMap<astrolabe_core::SymbolId, usize>,
    pub name_path: astrolabe_core::name_path::NamePathIndex,
    pub file_pos: std::collections::HashMap<FileId, usize>,
}

enum InnerIndex {
    Incremental(IncrementalIndex),
    #[cfg(test)]
    Static {
        graph: CodeGraph,
        report: IndexReport,
    },
}

impl std::fmt::Debug for RepoIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoIndex")
            .field("root", &self.root)
            .field("files", &self.graph().files.len())
            .field("symbols", &self.graph().symbols.len())
            .finish_non_exhaustive()
    }
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl RepoIndex {
    /// Index generation stamp (changes whenever the index is replaced).
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn from_incremental(root: PathBuf, incremental: IncrementalIndex) -> Self {
        Self {
            root,
            inner: InnerIndex::Incremental(incremental),
            in_graph_paths: OnceLock::new(),
            names: OnceLock::new(),
            coverage_report: OnceLock::new(),
            generation: next_generation(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_graph(root: PathBuf, graph: CodeGraph, report: IndexReport) -> Self {
        Self {
            root,
            inner: InnerIndex::Static { graph, report },
            in_graph_paths: OnceLock::new(),
            names: OnceLock::new(),
            coverage_report: OnceLock::new(),
            generation: next_generation(),
        }
    }

    pub(crate) fn names(&self) -> &NameIndex {
        self.names.get_or_init(|| {
            let symbols = &self.graph().symbols;
            let mut idx = NameIndex {
                lower: Vec::with_capacity(symbols.len()),
                exact: HashSet::new(),
                by_lower: std::collections::HashMap::new(),
                by_id: std::collections::HashMap::with_capacity(symbols.len()),
                name_path: astrolabe_core::name_path::NamePathIndex::new(symbols),
                file_pos: self
                    .graph()
                    .files
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(i, f)| (f.id, i))
                    .collect(),
            };
            for (i, symbol) in symbols.iter().enumerate() {
                let lower = symbol.name.to_lowercase();
                idx.by_lower
                    .entry(lower.clone())
                    .or_default()
                    .push(i as u32);
                idx.lower.push(lower);
                if !idx.exact.contains(&symbol.name) {
                    idx.exact.insert(symbol.name.clone());
                }
                idx.by_id.entry(symbol.id).or_insert(i);
            }
            idx
        })
    }

    pub(crate) fn graph(&self) -> &CodeGraph {
        match &self.inner {
            InnerIndex::Incremental(inc) => &inc.graph,
            #[cfg(test)]
            InnerIndex::Static { graph, .. } => graph,
        }
    }

    pub(crate) fn report(&self) -> &IndexReport {
        match &self.inner {
            InnerIndex::Incremental(inc) => &inc.report,
            #[cfg(test)]
            InnerIndex::Static { report, .. } => report,
        }
    }

    pub(crate) fn coverage_report_cell(&self) -> &OnceLock<Option<PathBuf>> {
        &self.coverage_report
    }

    pub(crate) fn incremental(&self) -> Option<&IncrementalIndex> {
        match &self.inner {
            InnerIndex::Incremental(inc) => Some(inc),
            #[cfg(test)]
            InnerIndex::Static { .. } => None,
        }
    }

    pub fn file(&self, id: FileId) -> Option<&CodeFile> {
        // `.rev()` above makes the first file with this id win, as `find` did.
        let pos = *self.names().file_pos.get(&id)?;
        self.graph().files.get(pos)
    }

    /// Count occurrences of unindexed code file extensions among admitted paths.
    ///
    /// Delegates to [`astrolabe_core::index::unindexed_code_exts`] on the retained
    /// [`IncrementalIndex`]. Only returns entries for unindexed, unsupported code languages.
    pub(crate) fn unindexed_code_exts(
        &self,
        filter: Option<&str>,
    ) -> std::collections::BTreeMap<String, u32> {
        match &self.inner {
            InnerIndex::Incremental(inc) => {
                let in_graph_set = self.in_graph_paths.get_or_init(|| {
                    self.graph()
                        .files
                        .iter()
                        .map(|file| file.path.as_str().to_owned())
                        .collect()
                });
                astrolabe_core::index::unindexed_code_exts(
                    inc.admitted_paths(),
                    &|path| in_graph_set.contains(path),
                    filter,
                )
            }
            #[cfg(test)]
            InnerIndex::Static { .. } => std::collections::BTreeMap::new(),
        }
    }

    /// Scanned text files that are not in the code graph (docs, config,
    /// SQL, templates, unsupported languages …), filtered by the
    /// `path_filter` substring. These already passed the scanner's
    /// gitignore / size / binary gates. Lock files are skipped: they are
    /// large, generated, and drown real hits.
    pub(crate) fn text_only_paths(&self, filter: Option<&str>) -> Vec<&RelPath> {
        match &self.inner {
            InnerIndex::Incremental(inc) => {
                let in_graph_set = self.in_graph_paths.get_or_init(|| {
                    self.graph()
                        .files
                        .iter()
                        .map(|file| file.path.as_str().to_owned())
                        .collect()
                });
                inc.admitted_paths()
                    .iter()
                    .filter(|path| {
                        let s = path.as_str();
                        filter.is_none_or(|f| s.contains(f))
                            && !in_graph_set.contains(s)
                            && !is_lock_file(s)
                    })
                    .collect()
            }
            #[cfg(test)]
            InnerIndex::Static { .. } => Vec::new(),
        }
    }

    /// Authored sources hidden by a declared build-output exclude (e.g. a
    /// tsconfig `outDir` that still contains sources). Empty for static test
    /// indexes.
    pub(crate) fn excluded_source_paths(&self) -> &[astrolabe_core::types::RelPath] {
        match &self.inner {
            InnerIndex::Incremental(inc) => inc.excluded_source_paths(),
            #[cfg(test)]
            InnerIndex::Static { .. } => &[],
        }
    }

    /// Load source lines through the process-wide [`FileCache`].
    ///
    /// The current mtime is always passed to [`FileCache::get`], so an edited
    /// file is a miss and the stale entry is invalidated. The `bool` is
    /// `true` on a cache hit.
    pub fn cached_lines(
        &self,
        cache: &FileCache,
        rel_path: &str,
    ) -> Option<(Arc<Vec<String>>, bool)> {
        load_source_lines(cache, &self.root, rel_path)
    }

    /// Resolve a user-supplied target to a file. Accepts a full repo-relative
    /// path or any path suffix, so an agent can pass `server.ts` without
    /// knowing the directory. Exact matches win; among suffix matches the
    /// shortest path wins, which keeps the choice stable and predictable.
    pub fn file_id(&self, target: &str) -> Option<FileId> {
        let target = target.trim_start_matches("./");
        if let Some(file) = self
            .graph()
            .files
            .iter()
            .find(|file| file.path.as_str() == target)
        {
            return Some(file.id);
        }
        let suffix = format!("/{target}");
        self.graph()
            .files
            .iter()
            .filter(|file| file.path.as_str().ends_with(&suffix))
            .min_by_key(|file| file.path.as_str().len())
            .map(|file| file.id)
    }
}

/// Generated dependency lock files excluded from text-only search.
fn is_lock_file(path: &str) -> bool {
    const LOCKS: &[&str] = &[
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "Cargo.lock",
        "go.sum",
        "poetry.lock",
        "Pipfile.lock",
        "uv.lock",
        "composer.lock",
        "Gemfile.lock",
        "pubspec.lock",
        "Podfile.lock",
        "packages.lock.json",
    ];
    let name = path.rsplit('/').next().unwrap_or(path);
    LOCKS.contains(&name)
}

/// Read a source file through [`FileCache`], keyed by repo-relative path.
///
/// Returns `(lines, hit)`. A changed mtime is a miss; the cache already
/// invalidates the stale entry in that case. On a miss the file is read from
/// disk, inserted, and returned even if it is immediately evicted for being
/// larger than the byte budget.
pub(crate) fn load_source_lines(
    cache: &FileCache,
    root: &Path,
    rel_path: &str,
) -> Option<(Arc<Vec<String>>, bool)> {
    let disk = root.join(rel_path);
    // Stat before reading: a write after this point changes mtime/len, so
    // the entry inserted below is never served for the newer content.
    let metadata = fs::metadata(&disk).ok();
    let mtime_ms = metadata.as_ref().and_then(mtime_ms_of).unwrap_or(0);
    let len = metadata.as_ref().map(fs::Metadata::len);
    if let Some(len) = len {
        if let Some(lines) = cache.get_checked(rel_path, mtime_ms, len) {
            return Some((lines, true));
        }
    }
    let source = astrolabe_core::text::read_source(&disk).ok()?;
    let lines = Arc::new(source.lines().map(str::to_owned).collect::<Vec<String>>());
    if let Some(len) = len {
        cache.insert_checked(rel_path, Arc::clone(&lines), mtime_ms, len);
    }
    Some((lines, false))
}

fn mtime_ms_of(metadata: &fs::Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
pub(crate) fn build(root: &Path) -> anyhow::Result<RepoIndex> {
    build_with_options(root, true, astrolabe_core::index::DEFAULT_PARSE_CACHE_BYTES)
}

pub(crate) fn build_with_options(
    root: &Path,
    persist: bool,
    parse_cache_bytes: u64,
) -> anyhow::Result<RepoIndex> {
    let opts = mcp_index_options(persist, parse_cache_bytes);
    let incremental = IncrementalIndex::build(root, &opts);
    Ok(RepoIndex::from_incremental(root.to_path_buf(), incremental))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir() -> PathBuf {
        // A timestamp alone is not unique: macOS resolves `SystemTime::now`
        // to roughly a microsecond, so two tests starting in the same instant
        // get the same path, and whichever finishes first deletes the other's
        // files mid-run. The counter makes collisions impossible.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-mcp-{}-{}-{}-{}",
            "index",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn build_returns_real_index_not_unimplemented() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("hello.py"), "def greet():\n    return 'hi'\n").unwrap();
        let index = build(&dir).expect("index_repo should return a graph, not fail");
        assert!(
            index
                .graph()
                .files
                .iter()
                .any(|file| file.path.as_str().ends_with("hello.py")),
            "expected hello.py in the graph.\n  dir={}\n  dir_entries={:?}\n  files={:?}\n  scanned={} parsed={}\n  parse_failures={:?}\n  skipped_or_unresolved={:?}",
            dir.display(),
            std::fs::read_dir(&dir).map(|rd| rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()),
            index
                .graph()
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            index.report().files_scanned,
            index.report().files_parsed,
            index.report().parse_failures,
            index.report().unresolved_imports,
        );
        assert!(
            index.incremental().is_some(),
            "production build must retain an IncrementalIndex snapshot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_read_hits_then_misses_after_mtime_change() {
        use astrolabe_core::cache::{CacheConfig, FileCache};
        use std::time::Duration;

        let dir = unique_temp_dir();
        let file = dir.join("hello.py");
        std::fs::write(&file, "line-one\n").unwrap();
        let cache = FileCache::new(CacheConfig {
            budget_bytes: 64 * 1024,
            idle_ttl: Duration::from_secs(60),
        });

        let (lines, hit) = load_source_lines(&cache, &dir, "hello.py").unwrap();
        assert!(!hit, "first read must be a miss");
        assert_eq!(&*lines, &["line-one".to_string()]);

        let (lines, hit) = load_source_lines(&cache, &dir, "hello.py").unwrap();
        assert!(hit, "unchanged mtime must be a hit");
        assert_eq!(&*lines, &["line-one".to_string()]);

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&file, "line-two\n").unwrap();
        let (lines, hit) = load_source_lines(&cache, &dir, "hello.py").unwrap();
        assert!(!hit, "changed mtime must be a miss");
        assert_eq!(&*lines, &["line-two".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unindexed_code_exts_caches_in_graph_set() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("script.ex"), "puts 'hello'\n").unwrap();
        let index = build(&dir).expect("index build");
        assert!(index.in_graph_paths.get().is_none());

        let counts1 = index.unindexed_code_exts(None);
        assert_eq!(counts1.get("ex"), Some(&1));
        assert!(index.in_graph_paths.get().is_some());

        let cached_ptr = index.in_graph_paths.get().unwrap() as *const _;
        let counts2 = index.unindexed_code_exts(None);
        assert_eq!(counts2.get("ex"), Some(&1));
        let cached_ptr2 = index.in_graph_paths.get().unwrap() as *const _;
        assert_eq!(cached_ptr, cached_ptr2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
