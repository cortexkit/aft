use super::*;
use std::fs;
use std::path::PathBuf;
use std::process::Output;

#[path = "failure_tests.rs"]
mod failure_tests;
#[path = "roles_tests.rs"]
mod roles_tests;
#[path = "task_executable_tests.rs"]
mod task_executable_tests;
#[path = "worker_read_broker_tests.rs"]
mod worker_read_broker_tests;

const SEED: &str = "s5-disposable-secret";

fn seed(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, SEED).unwrap();
}

fn fixture(base: &Path, mode: SandboxMode) -> SandboxProfile {
    let project = base.join("checkout");
    let temp = base.join("temp");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&temp).unwrap();
    let mut profile = SandboxProfile::build(
        vec![project],
        vec![],
        vec![],
        vec![PathBuf::from("/")],
        vec![],
        vec![],
        vec![],
        temp,
    )
    .unwrap();
    profile.mode = mode;
    profile
}

#[test]
fn bwrap_core_snapshot_orders_guards_and_nested_masks_and_names_missing_targets() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    let git = home.join(".config/git");
    let credential = git.join("credentials");
    seed(&credential);
    let aliases = home.join("aliases");
    fs::create_dir(&aliases).unwrap();
    let alias = aliases.join("git-credentials");
    std::os::unix::fs::symlink(&credential, &alias).unwrap();
    let aws = home.join(".aws");
    let aws_child = aws.join("nested/secret");
    seed(&aws_child);
    let cargo = home.join(".cargo");
    let late = cargo.join("Credentials.TOML");
    let sibling = cargo.join("config.toml");
    seed(&sibling);
    let absent = home.join(".netrc");
    let control = home.join(".bashrc");
    let keychains = home.join("Library/Keychains");
    let policy = LinuxMountPolicy {
        credential_masks: vec![
            credential.clone(),
            alias,
            aws.clone(),
            absent.clone(),
            keychains,
        ],
        read_only: vec![git.clone(), aws_child.clone(), control.clone()],
        cargo_homes: vec![cargo.clone()],
        ..Default::default()
    };
    seed(&late);
    let snapshot = Snapshot::take_policy(&policy).unwrap();
    assert_eq!(snapshot.mounts[&home.join(".config")], Mount::Guard);
    assert_eq!(snapshot.mounts[&git], Mount::ReadOnly);
    assert_eq!(snapshot.mounts[&credential], Mount::EmptyFile);
    assert_eq!(snapshot.mounts[&aliases], Mount::Guard);
    assert_eq!(snapshot.mounts[&aws], Mount::EmptyDir);
    assert_eq!(snapshot.mounts[&cargo], Mount::Guard);
    assert_eq!(snapshot.mounts[&late], Mount::EmptyFile);
    for unrelated in [aws_child, sibling, absent.clone(), control.clone()] {
        assert!(!snapshot.mounts.contains_key(&unrelated), "{unrelated:?}");
    }
    assert_eq!(
        snapshot.unenforced,
        [
            format!(
                "credential_filename_prefix_future_entries_unenforced_on_bwrap:{}*",
                cargo.join("credentials").display()
            ),
            format!("credential_path_absent_on_bwrap:{}", absent.display()),
            format!(
                "standard_control_target_absent_on_bwrap:{}",
                control.display()
            ),
        ]
    );
    assert!(!absent.exists() && !control.exists());
}

