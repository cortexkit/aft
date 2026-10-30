use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const SANDBOX_PROFILE_VERSION: u32 = 7;

/// Private stores with narrowly scoped exceptions. These exceptions apply only
/// to these trees, never to the credential floor or user-supplied read denies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxDataPolicy {
    pub deny: Vec<PathBuf>,
    pub read_allow: Vec<PathBuf>,
    pub write_allow: Vec<PathBuf>,
    /// Exact symlink paths on the way to a readable configured directory.
    /// These aliases permit metadata only; directory contents are allowed only
    /// through the resolved `read_allow` targets, and writes stay denied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_metadata_allow: Vec<PathBuf>,
    /// Readable task-file names beneath the calling session, never its environment.
    /// Seatbelt matches names; Landlock snapshots existing files at launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_task_read_dir: Option<PathBuf>,
}

impl SandboxDataPolicy {
    fn canonicalize(
        self,
        exact_files: &[PathBuf],
        preserve_aliases: bool,
    ) -> Result<Self, SandboxProfileError> {
        for path in &self.read_metadata_allow {
            validate_absolute(path, "data_policy.read_metadata_allow")?;
            validate_normalized(path, "data_policy.read_metadata_allow")?;
        }
        Ok(Self {
            deny: canonicalize_denies(
                self.deny,
                "data_policy.deny",
                &mut HashMap::new(),
                preserve_aliases,
            )?,
            read_allow: canonicalize_data_allows(
                self.read_allow,
                exact_files,
                "data_policy.read_allow",
            )?,
            write_allow: canonicalize_data_allows(
                self.write_allow,
                exact_files,
                "data_policy.write_allow",
            )?,
            read_metadata_allow: self.read_metadata_allow,
            session_task_read_dir: self
                .session_task_read_dir
                .map(|path| {
                    let canonical =
                        canonicalize_required_dir(path.clone(), "session_task_read_dir")?;
                    if canonical != path {
                        return Err(SandboxProfileError::new(
                            "session task read directory changed canonical identity",
                        ));
                    }
                    Ok(canonical)
                })
                .transpose()?,
        })
    }

