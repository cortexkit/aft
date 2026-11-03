//! Bounded project-intent evidence, independent of index configuration.
//!
//! Callers must authorize the root before probing it. This module never reads
//! worktree file contents or follows directory symlinks. Git control metadata
//! is checked directly. A time-limited `git rev-parse` also recognizes folders
//! inside a parent repository and unusual Git layouts. Neither path lists tracked files.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const POLICY_VERSION: u32 = 1;

pub(crate) const MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "go.work",
    "pyproject.toml",
    "pom.xml",
    "Makefile",
    "CMakeLists.txt",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "MODULE.bazel",
    "mix.exs",
    "dune-project",
    "Package.swift",
    "Gemfile",
    "composer.json",
    "meson.build",
    "deno.json",
    "deno.jsonc",
    "pubspec.yaml",
    "flake.nix",
];
pub(crate) const PERSONAL: &[&str] = &[
    "Desktop",
    "Downloads",
    "Documents",
    "Pictures",
    "Movies",
    "Music",
    "Library",
    "Applications",
    "Public",
];
const BULK: &[&str] = &[
    "node_modules",
    "vendor",
    "third_party",
    "third-party",
    "target",
    "dist",
    "build",
    "generated",
    "__pycache__",
    ".venv",
    "venv",
    ".cache",
    "coverage",
    "out",
];
const SOURCE: &[&str] = &[
    "c", "h", "cc", "cpp", "cxx", "hpp", "rs", "go", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts",
    "tsx", "java", "kt", "kts", "cs", "fs", "swift", "m", "mm", "rb", "ex", "exs", "erl", "scala",
    "sc", "sh", "bash", "zsh", "lua", "r", "R", "vue", "svelte", "php", "pl", "zig", "hs", "ml",
    "mli", "clj", "cljs", "dart", "jl", "pas", "cu", "cuh", "sql",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionMode {
    #[default]
    Auto,
    Project,
    Workspace,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectionOverride {
    pub path: PathBuf,
    pub mode: DetectionMode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectDetectionConfig {
    pub mode: DetectionMode,
    pub overrides: Vec<DetectionOverride>,
}

impl ProjectDetectionConfig {
    /// An exact canonical identity match only: an override never admits children.
    pub fn mode_for_root(&self, root: &Path) -> DetectionMode {
        let Ok(root) = crate::path_identity::ProjectRootId::from_path(root) else {
            return DetectionMode::Auto;
        };
        self.overrides
            .iter()
            .find(|entry| {
                entry.path.is_absolute()
                    && crate::path_identity::ProjectRootId::from_path(&entry.path)
                        .is_ok_and(|candidate| candidate == root)
            })
            .map_or(self.mode, |entry| entry.mode)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProbeBudgets {
    pub entries: usize,
    pub time: Duration,
    pub depth: usize,
    pub per_directory: usize,
}

impl Default for ProbeBudgets {
    fn default() -> Self {
        Self {
            entries: 4096,
            time: Duration::from_millis(100),
            depth: 6,
            per_directory: 256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderClassification {
    Project,
    Workspace,
    NotProject,
    /// No determination was possible; pending snapshots already use JSON null.
    #[serde(untagged)]
    Undetermined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReason {
    UserOverride,
    MixedPersonalTree,
    GitCheckout,
    RootMarker,
    MultipleChildProjects,
    SmallSourceTree,
    NoProjectEvidence,
    EvidenceBudgetExhausted,
    ProbeIoError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStop {
    Complete,
    Entries,
    Time,
    DirectoryLimit,
    DepthLimit,
    IoError,
    StrongEvidence,
    UserOverride,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Observed {
    pub entries: usize,
    pub files: usize,
    pub directories: usize,
    pub symlinks: usize,
    pub errors: usize,
    pub source_files: usize,
    pub bulk_files: usize,
    pub eligible_source_files: usize,
    pub non_bulk_root_directories: usize,
    pub personal_groups: usize,
    pub child_projects: usize,
    pub root_markers: Vec<String>,
    pub git_directories_skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeCoverage {
    /// Counts are exact only for a complete walk under the stated policy.
    /// Otherwise all observed counts are lower bounds, not population estimates.
    pub complete: bool,
    pub count_kind: CountKind,
    pub root_listing_complete: bool,
    /// Project roots within two directory levels below the selected root are
    /// known, and remaining paths not owned by them are reported as gaps.
    /// Sampling the projects' own source files is not required for this flag.
    pub workspace_complete: bool,
    pub stop: ProbeStop,
    pub entry_budget: usize,
    pub time_budget_ms: u64,
    pub depth_budget: usize,
    pub per_directory_budget: usize,
    pub directory_cuts: usize,
    pub depth_cuts: usize,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountKind {
    Exact,
    LowerBound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderAdmission {
    pub policy_version: u32,
    pub canonical_root: Option<PathBuf>,
    pub classification: FolderClassification,
    pub reason: AdmissionReason,
    pub observed: Observed,
    pub coverage: ProbeCoverage,
    pub override_mode: DetectionMode,
    /// None means the tracked-path count is unknown. A Git index header counts
    /// entries, which can include multiple stages or sparse-directory records,
    /// so it cannot establish the number of logical tracked file paths.
    pub tracked: Option<TrackedCount>,
    /// Actual shallow project roots, not their grouping directories.
    pub child_roots: Vec<PathBuf>,
    pub immediate_directories: Vec<PathBuf>,
    pub observed_directories: Vec<PathBuf>,
    pub immediate_entries: Vec<PathBuf>,
    pub evidence_markers: Vec<PathBuf>,
    pub git_controls: Vec<PathBuf>,
    pub outside_paths: Vec<PathBuf>,
}

impl FolderAdmission {
    // Reaching an entry/width/depth limit finishes a bounded policy check.
    // Time or I/O interruption before the entry quota is spent does not.
    fn bounded_examination_complete(&self) -> bool {
        let spent =
            self.coverage.entry_budget > 0 && self.observed.entries >= self.coverage.entry_budget;
        let interrupted = matches!(self.coverage.stop, ProbeStop::Time | ProbeStop::IoError)
            || self.reason == AdmissionReason::ProbeIoError;
        self.observed.entries > 0 && (spent || !interrupted)
    }

    pub(crate) fn is_determined(&self) -> bool {
        self.classification != FolderClassification::Undetermined
            && !(self.classification == FolderClassification::NotProject
                && matches!(
                    self.reason,
                    AdmissionReason::EvidenceBudgetExhausted | AdmissionReason::ProbeIoError
                )
                && !self.bounded_examination_complete())
    }

    pub(crate) fn is_structural_refusal(&self) -> bool {
        self.classification == FolderClassification::NotProject
            && self.reason == AdmissionReason::NoProjectEvidence
            && self.coverage.stop == ProbeStop::StrongEvidence
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedCount {
    pub count: usize,
    pub index_identity: String,
}

/// A worker that cannot finish an OS operation has observed no complete tree.
/// Keep that absence of coverage distinct from a completed negative probe.
pub(crate) fn exhausted(
    root: &Path,
    mode: DetectionMode,
    budgets: ProbeBudgets,
) -> FolderAdmission {
    FolderAdmission {
        policy_version: POLICY_VERSION,
        canonical_root: Some(root.to_path_buf()),
        classification: FolderClassification::Undetermined,
        reason: AdmissionReason::EvidenceBudgetExhausted,
        observed: Observed::default(),
        coverage: ProbeCoverage {
            complete: false,
            count_kind: CountKind::LowerBound,
            root_listing_complete: false,
            workspace_complete: false,
            stop: ProbeStop::Time,
            entry_budget: budgets.entries,
            time_budget_ms: millis(budgets.time),
            depth_budget: budgets.depth,
            per_directory_budget: budgets.per_directory,
            directory_cuts: 0,
            depth_cuts: 0,
            elapsed_ms: millis(budgets.time),
        },
        override_mode: mode,
        tracked: None,
        child_roots: Vec::new(),
        immediate_directories: Vec::new(),
        observed_directories: Vec::new(),
        immediate_entries: Vec::new(),
        evidence_markers: Vec::new(),
        git_controls: Vec::new(),
        outside_paths: Vec::new(),
    }
}

/// Project and Off are user decisions, not requests for an evidence probe.
pub(crate) fn override_decision(root: &Path, mode: DetectionMode) -> Option<FolderAdmission> {
    let classification = match mode {
        DetectionMode::Project => FolderClassification::Project,
        DetectionMode::Off => FolderClassification::NotProject,
        DetectionMode::Auto | DetectionMode::Workspace => return None,
    };
    let mut decision = exhausted(root, mode, ProbeBudgets::default());
    decision.classification = classification;
    decision.reason = AdmissionReason::UserOverride;
    decision.coverage.stop = ProbeStop::UserOverride;
    decision.coverage.elapsed_ms = 0;
    Some(decision)
}

/// Root identity is a safety boundary, independent of project evidence or policy overrides.
pub(crate) fn boundary_decision(root: &Path, mode: DetectionMode) -> Option<FolderAdmission> {
    let broad = crate::path_identity::project_root_is_volume_root(root);
    if broad == Some(false) {
        return None;
    }
    let mut decision = exhausted(root, mode, ProbeBudgets::default());
    decision.coverage.elapsed_ms = 0;
    if broad == Some(true) {
        decision.classification = FolderClassification::NotProject;
        decision.reason = AdmissionReason::NoProjectEvidence;
        decision.coverage.stop = ProbeStop::StrongEvidence;
    } else {
        decision.reason = AdmissionReason::ProbeIoError;
        decision.coverage.stop = ProbeStop::IoError;
        decision.observed.errors = 1;
    }
    Some(decision)
}

struct Probe {
    result: FolderAdmission,
    start: Instant,
    budgets: ProbeBudgets,
    root_io_error: bool,
    child_groups: BTreeSet<PathBuf>,
    workspace_override: bool,
    outside_candidates: Vec<PathBuf>,
}

impl Probe {
    fn time_left(&mut self) -> bool {
        if self.start.elapsed() >= self.budgets.time {
            self.result.coverage.stop = ProbeStop::Time;
            false
        } else {
            true
        }
    }

    fn metadata(&mut self, path: &Path) -> Option<std::fs::Metadata> {
        if !self.time_left() {
            return None;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                self.result.observed.errors += 1;
                None
            }
        }
    }

    fn finish(mut self) -> FolderAdmission {
        let coverage = &mut self.result.coverage;
        if coverage.stop == ProbeStop::Complete {
            coverage.stop = if coverage.directory_cuts > 0 {
                ProbeStop::DirectoryLimit
            } else if coverage.depth_cuts > 0 {
                ProbeStop::DepthLimit
            } else if self.result.observed.errors > 0 {
                ProbeStop::IoError
            } else {
                ProbeStop::Complete
            };
        }
        coverage.complete = coverage.stop == ProbeStop::Complete;
        coverage.workspace_complete |= coverage.complete;
        coverage.count_kind = if coverage.complete {
            CountKind::Exact
        } else {
            CountKind::LowerBound
        };
        coverage.elapsed_ms = millis(self.start.elapsed());
        self.result.observed.child_projects = self.child_groups.len();
        if self.result.classification == FolderClassification::Undetermined
            && matches!(
                self.result.reason,
                AdmissionReason::EvidenceBudgetExhausted | AdmissionReason::ProbeIoError
            )
            && self.result.bounded_examination_complete()
        {
            self.result.classification = FolderClassification::NotProject;
        }
        self.result.observed.root_markers.sort();
        self.result.observed.root_markers.dedup();
        self.result.child_roots.sort();
        self.result.child_roots.dedup();
        if let Some(root) = self.result.canonical_root.as_ref() {
            self.result.outside_paths = self
                .outside_candidates
                .iter()
                .filter(|path| {
                    !self
                        .result
                        .child_roots
                        .iter()
                        .any(|child| path.starts_with(child))
                })
                .filter_map(|path| {
                    path.strip_prefix(root)
                        .ok()?
                        .components()
                        .next()
                        .map(|part| root.join(part))
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
        }
        self.result
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn marker(name: &str) -> bool {
    MARKERS.contains(&name)
        || [".sln", ".slnx", ".csproj", ".xcodeproj"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Probe an already-authorized root. Bounds are cooperative: a filesystem
/// syscall can outlast the deadline, so callers must acknowledge root selection
/// before scheduling this work. Discard a late result if a newer root selection
/// or policy change has replaced the request that scheduled it.
pub fn probe(
    root: &Path,
    policy: &ProjectDetectionConfig,
    budgets: ProbeBudgets,
) -> FolderAdmission {
    probe_with_read_dir(root, policy, budgets, |path| {
        // Model a slow directory open (for example, an antivirus scan) without
        // changing the evidence budget or delaying direct metadata lookups.
        #[cfg(debug_assertions)]
        if let Some(delay) = std::env::var("AFT_TEST_ADMISSION_READ_DIR_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(delay));
        }
        std::fs::read_dir(path)
    })
}

fn probe_with_read_dir(
    root: &Path,
    policy: &ProjectDetectionConfig,
    budgets: ProbeBudgets,
    read_dir: impl Fn(&Path) -> std::io::Result<std::fs::ReadDir>,
) -> FolderAdmission {
    let start = Instant::now();
    let canonical = crate::path_identity::ProjectRootId::from_path(root);
    let mut probe = Probe {
        result: FolderAdmission {
            policy_version: POLICY_VERSION,
            canonical_root: canonical
                .as_ref()
                .ok()
                .map(|identity| identity.as_path().to_path_buf()),
            classification: FolderClassification::Undetermined,
            reason: AdmissionReason::NoProjectEvidence,
            observed: Observed::default(),
            coverage: ProbeCoverage {
                complete: false,
                count_kind: CountKind::LowerBound,
                root_listing_complete: false,
                workspace_complete: false,
                stop: ProbeStop::Complete,
                entry_budget: budgets.entries,
                time_budget_ms: millis(budgets.time),
                depth_budget: budgets.depth,
                per_directory_budget: budgets.per_directory,
                directory_cuts: 0,
                depth_cuts: 0,
                elapsed_ms: 0,
            },
            override_mode: DetectionMode::Auto,
            tracked: None,
            child_roots: Vec::new(),
            immediate_directories: Vec::new(),
            observed_directories: Vec::new(),
            immediate_entries: Vec::new(),
            evidence_markers: Vec::new(),
            git_controls: Vec::new(),
            outside_paths: Vec::new(),
        },
        start,
        budgets,
        root_io_error: false,
        child_groups: BTreeSet::new(),
        workspace_override: false,
        outside_candidates: Vec::new(),
    };
    let root = probe
        .result
        .canonical_root
        .clone()
        .unwrap_or_else(|| root.to_path_buf());
    if canonical.is_err() || !root.is_dir() {
        probe.result.observed.errors += 1;
        probe.result.coverage.stop = ProbeStop::IoError;
        probe.result.reason = AdmissionReason::ProbeIoError;
        return probe.finish();
    }
    probe.result.override_mode = policy.mode_for_root(&root);
    if let Some(decision) = boundary_decision(&root, probe.result.override_mode) {
        return decision;
    }
    if let Some(decision) = override_decision(&root, probe.result.override_mode) {
        return decision;
    }
    if probe.result.override_mode == DetectionMode::Workspace {
        if probe.time_left() && read_dir(&root).is_err() {
            probe.result.observed.errors += 1;
            probe.result.reason = AdmissionReason::ProbeIoError;
            probe.result.coverage.stop = ProbeStop::IoError;
            return probe.finish();
        }
        probe.workspace_override = true;
        probe.walk(&root, read_dir);
        probe.result.classification = FolderClassification::Workspace;
        probe.result.reason = AdmissionReason::UserOverride;
        return probe.finish();
    }
    // Three immediate personal directories (such as Desktop, Downloads and
    // Documents) identify a mixed personal container. Fixed-name checks keep
    // that pattern and root manifests independent of directory listing order.
    for name in PERSONAL {
        if probe.metadata(&root.join(name)).is_some_and(|m| m.is_dir()) {
            probe.result.observed.personal_groups += 1;
        }
    }
    if probe.result.observed.errors > 0 {
        probe.result.reason = AdmissionReason::ProbeIoError;
        probe.result.coverage.stop = ProbeStop::IoError;
        return probe.finish();
    }
    if probe.result.observed.personal_groups >= 3 {
        probe.result.classification = FolderClassification::NotProject;
        probe.result.reason = AdmissionReason::MixedPersonalTree;
        probe.result.coverage.stop = ProbeStop::StrongEvidence;
        return probe.finish();
    }
    // Ordinary repositories and linked worktrees have enough control metadata
    // to decide without starting Git. The time-limited `git rev-parse` fallback
    // also recognizes subdirectories of a parent repository and unusual layouts.
    if let Some(head) = direct_git_head(&root, &mut probe) {
        probe.git_project(head);
        return probe.finish();
    }
    for name in MARKERS {
        if probe
            .metadata(&root.join(name))
            .is_some_and(|m| m.is_file())
        {
            probe.result.observed.root_markers.push((*name).to_owned());
            probe.result.classification = FolderClassification::Project;
            probe.result.reason = AdmissionReason::RootMarker;
            probe.result.coverage.stop = ProbeStop::StrongEvidence;
            return probe.finish();
        }
    }
    if let Some(head) = probe
        .time_left()
        .then(|| validated_git_worktree(&root, start, budgets.time))
        .flatten()
    {
        probe.git_project(head);
        return probe.finish();
    }

    probe.walk(&root, read_dir);
    if probe.result.classification == FolderClassification::Project {
        return probe.finish();
    }
    let observed = &probe.result.observed;
    if probe.root_io_error {
        probe.result.reason = AdmissionReason::ProbeIoError;
    } else if probe.result.coverage.root_listing_complete
        && probe.child_groups.len() >= 2
        && probe.child_groups.len() * 2 >= observed.non_bulk_root_directories
    {
        probe.result.classification = FolderClassification::Workspace;
        probe.result.reason = AdmissionReason::MultipleChildProjects;
    } else if probe.result.coverage.stop == ProbeStop::Complete
        && probe.result.coverage.directory_cuts == 0
        && probe.result.coverage.depth_cuts == 0
        && observed.errors == 0
    {
        if observed.eligible_source_files >= 2
            && observed.eligible_source_files * 2 >= observed.files - observed.bulk_files
        {
            probe.result.classification = FolderClassification::Project;
            probe.result.reason = AdmissionReason::SmallSourceTree;
        } else {
            probe.result.classification = FolderClassification::NotProject;
        }
    } else {
        probe.result.reason = AdmissionReason::EvidenceBudgetExhausted;
    }
    probe.finish()
}

impl Probe {
    fn git_project(&mut self, head: PathBuf) {
        // Replay validation and the metadata watcher use HEAD as their anchor.
        self.result.git_controls.insert(0, head.clone());
        if let Some(directory) = head.parent() {
            self.result.git_controls.push(directory.to_path_buf());
            for name in ["index", "config", "commondir", "objects"] {
                self.result.git_controls.push(directory.join(name));
            }
        }
        self.result.classification = FolderClassification::Project;
        self.result.reason = AdmissionReason::GitCheckout;
        self.result.coverage.stop = ProbeStop::StrongEvidence;
    }
}

fn direct_git_head(root: &Path, probe: &mut Probe) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let metadata = probe.metadata(&dot_git)?;
    let gitdir = if metadata.is_dir() {
        dot_git
    } else if metadata.is_file() && probe.time_left() {
        let target = crate::project_content::read_gitdir_target(&dot_git)?;
        root.join(target)
    } else {
        return None;
    };
    let head = gitdir.join("HEAD");
    if !probe.metadata(&head)?.is_file() {
        return None;
    }
    let common = if probe.metadata(&gitdir.join("commondir")).is_some() {
        if !probe.time_left() {
            return None;
        }
        gitdir.join(crate::project_content::read_commondir_target(&gitdir)?)
    } else {
        gitdir
    };
    if !probe.metadata(&common.join("objects"))?.is_dir()
        || !probe.metadata(&common.join("refs"))?.is_dir()
    {
        return None;
    }
    probe
        .result
        .git_controls
        .extend([common.clone(), common.join("objects"), common.join("refs")]);
    Some(head)
}

impl Probe {
    fn child_markers(&mut self, directory: &Path, group: &Path) -> bool {
        if self
            .result
            .child_roots
            .iter()
            .any(|project| directory.starts_with(project))
        {
            return true;
        }
        let git = directory.join(".git");
        let git_marker = self
            .metadata(&git)
            .is_some_and(|m| m.is_dir() || m.is_file())
            && child_git_evidence(directory, &git, self.start, self.budgets.time);
        let found = if git_marker {
            Some(git)
        } else {
            MARKERS.iter().find_map(|name| {
                let path = directory.join(name);
                self.metadata(&path)
                    .is_some_and(|m| m.is_file())
                    .then_some(path)
            })
        };
        if let Some(marker) = found {
            self.child_groups.insert(group.to_path_buf());
            self.result.child_roots.push(directory.to_path_buf());
            self.result.evidence_markers.push(marker);
            true
        } else {
            false
        }
    }

    fn enough_workspace_evidence(&self) -> bool {
        self.result.coverage.root_listing_complete
            && self.child_groups.len() >= 2
            && self.child_groups.len() * 2 >= self.result.observed.non_bulk_root_directories
    }

    fn walk(&mut self, root: &Path, read_dir: impl Fn(&Path) -> std::io::Result<std::fs::ReadDir>) {
        let mut pending = VecDeque::from([(root.to_path_buf(), 0, false, PathBuf::new())]);
        let mut deferred = VecDeque::new();
        let mut diagnostics = false;
        loop {
            if pending.is_empty() {
                if !diagnostics && self.enough_workspace_evidence() {
                    self.result.coverage.workspace_complete = self.result.coverage.directory_cuts
                        == 0
                        && self.result.coverage.depth_cuts == 0
                        && self.result.observed.errors == 0;
                    self.result.coverage.stop = ProbeStop::StrongEvidence;
                    return;
                }
                if deferred.is_empty() {
                    break;
                }
                pending.append(&mut deferred);
                diagnostics = true;
            }
            let Some((directory, level, bulk, top)) = pending.pop_front() else {
                break;
            };
            if !self.time_left() || self.result.observed.entries >= self.budgets.entries {
                if self.result.coverage.stop != ProbeStop::Time {
                    self.result.coverage.stop = ProbeStop::Entries;
                }
                break;
            }
            if !diagnostics && level > 2 {
                if self.enough_workspace_evidence() {
                    if !bulk {
                        self.outside_candidates.push(directory.clone());
                    }
                    self.outside_candidates.extend(
                        pending
                            .iter()
                            .filter(|(_, _, bulk, _)| !*bulk)
                            .map(|(directory, _, _, _)| directory.clone()),
                    );
                    self.result.coverage.workspace_complete = self.result.coverage.directory_cuts
                        == 0
                        && self.result.coverage.depth_cuts == 0
                        && self.result.observed.errors == 0;
                    self.result.coverage.stop = ProbeStop::StrongEvidence;
                    return;
                }
                pending.append(&mut deferred);
                diagnostics = true;
            }
            let owned = self
                .result
                .child_roots
                .iter()
                .any(|project| directory.starts_with(project));
            if level > 0 && level <= 2 && !bulk && !owned {
                self.child_markers(&directory, &top);
            }
            if !diagnostics
                && level > 0
                && self
                    .result
                    .child_roots
                    .iter()
                    .any(|project| project == &directory)
            {
                deferred.push_back((directory, level, bulk, top));
                continue;
            }
            let entries = match read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => {
                    self.result.observed.errors += 1;
                    self.root_io_error |= level == 0;
                    continue;
                }
            };
            let mut listing_complete = true;
            for (ordinal, entry) in entries.enumerate() {
                if !self.time_left() || self.result.observed.entries >= self.budgets.entries {
                    if self.result.coverage.stop != ProbeStop::Time {
                        self.result.coverage.stop = ProbeStop::Entries;
                    }
                    listing_complete = false;
                    break;
                }
                self.result.observed.entries += 1;
                // A counted look-ahead distinguishes a full directory from a cut
                // without collecting or sorting all of its names.
                if ordinal == self.budgets.per_directory {
                    self.result.coverage.directory_cuts += 1;
                    listing_complete = false;
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        self.result.observed.errors += 1;
                        listing_complete = false;
                        continue;
                    }
                };
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(_) => {
                        self.result.observed.errors += 1;
                        listing_complete = false;
                        continue;
                    }
                };
                if kind.is_symlink() {
                    self.result.observed.symlinks += 1;
                    continue;
                }
                let path = entry.path();
                if level == 0 {
                    self.result.immediate_entries.push(path.clone());
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let group = if level == 0 {
                    path.clone()
                } else {
                    top.clone()
                };
                if name == ".git" {
                    if kind.is_dir() {
                        self.result.observed.git_directories_skipped += 1;
                    }
                    if level > 0
                        && level <= 2
                        && !bulk
                        && !owned
                        && (kind.is_dir() || kind.is_file())
                        && child_git_evidence(&directory, &path, self.start, self.budgets.time)
                    {
                        self.child_groups.insert(group);
                        self.result.child_roots.push(directory.clone());
                        self.result.evidence_markers.push(path.clone());
                    }
                    continue;
                }
                if marker(&name)
                    && (kind.is_file() || (kind.is_dir() && name.ends_with(".xcodeproj")))
                {
                    if level == 0 {
                        self.result.observed.root_markers.push(name.to_string());
                        if !self.workspace_override {
                            self.result.classification = FolderClassification::Project;
                            self.result.reason = AdmissionReason::RootMarker;
                            self.result.coverage.stop = ProbeStop::StrongEvidence;
                            return;
                        }
                    } else if level <= 2 && !bulk && !owned {
                        self.child_groups.insert(group.clone());
                        self.result.child_roots.push(directory.clone());
                        self.result.evidence_markers.push(path.clone());
                    }
                }
                if kind.is_dir() {
                    self.result.observed.directories += 1;
                    self.result.observed_directories.push(path.clone());
                    if level == 0 {
                        self.result.immediate_directories.push(path.clone());
                    }
                    let next_bulk = bulk || BULK.contains(&name.as_ref());
                    if level == 0 && !next_bulk {
                        self.result.observed.non_bulk_root_directories += 1;
                    }
                    if level < self.budgets.depth {
                        pending.push_back((path, level + 1, next_bulk, group));
                    } else {
                        self.result.coverage.depth_cuts += 1;
                    }
                } else if kind.is_file() {
                    if !bulk
                        && !group
                            .file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                    {
                        self.outside_candidates.push(path.clone());
                    }
                    self.result.observed.files += 1;
                    let source = path
                        .extension()
                        .and_then(|s| s.to_str())
                        .is_some_and(|extension| SOURCE.contains(&extension));
                    self.result.observed.source_files += usize::from(source);
                    self.result.observed.bulk_files += usize::from(bulk);
                    self.result.observed.eligible_source_files += usize::from(source && !bulk);
                }
            }
            if level == 0 {
                self.result.coverage.root_listing_complete = listing_complete;
                // Inspect every immediate candidate's fixed markers before
                // spending the shared budget on one project's source files.
                let candidates = pending
                    .iter()
                    .filter(|(_, level, bulk, _)| *level == 1 && !*bulk)
                    .map(|(directory, _, _, top)| (directory.clone(), top.clone()))
                    .collect::<Vec<_>>();
                for (directory, top) in candidates {
                    if !self.time_left() {
                        break;
                    }
                    self.child_markers(&directory, &top);
                }
            }
            if matches!(
                self.result.coverage.stop,
                ProbeStop::Entries | ProbeStop::Time
            ) {
                break;
            }
        }
    }
}

/// Git's read-only plumbing accepts empty repositories, subdirectories and
/// linked worktrees, but not bare databases or a stray directory named `.git`.
/// Never parse the index or walk the worktree just to establish this evidence.
fn validated_git_worktree(root: &Path, start: Instant, budget: Duration) -> Option<PathBuf> {
    let bytes = git_output(
        root,
        start,
        budget,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "HEAD",
            "--is-inside-work-tree",
        ],
    )?;
    let head = bytes
        .strip_suffix(b"\ntrue\n")
        .or_else(|| bytes.strip_suffix(b"\r\ntrue\r\n"))?;
    let path = crate::git_probe::git_output_path(head)?;
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn child_git_evidence(root: &Path, marker: &Path, start: Instant, budget: Duration) -> bool {
    // Directory control metadata is enough for a workspace candidate, not an
    // admission to build that child's index. Opening the child validates its own
    // working tree. Gitfiles and unusual layouts use bounded Git validation.
    if std::fs::symlink_metadata(marker).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(marker.join("HEAD")).is_ok_and(|m| m.is_file())
        && std::fs::symlink_metadata(marker.join("objects")).is_ok_and(|m| m.is_dir())
        && std::fs::symlink_metadata(marker.join("refs")).is_ok_and(|m| m.is_dir())
    {
        return true;
    }
    git_output(root, start, budget, &["rev-parse", "--show-toplevel"])
        .and_then(|bytes| crate::git_probe::git_output_path(&bytes))
        .and_then(|path| crate::path_identity::ProjectRootId::from_path(&path).ok())
        .is_some_and(|git_root| {
            crate::path_identity::ProjectRootId::from_path(root)
                .ok()
                .as_ref()
                == Some(&git_root)
        })
}

#[cfg(test)]
thread_local! {
    static GIT_VALIDATION_CALLS_FOR_TEST: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn git_output(root: &Path, start: Instant, budget: Duration, args: &[&str]) -> Option<Vec<u8>> {
    #[cfg(test)]
    GIT_VALIDATION_CALLS_FOR_TEST.with(|calls| calls.set(calls.get() + 1));
    if start.elapsed() >= budget || !crate::developer_tools::git_usable() {
        return None;
    }
    let mut command = crate::git_probe::command();
    command
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return None;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    child.wait_with_output().ok().map(|output| output.stdout)
                } else {
                    None
                };
            }
            Ok(None) if start.elapsed() < budget => {
                std::thread::sleep(Duration::from_millis(1));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures can live under this checkout when TMPDIR is worktree-local.
    // Do not mistake the enclosing AFT repository for fixture Git evidence.
    fn isolated<R>(root: &Path, body: impl FnOnce() -> R) -> R {
        let ceiling = root.parent().unwrap_or(root);
        crate::git_probe::with_child_env_for_test(
            &[("GIT_CEILING_DIRECTORIES", ceiling.as_os_str())],
            body,
        )
    }

    fn probe(
        root: &Path,
        policy: &ProjectDetectionConfig,
        budgets: ProbeBudgets,
    ) -> FolderAdmission {
        isolated(root, || super::probe(root, policy, budgets))
    }

    fn probe_with_read_dir(
        root: &Path,
        policy: &ProjectDetectionConfig,
        budgets: ProbeBudgets,
        read_dir: impl Fn(&Path) -> std::io::Result<std::fs::ReadDir>,
    ) -> FolderAdmission {
        isolated(root, || {
            super::probe_with_read_dir(root, policy, budgets, read_dir)
        })
    }

    fn classify(root: &Path) -> FolderAdmission {
        probe(
            root,
            &ProjectDetectionConfig::default(),
            // Shape tests exercise evidence rules, not host Git startup speed.
            // Deadline and default-budget tests pass their own budgets below.
            ProbeBudgets {
                time: Duration::from_secs(5),
                ..ProbeBudgets::default()
            },
        )
    }

    fn file(root: &Path, name: &str) {
        std::fs::write(root.join(name), []).unwrap();
    }

    fn git(root: &Path, args: &[&str]) {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(root));
        let output = command.args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn approved_default_budgets() {
        let budgets = ProbeBudgets::default();
        assert_eq!(budgets.entries, 4096);
        assert_eq!(budgets.time, Duration::from_millis(100));
        assert_eq!(budgets.depth, 6);
        assert_eq!(budgets.per_directory, 256);
    }

    #[test]
    fn home_shape_precedes_incidental_git_and_root_marker() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        file(root.path(), "package.json");
        for name in ["Desktop", "Downloads", "Documents"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::NotProject);
        assert_eq!(result.reason, AdmissionReason::MixedPersonalTree);
        assert_eq!(result.observed.personal_groups, 3);
        assert!(!result.coverage.complete);
    }

    #[test]
    fn tmp_shape_with_one_stray_repo_is_not_a_workspace() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..10 {
            let run = root.path().join(format!("run-{i}"));
            std::fs::create_dir(&run).unwrap();
            file(&run, "output.log");
        }
        let stray = root.path().join("stray");
        std::fs::create_dir(&stray).unwrap();
        git(&stray, &["init", "-q"]);
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::NotProject);
        assert_eq!(result.reason, AdmissionReason::NoProjectEvidence);
        assert_eq!(result.observed.child_projects, 1);
        assert!(result.coverage.complete);
    }

    #[test]
    fn concentrated_child_projects_make_a_workspace_and_keep_actual_roots() {
        let root = tempfile::tempdir().unwrap();
        for name in ["one", "two", "three", "notes"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        file(&root.path().join("one"), "Cargo.toml");
        let nested = root.path().join("two/app");
        std::fs::create_dir(&nested).unwrap();
        file(&nested, "package.json");
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::Workspace);
        assert_eq!(result.reason, AdmissionReason::MultipleChildProjects);
        assert_eq!(result.observed.child_projects, 2);
        assert_eq!(result.observed.non_bulk_root_directories, 4);
        let nested = crate::path_identity::ProjectRootId::from_path(&nested).unwrap();
        assert!(result.child_roots.contains(&nested.as_path().to_path_buf()));
        let container =
            crate::path_identity::ProjectRootId::from_path(&root.path().join("two")).unwrap();
        assert!(!result
            .child_roots
            .contains(&container.as_path().to_path_buf()));
    }

    #[test]
    fn sparse_child_projects_and_bulk_manifests_do_not_admit_a_container() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..6 {
            let child = root.path().join(i.to_string());
            std::fs::create_dir(&child).unwrap();
            if i < 2 {
                file(&child, "package.json");
            }
        }
        let bulk = root.path().join("node_modules");
        std::fs::create_dir(&bulk).unwrap();
        file(&bulk, "package.json");
        file(&bulk, "a.js");
        file(&bulk, "b.js");
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::NotProject);
        assert_eq!(result.observed.child_projects, 2);
        assert_eq!(result.observed.eligible_source_files, 0);
    }

    #[test]
    fn tiny_marker_free_source_tree_needs_two_sources_and_half_eligible_share() {
        let root = tempfile::tempdir().unwrap();
        file(root.path(), "a.py");
        assert_eq!(
            classify(root.path()).classification,
            FolderClassification::NotProject
        );
        file(root.path(), "b.rs");
        file(root.path(), "README.md");
        file(root.path(), "data.json");
        let result = classify(root.path());
        assert_eq!(result.reason, AdmissionReason::SmallSourceTree);
        assert_eq!(result.observed.files, 4);
        assert!(result.coverage.complete);
        file(root.path(), "extra.dat");
        assert_eq!(
            classify(root.path()).classification,
            FolderClassification::NotProject
        );
    }

    #[test]
    fn root_marker_admits_at_any_size_and_before_listing() {
        let root = tempfile::tempdir().unwrap();
        file(root.path(), "Cargo.toml");
        let result = probe(
            root.path(),
            &ProjectDetectionConfig::default(),
            ProbeBudgets {
                entries: 0,
                ..ProbeBudgets::default()
            },
        );
        assert_eq!(result.classification, FolderClassification::Project);
        assert_eq!(result.reason, AdmissionReason::RootMarker);
        assert_eq!(result.observed.entries, 0);
        assert_eq!(result.observed.root_markers, ["Cargo.toml"]);
        assert!(!result.coverage.complete);
    }

    #[test]
    fn strong_root_evidence_does_not_open_a_directory() {
        for name in ["Cargo.toml", "package.json", "Makefile", ".git"] {
            let root = tempfile::tempdir().unwrap();
            if name == ".git" {
                git(root.path(), &["init", "-q"]);
            } else {
                file(root.path(), name);
            }
            super::GIT_VALIDATION_CALLS_FOR_TEST.with(|calls| calls.set(0));
            let result = probe_with_read_dir(
                root.path(),
                &ProjectDetectionConfig::default(),
                ProbeBudgets::default(),
                |_| panic!("directory enumeration preceded strong {name} evidence"),
            );
            assert_eq!(
                result.classification,
                FolderClassification::Project,
                "{name}: {result:?}"
            );
            assert_eq!(result.coverage.stop, ProbeStop::StrongEvidence);
            assert_eq!(result.observed.entries, 0);
            if name == ".git" {
                assert_eq!(result.git_controls[0].file_name().unwrap(), "HEAD");
            }
            super::GIT_VALIDATION_CALLS_FOR_TEST.with(|calls| assert_eq!(calls.get(), 0, "{name}"));
        }
    }

    #[test]
    fn linked_worktree_control_metadata_decides_without_git_or_listing() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let linked = temp.path().join("linked");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        );
        git(
            &main,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );
        super::GIT_VALIDATION_CALLS_FOR_TEST.with(|calls| calls.set(0));
        let result = probe_with_read_dir(
            &linked,
            &ProjectDetectionConfig::default(),
            ProbeBudgets::default(),
            |_| panic!("linked-worktree evidence must precede directory enumeration"),
        );
        assert_eq!(
            result.classification,
            FolderClassification::Project,
            "{result:?}"
        );
        assert_eq!(result.reason, AdmissionReason::GitCheckout);
        assert_eq!(result.observed.entries, 0);
        assert_eq!(result.git_controls[0].file_name().unwrap(), "HEAD");
        super::GIT_VALIDATION_CALLS_FOR_TEST.with(|calls| assert_eq!(calls.get(), 0));
    }

    #[test]
    fn bounded_wildcard_marker_is_root_evidence_not_child_evidence() {
        let root = tempfile::tempdir().unwrap();
        file(root.path(), "app.slnx");
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::Project);
        assert_eq!(result.reason, AdmissionReason::RootMarker);
        assert_eq!(result.observed.root_markers, ["app.slnx"]);
    }

    #[test]
    fn zero_time_entry_width_and_depth_budgets_are_honest_cuts() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..20 {
            file(root.path(), &format!("{i}.rs"));
        }
        for budgets in [
            ProbeBudgets {
                time: Duration::ZERO,
                ..ProbeBudgets::default()
            },
            ProbeBudgets {
                entries: 3,
                ..ProbeBudgets::default()
            },
            ProbeBudgets {
                per_directory: 3,
                ..ProbeBudgets::default()
            },
        ] {
            let result = crate::developer_tools::with_git_usable_for_test(false, || {
                probe(root.path(), &ProjectDetectionConfig::default(), budgets)
            });
            assert_eq!(result.reason, AdmissionReason::EvidenceBudgetExhausted);
            assert!(!result.coverage.complete);
            assert!(result.observed.entries <= budgets.entries);
            assert!(result.observed.entries <= budgets.per_directory + 1);
            if budgets.time.is_zero() {
                assert_eq!(result.classification, FolderClassification::Undetermined);
                assert_eq!(result.observed.entries, 0);
            } else {
                assert_eq!(result.classification, FolderClassification::NotProject);
                assert!(result.is_determined());
            }
        }
        let tree = tempfile::tempdir().unwrap();
        let child = tree.path().join("child");
        std::fs::create_dir(&child).unwrap();
        file(&child, "a.rs");
        file(&child, "b.rs");
        // This case measures the depth cut, not the startup time of host Git.
        let result = crate::developer_tools::with_git_usable_for_test(false, || {
            probe(
                tree.path(),
                &ProjectDetectionConfig::default(),
                ProbeBudgets {
                    depth: 0,
                    ..ProbeBudgets::default()
                },
            )
        });
        assert_eq!(result.coverage.stop, ProbeStop::DepthLimit);
        assert_eq!(result.observed.files, 0);
    }

    #[test]
    fn cut_root_listing_does_not_claim_workspace_concentration() {
        let root = tempfile::tempdir().unwrap();
        for i in 0..10 {
            let child = root.path().join(i.to_string());
            std::fs::create_dir(&child).unwrap();
            file(&child, "Cargo.toml");
        }
        let result = probe(
            root.path(),
            &ProjectDetectionConfig::default(),
            ProbeBudgets {
                per_directory: 2,
                time: Duration::from_secs(5),
                ..ProbeBudgets::default()
            },
        );
        assert!(!result.coverage.root_listing_complete);
        assert_eq!(result.classification, FolderClassification::NotProject);
        assert_eq!(result.observed.child_projects, 2);
    }

    #[test]
    fn missing_or_non_directory_root_is_an_io_error_not_an_empty_tree() {
        let root = tempfile::tempdir().unwrap();
        file(root.path(), "file");
        for name in ["missing", "file"] {
            let result = classify(&root.path().join(name));
            assert_eq!(result.reason, AdmissionReason::ProbeIoError);
            assert_eq!(result.observed.errors, 1);
            assert!(!result.coverage.complete);
        }
    }

    #[test]
    fn unreadable_root_reports_probe_io_error_without_retrying() {
        let root = tempfile::tempdir().unwrap();
        let calls = std::cell::Cell::new(0);
        let result = probe_with_read_dir(
            root.path(),
            &ProjectDetectionConfig::default(),
            ProbeBudgets::default(),
            |_| {
                calls.set(calls.get() + 1);
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            },
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(result.classification, FolderClassification::Undetermined);
        assert_eq!(result.reason, AdmissionReason::ProbeIoError);
        assert_eq!(result.coverage.stop, ProbeStop::IoError);
        assert_eq!(result.observed.errors, 1);
        assert!(!result.coverage.complete);
    }

    #[test]
    fn override_precedes_personal_shape_and_is_exact_root_only() {
        let root = tempfile::tempdir().unwrap();
        for name in ["Desktop", "Downloads", "Documents"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let policy = ProjectDetectionConfig {
            mode: DetectionMode::Auto,
            overrides: vec![DetectionOverride {
                path: root.path().to_path_buf(),
                mode: DetectionMode::Project,
            }],
        };
        let result = probe(root.path(), &policy, ProbeBudgets::default());
        assert_eq!(result.classification, FolderClassification::Project);
        assert_eq!(result.reason, AdmissionReason::UserOverride);
        assert_eq!(result.observed.entries, 0);
        assert_eq!(
            policy.mode_for_root(&root.path().join("Desktop")),
            DetectionMode::Auto
        );
        for mode in [DetectionMode::Off, DetectionMode::Workspace] {
            let result = probe(
                root.path(),
                &ProjectDetectionConfig {
                    mode,
                    overrides: vec![],
                },
                ProbeBudgets::default(),
            );
            assert_eq!(result.override_mode, mode);
            assert_eq!(result.reason, AdmissionReason::UserOverride);
        }
    }

    #[test]
    fn folder_admission_regression_project_override_does_not_list() {
        let root = tempfile::tempdir().unwrap();
        let result = probe_with_read_dir(
            root.path(),
            &ProjectDetectionConfig {
                mode: DetectionMode::Project,
                overrides: vec![],
            },
            ProbeBudgets::default(),
            |_| panic!("a project override must not run an evidence listing"),
        );
        assert_eq!(result.classification, FolderClassification::Project);
        assert_eq!(result.reason, AdmissionReason::UserOverride);
        assert_eq!(result.observed.entries, 0);
    }

    #[test]
    fn folder_admission_regression_unexamined_probe_has_no_classification() {
        let root = tempfile::tempdir().unwrap();
        let result = probe(
            root.path(),
            &ProjectDetectionConfig::default(),
            ProbeBudgets {
                time: Duration::ZERO,
                ..ProbeBudgets::default()
            },
        );
        assert_eq!(result.reason, AdmissionReason::EvidenceBudgetExhausted);
        assert!(!result.coverage.root_listing_complete);
        assert_eq!(result.observed.entries, 0);
        assert_eq!(
            serde_json::to_value(result).unwrap()["classification"],
            serde_json::Value::Null
        );
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_root_is_refused_without_listing_even_with_an_override() {
        for mode in [
            DetectionMode::Auto,
            DetectionMode::Project,
            DetectionMode::Workspace,
            DetectionMode::Off,
        ] {
            let result = probe_with_read_dir(
                Path::new("/"),
                &ProjectDetectionConfig {
                    mode,
                    overrides: vec![],
                },
                ProbeBudgets {
                    time: Duration::ZERO,
                    ..ProbeBudgets::default()
                },
                |_| panic!("structural roots must not enumerate evidence"),
            );
            assert_eq!(result.classification, FolderClassification::NotProject);
            assert!(result.is_structural_refusal());
            assert!(result.is_determined());
            assert_eq!(result.observed.entries, 0);
        }
    }

    #[test]
    fn full_entry_budget_is_a_decision_but_early_time_and_io_are_not() {
        let root = tempfile::tempdir().unwrap();
        for stop in [ProbeStop::Time, ProbeStop::IoError] {
            let mut result = exhausted(
                root.path(),
                DetectionMode::Auto,
                ProbeBudgets {
                    entries: 3,
                    ..ProbeBudgets::default()
                },
            );
            result.classification = FolderClassification::NotProject;
            result.coverage.stop = stop;
            result.observed.entries = 3;
            assert!(result.is_determined());
            result.observed.entries = 2;
            assert!(!result.is_determined());
            result.observed.entries = 0;
            assert!(!result.is_determined());
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonical_override_matches_alias_but_symlinks_are_not_evidence() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        file(other.path(), "package.json");
        symlink(other.path(), root.path().join("linked-project")).unwrap();
        symlink(
            other.path().join("package.json"),
            root.path().join("Cargo.toml"),
        )
        .unwrap();
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::NotProject);
        assert_eq!(result.observed.symlinks, 2);
        let alias = root.path().join("linked-project");
        let policy = ProjectDetectionConfig {
            mode: DetectionMode::Auto,
            overrides: vec![DetectionOverride {
                path: alias.clone(),
                mode: DetectionMode::Off,
            }],
        };
        assert_eq!(policy.mode_for_root(other.path()), DetectionMode::Off);
        assert_eq!(
            probe(&alias, &policy, ProbeBudgets::default()).reason,
            AdmissionReason::UserOverride
        );
    }

    #[test]
    fn git_validation_rejects_a_stub_and_bare_repository() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        assert_eq!(
            classify(root.path()).classification,
            FolderClassification::NotProject
        );
        let bare = tempfile::tempdir().unwrap();
        git(bare.path(), &["init", "--bare", "-q"]);
        assert_eq!(
            classify(bare.path()).classification,
            FolderClassification::NotProject
        );
    }

    #[test]
    fn project_subdirectory_inherits_git_evidence_without_broadening_the_root() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        let child = root.path().join("src");
        std::fs::create_dir(&child).unwrap();
        let result = isolated(root.path(), || {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let result = super::probe(
                    &child,
                    &ProjectDetectionConfig::default(),
                    ProbeBudgets::default(),
                );
                if result.is_determined() {
                    break result;
                }
                // A probe timeout is not a verdict; retry with the same default budget.
                assert_eq!(result.reason, AdmissionReason::EvidenceBudgetExhausted);
                assert!(
                    Instant::now() < deadline,
                    "Git evidence never became available: {result:?}"
                );
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        assert_eq!(result.reason, AdmissionReason::GitCheckout);
        assert_eq!(
            result.canonical_root,
            Some(
                crate::path_identity::ProjectRootId::from_path(&child)
                    .unwrap()
                    .into_path_buf()
            )
        );
    }

    #[test]
    fn git_repo_over_twenty_thousand_tracked_paths_is_still_a_project() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        for i in 0..20_001 {
            file(root.path(), &format!("{i}.dat"));
        }
        git(root.path(), &["add", "."]);
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::Project);
        assert_eq!(result.reason, AdmissionReason::GitCheckout);
        assert_eq!(result.observed.entries, 0);
        assert!(result.tracked.is_none());
    }

    fn scratch_is_rejected_with_bounded_observations(shards: usize, runs: usize, names: usize) {
        let root = tempfile::tempdir().unwrap();
        for shard in 0..shards {
            let shard = root.path().join(shard.to_string());
            std::fs::create_dir(&shard).unwrap();
            for run in 0..runs {
                let leaf = shard.join(run.to_string());
                std::fs::create_dir(&leaf).unwrap();
                file(&leaf, "seed.dat");
                for name in 1..names {
                    std::fs::hard_link(leaf.join("seed.dat"), leaf.join(format!("{name}.dat")))
                        .unwrap();
                }
            }
        }
        let result = classify(root.path());
        assert_eq!(result.classification, FolderClassification::NotProject);
        assert_eq!(result.reason, AdmissionReason::EvidenceBudgetExhausted);
        assert!(result.observed.entries <= 4096);
        assert!(!result.coverage.complete);
        let wire = serde_json::to_value(result).unwrap();
        assert_eq!(wire["coverage"]["entry_budget"], 4096);
        assert_eq!(wire["classification"], "not_project");
    }

    #[test]
    fn scratch_is_rejected_at_the_entry_budget() {
        scratch_is_rejected_with_bounded_observations(5, 20, 500);
    }

    // Fifty thousand names already exceed the evidence budget. The full test
    // performs millions of filesystem operations, so reserve it for manual runs.
    #[test]
    #[ignore]
    fn million_name_scratch_is_rejected_with_bounded_observations() {
        scratch_is_rejected_with_bounded_observations(21, 100, 1000);
    }
}
