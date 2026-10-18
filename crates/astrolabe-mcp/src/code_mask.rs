//! Line lexer for `search_code skip_comments`: marks which bytes of each line
//! are real code, i.e. not inside a comment (line or block) nor, optionally,
//! a string literal. Heuristic, per file, no parsing: good enough to separate
//! `SysLog(` calls from `// SysLog(` notes and `"SysLog("` strings.
//!
//! Languages without a lexer return `None` from [`code_mask`]; callers must
//! say so instead of pretending comments were skipped (see [`has_lexer`]).

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    C,
    Go,
    Rust,
    Java,
    Js,
    Dart,
    Kotlin,
    Swift,
    CSharp,
    Scala,
    Php,
    Vue,
    Python,
    Ruby,
    Shell,
    Yaml,
    Toml,
    Lua,
    Sql,
    Vb,
}

fn lang(path: &str) -> Option<Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "inl" | "ipp" | "cu" | "cuh"
        | "m" | "mm" => Lang::C,
        "go" => Lang::Go,
        "rs" => Lang::Rust,
        "java" => Lang::Java,
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => Lang::Js,
        "dart" => Lang::Dart,
        "kt" | "kts" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "cs" | "csx" => Lang::CSharp,
        "scala" => Lang::Scala,
        "php" | "phtml" => Lang::Php,
        "vue" => Lang::Vue,
        "py" | "pyi" => Lang::Python,
        "rb" | "rbw" | "rake" | "gemspec" => Lang::Ruby,
        "sh" | "bash" | "zsh" | "ksh" | "bats" | "zsh-theme" => Lang::Shell,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "lua" => Lang::Lua,
        "sql" => Lang::Sql,
        "vb" => Lang::Vb,
        _ => return None,
    })
}

/// Whether [`code_mask`] can lex this path (by extension).
pub(crate) fn has_lexer(path: &str) -> bool {
    lang(path).is_some()
}

/// Short label of a path's type for "no comment skipping" notes.
pub(crate) fn type_label(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) => format!(".{}", ext.to_ascii_lowercase()),
        None => name.to_string(),
    }
}

impl Lang {
    fn slash_line(self) -> bool {
        matches!(
            self,
            Lang::C
                | Lang::Go
                | Lang::Rust
                | Lang::Java
                | Lang::Js
                | Lang::Dart
                | Lang::Kotlin
                | Lang::Swift
                | Lang::CSharp
                | Lang::Scala
                | Lang::Php
                | Lang::Vue
        )
    }
    fn block_c(self) -> bool {
        self.slash_line() || self == Lang::Sql
    }
    fn nested(self) -> bool {
        matches!(
            self,
            Lang::Rust | Lang::Dart | Lang::Kotlin | Lang::Swift | Lang::Scala
        )
    }
    fn hash_line(self) -> bool {
        matches!(
            self,
            Lang::Php | Lang::Python | Lang::Ruby | Lang::Shell | Lang::Yaml | Lang::Toml
        )
    }
    fn triple(self, q: u8) -> bool {
        match self {
            Lang::Python | Lang::Dart | Lang::Toml => q == b'"' || q == b'\'',
            Lang::Java | Lang::Kotlin | Lang::Swift | Lang::CSharp | Lang::Scala => q == b'"',
            _ => false,
        }
    }
    fn backtick_multi(self) -> bool {
        matches!(self, Lang::Go | Lang::Js)
    }
    /// Is `\` an escape inside a string delimited by `q`?
    fn escapes(self, q: u8) -> bool {
        match self {
            Lang::Sql | Lang::Vb => false,
            Lang::Shell => q != b'\'',
            Lang::Go => q != b'`',
            _ => true,
        }
    }
}

