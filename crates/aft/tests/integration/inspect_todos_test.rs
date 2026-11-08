use std::fs;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::test_helpers::AftProcess;

// The comment rules themselves (comment nodes, Markdown comments, strings,
// prose) are unit-tested where the index store records the markers
// (`inspect::scanners::todos`); this checks the whole path through a
// configured AFT, whose TODO count reads the store's records.

#[test]
fn inspect_todos_binary_uses_comment_nodes_without_regressing_real_comments() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let src = temp_dir.path().join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        src.join("strings.rs"),
        "fn main() {\n\
             let plain = \" // TODO: not a comment\";\n\
             let escaped = \"\\\" // FIXME: not a comment\";\n\
             let raw = r#\"// TODO: not a comment\"#;\n\
         }\n\
         // TODO: real Rust line comment\n\
         /* FIXME: real Rust block comment */\n",
    )
    .expect("write Rust fixture");
    fs::write(
        src.join("template.ts"),
        "const fake = `// HACK: not a comment`;\n",
    )
    .expect("write TypeScript fixture");
    fs::write(src.join("worker.py"), "# HACK: real hash comment\n").expect("write Python fixture");
    fs::write(
        src.join("page.html"),
        "<script>const fake = \"<!-- TODO: not a comment -->\";</script>\n\
         <!-- XXX: real HTML comment -->\n",
    )
    .expect("write HTML fixture");
    fs::write(
        temp_dir.path().join("README.md"),
        "<!-- BUG: real Markdown comment -->\n",
    )
    .expect("write Markdown fixture");
    fs::write(src.join("script.lua"), "-- BUG: real Lua comment\n").expect("write Lua fixture");

    let mut aft = AftProcess::spawn();
    let configured = aft.configure(temp_dir.path());
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    // The count is pending until the index store has built its records.
    let deadline = Instant::now() + Duration::from_secs(120);
    let response = loop {
        let response = aft.send(
            &json!({
                "id": "inspect-todos-comments",
                "command": "tool_call",
                "name": "inspect",
                "arguments": { "sections": ["todos"] }
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "inspect failed: {response:?}");
        if response["summary"]["todos"]["count"].is_u64() {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "the TODO count never arrived: {response:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };

    assert_eq!(response["summary"]["todos"]["count"], 6, "{response:?}");
    assert_eq!(response["summary"]["todos"]["by_kind"]["TODO"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["FIXME"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["HACK"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["XXX"], 1);
    assert_eq!(response["summary"]["todos"]["by_kind"]["BUG"], 2);

    let items = response["details"]["todos"]
        .as_array()
        .expect("TODO details array");
    assert!(items.iter().all(|item| {
        !item["text"]
            .as_str()
            .is_some_and(|text| text.contains("not a comment"))
    }));
}
