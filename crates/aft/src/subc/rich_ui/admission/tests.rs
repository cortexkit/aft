use super::*;
use serde_json::json;

fn manifest() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../../tests/fixtures/rich_ui/manifest.jcs.json"
    ))
    .unwrap()
}

fn prepare(name: &str, arguments: Value) -> Result<Operation, ErrorBody> {
    prepare_with_manifest(name, &arguments, Some(&manifest()))
}

#[test]
fn operation_admission_precedence_and_bound_fields() {
    for args in [
        json!({}),
        json!({"rich_ui_op":null}),
        json!({"rich_ui_op":"read"}),
        json!({"rich_ui_op":"read@x"}),
        json!({"rich_ui_op":"read@1@2"}),
    ] {
        assert_eq!(
            prepare("read", args).unwrap_err().code,
            "rich_ui_op_malformed"
        );
    }
    for encoded in ["read@2", "unpublished@1"] {
        assert_eq!(
            prepare("grep", json!({"rich_ui_op":encoded,"session":"canary"}))
                .unwrap_err()
                .code,
            "operation_version_unsupported"
        );
    }
    assert_eq!(
        prepare("grep", json!({"rich_ui_op":"read@1","session":"canary"}))
            .unwrap_err()
            .code,
        "operation_target_mismatch"
    );
    for key in ["session", "session_id", "sessionId"] {
        let mut args = json!({"rich_ui_op":"read@1","path":"x".repeat(8193)});
        args[key] = json!("canary");
        assert_eq!(
            prepare("read", args).unwrap_err().code,
            "bound_field_supplied"
        );
    }
    for (name, args) in [
        ("bash_status", json!({"rich_ui_op":"bash_status@1"})),
        (
            "bash_status",
            json!({"rich_ui_op":"bash_status@1","task":4}),
        ),
        (
            "read",
            json!({"rich_ui_op":"read@1","task":"key","path":"x"}),
        ),
        (
            "bash_status",
            json!({"rich_ui_op":"bash_status@1","task":"key","taskId":"id"}),
        ),
    ] {
        assert_eq!(prepare(name, args).unwrap_err().code, "args_invalid");
    }
    let op = prepare(
        "bash_status",
        json!({"rich_ui_op":"bash_status@1","task":"viewed-key"}),
    )
    .unwrap();
    assert_eq!(op.task.as_deref(), Some("viewed-key"));
    assert_eq!(op.arguments, json!({}));
    assert_eq!(
        prepare(
            "aft_outline",
            json!({"rich_ui_op":"outline@1","target":"x"})
        )
        .unwrap()
        .entry["id"],
        "outline"
    );
}

