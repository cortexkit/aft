use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    static ROOTS_FOR_TEST: std::cell::RefCell<Option<Vec<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the previous temp-root override when dropped.
#[cfg(test)]
pub(crate) struct RootsOverride(Option<Vec<PathBuf>>);

#[cfg(test)]
impl Drop for RootsOverride {
    fn drop(&mut self) {
        let previous = self.0.take();
        ROOTS_FOR_TEST.with(|roots| *roots.borrow_mut() = previous);
    }
}

/// Make this thread's access policies treat `roots` as the only system temp
/// directories, so a test can exercise the temp guards on a directory it owns
/// instead of the host's real `/tmp`.
#[cfg(test)]
pub(crate) fn override_roots_for_test(roots: Vec<PathBuf>) -> RootsOverride {
    RootsOverride(ROOTS_FOR_TEST.with(|current| current.borrow_mut().replace(roots)))
}

/// The temp-root override installed on the calling thread, if any. Read it on
/// the request thread: policy construction may run on the bounded IO helper.
pub(crate) fn roots_override() -> Option<Vec<PathBuf>> {
    #[cfg(test)]
    {
        ROOTS_FOR_TEST.with(|roots| roots.borrow().clone())
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// Every system temp directory the standalone temp guards cover.
pub(crate) fn guard_roots(overridden: Option<&[PathBuf]>) -> Vec<PathBuf> {
    match overridden {
        Some(roots) => roots.iter().map(|root| super::policy_path(root)).collect(),
        None => crate::bash_permissions::system_temp_roots()
            .iter()
            .map(|path| super::policy_path(path))
            .collect(),
    }
}

/// The temp directories a standalone call may use without an outside ask.
pub(crate) fn allowed_roots(overridden: Option<&[PathBuf]>) -> Vec<PathBuf> {
    match overridden {
        Some(roots) => roots.iter().map(|root| super::policy_path(root)).collect(),
        None => roots(),
    }
}

pub(crate) fn roots() -> Vec<PathBuf> {
    let own = if cfg!(target_os = "macos") {
        std::env::var_os("TMPDIR").map(PathBuf::from)
    } else {
        Some(std::env::temp_dir())
    };
    roots_for(own, cfg!(target_os = "macos"))
}

pub(crate) fn roots_for(own: Option<PathBuf>, macos: bool) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(own);
    #[cfg(unix)]
    if !macos {
        roots.extend(["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"].map(PathBuf::from));
    }
    #[cfg(not(unix))]
    let _ = macos;
    roots
        .into_iter()
        .filter(|root| root.is_absolute() && root.parent().is_some())
        .map(|root| super::policy_path(&root))
        .collect()
}

pub(crate) fn managed_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.push(std::env::temp_dir().join("aft-w"));
    #[cfg(unix)]
    roots.extend(
        [
            "/tmp/aft-w",
            "/private/tmp/aft-w",
            "/var/tmp/aft-w",
            "/private/var/tmp/aft-w",
        ]
        .map(PathBuf::from),
    );
    roots
        .into_iter()
        .map(|root| super::policy_path(&root))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempReason {
    SqliteFile,
    HardLink,
    OtherSessionScratch,
    Unreadable,
    SpecialFile,
    ProtectedRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempDenial {
    pub entry: PathBuf,
    pub reason: TempReason,
}

const SPECIAL_FILE_MESSAGE: &str =
    "AFT changes only regular files, folders and symbolic links under the temporary directory";
const PROTECTED_ROOT_MESSAGE: &str = "AFT does not replace or remove temporary directory roots";

impl TempDenial {
    pub(crate) fn new(entry: &Path, reason: TempReason) -> Self {
        Self {
            entry: entry.to_path_buf(),
            reason,
        }
    }

    pub(crate) fn message(&self, requested: &Path) -> String {
        let folder = requested.display();
        let entry = self.entry.display();
        match self.reason {
            TempReason::SqliteFile => format!("temporary path '{folder}' contains a SQLite file ('{entry}'); AFT does not change SQLite files under the temporary directory, because they can be another program's live database"),
            TempReason::HardLink => format!("temporary path '{folder}' contains a file with more than one hard link ('{entry}'); AFT does not change hard-linked files under the temporary directory, because another link can be outside it"),
            TempReason::OtherSessionScratch => format!("temporary path '{folder}' is another session's scratch folder; AFT changes only this session's scratch folders"),
            TempReason::Unreadable => format!("temporary path '{folder}' could not be fully checked ('{entry}' could not be read)"),
            TempReason::SpecialFile => format!("temporary path '{folder}' contains a special file ('{entry}'); {SPECIAL_FILE_MESSAGE}"),
            TempReason::ProtectedRoot => format!("temporary path '{folder}' is a temporary directory root; {PROTECTED_ROOT_MESSAGE}"),
        }
    }
}

/// Checks run before a temp allowance, never after opening data for modification.
pub(crate) fn safe_file(path: &Path) -> Result<(), TempDenial> {
    let sqlite_name = path.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .to_ascii_lowercase()
            .contains(".sqlite")
    });
    if sqlite_name || crate::project_content::is_sqlite_or_metadata(path) {
        return Err(TempDenial::new(path, TempReason::SqliteFile));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return if error.kind() == std::io::ErrorKind::NotFound
                || names_nothing(&error, cfg!(windows))
            {
                Ok(())
            } else {
                Err(TempDenial::new(path, TempReason::Unreadable))
            }
        }
    };
    if metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(TempDenial::new(path, TempReason::SpecialFile));
    }
    single_link(path, &metadata)
}

