use super::*;
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::time::Duration;

struct Broker {
    profile: SandboxProfile,
    root: PathBuf,
    socket: PathBuf,
    other_socket: PathBuf,
    record: PathBuf,
    credential: PathBuf,
    listener: UnixListener,
    _other_listener: UnixListener,
}

impl Broker {
    fn new(base: &Path) -> Self {
        let mut profile = fixture(base, SandboxMode::Strict);
        let root = base.join("aft-w");
        let temp = root.join("012345abcdef");
        fs::create_dir_all(&temp).unwrap();
        let socket = root.join("012345abcdef.gh-read.sock");
        let other_socket = root.join("fedcba543210.gh-read.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let other_listener = UnixListener::bind(&other_socket).unwrap();
        let record = socket.with_extension("owner");
        let credential = base.join("home/.netrc");
        for path in [&record, &credential, &temp.join(".gh-read-capability")] {
            seed(path);
        }
        fs::create_dir(temp.join(".gh-shim")).unwrap();
        profile.worker_jail = true;
        profile.temp_dir = temp.clone();
        profile.read_allow = vec![temp.clone(), PathBuf::from("/dev/null")];
        profile.data_policy.deny = vec![root.clone()];
        profile.data_policy.read_allow = vec![temp.clone()];
        profile.data_policy.write_allow = vec![temp];
        profile.linux_mounts = Some(LinuxMountPolicy {
            credential_masks: vec![credential.clone()],
            worker_read_broker_socket: Some(socket.clone()),
            ..Default::default()
        });
        Self {
            profile,
            root,
            socket,
            other_socket,
            record,
            credential,
            listener,
            _other_listener: other_listener,
        }
    }

    fn snapshot(&self) -> Result<Snapshot, String> {
        Snapshot::take(&self.profile, &std::env::current_exe().unwrap())
    }
}

#[test]
fn bwrap_roles_snapshot_restores_only_the_provisioned_worker_read_socket() {
    let base = tempfile::tempdir().unwrap();
    let mut f = Broker::new(base.path());
    let snapshot = f.snapshot().unwrap();
    assert_eq!(snapshot.mounts[&f.root], Mount::PrivateDir);
    assert_eq!(snapshot.mounts[&f.profile.temp_dir], Mount::WriteException);
    assert_eq!(snapshot.mounts[&f.socket], Mount::ReadException);
    assert_eq!(snapshot.mounts[&f.credential], Mount::EmptyFile);
    assert!(!snapshot.mounts.contains_key(&f.other_socket));
    assert!(!snapshot.mounts.contains_key(&f.record));
    let inner = prepare_inner(f.profile.clone(), &snapshot);
    assert_eq!(inner.profile.read_allow, f.profile.read_allow);

    f.profile
        .linux_mounts
        .as_mut()
        .unwrap()
        .worker_read_broker_socket = None;
    assert!(!f.snapshot().unwrap().mounts.contains_key(&f.socket));
}

#[test]
fn bwrap_roles_snapshot_rejects_wrong_worker_read_socket_and_keeps_hard_masks() {
    let base = tempfile::tempdir().unwrap();
    let mut f = Broker::new(base.path());
    f.profile
        .linux_mounts
        .as_mut()
        .unwrap()
        .worker_read_broker_socket = Some(f.other_socket.clone());
    assert!(f.snapshot().is_err());
    f.profile
        .linux_mounts
        .as_mut()
        .unwrap()
        .worker_read_broker_socket = Some(f.socket.clone());

    for mask in [&f.root, &f.socket] {
        f.profile.linux_mounts.as_mut().unwrap().read_masks = vec![mask.clone()];
        assert_ne!(
            f.snapshot().unwrap().mounts.get(&f.socket),
            Some(&Mount::ReadException)
        );
    }
    f.profile.linux_mounts.as_mut().unwrap().read_masks.clear();
    f.profile.socket_deny.push(f.socket.clone());
    assert!(!f.snapshot().unwrap().mounts.contains_key(&f.socket));
    f.profile.socket_deny.clear();

    fs::remove_file(&f.socket).unwrap();
    std::os::unix::fs::symlink(&f.other_socket, &f.socket).unwrap();
    assert!(f.snapshot().is_err());
    fs::remove_file(&f.socket).unwrap();
    assert!(!f.snapshot().unwrap().mounts.contains_key(&f.socket));
    seed(&f.socket);
    assert!(f.snapshot().is_err());
}

const CONNECT_TO_BROKER: &str = r#"
import os, socket, sys

endpoint, other_endpoint, record, credential, temp = sys.argv[1:]
assert os.stat(credential).st_size == 0, 'credential mask is missing'
assert not os.path.lexists(other_endpoint), 'another task socket is visible'
assert not os.path.lexists(record), 'broker ownership record is visible'
with open(os.path.join(temp, '.gh-read-capability')) as capability:
    assert capability.read() == 's5-disposable-secret'
with open(os.path.join(temp, '.gh-shim', 'lookup'), 'w') as state:
    state.write('ready')
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as broker:
    broker.connect(endpoint)
    broker.sendall(b'worker connected')
"#;

#[test]
fn bwrap_roles_live_strict_connects_to_the_exact_worker_read_socket() {
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    let base = tempfile::tempdir().unwrap();
    let mut f = Broker::new(base.path());
    f.profile.read_allow.extend(
        ["/usr", "/bin", "/lib", "/lib64", "/etc"]
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| path.exists()),
    );
    let report = passed(
        launch(&launcher, &f.profile)
            .current_dir(&f.profile.writable_roots[0])
            .args(["--", "/usr/bin/python3", "-I", "-c", CONNECT_TO_BROKER])
            .args([
                &f.socket,
                &f.other_socket,
                &f.record,
                &f.credential,
                &f.profile.temp_dir,
            ])
            .output()
            .unwrap(),
    );
    assert!(
        report.contains("profile=strict backend=bwrap+landlock"),
        "{report}"
    );
    f.listener.set_nonblocking(true).unwrap();
    let (mut server, _) = f.listener.accept().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = String::new();
    server.read_to_string(&mut request).unwrap();
    assert_eq!(request, "worker connected");
}
