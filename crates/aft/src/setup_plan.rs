//! The feature plan behind `aft setup --plan`, the setup wizard and doctor.
//!
//! The binary owns the feature catalog. Every consumer (the npm wizard, doctor,
//! a future `ck setup`) renders the plan this module derives instead of
//! re-deriving feature state itself, so the rows, their order and the
//! derivation of `effective`/`reason`/`unavailable_reason` stay identical
//! everywhere.
//!
//! Derivation follows the shared matrix in `spec/feature-config/SPEC.md`:
//! `configured`/`source` describe only the user's base file; `effective`,
//! `reason` and `unavailable_reason` describe the fully resolved state for the
//! invocation context (user base, active harness block, project tier). `reason`
//! says how the effective choice was derived and never carries a failure cause;
//! `unavailable_reason` alone names what blocks an enabled feature.
//!
//! Index state is runtime state. It comes from a [`FeatureObserver`]; a
//! standalone CLI has no running engine to ask, so [`NoRuntimeObservation`]
//! reports every enabled index as unavailable with `runtime_not_observed`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::Config;
use crate::config_resolve::{resolve_config_for_harness, ConfigTier};
use crate::feature_config;
use crate::harness::Harness;
use crate::jsonc::strip_jsonc;
use crate::jsonc_edit::JsoncDocument;

/// The only plan format this binary emits and accepts.
pub const PLAN_VERSION: u64 = 1;
/// Diagnostic for a plan or answers version other than [`PLAN_VERSION`].
pub const UNSUPPORTED_PLAN_VERSION: &str = "unsupported_setup_plan_version";
/// Cause reported for an enabled index when no running engine was observed.
pub const RUNTIME_NOT_OBSERVED: &str = "runtime_not_observed";
/// Warning code listing disabled names that match no known tool.
pub const UNKNOWN_DISABLED_TOOLS: &str = "unknown_disabled_tools";

/// Derivation strings used in `reason`.
pub const REASON_DEFAULT: &str = "default";
pub const REASON_CONFIGURED: &str = "configured";
pub const REASON_IMPLIED_BY_WRITE: &str = "implied by github.write";
/// A bash setting that is on but has nothing to act on, because AFT does not
/// run the bash tool (`bash` is in `disabled_tools`).
pub const REASON_REQUIRES_BASH: &str = "requires bash";

/// Feature kinds, serialized as the plan's `kind` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FeatureKind {
    Tool,
    Index,
    Capability,
    /// A boolean setting of an AFT tool (`bash.compress`, `bash.rewrite`,
    /// `bash.background`). Its prerequisite is the tool it configures, and it
    /// has no effect while that tool is disabled.
    Setting,
}

/// One catalog row. IDs and order are stable for plan version 1 and mirror
/// `spec/feature-config/catalog.json`.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub kind: FeatureKind,
    pub group: &'static str,
    pub order: u32,
    pub label: &'static str,
    pub description: &'static str,
    pub cost_note: Option<&'static str>,
    pub prerequisites: &'static [&'static str],
}

impl CatalogEntry {
    /// Config path the row binds to: tools bind to membership in `disabled_tools`.
    pub fn binding_path(&self) -> &'static str {
        match self.kind {
            FeatureKind::Tool => "disabled_tools",
            _ => self.id,
        }
    }

    /// Tool name bound by a tool row.
    pub fn tool_name(&self) -> Option<&'static str> {
        (self.kind == FeatureKind::Tool).then_some(self.id)
    }
}

const fn tool(
    id: &'static str,
    group: &'static str,
    order: u32,
    label: &'static str,
    description: &'static str,
    prerequisites: &'static [&'static str],
) -> CatalogEntry {
    CatalogEntry {
        id,
        kind: FeatureKind::Tool,
        group,
        order,
        label,
        description,
        cost_note: None,
        prerequisites,
    }
}

const SEARCH: &str = "Search/navigation";
const EDITING: &str = "Editing";
const SHELL: &str = "Shell";
const INDEXES: &str = "Indexes";
const GITHUB: &str = "GitHub";

const fn bash_setting(
    id: &'static str,
    order: u32,
    label: &'static str,
    description: &'static str,
) -> CatalogEntry {
    CatalogEntry {
        id,
        kind: FeatureKind::Setting,
        group: SHELL,
        order,
        label,
        description,
        cost_note: None,
        prerequisites: &["bash"],
    }
}

