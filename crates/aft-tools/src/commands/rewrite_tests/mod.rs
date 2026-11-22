pub(crate) mod bash_background;
mod blocking_wait;
mod dispatch;
pub(crate) mod remote;
mod rules;

#[cfg(target_os = "linux")]
mod git_policy_cache_tests;

#[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
mod speech_dispatch_tests;
