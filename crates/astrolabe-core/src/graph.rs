//! The code graph: import edges, centrality, and task-relevant ranking.
//!
//! Determinism is a feature. Same repo bytes must give the same graph and the
//! same ordering, every run, with no LLM and no embeddings in the loop. Use
//! fixed iteration counts and stable tie-breaks (path, then line) rather than
//! convergence thresholds or hash-map iteration order.
//!
//! Two ranking layers, kept separate so the expensive one stays cacheable:
//!   * **Static centrality** — PageRank over import edges. Cache it; it only
//!     changes when the graph does.
//!   * **Task personalization** — a restart vector derived from the task text,
//!     applied on top. Worth borrowing from aider's repo map, which weights
//!     identifiers named in the conversation ~10x, long specific identifiers
//!     ~10x, and files already in context ~50x.
//!
//! Import edges carry `Confidence::Scoped` (build-config-backed). Call edges
//! carry `Confidence::Syntactic` and must be labelled as such: measured
//! against a language server, name-matched call graphs recalled 66% on
//! TypeScript and 18% on Python. File-level import edges recalled 100%, which
//! is why file-level questions route here and symbol-level ones do not.

use crate::types::{CodeEdge, CodeFile, CodeSymbol, EdgeKind, FileId};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const PAGERANK_DAMPING: f64 = 0.85;
pub const PAGERANK_ITERATIONS: usize = 40;

#[derive(Debug, Default)]
pub struct CodeGraph {
    pub files: Vec<CodeFile>,
    pub symbols: Vec<CodeSymbol>,
    pub edges: Vec<CodeEdge>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Centrality {
    pub by_file: HashMap<FileId, f64>,
}

/// PageRank over file import edges (`importer -> imported`).
///
/// Files and edges are canonicalised before any floating-point addition. This
/// is deliberately sequential: changing the summation order changes the last
/// bits of a score and can consequently change a ranking.
pub fn compute_centrality(g: &CodeGraph) -> Centrality {
    let files = sorted_files(g);
    let n = files.len();
    if n == 0 {
        return Centrality::default();
    }

    let positions = file_positions(&files);
    let outgoing = import_adjacency(g, &positions, None);
    let mut scores = vec![1.0 / n as f64; n];
    let restart = scores.clone();
    run_pagerank(&outgoing, &restart, &mut scores);

    Centrality {
        by_file: files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.id, scores[i]))
            .collect(),
    }
}

/// Add task-specific restart and edge weights without modifying cached static
/// centrality. A file path mentioned in `task` is treated as already being in
/// context, because the public API intentionally has no separate context list.
pub fn rank_for_task(g: &CodeGraph, task: &str, base: &Centrality) -> Centrality {
    let files = sorted_files(g);
    let n = files.len();
    if n == 0 {
        return Centrality::default();
    }

    let task_words = split_words(task);
    let task_compact: String = compact_lower(task);
    let hits = symbol_hits(g, &task_words, &task_compact);
    rank_with_hits(g, task, base, &task_words, &task_compact, &hits)
}

/// `rank_for_task` and `match_task` in one pass over the symbols.
pub fn rank_and_match(g: &CodeGraph, task: &str, base: &Centrality) -> (Centrality, TaskMatch) {
    let task_words = split_words(task);
    let task_compact = compact_lower(task);
    let hits = symbol_hits(g, &task_words, &task_compact);
    let ranked = rank_with_hits(g, task, base, &task_words, &task_compact, &hits);
    let mut m = match_task_files(g, task, &task_words, &task_compact);
    for h in &hits {
        if (h.explicit && h.compact_len >= 4) || h.split {
            m.symbols.insert(h.idx);
        }
    }
    (ranked, m)
}

/// One symbol that matches the task (explicit with min len 3, or split).
struct SymHit {
    idx: usize,
    compact_len: usize,
    explicit: bool,
    split: bool,
}

fn symbol_hits(g: &CodeGraph, task_words: &BTreeSet<String>, task_compact: &str) -> Vec<SymHit> {
    let mut buf = String::new();
    let mut hits = Vec::new();
    for (idx, symbol) in g.symbols.iter().enumerate() {
        buf.clear();
        buf.extend(
            symbol
                .name
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase),
        );
        let (explicit, split) = symbol_matches(&symbol.name, &buf, task_words, task_compact, 3);
        if explicit || split {
            hits.push(SymHit {
                idx,
                compact_len: buf.len(),
                explicit,
                split,
            });
        }
    }
    hits
}

