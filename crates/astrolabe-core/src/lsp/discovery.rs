//! Locate a language-server binary before anyone tries to start one.
//!
//! A missing server is a **reportable state** ([`LspError::Unavailable`]), not
//! silence and not a guessed fallback. Every [`ServerSpec`] therefore carries
//! an [`ServerSpec::install_hint`] the agent can show the user.
//!
//! ## Discovery ladder
//!
//! 1. `ASTROLABE_LSP_*` env override
//! 2. system `PATH`
//! 3. local cache under `~/.astrolabe/servers/{name}/{version}/`
//! 4. if [`crate::lsp::installer::AutoInstallMode::On`], [`ensure_installed`]
//! 5. else [`LspError::Unavailable`] with a structured [`NeedsInstall`] hint
//!
//! [`ensure_installed`] is **never** called unless mode is `On`
//! (`ASTROLABE_AUTO_INSTALL=on`). Default mode is `prompt`.
//!
//! ## Testability
//!
//! Lookups take an explicit [`Discovery`] (PATH directories + override map).
//! Production code calls [`discover`] / [`probe_report`], which snapshot the
//! process environment once. Tests inject a fake PATH built under
//! `std::env::temp_dir()` and **must not** call `std::env::set_var`, so they
//! stay isolated from other parallel tests. `Discovery::new` disables cache
//! probing and auto-install so existing unit tests stay hermetic.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::types::Language;

use super::installer::{
    self, ensure_installed_with, AutoInstallMode, InstallContext, InstallSpec, NeedsInstall,
    UreqFetcher,
};
use super::{LspError, ServerSpec};

/// Languages this module probes, in the order MCP reports should list them.
pub const PROBED_LANGUAGES: &[Language] = &[
    Language::Python,
    Language::Go,
    Language::Java,
    Language::Rust,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::C,
    Language::Cpp,
    Language::ObjC,
    Language::ObjCpp,
    Language::Swift,
    Language::Php,
    Language::Vue,
];

/// Snapshot of PATH and override variables used for one probe.
///
/// Constructed once, then reused. Never mutates process-global environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    path_dirs: Vec<PathBuf>,
    env: BTreeMap<String, String>,
    windows: bool,
    /// `None` skips cache probing (default for [`Discovery::new`] / tests).
    cache_root: Option<PathBuf>,
    auto_install: AutoInstallMode,
}

/// One language's probe outcome. Missing is a row, not an omitted row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    pub language: Language,
    /// First available server, if any.
    pub spec: Option<ServerSpec>,
    /// Always non-empty. When `spec` is `None`, this is how to install; when
    /// `Some`, it repeats the chosen server's hint.
    pub install_hint: String,
    /// Structured install payload for MCP when the binary is missing.
    pub needs_install: Option<NeedsInstall>,
}

impl ProbeResult {
    pub fn available(&self) -> bool {
        self.spec.is_some()
    }

    /// One-line status for MCP tools.
    pub fn summary(&self) -> String {
        match &self.spec {
            Some(spec) if spec.args.is_empty() => {
                format!("{}: {}", self.language.name(), spec.command.display())
            }
            Some(spec) => format!(
                "{}: {} {}",
                self.language.name(),
                spec.command.display(),
                spec.args.join(" ")
            ),
            None => format!(
                "{}: unavailable. {}",
                self.language.name(),
                self.install_hint
            ),
        }
    }
}

struct Candidate {
    bin: &'static str,
    args: &'static [&'static str],
    install: &'static str,
}

const PYTHON: &[Candidate] = &[
    Candidate {
        bin: "pyright-langserver",
        args: &["--stdio"],
        install: "npm i -g pyright",
    },
    Candidate {
        bin: "basedpyright-langserver",
        args: &["--stdio"],
        install: "npm i -g basedpyright",
    },
    Candidate {
        bin: "pylsp",
        args: &[],
        install: "pip install python-lsp-server",
    },
    Candidate {
        bin: "jedi-language-server",
        args: &[],
        install: "pip install jedi-language-server",
    },
    Candidate {
        bin: "ruff",
        args: &["server"],
        install: "pip install ruff",
    },
    Candidate {
        bin: "ruff-lsp",
        args: &[],
        install: "pip install ruff-lsp  # deprecated; prefer `pip install ruff` then `ruff server`",
    },
];

const GO: &[Candidate] = &[Candidate {
    bin: "gopls",
    args: &[],
    install: "go install golang.org/x/tools/gopls@latest",
}];

const JAVA: &[Candidate] = &[Candidate {
    bin: "jdtls",
    args: &[],
    install: "brew install jdtls",
}];

const RUST: &[Candidate] = &[Candidate {
    bin: "rust-analyzer",
    args: &[],
    install: "rustup component add rust-analyzer",
}];

const TYPESCRIPT: &[Candidate] = &[
    Candidate {
        bin: "typescript-language-server",
        args: &["--stdio"],
        install: "npm i -g typescript-language-server typescript",
    },
    Candidate {
        bin: "vtsls",
        args: &["--stdio"],
        install: "npm i -g @vtsls/language-server",
    },
];

const PHP: &[Candidate] = &[
    Candidate {
        bin: "intelephense",
        args: &["--stdio"],
        // Free features work without a key. Premium requires a purchased
        // INTELEPHENSE_LICENSE_KEY (LSP initializationOptions.licenceKey).
        // Do not pirate or share paid license keys.
        install: "npm i -g intelephense  # free tier works without a key; set a purchased INTELEPHENSE_LICENSE_KEY for premium (no pirated keys)",
    },
    Candidate {
        bin: "phpactor",
        args: &["language-server"],
        install: "composer global require phpactor/phpactor  # MIT; needs PHP 8.1+. Or download phpactor.phar from https://github.com/phpactor/phpactor/releases and run: php phpactor.phar language-server",
    },
];

