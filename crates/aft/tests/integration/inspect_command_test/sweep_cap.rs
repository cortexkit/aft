use super::*;
use lsp_types::request::HoverRequest;
use lsp_types::{HoverParams, Position, TextDocumentIdentifier, TextDocumentPositionParams};

#[test]
fn scoped_inspect_reopen_keeps_the_clean_publish_queued_behind_the_close_reply() {
    let (_temp, root) = fixture_project();
    write_file(
        &root,
        "settings.gradle",
        "rootProject.name = 'close-reply'\n",
    );
    write_file(&root, "warm.kt", "fun warm() {}\n");
    let file = write_file(&root, "target.kt", "fun target() {}\n");
    let ctx = configured_context_with_diagnostics_timeout(&root, 5_000);
    let ready = root.join("reopen-burst-ready");
    let uri = file_uri(&file);
    {
        let mut lsp = ctx.lsp();
        lsp.override_binary(ServerKind::KotlinLs, fake_server_path());
        lsp.set_extra_env("AFT_FAKE_LSP_UNVERSIONED_PUSH", "1");
        lsp.set_extra_env("AFT_FAKE_LSP_REOPEN_BURST_URI", &uri);
        lsp.set_extra_env("AFT_FAKE_LSP_REOPEN_BURST_READY", ready.to_str().unwrap());
    }

    // Let the manager observe a close reply from this server before holding
    // the target file's reply back until the next inspect reopens it.
    let warm = scoped_diagnostics_inspect(&ctx, "close-reply-warm", "warm.kt");
    assert_eq!(warm["complete"], true, "{warm:#}");
    wait_for_close_reply(&ctx);
    let first = scoped_diagnostics_inspect(&ctx, "close-reply-first", "target.kt");
    assert_eq!(first["summary"]["diagnostics"]["errors"], 1, "{first:#}");
    assert!(!ctx.lsp().document_is_open_for_test(&file));

    let reopened = thread::scope(|thread| {
        let inspect =
            thread.spawn(|| scoped_diagnostics_inspect(&ctx, "close-reply-reopen", "target.kt"));
        wait_for_path_event(&ready, "fake analyzer to receive the reopened document");
        let config = ctx.config().clone();
        {
            // The reader ingests both publishes before waking this request.
            // The manager lock prevents a drain between the two publishes.
            let mut lsp = ctx.lsp();
            lsp.client_for_file_mut(&file, &config)
                .unwrap()
                .send_request::<HoverRequest>(HoverParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.parse().unwrap()),
                        Position::new(0, 0),
                    ),
                    work_done_progress_params: Default::default(),
                })
                .expect("flush the close reply and clean report before draining");
        }
        inspect.join().unwrap()
    });

    assert_eq!(
        reopened["summary"]["diagnostics"]["coverage"]["authoritative"], 1,
        "the reopened file's clean publish must not be discarded as its close reply: {reopened:#}"
    );
    assert_eq!(
        reopened["summary"]["diagnostics"]["errors"], 0,
        "{reopened:#}"
    );
}

/// The scope reports its exact cap even when the analyzer batches all of its
/// didOpen diagnostics and the manager cannot drain them during the burst.
#[test]
fn scoped_blocking_inspect_caps_the_files_it_opens() {
    let (_temp_dir, root, lib) = single_crate_fixture("sweep-cap");
    for index in 0..1000 {
        write_file(&root, &format!("src/m{index:04}.rs"), "pub fn g() {}\n");
    }
    let ctx = configured_context_with_diagnostics_timeout(&root, 40_000);
    configure_fake_rust_lsp(&ctx);
    let buffered = root.join("burst-ready");
    {
        let mut lsp = ctx.lsp();
        lsp.set_extra_env("AFT_FAKE_LSP_OPEN_BURST_READY", buffered.to_str().unwrap());
        lsp.set_extra_env("AFT_FAKE_LSP_OPEN_BURST_COUNT", "1000");
    }
    let response = thread::scope(|thread| {
        let inspect = thread.spawn(|| scoped_diagnostics_inspect(&ctx, "inspect-sweep-cap", "src"));
        wait_for_path_event(&buffered, "fake analyzer to buffer 1,000 didOpen reports");
        let config = ctx.config().clone();
        {
            // The fake sends the buffered publishes before its hover reply;
            // the reader has ingested every publish by the time this returns.
            // Holding the manager lock rules out a concurrent drain.
            let mut lsp = ctx.lsp();
            let client = lsp.client_for_file_mut(&lib, &config).unwrap();
            client
                .send_request::<HoverRequest>(HoverParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(file_uri(&lib).parse().unwrap()),
                        Position::new(0, 0),
                    ),
                    work_done_progress_params: Default::default(),
                })
                .expect("flush all buffered reports before releasing the manager lock");
        }
        inspect.join().unwrap()
    });

    let uncovered = uncovered_files(&response);
    assert_eq!(
        response["summary"]["diagnostics"]["coverage"],
        json!({"files": 1001, "examined": 1000, "authoritative": 1000, "not_examined": 1, "file_cap": 1000}),
        "first uncovered file: {:?}",
        uncovered.first()
    );
    assert_eq!(uncovered.len(), 1, "response: {response:#}");
    assert_eq!(uncovered[0].0, "src/m0999.rs", "response: {response:#}");
    assert!(
        uncovered[0].1.starts_with("not examined:"),
        "response: {response:#}"
    );
    // The cap's unknown file is explained once in the header; exact coverage
    // remains structured. Verifying that the files are still current can
    // hit the deadline after all 1,000 diagnostic reports have been collected.
    assert_sweep_cap_header(&response);
}

fn assert_sweep_cap_header(response: &Value) {
    const CAP: &str = "PARTIAL — diagnostics unknown: rust-analyzer @ .: not examined: a scoped inspect opens at most 1000 files; narrow the scope (1 file)";
    const FRESHNESS: &str = "file freshness verification exceeded the request budget";
    const RETRY: &str = "retry aft_inspect.";
    let gaps = response["gaps"].as_array().expect("structured gaps");
    let freshness = gaps
        .iter()
        .find(|gap| gap["kind"] == "stat_verification_incomplete");
    let expected = if let Some(gap) = freshness {
        assert_eq!(gap["reason"], format!("{FRESHNESS}; retry aft_inspect"));
        format!("{CAP}; file freshness unverified: {FRESHNESS}; {RETRY}")
    } else {
        format!("{CAP}; {RETRY}")
    };
    let text = response["text"].as_str().expect("rendered text");
    assert_eq!(text.lines().next(), Some(expected.as_str()), "{text}");
}

#[test]
fn sweep_cap_header_can_explain_a_later_freshness_deadline() {
    assert_sweep_cap_header(&json!({
        "gaps": [],
        "text": "PARTIAL — diagnostics unknown: rust-analyzer @ .: not examined: a scoped inspect opens at most 1000 files; narrow the scope (1 file); retry aft_inspect.\nscope: 1 root, 1001 files"
    }));
    assert_sweep_cap_header(&json!({
        "gaps": [{
            "kind": "stat_verification_incomplete",
            "reason": "file freshness verification exceeded the request budget; retry aft_inspect"
        }],
        "text": "PARTIAL — diagnostics unknown: rust-analyzer @ .: not examined: a scoped inspect opens at most 1000 files; narrow the scope (1 file); file freshness unverified: file freshness verification exceeded the request budget; retry aft_inspect.\nscope: 1 root, 1001 files"
    }));
}
