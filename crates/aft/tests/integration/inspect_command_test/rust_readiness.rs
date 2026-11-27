use super::*;

/// A cold request can expire before rust-analyzer reports quiescence. Once
/// the relay delivers that status, it delays compiler-check progress and
/// diagnostic reports to exercise the separate wait for the first check.
#[test]
fn scoped_rust_inspect_waits_for_a_late_first_cargo_check_with_real_rust_analyzer() {
    if !crate::helpers::real_rust_analyzer_available(
        "scoped_rust_inspect_waits_for_a_late_first_cargo_check_with_real_rust_analyzer",
    ) {
        return;
    }
    let (_temp_dir, root, _s) = field_removal_crate();
    write_file(
        &root,
        "src/moves.rs",
        "pub fn twice(v: Vec<u8>) -> (Vec<u8>, Vec<u8>) {\n    (v, v)\n}\n",
    );
    write_file(
        &root,
        "src/lib.rs",
        "pub mod moves;\npub mod s;\npub mod user;\n",
    );
    let ctx = configured_context(&root);
    enable_automatic_rust_checks(&ctx);
    let release = root.join("quiescence.release");
    {
        let mut lsp = ctx.lsp();
        lsp.override_binary(ServerKind::Rust, fake_server_path());
        lsp.set_extra_env("AFT_FAKE_LSP_PROXY", "rust-analyzer");
        lsp.set_extra_env("AFT_FAKE_LSP_PROXY_CHECK_DELAY_MS", "3000");
        lsp.set_extra_env(
            "AFT_FAKE_LSP_PROXY_QUIESCENCE_RELEASE_FILE",
            release.to_str().unwrap(),
        );
    }

    let cold = scoped_diagnostics_inspect(&ctx, "before-quiescence", "src");
    assert_eq!(cold["inspect_terminal"], "partial", "{cold:#}");
    assert_eq!(cold["complete"], false, "{cold:#}");
    assert!(
        cold["summary"]["diagnostics"]["by_producer"]["rust"]["errors"].is_null(),
        "{cold:#}"
    );
    assert_eq!(
        cold["summary"]["diagnostics"]["coverage"]["authoritative"], 0,
        "{cold:#}"
    );
    assert!(
        cold["gaps"].as_array().unwrap().iter().any(|gap| {
            gap["kind"] == "failed_producer"
                && gap["producer"] == "rust"
                && gap["root"] == "."
                && gap["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("still indexing after "))
        }),
        "{cold:#}"
    );

    wait_for_path_event(
        &release.with_extension("ready"),
        "proxy to hold quiescent status",
    );
    fs::write(release, []).unwrap();
    let response = scoped_diagnostics_inspect(&ctx, "late-first-check", "src");
    if reported_check_still_running(&response) {
        return;
    }
    // The moved-value error comes from the compiler, not rust-analyzer's own analysis.
    assert!(
        diagnostic_sources_for(&response, "src/moves.rs")
            .iter()
            .any(|(_, message)| message.contains("moved value")),
        "the compiler error of the late check is missing: {response:#}"
    );
}
