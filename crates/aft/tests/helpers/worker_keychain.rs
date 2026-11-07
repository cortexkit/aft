//! Native keychain probes share the authenticated worker harness with worker_jail.

use super::*;

const SECRET: &str = "aft-disposable-keychain-secret-7391";
const SERVICE: &str = "aft-disposable-worker-probe";
const ACCOUNT: &str = "worker-probe";
const PASSWORD: &str = "disposable-keychain-password";

struct Keychain {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Drop for Keychain {
    fn drop(&mut self) {
        // Delete only the disposable keychain, including its search-list entry.
        let _ = Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(&self.path)
            .output();
    }
}

impl Keychain {
    fn create() -> Self {
        let scratch = crate::manifest_dir::manifest_dir().join("../../target/keychain-fixtures");
        std::fs::create_dir_all(&scratch).unwrap();
        let dir = tempfile::tempdir_in(scratch).unwrap();
        let fixture = Self {
            path: dir.path().join("t.keychain-db"),
            _dir: dir,
        };
        run(Command::new("/usr/bin/security")
            .args(["create-keychain", "-p", PASSWORD])
            .arg(&fixture.path));
        run(Command::new("/usr/bin/security")
            .args(["unlock-keychain", "-p", PASSWORD])
            .arg(&fixture.path));
        // The disposable item allows any client so a denied query cannot be
        // explained by an ACL mismatch or by a keychain authorization prompt.
        run(Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-A",
                "-s",
                SERVICE,
                "-a",
                ACCOUNT,
                "-w",
                SECRET,
            ])
            .arg(&fixture.path));
        fixture.assert_outside_readable();
        fixture
    }

    fn query(&self) -> String {
        format!(
            "/usr/bin/security find-generic-password -s {SERVICE} -a {ACCOUNT} -w {}",
            quote(&self.path)
        )
    }

    fn assert_outside_readable(&self) {
        let secret = run(Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])
            .arg(&self.path));
        assert_eq!(secret.trim(), SECRET);
    }
}

fn default_keychain() -> PathBuf {
    let path = run(Command::new("/usr/bin/security").args(["default-keychain", "-d", "user"]));
    let path = PathBuf::from(path.trim().trim_matches('"'));
    assert!(path.is_absolute(), "default keychain must be absolute");
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
    path
}

#[tokio::test]
async fn worker_jail_throwaway_keychain_is_unreadable() {
    let fixture = Keychain::create();
    for profile in ["strict", "relaxed"] {
        let mut jail = Jail::start_with_profile(None, None, true, profile, false).await;
        // The disposable keychain is outside both protected Keychains directories.
        // This direct file read must succeed; the following API query must fail.
        let readable = jail
            .bash(&format!(
                "cat {} >/dev/null && echo BYTES_$((1+1))",
                quote(&fixture.path)
            ))
            .await;
        assert!(readable.contains("BYTES_2"), "{profile}: {readable}");
        let reply = jail
            .bash(&format!("{}; echo QUERY_STATUS=$?", fixture.query()))
            .await;
        assert!(
            reply.contains("QUERY_STATUS="),
            "shell did not execute: {reply}"
        );
        assert!(!reply.contains("QUERY_STATUS=0"), "{profile}: {reply}");
        assert!(
            !reply.contains(SECRET),
            "worker disclosed disposable secret"
        );
        fixture.assert_outside_readable();
    }
}

#[tokio::test]
async fn worker_jail_default_keychain_file_is_unreadable() {
    let path = default_keychain();
    // Confirm the file is nonempty without exposing its contents in test output.
    assert!(!std::fs::read(&path).unwrap().is_empty());
    for profile in ["strict", "relaxed"] {
        let mut jail = Jail::start_with_profile(None, None, true, profile, false).await;
        let reply = jail
            .bash(&format!(
                "dd if={} of=/dev/null bs=1 count=1 2>/dev/null; echo FILE_STATUS=$?",
                quote(&path)
            ))
            .await;
        assert!(
            reply.contains("FILE_STATUS="),
            "shell did not execute: {reply}"
        );
        assert!(!reply.contains("FILE_STATUS=0"), "{profile}: {reply}");
    }
}

#[tokio::test]
async fn head_shell_throwaway_keychain_remains_readable() {
    let fixture = Keychain::create();
    let mut jail = Jail::start().await;
    jail.bind_grant_head().await;
    let reply = jail
        .tool_reply_on_channel(2, "bash", json!({"command":fixture.query()}))
        .await;
    assert!(
        reply.to_string().contains(SECRET),
        "head cannot query disposable keychain: {reply}"
    );
}

