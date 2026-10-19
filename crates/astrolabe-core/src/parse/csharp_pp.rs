//! C# preprocessor pre-pass for the error retry.
//!
//! tree-sitter-c-sharp parses `#if` / `#elif` / `#else` / `#endif` as syntax
//! and only accepts them in some positions. A directive that splits a
//! declaration (`class X\n#if A\n : Base\n#endif\n{`), a type parameter list
//! or a switch section yields ERROR nodes, drops type declarations and
//! truncates symbol ranges. Roslyn treats inactive branches as disabled text.
//! Without the project's `DefineConstants` we keep the first branch of every
//! conditional (`#if false` keeps the next one instead) and blank the others.
//!
//! .NET 10 file-based apps start with `#:package` / `#:property` lines the
//! grammar does not know; those lines are blanked too.
//!
//! Blanking replaces every byte of a line except `\r` / `\n` with a space, so
//! byte offsets and line numbers of the remaining code are unchanged and the
//! result is valid UTF-8 (whole lines are replaced).

/// Kind of a preprocessor line relevant to the pre-pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Directive<'a> {
    If(&'a str),
    Elif(&'a str),
    Else,
    Endif,
    /// `#:` file-level directive (.NET 10 file-based apps).
    FileLevel,
}

fn directive(line: &str, vb: bool) -> Option<Directive<'_>> {
    let rest = line.trim_start_matches([' ', '\t']).strip_prefix('#')?;
    if !vb && rest.starts_with(':') {
        return Some(Directive::FileLevel);
    }
    let rest = rest.trim_start_matches([' ', '\t']);
    let word_end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    let (word, tail) = rest.split_at(word_end);
    if vb {
        // VB keywords are case-insensitive: `#If`, `#ElseIf`, `#Else`, `#End If`.
        let word = word.to_ascii_lowercase();
        return match word.as_str() {
            "if" => Some(Directive::If(tail)),
            "elseif" => Some(Directive::Elif(tail)),
            "else" => Some(Directive::Else),
            "end"
                if tail
                    .trim_start()
                    .get(..2)
                    .is_some_and(|w| w.eq_ignore_ascii_case("if")) =>
            {
                Some(Directive::Endif)
            }
            _ => None,
        };
    }
    match word {
        "if" => Some(Directive::If(tail)),
        "elif" => Some(Directive::Elif(tail)),
        "else" => Some(Directive::Else),
        "endif" => Some(Directive::Endif),
        _ => None,
    }
}

/// Whether the branch guarded by `expr` is taken. Only a literal `false` (or
/// `!true`; VB `False` / `Not True`) is known to be inactive; every other
/// condition is assumed true.
fn branch_taken(expr: &str, vb: bool) -> bool {
    let cut = if vb { '\'' } else { '/' };
    let mut expr = expr.split(cut).next().unwrap_or("").trim().to_owned();
    if vb {
        expr = expr.to_ascii_lowercase();
        if let Some(e) = expr.strip_suffix("then") {
            expr = e.trim().to_owned();
        }
    }
    let expr = expr.trim_start_matches('(').trim_end_matches(')').trim();
    let compact = expr.replace(' ', "");
    !(expr == "false" || compact == "!true" || (vb && compact == "nottrue"))
}

/// One open `#if` group.
struct Group {
    /// The group is inside an active region.
    parent_active: bool,
    /// Some branch of this group was already taken.
    taken: bool,
}

/// Returns the source with directive lines and inactive conditional branches
/// blanked (same byte length), or `None` when the source has no `#if` /
/// `#:` directive at all.
pub(super) fn blank_inactive(source: &str) -> Option<String> {
    blank_inactive_for(source, false)
}

/// [`blank_inactive`] for Visual Basic (`#If … Then` / `#ElseIf` / `#Else` /
/// `#End If`; no `#:` lines).
pub(super) fn blank_inactive_vb(source: &str) -> Option<String> {
    blank_inactive_for(source, true)
}

