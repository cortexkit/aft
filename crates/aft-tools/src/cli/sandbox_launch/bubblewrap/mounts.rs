use aft::sandbox_profile::LinuxMountPolicy;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mount {
    Guard,
    ReadOnly,
    EmptyFile,
    EmptyDir,
}

impl Mount {
    fn masks(self) -> bool {
        matches!(self, Self::EmptyFile | Self::EmptyDir)
    }
}

#[derive(Debug)]
pub(super) struct Snapshot {
    pub mounts: BTreeMap<PathBuf, Mount>,
    pub unenforced: Vec<String>,
}

impl Snapshot {
    pub fn take(policy: &LinuxMountPolicy) -> Result<Self, String> {
        let mut snapshot = Self {
            mounts: BTreeMap::new(),
            unenforced: Vec::new(),
        };
        // Rescan immediately before building mount arguments: Cargo can create
        // a credentials file after the engine has prepared the profile.
        let mut credentials = policy.credential_masks.clone();
        for home in &policy.cargo_homes {
            scan_cargo_home(home, &mut credentials)?;
            snapshot.unenforced.push(format!(
                "credential_filename_prefix_future_entries_unenforced_on_bwrap:{}*",
                home.join("credentials").display()
            ));
        }
        for path in &credentials {
            snapshot.add(path, true, "credential_path_absent_on_bwrap")?;
        }
        let credential_paths = credentials.iter().collect::<BTreeSet<_>>();
        let read_masks = policy.read_masks.iter().collect::<BTreeSet<_>>();
        for path in read_masks.difference(&credential_paths) {
            snapshot.add(path, true, "standard_control_target_absent_on_bwrap")?;
        }
        for path in &policy.read_only {
            if credential_paths.contains(path) || read_masks.contains(path) {
                continue;
            }
            snapshot.add(path, false, "standard_control_target_absent_on_bwrap")?;
        }
        snapshot.guard_ancestors();
        snapshot.unenforced.sort_unstable();
        snapshot.unenforced.dedup();
        Ok(snapshot)
    }

    fn add(&mut self, path: &Path, mask: bool, absent: &str) -> Result<(), String> {
        require_absolute(path)?;
        let (path, metadata) = match existing_target(path) {
            Ok(target) => target,
            Err(error) if missing(&error) => {
                if !(mask && path.ends_with("Library/Keychains")) {
                    self.unenforced.push(format!("{absent}:{}", path.display()));
                }
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect mount target {}: {error}",
                    path.display()
                ))
            }
        };
        if path == Path::new("/") {
            return Err("cannot mask or make the bubblewrap root read-only".into());
        }
        let mount = match (mask, metadata.is_dir()) {
            (true, true) => Mount::EmptyDir,
            // Sockets, like regular files, can be covered by a file mount.
            (true, false) => Mount::EmptyFile,
            (false, _) => Mount::ReadOnly,
        };
        let entry = self.mounts.entry(path).or_insert(mount);
        if mount.masks() {
            *entry = mount;
        }
        Ok(())
    }

    fn guard_ancestors(&mut self) {
        let mut planned = BTreeMap::new();
        // BTreeMap orders a path before its descendants. An empty directory
        // mask already blocks its children; binding one back would expose it.
        for (path, mount) in std::mem::take(&mut self.mounts) {
            if path
                .ancestors()
                .skip(1)
                .any(|ancestor| planned.get(ancestor) == Some(&Mount::EmptyDir))
            {
                continue;
            }
            let mut ancestors = path
                .ancestors()
                .skip(1)
                .take_while(|path| *path != Path::new("/"));
            if !ancestors
                .clone()
                .any(|ancestor| planned.get(ancestor) == Some(&Mount::ReadOnly))
            {
                for ancestor in ancestors.by_ref() {
                    planned
                        .entry(ancestor.to_path_buf())
                        .or_insert(Mount::Guard);
                }
            }
            planned.insert(path, mount);
        }
        self.mounts = planned;
    }
}

fn require_absolute(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "bubblewrap policy path is not absolute: {}",
            path.display()
        ));
    }
    Ok(())
}

fn existing_target(path: &Path) -> io::Result<(PathBuf, std::fs::Metadata)> {
    let target = path.canonicalize()?;
    let metadata = std::fs::metadata(&target)?;
    Ok((target, metadata))
}

fn missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn scan_cargo_home(home: &Path, credentials: &mut Vec<PathBuf>) -> Result<(), String> {
    require_absolute(home)?;
    let failure = |error| {
        format!(
            "cannot inspect CARGO_HOME credentials in {}: {error}",
            home.display()
        )
    };
    let entries = match std::fs::read_dir(home) {
        Ok(entries) => entries,
        Err(error) if missing(&error) => return Ok(()),
        Err(error) => return Err(failure(error)),
    };
    for entry in entries {
        let entry = entry.map_err(failure)?;
        if entry
            .file_name()
            .to_string_lossy()
            .to_lowercase()
            .starts_with("credentials")
        {
            credentials.push(entry.path());
        }
    }
    Ok(())
}
