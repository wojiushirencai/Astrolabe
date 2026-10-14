//! explain_symbol caller rendering: one entry per call-site line, non-test
//! callers first, receiver/type-qualified names, method vs function split.

use astrolabe_core::types::{CodeSymbol, SymbolKind};

/// Test code by path or by caller name (Go `TestX`, Python `test_x`, JS specs).
pub(crate) fn is_test_caller(path: &str, caller: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let file = p.rsplit('/').next().unwrap_or(&p);
    file.contains("_test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.starts_with("test_")
        || p.starts_with("test/")
        || p.starts_with("tests/")
        || p.contains("/test/")
        || p.contains("/tests/")
        || p.contains("__tests__/")
        || caller.starts_with("Test")
        || caller.starts_with("test_")
        || caller.starts_with("Benchmark")
}

/// Receiver / container qualifier for display: Go `func (c *Command) X` →
/// `(*Command).X`; otherwise the smallest enclosing type in the same file
/// (`Type.name`); otherwise the bare name.
pub(crate) fn qualified_name(sym: &CodeSymbol, same_file: &[&CodeSymbol]) -> String {
    let sig = sym.signature.trim_start();
    if let Some(rest) = sig.strip_prefix("func") {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_prefix('(') {
            if let Some(close) = inner.find(')') {
                let recv = inner[..close].trim();
                let ty = recv.rsplit(' ').next().unwrap_or(recv).trim();
                let ty = ty.split('[').next().unwrap_or(ty);
                if !ty.is_empty() {
                    let ty = if ty.starts_with('*') {
                        format!("({ty})")
                    } else {
                        ty.to_string()
                    };
                    return format!("{ty}.{}", sym.name);
                }
            }
        }
    }
    let container = same_file
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Interface
                    | SymbolKind::Trait
                    | SymbolKind::Enum
                    | SymbolKind::Module
                    | SymbolKind::Type
            ) && s.id != sym.id
                && s.start_line <= sym.start_line
                && s.end_line >= sym.end_line
        })
        .min_by_key(|s| s.end_line - s.start_line);
    match container {
        Some(c) => format!("{}.{}", c.name, sym.name),
        None => sym.name.clone(),
    }
}

/// Whether `def` is a method (Go receiver, or nested in a type).
pub(crate) fn is_method(def: &CodeSymbol, same_file: &[&CodeSymbol]) -> bool {
    def.kind == SymbolKind::Method || qualified_name(def, same_file) != def.name
}

/// How a call site spells the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallForm {
    /// `name(`
    Bare,
    /// `x.name(`
    Dotted,
    /// `x::name(` / `x->name(`
    Path,
}

/// 1-based line numbers in `[start, end]` where `name` is called, with the
/// call form. Skips the declaration line of `skip_decl` (the symbol itself)
/// and comment-only lines.
pub(crate) fn call_sites(
    lines: &[String],
    start: usize,
    end: usize,
    name: &str,
    skip_decl: Option<usize>,
) -> Vec<(usize, CallForm)> {
    let mut out = Vec::new();
    if name.is_empty() {
        return out;
    }
    for n in start.max(1)..=end.min(lines.len()) {
        if Some(n) == skip_decl {
            continue;
        }
        let line = &lines[n - 1];
        let trimmed = line.trim_start();
        if trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }
        let bytes = line.as_bytes();
        let mut from = 0;
        while let Some(pos) = line[from..].find(name) {
            let at = from + pos;
            from = at + name.len();
            let before_ok = at == 0 || !is_ident(bytes[at - 1]);
            let after = line[at + name.len()..].trim_start();
            let after_ok = after.starts_with('(')
                || after.starts_with("!(")
                || (after.starts_with('[') && after.contains("]("))
                || (after.starts_with("::<"))
                || (after.starts_with('<') && after.contains(">("));
            if !before_ok || !after_ok {
                continue;
            }
            let prefix = line[..at].trim_end();
            let form = if prefix.ends_with('.') || prefix.ends_with("?.") {
                CallForm::Dotted
            } else if prefix.ends_with("::") || prefix.ends_with("->") {
                CallForm::Path
            } else {
                // `func Name(` / `fn name(` / `def name(` are declarations.
                let word = prefix
                    .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                if matches!(word, "func" | "fn" | "def" | "function")
                    || prefix.ends_with(')') && prefix.trim_start().starts_with("func")
                {
                    continue;
                }
                CallForm::Bare
            };
            out.push((n, form));
            break;
        }
    }
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Whether a call of `form` at `caller_path` can bind to `def` at `def_path`.
/// Methods are called dotted or path-qualified (`recv.m(`, `Type::m(`), or bare
/// inside their own type (Python/Java/C++ implicit `this`). Functions are
/// called bare, path-qualified, or dotted from another directory (Go
/// `pkg.Func(`, JS `mod.fn(`); a dotted call in the same directory is a
/// method call on something else.
pub(crate) fn form_matches(form: CallForm, def_is_method: bool, same_dir: bool, go: bool) -> bool {
    match (def_is_method, form) {
        (true, CallForm::Dotted | CallForm::Path) => true,
        (true, CallForm::Bare) => !go,
        (false, CallForm::Bare | CallForm::Path) => true,
        (false, CallForm::Dotted) => !same_dir || !go,
    }
}

