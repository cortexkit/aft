//! Admission state belongs to the bound root, ahead of every index plane.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::Watcher;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::context::AppContext;
use crate::folder_admission::{
    FolderAdmission, FolderClassification, ProbeBudgets, POLICY_VERSION,
};
use crate::protocol::{RawRequest, Response};

pub const OFF_TEXT: &str = "Indexing is off: this folder does not look like a project. grep and glob still work, with limits. To index it, set project_detection in your AFT user config.";
const RECHECK_MS: u64 = 60_000;
const CACHE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) struct State {
    binding: parking_lot::Mutex<Option<Binding>>,
    dirty: Arc<AtomicBool>,
    watcher: parking_lot::Mutex<Option<notify::RecommendedWatcher>>,
    allowed: Arc<AtomicBool>,
    workspace_allowed: Arc<AtomicBool>,
    bound: AtomicBool,
    probe: parking_lot::Mutex<Option<ProbeTask>>,
    refresh_watch: AtomicBool,
    #[cfg(test)]
    probes: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    cache_hits: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    probe_gate:
        parking_lot::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            binding: Default::default(),
            dirty: Default::default(),
            watcher: Default::default(),
            allowed: Arc::new(AtomicBool::new(true)),
            workspace_allowed: Arc::new(AtomicBool::new(true)),
            bound: AtomicBool::new(false),
            probe: Default::default(),
            refresh_watch: AtomicBool::new(false),
            #[cfg(test)]
            probes: Default::default(),
            #[cfg(test)]
            cache_hits: Default::default(),
            #[cfg(test)]
            probe_gate: Default::default(),
        }
    }
}

impl State {
    pub(crate) fn work_gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.allowed)
    }
    pub(crate) fn close(&self) {
        self.allowed.store(false, Ordering::Release);
        self.workspace_allowed.store(false, Ordering::Release);
    }
    pub(crate) fn workspace_gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.workspace_allowed)
    }
    pub(crate) fn allow_workspace(&self) {
        self.workspace_allowed.store(true, Ordering::Release);
    }
}

struct Binding {
    root: PathBuf,
    requested: Config,
    request: RawRequest,
    generation: u64,
    decision: Option<Cache>,
    pending: bool,
    checking: bool,
    invalidated: bool,
    needs_apply: bool,
    needs_off_bind: bool,
    announced: std::collections::HashSet<String>,
    configure_announced: std::collections::HashSet<String>,
    waiting_sessions: std::collections::HashSet<String>,
    force: bool,
    next_probe_ms: u64,
    wait_deadline: Instant,
    mode: crate::folder_admission::DetectionMode,
    fresh: bool,
    deadline_exhausted: bool,
}

struct ProbeTask {
    generation: u64,
    receiver: std::sync::mpsc::Receiver<ProbeResult>,
}

enum ProbeResult {
    Complete(Cache, bool),
    Wait(u64),
    Exhausted,
}

pub fn wait_budget() -> Duration {
    ProbeBudgets::default().time + Duration::from_millis(250)
}

pub fn waiting(ctx: &AppContext) -> bool {
    reason(ctx).as_deref() == Some("folder_admission_pending")
}

pub fn request_waits(request: &RawRequest, ctx: &AppContext) -> bool {
    waiting(ctx)
        && !matches!(
            request.command.as_str(),
            "configure"
                | "ping"
                | "status"
                | "health"
                | "shutdown"
                | "cancel_request"
                | "get_capabilities"
        )
}

/// Poll before taking a request's executor lease or configuration snapshot.
/// Expiry is a conservative, incomplete decision, never a pending reply.
pub fn poll_wait(ctx: &AppContext) -> bool {
    if !waiting(ctx) {
        return true;
    }
    let _ = resolve(ctx);
    if !waiting(ctx) {
        return true;
    }
    let mut probe = ctx.folder_admission().probe.lock();
    let mut binding = ctx.folder_admission().binding.lock();
    if let Some(binding) = binding
        .as_mut()
        .filter(|b| Instant::now() >= b.wait_deadline)
    {
        binding.pending = true;
        binding.checking = false;
        binding.needs_apply = false;
        binding.decision = None;
        *probe = None;
    }
    drop(binding);
    drop(probe);
    let _ = resolve(ctx);
    !waiting(ctx)
}

pub async fn wait(ctx: &AppContext) {
    while !poll_wait(ctx) {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    path: PathBuf,
    facts: Option<(u64, u128, u128, u64, u64)>,
}

impl Stamp {
    fn read(path: PathBuf) -> Self {
        let facts = std::fs::symlink_metadata(&path).ok().map(|m| {
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                (m.dev(), m.ino())
            };
            #[cfg(not(unix))]
            let identity = (0, 0);
            (
                m.len(),
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |t| t.as_nanos()),
                m.created()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |t| t.as_nanos()),
                identity.0,
                identity.1,
            )
        });
        Self { path, facts }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    relative_paths: bool,
    policy: String,
    checked_ms: u64,
    stamps: Vec<Stamp>,
    decision: FolderAdmission,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn policy(config: &Config, root: &Path) -> String {
    mode_policy(config.project_detection.mode_for_root(root))
}

fn mode_policy(mode: crate::folder_admission::DetectionMode) -> String {
    blake3::hash(&serde_json::to_vec(&(POLICY_VERSION, mode)).unwrap_or_default())
        .to_hex()
        .to_string()
}

fn timeout_cache(
    root: &Path,
    _config: &Config,
    mode: crate::folder_admission::DetectionMode,
) -> Cache {
    Cache {
        relative_paths: false,
        policy: mode_policy(mode),
        checked_ms: now_ms(),
        stamps: Vec::new(),
        decision: crate::folder_admission::exhausted(root, mode, ProbeBudgets::default()),
    }
}

fn cache_path(config: &Config, root: &Path) -> PathBuf {
    crate::bash_background::storage_dir(config.storage_dir.as_deref())
        .join("folder-admission")
        .join(format!(
            "{}.json",
            crate::path_identity::project_scope_key(root)
        ))
}

fn stamps(decision: &FolderAdmission) -> Vec<Stamp> {
    let Some(root) = decision.canonical_root.as_ref() else {
        return Vec::new();
    };
    let mut paths = vec![root.clone()];
    for name in [".git", ".git/HEAD", ".git/index", ".git/config"] {
        paths.push(root.join(name));
    }
    for marker in &decision.observed.root_markers {
        paths.push(root.join(marker));
    }
    paths.extend(decision.evidence_markers.iter().cloned());
    paths.extend(decision.git_controls.iter().cloned());
    if decision.reason == crate::folder_admission::AdmissionReason::SmallSourceTree {
        paths.extend(decision.observed_directories.iter().cloned());
    }
    for child in &decision.immediate_directories {
        paths.push(child.clone());
        paths.push(child.join(".git"));
    }
    paths.into_iter().map(Stamp::read).collect()
}

