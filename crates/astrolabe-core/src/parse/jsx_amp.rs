//! JSX attribute strings with a bare `&` (`href="/?a=1&b=2"`).
//!
//! tree-sitter-typescript (TSX) and tree-sitter-javascript read `&` in a
//! JSX attribute string as the start of an HTML character reference; one
//! that is not `&name;` / `&#38;` / `&#x26;` is an ERROR, and the whole
//! file is flagged partial. React and TypeScript accept it as a literal
//! `&`. For the error retry, each such `&` inside an `attr="…"` / `attr='…'`
//! string is replaced with `_` (same length; string content carries no
//! symbols). The caller keeps the result only when it parses clean.

/// Source with bare `&` in attribute-like strings replaced, or `None` when
/// there is none.
pub(super) fn escape_bare_ampersands(source: &str) -> Option<String> {
    if !source.contains('&') {
        return None;
    }
    let mut out: Option<Vec<u8>> = None;
    let b = source.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // `name=` then optional spaces, then a quote.
        if b[i] != b'=' || i == 0 || !is_attr_char(b[i - 1]) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        let Some(&q) = b.get(j).filter(|&&c| c == b'"' || c == b'\'') else {
            i += 1;
            continue;
        };
        let mut k = j + 1;
        while k < b.len() && b[k] != q && b[k] != b'\n' {
            if b[k] == b'\\' {
                k += 2;
                continue;
            }
            if b[k] == b'&' && !is_char_reference(&b[k + 1..]) {
                let next = b.get(k + 1).copied().unwrap_or(b' ');
                if next.is_ascii_alphanumeric() || next == b'#' {
                    out.get_or_insert_with(|| b.to_vec())[k] = b'_';
                }
            }
            k += 1;
        }
        i = k + 1;
    }
    // Only ASCII `&` bytes were replaced by ASCII.
    out.and_then(|bytes| String::from_utf8(bytes).ok())
}

fn is_attr_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b':' | b'$')
}

/// `name;`, `#38;` or `#x26;` right after an `&`.
fn is_char_reference(rest: &[u8]) -> bool {
    let (digits, body): (fn(&u8) -> bool, &[u8]) = match rest {
        [b'#', b'x' | b'X', tail @ ..] => (u8::is_ascii_hexdigit, tail),
        [b'#', tail @ ..] => (u8::is_ascii_digit, tail),
        _ => (u8::is_ascii_alphanumeric, rest),
    };
    let n = body.iter().take_while(|c| digits(c)).count();
    n > 0 && body.get(n) == Some(&b';')
}

#[cfg(test)]
mod tests {
    use super::escape_bare_ampersands as esc;
    use crate::parse::ParserPool;
    use crate::types::{Language, RelPath};

    #[test]
    fn replaces_only_bare_ampersands_in_attribute_strings() {
        let src = "<a href=\"/?a=1&b=2&amp;c=3&#38;d&#x26;e\" t='x&y' n={a&&b}>p & q&r</a>";
        let out = esc(src).unwrap();
        assert_eq!(out.len(), src.len());
        assert_eq!(
            out,
            "<a href=\"/?a=1_b=2&amp;c=3&#38;d&#x26;e\" t='x_y' n={a&&b}>p & q&r</a>"
        );
        assert!(esc("<a href=\"a & b\">x</a>").is_none());
        assert!(esc("const x = a && b;").is_none());
    }

    #[test]
    fn tsx_and_jsx_files_parse_clean() {
        let src = "export function Footer() {\n  return <a href=\"https://x.dev/?utm_source=t&utm_campaign=oss\" target=\"_blank\">v</a>;\n}\n";
        for (lang, path) in [(Language::Tsx, "f.tsx"), (Language::JavaScript, "f.jsx")] {
            let p = ParserPool::new()
                .parse(lang, &RelPath::new(path), src)
                .expect("parse");
            assert!(!p.partial, "{path}");
            assert!(p.symbols.iter().any(|s| s.name == "Footer"), "{path}");
        }
    }
}
