//! A kernel-enforced marker that tells the gh shim it runs inside AFT's own
//! worker jail.
//!
//! The shim must refuse to fall back to the operator's gh for a jailed worker,
//! even when the worker clears its environment (`env -i`) or unsets its ticket.
//! Generic "am I sandboxed" signals cannot do this: Linux `no_new_privs` and a
//! macOS Seatbelt policy are also set by containers, CI runners, systemd units,
//! bubblewrap or another tool's sandbox. A standalone user there must keep gh
//! passthrough.
//!
//! The marker is a canary file at a fixed path under the account's default AFT
//! storage. Only the worker-jail profile denies reading it. The path comes from
//! the password database (and `$HOME`), never from a variable a worker could
//! point somewhere readable. It is not secret: a same-uid shim can find it, and
//! so can the worker. Security rests on the kernel deny alone. The jail denies
//! reads and writes of the canary directory, so the worker can neither make the
//! canary readable nor delete, rename or replace it.
//!
//! Head shells (the operator's own agent) never deny the canary; their profiles
//! grant reading it, so a sandboxed head keeps v19 passthrough.
//!
//! The fence applies only to strict Linux/macOS and relaxed macOS workers.
//! Off workers, relaxed Linux and Windows have no proven canary fence; their
//! ticket/environment checks alone do not survive a worker clearing its env.
//! A canary gates this shim, not a directly executed upstream gh or credential
//! services such as the macOS keychain. Those need independent confinement.

use std::path::{Path, PathBuf};

/// Directory, relative to an account home, that holds only the canary. It sits
/// in AFT's default storage root, `~/.local/share/cortexkit/aft`, even when the
/// installation is configured to use a different storage root.
pub const CANARY_DIR_FROM_HOME: &str = ".local/share/cortexkit/aft/gh-shim-worker-jail";
/// File name of the canary inside [`CANARY_DIR_FROM_HOME`].
pub const CANARY_FILE: &str = "canary";
#[cfg(unix)]
const CANARY_CONTENTS: &[u8] =
    b"AFT worker-jail canary. Worker shells cannot read this file; the gh shim uses that to recognize them.\n";

/// Public only so integration tests use the same account-home lookup as the sandbox.
#[doc(hidden)]
#[cfg(unix)]
pub fn account_home() -> std::io::Result<PathBuf> {
    crate::production_storage::account_home()
}

/// The canary directory for one account home.
pub fn dir_for_home(home: &Path) -> PathBuf {
    home.join(CANARY_DIR_FROM_HOME)
}

/// The canary file for one account home.
pub fn path_for_home(home: &Path) -> PathBuf {
    dir_for_home(home).join(CANARY_FILE)
}

/// Create the canary, or repair one whose contents or permissions changed, and
/// return its canonical directory. Runs outside any jail, before a profile is
/// built. Any failure means the jail cannot be fenced.
#[cfg(unix)]
pub fn prepare(home: &Path) -> Result<PathBuf, String> {
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let dir = dir_for_home(home);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| {
            format!(
                "cannot create worker-jail canary {}: {error}",
                dir.display()
            )
        })?;
    let metadata = fs::symlink_metadata(&dir).map_err(|error| {
        format!(
            "cannot inspect worker-jail canary {}: {error}",
            dir.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "worker-jail canary directory is not a real directory: {}",
            dir.display()
        ));
    }
    // A worker on a backend without chmod mediation (Landlock) may have
    // changed these modes in an earlier run; put them back each launch.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot restore canary directory mode: {error}"))?;
    let file = dir.join(CANARY_FILE);
    let intact = fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file())
        && fs::read(&file).is_ok_and(|bytes| bytes == CANARY_CONTENTS);
    if !intact {
        let staging = dir.join(format!(".{CANARY_FILE}.{}", std::process::id()));
        let _ = fs::remove_file(&staging);
        fs::write(&staging, CANARY_CONTENTS)
            .and_then(|()| fs::rename(&staging, &file))
            .map_err(|error| {
                format!(
                    "cannot write worker-jail canary {}: {error}",
                    file.display()
                )
            })?;
    }
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot restore canary mode: {error}"))?;
    if fs::read(&file).ok().as_deref() != Some(CANARY_CONTENTS) {
        return Err(format!(
            "worker-jail canary did not verify after writing: {}",
            file.display()
        ));
    }
    dir.canonicalize()
        .map_err(|error| format!("cannot canonicalize worker-jail canary: {error}"))
}

