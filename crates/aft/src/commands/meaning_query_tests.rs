mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    use crate::config::{SemanticBackend, SemanticBackendConfig};
    use crate::context::AppContext;
    use crate::data_layer::meaning_query::DataSemantic;
    use crate::protocol::RawRequest;
    use aft_engine::test_bridge::display_data::meaning_query::serve;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(found) = check() {
                return found;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn search(ctx: &AppContext, id: &str, query: &str) -> Value {
        let request: RawRequest = serde_json::from_value(json!({
            "id": id,
            "command": "semantic_search",
            "query": query,
            "top_k": 5,
        }))
        .unwrap();
        serde_json::to_value(crate::commands::semantic_search::handle_semantic_search(
            &request, ctx,
        ))
        .unwrap()
    }

    /// While a fill is under way the plane serves what is filled and every
    /// surface says the answer is partial; it reports ready only once every
    /// file is filled, and the filled answer is cached under another
    /// generation than the partial one.
    #[test]
    fn a_fill_under_way_is_reported_partial_and_ready_only_once_every_file_is_filled() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // More files than one fill round takes, the last of them held back
        // by the embedder, so the first round lands and the second waits.
        for index in 0..300 {
            write(
                &root,
                &format!("src/f{index:03}.rs"),
                &format!("/// Counts the widgets of batch {index}.\npub fn widget_count_{index}(items: &[u32]) -> usize {{\n    items.len() + {index}\n}}\n"),
            );
        }
        write(
            &root,
            "src/zz_last.rs",
            "/// The last file.\npub fn omega_marker() -> u32 {\n    7\n}\n",
        );
        let (base_url, gate) = serve("omega_marker");
        let config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            indexes: crate::config::IndexesConfig {
                trigram: true,
                semantic: true,
                callgraph: false,
            },
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: "test-hashed".to_string(),
                base_url: Some(base_url),
                timeout_ms: 120_000,
                query_timeout_ms: 30_000,
                ..SemanticBackendConfig::default()
            },
            ..crate::config::Config::default()
        };
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.clone());
        crate::semantic_admission::note_opened("data-layer-meaning-test", &root);
        let checkout = ctx.data_checkout().unwrap();
        wait_for("the text build", || checkout.view());

        // Asking starts the fill; its first round lands, the second waits.
        let partial = wait_for("the first fill round", || {
            DataSemantic::current(&ctx).and_then(|data| data.serving().cloned())
        });
        assert!(!partial.complete());
        assert_eq!(partial.coverage().files, 301);
        assert_eq!(
            partial.coverage().pending.len(),
            45,
            "{:?}",
            partial.coverage()
        );
        wait_for("the held request", || (gate.held() > 0).then_some(()));

        let status = ctx.build_status_snapshot();
        assert_eq!(status["semantic_index"]["status"], "building", "{status}");
        assert_eq!(status["semantic_index"]["files_pending"], 45, "{status}");
        // `aft warmup` keeps waiting while files wait for vectors.
        assert_eq!(
            crate::data_layer::surfaces::warm_meaning(&ctx),
            Some(crate::data_layer::surfaces::WarmState::Pending(
                "filling, files=256/301".to_string()
            ))
        );

        let reply = search(&ctx, "meaning-partial", "count the widgets of a batch");
        assert_eq!(reply["success"], true, "{reply}");
        assert_eq!(reply["status"], "partial", "{reply}");
        assert_eq!(reply["complete"], false, "{reply}");
        let text = reply["text"].as_str().unwrap();
        assert!(
            text.contains("semantic: 45 file(s) not yet embedded"),
            "{text}"
        );
        assert!(text.contains("semantic: partial"), "{text}");
        assert!(!reply["results"].as_array().unwrap().is_empty(), "{reply}");
        let partial_generation = reply["structuredContent"]["plan"]["snapshot_generation"].clone();

        gate.open();
        wait_for("the fill to finish", || {
            DataSemantic::current(&ctx)
                .and_then(|data| data.serving().cloned())
                .filter(|meaning| meaning.complete())
        });
        let status = ctx.build_status_snapshot();
        assert_eq!(status["semantic_index"]["status"], "ready", "{status}");
        assert_eq!(status["semantic_index"]["files_embedded"], 301, "{status}");
        assert_eq!(
            crate::data_layer::surfaces::warm_meaning(&ctx),
            Some(crate::data_layer::surfaces::WarmState::Ready)
        );

        let reply = search(&ctx, "meaning-ready", "count the widgets of a batch");
        assert_eq!(reply["success"], true, "{reply}");
        assert_eq!(reply["status"], "ready", "{reply}");
        assert_eq!(reply["complete"], true, "{reply}");
        let text = reply["text"].as_str().unwrap();
        assert!(!text.contains("not yet embedded"), "{text}");
        assert_ne!(
            reply["structuredContent"]["plan"]["snapshot_generation"], partial_generation,
            "a fill that lands changes the generation cached lists are keyed by"
        );
    }

    /// After an idle release drops the checkout, the next search binds it
    /// again from its stored publication and reports meaning search as not
    /// ready, with the lexical fallback, while the plane is read back (its
    /// model loaded and asked for its vector length); once it is back,
    /// searches are ready again and nothing was embedded again.
    #[test]
    fn a_search_after_an_idle_release_reports_the_plane_loading_until_it_is_back() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write(
            &root,
            "src/config.rs",
            "/// Reads a configuration file from disk.\npub fn read_config(path: &str) -> String {\n    std::fs::read_to_string(path).unwrap_or_default()\n}\n",
        );
        let (base_url, gate) = serve("never-held");
        gate.open();
        let config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            indexes: crate::config::IndexesConfig {
                trigram: true,
                semantic: true,
                callgraph: false,
            },
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: "test-hashed".to_string(),
                base_url: Some(base_url),
                timeout_ms: 120_000,
                query_timeout_ms: 30_000,
                ..SemanticBackendConfig::default()
            },
            ..crate::config::Config::default()
        };
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.clone());
        crate::semantic_admission::note_opened("data-layer-meaning-release-test", &root);
        wait_for("the first fill", || {
            DataSemantic::current(&ctx)
                .and_then(|data| data.serving().cloned())
                .filter(|meaning| meaning.complete())
        });
        let embedded = gate.texts_embedded();
        assert!(embedded > 0);

        let held = crate::data_layer::meaning::hold_model_loads_for_test(&root);
        assert!(
            ctx.try_release_idle_checkout_views(),
            "the root must be idle"
        );
        let reply = search(
            &ctx,
            "meaning-after-release",
            "read a configuration file from disk",
        );
        eprintln!("reply while the plane is read back: {}", reply["text"]);
        assert_eq!(reply["success"], true, "{reply}");
        assert_eq!(reply["semantic_status"], "building", "{reply}");
        assert_eq!(reply["complete"], false, "{reply}");
        assert_eq!(reply["lexical_only_fallback"], true, "{reply}");
        assert!(
            reply["note"].as_str().is_some_and(|note| !note.is_empty()),
            "the reply must disclose the lexical fallback: {reply}"
        );
        let text = reply["text"].as_str().unwrap_or_default();
        assert!(
            !text.lines().any(|line| line == "semantic: ready"),
            "the reply must not claim a ready semantic lane: {text}"
        );

        drop(held);
        wait_for("the plane to be back", || {
            DataSemantic::current(&ctx)
                .and_then(|data| data.serving().cloned())
                .filter(|meaning| meaning.complete())
        });
        assert_eq!(
            gate.texts_embedded(),
            embedded,
            "reading the plane back embedded files again"
        );
        let reply = search(
            &ctx,
            "meaning-after-reload",
            "read a configuration file from disk",
        );
        assert_eq!(reply["semantic_status"], "ready", "{reply}");
        assert_eq!(reply["complete"], true, "{reply}");
    }

    /// `aft index` fills a checkout's meaning plane in the foreground: every
    /// file gets its vectors before it returns, and a second fill of the same
    /// files embeds nothing, since the plane reuses what it stored.
    #[test]
    fn a_foreground_fill_embeds_every_file_and_a_second_one_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // More files than one fill round takes, so the fill runs rounds
        // until none waits.
        for index in 0..300 {
            write(
                &root,
                &format!("src/f{index:03}.rs"),
                &format!("/// Counts the widgets of batch {index}.\npub fn widget_count_{index}(items: &[u32]) -> usize {{\n    items.len() + {index}\n}}\n"),
            );
        }
        let (base_url, gate) = serve("never-held");
        gate.open();
        let config = SemanticBackendConfig {
            backend: SemanticBackend::OpenAiCompatible,
            model: "test-hashed".to_string(),
            base_url: Some(base_url),
            timeout_ms: 120_000,
            query_timeout_ms: 30_000,
            ..SemanticBackendConfig::default()
        };

        let filled = crate::data_layer::surfaces::fill_root(&root, storage.path(), &config)
            .expect("the fill");
        assert_eq!(
            (filled.files, filled.files_embedded, filled.files_past_cap),
            (300, 300, 0),
            "{filled:?}"
        );
        assert!(filled.chunks >= 300, "{filled:?}");
        let embedded = gate.texts_embedded();
        assert!(embedded >= 300, "{embedded} texts embedded");

        let again = crate::data_layer::surfaces::fill_root(&root, storage.path(), &config)
            .expect("the second fill");
        assert_eq!(again, filled);
        assert_eq!(
            gate.texts_embedded(),
            embedded,
            "the second fill embedded files again"
        );
    }
}
