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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::graph::CodeGraph;
use crate::parse::{ParsedFile, ParserPool};
use crate::resolvers::ResolverSet;
use crate::scan::{self, ScanOptions};
use crate::store::{self, Store};
use crate::types::{
    CodeEdge, CodeFile, CodeSymbol, Confidence, EdgeKind, FileId, Language, RelPath, SymbolId,
};
use crate::IndexReport;

/// Maximum number of syntactic call targets allowed for a single callee name.
/// Common identifiers exceeding this threshold (e.g. `new`, `get`, `default`, `clone`)
/// are skipped to prevent combinatorial edge explosion and OOM.
pub const MAX_SYNTACTIC_CALL_TARGETS: usize = 50;

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
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            call_edges: false,
            persist: true,
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

/// Build the graph for a repository.
///
/// Parsing runs in parallel; edge assembly is sequential and sorted so the
/// same bytes always produce the same graph.
pub fn index_repo(root: &Path, opts: &IndexOptions) -> (CodeGraph, IndexReport) {
    let mut report = IndexReport::default();

    let scanned = scan::scan(root, &opts.scan);
    let files = scan::to_index(root, &scanned);
    let resolvers = ResolverSet::detect(&files);

    // Build output can sit outside the default exclude list — Cargo's
    // `crates/*/target`, a tsconfig `outDir` like `packages/app/dist`. Only
    // `detect` knows about those, so the scan is filtered afterwards rather
    // than walked twice. Detection itself stays valid: dropping generated
    // files cannot invalidate the build config we already read.
    let declared_excludes: Vec<String> = [
        Language::Python,
        Language::Go,
        Language::Java,
        Language::Rust,
        Language::TypeScript,
    ]
    .iter()
    .filter_map(|lang| resolvers.meta_for(*lang))
    .flat_map(|meta| meta.excludes.iter().cloned())
    .collect();

    let (scanned, files) = if declared_excludes.is_empty() {
        (scanned, files)
    } else {
        let scanned = scan::apply_excludes(scanned, &declared_excludes);
        let files = scan::to_index(root, &scanned);
        (scanned, files)
    };

    report.files_scanned = scanned.files.len();
    let pool = ParserPool::new();
    let store = open_store(root, opts);

    // Parse every file that has a known language, in parallel. Failures are
    // collected, never swallowed. A cache hit skips tree-sitter; the result
    // is still sorted by path below so FileId does not depend on rayon order
    // or on which files hit the cache.
    let failures = Mutex::new(Vec::new());
    let cache_writes = Mutex::new(Vec::new());
    let cache_hits = AtomicUsize::new(0);
    let mut parsed: Vec<(RelPath, Language, String, u32, ParsedFile)> = scanned
        .files
        .par_iter()
        .filter_map(|path| {
            let lang = Language::from_path(path)?;
            let source = match std::fs::read_to_string(root.join(path.as_str())) {
                Ok(s) => s,
                Err(e) => {
                    failures
                        .lock()
                        .expect("failure list poisoned")
                        .push((path.clone(), format!("read: {e}")));
                    return None;
                }
            };
            let (sha, loc) = digest(&source);
            let cache_key = store::parse_cache_key(lang, &sha);

            if let Some(store) = store.as_ref() {
                match store.cached_parse(&cache_key) {
                    Ok(Some(bytes)) => match store::decode_parsed(&bytes) {
                        Ok(pf) => {
                            cache_hits.fetch_add(1, Ordering::Relaxed);
                            return Some((path.clone(), lang, sha, loc, pf));
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

            match pool.parse(lang, path, &source) {
                Ok(pf) => {
                    if store.is_some() {
                        match store::encode_parsed(&pf) {
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
                    Some((path.clone(), lang, sha, loc, pf))
                }
                Err(e) => {
                    failures
                        .lock()
                        .expect("failure list poisoned")
                        .push((path.clone(), e.to_string()));
                    None
                }
            }
        })
        .collect();

    // Parallel collection order is not stable; sort before assigning ids so
    // FileId is a deterministic function of the repo contents.
    parsed.sort_by(|a, b| a.0.cmp(&b.0));
    report.parse_failures = failures.into_inner().expect("failure list poisoned");
    report.parse_failures.sort();
    report.files_parsed = parsed.len();

    if let Some(store) = store.as_ref() {
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

    let mut graph = CodeGraph::default();
    let mut index_of = std::collections::HashMap::new();
    for (i, (path, lang, sha, loc, _)) in parsed.iter().enumerate() {
        let id = FileId(i as u32);
        index_of.insert(path.clone(), id);
        graph.files.push(CodeFile {
            id,
            path: path.clone(),
            language: Some(*lang),
            loc: *loc,
            sha: sha.clone(),
        });
    }

    let mut next_symbol = 0u32;
    for (path, _, _, _, pf) in &parsed {
        let file = index_of[path];
        for sym in &pf.symbols {
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
    for (path, _, _, _, pf) in &parsed {
        let from = index_of[path];
        for spec in &pf.imports {
            match resolvers.resolve(path, spec, &files) {
                Some(target) => {
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
                None => {
                    if looks_internal(spec) {
                        report.unresolved_imports.push((path.clone(), spec.clone()));
                    }
                }
            }
        }
    }

    if opts.call_edges {
        // Import edges join files; call edges join symbols. Both endpoints are
        // plain u32, so the distinction lives in `kind` — callers must read it
        // before interpreting `from`/`to`.
        let mut by_name: std::collections::HashMap<&str, Vec<SymbolId>> =
            std::collections::HashMap::new();
        for s in &graph.symbols {
            by_name.entry(s.name.as_str()).or_default().push(s.id);
        }
        let mut symbols_by_file: std::collections::HashMap<FileId, Vec<&CodeSymbol>> =
            std::collections::HashMap::new();
        for s in &graph.symbols {
            symbols_by_file.entry(s.file).or_default().push(s);
        }

        for (path, _, _, _, pf) in &parsed {
            let file = index_of[path];
            let in_file = symbols_by_file.get(&file);
            for (callee, line) in &pf.calls {
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
}
