use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::{fs::MetadataExt, process::CommandExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::json;
use subc_os::registration_channel::{ChildDescriptorSlots, DAEMON_PID_ENV, REGISTRATION_FD_ENV};
use subc_protocol::{Frame, FrameType, ModuleHelloAckBody, PROTOCOL_VERSION};
use subc_transport::authenticate_server;
use tokio::net::UnixStream;

use super::subc_detach_test::{
    bind_route, control_flags, extract_task_id, frame_body, read_any_frame_timeout,
    read_tool_response, send_connection_goodbye, send_frame, send_tool_call, tool_result_is_error,
    wait_for_status, write_raw_connection_file, write_user_config, ModuleProcess,
};

fn handoff(command: &mut Command) -> std::os::unix::net::UnixStream {
    let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
    let client: OwnedFd = client.into();
    let client = std::fs::File::from(client);
    let inode = client.metadata().unwrap().ino();
    let mut slots = ChildDescriptorSlots::default();
    slots.channel(client.into()).unwrap();
    command
        .env(REGISTRATION_FD_ENV, format!("4:{inode}"))
        .env(DAEMON_PID_ENV, std::process::id().to_string());
    slots.install(command);
    server
}

fn spawn_module(path: &Path, config: &Path, data: &Path) -> (ModuleProcess, UnixStream) {
    let mut command = Command::new(crate::test_helpers::aft_binary::aft_binary());
    command
        .args(["--subc".as_ref(), path.as_os_str()])
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_DATA_HOME", data)
        .env_remove("SUBC_MODULE_ID")
        .env("SUBC_LAUNCH_NONCE", "registration-must-ignore-this-nonce")
        .env("SUBC_LAUNCH_NONCE_FD", "not-a-valid-nonce-descriptor")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let stream = handoff(&mut command);
    let child = command.spawn().unwrap();
    drop(command);
    stream.set_nonblocking(true).unwrap();
    (
        ModuleProcess { child },
        UnixStream::from_std(stream).unwrap(),
    )
}

// Test the shell's open descriptors, not a grandchild's: the shell could
// otherwise close the socket before launching the probe and hide a leak.
const CHILD_PROBE: &str = r#"
    n=0; for fd in /dev/fd/*; do if [ -S "$fd" ]; then n=$((n+1)); fi; done
    printf 'sockets-%s\n' "$n"
    n=0; for name in SUBC_REGISTRATION_FD SUBC_LAUNCH_NONCE SUBC_LAUNCH_NONCE_FD; do
        if [ "${!name+x}" ]; then n=$((n+1)); fi
    done
    printf 'handoff-vars-%s\n' "$n"
"#;

fn assert_clean_child(label: &str, output: &str) {
    assert!(
        output.contains("sockets-0"),
        "{label} inherited a socket: {output}"
    );
    assert!(
        output.contains("handoff-vars-0"),
        "{label} inherited a handoff variable: {output}"
    );
}

#[tokio::test]
async fn registration_fake_hello_and_no_shell_inherits_channel() {
    let mut positive = Command::new("/bin/bash");
    positive.args(["-c", CHILD_PROBE]);
    let _peer = handoff(&mut positive);
    let output = positive.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("sockets-1"),
        "probe missed its inherited socket: {stdout:?}"
    );
    drop(positive);

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("project");
    let config = fixture.path().join("config");
    let data = fixture.path().join("data");
    write_user_config(&config, &fixture.path().join("storage"));
    // Leave TCP unserved so the module can only authenticate and register on
    // its handoff; post-HELLO background consumers cannot mint on this fake.
    let _listener = write_raw_connection_file(fixture.path()).await;
    let (mut module, mut channel) =
        spawn_module(&fixture.path().join("subc-connection.json"), &config, &data);
    authenticate_server(
        &mut channel,
        &[0x42; subc_transport::KEY_LEN],
        &[0x24; subc_transport::DAEMON_ID_LEN],
        "subc-detach-test",
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let hello = read_any_frame_timeout(&mut channel, "registration HELLO").await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    let body: serde_json::Value = serde_json::from_slice(&hello.body).unwrap();
    assert!(
        body.get("launch_nonce").is_none(),
        "registration HELLO must omit nonce"
    );
    assert!(body["manifest"]["provenance"]
        .get("launch_nonce_source")
        .is_none());
    let ack = ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: vec![],
        subc_capabilities: vec![],
        storage: None,
        machine_id: None,
    };
    send_frame(
        &mut channel,
        Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            hello.header.corr,
            serde_json::to_vec(&ack).unwrap(),
        )
        .unwrap(),
    )
    .await;
    bind_route(&mut channel, &root).await;

    for (corr, label, background, pty) in [
        (20, "foreground", false, false),
        (30, "background", true, false),
        (50, "PTY", true, true),
    ] {
        send_tool_call(&mut channel, 1, corr, "bash", json!({"command": CHILD_PROBE, "background": background, "pty": pty, "compressed": false, "timeout": 30_000})).await;
        let response = read_tool_response(&mut channel, corr, label).await;
        assert!(
            !tool_result_is_error(&response),
            "{label}: {}",
            frame_body(&response)
        );
        if background {
            let result = wait_for_status(
                &mut channel,
                corr + 1,
                &extract_task_id(&response),
                "completed",
            )
            .await;
            assert_clean_child(label, &result.to_string());
        } else {
            assert_clean_child(label, &frame_body(&response));
        }
    }
    send_connection_goodbye(&mut channel).await;
    assert!(module
        .wait_for_exit("registration module graceful shutdown")
        .success());
}
