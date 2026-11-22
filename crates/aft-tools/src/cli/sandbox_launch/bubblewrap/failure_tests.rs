use super::*;
use crate::cli::sandbox_launch::{self, report_failure, SANDBOX_UNAVAILABLE_EXIT_CODE};
use std::ffi::OsString;
use std::os::fd::{IntoRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

const UNAVAILABLE: &str = "sandbox_unavailable";
const CHILD: &str = "AFT_BWRAP_FAILURE_TEST_CHILD";
const STATUS_PROBE: &str = "AFT_BWRAP_FAILURE_TEST_PROBE";
const NO_USERNS: &str = "bwrap: No permissions to create a new namespace, likely because the kernel does not allow non-privileged user namespaces.";

#[test]
#[ignore = "subprocess for tests that install confinement or exec a payload"]
fn bwrap_failures_child() {
    let stage = std::env::var(CHILD).unwrap();
    let profile: SandboxProfile = serde_json::from_str(&std::env::var("PROFILE").unwrap()).unwrap();
    let payload = serde_json::from_str::<Vec<String>>(&std::env::var("PAYLOAD").unwrap())
        .unwrap()
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let marker = PathBuf::from(std::env::var_os("MARKER").unwrap());
    let result = match stage.as_str() {
        "fallback" => {
            let args = LaunchArgs {
                failure_fd: Some(4),
                failure_marker: Some(marker),
                command: payload,
                ..Default::default()
            };
            let probe_path = PathBuf::from(std::env::var_os(STATUS_PROBE).unwrap());
            report_failure(
                &args,
                launch_with_probe(&args, profile, || probe_at(&probe_path)),
            )
        }
        "inner" => {
            let mut input = memfd(c"aft-test-inner-profile").unwrap();
            serde_json::to_writer(
                &mut input,
                &InnerProfile {
                    profile,
                    unenforced: vec![],
                },
            )
            .unwrap();
            input.rewind().unwrap();
            sandbox_launch::run(inner_argv(input.into_raw_fd(), &marker, payload))
        }
        "remote" => {
            let mut args = vec![
                "--remote-strict".into(),
                "--profile-json".into(),
                serde_json::to_string(&profile).unwrap().into(),
                "--failure-fd".into(),
                "3".into(),
                "--".into(),
            ];
            args.extend(payload);
            sandbox_launch::run(args)
        }
        "pre-inner" | "real-mount-error" => {
            let mut command = if stage == "pre-inner" {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", "exit 27"]);
                command
            } else {
                let mut command = bwrap_command(Path::new(BWRAP));
                command
                    .arg("--ro-bind")
                    .arg(&marker)
                    .arg(&marker)
                    .arg("--")
                    .args(payload);
                command
            };
            let args = LaunchArgs {
                failure_fd: Some(4),
                failure_marker: Some(marker),
                ..Default::default()
            };
            let handoff = handoff::Handoff::new().unwrap();
            match handoff.supervise(&mut command) {
                Ok(status) => handoff::exit_with_status(status),
                Err(error) => report_failure(&args, Err(error)),
            }
        }
        "pty" => {
            let handoff = handoff::Handoff::new().unwrap();
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "printf C >&4; exec 4>&-; exec \"$@\"", "sh"])
                .args(payload);
            handoff::exit_with_status(handoff.supervise(&mut command).unwrap());
        }
        _ => panic!("unknown subprocess stage {stage}"),
    };
    match result {
        Ok(()) => panic!("test subprocess returned without executing or refusing payload"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(error.exit_code());
        }
    }
}

fn inner_argv(fd: RawFd, marker: &Path, payload: Vec<OsString>) -> Vec<OsString> {
    let mut args = vec![
        "--bwrap-inner".into(),
        "--profile-fd".into(),
        fd.to_string().into(),
        "--failure-marker".into(),
        marker.as_os_str().into(),
        "--failure-fd".into(),
        "4".into(),
        "--".into(),
    ];
    args.extend(payload);
    args
}

