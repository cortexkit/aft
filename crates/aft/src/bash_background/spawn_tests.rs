//! End-to-end pipe-spawn contracts at the registry boundary.
use super::*;
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static FORKS: AtomicUsize = AtomicUsize::new(0);
static COUNT_FORKS: AtomicBool = AtomicBool::new(false);
static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

extern "C" fn count_fork() {
    if COUNT_FORKS.load(Ordering::Relaxed) {
        FORKS.fetch_add(1, Ordering::Relaxed);
    }
}

fn start_fork_counter() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        assert_eq!(
            unsafe { libc::pthread_atfork(Some(count_fork), None, None) },
            0
        )
    });
    FORKS.store(0, Ordering::Relaxed);
    COUNT_FORKS.store(true, Ordering::Relaxed);
}

fn spawn(registry: &BgTaskRegistry, root: &Path, command: &str, background: bool) -> String {
    registry
        .spawn_with_shell_in_slot(
            SpawnPlan::Unsandboxed,
            command,
            super::super::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "spawn-contract".into(),
            root.to_owned(),
            HashMap::new(),
            super::super::HardKill::After(Duration::from_secs(60)),
            None,
            root.to_owned(),
            if background {
                super::super::TaskSlot::Background { max: 100 }
            } else {
                super::super::TaskSlot::Foreground
            },
            background,
            false,
            Some(root.to_owned()),
        )
        .unwrap()
}

fn wait_done(registry: &BgTaskRegistry, id: &str) {
    let start = Instant::now();
    while !registry.is_task_terminal(id) {
        let task = registry.task(id).unwrap();
        registry.poll_task(&task).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "child did not finish"
        );
        std::thread::yield_now();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_oom_priority_local_pipes_and_pty_raise_children_not_aft() {
    let before = std::fs::read_to_string("/proc/self/oom_score_adj").unwrap();
    let root = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let command = "IFS= read -r value < /proc/self/oom_score_adj; printf '%s\\n' \"$value\"; /bin/sh -c 'cat /proc/self/oom_score_adj'";
    for background in [false, true] {
        let id = spawn(&registry, root.path(), command, background);
        wait_done(&registry, &id);
        let task = registry.task(&id).unwrap();
        assert_eq!(std::fs::read(&task.paths.stdout).unwrap(), b"1000\n1000\n");
        assert_eq!(std::fs::read(&task.paths.stderr).unwrap(), b"");
    }
    let id = registry
        .spawn_pty_with_shell(
            SpawnPlan::Unsandboxed,
            command,
            super::super::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "spawn-contract".into(),
            root.path().to_owned(),
            HashMap::new(),
            super::super::HardKill::After(Duration::from_secs(30)),
            None,
            root.path().to_owned(),
            4,
            false,
            false,
            Some(root.path().to_owned()),
            24,
            80,
        )
        .unwrap();
    wait_done(&registry, &id);
    let task = registry.task(&id).unwrap();
    assert_eq!(
        std::fs::read_to_string(&task.paths.pty)
            .unwrap()
            .replace("\r\n", "\n"),
        "1000\n1000\n"
    );
    assert_eq!(
        std::fs::read_to_string("/proc/self/oom_score_adj").unwrap(),
        before
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_scope_preserves_shell_dollar_expressions() {
    const FIXTURE: &str = "AFT_TEST_SYSTEMD_SCOPE_FIXTURE";
    if let Some(root) = std::env::var_os(FIXTURE) {
        let root = PathBuf::from(root);
        let registry = BgTaskRegistry::default();
        let command = r#"X=inner; printf '%s' "$BASHPID" > shell-pid; printf 'braced=%s plain=%s pid=%s\n' "${X}" "$X" "$$"; cd child-dir ; echo "${PWD}""#;
        let id = registry
            .spawn_with_shell(
                SpawnPlan::Unsandboxed,
                command,
                super::super::BashShell::Bash,
                PathBuf::from("/bin/bash"),
                "scope-contract".into(),
                root.clone(),
                HashMap::from([
                    (LINUX_SCOPE_ENV.into(), "1".into()),
                    ("X".into(), "outer".into()),
                ]),
                super::super::HardKill::After(Duration::from_secs(15)),
                None,
                root.clone(),
                4,
                false,
                false,
                Some(root.clone()),
            )
            .unwrap();
        wait_done(&registry, &id);
        let task = registry.task(&id).unwrap();
        let stderr = std::fs::read_to_string(&task.paths.stderr).unwrap();
        assert_eq!(
            registry
                .status(&id, "scope-contract", None, None, 1024)
                .unwrap()
                .exit_code,
            Some(0),
            "{stderr}"
        );
        // BASHPID is independent of the $$ expression that systemd can corrupt.
        let pid: u32 = std::fs::read_to_string(root.join("shell-pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(pid > 0);
        assert_eq!(
            std::fs::read_to_string(&task.paths.stdout).unwrap(),
            format!(
                "braced=inner plain=inner pid={pid}\n{}\n",
                root.join("child-dir").display()
            )
        );
        assert_eq!(stderr, "");
        return;
    }

    // Isolate PATH changes from every other test, including launcher discovery.
    for mode in ["modern", "legacy"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("child-dir")).unwrap();
        let script = r#"#!/bin/bash
mode=@MODE@
expand=yes
while [[ $1 == --* ]]; do
    case $1 in
        --expand-environment=no)
            if [[ $mode == legacy ]]; then
                echo probe:unsupported >> "${0%/*}/calls"
                exit 1
            fi
            expand=no ;;
        --user|--scope|--collect|--quiet) ;;
        *) exit 2 ;;
    esac
    shift
done
kind=launch
[[ $1 == /bin/true ]] && kind=probe
echo "$kind:$expand" >> "${0%/*}/calls"
if [[ $mode == modern && $expand == yes ]]; then
    # Model systemd's embedded ${...}/$$ expansion, never shell-eval the input.
    args=()
    for rest in "$@"; do
        out=
        while [[ -n $rest ]]; do
            if [[ $rest == '$$'* ]]; then
                out+='$'
                rest=${rest:2}
            elif [[ $rest =~ ^\$\{([^}]*)\} ]]; then
                match=${BASH_REMATCH[0]}
                name=${BASH_REMATCH[1]}
                if [[ $name =~ ^[a-zA-Z_][a-zA-Z_0-9]*$ ]]; then out+=${!name-}; fi
                rest=${rest:${#match}}
            else
                out+=${rest:0:1}
                rest=${rest:1}
            fi
        done
        args+=("$out")
    done
    set -- "${args[@]}"
fi
exec "$@"
"#
        .replace("@MODE@", mode);
        crate::executable_cache::write_executable(
            &dir.path().join("systemd-run"),
            script.as_bytes(),
        )
        .unwrap();
        let path = std::env::join_paths(std::iter::once(dir.path().to_owned()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bash_background::registry::spawn_tests::linux_scope_preserves_shell_dollar_expressions",
                "--nocapture",
            ])
            .env(FIXTURE, dir.path())
            .env("PATH", path)
            .env("X", "outer")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("calls")).unwrap(),
            if mode == "modern" {
                "probe:no\nlaunch:no\n"
            } else {
                "probe:unsupported\nprobe:yes\nlaunch:yes\n"
            },
            "{mode} must exercise the availability probe and the real scope launch"
        );
    }
}

