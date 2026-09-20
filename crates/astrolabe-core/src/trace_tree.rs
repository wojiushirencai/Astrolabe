//! 多跳调用树：沿 [`EdgeKind::Call`] 边做 DFS，回答"谁调用它 / 它调用谁"。
//!
//! 对齐 openvisio `core/src/trace.ts` 的树形 trace（depth 上限、环上截断、
//! 确定性输出），两点刻意不同：
//!
//! * **环检测按路径，而非全局 visited。** openvisio 的 `renderTree` 用一个
//!   全局 `visited` 集合，同一符号只在第一个分支出现；这里用"根 → 当前栈"
//!   的路径集——菱形（A→B→D、A→C→D）里的 D 在两个分支各展开一次，只有
//!   真正的环（路径上重复出现）才截断。代价是输出可能更大，收益是不把
//!   "经由不同路径到达"误报成环。
//! * **只产数据，不渲染。** token 预算、缩进、centrality 排序、警示语都
//!   留给渲染层；本模块只保证 DFS 序与兄弟按 `SymbolId` 升序的确定性。
//!
//! 多根批量场景用 [`trace_forest`]：邻接只建一次，且带全森林总节点硬
//! 上限——上层对每个同名符号逐根调 [`trace_tree`] 会重复 O(E log E)
//! 建表，又没有任何总量约束，depth=6 的稠密调用图上是组合爆炸
//! （审查发现）。
//!
//! ⚠️ `Call` 边是名字匹配的产物（`Confidence::Syntactic`）：动态分派、
//! 接口/虚方法调用可能漏报，同名的不同符号可能被接到一起。渲染层必须
//! 向使用者标注这一点；把本树当线索，不当事实。

use crate::graph::CodeGraph;
use crate::types::{EdgeKind, SymbolId};
use std::collections::{BTreeMap, BTreeSet};

/// openvisio 同款上限：最深 6 跳，防止在稠密调用图上指数爆炸。
const MAX_DEPTH: u8 = 6;

/// 调用树方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceDirection {
    Callers,
    Callees,
    Both,
}

/// 树节点：符号 + 深度 + 环标记（同一符号在当前根→叶路径上重复出现）。
#[derive(Debug)]
pub struct TraceNode {
    pub symbol: SymbolId,
    pub depth: u8,
    /// true = 该符号在根到此节点的路径上已出现过（环），其子树不再展开。
    pub cycle: bool,
}

/// 从起始符号出发的 DFS 调用树（沿 `Call` 边）。
///
/// 边方向语义：`CodeEdge` 的 `from` 调用 `to`。[`TraceDirection::Callers`]
/// 反向遍历（谁调用 `start`），[`TraceDirection::Callees`] 正向遍历
/// （`start` 调用谁），[`TraceDirection::Both`] 先输出 Callees 树、再输出
/// Callers 树——两棵完整树顺序拼接，根节点因此出现两次，环检测也各自独立。
///
/// `depth` 是根之外还要展开的跳数，上限 [`MAX_DEPTH`]（6，openvisio 同款）：
/// `0` 仅返回根节点；`1..=6` 原样使用；`> 6` 按 `6` 处理。
///
/// 环检测基于"根 → 当前节点"的路径集而非全局 visited：同一符号出现在
/// 不同分支时各自展开（`cycle = false`），只在真环上截断（`cycle = true`，
/// 不再深入）。结果按 DFS 序返回扁平 [`Vec`]，渲染层靠 `depth`/`cycle`
/// 重建树形；兄弟节点按 `SymbolId` 升序保证确定性；端点不在符号表里的边
/// 被忽略，同一对孩子（重复 Call 边）只出现一次。
///
/// ⚠️ 结果是 [`Confidence::Syntactic`](crate::types::Confidence) 级别的：
/// `Call` 边由名字匹配导出，动态分派与虚方法调用可能漏报，同名符号可能
/// 混线。渲染层必须带上这个警示，必要时让使用者回退到全文搜索核验。
///
/// `start` 不在 `g.symbols` 中时返回空 [`Vec`]。
pub fn trace_tree(
    g: &CodeGraph,
    start: SymbolId,
    direction: TraceDirection,
    depth: u8,
) -> Vec<TraceNode> {
    let known: BTreeSet<SymbolId> = g.symbols.iter().map(|s| s.id).collect();
    if !known.contains(&start) {
        return Vec::new();
    }
    let max_depth = depth.min(MAX_DEPTH);
    let (fwd, rev) = call_adjacency_pair(g, &known);
    let mut out = Vec::new();
    // 单根无总量上限：预算传 usize::MAX，等效于不设限。
    let mut unlimited_budget = usize::MAX;
    let _ = expand_root(
        direction,
        &fwd,
        &rev,
        start,
        max_depth,
        &mut unlimited_budget,
        &mut out,
    );
    out
}

