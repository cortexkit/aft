#![cfg(target_os = "linux")]

use aft::{
    context::AppContext, parser::TreeSitterProvider, sandbox_spawn::run_remote_shell_for_test,
};
use std::{
    fs,
    net::TcpListener,
    os::unix::{fs::symlink, net::UnixListener},
    path::Path,
    process::Command,
    time::Duration,
};

const TOKEN: &str = "synthetic-token-only-for-remote-shell-test";
const CHILD: &str = "AFT_REMOTE_SHELL_TEST_CHILD";
const TEST: &str = "remote_shell_test::remote_shell_native_confinement_and_process_cleanup";

fn script(path: &Path, source: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn remote_shell_native_confinement_and_process_cleanup() {
    // Seed the environment of a separate test process rather than changing
    // process-global variables while the rest of the suite is running.
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .env("GH_TOKEN", TOKEN)
            .env("GITHUB_TOKEN", TOKEN)
            .env("GH_ENTERPRISE_TOKEN", TOKEN)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(native());
}

async fn native() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let other = base.join("other-project");
    let data = base.join("aft-data");
    for dir in [&project, &other, &data] {
        fs::create_dir(dir).unwrap();
    }
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        aft::config::Config {
            storage_dir: Some(data.clone()),
            ..Default::default()
        },
    );
    ctx.update_config(|cfg| {
        cfg.project_root = Some(other.clone());
        cfg.github.shim = true;
    });
    let executable = Path::new(env!("CARGO_BIN_EXE_aft"));
    let shims = ctx.storage_dir().join("shims");
    fs::create_dir_all(&shims).unwrap();
    symlink(executable, shims.join("gh")).unwrap();
    fs::write(project.join("inside"), "project contents").unwrap();
    fs::write(other.join("credential"), TOKEN).unwrap();
    fs::write(ctx.storage_dir().join("private"), TOKEN).unwrap();
    let upstream = project.join("upstream-gh");
    script(&upstream, &format!("#!/bin/bash\n[[ -n $GH_TOKEN ]] || [[ -n $GITHUB_TOKEN ]] || [[ -n $GH_ENTERPRISE_TOKEN ]] || IFS= read -r -n 1 seeded < '{}'\n", other.join("credential").display()));
    // A standalone command sees all three synthetic environment credentials
    // and can read the host's credential file. No actual gh account is used.
    let local = Command::new(&upstream).status().unwrap();
    assert!(local.success());
    let local = Command::new(&upstream).env_clear().status().unwrap();
    assert!(local.success(), "credential-file positive control");
    for name in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"] {
        assert_eq!(std::env::var(name).unwrap(), TOKEN);
    }
    let socket_path = project.join("credential.sock");
    let _unix = UnixListener::bind(&socket_path).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    let service_probe = project.join("service.py");
    fs::write(
        &service_probe,
        format!(
            r#"import socket, sys
targets = [(socket.AF_UNIX, {:?}), (socket.AF_INET, ('127.0.0.1', {}))]
for family, address in targets:
    try:
        with socket.socket(family) as client:
            client.connect(address)
    except PermissionError:
        if sys.argv[1] != 'denied': raise
    else:
        if sys.argv[1] != 'local': raise RuntimeError('remote shell reached a host service')
print('service control: ' + sys.argv[1])
"#,
            socket_path.to_string_lossy(),
            port
        ),
    )
    .unwrap();
    let local = Command::new("/usr/bin/python3")
        .arg(&service_probe)
        .arg("local")
        .output()
        .unwrap();
    assert!(local.status.success(), "{local:?}");
    let command = format!(
        r#"set -eu
test "$(cat inside)" = 'project contents'
printf writable > project-write
printf scratch > "$TMPDIR/scratch-write"
if cat '{}' || cat '{}' || printf nope > '{}/unregistered'; then exit 11; fi
test -z "${{GH_TOKEN-}}${{GITHUB_TOKEN-}}${{GH_ENTERPRISE_TOKEN-}}"
if '{}' || env -i PATH= '{}' ; then exit 12; fi
for variant in direct env-clear unset; do
  case "$variant" in
    direct) reply=$(gh issue close 1 2>&1) && exit 13;;
    env-clear) reply=$(env -i PATH="$PATH" GH_SHIM_BYPASS=operator gh issue close 1 2>&1) && exit 14;;
    unset) reply=$(unset HOME GH_SHIM_WORKER AFT_GH_SHIM_WORKER; GH_SHIM_BYPASS=operator gh issue close 1 2>&1) && exit 15;;
  esac
  test "${{reply#*GitHub commands are not available in remote shells.}}" != "$reply"
done
/usr/bin/python3 service.py denied
printf 'native confinement checked\n'
"#,
        other.join("credential").display(),
        ctx.storage_dir().join("private").display(),
        base.display(),
        upstream.display(),
        upstream.display()
    );
    let result =
        run_remote_shell_for_test(&ctx, &project, executable, command, Duration::from_secs(15))
            .await
            .unwrap();
    assert!(result.response.success, "{result:?}");
    assert!(
        result.text.contains("native confinement checked")
            && result.text.contains("service control: denied"),
        "{result:?}"
    );
    assert!(!base.join("unregistered").exists());

    fs::write(
        project.join("daemon.py"),
        r#"import os, time
if os.fork() != 0: os._exit(0)
if os.fork() != 0: os._exit(0)
for escape in (os.setsid, lambda: os.setpgid(0, 0)):
    try:
        escape()
    except PermissionError:
        pass
    else:
        raise RuntimeError('process escaped its session')
with open('daemon-pid', 'w') as f: f.write(str(os.getpid()))
while True: time.sleep(10)
"#,
    )
    .unwrap();
    let result = run_remote_shell_for_test(&ctx, &project, executable, "python3 daemon.py; while [[ ! -s daemon-pid ]]; do sleep .02; done; echo before-timeout; sleep 20".into(), Duration::from_secs(2)).await.unwrap();
    assert_eq!(result.response.data["timedOut"], true, "{result:?}");
    assert!(result.text.contains("before-timeout"), "{result:?}");
    let pid = fs::read_to_string(project.join("daemon-pid")).unwrap();
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        assert_eq!(
            stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
            Some("Z"),
            "{stat}"
        );
    }
}
