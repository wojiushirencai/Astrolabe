//! Roslyn (Microsoft.CodeAnalysis.LanguageServer) transport helpers.
//!
//! Launch shape is always `dotnet <Microsoft.CodeAnalysis.LanguageServer.dll>
//! --stdio`. After `initialize` / `initialized`, the client must open a
//! solution or projects with the Microsoft-specific notifications
//! `solution/open` and `project/open` — verified against Roslyn's
//! OpenSolutionHandler / OpenProjectsHandler and the VS Code C# extension's
//! `roslynProtocol.ts`. Do not invent alternate notifications.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::transport::LspTransport;
use super::LspError;

/// DLL file name shipped by the official Roslyn language-server packages.
pub const ROSLYN_DLL_NAME: &str = "Microsoft.CodeAnalysis.LanguageServer.dll";

/// Install guidance: Roslyn LS needs a .NET 10 runtime even when the analyzed
/// project targets an older TFM. No silent download from Astrolabe.
pub const CSHARP_INSTALL_HINT: &str = "\
Microsoft.CodeAnalysis.LanguageServer (Roslyn) not found. \
Astrolabe does not download it. Install the .NET 10 runtime \
(https://dotnet.microsoft.com/download/dotnet/10.0) — required to *run* the \
language server even if the analyzed project targets an older framework — \
then obtain Microsoft.CodeAnalysis.LanguageServer.dll (NuGet package \
roslyn-language-server.<rid>, or the VS Code C# / C# Dev Kit extension) and set \
ASTROLABE_LSP_CSHARP=/path/to/Microsoft.CodeAnalysis.LanguageServer.dll. \
Launch shape: dotnet <dll> --stdio. Not OmniSharp; not community csharp-ls.";

/// Workspace entry Roslyn should load after initialize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CsharpWorkspace {
    /// Prefer a single shallowest `.sln` / `.slnx`.
    Solution(PathBuf),
    /// No solution: open discovered `.csproj` files.
    Projects(Vec<PathBuf>),
}

/// Strip Roslyn document-symbol decorations so names match the syntactic index.
///
/// Roslyn 5.5+ often returns names like `Add(int, int) : int` or
/// `Name : string`. The graph stores the bare identifier (`Add`, `Name`).
///
/// Examples:
/// - `Add(int, int) : int` → `Add`
/// - `Name : string` → `Name`
/// - `ToString()` → `ToString`
/// - `Position : (int X, string Y)` → `Position`
/// - `SimpleMethod` → `SimpleMethod`
pub fn strip_roslyn_symbol_name(name: &str) -> &str {
    let name = name.trim();
    if name.is_empty() {
        return name;
    }
    // Property / field: "Name : Type". Guard parentheses only on the name
    // segment so tuple types ("(int X, string Y)") stay intact.
    if let Some((base, _)) = name.split_once(" : ") {
        let base = base.trim();
        if !base.is_empty() && !base.contains('(') {
            return base;
        }
    }
    // Method: "MethodName(params) : ReturnType" or "MethodName()"
    if let Some(paren) = name.find('(') {
        let base = name[..paren].trim();
        if !base.is_empty() {
            return base;
        }
    }
    name
}

/// Prefer the shallowest `.sln` / `.slnx` (BFS), else `.csproj` and `.vbproj`
/// files. Skips `bin`, `obj`, `.vs`, and hidden directories. Visual Basic
/// opens the same Roslyn server; there is no separate discovery env.
pub fn find_csharp_workspace(root: &Path) -> Option<CsharpWorkspace> {
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(root.to_path_buf());
    let mut projects: Vec<PathBuf> = Vec::new();

    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let lower = name_str.to_ascii_lowercase();
                if lower == "bin" || lower == "obj" || lower == ".vs" {
                    continue;
                }
                // `.vs` already skipped by hidden-dot check; keep explicit.
                dirs.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let lower = name_str.to_ascii_lowercase();
            if lower.ends_with(".sln") || lower.ends_with(".slnx") {
                return Some(CsharpWorkspace::Solution(path));
            }
            if lower.ends_with(".csproj") || lower.ends_with(".vbproj") {
                projects.push(path);
            }
        }
        // Stable BFS within a directory.
        dirs.sort();
        for d in dirs {
            queue.push_back(d);
        }
    }

    if projects.is_empty() {
        None
    } else {
        projects.sort();
        Some(CsharpWorkspace::Projects(projects))
    }
}

