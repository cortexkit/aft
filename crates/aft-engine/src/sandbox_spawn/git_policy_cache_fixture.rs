use super::*;

pub struct Fixture {
    _dir: tempfile::TempDir,
    pub(super) root: PathBuf,
    roots: Mutex<HashSet<PathBuf>>,
}

impl Fixture {
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let environment = vec![
            (OsString::from("HOME"), root.clone().into_os_string()),
            (
                OsString::from("XDG_CONFIG_HOME"),
                root.join("xdg").into_os_string(),
            ),
            (
                OsString::from("GIT_CONFIG_GLOBAL"),
                root.join("global").into_os_string(),
            ),
            (
                OsString::from("GIT_CONFIG_SYSTEM"),
                root.join("system").into_os_string(),
            ),
        ];
        git_policy_test_states()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                root.clone(),
                GitPolicyTestState {
                    environment,
                    ..Default::default()
                },
            );
        let output = git_policy_output(&root, &["init", "--quiet"]).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fixture = Self {
            _dir: dir,
            roots: Mutex::new(HashSet::from([root.clone()])),
            root,
        };
        fixture.hooks("first-hooks");
        fixture.state(|state| state.calls = 0);
        fixture
    }

    pub fn hooks(&self, path: &str) {
        std::fs::create_dir_all(self.root.join(path)).unwrap();
        std::fs::write(
            self.root.join(".git/config"),
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\thooksPath = {path}\n"
            ),
        )
        .unwrap();
    }

    pub(super) fn resolve(&self) -> (GitPolicy, usize) {
        self.resolve_at(&self.root)
    }

    pub(super) fn resolve_at(&self, root: &Path) -> (GitPolicy, usize) {
        self.register_root(root);
        let before = with_git_policy_test_state(root, |state| state.calls).unwrap();
        let policy = with_worker_jail(true, || resolve_git_policy(root)).unwrap();
        (
            policy,
            with_git_policy_test_state(root, |state| state.calls).unwrap() - before,
        )
    }

    pub(super) fn state<R>(&self, run: impl FnOnce(&mut GitPolicyTestState) -> R) -> R {
        with_git_policy_test_state(&self.root, run).unwrap()
    }

    pub fn register_root(&self, root: &Path) {
        let root = root.canonicalize().unwrap();
        let environment = self.state(|state| state.environment.clone());
        git_policy_test_states()
            .lock()
            .unwrap()
            .entry(root.clone())
            .or_insert_with(|| GitPolicyTestState {
                environment,
                ..Default::default()
            });
        self.roots.lock().unwrap().insert(root);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for root in self.roots.lock().unwrap().iter() {
            let environment = git_policy_environment(root);
            for worker in [false, true] {
                if let Some(key) = git_policy_cache_key(root, &environment, worker) {
                    git_policy_cache().lock().unwrap().remove(&key);
                }
            }
            git_policy_test_states().lock().unwrap().remove(root);
        }
    }
}

pub fn commit(root: &Path) {
    let commit = git_policy_output(
        root,
        &[
            "-c",
            "user.name=Cache Test",
            "-c",
            "user.email=cache@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "cache fixture",
        ],
    )
    .unwrap();
    assert!(commit.status.success(), "{commit:?}");
}

pub fn git_ok(root: &Path, args: &[&str]) {
    let output = git_policy_output(root, args).unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}
