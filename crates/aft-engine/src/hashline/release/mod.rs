//! Performance and release gates for the hashline edit surface (slice 9).
//!
//! This module owns:
//! - **A13** — pinned-runner performance method (1 MiB fixture, 3 warm-ups,
//!   10 timed reps, median tag ≤ 1 ms, gate-on/off read-render median delta
//!   ≤ 10%) and the channel-0 health isolation probe at full snapshot
//!   residency
//! - **FINAL GATES** — ownership + nonintersecting fence manifests covering
//!   A1–A18 and the release-train gate inventory (rust-test-gate, TypeScript
//!   suites, governed-artifact audit, Windows path/CRLF CI)
//!
//! File fence: `crates/aft/src/hashline/release/**`.

mod fixture;
mod manifests;
mod performance;
mod windows_path_crlf;

pub use fixture::{
    build_a13_fixture, build_a13_fixture_crlf, normalize_agent_path_spelling, sha256_hex,
    FIXTURE_SHA256_HEX, FIXTURE_SIZE_BYTES,
};
pub use manifests::{
    load_and_check_fences, load_final_gates, load_ownership, noise_policy_is_well_formed,
    ownership_owners_are_fenced, REQUIRED_ROWS, REQUIRED_SLICES,
};
pub use performance::{
    channel0_health_reply_avoids_hashline_stores, fill_snapshot_store_to_path_maximum,
    measure_render_medians, measure_tag_median, median_duration, ratio_delta,
    HealthIsolationReport, RENDER_REGRESSION_MAX_RATIO, TAG_MEDIAN_MAX, TIMED_REPS, WARMUPS,
};
