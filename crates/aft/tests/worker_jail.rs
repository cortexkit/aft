//! A daemon-authenticated background worker gets a native sandbox, even when
//! ordinary sessions have sandboxing disabled. Drive the real `aft --subc`
//! process through a fake daemon that binds such a worker route, then check its
//! shell's allowed work and denied access. Seatbelt runs on macOS; hook and
//! temporary-file probes also run on Linux when Landlock is available.
//! Everything lives under one throwaway HOME and XDG layout, never the live
//! CortexKit data store. Every daemon has a HOME, even on CI runners without one.
#![cfg(unix)]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION},
    read_frame, write_frame,
};
use tokio::net::{TcpListener, TcpStream};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;
#[path = "helpers/executable_cache.rs"]
mod executable_cache;
#[path = "helpers/worker_gh_read.rs"]
mod worker_gh_read;

fn flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn run(command: &mut Command) -> String {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    run(Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.com",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("HOME", dir))
}

struct Jail {
    _dir: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    main: PathBuf,
    worktree: PathBuf,
    #[cfg(target_os = "macos")]
    other_repo: PathBuf,
    data: PathBuf,
    #[cfg(target_os = "macos")]
    rustup: PathBuf,
    child: Child,
    stream: TcpStream,
    next_corr: u64,
    pending_frames: std::collections::VecDeque<Frame>,
    read_holder: std::sync::Arc<std::sync::Mutex<worker_gh_read::ReadHolder>>,
    read_router: tokio::task::JoinHandle<()>,
}

impl Drop for Jail {
    fn drop(&mut self) {
        let process_groups = sandbox_task_process_groups(&self.data);
        self.read_router.abort();
        let _ = self.child.kill();
        let _ = self.child.wait();

        // The daemon is killed on fixture teardown, but each detached shell
        // owns a new process group and can outlive that direct child.
        let (leaked, remaining) = cleanup_sandbox_task_process_groups(&process_groups);
        if !std::thread::panicking() {
            assert!(
                leaked.is_empty(),
                "sandbox launcher process groups outlived the worker daemon: {leaked:?}"
            );
            assert!(
                remaining.is_empty(),
                "sandbox launcher process groups survived fixture cleanup: {remaining:?}"
            );
        }
    }
}

#[derive(Debug)]
struct RecordedSandboxProcessGroup {
    task_id: String,
    child_pid: Option<u32>,
    pgid: i32,
    child_start_time_ms: Option<u64>,
    terminal: bool,
}

fn process_group_skip_reason(group: &RecordedSandboxProcessGroup) -> Option<&'static str> {
    if group.terminal {
        return Some("task is already terminal");
    }
    let Some(child_pid) = group.child_pid else {
        return Some("task has no recorded child PID");
    };
    if child_pid != group.pgid as u32 {
        return Some("recorded child PID is not the process-group leader");
    }
    let Some(expected_start_time_ms) = group.child_start_time_ms else {
        return Some("process start time was not recorded at spawn");
    };
    let Some(actual_start_time_ms) = aft::root_cache::process_start_time_ms(child_pid) else {
        return Some("process-group leader is absent or its start time is unavailable");
    };
    (actual_start_time_ms != expected_start_time_ms)
        .then_some("process-group leader start time differs from spawn time")
}

fn cleanup_sandbox_task_process_groups(
    process_groups: &[RecordedSandboxProcessGroup],
) -> (Vec<i32>, Vec<i32>) {
    let mut owned_groups = std::collections::BTreeSet::new();
    for group in process_groups {
        if let Some(reason) = process_group_skip_reason(group) {
            eprintln!(
                "worker-jail teardown skipped process group {} for task {}: {reason}",
                group.pgid, group.task_id
            );
        } else {
            owned_groups.insert(group.pgid);
        }
    }
    let owned_groups = owned_groups.into_iter().collect::<Vec<_>>();
    let leaked = wait_for_process_groups(&owned_groups, Duration::from_millis(250));
    for pgid in &owned_groups {
        unsafe {
            libc::kill(-*pgid, libc::SIGKILL);
        }
    }
    let remaining = wait_for_process_groups(&owned_groups, Duration::from_secs(2));
    (leaked, remaining)
}

fn sandbox_task_process_groups(data: &Path) -> Vec<RecordedSandboxProcessGroup> {
    let root = data.join("cortexkit/aft/cache/aft/runner/bash-tasks");
    let mut pending = vec![root];
    let mut groups = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                let Ok(task) = serde_json::from_slice::<
                    aft::bash_background::persistence::PersistedTask,
                >(&bytes) else {
                    continue;
                };
                if task.sandbox_native {
                    if let Some(pgid) = task.pgid.filter(|pgid| *pgid > 1) {
                        groups.push(RecordedSandboxProcessGroup {
                            task_id: task.task_id,
                            child_pid: task.child_pid,
                            pgid,
                            child_start_time_ms: task.child_start_time_ms,
                            terminal: task.status.is_terminal(),
                        });
                    }
                }
            }
        }
    }
    groups
}

fn process_group_has_live_members(pgid: i32) -> bool {
    let Ok(output) = Command::new("ps").args(["-axo", "pgid=,stat="]).output() else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split_whitespace();
        matches!(
            (fields.next().and_then(|field| field.parse::<i32>().ok()), fields.next()),
            (Some(group), Some(state)) if group == pgid && !state.starts_with('Z')
        )
    })
}

fn wait_for_process_groups(groups: &[i32], timeout: Duration) -> Vec<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        let alive = groups
            .iter()
            .copied()
            .filter(|pgid| process_group_has_live_members(*pgid))
            .collect::<Vec<_>>();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn persist_sandbox_task_for_test(
    data: &Path,
    task: &aft::bash_background::persistence::PersistedTask,
) {
    let session_dir = data.join("cortexkit/aft/cache/aft/runner/bash-tasks/session");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join(format!("{}.json", task.task_id)),
        serde_json::to_vec(task).unwrap(),
    )
    .unwrap();
}

struct KillProcessGroupOnDrop(i32);

impl Drop for KillProcessGroupOnDrop {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

#[test]
fn worker_jail_teardown_kills_recorded_sandbox_process_groups() {
    use std::os::unix::process::CommandExt;

    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let release = temp.path().join("release");
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            &format!("while ! test -f {}; do sleep .02; done", quote(&release)),
        ])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = i32::try_from(child.id()).unwrap();
    let _kill_group_on_drop = KillProcessGroupOnDrop(pgid);
    assert!(process_group_has_live_members(pgid));

    let mut task = aft::bash_background::persistence::PersistedTask::starting(
        "bash-0000000000000001".into(),
        "session".into(),
        "test shell".into(),
        temp.path().to_path_buf(),
        Some(temp.path().to_path_buf()),
        Some(60_000),
        false,
        false,
    );
    task.sandbox_native = true;
    task.mark_running(child.id(), pgid);
    assert!(task.child_start_time_ms.is_some());
    persist_sandbox_task_for_test(&data, &task);

    let groups = sandbox_task_process_groups(&data);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].pgid, pgid);
    let (leaked, remaining) = cleanup_sandbox_task_process_groups(&groups);
    assert_eq!(leaked, [pgid], "the live fixture shell must be detected");
    assert!(
        remaining.is_empty(),
        "teardown must kill every recorded group"
    );
    assert!(!process_group_has_live_members(pgid));
    assert!(!child.wait().unwrap().success());
}

#[test]
fn worker_jail_teardown_skips_reused_process_group() {
    use std::os::unix::process::CommandExt;

    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let release = temp.path().join("release");
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            &format!("while ! test -f {}; do sleep .02; done", quote(&release)),
        ])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = i32::try_from(child.id()).unwrap();
    let kill_group_on_drop = KillProcessGroupOnDrop(pgid);
    assert!(process_group_has_live_members(pgid));

    let mut task = aft::bash_background::persistence::PersistedTask::starting(
        "bash-0000000000000002".into(),
        "session".into(),
        "reused group fixture".into(),
        temp.path().to_path_buf(),
        Some(temp.path().to_path_buf()),
        Some(60_000),
        false,
        false,
    );
    task.sandbox_native = true;
    task.mark_running(child.id(), pgid);
    let spawn_start_time_ms = task.child_start_time_ms.expect("start time at spawn");
    task.child_start_time_ms = Some(spawn_start_time_ms.saturating_add(1));
    persist_sandbox_task_for_test(&data, &task);

    let groups = sandbox_task_process_groups(&data);
    let (leaked, remaining) = cleanup_sandbox_task_process_groups(&groups);
    assert!(leaked.is_empty(), "unowned groups are not checked as leaks");
    assert!(remaining.is_empty());
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, 0) },
        0,
        "a reused process group must not be signalled"
    );
    assert!(process_group_has_live_members(pgid));

    drop(kill_group_on_drop);
    assert!(!child.wait().unwrap().success());
}

#[test]
fn worker_jail_teardown_skips_terminal_process_group() {
    use std::os::unix::process::CommandExt;

    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let release = temp.path().join("release");
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            &format!("while ! test -f {}; do sleep .02; done", quote(&release)),
        ])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = i32::try_from(child.id()).unwrap();
    let kill_group_on_drop = KillProcessGroupOnDrop(pgid);

    let mut task = aft::bash_background::persistence::PersistedTask::starting(
        "bash-0000000000000003".into(),
        "session".into(),
        "terminal task fixture".into(),
        temp.path().to_path_buf(),
        Some(temp.path().to_path_buf()),
        Some(60_000),
        false,
        false,
    );
    task.sandbox_native = true;
    task.mark_running(child.id(), pgid);
    task.mark_terminal(aft::bash_background::BgTaskStatus::Completed, Some(0), None);
    persist_sandbox_task_for_test(&data, &task);

    let groups = sandbox_task_process_groups(&data);
    let (leaked, remaining) = cleanup_sandbox_task_process_groups(&groups);
    assert!(leaked.is_empty());
    assert!(remaining.is_empty());
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, 0) },
        0,
        "a terminal task's saved pgid must not be signalled"
    );
    assert!(process_group_has_live_members(pgid));

    drop(kill_group_on_drop);
    assert!(!child.wait().unwrap().success());
}

async fn next_frame(stream: &mut TcpStream) -> Frame {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(120), read_frame(stream))
            .await
            .expect("module answers in time")
            .expect("module frame")
            .expect("module stays connected");
        if frame.header.ty == FrameType::Ping {
            let pong =
                Frame::build(FrameType::Pong, flags(), 0, 0, frame.header.corr, vec![]).unwrap();
            write_frame(stream, &pong).await.unwrap();
            continue;
        }
        if frame.header.ty == FrameType::Push {
            continue;
        }
        return frame;
    }
}

impl Jail {
    async fn start() -> Self {
        Self::start_with_sccache(None, None).await
    }

    async fn start_with_sccache(port: Option<u16>, uds: Option<&Path>) -> Self {
        Self::start_with_options(port, uds, true).await
    }

    async fn start_with_options(port: Option<u16>, uds: Option<&Path>, pack_refs: bool) -> Self {
        Self::start_with_profile(port, uds, pack_refs, "strict", false).await
    }

    #[cfg(target_os = "macos")]
    async fn relaxed(standalone: bool) -> Self {
        Self::start_with_profile(None, None, true, "relaxed", standalone).await
    }

    #[cfg(target_os = "macos")]
    async fn relaxed_with_env(configure: impl FnOnce(&Path, &mut Command)) -> Self {
        Self::start_with_layout_and_env(None, None, true, "relaxed", false, false, configure).await
    }

    async fn start_with_profile(
        port: Option<u16>,
        uds: Option<&Path>,
        pack_refs: bool,
        profile: &str,
        standalone: bool,
    ) -> Self {
        Self::start_with_layout(port, uds, pack_refs, profile, standalone, false).await
    }

    async fn start_with_layout(
        port: Option<u16>,
        uds: Option<&Path>,
        pack_refs: bool,
        profile: &str,
        standalone: bool,
        custom_layout: bool,
    ) -> Self {
        Self::start_with_layout_and_env(
            port,
            uds,
            pack_refs,
            profile,
            standalone,
            custom_layout,
            |_, _| {},
        )
        .await
    }

