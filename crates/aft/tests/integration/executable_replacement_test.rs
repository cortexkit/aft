use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path, mode: &str) {
    let response = aft.send_with_timeout(
        &json!({
            "id": format!("configure-{mode}"), "command": "configure",
            "harness": "opencode", "project_root": project, "storage_dir": storage,
            "bash_permissions": true,
            "config": user_config(json!({
                "github": { "shim": true },
                "git": { "co_author": "off" },
                "sandbox": { "profile": { "head": mode } },
                "bash": { "background": true, "rewrite": true, "shell": "/bin/bash" },
            })),
        })
        .to_string(),
        Duration::from_secs(60),
    );
    assert_eq!(response["success"], true, "{response:?}");
}

fn foreground(aft: &mut AftProcess, id: &str, command: &str) -> Value {
    let mut request = json!({
        "id": id, "method": "bash", "session_id": "image-pin-test",
        "params": {
            "command": command, "foreground_orchestrate": true,
            "permissions_requested": true, "compressed": false,
        },
    });
    let response = aft.send_with_timeout(&request.to_string(), Duration::from_secs(60));
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
    request["params"]["permissions_granted"] = json!(approved);
    aft.send_with_timeout(&request.to_string(), Duration::from_secs(60))
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
fn pinned_executable_real_server_survives_binary_replacement_and_cleans_dead_pins() {
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

    let envs = [
        ("HOME", home.as_os_str()),
        ("AFT_STORAGE_DIR", storage.as_os_str()),
    ];
    let mut aft = AftProcess::spawn_binary_with_env(&deploy, &envs);
    configure(&mut aft, &project, &storage, "off");
    let pins = pin_directories(&storage);
    assert_eq!(pins.len(), 1, "{pins:?}");
    let first_pin = pins[0].clone();
    let gh = first_pin.join("shims/gh");
    let pinned_image = match std::fs::read_link(&gh) {
        Ok(image) => image,
        Err(error) => {
            let (_, stderr) = aft.stderr_output();
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

    let shim = foreground(&mut aft, "shim-after-swap", "gh --shim-version");
    assert_eq!(shim["status"], "completed", "{shim:?}");
    let output = shim["output"].as_str().unwrap();
    assert!(output.contains("\"shim_version\""), "{shim:?}");
    assert!(!output.contains("REPLACEMENT_IMAGE"), "{shim:?}");

    configure(&mut aft, &project, &storage, "strict");
    let native = foreground(
        &mut aft,
        "sandbox-after-swap",
        &format!(
            "printf 'sandbox-start\\n'; gh --shim-version; cat {}",
            quote(&secret)
        ),
    );
    let output = native["output"].as_str().unwrap();
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
    let mut other = AftProcess::spawn_binary_with_env(&other_deploy, &envs);
    configure(&mut other, &project, &storage, "off");
    assert_eq!(pin_directories(&storage).len(), 2);
    assert_eq!(std::fs::read_link(&gh).unwrap(), pinned_image);
    let original_again = foreground(&mut aft, "shim-after-second-server", "gh --shim-version");
    assert_eq!(original_again["status"], "completed", "{original_again:?}");
    assert!(
        original_again["output"]
            .as_str()
            .unwrap()
            .contains("\"shim_version\""),
        "{original_again:?}"
    );
    other.shutdown();
    aft.shutdown();

    let mut restarted = AftProcess::spawn_binary_with_env(&other_deploy, &envs);
    configure(&mut restarted, &project, &storage, "off");
    assert!(!first_pin.exists(), "a dead server's pin was retained");
    assert_eq!(pin_directories(&storage).len(), 1);
    restarted.shutdown();
}