const C_FAMILY: &[Candidate] = &[
    Candidate {
        bin: "clangd",
        // Serena launch: `--background-index` so cross-file refs work.
        args: &["--background-index"],
        install: "clangd not found on PATH. Install LLVM clangd, then ensure `clangd` is on PATH:\n  \
macOS:  brew install llvm  # add $(brew --prefix llvm)/bin to PATH\n  \
Ubuntu/Debian: sudo apt-get install clangd\n  \
Fedora/RHEL: sudo dnf install clang-tools-extra\n  \
Arch: sudo pacman -S clang\n  \
Or download a release from https://clangd.llvm.org/installation\n\
Prefer a compile_commands.json at the repo root (CMake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON).\n\
Override with ASTROLABE_LSP_CXX=/path/to/clangd (or ASTROLABE_LSP_C).\n\
With ASTROLABE_AUTO_INSTALL=on, Astrolabe can download a pinned clangd into ~/.astrolabe/servers/.",
    },
    Candidate {
        bin: "ccls",
        args: &[],
        install: "ccls (clangd alternative): brew install ccls  # or see https://github.com/MaskRay/ccls/wiki/Build\n\
Also accepts compile_commands.json at the repo root.\n\
Override with ASTROLABE_LSP_CXX=/path/to/ccls.",
    },
];

/// Swift — sourcekit-lsp only (RFC primary; ships with Xcode / CLT).
const APPLE_SWIFT: &[Candidate] = &[Candidate {
    bin: "sourcekit-lsp",
    args: &[],
    install: "xcode-select --install  # Xcode CLT ships sourcekit-lsp; or install full Xcode / Swift toolchain",
}];

/// ObjC/ObjC++ — sourcekit-lsp first (RFC), clangd second (Serena-shaped fallback).
const APPLE_OBJC: &[Candidate] = &[
    Candidate {
        bin: "sourcekit-lsp",
        args: &[],
        install: "xcode-select --install  # Xcode CLT ships sourcekit-lsp; or install full Xcode",
    },
    Candidate {
        bin: "clangd",
        args: &[],
        install: "brew install llvm  # clangd fallback for ObjC/ObjC++ when sourcekit-lsp is absent",
    },
];

const VUE: &[Candidate] = &[
    Candidate {
        bin: "vue-language-server",
        args: &["--stdio"],
        install: "npm i -g @vue/language-server",
    },
];

const JAVA_UNAVAILABLE: &str = "\
jdtls not found on PATH. Eclipse JDT Language Server (eclipse.jdt.ls) is the \
mainstream Java LSP, but it is not a single static binary.

Install the wrapper so `jdtls` is on PATH (stdio is the default transport):
  brew install jdtls
  # or download a release from https://github.com/eclipse-jdtls/eclipse.jdt.ls/releases \
and add its bin/ directory to PATH (requires Java 17+)

A unique per-project workspace directory is required at launch:
  jdtls -data /path/to/unique/workspace

Astrolabe does not auto-assemble the raw JVM launch, which looks like:
  java -Declipse.application=org.eclipse.jdt.ls.core.id1 \\
       -Dosgi.bundles.defaultStartLevel=4 \\
       -Declipse.product=org.eclipse.jdt.ls.core.product \\
       -jar plugins/org.eclipse.equinox.launcher_<version>.jar \\
       -configuration ./config_{linux|mac|win} \\
       -data /path/to/unique/workspace
The launcher jar version, OS-specific config directory, and a unique -data \
path must match the unpacked distribution; a shared -data directory corrupts \
the index.

Override with ASTROLABE_LSP_JAVA=/path/to/jdtls";

const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

impl Discovery {
    /// Snapshot `PATH` and `ASTROLABE_LSP_*` from the current process.
    pub fn from_process() -> Self {
        let mut env = BTreeMap::new();
        for key in override_keys_all().iter().copied().chain(["PATHEXT"]) {
            if let Ok(value) = std::env::var(key) {
                env.insert(key.to_string(), value);
            }
        }
        Self {
            path_dirs: parse_path(std::env::var_os("PATH").unwrap_or_default()),
            env,
            windows: cfg!(windows),
            cache_root: Some(installer::default_servers_dir()),
            auto_install: AutoInstallMode::from_env(),
        }
    }

    /// Probe against an explicit PATH string (same encoding as the `PATH` env var).
    ///
    /// Hermetic for tests: no cache root, auto-install [`AutoInstallMode::Off`].
    pub fn new(path: impl AsRef<OsStr>) -> Self {
        Self {
            path_dirs: parse_path(path),
            env: BTreeMap::new(),
            windows: cfg!(windows),
            cache_root: None,
            auto_install: AutoInstallMode::Off,
        }
    }

