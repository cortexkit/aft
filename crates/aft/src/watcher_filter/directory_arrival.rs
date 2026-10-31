//! Recover existing children of a directory introduced by one native event.

use super::*;

const ENTRY_LIMIT: usize = 1024;
const TIME_LIMIT: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub(super) struct Failure {
    pub(super) reason: &'static str,
    pub(super) entries: usize,
    path: PathBuf,
}

impl Failure {
    pub(super) fn report(&self, config: &WatcherFilterConfig) {
        config.counters.note_directory_walk_fallback();
        crate::slog_info!(
            "watcher subtree fallback root={:?} path={:?} reason={} entries={}",
            config.project_root,
            self.path,
            self.reason,
            self.entries
        );
    }
}

struct Budget {
    limit: usize,
    deadline: Instant,
    entries: usize,
}

impl Budget {
    fn check(&self) -> Result<(), &'static str> {
        if self.entries >= self.limit {
            Err("entry_limit")
        } else if Instant::now() >= self.deadline {
            Err("time_limit")
        } else {
            Ok(())
        }
    }

    fn next<I: Iterator>(&mut self, iterator: &mut I) -> Result<Option<I::Item>, &'static str> {
        self.check()?;
        let entry = iterator.next();
        self.entries += usize::from(entry.is_some());
        Ok(entry)
    }
}

