//! 文件夹级架构图：把文件级 import 图折叠成宏观的"组 → 组"依赖视图。
//!
//! 这是对齐 OpenVisio `toGroupGraph`（`mcp/src/adapter.ts`）的实现，补上
//! 报告 1 指出的缺口：顶层文件夹作为节点、聚合后的跨文件夹 import 作为
//! 加权边。文件级图太细、单文件 centrality 又太局部，这一层恰好回答
//! "仓库分几块、块与块怎么依赖"的架构级问题。
//!
//! 与 OpenVisio 的差异（均有测试兜底）：
//! * 根级文件归 `"(root)"` 而非 `"."`，避免与任何真实目录名混淆；
//! * 分组粒度由 `depth` 控制（1 = 首段目录，即 openvisio 同款），
//!   钳制到 1..=3，防止把图切得过碎；
//! * 悬空边（端点解析不到文件信息）静默跳过，与 `graph.rs` 的处理同款；
//! * churn 不在本层聚合，由调用方把热点信息融合进渲染。
//!
//! 确定性与 `graph.rs` 同一纪律：组按名字典序输出，边按 `(from, to)`
//! 字典序输出；centrality 求和前先按 (path, id) 规范化文件遍历顺序，
//! 保证浮点加法顺序稳定。

use crate::graph::{Centrality, CodeGraph};
use crate::types::{CodeFile, EdgeKind, FileId, RelPath};
use std::collections::BTreeMap;

/// 根级文件（没有任何目录段）归入的组名。
const ROOT_GROUP: &str = "(root)";

/// 分组深度的钳制上限。
const MAX_GROUP_DEPTH: u8 = 3;

/// 文件夹组节点：组名（首段目录，根文件归 "(root)"）。
pub type GroupId = String;

/// 组级聚合边：src 组 → dst 组（同组内边不计），weight = 跨组 Import 边的
/// `edge.weight.max(1)` 之和（不是每条边记录 +1）。
#[derive(Debug, PartialEq, Eq)]
pub struct GroupEdge {
    pub from: GroupId,
    pub to: GroupId,
    pub weight: u32,
}

/// 组节点汇总。
#[derive(Debug, PartialEq)]
pub struct GroupNode {
    pub id: GroupId,
    pub files: u32,
    /// 组内文件的中心性之和（churn 不在此层，由调用方融合）。
    pub centrality: f64,
}

/// 把文件级 import 图折叠为顶层文件夹架构图。
///
/// depth 参数：分组粒度——1 = 首段目录（openvisio 同款），2 = 前两段；
/// 钳制 1..=3。路径段数不足的文件归入已有组或 "(root)"。
/// 确定性：组按名字典序、边按 (from, to) 字典序。
pub fn group_graph(
    g: &CodeGraph,
    centrality: &Centrality,
    depth: u8,
) -> (Vec<GroupNode>, Vec<GroupEdge>) {
    let depth = depth.clamp(1, MAX_GROUP_DEPTH);

    // 与 compute_centrality 相同的规范化：浮点求和前先定死遍历顺序，
    // 否则 Vec 顺序变化会改变组中心性的末几位。
    let mut files: Vec<&CodeFile> = g.files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));

    let mut members: BTreeMap<GroupId, (u32, f64)> = BTreeMap::new();
    let mut file_group: BTreeMap<FileId, GroupId> = BTreeMap::new();
    for file in files {
        let group = group_of(&file.path, depth);
        let score = centrality.by_file.get(&file.id).copied().unwrap_or(0.0);
        let entry = members.entry(group.clone()).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += score;
        file_group.insert(file.id, group);
    }

    // 聚合跨组 import 边；BTreeMap 让 (from, to) 的输出顺序天然有序。
    let mut counts: BTreeMap<(GroupId, GroupId), u32> = BTreeMap::new();
    for edge in g.edges.iter().filter(|edge| edge.kind == EdgeKind::Import) {
        let (Some(from_group), Some(to_group)) = (
            file_group.get(&FileId(edge.from)),
            file_group.get(&FileId(edge.to)),
        ) else {
            // 悬空边：照 graph.rs 的风格跳过，不 panic 也不计数。
            continue;
        };
        if from_group == to_group {
            continue;
        }
        *counts
            .entry((from_group.clone(), to_group.clone()))
            .or_insert(0) += edge.weight.max(1);
    }

    let nodes = members
        .into_iter()
        .map(|(id, (files, centrality))| GroupNode {
            id,
            files,
            centrality,
        })
        .collect();
    let edges = counts
        .into_iter()
        .map(|((from, to), weight)| GroupEdge { from, to, weight })
        .collect();
    (nodes, edges)
}

