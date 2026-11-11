#[cfg(unix)]
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::time::SystemTime;

/// Nextest remaps archive binaries into its extraction directory. Prefer its
/// runtime executable path to Cargo's path captured when the tests were built.
pub fn source_binary() -> PathBuf {
    std::env::var_os("AFT_TEST_AFT_BINARY")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")))
}

/// Deploy the real image once per test run on the fixture temp filesystem.
/// This keeps normal pin creation on the same filesystem rather than copying
/// a large debug image from the build server's target mount for every child.
#[cfg(unix)]
pub fn aft_binary() -> PathBuf {
    #[cfg(target_os = "macos")]
    let warm = warm_fixture_image;
    #[cfg(not(target_os = "macos"))]
    let warm = |_: &Path| Ok(());
    deploy_with_warmup(&source_binary(), &std::env::temp_dir(), |_| {}, warm)
        .unwrap_or_else(|error| panic!("cannot deploy real AFT fixture image: {error}"))
}

/// Pay macOS's first-exec cost before a command's own timeout starts. Production
/// warms its startup pin too; the integration launcher override uses this copy.
#[cfg(target_os = "macos")]
fn warm_fixture_image(image: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let mut child = Command::new(image)
        .env_clear()
        .args(["gh-shim", "--shim-version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(io::Error::other(format!("fixture warmup exited {status}")))
            }
            Ok(None) if start.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                // SAFETY: the warmup child was placed in its own process group.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err(match result {
                    Err(error) => error,
                    _ => io::Error::other("fixture image warmup exceeded 30s"),
                });
            }
        }
    }
}

