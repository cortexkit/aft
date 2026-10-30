#![cfg(unix)]
#[path = "helpers/context_storage.rs"]
mod context_storage;

#[cfg(target_os = "linux")]
use aft::bash_background::persistence::resolve_task;
use aft::sandbox_profile::SandboxProfile;
use portable_pty::{CommandBuilder, PtySize};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::{NamedTempFile, TempDir};

#[cfg(target_os = "linux")]
#[path = "helpers/mod.rs"]
mod test_helpers;

#[cfg(target_os = "linux")]
use test_helpers::{user_config, AftProcess};

const AFT_BIN: &str = env!("CARGO_BIN_EXE_aft");

struct ProbeFixture {
    _root_guard: TempDir,
    project: PathBuf,
    outside_home: PathBuf,
    secret: PathBuf,
    arbitrary_file: PathBuf,
    docker_socket: PathBuf,
    #[cfg(target_os = "macos")]
    agent_socket: PathBuf,
    profile: InheritedProfile,
}

impl ProbeFixture {
    fn new() -> Self {
        Self::with_worker_jail(false)
    }

    fn with_worker_jail(worker_jail: bool) -> Self {
        let root_guard = tempfile::tempdir().expect("create probe root");
        let root = root_guard
            .path()
            .canonicalize()
            .expect("canonical probe root");
        let project = root.join("project");
        let artifacts = root.join("task-artifacts");
        let task_temp = root.join("task-temp");
        let cargo_cache = root.join("cache/cargo-registry");
        let npm_cache = root.join("cache/npm");
        let outside_home = root.join("outside-home");
        let secret = outside_home.join(".ssh-like");
        let arbitrary_file = outside_home.join("ordinary.txt");
        let docker_socket = outside_home.join("docker.sock");
        let agent_socket = outside_home.join("agent.sock");

        for directory in [
            &project,
            &artifacts,
            &task_temp,
            &cargo_cache,
            &npm_cache,
            &secret,
        ] {
            fs::create_dir_all(directory).expect("create probe directory");
        }
        fs::create_dir_all(project.join(".git/hooks")).expect("create .git fixture");
        fs::create_dir_all(project.join(".cortexkit")).expect("create .cortexkit fixture");
        fs::write(project.join(".git/config"), b"original\n").expect("write git config fixture");
        fs::write(secret.join("id_probe"), b"probe-secret\n").expect("write secret fixture");
        fs::write(&arbitrary_file, b"ordinary\n").expect("write ordinary fixture");

        let read_allow = vec![
            project.clone(),
            artifacts.clone(),
            task_temp.clone(),
            cargo_cache.clone(),
            npm_cache.clone(),
            arbitrary_file.clone(),
        ];
        #[cfg(target_os = "linux")]
        let read_allow = {
            let mut read_allow = read_allow;
            read_allow.extend(linux_system_read_allow());
            read_allow
        };
        let mut profile = SandboxProfile::build(
            vec![project.clone(), artifacts],
            Vec::new(),
            vec![project.join(".git"), project.join(".cortexkit")],
            read_allow,
            vec![secret.clone()],
            vec![docker_socket.clone(), agent_socket.clone()],
            vec![cargo_cache, npm_cache],
            task_temp,
        )
        .expect("build canonical sandbox profile");
        profile.worker_jail = worker_jail;

        Self {
            _root_guard: root_guard,
            project,
            outside_home,
            secret,
            arbitrary_file,
            docker_socket,
            #[cfg(target_os = "macos")]
            agent_socket,
            profile: InheritedProfile::new(&profile),
        }
    }

    fn launch_bash(&mut self, script: &str) -> Output {
        self.profile.rewind();
        launcher_command(self.profile.fd(), &["/bin/bash", "-c", script])
            .output()
            .expect("spawn sandbox launcher")
    }
}

struct InheritedProfile {
    file: NamedTempFile,
    fd: RawFd,
}

impl InheritedProfile {
    fn new(profile: &SandboxProfile) -> Self {
        let mut file = NamedTempFile::new().expect("create profile file");
        serde_json::to_writer(file.as_file_mut(), profile).expect("serialize profile");
        file.as_file_mut().flush().expect("flush profile");
        file.as_file_mut().rewind().expect("rewind profile");
        let fd = file.as_file().as_raw_fd();
        set_close_on_exec(fd, false);
        Self { file, fd }
    }

    fn fd(&self) -> RawFd {
        self.fd
    }

    fn path(&self) -> &Path {
        self.file.path()
    }

    fn rewind(&mut self) {
        self.file.as_file_mut().rewind().expect("rewind profile");
    }
}

impl Drop for InheritedProfile {
    fn drop(&mut self) {
        set_close_on_exec(self.fd, true);
    }
}

fn set_close_on_exec(fd: RawFd, enabled: bool) {
    // The profile descriptor must survive the launcher exec but should not leak
    // into unrelated commands spawned by the integration-test process.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert!(flags >= 0, "F_GETFD failed");
        let flags = if enabled {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        assert_eq!(libc::fcntl(fd, libc::F_SETFD, flags), 0, "F_SETFD failed");
    }
}

