//! Open scratch content without letting a replaced symlink widen a read grant.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct OpenCheck {
    root: PathBuf,
    expected: PathBuf,
}

pub fn is_scratch(path: &Path) -> bool {
    path.ancestors().any(is_managed_task_temp_dir)
}

pub(crate) fn check_path(path: &Path) -> io::Result<Option<OpenCheck>> {
    // Both open callers resolve relative paths against the process cwd. Make
    // that base explicit without following links, so the root search below
    // also sees task-temp directories that sit above the cwd.
    let absolute_path = std::path::absolute(path)?;
    // The trusted root must be the task-temp directory the task registry
    // records, never a directory the task itself created. Registry roots are
    // either /private/tmp/aft-w/<12 hex> (/tmp/aft-w/<12 hex> on Linux), which
    // only matches when its parent is exactly that fixed daemon-owned folder,
    // or a legacy .sandbox-tmp-* folder the daemon made inside the task's IO
    // bundle. A sandboxed task can only write inside its root, so any other
    // matching name it creates sits BELOW the registry root, while nothing
    // above the root is task-created. Taking the outermost match therefore
    // always lands on the registry root for registry-authorized paths.
    let Some(absolute_root) = absolute_path
        .ancestors()
        .filter(|path| is_managed_task_temp_dir(path))
        .last()
    else {
        return Ok(general_temp_check(&absolute_path));
    };
    if crate::project_content::is_sqlite_or_metadata(path) {
        return Err(refused());
    }
    let remainder = absolute_path
        .strip_prefix(absolute_root)
        .map_err(|_| refused())?;
    // Resolve aliases such as /var on macOS only ABOVE the root: canonicalize
    // the root's parent and append the root's own name unresolved. Following
    // the root or anything below it would trust a replacement symlink's
    // outside target; left unresolved, such a swap changes the opened
    // descriptor's location and the check after open refuses it.
    let (Some(parent), Some(name)) = (absolute_root.parent(), absolute_root.file_name()) else {
        return Err(refused());
    };
    let root = crate::path_identity::normalize_path(&std::fs::canonicalize(parent)?.join(name));
    let expected = crate::path_identity::normalize_path(&root.join(remainder));
    Ok(Some(OpenCheck { root, expected }))
}

thread_local! {
    static GENERAL_TEMP_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the previous general temp root when dropped.
pub struct GeneralTempScope(Option<PathBuf>);

impl Drop for GeneralTempScope {
    fn drop(&mut self) {
        let previous = self.0.take();
        GENERAL_TEMP_ROOT.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Mark content opens on this thread, and on the filesystem helpers it
/// starts, as reads the general temp allowance admitted under `root`. The
/// allowance checked the path's file type and link count before the read;
/// opens below `root` then get the same opened-handle checks as task scratch,
/// so a name replaced after that check cannot redirect the read. `root` and
/// the paths opened under it are fully resolved policy paths.
pub fn admit_general_temp(root: Option<PathBuf>) -> GeneralTempScope {
    GeneralTempScope(GENERAL_TEMP_ROOT.with(|slot| slot.replace(root)))
}

pub(crate) fn current_general_temp() -> Option<PathBuf> {
    GENERAL_TEMP_ROOT.with(|slot| slot.borrow().clone())
}

fn general_temp_check(absolute_path: &Path) -> Option<OpenCheck> {
    let root = crate::path_identity::normalize_path(&current_general_temp()?);
    let expected = crate::path_identity::normalize_path(absolute_path);
    expected
        .starts_with(&root)
        .then_some(OpenCheck { root, expected })
}

pub fn open(path: &Path) -> io::Result<File> {
    let Some(check) = check_path(path)? else {
        return crate::bounded_io::open_content_file(path);
    };
    // Sandbox task-temp directory names, and reads the general temp allowance
    // admitted, select stricter open checks so links cannot redirect the read
    // elsewhere. A matching name never grants access; the caller still needs
    // authorization from the task registry or the access policy.
    let file = crate::bounded_io::open_content_file_no_follow(path)?;
    validate_opened(path, &check, &file)?;
    Ok(file)
}

fn validate_opened(path: &Path, check: &OpenCheck, file: &File) -> io::Result<()> {
    let opened = crate::path_identity::normalize_path(&opened_location(file)?);
    // Judge the descriptor, not a second canonicalization of the input: a
    // parent directory may have become a symlink after permission was checked.
    if !opened.starts_with(&check.root) || opened != check.expected || !file.metadata()?.is_file() {
        return Err(refused());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata()?.nlink() != 1 {
            return Err(refused());
        }
    }
    // Windows has no O_NOFOLLOW: the checked open asked for a final reparse
    // point itself, so refuse one here, along with a second hard link.
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open for the whole call and `info` is writable.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || info.nNumberOfLinks != 1 {
            return Err(refused());
        }
    }
    if crate::project_content::is_sqlite_or_metadata(&opened)
        || crate::project_content::is_sqlite_or_metadata(path)
    {
        return Err(refused());
    }
    Ok(())
}

pub(crate) fn check_opened(path: &Path, check: Option<&OpenCheck>, file: &File) -> io::Result<()> {
    if let Some(check) = check {
        validate_opened(path, check, file)?;
    }
    Ok(())
}

pub(crate) fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "scratch read escaped its task directory or named unsafe content",
    )
}

