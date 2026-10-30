//! Machine-parsed Git probes that keep "this is not a repository" separate from
//! failures whose meaning is uncertain (damaged metadata, broken refs, signals).
//!
//! Callers keep their own path decoding, canonicalization and error text; this
//! module only decides which failures may be treated as certain absence.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// Git exited normally and no repository marker is visible on disk.
    NotRepository,
    /// A verified repository whose current branch has no commit yet.
    NoCommit,
    /// Anything else, including damaged metadata and signal deaths.
    Other,
}

#[derive(Debug)]
pub(crate) enum ProbeError {
    Spawn(io::Error),
    Failed { kind: FailureKind, output: Output },
}

/// A Git command whose diagnostics are always in the C locale.
pub(crate) fn command() -> Command {
    let mut builder = crate::effective_path::new_command("git");
    // These probes classify Git's diagnostic text, so an inherited translation
    // must not change it. Environment overrides go before `build` resolves
    // the executable against the child PATH.
    builder
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("LANGUAGE", "C");
    #[cfg(test)]
    TEST_CHILD_ENV.with(|overrides| {
        for (key, value) in overrides.borrow().iter() {
            builder.env(key, value);
        }
    });
    builder.build()
}

/// `git rev-parse --show-toplevel`, run with `-C root` from AFT's own working
/// directory.
#[cfg(test)]
pub(crate) fn toplevel(root: &Path) -> Result<Vec<u8>, ProbeError> {
    Probe::new(root, ChildDirectory::Inherited).run(&["rev-parse", "--show-toplevel"])
}

/// `git rev-parse --show-toplevel`, run with `root` as the child's working
/// directory. A relative or empty PATH entry then finds `git` relative to
/// `root`, as it does for any other command started in that folder.
pub(crate) fn toplevel_in_directory(root: &Path) -> Result<Vec<u8>, ProbeError> {
    Probe::new(root, ChildDirectory::Root).run(&["rev-parse", "--show-toplevel"])
}

/// Check the same repository discovery range without requiring a Git executable.
pub(crate) fn repository_marker_or_uncertainty_in_directory(root: &Path) -> bool {
    let probe = Probe::new(root, ChildDirectory::Root);
    repository_marker_or_uncertainty(root, &DiscoveryLimits::from_command(&probe.command()))
}

/// A probe run with `root` as the child's working directory; see
/// [`toplevel_in_directory`].
pub(crate) fn probe_in_directory(root: &Path, args: &[&str]) -> Result<Vec<u8>, ProbeError> {
    Probe::new(root, ChildDirectory::Root).run(args)
}

/// `git rev-list --max-parents=0 HEAD`, run with `-C root`. `spawned` grows by
/// the number of Git processes actually started, including the extra checks
/// that verify an unborn branch.
pub(crate) fn root_commits(root: &Path, spawned: &mut usize) -> Result<Vec<u8>, ProbeError> {
    let mut probe = Probe::new(root, ChildDirectory::Inherited);
    let result = probe.run(&["rev-list", "--max-parents=0", "HEAD"]);
    *spawned += probe.spawned;
    result
}

/// Where a probe's Git child starts. The working directory decides how a
/// relative PATH entry resolves, so each caller keeps the choice it made
/// before these probes were shared.
#[derive(Clone, Copy)]
enum ChildDirectory {
    /// Inherit AFT's working directory and pass `-C root`.
    Inherited,
    /// Start the child in `root` itself.
    Root,
}

struct Probe<'a> {
    root: &'a Path,
    directory: ChildDirectory,
    spawned: usize,
}

impl<'a> Probe<'a> {
    fn new(root: &'a Path, directory: ChildDirectory) -> Self {
        Self {
            root,
            directory,
            spawned: 0,
        }
    }

    fn command(&self) -> Command {
        let mut command = command();
        match self.directory {
            ChildDirectory::Inherited => {
                command.arg("-C").arg(self.root);
            }
            ChildDirectory::Root => {
                command.current_dir(self.root);
            }
        }
        command
    }

