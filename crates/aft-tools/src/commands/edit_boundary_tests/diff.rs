#[cfg(test)]
mod diff_budget_tests {
    use crate::edit::{
        attach_edit_diff, attach_mutation_diff, attach_preview_diff, DiffBudget,
        DIFF_OMITTED_NOTICE, DIFF_RESPONSE_BUDGET,
    };
    use aft_engine::test_bridge::search_misc::{
        diff_budget_omitted, diff_budget_remaining, set_diff_budget_remaining, BoundedDiffText,
    };
    use serde_json::json;
    use std::fmt::Write;

    #[test]
    fn diff_budget_writer_stops_the_stream_before_allocating_large_chunks() {
        let mut sink = BoundedDiffText::new(16);
        let mut visited = 0;
        let rendered = (0..1_000_000).try_for_each(|_| {
            visited += 1;
            sink.write_str("12345678")
        });
        assert!(rendered.is_err());
        assert_eq!(visited, 3);
        assert_eq!(sink.text(), "1234567812345678");
        let capacity = sink.text().capacity();
        assert!(sink.write_str(&"x".repeat(1_000_000)).is_err());
        assert_eq!(sink.text().capacity(), capacity);
    }

    #[test]
    fn diff_budget_counts_all_files_and_duplicate_fields() {
        let mut budget = DiffBudget::new(2);
        let text = "x\n".repeat(15_000);
        let first = budget.render("first.txt", &text, "").unwrap();
        assert!(first.len() * 2 < DIFF_RESPONSE_BUDGET);
        assert!(budget.render("second.txt", &text, "").is_none());
        let mut result = json!({"metadata": {"diff": first, "files": [
            {"type": "delete", "patch": first, "deletions": 15_000},
            {"type": "delete", "patch": null, "deletions": 15_000}
        ]}});
        budget.finish(&mut result);
        assert!(result["metadata"].get("diff").is_none());
        assert!(result["metadata"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f.get("patch").is_none()));
        assert_eq!(result["metadata"]["files"][1]["deletions"], 15_000);
    }

    #[test]
    fn diff_budget_bounds_mutations_and_edit_write_previews_with_counts() {
        let before = "x\n".repeat(100_000);
        for mode in 0..3 {
            let mut result = json!({"ok": true});
            match mode {
                0 => attach_mutation_diff(&mut result, "large.txt", &before, ""),
                1 => attach_edit_diff(
                    &mut result,
                    &json!({"include_diff_content": true}),
                    "large.txt",
                    &before,
                    "",
                ),
                _ => attach_preview_diff(
                    &mut result,
                    &json!({"include_diff_content": true}),
                    "large.txt",
                    &before,
                    "",
                ),
            }
            assert_eq!(result["diff_preview_omitted"], true);
            assert!(result["metadata"].get("diff").is_none());
            assert!(result.get("preview_diff").is_none());
            if mode != 0 {
                assert_eq!(result["diff"]["additions"], 0);
                assert_eq!(result["diff"]["deletions"], 100_000);
                assert!(result["diff"].get("before").is_none());
            }
            let response = crate::protocol::Response::success("budget", result);
            let rendered = crate::subc_format::format_response("edit", &response, false);
            assert_eq!(rendered.lines().last(), Some(DIFF_OMITTED_NOTICE));
            assert!(serde_json::to_vec(&response).unwrap().len() < DIFF_RESPONSE_BUDGET);
        }
    }

    #[test]
    fn diff_budget_boundary_and_small_utf8_patches_preserve_exact_bytes() {
        let mut budget = DiffBudget::new(1);
        let patch = budget.render("small.txt", "old\r\n", "é\n").unwrap();
        assert_eq!(patch, "Index: small.txt\n===================================================================\n--- small.txt\n+++ small.txt\n@@ -1 +1 @@\n-old\r\n+é\n");
        let mut budget = DiffBudget::new(2);
        set_diff_budget_remaining(&mut budget, patch.len() * 2);
        assert_eq!(budget.render("small.txt", "old\r\n", "é\n").unwrap(), patch);
        assert_eq!(diff_budget_remaining(&budget), 0);
        budget.separator(&mut String::new(), "\n");
        assert!(diff_budget_omitted(&budget));
    }
}
