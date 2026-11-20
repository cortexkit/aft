use super::*;

#[rustfmt::skip]
test_support_visibility! {
#[cfg_attr(test, test)]
fn child() {
    let Some(root) = std::env::var_os("AFT_TEST_SPEECH_LOCK_ROOT") else {
        return;
    };
    let expected = std::env::var("AFT_TEST_SPEECH_LOCK_EXPECT").unwrap();
    let acquired = acquire_speech_serving_lease(Path::new(&root)).is_ok();
    println!("speech-lock-child-executed");
    assert_eq!(acquired, expected == "available");
}
}

pub fn probe(root: &Path, expected: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cache_location::speech_serving_tests::child",
            "--nocapture",
        ])
        .env("AFT_TEST_SPEECH_LOCK_ROOT", root)
        .env("AFT_TEST_SPEECH_LOCK_EXPECT", expected)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("speech-lock-child-executed"),
        "child test did not run: {stdout}"
    );
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed"), "{stdout}");
}

#[test]
fn speech_serving_exclusion_lasts_until_process_exit_not_guard_drop() {
    let dir = tempfile::tempdir().unwrap();
    let guard = acquire_speech_serving_lease(dir.path()).unwrap();
    probe(dir.path(), "blocked");
    drop(guard);
    probe(dir.path(), "blocked");
    let other = tempfile::tempdir().unwrap();
    probe(other.path(), "available");
}
