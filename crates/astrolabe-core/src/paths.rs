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

/// Strip a leading Windows verbatim prefix (`\\?\` / `\\?\UNC\`) when present.
///
/// Non-Windows paths, and Windows paths that are already non-verbatim, are
/// returned unchanged (as a [`PathBuf`] clone of the input). Safe to call on
/// any platform so unit tests can feed `\\?\C:\…` style strings on Unix.
pub fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        // `\\?\UNC\server\share\…` → `\\server\share\…`
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Human-facing path string without a Windows verbatim prefix.
pub fn display_path(path: &Path) -> String {
    without_verbatim_prefix(path).display().to_string()
}

fn looks_windows_path(s: &str) -> bool {
    // `C:/…`, `C:\…`, or UNC `//server/share`
    let b = s.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || s.starts_with("//")
}

/// Stable comparison key: no verbatim prefix, `/` separators, Windows paths
/// lowercased.
pub fn path_norm_key(path: &Path) -> String {
    let s = without_verbatim_prefix(path)
        .to_string_lossy()
        .replace('\\', "/");
    if looks_windows_path(&s) {
        s.to_ascii_lowercase()
    } else {
        s
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
    // Reject `/proj` matching `/proj-other` (prefix without boundary).
    Some(PathBuf::from(rest))
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
}
