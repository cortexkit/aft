use cortexkit_role_tool_provider::catalog::ToolVersion;
use cortexkit_role_tool_provider_conformance::{MinorVersionProbe, ToolVersionFixture};
use serde_json::{json, Value};

pub(super) const CASES: [&str; 5] = [
    "tool_versions_older_minor_accepted",
    "tool_versions_newer_minor_argument_refused",
    "tool_versions_different_major_refused",
    "tool_versions_malformed_pin_refused",
    "tool_versions_shipped_tp1_admitted",
];

pub(super) fn fixture() -> ToolVersionFixture {
    let schemas: Value =
        serde_json::from_str(include_str!("../../src/subc_tool_schemas.json")).unwrap();
    let mut older = schemas["read"].clone();
    let properties = older["properties"].as_object_mut().unwrap();
    let path = properties.remove("path").unwrap();
    properties.insert("filePath".into(), path);
    older["required"] = json!(["filePath"]);
    let mut newer = schemas["read"].clone();
    newer["properties"]["futureArgument"] = json!({"type":"boolean"});
    ToolVersionFixture {
        tool: "read".into(),
        older: MinorVersionProbe {
            version: ToolVersion::new(1, 0, 0),
            input_schema: older,
            arguments: json!({"filePath":".cortexkit/aft.jsonc"}),
        },
        newer: MinorVersionProbe {
            version: ToolVersion::new(1, 2, 0),
            input_schema: newer,
            arguments: json!({"path":".cortexkit/aft.jsonc", "futureArgument":true}),
        },
        shipped_tp1: "tp1:read:ebd897cd5eb17d7ab3eaa037fafc9798cee370be1ea1309275d36cf99cca3b98:1"
            .into(),
        argument_refusal_code: "invalid_request".into(),
    }
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn versioned_catalog_passes_all_five_version_cases() {
    use cortexkit_role_tool_provider_conformance::{run_suite, CaseOutcome};

    let mut subject = super::Subject::new(Some("reader"));
    subject.versioned = true;
    let work = tempfile::tempdir().unwrap();
    let report = run_suite(&subject, &work.path().join("run")).await.unwrap();
    for case in CASES {
        assert_eq!(report.outcome(case), Some(&CaseOutcome::Passed), "{case}");
        eprintln!("{case}: PASS");
    }
}