fn child(stage: &str, profile: &SandboxProfile, marker: &Path, payload: &[&str]) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            concat!(module_path!(), "::bwrap_failures_child"),
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD, stage)
        .env("PROFILE", serde_json::to_string(profile).unwrap())
        .env("PAYLOAD", serde_json::to_string(payload).unwrap())
        .env("MARKER", marker);
    command
}

fn install_marker(command: &mut Command, marker: &Path, fd: RawFd) {
    let file = File::create(marker).unwrap();
    let source = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
    assert!(source >= 5);
    let source = unsafe { File::from_raw_fd(source) };
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source.as_raw_fd(), fd) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn probe_fixture(path: &Path, stderr: &str, code: i32) {
    fs::write(
        path,
        format!("#!/bin/sh\nprintf '%s\\n' '{stderr}' >&2\nexit {code}\n"),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn expect_refusal(output: Output, marker: &Path, fd_contents: &Path, detail: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(SANDBOX_UNAVAILABLE_EXIT_CODE),
        "{stderr}"
    );
    assert!(
        stderr.contains(UNAVAILABLE) && stderr.contains(detail),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(marker).unwrap(), UNAVAILABLE);
    assert_eq!(fs::read_to_string(fd_contents).unwrap(), UNAVAILABLE);
}

const CHECK_FD_AND_EXIT: &str = r#"
import errno, fcntl, sys
from pathlib import Path
fd, sentinel, code = sys.argv[1:]
try:
    fcntl.fcntl(int(fd), fcntl.F_GETFD)
except OSError as e:
    assert e.errno == errno.EBADF, e
else:
    raise AssertionError('setup descriptor reached the payload')
with Path(sentinel).open('a') as f:
    f.write('started\n')
sys.exit(int(code))
"#;

fn fd_payload<'a>(fd: &'a str, sentinel: &'a Path, code: &'a str) -> [&'a str; 8] {
    [
        "/usr/bin/python3",
        "-I",
        "-B",
        "-c",
        CHECK_FD_AND_EXIT,
        fd,
        sentinel.to_str().unwrap(),
        code,
    ]
}

#[test]
fn bwrap_failures_only_trusted_path_absence_and_explicit_userns_refusal_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let missing = root.join("missing-bwrap");
    let denied = root.join("denied-bwrap");
    probe_fixture(&denied, NO_USERNS, 1);
    let profile = fixture(root, SandboxMode::Credentials);
    for (path, reason) in [
        (&missing, Fallback::Missing),
        (&denied, Fallback::UserNamespaceUnavailable),
    ] {
        assert_eq!(probe_at(path).unwrap(), Some(reason));
    }
    for (path, diagnostic, status) in [
        (
            root.join("fake-mount-error"),
            "bwrap: failed to bind mount: Permission denied",
            1,
        ),
        (root.join("different-status"), NO_USERNS, 2),
    ] {
        probe_fixture(&path, diagnostic, status);
        let mut profile = profile.clone();
        profile.linux_mounts = Some(LinuxMountPolicy::default());
        let marker = path.with_extension("marker");
        let fd_contents = path.with_extension("fd");
        let sentinel = path.with_extension("started");
        let mut cmd = child(
            "fallback",
            &profile,
            &marker,
            &fd_payload("4", &sentinel, "78"),
        );
        install_marker(&mut cmd, &fd_contents, 4);
        let output = cmd.env(STATUS_PROBE, path).output().unwrap();
        assert!(!String::from_utf8_lossy(&output.stderr).contains("using direct Landlock"));
        expect_refusal(
            output,
            &marker,
            &fd_contents,
            "bubblewrap namespace probe failed",
        );
        assert!(!sentinel.exists());
    }
    let invalid_image = root.join("invalid-image");
    fs::write(&invalid_image, "not an executable").unwrap();
    assert!(probe_at(&invalid_image)
        .unwrap_err()
        .contains("cannot execute"));
}