    async fn start_with_layout_and_env(
        port: Option<u16>,
        uds: Option<&Path>,
        pack_refs: bool,
        profile: &str,
        standalone: bool,
        custom_layout: bool,
        configure: impl FnOnce(&Path, &mut Command),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let main = base.join("main");
        let data = home.join(".local/share");
        let worktree = if custom_layout {
            home.join("Work")
        } else {
            data.join("cortexkit/alfonso/worktrees/0123456789abcdef/worker")
        };
        let sibling = if custom_layout {
            worktree.join("sibling")
        } else {
            base.join("sibling")
        };
        let other_repo = base.join("other-repo");
        let rustup = base.join("rustup");
        let xdg = base.join("xdg");
        for path in [
            home.join(".ssh"),
            home.join(".cargo/bin"),
            home.join(".npm"),
            home.join(".bun/install/cache"),
            main.clone(),
            other_repo.clone(),
            rustup.clone(),
            base.join("lexar"),
            base.join("bin"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        for name in ["config", "data", "state", "cache"] {
            std::fs::create_dir_all(xdg.join(name)).unwrap();
        }
        std::fs::create_dir_all(data.join("cortexkit")).unwrap();

        // The main checkout, the worker's linked worktree and a sibling worktree.
        std::fs::write(main.join("README.md"), "main\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        // Keep automatic maintenance from adding unrelated shared-metadata
        // writes to these commit and permission probes.
        git(&main, &["config", "gc.auto", "0"]);
        git(&main, &["config", "maintenance.auto", "false"]);
        git(&main, &["add", "README.md"]);
        git(&main, &["commit", "-q", "-m", "initial"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "work",
                worktree.to_str().unwrap(),
            ],
        );
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "sibling",
                sibling.to_str().unwrap(),
            ],
        );
        std::fs::write(other_repo.join("README.md"), "other\n").unwrap();
        git(&other_repo, &["init", "-q"]);
        if standalone {
            std::fs::remove_file(worktree.join(".git")).unwrap();
            git(&worktree, &["init", "-q", "-b", "own"]);
            git(&worktree, &["add", "README.md"]);
            git(&worktree, &["commit", "-q", "-m", "own initial"]);
        }

        // A local bare remote with one commit the worker has not fetched yet,
        // and packed refs in the main repository, as a clone would have.
        let remote = base.join("remote.git");
        let upstream = base.join("upstream");
        git(
            &base,
            &[
                "clone",
                "-q",
                "--bare",
                main.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git(
            &base,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                upstream.to_str().unwrap(),
            ],
        );
        std::fs::write(upstream.join("UPSTREAM.md"), "upstream\n").unwrap();
        git(&upstream, &["add", "UPSTREAM.md"]);
        git(&upstream, &["commit", "-q", "-m", "upstream change"]);
        git(&upstream, &["push", "-q", "origin", "main"]);
        git(
            &main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        if pack_refs {
            git(&main, &["pack-refs", "--all"]);
        }

        // Co-author attribution on, so AFT's managed Git hooks are in play.
        std::fs::create_dir_all(xdg.join("config/cortexkit")).unwrap();
        std::fs::write(
            xdg.join("config/cortexkit/aft.jsonc"),
            format!(r#"{{"sandbox":{{"profile":{{"worker":"{profile}"}}}},"git":{{"co_author":"Jail Bot <jail@example.com>"}}}}"#),
        )
        .unwrap();

        // A tiny crate in the worktree, and operator cargo configuration
        // (outside the worktree) naming a shared target and an sccache wrapper.
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[package]\nname = \"jailcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(worktree.join("src/main.rs"), "fn main() {}\n").unwrap();
        let wrapper = base.join("bin/rustc-wrapper");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\n# Stands in for a compile wrapper such as sccache: records that it ran.\necho used >> {}\nexec \"$@\"\n",
                quote(&worktree.join("wrapper-used"))
            ),
        )
        .unwrap();
        run(Command::new("chmod").arg("755").arg(&wrapper));
        std::fs::write(
            home.join(".cargo/config.toml"),
            format!(
                "[build]\ntarget-dir = {:?}\nrustc-wrapper = {:?}\n",
                base.join("shared-target").display().to_string(),
                wrapper.display().to_string()
            ),
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
            pid: std::process::id(),
            daemon_ver: "worker-jail".into(),
        };
        let connection_path = if custom_layout {
            let auth = base.join("custom-relay/auth");
            std::fs::create_dir_all(&auth).unwrap();
            auth.join("connection.json")
        } else {
            base.join("connection.json")
        };
        connection_file::write_atomic(&connection_path, &connection).unwrap();
        let mut command = Command::new(aft_binary::aft_binary());
        command
            .current_dir(&base)
            .arg("--subc")
            .arg(&connection_path)
            .env("SUBC_CONNECTION_FILE", &connection_path)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_DATA_HOME", &data)
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .env("AFT_CACHE_DIR", data.join("cortexkit/aft/cache"))
            .env("RUSTUP_HOME", &rustup)
            // An operator-set shared target in the daemon's environment.
            .env("CARGO_TARGET_DIR", base.join("env-target"))
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("AFT_STORAGE_DIR")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("CARGO_HOME")
            .env_remove("SCCACHE_DIR")
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
            .env_remove("SCCACHE_SERVER_PORT")
            .env_remove("SCCACHE_SERVER_UDS");
        if custom_layout {
            let cargo = base.join("custom-cargo");
            std::fs::create_dir_all(&cargo).unwrap();
            for name in ["credentials", "credentials.toml"] {
                std::fs::write(cargo.join(name), "custom-cargo-secret").unwrap();
            }
            command.env("CARGO_HOME", cargo);
        }
        if let Some(port) = port {
            command.env("SCCACHE_SERVER_PORT", port.to_string());
        }
        if let Some(uds) = uds {
            command.env("SCCACHE_SERVER_UDS", uds);
        }
        configure(&base, &mut command);
        let child = command.spawn().unwrap();
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .unwrap()
            .unwrap();
        authenticate_server(
            &mut stream,
            &[0x42; subc_transport::KEY_LEN],
            &[0x24; subc_transport::DAEMON_ID_LEN],
            "worker-jail",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let hello = next_frame(&mut stream).await;
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec![],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut stream,
            &Frame::build(
                FrameType::HelloAck,
                flags(),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ack).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let read_holder =
            std::sync::Arc::new(std::sync::Mutex::new(worker_gh_read::ReadHolder::default()));
        let read_router = tokio::spawn(worker_gh_read::serve(listener, read_holder.clone()));
        let mut jail = Self {
            _dir: dir,
            base,
            home,
            main,
            worktree,
            #[cfg(target_os = "macos")]
            other_repo,
            data,
            #[cfg(target_os = "macos")]
            rustup,
            child,
            stream,
            next_corr: 100,
            pending_frames: std::collections::VecDeque::new(),
            read_holder,
            read_router,
        };
        jail.bind_worker().await;
        jail
    }

    /// Bind channel 1 the way the daemon binds a worker route: principal
    /// reserved:broca, scope owned by reserved:prefrontal-core, kind worker,
    /// with its head as parent.
    async fn bind_worker(&mut self) {
        let owner = Principal::Reserved {
            module_id: "prefrontal-core".into(),
        };
        let scope: subc_protocol::scope::ScopeStamp = serde_json::from_value(json!({
            "owner": owner,
            "ref": "worker-scope",
            "scope_epoch": 3,
            "kind": "worker",
            "parent": {"owner": owner, "ref": "head-scope", "scope_epoch": 7},
            "owner_authorized": true,
            "attributes": {"agent_id":"worker-read-principal", "delegates":true},
        }))
        .unwrap();
        let bind = ModuleControlRequest::RouteBind {
            route_channel: 1,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new(&self.worktree, "runner", "worker-session"),
            principal: Some(Principal::Reserved {
                module_id: "broca".into(),
            }),
            consumer_capabilities: None,
            admission_facts: None,
            scope: Some(scope),
            role_versions: Some(std::collections::BTreeMap::from([(
                "tool-provider".into(),
                "v1".into(),
            )])),
            call_mode: None,
        };
        self.bind(bind).await;
    }

    async fn bind_grant_head(&mut self) {
        let root = self.base.clone();
        self.bind(ModuleControlRequest::RouteBind {
            route_channel: 2,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new(&root, "runner", "head-session"),
            principal: Some(Principal::Reserved {
                module_id: "broca".into(),
            }),
            consumer_capabilities: None,
            admission_facts: None,
            scope: Some(
                serde_json::from_value(json!({
                    "owner": {"kind":"reserved","module_id":"prefrontal-core"},
                    "ref":"head-scope", "scope_epoch":7, "kind":"head",
                    "owner_authorized":true, "attributes":{"agent_id":"head"},
                }))
                .unwrap(),
            ),
            role_versions: Some(std::collections::BTreeMap::from([(
                "tool-provider".into(),
                "v1".into(),
            )])),
            call_mode: None,
        })
        .await;
    }

    async fn bind(&mut self, bind: ModuleControlRequest) {
        let binding = match &bind {
            ModuleControlRequest::RouteBind {
                identity, scope, ..
            } => Some((identity.session.clone(), scope.clone())),
            _ => None,
        };
        let corr = self.next_corr;
        self.next_corr += 1;
        write_frame(
            &mut self.stream,
            &Frame::build(
                FrameType::Request,
                flags(),
                0,
                0,
                corr,
                serde_json::to_vec(&bind).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ack = self.reply_frame(0, corr).await;
        assert_eq!(ack.header.corr, corr);
        let response: ModuleControlResponse =
            serde_json::from_slice(&ack.body).unwrap_or_else(|error| {
                panic!(
                    "invalid bind response: {error}: {}",
                    String::from_utf8_lossy(&ack.body)
                )
            });
        assert_eq!(response, ModuleControlResponse::RouteBindAck {});
        if let Some((session, scope)) = binding {
            let mut holder = self.read_holder.lock().unwrap();
            holder.binds.remove(&session);
            if let Some(scope) = scope {
                holder.binds.insert(session, scope);
            }
        }
    }

    async fn reply_frame(&mut self, channel: u16, corr: u64) -> Frame {
        if let Some(index) = self
            .pending_frames
            .iter()
            .position(|frame| frame.header.channel == channel && frame.header.corr == corr)
        {
            return self.pending_frames.remove(index).unwrap();
        }
        loop {
            let frame = next_frame(&mut self.stream).await;
            if frame.header.channel == channel && frame.header.corr == corr {
                return frame;
            }
            self.pending_frames.push_back(frame);
        }
    }

    async fn allow_paths(&mut self, paths: Value) {
        for channel in [2, 1] {
            let corr = self.next_corr;
            self.next_corr += 1;
            write_frame(
                &mut self.stream,
                &Frame::build(
                    FrameType::Request,
                    flags(),
                    channel,
                    1,
                    corr,
                    serde_json::to_vec(&json!({
                        "name":"tool.catalog",
                        "arguments":{"preset":"worker","params":{"allow_paths":paths}},
                    }))
                    .unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            let frame = self.reply_frame(channel, corr).await;
            assert_eq!(
                frame.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&frame.body)
            );
        }
    }

    async fn submit_tool(&mut self, name: &str, arguments: Value) -> u64 {
        self.submit_tool_on_channel(1, name, arguments).await
    }

    async fn submit_held_bash(&mut self, arguments: Value) -> u64 {
        let warm = self.tool_reply("bash", json!({"command":"true"})).await;
        assert_eq!(warm["structuredContent"]["exit_code"], 0, "{warm}");
        self.submit_tool("bash", arguments).await
    }

    async fn submit_tool_on_channel(&mut self, channel: u16, name: &str, arguments: Value) -> u64 {
        let corr = self.next_corr;
        self.next_corr += 1;
        let body = json!({
            "name": name,
            "preset": "worker",
            "arguments": arguments,
        });
        write_frame(
            &mut self.stream,
            &Frame::build(
                FrameType::Request,
                flags(),
                channel,
                1,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        corr
    }

    async fn tool_reply(&mut self, name: &str, arguments: Value) -> Value {
        self.tool_reply_on_channel(1, name, arguments).await
    }

    async fn read_file(&mut self, path: &Path) -> Value {
        self.tool_reply("read", json!({"path":path})).await
    }

    async fn head_shell_read(&mut self, path: &Path) -> Value {
        // A bare cat can be rewritten into AFT read. Send this compound command
        // over the head-scoped Bash channel to check shell confinement, not the
        // read tool's session ACL.
        self.tool_reply_on_channel(
            2,
            "bash",
            json!({"command":format!("printf 'HEAD_SHELL_READ\\n'; cat {}", quote(path))}),
        )
        .await
    }

    async fn catalog_on_channel(&mut self, channel: u16) -> Value {
        let corr = self.next_corr;
        self.next_corr += 1;
        write_frame(
            &mut self.stream,
            &Frame::build(
                FrameType::Request,
                flags(),
                channel,
                1,
                corr,
                serde_json::to_vec(&json!({"op":"tool.catalog","preset":"worker"})).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let frame = self.reply_frame(channel, corr).await;
        assert_eq!(
            frame.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&frame.body)
        );
        serde_json::from_slice(&frame.body).unwrap()
    }

    async fn tool_reply_on_channel(&mut self, channel: u16, name: &str, arguments: Value) -> Value {
        for _ in 0..200 {
            let corr = self
                .submit_tool_on_channel(channel, name, arguments.clone())
                .await;
            let frame = self.reply_frame(channel, corr).await;
            let text = String::from_utf8_lossy(&frame.body).into_owned();
            if text.contains("database_initializing")
                || text.contains("Project persistence is still initializing")
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            return serde_json::from_slice(&frame.body).expect("JSON tool reply");
        }
        panic!("database initialization never completed");
    }

    async fn tool(&mut self, name: &str, arguments: Value) -> String {
        let value = self.tool_reply(name, arguments).await;
        let content = value["content"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        format!("{content}\n{value}")
    }

    /// Run one shell command as the worker and return the reply as text.
    async fn bash(&mut self, command: &str) -> String {
        self.tool("bash", json!({"command": command, "timeout": 300_000}))
            .await
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_allow_paths_bash_writes_work_grant_but_not_siblings() {
    for profile in ["relaxed", "strict"] {
        let mut jail = Jail::start_with_profile(None, None, true, profile, false).await;
        jail.bind_grant_head().await;
        // Wait for persistence before catalog materializes the worker's grants.
        let ready = jail.bash("printf READY").await;
        assert!(ready.contains("READY"), "{ready}");
        let granted = jail.home.join("Work/granted");
        let sibling = jail.home.join("Work/ungranted");
        let nested = jail.main.join("granted");
        for path in [&granted, &sibling, &nested] {
            std::fs::create_dir_all(path).unwrap();
        }
        jail.allow_paths(json!([
            {"path":granted,"mode":"write"}, {"path":nested,"mode":"write"},
        ]))
        .await;
        let output = jail.bash(&format!(
            "printf granted > {}/result && echo GRANT_WRITABLE; printf nested > {}/result && echo NESTED_WRITABLE; printf sibling > {}/result; printf checkout > {}/ungranted-result",
            quote(&granted), quote(&nested), quote(&sibling), quote(&jail.main),
        )).await;
        assert!(output.contains("GRANT_WRITABLE"), "{profile}: {output}");
        assert!(output.contains("NESTED_WRITABLE"), "{profile}: {output}");
        assert!(
            output.contains("Operation not permitted"),
            "{profile}: {output}"
        );
        assert_eq!(
            std::fs::read_to_string(granted.join("result")).unwrap(),
            "granted"
        );
        assert_eq!(
            std::fs::read_to_string(nested.join("result")).unwrap(),
            "nested"
        );
        assert!(!sibling.join("result").exists(), "{profile}: {output}");
        assert!(
            !jail.main.join("ungranted-result").exists(),
            "{profile}: {output}"
        );
    }
}

async fn granted_checkout_fixture(profile: &str) -> (Jail, Vec<(PathBuf, PathBuf)>) {
    let mut jail = Jail::start_with_profile(None, None, true, profile, false).await;
    jail.bind_grant_head().await;
    let ready = jail.bash("printf READY").await;
    assert!(ready.contains("READY"), "{ready}");
    let normal = jail.home.join("Work/granted-normal");
    let common = jail.home.join("Work/granted-common");
    let linked = jail.home.join("Work/granted-linked");
    for root in [&normal, &common] {
        std::fs::create_dir_all(root).unwrap();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "fixture\n").unwrap();
        git(root, &["add", "README.md"]);
        git(root, &["commit", "-q", "-m", "initial"]);
    }
    git(
        &common,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    assert!(linked.join(".git").is_file());
    for root in [&normal, &common, &linked] {
        std::fs::create_dir_all(root.join(".cortexkit/aft/filters")).unwrap();
        std::fs::write(root.join(".cortexkit/aft.jsonc"), "{}\n").unwrap();
    }
    // Grant the common checkout too, so its config/hooks are not protected
    // merely by the unrelated sibling-checkout deny.
    jail.allow_paths(json!([
        {"path":normal,"mode":"write"}, {"path":common,"mode":"write"},
        {"path":linked,"mode":"write"},
    ]))
    .await;
    let checkouts = vec![
        (normal.clone(), normal.join(".git")),
        (linked, common.join(".git")),
    ];
    (jail, checkouts)
}

#[cfg(target_os = "macos")]
async fn assert_granted_checkout_control_denied(relative: &str, git_control: bool) {
    for profile in ["relaxed", "strict"] {
        let (mut jail, checkouts) = granted_checkout_fixture(profile).await;
        for (root, git_dir) in checkouts {
            let working = root.join("working-file");
            let control = if git_control {
                git_dir.join(relative)
            } else {
                root.join(relative)
            };
            let before = std::fs::read(&control).ok();
            let output = jail.bash(&format!(
                "printf working > {} && echo WORKING_WRITABLE; printf altered > {} && echo CONTROL_WRITABLE",
                quote(&working), quote(&control),
            )).await;
            assert!(output.contains("WORKING_WRITABLE"), "{profile}: {output}");
            assert_eq!(std::fs::read_to_string(working).unwrap(), "working");
            assert!(
                !output.contains("CONTROL_WRITABLE"),
                "{profile} {}: {output}",
                control.display()
            );
            assert!(
                output.contains("Operation not permitted"),
                "{profile}: {output}"
            );
            assert_eq!(
                std::fs::read(&control).ok(),
                before,
                "{}",
                control.display()
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_granted_checkout_hooks_are_immutable() {
    assert_granted_checkout_control_denied("hooks/grant-probe", true).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_granted_checkout_git_config_is_immutable() {
    assert_granted_checkout_control_denied("config", true).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_granted_checkout_aft_config_is_immutable() {
    assert_granted_checkout_control_denied(".cortexkit/aft.jsonc", false).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_granted_checkout_filters_are_immutable() {
    assert_granted_checkout_control_denied(".cortexkit/aft/filters/grant-probe", false).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn worker_granted_checkout_linux_reports_unenforced_controls() {
    if !native_available() {
        return;
    }
    for profile in ["relaxed", "strict"] {
        let (mut jail, checkouts) = granted_checkout_fixture(profile).await;
        let output = jail.bash("printf LINUX_GRANT_REPORT").await;
        assert!(output.contains("LINUX_GRANT_REPORT"), "{profile}: {output}");
        for (root, git_dir) in checkouts {
            for path in [
                git_dir.join("hooks"),
                git_dir.join("config"),
                root.join(".cortexkit/aft.jsonc"),
                root.join(".cortexkit/aft/filters"),
            ] {
                assert!(
                    output.contains(&format!("write:{}", path.display())),
                    "{profile} {}: {output}",
                    path.display()
                );
            }
        }
        if profile == "strict" {
            assert!(output.contains("nested_write_deny"), "{output}");
            assert!(
                output.contains("worktree_git_config_mutable_on_landlock"),
                "{output}"
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_env_dotdot_existing_credential_is_denied() {
    let mut jail = Jail::relaxed_with_env(|base, command| {
        std::fs::create_dir_all(base.join("credential-config/nested")).unwrap();
        std::fs::write(base.join("credential-config/secret"), "env-dotdot-secret").unwrap();
        command.env(
            "AWS_CONFIG_FILE",
            base.join("credential-config/nested/../secret"),
        );
    })
    .await;
    let reply = jail
        .bash(&format!(
            "echo ENV_SHELL_RAN; cat {}",
            quote(&jail.base.join("credential-config/secret"))
        ))
        .await;
    assert!(reply.contains("ENV_SHELL_RAN"), "{reply}");
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert!(!reply.contains("env-dotdot-secret"), "{reply}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_env_dotdot_missing_credential_does_not_refuse_shell() {
    let mut jail = Jail::relaxed_with_env(|base, command| {
        std::fs::create_dir(base.join("credential-config")).unwrap();
        command.env(
            "AWS_SHARED_CREDENTIALS_FILE",
            base.join("credential-config/../future/credentials"),
        );
    })
    .await;
    let reply = jail.bash("printf 'MISSING_ENV_SHELL_RAN\\n'").await;
    assert!(reply.contains("MISSING_ENV_SHELL_RAN"), "{reply}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_env_symlink_config_target_is_denied() {
    let mut jail = Jail::relaxed_with_env(|base, command| {
        std::fs::create_dir(base.join("credential-target")).unwrap();
        std::fs::write(
            base.join("credential-target/hosts.yml"),
            "env-symlink-secret",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            base.join("credential-target"),
            base.join("credential-alias"),
        )
        .unwrap();
        command.env("GH_CONFIG_DIR", base.join("credential-alias"));
    })
    .await;
    for name in ["credential-target", "credential-alias"] {
        let reply = jail
            .bash(&format!(
                "echo SYMLINK_ENV_SHELL_RAN; cat {}",
                quote(&jail.base.join(name).join("hosts.yml"))
            ))
            .await;
        assert!(reply.contains("SYMLINK_ENV_SHELL_RAN"), "{reply}");
        assert!(reply.contains("Operation not permitted"), "{reply}");
        assert!(!reply.contains("env-symlink-secret"), "{reply}");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_private_file_ancestor_rename() {
    let mut jail = Jail::start_with_layout(None, None, true, "relaxed", false, true).await;
    let auth = jail.base.join("custom-relay/auth");
    let moved = jail.base.join("custom-relay/moved");
    let before = std::fs::read(auth.join("connection.json")).unwrap();
    let reply = jail
        .bash(&format!(
            "mv {} {}; cat {}/connection.json; cat {}/connection.json",
            quote(&auth),
            quote(&moved),
            quote(&moved),
            quote(&auth)
        ))
        .await;
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert!(!moved.exists(), "private ancestor was renamed: {reply}");
    assert_eq!(std::fs::read(auth.join("connection.json")).unwrap(), before);
    assert!(
        !reply.contains("worker-jail"),
        "credentials leaked: {reply}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_nested_sibling_parent_rename_but_allows_own_content_renames() {
    let mut jail = Jail::relaxed_with_env(|base, _| {
        let group = base.join("group");
        std::fs::create_dir(&group).unwrap();
        git(
            &base.join("main"),
            &[
                "worktree",
                "move",
                base.join("sibling").to_str().unwrap(),
                group.join("sibling").to_str().unwrap(),
            ],
        );
    })
    .await;
    let group = jail.base.join("group");
    let moved = jail.base.join("moved");
    let own = jail.worktree.join("own-content");
    let reply = jail
        .bash(&format!(
            "mkdir {own} && printf own > {own}/file && mv {own}/file {own}/renamed && \
             mv {own} {own}-renamed && rm {own}-renamed/renamed && rmdir {own}-renamed && \
             echo OWN_CONTENT_RENAMES_OK; \
             if mv {group} {moved}; then printf tamper >> {moved}/sibling/README.md; fi",
            own = quote(&own),
            group = quote(&group),
            moved = quote(&moved),
        ))
        .await;
    assert!(reply.contains("OWN_CONTENT_RENAMES_OK"), "{reply}");
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert!(!moved.exists(), "sibling parent was renamed: {reply}");
    assert!(!own.exists());
    assert!(!jail.worktree.join("own-content-renamed").exists());
    assert_eq!(
        std::fs::read_to_string(group.join("sibling/README.md")).unwrap(),
        "main\n"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_custom_cargo_home_credentials() {
    let mut jail = Jail::start_with_layout(None, None, true, "relaxed", false, true).await;
    for name in ["credentials", "credentials.toml"] {
        let path = jail.base.join("custom-cargo").join(name);
        let reply = jail.bash(&format!("cat {}", quote(&path))).await;
        assert!(reply.contains("Operation not permitted"), "{reply}");
        assert!(!reply.contains("custom-cargo-secret"), "{reply}");
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_checkout_at_work_is_writable_but_sibling_is_not() {
    let mut jail = Jail::start_with_layout(None, None, true, "relaxed", false, true).await;
    assert_eq!(jail.worktree, jail.home.join("Work"));
    let own = jail.worktree.join("own-write");
    let sibling = jail.worktree.join("sibling/README.md");
    let reply = jail
        .bash(&format!(
            "printf own > {} && echo OWN_WRITABLE; printf tamper >> {}",
            quote(&own),
            quote(&sibling)
        ))
        .await;
    assert!(reply.contains("OWN_WRITABLE"), "{reply}");
    assert_eq!(std::fs::read_to_string(own).unwrap(), "own");
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert_eq!(std::fs::read_to_string(sibling).unwrap(), "main\n");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_ssh_reads_including_aliases() {
    let mut jail = Jail::relaxed(false).await;
    let secret = jail.home.join(".ssh/probe");
    std::fs::write(&secret, "private-key-sentinel").unwrap();
    std::os::unix::fs::symlink(&secret, jail.worktree.join("key-link")).unwrap();
    for path in [
        secret,
        jail.worktree.join("key-link"),
        jail.home.join(".SSH/probe"),
    ] {
        if !path.exists() {
            continue;
        }
        let reply = jail.bash(&format!("cat {}", quote(&path))).await;
        assert!(
            reply.contains("Operation not permitted"),
            "{}: {reply}",
            path.display()
        );
        assert!(!reply.contains("private-key-sentinel"), "{reply}");
    }
}

#[tokio::test]
async fn off_worker_keeps_private_temp_and_governance_without_os_confinement() {
    let mut jail = Jail::start_with_profile(None, None, true, "off", false).await;
    let secret = jail.base.join("home/.ssh/probe");
    std::fs::write(&secret, "off-secret-readable").unwrap();
    let reply = jail.bash(&format!("cat {} && printf off > {} && test -z \"$SUBC_CONNECTION_FILE\" && test -n \"$TMPDIR\" && test \"$(command -v gh)\" = \"$AFT_GH_SHIMS_DIR/gh\" && echo TEMP=\"$TMPDIR\" && echo OFF_$((1+1))", quote(&secret), quote(&jail.base.join("sibling/off-write")))).await;
    assert!(reply.contains("OFF_2"), "{reply}");
    assert!(reply.contains("off-secret-readable"), "{reply}");
    assert!(reply.contains("/tmp/aft-w/"), "{reply}");
    assert!(
        reply.contains("profile=off unenforced=[operator gh fence: unenforced"),
        "{reply}"
    );
    assert_eq!(
        std::fs::read_to_string(jail.base.join("sibling/off-write")).unwrap(),
        "off"
    );
}

/// The worker-jail gh fence: only the jail denies this canary, at a fixed path
/// in the account's default AFT storage.
fn canary_dir(jail: &Jail) -> PathBuf {
    jail.base
        .join("home/.local/share/cortexkit/aft/gh-shim-worker-jail")
}

/// The shim's native exits, with the ticket and worker marker removed (and
/// once with the environment cleared down to HOME and PATH): only the kernel
/// canary deny can tell the shim it runs in a worker jail.
const STRIPPED_GH: &str = "env -u AFT_GH_SHIM_TICKET -u AFT_GH_SHIM_WORKER gh version; echo STRIPPED_EXIT=$?; env -i HOME=\"$HOME\" PATH=\"$PATH\" gh api repos/cortexkit/aft/issues; echo ENV_I_EXIT=$?";

fn assert_stripped_gh_refused(reply: &str) {
    assert!(reply.contains("STRIPPED_EXIT=86"), "{reply}");
    assert!(reply.contains("ENV_I_EXIT=86"), "{reply}");
    assert!(reply.contains("gh_shim_worker_native_gh"), "{reply}");
}

/// Even without the gh shim, workers must not read ~/.config/gh/hosts.yml or
/// inherit GH_TOKEN or GITHUB_TOKEN from the daemon.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn worker_jail_denies_native_gh_credentials() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start_with_layout_and_env(
        None,
        None,
        true,
        "strict",
        false,
        false,
        |base, command| {
            let config = base.join("home/.config/gh");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(config.join("hosts.yml"), "test-only-gh-token").unwrap();
            command.env("GH_TOKEN", "test-only-gh-token");
            command.env("GITHUB_TOKEN", "test-only-github-token");
        },
    )
    .await;
    let reply = jail.bash("if /bin/cat \"$HOME/.config/gh/hosts.yml\" >/dev/null 2>&1; then echo HOSTS_READABLE; else echo HOSTS_DENIED; fi; if [ \"${GH_TOKEN+x}${GITHUB_TOKEN+x}\" = '' ]; then echo TOKENS_ABSENT; else echo TOKENS_PRESENT; fi").await;
    assert!(reply.contains("HOSTS_DENIED"), "{reply}");
    assert!(reply.contains("TOKENS_ABSENT"), "{reply}");
    assert!(!reply.contains("HOSTS_READABLE"), "{reply}");
    assert!(!reply.contains("TOKENS_PRESENT"), "{reply}");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn worker_credential_floor_covers_daemon_home_and_config_overrides() {
    if !native_available() {
        return;
    }
    const FILES: &[&str] = &[
        "home/.config/gh/hosts.yml",
        "custom-gh/hosts.yml",
        "xdg/config/gh/hosts.yml",
        "home/.ssh/key",
        "home/.gnupg/key",
        "home/.aws/credentials",
        "home/.config/gcloud/key",
        "home/.azure/key",
        "home/.netrc",
        "home/.git-credentials",
        "home/.config/git/credentials",
        "home/.docker/config.json",
        "home/.npmrc",
        "home/.pypirc",
        "home/.cargo/credentials.toml",
    ];
    for profile in ["strict", "relaxed"] {
        if profile == "relaxed" && cfg!(target_os = "linux") {
            continue;
        }
        let mut jail = Jail::start_with_layout_and_env(
            None,
            None,
            true,
            profile,
            false,
            false,
            |base, command| {
                for path in FILES {
                    let path = base.join(path);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, "synthetic-credential").unwrap();
                }
                command.env("GH_CONFIG_DIR", base.join("custom-gh"));
            },
        )
        .await;
        let command = FILES.iter().enumerate().map(|(index, path)| {
            let path = quote(&jail.base.join(path));
            format!("if /bin/cat {path} >/dev/null 2>&1; then echo READABLE_{index}; else echo READ_DENIED_{index}; fi; if : >>{path} 2>/dev/null; then echo WRITABLE_{index}; else echo WRITE_DENIED_{index}; fi")
        }).collect::<Vec<_>>().join("; ");
        let reply = jail.bash(&command).await;
        for (index, path) in FILES.iter().enumerate() {
            assert!(
                reply.contains(&format!("READ_DENIED_{index}\n")),
                "{profile} {path}: {reply}"
            );
            assert!(
                reply.contains(&format!("WRITE_DENIED_{index}\n")),
                "{profile} {path}: {reply}"
            );
        }
    }
}

/// Opt in on an unlocked Mac: creates and removes only a uniquely named test item.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires an unlocked login keychain and explicit test-item consent"]
async fn worker_jail_login_keychain_test_item_is_unreadable() {
    assert_eq!(
        std::env::var("AFT_TEST_LOGIN_KEYCHAIN").as_deref(),
        Ok("1"),
        "run explicitly with AFT_TEST_LOGIN_KEYCHAIN=1 on an unlocked Mac"
    );
    let service = format!(
        "aft-worker-jail-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let account = "aft-credential-isolation-test";
    let secret = "disposable-test-value-not-an-operator-token";
    let login = run(Command::new("/usr/bin/security").args(["default-keychain", "-d", "user"]));
    let keychain = PathBuf::from(login.trim().trim_matches('"'));
    assert!(
        keychain.is_file(),
        "login keychain unavailable: {}",
        keychain.display()
    );
    let keychain = keychain.to_str().unwrap();
    struct Cleanup(String, String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Command::new("/usr/bin/security")
                .args([
                    "delete-generic-password",
                    "-s",
                    &self.0,
                    "-a",
                    "aft-credential-isolation-test",
                    &self.1,
                ])
                .output();
        }
    }
    run(Command::new("/usr/bin/security").args([
        "add-generic-password",
        "-s",
        &service,
        "-a",
        account,
        "-w",
        secret,
        "-T",
        "/usr/bin/security",
        keychain,
    ]));
    let _cleanup = Cleanup(service.clone(), keychain.into());
    let baseline = run(Command::new("/usr/bin/security").args([
        "find-generic-password",
        "-s",
        &service,
        "-a",
        account,
        "-w",
        keychain,
    ]));
    assert_eq!(
        baseline.trim(),
        secret,
        "test item must be readable outside the jail"
    );
    let mut jail = Jail::start().await;
    let reply = jail.bash(&format!(
        "/usr/bin/security find-generic-password -s '{service}' -a '{account}' -w {}; echo KEYCHAIN_EXIT=$?", quote(Path::new(keychain))
    )).await;
    assert!(
        reply.contains("KEYCHAIN_EXIT="),
        "probe did not execute: {reply}"
    );
    assert!(
        !reply.contains(secret),
        "worker read the disposable login-keychain secret: {reply}"
    );
    assert!(
        !reply.contains("KEYCHAIN_EXIT=0"),
        "worker queried the login-keychain item: {reply}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn worker_jail_fences_operator_gh_and_its_canary_resists_tampering() {
    #[cfg(target_os = "linux")]
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let dir = canary_dir(&jail);
    let canary = dir.join("canary");
    let reply = jail.bash(STRIPPED_GH).await;
    assert_stripped_gh_refused(&reply);
    let original = std::fs::read(&canary).expect("the worker shell launch created the canary");

    let parent = dir.parent().unwrap();
    let tamper = format!(
        "cat {c} >/dev/null 2>&1; echo READ=$?; chmod 644 {c} 2>/dev/null; echo CHMOD=$?; rm -f {c} 2>/dev/null; echo RM=$?; mv {c} {d}/moved 2>/dev/null; echo MV=$?; printf x > {d}/new 2>/dev/null; echo CREATE=$?; mv {d} {d}.moved 2>/dev/null; echo MVDIR=$?; mv {p} {p}.moved 2>/dev/null; echo MVPARENT=$?; {stripped}",
        c = quote(&canary),
        d = quote(&dir),
        p = quote(parent),
        stripped = STRIPPED_GH,
    );
    let reply = jail.bash(&tamper).await;
    for denied in ["READ", "RM", "MV", "CREATE", "MVDIR", "MVPARENT"] {
        assert!(!reply.contains(&format!("{denied}=0")), "{denied}: {reply}");
    }
    // Seatbelt denies the mode change. Landlock does not mediate chmod, but a
    // changed mode cannot make the denied canary readable or hide it.
    #[cfg(target_os = "macos")]
    assert!(!reply.contains("CHMOD=0"), "{reply}");
    assert_stripped_gh_refused(&reply);
    assert_eq!(std::fs::read(&canary).unwrap(), original);
    assert!(!dir.join("new").exists());
}

/// The daemon's temporary HOME is not the account home that a cleared shim uses.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn worker_jail_account_home_anchor_survives_replaced_or_missing_home() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let upstream = jail.worktree.join("upstream-gh");
    std::fs::create_dir(&upstream).unwrap();
    executable(
        &upstream.join("gh"),
        "#!/bin/sh\nprintf 'NATIVE_GH_DISPATCHED\\n'\n",
    );
    // Use a local upstream so losing the fence cannot enter an operator's gh
    // wrapper or start network/authentication work. Kill the whole process group
    // on timeout so descendants cannot keep the tool's output pipes open.
    let script = r#"import os, signal, shutil, subprocess, sys
shim = shutil.which('gh')
assert shim, 'managed gh shim missing'
for label, extra in [('NO_HOME', {}), ('WRONG_HOME', {'HOME': '/nonexistent'})]:
    child = subprocess.Popen([shim, 'version'], env={'PATH': sys.argv[1], **extra},
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        output, _ = child.communicate(timeout=5)
        code = child.returncode
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        output, _ = child.communicate(timeout=2)
        code = 124
    print(output.decode(errors='replace'), end='')
    print(f'{label}_EXIT={code}', flush=True)
"#;
    let reply = jail.tool("bash", json!({
        "command": format!("python3 -B -c {} {}", quote(Path::new(script)), quote(&upstream)),
        "timeout": 20_000,
        "wait": true,
    })).await;
    assert!(reply.contains("NO_HOME_EXIT=86"), "{reply}");
    assert!(reply.contains("WRONG_HOME_EXIT=86"), "{reply}");
    assert!(reply.contains("gh_shim_worker_native_gh"), "{reply}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_canary_metadata_is_readable_but_data_and_writes_are_denied() {
    let script = r#"import errno, os, pathlib, pwd
homes = {os.environ['HOME'], pwd.getpwuid(os.geteuid()).pw_dir}
for home in homes:
    marker = pathlib.Path(home) / '.local/share/cortexkit/aft/gh-shim-worker-jail/canary'
    marker.resolve(strict=True)
    for path in [marker, *marker.parents]:
        path.lstat()
    for flags in [os.O_RDONLY, os.O_WRONLY]:
        try:
            fd = os.open(marker, flags)
        except OSError as error:
            assert error.errno in (errno.EACCES, errno.EPERM), error
        else:
            os.close(fd)
            raise AssertionError(f'canary data access allowed: {marker}, flags={flags}')
print('CANARY_METADATA_ONLY')
"#;
    for profile in ["strict", "relaxed"] {
        let mut jail = Jail::start_with_profile(None, None, true, profile, false).await;
        let reply = jail
            .bash(&format!("python3 -B -c {}", quote(Path::new(script))))
            .await;
        assert!(reply.contains("CANARY_METADATA_ONLY"), "{profile}: {reply}");
    }
}

/// The canary sits in the account's default storage, so a configured storage
/// root elsewhere still leaves it denied and the fence in place.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn worker_jail_fence_holds_with_storage_configured_elsewhere() {
    #[cfg(target_os = "linux")]
    if !native_available() {
        return;
    }
    let mut jail = Jail::start_with_layout_and_env(
        None,
        None,
        true,
        "strict",
        false,
        false,
        |base, command| {
            let storage = base.join("elsewhere-storage");
            std::fs::create_dir_all(&storage).unwrap();
            command.env("AFT_STORAGE_DIR", storage);
        },
    )
    .await;
    let canary = canary_dir(&jail).join("canary");
    let reply = jail
        .bash(&format!(
            "cat {} >/dev/null 2>&1; echo READ=$?; {STRIPPED_GH}",
            quote(&canary)
        ))
        .await;
    assert!(!reply.contains("READ=0"), "{reply}");
    assert_stripped_gh_refused(&reply);
    assert!(canary.is_file());
    assert!(!jail
        .base
        .join("elsewhere-storage/gh-shim-worker-jail")
        .exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_macos_worker_jail_still_fences_operator_gh() {
    let mut jail = Jail::relaxed(false).await;
    let canary = canary_dir(&jail).join("canary");
    let reply = jail
        .bash(&format!(
            "cat {c} >/dev/null 2>&1; echo READ=$?; rm -f {c} 2>/dev/null; echo RM=$?; {STRIPPED_GH}",
            c = quote(&canary)
        ))
        .await;
    assert!(!reply.contains("READ=0"), "{reply}");
    assert!(!reply.contains("RM=0"), "{reply}");
    assert_stripped_gh_refused(&reply);
    assert!(canary.is_file());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn relaxed_linux_names_unenforced_denies_and_keeps_broad_access() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start_with_profile(None, None, true, "relaxed", false).await;
    let home = jail.base.join("home");
    std::fs::write(home.join(".ssh/probe"), "linux-secret-unenforced").unwrap();
    let reply = jail
        .bash(&format!(
            "cat {} && printf linux >> {} && echo BROAD_$((1+1))",
            quote(&home.join(".ssh/probe")),
            quote(&jail.base.join("sibling/README.md"))
        ))
        .await;
    assert!(reply.contains("BROAD_2"), "{reply}");
    assert!(reply.contains("linux-secret-unenforced"), "{reply}");
    assert!(reply.contains("profile=relaxed unenforced=["), "{reply}");
    // Relaxed Linux cannot deny the gh-shim canary, so the worker gets no
    // shim-level fence; the launcher names that alongside the other denies.
    assert!(
        reply.contains("operator gh fence: unenforced (relaxed profile)"),
        "{reply}"
    );
    let stripped = jail.bash(STRIPPED_GH).await;
    assert!(!stripped.contains("gh_shim_worker_native_gh"), "{stripped}");
    for name in [
        ".ssh",
        ".gnupg",
        "Library/Keychains",
        ".aws",
        ".config/gcloud",
        ".azure",
        ".config/gh",
        ".netrc",
        ".docker/config.json",
        ".npmrc",
        ".pypirc",
        ".cargo/credentials*",
    ] {
        assert!(
            reply.contains(&format!("read:{}", home.join(name).display())),
            "missing named read deny {name}: {reply}"
        );
        assert!(
            reply.contains(&format!("write:{}", home.join(name).display())),
            "missing named write deny {name}: {reply}"
        );
    }
    for path in [
        home.join("Work"),
        home.join(".gitconfig"),
        jail.main.join(".git/config"),
        jail.main.join(".git/hooks"),
        jail.data.join("cortexkit"),
    ] {
        assert!(
            reply.contains(&format!("write:{}", path.display())),
            "missing named write deny {}: {reply}",
            path.display()
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_sibling_checkout_writes_but_allows_reads() {
    let mut jail = Jail::relaxed(false).await;
    let path = jail.base.join("sibling/README.md");
    let reply = jail
        .bash(&format!(
            "cat {} && printf tamper >> {}",
            quote(&path),
            quote(&path)
        ))
        .await;
    assert!(reply.contains("main"), "sibling read failed: {reply}");
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "main\n");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_git_config() {
    let mut jail = Jail::relaxed(true).await;
    let path = jail.worktree.join(".git/config");
    let before = std::fs::read(&path).unwrap();
    let reply = jail
        .bash(&format!("printf tamper >> {}", quote(&path)))
        .await;
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_git_hooks() {
    let mut jail = Jail::relaxed(true).await;
    let path = jail.worktree.join(".git/hooks/post-commit");
    let reply = jail
        .bash(&format!("printf tamper > {}", quote(&path)))
        .await;
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert!(!path.exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_denies_global_gitconfig() {
    let mut jail = Jail::relaxed(false).await;
    let path = jail.home.join(".gitconfig");
    std::fs::write(&path, "# operator config\n").unwrap();
    let reply = jail
        .bash(&format!("printf tamper >> {}", quote(&path)))
        .await;
    assert!(reply.contains("Operation not permitted"), "{reply}");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "# operator config\n"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relaxed_allows_processes_caches_socket_hooks_packed_refs_and_cargo() {
    let mut jail = Jail::relaxed(false).await;
    let reply = jail.bash("pgrep -lf launchd && mkdir -p \"$HOME/Library/Caches/probe\" && printf cache > \"$HOME/Library/Caches/probe/file\" && echo NORMAL_$((1+1))").await;
    assert!(reply.contains("NORMAL_2"), "{reply}");
    assert_eq!(
        std::fs::read_to_string(jail.home.join("Library/Caches/probe/file")).unwrap(),
        "cache"
    );
    let socket = "import os,socket; p=os.environ['TMPDIR']+'/probe.sock'; s=socket.socket(socket.AF_UNIX); s.bind(p); print('SOCKET_OK')";
    let reply = jail
        .bash(&format!("python3 -B -c {}", quote(Path::new(socket))))
        .await;
    assert!(reply.contains("SOCKET_OK"), "{reply}");
    let reply = jail.bash("printf commit > relaxed.txt && git add relaxed.txt && git -c user.name=w -c user.email=w@example.com commit -q -m relaxed && echo COMMIT_$((1+1))").await;
    assert!(reply.contains("COMMIT_2"), "{reply}");
    assert!(git(&jail.worktree, &["log", "-1", "--format=%B"])
        .contains("Co-authored-by: Jail Bot <jail@example.com>"));
    git(&jail.main, &["branch", "packed-delete", "main"]);
    git(&jail.main, &["pack-refs", "--all"]);
    let reply = jail
        .bash("git branch -D packed-delete && echo PACKED_$((1+1))")
        .await;
    assert!(reply.contains("PACKED_2"), "{reply}");
    assert!(!git(&jail.main, &["branch", "--list", "packed-delete"]).contains("packed-delete"));
    let reply = jail.bash(&cargo_build(&jail, "", "CARGO")).await;
    assert!(reply.contains("CARGO_2"), "{reply}");
    assert!(
        jail.worktree.join("wrapper-used").is_file(),
        "relaxed must allow configured build wrappers"
    );
    let reply = jail
        .bash("test \"$(command -v gh)\" = \"$AFT_GH_SHIMS_DIR/gh\" && echo SHIM_$((1+1))")
        .await;
    assert!(reply.contains("SHIM_2"), "{reply}");
}

#[cfg(target_os = "macos")]
fn real_rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".rustup"))
}

/// Run `cargo build --offline` from the worker shell with the throwaway
/// CARGO_HOME (the shell's HOME default) and the real toolchain.
#[cfg(target_os = "macos")]
fn cargo_build(jail: &Jail, prefix: &str, marker: &str) -> String {
    format!(
        "cd {} && command -v cargo && {prefix} RUSTUP_HOME={} cargo build --offline -q && echo \"{marker}_$((1+1))\"",
        quote(&jail.worktree),
        quote(&real_rustup_home()),
    )
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_cargo_uses_jailed_path_not_cargo_home() {
    let mut jail = Jail::start_with_layout_and_env(
        None,
        None,
        true,
        "strict",
        false,
        false,
        |base, command| {
            let bin = base.join("toolchain/bin");
            std::fs::create_dir_all(&bin).unwrap();
            executable(
                &bin.join("cargo"),
                "#!/bin/sh\nprintf 'PATH_CARGO %s\\n' \"$*\"\n",
            );
            let mut paths = vec![bin];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            command.env("PATH", std::env::join_paths(paths).unwrap());
        },
    )
    .await;
    // The shell must resolve the stub from PATH, without relying on ~/.cargo/bin.
    assert!(!jail.home.join(".cargo/bin/cargo").exists());
    let reply = jail.bash(&cargo_build(&jail, "", "PATHCARGO")).await;
    assert!(reply.contains("PATH_CARGO build --offline -q"), "{reply}");
    assert!(reply.contains("PATHCARGO_2"), "{reply}");
    assert!(
        reply.contains(&jail.base.join("toolchain/bin/cargo").display().to_string()),
        "{reply}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_allows_normal_worker_work_and_refuses_everything_else() {
    let mut jail = Jail::start().await;

    // git add and git commit in the linked worktree write the worktree's own
    // Git directory and the main repository's object, ref and log stores.
    let committed = jail
        .bash(&format!(
            "cd {} && echo change > change.txt && git add change.txt && git -c user.name=w -c user.email=w@example.com commit -q -m jailed && echo \"GIT_$((1+1))\"",
            quote(&jail.worktree)
        ))
        .await;
    assert!(committed.contains("GIT_2"), "{committed}");
    assert!(git(&jail.main, &["log", "--oneline", "work"]).contains("jailed"));
    // AFT's managed Git hooks ran on that commit inside the jail.
    assert!(
        git(&jail.main, &["log", "-1", "--format=%B", "work"])
            .contains("Co-authored-by: Jail Bot <jail@example.com>"),
        "managed prepare-commit-msg hook did not run"
    );

    // gh resolves to AFT's governed shim, never to an ungoverned gh.
    let shim = jail
        .bash("test -n \"$AFT_GH_SHIMS_DIR\" && [ \"$(command -v gh)\" = \"$AFT_GH_SHIMS_DIR/gh\" ] && echo \"SHIM_$((1+1))\" && gh --status")
        .await;
    assert!(shim.contains("SHIM_2"), "{shim}");
    assert!(
        shim.contains("rung"),
        "gh --status did not come from the shim: {shim}"
    );

    // The rest of a worker's Git routine: fetch from the remote, rebase onto
    // it, stash and restore a change, and list worktrees.
    let routine = jail
        .bash(&format!(
            "cd {} && git fetch -q origin && git -c user.name=w -c user.email=w@example.com rebase -q origin/main && echo stashed > stash.txt && git add stash.txt && git -c user.name=w -c user.email=w@example.com stash -q && test ! -e stash.txt && git stash pop -q && test -e stash.txt && git worktree list && echo \"ROUTINE_$((1+1))\"",
            quote(&jail.worktree)
        ))
        .await;
    assert!(routine.contains("ROUTINE_2"), "{routine}");
    git(
        &jail.main,
        &["merge-base", "--is-ancestor", "origin/main", "work"],
    );
    assert!(git(&jail.main, &["stash", "list"]).is_empty());

    // Cargo with the operator's target-dir from ~/.cargo/config.toml (outside
    // the worktree). The rustc wrapper that file also names is bypassed in the
    // jail: a wrapper such as sccache would compile outside the sandbox.
    let built = jail.bash(&cargo_build(&jail, "", "CARGO")).await;
    assert!(built.contains("CARGO_2"), "{built}");
    assert!(jail.base.join("shared-target/debug/jailcrate").is_file());
    assert!(
        !jail.worktree.join("wrapper-used").exists(),
        "the configured rustc wrapper ran inside the jail"
    );

    // The daemon environment's CARGO_TARGET_DIR is operator-set too.
    let env_target = jail.base.join("env-target");
    let built = jail
        .bash(&cargo_build(
            &jail,
            &format!("CARGO_TARGET_DIR={}", quote(&env_target)),
            "ENVTARGET",
        ))
        .await;
    assert!(built.contains("ENVTARGET_2"), "{built}");
    assert!(env_target.join("debug/jailcrate").is_file());

    // A target the worker picks itself, inline or exported, fails like any
    // other outside write and leaves nothing behind.
    for (prefix, label) in [
        (
            format!(
                "CARGO_TARGET_DIR={}",
                quote(&jail.other_repo.join("target"))
            ),
            "INLINE",
        ),
        (
            format!(
                "export CARGO_TARGET_DIR={} &&",
                quote(&jail.home.join("chosen-target"))
            ),
            "EXPORTED",
        ),
    ] {
        let refused = jail.bash(&cargo_build(&jail, &prefix, label)).await;
        assert!(!refused.contains(&format!("{label}_2")), "{refused}");
    }
    assert!(!jail.other_repo.join("target").exists());
    assert!(!jail.home.join("chosen-target").exists());

    // Cargo and rustup caches, the package manager caches, and TMPDIR.
    let caches = jail
        .bash(&format!(
            "touch \"$HOME/.cargo/registry/probe\" \"$HOME/.cargo/git/probe\" \"$HOME/.cargo/.package-cache\" {} \"$HOME/.npm/probe\" \"$HOME/.bun/install/cache/probe\" \"$TMPDIR/probe\" && echo \"CACHES_$((1+1))\"",
            quote(&jail.rustup.join("downloads/probe"))
        ))
        .await;
    assert!(caches.contains("CACHES_2"), "{caches}");
    let bun = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".bun/bin/bun");
    if bun.is_file() {
        let installed = jail
            .bash(&format!(
                "cd {} && printf '{{\"name\":\"jail\",\"version\":\"1.0.0\"}}' > package.json && {} install && echo \"BUN_$((1+1))\"",
                quote(&jail.worktree),
                quote(&bun)
            ))
            .await;
        assert!(installed.contains("BUN_2"), "{installed}");
    }

    // A target/ symlink inside the worktree does not widen the grant: its
    // real path is outside and stays unwritable.
    let linked = jail
        .bash(&format!(
            "cd {} && ln -s {} target-link && if (echo x > target-link/f) 2>/dev/null; then echo \"WRO\"\"TE\"; else echo \"REF\"\"USED\"; fi",
            quote(&jail.worktree),
            quote(&jail.base.join("lexar"))
        ))
        .await;
    assert!(linked.contains("REFUSED"), "{linked}");
    assert!(!jail.base.join("lexar/f").exists());

    // Everything else outside the worktree stays refused.
    let refused_paths = [
        jail.main.join("README.md"),
        jail.main.join("new-file"),
        jail.main.join(".git/config"),
        jail.main.join(".git/hooks/post-commit"),
        jail.main.join(".git/info/exclude"),
        jail.main.join(".git/worktrees/sibling/HEAD"),
        jail.base.join("sibling/README.md"),
        jail.other_repo.join("README.md"),
        jail.home.join(".ssh/authorized_keys"),
        jail.home.join(".cargo/config.toml"),
        jail.home.join(".cargo/bin/cargo"),
        jail.data.join("cortexkit/probe"),
        jail.home.join("probe"),
    ];
    let before: Vec<_> = refused_paths
        .iter()
        .map(|path| std::fs::read(path).ok())
        .collect();
    let script = refused_paths
        .iter()
        .map(|path| {
            format!(
                "if (printf x >> {0}) 2>/dev/null; then echo \"WRO\"\"TE {1}\"; else echo \"REF\"\"USED {1}\"; fi",
                quote(path),
                path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let refusals = jail.bash(&script).await;
    for (path, before) in refused_paths.iter().zip(before) {
        assert!(
            refusals.contains(&format!("REFUSED {}", path.display())),
            "{} was writable:\n{refusals}",
            path.display()
        );
        assert_eq!(std::fs::read(path).ok(), before, "{}", path.display());
    }
}

#[cfg(target_os = "macos")]
fn sccache_connect_script(address: &str, unix: bool) -> String {
    let connect = if unix {
        "s = socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(sys.argv[1])"
    } else {
        "host, port = sys.argv[1].rsplit(':', 1); s = socket.create_connection((host, int(port)), timeout=3)"
    };
    let python = format!(
        "import socket,sys,errno\ntry:\n {connect}\n print('CONNECTED')\nexcept OSError as e:\n assert e.errno in (errno.EPERM, errno.EACCES), e\n print('DENIED_BY_SANDBOX')"
    );
    format!(
        "python3 -B -c {} {}",
        quote(Path::new(&python)),
        quote(Path::new(address))
    )
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_denies_shared_sccache_tcp_but_preserves_other_loopback() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ipv6_listener = std::net::TcpListener::bind(("::1", port)).unwrap();
    let mut jail = Jail::start_with_sccache(Some(port), None).await;
    for host in ["127.0.0.1", "localhost", "::1"] {
        let script = sccache_connect_script(&format!("{host}:{port}"), false);
        let outside = run(Command::new("/bin/bash").args(["-c", &script]));
        assert!(outside.contains("CONNECTED"), "{host}: {outside}");
        let reply = jail.bash(&script).await;
        assert!(reply.contains("DENIED_BY_SANDBOX"), "{host}: {reply}");
        assert!(!reply.contains("CONNECTED"), "{host}: {reply}");
    }
    drop(ipv6_listener);

    // A build or test can still serve and connect to its own ephemeral loopback port.
    let python = "import socket\ns = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen()\nc = socket.create_connection(s.getsockname(), timeout=3)\ns.accept()[0].close()\nprint('OWN_LOOPBACK_CONNECTED')";
    let reply = jail
        .bash(&format!("python3 -B -c {}", quote(Path::new(python))))
        .await;
    assert!(reply.contains("OWN_LOOPBACK_CONNECTED"), "{reply}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_denies_shared_sccache_uds() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sccache.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let script = sccache_connect_script(path.to_str().unwrap(), true);
    let outside = run(Command::new("/bin/bash").args(["-c", &script]));
    assert!(outside.contains("CONNECTED"), "{outside}");
    let mut jail = Jail::start_with_sccache(None, Some(&path)).await;
    let reply = jail.bash(&script).await;
    assert!(reply.contains("DENIED_BY_SANDBOX"), "{reply}");
    assert!(!reply.contains("CONNECTED"), "{reply}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_spawns_with_dangling_socket_deny() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("docker.sock");
    let target = root.path().join("not-running.sock");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let mut jail = Jail::start_with_sccache(None, Some(&path)).await;
    let reply = jail.bash("echo DANGLING_SOCKET_$((1+1))").await;
    assert!(reply.contains("DANGLING_SOCKET_2"), "{reply}");
    assert!(!target.exists());
}

#[tokio::test]
async fn worker_jail_shell_runs_without_any_packed_refs_files() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start_with_options(None, None, false).await;
    let common = jail.main.join(".git");
    for name in ["packed-refs", "packed-refs.lock", "packed-refs.new"] {
        assert!(!common.join(name).exists());
    }
    let output = jail.bash("printf 'ABSENT_REFS_%s\\n' $((1+1))").await;
    assert!(
        output.lines().any(|line| line.trim() == "ABSENT_REFS_2"),
        "{output}"
    );
    assert!(!output.contains("sandbox_unavailable"), "{output}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_commits_and_deletes_packed_ref_preserving_config() {
    let mut jail = Jail::start().await;
    git(&jail.main, &["branch", "packed-delete", "work"]);
    // Give branch deletion a real config section to remove if writes become
    // possible, so the unchanged-bytes assertion also detects weakened policy.
    git(
        &jail.main,
        &[
            "config",
            "branch.packed-delete.description",
            "protected branch config",
        ],
    );
    git(&jail.main, &["pack-refs", "--all"]);
    let common = jail.main.join(".git");
    let packed = std::fs::read_to_string(common.join("packed-refs")).unwrap();
    assert!(packed.contains(" refs/heads/packed-delete\n"));
    assert!(!common.join("refs/heads/packed-delete").exists());
    let committed = jail
        .bash(&format!(
            "cd {} && git -c user.name=w -c user.email=w@example.com commit --allow-empty -q -m packed-probe && echo PACKED_COMMIT_$((1+1))",
            quote(&jail.worktree),
        ))
        .await;
    assert!(committed.contains("PACKED_COMMIT_2"), "{committed}");
    assert!(!committed.contains("error:"), "{committed}");
    assert!(!committed.contains("fatal:"), "{committed}");
    assert!(git(&jail.main, &["log", "-1", "--format=%s", "work"]).contains("packed-probe"));
    let config = common.join("config");
    let config_before = std::fs::read(&config).unwrap();
    let deleted = jail
        .tool_reply(
            "bash",
            json!({
                "command": format!(
                    "cd {} && git branch -D packed-delete && echo PACKED_DELETE_$((1+1))",
                    quote(&jail.worktree),
                ),
                "timeout": 300_000,
            }),
        )
        .await;
    let result = &deleted["structuredContent"];
    assert_eq!(result["success"].as_bool(), Some(true), "{deleted}");
    assert_eq!(result["status"].as_str(), Some("completed"), "{deleted}");
    assert_eq!(result["exit_code"].as_i64(), Some(0), "{deleted}");
    let output = result["output"].as_str().expect("completed bash output");
    assert!(
        output.lines().any(|line| line.trim() == "PACKED_DELETE_2"),
        "{deleted}"
    );
    // Git deletes the ref before trying to remove its branch config section.
    // Shared config stays read-only so a worker cannot redirect core.hooksPath
    // and bypass hooks; tolerate only that exact lock error/warning pair.
    packed_ref_delete_diagnostics(output, &config)
        .unwrap_or_else(|error| panic!("{error}\n{deleted}"));
    assert_eq!(
        std::fs::read(&config).unwrap(),
        config_before,
        "shared config changed during deletion"
    );
    assert!(!common.join("refs/heads/packed-delete").exists());
    let absent = Command::new("git")
        .current_dir(&jail.main)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/packed-delete",
        ])
        .status()
        .unwrap();
    assert_eq!(absent.code(), Some(1));
    let packed = std::fs::read_to_string(common.join("packed-refs")).unwrap();
    assert!(!packed.contains(" refs/heads/packed-delete\n"));
    assert!(!common.join("packed-refs.lock").exists());
    assert!(!common.join("packed-refs.new").exists());
}

fn packed_ref_delete_diagnostics(output: &str, config: &Path) -> Result<(), String> {
    let expected_error = format!("error: could not lock config file {}", config.display());
    let expected_warning = "warning: update of config-file failed";
    let mut errors = 0;
    let mut warnings = 0;
    for line in output.lines().map(str::trim) {
        if line == expected_error {
            errors += 1;
        } else if line == expected_warning {
            warnings += 1;
        } else if line.contains("error:") || line.contains("fatal:") || line.contains("warning:") {
            return Err(format!("unexpected branch-delete diagnostic: {line}"));
        }
    }
    if errors != warnings || errors > 1 {
        return Err(format!("expected no diagnostics or one config-lock error/warning pair, got {errors} errors and {warnings} warnings"));
    }
    Ok(())
}

#[test]
fn packed_ref_delete_diagnostics_accepts_only_the_config_lock_pair_or_no_diagnostics() {
    let config = Path::new("/fixture/main/.git/config");
    assert!(packed_ref_delete_diagnostics(
        "Deleted branch packed-delete\nPACKED_DELETE_2\n",
        config
    )
    .is_ok());
    let output = format!("Deleted branch packed-delete\nerror: could not lock config file {}\nwarning: update of config-file failed\nPACKED_DELETE_2\n", config.display());
    assert!(packed_ref_delete_diagnostics(&output, config).is_ok());
}

#[test]
fn packed_ref_delete_diagnostics_rejects_other_errors_and_incomplete_pairs() {
    let config = Path::new("/fixture/main/.git/config");
    let error = format!("error: could not lock config file {}", config.display());
    let warning = "warning: update of config-file failed";
    let pair = format!("{error}\n{warning}\n");
    for output in [
        error.clone(),
        warning.to_string(),
        format!("{pair}{pair}"),
        format!("{pair}error: unable to create file /fixture/main/.git/packed-refs.new: Operation not permitted\n"),
        format!("{pair}fatal: failed to delete packed ref\n"),
        format!("{pair}warning: another warning\n"),
        format!("{error}: Operation not permitted\n{warning}\n"),
        format!("error: could not lock config file /other/.git/config\n{warning}\n"),
    ] {
        assert!(packed_ref_delete_diagnostics(&output, config).is_err(), "unexpected diagnostics were accepted: {output}");
    }
}

#[test]
fn packed_ref_delete_writable_config_control_removes_seeded_section() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let main = base.join("main");
    let worktree = base.join("worktree");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "--quiet"]);
    git(
        &main,
        &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
    );
    git(&main, &["branch", "packed-delete", "HEAD"]);
    git(
        &main,
        &[
            "config",
            "branch.packed-delete.description",
            "protected branch config",
        ],
    );
    git(&main, &["pack-refs", "--all"]);
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    let common = main.join(".git");
    let config_before = std::fs::read(common.join("config")).unwrap();
    assert!(std::fs::read_to_string(common.join("packed-refs"))
        .unwrap()
        .contains(" refs/heads/packed-delete\n"));
    assert!(!common.join("refs/heads/packed-delete").exists());
    git(&worktree, &["branch", "-D", "packed-delete"]);
    let config_after = std::fs::read(common.join("config")).unwrap();
    assert_ne!(
        config_after, config_before,
        "writable config control must change the protected bytes"
    );
    assert!(!String::from_utf8(config_after)
        .unwrap()
        .contains("[branch \"packed-delete\"]"));
    assert!(!std::fs::read_to_string(common.join("packed-refs"))
        .unwrap()
        .contains(" refs/heads/packed-delete\n"));
}

fn native_available() -> bool {
    #[cfg(target_os = "linux")]
    if !Command::new(aft_binary::aft_binary())
        .args(["sandbox-launch", "--support"])
        .output()
        .unwrap()
        .status
        .success()
    {
        assert_ne!(
            std::env::var("AFT_REQUIRE_LANDLOCK").as_deref(),
            Ok("1"),
            "required Landlock ABI unavailable"
        );
        eprintln!("Landlock probe skipped: mandatory ABI unavailable");
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let home = aft::worker_jail_canary::account_home()
            .expect("resolve account home for worker canary probe");
        if let Err(error) = account_home_canary_writable(&home) {
            let reason = format!(
                "account home {} cannot hold the worker canary: {error}",
                home.display()
            );
            assert_ne!(
                std::env::var("AFT_REQUIRE_LANDLOCK").as_deref(),
                Ok("1"),
                "{reason}"
            );
            eprintln!("{reason}");
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn account_home_canary_writable(home: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let canary = aft::worker_jail_canary::dir_for_home(home);
    // Canary preparation creates missing parents; probe the nearest existing
    // directory without creating anything in the account's real home.
    for ancestor in canary.ancestors() {
        match std::fs::metadata(ancestor) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
                }
                let path = std::ffi::CString::new(ancestor.as_os_str().as_bytes())?;
                // SAFETY: path is a live, NUL-terminated string for this call.
                if unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
                    let error = std::io::Error::last_os_error();
                    // Preparation chmods only the real canary directory to 0700.
                    // Its owner can repair permissions, but existing parents are
                    // not repaired and symlink canary directories are refused.
                    if error.kind() == std::io::ErrorKind::PermissionDenied && ancestor == canary {
                        let metadata = std::fs::symlink_metadata(ancestor)?;
                        // SAFETY: geteuid takes no arguments and has no preconditions.
                        if metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() } {
                            return Ok(());
                        }
                    }
                    return Err(error);
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::from_raw_os_error(libc::ENOENT))
}

#[cfg(target_os = "linux")]
#[test]
fn worker_canary_preflight_accepts_missing_and_existing_directories_without_writes() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    account_home_canary_writable(&home).unwrap();
    assert!(!home.exists());

    let canary = aft::worker_jail_canary::dir_for_home(&home);
    std::fs::create_dir_all(&canary).unwrap();
    account_home_canary_writable(&home).unwrap();
    assert!(std::fs::read_dir(&canary).unwrap().next().is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn worker_canary_preflight_accepts_owned_readonly_canary_without_chmod() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let canary = aft::worker_jail_canary::dir_for_home(&home);
    std::fs::create_dir_all(&canary).unwrap();
    // SAFETY: geteuid takes no arguments and has no preconditions.
    assert_eq!(std::fs::metadata(&canary).unwrap().uid(), unsafe {
        libc::geteuid()
    });
    std::fs::set_permissions(&canary, std::fs::Permissions::from_mode(0o500)).unwrap();

    let result = account_home_canary_writable(&home);
    let mode = std::fs::metadata(&canary).unwrap().permissions().mode() & 0o777;
    std::fs::set_permissions(&canary, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(mode, 0o500, "the preflight must not repair permissions");
    result.unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn worker_canary_preflight_does_not_repair_readonly_ancestor() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: geteuid takes no arguments and has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("readonly ancestor probe skipped: root can bypass directory permissions");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let canary = aft::worker_jail_canary::dir_for_home(&home);
    let parent = canary.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o500)).unwrap();

    let result = account_home_canary_writable(&home);
    let mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(mode, 0o500, "the preflight must not repair permissions");
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EACCES));
    assert!(!canary.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn worker_canary_preflight_rejects_a_file_ancestor() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::write(&home, "not a directory").unwrap();
    let error = account_home_canary_writable(&home).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
    assert_eq!(std::fs::read_to_string(&home).unwrap(), "not a directory");
}

fn executable(path: &Path, contents: &str) {
    executable_cache::write_executable(path, contents.as_bytes()).unwrap();
}

#[tokio::test]
async fn worker_jail_private_hook_chain_executes_read_only() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    git(&jail.main, &["config", "extensions.worktreeConfig", "true"]);
    // First create AFT's hook scripts, then copy them under an older content
    // key. A worktree manager can retain that older path in its hook chain
    // even after AFT starts selecting a newer set of scripts.
    let selected = jail.bash("git config --path core.hooksPath").await;
    let hook_root = jail.data.join("cortexkit/aft/cache/git-hooks");
    let managed = std::fs::read_dir(&hook_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir() && path.file_name().unwrap().len() == 32)
        .unwrap();
    assert!(selected.contains(managed.to_str().unwrap()), "{selected}");
    let original = hook_root.join("0123456789abcdef0123456789abcdef");
    std::fs::create_dir_all(&original).unwrap();
    for entry in std::fs::read_dir(&managed).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), original.join(entry.file_name())).unwrap();
    }
    let hooks = jail.data.join("cortexkit/prefrontal-core/hooks/hash-1");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook_link = hooks.parent().unwrap().join("selected");
    std::os::unix::fs::symlink(&hooks, &hook_link).unwrap();
    let original_link = hook_root.join("original-link");
    std::os::unix::fs::symlink(&original, &original_link).unwrap();
    for name in ["pre-commit", "prepare-commit-msg", "post-index-change"] {
        executable(
            &hooks.join(name),
            &format!(
                "#!/bin/sh\nexec \"$(git config --path mason.originalHooksPath)/{name}\" \"$@\"\n"
            ),
        );
    }
    // The core stub execs the copied AFT hook, which returns to the common
    // repository's native hook on its second invocation. The marker proves
    // that whole chain ran, not merely the first wrapper.
    executable(
        &jail.main.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nprintf chained > hook-marker\n",
    );
    git(
        &jail.worktree,
        &[
            "config",
            "--worktree",
            "core.hooksPath",
            hook_link.to_str().unwrap(),
        ],
    );
    git(
        &jail.worktree,
        &[
            "config",
            "--worktree",
            "mason.originalHooksPath",
            original_link.to_str().unwrap(),
        ],
    );
    let direct = jail.bash(&format!(
        "cd {} && cat {}/pre-commit > /dev/null && unset GIT_CONFIG_COUNT && git config --path core.hooksPath && git -c user.name=w -c user.email=w@example.com commit --allow-empty -m direct && echo DIRECT_HOOK_$((1+1))",
        quote(&jail.worktree), quote(&hooks)
    )).await;
    eprintln!("Native direct hook output:\n{direct}");
    assert!(direct.contains("DIRECT_HOOK_2"), "{direct}");
    assert!(direct.contains(hook_link.to_str().unwrap()), "{direct}");
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join("hook-marker")).unwrap(),
        "chained"
    );
    std::fs::remove_file(jail.worktree.join("hook-marker")).unwrap();
    let committed = jail.bash(&format!(
        "cd {} && git config --path core.hooksPath && printf change > hook-change && git add hook-change && git -c user.name=w -c user.email=w@example.com commit -m chained && echo HOOK_COMMIT_$((1+1))",
        quote(&jail.worktree)
    )).await;
    eprintln!("Native hook commit output:\n{committed}");
    assert!(committed.contains("HOOK_COMMIT_2"), "{committed}");
    #[cfg(target_os = "linux")]
    assert!(
        committed.contains("worktree_git_config_mutable_on_landlock"),
        "the writable Git configuration limitation was not reported: {committed}"
    );
    #[cfg(target_os = "linux")]
    assert!(
        committed.contains("worktree_git_binding_mutable_on_landlock"),
        "the writable Git binding limitation was not reported: {committed}"
    );
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join("hook-marker")).unwrap(),
        "chained"
    );
    assert!(git(&jail.main, &["log", "-1", "--format=%B", "work"])
        .contains("Co-authored-by: Jail Bot <jail@example.com>"));
    #[cfg(target_os = "macos")]
    {
        let denied = jail
            .bash("git config --worktree core.hooksPath /tmp/worker-selected-hooks")
            .await;
        assert!(denied.contains("Operation not permitted"), "{denied}");
        assert_eq!(
            git(
                &jail.worktree,
                &["config", "--worktree", "--get", "core.hooksPath"]
            )
            .trim(),
            hook_link.to_str().unwrap()
        );
        for path in [
            jail.worktree.join(".git"),
            jail.main.join(".git/worktrees/worker/commondir"),
            jail.main.join(".git/worktrees/worker/gitdir"),
        ] {
            let before = std::fs::read(&path).unwrap();
            let denied = jail
                .bash(&format!("printf redirect >> {}", quote(&path)))
                .await;
            assert!(denied.contains("Operation not permitted"), "{denied}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "Git binding was writable: {denied}"
            );
        }
    }
    let sibling = hooks.parent().unwrap().join("sibling-secret");
    std::fs::write(&sibling, "private-hook-sibling").unwrap();
    std::fs::write(jail.data.join("cortexkit/aft/aft.db"), "private-db").unwrap();
    for path in [
        &sibling,
        &jail.data.join("cortexkit/aft/aft.db"),
        &jail.base.join("connection.json"),
    ] {
        let denied = jail.bash(&format!("cat {}", quote(path))).await;
        assert!(
            !denied.contains("private-hook-sibling") && !denied.contains("private-db"),
            "{denied}"
        );
        assert!(
            denied.contains("Operation not permitted") || denied.contains("Permission denied"),
            "{denied}"
        );
    }
    let denied = jail
        .bash(&format!("ls {}", quote(&jail.data.join("cortexkit"))))
        .await;
    assert!(
        denied.contains("Operation not permitted") || denied.contains("Permission denied"),
        "{denied}"
    );
    for (key, dir, configured_dir) in [
        ("core.hooksPath", &hooks, &hook_link),
        ("mason.originalHooksPath", &original, &original_link),
    ] {
        let path = dir.join("pre-commit");
        let before = std::fs::read(&path).unwrap();
        for (operation, attempt) in [
            ("append", "printf tamper >> \"$hook_dir/pre-commit\""),
            ("create", "printf new > \"$hook_dir/new-hook\""),
            ("delete", "rm \"$hook_dir/pre-commit\""),
            (
                "rename",
                "mv \"$hook_dir/pre-commit\" \"$hook_dir/renamed-hook\"",
            ),
        ] {
            // Resolve each directory inside the jailed child. Disable AFT's
            // command-scope hooksPath override to exercise the repository's
            // configured core stub as well as its original-hook target.
            let command = format!(
                "cd {} && unset GIT_CONFIG_COUNT && hook_dir=$(git config --path {key}) && test -r \"$hook_dir/pre-commit\" && printf 'HOOK_PROBE_%s path=%s\\n' \"$((1+1))\" \"$hook_dir\" && {attempt}",
                quote(&jail.worktree),
            );
            let reply = jail
                .tool_reply("bash", json!({"command": command, "timeout": 300_000}))
                .await;
            let result = &reply["structuredContent"];
            let output = result["output"].as_str().unwrap_or_default();
            eprintln!(
                "Hook immutability {key} {operation}: status={} exit_code={}\n{output}",
                result["status"], result["exit_code"]
            );
            assert_eq!(
                result["success"], true,
                "{key} {operation} did not launch: {reply}"
            );
            assert!(
                output.contains("HOOK_PROBE_2"),
                "{key} {operation} did not reach the child operation: {reply}"
            );
            assert!(
                output.contains(&format!("path={}", configured_dir.display())),
                "{key} {operation} resolved the wrong directory: {reply}"
            );
            assert_eq!(
                result["status"], "failed",
                "{key} {operation} unexpectedly succeeded: {reply}"
            );
            assert!(
                result["exit_code"].as_i64().is_some_and(|code| code != 0),
                "{key} {operation} has no failing child exit status: {reply}"
            );
            let kernel_denial = if cfg!(target_os = "macos") {
                "Operation not permitted"
            } else {
                "Permission denied"
            };
            assert!(
                output.contains(kernel_denial),
                "{key} {operation} was not a kernel write denial: {reply}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap_or_default(),
                before,
                "{key} {operation} changed the hook bytes: {reply}"
            );
            assert!(
                !dir.join("new-hook").exists(),
                "{key} {operation} created a file: {reply}"
            );
            assert!(
                !dir.join("renamed-hook").exists(),
                "{key} {operation} renamed the hook: {reply}"
            );
        }
    }
}

// This Cargo probe is macOS-only because it uses the host's real toolchain.
// Linux runners may install Cargo outside HOME and the system read roots, where
// the jail correctly denies it. Hook and temporary-file probes still use Landlock.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_jail_operator_build_slot_and_container_reads() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let container = jail.worktree.parent().unwrap().to_path_buf();
    let cargo_config = container.join(".cargo/config.toml");
    let budget = container.join(".cargo/alfonso-test-threads");
    let seeds = container.join(".rust-build-seeds");
    let slots = jail.data.join("cortexkit/prefrontal-core/build-slots");
    for path in [cargo_config.parent().unwrap(), &seeds, &slots] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(&budget, "2\n").unwrap();
    std::fs::write(seeds.join("seed"), "build-seed").unwrap();
    for (name, value) in [
        (".root", "slot-root"),
        (".cache", "slot-cache"),
        ("limit", "2\n"),
    ] {
        std::fs::write(slots.join(name), value).unwrap();
    }
    let wrapper = slots.join("prefrontal-build-slot");
    executable(
        &wrapper,
        r#"#!/usr/bin/env python3
import fcntl, os, pathlib, sys
base = pathlib.Path(__file__).parent
assert (base / '.root').read_text() == 'slot-root'
assert (base / '.cache').read_text() == 'slot-cache'
assert (base / 'limit').read_text().strip() == '2'
with (base / 'queue.lock').open('a') as queue, (base / '0.lock').open('a') as slot:
    fcntl.flock(queue, fcntl.LOCK_EX)
    fcntl.flock(slot, fcntl.LOCK_EX)
    pathlib.Path('slot-wrapper-ran').write_text('locked')
    fcntl.flock(queue, fcntl.LOCK_UN)
    child = os.fork()
    if child == 0:
        direct = str(base / 'rustc-lowprio-direct')
        os.execv(direct, [direct, *sys.argv[1:]])
    _, status = os.waitpid(child, 0)
    sys.exit(os.waitstatus_to_exitcode(status))
"#,
    );
    executable(
        &slots.join("rustc-lowprio-direct"),
        "#!/bin/sh\nexec \"$@\"\n",
    );
    std::fs::write(
        &cargo_config,
        format!(
            "[build]\nrustc-wrapper = {:?}\ntarget-dir = {:?}\n",
            wrapper,
            jail.base.join("shared-target")
        ),
    )
    .unwrap();
    let excludes = jail
        .main
        .join(".git/worktrees/worker/alfonso-hydration-exclude");
    // Git names the administrative directory after the checkout's basename.
    std::fs::write(&excludes, "hydration-excluded\n").unwrap();
    git(
        &jail.worktree,
        &["config", "core.excludesFile", excludes.to_str().unwrap()],
    );
    std::fs::write(jail.worktree.join("hydration-excluded"), "ignored").unwrap();
    let read = jail.bash(&format!(
        "cat {} {} {} {} && git check-ignore hydration-excluded && git config --local --get core.excludesFile && echo CONTAINER_$((1+1))",
        quote(&cargo_config), quote(&budget), quote(&seeds.join("seed")), quote(&excludes)
    )).await;
    assert!(read.contains("CONTAINER_2"), "{read}");
    assert!(read.contains("build-seed"), "{read}");
    let built = jail.bash(&cargo_build(&jail, "", "BUILD_SLOT")).await;
    eprintln!("Native build-slot output:\n{built}");
    assert!(built.contains("BUILD_SLOT_2"), "{built}");
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join("slot-wrapper-ran")).unwrap(),
        "locked"
    );
    assert!(jail.base.join("shared-target/debug/jailcrate").is_file());
    for name in ["queue.lock", "0.lock", "1.lock", "2.lock"] {
        let locked = jail
            .bash(&format!(
                "printf locked >> {} && echo SLOT_LOCK_$((1+1))",
                quote(&slots.join(name))
            ))
            .await;
        assert!(locked.contains("SLOT_LOCK_2"), "{locked}");
    }
    for path in [
        &wrapper,
        &slots.join("rustc-lowprio-direct"),
        &slots.join(".root"),
        &slots.join(".cache"),
        &slots.join("limit"),
        &cargo_config,
        &budget,
        &seeds.join("seed"),
    ] {
        let before = std::fs::read(path).unwrap();
        let denied = jail
            .bash(&format!("printf tamper >> {}", quote(path)))
            .await;
        assert!(
            denied.contains("Operation not permitted") || denied.contains("Permission denied"),
            "{denied}"
        );
        assert_eq!(std::fs::read(path).unwrap(), before, "{denied}");
    }
    for path in [
        slots.join("3.lock"),
        slots.join("arbitrary"),
        container.join("other-container-data"),
    ] {
        let denied = jail
            .bash(&format!("printf tamper > {}", quote(&path)))
            .await;
        assert!(
            denied.contains("Operation not permitted") || denied.contains("Permission denied"),
            "{denied}"
        );
        assert!(!path.exists(), "{denied}");
    }
}

#[tokio::test]
async fn worker_jail_temp_and_cortexkit_scratch_match_tools() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let temp = jail.bash("printf 'TMPDIR=%s TMP=%s TEMP=%s\\n' \"$TMPDIR\" \"$TMP\" \"$TEMP\"; set -e; file=$(mktemp); dir=$(mktemp -d); test -f \"$file\"; test -d \"$dir\"; test \"${file%/*}\" = \"$TMPDIR\"; test \"${dir%/*}\" = \"$TMPDIR\"; test \"$TMP\" = \"$TMPDIR\"; test \"$TEMP\" = \"$TMPDIR\"; echo TEMP_$((1+1))").await;
    eprintln!("Native mktemp output:\n{temp}");
    assert!(temp.contains("TEMP_2"), "{temp}");
    let explicit = jail.bash("file=$(mktemp ./explicit.XXXXXXXX) && test -f \"$file\" && rm \"$file\" && echo EXPLICIT_TEMP_$((1+1))").await;
    assert!(explicit.contains("EXPLICIT_TEMP_2"), "{explicit}");
    let outside = jail.bash("TMPDIR=/tmp mktemp").await;
    assert!(
        outside.contains("Operation not permitted") || outside.contains("Permission denied"),
        "{outside}"
    );
    let scratch = jail.bash("mkdir -p .cortexkit/tmp .cortexkit/internal-docs && printf shell > .cortexkit/tmp/shell && printf docs > .cortexkit/internal-docs/note && cat .cortexkit/tmp/shell && echo SCRATCH_$((1+1))").await;
    assert!(scratch.contains("SCRATCH_2"), "{scratch}");
    let written = jail
        .tool(
            "write",
            json!({"filePath": jail.worktree.join(".cortexkit/tmp/tool"), "content": "tool"}),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(jail.worktree.join(".cortexkit/tmp/tool")).unwrap_or_default(),
        "tool",
        "{written}"
    );
    // Seatbelt protects .cortexkit/aft.jsonc and .cortexkit/aft/filters
    // inside a writable worktree. Landlock's additive rules cannot exclude
    // those children; its nested_write_deny warning reports that limitation.
    #[cfg(target_os = "macos")]
    for path in [
        jail.worktree.join(".cortexkit/aft.jsonc"),
        jail.worktree.join(".cortexkit/aft/filters/policy.toml"),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "protected").unwrap();
        let denied = jail
            .bash(&format!("printf tamper >> {}", quote(&path)))
            .await;
        assert!(denied.contains("Operation not permitted"), "{denied}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "protected",
            "{denied}"
        );
    }
}

#[tokio::test]
async fn worker_jail_short_temp_binds_and_connects_unix_socket() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let script = r#"import os, pathlib, socket, stat
root = pathlib.Path(os.environ['TMPDIR'])
assert os.environ['TMP'] == str(root) == os.environ['TEMP']
assert root == root.resolve()
assert stat.S_IMODE(root.stat().st_mode) == 0o700
nested = root / ('a' * 30)
nested.mkdir()
path = str(nested / ('b' * 30 + '.sock'))
server = socket.socket(socket.AF_UNIX)
server.settimeout(3)
server.bind(path)
server.listen(1)
client = socket.socket(socket.AF_UNIX)
client.settimeout(3)
client.connect(path)
peer, _ = server.accept()
peer.sendall(b'hello')
assert client.recv(5) == b'hello'
print('SOCKET_ROUNDTRIP_OK')
"#;
    let reply = jail
        .bash(&format!("python3 -B -c {}", quote(Path::new(script))))
        .await;
    assert!(reply.contains("SOCKET_ROUNDTRIP_OK"), "{reply}");
}

#[tokio::test]
async fn worker_jail_temp_denies_other_task_directory() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    let recorded = jail.worktree.join("first-temp.txt");
    let release = jail.worktree.join("release-first-temp");
    // Probe once while the first command runs and once after it finished,
    // when its scratch is retained for its own session.
    let ready = jail.bash("true").await;
    let first_corr = jail.submit_tool("bash", json!({
        "command": format!("printf original > \"$TMPDIR/sentinel\" && printf '%s' \"$TMPDIR\" > {}; while [ ! -f {} ]; do sleep 0.05; done", quote(&recorded), quote(&release)),
        "timeout": 30_000,
    })).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let first = loop {
        if let Ok(path) = std::fs::read_to_string(&recorded) {
            if !path.is_empty() {
                break PathBuf::from(path);
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first temp was not published for request {first_corr}; initialization: {ready}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(first.join("sentinel").is_file());
    let alias = jail.worktree.join("other-task-temp-alias");
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let script = r#"import errno, os, pathlib, sys
own = pathlib.Path(os.environ['TMPDIR'])
assert own != pathlib.Path(sys.argv[1])
(own / 'positive-control').write_text('allowed')
assert (own / 'positive-control').read_text() == 'allowed'
for candidate in [pathlib.Path(sys.argv[1]) / 'sentinel', pathlib.Path(sys.argv[2]) / 'sentinel']:
    try:
        candidate.read_bytes()
    except OSError as e:
        assert e.errno in (errno.EPERM, errno.EACCES), e
    else:
        raise AssertionError('other task directory was readable')
try:
    (pathlib.Path(sys.argv[1]) / 'sentinel').write_text('tampered')
except OSError as e:
    assert e.errno in (errno.EPERM, errno.EACCES), e
    print('OTHER_TASK_DENIED')
else:
    raise AssertionError('other task directory was writable')
"#;
    let reply = jail
        .bash(&format!(
            "python3 -B -c {} {} {}",
            quote(Path::new(script)),
            quote(&first),
            quote(&alias)
        ))
        .await;
    assert!(reply.contains("OTHER_TASK_DENIED"), "{reply}");
    assert_eq!(
        std::fs::read_to_string(first.join("sentinel")).unwrap(),
        "original"
    );
    std::fs::write(&release, "done").unwrap();
    // Wait for the first command's own reply, so the checks below run after it
    // finished rather than while it still holds its scratch.
    loop {
        let frame = next_frame(&mut jail.stream).await;
        if frame.header.channel == 1 && frame.header.corr == first_corr {
            break;
        }
    }
    // A finished command's scratch stays for its own session to read for
    // SCRATCH_RETENTION (expiry and scope-end deletion are tested in
    // bash_background::registry). Retention must not open it to other tasks.
    assert_eq!(
        std::fs::read_to_string(first.join("sentinel")).unwrap(),
        "original"
    );
    let reply = jail
        .bash(&format!(
            "python3 -B -c {} {} {}",
            quote(Path::new(script)),
            quote(&first),
            quote(&alias)
        ))
        .await;
    assert!(reply.contains("OTHER_TASK_DENIED"), "{reply}");
    assert_eq!(
        std::fs::read_to_string(first.join("sentinel")).unwrap(),
        "original"
    );
    let read = jail.read_file(&first.join("sentinel")).await;
    assert!(
        read["content"].as_array().is_some(),
        "retained-scratch read must pass schema admission: {read}"
    );
    assert_ne!(
        read["isError"], true,
        "session-owned retained scratch stays available to AFT read: {read}"
    );
    assert!(read.to_string().contains("original"), "{read}");
    jail.bind_grant_head().await;
    let head = jail.head_shell_read(&first.join("sentinel")).await;
    assert_eq!(
        head["structuredContent"]["exit_code"], 0,
        "head shell access stays unchanged: {head}"
    );
    assert!(head.to_string().contains("original"), "{head}");
    assert!(head.to_string().contains("HEAD_SHELL_READ"), "{head}");
}

#[tokio::test]
async fn worker_jail_temp_denies_shared_tmp_write() {
    if !native_available() {
        return;
    }
    let mut jail = Jail::start().await;
    // Use a fresh name so even a broken policy cannot overwrite a host file.
    let outside = PathBuf::from(format!(
        "/tmp/aft-denied-{}",
        jail.base.file_name().unwrap().to_str().unwrap()
    ));
    let script = r#"import errno, os, pathlib, sys
(pathlib.Path(os.environ['TMPDIR']) / 'positive-control').write_text('allowed')
try:
    with open(sys.argv[1], 'x') as f:
        f.write('unexpected')
except OSError as e:
    assert e.errno in (errno.EPERM, errno.EACCES), e
    print('SHARED_TMP_DENIED')
else:
    os.unlink(sys.argv[1])
    raise AssertionError('shared tmp was writable')
"#;
    let reply = jail
        .bash(&format!(
            "python3 -B -c {} {}",
            quote(Path::new(script)),
            quote(&outside)
        ))
        .await;
    assert!(reply.contains("SHARED_TMP_DENIED"), "{reply}");
    assert!(!outside.exists());
}

#[path = "helpers/session_output.rs"]
mod session_output;

async fn session_output_jail() -> (Jail, session_output::SessionOutput) {
    let mut jail = Jail::start().await;
    let ready = jail.tool_reply("bash", json!({"command": "true"})).await;
    let capture = PathBuf::from(
        ready["structuredContent"]["output_path"]
            .as_str()
            .unwrap_or_else(|| panic!("missing task capture: {ready}")),
    );
    let storage = capture
        .ancestors()
        .nth(6)
        .expect("namespaced task capture root");
    let outputs = session_output::SessionOutput::new(storage, "worker-session");
    (jail, outputs)
}

#[tokio::test]
async fn worker_jail_reads_own_saved_task_output() {
    if !native_available() {
        return;
    }
    let (mut jail, outputs) = session_output_jail().await;
    let copied = jail.worktree.join("saved-gate.log");
    let reply = jail
        .bash(&format!(
            "cat {} && cp {} {} && echo OWN_OUTPUT_READ",
            quote(&outputs.own.stdout),
            quote(&outputs.own.stdout),
            quote(&copied),
        ))
        .await;
    assert!(reply.contains("saved-output\n"), "{reply}");
    assert_eq!(std::fs::read_to_string(copied).unwrap(), "saved-output\n");
}

#[tokio::test]
async fn worker_jail_denies_other_session_saved_output() {
    if !native_available() {
        return;
    }
    let (mut jail, outputs) = session_output_jail().await;
    for path in [&outputs.other.stdout, &outputs.other_harness.stdout] {
        let reply = jail
            .bash(&format!(
                "if cat {}; then echo UNEXPECTED_READ; else echo OTHER_SESSION_DENIED; fi",
                quote(path)
            ))
            .await;
        assert!(reply.contains("OTHER_SESSION_DENIED"), "{reply}");
        assert!(!reply.contains("saved-output"), "{reply}");
    }
}

#[tokio::test]
async fn worker_jail_denies_saved_task_environment() {
    if !native_available() {
        return;
    }
    let (mut jail, outputs) = session_output_jail().await;
    let reply = jail
        .bash(&format!(
            "if cat {}; then echo UNEXPECTED_READ; else echo ENVIRONMENT_DENIED; fi",
            quote(&outputs.own.environment)
        ))
        .await;
    assert!(reply.contains("ENVIRONMENT_DENIED"), "{reply}");
    assert!(!reply.contains("secret-environment"), "{reply}");
}

#[tokio::test]
async fn worker_jail_denies_private_storage_and_output_symlinks() {
    if !native_available() {
        return;
    }
    let (mut jail, outputs) = session_output_jail().await;
    for (index, path) in outputs
        .private
        .iter()
        .chain([&outputs.other.stdout])
        .enumerate()
    {
        let link = outputs.own.io_dir.join("stdout-link");
        std::os::unix::fs::symlink(path, &link).unwrap();
        let reply = jail
            .bash(&format!(
                "if cat {} || cat {}; then echo UNEXPECTED_READ; else echo PRIVATE_DENIED; fi",
                quote(path),
                quote(&link)
            ))
            .await;
        assert!(reply.contains("PRIVATE_DENIED"), "case {index}: {reply}");
        std::fs::remove_file(link).unwrap();
    }
    // A readable artifact name must not turn a symlink into a storage escape.
    std::fs::remove_file(&outputs.own.stdout).unwrap();
    std::os::unix::fs::symlink(&outputs.private[0], &outputs.own.stdout).unwrap();
    let reply = jail
        .bash(&format!(
            "if cat {}; then echo UNEXPECTED_READ; else echo SYMLINK_DENIED; fi",
            quote(&outputs.own.stdout)
        ))
        .await;
    assert!(reply.contains("SYMLINK_DENIED"), "{reply}");
}

#[tokio::test]
async fn worker_jail_cannot_write_saved_task_output() {
    if !native_available() {
        return;
    }
    let (mut jail, outputs) = session_output_jail().await;
    for path in [&outputs.own.stdout, &outputs.own.io_dir.join("new-file")] {
        let reply = jail.bash(&format!(
            "if printf tampered > {}; then echo UNEXPECTED_WRITE; else echo SAVED_WRITE_DENIED; fi", quote(path)
        )).await;
        assert!(reply.contains("SAVED_WRITE_DENIED"), "{reply}");
    }
    assert_eq!(
        std::fs::read_to_string(&outputs.own.stdout).unwrap(),
        "saved-output\n"
    );
}

#[cfg(target_os = "macos")]
#[path = "helpers/worker_keychain.rs"]
mod keychain;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn worker_jail_session_dbus_socket_is_reachable_and_reported_unenforced() {
    if !native_available() {
        return;
    }
    for profile in ["strict", "relaxed"] {
        let mut listener = None;
        let mut socket = PathBuf::new();
        let mut jail = Jail::start_with_layout_and_env(
            None,
            None,
            true,
            profile,
            false,
            false,
            |base, command| {
                let runtime = base.join("runtime");
                std::fs::create_dir(&runtime).unwrap();
                socket = runtime.join("bus");
                listener = Some(std::os::unix::net::UnixListener::bind(&socket).unwrap());
                command.env("XDG_RUNTIME_DIR", &runtime).env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", socket.display()),
                );
            },
        )
        .await;
        let python = format!("import socket; s=socket.socket(socket.AF_UNIX); s.connect({:?}); s.sendall(b'worker-session-bus-probe')", socket.to_str().unwrap());
        let reply = jail.bash(&format!(
            "test -z \"${{DBUS_SESSION_BUS_ADDRESS+x}}\" && echo ENV_STRIPPED_$((1+1)); python3 -B -c {} && echo BUS_REACHABLE_$((1+1))",
            quote(Path::new(&python)),
        )).await;
        assert!(reply.contains("ENV_STRIPPED_2"), "{profile}: {reply}");
        assert!(reply.contains("BUS_REACHABLE_2"), "{profile}: {reply}");
        assert!(
            reply.contains("unenforced=[session_dbus_keyring,")
                || reply.contains("unenforced=[session_dbus_keyring]"),
            "missing named unenforced capability: {reply}"
        );
        // A real AF_UNIX connect and payload, not merely an environment check,
        // demonstrate data exchange at the configured session-bus socket path.
        let listener = listener.take().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (mut connection, _) = listener.accept().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = [0; 24];
        std::io::Read::read_exact(&mut connection, &mut bytes).unwrap();
        assert_eq!(&bytes, b"worker-session-bus-probe");
    }
}

async fn gh_read_jail() -> Jail {
    gh_read_jail_with_profile("strict").await
}

async fn gh_read_jail_with_profile(profile: &str) -> Jail {
    Jail::start_with_layout_and_env(
        None,
        None,
        true,
        profile,
        false,
        false,
        worker_gh_read::configure,
    )
    .await
}

#[tokio::test]
async fn worker_jail_teardown_kills_sandboxed_shell_groups_during_unwind() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let ready = jail.worktree.join("teardown-ready");
    let release = jail.worktree.join("teardown-release");
    jail.submit_held_bash(json!({
        "command": format!(
            "printf '%s\\n' \"$TMPDIR\" > {}; while ! test -f {}; do sleep .02; done",
            quote(&ready),
            quote(&release),
        ),
        "background": true,
        "timeout": 60_000,
    }))
    .await;
    let temp = wait_test_file(&ready).await;
    let (_, metadata_path) = worker_gh_read::observe_task(Path::new(temp.trim()));
    let task = aft::bash_background::persistence::read_task(&metadata_path).unwrap();
    assert!(task.sandbox_native, "fixture must start a sandboxed shell");
    let pgid = task.pgid.expect("running shell process group");
    assert!(
        sandbox_task_process_groups(&jail.data)
            .iter()
            .any(|group| group.pgid == pgid),
        "teardown must find this native shell's process group"
    );
    assert!(process_group_has_live_members(pgid));

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _jail = jail;
        panic!("exercise worker fixture teardown while unwinding");
    }));
    assert!(panic.is_err());
    assert!(
        !process_group_has_live_members(pgid),
        "sandboxed shell process group {pgid} survived fixture teardown"
    );
}

async fn wait_test_file(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("test file not produced: {}", path.display()))
}

#[tokio::test]
async fn worker_gh_read_t8_first_bound_read_uses_broker_with_credentials_denied() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let ssh = jail.home.join(".ssh/worker-read-private-key");
    std::fs::write(&ssh, "not-a-real-key").unwrap();
    let gh_store = jail.home.join(".config/gh/hosts.yml");
    std::fs::create_dir_all(gh_store.parent().unwrap()).unwrap();
    std::fs::write(&gh_store, "fixture-gh-credential").unwrap();
    let cargo_store = jail.home.join(".cargo/credentials.toml");
    std::fs::write(&cargo_store, "fixture-cargo-credential").unwrap();
    // Landlock denies opening credentials, not stat(2). Seatbelt also denies
    // their metadata, so require that stronger property only on macOS.
    let metadata_check = if cfg!(target_os = "macos") {
        format!(
            "if /usr/bin/stat {} >/dev/null 2>&1; then echo 'CONFIG METADATA OPEN'; exit 94; fi",
            quote(&jail.base.join("xdg/config/cortexkit/aft.jsonc"))
        )
    } else {
        String::new()
    };
    let command = format!(
        r#"set -eu
for denied in {} {} {} {} {}; do
  if cat "$denied" >/dev/null 2>&1; then echo "CREDENTIAL FLOOR OPEN: $denied"; exit 93; fi
done
{}
test -z "${{AFT_GH_SHIM_TICKET+x}}"
test -r "$AFT_GH_READ_CAPABILITY_FILE"
echo CREDENTIALS_DENIED
# The first routing probe must be the real read, not --status.
if gh api repos/cortexkit/aft; then
  test -n "$AFT_GH_READ_SOCKET"
else
  read_status=$?
  echo FIRST_READ_LOOKUP >&2
  cat "$AFT_GH_SHIM_STATE_DIR/read-session-lookup.json" >&2 || true
  exit "$read_status"
fi
gh api repos/cortexkit/aft
echo TWO_READS_OK
"#,
        quote(&jail.base.join("xdg/config/cortexkit/aft.jsonc")),
        quote(&jail.base.join("connection.json")),
        quote(&ssh),
        quote(&gh_store),
        quote(&cargo_store),
        metadata_check
    );
    let response = jail
        .tool_reply("bash", json!({"command":command,"timeout":300_000}))
        .await;
    let reply = response.to_string();
    assert_eq!(response["structuredContent"]["exit_code"], 0, "{reply}");
    assert!(reply.contains("CREDENTIALS_DENIED"), "{reply}");
    assert!(reply.contains("TWO_READS_OK"), "{reply}");
    assert!(reply.matches("worker-bound-read").count() >= 2, "{reply}");
    let calls = jail.read_holder.lock().unwrap().calls.clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in calls {
        assert_eq!(call["principal"], "worker-read-principal");
        assert_eq!(call["repository"], "cortexkit/aft");
        assert_eq!(call["scope"]["scope_epoch"], 3);
    }
    assert!(!jail.worktree.join("native-gh.log").exists());
    let trace = std::fs::read_to_string(jail.base.join("daemon.log")).unwrap();
    assert_eq!(
        trace
            .lines()
            .filter(|line| line.contains("gh_shim worker_read:") && line.contains("read=true"))
            .count(),
        2,
        "both reads need their own audit record: {trace}"
    );
    let task_id = response["structuredContent"]["task_id"]
        .as_str()
        .expect("command task id");
    assert!(
        trace.contains(&format!(
            "gh_shim worker_read: session=worker-session task={task_id} "
        )),
        "{trace}"
    );
}

#[tokio::test]
async fn worker_gh_read_log_failed_whole_job_fallback_through_sandboxed_gh() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let log = include_str!("fixtures/gh_shim/run-40083893167-job-display.txt");
    jail.read_holder.lock().unwrap().results.extend([
        json!({"status":200,"body":{"jobs":[
            {"id":1,"name":"successful job","conclusion":"success"},
            {"id":120842790454_u64,"name":"Landing policy","conclusion":"failure","steps":[
                {"name":"checkout","started_at":"2026-10-24T09:57:11Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"success"},
                {"name":"Original PR commit identity and agent attribution","started_at":"2026-10-24T09:57:22Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"failure"},
                {"name":"cleanup","started_at":"2026-10-24T09:57:22Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"success"}
            ]}
        ]},"next_page":null}),
        json!({"body":log,"offset":0,"length":log.len()+9,"total_bytes":log.len()+9}),
    ]);
    let response = jail
        .tool_reply(
            "bash",
            json!({
                "command":"gh run view 40083893167 --repo cortexkit/aft --log-failed > gh-log-failed.txt",
                "timeout":300_000
            }),
        )
        .await;
    let text = response.to_string();
    assert_eq!(response["structuredContent"]["exit_code"], 0, "{text}");
    let expected = log
        .lines()
        .map(|line| format!("Landing policy\tUNKNOWN STEP\t{line}\n"))
        .collect::<String>();
    // Compare gh's own stdout from a file: AFT's bash reply adds its task
    // line and, on some sandboxes, notes of its own after the command output.
    let gh_output = std::fs::read_to_string(jail.worktree.join("gh-log-failed.txt"))
        .unwrap_or_else(|error| panic!("gh output file missing ({error}): {text}"));
    assert_eq!(gh_output, expected, "{text}");
    let calls = jail.read_holder.lock().unwrap().calls.clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1]["request"]["arguments"]["action"],
        "github.actions.job_log"
    );
    assert_eq!(
        calls[1]["request"]["arguments"]["args"]["job_id"],
        120842790454_u64
    );
    assert!(!jail.worktree.join("native-gh.log").exists());
}

#[tokio::test]
async fn worker_gh_read_t8_live_scope_rebind_refuses_before_upstream() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let ready = jail.worktree.join("broker-ready");
    let go = jail.worktree.join("broker-go");
    let outcome = jail.worktree.join("broker-outcome");
    let command = format!("echo ready > {}; while ! test -f {}; do sleep 0.02; done; gh api repos/cortexkit/aft > /dev/null 2>&1; echo $? > {}", quote(&ready), quote(&go), quote(&outcome));
    let corr = jail
        .submit_held_bash(json!({"command":command,"background":true,"timeout":30_000}))
        .await;
    wait_test_file(&ready).await;
    let mut scope = jail.read_holder.lock().unwrap().binds["worker-session"].clone();
    scope.scope_epoch += 1;
    jail.bind(ModuleControlRequest::RouteBind {
        route_channel: 2,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new(&jail.worktree, "runner", "worker-session"),
        principal: Some(Principal::Reserved {
            module_id: "broca".into(),
        }),
        consumer_capabilities: None,
        admission_facts: None,
        scope: Some(scope),
        role_versions: Some(std::collections::BTreeMap::from([(
            "tool-provider".into(),
            "v1".into(),
        )])),
        call_mode: None,
    })
    .await;
    std::fs::write(go, "go").unwrap();
    assert_ne!(wait_test_file(&outcome).await.trim(), "0");
    let finished = jail.reply_frame(1, corr).await;
    let finished: Value = serde_json::from_slice(&finished.body).unwrap();
    assert_eq!(
        finished["structuredContent"]["exit_code"], 0,
        "the probe itself must complete: {finished}"
    );
    assert!(jail.read_holder.lock().unwrap().calls.is_empty());
    assert!(
        jail.read_holder.lock().unwrap().dials.is_empty(),
        "binding refusal must happen in the broker, not be rescued by the synthetic upstream"
    );
    assert!(!jail.worktree.join("native-gh.log").exists());
}

#[tokio::test]
async fn worker_gh_read_t9_policy_matrix_keeps_repository_and_search_fences() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let reply = jail
        .bash(
            r#"set -eu
gh search issues sentinel >/dev/null
gh pr view 7 --json number >/dev/null
gh issue view 7 --json number >/dev/null
gh run list --json databaseId >/dev/null
printf '{}' > "$TMPDIR/input.json"
if gh api --input "$TMPDIR/input.json" repos/cortexkit/aft/issues >/dev/null 2>&1; then exit 93; fi
for command in \
 'api repos/cortexkit/other' \
 'api --hostname example.com repos/cortexkit/aft' \
 'api -f x=y repos/cortexkit/aft/issues' \
 'api graphql -f query={viewer{login}}' \
 'search issues repo:other sentinel' \
 'search issues org:cortexkit sentinel' \
 'search issues user:someone sentinel' \
 'search issues (sentinel)' \
 'search issues sentinel OR other' \
 'pr comment 7 --body forbidden'; do
  if gh $command >/dev/null 2>&1; then echo "UNEXPECTED ALLOW: $command"; exit 92; fi
done
echo MATRIX_OK
"#,
        )
        .await;
    assert!(reply.contains("MATRIX_OK"), "{reply}");
    let calls = &jail.read_holder.lock().unwrap().calls;
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(
        calls[0]["request"]["arguments"]["action"],
        "github.search.issues"
    );
    assert_eq!(calls[0]["repository"], "cortexkit/aft");
    assert!(!jail.worktree.join("native-gh.log").exists());
}

#[tokio::test]
async fn worker_gh_read_t12_missing_and_stale_endpoint_fail_without_native_gh() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let reply = jail
        .bash(
            r#"set -eu
lookup="$AFT_GH_SHIM_STATE_DIR/read-session-lookup.json"
rm -f "$lookup"
unset AFT_GH_READ_SOCKET
if gh api repos/cortexkit/aft >/dev/null 2>&1; then exit 91; fi
python3 - "$lookup" 'worker read broker requires AFT_GH_READ_SOCKET' <<'PY'
import json, sys
with open(sys.argv[1]) as f: record = json.load(f)
assert record['outcome'] == 'unavailable' and record['reason'] == sys.argv[2], record
PY
rm "$lookup"
export AFT_GH_READ_SOCKET="$TMPDIR/stale.sock"
if gh api repos/cortexkit/aft >/dev/null 2>&1; then exit 92; fi
python3 - "$lookup" 'worker read broker unavailable' <<'PY'
import json, sys
with open(sys.argv[1]) as f: record = json.load(f)
assert record['outcome'] == 'unavailable' and record['reason'] == sys.argv[2], record
PY
echo MISSING_AND_STALE_ENDPOINTS_REFUSED
"#,
        )
        .await;
    assert!(
        reply.contains("MISSING_AND_STALE_ENDPOINTS_REFUSED"),
        "{reply}"
    );
    assert!(jail.read_holder.lock().unwrap().calls.is_empty());
    assert!(!jail.worktree.join("native-gh.log").exists());
}

#[tokio::test]
async fn worker_gh_read_t11_cross_command_capability_file_and_socket_isolation() {
    if !native_available() {
        return;
    }
    cross_command_broker_isolation("strict").await;
}

/// Relaxed macOS workers get the broker, so the same boundary must hold there:
/// another command cannot list the shared task-temp root, read this command's
/// capability file, connect to its broker socket, or unlink that socket to
/// stand up its own listener in its place.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_gh_read_relaxed_cross_command_capability_file_and_socket_isolation() {
    if !native_available() {
        return;
    }
    cross_command_broker_isolation("relaxed").await;
}

async fn cross_command_broker_isolation(profile: &str) {
    let mut jail = gh_read_jail_with_profile(profile).await;
    let paths = jail.worktree.join("broker-a-paths");
    let finish = jail.worktree.join("broker-a-finish");
    let corr = jail.submit_held_bash(json!({"command":format!(
        "printf '%s\\n%s\\n%s\\n' \"$AFT_GH_READ_SOCKET\" \"$AFT_GH_READ_CAPABILITY_FILE\" \"$$\" > {}.tmp && mv {}.tmp {}; while ! test -f {}; do sleep 0.02; done",
        quote(&paths), quote(&paths), quote(&paths), quote(&finish)),"background":true,"timeout":30_000})).await;
    // The file is renamed into place whole: the test polls for it and must
    // never read a partly written list of three lines.
    let paths = wait_test_file(&paths).await;
    let parts: Vec<_> = paths.lines().collect();
    assert_eq!(parts.len(), 3);
    let (first_task, _) = worker_gh_read::observe_task(Path::new(parts[1]).parent().unwrap());
    let status = jail
        .tool_reply("bash_status", json!({"taskId":first_task}))
        .await;
    assert_eq!(
        status["structuredContent"]["status"], "running",
        "probe a live capability, not only retained scratch: {status}"
    );
    let other_root = jail.base.join("other-repo");
    git(
        &other_root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/cortexkit/common-auth.git",
        ],
    );
    let alias = other_root.join("foreign-broker-alias.sock");
    std::os::unix::fs::symlink(parts[0], &alias).unwrap();
    let mut other_scope = jail.read_holder.lock().unwrap().binds["worker-session"].clone();
    other_scope.scope_ref = "other-worker-scope".into();
    jail.bind(ModuleControlRequest::RouteBind {
        route_channel: 2,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new(&other_root, "runner", "other-worker-session"),
        principal: Some(Principal::Reserved {
            module_id: "broca".into(),
        }),
        consumer_capabilities: None,
        admission_facts: None,
        scope: Some(other_scope),
        role_versions: Some(std::collections::BTreeMap::from([(
            "tool-provider".into(),
            "v1".into(),
        )])),
        call_mode: None,
    })
    .await;
    let task_temp_root = Path::new(parts[1])
        .parent()
        .and_then(Path::parent)
        .expect("capability file lives in a task temp under the shared root");
    let probe = format!(
        r#"set -eu
if ls {} >/dev/null 2>&1; then echo TASK_TEMP_ROOT_LISTED; exit 94; fi
if cat {} >/dev/null 2>&1; then echo CAPABILITY_STOLEN; exit 95; fi
if rm {} 2>/dev/null; then echo FOREIGN_SOCKET_UNLINKED; exit 98; fi
if gh api repos/cortexkit/aft >/dev/null 2>&1; then echo FOREIGN_REPOSITORY_ALLOWED; exit 96; fi
if AFT_GH_READ_SOCKET={} gh api repos/cortexkit/common-auth >/dev/null 2>&1; then echo WRONG_BROKER_ALLOWED; exit 97; fi
python3 - {} {} <<'PY'
import json, os, socket, struct, sys
with open(os.environ['AFT_GH_READ_CAPABILITY_FILE']) as f:
 capability = f.read()
own = socket.socket(socket.AF_UNIX); own.settimeout(5); own.connect(os.environ['AFT_GH_READ_SOCKET'])
request = json.dumps(dict(capability=capability, operation=dict(op='gh_shim.session_context'))).encode()
own.sendall(struct.pack('!I', len(request)) + request)
header = own.recv(4); size = struct.unpack('!I', header)[0]; body = b''
while len(body) < size:
 part = own.recv(size - len(body))
 assert part
 body += part
context = json.loads(body)
assert context['status'] == 'ok' and context['data']['repository'] == 'cortexkit/common-auth'
print('OWN_CONTEXT_OK'); own.close()
for label, path in [('CROSS', sys.argv[1]), ('ALIAS', sys.argv[2])]:
 s = socket.socket(socket.AF_UNIX)
 try:
  s.connect(path); print(label + '_CONNECT_ALLOWED')
 except OSError:
  print(label + '_CONNECT_DENIED')
 finally:
  s.close()
PY
printf 'ISOLATION_OK\n'
"#,
        quote(task_temp_root),
        quote(Path::new(parts[1])),
        quote(Path::new(parts[0])),
        quote(Path::new(parts[0])),
        quote(Path::new(parts[0])),
        quote(&alias)
    );
    let output = jail
        .tool_reply_on_channel(2, "bash", json!({"command":probe,"timeout":30_000}))
        .await
        .to_string();
    assert!(output.contains("ISOLATION_OK"), "{output}");
    assert!(output.contains("OWN_CONTEXT_OK"), "{output}");
    #[cfg(target_os = "macos")]
    assert!(
        output.contains("CROSS_CONNECT_DENIED") && output.contains("ALIAS_CONNECT_DENIED"),
        "{output}"
    );
    #[cfg(target_os = "linux")]
    assert!(
        output.contains("CROSS_CONNECT_ALLOWED"),
        "Landlock pathname AF_UNIX measurement changed: {output}"
    );
    let status = jail
        .tool_reply("bash_status", json!({"taskId":first_task}))
        .await;
    assert_eq!(
        status["structuredContent"]["status"], "running",
        "the foreign listener must still belong to a live command: {status}"
    );
    // The launch environment contains only capability-file/socket paths, never
    // the bearer. Cross-command file denial is the Linux isolation boundary.
    let capability = std::fs::read_to_string(parts[1]).unwrap();
    let inspected = jail.tool_reply_on_channel(2, "bash", json!({"command":format!("ps eww -p {} 2>/dev/null || true; echo ENV_INSPECTION_DONE", parts[2]),"timeout":30_000})).await.to_string();
    assert!(inspected.contains("ENV_INSPECTION_DONE"));
    assert!(
        !inspected.contains(&capability),
        "another worker must not obtain bearer bytes from launch environments"
    );
    let environment = Command::new("ps")
        .args(["eww", "-p", parts[2]])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&environment.stdout).contains(&capability));
    std::fs::write(finish, "finish").unwrap();
    let finished = jail.reply_frame(1, corr).await;
    let finished: Value = serde_json::from_slice(&finished.body).unwrap();
    assert_eq!(finished["structuredContent"]["exit_code"], 0, "{finished}");
    assert!(jail.read_holder.lock().unwrap().calls.is_empty());
    assert!(!other_root.join("native-gh.log").exists());
}

/// The default worker profile is relaxed. On macOS its shell gets the broker
/// paths, the broker files exist in its private task temp, and a bound read
/// goes through the broker rather than any native `gh`.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn worker_gh_read_relaxed_shell_reads_through_broker() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail_with_profile("relaxed").await;
    let reply = jail
        .bash(
            r#"set -eu
test -S "$AFT_GH_READ_SOCKET"
test -r "$AFT_GH_READ_CAPABILITY_FILE"
test -d "$AFT_GH_SHIM_STATE_DIR"
test -z "${AFT_GH_SHIM_TICKET+x}"
gh api repos/cortexkit/aft
echo RELAXED_READ_OK
"#,
        )
        .await;
    assert!(reply.contains("RELAXED_READ_OK"), "{reply}");
    assert!(reply.contains("worker-bound-read"), "{reply}");
    let calls = jail.read_holder.lock().unwrap().calls.clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["repository"], "cortexkit/aft");
    assert!(!jail.worktree.join("native-gh.log").exists());
}

/// Relaxed Linux enforces no filesystem deny, so another worker could read a
/// broker capability or replace a broker socket in the shared task-temp root.
/// Its shell therefore runs without the broker, and the daemon log says why.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn worker_gh_read_relaxed_linux_withholds_broker_and_logs_reason() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail_with_profile("relaxed").await;
    let reply = jail
        .bash(
            r#"test -z "${AFT_GH_READ_SOCKET+x}" && test -z "${AFT_GH_READ_CAPABILITY_FILE+x}" && test -z "${AFT_GH_SHIM_STATE_DIR+x}" && test ! -e "$TMPDIR/.gh-read-capability" && echo RELAXED_LINUX_NO_BROKER"#,
        )
        .await;
    assert!(reply.contains("RELAXED_LINUX_NO_BROKER"), "{reply}");
    assert!(jail.read_holder.lock().unwrap().calls.is_empty());
    let trace = std::fs::read_to_string(jail.base.join("daemon.log")).unwrap();
    assert!(
        trace.lines().any(|line| line.contains("WARN")
            && line.contains("gh_shim worker_read unavailable")
            && line.contains("reason=sandbox profile relaxed on Linux does not protect the shared task-temp root")),
        "{trace}"
    );
}

#[tokio::test]
async fn worker_gh_read_t10_terminal_revocation_unlinks_endpoint() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let paths = jail.worktree.join("terminal-broker-paths");
    let output = jail
        .bash(&format!(
            "printf '%s\\n' \"$AFT_GH_READ_SOCKET\" > {}; gh api repos/cortexkit/aft",
            quote(&paths)
        ))
        .await;
    assert!(output.contains("worker-bound-read"), "{output}");
    let socket = std::fs::read_to_string(paths).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while Path::new(socket.trim()).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("terminal transition must unlink its broker before descendants can replay");
    assert!(std::os::unix::net::UnixStream::connect(socket.trim()).is_err());
}

#[tokio::test]
async fn gh_shim_router_host_scope_dials_require_the_live_bind() {
    let jail = gh_read_jail().await;
    let consumer = subc_client_rs::SubcConsumer::connect(
        &jail.base.join("connection.json"),
        subc_client_rs::ConsumerOptions {
            call_timeout: Duration::from_secs(3),
            handshake_timeout: Duration::from_secs(3),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let stamp = jail.read_holder.lock().unwrap().binds["worker-session"].clone();
    let selector = subc_protocol::scope::ScopeSelector::new(
        stamp.owner.clone(),
        stamp.scope_ref.clone(),
        Some(stamp.scope_epoch),
    );
    let target = RouteTarget::ToolProvider {
        module_id: "plexus".into(),
    };
    let identity = BindIdentity::new(&jail.worktree, "aft-gh-relay", "worker-session");
    let options = subc_client_rs::CallOptions {
        timeout: Duration::from_secs(3),
        route_retry_deadline: Duration::from_secs(3),
        ..Default::default()
    };
    assert!(
        consumer
            .open_route(target.clone(), identity.clone(), options.clone())
            .await
            .is_err(),
        "unscoped routes must not get a read stamp"
    );
    let route = consumer
        .open_route_scoped(target, identity, selector, options)
        .await
        .unwrap();
    let request = json!({"name":"github_read","arguments":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":""}}});
    let bytes = consumer
        .request(
            &route,
            serde_json::to_vec(&request).unwrap(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["result"]["body"]["sentinel"],
        "worker-bound-read"
    );
    jail.read_holder.lock().unwrap().binds.clear();
    assert!(
        consumer
            .request(
                &route,
                serde_json::to_vec(&request).unwrap(),
                Default::default()
            )
            .await
            .is_err(),
        "closing the bind must invalidate an already-open read route"
    );
    assert_eq!(jail.read_holder.lock().unwrap().calls.len(), 1);
}

#[tokio::test]
async fn worker_gh_read_t10_retained_descendants_fail_after_end_cancel_timeout_and_restart() {
    if !native_available() {
        return;
    }
    for mode in ["end", "cancel", "timeout", "restart"] {
        let mut jail = gh_read_jail().await;
        let ready = jail.worktree.join("descendant-ready");
        let go = jail.worktree.join("descendant-go");
        let outcome = jail.worktree.join("descendant-outcome");
        let finish = jail.worktree.join("parent-finish");
        let parent_done = jail.worktree.join("parent-finished");

        let mut command = format!(
            r#"set -eu
gh api repos/cortexkit/aft >/dev/null
python3 - {} {} {} <<'PY'
import os, json, socket, struct, sys, time
with open(os.environ['AFT_GH_READ_CAPABILITY_FILE']) as f:
 capability = f.read()
endpoint = os.environ['AFT_GH_READ_SOCKET']
if os.fork():
 os._exit(0)
os.setsid()
fd = os.open('/dev/null', os.O_RDWR)
for n in range(3): os.dup2(fd, n)
if fd > 2: os.close(fd)
ready, go, outcome = sys.argv[1:]
with open(ready, 'w') as f: f.write(endpoint + chr(10) + os.environ['TMPDIR'])
stop = time.monotonic() + 60
while not os.path.exists(go):
 if time.monotonic() > stop: os._exit(9)
 time.sleep(.02)
allowed = False
try:
 s = socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(endpoint)
 request = json.dumps({{'capability':capability,'operation':{{'op':'gh_shim.session_context'}}}}).encode()
 s.sendall(struct.pack('!I', len(request)) + request)
 header = s.recv(4)
 if len(header) == 4:
  size = struct.unpack('!I', header)[0]; body = b''
  while len(body) < size:
   part = s.recv(size - len(body))
   if not part: break
   body += part
  allowed = json.loads(body)['status'] == 'ok'
except (OSError, ValueError, KeyError): pass
with open(outcome, 'w') as f: f.write('REPLAY_ALLOWED' if allowed else 'REPLAY_DENIED')
os._exit(0)
PY
"#,
            quote(&ready),
            quote(&go),
            quote(&outcome)
        );
        if mode != "end" {
            command.push_str(&format!(
                "while ! test -f {}; do sleep .02; done; echo finished > {}\n",
                quote(&finish),
                quote(&parent_done)
            ));
        }

        let corr = jail.submit_held_bash(json!({"command":command,"background":true,"timeout":if mode == "timeout" {5000} else {30_000}})).await;
        let witness = wait_test_file(&ready).await;
        let mut paths = witness.lines();
        let socket = paths.next().expect("published broker socket");
        let temp = paths.next().expect("published command temp");
        let (task, _) = worker_gh_read::observe_task(Path::new(temp));
        if mode == "cancel" {
            let _ = jail.tool_reply("bash_kill", json!({"taskId":task})).await;
        }

        if mode == "restart" {
            jail.child.kill().unwrap();
            jail.child.wait().unwrap();
            let replacement = gh_read_jail().await;
            assert_eq!(
                Path::new(socket).parent(),
                Some(aft::sandbox_spawn::managed_task_temp_root()),
                "restart must reuse the original endpoint namespace"
            );
            assert!(
                !Path::new(socket).exists(),
                "restart must unlink the SIGKILLed generation's endpoint"
            );
            assert!(replacement.read_holder.lock().unwrap().calls.is_empty());
        } else {
            tokio::time::timeout(Duration::from_secs(15), async {
                while Path::new(&socket).exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{mode}: broker socket survived terminal state"));
            let finished = jail.reply_frame(1, corr).await;
            let finished: Value = serde_json::from_slice(&finished.body).unwrap();
            assert_eq!(finished["structuredContent"]["task_id"], task, "{finished}");
            if mode == "end" {
                assert_eq!(finished["structuredContent"]["exit_code"], 0, "{finished}");
            }
        }
        std::fs::write(&go, "go").unwrap();
        assert_eq!(wait_test_file(&outcome).await, "REPLAY_DENIED", "{mode}");
        std::fs::write(&finish, "finish").unwrap();
        if mode == "restart" {
            wait_test_file(&parent_done).await;
        }
        assert!(!jail.worktree.join("native-gh.log").exists(), "{mode}");
    }
}

#[tokio::test]
async fn worker_gh_read_t12_malformed_peer_reply_and_endpoint_replacement_fail_closed() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let reply = jail.bash(r#"set -eu
if rm "$AFT_GH_READ_SOCKET" 2>/dev/null; then echo ENDPOINT_REPLACED; exit 91; fi
python3 - "$TMPDIR/malformed.sock" <<'PY' &
import socket, struct, sys
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(1); s.settimeout(5)
c, _ = s.accept(); c.settimeout(5)
header = c.recv(4)
if len(header) == 4:
 size = struct.unpack('!I', header)[0]
 while size:
  data = c.recv(size)
  if not data: break
  size -= len(data)
 body = b'{malformed-response'
 c.sendall(struct.pack('!I', len(body)) + body)
c.close(); s.close()
PY
peer=$!
while ! test -S "$TMPDIR/malformed.sock"; do sleep .02; done
rm -f "$AFT_GH_SHIM_STATE_DIR/read-session-lookup.json"
if AFT_GH_READ_SOCKET="$TMPDIR/malformed.sock" gh api repos/cortexkit/aft >/dev/null 2>&1; then exit 92; fi
wait "$peer"
python3 - "$AFT_GH_SHIM_STATE_DIR/read-session-lookup.json" <<'PY'
import json, sys
with open(sys.argv[1]) as f: record = json.load(f)
assert record['outcome'] == 'unavailable' and record['reason'] == 'worker read broker reply malformed', record
PY
echo MALFORMED_BROKER_REPLY_REFUSED
"#).await;
    assert!(reply.contains("MALFORMED_BROKER_REPLY_REFUSED"), "{reply}");
    assert!(jail.read_holder.lock().unwrap().calls.is_empty());
    assert!(!jail.worktree.join("native-gh.log").exists());
}

#[tokio::test]
async fn worker_gh_read_t10_cancel_midflight_terminates_reader_without_output() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let hold = std::sync::Arc::new(tokio::sync::Notify::new());
    jail.read_holder.lock().unwrap().hold_reads = Some(hold.clone());
    let output = jail.worktree.join("inflight-read-output");
    let ready = jail.worktree.join("inflight-read-ready");
    let corr = jail.submit_held_bash(json!({"command":format!("printf '%s' \"$TMPDIR\" > {}; gh api repos/cortexkit/aft > {}", quote(&ready), quote(&output)),"background":true,"timeout":30_000})).await;
    let temp = wait_test_file(&ready).await;
    let (task, _) = worker_gh_read::observe_task(Path::new(&temp));
    tokio::time::timeout(Duration::from_secs(10), async {
        while jail.read_holder.lock().unwrap().calls.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the upstream read must be reached before cancellation");
    let killed = jail.tool_reply("bash_kill", json!({"taskId":task})).await;
    assert_ne!(killed["isError"], true, "{killed}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = jail.tool_reply("bash_status", json!({"taskId":task})).await;
            assert_ne!(status["isError"], true, "{status}");
            if status["structuredContent"]["status"] == "killed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the issued kill must reach terminal state before upstream release");
    hold.notify_one();
    let status = jail.tool_reply("bash_status", json!({"taskId":task})).await;
    assert!(!status.to_string().contains("worker-bound-read"));
    assert!(!std::fs::read_to_string(output)
        .unwrap_or_default()
        .contains("worker-bound-read"));
    assert_eq!(jail.read_holder.lock().unwrap().calls.len(), 1);
    let finished = jail.reply_frame(1, corr).await;
    let finished: Value = serde_json::from_slice(&finished.body).unwrap();
    assert_eq!(finished["structuredContent"]["task_id"], task, "{finished}");
    assert!(!jail.worktree.join("native-gh.log").exists());
}

#[tokio::test]
async fn gh_shim_router_host_v1_background_reply_is_terminal_and_flat() {
    let mut jail = Jail::start().await;
    jail.bind_grant_head().await;
    let ready = jail.base.join("v1-ready");
    let release = jail.base.join("v1-release");
    let warm = jail
        .tool_reply_on_channel(2, "bash", json!({"command":"true"}))
        .await;
    assert_eq!(warm["structuredContent"]["exit_code"], 0, "{warm}");
    let corr = jail.submit_tool_on_channel(2, "bash", json!({"command":format!("echo ready > {}; while ! test -f {}; do sleep .02; done", quote(&ready), quote(&release)),"background":true,"timeout":10_000})).await;
    wait_test_file(&ready).await;
    let pending = jail.reply_frame(2, corr);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
            .await
            .is_err(),
        "V1 must not send an early background launch response"
    );
    std::fs::write(release, "release").unwrap();
    let frame = pending.await;
    let reply: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(reply["structuredContent"]["status"], "completed", "{reply}");
    assert_eq!(reply["structuredContent"]["exit_code"], 0, "{reply}");
    assert!(reply["structuredContent"]["task_id"]
        .as_str()
        .unwrap()
        .starts_with("bash-"));
    assert!(reply["structuredContent"].get("data").is_none());
}

/// Reads a task record the daemon may be rewriting at that moment. The daemon
/// replaces the file atomically, and a read that races the replacement fails
/// with `Interrupted` ("task artifact was concurrently replaced") rather than
/// returning stale or torn data; that is a "read again", not a test failure.
fn read_task_settled(metadata: &Path) -> aft::bash_background::persistence::PersistedTask {
    for _ in 0..200 {
        match aft::bash_background::persistence::read_task(metadata) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other.unwrap(),
        }
    }
    panic!("task record at {} kept being replaced", metadata.display());
}

#[tokio::test]
async fn worker_gh_read_t10_promotion_keeps_the_original_command_grant() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let ready = jail.worktree.join("promotion-ready");
    let release = jail.worktree.join("promotion-release");
    let output = jail.worktree.join("promotion-read");
    jail.submit_held_bash(json!({"command":format!("gh api repos/cortexkit/aft >/dev/null; printf '%s' \"$TMPDIR\" > {}; while ! test -f {}; do sleep .02; done; gh api repos/cortexkit/aft > {}", quote(&ready), quote(&release), quote(&output)),"background":false,"timeout":30_000})).await;
    let temp = wait_test_file(&ready).await;
    let (task, metadata) = worker_gh_read::observe_task(Path::new(&temp));
    assert!(
        !read_task_settled(&metadata).notify_on_completion,
        "start as a foreground command"
    );
    let scope = jail.read_holder.lock().unwrap().binds["worker-session"].clone();
    jail.bind(ModuleControlRequest::RouteBind {
        route_channel: 2,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new(&jail.worktree, "runner", "worker-session"),
        principal: Some(Principal::Reserved {
            module_id: "broca".into(),
        }),
        consumer_capabilities: None,
        admission_facts: None,
        scope: Some(scope),
        role_versions: Some(std::collections::BTreeMap::from([(
            "tool-provider".into(),
            "v1".into(),
        )])),
        call_mode: None,
    })
    .await;
    // Detaching the route's foreground wait promotes its running bash command
    // to a background task without ending the scope. The replacement route
    // retains that scope for subsequent broker reads.
    write_frame(
        &mut jail.stream,
        &Frame::build(FrameType::Goodbye, flags(), 1, 1, 0, vec![]).unwrap(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !read_task_settled(&metadata).notify_on_completion {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("route wait detachment must really promote the foreground command");
    let status = jail
        .tool_reply_on_channel(2, "bash_status", json!({"taskId":task}))
        .await;
    assert_eq!(status["structuredContent"]["status"], "running", "{status}");
    std::fs::write(release, "release").unwrap();
    let body = wait_test_file(&output).await;
    assert!(body.contains("worker-bound-read"), "{body}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while Path::new(&temp)
            .with_file_name(format!(
                "{}.gh-read.sock",
                Path::new(&temp).file_name().unwrap().to_string_lossy()
            ))
            .exists()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the promoted command still revokes its grant at terminal state");
    assert_eq!(jail.read_holder.lock().unwrap().calls.len(), 2);
    assert!(!jail.worktree.join("native-gh.log").exists());
}

#[tokio::test]
async fn gh_shim_router_host_read_uses_served_catalog_arguments() {
    let mut jail = Jail::start().await;
    let catalog = jail.catalog_on_channel(1).await;
    let tools = catalog["tools"]
        .as_array()
        .expect("served worker catalog tools");
    let read = tools
        .iter()
        .find(|tool| tool["name"] == "read")
        .expect("served read tool");
    assert_eq!(read["input_schema"]["required"], json!(["path"]));
    assert!(read["input_schema"]["properties"].get("path").is_some());
    assert!(read["input_schema"]["properties"].get("filePath").is_none());
    let fixture = jail.worktree.join("served-read-key.txt");
    std::fs::write(&fixture, "served-read-key-sentinel\n").unwrap();
    let reply = jail.read_file(&fixture).await;
    assert!(
        reply["content"].as_array().is_some(),
        "the real read must pass served-schema admission: {reply}"
    );
    assert_ne!(reply["isError"], true, "{reply}");
    assert!(
        reply.to_string().contains("served-read-key-sentinel"),
        "{reply}"
    );
    let refused = jail.tool_reply("read", json!({"file_path":fixture})).await;
    assert_eq!(refused["code"], "invalid_request", "{refused}");
    assert_eq!(refused["detail"]["field"], "file_path", "{refused}");
    jail.bind_grant_head().await;
    let head_catalog = jail.catalog_on_channel(2).await;
    let bash = head_catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bash")
        .unwrap();
    assert!(bash["input_schema"]["properties"].get("command").is_some());
    let head_fixture = jail.base.join("head-read-key.txt");
    std::fs::write(&head_fixture, "head-command-key-sentinel\n").unwrap();
    let head = jail.head_shell_read(&head_fixture).await;
    assert_eq!(head["structuredContent"]["exit_code"], 0, "{head}");
    assert!(
        head.to_string().contains("head-command-key-sentinel"),
        "{head}"
    );
}

#[tokio::test]
async fn gh_shim_router_host_lookup_transport_failures_record_their_stage() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let jail = gh_read_jail().await;
    let state = jail.base.join("xdg/state/cortexkit/aft/gh-shim");
    let lookup = state.join("read-session-lookup.json");
    let capability = jail.worktree.join("host-test-capability");
    std::fs::write(&capability, "0123456789abcdef0123456789abcdef").unwrap();
    for (case, expected) in [
        ("missing", "worker read broker requires AFT_GH_READ_SOCKET"),
        ("stale", "worker read broker unavailable"),
        ("malformed", "worker read broker reply malformed"),
    ] {
        let _ = std::fs::remove_file(&lookup);
        let socket = jail.base.join(format!("{case}.sock"));
        let peer = if case == "malformed" {
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            Some(tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let size = stream.read_u32().await.unwrap() as usize;
                    assert!(size > 0 && size <= 65_536);
                    let mut bytes = vec![0; size];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(request["operation"]["op"], "gh_shim.session_context");
                    let malformed = b"{malformed-response";
                    stream.write_u32(malformed.len() as u32).await.unwrap();
                    stream.write_all(malformed).await.unwrap();
                })
                .await
                .expect("the real shim must reach the malformed peer");
            }))
        } else {
            None
        };
        let mut command = Command::new(aft_binary::aft_binary());
        command
            .current_dir(&jail.worktree)
            .args(["gh-shim", "api", "repos/cortexkit/aft"])
            .env_clear()
            .env("HOME", &jail.home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    jail.base.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("XDG_CONFIG_HOME", jail.base.join("xdg/config"))
            .env("AFT_GH_SHIM_STATE_DIR", &state)
            .env("AFT_GH_SHIM_WORKER", "1")
            .env("AFT_GH_READ_CAPABILITY_FILE", &capability);
        if case != "missing" {
            command.env("AFT_GH_READ_SOCKET", socket);
        }
        let output = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(86),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if let Some(peer) = peer {
            peer.await.unwrap();
        }
        let record: Value = serde_json::from_slice(&std::fs::read(&lookup).unwrap()).unwrap();
        assert_eq!(record["outcome"], "unavailable", "{case}: {record}");
        assert_eq!(record["reason"], expected, "{case}: {record}");
        assert!(
            !jail.worktree.join("native-gh.log").exists(),
            "{case}: native gh must never run"
        );
    }
    assert!(jail.read_holder.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn worker_gh_read_revocation_suppresses_reply_to_surviving_observer() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let hold = std::sync::Arc::new(tokio::sync::Notify::new());
    jail.read_holder.lock().unwrap().hold_reads = Some(hold.clone());
    let ready = jail.worktree.join("observer-ready");
    let finish = jail.worktree.join("observer-finish");
    let corr=jail.submit_held_bash(json!({"command":format!("printf '%s' \"$TMPDIR\" > {}; while ! test -f {}; do sleep .02; done",quote(&ready),quote(&finish)),"timeout":30_000})).await;
    let temp = wait_test_file(&ready).await;
    let socket = Path::new(&temp).with_file_name(format!(
        "{}.gh-read.sock",
        Path::new(&temp).file_name().unwrap().to_string_lossy()
    ));
    let capability = std::fs::read_to_string(Path::new(&temp).join(".gh-read-capability")).unwrap();
    let mut observer = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let body=serde_json::to_vec(&json!({"capability":capability,"operation":{"op":"gh_shim.read_request","request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":""}}}})).unwrap();
    observer.write_u32(body.len() as u32).await.unwrap();
    observer.write_all(&body).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while jail.read_holder.lock().unwrap().calls.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("observer read must be forwarded before revocation");
    std::fs::write(finish, "finish").unwrap();
    let frame = jail.reply_frame(1, corr).await;
    let terminal: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(
        terminal["structuredContent"]["exit_code"], 0,
        "end the command naturally, not by killing its reader: {terminal}"
    );
    hold.notify_one();
    let received = tokio::time::timeout(Duration::from_secs(5), observer.read_u32())
        .await
        .expect("broker must close or refuse a revoked in-flight request");
    if let Ok(size) = received {
        assert!(size <= 2_097_152);
        let mut response = vec![0; size as usize];
        observer.read_exact(&mut response).await.unwrap();
        let response: Value = serde_json::from_slice(&response).unwrap();
        assert_ne!(
            response["status"], "ok",
            "the surviving observer must not receive the held result: {response}"
        );
        assert!(!response.to_string().contains("worker-bound-read"));
    }
    assert_eq!(jail.read_holder.lock().unwrap().calls.len(), 1);
}

#[tokio::test]
async fn worker_gh_read_common_git_config_is_write_denied() {
    if !native_available() {
        return;
    }
    let mut jail = gh_read_jail().await;
    let common = jail.main.join(".git/config");
    let before = std::fs::read(&common).unwrap();
    let output=jail.bash(&format!("set -eu; if printf '\\n# forbidden\\n' >> {}; then echo COMMON_CONFIG_WRITABLE; exit 91; fi; if git config --file {} remote.origin.url https://github.com/cortexkit/other.git; then echo COMMON_CONFIG_RETARGETED; exit 92; fi; echo COMMON_CONFIG_DENIED",quote(&common),quote(&common))).await;
    assert!(output.contains("COMMON_CONFIG_DENIED"), "{output}");
    assert_eq!(
        std::fs::read(common).unwrap(),
        before,
        "the next ticket's repository source must stay unchanged"
    );
}
