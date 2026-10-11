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
    let started = Instant::now();
    let mut files = scan(repo_root, &ScanOptions::default()).files;
    files.retain(|f| Language::from_path(f) == language);
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
                hits.push(Location {
                    path: file.clone(),
                    range: Range { start, end },
                });
            }
        }
    }
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
