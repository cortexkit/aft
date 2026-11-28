//! Host-owned endpoint generations. All bind/unlink operations serialize on a
//! namespace lock; a separate held file lock proves a generation is still live.
//! Neither lock nor registration records are in a worker-writable directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const PREFIX: &str = ".gh-broker-generation-";

struct NamespaceLock(File);
impl Drop for NamespaceLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
fn lock_namespace(root: &Path) -> io::Result<NamespaceLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join(".gh-broker-namespace.lock"))?;
    if file.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("broker namespace lock owner mismatch"));
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(NamespaceLock(file))
}
fn private_dir(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(
            "broker namespace must be owned by this user with mode 0700",
        ));
    }
    Ok(file)
}
fn random_name() -> io::Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn task_name(name: &str) -> bool {
    name.len() == 12
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    generation: String,
    token: String,
    socket: String,
    device: u64,
    inode: u64,
}
fn read_registration(path: &Path) -> io::Result<Registration> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.len() > 4096
    {
        return Err(io::Error::other("invalid broker registration"));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn write_record(path: &Path, registration: &Registration, replace: bool) -> io::Result<()> {
    let temporary = path.with_file_name(format!(".write-{}", random_name()?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(registration).map_err(io::Error::other)?)?;
        if replace {
            fs::rename(&temporary, path)
        } else {
            fs::hard_link(&temporary, path)
        }
    })();
    let _ = fs::remove_file(temporary);
    result
}
fn publish_index(path: &Path, registration: &Registration) -> io::Result<()> {
    let task = registration.socket.strip_suffix(".gh-read.sock").unwrap();
    // Linking the complete initial record publishes an immutable reservation
    // atomically, without replacing another generation's reservation.
    fs::hard_link(
        path.parent()
            .unwrap()
            .join(&registration.generation)
            .join(format!("{task}.json")),
        path,
    )
}

pub(crate) struct Generation {
    root: PathBuf,
    name: String,
    directory: PathBuf,
    _lease: crate::fs_lock::AdvisoryLockGuard,
}
impl Generation {
    pub(crate) fn new(root: &Path) -> io::Result<Arc<Self>> {
        match fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let _root = private_dir(root)?;
        let _namespace = lock_namespace(root)?;
        reclaim_dead(root)?;
        let name = format!("{PREFIX}{}", random_name()?);
        let directory = root.join(&name);
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("lease"))?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            root: root.to_owned(),
            name,
            directory,
            _lease: crate::fs_lock::AdvisoryLockGuard::from_locked_file(lease),
        }))
    }
    pub(crate) fn bind(self: &Arc<Self>, task: &str) -> io::Result<(UnixListener, Arc<Endpoint>)> {
        if !task_name(task) {
            return Err(io::Error::other("invalid command temp name"));
        }
        let socket_name = format!("{task}.gh-read.sock");
        let path = self.root.join(&socket_name);
        let record = self.directory.join(format!("{task}.json"));
        let registration = Registration {
            generation: self.name.clone(),
            token: random_name()?,
            socket: socket_name,
            device: 0,
            inode: 0,
        };
        let owner = Arc::new(Endpoint {
            path: path.clone(),
            generation: self.clone(),
            state: Mutex::new(Some((registration.clone(), record.clone(), false))),
        });
        // The owner exists before the lock, so an error drops the lock before
        // cleanup. The immutable index reserves the pathname before bind.
        let namespace = lock_namespace(&self.root)?;
        let result = (|| {
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "broker endpoint already exists",
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            write_record(&record, &registration, false)?;
            publish_index(
                &self.root.join(format!("{task}.gh-read.owner")),
                &registration,
            )?;
            let listener = UnixListener::bind(&path)?;
            owner.state.lock().unwrap().as_mut().unwrap().2 = true;
            let metadata = fs::symlink_metadata(&path)?;
            let mut registered = registration.clone();
            registered.device = metadata.dev();
            registered.inode = metadata.ino();
            *owner.state.lock().unwrap() = Some((registered.clone(), record.clone(), true));
            write_record(&record, &registered, true)?;
            listener.set_nonblocking(true)?;
            Ok(listener)
        })();
        drop(namespace);
        match result {
            Ok(listener) => Ok((listener, owner)),
            Err(error) => {
                owner.remove();
                Err(error)
            }
        }
    }
}