pub(super) fn expand(
    config: &WatcherFilterConfig,
    matcher: &SharedGitignore,
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>, Failure> {
    expand_with_limits(
        config,
        matcher,
        paths,
        ENTRY_LIMIT,
        Instant::now() + TIME_LIMIT,
    )
}

fn expand_with_limits(
    config: &WatcherFilterConfig,
    matcher: &SharedGitignore,
    paths: &[PathBuf],
    limit: usize,
    deadline: Instant,
) -> Result<Vec<PathBuf>, Failure> {
    let mut budget = Budget {
        limit,
        deadline,
        entries: 0,
    };
    let mut files = Vec::new();
    for path in paths {
        // Use the same root-relative rules as ordinary watcher paths before
        // opening a directory. The boundary also rejects symlinked parents.
        if path
            .strip_prefix(&config.project_root)
            .is_ok_and(watcher_path_is_high_churn_infra)
        {
            continue;
        }
        let path = canonicalize_watcher_path(path.clone());
        if !path.starts_with(&config.project_root)
            || watcher_directory_is_ignored_by_matcher(matcher, &path, &config.project_root)
        {
            continue;
        }
        config.counters.note_directory_arrival();
        let result = scan(config, matcher, &path, &mut budget, &mut files);
        if let Err(reason) = result {
            return Err(Failure {
                reason,
                entries: budget.entries,
                path,
            });
        }
    }
    Ok(files)
}

fn scan(
    config: &WatcherFilterConfig,
    matcher: &SharedGitignore,
    arrival: &Path,
    budget: &mut Budget,
    files: &mut Vec<PathBuf>,
) -> Result<(), &'static str> {
    let boundary = WatcherWalkBoundary::for_root(&config.project_root).ok_or("root_unavailable")?;
    let mut directories = VecDeque::from([arrival.to_path_buf()]);
    let mut initial = true;
    while let Some(directory) = directories.pop_front() {
        budget.check()?;
        match boundary.classify(&directory).0 {
            ExclusionPathKind::External => continue,
            ExclusionPathKind::LocalDirectory => {}
            _ => return Err("directory_unavailable"),
        }
        let mut entries = exclusion_read_dir(&directory).map_err(|_| "read_error")?;
        // An empty arrival needs only this one-entry probe. Do not start a
        // subtree walk: any later file creation has its own native event.
        let mut entry = budget.next(&mut entries)?;
        if initial {
            initial = false;
            if entry.is_none() {
                config.counters.note_empty_directory();
                return Ok(());
            }
            config.counters.note_directory_walk();
        }
        while let Some(found) = entry {
            let found = found.map_err(|_| "read_error")?;
            let kind = found.file_type().map_err(|_| "read_error")?;
            let path = found.path();
            if kind.is_dir() && !kind.is_symlink() {
                if !watcher_directory_is_ignored_by_matcher(matcher, &path, &config.project_root) {
                    directories.push_back(path);
                }
            } else if kind.is_file()
                && !kind.is_symlink()
                && !watcher_path_is_ignored_by_matcher(matcher, &path, &config.project_root)
            {
                files.push(path);
            }
            // Check before advancing even for ignored entries and directories;
            // counting only the files kept would leave wide trees unbounded.
            entry = budget.next(&mut entries)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, WatcherFilterConfig, SharedGitignore) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = WatcherFilterConfig::new(root.clone(), None);
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&root);
        builder.add_line(None, "ignored/").unwrap();
        builder.add_line(None, "*.skip").unwrap();
        let matcher = Arc::new(RwLock::new(Some(Arc::new(builder.build().unwrap()))));
        (temp, config, matcher)
    }

    #[test]
    fn directory_arrival_empty_probe_does_not_start_a_walk() {
        let (_temp, config, matcher) = fixture();
        let path = config.project_root.join("empty");
        fs::create_dir(&path).unwrap();
        assert!(expand(&config, &matcher, &[path]).unwrap().is_empty());
        let counts = config.counters.snapshot();
        assert_eq!(counts.directory_arrivals_total, 1);
        assert_eq!(counts.directory_empty_total, 1);
        assert_eq!(counts.directory_subtree_walks_total, 0);
        assert_eq!(counts.directory_walk_fallbacks_total, 0);
    }

    #[test]
    fn directory_arrival_scans_only_its_subtree_and_prunes_ignored_and_infra() {
        let (_temp, config, matcher) = fixture();
        let path = config.project_root.join("incoming");
        for name in ["src", "ignored", "target"] {
            fs::create_dir_all(path.join(name)).unwrap();
            fs::write(path.join(name).join("file.rs"), b"source").unwrap();
        }
        fs::write(config.project_root.join("sibling.rs"), b"sibling").unwrap();
        super::super::EXCLUSION_TEST_OPENS.with(|opens| opens.borrow_mut().clear());
        let files = expand(&config, &matcher, &[path.clone()]).unwrap();
        assert_eq!(files, vec![path.join("src/file.rs")]);
        // Filtering the returned files is not enough: excluded subtrees must
        // never be opened, or they could consume the whole discovery budget.
        let opened = super::super::EXCLUSION_TEST_OPENS.with(|opens| opens.borrow().clone());
        assert_eq!(opened, vec![path.clone(), path.join("src")]);
        assert_eq!(config.counters.snapshot().directory_subtree_walks_total, 1);
    }

    #[test]
    fn directory_arrival_limit_counts_ignored_entries_before_advancing() {
        let (_temp, config, matcher) = fixture();
        let path = config.project_root.join("incoming");
        fs::create_dir(&path).unwrap();
        for i in 0..8 {
            fs::write(path.join(format!("{i}.skip")), b"ignored").unwrap();
        }
        let error = expand_with_limits(
            &config,
            &matcher,
            &[path],
            3,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error.reason, "entry_limit");
        assert_eq!(error.entries, 3);
        error.report(&config);
        assert_eq!(config.counters.snapshot().directory_walk_fallbacks_total, 1);
    }

    #[test]
    fn directory_arrival_expired_budget_does_not_advance_the_iterator() {
        for (limit, deadline, reason) in [
            (0, Instant::now() + Duration::from_secs(5), "entry_limit"),
            (8, Instant::now() - Duration::from_secs(1), "time_limit"),
        ] {
            let advances = std::cell::Cell::new(0);
            let mut entries = (0..10).inspect(|_| advances.set(advances.get() + 1));
            let mut budget = Budget {
                limit,
                deadline,
                entries: 0,
            };
            assert_eq!(budget.next(&mut entries).unwrap_err(), reason);
            assert_eq!(advances.get(), 0);
        }
    }

    #[test]
    fn directory_arrival_failure_is_not_a_successful_partial_listing() {
        let (_temp, config, matcher) = fixture();
        let error = expand(&config, &matcher, &[config.project_root.join("missing")]).unwrap_err();
        assert_eq!(error.reason, "directory_unavailable");
        error.report(&config);
        assert_eq!(config.counters.snapshot().directory_walk_fallbacks_total, 1);
    }

    #[cfg(unix)]
    #[test]
    fn directory_arrival_never_follows_a_child_symlink() {
        let (_temp, config, matcher) = fixture();
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("trap.rs"), b"external").unwrap();
        let path = config.project_root.join("incoming");
        fs::create_dir(&path).unwrap();
        std::os::unix::fs::symlink(external.path(), path.join("linked")).unwrap();
        assert!(expand(&config, &matcher, &[path]).unwrap().is_empty());
    }
}