    /// Add or replace one environment variable (overrides or `PATHEXT`).
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Enable / override the servers cache root used by the discovery ladder.
    pub fn with_cache_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.cache_root = Some(root.into());
        self
    }

    /// Override [`AutoInstallMode`] (tests / session policy).
    pub fn with_auto_install(mut self, mode: AutoInstallMode) -> Self {
        self.auto_install = mode;
        self
    }

    pub fn auto_install_mode(&self) -> AutoInstallMode {
        self.auto_install
    }

    pub fn cache_root(&self) -> Option<&Path> {
        self.cache_root.as_deref()
    }

    /// First available [`ServerSpec`] for `language`, or [`LspError::Unavailable`].
    pub fn discover(&self, language: Language) -> Result<ServerSpec, LspError> {
        let probed = self.probe(language);
        match probed.spec {
            Some(spec) => Ok(spec),
            None => Err(LspError::Unavailable(language, probed.install_hint)),
        }
    }

    /// Probe one language. Absence is `spec: None`, never an error.
    ///
    /// Ladder: env override → PATH → cache → (if On) install → Unavailable hint.
    pub fn probe(&self, language: Language) -> ProbeResult {
        if let Some((key, value)) = self.override_value(language) {
            match self.resolve_override(&value) {
                Some(command) => {
                    let spec = spec_for_resolved(language, command);
                    let install_hint = spec.install_hint.clone();
                    return ProbeResult {
                        language,
                        spec: Some(spec),
                        install_hint,
                        needs_install: None,
                    };
                }
                None => {
                    let hint = format!(
                        "{key} is set to `{value}` but that executable was not found. {}",
                        self.missing_hint(language)
                    );
                    return self.unavailable(language, hint);
                }
            }
        }

        for candidate in candidates(language) {
            if let Some(command) = self.find_on_path(candidate.bin) {
                let spec = spec_from_candidate(language, command, candidate);
                let install_hint = spec.install_hint.clone();
                return ProbeResult {
                    language,
                    spec: Some(spec),
                    install_hint,
                    needs_install: None,
                };
            }
        }

        // Cache: ~/.astrolabe/servers/{name}/{version}/…
        if let Some(cache_root) = &self.cache_root {
            for candidate in candidates(language) {
                if let Some(command) =
                    installer::find_cached_binary(cache_root, candidate.bin, Path::new(candidate.bin))
                {
                    let spec = spec_from_candidate(language, command, candidate);
                    let install_hint = spec.install_hint.clone();
                    return ProbeResult {
                        language,
                        spec: Some(spec),
                        install_hint,
                        needs_install: None,
                    };
                }
                // Also try common bin/ layout used by packaged releases.
                let rel = PathBuf::from("bin").join(candidate.bin);
                if let Some(command) =
                    installer::find_cached_binary(cache_root, candidate.bin, &rel)
                {
                    let spec = spec_from_candidate(language, command, candidate);
                    let install_hint = spec.install_hint.clone();
                    return ProbeResult {
                        language,
                        spec: Some(spec),
                        install_hint,
                        needs_install: None,
                    };
                }
            }
        }

        // Auto-install only when mode is On (never for Prompt/Off).
        if self.auto_install == AutoInstallMode::On {
            if let Some(install_spec) = catalog_install_spec(language) {
                if let Some(cache_root) = &self.cache_root {
                    let fetcher = UreqFetcher::default();
                    let ctx = InstallContext {
                        cache_root: cache_root.clone(),
                        fetcher: &fetcher,
                        offline: std::env::var(installer::ENV_OFFLINE)
                            .map(|v| {
                                matches!(
                                    v.trim().to_ascii_lowercase().as_str(),
                                    "1" | "true" | "yes" | "on"
                                )
                            })
                            .unwrap_or(false),
                        mirror_github: std::env::var(installer::ENV_MIRROR_GITHUB)
                            .ok()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                        lock_timeout: std::time::Duration::from_secs(120),
                    };
                    match ensure_installed_with(&install_spec, &ctx) {
                        Ok(command) => {
                            let cands = candidates(language);
                            let candidate = cands.first().expect("catalog langs have candidates");
                            let spec = spec_from_candidate(language, command, candidate);
                            let install_hint = spec.install_hint.clone();
                            return ProbeResult {
                                language,
                                spec: Some(spec),
                                install_hint,
                                needs_install: None,
                            };
                        }
                        Err(err) => {
                            let hint = format!(
                                "auto-install of {} failed: {err}. {}",
                                install_spec.name,
                                self.missing_hint(language)
                            );
                            return self.unavailable(language, hint);
                        }
                    }
                }
            }
        }

        self.unavailable(language, self.missing_hint(language))
    }

    fn unavailable(&self, language: Language, hint: String) -> ProbeResult {
        let server = primary_bin(language);
        let cache_root = self
            .cache_root
            .clone()
            .unwrap_or_else(installer::default_servers_dir);
        let needs = installer::needs_install_for(
            language,
            hint.clone(),
            server,
            catalog_install_spec(language),
            self.auto_install,
            &cache_root,
        );
        ProbeResult {
            language,
            spec: None,
            install_hint: needs.message(),
            needs_install: Some(needs),
        }
    }

    /// Probe every supported language. Order is [`PROBED_LANGUAGES`].
    pub fn probe_report(&self) -> Vec<ProbeResult> {
        PROBED_LANGUAGES
            .iter()
            .map(|lang| self.probe(*lang))
            .collect()
    }

    fn override_value(&self, language: Language) -> Option<(String, String)> {
        for key in env_override_keys(language) {
            if let Some(value) = self.env.get(*key) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(((*key).to_string(), trimmed.to_string()));
                }
            }
        }
        None
    }

    fn resolve_override(&self, value: &str) -> Option<PathBuf> {
        let path = Path::new(value);
        if let Some(found) = self.existing_command(path) {
            return Some(found);
        }
        if is_bare_name(path) {
            return self.find_on_path(value);
        }
        None
    }

    fn existing_command(&self, path: &Path) -> Option<PathBuf> {
        if let Some(found) = self.runnable_path(path) {
            return Some(found);
        }
        if self.windows && path.extension().is_none() {
            for ext in self.pathext() {
                let with_ext = path.with_extension(ext.trim_start_matches('.'));
                if let Some(found) = self.runnable_path(&with_ext) {
                    return Some(found);
                }
            }
        }
        None
    }

    fn find_on_path(&self, bin: &str) -> Option<PathBuf> {
        let names = command_names(bin, self.windows, &self.pathext_joined());
        for dir in &self.path_dirs {
            for name in &names {
                let candidate = dir.join(name);
                if let Some(found) = self.runnable_path(&candidate) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Like [`is_runnable`], but when `windows` is set also accepts a
    /// case-insensitive filename match in the same directory.
    ///
    /// Real Windows PATH search is case-insensitive. Tests force
    /// `Discovery.windows = true` on Linux CI, where the filesystem is
    /// case-sensitive and `PATHEXT` defaults to uppercase (`.CMD`) while
    /// shims are often written as `.cmd`. Without this fallback the
    /// `windows_cmd_suffix_is_accepted` unit test fails on Linux runners.
    fn runnable_path(&self, path: &Path) -> Option<PathBuf> {
        if is_runnable(path) {
            return Some(path.to_path_buf());
        }
        if !self.windows {
            return None;
        }
        let name = path.file_name()?.to_string_lossy();
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
        let entries = std::fs::read_dir(parent).ok()?;
        for entry in entries.flatten() {
            let entry_name = entry.file_name();
            if !entry_name
                .to_string_lossy()
                .eq_ignore_ascii_case(name.as_ref())
            {
                continue;
            }
            let candidate = entry.path();
            if is_runnable(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn pathext_joined(&self) -> String {
        self.env
            .get("PATHEXT")
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_PATHEXT)
            .to_string()
    }

    fn pathext(&self) -> Vec<String> {
        parse_pathext(&self.pathext_joined())
    }

    fn missing_hint(&self, language: Language) -> String {
        if language == Language::Java {
            return JAVA_UNAVAILABLE.to_string();
        }
        let cands = candidates(language);
        if cands.is_empty() {
            return format!(
                "AST-only: no language-server discovery candidates wired for {}.                  Or set {} to an executable when support lands.",
                language.name(),
                env_override_keys(language)[0]
            );
        }
        let bins: Vec<&str> = cands.iter().map(|c| c.bin).collect();
        let installs: Vec<&str> = cands.iter().map(|c| c.install).collect();
        let extra = match language {
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                " TypeScript 7's native `tsc --lsp --stdio` is not auto-detected \
                  because older `tsc` binaries do not speak LSP; set \
                  ASTROLABE_LSP_TYPESCRIPT to that executable if you want it."
            }
            Language::Php => {
                " Intelephense premium features need a purchased INTELEPHENSE_LICENSE_KEY;                   never use a pirated key. Phpactor is MIT and needs PHP 8.1+."
            }
            Language::Vue => {
                " Volar (`@vue/language-server`) is the P0 Vue server; a dual-server                   TypeScript arrangement is a follow-up, not a silent fallback."
            }
            Language::Swift => {
                " sourcekit-lsp is the RFC-primary Swift server; it ships with                   Xcode / Command Line Tools (`xcode-select --install`)."
            }
            Language::ObjC | Language::ObjCpp => {
                " Prefer sourcekit-lsp (RFC); clangd is a practical fallback for                   .m/.mm (Serena routes ObjC there). Pure .h headers remain                   Language::C unless content-aware ObjC header detection lands."
            }
            _ => "",
        };
        format!(
            "none of {} found on PATH. Install one of:\n  {}\nOr set {} to the executable.{}",
            bins.join(", "),
            installs.join("\n  "),
            env_override_keys(language)[0],
            extra
        )
    }
}

impl Default for Discovery {
    /// Empty PATH, auto-install off — safe for `StubInstaller::default()` and tests.
    fn default() -> Self {
        Self::new("")
    }
}

/// Discover using the current process environment.
pub fn discover(language: Language) -> Result<ServerSpec, LspError> {
    Discovery::from_process().discover(language)
}

/// Probe every supported language using the current process environment.
pub fn probe_report() -> Vec<ProbeResult> {
    Discovery::from_process().probe_report()
}

/// Primary override variable for `language` (`ASTROLABE_LSP_PYTHON`, …).
pub fn env_override_var(language: Language) -> &'static str {
    env_override_keys(language)[0]
}

fn candidates(language: Language) -> &'static [Candidate] {
    match language {
        Language::Python => PYTHON,
        Language::Go => GO,
        Language::Java => JAVA,
        Language::Rust => RUST,
        Language::TypeScript | Language::Tsx | Language::JavaScript => TYPESCRIPT,
        Language::Php => PHP,
        Language::C | Language::Cpp => C_FAMILY,
        Language::Swift => APPLE_SWIFT,
        Language::ObjC | Language::ObjCpp => APPLE_OBJC,
        Language::Vue => VUE,
    }
}

fn env_override_keys(language: Language) -> &'static [&'static str] {
    match language {
        Language::Python => &["ASTROLABE_LSP_PYTHON"],
        Language::Go => &["ASTROLABE_LSP_GO"],
        Language::Java => &["ASTROLABE_LSP_JAVA"],
        Language::Rust => &["ASTROLABE_LSP_RUST"],
        Language::TypeScript => &["ASTROLABE_LSP_TYPESCRIPT"],
        Language::Tsx => &["ASTROLABE_LSP_TSX", "ASTROLABE_LSP_TYPESCRIPT"],
        Language::JavaScript => &["ASTROLABE_LSP_JAVASCRIPT", "ASTROLABE_LSP_TYPESCRIPT"],
        Language::Php => &["ASTROLABE_LSP_PHP"],
        Language::C => &["ASTROLABE_LSP_C", "ASTROLABE_LSP_CXX"],
        Language::Cpp => &["ASTROLABE_LSP_CXX", "ASTROLABE_LSP_C"],
        Language::ObjC => &["ASTROLABE_LSP_OBJC", "ASTROLABE_LSP_SWIFT"],
        Language::ObjCpp => &["ASTROLABE_LSP_OBJCPP", "ASTROLABE_LSP_SWIFT"],
        Language::Swift => &["ASTROLABE_LSP_SWIFT"],
        Language::Vue => &["ASTROLABE_LSP_VUE"],
    }
}

