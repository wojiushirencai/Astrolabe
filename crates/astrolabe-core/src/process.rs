//! Cross-platform process group / Job Object lifecycle and command resolution.
//!
//! Centralizes the OS primitives needed when spawning tool or language-server
//! subprocesses:
//! - [`configure_process_group`] puts Unix children in their own process group
//!   so wrappers (Node/JVM/dotnet) cannot leave orphaned grandchildren behind.
//! - [`ChildProcessTree`] tracks either the Unix process group or a Windows
//!   kill-on-close Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) and kills
//!   the whole tree on [`ChildProcessTree::terminate`] (or `Drop` on Windows).
//! - [`resolve_command`] / [`resolve_command_in`] locate executables across
//!   `PATH` with Windows `PATHEXT` (`.COM;.EXE;.BAT;.CMD`) probing.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Configure `cmd` so the spawned child starts in its own process group.
///
/// On Unix this calls `CommandExt::process_group(0)` so [`ChildProcessTree`]
/// can signal the entire process tree via `-pgid`. On non-Unix platforms this
/// is a no-op because process trees are tracked post-spawn via Job Objects.
pub fn configure_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Handle to a spawned child's process tree for whole-tree termination.
pub struct ChildProcessTree {
    #[cfg(unix)]
    pgid: u32,
    #[cfg(windows)]
    job: Option<win::JobHandle>,
}

#[cfg(windows)]
mod win {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub(super) struct JobHandle(HANDLE);

    // SAFETY: A Win32 kernel Job Object handle is process-wide and safe to send
    // across or share between threads; mutating operations require `&mut self`.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid non-null handle owned exclusively by this wrapper.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) fn attach_job(child: &Child) -> Option<JobHandle> {
        // SAFETY: Standard Win32 Job Object setup; checked for null / 0 on every step
        // and `JobHandle::drop` closes the handle if any subsequent step fails.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let job = JobHandle(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 {
                return None;
            }
            // If the child already exited before assignment, `AssignProcessToJobObject`
            // fails harmlessly; return `None` without panicking.
            if AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) == 0 {
                return None;
            }
            Some(job)
        }
    }

    pub(super) fn terminate_job(job: &JobHandle) {
        // SAFETY: `job.0` is a valid owned Job Object handle.
        unsafe {
            TerminateJobObject(job.0, 1);
        }
    }
}

impl ChildProcessTree {
    /// Attach to a freshly spawned `child` process.
    ///
    /// - Unix: records `pgid = child.id()`. Call [`configure_process_group`]
    ///   on the `Command` before spawning so `pgid` owns the child's tree.
    /// - Windows: creates a `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job Object and
    ///   assigns the child to it (ignoring errors if the child exited early).
    pub fn attach(child: &Child) -> Self {
        #[cfg(unix)]
        {
            Self { pgid: child.id() }
        }
        #[cfg(windows)]
        {
            Self {
                job: win::attach_job(child),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Self {}
        }
    }

    /// Forcefully terminate every process in the tracked tree.
    pub fn terminate(&mut self) {
        #[cfg(unix)]
        {
            let pgid = self.pgid;
            if pgid > 0 && pgid <= libc::pid_t::MAX as u32 {
                // SAFETY: sending SIGKILL to a positive pgid negated targets that process group.
                unsafe {
                    libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
                }
                self.pgid = 0;
            }
        }
        #[cfg(windows)]
        {
            if let Some(job) = self.job.take() {
                win::terminate_job(&job);
            }
        }
    }
}

/// Resolve a command name or path using the current process's `PATH` and `PATHEXT`.
pub fn resolve_command(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH");
    let pathext = std::env::var("PATHEXT").ok();
    resolve_command_in(name, path_var.as_deref(), pathext.as_deref(), cfg!(windows))
}

/// Pure, testable command resolver parameterized by `PATH`, `PATHEXT`, and OS mode.
///
/// - When `name` contains a path separator, checks existence directly (and on
///   Windows without an extension, also probes `PATHEXT`, defaulting to
///   `.COM;.EXE;.BAT;.CMD`).
/// - Otherwise searches directories in `path_var`, checking executable bits on
///   Unix or `PATHEXT` (case-insensitive) on Windows, returning the resolved
///   path including its matched extension.
pub fn resolve_command_in(
    name: &str,
    path_var: Option<&OsStr>,
    pathext: Option<&str>,
    is_windows: bool,
) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }

    let exts = parse_pathext(pathext);
    let has_sep = name.contains('/') || (is_windows && name.contains('\\'));
    if has_sep {
        let p = Path::new(name);
        if !is_windows {
            return is_runnable(p, false).then(|| p.to_path_buf());
        }
        return resolve_explicit_windows(p, &exts);
    }

    let path_var = path_var?;
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if !is_windows {
            let candidate = dir.join(name);
            if is_runnable(&candidate, false) {
                return Some(candidate);
            }
        } else if let Some(found) = resolve_in_dir_windows(&dir, name, &exts) {
            return Some(found);
        }
    }
    None
}

