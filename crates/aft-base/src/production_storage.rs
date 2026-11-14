//! Debug builds may inspect the account's store, but must not change it.
//! Unix protects the effective account's passwd home, independent of HOME/XDG.
//! Windows always protects the process token's profile-derived LocalAppData,
//! independent of HOME/USERPROFILE/LOCALAPPDATA. A successful shell known-folder
//! lookup adds another root; that lookup can reflect redirected or expanded
//! environment paths. If neither OS lookup supplies a root, writes fail closed.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

pub(crate) const CODE: &str = "dev_build_refused_production_migration";

// Resolve an existing ancestor as well as the leaf, which may not exist yet.
pub fn canonicalized_with(
    path: &Path,
    canonicalize: &impl Fn(&Path) -> Option<PathBuf>,
) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Some(canonical) = canonicalize(ancestor) {
            if let Ok(tail) = path.strip_prefix(ancestor) {
                return canonical.join(tail);
            }
        }
    }
    path.to_path_buf()
}

pub(crate) fn comparison_key(path: &Path, windows: bool) -> Vec<OsString> {
    if windows {
        crate::windows_path::normalize_windows_path(path)
            .to_string_lossy()
            .trim_end_matches('\\')
            .split('\\')
            .map(|part| OsString::from(part.to_lowercase()))
            .collect()
    } else {
        path.components()
            .map(|part| part.as_os_str().into())
            .collect()
    }
}

pub(crate) fn is_within(path: &Path, parent: &Path) -> bool {
    fn absolute_normalized(path: &Path) -> io::Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("storage fence current-directory lookup failed: {error}"),
                    )
                })?
                .join(path)
        };
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => (),
                Component::ParentDir => {
                    normalized.pop();
                }
                _ => normalized.push(component),
            }
        }
        Ok(normalized)
    }
    let canonicalize = |path: &Path| path.canonicalize().ok();
    let Ok(path) = absolute_normalized(&canonicalized_with(path, &canonicalize)) else {
        return true;
    };
    let Ok(parent) = absolute_normalized(&canonicalized_with(parent, &canonicalize)) else {
        return true;
    };
    comparison_key(&path, cfg!(windows)).starts_with(&comparison_key(&parent, cfg!(windows)))
}

#[cfg(unix)]
pub fn account_home() -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0u8; 16384];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: entry and buffer are writable for the supplied sizes.
        let error = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE {
            if buffer.len() >= 1024 * 1024 {
                return Err(io::Error::other(
                    "getpwuid_r storage fence lookup exceeded its buffer limit",
                ));
            }
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if error != 0 || result.is_null() {
            return Err(io::Error::other(format!(
                "getpwuid_r storage fence lookup failed (status {error}, no entry: {})",
                result.is_null()
            )));
        }
        // SAFETY: getpwuid_r initialized result on success.
        if unsafe { (*result).pw_dir.is_null() } {
            return Err(io::Error::other(
                "getpwuid_r storage fence lookup returned no home",
            ));
        }
        // SAFETY: a successful lookup supplies a NUL-terminated pw_dir in buffer.
        let home = unsafe { std::ffi::CStr::from_ptr((*result).pw_dir) };
        return valid_account_path(
            PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())),
            "getpwuid_r",
        );
    }
}

#[cfg(windows)]
pub fn account_home() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: this handle belongs to the successful OpenProcessToken call.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let mut token = std::ptr::null_mut();
    // SAFETY: the current process pseudo-handle is valid and token is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::other(format!(
            "OpenProcessToken(TOKEN_QUERY) storage fence lookup failed: {}",
            io::Error::last_os_error()
        )));
    }
    let token = Token(token);
    let mut size = 0;
    // SAFETY: a null buffer queries the required UTF-16 buffer size.
    unsafe {
        GetUserProfileDirectoryW(token.0, std::ptr::null_mut(), &mut size);
    }
    if size == 0 || size > 32768 {
        return Err(io::Error::other(format!(
            "GetUserProfileDirectoryW storage fence size lookup failed (size {size}): {}",
            io::Error::last_os_error()
        )));
    }
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: the buffer has the size supplied by the profile API.
    if unsafe { GetUserProfileDirectoryW(token.0, buffer.as_mut_ptr(), &mut size) } == 0 {
        return Err(io::Error::other(format!(
            "GetUserProfileDirectoryW storage fence lookup failed: {}",
            io::Error::last_os_error()
        )));
    }
    let len = buffer.iter().position(|unit| *unit == 0).ok_or_else(|| {
        io::Error::other("GetUserProfileDirectoryW returned an unterminated path")
    })?;
    valid_account_path(
        PathBuf::from(OsString::from_wide(&buffer[..len])),
        "GetUserProfileDirectoryW",
    )
}