fn override_keys_all() -> &'static [&'static str] {
    &[
        "ASTROLABE_LSP_PYTHON",
        "ASTROLABE_LSP_GO",
        "ASTROLABE_LSP_JAVA",
        "ASTROLABE_LSP_RUST",
        "ASTROLABE_LSP_TYPESCRIPT",
        "ASTROLABE_LSP_TSX",
        "ASTROLABE_LSP_JAVASCRIPT",
        "ASTROLABE_LSP_PHP",
        "ASTROLABE_LSP_C",
        "ASTROLABE_LSP_CXX",
        "ASTROLABE_LSP_OBJC",
        "ASTROLABE_LSP_OBJCPP",
        "ASTROLABE_LSP_SWIFT",
        "ASTROLABE_LSP_VUE",
    ]
}

fn primary_bin(language: Language) -> &'static str {
    candidates(language)
        .first()
        .map(|c| c.bin)
        .unwrap_or(language.name())
}

/// Optional download catalog entry. Absent means Prompt/On still surface a
/// manual hint but cannot call [`crate::lsp::ensure_installed`] until a release
/// matrix fills the URL + official sha256.
pub fn catalog_install_spec(language: Language) -> Option<InstallSpec> {
    match language {
        // Official clangd GitHub release (Serena pins 19.1.2). Only used when
        // AutoInstallMode::On — Prompt/Off never download.
        Language::C | Language::Cpp => clangd_install_spec(),
        _ => None,
    }
}

