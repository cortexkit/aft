mod tests {
    use crate::config::Config;
    use crate::edit::{
        attach_edit_diff, build_unbounded_unified_diff, read_edit_source,
        write_format_validate_observed,
    };
    #[cfg(unix)]
    use aft_engine::test_bridge::bash_remote::ToolLaunchPolicyTestExt as _;
    use aft_engine::test_bridge::search_misc::{A03_HANDLER_READS, A03_SYNTAX_PARSES};

    #[cfg(unix)]
    #[test]
    fn a03_formatter_checker_final_buffer_parity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("edge.ts");
        let bin = root.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(root.path().join(".oxfmtrc.json"), "{}").unwrap();
        std::fs::write(root.path().join("tsconfig.json"), "{}").unwrap();
        for (tool, value) in [("oxfmt", 3), ("tsc", 4)] {
            let tool_path = bin.join(tool);
            crate::executable_cache::write_executable(
                &tool_path,
                format!("#!/bin/sh\nif [ \"$1\" = '--version' ]; then echo '{tool} 1.0'; exit 0; fi\nprintf 'const value = {value};\\n' > '{}'\n", path.display()).as_bytes(),
            )
            .unwrap();
        }
        let config = Config {
            project_root: Some(root.path().to_path_buf()),
            format_on_edit: true,
            formatter: [("typescript".into(), "oxfmt".into())].into(),
            checker: [("typescript".into(), "tsc".into())].into(),
            ..Config::default()
        };
        for full in [false, true] {
            std::fs::write(&path, "const value = 1;\n").unwrap();
            let (original, observation) = read_edit_source(&path).unwrap();
            A03_HANDLER_READS.with(|count| count.set(0));
            A03_SYNTAX_PARSES.with(|count| count.set(0));
            let params = serde_json::json!({"validate": if full {"full"} else {"syntax"}, "include_diff_content": true});
            let mut result = write_format_validate_observed(
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                &path,
                "const value = 2;\n",
                &config,
                &params,
                &original,
                &observation,
            )
            .unwrap();
            let final_text = if full {
                "const value = 4;\n"
            } else {
                "const value = 3;\n"
            };
            assert!(result.formatted, "{:?}", result.format_skipped_reason);
            assert!(result.validation_errors.is_empty());
            assert_eq!(result.syntax_valid, Some(true));
            assert_eq!(
                result.reformatted_excerpt.as_ref().unwrap().text,
                final_text.trim_end()
            );
            assert_eq!(result.final_content(&path), Some(final_text));
            let mut response = serde_json::json!({});
            attach_edit_diff(
                &mut response,
                &params,
                "edge.ts",
                &original,
                result.final_content(&path).unwrap(),
            );
            assert_eq!(
                response["diff"],
                serde_json::json!({"before": original, "after": final_text, "additions": 1, "deletions": 1})
            );
            assert_eq!(response["metadata"]["diff"], format!("Index: edge.ts\n===================================================================\n--- edge.ts\n+++ edge.ts\n@@ -1 +1 @@\n-const value = 1;\n+{}", final_text));
            assert_eq!(
                A03_HANDLER_READS.with(|count| count.get()),
                if full { 2 } else { 1 }
            );
            assert_eq!(A03_SYNTAX_PARSES.with(|count| count.get()), 2);

            std::fs::write(&path, "const value = 1;\n").unwrap();
            crate::backup::reset_capture_read_count(&path);
            A03_HANDLER_READS.with(|count| count.set(0));
            A03_SYNTAX_PARSES.with(|count| count.set(0));
            let ctx = crate::test_context(Box::new(crate::language::StubProvider), config.clone());
            let req: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id": "formatter-checker", "command": "batch", "file": path,
                "edits": [{"match": "1", "replacement": "2"}],
                "validate": if full {"full"} else {"syntax"}, "include_diff_content": true
            }))
            .unwrap();
            let response = crate::commands::batch::handle_batch(&req, &ctx);
            assert!(response.success, "{:?}", response);
            assert_eq!(response.data["diff"]["after"], final_text);
            assert_eq!(
                response.data["metadata"]["diff"],
                build_unbounded_unified_diff(
                    path.to_str().unwrap(),
                    "const value = 1;\n",
                    final_text
                )
            );
            let reads = A03_HANDLER_READS.with(|count| count.get())
                + crate::backup::capture_read_count(&path);
            assert_eq!(
                reads,
                if full { 3 } else { 2 },
                "handler-wide formatter/checker content reads"
            );
            assert_eq!(A03_SYNTAX_PARSES.with(|count| count.get()), 2);
        }
    }
}
