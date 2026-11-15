//! Resolve the directory Astrolabe actually indexes.
//!
//! Like Serena's `--project-from-cwd`, the cwd sentinel `.` (and "no
//! argument") is **not** indexed as a literal relative path: the MCP process
//! cwd is classified against project boundaries and the **absolute** path is
//! stored.
//!
//! Markers:
//!
//! * `.git` — a directory, or a worktree/submodule pointer file
//! * `.astrolabe/project.yml` — an explicit project without git.
//!   `.serena/project.yml` is accepted for Serena compatibility. Only the
//!   YAML file counts: Astrolabe creates `.astrolabe/` (index store, lock,
//!   memories) in every root it indexes, and a bare `.serena/` is not a
//!   Serena project either.
//!
//! Explicit paths (`astrolabe /path/to/repo`, `ASTROLABE_ROOT=/path`) stay
//! as given after canonicalization — that is Serena's `--project` flag.
//!
//! Git wins over project files. [`resolve_root`] classifies the spawn
//! directory in this order instead of indexing it blindly (a blind fallback
//! once indexed a 350k-file parent of many git checkouts, and a non-git
//! parent with `.serena/project.yml` above six checkouts was indexed whole):
//!
//! 1. Walk **up** for the nearest `.git`. A nested checkout such as
//!    `repo/web` resolves to `repo`, a nested worktree wins over its ancestor
//!    repo, and a project file inside a repo does not split it.
//! 2. Look **down** one level for direct children that contain `.git`
//!    (hidden names such as `.Trash` are skipped). Exactly one hit is served
//!    directly as `Repo`; two or more are `MultiProject`, even when cwd has a
//!    project file.
//! 3. Walk **up** for the nearest project file (no git around).
//! 4. Count files (capped) and accept as `Single` or refuse as `TooLarge`.
//!
//! The MCP binary starts `MultiProject` in dispatcher mode (lazy per-child
//! indexing, see `session.rs` / `roots.rs`) and still refuses `TooLarge` on
//! the cwd sentinel (`exit 2`). An explicit requested root skips the refuse.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use astrolabe_core::scan::{count_files, ScanOptions};

/// Default file-count cap for an unmarked (non-git) root.
///
/// Overridden by `ASTROLABE_MAX_ROOT_FILES` in the binary.
pub const DEFAULT_MAX_ROOT_FILES: u64 = 20_000;

/// Outcome of [`resolve_root`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedRoot {
    /// The nearest `.git` ancestor, the only git child, or (without git) the
    /// nearest `.astrolabe/project.yml` / `.serena/project.yml` ancestor.
    Repo(PathBuf),
    /// No project marker, no child git repos, file count ≤ `limit`.
    Single(PathBuf),
    /// Two or more direct children contain `.git`. Hidden names are not listed.
    MultiProject {
        root: PathBuf,
        children: Vec<PathBuf>,
    },
    /// No project marker, no child git repos, file count exceeds `limit`.
    TooLarge {
        root: PathBuf,
        files: u64,
        limit: u64,
    },
}

/// Walk `start` and its parents: the nearest `.git` wins; without one, the
/// nearest project file. Upward only — [`resolve_root`] also probes child
/// repos between the two walks.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    find_git_root(start).or_else(|| find_project_file_root(start))
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    find_upward(start, has_git_marker)
}

fn find_project_file_root(start: &Path) -> Option<PathBuf> {
    find_upward(start, has_project_file)
}

fn find_upward(start: &Path, is_marked: fn(&Path) -> bool) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if is_marked(&dir) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// True when `dir` is a project root on its own: it has `.git` or a project
/// file. Used wherever a directory is labelled repo vs single.
pub fn is_project_dir(dir: &Path) -> bool {
    has_git_marker(dir) || has_project_file(dir)
}

