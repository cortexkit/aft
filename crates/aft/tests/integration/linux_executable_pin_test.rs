use super::{
    call_with_permissions, configure, copy_and_warm_aft, foreground, pin_directories, quote, Daemon,
};
use serde_json::json;
use std::fs::{self, Metadata};
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) async fn check_shell_pin_protection(
    aft: &mut Daemon,
    home: &Path,
    project: &Path,
    storage: &Path,
    deploy: &Path,
    pin: &Path,
    original: &Metadata,
) {
    configure(aft, home, project, storage, "strict").await;
    let ready = project.join("task-image-alias");
    let before = pin.metadata().unwrap();
    let task = call_with_permissions(aft, json!({
        "background": true, "permissions_requested": true, "compressed": false,
        "command": format!(
            "pin={}; pin_dir={}; \
             gh --shim-version || exit 11; \
             if ls -- \"$pin_dir\" >/dev/null 2>&1; then exit 12; fi; \
             chmod u+w -- \"$pin\" 2>/dev/null || :; \
             if ( printf 'corrupted image' > \"$pin\" ) 2>/dev/null; then exit 13; fi; \
             if ( printf 'corrupted through symlink' > \"$AFT_GH_SHIM_BINARY\" ) 2>/dev/null; then exit 14; fi; \
             chmod u-w -- \"$pin\" 2>/dev/null || :; \
             printf 'wrong pin' > replacement-pin; \
             if mv -f -- replacement-pin \"$pin\" 2>/dev/null; then exit 15; fi; \
             if rm -- \"$pin\" 2>/dev/null; then exit 16; fi; \
             printf '%s' \"$AFT_GH_SHIM_BINARY\" > {}.part; \
             mv -- {}.part {}; sleep 30",
            quote(pin), quote(pin.parent().unwrap()), quote(&ready), quote(&ready), quote(&ready),
        ),
    })).await;
    let task_id = task["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("task did not start: {task:?}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.is_file() {
        if Instant::now() >= deadline {
            let status = aft
                .tool_call("bash_status", json!({"task_id": task_id}))
                .await;
            panic!("strict shell did not execute its shim and reject all pin writes: {status:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let alias = fs::read_to_string(ready).unwrap();
    let alias = Path::new(&alias);
    assert_eq!(fs::read_link(alias).unwrap(), pin);
    let after = pin.metadata().unwrap();
    assert_eq!(
        (
            before.dev(),
            before.ino(),
            before.size(),
            before.mtime(),
            before.mtime_nsec()
        ),
        (
            after.dev(),
            after.ino(),
            after.size(),
            after.mtime(),
            after.mtime_nsec()
        )
    );

    // Replace the running task's mutable alias with an executable which leaves
    // evidence if any daemon action trusts the task path again.
    let ran = project.join("wrong-image-ran");
    let decoy = project.join("wrong-image");
    fs::write(&decoy, format!("#!/bin/sh\n: > {}\nexit 91\n", quote(&ran))).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(alias).unwrap();
    symlink(decoy, alias).unwrap();

    let status = aft
        .tool_call("bash_status", json!({"task_id": task_id}))
        .await;
    assert_eq!(status["status"], "running", "{status:?}");
    let killed = aft
        .tool_call("bash_kill", json!({"task_id": task_id}))
        .await;
    assert_eq!(killed["status"], "killed", "{killed:?}");
    let next = foreground(aft, "gh --shim-version").await;
    assert_eq!(next["status"], "completed", "{next:?}");
    assert!(next["output"]
        .as_str()
        .unwrap()
        .contains("\"shim_version\""));
    assert!(!ran.exists(), "AFT executed the substituted task image");

    let installed = deploy.metadata().unwrap();
    assert_eq!(
        (
            original.ctime(),
            original.ctime_nsec(),
            original.nlink(),
            original.mode()
        ),
        (
            installed.ctime(),
            installed.ctime_nsec(),
            installed.nlink(),
            installed.mode()
        ),
        "starting shell tasks touched the deployed executable"
    );
    configure(aft, home, project, storage, "off").await;
}

#[tokio::test]
async fn pinned_executable_linux_concurrent_daemons_share_the_complete_pin() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("home");
    let project = root.join("project");
    let storage = home.join("storage");
    let first_root = root.join("first");
    let second_root = root.join("second");
    for directory in [&project, &storage, &first_root, &second_root] {
        fs::create_dir_all(directory).unwrap();
    }
    let deploy = root.join("aft-deploy");
    copy_and_warm_aft(&deploy);
    let (mut first, mut second) = tokio::join!(
        Daemon::spawn(&deploy, &first_root, &home, &storage),
        Daemon::spawn(&deploy, &second_root, &home, &storage),
    );
    configure(&mut first, &home, &project, &storage, "off").await;
    configure(&mut second, &home, &project, &storage, "off").await;
    let pins = pin_directories(&storage);
    assert_eq!(pins.len(), 2);
    let pin = fs::read_link(pins[0].join("shims/gh")).unwrap();
    assert_eq!(pin, fs::read_link(pins[1].join("shims/gh")).unwrap());
    assert_eq!(
        fs::read_dir(storage.join("run/aft-images"))
            .unwrap()
            .count(),
        1
    );
    let identity = pin.metadata().unwrap();
    assert_eq!(identity.size(), deploy.metadata().unwrap().size());
    for mut aft in [first, second] {
        let version = foreground(&mut aft, "gh --shim-version").await;
        assert_eq!(version["status"], "completed", "{version:?}");
        assert!(version["output"]
            .as_str()
            .unwrap()
            .contains("\"shim_version\""));
        aft.shutdown().await;
    }
    // Even with both owners gone, the shared image is ready for another start.
    let surviving = pin.metadata().unwrap();
    assert_eq!(
        (identity.dev(), identity.ino()),
        (surviving.dev(), surviving.ino())
    );
}
