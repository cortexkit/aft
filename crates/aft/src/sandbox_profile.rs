use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const SANDBOX_PROFILE_VERSION: u32 = 3;

/// Private stores with narrowly scoped exceptions. These exceptions apply only
/// to these trees, never to the credential floor or user-supplied read denies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxDataPolicy {
    pub deny: Vec<PathBuf>,
    pub read_allow: Vec<PathBuf>,
    pub write_allow: Vec<PathBuf>,
}

impl SandboxDataPolicy {
    fn canonicalize(self) -> Result<Self, SandboxProfileError> {
        Ok(Self {
            deny: canonicalize_optional_paths(self.deny, "data_policy.deny", &mut HashMap::new())?,
            read_allow: canonicalize_required_paths(self.read_allow, "data_policy.read_allow")?,
            write_allow: canonicalize_required_dirs(self.write_allow, "data_policy.write_allow")?,
        })
    }

    /// Landlock grants are additive: no emitted rule may encompass private data
    /// or sit inside it unless the entire rule is inside an explicit exception.
    pub fn validate_grants<'a>(
        &self,
        grants: impl IntoIterator<Item = &'a Path>,
        exceptions: &[PathBuf],
    ) -> Result<(), SandboxProfileError> {
        for grant in grants {
            for deny in &self.deny {
                if (grant.starts_with(deny) || deny.starts_with(grant))
                    && !exceptions.iter().any(|allow| {
                        grant.starts_with(allow) && allow.starts_with(deny) && allow != deny
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
    pub v: u32,
    #[serde(default)]
    pub data_policy: SandboxDataPolicy,
    pub writable_roots: Vec<PathBuf>,
    /// Mandatory write-deny paths for backends that support nested exclusions.
    #[serde(default)]
    pub write_deny: Vec<PathBuf>,
    pub write_deny_nested: Vec<PathBuf>,
    /// Existing paths that Landlock may grant read access beneath.
    #[serde(default)]
    pub read_allow: Vec<PathBuf>,
    pub read_deny: Vec<PathBuf>,
    pub socket_deny: Vec<PathBuf>,
    pub cache_roots: Vec<PathBuf>,
    pub temp_dir: PathBuf,
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
            v: SANDBOX_PROFILE_VERSION,
            data_policy: SandboxDataPolicy::default(),
            writable_roots: canonicalize_required_dirs(writable_roots, "writable_roots")?,
            write_deny: canonicalize_optional_paths(write_deny, "write_deny", &mut deny_paths)?,
            write_deny_nested: canonicalize_optional_paths(
                write_deny_nested,
                "write_deny_nested",
                &mut deny_paths,
            )?,
            read_allow: canonicalize_required_paths(read_allow, "read_allow")?,
            read_deny: canonicalize_optional_paths(read_deny, "read_deny", &mut deny_paths)?,
            socket_deny: canonicalize_optional_paths(socket_deny, "socket_deny", &mut deny_paths)?,
            cache_roots: canonicalize_required_dirs(cache_roots, "cache_roots")?,
            temp_dir: canonicalize_required_dir(temp_dir, "temp_dir")?,
        })
    }

    /// Revalidate an inherited profile and canonicalize it in the launcher.
    ///
    /// Missing deny targets remain enforceable by resolving their nearest
    /// existing ancestor. Every path that grants write access must exist.
    pub fn canonicalize_for_launch(self) -> Result<Self, SandboxProfileError> {
        if self.v != SANDBOX_PROFILE_VERSION {
            return Err(SandboxProfileError::new(format!(
                "unsupported sandbox profile version {}; expected {SANDBOX_PROFILE_VERSION}",
                self.v
            )));
        }

        let mut deny_paths = HashMap::new();
        Ok(Self {
            v: self.v,
            data_policy: self.data_policy.canonicalize()?,
            writable_roots: canonicalize_required_dirs(self.writable_roots, "writable_roots")?,
            write_deny: canonicalize_optional_paths(
                self.write_deny,
                "write_deny",
                &mut deny_paths,
            )?,
            write_deny_nested: canonicalize_optional_paths(
                self.write_deny_nested,
                "write_deny_nested",
                &mut deny_paths,
            )?,
            read_allow: canonicalize_required_paths(self.read_allow, "read_allow")?,
            read_deny: canonicalize_optional_paths(self.read_deny, "read_deny", &mut deny_paths)?,
            socket_deny: canonicalize_optional_paths(
                self.socket_deny,
                "socket_deny",
                &mut deny_paths,
            )?,
            cache_roots: canonicalize_required_dirs(self.cache_roots, "cache_roots")?,
            temp_dir: canonicalize_required_dir(self.temp_dir, "temp_dir")?,
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

fn canonicalization_error(path: &Path, field: &str, error: &std::io::Error) -> SandboxProfileError {
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
        };
        for allowed in [&own, &io, &own.join("new-file"), &io.join("temp")] {
            policy
                .validate_grants([allowed.as_path()], &policy.read_allow)
                .unwrap();
            policy
                .validate_grants([allowed.as_path()], &policy.write_allow)
                .unwrap();
        }
        policy
            .validate_grants([hooks.as_path()], &policy.read_allow)
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
                    .validate_grants([denied.as_path()], &policy.write_allow)
                    .is_err(),
                "{}",
                denied.display()
            );
        }
        assert!(
            policy
                .validate_grants([store.as_path()], &[store.clone()])
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
            .contains("unsupported sandbox profile version 1; expected 3"));

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
        let error = serde_json::from_value::<LegacyProfile>(
            serde_json::to_value(current).expect("serialize current profile"),
        )
        .expect_err("v1 launcher shape must reject the current profile fields");
        assert!(error.to_string().contains("unknown field `data_policy`"));
    }

    #[test]
    fn launch_validation_retains_normalized_missing_deny_paths() {
        let root = tempfile::tempdir().expect("temp root");
        let root = root.path().canonicalize().expect("canonical root");
        let profile = SandboxProfile {
            v: SANDBOX_PROFILE_VERSION,
            data_policy: SandboxDataPolicy::default(),
            writable_roots: vec![root.clone()],
            write_deny: vec![root.join("missing-write-deny")],
            write_deny_nested: vec![root.join("missing-nested")],
            read_allow: vec![root.clone()],
            read_deny: vec![root.join("missing-secret")],
            socket_deny: vec![root.join("missing.sock")],
            cache_roots: Vec::new(),
            temp_dir: root.clone(),
        }
        .canonicalize_for_launch()
        .expect("validate profile");

        assert_eq!(profile.write_deny, vec![root.join("missing-write-deny")]);
        assert_eq!(profile.write_deny_nested, vec![root.join("missing-nested")]);
        assert_eq!(profile.read_deny, vec![root.join("missing-secret")]);
        assert_eq!(profile.socket_deny, vec![root.join("missing.sock")]);
    }
}
