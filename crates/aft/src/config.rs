use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS: u64 = 3_000;
pub(crate) const MIN_SEMANTIC_QUERY_TIMEOUT_MS: u64 = 500;
pub(crate) const MAX_SEMANTIC_QUERY_TIMEOUT_MS: u64 = 15_000;
pub(crate) const DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS: u64 = 120_000;
pub(crate) const MIN_INSPECT_DIAGNOSTICS_TIMEOUT_MS: u64 = 10_000;
pub(crate) const MAX_INSPECT_DIAGNOSTICS_TIMEOUT_MS: u64 = 600_000;

const fn default_semantic_query_timeout_ms() -> u64 {
    DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS
}

const fn default_inspect_diagnostics_timeout_ms() -> u64 {
    DEFAULT_INSPECT_DIAGNOSTICS_TIMEOUT_MS
}

use crate::harness::Harness;

/// Runtime configuration for the aft process.
///
/// Holds project-scoped settings and tuning knobs. Values are set at startup
/// and remain immutable for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticBackend {
    Fastembed,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    Ollama,
}

impl SemanticBackend {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Fastembed => "fastembed",
            Self::OpenAiCompatible => "openai_compatible",
            Self::Ollama => "ollama",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "fastembed" => Some(Self::Fastembed),
            "openai_compatible" => Some(Self::OpenAiCompatible),
            "ollama" => Some(Self::Ollama),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticBackendConfig {
    pub backend: SemanticBackend,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub timeout_ms: u64,
    /// Deadline for one interactive query embedding request. Unlike `timeout_ms`,
    /// this budget never controls background index builds.
    #[serde(default = "default_semantic_query_timeout_ms")]
    pub query_timeout_ms: u64,
    pub max_batch_size: usize,
    /// Maximum number of project files to semantically index. Guards local
    /// fastembed memory (model + embeddings + batch buffers) on huge project
    /// roots; remote backends that embed server-side can raise it freely.
    pub max_files: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserServerDef {
    pub id: String,
    pub extensions: Vec<String>,
    pub binary: String,
    pub args: Vec<String>,
    pub root_markers: Vec<String>,
    pub env: HashMap<String, String>,
    pub initialization_options: Option<serde_json::Value>,
    pub disabled: bool,
}

impl Default for SemanticBackendConfig {
    fn default() -> Self {
        Self {
            backend: SemanticBackend::Fastembed,
            model: DEFAULT_SEMANTIC_MODEL.to_string(),
            base_url: None,
            api_key_env: None,
            // Keep the default below the plugin bridge timeout to avoid bridge-killed
            // semantic_search requests when callers do not set an explicit timeout.
            timeout_ms: 25_000,
            query_timeout_ms: DEFAULT_SEMANTIC_QUERY_TIMEOUT_MS,
            max_batch_size: 64,
            max_files: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InspectConfig {
    pub enabled: bool,
    /// Deadline for the blocking LSP diagnostics phase of `aft_inspect`.
    #[serde(default = "default_inspect_diagnostics_timeout_ms")]
    pub diagnostics_timeout_ms: u64,
    pub duplicates: InspectDuplicatesConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InspectDuplicatesConfig {
    pub expected_mirrors: Vec<[String; 2]>,
}

impl Default for InspectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            diagnostics_timeout_ms: default_inspect_diagnostics_timeout_ms(),
            duplicates: InspectDuplicatesConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    pub enabled: Option<bool>,
    pub max_depth: Option<usize>,
    pub max_file_size: Option<u64>,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: Some(true),
            max_depth: Some(crate::backup::DEFAULT_MAX_UNDO_DEPTH),
            max_file_size: None,
        }
    }
}

/// `gh` routing shim operator hard-off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GhShimConfig {
    /// When false, the `gh` routing shim short-circuits to byte-transparent
    /// passthrough (R1) before any daemon/catalog probing, so a disabled shim
    /// performs zero subc traffic. Default true. This is an operator hard-off
    /// for fleet rollout safety; it adds no capability beyond today's
    /// structural rungs.
    pub enabled: bool,
}

impl Default for GhShimConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Linked-worktree behavior that never writes shared on-disk artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    /// When true, a borrow-only (linked worktree) root applies its own
    /// file-watcher events to the in-RAM trigram delta and invalidates the
    /// symbol cache so search reflects local edits. Default false. Semantic
    /// search and the callgraph stay frozen. Never persists to the shared
    /// `cache.bin`.
    pub ram_overlay: bool,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self { ram_overlay: false }
    }
}

pub const DEFAULT_SEMANTIC_MODEL: &str = "all-MiniLM-L6-v2";

