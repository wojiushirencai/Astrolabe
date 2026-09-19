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
//! Explicit paths (`astrolabe /path/to/repo`, `ASTROLABE_ROOT=/path`) stay
//! as given after canonicalization — that is Serena's `--project` flag.
//! The cwd sentinel `.` (and "no argument") is Serena's `--project-from-cwd`.
//!
//! When no marker is found Serena leaves the project inactive (`None`).
//! Astrolabe still indexes canonicalize(cwd) and warns: an MCP server that
//! refuses to start is worse than indexing the spawn directory.

use std::path::{Path, PathBuf};

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

fn is_cwd_sentinel(path: &Path) -> bool {
    path == Path::new(".") || path == Path::new("./")
}

/// Turn a user/MCP-supplied path into the directory that will be indexed.
///
/// * `.` / `./` — detect from cwd (walk up for `.git` or `.serena/project.yml`);
///   if none, canonicalize cwd (unlike Serena, which would stay inactive).
/// * any other path — canonicalize that directory, do **not** walk up.
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
}
