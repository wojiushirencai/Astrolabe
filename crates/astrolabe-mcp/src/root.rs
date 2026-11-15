//! Resolve the directory Astrolabe actually indexes.
//!
//! Serena's Claude Code setup does **not** index `args: ["."]` as a literal
//! relative path. It walks up from the MCP process cwd looking for a project
//! boundary, then stores the **absolute** path. A nested worktree must win
//! over an ancestor so a checkout inside another repo is not hijacked.
//!
//! Markers (nearest wins, same single-pass rule as Serena's
//! `find_project_root`):
//!
//! * `.git` — a directory, or a worktree/submodule pointer file
//! * `.serena/project.yml` — an explicit Serena project (a bare `.serena/`
//!   directory is **not** enough; Serena requires the YAML file)
//!
//! Explicit paths (`astrolabe /path/to/repo`, `ASTROLABE_ROOT=/path`) are
//! not walked up after canonicalization — that is Serena's `--project` flag.
//! The cwd sentinel `.` (and "no argument") is Serena's `--project-from-cwd`.
//!
//! Astrolabe only indexes git projects. A path is *inside a git project*
//! ([`is_in_git_project`]) when [`find_project_root`] finds a marker at or
//! above it and that marker is not the home directory (a dotfiles `~/.git`
//! does not make every folder under `$HOME` a project).
//!
//! When no ancestor marker is adopted, [`resolve_root`] classifies the spawn
//! directory instead of indexing it blindly (that fallback once indexed a
//! 350k-file parent of many git checkouts, and later a non-git folder of 25
//! worktrees at 6 GB):
//!
//! 1. Walk **up** first — a nested checkout such as `newapi/web` must still
//!    resolve to `newapi`. Downward probes must not change that.
//! 2. Look **down** one level for direct children that contain `.git`
//!    (hidden names such as `.Trash` are skipped). Exactly one hit is served
//!    directly as `Repo`; two or more are `MultiProject`.
//! 3. Count files (capped): over the limit is `TooLarge`. Within it, a
//!    directory outside any git project is `NotGit`; a subdirectory of a git
//!    project whose oversized root was skipped is `Single`.
//!
//! The MCP binary starts `MultiProject` in dispatcher mode (lazy per-child
//! indexing, see `session.rs` / `roots.rs`; the non-git parent itself is
//! never indexed) and refuses `TooLarge` and `NotGit` on the cwd sentinel
//! (`exit 2`). An explicit root ([`resolve_explicit_root`]) skips the size
//! gate and is served as given inside a git project; outside one it is
//! replaced by its child repos the same way, or refused when it has none.
//! A per-call `index_root` outside a git project is refused with its child
//! repos listed ([`non_git_index_root_message`]).
//!
//! `ASTROLABE_ALLOW_NON_GIT_ROOT=1` restores indexing non-git directories
//! as given ([`resolve_startup_root`]).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use astrolabe_core::scan::{count_files, ScanOptions};

/// Default file-count cap for an unmarked (non-git) root.
///
/// Overridden by `ASTROLABE_MAX_ROOT_FILES` in the binary.
pub const DEFAULT_MAX_ROOT_FILES: u64 = 20_000;

/// Outcome of [`resolve_root`] / [`resolve_explicit_root`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedRoot {
    /// A project root: the nearest `.git` / `.serena/project.yml` at or
    /// above the cwd, an explicit root that carries one, or the only direct
    /// child repo of a non-git directory.
    Repo(PathBuf),
    /// Indexed as given without being a project root: a subdirectory of a
    /// git project whose root was not adopted (explicit path, or an ancestor
    /// over `limit`), or any directory under `ASTROLABE_ALLOW_NON_GIT_ROOT`.
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
    /// Outside any git project and without child git repos (for the cwd:
    /// within `limit`). Refused unless `ASTROLABE_ALLOW_NON_GIT_ROOT` is set.
    NotGit(PathBuf),
}

/// Walk `start` and its parents. The nearest directory that contains `.git`
/// or `.serena/project.yml` wins.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if is_project_marker(&dir) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Serena: `.serena/project.yml` is the project file; `.git` may be a dir or
/// a gitdir pointer. Same-directory both-present is the same root.
fn is_project_marker(dir: &Path) -> bool {
    dir.join(".git").exists() || dir.join(".serena").join("project.yml").is_file()
}