/// `.git` may be a directory or a worktree/submodule gitdir pointer file.
fn has_git_marker(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// `.astrolabe/project.yml`, or Serena's `.serena/project.yml`. A bare
/// `.astrolabe/` is not enough: the index store creates it in every root.
fn has_project_file(dir: &Path) -> bool {
    has_astrolabe_project_file(dir) || has_serena_project_file(dir)
}

fn has_astrolabe_project_file(dir: &Path) -> bool {
    dir.join(".astrolabe").join("project.yml").is_file()
}

fn has_serena_project_file(dir: &Path) -> bool {
    dir.join(".serena").join("project.yml").is_file()
}

/// Nudge Serena-only projects toward the native marker.
fn note_serena_project_file(root: &Path) {
    if !has_git_marker(root) && !has_astrolabe_project_file(root) && has_serena_project_file(root) {
        tracing::info!(
            root = %root.display(),
            "project root marked by .serena/project.yml (Serena compatibility); \
             consider adding .astrolabe/project.yml"
        );
    }
}

/// True when the launch request means "detect from process cwd".
pub fn is_cwd_sentinel(path: &Path) -> bool {
    path == Path::new(".") || path == Path::new("./")
}

/// Classify `cwd` for the index-root guardrail.
///
/// Order (see the module docs): nearest `.git` up, git children down,
/// nearest project file up, then the file count. A git root found upward is
/// final: a nested repo is never reclassified because a parent directory
/// happens to contain sibling checkouts.
///
/// A marker found in an *ancestor* of `cwd` is size-checked like an unmarked
/// directory: a stray `~/.git` (dotfiles) or a repo above the limit must not
/// silently make the whole ancestor the index root. Such an ancestor is
/// skipped (with a warning) and `cwd` is classified instead; set
/// `ASTROLABE_ROOT` or raise `ASTROLABE_MAX_ROOT_FILES` to force it.
pub fn resolve_root(cwd: &Path, limit: u64) -> ResolvedRoot {
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if let Some(found) = find_git_root(cwd) {
        if let Some(found) = adopt_marked_root(found, &cwd_canon, limit) {
            return ResolvedRoot::Repo(found);
        }
    }
    let root = cwd_canon;
    let mut children = child_git_repos(&root);
    if children.len() == 1 {
        // A single nested repo needs no routing: serve it directly.
        return ResolvedRoot::Repo(children.remove(0));
    }
    if !children.is_empty() {
        return ResolvedRoot::MultiProject { root, children };
    }
    if let Some(found) = find_project_file_root(&root) {
        if let Some(found) = adopt_marked_root(found, &root, limit) {
            note_serena_project_file(&found);
            return ResolvedRoot::Repo(found);
        }
    }
    let (files, truncated) = count_root_files(&root, limit);
    if truncated {
        return ResolvedRoot::TooLarge { root, files, limit };
    }
    ResolvedRoot::Single(root)
}

/// `found` if it may serve as the root for `cwd`; an ancestor that
/// [`ancestor_rejection`] refuses is skipped with a warning.
fn adopt_marked_root(found: PathBuf, cwd: &Path, limit: u64) -> Option<PathBuf> {
    if found == cwd {
        return Some(found);
    }
    match ancestor_rejection(&found, limit) {
        None => Some(found),
        Some(reason) => {
            tracing::warn!(
                ancestor = %found.display(),
                cwd = %cwd.display(),
                "ignoring project marker in ancestor directory ({reason}); \
                 set ASTROLABE_ROOT or ASTROLABE_MAX_ROOT_FILES to use it"
            );
            None
        }
    }
}

/// Why an ancestor project root must not be adopted, if any.
fn ancestor_rejection(found: &Path, limit: u64) -> Option<String> {
    let is_home = ["HOME", "USERPROFILE"].iter().any(|var| {
        std::env::var_os(var)
            .and_then(|home| Path::new(&home).canonicalize().ok())
            .is_some_and(|home| home == found)
    });
    if is_home {
        return Some("it is the home directory".to_string());
    }
    let (_, truncated) = count_root_files(found, limit);
    truncated.then(|| format!("it holds more than {limit} files"))
}

/// `(files, truncated)` for the unmarked-root gate and startup telemetry.
///
/// `truncated` is true only when the tree holds **more than** `limit` files,
/// so a directory with exactly `limit` files is accepted. `files` is then a
/// lower bound (`> limit`). Wrapper around
/// [`astrolabe_core::scan::count_files`], which stops at its own cap.
pub fn count_root_files(root: &Path, limit: u64) -> (u64, bool) {
    let count = count_files(root, &ScanOptions::default(), limit.saturating_add(1));
    (count.files, count.files > limit)
}

/// Direct child directories of `root` that contain a `.git` marker.
///
/// Names starting with `.` (e.g. `.Trash`) are skipped. Used both by
/// [`resolve_root`] and by the binary to warn on an explicit root.
pub fn child_git_repos(root: &Path) -> Vec<PathBuf> {
    let mut children = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return children;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if has_git_marker(&path) {
            children.push(path.canonicalize().unwrap_or(path));
        }
    }
    children.sort();
    children
}