#[test]
fn operation_arguments_caps_closed_fields_and_remote_targets() {
    let overhead = serde_json::to_vec(&json!({"path":""})).unwrap().len();
    assert!(prepare(
        "read",
        json!({"rich_ui_op":"read@1","path":"x".repeat(8192-overhead)})
    )
    .is_ok());
    assert_eq!(
        prepare(
            "read",
            json!({"rich_ui_op":"read@1","path":"x".repeat(8193-overhead)})
        )
        .unwrap_err()
        .code,
        "args_invalid"
    );
    for extra in [
        json!({"ref":"HEAD"}),
        json!({"output_offset":0}),
        json!({"outputMode":"raw"}),
        json!({"background":true}),
        json!({"once":true}),
    ] {
        let mut args = json!({"rich_ui_op":"bash_status@1","task":"key"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(
            prepare("bash_status", args).unwrap_err().code,
            "args_invalid"
        );
    }
    for regex in [false, true] {
        assert!(prepare("bash_watch", json!({"rich_ui_op":"bash_watch@1","task":"key","pattern":"needle","regex":regex,"timeoutMs":20_000})).is_ok());
    }
    assert_eq!(
        prepare(
            "bash_watch",
            json!({"rich_ui_op":"bash_watch@1","task":"key","pattern":"needle","timeoutMs":20_001})
        )
        .unwrap_err()
        .code,
        "args_invalid"
    );
    for remote in [
        "https://example.com",
        "http://example.com",
        "issue://123",
        "pr://123",
    ] {
        for (name, args) in [
            ("read", json!({"path":remote})),
            ("outline", json!({"target":remote})),
            ("outline", json!({"target":[remote]})),
            ("zoom", json!({"batch":[{"target":remote,"symbols":["x"]}]})),
            ("grep", json!({"path":remote,"pattern":"x"})),
            (
                "callgraph",
                json!({"op":"callers","path":"x","toPath":remote,"symbol":"x"}),
            ),
        ] {
            let mut args = args;
            args["rich_ui_op"] = json!(format!("{name}@1"));
            assert_eq!(
                prepare(name, args).unwrap_err().code,
                "args_invalid",
                "{name} {remote}"
            );
        }
    }
}

#[test]
fn watch_arguments_are_closed_and_timeouts_are_bounded() {
    let base = json!({"rich_ui_op":"bash_watch@1","task":"key","pattern":"needle"});
    for (key, value) in [
        ("background", json!(true)),
        ("once", json!(true)),
        ("deliver", json!("each_line")),
        ("taskId", json!("bash-0123456789abcdef")),
        ("ref", json!("HEAD")),
        ("output_offset", json!(0)),
        ("outputMode", json!("raw")),
        ("unknown", json!(true)),
        ("timeoutMs", json!(0)),
        ("timeoutMs", json!(20_001)),
        ("timeoutMs", json!(1.5)),
        ("timeoutMs", json!("1000")),
        ("pattern", json!("x".repeat(1025))),
        ("regex", json!("true")),
    ] {
        let mut args = base.clone();
        args[key] = value;
        assert_eq!(
            prepare("bash_watch", args).unwrap_err().code,
            "args_invalid",
            "{key}"
        );
    }
    for timeout in [1, 20_000] {
        let mut args = base.clone();
        args["timeoutMs"] = timeout.into();
        assert!(prepare("bash_watch", args).is_ok());
    }
    let operation = prepare("bash_watch", base).unwrap();
    assert_eq!(
        operation
            .translate_query(Some("bash-0123456789abcdef"), 120_000)
            .unwrap()["timeoutMs"],
        20_000
    );
}

#[test]
fn operation_requires_publication_and_non_carrier_rejects_even_null_marker() {
    for name in ["read", "bash_status"] {
        assert_eq!(
            prepare_with_manifest(name, &json!({"rich_ui_op":format!("{name}@1")}), None)
                .unwrap_err()
                .code,
            "operation_version_unsupported"
        );
    }
    assert_eq!(
        reject_non_carrier(&json!({"rich_ui_op":null}))
            .unwrap_err()
            .code,
        "rich_ui_op_not_allowed"
    );
    assert!(reject_non_carrier(&json!({"path":"x"})).is_ok());
}

#[test]
fn operation_query_translation_passes_served_schemas_without_bound_fields() {
    let status = prepare(
        "bash_status",
        json!({"rich_ui_op":"bash_status@1","task":"viewed-key"}),
    )
    .unwrap();
    assert_eq!(
        status
            .translate_query(Some("bash-0123456789abcdef"), 5000)
            .unwrap(),
        json!({"taskId":"bash-0123456789abcdef"})
    );
    for regex in [false, true] {
        let op = prepare("bash_watch", json!({"rich_ui_op":"bash_watch@1","task":"key","pattern":"needle","regex":regex,"timeoutMs":10000})).unwrap();
        let args = op
            .translate_query(Some("bash-0123456789abcdef"), 5000)
            .unwrap();
        assert_eq!(args["timeoutMs"], 5000);
        assert_eq!(
            args["pattern"],
            if regex {
                json!({"regex":"needle"})
            } else {
                json!("needle")
            }
        );
        assert!(
            args.get("task").is_none()
                && args.get("rich_ui_op").is_none()
                && args.get("regex").is_none()
        );
    }
    let read = prepare("read", json!({"rich_ui_op":"read@1","path":"x"})).unwrap();
    assert_eq!(
        read.translate_query(None, 5000).unwrap(),
        json!({"path":"x"})
    );
}

#[test]
fn session_read_union_free_arguments_pass_served_schemas() {
    for (name, args) in [
        (
            "read",
            json!({"path":"src/lib.rs", "startLine":2, "endLine":8}),
        ),
        (
            "outline",
            json!({"target":"src/lib.rs", "includeTests":true}),
        ),
        ("outline", json!({"target":"src", "files":true})),
        (
            "zoom",
            json!({"batch":[{"target":"src/lib.rs", "symbols":["one", "two"]}], "contextLines":2, "callgraph":true}),
        ),
        (
            "search",
            json!({"pattern":"one", "path":"src", "topK":3, "offset":0, "includeTests":true}),
        ),
        (
            "grep",
            json!({"pattern":"one", "path":"src", "include":"*.rs", "offset":0}),
        ),
        ("glob", json!({"pattern":"**/*.rs", "path":"src"})),
        (
            "callgraph",
            json!({"op":"trace_to_symbol", "path":"src/lib.rs", "symbol":"one", "toPath":"src/lib.rs", "toSymbol":"two", "depth":2, "includeTests":true, "includeUnresolved":true}),
        ),
    ] {
        let mut input = args.clone();
        input["rich_ui_op"] = json!(format!("{name}@1"));
        let operation = prepare(name, input).unwrap();
        // The manifest's schemas accept these argument forms directly; do not
        // coerce symbol lists to strings or wrap a target in an array.
        assert_eq!(
            operation.translate_query(None, 5000).unwrap(),
            args,
            "{name}"
        );
    }
    for args in [
        json!({"target":["src/lib.rs"]}),
        json!({"target":"src/lib.rs", "ref":"HEAD"}),
    ] {
        let mut args = args;
        args["rich_ui_op"] = json!("outline@1");
        assert_eq!(prepare("outline", args).unwrap_err().code, "args_invalid");
    }
    for batch in [
        json!([{"target":"src/lib.rs", "symbols":"one"}]),
        json!([{"target":"src/lib.rs", "symbols":["one"], "ref":"HEAD"}]),
    ] {
        assert_eq!(
            prepare("zoom", json!({"rich_ui_op":"zoom@1", "batch":batch}))
                .unwrap_err()
                .code,
            "args_invalid"
        );
    }
}
