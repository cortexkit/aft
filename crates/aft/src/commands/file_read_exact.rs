use crate::access_policy::{AccessDecision, AccessOperation, AccessPolicy};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(crate) const OPERATION: &str = "file.read_exact";
const MAX_BYTES: u64 = 1024 * 1024;

pub(crate) struct ExactRead {
    pub value: Value,
    pub resolved: PathBuf,
}

#[derive(Default)]
pub(crate) struct ReadHooks {
    #[cfg(test)]
    pub mid_read: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Runs after the path and policy checks, before the file is identified
    /// and opened, so tests can swap checkout entries inside that window.
    #[cfg(test)]
    pub after_check: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

pub(crate) fn read_exact(
    path: &Path,
    root: &Path,
    policy: &AccessPolicy,
    hooks: ReadHooks,
) -> Result<ExactRead, ReadExactRefusal> {
    read_with(path, root, policy, open_contents, hooks)
}

fn read_with(
    path: &Path,
    root: &Path,
    policy: &AccessPolicy,
    open: impl FnOnce(&Path, &Path) -> Result<File, ReadExactRefusal>,
    _hooks: ReadHooks,
) -> Result<ExactRead, ReadExactRefusal> {
    let joined = root.join(path);
    let resolved = crate::inspect::job::normalize_path(&joined.canonicalize().map_err(open_error)?);
    let bind_root = crate::inspect::job::normalize_path(&root.canonicalize().map_err(open_error)?);
    // Delegation is narrower than head authority: flow files must stay in its checkout.
    if !resolved.starts_with(&bind_root) {
        return Err(ReadExactRefusal::AccessDenied);
    }
    // Resolve aliases before consulting grants or floors, and before opening contents.
    if policy.decide(AccessOperation::Read, &resolved, false) != AccessDecision::Allow
        || sqlite_file(&joined)
        || sqlite_file(&resolved)
    {
        return Err(ReadExactRefusal::AccessDenied);
    }
    #[cfg(test)]
    if let Some(hook) = &_hooks.after_check {
        hook();
    }
    let checked = path_identity(&resolved).map_err(open_error)?;
    // The checks above judged a path; a concurrent writer can still swap a
    // directory in it for a symlink before this open. The open must not
    // follow such a swap, and the opened file's real location is judged again.
    let mut file = open(&bind_root, &resolved)?;
    let opened = opened_location(&file).map_err(|_| ReadExactRefusal::IoError)?;
    let opened = crate::inspect::job::normalize_path(&opened);
    if !opened.starts_with(&bind_root)
        || policy.decide(AccessOperation::Read, &opened, false) != AccessDecision::Allow
        || sqlite_file(&opened)
    {
        return Err(ReadExactRefusal::AccessDenied);
    }
    let metadata = file.metadata().map_err(|_| ReadExactRefusal::IoError)?;
    if !metadata.is_file() {
        return Err(ReadExactRefusal::NotRegular);
    }
    if checked != handle_identity(&file).map_err(|_| ReadExactRefusal::IoError)? {
        return Err(ReadExactRefusal::IoError);
    }
    if metadata.len() > MAX_BYTES {
        return Err(ReadExactRefusal::TooLarge);
    }
    #[cfg(test)]
    let bytes = bounded_bytes(HookedRead {
        file: &mut file,
        hook: _hooks.mid_read,
    })?;
    #[cfg(not(test))]
    let bytes = bounded_bytes(&mut file)?;
    // Callers re-check the head's policy after the read; give them the place
    // the bytes actually came from rather than the path that was asked for.
    Ok(ExactRead {
        value: encode_bytes(&bytes),
        resolved: opened,
    })
}

#[cfg(test)]
struct HookedRead<'a> {
    file: &'a mut File,
    hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

#[cfg(test)]
impl Read for HookedRead<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(bytes)?;
        if count > 0 {
            if let Some(hook) = self.hook.take() {
                hook();
            }
        }
        Ok(count)
    }
}

fn encode_bytes(bytes: &[u8]) -> Value {
    json!({"bytes_base64": STANDARD.encode(bytes), "size": bytes.len(), "sha256": format!("{:x}", Sha256::digest(bytes))})
}

