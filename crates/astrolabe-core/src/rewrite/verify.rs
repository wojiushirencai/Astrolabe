//! Post-rewrite parse verification.
//!
//! [`ParserPool::parse`](crate::parse::ParserPool::parse) is the wrong tool
//! here: it runs symbol queries, drops the tree, and returns `Ok` for source
//! that still contains `ERROR` / `MISSING` nodes (tree-sitter recovers). This
//! module re-parses with a reused per-language parser, walks only subtrees
//! whose `has_error` flag is set, and compares error-node counts.
//!
//! A file that already had error nodes is allowed to keep the same count —
//! real repos contain permanently-unparseable files, and refusing every
//! rewrite because of them would disable the feature. A file that parsed
//! cleanly (or with *n* errors) and now has more, or that previously produced
//! a tree and now produces none, fails the whole transaction.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tree_sitter::{Parser, Tree, TreeCursor};

use crate::types::{Language, RelPath};

use super::RewriteError;

const PARSE_BUDGET: Duration = Duration::from_secs(5);
const LANGUAGES: [Language; 10] = [
    Language::Python,
    Language::Go,
    Language::Java,
    Language::Rust,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Php,
    Language::C,
    Language::Cpp,
];

/// Parse-health of one file, captured before or after a rewrite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileHealth {
    /// No grammar for this path (markdown, JSON, …). Verification skips it.
    Skipped,
    /// Tree-sitter returned no tree (`None`): timeout, cancellation, or a
    /// parser that could not be configured. Distinct from "the tree has
    /// error nodes" — recovery still produces a `Tree`.
    Unparseable,
    /// `error_nodes` is the number of `ERROR` plus `MISSING` nodes. Zero
    /// means the file parsed cleanly.
    Parsed { error_nodes: u32 },
}

/// Pre-rewrite health keyed by path. Built once, compared after the edit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthSnapshot {
    files: BTreeMap<RelPath, FileHealth>,
}

impl HealthSnapshot {
    pub fn get(&self, path: &RelPath) -> Option<FileHealth> {
        self.files.get(path).copied()
    }
}

/// Reusable parsers for the verification hot path.
///
/// Same pooling shape as [`crate::parse::ParserPool`]: one parser per
/// language, never grown with file count, trees dropped after the walk.
/// Queries are not run — counting error nodes does not need them.
pub struct Verifier {
    parsers: [Mutex<Option<Parser>>; LANGUAGES.len()],
    budget: Duration,
}

impl Verifier {
    pub fn new() -> Self {
        Self::with_budget(PARSE_BUDGET)
    }

    fn with_budget(budget: Duration) -> Self {
        Verifier {
            parsers: std::array::from_fn(|index| Mutex::new(configured_parser(LANGUAGES[index]))),
            budget,
        }
    }

