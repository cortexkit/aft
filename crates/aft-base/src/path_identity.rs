//! Project-root identity primitives for per-checkout routing and state.
//!
//! Three distinct identity roles live here or are named here — do NOT conflate
//! them, because conflating them is the bug class this module exists to kill:
//!
//! 1. [`ProjectRootId`] (re-exported from `cortexkit-paths`) — the canonical
//!    *path* identity for ROUTING and per-root SCOPING: bridge routing, RPC
//!    port files, the (future) ProjectActor map and lease, and the
//!    bash/compression/backup/checkpoint scope keys derived via
//!    [`project_scope_key`]. Shared byte-for-byte with subc by construction
//!    (same crate), so a session attaches to the same root subc routed it to.
//!
//! 2. Repository artifact identity — use one shared cache key for data that can
//!    be reused across all worktrees of the same Git repository, and a separate
//!    key for checkout-specific inspect data. For Git repositories, derive the
//!    shared key from the repository's root commit so every worktree can reuse
//!    the same search cache; for non-Git projects, derive it from the canonical
//!    filesystem path. Keep the previous `project_cache_key` format for
//!    compatibility with existing caches.
//!
//! 3. Operation-target / lexical path handling (create-file fallback, relative
//!    path joins) — not identity; stays local to its call sites.
//!
//! Consequence of the split: two worktrees of one repo SHARE the artifact key
//! (same root commit → shared index) but get DISTINCT `ProjectRootId`s → a
//! distinct [`project_scope_key`]. A background bash task, undo history, or
//! token-savings stat created in worktree A never surfaces under worktree B.

use std::path::{Path, PathBuf};

pub use cortexkit_paths::{IdentityError, ProjectRootId};

/// Stable 16-hex scope key for per-root mutable state keyed by *canonical path*:
/// background bash tasks, compression aggregation, backup metadata, checkpoint
/// locks.
///
/// Derived from [`ProjectRootId`] (the shared canonical path), so it is the
/// per-checkout identity — distinct from `artifact_cache_key`, which is the
/// per-repository identity (root commit). Two worktrees of one repo get
/// different scope keys (correct) but the same artifact key (shared index).
///
/// Non-existent roots fall back to a lexical normalization so derivation is
/// total: the shared crate rejects non-existent paths, and while runtime
/// project roots always exist, `current_dir`-derived callers (e.g. a default
/// checkpoint store) and tests may pass a path that does not.
pub fn project_scope_key(project_root: &Path) -> String {
    let canonical = ProjectRootId::from_path(project_root)
        .map(ProjectRootId::into_path_buf)
        .unwrap_or_else(|_| lexical_normalize(project_root));
    hash16(&canonical)
}

/// Inputs are canonical policy identities, including home aliases and native
/// volume roots. Exclude system trees, root-level folders and all home ancestors.
pub fn directory_scope_is_narrow(directory: &Path, home: &Path, volume: &Path) -> bool {
    project_root_path_is_narrow(directory, home, volume)
        && directory.parent().and_then(Path::parent).is_some()
        && directory.parent() != Some(volume)
        && !directory_is_system_tree(directory)
}

/// These trees may contain unrelated users' or system data even several levels
/// below their root. Only macOS's per-user temporary tree is exempt; component
/// comparisons keep similarly named ordinary folders eligible.
#[cfg(unix)]
fn directory_is_system_tree(directory: &Path) -> bool {
    if ["/var/folders", "/private/var/folders"]
        .iter()
        .any(|root| directory.starts_with(root))
    {
        return false;
    }
    [
        "/usr",
        "/etc",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/libexec",
        "/opt",
        "/var",
        "/System",
        "/Library",
        "/private/etc",
        "/private/var",
    ]
    .iter()
    .any(|root| directory.starts_with(root))
}

#[cfg(windows)]
fn directory_is_system_tree(directory: &Path) -> bool {
    directory
        .components()
        .find_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .is_some_and(|name| {
            matches!(
                name.as_str(),
                "windows" | "program files" | "program files (x86)"
            )
        })
}

