//! A completed compiler check, not an idle server, is the authority behind this
//! cache. Disk validation is fail-closed and never installs cached rows into the
//! live diagnostics store: a cold server can publish partial results independently.
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::diagnostics::StoredDiagnostic;

pub(crate) const BUDGET: Duration = Duration::from_secs(2);
// The first completed check has no trusted hash ledger yet. Parsing literal
// include dependencies is a one-time cost; later requests reuse the ledger and
// retain the much smaller serving budget.
const INITIAL_CAPTURE_BUDGET: Duration = Duration::from_secs(15);
// Worktree churn must not turn completed compiler checks into an unbounded disk cache.
const MAX_RECORDS: usize = 64;
const RECORD_SCAN_LIMIT: usize = 1024;

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
        #[cfg(test)]
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

fn source_inputs(
    path: &Path,
    bytes: &[u8],
    deadline: Instant,
) -> Option<(Vec<PathBuf>, Vec<String>)> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.contains("include") && !text.contains("env") {
        return Some((Vec::new(), Vec::new()));
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    )?;
    expired(deadline)?;
    let mut nodes = vec![tree.root_node()];
    let mut included = Vec::new();
    let mut env_keys = Vec::new();
    while let Some(node) = nodes.pop() {
        expired(deadline)?;
        for index in 0..u32::try_from(node.child_count()).ok()? {
            let child = node.child(index)?;
            nodes.push(child);
            if !matches!(child.kind(), "identifier" | "scoped_identifier") {
                continue;
            }
            let name = child.utf8_text(bytes).ok()?.rsplit("::").next()?;
            if !matches!(
                name,
                "include" | "include_str" | "include_bytes" | "env" | "option_env"
            ) {
                continue;
            }
            if node.child(index + 1).is_none_or(|bang| bang.kind() != "!") {
                continue;
            }
            let tokens = node.child(index + 2)?;
            let mut cursor = tokens.walk();
            let values = tokens
                .named_children(&mut cursor)
                .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
                .collect::<Vec<_>>();
            // A generated/dynamic include path is uncertain. Do not guess what
            // concat!, env!, or a procedural macro will read from disk.
            if values.is_empty() || (values.len() != 1 && !matches!(name, "env" | "option_env")) {
                return None;
            }
            let literal = values[0].utf8_text(bytes).ok()?;
            let relative = match values[0].kind() {
                "string_literal" => serde_json::from_str::<String>(literal).ok()?,
                "raw_string_literal" => {
                    let start = literal.find('"')?;
                    let end = literal.rfind('"')?;
                    literal.get(start + 1..end)?.to_string()
                }
                _ => return None,
            };
            if matches!(name, "env" | "option_env") {
                env_keys.push(relative);
            } else {
                included.push(canonical(&path.parent()?.join(relative))?);
            }
        }
    }
    Some((included, env_keys))
}

