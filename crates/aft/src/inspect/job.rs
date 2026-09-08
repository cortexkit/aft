use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cache_freshness::FileFreshness;
use crate::config::Config;
use crate::parser::{LangId, SharedSymbolCache};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectCategory {
    Diagnostics,
    Metrics,
    Todos,
    DeadCode,
    UnusedExports,
    Duplicates,
    Cycles,
    Complexity,
    CircularDeps,
    OutdatedDeps,
    Vulnerabilities,
    TestCoverageGaps,
    ApiSurface,
}

impl InspectCategory {
    pub const ACTIVE: [InspectCategory; 8] = [
        InspectCategory::Diagnostics,
        InspectCategory::Metrics,
        InspectCategory::Todos,
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
        InspectCategory::Cycles,
        InspectCategory::Complexity,
    ];

    pub const DISABLED: [InspectCategory; 5] = [
        InspectCategory::CircularDeps,
        InspectCategory::OutdatedDeps,
        InspectCategory::Vulnerabilities,
        InspectCategory::TestCoverageGaps,
        InspectCategory::ApiSurface,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            InspectCategory::Diagnostics => "diagnostics",
            InspectCategory::Metrics => "metrics",
            InspectCategory::Todos => "todos",
            InspectCategory::DeadCode => "dead_code",
            InspectCategory::UnusedExports => "unused_exports",
            InspectCategory::Duplicates => "duplicates",
            InspectCategory::Cycles => "cycles",
            InspectCategory::Complexity => "complexity",
            InspectCategory::CircularDeps => "circular_deps",
            InspectCategory::OutdatedDeps => "outdated_deps",
            InspectCategory::Vulnerabilities => "vulnerabilities",
            InspectCategory::TestCoverageGaps => "test_coverage_gaps",
            InspectCategory::ApiSurface => "api_surface",
        }
    }

    pub fn tier(self) -> InspectTier {
        match self {
            InspectCategory::Diagnostics | InspectCategory::Metrics | InspectCategory::Todos => {
                InspectTier::Tier1
            }
            InspectCategory::DeadCode
            | InspectCategory::UnusedExports
            | InspectCategory::Duplicates
            | InspectCategory::Cycles
            | InspectCategory::Complexity
            | InspectCategory::CircularDeps
            | InspectCategory::ApiSurface => InspectTier::Tier2,
            InspectCategory::OutdatedDeps
            | InspectCategory::Vulnerabilities
            | InspectCategory::TestCoverageGaps => InspectTier::Tier3,
        }
    }

    pub fn is_tier2(self) -> bool {
        self.tier() == InspectTier::Tier2
    }

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }

    pub fn active() -> &'static [InspectCategory] {
        &Self::ACTIVE
    }

    pub fn disabled() -> &'static [InspectCategory] {
        &Self::DISABLED
    }
}

impl fmt::Display for InspectCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for InspectCategory {
    type Err = InspectCategoryParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "diagnostics" => Ok(Self::Diagnostics),
            "metrics" => Ok(Self::Metrics),
            "todos" => Ok(Self::Todos),
            "dead_code" => Ok(Self::DeadCode),
            "unused_exports" => Ok(Self::UnusedExports),
            "duplicates" => Ok(Self::Duplicates),
            "cycles" => Ok(Self::Cycles),
            "complexity" => Ok(Self::Complexity),
            "circular_deps" => Ok(Self::CircularDeps),
            "outdated_deps" => Ok(Self::OutdatedDeps),
            "vulnerabilities" => Ok(Self::Vulnerabilities),
            "test_coverage_gaps" => Ok(Self::TestCoverageGaps),
            "api_surface" => Ok(Self::ApiSurface),
            other => Err(InspectCategoryParseError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectCategoryParseError(String);

impl fmt::Display for InspectCategoryParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown inspect category '{}'", self.0)
    }
}

