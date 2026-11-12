//! A completed compiler check, not an idle server, is the authority behind this
//! cache. Disk validation is fail-closed and never installs cached rows into the
//! live diagnostics store: a cold server can publish partial results independently.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::diagnostics::{DiagnosticSeverity, StoredDiagnostic};

mod inputs;
use inputs::{CachedFacts, SourceInputs, UnresolvedInput};

pub(crate) const BUDGET: Duration = Duration::from_secs(2);
// The first completed check has no trusted hash ledger yet. Later requests
// reuse content hashes and parsed facts, then resolve cross-file macro inputs.
const INITIAL_CAPTURE_BUDGET: Duration = Duration::from_secs(15);
// Filesystem timestamps can round a write down to a tick before the wall-clock
// instant at which it actually happened. Two seconds covers coarse timestamp
// resolution as well as sub-second Unix clock ticks. Inputs inside that window
// cannot certify a check's starting state: skip persistence rather than reuse
// diagnostics for bytes the compiler may never have read.
const CHECK_BEGIN_TIMESTAMP_MARGIN: Duration = Duration::from_secs(2);
// Worktree churn must not turn completed compiler checks into an unbounded disk cache.
const MAX_RECORDS: usize = 64;
const RECORD_SCAN_LIMIT: usize = 1024;
// Older records have no proof that skipped sources were checked for the set of
// include-bearing macro names. Rebuild their facts before trusting a check.
const RECORD_SCHEMA: u32 = 4;

fn canonical(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path)
        .ok()
        .map(|path| crate::inspect::job::normalize_path(&path))
}

struct WalkReport<'a> {
    root: &'a Path,
    examined: usize,
    complete: bool,
    last: PathBuf,
    started: Instant,
}
impl Drop for WalkReport<'_> {
    fn drop(&mut self) {
        crate::slog_info!(
            "rust check fingerprint walk root={} examined={} complete={} last={} elapsed_ms={}",
            self.root.display(),
            self.examined,
            self.complete,
            self.last.display(),
            self.started.elapsed().as_millis()
        );
        #[cfg(any(test, feature = "test-hooks"))]
        if !self.complete {
            eprintln!(
                "fingerprint refused at {} after {:?}; examined {}",
                self.last.display(),
                self.started.elapsed(),
                self.examined
            );
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(from = "CompactStamp", into = "CompactStamp")]
struct Stamp {
    size: u64,
    modified: u128,
    // Size and mtime alone cannot prove unchanged bytes (editors can preserve
    // both). Unix inode change time closes that hole. Elsewhere always hash.
    changed: Option<(i64, i64)>,
    hash: String,
    included: Vec<PathBuf>,
    env_keys: Vec<String>,
}

// Positional fields avoid repeating six property names for every input. The
// digest remains a single 64-character hex string, not a JSON array of bytes.
type CompactStamp = (
    u64,
    u128,
    Option<(i64, i64)>,
    String,
    Vec<PathBuf>,
    Vec<String>,
);

impl From<Stamp> for CompactStamp {
    fn from(stamp: Stamp) -> Self {
        (
            stamp.size,
            stamp.modified,
            stamp.changed,
            stamp.hash,
            stamp.included,
            stamp.env_keys,
        )
    }
}

impl From<CompactStamp> for Stamp {
    fn from((size, modified, changed, hash, included, env_keys): CompactStamp) -> Self {
        Self {
            size,
            modified,
            changed,
            hash,
            included,
            env_keys,
        }
    }
}

fn stamp(meta: &fs::Metadata) -> Option<Stamp> {
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        Some((meta.ctime(), meta.ctime_nsec()))
    };
    #[cfg(not(unix))]
    let changed = None;
    Some(Stamp {
        size: meta.len(),
        modified: meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
        changed,
        hash: String::new(),
        included: Vec::new(),
        env_keys: Vec::new(),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Fingerprint {
    files: BTreeMap<PathBuf, Stamp>,
    runtime: String,
    digest: String,
    pub(crate) examined: usize,
    pub(crate) hashed: usize,
    extra_inputs: Vec<PathBuf>,
    extra_env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    source_facts: BTreeMap<PathBuf, CachedFacts>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    macro_names: BTreeSet<String>,
    #[serde(skip)]
    environment_overrides: BTreeMap<String, String>,
}

fn expired(deadline: Instant) -> Option<()> {
    (Instant::now() < deadline).then_some(())
}

fn read_bounded(path: &Path, deadline: Instant) -> Option<Vec<u8>> {
    expired(deadline)?;
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 65536];
    loop {
        expired(deadline)?;
        let n = file.read(&mut buffer).ok()?;
        if n == 0 {
            return Some(bytes);
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
}

// Refuse external path dependencies, workspace members and symlinks instead of
// guessing their input boundary. Include all manifests, even excluded members:
// Cargo can use them through patches, target-specific and build dependencies.
fn paths_stay_inside(
    value: &toml::Value,
    base: &Path,
    root: &Path,
    packages: &mut Vec<PathBuf>,
) -> Option<()> {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                if key == "path" {
                    let path = canonical(&base.join(value.as_str()?))?;
                    if !path.starts_with(root) {
                        return None;
                    }
                    if path.is_dir() && path.join("Cargo.toml").is_file() {
                        packages.push(path);
                    }
                } else if key == "members" {
                    for member in value.as_array()? {
                        if member.as_str()?.split('/').any(|part| part == "..") {
                            return None;
                        }
                    }
                } else {
                    paths_stay_inside(value, base, root, packages)?;
                }
            }
        }
        toml::Value::Array(array) => {
            for value in array {
                paths_stay_inside(value, base, root, packages)?;
            }
        }
        _ => {}
    }
    Some(())
}

fn workspace_packages(root: &Path, deadline: Instant) -> Option<Vec<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    let mut visited = std::collections::BTreeSet::new();
    let mut packages = Vec::new();
    while let Some(package) = pending.pop() {
        expired(deadline)?;
        let package = canonical(&package)?;
        if !package.starts_with(root) {
            return None;
        }
        if !visited.insert(package.clone()) {
            continue;
        }
        let bytes = read_bounded(&package.join("Cargo.toml"), deadline)?;
        let value: toml::Value = std::str::from_utf8(&bytes).ok()?.parse().ok()?;
        paths_stay_inside(&value, &package, root, &mut pending)?;
        if value.get("package").is_some() {
            packages.push(package.clone());
        }
        if let Some(members) = value.get("workspace").and_then(|w| w.get("members")) {
            for member in members.as_array()? {
                let pattern = package.join(member.as_str()?);
                let mut matches = glob::glob(pattern.to_str()?).ok()?;
                loop {
                    expired(deadline)?;
                    let Some(path) = matches.next() else {
                        break;
                    };
                    pending.push(path.ok()?);
                }
            }
        }
    }
    Some(packages)
}

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) fn fingerprint(
    root: &Path,
    runtime: String,
    previous: Option<&Fingerprint>,
    deadline: Instant,
    // When capturing a check's inputs after reading its begin event, no file
    // or directory may have changed since the reader observed that event.
    began: Option<SystemTime>,
) -> Option<Fingerprint> {
    let mut unresolved = None;
    let captured =
        fingerprint_with_reason(root, runtime, previous, deadline, began, &mut unresolved);
    #[cfg(feature = "test-hooks")]
    if let Some(unresolved) = unresolved {
        eprintln!("{}", unresolved.note(root));
    }
    captured
}

/// Test-only access to the production fingerprint for paired release probes.
#[cfg(feature = "test-hooks")]
pub struct FingerprintProbe(Fingerprint);

#[cfg(feature = "test-hooks")]
impl FingerprintProbe {
    pub fn capture(root: &Path, previous: Option<&Self>) -> Option<Self> {
        fingerprint(
            root,
            "release-probe".into(),
            previous.map(|previous| &previous.0),
            Instant::now() + Duration::from_secs(60),
            None,
        )
        .map(Self)
    }

    pub fn stats(&self) -> (usize, usize, usize, u64, usize) {
        let (rust_files, rust_bytes) = self
            .0
            .files
            .iter()
            .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "rs"))
            .fold((0, 0), |(count, bytes), (_, stamp)| {
                (count + 1, bytes + stamp.size)
            });
        (
            self.0.examined,
            self.0.files.len(),
            rust_files,
            rust_bytes,
            self.0.hashed,
        )
    }

    pub fn same_digest(&self, other: &Self) -> bool {
        self.0.digest == other.0.digest
    }
}

fn fingerprint_with_reason(
    root: &Path,
    runtime: String,
    previous: Option<&Fingerprint>,
    deadline: Instant,
    began: Option<SystemTime>,
    unresolved: &mut Option<UnresolvedInput>,
) -> Option<Fingerprint> {
    let latest_safe_time = match began {
        Some(begin) => Some(begin.checked_sub(CHECK_BEGIN_TIMESTAMP_MARGIN)?),
        None => None,
    };
    let mut files = BTreeMap::new();
    let mut report = WalkReport {
        root,
        examined: 0,
        complete: false,
        last: root.to_path_buf(),
        started: Instant::now(),
    };
    let mut hashed = 0;
    let mut source_inputs =
        SourceInputs::new(previous.map_or_else(BTreeSet::new, |p| p.macro_names.clone()));
    let mut pending = workspace_packages(root, deadline)?;
    let mut inputs = vec![root.join("Cargo.toml")];
    let extra_inputs = previous.map_or_else(Vec::new, |p| p.extra_inputs.clone());
    let extra_env_keys = previous.map_or_else(Vec::new, |p| p.extra_env_keys.clone());
    let environment_overrides =
        previous.map_or_else(BTreeMap::new, |p| p.environment_overrides.clone());
    inputs.extend(extra_inputs.clone());
    if root.join("Cargo.lock").is_file() {
        inputs.push(root.join("Cargo.lock"));
    }
    while let Some(directory) = pending.pop() {
        report.last = directory.clone();
        expired(deadline)?;
        if latest_safe_time.is_some_and(|cutoff| {
            fs::metadata(&directory)
                .ok()
                .and_then(|m| m.modified().ok())
                .is_none_or(|time| time >= cutoff)
        }) {
            return None;
        }
        let mut entries = fs::read_dir(&directory).ok()?;
        loop {
            // The deadline is checked BEFORE advancing the iterator, not after
            // collecting an unbounded list of entries.
            expired(deadline)?;
            let Some(entry) = entries.next() else {
                break;
            };
            let entry = entry.ok()?;
            report.examined += 1;
            let name = entry.file_name();
            if matches!(name.to_str(), Some(".git" | "target" | "node_modules")) {
                continue;
            }
            let kind = entry.file_type().ok()?;
            if kind.is_symlink() {
                return None;
            }
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && (path.extension().is_some_and(|ext| ext == "rs")
                    || matches!(
                        name.to_str(),
                        Some(
                            "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
                        )
                    ))
            {
                inputs.push(path);
            }
        }
    }
    // Cargo and rustup discover configuration upward, including legacy config.
    for parent in root.ancestors() {
        expired(deadline)?;
        for name in [
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            let path = parent.join(name);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() => inputs.push(path),
                Ok(_) => return None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
    }
    if let Some(home) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
    {
        for name in ["config", "config.toml"] {
            let path = home.join(name);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() => inputs.push(path),
                Ok(_) => return None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return None,
            }
        }
    }
    loop {
        if inputs.is_empty() {
            source_inputs.scan_pending(root, &files, deadline, unresolved)?;
            for path in source_inputs.resolve(root, deadline, unresolved)? {
                expired(deadline)?;
                if !files.contains_key(&path)
                    || (!path.extension().is_some_and(|ext| ext == "rs")
                        && source_inputs.needs_parse(&path))
                {
                    inputs.push(path);
                }
            }
        }
        let Some(path) = inputs.pop() else {
            break;
        };
        report.last = path.clone();
        expired(deadline)?;
        if files.contains_key(&path)
            && (path.extension().is_some_and(|ext| ext == "rs")
                || !source_inputs.needs_parse(&path))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).ok()?;
        if metadata.is_symlink() {
            return None;
        }
        if metadata.is_dir() {
            if latest_safe_time
                .is_some_and(|cutoff| metadata.modified().ok().is_none_or(|time| time >= cutoff))
            {
                return None;
            }
            let mut entries = fs::read_dir(path).ok()?;
            loop {
                expired(deadline)?;
                let Some(entry) = entries.next() else {
                    break;
                };
                inputs.push(entry.ok()?.path());
            }
            continue;
        }
        if !metadata.is_file() {
            return None;
        }
        let mut current = stamp(&fs::metadata(&path).ok()?)?;
        if let Some(cutoff) = latest_safe_time {
            let cutoff = cutoff.duration_since(UNIX_EPOCH).ok()?.as_nanos();
            if current.modified >= cutoff {
                return None;
            }
            #[cfg(unix)]
            if current
                .changed
                .is_none_or(|(s, ns)| s < 0 || (s as u128 * 1_000_000_000 + ns as u128) >= cutoff)
            {
                return None;
            }
        }
        let old = previous.and_then(|p| p.files.get(&path));
        let mut source_bytes = None;
        // Manifests are always read, because their path dependencies also need
        // validating. Index hashes with no inode-change proof cannot be reused.
        let manifest = path.file_name().is_some_and(|name| name == "Cargo.toml");
        if !manifest
            && current.changed.is_some()
            && old.is_some_and(|old| {
                old.size == current.size
                    && old.modified == current.modified
                    && old.changed == current.changed
            })
        {
            current.hash = old?.hash.clone();
            current.included = old?.included.clone();
            current.env_keys = old?.env_keys.clone();
        } else {
            let bytes = read_bounded(&path, deadline)?;
            if stamp(&fs::metadata(&path).ok()?)? != current {
                return None;
            }
            current.hash = blake3::hash(&bytes).to_hex().to_string();
            source_bytes = Some(bytes);
            hashed += 1;
        }
        if source_inputs.needs_parse(&path) {
            // Facts are keyed by the same content hash as the file stamp.
            // Unchanged sources need neither a read nor a tree-sitter parse.
            let cached = previous
                .filter(|_| old.is_some_and(|old| old.hash == current.hash))
                .and_then(|previous| previous.source_facts.get(&path))
                .cloned();
            let (included, env_keys) =
                source_inputs.add(root, &path, source_bytes, cached, deadline, unresolved)?;
            current.included = included;
            current.env_keys = env_keys;
        }
        inputs.extend(current.included.clone());
        files.insert(path, current);
    }
    expired(deadline)?;
    let content = files
        .iter()
        .map(|(path, stamp)| (path, &stamp.hash))
        .collect::<Vec<_>>();
    let mut env = BTreeMap::new();
    for key in files
        .values()
        .flat_map(|stamp| &stamp.env_keys)
        .chain(&extra_env_keys)
    {
        let value = environment_overrides
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok());
        env.insert(
            key,
            blake3::hash(&serde_json::to_vec(&value).ok()?)
                .to_hex()
                .to_string(),
        );
    }
    let digest = blake3::hash(&serde_json::to_vec(&(&runtime, content, env)).ok()?)
        .to_hex()
        .to_string();
    crate::slog_info!(
        "rust check fingerprint root={} examined={} inputs={} hashed={} complete=true",
        root.display(),
        report.examined,
        files.len(),
        hashed
    );
    report.complete = true;
    let macro_names = source_inputs.macro_names();
    Some(Fingerprint {
        files,
        runtime,
        digest,
        examined: report.examined,
        hashed,
        extra_inputs,
        extra_env_keys,
        source_facts: source_inputs.facts,
        macro_names,
        environment_overrides,
    })
}

