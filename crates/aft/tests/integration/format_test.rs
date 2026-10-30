//! Integration tests for the auto-format pipeline through the binary protocol.
//!
//! Verifies that mutation commands run the formatter when available and
//! gracefully degrade when the formatter is missing or the language is unsupported.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;

#[cfg(unix)]
use super::helpers::warm_executable;
use super::helpers::{link_cached_executable, user_config, AftProcess};

// ============================================================================
// Helpers
// ============================================================================

/// Check if a binary is available on PATH by attempting to run `--version`.
fn is_on_path(binary: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn install_tsc_stub(dir: &std::path::Path, file_name: &str) {
    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();

    #[cfg(unix)]
    {
        let stub = bin_dir.join("tsc");
        let source = format!(
            "#!/bin/sh\nprintf '%s(1,7): error TS2322: Type \\\"string\\\" is not assignable to type \\\"number\\\".\\n' '{}/{file_name}'\nexit 2\n",
            dir.display()
        );
        link_cached_executable(&stub, source.as_bytes()).unwrap();
        warm_executable(&stub, &["--version"]);
    }

    #[cfg(windows)]
    {
        let stub = bin_dir.join("tsc.cmd");
        let source = format!(
            r#"@echo off
echo {}\{file_name}(1,7): error TS2322: Type "string" is not assignable to type "number".
exit /b 2
"#,
            dir.display()
        )
        .replace('\n', "\r\n");
        link_cached_executable(&stub, source.as_bytes()).unwrap();
    }
}

/// Prepend `<dir>/node_modules/.bin` to a PATH-style env var so a stub
/// installed via `install_tsc_stub` resolves before any real `tsc` on the
/// runner. Cross-platform: `std::env::split_paths` and `join_paths` use
/// `:` on Unix and `;` on Windows automatically.
fn prepend_path(existing_path: &std::ffi::OsStr, dir: &std::path::Path) -> std::ffi::OsString {
    let mut paths = std::env::split_paths(existing_path).collect::<Vec<_>>();
    paths.insert(0, dir.join("node_modules").join(".bin"));
    std::env::join_paths(paths).unwrap()
}

/// Serialize rustfmt-edition tests when `cargo test` runs them in one process.
///
/// Nextest runs each test in its own process, so it cannot share this lock;
/// `warm_rustfmt` and `configure_real_rustfmt_test` below supply the timeout
/// isolation those runs need.
fn rustfmt_edition_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Formatter budget for the tests that assert real rustfmt formatted a file.
///
/// These tests check that formatting happened, not the product's 10-second
/// `formatter_timeout_secs` default, so they configure a wider budget for
/// their own AFT process only. It stays below the test harness's 60-second
/// response wait, so a formatter that is still too slow fails the assertion
/// with `format_skipped_reason: "timeout"` instead of a lost response.
const REAL_RUSTFMT_TEST_TIMEOUT_SECS: u32 = 45;

/// Longest time `warm_rustfmt` waits for its untimed first rustfmt run.
const RUSTFMT_WARM_LIMIT: Duration = Duration::from_secs(120);

/// Run the real rustfmt once, outside any product deadline, before a test
/// sends the edit whose formatting it asserts.
///
/// On a fresh Windows CI runner the first rustfmt start goes through the
/// rustup proxy and loads the toolchain from a cold disk. Windows CI runs
/// have seen that first start take longer than 30 seconds, so no formatter
/// budget that fits under the harness's 60-second response wait is a
/// reliable fix on its own. Later starts are fast. Paying the cold start
/// here, with no deadline, keeps the asserted edit measuring AFT's
/// formatting path instead of the runner's disk.
///
/// The warm-up runs in `project_dir` so rustup picks the same toolchain AFT
/// will. It formats a throwaway file, ignores the result, and gives up after
/// `RUSTFMT_WARM_LIMIT`; a formatter that is still slow then fails the test's
/// own assertion. The elapsed time is printed so a failing run shows whether
/// the start was cold.
fn warm_rustfmt(rustfmt: &Path, project_dir: &Path) {
    let warm_dir = project_dir.join("rustfmt-warm-up");
    fs::create_dir_all(&warm_dir).unwrap();
    let warm_file = warm_dir.join("warm_up.rs");
    fs::write(&warm_file, "fn  warm_up( ) {  }\n").unwrap();

    let started = Instant::now();
    let outcome = match Command::new(rustfmt)
        .args(["--edition", "2021"])
        .arg(&warm_file)
        .current_dir(project_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => loop {
            match child.try_wait() {
                Ok(Some(status)) => break format!("exited with {status}"),
                Ok(None) if started.elapsed() >= RUSTFMT_WARM_LIMIT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break "gave up and killed it".to_string();
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => break format!("could not be waited on: {error}"),
            }
        },
        Err(error) => format!("could not start: {error}"),
    };
    eprintln!(
        "rustfmt warm-up ({}) {outcome} after {} ms",
        rustfmt.display(),
        started.elapsed().as_millis()
    );
    let _ = fs::remove_dir_all(&warm_dir);
}

/// Configure a test that asserts real rustfmt output: format on edit, with
/// the test-only `REAL_RUSTFMT_TEST_TIMEOUT_SECS` budget. Call `warm_rustfmt`
/// first so the asserted edit does not pay the formatter's cold start.
fn configure_real_rustfmt_test(aft: &mut AftProcess, project_root: &Path) {
    let response = aft.send(
        &serde_json::json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project_root.display().to_string(),
            "config": user_config(json!({
                "format_on_edit": true,
                "formatter_timeout_secs": REAL_RUSTFMT_TEST_TIMEOUT_SECS,
            })),
        })
        .to_string(),
    );
    assert_eq!(
        response["success"], true,
        "configure should succeed: {response:?}"
    );
    // An out-of-range timeout does not fail configure: AFT discards just that
    // key with an `invalid_config_section` warning and keeps the 10-second
    // default, which would silently bring back the timeouts this budget
    // exists to avoid. Fail here instead.
    let discarded = response["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| warning["code"] == "invalid_config_section")
        .collect::<Vec<_>>();
    assert!(
        discarded.is_empty(),
        "the test formatter budget must be accepted, not discarded: {discarded:?}"
    );
}

