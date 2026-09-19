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
    let task_compact: String = task
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let mut boosts = vec![1.0; n];
    let positions = file_positions(&files);

    let mut symbols: Vec<&CodeSymbol> = g.symbols.iter().collect();
    symbols.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    for symbol in symbols {
        let Some(&i) = positions.get(&symbol.file) else {
            continue;
        };
        let words = split_words(&symbol.name);
        let meaningful: Vec<&String> = words.iter().filter(|word| word.len() >= 3).collect();
        let split_match = !meaningful.is_empty()
            && meaningful
                .iter()
                .filter(|word| task_words.contains((*word).as_str()))
                .count()
                >= meaningful.len().min(2);
        let compact: String = symbol
            .name
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let explicit_match = compact.len() >= 3 && task_compact.contains(&compact);

        if explicit_match || split_match {
            boosts[i] *= 10.0;
            // Specific names encode more relationship information than generic
            // names such as `run` or `helper`.
            if compact.len() >= 12 || meaningful.len() >= 3 {
                boosts[i] *= 10.0;
            }
        }
    }

    let context_files: BTreeSet<FileId> = files
        .iter()
        .filter(|file| {
            let path = file.path.as_str().to_lowercase();
            let name = file.path.file_name().to_lowercase();
            task.to_lowercase().contains(&path) || task.to_lowercase().contains(&name)
        })
        .map(|file| file.id)
        .collect();
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
        *aggregated.entry((from_i, to_i)).or_default() = aggregated
            .get(&(from_i, to_i))
            .copied()
            .unwrap_or_default()
            .saturating_add(weight);
    }

    let mut outgoing = vec![Vec::new(); positions.len()];
    for ((from, to), weight) in aggregated {
        outgoing[from].push((to, weight as f64));
    }
    outgoing
}

fn run_pagerank(outgoing: &[Vec<(usize, f64)>], restart: &[f64], scores: &mut Vec<f64>) {
    let n = scores.len();
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
            let total_weight: f64 = outgoing[from].iter().map(|(_, weight)| *weight).sum();
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

fn reachable_imports(g: &CodeGraph, target: FileId, reverse: bool) -> Vec<FileId> {
    let known: BTreeSet<FileId> = g.files.iter().map(|file| file.id).collect();
    if !known.contains(&target) {
        return Vec::new();
    }

    let mut adjacency: BTreeMap<FileId, BTreeSet<FileId>> = BTreeMap::new();
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

    let centrality = compute_centrality(g);
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
}