fn rank_with_hits(
    g: &CodeGraph,
    task: &str,
    base: &Centrality,
    task_words: &BTreeSet<String>,
    task_compact: &str,
    hits: &[SymHit],
) -> Centrality {
    let files = sorted_files(g);
    let n = files.len();
    if n == 0 {
        return Centrality::default();
    }
    let mut boosts = vec![1.0; n];
    let positions = file_positions(&files);
    // Only matching symbols touch `boosts`; sort just those, in the same
    // (file, line, name, id) order as before.
    let mut matched: Vec<(&CodeSymbol, usize)> = hits
        .iter()
        .map(|h| (&g.symbols[h.idx], h.compact_len))
        .collect();
    matched.sort_by(|(a, _), (b, _)| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    for (symbol, compact_len) in matched {
        let Some(&i) = positions.get(&symbol.file) else {
            continue;
        };
        boosts[i] *= 10.0;
        if compact_len >= 12
            || split_words(&symbol.name)
                .iter()
                .filter(|word| word.len() >= 3)
                .count()
                >= 3
        {
            boosts[i] *= 10.0;
        }
    }

    let task_lower = task.to_lowercase();
    let context_files: BTreeSet<FileId> = files
        .iter()
        .filter(|file| file_mentioned(&task_lower, file))
        .map(|file| file.id)
        .collect();
    // A file whose name words all appear in the task (`loop_guard.rs` for
    // "loop guard") is task-relevant even when the task never types the
    // extension. Weaker than an explicit path (10x, not 50x).
    for file in &files {
        if context_files.contains(&file.id) || !stem_matches(file, task_words, task_compact) {
            continue;
        }
        if let Some(&i) = positions.get(&file.id) {
            boosts[i] *= 10.0;
        }
    }
    for file in &context_files {
        if let Some(&i) = positions.get(file) {
            // Giving the source more restart mass makes its 50x outgoing
            // emphasis observable even when all of its import edges have the
            // same weight (where ordinary PageRank normalization would
            // otherwise cancel a uniform multiplier).
            boosts[i] *= 50.0;
        }
    }

    let mut restart: Vec<f64> = files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            base.by_file
                .get(&file.id)
                .copied()
                .unwrap_or(1.0 / n as f64)
                * boosts[i]
        })
        .collect();
    normalize(&mut restart);

    let outgoing = import_adjacency(g, &positions, Some(&context_files));
    let mut scores = restart.clone();
    run_pagerank(&outgoing, &restart, &mut scores);

    Centrality {
        by_file: files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.id, scores[i]))
            .collect(),
    }
}

/// Files that import `target`, ranked. This is the query that replaces a
/// language server for file-level impact analysis.
pub fn dependents(g: &CodeGraph, target: FileId) -> Vec<FileId> {
    reachable_imports(g, target, true)
}

pub fn dependencies(g: &CodeGraph, target: FileId) -> Vec<FileId> {
    reachable_imports(g, target, false)
}

fn sorted_files(g: &CodeGraph) -> Vec<&CodeFile> {
    let mut files: Vec<&CodeFile> = g.files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
    files
}

fn file_positions(files: &[&CodeFile]) -> BTreeMap<FileId, usize> {
    files
        .iter()
        .enumerate()
        .map(|(position, file)| (file.id, position))
        .collect()
}

fn import_adjacency(
    g: &CodeGraph,
    positions: &BTreeMap<FileId, usize>,
    context_files: Option<&BTreeSet<FileId>>,
) -> Vec<Vec<(usize, f64)>> {
    let mut aggregated: BTreeMap<(usize, usize), u64> = BTreeMap::new();
    // Namespace fan-out and name-matched calls are not real file references.
    for edge in g.edges.iter().filter(|edge| edge.kind == EdgeKind::Import) {
        let (from, to) = (FileId(edge.from), FileId(edge.to));
        let (Some(&from_i), Some(&to_i)) = (positions.get(&from), positions.get(&to)) else {
            continue;
        };
        let context_multiplier = if context_files.is_some_and(|ids| ids.contains(&from)) {
            50_u64
        } else {
            1
        };
        let weight = u64::from(edge.weight.max(1)).saturating_mul(context_multiplier);
        let slot = aggregated.entry((from_i, to_i)).or_default();
        *slot = slot.saturating_add(weight);
    }

    let mut outgoing = vec![Vec::new(); positions.len()];
    for ((from, to), weight) in aggregated {
        outgoing[from].push((to, weight as f64));
    }
    outgoing
}

