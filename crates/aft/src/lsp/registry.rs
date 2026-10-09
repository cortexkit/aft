use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::config::{Config, UserServerDef};
use crate::lsp::roots::{
    find_rust_workspace_root, find_workspace_root, find_workspace_root_within,
    outermost_marker_root,
};
use crate::lsp::typescript_project::{native_typescript_for, NATIVE_SERVER_ARGS};

/// Resolve an LSP binary name to a full path.
///
/// Resolution order (mirrors `format::resolve_tool` for formatters/checkers):
/// 1. `<project_root>/node_modules/.bin/<binary>` — project devDependency
/// 2. Each path in `extra_paths` joined with `<binary>` — plugin-supplied
///    auto-install cache locations such as
///    `~/.cache/aft/lsp-packages/<pkg>/node_modules/.bin/`
/// 3. PATH via [`which::which`]
///
/// On Windows, candidate directories are also probed with `.cmd`, `.exe`,
/// and `.bat` extensions because npm-installed shims often use `.cmd`.
/// `which::which` handles PATHEXT natively for the PATH fallback.
pub fn resolve_lsp_binary(
    binary: &str,
    project_root: Option<&Path>,
    extra_paths: &[PathBuf],
) -> Option<PathBuf> {
    // 1. Project-local node_modules/.bin
    if let Some(root) = project_root {
        let local_bin = root.join("node_modules").join(".bin");
        if let Some(found) = probe_dir(&local_bin, binary) {
            return Some(found);
        }
    }

    // 2. Plugin-supplied extra paths (auto-install cache, etc.)
    for dir in extra_paths {
        if let Some(found) = probe_dir(dir, binary) {
            return Some(found);
        }
    }

    // 3. PATH fallback. An Apple developer-tools launcher (clangd,
    // sourcekit-lsp, ... in /usr/bin) on a Mac without the tools counts as
    // not installed: running it would open the install dialog.
    which::which(binary)
        .ok()
        .filter(|path| !crate::developer_tools::is_unusable_launcher(path))
}

/// Resolve a server binary, adding nested Python workspace lookup before the
/// configured project-root resolver used by every other language.
pub fn resolve_server_binary(
    server: &ServerDef,
    workspace_root: Option<&Path>,
    config: &Config,
) -> Option<PathBuf> {
    if server.kind == ServerKind::Oxlint && server.binary == "oxlint" {
        // Older npm releases ship a standalone server, including versions before
        // oxlint gained --lsp (1.29). Prefer it when present, even alongside
        // oxlint, so those projects work without a version/help subprocess probe.
        for root in [workspace_root, config.project_root.as_deref()]
            .into_iter()
            .flatten()
        {
            if let Some(found) = probe_dir(
                &root.join("node_modules").join(".bin"),
                "oxc_language_server",
            ) {
                return Some(found);
            }
        }
    }
    if server.kind == ServerKind::Biome {
        if let (Some(workspace), Some(project)) = (workspace_root, config.project_root.as_deref()) {
            if let Some(found) = probe_node_bin_ancestors(&server.binary, workspace, project) {
                return Some(found);
            }
        }
    }
    let python_family = matches!(server.kind, ServerKind::Python | ServerKind::Ty);

    if python_family {
        if let Some(root) = workspace_root.or(config.project_root.as_deref()) {
            if let Some(found) = probe_project_virtualenv(root, &server.binary) {
                return Some(found);
            }
        }
        if let Some(root) = workspace_root {
            if config.project_root.as_deref() != Some(root) {
                if let Some(found) =
                    probe_dir(&root.join("node_modules").join(".bin"), &server.binary)
                {
                    return Some(found);
                }
            }
        }
    }

    // Python-family may fall back to the workspace root when no project root
    // is configured; every other language keeps the pre-existing ladder rooted
    // strictly at the configured project root (Biome only after its nearest
    // package-local lookup above found nothing).
    let project_root = if python_family {
        config.project_root.as_deref().or(workspace_root)
    } else {
        config.project_root.as_deref()
    };
    if server.kind == ServerKind::Dockerfile && server.binary == "docker-langserver" {
        if let Some(preferred) = resolve_lsp_binary(
            "docker-language-server",
            project_root,
            &config.lsp_paths_extra,
        ) {
            return Some(preferred);
        }
    }
    resolve_lsp_binary(&server.binary, project_root, &config.lsp_paths_extra)
}

/// Find `binary` in the nearest `node_modules/.bin` from `workspace_root` up
/// to and including `project_root`, the order Node resolves a package from
/// the workspace's own directory.
///
/// Biome uses this because its configuration schema follows the installed
/// version, so each package's `biome.json` must be served by that package's
/// own Biome. Package managers that install per package (Bun 1.4 workspaces,
/// pnpm without hoisting) put `biome` only in
/// `packages/<name>/node_modules/.bin`, never in the project root's, so a
/// lookup at the project root alone reported an installed Biome as missing.
/// The walk never leaves the project root, and a workspace outside it gets
/// no walk at all; the generic ladder still runs afterwards.
fn probe_node_bin_ancestors(
    binary: &str,
    workspace_root: &Path,
    project_root: &Path,
) -> Option<PathBuf> {
    let workspace_root = crate::inspect::job::canonicalize_normalized(workspace_root);
    let project_root = crate::inspect::job::canonicalize_normalized(project_root);
    if !workspace_root.starts_with(&project_root) {
        return None;
    }
    for directory in workspace_root.ancestors() {
        if let Some(found) = probe_dir(&directory.join("node_modules").join(".bin"), binary) {
            return Some(found);
        }
        if directory == project_root {
            break;
        }
    }
    None
}

fn probe_project_virtualenv(root: &Path, binary: &str) -> Option<PathBuf> {
    [root.join(".venv"), root.join("venv")]
        .into_iter()
        .find_map(|virtualenv| {
            let bin_dir = if cfg!(windows) {
                virtualenv.join("Scripts")
            } else {
                virtualenv.join("bin")
            };
            probe_dir(&bin_dir, binary)
        })
}

