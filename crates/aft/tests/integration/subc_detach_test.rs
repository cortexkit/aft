#![cfg(unix)]

// This regression drives the daemon's Unix-shaped drain restart: SIGTERM to the
// module process group. Windows uses CREATE_NEW_PROCESS_GROUP instead of POSIX
// process groups, so Windows coverage is the cross-target `cargo check` gate.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::ReleaseOnDrop;
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, Principal,
    Priority, RouteTarget, PROTOCOL_VERSION,
};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::net::{TcpListener, TcpStream};

const SESSION_ID: &str = "subc-detach-session";
const ROUTE_CHANNEL: u16 = 1;

#[test]
fn subc_background_bash_survives_module_process_group_restart() {
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

        let ready = project.path().join("bg.ready");
        let stop = project.path().join("bg.stop");
        // Declare after all TempDirs: Rust drops locals in reverse declaration
        // order, so this guard writes the sentinel before any TempDir removes its directory.
        let _stop_guard = ReleaseOnDrop::new(stop.clone());
        let command = sentinel_command(&ready, &stop);

        let mut first_module = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;

        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            20,
            "bash",
            json!({
                "command": command,
                "background": true,
                "timeout": 60_000,
                "compressed": false,
            }),
        )
        .await;
        let launch = read_tool_response(&mut stream, 20, "background bash launch").await;
        assert!(!tool_result_is_error(&launch), "launch failed: {}", frame_body(&launch));
        let task_id = extract_task_id(&launch);
        wait_for_path(&ready, "background task ready file");

        let running = bash_status(&mut stream, 21, &task_id).await;
        assert_eq!(running["status"], "running", "unexpected running status: {running}");
        let child_pid = running["child_pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .expect("running bash_status should report child_pid");

        let first_exit = first_module.terminate_process_group(libc::SIGTERM);
        assert_eq!(
            first_exit.code(),
            Some(128 + libc::SIGTERM),
            "module should exit through the subc signal handler after detaching; signal={:?}, status={first_exit}",
            first_exit.signal()
        );
        drop(stream);
        assert_process_alive(child_pid, "background child after module process-group SIGTERM");

        let mut second_module = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;

        let replayed = bash_status(&mut stream, 30, &task_id).await;
        assert_eq!(
            replayed["status"], "running",
            "fresh module should rehydrate running task: {replayed}"
        );
        assert_eq!(
            replayed["child_pid"].as_u64(),
            Some(u64::from(child_pid)),
            "rehydrated status should describe the same detached child"
        );
        assert_process_alive(child_pid, "rehydrated background child");

        drop(_stop_guard);
        let completed = wait_for_status(&mut stream, 31, &task_id, "completed").await;
        assert_eq!(completed["exit_code"], 0, "task should exit cleanly: {completed}");
        let output = completed["output_preview"].as_str().unwrap_or_default();
        assert!(
            output.contains("sentinel-stopped"),
            "completion should surface captured output, got {completed}"
        );

        send_connection_goodbye(&mut stream).await;
        let second_exit = second_module.wait_for_exit("second module graceful shutdown");
        assert!(second_exit.success(), "second module exit status: {second_exit}");
    });
}

