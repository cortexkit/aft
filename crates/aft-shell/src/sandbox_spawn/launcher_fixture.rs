use super::{observation_key, HashMap, Mutex, Path, PathBuf, TEST_LAUNCHERS};

/// Use the real aft executable for an in-process integration fixture. The test
/// harness cannot re-exec itself as `aft sandbox-launch`; it only accepts libtest
/// arguments. Only executable selection changes, not the native sandbox policy.
#[doc(hidden)]
pub fn install_sandbox_launcher_for_test(project_root: &Path, launcher_path: PathBuf) -> impl Drop {
    struct LauncherScope(PathBuf, Option<PathBuf>);

    impl Drop for LauncherScope {
        fn drop(&mut self) {
            let mut launchers = TEST_LAUNCHERS
                .get()
                .expect("sandbox launcher test map installed")
                .lock()
                .expect("sandbox launcher test mutex poisoned");
            if let Some(previous) = self.1.take() {
                launchers.insert(self.0.clone(), previous);
            } else {
                launchers.remove(&self.0);
            }
        }
    }

    let root = observation_key(project_root);
    let previous = TEST_LAUNCHERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("sandbox launcher test mutex poisoned")
        .insert(root.clone(), launcher_path);
    LauncherScope(root, previous)
}
