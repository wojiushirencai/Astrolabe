//! Index orchestration: the one path that turns a directory into a graph.
//!
//! Wires together scan -> parse -> resolve -> graph. Every stage reports what
//! it could not do into [`IndexReport`] rather than dropping it, so a partial
//! index never looks complete.
//!
//! When [`IndexOptions::persist`] is on (the default), parse results are stored
//! under `<repo>/.astrolabe/` and reused on the next run if the content hash
//! still matches. A missing, read-only, or corrupt store is logged and the
//! index continues in memory — the cache is an optimisation, not a dependency.
//!
//! ## Incremental reindex
//!
//! A full [`index_repo`] always walks, reads, hashes, and (on cache miss)
//! tree-sitter-parses every admitted file. On a 94k-file tree that is
//! minutes of CPU even when almost nothing changed. [`IncrementalIndex`]
//! keeps the last parse products in a byte-capped LRU and applies a watcher
//! [`ChangeSet`]: only `added`/`modified` source files are read and parsed
//! (plus LRU evictions, which fall back to a re-parse of that one file).
//! The graph is then reassembled in memory.
//!
//! Route (b) — retain per-file products and reassemble — rather than true
//! graph-delta (route a): import resolution, FileId assignment, and
//! name-matched call edges are coupled across the whole file set, so
//! rewriting only the dirty files' edges is more complex than the sequential
//! assemble step, which on ~10^5 files is typically sub-second. Resolver
//! `detect` may still read a handful of build-config files (`go.mod`,
//! `tsconfig.json`, …); those are not the 94k-file read+hash+parse path.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use moka::sync::Cache;
use rayon::prelude::*;

use crate::graph::CodeGraph;
use crate::parse::{ParsedFile, ParserPool};
use crate::resolvers::ResolverSet;
use crate::scan::{self, ScanOptions, ScanResult};
use crate::store::{self, Store};
use crate::types::{
    CodeEdge, CodeFile, CodeSymbol, Confidence, EdgeKind, FileId, FileIndex, ImportResolution,
    Language, RelPath, SymbolId,
};
use crate::watch::ChangeSet;
use crate::IndexReport;

/// Maximum number of syntactic call targets allowed for a single callee name.
/// Common identifiers exceeding this threshold (e.g. `new`, `get`, `default`, `clone`)
/// are skipped to prevent combinatorial edge explosion and OOM.
pub const MAX_SYNTACTIC_CALL_TARGETS: usize = 50;

/// Default in-memory budget for retained parse products (encoded `ParsedFile`
/// bytes plus a small per-entry overhead). Oversized entries are still used
/// for the current assemble pass, but are not kept for the next one.
pub const DEFAULT_PARSE_CACHE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub scan: ScanOptions,
    /// Emit name-matched call edges. They carry `Confidence::Syntactic` and
    /// recall poorly (measured: 66% on TypeScript, 18% on Python), so callers
    /// that only need file-level impact should leave this off.
    pub call_edges: bool,
    /// Persist parse results under `<repo>/.astrolabe/` and reuse them on
    /// the next index when the content hash still matches.
    ///
    /// On by default so the MCP server — which constructs
    /// [`IndexOptions::default`] and is not part of this change — actually
    /// skips unchanged files after a restart. Persistence is an optimisation:
    /// tests and one-shot runs can set this to `false`.
    pub persist: bool,
    /// Byte budget for [`IncrementalIndex`]'s in-memory parse-product LRU.
    /// Unused by [`index_repo`]. Default [`DEFAULT_PARSE_CACHE_BYTES`].
    pub parse_cache_bytes: u64,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            call_edges: false,
            persist: true,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
        }
    }
}

/// On-disk parse cache for `root`. Gitignored via the workspace `.gitignore`.
pub fn store_path(root: &Path) -> PathBuf {
    root.join(".astrolabe").join("index.redb")
}

fn open_store(root: &Path, opts: &IndexOptions) -> Option<Store> {
    if !opts.persist {
        return None;
    }
    let dir = root.join(".astrolabe");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            error = %error,
            path = %dir.display(),
            "index persist disabled: cannot create store directory"
        );
        return None;
    }
    // The cache lives inside the repository being indexed, so it must not show
    // up in that repository's `git status`. A self-ignoring `.gitignore` makes
    // the whole directory invisible to git without asking the user to edit
    // their own ignore rules — the same trick pytest and ruff use for their
    // caches. Best effort: failing to write it costs cleanliness, not
    // correctness.
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "# Created by Astrolabe. Ignores itself.\n*\n");
    }
    let path = store_path(root);
    // Sibling processes cold-starting the same repository race for this
    // single-writer lock; retry before giving up persistence.
    match Store::open_or_heal_with_retry(
        &path,
        Store::DEFAULT_RETRY_ATTEMPTS,
        Store::DEFAULT_RETRY_DELAY,
    ) {
        Ok(store) => Some(store),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "index persist disabled: store unavailable"
            );
            None
        }
    }
}

/// Content hash and line count, computed once so the store can skip unchanged
/// files on reindex.
fn digest(source: &str) -> (String, u32) {
    // FNV-1a: not cryptographic, but stable across runs and platforms, which
    // is all a change-detection key needs.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in source.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    let loc = source.lines().count() as u32;
    (format!("{h:016x}"), loc)
}

struct ParsedRow {
    path: RelPath,
    language: Language,
    sha: String,
    loc: u32,
    parsed: ParsedFile,
}

#[derive(Clone)]
struct CachedProduct {
    language: Language,
    sha: String,
    loc: u32,
    encoded: Arc<[u8]>,
}

/// Byte-capped LRU of encoded [`ParsedFile`] products, keyed by repo-relative
/// path. Values are cheap to clone (`Arc` bytes). A new cache is filled on
/// each incremental apply so a retained previous [`IncrementalIndex`] is
/// not mutated.
struct ParseProductCache {
    inner: Cache<RelPath, CachedProduct>,
}

impl ParseProductCache {
    fn new(budget_bytes: u64) -> Self {
        let budget = budget_bytes.max(1);
        let inner = Cache::builder()
            .max_capacity(budget)
            .weigher(|path: &RelPath, value: &CachedProduct| -> u32 {
                let bytes = path
                    .as_str()
                    .len()
                    .saturating_add(value.sha.len())
                    .saturating_add(value.encoded.len())
                    .saturating_add(std::mem::size_of::<CachedProduct>());
                u32::try_from(bytes).unwrap_or(u32::MAX).max(1)
            })
            .build();
        Self { inner }
    }

    fn get(&self, path: &RelPath) -> Option<CachedProduct> {
        self.inner.get(path)
    }

    fn insert(&self, path: RelPath, product: CachedProduct) {
        self.inner.insert(path, product);
    }

