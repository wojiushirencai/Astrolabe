//! import 图的多跳邻域（BFS），对齐 OpenVisio 的 `get_neighborhood`。
//!
//! OpenVisio 的工具层沿 import 边做 depth 1..=3 的有向 BFS，把中心文件的
//! 邻域切成"带跳数标注的局部子图"。Astrolabe 原先只有不分跳数的可达集
//! 查询（[`crate::graph::dependencies`] / [`crate::graph::dependents`]），
//! 本模块补上带深度的版本：每个文件标注距起点的跳数，首次到达的深度即
//! 最终深度。
//!
//! 确定性是本仓的设计规则：同层按 FileId 升序展开，结果按
//! `(depth, FileId)` 排序，同一图多次调用输出全序一致；visited 集让环
//! 天然安全。边遍历口径与 `graph.rs` 一致：只走 `EdgeKind::Import`，
//! 只认两端都出现在 `g.files` 里的边。

use crate::graph::CodeGraph;
use crate::types::{EdgeKind, FileId};
use std::collections::{BTreeMap, BTreeSet};

/// 深度上限。OpenVisio 的 `get_neighborhood` 在工具层用
/// `z.number().int().min(1).max(3)` 约束 depth：邻域 BFS 的开销随深度按
/// 分支因子指数增长，3 跳已足够框定"资深工程师会指给新人看的那块子图"，
/// 同时防止大仓库上一次查询爆炸。
pub const MAX_NEIGHBORHOOD_DEPTH: u8 = 3;

/// 邻域方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeighborhoodDirection {
    /// 我依赖谁（沿 import 边出向）。
    Dependencies,
    /// 谁依赖我（沿 import 边入向）。
    Dependents,
    /// 双向：每层同时沿 import 出向与入向扩展（单次 BFS，对齐 OpenVisio gatherNeighborhood）。
    Both,
}

/// BFS 多跳邻域结果：文件 + 距起点的跳数。
#[derive(Debug, PartialEq, Eq)]
pub struct NeighborhoodEntry {
    pub file: FileId,
    pub depth: u8,
}

/// 从 `target` 出发的 k 跳 import 邻域（BFS，去重，首次到达深度即最终
/// 深度；`target` 自身不包含在结果里）。
///
/// 对齐 OpenVisio 的 `get_neighborhood`；depth=1 的直接邻居集与
/// [`crate::graph::dependencies`] / [`crate::graph::dependents`] 的第一跳
/// 语义一致，同一方向上放大 depth 最终收敛到这两个函数的可达集。
///
/// - `depth == 0` 返回空 Vec；`depth > 3` 钳制到 [`MAX_NEIGHBORHOOD_DEPTH`]
///   （选钳制而非报错：上限沿用 OpenVisio 的 1..=3 约束，防大仓库 BFS
///   爆炸，且本函数承诺不 panic）；
/// - `Both` = 单次双向 BFS（每层同时扩展出向与入向），对齐 OpenVisio
///   `gatherNeighborhood`；不是两次单向 BFS 的并集；
/// - `target` 不在 `g.files` 里时返回空（与 `graph::dependencies` 对未知
///   目标的处理一致）；
/// - 结果按 `(depth, FileId)` 升序排序，同一图多次调用输出全序一致。
pub fn neighborhood(
    g: &CodeGraph,
    target: FileId,
    direction: NeighborhoodDirection,
    depth: u8,
) -> Vec<NeighborhoodEntry> {
    if depth == 0 {
        return Vec::new();
    }
    let depth = depth.min(MAX_NEIGHBORHOOD_DEPTH);

    let known: BTreeSet<FileId> = g.files.iter().map(|file| file.id).collect();
    if !known.contains(&target) {
        return Vec::new();
    }

    // 正反邻接一次构建，Both 与单方向共用，避免两次扫边。
    let (fwd, rev) = import_adjacency_pair(g, &known);
    match direction {
        NeighborhoodDirection::Dependencies => bfs_layers(&[&fwd], target, depth),
        NeighborhoodDirection::Dependents => bfs_layers(&[&rev], target, depth),
        // 单次双向 BFS：每一层同时沿出向与入向扩展（非两次单向并集）。
        NeighborhoodDirection::Both => bfs_layers(&[&fwd, &rev], target, depth),
    }
}