#[test]
fn subc_foreground_drain_preserves_route_harness_across_restart() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let conn_dir = tempfile::tempdir().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        let data_home = tempfile::tempdir().unwrap();
        write_user_config(config_home.path(), storage.path());
        let listener = write_connection_file(conn_dir.path()).await;
        let conn_path = conn_dir.path().join("subc-connection.json");
        let ready = project.path().join("foreground.ready");
        let stop = project.path().join("foreground.stop");
        let release = ReleaseOnDrop::new(stop.clone());
        let mut first = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;
        // Another consumer can configure the same root without owning this route.
        bind_route_as(&mut stream, project.path(), 2, "runner").await;
        send_tool_call(&mut stream, ROUTE_CHANNEL, 20, "bash", json!({
            "command": sentinel_command(&ready, &stop), "wait": true,
            "timeout": 60_000, "compressed": false,
        })).await;
        wait_for_path(&ready, "foreground ready");
        send_module_draining(&mut stream).await;
        let detached = read_tool_response(&mut stream, 20, "drain detach").await;
        assert!(!tool_result_is_error(&detached), "{}", frame_body(&detached));
        let task_id = extract_task_id(&detached);
        let db = rusqlite::Connection::open(data_home.path().join("cortexkit/aft/aft.db")).unwrap();
        let (harness, stdout, stderr, pgid, notify): (String, String, String, i64, bool) = db.query_row(
            "SELECT harness, stdout_path, stderr_path, pgid, json_extract(metadata, '$.notify_on_completion') FROM bash_tasks WHERE task_id = ?1",
            [&task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).unwrap();
        assert_eq!(harness, "opencode", "persisted owner must be the command route, not the last root configure");
        assert!(pgid > 0 && notify);
        assert!(Path::new(&stdout).is_file() && Path::new(&stderr).is_file());
        assert!(frame_body(&detached).contains(&stdout), "detach must name its output: {}", frame_body(&detached));
        send_connection_goodbye(&mut stream).await;
        assert!(first.wait_for_exit("drained module").success());
        drop(stream);
        let mut second = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;
        let running = bash_status(&mut stream, 30, &task_id).await;
        assert_eq!(running["status"], "running", "{running}");
        assert_eq!(running["child_pid"].as_i64(), Some(pgid));
        drop(release);
        let completed = wait_for_status(&mut stream, 31, &task_id, "completed").await;
        assert_eq!(completed["exit_code"], 0);
        assert!(completed["output_preview"].as_str().unwrap().contains("sentinel-stopped"));
        assert!(std::fs::read_to_string(&stdout).unwrap().contains("sentinel-stopped"));
        send_tool_call(&mut stream, ROUTE_CHANNEL, 100, "bash_drain_completions", json!({})).await;
        let completions = read_tool_response(&mut stream, 100, "restarted completion delivery").await;
        assert!(tool_response_json(&completions)["bg_completions"].as_array().unwrap().iter().any(|item| item["task_id"] == task_id), "{}", frame_body(&completions));
        send_tool_call(&mut stream, ROUTE_CHANNEL, 110, "bash_watch", json!({ "taskId": task_id })).await;
        let watch = read_tool_response(&mut stream, 110, "terminal watch delivery").await;
        assert!(!tool_result_is_error(&watch), "{}", frame_body(&watch));
        assert_eq!(tool_response_json(&watch)["status"], "completed");
        assert!(tool_response_json(&watch)["output_preview"].as_str().unwrap().contains("sentinel-stopped"));
        send_tool_call(&mut stream, ROUTE_CHANNEL, 111, "bash_drain_completions", json!({})).await;
        let after_watch = read_tool_response(&mut stream, 111, "watch acknowledged completion").await;
        assert!(tool_response_json(&after_watch)["bg_completions"].as_array().unwrap().is_empty(), "{}", frame_body(&after_watch));
        send_connection_goodbye(&mut stream).await;
        assert!(second.wait_for_exit("restarted module").success());
        drop(stream);
        let mut third = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;
        assert_eq!(bash_status(&mut stream, 120, &task_id).await["status"], "completed");
        send_tool_call(&mut stream, ROUTE_CHANNEL, 121, "bash_drain_completions", json!({})).await;
        let collected = read_tool_response(&mut stream, 121, "collected completion after restart").await;
        assert!(tool_response_json(&collected)["bg_completions"].as_array().unwrap().is_empty(), "{}", frame_body(&collected));
        send_connection_goodbye(&mut stream).await;
        assert!(third.wait_for_exit("collected module").success());
        let harnesses: String = db.query_row("SELECT group_concat(harness) FROM bash_tasks WHERE task_id = ?1", [&task_id], |row| row.get(0)).unwrap();
        assert_eq!(harnesses, "opencode", "watchdog must not rewrite ownership");
    });
}

/// A root context is shared by routes from several harnesses, and each bind
/// reconfigures it with its own harness. An edit and a background command made
/// through the opencode route after a runner route bound the same root must
/// still be found by the opencode route after the module restarts, when only
/// the opencode route binds again.
#[test]
fn subc_undo_and_background_task_survive_restart_after_mixed_harness_binds() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Undo snapshots skip files under the system temp directory, so the
        // edited project lives under the target directory instead.
        // CI runs this binary from a nextest archive on another runner, where
        // the compile-time CARGO_TARGET_TMPDIR path has not been created.
        std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let project = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let storage = tempfile::tempdir().unwrap();
        let conn_dir = tempfile::tempdir().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        let data_home = tempfile::tempdir().unwrap();
        write_user_config(config_home.path(), storage.path());
        let listener = write_connection_file(conn_dir.path()).await;
        let conn_path = conn_dir.path().join("subc-connection.json");
        let target = project.path().join("undo-target.txt");
        std::fs::write(&target, "original\n").unwrap();

        let mut first = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;
        // Another consumer configures the same root without owning this route.
        bind_route_as(&mut stream, project.path(), 2, "runner").await;

        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            20,
            "write",
            json!({ "filePath": target.to_string_lossy(), "content": "changed\n" }),
        )
        .await;
        let written = read_tool_response(&mut stream, 20, "opencode write").await;
        assert!(!tool_result_is_error(&written), "{}", frame_body(&written));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "changed\n");

        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            21,
            "bash",
            json!({
                "command": "echo mixed-harness-done", "background": true,
                "timeout": 60_000, "compressed": false,
            }),
        )
        .await;
        let launch = read_tool_response(&mut stream, 21, "opencode background launch").await;
        assert!(!tool_result_is_error(&launch), "{}", frame_body(&launch));
        let task_id = extract_task_id(&launch);
        wait_for_status(&mut stream, 22, &task_id, "completed").await;

        send_module_draining(&mut stream).await;
        send_connection_goodbye(&mut stream).await;
        assert!(first.wait_for_exit("mixed-harness module").success());
        drop(stream);

        let mut second = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;

        let replayed = bash_status(&mut stream, 40, &task_id).await;
        assert_eq!(replayed["status"], "completed", "{replayed}");
        assert!(
            replayed["output_preview"]
                .as_str()
                .unwrap_or_default()
                .contains("mixed-harness-done"),
            "{replayed}"
        );

        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            50,
            "safety",
            json!({ "op": "undo", "filePath": target.to_string_lossy() }),
        )
        .await;
        let undo = read_tool_response(&mut stream, 50, "undo after restart").await;
        assert!(
            !tool_result_is_error(&undo),
            "undo history written through the opencode route must survive the restart: {}",
            frame_body(&undo)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");

        send_connection_goodbye(&mut stream).await;
        assert!(second.wait_for_exit("restarted module").success());
    });
}

