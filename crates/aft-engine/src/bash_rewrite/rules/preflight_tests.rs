use super::*;
use crate::bash_rewrite::RewriteDecision;
use crate::config::Config;
use crate::context::default_language_provider_factory;
use crate::hashline::integration::RegistrationRequest;
use crate::protocol::DEFAULT_SESSION_ID;
use std::fs;

/// Deciding a head/tail rewrite must not load a file larger than the
/// preflight budget, and a session that cannot rewrite reads nothing.
#[test]
fn head_tail_decisions_skip_whole_file_reads_they_cannot_use() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    fs::write(root.join("src/app.ts"), "one\ntwo\nthree\nfour\n").unwrap();
    // A sparse file: large on disk metadata, cheap to create.
    let big = fs::File::create(root.join("src/big.log")).unwrap();
    big.set_len(64 * 1024 * 1024).unwrap();
    drop(big);
    let ctx = AppContext::new(
        default_language_provider_factory(),
        Config {
            project_root: Some(root.clone()),
            experimental_bash_rewrite: true,
            ..Default::default()
        },
    );

    // No hashline session: tail -n +N declines before any read.
    reset_head_tail_preflight_reads();
    assert!(matches!(
        TailRule.decide("tail -n +2 src/app.ts", "tail-off", None, &ctx),
        RewriteDecision::Decline(_)
    ));
    assert_eq!(head_tail_preflight_reads(), 0);

    ctx.hashline_bindings().register(
        &root,
        DEFAULT_SESSION_ID.to_string(),
        RegistrationRequest {
            configured_enabled: true,
            edit_slot_survives: true,
            read_slot_survives: true,
        },
    );
    reset_head_tail_preflight_reads();
    assert!(matches!(
        HeadRule.decide("head -n 1 src/big.log", "big", None, &ctx),
        RewriteDecision::Decline(_)
    ));
    for command in ["tail -n 1 src/big.log", "tail -n +2 src/big.log"] {
        assert!(
            matches!(
                TailRule.decide(command, "big", None, &ctx),
                RewriteDecision::Decline(_)
            ),
            "{command}"
        );
    }
    assert_eq!(head_tail_preflight_reads(), 0);

    // A small file is read once to decide, even for tail -n +N.
    reset_head_tail_preflight_reads();
    assert!(matches!(
        TailRule.decide("tail -n +2 src/app.ts", "tail-on", None, &ctx),
        RewriteDecision::Accept(_)
    ));
    assert_eq!(head_tail_preflight_reads(), 1);
}
