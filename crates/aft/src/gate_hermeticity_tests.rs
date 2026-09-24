//! `scripts/rust-test-gate.sh` gives the suite its own XDG config, state, cache
//! and data homes and strips the git-config injection an aft agent shell
//! carries. The test here checks what the production resolvers actually return
//! under that environment, so removing the isolation from the script fails the
//! gate instead of silently pointing test processes at the operator's live
//! config and gh-shim state again.

use std::path::PathBuf;

/// Outside the gate (plain `cargo test`) there is nothing to check and the test
/// reports itself skipped. Inside the gate, a missing isolation marker is itself
/// a failure, so the check cannot pass vacuously.
#[test]
fn gate_resolves_every_user_config_and_state_path_under_the_gate_homes() {
    if std::env::var_os("AFT_RUST_TEST_GATE").is_none() {
        eprintln!("skipped: only meaningful under scripts/rust-test-gate.sh");
        return;
    }
    // Other tests temporarily override HOME, XDG_* and the gh-shim state
    // directory while holding this lock; read the environment only while no such
    // override is installed.
    let _lock = crate::test_env::process_env_lock();
    let root = std::env::var_os("AFT_GATE_HERMETIC_HOME_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .expect(
            "running under rust-test-gate.sh but AFT_GATE_HERMETIC_HOME_ROOT is unset: \
             the gate no longer isolates the XDG homes",
        );

    let mut resolved: Vec<(&str, PathBuf)> = vec![
        (
            "user-tier aft.jsonc",
            crate::subc_config::cortexkit_user_config_path().expect("user config path resolves"),
        ),
        ("storage root", crate::bash_background::storage_dir(None)),
        (
            "fastembed model cache",
            crate::local_embed::embedding_cache_dir().expect("model cache resolves"),
        ),
        (
            "gh credential wrapper config dirs",
            PathBuf::from(crate::gh_shim::wrapper_config_dir_pattern_from_process(
                "~/.config/gh-alfonso-*/",
            )),
        ),
    ];
    #[cfg(unix)]
    resolved.push((
        "effective-path cache",
        crate::effective_path::effective_path_cache_path(),
    ));
    resolved.extend(
        crate::gh_shim::process_state_files_for_test()
            .into_iter()
            .map(|path| ("gh-shim state", path)),
    );

    let escaped: Vec<String> = resolved
        .iter()
        .filter(|(_, path)| !path.starts_with(&root))
        .map(|(label, path)| format!("{label}: {}", path.display()))
        .collect();
    assert!(
        escaped.is_empty(),
        "paths resolved outside the gate homes under {}:\n{}",
        root.display(),
        escaped.join("\n")
    );

    let injected: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| {
            name == "GIT_CONFIG_COUNT"
                || name == "GIT_CONFIG_PARAMETERS"
                || name == "AFT_GIT_CO_AUTHOR"
                || name.starts_with("GIT_CONFIG_KEY_")
                || name.starts_with("GIT_CONFIG_VALUE_")
        })
        .collect();
    assert!(
        injected.is_empty(),
        "the gate inherited git-config injection variables: {injected:?}"
    );
}