/// The complete v1 catalog in plan order.
///
/// The bash companions (`bash_status`, `bash_write`, `bash_watch`,
/// `bash_kill`) are not rows: they follow the `bash` row, and setup never
/// writes them to `disabled_tools`. Existing entries naming them are kept.
pub const CATALOG: [CatalogEntry; 27] = [
    tool(
        "aft_search",
        SEARCH,
        1,
        "aft_search",
        "Ranked code search combining the trigram (exact/regex) and semantic (meaning) lanes.",
        &["indexes.trigram", "indexes.semantic"],
    ),
    tool(
        "grep",
        SEARCH,
        2,
        "grep",
        "AFT takes over the host grep tool; indexed when the trigram index is ready, filesystem matching otherwise.",
        &["indexes.trigram"],
    ),
    tool(
        "glob",
        SEARCH,
        3,
        "glob",
        "AFT takes over the host glob tool; indexed when the trigram index is ready, filesystem matching otherwise.",
        &["indexes.trigram"],
    ),
    tool(
        "aft_outline",
        SEARCH,
        4,
        "aft_outline",
        "Structural outline of files, directories and documents.",
        &[],
    ),
    tool(
        "aft_zoom",
        SEARCH,
        5,
        "aft_zoom",
        "Read one symbol or section; optional call-graph annotations use the callgraph index.",
        &["indexes.callgraph"],
    ),
    tool(
        "aft_callgraph",
        SEARCH,
        6,
        "aft_callgraph",
        "Callers, call paths and impact analysis from the callgraph index.",
        &["indexes.callgraph"],
    ),
    tool(
        "aft_inspect",
        SEARCH,
        7,
        "aft_inspect",
        "Diagnostics, TODOs, duplicates and (with the callgraph index) dead-code hints.",
        &["indexes.callgraph"],
    ),
    tool(
        "aft_conflicts",
        SEARCH,
        8,
        "aft_conflicts",
        "Show every git merge conflict in one call.",
        &[],
    ),
    tool(
        "read",
        EDITING,
        9,
        "read",
        "AFT takes over the host read tool.",
        &[],
    ),
    tool(
        "write",
        EDITING,
        10,
        "write",
        "AFT takes over the host write tool; every write is backed up for undo.",
        &[],
    ),
    tool(
        "edit",
        EDITING,
        11,
        "edit",
        "AFT takes over the host edit tool (find/replace, symbol and batch edits).",
        &[],
    ),
    tool(
        "apply_patch",
        EDITING,
        12,
        "apply_patch",
        "AFT takes over the host apply_patch tool.",
        &[],
    ),
    tool(
        "aft_import",
        EDITING,
        13,
        "aft_import",
        "Language-aware import add/remove/organize.",
        &[],
    ),
    tool(
        "aft_move",
        EDITING,
        14,
        "aft_move",
        "Move or rename files. Off by default: each move is backed up for undo, needs write permission on both locations, and does not rewrite imports.",
        &[],
    ),
    tool(
        "aft_delete",
        EDITING,
        15,
        "aft_delete",
        "Delete files or directories. Off by default: every deleted file is backed up for undo, and deleting through a symlink is refused.",
        &[],
    ),
    tool(
        "aft_safety",
        EDITING,
        16,
        "aft_safety",
        "Undo, edit history and named checkpoints over AFT's backups.",
        &[],
    ),
    tool(
        "ast_grep_search",
        EDITING,
        17,
        "ast_grep_search",
        "AST-aware structural code search.",
        &[],
    ),
    tool(
        "ast_grep_replace",
        EDITING,
        18,
        "ast_grep_replace",
        "AST-aware structural rewrite across files.",
        &[],
    ),
    tool(
        "bash",
        SHELL,
        19,
        "bash",
        "AFT takes over the host bash tool.",
        &[],
    ),
    bash_setting(
        "bash.compress",
        20,
        "Output compression",
        "Shortens command output before the agent reads it: test, build and install logs keep their errors and summaries and drop progress noise, so each command uses fewer tokens.",
    ),
    bash_setting(
        "bash.rewrite",
        21,
        "Command rewrites",
        "Simple reads and searches such as cat, grep, find and ls run through AFT's own read, grep and glob tools instead of the shell.",
    ),
    bash_setting(
        "bash.background",
        22,
        "Background commands",
        "Long commands move to the background instead of blocking the agent, and the agent gets a notice when they finish.",
    ),
    CatalogEntry {
        id: "indexes.trigram",
        kind: FeatureKind::Index,
        group: INDEXES,
        order: 23,
        label: "Trigram index",
        description: "Background index for fast exact and regex search (grep, glob and the lexical aft_search lane).",
        cost_note: Some("Uses disk space and CPU while the index builds."),
        prerequisites: &[],
    },
    CatalogEntry {
        id: "indexes.semantic",
        kind: FeatureKind::Index,
        group: INDEXES,
        order: 24,
        label: "Semantic index",
        description: "Background embedding index for meaning-based search (the semantic aft_search lane).",
        cost_note: Some("The local backend may download an ONNX runtime and an embedding model, and uses CPU while indexing."),
        prerequisites: &[],
    },
    CatalogEntry {
        id: "indexes.callgraph",
        kind: FeatureKind::Index,
        group: INDEXES,
        order: 25,
        label: "Callgraph index",
        description: "Persisted call graph used by aft_callgraph, zoom annotations, dead-code hints and search enrichment.",
        cost_note: Some("Uses disk space and CPU while the index builds."),
        prerequisites: &[],
    },
    CatalogEntry {
        id: "github.read",
        kind: FeatureKind::Capability,
        group: GITHUB,
        order: 26,
        label: "Read",
        description: "The agent can open issues and pull requests with read, aft_outline and aft_zoom using issue://N and pr://N addresses. Uses the GitHub CLI (gh), signed in to your account.",
        cost_note: None,
        prerequisites: &[],
    },
    CatalogEntry {
        id: "github.write",
        kind: FeatureKind::Capability,
        group: GITHUB,
        order: 27,
        label: "Write",
        description: "The agent can post a comment with write(\"issue://N\") or write(\"pr://N\") and edit its comments with edit(\"issue://N/comments/K\"). You are asked before each post. Comments go through AFT's gh shim: as the repository's bot where a signed GitHub routing setup is installed, otherwise as your own gh account. Needs Read.",
        cost_note: None,
        prerequisites: &["github.read"],
    },
];

