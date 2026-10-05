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
        let profile = SandboxProfile::build(
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
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new(AFT_BIN)
            .args(["sandbox-launch", "--support"])
            .output()
            .is_ok_and(|output| output.status.success())
    })
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
    assert!(stderr.contains("unsupported sandbox profile version 1; expected 2"));
}

#[test]
fn target_arguments_are_not_interpreted_as_global_aft_flags() {
    skip_if_landlock_absent!();
    let mut fixture = ProbeFixture::new();
    fixture.profile.rewind();
    let output = launcher_command(
        fixture.profile.fd(),
        &["/usr/bin/printf", "%s\n", "--version"],
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

#[cfg(target_os = "macos")]
#[test]
fn task_store_paths_are_denied_while_held_payload_fd_remains_readable() {
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
    let payload_fd = own_payload.as_raw_fd();
    set_close_on_exec(payload_fd, false);

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
    let script = format!(
        "cat /dev/fd/{payload_fd}; cat {}",
        shell_path(&sibling_payload_path)
    );
    let output = launcher_command(inherited.fd(), &["/bin/bash", "-c", &script])
        .output()
        .expect("launch task-store Seatbelt probe");
    set_close_on_exec(payload_fd, true);

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