/// 取路径前 `depth` 段目录（以 '/' 连接）作为组名；没有目录段的根级文件
/// 归 `"(root)"`。目录段数不足 `depth` 时取全部可用段。
fn group_of(path: &RelPath, depth: u8) -> GroupId {
    let segments: Vec<&str> = path.as_str().split('/').collect();
    // 末段是文件名，其余才是目录段。
    let dirs = &segments[..segments.len() - 1];
    if dirs.is_empty() {
        return ROOT_GROUP.to_string();
    }
    dirs.iter()
        .take(depth as usize)
        .copied()
        .collect::<Vec<_>>()
        .join("/")
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
            weight: 1,
            confidence: Confidence::Scoped,
        }
    }

    fn centrality_of(entries: &[(u32, f64)]) -> Centrality {
        Centrality {
            by_file: entries
                .iter()
                .map(|&(id, score)| (FileId(id), score))
                .collect(),
        }
    }

    #[test]
    fn two_dirs_mutually_importing_yield_two_nodes_and_two_edges() {
        let g = CodeGraph {
            files: vec![
                file(0, "a/one.rs"),
                file(1, "a/two.rs"),
                file(2, "b/three.rs"),
            ],
            symbols: Vec::new(),
            edges: vec![import(0, 2), import(1, 2), import(2, 0)],
        };
        let (nodes, edges) = group_graph(&g, &Centrality::default(), 1);
        assert_eq!(
            nodes,
            vec![
                GroupNode {
                    id: "a".into(),
                    files: 2,
                    centrality: 0.0,
                },
                GroupNode {
                    id: "b".into(),
                    files: 1,
                    centrality: 0.0,
                },
            ]
        );
        // a→b 聚合了两条文件边（0→2 与 1→2），b→a 只有一条（2→0）。
        assert_eq!(
            edges,
            vec![
                GroupEdge {
                    from: "a".into(),
                    to: "b".into(),
                    weight: 2,
                },
                GroupEdge {
                    from: "b".into(),
                    to: "a".into(),
                    weight: 1,
                },
            ]
        );
    }

    #[test]
    fn intra_group_and_non_import_edges_are_not_counted() {
        let g = CodeGraph {
            files: vec![file(0, "a/one.rs"), file(1, "a/two.rs")],
            symbols: Vec::new(),
            edges: vec![
                import(0, 1),
                import(1, 0),
                CodeEdge {
                    from: 0,
                    to: 1,
                    kind: EdgeKind::Call,
                    weight: 5,
                    confidence: Confidence::Syntactic,
                },
            ],
        };
        let (nodes, edges) = group_graph(&g, &Centrality::default(), 1);
        // 同组内边不计、Call 边不算 import，但组节点（架构孤岛）仍在。
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, "a");
        assert_eq!(nodes[0].files, 2);
        assert!(edges.is_empty());
    }

    #[test]
    fn depth_two_groups_finer_than_depth_one() {
        let g = CodeGraph {
            files: vec![file(0, "a/b/c.rs"), file(1, "a/d/e.rs")],
            symbols: Vec::new(),
            edges: vec![import(0, 1)],
        };

        let (nodes1, edges1) = group_graph(&g, &Centrality::default(), 1);
        let ids1: Vec<&str> = nodes1.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids1, vec!["a"]);
        assert!(edges1.is_empty(), "同属 a 组，跨组边不存在");

        let (nodes2, edges2) = group_graph(&g, &Centrality::default(), 2);
        let ids2: Vec<&str> = nodes2.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids2, vec!["a/b", "a/d"]);
        assert!(nodes2.len() > nodes1.len(), "depth=2 应切出更多组");
        assert_eq!(
            edges2,
            vec![GroupEdge {
                from: "a/b".into(),
                to: "a/d".into(),
                weight: 1,
            }]
        );
    }

    #[test]
    fn root_level_files_join_the_root_group() {
        let g = CodeGraph {
            files: vec![file(0, "main.rs"), file(1, "src/lib.rs")],
            symbols: Vec::new(),
            edges: vec![import(0, 1), import(1, 0)],
        };
        let (nodes, edges) = group_graph(&g, &Centrality::default(), 1);
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["(root)", "src"]);
        assert_eq!(
            edges,
            vec![
                GroupEdge {
                    from: "(root)".into(),
                    to: "src".into(),
                    weight: 1,
                },
                GroupEdge {
                    from: "src".into(),
                    to: "(root)".into(),
                    weight: 1,
                },
            ]
        );
    }

    #[test]
    fn node_centrality_is_the_sum_of_member_scores() {
        let g = CodeGraph {
            files: vec![
                file(0, "a/one.rs"),
                file(1, "a/two.rs"),
                file(2, "b/three.rs"),
                file(3, "b/four.rs"),
            ],
            symbols: Vec::new(),
            edges: vec![import(0, 2)],
        };
        // b/four.rs（id 3）不在 centrality 表里：按 0 计入。
        let centrality = centrality_of(&[(0, 0.2), (1, 0.05), (2, 0.25)]);
        let (nodes, _) = group_graph(&g, &centrality, 1);
        assert_eq!(nodes.len(), 2);
        assert!((nodes[0].centrality - 0.25).abs() < 1e-12); // a: 0.2 + 0.05
        assert!((nodes[1].centrality - 0.25).abs() < 1e-12); // b: 0.25 + 0
        assert_eq!(nodes[0].files, 2);
        assert_eq!(nodes[1].files, 2);
    }

    #[test]
    fn dangling_edges_are_skipped_without_panicking() {
        let g = CodeGraph {
            files: vec![file(0, "a/one.rs"), file(1, "b/two.rs")],
            symbols: Vec::new(),
            edges: vec![
                import(0, 99), // to 端解析不到文件
                import(99, 1), // from 端解析不到文件
                import(0, 1),  // 正常跨组边
            ],
        };
        let (nodes, edges) = group_graph(&g, &Centrality::default(), 1);
        assert_eq!(nodes.len(), 2);
        assert_eq!(
            edges,
            vec![GroupEdge {
                from: "a".into(),
                to: "b".into(),
                weight: 1,
            }]
        );
    }

    #[test]
    fn output_is_deterministic_and_totally_ordered() {
        let first = CodeGraph {
            files: vec![
                file(0, "z/zero.rs"),
                file(1, "a/one.rs"),
                file(2, "m/two.rs"),
                file(3, "a/nested/three.rs"),
            ],
            symbols: Vec::new(),
            edges: vec![import(0, 2), import(2, 0), import(1, 2), import(0, 1)],
        };
        // 同样的文件与边，打乱存放顺序：输出必须逐项一致。
        let second = CodeGraph {
            files: vec![
                file(3, "a/nested/three.rs"),
                file(2, "m/two.rs"),
                file(0, "z/zero.rs"),
                file(1, "a/one.rs"),
            ],
            symbols: Vec::new(),
            edges: vec![import(0, 1), import(1, 2), import(2, 0), import(0, 2)],
        };

        let (nodes_a, edges_a) = group_graph(&first, &Centrality::default(), 1);
        let (nodes_b, edges_b) = group_graph(&second, &Centrality::default(), 1);
        assert_eq!(nodes_a, nodes_b);
        assert_eq!(edges_a, edges_b);

        let ids: Vec<&str> = nodes_a.iter().map(|n| n.id.as_str()).collect();
        let mut sorted_ids = ids.clone();
        sorted_ids.sort_unstable();
        assert_eq!(ids, sorted_ids, "组节点必须按名字典序");

        let pairs: Vec<(&str, &str)> = edges_a
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        let mut sorted_pairs = pairs.clone();
        sorted_pairs.sort_unstable();
        assert_eq!(pairs, sorted_pairs, "组边必须按 (from, to) 字典序");
    }

    #[test]
    fn group_edge_weight_sums_edge_weight_max_one() {
        let g = CodeGraph {
            files: vec![file(0, "a/one.rs"), file(1, "b/two.rs")],
            symbols: Vec::new(),
            edges: vec![
                CodeEdge {
                    from: 0,
                    to: 1,
                    kind: EdgeKind::Import,
                    weight: 5,
                    confidence: Confidence::Scoped,
                },
                CodeEdge {
                    from: 0,
                    to: 1,
                    kind: EdgeKind::Import,
                    weight: 0, // max(0,1) = 1
                    confidence: Confidence::Scoped,
                },
            ],
        };
        let (_, edges) = group_graph(&g, &Centrality::default(), 1);
        assert_eq!(
            edges,
            vec![GroupEdge {
                from: "a".into(),
                to: "b".into(),
                weight: 6, // 5 + 1，而非 +1 +1
            }]
        );
    }

    #[test]
    fn depth_is_clamped_between_one_and_three() {
        let g = CodeGraph {
            files: vec![file(0, "a/b/c/d.rs")],
            symbols: Vec::new(),
            edges: Vec::new(),
        };
        let ids = |depth: u8| {
            group_graph(&g, &Centrality::default(), depth)
                .0
                .iter()
                .map(|n| n.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(0), vec!["a"]); // 钳到 1
        assert_eq!(ids(1), vec!["a"]);
        assert_eq!(ids(2), vec!["a/b"]);
        assert_eq!(ids(3), vec!["a/b/c"]);
        assert_eq!(ids(9), vec!["a/b/c"]); // 钳到 3
    }
}
