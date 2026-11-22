use super::*;
use std::io::Read;
use std::os::unix::fs::{symlink, PermissionsExt};

struct TaskImage {
    profile: SandboxProfile,
    io: PathBuf,
    alias: PathBuf,
    shim: PathBuf,
    pin: PathBuf,
    credential: PathBuf,
}

impl TaskImage {
    fn new(base: &Path) -> Self {
        let session = base.join("private/session");
        let io = session.join("bash-0000000000000001/io");
        let alias = io.join("startup-image/aft");
        let shim = io.join("startup-image/shims/gh");
        let cache = base.join("private/run/aft-images");
        let pin = cache.join("current/aft");
        let credential = base.join("home/.netrc");
        seed(&pin);
        fs::set_permissions(&pin, fs::Permissions::from_mode(0o755)).unwrap();
        seed(&pin.with_file_name("other-image"));
        seed(&io.join("private-payload"));
        seed(&credential);
        fs::create_dir_all(shim.parent().unwrap()).unwrap();
        symlink(&pin, &alias).unwrap();
        symlink(&alias, &shim).unwrap();

        let mut profile = fixture(base, SandboxMode::Strict);
        profile.worker_jail = true;
        profile.data_policy.deny = vec![session, cache];
        profile.data_policy.read_allow = vec![shim.parent().unwrap().to_path_buf(), pin.clone()];
        profile
            .read_allow
            .clone_from(&profile.data_policy.read_allow);
        profile.linux_mounts = Some(LinuxMountPolicy {
            credential_masks: vec![credential.clone()],
            task_executable_alias: Some(alias.clone()),
            ..Default::default()
        });
        Self {
            profile,
            io,
            alias,
            shim,
            pin,
            credential,
        }
    }

    fn snapshot(&self) -> Result<Snapshot, String> {
        Snapshot::take(&self.profile, &self.pin)
    }
}

#[test]
fn bwrap_roles_snapshot_restores_exact_task_executable_alias() {
    let root = tempfile::tempdir().unwrap();
    let mut f = TaskImage::new(root.path());
    let snapshot = f.snapshot().unwrap();
    assert_eq!(
        snapshot.task_executable_bind,
        Some((f.pin.clone(), f.alias.clone()))
    );
    assert_eq!(snapshot.mounts[&f.pin], Mount::ReadException);
    assert_eq!(
        snapshot.mounts[f.shim.parent().unwrap()],
        Mount::ReadException
    );
    assert_eq!(snapshot.mounts[&f.credential], Mount::EmptyFile);
    for path in [
        &f.io,
        &f.io.join("private-payload"),
        &f.pin.with_file_name("other-image"),
    ] {
        assert!(!snapshot.mounts.contains_key(path), "{path:?}");
    }
    let inner = prepare_inner(f.profile.clone(), &snapshot);
    assert_eq!(inner.profile.read_allow, f.profile.read_allow);

    let (mut args, _empty_files) = append_mounts(&mut Command::new(BWRAP), snapshot).unwrap();
    let mut bytes = Vec::new();
    args.read_to_end(&mut bytes).unwrap();
    let args: Vec<_> = bytes.split(|byte| *byte == 0).collect();
    assert!(args.windows(3).any(|args| args
        == [
            b"--ro-bind",
            f.pin.as_os_str().as_bytes(),
            f.alias.as_os_str().as_bytes(),
        ]));

    // Standard mounts the task I/O directory, so the alias is already visible.
    f.profile.mode = SandboxMode::Standard;
    f.profile.data_policy.write_allow.push(f.io.clone());
    let snapshot = f.snapshot().unwrap();
    assert_eq!(snapshot.mounts[&f.io], Mount::WriteException);
    assert!(snapshot.task_executable_bind.is_none());
}

#[test]
fn bwrap_roles_snapshot_rejects_retargeted_or_masked_task_executable() {
    let root = tempfile::tempdir().unwrap();
    let mut f = TaskImage::new(root.path());
    fs::remove_file(&f.alias).unwrap();
    symlink(&f.credential, &f.alias).unwrap();
    assert!(f.snapshot().is_err());

    fs::remove_file(&f.alias).unwrap();
    symlink(&f.pin, &f.alias).unwrap();
    f.profile.read_allow.retain(|path| path != &f.pin);
    assert!(f.snapshot().is_err());
    f.profile.read_allow.push(f.pin.clone());
    f.profile
        .linux_mounts
        .as_mut()
        .unwrap()
        .credential_masks
        .push(f.pin.clone());
    assert!(f.snapshot().is_err());
}

#[test]
fn bwrap_roles_snapshot_keeps_task_executable_hidden_under_a_hard_mask() {
    let root = tempfile::tempdir().unwrap();
    let mut f = TaskImage::new(root.path());
    f.profile
        .linux_mounts
        .as_mut()
        .unwrap()
        .credential_masks
        .push(f.alias.parent().unwrap().to_path_buf());
    let snapshot = f.snapshot().unwrap();
    assert!(snapshot.task_executable_bind.is_none());
    assert!(!snapshot.mounts.contains_key(f.shim.parent().unwrap()));
}

#[test]
fn bwrap_roles_live_strict_runs_governed_gh_through_task_executable_alias() {
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let mut f = TaskImage::new(root.path());
    fs::copy(launcher, &f.pin).unwrap();
    f.profile.read_allow.extend(
        ["/usr", "/bin", "/lib", "/lib64", "/etc"]
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.exists()),
    );
    let checkout = &f.profile.writable_roots[0];
    let fallback = checkout.join("native-bin");
    let fallback_gh = fallback.join("gh");
    fs::create_dir(&fallback).unwrap();
    fs::write(
        &fallback_gh,
        "#!/bin/sh\nprintf 'ran' > native-gh.log\nexit 97\n",
    )
    .unwrap();
    fs::set_permissions(&fallback_gh, fs::Permissions::from_mode(0o755)).unwrap();
    f.profile
        .read_allow
        .extend([checkout.clone(), f.profile.temp_dir.clone()]);
    let output = launch(&f.pin, &f.profile)
        .current_dir(checkout)
        .env(
            "PATH",
            std::env::join_paths([f.shim.parent().unwrap(), &fallback]).unwrap(),
        )
        .args([
            "--",
            "/bin/sh",
            "-ec",
            r#"gh --shim-version; "$1" gh-shim --shim-version"#,
            "sh",
        ])
        .arg(&f.alias)
        .output()
        .unwrap();
    assert!(
        !checkout.join("native-gh.log").exists(),
        "fell through to native gh"
    );
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let report = passed(output);
    assert!(
        report.contains("profile=strict backend=bwrap+landlock"),
        "{report}"
    );
    assert_eq!(stdout.matches("\"shim_version\"").count(), 2, "{stdout}");
}