/// True when the launch request means "detect from process cwd".
pub fn is_cwd_sentinel(path: &Path) -> bool {
    path == Path::new(".") || path == Path::new("./")
}

/// Classify `cwd` for the index-root guardrail.
///
/// Upward detection is first and final: a nested repo is never reclassified
/// because a parent directory happens to contain sibling checkouts.
///
/// A marker found in an *ancestor* of `cwd` is size-checked like an unmarked
/// directory: a stray `~/.git` (dotfiles) or a repo above the limit must not
/// silently make the whole ancestor the index root. Such an ancestor is
/// skipped (with a warning) and `cwd` is classified instead; set
/// `ASTROLABE_ROOT` or raise `ASTROLABE_MAX_ROOT_FILES` to force it.
pub fn resolve_root(cwd: &Path, limit: u64) -> ResolvedRoot {
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if let Some(found) = find_project_root(cwd) {
        if found == cwd_canon {
            return ResolvedRoot::Repo(found);
        }
        match ancestor_rejection(&found, limit) {
            None => return ResolvedRoot::Repo(found),
            Some(reason) => tracing::warn!(
                ancestor = %found.display(),
                cwd = %cwd_canon.display(),
                "ignoring project marker in ancestor directory ({reason}); \
                 set ASTROLABE_ROOT or ASTROLABE_MAX_ROOT_FILES to use it"
            ),
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
    let (files, truncated) = count_root_files(&root, limit);
    if truncated {
        return ResolvedRoot::TooLarge { root, files, limit };
    }
    if is_in_git_project(&root) {
        ResolvedRoot::Single(root)
    } else {
        ResolvedRoot::NotGit(root)
    }
}

/// Why an ancestor project root must not be adopted, if any.
fn ancestor_rejection(found: &Path, limit: u64) -> Option<String> {
    if home_dirs().iter().any(|home| home == found) {
        return Some("it is the home directory".to_string());
    }
    let (_, truncated) = count_root_files(found, limit);
    truncated.then(|| format!("it holds more than {limit} files"))
}

/// Canonical `$HOME` / `%USERPROFILE%`.
fn home_dirs() -> Vec<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .filter_map(|home| Path::new(&home).canonicalize().ok())
        .collect()
}

/// True when `path` lies inside a git project: the nearest `.git` /
/// `.serena/project.yml` at or above it exists and is not the home
/// directory. Only such paths are indexed (unless
/// `ASTROLABE_ALLOW_NON_GIT_ROOT` is set).
pub fn is_in_git_project(path: &Path) -> bool {
    in_git_project(path, &home_dirs())
}

fn in_git_project(path: &Path, homes: &[PathBuf]) -> bool {
    find_project_root(path).is_some_and(|found| !homes.contains(&found))
}

/// Classify an explicit root (`astrolabe /path`, `ASTROLABE_ROOT`) that
/// [`resolve_index_root`] already canonicalized. Inside a git project it is
/// served as given — `Repo` when it carries the marker, else `Single` — and
/// no size gate applies. Outside one, its direct child repos are served the
/// way [`resolve_root`] serves them, and a directory without any is `NotGit`.
pub fn resolve_explicit_root(path: &Path) -> ResolvedRoot {
    let root = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if is_in_git_project(&root) {
        return if is_project_marker(&root) {
            ResolvedRoot::Repo(root)
        } else {
            ResolvedRoot::Single(root)
        };
    }
    let mut children = child_git_repos(&root);
    match children.len() {
        0 => ResolvedRoot::NotGit(root),
        1 => ResolvedRoot::Repo(children.remove(0)),
        _ => ResolvedRoot::MultiProject { root, children },
    }
}

/// Startup root of the binary: the cwd sentinel goes through
/// [`resolve_root`], any other path through [`resolve_explicit_root`].
///
/// `allow_non_git` (`ASTROLABE_ALLOW_NON_GIT_ROOT`) restores the behavior
/// before the git-only rule: a `NotGit` cwd is served as `Single`, and an
/// explicit path is always served as given, nested repos or not.
pub fn resolve_startup_root(
    requested: &Path,
    limit: u64,
    allow_non_git: bool,
) -> anyhow::Result<ResolvedRoot> {
    if is_cwd_sentinel(requested) {
        let cwd = std::env::current_dir().context("cannot read current directory")?;
        return Ok(startup_from_cwd(&cwd, limit, allow_non_git));
    }
    let path = resolve_index_root(requested)?;
    Ok(startup_from_explicit(path, allow_non_git))
}

fn startup_from_cwd(cwd: &Path, limit: u64, allow_non_git: bool) -> ResolvedRoot {
    match resolve_root(cwd, limit) {
        ResolvedRoot::NotGit(root) if allow_non_git => ResolvedRoot::Single(root),
        other => other,
    }
}

fn startup_from_explicit(path: PathBuf, allow_non_git: bool) -> ResolvedRoot {
    let resolved = if allow_non_git {
        if is_project_marker(&path) {
            ResolvedRoot::Repo(path)
        } else {
            ResolvedRoot::Single(path)
        }
    } else {
        resolve_explicit_root(&path)
    };
    if let ResolvedRoot::Repo(root) | ResolvedRoot::Single(root) = &resolved {
        let nested = child_git_repos(root);
        if !nested.is_empty() {
            tracing::warn!(
                root = %root.display(),
                nested_repos = nested.len(),
                "explicit root contains nested git repositories; indexing anyway \
                 (cd into a sub-repo, or set ASTROLABE_ROOT to force a specific root)"
            );
        }
    }
    resolved
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
        if path.join(".git").exists() {
            children.push(path.canonicalize().unwrap_or(path));
        }
    }
    children.sort();
    children
}