fn install_rustfmt_edition_guard(dir: &std::path::Path, expected_edition: Option<&str>) {
    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();

    #[cfg(unix)]
    {
        let guard = match expected_edition {
            Some(edition) => format!(
                "if [ \"$1\" != \"--edition\" ] || [ \"$2\" != \"{edition}\" ]; then\n  printf '%s\\n' 'rustfmt expected --edition {edition}' >&2\n  exit 1\nfi\n"
            ),
            None => "if [ \"$1\" = \"--edition\" ]; then\n  printf '%s\\n' 'rustfmt expected no edition flag' >&2\n  exit 1\nfi\n".to_string(),
        };
        let stub = bin_dir.join("rustfmt");
        let source = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  exec \"$AFT_TEST_REAL_RUSTFMT\" \"$@\"\nfi\n{guard}exec \"$AFT_TEST_REAL_RUSTFMT\" \"$@\"\n"
        );
        link_cached_executable(&stub, source.as_bytes()).unwrap();
        warm_executable(&stub, &["--version"]);
    }

    #[cfg(windows)]
    {
        let source = match expected_edition {
            Some(edition) => format!(
                "@echo off\r\nif \"%~1\"==\"--version\" goto version\r\nif not \"%~1\"==\"--edition\" goto missing\r\nif not \"%~2\"==\"{edition}\" goto missing\r\n\"%AFT_TEST_REAL_RUSTFMT%\" %*\r\nexit /b %errorlevel%\r\n:version\r\n\"%AFT_TEST_REAL_RUSTFMT%\" --version\r\nexit /b %errorlevel%\r\n:missing\r\necho rustfmt expected --edition {edition} 1>&2\r\nexit /b 1\r\n"
            ),
            None => "@echo off\r\nif \"%~1\"==\"--version\" goto version\r\nif \"%~1\"==\"--edition\" goto unexpected_edition\r\n\"%AFT_TEST_REAL_RUSTFMT%\" %*\r\nexit /b %errorlevel%\r\n:version\r\n\"%AFT_TEST_REAL_RUSTFMT%\" --version\r\nexit /b %errorlevel%\r\n:unexpected_edition\r\necho rustfmt expected no edition flag 1>&2\r\nexit /b 1\r\n".to_string(),
        };
        link_cached_executable(&bin_dir.join("rustfmt.cmd"), source.as_bytes()).unwrap();
    }
}