/// Catalog row for `id`.
pub fn catalog_entry(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.id == id)
}

/// Literal accepted `--harness` values (`spec/feature-config/harnesses.json`).
pub const SETUP_HARNESS_SELECTORS: [&str; 3] = ["opencode", "pi", "omp"];

/// Environment variable through which an adapter that launches the binary
/// supplies its harness. An explicit `--harness` flag wins over it.
pub const SETUP_HARNESS_ENV: &str = "AFT_SETUP_HARNESS";

/// A setup harness selector. OpenCode V1/V2 are registration shapes of one
/// selector, and OMP runs the Pi extension, so it resolves Pi's harness block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupHarness {
    Opencode,
    Pi,
    Omp,
}

impl SetupHarness {
    /// Parse a selector; anything outside the frozen list is refused.
    pub fn from_selector(value: &str) -> Result<Self, String> {
        match value {
            "opencode" => Ok(Self::Opencode),
            "pi" => Ok(Self::Pi),
            "omp" => Ok(Self::Omp),
            other => Err(format!(
                "unsupported_harness:{other}: expected one of {}",
                SETUP_HARNESS_SELECTORS.join(", ")
            )),
        }
    }

    pub fn selector(self) -> &'static str {
        match self {
            Self::Opencode => "opencode",
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }

    /// Engine harness whose `harnesses.<label>` block applies.
    pub fn runtime_harness(self) -> Harness {
        match self {
            Self::Opencode => Harness::Opencode,
            Self::Pi | Self::Omp => Harness::Pi,
        }
    }
}

/// Background index planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPlane {
    Trigram,
    Semantic,
    Callgraph,
}

impl IndexPlane {
    fn from_feature_id(id: &str) -> Option<Self> {
        match id {
            "indexes.trigram" => Some(Self::Trigram),
            "indexes.semantic" => Some(Self::Semantic),
            "indexes.callgraph" => Some(Self::Callgraph),
            _ => None,
        }
    }
}

/// Feature status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effective {
    Off,
    Building,
    Ready,
    Unavailable,
}

/// What a runtime observer saw for one enabled index. Mirrors the engine's
/// index observation so a running engine can back the plan directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexObservation {
    pub effective: Effective,
    pub unavailable_reason: Option<String>,
}

/// Source of runtime state for the plan. Observation never starts a build.
pub trait FeatureObserver {
    /// State of an index that resolved configuration turned on.
    fn index(&self, plane: IndexPlane) -> IndexObservation;

    /// Why the local semantic backend cannot run here, when it cannot.
    fn semantic_unsupported(&self) -> Option<String> {
        None
    }

    /// Runtime cause blocking an enabled capability (`github.read`/`github.write`).
    fn capability_blocked(&self, _id: &str) -> Option<String> {
        None
    }
}

/// Observer for a CLI with no running engine: every enabled index is
/// unavailable with `runtime_not_observed`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRuntimeObservation;

impl FeatureObserver for NoRuntimeObservation {
    fn index(&self, _plane: IndexPlane) -> IndexObservation {
        IndexObservation {
            effective: Effective::Unavailable,
            unavailable_reason: Some(RUNTIME_NOT_OBSERVED.to_string()),
        }
    }
}

