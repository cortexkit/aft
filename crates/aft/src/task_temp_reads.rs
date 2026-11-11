//! Open scratch content without letting a replaced symlink widen a read grant.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct OpenCheck {
    root: PathBuf,
    expected: PathBuf,
}

pub(crate) fn is_scratch(path: &Path) -> bool {
    path.ancestors()
        .any(crate::sandbox_spawn::is_managed_task_temp_dir)
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
        .filter(|path| crate::sandbox_spawn::is_managed_task_temp_dir(path))
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

pub(crate) fn open(path: &Path) -> io::Result<File> {
    let Some(check) = check_path(path)? else {
        return File::open(path);
    };
    // Sandbox task-temp directory names select stricter open checks so links
    // cannot redirect scratch reads elsewhere. A matching name never grants
    // access; the caller still needs authorization from the task registry.
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    validate_opened(path, &check, &file)?;
    Ok(file)
}

fn validate_opened(path: &Path, check: &OpenCheck, file: &File) -> io::Result<()> {
    let opened = crate::path_identity::normalize_path(
        &crate::commands::file_read_exact::opened_location(file)?,
    );
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
pub(crate) fn task_directory(path: &Path) -> Option<&Path> {
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

pub(crate) fn is_task_environment(path: &Path) -> bool {
    task_directory(path).is_some_and(|root| path == root.join("control/environment.bin"))
}

#[cfg(test)]
mod task_path_tests {
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
