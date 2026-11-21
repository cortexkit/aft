use serde_json::{Map, Value};

/// Select overrides that the tool or harness consumes per session or call.
/// Shared daemon root setup and standalone transport settings belong only at
/// the top level. Clone from the editor schema so both spellings type-check the
/// same values.
pub(super) fn fields(base: &Map<String, Value>) -> Map<String, Value> {
    let mut result = Map::new();
    for name in [
        "edit_mode",
        "format_on_edit",
        "revision_reads",
        "formatter_timeout_secs",
        "type_checker_timeout_secs",
        "validate_on_edit",
        "formatter",
        "checker",
        "configure_warnings_delivery",
        "disabled_tools",
        "restrict_to_project_root",
        "inspect",
        "git",
        "pi",
        "bash",
        "shell",
        "url_fetch_allow_private",
        "search",
    ] {
        result.insert(name.into(), base[name].clone());
    }
    for (name, leaves) in [
        ("github", &["read", "write"][..]),
        ("sandbox", &["read_deny", "write_allow"][..]),
        ("lsp", &["diagnostics_on_edit"][..]),
        ("semantic", &["query_timeout_ms", "query_instruction"][..]),
    ] {
        let mut section = base[name].clone();
        section["properties"]
            .as_object_mut()
            .expect("harness section properties")
            .retain(|key, _| leaves.contains(&key.as_str()));
        result.insert(name.into(), section);
    }
    result["bash"]["properties"]
        .as_object_mut()
        .expect("bash override fields")
        .retain(|key, _| {
            !matches!(
                key.as_str(),
                "long_running_reminder_enabled"
                    | "long_running_reminder_interval_ms"
                    | "powershell_tool"
            )
        });
    result
}