/// `binding` object of a plan row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Binding {
    pub path: &'static str,
    pub tool_name: Option<&'static str>,
}

/// One plan row, with exactly the v1 fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanFeature {
    pub id: &'static str,
    pub kind: FeatureKind,
    pub group: &'static str,
    pub order: u32,
    pub label: &'static str,
    pub description: &'static str,
    pub binding: Binding,
    pub default: bool,
    pub configured: bool,
    pub source: &'static str,
    pub proposed: bool,
    pub effective: Effective,
    pub reason: Option<&'static str>,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub cost_note: Option<&'static str>,
    pub prerequisites: Vec<&'static str>,
}

/// The `aft setup --plan` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetupPlan {
    pub plan_version: u64,
    pub features: Vec<PlanFeature>,
}

impl SetupPlan {
    pub fn feature(&self, id: &str) -> Option<&PlanFeature> {
        self.features.iter().find(|feature| feature.id == id)
    }
}

/// One config file supplied to derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub text: String,
}

/// The config files ordinary loading would consume for one invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigInputs {
    pub user: Option<ConfigFile>,
    pub project: Option<ConfigFile>,
}

/// Project config consumed for a project rooted at `root`.
pub fn project_config_path(root: &Path) -> PathBuf {
    root.join(".cortexkit").join("aft.jsonc")
}

fn read_optional(path: &Path) -> Result<Option<ConfigFile>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(ConfigFile {
            path: path.to_path_buf(),
            text,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

/// Read the files ordinary loading consumes: the user file and, when `cwd`
/// holds one, the project file. The loader treats the invocation directory as
/// the project root (no ancestor search), so the project tier exists exactly
/// when `<cwd>/.cortexkit/aft.jsonc` does.
pub fn read_inputs(user_config_path: Option<&Path>, cwd: &Path) -> Result<ConfigInputs, String> {
    let user = match user_config_path {
        Some(path) => read_optional(path)?,
        None => None,
    };
    let project = read_optional(&project_config_path(cwd))?;
    Ok(ConfigInputs { user, project })
}

/// Parse a raw config document into its JSON object.
pub fn parse_config_object(file: &ConfigFile) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(&strip_jsonc(&file.text)) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(format!(
            "invalid_config:{}: the config must be a JSON object",
            file.path.display()
        )),
        Err(error) => Err(format!("invalid_config:{}: {error}", file.path.display())),
    }
}

/// Translated base block and active harness block of one tier.
#[derive(Debug, Default)]
struct TierBlocks {
    base: Map<String, Value>,
    harness: Map<String, Value>,
}

fn tier_blocks(
    raw: Option<Map<String, Value>>,
    harness_key: Option<&str>,
    tier: feature_config::DocumentTier,
) -> TierBlocks {
    let Some(mut map) = raw else {
        return TierBlocks::default();
    };
    feature_config::translate_document(&mut map, tier);
    let harness = harness_key
        .and_then(|key| map.get("harnesses")?.get(key)?.as_object().cloned())
        .unwrap_or_default();
    TierBlocks { base: map, harness }
}