    fn run_pending(&self) {
        self.inner.run_pending_tasks();
    }

    #[cfg(test)]
    fn weighted_size(&self) -> u64 {
        self.run_pending();
        self.inner.weighted_size()
    }
}

fn declared_excludes(resolvers: &ResolverSet) -> Vec<String> {
    [
        Language::Python,
        Language::Go,
        Language::Java,
        Language::Rust,
        Language::TypeScript,
        Language::Dart,
        Language::CSharp,
        Language::VisualBasic,
    ]
    .iter()
    .filter_map(|lang| resolvers.meta_for(*lang))
    .flat_map(|meta| meta.excludes.iter().cloned())
    .collect()
}

/// Scan + detect + resolver-declared excludes. Shared by the full and
/// incremental-capable first build.
fn discover(root: &Path, opts: &IndexOptions) -> (ScanResult, FileIndex, ResolverSet) {
    let scanned = scan::scan(root, &opts.scan);
    let files = scan::to_index(root, &scanned);
    let resolvers = ResolverSet::detect(&files);
    let (scanned, files) = apply_declared_excludes(root, scanned, &resolvers);
    (scanned, files, resolvers)
}

fn apply_declared_excludes(
    root: &Path,
    scanned: ScanResult,
    resolvers: &ResolverSet,
) -> (ScanResult, FileIndex) {
    // Build output can sit outside the default exclude list — Cargo's
    // `crates/*/target`, a tsconfig `outDir` like `packages/app/dist`. Only
    // `detect` knows about those, so the scan is filtered afterwards rather
    // than walked twice. Detection itself stays valid: dropping generated
    // files cannot invalidate the build config we already read.
    let excludes = declared_excludes(resolvers);
    if excludes.is_empty() {
        let files = scan::to_index(root, &scanned);
        (scanned, files)
    } else {
        let scanned = scan::apply_excludes(scanned, &excludes);
        let files = scan::to_index(root, &scanned);
        (scanned, files)
    }
}

fn parse_one(
    root: &Path,
    path: &RelPath,
    pool: &ParserPool,
    store: Option<&Store>,
    failures: &Mutex<Vec<(RelPath, String)>>,
    cache_writes: &Mutex<Vec<(String, Vec<u8>)>>,
    cache_hits: &AtomicUsize,
) -> Option<ParsedRow> {
    let language = Language::from_path(path)?;
    let source = match std::fs::read_to_string(root.join(path.as_str())) {
        Ok(source) => source,
        Err(error) => {
            failures
                .lock()
                .expect("failure list poisoned")
                .push((path.clone(), format!("read: {error}")));
            return None;
        }
    };
    let (sha, loc) = digest(&source);
    let cache_key = store::parse_cache_key(language, &sha);

    if let Some(store) = store {
        match store.cached_parse(&cache_key) {
            Ok(Some(bytes)) => match store::decode_parsed(&bytes) {
                Ok(parsed) => {
                    cache_hits.fetch_add(1, Ordering::Relaxed);
                    return Some(ParsedRow {
                        path: path.clone(),
                        language,
                        sha,
                        loc,
                        parsed,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.as_str(),
                        "parse cache entry ignored; re-parsing"
                    );
                }
            },
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    path = %path.as_str(),
                    "parse cache lookup failed; re-parsing"
                );
            }
        }
    }

    match pool.parse(language, path, &source) {
        Ok(parsed) => {
            if store.is_some() {
                match store::encode_parsed(&parsed) {
                    Ok(bytes) => cache_writes
                        .lock()
                        .expect("cache write list poisoned")
                        .push((cache_key, bytes)),
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            path = %path.as_str(),
                            "parse cache encode failed"
                        );
                    }
                }
            }
            Some(ParsedRow {
                path: path.clone(),
                language,
                sha,
                loc,
                parsed,
            })
        }
        Err(error) => {
            failures
                .lock()
                .expect("failure list poisoned")
                .push((path.clone(), error.to_string()));
            None
        }
    }
}

fn persist_cache_writes(
    root: &Path,
    store: Option<&Store>,
    cache_writes: Mutex<Vec<(String, Vec<u8>)>>,
    cache_hits: AtomicUsize,
) {
    let Some(store) = store else {
        return;
    };
    let writes = cache_writes
        .into_inner()
        .expect("cache write list poisoned");
    let stored = writes.len();
    if stored > 0 {
        if let Err(error) = store.put_cached_parses(
            writes
                .iter()
                .map(|(key, bytes)| (key.as_str(), bytes.as_slice())),
        ) {
            tracing::warn!(
                error = %error,
                "parse cache write failed; index is still valid"
            );
        }
    }
    tracing::info!(
        hits = cache_hits.load(Ordering::Relaxed),
        stored,
        path = %store_path(root).display(),
        "parse cache"
    );
}

/// Parse every path that has a known language. Failures are collected, never
/// swallowed. A disk-store cache hit skips tree-sitter; FileId assignment
/// still happens later after a path sort, so rayon order does not leak.
fn parse_paths(root: &Path, paths: &[RelPath], opts: &IndexOptions) -> ParseOutcome {
    let pool = ParserPool::new();
    let store = open_store(root, opts);
    let failures = Mutex::new(Vec::new());
    let cache_writes = Mutex::new(Vec::new());
    let cache_hits = AtomicUsize::new(0);
    let parsed: Vec<ParsedRow> = paths
        .par_iter()
        .filter_map(|path| {
            parse_one(
                root,
                path,
                &pool,
                store.as_ref(),
                &failures,
                &cache_writes,
                &cache_hits,
            )
        })
        .collect();
    persist_cache_writes(root, store.as_ref(), cache_writes, cache_hits);
    let mut failures = failures.into_inner().expect("failure list poisoned");
    failures.sort();
    ParseOutcome { parsed, failures }
}

struct ParseOutcome {
    parsed: Vec<ParsedRow>,
    failures: Vec<(RelPath, String)>,
}

fn insert_product(cache: &ParseProductCache, row: &ParsedRow) {
    match store::encode_parsed(&row.parsed) {
        Ok(bytes) => cache.insert(
            row.path.clone(),
            CachedProduct {
                language: row.language,
                sha: row.sha.clone(),
                loc: row.loc,
                encoded: Arc::from(bytes),
            },
        ),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %row.path.as_str(),
                "parse product encode failed; file will be re-parsed on next incremental apply"
            );
        }
    }
}

fn fill_products(rows: &[ParsedRow], budget_bytes: u64) -> ParseProductCache {
    let products = ParseProductCache::new(budget_bytes);
    for row in rows {
        insert_product(&products, row);
    }
    products.run_pending();
    products
}

