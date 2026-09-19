//! Transactional application of a [`RewritePlan`].
//!
//! This is the rewrite pipeline's last line of defence. A half-applied plan
//! (three files rewritten, the fourth failing) leaves a tree that does not
//! compile, and the user may have no git index to recover from. Every entry
//! point therefore guarantees: **all sites are written, or no byte on disk
//! changes**.
//!
//! ## Atomicity
//!
//! Two layers, because they fail in different ways:
//!
//! 1. **In-memory snapshot** of every touched file, taken after the stale /
//!    overlap / encoding / permission checks and before the first write.
//!    Any later error restores those bytes. This is the multi-file
//!    transaction. It is lost if the process is killed; there is no journal.
//! 2. **Temp file + `rename` in the same directory** for each individual
//!    write. On Unix `rename(2)` replaces the destination atomically, so a
//!    crash mid-write cannot leave a truncated file. The temp file lives
//!    next to the *canonical* target so it is on the same filesystem
//!    (rename across devices is `EXDEV`, not atomic).
//!
//! On Windows, `std::fs::rename` cannot replace an existing file. The
//! fallback is "move dest aside, move temp into place, delete the backup".
//! That sequence is not atomic: a crash between the first two steps leaves
//! the original content under a `*.astrolabe-bak-*` name. Rollback still
//! puts the snapshot back when the process stays alive.
//!
//! Other documented holes: a crash after file *k* of *n* has been renamed
//! leaves a mixed tree (in-memory rollback never runs); TOCTOU between the
//! stale check and the rename; networked filesystems that lie about rename
//! atomicity; running out of disk *during rollback* after a failed write
//! that already grew other files.
//!
//! ## Symlinks
//!
//! Edits follow the link and rewrite the target. A code rewrite that
//! `rename`d onto the link path would replace the symlink with a regular
//! file and leave the real source untouched. Destinations whose canonical
//! path escapes `root` are refused.
//!
//! ## Encoding
//!
//! Only UTF-8. Invalid sequences are rejected with [`RewriteError::Io`]
//! before any write; we never pass `String` slices through a lossy decode.

use super::{RewriteError, RewritePlan, Site};
use crate::types::RelPath;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Outcome of a fully committed plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    /// Touched files, sorted and unique.
    pub files: Vec<RelPath>,
    pub sites_applied: usize,
}

/// Post-write check injected by the verify workstream (or a test).
///
/// Called once per touched file *after* every file in the plan has been
/// committed, with the new UTF-8 source. Returning `Err` rolls the whole
/// transaction back. Closures implement this trait.
pub trait Verifier {
    fn verify(&mut self, path: &RelPath, source: &str) -> Result<(), RewriteError>;
}

impl<F> Verifier for F
where
    F: FnMut(&RelPath, &str) -> Result<(), RewriteError>,
{
    fn verify(&mut self, path: &RelPath, source: &str) -> Result<(), RewriteError> {
        self(path, source)
    }
}

/// Apply `plan` under repository `root` as a single transaction.
///
/// `verifier` runs only after every file has been written. An empty plan is
/// a no-op and does not require `root` to exist.
pub fn apply(
    root: impl AsRef<Path>,
    plan: &RewritePlan,
    verifier: impl Verifier,
) -> Result<ApplyReport, RewriteError> {
    apply_inner(root.as_ref(), plan, verifier)
}