fn version(
    binary: &Path,
    args: &[&str],
    root: &Path,
    env: &HashMap<String, String>,
    deadline: Instant,
) -> Option<String> {
    expired(deadline)?;
    let mut child = Command::new(binary)
        .args(args)
        .current_dir(root)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut text = String::new();
        let result = stdout.read_to_string(&mut text).map(|_| text);
        let _ = tx.send(result);
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let text = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?
        .ok()?;
    expired(deadline)?;
    (!text.trim().is_empty()).then_some(text)
}

#[derive(Clone)]
pub(crate) struct Runtime {
    pub(crate) binary: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: HashMap<String, String>,
    pub(crate) options: Option<serde_json::Value>,
    pub(crate) launch_env: Option<BTreeMap<String, String>>,
    #[cfg(test)]
    pub(crate) validation_gate: Option<(
        crossbeam_channel::Sender<()>,
        crossbeam_channel::Receiver<()>,
    )>,
}

fn relevant_environment_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("RUST")
        || key.starts_with("CARGO_")
        || key.starts_with("RA_")
        || key.starts_with("CC_")
        || key.starts_with("CXX_")
        || key.starts_with("AR_")
        || key.starts_with("PKG_CONFIG")
        || key.starts_with("BINDGEN_")
        || matches!(
            key.as_str(),
            "PATH"
                | "HOME"
                | "USERPROFILE"
                | "SYSTEMROOT"
                | "TEMP"
                | "TMP"
                | "CC"
                | "CXX"
                | "AR"
                | "LD"
                | "CFLAGS"
                | "CXXFLAGS"
                | "CPPFLAGS"
                | "LDFLAGS"
                | "LIB"
                | "INCLUDE"
                | "LIBCLANG_PATH"
        )
}

impl Runtime {
    fn effective_env(&self) -> Option<BTreeMap<String, String>> {
        let mut env = BTreeMap::new();
        for (key, value) in std::env::vars_os() {
            env.insert(key.into_string().ok()?, value.into_string().ok()?);
        }
        env.extend(self.env.clone());
        Some(env)
    }

    fn hash(&self, root: &Path, deadline: Instant) -> Option<String> {
        // Store only a digest of environment/settings, never secrets in clear.
        // Compiler/tool-discovery variables and explicitly passed settings are
        // included here. Source env! and build-script declarations add their
        // own keys during capture; unrelated launch nonces must not prevent reuse.
        let env = self.effective_env()?;
        let relevant = |env: &BTreeMap<String, String>| {
            env.iter()
                .filter(|(key, _)| relevant_environment_key(key) || self.env.contains_key(*key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        if self
            .launch_env
            .as_ref()
            .is_some_and(|launched| relevant(launched) != relevant(&env))
        {
            return None;
        }
        let rustc = env
            .get("RUSTC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rustc"));
        let rustc_version = version(&rustc, &["-vV"], root, &self.env, deadline)?;
        let analyzer_version = version(&self.binary, &["--version"], root, &self.env, deadline)?;
        Some(
            blake3::hash(
                &serde_json::to_vec(&(
                    &self.binary,
                    &self.args,
                    &self.options,
                    relevant(&env),
                    rustc_version,
                    analyzer_version,
                ))
                .ok()?,
            )
            .to_hex()
            .to_string(),
        )
    }

    fn capture(
        &self,
        root: &Path,
        previous: Option<&Fingerprint>,
        deadline: Instant,
        began: Option<SystemTime>,
    ) -> Option<Fingerprint> {
        self.capture_with_reason(root, previous, deadline, began, &mut None)
    }

    fn capture_with_reason(
        &self,
        root: &Path,
        previous: Option<&Fingerprint>,
        deadline: Instant,
        began: Option<SystemTime>,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<Fingerprint> {
        let runtime = self.hash(root, deadline)?;
        let mut seed = previous.cloned().unwrap_or(Fingerprint {
            files: BTreeMap::new(),
            runtime: String::new(),
            digest: String::new(),
            examined: 0,
            hashed: 0,
            extra_inputs: Vec::new(),
            extra_env_keys: Vec::new(),
            source_facts: BTreeMap::new(),
            macro_names: BTreeSet::new(),
            environment_overrides: BTreeMap::new(),
        });
        seed.environment_overrides = self
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let env = self.effective_env()?;
        if let Some(home) = env.get("CARGO_HOME").map(PathBuf::from).or_else(|| {
            env.get("HOME")
                .map(|home| PathBuf::from(home).join(".cargo"))
        }) {
            for name in ["config", "config.toml"] {
                let path = home.join(name);
                match fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_file() => seed.extra_inputs.push(path),
                    Ok(_) => return None,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
        seed.extra_inputs.sort();
        seed.extra_inputs.dedup();
        let captured =
            fingerprint_with_reason(root, runtime, Some(&seed), deadline, began, unresolved)?;
        for key in captured
            .files
            .values()
            .flat_map(|stamp| &stamp.env_keys)
            .chain(&captured.extra_env_keys)
        {
            if self
                .launch_env
                .as_ref()
                .is_some_and(|launched| launched.get(key) != env.get(key))
            {
                return None;
            }
        }
        Some(captured)
    }

    fn build_inputs(
        &self,
        root: &Path,
        inputs: &Fingerprint,
        deadline: Instant,
    ) -> Option<(Vec<PathBuf>, Vec<String>)> {
        let mut outputs = Vec::new();
        let mut env_keys = Vec::new();
        for package in workspace_packages(root, deadline)? {
            let bytes = read_bounded(&package.join("Cargo.toml"), deadline)?;
            let manifest: toml::Value = std::str::from_utf8(&bytes).ok()?.parse().ok()?;
            let config = manifest.get("package")?;
            if config.get("build").and_then(toml::Value::as_bool) == Some(false) {
                continue;
            }
            if !package
                .join(
                    config
                        .get("build")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("build.rs"),
                )
                .is_file()
            {
                continue;
            }
            // Without a known Cargo output directory we cannot discover a build
            // script's non-Rust inputs. Refuse unusual layouts, never guess.
            for path in inputs.files.keys().filter(|path| {
                path.file_name()
                    .is_some_and(|name| name == "config" || name == "config.toml")
                    && path.parent().is_some_and(|parent| {
                        parent.file_name().is_some_and(|name| name == ".cargo")
                    })
            }) {
                let bytes = read_bounded(path, deadline)?;
                let config: toml::Value = std::str::from_utf8(&bytes).ok()?.parse().ok()?;
                if config.get("source").is_some()
                    || config
                        .get("build")
                        .and_then(|b| b.get("target-dir"))
                        .is_some()
                {
                    return None;
                }
            }
            if self.options.as_ref().is_some_and(|o| {
                o.get("linkedProjects").is_some() || o.pointer("/check/overrideCommand").is_some()
            }) {
                return None;
            }
            for args in ["/check/extraArgs", "/cargo/extraArgs"] {
                if self
                    .options
                    .as_ref()
                    .and_then(|o| o.pointer(args))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|args| {
                        args.iter()
                            .filter_map(serde_json::Value::as_str)
                            .any(|arg| arg.starts_with("--target-dir"))
                    })
                {
                    return None;
                }
            }
            let env = self.effective_env()?;
            let mut target = root.join(
                env.get("CARGO_TARGET_DIR")
                    .or_else(|| env.get("CARGO_BUILD_TARGET_DIR"))
                    .map(String::as_str)
                    .unwrap_or("target"),
            );
            match self
                .options
                .as_ref()
                .and_then(|o| o.pointer("/cargo/targetDir"))
            {
                Some(serde_json::Value::String(value)) => target = root.join(value),
                Some(serde_json::Value::Bool(true)) => target.push("rust-analyzer"),
                _ => {}
            }
            let mut profiles = vec![target.join("debug/build"), target.join("release/build")];
            let mut triples = fs::read_dir(&target).ok()?;
            loop {
                expired(deadline)?;
                let Some(triple) = triples.next() else {
                    break;
                };
                let triple = triple.ok()?;
                if triple.file_type().ok()?.is_dir() {
                    profiles.push(triple.path().join("debug/build"));
                    profiles.push(triple.path().join("release/build"));
                }
            }
            let prefix = format!("{}-", config.get("name")?.as_str()?);
            let mut declared = false;
            for profile in profiles {
                let mut entries = match fs::read_dir(profile) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return None,
                };
                loop {
                    expired(deadline)?;
                    let Some(entry) = entries.next() else {
                        break;
                    };
                    let entry = entry.ok()?;
                    if !entry.file_name().to_str()?.starts_with(&prefix) {
                        continue;
                    }
                    let output = entry.path().join("output");
                    if !output.is_file() {
                        continue;
                    }
                    let bytes = read_bounded(&output, deadline)?;
                    for line in std::str::from_utf8(&bytes).ok()?.lines() {
                        expired(deadline)?;
                        if let Some(key) = line
                            .strip_prefix("cargo:rerun-if-env-changed=")
                            .or_else(|| line.strip_prefix("cargo::rerun-if-env-changed="))
                        {
                            declared = true;
                            env_keys.push(key.to_string());
                        }
                        if let Some(path) = line
                            .strip_prefix("cargo:rerun-if-changed=")
                            .or_else(|| line.strip_prefix("cargo::rerun-if-changed="))
                        {
                            declared = true;
                            outputs.push(canonical(&package.join(path))?);
                        }
                    }
                }
            }
            // With no declarations Cargo watches the entire package, possibly
            // including ignored/generated inputs. That is an uncertain boundary.
            if !declared {
                return None;
            }
        }
        outputs.sort();
        outputs.dedup();
        env_keys.sort();
        env_keys.dedup();
        Some((outputs, env_keys))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "DiskCheck", into = "DiskCheck")]
pub(crate) struct SavedCheck {
    schema: u32,
    root: PathBuf,
    checkout: PathBuf,
    fingerprint: Fingerprint,
    pub(crate) diagnostics: BTreeMap<PathBuf, Vec<StoredDiagnostic>>,
    completed_seconds: u64,
    duration_seconds: u64,
}

#[derive(Serialize, Deserialize)]
struct DiskCheck {
    schema: u32,
    root: PathBuf,
    checkout: PathBuf,
    fingerprint: Fingerprint,
    diagnostics: BTreeMap<PathBuf, Vec<StoredDiagnostic>>,
    completed_seconds: u64,
    duration_seconds: u64,
}

fn relative_input(root: &Path, path: PathBuf) -> PathBuf {
    // Cargo also reads ancestor and user-home configuration. Keep those few
    // external inputs absolute rather than silently dropping their hashes.
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or(path)
}

fn map_fingerprint_paths(
    mut fingerprint: Fingerprint,
    mut map: impl FnMut(PathBuf) -> Result<PathBuf, &'static str>,
) -> Result<Fingerprint, &'static str> {
    let mut files = BTreeMap::new();
    for (path, mut stamp) in fingerprint.files {
        stamp.included = stamp
            .included
            .into_iter()
            .map(&mut map)
            .collect::<Result<_, _>>()?;
        if files.insert(map(path)?, stamp).is_some() {
            return Err("duplicate input path");
        }
    }
    fingerprint.files = files;
    fingerprint.extra_inputs = fingerprint
        .extra_inputs
        .into_iter()
        .map(&mut map)
        .collect::<Result<_, _>>()?;
    let mut source_facts = BTreeMap::new();
    for (path, mut facts) in fingerprint.source_facts {
        facts.map_paths(&mut map)?;
        if source_facts.insert(map(path)?, facts).is_some() {
            return Err("duplicate source facts path");
        }
    }
    fingerprint.source_facts = source_facts;
    Ok(fingerprint)
}

impl From<SavedCheck> for DiskCheck {
    fn from(saved: SavedCheck) -> Self {
        let fingerprint = map_fingerprint_paths(saved.fingerprint, |path| {
            Ok(relative_input(&saved.root, path))
        })
        .expect("canonical fingerprint paths remain distinct when made relative");
        Self {
            schema: saved.schema,
            root: saved.root,
            checkout: saved.checkout,
            fingerprint,
            diagnostics: saved.diagnostics,
            completed_seconds: saved.completed_seconds,
            duration_seconds: saved.duration_seconds,
        }
    }
}

impl TryFrom<DiskCheck> for SavedCheck {
    type Error = &'static str;

    fn try_from(disk: DiskCheck) -> Result<Self, Self::Error> {
        if disk.schema != RECORD_SCHEMA || !disk.root.is_absolute() || !disk.checkout.is_absolute()
        {
            return Err("unsupported completed-check identity");
        }
        for stamp in disk.fingerprint.files.values() {
            blake3::Hash::from_hex(&stamp.hash).map_err(|_| "invalid input hash")?;
        }
        let fingerprint = map_fingerprint_paths(disk.fingerprint, |path| {
            if path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            }) {
                return Err("non-canonical input path");
            }
            Ok(disk.root.join(path))
        })?;
        Ok(Self {
            schema: disk.schema,
            root: disk.root,
            checkout: disk.checkout,
            fingerprint,
            diagnostics: disk.diagnostics,
            completed_seconds: disk.completed_seconds,
            duration_seconds: disk.duration_seconds,
        })
    }
}

