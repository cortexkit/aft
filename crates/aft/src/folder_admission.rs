//! Bounded project-intent evidence, independent of index configuration.
//!
//! Callers must authorize the root before probing it. This module never reads
//! worktree file contents or follows directory symlinks. Git plumbing validates
//! repository control metadata only; it does not inventory tracked paths.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const POLICY_VERSION: u32 = 1;

const MARKERS: &[&str] = &[
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
const PERSONAL: &[&str] = &[
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
    pub root_listing_complete: bool,
    pub stop: ProbeStop,
    pub entry_budget: usize,
    pub time_budget_ms: u64,
    pub depth_budget: usize,
    pub per_directory_budget: usize,
    pub directory_cuts: usize,
    pub depth_cuts: usize,
    pub elapsed_ms: u64,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedCount {
    pub count: usize,
    pub index_identity: String,
}

struct Probe {
    result: FolderAdmission,
    start: Instant,
    budgets: ProbeBudgets,
    root_io_error: bool,
    child_groups: BTreeSet<PathBuf>,
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
        coverage.elapsed_ms = millis(self.start.elapsed());
        self.result.observed.child_projects = self.child_groups.len();
        self.result.observed.root_markers.sort();
        self.result.observed.root_markers.dedup();
        self.result.child_roots.sort();
        self.result.child_roots.dedup();
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
    probe_with_read_dir(root, policy, budgets, |path| std::fs::read_dir(path))
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
            classification: FolderClassification::NotProject,
            reason: AdmissionReason::NoProjectEvidence,
            observed: Observed::default(),
            coverage: ProbeCoverage {
                complete: false,
                root_listing_complete: false,
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
        },
        start,
        budgets,
        root_io_error: false,
        child_groups: BTreeSet::new(),
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
    if probe.result.override_mode != DetectionMode::Auto {
        probe.result.classification = match probe.result.override_mode {
            DetectionMode::Project => FolderClassification::Project,
            DetectionMode::Workspace => FolderClassification::Workspace,
            DetectionMode::Off => FolderClassification::NotProject,
            DetectionMode::Auto => unreachable!(),
        };
        probe.result.reason = AdmissionReason::UserOverride;
        probe.result.coverage.stop = ProbeStop::UserOverride;
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
    if probe.result.observed.personal_groups >= 3 {
        probe.result.reason = AdmissionReason::MixedPersonalTree;
        probe.result.coverage.stop = ProbeStop::StrongEvidence;
        return probe.finish();
    }
    for name in MARKERS {
        if probe
            .metadata(&root.join(name))
            .is_some_and(|m| m.is_file())
        {
            probe.result.observed.root_markers.push((*name).to_owned());
        }
    }
    if probe.time_left() && validated_git_worktree(&root, start, budgets.time) {
        probe.result.classification = FolderClassification::Project;
        probe.result.reason = AdmissionReason::GitCheckout;
        probe.result.coverage.stop = ProbeStop::StrongEvidence;
        return probe.finish();
    }
    if !probe.result.observed.root_markers.is_empty() {
        probe.result.classification = FolderClassification::Project;
        probe.result.reason = AdmissionReason::RootMarker;
        probe.result.coverage.stop = ProbeStop::StrongEvidence;
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
        }
    } else {
        probe.result.reason = AdmissionReason::EvidenceBudgetExhausted;
    }
    probe.finish()
}

impl Probe {
    fn walk(&mut self, root: &Path, read_dir: impl Fn(&Path) -> std::io::Result<std::fs::ReadDir>) {
        let mut pending = VecDeque::from([(root.to_path_buf(), 0, false, PathBuf::new())]);
        while let Some((directory, level, bulk, top)) = pending.pop_front() {
            if !self.time_left() || self.result.observed.entries >= self.budgets.entries {
                if self.result.coverage.stop != ProbeStop::Time {
                    self.result.coverage.stop = ProbeStop::Entries;
                }
                break;
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
                        && (kind.is_dir() || kind.is_file())
                        && validated_git_worktree(&directory, self.start, self.budgets.time)
                    {
                        self.child_groups.insert(group);
                        self.result.child_roots.push(directory.clone());
                    }
                    continue;
                }
                if marker(&name)
                    && (kind.is_file() || (kind.is_dir() && name.ends_with(".xcodeproj")))
                {
                    if level == 0 {
                        self.result.observed.root_markers.push(name.to_string());
                        self.result.classification = FolderClassification::Project;
                        self.result.reason = AdmissionReason::RootMarker;
                        self.result.coverage.stop = ProbeStop::StrongEvidence;
                        return;
                    } else if level <= 2 && !bulk {
                        self.child_groups.insert(group.clone());
                        self.result.child_roots.push(directory.clone());
                    }
                }
                if kind.is_dir() {
                    self.result.observed.directories += 1;
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
fn validated_git_worktree(root: &Path, start: Instant, budget: Duration) -> bool {
    if start.elapsed() >= budget || !crate::developer_tools::git_usable() {
        return false;
    }
    let mut command = crate::git_probe::command();
    command
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return status.success()
                    && child.wait_with_output().is_ok_and(|output| {
                        output.stdout == b"true\n" || output.stdout == b"true\r\n"
                    });
            }
            Ok(None) if start.elapsed() < budget => {
                std::thread::sleep(Duration::from_millis(1));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
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
            ProbeBudgets::default(),
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
        assert!(result.child_roots.contains(&nested));
        assert!(!result.child_roots.contains(&root.path().join("two")));
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
            let result = probe(root.path(), &ProjectDetectionConfig::default(), budgets);
            assert_eq!(result.reason, AdmissionReason::EvidenceBudgetExhausted);
            assert!(!result.coverage.complete);
            assert!(result.observed.entries <= budgets.entries);
            assert!(result.observed.entries <= budgets.per_directory + 1);
        }
        let tree = tempfile::tempdir().unwrap();
        let child = tree.path().join("child");
        std::fs::create_dir(&child).unwrap();
        file(&child, "a.rs");
        file(&child, "b.rs");
        let result = probe(
            tree.path(),
            &ProjectDetectionConfig::default(),
            ProbeBudgets {
                depth: 0,
                ..ProbeBudgets::default()
            },
        );
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
        assert_eq!(result.classification, FolderClassification::NotProject);
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
            super::probe(
                &child,
                &ProjectDetectionConfig::default(),
                ProbeBudgets::default(),
            )
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

    #[test]
    fn million_name_scratch_is_rejected_with_bounded_observations() {
        let root = tempfile::tempdir().unwrap();
        for shard in 0..21 {
            let shard = root.path().join(shard.to_string());
            std::fs::create_dir(&shard).unwrap();
            for run in 0..100 {
                let leaf = shard.join(run.to_string());
                std::fs::create_dir(&leaf).unwrap();
                file(&leaf, "seed.dat");
                for name in 1..1000 {
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
}
