//! Native acceptance tests are opt-in: the credential matrix needs an enforcing
//! runner with Git and OpenSSH, and the HTTPS matrix needs outbound networking.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod credentials;
mod launch;
#[cfg(target_os = "macos")]
mod macos;
mod socket;

use aft_base::config::SandboxMode;
use std::path::Path;
use std::process::{Command, Output};

const HOME_PROCESS: &str = "AFT_SANDBOX_ACCEPTANCE_HOME_PROCESS";
const REQUIRE_OSXKEYCHAIN: &str = "AFT_REQUIRE_ACCEPTANCE_OSXKEYCHAIN";
const LEVELS: [SandboxMode; 4] = [
    SandboxMode::Off,
    SandboxMode::Credentials,
    SandboxMode::Standard,
    SandboxMode::Strict,
];

#[test]
#[ignore = "run in the native sandbox acceptance CI lanes"]
fn role_matrix_credentials_head() {
    isolated("role_matrix_credentials_head", |root| {
        credentials(root, false)
    });
}

#[test]
#[ignore = "run in the native sandbox acceptance CI lanes"]
fn role_matrix_credentials_worker() {
    isolated("role_matrix_credentials_worker", |root| {
        credentials(root, true)
    });
}

#[test]
#[ignore = "run in the native sandbox acceptance CI lanes with outbound HTTPS"]
fn role_matrix_https_head() {
    isolated("role_matrix_https_head", |root| https(root, false));
}

#[test]
#[ignore = "run in the native sandbox acceptance CI lanes with outbound HTTPS"]
fn role_matrix_https_worker() {
    isolated("role_matrix_https_worker", |root| https(root, true));
}

#[test]
#[ignore = "run once the local-secrets git proxy lands"]
fn production_per_call_git_proxy() {
    panic!("the production git proxy is not yet available");
}

fn isolated(test_name: &str, run: impl FnOnce(&Path)) {
    // Role resolution reads the daemon's environment. Set it only in a fresh
    // process; neither Cargo's HOME nor another test's credentials are fixtures.
    let Some(root) = std::env::var_os(HOME_PROCESS).map(std::path::PathBuf::from) else {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("home")).unwrap();
        std::fs::create_dir(base.join("project")).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--exact", "--ignored", "--nocapture"])
            .current_dir(&base)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env(HOME_PROCESS, &base)
            .env(
                REQUIRE_OSXKEYCHAIN,
                std::env::var_os(REQUIRE_OSXKEYCHAIN).unwrap_or_default(),
            )
            .env("HOME", base.join("home"))
            .env("XDG_CONFIG_HOME", base.join("home/.config"))
            .env("XDG_DATA_HOME", base.join("xdg/data"))
            .env("XDG_STATE_HOME", base.join("xdg/state"))
            .env("XDG_CACHE_HOME", base.join("xdg/cache"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("SSH_AUTH_SOCK", base.join("a"))
            .env("AWS_SHARED_CREDENTIALS_FILE", base.join("operator/aws"))
            .env(
                "KUBECONFIG",
                std::env::join_paths([base.join("operator/kube"), "kube-relative/config".into()])
                    .unwrap(),
            )
            .output()
            .expect("re-execute acceptance test with a disposable daemon HOME");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{stderr}"
        );
        print!("{stdout}");
        eprint!("{stderr}");
        return;
    };

    run(&root);
}

fn credentials(root: &Path, worker: bool) {
    let mut fixture = credentials::Fixture::new(root);
    let shell = launch::Shell::new(root, worker);
    #[cfg(target_os = "macos")]
    let keychain = macos::Keychain::add_lookups(root, &mut fixture);
    let script = fixture.script();
    #[cfg(target_os = "macos")]
    let script = script + &keychain.readable_file_check();

    for mode in LEVELS {
        let enabled = mode != SandboxMode::Off;
        let label = format!("{} {mode}", if worker { "worker" } else { "head" });
        let run = shell.run(mode, &script, enabled);
        fixture.assert_oracle(&label, &run, enabled);
        #[cfg(target_os = "macos")]
        assert!(
            run.lines().any(|line| line == "KEYCHAIN_BYTES_READABLE"),
            "{label}: keychain file was not readable: {run}"
        );
        if enabled {
            assert!(
                run.lines().any(|line| line == socket::SUCCESS),
                "{label}: git credential did not use the per-call socket: {run}"
            );
        }
        eprintln!("{label}: checked paired credential lookups and host file identity");
    }
}

fn https(root: &Path, worker: bool) {
    let shell = launch::Shell::new(root, worker);
    let script = format!("{} && printf 'HTTPS_OK\\n'", https_command());
    for mode in LEVELS {
        let run = shell.run(mode, &script, false);
        let label = format!("{} {mode}", if worker { "worker" } else { "head" });
        assert!(
            run.lines().any(|line| line == "HTTPS_OK"),
            "{label}: HTTPS failed: {run}"
        );
        eprintln!("{label}: HTTPS succeeded");
    }
}

fn https_command() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        // NSURLSession uses the system trust evaluator rather than a CA bundle.
        "/usr/bin/nscurl --max-time 20 --ignore-location -D - -o /dev/null https://example.com/ | /usr/bin/grep -E '^HTTP/[^ ]+ 200([[:space:]]|$)'"
    }
    #[cfg(target_os = "linux")]
    {
        "/usr/bin/curl -fsS --connect-timeout 10 --max-time 20 -o /dev/null https://example.com/"
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn path_arg(path: &Path) -> String {
    quote(path.to_str().expect("fixture path is UTF-8"))
}

fn checked(command: &mut Command) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    assert!(output.status.success(), "{command:?}: {output:?}");
    output
}