#[tokio::test]
async fn worker_jail_keychain_build_and_tls_compatibility() {
    for profile in ["strict", "relaxed"] {
        let mut jail = Jail::start_with_profile(None, None, true, profile, false).await;
        std::fs::write(
            jail.worktree.join("src/main.rs"),
            "fn main() { println!(\"LINKED_BINARY_OK\"); }\n#[test] fn arithmetic() { assert_eq!(2 + 2, 4); }\n",
        ).unwrap();
        let build = jail.bash(&format!(
            "cd {} && command -v cargo && RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= RUSTUP_HOME={} cargo build --offline --target-dir target && echo BUILD_$((1+1))",
            quote(&jail.worktree), quote(&real_rustup_home()),
        )).await;
        assert!(build.contains("BUILD_2"), "{profile}: {build}");
        let reply = jail.bash(&format!(
            "cd {} && command -v cargo && RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= RUSTUP_HOME={} cargo test --offline --target-dir target && ./target/debug/jailcrate && echo TEST_$((1+1))",
            quote(&jail.worktree), quote(&real_rustup_home()),
        )).await;
        assert!(reply.contains("1 passed"), "{profile}: {reply}");
        assert!(reply.contains("LINKED_BINARY_OK"), "{profile}: {reply}");
        assert!(reply.contains("TEST_2"), "{profile}: {reply}");
        // The network half needs live GitHub and npm, so it is opt-in: an
        // offline machine must not fail a sandbox test that has nothing to do
        // with the network. The maintainer's native gate sets this variable.
        if std::env::var_os("AFT_TEST_LIVE_NETWORK").is_none() {
            continue;
        }
        // Fetch into an independent repository: the linked main repository's
        // shallow metadata is intentionally read-only to workers. Every request
        // is bounded so a stalled network fails the test instead of hanging it.
        let reply = jail.bash(
                "fetch_repo=$(mktemp -d \"$TMPDIR/https-fetch.XXXXXX\") && git init -q \"$fetch_repo\" && git -C \"$fetch_repo\" -c credential.helper= -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=30 fetch --depth=1 https://github.com/octocat/Hello-World.git HEAD && echo FETCH_$((1+1))",
        ).await;
        assert!(reply.contains("FETCH_2"), "{profile}: {reply}");
        let reply = jail
            .bash("curl -sSf --connect-timeout 15 --max-time 60 https://github.com >/dev/null && echo CURL_$((1+1))")
            .await;
        assert!(reply.contains("CURL_2"), "{profile}: {reply}");
        let reply = jail.bash("bun --version && mkdir -p bun-probe && cd bun-probe && printf '%s' '{\"dependencies\":{\"is-number\":\"7.0.0\"}}' > package.json && perl -e 'alarm shift; exec @ARGV' 120 bun install && bun -e 'if (!require(\"is-number\")(42)) process.exit(1)' && echo BUN_$((1+1))").await;
        assert!(reply.contains("BUN_2"), "{profile}: {reply}");
    }
}

#[tokio::test]
#[ignore = "opt-in: creates and deletes one disposable item in the user's default keychain"]
async fn worker_jail_login_keychain_test_item_is_unreadable() {
    let path = default_keychain();
    let service = format!(
        "aft-worker-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    struct Item {
        service: String,
        path: PathBuf,
    }
    impl Drop for Item {
        fn drop(&mut self) {
            let _ = Command::new("/usr/bin/security")
                .args([
                    "delete-generic-password",
                    "-s",
                    &self.service,
                    "-a",
                    ACCOUNT,
                ])
                .arg(&self.path)
                .output();
        }
    }
    let item = Item { service, path };
    run(Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-A",
            "-s",
            &item.service,
            "-a",
            ACCOUNT,
            "-w",
            SECRET,
        ])
        .arg(&item.path));
    let query = format!(
        "/usr/bin/security find-generic-password -s {} -a {ACCOUNT} -w {}",
        quote(Path::new(&item.service)),
        quote(&item.path),
    );
    let outside = run(Command::new("/bin/sh").args(["-c", &query]));
    assert_eq!(outside.trim(), SECRET);
    for profile in ["strict", "relaxed"] {
        let mut jail = Jail::start_with_profile(None, None, true, profile, false).await;
        let reply = jail.bash(&format!("{query}; echo QUERY_STATUS=$?")).await;
        assert!(
            reply.contains("QUERY_STATUS="),
            "shell did not execute: {reply}"
        );
        assert!(!reply.contains("QUERY_STATUS=0"), "{profile}: {reply}");
        assert!(
            !reply.contains(SECRET),
            "worker disclosed disposable secret"
        );
    }
}