fn launcher_command(fd: RawFd, target: &[&str]) -> Command {
    let mut command = Command::new(AFT_BIN);
    command
        .arg("sandbox-launch")
        .arg("--profile-fd")
        .arg(fd.to_string())
        .arg("--")
        .args(target);
    command
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

mod signal_scope {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Child;

    struct Victim(Child);

    fn signal_scope_available() -> bool {
        #[cfg(target_os = "linux")]
        {
            let output = Command::new(AFT_BIN)
                .args(["sandbox-launch", "--support"])
                .output()
                .expect("probe native signal support");
            if !output.status.success() {
                assert!(
                    std::env::var("AFT_REQUIRE_SIGNAL_SCOPE").as_deref() != Ok("1")
                        && std::env::var("AFT_REQUIRE_LANDLOCK").as_deref() != Ok("1"),
                    "Landlock required but unavailable: {output:?}"
                );
                eprintln!("signal scope test skipped: Landlock unavailable: {output:?}");
                return false;
            }
            let support: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(support["signal_scope_profile"], "worker");
            if support["signal_scope"] != "same_command" {
                assert_eq!(support["signal_scope"], "not_enforced");
                assert!(support["unenforced"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!("signal_scope")));
                assert_ne!(
                    std::env::var("AFT_REQUIRE_SIGNAL_SCOPE").as_deref(),
                    Ok("1"),
                    "signal scoping required but unavailable: {support}"
                );
                eprintln!("signal scope test skipped: kernel does not support Landlock ABI 6; status={support}");
                return false;
            }
        }
        true
    }

    impl Victim {
        fn new() -> Self {
            let mut command = Command::new("/bin/sleep");
            command.arg("60");
            Self(command.spawn().expect("spawn unrelated fixture"))
        }

        fn assert_alive(&mut self) {
            assert!(
                self.0.try_wait().expect("inspect fixture").is_none(),
                "sandboxed signal killed an unrelated same-uid fixture"
            );
        }
    }

    impl Drop for Victim {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn probe_signal(fixture: &mut ProbeFixture, target: &str, signal: &str) -> Output {
        fixture.profile.rewind();
        let mut command = launcher_command(
            fixture.profile.fd(),
            &[
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-c",
                &format!("kill -{signal} -- {target}"),
            ],
        );
        command.env("HOME", &fixture.outside_home);
        command.output().expect("launch signal probe")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_signal_zero_allows_unrelated_same_uid() {
        let mut fixture = ProbeFixture::with_worker_jail(true);
        let mut victim = Victim::new();
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "0");
        victim.assert_alive();
        assert!(
            output.status.success(),
            "outside-process existence probe failed: {output:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_signal_zero_to_unrelated_same_uid_is_refused_by_landlock() {
        // Landlock has no per-signal exception, so the existence probe is
        // refused like a real signal; docs/config.md states this limit.
        if !signal_scope_available() {
            return;
        }
        let mut fixture = ProbeFixture::with_worker_jail(true);
        let mut victim = Victim::new();
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "0");
        assert_refused(output, &mut victim);
    }

    #[test]
    fn native_signal_term_refuses_unrelated_same_uid() {
        if !signal_scope_available() {
            return;
        }
        let mut fixture = ProbeFixture::with_worker_jail(true);
        let mut victim = Victim::new();
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "TERM");
        assert_refused(output, &mut victim);
    }

    #[test]
    fn native_signal_stop_refuses_unrelated_same_uid() {
        if !signal_scope_available() {
            return;
        }
        let mut fixture = ProbeFixture::with_worker_jail(true);
        let mut victim = Victim::new();
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "STOP");
        assert_refused(output, &mut victim);
    }

    #[test]
    fn native_signal_cont_refuses_unrelated_same_uid() {
        if !signal_scope_available() {
            return;
        }
        let mut fixture = ProbeFixture::with_worker_jail(true);
        let mut victim = Victim::new();
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "CONT");
        assert_refused(output, &mut victim);
    }

    fn assert_refused(output: Output, victim: &mut Victim) {
        victim.assert_alive();
        assert_eq!(
            output.status.signal(),
            None,
            "attacker was killed: {output:?}"
        );
        assert!(
            !output.status.success(),
            "kill unexpectedly succeeded: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Operation not permitted"),
            "expected a native signal permission denial, not a launcher error: {output:?}"
        );
    }

    #[test]
    fn native_signal_refuses_unrelated_same_uid() {
        if !signal_scope_available() {
            return;
        }
        let mut fixture = ProbeFixture::with_worker_jail(true);
        let mut victim = Victim::new();
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "KILL");
        assert_refused(output, &mut victim);
    }

    #[test]
    fn native_signal_refuses_separate_identical_sandbox() {
        if !signal_scope_available() {
            return;
        }
        let mut fixture = ProbeFixture::with_worker_jail(true);
        fixture.profile.rewind();
        let mut command = launcher_command(
            fixture.profile.fd(),
            &[
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-c",
                "echo ready; exec sleep 60",
            ],
        );
        command.env("HOME", &fixture.outside_home);
        command.stdout(std::process::Stdio::piped());
        let mut victim = Victim(command.spawn().expect("spawn separately sandboxed fixture"));
        let mut ready = String::new();
        BufReader::new(victim.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .expect("wait for sandbox entry");
        assert_eq!(ready.trim(), "ready", "first sandbox did not start");
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "KILL");
        assert_refused(output, &mut victim);
    }

    #[test]
    fn native_signal_allows_own_children() {
        if !signal_scope_available() {
            return;
        }
        let mut fixture = ProbeFixture::with_worker_jail(true);
        fixture.profile.rewind();
        let output = launcher_command(fixture.profile.fd(), &[
            "/bin/bash", "--noprofile", "--norc", "-c",
            "sleep 2 >/dev/null 2>&1 & p=$!; kill -0 \"$p\" || exit 89; kill -9 \"$p\" || exit 90; wait \"$p\"; test \"$?\" = 137",
        ])
        .env("HOME", &fixture.outside_home)
        .output()
        .expect("launch own-child signal probe");
        assert!(output.status.success(), "own-child kill failed: {output:?}");
    }

    #[test]
    fn native_signal_nonworker_profile_is_unchanged() {
        #[cfg(target_os = "linux")]
        if !landlock_available() {
            assert_ne!(std::env::var("AFT_REQUIRE_LANDLOCK").as_deref(), Ok("1"));
            eprintln!("nonworker signal test skipped: Landlock unavailable");
            return;
        }
        let mut fixture = ProbeFixture::new();
        let mut victim = Victim::new();
        let output = probe_signal(&mut fixture, &victim.0.id().to_string(), "KILL");
        assert!(
            output.status.success(),
            "operator signal unexpectedly refused: {output:?}"
        );
        assert_eq!(victim.0.wait().unwrap().signal(), Some(9));
    }
}

#[cfg(target_os = "linux")]
fn linux_system_read_allow() -> Vec<PathBuf> {
    [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/etc",
        "/proc",
        "/sys/devices/system/cpu",
        "/sys/fs/cgroup",
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/ptmx",
        "/dev/pts",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|path| path.exists())
    .collect()
}

