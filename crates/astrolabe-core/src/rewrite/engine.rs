//! Rewrite *plan* generation: decide which sites to change, never write.
//!
//! The three parent-module rules that apply here:
//!
//! 1. A plan is computed and returned. This file does not touch the
//!    filesystem — application lives in the transaction workstream.
//! 2. Plan-level confidence is the **weakest** evidence among all included
//!    sites. A single [`Evidence::NameMatch`] site makes the whole plan
//!    [`Confidence::Syntactic`] and therefore not auto-applicable.
//! 3. Lookalikes that must not change go in [`RewritePlan::excluded`] with a
//!    non-empty reason. An empty `excluded` would hide over-narrow plans.
//!
//! `ast-grep` is not used. Its documentation is explicit that it performs no
//! scope, type, or dataflow analysis, so it cannot decide *which* sites to
//! change. Identifier scanning plus an injected [`ScopeResolver`] decides;
//! string slicing records [`Site::current`] for later stale-file checks.
//! Execution-layer tools may still use ast-grep; this module will not.

use crate::types::{Confidence, Language, RelPath};

use super::{ByteRange, Evidence, RewriteError, RewritePlan, ScopeResolver, Site};

/// One file the planner should inspect.
///
/// The caller chooses the candidate set: a single file for a local rename,
/// or a dependency-graph neighborhood for a cross-file rename. This layer
/// does not walk the graph and does not read the disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSource {
    pub path: RelPath,
    pub source: String,
}

impl FileSource {
    pub fn new(path: impl AsRef<str>, source: impl Into<String>) -> Self {
        FileSource {
            path: RelPath::new(path.as_ref()),
            source: source.into(),
        }
    }
}

/// Build a [`RewritePlan`] for renaming `symbol` to `new_name`.
///
/// `at` locates the declaration (or a use of it) in `origin`. The same
/// range is passed to [`ScopeResolver::bindings`] for every candidate file:
/// a file-local resolver may return nothing on other files, a cross-file
/// resolver may use it as the declaration key.
///
/// A file that answers [`RewriteError::NoResolver`] falls back to
/// identifier name matching on that file only. Those sites carry
/// [`Evidence::NameMatch`] and drag the whole plan down to
/// [`Confidence::Syntactic`].
pub fn plan_rename(
    resolver: &dyn ScopeResolver,
    symbol: &str,
    new_name: &str,
    origin: &RelPath,
    at: ByteRange,
    files: &[FileSource],
) -> Result<RewritePlan, RewriteError> {
    validate_new_name(new_name, origin, files)?;

    let mut files: Vec<&FileSource> = files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);

    let mut sites: Vec<(Site, Evidence)> = Vec::new();
    let mut excluded: Vec<(Site, String)> = Vec::new();
    // A file-local resolver can tell that a symbol is exported but cannot see
    // its uses elsewhere. If the origin file says so, the candidate set has to
    // be treated as possibly incomplete — renaming a `pub fn` while callers in
    // unexamined files keep the old name is the failure this module exists to
    // prevent.
    let mut may_span_files = false;

    for file in files {
        if &file.path == origin {
            may_span_files = resolver.may_span_files(&file.path, &file.source, at);
        }
        let planned = plan_file(resolver, symbol, new_name, at, file)?;
        sites.extend(planned.sites);
        excluded.extend(planned.excluded);
    }

    if let Some(path) = first_overlap(sites.iter().map(|(s, _)| s)) {
        return Err(RewriteError::Overlapping(path));
    }

    let (evidence, mut confidence) = match weakest_evidence(sites.iter().map(|(_, e)| *e)) {
        Some(evidence) => (evidence, confidence_of(evidence)),
        None => (resolver.evidence(), Confidence::Unknown),
    };

    // Only a language server can see across files. Anything weaker that
    // touches a possibly-exported symbol is a candidate list, not a complete
    // plan, so it must not qualify for automatic application.
    if may_span_files && evidence != Evidence::LanguageServer {
        confidence = Confidence::Syntactic.max(confidence);
    }

    sites.sort_by(|(a, _), (b, _)| compare_sites(a, b));
    excluded.sort_by(|(a, _), (b, _)| compare_sites(a, b));

    Ok(RewritePlan {
        symbol: symbol.to_owned(),
        new_name: new_name.to_owned(),
        sites: sites.into_iter().map(|(s, _)| s).collect(),
        evidence,
        confidence,
        excluded,
    })
}

struct FilePlan {
    sites: Vec<(Site, Evidence)>,
    excluded: Vec<(Site, String)>,
}

