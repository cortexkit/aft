pub use aft_github::gh_shim_speech::test_bridge::*;

pub fn seal_spawn(metadata: &crate::bash_background::persistence::PersistedTask) {
    aft_shell::test_bridge::seal_spawn(metadata);
}
