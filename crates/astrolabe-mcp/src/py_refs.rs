//! Python name binding for index-side `find_references` (batch 7, stream PY).
//!
//! The import graph is file-level: `from solidlsp import SolidLanguageServer`
//! is an edge to `solidlsp/__init__.py`, not to `ls.py` where the class lives,
//! and `from m import A as B` uses `B` from then on. This module recovers the
//! name level, by reading the few files involved at query time:
//!
//! * [`python_scope`]: the files that can reach a declaration, with the local
//!   names it goes by in each. Re-exports (`from .m import A` / `import *` /
//!   `__all__` in a module, typically `__init__.py`) are followed through the
//!   re-exporting module's importers, up to [`MAX_REEXPORT_DEPTH`] levels;
//!   `from m import A as B` adds `B` in that file (and is re-exported as `B`).
//!   `import m as y` needs nothing: `y.A` still contains the word `A`.
//! * [`type_string_at`]: a hit inside a string literal that is a type
//!   expression in annotation position (`x: "Foo"`, `-> "Foo"`,
//!   `List["Foo"]`, `cast("Foo", v)`, `TypeVar("T", bound="Foo")`) is code,
//!   which is how pyright counts forward references. Docstrings, f-string
//!   text and ordinary string values are not.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::Path;

use astrolabe_core::types::{EdgeKind, FileId, Language};

use crate::index::RepoIndex;

/// Re-export chains followed at most this deep.
pub(crate) const MAX_REEXPORT_DEPTH: usize = 8;

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && !s.starts_with(|c: char| c.is_ascii_digit()) && s.chars().all(is_ident_char)
}

/// One `from M import …` statement (possibly spanning lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FromImport {
    pub module: String,
    /// `(imported name, alias)`.
    pub names: Vec<(String, Option<String>)>,
    pub star: bool,
    /// At module level (top level, or under `if` / `try` / `else` /
    /// `except` at top level, e.g. `if TYPE_CHECKING:`), so it binds a
    /// module attribute that importers can import again.
    pub module_level: bool,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Whether the statement at `idx` (indent > 0) sits in module-level
/// control flow rather than in a `def` / `class` body.
fn module_level_at(lines: &[String], idx: usize) -> bool {
    let mut want = indent_of(&lines[idx]);
    if want == 0 {
        return true;
    }
    for l in lines[..idx].iter().rev() {
        let t = l.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let ind = indent_of(l);
        if ind >= want {
            continue;
        }
        let head = t.split(|c: char| !is_ident_char(c)).next().unwrap_or("");
        if !matches!(
            head,
            "if" | "elif" | "else" | "try" | "except" | "finally" | "with"
        ) {
            return false;
        }
        if ind == 0 {
            return true;
        }
        want = ind;
    }
    true
}

/// `from M import …` statements in `lines`. Handles parenthesised and
/// backslash-continued name lists and `;`-free one-liners.
pub(crate) fn from_imports(lines: &[String]) -> Vec<FromImport> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        let start = i;
        i += 1;
        let Some(rest) = t.strip_prefix("from ") else {
            continue;
        };
        let Some((module, names)) = rest.split_once(" import ") else {
            continue;
        };
        let mut names = names.split('#').next().unwrap_or("").trim().to_string();
        if names.starts_with('(') {
            while !names.contains(')') && i < lines.len() {
                names.push(' ');
                names.push_str(lines[i].split('#').next().unwrap_or(""));
                i += 1;
            }
        } else {
            while names.ends_with('\\') && i < lines.len() {
                names.pop();
                names.push(' ');
                names.push_str(lines[i].split('#').next().unwrap_or("").trim());
                i += 1;
            }
        }
        let names = names.trim().trim_start_matches('(');
        let names = names.split(')').next().unwrap_or("");
        let mut fi = FromImport {
            module: module.trim().to_string(),
            names: Vec::new(),
            star: false,
            module_level: module_level_at(lines, start),
        };
        for part in names.split(',') {
            let part = part.trim();
            if part == "*" {
                fi.star = true;
                continue;
            }
            let mut words = part.split_whitespace();
            let Some(n) = words.next().filter(|n| is_ident(n)) else {
                continue;
            };
            let alias = match (words.next(), words.next()) {
                (Some("as"), Some(a)) if is_ident(a) => Some(a.to_string()),
                _ => None,
            };
            fi.names.push((n.to_string(), alias));
        }
        out.push(fi);
    }
    out
}

