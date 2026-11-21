/// Whole searches, counting every read of file content per path.
#[cfg(test)]
mod search_read_tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::commands::semantic_search::handle_semantic_search;
    use crate::config::Config;
    use crate::context::{AppContext, SemanticIndexStatus};
    use crate::parser::TreeSitterProvider;
    use crate::protocol::RawRequest;
    use aft_engine::test_bridge::search_misc::{without_request_files_for_test, ReadProbe};

    const FILES: &[(&str, &str)] = &[
        (
            "src/cache_store.rs",
            "/// Keeps prepared text by digest.\npub fn cache_sharing_marker() -> usize {\n    // the cache sharing marker keeps prepared text\n    42\n}\n",
        ),
        (
            "src/reader.rs",
            "use crate::cache_store::cache_sharing_marker;\n\npub fn read_files() -> usize {\n    // cache sharing happens while reading files\n    cache_sharing_marker() + 1\n}\n",
        ),
        (
            "src/writer.rs",
            "pub fn write_files() {\n    // writers never use the shared cache marker\n}\n",
        ),
        (
            "docs/cache.md",
            "# Cache sharing\n\nThe cache sharing marker is read once per request.\n",
        ),
        ("data/sample.json", "{\"cache\": \"sharing\", \"marker\": 1}\n"),
    ];

    fn project() -> (tempfile::TempDir, PathBuf, AppContext) {
        let project = tempfile::tempdir().expect("project");
        let root = std::fs::canonicalize(project.path()).expect("canonical root");
        for (relative, text) in FILES {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, text).expect("write fixture");
        }
        let ctx = crate::test_context(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.clone()),
                ..Config::default()
            },
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;
        crate::data_layer::surfaces::serve_text_index_for_test(&ctx);
        (project, root, ctx)
    }

    /// Reads per path, under `root`, of one search with `params`.
    fn reads_of_one_search(
        ctx: &AppContext,
        root: &Path,
        params: &serde_json::Value,
    ) -> BTreeMap<PathBuf, usize> {
        let mut request = serde_json::json!({
            "id": "request-files-read-count",
            "command": "semantic_search",
            "top_k": 10,
        });
        for (key, value) in params.as_object().expect("params object") {
            request[key] = value.clone();
        }
        let request: RawRequest = serde_json::from_value(request).expect("request");
        let probe = ReadProbe::new(root);
        let response = serde_json::to_value(handle_semantic_search(&request, ctx)).unwrap();
        assert_eq!(response["success"], true, "{params}: {response}");
        assert!(
            response["results"]
                .as_array()
                .is_some_and(|results| !results.is_empty()),
            "{params} must find results: {response}"
        );
        probe.reads()
    }

    fn searches() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"query": "where is the cache sharing marker kept"}),
            serde_json::json!({"query": "cache_sharing_marker"}),
            serde_json::json!({"query": "cache_sharing_marker()"}),
            serde_json::json!({"query": "how does cache_sharing_marker read files"}),
            serde_json::json!({"pattern": "cache_sharing_marker"}),
            serde_json::json!({"query": "cache sharing", "pattern": "marker"}),
        ]
    }

    #[test]
    fn one_search_reads_each_candidate_file_at_most_once() {
        let (_project, root, ctx) = project();
        for params in searches() {
            // The second search also exercises digest checks for cached exact
            // matches and the other caches shared across requests.
            for _ in 0..2 {
                let reads = reads_of_one_search(&ctx, &root, &params);
                assert!(!reads.is_empty(), "{params} read no file");
                let repeated = reads
                    .iter()
                    .filter(|(_, count)| **count > 1)
                    .collect::<Vec<_>>();
                assert!(
                    repeated.is_empty(),
                    "{params} read files more than once: {repeated:?}"
                );
            }
        }
    }

    /// The probe sees the reads the set saves: without the set, each phase
    /// reads the file again, as every search did before the set existed.
    #[test]
    fn without_the_set_phases_read_the_same_file_again() {
        let (_project, root, ctx) = project();
        let most = without_request_files_for_test(|| {
            searches()
                .iter()
                .flat_map(|params| reads_of_one_search(&ctx, &root, params).into_values())
                .max()
                .unwrap_or_default()
        });
        assert!(
            most > 1,
            "some search must read a file twice without the set"
        );
    }
}