fn row_from_product(
    path: RelPath,
    product: &CachedProduct,
) -> Result<ParsedRow, store::StoreError> {
    let parsed = store::decode_parsed(&product.encoded)?;
    Ok(ParsedRow {
        path,
        language: product.language,
        sha: product.sha.clone(),
        loc: product.loc,
        parsed,
    })
}

/// Parallel collection order is not stable; sort before assigning ids so
/// FileId is a deterministic function of the repo contents.
fn assemble_graph(
    mut parsed: Vec<ParsedRow>,
    files: &FileIndex,
    resolvers: &ResolverSet,
    opts: &IndexOptions,
    mut report: IndexReport,
) -> (CodeGraph, IndexReport) {
    parsed.sort_by(|a, b| a.path.cmp(&b.path));
    report.files_parsed = parsed.len();

    let mut graph = CodeGraph::default();
    let mut index_of = HashMap::new();
    for (i, row) in parsed.iter().enumerate() {
        let id = FileId(i as u32);
        index_of.insert(row.path.clone(), id);
        graph.files.push(CodeFile {
            id,
            path: row.path.clone(),
            language: Some(row.language),
            loc: row.loc,
            sha: row.sha.clone(),
        });
    }

    let mut next_symbol = 0u32;
    for row in &parsed {
        let file = index_of[&row.path];
        for sym in &row.parsed.symbols {
            graph.symbols.push(CodeSymbol {
                id: SymbolId(next_symbol),
                file,
                ..sym.clone()
            });
            next_symbol += 1;
        }
    }
    report.symbols = graph.symbols.len();

    // Import edges. An unresolved specifier is only worth reporting when it
    // looks like it points inside the repo — stdlib and third-party imports
    // resolving to None is the correct answer, not a gap.
    let mut edges = Vec::new();
    for row in &parsed {
        let from = index_of[&row.path];
        for spec in &row.parsed.imports {
            match resolvers.resolve_import(&row.path, spec, files) {
                ImportResolution::File(target) => {
                    if let Some(&to) = index_of.get(&target) {
                        edges.push(CodeEdge {
                            from: from.0,
                            to: to.0,
                            kind: EdgeKind::Import,
                            weight: 1,
                            // Backed by build configuration, not by a compiler.
                            confidence: Confidence::Scoped,
                        });
                    }
                }
                ImportResolution::Namespace { files: targets, .. } => {
                    // Every declaring file, partials included. Not an Import:
                    // PageRank stays on real file edges.
                    for target in targets {
                        if let Some(&to) = index_of.get(&target) {
                            edges.push(CodeEdge {
                                from: from.0,
                                to: to.0,
                                kind: EdgeKind::Namespace,
                                weight: 1,
                                confidence: Confidence::Scoped,
                            });
                        }
                    }
                }
                ImportResolution::Unresolved => {
                    if looks_internal(spec) {
                        report
                            .unresolved_imports
                            .push((row.path.clone(), spec.clone()));
                    }
                }
            }
        }
    }

    // ProjectReference is a file-level import even when no source file
    // repeats it as a using. Endpoints that were not parsed are skipped;
    // the relationship still lives on ProjectMeta::project_refs.
    for (_lang, meta) in resolvers.iter_meta() {
        for pref in &meta.project_refs {
            let from_path = RelPath::new(&pref.from);
            let to_path = RelPath::new(&pref.to);
            if let (Some(&from), Some(&to)) = (index_of.get(&from_path), index_of.get(&to_path)) {
                if from != to {
                    edges.push(CodeEdge {
                        from: from.0,
                        to: to.0,
                        kind: EdgeKind::Import,
                        weight: 1,
                        confidence: Confidence::Scoped,
                    });
                }
            }
        }
    }

    if opts.call_edges {
        // Import edges join files; call edges join symbols. Both endpoints are
        // plain u32, so the distinction lives in `kind` — callers must read it
        // before interpreting `from`/`to`.
        let mut by_name: HashMap<&str, Vec<SymbolId>> = HashMap::new();
        for s in &graph.symbols {
            by_name.entry(s.name.as_str()).or_default().push(s.id);
        }
        let mut symbols_by_file: HashMap<FileId, Vec<&CodeSymbol>> = HashMap::new();
        for s in &graph.symbols {
            symbols_by_file.entry(s.file).or_default().push(s);
        }

        for row in &parsed {
            let file = index_of[&row.path];
            let in_file = symbols_by_file.get(&file);
            for (callee, line) in &row.parsed.calls {
                // A call belongs to the smallest symbol spanning its line;
                // attributing it to the file would collapse every caller in
                // the file into one node.
                let Some(caller) = in_file.and_then(|syms| {
                    syms.iter()
                        .filter(|s| s.start_line <= *line && *line <= s.end_line)
                        .min_by_key(|s| s.end_line - s.start_line)
                }) else {
                    continue;
                };
                // Take the trailing segment so `a.b.c()` matches `c`.
                let short = callee
                    .rsplit(['.', ':'])
                    .find(|p| !p.is_empty())
                    .unwrap_or(callee);
                let Some(targets) = by_name.get(short) else {
                    continue;
                };
                if targets.len() > MAX_SYNTACTIC_CALL_TARGETS {
                    continue;
                }
                for target in targets {
                    if *target == caller.id {
                        continue;
                    }
                    edges.push(CodeEdge {
                        from: caller.id.0,
                        to: target.0,
                        kind: EdgeKind::Call,
                        weight: 1,
                        // Name matching only, with no scope or type
                        // information. Measured recall against a language
                        // server: 66% on TypeScript, 18% on Python. Labelled
                        // so callers can decide whether to verify.
                        confidence: Confidence::Syntactic,
                    });
                }
            }
        }
    }

    // Collapse duplicates into weights, with a stable order.
    edges.sort_by_key(|e| (e.from, e.to, e.kind as u8));
    let mut merged: Vec<CodeEdge> = Vec::with_capacity(edges.len());
    for e in edges {
        match merged.last_mut() {
            Some(prev) if prev.from == e.from && prev.to == e.to && prev.kind == e.kind => {
                prev.weight += 1;
            }
            _ => merged.push(e),
        }
    }
    report.import_edges = merged.iter().filter(|e| e.kind == EdgeKind::Import).count();
    graph.edges = merged;
    report.unresolved_imports.sort();

    (graph, report)
}

/// Build the graph for a repository.
///
/// Parsing runs in parallel; edge assembly is sequential and sorted so the
/// same bytes always produce the same graph.
///
/// One-shot callers that will not apply a later [`ChangeSet`] can use this
/// and drop the parse products. MCP (and anything else that reindexes on
/// watcher events) should keep an [`IncrementalIndex`] instead.
pub fn index_repo(root: &Path, opts: &IndexOptions) -> (CodeGraph, IndexReport) {
    let (scanned, files, resolvers) = discover(root, opts);
    let mut report = IndexReport {
        files_scanned: scanned.files.len(),
        ..IndexReport::default()
    };
    let outcome = parse_paths(root, &scanned.files, opts);
    report.parse_failures = outcome.failures;
    assemble_graph(outcome.parsed, &files, &resolvers, opts, report)
}