fn valid(cache: &Cache, config: &Config, root: &Path) -> bool {
    if cache.decision.classification == FolderClassification::Project
        && std::fs::read_dir(root).is_err()
    {
        return false;
    }
    cache.decision.policy_version == POLICY_VERSION
        && cache
            .decision
            .canonical_root
            .as_deref()
            .is_some_and(|saved| same_root(saved, root))
        && cache.policy == policy(config, root)
        && cache.decision.override_mode == config.project_detection.mode_for_root(root)
        && !cache.stamps.is_empty()
        && (cache.decision.reason != crate::folder_admission::AdmissionReason::GitCheckout
            || cache.decision.git_controls.first().is_some_and(|head| {
                cache
                    .stamps
                    .iter()
                    .any(|stamp| stamp.path == *head && stamp.facts.is_some())
            }))
        && cache
            .stamps
            .iter()
            .all(|stamp| Stamp::read(stamp.path.clone()) == *stamp)
}

fn intent_valid(cache: &Cache, config: &Config, root: &Path) -> bool {
    if cache.decision.classification == FolderClassification::Project
        && std::fs::read_dir(root).is_err()
    {
        return false;
    }
    if valid(cache, config, root) {
        return true;
    }
    if cache.decision.policy_version != POLICY_VERSION
        || cache.policy != policy(config, root)
        || !cache
            .decision
            .canonical_root
            .as_deref()
            .is_some_and(|saved| same_root(saved, root))
    {
        return false;
    }
    let mode = config.project_detection.mode_for_root(root);
    if mode != cache.decision.override_mode {
        return false;
    }
    let same_identity = cache.stamps.first().is_some_and(|before| {
        before
            .facts
            .zip(Stamp::read(before.path.clone()).facts)
            .is_some_and(|(old, new)| (old.2, old.3, old.4) == (new.2, new.3, new.4))
    });
    if !same_identity {
        return false;
    }
    match mode {
        crate::folder_admission::DetectionMode::Project => {
            return cache.decision.classification == FolderClassification::Project
        }
        crate::folder_admission::DetectionMode::Off => {
            return cache.decision.classification == FolderClassification::NotProject
        }
        crate::folder_admission::DetectionMode::Workspace => {
            return cache.decision.classification == FolderClassification::Workspace
        }
        crate::folder_admission::DetectionMode::Auto => {}
    }
    if crate::folder_admission::PERSONAL
        .iter()
        .filter(|name| std::fs::symlink_metadata(root.join(name)).is_ok_and(|m| m.is_dir()))
        .count()
        >= 3
    {
        return false;
    }
    if cache.decision.classification == FolderClassification::Workspace {
        let root_unchanged = cache
            .stamps
            .first()
            .is_some_and(|stamp| Stamp::read(stamp.path.clone()) == *stamp);
        if !root_unchanged
            || crate::folder_admission::MARKERS
                .iter()
                .any(|name| std::fs::symlink_metadata(root.join(name)).is_ok_and(|m| m.is_file()))
        {
            return false;
        }
        let mut groups = std::collections::BTreeSet::new();
        for marker in &cache.decision.evidence_markers {
            if std::fs::symlink_metadata(marker).is_ok_and(|m| {
                m.is_file()
                    || (marker.file_name().is_some_and(|name| name == ".git") && m.is_dir())
                    || (marker.extension().is_some_and(|ext| ext == "xcodeproj") && m.is_dir())
            }) {
                if let Some(first) = marker
                    .strip_prefix(root)
                    .ok()
                    .and_then(|relative| relative.components().next())
                {
                    groups.insert(first.as_os_str().to_os_string());
                }
            }
        }
        return groups.len() >= 2
            && groups.len() * 2 >= cache.decision.observed.non_bulk_root_directories;
    }
    if cache.decision.classification != FolderClassification::Project {
        return false;
    }
    let marker = crate::folder_admission::MARKERS
        .iter()
        .any(|name| std::fs::symlink_metadata(root.join(name)).is_ok_and(|m| m.is_file()))
        || cache.decision.observed.root_markers.iter().any(|name| {
            std::fs::symlink_metadata(root.join(name))
                .is_ok_and(|m| m.is_file() || (name.ends_with(".xcodeproj") && m.is_dir()))
        });
    if marker {
        return true;
    }
    if cache.decision.reason != crate::folder_admission::AdmissionReason::GitCheckout {
        return false;
    }
    // Index and object-directory mtimes change on ordinary Git writes. They do
    // not revoke project intent; retain the validated control directory while
    // requiring its locator/configuration and root identity to remain coherent.
    let head = cache.decision.git_controls.first();
    head.is_some_and(|head| std::fs::symlink_metadata(head).is_ok_and(|m| m.is_file()))
        && cache
            .stamps
            .iter()
            .filter(|stamp| {
                let name = stamp
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                matches!(name, "config" | "commondir" | ".git")
                    || head.is_some_and(|head| head.parent() == Some(stamp.path.as_path()))
            })
            .all(|stamp| {
                if std::fs::symlink_metadata(&stamp.path).is_ok_and(|m| m.is_dir()) {
                    return stamp
                        .facts
                        .zip(Stamp::read(stamp.path.clone()).facts)
                        .is_some_and(|(old, new)| (old.2, old.3, old.4) == (new.2, new.3, new.4));
                }
                Stamp::read(stamp.path.clone()) == *stamp
            })
}

fn same_root(left: &Path, right: &Path) -> bool {
    crate::path_identity::ProjectRootId::from_path(left)
        .ok()
        .zip(crate::path_identity::ProjectRootId::from_path(right).ok())
        .is_some_and(|(left, right)| left == right)
}