/// Decide whether a canonical project root admits watching and indexing.
/// A volume's direct children can be ordinary projects, unlike permission-card
/// directory scopes, which also exclude root-level containers.
/// Unknown protected-directory identities must not admit background work.
pub(crate) fn project_root_is_narrow(directory: &Path, home: &Path, volume: &Path) -> Option<bool> {
    if !project_root_path_is_narrow(directory, home, volume)
        || project_root_is_shared_temp(directory)
    {
        return Some(false);
    }
    protected_directory_alias(directory, home, volume).map(|protected| !protected)
}

/// Shared temporary roots hold other applications' files on any mount layout.
/// Compare canonical paths, not prefixes, so projects below them stay enabled.
fn project_root_is_shared_temp(directory: &Path) -> bool {
    let directory = crate::policy_path::policy_path(directory);
    let platform_temp = std::env::temp_dir();
    let roots = std::iter::once(platform_temp.as_path());
    #[cfg(unix)]
    let roots = roots.chain([
        Path::new("/tmp"),
        Path::new("/var/tmp"),
        Path::new("/private/tmp"),
        Path::new("/private/var/tmp"),
    ]);
    let shared = roots
        .filter_map(|root| ProjectRootId::from_path(root).ok())
        .any(|root| crate::policy_path::policy_path(&root.into_path_buf()) == directory);
    shared
}

fn project_root_path_is_narrow(directory: &Path, home: &Path, volume: &Path) -> bool {
    directory.is_absolute()
        && directory.parent().is_some()
        && !home.starts_with(directory)
        && directory != volume
}

/// The volume a canonical project root is judged against when deciding whether
/// it is too broad to watch and index. `None` means the volume is unknown.
///
/// The device walk in `volume_anchor` stops at any directory whose device
/// differs from its parent's. On Linux that includes a bind mount of a
/// subdirectory, such as a dev container's `-v "$PWD":/workspace`: the mount
/// point looks like a volume root to the device walk, although it exposes only
/// a subdirectory of another filesystem. For a root that is itself such a
/// mount point, judge it against the volume that contains the mount point
/// instead. Other platforms keep the device walk.
pub(crate) fn project_root_volume(canonical_root: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let (volume, _) = crate::volume::volume_anchor(canonical_root)?;
    #[cfg(target_os = "linux")]
    return project_root_volume_with(canonical_root, volume, home, read_mountinfo, |path| {
        crate::volume::volume_anchor(path).map(|(volume, _)| volume)
    });
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (canonical_root, home);
        Some(volume)
    }
}

/// Testable core of [`project_root_volume`]: `read` supplies the raw bytes of
/// `/proc/self/mountinfo` and `volume_of` runs the device walk for a path.
#[cfg(target_os = "linux")]
fn project_root_volume_with(
    root: &Path,
    volume: PathBuf,
    home: Option<&Path>,
    read: impl FnOnce() -> std::io::Result<Vec<u8>>,
    volume_of: impl FnOnce(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    // Only a root that is itself the mount point is misjudged; a directory
    // below a mount point is already narrow. The filesystem root stays broad
    // whatever it mounts.
    if root != volume {
        return Some(volume);
    }
    let Some(parent) = volume.parent() else {
        return Some(volume);
    };
    let mountinfo = match read() {
        Ok(text) => text,
        Err(error) => {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                crate::slog_warn!(
                    "cannot read {MOUNTINFO_PATH} ({error}); mount points are judged as volume roots"
                );
            });
            return Some(volume);
        }
    };
    match mount_root_in(&mountinfo, &volume) {
        MountRoot::Subdirectory(source_root) if !source_root_is_home_like(&source_root, home) => {
            volume_of(parent)
        }
        _ => Some(volume),
    }
}

#[cfg(target_os = "linux")]
const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";
/// Hosts with many containers can list thousands of mounts; a larger table is
/// treated as unreadable rather than parsed in part, because a cut table could
/// miss the entry that is mounted on top.
#[cfg(target_os = "linux")]
const MOUNTINFO_MAX_BYTES: u64 = 16 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MOUNTINFO_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

