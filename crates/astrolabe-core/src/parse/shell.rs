//! Shell / Bash (`.sh`, `.bash`, shebang) extraction hooks (lang-batch-8).
//!
//! Owned by the shell worker; see `LANG-PLAN.md`. `parse/mod.rs` calls these
//! hooks for [`Language::Shell`] only, so all language-specific post-processing
//! lives here and never in shared code.
//!
//! Shell has no namespaces, so names stay unqualified. The only refinement is
//! the kind of file-scope variables: `readonly X=1` and `declare -r X=1`
//! (`typeset -r`, combined flags like `-gr`) are constants.

use crate::types::{Language, SymbolKind};

#[allow(dead_code)]
const LANG: Language = Language::Shell;

/// `\\. FILE` (nvm's alias-proof `source`) at the start of a line that
/// follows another statement is mis-parsed by tree-sitter-bash 0.25: the
/// external scanner reads `\\` + newline context as a continuation and glues
/// `\\.` onto the previous command as plain arguments, so the `source` is
/// lost. Rewrite a line-leading `\\.` followed by whitespace to ` .` — same
/// byte length, so every line / column the extractor reports is unchanged.
/// Returns `None` when there is nothing to rewrite.
pub(super) fn normalize_backslash_dot(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut out: Option<Vec<u8>> = None;
    let mut line_start = 0usize;
    while line_start < bytes.len() {
        let mut i = line_start;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if bytes.get(i) == Some(&b'\\')
            && bytes.get(i + 1) == Some(&b'.')
            && matches!(bytes.get(i + 2), Some(b' ' | b'\t'))
        {
            out.get_or_insert_with(|| bytes.to_vec())[i] = b' ';
        }
        line_start = match bytes[i..].iter().position(|&b| b == b'\n') {
            Some(n) => i + n + 1,
            None => bytes.len(),
        };
    }
    // Only an ASCII byte was replaced by another, so UTF-8 stays valid.
    out.map(|v| String::from_utf8(v).expect("ascii-for-ascii rewrite"))
}

/// Final symbol name / kind for one `@definition.*` match.
pub(super) fn symbol_name(
    definition: tree_sitter::Node<'_>,
    name: &str,
    kind: SymbolKind,
    source: &str,
) -> (String, SymbolKind) {
    if kind == SymbolKind::Variable && is_readonly(definition, source) {
        return (name.to_owned(), SymbolKind::Const);
    }
    (name.to_owned(), kind)
}

/// `true` when the assignment sits in `readonly …` or `declare/typeset -…r…`.
fn is_readonly(assignment: tree_sitter::Node<'_>, source: &str) -> bool {
    let Some(decl) = assignment.parent() else {
        return false;
    };
    if decl.kind() != "declaration_command" {
        return false;
    }
    let bytes = source.as_bytes();
    let mut cursor = decl.walk();
    let mut keyword = "";
    for child in decl.children(&mut cursor) {
        let text = child.utf8_text(bytes).unwrap_or("");
        if !child.is_named() && keyword.is_empty() {
            keyword = text;
            if keyword == "readonly" {
                return true;
            }
            continue;
        }
        if child.kind() == "word"
            && matches!(keyword, "declare" | "typeset")
            && text.starts_with('-')
            && text[1..].contains('r')
        {
            return true;
        }
    }
    false
}

