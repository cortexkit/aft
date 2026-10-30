use super::context_storage::isolate;
use aft::config::Config;

#[test]
fn integration_context_storage_is_explicit_and_retained() {
    let first = isolate(Config::default());
    let second = isolate(Config::default());
    let path = first.storage_dir.clone().unwrap();
    assert_ne!(first.storage_dir, second.storage_dir);
    let ctx =
        aft::context::AppContext::new(Box::new(aft::parser::TreeSitterProvider::new()), first)
            .with_test_checkpoint_store();
    assert_eq!(ctx.config().storage_dir, Some(path.clone()));
    drop(ctx);
    assert!(path.is_dir());
}

#[test]
fn integration_context_storage_preserves_explicit_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let config = isolate(Config {
        storage_dir: Some(dir.path().into()),
        ..Default::default()
    });
    assert_eq!(config.storage_dir.as_deref(), Some(dir.path()));
}

#[test]
fn integration_context_checkpoints_use_private_unbound_storage() {
    let ctx = aft::context::AppContext::new(
        Box::new(aft::parser::TreeSitterProvider::new()),
        isolate(Config::default()),
    )
    .with_test_checkpoint_store();
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("source.txt");
    std::fs::write(&file, "checkpoint bytes").unwrap();
    let info = ctx
        .checkpoint()
        .lock()
        .create(
            "isolation",
            "private",
            vec![file],
            &aft::backup::BackupStore::new(),
        )
        .unwrap();
    assert!(info
        .storage_path
        .unwrap()
        .starts_with(ctx.storage_dir().join("unbound/checkpoints")));
}