impl std::error::Error for InspectCategoryParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectTier {
    Tier1,
    Tier2,
    Tier3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobScope {
    project_root: PathBuf,
    roots: Vec<PathBuf>,
    scope_hash: String,
}

impl JobScope {
    pub fn for_project(project_root: impl Into<PathBuf>) -> Self {
        let project_root = project_root.into();
        Self {
            roots: Vec::new(),
            scope_hash: "project".to_string(),
            project_root,
        }
    }

    pub fn from_roots(project_root: impl Into<PathBuf>, roots: Vec<PathBuf>) -> Self {
        let project_root = project_root.into();
        let mut roots = roots
            .into_iter()
            .map(|root| normalize_path(&root))
            .collect::<Vec<_>>();
        roots.sort();
        roots.dedup();

        if roots.is_empty() || (roots.len() == 1 && normalize_path(&project_root) == roots[0]) {
            return Self::for_project(project_root);
        }

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for root in &roots {
            root.to_string_lossy().hash(&mut hasher);
            "\0".hash(&mut hasher);
        }

        Self {
            project_root,
            roots,
            scope_hash: format!("{:016x}", hasher.finish()),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn scope_hash(&self) -> &str {
        &self.scope_hash
    }

    pub fn is_project_wide(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn contains(&self, path: &Path) -> bool {
        if self.roots.is_empty() {
            return true;
        }
        let normalized = normalize_path(path);
        self.roots.iter().any(|root| normalized.starts_with(root))
    }

    pub fn contains_display_path(&self, value: &str) -> bool {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            self.contains(&path)
        } else {
            self.contains(&self.project_root.join(path))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobKey {
    pub category: InspectCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_hash: Option<String>,
}

impl JobKey {
    pub fn for_category_scope(category: InspectCategory, scope: &JobScope) -> Self {
        if category.is_tier2() {
            Self::for_project_category(category)
        } else {
            Self {
                category,
                scope_hash: Some(scope.scope_hash().to_string()),
            }
        }
    }

    pub fn for_project_category(category: InspectCategory) -> Self {
        Self {
            category,
            scope_hash: None,
        }
    }

    pub fn display_key(&self) -> String {
        match &self.scope_hash {
            Some(scope_hash) => format!("{}:{scope_hash}", self.category),
            None => self.category.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct InspectSnapshot {
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub config: Arc<Config>,
    pub symbol_cache: SharedSymbolCache,
    pub inspect_writer: bool,
    pub callgraph_writer: bool,
}

impl InspectSnapshot {
    pub fn new(
        project_root: PathBuf,
        inspect_dir: PathBuf,
        config: Arc<Config>,
        symbol_cache: SharedSymbolCache,
    ) -> Self {
        Self::new_with_capabilities(project_root, inspect_dir, config, symbol_cache, true, true)
    }

    pub fn new_with_capabilities(
        project_root: PathBuf,
        inspect_dir: PathBuf,
        config: Arc<Config>,
        symbol_cache: SharedSymbolCache,
        inspect_writer: bool,
        callgraph_writer: bool,
    ) -> Self {
        Self {
            project_root,
            inspect_dir,
            config,
            symbol_cache,
            inspect_writer,
            callgraph_writer,
        }
    }
}

#[derive(Clone)]
pub struct WorkerCtx {
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub config: Arc<Config>,
    pub symbol_cache: SharedSymbolCache,
}

impl From<&InspectSnapshot> for WorkerCtx {
    fn from(snapshot: &InspectSnapshot) -> Self {
        Self {
            project_root: snapshot.project_root.clone(),
            inspect_dir: snapshot.inspect_dir.clone(),
            config: Arc::clone(&snapshot.config),
            symbol_cache: Arc::clone(&snapshot.symbol_cache),
        }
    }
}

#[derive(Clone)]
pub struct InspectJob {
    pub job_id: u64,
    pub key: JobKey,
    pub category: InspectCategory,
    pub scope_files: Vec<PathBuf>,
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub config: Arc<Config>,
    pub symbol_cache: SharedSymbolCache,
    pub inspect_writer: bool,
    pub callgraph_writer: bool,
    pub callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
}

impl InspectJob {
    pub fn worker_ctx(&self) -> WorkerCtx {
        WorkerCtx {
            project_root: self.project_root.clone(),
            inspect_dir: self.inspect_dir.clone(),
            config: Arc::clone(&self.config),
            symbol_cache: Arc::clone(&self.symbol_cache),
        }
    }

    /// Tier-1 jobs retain their scope hash, and `project` identifies the
    /// complete project file set rather than a user-selected subset.
    pub fn is_full_project_scope(&self) -> bool {
        self.key.scope_hash.as_deref() == Some("project")
    }
}

pub(crate) fn is_js_ts_language(language: LangId) -> bool {
    matches!(
        language,
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript
    )
}

pub(crate) fn dead_code_supports_language(language: LangId) -> bool {
    is_js_ts_language(language) || callgraph_store_dead_code_supports_language(language)
}

pub(crate) fn dead_code_skipped_language(file: &Path) -> Option<&'static str> {
    let language = crate::parser::detect_language(file)?;
    (!dead_code_supports_language(language)).then(|| language_name(language))
}

fn callgraph_store_dead_code_supports_language(language: LangId) -> bool {
    // Dead-code reachability needs real call edges, not just outline symbols.
    // Keep this narrower than every language with a tree-sitter grammar: the
    // store-backed liveness path is trusted only for languages with call-edge
    // extraction that dead-code consumes, and the call_node_kinds check keeps the
    // gate tied to the extraction substrate instead of extension names alone.
    let supported_by_store_liveness = matches!(
        language,
        LangId::Rust
            | LangId::Go
            | LangId::C
            | LangId::Cpp
            | LangId::Cuda
            | LangId::Metal
            | LangId::Zig
            | LangId::CSharp
    );
    supported_by_store_liveness && !crate::calls::call_node_kinds(language).is_empty()
}

pub(crate) fn language_name(language: LangId) -> &'static str {
    match language {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Cuda => "cuda",
        LangId::Metal => "metal",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
        LangId::Toml => "toml",
    }
}

#[derive(Debug, Clone, Default)]
pub struct CallgraphSnapshot {
    pub generated_at: Option<SystemTime>,
    pub files: Vec<PathBuf>,
    pub exported_symbols: Vec<CallgraphExport>,
    pub outbound_calls: Vec<CallgraphOutboundCall>,
    pub entry_points: BTreeSet<PathBuf>,
    pub entry_point_symbols: BTreeMap<PathBuf, BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallgraphExport {
    pub file: PathBuf,
    pub symbol: String,
    pub kind: String,
    pub line: u32,
}

pub(crate) const DISPATCHED_CALLEE_SEPARATOR: char = '\u{1f}';
pub(crate) const CALLGRAPH_PROVENANCE_TREESITTER: &str = "treesitter";
pub(crate) const CALLGRAPH_PROVENANCE_REEXPORT: &str = "reexport";

fn default_callgraph_outbound_provenance() -> String {
    CALLGRAPH_PROVENANCE_TREESITTER.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallgraphOutboundCall {
    pub caller_file: PathBuf,
    pub caller_symbol: String,
    pub target: String,
    pub line: u32,
    #[serde(default = "default_callgraph_outbound_provenance")]
    pub provenance: String,
}

#[derive(Debug, Clone)]
pub struct FileContribution {
    pub category: InspectCategory,
    pub file_path: PathBuf,
    pub freshness: FileFreshness,
    pub contribution: serde_json::Value,
    pub type_ref_names: BTreeSet<String>,
}

impl FileContribution {
    pub fn new(
        category: InspectCategory,
        file_path: impl Into<PathBuf>,
        freshness: FileFreshness,
        contribution: serde_json::Value,
    ) -> Self {
        let type_ref_names = type_ref_names_from_contribution(&contribution);
        Self {
            category,
            file_path: file_path.into(),
            freshness,
            contribution,
            type_ref_names,
        }
    }

    pub fn with_type_ref_names<I>(mut self, type_ref_names: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        self.type_ref_names = type_ref_names.into_iter().collect();
        self.contribution =
            contribution_with_type_ref_names(self.contribution, &self.type_ref_names);
        self
    }
}

pub(crate) fn type_ref_names_from_contribution(
    contribution: &serde_json::Value,
) -> BTreeSet<String> {
    contribution
        .get("type_ref_names")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn contribution_with_type_ref_names(
    mut contribution: serde_json::Value,
    type_ref_names: &BTreeSet<String>,
) -> serde_json::Value {
    if let serde_json::Value::Object(object) = &mut contribution {
        if type_ref_names.is_empty() {
            object.remove("type_ref_names");
        } else {
            object.insert(
                "type_ref_names".to_string(),
                serde_json::Value::Array(
                    type_ref_names
                        .iter()
                        .map(|name| serde_json::Value::String(name.clone()))
                        .collect(),
                ),
            );
        }
    }
    contribution
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct InspectScanSuccess {
    pub scanned_files: Vec<PathBuf>,
    pub contributions: Vec<FileContribution>,
    pub aggregate: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct InspectResult {
    pub job_id: u64,
    pub key: JobKey,
    pub category: InspectCategory,
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub config: Arc<Config>,
    pub outcome: Result<InspectScanSuccess, String>,
    pub duration: Duration,
}

impl InspectResult {
    pub fn success(job: &InspectJob, success: InspectScanSuccess, duration: Duration) -> Self {
        Self {
            job_id: job.job_id,
            key: job.key.clone(),
            category: job.category,
            project_root: job.project_root.clone(),
            inspect_dir: job.inspect_dir.clone(),
            config: Arc::clone(&job.config),
            outcome: Ok(success),
            duration,
        }
    }

    pub fn failed(job: &InspectJob, message: impl Into<String>, duration: Duration) -> Self {
        Self {
            job_id: job.job_id,
            key: job.key.clone(),
            category: job.category,
            project_root: job.project_root.clone(),
            inspect_dir: job.inspect_dir.clone(),
            config: Arc::clone(&job.config),
            outcome: Err(message.into()),
            duration,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingWaitCause {
    WaiterDropped,
    ResultChannelDisconnected,
    DeadlineElapsed,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingWait {
    pub cause: PendingWaitCause,
    pub elapsed_ms: u64,
    pub budget_ms: u64,
}

impl PendingWait {
    pub fn new(cause: PendingWaitCause, elapsed: Duration, budget: Duration) -> Self {
        Self {
            cause,
            elapsed_ms: duration_millis(elapsed),
            budget_ms: duration_millis(budget),
        }
    }

    pub fn detail(&self) -> String {
        let elapsed = display_duration_millis(self.elapsed_ms);
        let budget = display_duration_millis(self.budget_ms);
        match self.cause {
            PendingWaitCause::WaiterDropped => {
                format!("waiter dropped without outcome after {elapsed}; budget {budget}")
            }
            PendingWaitCause::ResultChannelDisconnected => {
                format!("result channel disconnected after {elapsed}; budget {budget}")
            }
            PendingWaitCause::DeadlineElapsed => {
                format!("deadline elapsed after {elapsed}; budget {budget}")
            }
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn display_duration_millis(millis: u64) -> String {
    if millis >= 1_000 && millis % 1_000 == 0 {
        format!("{}s", millis / 1_000)
    } else if millis >= 1_000 {
        format!("{:.1}s", millis as f64 / 1_000.0)
    } else {
        format!("{millis}ms")
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobOutcome {
    Fresh {
        payload: serde_json::Value,
    },
    Stale {
        cached: Option<serde_json::Value>,
        in_flight: bool,
    },
    Pending {
        in_flight: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        wait: Option<PendingWait>,
    },
    Failed {
        message: String,
    },
}

impl JobOutcome {
    pub fn pending(in_flight: bool) -> Self {
        Self::Pending {
            in_flight,
            wait: None,
        }
    }

    pub fn pending_wait(
        in_flight: bool,
        cause: PendingWaitCause,
        elapsed: Duration,
        budget: Duration,
    ) -> Self {
        Self::Pending {
            in_flight,
            wait: Some(PendingWait::new(cause, elapsed, budget)),
        }
    }

    pub fn pending_detail(&self) -> Option<String> {
        match self {
            Self::Pending {
                wait: Some(wait), ..
            } => Some(wait.detail()),
            _ => None,
        }
    }

    pub fn payload(&self) -> Option<&serde_json::Value> {
        match self {
            JobOutcome::Fresh { payload } => Some(payload),
            JobOutcome::Stale { cached, .. } => cached.as_ref(),
            JobOutcome::Pending { .. } | JobOutcome::Failed { .. } => None,
        }
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, JobOutcome::Stale { .. })
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, JobOutcome::Pending { .. })
    }

    pub fn summary_status(&self) -> Option<&'static str> {
        match self {
            JobOutcome::Fresh { .. } => None,
            JobOutcome::Stale { .. } => Some("stale"),
            JobOutcome::Pending { .. } => Some("pending"),
            JobOutcome::Failed { .. } => Some("failed"),
        }
    }
}

/// Whether a project-relative path is a test-support / standalone file that
/// should be EXCLUDED from dead_code and unused_exports reporting. These files
/// (test fixtures, corpora, mock data, snapshots) are consumed by file path or
/// loaded dynamically — never imported as modules — so static reachability
/// always reports their symbols as dead/unused. They are noise, not signal.
///
/// Edges FROM these files are still honored elsewhere (their imports keep
/// product code live); only their own exported symbols are suppressed from the
/// dead/unused lists. The match is on conventional directory names anywhere in
/// the path, kept conservative to avoid hiding real product directories.
pub(crate) fn is_test_support_file(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");
    normalized.split('/').any(|segment| {
        matches!(
            segment,
            "fixtures"
                | "__fixtures__"
                | "testdata"
                | "test-data"
                | "__mocks__"
                | "__snapshots__"
                | "corpora"
        )
    })
}

/// Whether a project-relative path is an actual automated-test file (unit /
/// integration / spec), as opposed to product code. Search uses this to hide test
/// files by default; inspect uses it to distinguish test-origin references from
/// production usage without excluding the test files from analysis.
///
/// This is intentionally SEPARATE from `is_test_support_file`: test-support files
/// are reporting noise excluded by path, while real tests remain scanned and can
/// move otherwise-dead production symbols into the test-only bucket. Matching is
/// high-precision to avoid hiding real product code.
pub(crate) fn is_test_file(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");

    // Directory-segment conventions: `__tests__` (JS/TS) and a `tests` test root
    // (Rust integration tests, Python). Singular `test`/`spec` are omitted —
    // they collide with product modules and their files are caught by name below.
    if normalized
        .split('/')
        .any(|segment| matches!(segment, "__tests__" | "__test__" | "tests"))
    {
        return true;
    }

    let file = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower = file.to_ascii_lowercase();

    // `*.test.<ext>` / `*.spec.<ext>` — JS/TS/JSX/TSX/Vue/mjs/cjs/…
    if lower.contains(".test.") || lower.contains(".spec.") {
        return true;
    }

    // Per-language filename suffixes (lowercase conventions).
    if lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.rb")
        || lower.ends_with("_test.exs")
        || lower.ends_with("_spec.rb")
        || (lower.starts_with("test_") && lower.ends_with(".py"))
    {
        return true;
    }

    // CamelCase test classes — matched case-SENSITIVELY so product files like
    // `latest.java` (which ends with "test.java" lowercased) don't false-match.
    const CAMEL_SUFFIXES: &[&str] = &[
        "Test.java",
        "Tests.java",
        "Test.kt",
        "Tests.kt",
        "Test.cs",
        "Tests.cs",
        "Test.swift",
        "Tests.swift",
        "Test.scala",
        "Spec.scala",
    ];
    CAMEL_SUFFIXES.iter().any(|suffix| file.ends_with(suffix))
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    // Strip Windows verbatim prefixes before component normalization so paths
    // that went through fs::canonicalize (\\?\C:\ form) compare equal to paths
    // that never did. The oxc engine's resolver applies the same rule; the two
    // normalizers must stay in agreement or membership/strip_prefix checks
    // that cross the engine boundary silently miss on Windows.
    #[cfg(windows)]
    let path = &crate::windows_path::normalize_windows_path(path);

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !result.pop() {
                    result.push(component);
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Canonicalize a path for comparison inside the inspect subsystem.
///
/// Never returns the raw `fs::canonicalize` result: on Windows that is a
/// verbatim (`\\?\C:\`) path, and mixing verbatim and non-verbatim forms in
/// the same comparison is this subsystem's recurring bug class. Both branches
/// route through [`normalize_path`].
pub(crate) fn canonicalize_normalized(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(canonical) => normalize_path(&canonical),
        Err(_) => normalize_path(path),
    }
}

#[cfg(test)]
mod test_support_tests {
    use super::{is_test_file, is_test_support_file};

    #[test]
    fn is_test_file_matches_real_test_files() {
        // JS/TS family: *.test.* / *.spec.*
        for p in [
            "src/foo.test.ts",
            "src/foo.test.tsx",
            "src/bar.spec.js",
            "packages/x/component.test.jsx",
            "app/foo.test.mjs",
            "src/comp.spec.vue",
        ] {
            assert!(is_test_file(p), "{p} should be a test file");
        }
        // __tests__ directory and tests roots.
        assert!(is_test_file("packages/x/__tests__/reading.ts"));
        assert!(is_test_file("crates/aft/tests/integration/main.rs"));
        // Per-language filename suffixes.
        assert!(is_test_file("crates/aft/src/foo_test.rs"));
        assert!(is_test_file("pkg/handler_test.go"));
        assert!(is_test_file("app/test_models.py"));
        assert!(is_test_file("app/models_test.py"));
        assert!(is_test_file("spec/user_spec.rb"));
        // CamelCase test classes (case-sensitive).
        assert!(is_test_file("src/main/UserServiceTest.java"));
        assert!(is_test_file("src/FooTests.cs"));
        assert!(is_test_file("Sources/AppTests.swift"));
        // Windows separators normalize.
        assert!(is_test_file("packages\\x\\__tests__\\a.ts"));
    }

    #[test]
    fn is_test_file_rejects_product_files() {
        for p in [
            "crates/aft/src/inspect/job.rs",
            "packages/x/src/index.ts",
            "src/contestant.ts",     // "test" substring, not a test file
            "src/greatest.ts",       // ends with "test" stem, not ".test."
            "src/latest.java",       // case-sensitive guard: not "Test.java"
            "src/my_attestation.py", // not test_*/*_test
            "src/test/helper.ts",    // singular `test` dir must not blanket-match
        ] {
            assert!(!is_test_file(p), "{p} must NOT be a test file");
        }
    }

    #[test]
    fn matches_conventional_support_dirs() {
        assert!(is_test_support_file("crates/aft/tests/fixtures/sample.ts"));
        assert!(is_test_support_file(
            "packages/x/__tests__/e2e/fixtures/a.ts"
        ));
        assert!(is_test_support_file(
            "benchmarks/codegraph/corpora/repo/lib.go"
        ));
        assert!(is_test_support_file("src/__mocks__/fs.ts"));
        assert!(is_test_support_file("src/__snapshots__/render.snap"));
        assert!(is_test_support_file("internal/testdata/golden.json"));
        // Windows-style separators normalize.
        assert!(is_test_support_file("crates\\aft\\tests\\fixtures\\x.rs"));
    }

    #[test]
    fn does_not_match_product_or_test_files() {
        // A real product file under src must never be excluded.
        assert!(!is_test_support_file("crates/aft/src/inspect/job.rs"));
        // Test FILES are not support files (they hold real assertions/roots).
        assert!(!is_test_support_file(
            "packages/x/__tests__/reading.test.ts"
        ));
        assert!(!is_test_support_file(
            "crates/aft/tests/integration/main.rs"
        ));
        // Substring of a segment must not match (only whole segments).
        assert!(!is_test_support_file("src/fixturesHelper.ts"));
        assert!(!is_test_support_file("src/my_corpora_loader.rs"));
    }
}