/// Check `dir/<binary>` and (on Windows) `dir/<binary>.cmd|.exe|.bat`.
fn probe_dir(dir: &Path, binary: &str) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }

    if cfg!(windows) {
        // npm creates both an extensionless POSIX shell shim and a `.cmd`
        // wrapper under node_modules/.bin. The extensionless shim exists on
        // Windows too but is not a Win32 executable, so prefer Windows-native
        // wrappers before falling back to the direct path.
        for ext in ["cmd", "exe", "bat"] {
            let candidate = dir.join(format!("{binary}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let direct = dir.join(binary);
    if direct.is_file() {
        return Some(direct);
    }

    None
}

/// Unique identifier for a language server kind.
///
/// IDs match OpenCode's `lsp/server.ts` registry where possible so users can
/// refer to the same names in `lsp.disabled` config across both projects.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ServerKind {
    // --- Built-in (existing, pre-v0.17.0) ---
    TypeScript,
    /// TypeScript 7 and later's own language server (`tsc --lsp --stdio`),
    /// carrying the project's `node_modules/typescript` directory. It never
    /// appears in the static registry: [`servers_for_file`] swaps it in for
    /// the `typescript` server when the file's installed TypeScript is the
    /// native compiler. The directory is part of the kind, and so of the
    /// server key, so packages with different TypeScript installations under
    /// one root get separate servers and a TypeScript 5 server never serves a
    /// TypeScript 7 package's files (or the reverse).
    TypeScriptNative(Arc<Path>),
    Python, // pyright
    Rust,
    Go,
    Bash,
    Yaml,
    Ty, // experimental Astral Python LSP
    // --- v0.17.0: PATH-only servers (Pattern A) ---
    Clojure,
    Dart,
    ElixirLs,
    FSharp,
    Gleam,
    Haskell,
    Jdtls, // Java
    Julia,
    Nixd,
    OcamlLsp,
    PhpIntelephense,
    RubyLsp,
    SourceKit, // Swift
    CSharp,
    Razor,
    // --- v0.17.0: Pattern C (PATH-first, GitHub-release auto-download in plugin) ---
    Clangd,
    LuaLs,
    Zls,
    Tinymist,
    KotlinLs,
    Texlab,
    Oxlint,
    TerraformLs,
    // --- v0.17.0: Pattern B/D (npm auto-installable in plugin) ---
    Vue,
    Astro,
    Prisma, // resolves the project's `prisma` CLI from node_modules; not auto-installed by AFT
    Biome,
    Svelte,
    Dockerfile,
    Custom(Arc<str>),
}

impl ServerKind {
    pub fn id_str(&self) -> &str {
        match self {
            Self::TypeScript => "typescript",
            Self::TypeScriptNative(_) => "typescript-native",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Go => "go",
            Self::Bash => "bash",
            Self::Yaml => "yaml",
            Self::Ty => "ty",
            // Pattern A
            Self::Clojure => "clojure-lsp",
            Self::Dart => "dart",
            Self::ElixirLs => "elixir-ls",
            Self::FSharp => "fsharp",
            Self::Gleam => "gleam",
            Self::Haskell => "haskell-language-server",
            Self::Jdtls => "jdtls",
            Self::Julia => "julials",
            Self::Nixd => "nixd",
            Self::OcamlLsp => "ocaml-lsp",
            Self::PhpIntelephense => "php-intelephense",
            Self::RubyLsp => "ruby-lsp",
            Self::SourceKit => "sourcekit-lsp",
            Self::CSharp => "csharp",
            Self::Razor => "razor",
            // Pattern C
            Self::Clangd => "clangd",
            Self::LuaLs => "lua-ls",
            Self::Zls => "zls",
            Self::Tinymist => "tinymist",
            Self::KotlinLs => "kotlin-ls",
            Self::Texlab => "texlab",
            Self::Oxlint => "oxlint",
            Self::TerraformLs => "terraform",
            // Pattern B/D
            Self::Vue => "vue",
            Self::Astro => "astro",
            Self::Prisma => "prisma",
            Self::Biome => "biome",
            Self::Svelte => "svelte",
            Self::Dockerfile => "dockerfile",
            Self::Custom(id) => id.as_ref(),
        }
    }
}

/// Definition of a language server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDef {
    pub kind: ServerKind,
    /// Display name.
    pub name: String,
    /// File extensions this server handles.
    pub extensions: Vec<String>,
    /// Binary name to look up on PATH.
    pub binary: String,
    /// Arguments to pass when spawning.
    pub args: Vec<String>,
    /// Root marker files — presence indicates a workspace root.
    pub root_markers: Vec<String>,
    /// Higher-priority root markers checked before fallback markers.
    ///
    /// Pyright uses this for configuration files because, in language-server
    /// mode, Pyright looks for pyrightconfig.json and pyproject.toml only in
    /// the workspace root supplied by the client. A nearer fallback marker like
    /// requirements.txt must not hide the directory that contains the actual
    /// Pyright configuration.
    pub priority_root_markers: Vec<String>,
    /// Extra environment variables for this server process.
    pub env: HashMap<String, String>,
    /// Optional JSON initializationOptions for the initialize request.
    pub initialization_options: Option<serde_json::Value>,
}

impl ServerDef {
    /// Return executable names in preference order. Docker's upstream server
    /// takes precedence over the older npm server, which remains the fallback.
    pub fn binary_candidates(&self) -> Vec<String> {
        if self.kind == ServerKind::Dockerfile && self.binary == "docker-langserver" {
            vec!["docker-language-server".to_string(), self.binary.clone()]
        } else {
            vec![self.binary.clone()]
        }
    }

    /// The standalone Oxlint server uses stdio without the CLI's --lsp flag.
    pub fn spawn_args_for_binary(&self, binary: &Path) -> Vec<String> {
        if self.kind == ServerKind::Dockerfile
            && self.binary == "docker-langserver"
            && matches!(
                binary.file_name().and_then(|name| name.to_str()),
                Some(
                    "docker-language-server"
                        | "docker-language-server.cmd"
                        | "docker-language-server.exe"
                        | "docker-language-server.bat"
                )
            )
        {
            return vec!["start".to_string(), "--stdio".to_string()];
        }
        if self.kind == ServerKind::Oxlint
            && self.binary == "oxlint"
            && matches!(
                binary.file_name().and_then(|name| name.to_str()),
                Some(
                    "oxc_language_server"
                        | "oxc_language_server.cmd"
                        | "oxc_language_server.exe"
                        | "oxc_language_server.bat"
                )
            )
        {
            Vec::new()
        } else {
            self.args.clone()
        }
    }

    /// Return the workspace root this server should use for a file.
    pub fn workspace_root_for_file(&self, file_path: &Path) -> Option<PathBuf> {
        self.workspace_root_for_file_with_project_root(file_path, None)
    }

    /// Return the workspace root for a file without searching above `project_root`.
    ///
    /// Rust Analyzer indexes the full Cargo workspace passed to it. Resolve Rust
    /// member crates to their owning workspace before the generic nearest-marker
    /// fallback so sibling members share a single analyzer. Other languages keep
    /// their established priority-marker behavior unchanged.
    pub fn workspace_root_for_file_with_project_root(
        &self,
        file_path: &Path,
        project_root: Option<&Path>,
    ) -> Option<PathBuf> {
        if self.kind == ServerKind::Rust {
            if let Some(root) = find_rust_workspace_root(file_path, project_root) {
                return Some(root);
            }
        }

        let bounded_to_project = matches!(
            self.kind,
            ServerKind::Rust | ServerKind::Python | ServerKind::Ty
        );
        let nearest = match self.nearest_marker_root(file_path, project_root, bounded_to_project) {
            Some(nearest) => nearest,
            // A Python script with no project file above it (a helper under
            // `scripts/`, say) is still Python that Pyright can check with its
            // defaults. Serving it from the project root keeps such files from
            // being permanently "no workspace root" gaps in a scoped inspect.
            None if matches!(self.kind, ServerKind::Python | ServerKind::Ty) => {
                let project_root = crate::inspect::job::canonicalize_normalized(project_root?);
                crate::inspect::job::canonicalize_normalized(file_path)
                    .starts_with(&project_root)
                    .then_some(project_root)?
            }
            None => return None,
        };
        let Some(project_root) = project_root else {
            return Some(nearest);
        };
        match self.kind {
            // One TypeScript language server serves every tsconfig project
            // below its root: tsserver finds the nearest tsconfig of each file
            // it opens. Packages share a server as long as they see the same
            // TypeScript version, so a monorepo whose packages each install
            // the same TypeScript runs one server instead of one per package.
            ServerKind::TypeScript => {
                Some(super::typescript_project::shared_typescript_server_root(
                    &nearest,
                    project_root,
                    |dir| self.has_root_marker(dir),
                ))
            }
            // Bash and YAML servers analyze each file on its own; their
            // workspace root only bounds background indexing. Their markers
            // (`package.json`, `.git`) mark every JavaScript package, which
            // started one server per package for the same project.
            ServerKind::Bash | ServerKind::Yaml => {
                Some(outermost_marker_root(&nearest, project_root, |dir| {
                    self.has_root_marker(dir)
                }))
            }
            _ => Some(nearest),
        }
    }

    fn has_root_marker(&self, dir: &Path) -> bool {
        self.root_markers
            .iter()
            .chain(self.priority_root_markers.iter())
            .any(|marker| dir.join(marker).exists())
    }

    fn nearest_marker_root(
        &self,
        file_path: &Path,
        project_root: Option<&Path>,
        bounded_to_project: bool,
    ) -> Option<PathBuf> {
        for marker in &self.priority_root_markers {
            let root = if bounded_to_project {
                find_workspace_root_within(file_path, &[marker.as_str()], project_root)
            } else {
                find_workspace_root(file_path, &[marker.as_str()])
            };
            if let Some(root) = root {
                return Some(root);
            }
        }

        if bounded_to_project {
            find_workspace_root_within(file_path, &self.root_markers, project_root)
        } else {
            find_workspace_root(file_path, &self.root_markers)
        }
    }

    /// Check if this server handles a given file extension.
    pub fn matches_extension(&self, ext: &str) -> bool {
        self.extensions
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    }

    /// Check if the server binary is available on PATH.
    pub fn is_available(&self) -> bool {
        which::which(&self.binary)
            .is_ok_and(|path| !crate::developer_tools::is_unusable_launcher(&path))
    }
}

#[cfg(test)]
static BUILTIN_SERVER_BUILDS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Built-in server definitions.
pub fn builtin_servers() -> Vec<ServerDef> {
    #[cfg(test)]
    BUILTIN_SERVER_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    vec![
        builtin_server(
            ServerKind::TypeScript,
            "TypeScript Language Server",
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
            "typescript-language-server",
            &["--stdio"],
            &["tsconfig.json", "jsconfig.json", "package.json"],
        ),
        builtin_server_with_priority_roots(
            ServerKind::Python,
            "Pyright",
            &["py", "pyi"],
            "pyright-langserver",
            &["--stdio"],
            &[
                "pyrightconfig.json",
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
            ],
            &["pyrightconfig.json", "pyproject.toml"],
        ),
        builtin_server_with_init(
            ServerKind::Rust,
            "rust-analyzer",
            &["rs"],
            "rust-analyzer",
            &[],
            &["Cargo.toml", "Cargo.lock"],
            // Lock metadata resolution and build-script discovery/flycheck so a
            // stale Cargo.lock fails analysis instead of being rewritten by Cargo.
            serde_json::json!({ "checkOnSave": false, "cargo": {
                "extraArgs": ["--locked"],
                "metadataExtraArgs": ["--locked"]
            } }),
        ),
        // gopls requires opt-in for `textDocument/diagnostic` (LSP 3.17 pull)
        // via the `pullDiagnostics` initializationOption. Without this the
        // server still publishes via push but ignores pull requests.
        // See https://github.com/golang/tools/blob/master/gopls/doc/settings.md
        builtin_server_with_init(
            ServerKind::Go,
            "gopls",
            &["go"],
            "gopls",
            &["serve"],
            &["go.mod", "go.sum"],
            serde_json::json!({ "pullDiagnostics": true }),
        ),
        builtin_server(
            ServerKind::Bash,
            "bash-language-server",
            &["sh", "bash", "zsh"],
            "bash-language-server",
            &["start"],
            &["package.json", ".git"],
        ),
        builtin_server(
            ServerKind::Yaml,
            "yaml-language-server",
            &["yaml", "yml"],
            "yaml-language-server",
            &["--stdio"],
            &["package.json", ".git"],
        ),
        builtin_server(
            ServerKind::Ty,
            "ty",
            &["py", "pyi"],
            "ty",
            &["server"],
            &[
                "pyproject.toml",
                "ty.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
                "Pipfile",
                "pyrightconfig.json",
            ],
        ),
        // ===== Pattern A: PATH-only servers =====
        // These servers are not auto-installed by AFT (the toolchain itself
        // ships the LSP, e.g. `dart`, `gleam`; or installation is highly
        // platform-specific, e.g. `jdtls`). Users install via system package
        // manager / language toolchain. AFT registers the def so users with
        // the binary on PATH get LSP coverage.
        builtin_server(
            ServerKind::Clojure,
            "clojure-lsp",
            &["clj", "cljs", "cljc", "edn"],
            "clojure-lsp",
            &[],
            &[
                "deps.edn",
                "project.clj",
                "shadow-cljs.edn",
                "bb.edn",
                "build.boot",
            ],
        ),
        builtin_server(
            ServerKind::Dart,
            "Dart Language Server",
            &["dart"],
            "dart",
            &["language-server", "--lsp"],
            &["pubspec.yaml", "analysis_options.yaml"],
        ),
        builtin_server(
            ServerKind::ElixirLs,
            "elixir-ls",
            &["ex", "exs"],
            "elixir-ls",
            &[],
            &["mix.exs", "mix.lock"],
        ),
        builtin_server(
            ServerKind::FSharp,
            "FSAutoComplete",
            &["fs", "fsi", "fsx", "fsscript"],
            "fsautocomplete",
            &[],
            &[".slnx", ".sln", ".fsproj", "global.json"],
        ),
        builtin_server(
            ServerKind::Gleam,
            "Gleam Language Server",
            &["gleam"],
            "gleam",
            &["lsp"],
            &["gleam.toml"],
        ),
        builtin_server(
            ServerKind::Haskell,
            "haskell-language-server",
            &["hs", "lhs"],
            "haskell-language-server-wrapper",
            &["--lsp"],
            &["stack.yaml", "cabal.project", "hie.yaml"],
        ),
        builtin_server(
            ServerKind::Jdtls,
            "Eclipse JDT Language Server",
            &["java"],
            "jdtls",
            &[],
            &["pom.xml", "build.gradle", "build.gradle.kts", ".project"],
        ),
        builtin_server(
            ServerKind::Julia,
            "Julia Language Server",
            &["jl"],
            "julia",
            &[
                "--startup-file=no",
                "--history-file=no",
                "-e",
                "using LanguageServer; runserver()",
            ],
            &["Project.toml", "Manifest.toml"],
        ),
        builtin_server(
            ServerKind::Nixd,
            "nixd",
            &["nix"],
            "nixd",
            &[],
            &["flake.nix", "default.nix", "shell.nix"],
        ),
        builtin_server(
            ServerKind::OcamlLsp,
            "ocaml-lsp",
            &["ml", "mli"],
            "ocamllsp",
            &[],
            &["dune-project", "dune-workspace", ".merlin", "opam"],
        ),
        builtin_server(
            ServerKind::PhpIntelephense,
            "Intelephense",
            &["php"],
            "intelephense",
            &["--stdio"],
            &["composer.json", "composer.lock", ".php-version"],
        ),
        builtin_server(
            ServerKind::RubyLsp,
            "ruby-lsp",
            &["rb", "rake", "gemspec", "ru"],
            "ruby-lsp",
            &[],
            &["Gemfile"],
        ),
        builtin_server(
            ServerKind::SourceKit,
            "SourceKit-LSP",
            &["swift"],
            "sourcekit-lsp",
            &[],
            &["Package.swift"],
        ),
        builtin_server(
            ServerKind::CSharp,
            "Roslyn Language Server",
            &["cs", "csx"],
            "roslyn-language-server",
            &[],
            &[".slnx", ".sln", ".csproj", "global.json"],
        ),
        builtin_server(
            ServerKind::Razor,
            "rzls",
            &["razor", "cshtml"],
            "rzls",
            &[],
            &[".slnx", ".sln", ".csproj", "global.json"],
        ),
        // ===== Pattern C: PATH-first; plugin auto-downloads from GitHub releases =====
        builtin_server(
            ServerKind::Clangd,
            "clangd",
            &[
                "c", "cpp", "cc", "cxx", "c++", "h", "hpp", "hh", "hxx", "h++",
            ],
            "clangd",
            &[],
            &["compile_commands.json", "compile_flags.txt", ".clangd"],
        ),
        builtin_server(
            ServerKind::LuaLs,
            "lua-language-server",
            &["lua"],
            "lua-language-server",
            &[],
            &[".luarc.json", ".luarc.jsonc", ".stylua.toml", "stylua.toml"],
        ),
        builtin_server(
            ServerKind::Zls,
            "zls",
            &["zig", "zon"],
            "zls",
            &[],
            &["build.zig"],
        ),
        builtin_server(
            ServerKind::Tinymist,
            "tinymist",
            &["typ", "typc"],
            "tinymist",
            &[],
            &["typst.toml"],
        ),
        builtin_server(
            ServerKind::KotlinLs,
            "kotlin-language-server",
            &["kt", "kts"],
            "kotlin-language-server",
            &[],
            &["settings.gradle", "settings.gradle.kts", "build.gradle"],
        ),
        builtin_server(
            ServerKind::Texlab,
            "texlab",
            &["tex", "bib"],
            "texlab",
            &[],
            &[".latexmkrc", "latexmkrc", ".texlabroot", "texlabroot"],
        ),
        builtin_server(
            ServerKind::Oxlint,
            "oxlint",
            // Same JS/TS family as TypeScript LS; coexists rather than replaces.
            &[
                "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "vue", "astro", "svelte",
            ],
            "oxlint",
            &["--lsp"],
            // Only trigger on actual oxlint config files. We previously also
            // matched `package.json`, but that fired oxc on every JS/TS project
            // whether they used oxlint or not, producing a persistent warning
            // for the (overwhelmingly common) case where the user never opted
            // into oxlint. Users who run oxlint will have one of these config
            // files; everyone else gets silence.
            &[".oxlintrc.json", ".oxlintrc"],
        ),
        builtin_server(
            ServerKind::TerraformLs,
            "terraform-ls",
            &["tf", "tfvars"],
            "terraform-ls",
            &["serve"],
            &[".terraform.lock.hcl", "terraform.tfstate"],
        ),
        // ===== Pattern B/D: PATH-first; plugin auto-installs from npm =====
        // Order matters slightly: vue/svelte/astro use TypeScript-family
        // extensions when paired with their primary file extension. Each
        // server only runs against its own primary extension here; agents
        // run TypeScript LS for the rest.
        builtin_server(
            ServerKind::Vue,
            "Vue Language Server",
            &["vue"],
            "vue-language-server",
            &["--stdio"],
            &[
                "package-lock.json",
                "bun.lockb",
                "bun.lock",
                "pnpm-lock.yaml",
                "yarn.lock",
            ],
        ),
        builtin_server(
            ServerKind::Astro,
            "Astro Language Server",
            &["astro"],
            "astro-ls",
            &["--stdio"],
            &[
                "astro.config.js",
                "astro.config.mjs",
                "astro.config.ts",
                "astro.config.cjs",
                "package.json",
                "package-lock.json",
                "bun.lockb",
                "bun.lock",
                "pnpm-lock.yaml",
                "yarn.lock",
            ],
        ),
        // Prisma's LSP runs via `prisma language-server` from the project's
        // own `prisma` CLI (resolved through node_modules/.bin). AFT does NOT
        // auto-install the prisma package — users get LSP coverage when their
        // project has prisma as a devDependency.
        builtin_server(
            ServerKind::Prisma,
            "Prisma Language Server",
            &["prisma"],
            "prisma",
            &["language-server"],
            &["schema.prisma", "package.json"],
        ),
        // Biome: lint+format LSP for the JS/TS family. Coexists with the
        // TypeScript Language Server (different responsibilities). Disable
        // via `lsp.disabled: ["biome"]` when not desired.
        builtin_server(
            ServerKind::Biome,
            "Biome",
            &[
                "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "json", "jsonc",
            ],
            "biome",
            &["lsp-proxy"],
            &["biome.json", "biome.jsonc"],
        ),
        builtin_server(
            ServerKind::Svelte,
            "Svelte Language Server",
            &["svelte"],
            "svelteserver",
            &["--stdio"],
            &[
                "package-lock.json",
                "bun.lockb",
                "bun.lock",
                "pnpm-lock.yaml",
                "yarn.lock",
            ],
        ),
        builtin_server(
            ServerKind::Dockerfile,
            "Dockerfile Language Server",
            // OpenCode special-cases the literal "Dockerfile" name; AFT's
            // extension-only matcher cannot. Users can `aft_outline`/edit
            // Dockerfiles by extension `.dockerfile`. Plain `Dockerfile`
            // files won't auto-trigger LSP — acknowledged limitation; can
            // be revisited if users complain.
            &["dockerfile"],
            "docker-langserver",
            &["--stdio"],
            &["Dockerfile", "dockerfile", ".dockerignore"],
        ),
        // NOTE: ESLint LSP intentionally not registered — OpenCode resolves it
        // through `Module.resolve("eslint", root)` and runs custom server-side
        // logic. AFT does not implement that flow yet; users with ESLint can
        // run `eslint --fix` via bash.
    ]
}

/// Find all server definitions that handle a given file path.
pub fn servers_for_file(path: &Path, config: &Config) -> Vec<ServerDef> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();

    // Without user server entries the definitions are the built-in ones,
    // built once per process; only those handling this extension are cloned.
    // Building all of them per call cost dozens of allocations for every
    // file an LSP path looked at.
    let candidates: Vec<ServerDef> = if config.lsp_servers.is_empty() {
        builtin_servers_shared()
            .iter()
            .filter(|server| server.matches_extension(extension))
            .cloned()
            .collect()
    } else {
        resolved_servers(config)
            .into_iter()
            .filter(|server| server.matches_extension(extension))
            .collect()
    };
    candidates
        .into_iter()
        .filter(|server| !is_disabled(server, config))
        .filter(|server| config.experimental_lsp_ty || server.kind != ServerKind::Ty)
        // The swap runs after the disabled filter, so `lsp.disabled:
        // ["typescript"]` still turns TypeScript off for native projects;
        // the second filter lets `typescript-native` be disabled on its own.
        .map(|server| select_typescript_server(server, path, config))
        .filter(|server| !is_disabled(server, config))
        .filter(|server| !biome_config_excludes(server, path))
        .collect()
}