pub(crate) fn fingerprint(
    root: &Path,
    runtime: String,
    previous: Option<&Fingerprint>,
    deadline: Instant,
    // When capturing a check's inputs after reading its begin event, no file
    // or directory may have changed since the reader observed that event.
    began: Option<SystemTime>,
) -> Option<Fingerprint> {
    let mut files = BTreeMap::new();
    let mut report = WalkReport {
        root,
        examined: 0,
        complete: false,
        last: root.to_path_buf(),
        started: Instant::now(),
    };
    let mut hashed = 0;
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
        if began.is_some_and(|begin| {
            fs::metadata(&directory)
                .ok()
                .and_then(|m| m.modified().ok())
                .is_none_or(|time| time > begin)
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
    while let Some(path) = inputs.pop() {
        report.last = path.clone();
        expired(deadline)?;
        if files.contains_key(&path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).ok()?;
        if metadata.is_symlink() {
            return None;
        }
        if metadata.is_dir() {
            if began.is_some_and(|begin| metadata.modified().ok().is_none_or(|time| time > begin)) {
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
        if let Some(begin) = began {
            let begin = begin.duration_since(UNIX_EPOCH).ok()?.as_nanos();
            if current.modified > begin {
                return None;
            }
            #[cfg(unix)]
            if current
                .changed
                .is_none_or(|(s, ns)| s < 0 || (s as u128 * 1_000_000_000 + ns as u128) > begin)
            {
                return None;
            }
        }
        let old = previous.and_then(|p| p.files.get(&path));
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
            if path.extension().is_some_and(|extension| extension == "rs") {
                (current.included, current.env_keys) = source_inputs(&path, &bytes, deadline)?;
            }
            hashed += 1;
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
    Some(Fingerprint {
        files,
        runtime,
        digest,
        examined: report.examined,
        hashed,
        extra_inputs,
        extra_env_keys,
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
    pub(crate) validation_delay: Duration,
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
        let runtime = self.hash(root, deadline)?;
        let mut seed = previous.cloned().unwrap_or(Fingerprint {
            files: BTreeMap::new(),
            runtime: String::new(),
            digest: String::new(),
            examined: 0,
            hashed: 0,
            extra_inputs: Vec::new(),
            extra_env_keys: Vec::new(),
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
        let captured = fingerprint(root, runtime, Some(&seed), deadline, began)?;
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
pub(crate) struct SavedCheck {
    schema: u32,
    root: PathBuf,
    checkout: PathBuf,
    fingerprint: Fingerprint,
    pub(crate) diagnostics: BTreeMap<PathBuf, Vec<StoredDiagnostic>>,
    completed_seconds: u64,
    duration_seconds: u64,
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
                envelope.record.schema == 1
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
        self.began = Some(began);
        self.started = SystemTime::now()
            .duration_since(began)
            .ok()
            .and_then(|elapsed| Instant::now().checked_sub(elapsed));
        self.finished = false;
        let deadline = Instant::now()
            + if self.saved.is_none() {
                INITIAL_CAPTURE_BUDGET
            } else {
                BUDGET
            };
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

    pub(crate) fn complete(&mut self) {
        if !self.finished {
            return;
        }
        self.finished = false;
        let started = self.started.take();
        let Some(pending) = self.pending.take() else {
            return;
        };
        let deadline = Instant::now() + BUDGET;
        let Some(mut current) = self
            .runtime
            .capture(&self.root, Some(&pending), deadline, None)
        else {
            return;
        };
        if current.digest != pending.digest {
            return;
        }
        let Some((build_inputs, env_keys)) =
            self.runtime.build_inputs(&self.root, &current, deadline)
        else {
            return;
        };
        current.extra_inputs.extend(build_inputs);
        current.extra_env_keys.extend(env_keys);
        current.extra_env_keys.sort();
        current.extra_env_keys.dedup();
        current.extra_inputs.sort();
        current.extra_inputs.dedup();
        let Some(current) = self
            .runtime
            .capture(&self.root, Some(&current), deadline, self.began)
        else {
            return;
        };
        #[cfg(not(unix))]
        if current.digest != pending.digest {
            return;
        }
        let record = SavedCheck {
            schema: 1,
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
            self.saved = Some(Arc::new(record));
            #[cfg(test)]
            worker.join().unwrap();
            #[cfg(not(test))]
            drop(worker);
        }
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
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        *active = Some(rx);
        if std::thread::Builder::new()
            .name("aft-rust-fingerprint".into())
            .spawn(move || {
                #[cfg(test)]
                std::thread::sleep(runtime.validation_delay);
                let result = runtime
                    .capture(&root, Some(&saved.fingerprint), deadline, None)
                    .and_then(|current| accept_saved(&saved, current));
                if result.is_some() && expired(deadline).is_some() {
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
            validation_delay: Duration::ZERO,
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
            schema: 1,
            root: cache.root.clone(),
            checkout: cache.checkout.clone(),
            fingerprint,
            diagnostics: BTreeMap::new(),
            completed_seconds: 77477,
            duration_seconds: 217,
        }));
        (temp, cache)
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
        let record = cache.saved.as_ref().unwrap();
        let mut paths = Vec::new();
        for index in 0..64 {
            let path = cache.path.with_file_name(format!(
                "{}.json",
                blake3::hash(index.to_string().as_bytes())
            ));
            write_record(&path, record).unwrap();
            fs::File::open(&path)
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
        fs::File::open(&paths[0])
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

    #[cfg(unix)]
    #[test]
    fn successful_validation_refreshes_record_recency_but_invalid_validation_does_not() {
        let (_temp, cache) = live_fixture();
        let before = UNIX_EPOCH + Duration::from_secs(100);
        let reset = || {
            fs::File::open(&cache.path)
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
        use std::os::unix::fs::PermissionsExt;
        let (temp, mut cache) = fixture();
        let binary = temp.path().join("compiler");
        fs::write(&binary, "#!/bin/sh\nprintf 'compiler 1\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        cache.runtime.binary = binary.clone();
        let analyzer = temp.path().join("analyzer");
        fs::write(&analyzer, "#!/bin/sh\nprintf 'analyzer 1\\n'\n").unwrap();
        fs::set_permissions(&analyzer, fs::Permissions::from_mode(0o755)).unwrap();
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
        fs::write(binary, "#!/bin/sh\nprintf 'compiler 2\\n'\n").unwrap();
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

    #[test]
    fn repository_fingerprint_completes_and_reuses_unchanged_hashes() {
        let root = canonical(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime {
            binary: which::which("rust-analyzer").expect("rust-analyzer version probe is required"),
            args: Vec::new(),
            env: HashMap::new(),
            options: Some(
                serde_json::json!({"cargo":{"targetDir":true,"extraArgs":["--locked"],"metadataExtraArgs":["--locked"]}}),
            ),
            launch_env: None,
            validation_delay: Duration::ZERO,
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
            schema: 1,
            root: root.clone(),
            checkout: root.clone(),
            fingerprint: warm,
            diagnostics: BTreeMap::new(),
            completed_seconds: 1,
            duration_seconds: 217,
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        let envelope = Envelope {
            record,
            checksum: blake3::hash(&bytes).to_hex().to_string(),
        };
        crate::jsonc_edit::write_atomic(&cache.path, &serde_json::to_string(&envelope).unwrap())
            .unwrap();
        println!(
            "repository completed-check record: {} bytes at {}",
            fs::metadata(&cache.path).unwrap().len(),
            cache.path.display()
        );
    }

    #[cfg(unix)]
    fn live_fixture() -> (tempfile::TempDir, CompletedRustCheck) {
        use std::os::unix::fs::PermissionsExt;
        let (temp, mut cache) = fixture();
        let compiler = temp.path().join("compiler");
        fs::write(&compiler, "#!/bin/sh\nprintf 'compiler 1\\n'\n").unwrap();
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
        cache.runtime.binary = compiler.clone();
        cache
            .runtime
            .env
            .insert("RUSTC".into(), compiler.to_string_lossy().into_owned());
        cache.runtime.launch_env = Some(cache.runtime.effective_env().unwrap());
        write(
            &cache.root,
            "target/debug/build/member-control/output",
            "cargo:rerun-if-changed=build.rs\n",
        );
        cache.saved = None;
        cache.begin(SystemTime::now());
        assert!(
            cache.pending.is_some(),
            "control: the check has known inputs"
        );
        cache.finished = true;
        cache.complete();
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
    fn analyzer_version_change_invalidates_completed_check() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, mut cache) = live_fixture();
        let analyzer = temp.path().join("analyzer");
        fs::write(&analyzer, "#!/bin/sh\nprintf 'analyzer 1\\n'\n").unwrap();
        fs::set_permissions(&analyzer, fs::Permissions::from_mode(0o755)).unwrap();
        cache.runtime.binary = analyzer.clone();
        cache.begin(SystemTime::now());
        cache.finished = true;
        cache.complete();
        assert!(cache.validated(Instant::now() + BUDGET).is_some());
        fs::write(analyzer, "#!/bin/sh\nprintf 'analyzer 2\\n'\n").unwrap();
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
        let began = SystemTime::now();
        write(
            &cache.root,
            "member/src/lib.rs",
            "pub fn after_begin() {}\n",
        );
        cache.begin(began);
        cache.finished = true;
        cache.complete();
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
        cache.begin(SystemTime::now());
        assert!(cache.pending.is_some());
        write(
            &cache.root,
            "member/src/lib.rs",
            "pub fn during_check() {}\n",
        );
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
        cache.complete();
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
        cache.runtime.validation_delay = Duration::from_millis(250);
        let started = Instant::now();
        assert!(
            cache
                .validated(started + Duration::from_millis(20))
                .is_none(),
            "an incomplete validation served saved diagnostics"
        );
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "inspect waited past the validation budget"
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
        cache.complete();
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