pub fn validate_task_id(task_id: &str) -> std::io::Result<()> {
    let bytes = task_id.as_bytes();
    if bytes.len() == 21
        && bytes.starts_with(b"bash-")
        && bytes[5..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "background task id must match ^bash-[0-9a-f]{16}$",
        ))
    }
}

/// Recognize the task bundle structurally, without trusting a caller's session
/// namespace or granting access to the enclosing private store.
pub fn task_directory(path: &Path) -> Option<&Path> {
    // Ordinary project paths never need ancestor-by-ancestor task validation.
    // A marker match is only a fast prefilter; the component checks still win.
    static MARKER: std::sync::LazyLock<memchr::memmem::Finder<'static>> =
        std::sync::LazyLock::new(|| memchr::memmem::Finder::new(b"bash-tasks"));
    MARKER.find(path.as_os_str().as_encoded_bytes())?;
    path.ancestors().find(|ancestor| {
        ancestor
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            == Some(std::ffi::OsStr::new("bash-tasks"))
            && ancestor
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| validate_task_id(name).is_ok())
    })
}

pub fn is_task_environment(path: &Path) -> bool {
    task_directory(path).is_some_and(|root| path == root.join("control/environment.bin"))
}

#[cfg(test)]
mod task_path_tests {
    #[test]
    fn managed_temp_predicates_preserve_legacy_and_private_root_names() {
        use super::*;
        assert!(is_managed_task_temp_dir(Path::new(
            "/task/.sandbox-tmp-old"
        )));
        assert!(!is_managed_task_temp_dir(Path::new(
            "/task/sandbox-tmp-old"
        )));
        #[cfg(unix)]
        {
            assert!(is_managed_task_temp_dir(
                &Path::new(MANAGED_TEMP_ROOT).join("012345abcdef")
            ));
            for path in [
                Path::new(MANAGED_TEMP_ROOT).join("012345ABCDEF"),
                Path::new("/other").join("012345abcdef"),
            ] {
                assert!(!is_managed_task_temp_dir(&path), "{}", path.display());
            }
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        windows
    ))]
    #[test]
    fn opened_location_still_resolves_the_open_handle() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("content");
        std::fs::write(&path, "content").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let actual = super::opened_location(&file).unwrap();
        assert_eq!(
            crate::path_identity::normalize_path(&actual),
            crate::path_identity::normalize_path(&std::fs::canonicalize(path).unwrap())
        );
    }

    use super::*;
    #[test]
    fn task_id_validation_is_exact() {
        assert!(validate_task_id("bash-0123456789abcdef").is_ok());
        for invalid in [
            "bash-0123456789abcde",
            "bash-0123456789abcdef0",
            "bash-0123456789ABCDEf",
            "bash-0123456789abcdeg",
            "../bash-0123456789abcdef",
        ] {
            assert!(validate_task_id(invalid).is_err(), "accepted {invalid}");
        }
    }
}

pub fn is_managed_task_temp_dir(path: &Path) -> bool {
    #[cfg(unix)]
    if is_managed(path) {
        return true;
    }
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name.starts_with(".sandbox-tmp-"))
}