#[test]
fn bash_posix_spawn_zero_daemon_forks() {
    if !crate::posix_spawn::native_backend_for_tests() {
        return;
    }
    // Isolate the counter from unrelated tests so it can observe every daemon thread.
    const CHILD: &str = "AFT_BASH_FORK_COUNTER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bash_background::registry::spawn_tests::bash_posix_spawn_zero_daemon_forks",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "isolated all-thread fork counter failed"
        );
        return;
    }
    let _lock = COUNTER_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut counts = Vec::new();
    // One-time login PATH discovery is not a bash command launch.
    let warmup = spawn(&registry, dir.path(), "true", false);
    wait_done(&registry, &warmup);
    for mode in ["foreground", "background", "promoted"] {
        start_fork_counter();
        for iteration in 0..5 {
            let release = dir.path().join(format!("release-{iteration}"));
            let command = if mode == "promoted" {
                format!(
                    "while [ ! -f '{}' ]; do sleep 0.01; done; printf spawn-ok",
                    release.display()
                )
            } else {
                "printf spawn-ok".into()
            };
            let id = spawn(&registry, dir.path(), &command, mode == "background");
            if mode == "promoted" {
                assert!(!registry.is_task_terminal(&id));
                assert!(registry.promote(&id, "spawn-contract").unwrap());
                std::fs::write(release, b"run").unwrap();
            }
            wait_done(&registry, &id);
        }
        counts.push((mode, FORKS.load(Ordering::Relaxed)));
    }
    COUNT_FORKS.store(false, Ordering::Relaxed);
    eprintln!("fork counter: {counts:?}; N=5 per mode");
    assert!(
        counts.iter().all(|(_, n)| *n == 0),
        "daemon forked: {counts:?}"
    );
}

