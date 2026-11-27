//! Path comparison helpers that treat Windows verbatim (`\\?\`) and
//! non-verbatim forms of the same path as equal.
//!
//! On Windows, [`std::fs::canonicalize`] returns paths with a `\\?\` (or
//! `\\?\UNC\`) prefix. OS file-watch backends and agents often emit the
//! ordinary `C:\…` form. Byte-wise [`Path::starts_with`] / [`Path::strip_prefix`]
//! then fail, which used to mark every watch event as "foreign" and degrade
//! the watcher to polling, and made status text show the raw `\\?\` prefix.
//!
//! Comparison keys also unify `\` / `/` and ASCII-lowercase Windows drive/UNC
//! paths so the same helpers work in unit tests on Unix (where `\` is not a
//! separator) and tolerate drive-letter case differences on Windows.

use std::path::{Path, PathBuf};

fn strip_ascii_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    split_ascii_prefix_ignore_case(s, prefix).map(|(_, rest)| rest)
}

fn split_ascii_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    debug_assert!(prefix.is_ascii());
    let bytes = s.as_bytes();
    let p = prefix.as_bytes();
    if prefix.is_ascii() && bytes.len() >= p.len() && bytes[..p.len()].eq_ignore_ascii_case(p) {
        let head = s.get(..p.len())?;
        let rest = s.get(p.len()..)?;
        Some((head, rest))
    } else {
        None
    }
}

fn split_ascii_drive_prefix(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some((s.get(..2)?, s.get(2..)?))
    } else {
        None
    }
}

/// Strip a leading Windows verbatim prefix (`\\?\` / `\\?\UNC\`) when present.
///
/// Non-Windows paths, Windows device namespace paths (`\\.\...`), and Windows
/// paths that are already non-verbatim are returned unchanged (as a
/// [`PathBuf`] clone of the input). Safe to call on any platform so unit tests
/// can feed `\\?\C:\…` or `//?/C:/…` style strings on Unix.
pub fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\") {
        // `\\?\UNC\server\share\…` (case-insensitive) → `\\server\share\…`
        if let Some(after_unc) = strip_ascii_prefix_ignore_case(rest, r"UNC\") {
            PathBuf::from(format!(r"\\{after_unc}"))
        } else if let Some(after_unc) = strip_ascii_prefix_ignore_case(rest, "UNC/") {
            PathBuf::from(format!("//{after_unc}"))
        } else {
            PathBuf::from(rest)
        }
    } else if let Some(rest) = raw.strip_prefix("//?/") {
        // `//?/UNC/server/share/…` (case-insensitive) → `//server/share/…`
        if let Some(after_unc) = strip_ascii_prefix_ignore_case(rest, "UNC/") {
            PathBuf::from(format!("//{after_unc}"))
        } else if let Some(after_unc) = strip_ascii_prefix_ignore_case(rest, r"UNC\") {
            PathBuf::from(format!(r"\\{after_unc}"))
        } else {
            PathBuf::from(rest)
        }
    } else {
        path.to_path_buf()
    }
}

/// Human-facing path string without a Windows verbatim prefix.
///
/// Uppercases an ASCII Windows drive letter (e.g. `c:\repo` → `C:\repo`) so
/// displayed roots stay consistent across sessions.
pub fn display_path(path: &Path) -> String {
    let s = without_verbatim_prefix(path).display().to_string();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_lowercase() {
        let upper = (bytes[0] as char).to_ascii_uppercase();
        if let Some(rest) = s.get(1..) {
            return format!("{upper}{rest}");
        }
    }
    s
}

fn looks_windows_path(s: &str) -> bool {
    // `C:/…`, `C:\…`, or UNC / device namespace `//...`
    let b = s.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || s.starts_with("//")
}

fn trim_trailing_slashes_preserving_root(s: &str) -> &str {
    if s.is_empty() {
        return s;
    }
    if s.bytes().all(|b| b == b'/') {
        return if s.len() == 2 { "//" } else { "/" };
    }
    let b = s.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
        let trimmed = s.trim_end_matches('/');
        if trimmed.len() <= 2 {
            return s.get(..3).unwrap_or(trimmed);
        }
        return trimmed;
    }
    s.trim_end_matches('/')
}