/// Whether a definition is visible outside its file. Sourcing a shell file
/// shares every function and file-scope variable, so everything is.
pub(super) fn exported(_name: &str, _node: tree_sitter::Node<'_>, _source: &str) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::super::ParserPool;
    use crate::types::{Language, RelPath, SymbolKind};

    const FIXTURE: &str = include_str!("../../tests/fixtures/shell/lib.sh");

    #[test]
    fn shell_full_declaration_coverage() {
        let parsed = ParserPool::new()
            .parse(Language::Shell, &RelPath::new("lib.sh"), FIXTURE)
            .expect("parse");
        let mut got: Vec<(String, SymbolKind)> = parsed
            .symbols
            .iter()
            .map(|s| (s.name.clone(), s.kind))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        use SymbolKind::*;
        let mut want: Vec<(String, SymbolKind)> = [
            ("APP_HOME", Variable),
            ("EXPORTED", Variable),
            ("MORE", Variable),
            ("VERSION", Const),
            ("LIMIT", Const),
            ("GLOBAL_RO", Const),
            ("ARR", Variable),
            ("TYPESET_VAR", Variable),
            ("posix_fn", Function),
            ("kw_fn", Function),
            ("kw_paren_fn", Function),
            ("inner_fn", Function),
            ("_private_helper", Function),
            ("in_if_fn", Function),
        ]
        .iter()
        .map(|(n, k)| (n.to_string(), *k))
        .collect();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, want);
        assert!(parsed.symbols.iter().all(|s| s.exported));

        let posix = parsed
            .symbols
            .iter()
            .find(|s| s.name == "posix_fn")
            .unwrap();
        assert!(posix.end_line > posix.start_line);
    }

    #[test]
    fn shell_imports_and_calls() {
        let parsed = ParserPool::new()
            .parse(Language::Shell, &RelPath::new("lib.sh"), FIXTURE)
            .expect("parse");
        assert_eq!(
            parsed.imports,
            vec![
                "./common.sh",
                "helpers/util.bash",
                "${BASH_SOURCE%/*}/dyn.sh",
                "$(dirname \"$0\")/sub.sh",
            ]
        );
        let names: Vec<&str> = parsed.calls.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"posix_fn"));
        assert!(names.contains(&"echo"));
        assert!(!names.iter().any(|n| n.starts_with('$')));
    }

    /// Corpus symbol dump for the recall bench (`/workspace/recall-bench/lang8`).
    /// `DUMP_DIR` root, `DUMP_LIST` repo-relative paths, `DUMP_OUT` TSV
    /// `file\tline\tname\tkind`. Files whose language is not detected as
    /// Shell are reported as `SKIP`.
    #[test]
    #[ignore = "corpus dump; run with -- --ignored"]
    fn zz_dump_shell_symbols() {
        use std::io::Write;
        let (Ok(dir), Ok(list), Ok(out)) = (
            std::env::var("DUMP_DIR"),
            std::env::var("DUMP_LIST"),
            std::env::var("DUMP_OUT"),
        ) else {
            return;
        };
        let pool = ParserPool::new();
        let mut w = std::fs::File::create(out).unwrap();
        let t0 = std::time::Instant::now();
        let (mut files, mut bytes, mut errs) = (0usize, 0usize, 0usize);
        for rel in std::fs::read_to_string(list).unwrap().lines() {
            let Ok(raw) = std::fs::read(format!("{dir}/{rel}")) else {
                continue;
            };
            let src = String::from_utf8_lossy(&raw);
            let path = RelPath::new(rel);
            if Language::from_path_and_source(&path, &src) != Some(Language::Shell) {
                println!("SKIP {rel}");
                continue;
            }
            files += 1;
            bytes += raw.len();
            match pool.parse(Language::Shell, &path, &src) {
                Ok(p) => {
                    for s in p.symbols {
                        writeln!(w, "{rel}\t{}\t{}\t{:?}", s.start_line, s.name, s.kind).unwrap();
                    }
                }
                Err(e) => {
                    errs += 1;
                    println!("ERR {rel} {e:?}");
                }
            }
        }
        println!(
            "DUMP files={files} bytes={bytes} errs={errs} ms={}",
            t0.elapsed().as_millis()
        );
    }
}

#[cfg(test)]
mod backslash_dot_tests {
    use super::normalize_backslash_dot;

    #[test]
    fn rewrites_only_line_leading_backslash_dot() {
        let src = ": nvm.sh\n\\. ../nvm.sh\n  \\.\t./a.sh\necho \\. x\n\\.x\n";
        let got = normalize_backslash_dot(src).expect("rewritten");
        assert_eq!(
            got,
            ": nvm.sh\n . ../nvm.sh\n   .\t./a.sh\necho \\. x\n\\.x\n"
        );
        assert_eq!(got.len(), src.len());
        assert_eq!(normalize_backslash_dot("source a.sh\n"), None);
    }
}