fn blank_inactive_for(source: &str, vb: bool) -> Option<String> {
    if !source.contains('#') {
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    let mut changed = false;
    let mut stack: Vec<Group> = Vec::new();
    let mut active = true;
    let mut pos = 0usize;
    for line in source.split_inclusive('\n') {
        let blank = match directive(line, vb) {
            Some(Directive::FileLevel) => true,
            Some(Directive::If(expr)) => {
                let taken = branch_taken(expr, vb);
                stack.push(Group {
                    parent_active: active,
                    taken,
                });
                active = active && taken;
                true
            }
            Some(d @ (Directive::Elif(_) | Directive::Else)) => {
                if let Some(group) = stack.last_mut() {
                    let cur = match d {
                        Directive::Elif(expr) => !group.taken && branch_taken(expr, vb),
                        _ => !group.taken,
                    };
                    group.taken |= cur;
                    active = group.parent_active && cur;
                }
                true
            }
            Some(Directive::Endif) => {
                if let Some(group) = stack.pop() {
                    active = group.parent_active;
                }
                true
            }
            None => !active,
        };
        if blank {
            if !changed {
                out.reserve(source.len());
                out.extend_from_slice(&source.as_bytes()[..pos]);
                changed = true;
            }
            out.extend(
                line.bytes()
                    .map(|b| if b == b'\n' || b == b'\r' { b } else { b' ' }),
            );
        } else if changed {
            out.extend_from_slice(line.as_bytes());
        }
        pos += line.len();
    }
    if !changed {
        return None;
    }
    // Whole lines were replaced by ASCII, so the bytes stay valid UTF-8.
    String::from_utf8(out).ok()
}

/// Imports of the original (partial) parse in source order, then those only
/// the retry found. The blanked branches' `using`s are real dependencies of
/// some build configuration, so dropping them would lose import edges.
pub(super) fn union_imports(original: Vec<String>, retry: Vec<String>) -> Vec<String> {
    let mut out = original;
    let known: std::collections::HashSet<String> = out.iter().cloned().collect();
    out.extend(retry.into_iter().filter(|i| !known.contains(i)));
    out
}

#[cfg(test)]
mod tests {
    use super::{blank_inactive, union_imports};

    #[test]
    fn union_keeps_original_order_and_adds_retry_only() {
        let got = union_imports(vec!["A".into(), "B".into()], vec!["B".into(), "C".into()]);
        assert_eq!(got, ["A", "B", "C"]);
    }

    #[test]
    fn keeps_first_branch_and_preserves_offsets() {
        let src = "class X\n#if A\n : Base\n#elif B\n : Other\n#else\n : Third\n#endif\n{ }\n";
        let out = blank_inactive(src).expect("changed");
        assert_eq!(out.len(), src.len());
        assert_eq!(out.lines().count(), src.lines().count());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "class X");
        assert_eq!(lines[1].trim(), "");
        assert_eq!(lines[2], " : Base");
        assert_eq!(lines[4].trim(), "");
        assert_eq!(lines[6].trim(), "");
        assert_eq!(lines[8], "{ }");
    }

    #[test]
    fn if_false_takes_next_branch_and_nesting_is_respected() {
        let src = "#if false\nA\n#elif X\nB\n#if Y\nC\n#else\nD\n#endif\n#else\nE\n#endif\n";
        let out = blank_inactive(src).unwrap();
        let kept: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(kept, ["B", "C"]);
    }

    #[test]
    fn file_level_directives_and_crlf_and_multibyte() {
        let src = "#:package Foo@1.0\r\n#:property X=é\r\nclass C {}\r\n";
        let out = blank_inactive(src).unwrap();
        assert_eq!(out.len(), src.len());
        assert!(out.ends_with("class C {}\r\n"));
        assert_eq!(out.matches("\r\n").count(), 3);
    }

    #[test]
    fn vb_directives_case_insensitive_and_false() {
        let src = "#If False Then\nA\n#ElseIf X Then ' c\nB\n#Else\nC\n#End If\n#Region \"R\"\n#End Region\n";
        let out = super::blank_inactive_vb(src).unwrap();
        assert_eq!(out.len(), src.len());
        let kept: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(kept, ["B", "#Region \"R\"", "#End Region"]);
    }

    #[test]
    fn no_directive_is_none() {
        assert!(blank_inactive("class C { string s = \"#\"; }\n").is_none());
        assert!(blank_inactive("#region R\nclass C {}\n#endregion\n").is_none());
    }

    mod retry {
        use crate::parse::{ParsedFile, ParserPool};
        use crate::types::{Language, RelPath};

        fn parse(path: &str, src: &str) -> ParsedFile {
            ParserPool::new()
                .parse(Language::CSharp, &RelPath::new(path), src)
                .expect("parse")
        }

        fn has(p: &ParsedFile, kind: &str, name: &str) -> Option<(u32, u32)> {
            p.symbols
                .iter()
                .find(|s| format!("{:?}", s.kind) == kind && s.name == name)
                .map(|s| (s.start_line, s.end_line))
        }

        #[test]
        fn if_splitting_base_list_recovers_class_and_range() {
            let src = "public class X\n#if A\n  : Base\n#endif\n{\n  void M() {}\n}\n";
            let p = parse("X.cs", src);
            assert!(!p.partial, "retry must parse clean");
            assert_eq!(has(&p, "Class", "X"), Some((1, 7)));
            assert!(has(&p, "Method", "M").is_some());
            assert!(!p.symbols.iter().any(|s| s.name == "if"));
        }

        #[test]
        fn if_inside_type_parameters_and_switch_arms() {
            let p = parse(
                "I.cs",
                "public interface IJ<\n#if V\n  out\n#endif\n  T> { }\n",
            );
            assert!(!p.partial);
            assert_eq!(has(&p, "Interface", "IJ"), Some((1, 5)));
            let p = parse(
                "C.cs",
                "class C { int F(int t) => t switch {\n  1 => 2,\n#if !N\n  3 => 4,\n#endif\n  _ => 0 }; }\n",
            );
            assert!(!p.partial);
            assert!(has(&p, "Method", "F").is_some());
        }

        #[test]
        fn dotnet10_file_directives_in_cs_and_csx() {
            let src =
                "#:package Foo@1.0\n#:property LangVersion=preview\nclass C { void Run() {} }\n";
            for path in ["app.cs", "script.csx"] {
                let p = parse(path, src);
                assert!(!p.partial, "{path}");
                assert_eq!(has(&p, "Class", "C"), Some((3, 3)), "{path}");
            }
        }

        #[test]
        fn blanked_branch_usings_stay_imports() {
            let src = "#if NET20\nusing Old.Linq;\n#else\nusing System.Linq;\n#endif\npublic class X\n#if A\n  : Base\n#endif\n{ }\n";
            let p = parse("U.cs", src);
            assert!(!p.partial);
            assert!(
                p.imports.iter().any(|i| i.contains("System.Linq")),
                "{:?}",
                p.imports
            );
            assert!(
                p.imports.iter().any(|i| i.contains("Old.Linq")),
                "{:?}",
                p.imports
            );
        }

        #[test]
        fn vb_if_directives_in_class_body() {
            // tree-sitter-vb has no `#If` support at all: any directive is an
            // ERROR. (Blank lines between `Imports` are a separate grammar bug,
            // so the fixture keeps directives inside the class.)
            let src = "Imports System.Linq\nPublic Class X\n#If DEBUG Then\n    Sub M()\n    End Sub\n#ElseIf B Then\n    Sub Q()\n    End Sub\n#End If\n    Sub N()\n    End Sub\nEnd Class\n";
            let p = ParserPool::new()
                .parse(Language::VisualBasic, &RelPath::new("X.vb"), src)
                .expect("parse");
            assert!(!p.partial, "VB retry must parse clean");
            assert!(has(&p, "Class", "X").is_some());
            assert_eq!(has(&p, "Method", "M").map(|r| r.0), Some(4));
            assert!(has(&p, "Method", "N").is_some());
            assert!(has(&p, "Method", "Q").is_none(), "inactive branch");
        }

        #[test]
        fn unrelated_error_keeps_first_pass() {
            // The retry is still partial (real syntax error), so the original
            // parse is returned unchanged.
            let src = "public class X\n#if A\n  : Base\n#endif\n{\n  void M() { int = ; }\n}\n";
            let p = parse("E.cs", src);
            assert!(p.partial);
        }

        #[test]
        fn clean_file_with_directives_is_untouched() {
            let src = "class C {\n#if DEBUG\n  void D() {}\n#else\n  void R() {}\n#endif\n}\n";
            let p = parse("Clean.cs", src);
            assert!(!p.partial);
            // First pass was clean: both branches stay visible.
            assert!(has(&p, "Method", "D").is_some());
            assert!(has(&p, "Method", "R").is_some());
        }
    }
}