/// Import 邻接表，正反两张一次遍历同时构建。
fn import_adjacency_pair(
    g: &CodeGraph,
    known: &BTreeSet<FileId>,
) -> (
    BTreeMap<FileId, BTreeSet<FileId>>,
    BTreeMap<FileId, BTreeSet<FileId>>,
) {
    let mut fwd: BTreeMap<FileId, BTreeSet<FileId>> = BTreeMap::new();
    let mut rev: BTreeMap<FileId, BTreeSet<FileId>> = BTreeMap::new();
    for edge in g.edges.iter().filter(|edge| edge.kind == EdgeKind::Import) {
        let (from, to) = (FileId(edge.from), FileId(edge.to));
        if !known.contains(&from) || !known.contains(&to) {
            continue;
        }
        fwd.entry(from).or_default().insert(to);
        rev.entry(to).or_default().insert(from);
    }
    (fwd, rev)
}

/// 层序 BFS：每个 frontier 节点按传入的邻接表依次扩展（Both 时传正+反）。
/// visited 保证环安全；首次到达深度即最终深度。
fn bfs_layers(
    adjacencies: &[&BTreeMap<FileId, BTreeSet<FileId>>],
    target: FileId,
    depth: u8,
) -> Vec<NeighborhoodEntry> {
    // 起点也标记为已访问：它不出现在结果里，环回到起点时自然被挡下。
    let mut visited: BTreeSet<FileId> = BTreeSet::new();
    visited.insert(target);
    let mut frontier = vec![target];
    let mut result = Vec::new();
    for ring in 1..=depth {
        let mut next: Vec<FileId> = Vec::new();
        for &current in &frontier {
            for adjacency in adjacencies {
                if let Some(neighbors) = adjacency.get(&current) {
                    // BTreeSet 升序迭代让同层展开顺序确定；最终顺序仍以
                    // (depth, FileId) 的显式排序为准。
                    for &file in neighbors {
                        if visited.insert(file) {
                            result.push(NeighborhoodEntry { file, depth: ring });
                            next.push(file);
                        }
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    result.sort_by_key(|entry| (entry.depth, entry.file));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CodeEdge, CodeFile, Confidence, Language, RelPath};

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
            confidence: Confidence::Scoped,
            weight: 1,
        }
    }

    fn graph(files: Vec<CodeFile>, edges: Vec<CodeEdge>) -> CodeGraph {
        CodeGraph {
            files,
            symbols: Vec::new(),
            edges,
        }
    }

    fn ids(entries: &[NeighborhoodEntry]) -> Vec<FileId> {
        entries.iter().map(|entry| entry.file).collect()
    }

    fn diamond() -> CodeGraph {
        // A(0) → B(1) → D(3)，A(0) → C(2) → D(3)：D 有两条 2 跳路径。
        graph(
            vec![
                file(0, "a.rs"),
                file(1, "b.rs"),
                file(2, "c.rs"),
                file(3, "d.rs"),
            ],
            vec![import(0, 1), import(1, 3), import(0, 2), import(2, 3)],
        )
    }

    #[test]
    fn diamond_reaches_shared_dependency_once_at_correct_depth() {
        let g = diamond();
        let entries = neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 2);
        assert_eq!(
            entries,
            vec![
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(3),
                    depth: 2
                },
            ]
        );
        assert_eq!(entries.iter().filter(|e| e.file == FileId(3)).count(), 1);
    }

    #[test]
    fn cycle_terminates_with_correct_depths() {
        // A(0) → B(1) → C(2) → A(0)。
        let g = graph(
            vec![file(0, "a.rs"), file(1, "b.rs"), file(2, "c.rs")],
            vec![import(0, 1), import(1, 2), import(2, 0)],
        );
        let entries = neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 3);
        assert_eq!(
            entries,
            vec![
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 2
                },
            ]
        );
        // 环不会把起点自己带回来。
        assert!(entries.iter().all(|e| e.file != FileId(0)));
    }

    #[test]
    fn dependents_mirror_dependencies() {
        let g = diamond();
        assert_eq!(
            neighborhood(&g, FileId(3), NeighborhoodDirection::Dependents, 2),
            vec![
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(0),
                    depth: 2
                },
            ]
        );
        assert_eq!(
            neighborhood(&g, FileId(3), NeighborhoodDirection::Dependencies, 2),
            Vec::<NeighborhoodEntry>::new()
        );
        // 对称性：把每条边反向后，Dependencies 的结果与原图 Dependents 一致。
        let flipped = graph(
            vec![
                file(0, "a.rs"),
                file(1, "b.rs"),
                file(2, "c.rs"),
                file(3, "d.rs"),
            ],
            vec![import(1, 0), import(3, 1), import(2, 0), import(3, 2)],
        );
        assert_eq!(
            neighborhood(&flipped, FileId(3), NeighborhoodDirection::Dependencies, 2),
            neighborhood(&g, FileId(3), NeighborhoodDirection::Dependents, 2),
        );
    }

    #[test]
    fn both_bidirectional_reaches_min_depth_neighbors() {
        // A(0)：依赖 C(2)、D(3)；被 B(1)、E(4) 依赖；另有 C(2) → B(1)。
        // 双向 BFS 下 B/C 在第一层即可到达（入/出向各一跳）。
        let g = graph(
            vec![
                file(0, "a.rs"),
                file(1, "b.rs"),
                file(2, "c.rs"),
                file(3, "d.rs"),
                file(4, "e.rs"),
            ],
            vec![
                import(1, 0),
                import(0, 2),
                import(2, 1),
                import(0, 3),
                import(4, 0),
            ],
        );
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 2),
            vec![
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(3),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 2
                },
            ]
        );
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependents, 2),
            vec![
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(4),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 2
                },
            ]
        );
        // 双向第一层即覆盖 B/C/D/E（含只出现在一侧的 D 与 E）。
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Both, 2),
            vec![
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(3),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(4),
                    depth: 1
                },
            ]
        );
    }

    /// A→B←C：两次单向并集从 A 到不了 C；双向 BFS depth=2 能经 B 的入边到达 C。
    #[test]
    fn both_bidirectional_reaches_co_dependents_via_shared_neighbor() {
        // A(0) → B(1) ← C(2)
        let g = graph(
            vec![file(0, "a.rs"), file(1, "b.rs"), file(2, "c.rs")],
            vec![import(0, 1), import(2, 1)],
        );
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 2),
            vec![NeighborhoodEntry {
                file: FileId(1),
                depth: 1
            }]
        );
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependents, 2),
            Vec::<NeighborhoodEntry>::new()
        );
        // 并集视角也只有 B；双向 BFS 在 depth=2 纳入 C。
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Both, 2),
            vec![
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 2
                },
            ]
        );
    }

    #[test]
    fn depth_is_clamped_to_the_1_to_3_domain() {
        // 链 A(0) → B(1) → C(2) → D(3) → E(4)。
        let g = graph(
            vec![
                file(0, "a.rs"),
                file(1, "b.rs"),
                file(2, "c.rs"),
                file(3, "d.rs"),
                file(4, "e.rs"),
            ],
            vec![import(0, 1), import(1, 2), import(2, 3), import(3, 4)],
        );
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 0),
            Vec::<NeighborhoodEntry>::new()
        );
        // 5 被钳到 3：E 在 4 跳处，不出现。
        let clamped = neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 5);
        assert_eq!(
            clamped,
            vec![
                NeighborhoodEntry {
                    file: FileId(1),
                    depth: 1
                },
                NeighborhoodEntry {
                    file: FileId(2),
                    depth: 2
                },
                NeighborhoodEntry {
                    file: FileId(3),
                    depth: 3
                },
            ]
        );
        assert_eq!(
            clamped,
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 3)
        );
        assert_eq!(
            clamped,
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, u8::MAX)
        );
    }

    #[test]
    fn isolated_unknown_and_non_import_edges_yield_empty() {
        // 只有 Call 边：邻域只走 EdgeKind::Import。
        let mut call = import(0, 1);
        call.kind = EdgeKind::Call;
        call.confidence = Confidence::Syntactic;
        let g = graph(vec![file(0, "a.rs"), file(1, "b.rs")], vec![call]);
        for direction in [
            NeighborhoodDirection::Dependencies,
            NeighborhoodDirection::Dependents,
            NeighborhoodDirection::Both,
        ] {
            assert_eq!(
                neighborhood(&g, FileId(0), direction, 3),
                Vec::<NeighborhoodEntry>::new()
            );
        }
        // 悬挂边（端点不在 files 里）不算邻域。
        let dangling = graph(vec![file(0, "a.rs")], vec![import(0, 9)]);
        assert_eq!(
            neighborhood(&dangling, FileId(0), NeighborhoodDirection::Dependencies, 3),
            Vec::<NeighborhoodEntry>::new()
        );
        // 未知目标：与 graph::dependencies 的处理一致，返回空。
        assert_eq!(
            neighborhood(&dangling, FileId(7), NeighborhoodDirection::Both, 2),
            Vec::<NeighborhoodEntry>::new()
        );
    }

    #[test]
    fn output_is_deterministic_across_calls_and_edge_order() {
        let files = || {
            vec![
                file(0, "a.rs"),
                file(1, "b.rs"),
                file(2, "c.rs"),
                file(3, "d.rs"),
                file(4, "e.rs"),
            ]
        };
        let g = graph(
            files(),
            vec![
                import(0, 1),
                import(1, 2),
                import(2, 0),
                import(0, 3),
                import(3, 2),
                import(4, 0),
                import(4, 3),
            ],
        );
        // 同样的边、不同的输入顺序：输出必须一致。
        let shuffled = graph(
            files(),
            vec![
                import(4, 3),
                import(3, 2),
                import(2, 0),
                import(4, 0),
                import(1, 2),
                import(0, 3),
                import(0, 1),
            ],
        );
        for direction in [
            NeighborhoodDirection::Dependencies,
            NeighborhoodDirection::Dependents,
            NeighborhoodDirection::Both,
        ] {
            let first = neighborhood(&g, FileId(0), direction, 3);
            assert!(!first.is_empty());
            assert_eq!(first, neighborhood(&shuffled, FileId(0), direction, 3));
            for _ in 0..3 {
                assert_eq!(first, neighborhood(&g, FileId(0), direction, 3));
            }
            // 全序：严格按 (depth, FileId) 升序（去重保证无并列）。
            for pair in first.windows(2) {
                assert!((pair[0].depth, pair[0].file) < (pair[1].depth, pair[1].file));
            }
        }
    }

    #[test]
    fn agrees_with_graph_reachable_imports() {
        // 链 A(0) → B(1) → C(2) → D(3)：直径 3，depth=3 的邻域集等于
        // graph::dependencies/dependents 的可达集；depth=1 即直接邻居。
        let g = graph(
            vec![
                file(0, "a.rs"),
                file(1, "b.rs"),
                file(2, "c.rs"),
                file(3, "d.rs"),
            ],
            vec![import(0, 1), import(1, 2), import(2, 3)],
        );
        assert_eq!(
            neighborhood(&g, FileId(0), NeighborhoodDirection::Dependencies, 1),
            vec![NeighborhoodEntry {
                file: FileId(1),
                depth: 1
            }]
        );

        let mut reachable = crate::graph::dependencies(&g, FileId(0));
        reachable.sort();
        let mut got = ids(&neighborhood(
            &g,
            FileId(0),
            NeighborhoodDirection::Dependencies,
            3,
        ));
        got.sort();
        assert_eq!(got, reachable);

        let mut users = crate::graph::dependents(&g, FileId(3));
        users.sort();
        let mut got_users = ids(&neighborhood(
            &g,
            FileId(3),
            NeighborhoodDirection::Dependents,
            3,
        ));
        got_users.sort();
        assert_eq!(got_users, users);
    }
}