#[test]
fn bwrap_failures_relaxed_fallback_runs_once_with_broad_access_and_full_report() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let denied = root.join("denied-bwrap");
    probe_fixture(&denied, NO_USERNS, 1);
    let missing = root.join("missing-bwrap");
    let credential = root.join("home/cred");
    let read_mask = root.join("other/read");
    let read_only = root.join("other/write");
    let deferred = root.join("checkout");
    let cargo = root.join("home/cargo");
    let private = root.join("private/db");
    let socket = root.join("home/ssh.sock");
    for path in [&credential, &read_mask, &read_only, &private] {
        seed(path);
    }
    fs::create_dir(&cargo).unwrap();
    for mode in [SandboxMode::Credentials, SandboxMode::Standard] {
        for (probe, reason) in [
            (&missing, Fallback::Missing),
            (&denied, Fallback::UserNamespaceUnavailable),
        ] {
            let mut profile = fixture(root, mode);
            profile.worker_jail = true;
            let mut mounts = LinuxMountPolicy {
                credential_masks: vec![credential.clone()],
                cargo_homes: vec![cargo.clone()],
                ..Default::default()
            };
            let mut expected = vec![
                reason.code().into(),
                format!("mask:{}", credential.display()),
                format!("mask:{}*", cargo.join("credentials").display()),
                format!("operator gh fence: unenforced ({mode} profile)"),
                "session_dbus_keyring".into(),
            ];
            if mode == SandboxMode::Standard {
                mounts.read_masks.push(read_mask.clone());
                mounts.read_only.push(read_only.clone());
                mounts.deferred_read_only.push(deferred.clone());
                profile.data_policy.deny.push(private.clone());
                profile.socket_deny.push(socket.clone());
                profile.worker_gh_fence = Some(WorkerGhFence::Canary {
                    paths: vec![credential.clone()],
                });
                expected.extend([
                    format!("mask:{}", read_mask.display()),
                    format!("mask:{}", private.display()),
                    format!("read_only_bind:{}", read_only.display()),
                    format!("read_only_bind:{}", deferred.display()),
                    format!("socket_deny:{}", socket.display()),
                ]);
            } else {
                profile.worker_gh_fence = Some(WorkerGhFence::Unenforced);
            }
            profile.linux_mounts = Some(mounts);
            let sentinel = root.join(format!("started-{mode}-{}", reason.code()));
            let marker = sentinel.with_extension("marker");
            let fd_contents = sentinel.with_extension("fd");
            let script = format!(
                "{}\n{}",
                r#"
from pathlib import Path
import sys
credential, broad = sys.argv[-2:]
assert Path(credential).read_text() == 's5-disposable-secret'
Path(broad).write_text('broad write allowed')
del sys.argv[-2:]
"#,
                CHECK_FD_AND_EXIT
            );
            let payload = [
                "/usr/bin/python3",
                "-I",
                "-B",
                "-c",
                &script,
                "4",
                sentinel.to_str().unwrap(),
                "78",
                credential.to_str().unwrap(),
                read_only.to_str().unwrap(),
            ];
            let mut cmd = child("fallback", &profile, &marker, &payload);
            install_marker(&mut cmd, &fd_contents, 4);
            let output = cmd.env(STATUS_PROBE, probe).output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(78), "{stderr}");
            assert_eq!(fs::read_to_string(&sentinel).unwrap(), "started\n");
            assert_eq!(
                fs::read_to_string(&read_only).unwrap(),
                "broad write allowed"
            );
            assert!(!marker.exists(), "{stderr}");
            assert_eq!(fs::read(&fd_contents).unwrap(), b"");
            assert!(
                stderr.contains(&format!("sandbox-launch: profile={mode} unenforced=[")),
                "{stderr}"
            );
            assert!(!stderr.contains(UNAVAILABLE), "{stderr}");
            for block in expected {
                assert!(stderr.contains(&block), "missing {block} in {stderr}");
            }
        }
    }
}