/// One object owns removal. Revocation and later drops may call remove, but only
/// the first call can consume the registration; no delayed second unlink exists.
pub(crate) struct Endpoint {
    path: PathBuf,
    generation: Arc<Generation>,
    state: Mutex<Option<(Registration, PathBuf, bool)>>,
}
impl Endpoint {
    pub(crate) fn path(&self) -> PathBuf {
        self.path.clone()
    }
    pub(crate) fn remove(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some((registration, record, bound)) = state.as_ref() else {
            return;
        };
        let Ok(_namespace) = lock_namespace(&self.generation.root) else {
            return;
        };
        let index = self.generation.root.join(
            registration
                .socket
                .replace(".gh-read.sock", ".gh-read.owner"),
        );
        if read_registration(&index).is_ok_and(|current| {
            current.token == registration.token && current.generation == registration.generation
        }) {
            // A failed bind never owned a listener. Keep records on I/O failure
            // so restart reclamation can retry rather than orphan the pathname.
            if *bound && unlink_registration(&self.generation.root, registration).is_err() {
                return;
            }
            if let Err(error) = fs::remove_file(index) {
                if error.kind() != io::ErrorKind::NotFound {
                    return;
                }
            }
        }
        if read_registration(record).is_ok_and(|current| current.token == registration.token) {
            if let Err(error) = fs::remove_file(record) {
                if error.kind() != io::ErrorKind::NotFound {
                    return;
                }
            }
        }
        state.take();
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.remove();
    }
}

