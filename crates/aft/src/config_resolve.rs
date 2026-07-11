//! Pure aft.jsonc tier resolver.
//!
//! This module mirrors the TypeScript config pipeline for the core-consumed
//! slice: raw JSONC tiers -> strict raw schema -> user/project trust merge ->
//! flat [`Config`]. It intentionally performs no IO; callers supply the already
//! read config documents.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::de;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::config::{
    BackupConfig, CheckConfig, Config, InspectConfig, SemanticBackend, SemanticBackendConfig,
    UserServerDef, MAX_SEMANTIC_QUERY_TIMEOUT_MS, MIN_SEMANTIC_QUERY_TIMEOUT_MS,
};
use crate::jsonc::strip_jsonc;

const FOREGROUND_WAIT_WINDOW_DEFAULT_MS: u64 = 15_000;
const FOREGROUND_WAIT_WINDOW_MIN_MS: u64 = 5_000;

// Semantic budget clamps — restored from the deleted configure-time
// parse_semantic_config so the tier-resolved Config matches the historical
// clamping (zero-behavior-change relocation, not a new policy).
const MAX_SEMANTIC_TIMEOUT_MS: u64 = 120_000;
const MAX_SEMANTIC_BATCH_SIZE: usize = 1_024;

const USER_ONLY_REASON: &str =
    "security: this setting only honors user-level config and project values are ignored";
const SEMANTIC_SECRET_REASON: &str =
    "security: semantic backend credentials and endpoints must come from user-level config";
const LSP_USER_ONLY_REASON: &str =
    "security: LSP executable-origin and diagnostic-suppression settings must come from user-level config";

/// One raw config document supplied by the host plugin.
///
/// `tier` is trusted process metadata stamped by the caller. The document body is
/// never allowed to relabel itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigTier {
    pub tier: String,
    pub source: String,
    pub doc: String,
}

/// A project-tier key that was intentionally ignored at the user/project trust boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedKey {
    pub key: String,
    pub tier: String,
    pub reason: String,
}

/// Fully resolved core config plus trust-boundary diagnostics.
#[derive(Debug, Clone)]
pub struct ResolveResult {
    pub config: Config,
    pub dropped: Vec<DroppedKey>,
}

/// Strict raw shape for aft.jsonc. This mirrors the TypeScript Zod schema, not
/// the flat runtime [`Config`]. Privileged process-state fields are deliberately
/// absent and therefore rejected by `deny_unknown_fields`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RawAftConfig {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    /// Master switch read by the TypeScript plugins before they start AFT.
    /// The resolver accepts and merges it for validation, but does not copy it
    /// into `Config`; when this is false the plugin returns before launching the
    /// Rust process.
    pub enabled: Option<bool>,
    pub format_on_edit: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_timeout_secs")]
    pub formatter_timeout_secs: Option<u32>,
    #[serde(deserialize_with = "deserialize_opt_timeout_secs")]
    pub type_checker_timeout_secs: Option<u32>,
    pub validate_on_edit: Option<RawValidateOnEdit>,
    pub formatter: Option<HashMap<String, RawFormatter>>,
    pub checker: Option<HashMap<String, RawChecker>>,
    pub configure_warnings_delivery: Option<RawConfigureWarningsDelivery>,
    pub hoist_builtin_tools: Option<bool>,
    pub tool_surface: Option<RawToolSurface>,
    pub disabled_tools: Option<Vec<String>>,
    pub restrict_to_project_root: Option<bool>,
    pub search_index: Option<bool>,
    pub semantic_search: Option<bool>,
    pub callgraph_store: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_usize")]
    pub callgraph_chunk_size: Option<usize>,
    pub inspect: Option<RawInspect>,
    pub check: Option<RawCheck>,
    pub backup: Option<RawBackup>,
    pub bash: Option<RawBash>,
    pub experimental: Option<RawExperimental>,
    pub lsp: Option<RawLsp>,
    pub url_fetch_allow_private: Option<bool>,
    pub semantic: Option<RawSemantic>,
    pub auto_update: Option<bool>,
    pub bridge: Option<RawBridge>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawValidateOnEdit {
    Syntax,
    Full,
}

