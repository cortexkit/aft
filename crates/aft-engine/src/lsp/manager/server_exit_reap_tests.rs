use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use super::LspManager;
use crate::config::Config;
use crate::lsp::child_registry::LspChildRegistry;
use crate::lsp::client::{LspClient, LspEvent, LspEventSender, ServerExitReason};
use crate::lsp::registry::ServerKind;

#[cfg(unix)]
fn spawn_session_sleep() -> Child {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("sh");
    command.args(["-c", "exec sleep 60"]);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn session-leader sleep")
}

#[cfg(unix)]
fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !crate::process_identity::is_process_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

#[cfg(unix)]
fn spawn_malformed_then_sleep_client(
    event_tx: LspEventSender,
    registry: LspChildRegistry,
    root: std::path::PathBuf,
) -> LspClient {
    LspClient::spawn_with_reclaim_root(
        ServerKind::TypeScript(None),
        root,
        Path::new("sh"),
        &[
            "-c".to_string(),
            "printf 'Content-Length: invalid\r\n\r\n'; exec sleep 60".to_string(),
        ],
        &HashMap::new(),
        event_tx,
        registry,
        None,
        false,
    )
    .expect("spawn malformed-then-sleep stand-in")
}

#[cfg(unix)]
#[test]
fn server_exited_handler_kills_live_child_and_untracks() {
    let registry = LspChildRegistry::new();
    let mut manager = LspManager::new();
    manager.set_child_registry(registry.clone());
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let mut client = spawn_malformed_then_sleep_client(
        manager.event_sender_for_test(),
        registry.clone(),
        root.clone(),
    );
    let pid = client.child_pid();
    client.suppress_kill_on_drop_for_test();
    manager.insert_client_for_test(client);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_exit = false;
    while Instant::now() < deadline {
        let drained = manager.drain_events();
        if drained.events.iter().any(|event| {
            matches!(
                event,
                LspEvent::ServerExited {
                    reason: ServerExitReason::ReadError(_),
                    ..
                }
            )
        }) {
            saw_exit = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(saw_exit, "reader must emit ServerExited with ReadError");
    assert!(
        wait_until_dead(pid, Duration::from_secs(5)),
        "ServerExited handler must kill the still-running child"
    );
    assert!(
        !registry.pids().contains(&pid),
        "ServerExited handler must untrack the child"
    );
    assert_eq!(manager.active_client_count(), 0);
}

#[cfg(unix)]
#[test]
fn lifecycle_census_reports_dropped_client_until_reaper_cleans_child() {
    let registry = LspChildRegistry::new();
    let (events, _event_rx) = crossbeam_channel::unbounded();
    let root = tempfile::tempdir().expect("tempdir");
    let mut client = spawn_malformed_then_sleep_client(
        LspEventSender::Standalone(events),
        registry.clone(),
        root.path().to_path_buf(),
    );
    let pid = client.child_pid() as libc::pid_t;
    client.suppress_kill_on_drop_for_test();
    drop(client);

    let leaked = registry.health_snapshot();
    assert_eq!(leaked.children_without_client, 1);
    assert_eq!(leaked.children_total, 1);
    assert_eq!(registry.reap_children_without_client(), 0);
    assert_eq!(registry.pids(), vec![pid as u32]);
    // Signal delivery is not a reap. The fixture takes over the wait
    // that the test deliberately skipped when dropping the client.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            break;
        }
        assert_eq!(waited, 0, "{}", std::io::Error::last_os_error());
        assert!(
            Instant::now() < deadline,
            "orphan did not exit after SIGTERM"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(libc::WIFSIGNALED(status));
    assert_eq!(libc::WTERMSIG(status), libc::SIGTERM);
    assert_eq!(registry.reap_children_without_client(), 1);
    assert_eq!(registry.health_snapshot().children_without_client, 0);
    assert_eq!(registry.health_snapshot().children_total, 0);
}

#[cfg(unix)]
#[test]
fn ensure_server_reaps_unreferenced_children_before_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    let main_rs = src.join("main.rs");
    std::fs::write(&main_rs, "fn main() {}\n").unwrap();
    let config = Config::default();
    let rust_def = crate::lsp::registry::servers_for_file(&main_rs, &config)
        .into_iter()
        .find(|def| matches!(def.kind, ServerKind::Rust))
        .expect("rust server applies to main.rs");
    let key = super::server_key_for_definition(&rust_def, &main_rs, &config)
        .expect("rust workspace root");

    let registry = LspChildRegistry::new();
    let mut first = spawn_session_sleep();
    let mut second = spawn_session_sleep();
    let pid1 = first.id();
    let pid2 = second.id();
    registry.track_child(pid1, Some(&key.root), Some(&key.root), Some(&key.kind));
    registry.track_child(pid2, Some(&key.root), Some(&key.root), Some(&key.kind));

    let mut manager = LspManager::new();
    manager.set_child_registry(registry.clone());
    manager.override_binary(ServerKind::Rust, Path::new("false").to_path_buf());
    assert_eq!(
        registry.pids_for_server(&key.root, &key.kind).len(),
        2,
        "both orphans must be registered under the spawn key"
    );
    let _ = crate::lsp::manager::settled_lsp(&mut manager, |startup_manager| {
        startup_manager.ensure_server_for_file(&main_rs, &config)
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let first_exited = first.try_wait().ok().flatten().is_some();
        let second_exited = second.try_wait().ok().flatten().is_some();
        if first_exited && second_exited {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "orphans must exit after rebind reap (first alive={}, second alive={})",
            crate::process_identity::is_process_alive(pid1),
            crate::process_identity::is_process_alive(pid2)
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(registry.reap_children_without_client(), 2);
    assert!(
        !registry.pids().contains(&pid1) && !registry.pids().contains(&pid2),
        "the registry must forget the orphans once their groups are gone"
    );
}
