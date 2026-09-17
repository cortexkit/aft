use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc};

use crate::watcher_filter::SharedGitignore;

#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(target_os = "linux")]
mod inotify;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
pub(crate) use fsevents::ProjectWatcher;
#[cfg(target_os = "linux")]
pub(crate) use inotify::ProjectWatcher;
#[cfg(windows)]
pub(crate) use windows::ProjectWatcher;

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) struct ProjectWatcher {
    _watcher: notify::RecommendedWatcher,
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
impl ProjectWatcher {
    fn create(
        root: PathBuf,
        extra_watch_paths: Vec<PathBuf>,
        tx: mpsc::Sender<notify::Result<notify::Event>>,
    ) -> notify::Result<Self> {
        use notify::{RecursiveMode, Watcher};

        let mut watcher = notify::recommended_watcher(tx)?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        for path in extra_watch_paths {
            if path.exists() {
                watcher.watch(&path, RecursiveMode::NonRecursive)?;
            }
        }
        Ok(Self { _watcher: watcher })
    }
}

pub(crate) fn create_project_watcher(
    root: PathBuf,
    extra_watch_paths: Vec<PathBuf>,
    tx: mpsc::Sender<notify::Result<notify::Event>>,
    matcher: SharedGitignore,
    matcher_generation: Arc<AtomicU64>,
) -> notify::Result<ProjectWatcher> {
    #[cfg(target_os = "macos")]
    {
        return ProjectWatcher::create(root, extra_watch_paths, tx, matcher, matcher_generation);
    }
    #[cfg(target_os = "linux")]
    {
        return ProjectWatcher::create(root, extra_watch_paths, tx, matcher, matcher_generation);
    }
    #[cfg(windows)]
    {
        return ProjectWatcher::create(root, extra_watch_paths, tx, matcher, matcher_generation);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (matcher, matcher_generation);
        ProjectWatcher::create(root, extra_watch_paths, tx)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn render_exclusions(exclusions: &[crate::watcher_filter::WatcherExclusion]) -> String {
    exclusions
        .iter()
        .map(|exclusion| {
            format!(
                "{} source={}",
                exclusion.path().display(),
                exclusion.source().as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn log_exclusions(
    root: &std::path::Path,
    exclusions: &[crate::watcher_filter::WatcherExclusion],
) {
    let rendered = render_exclusions(exclusions);
    crate::slog_info!(
        "watcher exclusions for {} ({}): [{}]",
        root.display(),
        exclusions.len(),
        rendered
    );
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use ignore::gitignore::GitignoreBuilder;

    use super::*;
    use crate::watcher_filter::{derive_excluded_subtrees, WATCHER_EXCLUSION_LIMIT};

    #[test]
    fn exclusion_log_keeps_shape_and_names_each_slot_source() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::create_dir(root.path().join("generated")).unwrap();
        std::fs::write(root.path().join(".gitignore"), "target/\ngenerated/\n").unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let mut builder = GitignoreBuilder::new(&root);
        builder.add(root.join(".gitignore"));
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        let exclusions = derive_excluded_subtrees(&root, &matcher, Some(WATCHER_EXCLUSION_LIMIT));

        let rendered = format!(
            "watcher exclusions for {} ({}): [{}]",
            root.display(),
            exclusions.len(),
            render_exclusions(&exclusions)
        );

        assert_eq!(
            rendered,
            format!(
                "watcher exclusions for {} (3): [{} source=seed, {} source=seed, {} source=gitignore]",
                root.display(),
                root.join(".git").display(),
                root.join("target").display(),
                root.join("generated").display()
            )
        );
    }
}