fn plan_file(
    resolver: &dyn ScopeResolver,
    symbol: &str,
    new_name: &str,
    at: ByteRange,
    file: &FileSource,
) -> Result<FilePlan, RewriteError> {
    let lang = Language::from_path(&file.path);
    let class = classify(&file.source, lang);
    let hits = ident_hits(&file.source, symbol);

    let (claimed, evidence, name_match_fallback) =
        match resolver.bindings(&file.path, &file.source, at) {
            Ok(ranges) => (dedup_ranges(ranges), resolver.evidence(), false),
            Err(RewriteError::NoResolver(_)) => (Vec::new(), Evidence::NameMatch, true),
            Err(err) => return Err(err),
        };

    if ranges_overlap_in(&claimed) {
        return Err(RewriteError::Overlapping(file.path.clone()));
    }

    let mut sites = Vec::new();
    let mut excluded = Vec::new();

    // The resolver decides which ranges to change. Record `current` as
    // sliced, even when it does not spell `symbol` — application checks
    // staleness against that snapshot.
    for range in &claimed {
        if range.start >= range.end {
            continue;
        }
        let current = current_text(&file.path, &file.source, *range)?;
        sites.push((make_site(&file.path, *range, current, new_name), evidence));
    }

    for hit in hits {
        if claimed.iter().any(|c| ranges_overlap(*c, hit)) {
            continue;
        }
        let current = current_text(&file.path, &file.source, hit)?;
        let site = make_site(&file.path, hit, current, new_name);
        let why = classify_hit(&class, hit.start);
        if name_match_fallback && why == ExcludeWhy::OtherBinding {
            sites.push((site, Evidence::NameMatch));
            continue;
        }
        excluded.push((
            site,
            exclude_reason(&file.path, &file.source, hit.start, symbol, why),
        ));
    }

    Ok(FilePlan { sites, excluded })
}

fn make_site(path: &RelPath, range: ByteRange, current: String, new_name: &str) -> Site {
    Site {
        path: path.clone(),
        range,
        current,
        replacement: new_name.to_owned(),
    }
}

fn current_text(path: &RelPath, source: &str, range: ByteRange) -> Result<String, RewriteError> {
    if range.end > source.len() || range.start > range.end {
        return Err(RewriteError::Io(
            path.clone(),
            format!(
                "byte range {}..{} is outside the file ({} bytes)",
                range.start,
                range.end,
                source.len()
            ),
        ));
    }
    if !source.is_char_boundary(range.start) || !source.is_char_boundary(range.end) {
        return Err(RewriteError::Io(
            path.clone(),
            format!(
                "byte range {}..{} is not on a UTF-8 boundary",
                range.start, range.end
            ),
        ));
    }
    Ok(source[range.start..range.end].to_owned())
}

fn compare_sites(a: &Site, b: &Site) -> std::cmp::Ordering {
    a.path
        .cmp(&b.path)
        .then(a.range.start.cmp(&b.range.start))
        .then(a.range.end.cmp(&b.range.end))
}

fn first_overlap<'a, I>(sites: I) -> Option<RelPath>
where
    I: Iterator<Item = &'a Site>,
{
    let mut keys: Vec<(&RelPath, ByteRange)> = sites.map(|s| (&s.path, s.range)).collect();
    keys.sort_by(|a, b| {
        a.0.cmp(b.0)
            .then(a.1.start.cmp(&b.1.start))
            .then(a.1.end.cmp(&b.1.end))
    });
    keys.dedup();
    for pair in keys.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1.end > pair[1].1.start {
            return Some(pair[0].0.clone());
        }
    }
    None
}

fn dedup_ranges(mut ranges: Vec<ByteRange>) -> Vec<ByteRange> {
    ranges.retain(|r| r.start < r.end);
    ranges.sort();
    ranges.dedup();
    ranges
}

fn ranges_overlap(a: ByteRange, b: ByteRange) -> bool {
    a.start < b.end && b.start < a.end
}

fn ranges_overlap_in(ranges: &[ByteRange]) -> bool {
    ranges.windows(2).any(|pair| pair[0].end > pair[1].start)
}

/// Strength order: [`Evidence::LanguageServer`] > [`Evidence::ScopeBinding`]
/// > [`Evidence::NameMatch`]. Weakest is the minimum of that order.
fn evidence_rank(e: Evidence) -> u8 {
    match e {
        Evidence::NameMatch => 0,
        Evidence::ScopeBinding => 1,
        Evidence::LanguageServer => 2,
    }
}

fn weakest_evidence(iter: impl Iterator<Item = Evidence>) -> Option<Evidence> {
    iter.min_by_key(|e| evidence_rank(*e))
}

fn confidence_of(evidence: Evidence) -> Confidence {
    match evidence {
        Evidence::LanguageServer => Confidence::Exact,
        Evidence::ScopeBinding => Confidence::Scoped,
        Evidence::NameMatch => Confidence::Syntactic,
    }
}

