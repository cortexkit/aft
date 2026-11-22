use super::*;

struct Roles {
    profile: SandboxProfile,
    store: PathBuf,
    checkout: PathBuf,
    sibling: PathBuf,
    broad: PathBuf,
    target: PathBuf,
    io: PathBuf,
    task_log: PathBuf,
    task_private: PathBuf,
    shim: PathBuf,
    hooks: PathBuf,
    slot: PathBuf,
    hard_dir: PathBuf,
    credential: PathBuf,
    canaries: Vec<PathBuf>,
}

impl Roles {
    fn new(base: &Path, worker: bool) -> Self {
        let mut profile = fixture(base, SandboxMode::Standard);
        let broad = base.join("Work");
        let store = broad.join("cortexkit");
        let pool = store.join("alfonso/worktrees");
        let checkout = pool.join("repo/own");
        let sibling = pool.join("repo/peer/file");
        let target = pool.join("repo/build-target");
        let slot = pool.join(".build-slots/queue.lock");
        let shim = store.join("aft/shims/gh");
        let hooks = checkout.join(".git/hooks");
        let session = store.join("aft/sessions/current");
        let io = session.join("bash-0000000000000001/io");
        let task_log = session.join("bash-0000000000000002.stdout");
        let task_private = session.join("auth-token");
        let hard_dir = checkout.join("configured-private");
        let credential = checkout.join(".netrc");
        let local_canary = aft::worker_jail_canary::prepare(&store.join("daemon-home")).unwrap();
        let account_canary = aft::worker_jail_canary::prepare(&base.join("account-home")).unwrap();
        let canaries = [local_canary, account_canary]
            .into_iter()
            .map(|dir| dir.join(aft::worker_jail_canary::CANARY_FILE))
            .collect::<Vec<_>>();
        for path in [&target, &io] {
            fs::create_dir_all(path).unwrap();
        }
        for path in [
            &sibling,
            &slot,
            &shim,
            &task_log,
            &task_private,
            &credential,
            &hard_dir.join("inside"),
            &hooks.join("pre-commit"),
            &checkout.join("own-file"),
            &broad.join("keep-read-only"),
        ] {
            seed(path);
        }
        profile.worker_jail = worker;
        profile.writable_roots = vec![checkout.clone()];
        profile.writable_files = vec![slot.clone()];
        profile.data_policy.deny = vec![store.clone(), session.clone()];
        profile.data_policy.write_allow =
            vec![checkout.clone(), target.clone(), io.clone(), slot.clone()];
        profile.data_policy.read_allow = vec![
            shim.parent().unwrap().to_path_buf(),
            slot.parent().unwrap().to_path_buf(),
            checkout.clone(),
            target.clone(),
            io.clone(),
        ];
        profile.data_policy.read_files = vec![credential.clone(), hard_dir.join("inside")];
        profile.data_policy.session_task_read_dir = Some(session);
        profile.linux_mounts = Some(LinuxMountPolicy {
            credential_masks: vec![credential.clone()],
            read_masks: vec![hard_dir.clone()],
            read_only: vec![hooks.clone()],
            deferred_read_only: vec![broad.clone()],
            ..Default::default()
        });
        if worker {
            profile.worker_gh_fence = Some(WorkerGhFence::Canary {
                paths: canaries.clone(),
            });
        } else {
            profile.data_policy.read_allow.push(pool);
            profile
                .data_policy
                .read_allow
                .extend(canaries.iter().map(|p| p.parent().unwrap().to_path_buf()));
        }
        Self {
            profile,
            store,
            checkout,
            sibling,
            broad,
            target,
            io,
            task_log,
            task_private,
            shim,
            hooks,
            slot,
            hard_dir,
            credential,
            canaries,
        }
    }
}

#[test]
fn bwrap_roles_snapshot_preserves_nested_controls_and_only_exact_exceptions() {
    let root = tempfile::tempdir().unwrap();
    let f = Roles::new(root.path(), true);
    let snapshot = Snapshot::take(&f.profile).unwrap();
    let mounts = snapshot.mounts;
    for (path, mode) in [
        (&f.broad, Mount::DeferredReadOnly),
        (&f.store, Mount::PrivateDir),
        (&f.checkout, Mount::WriteException),
        (&f.credential, Mount::EmptyFile),
        (&f.hard_dir, Mount::EmptyDir),
        (&f.task_log, Mount::ReadException),
        (&f.io, Mount::WriteException),
        (&f.target, Mount::WriteException),
        (&f.slot, Mount::WriteException),
        (&f.hooks, Mount::ReadOnly),
    ] {
        assert_eq!(mounts.get(path), Some(&mode), "{}", path.display());
    }
    assert_eq!(mounts[f.shim.parent().unwrap()], Mount::ReadException);
    for path in [&f.sibling, &f.task_private, &f.hard_dir.join("inside")] {
        assert!(path.exists());
        assert!(!mounts.contains_key(path), "{}", path.display());
    }
    for marker in &f.canaries {
        assert_eq!(mounts[marker.parent().unwrap()], Mount::NoSearchDir);
    }
}

const ROLE_ORACLE: &str = r#"
import errno, os, sys
from pathlib import Path
mode, worker, parent, base, *markers = sys.argv[1:]
base = Path(base)
store = base / 'Work/cortexkit'
pool = store / 'alfonso/worktrees'
own = pool / 'repo/own'
paths = [own / '.netrc', own / 'configured-private/inside']
seed = 's5-disposable-secret'
def read(path):
    try:
        return path.read_text()
    except OSError:
        return None