    fn output<I, S>(&mut self, args: I) -> io::Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        // Unavailable Git cannot attest repository absence. Do not launch the
        // developer-tools installer while trying to determine an identity.
        if !crate::developer_tools::git_usable() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                crate::developer_tools::MISSING_DEVELOPER_TOOLS_REASON,
            ));
        }
        let output = self.command().args(args).output()?;
        self.spawned += 1;
        Ok(output)
    }

    fn run(&mut self, args: &[&str]) -> Result<Vec<u8>, ProbeError> {
        let output = self.output(args).map_err(ProbeError::Spawn)?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        // Read the discovery limits from the same environment the Git child
        // received, not from AFT's own process environment.
        let limits = DiscoveryLimits::from_command(&self.command());
        let root = self.root;
        classify_failure(root, &limits, args, output, || self.unborn_head())
    }
}

/// Where Git's upward repository search stops, read from a child environment.
struct DiscoveryLimits {
    /// Resolved `GIT_CEILING_DIRECTORIES` entries.
    ceilings: Vec<PathBuf>,
    /// Without `GIT_DISCOVERY_ACROSS_FILESYSTEM`, Git stops at a device change.
    one_filesystem: bool,
}

impl DiscoveryLimits {
    fn from_command(command: &Command) -> Self {
        Self {
            ceilings: ceiling_directories(
                child_environment_value(command, "GIT_CEILING_DIRECTORIES").as_deref(),
            ),
            one_filesystem: !child_environment_value(command, "GIT_DISCOVERY_ACROSS_FILESYSTEM")
                .as_deref()
                .is_some_and(git_env_bool),
        }
    }
}

/// The value `key` has in the child: an explicit override on the command, or
/// otherwise the inherited process value.
fn child_environment_value(command: &Command, key: &str) -> Option<OsString> {
    for (name, value) in command.get_envs() {
        if name == OsStr::new(key) {
            return value.map(OsStr::to_os_string);
        }
    }
    std::env::var_os(key)
}

/// Git's boolean environment parsing: true/yes/on or a non-zero number.
/// Values Git would reject make Git fail with another message first.
fn git_env_bool(value: &OsStr) -> bool {
    let value = value.to_string_lossy().to_ascii_lowercase();
    match value.as_str() {
        "true" | "yes" | "on" => true,
        "false" | "no" | "off" | "" => false,
        number => number.parse::<i64>().map_or(true, |number| number != 0),
    }
}

/// Parse `GIT_CEILING_DIRECTORIES` the way Git does: path-separator delimited,
/// relative entries ignored, entries resolved through symlinks (an entry that
/// cannot be resolved is dropped), and every entry after an empty one taken
/// literally without symlink resolution.
fn ceiling_directories(value: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut resolve = true;
    let mut ceilings = Vec::new();
    for entry in std::env::split_paths(value) {
        if entry.as_os_str().is_empty() {
            resolve = false;
            continue;
        }
        if !entry.is_absolute() {
            continue;
        }
        let ceiling = if resolve {
            match std::fs::canonicalize(&entry) {
                Ok(resolved) => comparable(resolved),
                Err(_) => continue,
            }
        } else {
            comparable(crate::path_identity::lexical_normalize(&entry))
        };
        ceilings.push(ceiling);
    }
    ceilings
}

/// Drop Windows verbatim prefixes so resolved and literal paths compare equal.
fn comparable(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        crate::windows_path::normalize_windows_path(&path)
    }
    #[cfg(not(windows))]
    {
        path
    }
}

fn classify_failure(
    root: &Path,
    limits: &DiscoveryLimits,
    args: &[&str],
    output: Output,
    verify_unborn: impl FnOnce() -> bool,
) -> Result<Vec<u8>, ProbeError> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Only Git's ordinary fatal exit (128) can certify absence or an unborn
    // branch. A process killed by a signal may have printed anything first.
    let ordinary_fatal_exit = output.status.code() == Some(128);
    let absence = stderr
        .starts_with("fatal: not a git repository (or any of the parent directories):")
        || stderr.starts_with("fatal: not a git repository (or any parent up to mount point");
    let kind = if ordinary_fatal_exit && absence && !repository_marker_or_uncertainty(root, limits)
    {
        FailureKind::NotRepository
    } else if ordinary_fatal_exit
        && args == ["rev-list", "--max-parents=0", "HEAD"]
        && (stderr.starts_with("fatal: ambiguous argument 'HEAD':")
            || stderr.starts_with("fatal: bad revision 'HEAD'"))
        && verify_unborn()
    {
        FailureKind::NoCommit
    } else {
        FailureKind::Other
    };
    Err(ProbeError::Failed { kind, output })
}