/// 按方向展开一个根：Callees / Callers / Both（先 Callees 再 Callers）。
/// 与 [`trace_tree`] / [`trace_forest`] 共用，去掉方向分发重复。
/// 返回 `true` 表示因预算耗尽而未能完整展开（方向或子树被截断）。
fn expand_root(
    direction: TraceDirection,
    fwd: &BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    rev: &BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    root: SymbolId,
    max_depth: u8,
    budget: &mut usize,
    out: &mut Vec<TraceNode>,
) -> bool {
    let directions: &[bool] = match direction {
        TraceDirection::Callees => &[false],
        TraceDirection::Callers => &[true],
        TraceDirection::Both => &[false, true],
    };
    let mut cut_short = false;
    for &callers in directions {
        if *budget == 0 {
            cut_short = true;
            break;
        }
        let adj = if callers { rev } else { fwd };
        out.push(TraceNode {
            symbol: root,
            depth: 0,
            cycle: false,
        });
        *budget -= 1;
        if walk(adj, root, 0, max_depth, &mut vec![root], budget, out) {
            cut_short = true;
        }
    }
    cut_short
}

/// 森林条目：根 + 该根树下的节点（含 depth=0 的根自身）。
#[derive(Debug)]
pub struct TraceForestEntry {
    pub root: SymbolId,
    pub nodes: Vec<TraceNode>,
}

/// [`trace_forest`] 的完整结果：条目 + 预算耗尽信号。
#[derive(Debug)]
pub struct TraceForest {
    pub entries: Vec<TraceForestEntry>,
    /// `max_nodes` 耗尽导致展开提前停止（含中途截断某一根，或一开始即为 0）。
    pub truncated: bool,
    /// 因预算耗尽而完全未展开、未进入 `entries` 的已知根数量。
    pub omitted_roots: usize,
}

/// 多根调用森林：一次构建邻接表后从每个根各自展开（每根独立环检测），
/// 输出按根分组且带根标记；全森林总节点数达 max_nodes 即截断（截断处
/// 该根的剩余子树整体丢弃，已收集部分保留）。
///
/// 与逐根调用 trace_tree 的差异：邻接只建一次（O(E log E) 一次）+
/// 硬上限防爆。roots 为空 → 空 entries。根不存在的跳过（与 trace_tree
/// 单根返回空一致）。
///
/// **为什么必须有 max_nodes（审查发现）：** 上层按同名符号逐根调
/// [`trace_tree`] 时输出无任何总量约束，而路径级环检测（见模块文档）
/// 允许同一符号在不同分支重复展开——depth=6 的稠密调用图上节点数是
/// 指数级的，一次请求就能组合爆炸。max_nodes 是全森林的节点硬上限：
/// 计入所有根输出的每个节点（含 depth=0 的根自身；[`TraceDirection::Both`]
/// 下根在 Callees/Callers 两棵树里各计一次）。预算耗尽时当前根保留已
/// 收集部分并停止展开，**后续根直接跳过、不出现在 entries 里**（计入
/// [`TraceForest::omitted_roots`]，且 [`TraceForest::truncated`] = true）；
/// `max_nodes = 0` 则所有已知根都跳过。预算足够大（`usize::MAX`）时，
/// 每个 entry 的 `nodes` 与逐根调用 `trace_tree(g, root, direction, depth)`
/// 完全一致，且 `truncated == false`、`omitted_roots == 0`。
///
/// depth 语义同 [`trace_tree`]：`0` 仅根、`1..=6` 原样、`> 6` 钳为 6。
/// 环检测也同 [`trace_tree`]——每根一条"根 → 当前栈"路径，根与根之间
/// 互不影响：一个根里截断的环不会截断另一个根的路径。
pub fn trace_forest(
    g: &CodeGraph,
    roots: &[SymbolId],
    direction: TraceDirection,
    depth: u8,
    max_nodes: usize,
) -> TraceForest {
    if roots.is_empty() {
        return TraceForest {
            entries: Vec::new(),
            truncated: false,
            omitted_roots: 0,
        };
    }
    let known: BTreeSet<SymbolId> = g.symbols.iter().map(|s| s.id).collect();
    let max_depth = depth.min(MAX_DEPTH);
    let (fwd, rev) = call_adjacency_pair(g, &known);

    let mut budget = max_nodes;
    let mut truncated = false;
    let mut omitted_roots = 0;
    let mut entries = Vec::new();
    for &root in roots {
        if !known.contains(&root) {
            continue;
        }
        if budget == 0 {
            truncated = true;
            omitted_roots += 1;
            continue;
        }
        let mut nodes = Vec::new();
        if expand_root(
            direction,
            &fwd,
            &rev,
            root,
            max_depth,
            &mut budget,
            &mut nodes,
        ) {
            truncated = true;
        }
        entries.push(TraceForestEntry { root, nodes });
    }
    TraceForest {
        entries,
        truncated,
        omitted_roots,
    }
}

