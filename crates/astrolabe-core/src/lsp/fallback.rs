//! Text-search fallback for precise queries that timed out.
//!
//! A hung language server must not leave the caller with nothing: an empty
//! "unknown" answer sends an agent off to grep by hand, and a partial edit
//! made from it is how a call site gets missed. Instead we return every
//! whole-word occurrence of the name in files of the same language, labelled
//! [`Confidence::Syntactic`] with a note that says exactly what happened.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::scan::{scan, ScanOptions};
use crate::types::{Confidence, Language, RelPath};

use super::{Location, Position, Precise, Range};

/// Hard caps so the fallback itself can never become the new hang.
const MAX_FILES: usize = 20_000;
const MAX_HITS: usize = 2_000;
const BUDGET: Duration = Duration::from_secs(10);

/// The identifier under `position` in `path`, if any.
pub(crate) fn identifier_at(
    repo_root: &Path,
    path: &RelPath,
    position: Position,
) -> Option<String> {
    let source = std::fs::read_to_string(repo_root.join(path.as_str())).ok()?;
    let line = source.split('\n').nth(position.line as usize)?;
    let line = line.strip_suffix('\r').unwrap_or(line);
    // Map the UTF-16 column to a char index.
    let mut utf16 = 0u32;
    let chars: Vec<char> = line.chars().collect();
    let mut idx = chars.len();
    for (i, ch) in chars.iter().enumerate() {
        if utf16 >= position.character {
            idx = i;
            break;
        }
        utf16 += ch.len_utf16() as u32;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    if idx >= chars.len() || !is_ident(chars[idx]) {
        return None;
    }
    let mut start = idx;
    while start > 0 && is_ident(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx;
    while end < chars.len() && is_ident(chars[end]) {
        end += 1;
    }
    Some(chars[start..end].iter().collect())
}

/// Whole-word occurrences of `name` in files sharing `origin`'s language.
pub(crate) fn text_references(
    repo_root: &Path,
    origin: &RelPath,
    name: &str,
    reason: &str,
) -> Precise<Vec<Location>> {
    let language = Language::from_path(origin);
    let (hits, truncated) = collect(
        repo_root,
        origin,
        name,
        |f| Language::from_path(f) == language,
        false,
    );
    let mut note = format!(
        "{reason}; fell back to a whole-word text search for `{name}`. \
         These are textual matches (may include comments, strings, and \
         unrelated symbols with the same name), not language-server results"
    );
    if truncated {
        note.push_str("; the text search hit its size/time budget and may be incomplete");
    }
    Precise {
        value: hits,
        confidence: Confidence::Syntactic,
        note: Some(note),
    }
}

/// Code (non-comment, non-string) occurrences of `name` across the Swift or
/// Objective-C family `origin` belongs to, located with tree-sitter.
///
/// Used when sourcekit-lsp / clangd answer without an index (no SwiftPM
/// build, no compile flags, code inside an inactive `#if`). Objective-C
/// includes `.m`, `.mm` and `.h`, since an ObjC symbol is declared in a
/// header and used in implementation files.
pub(crate) fn syntactic_references(
    repo_root: &Path,
    origin: &RelPath,
    name: &str,
    reason: &str,
) -> Precise<Vec<Location>> {
    let swift = Language::from_path(origin) == Some(Language::Swift);
    let (hits, truncated) = collect(
        repo_root,
        origin,
        name,
        |f| {
            if swift {
                Language::from_path(f) == Some(Language::Swift)
            } else {
                matches!(
                    Language::from_path(f),
                    Some(Language::ObjC | Language::ObjCpp)
                ) || f.extension().is_some_and(|e| e.eq_ignore_ascii_case("h"))
            }
        },
        true,
    );
    let mut note = format!(
        "{reason}. 已退回 tree-sitter 语法检索：以下是 `{name}` 在代码中（已排除注释和字符串）的同名出现，\
         不是语言服务器的语义结果，可能包含同名但无关的符号（confidence: Syntactic）"
    );
    if truncated {
        note.push_str("；检索达到文件数/时间上限，结果可能不完整");
    }
    Precise {
        value: hits,
        confidence: Confidence::Syntactic,
        note: Some(note),
    }
}

fn collect(
    repo_root: &Path,
    origin: &RelPath,
    name: &str,
    keep: impl Fn(&RelPath) -> bool,
    code_only: bool,
) -> (Vec<Location>, bool) {
    let started = Instant::now();
    let mut files = scan(repo_root, &ScanOptions::default()).files;
    files.retain(|f| keep(f));
    files.sort();
    // Origin first: if the budget runs out, the local hits survive.
    if let Some(pos) = files.iter().position(|f| f == origin) {
        let f = files.remove(pos);
        files.insert(0, f);
    }
    let mut hits = Vec::new();
    let mut truncated = false;
    for (n, file) in files.iter().enumerate() {
        if n >= MAX_FILES || hits.len() >= MAX_HITS || started.elapsed() > BUDGET {
            truncated = true;
            break;
        }
        let Ok(source) = std::fs::read_to_string(repo_root.join(file.as_str())) else {
            continue;
        };
        let mut file_hits = Vec::new();
        for (line_idx, line) in source.split('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            for column in word_columns(line, name) {
                let start = Position {
                    line: line_idx as u32,
                    character: column,
                };
                let end = Position {
                    line: line_idx as u32,
                    character: column + name.encode_utf16().count() as u32,
                };
                file_hits.push(Location {
                    path: file.clone(),
                    range: Range { start, end },
                });
            }
        }
        if code_only && !file_hits.is_empty() {
            let starts: Vec<Position> = file_hits.iter().map(|l| l.range.start).collect();
            if let Some(mask) = super::anchor::code_mask(file, &source, &starts) {
                let mut keep = mask.into_iter();
                file_hits.retain(|_| keep.next().unwrap_or(true));
            }
        }
        hits.extend(file_hits);
    }
    (hits, truncated)
}

fn word_columns(line: &str, name: &str) -> Vec<u32> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    if name.is_empty() {
        return out;
    }
    for (byte_idx, _) in line.match_indices(name) {
        let before_ok = line[..byte_idx]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after_ok = line[byte_idx + name.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            out.push(line[..byte_idx].encode_utf16().count() as u32);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_whole_words_across_same_language_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\nfn foobar() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn x() { foo(); }\n").unwrap();
        std::fs::write(dir.path().join("c.py"), "foo()\n").unwrap();
        let r = text_references(dir.path(), &RelPath::new("a.rs"), "foo", "timed out");
        assert_eq!(r.confidence, Confidence::Syntactic);
        assert!(r.note.unwrap().contains("timed out"));
        let paths: Vec<_> = r
            .value
            .iter()
            .map(|l| l.path.as_str().to_string())
            .collect();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn syntactic_references_skip_comments_and_span_objc_family() {
        let dir = tempfile::tempdir().unwrap();
        let w = |p: &str, b: &str| std::fs::write(dir.path().join(p), b).unwrap();
        w(
            "A.h",
            "// Widget is documented here\n@interface Widget : NSObject\n@end\n",
        );
        w("A.m", "#import \"A.h\"\n@implementation Widget\n- (void)f { NSLog(@\"Widget\"); [Widget new]; }\n@end\n");
        w("B.mm", "void g() { Widget *w = nil; }\n");
        w("C.swift", "let Widget = 1\n");
        let r = syntactic_references(
            dir.path(),
            &RelPath::new("A.h"),
            "Widget",
            "clangd has no index",
        );
        assert_eq!(r.confidence, Confidence::Syntactic);
        assert!(r.note.as_deref().unwrap().contains("tree-sitter"));
        let got: Vec<_> = r
            .value
            .iter()
            .map(|l| (l.path.as_str().to_string(), l.range.start.line))
            .collect();
        assert_eq!(
            got,
            vec![
                ("A.h".to_string(), 1),
                ("A.m".to_string(), 1),
                ("A.m".to_string(), 2),
                ("B.mm".to_string(), 0),
            ],
            "comment / string hits dropped, swift excluded"
        );

        w(
            "D.swift",
            "/// Widget doc\nstruct Gadget { let w: Widget }\n",
        );
        let r = syntactic_references(dir.path(), &RelPath::new("C.swift"), "Widget", "no index");
        let got: Vec<_> = r
            .value
            .iter()
            .map(|l| l.path.as_str().to_string())
            .collect();
        assert_eq!(got, vec!["C.swift", "D.swift"]);
    }

    #[test]
    fn identifier_at_position() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "let total_count = 1;\n").unwrap();
        let pos = Position {
            line: 0,
            character: 6,
        };
        assert_eq!(
            identifier_at(dir.path(), &RelPath::new("a.rs"), pos).as_deref(),
            Some("total_count")
        );
    }
}