#[cfg(windows)]
pub(crate) fn known_local_app_data() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY,
    };
    struct FolderPath(*mut u16);
    impl Drop for FolderPath {
        fn drop(&mut self) {
            // SAFETY: shell paths use the COM allocator; freeing null is allowed.
            unsafe {
                CoTaskMemFree(self.0.cast());
            }
        }
    }
    let mut raw = std::ptr::null_mut();
    // SAFETY: the GUID is valid and raw is writable. Do not require existence:
    // a harness may redirect the shell's expanded profile to a missing folder.
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = FolderPath(raw);
    if status < 0 || path.0.is_null() {
        return Err(io::Error::other(format!("SHGetKnownFolderPath(FOLDERID_LocalAppData, KF_FLAG_DONT_VERIFY) storage fence lookup failed (HRESULT 0x{:08x})", status as u32)));
    }
    let mut len = 0;
    // SAFETY: a successful shell lookup returns an allocated NUL-terminated path.
    while unsafe { *path.0.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the scan above found the end of the allocated path.
    let units = unsafe { std::slice::from_raw_parts(path.0, len) };
    valid_account_path(
        PathBuf::from(OsString::from_wide(units)),
        "SHGetKnownFolderPath",
    )
}

fn valid_account_path(path: PathBuf, lookup: &str) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::other(format!(
            "{lookup} storage fence lookup returned an empty or relative path"
        )));
    }
    Ok(path)
}

pub(crate) fn account_storage_roots() -> io::Result<Vec<PathBuf>> {
    #[cfg(windows)]
    {
        combine_windows_roots(account_home(), known_local_app_data())
    }
    #[cfg(unix)]
    {
        Ok(vec![account_home()?.join(".local/share/cortexkit/aft")])
    }
}

#[cfg(any(windows, any(test, feature = "test-support")))]
fn combine_windows_roots(
    profile: io::Result<PathBuf>,
    local_data: io::Result<PathBuf>,
) -> io::Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    let mut errors = Vec::new();
    match profile {
        Ok(profile) => roots.push(profile.join("AppData/Local/cortexkit/aft")),
        Err(error) => errors.push(error.to_string()),
    }
    match local_data {
        Ok(data) => roots.push(data.join("cortexkit/aft")),
        Err(error) => errors.push(error.to_string()),
    }
    if roots.is_empty() {
        return Err(io::Error::other(format!(
            "no protected account storage root resolved: {}",
            errors.join("; ")
        )));
    }
    Ok(roots)
}

fn migration_opt_in() -> bool {
    let lookup = |name: &str| {
        #[cfg(any(test, feature = "test-support"))]
        if let Some((_, allow)) = TEST_ACCOUNT.with(|slot| slot.borrow().clone()) {
            // Supply an environment lookup, not a policy override: the opt-in
            // test must exercise the same variable name and exact-value check.
            return (allow && name == "AFT_ALLOW_PRODUCTION_MIGRATION")
                .then(|| OsString::from("1"));
        }
        std::env::var_os(name)
    };
    lookup("AFT_ALLOW_PRODUCTION_MIGRATION").as_deref() == Some(std::ffi::OsStr::new("1"))
}

pub fn protected(path: &Path) -> bool {
    protection_status_with(path, account_storage_roots).unwrap_or(true)
}

/// The account's own CortexKit config directories, derived from the OS
/// account record like [`account_storage_roots`] and never from HOME, XDG or
/// USERPROFILE. The user config file AFT reads by default lives under one of
/// them (`~/.config/cortexkit/aft.jsonc`; on Windows the profile's
/// `.config` or roaming `AppData` folder).
pub(crate) fn account_config_roots() -> io::Result<Vec<PathBuf>> {
    let home = account_home()?;
    #[cfg(windows)]
    {
        Ok(vec![
            home.join(".config/cortexkit"),
            home.join("AppData/Roaming/cortexkit"),
        ])
    }
    #[cfg(not(windows))]
    {
        Ok(vec![home.join(".config/cortexkit")])
    }
}