/// Call 邻接表，正反两张一次遍历同时构建：正向（`fwd`）key 是调用方、
/// 值是被调方；反向（`rev`）key 是被调方、值是调用方。`BTreeMap`/`BTreeSet`
/// 让兄弟天然按 `SymbolId` 升序，无需显式排序——同 `graph.rs` 的做法。
///
/// 抽成 [`trace_tree`] 与 [`trace_forest`] 共用的 helper：多根场景里
/// 邻接只建一次（O(E log E) 一次），而不是每根（甚至每个方向）重建。
fn call_adjacency_pair(
    g: &CodeGraph,
    known: &BTreeSet<SymbolId>,
) -> (
    BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    BTreeMap<SymbolId, BTreeSet<SymbolId>>,
) {
    let mut fwd: BTreeMap<SymbolId, BTreeSet<SymbolId>> = BTreeMap::new();
    let mut rev: BTreeMap<SymbolId, BTreeSet<SymbolId>> = BTreeMap::new();
    for edge in g.edges.iter().filter(|e| e.kind == EdgeKind::Call) {
        let (from, to) = (SymbolId(edge.from), SymbolId(edge.to));
        if !known.contains(&from) || !known.contains(&to) {
            continue;
        }
        fwd.entry(from).or_default().insert(to);
        rev.entry(to).or_default().insert(from);
    }
    (fwd, rev)
}

