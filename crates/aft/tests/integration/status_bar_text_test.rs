//! The AFT status bar as a standalone (NDJSON) agent sees it: a trailing line of
//! the tool-result text, emitted when a count changes and not on an unchanged
//! result. A standalone bridge has no fleet holder, so nothing else renders it.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::test_helpers::AftProcess;

const SESSION: &str = "status-bar-text-session";

fn agent_text(aft: &mut AftProcess, id: &str, name: &str, arguments: Value) -> String {
    let response = aft.send_with_timeout(
        &serde_json::to_string(&json!({
            "id": id,
            "command": "tool_call",
            "session_id": SESSION,
            "name": name,
            "arguments": arguments,
        }))
        .expect("serialize tool_call"),
        Duration::from_secs(60),
    );
    assert_eq!(response["success"], true, "{name} failed: {response:?}");
    assert!(
        response.get("status_bar").is_none(),
        "the bar never rides as a structured field: {response:?}"
    );
    response["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name} response missing text: {response:?}"))
        .to_string()
}

/// The trailing status-bar line, if the text ends with one. Diagnostics may or
/// may not have reported on a given machine, so callers match the Tier-2 tail.
fn trailing_bar(text: &str) -> Option<&str> {
    let (_, last) = text.rsplit_once("\n\n")?;
    (last.starts_with("[AFT E") && last.ends_with(']')).then_some(last)
}

fn inspect(aft: &mut AftProcess, id: &str) -> String {
    agent_text(aft, id, "aft_inspect", json!({}))
}

fn read(aft: &mut AftProcess, id: &str, file: &Path) -> String {
    agent_text(aft, id, "read", json!({ "filePath": file }))
}

#[test]
fn standalone_status_bar_trails_text_on_change_and_not_on_unchanged_result() {
    let project = tempfile::tempdir().expect("status bar project");
    let file = project.path().join("a.ts");
    std::fs::write(&file, "// TODO: first\nexport const a = 1;\n").expect("write fixture");
    let mut aft = AftProcess::spawn();
    aft.configure(project.path());

    // Inspect produces the Tier-2 and TODO counts, so its own result is the change.
    let text = inspect(&mut aft, "inspect-1");
    let bar = trailing_bar(&text).unwrap_or_else(|| panic!("no trailing bar: {text:?}"));
    assert!(bar.ends_with("| D1 U1 C0 | T1]"), "unexpected bar {bar:?}");

    let text = read(&mut aft, "read-unchanged", &file);
    // The contract is that the bar is emitted only when it changes. On Windows the
    // watcher can deliver a late event for the fixture write, which marks the
    // Tier-2 counts stale (`~D1`) between the two calls; that is a real change
    // and may be shown. Repeating the identical bar is what must never happen.
    if let Some(next) = trailing_bar(&text) {
        assert_ne!(
            next, bar,
            "an unchanged result must not repeat the bar: {text:?}"
        );
    } else {
        assert!(
            !text.contains("[AFT "),
            "a bar may only trail the text: {text:?}"
        );
    }

    std::fs::write(
        &file,
        "// TODO: first\n// TODO: second\nexport const a = 1;\n",
    )
    .expect("rewrite fixture");
    let text = inspect(&mut aft, "inspect-2");
    let bar = trailing_bar(&text).unwrap_or_else(|| panic!("no trailing bar: {text:?}"));
    assert!(
        bar.ends_with("| T2]"),
        "changed TODO count not shown: {bar:?}"
    );

    assert!(aft.shutdown().success());
}