/// Snapshot of a previous index that can absorb a watcher [`ChangeSet`]
/// without re-reading unchanged source files.
///
/// Route (b): encoded parse products live in a byte-capped LRU; each apply
/// reuses hits, re-parses `added`/`modified` (and LRU misses), then
/// reassembles the graph. True graph-delta (route a) is deferred because
/// import resolution, FileId assignment, and name-matched call edges are
/// coupled across the whole file set.
///
/// MCP integration:
///
/// ```ignore
/// let mut index = IncrementalIndex::build(root, &opts);
/// // serve index.graph / index.report
/// // on ChangeSet from the watcher:
/// index = index_repo_incremental(root, &index, &changes, &opts);
/// ```
pub struct IncrementalIndex {
    pub graph: CodeGraph,
    pub report: IndexReport,
    /// Admitted paths after resolver excludes, including non-source files
    /// (`go.mod`, `Cargo.toml`, …) that resolvers need in [`FileIndex`].
    files: Vec<RelPath>,
    products: ParseProductCache,
    /// Parse failures for files not re-read on the next apply.
    failures: Vec<(RelPath, String)>,
}

impl std::fmt::Debug for IncrementalIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalIndex")
            .field("files", &self.files.len())
            .field("graph_files", &self.graph.files.len())
            .field("symbols", &self.graph.symbols.len())
            .finish_non_exhaustive()
    }
}

impl IncrementalIndex {
    /// Full index, retaining parse products for later [`Self::apply_changeset`].
    pub fn build(root: &Path, opts: &IndexOptions) -> Self {
        let (scanned, files, resolvers) = discover(root, opts);
        let mut report = IndexReport {
            files_scanned: scanned.files.len(),
            ..IndexReport::default()
        };
        let outcome = parse_paths(root, &scanned.files, opts);
        let failures = outcome.failures.clone();
        report.parse_failures = outcome.failures;
        let products = fill_products(&outcome.parsed, opts.parse_cache_bytes);
        let (graph, report) = assemble_graph(outcome.parsed, &files, &resolvers, opts, report);
        Self {
            graph,
            report,
            files: scanned.files,
            products,
            failures,
        }
    }

    /// Apply a watcher changeset. Only `added`/`modified` source files are
    /// read and tree-sitter parsed, plus LRU evictions (explicit fallback).
    ///
    /// Resolver `detect` may still read a handful of build-config files;
    /// those are not the 94k-file source parse path. Newly excluded paths
    /// are dropped in memory; newly un-excluded paths require an `added`
    /// event (or a full [`Self::build`]).
    pub fn apply_changeset(&self, root: &Path, changes: &ChangeSet, opts: &IndexOptions) -> Self {
        index_repo_incremental(root, self, changes, opts)
    }

    /// Admitted paths after resolver excludes, including non-source files
    /// (`go.mod`, `Cargo.toml`, …) that resolvers need in [`FileIndex`].
    pub fn admitted_paths(&self) -> &[RelPath] {
        &self.files
    }

    #[cfg(test)]
    fn product_bytes(&self) -> u64 {
        self.products.weighted_size()
    }
}

/// Incremental rebuild from a previous [`IncrementalIndex`] and a watcher
/// [`ChangeSet`]. The MCP server will pass `(root, changes)` and keep the
/// returned snapshot; `previous` is not mutated.
pub fn index_repo_incremental(
    root: &Path,
    previous: &IncrementalIndex,
    changes: &ChangeSet,
    opts: &IndexOptions,
) -> IncrementalIndex {
    let mut files: BTreeSet<RelPath> = previous.files.iter().cloned().collect();
    for path in &changes.removed {
        files.remove(path);
    }
    for path in &changes.added {
        files.insert(path.clone());
    }
    // A modified path missing from the previous set is treated as added.
    for path in &changes.modified {
        files.insert(path.clone());
    }

    let mut dirty: HashSet<RelPath> = HashSet::new();
    for path in changes.added.iter().chain(&changes.modified) {
        if files.contains(path) {
            dirty.insert(path.clone());
        }
    }

    let scanned = ScanResult {
        files: files.into_iter().collect(),
        skipped: Vec::new(),
    };
    let file_index = scan::to_index(root, &scanned);
    let resolvers = ResolverSet::detect(&file_index);
    let (scanned, file_index) = apply_declared_excludes(root, scanned, &resolvers);

    let mut report = IndexReport {
        files_scanned: scanned.files.len(),
        ..IndexReport::default()
    };

    let mut to_parse = Vec::new();
    let mut reused = Vec::new();
    let mut reused_products = Vec::new();
    let mut failures = Vec::new();
    let mut lru_misses = 0usize;

    for path in &scanned.files {
        if dirty.contains(path) {
            to_parse.push(path.clone());
            continue;
        }
        if Language::from_path(path).is_none() {
            continue;
        }
        if let Some(product) = previous.products.get(path) {
            match row_from_product(path.clone(), &product) {
                Ok(row) => {
                    reused_products.push((path.clone(), product));
                    reused.push(row);
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.as_str(),
                        "in-memory parse product ignored; re-parsing"
                    );
                    lru_misses += 1;
                    to_parse.push(path.clone());
                }
            }
        } else if let Some((_, reason)) =
            previous.failures.iter().find(|(failed, _)| failed == path)
        {
            // Unchanged previous failure: do not re-read.
            failures.push((path.clone(), reason.clone()));
        } else {
            lru_misses += 1;
            to_parse.push(path.clone());
        }
    }

    let reused_count = reused.len();
    let outcome = parse_paths(root, &to_parse, opts);
    let parsed_count = outcome.parsed.len();
    failures.extend(outcome.failures);
    failures.sort();
    report.parse_failures = failures.clone();

    tracing::info!(
        added = changes.added.len(),
        modified = changes.modified.len(),
        removed = changes.removed.len(),
        reused = reused_count,
        parsed = parsed_count,
        lru_misses,
        "incremental index"
    );

    let products = ParseProductCache::new(opts.parse_cache_bytes);
    for (path, product) in reused_products {
        products.insert(path, product);
    }
    for row in &outcome.parsed {
        insert_product(&products, row);
    }
    products.run_pending();

    reused.extend(outcome.parsed);
    let (graph, report) = assemble_graph(reused, &file_index, &resolvers, opts, report);
    IncrementalIndex {
        graph,
        report,
        files: scanned.files,
        products,
        failures,
    }
}

