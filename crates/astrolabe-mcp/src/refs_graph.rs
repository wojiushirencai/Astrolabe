//! Index-side helpers for `find_references` (batch 6, stream R).
//!
//! The syntax index is the primary source; the language server is the
//! memory-heavy fallback. This module holds everything `find_references`
//! can answer without a language server:
//!
//! * [`snap_position`]: a `line`/`character` that does not sit on the named
//!   `symbol` is moved onto it (same line, then ±1/±2 lines). The symbol wins.
//! * [`fix_path`]: a missing / non-existent / wrong `path` is replaced by the
//!   file of the symbol's only declaration in the index.
//! * [`graph_references`]: when the index holds exactly one declaration of
//!   the name, its code occurrences in the declaring package / file and in
//!   the files that import it are the answer, with no language server.
//! * [`annotate`]: per hit, the source line, the enclosing function (with
//!   its line range), whether it is the declaration, comment / string
//!   classification and test-file status, so a hit list needs no Reads.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use astrolabe_core::lsp::{Location, Position, Precise, Range};
use astrolabe_core::types::{
    CodeSymbol, Confidence, EdgeKind, FileId, Language, RelPath, SymbolKind,
};

use crate::index::RepoIndex;

/// Lines searched above and below a caller-given line when snapping.
const SNAP_RADIUS: u32 = 2;
/// Cap on the occurrences a graph answer may list before it defers to the
/// language server (a name used this widely is rarely unambiguous).
const MAX_GRAPH_HITS: usize = 2_000;
/// Source line text kept per hit.
pub(crate) const HIT_TEXT_CHARS: usize = 120;

/// Languages whose `find_references` is answered from the index first.
///
/// Chosen by measurement against the language servers
/// (`docs/find-references-graph-first.md`): on cobra / aiproxy / aicon the
/// Go answer recalled every gopls hit (precision ≥ 0.996). TypeScript /
/// JavaScript and Python keep the language server first: members reached
/// through attributes (`self.x.method`, `exports.x`) are not tied to the
/// declaring file by the import graph, which measurably cost recall
/// (Python 0.78) or precision (JS 0.70).
pub(crate) fn graph_first_language(language: Language) -> bool {
    matches!(language, Language::Go)
}

fn read_lines(root: &Path, path: &str) -> Option<Vec<String>> {
    let source = astrolabe_core::text::read_source(&root.join(path)).ok()?;
    Some(
        source
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_owned())
            .collect(),
    )
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Byte offsets of whole-word occurrences of `name` in `line`.
pub(crate) fn word_offsets(line: &str, name: &str) -> Vec<usize> {
    if name.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = line[from..].find(name) {
        let start = from + i;
        let end = start + name.len();
        let before = line[..start].chars().next_back();
        let after = line[end..].chars().next();
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            out.push(start);
        }
        from = start + name.len().max(1);
        while from < line.len() && !line.is_char_boundary(from) {
            from += 1;
        }
        if from >= line.len() {
            break;
        }
    }
    out
}

fn utf16_col(line: &str, byte: usize) -> u32 {
    line[..byte.min(line.len())].encode_utf16().count() as u32
}

fn byte_of_utf16(line: &str, col: u32) -> usize {
    let mut units = 0u32;
    for (i, ch) in line.char_indices() {
        if units >= col {
            return i;
        }
        units += ch.len_utf16() as u32;
    }
    line.len()
}

/// What a hit sits in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HitKind {
    #[default]
    Code,
    Comment,
    Str,
}

/// Comment / string classification of `(line, byte)` hits in one file.
/// `Code` everywhere when the file type has no lexer.
fn classify(path: &str, lines: &[String], hits: &[(usize, usize)]) -> Vec<HitKind> {
    let all_masked = crate::code_mask::code_mask(path, lines, false);
    let comments = crate::code_mask::code_mask(path, lines, true);
    hits.iter()
        .map(|&(line, byte)| {
            let at = |m: &Option<Vec<Vec<bool>>>| {
                m.as_ref()
                    .and_then(|m| m.get(line))
                    .and_then(|l| l.get(byte))
                    .copied()
                    .unwrap_or(true)
            };
            if !at(&comments) {
                HitKind::Comment
            } else if !at(&all_masked) {
                HitKind::Str
            } else {
                HitKind::Code
            }
        })
        .collect()
}

