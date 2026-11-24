#[test]
#[ignore = "requires cached MiniLM and ONNX Runtime; run explicitly with ORT_DYLIB_PATH"]
fn local_embedding_root_eviction_unbind_and_census_release_sessions() {
    let _serial = native_test();
    for eviction in [0, 1, 2] {
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        *ctx.semantic_embedding_model().lock() = Some(
            crate::semantic_index::SemanticEmbeddingModel::from_config_for_query(
                &crate::config::SemanticBackendConfig::default(),
            )
            .unwrap(),
        );
        let snapshot = crate::memory_census::snapshot("ready", Default::default());
        assert_eq!(snapshot.process.embedding_sessions, 1);
        assert!(snapshot.process.embedding_model_bytes > 80_000_000);
        assert!(snapshot.process.total_attributed_bytes >= snapshot.process.embedding_model_bytes);
        match eviction {
            0 => assert!(ctx.evict_idle_artifacts()),
            1 => assert!(ctx.try_evict_idle_artifacts()),
            _ => ctx.cancel_unbound_artifact_work(),
        }
        assert!(ctx.semantic_embedding_model().lock().is_none());
        wait_for_sessions(0);
        let snapshot = crate::memory_census::snapshot("ready", Default::default());
        assert_eq!(snapshot.process.embedding_sessions, 0);
        assert_eq!(snapshot.process.embedding_model_bytes, 0);
    }
}
use crate::local_embed::test_helpers::{native_test, wait_for_sessions};