#[cfg(target_os = "linux")]
fn read_mountinfo() -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    // Read under this function's own deadline instead of the caller's I/O
    // budget: a failed read recorded in that budget would make the rest of the
    // configure request's file access fail too.
    crate::bounded_io::with_budget(None, || {
        crate::bounded_io::run(
            Path::new(MOUNTINFO_PATH),
            Some(std::time::Instant::now() + MOUNTINFO_BUDGET),
            |path| {
                let mut bytes = Vec::new();
                std::fs::File::open(&path)?
                    .take(MOUNTINFO_MAX_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MOUNTINFO_MAX_BYTES {
                    return Err(std::io::Error::other("mount table exceeds the size limit"));
                }
                Ok(bytes)
            },
        )
    })
}

/// What a mount exposes of its filesystem, read from one mountinfo entry.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum MountRoot {
    /// The filesystem's root, or the root of a whole btrfs subvolume.
    WholeVolume,
    /// A subdirectory of the filesystem, as a bind mount exposes. Holds the
    /// subdirectory's path inside its filesystem.
    Subdirectory(PathBuf),
    /// No entry for the mount point, or one this parser cannot interpret.
    Unknown,
}

/// Classify the mount at `mount_point` from the contents of
/// `/proc/self/mountinfo`. Line format (proc(5)): id, parent id, major:minor,
/// root, mount point, mount options, optional fields, `-`, filesystem type,
/// source, super options.
///
/// The table is parsed as bytes: Linux names may hold any byte except `/` and
/// NUL, and mountinfo octal-escapes only whitespace and backslashes, leaving
/// non-UTF-8 bytes as they are. A non-UTF-8 mount elsewhere in the table must
/// not make the requested mount's entry unreadable.
#[cfg(target_os = "linux")]
fn mount_root_in(mountinfo: &[u8], mount_point: &Path) -> MountRoot {
    // Mounts stacked on one mount point are listed in mount order, so the last
    // matching entry is the one that is visible.
    let Some(fields) = mountinfo
        .split(|byte| *byte == b'\n')
        .map(|line| line.split(|byte| *byte == b' ').collect::<Vec<_>>())
        .filter(|fields| {
            fields
                .get(4)
                .is_some_and(|point| unescape_mount_field(point) == mount_point)
        })
        .last()
    else {
        return MountRoot::Unknown;
    };
    let Some(separator) = fields
        .iter()
        .skip(6)
        .position(|field| *field == b"-".as_slice())
    else {
        return MountRoot::Unknown;
    };
    let tail = &fields[6 + separator + 1..];
    let (Some(fs_type), Some(super_options)) = (tail.first(), tail.get(2)) else {
        return MountRoot::Unknown;
    };
    let root = unescape_mount_field(fields[3]);
    if !root.is_absolute() {
        return MountRoot::Unknown;
    }
    if root == Path::new("/") {
        return MountRoot::WholeVolume;
    }
    // A btrfs subvolume mount reports the subvolume's path as its root, so a
    // root other than `/` only means a bind of a subdirectory when it lies
    // below the mounted subvolume.
    let subvolume = super_options
        .split(|byte| *byte == b',')
        .find_map(|option| option.strip_prefix(b"subvol=".as_slice()))
        .map(unescape_mount_field);
    match subvolume {
        Some(subvolume) if root == subvolume => MountRoot::WholeVolume,
        Some(subvolume) if root.starts_with(&subvolume) => MountRoot::Subdirectory(root),
        Some(_) => MountRoot::Unknown,
        // Subvolume-capable filesystems that do not name the subvolume give no
        // way to tell a subvolume root from a bind.
        None if matches!(*fs_type, b"btrfs" | b"bcachefs") => MountRoot::Unknown,
        None => MountRoot::Subdirectory(root),
    }
}