// ---------------------------------------------------------------- snapping

/// Outcome of [`snap_position`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Snap {
    /// Position to query, or `None` to query by name.
    pub position: Option<Position>,
    /// Correction to report, when the position was changed.
    pub note: Option<String>,
}

/// Trust `symbol` over a `line`/`character` that disagrees with it: move to
/// the nearest code occurrence of `symbol` on that line, then ±1 and ±2
/// lines. With no occurrence nearby, drop the position (query by name).
pub(crate) fn snap_position(root: &Path, path: &str, symbol: &str, pos: Position) -> Snap {
    let symbol = symbol.trim();
    let keep = Snap {
        position: Some(pos),
        note: None,
    };
    if symbol.is_empty() {
        return keep;
    }
    let Some(lines) = read_lines(root, path) else {
        return keep;
    };
    let at = astrolabe_core::lsp::queries::identifier_at(root, &RelPath::new(path), pos);
    if at.as_deref() == Some(symbol) {
        return keep;
    }
    let mut order: Vec<i64> = vec![0];
    for d in 1..=i64::from(SNAP_RADIUS) {
        order.push(-d);
        order.push(d);
    }
    for d in order {
        let line_no = i64::from(pos.line) + d;
        let Some(line) = usize::try_from(line_no).ok().and_then(|l| lines.get(l)) else {
            continue;
        };
        let offsets = word_offsets(line, symbol);
        if offsets.is_empty() {
            continue;
        }
        let wanted = byte_of_utf16(line, pos.character);
        let hits: Vec<(usize, usize)> = offsets.iter().map(|&b| (line_no as usize, b)).collect();
        let kinds = classify(path, &lines, &hits);
        let best = offsets
            .iter()
            .zip(kinds.iter())
            .filter(|(_, k)| **k == HitKind::Code)
            .map(|(b, _)| *b)
            .min_by_key(|b| b.abs_diff(wanted))
            .or_else(|| offsets.iter().copied().min_by_key(|b| b.abs_diff(wanted)));
        let Some(byte) = best else { continue };
        let snapped = Position {
            line: line_no as u32,
            character: utf16_col(line, byte),
        };
        let was = at
            .map(|w| format!("是 `{w}`"))
            .unwrap_or_else(|| "不在标识符上".to_string());
        return Snap {
            position: Some(snapped),
            note: Some(format!(
                "坐标已校正：line={} character={} 处{was}，已按 symbol 改为 `{symbol}` 所在的 line={} character={}（0 起）。",
                pos.line, pos.character, snapped.line, snapped.character
            )),
        };
    }
    Snap {
        position: None,
        note: Some(format!(
            "line={} 附近 ±{SNAP_RADIUS} 行没有 `{symbol}`，已忽略 line/character，按名字查询。",
            pos.line
        )),
    }
}

// ----------------------------------------------------------- path fixing

/// Outcome of [`fix_path`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathFix {
    /// The given path is usable as is.
    Keep,
    /// Use this path instead; `note` says why.
    Use { path: String, note: String },
    /// No usable path and several candidates: answer Unknown with `note`.
    Ambiguous { note: String },
}

/// Same-family declarations of exactly `name`, deduplicated per line.
fn declarations<'a>(
    index: &'a RepoIndex,
    name: &str,
    family: Option<Language>,
) -> Vec<&'a CodeSymbol> {
    let graph = index.graph();
    let mut seen = HashSet::new();
    let mut out: Vec<&CodeSymbol> = index
        .names()
        .by_lower(&name.to_lowercase())
        .iter()
        .map(|&i| &graph.symbols[i as usize])
        .filter(|s| s.name == name)
        .filter(|s| {
            family.is_none_or(|lang| {
                index
                    .file(s.file)
                    .and_then(|f| f.language)
                    .is_some_and(|l| same_family(l, lang))
            })
        })
        .filter(|s| seen.insert((s.file, s.start_line)))
        .collect();
    out.sort_by_key(|s| (s.file, s.start_line));
    out
}