/// Code file extensions across known programming languages (46 pure programming language extensions).
///
/// 收窄说明：这是"未支持编程语言"的统计口径，纯编程语言源码扩展名；
/// 数据/配置文件（json, jsonc, yaml, yml, toml, html, css, graphql, gql, sql, sh, bash, ps1, tf, tfvars, hcl 等）
/// 不在此列，避免把数据/配置文件提示为"尚未支持的语言"从而稀释信号；
/// Serena 58 项全集的 read-deny 用途与统计用途分离。
pub const KNOWN_CODE_EXTENSIONS: &[&str] = &[
    "al", "c", "clj", "cljs", "cpp", "cs", "cshtml", "csx", "dart", "elm", "ex", "exs", "fs",
    "fsx", "go", "groovy", "h", "hpp", "hs", "java", "jl", "js", "jsx", "kt", "kts", "lean", "lua",
    "m", "matlab", "nf", "php", "proto", "py", "r", "razor", "rb", "rs", "scala", "sol", "svelte",
    "swift", "ts", "tsx", "vb", "vue", "zig",
];

/// Count occurrences of unindexed code file extensions among admitted paths.
///
/// - `filter`: when Some, paths not containing this substring are skipped (same contains semantics as `path_filter`).
/// - Skips paths where `in_graph(path.as_str())` is true.
/// - Skips paths whose language is already supported (`Language::from_path(path).is_some()`).
/// - Skips paths without an extension.
/// - Gated by vocabulary: only extensions in `KNOWN_CODE_EXTENSIONS` are counted.
/// - Extensions are normalized to lowercase.
pub fn unindexed_code_exts(
    admitted: &[RelPath],
    in_graph: &dyn Fn(&str) -> bool,
    filter: Option<&str>,
) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for p in admitted {
        let s = p.as_str();
        if let Some(f) = filter {
            if !s.contains(f) {
                continue;
            }
        }
        if in_graph(s) {
            continue;
        }
        if Language::from_path(p).is_some() {
            continue;
        }
        let Some(ext) = p.extension() else {
            continue;
        };
        let ext_lower = ext.to_ascii_lowercase();
        if KNOWN_CODE_EXTENSIONS.contains(&ext_lower.as_str()) {
            *counts.entry(ext_lower).or_insert(0) += 1;
        }
    }
    counts
}

