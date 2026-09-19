//! Repository index held by the server.
//!
//! The build itself lives in `astrolabe_core::index` — this module only adds
//! the lookups the tool layer needs. Keeping one orchestration path means the
//! numbers the server reports are the same ones the acceptance suite measures.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};

use astrolabe_core::cache::FileCache;
use astrolabe_core::graph::CodeGraph;
use astrolabe_core::index::{index_repo, IndexOptions};
use astrolabe_core::{CodeFile, FileId, IndexReport};

#[derive(Debug)]
pub(crate) struct RepoIndex {
    pub root: PathBuf,
    pub graph: CodeGraph,
    pub report: IndexReport,
}

impl RepoIndex {
    pub fn file(&self, id: FileId) -> Option<&CodeFile> {
        self.graph.files.iter().find(|file| file.id == id)
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
            .graph
            .files
            .iter()
            .find(|file| file.path.as_str() == target)
        {
            return Some(file.id);
        }
        let suffix = format!("/{target}");
        self.graph
            .files
            .iter()
            .filter(|file| file.path.as_str().ends_with(&suffix))
            .min_by_key(|file| file.path.as_str().len())
            .map(|file| file.id)
    }
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
    let mtime_ms = file_mtime_ms(&disk).unwrap_or(0);
    if let Some(lines) = cache.get(rel_path, mtime_ms) {
        return Some((lines, true));
    }
    let source = fs::read_to_string(&disk).ok()?;
    let lines: Vec<String> = source.lines().map(str::to_owned).collect();
    cache.insert(rel_path, lines.clone(), mtime_ms);
    Some((Arc::new(lines), false))
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

pub(crate) fn build(root: &Path) -> anyhow::Result<RepoIndex> {
    let opts = IndexOptions {
        call_edges: true,
        ..IndexOptions::default()
    };
    let (graph, report) = index_repo(root, &opts);
    Ok(RepoIndex {
        root: root.to_path_buf(),
        graph,
        report,
    })
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
                .graph
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
                .graph
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            index.report.files_scanned,
            index.report.files_parsed,
            index.report.parse_failures,
            index.report.unresolved_imports,
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
}