/// Languages whose declarations can answer for each other.
pub(crate) fn same_family(a: Language, b: Language) -> bool {
    use Language::*;
    let group = |l: Language| match l {
        Kotlin | Java => 1,
        TypeScript | Tsx | JavaScript | Vue => 2,
        C | Cpp | ObjC | ObjCpp => 3,
        CSharp | VisualBasic => 4,
        _ => 0,
    };
    a == b || (group(a) != 0 && group(a) == group(b))
}

fn contains_word(root: &Path, path: &str, name: &str) -> bool {
    read_lines(root, path)
        .is_some_and(|lines| lines.iter().any(|l| !word_offsets(l, name).is_empty()))
}

/// Replace a missing, non-existent, or symbol-free `path` with the file of
/// the symbol's only declaration (one file) in the index.
pub(crate) fn fix_path(
    index: &RepoIndex,
    root: &Path,
    symbol: &str,
    given: Option<&str>,
) -> PathFix {
    let symbol = symbol.trim();
    if symbol.is_empty() {
        return PathFix::Keep;
    }
    let given = given.map(str::trim).filter(|p| !p.is_empty());
    let exists = given.is_some_and(|p| root.join(p).is_file());
    if exists && contains_word(root, given.unwrap_or_default(), symbol) {
        return PathFix::Keep;
    }
    let hint_lang = given.and_then(|p| Language::from_path(&RelPath::new(p)));
    let mut decls = declarations(index, symbol, hint_lang);
    if decls.is_empty() && hint_lang.is_some() {
        decls = declarations(index, symbol, None);
    }
    let files: BTreeSet<FileId> = decls.iter().map(|s| s.file).collect();
    let why = match given {
        None => "未给 path".to_string(),
        Some(p) if !exists => format!("path `{p}` 不存在"),
        Some(p) => format!("`{p}` 里没有 `{symbol}`"),
    };
    match files.len() {
        0 => PathFix::Keep,
        1 => {
            let s = decls[0];
            let Some(file) = index.file(s.file) else {
                return PathFix::Keep;
            };
            PathFix::Use {
                path: file.path.as_str().to_string(),
                note: format!(
                    "{why}；已按索引改用 `{symbol}` 唯一定义所在文件 {}（定义 @{}:{}）。",
                    file.path.as_str(),
                    file.path.as_str(),
                    s.start_line
                ),
            }
        }
        n if !exists => {
            let list: Vec<String> = decls
                .iter()
                .take(8)
                .filter_map(|s| {
                    index
                        .file(s.file)
                        .map(|f| format!("{}:{}", f.path.as_str(), s.start_line))
                })
                .collect();
            let more = if decls.len() > 8 {
                format!(" …共 {} 处", decls.len())
            } else {
                String::new()
            };
            PathFix::Ambiguous {
                note: format!(
                    "{why}，而索引里 `{symbol}` 定义在 {n} 个文件：{}{more}。请把其中一个作为 path 再查。",
                    list.join("、")
                ),
            }
        }
        _ => PathFix::Keep,
    }
}

// --------------------------------------------------------- graph-first

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Import path of the Go package in `def_dir`: the `module` line of the
/// nearest enclosing `go.mod` plus the rest of the directory.
fn go_import_path(root: &Path, def_dir: &str) -> Option<String> {
    let mut dir = def_dir;
    loop {
        let go_mod = if dir.is_empty() {
            root.join("go.mod")
        } else {
            root.join(dir).join("go.mod")
        };
        if let Ok(text) = std::fs::read_to_string(go_mod) {
            let module = text.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("module")
                    .map(|m| m.trim().trim_matches('"').to_string())
                    .filter(|m| !m.is_empty())
            })?;
            let rest = def_dir[dir.len()..].trim_start_matches('/');
            return Some(if rest.is_empty() {
                module
            } else {
                format!("{module}/{rest}")
            });
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir_of(dir);
    }
}