/// Heuristic for "this specifier was meant to resolve inside the repo".
/// Deliberately conservative: a false negative just omits a diagnostic, while
/// a false positive would make the resolution-rate metric meaningless.
fn looks_internal(spec: &str) -> bool {
    spec.starts_with('.') || spec.starts_with("crate::") || spec.starts_with("self::")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, Store};

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "astrolabe-index-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn indexes_a_repo_end_to_end() {
        let root = scratch("e2e");
        write(&root, "go.mod", "module example.com/app\n");
        write(
            &root,
            "main.go",
            "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/app/util\"\n)\n\nfunc main() { fmt.Println(util.Greet()) }\n",
        );
        write(
            &root,
            "util/util.go",
            "package util\n\nfunc Greet() string { return \"hi\" }\n",
        );

        let (graph, report) = index_repo(&root, &IndexOptions::default());

        assert_eq!(report.files_parsed, 2, "both .go files parse");
        assert!(
            report.parse_failures.is_empty(),
            "{:?}",
            report.parse_failures
        );
        assert!(
            graph.symbols.iter().any(|s| s.name == "Greet"),
            "symbols extracted: {:?}",
            graph.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(report.import_edges, 1, "the in-repo import became an edge");
        assert!(
            report.unresolved_imports.is_empty(),
            "stdlib fmt must not count as a gap: {:?}",
            report.unresolved_imports
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn index_is_deterministic() {
        let root = scratch("det");
        write(&root, "go.mod", "module example.com/d\n");
        for n in 0..12 {
            write(
                &root,
                &format!("pkg{n}/a.go"),
                &format!("package pkg{n}\n\nfunc F{n}() {{}}\n"),
            );
        }
        write(
            &root,
            "main.go",
            "package main\n\nimport (\n\t\"example.com/d/pkg1\"\n\t\"example.com/d/pkg2\"\n)\n\nfunc main() { pkg1.F1(); pkg2.F2() }\n",
        );

        let (g1, _) = index_repo(&root, &IndexOptions::default());
        let (g2, _) = index_repo(&root, &IndexOptions::default());

        let paths = |g: &CodeGraph| {
            g.files
                .iter()
                .map(|f| f.path.to_string())
                .collect::<Vec<_>>()
        };
        let edges = |g: &CodeGraph| {
            g.edges
                .iter()
                .map(|e| (e.from, e.to, e.kind as u8, e.weight))
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&g1), paths(&g2), "file ids must be reproducible");
        assert_eq!(edges(&g1), edges(&g2), "edges must be reproducible");

        std::fs::remove_dir_all(&root).ok();
    }

    /// End-to-end over the five real corpora. This is the number that matters:
    /// not "the resolver works in isolation" but "the whole pipeline produces
    /// a graph with no silent gaps".
    #[test]
    #[ignore = "needs the real corpora; run with --ignored"]
    fn real_corpora_produce_complete_graphs() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let cases = [
            ("Go", ws.join("corpus/go")),
            ("Java", ws.join("corpus/java")),
            ("Rust", ws.join("corpus/rust")),
            ("Python", ws.join("../serena")),
            ("TypeScript", ws.join("../openvisio-oss")),
        ];

        println!(
            "\n{:<11} {:>7} {:>7} {:>8} {:>8} {:>9} {:>8}",
            "语言", "扫描", "解析", "符号", "导入边", "解析失败", "未解析"
        );
        println!("{}", "-".repeat(64));
        let mut worst_fail = 0usize;
        for (lang, root) in &cases {
            if !root.exists() {
                println!("{lang:<11} (语料缺失，跳过)");
                continue;
            }
            let t = std::time::Instant::now();
            let (g, r) = index_repo(root, &IndexOptions::default());
            println!(
                "{:<11} {:>7} {:>7} {:>8} {:>8} {:>9} {:>8}  {:.1}s",
                lang,
                r.files_scanned,
                r.files_parsed,
                g.symbols.len(),
                r.import_edges,
                r.parse_failures.len(),
                r.unresolved_imports.len(),
                t.elapsed().as_secs_f64()
            );
            for (p, why) in r.parse_failures.iter().take(3) {
                println!("      解析失败: {p} — {why}");
            }
            for (p, s) in r.unresolved_imports.iter().take(3) {
                println!("      未解析:   {p} — {s}");
            }
            worst_fail = worst_fail.max(r.parse_failures.len());
        }
        assert_eq!(
            worst_fail, 0,
            "任何语料上出现解析失败都要先查清楚，不能当作正常"
        );
    }

    #[test]
    fn parse_failures_are_reported_not_swallowed() {
        let root = scratch("fail");
        // Valid UTF-8 but not valid Go; tree-sitter recovers, so this should
        // still parse. The point is that whatever happens, nothing is silent.
        write(&root, "broken.go", "package main\nfunc ( { { {\n");
        let (_, report) = index_repo(&root, &IndexOptions::default());
        assert_eq!(
            report.files_scanned, 1,
            "the file was discovered regardless of its contents"
        );
        assert_eq!(
            report.files_parsed + report.parse_failures.len(),
            1,
            "every scanned source file is either parsed or reported"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn persist_can_be_disabled() {
        let root = scratch("no-persist");
        write(&root, "main.go", "package main\nfunc main() {}\n");
        let opts = IndexOptions {
            persist: false,
            ..IndexOptions::default()
        };
        let (_, report) = index_repo(&root, &opts);
        assert_eq!(report.files_parsed, 1);
        assert!(
            !root.join(".astrolabe").exists(),
            "disabled persist must not create a store"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unwritable_store_does_not_fail_the_index() {
        let root = scratch("readonly-store");
        write(&root, "main.go", "package main\nfunc Hello() {}\n");
        // Occupy the store directory name so create_dir_all / Database::create
        // cannot succeed. The index must still return a graph.
        std::fs::write(root.join(".astrolabe"), b"not a directory").unwrap();
        let (graph, report) = index_repo(&root, &IndexOptions::default());
        assert_eq!(report.files_parsed, 1, "parse must not depend on the store");
        assert!(
            report.parse_failures.is_empty(),
            "{:?}",
            report.parse_failures
        );
        assert_eq!(graph.files.len(), 1);
        assert!(
            graph.symbols.iter().any(|s| s.name == "Hello"),
            "symbols extracted without a store: {:?}",
            graph.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_store_directory_does_not_fail_the_index() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("unix-ro-store");
        write(&root, "main.go", "package main\nfunc main() {}\n");
        let dir = root.join(".astrolabe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let (graph, report) = index_repo(&root, &IndexOptions::default());
        assert_eq!(report.files_parsed, 1);
        assert!(report.parse_failures.is_empty());
        assert_eq!(graph.files.len(), 1);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn persist_writes_parse_cache_keyed_by_sha() {
        let root = scratch("cache-write");
        write(&root, "lib.go", "package lib\nfunc F() {}\n");
        let (graph, _) = index_repo(&root, &IndexOptions::default());
        let file = &graph.files[0];
        let store = Store::open(&store_path(&root)).expect("store created on first index");
        let key = store::parse_cache_key(file.language.unwrap(), &file.sha);
        assert!(
            store.cached_parse(&key).unwrap().is_some(),
            "freshly parsed file must land in the parse cache"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cached_second_index_matches_cold_index() {
        let root = scratch("cache-hit");
        write(&root, "go.mod", "module example.com/c\n");
        write(&root, "a.go", "package c\nfunc A() {}\n");
        write(&root, "b.go", "package c\nfunc B() { A() }\n");

        let (g1, r1) = index_repo(&root, &IndexOptions::default());
        let (g2, r2) = index_repo(&root, &IndexOptions::default());

        assert_eq!(r1.files_parsed, r2.files_parsed);
        assert_eq!(
            g1.files
                .iter()
                .map(|f| (f.path.to_string(), f.sha.clone(), f.id.0))
                .collect::<Vec<_>>(),
            g2.files
                .iter()
                .map(|f| (f.path.to_string(), f.sha.clone(), f.id.0))
                .collect::<Vec<_>>(),
            "cache hits must not shuffle FileId assignment"
        );
        let symbols = |g: &CodeGraph| {
            g.symbols
                .iter()
                .map(|s| (s.id.0, s.file.0, s.name.clone(), s.start_line, s.end_line))
                .collect::<Vec<_>>()
        };
        assert_eq!(symbols(&g1), symbols(&g2));
        let edges = |g: &CodeGraph| {
            g.edges
                .iter()
                .map(|e| (e.from, e.to, e.kind as u8, e.weight))
                .collect::<Vec<_>>()
        };
        assert_eq!(edges(&g1), edges(&g2));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn call_edges_skips_ubiquitous_names_exceeding_threshold() {
        let root = scratch("ubiquitous");
        write(&root, "go.mod", "module example.com/ubiq\n");
        // Create files defining more than MAX_SYNTACTIC_CALL_TARGETS functions named "New"
        let mut defs = String::from("package ubiq\n");
        for n in 0..=(MAX_SYNTACTIC_CALL_TARGETS + 5) {
            defs.push_str(&format!("func New{n}() {{}}\n"));
        }
        write(&root, "defs.go", &defs);

        // Many functions with the exact same name "Init"
        let mut inits = String::from("package ubiq\n");
        for n in 0..=(MAX_SYNTACTIC_CALL_TARGETS + 5) {
            inits.push_str(&format!(
                "type S{n} struct {{}}\nfunc (s S{n}) Init() {{}}\n"
            ));
        }
        write(&root, "inits.go", &inits);

        // Caller calling Init() and Rare()
        write(
            &root,
            "caller.go",
            "package ubiq\nfunc Rare() {}\nfunc Call() { Init(); Rare(); }\n",
        );

        let opts = IndexOptions {
            call_edges: true,
            persist: false,
            ..IndexOptions::default()
        };
        let (g, _) = index_repo(&root, &opts);

        let call_edges: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Call)
            .collect();

        // Target for "Init" has > MAX_SYNTACTIC_CALL_TARGETS symbols, so it must be skipped.
        // But "Rare" has only 1 target, so its call edge should be generated.
        let rare_sym = g.symbols.iter().find(|s| s.name == "Rare").unwrap();
        assert!(
            call_edges.iter().any(|e| e.to == rare_sym.id.0),
            "Rare call edge must exist"
        );

        let init_sym_ids: std::collections::HashSet<u32> = g
            .symbols
            .iter()
            .filter(|s| s.name == "Init")
            .map(|s| s.id.0)
            .collect();
        assert_eq!(init_sym_ids.len(), MAX_SYNTACTIC_CALL_TARGETS + 6);
        assert!(
            !call_edges.iter().any(|e| init_sym_ids.contains(&e.to)),
            "Ubiquitous Init calls must be skipped"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    fn no_persist() -> IndexOptions {
        IndexOptions {
            persist: false,
            ..IndexOptions::default()
        }
    }

    fn with_calls() -> IndexOptions {
        IndexOptions {
            persist: false,
            call_edges: true,
            ..IndexOptions::default()
        }
    }

    type FileSnap = (u32, String, Option<Language>, u32, String);
    type SymbolSnap = (u32, u32, String, u8, String, u32, u32, bool);
    type EdgeSnap = (u32, u32, u8, u32, u8);

    fn graph_snapshot(g: &CodeGraph) -> (Vec<FileSnap>, Vec<SymbolSnap>, Vec<EdgeSnap>) {
        let files = g
            .files
            .iter()
            .map(|f| (f.id.0, f.path.to_string(), f.language, f.loc, f.sha.clone()))
            .collect();
        let symbols = g
            .symbols
            .iter()
            .map(|s| {
                (
                    s.id.0,
                    s.file.0,
                    s.name.clone(),
                    s.kind as u8,
                    s.signature.clone(),
                    s.start_line,
                    s.end_line,
                    s.exported,
                )
            })
            .collect();
        let edges = g
            .edges
            .iter()
            .map(|e| (e.from, e.to, e.kind as u8, e.weight, e.confidence as u8))
            .collect();
        (files, symbols, edges)
    }

    fn assert_graphs_equivalent(a: &CodeGraph, b: &CodeGraph) {
        assert_eq!(graph_snapshot(a), graph_snapshot(b));
    }

    fn seed_go_repo(root: &Path) {
        write(root, "go.mod", "module example.com/inc\n");
        write(
            root,
            "a.go",
            "package inc\n\nfunc A() {}\nfunc Shared() {}\n",
        );
        write(root, "b.go", "package inc\n\nfunc B() { A(); Shared() }\n");
        write(root, "c.go", "package inc\n\nfunc C() { B() }\n");
        write(
            root,
            "util/u.go",
            "package util\n\nfunc U() string { return \"u\" }\n",
        );
    }

    #[test]
    fn incremental_build_matches_index_repo() {
        let root = scratch("inc-build");
        seed_go_repo(&root);
        let opts = with_calls();
        let (g1, r1) = index_repo(&root, &opts);
        let inc = IncrementalIndex::build(&root, &opts);
        assert_graphs_equivalent(&g1, &inc.graph);
        assert_eq!(r1.files_parsed, inc.report.files_parsed);
        assert_eq!(r1.import_edges, inc.report.import_edges);
        assert_eq!(r1.symbols, inc.report.symbols);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn empty_changeset_reuses_products_and_matches_full() {
        let root = scratch("inc-empty");
        seed_go_repo(&root);
        let opts = with_calls();
        let inc = IncrementalIndex::build(&root, &opts);
        let next = inc.apply_changeset(&root, &ChangeSet::default(), &opts);
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&inc.graph, &next.graph);
        assert_graphs_equivalent(&full, &next.graph);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn incremental_add_modify_remove_match_full_index() {
        let root = scratch("inc-amr");
        seed_go_repo(&root);
        let opts = with_calls();
        let mut inc = IncrementalIndex::build(&root, &opts);

        write(&root, "d.go", "package inc\n\nfunc D() { A() }\n");
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                added: vec![RelPath::new("d.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);

        write(
            &root,
            "a.go",
            "package inc\n\nimport \"example.com/inc/util\"\n\nfunc A() { util.U() }\nfunc Shared() {}\n",
        );
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("a.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);
        assert!(
            inc.graph.edges.iter().any(|e| e.kind == EdgeKind::Import),
            "import from a.go to util should resolve"
        );

        std::fs::remove_file(root.join("c.go")).unwrap();
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                removed: vec![RelPath::new("c.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);
        assert!(
            inc.graph.files.iter().all(|f| f.path.as_str() != "c.go"),
            "removed file must leave the graph"
        );

        // Adding a path that sorts between existing FileIds must remap ids
        // the same way a full index does.
        write(&root, "ab.go", "package inc\n\nfunc AB() {}\n");
        inc = inc.apply_changeset(
            &root,
            &ChangeSet {
                added: vec![RelPath::new("ab.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let (full, _) = index_repo(&root, &opts);
        assert_graphs_equivalent(&full, &inc.graph);

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn incremental_does_not_reread_unchanged_source_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("inc-noread");
        seed_go_repo(&root);
        let opts = no_persist();
        let inc = IncrementalIndex::build(&root, &opts);

        let a = root.join("a.go");
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000)).unwrap();

        write(
            &root,
            "b.go",
            "package inc\n\nfunc B() { Shared() }\nfunc B2() {}\n",
        );
        let next = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("b.go")],
                ..ChangeSet::default()
            },
            &opts,
        );

        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            next.graph.files.iter().any(|f| f.path.as_str() == "a.go"),
            "unchanged a.go must be reused from the parse-product cache, not re-read"
        );
        assert!(
            next.graph.symbols.iter().any(|s| s.name == "A"),
            "symbols from the unreadable-on-disk file must survive"
        );
        assert!(
            next.graph.symbols.iter().any(|s| s.name == "B2"),
            "modified b.go must be re-parsed"
        );
        assert!(
            next.report.parse_failures.is_empty(),
            "re-reading a.go would have failed: {:?}",
            next.report.parse_failures
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn incremental_keeps_stale_product_when_changeset_omits_a_write() {
        let root = scratch("inc-stale");
        seed_go_repo(&root);
        let opts = no_persist();
        let inc = IncrementalIndex::build(&root, &opts);
        let old_sha = inc
            .graph
            .files
            .iter()
            .find(|f| f.path.as_str() == "a.go")
            .unwrap()
            .sha
            .clone();

        write(&root, "a.go", "package inc\n\nfunc ARenamed() {}\n");
        write(
            &root,
            "b.go",
            "package inc\n\nfunc B() {}\nfunc Extra() {}\n",
        );
        let next = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("b.go")],
                ..ChangeSet::default()
            },
            &opts,
        );
        let a = next
            .graph
            .files
            .iter()
            .find(|f| f.path.as_str() == "a.go")
            .unwrap();
        assert_eq!(
            a.sha, old_sha,
            "a.go was not in the changeset, so the cached product must win"
        );
        assert!(next.graph.symbols.iter().any(|s| s.name == "A"));
        assert!(!next.graph.symbols.iter().any(|s| s.name == "ARenamed"));
        assert!(next.graph.symbols.iter().any(|s| s.name == "Extra"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn lru_eviction_falls_back_to_reparse_and_stays_equivalent() {
        let root = scratch("inc-lru");
        write(&root, "go.mod", "module example.com/lru\n");
        for n in 0..12 {
            write(
                &root,
                &format!("f{n}.go"),
                &format!("package lru\n\nfunc F{n}() {{}}\n"),
            );
        }
        let tiny = IndexOptions {
            persist: false,
            call_edges: true,
            parse_cache_bytes: 64,
            ..IndexOptions::default()
        };
        let inc = IncrementalIndex::build(&root, &tiny);
        write(
            &root,
            "f0.go",
            "package lru\n\nfunc F0() {}\nfunc Extra() {}\n",
        );
        let next = inc.apply_changeset(
            &root,
            &ChangeSet {
                modified: vec![RelPath::new("f0.go")],
                ..ChangeSet::default()
            },
            &tiny,
        );
        let (full, _) = index_repo(&root, &tiny);
        assert_graphs_equivalent(&full, &next.graph);
        // moka admits an entry even when its weight exceeds max_capacity, then
        // evicts asynchronously. After run_pending the cache must not grow with
        // the file count — 12 files at an unbounded cache would be far larger.
        assert!(
            next.product_bytes() < 8 * 1024,
            "LRU must stay bounded, got {}",
            next.product_bytes()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Property: fixture → full incremental build → random add/mod/del →
    /// apply_changeset ≡ a fresh [`index_repo`] of the same tree.
    /// FileIds, symbols, and edges must match (determinism of assemble).
    #[test]
    fn incremental_matches_full_index_after_random_edits() {
        for seed in 1u64..=20 {
            incremental_random_edits_once(seed, false);
            incremental_random_edits_once(seed.wrapping_mul(0x9e37), true);
        }
    }

    fn incremental_random_edits_once(seed: u64, call_edges: bool) {
        let root = scratch(&format!("inc-prop-{seed}-{call_edges}"));
        seed_go_repo(&root);
        let opts = IndexOptions {
            persist: false,
            call_edges,
            ..IndexOptions::default()
        };
        let mut inc = IncrementalIndex::build(&root, &opts);
        let mut rng = seed;
        let mut extra: Vec<String> = Vec::new();
        let mut next_id = 0u32;

        for step in 0..12 {
            rng = xorshift(rng);
            let op = (rng % 3) as u8;
            let changes = match op {
                0 => {
                    let name = format!("gen{next_id}.go");
                    next_id += 1;
                    write(
                        &root,
                        &name,
                        &format!("package inc\n\nfunc Gen{next_id}() {{ A(); Shared() }}\n"),
                    );
                    extra.push(name.clone());
                    ChangeSet {
                        added: vec![RelPath::new(name)],
                        ..ChangeSet::default()
                    }
                }
                1 => {
                    let target = if extra.is_empty() || rng.is_multiple_of(2) {
                        "b.go".to_string()
                    } else {
                        extra[(rng as usize) % extra.len()].clone()
                    };
                    write(
                        &root,
                        &target,
                        &format!(
                            "package inc\n\nfunc Step{step}() {{}}\nfunc SharedStep{step}() {{ Step{step}() }}\n"
                        ),
                    );
                    ChangeSet {
                        modified: vec![RelPath::new(target)],
                        ..ChangeSet::default()
                    }
                }
                _ => {
                    if extra.len() < 2 {
                        write(
                            &root,
                            "c.go",
                            &format!("package inc\n\nfunc C{step}() {{}}\n"),
                        );
                        ChangeSet {
                            modified: vec![RelPath::new("c.go")],
                            ..ChangeSet::default()
                        }
                    } else {
                        let idx = (rng as usize) % extra.len();
                        let name = extra.remove(idx);
                        std::fs::remove_file(root.join(&name)).unwrap();
                        ChangeSet {
                            removed: vec![RelPath::new(name)],
                            ..ChangeSet::default()
                        }
                    }
                }
            };

            inc = inc.apply_changeset(&root, &changes, &opts);
            let (full, full_report) = index_repo(&root, &opts);
            assert_graphs_equivalent(&full, &inc.graph);
            assert_eq!(
                full_report.files_parsed, inc.report.files_parsed,
                "seed={seed} step={step} call_edges={call_edges}"
            );
            assert_eq!(full_report.import_edges, inc.report.import_edges);
            assert_eq!(full_report.symbols, inc.report.symbols);
        }

        std::fs::remove_dir_all(&root).ok();
    }

    fn xorshift(mut x: u64) -> u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }

    #[test]
    fn test_unindexed_code_exts_basic() {
        let admitted = vec![
            RelPath::new("src/main.rs"),     // supported (Rust) -> skipped
            RelPath::new("scripts/run.rb"),  // not supported, in KNOWN -> counted
            RelPath::new("scripts/test.rb"), // not supported, in KNOWN -> counted
            RelPath::new("src/App.kt"),      // not supported, in KNOWN -> counted
            RelPath::new("data/info.json"),  // data file, removed from KNOWN -> skipped
            RelPath::new("docs/readme.txt"), // txt not in KNOWN -> skipped
            RelPath::new("LICENSE"),         // no ext -> skipped
            RelPath::new("src/lib.rs"),      // in_graph -> skipped
        ];

        let in_graph = |p: &str| p == "src/lib.rs";
        let counts = unindexed_code_exts(&admitted, &in_graph, None);
        assert_eq!(counts.get("rb"), Some(&2));
        assert_eq!(counts.get("kt"), Some(&1)); // .kt 仍被统计
        assert_eq!(counts.get("json"), None); // .json 数据文件不再被统计
        assert_eq!(counts.get("rs"), None);
        assert_eq!(counts.get("txt"), None);
    }

    #[test]
    fn test_unindexed_code_exts_with_filter() {
        let admitted = vec![
            RelPath::new("backend/script.lua"),
            RelPath::new("frontend/script.lua"),
            RelPath::new("backend/service.scala"),
        ];
        let in_graph = |_p: &str| false;
        let counts = unindexed_code_exts(&admitted, &in_graph, Some("backend"));
        assert_eq!(counts.get("lua"), Some(&1));
        assert_eq!(counts.get("scala"), Some(&1));

        let counts_fe = unindexed_code_exts(&admitted, &in_graph, Some("frontend"));
        assert_eq!(counts_fe.get("lua"), Some(&1));
        assert_eq!(counts_fe.get("scala"), None);
    }

    #[test]
    fn test_unindexed_code_exts_case_insensitive_and_vocabulary() {
        let admitted = vec![
            RelPath::new("build/deploy.KT"),
            RelPath::new("model.SCALA"),
            RelPath::new("unknown.xyz123"),
        ];
        let in_graph = |_p: &str| false;
        let counts = unindexed_code_exts(&admitted, &in_graph, None);
        assert_eq!(counts.get("kt"), Some(&1));
        assert_eq!(counts.get("scala"), Some(&1));
        assert_eq!(counts.get("xyz123"), None);
    }

    #[test]
    fn csx_is_classified_not_unindexed() {
        let admitted = vec![
            RelPath::new("scripts/main.csx"),
            RelPath::new("lib/bin/Hidden.csx"),
        ];
        let in_graph = |_p: &str| false;
        let counts = unindexed_code_exts(&admitted, &in_graph, None);
        assert!(
            !counts.contains_key("csx"),
            ".csx is C#, not an unsupported extension: {counts:?}"
        );
        assert_eq!(
            Language::from_path(&RelPath::new("scripts/main.csx")),
            Some(Language::CSharp)
        );
    }
}