/// Where an open file really is, asked of the kernel through its handle.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn opened_location(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Where an open file really is, asked of the kernel through its handle.
#[cfg(target_os = "macos")]
pub fn opened_location(file: &File) -> io::Result<PathBuf> {
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
pub(crate) fn opened_location(_file: &File) -> io::Result<PathBuf> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Where an open file really is, with every junction and symlink resolved.
#[cfg(windows)]
pub fn opened_location(file: &File) -> io::Result<PathBuf> {
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

#[cfg(target_os = "macos")]
pub const MANAGED_TEMP_ROOT: &str = "/private/tmp/aft-w";
#[cfg(all(unix, not(target_os = "macos")))]
pub const MANAGED_TEMP_ROOT: &str = "/tmp/aft-w";

#[cfg(unix)]
pub fn task_name(name: &str) -> bool {
    name.len() == 12
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(unix)]
pub fn provider_root() -> std::path::PathBuf {
    Path::new(MANAGED_TEMP_ROOT).join("env-provider")
}

#[cfg(unix)]
pub fn is_managed(path: &Path) -> bool {
    let provider_parent = path.parent().is_some_and(|parent| {
        parent.parent() == Some(provider_root().as_path())
            && parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(task_name)
    });
    (path.parent() == Some(Path::new(MANAGED_TEMP_ROOT)) || provider_parent)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(task_name)
}

#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]
mod general_temp_tests {
    use super::*;

    /// A guarded temp root holding `data.txt`, and an outside folder holding
    /// `secret.txt` that a swapped name could point at.
    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(temp.path()).unwrap();
            let root = base.join("temp-root");
            let outside = base.join("outside");
            std::fs::create_dir_all(root.join("folder")).unwrap();
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(root.join("data.txt"), "stable").unwrap();
            std::fs::write(root.join("folder/data.txt"), "stable").unwrap();
            std::fs::write(outside.join("secret.txt"), "secret").unwrap();
            std::fs::write(outside.join("data.txt"), "secret").unwrap();
            Self {
                _temp: temp,
                root,
                outside,
            }
        }
    }

    fn read(path: &Path) -> io::Result<String> {
        std::io::read_to_string(open(path)?)
    }

    #[test]
    fn stable_general_temp_reads_still_work() {
        let f = Fixture::new();
        let _scope = admit_general_temp(Some(f.root.clone()));
        assert_eq!(read(&f.root.join("data.txt")).unwrap(), "stable");
        assert_eq!(read(&f.root.join("folder/data.txt")).unwrap(), "stable");
        assert_eq!(
            crate::bounded_io::read(&f.root.join("data.txt"), None).unwrap(),
            b"stable"
        );
    }

    #[test]
    fn general_temp_read_refuses_a_final_name_swapped_for_a_link() {
        let f = Fixture::new();
        let path = f.root.join("data.txt");
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(f.outside.join("secret.txt"), &path).unwrap();
        // Without the allowance's checks the open follows the swapped link.
        assert_eq!(read(&path).unwrap(), "secret");
        let _scope = admit_general_temp(Some(f.root.clone()));
        assert!(read(&path).is_err());
        assert!(crate::bounded_io::read(&path, None).is_err());
    }

    #[test]
    fn general_temp_read_refuses_a_parent_swapped_for_a_link() {
        let f = Fixture::new();
        let folder = f.root.join("folder");
        std::fs::remove_dir_all(&folder).unwrap();
        std::os::unix::fs::symlink(&f.outside, &folder).unwrap();
        let path = folder.join("data.txt");
        let _scope = admit_general_temp(Some(f.root.clone()));
        let error = read(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(crate::bounded_io::read(&path, None).is_err());
    }

    #[test]
    fn general_temp_read_refuses_a_second_hard_link() {
        let f = Fixture::new();
        let path = f.root.join("data.txt");
        std::fs::hard_link(&path, f.outside.join("alias.txt")).unwrap();
        let _scope = admit_general_temp(Some(f.root.clone()));
        assert_eq!(
            read(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn general_temp_checks_follow_the_read_onto_filesystem_helpers() {
        let f = Fixture::new();
        let folder = f.root.join("folder");
        std::fs::remove_dir_all(&folder).unwrap();
        std::os::unix::fs::symlink(&f.outside, &folder).unwrap();
        let path = folder.join("data.txt");
        let _scope = admit_general_temp(Some(f.root.clone()));
        let caller = std::thread::current().id();
        let (helper, result) = crate::bounded_io::run(&path, None, |path| {
            Ok((std::thread::current().id(), read(&path)))
        })
        .unwrap();
        assert_ne!(helper, caller, "the read must run on a helper thread");
        assert!(result.is_err());
        drop(_scope);
        // The helper thread does not keep the checks once the scope ends.
        assert_eq!(
            crate::bounded_io::run(&path, None, |path| Ok(read(&path)))
                .unwrap()
                .unwrap(),
            "secret"
        );
    }
}