/// Local names of the Go imports in `lines` that are not the package at
/// `def_dir` (`fmt`, `exec`, an alias…): `name.X` through one of them is
/// another package's `X`, not ours.
fn go_foreign_imports(
    lines: &[String],
    def_import: Option<&str>,
    def_dir: &str,
) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut in_block = false;
    for line in lines {
        let t = line.trim();
        let spec = if in_block {
            if t.starts_with(')') {
                in_block = false;
                continue;
            }
            t
        } else if let Some(rest) = t.strip_prefix("import") {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('(') {
                in_block = true;
                inner.trim()
            } else {
                rest
            }
        } else if t.starts_with("func ") || t.starts_with("type ") || t.starts_with("var ") {
            break;
        } else {
            continue;
        };
        let Some(q) = spec.find('"') else { continue };
        let Some(end) = spec[q + 1..].find('"') else {
            continue;
        };
        let path = &spec[q + 1..q + 1 + end];
        let alias = spec[..q].trim();
        // Unknown module layout: a path ending in the package directory's
        // name is kept as ours (recall over precision).
        let ours = def_import.is_some_and(|d| path == d)
            || (def_import.is_none()
                && !def_dir.is_empty()
                && path.rsplit('/').next() == def_dir.rsplit('/').next());
        if ours || alias == "_" || alias == "." {
            continue;
        }
        let local = if alias.is_empty() {
            let last = path.rsplit('/').next().unwrap_or(path);
            let last = if last.len() > 1
                && last.starts_with('v')
                && last[1..].chars().all(|c| c.is_ascii_digit())
            {
                path.rsplit('/').nth(1).unwrap_or(last)
            } else {
                last
            };
            let last = last.split('.').next().unwrap_or(last);
            last.trim_start_matches("go-").replace('-', "_")
        } else {
            alias.to_string()
        };
        out.insert(local);
    }
    out
}

/// Identifier right before `.` at `byte` (`fmt` in `fmt.Println`).
fn selector_base(line: &str, byte: usize) -> Option<&str> {
    let before = line[..byte].strip_suffix('.')?;
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_ident(*c))
        .last()
        .map(|(i, _)| i)?;
    Some(&before[start..])
}