fn apply_inner(
    root: &Path,
    plan: &RewritePlan,
    mut verifier: impl Verifier,
) -> Result<ApplyReport, RewriteError> {
    if plan.sites.is_empty() {
        return Ok(ApplyReport {
            files: Vec::new(),
            sites_applied: 0,
        });
    }

    let hint = &plan.sites[0].path;
    let root = canonicalize_root(root, hint)?;
    let prepared = prepare(&root, plan)?;
    let files: Vec<RelPath> = prepared.iter().map(|f| f.rel.clone()).collect();
    let sites_applied = plan.sites.len();

    let mut pending = PendingRollback::new(&prepared);

    for file in &prepared {
        #[cfg(test)]
        if let Err(e) = failpoint::check(&file.rel) {
            return Err(pending.abort(e));
        }
        if let Err(e) = write_prepared(file) {
            return Err(pending.abort(e));
        }
        pending.mark_committed();
    }

    for file in &prepared {
        if let Err(e) = verifier.verify(&file.rel, &file.updated) {
            return Err(pending.abort(e));
        }
    }

    pending.disarm();
    Ok(ApplyReport {
        files,
        sites_applied,
    })
}

struct PreparedFile {
    rel: RelPath,
    dest: PathBuf,
    original: String,
    updated: String,
    permissions: Permissions,
}

fn prepare(root: &Path, plan: &RewritePlan) -> Result<Vec<PreparedFile>, RewriteError> {
    let mut grouped: BTreeMap<RelPath, Vec<&Site>> = BTreeMap::new();
    for site in &plan.sites {
        grouped.entry(site.path.clone()).or_default().push(site);
    }

    let mut prepared = Vec::with_capacity(grouped.len());
    for (rel, sites) in grouped {
        let dest = resolve_dest(root, &rel)?;
        let original = read_utf8(&rel, &dest)?;
        let meta = fs::metadata(&dest).map_err(|e| io_err(&rel, e))?;
        if !meta.is_file() {
            return Err(RewriteError::Io(
                rel,
                "destination is not a regular file".into(),
            ));
        }
        if meta.permissions().readonly() {
            return Err(RewriteError::Io(rel, "file is read-only".into()));
        }
        check_sites(&rel, &original, &sites)?;
        let updated = apply_sites(&original, &sites);
        prepared.push(PreparedFile {
            rel,
            dest,
            original,
            updated,
            permissions: meta.permissions(),
        });
    }
    Ok(prepared)
}

fn canonicalize_root(root: &Path, hint: &RelPath) -> Result<PathBuf, RewriteError> {
    let canon = fs::canonicalize(root).map_err(|e| {
        RewriteError::Io(hint.clone(), format!("cannot resolve repository root: {e}"))
    })?;
    if !canon.is_dir() {
        return Err(RewriteError::Io(
            hint.clone(),
            "repository root is not a directory".into(),
        ));
    }
    Ok(canon)
}

/// Follow symlinks, then refuse anything that landed outside `root`.
fn resolve_dest(root: &Path, rel: &RelPath) -> Result<PathBuf, RewriteError> {
    let joined = root.join(rel.as_str());
    let dest = fs::canonicalize(&joined).map_err(|e| io_err(rel, e))?;
    if !dest.starts_with(root) {
        return Err(RewriteError::Io(
            rel.clone(),
            "canonical path escapes the repository root".into(),
        ));
    }
    Ok(dest)
}

fn read_utf8(rel: &RelPath, path: &Path) -> Result<String, RewriteError> {
    let bytes = fs::read(path).map_err(|e| io_err(rel, e))?;
    String::from_utf8(bytes)
        .map_err(|_| RewriteError::Io(rel.clone(), "file is not valid UTF-8".into()))
}

fn check_sites(rel: &RelPath, source: &str, sites: &[&Site]) -> Result<(), RewriteError> {
    let mut ranges: Vec<(usize, usize)> =
        sites.iter().map(|s| (s.range.start, s.range.end)).collect();
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        let (a0, a1) = pair[0];
        let (b0, _) = pair[1];
        // Identical start (including two insertions at the same point) or a
        // true overlap. Adjacent ranges (`a1 == b0`) are allowed.
        if a0 == b0 || a1 > b0 {
            return Err(RewriteError::Overlapping(rel.clone()));
        }
    }

    for site in sites {
        let slice = slice_at(rel, source, site.range.start, site.range.end)?;
        if slice != site.current {
            return Err(RewriteError::Stale(rel.clone()));
        }
    }
    Ok(())
}