#[test]
fn bwrap_core_strict_fallback_restores_checkout_relative_kube_mandatory_deny() {
    let root = tempfile::tempdir().unwrap();
    let mut profile = fixture(root.path(), SandboxMode::Strict);
    let kube = root.path().join("checkout/kube/dev");
    seed(&kube);
    profile.linux_mounts = Some(LinuxMountPolicy {
        credential_masks: vec![kube.clone()],
        fallback_write_deny: vec![kube.clone()],
        ..Default::default()
    });
    let snapshot = Snapshot::take(&profile, &std::env::current_exe().unwrap()).unwrap();
    let inner = prepare_inner(profile.clone(), &snapshot);
    assert_eq!(snapshot.mounts[&kube], Mount::EmptyFile);
    assert!(inner.profile.write_deny.is_empty());
    let fallback = fallback_profile(profile).unwrap();
    let error = landlock_backend::apply(&fallback).unwrap_err();
    assert!(
        error.contains("overlaps mandatory deny") && error.contains("kube/dev"),
        "{error}"
    );
}

fn enforcing_launcher() -> Option<PathBuf> {
    let Some(path) = std::env::var_os("AFT_TEST_BWRAP_AFT") else {
        eprintln!(
            "skipped active bubblewrap oracle: AFT_TEST_BWRAP_AFT does not name a built aft binary"
        );
        return None;
    };
    assert_eq!(
        probe().expect("trusted bubblewrap namespace probe"),
        None,
        "enforcing CI requires active bubblewrap"
    );
    Some(
        PathBuf::from(path)
            .canonicalize()
            .expect("AFT_TEST_BWRAP_AFT must name a built aft binary"),
    )
}

fn launch(launcher: &Path, profile: &SandboxProfile) -> Command {
    let mut command = Command::new(launcher);
    command
        .arg("sandbox-launch")
        .arg("--profile-json")
        .arg(serde_json::to_string(profile).unwrap());
    command
}

