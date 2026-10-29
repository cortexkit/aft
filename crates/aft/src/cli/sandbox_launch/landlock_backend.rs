use aft::sandbox_profile::SandboxProfile;
use landlock::{
    AccessFs, BitFlags, CompatLevel, Compatible, LandlockStatus, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope, ABI,
};
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Highest ABI understood by the pinned landlock crate.
pub(super) const TARGET_ABI: ABI = ABI::V7;
/// V2 is required so handled `Refer` rights prevent link-based access widening.
pub(super) const REQUIRED_REFER_ABI: ABI = ABI::V2;
/// V3 closes the `truncate(2)` gap in the write allowlist.
pub(super) const REQUIRED_WRITE_ABI: ABI = ABI::V3;

#[derive(Debug, Clone, Copy)]
pub(super) struct AppliedLandlock {
    pub effective_abi: ABI,
    pub partially_enforced: bool,
    pub yama_same_uid_exposed: bool,
    pub signal_scope_enforced: bool,
}

pub(super) fn apply(profile: &SandboxProfile) -> Result<AppliedLandlock, String> {
    if profile.mode == aft::config::SandboxMode::Relaxed {
        // Landlock cannot subtract protected paths from a root-wide grant.
        // Grant reads and writes beneath /, then list each unenforced path on
        // stderr in emit_linux_warning; relaxed is not a Linux access boundary.
        return probe_with_signal_scope(profile.worker_jail);
    }
    // Recheck after launcher canonicalization: a changed symlink must not turn
    // the configured connection-file deny into an additive Landlock grant.
    for (grants, denies) in [
        (
            profile
                .read_allow
                .iter()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>(),
            &profile.read_deny,
        ),
        (
            profile
                .write_allow_roots()
                .into_iter()
                .chain(profile.writable_files.iter().map(PathBuf::as_path))
                .collect(),
            &profile.write_deny,
        ),
    ] {
        for grant in grants {
            for deny in denies {
                if grant.starts_with(deny) || deny.starts_with(grant) {
                    return Err(format!(
                        "Landlock grant {} overlaps mandatory deny {}",
                        grant.display(),
                        deny.display()
                    ));
                }
            }
        }
    }
    profile
        .data_policy
        .validate_grants(
            profile.read_allow.iter().map(PathBuf::as_path),
            &profile.data_policy.read_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;
    profile
        .data_policy
        .validate_grants(
            profile
                .write_allow_roots()
                .into_iter()
                .chain(profile.writable_files.iter().map(PathBuf::as_path)),
            &profile.data_policy.write_allow,
            &profile.writable_files,
        )
        .map_err(|error| error.to_string())?;
    let yama_same_uid_exposed = yama_same_uid_exposed();
    let mut session_reads = profile
        .data_policy
        .session_task_read_dir
        .as_deref()
        .map(session_task_read_paths)
        .transpose()?
        .unwrap_or_default();
    session_reads.retain(|path| {
        !profile
            .read_deny
            .iter()
            .any(|deny| path.starts_with(deny) || deny.starts_with(path))
    });
    close_inherited_fds()?;
    // Git and other standard tools open /dev/null read-write; granting only
    // file rights on this sink does not make any persistent path writable.
    apply_paths(
        profile
            .read_allow
            .iter()
            .map(|path| (path.as_path(), profile.read_allow_listed.contains(path))),
        profile
            .write_allow_roots()
            .into_iter()
            // Landlock grants rights on an opened file, so a writable file
            // that does not exist yet stays unwritable here.
            .chain(
                profile
                    .writable_files
                    .iter()
                    .map(PathBuf::as_path)
                    .filter(|file| file.is_file()),
            )
            .chain([Path::new("/dev/null")]),
        &session_reads,
        yama_same_uid_exposed,
        profile.worker_jail,
    )
}

const MAX_SESSION_READ_TASKS: usize = 500;

fn session_task_read_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    use aft::bash_background::persistence::{
        readable_task_file_names, validate_task_id, TaskLayout,
    };
    let mut tasks = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(root)
        .map_err(|error| format!("cannot enumerate session task reads: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("cannot enumerate session task entry: {error}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let metadata = match entry.path().symlink_metadata() {
            Ok(metadata) if !metadata.file_type().is_symlink() => metadata,
            _ => continue,
        };
        let (task, layout) = if validate_task_id(name).is_ok() && metadata.is_dir() {
            (name, TaskLayout::Directory)
        } else if let Some((id, _)) = name.split_once('.') {
            if validate_task_id(id).is_err() || !metadata.is_file() {
                continue;
            }
            (id, TaskLayout::Flat)
        } else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        tasks
            .entry(task.to_string())
            .and_modify(|value: &mut (std::time::SystemTime, TaskLayout)| {
                if modified > value.0 {
                    *value = (modified, layout);
                }
            })
            .or_insert((modified, layout));
    }
    let mut tasks = tasks.into_iter().collect::<Vec<_>>();
    tasks.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(&b.0)));
    // Old tasks beyond this snapshot stay unreadable; never substitute a parent
    // directory grant when a session has more saved commands than the budget.
    tasks.truncate(MAX_SESSION_READ_TASKS);
    let mut paths = Vec::new();
    for (task, (_, layout)) in tasks {
        for name in readable_task_file_names(layout) {
            let path = match layout {
                TaskLayout::Directory => root.join(&task).join(name),
                TaskLayout::Flat => root.join(format!("{task}.{name}")),
            };
            // Landlock opens every component without following symlinks. Skip
            // replaced or missing files rather than resolving them to new grants.
            if path
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.is_file())
                && path.canonicalize().is_ok_and(|canonical| canonical == path)
            {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

pub(super) fn probe() -> Result<AppliedLandlock, String> {
    probe_with_signal_scope(true)
}

fn probe_with_signal_scope(scope_signals: bool) -> Result<AppliedLandlock, String> {
    close_inherited_fds()?;
    apply_paths(
        [(Path::new("/"), false)],
        [Path::new("/")],
        &[],
        false,
        scope_signals,
    )
}

fn apply_paths<'a>(
    read_paths: impl IntoIterator<Item = (&'a Path, bool)>,
    write_paths: impl IntoIterator<Item = &'a Path>,
    exact_read_paths: &[PathBuf],
    yama_same_uid_exposed: bool,
    scope_signals: bool,
) -> Result<AppliedLandlock, String> {
    let read_access = AccessFs::from_read(TARGET_ABI);
    // Refer is a V2 write-class right. Granting it only with writable-root
    // rules keeps in-root renames working without allowing access widening.
    let write_access = AccessFs::from_write(TARGET_ABI) & !AccessFs::IoctlDev;
    let handled_access = read_access | write_access;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(handled_access)
        .map_err(|error| format!("failed to configure Landlock filesystem access: {error}"))?
        .create()
        .map_err(|error| format!("failed to create Landlock ruleset: {error}"))?;

    let mut read_paths = read_paths.into_iter().collect::<BTreeMap<_, _>>();
    for path in write_paths {
        let opened = open_rule_path(path).map_err(|error| error.to_string())?;
        let access = if read_paths.remove(path).is_some() {
            read_access | write_access
        } else {
            write_access
        };
        let access = access_for_opened_path(access, opened.is_dir);
        ruleset = ruleset
            .add_rule(PathBeneath::new(opened.fd, access))
            .map_err(|error| {
                format!(
                    "failed to grant Landlock access beneath {}: {error}",
                    path.display()
                )
            })?;
    }

    for (path, from_listing) in read_paths {
        let Some(opened) = open_read_rule_path(path, from_listing)? else {
            continue;
        };
        let access = access_for_opened_path(read_access, opened.is_dir);
        ruleset = ruleset
            .add_rule(PathBeneath::new(opened.fd, access))
            .map_err(|error| {
                format!(
                    "failed to grant Landlock reads beneath {}: {error}",
                    path.display()
                )
            })?;
    }

    for path in exact_read_paths {
        let Some(opened) = open_read_rule_path(path, true)? else {
            continue;
        };
        if opened.is_dir {
            return Err(format!(
                "session task read file became a directory: {}",
                path.display()
            ));
        }
        ruleset = ruleset
            .add_rule(PathBeneath::new(
                opened.fd,
                access_for_opened_path(read_access, false),
            ))
            .map_err(|error| format!("failed to grant Landlock task output read: {error}"))?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|error| format!("failed to restrict process with Landlock: {error}"))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("Landlock ruleset was not enforced".to_string());
    }

    let effective_abi = match status.landlock {
        LandlockStatus::Available { effective_abi, .. } => effective_abi,
        LandlockStatus::NotEnabled => return Err("Landlock is disabled by the kernel".to_string()),
        LandlockStatus::NotImplemented => {
            return Err("Landlock is not implemented by this kernel".to_string());
        }
    };
    validate_required_abi(effective_abi)?;
    if status.ruleset == RulesetStatus::PartiallyEnforced {
        return Err(
            "Landlock did not fully enforce the mandatory read, write, and refer rights"
                .to_string(),
        );
    }

    Ok(AppliedLandlock {
        effective_abi,
        partially_enforced: false,
        yama_same_uid_exposed,
        signal_scope_enforced: scope_signals && apply_signal_scope()?,
    })
}

fn apply_signal_scope() -> Result<bool, String> {
    // Keep optional ABI 6 signal scoping separate from mandatory filesystem
    // rights: an older kernel must not downgrade the filesystem enforcement
    // check merely because it cannot mediate signals.
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .scope(Scope::Signal)
        .map_err(|error| format!("failed to configure Landlock signal scope: {error}"))?
        .create()
        .map_err(|error| format!("failed to create Landlock signal scope: {error}"))?
        .restrict_self()
        .map_err(|error| format!("failed to restrict Landlock signals: {error}"))?;
    Ok(status.ruleset == RulesetStatus::FullyEnforced)
}

fn access_for_opened_path(access: BitFlags<AccessFs>, is_dir: bool) -> BitFlags<AccessFs> {
    if is_dir {
        access
    } else {
        access & AccessFs::from_file(TARGET_ABI)
    }
}

fn validate_required_abi(effective_abi: ABI) -> Result<(), String> {
    if effective_abi < REQUIRED_REFER_ABI {
        return Err(format!(
            "Landlock {} cannot enforce refer rights; {} or newer is required",
            abi_label(effective_abi),
            abi_label(REQUIRED_REFER_ABI)
        ));
    }
    if effective_abi < REQUIRED_WRITE_ABI {
        return Err(format!(
            "Landlock {} cannot enforce truncate rights; {} or newer is required",
            abi_label(effective_abi),
            abi_label(REQUIRED_WRITE_ABI)
        ));
    }
    Ok(())
}

fn yama_same_uid_exposed() -> bool {
    yama_value_same_uid_exposed(std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok())
}

fn yama_value_same_uid_exposed(value: Option<String>) -> bool {
    value
        .and_then(|value| value.trim().parse::<u32>().ok())
        .is_none_or(|scope| scope == 0)
}

fn close_inherited_fds() -> Result<(), String> {
    // Keep stdio (0-2) and the daemon-controlled markers (3-4) alive. The
    // parent deliberately remaps the exit and failure markers to 3 and 4 in
    // apply_marker_fd_allowlist; descriptors >= 5 already carry FD_CLOEXEC
    // there, so closing from 5 is defense-in-depth for a non-CLOEXEC leak,
    // not the primary descriptor-hygiene mechanism. This launcher preserves
    // only the exit and failure markers remapped to descriptors 3 and 4.
    aft::launch_fds::close_inherited()
}

struct OpenedRulePath {
    fd: OwnedFd,
    is_dir: bool,
}

fn open_read_rule_path(path: &Path, from_listing: bool) -> Result<Option<OpenedRulePath>, String> {
    match open_rule_path(path) {
        Ok(opened) => Ok(Some(opened)),
        // A child can disappear even after launcher validation. Removing this
        // read rule cannot widen access; never substitute its parent directory.
        Err(error) if from_listing && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn open_rule_path(path: &Path) -> std::io::Result<OpenedRulePath> {
    if !path.is_absolute() {
        return Err(std::io::Error::other(format!(
            "Landlock rule path is not absolute: {}",
            path.display()
        )));
    }
    if path == Path::new("/") {
        let fd = open_filesystem_root()?;
        return Ok(OpenedRulePath { fd, is_dir: true });
    }

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::other(format!(
            "Landlock rule path has no parent directory: {}",
            path.display()
        ))
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::other(format!(
            "Landlock rule path has no final component: {}",
            path.display()
        ))
    })?;
    let parent_fd = open_directory_no_symlinks(parent)?;
    let fd = open_relative_no_symlinks(parent_fd.as_raw_fd(), name, false).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("failed to securely open {}: {error}", path.display()),
        )
    })?;
    let metadata = fstat_fd(&fd).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("failed to inspect opened path {}: {error}", path.display()),
        )
    })?;
    if file_type(&metadata) == libc::S_IFLNK {
        return Err(std::io::Error::other(format!(
            "Landlock rule path resolved to a symlink: {}",
            path.display()
        )));
    }
    Ok(OpenedRulePath {
        fd,
        is_dir: file_type(&metadata) == libc::S_IFDIR,
    })
}

