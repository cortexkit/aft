use std::time::Duration;

use crate::hashline::release::channel0_health_reply_avoids_hashline_stores;

#[test]
fn a13_channel0_health_reply_does_not_access_hashline_stores_at_capacity() {
    let report = channel0_health_reply_avoids_hashline_stores();
    assert_eq!(report.reply_status, "ok");
    assert!(
        !report.hashline_stores_accessed,
        "health reply must not access hashline stores"
    );
    assert!(
        report.snapshot_paths_at_capacity,
        "probe must run with the snapshot store at MAX_SNAPSHOT_PATHS"
    );

    let health_mentions_hashline = include_str!("../../health.rs")
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//") && !trimmed.starts_with('*')
        })
        .any(|line| line.contains("hashline"));
    assert!(
        !health_mentions_hashline,
        "subc health source must not reference hashline (channel-0 isolation)"
    );
    // A 50 ms ceiling catches unexpected work in the simulated JSON reply.
    // It is not the release benchmark for tagging or read rendering.
    assert!(
        report.reply_elapsed < Duration::from_millis(50),
        "health-shaped reply took {:?}, suggesting unexpected work",
        report.reply_elapsed
    );
    let _ = report.session_key;
}