impl SavedCheck {
    pub(crate) fn note(&self) -> String {
        // UTC is explicit, so a restart or a timezone change cannot silently
        // change the interpretation of the saved completion timestamp.
        let time = self.completed_seconds % 86400;
        format!("rust: from the last completed check at {:02}:{:02} UTC (no Rust inputs changed since; examined {} entries, {} inputs)", time / 3600, time / 60 % 60, self.fingerprint.examined, self.fingerprint.files.len())
    }
    pub(crate) fn covers(&self, file: &Path) -> bool {
        file.extension().is_some_and(|extension| extension == "rs")
            && self.fingerprint.files.contains_key(file)
    }
    pub(crate) fn content_hash(&self, file: &Path) -> Option<[u8; 32]> {
        let stamp = self.fingerprint.files.get(file)?;
        Some(*blake3::Hash::from_hex(&stamp.hash).ok()?.as_bytes())
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    record: SavedCheck,
    checksum: String,
}

fn write_record(path: &Path, record: &SavedCheck) -> Option<()> {
    let bytes = serde_json::to_vec(record).ok()?;
    let envelope = Envelope {
        record: record.clone(),
        checksum: blake3::hash(&bytes).to_hex().to_string(),
    };
    let text = serde_json::to_string(&envelope).ok()?;
    let directory = path.parent()?;
    // Serialize writers across daemon processes, including eviction, so parallel
    // worktree checks cannot each claim the last free cache slot.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".write-lock"))
        .ok()?;
    lock.lock().ok()?;
    let _lock = crate::fs_lock::AdvisoryLockGuard::from_locked_file(lock);
    if !sweep_records(directory, path, RECORD_SCAN_LIMIT).complete {
        // A saturated legacy store is reduced on each write attempt. Do not add
        // another record until a bounded scan can certify the total record cap.
        return None;
    }
    crate::jsonc_edit::write_atomic(path, &text).ok()
}

#[derive(Deserialize)]
struct RecordHeader {
    checkout: PathBuf,
}

#[derive(Deserialize)]
struct EnvelopeHeader {
    record: RecordHeader,
}

#[derive(Default)]
struct RecordSweep {
    examined: usize,
    removed: usize,
    complete: bool,
}

fn sweep_records(directory: &Path, writing: &Path, scan_limit: usize) -> RecordSweep {
    let mut sweep = RecordSweep::default();
    let Ok(mut entries) = fs::read_dir(directory) else {
        return sweep;
    };
    let deadline = Instant::now() + BUDGET;
    let mut retained = Vec::new();
    loop {
        // Count every entry, including non-records, before advancing the iterator.
        if sweep.examined >= scan_limit || expired(deadline).is_none() {
            break;
        }
        let Some(entry) = entries.next() else {
            sweep.complete = true;
            break;
        };
        sweep.examined += 1;
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path == writing
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
            || path.extension().is_none_or(|ext| ext != "json")
            || !path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| {
                    stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        {
            continue;
        }
        let dead = read_bounded(&path, deadline)
            .and_then(|bytes| serde_json::from_slice::<EnvelopeHeader>(&bytes).ok())
            .is_some_and(|header| {
                header.record.checkout.is_absolute()
                    && matches!(fs::metadata(&header.record.checkout), Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound)
            });
        if dead && fs::remove_file(&path).is_ok() {
            sweep.removed += 1;
            continue;
        }
        // Record mtime is validation recency, not access recency. Successful
        // validation refreshes it; merely reading an invalid cache never does.
        let validated = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        retained.push((validated, path));
    }
    retained.sort_unstable_by(|a, b| b.cmp(a));
    // Reserve one slot for the record about to be atomically written.
    for (_, path) in retained.into_iter().skip(MAX_RECORDS - 1) {
        if fs::remove_file(path).is_ok() {
            sweep.removed += 1;
        } else {
            sweep.complete = false;
        }
    }
    crate::slog_info!(
        "rust completed-check sweep examined={} removed={} complete={}",
        sweep.examined,
        sweep.removed,
        sweep.complete
    );
    sweep
}

fn mark_validated(path: &Path) {
    // Opening without create cannot resurrect an evicted record. Using the open
    // file also avoids touching an atomic replacement that raced validation.
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
    }
}

pub(crate) struct CompletedRustCheck {
    root: PathBuf,
    checkout: PathBuf,
    path: PathBuf,
    persistence_enabled: bool,
    runtime: Runtime,
    saved: Option<Arc<SavedCheck>>,
    validation: parking_lot::Mutex<Option<std::sync::mpsc::Receiver<Option<SavedCheck>>>>,
    pending: Option<Fingerprint>,
    pub(crate) reports: BTreeMap<PathBuf, Vec<StoredDiagnostic>>,
    started: Option<Instant>,
    began: Option<SystemTime>,
    pub(crate) finished: bool,
    #[cfg(not(unix))]
    pre_spawn: Option<Fingerprint>,
}

impl CompletedRustCheck {
    pub(crate) fn in_memory(root: &Path, checkout: &Path, mut runtime: Runtime) -> Option<Self> {
        let root = canonical(root)?;
        let checkout = canonical(checkout)?;
        runtime.launch_env = Some(runtime.effective_env()?);
        #[cfg(not(unix))]
        let pre_spawn = runtime.capture(&root, None, Instant::now() + INITIAL_CAPTURE_BUDGET, None);
        Some(Self {
            root,
            checkout,
            runtime,
            // No disk operation uses the empty path while persistence is disabled.
            path: PathBuf::new(),
            persistence_enabled: false,
            saved: None,
            validation: parking_lot::Mutex::new(None),
            pending: None,
            reports: BTreeMap::new(),
            started: None,
            began: None,
            finished: false,
            #[cfg(not(unix))]
            pre_spawn,
        })
    }

    pub(crate) fn automatic_checks_disabled(&self) -> bool {
        super::client::rust_check_triggers(self.runtime.options.as_ref()) == (false, false)
    }

    pub(crate) fn running(&self) -> bool {
        self.started.is_some()
    }

    /// Compile only for an explicit inspect request. Native analyzer diagnostics
    /// cannot prove borrow checking succeeded, so an idle server is insufficient.
    pub(crate) fn run_for_inspect(
        &mut self,
        deadline: Instant,
        cancelled: impl Fn() -> bool,
    ) -> Result<SavedCheck, String> {
        if let Some(saved) = self.validated(deadline.min(Instant::now() + BUDGET)) {
            return Ok(saved);
        }
        let running = "cargo check running for aft_inspect; retry";
        if cancelled() || Instant::now() >= deadline {
            return Err(running.into());
        }
        // Capture before spawning, not after receiving a progress event. This
        // also works on platforms without inode change timestamps.
        self.reports.clear();
        let mut unresolved = None;
        let before = self.runtime.capture_with_reason(
            &self.root,
            self.saved.as_ref().map(|saved| &saved.fingerprint),
            deadline,
            None,
            &mut unresolved,
        );
        let mut args = vec![
            "check".to_string(),
            "--workspace".into(),
            "--message-format=json".into(),
            "--locked".into(),
        ];
        if let Some(target) = self.inspect_target_dir(deadline, &cancelled)? {
            args.push("--target-dir".into());
            args.push(target.to_string_lossy().into_owned());
        }
        self.started = Some(Instant::now());
        self.began = Some(SystemTime::now());
        let result = self.cargo(&args, deadline, &cancelled);
        let output = match result {
            Ok(output) => output,
            Err(reason) => {
                self.abort();
                return Err(reason);
            }
        };
        let (reports, finished) = cargo_reports(&self.root, &output.stdout)?;
        let has_error = reports
            .values()
            .flatten()
            .any(|row| row.severity == DiagnosticSeverity::Error);
        if !finished || output.truncated || (!output.status.success() && !has_error) {
            self.abort();
            return Err(format!(
                "aft_inspect: cargo check did not produce a complete diagnostic report: {}",
                output.stderr.trim()
            ));
        }
        self.reports = reports;
        let Some(before) = before else {
            self.abort();
            if let Some(unresolved) = unresolved {
                return Err(format!("aft_inspect: {}", unresolved.note(&self.root)));
            }
            return Err("aft_inspect: cargo check completed, but Rust inputs cannot be fingerprinted; diagnostics are provisional".into());
        };
        let after =
            self.runtime
                .capture_with_reason(&self.root, Some(&before), deadline, None, &mut unresolved)
                .ok_or_else(|| match unresolved {
                    Some(unresolved) => format!("aft_inspect: {}", unresolved.note(&self.root)),
                    None => "aft_inspect: cannot validate Rust inputs after cargo check; retry"
                        .to_string(),
                })?;
        if cancelled() || before.files != after.files || before.digest != after.digest {
            self.abort();
            return Err("aft_inspect: Rust inputs changed during cargo check; retry".into());
        }
        self.pending = Some(before);
        self.finished = true;
        // Completion already captures and certifies the post-check inputs. A
        // second fingerprint worker can lose the remaining request budget to
        // scheduling even though this check has just been certified.
        let completed = self.complete_until(deadline, true);
        if cancelled() || Instant::now() >= deadline {
            return Err("aft_inspect: cannot certify completed cargo check; retry".into());
        }
        completed.ok_or_else(|| "aft_inspect: cannot certify completed cargo check; retry".into())
    }