#[test]
fn subc_unadopted_task_reports_output_without_cross_harness_recovery() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let conn_dir = tempfile::tempdir().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        let data_home = tempfile::tempdir().unwrap();
        write_user_config(config_home.path(), storage.path());
        let listener = write_connection_file(conn_dir.path()).await;
        let conn_path = conn_dir.path().join("subc-connection.json");
        let ready = project.path().join("foreign.ready");
        let stop = project.path().join("foreign.stop");
        let _release = ReleaseOnDrop::new(stop.clone());
        let mut first = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route_as(&mut stream, project.path(), ROUTE_CHANNEL, "runner").await;
        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            20,
            "bash",
            json!({
                "command": sentinel_command(&ready, &stop), "background": true,
                "timeout": 60_000, "compressed": false,
            }),
        )
        .await;
        let launch = read_tool_response(&mut stream, 20, "runner launch").await;
        let task_id = extract_task_id(&launch);
        wait_for_path(&ready, "runner task ready");
        let running = bash_status(&mut stream, 21, &task_id).await;
        let stdout = running["output_path"].as_str().unwrap();
        let stderr = running["stderr_path"].as_str().unwrap();
        send_module_draining(&mut stream).await;
        send_connection_goodbye(&mut stream).await;
        assert!(first.wait_for_exit("runner module").success());
        drop(stream);
        let mut second = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;
        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            30,
            "bash_status",
            json!({ "task_id": task_id }),
        )
        .await;
        let refused = read_tool_response(&mut stream, 30, "foreign task refusal").await;
        assert!(
            tool_result_is_error(&refused),
            "must not adopt a foreign namespace"
        );
        let text = frame_body(&refused);
        assert!(
            text.contains("could not be adopted") && text.contains(stdout) && text.contains(stderr),
            "{text}"
        );
        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            40,
            "bash_notify",
            json!({ "task_id": task_id, "pattern": "sentinel-stopped" }),
        )
        .await;
        let refused_watch =
            read_tool_response(&mut stream, 40, "foreign async watch refusal").await;
        let text = frame_body(&refused_watch);
        assert!(
            tool_result_is_error(&refused_watch)
                && text.contains("could not be adopted")
                && text.contains(stdout)
                && text.contains(stderr),
            "{text}"
        );
        assert_process_alive(
            running["child_pid"].as_u64().unwrap() as u32,
            "unadopted child",
        );
        send_connection_goodbye(&mut stream).await;
        assert!(second.wait_for_exit("refusal module").success());
        drop(stream);
        // Returning to the owning namespace restores normal process control.
        let mut third = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route_as(&mut stream, project.path(), ROUTE_CHANNEL, "runner").await;
        send_tool_call(
            &mut stream,
            ROUTE_CHANNEL,
            31,
            "bash_kill",
            json!({ "task_id": task_id }),
        )
        .await;
        let killed = read_tool_response(&mut stream, 31, "rehydrated kill").await;
        assert!(!tool_result_is_error(&killed), "{}", frame_body(&killed));
        wait_for_status(&mut stream, 32, &task_id, "killed").await;
        send_connection_goodbye(&mut stream).await;
        assert!(third.wait_for_exit("owning module").success());
    });
}

/// The supervisor respawns a module only on a non-zero exit; exit 0 means
/// "stopped on request" and leaves the module down with no respawn. So the
/// two ways a daemon connection can end must map to different exit codes:
/// a channel-0 Goodbye is a stop request (0), a bare EOF is a connection loss
/// (3). Both arms run the real binary; the second is the control that keeps
/// the codes provably distinct.
#[test]
fn subc_bare_eof_exits_nonzero_and_goodbye_exits_zero() {
    const CONNECTION_LOST_EXIT_CODE: i32 = 3;
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

        // Arm 1: the daemon's socket goes away with no Goodbye (the 2026-09-06
        // outage shape: the daemon dropped the module connection on a client
        // error and the module exited 0, so nothing respawned it for 4.5 h).
        let mut module = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;
        drop(stream);
        let exit = module.wait_for_exit("module after bare EOF");
        assert_eq!(
            exit.code(),
            Some(CONNECTION_LOST_EXIT_CODE),
            "bare EOF must exit with the connection-lost code so the supervisor respawns; status={exit}"
        );

        // Arm 2 (control): an explicit channel-0 Goodbye is a stop request.
        let mut module = ModuleProcess::spawn(&conn_path, config_home.path(), data_home.path());
        let mut stream = accept_module(&listener).await;
        bind_route(&mut stream, project.path()).await;
        send_connection_goodbye(&mut stream).await;
        let exit = module.wait_for_exit("module after channel-0 Goodbye");
        assert_eq!(
            exit.code(),
            Some(0),
            "channel-0 Goodbye is a stop request and must exit 0; status={exit}"
        );
    });
}

