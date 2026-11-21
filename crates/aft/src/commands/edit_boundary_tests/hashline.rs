#[cfg(test)]
mod diff_budget_tests {
    use crate::hashline::integration::{build_display_metadata, DisplayFileBytes};
    use serde_json::json;

    #[test]
    fn diff_budget_hashline_keeps_all_outcomes_when_a_deletion_is_omitted() {
        let display = [
            DisplayFileBytes {
                requested_path: "large.txt".into(),
                before: "x\n".repeat(100_000).into_bytes(),
                remove_file: true,
                ..Default::default()
            },
            DisplayFileBytes {
                requested_path: "small.txt".into(),
                before: b"old\n".to_vec(),
                after: Some(b"new\n".to_vec()),
                ..Default::default()
            },
        ];
        let (diff, files, budget) = build_display_metadata(&display, None);
        let mut result = json!({"hashline": true, "complete": true,
            "output": "Deleted large.txt\nUpdated small.txt",
            "metadata": {"diff": diff, "files": files}});
        budget.finish(&mut result);
        assert_eq!(result["metadata"]["files"].as_array().unwrap().len(), 2);
        assert_eq!(result["metadata"]["files"][0]["type"], "delete");
        assert_eq!(result["metadata"]["files"][0]["deletions"], 100_000);
        assert_eq!(result["metadata"]["files"][1]["type"], "update");
        assert_eq!(result["metadata"]["files"][1]["additions"], 1);
        assert!(result["metadata"].get("diff").is_none());
        assert!(result["metadata"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file.get("patch").is_none()));
        let response = crate::protocol::Response::success("hashline-budget", result);
        assert_eq!(crate::subc_format::format_response("edit", &response, false),
            "Deleted large.txt\nUpdated small.txt\nDiff preview omitted: exceeds the response budget.");
    }
}