def write(path):
    try:
        path.write_text('written')
        return True
    except OSError:
        return False

for path in paths:
    assert (read(path) == seed) == (mode == 'off'), path
assert read(own / 'own-file') == seed
peer = pool / 'repo/peer/file'
assert (read(peer) == seed) == (mode == 'off' or worker == 'head')
session = store / 'aft/sessions/current'
assert read(session / 'bash-0000000000000002.stdout') == seed
assert (read(session / 'auth-token') == seed) == (mode == 'off')
for marker in map(Path, markers):
    if mode == 'off' or worker == 'head':
        assert marker.read_text().startswith('AFT worker-jail canary.')
    else:
        try:
            marker.stat()
        except OSError as error:
            assert error.errno == errno.EACCES, error
        else:
            raise AssertionError('canary was not denied: ' + str(marker))
if mode != 'off':
    for path in [store / 'aft/shims/gh', own / '.git/hooks/pre-commit', base / 'Work/keep-read-only']:
        assert read(path) == seed, path
        assert not write(path), path
    for path in [own / 'output', pool / 'repo/build-target/output', session / 'bash-0000000000000001/io/output', pool / '.build-slots/queue.lock']:
        assert write(path), path
try:
    os.kill(int(parent), 0)
except OSError as error:
    assert error.errno == errno.EPERM, error
    print('outside_signal=denied')
else:
    print('outside_signal=allowed')
"#;

#[test]
fn bwrap_roles_live_standard_worker_and_head_observe_role_and_signal_scope() {
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    for worker in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let f = Roles::new(root.path(), worker);
        let run = |profile: &SandboxProfile| {
            launch(&launcher, profile)
                .current_dir(&f.checkout)
                .args(["--", "/usr/bin/python3", "-I", "-B", "-c", ROLE_ORACLE])
                .arg(profile.mode.to_string())
                .arg(if worker { "worker" } else { "head" })
                .arg(std::process::id().to_string())
                .arg(root.path())
                .args(&f.canaries)
                .output()
                .unwrap()
        };
        let mut off = f.profile.clone();
        off.mode = SandboxMode::Off;
        off.linux_mounts = None;
        off.worker_gh_fence = None;
        let control = run(&off);
        assert_eq!(
            String::from_utf8_lossy(&control.stdout).trim(),
            "outside_signal=allowed"
        );
        passed(control);
        let output = run(&f.profile);
        let signal = String::from_utf8(output.stdout.clone()).unwrap();
        let report = passed(output);
        assert!(
            report.contains("profile=standard backend=bwrap"),
            "{report}"
        );
        if worker {
            assert_signal_report(&report, signal.trim());
            assert!(
                !report.contains("operator gh fence: unenforced"),
                "{report}"
            );
        } else {
            assert!(!report.contains("signal_scope="), "{report}");
            assert_eq!(signal.trim(), "outside_signal=allowed");
        }
        for path in [
            &f.shim,
            &f.hooks.join("pre-commit"),
            &f.broad.join("keep-read-only"),
            &f.credential,
        ] {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                SEED,
                "{}",
                path.display()
            );
        }
        for path in [&f.target.join("output"), &f.io.join("output"), &f.slot] {
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                "written",
                "{}",
                path.display()
            );
        }
    }
}

pub(super) fn assert_signal_report(report: &str, signal: &str) {
    if report.contains("signal_scope=same_command") {
        assert_eq!(signal, "outside_signal=denied", "{report}");
    } else {
        assert!(report.contains("signal_scope=not_enforced"), "{report}");
        assert_eq!(signal, "outside_signal=allowed", "{report}");
    }
}

#[test]
fn bwrap_roles_live_strict_worker_canary_is_denied_in_both_homes() {
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let f = Roles::new(root.path(), true);
    let mut profile = f.profile;
    profile.mode = SandboxMode::Strict;
    profile.read_allow = ["/usr", "/lib", "/lib64", "/etc"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    profile.read_deny = f
        .canaries
        .iter()
        .map(|p| p.parent().unwrap().to_path_buf())
        .collect();
    profile.write_deny.clone_from(&profile.read_deny);
    let mut control = profile.clone();
    control.mode = SandboxMode::Off;
    control.linux_mounts = None;
    control.worker_gh_fence = None;
    let run = |profile: &SandboxProfile| {
        launch(&launcher, profile)
            .current_dir(&f.checkout)
            .args([
                "--",
                "/usr/bin/python3",
                "-I",
                "-B",
                "-c",
                r#"
import errno, sys
from pathlib import Path
expect_denied = sys.argv[1] == 'strict'
for path in map(Path, sys.argv[2:]):
    try:
        value = path.read_text()
    except OSError as error:
        assert expect_denied and error.errno == errno.EACCES, error
    else:
        assert not expect_denied and value.startswith('AFT worker-jail canary.')
"#,
            ])
            .arg(profile.mode.to_string())
            .args(&f.canaries)
            .output()
            .unwrap()
    };
    passed(run(&control));
    let report = passed(run(&profile));
    assert!(
        report.contains("profile=strict backend=bwrap+landlock"),
        "{report}"
    );
    assert!(
        !report.contains("operator gh fence: unenforced"),
        "{report}"
    );
}