    fn cargo(
        &self,
        args: &[String],
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> Result<crate::format::SubprocessOutcome, String> {
        if cancelled() || Instant::now() >= deadline {
            return Err("cargo check running for aft_inspect; retry".into());
        }
        let binary = self
            .runtime
            .env
            .get("CARGO")
            .map(String::as_str)
            .unwrap_or("cargo");
        let mut command = crate::effective_path::new_command(binary)
            .envs(&self.runtime.env)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .build();
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::format::isolate_in_process_group(&mut command);
        let child = command
            .spawn()
            .map_err(|error| format!("aft_inspect: cargo check could not start: {error}"))?;
        crate::format::wait_with_deadline(child, "cargo check", deadline, cancelled).map_err(
            |error| match error {
                crate::format::FormatError::Timeout { .. } => {
                    "cargo check running for aft_inspect; retry".into()
                }
                _ => format!("aft_inspect: cargo check failed: {error}"),
            },
        )
    }

    fn inspect_target_dir(
        &self,
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Option<PathBuf>, String> {
        match self
            .runtime
            .options
            .as_ref()
            .and_then(|o| o.pointer("/cargo/targetDir"))
        {
            Some(serde_json::Value::String(path)) => Ok(Some(self.root.join(path))),
            Some(serde_json::Value::Bool(true)) => {
                // Metadata resolves Cargo's environment/config target directory.
                // Rust-analyzer's cargo.targetDir=true setting uses its
                // rust-analyzer child directory rather than the build directory.
                let output = self.cargo(
                    &[
                        "metadata".into(),
                        "--no-deps".into(),
                        "--format-version=1".into(),
                        "--locked".into(),
                    ],
                    deadline,
                    cancelled,
                )?;
                let metadata: serde_json::Value =
                    serde_json::from_str(&output.stdout).map_err(|_| {
                        "aft_inspect: cannot resolve Cargo target directory".to_string()
                    })?;
                if !output.status.success() || output.truncated {
                    return Err("aft_inspect: cannot resolve Cargo target directory".into());
                }
                let path = metadata
                    .get("target_directory")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        "aft_inspect: Cargo metadata has no target directory".to_string()
                    })?;
                Ok(Some(PathBuf::from(path).join("rust-analyzer")))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn new(
        root: &Path,
        checkout: &Path,
        storage: &Path,
        mut runtime: Runtime,
    ) -> Option<Self> {
        let root = canonical(root)?;
        let checkout = root
            .ancestors()
            .find(|path| path.join(".git").exists())
            .map(Path::to_path_buf)
            .or_else(|| canonical(checkout))?;
        runtime.launch_env = Some(runtime.effective_env()?);
        // A misconfigured storage directory must not write cache artifacts into
        // the source checkout. Resolve symlink parents as well.
        fs::create_dir_all(storage).ok()?;
        let storage = canonical(storage)?;
        if storage.starts_with(&checkout) || storage.starts_with(&root) {
            return None;
        }
        let key = blake3::hash(&serde_json::to_vec(&(&checkout, &root)).ok()?)
            .to_hex()
            .to_string();
        let directory = storage.join("rust-completed-checks");
        fs::create_dir_all(&directory).ok()?;
        if fs::symlink_metadata(&directory)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).ok()?;
        }
        let path = directory.join(format!("{key}.json"));
        let saved = read_bounded(&path, Instant::now() + BUDGET)
            .and_then(|bytes| serde_json::from_slice::<Envelope>(&bytes).ok())
            .filter(|envelope| {
                envelope.record.schema == RECORD_SCHEMA
                    && envelope.record.root == root
                    && envelope.record.checkout == checkout
                    && serde_json::to_vec(&envelope.record)
                        .ok()
                        .is_some_and(|bytes| {
                            blake3::hash(&bytes).to_hex().as_str() == envelope.checksum
                        })
            })
            .map(|envelope| Arc::new(envelope.record));
        #[cfg(not(unix))]
        let pre_spawn = {
            let deadline = Instant::now() + INITIAL_CAPTURE_BUDGET;
            runtime.capture(
                &root,
                saved.as_ref().map(|s| &s.fingerprint),
                deadline,
                None,
            )
        };
        Some(Self {
            root,
            checkout,
            path,
            persistence_enabled: true,
            runtime,
            saved,
            validation: parking_lot::Mutex::new(None),
            pending: None,
            reports: BTreeMap::new(),
            started: None,
            began: None,
            finished: false,
            #[cfg(not(unix))]
            pre_spawn,
        })
    }

    pub(crate) fn begin(&mut self, began: SystemTime) {
        let deadline = Instant::now()
            + if self.saved.is_none() {
                INITIAL_CAPTURE_BUDGET
            } else {
                BUDGET
            };
        self.begin_until(began, deadline);
    }

    fn begin_until(&mut self, began: SystemTime, deadline: Instant) {
        self.began = Some(began);
        self.started = SystemTime::now()
            .duration_since(began)
            .ok()
            .and_then(|elapsed| Instant::now().checked_sub(elapsed));
        self.finished = false;
        #[cfg(unix)]
        {
            self.pending = self.runtime.capture(
                &self.root,
                self.saved.as_ref().map(|s| &s.fingerprint),
                deadline,
                Some(began),
            );
        }
        // Windows has no inode-change-time proof here. Only a snapshot captured
        // before spawning may certify a run; changed inputs conservatively need
        // another cold server. Validation still hashes every file on Windows.
        #[cfg(not(unix))]
        {
            let _ = deadline;
            self.pending = self.pre_spawn.clone();
        }
    }

    pub(crate) fn complete(&mut self) -> Option<SavedCheck> {
        self.complete_until(Instant::now() + BUDGET, false)
    }

    fn complete_until(
        &mut self,
        deadline: Instant,
        captured_before_spawn: bool,
    ) -> Option<SavedCheck> {
        if !self.finished {
            return None;
        }
        self.finished = false;
        let started = self.started.take();
        let Some(pending) = self.pending.take() else {
            return None;
        };
        let Some(mut current) = self
            .runtime
            .capture(&self.root, Some(&pending), deadline, None)
        else {
            return None;
        };
        if current.digest != pending.digest {
            return None;
        }
        let Some((build_inputs, env_keys)) =
            self.runtime.build_inputs(&self.root, &current, deadline)
        else {
            return None;
        };
        if captured_before_spawn {
            // Newly discovered build-script inputs were not in the pre-spawn
            // snapshot. They must predate the run, rather than certify bytes
            // changed after the compiler might already have read them.
            let Some(cutoff) = self
                .began
                .and_then(|begin| begin.checked_sub(CHECK_BEGIN_TIMESTAMP_MARGIN))
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_nanos())
            else {
                return None;
            };
            for input in &build_inputs {
                let Some(input) = canonical(input) else {
                    return None;
                };
                if pending.files.contains_key(&input) {
                    continue;
                }
                let Some(metadata) = fs::metadata(input).ok().and_then(|meta| stamp(&meta)) else {
                    return None;
                };
                if metadata.modified >= cutoff {
                    return None;
                }
                if metadata.changed.is_none_or(|(s, ns)| {
                    s < 0 || (s as u128 * 1_000_000_000 + ns as u128) >= cutoff
                }) {
                    return None;
                }
            }
        }
        current.extra_inputs.extend(build_inputs);
        current.extra_env_keys.extend(env_keys);
        current.extra_env_keys.sort();
        current.extra_env_keys.dedup();
        current.extra_inputs.sort();
        current.extra_inputs.dedup();
        let Some(current) = self.runtime.capture(
            &self.root,
            Some(&current),
            deadline,
            if captured_before_spawn {
                None
            } else {
                self.began
            },
        ) else {
            return None;
        };
        #[cfg(not(unix))]
        if current.digest != pending.digest {
            return None;
        }
        let record = SavedCheck {
            schema: RECORD_SCHEMA,
            root: self.root.clone(),
            checkout: self.checkout.clone(),
            fingerprint: current,
            diagnostics: self.reports.clone(),
            completed_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            duration_seconds: started.map_or(0, |start| start.elapsed().as_secs()),
        };
        let path = self.path.clone();
        if !self.persistence_enabled {
            self.saved = Some(Arc::new(record.clone()));
            return Some(record);
        }
        let persisted = record.clone();
        if let Ok(worker) = std::thread::Builder::new()
            .name("aft-rust-check-save".into())
            .spawn(move || {
                if write_record(&path, &persisted).is_none() {
                    crate::slog_info!(
                        "rust completed-check persistence refused path={}",
                        path.display()
                    );
                }
            })
        {
            // The compiler check is already certified; a disk-cache failure does
            // not prevent using its authoritative in-memory snapshot.
            self.saved = Some(Arc::new(record.clone()));
            #[cfg(test)]
            worker.join().unwrap();
            #[cfg(not(test))]
            drop(worker);
            return Some(record);
        }
        None
    }

    pub(crate) fn validated(&self, deadline: Instant) -> Option<SavedCheck> {
        expired(deadline)?;
        let saved = self.saved.clone()?;
        let mut active = self.validation.lock();
        if let Some(receiver) = active.as_ref() {
            if matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ) {
                // An earlier timed-out walk still owns this root's worker.
                // Never accumulate blocked filesystem workers or reuse its old verdict.
                return None;
            }
            *active = None;
        }
        let runtime = self.runtime.clone();
        let root = self.root.clone();
        let path = self.path.clone();
        let persistence_enabled = self.persistence_enabled;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        *active = Some(rx);
        if std::thread::Builder::new()
            .name("aft-rust-fingerprint".into())
            .spawn(move || {
                #[cfg(test)]
                if let Some((entered, release)) = &runtime.validation_gate {
                    let _ = entered.send(());
                    // Disconnecting the sender also releases the worker if the
                    // test unwinds before explicitly opening the gate.
                    let _ = release.recv();
                }
                let result = runtime
                    .capture(&root, Some(&saved.fingerprint), deadline, None)
                    .and_then(|current| accept_saved(&saved, current));
                if persistence_enabled && result.is_some() && expired(deadline).is_some() {
                    mark_validated(&path);
                }
                let _ = tx.send(result);
            })
            .is_err()
        {
            *active = None;
            return None;
        }
        let result = active
            .as_ref()?
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()?;
        *active = None;
        expired(deadline)?;
        result
    }

    pub(crate) fn abort(&mut self) {
        self.pending = None;
        self.finished = false;
        self.started = None;
    }

    #[cfg(test)]
    fn validated_with_runtime(&self, runtime: String, deadline: Instant) -> Option<SavedCheck> {
        let saved = self.saved.as_ref()?;
        let current = fingerprint(
            &self.root,
            runtime,
            Some(&saved.fingerprint),
            deadline,
            None,
        )?;
        self.accept_current(current)
    }

    #[cfg(test)]
    fn accept_current(&self, current: Fingerprint) -> Option<SavedCheck> {
        let saved = self.saved.as_ref()?;
        accept_saved(saved, current)
    }

    #[cfg(test)]
    fn saved_mut(&mut self) -> &mut SavedCheck {
        Arc::make_mut(self.saved.as_mut().unwrap())
    }

    pub(crate) fn running_reason(&self) -> Option<String> {
        let elapsed = self.started?.elapsed().as_secs();
        let last = self.saved.as_ref()?;
        Some(format!("rust-analyzer: cargo check running for {elapsed} s; the last full check here took {}; retry", duration(last.duration_seconds)))
    }
}

fn cargo_reports(
    root: &Path,
    stdout: &str,
) -> Result<(BTreeMap<PathBuf, Vec<StoredDiagnostic>>, bool), String> {
    let mut finished = false;
    for line in stdout.lines() {
        let message: serde_json::Value = serde_json::from_str(line)
            .map_err(|_| "aft_inspect: invalid Cargo JSON diagnostic stream".to_string())?;
        if message.get("reason").and_then(serde_json::Value::as_str) == Some("build-finished") {
            finished = true;
        }
    }
    let mut reports = BTreeMap::<PathBuf, Vec<StoredDiagnostic>>::new();
    for error in crate::format::parse_cargo_messages(stdout) {
        let Some(file) = canonical(&root.join(&error.file)) else {
            continue;
        };
        if !file.starts_with(root) {
            continue;
        }
        reports
            .entry(file.clone())
            .or_default()
            .push(StoredDiagnostic {
                file,
                line: error.line,
                column: error.column,
                end_line: error.line,
                end_column: error.column,
                severity: if error.severity == "warning" {
                    DiagnosticSeverity::Warning
                } else {
                    DiagnosticSeverity::Error
                },
                message: error.message,
                code: None,
                source: Some("cargo check (aft_inspect)".into()),
            });
    }
    Ok((reports, finished))
}

fn accept_saved(saved: &SavedCheck, current: Fingerprint) -> Option<SavedCheck> {
    if current.digest != saved.fingerprint.digest {
        return None;
    }
    let mut saved = saved.clone();
    saved.fingerprint = current;
    Some(saved)
}

