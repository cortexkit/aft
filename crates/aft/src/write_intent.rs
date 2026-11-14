pub use aft_base::write_intent::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(test)]
mod tests {
    use super::*;

    type Events = Arc<Mutex<Vec<(WriteIntent, WritePhase, Vec<u8>)>>>;
    struct Listener {
        root: PathBuf,
        events: Events,
    }
    impl WriteIntentListener for Listener {
        fn record_change(&self, change: &WriteIntent, phase: WritePhase) {
            let paths = match change {
                WriteIntent::Paths(paths) => paths.clone(),
                WriteIntent::Directory(path) => vec![path.clone()],
            };
            if paths.iter().any(|path| path.starts_with(&self.root)) {
                let bytes = paths
                    .first()
                    .and_then(|path| std::fs::read(path).ok())
                    .unwrap_or_default();
                self.events
                    .lock()
                    .unwrap()
                    .push((change.clone(), phase, bytes));
            }
        }
    }

    #[test]
    fn intent_paths_are_classified_once() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("new.rs");
        let listener: Arc<dyn WriteIntentListener> = Arc::new(Listener {
            root: root.path().to_path_buf(),
            events: Arc::new(Mutex::new(Vec::new())),
        });
        register_listener(&listener);
        CLASSIFICATION_STATS.with(|count| count.set(0));
        let guard = record_paths([path.as_path()]);
        assert_eq!(CLASSIFICATION_STATS.with(|count| count.get()), 1);
        drop(guard);
    }

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