/// Pinned clangd 19.1.2 artifact for the current host (SHA-256 of the archive).
fn clangd_install_spec() -> Option<InstallSpec> {
    const VERSION: &str = "19.1.2";
    // Checksums from Serena's clangd_language_server.py (official release assets).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-linux-{VERSION}.zip"
            ),
            sha256: "7c09614eff857d590e4502ef516f035ff94cfb8b795de14ece5afbc53a206caf".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd")),
        });
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-mac-{VERSION}.zip"
            ),
            sha256: "d3b329b3f58602c57ca6501d255147af1bccad3691b1cb0c12c258fcd2da1be3".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd")),
        });
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-mac-{VERSION}.zip"
            ),
            sha256: "d3b329b3f58602c57ca6501d255147af1bccad3691b1cb0c12c258fcd2da1be3".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd")),
        });
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return Some(InstallSpec {
            name: "clangd".into(),
            version: VERSION.into(),
            url: format!(
                "https://github.com/clangd/clangd/releases/download/{VERSION}/clangd-windows-{VERSION}.zip"
            ),
            sha256: "5b6ceb0f85d63fa0c2c9aab31c29bebd41dc11da1f160ef21bc2fea93270a20d".into(),
            archive: installer::ArchiveFormat::Zip,
            binary_relative: PathBuf::from(format!("clangd_{VERSION}/bin/clangd.exe")),
        });
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64"),
    )))]
    {
        None
    }
}

fn spec_from_candidate(language: Language, command: PathBuf, candidate: &Candidate) -> ServerSpec {
    ServerSpec {
        language,
        command,
        args: candidate.args.iter().map(|s| (*s).to_string()).collect(),
        install_hint: candidate.install.to_string(),
    }
}

fn spec_for_resolved(language: Language, command: PathBuf) -> ServerSpec {
    let stem = bin_stem(&command);
    if let Some(candidate) = find_candidate_by_bin(&stem) {
        return spec_from_candidate(language, command, candidate);
    }
    let args = extra_args_for_unknown_stem(&stem);
    ServerSpec {
        language,
        command,
        args,
        install_hint: format!("user override via {}", env_override_var(language)),
    }
}

fn find_candidate_by_bin(bin: &str) -> Option<&'static Candidate> {
    PYTHON
        .iter()
        .chain(GO)
        .chain(JAVA)
        .chain(RUST)
        .chain(TYPESCRIPT)
        .chain(PHP)
        .chain(C_FAMILY)
        .chain(APPLE_SWIFT)
        .chain(APPLE_OBJC)
        .chain(VUE)
        .find(|c| c.bin == bin)
}

fn extra_args_for_unknown_stem(stem: &str) -> Vec<String> {
    match stem {
        "tsc" | "tsgo" => vec!["--lsp".into(), "--stdio".into()],
        "clangd" => vec!["--background-index".into()],
        _ => Vec::new(),
    }
}

fn bin_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    strip_windows_ext(&name)
}

fn strip_windows_ext(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for ext in [".cmd", ".exe", ".bat", ".com"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            return name[..stripped.len()].to_string();
        }
    }
    name.to_string()
}

fn command_names(bin: &str, windows: bool, pathext: &str) -> Vec<String> {
    let mut names = vec![bin.to_string()];
    if !windows {
        return names;
    }
    if has_pathext_suffix(bin, pathext) {
        return names;
    }
    for ext in parse_pathext(pathext) {
        names.push(format!("{bin}{ext}"));
    }
    names
}