/// Fence the password-database home even when the daemon uses a different HOME.
/// Head shells retain read access to both canaries; a failed repair is non-fatal
/// for heads so a missing canary cannot block an operator shell.
#[cfg(unix)]
pub fn prepare_homes(daemon_home: &Path, fenced: bool) -> Result<Vec<PathBuf>, String> {
    #[cfg(not(test))]
    let account = crate::production_storage::account_home();
    #[cfg(test)]
    let account = TEST_PREPARATION_HOME.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.as_mut() {
            Some((home, reads)) => {
                *reads += 1;
                Ok(home.clone())
            }
            None => crate::production_storage::account_home(),
        }
    });
    prepare_homes_with_account(daemon_home, account, fenced)
}

#[cfg(all(test, unix))]
thread_local! {
    static TEST_PREPARATION_HOME: std::cell::RefCell<Option<(PathBuf, usize)>> = const { std::cell::RefCell::new(None) };
}

/// A fixture must observe every expected preparation on the installing thread.
/// A spawned thread that misses this override therefore fails the fixture's
/// read-count assertion instead of silently testing the real account home.
#[cfg(all(test, unix))]
pub(crate) fn with_test_preparation_home<R>(
    home: &Path,
    expected_reads: usize,
    run: impl FnOnce() -> R,
) -> R {
    struct Restore {
        previous: Option<(PathBuf, usize)>,
        expected_reads: usize,
        _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let current = TEST_PREPARATION_HOME.with(|slot| slot.replace(self.previous.take()));
            if !std::thread::panicking() {
                assert_eq!(
                    current.unwrap().1,
                    self.expected_reads,
                    "account-home preparation escaped its fixture thread or did not run"
                );
            }
        }
    }
    let previous = TEST_PREPARATION_HOME.with(|slot| slot.replace(Some((home.to_path_buf(), 0))));
    let _restore = Restore {
        previous,
        expected_reads,
        _same_thread: std::marker::PhantomData,
    };
    run()
}

#[cfg(unix)]
fn prepare_homes_with_account(
    daemon_home: &Path,
    account: std::io::Result<PathBuf>,
    fenced: bool,
) -> Result<Vec<PathBuf>, String> {
    let mut homes = match account {
        Ok(home) => vec![home],
        Err(error) if fenced => {
            return Err(format!("cannot resolve worker-jail account home: {error}"))
        }
        Err(_) => Vec::new(),
    };
    homes.push(daemon_home.to_path_buf());
    let mut dirs = Vec::new();
    for home in homes {
        match prepare(&home) {
            Ok(dir) if !dirs.contains(&dir) => dirs.push(dir),
            Err(error) if fenced => return Err(error),
            _ => {}
        }
    }
    Ok(dirs)
}

/// What the current process may do with one canary path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanaryAccess {
    /// The kernel policy over this process denies reading the canary.
    Denied,
    /// The canary exists and this process can read it.
    Readable,
    /// No canary exists at this path.
    Missing,
    /// The probe could not establish a deny: `sandbox_check` failed, or it
    /// allowed the read while the canary's metadata was unavailable. The shim
    /// treats this as "not jailed". That keeps the fence because the worker
    /// launcher runs the same probe after applying the jail and refuses to
    /// start the shell on anything but [`CanaryAccess::Denied`]: a running
    /// worker shell has a proven deny.
    Unproven,
}

/// Probe one canary path from inside the current process's policy.
///
/// Linux: Landlock does not mediate stat, so a jailed process sees the file
/// exist while `open` fails with EACCES. A stat that itself fails with EACCES
/// means an ancestor's mode was changed (Landlock cannot stop chmod); that is
/// treated as denied so a worker cannot hide the canary that way.
///
/// macOS: ask Seatbelt whether this process may read the file's data. The
/// query opens nothing and, with SANDBOX_CHECK_NO_REPORT, logs no violation.
/// `sandbox_check` also answers 1 ("denied") for a path that does not exist,
/// so existence is checked first: a missing canary is never a deny.
pub fn access(path: &Path) -> CanaryAccess {
    probe(path).access
}

/// One probe's verdict plus the raw facts behind it, for diagnostics.
struct Probe {
    access: CanaryAccess,
    detail: String,
}