impl RawValidateOnEdit {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawFormatter {
    Biome,
    Oxfmt,
    Prettier,
    Deno,
    Ruff,
    Black,
    Rustfmt,
    Goimports,
    Gofmt,
    None,
}

impl RawFormatter {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Biome => "biome",
            Self::Oxfmt => "oxfmt",
            Self::Prettier => "prettier",
            Self::Deno => "deno",
            Self::Ruff => "ruff",
            Self::Black => "black",
            Self::Rustfmt => "rustfmt",
            Self::Goimports => "goimports",
            Self::Gofmt => "gofmt",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawChecker {
    Tsc,
    Tsgo,
    Biome,
    Pyright,
    Ruff,
    Cargo,
    Go,
    Staticcheck,
    None,
}

impl RawChecker {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Tsc => "tsc",
            Self::Tsgo => "tsgo",
            Self::Biome => "biome",
            Self::Pyright => "pyright",
            Self::Ruff => "ruff",
            Self::Cargo => "cargo",
            Self::Go => "go",
            Self::Staticcheck => "staticcheck",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawConfigureWarningsDelivery {
    Toast,
    Log,
    Chat,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawToolSurface {
    Minimal,
    Recommended,
    All,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
// Nested objects mirror TS sub-schemas, which are non-strict z.object (unknown
// keys are silently stripped, the object survives). Only the TOP-LEVEL
// RawAftConfig is strict (matches AftConfigSchema.strict()). Privileged denylist
// fields are all top-level, so nested unknowns are harmlessly ignored.
pub struct RawSemantic {
    pub backend: Option<SemanticBackend>,
    #[serde(default, deserialize_with = "deserialize_opt_trimmed_non_empty_string")]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_trimmed_non_empty_string")]
    pub base_url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_trimmed_non_empty_string")]
    pub api_key_env: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_positive_u64")]
    pub timeout_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_positive_u64")]
    pub query_timeout_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_positive_usize")]
    pub max_batch_size: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_opt_positive_usize")]
    pub max_files: Option<usize>,
}

impl RawSemantic {
    fn is_empty(&self) -> bool {
        self.backend.is_none()
            && self.model.is_none()
            && self.base_url.is_none()
            && self.api_key_env.is_none()
            && self.timeout_ms.is_none()
            && self.query_timeout_ms.is_none()
            && self.max_batch_size.is_none()
            && self.max_files.is_none()
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RawLsp {
    #[serde(default, deserialize_with = "deserialize_opt_lsp_servers")]
    pub servers: Option<BTreeMap<String, RawLspServerEntry>>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_trimmed_non_empty_string_vec"
    )]
    pub disabled: Option<Vec<String>>,
    pub python: Option<RawPythonLsp>,
    pub diagnostics_on_edit: Option<bool>,
    pub auto_install: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_opt_positive_u64")]
    pub grace_days: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_versions_map")]
    pub versions: Option<HashMap<String, String>>,
}

impl RawLsp {
    fn is_empty(&self) -> bool {
        self.servers.is_none()
            && self.disabled.is_none()
            && self.python.is_none()
            && self.diagnostics_on_edit.is_none()
            && self.auto_install.is_none()
            && self.grace_days.is_none()
            && self.versions.is_none()
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawPythonLsp {
    Pyright,
    Ty,
    Auto,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawLspServerEntry {
    #[serde(deserialize_with = "deserialize_opt_lsp_extensions")]
    pub extensions: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_opt_trimmed_non_empty_string")]
    pub binary: Option<String>,
    pub args: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_opt_trimmed_non_empty_string_vec")]
    pub root_markers: Option<Vec<String>>,
    pub disabled: Option<bool>,
    pub env: Option<HashMap<String, String>>,
    pub initialization_options: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RawBash {
    Bool(bool),
    Features(RawBashFeatures),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawBashFeatures {
    pub rewrite: Option<bool>,
    pub compress: Option<bool>,
    pub background: Option<bool>,
    pub subagent_background: Option<bool>,
    pub long_running_reminder_enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_positive_u64")]
    pub long_running_reminder_interval_ms: Option<u64>,
    #[serde(deserialize_with = "deserialize_opt_positive_u64")]
    pub foreground_wait_window_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawExperimental {
    pub bash: Option<RawExperimentalBash>,
    pub lsp_ty: Option<bool>,
}

impl RawExperimental {
    fn is_empty(&self) -> bool {
        self.bash.is_none() && self.lsp_ty.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawExperimentalBash {
    pub rewrite: Option<bool>,
    pub compress: Option<bool>,
    pub background: Option<bool>,
    pub long_running_reminder_enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_positive_u64")]
    pub long_running_reminder_interval_ms: Option<u64>,
}

impl RawExperimentalBash {
    fn has_any_value(&self) -> bool {
        self.rewrite.is_some()
            || self.compress.is_some()
            || self.background.is_some()
            || self.long_running_reminder_enabled.is_some()
            || self.long_running_reminder_interval_ms.is_some()
    }

    fn has_legacy_feature_flag(&self) -> bool {
        self.rewrite.is_some() || self.compress.is_some() || self.background.is_some()
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawInspect {
    pub enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_opt_nonnegative_f64")]
    pub tier2_idle_minutes: Option<f64>,
    pub categories: Option<HashMap<String, bool>>,
    #[serde(deserialize_with = "deserialize_opt_positive_u64")]
    pub tier2_soft_deadline_ms: Option<u64>,
    #[serde(deserialize_with = "deserialize_opt_drill_down_items")]
    pub max_drill_down_items: Option<usize>,
    pub duplicates: Option<RawInspectDuplicates>,
}

impl RawInspect {
    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.tier2_idle_minutes.is_none()
            && self.categories.is_none()
            && self.tier2_soft_deadline_ms.is_none()
            && self.max_drill_down_items.is_none()
            && self.duplicates.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawCheck {
    pub enabled: Option<bool>,
}

impl RawCheck {
    fn is_empty(&self) -> bool {
        self.enabled.is_none()
    }
}

/// Only `expected_mirrors` survives here: `lower_bound`, `discard_cost`, and
/// `anonymize` were accepted-but-never-read knobs (the scanner hardcodes its
/// cost bounds and anonymization rules), so they were removed from the schema
/// rather than wired up.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RawInspectDuplicates {
    pub expected_mirrors: Option<Vec<[String; 2]>>,
}

impl RawInspectDuplicates {
    fn is_empty(&self) -> bool {
        self.expected_mirrors.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RawBridge {
    #[serde(deserialize_with = "deserialize_opt_bridge_request_timeout_ms")]
    pub request_timeout_ms: Option<u64>,
    #[serde(deserialize_with = "deserialize_opt_positive_u64")]
    pub hang_threshold: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RawBackup {
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_opt_positive_usize")]
    pub max_depth: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_opt_positive_u64")]
    pub max_file_size: Option<u64>,
}

/// Resolve raw user/project config tiers into the flat core [`Config`].
///
/// Empty input is NOT special-cased: no config file is equivalent to an empty
/// config object, so it still flows through the resolver and picks up the bash
/// surface default (recommended ⇒ bash on), matching the TypeScript pipeline
/// which always runs `resolveProjectOverridesForConfigure` even on `{}`.
pub fn resolve_config(tiers: &[ConfigTier]) -> ResolveResult {
    let mut merged = RawAftConfig::default();
    let mut dropped = Vec::new();

    for tier in tiers {
        let Some(raw) = parse_tier(tier) else {
            continue;
        };

        if tier.tier == "user" {
            merge_trusted_config(&mut merged, raw);
        } else {
            record_project_drops(&raw, &tier.tier, &mut dropped);
            merge_project_config(&mut merged, raw);
        }
    }

    let mut config = Config::default();
    apply_resolved_config(&merged, &mut config);
    ResolveResult { config, dropped }
}

/// Resolve raw config tiers into the core-domain config and RESET it onto an
/// existing `base`, preserving only `base`'s process-state fields (storage_dir,
/// harness, lsp_paths_extra, bash_permissions, …). This is the configure-path
/// entry.
///
/// RESET, not overlay: the core-domain config is rebuilt from DEFAULT + the
/// supplied tiers, so a field absent from the tiers returns to its default —
/// it NEVER keeps `base`'s prior value. This closes a cross-bind privilege
/// escalation: under the subc daemon a single `AppContext` per project root is
/// shared across harness identities, and `configure` seeds `base` from the
/// previous bind's config. With the old overlay semantics, a later low-trust
/// bind (e.g. `mcp:*` or `fed:*`) that omitted a field inherited an earlier high-trust
/// bind's capability for it (confirmed on the wire: `url_fetch_allow_private`
/// SSRF, and `lsp_servers` arbitrary-binary). Reset-onto-default makes the
/// resolved core config a pure function of THIS bind's own tiers.
///
/// Parity-safe by construction: this routes through [`resolve_config`] (the same
/// entry the cross-language parity gate validates), which builds onto
/// `Config::default()` — so reset-onto-default == overlay-onto-default there and
/// no parity/unit fixture changes. Only this configure path, seeded from a prior
/// config, changes behavior — which is exactly the leak site.
///
/// Process-state fields are not part of `RawAftConfig`; they are carried from
/// `base` here and re-applied by `handle_configure`'s flat-param parsing
/// afterwards, so plugin-mode behavior is unchanged (the plugin re-sends them on
/// every configure). They are also unreachable as a subc escalation vector: a
/// subc RouteBind sends only `config:[tiers]`, never the flat process-state
/// params, so they stay at default for every subc bind regardless.
pub fn resolve_config_onto(tiers: &[ConfigTier], base: &mut Config) -> Vec<DroppedKey> {
    let ResolveResult {
        mut config,
        dropped,
    } = resolve_config(tiers);
    carry_process_state(base, &mut config);
    *base = config;
    dropped
}

/// Carry the process-state (non-`RawAftConfig`) fields from `base` onto a
/// freshly-resolved core config. EVERY `Config` field not copied here is
/// core-domain and intentionally comes from the resolved tiers (reset). Keeping
/// this list complete is load-bearing: a core field accidentally copied here
/// would re-introduce cross-bind inheritance for it.
fn carry_process_state(base: &Config, resolved: &mut Config) {
    resolved.project_root = base.project_root.clone();
    resolved.harness = base.harness.clone();
    resolved.validation_depth = base.validation_depth;
    resolved.checkpoint_ttl_hours = base.checkpoint_ttl_hours;
    resolved.max_symbol_depth = base.max_symbol_depth;
    resolved.diagnostic_cache_size = base.diagnostic_cache_size;
    resolved.aft_search_registered = base.aft_search_registered;
    resolved.max_background_bash_tasks = base.max_background_bash_tasks;
    resolved.bash_permissions = base.bash_permissions;
    resolved.search_index_max_file_size = base.search_index_max_file_size;
    resolved.storage_dir = base.storage_dir.clone();
    resolved.lsp_paths_extra = base.lsp_paths_extra.clone();
    resolved.lsp_auto_install_binaries = base.lsp_auto_install_binaries.clone();
    resolved.lsp_inflight_installs = base.lsp_inflight_installs.clone();
}

fn parse_tier(tier: &ConfigTier) -> Option<RawAftConfig> {
    let stripped = strip_jsonc(&tier.doc);
    let value = serde_json::from_str::<Value>(&stripped).ok()?;
    let Value::Object(map) = value else {
        return None;
    };

    match serde_json::from_value::<RawAftConfig>(Value::Object(map.clone())) {
        Ok(config) => Some(config),
        Err(_) => Some(parse_config_partially(map)),
    }
}

fn parse_config_partially(raw_config: Map<String, Value>) -> RawAftConfig {
    let mut partial = RawAftConfig::default();

    for (key, value) in raw_config {
        let mut one_field = Map::new();
        one_field.insert(key, value);
        if let Ok(section) = serde_json::from_value::<RawAftConfig>(Value::Object(one_field)) {
            merge_trusted_config(&mut partial, section);
        }
    }

    partial
}

fn merge_trusted_config(base: &mut RawAftConfig, override_config: RawAftConfig) {
    if override_config.schema.is_some() {
        base.schema = override_config.schema;
    }
    if override_config.enabled.is_some() {
        base.enabled = override_config.enabled;
    }
    if override_config.format_on_edit.is_some() {
        base.format_on_edit = override_config.format_on_edit;
    }
    if override_config.formatter_timeout_secs.is_some() {
        base.formatter_timeout_secs = override_config.formatter_timeout_secs;
    }
    if override_config.type_checker_timeout_secs.is_some() {
        base.type_checker_timeout_secs = override_config.type_checker_timeout_secs;
    }
    if override_config.validate_on_edit.is_some() {
        base.validate_on_edit = override_config.validate_on_edit;
    }
    if override_config.formatter.is_some() {
        base.formatter = override_config.formatter;
    }
    if override_config.checker.is_some() {
        base.checker = override_config.checker;
    }
    if override_config.configure_warnings_delivery.is_some() {
        base.configure_warnings_delivery = override_config.configure_warnings_delivery;
    }
    if override_config.hoist_builtin_tools.is_some() {
        base.hoist_builtin_tools = override_config.hoist_builtin_tools;
    }
    if override_config.tool_surface.is_some() {
        base.tool_surface = override_config.tool_surface;
    }
    if override_config.disabled_tools.is_some() {
        base.disabled_tools = override_config.disabled_tools;
    }
    if override_config.restrict_to_project_root.is_some() {
        base.restrict_to_project_root = override_config.restrict_to_project_root;
    }
    if override_config.search_index.is_some() {
        base.search_index = override_config.search_index;
    }
    if override_config.semantic_search.is_some() {
        base.semantic_search = override_config.semantic_search;
    }
    if override_config.callgraph_store.is_some() {
        base.callgraph_store = override_config.callgraph_store;
    }
    if override_config.callgraph_chunk_size.is_some() {
        base.callgraph_chunk_size = override_config.callgraph_chunk_size;
    }
    if override_config.inspect.is_some() {
        base.inspect = override_config.inspect;
    }
    if override_config.check.is_some() {
        base.check = override_config.check;
    }
    if override_config.backup.is_some() {
        base.backup = override_config.backup;
    }
    if override_config.bash.is_some() {
        base.bash = override_config.bash;
    }
    if override_config.experimental.is_some() {
        base.experimental = override_config.experimental;
    }
    if override_config.lsp.is_some() {
        base.lsp = override_config.lsp;
    }
    if override_config.url_fetch_allow_private.is_some() {
        base.url_fetch_allow_private = override_config.url_fetch_allow_private;
    }
    if override_config.semantic.is_some() {
        base.semantic = override_config.semantic;
    }
    if override_config.auto_update.is_some() {
        base.auto_update = override_config.auto_update;
    }
    if override_config.bridge.is_some() {
        base.bridge = override_config.bridge;
    }
}

fn merge_project_config(base: &mut RawAftConfig, project: RawAftConfig) {
    // Project-safe shallow top-level fields.
    if project.enabled.is_some() {
        base.enabled = project.enabled;
    }
    if project.format_on_edit.is_some() {
        base.format_on_edit = project.format_on_edit;
    }
    if project.validate_on_edit.is_some() {
        base.validate_on_edit = project.validate_on_edit;
    }
    if project.configure_warnings_delivery.is_some() {
        base.configure_warnings_delivery = project.configure_warnings_delivery;
    }
    if project.hoist_builtin_tools.is_some() {
        base.hoist_builtin_tools = project.hoist_builtin_tools;
    }
    if project.tool_surface.is_some() {
        base.tool_surface = project.tool_surface;
    }
    if project.search_index.is_some() {
        base.search_index = project.search_index;
    }
    if project.semantic_search.is_some() {
        base.semantic_search = project.semantic_search;
    }
    if project.callgraph_store.is_some() {
        base.callgraph_store = project.callgraph_store;
    }
    if project.callgraph_chunk_size.is_some() {
        base.callgraph_chunk_size = project.callgraph_chunk_size;
    }

    merge_formatter_map(&mut base.formatter, project.formatter);
    merge_checker_map(&mut base.checker, project.checker);
    merge_disabled_tools(&mut base.disabled_tools, project.disabled_tools);
    base.semantic = merge_semantic_config(base.semantic.clone(), project.semantic);
    base.lsp = merge_lsp_config(base.lsp.clone(), project.lsp);
    base.experimental = merge_experimental_config(base.experimental.clone(), project.experimental);
    base.bash = merge_bash_config(base.bash.clone(), project.bash);
    base.inspect = merge_inspect_config(base.inspect.clone(), project.inspect);
    base.check = merge_check_config(base.check.clone(), project.check);
}

fn merge_formatter_map(
    base: &mut Option<HashMap<String, RawFormatter>>,
    override_map: Option<HashMap<String, RawFormatter>>,
) {
    let Some(override_map) = override_map else {
        return;
    };
    if override_map.is_empty() && base.as_ref().is_none_or(HashMap::is_empty) {
        return;
    }
    let target = base.get_or_insert_with(HashMap::new);
    target.extend(override_map);
}

fn merge_checker_map(
    base: &mut Option<HashMap<String, RawChecker>>,
    override_map: Option<HashMap<String, RawChecker>>,
) {
    let Some(override_map) = override_map else {
        return;
    };
    if override_map.is_empty() && base.as_ref().is_none_or(HashMap::is_empty) {
        return;
    }
    let target = base.get_or_insert_with(HashMap::new);
    target.extend(override_map);
}

fn merge_disabled_tools(base: &mut Option<Vec<String>>, override_tools: Option<Vec<String>>) {
    let Some(override_tools) = override_tools else {
        return;
    };
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for tool in base.iter().flatten() {
        if seen.insert(tool.clone()) {
            merged.push(tool.clone());
        }
    }
    for tool in override_tools
        .iter()
        .filter(|tool| tool.as_str() != "aft_safety")
    {
        if seen.insert(tool.clone()) {
            merged.push(tool.clone());
        }
    }
    if !merged.is_empty() {
        *base = Some(merged);
    }
}

fn merge_semantic_config(
    base: Option<RawSemantic>,
    override_semantic: Option<RawSemantic>,
) -> Option<RawSemantic> {
    let mut semantic = base.unwrap_or(RawSemantic {
        backend: None,
        model: None,
        base_url: None,
        api_key_env: None,
        timeout_ms: None,
        query_timeout_ms: None,
        max_batch_size: None,
        max_files: None,
    });

    if let Some(project) = override_semantic {
        if project.model.is_some() {
            semantic.model = project.model;
        }
        if project.timeout_ms.is_some() {
            semantic.timeout_ms = project.timeout_ms;
        }
        if project.max_batch_size.is_some() {
            semantic.max_batch_size = project.max_batch_size;
        }
        if project.max_files.is_some() {
            semantic.max_files = project.max_files;
        }
    }

    (!semantic.is_empty()).then_some(semantic)
}

fn merge_lsp_config(base: Option<RawLsp>, override_lsp: Option<RawLsp>) -> Option<RawLsp> {
    let mut lsp = base.unwrap_or(RawLsp {
        servers: None,
        disabled: None,
        python: None,
        diagnostics_on_edit: None,
        auto_install: None,
        grace_days: None,
        versions: None,
    });

    if let Some(project) = override_lsp {
        if project.python.is_some() {
            lsp.python = project.python;
        }
        if project.diagnostics_on_edit.is_some() {
            lsp.diagnostics_on_edit = project.diagnostics_on_edit;
        }
    }

    (!lsp.is_empty()).then_some(lsp)
}

fn merge_experimental_config(
    base: Option<RawExperimental>,
    override_experimental: Option<RawExperimental>,
) -> Option<RawExperimental> {
    let Some(override_experimental) = override_experimental else {
        return base;
    };

    let mut experimental = base.unwrap_or_default();
    experimental.lsp_ty = override_experimental.lsp_ty.or(experimental.lsp_ty);
    experimental.bash = merge_experimental_bash(experimental.bash, override_experimental.bash);

    (!experimental.is_empty()).then_some(experimental)
}

fn merge_experimental_bash(
    base: Option<RawExperimentalBash>,
    override_bash: Option<RawExperimentalBash>,
) -> Option<RawExperimentalBash> {
    let Some(override_bash) = override_bash else {
        return base;
    };
    let mut bash = base.unwrap_or_default();
    bash.rewrite = override_bash.rewrite.or(bash.rewrite);
    bash.compress = override_bash.compress.or(bash.compress);
    bash.background = override_bash.background.or(bash.background);
    bash.long_running_reminder_enabled = override_bash
        .long_running_reminder_enabled
        .or(bash.long_running_reminder_enabled);
    bash.long_running_reminder_interval_ms = override_bash
        .long_running_reminder_interval_ms
        .or(bash.long_running_reminder_interval_ms);

    bash.has_any_value().then_some(bash)
}

fn merge_bash_config(base: Option<RawBash>, override_bash: Option<RawBash>) -> Option<RawBash> {
    match (base, override_bash) {
        (None, None) => None,
        (None, Some(override_bash)) => Some(override_bash),
        (Some(base), None) => Some(base),
        (Some(base), Some(override_bash)) => {
            let base = expand_bash_for_merge(&base);
            let override_features = expand_bash_for_merge(&override_bash);
            Some(RawBash::Features(RawBashFeatures {
                rewrite: override_features.rewrite.or(base.rewrite),
                compress: override_features.compress.or(base.compress),
                background: override_features.background.or(base.background),
                subagent_background: override_features
                    .subagent_background
                    .or(base.subagent_background),
                long_running_reminder_enabled: override_features
                    .long_running_reminder_enabled
                    .or(base.long_running_reminder_enabled),
                long_running_reminder_interval_ms: override_features
                    .long_running_reminder_interval_ms
                    .or(base.long_running_reminder_interval_ms),
                foreground_wait_window_ms: override_features
                    .foreground_wait_window_ms
                    .or(base.foreground_wait_window_ms),
            }))
        }
    }
}

fn expand_bash_for_merge(value: &RawBash) -> RawBashFeatures {
    match value {
        RawBash::Bool(enabled) => RawBashFeatures {
            rewrite: Some(*enabled),
            compress: Some(*enabled),
            background: Some(*enabled),
            subagent_background: None,
            long_running_reminder_enabled: None,
            long_running_reminder_interval_ms: None,
            foreground_wait_window_ms: None,
        },
        RawBash::Features(features) => features.clone(),
    }
}

fn merge_inspect_config(
    base: Option<RawInspect>,
    override_inspect: Option<RawInspect>,
) -> Option<RawInspect> {
    let Some(override_inspect) = override_inspect else {
        return base;
    };

    let mut inspect = base.unwrap_or_default();
    inspect.enabled = override_inspect.enabled.or(inspect.enabled);
    inspect.tier2_idle_minutes = override_inspect
        .tier2_idle_minutes
        .or(inspect.tier2_idle_minutes);
    inspect.categories = override_inspect.categories.or(inspect.categories);
    inspect.tier2_soft_deadline_ms = override_inspect
        .tier2_soft_deadline_ms
        .or(inspect.tier2_soft_deadline_ms);
    inspect.max_drill_down_items = override_inspect
        .max_drill_down_items
        .or(inspect.max_drill_down_items);
    inspect.duplicates = merge_inspect_duplicates(inspect.duplicates, override_inspect.duplicates);

    (!inspect.is_empty()).then_some(inspect)
}

fn merge_check_config(
    base: Option<RawCheck>,
    override_check: Option<RawCheck>,
) -> Option<RawCheck> {
    let Some(override_check) = override_check else {
        return base;
    };
    let mut check = base.unwrap_or_default();
    check.enabled = override_check.enabled.or(check.enabled);
    (!check.is_empty()).then_some(check)
}

fn merge_inspect_duplicates(
    base: Option<RawInspectDuplicates>,
    override_duplicates: Option<RawInspectDuplicates>,
) -> Option<RawInspectDuplicates> {
    let Some(override_duplicates) = override_duplicates else {
        return base;
    };

    let mut duplicates = base.unwrap_or_default();
    duplicates.expected_mirrors = override_duplicates
        .expected_mirrors
        .or(duplicates.expected_mirrors);

    (!duplicates.is_empty()).then_some(duplicates)
}

fn record_project_drops(raw: &RawAftConfig, tier: &str, dropped: &mut Vec<DroppedKey>) {
    if raw.restrict_to_project_root.is_some() {
        push_drop(dropped, "restrict_to_project_root", tier, USER_ONLY_REASON);
    }
    if raw.url_fetch_allow_private.is_some() {
        push_drop(dropped, "url_fetch_allow_private", tier, USER_ONLY_REASON);
    }
    if raw.formatter_timeout_secs.is_some() {
        push_drop(dropped, "formatter_timeout_secs", tier, USER_ONLY_REASON);
    }
    if raw.type_checker_timeout_secs.is_some() {
        push_drop(dropped, "type_checker_timeout_secs", tier, USER_ONLY_REASON);
    }
    if raw.auto_update.is_some() {
        push_drop(dropped, "auto_update", tier, USER_ONLY_REASON);
    }
    if raw.bridge.is_some() {
        push_drop(dropped, "bridge", tier, USER_ONLY_REASON);
    }
    if raw.backup.is_some() {
        push_drop(dropped, "backup", tier, USER_ONLY_REASON);
    }
    if raw
        .disabled_tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| tool == "aft_safety"))
    {
        push_drop(dropped, "disabled_tools.aft_safety", tier, USER_ONLY_REASON);
    }

    if let Some(semantic) = &raw.semantic {
        if semantic.backend.is_some() {
            push_drop(dropped, "semantic.backend", tier, SEMANTIC_SECRET_REASON);
        }
        if semantic.base_url.is_some() {
            push_drop(dropped, "semantic.base_url", tier, SEMANTIC_SECRET_REASON);
        }
        if semantic.api_key_env.is_some() {
            push_drop(
                dropped,
                "semantic.api_key_env",
                tier,
                SEMANTIC_SECRET_REASON,
            );
        }
        if semantic.query_timeout_ms.is_some() {
            push_drop(dropped, "semantic.query_timeout_ms", tier, USER_ONLY_REASON);
        }
    }

    if let Some(lsp) = &raw.lsp {
        if lsp.servers.is_some() {
            push_drop(dropped, "lsp.servers", tier, LSP_USER_ONLY_REASON);
        }
        if lsp.versions.is_some() {
            push_drop(dropped, "lsp.versions", tier, LSP_USER_ONLY_REASON);
        }
        if lsp.auto_install.is_some() {
            push_drop(dropped, "lsp.auto_install", tier, LSP_USER_ONLY_REASON);
        }
        if lsp.grace_days.is_some() {
            push_drop(dropped, "lsp.grace_days", tier, LSP_USER_ONLY_REASON);
        }
        if lsp.disabled.is_some() {
            push_drop(dropped, "lsp.disabled", tier, LSP_USER_ONLY_REASON);
        }
    }
}

fn push_drop(dropped: &mut Vec<DroppedKey>, key: &str, tier: &str, reason: &str) {
    dropped.push(DroppedKey {
        key: key.to_string(),
        tier: tier.to_string(),
        reason: reason.to_string(),
    });
}

/// Overlay the resolved core-domain fields from `raw` onto `config`. Scalar
/// fields are only written when present in the merged tiers (preserving the
/// caller's base for absent keys); semantic/inspect/lsp are core-domain and are
/// fully resolved from the tiers. Process-state fields on `config` are never
/// touched (they are not part of `RawAftConfig`).
fn apply_resolved_config(raw: &RawAftConfig, config: &mut Config) {
    if let Some(value) = raw.format_on_edit {
        config.format_on_edit = value;
    }
    if let Some(value) = raw.formatter_timeout_secs {
        config.formatter_timeout_secs = value;
    }
    if let Some(value) = raw.type_checker_timeout_secs {
        config.type_checker_timeout_secs = value;
    }
    if let Some(value) = raw.validate_on_edit {
        config.validate_on_edit = Some(value.as_str().to_string());
    }
    if let Some(formatter) = &raw.formatter {
        config.formatter = formatter
            .iter()
            .map(|(language, formatter)| (language.clone(), formatter.as_str().to_string()))
            .collect();
    }
    if let Some(checker) = &raw.checker {
        config.checker = checker
            .iter()
            .map(|(language, checker)| (language.clone(), checker.as_str().to_string()))
            .collect();
    }
    if let Some(value) = raw.restrict_to_project_root {
        config.restrict_to_project_root = value;
    }
    if let Some(value) = raw.search_index {
        config.search_index = value;
    }
    if let Some(value) = raw.semantic_search {
        config.semantic_search = value;
    }
    if let Some(value) = raw.callgraph_store {
        config.callgraph_store = value;
    }
    if let Some(value) = raw.callgraph_chunk_size {
        config.callgraph_chunk_size = value;
    }
    if let Some(value) = raw.url_fetch_allow_private {
        config.url_fetch_allow_private = value;
    }
    config.semantic = resolve_semantic_config(raw.semantic.as_ref());
    config.inspect = resolve_inspect_config(raw.inspect.as_ref());
    config.check = resolve_check_config(raw.check.as_ref());
    config.backup = resolve_backup_config(raw.backup.as_ref());
    resolve_lsp_config(raw, config);
    resolve_bash_fields(raw, config);
}

fn resolve_semantic_config(raw: Option<&RawSemantic>) -> SemanticBackendConfig {
    let mut semantic = SemanticBackendConfig::default();
    let Some(raw) = raw else {
        return semantic;
    };

    if let Some(value) = raw.backend {
        semantic.backend = value;
    }
    if let Some(value) = &raw.model {
        semantic.model = value.clone();
    }
    if let Some(value) = &raw.base_url {
        semantic.base_url = Some(value.clone());
    }
    if let Some(value) = &raw.api_key_env {
        semantic.api_key_env = Some(value.clone());
    }
    if let Some(value) = raw.timeout_ms {
        semantic.timeout_ms = value.min(MAX_SEMANTIC_TIMEOUT_MS);
    }
    if let Some(value) = raw.query_timeout_ms {
        semantic.query_timeout_ms =
            value.clamp(MIN_SEMANTIC_QUERY_TIMEOUT_MS, MAX_SEMANTIC_QUERY_TIMEOUT_MS);
    }
    if let Some(value) = raw.max_batch_size {
        semantic.max_batch_size = value.min(MAX_SEMANTIC_BATCH_SIZE);
    }
    if let Some(value) = raw.max_files {
        semantic.max_files = value;
    }

    semantic
}

fn resolve_inspect_config(raw: Option<&RawInspect>) -> InspectConfig {
    let mut inspect = InspectConfig::default();
    let Some(raw) = raw else {
        return inspect;
    };
    if let Some(enabled) = raw.enabled {
        inspect.enabled = enabled;
    }
    if let Some(expected_mirrors) = raw
        .duplicates
        .as_ref()
        .and_then(|duplicates| duplicates.expected_mirrors.clone())
    {
        inspect.duplicates.expected_mirrors = expected_mirrors;
    }
    inspect
}

fn resolve_check_config(raw: Option<&RawCheck>) -> CheckConfig {
    let mut check = CheckConfig::default();
    if let Some(raw) = raw {
        if let Some(enabled) = raw.enabled {
            check.enabled = enabled;
        }
    }
    check
}

fn resolve_backup_config(raw: Option<&RawBackup>) -> BackupConfig {
    let mut backup = BackupConfig::default();
    if let Some(raw) = raw {
        if raw.enabled.is_some() {
            backup.enabled = raw.enabled;
        }
        if raw.max_depth.is_some() {
            backup.max_depth = raw.max_depth;
        }
        if raw.max_file_size.is_some() {
            backup.max_file_size = raw.max_file_size;
        }
    }
    backup
}

fn resolve_lsp_config(raw: &RawAftConfig, config: &mut Config) {
    let lsp = raw.lsp.as_ref();
    let mut disabled: HashSet<String> = lsp
        .and_then(|lsp| lsp.disabled.as_ref())
        .into_iter()
        .flatten()
        .map(|value| value.to_ascii_lowercase())
        .collect();
    let mut experimental_ty = raw
        .experimental
        .as_ref()
        .and_then(|experimental| experimental.lsp_ty);

    match lsp.and_then(|lsp| lsp.python).unwrap_or(RawPythonLsp::Auto) {
        RawPythonLsp::Ty => {
            experimental_ty = Some(true);
            disabled.insert("python".to_string());
        }
        RawPythonLsp::Pyright => {
            experimental_ty = Some(false);
            disabled.insert("ty".to_string());
        }
        RawPythonLsp::Auto => {}
    }

    if let Some(value) = experimental_ty {
        config.experimental_lsp_ty = value;
    }

    if let Some(value) = lsp.and_then(|lsp| lsp.diagnostics_on_edit) {
        config.diagnostics_on_edit = value;
    }

    if let Some(servers) = lsp.and_then(|lsp| lsp.servers.as_ref()) {
        config.lsp_servers = servers
            .iter()
            .map(|(id, server)| UserServerDef {
                id: id.clone(),
                extensions: server
                    .extensions
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|extension| extension.trim_start_matches('.').to_string())
                    .collect(),
                binary: server.binary.clone().unwrap_or_default(),
                args: server.args.clone().unwrap_or_default(),
                root_markers: server
                    .root_markers
                    .clone()
                    .unwrap_or_else(|| vec![".git".to_string()]),
                env: server.env.clone().unwrap_or_default(),
                initialization_options: server.initialization_options.clone(),
                disabled: server.disabled.unwrap_or(false),
            })
            .collect();
    }

    if !disabled.is_empty() {
        config.disabled_lsp = disabled;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedBashConfig {
    enabled: bool,
    rewrite: bool,
    compress: bool,
    background: bool,
    subagent_background: bool,
    long_running_reminder_enabled: Option<bool>,
    long_running_reminder_interval_ms: Option<u64>,
    foreground_wait_window_ms: u64,
}

fn resolve_bash_fields(raw: &RawAftConfig, config: &mut Config) {
    let bash = resolve_bash_config(raw);
    // Resolve enabled and subagent_background, then discard them, because Rust
    // does not use those two bash settings at runtime yet. Doing the resolution
    // here keeps user config overriding project config in the same way as the
    // bash fields below, so a later response can expose the values without
    // changing how config files are interpreted.
    let _registration_only = (bash.enabled, bash.subagent_background);
    config.experimental_bash_rewrite = bash.rewrite;
    config.experimental_bash_compress = bash.compress;
    config.experimental_bash_background = bash.background;
    config.foreground_wait_window_ms = bash.foreground_wait_window_ms;
    if let Some(value) = bash.long_running_reminder_enabled {
        config.bash_long_running_reminder_enabled = value;
    }
    if let Some(value) = bash.long_running_reminder_interval_ms {
        config.bash_long_running_reminder_interval_ms = value;
    }
}

fn resolve_bash_config(raw: &RawAftConfig) -> ResolvedBashConfig {
    let top = raw.bash.as_ref();
    let legacy = raw
        .experimental
        .as_ref()
        .and_then(|experimental| experimental.bash.as_ref());
    let surface = raw.tool_surface.unwrap_or(RawToolSurface::Recommended);
    let surface_default_enabled = surface != RawToolSurface::Minimal;

    let top_features = match top {
        Some(RawBash::Features(features)) => Some(features),
        _ => None,
    };
    let reminder_enabled = top_features
        .and_then(|features| features.long_running_reminder_enabled)
        .or_else(|| legacy.and_then(|legacy| legacy.long_running_reminder_enabled));
    let reminder_interval = top_features
        .and_then(|features| features.long_running_reminder_interval_ms)
        .or_else(|| legacy.and_then(|legacy| legacy.long_running_reminder_interval_ms));
    let top_subagent_background = top_features
        .and_then(|features| features.subagent_background)
        .unwrap_or(false);
    let raw_foreground_wait = top_features.and_then(|features| features.foreground_wait_window_ms);
    let foreground_wait_window_ms = raw_foreground_wait
        .unwrap_or(FOREGROUND_WAIT_WINDOW_DEFAULT_MS)
        .max(FOREGROUND_WAIT_WINDOW_MIN_MS);

    let base = ResolvedBashConfig {
        enabled: false,
        rewrite: false,
        compress: false,
        background: false,
        subagent_background: false,
        long_running_reminder_enabled: reminder_enabled,
        long_running_reminder_interval_ms: reminder_interval,
        foreground_wait_window_ms,
    };

    match top {
        Some(RawBash::Bool(false)) => base,
        Some(RawBash::Bool(true)) => ResolvedBashConfig {
            enabled: true,
            rewrite: true,
            compress: true,
            background: true,
            ..base
        },
        Some(RawBash::Features(features)) => ResolvedBashConfig {
            enabled: true,
            rewrite: features.rewrite.unwrap_or(true),
            compress: features.compress.unwrap_or(true),
            background: features.background.unwrap_or(true),
            subagent_background: top_subagent_background,
            ..base
        },
        None => {
            if legacy.is_some_and(RawExperimentalBash::has_legacy_feature_flag) {
                let legacy = legacy.cloned().unwrap_or_default();
                let rewrite = legacy.rewrite == Some(true);
                let compress = legacy.compress == Some(true);
                let background = legacy.background == Some(true);
                return ResolvedBashConfig {
                    enabled: rewrite || compress || background,
                    rewrite,
                    compress,
                    background,
                    ..base
                };
            }

            ResolvedBashConfig {
                enabled: surface_default_enabled,
                rewrite: surface_default_enabled,
                compress: surface_default_enabled,
                background: surface_default_enabled,
                ..base
            }
        }
    }
}

fn deserialize_opt_trimmed_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                Err(de::Error::custom("must be a non-empty string"))
            } else {
                Ok(trimmed)
            }
        })
        .transpose()
}

fn deserialize_opt_trimmed_non_empty_string_vec<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Vec<String>>::deserialize(deserializer)?;
    value
        .map(|values| {
            values
                .into_iter()
                .map(|value| {
                    let trimmed = value.trim().to_string();
                    if trimmed.is_empty() {
                        Err(de::Error::custom("array entries must be non-empty strings"))
                    } else {
                        Ok(trimmed)
                    }
                })
                .collect()
        })
        .transpose()
}

fn deserialize_opt_lsp_extensions<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Vec<String>>::deserialize(deserializer)?;
    value
        .map(|values| {
            if values.is_empty() {
                return Err(de::Error::custom(
                    "extensions must contain at least one entry",
                ));
            }
            values
                .into_iter()
                .map(|value| {
                    let trimmed = value.trim().to_string();
                    if trimmed.is_empty() || trimmed.trim_start_matches('.').is_empty() {
                        Err(de::Error::custom(
                            "extension must include characters other than leading dots",
                        ))
                    } else {
                        Ok(trimmed)
                    }
                })
                .collect()
        })
        .transpose()
}

fn deserialize_opt_lsp_servers<'de, D>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, RawLspServerEntry>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<BTreeMap<String, RawLspServerEntry>>::deserialize(deserializer)?;
    value
        .map(|entries| {
            entries
                .into_iter()
                .map(|(key, value)| {
                    let trimmed = key.trim().to_string();
                    if trimmed.is_empty() {
                        Err(de::Error::custom(
                            "lsp.servers keys must be non-empty strings",
                        ))
                    } else {
                        Ok((trimmed, value))
                    }
                })
                .collect()
        })
        .transpose()
}

fn deserialize_opt_versions_map<'de, D>(
    deserializer: D,
) -> Result<Option<HashMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<HashMap<String, String>>::deserialize(deserializer)?;
    value
        .map(|entries| {
            entries
                .into_iter()
                .map(|(key, value)| {
                    let trimmed_key = key.trim().to_string();
                    let trimmed_value = value.trim().to_string();
                    if trimmed_key.is_empty() || trimmed_value.is_empty() {
                        Err(de::Error::custom(
                            "lsp.versions keys and values must be non-empty strings",
                        ))
                    } else {
                        Ok((trimmed_key, trimmed_value))
                    }
                })
                .collect()
        })
        .transpose()
}

fn deserialize_opt_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    value
        .map(|value| usize::try_from(value).map_err(|_| de::Error::custom("value is too large")))
        .transpose()
}

fn deserialize_opt_positive_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    match value {
        Some(0) => Err(de::Error::custom("must be a positive integer")),
        other => Ok(other),
    }
}

fn deserialize_opt_positive_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = deserialize_opt_positive_u64(deserializer)?;
    value
        .map(|value| usize::try_from(value).map_err(|_| de::Error::custom("value is too large")))
        .transpose()
}

fn deserialize_opt_timeout_secs<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    match value {
        Some(value) if !(1..=600).contains(&value) => {
            Err(de::Error::custom("timeout must be in 1..=600 seconds"))
        }
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| de::Error::custom("timeout is too large")),
        None => Ok(None),
    }
}

fn deserialize_opt_bridge_request_timeout_ms<'de, D>(
    deserializer: D,
) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    match value {
        Some(value) if value < 1_000 => Err(de::Error::custom(
            "bridge.request_timeout_ms must be at least 1000",
        )),
        other => Ok(other),
    }
}

fn deserialize_opt_nonnegative_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<f64>::deserialize(deserializer)?;
    match value {
        Some(value) if value < 0.0 => Err(de::Error::custom("must be non-negative")),
        other => Ok(other),
    }
}