/// 从 `current` 展开，`path` 是根到 `current` 的符号栈（环检测依据）。
/// 已在 `max_depth` 的节点不再有子节点，返回而非压栈。`budget` 是还允许
/// 输出的节点数，每压一个节点扣 1、归零即停止展开（剩余子树整体丢弃）；
/// [`trace_tree`] 传 `usize::MAX`，等效于原来的无上限行为。
/// 返回 `true` 表示因预算耗尽提前停止（还有未输出的兄弟/子树）。
fn walk(
    adj: &BTreeMap<SymbolId, BTreeSet<SymbolId>>,
    current: SymbolId,
    depth: u8,
    max_depth: u8,
    path: &mut Vec<SymbolId>,
    budget: &mut usize,
    out: &mut Vec<TraceNode>,
) -> bool {
    if depth >= max_depth {
        return false;
    }
    if *budget == 0 {
        // 调用方在仍有可展开空间时把预算耗尽——视为截断。
        return adj.get(&current).is_some_and(|c| !c.is_empty());
    }
    let Some(children) = adj.get(&current) else {
        return false;
    };
    let mut cut_short = false;
    for &child in children {
        if *budget == 0 {
            return true;
        }
        let cycle = path.contains(&child);
        out.push(TraceNode {
            symbol: child,
            depth: depth + 1,
            cycle,
        });
        *budget -= 1;
        if !cycle {
            if *budget == 0 {
                // 刚收下该节点，若它本还可再展开则算截断。
                if depth + 1 < max_depth && adj.get(&child).is_some_and(|c| !c.is_empty()) {
                    cut_short = true;
                }
            } else {
                path.push(child);
                if walk(adj, child, depth + 1, max_depth, path, budget, out) {
                    cut_short = true;
                }
                path.pop();
            }
        }
    }
    cut_short
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CodeEdge, CodeFile, CodeSymbol, Confidence, FileId, Language, RelPath, SymbolKind,
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

    fn sym(id: u32) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(id),
            file: FileId(0),
            name: format!("s{id}"),
            kind: SymbolKind::Function,
            signature: String::new(),
            start_line: 1,
            end_line: 1,
            exported: true,
        }
    }

    /// `from` 调用 `to`，与生产边的方向语义一致。
    fn call(from: u32, to: u32) -> CodeEdge {
        CodeEdge {
            from,
            to,
            kind: EdgeKind::Call,
            weight: 1,
            confidence: Confidence::Syntactic,
        }
    }

    fn graph(n_symbols: u32, edges: Vec<CodeEdge>) -> CodeGraph {
        CodeGraph {
            files: vec![file(0, "a.rs")],
            symbols: (0..n_symbols).map(sym).collect(),
            edges,
        }
    }

    fn rows(nodes: &[TraceNode]) -> Vec<(u32, u8, bool)> {
        nodes
            .iter()
            .map(|n| (n.symbol.0, n.depth, n.cycle))
            .collect()
    }

    /// 1. 链 A→B→C，depth 2：三节点，深度 0/1/2。
    #[test]
    fn chain_walks_three_levels() {
        let g = graph(3, vec![call(0, 1), call(1, 2)]);
        let t = trace_tree(&g, SymbolId(0), TraceDirection::Callees, 2);
        assert_eq!(rows(&t), vec![(0, 0, false), (1, 1, false), (2, 2, false)]);
    }

    /// 2. 直接环 A→B→A：回到 A 时 cycle=true，且没有孙辈。
    #[test]
    fn direct_cycle_is_marked_and_pruned() {
        let g = graph(2, vec![call(0, 1), call(1, 0)]);
        let t = trace_tree(&g, SymbolId(0), TraceDirection::Callees, 6);
        assert_eq!(rows(&t), vec![(0, 0, false), (1, 1, false), (0, 2, true)]);
    }

    /// 自环 A→A：根在自身的孩子位置标环。
    #[test]
    fn self_loop_is_a_cycle_at_depth_one() {
        let g = graph(1, vec![call(0, 0)]);
        let t = trace_tree(&g, SymbolId(0), TraceDirection::Callees, 3);
        assert_eq!(rows(&t), vec![(0, 0, false), (0, 1, true)]);
    }

    /// 3. 菱形 A→B→D、A→C→D：D 在两个分支各出现一次，均非环。
    #[test]
    fn diamond_expands_each_branch_without_false_cycle() {
        let g = graph(4, vec![call(0, 1), call(0, 2), call(1, 3), call(2, 3)]);
        let t = trace_tree(&g, SymbolId(0), TraceDirection::Callees, 2);
        assert_eq!(
            rows(&t),
            vec![
                (0, 0, false),
                (1, 1, false),
                (3, 2, false),
                (2, 1, false),
                (3, 2, false),
            ]
        );
    }

    /// 4. Callers 反向：链 A→B→C 从 C 出发得到 C←B←A。
    #[test]
    fn callers_traverses_incoming_edges() {
        let g = graph(3, vec![call(0, 1), call(1, 2)]);
        let t = trace_tree(&g, SymbolId(2), TraceDirection::Callers, 2);
        assert_eq!(rows(&t), vec![(2, 0, false), (1, 1, false), (0, 2, false)]);
        // 反过来，Callees 从 C 出发只根。
        assert_eq!(
            rows(&trace_tree(&g, SymbolId(2), TraceDirection::Callees, 2)),
            vec![(2, 0, false)]
        );
    }

    /// 5. depth 钳制：0 → 仅根；> 6 → 按 6。
    #[test]
    fn depth_zero_is_root_only_and_above_six_clamps() {
        let edges: Vec<CodeEdge> = (0..7).map(|i| call(i, i + 1)).collect();
        let g = graph(8, edges);

        assert_eq!(
            rows(&trace_tree(&g, SymbolId(0), TraceDirection::Callees, 0)),
            vec![(0, 0, false)]
        );

        let expected: Vec<(u32, u8, bool)> = (0..=6u8).map(|i| (u32::from(i), i, false)).collect();
        let clamped = trace_tree(&g, SymbolId(0), TraceDirection::Callees, 9);
        assert_eq!(rows(&clamped), expected);
        // 钳到 6 与显式给 6 完全一致。
        assert_eq!(
            rows(&trace_tree(&g, SymbolId(0), TraceDirection::Callees, 6)),
            expected
        );
    }

    /// 6. 兄弟按 SymbolId 升序；重复边折叠；端点不在符号表的边被忽略。
    #[test]
    fn siblings_are_deduped_sorted_and_dangling_edges_skipped() {
        // 刻意乱序：s2 边先于 s1，s1 重复两次，s7 无对应符号。
        let g = graph(3, vec![call(0, 2), call(0, 7), call(0, 1), call(0, 1)]);
        let t = trace_tree(&g, SymbolId(0), TraceDirection::Callees, 1);
        assert_eq!(rows(&t), vec![(0, 0, false), (1, 1, false), (2, 1, false)]);
    }

    /// 7. 无 Call 边触碰的符号 → 只根（Both 也只根，两棵树各一个）。
    #[test]
    fn isolated_symbol_yields_only_root() {
        let g = graph(3, vec![call(0, 1)]);
        assert_eq!(
            rows(&trace_tree(&g, SymbolId(2), TraceDirection::Both, 6)),
            vec![(2, 0, false), (2, 0, false)]
        );
    }

    /// Both = Callees 树在前、Callers 树在后，根各出现一次。
    #[test]
    fn both_concatenates_callees_then_callers() {
        // s0→s1（s0 的 callee），s2→s0（s0 的 caller）。
        let g = graph(3, vec![call(0, 1), call(2, 0)]);
        let t = trace_tree(&g, SymbolId(0), TraceDirection::Both, 1);
        assert_eq!(
            rows(&t),
            vec![(0, 0, false), (1, 1, false), (0, 0, false), (2, 1, false),]
        );
    }

    /// 不在符号表里的 start → 空 Vec，而不是一棵假树。
    #[test]
    fn unknown_start_returns_empty() {
        let g = graph(2, vec![call(0, 1)]);
        assert!(trace_tree(&g, SymbolId(9), TraceDirection::Callees, 3).is_empty());
    }

    /// 森林：两根共享同一张邻接表，结果按根分组，组内各含 depth=0 根；
    /// 两根共同到达的符号（s2）在两组里各自展开，互不干扰。
    #[test]
    fn forest_two_roots_grouped_each_with_root() {
        let g = graph(3, vec![call(0, 2), call(1, 2)]);
        let f = trace_forest(
            &g,
            &[SymbolId(0), SymbolId(1)],
            TraceDirection::Callees,
            1,
            100,
        );
        assert!(!f.truncated);
        assert_eq!(f.omitted_roots, 0);
        assert_eq!(f.entries.len(), 2);
        assert_eq!(f.entries[0].root, SymbolId(0));
        assert_eq!(
            rows(&f.entries[0].nodes),
            vec![(0, 0, false), (2, 1, false)]
        );
        assert_eq!(f.entries[1].root, SymbolId(1));
        assert_eq!(
            rows(&f.entries[1].nodes),
            vec![(1, 0, false), (2, 1, false)]
        );
    }

    /// max_nodes=3：第一根收满 3 个节点即截断（剩余子树丢弃），第二根
    /// 整体跳过——entries 里没有它的条目。max_nodes=0 则所有根都跳过。
    #[test]
    fn forest_truncates_at_max_nodes_and_skips_rest() {
        let g = graph(
            6,
            vec![call(0, 1), call(1, 2), call(2, 3), call(3, 4), call(4, 5)],
        );
        let f = trace_forest(
            &g,
            &[SymbolId(0), SymbolId(3)],
            TraceDirection::Callees,
            6,
            3,
        );
        assert!(f.truncated);
        assert_eq!(f.omitted_roots, 1);
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].root, SymbolId(0));
        assert_eq!(
            rows(&f.entries[0].nodes),
            vec![(0, 0, false), (1, 1, false), (2, 2, false)]
        );
        let empty = trace_forest(&g, &[SymbolId(0)], TraceDirection::Callees, 6, 0);
        assert!(empty.entries.is_empty());
        assert!(empty.truncated);
        assert_eq!(empty.omitted_roots, 1);
    }

    /// 等价性：预算足够大（usize::MAX）时，森林各组节点 == 各根单独调
    /// trace_tree 的结果；Both 形态下每组内根出现两次（Callees 树 +
    /// Callers 树拼接）。
    #[test]
    fn forest_with_big_budget_matches_per_root_trace_tree() {
        let g = graph(5, vec![call(0, 1), call(1, 3), call(2, 3), call(3, 4)]);
        let roots = [SymbolId(0), SymbolId(2)];
        for dir in [
            TraceDirection::Callees,
            TraceDirection::Callers,
            TraceDirection::Both,
        ] {
            let f = trace_forest(&g, &roots, dir, 3, usize::MAX);
            assert!(!f.truncated && f.omitted_roots == 0, "方向 {dir:?}");
            assert_eq!(f.entries.len(), 2, "方向 {dir:?}");
            for (entry, &root) in f.entries.iter().zip(roots.iter()) {
                assert_eq!(entry.root, root, "方向 {dir:?}");
                assert_eq!(
                    rows(&entry.nodes),
                    rows(&trace_tree(&g, root, dir, 3)),
                    "方向 {dir:?} 根 {root:?}"
                );
            }
        }
        // Both 双根形态显式锁一下：s2 的 Callees 树（2→3→4）在前，
        // Callers 树（无人调用 s2，仅根）在后。
        let both = trace_forest(&g, &roots, TraceDirection::Both, 3, usize::MAX);
        assert_eq!(
            rows(&both.entries[1].nodes),
            vec![(2, 0, false), (3, 1, false), (4, 2, false), (2, 0, false)]
        );
    }

    /// 空 roots → 空 Vec；根全部不存在 → 同样空；未知根夹在已知根之间
    /// 被跳过，不占条目也不耗预算。
    #[test]
    fn forest_empty_or_unknown_roots_yield_no_entries() {
        let g = graph(3, vec![call(0, 1)]);
        let empty = trace_forest(&g, &[], TraceDirection::Both, 6, 100);
        assert!(empty.entries.is_empty());
        assert!(!empty.truncated && empty.omitted_roots == 0);
        let unknown = trace_forest(
            &g,
            &[SymbolId(9), SymbolId(10)],
            TraceDirection::Callees,
            6,
            100,
        );
        assert!(unknown.entries.is_empty());
        assert!(!unknown.truncated && unknown.omitted_roots == 0);
        let f = trace_forest(
            &g,
            &[SymbolId(9), SymbolId(0), SymbolId(10)],
            TraceDirection::Callees,
            2,
            100,
        );
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].root, SymbolId(0));
        assert_eq!(
            rows(&f.entries[0].nodes),
            vec![(0, 0, false), (1, 1, false)]
        );
    }

    /// 环语义继承：森林里 A→B→A 仍按"根 → 当前栈"路径截断；根 2 经
    /// 2→1→0 绕回 1 才截断，另一根的环不影响本根的路径。
    #[test]
    fn forest_inherits_path_cycle_detection() {
        let g = graph(3, vec![call(0, 1), call(1, 0), call(2, 1)]);
        let f = trace_forest(
            &g,
            &[SymbolId(0), SymbolId(2)],
            TraceDirection::Callees,
            6,
            100,
        );
        assert_eq!(f.entries.len(), 2);
        assert_eq!(
            rows(&f.entries[0].nodes),
            vec![(0, 0, false), (1, 1, false), (0, 2, true)]
        );
        assert_eq!(
            rows(&f.entries[1].nodes),
            vec![(2, 0, false), (1, 1, false), (0, 2, false), (1, 3, true)]
        );
    }
}