fn parse_pathext(pathext: &str) -> Vec<String> {
    pathext
        .split(';')
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

fn has_pathext_suffix(name: &str, pathext: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    parse_pathext(pathext)
        .iter()
        .any(|ext| lower.ends_with(&ext.to_ascii_lowercase()))
}

fn parse_path(path: impl AsRef<OsStr>) -> Vec<PathBuf> {
    std::env::split_paths(path.as_ref())
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

fn is_bare_name(path: &Path) -> bool {
    path.components().count() == 1
}

fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
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

/// Lets the pool drive discovery without depending on this module's concrete
/// type.
impl super::pool::DiscoverServers for Discovery {
    fn discover(
        &self,
        language: crate::types::Language,
    ) -> Result<super::ServerSpec, super::LspError> {
        Discovery::discover(self, language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process;
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
            "astrolabe-lsp-discovery-{}-{}",
            process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create temp dir for discovery tests");
        Scratch(dir)
    }

    fn write_fake_executable(path: &Path) {
        fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write fake executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).expect("chmod +x");
        }
    }

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let path = if cfg!(windows) {
            dir.join(format!("{name}.cmd"))
        } else {
            dir.join(name)
        };
        write_fake_executable(&path);
        path
    }

    fn discovery_with_dirs(dirs: &[&Path]) -> Discovery {
        let path = std::env::join_paths(dirs).expect("join PATH");
        Discovery::new(path)
    }

    fn unavailable_hint(err: LspError) -> (Language, String) {
        match err {
            LspError::Unavailable(lang, hint) => (lang, hint),
            other => panic!("expected Unavailable, got {other}"),
        }
    }

    #[test]
    fn missing_binary_is_unavailable_with_install_command() {
        let d = Discovery::new("");
        let err = d.discover(Language::Python).unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Python);
        assert!(!hint.is_empty());
        assert!(
            hint.contains("npm i -g pyright"),
            "install_hint must contain an executable install command, got: {hint}"
        );
        assert!(hint.contains("ASTROLABE_LSP_PYTHON"));
    }

    #[test]
    fn empty_path_install_hints_cover_every_language() {
        let d = Discovery::new("");
        for language in PROBED_LANGUAGES {
            let result = d.probe(*language);
            assert!(result.spec.is_none(), "{language:?} should be missing");
            assert!(!result.install_hint.is_empty());
            match language {
                Language::Python => assert!(result.install_hint.contains("npm i -g pyright")),
                Language::Go => {
                    assert!(result
                        .install_hint
                        .contains("go install golang.org/x/tools/gopls@latest"))
                }
                Language::Java => {
                    assert!(result.install_hint.contains("brew install jdtls"));
                    assert!(result.install_hint.contains("java -"));
                    assert!(result.install_hint.contains("-data"));
                    assert!(result.install_hint.contains("org.eclipse.equinox.launcher"));
                }
                Language::Rust => {
                    assert!(result
                        .install_hint
                        .contains("rustup component add rust-analyzer"))
                }
                Language::TypeScript | Language::Tsx | Language::JavaScript => {
                    assert!(result
                        .install_hint
                        .contains("npm i -g typescript-language-server"))
                }
                Language::Php => {
                    assert!(result.install_hint.contains("npm i -g intelephense"));
                    assert!(result.install_hint.contains("phpactor"));
                    assert!(
                        result.install_hint.contains("INTELEPHENSE_LICENSE_KEY")
                            || result.install_hint.contains("purchased"),
                        "license guidance required, got: {}",
                        result.install_hint
                    );
                }
                Language::C | Language::Cpp => {
                    assert!(result.install_hint.to_ascii_lowercase().contains("clangd"))
                }
                Language::ObjC | Language::ObjCpp | Language::Swift => {
                    assert!(result.install_hint.contains("sourcekit-lsp"))
                }
                Language::Vue => {
                    assert!(result.install_hint.contains("@vue/language-server"))
                }
            }
            assert!(
                result.needs_install.is_some(),
                "{language:?} must export NeedsInstall"
            );
        }
    }

    #[test]
    fn finds_first_priority_binary_on_path() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        fake_bin(&dir.0, "pylsp");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Python)
            .unwrap();
        assert!(
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("pyright-langserver"),
            "priority must prefer pyright over pylsp, got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["--stdio"]);
        assert_eq!(spec.language, Language::Python);
        assert!(spec.install_hint.contains("npm i -g pyright"));
    }

    #[test]
    fn skips_missing_higher_priority_and_uses_next() {
        let dir = scratch();
        fake_bin(&dir.0, "pylsp");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Python)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("pylsp"));
        assert!(spec.args.is_empty());
        assert!(spec.install_hint.contains("pip install python-lsp-server"));
    }

    #[test]
    fn env_override_wins_over_path_priority() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        let pylsp = fake_bin(&dir.0, "pylsp");
        let spec = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_PYTHON", pylsp.to_string_lossy())
            .discover(Language::Python)
            .unwrap();
        assert_eq!(spec.command, pylsp);
        assert!(spec.args.is_empty());
    }

    #[test]
    fn env_override_bare_name_searches_path() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        fake_bin(&dir.0, "jedi-language-server");
        let spec = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_PYTHON", "jedi-language-server")
            .discover(Language::Python)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("jedi-language-server"));
    }

    #[test]
    fn env_override_missing_is_unavailable_not_silent() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        let missing = dir.0.join("does-not-exist");
        let err = discovery_with_dirs(&[&dir.0])
            .with_env("ASTROLABE_LSP_PYTHON", missing.to_string_lossy())
            .discover(Language::Python)
            .unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Python);
        assert!(hint.contains("ASTROLABE_LSP_PYTHON"));
        assert!(
            hint.contains("does-not-exist"),
            "hint should mention the missing override path, got: {hint}"
        );
        assert!(hint.contains("npm i -g pyright"));
    }

    #[test]
    fn stdio_args_for_known_servers() {
        let dir = scratch();
        fake_bin(&dir.0, "pyright-langserver");
        fake_bin(&dir.0, "typescript-language-server");
        fake_bin(&dir.0, "vtsls");
        fake_bin(&dir.0, "gopls");
        fake_bin(&dir.0, "rust-analyzer");
        fake_bin(&dir.0, "ruff");
        let d = discovery_with_dirs(&[&dir.0]);

        assert_eq!(d.discover(Language::Python).unwrap().args, vec!["--stdio"]);
        assert_eq!(
            d.discover(Language::TypeScript).unwrap().args,
            vec!["--stdio"]
        );
        assert_eq!(d.discover(Language::Go).unwrap().args, Vec::<String>::new());
        assert_eq!(
            d.discover(Language::Rust).unwrap().args,
            Vec::<String>::new()
        );

        let ruff_only = scratch();
        fake_bin(&ruff_only.0, "ruff");
        assert_eq!(
            discovery_with_dirs(&[&ruff_only.0])
                .discover(Language::Python)
                .unwrap()
                .args,
            vec!["server"]
        );
    }

    #[test]
    fn typescript_javascript_and_tsx_share_servers() {
        let dir = scratch();
        fake_bin(&dir.0, "vtsls");
        let d = discovery_with_dirs(&[&dir.0]);
        for language in [Language::TypeScript, Language::Tsx, Language::JavaScript] {
            let spec = d.discover(language).unwrap();
            assert_eq!(spec.language, language);
            assert!(spec
                .command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("vtsls"));
            assert_eq!(spec.args, vec!["--stdio"]);
        }
    }

    #[test]
    fn javascript_override_falls_back_to_typescript_var() {
        let dir = scratch();
        let vtsls = fake_bin(&dir.0, "vtsls");
        let spec = Discovery::new("")
            .with_env("ASTROLABE_LSP_TYPESCRIPT", vtsls.to_string_lossy())
            .discover(Language::JavaScript)
            .unwrap();
        assert_eq!(spec.command, vtsls);
    }

    #[test]
    fn java_without_jdtls_explains_wrapper_and_java_jar() {
        let err = Discovery::new("").discover(Language::Java).unwrap_err();
        let (_, hint) = unavailable_hint(err);
        assert!(hint.contains("brew install jdtls"));
        assert!(hint.contains("java -Declipse.application"));
        assert!(hint.contains("-jar"));
        assert!(hint.contains("-data"));
        assert!(hint.contains("config_{linux|mac|win}"));
    }

    #[test]
    fn java_finds_jdtls_wrapper_when_present() {
        let dir = scratch();
        fake_bin(&dir.0, "jdtls");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Java)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("jdtls"));
        assert!(spec.args.is_empty());
    }

    #[test]
    fn probe_report_covers_every_language_and_is_deterministic() {
        let dir = scratch();
        fake_bin(&dir.0, "gopls");
        let d = discovery_with_dirs(&[&dir.0]);
        let first = d.probe_report();
        let second = d.probe_report();
        assert_eq!(first, second);
        assert_eq!(first.len(), PROBED_LANGUAGES.len());
        let langs: Vec<Language> = first.iter().map(|r| r.language).collect();
        assert_eq!(langs, PROBED_LANGUAGES);
        for row in &first {
            assert!(!row.install_hint.is_empty());
        }
        let go = first.iter().find(|r| r.language == Language::Go).unwrap();
        assert!(go.available());
        let rust = first.iter().find(|r| r.language == Language::Rust).unwrap();
        assert!(!rust.available());
        assert!(rust.summary().contains("unavailable"));
    }

    #[test]
    fn process_snapshot_reports_every_language() {
        // Does not compare two snapshots: other tests in the crate must not
        // mutate process env, but we still avoid depending on that here.
        let report = probe_report();
        assert_eq!(report.len(), PROBED_LANGUAGES.len());
        let langs: Vec<Language> = report.iter().map(|r| r.language).collect();
        assert_eq!(langs, PROBED_LANGUAGES);
        for row in &report {
            assert!(!row.install_hint.is_empty());
        }
    }

    #[test]
    fn windows_cmd_suffix_is_accepted() {
        let dir = scratch();
        let cmd = dir.0.join("pyright-langserver.cmd");
        write_fake_executable(&cmd);
        let mut d = Discovery::new(dir.0.as_os_str());
        d.windows = true;
        let spec = d.discover(Language::Python).unwrap();
        assert!(
            spec.command
                .file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("pyright-langserver.cmd"),
            "expected a .cmd shim, got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["--stdio"]);
    }

    #[test]
    fn command_names_include_windows_extensions() {
        let names = command_names("gopls", true, DEFAULT_PATHEXT);
        assert_eq!(names[0], "gopls");
        assert!(
            names.contains(&"gopls.EXE".to_string())
                || names.iter().any(|n| n.eq_ignore_ascii_case("gopls.exe"))
        );
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("gopls.cmd")));
        let unix = command_names("gopls", false, DEFAULT_PATHEXT);
        assert_eq!(unix, vec!["gopls".to_string()]);
        let already = command_names("gopls.cmd", true, DEFAULT_PATHEXT);
        assert_eq!(already, vec!["gopls.cmd".to_string()]);
    }

    #[test]
    fn higher_priority_in_later_path_dir_still_wins() {
        let first = scratch();
        let second = scratch();
        fake_bin(&first.0, "pylsp");
        fake_bin(&second.0, "pyright-langserver");
        let spec = discovery_with_dirs(&[&first.0, &second.0])
            .discover(Language::Python)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("pyright-langserver"));
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_skipped() {
        let dir = scratch();
        fs::write(dir.0.join("gopls"), b"not executable").unwrap();
        let err = discovery_with_dirs(&[&dir.0])
            .discover(Language::Go)
            .unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Go);
        assert!(hint.contains("go install golang.org/x/tools/gopls@latest"));
    }

    #[test]
    fn tsc_override_gets_lsp_stdio_args() {
        let dir = scratch();
        let tsc = fake_bin(&dir.0, "tsc");
        let spec = Discovery::new("")
            .with_env("ASTROLABE_LSP_TYPESCRIPT", tsc.to_string_lossy())
            .discover(Language::TypeScript)
            .unwrap();
        assert_eq!(spec.args, vec!["--lsp", "--stdio"]);
    }

    #[test]
    fn env_override_var_names_are_stable() {
        assert_eq!(env_override_var(Language::Python), "ASTROLABE_LSP_PYTHON");
        assert_eq!(env_override_var(Language::Go), "ASTROLABE_LSP_GO");
        assert_eq!(env_override_var(Language::Java), "ASTROLABE_LSP_JAVA");
        assert_eq!(env_override_var(Language::Rust), "ASTROLABE_LSP_RUST");
        assert_eq!(
            env_override_var(Language::TypeScript),
            "ASTROLABE_LSP_TYPESCRIPT"
        );
        assert_eq!(env_override_var(Language::Tsx), "ASTROLABE_LSP_TSX");
        assert_eq!(
            env_override_var(Language::JavaScript),
            "ASTROLABE_LSP_JAVASCRIPT"
        );
        assert_eq!(env_override_var(Language::Php), "ASTROLABE_LSP_PHP");
    
        assert_eq!(env_override_var(Language::Swift), "ASTROLABE_LSP_SWIFT");
        assert_eq!(env_override_var(Language::ObjC), "ASTROLABE_LSP_OBJC");
        assert_eq!(env_override_var(Language::ObjCpp), "ASTROLABE_LSP_OBJCPP");
    }


    #[test]
    fn php_prefers_intelephense_then_phpactor() {
        let dir = scratch();
        fake_bin(&dir.0, "intelephense");
        fake_bin(&dir.0, "phpactor");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Php)
            .unwrap();
        assert!(
            spec.command
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("intelephense"),
            "priority must prefer intelephense, got {:?}",
            spec.command
        );
        assert_eq!(spec.args, vec!["--stdio"]);
        assert!(spec.install_hint.contains("npm i -g intelephense"));
    }

    #[test]
    fn php_falls_back_to_phpactor_language_server() {
        let dir = scratch();
        fake_bin(&dir.0, "phpactor");
        let spec = discovery_with_dirs(&[&dir.0])
            .discover(Language::Php)
            .unwrap();
        assert!(spec
            .command
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .starts_with("phpactor"));
        assert_eq!(spec.args, vec!["language-server"]);
        assert!(spec.install_hint.contains("phpactor"));
    }

    #[test]
    fn cache_root_is_consulted_after_path_miss() {
        let cache = scratch();
        let version = cache.0.join("clangd").join("18.1.3");
        fs::create_dir_all(&version).unwrap();
        let bin = fake_bin(&version, "clangd");
        fs::write(version.join(".installed"), "ok").unwrap();
        let d = Discovery::new("")
            .with_cache_root(&cache.0)
            .with_auto_install(AutoInstallMode::Off);
        let spec = d.discover(Language::Cpp).unwrap();
        assert_eq!(spec.command, bin);
        assert!(d.probe(Language::Cpp).needs_install.is_none());
    }

    #[test]
    fn prompt_mode_never_auto_installs() {
        let cache = scratch();
        let d = Discovery::new("")
            .with_cache_root(&cache.0)
            .with_auto_install(AutoInstallMode::Prompt);
        let result = d.probe(Language::Cpp);
        assert!(result.spec.is_none());
        let needs = result.needs_install.expect("structured hint");
        assert_eq!(needs.hint.mode, AutoInstallMode::Prompt);
    }

    #[test]
    fn new_discovery_defaults_are_hermetic() {
        let d = Discovery::new("");
        assert!(d.cache_root().is_none());
        assert_eq!(d.auto_install_mode(), AutoInstallMode::Off);
    }


    #[test]
    fn sourcekit_is_primary_for_swift_and_objc_family() {
        let dir = scratch();
        fake_bin(&dir.0, "sourcekit-lsp");
        fake_bin(&dir.0, "clangd");
        let d = discovery_with_dirs(&[&dir.0]);
        for language in [Language::Swift, Language::ObjC, Language::ObjCpp] {
            let spec = d.discover(language).unwrap();
            assert_eq!(spec.language, language);
            assert!(
                spec.command
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("sourcekit-lsp"),
                "{language:?} must prefer sourcekit-lsp, got {:?}",
                spec.command
            );
        }
    }

    #[test]
    fn objc_falls_back_to_clangd_when_sourcekit_missing() {
        let dir = scratch();
        fake_bin(&dir.0, "clangd");
        let d = discovery_with_dirs(&[&dir.0]);
        for language in [Language::ObjC, Language::ObjCpp] {
            let spec = d.discover(language).unwrap();
            assert!(
                spec.command
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("clangd"),
                "{language:?} should fall back to clangd, got {:?}",
                spec.command
            );
        }
        let err = d.discover(Language::Swift).unwrap_err();
        let (lang, hint) = unavailable_hint(err);
        assert_eq!(lang, Language::Swift);
        assert!(hint.contains("sourcekit-lsp") || hint.contains("xcode-select"));
    }

    #[test]
    fn from_path_recognizes_apple_extensions() {
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.m")),
            Some(Language::ObjC)
        );
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.mm")),
            Some(Language::ObjCpp)
        );
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.swift")),
            Some(Language::Swift)
        );
        // .h stays C (foundation); ObjC headers need companion/.m awareness later.
        assert_eq!(
            Language::from_path(&crate::types::RelPath::new("Foo.h")),
            Some(Language::C)
        );
    }

}