// The namespace lock serializes identity checks, binds and removal. The inode
// check is additional protection, not a claim that stat+unlink is atomic alone.
fn unlink_registration(root: &Path, registration: &Registration) -> io::Result<()> {
    let Some(task) = registration
        .socket
        .strip_suffix(".gh-read.sock")
        .filter(|name| task_name(name))
    else {
        return Err(io::Error::other("invalid socket record"));
    };
    let path = root.join(format!("{task}.gh-read.sock"));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_socket()
        && metadata.uid() == unsafe { libc::geteuid() }
        && (registration.inode == 0
            || (metadata.dev() == registration.device && metadata.ino() == registration.inode))
    {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn reclaim_dead(root: &Path) -> io::Result<()> {
    // Caller holds the namespace lock, including throughout the liveness check
    // and removal. A live generation's advisory lock is never inferred from PID.
    for item in fs::read_dir(root)? {
        let item = item?;
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(nonce) = name.strip_prefix(PREFIX) else {
            continue;
        };
        if nonce.len() != 32
            || !nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            continue;
        }
        let directory = item.path();
        if private_dir(&directory).is_err() {
            continue;
        }
        let lease = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join("lease"))
        {
            Ok(file) => file,
            Err(_) => continue,
        };
        if !lease.metadata()?.is_file() || lease.metadata()?.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        let _lease = crate::fs_lock::AdvisoryLockGuard::from_locked_file(lease);
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let file = entry.file_name();
            let Some(file) = file.to_str() else {
                continue;
            };
            if file.starts_with(".write-") {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            let Some(task) = file.strip_suffix(".json").filter(|name| task_name(name)) else {
                continue;
            };
            let registration = match read_registration(&entry.path()) {
                Ok(record) => record,
                Err(_) => continue,
            };
            if registration.generation != name
                || registration.socket != format!("{task}.gh-read.sock")
            {
                continue;
            }
            let index = root.join(format!("{task}.gh-read.owner"));
            if read_registration(&index).is_ok_and(|current| {
                current.generation == name && current.token == registration.token
            }) {
                unlink_registration(root, &registration)?;
                fs::remove_file(index)?;
            }
            fs::remove_file(entry.path())?;
        }
        fs::remove_file(directory.join("lease"))?;
        match fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    #[ignore]
    fn endpoint_generation_child() {
        let Some(root) = std::env::var_os("AFT_BROKER_ENDPOINT_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let generation = Generation::new(&root).unwrap();
        let (_listener, _owner) = generation.bind("000000000001").unwrap();
        fs::write(root.join("child-ready"), "ready").unwrap();
        let _ = std::io::stdin().read(&mut [0u8; 1]);
    }
    #[test]
    fn gh_shim_endpoint_restart_reclaims_dead_generation() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gh_shim_worker_endpoints::tests::endpoint_generation_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AFT_BROKER_ENDPOINT_TEST_ROOT", root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(File::create(root.path().join("child.stderr")).unwrap())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.path().join("child-ready").exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "endpoint child exited {status}: {}",
                    fs::read_to_string(root.path().join("child.stderr")).unwrap()
                );
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "endpoint child did not become ready: {}",
                    fs::read_to_string(root.path().join("child.stderr")).unwrap()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let path = root.path().join("000000000001.gh-read.sock");
        assert!(path.exists());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            path.exists(),
            "SIGKILL does not run the endpoint destructor"
        );
        let _replacement = Generation::new(root.path()).unwrap();
        assert!(
            !path.exists(),
            "restart in the same namespace must unlink dead-generation endpoints"
        );
    }
    #[test]
    fn gh_shim_endpoint_reclamation_preserves_live_replacement() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let old = Generation::new(root.path()).unwrap();
        let (_listener, owner) = old.bind("000000000001").unwrap();
        let current = Generation::new(root.path()).unwrap();
        assert!(
            owner.path().exists(),
            "a live generation must not be reclaimed"
        );
        owner.remove();
        let (_replacement, replacement_owner) = current.bind("000000000001").unwrap();
        drop(owner);
        drop(old);
        let _next = Generation::new(root.path()).unwrap();
        assert!(
            replacement_owner.path().exists(),
            "old cleanup must preserve a live replacement"
        );
        assert!(std::os::unix::net::UnixStream::connect(replacement_owner.path()).is_ok());
    }
}

#[test]
fn gh_shim_endpoint_failed_bind_preserves_existing_socket() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let generation = Generation::new(root.path()).unwrap();
    let path = root.path().join("000000000001.gh-read.sock");
    let _existing = UnixListener::bind(&path).unwrap();
    assert!(generation.bind("000000000001").is_err());
    assert!(
        path.exists(),
        "failed bind cleanup must not remove a listener it never owned"
    );
    assert!(std::os::unix::net::UnixStream::connect(path).is_ok());
}

#[test]
fn gh_shim_endpoint_identity_checks_protect_replacement_before_remove() {
    use std::os::unix::fs::PermissionsExt;
    for change in ["token", "inode"] {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let generation = Generation::new(root.path()).unwrap();
        let (_old_listener, owner) = generation.bind("000000000001").unwrap();
        let path = owner.path();
        let replacement = if change == "token" {
            let _namespace = lock_namespace(root.path()).unwrap();
            let index = root.path().join("000000000001.gh-read.owner");
            let mut record = read_registration(&index).unwrap();
            record.token = random_name().unwrap();
            write_record(&index, &record, true).unwrap();
            None
        } else {
            let _namespace = lock_namespace(root.path()).unwrap();
            fs::remove_file(&path).unwrap();
            Some(UnixListener::bind(&path).unwrap())
        };
        owner.remove();
        assert!(
            path.exists(),
            "{change} identity changed before removal: the old owner must not unlink it"
        );
        assert!(
            std::os::unix::net::UnixStream::connect(&path).is_ok(),
            "replacement must remain connectable"
        );
        drop(replacement);
    }
}