// ---------------------------------------------------------------- names

fn validate_new_name(
    new_name: &str,
    origin: &RelPath,
    files: &[FileSource],
) -> Result<(), RewriteError> {
    let mut langs: Vec<Language> = files
        .iter()
        .filter_map(|f| Language::from_path(&f.path))
        .collect();
    if let Some(lang) = Language::from_path(origin) {
        langs.push(lang);
    }
    langs.sort();
    langs.dedup();

    let fail = |reason: String| -> Result<(), RewriteError> {
        Err(RewriteError::Io(origin.clone(), reason))
    };

    if new_name.is_empty() {
        return fail("new name is empty".into());
    }
    if new_name.chars().any(char::is_whitespace) {
        return fail(format!("new name `{new_name}` contains whitespace"));
    }
    if new_name.starts_with(|c: char| c.is_ascii_digit()) {
        return fail(format!("new name `{new_name}` starts with a digit"));
    }

    if langs.is_empty() {
        if !generic_ident(new_name) {
            return fail(format!("new name `{new_name}` is not a valid identifier"));
        }
        return Ok(());
    }

    for lang in langs {
        if !ident_ok(new_name, lang) {
            return fail(format!(
                "new name `{new_name}` is not a valid {} identifier",
                lang.name()
            ));
        }
        if is_keyword(new_name, lang) {
            return fail(format!(
                "new name `{new_name}` is a reserved keyword in {}",
                lang.name()
            ));
        }
    }
    Ok(())
}

fn generic_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    ident_start(first, None) && chars.all(|c| ident_continue(c, None))
}

fn ident_ok(name: &str, lang: Language) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    ident_start(first, Some(lang)) && chars.all(|c| ident_continue(c, Some(lang)))
}

fn ident_start(c: char, lang: Option<Language>) -> bool {
    if c.is_ascii_digit() {
        return false;
    }
    match lang {
        Some(
            Language::Java
            | Language::TypeScript
            | Language::Tsx
            | Language::JavaScript
            | Language::Php
            | Language::Vue,
        ) => c.is_alphabetic() || c == '_' || c == '$',
        Some(
            Language::Python
            | Language::Go
            | Language::Rust
            | Language::C
            | Language::Cpp
            | Language::ObjC
            | Language::ObjCpp
            | Language::Swift,
        )
        | None => c.is_alphabetic() || c == '_' || (lang.is_none() && c == '$'),
    }
}

fn ident_continue(c: char, lang: Option<Language>) -> bool {
    match lang {
        Some(
            Language::Java
            | Language::TypeScript
            | Language::Tsx
            | Language::JavaScript
            | Language::Php
            | Language::Vue,
        ) => c.is_alphanumeric() || c == '_' || c == '$',
        Some(
            Language::Python
            | Language::Go
            | Language::Rust
            | Language::C
            | Language::Cpp
            | Language::ObjC
            | Language::ObjCpp
            | Language::Swift,
        )
        | None => c.is_alphanumeric() || c == '_' || (lang.is_none() && c == '$'),
    }
}

fn is_keyword(name: &str, lang: Language) -> bool {
    keywords(lang).binary_search(&name).is_ok()
}

/// Python 3 keywords, including the soft keywords `match` / `case` / `type`.
/// Source: <https://docs.python.org/3/reference/lexical_analysis.html#keywords>
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "case", "class",
    "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if",
    "import", "in", "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return",
    "try", "type", "while", "with", "yield",
];

/// Go keywords. Source: <https://go.dev/ref/spec#Keywords>
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];

/// Java reserved words and literals. Source: JLS §3.9 plus `true` / `false` /
/// `null` and the restricted identifiers `var`, `yield`, `record`,
/// `sealed`, `permits`.
const JAVA_KEYWORDS: &[&str] = &[
    "_",
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "permits",
    "private",
    "protected",
    "public",
    "record",
    "return",
    "sealed",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "var",
    "void",
    "volatile",
    "while",
    "yield",
];