#[derive(Clone, PartialEq)]
enum State {
    Code,
    /// `/* */` block comment with nesting depth.
    Block(u32),
    /// Multi-line string: triple quote (the quote byte) or backtick.
    Multi(u8),
    /// Rust `"` string that continued past the end of a line.
    Str(u8),
    /// Raw string / long comment closed by an exact terminator.
    Raw {
        term: Vec<u8>,
        comment: bool,
    },
    /// Ruby `=begin` … `=end`.
    LineBlock,
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if pat.is_empty() || from > b.len() || b.len() - from < pat.len() {
        return None;
    }
    (from..=b.len() - pat.len()).find(|&p| &b[p..p + pat.len()] == pat)
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `[=*[` at `i`: returns the level and the length of the opener.
fn lua_long_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    while b.get(j) == Some(&b'=') {
        j += 1;
    }
    (b.get(j) == Some(&b'[')).then_some((j - i - 1, j - i + 1))
}

/// Can a `/` at `i` start a JS regex literal? (Previous token is an operator.)
fn regex_allowed(b: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && (b[j - 1] == b' ' || b[j - 1] == b'\t') {
        j -= 1;
    }
    if j == 0 {
        return true;
    }
    let p = b[j - 1];
    if b"=(,:[!&|?{};+-*%<>~^".contains(&p) {
        return true;
    }
    if is_ident(p) {
        let mut s = j;
        while s > 0 && is_ident(b[s - 1]) {
            s -= 1;
        }
        return matches!(
            &b[s..j],
            b"return"
                | b"typeof"
                | b"case"
                | b"in"
                | b"of"
                | b"delete"
                | b"void"
                | b"throw"
                | b"new"
                | b"else"
                | b"do"
                | b"yield"
                | b"await"
        );
    }
    false
}

