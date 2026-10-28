use super::*;

#[test]
fn child() {
    let Some(root) = std::env::var_os("AFT_TEST_SPEECH_LOCK_ROOT") else {
        return;
    };
    let expected = std::env::var("AFT_TEST_SPEECH_LOCK_EXPECT").unwrap();
    let acquired = acquire_speech_serving_lease(Path::new(&root)).is_ok();
    println!("speech-lock-child-executed");
    assert_eq!(acquired, expected == "available");
}

fn probe(root: &Path, expected: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "root_cache::location::speech_serving_tests::child",
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

#[tokio::test]
async fn speech_runtime_successor_waits_for_quiescent_relay_and_finalization() {
    let dir = tempfile::tempdir().unwrap();
    let relay = crate::gh_shim_relay::GhRelay::new(dir.path().join("unused-connection.json"));
    let first = crate::gh_shim_speech::service::start(
        dir.path(),
        &relay,
        Arc::new(|_| Box::pin(async { Err("unused describe".into()) })),
        "first-runtime".into(),
        false,
    );
    let first_runtime = first.runtime_guard();
    assert!(first.try_serve());
    probe(dir.path(), "blocked");
    assert!(
        acquire_speech_serving_lease(dir.path()).is_err(),
        "a same-process caller must not reuse the holder's lease"
    );
    let post = first.begin_post().unwrap();
    let finalization = first.track_finalization();
    let successor = crate::gh_shim_speech::service::start(
        dir.path(),
        &relay,
        Arc::new(|_| Box::pin(async { Err("unused describe".into()) })),
        "successor-runtime".into(),
        false,
    );
    let successor_runtime = successor.runtime_guard();
    assert!(!Arc::ptr_eq(&first, &successor));
    assert!(!successor.try_serve());
    assert!(
        !first_runtime.retire(),
        "outstanding work prevents quiescence"
    );
    assert_eq!(first.begin_post().err().unwrap().reason, "not_serving_mode");
    drop(post);
    assert!(
        !successor.try_serve(),
        "reply finalization still owns the first runtime"
    );
    probe(dir.path(), "blocked");
    drop(finalization);
    assert!(
        successor.try_serve(),
        "the last old work guard must release the real OS lock"
    );
    drop(first_runtime);
    assert!(
        successor.try_serve(),
        "old retirement must not release its successor's lease"
    );
    probe(dir.path(), "blocked");
    assert!(successor_runtime.retire());
    probe(dir.path(), "available");
}