/// Rust strict, reserved, and weak keywords.
/// Source: <https://doc.rust-lang.org/reference/keywords.html>
const RUST_KEYWORDS: &[&str] = &[
    "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if",
    "impl", "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// ECMA-262 reserved words, including strict-mode reserved words. Used for
/// JavaScript, TypeScript, and TSX. Source: ECMA-262 §12.6 plus MDN
/// "Reserved words". `type` is a TypeScript contextual keyword and remains a
/// valid identifier, so it is not listed.
const JS_KEYWORDS: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

// Thin keyword lists for P0 languages (rewrite planner). Full sets can land
// with language-specific agents; empty/small lists only affect is_keyword rejects.
const PHP_KEYWORDS: &[&str] = &[
    "abstract", "and", "array", "as", "break", "callable", "case", "catch",
    "class", "clone", "const", "continue", "declare", "default", "do", "echo",
    "else", "elseif", "empty", "enddeclare", "endfor", "endforeach", "endif",
    "endswitch", "endwhile", "enum", "extends", "final", "finally", "fn",
    "for", "foreach", "function", "global", "goto", "if", "implements",
    "include", "include_once", "instanceof", "insteadof", "interface",
    "isset", "list", "match", "namespace", "new", "or", "print", "private",
    "protected", "public", "readonly", "require", "require_once", "return",
    "static", "switch", "throw", "trait", "try", "unset", "use", "var",
    "while", "xor", "yield",
];

const C_FAMILY_KEYWORDS: &[&str] = &[
    "alignas", "alignof", "asm", "auto", "bool", "break", "case", "catch",
    "char", "class", "const", "constexpr", "continue", "default", "delete",
    "do", "double", "else", "enum", "explicit", "extern", "false", "float",
    "for", "friend", "goto", "if", "inline", "int", "long", "mutable",
    "namespace", "new", "noexcept", "nullptr", "operator", "private",
    "protected", "public", "register", "return", "short", "signed", "sizeof",
    "static", "struct", "switch", "template", "this", "throw", "true", "try",
    "typedef", "typeid", "typename", "union", "unsigned", "using", "virtual",
    "void", "volatile", "while",
];

const SWIFT_KEYWORDS: &[&str] = &[
    "associatedtype", "class", "deinit", "enum", "extension", "fileprivate",
    "func", "import", "init", "inout", "internal", "let", "operator",
    "private", "protocol", "public", "rethrows", "static", "struct",
    "subscript", "typealias", "var", "break", "case", "continue", "default",
    "defer", "do", "else", "fallthrough", "for", "guard", "if", "in",
    "repeat", "return", "switch", "where", "while", "as", "Any", "catch",
    "false", "is", "nil", "super", "self", "Self", "throw", "throws", "true",
    "try",
];

fn keywords(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Python => PYTHON_KEYWORDS,
        Language::Go => GO_KEYWORDS,
        Language::Java => JAVA_KEYWORDS,
        Language::Rust => RUST_KEYWORDS,
        Language::TypeScript | Language::Tsx | Language::JavaScript => JS_KEYWORDS,
        Language::Php => PHP_KEYWORDS,
        Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp => C_FAMILY_KEYWORDS,
        Language::Swift => SWIFT_KEYWORDS,
        Language::Vue => JS_KEYWORDS,
    }
}

// -------------------------------------------------------------- occurrences

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SliceKind {
    Code,
    String,
    Comment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExcludeWhy {
    String,
    Comment,
    OtherBinding,
}

fn classify_hit(class: &[SliceKind], start: usize) -> ExcludeWhy {
    match class.get(start).copied().unwrap_or(SliceKind::Code) {
        SliceKind::String => ExcludeWhy::String,
        SliceKind::Comment => ExcludeWhy::Comment,
        SliceKind::Code => ExcludeWhy::OtherBinding,
    }
}

fn exclude_reason(
    path: &RelPath,
    source: &str,
    start: usize,
    symbol: &str,
    why: ExcludeWhy,
) -> String {
    let line = line_number(source, start);
    let where_ = match why {
        ExcludeWhy::String => "在字符串字面量里",
        ExcludeWhy::Comment => "在注释里",
        ExcludeWhy::OtherBinding => "绑定到另一个声明",
    };
    format!("`{path}:{line}` 的 `{symbol}` {where_}")
}

fn line_number(source: &str, byte: usize) -> usize {
    let end = byte.min(source.len());
    source[..end].bytes().filter(|&b| b == b'\n').count() + 1
}

fn ident_hits(source: &str, name: &str) -> Vec<ByteRange> {
    if name.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(rel) = source[from..].find(name) {
        let start = from + rel;
        let end = start + name.len();
        if is_ident_boundary(source, start, end) {
            hits.push(ByteRange { start, end });
            from = end;
        } else {
            from = start + name.len().max(1);
        }
    }
    hits
}

fn is_ident_boundary(source: &str, start: usize, end: usize) -> bool {
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return false;
    }
    let before = source[..start].chars().next_back();
    let after = source[end..].chars().next();
    !before.is_some_and(generic_ident_char) && !after.is_some_and(generic_ident_char)
}

fn generic_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

#[derive(Clone, Copy)]
enum Flavor {
    Go,
    Java,
    Rust,
    Js,
}