#[test]
#[ignore = "allocates a daemon-sized heap; run explicitly for spawn latency evidence"]
fn bash_posix_spawn_heap_latency() {
    if !crate::posix_spawn::native_backend_for_tests() {
        return;
    }
    let mib: usize = std::env::var("AFT_SPAWN_HEAP_MIB")
        .unwrap_or_else(|_| "4096".into())
        .parse()
        .unwrap();
    let mut heap = Vec::new();
    let mut bytes = 0;
    let mut seed = 1u32;
    while bytes < mib * 1024 * 1024 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let size = if heap.len() % 4 == 0 {
            4096 + seed as usize % 262144
        } else {
            16 + seed as usize % 1024
        };
        heap.push(vec![seed as u8; size]);
        bytes += size;
    }
    let dir = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut times = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        let id = spawn(&registry, dir.path(), "true", false);
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        wait_done(&registry, &id);
    }
    times.sort_by(f64::total_cmp);
    eprintln!("heap={} MiB blocks={} bash commands=50 registry spawn latency ms: p50={:.3} p90={:.3} mean={:.3}", bytes / 1024 / 1024, heap.len(), times[25], times[45], times.iter().sum::<f64>() / 50.0);
    let _lock = COUNTER_LOCK.lock().unwrap();
    start_fork_counter();
    let mut pty_times = Vec::new();
    for _ in 0..10 {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", "true"]);
        let start = Instant::now();
        let mut child = pair.slave.spawn_command(command).unwrap();
        pty_times.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(child.wait().unwrap().success());
    }
    COUNT_FORKS.store(false, Ordering::Relaxed);
    pty_times.sort_by(f64::total_cmp);
    eprintln!(
        "PTY heap={} MiB commands=10 forks={} spawn latency ms: p50={:.3} p90={:.3}",
        bytes / 1024 / 1024,
        FORKS.load(Ordering::Relaxed),
        pty_times[5],
        pty_times[9]
    );
    std::hint::black_box(heap);
}

#[test]
fn bash_posix_spawn_fallback_session_fds_and_health() {
    let _lock = COUNTER_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let before = crate::posix_spawn::health()["fallback_spawns"]
        .as_u64()
        .unwrap();
    crate::posix_spawn::FORCE_FALLBACK.set(true);
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::posix_spawn::FORCE_FALLBACK.set(false);
        }
    }
    let _reset = Reset;
    let health = crate::posix_spawn::health();
    assert_eq!(health["backend"], "fork_fallback");
    assert!(health["missing_capability"]
        .as_str()
        .unwrap()
        .contains("test-forced"));
    assert_grandchild_killed("fallback", true);
    assert_eq!(
        crate::posix_spawn::health()["fallback_spawns"]
            .as_u64()
            .unwrap(),
        before + 1
    );
    let id = spawn(&registry, dir.path(), "printf marker-ok", false);
    wait_done(&registry, &id);
    assert_eq!(
        registry
            .status(&id, "spawn-contract", None, None, 1024)
            .unwrap()
            .exit_code,
        Some(0)
    );
}

fn assert_grandchild_killed(action: &str, fallback: bool) {
    let dir = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let pidfile = dir.path().join("grandchild.pid");
    let command = format!("sleep 60 & echo $! > '{}'; wait", pidfile.display());
    let id = spawn(&registry, dir.path(), &command, false);
    let start = Instant::now();
    let descendant: u32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            break pid;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "grandchild did not start"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let task = registry.task(&id).unwrap();
    let leader = {
        let state = task.lock_state().unwrap();
        match &state.runtime {
            TaskRuntime::Piped(Some(child)) => child.id() as i32,
            _ => panic!("running pipe child missing"),
        }
    };
    // Ensure child processes are cleaned up even if the isolation assertion fails.
    struct Cleanup(Option<(u32, u32)>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some((leader, descendant)) = self.0 {
                assert!(leader > 1 && descendant > 1);
                unsafe {
                    libc::kill(leader as i32, libc::SIGKILL);
                    libc::kill(descendant as i32, libc::SIGKILL);
                }
            }
        }
    }
    let mut cleanup = Cleanup(Some((leader as u32, descendant)));
    let session = unsafe { libc::getsid(leader) };
    let group = unsafe { libc::getpgid(leader) };
    assert_eq!(
        session, leader,
        "child must not inherit the daemon's session"
    );
    assert_eq!(group, leader, "killpg must address the child's descendants");
    match action {
        "timeout" => registry.kill_for_timeout(&id, "spawn-contract").unwrap(),
        "cancel" => {
            registry.register_foreground_task("spawn-contract", &id);
            assert_eq!(registry.abort_inflight("spawn-contract").unwrap(), 1);
        }
        "scope" => {
            registry
                .kill_for_scope_revocation(&id, "spawn-contract")
                .unwrap();
        }
        "fallback" => {
            registry.kill(&id, "spawn-contract").unwrap();
        }
        _ => unreachable!(),
    }
    let start = Instant::now();
    while super::super::process::is_process_alive(descendant)
        && start.elapsed() < Duration::from_secs(3)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !super::super::process::is_process_alive(descendant),
        "{action}: grandchild survived kill; fallback={fallback}"
    );
    cleanup.0 = None;
}