/// Per line, per byte: `true` when the byte is code. `None` for languages
/// without a lexer. With `strings_are_code`, only comments are masked.
pub(crate) fn code_mask(
    path: &str,
    lines: &[String],
    strings_are_code: bool,
) -> Option<Vec<Vec<bool>>> {
    let lg = lang(path)?;
    let mut state = State::Code;
    // JS template literals: brace depth of each open `${ … }`.
    let mut tmpl: Vec<u32> = Vec::new();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let b = line.as_bytes();
        let n = b.len();
        let mut mask = vec![true; n];
        let comment = |mask: &mut Vec<bool>, from: usize, to: usize| {
            for m in &mut mask[from.min(n)..to.min(n)] {
                *m = false;
            }
        };
        let string = |mask: &mut Vec<bool>, from: usize, to: usize| {
            if !strings_are_code {
                for m in &mut mask[from.min(n)..to.min(n)] {
                    *m = false;
                }
            }
        };
        if lg == Lang::Ruby {
            if state == State::Code && line.starts_with("=begin") {
                state = State::LineBlock;
            }
            if state == State::LineBlock {
                comment(&mut mask, 0, n);
                if line.starts_with("=end") {
                    state = State::Code;
                }
                out.push(mask);
                continue;
            }
        }
        let mut i = 0;
        while i < n {
            match state.clone() {
                State::LineBlock => i = n,
                State::Block(depth) => {
                    let start = i;
                    let mut d = depth;
                    let mut closed = false;
                    while i < n {
                        if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                            i += 2;
                            d -= 1;
                            if d == 0 {
                                closed = true;
                                break;
                            }
                            continue;
                        }
                        if lg.nested() && b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                            d += 1;
                            i += 2;
                            continue;
                        }
                        i += 1;
                    }
                    comment(&mut mask, start, i);
                    state = if closed { State::Code } else { State::Block(d) };
                }
                State::Raw {
                    term,
                    comment: is_comment,
                } => {
                    let start = i;
                    let closed = find(b, i, &term);
                    i = closed.map_or(n, |p| p + term.len());
                    if is_comment {
                        comment(&mut mask, start, i);
                    } else {
                        string(&mut mask, start, i);
                    }
                    if closed.is_some() {
                        state = State::Code;
                    }
                }
                State::Str(q) => {
                    let start = i;
                    let mut closed = false;
                    while i < n {
                        if b[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        if b[i] == q {
                            i += 1;
                            closed = true;
                            break;
                        }
                        i += 1;
                    }
                    string(&mut mask, start, i);
                    if closed {
                        state = State::Code;
                    }
                }
                State::Multi(q) => {
                    let start = i;
                    let mut interp = false;
                    while i < n {
                        if b[i] == b'\\' && lg.escapes(q) {
                            i += 2;
                            continue;
                        }
                        if q == b'`' {
                            if lg == Lang::Js && b[i] == b'$' && b.get(i + 1) == Some(&b'{') {
                                string(&mut mask, start, i + 2);
                                i += 2;
                                tmpl.push(0);
                                state = State::Code;
                                interp = true;
                                break;
                            }
                            if b[i] == b'`' {
                                i += 1;
                                state = State::Code;
                                break;
                            }
                        } else if b[i] == q && b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q)
                        {
                            i += 3;
                            state = State::Code;
                            break;
                        }
                        i += 1;
                    }
                    if !interp {
                        string(&mut mask, start, i);
                    }
                }
                State::Code => {
                    let c = b[i];
                    let next = b.get(i + 1).copied().unwrap_or(0);
                    let prev = if i > 0 { b[i - 1] } else { 0 };
                    if lg == Lang::Js && !tmpl.is_empty() && (c == b'{' || c == b'}') {
                        let top = tmpl.last_mut().expect("non-empty");
                        if c == b'{' {
                            *top += 1;
                        } else if *top == 0 {
                            tmpl.pop();
                            state = State::Multi(b'`');
                        } else {
                            *top -= 1;
                        }
                        i += 1;
                        continue;
                    }
                    if lg.slash_line() && c == b'/' && next == b'/' {
                        comment(&mut mask, i, n);
                        i = n;
                    } else if lg.block_c() && c == b'/' && next == b'*' {
                        state = State::Block(1);
                        comment(&mut mask, i, i + 2);
                        i += 2;
                    } else if lg == Lang::Vue && b[i..].starts_with(b"<!--") {
                        state = State::Raw {
                            term: b"-->".to_vec(),
                            comment: true,
                        };
                        comment(&mut mask, i, i + 4);
                        i += 4;
                    } else if lg.hash_line() && c == b'#' {
                        let is_comment = match lg {
                            Lang::Php => next != b'[',
                            Lang::Shell => {
                                i == 0
                                    || matches!(
                                        prev,
                                        b' ' | b'\t' | b';' | b'|' | b'&' | b'(' | b'`'
                                    )
                            }
                            Lang::Yaml => i == 0 || prev == b' ' || prev == b'\t',
                            _ => true,
                        };
                        if is_comment {
                            comment(&mut mask, i, n);
                            i = n;
                        } else {
                            i += 1;
                        }
                    } else if lg == Lang::Vb && c == b'\'' {
                        comment(&mut mask, i, n);
                        i = n;
                    } else if matches!(lg, Lang::Lua | Lang::Sql) && c == b'-' && next == b'-' {
                        if lg == Lang::Lua {
                            if let Some((level, len)) = lua_long_open(b, i + 2) {
                                let mut term = vec![b']'];
                                term.extend(std::iter::repeat_n(b'=', level));
                                term.push(b']');
                                state = State::Raw {
                                    term,
                                    comment: true,
                                };
                                comment(&mut mask, i, i + 2 + len);
                                i += 2 + len;
                                continue;
                            }
                        }
                        comment(&mut mask, i, n);
                        i = n;
                    } else if lg == Lang::Lua && c == b'[' && lua_long_open(b, i).is_some() {
                        let (level, len) = lua_long_open(b, i).expect("checked");
                        let mut term = vec![b']'];
                        term.extend(std::iter::repeat_n(b'=', level));
                        term.push(b']');
                        state = State::Raw {
                            term,
                            comment: false,
                        };
                        string(&mut mask, i, i + len);
                        i += len;
                    } else if lg == Lang::Rust
                        && matches!(c, b'r' | b'b' | b'c')
                        && !(i > 0 && is_ident(prev))
                        && rust_raw_open(b, i).is_some()
                    {
                        let (hashes, len) = rust_raw_open(b, i).expect("checked");
                        let mut term = vec![b'"'];
                        term.extend(std::iter::repeat_n(b'#', hashes));
                        state = State::Raw {
                            term,
                            comment: false,
                        };
                        string(&mut mask, i, i + len);
                        i += len;
                    } else if lg == Lang::C
                        && c == b'"'
                        && prev == b'R'
                        && cpp_raw_open(b, i).is_some()
                    {
                        let (term, len) = cpp_raw_open(b, i).expect("checked");
                        state = State::Raw {
                            term,
                            comment: false,
                        };
                        string(&mut mask, i, i + len);
                        i += len;
                    } else if (c == b'"' || c == b'\'')
                        && lg.triple(c)
                        && next == c
                        && b.get(i + 2) == Some(&c)
                    {
                        state = State::Multi(c);
                        string(&mut mask, i, i + 3);
                        i += 3;
                    } else if c == b'`' && lg.backtick_multi() {
                        state = State::Multi(b'`');
                        string(&mut mask, i, i + 1);
                        i += 1;
                    } else if lg == Lang::Shell && c == b'\\' {
                        i += 2;
                    } else if lg == Lang::Rust && c == b'\'' {
                        // `'a` lifetimes are code, `'x'` / `'\n'` / `b'\''` are chars.
                        let lit = if next == b'\\' {
                            b.get(i + 3..)
                                .and_then(|r| r.iter().position(|&x| x == b'\''))
                                .map(|p| p + 4)
                        } else {
                            let w = line[i + 1..].chars().next().map_or(0, char::len_utf8);
                            (w > 0 && b.get(i + 1 + w) == Some(&b'\'')).then_some(w + 2)
                        };
                        match lit {
                            Some(l) => {
                                string(&mut mask, i, i + l);
                                i += l;
                            }
                            None => i += 1,
                        }
                    } else if lg == Lang::C && c == b'\'' && number_before(b, i) {
                        // C++14 digit separator `1'000`.
                        i += 1;
                    } else if lg == Lang::Js && c == b'/' && regex_allowed(b, i) {
                        match regex_end(b, i) {
                            Some(end) => {
                                string(&mut mask, i, end);
                                i = end;
                            }
                            None => i += 1,
                        }
                    } else if lg == Lang::Yaml
                        && (c == b'"' || c == b'\'')
                        && !(i == 0
                            || matches!(
                                prev,
                                b' ' | b'\t' | b'[' | b'{' | b',' | b':' | b'-' | b'?'
                            ))
                    {
                        // `don't`: an apostrophe inside a plain scalar.
                        i += 1;
                    } else if c == b'"' || c == b'\'' || c == b'`' {
                        let start = i;
                        i += 1;
                        let mut closed = false;
                        while i < n {
                            if b[i] == b'\\' && lg.escapes(c) {
                                i += 2;
                                continue;
                            }
                            if b[i] == c {
                                i += 1;
                                closed = true;
                                break;
                            }
                            i += 1;
                        }
                        string(&mut mask, start, i);
                        // Rust strings may continue on the next line.
                        if !closed && lg == Lang::Rust && c == b'"' {
                            state = State::Str(b'"');
                        }
                    } else {
                        i += 1;
                    }
                }
            }
        }
        out.push(mask);
    }
    Some(out)
}