fn classify(source: &str, lang: Option<Language>) -> Vec<SliceKind> {
    let mut kind = vec![SliceKind::Code; source.len()];
    match lang {
        Some(Language::Python) => classify_python(source.as_bytes(), &mut kind),
        Some(Language::Go) => classify_c_like(source.as_bytes(), &mut kind, Flavor::Go),
        Some(Language::Java | Language::Php) => {
            classify_c_like(source.as_bytes(), &mut kind, Flavor::Java)
        }
        Some(Language::Rust) => classify_c_like(source.as_bytes(), &mut kind, Flavor::Rust),
        Some(
            Language::TypeScript
            | Language::Tsx
            | Language::JavaScript
            | Language::Vue
            | Language::C
            | Language::Cpp
            | Language::ObjC
            | Language::ObjCpp
            | Language::Swift,
        )
        | None => classify_c_like(source.as_bytes(), &mut kind, Flavor::Js),
    }
    kind
}

fn mark(kind: &mut [SliceKind], start: usize, end: usize, k: SliceKind) {
    let end = end.min(kind.len());
    let start = start.min(end);
    kind[start..end].fill(k);
}

fn classify_python(bytes: &[u8], kind: &mut [SliceKind]) {
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        if bytes[i] == b'#' {
            let start = i;
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            mark(kind, start, i, SliceKind::Comment);
            continue;
        }
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            let triple = i + 2 < n && bytes[i + 1] == quote && bytes[i + 2] == quote;
            let raw = python_raw_prefix(bytes, i);
            let start = i;
            i += if triple { 3 } else { 1 };
            if triple {
                while i + 2 < n
                    && !(bytes[i] == quote && bytes[i + 1] == quote && bytes[i + 2] == quote)
                {
                    i += 1;
                }
                i = (i + 3).min(n);
            } else {
                while i < n && bytes[i] != quote {
                    if !raw && bytes[i] == b'\\' && i + 1 < n {
                        i += 2;
                        continue;
                    }
                    i += 1;
                }
                if i < n {
                    i += 1;
                }
            }
            mark(kind, start, i, SliceKind::String);
            continue;
        }
        i += 1;
    }
}

fn python_raw_prefix(bytes: &[u8], quote_at: usize) -> bool {
    let mut j = quote_at;
    let mut raw = false;
    let mut count = 0;
    while j > 0 && count < 2 {
        let c = bytes[j - 1];
        if !matches!(c, b'r' | b'R' | b'u' | b'U' | b'f' | b'F' | b'b' | b'B') {
            break;
        }
        if matches!(c, b'r' | b'R') {
            raw = true;
        }
        j -= 1;
        count += 1;
    }
    if j > 0 && generic_ident_char(bytes[j - 1] as char) {
        return false;
    }
    raw
}

fn classify_c_like(bytes: &[u8], kind: &mut [SliceKind], flavor: Flavor) {
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        if matches!(flavor, Flavor::Rust) {
            if let Some((start_len, hashes)) = rust_raw_string(bytes, i) {
                let start = i;
                i += start_len;
                while i < n {
                    if bytes[i] == b'"' && rust_raw_closer(bytes, i + 1, hashes) {
                        i += 1 + hashes;
                        break;
                    }
                    i += 1;
                }
                mark(kind, start, i, SliceKind::String);
                continue;
            }
        }
        if matches!(flavor, Flavor::Java)
            && bytes[i] == b'"'
            && i + 2 < n
            && bytes[i + 1] == b'"'
            && bytes[i + 2] == b'"'
        {
            let start = i;
            i += 3;
            while i + 2 < n && !(bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"') {
                i += 1;
            }
            i = (i + 3).min(n);
            mark(kind, start, i, SliceKind::String);
            continue;
        }
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            let start = i;
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            mark(kind, start, i, SliceKind::Comment);
            continue;
        }
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut depth = 1u32;
            while i < n && depth > 0 {
                if matches!(flavor, Flavor::Rust)
                    && bytes[i] == b'/'
                    && i + 1 < n
                    && bytes[i + 1] == b'*'
                {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            mark(kind, start, i, SliceKind::Comment);
            continue;
        }
        if bytes[i] == b'"' {
            i = consume_quoted(bytes, kind, i, b'"', true);
            continue;
        }
        if bytes[i] == b'\'' {
            if matches!(flavor, Flavor::Rust) && rust_lifetime(bytes, i) {
                i += 1;
                continue;
            }
            i = consume_quoted(bytes, kind, i, b'\'', true);
            continue;
        }
        if bytes[i] == b'`' && matches!(flavor, Flavor::Go | Flavor::Js) {
            let escapes = matches!(flavor, Flavor::Js);
            i = consume_quoted(bytes, kind, i, b'`', escapes);
            continue;
        }
        i += 1;
    }
}