/// A supervisor restart or daemon shutdown sends `module.draining`, then the
/// daemon closes the module's connection. That end is initiated by the
/// supervisor, so the module must exit 0 and say so in its log; the supervisor
/// reads any non-zero exit as a crash. A 2026-09-24 daemon shutdown recorded
/// such an exit as `exit 1` with no log line explaining it. Both ways the
/// daemon can drop the socket are driven: an orderly close (EOF) and an abort
/// (reset) while a request is still held open. The bare-EOF test above is the
/// control: without the drain notice the same close is still a lost
/// connection (exit 3).
#[test]
fn subc_connection_close_after_drain_exits_zero_and_logs_it() {
    const DRAIN_CLOSE_LINE: &str = "daemon closed the connection after its module.draining notice; supervisor-initiated shutdown, exiting 0";
    const EXIT_LINE: &str = "subc module stopped at the daemon's request; exiting 0";
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
        let logs = tempfile::tempdir().expect("module stderr tempdir");
        write_user_config(config_home.path(), storage.path());
        let listener = write_connection_file(conn_dir.path()).await;
        let conn_path = conn_dir.path().join("subc-connection.json");

        for abort in [false, true] {
            let label = if abort { "reset" } else { "close" };
            let stderr_path = logs.path().join(format!("module-{label}.stderr"));
            let mut module = ModuleProcess::spawn_with_stderr(
                &conn_path,
                config_home.path(),
                data_home.path(),
                Some(&stderr_path),
            );
            let mut stream = accept_module(&listener).await;
            bind_route(&mut stream, project.path()).await;
            if abort {
                // A request still held when the daemon aborts the socket: the
                // module is mid-answer when the connection goes.
                send_tool_call(
                    &mut stream,
                    ROUTE_CHANNEL,
                    40,
                    "bash",
                    json!({
                        "command": "sleep 5",
                        "foreground_orchestrate": true,
                        "wait": true,
                        "timeout": 60_000,
                        "compressed": false,
                    }),
                )
                .await;
            }
            send_module_draining(&mut stream).await;
            // Let the module act on the notice before the daemon drops it.
            tokio::time::sleep(Duration::from_millis(500)).await;
            if abort {
                // Linger 0 makes the drop send a reset instead of a FIN. It
                // cannot block here: there is nothing left to flush.
                #[allow(deprecated)]
                let linger = stream.set_linger(Some(Duration::ZERO));
                linger.expect("set linger 0");
            }
            drop(stream);

            let exit = module.wait_for_exit(&format!("module after drain and {label}"));
            let log = std::fs::read_to_string(&stderr_path).expect("read module stderr");
            assert_eq!(
                exit.code(),
                Some(0),
                "a connection {label} after module.draining is a supervisor-initiated shutdown and must exit 0; status={exit}; log tail:\n{}",
                log_tail(&log)
            );
            assert!(
                log.contains(DRAIN_CLOSE_LINE) && log.contains(EXIT_LINE),
                "the module log must say why it exited 0 after the drain ({label}); log tail:\n{}",
                log_tail(&log)
            );
        }
    });
}

#[test]
fn subc_drain_exits_with_many_live_lsp_servers_in_one_deadline() {
    // Four independent slow servers distinguish one shared budget from a
    // per-server budget while keeping the negative control below the hang cap.
    drain_with_live_lsp_servers_and_writer("", false, "40", None, None, 4);
}

/// Servers that ignore the Shutdown request and SIGTERM, and linger after
/// their client leaves, stop only for a forced kill. An injected 30 s fake
/// server exit delay, rather than busy-loop threads, prevents natural exit
/// from satisfying the test. The LSP phase (kill and reap included) must end
/// within its ceiling, the whole exit within the drain budget, and no server
/// may outlive the module.
#[test]
fn subc_drain_exit_stays_bounded_when_lsp_servers_ignore_sigterm_with_delayed_exit() {
    drain_with_live_lsp_servers(
        "AFT_FAKE_LSP_IGNORE_SIGTERM=1 AFT_FAKE_LSP_EXIT_DELAY_MS=30000",
        false,
    );
}

/// Scoped inspect waits, within its startup budget, for a server it starts:
/// with every fake server slow to come up (nothing, not even its pid file,
/// exists for half a second), each inspect still answers only after that
/// root's server is running, so all 34 pid files exist when the drain starts.
#[test]
fn subc_drain_with_slow_starting_lsp_servers_finds_every_server_started() {
    drain_with_live_lsp_servers("AFT_FAKE_LSP_START_DELAY_MS=500", false);
}

#[test]
fn subc_drain_with_active_ort_flushes_final_line_before_hard_exit() {
    drain_with_live_lsp_servers("", true);
}

#[test]
fn subc_drain_with_slow_writer_persists_terminal_line() {
    drain_with_live_lsp_servers_and_writer("", false, "1000", Some("1000"), Some("450"), 34);
}

/// Bind independent roots that each start a fake rust-analyzer which never
/// answers Shutdown, then check the LSP phase's own budget and absence of
/// surviving children. `server_env` holds extra
/// `NAME=value` assignments for the fake servers, including deterministic
/// startup and exit delays.
fn drain_with_live_lsp_servers(server_env: &str, active_ort: bool) {
    drain_with_live_lsp_servers_and_writer(server_env, active_ort, "40", None, None, 34);
}