/// The index's answer to "references of `name`", when it is unambiguous:
/// exactly one same-family declaration of `name` in the repo. `None`
/// otherwise (the caller then asks the language server).
///
/// Go: occurrences in the declaring package (its directory) and, for an
/// exported name, in files whose import edges reach that package. Other
/// languages: the declaring file and the files importing it. Comments and
/// strings are excluded; hits come back sorted by path and position.
pub(crate) fn graph_references(
    index: &RepoIndex,
    root: &Path,
    name: &str,
    origin: &RelPath,
) -> Option<Precise<Vec<Location>>> {
    let language = Language::from_path(origin)?;
    let decls = declarations(index, name, Some(language));
    let [decl] = decls.as_slice() else {
        return None;
    };
    let graph = index.graph();
    let def_file = index.file(decl.file)?;
    let def_path = def_file.path.as_str();
    let def_dir = dir_of(def_path);
    let exported = name.chars().next().is_some_and(char::is_uppercase);
    let go = language == Language::Go;
    // Files whose imports reach the declaration (Go: any file of the package).
    let targets: HashSet<u32> = graph
        .files
        .iter()
        .filter(|f| {
            if go {
                dir_of(f.path.as_str()) == def_dir && f.language == Some(Language::Go)
            } else {
                f.id == decl.file
            }
        })
        .map(|f| f.id.0)
        .collect();
    let importers: HashSet<u32> = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import && targets.contains(&e.to))
        .map(|e| e.from)
        .collect();
    let mut files: Vec<&RelPath> = graph
        .files
        .iter()
        .filter(|f| f.language.is_some_and(|l| same_family(l, language)))
        .filter(|f| {
            if go {
                dir_of(f.path.as_str()) == def_dir || (exported && importers.contains(&f.id.0))
            } else {
                f.id == decl.file || importers.contains(&f.id.0)
            }
        })
        .map(|f| &f.path)
        .collect();
    files.sort();
    let def_import = go.then(|| go_import_path(root, def_dir)).flatten();
    let name_units = name.encode_utf16().count() as u32;
    let mut hits = Vec::new();
    for path in files {
        let Some(lines) = read_lines(root, path.as_str()) else {
            continue;
        };
        let mut found: Vec<(usize, usize)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            for b in word_offsets(line, name) {
                found.push((i, b));
            }
        }
        if found.is_empty() {
            continue;
        }
        let kinds = classify(path.as_str(), &lines, &found);
        let foreign = if go {
            go_foreign_imports(&lines, def_import.as_deref(), def_dir)
        } else {
            HashSet::new()
        };
        for ((line, byte), kind) in found.into_iter().zip(kinds) {
            if kind != HitKind::Code {
                continue;
            }
            if selector_base(&lines[line], byte).is_some_and(|b| foreign.contains(b)) {
                continue;
            }
            let character = utf16_col(&lines[line], byte);
            let start = Position {
                line: line as u32,
                character,
            };
            let end = Position {
                line: line as u32,
                character: character + name_units,
            };
            hits.push(Location {
                path: path.clone(),
                range: Range { start, end },
            });
        }
        if hits.len() > MAX_GRAPH_HITS {
            return None;
        }
    }
    if hits.is_empty() {
        return None;
    }
    let scope = if go {
        if exported {
            format!(
                "包 {}/ 内及导入该包的文件",
                if def_dir.is_empty() { "." } else { def_dir }
            )
        } else {
            format!(
                "包 {}/ 内（未导出名）",
                if def_dir.is_empty() { "." } else { def_dir }
            )
        }
    } else {
        "定义文件及导入它的文件".to_string()
    };
    Some(Precise {
        value: hits,
        confidence: Confidence::Scoped,
        note: Some(format!(
            "索引作答（未启动语言服务器）：`{name}` 全仓唯一定义 @{def_path}:{}；以下是{scope}中的代码出现，已排除注释和字符串。",
            decl.start_line
        )),
    })
}

// ------------------------------------------------------------ annotate

/// Self-contained description of one hit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HitInfo {
    /// Source line, trimmed and cut to [`HIT_TEXT_CHARS`] chars.
    pub text: Option<String>,
    /// Enclosing function / type: name, first and last line (1-based).
    pub enclosing: Option<(String, u32, u32)>,
    /// The hit is the symbol's own declaration.
    pub declaration: bool,
    pub kind: HitKind,
    /// In a test file or test function.
    pub test: bool,
}

fn enclosing_symbol<'a>(symbols: &[&'a CodeSymbol], line1: u32) -> Option<&'a CodeSymbol> {
    let pick = |callable: bool| {
        symbols
            .iter()
            .copied()
            .filter(|s| s.start_line <= line1 && line1 <= s.end_line)
            .filter(|s| s.end_line >= s.start_line + 2 || s.start_line == line1)
            .filter(|s| !callable || matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
            .filter(|s| {
                !matches!(
                    s.kind,
                    SymbolKind::Field | SymbolKind::Variable | SymbolKind::Const
                ) || s.end_line > s.start_line + 2
            })
            .min_by_key(|s| s.end_line - s.start_line)
    };
    pick(true).or_else(|| pick(false))
}