#[cfg(target_os = "linux")]
fn landlock_available() -> bool {
    if std::env::var("AFT_PROBE_FORCE_NO_LANDLOCK").as_deref() == Ok("1") {
        return false;
    }
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new(AFT_BIN)
            .args(["sandbox-launch", "--support"])
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

#[cfg(target_os = "linux")]
#[test]
fn launcher_fd_probe_required_mode_cannot_silently_skip() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "launcher_closes_late_inherited_listener_above_marker_slots",
            "--nocapture",
        ])
        .env("AFT_REQUIRE_LANDLOCK", "1")
        .env("AFT_PROBE_FORCE_NO_LANDLOCK", "1")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "mandatory fd probe must fail when the backend is unavailable, not return success"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("AFT_REQUIRE_LANDLOCK=1 but Landlock ABI is unavailable"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

macro_rules! skip_if_landlock_absent {
    () => {
        #[cfg(target_os = "linux")]
        if !landlock_available() {
            if std::env::var("AFT_REQUIRE_LANDLOCK").as_deref() == Ok("1") {
                panic!("AFT_REQUIRE_LANDLOCK=1 but Landlock ABI is unavailable");
            }
            eprintln!("Landlock probe skipped: mandatory ABI is unavailable");
            return;
        }
    };
}

fn assert_denied(output: &Output, context: &str) {
    assert!(
        !output.status.success(),
        "{context} unexpectedly succeeded; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
fn assert_linux_unenforced_warning(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warnings: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("sandbox-launch: unenforced="))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "expected one structured warning: {stderr}"
    );
    assert!(
        warnings[0].starts_with("sandbox-launch: unenforced=[nested_write_deny,socket_deny]"),
        "unexpected structured warning: {}",
        warnings[0]
    );
}

fn connect_unix_socket(fixture: &mut ProbeFixture, path: &Path) -> Output {
    let python = "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])";
    let script = format!(
        "python3 -B -c {} {}",
        shell_path(Path::new(python)),
        shell_path(path)
    );
    fixture.launch_bash(&script)
}

#[cfg(target_os = "linux")]
#[test]
fn dangling_socket_deny_launches_with_linux_limitation_warning() {
    skip_if_landlock_absent!();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("not-running.sock");
    let alias = root.path().join("docker.sock");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let profile = SandboxProfile::build(
        vec![root.path().to_path_buf()],
        Vec::new(),
        Vec::new(),
        linux_system_read_allow(),
        Vec::new(),
        vec![alias],
        Vec::new(),
        root.path().to_path_buf(),
    )
    .expect("compile dangling socket deny on Linux");
    let inherited = InheritedProfile::new(&profile);
    let output = launcher_command(inherited.fd(), &["/bin/bash", "-c", "echo SHELL_RAN"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "SHELL_RAN");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("sandbox-launch: unenforced=[socket_deny]"),
        "{stderr}"
    );
    assert!(!target.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn dangling_socket_deny_blocks_target_created_after_sandbox_start() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let target = root.join("late.sock");
    let alias = root.join("docker.sock");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let profile = SandboxProfile::build(
        vec![root.clone()],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![alias.clone()],
        Vec::new(),
        root.clone(),
    )
    .expect("compile dangling socket deny");
    assert_eq!(profile.socket_deny, vec![target.clone()]);
    let inherited = InheritedProfile::new(&profile);
    let release = root.join("release");
    let python = "import socket,sys,errno\nfor path in sys.argv[1:]:\n try:\n  socket.socket(socket.AF_UNIX).connect(path)\n except OSError as e:\n  assert e.errno in (errno.EPERM, errno.EACCES), e\n else:\n  raise AssertionError('socket connection escaped deny: ' + path)\nprint('BOTH_DENIED')";
    let script = format!(
        "printf 'ready\\n'; while [ ! -e {} ]; do sleep 0.01; done; python3 -B -c {} {} {}",
        shell_path(&release),
        shell_path(Path::new(python)),
        shell_path(&alias),
        shell_path(&target),
    );
    let mut child = launcher_command(inherited.fd(), &["/bin/bash", "-c", &script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    if ready.is_empty() {
        let output = child.wait_with_output().unwrap();
        panic!("sandboxed shell exited before readiness: {output:?}");
    }
    assert_eq!(ready, "ready\n", "sandboxed shell did not become ready");
    let listener = UnixListener::bind(&target).unwrap();
    listener.set_nonblocking(true).unwrap();
    for path in [&alias, &target] {
        let _connection = std::os::unix::net::UnixStream::connect(path).unwrap();
        listener.accept().expect("unsandboxed positive control");
    }
    fs::write(&release, b"go").unwrap();
    let output = child.wait_with_output().unwrap();
    let mut result = String::new();
    stdout.read_to_string(&mut result).unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(result.trim(), "BOTH_DENIED");
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(target_os = "linux")]
#[test]
fn native_background_exit_marker_survives_landlock_launcher() {
    skip_if_landlock_absent!();
    let fixture = tempfile::tempdir().expect("create marker fixture");
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    for directory in [&project, &storage, &home] {
        fs::create_dir_all(directory).expect("create marker fixture directory");
    }

    let mut aft = AftProcess::spawn_with_env(&[("HOME", std::ffi::OsStr::new(&home))]);
    let configured = aft.send(
        &serde_json::json!({
            "id": "cfg-marker",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.clone(),
            "storage_dir": storage.clone(),
            "bash_permissions": true,
            "config": user_config(serde_json::json!({
                "bash": { "background": true, "rewrite": true },
                "sandbox": { "enabled": true },
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let session = "landlock-marker-session";
    // Use the production background request so SpawnPlan::Launcher wires the
    // exit and failure markers through apply_marker_fd_allowlist onto fds 3/4
    // before the real sandbox-launch process applies Landlock.
    let launch = aft.send(
        &serde_json::json!({
            "id": "launch-marker",
            "method": "bash",
            "session_id": session,
            "params": {
                "command": "exit 37",
                "background": true,
                "permissions_requested": true,
                "compressed": false,
            },
        })
        .to_string(),
    );
    assert_eq!(
        launch["success"], true,
        "background launch failed: {launch:?}"
    );
    let task_id = launch["task_id"].as_str().expect("background task id");

    let started = Instant::now();
    let terminal = loop {
        let status = aft.send(
            &serde_json::json!({
                "id": "status-marker",
                "method": "bash_status",
                "session_id": session,
                "params": { "task_id": task_id },
            })
            .to_string(),
        );
        assert_eq!(status["success"], true, "status failed: {status:?}");
        if matches!(
            status["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out")
        ) {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "sandboxed task never recorded an exit marker: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        terminal["status"], "failed",
        "unexpected terminal state: {terminal:?}"
    );
    assert_eq!(
        terminal["exit_code"], 37,
        "exit marker was not consumed: {terminal:?}"
    );

    let paths = resolve_task(&storage, session, task_id)
        .expect("resolve background task layout")
        .paths;
    assert_eq!(
        fs::read_to_string(&paths.exit).expect("read exit marker"),
        "37",
        "production marker wire did not record the command exit code"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn launcher_support_reports_first_party_backend() {
    skip_if_landlock_absent!();
    let output = Command::new(AFT_BIN)
        .args(["sandbox-launch", "--support"])
        .output()
        .expect("run support probe");
    assert!(output.status.success());
    let support: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse support JSON");
    assert_eq!(support["supported"], true);
    #[cfg(target_os = "macos")]
    assert_eq!(support["backend"], "seatbelt");
    #[cfg(target_os = "linux")]
    {
        assert_eq!(support["backend"], "landlock");
        assert!(support["landlock_abi"].as_str().is_some());
    }
    eprintln!("sandbox support: {support}");
}

#[test]
fn closed_profile_fd_returns_an_error_instead_of_aborting() {
    let output = Command::new(AFT_BIN)
        .args([
            "sandbox-launch",
            "--profile-fd",
            "999999",
            "--",
            "/usr/bin/true",
        ])
        .output()
        .expect("run launcher with closed profile descriptor");
    assert_eq!(
        output.status.code(),
        Some(1),
        "launcher aborted: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("is not open"),
        "unexpected launcher error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn v1_profile_is_a_structured_sandbox_unavailable_failure() {
    let root = tempfile::tempdir().expect("create profile root");
    let root = root.path().canonicalize().expect("canonical profile root");
    let mut profile = NamedTempFile::new().expect("create v1 profile");
    serde_json::to_writer(
        profile.as_file_mut(),
        &serde_json::json!({
            "v": 1,
            "writable_roots": [root],
            "write_deny": [],
            "write_deny_nested": [],
            "read_deny": [],
            "socket_deny": [],
            "cache_roots": [],
            "temp_dir": root,
        }),
    )
    .expect("serialize v1 profile");
    profile.as_file_mut().flush().expect("flush v1 profile");
    profile.as_file_mut().rewind().expect("rewind v1 profile");
    let fd = profile.as_file().as_raw_fd();
    set_close_on_exec(fd, false);

    let output = launcher_command(fd, &["/usr/bin/true"])
        .output()
        .expect("launch with v1 profile");
    set_close_on_exec(fd, true);
    assert_eq!(output.status.code(), Some(78));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("sandbox_unavailable:"));
    assert!(stderr.contains(&format!(
        "unsupported sandbox profile version 1; expected {}",
        aft::sandbox_profile::SANDBOX_PROFILE_VERSION
    )));
}

#[test]
fn target_arguments_are_not_interpreted_as_global_aft_flags() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    fixture.profile.rewind();
    // The shell's builtin printf prints `--version` as data. An external
    // printf is not portable here: uutils (Ubuntu 26.04's default coreutils)
    // treats a trailing `--version` as its own option.
    let output = launcher_command(
        fixture.profile.fd(),
        &["/bin/sh", "-c", "printf '%s\\n' \"$1\"", "sh", "--version"],
    )
    .output()
    .expect("launch target with version argument");
    assert!(output.status.success(), "target failed: {output:?}");
    assert_eq!(output.stdout, b"--version\n");
}

#[test]
fn p1_write_inside_project_is_allowed() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let destination = fixture.project.join("p1-created");
    let output = fixture.launch_bash(&format!("touch {}", shell_path(&destination)));
    assert!(
        output.status.success(),
        "project write failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(destination.exists());
}

#[test]
fn p2_recursive_delete_outside_allowlist_is_denied() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let sentinel = fixture.outside_home.join("keep/sentinel");
    fs::create_dir_all(sentinel.parent().expect("sentinel parent")).expect("create sentinel dir");
    fs::write(&sentinel, b"keep").expect("create sentinel");

    let outside = fixture.outside_home.clone();
    let output = fixture.launch_bash(&format!("rm -rf {}", shell_path(&outside)));
    assert_denied(&output, "recursive delete outside allowlist");
    assert!(
        sentinel.exists(),
        "sandbox allowed deletion of outside data"
    );
}

#[test]
fn p3_nested_children_inherit_confinement() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let destination = fixture.outside_home.join("nested-child-write");
    let inner = format!("touch {}", shell_path(&destination));
    let script = format!("/bin/bash -c {}", shell_path(Path::new(&inner)));
    let output = fixture.launch_bash(&script);
    assert_denied(&output, "nested child write outside allowlist");
    assert!(!destination.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn p4_nested_project_write_denies_are_enforced() {
    let mut fixture = ProbeFixture::new();
    let git_config = fixture.project.join(".git/config");
    let cortex_file = fixture.project.join(".cortexkit/x");
    let script = format!(
        "printf changed > {}; touch {}",
        shell_path(&git_config),
        shell_path(&cortex_file)
    );
    let output = fixture.launch_bash(&script);
    assert_denied(&output, "nested project deny");
    assert_eq!(
        fs::read(&git_config).expect("read git config"),
        b"original\n"
    );
    assert!(!cortex_file.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn missing_project_metadata_remains_write_denied() {
    let root = tempfile::tempdir().expect("create probe root");
    let root = root.path().canonicalize().expect("canonical probe root");
    let project = root.join("project");
    let task_temp = root.join("task-temp");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&task_temp).expect("create task temp");
    let git_dir = project.join(".git");
    let hook = git_dir.join("hooks/pre-commit");
    let profile = SandboxProfile::build(
        vec![project],
        Vec::new(),
        vec![git_dir],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        task_temp,
    )
    .expect("build profile with absent metadata deny");
    let mut inherited = InheritedProfile::new(&profile);
    inherited.rewind();

    let script = format!(
        "mkdir -p {} && printf x > {}",
        shell_path(hook.parent().expect("hook parent")),
        shell_path(&hook)
    );
    let output = launcher_command(inherited.fd(), &["/bin/bash", "-c", &script])
        .output()
        .expect("launch absent metadata probe");

    assert_denied(&output, "absent project metadata write");
    assert!(!hook.exists(), "sandbox created a denied hook");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_git_init_add_and_commit_pass_but_hooks_write_fails() {
    let root = tempfile::tempdir().expect("create probe root");
    let root = root.path().canonicalize().expect("canonical probe root");
    let project = root.join("project");
    let task_temp = root.join("task-temp");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&task_temp).expect("create task temp");
    let hooks = project.join(".git/hooks");
    let profile = SandboxProfile::build(
        vec![project.clone()],
        vec![hooks.clone()],
        Vec::new(),
        Vec::new(),
        vec![hooks.clone()],
        Vec::new(),
        Vec::new(),
        task_temp,
    )
    .expect("build git-friendly profile");
    let mut inherited = InheritedProfile::new(&profile);
    inherited.rewind();

    let output = launcher_command(
        inherited.fd(),
        &[
            "/bin/bash",
            "-c",
            "git init -q --template= && printf tracked > tracked && git add tracked && git -c user.name='AFT Test' -c user.email=aft@example.invalid commit -qm initial",
        ],
    )
    .current_dir(&project)
    .output()
    .expect("launch git regression probe");
    assert!(
        output.status.success(),
        "git init/add/commit failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(project.join(".git/index").is_file());

    inherited.rewind();
    let hook = hooks.join("pre-commit");
    let output = launcher_command(
        inherited.fd(),
        &[
            "/bin/bash",
            "-c",
            &format!("printf pwned > {}", shell_path(&hook)),
        ],
    )
    .current_dir(&project)
    .output()
    .expect("launch hook-write probe");
    assert_denied(&output, "hooks write");
    assert!(!hook.exists(), "sandbox created a denied hook");
}

#[cfg(target_os = "macos")]
#[test]
fn read_deny_created_after_sandbox_start_remains_denied() {
    let root = tempfile::tempdir().expect("create probe root");
    let root = root.path().canonicalize().expect("canonical probe root");
    let project = root.join("project");
    let task_temp = root.join("task-temp");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&task_temp).expect("create task temp");
    let secret_dir = root.join("late-secret");
    let secret_file = secret_dir.join("token");
    let release = root.join("release");
    let profile = SandboxProfile::build(
        vec![project],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![secret_dir.clone()],
        Vec::new(),
        Vec::new(),
        task_temp,
    )
    .expect("build profile with absent read deny");
    let mut inherited = InheritedProfile::new(&profile);
    inherited.rewind();
    let script = format!(
        "printf 'ready\\n'; while [ ! -e {} ]; do sleep 0.01; done; cat {}",
        shell_path(&release),
        shell_path(&secret_file)
    );
    let mut child = launcher_command(inherited.fd(), &["/bin/bash", "-c", &script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("launch late read-deny probe");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut ready = String::new();
    stdout.read_line(&mut ready).expect("read readiness line");
    assert_eq!(ready, "ready\n", "sandboxed shell did not become ready");

    fs::create_dir(&secret_dir).expect("create late secret directory");
    fs::write(&secret_file, b"late-secret\n").expect("write late secret");
    fs::write(&release, b"go").expect("release sandboxed shell");
    let status = child.wait().expect("wait for late read-deny probe");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_string(&mut stderr)
        .expect("read child stderr");

    assert!(
        !status.success(),
        "late-created secret read unexpectedly succeeded; stderr={stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn home_directory_created_after_launch_remains_read_denied() {
    skip_if_landlock_absent!();
    let root = tempfile::tempdir().expect("create probe root");
    let root = root.path().canonicalize().expect("canonical probe root");
    let home = root.join("home");
    let existing = home.join("existing");
    let project = root.join("project");
    let task_temp = root.join("task-temp");
    for directory in [&existing, &project, &task_temp] {
        fs::create_dir_all(directory).expect("create probe directory");
    }
    let mut read_allow = linux_system_read_allow();
    read_allow.extend([existing, project.clone(), task_temp.clone()]);
    let profile = SandboxProfile::build(
        vec![project.clone()],
        Vec::new(),
        Vec::new(),
        read_allow,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        task_temp,
    )
    .expect("build late-HOME profile");
    let mut inherited = InheritedProfile::new(&profile);
    inherited.rewind();
    let release = project.join("release");
    let late_dir = home.join("late-directory");
    let late_file = late_dir.join("value");
    let script = format!(
        "printf 'ready\\n'; while [ ! -e {} ]; do sleep 0.01; done; cat {}",
        shell_path(&release),
        shell_path(&late_file)
    );
    let mut child = launcher_command(inherited.fd(), &["/bin/bash", "-c", &script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("launch late HOME probe");
    let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut ready = String::new();
    stdout.read_line(&mut ready).expect("read readiness line");
    assert_eq!(ready, "ready\n");

    fs::create_dir(&late_dir).expect("create late HOME directory");
    fs::write(&late_file, b"late-value\n").expect("write late HOME file");
    fs::write(&release, b"go").expect("release child");
    let status = child.wait().expect("wait for late HOME probe");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_string(&mut stderr)
        .expect("read child stderr");
    assert!(!status.success(), "late HOME read succeeded: {stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn p4_linux_records_nested_project_deny_gap() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let cortex_destination = fixture.project.join(".cortexkit/linux-gap");
    let hook_destination = fixture.project.join(".git/hooks/linux-gap");
    let output = fixture.launch_bash(&format!(
        "touch {} {}",
        shell_path(&cortex_destination),
        shell_path(&hook_destination)
    ));
    assert!(
        output.status.success(),
        "unexpected result changed: {output:?}"
    );
    assert!(cortex_destination.exists());
    assert!(hook_destination.exists());
    assert_linux_unenforced_warning(&output);
    eprintln!("P4 Linux observed ALLOWED with nested_write_deny warning");
}

#[test]
fn p5_secret_read_is_denied_and_other_reads_are_allowed() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let secret_file = fixture.secret.join("id_probe");
    let secret_output = fixture.launch_bash(&format!("cat {}", shell_path(&secret_file)));
    assert_denied(&secret_output, "secret read");
    #[cfg(target_os = "linux")]
    assert_linux_unenforced_warning(&secret_output);

    let arbitrary_file = fixture.arbitrary_file.clone();
    let ordinary_output = fixture.launch_bash(&format!("cat {}", shell_path(&arbitrary_file)));
    assert!(
        ordinary_output.status.success(),
        "ordinary read failed: {}",
        String::from_utf8_lossy(&ordinary_output.stderr)
    );
    assert_eq!(ordinary_output.stdout, b"ordinary\n");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_vanished_listed_read_grant_launches_without_broadening_access() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let vanished = fixture.outside_home.join("listed-sibling");
    fs::create_dir(&vanished).unwrap();
    let mut profile: SandboxProfile =
        serde_json::from_slice(&fs::read(fixture.profile.path()).unwrap()).unwrap();
    profile.read_allow.push(vanished.clone());
    profile.read_allow_listed.push(vanished.clone());
    fixture.profile = InheritedProfile::new(&profile);
    fs::remove_dir(&vanished).unwrap();
    let new_sibling = fixture.outside_home.join("unlisted-sibling");
    fs::write(&new_sibling, "must remain unreadable").unwrap();
    let output = fixture.launch_bash(&format!(
        "cat {} || exit 10; if cat {} >/dev/null 2>&1; then exit 11; fi; if cat {} >/dev/null 2>&1; then exit 12; fi",
        shell_path(&fixture.arbitrary_file),
        shell_path(&fixture.secret.join("id_probe")),
        shell_path(&new_sibling),
    ));
    assert!(
        output.status.success(),
        "vanished listed read grant refused or widened access: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ordinary\n");
}

#[cfg(target_os = "macos")]
#[test]
fn task_store_paths_are_denied_while_explicit_stdin_remains_readable() {
    let root = tempfile::tempdir().expect("create task-store probe root");
    let store = root.path().join("bash-tasks/session");
    let own_control = store.join("bash-0000000000000001/control");
    let own_io = store.join("bash-0000000000000001/io");
    let sibling_control = store.join("bash-0000000000000002/control");
    let temp = own_io.join("temp");
    for path in [&own_control, &own_io, &sibling_control, &temp] {
        fs::create_dir_all(path).expect("create task-store probe directory");
    }
    let own_payload_path = own_control.join("command.sh");
    let sibling_payload_path = sibling_control.join("command.sh");
    fs::write(&own_payload_path, b"approved-payload\n").expect("write own payload");
    fs::write(&sibling_payload_path, b"sibling-secret\n").expect("write sibling payload");
    let own_payload = fs::File::open(&own_payload_path).expect("open own payload before sandbox");
    // Payloads intentionally carried by the caller use stdio; arbitrary
    // inherited descriptors above the reserved marker slots are closed.

    let profile = SandboxProfile::build(
        vec![own_io],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![store.canonicalize().expect("canonical task store")],
        Vec::new(),
        Vec::new(),
        temp,
    )
    .expect("build task-store denial profile");
    let mut inherited = InheritedProfile::new(&profile);
    inherited.rewind();
    let script = format!("cat; cat {}", shell_path(&sibling_payload_path));
    let output = launcher_command(inherited.fd(), &["/bin/bash", "-c", &script])
        .stdin(std::process::Stdio::from(own_payload))
        .output()
        .expect("launch task-store Seatbelt probe");

    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "approved-payload\n"
    );
    assert!(
        !output.status.success(),
        "sibling control read unexpectedly succeeded"
    );
    assert_eq!(
        fs::read(&sibling_payload_path).expect("read sibling victim after probe"),
        b"sibling-secret\n"
    );
}
#[cfg(target_os = "macos")]
#[test]
fn macos_secret_floor_write_is_denied_under_enclosing_write_root() {
    let root = tempfile::tempdir().expect("create probe root");
    let root = root.path().canonicalize().expect("canonical probe root");
    let home = root.join("fake-home");
    let secret_floor = home.join(".ssh");
    let authorized_keys = secret_floor.join("authorized_keys");
    let work = home.join("work");
    let allowed_file = work.join("allowed");
    let task_temp = home.join("task-temp");
    for directory in [&secret_floor, &work, &task_temp] {
        fs::create_dir_all(directory).expect("create fake HOME fixture");
    }
    fs::write(&authorized_keys, b"original\n").expect("write fake authorized_keys");
    let profile = SandboxProfile::build(
        vec![home.clone(), work],
        vec![secret_floor.clone()],
        Vec::new(),
        Vec::new(),
        vec![secret_floor],
        Vec::new(),
        Vec::new(),
        task_temp,
    )
    .expect("build profile with secret write floor");
    let mut inherited = InheritedProfile::new(&profile);
    inherited.rewind();
    let script = format!(
        "printf allowed > {}; printf changed >> {}",
        shell_path(&allowed_file),
        shell_path(&authorized_keys)
    );
    let output = launcher_command(inherited.fd(), &["/bin/bash", "-c", &script])
        .env("HOME", &home)
        .output()
        .expect("launch secret-floor write probe");

    assert_denied(&output, "secret-floor append");
    assert_eq!(
        fs::read(&authorized_keys).expect("read fake authorized_keys"),
        b"original\n"
    );
    assert_eq!(
        fs::read(&allowed_file).expect("read disjoint allowed file"),
        b"allowed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_ascii_lowercase()
            .contains("operation not permitted"),
        "secret-floor write did not fail with EPERM: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    inherited.rewind();
    let read_output = launcher_command(
        inherited.fd(),
        &[
            "/bin/bash",
            "-c",
            &format!("cat {}", shell_path(&authorized_keys)),
        ],
    )
    .env("HOME", &home)
    .output()
    .expect("launch secret-floor read probe");
    assert_denied(&read_output, "secret-floor read");
}

#[test]
fn p6_symlink_escape_is_denied_at_access_time() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let outside_target = fixture.outside_home.join("symlink-target");
    fs::create_dir(&outside_target).expect("create symlink target");
    let link = fixture.project.join("evil");
    std::os::unix::fs::symlink(&outside_target, &link).expect("plant symlink");
    let escaped_file = outside_target.join("escaped");

    let output = fixture.launch_bash(&format!(
        "printf escaped > {}",
        shell_path(&link.join("escaped"))
    ));
    assert_denied(&output, "write through symlink outside allowlist");
    assert!(!escaped_file.exists());
}

#[test]
fn p7_preexisting_hardlink_alias_is_asserted_writable_gap() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let outside_file = fixture.outside_home.join("hardlink-target");
    let inside_link = fixture.project.join("hardlink-inside");
    fs::write(&outside_file, b"before\n").expect("write hardlink target");
    fs::hard_link(&outside_file, &inside_link).expect("create pre-existing hardlink");

    let output = fixture.launch_bash(&format!("printf after > {}", shell_path(&inside_link)));
    assert!(
        output.status.success(),
        "pre-existing hard-link alias should expose the trusted project path: {output:?}"
    );
    assert_eq!(
        fs::read(&outside_file).expect("read hardlink target"),
        b"after"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_refer_denies_laundering_secret_with_a_new_hard_link() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let source = fixture.secret.join("id_probe");
    let destination = fixture.project.join("laundered-secret");
    let output = fixture.launch_bash(&format!(
        "ln {} {}",
        shell_path(&source),
        shell_path(&destination)
    ));

    assert_denied(&output, "hard-link secret laundering");
    assert!(!destination.exists());
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    assert!(
        stderr.contains("cross-device link") || stderr.contains("permission denied"),
        "hard-link denial should report EXDEV or EACCES: {stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_refer_allows_rename_inside_writable_project() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let source = fixture.project.join("rename-source");
    let destination = fixture.project.join("rename-destination");
    fs::write(&source, b"rename-me").expect("create rename source");
    let output = fixture.launch_bash(&format!(
        "mv {} {}",
        shell_path(&source),
        shell_path(&destination)
    ));

    assert!(
        output.status.success(),
        "in-project rename failed: {output:?}"
    );
    assert!(!source.exists());
    assert_eq!(fs::read(&destination).expect("renamed file"), b"rename-me");
}

#[cfg(target_os = "macos")]
#[test]
fn p8_docker_and_agent_socket_connects_are_denied() {
    let mut fixture = ProbeFixture::new();
    let docker_socket = fixture.docker_socket.clone();
    let agent_socket = fixture.agent_socket.clone();
    let docker_listener = UnixListener::bind(&docker_socket).expect("bind fake docker socket");
    let agent_listener = UnixListener::bind(&agent_socket).expect("bind fake agent socket");
    docker_listener
        .set_nonblocking(true)
        .expect("make docker listener nonblocking");
    agent_listener
        .set_nonblocking(true)
        .expect("make agent listener nonblocking");

    let docker_output = connect_unix_socket(&mut fixture, &docker_socket);
    assert_denied(&docker_output, "fake docker socket connect");

    let agent_output = connect_unix_socket(&mut fixture, &agent_socket);
    assert_denied(&agent_output, "fake agent socket connect");

    assert!(
        docker_listener.accept().is_err(),
        "docker listener accepted a connection"
    );
    assert!(
        agent_listener.accept().is_err(),
        "agent listener accepted a connection"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn p8_linux_records_socket_connect_gap() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let socket_path = fixture.docker_socket.clone();
    let listener = UnixListener::bind(&socket_path).expect("bind fake docker socket");
    listener
        .set_nonblocking(true)
        .expect("make listener nonblocking");
    let output = connect_unix_socket(&mut fixture, &socket_path);
    assert!(
        output.status.success(),
        "unexpected result changed: {output:?}"
    );
    assert!(
        listener.accept().is_ok(),
        "listener did not observe the allowed connect"
    );
    assert_linux_unenforced_warning(&output);
    eprintln!("P8 Linux observed ALLOWED with socket_deny warning");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_root_and_cwd_aliases_cannot_escape_the_read_floor() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let secret = fixture.secret.join("id_probe");
    let root_alias = PathBuf::from("/proc/self/root").join(
        secret
            .strip_prefix("/")
            .expect("secret fixture is absolute"),
    );
    let root_output = fixture.launch_bash(&format!("cat {}", shell_path(&root_alias)));
    assert_denied(&root_output, "/proc/self/root secret alias");
    assert!(!root_output
        .stdout
        .windows(b"probe-secret".len())
        .any(|bytes| bytes == b"probe-secret"));

    fixture.profile.rewind();
    let cwd_output = launcher_command(
        fixture.profile.fd(),
        &[
            "/bin/bash",
            "-c",
            "cat /proc/self/cwd/../outside-home/.ssh-like/id_probe",
        ],
    )
    .current_dir(&fixture.project)
    .output()
    .expect("launch /proc/self/cwd alias probe");
    assert_denied(&cwd_output, "/proc/self/cwd secret alias");
    assert!(!cwd_output
        .stdout
        .windows(b"probe-secret".len())
        .any(|bytes| bytes == b"probe-secret"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_close_range_blocks_proc_and_dev_fd_secret_aliases() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let secret = std::fs::File::open(fixture.secret.join("id_probe")).expect("open secret fd");
    let secret_fd = secret.as_raw_fd();
    set_close_on_exec(secret_fd, false);

    for alias in [
        format!("/proc/self/fd/{secret_fd}"),
        format!("/dev/fd/{secret_fd}"),
    ] {
        let output = fixture.launch_bash(&format!("cat {}", shell_path(Path::new(&alias))));
        assert_denied(&output, "inherited secret descriptor alias");
        assert!(!output
            .stdout
            .windows(b"probe-secret".len())
            .any(|bytes| bytes == b"probe-secret"));
    }
    set_close_on_exec(secret_fd, true);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_dev_kmsg_is_not_readable() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let output = fixture.launch_bash("head -c 1 /dev/kmsg");
    assert_denied(&output, "/dev/kmsg read");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_bind_mount_alias_is_readable_only_when_the_host_can_create_it() {
    skip_if_landlock_absent!();
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("bind-mount alias probe skipped: test process is not root");
        return;
    }
    let mut fixture = ProbeFixture::new();
    let alias = fixture.project.join("bind-secret");
    fs::create_dir(&alias).expect("bind alias mountpoint");
    let status = Command::new("mount")
        .args(["--bind"])
        .arg(&fixture.secret)
        .arg(&alias)
        .status()
        .expect("invoke mount");
    if !status.success() {
        eprintln!("bind-mount alias probe skipped: mount capability unavailable");
        return;
    }

    let output = fixture.launch_bash(&format!("cat {}", shell_path(&alias.join("id_probe"))));
    let unmount = Command::new("umount")
        .arg(&alias)
        .status()
        .expect("invoke umount");
    assert!(unmount.success(), "failed to unmount bind alias");
    assert!(
        output.status.success(),
        "pre-existing bind alias is a documented readable gap: {output:?}"
    );
    assert_eq!(output.stdout, b"probe-secret\n");
}

#[test]
fn p9_pty_child_and_nested_child_remain_confined() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let destination = fixture.outside_home.join("pty-nested-write");
    let inner = format!("touch {}", shell_path(&destination));
    let script = format!(
        "/bin/bash -c {} || true; test ! -e {}",
        shell_path(Path::new(&inner)),
        shell_path(&destination)
    );
    fixture.profile.rewind();

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let output_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output).expect("read PTY output");
        output
    });

    // portable_pty intentionally closes non-stdio descriptors. A tiny shell
    // opens the serialized profile onto fd 9, then immediately execs the real
    // launcher so the process carrying the sandbox remains single-threaded.
    let launcher_script = format!(
        "exec 9< {}; exec {} sandbox-launch --profile-fd 9 -- /bin/bash -c {}",
        shell_path(fixture.profile.path()),
        shell_path(Path::new(AFT_BIN)),
        shell_path(Path::new(&script))
    );
    let mut command = CommandBuilder::new("/bin/bash");
    command.args(["-c", &launcher_script]);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("spawn PTY launcher");
    drop(pair.slave);
    let status = child.wait().expect("wait for PTY launcher");
    drop(pair.master);
    let pty_output = output_reader.join().expect("join PTY reader");

    assert!(
        status.success(),
        "PTY probe command failed: {status:?}; output={}",
        String::from_utf8_lossy(&pty_output)
    );
    assert!(!destination.exists(), "PTY child escaped write confinement");
}

#[test]
fn p10_launcher_latency_delta_is_measured_over_twenty_iterations() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    const ITERATIONS: u32 = 20;

    for _ in 0..2 {
        assert!(Command::new("/bin/bash")
            .args(["-c", "true"])
            .status()
            .expect("bare warmup")
            .success());
        fixture.profile.rewind();
        assert!(
            launcher_command(fixture.profile.fd(), &["/bin/bash", "-c", "true"])
                .status()
                .expect("launcher warmup")
                .success()
        );
    }

    let bare_start = Instant::now();
    for _ in 0..ITERATIONS {
        assert!(Command::new("/bin/bash")
            .args(["-c", "true"])
            .status()
            .expect("bare spawn")
            .success());
    }
    let bare = bare_start.elapsed();

    let launcher_start = Instant::now();
    for _ in 0..ITERATIONS {
        fixture.profile.rewind();
        assert!(
            launcher_command(fixture.profile.fd(), &["/bin/bash", "-c", "true"])
                .status()
                .expect("launcher spawn")
                .success()
        );
    }
    let launcher = launcher_start.elapsed();

    let bare_ms = duration_ms(bare) / f64::from(ITERATIONS);
    let launcher_ms = duration_ms(launcher) / f64::from(ITERATIONS);
    eprintln!(
        "P10 iterations={ITERATIONS} bare_avg_ms={bare_ms:.3} launcher_avg_ms={launcher_ms:.3} delta_ms={:.3}",
        launcher_ms - bare_ms
    );
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[test]
fn access_policy_compiled_file_grant_does_not_widen_shell_writes() {
    skip_if_landlock_absent!();
    use aft::access_policy::{AccessGrant, AccessOperation, AccessPolicy, GrantKind};
    let fixture = tempfile::tempdir().unwrap();
    let base = fixture.path().canonicalize().unwrap();
    let root = base.join("worktree");
    let outside = base.join("outside");
    let directory = outside.join("approved-directory");
    let temp = base.join("task-temp");
    for path in [&root, &directory, &temp] {
        fs::create_dir_all(path).unwrap();
    }
    let file = outside.join("approved-file");
    let sibling = outside.join("sibling");
    fs::write(&file, "original").unwrap();
    let mut policy = AccessPolicy::from_config(
        &aft::config::Config::default(),
        &base.join("store"),
        vec![root.clone()],
        &base.join("home"),
    );
    policy.grants = vec![
        AccessGrant {
            path: file.clone(),
            kind: GrantKind::File,
            operation: AccessOperation::Write,
        },
        AccessGrant {
            path: directory.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        },
    ];
    assert_eq!(
        policy.decide(AccessOperation::Write, &file, false),
        aft::access_policy::AccessDecision::Allow
    );
    let profile = policy.compile_shell(temp, Vec::new(), Vec::new()).unwrap();
    #[cfg(target_os = "linux")]
    let profile = {
        let mut profile = profile;
        profile.read_allow = linux_system_read_allow();
        profile.read_allow.extend([root.clone(), directory.clone()]);
        profile
    };
    let mut profile = InheritedProfile::new(&profile);
    let output = launcher_command(
        profile.fd(),
        &[
            "/bin/bash",
            "-c",
            &format!("printf forbidden > {}", shell_path(&file)),
        ],
    )
    .output()
    .unwrap();
    assert_denied(&output, "file grant must remain in-process only");
    assert_eq!(fs::read_to_string(&file).unwrap(), "original");
    profile.rewind();
    let output = launcher_command(
        profile.fd(),
        &[
            "/bin/bash",
            "-c",
            &format!("printf forbidden > {}", shell_path(&sibling)),
        ],
    )
    .output()
    .unwrap();
    assert_denied(&output, "a file grant cannot grant its sibling");
    assert!(!sibling.exists());
    let approved = directory.join("new-file");
    profile.rewind();
    let output = launcher_command(
        profile.fd(),
        &[
            "/bin/bash",
            "-c",
            &format!("printf approved > {}", shell_path(&approved)),
        ],
    )
    .output()
    .unwrap();
    assert!(output.status.success(), "directory grant: {:?}", output);
    assert_eq!(fs::read_to_string(approved).unwrap(), "approved");
}

#[test]
fn access_policy_builtin_floor_inside_worktree_refuses_shell_launch() {
    skip_if_landlock_absent!();
    use aft::access_policy::{
        AccessDecision, AccessGrant, AccessOperation, AccessPolicy, GrantKind,
    };
    let fixture = tempfile::tempdir().unwrap();
    let base = fixture.path().canonicalize().unwrap();
    let root = base.join("worktree");
    let temp = base.join("task-temp");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&temp).unwrap();
    fs::create_dir(root.join(".ssh")).unwrap();
    let denied = root.join(".ssh/id_test");
    fs::write(&denied, "must-not-leak").unwrap();
    let mut config = aft::config::Config::default();
    config.project_root = Some(root.clone());
    let mut policy =
        AccessPolicy::from_config(&config, &base.join("store"), vec![root.clone()], &root);
    policy.outside = AccessDecision::Allow;
    policy.grants.push(AccessGrant {
        path: root.clone(),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Write,
    });
    assert_eq!(
        policy.decide(AccessOperation::Read, &denied, true),
        AccessDecision::Deny(aft::access_policy::AccessDenial::Floor)
    );
    let profile = policy.compile_shell(temp, Vec::new(), Vec::new()).unwrap();
    #[cfg(target_os = "linux")]
    let profile = {
        let mut profile = profile;
        profile.read_allow = linux_system_read_allow();
        // Landlock cannot subtract a nested write floor from an enclosing root;
        // launch validation must refuse this profile instead of weakening it.
        profile
    };
    let profile = InheritedProfile::new(&profile);
    let output = launcher_command(
        profile.fd(),
        &["/bin/bash", "-c", &format!("cat {}", shell_path(&denied))],
    )
    .output()
    .unwrap();
    assert_denied(&output, "absolute floor inside the worker's own worktree");
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            output.status.code(),
            Some(78),
            "launch must refuse the overlapping built-in floor: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("sandbox_unavailable:"),
            "{output:?}"
        );
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains("must-not-leak"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_support_reports_sccache_server_unblockable_on_landlock() {
    let output = Command::new(AFT_BIN)
        .args(["sandbox-launch", "--support"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let support: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(support["backend"], "landlock");
    // Kernels before Landlock ABI 6 cannot scope signals, so the worker signal
    // limit is listed as unenforced there and nowhere else.
    let mut expected = vec!["sccache_server_unblockable_on_landlock"];
    match support["signal_scope"].as_str() {
        Some("same_command") => {}
        Some("not_enforced") => expected.push("signal_scope"),
        other => panic!("unexpected signal_scope {other:?}: {support}"),
    }
    assert_eq!(support["unenforced"], serde_json::json!(expected));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_jailed_launch_reports_sccache_gap_without_blocking_tcp() {
    let mut fixture = ProbeFixture::new();
    let mut profile: SandboxProfile =
        serde_json::from_reader(fixture.profile.file.as_file_mut()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    profile.loopback_tcp_connect_deny = vec![4226, port];
    fixture.profile = InheritedProfile::new(&profile);
    let output = fixture.launch_bash(&format!(
        "echo OK; echo connected >/dev/tcp/127.0.0.1/{port}"
    ));
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("sccache_server_unblockable_on_landlock"),
        "{output:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_reports_unenforced_credential_filename_prefixes() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    let mut profile: SandboxProfile =
        serde_json::from_reader(fixture.profile.file.as_file_mut()).unwrap();
    profile
        .deny_prefixes
        .push(fixture.secret.join("credentials"));
    fixture.profile = InheritedProfile::new(&profile);
    let output = fixture.launch_bash("printf PREFIX_PROBE_RAN");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"PREFIX_PROBE_RAN");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("credential_filename_prefix_future_entries_unenforced_on_landlock"),
        "{output:?}"
    );
}

#[path = "helpers/session_output.rs"]
mod session_output;

fn session_output_probe() -> (ProbeFixture, session_output::SessionOutput) {
    let mut fixture = ProbeFixture::new();
    let storage = fixture.outside_home.join("storage.with[regex]+chars");
    let outputs = session_output::SessionOutput::new(&storage, "probe-session");
    let mut profile: SandboxProfile =
        serde_json::from_reader(fs::File::open(fixture.profile.path()).unwrap()).unwrap();
    profile.data_policy.deny.push(storage);
    profile.data_policy.session_task_read_dir = Some(outputs.own.session_dir.clone());
    fixture.profile = InheritedProfile::new(&profile);
    (fixture, outputs)
}

#[test]
fn shell_reads_own_saved_task_output() {
    skip_if_landlock_absent!();
    let (mut fixture, outputs) = session_output_probe();
    let copied = fixture.project.join("gate.log");
    let output = fixture.launch_bash(&format!(
        "cat {} && cp {} {}",
        shell_path(&outputs.own.stdout),
        shell_path(&outputs.own.stdout),
        shell_path(&copied),
    ));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"saved-output\n");
    assert_eq!(fs::read(&copied).unwrap(), b"saved-output\n");
    #[cfg(target_os = "macos")]
    if let Ok(alias) = outputs.own.stdout.strip_prefix("/private") {
        let output =
            fixture.launch_bash(&format!("cat {}", shell_path(&Path::new("/").join(alias))));
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"saved-output\n");
    }
}

#[test]
fn shell_denies_other_session_saved_output() {
    skip_if_landlock_absent!();
    let (mut fixture, outputs) = session_output_probe();
    for path in [&outputs.other.stdout, &outputs.other_harness.stdout] {
        let output = fixture.launch_bash(&format!("cat {}", shell_path(path)));
        assert_denied(&output, "other session task output");
        assert!(output.stdout.is_empty(), "{output:?}");
    }
}

#[test]
fn shell_denies_saved_task_environment() {
    skip_if_landlock_absent!();
    let (mut fixture, outputs) = session_output_probe();
    let output = fixture.launch_bash(&format!("cat {}", shell_path(&outputs.own.environment)));
    assert_denied(&output, "same-session task environment");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn shell_denies_private_storage_and_output_symlinks() {
    skip_if_landlock_absent!();
    let (mut fixture, outputs) = session_output_probe();
    for path in outputs.private.iter().chain([&outputs.other.stdout]) {
        let output = fixture.launch_bash(&format!("cat {}", shell_path(path)));
        assert_denied(&output, "private storage");
    }
    fs::remove_file(&outputs.own.stdout).unwrap();
    for target in outputs.private.iter().chain([&outputs.other.stdout]) {
        std::os::unix::fs::symlink(target, &outputs.own.stdout).unwrap();
        let output = fixture.launch_bash(&format!("cat {}", shell_path(&outputs.own.stdout)));
        assert_denied(&output, "symlink outside task read grant");
        assert!(output.stdout.is_empty(), "{output:?}");
        fs::remove_file(&outputs.own.stdout).unwrap();
    }
}

#[test]
fn shell_cannot_write_saved_task_output() {
    skip_if_landlock_absent!();
    let (mut fixture, outputs) = session_output_probe();
    for path in [&outputs.own.stdout, &outputs.own.io_dir.join("new-file")] {
        let output = fixture.launch_bash(&format!("printf tampered > {}", shell_path(path)));
        assert_denied(&output, "saved output write");
    }
    assert_eq!(fs::read(&outputs.own.stdout).unwrap(), b"saved-output\n");
}

#[test]
fn launcher_closes_late_inherited_listener_above_marker_slots() {
    #[cfg(target_os = "linux")]
    skip_if_landlock_absent!();
    use std::os::unix::process::CommandExt;
    let mut fixture = ProbeFixture::new();
    let listener =
        std::os::unix::net::UnixListener::bind(fixture.project.join("listener.sock")).unwrap();
    let source = listener.as_raw_fd();
    let profile_fd = fixture.profile.fd();
    fixture.profile.rewind();
    let script="import errno,os\nfor fd in [5,10,128]:\n try:\n  os.fstat(fd)\n except OSError as e:\n  assert e.errno == errno.EBADF\n else:\n  raise AssertionError('inherited broker listener remained open')\nprint('LATE_LISTENERS_CLOSED')\n";
    let mut command = launcher_command(131, &["python3", "-c", script]);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(profile_fd, 131) < 0 || libc::fcntl(131, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for target in [5, 10, 128] {
                if libc::dup2(source, target) < 0 || libc::fcntl(target, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if ![5, 10, 128, 131].contains(&source) {
                libc::close(source);
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("LATE_LISTENERS_CLOSED"));
}

#[test]
fn launcher_failure_marker_cannot_be_forged_by_jailed_exec() {
    #[cfg(target_os = "linux")]
    skip_if_landlock_absent!();
    use std::os::unix::process::CommandExt;
    for slot in [3, 4] {
        let mut fixture = ProbeFixture::new();
        fixture.profile.rewind();
        let marker_path = fixture.outside_home.join("supervisor-failure-marker");
        let marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
            .unwrap();
        let profile_fd = fixture.profile.fd();
        let marker_fd = marker.as_raw_fd();
        let script="import errno,os,sys\nfd=int(sys.argv[1])\ntry:\n os.write(fd,b'sandbox_unavailable')\nexcept OSError as e:\n assert e.errno==errno.EBADF\n print('FAILURE_MARKER_CLOSED')\nelse:\n raise AssertionError('jailed exec can forge sandbox_unavailable')\n";
        let mut command = Command::new(AFT_BIN);
        command.args([
            "sandbox-launch",
            "--profile-fd",
            "130",
            "--failure-fd",
            &slot.to_string(),
            "--",
            "python3",
            "-c",
            script,
            &slot.to_string(),
        ]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(profile_fd, 130) < 0 || libc::fcntl(130, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(marker_fd, slot) < 0 || libc::fcntl(slot, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "reserved failure slot {slot} reached jailed exec: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("FAILURE_MARKER_CLOSED"));
        assert!(
            fs::read(marker_path).unwrap().is_empty(),
            "the supervisor's marker must remain unforgeable"
        );
    }
}