// Windows holds the running .exe open and does not copy or link it into storage.
#[cfg(not(unix))]
pub fn aft_binary() -> PathBuf {
    source_binary()
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct Identity {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[cfg(unix)]
impl Identity {
    fn from_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            path: path.to_path_buf(),
            size: metadata.len(),
            modified: metadata.modified()?,
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    fn key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(format!("{self:?}"));
        format!("{:x}", hash.finalize())
    }
}

#[cfg(unix)]
fn unchanged(source: &File, path: &Path, expected: &Identity) -> io::Result<()> {
    let held = Identity::from_metadata(path, &source.metadata()?)?;
    let named = Identity::from_metadata(path, &fs::metadata(path)?)?;
    if &held != expected || &named != expected {
        return Err(io::Error::other(
            "AFT source image changed during fixture deployment",
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
fn deploy(source: &Path, temporary_root: &Path) -> io::Result<PathBuf> {
    deploy_observing_copy(source, temporary_root, |_| {})
}

#[cfg(all(test, unix))]
fn deploy_observing_copy(
    source: &Path,
    temporary_root: &Path,
    copied: impl FnOnce(&Path),
) -> io::Result<PathBuf> {
    deploy_with_warmup(source, temporary_root, copied, |_| Ok(()))
}

#[cfg(unix)]
fn deploy_with_warmup(
    source: &Path,
    temporary_root: &Path,
    copied: impl FnOnce(&Path),
    warm: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    use std::process::{Command, Stdio};

    let source = source.canonicalize()?;
    let run = std::env::var("NEXTEST_RUN_ID")
        .ok()
        .map(|run| {
            let mut hash = Sha256::new();
            hash.update(run);
            format!("nextest-{:x}", hash.finalize())
        })
        .unwrap_or_else(|| format!("cargo-{}", std::process::id()));
    let cache = temporary_root.join(format!("aft-real-module-images-{run}"));
    fs::create_dir_all(&cache)?;
    // The helper is included by several fixture modules. A file lock makes
    // them share one publication even though their Rust statics are separate.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache.join("publish.lock"))?;
    lock.lock()?;
    let input = File::open(&source)?;
    let identity = Identity::from_metadata(&source, &input.metadata()?)?;
    let directory = cache.join(identity.key());
    let image = directory.join(if cfg!(windows) { "aft.exe" } else { "aft" });
    if image.exists() {
        let metadata = fs::metadata(&image)?;
        if !metadata.is_file()
            || metadata.len() != identity.size
            || !metadata.permissions().readonly()
        {
            return Err(io::Error::other(
                "cached AFT fixture image is not complete and immutable",
            ));
        }
        unchanged(&input, &source, &identity)?;
        return Ok(image);
    }
    let pending = tempfile::Builder::new()
        .prefix(".publishing-")
        .tempdir_in(&cache)?;
    let pending_image = pending.path().join(image.file_name().unwrap());
    // Other test threads fork while this copy is in progress. Only the writer
    // child may hold a writable image descriptor: an unrelated fork could
    // retain a parent's descriptor after publication and make exec fail with
    // "text file busy" (ETXTBSY) even after the parent closed it.
    let status = Command::new("/bin/sh")
        .args([
            "-c",
            "umask 077; set -C; /bin/cat > \"$1\"",
            "aft-fixture-image-writer",
        ])
        .arg(&pending_image)
        .stdin(Stdio::from(input.try_clone()?))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other("AFT fixture image writer failed"));
    }
    if fs::metadata(&pending_image)?.len() != identity.size {
        return Err(io::Error::other("AFT fixture image copy was incomplete"));
    }
    File::open(&pending_image)?.sync_all()?;
    copied(&pending_image);
    let mut permissions = input.metadata()?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&pending_image, permissions)?;
    unchanged(&input, &source, &identity)?;
    // Publish a completed directory by rename. Readers never see a partial
    // image and an existing nonempty source-identity directory is not replaced.
    fs::rename(pending.path(), &directory)?;
    unchanged(&input, &source, &identity)?;
    // Warm the final pathname while the publication lock still excludes other
    // fixtures. Never cache a failed warmup as an image ready for tool traffic.
    if let Err(error) = warm(&image) {
        fs::remove_dir_all(&directory)?;
        return Err(error);
    }
    Ok(image)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn source_image(root: &Path, bytes: &[u8]) -> PathBuf {
        let source = root.join("source-aft");
        fs::write(&source, bytes).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        source
    }

    #[test]
    fn fixture_image_is_complete_immutable_and_reused() {
        let root = tempfile::tempdir().unwrap();
        let source = source_image(root.path(), b"real image bytes");
        let first = deploy(&source, root.path()).unwrap();
        let second = deploy(&source, root.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"real image bytes");
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&first).unwrap().ino()
        );
        assert!(fs::metadata(&first).unwrap().permissions().readonly());
        assert_ne!(
            fs::metadata(&first).unwrap().permissions().mode() & 0o111,
            0
        );
    }

    #[test]
    fn fixture_image_is_warmed_once_at_its_immutable_final_path() {
        let root = tempfile::tempdir().unwrap();
        let source = source_image(root.path(), b"real image bytes");
        let warmed = std::cell::RefCell::new(Vec::new());
        let first = deploy_with_warmup(
            &source,
            root.path(),
            |_| {},
            |image| {
                assert_eq!(fs::read(image).unwrap(), b"real image bytes");
                assert!(fs::metadata(image).unwrap().permissions().readonly());
                assert!(!image
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".publishing-"));
                warmed.borrow_mut().push(image.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        let second = deploy_with_warmup(
            &source,
            root.path(),
            |_| panic!("a cached fixture must not be copied again"),
            |_| panic!("a cached fixture must not be warmed again"),
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(*warmed.borrow(), vec![first]);
    }

    #[test]
    fn failed_fixture_warmup_is_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let source = source_image(root.path(), b"real image bytes");
        let failed_path = std::cell::RefCell::new(None);
        let error = deploy_with_warmup(
            &source,
            root.path(),
            |_| {},
            |image| {
                failed_path.replace(Some(image.to_path_buf()));
                Err(io::Error::other("injected warmup failure"))
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected warmup failure"));
        let image = failed_path.into_inner().unwrap();
        assert!(!image.parent().unwrap().exists());

        let copied = std::cell::Cell::new(false);
        let warmed = std::cell::Cell::new(false);
        let retry = deploy_with_warmup(
            &source,
            root.path(),
            |_| copied.set(true),
            |_| {
                warmed.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(retry, image);
        assert!(copied.get(), "the failed image must be republished");
        assert!(
            warmed.get(),
            "the retry must finish warmup before returning"
        );
    }

    #[test]
    fn rebuilt_source_never_reuses_a_stale_fixture_image() {
        let root = tempfile::tempdir().unwrap();
        let source = source_image(root.path(), b"first image");
        let old = deploy(&source, root.path()).unwrap();
        let replacement = root.path().join("replacement");
        fs::write(&replacement, b"other image").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(replacement, &source).unwrap();
        let new = deploy(&source, root.path()).unwrap();
        assert_ne!(old, new);
        assert_eq!(fs::read(old).unwrap(), b"first image");
        assert_eq!(fs::read(new).unwrap(), b"other image");
    }

    #[test]
    fn parallel_fixture_deployment_publishes_one_complete_image() {
        let root = tempfile::tempdir().unwrap();
        let bytes = vec![0x5a; 128 * 1024];
        let source = source_image(root.path(), &bytes);
        let paths = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let source = &source;
                    let root = root.path();
                    let bytes = &bytes;
                    scope.spawn(move || {
                        let image = deploy(source, root).unwrap();
                        assert_eq!(fs::read(&image).unwrap(), *bytes);
                        image
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(paths.iter().all(|path| path == &paths[0]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fixture_image_writer_is_not_inherited_by_parallel_forks() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        struct ForkGuard {
            pid: libc::pid_t,
            release: UnixStream,
        }
        impl Drop for ForkGuard {
            fn drop(&mut self) {
                self.release.shutdown(std::net::Shutdown::Both).unwrap();
                // SAFETY: pid names only the child created below.
                unsafe {
                    while libc::waitpid(self.pid, std::ptr::null_mut(), 0) == -1
                        && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                    {
                    }
                }
            }
        }

        let root = tempfile::tempdir().unwrap();
        let source = source_image(root.path(), b"#!/bin/sh\nexit 0\n");
        let mut inherited = None;
        let image = deploy_observing_copy(&source, root.path(), |_| {
            let (reader, release) = UnixStream::pair().unwrap();
            let reader_fd = reader.as_raw_fd();
            let release_fd = release.as_raw_fd();
            // Model an unrelated test thread's fork retaining all descriptors
            // until after the copied image has been published and executed.
            // SAFETY: the child calls only async-signal-safe libc functions.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());
            if pid == 0 {
                unsafe {
                    libc::close(release_fd);
                    let mut byte = 0u8;
                    libc::read(reader_fd, std::ptr::addr_of_mut!(byte).cast(), 1);
                    libc::_exit(0);
                }
            }
            inherited = Some(ForkGuard { pid, release });
        })
        .unwrap();
        let result = std::process::Command::new(image).status();
        drop(inherited);
        assert!(result.unwrap().success());
    }
}