fn consume_quoted(
    bytes: &[u8],
    kind: &mut [SliceKind],
    start: usize,
    quote: u8,
    escapes: bool,
) -> usize {
    let n = bytes.len();
    let mut i = start + 1;
    while i < n && bytes[i] != quote {
        if escapes && bytes[i] == b'\\' && i + 1 < n {
            i += 2;
            continue;
        }
        i += 1;
    }
    if i < n {
        i += 1;
    }
    mark(kind, start, i, SliceKind::String);
    i
}

fn rust_lifetime(bytes: &[u8], i: usize) -> bool {
    let Some(&next) = bytes.get(i + 1) else {
        return false;
    };
    let next = next as char;
    if !(next.is_alphabetic() || next == '_') {
        return false;
    }
    let mut j = i + 2;
    while j < bytes.len() {
        let c = bytes[j] as char;
        if !(c.is_alphanumeric() || c == '_') {
            break;
        }
        j += 1;
    }
    bytes.get(j) != Some(&b'\'') || j > i + 2
}

fn rust_raw_string(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if matches!(bytes.get(j), Some(&b'b') | Some(&b'c')) {
        j += 1;
    }
    if bytes.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hash_start = j;
    while bytes.get(j) == Some(&b'#') {
        j += 1;
    }
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    let hashes = j - hash_start;
    Some((j - i + 1, hashes))
}