/// Biome analyzes only the files its configuration's `files.includes` lists
/// and publishes nothing for the rest. Treating Biome as a producer for an
/// excluded file (this repository's biome.json lists only package sources,
/// so every `.json` under `crates/` is excluded) left that file a permanent
/// "published no diagnostics" gap in every scoped inspect. A configuration
/// without `files.includes`, or one that cannot be read, includes every file.
fn biome_config_excludes(server: &ServerDef, path: &Path) -> bool {
    if server.kind != ServerKind::Biome {
        return false;
    }
    let Some(root) = server.workspace_root_for_file(path) else {
        return false;
    };
    let Some(config_path) = ["biome.json", "biome.jsonc"]
        .iter()
        .map(|name| root.join(name))
        .find(|candidate| candidate.is_file())
    else {
        return false;
    };
    let Some(includes) = biome_includes(&config_path) else {
        return false;
    };
    let path = crate::inspect::job::canonicalize_normalized(path);
    let Ok(relative) = path.strip_prefix(&root) else {
        return false;
    };
    !includes.includes(relative)
}

/// A Biome `files.includes` list: a file is included when a positive pattern
/// matches it and no later `!` pattern does.
struct BiomeIncludes {
    patterns: Vec<(bool, globset::GlobMatcher)>,
}

impl BiomeIncludes {
    fn includes(&self, relative: &Path) -> bool {
        let mut included = false;
        for (negated, matcher) in &self.patterns {
            if matcher.is_match(relative) {
                included = !negated;
            }
        }
        included
    }
}

/// Parsed `files.includes` of one Biome configuration, memoized by path and
/// modification time so a walk over thousands of files reads it once.
fn biome_includes(config_path: &Path) -> Option<Arc<BiomeIncludes>> {
    type Memo = parking_lot::Mutex<
        HashMap<PathBuf, (Option<std::time::SystemTime>, Option<Arc<BiomeIncludes>>)>,
    >;
    static MEMO: OnceLock<Memo> = OnceLock::new();
    let modified = std::fs::metadata(config_path)
        .and_then(|metadata| metadata.modified())
        .ok();
    let memo = MEMO.get_or_init(Default::default);
    if let Some((at, includes)) = memo.lock().get(config_path) {
        if *at == modified {
            return includes.clone();
        }
    }
    let parsed = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|source| {
            serde_json::from_str::<serde_json::Value>(&crate::jsonc::strip_jsonc(&source)).ok()
        })
        .and_then(|config| {
            let patterns = config.pointer("/files/includes")?.as_array()?;
            let patterns = patterns
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|pattern| {
                    let negated = pattern.starts_with('!');
                    let glob = pattern.trim_start_matches('!');
                    let matcher = globset::GlobBuilder::new(glob)
                        .literal_separator(true)
                        .build()
                        .ok()?
                        .compile_matcher();
                    Some((negated, matcher))
                })
                .collect::<Vec<_>>();
            Some(Arc::new(BiomeIncludes { patterns }))
        });
    memo.lock()
        .insert(config_path.to_path_buf(), (modified, parsed.clone()));
    parsed
}

const TYPESCRIPT_LANGUAGE_SERVER: &str = "typescript-language-server";

/// Serve a TypeScript 7+ file with the native compiler's own language server.
///
/// `typescript-language-server` needs the `lib/tsserver.js` that TypeScript 7
/// no longer ships, so for a file whose nearest installed TypeScript is the
/// native compiler the `typescript` definition becomes a
/// [`ServerKind::TypeScriptNative`] one that runs `tsc --lsp --stdio`. The
/// choice is per file because one project can mix TypeScript versions across
/// packages. The binary shown here is the platform binary when it resolves;
/// the manager resolves it again at spawn and reports a named gap, without
/// spawning anything, when it does not.
fn select_typescript_server(server: ServerDef, path: &Path, config: &Config) -> ServerDef {
    // Only the built-in program is swapped: a user who pointed the
    // `typescript` server at another binary has chosen their own server.
    if server.kind != ServerKind::TypeScript || server.binary != TYPESCRIPT_LANGUAGE_SERVER {
        return server;
    }
    let project_root = config.project_root.as_deref();
    let markers = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        server.root_markers.hash(&mut hasher);
        server.priority_root_markers.hash(&mut hasher);
        hasher.finish()
    };
    let Some(native) = native_typescript_for(path, project_root, markers, || {
        server.workspace_root_for_file_with_project_root(path, project_root)
    }) else {
        return server;
    };
    let binary = native
        .binary
        .map(|binary| binary.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tsc".into());
    ServerDef {
        kind: ServerKind::TypeScriptNative(Arc::from(native.project.package_dir.as_path())),
        name: "TypeScript native language server".into(),
        extensions: server.extensions,
        binary,
        args: NATIVE_SERVER_ARGS.map(String::from).to_vec(),
        root_markers: server.root_markers,
        priority_root_markers: server.priority_root_markers,
        env: server.env,
        // Options configured for typescript-language-server (a tsserver.path,
        // its preferences) mean nothing to the native server.
        initialization_options: None,
    }
}

