//! Memoize ticket bindings only while Git's repository inputs are unchanged.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};

use super::canonical_repository_key;

const MAX_ROOTS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    identity: crate::native_stat::FileIdentity,
    size: u64,
    modified_ns: i128,
}

fn stamp(path: &Path) -> Option<FileStamp> {
    // Symlinks can change their targets without changing the link's metadata.
    // Leave those uncommon layouts uncached rather than trust a partial stamp.
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() && !metadata.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileStamp {
            identity: crate::native_stat::FileIdentity::Unix {
                dev: metadata.dev(),
                ino: metadata.ino(),
                birth: None,
            },
            size: metadata.len(),
            modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        })
    }
    #[cfg(not(unix))]
    {
        let stat = crate::native_stat::NativeStat::read(path).ok()?;
        Some(FileStamp {
            identity: stat.identity,
            size: stat.size,
            modified_ns: stat.modified_ns,
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
struct LookupEnv {
    path: Option<OsString>,
    // Relative PATH entries also depend on the process's working directory.
    cwd: PathBuf,
    #[cfg(windows)]
    pathext: Option<OsString>,
    #[cfg(windows)]
    system_root: Option<OsString>,
}

impl LookupEnv {
    fn read() -> Option<Self> {
        Some(Self {
            path: std::env::var_os("PATH"),
            cwd: std::env::current_dir().ok()?,
            #[cfg(windows)]
            pathext: std::env::var_os("PATHEXT"),
            #[cfg(windows)]
            system_root: std::env::var_os("SystemRoot"),
        })
    }
}

struct Entry {
    root: PathBuf,
    dot_git: FileStamp,
    inputs: Inputs,
    env: LookupEnv,
    value: Option<String>,
}

#[derive(Default)]
struct Cache(VecDeque<Entry>);

impl Cache {
    fn get(&mut self, root: &Path, dot_git: &FileStamp, env: &LookupEnv) -> Option<Option<String>> {
        let index = self.0.iter().position(|entry| entry.root == root)?;
        let entry = self.0.remove(index)?;
        if entry.dot_git != *dot_git || entry.env != *env || !entry.inputs.unchanged() {
            return None;
        }
        let value = entry.value.clone();
        self.0.push_back(entry);
        Some(value)
    }

    fn insert(&mut self, entry: Entry) {
        self.0.retain(|old| old.root != entry.root);
        if self.0.len() >= MAX_ROOTS {
            self.0.pop_front();
        }
        self.0.push_back(entry);
    }
}

pub(super) fn snapshot(project_root: &Path) -> Option<String> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    snapshot_with_cache(project_root, CACHE.get_or_init(Mutex::default))
}

fn snapshot_with_cache(project_root: &Path, cache: &Mutex<Cache>) -> Option<String> {
    if !project_root.is_dir() {
        return None;
    }
    let env = LookupEnv::read()?;
    let root = fs::canonicalize(project_root).ok();
    let dot_git = stamp(&project_root.join(".git"));
    if let (Some(root), Some(dot_git)) = (&root, &dot_git) {
        if let Some(value) = cache
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(root, dot_git, &env))
        {
            return value;
        }
    }

    // Do not hold the cache lock while resolving Git or waiting on children.
    let mut inputs = None;
    let value = uncached(project_root, &env, &mut inputs);
    if let (Some(root), Some(dot_git), Some(inputs)) = (root, dot_git, inputs) {
        // A concurrent rewrite must not attach an old answer to new metadata.
        if stamp(&project_root.join(".git")).as_ref() == Some(&dot_git)
            && inputs.unchanged()
            && LookupEnv::read().as_ref() == Some(&env)
        {
            if let Ok(mut cache) = cache.lock() {
                cache.insert(Entry {
                    root,
                    dot_git,
                    inputs,
                    env,
                    value: value.clone(),
                });
            }
        }
    }
    value
}

fn uncached(project_root: &Path, env: &LookupEnv, inputs: &mut Option<Inputs>) -> Option<String> {
    let git = which::which_in("git", env.path.as_ref(), &env.cwd).ok()?;
    let command = || {
        let mut command = Command::new(&git);
        command.env_clear().env("GIT_CONFIG_NOSYSTEM", "1");
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        command
    };
    let output = git_output(command().arg("-C").arg(project_root).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let common = PathBuf::from(std::str::from_utf8(&output.stdout).ok()?.trim());
    if !common.is_absolute() {
        return None;
    }
    // Read the primary repository's config, not a worktree-specific override.
    // The daemon saves the name in its ticket registry before launching the shell.
    let path = common.join("config");
    *inputs = Inputs::read(project_root, &common);
    let output = git_output(
        command()
            .args(["config", "--no-includes", "--file"])
            .arg(path)
            .args(["--get", "remote.origin.url"]),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    canonical_repository_key(std::str::from_utf8(&output.stdout).ok()?.trim())
}

struct Inputs {
    config: PathBuf,
    config_stamp: FileStamp,
    git_dir: PathBuf,
    git_dir_stamp: FileStamp,
    common_dir_pointer: PathBuf,
    common_dir_pointer_stamp: Option<FileStamp>,
}

fn optional_stamp(path: &Path) -> std::io::Result<Option<FileStamp>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
        Ok(_) => stamp(path)
            .map(Some)
            .ok_or_else(|| std::io::Error::other("unavailable file identity")),
    }
}

impl Inputs {
    fn read(root: &Path, common: &Path) -> Option<Self> {
        let dot_git = root.join(".git");
        let metadata = fs::symlink_metadata(&dot_git).ok()?;
        let git_dir = if metadata.is_dir() {
            dot_git
        } else if metadata.is_file() {
            let pointer = fs::read_to_string(&dot_git).ok()?;
            let path = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
            root.join(path)
        } else {
            return None;
        };
        let git_dir_stamp = stamp(&git_dir)?;
        // A linked worktree's gitdir and commondir can be repaired independently
        // of its .git file. Watch both before trusting the old config path.
        let common_dir_pointer = git_dir.join("commondir");
        let common_dir_pointer_stamp = optional_stamp(&common_dir_pointer).ok()?;
        let resolved_common = if common_dir_pointer_stamp.is_some() {
            let pointer = fs::read_to_string(&common_dir_pointer).ok()?;
            git_dir.join(pointer.trim())
        } else {
            git_dir.clone()
        };
        // Parsing is only a cache-admission check: Git remains the authority.
        if fs::canonicalize(resolved_common).ok()? != fs::canonicalize(common).ok()? {
            return None;
        }
        let config = common.join("config");
        Some(Self {
            config_stamp: stamp(&config)?,
            config,
            git_dir,
            git_dir_stamp,
            common_dir_pointer,
            common_dir_pointer_stamp,
        })
    }

    fn unchanged(&self) -> bool {
        stamp(&self.config).as_ref() == Some(&self.config_stamp)
            && stamp(&self.git_dir).as_ref() == Some(&self.git_dir_stamp)
            && optional_stamp(&self.common_dir_pointer)
                .is_ok_and(|stamp| stamp == self.common_dir_pointer_stamp)
    }
}

fn git_output(command: &mut Command) -> std::io::Result<Output> {
    #[cfg(test)]
    SPAWNS.with(|count| count.set(count.get() + 1));
    command.output()
}

#[cfg(test)]
thread_local! {
    static SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[path = "ticket_repository_tests.rs"]
mod tests;
