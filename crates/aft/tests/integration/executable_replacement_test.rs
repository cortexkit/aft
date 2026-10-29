use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use tokio::net::TcpStream;

use super::helpers::AftProcess;
use super::subc_detach_test::{
    accept_module, bind_route_at_epoch, read_tool_response, send_connection_goodbye, send_frame,
    tool_response_json, write_connection_file, ModuleProcess,
};

struct Daemon {
    module: ModuleProcess,
    stream: TcpStream,
    epoch: u32,
    corr: u64,
}

impl Daemon {
    async fn spawn(binary: &Path, root: &Path, home: &Path, storage: &Path) -> Self {
        use std::os::unix::process::CommandExt;

        let listener = write_connection_file(root).await;
        let child = Command::new(binary)
            .arg("--subc")
            .arg(root.join("subc-connection.json"))
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("AFT_STORAGE_DIR", storage)
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .env_remove("AFT_DATA_HOME")
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("SUBC_LAUNCH_NONCE_FD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(binary.with_extension("stderr.log")).unwrap())
            .process_group(0)
            .spawn()
            .unwrap();
        let module = ModuleProcess { child };
        let stream = accept_module(&listener).await;
        Self {
            module,
            stream,
            epoch: 0,
            corr: 100,
        }
    }

    async fn call(&mut self, arguments: Value) -> Value {
        self.corr += 1;
        send_frame(
            &mut self.stream,
            Frame::build(
                FrameType::Request,
                Flags::new(false, Priority::Interactive, false),
                1,
                self.epoch,
                self.corr,
                serde_json::to_vec(&json!({ "name": "bash", "arguments": arguments })).unwrap(),
            )
            .unwrap(),
        )
        .await;
        let frame = read_tool_response(&mut self.stream, self.corr, "pinned daemon bash").await;
        tool_response_json(&frame)
    }

    async fn shutdown(&mut self) {
        send_connection_goodbye(&mut self.stream).await;
        assert!(self
            .module
            .wait_for_exit("pinned daemon clean exit")
            .success());
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

async fn configure(aft: &mut Daemon, home: &Path, project: &Path, storage: &Path, mode: &str) {
    let config_dir = home.join("config/cortexkit");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("aft.jsonc"),
        json!({
            "storage_dir": storage,
            "indexes": { "trigram": false, "semantic": false, "callgraph": false },
            "github": { "shim": true },
            "git": { "co_author": "off" },
            "sandbox": { "profile": { "head": mode } },
            "bash": { "background": true, "rewrite": true, "shell": "/bin/bash" },
        })
        .to_string(),
    )
    .unwrap();
    aft.epoch += 1;
    bind_route_at_epoch(&mut aft.stream, project, aft.epoch).await;
}

async fn foreground(aft: &mut Daemon, command: &str) -> Value {
    let mut arguments = json!({
        "command": command, "foreground_orchestrate": true,
        "permissions_requested": true, "compressed": false,
    });
    let response = aft.call(arguments.clone()).await;
    if response["code"] != "permission_required" {
        return response;
    }
    // Approve only the patterns requested by the shell permission gate. This
    // host approval does not disable the separately configured native sandbox.
    let approved: Vec<Value> = response["asks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|ask| ask["patterns"].as_array().unwrap().clone())
        .collect();
    assert!(!approved.is_empty(), "{response:?}");
    arguments["permissions_granted"] = json!(approved);
    aft.call(arguments).await
}

fn pin_directories(storage: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(storage.join("run"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("aft-pinned-")
        })
        .collect()
}

fn copy_and_warm_aft(destination: &Path) {
    std::fs::copy(crate::test_helpers::aft_binary::aft_binary(), destination).unwrap();
    let probe = Command::new(destination)
        .args(["gh-shim", "--shim-version"])
        .output()
        .unwrap();
    assert!(probe.status.success(), "{probe:?}");
    assert!(
        String::from_utf8_lossy(&probe.stdout).contains("\"shim_version\""),
        "{probe:?}"
    );
}

#[test]
fn pinned_executable_standalone_creates_no_pin() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("home");
    let storage = home.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let deploy = root.join("aft-deploy");
    copy_and_warm_aft(&deploy);
    let envs = [
        ("HOME", home.as_os_str()),
        ("AFT_STORAGE_DIR", storage.as_os_str()),
    ];
    let mut aft = AftProcess::spawn_binary_with_env(&deploy, &envs);
    let response = aft.send(r#"{"id":"standalone-echo","command":"echo","message":"hello"}"#);
    assert_eq!(response["success"], true, "{response:?}");
    assert_eq!(response["message"], "hello");
    let pins = if storage.join("run").exists() {
        pin_directories(&storage)
    } else {
        Vec::new()
    };
    assert!(
        pins.is_empty(),
        "standalone created executable pins: {pins:?}"
    );
    assert!(aft.shutdown().success());
}

#[tokio::test]
async fn pinned_executable_daemon_creates_pin_and_removes_it_on_clean_exit() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("home");
    let project = home.join("project");
    let storage = home.join("storage");
    std::fs::create_dir_all(&project).unwrap();
    let deploy = root.join("aft-deploy");
    copy_and_warm_aft(&deploy);
    let mut aft = Daemon::spawn(&deploy, &root, &home, &storage).await;
    configure(&mut aft, &home, &project, &storage, "off").await;
    let pins = pin_directories(&storage);
    assert_eq!(pins.len(), 1, "{pins:?}");
    aft.shutdown().await;
    assert!(!pins[0].exists(), "clean exit retained its pin");
    assert!(pin_directories(&storage).is_empty());
}

#[tokio::test]
async fn pinned_executable_real_server_survives_binary_replacement_and_cleans_dead_pins() {
    use std::os::unix::fs::MetadataExt;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let home = root.join("home");
    let project = home.join("project");
    let storage = home.join("storage");
    let ssh = home.join(".ssh");
    for directory in [&project, &storage, &ssh] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let secret = ssh.join("identity");
    std::fs::write(&secret, "PRIVATE_IMAGE_PIN_SENTINEL").unwrap();
    let deploy = root.join("aft-deploy");
    copy_and_warm_aft(&deploy);
    #[cfg(target_os = "linux")]
    {
        let support = Command::new(&deploy)
            .args(["sandbox-launch", "--support"])
            .output()
            .unwrap();
        assert!(
            support.status.success(),
            "native Landlock support is required: {support:?}"
        );
    }
    let original = deploy.metadata().unwrap();

    // The replacement is a different native image, not another path to AFT.
    // Its marker and exit code distinguish accidental re-exec from a refusal.
    let marker_source = root.join("marker.rs");
    let marker = root.join("marker");
    std::fs::write(
        &marker_source,
        "fn main() { println!(\"REPLACEMENT_IMAGE\"); std::process::exit(93); }",
    )
    .unwrap();
    let built = Command::new("rustc")
        .arg(&marker_source)
        .args(["--crate-name", "aft_replacement", "-o"])
        .arg(&marker)
        .output()
        .unwrap();
    assert!(built.status.success(), "{built:?}");

    let mut aft = Daemon::spawn(&deploy, &root, &home, &storage).await;
    configure(&mut aft, &home, &project, &storage, "off").await;
    let pins = pin_directories(&storage);
    assert_eq!(pins.len(), 1, "{pins:?}");
    let first_pin = pins[0].clone();
    let gh = first_pin.join("shims/gh");
    let pinned_image = match std::fs::read_link(&gh) {
        Ok(image) => image,
        Err(error) => {
            let stderr = std::fs::read_to_string(deploy.with_extension("stderr.log")).unwrap();
            panic!(
                "missing pinned gh link {}: {error}; daemon stderr: {stderr}",
                gh.display()
            );
        }
    };
    let pinned_metadata = pinned_image.metadata().unwrap();
    assert_eq!(
        (pinned_metadata.dev(), pinned_metadata.ino()),
        (original.dev(), original.ino()),
        "the initial shim must reference the server's original inode"
    );
    #[cfg(target_os = "macos")]
    assert!(pinned_image.starts_with(&storage));

    std::fs::rename(&marker, &deploy).unwrap();
    let decoy = Command::new(&deploy).output().unwrap();
    assert_eq!(decoy.status.code(), Some(93));
    assert!(String::from_utf8_lossy(&decoy.stdout).contains("REPLACEMENT_IMAGE"));

    let shim = foreground(&mut aft, "gh --shim-version").await;
    assert_eq!(shim["status"], "completed", "{shim:?}");
    let output = shim["output"].as_str().unwrap();
    assert!(output.contains("\"shim_version\""), "{shim:?}");
    assert!(!output.contains("REPLACEMENT_IMAGE"), "{shim:?}");

    configure(&mut aft, &home, &project, &storage, "strict").await;
    let native = foreground(
        &mut aft,
        &format!(
            "printf 'sandbox-start\\n'; gh --shim-version; cat {}",
            quote(&secret)
        ),
    )
    .await;
    let output = native["output"]
        .as_str()
        .unwrap_or_else(|| panic!("native launcher returned no child output: {native:?}"));
    assert!(
        output.contains("sandbox-start"),
        "the native launcher did not reach the shell: {native:?}"
    );
    assert!(
        output.contains("\"shim_version\""),
        "sandboxed gh used the wrong image: {native:?}"
    );
    assert!(!output.contains("REPLACEMENT_IMAGE"), "{native:?}");
    assert_eq!(native["status"], "failed", "{native:?}");
    assert!(!output.contains("PRIVATE_IMAGE_PIN_SENTINEL"), "{native:?}");
    assert!(
        output.contains("Permission denied") || output.contains("Operation not permitted"),
        "not a kernel denial: {native:?}"
    );
    assert_eq!(
        pin_directories(&storage),
        pins,
        "helpers must not create pins per command"
    );

    // Starting another real server must not retarget the first server's PATH.
    let other_deploy = root.join("aft-other");
    copy_and_warm_aft(&other_deploy);
    let mut other = Daemon::spawn(&other_deploy, &root, &home, &storage).await;
    configure(&mut other, &home, &project, &storage, "off").await;
    assert_eq!(pin_directories(&storage).len(), 2);
    assert_eq!(std::fs::read_link(&gh).unwrap(), pinned_image);
    let original_again = foreground(&mut aft, "gh --shim-version").await;
    assert_eq!(original_again["status"], "completed", "{original_again:?}");
    assert!(
        original_again["output"]
            .as_str()
            .unwrap()
            .contains("\"shim_version\""),
        "{original_again:?}"
    );
    other.shutdown().await;
    assert_eq!(
        pin_directories(&storage),
        pins,
        "clean exit removed another daemon's pin"
    );

    // A crash cannot run exit cleanup; the next daemon must reclaim its pin.
    aft.module.child.kill().unwrap();
    assert!(!aft.module.wait_for_exit("crashed pinned daemon").success());
    assert!(first_pin.exists(), "crash unexpectedly removed the pin");

    let mut restarted = Daemon::spawn(&other_deploy, &root, &home, &storage).await;
    configure(&mut restarted, &home, &project, &storage, "off").await;
    assert!(!first_pin.exists(), "a dead server's pin was retained");
    assert_eq!(pin_directories(&storage).len(), 1);
    restarted.shutdown().await;
    assert!(
        pin_directories(&storage).is_empty(),
        "clean restart retained its pin"
    );
}
