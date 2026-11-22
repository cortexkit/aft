use aft::config::SandboxMode;
use aft::sandbox_profile::{LinuxMountPolicy, SandboxProfile, WorkerGhFence};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mount {
    Guard,
    ReadOnly,
    DeferredReadOnly,
    ReadException,
    WriteException,
    EmptyFile,
    EmptyDir,
    PrivateDir,
    NoSearchDir,
}

impl Mount {
    fn masks(self) -> bool {
        matches!(self, Self::EmptyFile | Self::EmptyDir | Self::NoSearchDir)
    }

    fn host_writable(self) -> bool {
        matches!(self, Self::Guard | Self::WriteException)
    }
}

#[derive(Debug)]
pub(super) struct Snapshot {
    pub mounts: BTreeMap<PathBuf, Mount>,
    pub task_executable_bind: Option<(PathBuf, PathBuf)>,
    pub unenforced: Vec<String>,
}

impl Snapshot {
    pub fn take(profile: &SandboxProfile, executable: &Path) -> Result<Self, String> {
        let policy = profile.linux_mounts.as_ref().expect("local mount policy");
        let mut snapshot = Self::from_policy(policy)?;
        if profile.mode != SandboxMode::Credentials {
            snapshot.add_role_mounts(profile, policy)?;
        }
        let mut snapshot = snapshot.finish()?;
        if let Some(alias) = &policy.task_executable_alias {
            snapshot.add_task_executable_bind(profile, alias, executable)?;
        }
        Ok(snapshot)
    }

