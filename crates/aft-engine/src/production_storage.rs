#[cfg(test)]
pub(crate) use aft_base::production_storage::CODE;
pub use aft_db::production_storage::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn production_bash_record_upgrade_is_refused_untouched() {
        use crate::bash_background::persistence::*;
        let fixture = tempfile::tempdir().unwrap();
        let layout =
            create_task_layout(fixture.path(), "session", "bash-0000000000000001").unwrap();
        let mut metadata = PersistedTask::starting(
            layout.paths.task_id.clone(),
            "session".into(),
            "true".into(),
            fixture.path().into(),
            None,
            None,
            false,
            false,
        );
        // Seed an old record directly; the production writer would upgrade it.
        metadata.schema_version = 6;
        let before = serde_json::to_vec(&metadata).unwrap();
        std::fs::write(&layout.paths.json, &before).unwrap();
        with_test_account(fixture.path(), false, || {
            let error = write_task_at(&layout, &metadata).unwrap_err();
            assert!(error.to_string().contains(CODE), "{error}");
            assert!(
                create_task_layout(fixture.path(), "session", "bash-0000000000000002").is_err()
            );
        });
        assert_eq!(std::fs::read(&layout.paths.json).unwrap(), before);
        with_test_account(fixture.path(), true, || {
            write_task_at(&layout, &metadata).unwrap()
        });
        assert_eq!(
            read_task_at(&layout).unwrap().schema_version,
            SCHEMA_VERSION
        );
    }
}