fn string_list(block: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    match block.get(key)? {
        Value::Array(entries) => Some(
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

fn nested_bool(block: &Map<String, Value>, container: &str, leaf: &str) -> Option<bool> {
    block.get(container)?.as_object()?.get(leaf)?.as_bool()
}

/// The explicit value one block gives a bash setting (`compress`, `rewrite`,
/// `background`), following the resolver's precedence: a boolean `bash` sets
/// every setting to that value, an object sets the leaves it names, and only
/// when `bash` is absent does a legacy `experimental.bash` block with any
/// feature flag count, where an unnamed flag means off.
fn bash_setting_value(block: &Map<String, Value>, leaf: &str) -> Option<bool> {
    match block.get("bash") {
        Some(Value::Bool(value)) => return Some(*value),
        Some(Value::Object(bash)) => return bash.get(leaf).and_then(Value::as_bool),
        _ => {}
    }
    let legacy = block
        .get("experimental")?
        .as_object()?
        .get("bash")?
        .as_object()?;
    let has_flag = ["rewrite", "compress", "background"]
        .iter()
        .any(|flag| legacy.contains_key(*flag));
    has_flag.then(|| legacy.get(leaf).and_then(Value::as_bool) == Some(true))
}

/// Result of deriving a plan.
#[derive(Debug, Clone)]
pub struct PlanOutcome {
    pub plan: SetupPlan,
    /// Distinct unknown names in the resolved disabled list, sorted.
    pub unknown_disabled_tools: Vec<String>,
}

/// Why a runtime gate blocks a registered tool, if it does.
fn tool_runtime_block(tool: &str, config: &Config) -> Option<&'static str> {
    match tool {
        "aft_safety" if config.backup.enabled == Some(false) => Some("backup_disabled"),
        "aft_inspect" if !config.inspect.enabled => Some("inspect_disabled"),
        "bash" if !config.bash.enabled => Some("bash_disabled"),
        _ => None,
    }
}

/// Derive the plan for one invocation context.
///
/// Fails (returning every diagnostic, sorted) when ordinary loading would
/// reject the configuration, e.g. a file that is not a JSON object; callers
/// must then produce no plan output.
pub fn derive_plan(
    inputs: &ConfigInputs,
    harness: Option<SetupHarness>,
    observer: &dyn FeatureObserver,
) -> Result<PlanOutcome, Vec<String>> {
    // AFT rewrites the user file's retired keys when it loads the file
    // (`config_fix::auto_migrate_user_config`), so the plan reads the text
    // that rewrite leaves. Otherwise a name AFT refuses, but rewrites before
    // it ever reads the file, would make the plan fail. A file the rewrite
    // cannot parse keeps its own text and reports its own error below.
    let user_file = inputs.user.as_ref().map(|file| ConfigFile {
        path: file.path.clone(),
        text: crate::config_fix::migrate_user_config_text(&file.text)
            .map(|migration| migration.text)
            .unwrap_or_else(|_| file.text.clone()),
    });
    let user_raw = user_file.as_ref().map(parse_config_object).transpose();
    let project_raw = inputs.project.as_ref().map(parse_config_object).transpose();
    let (user_raw, project_raw) = match (user_raw, project_raw) {
        (Ok(user), Ok(project)) => (user, project),
        (user, project) => {
            let mut errors: Vec<String> =
                [user.err(), project.err()].into_iter().flatten().collect();
            errors.sort();
            return Err(errors);
        }
    };

    let mut tiers = Vec::new();
    if let Some(file) = &user_file {
        tiers.push(ConfigTier {
            tier: "user".to_string(),
            source: file.path.to_string_lossy().into_owned(),
            doc: file.text.clone(),
        });
    }
    if let Some(file) = &inputs.project {
        tiers.push(ConfigTier {
            tier: "project".to_string(),
            source: file.path.to_string_lossy().into_owned(),
            doc: file.text.clone(),
        });
    }
    let runtime_harness = harness.map(SetupHarness::runtime_harness);
    let resolved = resolve_config_for_harness(&tiers, runtime_harness.as_ref());
    if !resolved.errors.is_empty() {
        return Err(resolved.errors);
    }
    let config = resolved.config;

    let harness_key = runtime_harness.as_ref().map(Harness::wire_label);
    let user = tier_blocks(
        user_raw,
        harness_key.as_deref(),
        feature_config::DocumentTier::User,
    );
    let project = tier_blocks(
        project_raw,
        harness_key.as_deref(),
        feature_config::DocumentTier::Project,
    );

    let base_list = string_list(&user.base, "disabled_tools");
    // Disables accepted after the user base: the user's harness block, and
    // project lists minus the protected slots the resolver ignores.
    let mut later_disables: BTreeSet<String> = string_list(&user.harness, "disabled_tools")
        .unwrap_or_default()
        .into_iter()
        .collect();
    for block in [&project.base, &project.harness] {
        later_disables.extend(
            string_list(block, "disabled_tools")
                .unwrap_or_default()
                .into_iter()
                .filter(|name| !feature_config::is_project_protected_tool(name)),
        );
    }
    let resolved_disabled: BTreeSet<&str> =
        config.disabled_tools.iter().map(String::as_str).collect();

    let semantic_unsupported = observer.semantic_unsupported();
    let mut features = Vec::with_capacity(CATALOG.len());
    for entry in &CATALOG {
        let feature = match entry.kind {
            FeatureKind::Tool => {
                let default = !feature_config::DEFAULT_DISABLED_TOOLS.contains(&entry.id);
                let configured = base_list
                    .as_ref()
                    .map_or(default, |list| !list.iter().any(|name| name == entry.id));
                let disabled = resolved_disabled.contains(entry.id);
                let reason = if base_list.is_some() || later_disables.contains(entry.id) {
                    REASON_CONFIGURED
                } else {
                    REASON_DEFAULT
                };
                let block = (!disabled)
                    .then(|| tool_runtime_block(entry.id, &config))
                    .flatten();
                row(
                    entry,
                    default,
                    configured,
                    base_list.is_some(),
                    configured,
                    if disabled {
                        Effective::Off
                    } else {
                        Effective::Ready
                    },
                    reason,
                    block.map(str::to_string),
                )
            }
            FeatureKind::Index => {
                let leaf = entry.id.trim_start_matches("indexes.");
                let base_value = nested_bool(&user.base, "indexes", leaf);
                let user_value = nested_bool(&user.harness, "indexes", leaf).or(base_value);
                let project_explicit = nested_bool(&project.base, "indexes", leaf).is_some()
                    || nested_bool(&project.harness, "indexes", leaf).is_some();
                let enabled = match leaf {
                    "trigram" => config.indexes.trigram,
                    "semantic" => config.indexes.semantic,
                    _ => config.indexes.callgraph,
                };
                // Off can only come from an accepted explicit false; on is
                // configured when any explicit value took part.
                let reason = if !enabled || user_value.is_some() || project_explicit {
                    REASON_CONFIGURED
                } else {
                    REASON_DEFAULT
                };
                let configured = base_value.unwrap_or(true);
                let unsupported = if leaf == "semantic" {
                    semantic_unsupported.clone()
                } else {
                    None
                };
                let proposed = if unsupported.is_some() && base_value.is_none() {
                    false
                } else {
                    configured
                };
                let (effective, cause) = if !enabled {
                    (Effective::Off, None)
                } else if let Some(cause) = unsupported {
                    (Effective::Unavailable, Some(cause))
                } else {
                    let plane =
                        IndexPlane::from_feature_id(entry.id).unwrap_or(IndexPlane::Trigram);
                    let observed = observer.index(plane);
                    match observed.effective {
                        Effective::Ready | Effective::Building => (observed.effective, None),
                        Effective::Unavailable | Effective::Off => (
                            Effective::Unavailable,
                            Some(
                                observed
                                    .unavailable_reason
                                    .unwrap_or_else(|| RUNTIME_NOT_OBSERVED.to_string()),
                            ),
                        ),
                    }
                };
                let mut feature = row(
                    entry,
                    true,
                    configured,
                    base_value.is_some(),
                    proposed,
                    effective,
                    reason,
                    None,
                );
                if effective != Effective::Off {
                    feature.available = cause.is_none();
                    feature.unavailable_reason = cause;
                }
                feature
            }
            FeatureKind::Capability => {
                let leaf = entry.id.trim_start_matches("github.");
                let base_value = nested_bool(&user.base, "github", leaf);
                let explicit = nested_bool(&user.harness, "github", leaf).or(base_value);
                let base_write = nested_bool(&user.base, "github", "write").unwrap_or(false);
                let (enabled, reason) = if leaf == "read" {
                    let independent = explicit.unwrap_or(false);
                    let reason = if !independent && config.github.write {
                        REASON_IMPLIED_BY_WRITE
                    } else if explicit.is_some() {
                        REASON_CONFIGURED
                    } else {
                        REASON_DEFAULT
                    };
                    (config.github.read || config.github.write, reason)
                } else {
                    let reason = if explicit.is_some() {
                        REASON_CONFIGURED
                    } else {
                        REASON_DEFAULT
                    };
                    (config.github.write, reason)
                };
                let configured = base_value.unwrap_or(false);
                // The read checkbox shows checked (and locked) under write.
                let proposed = if leaf == "read" {
                    configured || base_write
                } else {
                    configured
                };
                let cause = if enabled {
                    observer.capability_blocked(entry.id)
                } else {
                    None
                };
                let effective = match (enabled, &cause) {
                    (false, _) => Effective::Off,
                    (true, Some(_)) => Effective::Unavailable,
                    (true, None) => Effective::Ready,
                };
                let mut feature = row(
                    entry,
                    false,
                    configured,
                    base_value.is_some(),
                    proposed,
                    effective,
                    reason,
                    None,
                );
                feature.available = cause.is_none();
                feature.unavailable_reason = cause;
                feature
            }
            FeatureKind::Setting => {
                let leaf = entry.id.trim_start_matches("bash.");
                let base_value = bash_setting_value(&user.base, leaf);
                let explicit = base_value.is_some()
                    || [&user.harness, &project.base, &project.harness]
                        .iter()
                        .any(|block| bash_setting_value(block, leaf).is_some());
                let enabled = match leaf {
                    "compress" => config.experimental_bash_compress,
                    "rewrite" => config.experimental_bash_rewrite,
                    _ => config.experimental_bash_background,
                };
                let configured = base_value.unwrap_or(true);
                // A setting of the bash tool does nothing while AFT does not
                // run that tool; the runtime gate (`bash.enabled`) is a
                // separate cause, reported the same way as on the tool row.
                let bash_registered = !resolved_disabled.contains("bash");
                let (effective, reason, block) = if !enabled {
                    (
                        Effective::Off,
                        if explicit {
                            REASON_CONFIGURED
                        } else {
                            REASON_DEFAULT
                        },
                        None,
                    )
                } else if !bash_registered {
                    (Effective::Off, REASON_REQUIRES_BASH, None)
                } else {
                    (
                        Effective::Ready,
                        if explicit {
                            REASON_CONFIGURED
                        } else {
                            REASON_DEFAULT
                        },
                        (!config.bash.enabled).then(|| "bash_disabled".to_string()),
                    )
                };
                row(
                    entry,
                    true,
                    configured,
                    base_value.is_some(),
                    configured,
                    effective,
                    reason,
                    block,
                )
            }
        };
        features.push(feature);
    }

    Ok(PlanOutcome {
        plan: SetupPlan {
            plan_version: PLAN_VERSION,
            features,
        },
        unknown_disabled_tools: feature_config::unknown_disabled_tools(&config.disabled_tools),
    })
}

#[allow(clippy::too_many_arguments)]
fn row(
    entry: &CatalogEntry,
    default: bool,
    configured: bool,
    from_config: bool,
    proposed: bool,
    effective: Effective,
    reason: &'static str,
    runtime_block: Option<String>,
) -> PlanFeature {
    PlanFeature {
        id: entry.id,
        kind: entry.kind,
        group: entry.group,
        order: entry.order,
        label: entry.label,
        description: entry.description,
        binding: Binding {
            path: entry.binding_path(),
            tool_name: entry.tool_name(),
        },
        default,
        configured,
        source: if from_config { "config" } else { "default" },
        proposed,
        effective,
        reason: Some(reason),
        available: runtime_block.is_none(),
        unavailable_reason: runtime_block,
        cost_note: entry.cost_note,
        prerequisites: entry.prerequisites.to_vec(),
    }
}

/// How `aft setup` chooses what to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupSelections {
    /// `--yes`: keep explicit choices, fill missing settings with proposed values.
    Yes,
    /// Answers: set exactly the listed features; everything else is preserved.
    Answers(BTreeMap<String, bool>),
}

/// Parse an answers document `{"plan_version":1,"selections":{"<id>":bool}}`.
pub fn parse_answers(text: &str) -> Result<BTreeMap<String, bool>, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("invalid_setup_answers: {error}"))?;
    let Value::Object(object) = value else {
        return Err("invalid_setup_answers: expected a JSON object".to_string());
    };
    match object.get("plan_version").and_then(Value::as_u64) {
        Some(PLAN_VERSION) => {}
        _ => return Err(UNSUPPORTED_PLAN_VERSION.to_string()),
    }
    let Some(Value::Object(selections)) = object.get("selections") else {
        return Err("invalid_setup_answers: selections must be an object".to_string());
    };
    let mut errors = Vec::new();
    let mut parsed = BTreeMap::new();
    for (id, choice) in selections {
        if catalog_entry(id).is_none() {
            errors.push(format!("unknown_feature_id:{id}"));
        } else if let Some(choice) = choice.as_bool() {
            parsed.insert(id.clone(), choice);
        } else {
            errors.push(format!("invalid_selection_value:{id}"));
        }
    }
    if errors.is_empty() {
        // Write needs read: an answer that turns write on and read off asks
        // for a state the resolver would silently override, so refuse it.
        if parsed.get("github.write") == Some(&true) && parsed.get("github.read") == Some(&false) {
            return Err(
                "conflicting_selections:github.write:github.read: github.write needs github.read; select github.read too, or leave github.write off"
                    .to_string(),
            );
        }
        Ok(parsed)
    } else {
        Err(errors.join("\n"))
    }
}

