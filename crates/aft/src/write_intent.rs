pub use aft_base::write_intent::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::OnceLock;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Weak;

#[cfg(test)]
#[allow(hidden_glob_reexports)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    pub(crate) use aft_base::write_intent::tests::*;

    #[test]
    fn composite_listener_sees_before_after_directory_and_unregisters_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.txt");
        std::fs::write(&path, b"old").unwrap();
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let listener: Arc<dyn WriteIntentListener> = Arc::new(Listener {
            root: directory.path().to_path_buf(),
            events: events.clone(),
        });
        register_listener(&listener);
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            "new",
            &crate::config::Config::default(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                (
                    WriteIntent::Paths(vec![path.clone()]),
                    WritePhase::Before,
                    b"old".to_vec()
                ),
                (
                    WriteIntent::Paths(vec![path.clone()]),
                    WritePhase::After,
                    b"new".to_vec()
                ),
            ]
        );
        events.lock().unwrap().clear();
        let tree = directory.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        let intent = record_paths([tree.as_path()]);
        std::fs::remove_dir(&tree).unwrap();
        drop(intent);
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                (
                    WriteIntent::Directory(tree.clone()),
                    WritePhase::Before,
                    Vec::new()
                ),
                (WriteIntent::Directory(tree), WritePhase::After, Vec::new()),
            ]
        );
        events.lock().unwrap().clear();
        let guard = record_paths([path.as_path()]);
        assert_eq!(events.lock().unwrap().len(), 1);
        drop(listener);
        drop(guard);
        crate::edit::write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            "again",
            &crate::config::Config::default(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "a weak registration must not keep the listener alive"
        );
    }
}