    /// Record parse health for every file a plan will touch.
    pub fn snapshot<'a, I, S>(&self, files: I) -> HealthSnapshot
    where
        I: IntoIterator<Item = (&'a RelPath, S)>,
        S: AsRef<str>,
    {
        let mut snap = HealthSnapshot::default();
        for (path, source) in files {
            snap.files
                .insert(path.clone(), self.measure(path, source.as_ref()));
        }
        snap
    }

    /// Re-parse `files` and fail if any target-language file got worse.
    ///
    /// Files are checked in path order so a multi-file regression names the
    /// lexicographically first offender. Non-code paths are skipped.
    pub fn verify<'a, I, S>(&self, before: &HealthSnapshot, files: I) -> Result<(), RewriteError>
    where
        I: IntoIterator<Item = (&'a RelPath, S)>,
        S: AsRef<str>,
    {
        let mut ordered: Vec<(&RelPath, S)> = files.into_iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(b.0));
        for (path, source) in ordered {
            let after = self.measure(path, source.as_ref());
            let prior = before
                .get(path)
                .unwrap_or(FileHealth::Parsed { error_nodes: 0 });
            if health_regressed(prior, after) {
                return Err(RewriteError::VerificationFailed(path.clone()));
            }
        }
        Ok(())
    }

    fn measure(&self, path: &RelPath, source: &str) -> FileHealth {
        let Some(lang) = Language::from_path(path) else {
            return FileHealth::Skipped;
        };
        match self.parse_tree(lang, source) {
            Some(tree) => {
                let error_nodes = count_error_nodes(tree.root_node().walk());
                FileHealth::Parsed { error_nodes }
            }
            None => FileHealth::Unparseable,
        }
    }

    fn parse_tree(&self, lang: Language, source: &str) -> Option<Tree> {
        let mut slot = self.parsers[language_index(lang)?].lock().ok()?;
        let parser = slot.as_mut()?;

        let deadline = Instant::now() + self.budget;
        let tree = {
            let mut progress = |_: &tree_sitter::ParseState| {
                if Instant::now() >= deadline {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress);
            let bytes = source.as_bytes();
            parser.parse_with_options(&mut |offset, _| &bytes[offset..], None, Some(options))
        };

        match tree {
            Some(tree) => Some(tree),
            None => {
                parser.reset();
                None
            }
        }
    }
}

impl Default for Verifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Record parse health for `files` using a fresh [`Verifier`].
pub fn snapshot<'a, I, S>(files: I) -> HealthSnapshot
where
    I: IntoIterator<Item = (&'a RelPath, S)>,
    S: AsRef<str>,
{
    Verifier::new().snapshot(files)
}

/// Re-parse `files` against a pre-rewrite [`HealthSnapshot`].
pub fn verify<'a, I, S>(before: &HealthSnapshot, files: I) -> Result<(), RewriteError>
where
    I: IntoIterator<Item = (&'a RelPath, S)>,
    S: AsRef<str>,
{
    Verifier::new().verify(before, files)
}

/// Capture health, then compare. For callers that already hold both buffers.
pub fn verify_rewrite<'a, I, J, S, T>(before: I, after: J) -> Result<(), RewriteError>
where
    I: IntoIterator<Item = (&'a RelPath, S)>,
    J: IntoIterator<Item = (&'a RelPath, T)>,
    S: AsRef<str>,
    T: AsRef<str>,
{
    let verifier = Verifier::new();
    let snap = verifier.snapshot(before);
    verifier.verify(&snap, after)
}

fn health_regressed(before: FileHealth, after: FileHealth) -> bool {
    match (before, after) {
        (_, FileHealth::Skipped) | (FileHealth::Skipped, _) => false,
        // Still no tree: the file was already beyond recovery, do not block
        // every rewrite in a repo that contains such files.
        (FileHealth::Unparseable, FileHealth::Unparseable) => false,
        (FileHealth::Unparseable, FileHealth::Parsed { .. }) => false,
        (_, FileHealth::Unparseable) => true,
        (
            FileHealth::Parsed {
                error_nodes: before_n,
            },
            FileHealth::Parsed {
                error_nodes: after_n,
            },
        ) => after_n > before_n,
    }
}

/// Count `ERROR` and `MISSING` nodes.
///
/// `Node::has_error` is an O(1) flag: clean subtrees are skipped without
/// walking their children. An `ERROR` node itself counts as one; its
/// non-error token children do not. Nested `ERROR` / `MISSING` nodes each
/// add one, so a rewrite that introduces additional recovery sites is
/// visible as a count increase.
fn count_error_nodes(mut cursor: TreeCursor<'_>) -> u32 {
    count_error_nodes_at(&mut cursor)
}

fn count_error_nodes_at(cursor: &mut TreeCursor<'_>) -> u32 {
    let node = cursor.node();
    if !node.has_error() && !node.is_missing() {
        return 0;
    }
    let mut total = u32::from(node.is_error() || node.is_missing());
    if !cursor.goto_first_child() {
        return total;
    }
    loop {
        total = total.saturating_add(count_error_nodes_at(cursor));
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    cursor.goto_parent();
    total
}

fn language_index(lang: Language) -> Option<usize> {
    Some(match lang {
        Language::Python => 0,
        Language::Go => 1,
        Language::Java => 2,
        Language::Rust => 3,
        Language::TypeScript => 4,
        Language::Tsx => 5,
        Language::JavaScript => 6,
        Language::Php => 7,
        Language::C => 8,
        Language::Cpp => 9,
        Language::ObjC
        | Language::ObjCpp
        | Language::Swift
        | Language::Vue => return None,
    })
}

fn grammar(lang: Language) -> Option<tree_sitter::Language> {
    Some(match lang {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::ObjC
        | Language::ObjCpp
        | Language::Swift
        | Language::Vue => return None,
    })
}

fn configured_parser(lang: Language) -> Option<Parser> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(lang)?).ok()?;
    Some(parser)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParserPool;

    fn path(p: &str) -> RelPath {
        RelPath::new(p)
    }

    fn failed_path(err: RewriteError) -> RelPath {
        match err {
            RewriteError::VerificationFailed(p) => p,
            other => panic!("expected VerificationFailed, got {other}"),
        }
    }

    fn measure(path: &RelPath, src: &str) -> FileHealth {
        Verifier::new().measure(path, src)
    }

    fn errors(src: &str, file: &str) -> u32 {
        match measure(&path(file), src) {
            FileHealth::Parsed { error_nodes } => error_nodes,
            other => panic!("{file}: expected Parsed, got {other:?} for {src:?}"),
        }
    }

    #[test]
    fn clean_file_stays_clean() {
        let p = path("src/ok.js");
        let before = "function f() { return 1; }\n";
        let after = "function f() { return 2; }\n";
        assert_eq!(errors(before, "src/ok.js"), 0);
        assert_eq!(errors(after, "src/ok.js"), 0);
        let snap = snapshot([(&p, before)]);
        verify(&snap, [(&p, after)]).expect("clean rewrite must pass");
    }

    #[test]
    fn clean_file_with_new_syntax_error_fails_and_names_the_file() {
        let p = path("src/app.js");
        let before = "function f() {}\n";
        let after = "function f( {}\n";
        assert_eq!(errors(before, "src/app.js"), 0);
        assert!(errors(after, "src/app.js") > 0);

        let err = verify_rewrite([(&p, before)], [(&p, after)]).unwrap_err();
        let msg = err.to_string();
        assert_eq!(failed_path(err).as_str(), "src/app.js");
        assert!(
            msg.contains("src/app.js"),
            "error must name the file, got {msg}"
        );
    }

    #[test]
    fn pre_existing_errors_with_unchanged_count_pass() {
        let p = path("legacy.js");
        // One recovery site; renaming the identifier must not add another.
        let before = "function f( {}\n";
        let after = "function renamed( {}\n";
        let n_before = errors(before, "legacy.js");
        let n_after = errors(after, "legacy.js");
        assert!(n_before > 0, "fixture must actually be broken");
        assert_eq!(
            n_before, n_after,
            "rename of a broken file must not change the error-node count ({n_before} -> {n_after})"
        );
        verify_rewrite([(&p, before)], [(&p, after)])
            .expect("pre-existing errors at the same count must pass");
    }

    #[test]
    fn error_count_increase_fails() {
        let p = path("worsen.js");
        let before = "function f( {}\n";
        let after = "function f( {}\nfunction g( {}\nfunction h( {}\n";
        let n_before = errors(before, "worsen.js");
        let n_after = errors(after, "worsen.js");
        assert_eq!(
            n_before, 1,
            "before fixture should be a single recovery site"
        );
        assert_eq!(
            n_after, 3,
            "after fixture should add two more recovery sites"
        );
        let err = verify_rewrite([(&p, before)], [(&p, after)]).unwrap_err();
        assert_eq!(failed_path(err).as_str(), "worsen.js");
    }

    #[test]
    fn becoming_unparseable_fails() {
        let p = path("gone.js");
        let src = "function f() {}\n";
        let verifier = Verifier::new();
        let before = verifier.snapshot([(&p, src)]);
        assert_eq!(before.get(&p), Some(FileHealth::Parsed { error_nodes: 0 }));

        // Dropping the pooled parser is the same outcome as parse() returning
        // None (timeout / NoTree): a file that had a tree now has none.
        *verifier.parsers[language_index(Language::JavaScript).expect("js")]
            .lock()
            .unwrap() = None;
        let err = verifier.verify(&before, [(&p, src)]).unwrap_err();
        let msg = err.to_string();
        assert_eq!(failed_path(err).as_str(), "gone.js");
        assert!(
            msg.contains("gone.js"),
            "error must name the file, got {msg}"
        );
    }

    #[test]
    fn already_unparseable_stays_unparseable_and_does_not_block() {
        let p = path("forever.js");
        let verifier = Verifier::new();
        *verifier.parsers[language_index(Language::JavaScript).expect("js")]
            .lock()
            .unwrap() = None;
        let snap = verifier.snapshot([(&p, "function f() {}\n")]);
        assert_eq!(snap.get(&p), Some(FileHealth::Unparseable));
        verifier
            .verify(&snap, [(&p, "function f() {}\n")])
            .expect("a file that never produced a tree must not veto the rewrite");
    }

    #[test]
    fn javascript_recovers_with_error_nodes_rust_with_missing_tokens() {
        // JS collapses a truncated function into a single ERROR wrapper.
        assert_eq!(errors("function f( {}\n", "a.js"), 1);
        // Three truncated functions nest as three ERROR nodes, not one.
        assert_eq!(
            errors("function f( {}\nfunction g( {}\nfunction h( {}\n", "a.js"),
            3
        );
        // Rust keeps the function_item and inserts a MISSING ")".
        assert_eq!(errors("fn f( {}\n", "a.rs"), 1);
        assert_eq!(errors("fn f() {}\nfn g( {}\nfn h( {}\n", "a.rs"), 2);
    }

    #[test]
    fn non_code_files_are_skipped() {
        let md = path("README.md");
        let json = path("package.json");
        let js = path("ok.js");
        assert_eq!(
            measure(&md, "# heading\n```\nfunction f( {\n```\n"),
            FileHealth::Skipped
        );
        assert_eq!(measure(&json, "{"), FileHealth::Skipped);

        let before_md = "# docs\n";
        let after_md = "# docs\n\nfunction f( {\n";
        let before_js = "function f() {}\n";
        let after_js = "function f() { return 1; }\n";
        verify_rewrite(
            [(&md, before_md), (&json, "{"), (&js, before_js)],
            [(&md, after_md), (&json, "{"), (&js, after_js)],
        )
        .expect("markdown/json must not be parsed, and must not fail verification");
    }

    #[test]
    fn multi_file_failure_names_the_broken_file() {
        let ok = path("a.js");
        let bad = path("b.js");
        let also_ok = path("c.js");
        let err = verify_rewrite(
            [
                (&ok, "function f() {}\n"),
                (&bad, "function g() {}\n"),
                (&also_ok, "function h() {}\n"),
            ],
            [
                (&ok, "function f() { return 1; }\n"),
                (&bad, "function g( {}\n"),
                (&also_ok, "function h() { return 2; }\n"),
            ],
        )
        .unwrap_err();
        assert_eq!(failed_path(err).as_str(), "b.js");
    }

    #[test]
    fn multi_file_reports_lexicographically_first_regression() {
        let z = path("z.js");
        let a = path("a.js");
        let err = verify_rewrite(
            [(&z, "function z() {}\n"), (&a, "function a() {}\n")],
            [(&z, "function z( {}\n"), (&a, "function a( {}\n")],
        )
        .unwrap_err();
        assert_eq!(failed_path(err).as_str(), "a.js");
    }

    #[test]
    fn parser_pool_returns_ok_for_syntax_errors() {
        // Why this module walks the tree instead of calling ParserPool::parse.
        let pool = ParserPool::new();
        let parsed = pool
            .parse(Language::JavaScript, &path("broken.js"), "function f( {}")
            .expect("error recovery still yields ParsedFile");
        assert!(
            parsed.symbols.len() <= 1,
            "pool success must not be treated as parse-clean"
        );
        assert!(errors("function f( {}", "broken.js") > 0);
    }

    #[test]
    fn rust_and_python_error_nodes_are_counted_too() {
        assert_eq!(errors("fn f() {}\n", "lib.rs"), 0);
        assert!(errors("fn f( {}\n", "lib.rs") > 0);
        assert_eq!(errors("def f():\n    return 1\n", "m.py"), 0);
        assert!(errors("def f(\n", "m.py") > 0);
    }

    #[test]
    fn empty_source_is_clean_not_a_parse_failure() {
        assert_eq!(
            measure(&path("empty.js"), ""),
            FileHealth::Parsed { error_nodes: 0 }
        );
        let p = path("empty.js");
        verify_rewrite([(&p, "")], [(&p, "")]).unwrap();
    }
}