/// The user file `aft setup` would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupWrite {
    pub text: String,
    /// Existing disabled entries naming no known tool, preserved verbatim.
    pub unknown_disabled_tools: Vec<String>,
}

/// Compute the new user-file text for `selections`.
///
/// Only base-level choices are written; harness blocks and every other key,
/// comment and formatting are left as they are. `disabled_tools` is always
/// written (an explicit `[]` included, because omission would restore the
/// default disables); unknown existing entries are kept verbatim and
/// historical aliases are written under their canonical names. Existing
/// entries for known tools that have no row (the bash companions) are kept
/// as they are, and setup never adds one.
///
/// Every chosen feature value is written explicitly, even when it equals the
/// default or follows from another choice, so the file shows each setting
/// and the user can edit either GitHub key directly.
pub fn render_setup(
    user_text: Option<&str>,
    plan: &SetupPlan,
    selections: &SetupSelections,
) -> Result<SetupWrite, String> {
    let mut doc = JsoncDocument::parse(user_text.unwrap_or(""))?;
    let raw = doc.value()?;
    let choice = |id: &str| match selections {
        SetupSelections::Answers(map) => map.get(id).copied(),
        SetupSelections::Yes => None,
    };
    let fill_missing = matches!(selections, SetupSelections::Yes);

    // Existing entries: canonicalize aliases, then split off unknown names
    // and known names that no row controls.
    let mut unknown = Vec::new();
    let mut unmanaged = Vec::new();
    if let Some(Value::Array(entries)) = raw.get("disabled_tools") {
        for name in entries.iter().filter_map(Value::as_str) {
            let canonical = feature_config::legacy_tool_alias(name).unwrap_or(name);
            if !feature_config::is_known_tool(canonical) {
                if !unknown.iter().any(|n| n == canonical) {
                    unknown.push(canonical.to_string());
                }
            } else if catalog_entry(canonical).is_none()
                && !unmanaged.iter().any(|n| n == canonical)
            {
                unmanaged.push(canonical.to_string());
            }
        }
    }
    let mut disabled: Vec<String> = plan
        .features
        .iter()
        .filter(|feature| feature.kind == FeatureKind::Tool)
        .filter(|feature| !choice(feature.id).unwrap_or(feature.configured))
        .map(|feature| feature.id.to_string())
        .chain(unmanaged)
        .collect();
    disabled.sort();
    disabled.extend(unknown.iter().cloned());
    doc.set(
        &["disabled_tools"],
        &Value::Array(disabled.into_iter().map(Value::String).collect()),
    )?;

    for feature in plan
        .features
        .iter()
        .filter(|feature| feature.kind == FeatureKind::Index)
    {
        let path: Vec<&str> = feature.id.split('.').collect();
        if let Some(value) = choice(feature.id) {
            doc.set(&path, &Value::Bool(value))?;
        } else if fill_missing && feature.source == "default" {
            doc.set(&path, &Value::Bool(feature.proposed))?;
        }
    }

    let mut bash_expanded = false;
    for feature in plan
        .features
        .iter()
        .filter(|feature| feature.kind == FeatureKind::Setting)
    {
        let value = match choice(feature.id) {
            Some(value) => value,
            None if fill_missing && feature.source == "default" => feature.proposed,
            None => continue,
        };
        // A boolean `bash` cannot hold a leaf. Spell it out as the object it
        // resolves to (the runtime gate plus every setting at that value)
        // before changing one setting, so the others keep their meaning.
        if let (false, Some(Value::Bool(all))) = (bash_expanded, raw.get("bash")) {
            let mut object = Map::new();
            object.insert("enabled".to_string(), Value::Bool(*all));
            for leaf in ["compress", "rewrite", "background"] {
                object.insert(leaf.to_string(), Value::Bool(*all));
            }
            doc.set(&["bash"], &Value::Object(object))?;
        }
        bash_expanded = true;
        let path: Vec<&str> = feature.id.split('.').collect();
        doc.set(&path, &Value::Bool(value))?;
    }

    // GitHub: write needs read. An answer that turns write on also records
    // read on, and an answer that turns read off also records write off.
    let read_choice = choice("github.read");
    let write_choice = choice("github.write");
    let read_value = match (read_choice, write_choice) {
        (Some(read), _) => Some(read),
        (None, Some(true)) => Some(true),
        (None, _) => plan
            .feature("github.read")
            .filter(|read| fill_missing && read.source == "default")
            .map(|read| read.proposed),
    };
    let write_value = match (write_choice, read_choice) {
        (Some(write), _) => Some(write),
        (None, Some(false)) => Some(false),
        (None, _) => plan
            .feature("github.write")
            .filter(|write| fill_missing && write.source == "default")
            .map(|write| write.configured),
    };
    if let Some(value) = read_value {
        doc.set(&["github", "read"], &Value::Bool(value))?;
    }
    if let Some(value) = write_value {
        doc.set(&["github", "write"], &Value::Bool(value))?;
    }

    Ok(SetupWrite {
        text: doc.text().to_string(),
        unknown_disabled_tools: unknown,
    })
}

/// Format the aggregated unknown-name warning, or `None` when there is none.
pub fn unknown_disabled_warning(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| {
        format!(
            "{UNKNOWN_DISABLED_TOOLS}: disabled_tools lists names that match no AFT tool and are kept as-is: {}",
            names.join(", ")
        )
    })
}

#[cfg(test)]
#[path = "setup_plan_tests.rs"]
mod tests;