fn passed(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn bwrap_core_active_levels_keep_nested_masks_read_only_controls_and_ancestor_inodes() {
    use std::os::unix::fs::MetadataExt;
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    for mode in [
        SandboxMode::Credentials,
        SandboxMode::Standard,
        SandboxMode::Strict,
    ] {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        let checkout = base.join("checkout");
        let home = base.join("home");
        let git = home.join(".config/git");
        let credential = git.join("credentials");
        let config = git.join("config");
        let enclosing = checkout.join("enclosing");
        let enclosed_credential = enclosing.join("token");
        let masked_dir = checkout.join("masked-directory");
        let masked_child = masked_dir.join("seed");
        let absent_credential = home.join(".netrc");
        let absent_control = checkout.join(".cortexkit/aft.jsonc");
        let hooks = checkout.join(".git/hooks");
        let hook = hooks.join("test-hook");
        // Model KUBECONFIG=kube/dev resolved by the engine against the checkout.
        let kube = checkout.join("kube/dev");
        let read_mask = checkout.join("read-mask");
        for path in [
            &credential,
            &config,
            &enclosed_credential,
            &masked_child,
            &hook,
            &kube,
            &read_mask,
        ] {
            seed(path);
        }
        let original = fs::metadata(&enclosed_credential).unwrap();
        let mut profile = fixture(base, mode);
        let mut mounts = LinuxMountPolicy {
            credential_masks: vec![
                credential.clone(),
                enclosed_credential.clone(),
                masked_dir,
                absent_credential.clone(),
                kube.clone(),
            ],
            ..Default::default()
        };
        if mode != SandboxMode::Credentials {
            mounts.read_masks.push(read_mask.clone());
            mounts
                .read_only
                .extend([git.clone(), hooks.clone(), absent_control.clone()]);
        }
        if mode == SandboxMode::Strict {
            mounts.fallback_write_deny.push(kube.clone());
            profile
                .write_deny_nested
                .extend([checkout.join(".git"), hooks.clone()]);
        }
        profile.linux_mounts = Some(mounts);
        let mut off = profile.clone();
        off.mode = SandboxMode::Off;
        off.linux_mounts = None;
        let run = |profile: &SandboxProfile| {
            launch(&launcher, profile)
                .env("KUBECONFIG", "kube/dev")
                .current_dir(&checkout)
                .args(["--", "/usr/bin/python3", "-I", "-c", ORACLE])
                .arg(profile.mode.to_string())
                .arg(&home)
                .arg(fs::read_link("/proc/self/ns/pid").unwrap())
                .output()
                .unwrap()
        };
        passed(run(&off));
        let output = run(&profile);
        if output
            .stderr
            .windows(b"ancestor rename unexpectedly succeeded".len())
            .any(|window| window == b"ancestor rename unexpectedly succeeded")
        {
            panic!(
                "kernel {} permitted ancestor rename under {mode}",
                String::from_utf8_lossy(
                    &Command::new("/usr/bin/uname")
                        .arg("-r")
                        .output()
                        .unwrap()
                        .stdout
                )
                .trim()
            );
        }
        let stderr = passed(output);
        assert!(stderr.contains("backend=bwrap"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "credential_path_absent_on_bwrap:{}",
                absent_credential.display()
            )),
            "{stderr}"
        );
        if mode != SandboxMode::Credentials {
            assert!(
                stderr.contains(&format!(
                    "standard_control_target_absent_on_bwrap:{}",
                    absent_control.display()
                )),
                "{stderr}"
            );
        }
        assert!(
            !stderr.contains(&format!("write:{}", hooks.display())),
            "{stderr}"
        );
        if mode == SandboxMode::Strict {
            let mut invalid = profile.clone();
            invalid.write_deny.push(kube.clone());
            let failure = run(&invalid);
            assert_eq!(
                failure.status.code(),
                Some(super::super::SANDBOX_UNAVAILABLE_EXIT_CODE)
            );
            let stderr = String::from_utf8(failure.stderr).unwrap();
            assert!(
                stderr.contains("overlaps mandatory deny") && stderr.contains("kube/dev"),
                "{stderr}"
            );
        }
        for path in [
            &credential,
            &config,
            &enclosed_credential,
            &masked_child,
            &hook,
            &kube,
            &read_mask,
        ] {
            assert_eq!(fs::read_to_string(path).unwrap(), SEED, "{path:?}");
        }
        assert!(!absent_credential.exists() && !absent_control.exists());
        let afterward = fs::metadata(&enclosed_credential).unwrap();
        assert_eq!(
            (original.dev(), original.ino()),
            (afterward.dev(), afterward.ino())
        );
        assert_eq!(
            fs::read_to_string(enclosing.join("writable")).unwrap(),
            "writable"
        );
        assert_eq!(
            fs::read_to_string(checkout.join(".git/writable")).unwrap(),
            "writable"
        );
    }
}

const ORACLE: &str = r#"
import os, sys
from pathlib import Path
mode, home, pidns = sys.argv[1:]
assert os.readlink('/proc/self/ns/pid') == pidns
home = Path(home)
seed = 's5-disposable-secret'

def read(path):
    try:
        return path.read_text()
    except OSError:
        return None

def cannot_write(path):
    try:
        path.write_text('modified')
    except OSError:
        return
    raise AssertionError(f'modified {path}')

for path in [home / '.config/git/credentials', Path('enclosing/token'), Path(os.environ['KUBECONFIG']), Path('masked-directory/seed')]:
    if mode == 'off':
        assert read(path) == seed, path
    else:
        assert read(path) != seed, path
        cannot_write(path)
if mode != 'off':
    cannot_write(Path('masked-directory/new'))
assert read(home / '.config/git/config') == seed
assert read(Path('.git/hooks/test-hook')) == seed
if mode in ('standard', 'strict'):
    cannot_write(home / '.config/git/config')
    cannot_write(Path('.git/hooks/test-hook'))
    assert read(Path('read-mask')) != seed
    cannot_write(Path('read-mask'))
else:
    assert read(Path('read-mask')) == seed
if mode == 'off':
    os.rename('enclosing', 'moved')
    os.rename('moved', 'enclosing')
else:
    try:
        os.rename('enclosing', 'moved')
    except OSError:
        pass
    else:
        raise AssertionError('ancestor rename unexpectedly succeeded')