fn load_raw(config: &Config, root: &Path) -> Option<Cache> {
    let file = std::fs::File::open(cache_path(config, root)).ok()?;
    let mut bytes = Vec::new();
    file.take(CACHE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > CACHE_BYTES {
        return None;
    }
    let mut cache: Cache = serde_json::from_slice(&bytes).ok()?;
    if cache.stamps.len() > 8192
        || [
            &cache.decision.child_roots,
            &cache.decision.immediate_directories,
            &cache.decision.immediate_entries,
            &cache.decision.evidence_markers,
            &cache.decision.git_controls,
            &cache.decision.outside_paths,
            &cache.decision.observed_directories,
        ]
        .iter()
        .any(|paths| paths.len() > 4096)
    {
        return None;
    }
    if cache.relative_paths {
        let base = cache.decision.canonical_root.clone()?;
        for stamp in &mut cache.stamps {
            if !stamp.path.is_absolute() {
                stamp.path = base.join(&stamp.path);
            }
        }
        for paths in [
            &mut cache.decision.child_roots,
            &mut cache.decision.immediate_directories,
            &mut cache.decision.immediate_entries,
            &mut cache.decision.evidence_markers,
            &mut cache.decision.git_controls,
            &mut cache.decision.outside_paths,
            &mut cache.decision.observed_directories,
        ] {
            for path in paths {
                if !path.is_absolute() {
                    *path = base.join(&*path);
                }
            }
        }
        cache.relative_paths = false;
    }
    Some(cache)
}

fn persist(config: &Config, root: &Path, cache: &Cache) {
    let mut packed = cache.clone();
    if let Some(base) = &cache.decision.canonical_root {
        for stamp in &mut packed.stamps {
            if let Ok(relative) = stamp.path.strip_prefix(base) {
                stamp.path = relative.to_path_buf();
            }
        }
        for paths in [
            &mut packed.decision.child_roots,
            &mut packed.decision.immediate_directories,
            &mut packed.decision.immediate_entries,
            &mut packed.decision.evidence_markers,
            &mut packed.decision.git_controls,
            &mut packed.decision.outside_paths,
            &mut packed.decision.observed_directories,
        ] {
            for path in paths {
                if let Ok(relative) = path.strip_prefix(base) {
                    *path = relative.to_path_buf();
                }
            }
        }
        packed.relative_paths = true;
    }
    let path = cache_path(config, root);
    if let (Some(parent), Ok(bytes)) = (path.parent(), serde_json::to_vec(&packed)) {
        if bytes.len() as u64 <= CACHE_BYTES
            && crate::private_storage::create_dir_all(parent).is_ok()
        {
            let _ = crate::durability::atomic_write(
                &path,
                &bytes,
                crate::durability::AtomicWriteDurability::None,
            );
        }
    }
}

fn root_probe_lock(root: &Path) -> Arc<parking_lot::Mutex<()>> {
    static LOCKS: OnceLock<
        parking_lot::Mutex<std::collections::HashMap<String, Weak<parking_lot::Mutex<()>>>>,
    > = OnceLock::new();
    let key = crate::path_identity::project_scope_key(root);
    let mut locks = LOCKS.get_or_init(Default::default).lock();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(parking_lot::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

pub(crate) fn admit_borrowed(
    root: &Path,
    storage: &Path,
    detection: &crate::folder_admission::ProjectDetectionConfig,
) -> Result<(), String> {
    let config = Config {
        storage_dir: Some(storage.to_path_buf()),
        project_detection: detection.clone(),
        ..Config::default()
    };
    let lock = root_probe_lock(root);
    let _probe = lock.lock();
    let raw = load_raw(&config, root);
    let mut opened = raw.as_ref().is_some_and(|cache| {
        cache
            .decision
            .canonical_root
            .as_deref()
            .is_some_and(|saved| same_root(saved, root))
            && cache.stamps.first().is_some_and(|stamp| {
                stamp
                    .facts
                    .zip(Stamp::read(stamp.path.clone()).facts)
                    .is_some_and(|(old, new)| (old.2, old.3, old.4) == (new.2, new.3, new.4))
            })
    }) || crate::root_identity::read(root, storage)
        .ok()
        .flatten()
        .is_some();
    if !opened {
        // Previously saved text-search indexes may lack admission records. Their presence is
        // historical indexing evidence, not current admission: validate the
        // child below before reading any artifact bytes.
        opened = crate::search_index::artifact_cache_key_read_only(root, storage)
            .ok()
            .is_some_and(|key| {
                crate::search_index::resolve_cache_dir_with_key(&key, Some(storage))
                    .join("cache.bin")
                    .is_file()
            });
    }
    if !opened {
        return Err("not_opened".into());
    }
    match detection.mode_for_root(root) {
        crate::folder_admission::DetectionMode::Off
        | crate::folder_admission::DetectionMode::Workspace => return Err("user_override".into()),
        crate::folder_admission::DetectionMode::Project => return Ok(()),
        crate::folder_admission::DetectionMode::Auto => {}
    }
    let cache = if let Some(cache) = raw.filter(|cache| {
        cache.decision.reason != crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted
            && intent_valid(cache, &config, root)
            && (cache.decision.classification == FolderClassification::Project
                || now_ms().saturating_sub(cache.checked_ms) < RECHECK_MS)
    }) {
        cache
    } else {
        if load_raw(&config, root).as_ref().is_some_and(|cache| {
            cache.decision.reason
                != crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted
                && now_ms().saturating_sub(cache.checked_ms) < RECHECK_MS
        }) {
            return Err("folder_admission_pending".into());
        }
        let decision = crate::folder_admission::probe(root, detection, ProbeBudgets::default());
        let cache = Cache {
            relative_paths: false,
            policy: policy(&config, root),
            checked_ms: now_ms(),
            stamps: stamps(&decision),
            decision,
        };
        persist(&config, root, &cache);
        cache
    };
    if cache.decision.classification == FolderClassification::Project {
        Ok(())
    } else {
        Err(serde_json::to_value(cache.decision.reason)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned())
    }
}

/// CLI indexing and the user's `index.roots` background entries have no
/// configure acknowledgement, but share the
/// same persisted, rate-limited evidence before opening any index artifacts.
pub fn standalone(root: &Path, config: &Config) -> Result<FolderAdmission, String> {
    let lock = root_probe_lock(root);
    let _probe = lock.lock();
    if let Some(cache) = load_raw(config, root).filter(|cache| {
        cache.decision.reason != crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted
    }) {
        if intent_valid(&cache, config, root)
            && (cache.decision.classification == FolderClassification::Project
                || now_ms().saturating_sub(cache.checked_ms) < RECHECK_MS)
        {
            return Ok(cache.decision);
        }
        if cache.policy == policy(config, root)
            && now_ms().saturating_sub(cache.checked_ms) < RECHECK_MS
        {
            return Err("folder_admission_pending".into());
        }
    }
    let mut decision =
        crate::folder_admission::probe(root, &config.project_detection, ProbeBudgets::default());
    let stamps = stamps(&decision);
    if decision.reason != crate::folder_admission::AdmissionReason::SmallSourceTree {
        decision.observed_directories.clear();
    }
    let cache = Cache {
        relative_paths: false,
        policy: policy(config, root),
        checked_ms: now_ms(),
        stamps,
        decision: decision.clone(),
    };
    persist(config, root, &cache);
    Ok(decision)
}

pub(crate) fn install_standalone(ctx: &AppContext, config: Config, decision: FolderAdmission) {
    let root = ctx
        .canonical_cache_root_opt()
        .or_else(|| config.project_root.clone())
        .unwrap_or_default();
    let cache = load_raw(&config, &root)
        .filter(|cache| {
            cache.decision.classification == decision.classification
                && cache.decision.reason == decision.reason
        })
        .unwrap_or_else(|| Cache {
            relative_paths: false,
            policy: policy(&config, &root),
            checked_ms: now_ms(),
            stamps: stamps(&decision),
            decision,
        });
    install_bound_cache(ctx, config, root, cache);
}

/// Install an already probed fixture decision without consulting persisted metadata.
/// Index-lifecycle fixtures can replace a checkout at the same path; admission-cache
/// reuse is exercised by the separate folder-admission lifecycle tests.
#[cfg(test)]
pub(crate) fn install_probe_for_test(
    ctx: &AppContext,
    root: &Path,
    config: Config,
    decision: FolderAdmission,
) {
    let cache = Cache {
        relative_paths: false,
        policy: policy(&config, root),
        checked_ms: now_ms(),
        stamps: stamps(&decision),
        decision,
    };
    install_bound_cache(ctx, config, root.to_path_buf(), cache);
}

fn install_bound_cache(ctx: &AppContext, config: Config, root: PathBuf, cache: Cache) {
    let mode = cache.decision.override_mode;
    let request: RawRequest = serde_json::from_value(json!({"id": "standing-folder", "command": "configure", "project_root": root, "harness": "runner"})).expect("standing request");
    let project = cache.decision.classification == FolderClassification::Project;
    let state = ctx.folder_admission();
    state.bound.store(true, Ordering::Release);
    state.allowed.store(project, Ordering::Release);
    state.workspace_allowed.store(false, Ordering::Release);
    ctx.set_heavy_root_work_allowed(project && ctx.cache_unavailable_reason().is_none());
    *state.binding.lock() = Some(Binding {
        root,
        requested: config,
        request,
        generation: ctx.configure_generation(),
        decision: Some(cache),
        pending: false,
        checking: false,
        invalidated: false,
        needs_apply: false,
        needs_off_bind: false,
        announced: Default::default(),
        configure_announced: Default::default(),
        waiting_sessions: Default::default(),
        force: false,
        next_probe_ms: 0,
        wait_deadline: Instant::now() + wait_budget(),
        mode,
        fresh: false,
        deadline_exhausted: false,
    });
    if !project {
        retire_index_planes(ctx);
        ctx.add_degraded_reason("folder_not_admitted");
    }
}

fn snapshot_binding(binding: &Binding) -> Value {
    let mut value = binding
        .decision
        .as_ref()
        .map(|cache| serde_json::to_value(&cache.decision).unwrap_or(Value::Null))
        .unwrap_or_else(|| {
            json!({"policy_version": POLICY_VERSION, "canonical_root": binding.root,
            "classification": null, "reason": null, "observed": null, "coverage": null})
        });
    if let Some(object) = value.as_object_mut() {
        for field in [
            "child_roots",
            "immediate_directories",
            "immediate_entries",
            "evidence_markers",
            "git_controls",
            "outside_paths",
            "observed_directories",
        ] {
            object.remove(field);
        }
    }
    value["state"] =
        json!(
            if binding.pending || binding.needs_apply || binding.force || binding.invalidated {
                "pending"
            } else {
                "decided"
            }
        );
    value["rechecking"] = json!(binding.checking);
    value["observed_at_ms"] = binding
        .decision
        .as_ref()
        .map_or(Value::Null, |cache| json!(cache.checked_ms));
    value["observation_basis"] = json!("probe_snapshot");
    if binding.pending
        || binding.needs_apply
        || binding
            .decision
            .as_ref()
            .is_none_or(|cache| cache.decision.classification != FolderClassification::Project)
    {
        value["indexes"] = json!({"text": "off", "semantic": "off", "call_graph": "off", "background_symbols": "off"});
        value["fallback"] = json!({"grep": "bounded_walk", "glob": "bounded_walk"});
        if !binding.pending && !binding.needs_apply {
            value["recursive_watcher"] = json!("off");
        }
    }
    if binding
        .decision
        .as_ref()
        .is_some_and(|cache| cache.decision.classification == FolderClassification::NotProject)
    {
        value["message"] = json!(OFF_TEXT);
    }
    value
}

pub(crate) fn snapshot(ctx: &AppContext) -> Option<Value> {
    ctx.folder_admission()
        .binding
        .lock()
        .as_ref()
        .map(snapshot_binding)
}

pub(crate) fn try_snapshot(ctx: &AppContext) -> Option<Value> {
    ctx.folder_admission()
        .binding
        .try_lock()?
        .as_ref()
        .map(snapshot_binding)
}

fn reply_snapshot(ctx: &AppContext) -> Option<Value> {
    let mut value = snapshot(ctx)?;
    value.as_object_mut()?.remove("message");
    Some(value)
}

pub(crate) fn blocked(ctx: &AppContext) -> bool {
    let state = ctx.folder_admission();
    state.bound.load(Ordering::Acquire) && !state.allowed.load(Ordering::Acquire)
}

pub(crate) fn reason(ctx: &AppContext) -> Option<String> {
    if !blocked(ctx) {
        return None;
    }
    let Some(binding) = ctx.folder_admission().binding.try_lock() else {
        return Some("folder_admission_pending".into());
    };
    let binding = binding.as_ref()?;
    if binding.pending || binding.needs_apply || binding.force || binding.invalidated {
        return Some("folder_admission_pending".into());
    }
    Some(
        binding
            .decision
            .as_ref()
            .filter(|cache| cache.decision.classification != FolderClassification::Project)
            .map(|cache| {
                serde_json::to_value(cache.decision.reason)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .unwrap_or_else(|| "folder_admission_pending".into()),
    )
}

/// No directory enumeration, Git command, or index artifact read runs here.
pub(crate) fn prepare(
    ctx: &AppContext,
    root: &Path,
    config: &Config,
    force: bool,
    replay: bool,
) -> Option<FolderAdmission> {
    if force {
        return None;
    }
    let cache = {
        let slot = ctx.folder_admission().binding.lock();
        let binding = slot.as_ref()?;
        if binding.root != root
            || policy(&binding.requested, root) != policy(config, root)
            || binding.pending
        {
            return None;
        }
        binding.decision.clone()?
    };
    if cache.decision.reason == crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted {
        return None;
    }
    if !replay
        && (!intent_valid(&cache, config, root)
            || (cache.decision.classification == FolderClassification::NotProject
                && now_ms().saturating_sub(cache.checked_ms) >= RECHECK_MS))
    {
        return None;
    }
    Some(cache.decision)
}

pub(crate) fn commit(
    ctx: &AppContext,
    root: &Path,
    requested: Config,
    request: &RawRequest,
    force: bool,
    pending: bool,
) {
    let state = ctx.folder_admission();
    let mode = requested.project_detection.mode_for_root(root);
    let mut active_probe = pending.then(|| state.probe.lock());
    if let Some(probe) = active_probe.as_mut() {
        **probe = None;
    }
    let mut slot = state.binding.lock();
    state.bound.store(true, Ordering::Release);
    if let Some(binding) = slot
        .as_mut()
        .filter(|b| b.root == root && policy(&b.requested, root) == policy(&requested, root))
    {
        if !pending && binding.deadline_exhausted {
            return;
        }
        binding.requested = requested;
        binding.request = request.clone();
        binding.generation = ctx.configure_generation();
        binding.needs_off_bind = false;
        binding.pending = pending;
        binding.checking = false;
        binding.invalidated = false;
        if force {
            binding.pending = true;
            binding.force = true;
            binding.announced.clear();
            binding.configure_announced.clear();
        }
        binding.needs_apply = binding.pending;
        binding.mode = mode;
        if pending {
            binding.wait_deadline = Instant::now() + wait_budget();
            binding.fresh = true;
            binding.deadline_exhausted = false;
        }
        if pending {
            binding
                .waiting_sessions
                .insert(request.session().to_owned());
        }
        state.allowed.store(
            !binding.pending
                && binding
                    .decision
                    .as_ref()
                    .is_some_and(|c| c.decision.classification == FolderClassification::Project),
            Ordering::Release,
        );
        state.workspace_allowed.store(
            !binding.pending
                && ctx.config().views.enabled
                && ctx.cache_unavailable_reason().is_none()
                && binding
                    .decision
                    .as_ref()
                    .is_some_and(|c| c.decision.classification == FolderClassification::Workspace),
            Ordering::Release,
        );
        return;
    }
    state.dirty.store(false, Ordering::Release);
    state.close();
    *slot = Some(Binding {
        root: root.to_path_buf(),
        requested,
        request: request.clone(),
        generation: ctx.configure_generation(),
        decision: None,
        pending: true,
        checking: false,
        invalidated: false,
        needs_apply: true,
        needs_off_bind: false,
        announced: Default::default(),
        configure_announced: Default::default(),
        waiting_sessions: std::collections::HashSet::from([request.session().to_owned()]),
        force,
        next_probe_ms: 0,
        wait_deadline: Instant::now() + wait_budget(),
        mode,
        fresh: true,
        deadline_exhausted: false,
    });
}

pub(crate) fn pending(ctx: &AppContext) -> bool {
    ctx.folder_admission()
        .binding
        .lock()
        .as_ref()
        .is_some_and(|b| b.pending || b.checking)
}

pub(crate) fn requested(ctx: &AppContext) -> Option<Config> {
    let wanted = ctx
        .folder_admission()
        .binding
        .lock()
        .as_ref()
        .map(|b| b.requested.clone())?;
    let mut current = (*ctx.config()).clone();
    current.indexes = wanted.indexes;
    current.views = wanted.views;
    current.disabled_lsp = wanted.disabled_lsp;
    current.lsp_servers = wanted.lsp_servers;
    current.project_detection = wanted.project_detection;
    Some(current)
}

pub(crate) fn recheck_due(ctx: &AppContext) -> bool {
    let state = ctx.folder_admission();
    if !state.dirty.load(Ordering::Acquire) {
        return false;
    }
    if ctx.subc_unbound_quiesced() {
        return false;
    }
    state.binding.try_lock().is_some_and(|slot| {
        slot.as_ref().is_some_and(|b| {
            !b.pending
                && !b.checking
                && !b.needs_apply
                && (b.force
                    || b.decision
                        .as_ref()
                        .is_none_or(|c| now_ms().saturating_sub(c.checked_ms) >= RECHECK_MS))
        })
    })
}

pub(crate) fn recheck(ctx: &AppContext, force: bool) {
    let state = ctx.folder_admission();
    if let Some(binding) = state.binding.lock().as_mut() {
        binding.force |= force;
    }
    state.dirty.store(true, Ordering::Release);
}

pub(crate) fn arm_recheck(ctx: &AppContext) -> Option<RawRequest> {
    if !recheck_due(ctx) {
        return None;
    }
    let state = ctx.folder_admission();
    let mut binding = state.binding.lock();
    let binding = binding.as_mut()?;
    let keep_serving = !binding.force
        && binding
            .decision
            .as_ref()
            .is_some_and(|cache| intent_valid(cache, &binding.requested, &binding.root))
        && (state.allowed.load(Ordering::Acquire)
            || state.workspace_allowed.load(Ordering::Acquire));
    binding.pending = !keep_serving;
    binding.fresh = false;
    binding.wait_deadline = Instant::now() + wait_budget();
    binding.checking = keep_serving;
    binding.needs_apply = !keep_serving;
    binding.needs_off_bind = !keep_serving;
    binding.announced.clear();
    binding.configure_announced.clear();
    binding
        .waiting_sessions
        .insert(binding.request.session().to_owned());
    if !keep_serving {
        state.close();
    }
    let request = binding.request.clone();
    state.dirty.store(false, Ordering::Release);
    if !keep_serving {
        ctx.set_heavy_root_work_allowed(false);
        binding.generation = ctx
            .configure_generation_flag()
            .fetch_add(1, Ordering::SeqCst)
            + 1;
    }
    Some(request)
}

pub(crate) enum ResolveResult {
    Ready(u64),
    Settled,
    Wait,
    Stale,
}

fn probe_cache(
    root: &Path,
    config: &Config,
    force: bool,
    checking: bool,
    fresh: bool,
) -> ProbeResult {
    let lock = root_probe_lock(root);
    let Some(_guard) = lock.try_lock_for(ProbeBudgets::default().time) else {
        return ProbeResult::Exhausted;
    };
    let raw = (!force)
        .then(|| load_raw(config, root))
        .flatten()
        .filter(|cache| {
            cache.decision.reason
                != crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted
        });
    if !fresh {
        if let Some(cache) = raw.as_ref().filter(|cache| {
            cache.decision.policy_version == POLICY_VERSION
                && cache.policy == policy(config, root)
                && !intent_valid(cache, config, root)
                && now_ms().saturating_sub(cache.checked_ms) < RECHECK_MS
        }) {
            return ProbeResult::Wait(cache.checked_ms.saturating_add(RECHECK_MS));
        }
    }
    if let Some(cache) = raw
        .filter(|cache| !checking && intent_valid(cache, config, root))
        .filter(|cache| {
            cache.decision.classification != FolderClassification::NotProject
                || now_ms().saturating_sub(cache.checked_ms) < RECHECK_MS
        })
    {
        return ProbeResult::Complete(cache, true);
    }
    let mut decision =
        crate::folder_admission::probe(root, &config.project_detection, ProbeBudgets::default());
    let stamps = stamps(&decision);
    if decision.reason != crate::folder_admission::AdmissionReason::SmallSourceTree {
        decision.observed_directories.clear();
    }
    let cache = Cache {
        policy: policy(config, root),
        checked_ms: now_ms(),
        stamps,
        decision,
        relative_paths: false,
    };
    persist(config, root, &cache);
    ProbeResult::Complete(cache, false)
}

/// Poll the metadata worker after configure acknowledged the selected root.
/// Requests wait outside executor/config leases; only the current generation
/// may publish a result, and an overdue attempt becomes an incomplete decision.
pub(crate) fn resolve(ctx: &AppContext) -> ResolveResult {
    let state = ctx.folder_admission();
    let input = state
        .binding
        .lock()
        .as_ref()
        .filter(|b| (b.pending || b.checking) && !b.needs_off_bind)
        .map(|b| {
            (
                b.root.clone(),
                b.requested.clone(),
                b.generation,
                b.force
                    || b.decision.as_ref().is_some_and(|cache| {
                        cache.decision.reason
                            == crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted
                    }),
                b.checking,
                b.fresh || b.force,
                b.wait_deadline,
                b.mode,
            )
        });
    let Some((root, config, generation, force, checking, fresh, deadline, mode)) = input else {
        return state
            .binding
            .lock()
            .as_ref()
            .filter(|b| b.needs_off_bind || b.needs_apply)
            .map_or(ResolveResult::Settled, |b| {
                ResolveResult::Ready(b.generation)
            });
    };
    let mut task = state.probe.lock();
    if let Some(binding) = state.binding.lock().as_ref() {
        if binding.generation != generation {
            return ResolveResult::Stale;
        }
        if !binding.pending && !binding.checking {
            return if binding.needs_apply {
                ResolveResult::Ready(binding.generation)
            } else {
                ResolveResult::Settled
            };
        }
    }
    if task
        .as_ref()
        .is_some_and(|task| task.generation != generation)
    {
        *task = None;
    }
    if task.is_none() && Instant::now() < deadline {
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        let worker_config = config.clone();
        #[cfg(test)]
        let gate = state.probe_gate.lock().take();
        #[cfg(test)]
        let child_env = crate::git_probe::capture_child_env_for_test();
        #[cfg(test)]
        let counts = (Arc::clone(&state.probes), Arc::clone(&state.cache_hits));
        let _ = std::thread::Builder::new()
            .name("aft-folder-probe".into())
            .spawn(move || {
                #[cfg(debug_assertions)]
                delay_probe_for_test();
                if Instant::now() >= deadline {
                    let _ = sender.send(ProbeResult::Exhausted);
                    return;
                }
                #[cfg(test)]
                if let Some((started, release)) = gate {
                    let _ = started.send(());
                    let _ = release.recv_timeout(Duration::from_secs(10));
                }
                if Instant::now() >= deadline {
                    let _ = sender.send(ProbeResult::Exhausted);
                    return;
                }
                let work = || probe_cache(&worker_root, &worker_config, force, checking, fresh);
                #[cfg(test)]
                let result = crate::git_probe::with_captured_child_env_for_test(child_env, work);
                #[cfg(not(test))]
                let result = work();
                #[cfg(test)]
                match &result {
                    ProbeResult::Complete(_, true) => {
                        counts.1.fetch_add(1, Ordering::SeqCst);
                    }
                    ProbeResult::Complete(_, false) => {
                        counts.0.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
                let result = if Instant::now() > deadline {
                    ProbeResult::Exhausted
                } else {
                    result
                };
                let _ = sender.send(result);
            });
        *task = Some(ProbeTask {
            generation,
            receiver,
        });
    }
    let result = task.as_ref().and_then(|task| task.receiver.try_recv().ok());
    let mut cache = match result {
        Some(ProbeResult::Complete(cache, cached)) => {
            let _ = cached;
            cache
        }
        Some(ProbeResult::Wait(until)) => {
            *task = None;
            if let Some(binding) = state
                .binding
                .lock()
                .as_mut()
                .filter(|b| b.generation == generation)
            {
                binding.next_probe_ms = until;
            }
            return ResolveResult::Wait;
        }
        Some(ProbeResult::Exhausted) => timeout_cache(&root, &config, mode),
        None if Instant::now() < deadline => return ResolveResult::Wait,
        None => timeout_cache(&root, &config, mode),
    };
    if cache.decision.reason == crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted {
        cache.decision.coverage.elapsed_ms = Instant::now()
            .saturating_duration_since(deadline - wait_budget())
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
    }
    let mut slot = state.binding.lock();
    let Some(binding) = slot.as_mut().filter(|b| {
        b.generation == generation
            && b.root == root
            && policy(&b.requested, &root) == policy(&config, &root)
    }) else {
        return ResolveResult::Stale;
    };
    if ctx.subc_unbound_quiesced() || ctx.configure_generation() != generation {
        return ResolveResult::Stale;
    }
    let negative = cache.decision.classification == FolderClassification::NotProject;
    let unchanged_project = checking
        && cache.decision.classification == FolderClassification::Project
        && binding
            .decision
            .as_ref()
            .is_some_and(|old| old.decision.classification == FolderClassification::Project);
    binding.pending = false;
    binding.checking = false;
    binding.invalidated = false;
    binding.needs_apply = !negative && !unchanged_project;
    binding.force = false;
    binding.decision = Some(cache.clone());
    binding.deadline_exhausted =
        cache.decision.reason == crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted;
    let was_serving =
        state.allowed.load(Ordering::Acquire) || state.workspace_allowed.load(Ordering::Acquire);
    if negative
        || (checking
            && !unchanged_project
            && cache.decision.classification == FolderClassification::Project)
    {
        state.close();
        ctx.set_heavy_root_work_allowed(false);
        if was_serving {
            binding.generation = ctx
                .configure_generation_flag()
                .fetch_add(1, Ordering::SeqCst)
                + 1;
        }
    }
    let apply_generation = binding.generation;
    let sessions = if negative {
        binding.waiting_sessions.drain().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    state.dirty.store(false, Ordering::Release);
    drop(slot);
    *task = None;
    drop(task);
    if cache.decision.reason == crate::folder_admission::AdmissionReason::EvidenceBudgetExhausted {
        let config = config.clone();
        let root = root.clone();
        let cache = cache.clone();
        let _ = std::thread::Builder::new()
            .name("aft-admission-cache".into())
            .spawn(move || {
                let lock = root_probe_lock(&root);
                let _guard = lock.lock();
                persist(&config, &root, &cache);
            });
    }
    if negative {
        retire_index_planes(ctx);
        if !ctx.replace_degraded_reason("folder_admission_pending", "folder_not_admitted") {
            ctx.add_degraded_reason("folder_not_admitted");
        }
        for session in sessions {
            if let Some(message) = notice(ctx, true, &session) {
                let _ = ctx.configure_warnings_sender().send((ctx.configure_generation(), crate::protocol::ConfigureWarningsFrame {
                    frame_type: "configure_warnings", session_id: Some(session), project_root: root.display().to_string(),
                    warnings: vec![json!({"code": "folder_not_admitted", "message": message, "folder": reply_snapshot(ctx)})],
                }));
            }
        }
    }
    state.refresh_watch.store(true, Ordering::Release);
    if negative || unchanged_project {
        ResolveResult::Settled
    } else {
        ResolveResult::Ready(apply_generation)
    }
}

#[cfg(debug_assertions)]
fn delay_probe_for_test() {
    let delay = std::env::var_os("AFT_TEST_ADMISSION_DELAY_FILE")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .or_else(|| std::env::var("AFT_TEST_ADMISSION_DELAY_MS").ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if let Some(path) = std::env::var_os("AFT_TEST_ADMISSION_STARTED") {
        let _ = std::fs::write(path, b"started");
    }
    std::thread::sleep(Duration::from_millis(delay));
}

pub(crate) fn ensure_metadata_watch(ctx: &AppContext) {
    if std::env::var("AFT_TEST_DISABLE_FILE_WATCHER").is_ok_and(|value| value == "1") {
        return;
    }
    let state = ctx.folder_admission();
    if state.refresh_watch.swap(false, Ordering::AcqRel) {
        drop(state.watcher.lock().take());
    }
    if state.watcher.lock().is_some() || ctx.subc_unbound_quiesced() {
        return;
    }
    let input = state.binding.lock().as_ref().and_then(|binding| {
        binding
            .decision
            .as_ref()
            .map(|cache| (binding.root.clone(), cache.decision.clone()))
    });
    let Some((root, decision)) = input else {
        return;
    };
    // Watch only known directories, never recursively discover watch targets.
    let dirty = Arc::clone(&state.dirty);
    if let Ok(mut watcher) =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok_and(|event| !matches!(event.kind, notify::EventKind::Access(_))) {
                dirty.store(true, Ordering::Release);
            }
        })
    {
        let _ = watcher.watch(&root, notify::RecursiveMode::NonRecursive);
        if root.join(".git").is_dir() {
            let _ = watcher.watch(&root.join(".git"), notify::RecursiveMode::NonRecursive);
        }
        let mut directories = decision.immediate_directories;
        directories.extend(decision.child_roots);
        if let Some(head) = decision.git_controls.first() {
            if let Some(parent) = head.parent() {
                directories.push(parent.to_path_buf());
            }
        }
        directories.sort();
        directories.dedup();
        for child in &directories {
            if std::fs::symlink_metadata(child).is_ok_and(|metadata| metadata.is_dir()) {
                let _ = watcher.watch(child, notify::RecursiveMode::NonRecursive);
            }
        }
        *state.watcher.lock() = Some(watcher);
    }
}

pub(crate) fn quiesce(ctx: &AppContext) {
    drop(ctx.folder_admission().watcher.lock().take());
}

#[cfg(test)]
pub(crate) fn probe_counts_for_test(ctx: &AppContext) -> (usize, usize) {
    (
        ctx.folder_admission().probes.load(Ordering::SeqCst),
        ctx.folder_admission().cache_hits.load(Ordering::SeqCst),
    )
}

#[cfg(test)]
pub(crate) fn gate_probe_for_test(
    ctx: &AppContext,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (started, observed) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    *ctx.folder_admission().probe_gate.lock() = Some((started, wait));
    (observed, release)
}

fn retire_index_planes(ctx: &AppContext) {
    ctx.set_heavy_root_work_allowed(false);
    ctx.set_cache_writer_capabilities(false, false);
    ctx.retire_search_index_rx("folder_not_admitted");
    ctx.retire_semantic_index_rx("folder_not_admitted");
    ctx.retire_callgraph_store_rx();
    ctx.clear_semantic_refresh_worker();
    *ctx.semantic_embedding_model().lock() = None;
    ctx.invalidate_artifacts_after_watcher_gap();
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        crate::context::SemanticIndexStatus::Disabled;
    ctx.set_semantic_build_progress(None);
    ctx.clear_view_runtime();
    ctx.clear_pending_index_updates();
    ctx.stop_watcher_runtime();
    ctx.clear_gitignore();
    ctx.lsp().shutdown_all();
    crate::views::parent::release_context(ctx);
}

pub(crate) fn wait_until_probe(ctx: &AppContext) -> Duration {
    Duration::from_millis(
        ctx.folder_admission()
            .binding
            .lock()
            .as_ref()
            .map_or(1000, |b| b.next_probe_ms.saturating_sub(now_ms()).max(1)),
    )
}

pub fn refusal(ctx: &AppContext, id: &str, session: &str) -> Option<Response> {
    let reason = reason(ctx)?;
    let message = notice(ctx, false, session).unwrap_or(OFF_TEXT);
    Some(Response::error_with_data(
        id,
        "folder_not_admitted",
        message,
        json!({"reason": reason, "folder": reply_snapshot(ctx)}),
    ))
}

fn notice(ctx: &AppContext, configure: bool, session: &str) -> Option<&'static str> {
    let mut slot = ctx.folder_admission().binding.lock();
    let binding = slot.as_mut()?;
    let off = !binding.pending
        && !binding.needs_apply
        && !binding.force
        && binding
            .decision
            .as_ref()
            .is_some_and(|c| c.decision.classification == FolderClassification::NotProject);
    let announced = if configure {
        &mut binding.configure_announced
    } else {
        &mut binding.announced
    };
    if off && announced.insert(session.to_owned()) {
        Some(OFF_TEXT)
    } else {
        None
    }
}

pub fn decorate(ctx: &AppContext, response: &mut Response, configure: bool, session: &str) {
    if !response.data.is_object() {
        return;
    }
    let Some(folder) = reply_snapshot(ctx) else {
        return;
    };
    response.data["folder"] = folder;
    if let Some(message) = notice(ctx, configure, session) {
        response.data["folder_notice"] = json!(message);
        if configure {
            if response.data.get("warnings").is_none() {
                response.data["warnings"] = json!([]);
            }
            let folder = response.data["folder"].clone();
            if let Some(warnings) = response.data["warnings"].as_array_mut() {
                warnings.push(
                    json!({"code": "folder_not_admitted", "message": message, "folder": folder}),
                );
            }
        }
        if let Some(Value::String(text)) = response.data.get_mut("text") {
            text.push_str("\n\n");
            text.push_str(message);
        }
    }
}

pub(crate) fn after_configure(ctx: &AppContext, request: &RawRequest) {
    let mut exhausted = false;
    if let Some(binding) = ctx.folder_admission().binding.lock().as_mut() {
        exhausted = binding.deadline_exhausted;
        binding.request = request.clone();
        binding.generation = ctx.configure_generation();
        if !binding.pending {
            binding.needs_apply = false;
        }
    }
    if exhausted {
        retire_index_planes(ctx);
    }
}

pub(crate) fn take_waiting_sessions(ctx: &AppContext) -> Vec<String> {
    ctx.folder_admission()
        .binding
        .lock()
        .as_mut()
        .map_or_else(Vec::new, |binding| {
            binding.waiting_sessions.drain().collect()
        })
}

pub(crate) fn policy_changed(ctx: &AppContext, config: &Config) {
    if let Some(binding) = ctx.folder_admission().binding.lock().as_mut() {
        binding.requested.project_detection = config.project_detection.clone();
    }
    recheck(ctx, true);
    ctx.folder_admission().close();
    ctx.set_heavy_root_work_allowed(false);
    ctx.configure_generation_flag()
        .fetch_add(1, Ordering::SeqCst);
}

pub fn route_refusal(request: &RawRequest, ctx: &AppContext) -> Option<Response> {
    let command = request.command.as_str();
    let needs_index = matches!(
        command,
        "semantic_search"
            | "call_tree"
            | "callers"
            | "impact"
            | "trace_to"
            | "trace_to_symbol"
            | "trace_data"
            | "inspect"
    ) || command.starts_with("lsp_");
    if !needs_index {
        return None;
    }
    if !command.starts_with("lsp_")
        && ctx
            .canonical_cache_root_opt()
            .is_some_and(|root| crate::views::parent::held_by(ctx, &root))
    {
        return None;
    }
    refusal(ctx, &request.id, request.session())
}

pub fn route_index_command(request: &RawRequest, ctx: &AppContext) -> Option<Response> {
    let _ = workspace_current(ctx);
    crate::views::parent::route(request, ctx).or_else(|| route_refusal(request, ctx))
}

pub(crate) fn workspace_current(ctx: &AppContext) -> bool {
    let state = ctx.folder_admission();
    let input = state.binding.lock().as_ref().and_then(|binding| {
        binding
            .decision
            .as_ref()
            .filter(|cache| cache.decision.classification == FolderClassification::Workspace)
            .map(|cache| {
                (
                    binding.root.clone(),
                    binding.requested.clone(),
                    cache.clone(),
                    binding.pending || binding.needs_apply,
                )
            })
    });
    let Some((root, config, cache, pending)) = input else {
        return true;
    };
    if pending {
        return false;
    }
    let membership = cache
        .stamps
        .first()
        .is_some_and(|stamp| Stamp::read(stamp.path.clone()) == *stamp)
        && cache
            .decision
            .immediate_directories
            .iter()
            .filter(|path| !cache.decision.child_roots.contains(path))
            .all(|path| {
                cache
                    .stamps
                    .iter()
                    .find(|stamp| stamp.path == *path)
                    .is_some_and(|stamp| Stamp::read(stamp.path.clone()) == *stamp)
            });
    if membership && intent_valid(&cache, &config, &root) {
        return true;
    }
    state.workspace_allowed.store(false, Ordering::Release);
    state.dirty.store(true, Ordering::Release);
    if let Some(binding) = state.binding.lock().as_mut() {
        binding.invalidated = true;
    }
    false
}

pub(crate) fn bounded_query(ctx: &AppContext) -> bool {
    if ctx.parent_query_only() {
        return ctx
            .search_index()
            .try_read()
            .ok()
            .is_none_or(|index| index.as_ref().is_none_or(|index| !index.ready));
    }
    !artifacts_admitted(ctx)
}

pub(crate) fn artifacts_admitted(ctx: &AppContext) -> bool {
    let state = ctx.folder_admission();
    if !state.bound.load(Ordering::Acquire) {
        return !ctx.parent_query_only();
    }
    if !state.allowed.load(Ordering::Acquire) {
        return false;
    }
    let input = state.binding.lock().as_ref().and_then(|b| {
        b.decision
            .clone()
            .map(|cache| (b.root.clone(), b.requested.clone(), cache))
    });
    if input.is_some_and(|(root, config, cache)| intent_valid(&cache, &config, &root)) {
        return true;
    }
    state.close();
    if let Some(binding) = state.binding.lock().as_mut() {
        binding.invalidated = true;
    }
    state.dirty.store(true, Ordering::Release);
    ctx.configure_generation_flag()
        .fetch_add(1, Ordering::SeqCst);
    retire_index_planes(ctx);
    false
}
