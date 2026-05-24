use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::config::{Config, UserServerDef};

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

    // 3. PATH fallback
    which::which(binary).ok()
}

/// Check `dir/<binary>` and (on Windows) `dir/<binary>.cmd|.exe|.bat`.
fn probe_dir(dir: &Path, binary: &str) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }

    let direct = dir.join(binary);
    if direct.is_file() {
        return Some(direct);
    }

    if cfg!(windows) {
        for ext in ["cmd", "exe", "bat"] {
            let candidate = dir.join(format!("{binary}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
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
    /// Extra environment variables for this server process.
    pub env: HashMap<String, String>,
    /// Optional JSON initializationOptions for the initialize request.
    pub initialization_options: Option<serde_json::Value>,
}

impl ServerDef {
    /// Check if this server handles a given file extension.
    pub fn matches_extension(&self, ext: &str) -> bool {
        self.extensions
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(ext))
    }

    /// Check if the server binary is available on PATH.
    pub fn is_available(&self) -> bool {
        which::which(&self.binary).is_ok()
    }
}

/// Built-in server definitions.
pub fn builtin_servers() -> Vec<ServerDef> {
    vec![
        builtin_server(
            ServerKind::TypeScript,
            "TypeScript Language Server",
            &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
            "typescript-language-server",
            &["--stdio"],
            &["tsconfig.json", "jsconfig.json", "package.json"],
        ),
        builtin_server(
            ServerKind::Python,
            "Pyright",
            &["py", "pyi"],
            "pyright-langserver",
            &["--stdio"],
            &[
                "pyproject.toml",
                "setup.py",
                "setup.cfg",
                "pyrightconfig.json",
                "requirements.txt",
            ],
        ),
        builtin_server(
            ServerKind::Rust,
            "rust-analyzer",
            &["rs"],
            "rust-analyzer",
            &[],
            &["Cargo.toml", "Cargo.lock"],
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
            "oxc-language-server",
            // Same JS/TS family as TypeScript LS; coexists rather than replaces.
            &[
                "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "vue", "astro", "svelte",
            ],
            "oxc-language-server",
            &[],
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

    resolved_servers(config)
        .into_iter()
        .filter(|server| !is_disabled(server, config))
        .filter(|server| config.experimental_lsp_ty || server.kind != ServerKind::Ty)
        .filter(|server| server.matches_extension(extension))
        .collect()
}

/// Resolve the full server set after applying user overrides.
///
/// When a user-defined server's `id` matches a built-in server's `id_str()`
/// (e.g. `lsp.servers.clangd`), the user entry REPLACES the built-in entry
/// rather than registering alongside it. Fields the user left at their
/// default value (empty array, empty string, empty map, None) are inherited
/// from the built-in so users only have to specify what they actually want
/// to override.
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
                env: if user.env.is_empty() {
                    builtin.env.clone()
                } else {
                    user.env.clone()
                },
                initialization_options: user
                    .initialization_options
                    .clone()
                    .or_else(|| builtin.initialization_options.clone()),
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
    const LOCKFILE_NAMES: &[&str] = &[
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
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
        env: HashMap::new(),
        initialization_options: None,
    }
}

/// Builder variant of [`builtin_server`] that includes a default
/// `initializationOptions` payload — used for servers that need server-specific
/// settings to enable LSP features (e.g., gopls's `pullDiagnostics`).
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

    use crate::config::{Config, UserServerDef};

    use super::{is_config_file_path, resolve_lsp_binary, servers_for_file, ServerKind};

    fn matching_kinds(path: &str, config: &Config) -> Vec<ServerKind> {
        servers_for_file(Path::new(path), config)
            .into_iter()
            .map(|server| server.kind)
            .collect()
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
        // list in is_config_file_path().
        for path in [
            "/repo/Cargo.lock",
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
        // Regression guard (v0.17.2): oxc-language-server previously listed
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