fn drain_with_live_lsp_servers_and_writer(
    server_env: &str,
    active_ort: bool,
    writer_delay: &str,
    flush_hold: Option<&str>,
    index_delay: Option<&str>,
    server_count: usize,
) {
    assert!(
        server_count >= 3,
        "shared-deadline proof needs at least three servers"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let projects = (0..server_count)
            .map(|_| tempfile::tempdir().unwrap())
            .collect::<Vec<_>>();
        let storage = tempfile::tempdir().unwrap();
        let conn_dir = tempfile::tempdir().unwrap();
        let config_home = tempfile::tempdir().unwrap();
        let data_home = tempfile::tempdir().unwrap();
        let pids = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let stderr_path = logs.path().join("module.stderr");
        let phase_events_path = logs.path().join("exit-phase-events.log");
        let config_dir = config_home.path().join("cortexkit");
        std::fs::create_dir_all(&config_dir).unwrap();
        let fake = crate::test_helpers::fake_lsp::fake_server_binary();
        let bin_dir = tempfile::tempdir().unwrap();
        let wrapper = bin_dir.path().join("rust-analyzer");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nAFT_FAKE_LSP_IGNORE_SHUTDOWN=1 {server_env} AFT_FAKE_LSP_PID_DIR='{}' exec '{}' \"$@\"\n",
                pids.path().display(),
                fake.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            config_dir.join("aft.jsonc"),
            serde_json::to_vec(&json!({
                "storage_dir": storage.path(), "indexes": { "trigram": false, "semantic": false, "callgraph": false }
            }))
            .unwrap(),
        )
        .unwrap();
        let listener = write_connection_file(conn_dir.path()).await;
        let conn_path = conn_dir.path().join("subc-connection.json");
        let mut module = ModuleProcess::spawn_with_exit_hooks(
            &conn_path,
            config_home.path(),
            data_home.path(),
            Some(&stderr_path),
            Some(bin_dir.path()),
            active_ort,
            true,
            writer_delay,
            flush_hold,
            index_delay,
            index_delay.map(|_| phase_events_path.as_path()),
        );
        let mut stream = accept_module(&listener).await;
        for (index, project) in projects.iter().enumerate() {
            std::fs::write(
                project.path().join("Cargo.toml"),
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::create_dir_all(project.path().join("src")).unwrap();
            let source = project.path().join("src/main.rs");
            std::fs::write(&source, "source").unwrap();
            bind_route_as(&mut stream, project.path(), (index + 1) as u16, "opencode").await;
            let corr = 100 + index as u64;
            send_tool_call(
                &mut stream,
                (index + 1) as u16,
                corr,
                "inspect",
                json!({"scope": source}),
            )
            .await;
            let result = read_frame_timeout(&mut stream, "spawn fake LSP").await;
            assert_eq!(result.header.channel, (index + 1) as u16);
            assert_eq!(result.header.corr, corr);
            assert!(!tool_result_is_error(&result), "{}", frame_body(&result));
        }
        let children = std::fs::read_dir(pids.path())
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .parse::<u32>()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(children.len(), server_count, "every root must own a live server");
        send_module_draining(&mut stream).await;
        let drained = Instant::now();
        send_connection_goodbye(&mut stream).await;
        let exit = module.wait_for_exit("drained module with live LSP servers");
        let elapsed = drained.elapsed();
        eprintln!(
            "drain completion to process exit: {} ms",
            elapsed.as_millis()
        );
        let log = std::fs::read_to_string(&stderr_path).unwrap();
        if index_delay.is_some() {
            let events = std::fs::read_to_string(&phase_events_path)
                .expect("shutdown phase event hook recorded its boundaries");
            assert_shutdown_phases_overlap(&events);
        }
        if writer_delay == "1000" {
            assert!(log.contains("phase=log_flush_done flushed=false"),
                "slow writer must exhaust the async flush budget: {}", log_tail(&log));
        }
        // Print phase and LSP summary lines. The bound below uses the LSP
        // phase's own elapsed value, never the outer process-exit observation.
        for line in log
            .lines()
            .filter(|line| line.contains("subc exit phase=") || line.contains("lsp shutdown_all:"))
        {
            eprintln!("{line}");
        }
        assert!(exit.success(), "{exit}; {}", log_tail(&log));
        // The process wait has a generous hang ceiling. Correct teardown is
        // proved below by its phases, durable terminal marker and child census,
        // not by how quickly the runner schedules this assertion thread.
        let shutdowns = log
            .lines()
            .filter(|line| line.contains("subc exit phase=lsp_shutdown "))
            .collect::<Vec<_>>();
        assert_eq!(shutdowns.len(), 1, "one shared LSP shutdown phase: {log}");
        assert!(
            shutdowns[0].contains(" budget_ms=1500 "),
            "all servers must be admitted under one 1500 ms shutdown budget: {log}"
        );
        let durable_path = data_home.path().join("aft").join("logs").join(format!("aft-{}.log", module.child.id()));
        let durable = std::fs::read_to_string(&durable_path)
            .unwrap_or_else(|error| panic!("{}: {error}", durable_path.display()));
        let mode = if active_ort { "skip_native_teardown" } else { "return" };
        assert!(
            durable.contains(&format!("subc exit phase=process_exit mode={mode}")),
            "missing durable terminal marker; {}", log_tail(&durable)
        );
        if active_ort {
            assert!(durable.contains("phase=ort_quiesce active=1 budget_ms=0"), "{}", log_tail(&durable));
        }
        // Draining roots may already have stopped some servers before the final
        // sweep, so the summary's split depends on timing. What must hold is one
        // summary, a bounded exit and no orphans.
        let summaries: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("lsp shutdown_all: servers="))
            .collect();
        assert_eq!(
            summaries.len(),
            1,
            "expected one shutdown summary; {}",
            log_tail(&log)
        );

        let summary_field = |field: &str| {
            summaries[0]
                .split_whitespace()
                .find_map(|word| word.strip_prefix(field))
                .and_then(|value| value.parse::<u128>().ok())
                .unwrap_or_else(|| panic!("missing {field} in shutdown summary: {}", summaries[0]))
        };
        let servers = summary_field("servers=");
        assert!(servers >= 3, "shared-deadline proof needs at least three shutdowns: {}", summaries[0]);
        let lsp_elapsed_ms = summary_field("elapsed_ms=");
        let lsp_ceiling_ms = 2 * aft::lsp::manager::LSP_SHUTDOWN_ALL_BUDGET.as_millis();
        // Measure AFT's phase, not the parent thread's wakeup or process exit.
        // Two shared budgets tolerate overshoot but not N sequential budgets.
        assert!(
            lsp_elapsed_ms < lsp_ceiling_ms,
            "LSP shutdown took {lsp_elapsed_ms} ms for {servers} servers; expected less than two shared budgets ({lsp_ceiling_ms} ms): {}",
            log_tail(&log)
        );

        // A server killed too late to be reaped before the module exited is
        // reaped by the system right after, so allow that a moment.
        let deadline = Instant::now() + Duration::from_secs(30);
        for pid in children {
            while aft::bash_background::process::is_process_alive(pid) && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !aft::bash_background::process::is_process_alive(pid),
                "orphaned LSP pid {pid}"
            );
        }
    });
}