fn duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} s")
    } else {
        format!("{} m {} s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::super::diagnostics::DiagnosticSeverity;
    use super::*;

    // Shared fixtures seed a completed check; scheduler delays should not decide
    // whether tests reach the behavior they are meant to exercise.
    #[cfg(unix)]
    const LIVE_FIXTURE_CAPTURE_BUDGET: Duration = Duration::from_secs(60);

    fn write(root: &Path, path: &str, content: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, CompletedRustCheck) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        write(&root, "Cargo.toml", "[workspace]\nmembers = [\"member\"]\n");
        write(&root, "Cargo.lock", "version = 4\n");
        write(
            &root,
            "member/Cargo.toml",
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        );
        write(&root, "member/src/lib.rs", "pub fn a() {}\n");
        write(&root, "member/build.rs", "fn main() {}\n");
        write(&root, "member/tests/check.rs", "#[test] fn a() {}\n");
        write(&root, "member/benches/check.rs", "fn main() {}\n");
        write(&root, "member/examples/check.rs", "fn main() {}\n");
        let runtime = Runtime {
            binary: "rust-analyzer".into(),
            args: vec![],
            env: HashMap::new(),
            options: None,
            launch_env: None,
            validation_gate: None,
        };
        let mut cache =
            CompletedRustCheck::new(&root, &root, &temp.path().join("storage"), runtime).unwrap();
        let fingerprint = fingerprint(
            &cache.root,
            "v1".into(),
            None,
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        cache.saved = Some(Arc::new(SavedCheck {
            schema: RECORD_SCHEMA,
            root: cache.root.clone(),
            checkout: cache.checkout.clone(),
            fingerprint,
            diagnostics: BTreeMap::new(),
            completed_seconds: 77477,
            duration_seconds: 217,
        }));
        write_record(&cache.path, cache.saved.as_ref().unwrap()).unwrap();
        let reloaded = reload(&cache);
        assert!(
            reloaded.saved.is_some(),
            "control: fixture survives disk round trip"
        );
        (temp, reloaded)
    }

    fn reload(cache: &CompletedRustCheck) -> CompletedRustCheck {
        CompletedRustCheck::new(
            &cache.root,
            &cache.checkout,
            cache.path.parent().unwrap().parent().unwrap(),
            cache.runtime.clone(),
        )
        .unwrap()
    }

    fn valid(cache: &CompletedRustCheck) -> Option<SavedCheck> {
        cache.validated_with_runtime("v1".into(), Instant::now() + BUDGET)
    }

    #[test]
    fn next_write_removes_record_for_deleted_checkout() {
        let (temp, cache) = fixture();
        let mut dead = (**cache.saved.as_ref().unwrap()).clone();
        dead.checkout = temp.path().join("deleted-checkout");
        fs::create_dir(&dead.checkout).unwrap();
        let dead_path = cache
            .path
            .with_file_name(format!("{}.json", blake3::hash(b"dead")));
        write_record(&dead_path, &dead).unwrap();
        assert!(
            dead_path.is_file(),
            "control: the live checkout is retained"
        );
        fs::remove_dir(&dead.checkout).unwrap();
        write_record(&cache.path, cache.saved.as_ref().unwrap()).unwrap();
        assert!(!dead_path.exists(), "a write must sweep deleted checkouts");
        assert!(cache.path.is_file());
    }

    #[test]
    fn record_cap_evicts_least_recently_validated() {
        let (_temp, cache) = fixture();
        fs::remove_file(&cache.path).unwrap();
        let record = cache.saved.as_ref().unwrap();
        let mut paths = Vec::new();
        for index in 0..64 {
            let path = cache.path.with_file_name(format!(
                "{}.json",
                blake3::hash(index.to_string().as_bytes())
            ));
            write_record(&path, record).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(UNIX_EPOCH + Duration::from_secs(100 + index)),
                )
                .unwrap();
            paths.push(path);
        }
        // Completion order is not validation recency: the oldest completion was
        // just validated again, so the next-oldest validation must be evicted.
        fs::OpenOptions::new()
            .write(true)
            .open(&paths[0])
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1000)))
            .unwrap();
        write_record(&cache.path, record).unwrap();
        assert!(paths[0].is_file());
        assert!(
            !paths[1].exists(),
            "least recently validated record must be evicted"
        );
        assert!(paths[2..].iter().all(|path| path.is_file()));
        assert!(cache.path.is_file());
        assert_eq!(
            fs::read_dir(cache.path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count(),
            64
        );
    }

    #[test]
    fn record_sweep_counts_non_records_inside_scan_limit() {
        let (_temp, cache) = fixture();
        let directory = cache.path.parent().unwrap();
        for index in 0..10 {
            fs::write(directory.join(format!("not-a-record-{index}")), "").unwrap();
        }
        let sweep = sweep_records(directory, &cache.path, 3);
        assert_eq!(sweep.examined, 3);
        assert!(!sweep.complete);
        assert_eq!(sweep.removed, 0);
    }

    #[test]
    fn record_ledger_uses_relative_paths_and_compact_stamps() {
        let (_temp, cache) = fixture();
        write_record(&cache.path, cache.saved.as_ref().unwrap()).unwrap();
        let disk: serde_json::Value =
            serde_json::from_slice(&fs::read(&cache.path).unwrap()).unwrap();
        let ledger = disk["record"]["fingerprint"]["files"].as_object().unwrap();
        // PathBuf's JSON spelling uses the host separator; the ledger's
        // contract is a relative workspace path, not a Unix-only string.
        let relative = Path::new("member").join("src").join("lib.rs");
        let key = relative.to_str().unwrap();
        assert!(
            ledger.contains_key(key),
            "workspace input paths must be relative"
        );
        let entry = ledger[key]
            .as_array()
            .expect("stamps must not repeat field names for every input");
        assert_eq!(
            entry[3].as_str().unwrap().len(),
            64,
            "one fixed-size hex digest per input"
        );
        let reloaded = CompletedRustCheck::new(
            &cache.root,
            &cache.checkout,
            cache.path.parent().unwrap().parent().unwrap(),
            cache.runtime.clone(),
        )
        .unwrap();
        assert!(valid(&reloaded).is_some());
        assert!(reloaded
            .saved
            .as_ref()
            .unwrap()
            .covers(&cache.root.join("member/src/lib.rs")));
    }

    fn legacy_record_text(record: &SavedCheck) -> (String, usize) {
        let legacy_files: BTreeMap<_, _> = record
            .fingerprint
            .files
            .iter()
            .map(|(path, stamp)| {
                (
                    path,
                    serde_json::json!({
                        "size": stamp.size, "modified": stamp.modified, "changed": stamp.changed,
                        "hash": stamp.hash, "included": stamp.included, "env_keys": stamp.env_keys,
                    }),
                )
            })
            .collect();
        let ledger_bytes = serde_json::to_vec(&legacy_files).unwrap().len();
        let mut legacy = serde_json::to_value(record).unwrap();
        legacy["schema"] = 1.into();
        legacy["fingerprint"]["files"] = serde_json::to_value(&legacy_files).unwrap();
        legacy["fingerprint"]["extra_inputs"] =
            serde_json::to_value(&record.fingerprint.extra_inputs).unwrap();
        let checksum = blake3::hash(&serde_json::to_vec(&legacy).unwrap())
            .to_hex()
            .to_string();
        let text =
            serde_json::to_string(&serde_json::json!({"record": legacy, "checksum": checksum}))
                .unwrap();
        (text, ledger_bytes)
    }

    #[test]
    fn fixed_fixture_compact_record_has_exact_encoding_savings() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("size-workspace");
        write(
            &root,
            "Cargo.toml",
            "[package]\nname = \"size-fixture\"\nversion = \"0.1.0\"\n",
        );
        write(&root, "Cargo.lock", "version = 4\n");
        write(&root, "src/lib.rs", "pub fn value() -> u32 { 1 }\n");
        let mut inputs = vec![
            PathBuf::from("Cargo.toml"),
            PathBuf::from("Cargo.lock"),
            PathBuf::from("src/lib.rs"),
        ];
        for index in 0..8 {
            let path = format!("src/module_{index}.rs");
            write(&root, &path, "pub fn value() -> u32 { 1 }\n");
            inputs.push(PathBuf::from(path));
        }
        inputs.sort();
        let root = canonical(&root).unwrap();
        let mut fingerprint = fingerprint(
            &root,
            "size-fixture".into(),
            None,
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        // Only the fixed workspace inputs belong to this encoding fixture;
        // ancestor and user-home Cargo configuration must not change its size.
        fingerprint.files.retain(|path, _| path.starts_with(&root));
        assert_eq!(
            fingerprint
                .files
                .keys()
                .map(|path| path.strip_prefix(&root).unwrap().to_path_buf())
                .collect::<Vec<_>>(),
            inputs
        );
        assert!(fingerprint
            .files
            .values()
            .all(|stamp| stamp.included.is_empty() && stamp.env_keys.is_empty()));
        let record = SavedCheck {
            schema: RECORD_SCHEMA,
            root: root.clone(),
            checkout: root.clone(),
            fingerprint,
            diagnostics: BTreeMap::new(),
            completed_seconds: 1,
            duration_seconds: 1,
        };
        let path = temp
            .path()
            .join("storage/rust-completed-checks")
            .join(format!("{}.json", blake3::hash(b"size-fixture")));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (legacy_text, _) = legacy_record_text(&record);
        crate::jsonc_edit::write_atomic(&path, &legacy_text).unwrap();
        let before_bytes = fs::metadata(&path).unwrap().len();
        write_record(&path, &record).unwrap();
        let after_bytes = fs::metadata(&path).unwrap().len();
        // Six JSON property names and their colons cost exactly 57 bytes per
        // stamp. Known fixture paths independently pin the prefix savings;
        // variable mtimes, temp-directory names and platform escaping cancel.
        let expected_savings = 57 * inputs.len() as u64
            + inputs
                .iter()
                .map(|input| {
                    (serde_json::to_vec(&root.join(input)).unwrap().len()
                        - serde_json::to_vec(input).unwrap().len()) as u64
                })
                .sum::<u64>();
        assert_eq!(
            after_bytes + expected_savings,
            before_bytes,
            "compact encoding must remove every repeated field name and workspace prefix"
        );
        println!("fixed-fixture completed-check record: before {before_bytes} bytes, after {after_bytes} bytes; expected savings {expected_savings} bytes for {} inputs", inputs.len());
    }

    #[test]
    fn compact_record_preserves_includes_build_inputs_and_external_config() {
        let (temp, mut cache) = fixture();
        write(&cache.root, "member/src/data.md", "before");
        write(
            &cache.root,
            "member/src/lib.rs",
            "pub const DATA: &str = include_str!(\"data.md\");\n",
        );
        write(&cache.root, "member/data/one.txt", "one");
        let external = temp.path().join("external-config");
        fs::write(&external, "external cargo configuration").unwrap();
        let mut seed = cache.saved.as_ref().unwrap().fingerprint.clone();
        seed.extra_inputs
            .extend([cache.root.join("member/data"), external.clone()]);
        seed.extra_env_keys
            .push("AFT_COMPLETED_CHECK_ROUND_TRIP_TEST".into());
        cache.saved_mut().fingerprint = fingerprint(
            &cache.root,
            "v1".into(),
            Some(&seed),
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        write_record(&cache.path, cache.saved.as_ref().unwrap()).unwrap();
        let reloaded = reload(&cache);
        let original = &cache.saved.as_ref().unwrap().fingerprint;
        let restored = &reloaded.saved.as_ref().unwrap().fingerprint;
        assert_eq!(original.files, restored.files);
        assert_eq!(original.extra_inputs, restored.extra_inputs);
        assert_eq!(original.extra_env_keys, restored.extra_env_keys);
        assert!(valid(&reloaded).is_some());
        fs::write(&external, "changed configuration").unwrap();
        assert!(valid(&reloaded).is_none());
        fs::write(&external, "external cargo configuration").unwrap();
        assert!(valid(&reloaded).is_some());
        write(&cache.root, "member/src/data.md", "after!");
        assert!(valid(&reloaded).is_none());
        write(&cache.root, "member/src/data.md", "before");
        assert!(valid(&reloaded).is_some());
        write(&cache.root, "member/data/two.txt", "two");
        assert!(valid(&reloaded).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn successful_validation_refreshes_record_recency_but_invalid_validation_does_not() {
        let (_temp, cache) = live_fixture();
        let before = UNIX_EPOCH + Duration::from_secs(100);
        let reset = || {
            fs::OpenOptions::new()
                .write(true)
                .open(&cache.path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(before))
                .unwrap()
        };
        reset();
        assert!(cache.validated(Instant::now() + BUDGET).is_some());
        assert!(fs::metadata(&cache.path).unwrap().modified().unwrap() > before);
        reset();
        write(&cache.root, "member/src/lib.rs", "pub fn changed() {}\n");
        assert!(cache.validated(Instant::now() + BUDGET).is_none());
        assert_eq!(
            fs::metadata(&cache.path).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn member_rust_edit_invalidates_completed_check() {
        let (_temp, cache) = fixture();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/src/lib.rs", "pub fn b() {}\n");
        assert!(valid(&cache).is_none());
    }
    #[test]
    fn build_script_edit_invalidates_completed_check() {
        let (_temp, cache) = fixture();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/build.rs", "fn main() { panic!(); }\n");
        assert!(valid(&cache).is_none());
    }
    #[test]
    fn test_rust_edit_invalidates_completed_check() {
        let (_temp, cache) = fixture();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/tests/check.rs", "#[test] fn b() {}\n");
        assert!(valid(&cache).is_none());
    }
    #[test]
    fn equal_size_and_mtime_rust_edit_invalidates_completed_check() {
        let (_temp, cache) = fixture();
        let path = cache.root.join("member/src/lib.rs");
        let meta = fs::metadata(&path).unwrap();
        write(&cache.root, "member/src/lib.rs", "pub fn b() {}\n");
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_last_modification_time(&meta),
        )
        .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), meta.len());
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            meta.modified().unwrap()
        );
        assert!(valid(&cache).is_none());
    }
    #[test]
    fn cargo_lock_edit_invalidates_completed_check() {
        let (_temp, cache) = fixture();
        write(&cache.root, "Cargo.lock", "version = 3\n");
        assert!(valid(&cache).is_none());
    }
    #[test]
    fn cargo_manifest_edit_invalidates_completed_check() {
        let (_temp, cache) = fixture();
        write(
            &cache.root,
            "member/Cargo.toml",
            "[package]\nname = \"member\"\nversion = \"0.2.0\"\n",
        );
        assert!(valid(&cache).is_none());
    }
    #[cfg(unix)]
    #[test]
    fn toolchain_version_change_invalidates_completed_check() {
        let (temp, mut cache) = fixture();
        let binary = temp.path().join("compiler");
        crate::executable_cache::write_executable(&binary, b"#!/bin/sh\nprintf 'compiler 1\\n'\n")
            .unwrap();
        cache.runtime.binary = binary.clone();
        let analyzer = temp.path().join("analyzer");
        crate::executable_cache::write_executable(
            &analyzer,
            b"#!/bin/sh\nprintf 'analyzer 1\\n'\n",
        )
        .unwrap();
        cache.runtime.binary = analyzer;
        cache
            .runtime
            .env
            .insert("RUSTC".into(), binary.to_string_lossy().into_owned());
        cache.runtime.launch_env = Some(cache.runtime.effective_env().unwrap());
        let runtime = cache
            .runtime
            .hash(&cache.root, Instant::now() + BUDGET)
            .unwrap();
        cache.saved_mut().fingerprint =
            fingerprint(&cache.root, runtime, None, Instant::now() + BUDGET, None).unwrap();
        assert!(cache.validated(Instant::now() + BUDGET).is_some());
        crate::executable_cache::write_executable(&binary, b"#!/bin/sh\nprintf 'compiler 2\\n'\n")
            .unwrap();
        assert!(cache.validated(Instant::now() + BUDGET).is_none());
    }
    #[test]
    fn unrelated_markdown_and_typescript_do_not_invalidate_completed_check() {
        let (_temp, cache) = fixture();
        write(&cache.root, "README.md", "new documentation");
        write(&cache.root, "app.ts", "throw new Error();");
        let saved = valid(&cache).unwrap();
        #[cfg(unix)]
        assert_eq!(
            saved.fingerprint.hashed, 2,
            "only manifests need re-reading on Unix"
        );
        #[cfg(not(unix))]
        assert!(saved.fingerprint.hashed >= 2);
    }
    #[test]
    fn saved_errors_are_served_as_errors() {
        let (_temp, mut cache) = fixture();
        let file = cache.root.join("member/src/lib.rs");
        cache.saved_mut().diagnostics.insert(
            file.clone(),
            vec![StoredDiagnostic {
                file: file.clone(),
                line: 1,
                column: 1,
                end_line: 1,
                end_column: 2,
                severity: DiagnosticSeverity::Error,
                message: "compiler error".into(),
                code: Some("E0425".into()),
                source: Some("rustc".into()),
            }],
        );
        let saved = valid(&cache).unwrap();
        assert_eq!(
            saved.diagnostics[&file][0].severity,
            DiagnosticSeverity::Error
        );
        assert_eq!(saved.diagnostics[&file][0].message, "compiler error");
    }
    #[test]
    fn corrupt_and_torn_completed_records_are_refused() {
        let (_temp, cache) = fixture();
        fs::create_dir_all(cache.path.parent().unwrap()).unwrap();
        let load = || {
            CompletedRustCheck::new(
                &cache.root,
                &cache.checkout,
                cache.path.parent().unwrap().parent().unwrap(),
                cache.runtime.clone(),
            )
            .unwrap()
        };
        let record = (**cache.saved.as_ref().unwrap()).clone();
        let envelope = Envelope {
            checksum: blake3::hash(&serde_json::to_vec(&record).unwrap())
                .to_hex()
                .to_string(),
            record,
        };
        let text = serde_json::to_string(&envelope).unwrap();
        crate::jsonc_edit::write_atomic(&cache.path, &text).unwrap();
        assert!(
            valid(&load()).is_some(),
            "control: an intact saved record loads"
        );
        let mut corrupt = envelope;
        corrupt.record.duration_seconds += 1;
        crate::jsonc_edit::write_atomic(&cache.path, &serde_json::to_string(&corrupt).unwrap())
            .unwrap();
        assert!(load().saved.is_none());
        fs::write(&cache.path, "{\"record\":").unwrap();
        assert!(load().saved.is_none());
    }

    #[test]
    fn schema_two_record_is_a_cache_miss_and_normal_write_replaces_it() {
        let (_temp, cache) = fixture();
        let certified = cache.saved.as_ref().unwrap();
        let mut old = (**certified).clone();
        old.schema = 2;
        write_record(&cache.path, &old).unwrap();
        let legacy_bytes = fs::read(&cache.path).unwrap();

        // Loading still constructs the completed-check state normally. Only the
        // cached authority is absent, so the caller runs the compiler instead
        // of treating an obsolete record as an analyzer or storage failure.
        let missed = reload(&cache);
        assert!(
            missed.saved.is_none(),
            "schema 2 cannot certify diagnostics"
        );
        assert!(valid(&missed).is_none());
        assert!(missed.pending.is_none());
        assert_eq!(missed.path, cache.path);
        assert_eq!(fs::read(&missed.path).unwrap(), legacy_bytes);

        // The regular atomic writer replaces the obsolete record in the same
        // namespace; no migration or special cleanup path is required.
        write_record(&missed.path, certified).unwrap();
        let disk: serde_json::Value =
            serde_json::from_slice(&fs::read(&missed.path).unwrap()).unwrap();
        assert_eq!(disk["record"]["schema"], RECORD_SCHEMA);
        assert!(valid(&reload(&missed)).is_some());
    }

    #[test]
    fn schema_three_record_without_source_facts_is_a_cache_miss() {
        let (_temp, cache) = fixture();
        let mut old = (**cache.saved.as_ref().unwrap()).clone();
        old.schema = 3;
        write_record(&cache.path, &old).unwrap();
        assert!(
            reload(&cache).saved.is_none(),
            "older records do not prove which macro names skipped sources were checked against"
        );
    }

    #[test]
    fn fingerprint_deadline_exceeded_refuses_saved_check() {
        let (_temp, cache) = fixture();
        assert!(cache
            .validated_with_runtime("v1".into(), Instant::now())
            .is_none());
    }
    #[test]
    fn external_path_dependency_refuses_completed_check() {
        let (_temp, cache) = fixture();
        fs::create_dir(cache.root.parent().unwrap().join("external")).unwrap();
        write(
            &cache.root,
            "member/Cargo.toml",
            "[dependencies]\nexternal = {path = \"../../external\"}\n",
        );
        assert!(valid(&cache).is_none());
    }
    #[test]
    #[cfg(unix)]
    fn inspect_refuses_build_inputs_changed_before_their_discovery() {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        if !Command::new(&cargo)
            .arg("--version")
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipping inspect_refuses_build_inputs_changed_before_their_discovery: cargo unavailable");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='inspect-build-input'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        fs::write(root.join("input.txt"), "original").unwrap();
        fs::write(root.join("build.rs"), "fn main() { let _ = std::fs::read_to_string(\"input.txt\").unwrap(); std::fs::write(\"input.txt\", \"changed\").unwrap(); println!(\"cargo:rerun-if-changed=input.txt\"); }\n").unwrap();
        assert!(Command::new(&cargo)
            .current_dir(&root)
            .args(["generate-lockfile", "--offline"])
            .status()
            .unwrap()
            .success());
        let runtime = Runtime {
            binary: crate::fake_lsp_test_helper::fake_server_binary(),
            args: vec![],
            env: HashMap::new(),
            options: Some(serde_json::json!({"checkOnSave": false, "cargo": {"targetDir": true}})),
            launch_env: None,
            validation_gate: None,
        };
        let mut check =
            CompletedRustCheck::new(&root, &root, &temp.path().join("storage"), runtime).unwrap();
        let result = check.run_for_inspect(Instant::now() + Duration::from_secs(30), || false);
        assert_eq!(
            fs::read_to_string(root.join("input.txt")).unwrap(),
            "changed",
            "control: the build script must run"
        );
        assert!(
            result.is_err(),
            "an input changed after being read must not certify a clean check"
        );
        assert!(check.saved.is_none());
    }

    #[test]
    fn saved_duration_is_in_running_wording() {
        let (_temp, mut cache) = fixture();
        cache.started = Some(Instant::now() - Duration::from_secs(40));
        assert_eq!(cache.running_reason().unwrap(), "rust-analyzer: cargo check running for 40 s; the last full check here took 3 m 37 s; retry");
    }

    #[test]
    fn included_non_rust_input_edit_invalidates_completed_check() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/src/data.md", "before");
        write(
            &cache.root,
            "member/src/lib.rs",
            "pub const DATA: &str = include_str!(\"data.md\");\n",
        );
        cache.saved_mut().fingerprint = fingerprint(
            &cache.root,
            "v1".into(),
            None,
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/src/data.md", "after!");
        assert!(valid(&cache).is_none());
    }

    fn capture_inputs(cache: &CompletedRustCheck) -> Option<Fingerprint> {
        fingerprint(
            &cache.root,
            "v1".into(),
            None,
            Instant::now() + BUDGET,
            None,
        )
    }

    #[test]
    fn warm_fingerprint_reuses_saved_source_facts_without_parsing() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/src/data/a.txt", "one");
        write(&cache.root, "member/src/data/b.txt", "two");
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            macro_rules! fixture { ($name:literal) => { include_str!(concat!("data/", $name)) }; }
            const ENV: Option<&str> = option_env!("AFT_TEST_CACHED_SOURCE_FACTS");
        "#,
        );
        write(
            &cache.root,
            "member/src/calls.rs",
            "const DATA: &str = fixture!(\"a.txt\");",
        );
        inputs::take_parses();
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert_eq!(
            inputs::take_parses(),
            2,
            "only the definition and its call need parsing"
        );
        write_record(&cache.path, cache.saved.as_ref().unwrap()).unwrap();
        let reloaded = reload(&cache);
        let original = &cache.saved.as_ref().unwrap().fingerprint;
        let restored = &reloaded.saved.as_ref().unwrap().fingerprint;
        assert_eq!(
            serde_json::to_value(&original.source_facts).unwrap(),
            serde_json::to_value(&restored.source_facts).unwrap()
        );
        assert_eq!(original.macro_names, restored.macro_names);
        inputs::take_parses();
        let warm = fingerprint(
            &cache.root,
            "v1".into(),
            Some(restored),
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert_eq!(
            inputs::take_parses(),
            0,
            "unchanged warm sources must not be parsed"
        );
        assert_eq!(warm.digest, restored.digest);
        write(
            &cache.root,
            "member/src/calls.rs",
            "const DATA: &str = fixture!(\"b.txt\");",
        );
        let changed = fingerprint(
            &cache.root,
            "v1".into(),
            Some(&warm),
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert_eq!(
            inputs::take_parses(),
            1,
            "only the changed call site needs parsing"
        );
        assert!(changed
            .files
            .contains_key(&cache.root.join("member/src/data/b.txt")));
        assert!(!changed
            .files
            .contains_key(&cache.root.join("member/src/data/a.txt")));
    }

    #[test]
    fn byte_prefilter_skips_irrelevant_sources_and_revisits_new_macro_names() {
        let (_temp, cache) = fixture();
        write(
            &cache.root,
            "member/src/calls.rs",
            "const DATA: &str = fixture!(\"a.txt\");",
        );
        write(&cache.root, "member/src/data/a.txt", "one");
        inputs::take_parses();
        let before = capture_inputs(&cache).unwrap();
        assert_eq!(
            inputs::take_parses(),
            0,
            "irrelevant bytes must not reach tree-sitter"
        );
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            macro_rules! fixture { ($name:literal) => { include_str!(concat!("data/", $name)) }; }
        "#,
        );
        let after = fingerprint(
            &cache.root,
            "v1".into(),
            Some(&before),
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert_eq!(
            inputs::take_parses(),
            2,
            "a new definition must discover unchanged callers"
        );
        assert!(after
            .files
            .contains_key(&cache.root.join("member/src/data/a.txt")));
    }

    #[test]
    fn new_macro_names_revisit_previously_cached_primary_files() {
        let (_temp, cache) = fixture();
        write(
            &cache.root,
            "member/src/calls.rs",
            "// env prefilter\nconst DATA: &str = fixture!(\"a.txt\");",
        );
        write(&cache.root, "member/src/data/a.txt", "one");
        inputs::take_parses();
        let before = capture_inputs(&cache).unwrap();
        assert_eq!(inputs::take_parses(), 1);
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            macro_rules! fixture { ($name:literal) => { include_str!(concat!("data/", $name)) }; }
        "#,
        );
        let after = fingerprint(
            &cache.root,
            "v1".into(),
            Some(&before),
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert_eq!(inputs::take_parses(), 2);
        assert!(after
            .files
            .contains_key(&cache.root.join("member/src/data/a.txt")));
    }

    fn unresolved_input(cache: &CompletedRustCheck) -> String {
        let mut unresolved = None;
        assert!(fingerprint_with_reason(
            &cache.root,
            "v1".into(),
            None,
            Instant::now() + BUDGET,
            None,
            &mut unresolved,
        )
        .is_none());
        unresolved
            .expect("the source causing uncertainty must be named")
            .note(&cache.root)
    }

    #[test]
    fn literal_concat_includes_track_exact_files() {
        for name in ["include", "include_str", "include_bytes"] {
            let (_temp, mut cache) = fixture();
            write(&cache.root, "member/src/data/a.txt", "before");
            write(&cache.root, "member/src/data/unused.txt", "unrelated");
            write(
                &cache.root,
                "member/src/lib.rs",
                &format!("const DATA: &str = {name}!(concat!(r#\"data/\"#, \"a\", \".txt\",));\n"),
            );
            cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
            assert!(valid(&cache).is_some());
            write(
                &cache.root,
                "member/src/data/unused.txt",
                "changed unrelated",
            );
            assert!(
                valid(&cache).is_some(),
                "only the exact concat path is an input"
            );
            write(&cache.root, "member/src/data/a.txt", "after!");
            assert!(
                valid(&cache).is_none(),
                "{name} input must invalidate the check"
            );
        }
    }

    #[test]
    fn concat_manifest_dir_uses_nearest_member_manifest() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/data/a.txt", "before");
        write(
            &cache.root,
            "member/src/nested/inputs.rs",
            concat!(
                r#"
            const DATA: &str = include_str!(concat!(env"#,
                r#"!("CARGO_MANIFEST_DIR"), "/data/", "a.txt"));
        "#
            ),
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(cache
            .saved
            .as_ref()
            .unwrap()
            .fingerprint
            .files
            .contains_key(&cache.root.join("member/data/a.txt")));
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/data/a.txt", "after!");
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn reporter_fixture_macro_tracks_all_literal_invocations_across_files() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "tests/fixtures/a.txt", "one");
        write(&cache.root, "tests/fixtures/b.txt", "two");
        write(&cache.root, "tests/fixtures/unread.txt", "not an input");
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            macro_rules! fixture {
                ($fixture:literal) => { include_str!(concat!("../../tests/fixtures/", $fixture)) };
            }
            const A: &str = fixture!("a.txt");
        "#,
        );
        write(
            &cache.root,
            "member/src/calls.rs",
            "const B: &str = fixture!(\"b.txt\");\n",
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(
            &cache.root,
            "tests/fixtures/unread.txt",
            "still not an input",
        );
        assert!(valid(&cache).is_some());
        for (file, original) in [("a.txt", "one"), ("b.txt", "two")] {
            write(&cache.root, &format!("tests/fixtures/{file}"), "changed");
            assert!(
                valid(&cache).is_none(),
                "every resolved invocation is an input"
            );
            write(&cache.root, &format!("tests/fixtures/{file}"), original);
        }
        write(&cache.root, "tests/fixtures/new.txt", "new input");
        write(
            &cache.root,
            "member/src/calls.rs",
            "const B: &str = fixture!(\"new.txt\");\n",
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "tests/fixtures/new.txt", "changed new input");
        assert!(
            valid(&cache).is_none(),
            "unchanged definitions must use the current calls"
        );
    }

    #[test]
    fn macro_paths_and_manifest_dir_are_relative_to_the_invocation() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/tests/data/a.txt", "before");
        write(&cache.root, "member/data/b.txt", "before");
        write(
            &cache.root,
            "member/src/lib.rs",
            concat!(
                r#"
            macro_rules! relative { ($name:literal) => { include_bytes!(concat!("data/", $name)) }; }
            macro_rules! absolute { ($name:literal) => { include_str!(concat!(env"#,
                r#"!("CARGO_MANIFEST_DIR"), "/data/", $name)) }; }
        "#
            ),
        );
        write(
            &cache.root,
            "member/tests/check.rs",
            r#"
            const A: &[u8] = relative!("a.txt");
            const B: &str = absolute!("b.txt");
        "#,
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/tests/data/a.txt", "after!");
        assert!(valid(&cache).is_none());
        write(&cache.root, "member/tests/data/a.txt", "before");
        write(&cache.root, "member/data/b.txt", "after!");
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn unused_fixture_macro_contributes_no_include_input() {
        let (_temp, cache) = fixture();
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            macro_rules! fixture { ($fixture:literal) => { include_str!(concat!("missing/", $fixture)) }; }
        "#,
        );
        assert!(capture_inputs(&cache).is_some());
    }

    #[test]
    fn macro_reexports_without_renaming_keep_exact_invocations() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/data/a.txt", "before");
        write(
            &cache.root,
            "member/src/lib.rs",
            concat!(
                r#"
            macro_rules! fixture { ($name:literal) => { include_str!(concat!(env"#,
                r#"!("CARGO_MANIFEST_DIR"), "/data/", $name)) }; }
            pub(crate) use fixture;
        "#
            ),
        );
        write(
            &cache.root,
            "member/tests/check.rs",
            r#"
            use member::fixture;
            const DATA: &str = member::fixture!("a.txt");
        "#,
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/data/a.txt", "after!");
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn unresolved_include_names_its_source_with_approved_wording() {
        let (_temp, cache) = fixture();
        for source in [
            "const DATA: &str = include_str!(some_macro!());",
            "const DATA: &str = include_str!(concat!(\"data/\", some_macro!()));",
            "const DATA: &str = include_str!(concat!(\"data/\", \"missing.txt\"));",
        ] {
            write(&cache.root, "member/src/lib.rs", source);
            assert_eq!(unresolved_input(&cache), "cargo check completed, but member/src/lib.rs includes a file whose path can't be resolved; diagnostics are provisional");
        }
    }

    #[test]
    fn non_literal_environment_keys_name_their_source_with_approved_wording() {
        let (_temp, cache) = fixture();
        for source in [
            "const DATA: &str = env!(key!());",
            "const DATA: Option<&str> = option_env!(concat!(\"A\", \"B\"));",
            "const DATA: &str = include_str!(concat!(env!(key!()), \"/data.txt\"));",
        ] {
            write(&cache.root, "member/src/lib.rs", source);
            assert_eq!(unresolved_input(&cache), "cargo check completed, but member/src/lib.rs reads an environment variable whose name can't be resolved; diagnostics are provisional");
        }
    }

    #[test]
    fn unresolved_fixture_invocations_stay_provisional() {
        for invocation in [
            "fixture!(name)",
            "fixture!(concat!(\"a\", \".txt\"))",
            "forward!(fixture!(\"a.txt\"))",
            "macro_rules! forward { () => { fixture!(\"a.txt\") }; }",
            "macro_rules! forward { ($($name:literal),*) => { $(fixture!($name);)* }; } forward!(\"a.txt\");",
            "forward!(fixture)",
            "use fixture as alias; alias!(\"a.txt\")",
        ] {
            let (_temp, cache) = fixture();
            write(&cache.root, "member/src/data/a.txt", "input");
            write(
                &cache.root,
                "member/src/lib.rs",
                r#"
                macro_rules! fixture { ($fixture:literal) => { include_str!(concat!("data/", $fixture)) }; }
            "#,
            );
            write(&cache.root, "member/src/calls.rs", invocation);
            assert_eq!(unresolved_input(&cache), "cargo check completed, but member/src/lib.rs includes a file whose path can't be resolved; diagnostics are provisional", "{invocation}");
        }
    }

    #[test]
    fn literal_macro_traversal_is_resolved_exactly_or_refused_outside_workspace() {
        let (temp, mut cache) = fixture();
        write(&cache.root, "member/src/data/placeholder.txt", "not read");
        write(&cache.root, "member/src/outside.txt", "before");
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            macro_rules! fixture { ($fixture:literal) => { include_str!(concat!("data/", $fixture)) }; }
            const DATA: &str = fixture!("../outside.txt");
        "#,
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/src/outside.txt", "after!");
        assert!(valid(&cache).is_none());
        fs::write(temp.path().join("outside.txt"), "external").unwrap();
        write(
            &cache.root,
            "member/src/calls.rs",
            "const DATA: &str = fixture!(\"../../../../outside.txt\");\n",
        );
        assert_eq!(unresolved_input(&cache), "cargo check completed, but member/src/lib.rs includes a file whose path can't be resolved; diagnostics are provisional");
    }

    #[cfg(unix)]
    #[test]
    fn computed_include_refuses_symlink_components() {
        let (temp, cache) = fixture();
        let input = temp.path().join("external.txt");
        fs::write(&input, "external").unwrap();
        fs::create_dir_all(cache.root.join("target")).unwrap();
        std::os::unix::fs::symlink(input, cache.root.join("target/link.txt")).unwrap();
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            const DATA: &str = include_str!(concat!("../../target/", "link.txt"));
        "#,
        );
        assert_eq!(unresolved_input(&cache), "cargo check completed, but member/src/lib.rs includes a file whose path can't be resolved; diagnostics are provisional");
    }

    #[cfg(unix)]
    #[test]
    fn warm_fingerprint_rechecks_excluded_include_parent_symlinks() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "target/inputs/a.txt", "input");
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"const DATA: &str = include_str!(concat!("../../target/inputs/", "a.txt"));"#,
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        fs::rename(
            cache.root.join("target/inputs"),
            cache.root.join("target/actual"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            cache.root.join("target/actual"),
            cache.root.join("target/inputs"),
        )
        .unwrap();
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn complex_fixture_matchers_stay_provisional_when_invoked() {
        for definition in [
            r#"macro_rules! fixture { ($($fixture:literal),*) => { include_str!(concat!("data/", $($fixture),*)) }; }"#,
            r#"macro_rules! fixture { ($fixture:literal) => { include_str!(concat!("data/", $fixture)) }; () => { "empty" }; }"#,
            r#"macro_rules! fixture { ($fixture:literal) => { macro_rules! generated { () => { include_str!(concat!("data/", $fixture)) }; } generated!() }; }"#,
        ] {
            let (_temp, cache) = fixture();
            write(&cache.root, "member/src/data/a.txt", "input");
            write(
                &cache.root,
                "member/src/lib.rs",
                &format!("{definition}\nconst DATA: &str = fixture!(\"a.txt\");"),
            );
            assert_eq!(unresolved_input(&cache), "cargo check completed, but member/src/lib.rs includes a file whose path can't be resolved; diagnostics are provisional");
        }
    }

    #[test]
    fn macro_calls_in_included_rust_with_non_rust_extension_are_scanned() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/src/data/a.txt", "before");
        write(
            &cache.root,
            "member/src/calls.inc",
            "const DATA: &str = fixture!(\"a.txt\");",
        );
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            macro_rules! fixture { ($fixture:literal) => { include_str!(concat!("data/", $fixture)) }; }
            include!(concat!("calls", ".inc"));
        "#,
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/src/data/a.txt", "after!");
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn concat_paths_decode_rust_string_escapes() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/src/data/a.txt", "before");
        write(
            &cache.root,
            "member/src/lib.rs",
            r#"
            const DATA: &str = include_str!(concat!("d\x61ta/", "\u{61}.txt"));
        "#,
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/src/data/a.txt", "after!");
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn unrelated_byte_and_c_string_macro_arguments_do_not_make_inputs_provisional() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/src/data.txt", "before");
        write(
            &cache.root,
            "member/src/lib.rs",
            r##"
            const DATA: &str = include_str!("data.txt");
            opaque!(b"\xff", br#"raw bytes"#, c"c string", cr#"raw c string"#, "include");
        "##,
        );
        cache.saved_mut().fingerprint = capture_inputs(&cache).unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/src/data.txt", "after!");
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn declared_build_input_directory_addition_invalidates_completed_check() {
        let (_temp, mut cache) = fixture();
        write(&cache.root, "member/data/one.txt", "one");
        let mut seed = cache.saved.as_ref().unwrap().fingerprint.clone();
        seed.extra_inputs.push(cache.root.join("member/data"));
        cache.saved_mut().fingerprint = fingerprint(
            &cache.root,
            "v1".into(),
            Some(&seed),
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert!(valid(&cache).is_some());
        write(&cache.root, "member/data/two.txt", "two");
        assert!(valid(&cache).is_none());
    }

    /// Measures fingerprint cost on the current checkout, not a fixed fixture.
    /// Run manually with `cargo test -p agent-file-tools --lib --
    /// repository_fingerprint_completes_and_reuses_unchanged_hashes --ignored --nocapture`.
    #[test]
    #[ignore = "measures the live checkout; run manually"]
    fn repository_fingerprint_completes_and_reuses_unchanged_hashes() {
        let root =
            canonical(&PathBuf::from(crate::test_support::manifest_dir()).join("../..")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            binary: which::which("rust-analyzer").expect("rust-analyzer version probe is required"),
            args: Vec::new(),
            env: HashMap::new(),
            options: Some(
                serde_json::json!({"cargo":{"targetDir":true,"extraArgs":["--locked"],"metadataExtraArgs":["--locked"]}}),
            ),
            launch_env: None,
            validation_gate: None,
        };
        let cache =
            CompletedRustCheck::new(&root, &root, &temp.path().join("storage"), runtime).unwrap();
        let start = Instant::now();
        let first = cache
            .runtime
            .capture(&root, None, Instant::now() + INITIAL_CAPTURE_BUDGET, None)
            .expect("the repository's Rust inputs must fit the bounded initial capture");
        let cold_ms = start.elapsed().as_millis();
        let start = Instant::now();
        let warm = cache
            .runtime
            .capture(&root, Some(&first), Instant::now() + BUDGET, None)
            .unwrap();
        let warm_ms = start.elapsed().as_millis();
        assert_eq!(first.digest, warm.digest);
        assert!(warm.hashed < first.hashed);
        println!(
            "repository Rust fingerprint: cold {} ms, warm {} ms; examined {}, inputs {}, first hashed {}, warm hashed {}",
            cold_ms, warm_ms,
            warm.examined,
            warm.files.len(),
            first.hashed,
            warm.hashed
        );
        let record = SavedCheck {
            schema: RECORD_SCHEMA,
            root: root.clone(),
            checkout: root.clone(),
            fingerprint: warm,
            diagnostics: BTreeMap::new(),
            completed_seconds: 1,
            duration_seconds: 217,
        };
        // The live checkout provides measurement and validation coverage, not a
        // compression ratio contract: its input paths and contents change over
        // time. A fixed fixture separately pins the encoding's exact savings.
        let (legacy_text, ledger_bytes) = legacy_record_text(&record);
        crate::jsonc_edit::write_atomic(&cache.path, &legacy_text).unwrap();
        let before_bytes = fs::metadata(&cache.path).unwrap().len();
        assert!(
            reload(&cache).saved.is_none(),
            "old schemas require a new certified check"
        );
        write_record(&cache.path, &record).unwrap();
        let after_bytes = fs::metadata(&cache.path).unwrap().len();
        let restored = reload(&cache);
        let saved = restored.saved.as_ref().unwrap();
        assert_eq!(record.fingerprint.files, saved.fingerprint.files);
        assert_eq!(record.fingerprint.digest, saved.fingerprint.digest);
        assert!(restored.validated(Instant::now() + BUDGET).is_some());
        println!(
            "repository completed-check record: before {} bytes (ledger {}), after {} bytes; {} inputs at {}",
            before_bytes, ledger_bytes, after_bytes, record.fingerprint.files.len(),
            cache.path.display()
        );
    }

    #[cfg(unix)]
    fn begin_after_fixture_inputs() -> SystemTime {
        // Simulate a check starting after the fixture's freshly written inputs
        // are unambiguously old, without sleeping or relying on scheduler time.
        SystemTime::now() + Duration::from_secs(3)
    }

    #[cfg(unix)]
    fn live_fixture() -> (tempfile::TempDir, CompletedRustCheck) {
        let (temp, mut cache) = fixture();
        let compiler = temp.path().join("compiler");
        crate::executable_cache::write_executable(
            &compiler,
            b"#!/bin/sh\nprintf 'compiler 1\\n'\n",
        )
        .unwrap();
        cache.runtime.binary = compiler.clone();
        cache
            .runtime
            .env
            .insert("RUSTC".into(), compiler.to_string_lossy().into_owned());
        // The fixture writes its own build-script output here; an inherited
        // Cargo target directory must not redirect that input discovery.
        cache.runtime.env.insert(
            "CARGO_TARGET_DIR".into(),
            cache.root.join("target").to_string_lossy().into_owned(),
        );
        cache.runtime.launch_env = Some(cache.runtime.effective_env().unwrap());
        write(
            &cache.root,
            "target/debug/build/member-control/output",
            "cargo:rerun-if-changed=build.rs\n",
        );
        cache.saved = None;
        fs::remove_file(&cache.path).unwrap();
        cache.begin_until(
            begin_after_fixture_inputs(),
            Instant::now() + LIVE_FIXTURE_CAPTURE_BUDGET,
        );
        assert!(
            cache.pending.is_some(),
            "control: the check has known inputs"
        );
        cache.finished = true;
        let _ = cache.complete_until(Instant::now() + LIVE_FIXTURE_CAPTURE_BUDGET, false);
        assert!(
            cache.path.is_file(),
            "control: a completed unchanged check is actually saved"
        );
        (temp, cache)
    }

    #[cfg(unix)]
    #[test]
    fn analyzer_settings_change_invalidates_completed_check() {
        let (_temp, mut cache) = live_fixture();
        assert!(cache.validated(Instant::now() + BUDGET).is_some());
        cache.runtime.options = Some(serde_json::json!({"check":{"features":["changed-feature"]}}));
        assert!(cache.validated(Instant::now() + BUDGET).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn inspect_uses_new_certificate_while_an_older_validation_is_still_pending() {
        let _env = crate::test_env::process_env_lock();
        let (temp, mut check) = live_fixture();
        let cargo = temp.path().join("cargo");
        crate::executable_cache::write_executable(
            &cargo,
            b"#!/bin/sh\nprintf 'checked\\n' > check-ran\nprintf '%s\\n' '{\"reason\":\"build-finished\",\"success\":true}'\n",
        )
        .unwrap();
        check
            .runtime
            .env
            .insert("CARGO".into(), cargo.to_string_lossy().into_owned());
        check.runtime.launch_env = Some(check.runtime.effective_env().unwrap());
        // A timed-out fingerprint worker remains owned until its reply arrives.
        // Its pending verdict must not veto a newly certified explicit check.
        let (_retained_sender, receiver) = std::sync::mpsc::sync_channel(1);
        *check.validation.lock() = Some(receiver);
        let completed = check
            .run_for_inspect(Instant::now() + Duration::from_secs(30), || false)
            .expect("the new check supplies its own post-check certificate");
        assert_eq!(
            fs::read_to_string(check.root.join("check-ran")).unwrap(),
            "checked\n"
        );
        assert!(completed.covers(&check.root.join("member/src/lib.rs")));
        assert!(completed.diagnostics.is_empty());
        assert!(
            check.validation.lock().is_some(),
            "the older worker is still owned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn analyzer_version_change_invalidates_completed_check() {
        let (temp, mut cache) = live_fixture();
        let analyzer = temp.path().join("analyzer");
        crate::executable_cache::write_executable(
            &analyzer,
            b"#!/bin/sh\nprintf 'analyzer 1\\n'\n",
        )
        .unwrap();
        cache.runtime.binary = analyzer.clone();
        cache.begin(begin_after_fixture_inputs());
        cache.finished = true;
        let _ = cache.complete();
        assert!(cache.validated(Instant::now() + BUDGET).is_some());
        crate::executable_cache::write_executable(
            &analyzer,
            b"#!/bin/sh\nprintf 'analyzer 2\\n'\n",
        )
        .unwrap();
        assert!(cache.validated(Instant::now() + BUDGET).is_none());
    }

    #[test]
    fn cargo_config_edit_invalidates_completed_check() {
        let (_temp, mut cache) = fixture();
        write(
            &cache.root,
            ".cargo/config.toml",
            "[build]\nrustflags = [\"--cfg=before\"]\n",
        );
        cache.saved_mut().fingerprint = fingerprint(
            &cache.root,
            "v1".into(),
            None,
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert!(valid(&cache).is_some());
        write(
            &cache.root,
            ".cargo/config.toml",
            "[build]\nrustflags = [\"--cfg=after\"]\n",
        );
        assert!(valid(&cache).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn check_beginning_before_last_edit_is_not_saved() {
        let (_temp, mut cache) = live_fixture();
        let before = fs::read(&cache.path).unwrap();
        let began = begin_after_fixture_inputs();
        write(
            &cache.root,
            "member/src/lib.rs",
            "pub fn after_begin() {}\n",
        );
        // Model a post-begin edit whose coarse filesystem clock rounded its
        // mtime to before the begin event. Pin the ambiguity explicitly so the
        // test does not depend on which clock tick a fast write lands in.
        let edited = cache.root.join("member/src/lib.rs");
        let ambiguous_mtime = began - Duration::from_secs(1);
        fs::File::open(&edited)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(ambiguous_mtime))
            .unwrap();
        assert_eq!(
            fs::metadata(&edited).unwrap().modified().unwrap(),
            ambiguous_mtime
        );
        cache.begin(began);
        cache.finished = true;
        let _ = cache.complete();
        assert!(
            fs::read(&cache.path).unwrap() == before,
            "a check which began before the edit replaced the authoritative record"
        );
    }

    #[cfg(unix)]
    #[test]
    fn end_of_run_input_revalidation_refuses_changed_inputs() {
        let (_temp, mut cache) = live_fixture();
        let before = fs::read(&cache.path).unwrap();
        let began = begin_after_fixture_inputs();
        cache.begin(began);
        assert!(cache.pending.is_some());
        write(
            &cache.root,
            "member/src/lib.rs",
            "pub fn during_check() {}\n",
        );
        fs::File::open(cache.root.join("member/src/lib.rs"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(began + Duration::from_secs(1)))
            .unwrap();
        cache.reports.insert(
            cache.root.join("member/src/lib.rs"),
            vec![StoredDiagnostic {
                file: cache.root.join("member/src/lib.rs"),
                line: 1,
                column: 1,
                end_line: 1,
                end_column: 2,
                severity: DiagnosticSeverity::Error,
                message: "report from the incomplete run".into(),
                code: None,
                source: Some("rustc".into()),
            }],
        );
        cache.finished = true;
        let _ = cache.complete();
        assert!(
            fs::read(&cache.path).unwrap() == before,
            "an input changed during the check, but its result replaced the saved record"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validation_deadline_returns_unknown_without_waiting_for_slow_fingerprint() {
        let (_temp, mut cache) = live_fixture();
        assert!(
            cache.validated(Instant::now() + BUDGET).is_some(),
            "control: the saved result is valid"
        );
        let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        cache.runtime.validation_gate = Some((entered_tx, release_rx));
        let cache = std::sync::Arc::new(cache);
        // A caller descheduled for longer than its 20 ms deadline returns before
        // spawning the fingerprint worker. That outcome is also correct (no saved
        // diagnostics served), but it skips the path under test, so retry with a
        // fresh deadline until one attempt reaches the gate.
        let mut gated = false;
        for _ in 0..5 {
            let (returned_tx, returned_rx) = crossbeam_channel::bounded(1);
            let attempt = std::sync::Arc::clone(&cache);
            let caller = std::thread::spawn(move || {
                let result = attempt.validated(Instant::now() + Duration::from_millis(20));
                returned_tx.send(result).unwrap();
            });
            crossbeam_channel::select! {
                recv(entered_rx) -> entered => {
                    entered.expect("validation entered the gate");
                    // The caller must return while fingerprinting is still
                    // held. The timeout is only a deadlock guard.
                    assert!(
                        returned_rx
                            .recv_timeout(BUDGET)
                            .expect("validation ignored its deadline")
                            .is_none(),
                        "an incomplete validation served saved diagnostics"
                    );
                    release_tx.send(()).unwrap();
                    caller.join().unwrap();
                    gated = true;
                }
                recv(returned_rx) -> returned => {
                    assert!(
                        returned.expect("caller returned").is_none(),
                        "an expired validation served saved diagnostics"
                    );
                    caller.join().unwrap();
                    // The caller may return before the worker it spawned has
                    // reached the gate. A recorded pending validation means a
                    // worker exists, so this attempt did exercise the path.
                    if cache.validation.lock().is_some() {
                        entered_rx
                            .recv_timeout(BUDGET)
                            .expect("spawned validation reached the gate");
                        release_tx.send(()).unwrap();
                        gated = true;
                    }
                }
            }
            if gated {
                break;
            }
        }
        assert!(
            gated,
            "no attempt reached the fingerprint worker before its deadline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ended_check_does_not_supply_elapsed_time_for_a_later_expected_check() {
        let (_temp, mut cache) = live_fixture();
        assert!(cache.running_reason().is_none());
        cache.begin(SystemTime::now());
        assert!(cache.running_reason().is_some());
        cache.finished = true;
        let _ = cache.complete();
        assert!(cache.running_reason().is_none());
        cache.begin(SystemTime::now());
        cache.abort();
        assert!(cache.running_reason().is_none());
    }

    #[test]
    fn source_env_macro_change_invalidates_completed_check() {
        let _guard = crate::test_env::process_env_lock();
        const KEY: &str = "AFT_TEST_COMPLETED_CHECK_ENV_INPUT";
        let previous = std::env::var_os(KEY);
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(value) = &self.0 {
                    std::env::set_var(KEY, value);
                } else {
                    std::env::remove_var(KEY);
                }
            }
        }
        let _restore = Restore(previous);
        std::env::set_var(KEY, "before");
        let (_temp, mut cache) = fixture();
        write(
            &cache.root,
            "member/src/lib.rs",
            &format!("pub const INPUT: &str = env!(\"{KEY}\");\n"),
        );
        cache.saved_mut().fingerprint = fingerprint(
            &cache.root,
            "v1".into(),
            None,
            Instant::now() + BUDGET,
            None,
        )
        .unwrap();
        assert!(valid(&cache).is_some());
        std::env::set_var(KEY, "after");
        assert!(valid(&cache).is_none());
    }

    #[test]
    fn unrelated_module_launch_nonce_is_not_a_rust_environment_input() {
        assert!(!relevant_environment_key("SUBC_LAUNCH_NONCE"));
        assert!(!relevant_environment_key("CORTEXKIT_SESSION_ID"));
        for key in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "RUSTUP_TOOLCHAIN",
            "CARGO_TARGET_DIR",
            "PATH",
            "CC",
        ] {
            assert!(relevant_environment_key(key), "{key}");
        }
    }
}