/// After `initialized`, open the solution/projects the way Roslyn requires.
pub fn open_csharp_workspace(transport: &LspTransport, root: &Path) -> Result<(), LspError> {
    match find_csharp_workspace(root) {
        Some(CsharpWorkspace::Solution(sln)) => {
            let uri = path_to_file_uri(&sln);
            transport.notify("solution/open", json!({ "solution": uri }))
        }
        Some(CsharpWorkspace::Projects(projects)) => {
            let uris: Vec<String> = projects.iter().map(|p| path_to_file_uri(p)).collect();
            transport.notify("project/open", json!({ "projects": uris }))
        }
        None => Ok(()),
    }
}

fn path_to_file_uri(path: &Path) -> String {
    super::transport::path_to_file_uri(path)
}

/// True when `path` looks like the Roslyn language-server DLL.
pub fn is_roslyn_dll(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.eq_ignore_ascii_case(ROSLYN_DLL_NAME) {
        return true;
    }
    let is_dll = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("dll"));
    is_dll && name.to_ascii_lowercase().contains("languageserver")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn scratch() -> Scratch {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-csharp-ws-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    #[test]
    fn strip_roslyn_symbol_name_examples() {
        assert_eq!(strip_roslyn_symbol_name("Add(int, int) : int"), "Add");
        assert_eq!(strip_roslyn_symbol_name("Name : string"), "Name");
        assert_eq!(strip_roslyn_symbol_name("ToString()"), "ToString");
        assert_eq!(
            strip_roslyn_symbol_name("Position : (int X, string Y)"),
            "Position"
        );
        assert_eq!(strip_roslyn_symbol_name("SimpleMethod"), "SimpleMethod");
        assert_eq!(strip_roslyn_symbol_name("  Add(int) : void  "), "Add");
    }

    #[test]
    fn prefers_shallowest_sln_over_deeper_sln_and_csproj() {
        let dir = scratch();
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(dir.0.join("App.sln"), "sln").unwrap();
        fs::write(dir.0.join("src").join("Deep.sln"), "sln").unwrap();
        fs::write(dir.0.join("src").join("App.csproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Solution(p)) => {
                assert_eq!(p.file_name().unwrap(), "App.sln");
            }
            other => panic!("expected shallow solution, got {other:?}"),
        }
    }

    #[test]
    fn prefers_slnx_when_present_at_same_depth() {
        let dir = scratch();
        fs::write(dir.0.join("App.slnx"), "slnx").unwrap();
        fs::write(dir.0.join("App.csproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Solution(p)) => {
                assert!(p.extension().unwrap().to_string_lossy().starts_with("sln"));
            }
            other => panic!("expected solution, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_csproj_skipping_bin_obj_vs() {
        let dir = scratch();
        fs::create_dir_all(dir.0.join("bin")).unwrap();
        fs::create_dir_all(dir.0.join("obj")).unwrap();
        fs::create_dir_all(dir.0.join(".vs")).unwrap();
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(dir.0.join("bin").join("Skip.csproj"), "x").unwrap();
        fs::write(dir.0.join("obj").join("Skip.csproj"), "x").unwrap();
        fs::write(dir.0.join(".vs").join("Skip.csproj"), "x").unwrap();
        fs::write(dir.0.join("src").join("App.csproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Projects(ps)) => {
                assert_eq!(ps.len(), 1);
                assert_eq!(ps[0].file_name().unwrap(), "App.csproj");
            }
            other => panic!("expected projects, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_vbproj_when_no_solution() {
        let dir = scratch();
        fs::write(dir.0.join("App.vbproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Projects(ps)) => {
                assert_eq!(ps.len(), 1);
                assert_eq!(ps[0].file_name().unwrap(), "App.vbproj");
            }
            other => panic!("expected projects, got {other:?}"),
        }
    }

    #[test]
    fn collects_vbproj_alongside_csproj() {
        let dir = scratch();
        fs::create_dir_all(dir.0.join("src")).unwrap();
        fs::write(dir.0.join("src").join("App.csproj"), "proj").unwrap();
        fs::write(dir.0.join("src").join("Lib.vbproj"), "proj").unwrap();
        match find_csharp_workspace(&dir.0) {
            Some(CsharpWorkspace::Projects(ps)) => {
                let names: Vec<String> = ps
                    .iter()
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .collect();
                assert!(names.iter().any(|n| n == "App.csproj"), "{names:?}");
                assert!(names.iter().any(|n| n == "Lib.vbproj"), "{names:?}");
            }
            other => panic!("expected projects, got {other:?}"),
        }
    }
}