/// Describe `locations` (any source: graph, LSP or text fallback) for
/// rendering. Works without an index (no enclosing / declaration info).
pub(crate) fn annotate(
    index: Option<&RepoIndex>,
    root: &Path,
    symbol: &str,
    locations: &[Location],
) -> Vec<HitInfo> {
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, l) in locations.iter().enumerate() {
        by_file.entry(l.path.as_str()).or_default().push(i);
    }
    let mut out = vec![HitInfo::default(); locations.len()];
    for (path, idxs) in by_file {
        let lines = read_lines(root, path).unwrap_or_default();
        let file_symbols: Vec<&CodeSymbol> = index
            .and_then(|ix| {
                ix.file_id(path)
                    .filter(|id| ix.file(*id).is_some_and(|f| f.path.as_str() == path))
                    .map(|id| (ix, id))
            })
            .map(|(ix, id)| ix.graph().symbols.iter().filter(|s| s.file == id).collect())
            .unwrap_or_default();
        let positions: Vec<(usize, usize)> = idxs
            .iter()
            .map(|&i| {
                let p = locations[i].range.start;
                let line = p.line as usize;
                let byte = lines.get(line).map_or(0, |l| byte_of_utf16(l, p.character));
                (line, byte)
            })
            .collect();
        let kinds = if lines.is_empty() {
            vec![HitKind::Code; idxs.len()]
        } else {
            classify(path, &lines, &positions)
        };
        let file_test = crate::explain::is_test_caller(path, "");
        for ((&i, &(line, byte)), kind) in idxs.iter().zip(positions.iter()).zip(kinds) {
            let line1 = line as u32 + 1;
            let text = lines.get(line).map(|l| {
                let t = l.trim();
                let mut s: String = t.chars().take(HIT_TEXT_CHARS).collect();
                if t.chars().count() > HIT_TEXT_CHARS {
                    s.push('…');
                }
                s
            });
            let enclosing = enclosing_symbol(&file_symbols, line1);
            let declaration = kind == HitKind::Code
                && file_symbols.iter().any(|s| {
                    s.name == symbol
                        && s.start_line <= line1
                        && line1
                            <= s.start_line
                                .saturating_add(3)
                                .min(s.end_line.max(s.start_line))
                        && first_occurrence(&lines, symbol, s.start_line, line1)
                            == Some((line, byte))
                });
            let test = file_test
                || enclosing.is_some_and(|s| crate::explain::is_test_caller(path, &s.name));
            out[i] = HitInfo {
                text,
                enclosing: enclosing.map(|s| (s.name.clone(), s.start_line, s.end_line)),
                declaration,
                kind,
                test,
            };
        }
    }
    out
}

