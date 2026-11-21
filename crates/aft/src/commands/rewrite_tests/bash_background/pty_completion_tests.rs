use crate::bash_background::registry::test_support::ConPtyFixture as Fixture;

fn assert_status_text(data: serde_json::Value, reason: &str) {
    let text = crate::subc_format::format_response(
        "bash_status",
        &crate::protocol::Response::success("status", data),
        false,
    );
    assert!(text.contains(reason), "status text: {text}");
}

#[test]
fn conpty_completion_drain_deadline_reports_incomplete_output() {
    for exit_code in [0, 17] {
        let fixture = Fixture::with_exit_code(false, exit_code);
        fixture.expire_drain_deadline();
        fixture.poll();
        fixture.assert_incomplete(exit_code, assert_status_text);
        fixture.finish_reader();
        fixture.poll();
        fixture.assert_incomplete(exit_code, assert_status_text); // Late EOF must not rewrite the declared outcome.
    }
}

#[test]
fn conpty_completion_reader_error_reports_incomplete_output() {
    for exit_code in [0, 17] {
        let fixture = Fixture::with_exit_code(true, exit_code);
        fixture.finish_reader();
        fixture.poll();
        fixture.assert_incomplete(exit_code, assert_status_text);
        fixture.poll();
        fixture.assert_incomplete(exit_code, assert_status_text);
    }
}
