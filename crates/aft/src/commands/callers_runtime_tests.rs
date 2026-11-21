mod tests {
    use crate::data_layer::runtime::tests::PublicationFixture;
    use crate::write_intent::{WriteIntent, WritePhase};
    use std::time::Duration;

    #[test]
    fn worker_only_edits_wait_for_callers_then_refresh_once() {
        let quiet = Duration::from_millis(100);
        let fixture = PublicationFixture::new(quiet, quiet);
        let ctx = fixture.context_with(crate::test_context);
        ctx.set_worker_only_routes(true);
        let before = fixture.refreshes();
        let lib = fixture.root().join("src/lib.rs");
        let extra = fixture.root().join("src/extra.rs");
        std::fs::write(&extra, "pub fn new_caller() { crate::original(); }\n").unwrap();
        fixture.note_paths(std::slice::from_ref(&extra));
        std::fs::write(&lib, "mod extra;\npub fn original() {}\n").unwrap();
        fixture.record_change(&WriteIntent::Paths(vec![lib.clone()]), WritePhase::After);
        std::thread::sleep(quiet * 3);
        assert_eq!(fixture.refreshes(), before);
        assert!(fixture.pending());
        assert!(!fixture.quiet_armed());

        let response = crate::commands::callers::handle_callers(
            &crate::protocol::RawRequest {
                id: "worker-callers".into(),
                command: "callers".into(),
                lsp_hints: None,
                session_id: None,
                params: serde_json::json!({"file": lib, "symbol": "original"}),
            },
            &ctx,
        );
        assert!(response.success, "{:?}", response.data);
        assert!(
            response.data.to_string().contains("new_caller"),
            "{:?}",
            response.data
        );
        fixture.wait_for_work();
        assert_eq!(fixture.refreshes(), before + 1);
        assert!(!fixture.pending());
    }
}