fn deserialize_opt_drill_down_items<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    match value {
        Some(value) if value == 0 || value > 100 => {
            Err(de::Error::custom("max_drill_down_items must be in 1..=100"))
        }
        Some(value) => usize::try_from(value)
            .map(Some)
            .map_err(|_| de::Error::custom("max_drill_down_items is too large")),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(tier: &str, doc: &str) -> ConfigTier {
        ConfigTier {
            tier: tier.to_string(),
            source: format!("/tmp/{tier}/aft.jsonc"),
            doc: doc.to_string(),
        }
    }

    fn drop_keys(result: &ResolveResult) -> Vec<String> {
        result
            .dropped
            .iter()
            .map(|dropped| dropped.key.clone())
            .collect()
    }

    /// Security invariant (Oracle drift decision): nested objects are non-strict
    /// (match TS z.object — unknown nested keys are stripped, object survives),
    /// but the TOP-LEVEL RawAftConfig stays strict. A privileged process-state
    /// field is top-level, so a project tier trying to smuggle one still hits the
    /// strict top-level → that tier fails full parse, and partial-parse drops the
    /// unknown key. It can NEVER reach Config.
    #[test]
    fn nested_unknown_keys_are_stripped_but_top_level_privileged_keys_cannot_smuggle() {
        // Nested unknown key: stripped, object survives — parity with TS (golden
        // `bash_unknown_nested_key`). `bash: { unknown_key }` resolves like
        // `bash: {}` → object form → bash ENABLED (object presence beats the
        // minimal surface default). The point: the unknown key did not fail the
        // parse — the object survived and resolved.
        let nested = resolve_config(&[tier(
            "user",
            r#"{ "tool_surface": "minimal", "bash": { "unknown_key": true } }"#,
        )]);
        assert!(nested.config.experimental_bash_rewrite);
        assert!(nested.config.experimental_bash_compress);
        assert!(nested.config.experimental_bash_background);

        // Top-level privileged process-state field (storage_dir) from a PROJECT
        // tier: not in RawAftConfig → full parse fails → partial-parse drops it.
        // It must never appear in Config (Config keeps its default storage_dir).
        let smuggle = resolve_config(&[
            tier("user", r#"{ "search_index": true }"#),
            tier(
                "project",
                r#"{ "storage_dir": "/tmp/evil", "bash_permissions": true, "search_index": false }"#,
            ),
        ]);
        // The valid project key (search_index) still applies via partial-parse...
        assert!(!smuggle.config.search_index);
        // ...but the smuggled process-state fields never reach Config.
        assert!(smuggle.config.storage_dir.is_none());
        assert!(!smuggle.config.bash_permissions);
    }

    #[test]
    fn config_resolve_empty_tiers_applies_bash_surface_default() {
        // No config file ⇒ empty config object, which still flows through the
        // resolver and picks up the bash surface default (recommended ⇒ on),
        // matching the TS pipeline (golden fixture `empty`). NOT Config::default()
        // — that would leave bash off, diverging from TS.
        let result = resolve_config(&[]);
        let default_config = Config::default();

        assert!(result.dropped.is_empty());
        // Non-bash fields stay at runtime default.
        assert_eq!(result.config.format_on_edit, default_config.format_on_edit);
        assert_eq!(result.config.search_index, default_config.search_index);
        assert_eq!(
            result.config.semantic_search,
            default_config.semantic_search
        );
        assert_eq!(result.config.semantic, default_config.semantic);
        assert_eq!(
            result.config.inspect.enabled,
            default_config.inspect.enabled
        );
        assert_eq!(result.config.lsp_servers.len(), 0);
        // Bash surface default: recommended ⇒ rewrite/compress/background all on.
        assert!(result.config.experimental_bash_rewrite);
        assert!(result.config.experimental_bash_compress);
        assert!(result.config.experimental_bash_background);
    }

    #[test]
    fn config_resolve_user_only_config_applies_fields() {
        let result = resolve_config(&[tier(
            "user",
            r#"{
              "$schema": "https://example.test/aft.schema.json",
              "format_on_edit": false,
              "formatter_timeout_secs": 42,
              "type_checker_timeout_secs": 43,
              "validate_on_edit": "full",
              "formatter": { "rust": "rustfmt", "typescript": "prettier" },
              "checker": { "rust": "cargo", "typescript": "tsc" },
              "restrict_to_project_root": true,
              "search_index": true,
              "semantic_search": true,
              "callgraph_store": false,
              "callgraph_chunk_size": 17,
              "url_fetch_allow_private": true,
              "semantic": {
                "backend": "openai_compatible",
                "model": "  user-model  ",
                "base_url": "https://semantic.example.test",
                "api_key_env": "AFT_API_KEY",
                "timeout_ms": 12345,
                "query_timeout_ms": 2345,
                "max_batch_size": 12,
                "max_files": 3456
              },
              "inspect": { "enabled": false },
              "experimental": { "lsp_ty": true },
              "lsp": {
                "servers": {
                  "rust": { "extensions": [".rs"], "binary": "rust-analyzer" }
                },
                "disabled": ["Python"],
                "python": "pyright"
              },
              "bash": { "rewrite": false, "compress": true, "background": false,
                        "long_running_reminder_enabled": false,
                        "long_running_reminder_interval_ms": 123000 }
            }"#,
        )]);

        assert!(result.dropped.is_empty());
        assert!(!result.config.format_on_edit);
        assert_eq!(result.config.formatter_timeout_secs, 42);
        assert_eq!(result.config.type_checker_timeout_secs, 43);
        assert_eq!(result.config.validate_on_edit.as_deref(), Some("full"));
        assert_eq!(
            result.config.formatter.get("rust").map(String::as_str),
            Some("rustfmt")
        );
        assert_eq!(
            result.config.checker.get("typescript").map(String::as_str),
            Some("tsc")
        );
        assert!(result.config.restrict_to_project_root);
        assert!(result.config.search_index);
        assert!(result.config.semantic_search);
        assert!(!result.config.callgraph_store);
        assert_eq!(result.config.callgraph_chunk_size, 17);
        assert!(result.config.url_fetch_allow_private);
        assert_eq!(
            result.config.semantic.backend,
            SemanticBackend::OpenAiCompatible
        );
        assert_eq!(result.config.semantic.model, "user-model");
        assert_eq!(
            result.config.semantic.base_url.as_deref(),
            Some("https://semantic.example.test")
        );
        assert_eq!(
            result.config.semantic.api_key_env.as_deref(),
            Some("AFT_API_KEY")
        );
        assert_eq!(result.config.semantic.timeout_ms, 12345);
        assert_eq!(result.config.semantic.query_timeout_ms, 2345);
        assert_eq!(result.config.semantic.max_batch_size, 12);
        assert_eq!(result.config.semantic.max_files, 3456);
        assert!(!result.config.inspect.enabled);
        assert!(!result.config.experimental_lsp_ty);
        assert!(result.config.disabled_lsp.contains("ty"));
        assert_eq!(result.config.lsp_servers.len(), 1);
        assert_eq!(result.config.lsp_servers[0].id, "rust");
        assert_eq!(
            result.config.lsp_servers[0].extensions,
            vec!["rs".to_string()]
        );
        assert_eq!(result.config.lsp_servers[0].binary, "rust-analyzer");
        assert_eq!(result.config.lsp_servers[0].args, Vec::<String>::new());
        assert_eq!(
            result.config.lsp_servers[0].root_markers,
            vec![".git".to_string()]
        );
        assert!(!result.config.experimental_bash_rewrite);
        assert!(result.config.experimental_bash_compress);
        assert!(!result.config.experimental_bash_background);
        assert!(!result.config.bash_long_running_reminder_enabled);
        assert_eq!(result.config.bash_long_running_reminder_interval_ms, 123000);
    }

    #[test]
    fn semantic_query_timeout_clamps_to_interactive_budget_range() {
        let below_min =
            resolve_config(&[tier("user", r#"{ "semantic": { "query_timeout_ms": 1 } }"#)]);
        assert_eq!(
            below_min.config.semantic.query_timeout_ms,
            MIN_SEMANTIC_QUERY_TIMEOUT_MS
        );

        let above_max = resolve_config(&[tier(
            "user",
            r#"{ "semantic": { "query_timeout_ms": 50000 } }"#,
        )]);
        assert_eq!(
            above_max.config.semantic.query_timeout_ms,
            MAX_SEMANTIC_QUERY_TIMEOUT_MS
        );
    }

    #[test]
    fn config_resolve_project_allowed_search_index_wins() {
        let result = resolve_config(&[
            tier("user", r#"{ "search_index": false }"#),
            tier("project", r#"{ "search_index": true }"#),
        ]);

        assert!(result.config.search_index);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn config_resolve_project_user_only_keys_are_dropped_and_user_values_win() {
        let result = resolve_config(&[
            tier(
                "user",
                r#"{
                  "restrict_to_project_root": true,
                  "url_fetch_allow_private": true,
                  "formatter_timeout_secs": 11,
                  "type_checker_timeout_secs": 33,
                  "auto_update": true,
                  "bridge": { "request_timeout_ms": 3000, "hang_threshold": 3 },
                  "semantic": {
                    "backend": "openai_compatible",
                    "base_url": "https://user.example.test",
                    "api_key_env": "USER_KEY",
                    "model": "user-model",
                    "query_timeout_ms": 900
                  },
                  "lsp": {
                    "servers": {
                      "rust": { "extensions": [".rs"], "binary": "rust-analyzer" }
                    },
                    "disabled": ["user-disabled"],
                    "versions": { "typescript-language-server": "1.0.0" },
                    "auto_install": true,
                    "grace_days": 7
                  }
                }"#,
            ),
            tier(
                "project",
                r#"{
                  "restrict_to_project_root": false,
                  "url_fetch_allow_private": false,
                  "formatter_timeout_secs": 22,
                  "type_checker_timeout_secs": 44,
                  "auto_update": false,
                  "bridge": { "request_timeout_ms": 4000, "hang_threshold": 4 },
                  "semantic": {
                    "backend": "ollama",
                    "base_url": "https://project.example.test",
                    "api_key_env": "PROJECT_KEY",
                    "model": "project-model",
                    "timeout_ms": 2222,
                    "query_timeout_ms": 2222
                  },
                  "lsp": {
                    "servers": {
                      "rust": { "extensions": [".evil"], "binary": "evil-lsp" }
                    },
                    "disabled": ["project-disabled"],
                    "versions": { "evil-lsp": "9.9.9" },
                    "auto_install": false,
                    "grace_days": 1,
                    "python": "ty"
                  }
                }"#,
            ),
        ]);

        assert!(result.config.restrict_to_project_root);
        assert!(result.config.url_fetch_allow_private);
        assert_eq!(result.config.formatter_timeout_secs, 11);
        assert_eq!(result.config.type_checker_timeout_secs, 33);
        assert_eq!(
            result.config.semantic.backend,
            SemanticBackend::OpenAiCompatible
        );
        assert_eq!(
            result.config.semantic.base_url.as_deref(),
            Some("https://user.example.test")
        );
        assert_eq!(
            result.config.semantic.api_key_env.as_deref(),
            Some("USER_KEY")
        );
        assert_eq!(result.config.semantic.model, "project-model");
        assert_eq!(result.config.semantic.timeout_ms, 2222);
        assert_eq!(result.config.semantic.query_timeout_ms, 900);
        assert_eq!(result.config.lsp_servers.len(), 1);
        assert_eq!(result.config.lsp_servers[0].binary, "rust-analyzer");
        assert!(result.config.disabled_lsp.contains("user-disabled"));
        assert!(!result.config.disabled_lsp.contains("project-disabled"));
        assert!(result.config.disabled_lsp.contains("python"));
        assert!(result.config.experimental_lsp_ty);

        let keys = drop_keys(&result);
        let expected = [
            "restrict_to_project_root",
            "url_fetch_allow_private",
            "formatter_timeout_secs",
            "type_checker_timeout_secs",
            "auto_update",
            "bridge",
            "semantic.backend",
            "semantic.base_url",
            "semantic.api_key_env",
            "semantic.query_timeout_ms",
            "lsp.servers",
            "lsp.versions",
            "lsp.auto_install",
            "lsp.grace_days",
            "lsp.disabled",
        ];
        for key in expected {
            assert!(keys.contains(&key.to_string()), "missing dropped key {key}");
        }
        assert_eq!(keys.len(), expected.len());
        assert!(result
            .dropped
            .iter()
            .all(|dropped| dropped.tier == "project"));
    }

    #[test]
    fn config_resolve_bash_ladder_and_merge_parity() {
        let true_result = resolve_config(&[tier("user", r#"{ "bash": true }"#)]);
        assert!(true_result.config.experimental_bash_rewrite);
        assert!(true_result.config.experimental_bash_compress);
        assert!(true_result.config.experimental_bash_background);

        let false_result = resolve_config(&[tier("user", r#"{ "bash": false }"#)]);
        assert!(!false_result.config.experimental_bash_rewrite);
        assert!(!false_result.config.experimental_bash_compress);
        assert!(!false_result.config.experimental_bash_background);

        let object_default_result = resolve_config(&[tier("user", r#"{ "bash": {} }"#)]);
        assert!(object_default_result.config.experimental_bash_rewrite);
        assert!(object_default_result.config.experimental_bash_compress);
        assert!(object_default_result.config.experimental_bash_background);

        let object_partial_result =
            resolve_config(&[tier("user", r#"{ "bash": { "compress": false } }"#)]);
        assert!(object_partial_result.config.experimental_bash_rewrite);
        assert!(!object_partial_result.config.experimental_bash_compress);
        assert!(object_partial_result.config.experimental_bash_background);

        let legacy_result = resolve_config(&[tier(
            "user",
            r#"{ "experimental": { "bash": { "rewrite": true } } }"#,
        )]);
        assert!(legacy_result.config.experimental_bash_rewrite);
        assert!(!legacy_result.config.experimental_bash_compress);
        assert!(!legacy_result.config.experimental_bash_background);

        let surface_default_result = resolve_config(&[tier("user", r#"{}"#)]);
        assert!(surface_default_result.config.experimental_bash_rewrite);
        assert!(surface_default_result.config.experimental_bash_compress);
        assert!(surface_default_result.config.experimental_bash_background);

        let minimal_surface_result =
            resolve_config(&[tier("user", r#"{ "tool_surface": "minimal" }"#)]);
        assert!(!minimal_surface_result.config.experimental_bash_rewrite);
        assert!(!minimal_surface_result.config.experimental_bash_compress);
        assert!(!minimal_surface_result.config.experimental_bash_background);

        let merged_result = resolve_config(&[
            tier("user", r#"{ "bash": true }"#),
            tier("project", r#"{ "bash": { "compress": false } }"#),
        ]);
        assert!(merged_result.config.experimental_bash_rewrite);
        assert!(!merged_result.config.experimental_bash_compress);
        assert!(merged_result.config.experimental_bash_background);

        let false_then_object_result = resolve_config(&[
            tier("user", r#"{ "bash": false }"#),
            tier("project", r#"{ "bash": { "compress": true } }"#),
        ]);
        assert!(!false_then_object_result.config.experimental_bash_rewrite);
        assert!(false_then_object_result.config.experimental_bash_compress);
        assert!(!false_then_object_result.config.experimental_bash_background);
    }

    #[test]
    fn config_resolve_bash_foreground_wait_clamps_to_floor() {
        let Some(raw) = parse_tier(&tier(
            "user",
            r#"{ "bash": { "foreground_wait_window_ms": 1, "subagent_background": true } }"#,
        )) else {
            panic!("test tier should parse");
        };
        let bash = resolve_bash_config(&raw);

        assert_eq!(
            bash.foreground_wait_window_ms,
            FOREGROUND_WAIT_WINDOW_MIN_MS
        );
        assert!(bash.subagent_background);

        let result = resolve_config(&[tier(
            "user",
            r#"{ "bash": { "foreground_wait_window_ms": 1 } }"#,
        )]);
        assert_eq!(
            result.config.foreground_wait_window_ms,
            FOREGROUND_WAIT_WINDOW_MIN_MS
        );

        // Unset → the default wait-window (matches the plugin's
        // FOREGROUND_WAIT_WINDOW_DEFAULT_MS = 15_000). This locks the default,
        // not just the floor, so a future edit can't silently shorten the
        // server-side promotion window once the plugin orchestrates through Rust.
        let defaulted = resolve_config(&[tier("user", r#"{ "bash": true }"#)]);
        assert_eq!(
            defaulted.config.foreground_wait_window_ms,
            FOREGROUND_WAIT_WINDOW_DEFAULT_MS
        );
        assert_eq!(FOREGROUND_WAIT_WINDOW_DEFAULT_MS, 15_000);
    }

    #[test]
    fn config_resolve_partial_parse_drops_invalid_section_and_keeps_valid_sections() {
        let result = resolve_config(&[tier(
            "user",
            r#"{
              "semantic": { "timeout_ms": 0 },
              "search_index": true,
              "format_on_edit": false
            }"#,
        )]);

        assert!(result.config.search_index);
        assert!(!result.config.format_on_edit);
        assert_eq!(result.config.semantic, SemanticBackendConfig::default());
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn config_resolve_unknown_top_level_key_is_dropped_but_rest_survives() {
        let result = resolve_config(&[tier(
            "user",
            r#"{ "not_a_real_key": true, "search_index": true }"#,
        )]);

        assert!(result.config.search_index);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn resolve_config_onto_resets_core_fields_no_cross_bind_inheritance() {
        // Cross-bind escalation regression: a first (trusted) bind sets a
        // capability field; a second bind that omits it must NOT inherit it.
        // This is the configure-path entry (seeded from a prior config), where
        // reset-onto-default — unlike the old overlay — makes the resolved core
        // config a pure function of the CURRENT bind's tiers.
        let mut config = Config::default();

        // Bind 1 (trusted user tier) sets url_fetch_allow_private + a custom LSP
        // server + restrict_to_project_root.
        let dropped1 = resolve_config_onto(
            &[tier(
                "user",
                r#"{
                  "url_fetch_allow_private": true,
                  "restrict_to_project_root": true,
                  "lsp": { "servers": { "rust": { "extensions": [".rs"], "binary": "rust-analyzer" } } }
                }"#,
            )],
            &mut config,
        );
        assert!(dropped1.is_empty());
        assert!(config.url_fetch_allow_private);
        assert!(config.restrict_to_project_root);
        assert_eq!(config.lsp_servers.len(), 1);

        // Bind 2 omits all three. With reset semantics they return to DEFAULT —
        // the second bind cannot inherit the first bind's capabilities.
        let _ = resolve_config_onto(&[tier("user", r#"{ "search_index": true }"#)], &mut config);
        assert!(
            !config.url_fetch_allow_private,
            "url_fetch_allow_private must reset to default, not inherit prior bind"
        );
        assert!(
            !config.restrict_to_project_root,
            "restrict_to_project_root must reset to default"
        );
        assert!(
            config.lsp_servers.is_empty(),
            "lsp_servers must reset to default, not inherit prior bind's custom server"
        );
        assert!(config.search_index, "this bind's own field still applies");
    }

    #[test]
    fn resolve_config_onto_empty_tiers_resets_to_default() {
        // The empty-tier path must still reset (it routes through the same
        // always-run resolution in handle_configure). A bind with no tiers after
        // a privileged bind must drop the privileged config.
        let mut config = Config::default();
        let _ = resolve_config_onto(
            &[tier("user", r#"{ "url_fetch_allow_private": true }"#)],
            &mut config,
        );
        assert!(config.url_fetch_allow_private);

        let _ = resolve_config_onto(&[], &mut config);
        assert!(
            !config.url_fetch_allow_private,
            "empty-tier bind must reset core config to default"
        );
    }

    #[test]
    fn resolve_config_onto_preserves_process_state_fields() {
        // Process-state fields (not part of RawAftConfig) are carried across the
        // reset so plugin-mode behavior is unchanged (the plugin re-sends them
        // via flat configure params right after this call).
        let mut config = Config {
            storage_dir: Some(std::path::PathBuf::from("/tmp/aft-store")),
            lsp_paths_extra: vec![std::path::PathBuf::from("/tmp/lsp-bin")],
            bash_permissions: true,
            project_root: Some(std::path::PathBuf::from("/tmp/proj")),
            ..Default::default()
        };

        let _ = resolve_config_onto(&[tier("user", r#"{ "search_index": true }"#)], &mut config);

        assert_eq!(
            config.storage_dir,
            Some(std::path::PathBuf::from("/tmp/aft-store"))
        );
        assert_eq!(
            config.lsp_paths_extra,
            vec![std::path::PathBuf::from("/tmp/lsp-bin")]
        );
        assert!(config.bash_permissions);
        assert_eq!(
            config.project_root,
            Some(std::path::PathBuf::from("/tmp/proj"))
        );
        assert!(config.search_index);
    }

    #[test]
    fn config_resolve_jsonc_comments_and_trailing_commas_parse() {
        let result = resolve_config(&[tier(
            "user",
            r#"{
              // line comment
              "search_index": true,
              "formatter": {
                "rust": "rustfmt", /* block comment */
              },
            }"#,
        )]);

        assert!(result.config.search_index);
        assert_eq!(
            result.config.formatter.get("rust").map(String::as_str),
            Some("rustfmt")
        );
    }

    #[test]
    fn check_config_defaults_to_disabled() {
        let result = resolve_config(&[tier("user", r#"{ "tool_surface": "all" }"#)]);
        assert!(!result.config.check.enabled);
    }

    #[test]
    fn check_config_enabled_via_user_tier() {
        let result = resolve_config(&[tier(
            "user",
            r#"{ "check": { "enabled": true } }"#,
        )]);
        assert!(result.config.check.enabled);
    }

    #[test]
    fn check_config_project_tier_overrides_user() {
        let result = resolve_config(&[
            tier("user", r#"{ "check": { "enabled": true } }"#),
            tier("project", r#"{ "check": { "enabled": false } }"#),
        ]);
        assert!(!result.config.check.enabled);
    }
}