fn log_tail(log: &str) -> String {
    let lines = log.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

fn assert_shutdown_phases_overlap(events: &str) {
    let events = events.lines().collect::<Vec<_>>();
    let position = |event: &str| {
        let positions = events
            .iter()
            .enumerate()
            .filter_map(|(index, recorded)| (*recorded == event).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(positions.len(), 1, "expected one {event} event: {events:?}");
        positions[0]
    };

    let index_start = position("index_flush:start");
    let index_end = position("index_flush:end");
    let lsp_start = position("lsp_shutdown:start");
    let lsp_end = position("lsp_shutdown:end");
    assert!(
        index_start < index_end,
        "invalid index flush interval: {events:?}"
    );
    assert!(
        lsp_start < lsp_end,
        "invalid LSP shutdown interval: {events:?}"
    );
    assert!(
        index_start < lsp_end && lsp_start < index_end,
        "index flush and LSP shutdown intervals did not intersect: {events:?}"
    );
}

pub(super) fn write_user_config(config_home: &Path, storage: &Path) {
    let config_dir = config_home.join("cortexkit");
    std::fs::create_dir_all(&config_dir).expect("create user config dir");
    std::fs::write(
        config_dir.join("aft.jsonc"),
        serde_json::to_string(&json!({
            "storage_dir": storage,
            "bash": { "background": true },
            "indexes": { "trigram": false, "semantic": false, "callgraph": false },
        }))
        .expect("serialize user config"),
    )
    .expect("write user config");
}

pub(super) async fn write_connection_file(conn_dir: &Path) -> TcpListener {
    // The connection-file contract rejects writable shared parents, even
    // when a permissive runner umask gives its temporary directories 0775.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(conn_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    std_listener
        .set_nonblocking(true)
        .expect("set fake daemon nonblocking");
    let port = std_listener.local_addr().expect("fake daemon addr").port();
    let conn_path = conn_dir.join("subc-connection.json");
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_string(),
            port,
        }],
        key: vec![0x42; subc_transport::KEY_LEN],
        daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
        pid: std::process::id(),
        daemon_ver: "subc-detach-test".to_string(),
    };
    connection_file::write_atomic(&conn_path, &conn).expect("write connection file");
    TcpListener::from_std(std_listener).expect("tokio listener")
}

pub(super) struct ModuleProcess {
    pub(super) child: Child,
}

impl ModuleProcess {
    fn spawn(conn_path: &Path, config_home: &Path, data_home: &Path) -> Self {
        Self::spawn_with_stderr(conn_path, config_home, data_home, None)
    }

    /// Spawns the module, sending its stderr (which carries every log line)
    /// to `stderr_path` when given.
    fn spawn_with_stderr(
        conn_path: &Path,
        config_home: &Path,
        data_home: &Path,
        stderr_path: Option<&Path>,
    ) -> Self {
        Self::spawn_with_stderr_and_path(conn_path, config_home, data_home, stderr_path, None)
    }

