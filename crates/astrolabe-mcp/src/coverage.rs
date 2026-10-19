//! Index-coverage reporting for `get_languages` and the per-query note.
//!
//! Two different things are kept apart:
//! * **hard gaps** — supported-language files that are NOT in the index (over
//!   the size cap, unreadable, parser timeout / no tree);
//! * **partial parses** — files that ARE in the index but have syntax/compile
//!   errors, so some symbols may be missing.
//!
//! Unsupported files are only ever summarised by extension. When more than
//! [`INLINE_LIMIT`] files have problems the details go to a Markdown report
//! next to the index cache (never inside the tracked tree), and the tool
//! output only gives the count and the absolute path.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use astrolabe_core::{Language, RelPath};

use crate::index::RepoIndex;

/// At most this many problem files are listed inline.
pub(crate) const INLINE_LIMIT: usize = 10;
/// Unsupported file types shown by name before folding into "other".
pub(crate) const TOP_TYPES: usize = 8;
/// Entries per section in the Markdown report.
const REPORT_MAX_ENTRIES: usize = 1000;
/// Hard byte cap of the Markdown report.
const REPORT_MAX_BYTES: usize = 192 * 1024;
const REPORT_FILE: &str = "coverage-report.md";

pub(crate) const PARTIAL_LABEL: &str = "has errors, index incomplete (symbols may be missing)";

/// Short reason bucket for a hard gap.
fn gap_kind(reason: &str) -> &'static str {
    if reason.starts_with("too large") {
        "too large (size cap)"
    } else if reason.starts_with("read:") {
        "read failure"
    } else if reason.contains("budget") || reason.contains("tree") {
        "parser timeout"
    } else {
        "parse error"
    }
}

impl RepoIndex {
    /// Supported-language files that are not in the index. Sorted, unique.
    pub(crate) fn hard_gaps(&self) -> Vec<(RelPath, String)> {
        let report = self.report();
        let mut gaps: Vec<(RelPath, String)> = report
            .parse_failures
            .iter()
            .cloned()
            .chain(
                report
                    .too_large
                    .iter()
                    .map(|path| (path.clone(), "too large (size cap)".to_string())),
            )
            .collect();
        gaps.sort();
        gaps.dedup_by(|a, b| a.0 == b.0);
        gaps
    }

