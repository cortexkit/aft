mod carrier_observation_tests;
#[cfg(unix)]
mod completion_replay_ordering_tests;
pub(crate) mod default_kill_tests;
mod launch_tests;
mod persistence;
mod pty_completion_tests;
mod registry;
#[cfg(unix)]
pub(crate) mod spawn_reply;
#[cfg(target_os = "linux")]
mod task_scope_tests;
mod watchdog_cost_tests;