    fn spawn_with_stderr_and_path(
        conn_path: &Path,
        config_home: &Path,
        data_home: &Path,
        stderr_path: Option<&Path>,
        bin_dir: Option<&Path>,
    ) -> Self {
        Self::spawn_with_exit_hooks(
            conn_path,
            config_home,
            data_home,
            stderr_path,
            bin_dir,
            false,
            false,
            "40",
            None,
            None,
            None,
        )
    }

    fn spawn_with_exit_hooks(
        conn_path: &Path,
        config_home: &Path,
        data_home: &Path,
        stderr_path: Option<&Path>,
        bin_dir: Option<&Path>,
        active_ort: bool,
        busy_index: bool,
        writer_delay: &str,
        flush_hold: Option<&str>,
        index_delay: Option<&str>,
        phase_events_path: Option<&Path>,
    ) -> Self {
        use std::os::unix::process::CommandExt;

        let stderr = match stderr_path {
            Some(path) => {
                Stdio::from(std::fs::File::create(path).expect("create module stderr file"))
            }
            None => Stdio::null(),
        };
        let binary = std::env::var_os("AFT_TEST_AFT_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")));
        let mut command = Command::new(binary);
        command
            .arg("--subc")
            .arg(conn_path)
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .env("XDG_CONFIG_HOME", config_home)
            .env("XDG_DATA_HOME", data_home)
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        if active_ort {
            command.env("AFT_TEST_HOLD_ORT_SECTION", "1");
        }
        if busy_index {
            command.env("AFT_CACHE_DIR", data_home);
            command.env("AFT_TEST_EXIT_INDEX_DELAY_MS", "450");
            command.env("AFT_TEST_LOG_WRITER_DELAY_MS", writer_delay);
        }
        if let Some(delay) = index_delay {
            command.env("AFT_TEST_EXIT_INDEX_DELAY_MS", delay);
        }
        if let Some(path) = phase_events_path {
            command.env("AFT_TEST_EXIT_PHASE_EVENTS", path);
        }
        if let Some(hold) = flush_hold {
            // A writer this slow cannot acknowledge a flush inside the exit
            // budget, wherever in its batch delay the request lands.
            command.env("AFT_TEST_LOG_FLUSH_HOLD_MS", hold);
        }
        if let Some(bin_dir) = bin_dir {
            // The test supplies its language servers through `bin_dir`. An
            // `AFT_LSP_<KIND>_BINARY` override inherited from the runner's
            // environment (set, for example, to run the real rust-analyzer
            // suites) would take precedence over PATH, start the real server
            // instead of the fake one, and leave the fake servers' pid files
            // unwritten.
            for (key, _) in std::env::vars_os() {
                let key = key.to_string_lossy();
                if key.starts_with("AFT_LSP_") && key.ends_with("_BINARY") {
                    command.env_remove(key.as_ref());
                }
            }
            command.env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().expect("spawn aft --subc module"),
        }
    }

    fn pid_i32(&self) -> i32 {
        i32::try_from(self.child.id()).expect("module pid fits i32")
    }

    fn terminate_process_group(&mut self, signal: i32) -> ExitStatus {
        let rc = unsafe { libc::killpg(self.pid_i32(), signal) };
        assert_eq!(
            rc,
            0,
            "failed to signal module process group: {}",
            std::io::Error::last_os_error()
        );
        self.wait_for_exit("module process-group termination")
    }

    pub(super) fn wait_for_exit(&mut self, label: &str) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Ok(None) => {
                    let _ = unsafe { libc::killpg(self.pid_i32(), libc::SIGKILL) };
                    let _ = self.child.wait();
                    panic!("timed out waiting for {label}");
                }
                Err(error) => panic!("wait for {label}: {error}"),
            }
        }
    }
}

impl Drop for ModuleProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = unsafe { libc::killpg(self.pid_i32(), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

async fn accept_module(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("timed out accepting module connection")
        .expect("accept module connection");
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
    let _hello_body: ModuleHelloBody = serde_json::from_slice(&hello.body).expect("hello body");
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
    stream
}

pub(super) async fn bind_route(stream: &mut TcpStream, root: &Path) {
    bind_route_as(stream, root, ROUTE_CHANNEL, "opencode").await;
}

async fn bind_route_as(stream: &mut TcpStream, root: &Path, channel: u16, harness: &str) {
    let project_cfg = root.join(".cortexkit").join("aft.jsonc");
    std::fs::create_dir_all(project_cfg.parent().expect("project config parent"))
        .expect("create project config dir");
    std::fs::write(
        &project_cfg,
        serde_json::to_string(&json!({
            "indexes": { "trigram": false, "semantic": false, "callgraph": false },
        }))
        .expect("serialize project config"),
    )
    .expect("write project config");

    let request = ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".to_string(),
        },
        identity: BindIdentity::new(
            root.to_path_buf(),
            harness.to_string(),
            SESSION_ID.to_string(),
        ),
        principal: Some(Principal::Direct),
        consumer_capabilities: None,
        admission_facts: Default::default(),
        scope: None,
        role_versions: None,
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            10,
            serde_json::to_vec(&request).expect("route bind body"),
        )
        .expect("route bind frame"),
    )
    .await;

    let ack = read_frame_timeout(stream, "RouteBindAck").await;
    assert_eq!(ack.header.ty, FrameType::Response);
    assert_eq!(ack.header.channel, 0);
    assert_eq!(ack.header.corr, 10);
    let body: ModuleControlResponse = serde_json::from_slice(&ack.body).expect("ack body");
    assert_eq!(body, ModuleControlResponse::RouteBindAck {});
}