/// Find every enabled server definition for which `path` is a root marker that
/// signals the server's language is in use (for example `package.json` for the
/// TypeScript server). A marker only says where a server's workspace would
/// start; it does not mean the server has any files to analyze.
pub fn servers_with_root_marker(path: &Path, config: &Config) -> Vec<ServerDef> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };

    resolved_servers(config)
        .into_iter()
        .filter(|server| !is_disabled(server, config))
        .filter(|server| config.experimental_lsp_ty || server.kind != ServerKind::Ty)
        .filter(|server| {
            server
                .root_markers
                .iter()
                .chain(server.priority_root_markers.iter())
                .any(|marker| marker == file_name)
        })
        .filter(|server| marker_signals_language(file_name, server))
        .collect()
}

/// Several servers borrow generic files as workspace-root fallbacks: Bash and
/// YAML use `package.json` and `.git`, Vue and Astro use package-manager
/// lockfiles, Prisma uses `package.json`. Those files say a JavaScript
/// package lives here, not that shell scripts or Vue components do, so they
/// signal only servers that handle JavaScript or TypeScript files; `.git`
/// signals nothing. Every other marker (`Cargo.toml`, `tsconfig.json`,
/// `astro.config.mjs`, ...) is specific to its server.
fn marker_signals_language(marker: &str, server: &ServerDef) -> bool {
    const JS_PACKAGE_FILES: &[&str] = &[
        "package.json",
        "package-lock.json",
        "bun.lock",
        "bun.lockb",
        "pnpm-lock.yaml",
        "yarn.lock",
    ];
    if marker == ".git" {
        return false;
    }
    if JS_PACKAGE_FILES.contains(&marker) {
        return server.matches_extension("js") || server.matches_extension("ts");
    }
    true
}

/// Resolve the full server set after applying user overrides.
///
/// When a user-defined server's `id` matches a built-in server's `id_str()`
/// (e.g. `lsp.servers.clangd`), the user entry REPLACES the built-in entry
/// rather than registering alongside it. Fields the user left at their
/// default value (empty array, empty string, empty map, None) are inherited
/// from the built-in so users only have to specify what they actually want
/// to override.
/// Initialization options instead merge JSON objects recursively, with explicit
/// arrays and scalars replacing built-in values, including empty arrays.
///
/// User-defined servers whose `id` does not match any built-in are appended
/// as `ServerKind::Custom(id)` with no merging — they're standalone.
fn resolved_servers(config: &Config) -> Vec<ServerDef> {
    let mut servers = builtin_servers();
    for user in &config.lsp_servers {
        if user.disabled {
            // Disabled user override means "drop the matching built-in if any"
            // — equivalent to adding the id to `lsp.disabled`. We don't include
            // it in the result regardless of whether it matched.
            servers.retain(|s| s.kind.id_str() != user.id);
            continue;
        }
        if let Some(position) = servers.iter().position(|s| s.kind.id_str() == user.id) {
            // Replace the built-in with a merged ServerDef. Keep the built-in
            // `kind` so callers that match on enum variants (e.g. cap probing
            // for `ServerKind::Go`) continue to work. Inherit any field the
            // user left at its default value.
            let builtin = &servers[position];
            let merged = ServerDef {
                kind: builtin.kind.clone(),
                name: builtin.name.clone(),
                extensions: if user.extensions.is_empty() {
                    builtin.extensions.clone()
                } else {
                    user.extensions.clone()
                },
                binary: if user.binary.is_empty() {
                    builtin.binary.clone()
                } else {
                    user.binary.clone()
                },
                args: if user.args.is_empty() {
                    builtin.args.clone()
                } else {
                    user.args.clone()
                },
                root_markers: if user.root_markers.is_empty() {
                    builtin.root_markers.clone()
                } else {
                    user.root_markers.clone()
                },
                priority_root_markers: if user.root_markers.is_empty() {
                    builtin.priority_root_markers.clone()
                } else {
                    Vec::new()
                },
                env: if user.env.is_empty() {
                    builtin.env.clone()
                } else {
                    user.env.clone()
                },
                initialization_options: match (
                    builtin.initialization_options.clone(),
                    user.initialization_options.clone(),
                ) {
                    (Some(mut base), Some(overrides)) => {
                        super::manager::merge_json_override(&mut base, overrides);
                        Some(base)
                    }
                    (base, overrides) => overrides.or(base),
                },
            };
            servers[position] = merged;
        } else if let Some(def) = custom_server(user) {
            servers.push(def);
        }
    }
    servers
}

/// Returns true when `path` is a project configuration file whose changes can
/// affect an LSP server's workspace/project graph, even if the edited file
/// itself is not a source file handled by that server.
pub fn is_config_file_path(path: &Path) -> bool {
    const IGNORED_COMPONENTS: &[&str] = &[
        "node_modules",
        "target",
        "vendor",
        ".git",
        "dist",
        "build",
        ".next",
        ".nuxt",
        "__pycache__",
    ];

    if path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| IGNORED_COMPONENTS.contains(&name))
    }) {
        return false;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    // Lockfiles appear in root_markers for workspace-detection but should NOT
    // trigger didChangeWatchedFiles notifications — they are regenerated by
    // package managers constantly and notifying LSP servers on every install
    // creates unnecessary churn without affecting language analysis.
    // Intentional: this list is checked BEFORE builtin_config_file_names so a
    // file that is both a root_marker and a lockfile is excluded.
    //
    // Cargo.lock is deliberately absent. AFT starts rust-analyzer with
    // `--locked`, so when Cargo.toml gains or changes a dependency the old
    // lockfile no longer matches and `cargo metadata` fails; rust-analyzer
    // then loads the workspace without its dependencies and re-reads the
    // lockfile only when told it changed. So a lockfile update is a
    // project-graph change for Rust, not install churn.
    const LOCKFILE_NAMES: &[&str] = &[
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Gemfile.lock",
        "poetry.lock",
        "go.sum",
        "bun.lock",
        "bun.lockb",
    ];
    if LOCKFILE_NAMES.contains(&file_name) {
        return false;
    }

    builtin_config_file_names().contains(file_name)
        || (file_name.starts_with("tsconfig.") && file_name.ends_with(".json"))
}

/// Extended variant that also considers root_markers from user-configured
/// custom LSP servers (#25). Call this from contexts where Config is available.
/// Falls back to `is_config_file_path` when `extra_markers` is empty.
pub fn is_config_file_path_with_custom(path: &Path, extra_markers: &[String]) -> bool {
    if is_config_file_path(path) {
        return true;
    }
    if extra_markers.is_empty() {
        return false;
    }
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    extra_markers.iter().any(|m| m == file_name)
}

/// The built-in server definitions, built once. They depend on nothing but
/// this source file.
fn builtin_servers_shared() -> &'static [ServerDef] {
    static SERVERS: OnceLock<Vec<ServerDef>> = OnceLock::new();
    SERVERS.get_or_init(builtin_servers)
}

fn builtin_config_file_names() -> &'static HashSet<String> {
    static NAMES: OnceLock<HashSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        builtin_servers()
            .into_iter()
            .flat_map(|server| server.root_markers)
            .collect()
    })
}

fn builtin_server(
    kind: ServerKind,
    name: &str,
    extensions: &[&str],
    binary: &str,
    args: &[&str],
    root_markers: &[&str],
) -> ServerDef {
    ServerDef {
        kind,
        name: name.to_string(),
        extensions: strings(extensions),
        binary: binary.to_string(),
        args: strings(args),
        root_markers: strings(root_markers),
        priority_root_markers: Vec::new(),
        env: HashMap::new(),
        initialization_options: None,
    }
}

/// Builder variant of [`builtin_server`] that checks some markers before
/// fallback root markers even when the fallback marker is closer to the file.
fn builtin_server_with_priority_roots(
    kind: ServerKind,
    name: &str,
    extensions: &[&str],
    binary: &str,
    args: &[&str],
    root_markers: &[&str],
    priority_root_markers: &[&str],
) -> ServerDef {
    let mut def = builtin_server(kind, name, extensions, binary, args, root_markers);
    def.priority_root_markers = strings(priority_root_markers);
    def
}

fn builtin_server_with_init(
    kind: ServerKind,
    name: &str,
    extensions: &[&str],
    binary: &str,
    args: &[&str],
    root_markers: &[&str],
    initialization_options: serde_json::Value,
) -> ServerDef {
    let mut def = builtin_server(kind, name, extensions, binary, args, root_markers);
    def.initialization_options = Some(initialization_options);
    def
}

fn custom_server(server: &UserServerDef) -> Option<ServerDef> {
    if server.disabled {
        return None;
    }

    Some(ServerDef {
        kind: ServerKind::Custom(Arc::from(server.id.as_str())),
        name: server.id.clone(),
        extensions: server.extensions.clone(),
        binary: server.binary.clone(),
        args: server.args.clone(),
        root_markers: server.root_markers.clone(),
        priority_root_markers: Vec::new(),
        env: server.env.clone(),
        initialization_options: server.initialization_options.clone(),
    })
}