fn run_pagerank(outgoing: &[Vec<(usize, f64)>], restart: &[f64], scores: &mut Vec<f64>) {
    let n = scores.len();
    // Edge weights do not change between iterations; same summation order as
    // before, so the scores are bit-identical.
    let total_weights: Vec<f64> = outgoing
        .iter()
        .map(|edges| edges.iter().map(|(_, weight)| *weight).sum())
        .collect();
    for _ in 0..PAGERANK_ITERATIONS {
        let dangling: f64 = (0..n)
            .filter(|&i| outgoing[i].is_empty())
            .map(|i| scores[i])
            .sum();
        let mut next: Vec<f64> = restart
            .iter()
            .map(|&value| (1.0 - PAGERANK_DAMPING) * value)
            .collect();

        for from in 0..n {
            if outgoing[from].is_empty() {
                continue;
            }
            let total_weight = total_weights[from];
            for &(to, weight) in &outgoing[from] {
                next[to] += PAGERANK_DAMPING * scores[from] * weight / total_weight;
            }
        }
        for i in 0..n {
            next[i] += PAGERANK_DAMPING * dangling * restart[i];
        }
        *scores = next;
    }
}

fn normalize(values: &mut [f64]) {
    let sum: f64 = values.iter().sum();
    if sum > 0.0 && sum.is_finite() {
        for value in values {
            *value /= sum;
        }
    }
}

fn compact_lower(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// `(explicit, split)` name-vs-task matches for one symbol. `explicit` means
/// the whole compacted name (>= `min_explicit_len` chars) occurs in the task;
/// `split` means the name's words occur among the task's words.
fn symbol_matches(
    name: &str,
    compact: &str,
    task_words: &BTreeSet<String>,
    task_compact: &str,
    min_explicit_len: usize,
) -> (bool, bool) {
    let explicit_match = compact.len() >= min_explicit_len && task_compact.contains(compact);
    // A split match needs at least one task word (>= 3 bytes) inside the
    // name; every name word is a contiguous run of `compact`. Skipping the
    // allocation-heavy `split_words` for the vast majority of symbols keeps
    // resolve_context fast on large graphs. ASCII names only, so lowercase
    // folding is identical.
    if compact.is_ascii()
        && !task_words
            .iter()
            .any(|w| w.len() >= 3 && compact.contains(w.as_str()))
    {
        return (explicit_match, false);
    }
    let words = split_words(name);
    let meaningful: Vec<&String> = words.iter().filter(|word| word.len() >= 3).collect();
    let split_match = !meaningful.is_empty()
        && meaningful
            .iter()
            .filter(|word| task_words.contains((*word).as_str()))
            .count()
            >= meaningful.len().min(2);
    (explicit_match, split_match)
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// `needle` occurs in `haystack` delimited by non-path characters on both
/// sides (so `a.rs` is not "mentioned" by `data.rs`). Both lowercase.
pub fn mentions_path(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_path_char(c));
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_path_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn file_mentioned(task_lower: &str, file: &CodeFile) -> bool {
    let path = file.path.as_str().to_lowercase();
    let name = file.path.file_name().to_lowercase();
    mentions_path(task_lower, &path) || mentions_path(task_lower, &name)
}

fn stem_matches(file: &CodeFile, task_words: &BTreeSet<String>, task_compact: &str) -> bool {
    let name = file.path.file_name();
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let compact = compact_lower(stem);
    if compact.len() < 4 {
        return false;
    }
    let words = split_words(stem);
    let meaningful: Vec<&String> = words.iter().filter(|w| w.len() >= 3).collect();
    let words_hit =
        !meaningful.is_empty() && meaningful.iter().all(|w| task_words.contains(w.as_str()));
    words_hit || (compact.len() >= 6 && task_compact.contains(&compact))
}

/// Files and symbols that the task text actually names (strict predicates:
/// path/name boundary match, file-stem words, whole-name symbol hits). Empty
/// result means the task matched nothing and any ranking is pure centrality.
#[derive(Clone, Debug, Default)]
pub struct TaskMatch {
    pub files: BTreeSet<FileId>,
    /// Indexes into `CodeGraph::symbols`.
    pub symbols: BTreeSet<usize>,
}

impl TaskMatch {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.symbols.is_empty()
    }
}

fn match_task_files(
    g: &CodeGraph,
    task: &str,
    task_words: &BTreeSet<String>,
    task_compact: &str,
) -> TaskMatch {
    let task_lower = task.to_lowercase();
    let mut out = TaskMatch::default();
    for file in &g.files {
        if file_mentioned(&task_lower, file) || stem_matches(file, task_words, task_compact) {
            out.files.insert(file.id);
        }
    }
    out
}

pub fn match_task(g: &CodeGraph, task: &str) -> TaskMatch {
    let task_words = split_words(task);
    let task_compact = compact_lower(task);
    let mut m = match_task_files(g, task, &task_words, &task_compact);
    for h in symbol_hits(g, &task_words, &task_compact) {
        if (h.explicit && h.compact_len >= 4) || h.split {
            m.symbols.insert(h.idx);
        }
    }
    m
}

fn split_words(text: &str) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                words.insert(current.to_lowercase());
                current.clear();
            }
            continue;
        }
        let previous = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
        let next = chars.get(i + 1).copied();
        let camel_boundary =
            ch.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_numeric());
        let acronym_boundary = ch.is_uppercase()
            && previous.is_some_and(char::is_uppercase)
            && next.is_some_and(char::is_lowercase);
        if (camel_boundary || acronym_boundary) && !current.is_empty() {
            words.insert(current.to_lowercase());
            current.clear();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.insert(current.to_lowercase());
    }
    words
}