    /// Landlock grants are additive: no emitted rule may encompass private data
    /// or sit inside it unless the entire rule is inside an explicit exception.
    pub fn validate_grants<'a>(
        &self,
        grants: impl IntoIterator<Item = &'a Path>,
        exceptions: &[PathBuf],
        exact_files: &[PathBuf],
    ) -> Result<(), SandboxProfileError> {
        for grant in grants {
            for deny in &self.deny {
                if (grant.starts_with(deny) || deny.starts_with(grant))
                    && !exceptions.iter().any(|allow| {
                        allow.starts_with(deny)
                            && allow != deny
                            && if exact_files.contains(allow) {
                                grant == allow
                            } else {
                                grant.starts_with(allow)
                            }
                    })
                {
                    return Err(SandboxProfileError::new(format!(
                        "Landlock grant {} overlaps private store {} outside an exact carve-out",
                        grant.display(),
                        deny.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Versioned policy transferred to `aft sandbox-launch` by descriptor or private task file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfile {
    /// Deny task-port access for a launch whose proxy holds vault material.
    /// This only tightens the OS profile; it never enables secret resolution.
    #[serde(default, skip_serializing_if = "is_false")]
    pub secret_proxy: bool,
    pub v: u32,
    #[serde(default)]
    pub mode: crate::config::SandboxMode,
    /// Authenticated worker boundary; head shells leave this false.
    #[serde(default)]
    pub worker_jail: bool,
    /// macOS keychain directories denied after all file grants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keychain_deny: Vec<PathBuf>,
    /// Deny reads and writes to credential filenames starting with these paths
    /// (for example ~/.cargo/credentials.toml), including future suffixes.
    #[serde(default)]
    pub deny_prefixes: Vec<PathBuf>,
    #[serde(default)]
    pub data_policy: SandboxDataPolicy,
    pub writable_roots: Vec<PathBuf>,
    /// Single files that may be written or created, without making their
    /// directory writable (a cache's lock files beside configuration that must
    /// stay read-only). Their parent directory must exist. Backends that can
    /// only grant existing files skip one that does not exist yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_files: Vec<PathBuf>,
    /// Mandatory write-deny paths for backends that support nested exclusions.
    #[serde(default)]
    pub write_deny: Vec<PathBuf>,
    pub write_deny_nested: Vec<PathBuf>,
    /// Existing paths that Landlock may grant read access beneath.
    #[serde(default)]
    pub read_allow: Vec<PathBuf>,
    /// Subset of `read_allow` discovered by directory enumeration. Missing entries
    /// may be dropped, but must never be replaced with a broader ancestor grant.
    #[serde(default)]
    pub read_allow_listed: Vec<PathBuf>,
    pub read_deny: Vec<PathBuf>,
    pub socket_deny: Vec<PathBuf>,
    /// Shared compile servers can write outside the client's jail. Seatbelt
    /// denies only these loopback TCP destinations, not ordinary test servers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loopback_tcp_connect_deny: Vec<u16>,
    pub cache_roots: Vec<PathBuf>,
    pub temp_dir: PathBuf,
    /// Set only for worker-jail shells: how this profile keeps the gh shim
    /// from falling back to the operator's gh. See `crate::worker_jail_canary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_gh_fence: Option<WorkerGhFence>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
#[test]
fn secret_bash_ordinary_profile_retains_legacy_bytes() {
    let legacy = r#"{"v":7,"mode":"strict","worker_jail":false,"deny_prefixes":[],"data_policy":{"deny":[],"read_allow":[],"write_allow":[]},"writable_roots":[],"write_deny":[],"write_deny_nested":[],"read_allow":[],"read_allow_listed":[],"read_deny":[],"socket_deny":[],"cache_roots":[],"temp_dir":"/policy"}"#;
    let profile: SandboxProfile = serde_json::from_str(legacy).unwrap();
    assert!(!profile.secret_proxy);
    assert_eq!(serde_json::to_string(&profile).unwrap(), legacy);
}

/// How a worker-jail profile fences the gh shim off the operator's gh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerGhFence {
    /// The jail denies reads and writes of every canary file, including the
    /// account-home anchor. The launcher verifies each deny before execution.
    Canary { paths: Vec<PathBuf> },
    /// This Unix worker profile has no canary deny (off or relaxed Linux). The
    /// launcher reports the missing fence; Windows reports its lack of a native
    /// backend separately through spawn diagnostics.
    Unenforced,
}

impl SandboxProfile {
    /// Build a profile whose existing paths are canonical before serialization.
    ///
    /// Writable roots, read grants, cache roots, and the task temp directory must already
    /// exist. Missing deny targets are normalized through their nearest
    /// existing ancestor so resources created after launch remain enforceable.
    pub fn build(
        writable_roots: Vec<PathBuf>,
        write_deny: Vec<PathBuf>,
        write_deny_nested: Vec<PathBuf>,
        read_allow: Vec<PathBuf>,
        read_deny: Vec<PathBuf>,
        socket_deny: Vec<PathBuf>,
        cache_roots: Vec<PathBuf>,
        temp_dir: PathBuf,
    ) -> Result<Self, SandboxProfileError> {
        // The credential floor appears in both write and read denies. Resolve
        // each path once in this profile's snapshot, never across launches:
        // symlinks and formerly missing paths may change between spawns.
        let mut deny_paths = HashMap::new();
        Ok(Self {
            secret_proxy: false,
            v: SANDBOX_PROFILE_VERSION,
            mode: crate::config::SandboxMode::Strict,
            worker_jail: false,
            keychain_deny: Vec::new(),
            deny_prefixes: Vec::new(),
            data_policy: SandboxDataPolicy::default(),
            writable_roots: canonicalize_required_dirs(writable_roots, "writable_roots")?,
            writable_files: Vec::new(),
            write_deny: canonicalize_optional_paths(write_deny, "write_deny", &mut deny_paths)?,
            write_deny_nested: canonicalize_optional_paths(
                write_deny_nested,
                "write_deny_nested",
                &mut deny_paths,
            )?,
            read_allow: canonicalize_required_paths(read_allow, "read_allow")?,
            read_allow_listed: Vec::new(),
            read_deny: canonicalize_optional_paths(read_deny, "read_deny", &mut deny_paths)?,
            socket_deny: canonicalize_optional_paths(socket_deny, "socket_deny", &mut deny_paths)?,
            loopback_tcp_connect_deny: Vec::new(),
            cache_roots: canonicalize_required_dirs(cache_roots, "cache_roots")?,
            temp_dir: canonicalize_required_dir(temp_dir, "temp_dir")?,
            worker_gh_fence: None,
        })
    }

    /// Revalidate an inherited profile and canonicalize it in the launcher.
    ///
    /// Missing deny targets remain enforceable by resolving their nearest
    /// existing ancestor. Directory grants and ordinary explicit read grants must
    /// exist; exact files need only their parent. Missing inventory and exact-file
    /// read grants are omitted on backends that require existing files.
    pub fn canonicalize_for_launch(self) -> Result<Self, SandboxProfileError> {
        if self.v != SANDBOX_PROFILE_VERSION {
            return Err(SandboxProfileError::new(format!(
                "unsupported sandbox profile version {}; expected {SANDBOX_PROFILE_VERSION}",
                self.v
            )));
        }

        let mut deny_paths = HashMap::new();
        let preserve_aliases = self.mode == crate::config::SandboxMode::Relaxed;
        let writable_files = canonicalize_files(self.writable_files, "writable_files")?;
        let (read_allow, read_allow_listed) =
            canonicalize_read_grants(self.read_allow, self.read_allow_listed, &writable_files)?;
        Ok(Self {
            secret_proxy: self.secret_proxy,
            v: self.v,
            mode: self.mode,
            worker_jail: self.worker_jail,
            keychain_deny: canonicalize_denies(
                self.keychain_deny,
                "keychain_deny",
                &mut deny_paths,
                true,
            )?,
            deny_prefixes: canonicalize_denies(
                self.deny_prefixes,
                "deny_prefixes",
                &mut deny_paths,
                preserve_aliases,
            )?,
            data_policy: self
                .data_policy
                .canonicalize(&writable_files, preserve_aliases)?,
            writable_roots: canonicalize_required_dirs(self.writable_roots, "writable_roots")?,
            writable_files,
            write_deny: canonicalize_denies(
                self.write_deny,
                "write_deny",
                &mut deny_paths,
                preserve_aliases,
            )?,
            write_deny_nested: canonicalize_denies(
                self.write_deny_nested,
                "write_deny_nested",
                &mut deny_paths,
                preserve_aliases,
            )?,
            read_allow,
            read_allow_listed,
            read_deny: canonicalize_denies(
                self.read_deny,
                "read_deny",
                &mut deny_paths,
                preserve_aliases,
            )?,
            socket_deny: canonicalize_optional_paths(
                self.socket_deny,
                "socket_deny",
                &mut deny_paths,
            )?,
            cache_roots: canonicalize_required_dirs(self.cache_roots, "cache_roots")?,
            loopback_tcp_connect_deny: self.loopback_tcp_connect_deny,
            temp_dir: canonicalize_required_dir(self.temp_dir, "temp_dir")?,
            worker_gh_fence: match self.worker_gh_fence {
                // The canary must exist when the jail is applied; a missing one
                // refuses the launch here rather than starting unfenced.
                Some(WorkerGhFence::Canary { paths }) => {
                    if paths.is_empty() {
                        return Err(SandboxProfileError::new(
                            "worker-jail canary paths are empty",
                        ));
                    }
                    Some(WorkerGhFence::Canary {
                        paths: paths
                            .into_iter()
                            .map(|path| {
                                path.canonicalize().map_err(|error| {
                                    SandboxProfileError::new(format!(
                                        "worker-jail canary {} is unavailable: {error}",
                                        path.display()
                                    ))
                                })
                            })
                            .collect::<Result<_, _>>()?,
                    })
                }
                other => other,
            },
        })
    }

    pub fn write_allow_roots(&self) -> Vec<&Path> {
        let mut roots: Vec<&Path> = self
            .writable_roots
            .iter()
            .chain(&self.cache_roots)
            .map(PathBuf::as_path)
            .collect();
        roots.push(self.temp_dir.as_path());
        roots.sort_unstable();
        roots.dedup();
        roots
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfileError {
    message: String,
}

impl SandboxProfileError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SandboxProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxProfileError {}

fn canonicalize_path(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(test)]
    CANONICALIZE_CALLS.with(|count| count.set(count.get() + 1));
    path.canonicalize()
}

#[cfg(test)]
thread_local! {
    static CANONICALIZE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn canonicalize_required_dirs(
    paths: Vec<PathBuf>,
    field: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    paths
        .into_iter()
        .map(|path| canonicalize_required_dir(path, field))
        .collect()
}

fn canonicalize_required_dir(path: PathBuf, field: &str) -> Result<PathBuf, SandboxProfileError> {
    validate_absolute(&path, field)?;
    let canonical = canonicalize_path(&path).map_err(|error| {
        SandboxProfileError::new(format!(
            "{field} path is not an existing directory: {}: {error}",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(SandboxProfileError::new(format!(
            "{field} path is not a directory: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

/// Resolve each file through its existing parent directory, keeping the final
/// name as written: a final symlink is never followed into a wider grant.
fn canonicalize_files(
    paths: Vec<PathBuf>,
    field: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    let mut canonical = Vec::with_capacity(paths.len());
    for path in paths {
        validate_absolute(&path, field)?;
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(SandboxProfileError::new(format!(
                "{field} path has no parent directory: {}",
                path.display()
            )));
        };
        canonical.push(canonicalize_required_dir(parent.to_path_buf(), field)?.join(name));
    }
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

fn canonicalize_required_paths(
    paths: Vec<PathBuf>,
    field: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    let mut canonical = Vec::with_capacity(paths.len());
    for path in paths {
        validate_absolute(&path, field)?;
        canonical.push(canonicalize_path(&path).map_err(|error| {
            SandboxProfileError::new(format!(
                "{field} path does not exist: {}: {error}",
                path.display()
            ))
        })?);
    }
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

/// Exact-file exceptions share the writable-file provenance, so a transient Git
/// lock never becomes a required path or resolves through its final symlink.
fn canonicalize_data_allows(
    paths: Vec<PathBuf>,
    exact_files: &[PathBuf],
    field: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    let mut canonical = Vec::new();
    let mut required = Vec::new();
    for path in paths {
        if let Some(file) = canonicalize_exact_file(&path, exact_files, field)? {
            canonical.push(file);
        } else {
            required.push(path);
        }
    }
    canonical.extend(canonicalize_required_paths(required, field)?);
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

fn canonicalize_exact_file(
    path: &Path,
    exact_files: &[PathBuf],
    field: &str,
) -> Result<Option<PathBuf>, SandboxProfileError> {
    if !exact_files
        .iter()
        .any(|file| file.file_name() == path.file_name())
    {
        return Ok(None);
    }
    let mut files = canonicalize_files(vec![path.to_path_buf()], field)?;
    Ok(files.pop().filter(|file| exact_files.contains(file)))
}

fn canonicalize_read_grants(
    paths: Vec<PathBuf>,
    listed: Vec<PathBuf>,
    exact_files: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), SandboxProfileError> {
    let inventory = listed.into_iter().collect::<BTreeSet<_>>();
    let path_set = paths.iter().collect::<BTreeSet<_>>();
    if inventory.iter().any(|path| !path_set.contains(path)) {
        return Err(SandboxProfileError::new(
            "read_allow_listed must be a subset of read_allow",
        ));
    }
    let mut canonical = Vec::new();
    let mut canonical_listed = Vec::new();
    let mut required = BTreeSet::new();
    for path in paths {
        validate_absolute(&path, "read_allow")?;
        if let Some(resolved) = canonicalize_exact_file(&path, exact_files, "read_allow")? {
            if resolved.is_file() {
                canonical_listed.push(resolved.clone());
                canonical.push(resolved);
            }
            continue;
        }
        let from_listing = inventory.contains(&path);
        let resolved = match canonicalize_path(&path) {
            Ok(resolved) => resolved,
            // Omitting a vanished inventory entry only removes an additive read
            // grant. Explicit read paths still fail closed, even after transfer.
            Err(error) if from_listing && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(SandboxProfileError::new(format!(
                    "read_allow path does not exist: {}: {error}",
                    path.display()
                )))
            }
        };
        if from_listing {
            canonical_listed.push(resolved.clone());
        } else {
            required.insert(resolved.clone());
        }
        canonical.push(resolved);
    }
    canonical.sort_unstable();
    canonical.dedup();
    canonical_listed.sort_unstable();
    canonical_listed.dedup();
    canonical_listed.retain(|path| !required.contains(path));
    Ok((canonical, canonical_listed))
}

fn canonicalize_denies(
    paths: Vec<PathBuf>,
    field: &str,
    resolved: &mut HashMap<PathBuf, PathBuf>,
    preserve_aliases: bool,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    let mut canonical = canonicalize_optional_paths(paths.clone(), field, resolved)?;
    if preserve_aliases {
        // Protect the alias as well as its target: replacing a config symlink
        // must not replace an immutable file with worker-controlled contents.
        for path in paths {
            validate_normalized(&path, field)?;
            canonical.push(path);
        }
        canonical.sort_unstable();
        canonical.dedup();
    }
    Ok(canonical)
}

fn canonicalize_optional_paths(
    paths: Vec<PathBuf>,
    field: &str,
    resolved: &mut HashMap<PathBuf, PathBuf>,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    let mut canonical = Vec::with_capacity(paths.len());
    for path in paths {
        validate_absolute(&path, field)?;
        if let Some(known) = resolved.get(&path) {
            canonical.push(known.clone());
            continue;
        }
        let normalized = match canonicalize_path(&path) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                validate_normalized(&path, field)?;
                canonicalize_missing_path(path.clone(), field)?
            }
            Err(error) => {
                return Err(canonicalization_error(&path, field, &error));
            }
        };
        resolved.insert(path, normalized.clone());
        canonical.push(normalized);
    }
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

/// Resolve an environment deny before profile validation, retaining a normalized
/// alias as well as its target so replacing a config symlink stays forbidden.
#[cfg(unix)]
pub(crate) fn resolve_env_deny_paths(
    path: PathBuf,
    variable: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    validate_absolute(&path, variable)?;
    let resolved = match canonicalize_path(&path) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Resolve the existing ancestor before appending missing components.
            // In particular, do not remove `..` before following symlinks.
            canonicalize_missing_path(path.clone(), variable)?
        }
        Err(error) => return Err(canonicalization_error(&path, variable, &error)),
    };
    validate_normalized(&resolved, variable)?;
    let mut paths = vec![resolved];
    if paths[0] != path && validate_normalized(&path, variable).is_ok() {
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(unix)]
pub(crate) fn resolved_env_deny_paths(variable: &str, path: PathBuf) -> Vec<PathBuf> {
    match resolve_env_deny_paths(path, variable) {
        Ok(paths) => paths,
        Err(error) => {
            log::warn!("skipping sandbox deny from {variable}: {error}");
            Vec::new()
        }
    }
}

fn canonicalize_missing_path(path: PathBuf, field: &str) -> Result<PathBuf, SandboxProfileError> {
    let original = path.clone();
    let mut ancestor = path;
    let mut missing_tail = Vec::new();

    loop {
        match canonicalize_path(&ancestor) {
            Ok(mut canonical) => {
                for component in missing_tail.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&ancestor) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return resolve_dangling_symlinks(&original, field);
                    }
                    Ok(_) => return Err(canonicalization_error(&original, field, &error)),
                    Err(probe_error) if probe_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(probe_error) => {
                        return Err(canonicalization_error(&original, field, &probe_error));
                    }
                }
                let Some(component) = ancestor.file_name().map(ToOwned::to_owned) else {
                    return Err(canonicalization_error(&original, field, &error));
                };
                missing_tail.push(component);
                if !ancestor.pop() {
                    return Err(canonicalization_error(&original, field, &error));
                }
            }
            Err(error) => return Err(canonicalization_error(&original, field, &error)),
        }
    }
}

fn resolve_dangling_symlinks(path: &Path, field: &str) -> Result<PathBuf, SandboxProfileError> {
    const MAX_SYMLINKS: usize = 8;
    let mut pending: VecDeque<_> = path
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    let mut canonical = PathBuf::new();
    let mut visited = BTreeSet::new();
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        match Path::new(&component).components().next().unwrap() {
            Component::ParentDir => {
                canonical.pop();
                continue;
            }
            Component::CurDir => continue,
            _ => canonical.push(&component),
        }
        match std::fs::symlink_metadata(&canonical) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // Include the remaining suffix so a finite path that visits a
                // link twice (for example via `..`) is not mistaken for a loop.
                if !visited.insert((canonical.clone(), pending.clone())) {
                    return Err(SandboxProfileError::new(format!(
                        "symlink loop in {field} path: {}",
                        path.display()
                    )));
                }
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(SandboxProfileError::new(format!(
                        "symlink limit ({MAX_SYMLINKS}) exceeded in {field} path: {}",
                        path.display()
                    )));
                }
                let target = std::fs::read_link(&canonical)
                    .map_err(|error| canonicalization_error(path, field, &error))?;
                if target.to_str().is_none() {
                    return Err(SandboxProfileError::new(format!(
                        "non-UTF-8 symlink target in {field} path: {}",
                        path.display()
                    )));
                }
                // Resolve relative targets against the link's resolved parent.
                // Process components in order: lexical cleanup before following
                // another symlink would give `..` the wrong parent.
                if target.is_absolute() {
                    canonical.clear();
                } else {
                    canonical.pop();
                }
                for part in target.components().rev() {
                    pending.push_front(part.as_os_str().to_owned());
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(canonicalization_error(path, field, &error)),
        }
    }
    // Seatbelt matches the resolved target, not the symlink's spelling.
    // Landlock grants also follow links, but cannot express negative socket
    // rules; the launcher reports that existing limitation separately.
    canonicalize_missing_path(canonical, field)
}

fn canonicalization_error(path: &Path, field: &str, error: &std::io::Error) -> SandboxProfileError {
    #[cfg(unix)]
    let symlink_loop = error.raw_os_error() == Some(libc::ELOOP);
    #[cfg(windows)]
    let symlink_loop = error.raw_os_error() == Some(1921); // ERROR_CANT_RESOLVE_FILENAME
    #[cfg(not(any(unix, windows)))]
    let symlink_loop = false;
    if symlink_loop {
        return SandboxProfileError::new(format!(
            "symlink loop while canonicalizing {field} path {}: {error}",
            path.display()
        ));
    }
    SandboxProfileError::new(format!(
        "failed to canonicalize {field} path {}: {error}",
        path.display()
    ))
}

fn validate_absolute(path: &Path, field: &str) -> Result<(), SandboxProfileError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(SandboxProfileError::new(format!(
            "{field} paths must be absolute: {}",
            path.display()
        )))
    }
}

fn validate_normalized(path: &Path, field: &str) -> Result<(), SandboxProfileError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SandboxProfileError::new(format!(
            "nonexistent {field} paths must be normalized: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_file_exceptions_survive_disappearing_locks_without_widening() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let common = root.join("main/.git");
        std::fs::create_dir_all(&common).unwrap();
        let files =
            ["packed-refs", "packed-refs.lock", "packed-refs.new"].map(|name| common.join(name));
        let mut profile = SandboxProfile::build(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root.clone(),
        )
        .unwrap();
        profile.writable_files = files.to_vec();
        profile.data_policy = SandboxDataPolicy {
            deny: vec![common.clone()],
            read_allow: files.to_vec(),
            write_allow: files.to_vec(),
            read_metadata_allow: Vec::new(),
            session_task_read_dir: None,
        };
        // None of Git's optional files needs to exist at initial profile build.
        let mut profile = profile.canonicalize_for_launch().unwrap();
        for file in &files {
            std::fs::write(file, "temporary Git state").unwrap();
        }
        profile.read_allow = files.to_vec();
        let profile = profile.canonicalize_for_launch().unwrap();
        let bytes = serde_json::to_vec(&profile).unwrap();
        for file in &files {
            std::fs::remove_file(file).unwrap();
        }
        // A lock present at build can be gone when the launcher receives it.
        let transferred: SandboxProfile = serde_json::from_slice(&bytes).unwrap();
        let profile = transferred.canonicalize_for_launch().unwrap();
        assert!(profile.read_allow.is_empty());
        assert_eq!(profile.writable_files.len(), 3);
        for allows in [
            &profile.data_policy.read_allow,
            &profile.data_policy.write_allow,
        ] {
            for file in &files {
                assert!(allows.contains(file));
                profile
                    .data_policy
                    .validate_grants([file.as_path()], allows, &profile.writable_files)
                    .unwrap();
            }
            for path in [
                common.clone(),
                common.join("config"),
                common.join("hooks/post-commit"),
                common.join("worktrees/sibling"),
                common.join("packed-refs.other"),
                common.join("packed-refs.lock/child"),
                common.join("packed-refs.new/child"),
            ] {
                assert!(
                    profile
                        .data_policy
                        .validate_grants([path.as_path()], allows, &profile.writable_files,)
                        .is_err(),
                    "unexpected private-store grant: {path:?}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_file_exceptions_resolve_parent_aliases_but_not_final_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let common = root.join("common");
        let alias = root.join("alias");
        std::fs::create_dir(&common).unwrap();
        symlink(&common, &alias).unwrap();
        let target = root.join("config");
        std::fs::write(&target, "protected").unwrap();
        symlink(&target, common.join("packed-refs.lock")).unwrap();
        let file = alias.join("packed-refs.lock");
        let mut profile = SandboxProfile::build(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root.clone(),
        )
        .unwrap();
        profile.writable_files = vec![common.join("packed-refs.lock")];
        profile.read_allow = vec![file.clone()];
        profile.data_policy = SandboxDataPolicy {
            read_allow: vec![file.clone()],
            write_allow: vec![file],
            ..Default::default()
        };
        let profile = profile.canonicalize_for_launch().unwrap();
        assert_eq!(profile.data_policy.read_allow, profile.writable_files);
        assert_eq!(profile.data_policy.write_allow, profile.writable_files);
        assert_eq!(profile.read_allow, profile.writable_files);
        assert!(!profile.data_policy.read_allow.contains(&target));
    }

    #[test]
    fn private_store_lock_exception_stays_a_single_file_after_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().canonicalize().unwrap().join("store");
        std::fs::create_dir_all(&store).unwrap();
        let lock = store.join("queue.lock");
        std::fs::write(&lock, "lock").unwrap();
        let policy = SandboxDataPolicy {
            deny: vec![store.clone()],
            read_allow: vec![lock.clone()],
            write_allow: vec![lock.clone()],
            read_metadata_allow: Vec::new(),
            session_task_read_dir: None,
        }
        .canonicalize(&[], false)
        .unwrap();
        let transferred: SandboxDataPolicy =
            serde_json::from_slice(&serde_json::to_vec(&policy).unwrap()).unwrap();
        let transferred = transferred.canonicalize(&[], false).unwrap();
        assert_eq!(transferred, policy);
        assert!(policy
            .validate_grants([lock.as_path()], &policy.write_allow, &[])
            .is_ok());
        for path in [&store, &store.join("other.lock")] {
            assert!(policy
                .validate_grants([path.as_path()], &policy.write_allow, &[])
                .is_err());
        }
    }

    #[test]
    fn private_store_grants_require_exact_descendant_carve_outs() {
        let base = tempfile::tempdir().unwrap();
        let store = base.path().join("cortexkit");
        let own = store.join("worktrees/own");
        let io = store.join("aft/tasks/own/io");
        let hooks = store.join("aft/git-hooks/current");
        let policy = SandboxDataPolicy {
            deny: vec![store.clone()],
            read_allow: vec![own.clone(), io.clone(), hooks.clone()],
            write_allow: vec![own.clone(), io.clone()],
            read_metadata_allow: Vec::new(),
            session_task_read_dir: None,
        };
        for allowed in [&own, &io, &own.join("new-file"), &io.join("temp")] {
            policy
                .validate_grants([allowed.as_path()], &policy.read_allow, &[])
                .unwrap();
            policy
                .validate_grants([allowed.as_path()], &policy.write_allow, &[])
                .unwrap();
        }
        policy
            .validate_grants([hooks.as_path()], &policy.read_allow, &[])
            .unwrap();
        for denied in [
            store.clone(),
            base.path().to_path_buf(),
            store.join("worktrees/other"),
            store.join("aft/tasks/other/io"),
            hooks,
        ] {
            assert!(
                policy
                    .validate_grants([denied.as_path()], &policy.write_allow, &[])
                    .is_err(),
                "{}",
                denied.display()
            );
        }
        assert!(
            policy
                .validate_grants([store.as_path()], &[store.clone()], &[])
                .is_err(),
            "a whole-store exception must fail closed"
        );
    }

    #[test]
    fn native_credential_floor_normalizes_each_path_once_per_profile() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let deny: Vec<_> = [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/gcloud",
            ".azure",
            ".config/cortexkit",
        ]
        .into_iter()
        .map(|name| root.join(name))
        .collect();
        CANONICALIZE_CALLS.with(|count| count.set(0));
        let profile = SandboxProfile::build(
            vec![root.clone()],
            deny.clone(),
            Vec::new(),
            Vec::new(),
            deny.clone(),
            Vec::new(),
            Vec::new(),
            root.clone(),
        )
        .unwrap();
        let calls = CANONICALIZE_CALLS.with(std::cell::Cell::get);
        eprintln!("credential-floor canonicalize calls per profile: {calls}");
        assert_eq!(calls, 22);
        let mut expected = deny;
        expected.sort_unstable();
        assert_eq!(profile.write_deny, expected);
        assert_eq!(profile.read_deny, expected);
        CANONICALIZE_CALLS.with(|count| count.set(0));
        assert_eq!(profile.clone().canonicalize_for_launch().unwrap(), profile);
        assert_eq!(CANONICALIZE_CALLS.with(std::cell::Cell::get), 22);
    }

    #[cfg(unix)]
    #[test]
    fn deny_normalization_is_fresh_for_every_profile_and_launcher() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let target = root.join("credential-target");
        std::fs::create_dir(&target).unwrap();
        let deny = root.join(".ssh");
        let build = || {
            SandboxProfile::build(
                vec![root.clone()],
                vec![deny.clone()],
                Vec::new(),
                Vec::new(),
                vec![deny.clone()],
                Vec::new(),
                Vec::new(),
                root.clone(),
            )
            .unwrap()
        };
        let missing = build();
        assert_eq!(missing.read_deny, vec![deny.clone()]);
        symlink(&target, &deny).unwrap();
        let existing = build();
        assert_eq!(existing.read_deny, vec![target.clone()]);
        assert_eq!(existing.write_deny, existing.read_deny);
        assert_eq!(
            missing.canonicalize_for_launch().unwrap().read_deny,
            vec![target]
        );
        std::fs::remove_file(&deny).unwrap();
        assert_eq!(build().read_deny, vec![deny]);
    }

    #[cfg(unix)]
    fn dangling_deny_profile(root: &Path, deny: &Path) -> SandboxProfile {
        SandboxProfile {
            secret_proxy: false,
            write_deny: vec![deny.to_path_buf()],
            write_deny_nested: vec![deny.to_path_buf()],
            read_deny: vec![deny.to_path_buf()],
            socket_deny: vec![deny.to_path_buf()],
            ..SandboxProfile::build(
                vec![root.to_path_buf()],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                root.to_path_buf(),
            )
            .unwrap()
        }
    }

    #[cfg(unix)]
    fn assert_dangling_denies_resolve(root: &Path, deny: &Path, target: &Path) {
        let raw = dangling_deny_profile(root, deny);
        let socket_only = SandboxProfile::build(
            raw.writable_roots.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            raw.socket_deny.clone(),
            Vec::new(),
            root.to_path_buf(),
        )
        .expect("compile dangling socket deny");
        assert_eq!(socket_only.socket_deny, &[target.to_path_buf()]);
        let compiled = SandboxProfile::build(
            raw.writable_roots.clone(),
            raw.write_deny.clone(),
            raw.write_deny_nested.clone(),
            raw.read_allow.clone(),
            raw.read_deny.clone(),
            raw.socket_deny.clone(),
            raw.cache_roots.clone(),
            raw.temp_dir.clone(),
        )
        .expect("compile dangling deny");
        let launched = raw.canonicalize_for_launch().expect("launch dangling deny");
        assert_eq!(compiled, launched);
        for paths in [
            &compiled.write_deny,
            &compiled.write_deny_nested,
            &compiled.read_deny,
            &compiled.socket_deny,
        ] {
            assert_eq!(paths, &[target.to_path_buf()]);
        }
        assert_eq!(
            compiled.clone().canonicalize_for_launch().unwrap(),
            compiled
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_denies_resolve_missing_target() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let target_dir = root.join("target-dir");
        std::fs::create_dir(&target_dir).unwrap();
        let alias = root.join("target-alias");
        std::os::unix::fs::symlink(&target_dir, &alias).unwrap();
        let deny = root.join("docker.sock");
        std::os::unix::fs::symlink(alias.join("missing/docker.sock"), &deny).unwrap();
        assert_dangling_denies_resolve(&root, &deny, &target_dir.join("missing/docker.sock"));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_denies_resolve_relative_two_hop_chain() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("links")).unwrap();
        let deny = root.join("links/docker.sock");
        std::os::unix::fs::symlink("../relay", &deny).unwrap();
        std::os::unix::fs::symlink("missing/docker.sock", root.join("relay")).unwrap();
        assert_dangling_denies_resolve(&root, &deny, &root.join("missing/docker.sock"));
        assert_dangling_denies_resolve(
            &root,
            &deny.join("nested"),
            &root.join("missing/docker.sock/nested"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_denies_reject_loop_by_name() {
        let root = tempfile::tempdir().unwrap();
        let deny = root.path().join("docker.sock");
        std::os::unix::fs::symlink("relay", &deny).unwrap();
        std::os::unix::fs::symlink("docker.sock", root.path().join("relay")).unwrap();
        let error = dangling_deny_profile(root.path(), &deny)
            .canonicalize_for_launch()
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlink loop"), "{error}");
        assert!(error.contains("write_deny"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_denies_reject_chain_limit_by_name() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..9 {
            std::os::unix::fs::symlink(
                format!("link-{}", index + 1),
                root.path().join(format!("link-{index}")),
            )
            .unwrap();
        }
        let error = dangling_deny_profile(root.path(), &root.path().join("link-0"))
            .canonicalize_for_launch()
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlink limit (8)"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_denies_accept_eight_hops() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        for index in 0..8 {
            std::os::unix::fs::symlink(
                format!("link-{}", index + 1),
                root.join(format!("link-{index}")),
            )
            .unwrap();
        }
        assert_dangling_denies_resolve(&root, &root.join("link-0"), &root.join("link-8"));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_denies_reject_non_utf8_target_by_name() {
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let deny = root.path().join("docker.sock");
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(b"missing-\xff"), &deny).unwrap();
        let error = dangling_deny_profile(root.path(), &deny)
            .canonicalize_for_launch()
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-UTF-8 symlink target"), "{error}");
    }

    #[test]
    fn build_canonicalizes_write_paths_and_retains_missing_denies() {
        let root = tempfile::tempdir().expect("temp root");
        let project = root.path().join("project");
        let cache = root.path().join("cache");
        let temp = root.path().join("temp");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::create_dir_all(&temp).expect("temp");
        std::fs::write(project.join("readable.txt"), b"readable").expect("readable file");
        let missing = root.path().join("missing-secret");
        let canonical_missing = root
            .path()
            .canonicalize()
            .expect("canonical root")
            .join("missing-secret");

        let profile = SandboxProfile::build(
            vec![project.clone()],
            Vec::new(),
            Vec::new(),
            vec![project.join("readable.txt")],
            vec![missing.clone()],
            Vec::new(),
            vec![cache.clone()],
            temp.clone(),
        )
        .expect("build profile");

        assert_eq!(
            profile.writable_roots,
            vec![project.canonicalize().unwrap()]
        );
        assert_eq!(
            profile.read_allow,
            vec![project.join("readable.txt").canonicalize().unwrap()]
        );
        assert_eq!(profile.cache_roots, vec![cache.canonicalize().unwrap()]);
        assert_eq!(profile.temp_dir, temp.canonicalize().unwrap());
        assert_eq!(profile.read_deny, vec![canonical_missing]);
    }

    #[test]
    fn explicit_read_allow_missing_at_build_or_launch_still_refuses() {
        let root = tempfile::tempdir().unwrap();
        let readable = root.path().join("explicit");
        let build = || {
            SandboxProfile::build(
                Vec::new(),
                Vec::new(),
                Vec::new(),
                vec![readable.clone()],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                root.path().to_path_buf(),
            )
        };
        assert!(build()
            .unwrap_err()
            .to_string()
            .contains("read_allow path does not exist"));
        std::fs::write(&readable, "explicit grant").unwrap();
        let profile = build().unwrap();
        assert!(profile.read_allow_listed.is_empty());
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        std::fs::remove_file(&readable).unwrap();
        assert!(transferred
            .canonicalize_for_launch()
            .unwrap_err()
            .to_string()
            .contains("read_allow path does not exist"));
    }

    #[test]
    fn listed_read_allow_provenance_survives_transfer_and_only_narrows_grants() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let kept = root.join("kept");
        let vanished = root.join("vanished");
        for path in [&kept, &vanished] {
            std::fs::write(path, "listed file").unwrap();
        }
        let mut profile = SandboxProfile::build(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![kept.clone(), vanished.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root.clone(),
        )
        .unwrap();
        profile.read_allow_listed = vec![kept.clone(), vanished.clone()];
        let transferred: SandboxProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        std::fs::remove_file(&vanished).unwrap();
        let validated = transferred.canonicalize_for_launch().unwrap();
        assert_eq!(validated.read_allow, vec![kept.clone()]);
        assert_eq!(validated.read_allow_listed, vec![kept]);
        assert!(!validated.read_allow.contains(&root));
        assert!(validated
            .read_allow
            .iter()
            .all(|path| profile.read_allow.contains(path)));
        assert_eq!(
            validated.clone().canonicalize_for_launch().unwrap(),
            validated
        );
    }

    #[test]
    fn listed_read_allow_rejects_unrelated_provenance() {
        let root = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::build(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root.path().to_path_buf(),
        )
        .unwrap();
        profile
            .read_allow_listed
            .push(root.path().join("not-a-grant"));
        assert!(profile
            .canonicalize_for_launch()
            .unwrap_err()
            .to_string()
            .contains("subset"));
    }

    #[cfg(unix)]
    #[test]
    fn listed_read_allow_does_not_hide_non_missing_errors_or_explicit_aliases() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let explicit = root.join("explicit");
        let alias = root.join("listed-alias");
        std::fs::write(&explicit, "required").unwrap();
        std::os::unix::fs::symlink(&explicit, &alias).unwrap();
        let (paths, listed) = canonicalize_read_grants(
            vec![explicit.clone(), alias.clone()],
            vec![alias.clone()],
            &[],
        )
        .unwrap();
        assert_eq!(paths, vec![explicit]);
        assert!(
            listed.is_empty(),
            "explicit grants must win canonical collisions"
        );
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&alias, &alias).unwrap();
        assert!(canonicalize_read_grants(vec![alias.clone()], vec![alias], &[]).is_err());
    }

    #[test]
    fn mixed_profile_versions_fail_closed() {
        #[derive(Debug, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyProfile {
            v: u32,
            writable_roots: Vec<PathBuf>,
            #[serde(default)]
            write_deny: Vec<PathBuf>,
            write_deny_nested: Vec<PathBuf>,
            read_deny: Vec<PathBuf>,
            socket_deny: Vec<PathBuf>,
            cache_roots: Vec<PathBuf>,
            temp_dir: PathBuf,
        }

        let root = tempfile::tempdir().expect("temp root");
        let root = root.path().canonicalize().expect("canonical root");
        let legacy_json = serde_json::json!({
            "v": 1,
            "writable_roots": [root],
            "write_deny": [],
            "write_deny_nested": [],
            "read_deny": [],
            "socket_deny": [],
            "cache_roots": [],
            "temp_dir": root,
        });
        let legacy: SandboxProfile =
            serde_json::from_value(legacy_json).expect("v1 shape remains parseable");
        let error = legacy
            .canonicalize_for_launch()
            .expect_err("new launcher must reject a v1 profile");
        assert!(error
            .to_string()
            .contains("unsupported sandbox profile version 1; expected 7"));

        let current = SandboxProfile::build(
            vec![root.clone()],
            Vec::new(),
            Vec::new(),
            vec![root.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root,
        )
        .expect("current profile");
        let mut previous = current.clone();
        previous.v = 4;
        assert!(previous
            .canonicalize_for_launch()
            .unwrap_err()
            .to_string()
            .contains("unsupported sandbox profile version 4; expected 7"));
        let error = serde_json::from_value::<LegacyProfile>(
            serde_json::to_value(current).expect("serialize current profile"),
        )
        .expect_err("v1 launcher shape must reject the current profile fields");
        assert!(error.to_string().contains("unknown field `mode`"));
    }

    #[test]
    fn launch_validation_retains_normalized_missing_deny_paths() {
        let root = tempfile::tempdir().expect("temp root");
        let root = root.path().canonicalize().expect("canonical root");
        let profile = SandboxProfile {
            secret_proxy: false,
            v: SANDBOX_PROFILE_VERSION,
            mode: crate::config::SandboxMode::Strict,
            worker_jail: false,
            keychain_deny: Vec::new(),
            deny_prefixes: Vec::new(),
            data_policy: SandboxDataPolicy::default(),
            writable_roots: vec![root.clone()],
            writable_files: Vec::new(),
            write_deny: vec![root.join("missing-write-deny")],
            write_deny_nested: vec![root.join("missing-nested")],
            read_allow: vec![root.clone()],
            read_allow_listed: Vec::new(),
            read_deny: vec![root.join("missing-secret")],
            socket_deny: vec![root.join("missing.sock")],
            loopback_tcp_connect_deny: Vec::new(),
            cache_roots: Vec::new(),
            temp_dir: root.clone(),
            worker_gh_fence: None,
        }
        .canonicalize_for_launch()
        .expect("validate profile");

        assert_eq!(profile.write_deny, vec![root.join("missing-write-deny")]);
        assert_eq!(profile.write_deny_nested, vec![root.join("missing-nested")]);
        assert_eq!(profile.read_deny, vec![root.join("missing-secret")]);
        assert_eq!(profile.socket_deny, vec![root.join("missing.sock")]);
    }

    #[cfg(unix)]
    #[test]
    fn relaxed_denies_retain_symlink_identity_and_resolved_target() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let target = root.join("operator-config");
        let alias = root.join("config");
        std::fs::write(&target, "operator").unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let mut profile = SandboxProfile::build(
            vec![root.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root,
        )
        .unwrap();
        profile.mode = crate::config::SandboxMode::Relaxed;
        profile.write_deny.push(alias.clone());
        let profile = profile.canonicalize_for_launch().unwrap();
        assert!(profile.write_deny.contains(&alias));
        assert!(profile.write_deny.contains(&target));
        assert_eq!(profile.clone().canonicalize_for_launch().unwrap(), profile);
    }
}