fn is_disabled(server: &ServerDef, config: &Config) -> bool {
    config
        .disabled_lsp
        .contains(&server.kind.id_str().to_ascii_lowercase())
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::{
        builtin_servers, is_config_file_path, resolve_lsp_binary, resolve_server_binary,
        servers_for_file, ServerDef, ServerKind,
    };
    use crate::config::{Config, UserServerDef};

    /// `servers_for_file` runs for every file an LSP path looks at; it must
    /// not rebuild every built-in definition each time.
    #[test]
    fn servers_for_file_does_not_rebuild_the_builtin_definitions_per_call() {
        use std::sync::atomic::Ordering;
        let config = Config::default();
        // Build the shared list first so the measured calls only read it.
        let expected = matching_kinds("/tmp/project/src/main.ts", &config);
        let before = super::BUILTIN_SERVER_BUILDS.load(Ordering::Relaxed);
        for _ in 0..1_000 {
            assert_eq!(
                matching_kinds("/tmp/project/src/main.ts", &config),
                expected
            );
        }
        let builds = super::BUILTIN_SERVER_BUILDS.load(Ordering::Relaxed) - before;
        // Other tests in this process may build the list directly meanwhile;
        // a per-call rebuild would add a thousand.
        assert!(
            builds < 100,
            "1000 lookups rebuilt the definitions {builds} times"
        );
    }

    fn matching_kinds(path: &str, config: &Config) -> Vec<ServerKind> {
        servers_for_file(Path::new(path), config)
            .into_iter()
            .map(|server| server.kind)
            .collect()
    }

    fn write_file(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn install_typescript(package_parent: &Path, version: &str) -> PathBuf {
        let package_dir = package_parent.join("node_modules").join("typescript");
        write_file(
            &package_dir.join("package.json"),
            &format!(r#"{{"name":"typescript","version":"{version}"}}"#),
        );
        package_dir
    }

    fn typescript_server(file: &Path, config: &Config) -> Option<super::ServerDef> {
        servers_for_file(file, config).into_iter().find(|server| {
            matches!(
                server.kind,
                ServerKind::TypeScript | ServerKind::TypeScriptNative(_)
            )
        })
    }

    /// Packages that each install the same TypeScript share one server at the
    /// highest root that still sees that version; a package with a different
    /// version keeps its own server root.
    #[test]
    fn typescript_packages_with_the_same_version_share_one_server_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(&root.join("package.json"), "{}");
        install_typescript(&root, "5.9.3");
        for package in ["cli", "plugin", "old"] {
            write_file(
                &root.join("packages").join(package).join("package.json"),
                "{}",
            );
            write_file(
                &root.join("packages").join(package).join("tsconfig.json"),
                "{}",
            );
            write_file(
                &root.join("packages").join(package).join("src/index.ts"),
                "",
            );
        }
        install_typescript(&root.join("packages/cli"), "5.9.3");
        install_typescript(&root.join("packages/old"), "5.4.5");
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let server_root = |package: &str| {
            let file = root.join("packages").join(package).join("src/index.ts");
            let server = typescript_server(&file, &config).unwrap();
            assert_eq!(server.kind, ServerKind::TypeScript);
            server
                .workspace_root_for_file_with_project_root(&file, Some(&root))
                .unwrap()
        };
        assert_eq!(server_root("cli"), root);
        assert_eq!(server_root("plugin"), root);
        assert_eq!(server_root("old"), root.join("packages/old"));
    }

    /// Biome is not a producer for a file its `files.includes` leaves out, so
    /// such a file is not an eternal "no diagnostics published" gap.
    #[test]
    fn biome_is_not_a_producer_for_files_its_config_excludes() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(
            &root.join("biome.json"),
            r#"{"files":{"includes":["packages/**/*.ts","!packages/gen/**"]}}"#,
        );
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let has_biome = |relative: &str| {
            let file = root.join(relative);
            write_file(&file, "");
            matching_kinds(file.to_str().unwrap(), &config).contains(&ServerKind::Biome)
        };
        assert!(has_biome("packages/app/src/index.ts"));
        assert!(!has_biome("crates/app/schema.json"));
        assert!(!has_biome("packages/gen/out.ts"));
    }

    /// A Python script with no project file above it is served from the
    /// project root instead of having no server at all.
    #[test]
    fn python_script_without_a_project_file_uses_the_project_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        let script = root.join("scripts/report.py");
        write_file(&script, "print(1)\n");
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let python = servers_for_file(&script, &config)
            .into_iter()
            .find(|server| server.kind == ServerKind::Python)
            .unwrap();
        assert_eq!(
            python.workspace_root_for_file_with_project_root(&script, Some(&root)),
            Some(root.clone())
        );
        assert_eq!(python.workspace_root_for_file(&script), None);
    }

    /// Bash marks every JavaScript package (`package.json`) as a root; one
    /// server at the outermost root in the project serves all of them.
    #[test]
    fn bash_scripts_in_nested_packages_share_the_outermost_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(&root.join("package.json"), "{}");
        write_file(&root.join("packages/cli/package.json"), "{}");
        let script = root.join("packages/cli/scripts/release.sh");
        write_file(&script, "echo hi\n");
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let bash = servers_for_file(&script, &config)
            .into_iter()
            .find(|server| server.kind == ServerKind::Bash)
            .unwrap();
        assert_eq!(
            bash.workspace_root_for_file_with_project_root(&script, Some(&root)),
            Some(root.clone())
        );
        // Without a project root there is nothing to bound the climb, so the
        // nearest marker stays the root.
        assert_eq!(
            bash.workspace_root_for_file(&script),
            Some(root.join("packages/cli"))
        );
    }

    /// One root can hold packages with different TypeScript installations.
    /// The native kind carries its TypeScript directory, so the two files get
    /// different server keys even though their workspace root is the same.
    #[test]
    fn typescript_7_files_get_the_native_server_keyed_by_their_typescript() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(&root.join("tsconfig.json"), "{}");
        let native_dir = install_typescript(&root.join("next"), "7.0.2");
        install_typescript(&root.join("legacy"), "5.9.3");
        let native_file = root.join("next").join("index.ts");
        let legacy_file = root.join("legacy").join("index.ts");
        write_file(&native_file, "");
        write_file(&legacy_file, "");
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };

        let native = typescript_server(&native_file, &config).unwrap();
        assert_eq!(
            native.kind,
            ServerKind::TypeScriptNative(Arc::from(native_dir.as_path()))
        );
        assert_eq!(native.kind.id_str(), "typescript-native");
        assert_eq!(native.args, vec!["--lsp", "--stdio"]);
        assert_eq!(native.initialization_options, None);
        let legacy = typescript_server(&legacy_file, &config).unwrap();
        assert_eq!(legacy.kind, ServerKind::TypeScript);
        assert_eq!(legacy.binary, "typescript-language-server");

        // Same workspace root, different keys.
        let config_root = config.project_root.as_deref();
        assert_eq!(
            native.workspace_root_for_file_with_project_root(&native_file, config_root),
            legacy.workspace_root_for_file_with_project_root(&legacy_file, config_root),
        );
        assert_ne!(native.kind, legacy.kind);
    }

    /// Choosing the TypeScript server for many files in one directory reads
    /// the installed package once, not once per file. Bypassing the memo makes
    /// this 200 reads or more; the bound leaves room for a parallel test that
    /// clears the memo mid-loop.
    #[test]
    fn typescript_server_choice_is_memoized_per_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(&root.join("package.json"), "{}");
        install_typescript(&root, "7.0.2");
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let files: Vec<PathBuf> = (0..100)
            .map(|index| root.join("src").join(format!("file{index}.ts")))
            .collect();
        for file in &files {
            write_file(file, "");
        }
        let before = crate::lsp::typescript_project::package_json_reads_on_this_thread();
        for file in &files {
            let server = typescript_server(file, &config).unwrap();
            assert!(matches!(server.kind, ServerKind::TypeScriptNative(_)));
        }
        let reads = crate::lsp::typescript_project::package_json_reads_on_this_thread() - before;
        assert!(
            reads <= 10,
            "{reads} package.json reads for 100 files in one directory"
        );
    }

    /// After the watcher reports an installation change, the TypeScript
    /// server choice is recomputed.
    #[test]
    fn typescript_server_choice_follows_an_install_after_invalidation() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(&root.join("package.json"), "{}");
        let package_dir = install_typescript(&root, "5.9.3");
        let file = root.join("index.ts");
        write_file(&file, "");
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        assert_eq!(
            typescript_server(&file, &config).unwrap().kind,
            ServerKind::TypeScript
        );
        install_typescript(&root, "7.0.2");
        let changed = package_dir.join("package.json");
        assert!(crate::lsp::typescript_project::path_affects_typescript_selection(&changed));
        crate::lsp::typescript_project::invalidate_typescript_selection([changed.as_path()]);
        assert_eq!(
            typescript_server(&file, &config).unwrap().kind,
            ServerKind::TypeScriptNative(Arc::from(package_dir.as_path()))
        );
        // Editing a source file cannot change which TypeScript is installed,
        // so it does not empty the memo.
        assert!(!crate::lsp::typescript_project::path_affects_typescript_selection(&file));
        for path in [
            "/r/bun.lock",
            "/r/node_modules/.bin/tsc",
            "/r/node_modules/@typescript/typescript-linux-x64/lib/tsc",
        ] {
            assert!(
                crate::lsp::typescript_project::path_affects_typescript_selection(Path::new(path)),
                "{path}"
            );
        }
    }

    #[test]
    fn disabling_typescript_also_disables_the_native_server() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(&root.join("package.json"), "{}");
        install_typescript(&root, "7.0.2");
        let file = root.join("index.ts");
        write_file(&file, "");
        for disabled in ["typescript", "typescript-native"] {
            let config = Config {
                project_root: Some(root.clone()),
                disabled_lsp: [disabled.to_string()].into_iter().collect(),
                ..Config::default()
            };
            assert!(
                typescript_server(&file, &config).is_none(),
                "lsp.disabled [{disabled}] left a TypeScript server"
            );
        }
    }

    /// A user who replaced the `typescript` server's program keeps it; only
    /// the built-in typescript-language-server is swapped.
    #[test]
    fn user_chosen_typescript_program_is_not_swapped() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::inspect::job::canonicalize_normalized(temp.path());
        write_file(&root.join("package.json"), "{}");
        install_typescript(&root, "7.0.2");
        let file = root.join("index.ts");
        write_file(&file, "");
        let config = Config {
            project_root: Some(root.clone()),
            lsp_servers: vec![UserServerDef {
                id: "typescript".to_string(),
                binary: "vtsls".to_string(),
                ..UserServerDef::default()
            }],
            ..Config::default()
        };
        let server = typescript_server(&file, &config).unwrap();
        assert_eq!(server.kind, ServerKind::TypeScript);
        assert_eq!(server.binary, "vtsls");
    }

    #[test]
    fn test_servers_for_typescript_file() {
        // TS files match TypeScript (primary) plus Biome / Oxlint / Eslint
        // co-servers. The full set is asserted in `test_typescript_co_servers`.
        let kinds = matching_kinds("/tmp/file.ts", &Config::default());
        assert!(
            kinds.contains(&ServerKind::TypeScript),
            "expected TypeScript in {kinds:?}",
        );
    }

    #[test]
    fn test_is_config_file_path_recognizes_project_graph_configs() {
        // These ARE config files that should trigger didChangeWatchedFiles.
        for path in [
            "/repo/package.json",
            "/repo/tsconfig.json",
            "/repo/tsconfig.build.json",
            "/repo/jsconfig.json",
            "/repo/pyproject.toml",
            "/repo/pyrightconfig.json",
            "/repo/Cargo.toml",
            "/repo/Cargo.lock",
            "/repo/go.mod",
            "/repo/biome.json",
        ] {
            assert!(
                is_config_file_path(Path::new(path)),
                "expected config: {path}"
            );
        }

        // Lockfiles are excluded even though they appear in root_markers —
        // they change on every package install and triggering LSP re-analysis
        // on each install creates unnecessary churn. See the LOCKFILE_NAMES
        // list in is_config_file_path(). Cargo.lock is the exception checked
        // above: a stale one breaks rust-analyzer's workspace load.
        for path in [
            "/repo/go.sum",
            "/repo/bun.lock",
            "/repo/bun.lockb",
            "/repo/package-lock.json",
            "/repo/yarn.lock",
            "/repo/pnpm-lock.yaml",
        ] {
            assert!(
                !is_config_file_path(Path::new(path)),
                "lockfile should be excluded from config-file detection: {path}"
            );
        }

        // Non-config files
        for path in [
            "/repo/tsconfig-json",
            "/repo/tsconfig.build.ts",
            "/repo/cargo.toml",
            "/repo/src/package.json.ts",
        ] {
            assert!(
                !is_config_file_path(Path::new(path)),
                "expected non-config: {path}"
            );
        }
    }

    #[test]
    fn test_typescript_co_servers() {
        let kinds = matching_kinds("/tmp/file.ts", &Config::default());
        assert!(kinds.contains(&ServerKind::TypeScript));
        assert!(kinds.contains(&ServerKind::Biome));
        assert!(kinds.contains(&ServerKind::Oxlint));
    }

    #[test]
    fn test_typescript_co_servers_can_be_disabled() {
        // `lsp.disabled` lets users opt out of co-servers individually.
        let mut disabled = std::collections::HashSet::new();
        disabled.insert("biome".to_string());
        disabled.insert("oxlint".to_string());

        let config = Config {
            disabled_lsp: disabled,
            ..Config::default()
        };

        assert_eq!(
            matching_kinds("/tmp/file.ts", &config),
            vec![ServerKind::TypeScript]
        );
    }

    #[test]
    fn test_servers_for_python_file() {
        assert_eq!(
            matching_kinds("/tmp/file.py", &Config::default()),
            vec![ServerKind::Python]
        );
    }

    #[test]
    fn test_servers_for_rust_file() {
        assert_eq!(
            matching_kinds("/tmp/file.rs", &Config::default()),
            vec![ServerKind::Rust]
        );
    }

    #[test]
    fn test_servers_for_go_file() {
        assert_eq!(
            matching_kinds("/tmp/file.go", &Config::default()),
            vec![ServerKind::Go]
        );
    }

    #[test]
    fn test_servers_for_unknown_file() {
        assert!(matching_kinds("/tmp/file.txt", &Config::default()).is_empty());
    }

    #[test]
    fn test_oxlint_root_markers_exclude_package_json() {
        // Oxlint previously listed
        // `package.json` as a root marker, which fired oxc on every JS/TS
        // project — including the overwhelming majority that don't use
        // oxlint — producing a persistent "binary missing" warning whenever
        // the binary wasn't installed. Root markers are now restricted to
        // actual oxlint config files, mirroring user intent.
        let oxlint = super::builtin_servers()
            .into_iter()
            .find(|s| s.kind == ServerKind::Oxlint)
            .expect("Oxlint server must be registered");

        assert!(
            !oxlint.root_markers.iter().any(|m| m == "package.json"),
            "package.json must not be a root marker for oxlint (got {:?})",
            oxlint.root_markers,
        );
        assert!(
            oxlint.root_markers.iter().any(|m| m == ".oxlintrc.json")
                || oxlint.root_markers.iter().any(|m| m == ".oxlintrc"),
            "expected an oxlint config file in root markers (got {:?})",
            oxlint.root_markers,
        );
    }

    #[test]
    fn test_tsx_matches_typescript() {
        let kinds = matching_kinds("/tmp/file.tsx", &Config::default());
        assert!(
            kinds.contains(&ServerKind::TypeScript),
            "expected TypeScript in {kinds:?}",
        );
    }

    #[test]
    fn test_case_insensitive_extension() {
        let kinds = matching_kinds("/tmp/file.TS", &Config::default());
        assert!(
            kinds.contains(&ServerKind::TypeScript),
            "expected TypeScript in {kinds:?}",
        );
    }

    #[test]
    fn test_bash_and_yaml_builtins() {
        assert_eq!(
            matching_kinds("/tmp/file.sh", &Config::default()),
            vec![ServerKind::Bash]
        );
        assert_eq!(
            matching_kinds("/tmp/file.yaml", &Config::default()),
            vec![ServerKind::Yaml]
        );
    }

    #[test]
    fn test_ty_requires_experimental_flag() {
        assert_eq!(
            matching_kinds("/tmp/file.py", &Config::default()),
            vec![ServerKind::Python]
        );

        let config = Config {
            experimental_lsp_ty: true,
            ..Config::default()
        };
        assert_eq!(
            matching_kinds("/tmp/file.py", &config),
            vec![ServerKind::Python, ServerKind::Ty]
        );
    }

    #[test]
    fn test_custom_server_matches_extension() {
        // Use an extension that no built-in server claims so the custom
        // server is the sole match.
        let config = Config {
            lsp_servers: vec![UserServerDef {
                id: "my-custom-lsp".to_string(),
                extensions: vec!["xyzcustom".to_string()],
                binary: "my-custom-lsp".to_string(),
                root_markers: vec!["custom.toml".to_string()],
                ..UserServerDef::default()
            }],
            ..Config::default()
        };

        assert_eq!(
            matching_kinds("/tmp/file.xyzcustom", &config),
            vec![ServerKind::Custom(Arc::from("my-custom-lsp"))]
        );
    }

    #[test]
    fn test_custom_server_coexists_with_builtin_for_same_extension() {
        // Both built-in tinymist and the user's custom override match
        // the same extension. Custom appears after built-ins in the chain.
        let config = Config {
            lsp_servers: vec![UserServerDef {
                id: "tinymist-fork".to_string(),
                extensions: vec!["typ".to_string()],
                binary: "tinymist-fork".to_string(),
                root_markers: vec!["typst.toml".to_string()],
                ..UserServerDef::default()
            }],
            ..Config::default()
        };

        let kinds = matching_kinds("/tmp/file.typ", &config);
        assert!(kinds.contains(&ServerKind::Tinymist));
        assert!(kinds.contains(&ServerKind::Custom(Arc::from("tinymist-fork"))));
    }

    #[test]
    fn test_pattern_a_servers_register_for_their_extensions() {
        let cases: &[(&str, ServerKind)] = &[
            ("/tmp/a.clj", ServerKind::Clojure),
            ("/tmp/a.dart", ServerKind::Dart),
            ("/tmp/a.ex", ServerKind::ElixirLs),
            ("/tmp/a.fs", ServerKind::FSharp),
            ("/tmp/a.gleam", ServerKind::Gleam),
            ("/tmp/a.hs", ServerKind::Haskell),
            ("/tmp/A.java", ServerKind::Jdtls),
            ("/tmp/a.jl", ServerKind::Julia),
            ("/tmp/a.nix", ServerKind::Nixd),
            ("/tmp/a.ml", ServerKind::OcamlLsp),
            ("/tmp/a.php", ServerKind::PhpIntelephense),
            ("/tmp/a.rb", ServerKind::RubyLsp),
            ("/tmp/a.swift", ServerKind::SourceKit),
            ("/tmp/a.cs", ServerKind::CSharp),
            ("/tmp/a.razor", ServerKind::Razor),
        ];

        for (path, expected) in cases {
            let kinds = matching_kinds(path, &Config::default());
            assert!(
                kinds.contains(expected),
                "expected {expected:?} for {path}; got {kinds:?}",
            );
        }
    }

    #[test]
    fn test_pattern_c_servers_register_for_their_extensions() {
        let cases: &[(&str, ServerKind)] = &[
            ("/tmp/a.c", ServerKind::Clangd),
            ("/tmp/a.cpp", ServerKind::Clangd),
            ("/tmp/a.h", ServerKind::Clangd),
            ("/tmp/a.lua", ServerKind::LuaLs),
            ("/tmp/a.zig", ServerKind::Zls),
            ("/tmp/a.typ", ServerKind::Tinymist),
            ("/tmp/a.kt", ServerKind::KotlinLs),
            ("/tmp/a.tex", ServerKind::Texlab),
            ("/tmp/a.tf", ServerKind::TerraformLs),
        ];

        for (path, expected) in cases {
            let kinds = matching_kinds(path, &Config::default());
            assert!(
                kinds.contains(expected),
                "expected {expected:?} for {path}; got {kinds:?}",
            );
        }
    }

    #[test]
    fn test_pattern_b_d_servers_register_for_their_extensions() {
        let cases: &[(&str, ServerKind)] = &[
            ("/tmp/a.vue", ServerKind::Vue),
            ("/tmp/a.astro", ServerKind::Astro),
            ("/tmp/a.prisma", ServerKind::Prisma),
            ("/tmp/a.svelte", ServerKind::Svelte),
            ("/tmp/a.dockerfile", ServerKind::Dockerfile),
        ];

        for (path, expected) in cases {
            let kinds = matching_kinds(path, &Config::default());
            assert!(
                kinds.contains(expected),
                "expected {expected:?} for {path}; got {kinds:?}",
            );
        }
    }

    #[test]
    fn test_lsp_disabled_filters_out_servers_by_id() {
        let mut disabled = std::collections::HashSet::new();
        disabled.insert("clangd".to_string());
        disabled.insert("dart".to_string());
        disabled.insert("rust".to_string());

        let config = Config {
            disabled_lsp: disabled,
            ..Config::default()
        };

        // Disabled servers don't appear; non-disabled servers still match.
        let c_kinds = matching_kinds("/tmp/a.c", &config);
        assert!(!c_kinds.contains(&ServerKind::Clangd));

        let dart_kinds = matching_kinds("/tmp/a.dart", &config);
        assert!(!dart_kinds.contains(&ServerKind::Dart));

        let rust_kinds = matching_kinds("/tmp/a.rs", &config);
        assert!(!rust_kinds.contains(&ServerKind::Rust));

        // Unrelated server still works.
        let ts_kinds = matching_kinds("/tmp/a.ts", &config);
        assert!(ts_kinds.contains(&ServerKind::TypeScript));
    }

    #[test]
    fn test_server_kind_ids_are_unique() {
        // Two server defs with the same `id_str()` would collide in
        // `lsp.disabled` and `lsp.versions` config — protect against that.
        use std::collections::HashSet;
        let servers = super::builtin_servers();
        let ids: Vec<String> = servers
            .iter()
            .map(|s| s.kind.id_str().to_string())
            .collect();
        let unique: HashSet<&String> = ids.iter().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "duplicate server IDs in registry: {ids:?}",
        );
    }

    #[test]
    fn rust_initialization_disables_checks_and_locks_cargo() {
        let rust = super::resolved_servers(&Config::default())
            .into_iter()
            .find(|server| server.kind == ServerKind::Rust)
            .unwrap();
        let options = rust.initialization_options.unwrap();
        assert_eq!(options["checkOnSave"], false);
        assert_eq!(
            options["cargo"]["extraArgs"],
            serde_json::json!(["--locked"])
        );
        assert_eq!(
            options["cargo"]["metadataExtraArgs"],
            serde_json::json!(["--locked"])
        );
    }

    #[test]
    fn rust_initialization_override_keeps_builtin_cargo_options() {
        let config = Config {
            lsp_servers: vec![UserServerDef {
                id: "rust".into(),
                initialization_options: Some(serde_json::json!({"checkOnSave": true})),
                ..UserServerDef::default()
            }],
            ..Config::default()
        };
        let rust = super::resolved_servers(&config)
            .into_iter()
            .find(|server| server.kind == ServerKind::Rust)
            .unwrap();
        assert_eq!(
            rust.initialization_options.unwrap(),
            serde_json::json!({"checkOnSave": true, "cargo": {
                "extraArgs": ["--locked"], "metadataExtraArgs": ["--locked"]
            }})
        );
    }

    #[test]
    fn initialization_override_merges_objects_and_replaces_arrays_and_scalars() {
        let config = Config {
            lsp_servers: vec![
                UserServerDef {
                    id: "rust".into(),
                    initialization_options: Some(serde_json::json!({"cargo": {
                        "extraArgs": [], "newOption": 42
                    }})),
                    ..UserServerDef::default()
                },
                UserServerDef {
                    id: "go".into(),
                    initialization_options: Some(serde_json::json!({
                        "pullDiagnostics": false, "newOption": "value"
                    })),
                    ..UserServerDef::default()
                },
            ],
            ..Config::default()
        };
        let servers = super::resolved_servers(&config);
        let rust = servers.iter().find(|s| s.kind == ServerKind::Rust).unwrap();
        assert_eq!(
            rust.initialization_options.as_ref().unwrap(),
            &serde_json::json!({"checkOnSave": false, "cargo": {
                "extraArgs": [], "metadataExtraArgs": ["--locked"], "newOption": 42
            }})
        );
        let go = servers.iter().find(|s| s.kind == ServerKind::Go).unwrap();
        assert_eq!(
            go.initialization_options.as_ref().unwrap(),
            &serde_json::json!({"pullDiagnostics": false, "newOption": "value"})
        );
    }

    #[test]
    fn user_override_with_matching_id_replaces_builtin_not_appended() {
        // Issue #56: setting `lsp.servers.clangd = { args: [...] }` should
        // result in ONE clangd entry (the user-overridden one), not two.
        let config = Config {
            lsp_servers: vec![UserServerDef {
                id: "clangd".to_string(),
                args: vec!["--query-driver=/path/to/arm-none-eabi-*".to_string()],
                ..UserServerDef::default()
            }],
            ..Config::default()
        };

        let cpp_servers = super::servers_for_file(Path::new("/tmp/a.cpp"), &config);
        let clangd_entries: Vec<_> = cpp_servers
            .iter()
            .filter(|s| s.kind.id_str() == "clangd")
            .collect();
        assert_eq!(
            clangd_entries.len(),
            1,
            "expected exactly one clangd server after user override; got {} ({:?})",
            clangd_entries.len(),
            cpp_servers.iter().map(|s| &s.kind).collect::<Vec<_>>()
        );

        // Override fields take effect.
        let clangd = clangd_entries[0];
        assert_eq!(clangd.args, vec!["--query-driver=/path/to/arm-none-eabi-*"],);

        // Fields the user left empty (extensions, root_markers) inherit from
        // the built-in — that's the whole point of the merge.
        assert!(
            !clangd.extensions.is_empty(),
            "extensions should inherit from built-in clangd, got empty",
        );
        assert!(
            !clangd.root_markers.is_empty(),
            "root_markers should inherit from built-in clangd, got empty",
        );
    }

    #[test]
    fn user_override_preserves_builtin_kind_not_custom() {
        // The merged entry must keep the built-in ServerKind variant (e.g.
        // ServerKind::Clangd) so callers that match on the enum continue to
        // work — including `lsp.disabled` and any kind-specific capability
        // probing in the LSP manager.
        let config = Config {
            lsp_servers: vec![UserServerDef {
                id: "clangd".to_string(),
                root_markers: vec![".clangd".to_string()],
                ..UserServerDef::default()
            }],
            ..Config::default()
        };

        let cpp_servers = super::servers_for_file(Path::new("/tmp/a.cpp"), &config);
        let clangd = cpp_servers
            .iter()
            .find(|s| s.kind.id_str() == "clangd")
            .expect("clangd entry");
        assert!(
            matches!(clangd.kind, ServerKind::Clangd),
            "merged server must keep ServerKind::Clangd, got {:?}",
            clangd.kind,
        );
    }

    #[test]
    fn user_override_with_non_matching_id_is_appended_as_custom() {
        // Pre-existing behavior preserved: a user-defined id that doesn't
        // match any built-in is registered as a Custom server alongside the
        // built-ins. (This is the workaround issue #56 reporters were using
        // — it must keep working.)
        //
        // Extensions in `lsp.servers` are matched WITHOUT a leading dot
        // (the same convention as built-in servers — see `builtin_server()`
        // calls). Users writing `".cpp"` in their config would silently
        // never match; that's a separate UX gap not part of this fix.
        let config = Config {
            lsp_servers: vec![UserServerDef {
                id: "custom-clangd".to_string(),
                extensions: vec!["c".to_string(), "cpp".to_string()],
                binary: "clangd".to_string(),
                ..UserServerDef::default()
            }],
            ..Config::default()
        };

        let cpp_servers = super::servers_for_file(Path::new("/tmp/a.cpp"), &config);
        let kinds: Vec<&ServerKind> = cpp_servers.iter().map(|s| &s.kind).collect();
        assert!(
            kinds.iter().any(|k| matches!(k, ServerKind::Clangd)),
            "built-in clangd should still be present alongside custom-clangd; got {kinds:?}",
        );
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, ServerKind::Custom(id) if id.as_ref() == "custom-clangd")),
            "custom-clangd should be appended as Custom; got {kinds:?}",
        );
    }

    #[test]
    fn user_override_with_disabled_true_drops_builtin() {
        // `lsp.servers.clangd = { disabled: true }` should be equivalent to
        // adding `"clangd"` to `lsp.disabled`.
        let config = Config {
            lsp_servers: vec![UserServerDef {
                id: "clangd".to_string(),
                disabled: true,
                ..UserServerDef::default()
            }],
            ..Config::default()
        };

        let cpp_servers = super::servers_for_file(Path::new("/tmp/a.cpp"), &config);
        assert!(
            !cpp_servers.iter().any(|s| s.kind.id_str() == "clangd"),
            "disabled user override should drop the built-in; got {:?}",
            cpp_servers.iter().map(|s| &s.kind).collect::<Vec<_>>(),
        );
    }

    /// Helper: write an executable file containing `#!/bin/sh\n` so it
    /// passes both `is_file()` checks and is executable on Unix.
    fn touch_exe(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    fn oxlint_def() -> ServerDef {
        builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::Oxlint)
            .unwrap()
    }

    #[test]
    fn oxlint_resolves_cli_with_lsp_args() {
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join("node_modules/.bin");
        touch_exe(&local_bin.join("oxlint"));
        let server = oxlint_def();
        assert_eq!(server.binary, "oxlint");
        assert_eq!(server.args, ["--lsp"]);
        let config = Config {
            project_root: Some(tmp.path().to_path_buf()),
            ..Config::default()
        };
        let binary = resolve_server_binary(&server, Some(tmp.path()), &config).unwrap();
        assert_eq!(binary, local_bin.join("oxlint"));
        assert_eq!(server.spawn_args_for_binary(&binary), ["--lsp"]);
    }

    #[test]
    fn oxlint_resolves_standalone_without_args() {
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join("node_modules/.bin");
        touch_exe(&local_bin.join("oxc_language_server"));
        let server = oxlint_def();
        let config = Config {
            project_root: Some(tmp.path().to_path_buf()),
            ..Config::default()
        };
        let binary = resolve_server_binary(&server, Some(tmp.path()), &config).unwrap();
        assert_eq!(binary, local_bin.join("oxc_language_server"));
        assert!(server.spawn_args_for_binary(&binary).is_empty());
    }

    #[test]
    fn oxlint_prefers_standalone_alongside_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join("node_modules/.bin");
        touch_exe(&local_bin.join("oxlint"));
        touch_exe(&local_bin.join("oxc_language_server"));
        let server = oxlint_def();
        let config = Config {
            project_root: Some(tmp.path().to_path_buf()),
            ..Config::default()
        };
        let binary = resolve_server_binary(&server, Some(tmp.path()), &config).unwrap();
        assert_eq!(binary, local_bin.join("oxc_language_server"));
        assert!(server.spawn_args_for_binary(&binary).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn oxlint_standalone_npm_shim_has_no_args() {
        let tmp = tempfile::tempdir().unwrap();
        let local_bin = tmp.path().join("node_modules/.bin");
        touch_exe(&local_bin.join("oxc_language_server.cmd"));
        let server = oxlint_def();
        let config = Config {
            project_root: Some(tmp.path().to_path_buf()),
            ..Config::default()
        };
        let binary = resolve_server_binary(&server, Some(tmp.path()), &config).unwrap();
        assert_eq!(binary, local_bin.join("oxc_language_server.cmd"));
        assert!(server.spawn_args_for_binary(&binary).is_empty());
    }

    #[test]
    fn resolve_lsp_binary_prefers_project_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let local_bin = project.join("node_modules").join(".bin");
        touch_exe(&local_bin.join("typescript-language-server"));

        let resolved = resolve_lsp_binary("typescript-language-server", Some(project), &[]);
        assert_eq!(
            resolved.as_deref(),
            Some(local_bin.join("typescript-language-server").as_path())
        );
    }

    #[test]
    fn resolve_lsp_binary_falls_back_to_extra_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let extra_a = tmp.path().join("extra_a");
        let extra_b = tmp.path().join("extra_b");
        std::fs::create_dir_all(&extra_a).unwrap();
        std::fs::create_dir_all(&extra_b).unwrap();
        touch_exe(&extra_b.join("yaml-language-server"));

        let resolved = resolve_lsp_binary(
            "yaml-language-server",
            Some(&project),
            &[extra_a.clone(), extra_b.clone()],
        );
        assert_eq!(
            resolved.as_deref(),
            Some(extra_b.join("yaml-language-server").as_path())
        );
    }

    #[test]
    fn resolve_lsp_binary_extra_paths_search_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let extra_a = tmp.path().join("extra_a");
        let extra_b = tmp.path().join("extra_b");
        std::fs::create_dir_all(&extra_a).unwrap();
        std::fs::create_dir_all(&extra_b).unwrap();
        // Same binary in both — earlier path wins.
        touch_exe(&extra_a.join("bash-language-server"));
        touch_exe(&extra_b.join("bash-language-server"));

        let resolved = resolve_lsp_binary(
            "bash-language-server",
            None,
            &[extra_a.clone(), extra_b.clone()],
        );
        assert_eq!(
            resolved.as_deref(),
            Some(extra_a.join("bash-language-server").as_path())
        );
    }

    #[test]
    fn resolve_lsp_binary_project_root_wins_over_extra_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let local_bin = project.join("node_modules").join(".bin");
        touch_exe(&local_bin.join("pyright-langserver"));

        let extra = tmp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        touch_exe(&extra.join("pyright-langserver"));

        let resolved = resolve_lsp_binary(
            "pyright-langserver",
            Some(&project),
            std::slice::from_ref(&extra),
        );
        assert_eq!(
            resolved.as_deref(),
            Some(local_bin.join("pyright-langserver").as_path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn dockerfile_server_prefers_docker_language_server_and_falls_back_on_path() {
        const CHILD_ENV: &str = "AFT_DOCKER_SERVER_PATH_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            let server = builtin_servers()
                .into_iter()
                .find(|server| server.kind == ServerKind::Dockerfile)
                .unwrap();
            let config = Config::default();
            let preferred = resolve_server_binary(&server, None, &config).unwrap();
            assert_eq!(
                preferred.file_name().and_then(|name| name.to_str()),
                Some("docker-language-server")
            );
            assert_eq!(
                server.spawn_args_for_binary(&preferred),
                ["start", "--stdio"]
            );

            std::fs::remove_file(&preferred).unwrap();
            let fallback = resolve_server_binary(&server, None, &config).unwrap();
            assert_eq!(
                fallback.file_name().and_then(|name| name.to_str()),
                Some("docker-langserver")
            );
            assert_eq!(server.spawn_args_for_binary(&fallback), ["--stdio"]);
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        touch_exe(&temp.path().join("docker-language-server"));
        touch_exe(&temp.path().join("docker-langserver"));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lsp::registry::tests::dockerfile_server_prefers_docker_language_server_and_falls_back_on_path",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env("PATH", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child test failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn generic_resolver_does_not_probe_project_virtualenv() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let extra = tmp.path().join("extra");
        let virtualenv_bin = if cfg!(windows) {
            project.join(".venv").join("Scripts")
        } else {
            project.join(".venv").join("bin")
        };
        touch_exe(&virtualenv_bin.join("typescript-language-server"));
        touch_exe(&extra.join("typescript-language-server"));

        let resolved = resolve_lsp_binary(
            "typescript-language-server",
            Some(&project),
            std::slice::from_ref(&extra),
        );

        assert_eq!(
            resolved.as_deref(),
            Some(extra.join("typescript-language-server").as_path())
        );
    }

    #[test]
    fn python_resolver_prefers_nested_project_virtualenv() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path();
        let backend = repository.join("backend");
        let virtualenv_bin = if cfg!(windows) {
            backend.join(".venv").join("Scripts")
        } else {
            backend.join(".venv").join("bin")
        };
        touch_exe(&virtualenv_bin.join("pyright-langserver"));
        let cache = repository.join("cache");
        touch_exe(&cache.join("pyright-langserver"));
        let config = Config {
            project_root: Some(repository.to_path_buf()),
            lsp_paths_extra: vec![cache],
            ..Config::default()
        };
        let server = builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::Python)
            .unwrap();

        let resolved = resolve_server_binary(&server, Some(&backend), &config);

        assert_eq!(
            resolved.as_deref(),
            Some(virtualenv_bin.join("pyright-langserver").as_path())
        );
    }

    #[test]
    fn python_resolver_falls_back_to_project_root_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let workspace = project.join("backend");
        let project_bin = project.join("node_modules").join(".bin");
        let hoisted = if cfg!(windows) {
            project_bin.join("pyright-langserver.cmd")
        } else {
            project_bin.join("pyright-langserver")
        };
        touch_exe(&hoisted);
        let server = builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::Python)
            .unwrap();
        let config = Config {
            project_root: Some(project.to_path_buf()),
            ..Config::default()
        };

        assert_eq!(
            resolve_server_binary(&server, Some(&workspace), &config),
            Some(hoisted)
        );
    }

    #[test]
    fn python_resolver_prefers_workspace_node_modules_over_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let workspace = project.join("backend");
        let workspace_bin = workspace.join("node_modules").join(".bin");
        let project_bin = project.join("node_modules").join(".bin");
        let binary_name = if cfg!(windows) {
            "pyright-langserver.cmd"
        } else {
            "pyright-langserver"
        };
        let nested = workspace_bin.join(binary_name);
        touch_exe(&nested);
        touch_exe(&project_bin.join(binary_name));
        let server = builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::Python)
            .unwrap();
        let config = Config {
            project_root: Some(project.to_path_buf()),
            ..Config::default()
        };

        assert_eq!(
            resolve_server_binary(&server, Some(&workspace), &config),
            Some(nested)
        );
    }

    #[test]
    fn ty_resolver_prefers_nested_project_virtualenv() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path();
        let backend = repository.join("backend");
        let virtualenv_bin = if cfg!(windows) {
            backend.join(".venv").join("Scripts")
        } else {
            backend.join(".venv").join("bin")
        };
        touch_exe(&virtualenv_bin.join("ty"));
        let config = Config {
            project_root: Some(repository.to_path_buf()),
            ..Config::default()
        };
        let server = builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::Ty)
            .unwrap();

        let resolved = resolve_server_binary(&server, Some(&backend), &config);

        assert_eq!(
            resolved.as_deref(),
            Some(virtualenv_bin.join("ty").as_path())
        );
    }

    #[test]
    fn non_python_resolver_keeps_configured_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        let backend = repository.join("backend");
        let repository_bin = repository.join("node_modules").join(".bin");
        let backend_bin = backend.join("node_modules").join(".bin");
        touch_exe(&repository_bin.join("typescript-language-server"));
        touch_exe(&backend_bin.join("typescript-language-server"));
        let config = Config {
            project_root: Some(repository.clone()),
            ..Config::default()
        };
        let server = builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::TypeScript)
            .unwrap();

        let resolved = resolve_server_binary(&server, Some(&backend), &config);

        assert_eq!(
            resolved.as_deref(),
            Some(repository_bin.join("typescript-language-server").as_path())
        );
    }

    #[test]
    fn non_python_without_project_root_ignores_workspace_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("backend");
        let workspace_bin = workspace.join("node_modules").join(".bin");
        touch_exe(&workspace_bin.join("typescript-language-server"));
        let config = Config::default();
        let server = builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::TypeScript)
            .unwrap();

        // The generic ladder stays rooted at the configured project root only;
        // with none configured it must not adopt the workspace root. (A PATH
        // install may still resolve, so assert on provenance, not absence.)
        let resolved = resolve_server_binary(&server, Some(&workspace), &config);
        assert!(
            resolved.map_or(true, |path| !path.starts_with(&workspace)),
            "generic resolution must not adopt the workspace root"
        );
    }

    fn biome_def() -> ServerDef {
        builtin_servers()
            .into_iter()
            .find(|server| server.kind == ServerKind::Biome)
            .unwrap()
    }

    /// A Bun 1.4 workspace installs per package: `biome` exists only in
    /// `packages/<name>/node_modules/.bin`, and the project root has a
    /// `node_modules` without `.bin/biome`. Biome's server root is the
    /// package (its `biome.json`), so the package-local Biome must resolve.
    #[test]
    fn biome_resolves_package_local_binary_in_a_per_package_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let project = crate::inspect::job::canonicalize_normalized(tmp.path());
        std::fs::create_dir_all(project.join("node_modules")).unwrap();
        let plugin = project.join("packages").join("plugin");
        let plugin_bin = plugin.join("node_modules").join(".bin");
        let binary_name = if cfg!(windows) { "biome.cmd" } else { "biome" };
        touch_exe(&plugin_bin.join(binary_name));
        let config = Config {
            project_root: Some(project.clone()),
            ..Config::default()
        };

        assert_eq!(
            resolve_server_binary(&biome_def(), Some(&plugin), &config),
            Some(plugin_bin.join(binary_name))
        );
    }

    /// The nearest `node_modules/.bin` wins, as in Node resolution: a
    /// package's own Biome over the project root's, and the project root's
    /// when a nested package has none of its own.
    #[test]
    fn biome_prefers_nearest_node_modules_and_falls_back_to_the_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let project = crate::inspect::job::canonicalize_normalized(tmp.path());
        let binary_name = if cfg!(windows) { "biome.cmd" } else { "biome" };
        let root_bin = project.join("node_modules").join(".bin");
        touch_exe(&root_bin.join(binary_name));
        let cli = project.join("packages").join("cli");
        let cli_bin = cli.join("node_modules").join(".bin");
        touch_exe(&cli_bin.join(binary_name));
        let nested = project.join("packages").join("dashboard").join("app");
        let dashboard_bin = project
            .join("packages")
            .join("dashboard")
            .join("node_modules")
            .join(".bin");
        touch_exe(&dashboard_bin.join(binary_name));
        let bare = project.join("packages").join("retina-local-fs");
        std::fs::create_dir_all(&bare).unwrap();
        let config = Config {
            project_root: Some(project.clone()),
            ..Config::default()
        };
        let server = biome_def();

        assert_eq!(
            resolve_server_binary(&server, Some(&cli), &config),
            Some(cli_bin.join(binary_name))
        );
        assert_eq!(
            resolve_server_binary(&server, Some(&nested), &config),
            Some(dashboard_bin.join(binary_name))
        );
        assert_eq!(
            resolve_server_binary(&server, Some(&bare), &config),
            Some(root_bin.join(binary_name))
        );
    }

    /// The walk stops at the project root: a Biome installed above it is
    /// never adopted through the package walk.
    #[test]
    fn biome_walk_does_not_leave_the_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = crate::inspect::job::canonicalize_normalized(tmp.path());
        let binary_name = if cfg!(windows) { "biome.cmd" } else { "biome" };
        let outside_bin = outside.join("node_modules").join(".bin");
        touch_exe(&outside_bin.join(binary_name));
        let project = outside.join("project");
        let plugin = project.join("packages").join("plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        let config = Config {
            project_root: Some(project.clone()),
            ..Config::default()
        };

        let resolved = resolve_server_binary(&biome_def(), Some(&plugin), &config);
        assert!(
            resolved.map_or(true, |path| !path.starts_with(&outside_bin)),
            "the Biome walk must stop at the project root"
        );
    }

    #[test]
    fn python_auto_stays_on_pyright_when_local_ty_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let source = project.join("main.py");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("pyproject.toml"), "[project]\nname = 'demo'\n").unwrap();
        let virtualenv_bin = if cfg!(windows) {
            project.join(".venv").join("Scripts")
        } else {
            project.join(".venv").join("bin")
        };
        touch_exe(&virtualenv_bin.join("ty"));
        let config = Config {
            project_root: Some(project.clone()),
            ..Config::default()
        };

        let servers = servers_for_file(&source, &config);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].kind, ServerKind::Python);
        assert_eq!(servers[0].binary, "pyright-langserver");
    }

    #[test]
    fn python_workspace_root_does_not_escape_configured_project() {
        let tmp = tempfile::tempdir().unwrap();
        let ancestor = tmp.path();
        let project = ancestor.join("project");
        let source = project.join("src").join("main.py");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "print('ok')\n").unwrap();
        std::fs::write(
            ancestor.join("pyproject.toml"),
            "[project]\nname = 'outside'\n",
        )
        .unwrap();
        let outside_python = if cfg!(windows) {
            ancestor.join(".venv").join("Scripts").join("python.exe")
        } else {
            ancestor.join(".venv").join("bin").join("python")
        };
        touch_exe(&outside_python);

        let project_root = crate::inspect::job::canonicalize_normalized(&project);
        for kind in [ServerKind::Python, ServerKind::Ty] {
            let server = builtin_servers()
                .into_iter()
                .find(|server| server.kind == kind)
                .unwrap();
            // With no marker inside the project, the script is served from
            // the project root itself, never from the marker above it.
            assert_eq!(
                server.workspace_root_for_file_with_project_root(&source, Some(&project)),
                Some(project_root.clone()),
                "{kind:?} must not select a marker above project_root"
            );
        }
    }

    #[test]
    fn resolve_lsp_binary_returns_none_for_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        // Use a binary name that's almost certainly not on PATH.
        let resolved =
            resolve_lsp_binary("aft-test-nonexistent-binary-xyz123", Some(&project), &[]);
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_lsp_binary_handles_missing_node_modules_gracefully() {
        // project_root is set but node_modules/.bin doesn't exist.
        // Should fall through to extra_paths and PATH without error.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let extra = tmp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        touch_exe(&extra.join("gopls"));

        let resolved = resolve_lsp_binary("gopls", Some(&project), std::slice::from_ref(&extra));
        assert_eq!(resolved.as_deref(), Some(extra.join("gopls").as_path()));
    }

    #[test]
    fn resolve_lsp_binary_skips_nonexistent_extra_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let valid = tmp.path().join("valid");
        std::fs::create_dir_all(&valid).unwrap();
        touch_exe(&valid.join("clangd"));

        let resolved = resolve_lsp_binary("clangd", None, &[missing, valid.clone()]);

        assert_eq!(resolved.as_deref(), Some(valid.join("clangd").as_path()));
    }

    #[test]
    fn resolve_lsp_binary_skips_file_extra_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        let valid = tmp.path().join("valid");
        std::fs::write(&file, "not a directory").unwrap();
        std::fs::create_dir_all(&valid).unwrap();
        touch_exe(&valid.join("lua-language-server"));

        let resolved = resolve_lsp_binary("lua-language-server", None, &[file, valid.clone()]);

        assert_eq!(
            resolved.as_deref(),
            Some(valid.join("lua-language-server").as_path())
        );
    }

    #[test]
    fn resolve_lsp_binary_skips_deleted_extra_path() {
        let tmp = tempfile::tempdir().unwrap();
        let deleted = tmp.path().join("deleted");
        let valid = tmp.path().join("valid");
        std::fs::create_dir_all(&deleted).unwrap();
        std::fs::remove_dir(&deleted).unwrap();
        std::fs::create_dir_all(&valid).unwrap();
        touch_exe(&valid.join("svelte-language-server"));

        let resolved =
            resolve_lsp_binary("svelte-language-server", None, &[deleted, valid.clone()]);

        assert_eq!(
            resolved.as_deref(),
            Some(valid.join("svelte-language-server").as_path())
        );
    }

    // Avoid unused-import warning on platforms where probe_dir's Windows
    // branch is dead code.
    #[allow(dead_code)]
    fn _path_buf_used(_p: PathBuf) {}
}