    fn from_policy(policy: &LinuxMountPolicy) -> Result<Self, String> {
        let mut snapshot = Self {
            mounts: BTreeMap::new(),
            task_executable_bind: None,
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
        Ok(snapshot)
    }

    fn finish(mut self) -> Result<Self, String> {
        self.guard_ancestors();
        self.unenforced.sort_unstable();
        self.unenforced.dedup();
        Ok(self)
    }

    #[cfg(test)]
    pub fn take_policy(policy: &LinuxMountPolicy) -> Result<Self, String> {
        Self::from_policy(policy)?.finish()
    }

    fn add_task_executable_bind(
        &mut self,
        profile: &SandboxProfile,
        alias: &Path,
        executable: &Path,
    ) -> Result<(), String> {
        if !alias.is_absolute()
            || !alias
                .parent()
                .is_some_and(|parent| parent.canonicalize().as_deref().ok() == Some(parent))
            || !std::fs::read_link(alias).is_ok_and(|target| target == executable)
        {
            return Err(format!(
                "task executable alias {} does not directly name the running AFT image",
                alias.display()
            ));
        }
        let covering_mount =
            |path: &Path| path.ancestors().find_map(|p| self.mounts.get(p).copied());
        if covering_mount(alias) != Some(Mount::PrivateDir) {
            return Ok(());
        }
        if covering_mount(executable)
            .is_some_and(|mount| mount.masks() || mount == Mount::PrivateDir)
            || (profile.mode == SandboxMode::Strict
                && !profile.read_allow.iter().any(|path| path == executable))
        {
            return Err(format!(
                "cannot restore task executable alias {}: the running AFT image lacks an exact read grant or is masked",
                alias.display()
            ));
        }
        // The private tmpfs hides the original symlink. Bind the same inode
        // at its old name so Strict's exact Landlock grant on the pin also
        // covers execution through the alias without granting its parent.
        self.task_executable_bind = Some((executable.to_path_buf(), alias.to_path_buf()));
        Ok(())
    }

    fn add_role_mounts(
        &mut self,
        profile: &SandboxProfile,
        policy: &LinuxMountPolicy,
    ) -> Result<(), String> {
        let data = &profile.data_policy;
        let mut stores = BTreeSet::new();
        let mut deferred = BTreeSet::new();
        for raw in &data.deny {
            if let Some((path, metadata)) = role_target(raw)? {
                stores.insert(path.clone());
                let entry = self.mounts.entry(path).or_insert(Mount::Guard);
                if !entry.masks() {
                    *entry = if metadata.is_dir() {
                        Mount::PrivateDir
                    } else {
                        Mount::EmptyFile
                    };
                }
            }
        }
        for raw in &policy.deferred_read_only {
            if let Some((path, _)) = role_target(raw)? {
                let entry = self.mounts.entry(path.clone()).or_insert(Mount::Guard);
                if *entry == Mount::Guard {
                    *entry = Mount::DeferredReadOnly;
                    deferred.insert(path);
                }
            } else {
                self.unenforced.push(format!(
                    "standard_control_target_absent_on_bwrap:{}",
                    raw.display()
                ));
            }
        }

        let session_reads = data
            .session_task_read_dir
            .as_deref()
            .map(super::landlock_backend::session_task_read_paths)
            .transpose()?
            .unwrap_or_default();
        let writable = data
            .write_allow
            .iter()
            .map(|path| (path, true, profile.writable_files.contains(path)));
        let readonly = data
            .read_allow
            .iter()
            .map(|path| (path, false, profile.writable_files.contains(path)))
            .chain(
                data.read_files
                    .iter()
                    .chain(&session_reads)
                    .map(|path| (path, false, true)),
            );
        let mut grants = BTreeMap::new();
        for (raw, write, exact_file) in writable.chain(readonly) {
            let Some((path, metadata)) = role_target(raw)? else {
                continue;
            };
            if exact_file && (path != *raw || !metadata.is_file()) {
                continue;
            }
            if stores.contains(&path) || deferred.contains(&path) {
                continue;
            }
            if path.ancestors().skip(1).any(|p| stores.contains(p))
                || write && path.ancestors().skip(1).any(|p| deferred.contains(p))
            {
                grants
                    .entry(path)
                    .and_modify(|w| *w |= write)
                    .or_insert(write);
            }
        }
        // Decide against the original hard controls before inserting any
        // exception. A private-store exception can never undo a credential
        // mask or a specific read-only control, including on its ancestors.
        let hard_controls = self.mounts.clone();
        for (path, writable) in grants {
            if path
                .ancestors()
                .any(|p| hard_controls.get(p).is_some_and(|m| m.masks()))
            {
                continue;
            }
            let writable = writable
                && !path
                    .ancestors()
                    .any(|p| hard_controls.get(p) == Some(&Mount::ReadOnly));
            let nearest = path.ancestors().skip(1).find_map(|p| self.mounts.get(p));
            if !writable
                && nearest == Some(&Mount::WriteException)
                && hard_controls.get(&path) != Some(&Mount::ReadOnly)
            {
                // A same-task session read is already covered by its writable I/O.
                continue;
            }
            self.mounts.insert(
                path,
                if writable {
                    Mount::WriteException
                } else {
                    Mount::ReadException
                },
            );
        }

        if let Some(WorkerGhFence::Canary { paths }) = &profile.worker_gh_fence {
            for marker in paths {
                let (marker, _) = existing_target(marker).map_err(|error| {
                    format!(
                        "cannot inspect worker-jail canary {}: {error}",
                        marker.display()
                    )
                })?;
                let dir = marker.parent().expect("canary has a directory");
                // A hard mask on an ancestor must also fail the canary probe
                // with EACCES, not hide the entire marker with ENOENT.
                let anchor = dir
                    .ancestors()
                    .filter(|p| {
                        matches!(
                            self.mounts.get(*p),
                            Some(Mount::EmptyDir | Mount::NoSearchDir)
                        )
                    })
                    .last();
                self.mounts
                    .insert(anchor.unwrap_or(dir).to_path_buf(), Mount::NoSearchDir);
            }
        }
        Ok(())
    }

    fn add(&mut self, path: &Path, mask: bool, absent: &str) -> Result<(), String> {
        require_absolute(path)?;
        let (target, metadata) = match existing_target(path) {
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
        if target == Path::new("/") {
            return Err("cannot mask or make the bubblewrap root read-only".into());
        }
        let mount = match (mask, metadata.is_dir()) {
            (true, true) => Mount::EmptyDir,
            // Sockets, like regular files, can be covered by a file mount.
            (true, false) => Mount::EmptyFile,
            (false, _) => Mount::ReadOnly,
        };
        let entry = self.mounts.entry(target.clone()).or_insert(mount);
        if mount.masks() || *entry == Mount::Guard {
            *entry = mount;
        }
        if target != path {
            // The mount hides the resolved target. Guard the alias's parent
            // directories too so renaming one cannot move the lookup path.
            for parent in path.ancestors().skip(1) {
                let resolved = parent.canonicalize().map_err(|error| {
                    format!(
                        "cannot resolve mount target ancestor {}: {error}",
                        parent.display()
                    )
                })?;
                if resolved != Path::new("/") {
                    self.mounts.entry(resolved).or_insert(Mount::Guard);
                }
            }
        }
        Ok(())
    }

    fn guard_ancestors(&mut self) {
        let mut planned: BTreeMap<PathBuf, Mount> = BTreeMap::new();
        // Paths sort before their descendants. Guard existing host directories,
        // not the synthetic directories used to reach exceptions in private stores.
        for (path, mount) in std::mem::take(&mut self.mounts) {
            let ancestors = path
                .ancestors()
                .skip(1)
                .take_while(|path| *path != Path::new("/"));
            let nearest = ancestors.clone().find_map(|p| planned.get(p).copied());
            if nearest.is_some_and(Mount::masks) {
                continue;
            }
            if nearest == Some(Mount::PrivateDir)
                && !matches!(
                    mount,
                    Mount::PrivateDir
                        | Mount::ReadException
                        | Mount::WriteException
                        | Mount::NoSearchDir
                )
            {
                continue;
            }
            if nearest.is_none_or(Mount::host_writable) {
                for ancestor in ancestors {
                    if planned.contains_key(ancestor) {
                        break;
                    }
                    planned.insert(ancestor.to_path_buf(), Mount::Guard);
                }
            } else if mount == Mount::Guard {
                continue;
            }
            planned.insert(path, mount);
        }
        self.mounts = planned;
    }
}

fn role_target(path: &Path) -> Result<Option<(PathBuf, std::fs::Metadata)>, String> {
    require_absolute(path)?;
    match existing_target(path) {
        Ok((target, _)) if target == Path::new("/") => Err(format!(
            "cannot make the bubblewrap root a role mount: {}",
            path.display()
        )),
        Ok(target) => Ok(Some(target)),
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(format!(
            "cannot inspect role mount {}: {error}",
            path.display()
        )),
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