fn probe(path: &Path) -> Probe {
    #[cfg(target_os = "linux")]
    {
        linux_probe(path)
    }
    #[cfg(target_os = "macos")]
    {
        macos_probe(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Probe {
            access: CanaryAccess::Missing,
            detail: format!("{}: no canary probe on this platform", path.display()),
        }
    }
}

#[cfg(unix)]
fn errno_of(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(-1)
}

#[cfg(target_os = "linux")]
fn linux_probe(path: &Path) -> Probe {
    let (access, detail) = match std::fs::metadata(path) {
        Err(error) if error.raw_os_error() == Some(libc::EACCES) => (
            CanaryAccess::Denied,
            format!("stat errno={}", errno_of(&error)),
        ),
        Err(error) => (
            CanaryAccess::Missing,
            format!("stat errno={}", errno_of(&error)),
        ),
        Ok(metadata) if !metadata.is_file() => (CanaryAccess::Missing, "stat: not a file".into()),
        Ok(_) => match std::fs::File::open(path) {
            Ok(_) => (CanaryAccess::Readable, "stat ok, open ok".into()),
            Err(error) if error.raw_os_error() == Some(libc::EACCES) => (
                CanaryAccess::Denied,
                format!("stat ok, open errno={}", errno_of(&error)),
            ),
            Err(error) => (
                CanaryAccess::Missing,
                format!("stat ok, open errno={}", errno_of(&error)),
            ),
        },
    };
    Probe {
        access,
        detail: format!("{}: {detail} -> {access:?}", path.display()),
    }
}

#[cfg(target_os = "macos")]
mod seatbelt {
    unsafe extern "C" {
        pub fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            filter: libc::c_int,
            ...
        ) -> libc::c_int;
        /// Exported by libsystem_sandbox; ORed into the filter type.
        pub static SANDBOX_CHECK_NO_REPORT: libc::c_int;
    }
    /// `SANDBOX_FILTER_PATH` in libsystem_sandbox's filter-type enumeration.
    pub const SANDBOX_FILTER_PATH: libc::c_int = 1;
}

/// Resolve the longest existing prefix of `path` and append the rest, since
/// Seatbelt rules name resolved paths (for example /private/var for /var). The
/// canary's own directory may deny metadata inside the jail, so the canary
/// itself often cannot be resolved there.
#[cfg(target_os = "macos")]
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut prefix = path;
    loop {
        if let Ok(resolved) = prefix.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(resolved, |path, name| path.join(name));
        }
        match (prefix.parent(), prefix.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                prefix = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_probe(path: &Path) -> Probe {
    use std::os::unix::ffi::OsStrExt;
    let resolved = resolve_existing_prefix(path);
    let probe = |access: CanaryAccess, detail: String| Probe {
        access,
        detail: format!(
            "{} (resolved {}): {detail} -> {access:?}",
            path.display(),
            resolved.display()
        ),
    };
    // Worker profiles permit metadata for this path and its ancestors. A missing
    // file is not evidence of confinement, even when a policy would deny data.
    // An outer sandbox may still hide metadata; record that error separately.
    let metadata = std::fs::symlink_metadata(&resolved);
    let stat = match &metadata {
        Ok(metadata) if metadata.is_file() => "stat ok".to_string(),
        Ok(_) => return probe(CanaryAccess::Missing, "stat: not a file".into()),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
            return probe(
                CanaryAccess::Missing,
                format!("stat errno={}", errno_of(error)),
            )
        }
        Err(error) => format!("stat errno={}", errno_of(error)),
    };
    let Ok(c_path) = std::ffi::CString::new(resolved.as_os_str().as_bytes()) else {
        return probe(CanaryAccess::Missing, "path contains NUL".into());
    };
    // SAFETY: both strings are NUL-terminated and live across the call; the
    // path filter takes exactly one const char* variadic argument. errno is
    // cleared first so the value read afterwards belongs to this call.
    let (result, errno) = unsafe {
        *libc::__error() = 0;
        let result = seatbelt::sandbox_check(
            libc::getpid(),
            c"file-read-data".as_ptr(),
            seatbelt::SANDBOX_FILTER_PATH | seatbelt::SANDBOX_CHECK_NO_REPORT,
            c_path.as_ptr(),
        );
        (result, *libc::__error())
    };
    let detail = format!("{stat}, sandbox_check={result} errno={errno}");
    match result {
        0 if metadata.is_ok() => probe(CanaryAccess::Readable, detail),
        // Policy allows the read, but the metadata check did not confirm the
        // canary exists.
        0 => probe(CanaryAccess::Unproven, detail),
        1 => probe(CanaryAccess::Denied, detail),
        _ => probe(CanaryAccess::Unproven, detail),
    }
}

/// What the probe saw for every candidate canary (and, on Linux, the
/// no_new_privs value), joined on one line for test and support diagnostics.
/// It never changes a decision.
pub fn diagnostics() -> String {
    let mut lines = Vec::new();
    #[cfg(target_os = "linux")]
    // SAFETY: PR_GET_NO_NEW_PRIVS takes no pointer arguments.
    lines.push(format!("no_new_privs={}", unsafe {
        libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0)
    }));
    for home in candidate_homes() {
        lines.push(probe(&path_for_home(&home)).detail);
    }
    lines.join("; ")
}

/// Whether this process runs inside AFT's worker jail, judged only by kernel
/// state and the account's password-database home.
///
/// Linux excludes a process only when `no_new_privs` is explicitly zero. A
/// seccomp filter can hide that query, but cannot remove the canary's deny.
pub fn inside_worker_jail() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: PR_GET_NO_NEW_PRIVS takes no pointer arguments.
        inside_worker_jail_with_no_new_privs(unsafe {
            libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0)
        })
    }
    #[cfg(not(target_os = "linux"))]
    denied_candidate()
}