if mode != 'off':
    for path in [Path('enclosing/writable'), Path('.git/writable')]:
        assert not path.exists(), path
        path.write_text('writable')
"#;

#[cfg(feature = "test-timing-hooks")]
#[test]
fn bwrap_core_active_cargo_snapshot_includes_entries_written_at_start_gate() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let base = root.path();
    let cargo = base.join("home/.cargo");
    let early = cargo.join("credentials-seeded.toml");
    let late = cargo.join("credentials-created-at-gate.toml");
    let sibling = cargo.join("config.toml");
    seed(&early);
    seed(&sibling);
    let mut profile = fixture(base, SandboxMode::Credentials);
    profile.linux_mounts = Some(LinuxMountPolicy {
        credential_masks: vec![early.clone()],
        cargo_homes: vec![cargo.clone()],
        ..Default::default()
    });
    let script = r#"
import sys
from pathlib import Path
mode, early, late, sibling = sys.argv[1:]
for name in [early, late, sibling]:
    observed = Path(name).read_text()
    assert (observed == 's5-disposable-secret') == (mode == 'off' or name == sibling), name
"#;
    let command = |profile: &SandboxProfile, gate: Option<&Path>| {
        let mut cmd = launch(&launcher, profile);
        if let Some(gate) = gate {
            cmd.arg("--test-start-gate").arg(gate);
        }
        cmd.args(["--", "/usr/bin/python3", "-I", "-c", script])
            .arg(profile.mode.to_string())
            .args([&early, &late, &sibling]);
        cmd
    };
    let gate = base.join("go");
    let mut child = command(&profile, Some(&gate))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !gate.with_extension("ready").exists() {
        if child.try_wait().unwrap().is_some() {
            passed(child.wait_with_output().unwrap());
            panic!("launcher exited before snapshot gate");
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("launcher did not reach snapshot gate");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    seed(&late);
    let mut off = profile;
    off.mode = SandboxMode::Off;
    off.linux_mounts = None;
    let control = command(&off, None).output().unwrap();
    fs::write(&gate, b"go").unwrap();
    passed(control);
    let stderr = passed(child.wait_with_output().unwrap());
    assert!(stderr.contains("backend=bwrap"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "credential_filename_prefix_future_entries_unenforced_on_bwrap:{}*",
            cargo.join("credentials").display()
        )),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(early).unwrap(), SEED);
    assert_eq!(fs::read_to_string(late).unwrap(), SEED);
}

#[test]
fn bwrap_core_standard_reports_missing_controls_but_not_enforced_store_masks() {
    let root = tempfile::tempdir().unwrap();
    let mut profile = fixture(root.path(), SandboxMode::Standard);
    let hooks = root.path().join("checkout/.git/hooks");
    let elsewhere = root.path().join("private");
    fs::create_dir(&elsewhere).unwrap();
    seed(&hooks.join("control"));
    profile.data_policy.deny.push(elsewhere.clone());
    profile.data_policy.write_allow = profile.writable_roots.clone();
    let absent = root.path().join("absent");
    profile.linux_mounts = Some(LinuxMountPolicy {
        read_only: vec![hooks.clone()],
        deferred_read_only: vec![root.path().to_path_buf(), absent.clone()],
        ..Default::default()
    });
    let snapshot = Snapshot::take(&profile, &std::env::current_exe().unwrap()).unwrap();
    assert_eq!(snapshot.mounts[&hooks], Mount::ReadOnly);
    assert_eq!(snapshot.mounts[root.path()], Mount::DeferredReadOnly);
    assert_eq!(snapshot.mounts[&elsewhere], Mount::PrivateDir);
    let inner = prepare_inner(profile, &snapshot);
    assert_eq!(
        inner.unenforced,
        [format!(
            "standard_control_target_absent_on_bwrap:{}",
            absent.display()
        )]
    );
}
