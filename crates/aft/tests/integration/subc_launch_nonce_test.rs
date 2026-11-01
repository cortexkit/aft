#![cfg(unix)]

//! The launch nonce reaches AFT the way the subc daemon hands it over: as a
//! pipe at descriptor 3, named by `SUBC_LAUNCH_NONCE_FD`, next to the
//! environment copy the daemon still sets while modules move over. This drives
//! the real `aft --subc` binary with both and checks that its HELLO carries the
//! pipe's nonce and reports `launch_nonce_source: "fd"`, and that the bash
//! children it spawns (foreground, background and PTY) see neither variable
//! and do not hold the pipe.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use subc_os::launch_nonce::{LaunchNonceHandoff, LAUNCH_NONCE_FD_ENV};
use subc_protocol::{
    Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, PROTOCOL_VERSION, SUBC_LAUNCH_NONCE_ENV,
};
use subc_transport::authenticate_server;
use tokio::net::{TcpListener, TcpStream};

use super::subc_detach_test::{
    bind_route, control_flags, extract_task_id, frame_body, read_any_frame_timeout,
    read_tool_response, send_connection_goodbye, send_frame, send_tool_call, tool_result_is_error,
    wait_for_status, write_connection_file, write_user_config, ModuleProcess,
};

const PIPE_NONCE: &str = "pipe-launch-nonce-0123456789abcdef";
const ENV_COPY_NONCE: &str = "environment-copy-nonce-must-not-be-sent";
const ROUTE_CHANNEL: u16 = 1;

/// Reports, from inside a child, how many `SUBC_LAUNCH_NONCE*` variables it
/// has and whether descriptor 3 is a pipe, as the daemon's nonce pipe is. (A
/// bash task's wrapper may hold its own exit-marker file at 3, which is a
/// regular file, so "is 3 open" alone would not tell the two apart.) The
/// markers are assembled by the shell (`vars-$(...)`, `pipe3-$((n))`) so the
/// command text echoed back in a tool result can never be mistaken for the
/// child's answer: a clean child prints `vars-0` and `pipe3-0`.
const CHILD_PROBE: &str = "printf 'vars-%s\\n' \"$(env | grep -c '^SUBC_LAUNCH_NONCE')\"; \
     if [ -p /dev/fd/3 ]; then echo \"pipe3-$((1))\"; else echo \"pipe3-$((0))\"; fi";

fn spawn_module_with_pipe_nonce(
    conn_path: &std::path::Path,
    config_home: &std::path::Path,
    data_home: &std::path::Path,
) -> ModuleProcess {
    use std::os::unix::process::CommandExt;

    let binary = crate::test_helpers::aft_binary::aft_binary();
    let mut command = Command::new(binary);
    command
        .arg("--subc")
        .arg(conn_path)
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_DATA_HOME", data_home)
        .env_remove("SUBC_MODULE_ID")
        .env(SUBC_LAUNCH_NONCE_ENV, ENV_COPY_NONCE)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group, so dropping `ModuleProcess` kills its children too.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let handoff = LaunchNonceHandoff::new(PIPE_NONCE).expect("launch nonce pipe");
    command.env(LAUNCH_NONCE_FD_ENV, handoff.fd_env_value());
    handoff.install_last(&mut command);
    ModuleProcess {
        child: command.spawn().expect("spawn aft --subc module"),
    }
}

async fn accept_module_hello(listener: &TcpListener) -> (TcpStream, ModuleHelloBody) {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("timed out accepting module connection")
        .expect("accept module connection");
    // The key, daemon id and version match the connection file written by
    // `write_connection_file`.
    authenticate_server(
        &mut stream,
        &[0x42; subc_transport::KEY_LEN],
        &[0x24; subc_transport::DAEMON_ID_LEN],
        "subc-detach-test",
        Duration::from_secs(5),
    )
    .await
    .expect("authenticate module");

    let hello = read_any_frame_timeout(&mut stream, "ModuleHello").await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    let body: ModuleHelloBody = serde_json::from_slice(&hello.body).expect("hello body");
    send_frame(
        &mut stream,
        Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            hello.header.corr,
            serde_json::to_vec(&ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: Vec::new(),
                subc_capabilities: Vec::new(),
                storage: None,
                machine_id: None,
            })
            .expect("hello ack body"),
        )
        .expect("hello ack frame"),
    )
    .await;
    (stream, body)
}