/// Whether any directory Git searched holds a repository marker.
///
/// Git also prints its ordinary absence message when a `.git` it would
/// normally accept is damaged (for example a corrupt HEAD). A marker that
/// looks like a repository, or one that cannot be checked, is therefore
/// uncertainty rather than absence. The scan covers exactly the directories
/// Git searched: it starts at the resolved starting folder and stops below
/// the nearest ceiling directory and, unless discovery may cross filesystems,
/// at the first parent on another device.
fn repository_marker_or_uncertainty(root: &Path, limits: &DiscoveryLimits) -> bool {
    let start = comparable(std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
    // Like Git, only a ceiling that is a proper ancestor of the start counts;
    // the starting folder itself is always searched.
    let ceiling = limits
        .ceilings
        .iter()
        .filter(|ceiling| start != **ceiling && start.starts_with(ceiling))
        .max_by_key(|ceiling| ceiling.components().count());
    let start_device = if limits.one_filesystem {
        match device_of(&start) {
            Ok(device) => device,
            Err(_) => return true,
        }
    } else {
        None
    };
    for directory in start.ancestors() {
        if directory != start {
            if ceiling
                .is_some_and(|ceiling| directory == ceiling || !directory.starts_with(ceiling))
            {
                break;
            }
            if let Some(start_device) = start_device {
                match device_of(directory) {
                    Ok(Some(device)) if device != start_device => break,
                    Ok(_) => {}
                    Err(_) => return true,
                }
            }
        }
        if git_marker_is_repository_candidate(&directory.join(".git")) {
            return true;
        }
        // A bare repository has no `.git` entry; recognise its layout instead.
        if directory.join("HEAD").exists()
            && directory.join("objects").exists()
            && directory.join("refs").exists()
        {
            return true;
        }
    }
    false
}

/// The filesystem device of `path`. Platforms without a device number return
/// `None`, which never stops the scan.
fn device_of(path: &Path) -> io::Result<Option<u64>> {
    #[cfg(test)]
    if let Some(device) = TEST_DEVICE_OF.with(|hook| hook.borrow().as_ref().map(|hook| hook(path)))
    {
        return Ok(device);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map(|metadata| Some(metadata.dev()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Whether a `.git` entry is something Git would treat as a repository.
///
/// Git only accepts a `.git` directory that holds a HEAD file; without one it
/// keeps searching the parent directories. A stub such as a `.git` holding
/// only `info/exclude` is therefore a plain folder, exactly as Git sees it.
/// A `.git` file (a worktree or submodule link) is a candidate because Git
/// follows it. Any entry whose state cannot be read counts as a candidate.
fn git_marker_is_repository_candidate(marker: &Path) -> bool {
    let absent = |error: &io::Error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        )
    };
    match std::fs::metadata(marker) {
        Ok(metadata) if metadata.is_dir() => match std::fs::symlink_metadata(marker.join("HEAD")) {
            Ok(_) => true,
            Err(error) => !absent(&error),
        },
        Ok(_) => true,
        Err(error) => !absent(&error),
    }
}

impl Probe<'_> {
    fn unborn_head(&mut self) -> bool {
        // A valid repository without any commit may use the path-derived cache
        // identity. Before allowing that, prove the repository is readable, HEAD
        // names a branch, and that branch's ref really does not exist: a broken
        // or unreadable ref produces the same quiet missing-HEAD result.
        let Ok(repository) = self.output(["rev-parse", "--git-dir"]) else {
            return false;
        };
        if !repository.status.success() {
            return false;
        }
        let missing_head = self
            .output(["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_ok_and(|output| output.status.code() == Some(1) && output.stderr.is_empty());
        if !missing_head {
            return false;
        }
        let Ok(symbolic) = self.output(["symbolic-ref", "--quiet", "HEAD"]) else {
            return false;
        };
        if !symbolic.status.success() || !symbolic.stderr.is_empty() {
            return false;
        }
        let Some(reference) = git_output_path(&symbolic.stdout) else {
            return false;
        };
        let Some(loose) = self.git_path(reference.as_os_str()) else {
            return false;
        };
        let Some(packed) = self.git_path(OsStr::new("packed-refs")) else {
            return false;
        };
        #[cfg(test)]
        BEFORE_LOOSE_REF_CHECK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook(&loose);
            }
        });
        // These filesystem checks run after the Git subprocesses above, so a ref
        // created (or broken) in between is still seen. Only actual nonexistence
        // of the loose ref proves an unborn branch.
        if !matches!(std::fs::symlink_metadata(loose), Err(error) if error.kind() == io::ErrorKind::NotFound)
        {
            return false;
        }
        match std::fs::File::open(packed) {
            Ok(_) => true,
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        }
    }

    fn git_path(&mut self, name: &OsStr) -> Option<PathBuf> {
        let output = self
            .output([OsStr::new("rev-parse"), OsStr::new("--git-path"), name])
            .ok()?;
        if !output.status.success() || !output.stderr.is_empty() {
            return None;
        }
        let path = git_output_path(&output.stdout)?;
        Some(if path.is_absolute() {
            path
        } else {
            self.root.join(path)
        })
    }
}

/// Test fixture: a real repository whose HEAD file is corrupt, so Git rejects
/// it and prints its ordinary absence message even though a repository exists.
#[cfg(test)]
pub(crate) fn init_repository_with_corrupt_head_for_test(path: &Path) {
    assert!(command()
        .args(["init", "--quiet"])
        .arg(path)
        .status()
        .unwrap()
        .success());
    std::fs::write(path.join(".git").join("HEAD"), "not a ref\n").unwrap();
}

/// A real repository with every nested-checkout shape below its root:
/// - `linked/`: a linked worktree of this repository (`git worktree add`), to
///   be skipped by project walks;
/// - `vendor/independent/`: a nested independent repository (its own `.git`
///   directory), still walked;
/// - `submod/`: a submodule-style checkout (a `.git` file naming
///   `<root>/.git/modules/submod`), still walked.
#[cfg(test)]
pub(crate) struct NestedWorktreeFixture {
    pub root: PathBuf,
    pub worktree: PathBuf,
    pub independent: PathBuf,
    pub submodule: PathBuf,
}

/// Test fixture: `root` becomes the repository above, with `marker` written
/// into one committed source file. Asserts Git runs and every step succeeds.
#[cfg(test)]
pub(crate) fn init_repo_with_nested_worktree_for_test(
    root: &Path,
    marker: &str,
) -> NestedWorktreeFixture {
    assert!(
        crate::developer_tools::git_usable(),
        "nested-worktree fixtures need git"
    );
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let git = |dir: &Path, args: &[&str]| {
        let output = command()
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_COUNT", "0")
            .output()
            .expect("git runs");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    let source = format!("pub fn probe_marker() {{ let _ = \"{marker}\"; }}\n");
    for relative in ["src/main.rs", "lib/util.rs"] {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, &source).unwrap();
    }
    git(&root, &["init", "--quiet"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "initial"]);
    // A linked worktree of this repository, nested in a subfolder of the main
    // checkout: the worktree's `.git` file names its admin dir under the
    // main checkout's `.git/worktrees/`.
    git(&root, &["worktree", "add", "--quiet", "linked"]);
    let worktree = root.join("linked");
    std::fs::write(worktree.join("extra.rs"), &source).unwrap();
    // A nested independent repository: its own `.git` directory.
    let independent = root.join("vendor").join("independent");
    std::fs::create_dir_all(&independent).unwrap();
    git(&independent, &["init", "--quiet"]);
    std::fs::write(independent.join("inner.rs"), &source).unwrap();
    // A submodule-style checkout: a `.git` file whose gitdir lives under
    // `.git/modules/`, not `worktrees/`.
    let submodule = root.join("submod");
    std::fs::create_dir_all(&submodule).unwrap();
    std::fs::create_dir_all(root.join(".git").join("modules").join("submod")).unwrap();
    std::fs::write(submodule.join(".git"), "gitdir: ../.git/modules/submod\n").unwrap();
    std::fs::write(submodule.join("code.rs"), &source).unwrap();
    NestedWorktreeFixture {
        root,
        worktree,
        independent,
        submodule,
    }
}

/// Test fixture: a `.git` folder holding only `info/exclude` and no HEAD, which
/// Git itself does not treat as a repository.
#[cfg(test)]
pub(crate) fn write_stub_git_directory_for_test(path: &Path) {
    let info = path.join(".git").join("info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("exclude"), "ignored/\n").unwrap();
}

#[cfg(test)]
type BeforeLooseRefCheck = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
type DeviceOf = Box<dyn Fn(&Path) -> Option<u64>>;
#[cfg(test)]
thread_local! {
    /// Test hook run just before the loose-ref existence check in `unborn_head`.
    static BEFORE_LOOSE_REF_CHECK: std::cell::RefCell<Option<BeforeLooseRefCheck>> = const { std::cell::RefCell::new(None) };
    /// Extra child environment applied to every probe command on this thread.
    static TEST_CHILD_ENV: std::cell::RefCell<Vec<(OsString, OsString)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Replacement device lookup for the filesystem-boundary check.
    static TEST_DEVICE_OF: std::cell::RefCell<Option<DeviceOf>> = const { std::cell::RefCell::new(None) };
}

/// Test fixture: an executable `git` stub in `directory` that prints `output`.
#[cfg(all(test, unix))]
pub(crate) fn write_stub_git_for_test(directory: &Path, output: &str) {
    std::fs::create_dir_all(directory).unwrap();
    let stub = directory.join("git");
    crate::executable_cache::write_executable(
        &stub,
        format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n").as_bytes(),
    )
    .unwrap();
}

/// A child PATH whose first entry is the relative directory `bin`, followed by
/// the effective PATH, so a `bin/git` stub wins only when the child starts in
/// the folder that holds it.
#[cfg(all(test, unix))]
pub(crate) fn relative_bin_path_for_test() -> OsString {
    let mut entries = vec![PathBuf::from("bin")];
    entries.extend(std::env::split_paths(
        crate::effective_path::effective_path(),
    ));
    std::env::join_paths(entries).unwrap()
}

/// Runs `body` with extra child environment for every Git probe started on
/// this thread, leaving AFT's own process environment untouched.
#[cfg(test)]
pub(crate) fn with_child_env_for_test<R>(
    overrides: &[(&str, &OsStr)],
    body: impl FnOnce() -> R,
) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_CHILD_ENV.with(|env| env.borrow_mut().clear());
        }
    }
    TEST_CHILD_ENV.with(|env| {
        *env.borrow_mut() = overrides
            .iter()
            .map(|(key, value)| (OsString::from(key), value.to_os_string()))
            .collect();
    });
    let _reset = Reset;
    body()
}