/// Rust `r"`, `r#"`, `br#"`, `cr#"`: (hash count, opener length).
fn rust_raw_open(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let mut j = i;
    if matches!(b[j], b'b' | b'c') {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let mut h = 0;
    while b.get(j) == Some(&b'#') {
        h += 1;
        j += 1;
    }
    (b.get(j) == Some(&b'"')).then_some((h, j + 1 - i))
}

/// C++ `R"delim(`: terminator `)delim"` and opener length (from the quote).
fn cpp_raw_open(b: &[u8], i: usize) -> Option<(Vec<u8>, usize)> {
    let mut j = i + 1;
    while j < b.len() && j - i <= 17 && b[j] != b'(' {
        if matches!(b[j], b' ' | b')' | b'\\' | b'"') {
            return None;
        }
        j += 1;
    }
    if b.get(j) != Some(&b'(') {
        return None;
    }
    let mut term = vec![b')'];
    term.extend_from_slice(&b[i + 1..j]);
    term.push(b'"');
    Some((term, j + 1 - i))
}

/// Is the token ending right before `i` a numeric literal (`1`, `0x1F`)?
fn number_before(b: &[u8], i: usize) -> bool {
    let mut s = i;
    while s > 0 && (b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'\'') {
        s -= 1;
    }
    s < i && b[s].is_ascii_digit()
}

/// End (exclusive) of a regex literal starting at `i`, if closed on the line.
fn regex_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut class = false;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 1,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => {
                j += 1;
                while j < b.len() && b[j].is_ascii_alphabetic() {
                    j += 1;
                }
                return (j > i + 2).then_some(j);
            }
            _ => {}
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ls(src: &str) -> Vec<String> {
        src.lines().map(str::to_string).collect()
    }

    /// Whether `needle` on line `l` sits on code bytes.
    fn is_code(path: &str, src: &str, l: usize, needle: &str, strings_code: bool) -> bool {
        let lines = ls(src);
        let m = code_mask(path, &lines, strings_code).unwrap();
        let at = lines[l].find(needle).unwrap();
        m[l][at]
    }

    #[test]
    fn go_line_block_and_strings() {
        let src = "SysLog(1)\n// SysLog(2)\nx := \"SysLog(3)\" + SysLog(4)\n/* a\nSysLog(5)\n*/ SysLog(6)\nf() // SysLog(7)\n`raw\nSysLog(8)`\nSysLog(9)";
        assert!(is_code("a.go", src, 0, "SysLog", false));
        assert!(!is_code("a.go", src, 1, "SysLog", false));
        assert!(!is_code("a.go", src, 2, "SysLog", false));
        assert!(is_code("a.go", src, 2, "SysLog(4", false));
        assert!(!is_code("a.go", src, 4, "SysLog", false));
        assert!(is_code("a.go", src, 5, "SysLog", false));
        assert!(!is_code("a.go", src, 6, "SysLog", false));
        assert!(!is_code("a.go", src, 8, "SysLog", false));
        assert!(is_code("a.go", src, 9, "SysLog", false));
        // Strings count as code when the query itself is a string.
        assert!(is_code("a.go", src, 2, "SysLog", true));
    }

    #[test]
    fn rust_nested_blocks_and_lifetimes() {
        let src = "/* a /* b */ still */ run()\nfn f<'a>(x: &'a str) { run() } // run()\nlet c = 'x'; run()";
        assert!(!is_code("a.rs", src, 0, "a /*", false));
        assert!(is_code("a.rs", src, 0, "run", false));
        assert!(is_code("a.rs", src, 1, "run", false));
        assert!(is_code("a.rs", src, 2, "run", false));
    }

    #[test]
    fn python_hash_and_triple_quotes() {
        let src = "x = 1  # call()\n\"\"\"\ncall()\n\"\"\"\ncall()\ns = 'call()'";
        assert!(!is_code("a.py", src, 0, "call", false));
        assert!(!is_code("a.py", src, 2, "call", false));
        assert!(is_code("a.py", src, 4, "call", false));
        assert!(!is_code("a.py", src, 5, "call", false));
    }

    // ---- audit parse-late A1..A5 / search-text #4..#6 regressions ----

    #[test]
    fn shell_hash_is_comment_only_at_word_start() {
        assert!(is_code("a.sh", "n=$#; foo_call", 0, "foo_call", false));
        assert!(is_code(
            "a.sh",
            "echo ${#arr[@]} foo_call",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.sh", "x=1 # foo_call", 0, "foo_call", false));
        assert!(!is_code("a.sh", "# foo_call", 0, "foo_call", false));
        // shell single quotes have no escapes
        assert!(is_code(
            "a.sh",
            "echo 'a\\' ; foo_call",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn php_attribute_is_not_a_comment() {
        assert!(is_code(
            "a.php",
            "#[Route('/x')] foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.php", "# foo_call();", 0, "foo_call", false));
        assert!(!is_code("a.php", "// foo_call();", 0, "foo_call", false));
    }

    #[test]
    fn lua_long_comments_and_strings() {
        let src = "--[[ start\nfoo_call()\n]] bar()";
        assert!(!is_code("a.lua", src, 1, "foo_call", false));
        assert!(is_code("a.lua", src, 2, "bar", false));
        let src = "--[==[ a\n]] foo_call()\n]==] real()\nx = [[\nfoo_call()\n]] foo_call()";
        assert!(!is_code("a.lua", src, 1, "foo_call", false));
        assert!(is_code("a.lua", src, 2, "real", false));
        assert!(!is_code("a.lua", src, 4, "foo_call", false));
        assert!(is_code("a.lua", src, 5, "foo_call", false));
        assert!(!is_code("a.lua", "x() -- foo_call", 0, "foo_call", false));
    }

    #[test]
    fn ruby_begin_end_block() {
        let src = "=begin\nfoo_call()\n=end\nfoo_call()";
        assert!(!is_code("a.rb", src, 1, "foo_call", false));
        assert!(is_code("a.rb", src, 3, "foo_call", false));
    }

    #[test]
    fn sql_block_comment_and_yaml_comment() {
        assert!(!is_code(
            "a.sql",
            "/* a\nfoo_call()\n*/",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.sql",
            "select 1; -- foo_call",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.sql",
            "select foo_call(1);",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.yaml",
            "k: don't # c\nfoo_call: 1",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.yaml",
            "k: don't # foo_call",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.toml", "a = 1 # foo_call", 0, "foo_call", false));
    }

    #[test]
    fn kotlin_swift_scala_nested_block_comments() {
        for p in ["a.kt", "a.swift", "a.scala", "a.dart"] {
            let src = "/* a /* b */ c\nfoo_call()\n*/ d foo_call()";
            assert!(!is_code(p, src, 1, "foo_call", false), "{p}");
            assert!(is_code(p, src, 2, "foo_call", false), "{p}");
        }
        // Java/C do not nest.
        assert!(is_code(
            "a.java",
            "/* a /* b */ foo_call()",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn rust_multiline_and_raw_strings() {
        assert!(!is_code(
            "a.rs",
            "let s = \"abc\nfoo_call()\"; real()",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.rs",
            "let s = \"abc\nfoo_call()\"; real()",
            1,
            "real",
            false
        ));
        let raw = "let s = r#\"abc\nfoo_call()\n\"#; real()";
        assert!(!is_code("a.rs", raw, 1, "foo_call", false));
        assert!(is_code("a.rs", raw, 2, "real", false));
        // `r"C:\dir\"` : backslash is not an escape in raw strings.
        assert!(is_code(
            "a.rs",
            "let p = r\"C:\\dir\\\"; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.rs",
            "let q = br#\"x\"#; foo_call();",
            0,
            "foo_call",
            false
        ));
        // identifiers ending in r are not raw-string openers
        assert!(is_code(
            "a.rs",
            "for x in y { foo_call(\"a\") }",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.rs",
            "let c = b'\\''; foo_call();",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn text_blocks_triple_quotes() {
        assert!(!is_code(
            "A.java",
            "String s = \"\"\"\n foo_call()\n \"\"\"; foo_call()",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "A.java",
            "String s = \"\"\"\n foo_call()\n \"\"\"; foo_call()",
            2,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.kt",
            "val s = \"\"\"\n foo_call()\n\"\"\"",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.swift",
            "let s = \"\"\"\n foo_call()\n\"\"\"",
            1,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.cs",
            "var s = \"\"\"\n foo_call()\n\"\"\";",
            1,
            "foo_call",
            false
        ));
    }

    #[test]
    fn cpp_digit_separator_and_raw_string() {
        assert!(is_code(
            "a.cpp",
            "int n = 1'000; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.cpp",
            "int n = 0x1F'00; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.cpp",
            "char c = 'a'; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.cpp",
            "auto s = R\"x(a\nfoo_call()\n)x\"; real();",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.cpp",
            "auto s = R\"x(a\nfoo_call()\n)x\"; real();",
            2,
            "real",
            false
        ));
    }

    #[test]
    fn js_template_interpolation_and_regex() {
        assert!(is_code(
            "a.js",
            "const s = `a ${foo_call()} b`;",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code(
            "a.js",
            "const s = `foo_call ${x} b`;",
            0,
            "foo_call",
            false
        ));
        let src = "const y = `line1\n ${run_f({a: 1})} tail foo_call\n`;\nrun_g();";
        assert!(is_code("a.js", src, 1, "run_f", false));
        assert!(!is_code("a.js", src, 1, "foo_call", false));
        assert!(is_code("a.js", src, 3, "run_g", false));
        assert!(is_code(
            "a.js",
            "const r = /\"/; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.ts",
            "const r = /[/']/g; foo_call();",
            0,
            "foo_call",
            false
        ));
        // division is not a regex
        assert!(is_code(
            "a.js",
            "x = a / 2; y = \"b\"; foo_call();",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.js", "x = /foo_call/;", 0, "foo_call", false));
    }

    #[test]
    fn vue_html_comment() {
        assert!(!is_code(
            "a.vue",
            "<!--\nfoo_call()\n-->",
            1,
            "foo_call",
            false
        ));
        assert!(is_code(
            "a.vue",
            "<!-- c -->foo_call()",
            0,
            "foo_call",
            false
        ));
    }

    #[test]
    fn go_rune_and_vb_comment_and_crlf() {
        assert!(is_code(
            "a.go",
            "c := '\"'; foo_call()",
            0,
            "foo_call",
            false
        ));
        assert!(!is_code("a.vb", "x = 1 ' foo_call()", 0, "foo_call", false));
        assert!(is_code("a.vb", "x = foo_call(\"a\")", 0, "foo_call", false));
        let lines = vec!["foo_call() // c\r".to_string()];
        let m = code_mask("a.go", &lines, false).unwrap();
        assert!(!m[0][lines[0].len() - 1]);
    }

    #[test]
    fn lexer_coverage_is_explicit() {
        for e in [
            "mts",
            "hxx",
            "inl",
            "cu",
            "ipp",
            "rake",
            "gemspec",
            "bats",
            "ksh",
            "lua",
            "pyi",
            "zsh-theme",
            "vb",
            "csx",
            "kts",
            "sql",
            "yaml",
            "toml",
        ] {
            assert!(has_lexer(&format!("a.{e}")), ".{e}");
        }
        for e in ["hs", "pl", "cshtml", "razor", "txt", "md"] {
            assert!(!has_lexer(&format!("a.{e}")), ".{e}");
        }
        assert_eq!(type_label("x/a.HS"), ".hs");
    }

    #[test]
    fn unknown_language_has_no_mask() {
        assert!(code_mask("a.zzz", &ls("x"), false).is_none());
    }
}
