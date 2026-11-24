pub use aft_github::gh_shim_speech::test_bridge::*;

pub fn seal_spawn(metadata: &crate::bash_background::persistence::PersistedTask) {
    crate::gh_shim_ticket::seal_spawn(metadata);
}