/// Why this build must not rewrite the config file at `path`, or `None` when
/// it may. A debug build (a test run, or the `target/debug/aft` a test spawns)
/// that inherited the operator's real HOME would otherwise rewrite the
/// operator's own config file when it migrates retired keys. Same policy as
/// the storage fence: release builds are never refused,
/// `AFT_ALLOW_PRODUCTION_MIGRATION=1` opts in, and a failed account lookup
/// refuses.
pub(crate) fn config_write_refusal(path: &Path) -> Option<String> {
    match protection_status_with(path, account_config_roots) {
        Ok(false) => None,
        Ok(true) => Some(format!(
            "{CODE}: a dev/test build does not rewrite the account's own config file; set AFT_ALLOW_PRODUCTION_MIGRATION=1 to allow it"
        )),
        Err(error) => Some(format!(
            "{CODE}: a dev/test build could not resolve the account's config directory ({error}), so it does not rewrite config files; set AFT_ALLOW_PRODUCTION_MIGRATION=1 to allow it"
        )),
    }
}

fn protection_status_with(
    path: &Path,
    roots: impl FnOnce() -> io::Result<Vec<PathBuf>>,
) -> io::Result<bool> {
    // Tests and development rigs normally run debug builds (including the
    // target/debug/aft children of Bun tests). Release builds must be allowed
    // automatically: packaging cannot depend on remembering extra opt-ins.
    if !cfg!(debug_assertions) {
        return Ok(false);
    }
    if migration_opt_in() {
        return Ok(false);
    }
    #[cfg(any(test, feature = "test-support"))]
    if let Some((root, _)) = TEST_ACCOUNT.with(|slot| slot.borrow().clone()) {
        return Ok(is_within(path, &root));
    }
    Ok(roots()?.iter().any(|root| is_within(path, root)))
}

pub fn refuse_write(path: &Path) -> io::Result<()> {
    refuse_write_for_status(path, protection_status_with(path, account_storage_roots))
}

fn refuse_write_for_status(path: &Path, status: io::Result<bool>) -> io::Result<()> {
    if !matches!(status, Ok(false)) {
        let reason = status
            .err()
            .map(|error| format!("; unresolved protected-root lookup: {error}"))
            .unwrap_or_default();
        Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
            "{CODE}: dev/test build refused to migrate production storage or write a versioned record at {}{reason}; use a disposable storage root or explicitly set AFT_ALLOW_PRODUCTION_MIGRATION=1", path.display()
        )))
    } else {
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_ACCOUNT: std::cell::RefCell<Option<(PathBuf, bool)>> = const { std::cell::RefCell::new(None) };
}

/// Thread-local OS-directory seam, never available to a spawned binary.
#[cfg(any(test, feature = "test-support"))]
pub fn with_test_account<T>(root: &Path, allow: bool, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<(PathBuf, bool)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_ACCOUNT.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore =
        Restore(TEST_ACCOUNT.with(|slot| slot.replace(Some((root.to_path_buf(), allow)))));
    run()
}

pub fn resolve_account_home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    let home = crate::production_storage::account_home().ok()?;
    #[cfg(windows)]
    let home = resolve_windows_profile_dir().ok()?;

    let canonical = std::fs::canonicalize(home).ok()?;
    canonical.is_dir().then_some(canonical)
}

#[cfg(windows)]
fn resolve_windows_profile_dir() -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Profile, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY,
    };

    struct FolderPath(*mut u16);
    impl Drop for FolderPath {
        fn drop(&mut self) {
            // SAFETY: SHGetKnownFolderPath allocates its result with the COM allocator.
            unsafe { CoTaskMemFree(self.0.cast()) };
        }
    }

    let mut raw = std::ptr::null_mut();
    // SAFETY: the folder identifier is valid and raw is writable.
    let status = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_Profile,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    let path = FolderPath(raw);
    if status < 0 || path.0.is_null() {
        return Err(std::io::Error::other(format!(
            "SHGetKnownFolderPath(FOLDERID_Profile) lookup failed (HRESULT 0x{:08x})",
            status as u32
        )));
    }
    let mut len = 0;
    // SAFETY: a successful lookup returns an allocated NUL-terminated path.
    while unsafe { *path.0.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the scan above found the end of the allocated path.
    let units = unsafe { std::slice::from_raw_parts(path.0, len) };
    Ok(PathBuf::from(std::ffi::OsString::from_wide(units)))
}