fn slice_at<'a>(
    rel: &RelPath,
    source: &'a str,
    start: usize,
    end: usize,
) -> Result<&'a str, RewriteError> {
    if start > end || end > source.len() {
        return Err(RewriteError::Stale(rel.clone()));
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return Err(RewriteError::Io(
            rel.clone(),
            format!("byte range {start}..{end} is not on a UTF-8 character boundary"),
        ));
    }
    Ok(&source[start..end])
}

/// Apply sites from the highest byte offset down so earlier ranges stay valid.
fn apply_sites(source: &str, sites: &[&Site]) -> String {
    let mut ordered = sites.to_vec();
    ordered.sort_by(|a, b| {
        b.range
            .start
            .cmp(&a.range.start)
            .then(b.range.end.cmp(&a.range.end))
    });
    let mut out = source.to_owned();
    for site in ordered {
        out.replace_range(site.range.start..site.range.end, &site.replacement);
    }
    out
}

fn write_prepared(file: &PreparedFile) -> Result<(), RewriteError> {
    write_atomically(&file.dest, &file.updated, &file.permissions).map_err(|e| io_err(&file.rel, e))
}

fn write_atomically(dest: &Path, contents: &str, perm: &Permissions) -> io::Result<()> {
    let (tmp, mut file) = temp_sibling(dest)?;
    let guard = TmpGuard(tmp.clone());
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::set_permissions(&tmp, perm.clone())?;
    install_temp(&tmp, dest)?;
    std::mem::forget(guard);
    Ok(())
}

fn temp_sibling(dest: &Path) -> io::Result<(PathBuf, File)> {
    let dir = dest
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let pid = std::process::id();
    for _ in 0..1024 {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.astrolabe-{pid}-{n}.tmp"));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "exhausted temp file names",
    ))
}

fn install_temp(tmp: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(tmp, dest)
    }
    #[cfg(not(unix))]
    {
        install_temp_replace(tmp, dest)
    }
}

#[cfg(not(unix))]
fn install_temp_replace(tmp: &Path, dest: &Path) -> io::Result<()> {
    if !dest.exists() {
        return fs::rename(tmp, dest);
    }
    let bak = {
        let dir = dest.parent().unwrap_or(Path::new("."));
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        dir.join(format!(".{name}.astrolabe-bak-{n}"))
    };
    fs::rename(dest, &bak)?;
    match fs::rename(tmp, dest) {
        Ok(()) => {
            let _ = fs::remove_file(&bak);
            Ok(())
        }
        Err(e) => {
            let _ = fs::rename(&bak, dest);
            Err(e)
        }
    }
}

struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct PendingRollback<'a> {
    files: &'a [PreparedFile],
    committed: usize,
    armed: bool,
}

impl<'a> PendingRollback<'a> {
    fn new(files: &'a [PreparedFile]) -> Self {
        Self {
            files,
            committed: 0,
            armed: true,
        }
    }

    fn mark_committed(&mut self) {
        self.committed += 1;
    }