pub(crate) fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::types::{FileId, SymbolId};

    fn sym(id: u32, name: &str, kind: SymbolKind, sig: &str, s: u32, e: u32) -> CodeSymbol {
        CodeSymbol {
            id: SymbolId(id),
            file: FileId(0),
            name: name.into(),
            kind,
            signature: sig.into(),
            start_line: s,
            end_line: e,
            exported: true,
        }
    }

    #[test]
    fn test_paths_and_names() {
        assert!(is_test_caller("service/channel_test.go", "TestX"));
        assert!(is_test_caller("a/b.go", "TestShouldDisable"));
        assert!(is_test_caller("tests/vectors.rs", "frame"));
        assert!(is_test_caller("src/__tests__/a.ts", "x"));
        assert!(!is_test_caller("controller/relay.go", "Relay"));
        assert!(!is_test_caller("contest/latest.go", "Run"));
    }

    #[test]
    fn go_receiver_and_container_qualify() {
        let m = sym(
            1,
            "MarkFlagRequired",
            SymbolKind::Function,
            "func (c *Command) MarkFlagRequired(name string) error",
            24,
            26,
        );
        let f = sym(
            2,
            "MarkFlagRequired",
            SymbolKind::Function,
            "func MarkFlagRequired(flags *pflag.FlagSet, name string) error",
            38,
            40,
        );
        assert_eq!(qualified_name(&m, &[]), "(*Command).MarkFlagRequired");
        assert_eq!(qualified_name(&f, &[]), "MarkFlagRequired");
        assert!(is_method(&m, &[]));
        assert!(!is_method(&f, &[]));
        let class = sym(3, "Store", SymbolKind::Class, "class Store", 1, 50);
        let meth = sym(4, "get", SymbolKind::Method, "def get(self)", 10, 20);
        assert_eq!(qualified_name(&meth, &[&class, &meth]), "Store.get");
    }

    #[test]
    fn call_sites_report_each_line_and_form() {
        let src: Vec<String> = [
            "func Relay(c *gin.Context) {",
            "  // processChannelError(c) in a comment",
            "  processChannelError(c, a, b)",
            "  x := 1",
            "  processChannelError(c,",
            "  service.ShouldDisableChannel(err, t)",
            "  c.MarkFlagRequired(n)",
            "  myprocessChannelError(c)",
            "}",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let sites = call_sites(&src, 1, 9, "processChannelError", None);
        assert_eq!(sites, vec![(3, CallForm::Bare), (5, CallForm::Bare)]);
        assert_eq!(
            call_sites(&src, 1, 9, "ShouldDisableChannel", None),
            vec![(6, CallForm::Dotted)]
        );
        assert_eq!(
            call_sites(&src, 1, 9, "MarkFlagRequired", None),
            vec![(7, CallForm::Dotted)]
        );
        let decl: Vec<String> = vec![
            "func MarkFlagRequired(f *F, n string) error {".into(),
            "  return nil".into(),
            "}".into(),
        ];
        assert!(call_sites(&decl, 1, 3, "MarkFlagRequired", None).is_empty());
    }

    #[test]
    fn method_vs_function_binding() {
        // Go: c.MarkFlagRequired( in same dir binds the method, not the func.
        assert!(form_matches(CallForm::Dotted, true, true, true));
        assert!(!form_matches(CallForm::Dotted, false, true, true));
        // Go: MarkFlagRequired(flags, n) bare binds the func, not the method.
        assert!(form_matches(CallForm::Bare, false, true, true));
        assert!(!form_matches(CallForm::Bare, true, true, true));
        // Go cross-package: service.ShouldDisableChannel( binds the func.
        assert!(form_matches(CallForm::Dotted, false, false, true));
    }
}