fn parse_pathext(pathext: Option<&str>) -> Vec<String> {
    let raw = pathext
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PATHEXT);
    raw.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|ext| {
            if ext.starts_with('.') {
                ext.to_string()
            } else {
                format!(".{ext}")
            }
        })
        .collect()
}

fn has_pathext_suffix(name: &str, exts: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    exts.iter()
        .any(|ext| lower.ends_with(&ext.to_ascii_lowercase()))
}

fn resolve_explicit_windows(path: &Path, exts: &[String]) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_string_lossy();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if has_pathext_suffix(&file_name, exts) {
        return find_file_in_dir_ci(parent, &file_name, path);
    }

    if path.extension().is_none() {
        for ext in exts {
            let with_ext_name = format!("{file_name}{ext}");
            let with_ext_path = parent.join(&with_ext_name);
            if let Some(found) = find_file_in_dir_ci(parent, &with_ext_name, &with_ext_path) {
                return Some(found);
            }
        }
        return None;
    }

    find_file_in_dir_ci(parent, &file_name, path)
}

fn resolve_in_dir_windows(dir: &Path, name: &str, exts: &[String]) -> Option<PathBuf> {
    if has_pathext_suffix(name, exts) {
        let candidate = dir.join(name);
        return find_file_in_dir_ci(dir, name, &candidate);
    }

    for ext in exts {
        let with_ext = format!("{name}{ext}");
        let candidate = dir.join(&with_ext);
        if let Some(found) = find_file_in_dir_ci(dir, &with_ext, &candidate) {
            return Some(found);
        }
    }

    // Also allow explicit non-PATHEXT extension (e.g. `tool.py`), while never
    // matching extensionless Unix shell shims (such as `node_modules/.bin/tsc`).
    if Path::new(name).extension().is_some() {
        let candidate = dir.join(name);
        return find_file_in_dir_ci(dir, name, &candidate);
    }

    None
}

