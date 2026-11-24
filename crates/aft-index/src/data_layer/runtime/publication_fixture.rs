use super::*;

pub struct PublicationFixture {
    pub(super) handle: Arc<DataCheckout>,
    _root: tempfile::TempDir,
    _storage: tempfile::TempDir,
}

impl PublicationFixture {
    pub fn new(settle: Duration, quiet: Duration) -> Self {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(path.join("src/lib.rs"), "pub fn original() {}\n").unwrap();
        let handle = DataCheckout::open_with_periods(
            &path,
            storage.path(),
            None,
            Planes::ALL,
            quiet,
            settle,
        )
        .unwrap();
        assert!(matches!(
            handle.fresh_view(Duration::from_secs(60)),
            Fresh::Ready(_)
        ));
        wait_until(|| !handle.work.running());
        Self {
            handle,
            _root: root,
            _storage: storage,
        }
    }

    #[cfg(test)]
    pub(super) fn add(&self, stem: &str, count: usize) {
        let paths = (0..count)
            .map(|i| {
                let path = self.handle.root().join(format!("src/{stem}{i}.rs"));
                std::fs::write(&path, format!("pub fn {stem}{i}() {{}}\n")).unwrap();
                path
            })
            .collect::<Vec<_>>();
        self.handle.note_paths(&paths);
    }

    #[cfg(test)]
    pub(super) fn refresh(&self) {
        assert!(matches!(
            self.handle.fresh_view(Duration::from_secs(60)),
            Fresh::Ready(_)
        ));
        wait_until(|| !self.handle.work.refreshing.load(Ordering::Acquire));
    }

    #[cfg(test)]
    pub(super) fn publications(&self) -> usize {
        self.handle.work.publications_started.load(Ordering::SeqCst)
    }

    pub fn root(&self) -> &Path {
        self.handle.root()
    }

    #[doc(hidden)]
    pub fn checkout_handle(&self) -> Arc<DataCheckout> {
        Arc::clone(&self.handle)
    }

    #[doc(hidden)]
    pub fn storage(&self) -> &Path {
        self._storage.path()
    }

    pub fn note_paths(&self, paths: &[PathBuf]) {
        self.handle.note_paths(paths);
    }

    pub fn record_change(&self, intent: &WriteIntent, phase: WritePhase) {
        self.handle._listener.record_change(intent, phase);
    }

    pub fn pending(&self) -> bool {
        pending(self.handle.checkout())
    }

    pub fn quiet_armed(&self) -> bool {
        self.handle.quiet.armed.load(Ordering::Acquire)
    }

    pub fn wait_for_work(&self) {
        wait_until(|| !self.handle.work.running());
    }

    pub fn refreshes(&self) -> usize {
        self.handle.work.refreshes_started.load(Ordering::SeqCst)
    }
}

pub(super) fn wait_until(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "background work did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}