    fn abort(&mut self, primary: RewriteError) -> RewriteError {
        self.armed = false;
        let rollback = restore_committed(self.files, self.committed);
        merge_error(primary, rollback)
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRollback<'_> {
    fn drop(&mut self) {
        if self.armed && self.committed > 0 {
            tracing::error!(
                count = self.committed,
                "rewrite transaction aborted without a clean error path; rolling back"
            );
            let _ = restore_committed(self.files, self.committed);
        }
    }
}

struct RollbackReport {
    restored: Vec<RelPath>,
    failed: Vec<(RelPath, String)>,
}

fn restore_committed(
    files: &[PreparedFile],
    committed: usize,
) -> Result<Vec<RelPath>, RollbackReport> {
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for file in files.iter().take(committed).rev() {
        match write_atomically(&file.dest, &file.original, &file.permissions) {
            Ok(()) => restored.push(file.rel.clone()),
            Err(e) => failed.push((file.rel.clone(), e.to_string())),
        }
    }
    if failed.is_empty() {
        Ok(restored)
    } else {
        Err(RollbackReport { restored, failed })
    }
}

fn merge_error(
    primary: RewriteError,
    rollback: Result<Vec<RelPath>, RollbackReport>,
) -> RewriteError {
    match rollback {
        Ok(_) => primary,
        Err(report) => {
            tracing::error!(
                restored = ?report.restored.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                unrestored = ?report
                    .failed
                    .iter()
                    .map(|(p, e)| format!("{p}: {e}"))
                    .collect::<Vec<_>>(),
                primary = %primary,
                "rewrite rollback failed; working tree is inconsistent"
            );
            let restored = report
                .restored
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let still = report
                .failed
                .iter()
                .map(|(p, e)| format!("{p} ({e})"))
                .collect::<Vec<_>>()
                .join(", ");
            let msg = format!(
                "rollback failed after {primary}; restored: [{restored}]; \
                 still modified (manual restore required): [{still}]"
            );
            let path = report.failed[0].0.clone();
            RewriteError::Io(path, msg)
        }
    }
}

fn io_err(rel: &RelPath, err: impl std::fmt::Display) -> RewriteError {
    RewriteError::Io(rel.clone(), err.to_string())
}

#[cfg(test)]
mod failpoint {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static FAIL: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub struct ArmGuard;

    impl Drop for ArmGuard {
        fn drop(&mut self) {
            FAIL.with(|f| *f.borrow_mut() = None);
        }
    }

    pub fn arm(path: &str) -> ArmGuard {
        FAIL.with(|f| *f.borrow_mut() = Some(path.to_string()));
        ArmGuard
    }

    pub fn check(rel: &RelPath) -> Result<(), RewriteError> {
        FAIL.with(|f| {
            if f.borrow().as_deref() == Some(rel.as_str()) {
                Err(RewriteError::Io(
                    rel.clone(),
                    "simulated write failure".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::{ByteRange, Evidence, Site};
    use crate::types::Confidence;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "astrolabe-tx-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, rel: &str, contents: &str) {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, contents).unwrap();
        }

        fn write_bytes(&self, rel: &str, contents: &[u8]) {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, contents).unwrap();
        }

        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.0.join(rel)).unwrap()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            clear_readonly_tree(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn clear_readonly_tree(root: &Path) {
        let meta = match fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(_) => return,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() | 0o700);
            let _ = fs::set_permissions(root, perm);
        }
        #[cfg(not(unix))]
        {
            let mut perm = meta.permissions();
            if perm.readonly() {
                perm.set_readonly(false);
                let _ = fs::set_permissions(root, perm);
            }
        }
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.flatten() {
                    clear_readonly_tree(&entry.path());
                }
            }
        }
    }

    fn site(path: &str, start: usize, end: usize, current: &str, replacement: &str) -> Site {
        Site {
            path: RelPath::new(path),
            range: ByteRange { start, end },
            current: current.into(),
            replacement: replacement.into(),
        }
    }

    fn plan(sites: Vec<Site>) -> RewritePlan {
        RewritePlan {
            symbol: "old".into(),
            new_name: "new".into(),
            sites,
            evidence: Evidence::ScopeBinding,
            confidence: Confidence::Scoped,
            excluded: Vec::new(),
        }
    }

    fn ok_verify(_path: &RelPath, _source: &str) -> Result<(), RewriteError> {
        Ok(())
    }

    #[test]
    fn empty_plan_is_a_noop() {
        let dir = TestDir::new();
        dir.write("keep.rs", "untouched");
        let report = apply(dir.path(), &plan(vec![]), ok_verify).unwrap();
        assert!(report.files.is_empty());
        assert_eq!(report.sites_applied, 0);
        assert_eq!(dir.read("keep.rs"), "untouched");
    }

