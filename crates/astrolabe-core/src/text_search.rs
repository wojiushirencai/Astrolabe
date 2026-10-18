//! Parallel line search over plain text files, built on ripgrep's crates
//! (`grep-regex` + `grep-searcher`) and `rayon`.
//!
//! Used for files that are scanned but not in the code graph (docs,
//! config, SQL, templates, unsupported languages). Semantics match the
//! in-memory `search_code` path: one line = one match, literal queries are
//! escaped, and the caller picks case sensitivity.
//!
//! - **Encoding**: UTF-8 is searched as-is. UTF-16 (BOM) and other non-UTF-8
//!   text (GBK, Shift-JIS, Latin-1 …) are transcoded via
//!   [`crate::text::decode_source`] before matching.
//! - **Binary**: a NUL byte stops the search of that file.
//! - **Size**: files larger than `max_file_bytes` at search time are skipped
//!   (the scanner already gates admission; this covers files that grew).
//! - **Order**: results come back in input-path order, so output is
//!   deterministic despite the parallel search.

use std::path::Path;

use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{sinks, BinaryDetection, SearcherBuilder};
use rayon::prelude::*;

/// One matching line: 1-based line number and the line without its
/// terminator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextHit {
    pub line: u64,
    pub text: String,
}

/// Compile a line matcher. `regex == false` escapes `query`.
pub fn build_matcher(
    query: &str,
    regex: bool,
    case_sensitive: bool,
) -> Result<RegexMatcher, grep_regex::Error> {
    let pattern = if regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        // `$` must match before `\r\n`, like `str::lines` in the cached path.
        .crlf(true)
        .build(&pattern)
}

/// Search `paths` (repo-relative, `/`-separated) under `root` in parallel.
/// Returns `(index into paths, hits)` for files with at least one hit, in
/// input order. Unreadable, binary or oversized files are skipped.
pub fn search_files<P: AsRef<str> + Sync>(
    root: &Path,
    paths: &[P],
    matcher: &RegexMatcher,
    max_file_bytes: u64,
) -> Vec<(usize, Vec<TextHit>)> {
    paths
        .par_iter()
        .enumerate()
        .filter_map(|(idx, rel)| {
            let hits = search_one(&root.join(rel.as_ref()), matcher, max_file_bytes);
            (!hits.is_empty()).then_some((idx, hits))
        })
        .collect()
}

fn search_one(path: &Path, matcher: &RegexMatcher, max_file_bytes: u64) -> Vec<TextHit> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= max_file_bytes => {}
        _ => return Vec::new(),
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    // UTF-16 BOM files contain NULs by design; everything else with a NUL is
    // binary. Non-UTF-8 text (GBK, Shift-JIS, Latin-1 …) is detected and
    // transcoded instead of being searched as lossy bytes.
    let utf16_bom = bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]);
    let decoded: Option<String> = if utf16_bom || std::str::from_utf8(&bytes).is_err() {
        if !utf16_bom && bytes.contains(&0) {
            return Vec::new();
        }
        Some(crate::text::decode_source(bytes.clone()))
    } else {
        None
    };
    let haystack: &[u8] = match &decoded {
        Some(text) => text.as_bytes(),
        None => &bytes,
    };
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_terminator(LineTerminator::crlf())
        .line_number(true)
        .build();
    let mut hits = Vec::new();
    let result = searcher.search_slice(
        matcher,
        haystack,
        sinks::Lossy(|line, text| {
            hits.push(TextHit {
                line,
                text: text.trim_end_matches(['\n', '\r']).to_string(),
            });
            Ok(true)
        }),
    );
    if result.is_err() {
        return Vec::new();
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "astrolabe-text-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn literal_regex_case_binary_size_and_encoding() {
        let d = tmp();
        std::fs::write(d.join("a.md"), "Hello World\nfoo.bar\r\nhello again\n").unwrap();
        std::fs::write(d.join("bin.dat"), b"hello\0world\n").unwrap();
        std::fs::write(d.join("big.txt"), "hello\n".repeat(100)).unwrap();
        // UTF-16LE with BOM.
        let mut utf16 = vec![0xFF, 0xFE];
        for u in "x\nhello utf16\n".encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(d.join("u16.txt"), utf16).unwrap();
        // GBK bytes are transcoded, not dropped or searched as mojibake.
        let (gbk, _, _) = encoding_rs::GBK.encode("hello 你好，这是中文注释\n");
        std::fs::write(d.join("gbk.txt"), &gbk).unwrap();
        let paths = [
            "a.md",
            "bin.dat",
            "big.txt",
            "u16.txt",
            "gbk.txt",
            "missing.txt",
        ];

        let m = build_matcher("hello", false, false).unwrap();
        let got = search_files(&d, &paths, &m, 100);
        let idx: Vec<usize> = got.iter().map(|(i, _)| *i).collect();
        assert_eq!(idx, vec![0, 3, 4], "{got:?}");
        assert_eq!(
            got[0].1,
            vec![
                TextHit {
                    line: 1,
                    text: "Hello World".into()
                },
                TextHit {
                    line: 3,
                    text: "hello again".into()
                },
            ]
        );
        assert_eq!(got[1].1[0].text, "hello utf16");
        assert_eq!(got[1].1[0].line, 2);

        // Case-sensitive literal.
        let m = build_matcher("Hello", false, true).unwrap();
        let got = search_files(&d, &["a.md"], &m, 100);
        assert_eq!(got[0].1.len(), 1);
        // Literal dot is escaped; regex dot is not.
        let m = build_matcher("foo.bar", false, true).unwrap();
        assert_eq!(search_files(&d, &["a.md"], &m, 100)[0].1[0].text, "foo.bar");
        let m = build_matcher("foo.ba", false, true).unwrap();
        assert_eq!(search_files(&d, &["a.md"], &m, 100).len(), 1);
        let m = build_matcher("f.o\\.bar$", true, true).unwrap();
        assert_eq!(search_files(&d, &["a.md"], &m, 100).len(), 1);
        let m = build_matcher("你好", false, true).unwrap();
        let got = search_files(&d, &["gbk.txt"], &m, 100);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1[0].text.contains("你好"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