fn open_filesystem_root() -> std::io::Result<OwnedFd> {
    let fd = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        Err(std::io::Error::new(
            error.kind(),
            format!("failed to open filesystem root for Landlock: {}", error),
        ))
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn open_directory_no_symlinks(path: &Path) -> std::io::Result<OwnedFd> {
    let components = normalized_relative_components(path).map_err(std::io::Error::other)?;
    let root = open_filesystem_root()?;
    if components.is_empty() {
        return Ok(root);
    }

    let relative = components
        .iter()
        .fold(PathBuf::new(), |path, component| path.join(component));
    let relative = CString::new(relative.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other(format!("path contains NUL: {}", path.display())))?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            relative.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(std::io::Error::new(
            error.kind(),
            format!("openat2 secure walk failed for {}: {error}", path.display()),
        ));
    }

    let mut current = root;
    for component in components {
        current =
            open_relative_no_symlinks(current.as_raw_fd(), component, true).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "component-wise secure walk failed for {}: {error}",
                        path.display()
                    ),
                )
            })?;
    }
    Ok(current)
}

fn open_relative_no_symlinks(
    parent_fd: i32,
    name: &OsStr,
    directory: bool,
) -> Result<OwnedFd, std::io::Error> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let flags = libc::O_PATH | libc::O_CLOEXEC | if directory { libc::O_DIRECTORY } else { 0 };
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent_fd,
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as libc::c_int
    };
    if opened >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(opened) });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }

    let flags = libc::O_PATH
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | if directory { libc::O_DIRECTORY } else { 0 };
    let opened = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
    if opened < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let opened = unsafe { OwnedFd::from_raw_fd(opened) };
    let metadata = fstat_fd(&opened)?;
    if file_type(&metadata) == libc::S_IFLNK {
        return Err(std::io::Error::from_raw_os_error(libc::ELOOP));
    }
    Ok(opened)
}