impl ResolvedRoot {
    /// Actionable refuse text for `MultiProject` / `TooLarge`. `None` means
    /// the binary should start.
    pub fn refuse_guidance(&self) -> Option<String> {
        match self {
            Self::MultiProject { root, children } => {
                Some(format_multi_project_refuse(root, children))
            }
            Self::TooLarge { root, files, limit } => Some(format!(
                "Astrolabe refused to index {}: found {} files (limit {limit}).\n\
                 cd into a sub-repo, or set ASTROLABE_ROOT to force a specific root",
                root.display(),
                if *files > *limit {
                    format!("more than {limit}")
                } else {
                    files.to_string()
                }
            )),
            Self::Repo(_) | Self::Single(_) => None,
        }
    }
}

fn format_multi_project_refuse(root: &Path, children: &[PathBuf]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Astrolabe refused to index {}: found {} nested git repositories:",
        root.display(),
        children.len()
    );
    let shown = children.len().min(20);
    for child in &children[..shown] {
        let _ = writeln!(out, "  {}", child.display());
    }
    if children.len() > 20 {
        let _ = writeln!(out, "  …and {} more", children.len() - 20);
    }
    let _ = write!(
        out,
        "cd into a sub-repo, or set ASTROLABE_ROOT to force a specific root"
    );
    out
}

/// Turn a user/MCP-supplied path into the directory that will be indexed.
///
/// * `.` / `./` — detect from cwd via [`find_project_root`] (nearest `.git`,
///   else nearest `.astrolabe/project.yml` / `.serena/project.yml`); if
///   none, canonicalize cwd. The child-repo probe and size / multi-project
///   refuse live in [`resolve_root`], applied by the binary for this sentinel.
/// * any other path — canonicalize that directory, do **not** walk up, and
///   skip the MultiProject / TooLarge refuse (explicit user intent).
pub fn resolve_index_root(requested: &Path) -> anyhow::Result<PathBuf> {
    let start = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()?.join(requested)
    };
    if !start.is_dir() {
        anyhow::bail!("not a directory: {}", requested.display());
    }

    if is_cwd_sentinel(requested) {
        if let Some(found) = find_project_root(&start) {
            tracing::info!(root = %found.display(), "detected project root from cwd");
            note_serena_project_file(&found);
            return Ok(found);
        }
        let cwd = start.canonicalize().unwrap_or(start);
        tracing::warn!(
            cwd = %cwd.display(),
            "no .git or .astrolabe/project.yml (or Serena's .serena/project.yml) \
             ancestor; indexing cwd as-is (Serena would leave the project inactive)"
        );
        return Ok(cwd);
    }

    Ok(start.canonicalize().unwrap_or(start))
}