#[test]
fn bwrap_failures_strict_fallback_enforces_allowlist_and_refuses_mandatory_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let denied = root.join("denied-bwrap");
    probe_fixture(&denied, NO_USERNS, 1);
    let missing = root.join("missing-bwrap");
    let outside = root.join("outside");
    seed(&outside);
    let checkout = root.join("checkout");
    let nested = checkout.join("mandatory");
    seed(&nested);
    let mut profile = fixture(root, SandboxMode::Strict);
    profile.read_allow = ["/usr", "/lib", "/lib64"]
        .map(PathBuf::from)
        .into_iter()
        .filter(|p| p.exists())
        .chain([checkout])
        .collect();
    for (probe, reason) in [
        (&missing, Fallback::Missing),
        (&denied, Fallback::UserNamespaceUnavailable),
    ] {
        profile.linux_mounts = Some(LinuxMountPolicy {
            credential_masks: vec![outside.clone()],
            fallback_write_deny: vec![outside.clone()],
            ..Default::default()
        });
        let sentinel = profile.temp_dir.join(reason.code());
        let marker = root.join(format!("{reason:?}.marker"));
        let fd_contents = root.join(format!("{reason:?}.fd"));
        let script = format!(
            "{}\n{}",
            r#"
from pathlib import Path
import sys
outside = Path(sys.argv.pop())
for try_access in [lambda: outside.read_bytes(), lambda: outside.write_bytes(b'new')]:
    try:
        try_access()
    except PermissionError:
        pass
    else:
        raise AssertionError('Strict fallback granted access outside its allowlist')
"#,
            CHECK_FD_AND_EXIT
        );
        let payload = [
            "/usr/bin/python3",
            "-I",
            "-B",
            "-c",
            &script,
            "4",
            sentinel.to_str().unwrap(),
            "0",
            outside.to_str().unwrap(),
        ];
        let run = |profile: &SandboxProfile| {
            let mut cmd = child("fallback", profile, &marker, &payload);
            install_marker(&mut cmd, &fd_contents, 4);
            cmd.env(STATUS_PROBE, probe).output().unwrap()
        };
        let stderr = passed(run(&profile));
        assert!(
            stderr.contains(reason.code())
                && stderr.contains(&format!("mask:{}", outside.display())),
            "{stderr}"
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "started\n");
        assert!(!marker.exists());
        assert_eq!(fs::read(&fd_contents).unwrap(), b"");
        fs::remove_file(&sentinel).unwrap();
        profile
            .linux_mounts
            .as_mut()
            .unwrap()
            .fallback_write_deny
            .push(nested.clone());
        expect_refusal(
            run(&profile),
            &marker,
            &fd_contents,
            "overlaps mandatory deny",
        );
        assert!(!sentinel.exists());
    }
    assert_eq!(fs::read_to_string(outside).unwrap(), SEED);
}

#[test]
fn bwrap_failures_missing_ack_is_refusal_even_when_launcher_exits_with_a_different_status() {
    let dir = tempfile::tempdir().unwrap();
    let profile = fixture(dir.path(), SandboxMode::Strict);
    let marker = dir.path().join("marker");
    let fd_contents = dir.path().join("fd");
    let mut cmd = child("pre-inner", &profile, &marker, &["/bin/false"]);
    install_marker(&mut cmd, &fd_contents, 4);
    expect_refusal(
        cmd.output().unwrap(),
        &marker,
        &fd_contents,
        "bubblewrap exited before sandbox setup completed (exit status: 27)",
    );
}