    #[test]
    fn applies_multiple_files_and_multiple_edits() {
        let dir = TestDir::new();
        dir.write("a.rs", "hello world");
        dir.write("b.rs", "foo bar baz");
        let p = plan(vec![
            site("a.rs", 0, 5, "hello", "hi"),
            site("b.rs", 0, 3, "foo", "FOO"),
            site("b.rs", 8, 11, "baz", "BAZ"),
        ]);
        let report = apply(dir.path(), &p, ok_verify).unwrap();
        assert_eq!(
            report
                .files
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>(),
            vec!["a.rs", "b.rs"]
        );
        assert_eq!(report.sites_applied, 3);
        assert_eq!(dir.read("a.rs"), "hi world");
        assert_eq!(dir.read("b.rs"), "FOO bar BAZ");
    }

    #[test]
    fn applies_same_line_replacements_back_to_front() {
        // Different-length replacements: left-to-right without fixing
        // offsets would miss the second X (or write into the wrong span).
        let dir = TestDir::new();
        dir.write("a.rs", "aaXbbXcc");
        let p = plan(vec![
            site("a.rs", 2, 3, "X", "YYYY"),
            site("a.rs", 5, 6, "X", "YYYY"),
        ]);
        apply(dir.path(), &p, ok_verify).unwrap();
        assert_eq!(dir.read("a.rs"), "aaYYYYbbYYYYcc");
    }

    #[test]
    fn stale_content_is_rejected_and_nothing_is_written() {
        let dir = TestDir::new();
        dir.write("a.rs", "hello");
        dir.write("b.rs", "world");
        let p = plan(vec![
            site("a.rs", 0, 5, "hello", "HELLO"),
            site("b.rs", 0, 5, "world", "WORLD"),
        ]);
        dir.write("b.rs", "werld");
        let err = apply(dir.path(), &p, ok_verify).unwrap_err();
        assert!(matches!(err, RewriteError::Stale(ref path) if path.as_str() == "b.rs"));
        assert_eq!(dir.read("a.rs"), "hello");
        assert_eq!(dir.read("b.rs"), "werld");
    }

    #[test]
    fn second_file_write_failure_restores_the_first() {
        let dir = TestDir::new();
        dir.write("a.rs", "aaa");
        dir.write("b.rs", "bbb");
        let p = plan(vec![
            site("a.rs", 0, 3, "aaa", "AAA"),
            site("b.rs", 0, 3, "bbb", "BBB"),
        ]);
        let _arm = failpoint::arm("b.rs");
        let err = apply(dir.path(), &p, ok_verify).unwrap_err();
        match err {
            RewriteError::Io(path, msg) => {
                assert_eq!(path.as_str(), "b.rs");
                assert!(msg.contains("simulated write failure"), "{msg}");
            }
            other => panic!("expected Io, got {other:?}"),
        }
        assert_eq!(dir.read("a.rs"), "aaa");
        assert_eq!(dir.read("b.rs"), "bbb");
    }