/// Decode one line of Git path output, keeping raw bytes on Unix.
fn git_output_path(bytes: &[u8]) -> Option<PathBuf> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure_kind(result: Result<Vec<u8>, ProbeError>) -> FailureKind {
        match result.unwrap_err() {
            ProbeError::Failed { kind, .. } => kind,
            ProbeError::Spawn(error) => panic!("Git must be runnable for probe fixtures: {error}"),
        }
    }

    #[cfg(unix)]
    fn unlimited() -> DiscoveryLimits {
        DiscoveryLimits {
            ceilings: Vec::new(),
            one_filesystem: false,
        }
    }

    fn init_repository(path: &Path) {
        assert!(command()
            .args(["init", "--quiet"])
            .arg(path)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn machine_git_command_pins_its_diagnostic_locale() {
        let command = command();
        let env = command
            .get_envs()
            .collect::<std::collections::HashMap<_, _>>();
        for key in ["LC_ALL", "LANG", "LANGUAGE"] {
            assert_eq!(
                env.get(std::ffi::OsStr::new(key)).copied().flatten(),
                Some(std::ffi::OsStr::new("C")),
                "Git diagnostic locale override missing: {key}"
            );
        }
    }

    #[test]
    fn corrupt_git_metadata_is_not_repository_absence() {
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::init_repository_with_corrupt_head_for_test(dir.path());
        assert_eq!(failure_kind(toplevel(dir.path())), FailureKind::Other);
        assert_eq!(
            failure_kind(root_commits(dir.path(), &mut 0)),
            FailureKind::Other
        );
    }

    /// Git does not treat a `.git` folder without HEAD as a repository, so such
    /// a stub stays a determinate "not a repository".
    #[test]
    fn stub_git_directory_without_head_is_repository_absence() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_git_directory_for_test(dir.path());
        assert_eq!(
            failure_kind(toplevel(dir.path())),
            FailureKind::NotRepository
        );
        assert_eq!(
            failure_kind(root_commits(dir.path(), &mut 0)),
            FailureKind::NotRepository
        );
    }

    #[test]
    fn broken_head_reference_is_not_an_unborn_repository() {
        let dir = tempfile::tempdir().unwrap();
        init_repository(dir.path());
        let head = std::fs::read_to_string(dir.path().join(".git/HEAD")).unwrap();
        let reference = head.trim().strip_prefix("ref: ").unwrap();
        std::fs::write(
            dir.path().join(".git").join(reference),
            "not an object id\n",
        )
        .unwrap();
        assert_eq!(
            failure_kind(root_commits(dir.path(), &mut 0)),
            FailureKind::Other
        );
    }

    #[test]
    fn ref_appearing_after_head_verification_is_not_unborn() {
        let dir = tempfile::tempdir().unwrap();
        init_repository(dir.path());
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = reached.clone();
        BEFORE_LOOSE_REF_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |reference| {
                std::fs::write(reference, "not an object id\n").unwrap();
                observed.set(true);
            }));
        });
        assert_eq!(
            failure_kind(root_commits(dir.path(), &mut 0)),
            FailureKind::Other
        );
        assert!(
            reached.get(),
            "real Git missing-HEAD verification must reach the filesystem check"
        );
    }

    #[test]
    fn empty_directory_is_repository_absence_but_unborn_repo_has_no_commit() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            failure_kind(toplevel(dir.path())),
            FailureKind::NotRepository
        );
        init_repository(dir.path());
        assert!(toplevel(dir.path()).is_ok());
        assert_eq!(
            failure_kind(root_commits(dir.path(), &mut 0)),
            FailureKind::NoCommit
        );
    }

    #[cfg(unix)]
    #[test]
    fn ambiguous_head_diagnostic_requires_verified_unborn_repository() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let output = Output {
            status: std::process::ExitStatus::from_raw(128 << 8),
            stdout: Vec::new(),
            stderr: b"fatal: ambiguous argument 'HEAD': unknown revision or path not in the working tree.\n".to_vec(),
        };
        assert_eq!(
            failure_kind(classify_failure(
                dir.path(),
                &unlimited(),
                &["rev-list", "--max-parents=0", "HEAD"],
                output,
                || false,
            )),
            FailureKind::Other
        );
    }

    #[cfg(unix)]
    #[test]
    fn signalled_probe_is_not_absence_even_with_absence_diagnostic() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let output = Output {
            // Raw wait status 9: terminated by SIGKILL, no exit code.
            status: std::process::ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: b"fatal: not a git repository (or any of the parent directories): .git\n"
                .to_vec(),
        };
        assert_eq!(
            failure_kind(classify_failure(
                dir.path(),
                &unlimited(),
                &[],
                output,
                || false
            )),
            FailureKind::Other
        );
    }

    fn commit_empty(path: &Path) {
        assert!(command()
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "root",
            ])
            .status()
            .unwrap()
            .success());
    }

    /// Git does not search at or above a ceiling directory, so a repository
    /// there cannot make a plain folder below it uncertain. The ceiling is set
    /// only in the probe's child environment, as Git receives it.
    #[test]
    fn parent_repository_above_a_ceiling_is_not_searched() {
        let parent = tempfile::tempdir().unwrap();
        init_repository(parent.path());
        let child = parent.path().join("work");
        std::fs::create_dir(&child).unwrap();
        let ceiling = parent.path().as_os_str().to_os_string();
        with_child_env_for_test(&[("GIT_CEILING_DIRECTORIES", &ceiling)], || {
            assert_eq!(failure_kind(toplevel(&child)), FailureKind::NotRepository);
            assert_eq!(
                failure_kind(root_commits(&child, &mut 0)),
                FailureKind::NotRepository
            );
        });
    }

    /// Without GIT_DISCOVERY_ACROSS_FILESYSTEM, Git stops at the first parent
    /// on another device, so a repository there cannot make the folder uncertain.
    #[cfg(unix)]
    #[test]
    fn parent_repository_on_another_device_is_not_searched() {
        use std::os::unix::process::ExitStatusExt;
        let parent = tempfile::tempdir().unwrap();
        init_repository(parent.path());
        let child = parent.path().join("mount");
        std::fs::create_dir(&child).unwrap();
        let mounted = std::fs::canonicalize(&child).unwrap();
        TEST_DEVICE_OF.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                Some(if path == mounted { 2 } else { 1 })
            }));
        });
        let absence = || Output {
            status: std::process::ExitStatus::from_raw(128 << 8),
            stdout: Vec::new(),
            stderr: b"fatal: not a git repository (or any parent up to mount point /)\n".to_vec(),
        };
        let one_filesystem = DiscoveryLimits {
            ceilings: Vec::new(),
            one_filesystem: true,
        };
        let kind_on_one_filesystem = failure_kind(classify_failure(
            &child,
            &one_filesystem,
            &[],
            absence(),
            || false,
        ));
        let kind_across = failure_kind(classify_failure(
            &child,
            &unlimited(),
            &[],
            absence(),
            || false,
        ));
        TEST_DEVICE_OF.with(|hook| *hook.borrow_mut() = None);
        assert_eq!(kind_on_one_filesystem, FailureKind::NotRepository);
        assert_eq!(kind_across, FailureKind::Other);
    }

    /// The filesystem-boundary switch is read from the child environment.
    #[test]
    fn discovery_limits_follow_the_child_environment() {
        for (value, one_filesystem) in [("0", true), ("false", true), ("true", false), ("1", false)]
        {
            let limits = with_child_env_for_test(
                &[("GIT_DISCOVERY_ACROSS_FILESYSTEM", OsStr::new(value))],
                || DiscoveryLimits::from_command(&command()),
            );
            assert_eq!(limits.one_filesystem, one_filesystem, "{value}");
        }
    }

    /// Ceiling entries follow Git: relative entries are ignored, entries are
    /// resolved through symlinks, and entries after an empty one are literal.
    #[cfg(unix)]
    #[test]
    fn ceiling_entries_follow_git_parsing_rules() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(base.join("real")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let link = base.join("link");
        let value = std::env::join_paths([
            PathBuf::from("relative"),
            link.clone(),
            base.join("missing"),
            PathBuf::new(),
            link.clone(),
        ])
        .unwrap();
        assert_eq!(
            ceiling_directories(Some(&value)),
            vec![base.join("real"), link]
        );
    }

    /// The spawn count covers every Git process, including the five that
    /// verify an unborn branch.
    #[test]
    fn root_commit_probe_counts_every_spawned_git_process() {
        let dir = tempfile::tempdir().unwrap();
        init_repository(dir.path());
        let mut unborn = 0;
        assert_eq!(
            failure_kind(root_commits(dir.path(), &mut unborn)),
            FailureKind::NoCommit
        );
        assert_eq!(unborn, 6);
        commit_empty(dir.path());
        let mut committed = 0;
        assert!(root_commits(dir.path(), &mut committed).is_ok());
        assert_eq!(committed, 1);
    }
}