/// Fingerprint of everything `compute_centrality` reads.
fn centrality_fingerprint(g: &CodeGraph) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    g.files.len().hash(&mut h);
    for f in &g.files {
        f.id.hash(&mut h);
        f.path.as_str().hash(&mut h);
    }
    for e in g.edges.iter().filter(|e| e.kind == EdgeKind::Import) {
        (e.from, e.to, e.weight).hash(&mut h);
    }
    h.finish()
}

/// PageRank is the expensive part of `dependents`/`dependencies`; the answer
/// only changes when files or import edges do, so keep the last few results
/// keyed by a fingerprint of those inputs instead of recomputing per call.
pub fn cached_centrality(g: &CodeGraph) -> std::sync::Arc<Centrality> {
    use std::sync::{Arc, Mutex};
    static CACHE: Mutex<Vec<(u64, Arc<Centrality>)>> = Mutex::new(Vec::new());
    let key = centrality_fingerprint(g);
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, c)) = cache.iter().find(|(k, _)| *k == key) {
            return Arc::clone(c);
        }
    }
    let computed = Arc::new(compute_centrality(g));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 4 {
        cache.remove(0);
    }
    cache.push((key, Arc::clone(&computed)));
    computed
}

fn reachable_imports(g: &CodeGraph, target: FileId, reverse: bool) -> Vec<FileId> {
    let known: BTreeSet<FileId> = g.files.iter().map(|file| file.id).collect();
    if !known.contains(&target) {
        return Vec::new();
    }

    let mut adjacency: BTreeMap<FileId, BTreeSet<FileId>> = BTreeMap::new();
    // Namespace fan-out and name-matched calls are not real file references.
    for edge in g.edges.iter().filter(|edge| edge.kind == EdgeKind::Import) {
        let (from, to) = (FileId(edge.from), FileId(edge.to));
        if !known.contains(&from) || !known.contains(&to) {
            continue;
        }
        let (source, destination) = if reverse { (to, from) } else { (from, to) };
        adjacency.entry(source).or_default().insert(destination);
    }

    let mut visited = BTreeSet::new();
    let mut frontier = vec![target];
    while let Some(current) = frontier.pop() {
        if let Some(next) = adjacency.get(&current) {
            // Reverse push order so the smallest ID is visited first. The
            // final score sort is authoritative, but traversal is stable too.
            for &file in next.iter().rev() {
                if file != target && visited.insert(file) {
                    frontier.push(file);
                }
            }
        }
    }

    let centrality = cached_centrality(g);
    let files_by_id: BTreeMap<FileId, &CodeFile> =
        g.files.iter().map(|file| (file.id, file)).collect();
    let first_line: BTreeMap<FileId, u32> =
        g.symbols.iter().fold(BTreeMap::new(), |mut lines, symbol| {
            lines
                .entry(symbol.file)
                .and_modify(|line| *line = (*line).min(symbol.start_line))
                .or_insert(symbol.start_line);
            lines
        });
    let mut result: Vec<FileId> = visited.into_iter().collect();
    result.sort_by(|a, b| {
        let score_a = centrality.by_file.get(a).copied().unwrap_or(0.0);
        let score_b = centrality.by_file.get(b).copied().unwrap_or(0.0);
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                files_by_id
                    .get(a)
                    .map(|f| &f.path)
                    .cmp(&files_by_id.get(b).map(|f| &f.path))
            })
            .then_with(|| {
                first_line
                    .get(a)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&first_line.get(b).copied().unwrap_or(0))
            })
            .then_with(|| a.cmp(b))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CodeEdge, CodeFile, CodeSymbol, Confidence, Language, RelPath, SymbolId, SymbolKind,
    };

    fn file(id: u32, path: &str) -> CodeFile {
        CodeFile {
            id: FileId(id),
            path: RelPath::new(path),
            language: Some(Language::Rust),
            loc: 10,
            sha: String::new(),
        }
    }

    fn import(from: u32, to: u32) -> CodeEdge {
        CodeEdge {
            from,
            to,
            kind: EdgeKind::Import,
            weight: 1,
            confidence: Confidence::Scoped,
        }
    }

    fn graph(edges: Vec<CodeEdge>) -> CodeGraph {
        CodeGraph {
            files: vec![file(2, "c.rs"), file(0, "a.rs"), file(1, "b.rs")],
            symbols: Vec::new(),
            edges,
        }
    }

    fn ranked(centrality: &Centrality) -> Vec<FileId> {
        let mut values: Vec<(FileId, f64)> = centrality
            .by_file
            .iter()
            .map(|(&id, &score)| (id, score))
            .collect();
        values.sort_by(|(a_id, a), (b_id, b)| {
            b.partial_cmp(a)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a_id.cmp(b_id))
        });
        values.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn pagerank_orders_a_three_node_chain() {
        let centrality = compute_centrality(&graph(vec![import(0, 1), import(1, 2)]));
        assert_eq!(ranked(&centrality), vec![FileId(2), FileId(1), FileId(0)]);
    }

    #[test]
    fn pagerank_ignores_namespace_fanout_and_name_matched_calls() {
        let imports_only = graph(vec![import(0, 1)]);
        let mixed = graph(vec![
            import(0, 1),
            CodeEdge {
                from: 0,
                to: 2,
                kind: EdgeKind::Namespace,
                weight: 4,
                confidence: Confidence::Scoped,
            },
            CodeEdge {
                from: 1,
                to: 0,
                kind: EdgeKind::Call,
                weight: 9,
                confidence: Confidence::Syntactic,
            },
        ]);
        assert_eq!(
            compute_centrality(&imports_only),
            compute_centrality(&mixed)
        );
        assert_eq!(
            dependencies(&mixed, FileId(0)),
            dependencies(&imports_only, FileId(0))
        );
        assert!(
            !dependencies(&mixed, FileId(0)).contains(&FileId(2)),
            "namespace edges are not file dependencies"
        );
        assert!(
            dependents(&mixed, FileId(0)).is_empty(),
            "a name-matched call into a file is not a dependent"
        );
    }

    #[test]
    fn pagerank_is_exactly_deterministic() {
        let g = graph(vec![import(1, 2), import(0, 1)]);
        let first = compute_centrality(&g);
        let second = compute_centrality(&g);
        assert_eq!(first, second);
        assert_eq!(ranked(&first), ranked(&second));
    }

    #[test]
    fn dangling_nodes_are_finite_and_do_not_leak_mass() {
        let centrality = compute_centrality(&graph(vec![import(0, 1)]));
        let sum: f64 = centrality.by_file.values().sum();
        assert!(centrality.by_file.values().all(|score| score.is_finite()));
        assert!((sum - 1.0).abs() < 1e-12, "PageRank mass was {sum}");
    }

    #[test]
    fn cycles_terminate_and_queries_exclude_the_target() {
        let g = graph(vec![import(0, 1), import(1, 2), import(2, 0)]);
        assert_eq!(dependencies(&g, FileId(0)).len(), 2);
        assert!(!dependencies(&g, FileId(0)).contains(&FileId(0)));
    }

    #[test]
    fn dependency_directions_are_correct_and_transitive() {
        let g = graph(vec![import(0, 1), import(1, 2)]);
        let deps = dependencies(&g, FileId(0));
        assert!(deps.contains(&FileId(1)));
        assert!(deps.contains(&FileId(2)));
        let users = dependents(&g, FileId(2));
        assert!(users.contains(&FileId(0)));
        assert!(users.contains(&FileId(1)));
        assert!(dependents(&g, FileId(0)).is_empty());
        assert!(dependencies(&g, FileId(2)).is_empty());
    }

    #[test]
    fn tokenizer_splits_camel_case_snake_case_and_acronyms() {
        assert_eq!(
            split_words("getUserName parse_http_URL"),
            ["get", "http", "name", "parse", "url", "user"]
                .map(str::to_string)
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn personalization_promotes_matching_file_without_mutating_base() {
        let mut g = graph(vec![]);
        g.symbols = vec![
            CodeSymbol {
                id: SymbolId(0),
                file: FileId(0),
                name: "helper".into(),
                kind: SymbolKind::Function,
                signature: String::new(),
                start_line: 1,
                end_line: 1,
                exported: false,
            },
            CodeSymbol {
                id: SymbolId(1),
                file: FileId(2),
                name: "processStripeWebhook".into(),
                kind: SymbolKind::Function,
                signature: String::new(),
                start_line: 7,
                end_line: 8,
                exported: true,
            },
        ];
        let base = compute_centrality(&g);
        let snapshot = base.clone();
        let first = rank_for_task(&g, "fix the stripe webhook processing", &base);
        let second = rank_for_task(&g, "fix the stripe webhook processing", &base);

        assert_eq!(ranked(&first)[0], FileId(2));
        assert_eq!(first, second);
        assert_eq!(base, snapshot);
    }

    #[test]
    fn mentions_path_requires_boundaries() {
        assert!(mentions_path("edit a.rs now", "a.rs"));
        assert!(mentions_path("see src/a.rs.", "src/a.rs"));
        assert!(mentions_path("see x/src/a.rs", "src/a.rs"));
        assert!(!mentions_path("edit data.rs now", "a.rs"));
        assert!(!mentions_path("edit a.rs2", "a.rs"));
        assert!(!mentions_path("edit my-a.rs", "a.rs"));
        assert!(mentions_path("data.rs then a.rs", "a.rs"));
    }

    #[test]
    fn substring_file_name_is_not_in_context() {
        // `data.rs` contains `a.rs`; a task naming only data.rs must not pull
        // a.rs into the 50x context set.
        let g = CodeGraph {
            files: vec![file(0, "a.rs"), file(1, "data.rs"), file(2, "z.rs")],
            symbols: Vec::new(),
            edges: vec![import(0, 2), import(1, 2)],
        };
        let m = match_task(&g, "please edit data.rs");
        assert_eq!(m.files, BTreeSet::from([FileId(1)]));
        let m = match_task(&g, "please edit a.rs");
        assert_eq!(m.files, BTreeSet::from([FileId(0)]));
    }

    #[test]
    fn file_stem_words_match_task_without_extension() {
        let g = CodeGraph {
            files: vec![file(0, "src/loop_guard.rs"), file(1, "src/server.rs")],
            symbols: Vec::new(),
            edges: Vec::new(),
        };
        let m = match_task(&g, "where does the loop guard block repeats");
        assert!(m.files.contains(&FileId(0)));
        assert!(!m.files.contains(&FileId(1)));
        let base = compute_centrality(&g);
        let r = rank_for_task(&g, "where does the loop guard block repeats", &base);
        assert_eq!(ranked(&r)[0], FileId(0));
    }

    #[test]
    fn gibberish_task_matches_nothing() {
        let g = graph(vec![import(0, 1)]);
        assert!(match_task(&g, "zzzz qqqq nonexistent thing").is_empty());
    }

    #[test]
    fn cached_dependents_equal_fresh_and_track_graph_changes() {
        let g1 = graph(vec![import(0, 1), import(2, 1)]);
        let first = dependents(&g1, FileId(1));
        assert_eq!(first, dependents(&g1, FileId(1)));
        let g2 = graph(vec![import(0, 1)]);
        assert_eq!(dependents(&g2, FileId(1)), vec![FileId(0)]);
        assert_eq!(first.len(), 2);
    }

    #[test]
    fn symbol_matches_prefilter_keeps_results() {
        // Reference: the unfiltered definition.
        fn reference(name: &str, tw: &BTreeSet<String>, tc: &str, min: usize) -> (bool, bool) {
            let words = split_words(name);
            let meaningful: Vec<&String> = words.iter().filter(|w| w.len() >= 3).collect();
            let split = !meaningful.is_empty()
                && meaningful
                    .iter()
                    .filter(|w| tw.contains((*w).as_str()))
                    .count()
                    >= meaningful.len().min(2);
            let c = compact_lower(name);
            (c.len() >= min && tc.contains(&c), split)
        }
        let task = "how does the JsonSerializer handle errors in redis cluster";
        let tw = split_words(task);
        let tc = compact_lower(task);
        for name in [
            "JsonSerializer",
            "serialize",
            "ErrorHandler",
            "handle",
            "RedisCluster",
            "cluster",
            "x",
            "run",
            "HTTPServer",
            "is_json",
            "Json",
            "errors",
            "Zzz",
            "über_cluster",
        ] {
            let c = compact_lower(name);
            assert_eq!(
                symbol_matches(name, &c, &tw, &tc, 4),
                reference(name, &tw, &tc, 4),
                "{name}"
            );
        }
    }

    #[test]
    fn rank_and_match_equals_separate_rank_and_match_passes() {
        let mut g = graph(vec![import(0, 1), import(2, 1), import(0, 2)]);
        let names = [
            "processStripeWebhook",
            "StripeWebhook",
            "helper",
            "run",
            "JsonSerializer",
            "serialize",
            "ErrorHandler",
            "über_cluster",
            "HTTPServer",
            "tie",
            "tie",
            "x",
        ];
        g.symbols = names
            .iter()
            .enumerate()
            .map(|(i, name)| CodeSymbol {
                id: SymbolId(i as u32),
                file: FileId([0, 1, 2][i % 3]),
                name: (*name).into(),
                kind: SymbolKind::Function,
                signature: String::new(),
                // Same file+line for the two `tie` symbols: ordering falls
                // through to name and id.
                start_line: if *name == "tie" { 5 } else { i as u32 + 1 },
                end_line: 20,
                exported: true,
            })
            .collect();
        g.symbols[10].file = g.symbols[9].file;
        let base = compute_centrality(&g);
        for task in [
            "fix the stripe webhook processing",
            "look at a.rs and the JsonSerializer",
            "zzzz qqqq nonexistent thing",
            "!!! ??? ###",
            "",
            "tie tie tie",
            "how does the json serializer handle errors in über cluster",
            "HTTPServer run x",
        ] {
            let (ranked_both, matched_both) = rank_and_match(&g, task, &base);
            assert_eq!(ranked_both, rank_for_task(&g, task, &base), "rank: {task}");
            let separate = match_task(&g, task);
            assert_eq!(matched_both.files, separate.files, "files: {task}");
            assert_eq!(matched_both.symbols, separate.symbols, "symbols: {task}");
        }
        let (_, none) = rank_and_match(&g, "zzzz qqqq nonexistent thing", &base);
        assert!(none.is_empty());
        let (_, some) = rank_and_match(&g, "fix the stripe webhook processing", &base);
        assert!(!some.symbols.is_empty());
    }
}