#[test]
fn bash_posix_spawn_timeout_kills_grandchild() {
    assert_grandchild_killed("timeout", false);
}
#[test]
fn bash_posix_spawn_cancel_kills_grandchild() {
    assert_grandchild_killed("cancel", false);
}
#[test]
fn bash_posix_spawn_scope_revocation_kills_grandchild() {
    assert_grandchild_killed("scope", false);
}

#[test]
fn bash_posix_spawn_no_daemon_fd_leak() {
    use std::os::fd::{AsRawFd, FromRawFd};
    let dir = tempfile::tempdir().unwrap();
    let sentinel = File::create(dir.path().join("daemon-secret")).unwrap();
    let fd = unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_DUPFD, 200) };
    assert!(fd >= 200);
    let _leaked = unsafe { File::from_raw_fd(fd) };
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    let _lock = COUNTER_LOCK.lock().unwrap();
    let registry = BgTaskRegistry::default();
    let native_available = crate::posix_spawn::native_backend_for_tests();
    if !native_available {
        eprintln!("skipping native descriptor assertion; testing forced fallback only");
    }
    let fallback_modes = if native_available {
        vec![false, true]
    } else {
        vec![true]
    };
    for fallback in fallback_modes {
        crate::posix_spawn::FORCE_FALLBACK.set(fallback);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::posix_spawn::FORCE_FALLBACK.set(false);
            }
        }
        let _reset = Reset;
        let fd_dir = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        if !fallback {
            assert_eq!(crate::posix_spawn::health()["backend"], "posix_spawn");
        }
        let id = spawn(
            &registry,
            dir.path(),
            &format!("test -e {fd_dir}/0 && test -e {fd_dir}/1 && test ! -e {fd_dir}/{fd}"),
            false,
        );
        wait_done(&registry, &id);
        assert_eq!(
            registry
                .status(&id, "spawn-contract", None, None, 1024)
                .unwrap()
                .exit_code,
            Some(0),
            "daemon-held non-CLOEXEC fd leaked into bash; fallback={fallback}"
        );
    }
}

#[test]
fn agent_commands_local_shell_roots_and_cpu_are_declared_but_internal_children_are_not() {
    const CHILD: &str = "AFT_AGENT_COMMANDS_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let name = concat!(
            module_path!(),
            "::agent_commands_local_shell_roots_and_cpu_are_declared_but_internal_children_are_not"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                name.strip_prefix("aft::").unwrap_or(name),
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "isolated regression did not run: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = tempfile::tempdir().unwrap();
        assert!(
            crate::agent_commands::started(std::process::id()).is_none(),
            "standalone mode must not register roots"
        );
        crate::agent_commands::enable(root.path());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _connection = crate::agent_commands::connect(move |batch| {
            tx.send(batch).unwrap();
            async { Ok(()) }
        });
        let initial = rx.recv().await.unwrap();
        assert_eq!(initial.known, Some(vec![]));
        let mut internal = std::process::Command::new("/bin/sh")
            .args(["-c", "true"])
            .spawn()
            .unwrap();
        internal.wait().unwrap();
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "internal subprocesses must not be declared"
        );

        let registry = BgTaskRegistry::default();
        for mode in ["foreground", "background", "pty"] {
            let command = "sleep 1; printf accounted";
            let task_id = if mode == "pty" {
                registry
                    .spawn_pty(
                        SpawnPlan::Unsandboxed,
                        command,
                        "spawn-contract".into(),
                        root.path().to_owned(),
                        HashMap::new(),
                        super::super::HardKill::After(Duration::from_secs(60)),
                        None,
                        root.path().to_owned(),
                        100,
                        false,
                        false,
                        Some(root.path().to_owned()),
                        24,
                        80,
                    )
                    .unwrap()
            } else {
                spawn(&registry, root.path(), command, mode == "background")
            };
            let task = registry.task(&task_id).unwrap();
            let pid = task.lock_state().unwrap().metadata.child_pid.unwrap();
            let start = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let batch = rx.recv().await.expect("declaration sender stopped");
                    if let Some(root) = batch.started.into_iter().find(|root| root.pid == pid) {
                        break root;
                    }
                }
            })
            .await
            .expect("local shell root was never declared");
            assert_eq!(
                Some(start.start_time_ms),
                subc_os::process_start_time_ms(pid),
                "{mode}: kernel start identity"
            );
            let finish = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    registry.reap_child(&task);
                    while let Ok(batch) = rx.try_recv() {
                        if let Some(finished) =
                            batch.finished.into_iter().find(|root| root.pid == pid)
                        {
                            return finished;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("reaped shell root was never finished");
            assert_eq!(finish.start_time_ms, start.start_time_ms);
            assert!(
                finish.cpu_ms.is_some(),
                "{mode}: wait4 must supply CPU, even if it rounds to zero"
            );
        }
    });
}