fn find_file_in_dir_ci(dir: &Path, target_name: &str, direct: &Path) -> Option<PathBuf> {
    if is_runnable(direct, true) {
        return Some(direct.to_path_buf());
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        if entry_name
            .to_string_lossy()
            .eq_ignore_ascii_case(target_name)
        {
            let candidate = entry.path();
            if is_runnable(&candidate, true) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_runnable(path: &Path, is_windows: bool) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    if is_windows {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: fn() = || {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ChildProcessTree>();
    };

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    #[test]
    fn resolve_command_in_windows_mode_probes_pathext_case_insensitively() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");

        // Mirror `node_modules/.bin` on Windows: extensionless POSIX shell
        // shim alongside `.cmd` wrapper.
        std::fs::write(bin.join("tsc"), "#!/bin/sh\n").expect("write tsc");
        #[cfg(unix)]
        make_executable(&bin.join("tsc"));
        std::fs::write(bin.join("tsc.cmd"), "@echo off\r\n").expect("write tsc.cmd");
        std::fs::write(bin.join("npx.CMD"), "@echo off\r\n").expect("write npx.CMD");
        std::fs::write(bin.join("tool.exe"), "mz").expect("write tool.exe");

        let path_var = std::env::join_paths([&bin]).expect("join paths");

        let tsc = resolve_command_in("tsc", Some(&path_var), None, true).expect("resolve tsc");
        assert_eq!(tsc.parent(), Some(bin.as_path()));
        assert!(
            tsc.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("tsc.cmd"),
            "expected tsc.cmd, got {tsc:?}"
        );

        let npx = resolve_command_in("npx", Some(&path_var), Some(".exe;.cmd"), true)
            .expect("resolve npx");
        assert_eq!(npx.parent(), Some(bin.as_path()));
        assert!(
            npx.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("npx.cmd"),
            "expected npx.cmd, got {npx:?}"
        );

        let tool =
            resolve_command_in("tool.EXE", Some(&path_var), None, true).expect("resolve tool.EXE");
        assert_eq!(tool.parent(), Some(bin.as_path()));
        assert!(
            tool.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("tool.exe"),
            "expected tool.exe, got {tool:?}"
        );

        // Explicit path with separator probes PATHEXT when extensionless.
        let explicit = bin.join("tsc");
        let resolved_explicit = resolve_command_in(explicit.to_str().unwrap(), None, None, true)
            .expect("resolve explicit");
        assert_eq!(resolved_explicit.parent(), Some(bin.as_path()));
        assert!(
            resolved_explicit
                .file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("tsc.cmd"),
            "expected tsc.cmd, got {resolved_explicit:?}"
        );

        assert!(resolve_command_in("missing", Some(&path_var), None, true).is_none());
        assert!(resolve_command_in("", Some(&path_var), None, true).is_none());
    }

    #[test]
    fn resolve_command_in_windows_mode_ignores_extensionless_shims() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin_both = tmp.path().join("both");
        std::fs::create_dir_all(&bin_both).expect("mkdir both");
        std::fs::write(bin_both.join("tsls"), "#!/bin/sh\n").expect("write tsls");
        #[cfg(unix)]
        make_executable(&bin_both.join("tsls"));
        std::fs::write(bin_both.join("tsls.cmd"), "@echo off\r\n").expect("write tsls.cmd");

        let path_both = std::env::join_paths([&bin_both]).expect("join both");
        let by_path = resolve_command_in("tsls", Some(&path_both), None, true)
            .expect("PATH lookup should find tsls.cmd");
        assert_eq!(by_path.parent(), Some(bin_both.as_path()));
        assert!(
            by_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("tsls.cmd"),
            "expected tsls.cmd, got {by_path:?}"
        );

        let explicit_both = bin_both.join("tsls");
        let by_explicit = resolve_command_in(explicit_both.to_str().unwrap(), None, None, true)
            .expect("explicit lookup should find tsls.cmd");
        assert_eq!(by_explicit.parent(), Some(bin_both.as_path()));
        assert!(
            by_explicit
                .file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("tsls.cmd"),
            "expected tsls.cmd, got {by_explicit:?}"
        );

        let bin_only = tmp.path().join("only_shim");
        std::fs::create_dir_all(&bin_only).expect("mkdir only_shim");
        std::fs::write(bin_only.join("tsls"), "#!/bin/sh\n").expect("write tsls shim");
        #[cfg(unix)]
        make_executable(&bin_only.join("tsls"));

        let path_only = std::env::join_paths([&bin_only]).expect("join only_shim");
        assert!(resolve_command_in("tsls", Some(&path_only), None, true).is_none());
        let explicit_only = bin_only.join("tsls");
        assert!(resolve_command_in(explicit_only.to_str().unwrap(), None, None, true).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_command_in_unix_mode_requires_executable_bit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");

        let non_exec = bin.join("notexec");
        std::fs::write(&non_exec, "#!/bin/sh\n").expect("write non_exec");

        let exec = bin.join("runme");
        std::fs::write(&exec, "#!/bin/sh\n").expect("write exec");
        make_executable(&exec);

        let path_var = std::env::join_paths([&bin]).expect("join paths");
        assert!(resolve_command_in("notexec", Some(&path_var), None, false).is_none());
        assert_eq!(
            resolve_command_in("runme", Some(&path_var), None, false),
            Some(exec.clone())
        );
        assert_eq!(
            resolve_command_in(exec.to_str().unwrap(), None, None, false),
            Some(exec)
        );
        assert!(resolve_command_in(non_exec.to_str().unwrap(), None, None, false).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn child_process_tree_terminates_process_group_including_grandchildren() {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        use std::time::{Duration, Instant};

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo $!; wait")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_process_group(&mut cmd);

        let mut child = cmd.spawn().expect("spawn sh");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read grandchild pid");
        let grandchild_pid: libc::pid_t = line.trim().parse().expect("parse grandchild pid");

        let mut tree = ChildProcessTree::attach(&child);
        tree.terminate();
        let _ = child.wait();

        // Wait briefly for init/launchd to reap the killed grandchild.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let rc = unsafe { libc::kill(grandchild_pid, 0) };
            if rc == -1 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ESRCH) {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "grandchild process {grandchild_pid} survived process-group termination"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