/// Names listed in the module's `__all__` (`=`, `+=`, `.extend` / `.append`).
pub(crate) fn dunder_all(lines: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        i += 1;
        if !t.starts_with("__all__") || indent_of(&lines[i - 1]) != 0 {
            continue;
        }
        let mut text = t.to_string();
        let mut depth: i32 =
            text.matches(['[', '(']).count() as i32 - text.matches([']', ')']).count() as i32;
        while depth > 0 && i < lines.len() {
            text.push(' ');
            text.push_str(&lines[i]);
            depth += lines[i].matches(['[', '(']).count() as i32
                - lines[i].matches([']', ')']).count() as i32;
            i += 1;
        }
        for (k, chunk) in text.split(['"', '\'']).enumerate() {
            if k % 2 == 1 && is_ident(chunk) {
                out.insert(chunk.to_string());
            }
        }
    }
    out
}

/// Exported names under which `module` (whose lines are `lines`) re-exports
/// `name`: `name` itself (`from .m import name`, `import *`, `__all__`), or
/// an alias (`from .m import name as other`).
fn reexported_as(lines: &[String], name: &str, from_module: &str) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for fi in from_imports(lines) {
        if !fi.module_level {
            continue;
        }
        let last = fi
            .module
            .trim_start_matches('.')
            .rsplit('.')
            .next()
            .unwrap_or("");
        if fi.star && (last == from_module || from_module.is_empty()) {
            out.insert(name.to_string());
        }
        for (n, alias) in &fi.names {
            if n == name {
                out.insert(alias.clone().unwrap_or_else(|| name.to_string()));
            }
        }
    }
    if dunder_all(lines).contains(name) {
        out.insert(name.to_string());
    }
    out.into_iter().collect()
}