pub(super) async fn send_tool_call(
    stream: &mut TcpStream,
    channel: u16,
    corr: u64,
    name: &str,
    arguments: Value,
) {
    let body = json!({ "name": name, "arguments": arguments });
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            channel,
            1,
            corr,
            serde_json::to_vec(&body).expect("tool call body"),
        )
        .expect("tool call frame"),
    )
    .await;
}

async fn bash_status(stream: &mut TcpStream, corr: u64, task_id: &str) -> Value {
    send_tool_call(
        stream,
        ROUTE_CHANNEL,
        corr,
        "bash_status",
        json!({ "params": { "task_id": task_id } }),
    )
    .await;
    let frame = read_tool_response(stream, corr, "bash_status response").await;
    assert!(
        !tool_result_is_error(&frame),
        "bash_status failed: {}",
        frame_body(&frame)
    );
    tool_response_json(&frame)
}

pub(super) async fn wait_for_status(
    stream: &mut TcpStream,
    start_corr: u64,
    task_id: &str,
    expected: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut corr = start_corr;
    loop {
        let status = bash_status(stream, corr, task_id).await;
        if status["status"] == expected {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected}: {status}"
        );
        corr += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn send_connection_goodbye(stream: &mut TcpStream) {
    send_frame(
        stream,
        Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 99, Vec::new())
            .expect("goodbye frame"),
    )
    .await;
}

/// Sends the daemon's one-way `module.draining` notice (a channel-0 Push), the
/// first step of a supervisor restart or a daemon shutdown.
async fn send_module_draining(stream: &mut TcpStream) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock after epoch")
        .as_millis() as u64;
    let body = json!({
        "op": "module.draining",
        "reason": "restart",
        "deadline_ms": now_ms + 30_000,
    });
    send_frame(
        stream,
        Frame::build(
            FrameType::Push,
            control_flags(),
            0,
            0,
            0,
            serde_json::to_vec(&body).expect("module.draining body"),
        )
        .expect("module.draining frame"),
    )
    .await;
}

pub(super) async fn send_frame(stream: &mut TcpStream, frame: Frame) {
    write_frame(stream, &frame).await.expect("write frame");
}

pub(super) async fn read_any_frame_timeout(stream: &mut TcpStream, label: &str) -> Frame {
    tokio::time::timeout(Duration::from_secs(30), read_frame(stream))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .expect("read frame")
        .unwrap_or_else(|| panic!("EOF waiting for {label}"))
}

async fn read_frame_timeout(stream: &mut TcpStream, label: &str) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for {label}");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for {label}"));
        if frame.header.ty != FrameType::Push {
            return frame;
        }
    }
}

pub(super) async fn read_tool_response(stream: &mut TcpStream, corr: u64, label: &str) -> Frame {
    let frame = read_frame_timeout(stream, label).await;
    assert_eq!(frame.header.ty, FrameType::Response, "{label} frame type");
    assert_eq!(frame.header.channel, ROUTE_CHANNEL, "{label} channel");
    assert_eq!(frame.header.corr, corr, "{label} corr");
    frame
}

pub(super) fn tool_response_json(frame: &Frame) -> Value {
    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    let structured = &body["structuredContent"];
    assert!(
        structured.is_object(),
        "tool response missing structuredContent envelope: {body}"
    );
    structured.clone()
}

pub(super) fn tool_result_is_error(frame: &Frame) -> bool {
    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    body["isError"].as_bool().unwrap_or(false)
}

pub(super) fn frame_body(frame: &Frame) -> String {
    String::from_utf8_lossy(&frame.body).into_owned()
}

pub(super) fn extract_task_id(frame: &Frame) -> String {
    let structured = tool_response_json(frame);
    if let Some(task_id) = structured.get("task_id").and_then(Value::as_str) {
        return task_id.to_string();
    }

    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    let text = body["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    let start = text
        .find("bash-")
        .unwrap_or_else(|| panic!("no bash task id in {text:?}"));
    let tail = &text[start..];
    let end = tail
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .unwrap_or(tail.len());
    tail[..end].to_string()
}

fn sentinel_command(ready: &Path, stop: &Path) -> String {
    format!(
        "printf 'sentinel-started\\n'; touch {ready}; polls=0; while [ ! -f {stop} ] && [ \"$polls\" -lt 6000 ]; do sleep 0.05; polls=$((polls + 1)); done; if [ -f {stop} ]; then printf 'sentinel-stopped\\n'; else printf 'gate-timeout\\n'; fi",
        ready = shell_quote(ready),
        stop = shell_quote(stop),
    )
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wait_for_path(path: &Path, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn assert_process_alive(pid: u32, label: &str) {
    let pid = i32::try_from(pid).expect("pid fits i32");
    let alive = unsafe { libc::kill(pid, 0) == 0 }
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    assert!(alive, "{label} should still be alive (pid {pid})");
}

pub(super) fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}
