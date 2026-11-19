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

fn handoff(command: &mut Command) -> (std::os::unix::net::UnixStream, u64) {
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
    (server, inode)
}

fn spawn_module(path: &Path, config: &Path, data: &Path) -> (ModuleProcess, UnixStream, u64) {
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
    let (stream, inode) = handoff(&mut command);
    let child = command.spawn().unwrap();
    drop(command);
    stream.set_nonblocking(true).unwrap();
    (
        ModuleProcess { child },
        UnixStream::from_std(stream).unwrap(),
        inode,
    )
}

// Test the shell's open descriptors, not a grandchild's: the shell could
// otherwise close the socket before launching the probe and hide a leak.
// A CI runner can leave unrelated inheritable sockets in the test process, so
// the probe reports two things: sockets above standard input, output and
// error, compared against a baseline, and sockets with the channel's inode.
// Some macOS virtual machines report every socket's inode as 0, so the inode
// check runs only where the test sees a real inode.
fn child_probe(channel_inode: u64) -> String {
    format!(
        r#"
    n=0; c=0; for fd in /dev/fd/*; do
        case "${{fd##*/}}" in 0|1|2) continue ;; esac
        if [ -S "$fd" ]; then
            n=$((n+1))
            set -- $(ls -iLd "$fd" 2>/dev/null)
            if [ "$1" = "{channel_inode}" ]; then c=$((c+1)); fi
        fi
    done
    printf 'sockets-%s\nchannel-sockets-%s\n' "$n" "$c"
    n=0; for name in SUBC_REGISTRATION_FD SUBC_LAUNCH_NONCE SUBC_LAUNCH_NONCE_FD; do
        if [ "${{!name+x}}" ]; then n=$((n+1)); fi
    done
    printf 'handoff-vars-%s\n' "$n"
"#
    )
}

fn probe_count(output: &str, name: &str) -> usize {
    // Tool results arrive as JSON text, where the probe's newlines are the
    // two characters `\n`; turn them back into line breaks first.
    let prefix = format!("{name}-");
    output
        .replace("\\n", "\n")
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .find_map(|word| word.strip_prefix(&prefix)?.parse().ok())
        .unwrap_or_else(|| panic!("probe output has no {name}: {output}"))
}

/// `baseline` is the number of sockets a child of this test process sees
/// without any handoff.
fn assert_clean_child(label: &str, output: &str, baseline: usize, channel_inode: u64) {
    assert!(
        probe_count(output, "sockets") <= baseline,
        "{label} inherited a socket beyond the {baseline} the test process leaks: {output}"
    );
    if channel_inode != 0 {
        assert_eq!(
            probe_count(output, "channel-sockets"),
            0,
            "{label} inherited the registration channel: {output}"
        );
    }
    assert_eq!(
        probe_count(output, "handoff-vars"),
        0,
        "{label} inherited a handoff variable: {output}"
    );
}

#[tokio::test]
async fn registration_fake_hello_and_no_shell_inherits_channel() {
    let base = Command::new("/bin/bash")
        .args(["-c", &child_probe(0)])
        .output()
        .unwrap();
    let baseline = probe_count(&String::from_utf8_lossy(&base.stdout), "sockets");
    let mut positive = Command::new("/bin/bash");
    let (_peer, positive_inode) = handoff(&mut positive);
    positive.args(["-c", &child_probe(positive_inode)]);
    let output = positive.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        probe_count(&stdout, "sockets"),
        baseline + 1,
        "probe missed its inherited socket: {stdout:?}"
    );
    if positive_inode != 0 {
        assert_eq!(probe_count(&stdout, "channel-sockets"), 1, "{stdout:?}");
    }
    drop(positive);

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("project");
    let config = fixture.path().join("config");
    let data = fixture.path().join("data");
    write_user_config(&config, &fixture.path().join("storage"));
    // Leave TCP unserved so the module can only authenticate and register on
    // its handoff; post-HELLO background consumers cannot mint on this fake.
    let _listener = write_raw_connection_file(fixture.path()).await;
    let (mut module, mut channel, channel_inode) =
        spawn_module(&fixture.path().join("subc-connection.json"), &config, &data);
    let probe = child_probe(channel_inode);
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
        send_tool_call(&mut channel, 1, corr, "bash", json!({"command": probe, "background": background, "pty": pty, "compressed": false, "timeout": 30_000})).await;
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
            assert_clean_child(label, &result.to_string(), baseline, channel_inode);
        } else {
            assert_clean_child(label, &frame_body(&response), baseline, channel_inode);
        }
    }
    send_connection_goodbye(&mut channel).await;
    assert!(module
        .wait_for_exit("registration module graceful shutdown")
        .success());
}