/// Create a directory for one format test inside that test's private scratch
/// dir, which is removed when the test ends.
fn format_test_dir(test_name: &str) -> std::path::PathBuf {
    let dir = crate::helpers::thread_scratch_dir()
        .join("aft_format_tests")
        .join(test_name);
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ============================================================================
// format_integration tests
// ============================================================================

#[test]

fn format_integration_applied_rustfmt() {
    let Some(real_rustfmt) = which::which("rustfmt").ok() else {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    };

    let dir = format_test_dir("applied_rustfmt");
    // Cargo.toml needed so config-file detection triggers for Rust
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
    let target = dir.join("format_applied.rs");
    let _ = fs::remove_file(&target);

    let ugly_code = "fn  main( ){  let   x=1;  }";

    warm_rustfmt(&real_rustfmt, &dir);
    let path = prepend_path(&std::env::var_os("PATH").unwrap_or_default(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    configure_real_rustfmt_test(&mut aft, &dir);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-1","command":"write","file":{},"content":"{}"}}"#,
        crate::helpers::json_string(&target.display()),
        ugly_code
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["formatted"], true,
        "rustfmt should have formatted the file"
    );
    assert!(
        resp.get("format_skipped_reason").is_none() || resp["format_skipped_reason"].is_null(),
        "no skip reason when formatted"
    );

    // Verify on-disk content is actually formatted
    let on_disk = fs::read_to_string(&target).unwrap();
    assert!(
        !on_disk.contains("fn  main"),
        "file should be reformatted, got: {}",
        on_disk
    );
    assert!(
        on_disk.contains("fn main()"),
        "file should contain properly formatted fn main(), got: {}",
        on_disk
    );

    let reflow_text = resp["reformatted"]["text"]
        .as_str()
        .expect("rustfmt reflow should surface reformatted.text");
    assert!(
        reflow_text.contains("fn main()"),
        "excerpt should show post-format text, got: {reflow_text}"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn format_integration_rustfmt_uses_package_edition() {
    // Each test starts an AFT process and invokes the real rustfmt through its
    // guard; serializing them avoids starving their response-timeout windows.
    let _lock = rustfmt_edition_test_lock();
    let Some(real_rustfmt) = which::which("rustfmt").ok() else {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    };

    let dir = format_test_dir("rustfmt_package_edition");
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    install_rustfmt_edition_guard(&dir, Some("2021"));
    warm_rustfmt(&real_rustfmt, &dir);
    let target = dir.join("src").join("format_edition.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();

    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_TEST_REAL_RUSTFMT", real_rustfmt.as_os_str()),
    ]);
    configure_real_rustfmt_test(&mut aft, &dir);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-rust-edition","command":"write","file":{},"content":"async   fn   format_me( ) {{ }}\n"}}"#,
        crate::helpers::json_string(&target.display()),
    ));

    assert_eq!(resp["success"], true, "write should succeed: {resp:?}");
    assert_eq!(
        resp["formatted"], true,
        "edition-aware rustfmt should format async Rust: {resp:?}"
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("async fn format_me() {}"),
        "rustfmt should format the async function"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn format_integration_rustfmt_uses_workspace_package_edition() {
    let _lock = rustfmt_edition_test_lock();
    let Some(real_rustfmt) = which::which("rustfmt").ok() else {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    };

    let workspace = format_test_dir("rustfmt_workspace_edition");
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.package]\nedition = \"2021\"\n",
    )
    .unwrap();
    let member = workspace.join("member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition.workspace = true\n",
    )
    .unwrap();
    install_rustfmt_edition_guard(&member, Some("2021"));
    warm_rustfmt(&real_rustfmt, &member);
    let target = member.join("src").join("lib.rs");

    let path = prepend_path(&std::ffi::OsString::new(), &member);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_TEST_REAL_RUSTFMT", real_rustfmt.as_os_str()),
    ]);
    configure_real_rustfmt_test(&mut aft, &member);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-workspace-edition","command":"write","file":{},"content":"async   fn   format_member( ) {{ }}\n"}}"#,
        crate::helpers::json_string(&target.display()),
    ));

    assert_eq!(resp["success"], true, "write should succeed: {resp:?}");
    assert_eq!(
        resp["formatted"], true,
        "workspace edition should be passed to rustfmt: {resp:?}"
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("async fn format_member() {}"),
        "rustfmt should format the async workspace member"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn format_integration_rustfmt_uses_member_edition_from_virtual_workspace() {
    let _lock = rustfmt_edition_test_lock();
    let Some(real_rustfmt) = which::which("rustfmt").ok() else {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    };

    let workspace = format_test_dir("rustfmt_virtual_workspace_edition");
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\n",
    )
    .unwrap();
    let member = workspace.join("member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    install_rustfmt_edition_guard(&workspace, Some("2021"));
    warm_rustfmt(&real_rustfmt, &workspace);
    let target = member.join("src").join("lib.rs");

    let path = prepend_path(&std::ffi::OsString::new(), &workspace);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_TEST_REAL_RUSTFMT", real_rustfmt.as_os_str()),
    ]);
    configure_real_rustfmt_test(&mut aft, &workspace);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-virtual-workspace-edition","command":"write","file":{},"content":"async   fn   format_member( ) {{ }}\n"}}"#,
        crate::helpers::json_string(&target.display()),
    ));

    assert_eq!(resp["success"], true, "write should succeed: {resp:?}");
    assert_eq!(
        resp["formatted"], true,
        "member edition should be passed through a virtual workspace: {resp:?}"
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("async fn format_member() {}"),
        "rustfmt should format the async virtual-workspace member"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn format_integration_rustfmt_prefers_member_edition_over_workspace_default() {
    let _lock = rustfmt_edition_test_lock();
    let Some(real_rustfmt) = which::which("rustfmt").ok() else {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    };

    let workspace = format_test_dir("rustfmt_member_edition_precedence");
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"member\"]\n\n[workspace.package]\nedition = \"2018\"\n",
    )
    .unwrap();
    let member = workspace.join("member");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    install_rustfmt_edition_guard(&workspace, Some("2021"));
    warm_rustfmt(&real_rustfmt, &workspace);
    let target = member.join("src").join("lib.rs");

    let path = prepend_path(&std::ffi::OsString::new(), &workspace);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_TEST_REAL_RUSTFMT", real_rustfmt.as_os_str()),
    ]);
    configure_real_rustfmt_test(&mut aft, &workspace);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-member-edition-precedence","command":"write","file":{},"content":"async   fn   format_member( ) {{ }}\n"}}"#,
        crate::helpers::json_string(&target.display()),
    ));

    assert_eq!(resp["success"], true, "write should succeed: {resp:?}");
    assert_eq!(
        resp["formatted"], true,
        "member edition should override workspace.package.edition: {resp:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn format_integration_rustfmt_preserves_bare_2015_invocation() {
    let _lock = rustfmt_edition_test_lock();
    let Some(real_rustfmt) = which::which("rustfmt").ok() else {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    };

    let dir = format_test_dir("rustfmt_no_edition");
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    install_rustfmt_edition_guard(&dir, None);
    warm_rustfmt(&real_rustfmt, &dir);
    let target = dir.join("src").join("format_2015.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();

    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_TEST_REAL_RUSTFMT", real_rustfmt.as_os_str()),
    ]);
    configure_real_rustfmt_test(&mut aft, &dir);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-no-edition","command":"write","file":{},"content":"fn  main( ) {{ }}\n"}}"#,
        crate::helpers::json_string(&target.display()),
    ));

    assert_eq!(resp["success"], true, "write should succeed: {resp:?}");
    assert_eq!(
        resp["formatted"], true,
        "a manifest without edition must run bare rustfmt: {resp:?}"
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("fn main() {}"),
        "rustfmt should format the Rust 2015-compatible function"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// edit_match with a mis-wrapped replacement → reformatted.text in response.
#[test]
fn format_integration_edit_match_reformatted_excerpt_on_reflow() {
    let Some(real_rustfmt) = which::which("rustfmt").ok() else {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    };

    let dir = format_test_dir("edit_match_reflow");
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
    let target = dir.join("format_edit_match_reflow.rs");
    fs::write(
        &target,
        "fn main() {\n    helper();\n}\n\nfn helper() {\n    println!(\"ok\");\n}\n",
    )
    .unwrap();

    warm_rustfmt(&real_rustfmt, &dir);
    let path = prepend_path(&std::env::var_os("PATH").unwrap_or_default(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    configure_real_rustfmt_test(&mut aft, &dir);

    let req = serde_json::json!({
        "id": "fmt-edit-match",
        "command": "edit_match",
        "file": target.display().to_string(),
        "match": "fn helper() {\n    println!(\"ok\");\n}",
        "replacement": "fn helper() {  println!(  \"ok\"  );  }",
    });
    let resp = aft.send(&serde_json::to_string(&req).unwrap());

    assert_eq!(
        resp["success"], true,
        "edit_match should succeed: {:?}",
        resp
    );
    assert_eq!(resp["formatted"], true);
    let reflow_text = resp["reformatted"]["text"]
        .as_str()
        .expect("replacement reflow should surface reformatted.text");
    assert!(
        reflow_text.contains("println!"),
        "excerpt should contain formatted helper body, got: {reflow_text}"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// Well-formatted write → reformatted field absent (self-suppress).
#[test]
fn format_integration_write_self_suppresses_reformatted_when_no_reflow() {
    if !is_on_path("rustfmt") {
        eprintln!("SKIP: rustfmt not on PATH");
        return;
    }

    let dir = format_test_dir("write_self_suppress");
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
    let target = dir.join("format_self_suppress.rs");
    let _ = fs::remove_file(&target);

    let neat = "fn main() {\n    let x = 1;\n}\n";

    let path = prepend_path(&std::env::var_os("PATH").unwrap_or_default(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    aft.configure(&dir);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-self","command":"write","file":{},"content":{}}}"#,
        crate::helpers::json_string(&target.display()),
        crate::helpers::json_string(&neat)
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert!(
        resp.get("reformatted").is_none() || resp["reformatted"].is_null(),
        "already-formatted content should not emit reformatted: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// Write a .txt file → formatter is unsupported for this language.
#[test]
fn format_integration_unsupported_language() {
    let dir = format_test_dir("unsupported_lang");
    let target = dir.join("format_unsupported.txt");
    let _ = fs::remove_file(&target);

    let path = prepend_path(&std::env::var_os("PATH").unwrap_or_default(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    aft.configure_format_on_edit(&dir);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-2","command":"write","file":{},"content":"hello world"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["formatted"], false,
        "txt files should not be formatted"
    );
    assert_eq!(
        resp["format_skipped_reason"], "unsupported_language",
        "skip reason should be unsupported_language"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// Write a .py file without a formatter config → no_formatter_configured.
#[test]
fn format_integration_no_formatter_configured() {
    let dir = format_test_dir("no_formatter_configured");
    let target = dir.join("format_no_formatter_configured.py");
    let _ = fs::remove_file(&target);

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-3","command":"write","file":{},"content":"x = 1"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["formatted"], false,
        "should not be formatted without formatter"
    );
    assert_eq!(
        resp["format_skipped_reason"], "no_formatter_configured",
        "skip reason should be no_formatter_configured"
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// A configured formatter whose binary is missing → formatter_not_installed.
#[test]
fn format_integration_formatter_not_installed() {
    let dir = format_test_dir("formatter_not_installed");
    fs::write(dir.join("biome.json"), "{}\n").unwrap();
    let target = dir.join("format_formatter_not_installed.ts");
    let _ = fs::remove_file(&target);

    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_DISABLE_WELL_KNOWN_LOOKUP", std::ffi::OsStr::new("1")),
    ]);
    let cfg = aft.configure_format_on_edit(&dir);
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-3b","command":"write","file":{},"content":"const x = 1;\n"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(resp["formatted"], false);
    assert_eq!(
        resp["format_skipped_reason"], "formatter_not_installed",
        "skip reason should be formatter_not_installed: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn format_integration_ruff_old_version_is_gated_at_format_time() {
    let dir = format_test_dir("ruff_old_version");
    fs::write(dir.join("ruff.toml"), "line-length = 88\n").unwrap();
    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let canary = dir.join("ruff-version-canary");
    let _ = fs::remove_file(&canary);
    let ruff = bin_dir.join("ruff");
    link_cached_executable(
        &ruff,
        br#"#!/bin/sh
if [ "$1" = "--version" ]; then printf executed > "$RUFF_CANARY"; printf 'ruff 0.1.1\n'; exit 0; fi
exit 0
"#,
    )
    .unwrap();
    // The `--version` branch writes the canary checked below, so warm through
    // the fixture's inert branch before AFT performs the real version probe.
    warm_executable(&ruff, &["--warmup"]);

    let target = dir.join("format_ruff_old.py");
    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("RUFF_CANARY", canary.as_os_str()),
    ]);
    let cfg = aft.configure_format_on_edit(&dir);
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let resp = aft.send(&format!(
        r#"{{"id":"fmt-ruff-old","command":"write","file":{},"content":"x = 1\\n"}}"#,
        crate::helpers::json_string(&target.display())
    ));
    assert_eq!(resp["success"], true, "write should succeed: {resp:?}");
    assert_eq!(resp["formatted"], false);
    assert_eq!(resp["format_skipped_reason"], "formatter_not_installed");
    assert!(
        canary.exists(),
        "the version probe should run on a format operation"
    );

    let (status, stderr) = aft.stderr_output();
    assert!(status.success());
    assert!(
        stderr.contains("ruff formatter version 0.1.1 is too old"),
        "version-gated format warning should name the detected version; stderr: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn format_integration_oxfmt_config_runs_oxfmt() {
    let dir = format_test_dir("oxfmt_config_runs");
    fs::write(dir.join(".oxfmtrc.json"), "{}\n").unwrap();
    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let stub = bin_dir.join("oxfmt");
    link_cached_executable(
        &stub,
        br#"#!/bin/sh
if [ "$1" = "--write" ]; then printf 'const x = 1;\n' > "$2"; fi
"#,
    )
    .unwrap();
    warm_executable(&stub, &["--version"]);

    let target = dir.join("format_oxfmt.ts");
    let _ = fs::remove_file(&target);
    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let cfg = aft.configure_format_on_edit(&dir);
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"fmt-3c","command":"write","file":{},"content":"const   x=1;\n"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["formatted"], true,
        "oxfmt should have formatted the file"
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "const x = 1;\n");

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn format_integration_oxfmt_ignored_path_reports_formatter_excluded_path() {
    let dir = format_test_dir("oxfmt_ignored_path");
    fs::write(dir.join(".oxfmtrc.json"), "{}\n").unwrap();
    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let stub = bin_dir.join("oxfmt");
    link_cached_executable(
        &stub,
        br#"#!/bin/sh
printf 'Expected at least one target file after applying ignore rules.\n' >&2
exit 1
"#,
    )
    .unwrap();
    warm_executable(&stub, &["--version"]);

    let target = dir.join("format_oxfmt_ignored.ts");
    let _ = fs::remove_file(&target);
    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let cfg = aft.configure_format_on_edit(&dir);
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"fmt-3d","command":"write","file":{},"content":"const   x=1;\n"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(resp["formatted"], false);
    assert_eq!(
        resp["format_skipped_reason"], "formatter_excluded_path",
        "oxfmt ignored paths should report formatter_excluded_path: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// add_import on a .rs file → verify response has formatted field.

#[test]
fn format_integration_add_import_with_format() {
    let dir = format_test_dir("add_import");
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
    let target = dir.join("format_add_import.rs");
    // Write a valid Rust file with a function
    fs::write(&target, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(&dir);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-4","command":"add_import","file":{},"module":"std::collections::HashMap"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(
        resp["success"], true,
        "add_import should succeed: {:?}",
        resp
    );
    assert_eq!(resp["added"], true);
    // The formatted field must always be present
    assert!(
        resp.get("formatted").is_some() && !resp["formatted"].is_null(),
        "formatted field must be present in add_import response: {:?}",
        resp
    );

    // Verify the import was actually added to the file
    let on_disk = fs::read_to_string(&target).unwrap();
    assert!(
        on_disk.contains("use std::collections::HashMap"),
        "import should be in file, got: {}",
        on_disk
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// edit_symbol on a .rs file → verify formatted field in response.

#[test]
fn format_integration_edit_symbol_with_format() {
    let dir = format_test_dir("edit_symbol");
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
    let target = dir.join("format_edit_symbol.rs");
    // Write a Rust file with a function to edit
    fs::write(&target, "fn greet() {\n    println!(\"hi\");\n}\n").unwrap();

    let mut aft = AftProcess::spawn();
    aft.configure(&dir);

    // Use edit_symbol to replace the function
    let new_body = r#"fn greet() {\n    println!(\"hello world\");\n}"#;
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-5","command":"edit_symbol","file":{},"symbol":"greet","operation":"replace","content":"{}"}}"#,
        crate::helpers::json_string(&target.display()),
        new_body
    ));

    assert_eq!(
        resp["success"], true,
        "edit_symbol should succeed: {:?}",
        resp
    );
    // The formatted field must always be present
    assert!(
        resp.get("formatted").is_some() && !resp["formatted"].is_null(),
        "formatted field must be present in edit_symbol response: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// Verify that the `formatted` field is always present in mutation responses,
/// even for unsupported languages.
#[test]
fn format_integration_fields_always_present() {
    let dir = format_test_dir("fields_present");
    // Cargo.toml needed for .rs test
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();

    // Test 1: write to a .md file (unsupported language for formatting)
    let md_target = dir.join("format_fields_check.md");
    let _ = fs::remove_file(&md_target);

    let mut aft = AftProcess::spawn();
    aft.configure_format_on_edit(&dir);
    let resp = aft.send(&format!(
        r#"{{"id":"fmt-6a","command":"write","file":{},"content":"Hello markdown"}}"#,
        crate::helpers::json_string(&md_target.display())
    ));

    assert_eq!(
        resp["success"], true,
        "write to .md should succeed: {:?}",
        resp
    );
    // `formatted` must be present (not missing from JSON)
    assert!(
        resp.get("formatted").is_some(),
        "formatted field must be present even for unsupported languages: {:?}",
        resp
    );
    assert_eq!(resp["formatted"], false);
    assert_eq!(resp["format_skipped_reason"], "unsupported_language");

    // Test 2: write to a .rs file — formatted field present with value true (if rustfmt available)
    let rs_target = dir.join("format_fields_check.rs");
    let _ = fs::remove_file(&rs_target);

    let resp2 = aft.send(&format!(
        r#"{{"id":"fmt-6b","command":"write","file":{},"content":"fn main() {{}}"}}"#,
        crate::helpers::json_string(&rs_target.display())
    ));

    assert_eq!(
        resp2["success"], true,
        "write to .rs should succeed: {:?}",
        resp2
    );
    assert!(
        resp2.get("formatted").is_some(),
        "formatted field must be present for .rs files: {:?}",
        resp2
    );

    let _ = fs::remove_file(&md_target);
    let _ = fs::remove_file(&rs_target);
    let status = aft.shutdown();
    assert!(status.success());
}

// ============================================================================
// validate_full integration tests
// ============================================================================

/// Send mutation without validate param → no validation_errors in response.
#[test]
fn validate_full_default_no_errors() {
    let dir = format_test_dir("validate_default");
    let target = dir.join("validate_default.rs");
    let _ = fs::remove_file(&target);

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"val-1","command":"write","file":{},"content":"fn main() {{}}"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    // Without validate:"full", validation_errors should not be present (or empty)
    let has_errors = resp.get("validation_errors").is_some()
        && !resp["validation_errors"].is_null()
        && resp["validation_errors"]
            .as_array()
            .is_some_and(|a| !a.is_empty());
    assert!(
        !has_errors,
        "validation_errors should be absent or empty without validate:full, got: {:?}",
        resp
    );
    // validate_skipped_reason should not be present
    assert!(
        resp.get("validate_skipped_reason").is_none() || resp["validate_skipped_reason"].is_null(),
        "validate_skipped_reason should not be present without validate:full: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn validate_on_edit_full_from_config_runs_checker() {
    if !cfg!(unix) {
        eprintln!("SKIP: tsc stub test requires unix executable permissions");
        return;
    }

    let dir = format_test_dir("validate_config_full");
    let target = dir.join("validate_config_full.ts");
    let _ = fs::remove_file(&target);
    fs::write(dir.join("tsconfig.json"), "{}\n").unwrap();
    install_tsc_stub(&dir, "validate_config_full.ts");

    let mut aft = AftProcess::spawn();
    let cfg = aft.send(
        &json!({
            "id": "cfg-val-full",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir,
            "config": user_config(serde_json::json!({
                "validate_on_edit": "full",
                "checker": { "typescript": "tsc" }
            }))
        })
        .to_string(),
    );
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"val-config-full","command":"write","file":{},"content":"const x: number = \"oops\";\n"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    let errors = resp["validation_errors"]
        .as_array()
        .expect("validate_on_edit:full should include validation_errors");
    assert!(
        !errors.is_empty(),
        "broken TypeScript types should produce validation_errors: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn validate_on_edit_off_from_config_skips_checker() {
    let dir = format_test_dir("validate_config_off");
    let target = dir.join("validate_config_off.ts");
    let _ = fs::remove_file(&target);
    fs::write(dir.join("tsconfig.json"), "{}\n").unwrap();
    #[cfg(unix)]
    install_tsc_stub(&dir, "validate_config_off.ts");

    let mut aft = AftProcess::spawn();
    let cfg = aft.send(
        &json!({
            "id": "cfg-val-off",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir,
        })
        .to_string(),
    );
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);

    let resp = aft.send(&format!(
        r#"{{"id":"val-config-off","command":"write","file":{},"content":"const x: number = \"oops\";\n"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    let has_errors = resp.get("validation_errors").is_some()
        && !resp["validation_errors"].is_null()
        && resp["validation_errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty());
    assert!(
        !has_errors,
        "validate_on_edit:off should not produce validation_errors: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// Creates the release file on drop so a failing assertion never leaves the
/// blocking checker stub (and the aft child waiting on it) running.
#[cfg(unix)]
struct ReleaseOnDrop(std::path::PathBuf);

#[cfg(unix)]
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "release");
    }
}

/// Read stdout lines until the response with `id` arrives or `budget` runs out.
/// Returns the response plus the ids of every other response seen first.
#[cfg(unix)]
fn read_response_within(
    aft: &mut AftProcess,
    id: &str,
    budget: std::time::Duration,
) -> (Option<serde_json::Value>, Vec<String>) {
    let deadline = std::time::Instant::now() + budget;
    let mut earlier_ids = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return (None, earlier_ids);
        }
        let Some(value) = aft.try_read_next_timeout(remaining) else {
            continue;
        };
        match value.get("id").and_then(serde_json::Value::as_str) {
            Some(seen) if seen == id => return (Some(value), earlier_ids),
            Some(seen) => earlier_ids.push(seen.to_string()),
            None => {}
        }
    }
}

/// Echo requests waiting behind a blocked type checker must leave status reachable.
/// A duplicate edit must be refused without losing the original edit's terminal reply.
#[cfg(unix)]
#[test]
fn validation_backlog_rejects_overflow_and_duplicate_without_hiding_status() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let dir = format_test_dir("bounded_validation_backlog");
    let target = dir.join("target.ts");
    let started = dir.join("checker-started");
    let release = dir.join("checker-release");
    for path in [&target, &started, &release] {
        let _ = fs::remove_file(path);
    }
    fs::write(dir.join("tsconfig.json"), "{}\n").unwrap();
    let bin_dir = dir.join("node_modules/.bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let stub = bin_dir.join("tsc");
    fs::write(&stub, format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'Version 5.0.0'; exit 0; fi\n: > '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0\n", started.display(), release.display())).unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    warm_executable(&stub, &["--version"]);
    let _release_guard = ReleaseOnDrop(release.clone());
    let mut aft = AftProcess::spawn();
    let configured = aft.send(&json!({"id":"cfg-bounded","command":"configure","harness":"opencode","project_root":dir,"config":user_config(json!({"validate_on_edit":"full","checker":{"typescript":"tsc"}}))}).to_string());
    assert_eq!(configured["success"], true);
    aft.send_silent(&json!({"id":"blocked-edit","command":"write","file":target,"content":"const original = 1;\n"}).to_string());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !started.exists() {
        assert!(
            Instant::now() < deadline,
            "checker must enter the release gate"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    for number in 0..33 {
        aft.send_silent(&json!({"id":format!("held-{number}"),"command":"echo","message":format!("held-{number}")}).to_string());
    }
    aft.send_silent(
        &json!({"id":"blocked-edit","command":"write","file":target,"content":"must not execute"})
            .to_string(),
    );
    aft.send_silent(&json!({"id":"status-with-full-backlog","command":"status"}).to_string());
    let overflow = read_response_within(&mut aft, "held-32", Duration::from_secs(5))
        .0
        .expect("held overflow must be refused while validation remains gated");
    assert_eq!(overflow["code"], "request_queue_full");
    let duplicate = read_response_within(&mut aft, "blocked-edit", Duration::from_secs(5))
        .0
        .expect("duplicate must be refused before it is held");
    assert_eq!(duplicate["code"], "duplicate_request_id");
    let status = read_response_within(&mut aft, "status-with-full-backlog", Duration::from_secs(5))
        .0
        .expect("status must remain reachable after overflow");
    assert_eq!(status["success"], true);
    assert!(!release.exists());
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "const original = 1;\n"
    );
    fs::write(&release, "released").unwrap();
    let original = read_response_within(&mut aft, "blocked-edit", Duration::from_secs(30))
        .0
        .expect("original must retain its terminal owner");
    assert_eq!(original["success"], true);
    for number in 0..32 {
        let id = format!("held-{number}");
        let response = read_response_within(&mut aft, &id, Duration::from_secs(30))
            .0
            .expect("held request must replay");
        assert_eq!(response["message"], id);
    }
    assert!(aft.shutdown().success());
}

/// Issue #334: with `validate_on_edit: "full"`, a standalone bridge ran the
/// type checker on its only request thread, so `status` (5s passive budget)
/// and `bash_drain_completions` (15s) queued behind every edit and timed out.
/// The checker stub here blocks until the test releases it; `status` and the
/// drain poll must be answered while it is blocked, the edit response must
/// still carry the checker's result, and a second write to the same file must
/// wait for the first edit instead of racing its checker.
#[cfg(unix)]
#[test]
fn validate_on_edit_full_keeps_status_responsive_while_checker_runs() {
    use std::time::{Duration, Instant};

    let dir = format_test_dir("validate_full_status_responsive");
    let target = dir.join("validate_full_status_responsive.ts");
    let started = dir.join("checker-started");
    let release = dir.join("checker-release");
    let seen = dir.join("checker-seen");
    for path in [&target, &started, &release, &seen] {
        let _ = fs::remove_file(path);
    }
    fs::write(dir.join("tsconfig.json"), "{}\n").unwrap();

    // The stub marks that it started, waits for the release file, then records
    // the file content it checked and reports one TS2322 error for it.
    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let stub = bin_dir.join("tsc");
    let source = format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'Version 5.0.0'; exit 0; fi
: > '{started}'
i=0
while [ ! -f '{release}' ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done
cat '{target}' >> '{seen}'
printf '%s(1,7): error TS2322: Type string is not assignable to type number.\\n' '{target}'
exit 2
"#,
        started = started.display(),
        release = release.display(),
        target = target.display(),
        seen = seen.display(),
    );
    link_cached_executable(&stub, source.as_bytes()).unwrap();
    warm_executable(&stub, &["--version"]);
    let _release_guard = ReleaseOnDrop(release.clone());

    let mut aft = AftProcess::spawn();
    let cfg = aft.send(
        &json!({
            "id": "cfg-val-responsive",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir,
            "config": user_config(serde_json::json!({
                "validate_on_edit": "full",
                "checker": { "typescript": "tsc" }
            }))
        })
        .to_string(),
    );
    assert_eq!(cfg["success"], true, "configure should succeed: {cfg:?}");

    let first = "const x: number = \"one\";\n";
    let second = "const x: number = \"two\";\n";
    aft.send_silent(
        &json!({ "id": "slow-edit", "command": "write", "file": target, "content": first })
            .to_string(),
    );
    let wait_started = Instant::now();
    while !started.exists() {
        assert!(
            wait_started.elapsed() < Duration::from_secs(30),
            "checker stub never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Queue a second write to the same file behind the blocked edit, then poll.
    aft.send_silent(
        &json!({ "id": "second-edit", "command": "write", "file": target, "content": second })
            .to_string(),
    );
    let status_sent = Instant::now();
    aft.send_silent(&json!({ "id": "status-during-check", "command": "status" }).to_string());
    let (status, before_status) =
        read_response_within(&mut aft, "status-during-check", Duration::from_secs(5));
    let status_latency = status_sent.elapsed();
    let status = status.unwrap_or_else(|| {
        panic!("status was not answered within its 5s budget while the checker ran")
    });
    assert_eq!(status["success"], true, "status should succeed: {status:?}");
    assert!(
        before_status.is_empty(),
        "no edit may answer before the checker is released: {before_status:?}"
    );
    aft.send_silent(
        &json!({ "id": "drain-during-check", "command": "bash_drain_completions" }).to_string(),
    );
    let (drain, before_drain) =
        read_response_within(&mut aft, "drain-during-check", Duration::from_secs(15));
    assert!(
        drain.is_some(),
        "bash_drain_completions was not answered within its 15s budget"
    );
    assert!(before_drain.is_empty(), "{before_drain:?}");
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        first,
        "the held second write must not touch the file while the first edit is validating"
    );
    eprintln!(
        "status answered in {}ms while the checker was blocked",
        status_latency.as_millis()
    );

    fs::write(&release, "release").unwrap();
    let (edit, before_edit) = read_response_within(&mut aft, "slow-edit", Duration::from_secs(30));
    let edit = edit.expect("edit response should arrive after the checker finishes");
    assert!(before_edit.is_empty(), "{before_edit:?}");
    assert_eq!(edit["success"], true, "write should succeed: {edit:?}");
    let errors = edit["validation_errors"]
        .as_array()
        .expect("the deferred edit response must carry validation_errors");
    assert_eq!(errors.len(), 1, "{edit:?}");
    assert!(
        errors[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("TS2322"),
        "{edit:?}"
    );

    let (second_edit, _) = read_response_within(&mut aft, "second-edit", Duration::from_secs(30));
    let second_edit = second_edit.expect("held second write should run after the first");
    assert_eq!(second_edit["success"], true, "{second_edit:?}");
    assert_eq!(
        fs::read_to_string(&seen).unwrap(),
        format!("{first}{second}"),
        "each checker run must see its own write"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
/// Send write with validate:"full" on a .rs file with valid code → if cargo available,
/// response includes validation_errors: [] (empty).
#[test]
fn validate_full_with_checker() {
    if !is_on_path("cargo") {
        eprintln!("SKIP: cargo not on PATH");
        return;
    }

    let dir = format_test_dir("validate_valid");
    let target = dir.join("validate_valid.rs");
    // Write valid Rust code
    let _ = fs::remove_file(&target);

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"val-2","command":"write","file":{},"content":"fn main() {{}}","validate":"full"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    // With validate:"full" and cargo available, we should get validation fields
    // Note: cargo check on an isolated .rs file may skip or error (no Cargo.toml),
    // so we check that the validate path was invoked (either errors or skip reason present)
    let has_validation =
        resp.get("validation_errors").is_some() || resp.get("validate_skipped_reason").is_some();
    assert!(
        has_validation,
        "validate:full should produce validation_errors or validate_skipped_reason: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// Send write with validate:"full" on a .txt file → validate_skipped_reason: "unsupported_language"
#[test]
fn validate_full_unsupported_language() {
    let dir = format_test_dir("validate_unsupported");
    let target = dir.join("validate_unsupported.txt");
    let _ = fs::remove_file(&target);

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"val-3","command":"write","file":{},"content":"hello","validate":"full"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["validate_skipped_reason"], "unsupported_language",
        "should skip validation for unsupported language: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn validate_full_no_checker_configured() {
    let dir = format_test_dir("validate_no_checker_configured");
    let target = dir.join("validate_no_checker_configured.ts");
    let _ = fs::remove_file(&target);

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"val-3b","command":"write","file":{},"content":"const x = 1;\n","validate":"full"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["validate_skipped_reason"], "no_checker_configured",
        "should skip validation without checker config: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn validate_full_nonzero_without_diagnostics_reports_error() {
    let dir = format_test_dir("validate_checker_error_no_diagnostics");
    fs::write(dir.join("tsconfig.json"), "{}\n").unwrap();
    let target = dir.join("validate_checker_error_no_diagnostics.ts");
    let _ = fs::remove_file(&target);

    let bin_dir = dir.join("node_modules").join(".bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let stub = bin_dir.join("tsc");
    link_cached_executable(
        &stub,
        br#"#!/bin/sh
echo 'failed before diagnostics' >&2
exit 2
"#,
    )
    .unwrap();
    warm_executable(&stub, &["--version"]);

    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[("PATH", path.as_os_str())]);
    let cfg = aft.configure(&dir);
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);
    let resp = aft.send(&format!(
        r#"{{"id":"val-error-no-diag","command":"write","file":{},"content":"const x = 1;\n","validate":"full"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["validate_skipped_reason"], "error",
        "non-zero checker without parseable diagnostics must not look clean: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn validate_full_checker_not_installed() {
    let dir = format_test_dir("validate_checker_not_installed");
    fs::write(dir.join("tsconfig.json"), "{}\n").unwrap();
    let target = dir.join("validate_checker_not_installed.ts");
    let _ = fs::remove_file(&target);

    let path = prepend_path(&std::ffi::OsString::new(), &dir);
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("AFT_DISABLE_WELL_KNOWN_LOOKUP", std::ffi::OsStr::new("1")),
    ]);
    let cfg = aft.configure(&dir);
    assert_eq!(cfg["success"], true, "configure should succeed: {:?}", cfg);
    let resp = aft.send(&format!(
        r#"{{"id":"val-3c","command":"write","file":{},"content":"const x = 1;\n","validate":"full"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(resp["success"], true, "write should succeed: {:?}", resp);
    assert_eq!(
        resp["validate_skipped_reason"], "checker_not_installed",
        "should report missing checker binary: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}

/// Send write with validate:"full" via add_import to verify validate param flows through
/// all mutation commands (not just write).
#[test]
fn validate_full_flows_through_add_import() {
    let dir = format_test_dir("validate_import");
    let target = dir.join("validate_import.rs");
    // Create a valid Rust file first
    fs::write(&target, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

    let mut aft = AftProcess::spawn();
    let resp = aft.send(&format!(
        r#"{{"id":"val-4","command":"add_import","file":{},"module":"std::collections::HashMap","validate":"full"}}"#,
        crate::helpers::json_string(&target.display())
    ));

    assert_eq!(
        resp["success"], true,
        "add_import should succeed: {:?}",
        resp
    );
    // Validate param should flow through — either errors or skip reason
    let has_validation =
        resp.get("validation_errors").is_some() || resp.get("validate_skipped_reason").is_some();
    assert!(
        has_validation,
        "validate:full should produce validation_errors or validate_skipped_reason via add_import: {:?}",
        resp
    );

    let _ = fs::remove_file(&target);
    let status = aft.shutdown();
    assert!(status.success());
}
