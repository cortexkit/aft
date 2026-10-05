//! Prompt-free real macOS launch coverage. Only process attribution and ordinary
//! temporary files/terminal IO are observed; no TCC-protected API is exercised.

use super::helpers::{user_config, AftProcess};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SESSION: &str = "privacy-spawn";

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path, sandbox: bool) {
    let response = aft.send(&json!({
        "id": "cfg-privacy", "session_id": SESSION, "command": "configure",
        "harness": "opencode", "project_root": project, "storage_dir": storage,
        "config": user_config(json!({ "bash": { "disclaim_privacy": true }, "sandbox": { "enabled": sandbox } })),
    }).to_string());
    assert_eq!(response["success"], true, "{response}");
}

fn request(aft: &mut AftProcess, params: Value) -> Value {
    aft.send(
        &json!({"id":"privacy-command", "session_id":SESSION, "command":"bash", "params":params})
            .to_string(),
    )
}

fn terminal(aft: &mut AftProcess, response: Value) -> Value {
    let task = response["task_id"].as_str().expect("task id").to_owned();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = aft.send(&json!({"id":"privacy-status", "session_id":SESSION, "command":"bash_status", "params":{"task_id":task}}).to_string());
        assert_eq!(status["success"], true, "{status}");
        if matches!(
            status["status"].as_str(),
            Some("completed" | "failed" | "timed_out" | "killed")
        ) {
            return status;
        }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn output(status: &Value) -> String {
    if let Some(path) = status["output_path"].as_str() {
        std::fs::read_to_string(path).unwrap()
    } else {
        status["output_preview"]
            .as_str()
            .or_else(|| status["output"].as_str())
            .expect("command output")
            .to_owned()
    }
}

fn attribution_probe(project: &Path) {
    let mut cc = Command::new("/usr/bin/cc")
        .args(["-x", "c", "-o"])
        .arg(project.join("probe"))
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    cc.stdin.take().unwrap().write_all(br#"
#include <dlfcn.h>
#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
int main(int argc, char **argv) {
  /* getpgrp needs no process enumeration permission under Seatbelt. */
  if (argc == 2 && strcmp(argv[1], "group") == 0) { printf("%d\n", getpgrp()); return 0; }
  int (*responsible)(int) = dlsym(RTLD_DEFAULT, "responsibility_get_pid_responsible_for_pid");
  if (!responsible) return 41;
  int group = getpgrp();
  printf("group=%d responsible-group=%d responsible-child=%d session=%d tty=%d\n", group, responsible(group), responsible(getpid()), getsid(0), isatty(0));
  /* Later exec descendants may be attributed to themselves. The launch group's
     leader must be its own responsible process, never the AFT supervisor. */
  if (responsible(group) != group || getsid(0) != group) return 42;
  if (fcntl(3, F_GETFD) >= 0 || fcntl(4, F_GETFD) >= 0) return 43;
  if (isatty(0)) {
    /* Seatbelt need not permit reopening /dev/tty. Query the inherited fd. */
    if (tcgetpgrp(0) != group) return 44;
    struct winsize size;
    if (ioctl(0, TIOCGWINSZ, &size) != 0) return 45;
    printf("size=%dx%d\n", size.ws_row, size.ws_col);
  }
  puts("own-responsible-group");
  return 0;
}
"#).unwrap();
    assert!(cc.wait().unwrap().success());
    super::helpers::warm_executable(&project.join("probe"), &["group"]);
}

#[test]
fn disclaimed_foreground_background_and_pty_preserve_attribution_io_and_exit_with_seatbelt() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    attribution_probe(project.path());
    let mut aft = AftProcess::spawn();
    for sandbox in [false, true] {
        configure(&mut aft, project.path(), storage.path(), sandbox);
        for (background, pty) in [(false, false), (true, false), (true, true)] {
            let response = request(
                &mut aft,
                json!({
                    "command": "./probe || exit $?; printf 'env=%s\\n' \"$PRIVACY_TEST\"; pwd; printf stderr >&2; exit 23",
                "background": background, "pty":pty, "wait": !background,
                "pty_rows":29, "pty_cols":97,
                    "compressed":false, "env":{"PRIVACY_TEST":"kept"}, "timeout": 10000,
                }),
            );
            assert_eq!(
                response["success"], true,
                "sandbox={sandbox} pty={pty}: {response}"
            );
            let status = terminal(&mut aft, response);
            let text = output(&status);
            assert_eq!(
                status["exit_code"], 23,
                "sandbox={sandbox} pty={pty}: {status} output={text}"
            );
            assert!(text.contains(if pty { "tty=1" } else { "tty=0" }), "{text}");
            assert!(text.contains("own-responsible-group"), "{text}");
            assert!(text.contains("env=kept"), "{text}");
            assert!(
                text.contains(project.path().canonicalize().unwrap().to_str().unwrap()),
                "{text}"
            );
            if pty {
                assert!(text.contains("stderr"), "{text}");
                assert!(text.contains("size=29x97"), "{text}");
            }
        }
    }
}

#[test]
fn disclaimed_timeout_kills_foreground_background_and_pty_groups_with_seatbelt() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    attribution_probe(project.path());
    let mut aft = AftProcess::spawn();
    for sandbox in [false, true] {
        configure(&mut aft, project.path(), storage.path(), sandbox);
        for (background, pty) in [(false, false), (true, false), (true, true)] {
            let group_file = project.path().join("group");
            let _ = std::fs::remove_file(&group_file);
            let response = request(
                &mut aft,
                json!({
                    "command":"./probe group > group; printf started; trap '' TERM; sleep 30", "background":background, "pty":pty,
                    "compressed":false, "timeout":1000,
                }),
            );
            assert_eq!(response["success"], true, "{response}");
            let status = terminal(&mut aft, response);
            assert_eq!(status["status"], "timed_out", "{status}");
            assert_eq!(status["exit_code"], 124, "{status}");
            assert!(output(&status).contains("started"), "{status}");
            let group: i32 = std::fs::read_to_string(group_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(group > 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            while unsafe { libc::kill(-group, 0) } == 0 {
                assert!(
                    Instant::now() < deadline,
                    "timed-out process group {group} survived"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
}
