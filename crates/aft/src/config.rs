use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    pub max_batch_size: usize,
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
            max_batch_size: 64,
        }
    }
}

pub const DEFAULT_SEMANTIC_MODEL: &str = "all-MiniLM-L6-v2";

impl Config {
    pub fn semantic_backend_label(&self) -> &'static str {
        self.semantic.backend.as_str()
    }
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
    /// Enable OpenCode-style bash permission prompts (default: false).
    pub bash_permissions: bool,
    /// Maximum file size to fully index in bytes (default: 1MB).
    pub search_index_max_file_size: u64,
    /// Maximum number of source files allowed for call-graph operations
    /// (`callers`, `trace_to`, `trace_data`, `impact`). When a project
    /// exceeds this count the reverse index is not built and those
    /// commands return a `project_too_large` error. Does not affect
    /// `grep`, `glob`, `read`, `edit`, or other non-callgraph features.
    /// Default: 5_000 (matches measured per-op cost ceilings; raise for
    /// very large projects if you accept multi-minute per-call latency).
    pub max_callgraph_files: usize,
    pub semantic: SemanticBackendConfig,
    /// Enable Astral ty as an experimental Python LSP server (default: false).
    pub experimental_lsp_ty: bool,
    /// User-defined LSP servers registered by the OpenCode plugin.
    pub lsp_servers: Vec<UserServerDef>,
    /// Lowercase LSP server IDs disabled by user config.
    pub disabled_lsp: HashSet<String>,
    /// Extra directories to search when resolving LSP binaries.
    /// The plugin populates these from its own auto-install cache (e.g.
    /// `~/.cache/aft/lsp-packages/<pkg>/node_modules/.bin/`) so a binary AFT
    /// installed itself is discoverable without needing it on PATH.
    /// Resolution order: `<project_root>/node_modules/.bin/<bin>` →
    /// `lsp_paths_extra/<bin>` (in order) → PATH via `which`.
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
            format_on_edit: true,
            validate_on_edit: None,
            formatter: HashMap::new(),
            checker: HashMap::new(),
            // Default to false to match OpenCode's existing permission-based model.
            // The plugin opts into root restriction explicitly when desired.
            restrict_to_project_root: false,
            search_index: false,
            semantic_search: false,
            experimental_bash_rewrite: false,
            experimental_bash_compress: false,
            experimental_bash_background: false,
            max_background_bash_tasks: 8,
            bash_long_running_reminder_enabled: true,
            bash_long_running_reminder_interval_ms: 600_000,
            bash_permissions: false,
            search_index_max_file_size: 1_048_576,
            // Projects larger than this skip call-graph reverse index construction.
            //
            // The previous default (20_000) was set by hand-wave to "fits under
            // the 30 s bridge timeout" without measurement. Direct benchmarks
            // showed the cost is super-linear (tree-sitter parse + reverse-index
            // build per file): a 6.8K-file Rust project took 41 s — already past
            // the 60 s per-callgraph-op timeout. At 10 K extrapolated cost is
            // ~80–100 s; at 20 K it's 5+ minutes. So the old default routinely
            // produced "timed out, restarting bridge" rather than a clean
            // `project_too_large` rejection.
            //
            // 5_000 reflects measured reality: at this size, callgraph
            // operations on a real Rust/TS project complete in roughly 30–40 s,
            // matching the per-op timeout budget. Users with bigger projects
            // can raise this knob, but the default should not advertise
            // capabilities that fail in practice. Read/edit/grep/glob/outline/
            // semantic_search/AST/LSP all remain unaffected by this cap —
            // it only gates `aft_navigate` and `aft_refactor op="move"`.
            max_callgraph_files: 5_000,
            semantic: SemanticBackendConfig::default(),
            experimental_lsp_ty: false,
            lsp_servers: Vec::new(),
            disabled_lsp: HashSet::new(),
            lsp_paths_extra: Vec::new(),
            lsp_auto_install_binaries: HashSet::new(),
            lsp_inflight_installs: HashSet::new(),
            storage_dir: None,
            harness: None,
            diagnostic_cache_size: 5000,
        }
    }
}