impl ResolvedRoot {
    /// Actionable refuse text for `MultiProject` / `TooLarge` / `NotGit`.
    /// `None` means the binary should start.
    pub fn refuse_guidance(&self) -> Option<String> {
        match self {
            Self::MultiProject { root, children } => {
                Some(format_multi_project_refuse(root, children))
            }
            Self::NotGit(root) => Some(format!(
                "Astrolabe refused to index {}: it is not inside a git repository \
                 (Astrolabe only indexes git repositories).\n\
                 cd into a repository (or pass its path), or run `git init` there; \
                 set ASTROLABE_ALLOW_NON_GIT_ROOT=1 to index it anyway",
                root.display()
            )),
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
    write_capped_paths(&mut out, children);
    let _ = write!(
        out,
        "cd into a sub-repo, or set ASTROLABE_ROOT to force a specific root"
    );
    out
}

/// One indented line per path, at most 20, then an "…and N more" line.
fn write_capped_paths(out: &mut String, paths: &[PathBuf]) {
    const SHOWN: usize = 20;
    for path in paths.iter().take(SHOWN) {
        let _ = writeln!(out, "  {}", path.display());
    }
    if paths.len() > SHOWN {
        let _ = writeln!(out, "  …and {} more", paths.len() - SHOWN);
    }
}

/// Tool error for a per-call `index_root` outside any git project. Nothing
/// is indexed for it; the direct child repos are listed as the paths to
/// pass instead.
pub fn non_git_index_root_message(root: &Path) -> String {
    let children = child_git_repos(root);
    let mut out = format!(
        "index_root `{}` is not inside a git repository; Astrolabe only indexes git repositories, so nothing was indexed.\n",
        root.display()
    );
    match children.first() {
        None => out.push_str(
            "It has no direct child git repositories: pass a git repository (or a directory inside one) as index_root.\n",
        ),
        Some(first) => {
            let _ = writeln!(
                out,
                "Pass one of its {} direct child git repositories as index_root instead:",
                children.len()
            );
            write_capped_paths(&mut out, &children);
            let _ = writeln!(out, "e.g. index_root=\"{}\"", first.display());
        }
    }
    out.push_str("confidence: unknown");
    out
}

/// Turn a user/MCP-supplied path into the directory that will be indexed.
///
/// * `.` / `./` — detect from cwd (walk up for `.git` or `.serena/project.yml`);
///   if none, canonicalize cwd. Size / multi-project refuse lives in
///   [`resolve_root`], applied by the binary for this sentinel.
/// * any other path — canonicalize that directory and do **not** walk up.
///   The size gate never applies (explicit user intent); the git-only rule
///   is [`resolve_explicit_root`].
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
            return Ok(found);
        }
        let cwd = start.canonicalize().unwrap_or(start);
        tracing::warn!(
            cwd = %cwd.display(),
            "no .git or .serena/project.yml ancestor; indexing cwd as-is \
             (Serena would leave the project inactive)"
        );
        return Ok(cwd);
    }

    Ok(start.canonicalize().unwrap_or(start))
}

