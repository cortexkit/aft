//! Immutable test executables shared across worktrees and archived test runs.

use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const KEEP: usize = 4;
const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
const SCAN_LIMIT: usize = 256;
static PRUNE: Once = Once::new();

pub fn root() -> PathBuf {
    // The OS temp directory is shared by local worktrees and is available on
    // CI runners without depending on a developer's home or an archive path.
    std::env::temp_dir().join("cortexkit-aft-test-helpers")
}

pub fn key(sources: &[&[u8]], rustc: &str, target: &str) -> String {
    let mut hash = Sha256::new();
    for bytes in sources
        .iter()
        .copied()
        .chain([rustc.as_bytes(), target.as_bytes()])
    {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())
}

pub fn executable(entry: &Path, target: &str) -> PathBuf {
    entry.join(if target.contains("windows") {
        "fake-lsp-server.exe"
    } else {
        "fake-lsp-server"
    })
}

pub fn install(
    root: &Path,
    key: &str,
    target: &str,
    build: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let root = root.join("fake-lsp-server");
    fs::create_dir_all(&root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".build.lock"))?;
    lock.lock()?;
    let entry = root.join(key);
    let binary = executable(&entry, target);
    if !binary.is_file() {
        let temporary = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(&root)?;
        let pending = executable(temporary.path(), target);
        build(&pending)?;
        // Read access is enough for fsync. A writable executable descriptor can
        // be inherited by a concurrent fork and make Linux exec return ETXTBSY
        // even after this thread closes it and publishes the immutable path.
        #[cfg(target_os = "linux")]
        aft_store::sync::sync_all(&fs::File::open(&pending)?, &pending)?;
        // Windows FlushFileBuffers requires a writable handle.
        #[cfg(not(target_os = "linux"))]
        aft_store::sync::sync_all(&OpenOptions::new().write(true).open(&pending)?, &pending)?;
        // Installation age lives beside the executable. Never write-open or
        // touch an installed executable: macOS can revoke running signatures.
        fs::write(
            temporary.path().join("installed"),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string(),
        )?;
        fs::rename(temporary.path(), &entry)?;
    }
    PRUNE.call_once(|| {
        // Nextest runs each test in a separate process. Its run ID prevents
        // those processes from all repeating the same bounded directory scan.
        let run = std::env::var("NEXTEST_RUN_ID").ok();
        let marker = root.join(".last-pruned-run");
        if run
            .as_ref()
            .is_some_and(|run| fs::read_to_string(&marker).ok().as_ref() == Some(run))
        {
            return;
        }
        prune(&root, &entry, SystemTime::now());
        if let Some(run) = run {
            let _ = fs::write(marker, run);
        }
    });
    Ok(binary)
}

pub fn prune(root: &Path, current: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries
        .take(SCAN_LIMIT)
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .filter_map(|entry| {
            let installed = fs::read_to_string(entry.path().join("installed"))
                .ok()?
                .parse::<u64>()
                .ok()?;
            Some((UNIX_EPOCH + Duration::from_secs(installed), entry.path()))
        })
        .collect();
    entries.sort_by_key(|(installed, _)| std::cmp::Reverse(*installed));
    // Reserve a slot for the entry being returned even when it is old.
    let mut kept = 1;
    for (installed, path) in entries {
        if path == current {
            continue;
        }
        if kept >= KEEP || now.duration_since(installed).unwrap_or_default() > MAX_AGE {
            let _ = fs::remove_dir_all(path);
        } else {
            kept += 1;
        }
    }
}

pub fn install_bytes(root: &Path, key: &str, target: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    install(root, key, target, |pending| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(pending)?;
        file.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        aft_store::sync::sync_all(&file, pending)
    })
}