/// Decode the octal escapes (`\040` for a space, for example) that mountinfo
/// uses for whitespace and backslashes in paths. Other bytes, including
/// non-UTF-8 ones, are kept as they are.
#[cfg(target_os = "linux")]
fn unescape_mount_field(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = bytes.get(index + 1..index + 4).filter(|digits| {
            bytes[index] == b'\\' && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        });
        match octal {
            Some(digits) => {
                let value = digits
                    .iter()
                    .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                decoded.push(value as u8);
                index += 4;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    PathBuf::from(std::ffi::OsStr::from_bytes(&decoded))
}

/// A bind of a home folder or of a folder containing home folders exposes as
/// much as opening that folder directly, so it stays too broad. The source path
/// comes from another machine or namespace (the host of a container), so the
/// account home is not enough: conventional home locations count too.
#[cfg(target_os = "linux")]
fn source_root_is_home_like(source_root: &Path, home: Option<&Path>) -> bool {
    if home.is_some_and(|home| home.starts_with(source_root)) {
        return true;
    }
    let names: Vec<&str> = source_root
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_str().unwrap_or("")),
            _ => None,
        })
        .collect();
    matches!(
        names.as_slice(),
        [] | ["root"]
            | ["home"]
            | ["home", _]
            | ["Users"]
            | ["Users", _]
            | ["var", "home"]
            | ["var", "home", _]
    )
}

/// Bind mounts can preserve a protected directory's identity without preserving
/// its path spelling. Device/inode checks also catch same-device volume aliases.
#[cfg(unix)]
pub fn directory_is_protected_alias(directory: &Path, home: &Path, volume: &Path) -> Option<bool> {
    if protected_directory_alias(directory, home, volume)? {
        return Some(true);
    }
    Some(directory_identity(directory.parent()?)? == directory_identity(volume)?)
}

#[cfg(unix)]
fn directory_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn protected_directory_alias(directory: &Path, home: &Path, volume: &Path) -> Option<bool> {
    let candidate = directory_identity(directory)?;
    for ancestor in home.ancestors().chain(std::iter::once(volume)) {
        if directory_identity(ancestor)? == candidate {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(not(unix))]
fn protected_directory_alias(_directory: &Path, _home: &Path, _volume: &Path) -> Option<bool> {
    Some(false)
}

#[cfg(not(unix))]
pub fn directory_is_protected_alias(
    _directory: &Path,
    _home: &Path,
    _volume: &Path,
) -> Option<bool> {
    Some(false)
}

fn hash16(path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..16].to_string()
}

/// Resolve lexical dot components without touching the filesystem. Leading
/// relative parents remain significant; rooted paths cannot escape their root.
/// This is not containment validation, and comparison callers apply any
/// Windows verbatim-prefix policy before passing their path here.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => match result.components().next_back() {
                Some(Component::Normal(_)) => {
                    result.pop();
                }
                Some(Component::RootDir) => {}
                Some(Component::Prefix(_)) if path.has_root() => {}
                _ => result.push(component),
            },
            Component::CurDir => {}
            other => result.push(other),
        }
    }
    result
}

pub fn normalize_path(path: &Path) -> PathBuf {
    // Strip Windows verbatim prefixes before component normalization so paths
    // that went through fs::canonicalize (\\?\C:\ form) compare equal to paths
    // that never did. The oxc engine's resolver applies the same rule; the two
    // normalizers must stay in agreement or membership/strip_prefix checks
    // that cross the engine boundary silently miss on Windows.
    #[cfg(windows)]
    let path = &crate::windows_path::normalize_windows_path(path);

    crate::path_identity::lexical_normalize(path)
}

/// Canonicalize a path for comparison inside the inspect subsystem.
///
/// Never returns the raw `fs::canonicalize` result: on Windows that is a
/// verbatim (`\\?\C:\`) path, and mixing verbatim and non-verbatim forms in
/// the same comparison is this subsystem's recurring bug class. Both branches
/// route through [`normalize_path`].
pub fn canonicalize_normalized(path: &Path) -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    CANONICALIZE_CALLS.with(|calls| {
        if let Some(calls) = calls.borrow_mut().as_mut() {
            calls.push(path.to_path_buf());
        }
    });
    match std::fs::canonicalize(path) {
        Ok(canonical) => normalize_path(&canonical),
        Err(_) => normalize_path(path),
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static CANONICALIZE_CALLS: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) };
}
