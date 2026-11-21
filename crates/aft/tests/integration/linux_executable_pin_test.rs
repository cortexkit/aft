use super::{
    call_with_permissions, configure, configure_with_shim_binary, copy_and_warm_aft, foreground,
    pin_directories, quote, Daemon,
};
use serde_json::json;
use std::fs::{self, Metadata};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
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
             printf 'PATH=%s SHIMS=%s\\n' \"$PATH\" \"$AFT_GH_SHIMS_DIR\"; \
             alias_path=${{AFT_GH_SHIMS_DIR%/shims}}/aft; \
             \"$pin\" gh-shim --shim-version || exit 10; \
             \"$alias_path\" gh-shim --shim-version || exit 11; \
             if ls -- \"$pin_dir\" >/dev/null 2>&1; then exit 12; fi; \
             chmod u+w -- \"$pin\" 2>/dev/null || :; \
             if ( printf 'corrupted image' > \"$pin\" ) 2>/dev/null; then exit 13; fi; \
             if ( printf 'corrupted through symlink' > \"$alias_path\" ) 2>/dev/null; then exit 14; fi; \
             chmod u-w -- \"$pin\" 2>/dev/null || :; \
             printf 'wrong pin' > replacement-pin; \
             if mv -f -- replacement-pin \"$pin\" 2>/dev/null; then exit 15; fi; \
             if rm -- \"$pin\" 2>/dev/null; then exit 16; fi; \
             printf '%s' \"$alias_path\" > {}.part; \
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
            panic!(
                "strict shell did not execute its shim and reject pin writes (exit {}): {}",
                status["exit_code"], status["output_preview"]
            );
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

#[tokio::test]
async fn pinned_executable_linux_shell_keeps_the_gh_shim_refusal() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("home");
    let storage = home.join("storage");
    let project = root.join("project");
    let module = root.join("module");
    let bin = root.join("bin");
    for directory in [&storage, &project, &module, &bin] {
        fs::create_dir_all(directory).unwrap();
    }
    let real_gh = bin.join("gh");
    fs::write(&real_gh, "#!/bin/sh\necho real gh\n").unwrap();
    fs::set_permissions(&real_gh, fs::Permissions::from_mode(0o700)).unwrap();
    let deploy = root.join("aft-deploy");
    copy_and_warm_aft(&deploy);
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let mut aft =
        Daemon::spawn_with_env(&deploy, &module, &home, &storage, &[("PATH", &path)]).await;
    configure_with_shim_binary(&mut aft, &home, &project, &storage, "off", Some(&real_gh)).await;

    let result = foreground(&mut aft, "gh --shim-version").await;
    aft.shutdown().await;
    assert_eq!(result["exit_code"], 86, "{result:?}");
    let output = result["output"].as_str().unwrap();
    assert!(
        output.contains("AFT could not verify its GitHub shim"),
        "{result:?}"
    );
    assert!(!output.contains("real gh"), "{result:?}");
}

#[tokio::test]
async fn pinned_executable_linux_peer_replay_keeps_a_starting_shell_shim() {
    use std::ffi::OsStr;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("home");
    let storage = home.join("storage");
    let project = root.join("project");
    let first_root = root.join("first");
    let peer_root = root.join("peer");
    let bin = root.join("bin");
    for directory in [&storage, &project, &first_root, &peer_root, &bin] {
        fs::create_dir_all(directory).unwrap();
    }
    let real_gh = bin.join("gh");
    fs::write(&real_gh, "#!/bin/sh\necho real gh\nexit 1\n").unwrap();
    fs::set_permissions(&real_gh, fs::Permissions::from_mode(0o700)).unwrap();
    let deploy = root.join("aft-deploy");
    copy_and_warm_aft(&deploy);
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();

    let starting = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_replayed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_replayed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let starting_addr = starting.local_addr().unwrap().to_string();
    let first_replayed_addr = first_replayed.local_addr().unwrap().to_string();
    let peer_replayed_addr = peer_replayed.local_addr().unwrap().to_string();
    let first_env = [
        ("PATH", path.as_os_str()),
        (
            "AFT_TEST_BASH_BEFORE_CHILD_IDENTITY",
            OsStr::new(&starting_addr),
        ),
        (
            "AFT_TEST_BASH_AFTER_STARTING_REPLAY",
            OsStr::new(&first_replayed_addr),
        ),
    ];
    let peer_env = [(
        "AFT_TEST_BASH_AFTER_STARTING_REPLAY",
        OsStr::new(&peer_replayed_addr),
    )];
    let (mut first, mut peer) = tokio::join!(
        Daemon::spawn_with_env(&deploy, &first_root, &home, &storage, &first_env),
        Daemon::spawn_with_env(&deploy, &peer_root, &home, &storage, &peer_env),
    );
    let checkpoint = async |listener: &TcpListener| {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), b'1');
    };
    let limit = Duration::from_secs(30);
    configure(&mut first, &home, &project, &storage, "off").await;
    tokio::time::timeout(limit, checkpoint(&first_replayed))
        .await
        .unwrap();

    let replay_while_starting = async {
        let (mut gate, _) = starting.accept().await.unwrap();
        let mut metadata_path = String::new();
        BufReader::new(&mut gate)
            .read_line(&mut metadata_path)
            .await
            .unwrap();
        let metadata_path = Path::new(metadata_path.trim_end());
        let read_task = || -> serde_json::Value {
            serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap()
        };
        let before = read_task();
        let shim = metadata_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("io/startup-image/shims/gh");
        assert_eq!(before["status"], "starting", "{before:?}");
        assert!(before["child_pid"].is_null(), "{before:?}");
        assert!(shim.exists(), "missing held shell shim: {shim:?}");
        configure(&mut peer, &home, &project, &storage, "off").await;
        checkpoint(&peer_replayed).await;
        let after = read_task();
        let shim_retained = shim.exists();
        gate.write_all(b"1").await.unwrap();
        (after["status"].clone(), shim_retained)
    };
    let (result, replay) = tokio::join!(
        foreground(&mut first, "gh --shim-version"),
        tokio::time::timeout(limit, replay_while_starting),
    );
    let (held_status, shim_retained) = replay.unwrap();
    first.shutdown().await;
    peer.shutdown().await;
    assert_eq!(
        result["status"], "completed",
        "peer left task {held_status}, shim retained: {shim_retained}; {result:?}"
    );
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("\"shim_version\""),
        "{result:?}"
    );
    assert_eq!(held_status, "starting");
    assert!(shim_retained);
}