fn fstat_fd(fd: &OwnedFd) -> Result<libc::stat, std::io::Error> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), metadata.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { metadata.assume_init() })
}

fn file_type(metadata: &libc::stat) -> libc::mode_t {
    metadata.st_mode & libc::S_IFMT
}

fn normalized_relative_components(path: &Path) -> Result<Vec<&OsStr>, String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(component) => components.push(component),
            _ => {
                return Err(format!(
                    "path is not normalized for secure open: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(components)
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;

pub(super) fn abi_label(abi: ABI) -> String {
    if abi == ABI::Unsupported {
        "unsupported".to_string()
    } else {
        format!("V{abi}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanished_listed_read_rule_is_omitted_after_validation() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let sibling = root.join("listed");
        let readable = sibling.join("readable");
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(&readable, "listed file").unwrap();
        let mut profile = SandboxProfile::build(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![readable.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root,
        )
        .unwrap();
        profile.read_allow_listed = vec![readable.clone()];
        let profile = profile.canonicalize_for_launch().unwrap();
        assert!(open_read_rule_path(&profile.read_allow[0], true)
            .unwrap()
            .is_some());
        std::fs::remove_file(&readable).unwrap();
        assert!(open_read_rule_path(&profile.read_allow[0], true)
            .unwrap()
            .is_none());
        std::fs::remove_dir(&sibling).unwrap();
        assert!(open_read_rule_path(&profile.read_allow[0], true)
            .unwrap()
            .is_none());
        assert!(open_read_rule_path(&profile.read_allow[0], false).is_err());
        assert!(
            open_rule_path(&profile.read_allow[0]).is_err(),
            "writes remain required"
        );
    }

    #[test]
    fn listed_read_rule_non_missing_open_errors_still_refuse() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(open_read_rule_path(&link, true).is_err());
        assert!(open_read_rule_path(&link.join("file"), true).is_err());
        assert!(open_read_rule_path(Path::new("relative"), true).is_err());
    }

    #[test]
    fn v1_and_v2_cannot_meet_the_mandatory_floor() {
        let v1 = validate_required_abi(ABI::V1).expect_err("V1 must refuse");
        assert!(v1.contains("refer rights"));
        let v2 = validate_required_abi(ABI::V2).expect_err("V2 must refuse");
        assert!(v2.contains("truncate rights"));
        validate_required_abi(ABI::V3).expect("V3 meets mandatory floor");
    }

    #[test]
    fn file_rules_drop_directory_only_access() {
        let read = AccessFs::from_read(TARGET_ABI);
        let file = access_for_opened_path(read, false);
        assert!(file.contains(AccessFs::ReadFile));
        assert!(!file.contains(AccessFs::ReadDir));
    }

    #[test]
    fn yama_missing_unparseable_and_zero_values_warn_conservatively() {
        assert!(yama_value_same_uid_exposed(None));
        assert!(yama_value_same_uid_exposed(Some("invalid".to_string())));
        assert!(yama_value_same_uid_exposed(Some("0\n".to_string())));
        assert!(!yama_value_same_uid_exposed(Some("1\n".to_string())));
    }
}

#[cfg(test)]
mod session_read_tests {
    use super::*;
    use aft::bash_background::persistence::create_task_layout;

    #[test]
    fn session_task_read_snapshot_is_exact_bounded_and_skips_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().canonicalize().unwrap();
        let mut session = PathBuf::new();
        for index in 0..=MAX_SESSION_READ_TASKS {
            let task =
                create_task_layout(&storage, "session", &format!("bash-{index:016x}")).unwrap();
            session = task.paths.session_dir.clone();
            std::fs::write(&task.paths.stdout, "output").unwrap();
            std::fs::write(&task.paths.environment, "secret").unwrap();
            filetime::set_file_mtime(
                &task.paths.dir,
                filetime::FileTime::from_unix_time(index as i64, 0),
            )
            .unwrap();
        }
        let reads = session_task_read_paths(&session).unwrap();
        assert_eq!(reads.len(), MAX_SESSION_READ_TASKS);
        assert!(!reads.contains(&session.join("bash-0000000000000000/io/stdout")));
        assert!(reads
            .iter()
            .all(|path| path.file_name().unwrap() == "stdout"));
        let path = &reads[0];
        std::fs::remove_file(path).unwrap();
        std::os::unix::fs::symlink(storage.join("private.db"), path).unwrap();
        assert_eq!(
            session_task_read_paths(&session).unwrap().len(),
            MAX_SESSION_READ_TASKS - 1
        );
    }
}
