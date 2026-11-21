#[test]
fn a_read_still_running_at_the_deadline_without_a_cached_copy_is_pending() {
    let (start, release, _started) = crate::github_read::uncached_running_read();
    // The call budget, less the reply margin, bounds the wait.
    let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(
        std::time::Duration::from_millis(1_300),
    )));
    let started = std::time::Instant::now();
    let error = crate::commands::read::wait_for_github_read(start)
        .expect_err("no copy can stand in for the running read");
    // The wait ends one reply margin before the 1.3 s call budget.
    assert!(started.elapsed() < std::time::Duration::from_millis(1_300));
    assert_eq!(error.code(), "github_read_pending", "{error}");
    assert!(
        error.to_string().contains("retry this call shortly"),
        "{error}"
    );
    release.send(()).unwrap();
}