fn assert_clean_child(label: &str, output: &str) {
    assert!(
        output.contains("vars-0"),
        "{label}: the child inherited a launch nonce variable: {output}"
    );
    assert!(
        output.contains("pipe3-0"),
        "{label}: the child has a pipe at descriptor 3: {output}"
    );
}

#[test]
fn subc_module_reads_the_pipe_nonce_and_no_child_inherits_it() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    runtime.block_on(async {
        let project = tempfile::tempdir().expect("project tempdir");
        let storage = tempfile::tempdir().expect("storage tempdir");
        let conn_dir = tempfile::tempdir().expect("connection tempdir");
        let config_home = tempfile::tempdir().expect("config home tempdir");
        let data_home = tempfile::tempdir().expect("data home tempdir");
        write_user_config(config_home.path(), storage.path());
        let listener = write_connection_file(conn_dir.path()).await;
        let conn_path = conn_dir.path().join("subc-connection.json");

        let mut module =
            spawn_module_with_pipe_nonce(&conn_path, config_home.path(), data_home.path());
        let (mut stream, hello) = accept_module_hello(&listener).await;

        assert_eq!(
            hello.launch_nonce.as_deref(),
            Some(PIPE_NONCE),
            "HELLO must carry the nonce from the pipe, not the environment copy"
        );
        let manifest = serde_json::to_value(&hello.manifest).expect("manifest json");
        eprintln!("captured module provenance: {}", manifest["provenance"]);
        assert_eq!(
            manifest["provenance"]["launch_nonce_source"],
            json!("fd"),
            "HELLO must report that the nonce came from the pipe"
        );

        assert_eq!(
            manifest["provenance"]["wire_crate_version"],
            json!("0.33.5")
        );
        // `build.rs` embeds Git's HEAD only when it recognizes the tracked AFT
        // workspace. Snapshot builds without that metadata can omit the SHA; the
        // pipe nonce and child checks below do not depend on build provenance.
        let build_git_sha = &manifest["provenance"]["build_git_sha"];
        // CI always builds from a full Git checkout, so a missing SHA there is a
        // provenance regression rather than a snapshot build.
        assert!(
            !build_git_sha.is_null() || std::env::var_os("CI").is_none(),
            "CI builds must embed the HELLO build Git SHA"
        );
        if !build_git_sha.is_null() {
            let build_git_sha = build_git_sha
                .as_str()
                .expect("HELLO build Git SHA must be a string when present");
            assert_eq!(build_git_sha.len(), 40, "HELLO build Git SHA must be full");
            let git = std::process::Command::new("git")
                .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
                .output()
                .expect("checkout build revision");
            assert!(git.status.success(), "checkout revision must be available");
            assert_eq!(
                build_git_sha,
                String::from_utf8_lossy(&git.stdout).trim(),
                "HELLO must carry the exact compiled checkout revision"
            );
        }

        bind_route(&mut stream, project.path()).await;

        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            20,
            "bash",
            json!({ "command": CHILD_PROBE, "compressed": false }),
        )
        .await;
        let foreground = read_tool_response(&mut stream, 20, "foreground bash").await;
        assert!(
            !tool_result_is_error(&foreground),
            "foreground bash failed: {}",
            frame_body(&foreground)
        );
        assert_clean_child("foreground bash", &frame_body(&foreground));

        for (corr, label, pty) in [(30, "background bash", false), (50, "PTY bash", true)] {
            send_tool_call(
                &mut stream,
                ROUTE_CHANNEL,
                corr,
                "bash",
                json!({
                    "command": CHILD_PROBE,
                    "background": true,
                    "pty": pty,
                    "compressed": false,
                    "timeout": 30_000,
                }),
            )
            .await;
            let launch = read_tool_response(&mut stream, corr, label).await;
            assert!(
                !tool_result_is_error(&launch),
                "{label} launch failed: {}",
                frame_body(&launch)
            );
            let task_id = extract_task_id(&launch);
            let completed: Value =
                wait_for_status(&mut stream, corr + 1, &task_id, "completed").await;
            assert_clean_child(label, &completed.to_string());
        }

        send_connection_goodbye(&mut stream).await;
        let exit = module.wait_for_exit("module graceful shutdown");
        assert!(exit.success(), "module exit status: {exit}");
    });
}
