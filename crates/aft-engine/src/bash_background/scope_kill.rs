use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub(super) fn kill(cgroup: &Path) -> io::Result<()> {
    kill_with(cgroup, |file| file.write_all(b"1"))
}

fn open_kill_file(path: &Path) -> io::Result<File> {
    #[cfg(test)]
    if let Some(error) = OPEN_ERROR_FOR_TEST.with(|slot| slot.borrow_mut().take()) {
        return Err(error(path));
    }
    OpenOptions::new().write(true).open(path)
}

#[cfg(test)]
type OpenErrorForTest = Box<dyn FnOnce(&Path) -> io::Error>;

#[cfg(test)]
thread_local! {
    static OPEN_ERROR_FOR_TEST: std::cell::RefCell<Option<OpenErrorForTest>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_scope_kill_open_error_for_test<T>(
    error: impl FnOnce(&Path) -> io::Error + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<OpenErrorForTest>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OPEN_ERROR_FOR_TEST.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(OPEN_ERROR_FOR_TEST.with(|slot| slot.replace(Some(Box::new(error)))));
    run()
}

fn kill_with(cgroup: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let existed_before = cgroup.try_exists();
    let (phase, result) = match open_kill_file(&cgroup.join("cgroup.kill")) {
        Ok(mut file) => ("write", write(&mut file)),
        Err(error) => ("open", Err(error)),
    };
    let Err(error) = result else { return Ok(()) };
    let exists_after = cgroup.try_exists();
    // The process-group kill can empty the scope before we reach cgroup.kill.
    // Linux can return ENOENT or ENODEV from either open or write while
    // systemd removes the scope. Accept neither without proof it is gone.
    if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENODEV))
        && matches!(exists_after, Ok(false))
    {
        return Ok(());
    }
    log::warn!(
        "task scope kill {phase} failed for {}: {error} (errno={:?}, existed before={existed_before:?}, exists after={exists_after:?})",
        cgroup.display(),
        error.raw_os_error(),
    );
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn task_scope_kill_accepts_collection_between_open_and_write() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join("scope");
        for errno in [libc::ENODEV, libc::ENOENT] {
            fs::create_dir(&scope).unwrap();
            fs::write(scope.join("cgroup.kill"), "0").unwrap();
            kill_with(&scope, |_| {
                fs::remove_dir_all(&scope).unwrap();
                Err(io::Error::from_raw_os_error(errno))
            })
            .unwrap();
        }
    }

    #[test]
    fn task_scope_kill_requires_directory_absence_for_collection() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("cgroup.kill"), "0").unwrap();
        for errno in [libc::ENODEV, libc::ENOENT] {
            let error = with_scope_kill_open_error_for_test(
                move |_| io::Error::from_raw_os_error(errno),
                || kill(root.path()).unwrap_err(),
            );
            assert_eq!(error.raw_os_error(), Some(errno));
            let error =
                kill_with(root.path(), |_| Err(io::Error::from_raw_os_error(errno))).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn task_scope_kill_preserves_other_write_errors_even_if_scope_disappears() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join("scope");
        fs::create_dir(&scope).unwrap();
        fs::write(scope.join("cgroup.kill"), "0").unwrap();
        let error = kill_with(&scope, |_| {
            fs::remove_dir_all(&scope).unwrap();
            Err(io::Error::from_raw_os_error(libc::EACCES))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