/// Resolve a per-call MCP `index_root` argument.
///
/// Absolute paths are used as-is; relative paths join the process cwd at call
/// time. The result is canonicalized and must be an existing directory. Unlike
/// startup [`resolve_index_root`], this never walks up to `.git` or a project
/// file and never applies MultiProject / TooLarge refuses.
pub fn resolve_call_index_root(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("index_root is empty".to_string());
    }
    let requested = PathBuf::from(raw);
    let start = if requested.is_absolute() {
        requested
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| format!("cannot resolve index_root relative to process cwd: {e}"))?;
        cwd.join(requested)
    };
    let canon = std::fs::canonicalize(&start).map_err(|e| {
        format!(
            "index_root `{}` is not an existing path ({e})",
            start.display()
        )
    })?;
    if !canon.is_dir() {
        return Err(format!(
            "index_root `{}` is not a directory",
            canon.display()
        ));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-root-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_serena_marker(dir: &Path) {
        std::fs::create_dir_all(dir.join(".serena")).unwrap();
        std::fs::write(dir.join(".serena").join("project.yml"), "project_name: t\n").unwrap();
    }

    fn write_astrolabe_marker(dir: &Path) {
        std::fs::create_dir_all(dir.join(".astrolabe")).unwrap();
        std::fs::write(
            dir.join(".astrolabe").join("project.yml"),
            "project_name: t\n",
        )
        .unwrap();
    }

    /// What the index store leaves behind in every indexed root.
    fn write_astrolabe_store(dir: &Path) {
        let store = dir.join(".astrolabe");
        std::fs::create_dir_all(store.join("memories")).unwrap();
        std::fs::write(store.join("index.redb"), b"redb").unwrap();
        std::fs::write(store.join("indexer.lock"), b"").unwrap();
        std::fs::write(store.join(".gitignore"), "*\n").unwrap();
    }

    fn git_marker(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }

    fn write_n_files(dir: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }
    }

    #[test]
    fn finds_git_dir_from_nested_folder() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("pkg").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        let found = find_project_root(&nested).expect("git root");
        assert_eq!(found, root.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_git_worktree_pointer_file() {
        let root = unique_temp_dir();
        std::fs::write(root.join(".git"), "gitdir: /tmp/fake.git\n").unwrap();
        let found = find_project_root(&root).expect("worktree pointer");
        assert_eq!(found, root.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_serena_project_yml_from_nested_folder() {
        let root = unique_temp_dir();
        write_serena_marker(&root);
        let nested = root.join("src").join("pkg");
        std::fs::create_dir_all(&nested).unwrap();
        let found = find_project_root(&nested).expect("serena root");
        assert_eq!(found, root.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bare_serena_dir_without_project_yml_is_not_a_marker() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join(".serena")).unwrap();
        assert!(
            find_project_root(&root).is_none(),
            "Serena requires .serena/project.yml, not a bare .serena/ directory"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn none_when_no_marker() {
        let root = unique_temp_dir();
        assert!(find_project_root(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nearest_git_wins_over_ancestor() {
        let outer = unique_temp_dir();
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        let inner = outer.join("nested-repo");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        let found = find_project_root(&inner).expect("inner git");
        assert_eq!(
            found,
            inner.canonicalize().unwrap(),
            "a nested worktree must not be hijacked by an ancestor repo"
        );
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn ancestor_git_wins_over_nearer_project_file() {
        let outer = unique_temp_dir();
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        let inner = outer.join("sub");
        std::fs::create_dir_all(&inner).unwrap();
        write_serena_marker(&inner);
        write_astrolabe_marker(&inner);
        let found = find_project_root(&inner).expect("ancestor git");
        assert_eq!(
            found,
            outer.canonicalize().unwrap(),
            "a project file inside a repo must not split it"
        );
        match resolve_root(&inner, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => assert_eq!(found, outer.canonicalize().unwrap()),
            other => panic!("expected Repo(outer), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn nearest_git_wins_over_ancestor_serena() {
        let outer = unique_temp_dir();
        write_serena_marker(&outer);
        let inner = outer.join("nested-worktree");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        let found = find_project_root(&inner).expect("inner git");
        assert_eq!(
            found,
            inner.canonicalize().unwrap(),
            "a nested git worktree must not be hijacked by an ancestor .serena/project.yml"
        );
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn nested_serena_wins_over_ancestor_serena() {
        let outer = unique_temp_dir();
        write_serena_marker(&outer);
        let inner = outer.join("nested-serena");
        std::fs::create_dir_all(&inner).unwrap();
        write_serena_marker(&inner);
        let deep = inner.join("pkg");
        std::fs::create_dir_all(&deep).unwrap();
        let found = find_project_root(&deep).expect("inner serena");
        assert_eq!(
            found,
            inner.canonicalize().unwrap(),
            "nearest .serena/project.yml must win over an ancestor's"
        );
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn same_directory_git_and_serena_is_that_root() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write_serena_marker(&root);
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let found = find_project_root(&nested).expect("same-dir markers");
        assert_eq!(
            found,
            root.canonicalize().unwrap(),
            "either marker in the same directory resolves to that directory"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_astrolabe_project_yml_from_nested_folder() {
        let root = unique_temp_dir();
        write_astrolabe_marker(&root);
        let nested = root.join("src").join("pkg");
        std::fs::create_dir_all(&nested).unwrap();
        let found = find_project_root(&nested).expect("astrolabe root");
        assert_eq!(found, root.canonicalize().unwrap());
        match resolve_root(&nested, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => assert_eq!(found, root.canonicalize().unwrap()),
            other => panic!("expected Repo(root), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn serena_project_yml_still_marks_a_root_without_git() {
        let root = unique_temp_dir();
        write_serena_marker(&root);
        write_n_files(&root, 2);
        let nested = root.join("pkg");
        std::fs::create_dir_all(&nested).unwrap();
        for cwd in [&root, &nested] {
            match resolve_root(cwd, DEFAULT_MAX_ROOT_FILES) {
                ResolvedRoot::Repo(found) => assert_eq!(found, root.canonicalize().unwrap()),
                other => panic!("expected Repo(root) from {}, got {other:?}", cwd.display()),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn astrolabe_dir_without_project_yml_is_not_a_marker() {
        let bare = unique_temp_dir();
        std::fs::create_dir_all(bare.join(".astrolabe")).unwrap();
        assert!(find_project_root(&bare).is_none());
        let _ = std::fs::remove_dir_all(&bare);

        let root = unique_temp_dir();
        write_astrolabe_store(&root);
        write_n_files(&root, 2);
        assert!(
            find_project_root(&root).is_none(),
            "the index store creates .astrolabe/ in every root it indexes"
        );
        let nested = root.join("pkg");
        std::fs::create_dir_all(&nested).unwrap();
        match resolve_root(&nested, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Single(found) => assert_eq!(found, nested.canonicalize().unwrap()),
            other => panic!("expected Single(nested), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn is_project_dir_requires_git_or_project_file() {
        let root = unique_temp_dir();
        let make = |name: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        };
        let git_dir = make("git-dir");
        git_marker(&git_dir);
        let git_file = make("git-file");
        std::fs::write(git_file.join(".git"), "gitdir: /tmp/fake.git\n").unwrap();
        let astrolabe = make("astrolabe");
        write_astrolabe_marker(&astrolabe);
        let serena = make("serena");
        write_serena_marker(&serena);
        for dir in [&git_dir, &git_file, &astrolabe, &serena] {
            assert!(is_project_dir(dir), "{}", dir.display());
        }
        let store = make("store");
        write_astrolabe_store(&store);
        let bare_serena = make("bare-serena");
        std::fs::create_dir_all(bare_serena.join(".serena")).unwrap();
        let plain = make("plain");
        for dir in [&store, &bare_serena, &plain] {
            assert!(!is_project_dir(dir), "{}", dir.display());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn project_file_root_with_child_repos_is_multiproject() {
        // A non-git parent with a project file above several checkouts (and
        // a non-git folder of worktrees) must not be indexed whole.
        for write_marker in [write_serena_marker as fn(&Path), write_astrolabe_marker] {
            let root = unique_temp_dir();
            write_marker(&root);
            git_marker(&root.join("alpha"));
            git_marker(&root.join("beta"));
            for name in ["wt1", "wt2"] {
                let worktree = root.join("worktrees").join(name);
                std::fs::create_dir_all(&worktree).unwrap();
                std::fs::write(worktree.join(".git"), "gitdir: /tmp/fake.git\n").unwrap();
            }
            match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
                ResolvedRoot::MultiProject {
                    root: got,
                    children,
                } => {
                    assert_eq!(got, root.canonicalize().unwrap());
                    let names: Vec<_> = children
                        .iter()
                        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                        .collect();
                    assert_eq!(names, ["alpha", "beta"]);
                }
                other => panic!("expected MultiProject, got {other:?}"),
            }
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn project_file_root_with_one_child_repo_serves_the_child() {
        let root = unique_temp_dir();
        write_astrolabe_marker(&root);
        git_marker(&root.join("solo"));
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => {
                assert_eq!(found, root.join("solo").canonicalize().unwrap());
            }
            other => panic!("expected Repo(solo), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nearest_project_file_wins_without_git() {
        let outer = unique_temp_dir();
        write_astrolabe_marker(&outer);
        let inner = outer.join("nested");
        std::fs::create_dir_all(&inner).unwrap();
        write_serena_marker(&inner);
        let deep = inner.join("pkg");
        std::fs::create_dir_all(&deep).unwrap();
        match resolve_root(&deep, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => assert_eq!(found, inner.canonicalize().unwrap()),
            other => panic!("expected Repo(inner), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn same_directory_astrolabe_and_serena_is_that_root() {
        let root = unique_temp_dir();
        write_astrolabe_marker(&root);
        write_serena_marker(&root);
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        match resolve_root(&nested, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => assert_eq!(found, root.canonicalize().unwrap()),
            other => panic!("expected Repo(root), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_ancestor_project_file_is_not_adopted_as_root() {
        let ancestor = unique_temp_dir();
        write_astrolabe_marker(&ancestor);
        write_n_files(&ancestor, 6);
        let proj = ancestor.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("a.rs"), b"fn a() {}").unwrap();
        match resolve_root(&proj, 3) {
            ResolvedRoot::Single(found) => assert_eq!(found, proj.canonicalize().unwrap()),
            other => panic!("expected Single(proj), got {other:?}"),
        }
        match resolve_root(&proj, 100) {
            ResolvedRoot::Repo(found) => assert_eq!(found, ancestor.canonicalize().unwrap()),
            other => panic!("expected Repo(ancestor), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&ancestor);
    }

    #[test]
    fn rejected_ancestor_git_falls_back_to_project_file() {
        let ancestor = unique_temp_dir();
        git_marker(&ancestor);
        write_n_files(&ancestor, 6);
        let proj = ancestor.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        write_astrolabe_marker(&proj);
        match resolve_root(&proj, 3) {
            ResolvedRoot::Repo(found) => assert_eq!(found, proj.canonicalize().unwrap()),
            other => panic!("expected Repo(proj), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&ancestor);
    }

    #[test]
    fn explicit_path_does_not_walk_up_to_git() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("subproject");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = resolve_index_root(&nested).unwrap();
        assert_eq!(
            resolved,
            nested.canonicalize().unwrap(),
            "explicit path must not be replaced by an ancestor git root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_path_does_not_walk_up_to_serena() {
        let root = unique_temp_dir();
        write_serena_marker(&root);
        let nested = root.join("subproject");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = resolve_index_root(&nested).unwrap();
        assert_eq!(
            resolved,
            nested.canonicalize().unwrap(),
            "explicit path must not be replaced by an ancestor .serena/project.yml"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd_sentinel_dot_walks_up_to_git() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("pkg").join("inner");
        std::fs::create_dir_all(&nested).unwrap();

        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&nested).expect("chdir nested");
        let resolved = resolve_index_root(Path::new(".")).expect("sentinel resolve");
        std::env::set_current_dir(&prev).expect("restore cwd");

        assert_eq!(
            resolved,
            root.canonicalize().unwrap(),
            "`.` must walk up from cwd to the nearest .git"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd_sentinel_dot_walks_up_to_serena() {
        let root = unique_temp_dir();
        write_serena_marker(&root);
        let nested = root.join("src").join("pkg");
        std::fs::create_dir_all(&nested).unwrap();

        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&nested).expect("chdir nested");
        let resolved = resolve_index_root(Path::new(".")).expect("sentinel resolve");
        std::env::set_current_dir(&prev).expect("restore cwd");

        assert_eq!(
            resolved,
            root.canonicalize().unwrap(),
            "`.` must walk up from cwd to the nearest .serena/project.yml"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_root_nested_checkout_is_repo_not_multiproject() {
        // newapi/web must resolve to newapi even when the parent workspace
        // contains sibling git checkouts.
        let workspace = unique_temp_dir();
        let newapi = workspace.join("newapi");
        git_marker(&newapi);
        let web = newapi.join("web");
        std::fs::create_dir_all(&web).unwrap();
        git_marker(&workspace.join("other"));

        match resolve_root(&web, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => {
                assert_eq!(found, newapi.canonicalize().unwrap());
            }
            other => panic!("expected Repo(newapi), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn resolve_root_git_root_wins_over_child_repos() {
        let root = unique_temp_dir();
        git_marker(&root);
        git_marker(&root.join("vendor"));
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => {
                assert_eq!(found, root.canonicalize().unwrap());
            }
            other => panic!("expected Repo, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_root_multi_project_lists_children() {
        let root = unique_temp_dir();
        git_marker(&root.join("alpha"));
        git_marker(&root.join("beta"));
        std::fs::create_dir_all(root.join("plain")).unwrap();
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::MultiProject {
                root: got,
                children,
            } => {
                assert_eq!(got, root.canonicalize().unwrap());
                let names: Vec<_> = children
                    .iter()
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                assert_eq!(names, ["alpha", "beta"]);
            }
            other => panic!("expected MultiProject, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_root_hidden_dirs_are_not_children() {
        let root = unique_temp_dir();
        git_marker(&root.join("visible"));
        git_marker(&root.join("visible2"));
        git_marker(&root.join(".Trash"));
        git_marker(&root.join(".hidden"));
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::MultiProject { children, .. } => {
                let names: Vec<_> = children
                    .iter()
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                assert_eq!(names, ["visible", "visible2"]);
            }
            other => panic!("expected MultiProject, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_root_only_hidden_children_is_not_multiproject() {
        let root = unique_temp_dir();
        git_marker(&root.join(".Trash"));
        write_n_files(&root, 2);
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Single(got) => {
                assert_eq!(got, root.canonicalize().unwrap());
            }
            other => panic!("expected Single (hidden .git ignored), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_root_unmarked_small_dir_is_single() {
        let root = unique_temp_dir();
        write_n_files(&root, 3);
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Single(got) => {
                assert_eq!(got, root.canonicalize().unwrap());
            }
            other => panic!("expected Single, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_root_unmarked_over_limit_is_too_large() {
        let root = unique_temp_dir();
        write_n_files(&root, 4);
        match resolve_root(&root, 2) {
            ResolvedRoot::TooLarge {
                root: got,
                files,
                limit,
            } => {
                assert_eq!(got, root.canonicalize().unwrap());
                assert_eq!(limit, 2);
                assert!(files >= 2, "count should hit the cap, got files={files}");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_requested_skips_multi_project_guard() {
        let root = unique_temp_dir();
        git_marker(&root.join("a"));
        git_marker(&root.join("b"));
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::MultiProject { .. } => {}
            other => panic!("fixture should be MultiProject, got {other:?}"),
        }
        let resolved = resolve_index_root(&root).expect("explicit path");
        assert_eq!(
            resolved,
            root.canonicalize().unwrap(),
            "explicit requested root must not refuse or rewrite a multi-project dir"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuse_guidance_lists_children_and_caps_at_twenty() {
        let root = PathBuf::from("/tmp/workspace");
        let children: Vec<PathBuf> = (0..21)
            .map(|i| PathBuf::from(format!("/tmp/workspace/r{i}")))
            .collect();
        let msg = format_multi_project_refuse(&root, &children);
        assert!(msg.contains("found 21 nested git repositories"));
        assert!(msg.contains("/tmp/workspace/r0"));
        assert!(msg.contains("/tmp/workspace/r19"));
        assert!(!msg.contains("/tmp/workspace/r20"));
        assert!(msg.contains("…and 1 more"));
        assert!(msg.contains("cd into a sub-repo, or set ASTROLABE_ROOT to force a specific root"));
    }

    #[test]
    fn resolve_root_single_child_repo_is_served_directly() {
        let root = unique_temp_dir();
        git_marker(&root.join("solo"));
        std::fs::write(root.join("notes.txt"), b"x").unwrap();
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::Repo(found) => {
                assert_eq!(found, root.join("solo").canonicalize().unwrap());
            }
            other => panic!("expected Repo(solo), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_with_exactly_limit_files_is_accepted() {
        let root = unique_temp_dir();
        write_n_files(&root, 10);
        assert_eq!(count_root_files(&root, 10), (10, false));
        match resolve_root(&root, 10) {
            ResolvedRoot::Single(_) => {}
            other => panic!("exactly `limit` files must be accepted, got {other:?}"),
        }
        write_n_files(&root, 11);
        assert!(
            count_root_files(&root, 10).1,
            "limit + 1 files is too large"
        );
        match resolve_root(&root, 10) {
            ResolvedRoot::TooLarge { .. } => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
        let msg = resolve_root(&root, 10).refuse_guidance().unwrap();
        assert!(msg.contains("more than 10 files"), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_ancestor_repo_is_not_adopted_as_root() {
        let ancestor = unique_temp_dir();
        git_marker(&ancestor);
        write_n_files(&ancestor, 6);
        let proj = ancestor.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("a.rs"), b"fn a() {}").unwrap();
        // Ancestor holds more than the limit: cwd is classified on its own.
        match resolve_root(&proj, 3) {
            ResolvedRoot::Single(found) => assert_eq!(found, proj.canonicalize().unwrap()),
            other => panic!("expected Single(proj), got {other:?}"),
        }
        // Within the limit the ancestor repo is still the root.
        match resolve_root(&proj, 100) {
            ResolvedRoot::Repo(found) => assert_eq!(found, ancestor.canonicalize().unwrap()),
            other => panic!("expected Repo(ancestor), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&ancestor);
    }

    #[test]
    fn resolve_call_index_root_accepts_abs_and_cwd_relative() {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let abs = resolve_call_index_root(root.to_str().unwrap()).unwrap();
        assert_eq!(abs, root.canonicalize().unwrap());

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let rel = resolve_call_index_root("nested").unwrap();
        std::env::set_current_dir(&prev).unwrap();
        assert_eq!(rel, root.join("nested").canonicalize().unwrap());

        let err = resolve_call_index_root("/no/such/astrolabe/index_root_xyz").unwrap_err();
        assert!(err.contains("not an existing path"), "{err}");
        let empty = resolve_call_index_root("   ").unwrap_err();
        assert!(empty.contains("empty"), "{empty}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
