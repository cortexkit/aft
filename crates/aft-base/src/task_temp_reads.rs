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
        return Ok(None);
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

pub fn open(path: &Path) -> io::Result<File> {
    let Some(check) = check_path(path)? else {
        return crate::bounded_io::open_content_file(path);
    };
    // Sandbox task-temp directory names select stricter open checks so links
    // cannot redirect scratch reads elsewhere. A matching name never grants
    // access; the caller still needs authorization from the task registry.
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

fn refused() -> io::Error {
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
pub(crate) fn opened_location(file: &File) -> io::Result<PathBuf> {
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
pub(crate) fn opened_location(file: &File) -> io::Result<PathBuf> {
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
pub(crate) const MANAGED_TEMP_ROOT: &str = "/private/tmp/aft-w";
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