#[test]
fn bwrap_failures_inner_failure_and_payload_exit_78_have_distinct_markers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut profile = fixture(root, SandboxMode::Strict);
    let denied = root.join("checkout/mandatory");
    seed(&denied);
    profile.write_deny.push(denied);
    let marker = root.join("marker");
    let fd_contents = root.join("fd");
    let sentinel = root.join("started");
    let payload = fd_payload("4", &sentinel, "78");
    let run = |profile: &SandboxProfile| {
        let mut cmd = child("inner", profile, &marker, &payload);
        install_marker(&mut cmd, &fd_contents, 4);
        cmd.output().unwrap()
    };
    let output = run(&profile);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("overlaps mandatory deny"), "{stderr}");
    assert_eq!(fs::read_to_string(&marker).unwrap(), UNAVAILABLE);
    assert_eq!(fs::read(&fd_contents).unwrap(), handoff::FAILED);
    assert!(!sentinel.exists());
    fs::remove_file(&marker).unwrap();
    profile.mode = SandboxMode::Credentials;
    let output = run(&profile);
    assert_eq!(output.status.code(), Some(78), "{output:?}");
    assert!(!marker.exists());
    assert_eq!(fs::read(&fd_contents).unwrap(), handoff::COMPLETE);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "started\n");
}

#[test]
fn bwrap_failures_remote_strict_closes_the_caller_fd_in_the_payload() {
    let dir = tempfile::tempdir().unwrap();
    let mut profile = fixture(dir.path(), SandboxMode::Strict);
    profile.read_allow = ["/usr", "/lib", "/lib64"]
        .map(PathBuf::from)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    let profile = profile.canonicalize_for_launch().unwrap();
    let marker = dir.path().join("marker");
    let fd_contents = dir.path().join("fd");
    let sentinel = profile.temp_dir.join("started");
    let mut cmd = child(
        "remote",
        &profile,
        &marker,
        &fd_payload("3", &sentinel, "78"),
    );
    install_marker(&mut cmd, &fd_contents, 3);
    let output = cmd.output().unwrap();
    assert_eq!(output.status.code(), Some(78), "{output:?}");
    assert_eq!(fs::read(&fd_contents).unwrap(), b"");
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "started\n");
}

#[test]
fn bwrap_failures_live_mount_error_is_reported_without_acknowledgement() {
    let Some(_launcher) = enforcing_launcher() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let profile = fixture(root, SandboxMode::Strict);
    let marker = root.join("absent-bind-source");
    let fd_contents = root.join("fd");
    let sentinel = root.join("started");
    let mut cmd = child(
        "real-mount-error",
        &profile,
        &marker,
        &fd_payload("4", &sentinel, "78"),
    );
    install_marker(&mut cmd, &fd_contents, 4);
    expect_refusal(
        cmd.output().unwrap(),
        &marker,
        &fd_contents,
        "bubblewrap exited before sandbox setup completed",
    );
    assert!(!sentinel.exists());
}

#[test]
fn bwrap_failures_live_inner_refusal_and_payload_status_reach_outer_callers() {
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = root.join("marker");
    let fd_contents = root.join("fd");
    let sentinel = root.join("started");
    let mut profile = fixture(root, SandboxMode::Strict);
    let denied = root.join("checkout/mandatory");
    seed(&denied);
    profile.write_deny.push(denied);
    profile.linux_mounts = Some(LinuxMountPolicy::default());
    let run = |profile: &SandboxProfile, code, caller_marker| {
        let mut cmd = launch(&launcher, profile);
        cmd.arg("--failure-marker").arg(&marker);
        if caller_marker {
            cmd.args(["--failure-fd", "4"]);
            install_marker(&mut cmd, &fd_contents, 4);
        }
        cmd.arg("--")
            .args(fd_payload("4", &sentinel, code))
            .output()
            .unwrap()
    };
    let output = run(&profile, "78", true);
    expect_refusal(
        output,
        &marker,
        &fd_contents,
        "bubblewrap inner sandbox setup failed",
    );
    assert!(!sentinel.exists());
    let output = run(&profile, "78", false);
    assert_eq!(output.status.code(), Some(78), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("sandbox_unavailable: bubblewrap inner sandbox setup failed"));
    assert!(!sentinel.exists());
    fs::remove_file(&marker).unwrap();
    profile.mode = SandboxMode::Credentials;
    for (code, caller_marker) in [("78", true), ("27", true), ("0", false)] {
        let output = run(&profile, code, caller_marker);
        assert_eq!(
            output.status.code(),
            Some(code.parse().unwrap()),
            "{output:?}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(UNAVAILABLE),
            "{output:?}"
        );
        assert!(!marker.exists());
        assert_eq!(fs::read(&fd_contents).unwrap(), b"");
    }
    assert_eq!(
        fs::read_to_string(sentinel).unwrap(),
        "started\nstarted\nstarted\n"
    );
}