    #[test]
    fn verify_failure_rolls_everything_back() {
        let dir = TestDir::new();
        dir.write("a.rs", "aaa");
        dir.write("b.rs", "bbb");
        let p = plan(vec![
            site("a.rs", 0, 3, "aaa", "AAA"),
            site("b.rs", 0, 3, "bbb", "BBB"),
        ]);
        let err = apply(dir.path(), &p, |path: &RelPath, _src: &str| {
            if path.as_str() == "b.rs" {
                Err(RewriteError::VerificationFailed(path.clone()))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(
            matches!(err, RewriteError::VerificationFailed(ref path) if path.as_str() == "b.rs")
        );
        assert_eq!(dir.read("a.rs"), "aaa");
        assert_eq!(dir.read("b.rs"), "bbb");
    }

    #[test]
    fn readonly_file_is_refused_before_any_write() {
        let dir = TestDir::new();
        dir.write("a.rs", "aaa");
        dir.write("b.rs", "bbb");
        let path = dir.path().join("b.rs");
        let mut perm = fs::metadata(&path).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&path, perm).unwrap();

        let p = plan(vec![
            site("a.rs", 0, 3, "aaa", "AAA"),
            site("b.rs", 0, 3, "bbb", "BBB"),
        ]);
        let err = apply(dir.path(), &p, ok_verify).unwrap_err();
        match err {
            RewriteError::Io(rel, msg) => {
                assert_eq!(rel.as_str(), "b.rs");
                assert!(msg.contains("read-only"), "{msg}");
            }
            other => panic!("expected Io, got {other:?}"),
        }
        assert_eq!(dir.read("a.rs"), "aaa");
        assert_eq!(dir.read("b.rs"), "bbb");
    }

    #[test]
    fn non_utf8_file_is_rejected() {
        let dir = TestDir::new();
        dir.write_bytes("bin.rs", &[b'h', b'i', 0xff, b'!']);
        let p = plan(vec![site("bin.rs", 0, 2, "hi", "ok")]);
        let err = apply(dir.path(), &p, ok_verify).unwrap_err();
        match err {
            RewriteError::Io(rel, msg) => {
                assert_eq!(rel.as_str(), "bin.rs");
                assert!(msg.contains("UTF-8"), "{msg}");
            }
            other => panic!("expected Io, got {other:?}"),
        }
        assert_eq!(
            fs::read(dir.path().join("bin.rs")).unwrap(),
            vec![b'h', b'i', 0xff, b'!']
        );
    }

    #[test]
    fn overlapping_ranges_are_rejected() {
        let dir = TestDir::new();
        dir.write("a.rs", "abcdefgh");
        let p = plan(vec![
            site("a.rs", 0, 5, "abcde", "X"),
            site("a.rs", 3, 8, "defgh", "Y"),
        ]);
        let err = apply(dir.path(), &p, ok_verify).unwrap_err();
        assert!(matches!(err, RewriteError::Overlapping(ref path) if path.as_str() == "a.rs"));
        assert_eq!(dir.read("a.rs"), "abcdefgh");
    }

    fn panicking_verify(_: &RelPath, _: &str) -> Result<(), RewriteError> {
        panic!("verify boom");
    }

    #[test]
    fn verify_panic_still_restores() {
        let dir = TestDir::new();
        dir.write("a.rs", "aaa");
        let p = plan(vec![site("a.rs", 0, 3, "aaa", "AAA")]);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = apply(dir.path(), &p, panicking_verify);
        }));
        assert!(panicked.is_err());
        assert_eq!(dir.read("a.rs"), "aaa");
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlink_and_does_not_replace_the_link() {
        let dir = TestDir::new();
        dir.write("src/target.rs", "old_name");
        let link = dir.path().join("link.rs");
        std::os::unix::fs::symlink(dir.path().join("src/target.rs"), &link).unwrap();

        let p = plan(vec![site("link.rs", 0, 8, "old_name", "new_name")]);
        apply(dir.path(), &p, ok_verify).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the path in the plan must remain a symlink"
        );
        assert_eq!(dir.read("src/target.rs"), "new_name");
        assert_eq!(dir.read("link.rs"), "new_name");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_that_escapes_the_repo() {
        let dir = TestDir::new();
        let outside = std::env::temp_dir().join(format!(
            "astrolabe-tx-outside-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&outside, "secret").unwrap();
        let link = dir.path().join("escape.rs");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let p = plan(vec![site("escape.rs", 0, 6, "secret", "leaked")]);
        let err = apply(dir.path(), &p, ok_verify).unwrap_err();
        match err {
            RewriteError::Io(rel, msg) => {
                assert_eq!(rel.as_str(), "escape.rs");
                assert!(msg.contains("escapes"), "{msg}");
            }
            other => panic!("expected Io, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&outside).unwrap(), "secret");
        let _ = fs::remove_file(&outside);
    }
}