/// Stable comparison key: no verbatim prefix, `/` separators, trailing `/`
/// trimmed on non-root paths (preserving POSIX `/`, drive root `c:/`, and
/// UNC share root `//server/share`), Windows paths lowercased.
pub fn path_norm_key(path: &Path) -> String {
    let slashed = slashes_preserving_root(&path.to_string_lossy());
    let trimmed = trim_trailing_slashes_preserving_root(&slashed);
    if looks_windows_path(trimmed) {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// Component-wise equality after stripping Windows verbatim prefixes.
pub fn paths_eq_normalized(a: &Path, b: &Path) -> bool {
    path_norm_key(a) == path_norm_key(b)
}

/// True when `path` equals `root` or is a component-bounded descendant, after
/// stripping Windows verbatim prefixes on both sides.
pub fn is_under_normalized(root: &Path, path: &Path) -> bool {
    strip_prefix_normalized(root, path).is_some()
}

/// `path` relative to `root` after stripping Windows verbatim prefixes.
///
/// Returns `Some("")` (empty relative) when the paths are equal, `Some(rel)`
/// when `path` is under `root` (relative uses `/` separators), and `None`
/// when it is not a component-bounded prefix match.
pub fn strip_prefix_normalized(root: &Path, path: &Path) -> Option<PathBuf> {
    let root_s = path_norm_key(root);
    let path_s = path_norm_key(path);
    if path_s == root_s {
        return Some(PathBuf::new());
    }
    if root_s.is_empty() {
        return None;
    }
    let prefix = if root_s.ends_with('/') {
        root_s
    } else {
        format!("{root_s}/")
    };
    let rest = path_s.strip_prefix(&prefix)?;
    // Comparison is case-insensitive for Windows roots, but returned relative
    // paths must retain the caller's case for case-sensitive index matching.
    let original = slashes_preserving_root(&path.to_string_lossy());
    let orig_trimmed = trim_trailing_slashes_preserving_root(&original);
    let start = orig_trimmed.len().checked_sub(rest.len())?;
    if !orig_trimmed.is_char_boundary(start) {
        return None;
    }
    orig_trimmed.get(start..).map(PathBuf::from)
}

/// `/`-separated form of a caller-supplied path: backslashes become `/` and a
/// Windows verbatim prefix (`\\?\` / `\\?\UNC\`) is dropped.
///
/// Windows device namespace paths (`\\.\...` → `//./...`) are not stripped.
/// Leading `./` and `/` are preserved: an absolute path must stay absolute for
/// [`normalize_path_filter_under`] to be able to trim it.
fn slashes_preserving_root(path: &str) -> String {
    let raw = path.replace('\\', "/");
    // `\\?\C:\…` / `\\?\UNC\server\share\…` arrive as `//?/C:/…` /
    // `//?/UNC/server/share/…` once the separators are unified.
    if let Some(rest) = raw.strip_prefix("//?/") {
        if let Some(after_unc) = strip_ascii_prefix_ignore_case(rest, "UNC/") {
            format!("//{after_unc}")
        } else {
            rest.to_string()
        }
    } else {
        raw
    }
}

/// Choose a safe `current_dir` for spawning child commands (especially
/// `cmd.exe` / `.cmd` wrappers on Windows, which reject `\\?\` verbatim CWDs).
///
/// Strips any verbatim prefix when the stripped path is `< 248` characters
/// and has no segment ending with `.` or `' '`; otherwise keeps the original
/// path untouched so long or trailing-dot/space paths preserve Win32 semantics.
pub fn command_cwd(path: &Path) -> PathBuf {
    let stripped = without_verbatim_prefix(path);
    let s = stripped.to_string_lossy();
    let has_trailing_dot_or_space = s
        .split(['/', '\\'])
        .any(|seg| !seg.is_empty() && (seg.ends_with('.') || seg.ends_with(' ')));
    if s.chars().count() < 248 && !has_trailing_dot_or_space {
        stripped
    } else {
        path.to_path_buf()
    }
}

fn split_first_segment(s: &str) -> (&str, Option<(char, &str)>) {
    match s.find(['/', '\\']) {
        Some(idx) => {
            if let (Some(head), Some(sep_and_tail)) = (s.get(..idx), s.get(idx..)) {
                let mut chars = sep_and_tail.chars();
                if let Some(sep) = chars.next() {
                    return (head, Some((sep, chars.as_str())));
                }
            }
            (s, None)
        }
        None => (s, None),
    }
}

/// Purely lexical `.` / `..` normalization without hitting the filesystem.
///
/// Works on relative, non-existent, POSIX, and Windows drive/UNC/verbatim
/// paths (including Windows-style paths passed in tests on Unix).
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return PathBuf::new();
    }

    let preferred_sep = if raw.contains('\\') && !raw.contains('/') {
        '\\'
    } else {
        '/'
    };

    // Parse optional root/prefix in a platform-independent way so Windows
    // drive, UNC, device, and verbatim paths normalize identically on Unix.
    let (prefix, is_rooted, root_sep, rest): (String, bool, char, &str) =
        if let Some((prefix_head, after_q)) = raw
            .strip_prefix(r"\\?\")
            .map(|rest| (r"\\?\", rest))
            .or_else(|| raw.strip_prefix("//?/").map(|rest| ("//?/", rest)))
        {
            if let Some((unc_head, after_unc)) = split_ascii_prefix_ignore_case(after_q, r"unc\")
                .or_else(|| split_ascii_prefix_ignore_case(after_q, "unc/"))
            {
                let (server, after_srv) = split_first_segment(after_unc);
                if let Some((sep1, tail1)) = after_srv {
                    let (share, after_share) = split_first_segment(tail1);
                    let pfx = format!("{prefix_head}{unc_head}{server}{sep1}{share}");
                    match after_share {
                        Some((sep2, rest)) => (pfx, true, sep2, rest),
                        None => (pfx, false, preferred_sep, ""),
                    }
                } else {
                    (
                        format!("{prefix_head}{unc_head}{server}"),
                        false,
                        preferred_sep,
                        "",
                    )
                }
            } else if let Some((drive, after_drive)) = split_ascii_drive_prefix(after_q) {
                let pfx = format!("{prefix_head}{drive}");
                if let Some(rest) = after_drive.strip_prefix('/') {
                    (pfx, true, '/', rest)
                } else if let Some(rest) = after_drive.strip_prefix('\\') {
                    (pfx, true, '\\', rest)
                } else {
                    (pfx, false, preferred_sep, after_drive)
                }
            } else {
                let (vol, after_vol) = split_first_segment(after_q);
                let pfx = format!("{prefix_head}{vol}");
                match after_vol {
                    Some((sep, rest)) => (pfx, true, sep, rest),
                    None => (pfx, false, preferred_sep, ""),
                }
            }
        } else if let Some((prefix_head, after_dev)) = raw
            .strip_prefix(r"\\.\")
            .map(|rest| (r"\\.\", rest))
            .or_else(|| raw.strip_prefix("//./").map(|rest| ("//./", rest)))
        {
            let (dev, after_seg) = split_first_segment(after_dev);
            let pfx = format!("{prefix_head}{dev}");
            match after_seg {
                Some((sep, rest)) => (pfx, true, sep, rest),
                None => (pfx, false, preferred_sep, ""),
            }
        } else if let Some((pfx, is_rooted, root_sep, rest)) = raw
            .strip_prefix(r"\\")
            .map(|rest| (r"\\", rest))
            .or_else(|| raw.strip_prefix("//").map(|rest| ("//", rest)))
            .and_then(|(lead, after_lead)| {
                if after_lead.starts_with(['/', '\\']) {
                    return None;
                }
                let (server, after_srv) = split_first_segment(after_lead);
                let (sep1, tail1) = after_srv?;
                let (share, after_share) = split_first_segment(tail1);
                if server.is_empty() || share.is_empty() {
                    return None;
                }
                let pfx = format!("{lead}{server}{sep1}{share}");
                Some(match after_share {
                    Some((sep2, rest)) => (pfx, true, sep2, rest),
                    None => (pfx, false, preferred_sep, ""),
                })
            })
        {
            (pfx, is_rooted, root_sep, rest)
        } else if let Some((drive, after_drive)) = split_ascii_drive_prefix(&raw) {
            let pfx = drive.to_string();
            if let Some(rest) = after_drive.strip_prefix('/') {
                (pfx, true, '/', rest)
            } else if let Some(rest) = after_drive.strip_prefix('\\') {
                (pfx, true, '\\', rest)
            } else {
                (pfx, false, preferred_sep, after_drive)
            }
        } else if raw.starts_with('/') {
            (String::new(), true, '/', raw.trim_start_matches('/'))
        } else {
            (String::new(), false, preferred_sep, raw.as_ref())
        };

    let mut stack: Vec<&str> = Vec::new();
    for seg in rest.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(&last) = stack.last() {
                    if last != ".." {
                        stack.pop();
                    } else if !is_rooted {
                        stack.push("..");
                    }
                } else if !is_rooted {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }

    let mut out = String::new();
    if !prefix.is_empty() {
        out.push_str(&prefix);
        if is_rooted {
            out.push(root_sep);
        }
        for (i, seg) in stack.iter().enumerate() {
            if i > 0 {
                out.push(preferred_sep);
            }
            out.push_str(seg);
        }
    } else if is_rooted {
        out.push(root_sep);
        for (i, seg) in stack.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(seg);
        }
    } else if stack.is_empty() {
        out.push('.');
    } else {
        for (i, seg) in stack.iter().enumerate() {
            if i > 0 {
                out.push(preferred_sep);
            }
            out.push_str(seg);
        }
    }

    PathBuf::from(out)
}

fn is_safe_relative(rel: &Path) -> bool {
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    let s = rel.to_string_lossy().replace('\\', "/");
    !s.starts_with("../") && s != ".." && !s.split('/').any(|seg| seg == "..")
}

fn canonicalize_best_effort(path: &Path) -> Option<PathBuf> {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return Some(canon);
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    while let Some(name) = cur.file_name() {
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(mut canon) = std::fs::canonicalize(parent) {
            for seg in tail.into_iter().rev() {
                canon.push(seg);
            }
            return Some(canon);
        }
        cur = parent;
    }
    None
}

/// Strip `root` from `path` with a zero-syscall lexical fast path, falling back
/// to `canonicalize` (supporting symlinks, NTFS junctions, and 8.3 short paths).
///
/// Rejects any relative result containing `Component::ParentDir` / `..`.
pub fn strip_prefix_normalized_with_canonical_fallback(
    root: &Path,
    path: &Path,
) -> Option<PathBuf> {
    let norm_root = lexical_normalize(root);
    let norm_path = lexical_normalize(path);
    if let Some(rel) = strip_prefix_normalized(&norm_root, &norm_path) {
        return is_safe_relative(&rel).then_some(rel);
    }

    let canon_root = canonicalize_best_effort(&norm_root);
    let canon_path = canonicalize_best_effort(&norm_path);
    if canon_root.is_none() && canon_path.is_none() {
        return None;
    }
    let eff_root = canon_root.as_deref().unwrap_or(&norm_root);
    let eff_path = canon_path.as_deref().unwrap_or(&norm_path);
    let rel = strip_prefix_normalized(eff_root, eff_path)?;
    is_safe_relative(&rel).then_some(rel)
}

/// Drop every leading `/` and `./` (the [`RelPath`] contract).
///
/// [`RelPath`]: crate::types::RelPath
fn strip_leading_dot_slash(s: &str) -> &str {
    let mut rest = s;
    loop {
        let trimmed = rest.trim_start_matches('/');
        match trimmed.strip_prefix("./") {
            Some(next) => rest = next,
            None => return trimmed,
        }
    }
}

/// Repo-relative, `/`-separated form of a caller `path_filter`.
///
/// Index paths are always `/`-separated and repo-relative, but callers (and
/// Windows MCP clients) often spell them natively:
///
/// - `crates\astrolabe-mcp` → `crates/astrolabe-mcp`
/// - `.\crates\astrolabe-mcp` → `crates/astrolabe-mcp`
/// - `C:\proj\crates\astrolabe-mcp` with `root = C:\proj` → `crates/astrolabe-mcp`
///
/// An absolute path outside `root` is only slash-normalised (it cannot be
/// read as repo-relative). Case is preserved: matching stays case-sensitive.
pub fn normalize_path_filter_under(root: &Path, filter: &str) -> String {
    let slashed = slashes_preserving_root(filter.trim());
    // Platform-independent absolute test: unit tests feed `C:\…` on Unix too.
    if slashed.starts_with('/') || looks_windows_path(&slashed) {
        if let Some(rel) = strip_prefix_normalized(root, Path::new(&slashed)) {
            let rel = rel.to_string_lossy();
            return strip_leading_dot_slash(&rel).to_string();
        }
    }
    strip_leading_dot_slash(&slashed).to_string()
}

/// [`normalize_path_filter_under`] without an index root: `\` → `/` plus the
/// leading `./` / `/` strip; absolute paths cannot be made repo-relative.
pub fn normalize_path_filter(filter: &str) -> String {
    strip_leading_dot_slash(&slashes_preserving_root(filter.trim())).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn strips_drive_verbatim_prefix() {
        let verbatim = PathBuf::from(r"\\?\C:\Users\z3437\project\AI-con");
        let plain = PathBuf::from(r"C:\Users\z3437\project\AI-con");
        assert_eq!(
            without_verbatim_prefix(&verbatim).to_string_lossy(),
            plain.to_string_lossy()
        );
        assert!(paths_eq_normalized(&verbatim, &plain));
        assert!(!display_path(&verbatim).contains(r"\\?\"));
    }

    #[test]
    fn strips_unc_verbatim_prefix() {
        let verbatim = PathBuf::from(r"\\?\UNC\server\share\repo");
        let plain = PathBuf::from(r"\\server\share\repo");
        assert!(paths_eq_normalized(&verbatim, &plain));
        assert_eq!(path_norm_key(&verbatim), "//server/share/repo");
    }

    #[test]
    fn under_and_strip_across_prefix_forms() {
        let root = PathBuf::from(r"\\?\C:\proj");
        let child = PathBuf::from(r"C:\proj\src\daemon");
        let file = PathBuf::from(r"C:\proj\src\daemon\main.go");
        let outside = PathBuf::from(r"C:\other\x");
        let sibling = PathBuf::from(r"C:\projectile\x");

        assert!(is_under_normalized(&root, &child));
        assert!(is_under_normalized(&root, &root));
        assert!(!is_under_normalized(&root, &outside));
        assert!(
            !is_under_normalized(&root, &sibling),
            "must be component-bounded"
        );

        let rel = strip_prefix_normalized(&root, &child).expect("under");
        assert_eq!(rel.to_string_lossy(), "src/daemon");

        let empty = strip_prefix_normalized(&root, &root).expect("eq");
        assert!(empty.as_os_str().is_empty());

        assert_eq!(
            strip_prefix_normalized(&root, &file)
                .unwrap()
                .to_string_lossy(),
            "src/daemon/main.go"
        );
        assert!(strip_prefix_normalized(&root, &outside).is_none());
    }

    #[test]
    fn windows_relative_tail_preserves_index_path_case() {
        for (root, child) in [
            (r"C:\repo", r"\\?\C:\repo\Src\Daemon"),
            (r"\\?\C:\repo", "c:/REPO/Src/Daemon"),
            ("//?/C:/repo", r"C:\repo\Src\Daemon"),
            (
                r"\\?\UNC\server\share\repo",
                r"\\SERVER\SHARE\repo\Src\Daemon",
            ),
            (
                "//?/UNC/server/share/repo",
                "//server/share/repo/Src/Daemon",
            ),
        ] {
            assert_eq!(
                strip_prefix_normalized(Path::new(root), Path::new(child)).unwrap(),
                PathBuf::from("Src/Daemon")
            );
        }
        assert!(strip_prefix_normalized(Path::new("/repo"), Path::new("/Repo/Src")).is_none());
    }

    #[test]
    fn unix_paths_unchanged() {
        let p = PathBuf::from("/Users/zhoulei/tmp/Astrolabe");
        assert_eq!(without_verbatim_prefix(&p), p);
        assert!(paths_eq_normalized(&p, &p));
        assert!(is_under_normalized(&p, &p.join("crates")));
        assert_eq!(
            strip_prefix_normalized(&p, &p.join("crates/x"))
                .unwrap()
                .to_string_lossy(),
            "crates/x"
        );
    }

    #[test]
    fn normalize_path_filter_unifies_windows_separators() {
        // The defect repro: an index path is `crates/astrolabe-mcp/src/…`, so
        // a backslash filter must match after normalization.
        assert_eq!(
            normalize_path_filter(r"crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        assert_eq!(
            normalize_path_filter(r".\crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        assert_eq!(
            normalize_path_filter(r"crates\astrolabe-mcp\src\server.rs"),
            "crates/astrolabe-mcp/src/server.rs"
        );
        // Forward-slash filters keep working unchanged.
        assert_eq!(
            normalize_path_filter("crates/astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        // Leading `/` and `./` are stripped like `RelPath::new` does; case is
        // preserved (matching stays case-sensitive).
        assert_eq!(normalize_path_filter("/src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_filter("./src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_filter("././src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_filter("Src/Lib.RS"), "Src/Lib.RS");
        // Surrounding whitespace is not part of the path.
        assert_eq!(normalize_path_filter("  src\\lib.rs  "), "src/lib.rs");
        assert_eq!(normalize_path_filter(""), "");
    }

    #[test]
    fn normalize_path_filter_trims_absolute_paths_under_root() {
        let root = PathBuf::from(r"C:\proj");
        assert_eq!(
            normalize_path_filter_under(&root, r"C:\proj\crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        // Verbatim root / caller form, and a differing separators mix.
        assert_eq!(
            normalize_path_filter_under(&root, r"\\?\C:\proj\crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        assert_eq!(
            normalize_path_filter_under(&PathBuf::from(r"\\?\C:\proj"), r"C:\proj\src\main.rs"),
            "src/main.rs"
        );
        // The caller's relative spelling (case) survives the trimming.
        assert_eq!(
            normalize_path_filter_under(&root, r"C:\Proj\Crates\Astrolabe-Mcp"),
            "Crates/Astrolabe-Mcp"
        );
        // Relative filters are only slash-normalized.
        assert_eq!(
            normalize_path_filter_under(&root, r"crates\astrolabe-mcp"),
            "crates/astrolabe-mcp"
        );
        // Absolute but outside the root: cannot be repo-relative, so only the
        // separators change (the caller gets the usual "matches no path" hint).
        assert_eq!(
            normalize_path_filter_under(&root, r"D:\other\repo"),
            "D:/other/repo"
        );
        assert_eq!(
            normalize_path_filter_under(&root, r"C:\projectile\x"),
            "C:/projectile/x"
        );
    }

    #[test]
    fn normalize_path_filter_under_unix_root() {
        let root = PathBuf::from("/Users/zhoulei/tmp/Astrolabe");
        assert_eq!(
            normalize_path_filter_under(&root, "/Users/zhoulei/tmp/Astrolabe/crates/x"),
            "crates/x"
        );
        assert_eq!(normalize_path_filter_under(&root, "crates/x"), "crates/x");
        // Same path as the root: empty filter (whole repo).
        assert_eq!(
            normalize_path_filter_under(&root, "/Users/zhoulei/tmp/Astrolabe"),
            ""
        );
    }

    #[test]
    fn without_verbatim_handles_case_insensitive_unc_forward_slash_and_device_paths() {
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\unc\srv\share\repo")),
            PathBuf::from(r"\\srv\share\repo")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new("//?/UNC/srv/share/repo")),
            PathBuf::from("//srv/share/repo")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new("//?/unc/srv/share/repo")),
            PathBuf::from("//srv/share/repo")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new("//?/C:/repo")),
            PathBuf::from("C:/repo")
        );
        // Device namespace paths (`\\.\...`) must NEVER be stripped.
        let pipe = Path::new(r"\\.\pipe\test");
        assert_eq!(without_verbatim_prefix(pipe), pipe);
        assert_eq!(display_path(pipe), r"\\.\pipe\test");
        assert_eq!(path_norm_key(pipe), "//./pipe/test");
    }

    #[test]
    fn display_path_uppercases_windows_drive_letter() {
        assert_eq!(display_path(Path::new(r"c:\repo")), r"C:\repo");
        assert_eq!(display_path(Path::new(r"\\?\c:\repo")), r"C:\repo");
        assert_eq!(display_path(Path::new("//?/c:/repo")), "C:/repo");
        assert_eq!(
            display_path(Path::new("/Users/dev/repo")),
            "/Users/dev/repo"
        );
    }

    #[test]
    fn path_norm_key_trims_trailing_slashes_while_preserving_roots() {
        assert_eq!(path_norm_key(Path::new("/")), "/");
        assert_eq!(path_norm_key(Path::new("/repo/")), "/repo");
        assert_eq!(path_norm_key(Path::new(r"C:\")), "c:/");
        assert_eq!(path_norm_key(Path::new("c:/")), "c:/");
        assert_eq!(path_norm_key(Path::new(r"C:\repo\")), "c:/repo");
        assert_eq!(path_norm_key(Path::new("c:/repo/")), "c:/repo");
        assert_eq!(
            path_norm_key(Path::new(r"\\?\unc\srv\share\repo\")),
            "//srv/share/repo"
        );
        assert_eq!(
            path_norm_key(Path::new(r"\\server\share\")),
            "//server/share"
        );
        assert_eq!(
            path_norm_key(Path::new(r"\\server\share")),
            "//server/share"
        );

        assert!(paths_eq_normalized(
            Path::new(r"C:\proj\"),
            Path::new(r"C:\proj")
        ));
        assert_eq!(
            strip_prefix_normalized(Path::new(r"C:\proj\"), Path::new(r"C:\proj")),
            Some(PathBuf::new())
        );
        assert_eq!(
            strip_prefix_normalized(Path::new(r"C:\proj"), Path::new(r"C:\proj\")),
            Some(PathBuf::new())
        );
        // Trailing slash on child must not cause out-of-bounds or shifted slice.
        assert_eq!(
            strip_prefix_normalized(Path::new(r"C:\proj\"), Path::new(r"C:\proj\Src\Daemon\")),
            Some(PathBuf::from("Src/Daemon"))
        );
        assert_eq!(
            strip_prefix_normalized(Path::new(r"C:\"), Path::new(r"C:\Src\Daemon\")),
            Some(PathBuf::from("Src/Daemon"))
        );
        assert_eq!(
            strip_prefix_normalized(Path::new("/"), Path::new("/Src/Daemon/")),
            Some(PathBuf::from("Src/Daemon"))
        );
    }

    #[test]
    fn command_cwd_respects_248_char_limit_and_trailing_dot_or_space() {
        assert_eq!(
            command_cwd(Path::new(r"\\?\C:\repo")),
            PathBuf::from(r"C:\repo")
        );
        assert_eq!(
            command_cwd(Path::new(r"\\?\UNC\srv\share\repo")),
            PathBuf::from(r"\\srv\share\repo")
        );

        // 247 chars after stripping `\\?\` -> stripped.
        let under = format!(r"\\?\C:\{}", "a".repeat(244));
        assert_eq!(
            without_verbatim_prefix(Path::new(&under))
                .to_string_lossy()
                .chars()
                .count(),
            247
        );
        assert_eq!(
            command_cwd(Path::new(&under)),
            PathBuf::from(format!(r"C:\{}", "a".repeat(244)))
        );

        // Exactly 248 chars after stripping `\\?\` -> keep verbatim.
        let exact = format!(r"\\?\C:\{}", "a".repeat(245));
        assert_eq!(
            without_verbatim_prefix(Path::new(&exact))
                .to_string_lossy()
                .chars()
                .count(),
            248
        );
        assert_eq!(command_cwd(Path::new(&exact)), PathBuf::from(&exact));

        // Segment ending with `.` or `' '` -> keep verbatim.
        let dot_seg = Path::new(r"\\?\C:\foo.\bar");
        assert_eq!(command_cwd(dot_seg), dot_seg);
        let space_seg = Path::new(r"\\?\C:\foo \bar");
        assert_eq!(command_cwd(space_seg), space_seg);
    }

    #[test]
    fn lexical_normalize_resolves_dots_and_clamps_roots() {
        assert_eq!(lexical_normalize(Path::new("")), PathBuf::new());
        assert_eq!(lexical_normalize(Path::new(".")), PathBuf::from("."));
        assert_eq!(lexical_normalize(Path::new("a/..")), PathBuf::from("."));
        assert_eq!(
            lexical_normalize(Path::new("a/b/../c/./d")),
            PathBuf::from("a/c/d")
        );
        assert_eq!(
            lexical_normalize(Path::new("../a/b/..")),
            PathBuf::from("../a")
        );
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../../..")),
            PathBuf::from("/")
        );
        assert_eq!(
            lexical_normalize(Path::new("/../../etc/passwd")),
            PathBuf::from("/etc/passwd")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"C:\a\b\..\c\.\d")),
            PathBuf::from(r"C:\a\c\d")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"C:\..\..\a")),
            PathBuf::from(r"C:\a")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"\\?\C:\proj\src\..\lib.rs")),
            PathBuf::from(r"\\?\C:\proj\lib.rs")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"\\?\unc\srv\share\a\..\b")),
            PathBuf::from(r"\\?\unc\srv\share\b")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"\\srv\share\..\b")),
            PathBuf::from(r"\\srv\share\b")
        );
    }

    #[test]
    fn lexical_normalize_collapses_posix_multiple_leading_slashes() {
        assert_eq!(
            lexical_normalize(Path::new("///repo/../../etc/passwd")),
            PathBuf::from("/etc/passwd")
        );
        assert_eq!(
            lexical_normalize(Path::new("///repo")),
            PathBuf::from("/repo")
        );
        assert_eq!(
            strip_prefix_normalized_with_canonical_fallback(
                Path::new("///repo"),
                Path::new("///repo/../../etc/passwd")
            ),
            None
        );
    }

    #[test]
    fn strip_prefix_normalized_with_canonical_fallback_handles_lexical_escape_and_symlinks() {
        // Zero-syscall fast path with non-existent paths and `.` / `..`.
        assert_eq!(
            strip_prefix_normalized_with_canonical_fallback(
                Path::new(r"C:\repo\."),
                Path::new(r"C:\repo\sub\..\src\main.rs")
            ),
            Some(PathBuf::from("src/main.rs"))
        );
        // Escape via `..` must be rejected.
        assert_eq!(
            strip_prefix_normalized_with_canonical_fallback(
                Path::new("/nonexistent/repo"),
                Path::new("/nonexistent/repo/../secret.txt")
            ),
            None
        );
        assert_eq!(
            strip_prefix_normalized_with_canonical_fallback(
                Path::new("repo"),
                Path::new("repo/../../outside.txt")
            ),
            None
        );
        assert_eq!(
            strip_prefix_normalized_with_canonical_fallback(
                Path::new("."),
                Path::new("../outside.txt")
            ),
            None
        );

        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().expect("tempdir");
            let real_root = tmp.path().join("real");
            let link_root = tmp.path().join("link");
            std::fs::create_dir_all(real_root.join("src")).expect("mkdir");
            std::fs::write(real_root.join("src/lib.rs"), "pub fn x() {}").expect("write");
            std::os::unix::fs::symlink(&real_root, &link_root).expect("symlink");

            assert_eq!(
                strip_prefix_normalized_with_canonical_fallback(
                    &real_root,
                    &link_root.join("src/lib.rs")
                ),
                Some(PathBuf::from("src/lib.rs"))
            );
            assert_eq!(
                strip_prefix_normalized_with_canonical_fallback(
                    &link_root,
                    &real_root.join("src/lib.rs")
                ),
                Some(PathBuf::from("src/lib.rs"))
            );
            // Non-existent file under symlinked root.
            assert_eq!(
                strip_prefix_normalized_with_canonical_fallback(
                    &real_root,
                    &link_root.join("src/future.rs")
                ),
                Some(PathBuf::from("src/future.rs"))
            );
        }
    }

    #[test]
    fn multibyte_utf8_paths_do_not_panic_and_normalize_correctly() {
        // `\\?\C:\项目` has byte index 4 right inside `项` (bytes 3..6 after `C:\`).
        let p_short = Path::new(r"\\?\C:\项目");
        assert_eq!(without_verbatim_prefix(p_short), PathBuf::from(r"C:\项目"));
        assert_eq!(display_path(p_short), r"C:\项目");
        assert_eq!(path_norm_key(p_short), "c:/项目");
        assert_eq!(lexical_normalize(p_short), PathBuf::from(r"\\?\C:\项目"));
        assert_eq!(command_cwd(p_short), PathBuf::from(r"C:\项目"));

        // Non-ASCII right at the start of the verbatim tail (also exercises byte 4 inside UTF-8).
        let p_non_ascii_head = Path::new(r"\\?\中文");
        assert_eq!(
            without_verbatim_prefix(p_non_ascii_head),
            PathBuf::from("中文")
        );
        assert_eq!(display_path(p_non_ascii_head), "中文");
        assert_eq!(
            lexical_normalize(p_non_ascii_head),
            PathBuf::from(r"\\?\中文")
        );

        // `\\?\C:\项目 目录\a.ts`
        let p_space = Path::new(r"\\?\C:\项目 目录\a.ts");
        assert_eq!(
            without_verbatim_prefix(p_space),
            PathBuf::from(r"C:\项目 目录\a.ts")
        );
        assert_eq!(display_path(p_space), r"C:\项目 目录\a.ts");
        assert_eq!(path_norm_key(p_space), "c:/项目 目录/a.ts");
        assert_eq!(
            lexical_normalize(Path::new(r"\\?\C:\项目 目录\子目录\..\a.ts")),
            PathBuf::from(r"\\?\C:\项目 目录\a.ts")
        );

        // `//?/C:/项目 目录/a.ts`
        let p_fwd = Path::new("//?/C:/项目 目录/a.ts");
        assert_eq!(
            without_verbatim_prefix(p_fwd),
            PathBuf::from("C:/项目 目录/a.ts")
        );
        assert_eq!(display_path(p_fwd), "C:/项目 目录/a.ts");
        assert_eq!(path_norm_key(p_fwd), "c:/项目 目录/a.ts");
        assert!(paths_eq_normalized(p_space, p_fwd));
        assert_eq!(
            lexical_normalize(Path::new("//?/C:/项目 目录/子目录/../a.ts")),
            PathBuf::from("//?/C:/项目 目录/a.ts")
        );

        // `\\?\UNC\服务器\共享\项目\a.rs`
        let p_unc = Path::new(r"\\?\UNC\服务器\共享\项目\a.rs");
        assert_eq!(
            without_verbatim_prefix(p_unc),
            PathBuf::from(r"\\服务器\共享\项目\a.rs")
        );
        assert_eq!(path_norm_key(p_unc), "//服务器/共享/项目/a.rs");
        assert_eq!(
            lexical_normalize(Path::new(r"\\?\UNC\服务器\共享\项目\子目录\..\a.rs")),
            PathBuf::from(r"\\?\UNC\服务器\共享\项目\a.rs")
        );

        // `strip_prefix_normalized` with Chinese paths and mixed drive/ASCII case.
        assert_eq!(
            strip_prefix_normalized(
                Path::new(r"\\?\C:\项目"),
                Path::new(r"c:\项目\子目录\文件.rs")
            ),
            Some(PathBuf::from("子目录/文件.rs"))
        );
        assert_eq!(
            strip_prefix_normalized(
                Path::new(r"\\?\C:\项目"),
                Path::new(r"C:\项目\Öffentlich\文件.rs")
            ),
            Some(PathBuf::from("Öffentlich/文件.rs"))
        );
        assert_eq!(
            normalize_path_filter_under(p_short, r"C:\项目\子目录\文件.rs"),
            "子目录/文件.rs"
        );
        assert_eq!(
            strip_prefix_normalized_with_canonical_fallback(
                Path::new(r"\\?\C:\项目\."),
                Path::new(r"c:\项目\临时\..\子目录\文件.rs")
            ),
            Some(PathBuf::from("子目录/文件.rs"))
        );

        // `command_cwd` counts characters (not UTF-8 bytes) for the 248 limit.
        let under_248_chars = format!(r"\\?\C:\{}", "项".repeat(244));
        assert!(under_248_chars.len() > 248);
        assert_eq!(
            command_cwd(Path::new(&under_248_chars)),
            PathBuf::from(format!(r"C:\{}", "项".repeat(244)))
        );
        let exact_248_chars = format!(r"\\?\C:\{}", "项".repeat(245));
        assert_eq!(
            command_cwd(Path::new(&exact_248_chars)),
            PathBuf::from(&exact_248_chars)
        );
        let trailing_space_cn = Path::new(r"\\?\C:\项目 \子目录");
        assert_eq!(command_cwd(trailing_space_cn), trailing_space_cn);
        let trailing_dot_cn = Path::new(r"\\?\C:\项目.\子目录");
        assert_eq!(command_cwd(trailing_dot_cn), trailing_dot_cn);
    }
}