/// Local aliases of `name` bound by `from … import name as alias` anywhere
/// in the file.
pub(crate) fn local_aliases(lines: &[String], name: &str) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for fi in from_imports(lines) {
        for (n, alias) in fi.names {
            if n == name {
                if let Some(a) = alias.filter(|a| a != name) {
                    out.insert(a);
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Files that reach the Python declaration of `name` in `decl_file`, with
/// the names to search in each (`name`, plus aliases / re-exported names;
/// an empty set when the file binds the name from another module). The
/// second value lists the re-exporting modules that were followed.
///
/// Importers of a module are its import-graph importers plus the files
/// that import it as a submodule (`from pkg import mod`), which the graph
/// may point at the package instead (a namespace package resolves to its
/// first file).
pub(crate) fn python_scope(
    index: &RepoIndex,
    root: &Path,
    decl_file: FileId,
    name: &str,
) -> (BTreeMap<u32, BTreeSet<String>>, Vec<String>) {
    let graph = index.graph();
    let mut importers_of: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for e in &graph.edges {
        if e.kind == EdgeKind::Import && e.from != e.to {
            importers_of.entry(e.to).or_default().push(e.from);
        }
    }
    let py_path = |id: u32| {
        index
            .file(FileId(id))
            .filter(|f| matches!(f.language, Some(Language::Python)))
            .map(|f| f.path.as_str().to_string())
    };
    let dir_of = |p: &str| {
        p.rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_string())
    };
    let mut by_dir: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for f in &graph.files {
        if matches!(f.language, Some(Language::Python)) {
            by_dir
                .entry(dir_of(f.path.as_str()))
                .or_default()
                .push(f.id.0);
        }
    }
    let mut scope: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    let mut via: Vec<String> = Vec::new();
    let mut seen: HashSet<(u32, String)> = HashSet::new();
    let mut queue: VecDeque<(u32, String, usize)> = VecDeque::new();
    scope
        .entry(decl_file.0)
        .or_default()
        .insert(name.to_string());
    seen.insert((decl_file.0, name.to_string()));
    queue.push_back((decl_file.0, name.to_string(), 0));
    while let Some((file, n, depth)) = queue.pop_front() {
        let Some(file_path) = py_path(file) else {
            continue;
        };
        let from_module = module_name(&file_path);
        let file_dir = dir_of(&file_path);
        let pkg = file_dir.rsplit('/').next().unwrap_or("").to_string();
        // (importer, needs the `from pkg import mod` check)
        let mut importers: Vec<(u32, bool)> = importers_of
            .get(&file)
            .map(|v| v.iter().map(|&g| (g, false)).collect())
            .unwrap_or_default();
        if !file_path.ends_with("__init__.py") && !file_path.ends_with("__init__.pyi") {
            let direct: HashSet<u32> = importers.iter().map(|(g, _)| *g).collect();
            let mut extra: BTreeSet<u32> = BTreeSet::new();
            for sib in by_dir.get(&file_dir).into_iter().flatten() {
                for &g in importers_of.get(sib).into_iter().flatten() {
                    if g != file && !direct.contains(&g) {
                        extra.insert(g);
                    }
                }
            }
            importers.extend(extra.into_iter().map(|g| (g, true)));
        }
        for (g, check_submodule) in importers {
            let Some(path) = py_path(g) else { continue };
            let Some(lines) = crate::refs_graph::read_lines(root, &path) else {
                continue;
            };
            let fis = from_imports(&lines);
            if check_submodule
                && !fis.iter().any(|fi| {
                    let last = fi
                        .module
                        .trim_start_matches('.')
                        .rsplit('.')
                        .next()
                        .unwrap_or("");
                    let same_pkg = last == pkg
                        || (last.is_empty()
                            && fi.module.starts_with('.')
                            && dir_of(&path) == file_dir);
                    same_pkg && fi.names.iter().any(|(m, _)| *m == from_module)
                })
            {
                continue;
            }
            // `from elsewhere import n` (and no import of it from `file`):
            // this file's `n` is another module's.
            let binders: Vec<&FromImport> = fis
                .iter()
                .filter(|fi| fi.names.iter().any(|(m, _)| *m == n))
                .collect();
            let foreign = !binders.is_empty()
                && !binders.iter().any(|fi| {
                    let last = fi
                        .module
                        .trim_start_matches('.')
                        .rsplit('.')
                        .next()
                        .unwrap_or("");
                    last == from_module || last.is_empty()
                });
            let names = scope.entry(g).or_default();
            if foreign {
                continue;
            }
            names.insert(n.clone());
            for a in local_aliases(&lines, &n) {
                names.insert(a);
            }
            if depth + 1 > MAX_REEXPORT_DEPTH {
                continue;
            }
            for e in reexported_as(&lines, &n, &from_module) {
                if seen.insert((g, e.clone())) {
                    if importers_of.contains_key(&g) && !via.contains(&path) {
                        via.push(path.clone());
                    }
                    queue.push_back((g, e, depth + 1));
                }
            }
        }
    }
    (scope, via)
}

/// Last dotted segment of the module at `path` (`ls` for `pkg/ls.py`,
/// `pkg` for `pkg/__init__.py`).
fn module_name(path: &str) -> String {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    if stem == "__init__" {
        parts.next().unwrap_or("").to_string()
    } else {
        stem.to_string()
    }
}

// ------------------------------------------------- string type annotations

/// Typing heads whose subscripts are type expressions even when lower-case.
const LOWER_TYPE_HEADS: &[&str] = &["list", "dict", "set", "frozenset", "tuple", "type"];

/// [`type_string_at`] with the file around it: a string that opens its
/// line inside a bracket left open on earlier lines (`Union[\n "Foo",`)
/// takes that bracket as its context, and an `__all__ = [...]` entry is a
/// reference too (pyright counts both).
pub(crate) fn type_string_in(lines: &[String], line: usize, byte: usize) -> bool {
    let Some(text) = lines.get(line) else {
        return false;
    };
    if type_string_at(text, byte) {
        return true;
    }
    if in_dunder_all(lines, line) {
        return true;
    }
    // Only a string that starts the line (after indentation) borrows the
    // previous lines' open bracket.
    let lead = text.trim_start();
    if !(lead.starts_with('"') || lead.starts_with('\'')) {
        return false;
    }
    let Some(head) = open_bracket_before(lines, line) else {
        return false;
    };
    let indent = text.len() - lead.len();
    let mut synth = String::new();
    synth.push_str(&head);
    synth.push('[');
    let shift = synth.len() as isize - indent as isize;
    synth.push_str(lead);
    let b = byte as isize + shift;
    b > 0 && type_string_at(&synth, b as usize)
}

/// Head identifier of the innermost `[` left open by up to 30 previous
/// lines, when that `[` is the innermost open bracket.
fn open_bracket_before(lines: &[String], line: usize) -> Option<String> {
    let mut depth = 0i32;
    for l in lines[line.saturating_sub(30)..line].iter().rev() {
        let code = l.split('#').next().unwrap_or("");
        for (i, c) in code.char_indices().rev() {
            match c {
                ')' | ']' | '}' => depth += 1,
                '(' | '{' if depth == 0 => return None,
                '[' if depth == 0 => {
                    let head: String = code[..i]
                        .trim_end()
                        .chars()
                        .rev()
                        .take_while(|c| is_ident_char(*c) || *c == '.')
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    return Some(head);
                }
                '(' | '[' | '{' => depth -= 1,
                _ => {}
            }
        }
    }
    None
}

/// The line is inside a top-level `__all__ = [ … ]` / `( … )`.
fn in_dunder_all(lines: &[String], line: usize) -> bool {
    for k in (line.saturating_sub(200)..=line).rev() {
        let l = &lines[k];
        if l.starts_with("__all__") {
            let mut depth = 0i32;
            for t in &lines[k..line] {
                depth +=
                    t.matches(['[', '(']).count() as i32 - t.matches([']', ')']).count() as i32;
            }
            // Same line, or still inside the brackets opened at `k`.
            return k == line || depth > 0;
        }
        if k != line
            && !l.is_empty()
            && !l.starts_with(char::is_whitespace)
            && !l.starts_with(['"', '\'', ']', ')', '#'])
        {
            return false;
        }
    }
    false
}

/// Whether a string-classified hit at `byte` in `line` is a forward
/// reference: the enclosing one-line string literal (no prefix, not
/// triple-quoted) holds only a type expression and sits in an annotation
/// position.
pub(crate) fn type_string_at(line: &str, byte: usize) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    // Innermost-bracket stack: (open char, head identifier before it).
    let mut stack: Vec<(u8, String)> = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b'#' => return false,
            b'(' | b'[' | b'{' => {
                let head: String = line[..i]
                    .trim_end()
                    .chars()
                    .rev()
                    .take_while(|c| is_ident_char(*c) || *c == '.')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                stack.push((c, head));
                i += 1;
            }
            b')' | b']' | b'}' => {
                stack.pop();
                i += 1;
            }
            b'"' | b'\'' => {
                let triple = b.len() >= i + 3 && b[i + 1] == c && b[i + 2] == c;
                let open = i;
                let body = if triple { i + 3 } else { i + 1 };
                let mut j = body;
                let close = loop {
                    if j >= b.len() {
                        break None;
                    }
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == c
                        && (!triple || (b.get(j + 1) == Some(&c) && b.get(j + 2) == Some(&c)))
                    {
                        break Some(j);
                    }
                    j += 1;
                };
                let end = close.map_or(b.len(), |e| if triple { e + 3 } else { e + 1 });
                if byte > open && byte < end {
                    let Some(close) = close else { return false };
                    if triple {
                        return false;
                    }
                    // A prefix (`f`, `r`, `b`, `u`) means not a plain annotation.
                    if open > 0 && (b[open - 1] as char).is_alphanumeric() {
                        return false;
                    }
                    let content = &line[body..close];
                    if !type_shaped(content) {
                        return false;
                    }
                    return annotation_position(&line[..open], stack.last());
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    false
}

/// `^[\w.\[\], |]+$` with at least one identifier character.
fn type_shaped(s: &str) -> bool {
    !s.trim().is_empty()
        && s.chars().any(is_ident_char)
        && s.chars()
            .all(|c| is_ident_char(c) || matches!(c, '.' | '[' | ']' | ',' | ' ' | '|'))
}

fn annotation_position(before: &str, inner: Option<&(u8, String)>) -> bool {
    let before = before.trim_end();
    let head_is_type = |h: &str| {
        let last = h.rsplit('.').next().unwrap_or(h);
        last.starts_with(|c: char| c.is_ascii_uppercase()) || LOWER_TYPE_HEADS.contains(&last)
    };
    match inner {
        Some((b'[', head)) if head_is_type(head) => {
            return before.ends_with('[') || before.ends_with(',') || before.ends_with('|');
        }
        Some((b'{', _)) => return false,
        Some((b'(', head)) => {
            let last = head.rsplit('.').next().unwrap_or(head);
            if last == "cast" && before.ends_with('(') {
                return true;
            }
            if before.ends_with("bound=") {
                return true;
            }
        }
        _ => {}
    }
    if before.ends_with("->") {
        return true;
    }
    if let Some(b) = before.strip_suffix(':') {
        // `x: "Foo"` / `def f(x: "Foo")`; not a slice `a[1:"x"]`, a
        // lambda body or a `dict(...)`-style key, and not `else:`-like.
        let b = b.trim_end();
        let prev = b.rsplit(|c: char| !is_ident_char(c)).next().unwrap_or("");
        return is_ident(prev)
            && !matches!(prev, "lambda" | "else" | "try" | "finally")
            && !b.ends_with("lambda");
    }
    if before.ends_with('|') {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::types::RelPath;

    fn ls(src: &str) -> Vec<String> {
        src.lines().map(str::to_string).collect()
    }

    fn at(line: &str, word: &str) -> bool {
        type_string_at(line, line.find(word).unwrap())
    }

    #[test]
    fn string_annotations_are_code() {
        assert!(at("    def f(self, x: \"Foo\") -> None:", "Foo"));
        assert!(at("x: \"Foo\" = None", "Foo"));
        assert!(at("def f() -> \"Foo\":", "Foo"));
        assert!(at("def f() -> 'mod.Foo':", "Foo"));
        assert!(at("    items: List[\"Foo\"] = []", "Foo"));
        assert!(at("    items: dict[str, \"Foo\"]", "Foo"));
        assert!(at("    a: Optional[\"Foo | None\"]", "Foo"));
        assert!(at("v = cast(\"Foo\", obj)", "Foo"));
        assert!(at("T = TypeVar(\"T\", bound=\"Foo\")", "Foo"));
        assert!(at(
            "    def __init__(self, retriever: \"LanguageServerSymbolRetriever\", x: int):",
            "LanguageServerSymbolRetriever"
        ));
    }

    #[test]
    fn other_strings_stay_strings() {
        // Plain values, dict values / keys, subscripts, prefixes, prose.
        assert!(!at("name = \"Foo\"", "Foo"));
        assert!(!at("d = {\"kind\": \"Foo\"}", "Foo"));
        assert!(!at("x = registry[\"Foo\"]", "Foo"));
        assert!(!at("log.info(f\"made {x}: Foo\")", "Foo"));
        assert!(!at("    \"\"\"Return a Foo.\"\"\"", "Foo"));
        assert!(!at("    \"\"\":param x: \"Foo\" \"\"\"", "Foo"));
        assert!(!at("raise ValueError(\"Foo is broken\")", "Foo"));
        assert!(!at("x: str = \"Foo\"", "Foo"));
        assert!(!at("x: \"Foo bar baz!\"", "Foo"));
        assert!(!at("    # x: \"Foo\"", "Foo"));
    }

    #[test]
    fn multiline_annotations_and_dunder_all_entries() {
        let src = ls(concat!(
            "class A:\n",
            "    sel: list[\n",
            "        Union[\n",
            "            \"Foo\",\n",
            "            str,\n",
            "        ]\n",
            "    ]\n",
            "    d = {\n",
            "        \"Foo\": 1,\n",
            "    }\n",
            "__all__ = [\n",
            "    \"Foo\",\n",
            "]\n",
            "x = [\n",
            "    \"Foo\",\n",
            "]\n",
            "__all__ = [\"Foo\"]\n",
        ));
        let col = |l: usize| src[l].find("Foo").unwrap();
        assert!(type_string_in(&src, 3, col(3)), "Union[ on the line above");
        assert!(!type_string_in(&src, 8, col(8)), "dict key");
        assert!(type_string_in(&src, 11, col(11)), "__all__ entry");
        assert!(!type_string_in(&src, 14, col(14)), "plain list element");
        assert!(type_string_in(&src, 16, col(16)), "one-line __all__");
    }

    #[test]
    fn from_imports_parse_aliases_parens_and_level() {
        let src = ls(concat!(
            "from .ls import SolidLanguageServer\n",
            "from .a import (\n    B,\n    C as D,  # c\n)\n",
            "from x import \\\n    E\n",
            "from .star import *\n",
            "if TYPE_CHECKING:\n    from .symbol import F\n",
            "def f():\n    from .late import G\n",
        ));
        let fis = from_imports(&src);
        assert_eq!(fis.len(), 6);
        assert_eq!(
            fis[0].names,
            vec![("SolidLanguageServer".to_string(), None)]
        );
        assert_eq!(
            fis[1].names,
            vec![
                ("B".to_string(), None),
                ("C".to_string(), Some("D".to_string()))
            ]
        );
        assert_eq!(fis[2].names, vec![("E".to_string(), None)]);
        assert!(fis[3].star);
        assert!(fis[4].module_level, "TYPE_CHECKING import is module level");
        assert!(!fis[5].module_level, "function-local import is not");
        assert_eq!(local_aliases(&src, "C"), vec!["D".to_string()]);
        assert_eq!(reexported_as(&src, "C", "a"), vec!["D".to_string()]);
        assert_eq!(
            reexported_as(&src, "G", "star"),
            vec!["G".to_string()],
            "star from its module"
        );
        assert!(
            reexported_as(&src, "G", "other").is_empty(),
            "star from another module"
        );
        assert_eq!(module_name("pkg/__init__.py"), "pkg");
        assert_eq!(module_name("pkg/ls.py"), "ls");
    }

    #[test]
    fn dunder_all_lists() {
        let src = ls("__all__ = [\n    \"A\",\n    'B',\n]\n__all__ += [\"C\"]\n");
        let all = dunder_all(&src);
        assert!(all.contains("A") && all.contains("B") && all.contains("C"));
        assert!(reexported_as(&src, "B", "m") == vec!["B".to_string()]);
        assert!(reexported_as(&src, "Z", "m").is_empty());
    }

    fn repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-py-refs-{}-{}",
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

    type Hit = (String, u32, u32, u32);

    fn refs(dir: &Path, name: &str, origin: &str) -> (Vec<Hit>, String) {
        let index = crate::index::build_with_options(dir, false, 1 << 20).expect("index");
        let r = crate::refs_graph::graph_references(
            &index,
            dir,
            name,
            &astrolabe_core::types::RelPath::new(origin),
        )
        .expect("graph answer");
        let mut v: Vec<Hit> = r
            .value
            .iter()
            .map(|l| {
                (
                    l.path.as_str().to_string(),
                    l.range.start.line + 1,
                    l.range.start.character,
                    l.range.end.character - l.range.start.character,
                )
            })
            .collect();
        v.sort();
        (v, r.note.unwrap_or_default())
    }

    #[test]
    fn graph_follows_init_reexports_aliases_and_string_annotations() {
        let dir = repo(&[
            ("pkg/__init__.py", "from .ls import Server\nfrom .ls import Server as Srv\n"),
            ("pkg/ls.py", "class Server:\n    \"\"\"A Server.\"\"\"\n    pass\n"),
            (
                "app/use.py",
                "from pkg import Server\n\n\ndef run(s: \"Server\") -> \"list[Server]\":\n    return [Server()]\n",
            ),
            (
                "app/alias.py",
                "from pkg.ls import Server as S\n\nx = S()\nname = \"Server\"\n",
            ),
            ("app/viaalias.py", "from pkg import Srv\n\ny = Srv()\n"),
            (
                "app/tc.py",
                "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    from pkg.ls import Server\n\n\ndef f(a: \"Server\"):\n    pass\n",
            ),
            ("app/modalias.py", "import pkg.ls as m\n\nz = m.Server()\n"),
        ]);
        let (hits, note) = refs(&dir, "Server", "pkg/ls.py");
        let want: Vec<Hit> = [
            ("app/alias.py", 1, 19, 6),
            ("app/alias.py", 1, 29, 1),
            ("app/alias.py", 3, 4, 1),
            ("app/modalias.py", 3, 6, 6),
            ("app/tc.py", 3, 23, 6),
            ("app/tc.py", 6, 10, 6),
            ("app/use.py", 1, 16, 6),
            ("app/use.py", 4, 12, 6),
            ("app/use.py", 4, 30, 6),
            ("app/use.py", 5, 12, 6),
            ("app/viaalias.py", 1, 16, 3),
            ("app/viaalias.py", 3, 4, 3),
            ("pkg/__init__.py", 1, 16, 6),
            ("pkg/__init__.py", 2, 16, 6),
            ("pkg/__init__.py", 2, 26, 3),
            ("pkg/ls.py", 1, 6, 6),
        ]
        .into_iter()
        .map(|(p, l, c, n)| (p.to_string(), l, c, n))
        .collect();
        assert_eq!(hits, want, "note: {note}");
        assert!(
            note.contains("转导出模块") && note.contains("pkg/__init__.py"),
            "{note}"
        );
        assert!(note.contains("`S`") && note.contains("`Srv`"), "{note}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn submodule_imports_and_foreign_same_name() {
        let dir = repo(&[
            ("nsp/aaa.py", "X = 1\n"),
            ("nsp/types.py", "class Item:\n    pass\n"),
            (
                "user.py",
                "from nsp import types\n\n\ndef f(i: \"types.Item\") -> types.Item:\n    pass\n",
            ),
            ("other.py", "class Thing:\n    pass\n"),
            (
                "shadow.py",
                "from nsp import types\nfrom ext.lib import Item\n\ny = Item()\n",
            ),
        ]);
        let (hits, _) = refs(&dir, "Item", "nsp/types.py");
        let got: Vec<(&str, u32)> = hits.iter().map(|h| (h.0.as_str(), h.1)).collect();
        assert_eq!(
            got,
            vec![("nsp/types.py", 1), ("user.py", 4), ("user.py", 4)],
            "{hits:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dunder_all_reexport_reaches_package_importers() {
        let dir = repo(&[
            (
                "lib/__init__.py",
                "from lib import core\nfrom lib.core import *\n__all__ = [\"Engine\"]\n",
            ),
            (
                "lib/core.py",
                "__all__ = [\"Engine\"]\n\n\ndef Engine():\n    pass\n",
            ),
            ("main.py", "import lib\n\nlib.Engine()\n"),
        ]);
        let (hits, _) = refs(&dir, "Engine", "lib/core.py");
        let files: BTreeSet<&str> = hits.iter().map(|h| h.0.as_str()).collect();
        assert!(files.contains("main.py"), "{hits:?}");
        assert!(hits.contains(&("main.py".to_string(), 3, 4, 6)), "{hits:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Graph-side dump for the Python recall re-measure (batch 7). Ignored;
    /// starts no language server. `PY_RECALL_REPO`, `PY_RECALL_SAMPLE=n`
    /// (same sampling as `live_backend::recall_harness`) and/or
    /// `PY_RECALL_CASES=path:sym,…`; writes JSON lines to `PY_RECALL_OUT`
    /// with the new answer and the batch-6 rule's answer (declaring file +
    /// direct importers, the bare name only, string annotations excluded).
    #[test]
    #[ignore = "needs a corpus (PY_RECALL_REPO)"]
    fn dump_graph_refs_for_recall() {
        use std::io::Write;
        let Some(root) = std::env::var_os("PY_RECALL_REPO").map(std::path::PathBuf::from) else {
            return;
        };
        let index = crate::index::build(&root).expect("index");
        let g = index.graph();
        let mut cases: Vec<(String, String)> = Vec::new();
        if let Ok(n) = std::env::var("PY_RECALL_SAMPLE") {
            let n: usize = n.parse().unwrap();
            let mut count: std::collections::HashMap<&str, usize> = Default::default();
            for s in &g.symbols {
                *count.entry(s.name.as_str()).or_default() += 1;
            }
            let mut pool: Vec<(String, String)> = Vec::new();
            for s in &g.symbols {
                let Some(f) = index.file(s.file) else {
                    continue;
                };
                let p = f.path.as_str();
                if !p.ends_with(".py")
                    || crate::explain::is_test_caller(p, &s.name)
                    || count[s.name.as_str()] != 1
                    || s.name.len() < 4
                    || !matches!(
                        s.kind,
                        astrolabe_core::SymbolKind::Function
                            | astrolabe_core::SymbolKind::Method
                            | astrolabe_core::SymbolKind::Class
                            | astrolabe_core::SymbolKind::Interface
                            | astrolabe_core::SymbolKind::Struct
                            | astrolabe_core::SymbolKind::Type
                    )
                {
                    continue;
                }
                pool.push((p.to_string(), s.name.clone()));
            }
            pool.sort();
            let step = (pool.len() / n.max(1)).max(1);
            cases.extend(pool.into_iter().step_by(step).take(n));
        }
        if let Ok(list) = std::env::var("PY_RECALL_CASES") {
            for c in list.split(',').filter(|c| !c.is_empty()) {
                let (p, s) = c.rsplit_once(':').expect("path:sym");
                cases.push((p.to_string(), s.to_string()));
            }
        }
        let out_path = std::env::var("PY_RECALL_OUT").expect("PY_RECALL_OUT");
        let mut out = std::fs::File::create(out_path).unwrap();
        for (path, sym) in &cases {
            let decl = g.symbols.iter().find(|s| {
                s.name == *sym && index.file(s.file).is_some_and(|f| f.path.as_str() == path)
            });
            let Some(decl) = decl else { continue };
            let lines = crate::refs_graph::read_lines(&root, path).unwrap_or_default();
            let dline = decl.start_line.saturating_sub(1) as usize;
            let dcol = lines
                .get(dline)
                .and_then(|l| {
                    crate::refs_graph::word_offsets(l, sym)
                        .first()
                        .copied()
                        .map(|b| l[..b].encode_utf16().count())
                })
                .unwrap_or(0);
            let t = std::time::Instant::now();
            let r = crate::refs_graph::graph_references(&index, &root, sym, &RelPath::new(path));
            let ms = t.elapsed().as_millis();
            let direct: BTreeSet<String> = {
                let id = decl.file.0;
                let mut s: BTreeSet<String> = g
                    .edges
                    .iter()
                    .filter(|e| e.kind == EdgeKind::Import && e.to == id)
                    .filter_map(|e| {
                        index
                            .file(FileId(e.from))
                            .map(|f| f.path.as_str().to_string())
                    })
                    .collect();
                s.insert(path.clone());
                s
            };
            let units = sym.encode_utf16().count() as u32;
            let (new, old): (Vec<serde_json::Value>, Vec<serde_json::Value>) = match &r {
                None => (Vec::new(), Vec::new()),
                Some(r) => {
                    let mut new = Vec::new();
                    let mut old = Vec::new();
                    for l in &r.value {
                        let v = serde_json::json!([
                            l.path.as_str(),
                            l.range.start.line,
                            l.range.start.character
                        ]);
                        let len = l.range.end.character - l.range.start.character;
                        let in_string = crate::refs_graph::read_lines(&root, l.path.as_str())
                            .and_then(|ls| {
                                let line = ls.get(l.range.start.line as usize)?.clone();
                                let mut units_seen = 0u32;
                                let mut byte = line.len();
                                for (i, ch) in line.char_indices() {
                                    if units_seen >= l.range.start.character {
                                        byte = i;
                                        break;
                                    }
                                    units_seen += ch.len_utf16() as u32;
                                }
                                Some(type_string_in(&ls, l.range.start.line as usize, byte))
                            })
                            .unwrap_or(false);
                        if len == units && direct.contains(l.path.as_str()) && !in_string {
                            old.push(v.clone());
                        }
                        new.push(v);
                    }
                    (new, old)
                }
            };
            let row = serde_json::json!({
                "path": path, "sym": sym, "line": dline, "col": dcol,
                "answered": r.is_some(), "ms": ms as u64, "new": new, "old": old,
            });
            writeln!(out, "{row}").unwrap();
        }
    }
}
