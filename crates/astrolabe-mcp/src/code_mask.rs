//! Line lexer for `search_code skip_comments`: marks which bytes of each line
//! are real code, i.e. not inside a comment (line or block) nor, optionally,
//! a string literal. Heuristic, per file, no parsing: good enough to separate
//! `SysLog(` calls from `// SysLog(` notes and `"SysLog("` strings.

#[derive(Clone, Copy, PartialEq)]
enum Family {
    /// `//` and `/* */` (C, C++, Go, Rust, Java, JS/TS, Dart, Kotlin, C#, ...).
    Slash,
    /// `#` and Python triple-quoted strings (Python, Ruby, shell).
    Hash,
    /// `--` (Lua, SQL).
    Dash,
}

fn family(path: &str) -> Option<(Family, bool)> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    let rust = ext == "rs";
    Some(match ext.as_str() {
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" | "mm" | "go" | "rs" | "java"
        | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "dart" | "kt" | "kts" | "swift" | "cs"
        | "scala" | "php" | "vue" => (Family::Slash, rust),
        "py" | "rb" | "sh" | "bash" | "zsh" | "yaml" | "yml" | "toml" => (Family::Hash, false),
        "lua" | "sql" => (Family::Dash, false),
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Code,
    Block(u32),
    /// Multi-line string: Python `"""`/`'''` (the quote byte), Go/JS backtick.
    Multi(u8),
}

/// Per line, per byte: `true` when the byte is code. `None` for languages
/// without a lexer. With `strings_are_code`, only comments are masked.
pub(crate) fn code_mask(
    path: &str,
    lines: &[String],
    strings_are_code: bool,
) -> Option<Vec<Vec<bool>>> {
    let (fam, rust) = family(path)?;
    let backtick = fam == Family::Slash && !path.ends_with(".dart") && !rust;
    let mut state = State::Code;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let b = line.as_bytes();
        let n = b.len();
        let mut mask = vec![true; n];
        let mut i = 0;
        let mark = |mask: &mut Vec<bool>, from: usize, to: usize, strings: bool| {
            if !strings || !strings_are_code {
                for m in &mut mask[from..to.min(n)] {
                    *m = false;
                }
            }
        };
        while i < n {
            match state {
                State::Block(depth) => {
                    let start = i;
                    let mut closed = false;
                    while i < n {
                        if b[i] == b'*' && i + 1 < n && b[i + 1] == b'/' {
                            i += 2;
                            closed = true;
                            break;
                        }
                        if rust && b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                            state = State::Block(match state {
                                State::Block(d) => d + 1,
                                _ => 1,
                            });
                            i += 2;
                            continue;
                        }
                        i += 1;
                    }
                    mark(&mut mask, start, i, false);
                    if closed {
                        let d = match state {
                            State::Block(d) => d,
                            _ => depth,
                        };
                        state = if d > 1 {
                            State::Block(d - 1)
                        } else {
                            State::Code
                        };
                    }
                }
                State::Multi(q) => {
                    let start = i;
                    let mut closed = false;
                    while i < n {
                        if b[i] == b'\\' && q != b'`' {
                            i += 2;
                            continue;
                        }
                        if q == b'`' {
                            if b[i] == b'`' {
                                i += 1;
                                closed = true;
                                break;
                            }
                        } else if b[i] == q && i + 2 < n && b[i + 1] == q && b[i + 2] == q {
                            i += 3;
                            closed = true;
                            break;
                        }
                        i += 1;
                    }
                    mark(&mut mask, start, i.min(n), true);
                    if closed {
                        state = State::Code;
                    }
                }
                State::Code => {
                    let c = b[i];
                    let next = b.get(i + 1).copied().unwrap_or(0);
                    if fam == Family::Slash && c == b'/' && next == b'/' {
                        mark(&mut mask, i, n, false);
                        i = n;
                    } else if fam == Family::Slash && c == b'/' && next == b'*' {
                        state = State::Block(1);
                        mark(&mut mask, i, i + 2, false);
                        i += 2;
                    } else if (fam == Family::Hash && c == b'#')
                        || (fam == Family::Dash && c == b'-' && next == b'-')
                        || (path.ends_with(".php") && c == b'#')
                    {
                        mark(&mut mask, i, n, false);
                        i = n;
                    } else if fam == Family::Hash
                        && (c == b'"' || c == b'\'')
                        && next == c
                        && b.get(i + 2) == Some(&c)
                        && path.ends_with(".py")
                    {
                        state = State::Multi(c);
                        mark(&mut mask, i, i + 3, true);
                        i += 3;
                    } else if c == b'`' && backtick {
                        state = State::Multi(b'`');
                        mark(&mut mask, i, i + 1, true);
                        i += 1;
                    } else if c == b'"' || c == b'\'' {
                        // Rust: `'a` lifetimes are code, `'x'` / `'\n'` are chars.
                        if rust && c == b'\'' {
                            let lit = if next == b'\\' {
                                b[i + 2..].iter().position(|&x| x == b'\'').map(|p| p + 3)
                            } else {
                                let w = line[i + 1..].chars().next().map_or(0, char::len_utf8);
                                (w > 0 && b.get(i + 1 + w) == Some(&b'\'')).then_some(w + 2)
                            };
                            match lit {
                                Some(l) => {
                                    mark(&mut mask, i, i + l, true);
                                    i += l;
                                }
                                None => i += 1,
                            }
                            continue;
                        }
                        let start = i;
                        i += 1;
                        while i < n {
                            if b[i] == b'\\' {
                                i += 2;
                                continue;
                            }
                            if b[i] == c {
                                i += 1;
                                break;
                            }
                            i += 1;
                        }
                        mark(&mut mask, start, i.min(n), true);
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

    #[test]
    fn unknown_language_has_no_mask() {
        assert!(code_mask("a.zzz", &ls("x"), false).is_none());
    }
}