fn bounded_bytes(reader: impl Read) -> Result<Vec<u8>, ReadExactRefusal> {
    let mut bytes = Vec::new();
    // One lookahead byte distinguishes an exact-cap file from a growing file.
    reader
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadExactRefusal::IoError)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(ReadExactRefusal::TooLarge);
    }
    Ok(bytes)
}

fn sqlite_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.ends_with(".db") || name.ends_with("-wal") || name.ends_with("-shm")
}

fn open_error(error: io::Error) -> ReadExactRefusal {
    if error.kind() == io::ErrorKind::NotFound {
        ReadExactRefusal::NotFound
    } else {
        ReadExactRefusal::IoError
    }
}

/// Opens `target` by walking down from an opened `root` directory, one
/// component at a time, without following a symlink at any step.
///
/// `target` is the canonical path the checks approved, so it has no symlink
/// components; one found during the walk was swapped in after the checks and
/// is refused. O_NOFOLLOW on a plain path open would protect only the final
/// component and still follow a swapped parent directory out of the checkout.
#[cfg(unix)]
fn open_contents(root: &Path, target: &Path) -> Result<File, ReadExactRefusal> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let relative = target
        .strip_prefix(root)
        .map_err(|_| ReadExactRefusal::AccessDenied)?;
    let names = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => {
                std::ffi::CString::new(name.as_bytes()).map_err(|_| ReadExactRefusal::AccessDenied)
            }
            _ => Err(ReadExactRefusal::AccessDenied),
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Nonblocking opens prevent a FIFO replacement from stalling before fstat.
    let mut handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root)
        .map_err(open_error)?;
    for (index, name) in names.iter().enumerate() {
        let last = index + 1 == names.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: `handle` is an open directory and `name` is NUL-terminated.
        let fd = unsafe { libc::openat(handle.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            // A parent that is no longer a real directory (a symlink gives
            // ELOOP or ENOTDIR) was swapped in after the checks.
            if !last && matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                return Err(ReadExactRefusal::AccessDenied);
            }
            return Err(open_error(error));
        }
        // SAFETY: `fd` was just returned by openat and nothing else owns it.
        handle = unsafe { File::from_raw_fd(fd) };
    }
    Ok(handle)
}

/// Windows has no component-relative open here, so the parent directories
/// are resolved by path; `opened_location` then judges where the handle
/// really points before any byte is read.
#[cfg(windows)]
fn open_contents(_root: &Path, target: &Path) -> Result<File, ReadExactRefusal> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(target)
        .map_err(open_error)
}

/// Where an open file really is, asked of the kernel through its handle.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn opened_location(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Where an open file really is, asked of the kernel through its handle.
#[cfg(target_os = "macos")]
fn opened_location(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL-terminated, into
    // the buffer, and the descriptor stays open for the whole call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let len = buffer
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| io::Error::other("unterminated F_GETPATH result"))?;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..len])))
}

/// Other Unix systems have no handle-to-path query wired up here, so the
/// read fails closed rather than trusting the path walk alone.
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn opened_location(_file: &File) -> io::Result<PathBuf> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Where an open file really is, with every junction and symlink resolved.
#[cfg(windows)]
fn opened_location(file: &File) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };
    let mut buffer = vec![0u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).map_err(io::Error::other)?;
        // SAFETY: the buffer holds `capacity` UTF-16 units and the handle is
        // open for the whole call.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if len < buffer.len() {
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])));
        }
        // Too small: `len` is the size needed, including the terminator.
        buffer.resize(len, 0);
    }
}

#[cfg(unix)]
fn path_identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn handle_identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn path_identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    };
    // Windows exposes file IDs through a metadata-only handle, not MetadataExt.
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    handle_identity(&file)
}

#[cfg(windows)]
fn handle_identity(file: &File) -> io::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// Stable module-operation refusals; messages deliberately omit paths and policy contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadExactRefusal {
    BadRequest,
    AccessDenied,
    NotFound,
    NotRegular,
    TooLarge,
    RouteNotAdmitted(&'static str),
    IoError,
}

