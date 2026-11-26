use super::*;
use lsp_types::request::HoverRequest;
use lsp_types::{HoverParams, Position, TextDocumentIdentifier, TextDocumentPositionParams};

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
    let text = response["text"].as_str().expect("rendered text");
    // The cap's unknown file is explained once in the header; exact coverage
    // remains structured rather than repeating incompleteness in a body line.
    assert!(
        text.starts_with("PARTIAL — diagnostics unknown: rust-analyzer @ .: not examined:")
            && text.contains("(1 file); retry aft_inspect."),
        "{text}"
    );
}