fn rust_raw_closer(bytes: &[u8], i: usize, hashes: usize) -> bool {
    match bytes.get(i..i + hashes) {
        Some(slice) => slice.iter().all(|&c| c == b'#'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    struct FakeResolver {
        evidence: Evidence,
        files: BTreeMap<String, Vec<ByteRange>>,
        unresolved: BTreeSet<String>,
        /// Most cases rename a file-local symbol; the cross-file guard has
        /// its own test that flips this on.
        may_span_files: bool,
    }

    impl FakeResolver {
        fn scoped(files: &[(&str, Vec<ByteRange>)]) -> Self {
            FakeResolver {
                evidence: Evidence::ScopeBinding,
                files: files
                    .iter()
                    .map(|(p, r)| (p.to_string(), r.clone()))
                    .collect(),
                unresolved: BTreeSet::new(),
                may_span_files: false,
            }
        }
    }

    impl ScopeResolver for FakeResolver {
        fn bindings(
            &self,
            path: &RelPath,
            _source: &str,
            _at: ByteRange,
        ) -> Result<Vec<ByteRange>, RewriteError> {
            if self.unresolved.contains(path.as_str()) {
                return Err(RewriteError::NoResolver(
                    Language::from_path(path)
                        .map(|l| l.name().to_string())
                        .unwrap_or_else(|| "unknown".into()),
                ));
            }
            Ok(self.files.get(path.as_str()).cloned().unwrap_or_default())
        }

        fn evidence(&self) -> Evidence {
            self.evidence
        }

        fn may_span_files(&self, _path: &RelPath, _source: &str, _at: ByteRange) -> bool {
            self.may_span_files
        }
    }

    fn first_ident(source: &str, name: &str) -> ByteRange {
        ident_hits(source, name)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no identifier `{name}` in source"))
    }

    fn plan(
        resolver: &FakeResolver,
        symbol: &str,
        new_name: &str,
        origin: &str,
        source: &str,
        extras: &[FileSource],
    ) -> Result<RewritePlan, RewriteError> {
        let origin = RelPath::new(origin);
        let mut files = vec![FileSource::new(origin.as_str(), source)];
        files.extend(extras.iter().cloned());
        let at = first_ident(source, symbol);
        plan_rename(resolver, symbol, new_name, &origin, at, &files)
    }

    #[test]
    fn keyword_tables_are_sorted() {
        for table in [
            PYTHON_KEYWORDS,
            GO_KEYWORDS,
            JAVA_KEYWORDS,
            RUST_KEYWORDS,
            JS_KEYWORDS,
        ] {
            let mut sorted = table.to_vec();
            sorted.sort();
            assert_eq!(table, sorted.as_slice());
        }
    }

    #[test]
    fn exported_symbol_is_not_auto_applicable_from_a_file_local_resolver() {
        // A `pub`/exported symbol may be referenced from files the resolver
        // never saw. The bindings it did find are correct, but the plan is a
        // candidate list rather than a complete rename, so it must not clear
        // the bar for automatic application.
        let source = "export function handler() { return handler; }\n";
        let hits = ident_hits(source, "handler");
        let mut resolver = FakeResolver::scoped(&[("a.ts", hits)]);
        resolver.may_span_files = true;

        let plan = plan(&resolver, "handler", "onEvent", "a.ts", source, &[]).unwrap();

        assert_eq!(
            plan.evidence,
            Evidence::ScopeBinding,
            "the bindings themselves are still scope-resolved"
        );
        assert_eq!(
            plan.confidence,
            Confidence::Syntactic,
            "but completeness across files is unproven"
        );
        assert!(
            !plan.is_auto_applicable(),
            "an incomplete rename must be reviewed, not applied"
        );
    }

    #[test]
    fn single_file_plan_includes_declaration_and_use() {
        let source = "function handler() { return handler; }\n";
        let hits = ident_hits(source, "handler");
        assert_eq!(hits.len(), 2);
        let resolver = FakeResolver::scoped(&[("a.ts", hits.clone())]);
        let plan = plan(&resolver, "handler", "onEvent", "a.ts", source, &[]).unwrap();

        assert_eq!(plan.symbol, "handler");
        assert_eq!(plan.new_name, "onEvent");
        assert_eq!(plan.sites.len(), 2);
        assert_eq!(plan.evidence, Evidence::ScopeBinding);
        assert_eq!(plan.confidence, Confidence::Scoped);
        assert!(plan.is_auto_applicable());
        assert_eq!(
            plan.files()
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>(),
            ["a.ts"]
        );
        assert!(plan.excluded.is_empty());
        for site in &plan.sites {
            assert_eq!(site.current, "handler");
            assert_eq!(site.replacement, "onEvent");
            assert_eq!(site.path.as_str(), "a.ts");
        }
    }

    #[test]
    fn multi_file_plan_collects_sites_from_each_candidate() {
        let a = "export function handler() { return 1; }\n";
        let b = "import { handler } from './a';\nexport const x = handler();\n";
        let a_hits = ident_hits(a, "handler");
        let b_hits = ident_hits(b, "handler");
        let resolver = FakeResolver::scoped(&[("a.ts", a_hits), ("b.ts", b_hits.clone())]);
        let origin = RelPath::new("a.ts");
        let files = [FileSource::new("a.ts", a), FileSource::new("b.ts", b)];
        let plan = plan_rename(
            &resolver,
            "handler",
            "onEvent",
            &origin,
            first_ident(a, "handler"),
            &files,
        )
        .unwrap();

        assert_eq!(plan.sites.len(), 1 + b_hits.len());
        let files: Vec<String> = plan.files().iter().map(|p| p.to_string()).collect();
        assert_eq!(files, vec!["a.ts", "b.ts"]);
        assert_eq!(plan.confidence, Confidence::Scoped);
        assert!(plan.is_auto_applicable());
        assert!(plan.sites.iter().all(|s| s.current == "handler"));
    }

    #[test]
    fn mixed_evidence_takes_the_weakest_confidence() {
        let a = "export function handler() {}\n";
        let b = "export function handler() {}\n";
        let mut resolver = FakeResolver::scoped(&[("a.ts", ident_hits(a, "handler"))]);
        resolver.unresolved.insert("b.ts".into());

        let origin = RelPath::new("a.ts");
        let files = [FileSource::new("a.ts", a), FileSource::new("b.ts", b)];
        let plan = plan_rename(
            &resolver,
            "handler",
            "onEvent",
            &origin,
            first_ident(a, "handler"),
            &files,
        )
        .unwrap();

        assert!(plan.sites.len() >= 2, "both files should contribute sites");
        assert!(
            plan.sites.iter().any(|s| s.path.as_str() == "a.ts"),
            "scoped file kept"
        );
        assert!(
            plan.sites.iter().any(|s| s.path.as_str() == "b.ts"),
            "name-matched file kept"
        );
        assert_eq!(plan.evidence, Evidence::NameMatch);
        assert_eq!(plan.confidence, Confidence::Syntactic);
        assert!(
            !plan.is_auto_applicable(),
            "one NameMatch site must force review of the whole plan"
        );
    }

    #[test]
    fn excluded_records_strings_comments_and_other_bindings() {
        let source = concat!(
            "function handler() { return 1; }\n",
            "const label = \"handler\";\n",
            "// handler leftover\n",
            "function other() { function handler() { return 2; } }\n",
        );
        let all = ident_hits(source, "handler");
        assert!(all.len() >= 3);
        let keep = vec![all[0]];
        let resolver = FakeResolver::scoped(&[("a.ts", keep)]);
        let plan = plan(&resolver, "handler", "onEvent", "a.ts", source, &[]).unwrap();

        assert_eq!(plan.sites.len(), 1);
        assert!(
            !plan.excluded.is_empty(),
            "lookalikes must be returned, not dropped"
        );
        assert!(
            plan.excluded.iter().all(|(_, reason)| !reason.is_empty()),
            "every exclusion needs a reason"
        );
        assert!(
            plan.excluded
                .iter()
                .any(|(_, r)| r.contains("字符串字面量")),
            "string lookalike: {:?}",
            plan.excluded
        );
        assert!(
            plan.excluded.iter().any(|(_, r)| r.contains("注释")),
            "comment lookalike: {:?}",
            plan.excluded
        );
        assert!(
            plan.excluded
                .iter()
                .any(|(_, r)| r.contains("绑定到另一个声明")),
            "shadowed binding: {:?}",
            plan.excluded
        );
        assert!(plan.excluded.iter().any(|(s, r)| {
            r.contains("字符串字面量") && s.current == "handler" && r.contains("a.ts:")
        }));
    }

    #[test]
    fn overlapping_sites_are_rejected() {
        let source = "handler handler\n";
        let overlapping = vec![
            ByteRange { start: 0, end: 7 },
            ByteRange { start: 4, end: 11 },
        ];
        let resolver = FakeResolver::scoped(&[("a.ts", overlapping)]);
        let origin = RelPath::new("a.ts");
        let files = [FileSource::new("a.ts", source)];
        let err = plan_rename(
            &resolver,
            "handler",
            "onEvent",
            &origin,
            ByteRange { start: 0, end: 7 },
            &files,
        )
        .unwrap_err();
        match err {
            RewriteError::Overlapping(path) => assert_eq!(path.as_str(), "a.ts"),
            other => panic!("expected Overlapping, got {other}"),
        }
    }

    #[test]
    fn adjacent_ranges_are_not_overlapping() {
        let source = "handler handler\n";
        let hits = ident_hits(source, "handler");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].end, hits[1].start - 1);
        let resolver = FakeResolver::scoped(&[("a.ts", hits)]);
        let plan = plan(&resolver, "handler", "onEvent", "a.ts", source, &[]).unwrap();
        assert_eq!(plan.sites.len(), 2);
    }

    #[test]
    fn rejects_illegal_new_names() {
        let source = "function handler() {}\n";
        let resolver = FakeResolver::scoped(&[("a.ts", ident_hits(source, "handler"))]);

        let cases: &[(&str, &str, &str)] = &[
            ("a.ts", "class", "keyword"),
            ("a.py", "def", "keyword"),
            ("a.go", "func", "keyword"),
            ("A.java", "class", "keyword"),
            ("a.rs", "fn", "keyword"),
            ("a.ts", "123abc", "digit"),
            ("a.ts", "on Event", "whitespace"),
        ];

        for (origin, new_name, needle) in cases {
            let origin_path = RelPath::new(*origin);
            let files = [FileSource::new(*origin, source)];
            let err = plan_rename(
                &resolver,
                "handler",
                new_name,
                &origin_path,
                first_ident(source, "handler"),
                &files,
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.to_ascii_lowercase().contains(needle),
                "origin={origin} new_name={new_name:?} err={msg}"
            );
        }
    }

    #[test]
    fn site_current_records_original_text() {
        let source = "const handler = 1;\n";
        let range = first_ident(source, "handler");
        let resolver = FakeResolver::scoped(&[("mod.go", vec![range])]);
        let plan = plan(&resolver, "handler", "onEvent", "mod.go", source, &[]).unwrap();
        assert_eq!(plan.sites.len(), 1);
        assert_eq!(plan.sites[0].current, "handler");
        assert_eq!(
            &source[plan.sites[0].range.start..plan.sites[0].range.end],
            "handler"
        );
        assert_eq!(plan.sites[0].replacement, "onEvent");
    }

    #[test]
    fn empty_plan_is_ok_with_unknown_confidence() {
        let source = "const z = 1;\n";
        let resolver = FakeResolver::scoped(&[]);
        let origin = RelPath::new("a.ts");
        let files = [FileSource::new("a.ts", source)];
        let plan = plan_rename(
            &resolver,
            "handler",
            "onEvent",
            &origin,
            ByteRange { start: 0, end: 1 },
            &files,
        )
        .unwrap();

        assert!(plan.sites.is_empty());
        assert!(plan.excluded.is_empty());
        assert_eq!(plan.confidence, Confidence::Unknown);
        assert!(!plan.is_auto_applicable());
        assert_eq!(plan.symbol, "handler");
        assert_eq!(plan.new_name, "onEvent");

        let empty = plan_rename(
            &resolver,
            "handler",
            "onEvent",
            &origin,
            ByteRange { start: 0, end: 1 },
            &[],
        )
        .unwrap();
        assert!(empty.sites.is_empty());
        assert_eq!(empty.confidence, Confidence::Unknown);
    }
}
