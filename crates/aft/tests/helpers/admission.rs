#![allow(dead_code)]

use aft::context::AppContext;
use std::time::{Duration, Instant};

/// Drive post-ack maintenance before a fixture observes configured index work.
/// This does not admit a folder: a negative decision is returned unchanged.
pub fn settle(ctx: &AppContext) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        aft::runtime_drain::drain_deferred_configure_maintenance(ctx);
        let status = ctx.build_status_snapshot();
        if status["folder"]["state"] == "decided" {
            return status["folder"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "folder admission did not settle: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