    /// Files in the index that have syntax errors. Sorted.
    pub(crate) fn partial_files(&self) -> Vec<RelPath> {
        let mut v: Vec<RelPath> = self
            .report()
            .partial_parses
            .iter()
            .map(|(p, _)| p.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Very short note appended to every query result; no paths. `None` when
    /// coverage is complete.
    pub(crate) fn coverage_note(&self) -> Option<String> {
        let hard = self.hard_gaps().len();
        let partial = self.report().partial_parses.len();
        if hard == 0 && partial == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if hard > 0 {
            parts.push(format!("{hard} 个文件未进入索引"));
        }
        if partial > 0 {
            parts.push(format!("{partial} 个文件有语法错误(符号可能不全)"));
        }
        Some(format!(
            "注意：{}，结果可能遗漏；详见 get_languages。",
            parts.join("，")
        ))
    }

    fn language_of(&self, path: &RelPath) -> Option<Language> {
        self.graph()
            .files
            .iter()
            .find(|f| &f.path == path)
            .and_then(|f| f.language)
    }

    fn first_error_of(&self, path: &RelPath) -> Option<(u32, String)> {
        let lang = self.language_of(path)?;
        let source = astrolabe_core::text::read_source(&self.root.join(path.as_str())).ok()?;
        astrolabe_core::parse::first_syntax_error(lang, &source)
    }

    /// The coverage block of `get_languages`.
    pub(crate) fn coverage_detail(&self) -> String {
        let hard = self.hard_gaps();
        let partial = self.partial_files();
        if hard.is_empty() && partial.is_empty() {
            return "index coverage: complete (every supported-language file is indexed, none has syntax errors)\n".to_string();
        }
        let mut out = String::new();
        let need_report = hard.len() > INLINE_LIMIT || partial.len() > INLINE_LIMIT;
        if !hard.is_empty() {
            let mut kinds: std::collections::BTreeMap<&str, usize> = Default::default();
            for (_, why) in &hard {
                *kinds.entry(gap_kind(why)).or_default() += 1;
            }
            let kinds = kinds
                .iter()
                .map(|(k, n)| format!("{k}: {n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "files NOT in the index: {} ({kinds})", hard.len());
            if hard.len() <= INLINE_LIMIT {
                for (path, why) in &hard {
                    let _ = writeln!(out, "  {path}: {why}");
                }
            }
        }
        if !partial.is_empty() {
            let _ = writeln!(
                out,
                "files with syntax/compile errors: {} (in the index, but symbols may be missing)",
                partial.len()
            );
            if partial.len() <= INLINE_LIMIT {
                for path in &partial {
                    match self.first_error_of(path) {
                        Some((line, what)) => {
                            let _ = writeln!(out, "  {path}: {PARTIAL_LABEL}; line {line}: {what}");
                        }
                        None => {
                            let _ = writeln!(out, "  {path}: {PARTIAL_LABEL}");
                        }
                    }
                }
            }
        }
        if need_report {
            match self.coverage_report_path() {
                Some(path) => {
                    let _ = writeln!(
                        out,
                        "details for all affected files (path, language, first error): Read {}",
                        path.display()
                    );
                }
                None => {
                    out.push_str("details report could not be written (cache dir not writable); use find_symbol/search_code on suspect files\n");
                }
            }
        }
        out
    }

    /// Absolute path of the Markdown report, written once per index snapshot.
    /// `None` when no writable location exists.
    pub(crate) fn coverage_report_path(&self) -> Option<PathBuf> {
        self.coverage_report_cell()
            .get_or_init(|| {
                let body = self.render_report();
                write_report(&self.root, &body)
            })
            .clone()
    }

    pub(crate) fn render_report(&self) -> String {
        let hard = self.hard_gaps();
        let partial = self.partial_files();
        let mut md = String::new();
        md.push_str("# Astrolabe index coverage report\n\n");
        let _ = writeln!(md, "- repository: `{}`", self.root.display());
        let _ = writeln!(md, "- files not in the index: {}", hard.len());
        let _ = writeln!(
            md,
            "- files with syntax/compile errors: {}\n",
            partial.len()
        );
        md.push_str(
            "Files with syntax errors are still indexed; symbols inside the broken region \
             may be missing. Use `Read`/`search_code` on them when a symbol seems absent.\n\n",
        );
        let mut truncated = false;
        if !hard.is_empty() {
            md.push_str("## Not in the index\n\n| path | reason |\n|---|---|\n");
            for (path, why) in hard.iter().take(REPORT_MAX_ENTRIES) {
                if md.len() > REPORT_MAX_BYTES {
                    truncated = true;
                    break;
                }
                let _ = writeln!(
                    md,
                    "| {path} | {} |",
                    why.replace('|', "/").replace('\n', " ")
                );
            }
            if hard.len() > REPORT_MAX_ENTRIES {
                truncated = true;
            }
            md.push('\n');
        }
        if !partial.is_empty() {
            md.push_str(
                "## Syntax/compile errors (indexed, possibly incomplete)\n\n\
                 | path | language | first error |\n|---|---|---|\n",
            );
            for path in partial.iter().take(REPORT_MAX_ENTRIES) {
                if md.len() > REPORT_MAX_BYTES {
                    truncated = true;
                    break;
                }
                let lang = self
                    .language_of(path)
                    .map(|l| l.name())
                    .unwrap_or("unknown");
                let err = match self.first_error_of(path) {
                    Some((line, what)) => format!("line {line}: {}", what.replace('|', "/")),
                    None => "syntax errors (location not determined)".to_string(),
                };
                let _ = writeln!(md, "| {path} | {lang} | {err} |");
            }
            if partial.len() > REPORT_MAX_ENTRIES {
                truncated = true;
            }
            md.push('\n');
        }
        if truncated {
            md.push_str(
                "_Report truncated: only the first entries (sorted by path) are listed._\n",
            );
        }
        md
    }
}

/// Write the report atomically (temp file + rename) into `<root>/.astrolabe/`
/// (the self-ignoring cache dir), falling back to the OS temp dir.
fn write_report(root: &Path, body: &str) -> Option<PathBuf> {
    let primary = root.join(".astrolabe");
    if let Some(path) = write_atomic(&primary, body) {
        return Some(path);
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    write_atomic(
        &std::env::temp_dir().join(format!("astrolabe-coverage-{h:016x}")),
        body,
    )
}

fn write_atomic(dir: &Path, body: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(&ignore, "# Created by Astrolabe. Ignores itself.\n*\n");
    }
    let dest = dir.join(REPORT_FILE);
    let tmp = dir.join(format!("{REPORT_FILE}.tmp{}", std::process::id()));
    if std::fs::write(&tmp, body).is_err() || std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(dest.canonicalize().unwrap_or(dest))
}

/// Unsupported-file summary by type: top [`TOP_TYPES`] extensions, the rest
/// folded into one count. Size is bounded regardless of file count.
pub(crate) fn unsupported_by_type(counts: &std::collections::BTreeMap<String, u32>) -> String {
    let mut list: Vec<(&String, &u32)> = counts.iter().collect();
    list.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut items: Vec<String> = list
        .iter()
        .take(TOP_TYPES)
        .map(|(ext, n)| format!(".{ext}×{n}"))
        .collect();
    if list.len() > TOP_TYPES {
        let rest = &list[TOP_TYPES..];
        let files: u32 = rest.iter().map(|(_, n)| **n).sum();
        items.push(format!("other {} types×{files}", rest.len()));
    }
    items.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::graph::CodeGraph;
    use astrolabe_core::IndexReport;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-cov-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn broken(dir: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(
                dir.join(format!("bad{i:02}.py")),
                format!("def keep{i}():\n    pass\n\ndef oops{i}(:\n"),
            )
            .unwrap();
        }
    }

    #[test]
    fn clean_repo_reports_complete_and_no_note() {
        let dir = temp_dir("clean");
        std::fs::write(dir.join("a.py"), "def a():\n    pass\n").unwrap();
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        assert!(index.coverage_note().is_none());
        assert!(index.coverage_detail().contains("complete"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_parse_stays_indexed_and_is_listed_inline_up_to_ten() {
        let dir = temp_dir("inline");
        broken(&dir, 3);
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        // Not a hard gap, and its symbols are in the index.
        assert!(index.hard_gaps().is_empty());
        assert_eq!(index.partial_files().len(), 3);
        assert!(index.graph().symbols.iter().any(|s| s.name == "keep0"));
        let detail = index.coverage_detail();
        assert!(detail.contains("syntax/compile errors: 3"), "{detail}");
        assert!(detail.contains("bad00.py"), "{detail}");
        assert!(detail.contains(PARTIAL_LABEL), "{detail}");
        assert!(detail.contains("line 4"), "first error line: {detail}");
        assert!(
            !detail.contains("Read "),
            "no report below the limit: {detail}"
        );
        assert!(!detail.contains("NOT in the index"), "{detail}");
        let note = index.coverage_note().unwrap();
        assert!(note.contains("3 个文件有语法错误"), "{note}");
        assert!(!note.contains("bad00"), "no paths in the note: {note}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn more_than_ten_problem_files_go_to_a_markdown_report() {
        let dir = temp_dir("report");
        broken(&dir, 12);
        let index = crate::index::build_with_options(&dir, false, 1 << 20).unwrap();
        let detail = index.coverage_detail();
        assert!(detail.contains("syntax/compile errors: 12"), "{detail}");
        assert!(!detail.contains("bad00.py"), "no inline list: {detail}");
        let path = index.coverage_report_path().expect("report written");
        assert!(path.is_absolute());
        assert!(detail.contains(&path.display().to_string()), "{detail}");
        let md = std::fs::read_to_string(&path).unwrap();
        assert!(md.contains("bad00.py") && md.contains("bad11.py"), "{md}");
        assert!(md.contains("python"), "{md}");
        assert!(md.contains("line 4"), "{md}");
        // Deterministic, no leftover temp file, self-ignoring directory.
        assert_eq!(md, index.render_report());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(path.parent().unwrap().join(".gitignore").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_is_bounded_with_truncation_note() {
        let report = IndexReport {
            partial_parses: (0..3000)
                .map(|i| (RelPath::new(format!("src/m{i:05}.py")), String::new()))
                .collect(),
            ..IndexReport::default()
        };
        let index = RepoIndex::from_graph(
            PathBuf::from("/nonexistent-root"),
            CodeGraph::default(),
            report,
        );
        let md = index.render_report();
        assert!(md.len() <= REPORT_MAX_BYTES + 512, "{}", md.len());
        assert!(
            md.contains("m00999") && !md.contains("m01000"),
            "cap entries"
        );
        assert!(md.contains("Report truncated"));
    }

    #[test]
    fn hard_gaps_are_counted_by_reason_and_not_partial() {
        let report = IndexReport {
            parse_failures: vec![
                (RelPath::new("a.py"), "parse exceeded its budget".into()),
                (RelPath::new("b.py"), "read: permission denied".into()),
            ],
            too_large: vec![RelPath::new("big.ts")],
            ..IndexReport::default()
        };
        let index = RepoIndex::from_graph(PathBuf::from("/r"), CodeGraph::default(), report);
        assert_eq!(index.hard_gaps().len(), 3);
        let detail = index.coverage_detail();
        assert!(detail.contains("files NOT in the index: 3"), "{detail}");
        assert!(detail.contains("too large (size cap): 1"), "{detail}");
        assert!(detail.contains("read failure: 1"), "{detail}");
        assert!(detail.contains("parser timeout: 1"), "{detail}");
        assert!(detail.contains("a.py"), "{detail}");
        let note = index.coverage_note().unwrap();
        assert!(
            note.contains("3 个文件未进入索引") && !note.contains("a.py"),
            "{note}"
        );
    }

    #[test]
    fn report_write_failure_is_graceful() {
        // `<root>/.astrolabe` is a regular file: primary location fails and the
        // temp-dir fallback (or None) is used without panicking.
        let dir = temp_dir("wfail");
        std::fs::write(dir.join(".astrolabe"), b"not a dir").unwrap();
        let path = write_report(&dir, "# x\n");
        if let Some(p) = &path {
            assert!(!p.starts_with(&dir));
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coverage_block_size_does_not_grow_with_file_count() {
        let mk = |n: usize| {
            let report = IndexReport {
                partial_parses: (0..n)
                    .map(|i| {
                        (
                            RelPath::new(format!("src/deep/dir/m{i:06}.c")),
                            String::new(),
                        )
                    })
                    .collect(),
                parse_failures: (0..n)
                    .map(|i| {
                        (
                            RelPath::new(format!("gen/x{i:06}.c")),
                            "read: denied".into(),
                        )
                    })
                    .collect(),
                ..IndexReport::default()
            };
            let dir = temp_dir("size");
            let index = RepoIndex::from_graph(dir.clone(), CodeGraph::default(), report);
            let len = index.coverage_detail().len();
            let _ = std::fs::remove_dir_all(&dir);
            len
        };
        let small = mk(50);
        let large = mk(5000);
        assert!(large <= 600, "coverage block is {large} B");
        // Only the digits of the counts may differ.
        assert!(large <= small + 16, "{small} -> {large}");
    }

    #[test]
    fn unsupported_files_are_summarised_by_type_with_bounded_size() {
        let mut counts = std::collections::BTreeMap::new();
        for (i, ext) in [
            "scala", "ex", "erl", "zig", "nim", "hs", "ml", "clj", "jl", "r", "pl",
        ]
        .iter()
        .enumerate()
        {
            counts.insert((*ext).to_string(), 100 - i as u32);
        }
        let s = unsupported_by_type(&counts);
        assert!(s.starts_with(".scala×100, .ex×99"), "{s}");
        assert!(s.contains("other 3 types×"), "{s}");
        assert_eq!(s.matches('×').count(), TOP_TYPES + 1, "{s}");
        // 10x the files: identical size.
        let big = counts.iter().map(|(k, v)| (k.clone(), v * 10)).collect();
        assert!(unsupported_by_type(&big).len() <= s.len() + 24);
    }
}
