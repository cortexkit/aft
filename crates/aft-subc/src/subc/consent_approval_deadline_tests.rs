/// The approval wait never runs into the time the approved run needs.
#[test]
fn standalone_approval_deadline_leaves_the_rerun_margin() {
    let now = Instant::now();
    let far = now + Duration::from_secs(600);
    assert_eq!(
        standalone::approval_deadline(now, far, Duration::from_secs(60)),
        now + Duration::from_secs(60)
    );
    let near = now + Duration::from_secs(20);
    assert_eq!(
        standalone::approval_deadline(now, near, Duration::from_secs(60)),
        near - RERUN_MARGIN
    );
    let past = now + Duration::from_secs(1);
    assert_eq!(
        standalone::approval_deadline(now, past, Duration::from_secs(60)),
        now
    );
}