impl Config {
    pub fn semantic_backend_label(&self) -> &'static str {
        self.semantic.backend.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SandboxConfig {
    /// Route first-party bash commands through the native platform sandbox.
    pub enabled: bool,
    /// User-approved writable roots in addition to projects, task artifacts, and caches.
    pub write_allow: Vec<PathBuf>,
    /// Extra paths that native backends should deny reading.
    pub read_deny: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BashConfig {
    /// Permit plugin-side break-glass execution when its AFT transport is unavailable.
    /// Rust accepts this for cross-language config parity but never acts on it.
    pub host_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Root directory of the project being analyzed. `None` if not scoped.
    pub project_root: Option<PathBuf>,
    /// How many levels of call-graph edges to follow during validation (default: 1).
    pub validation_depth: u32,
    /// Hours before a checkpoint expires and is eligible for cleanup (default: 24).
    pub checkpoint_ttl_hours: u32,
    /// Maximum depth for recursive symbol resolution (default: 10).
    pub max_symbol_depth: u32,
    /// Seconds before killing a formatter subprocess (default: 10).
    pub formatter_timeout_secs: u32,
    /// Seconds before killing a type-checker subprocess (default: 30).
    pub type_checker_timeout_secs: u32,
    /// Whether to auto-format files after edits (default: true).
    pub format_on_edit: bool,
    /// Whether the hashline edit/read surface is enabled for eligible sessions.
    /// Resolved from the public `edit_mode` enum in aft.jsonc.
    pub hashline_enabled: bool,
    /// Whether to auto-validate files after edits (default: false).
    /// When "syntax", only tree-sitter parse check. When "full", runs type checker.
    pub validate_on_edit: Option<String>,
    /// Per-language formatter overrides. Keys: "typescript", "python", "rust", "go".
    /// Values: "biome", "oxfmt", "prettier", "deno", "ruff", "black", "rustfmt", "goimports", "gofmt", "none".
    pub formatter: HashMap<String, String>,
    /// Per-language type checker overrides. Keys: "typescript", "python", "rust", "go".
    /// Values: "tsc", "tsgo", "biome", "pyright", "ruff", "cargo", "go", "staticcheck", "none".
    pub checker: HashMap<String, String>,
    /// Whether to restrict file operations to within `project_root` (default: false).
    /// When true, write-capable commands reject paths outside the project root.
    pub restrict_to_project_root: bool,
    /// Enable the trigram search index (default: false).
    pub search_index: bool,
    /// Enable semantic search (default: false).
    pub semantic_search: bool,
    /// Whether the plugin registered the `aft_search` tool for this surface
    /// (default: false). Forwarded by the plugin's resolved registration
    /// predicate (semantic on + not minimal + not disabled). Used only to pick
    /// the grep-rewrite footer: when true the footer steers to `aft_search`,
    /// otherwise to the `grep` tool. Not a capability gate.
    pub aft_search_registered: bool,
    /// Enable the persisted callgraph store substrate (default: true).
    pub callgraph_store: bool,
    /// Number of files to parse in a single batch during callgraph store cold build (default: 100).
    /// Lower values reduce peak memory during cold build.
    /// Set to 0 to disable chunking and parse all files at once.
    pub callgraph_chunk_size: usize,
    /// Enable experimental bash command rewriting (default: false).
    pub experimental_bash_rewrite: bool,
    /// Enable experimental bash command compression (default: false).
    pub experimental_bash_compress: bool,
    /// Enable experimental bash background execution (default: false).
    pub experimental_bash_background: bool,
    /// Maximum number of background bash tasks allowed to run concurrently (default: 8).
    pub max_background_bash_tasks: usize,
    /// Emit reminders for long-running bash tasks (default: true).
    pub bash_long_running_reminder_enabled: bool,
    /// Milliseconds between long-running bash reminders (default: 10 minutes).
    pub bash_long_running_reminder_interval_ms: u64,
    /// Milliseconds to wait before a foreground bash task is promoted to background handling.
    #[serde(skip, default = "default_foreground_wait_window_ms")]
    pub foreground_wait_window_ms: u64,
    /// Plugin-owned bash settings accepted by configure but inert in the engine.
    pub bash: BashConfig,
    /// Enable OpenCode-style bash permission prompts (default: false).
    pub bash_permissions: bool,
    /// Native sandbox policy for first-party bash and PTY processes.
    pub sandbox: SandboxConfig,
    /// Maximum file size to fully index in bytes (default: 1MB).
    pub search_index_max_file_size: u64,
    pub semantic: SemanticBackendConfig,
    pub inspect: InspectConfig,
    pub backup: BackupConfig,
    /// Linked-worktree RAM overlay. Default off; see [`WorktreeConfig`].
    pub worktree: WorktreeConfig,
    /// `gh` routing shim operator gate. Default on; see [`GhShimConfig`].
    pub gh_shim: GhShimConfig,
    /// Enable Astral ty as an experimental Python LSP server (default: false).
    pub experimental_lsp_ty: bool,
    /// User-defined LSP servers registered by the OpenCode plugin.
    pub lsp_servers: Vec<UserServerDef>,
    /// Lowercase LSP server IDs disabled by user config.
    pub disabled_lsp: HashSet<String>,
    /// Whether the system should request inline diagnostics after a tool call edits or writes a file.
    #[serde(skip)]
    pub diagnostics_on_edit: bool,
    /// Extra directories to search when resolving LSP binaries.
    /// The plugin populates these from its own auto-install cache (e.g.
    /// `~/.cache/aft/lsp-packages/<pkg>/node_modules/.bin/`) so a binary AFT
    /// installed itself is discoverable without needing it on PATH.
    /// Resolution order: `<project_root>/node_modules/.bin/<bin>` →
    /// `lsp_paths_extra/<bin>` (in order) → PATH via `which`. Python-family
    /// servers additionally probe the selected workspace's `.venv`/`venv` first.
    pub lsp_paths_extra: Vec<PathBuf>,
    /// Binary names the hosting plugin knows how to auto-install.
    ///
    /// Built-in LSPs discovered from files only emit missing-binary warnings
    /// when their binary is in this set. User-configured `lsp_servers` keep
    /// warning unconditionally.
    pub lsp_auto_install_binaries: HashSet<String>,
    /// Binary names with plugin-managed auto-installs currently in flight.
    ///
    /// Missing-binary warnings are suppressed while the install is actively
    /// running; install failure reporting is handled by the plugin after the
    /// background work settles.
    pub lsp_inflight_installs: HashSet<String>,
    /// Persistent storage directory for indexes (trigram, semantic).
    /// Set by the plugin to the XDG-compliant path (e.g. ~/.local/share/opencode/storage/plugin/aft/).
    /// Falls back to ~/.cache/aft/ if not set.
    pub storage_dir: Option<PathBuf>,
    /// Allow URL-fetch commands to access private network hosts.
    /// Default false; hosting plugins only forward this from user-level config.
    pub url_fetch_allow_private: bool,
    /// Hosting harness identity supplied by configure.
    #[serde(default)]
    pub harness: Option<Harness>,
    /// Maximum number of (server, file) entries kept in the in-memory
    /// diagnostic cache. Older entries are evicted in LRU order when the
    /// cap is exceeded. Set to 0 to disable the cap entirely.
    /// Default: 5000 (covers very large monorepos with bounded memory).
    pub diagnostic_cache_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            project_root: None,
            validation_depth: 1,
            checkpoint_ttl_hours: 24,
            max_symbol_depth: 10,
            formatter_timeout_secs: 10,
            type_checker_timeout_secs: 30,
            // Default OFF: formatting after an edit can silently reflow the file
            // under the agent (a formatter splitting/joining lines), staling the
            // context for the next edit/patch. Agents that want formatting opt in
            // via `format_on_edit: true`.
            format_on_edit: false,
            hashline_enabled: false,
            validate_on_edit: None,
            formatter: HashMap::new(),
            checker: HashMap::new(),
            // Default to false to match OpenCode's existing permission-based model.
            // The plugin opts into root restriction explicitly when desired.
            restrict_to_project_root: false,
            search_index: false,
            semantic_search: false,
            aft_search_registered: false,
            callgraph_store: true,
            callgraph_chunk_size: 100,
            experimental_bash_rewrite: false,
            experimental_bash_compress: false,
            experimental_bash_background: false,
            max_background_bash_tasks: 8,
            bash_long_running_reminder_enabled: true,
            bash_long_running_reminder_interval_ms: 600_000,
            foreground_wait_window_ms: default_foreground_wait_window_ms(),
            bash: BashConfig::default(),
            bash_permissions: false,
            sandbox: SandboxConfig::default(),
            search_index_max_file_size: 1_048_576,
            semantic: SemanticBackendConfig::default(),
            inspect: InspectConfig::default(),
            backup: BackupConfig::default(),
            worktree: WorktreeConfig::default(),
            gh_shim: GhShimConfig::default(),
            experimental_lsp_ty: false,
            lsp_servers: Vec::new(),
            disabled_lsp: HashSet::new(),
            diagnostics_on_edit: false,
            lsp_paths_extra: Vec::new(),
            lsp_auto_install_binaries: HashSet::new(),
            lsp_inflight_installs: HashSet::new(),
            storage_dir: None,
            url_fetch_allow_private: false,
            harness: None,
            diagnostic_cache_size: 5000,
        }
    }
}

fn default_foreground_wait_window_ms() -> u64 {
    15_000
}