impl ReadExactRefusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::BadRequest => "file_read_exact_bad_request",
            Self::AccessDenied => "file_read_exact_access_denied",
            Self::NotFound => "file_read_exact_not_found",
            Self::NotRegular => "file_read_exact_not_regular",
            Self::TooLarge => "file_read_exact_too_large",
            Self::RouteNotAdmitted(_) => "file_read_exact_route_not_admitted",
            Self::IoError => "file_read_exact_io_error",
        }
    }

    pub(crate) fn message(self) -> String {
        match self {
            Self::BadRequest => "Missing or invalid path.".into(),
            Self::AccessDenied => "Access denied by the parent head's policy.".into(),
            Self::NotFound => "File not found.".into(),
            Self::NotRegular => "Not a regular file.".into(),
            Self::TooLarge => "File exceeds the 1048576-byte limit.".into(),
            Self::RouteNotAdmitted(check) => format!("Route not admitted: {check}."),
            Self::IoError => "The file could not be read.".into(),
        }
    }

    pub(crate) fn error(self) -> subc_protocol::ErrorBody {
        subc_protocol::ErrorBody::new(self.code(), self.message())
    }

    #[cfg(test)]
    fn response(self) -> Value {
        json!({"error": self.error()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_exact(
        path: &Path,
        root: &Path,
        policy: &AccessPolicy,
    ) -> Result<Value, ReadExactRefusal> {
        super::read_exact(path, root, policy, ReadHooks::default()).map(|read| read.value)
    }

    fn read_with(
        path: &Path,
        root: &Path,
        policy: &AccessPolicy,
        open: impl FnOnce(&Path, &Path) -> Result<File, ReadExactRefusal>,
    ) -> Result<Value, ReadExactRefusal> {
        super::read_with(path, root, policy, open, ReadHooks::default()).map(|read| read.value)
    }

    fn fixture() -> (tempfile::TempDir, AccessPolicy) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let policy = AccessPolicy::from_config(
            &crate::config::Config::default(),
            &root.join("store"),
            vec![root],
            Path::new("/nonexistent-home"),
        );
        (dir, policy)
    }

    #[test]
    fn file_read_exact_allowed_bytes_and_hash() {
        let (dir, policy) = fixture();
        std::fs::write(dir.path().join("script"), b"abc").unwrap();
        assert_eq!(
            read_exact(Path::new("script"), dir.path(), &policy).unwrap(),
            json!({
                "bytes_base64": "YWJj", "size": 3,
                "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            })
        );
    }

    #[test]
    fn file_read_exact_empty_file() {
        let (dir, policy) = fixture();
        std::fs::write(dir.path().join("empty"), []).unwrap();
        assert_eq!(
            read_exact(Path::new("empty"), dir.path(), &policy).unwrap(),
            json!({
                "bytes_base64": "", "size": 0,
                "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            })
        );
    }

    #[test]
    fn file_read_exact_denied_policy() {
        let (dir, mut policy) = fixture();
        let file = dir.path().join("secret");
        std::fs::write(&file, "secret").unwrap();
        policy.absolute_floor.push(file.canonicalize().unwrap());
        assert_eq!(
            read_exact(&file, dir.path(), &policy),
            Err(ReadExactRefusal::AccessDenied)
        );
    }

    #[test]
    fn file_read_exact_non_regular() {
        let (dir, policy) = fixture();
        assert_eq!(
            read_exact(dir.path(), dir.path(), &policy),
            Err(ReadExactRefusal::NotRegular)
        );
    }

    #[test]
    fn file_read_exact_missing_and_io_error() {
        let (dir, policy) = fixture();
        assert_eq!(
            read_exact(Path::new("absent"), dir.path(), &policy),
            Err(ReadExactRefusal::NotFound)
        );
        std::fs::write(dir.path().join("script"), "abc").unwrap();
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other] {
            assert_eq!(
                read_with(Path::new("script"), dir.path(), &policy, |_, _| Err(
                    open_error(io::Error::from(kind))
                )),
                Err(ReadExactRefusal::IoError)
            );
        }
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::Other.into())
            }
        }
        assert_eq!(bounded_bytes(Broken), Err(ReadExactRefusal::IoError));
    }

    #[test]
    fn file_read_exact_sqlite_never_opened() {
        let (dir, policy) = fixture();
        for name in ["live.db", "live.db-wal", "live.db-shm"] {
            std::fs::write(dir.path().join(name), "sqlite").unwrap();
            assert_eq!(
                read_with(Path::new(name), dir.path(), &policy, |_, _| panic!(
                    "SQLite contents must not be opened"
                )),
                Err(ReadExactRefusal::AccessDenied)
            );
        }
    }

    #[test]
    fn file_read_exact_parent_escape_never_opened() {
        let (dir, mut policy) = fixture();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("secret"), "secret").unwrap();
        policy.roots = vec![root.canonicalize().unwrap()];
        policy.private_stores.read_allow = policy.roots.clone();
        policy.mode = crate::access_policy::ApprovalMode::FullAccess;
        assert_eq!(
            read_with(Path::new("../secret"), &root, &policy, |_, _| panic!(
                "escape must not open contents"
            )),
            Err(ReadExactRefusal::AccessDenied)
        );
    }

    #[test]
    fn file_read_exact_absolute_escape_full_access() {
        let (dir, mut policy) = fixture();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "secret").unwrap();
        policy.mode = crate::access_policy::ApprovalMode::FullAccess;
        assert_eq!(
            read_with(&outside, &root, &policy, |_, _| panic!(
                "outside contents must not be opened"
            )),
            Err(ReadExactRefusal::AccessDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_symlink_escape_never_opened() {
        let (dir, mut policy) = fixture();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "secret").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link")).unwrap();
        policy.roots = vec![root.canonicalize().unwrap()];
        policy.private_stores.read_allow = policy.roots.clone();
        policy.mode = crate::access_policy::ApprovalMode::FullAccess;
        assert_eq!(
            read_with(Path::new("link"), &root, &policy, |_, _| panic!(
                "symlink escape must not open contents"
            )),
            Err(ReadExactRefusal::AccessDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_fifo_device_and_directory_link() {
        use std::os::unix::ffi::OsStrExt;
        let (dir, mut policy) = fixture();
        let fifo = dir.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(
            read_exact(&fifo, dir.path(), &policy),
            Err(ReadExactRefusal::NotRegular)
        );
        std::os::unix::fs::symlink(dir.path(), dir.path().join("link")).unwrap();
        assert_eq!(
            read_exact(Path::new("link"), dir.path(), &policy),
            Err(ReadExactRefusal::NotRegular)
        );
        policy.roots.push(Path::new("/dev").into());
        assert_eq!(
            read_exact(Path::new("/dev/null"), Path::new("/dev"), &policy),
            Err(ReadExactRefusal::NotRegular)
        );
    }

    #[test]
    fn file_read_exact_cap_stops_iterator() {
        use std::cell::Cell;
        let (dir, _) = fixture();
        let path = dir.path().join("large");
        let file = File::create(&path).unwrap();
        file.set_len(4 * MAX_BYTES).unwrap();
        drop(file);
        struct Count<'a> {
            file: File,
            bytes: &'a Cell<u64>,
        }
        impl Read for Count<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let n = self.file.read(bytes)?;
                self.bytes.set(self.bytes.get() + n as u64);
                Ok(n)
            }
        }
        let count = Cell::new(0);
        assert_eq!(
            bounded_bytes(Count {
                file: File::open(&path).unwrap(),
                bytes: &count
            }),
            Err(ReadExactRefusal::TooLarge)
        );
        assert_eq!(count.get(), MAX_BYTES + 1);
    }

    #[test]
    fn file_read_exact_size_cap_and_boundary() {
        let (dir, policy) = fixture();
        let path = dir.path().join("large");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_BYTES + 1).unwrap();
        drop(file);
        assert_eq!(
            read_exact(&path, dir.path(), &policy),
            Err(ReadExactRefusal::TooLarge)
        );
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES)
            .unwrap();
        assert_eq!(
            read_exact(&path, dir.path(), &policy).unwrap()["size"],
            MAX_BYTES
        );
    }

    #[test]
    fn file_read_exact_growing_and_shrinking_handle() {
        use std::io::{Seek, SeekFrom, Write};
        let (dir, _) = fixture();
        let path = dir.path().join("changing");
        struct Changing {
            file: File,
            writer: File,
            grow: bool,
            changed: bool,
        }
        impl Read for Changing {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let limit = bytes.len().min(2);
                let n = self.file.read(&mut bytes[..limit])?;
                if !self.changed {
                    self.changed = true;
                    if self.grow {
                        self.writer.set_len(MAX_BYTES + 100)?;
                    } else {
                        self.writer.set_len(2)?;
                    }
                }
                Ok(n)
            }
        }
        for grow in [true, false] {
            std::fs::write(&path, b"abcdef").unwrap();
            let mut writer = File::options().write(true).open(&path).unwrap();
            writer.seek(SeekFrom::Start(0)).unwrap();
            writer.write_all(b"abcdef").unwrap();
            let result = bounded_bytes(Changing {
                file: File::open(&path).unwrap(),
                writer,
                grow,
                changed: false,
            });
            if grow {
                assert_eq!(result, Err(ReadExactRefusal::TooLarge));
            } else {
                assert_eq!(
                    encode_bytes(&result.unwrap()),
                    json!({"bytes_base64":"YWI=", "size":2,"sha256":"fb8e20fc2e4c3f248c60c39bd652f3c1347298bb977b8b4d5903b85055620603"})
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_one_handle() {
        let (dir, policy) = fixture();
        let path = dir.path().join("script");
        std::fs::write(&path, "abc").unwrap();
        let count = std::cell::Cell::new(0);
        let value = read_with(&path, dir.path(), &policy, |root, resolved| {
            count.set(count.get() + 1);
            let file = open_contents(root, resolved)?;
            std::fs::rename(resolved, resolved.with_extension("old")).unwrap();
            std::fs::write(resolved, "replacement").unwrap();
            Ok(file)
        })
        .unwrap();
        assert_eq!(count.get(), 1);
        assert_eq!(value["bytes_base64"], "YWJj");
        assert_eq!(
            value["sha256"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_read_exact_replacement_identity() {
        let (dir, policy) = fixture();
        let path = dir.path().join("script");
        std::fs::write(&path, "abc").unwrap();
        let result = read_with(&path, dir.path(), &policy, |root, resolved| {
            std::fs::rename(resolved, resolved.with_extension("old")).unwrap();
            std::fs::write(resolved, "replacement").unwrap();
            open_contents(root, resolved)
        });
        assert_eq!(result, Err(ReadExactRefusal::IoError));
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_nofollow_replacement() {
        let (dir, policy) = fixture();
        let path = dir.path().join("script");
        std::fs::write(&path, "abc").unwrap();
        let result = read_with(&path, dir.path(), &policy, |root, resolved| {
            std::fs::rename(resolved, resolved.with_extension("old")).unwrap();
            std::os::unix::fs::symlink(resolved.with_extension("old"), resolved).unwrap();
            open_contents(root, resolved)
        });
        assert_eq!(result, Err(ReadExactRefusal::IoError));
    }

    /// A checkout `root/sub/file` with an `outside/file` twin beside it, and a
    /// full-access policy, so only the checkout fence can refuse the twin.
    #[cfg(unix)]
    fn swap_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, AccessPolicy) {
        let (dir, mut policy) = fixture();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("sub/file"), "inside").unwrap();
        std::fs::write(outside.join("file"), "outside").unwrap();
        policy.mode = crate::access_policy::ApprovalMode::FullAccess;
        (dir, root, outside, policy)
    }

    /// Replaces `root/sub` with a symlink to `target` once the path and policy
    /// checks have passed, as a concurrent writer in the checkout could.
    #[cfg(unix)]
    fn swap_after_check(root: &Path, target: &Path) -> ReadHooks {
        let (root, target) = (root.to_path_buf(), target.to_path_buf());
        ReadHooks {
            after_check: Some(std::sync::Arc::new(move || {
                std::fs::rename(root.join("sub"), root.join("sub.old")).unwrap();
                std::os::unix::fs::symlink(&target, root.join("sub")).unwrap();
            })),
            ..ReadHooks::default()
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_directory_swapped_for_symlink_refused() {
        let (_dir, root, outside, policy) = swap_fixture();
        let result = super::read_exact(
            Path::new("sub/file"),
            &root,
            &policy,
            swap_after_check(&root, &outside),
        );
        assert_eq!(
            result.map(|read| read.value).err(),
            Some(ReadExactRefusal::AccessDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_walk_refuses_symlinked_parent() {
        let (_dir, root, outside, _) = swap_fixture();
        std::fs::rename(root.join("sub"), root.join("sub.old")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        assert_eq!(
            open_contents(&root, &root.join("sub/file")).err(),
            Some(ReadExactRefusal::AccessDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_opened_location_rechecked() {
        // A path open that follows a swapped parent stands in for any open
        // that could be fooled; the handle's real location must still refuse.
        let follow = |_: &Path, resolved: &Path| File::open(resolved).map_err(open_error);
        let (_dir, root, outside, policy) = swap_fixture();
        let outside_read = super::read_with(
            Path::new("sub/file"),
            &root,
            &policy,
            follow,
            swap_after_check(&root, &outside),
        );
        assert_eq!(
            outside_read.map(|read| read.value).err(),
            Some(ReadExactRefusal::AccessDenied)
        );

        // Inside the checkout but denied by the head's policy.
        let (_dir, root, _, mut policy) = swap_fixture();
        let private = root.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("file"), "private").unwrap();
        policy.absolute_floor.push(private.join("file"));
        let denied_read = super::read_with(
            Path::new("sub/file"),
            &root,
            &policy,
            follow,
            swap_after_check(&root, &private),
        );
        assert_eq!(
            denied_read.map(|read| read.value).err(),
            Some(ReadExactRefusal::AccessDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_symlinked_parent_outside_never_opened() {
        let (_dir, root, outside, policy) = swap_fixture();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert_eq!(
            read_with(Path::new("link/file"), &root, &policy, |_, _| panic!(
                "a parent symlink out of the checkout must not open contents"
            )),
            Err(ReadExactRefusal::AccessDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_read_exact_nested_and_in_tree_parent_link() {
        let (_dir, root, _, policy) = swap_fixture();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/file"), "abc").unwrap();
        // An in-tree parent symlink is resolved before the checks, then the
        // walk opens the resolved path, which has no symlink in it.
        std::os::unix::fs::symlink(root.join("a"), root.join("alias")).unwrap();
        for path in ["a/b/file", "alias/b/file"] {
            let read =
                super::read_exact(Path::new(path), &root, &policy, ReadHooks::default()).unwrap();
            assert_eq!(read.value["bytes_base64"], "YWJj");
            assert_eq!(read.resolved, root.join("a/b/file"));
        }
    }

    #[test]
    fn file_read_exact_refusal_contract() {
        for (refusal, code, message) in [
            (
                ReadExactRefusal::BadRequest,
                "file_read_exact_bad_request",
                "Missing or invalid path.",
            ),
            (
                ReadExactRefusal::AccessDenied,
                "file_read_exact_access_denied",
                "Access denied by the parent head's policy.",
            ),
            (
                ReadExactRefusal::NotFound,
                "file_read_exact_not_found",
                "File not found.",
            ),
            (
                ReadExactRefusal::NotRegular,
                "file_read_exact_not_regular",
                "Not a regular file.",
            ),
            (
                ReadExactRefusal::TooLarge,
                "file_read_exact_too_large",
                "File exceeds the 1048576-byte limit.",
            ),
            (
                ReadExactRefusal::IoError,
                "file_read_exact_io_error",
                "The file could not be read.",
            ),
        ] {
            assert_eq!(
                refusal.response(),
                json!({"error": {"code": code, "message": message}})
            );
        }
        for (check, message) in [
            ("principal", "Route not admitted: principal."),
            ("owner", "Route not admitted: owner."),
            ("kind", "Route not admitted: kind."),
            ("scope_ref", "Route not admitted: scope_ref."),
            ("parent", "Route not admitted: parent."),
            ("expiry", "Route not admitted: expiry."),
            ("operation", "Route not admitted: operation."),
        ] {
            assert_eq!(
                ReadExactRefusal::RouteNotAdmitted(check).response(),
                json!({"error": {
                    "code": "file_read_exact_route_not_admitted",
                    "message": message
                }})
            );
        }
    }
}
