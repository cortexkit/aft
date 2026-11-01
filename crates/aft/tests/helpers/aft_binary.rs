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
    deploy(&source_binary(), &std::env::temp_dir())
        .unwrap_or_else(|error| panic!("cannot deploy real AFT fixture image: {error}"))
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

#[cfg(unix)]
fn deploy(source: &Path, temporary_root: &Path) -> io::Result<PathBuf> {
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
    let mut input = File::open(&source)?;
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
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending_image)?;
    let copied = io::copy(&mut input, &mut output)?;
    if copied != identity.size {
        return Err(io::Error::other("AFT fixture image copy was incomplete"));
    }
    output.sync_all()?;
    let mut permissions = input.metadata()?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&pending_image, permissions)?;
    drop(output);
    unchanged(&input, &source, &identity)?;
    // Publish a completed directory by rename. Readers never see a partial
    // image and an existing nonempty source-identity directory is not replaced.
    fs::rename(pending.path(), &directory)?;
    unchanged(&input, &source, &identity)?;
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
}