/// First whole-word occurrence of `name` from 1-based line `from` through
/// `to`, as `(0-based line, byte)`.
fn first_occurrence(lines: &[String], name: &str, from: u32, to: u32) -> Option<(usize, usize)> {
    let start = from.saturating_sub(1) as usize;
    let end = to.saturating_sub(1) as usize;
    (start..=end).find_map(|l| {
        lines
            .get(l)
            .and_then(|text| word_offsets(text, name).first().map(|&b| (l, b)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-refs-graph-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    fn index(dir: &Path) -> RepoIndex {
        crate::index::build_with_options(dir, false, 1 << 20).expect("index")
    }

    #[test]
    fn go_foreign_imports_and_selector_base() {
        let lines: Vec<String> = "package p\n\nimport \"fmt\"\nimport (\n\tex \"os/exec\"\n\t\"example.com/m/service\"\n\t\"gopkg.in/yaml.v3\"\n\t\"github.com/x/y/v2\"\n)\n\nfunc F() {}\n"
            .lines()
            .map(str::to_string)
            .collect();
        let f = go_foreign_imports(&lines, Some("example.com/m/service"), "service");
        let want: HashSet<String> = ["fmt", "ex", "yaml", "y"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(f, want);
        // Unknown module: same last segment counts as ours.
        let f = go_foreign_imports(&lines, None, "x/service");
        assert!(!f.contains("service") && f.contains("fmt"));
        assert_eq!(selector_base("\tfmt.Println(x)", 5), Some("fmt"));
        assert_eq!(selector_base("Println(x)", 0), None);
    }

    #[test]
    fn go_import_path_uses_the_nearest_go_mod() {
        let dir = repo(&[
            ("src/go.mod", "module aicon\n\ngo 1.22\n"),
            ("src/server/push/q.go", "package push\n"),
        ]);
        assert_eq!(
            go_import_path(&dir, "src/server/push").as_deref(),
            Some("aicon/server/push")
        );
        assert_eq!(go_import_path(&dir, "src").as_deref(), Some("aicon"));
        assert_eq!(go_import_path(&dir, "other"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn word_offsets_respect_identifier_boundaries() {
        assert_eq!(
            word_offsets("a.Execute() + execute + ExecuteC", "Execute"),
            vec![2]
        );
        assert_eq!(word_offsets("Execute(Execute)", "Execute"), vec![0, 8]);
        assert!(word_offsets("x", "").is_empty());
        assert_eq!(word_offsets("é Execute", "Execute"), vec![3]);
    }

    #[test]
    fn off_by_one_column_snaps_onto_the_symbol() {
        let dir = repo(&[(
            "command.go",
            "package cobra\n\nfunc (c *Command) Execute() error {\n\treturn nil\n}\n",
        )]);
        // 0-based char 17 is the space before `Execute` (18).
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 2,
                character: 17,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 18
            })
        );
        assert!(
            snap.note.as_deref().unwrap().contains("坐标已校正"),
            "{snap:?}"
        );
        // Pointing at `Command` on the same line: symbol wins.
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 2,
                character: 10,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 18
            })
        );
        assert!(
            snap.note.as_deref().unwrap().contains("`Command`"),
            "{snap:?}"
        );
        // Already right: untouched, no note.
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 2,
                character: 20,
            },
        );
        assert_eq!(
            snap,
            Snap {
                position: Some(Position {
                    line: 2,
                    character: 20
                }),
                note: None
            }
        );
        // Two lines off: still found.
        let snap = snap_position(
            &dir,
            "command.go",
            "Execute",
            Position {
                line: 4,
                character: 0,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 18
            })
        );
        // Nowhere near: by name.
        let snap = snap_position(
            &dir,
            "command.go",
            "Missing",
            Position {
                line: 2,
                character: 0,
            },
        );
        assert_eq!(snap.position, None);
        assert!(snap.note.unwrap().contains("按名字查询"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snap_prefers_code_over_comment_occurrence() {
        let dir = repo(&[(
            "a.go",
            "package a\n\nfunc F() { G() } // G here\n\nfunc G() {}\n",
        )]);
        let snap = snap_position(
            &dir,
            "a.go",
            "G",
            Position {
                line: 2,
                character: 25,
            },
        );
        assert_eq!(
            snap.position,
            Some(Position {
                line: 2,
                character: 11
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_wrong_path_is_inferred_from_a_unique_declaration() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("service/channel.go", "package service\n\nfunc ShouldDisableChannel() bool { return false }\n"),
            ("controller/relay.go", "package controller\n\nimport \"example.com/m/service\"\n\nfunc R() { service.ShouldDisableChannel() }\n"),
            ("a/x.go", "package a\n\nfunc Dup() {}\n"),
            ("b/y.go", "package b\n\nfunc Dup() {}\n"),
        ]);
        let ix = index(&dir);
        let fix = fix_path(&ix, &dir, "ShouldDisableChannel", None);
        assert!(
            matches!(&fix, PathFix::Use { path, .. } if path == "service/channel.go"),
            "{fix:?}"
        );
        let fix = fix_path(
            &ix,
            &dir,
            "ShouldDisableChannel",
            Some("relay/channel/disable.go"),
        );
        match fix {
            PathFix::Use { path, note } => {
                assert_eq!(path, "service/channel.go");
                assert!(note.contains("不存在"), "{note}");
            }
            other => panic!("{other:?}"),
        }
        // An existing file that uses the symbol is kept.
        assert_eq!(
            fix_path(
                &ix,
                &dir,
                "ShouldDisableChannel",
                Some("controller/relay.go")
            ),
            PathFix::Keep
        );
        // Existing file without the symbol: unique definition wins.
        assert!(matches!(
            fix_path(&ix, &dir, "ShouldDisableChannel", Some("a/x.go")),
            PathFix::Use { .. }
        ));
        // Ambiguous without a path: candidates listed.
        match fix_path(&ix, &dir, "Dup", None) {
            PathFix::Ambiguous { note } => assert!(
                note.contains("a/x.go:3") && note.contains("b/y.go:3"),
                "{note}"
            ),
            other => panic!("{other:?}"),
        }
        // Unknown names: keep (the server path reports it).
        assert_eq!(fix_path(&ix, &dir, "Nope", None), PathFix::Keep);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn go_graph_references_scope_to_package_and_importers() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("service/channel.go", "package service\n\n// ShouldDisableChannel decides.\nfunc ShouldDisableChannel() bool { return false }\n\nfunc other() { _ = ShouldDisableChannel() }\n"),
            ("controller/relay.go", "package controller\n\nimport \"example.com/m/service\"\n\nfunc R() {\n\tif service.ShouldDisableChannel() {\n\t\tprintln(\"ShouldDisableChannel\")\n\t}\n}\n"),
            // Same name, package that does not import service: not ours.
            ("controller/log.go", "package controller\n\nimport (\n\tsvc \"example.com/m/service\"\n\tstrs \"example.com/ext/strs\"\n)\n\nfunc L() { strs.ShouldDisableChannel(); svc.ShouldDisableChannel() }\n"),
            ("other/z.go", "package other\n\nimport \"ext.com/x\"\n\nfunc Z(t *x.T) { t.ShouldDisableChannel() }\n"),
        ]);
        let ix = index(&dir);
        let r = graph_references(
            &ix,
            &dir,
            "ShouldDisableChannel",
            &RelPath::new("service/channel.go"),
        );
        let r = r.expect("unique declaration");
        let got: Vec<String> = r
            .value
            .iter()
            .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
            .collect();
        // `strs.ShouldDisableChannel` is another package's; `svc.` is ours.
        assert_eq!(
            got,
            vec![
                "controller/log.go:8",
                "controller/relay.go:6",
                "service/channel.go:4",
                "service/channel.go:6"
            ]
        );
        assert_eq!(r.confidence, Confidence::Scoped);
        assert!(r.note.unwrap().contains("未启动语言服务器"));
        // Ambiguous: two declarations → defer to the language server.
        let dir2 = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("a/x.go", "package a\n\nfunc Dup() {}\n"),
            ("b/y.go", "package b\n\nfunc Dup() {}\n"),
        ]);
        let ix2 = index(&dir2);
        assert!(graph_references(&ix2, &dir2, "Dup", &RelPath::new("a/x.go")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn annotate_marks_declaration_enclosing_comment_string_and_tests() {
        let dir = repo(&[
            ("go.mod", "module example.com/m\n"),
            ("p/a.go", "package p\n\n// Run starts.\nfunc Run() {\n}\n\nfunc caller() {\n\tRun()\n\ts := \"Run\"\n\t_ = s\n}\n"),
            ("p/a_test.go", "package p\n\nimport \"testing\"\n\nfunc TestRun(t *testing.T) {\n\tRun()\n}\n"),
        ]);
        let ix = index(&dir);
        let at = |path: &str, line: u32, character: u32| Location {
            path: RelPath::new(path),
            range: Range {
                start: Position { line, character },
                end: Position {
                    line,
                    character: character + 3,
                },
            },
        };
        let locs = vec![
            at("p/a.go", 2, 3),
            at("p/a.go", 3, 5),
            at("p/a.go", 7, 1),
            at("p/a.go", 8, 7),
            at("p/a_test.go", 5, 1),
        ];
        let info = annotate(Some(&ix), &dir, "Run", &locs);
        assert_eq!(info[0].kind, HitKind::Comment);
        assert!(info[1].declaration, "{:?}", info[1]);
        assert_eq!(info[1].text.as_deref(), Some("func Run() {"));
        assert!(!info[2].declaration);
        assert_eq!(info[2].enclosing, Some(("caller".to_string(), 7, 11)));
        assert_eq!(info[3].kind, HitKind::Str);
        assert!(info[4].test && !info[2].test);
        assert_eq!(
            info[4].enclosing.as_ref().map(|e| e.0.as_str()),
            Some("TestRun")
        );
        // Without an index: text and kinds still there.
        let bare = annotate(None, &dir, "Run", &locs);
        assert_eq!(bare[0].kind, HitKind::Comment);
        assert!(bare[2].enclosing.is_none() && bare[2].text.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