#[cfg(target_os = "linux")]
pub(crate) fn inside_worker_jail_with_no_new_privs(result: i32) -> bool {
    if result == 0 {
        return false;
    }
    denied_candidate()
}

fn denied_candidate() -> bool {
    candidate_homes()
        .iter()
        .any(|home| access(&path_for_home(home)) == CanaryAccess::Denied)
}

/// Homes whose canary the shim checks. The password database cannot be
/// changed by a worker; `$HOME` is only an extra place to look, so unsetting
/// or redirecting it can never hide the account's canary.
fn candidate_homes() -> Vec<PathBuf> {
    #[cfg(test)]
    if let Some(homes) = TEST_HOMES.with(|slot| slot.borrow().clone()) {
        return homes;
    }
    let mut homes = Vec::new();
    #[cfg(unix)]
    if let Ok(home) = crate::production_storage::account_home() {
        homes.push(home);
    }
    if let Some(home) = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    {
        if !homes.contains(&home) {
            homes.push(home);
        }
    }
    homes
}

#[cfg(test)]
thread_local! {
    static TEST_HOMES: std::cell::RefCell<Option<Vec<PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

/// Point this thread's shim checks at test homes instead of the account's.
#[cfg(all(test, unix))]
pub(crate) fn with_test_homes<R>(homes: Vec<PathBuf>, run: impl FnOnce() -> R) -> R {
    let previous = TEST_HOMES.with(|slot| slot.replace(Some(homes)));
    struct Restore(Option<Vec<PathBuf>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_HOMES.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(previous);
    run()
}

/// Launcher check, after the jail is applied and before the shell starts: the
/// jail must deny this canary. A jail that cannot is refused, never run
/// unfenced.
pub fn verify_denied_after_apply(path: &Path) -> Result<(), String> {
    match access(path) {
        CanaryAccess::Denied => Ok(()),
        other => Err(format!(
            "the worker jail does not deny its gh-shim canary {} ({other:?}); refusing to start an unfenced worker shell",
            path.display()
        )),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn prepare_creates_and_repairs_the_canary() {
        let home = tempfile::tempdir().unwrap();
        let dir = prepare(home.path()).unwrap();
        assert_eq!(dir, dir_for_home(home.path()).canonicalize().unwrap());
        let file = dir.join(CANARY_FILE);
        assert_eq!(std::fs::read(&file).unwrap(), CANARY_CONTENTS);
        std::fs::write(&file, b"tampered").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare(home.path()).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), CANARY_CONTENTS);
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn distinct_account_and_daemon_homes_are_prepared_for_workers_and_heads() {
        let account = tempfile::tempdir().unwrap();
        let daemon = tempfile::tempdir().unwrap();
        for fenced in [true, false] {
            let dirs = prepare_homes_with_account(daemon.path(), Ok(account.path().into()), fenced)
                .unwrap();
            assert_eq!(
                dirs,
                vec![
                    dir_for_home(account.path()).canonicalize().unwrap(),
                    dir_for_home(daemon.path()).canonicalize().unwrap()
                ]
            );
        }
        let dirs =
            prepare_homes_with_account(account.path(), Ok(account.path().into()), true).unwrap();
        assert_eq!(dirs.len(), 1);
    }

    #[test]
    fn missing_account_home_refuses_fenced_workers_but_not_heads() {
        let daemon = tempfile::tempdir().unwrap();
        assert!(prepare_homes_with_account(
            daemon.path(),
            Err(std::io::Error::other("no passwd entry")),
            true
        )
        .is_err());
        assert_eq!(
            prepare_homes_with_account(
                daemon.path(),
                Err(std::io::Error::other("no passwd entry")),
                false
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn unwritable_account_home_refuses_fenced_workers() {
        let fixture = tempfile::tempdir().unwrap();
        let account = fixture.path().join("account");
        std::fs::write(&account, "not a directory").unwrap();
        assert!(prepare_homes_with_account(fixture.path(), Ok(account), true).is_err());
    }

    #[test]
    fn prepare_refuses_a_canary_directory_replaced_by_a_symlink() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let dir = dir_for_home(home.path());
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &dir).unwrap();
        assert!(prepare(home.path()).is_err());
    }

    #[test]
    fn an_unconfined_process_sees_a_readable_or_missing_canary() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(access(&path_for_home(home.path())), CanaryAccess::Missing);
        prepare(home.path()).unwrap();
        assert_eq!(access(&path_for_home(home.path())), CanaryAccess::Readable);
        with_test_homes(vec![home.path().to_path_buf()], || {
            assert!(!inside_worker_jail());
        });
        assert!(verify_denied_after_apply(&path_for_home(home.path())).is_err());
    }
}
