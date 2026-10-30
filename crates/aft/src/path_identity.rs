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
pub(crate) fn directory_scope_is_narrow(directory: &Path, home: &Path, volume: &Path) -> bool {
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
    if !project_root_path_is_narrow(directory, home, volume) {
        return Some(false);
    }
    protected_directory_alias(directory, home, volume).map(|protected| !protected)
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
    let (volume, _) = crate::storage_retention::volume_anchor(canonical_root)?;
    #[cfg(target_os = "linux")]
    return project_root_volume_with(canonical_root, volume, home, read_mountinfo, |path| {
        crate::storage_retention::volume_anchor(path).map(|(volume, _)| volume)
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
pub(crate) fn directory_is_protected_alias(
    directory: &Path,
    home: &Path,
    volume: &Path,
) -> Option<bool> {
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
pub(crate) fn directory_is_protected_alias(
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
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
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

#[cfg(test)]
mod tests {
    #[test]
    fn project_root_volume_children_are_not_permission_scopes() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        for name in ["app", "workspace", "src", "code"] {
            let project = volume.join(name);
            std::fs::create_dir(&project).unwrap();
            assert_eq!(
                super::project_root_is_narrow(&project, &home, &volume),
                Some(true),
                "a project directly below a volume must stay enabled: {name}"
            );
            assert!(
                !super::directory_scope_is_narrow(&project, &home, &volume),
                "permission-card scopes must remain stricter than project roots"
            );
            #[cfg(unix)]
            assert_eq!(
                super::directory_is_protected_alias(&project, &home, &volume),
                Some(true)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_root_paths_allow_container_roots_but_not_protected_roots() {
        let home = std::path::Path::new("/home/person");
        let volume = std::path::Path::new("/");
        for project in ["/app", "/workspace", "/src", "/code"] {
            assert!(super::project_root_path_is_narrow(
                std::path::Path::new(project),
                home,
                volume
            ));
        }
        for protected in ["/", "/home", "/home/person"] {
            assert!(!super::project_root_path_is_narrow(
                std::path::Path::new(protected),
                home,
                volume
            ));
        }
        let mounted_volume = std::path::Path::new("/Volumes/Work");
        assert!(!super::project_root_path_is_narrow(
            mounted_volume,
            home,
            mounted_volume
        ));
        assert!(super::project_root_path_is_narrow(
            std::path::Path::new("/Volumes/Work/app"),
            home,
            mounted_volume,
        ));
    }

    #[cfg(windows)]
    #[test]
    fn project_root_paths_allow_drive_children_but_not_protected_roots() {
        let home = std::path::Path::new(r"C:\Users\person");
        let volume = std::path::Path::new(r"C:\");
        for project in [r"C:\app", r"C:\workspace", r"C:\src", r"C:\code"] {
            assert!(super::project_root_path_is_narrow(
                std::path::Path::new(project),
                home,
                volume
            ));
        }
        for protected in [volume, home, home.parent().unwrap()] {
            assert!(!super::project_root_path_is_narrow(protected, home, volume));
        }
    }

    #[test]
    fn project_root_protected_directories_remain_disabled() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        for root in [
            &volume,
            &home,
            home.parent().unwrap(),
            volume.ancestors().last().unwrap(),
        ] {
            assert_eq!(
                super::project_root_is_narrow(root, &home, &volume),
                Some(false)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_root_unknown_identity_is_not_admitted() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        let missing = volume.join("missing/project");
        assert_eq!(
            super::project_root_is_narrow(&missing, &home, &volume),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_root_protected_aliases_remain_disabled() {
        let volume = tempfile::tempdir().unwrap();
        let volume = volume.path().canonicalize().unwrap();
        let home = volume.join("users/person");
        std::fs::create_dir_all(&home).unwrap();
        // Following symlinks gives the same inode identities as protected
        // directories without requiring privileged bind mounts in the test.
        for (name, target) in [
            ("app", home.as_path()),
            ("workspace", home.parent().unwrap()),
            ("code", volume.as_path()),
        ] {
            let alias = volume.join(name);
            std::os::unix::fs::symlink(target, &alias).unwrap();
            assert!(super::project_root_path_is_narrow(&alias, &home, &volume));
            assert_eq!(
                super::project_root_is_narrow(&alias, &home, &volume),
                Some(false)
            );
        }
    }

    use super::*;
    use std::fs;

    /// Mount-table fixtures in the `/proc/self/mountinfo` format, judged the
    /// way a container sees its bind-mounted workspace: the project root is on
    /// a different device from its parent, and the parent is on the
    /// container's overlay root `/`.
    #[cfg(target_os = "linux")]
    mod mount_table {
        use super::super::{
            mount_root_in, project_root_path_is_narrow, project_root_volume_with, MountRoot,
        };
        use std::path::{Path, PathBuf};

        const OVERLAY_ROOT: &str = "1748 1 0:173 / / rw,relatime master:1 - overlay overlay rw,lowerdir=/l,upperdir=/u,workdir=/w\n";

        fn narrow_with(root: &str, read: impl FnOnce() -> std::io::Result<Vec<u8>>) -> bool {
            let root = Path::new(root);
            let home = Path::new("/root");
            let volume =
                project_root_volume_with(root, root.to_path_buf(), Some(home), read, |_| {
                    Some(PathBuf::from("/"))
                });
            volume.is_some_and(|volume| project_root_path_is_narrow(root, home, &volume))
        }

        fn narrow(root: &str, mount: &str) -> bool {
            narrow_with(root, || Ok(format!("{OVERLAY_ROOT}{mount}\n").into_bytes()))
        }

        #[test]
        fn bind_of_a_subdirectory_is_narrow() {
            // `docker run -v /tmp/repro/proj-x:/proj` and `-v /tmp/repro:/data`.
            assert!(narrow(
                "/proj",
                "1801 1748 8:1 /tmp/repro/proj-x /proj ro,relatime - ext4 /dev/sda1 rw"
            ));
            assert!(narrow(
                "/data",
                "1801 1748 8:1 /tmp/repro /data ro,relatime shared:5 master:1 - ext4 /dev/sda1 rw"
            ));
            assert!(narrow(
                "/workspace",
                "1801 1748 8:1 /var/lib/docker/volumes/x/_data /workspace rw - ext4 /dev/sda1 rw"
            ));
            assert!(narrow(
                "/work space",
                "1801 1748 8:1 /srv/my\\040proj /work\\040space rw - xfs /dev/sda1 rw"
            ));
        }

        #[test]
        fn whole_filesystem_mount_is_broad() {
            let mount = "1801 1748 8:17 / /data rw,relatime shared:5 - ext4 /dev/sdb1 rw";
            assert!(!narrow("/data", mount));
            // Classified as a whole volume, not as a bind that only the
            // home-folder rule keeps broad.
            assert_eq!(
                mount_root_in(mount.as_bytes(), Path::new("/data")),
                MountRoot::WholeVolume
            );
        }

        #[test]
        fn mount_on_top_of_a_bind_decides() {
            assert!(!narrow(
                "/data",
                "1801 1748 8:1 /tmp/repro /data rw - ext4 /dev/sda1 rw\n\
                 1802 1801 8:17 / /data rw - ext4 /dev/sdb1 rw"
            ));
        }

        #[test]
        fn btrfs_subvolume_root_is_broad() {
            assert!(!narrow(
                "/data",
                "1801 1748 0:45 /@data /data rw,relatime shared:5 - btrfs /dev/sdb2 rw,space_cache=v2,subvolid=257,subvol=/@data"
            ));
            // Without a `subvol=` option a subvolume cannot be told from a bind.
            assert!(!narrow(
                "/data",
                "1801 1748 0:45 /@data /data rw - btrfs /dev/sdb2 rw,space_cache=v2"
            ));
            // A root outside the named subvolume is not understood.
            assert!(!narrow(
                "/data",
                "1801 1748 0:45 /@other /data rw - btrfs /dev/sdb2 rw,subvolid=257,subvol=/@data"
            ));
            // A bind of a folder inside the subvolume is a project.
            assert!(narrow(
                "/data",
                "1801 1748 0:45 /@data/proj /data rw - btrfs /dev/sdb2 rw,subvolid=257,subvol=/@data"
            ));
        }

        #[test]
        fn bind_of_a_home_folder_is_broad() {
            for source in ["/home/u", "/home", "/Users/u", "/root", "/var/home/u"] {
                assert!(
                    !narrow(
                        "/workspace",
                        &format!("1801 1748 8:1 {source} /workspace rw - ext4 /dev/sda1 rw")
                    ),
                    "{source}"
                );
            }
            // The account home and its ancestors count wherever they live.
            let home = Path::new("/srv/people/u");
            for source in ["/srv/people/u", "/srv/people", "/srv"] {
                let root = Path::new("/workspace");
                let volume = project_root_volume_with(
                    root,
                    root.to_path_buf(),
                    Some(home),
                    || {
                        Ok(format!(
                            "{OVERLAY_ROOT}1801 1748 8:1 {source} /workspace rw - ext4 /dev/sda1 rw\n"
                        )
                        .into_bytes())
                    },
                    |_| Some(PathBuf::from("/")),
                );
                assert_eq!(volume.as_deref(), Some(root), "{source}");
            }
        }

        #[test]
        fn filesystem_root_is_broad() {
            // A sandbox whose `/` is itself a bind of a host subdirectory.
            assert!(!narrow_with("/", || Ok(
                b"1748 317 9:5 /runner/sandboxes/x/root / ro - xfs /dev/md5 rw\n".to_vec()
            )));
        }

        #[test]
        fn non_utf8_mount_names_do_not_hide_a_bind() {
            // Linux names may hold any byte except `/` and NUL, and mountinfo
            // leaves non-UTF-8 bytes unescaped, so a mount with such bytes
            // elsewhere in the table must not stop the `/workspace` entry from
            // being parsed.
            let table = [
                OVERLAY_ROOT.as_bytes(),
                b"1802 1748 8:1 /srv/caf\xe9 /mnt/caf\xe9 rw - ext4 /dev/sda1 rw\n",
                b"1801 1748 8:1 /tmp/repro/proj-x /workspace rw - ext4 /dev/sda1 rw\n",
            ]
            .concat();
            assert!(narrow_with("/workspace", || Ok(table)));
        }

        #[test]
        fn non_utf8_mount_point_matches_its_entry() {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let root = Path::new(OsStr::from_bytes(b"/caf\xe9"));
            let table = [
                OVERLAY_ROOT.as_bytes(),
                b"1801 1748 8:1 /srv/proj\xff /caf\xe9 rw - ext4 /dev/sda1 rw\n",
            ]
            .concat();
            assert_eq!(
                mount_root_in(&table, root),
                MountRoot::Subdirectory(PathBuf::from(OsStr::from_bytes(b"/srv/proj\xff")))
            );
            let volume = project_root_volume_with(
                root,
                root.to_path_buf(),
                Some(Path::new("/root")),
                || Ok(table.clone()),
                |_| Some(PathBuf::from("/")),
            );
            assert_eq!(volume, Some(PathBuf::from("/")));
        }

        #[test]
        fn unreadable_mount_table_keeps_the_device_answer() {
            assert!(!narrow_with("/proj", || Err(std::io::Error::other(
                "permission denied"
            ))));
            // A mount point missing from the table is not understood either.
            assert!(!narrow(
                "/proj",
                "1801 1748 8:1 /tmp/repro/proj-x /elsewhere rw - ext4 /dev/sda1 rw"
            ));
        }

        #[test]
        fn root_below_its_mount_point_keeps_its_volume() {
            let volume = project_root_volume_with(
                Path::new("/data/proj"),
                PathBuf::from("/data"),
                None,
                || panic!("the mount table is only read for a mount point root"),
                |_| panic!("no re-anchoring below a mount point"),
            );
            assert_eq!(volume, Some(PathBuf::from("/data")));
        }
    }

    #[test]
    fn lexical_normalization_preserves_parents_and_root_boundaries() {
        for (input, expected) in [
            ("../../x", "../../x"),
            ("../a/../../x", "../../x"),
            ("a/../b", "b"),
            ("a/../../x", "../x"),
        ] {
            assert_eq!(lexical_normalize(Path::new(input)), Path::new(expected));
        }
        #[cfg(unix)]
        assert_eq!(lexical_normalize(Path::new("/../../x")), Path::new("/x"));
    }

    #[cfg(windows)]
    #[test]
    fn lexical_normalization_preserves_drive_relative_parents() {
        assert_eq!(
            lexical_normalize(Path::new(r"C:..\..\x")),
            Path::new(r"C:..\..\x")
        );
        assert_eq!(
            lexical_normalize(Path::new(r"C:\..\..\x")),
            Path::new(r"C:\x")
        );
    }

    /// A fresh root directory, removed when the returned guard drops.
    fn temp_root(label: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("aft-path-identity-{label}-"))
            .tempdir()
            .expect("create temp root");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn scope_key_is_stable_and_16_hex() {
        let (_root_dir, root) = temp_root("stable");
        let a = project_scope_key(&root);
        let b = project_scope_key(&root);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn scope_key_distinguishes_distinct_roots() {
        let (_a_dir, a) = temp_root("root-a");
        let (_b_dir, b) = temp_root("root-b");
        assert_ne!(project_scope_key(&a), project_scope_key(&b));
    }

    #[test]
    fn scope_key_canonicalizes_equivalent_spellings() {
        let (_root_dir, root) = temp_root("spelling");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("create nested");
        // `root/nested/..` and `root/.` both canonicalize to `root`.
        assert_eq!(
            project_scope_key(&root),
            project_scope_key(&nested.join("..")),
        );
        assert_eq!(project_scope_key(&root), project_scope_key(&root.join(".")));
    }

    #[test]
    fn scope_key_total_on_non_existent_root() {
        // Non-existent path: lexical fallback, no panic, stable.
        let missing = std::env::temp_dir().join("aft-path-identity-definitely-missing-xyz/sub/..");
        let key = project_scope_key(&missing);
        assert_eq!(key.len(), 16);
    }

    /// The headline P0 invariant: a linked git worktree SHARES the main
    /// checkout's artifact cache key (same root commit → shared index, opened
    /// read-only) but gets a DISTINCT scope key (its own checkout path → its own
    /// bash/compression/backup/checkpoint namespace). Before the split, both
    /// were the same root-commit key, so worktree A's tasks/undo bled into B.
    #[test]
    fn worktree_shares_artifact_key_but_has_distinct_scope_key() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        use std::process::Command;

        let mut git_version = Command::new("git");
        let git_ok = crate::test_env::apply_hermetic_git_env(&mut git_version)
            .arg("--version")
            .output()
            .is_ok();
        if !git_ok {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = std::env::temp_dir().join(format!(
            "aft-worktree-iso-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = tmp.join("main");
        fs::create_dir_all(&main).expect("create main checkout");

        let run = |args: &[&str], cwd: &std::path::Path| {
            let mut command = Command::new("git");
            assert!(
                crate::test_env::apply_hermetic_git_env(command.current_dir(cwd))
                    .args(args)
                    .status()
                    .expect("run git")
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init"], &main);
        fs::write(main.join("f.txt"), "x\n").expect("write file");
        run(&["add", "."], &main);
        run(
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.x",
                "commit",
                "-m",
                "init",
            ],
            &main,
        );

        let worktree = tmp.join("wt");
        run(
            &[
                "worktree",
                "add",
                worktree.to_str().unwrap(),
                "-b",
                "feature",
            ],
            &main,
        );

        let main_artifact = crate::search_index::artifact_cache_key(&main);
        let wt_artifact = crate::search_index::artifact_cache_key(&worktree);
        let main_scope = project_scope_key(&main);
        let wt_scope = project_scope_key(&worktree);

        // Same repo (same root commit) → SHARED artifact key (worktree reuses
        // main's on-disk index, read-only).
        assert_eq!(
            main_artifact, wt_artifact,
            "worktree must share the main checkout's artifact cache key"
        );
        // Distinct checkout path → DISTINCT scope key (no bash/undo/stats bleed).
        assert_ne!(
            main_scope, wt_scope,
            "worktree must get its own per-checkout scope key"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