/// Whether `error` says the spelling cannot name any file at all. Win32 rejects
/// a literal glob (`src\*.ts`) or two paths joined by a space with
/// ERROR_INVALID_NAME (123) instead of "not found". Nothing exists at such a
/// spelling, so there is no hard link or database to refuse; the access probe
/// that follows still decides whether the spelling is usable, and a glob's
/// concrete matches are each validated on their own.
fn names_nothing(error: &std::io::Error, windows: bool) -> bool {
    windows && error.raw_os_error() == Some(123)
}

#[cfg(unix)]
fn single_link(path: &Path, metadata: &std::fs::Metadata) -> Result<(), TempDenial> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() == 1 {
        Ok(())
    } else {
        Err(TempDenial::new(path, TempReason::HardLink))
    }
}

#[cfg(windows)]
fn single_link(path: &Path, _metadata: &std::fs::Metadata) -> Result<(), TempDenial> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
    else {
        return Err(TempDenial::new(path, TempReason::Unreadable));
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        Err(TempDenial::new(path, TempReason::Unreadable))
    } else if info.nNumberOfLinks == 1 {
        Ok(())
    } else {
        Err(TempDenial::new(path, TempReason::HardLink))
    }
}

#[cfg(not(any(unix, windows)))]
fn single_link(path: &Path, _metadata: &std::fs::Metadata) -> Result<(), TempDenial> {
    Err(TempDenial::new(path, TempReason::Unreadable))
}

pub(crate) fn safe_tree(
    path: &Path,
    managed_roots: &[PathBuf],
    owned: bool,
) -> Result<(), TempDenial> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return safe_file(path);
    }
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        if !owned
            && (managed_roots.iter().any(|root| path.starts_with(root))
                || crate::task_temp_reads::is_scratch(&path))
        {
            return Err(TempDenial::new(&path, TempReason::OtherSessionScratch));
        }
        safe_file(&path)?;
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            let Ok(entries) = std::fs::read_dir(&path) else {
                return Err(TempDenial::new(&path, TempReason::Unreadable));
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return Err(TempDenial::new(&path, TempReason::Unreadable));
                };
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only Windows's "this cannot be a name" error (123) names nothing; the
    /// same number elsewhere and other errors still fail closed.
    #[test]
    fn temp_guard_invalid_name_error_names_nothing_only_on_windows() {
        let invalid = std::io::Error::from_raw_os_error(123);
        assert!(names_nothing(&invalid, true));
        assert!(!names_nothing(&invalid, false));
        assert!(!names_nothing(&std::io::Error::from_raw_os_error(5), true));
    }

    /// An unmatched glob or two paths joined by a space under a temp directory
    /// resolves to no file, so the temp guard has no metadata to refuse. A
    /// hard-linked file in the same directory is still refused.
    #[test]
    fn temp_guard_accepts_unstattable_spellings_but_not_hard_links() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        assert!(safe_file(&src.join("*.ts")).is_ok());
        let compound = PathBuf::from(format!("{} {}", src.display(), temp.path().display()));
        assert!(safe_file(&compound).is_ok());
        let file = src.join("a.ts");
        std::fs::write(&file, "export {};\n").unwrap();
        std::fs::hard_link(&file, src.join("b.ts")).unwrap();
        assert_eq!(
            safe_file(&file),
            Err(TempDenial::new(&file, TempReason::HardLink))
        );
    }
}