/// Resolve a per-call MCP `index_root` argument.
///
/// Absolute paths are used as-is; relative paths join the process cwd at call
/// time. The result is canonicalized and must be an existing directory. Unlike
/// startup [`resolve_index_root`], this never walks up to `.git` /
/// `.serena/project.yml` and never applies MultiProject / TooLarge refuses;
/// the server refuses a result outside any git project
/// ([`is_in_git_project`], [`non_git_index_root_message`]).
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
    fn nearest_serena_wins_over_ancestor_git() {
        let outer = unique_temp_dir();
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        let inner = outer.join("serena-project");
        std::fs::create_dir_all(&inner).unwrap();
        write_serena_marker(&inner);
        let found = find_project_root(&inner).expect("inner serena");
        assert_eq!(
            found,
            inner.canonicalize().unwrap(),
            "nearest .serena/project.yml must not be hijacked by an ancestor .git"
        );
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
            ResolvedRoot::NotGit(got) => {
                assert_eq!(got, root.canonicalize().unwrap());
            }
            other => panic!("expected NotGit (hidden .git ignored), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_root_unmarked_small_dir_is_not_git() {
        let root = unique_temp_dir();
        write_n_files(&root, 3);
        match resolve_root(&root, DEFAULT_MAX_ROOT_FILES) {
            ResolvedRoot::NotGit(got) => {
                assert_eq!(got, root.canonicalize().unwrap());
            }
            other => panic!("expected NotGit, got {other:?}"),
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
            ResolvedRoot::NotGit(_) => {}
            other => panic!("exactly `limit` files must pass the size gate, got {other:?}"),
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

    #[test]
    fn in_git_project_follows_markers_but_not_home() {
        let base = unique_temp_dir();
        let repo = base.join("repo");
        git_marker(&repo);
        let deep = repo.join("src").join("pkg");
        std::fs::create_dir_all(&deep).unwrap();
        let worktree = base.join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /tmp/fake.git\n").unwrap();
        let serena = base.join("serena");
        std::fs::create_dir_all(&serena).unwrap();
        write_serena_marker(&serena);
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let no_homes: &[PathBuf] = &[];
        for inside in [&repo, &deep, &worktree, &serena] {
            assert!(in_git_project(inside, no_homes), "{}", inside.display());
        }
        assert!(!in_git_project(&plain, no_homes));
        assert!(!in_git_project(&base, no_homes));

        // A dotfiles marker at $HOME does not cover the folders below it, but
        // a real repo under $HOME still counts.
        let home = base.join("home");
        git_marker(&home);
        let under_home = home.join("notes");
        std::fs::create_dir_all(&under_home).unwrap();
        let home_repo = home.join("proj");
        git_marker(&home_repo);
        let homes = [home.canonicalize().unwrap()];
        assert!(!in_git_project(&home, &homes));
        assert!(!in_git_project(&under_home, &homes));
        assert!(in_git_project(&home_repo, &homes));
        assert!(in_git_project(&under_home, no_homes));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn explicit_root_inside_git_project_is_served_as_given() {
        let root = unique_temp_dir();
        git_marker(&root);
        git_marker(&root.join("vendor").join("dep"));
        let sub = root.join("crates");
        git_marker(&sub.join("a"));
        git_marker(&sub.join("b"));
        assert_eq!(
            resolve_explicit_root(&root),
            ResolvedRoot::Repo(root.canonicalize().unwrap())
        );
        // Child repos under a git subdirectory do not turn it into a dispatcher.
        assert_eq!(
            resolve_explicit_root(&sub),
            ResolvedRoot::Single(sub.canonicalize().unwrap())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_non_git_root_serves_child_repos_or_is_refused() {
        let root = unique_temp_dir();
        write_n_files(&root, 3);
        let canon = root.canonicalize().unwrap();
        assert_eq!(
            resolve_explicit_root(&root),
            ResolvedRoot::NotGit(canon.clone())
        );

        let solo = root.join("solo");
        git_marker(&solo);
        assert_eq!(
            resolve_explicit_root(&root),
            ResolvedRoot::Repo(solo.canonicalize().unwrap())
        );

        // A worktree's `.git` pointer file counts as a child repo.
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /tmp/fake.git\n").unwrap();
        match resolve_explicit_root(&root) {
            ResolvedRoot::MultiProject {
                root: got,
                children,
            } => {
                assert_eq!(got, canon);
                let names: Vec<_> = children
                    .iter()
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                assert_eq!(names, ["solo", "wt"]);
            }
            other => panic!("expected MultiProject, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_root_refuses_non_git_unless_allowed() {
        let root = unique_temp_dir();
        write_n_files(&root, 3);
        let canon = root.canonicalize().unwrap();
        assert_eq!(
            startup_from_cwd(&root, DEFAULT_MAX_ROOT_FILES, false),
            ResolvedRoot::NotGit(canon.clone())
        );
        assert_eq!(
            startup_from_cwd(&root, DEFAULT_MAX_ROOT_FILES, true),
            ResolvedRoot::Single(canon.clone())
        );
        // The escape hatch does not lift the size gate.
        assert!(matches!(
            startup_from_cwd(&root, 2, true),
            ResolvedRoot::TooLarge { .. }
        ));
        assert_eq!(
            startup_from_explicit(canon.clone(), false),
            ResolvedRoot::NotGit(canon.clone())
        );
        assert_eq!(
            startup_from_explicit(canon.clone(), true),
            ResolvedRoot::Single(canon.clone())
        );

        git_marker(&root.join("a"));
        git_marker(&root.join("b"));
        assert!(matches!(
            startup_from_explicit(canon.clone(), false),
            ResolvedRoot::MultiProject { .. }
        ));
        // Legacy explicit behavior: the given directory, nested repos and all.
        assert_eq!(
            startup_from_explicit(canon.clone(), true),
            ResolvedRoot::Single(canon.clone())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_root_keeps_git_roots() {
        let root = unique_temp_dir();
        git_marker(&root);
        let canon = root.canonicalize().unwrap();
        for allow in [false, true] {
            assert_eq!(
                startup_from_cwd(&root, DEFAULT_MAX_ROOT_FILES, allow),
                ResolvedRoot::Repo(canon.clone())
            );
            assert_eq!(
                startup_from_explicit(canon.clone(), allow),
                ResolvedRoot::Repo(canon.clone())
            );
        }
        assert_eq!(
            resolve_startup_root(&root, DEFAULT_MAX_ROOT_FILES, false).unwrap(),
            ResolvedRoot::Repo(canon.clone())
        );
        assert!(
            resolve_startup_root(&root.join("missing"), DEFAULT_MAX_ROOT_FILES, false).is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn not_git_refuse_guidance_names_the_escape_hatch() {
        let msg = ResolvedRoot::NotGit(PathBuf::from("/tmp/plain"))
            .refuse_guidance()
            .unwrap();
        assert!(msg.contains("/tmp/plain"), "{msg}");
        assert!(msg.contains("not inside a git repository"), "{msg}");
        assert!(msg.contains("git init"), "{msg}");
        assert!(msg.contains("ASTROLABE_ALLOW_NON_GIT_ROOT=1"), "{msg}");
    }

    #[test]
    fn non_git_index_root_message_lists_child_repos() {
        let root = unique_temp_dir();
        let bare = non_git_index_root_message(&root);
        assert!(bare.contains("not inside a git repository"), "{bare}");
        assert!(bare.contains("no direct child git repositories"), "{bare}");
        assert!(bare.ends_with("confidence: unknown"), "{bare}");

        for i in 0..21 {
            git_marker(&root.join(format!("r{i:02}")));
        }
        let msg = non_git_index_root_message(&root);
        let canon = root.canonicalize().unwrap();
        let shown = |name: &str| canon.join(name).display().to_string();
        assert!(
            msg.contains("Pass one of its 21 direct child git repositories"),
            "{msg}"
        );
        assert!(msg.contains(&shown("r00")), "{msg}");
        assert!(msg.contains(&shown("r19")), "{msg}");
        assert!(!msg.contains(&shown("r20")), "{msg}");
        assert!(msg.contains("…and 1 more"), "{msg}");
        assert!(
            msg.contains(&format!("e.g. index_root=\"{}\"", shown("r00"))),
            "{msg}"
        );
        assert!(msg.ends_with("confidence: unknown"), "{msg}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
