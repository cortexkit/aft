// Isolated integration binary for tests that depend on the real OS file
// watcher (FSEvents on macOS, inotify on Linux).
//
// These tests spawn `aft` with a live recursive watcher and assert that a
// file change is delivered and triggers an index/cache refresh. When they ran
// inside the main `integration` binary (1150+ tests, ~600 concurrent `aft`
// process spawns), the watcher attach itself probabilistically hung under the
// load — `watch()` never returned for ~1-in-3 watcher processes, so the event
// was never delivered and the test timed out. The failure was a class problem
// (whichever watcher test drew the hung attach lost), not a single bad test.
//
// cargo runs separate test binaries sequentially, so this binary runs alone:
// no concurrent process load, the watcher attaches promptly, and these tests
// are deterministic. `helpers::watcher_serial_lock` additionally serializes
// them within this binary so at most one live watcher exists at a time.

#[path = "../helpers/mod.rs"]
mod test_helpers;

#[path = "../integration/helpers.rs"]
mod helpers;

#[path = "../integration/branch_switch_test.rs"]
mod branch_switch_test;
mod callgraph_watcher_test;
mod configure_watcher_test;
mod idle_root_watcher_test;
mod semantic_refresh_watcher_test;
mod watcher_search_semantic_regression_test;