// pty.fork creates an isolated session and controlling terminal. Ctrl-C is
// generated by that terminal, not by broadcasting a signal from the test.
const PTY_ORACLE: &str = r#"
import errno, json, os, pty, select, sys, time
mode = sys.argv[1]
argv = json.loads(sys.argv[2])
pid, master = pty.fork()
if pid == 0:
    os.execv(argv[0], argv)
seen = b''
def expect(token):
    global seen
    deadline = time.monotonic() + 10
    while token not in seen:
        ready, _, _ = select.select([master], [], [], max(0, deadline-time.monotonic()))
        assert ready, seen
        try:
            data = os.read(master, 4096)
        except OSError as error:
            assert error.errno != errno.EIO, seen
            raise
        assert data, seen
        seen += data
try:
    expect(b's6-payload-ready')
    if mode == 'job':
        os.write(master, b's6-tty-input\n')
        expect(b's6-foreground-job-done')
    else:
        os.write(master, b'\x03')
        expect(b's6-payload-received-int')
    deadline = time.monotonic() + 10
    while True:
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            if mode == 'job':
                assert status == 0, (status, seen)
            break
        assert time.monotonic() < deadline, seen
        time.sleep(0.02)
finally:
    os.close(master)
"#;

const FOREGROUND_JOB: &str = r#"
import os
assert os.getpgrp() == os.tcgetpgrp(0)
assert os.getpgrp() != os.getsid(0), 'bash did not create a foreground job group'
print('s6-payload-ready', flush=True)
assert input() == 's6-tty-input'
print('s6-foreground-job-done', flush=True)
"#;

const GROUP_MEMBER: &str = r#"
import os, signal, sys
def interrupt(*_):
    print('s6-payload-received-int', flush=True)
    sys.exit(35)
assert os.getpgrp() == os.tcgetpgrp(0) == os.getsid(0)
signal.signal(signal.SIGINT, interrupt)
print('s6-payload-ready', flush=True)
signal.pause()
"#;

fn check_pty(build_command: impl Fn(&[&str]) -> Command, root: &Path) {
    for (mode, payload) in [
        (
            "job",
            vec![
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-imc",
                "\"$@\"; exit $?",
                "bash",
                "/usr/bin/python3",
                "-I",
                "-c",
                FOREGROUND_JOB,
            ],
        ),
        ("group", vec!["/usr/bin/python3", "-I", "-c", GROUP_MEMBER]),
    ] {
        let command = build_command(&payload);
        let argv = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        let mut oracle = Command::new("/usr/bin/python3");
        oracle.args([
            "-I",
            "-c",
            PTY_ORACLE,
            mode,
            &serde_json::to_string(&argv).unwrap(),
        ]);
        oracle.env("TERM", "dumb").current_dir(root);
        for (name, value) in command.get_envs() {
            oracle.env(name, value.unwrap());
        }
        passed(oracle.output().unwrap());
    }
}

#[test]
fn bwrap_failures_supervisor_preserves_process_group_signals_and_pty_job_control() {
    let dir = tempfile::tempdir().unwrap();
    let profile = fixture(dir.path(), SandboxMode::Credentials);
    check_pty(
        |payload| child("pty", &profile, &dir.path().join("marker"), payload),
        dir.path(),
    );
}

#[test]
fn bwrap_failures_live_preserves_process_group_signals_and_pty_job_control() {
    let Some(launcher) = enforcing_launcher() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut profile = fixture(dir.path(), SandboxMode::Credentials);
    profile.linux_mounts = Some(LinuxMountPolicy::default());
    check_pty(
        |payload| {
            let mut command = launch(&launcher, &profile);
            command.arg("--").args(payload);
            command
        },
        dir.path(),
    );
}
