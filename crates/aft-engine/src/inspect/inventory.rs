//! A request-local census shared by freshness checks, Tier-2 jobs and manifest hashing.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::entry_points;
use super::job::canonicalize_normalized;

#[derive(Debug)]
pub(crate) struct InspectInventory {
    root: PathBuf,
    pub(crate) sources: Vec<PathBuf>,
    pub(crate) manifests: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A context ready to start the status bar's background TODO refresh,
    /// with the refresh due now.
    fn todo_refresh_context(root: &Path) -> Arc<crate::context::AppContext> {
        let ctx = Arc::new(crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                ..crate::config::Config::default()
            },
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        let configured_at = std::time::Instant::now()
            .checked_sub(
                crate::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY
                    + std::time::Duration::from_secs(1),
            )
            .unwrap();
        ctx.reset_tier2_refresh_scheduler_at(configured_at);
        ctx
    }

    /// Listing the files can walk the whole repository, so it must not hold
    /// the root lifecycle lock: an unbind during the listing finishes at once,
    /// and the listed work is then not enqueued for the unbound root.
    #[test]
    fn background_todo_refresh_lists_files_outside_the_lifecycle_lock() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let temp = tempfile::tempdir().unwrap();
        let root = canonicalize_normalized(temp.path());
        write(&root, "src/lib.rs", "// TODO: listed\n");

        // Control: with nothing unbinding the root, the refresh lists the
        // files and enqueues a job whose completion arrives.
        let control = todo_refresh_context(&root);
        let control_listed = Arc::new(AtomicBool::new(false));
        let hook_listed = Arc::clone(&control_listed);
        BACKGROUND_JOB_LISTING_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || hook_listed.store(true, Ordering::SeqCst)));
        });
        assert!(control
            .tick_tier2_refresh_scheduler_at(std::time::Instant::now(), 0)
            .is_some());
        BACKGROUND_JOB_LISTING_HOOK.with(|hook| hook.borrow_mut().take());
        assert!(
            control_listed.load(Ordering::SeqCst),
            "the control never listed files"
        );
        let todos = crate::inspect_category::InspectCategory::Todos;
        let absent = crate::inspect::InspectBuilderState::Absent;
        let enqueued = |ctx: &crate::context::AppContext| {
            ctx.inspect_manager().has_pending_completions()
                || ctx.inspect_manager().tier2_builder_state(todos) != absent
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !enqueued(&control) {
            assert!(
                std::time::Instant::now() < deadline,
                "the control job never ran"
            );
            std::thread::yield_now();
        }

        let ctx = todo_refresh_context(&root);
        let listed = Arc::new(AtomicBool::new(false));
        let unbound_while_listing = Arc::new(AtomicBool::new(false));
        let (hook_ctx, hook_listed, hook_unbound) = (
            Arc::clone(&ctx),
            Arc::clone(&listed),
            Arc::clone(&unbound_while_listing),
        );
        BACKGROUND_JOB_LISTING_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                hook_listed.store(true, Ordering::SeqCst);
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    hook_ctx.mark_subc_unbound();
                    let _ = done_tx.send(());
                });
                // The bound only stops a broken build from hanging: an unbind
                // that waits for this listing never finishes inside it.
                let finished = done_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .is_ok();
                hook_unbound.store(finished, Ordering::SeqCst);
            }));
        });
        ctx.tick_tier2_refresh_scheduler_at(std::time::Instant::now(), 0);
        BACKGROUND_JOB_LISTING_HOOK.with(|hook| hook.borrow_mut().take());
        assert!(
            listed.load(Ordering::SeqCst),
            "the refresh never listed files"
        );
        assert!(
            unbound_while_listing.load(Ordering::SeqCst),
            "unbinding the root waited for the file listing"
        );
        assert!(
            !enqueued(&ctx),
            "work listed before an unbind was enqueued for the unbound root"
        );
    }

    #[test]
    fn inspect_inventory_preserves_both_walks_and_their_directory_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonicalize_normalized(temp.path());
        write(&root, "src/main.rs", "fn main() {}\n");
        write(&root, "pkg/package.json", "{}");
        write(&root, "pkg/lib.ts", "export const value = 1;\n");
        write(&root, "Cargo.toml", "[workspace]\n");
        write(&root, "go/go.mod", "module fixture\n");
        write(&root, "target/package.json", "{}");
        write(&root, ".hidden/package.json", "{}");
        write(&root, "ignored/package.json", "{}");
        write(&root, "aftignored/package.json", "{}");
        write(&root, ".gitignore", "ignored/\n");
        write(&root, ".aftignore", "aftignored/\n");
        // Manifest discovery admits a visible storage directory, but source
        // enumeration must not descend into it, even through the shared walk.
        write(&root, "storage/pkg/package.json", "{}");
        write(
            &root,
            "storage/pkg/internal.ts",
            "export const internal = 1;\n",
        );
        crate::project_content::register_storage_root(&root.join("storage"));
        let mut sources = crate::callgraph::walk_project_files(
            &root,
            if crate::data_layer::read_fence::active() {
                crate::project_walk::IgnoreFileAccess::Fenced
            } else {
                crate::project_walk::IgnoreFileAccess::Allowed
            },
        )
        .collect::<Vec<_>>();
        sources.sort();
        let manifests = entry_points::collect_entry_point_manifests(&root);
        let inventory = Arc::new(InspectInventory::collect(&root, || true));
        assert_eq!(inventory.sources, sources);
        assert_eq!(inventory.manifests, manifests);
        assert!(inventory
            .manifests
            .contains(&root.join("storage/pkg/package.json")));
        assert!(!inventory
            .sources
            .contains(&root.join("storage/pkg/internal.ts")));
        let _guard = inventory.install();
        assert_eq!(
            entry_points::collect_entry_point_manifests(&root),
            manifests
        );
        let other = tempfile::tempdir().unwrap();
        assert!(current(other.path()).is_none());
    }

    /// A context for `root` whose index-store checkout is bound and has
    /// published its scan of the folder.
    fn bound_context(root: &Path) -> crate::context::AppContext {
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        crate::data_layer::surfaces::serve_text_only_for_test(&ctx);
        ctx
    }

    /// The index store's members, filtered by the walks' own rules, list
    /// exactly the sources and manifests the shared walk finds, across every
    /// rule where the two could part: hidden paths, ignore files, the walks'
    /// skipped folders, manifests the source walk skips, AFT's own storage
    /// and files, symlinks, oversize sources and non-source files. A change
    /// recorded after the publication shows up as the walk sees it.
    #[test]
    fn inventory_from_the_index_store_lists_what_the_walk_lists() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonicalize_normalized(temp.path());
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        for (path, contents) in [
            ("src/main.rs", "fn main() {}\n"),
            ("src/.hidden.rs", "fn hidden() {}\n"),
            (".github/tool.py", "x = 1\n"),
            ("pkg/package.json", "{}"),
            ("pkg/lib.ts", "export const value = 1;\n"),
            ("pkg/dist/package.json", "{}"),
            ("pkg/dist/out.js", "export {};\n"),
            ("build/gen.rs", "fn generated() {}\n"),
            ("node_modules/dep/package.json", "{}"),
            ("node_modules/dep/index.js", "module.exports = 1;\n"),
            ("target/debug/x.rs", "fn x() {}\n"),
            ("venv/lib.py", "y = 1\n"),
            ("__pycache__/m.py", "z = 1\n"),
            ("Cargo.toml", "[workspace]\n"),
            ("go/go.mod", "module fixture\n"),
            ("go/main.go", "package main\nfunc main() {}\n"),
            ("ignored/lib.rs", "fn ignored() {}\n"),
            ("aftignored/lib.rs", "fn aftignored() {}\n"),
            (".gitignore", "ignored/\n"),
            (".aftignore", "aftignored/\n"),
            (".DS_Store", ""),
            ("docs/readme.md", "# docs\n"),
            ("notes.txt", "notes\n"),
            ("data.db", "not a database\n"),
            ("deep/a/b/c.py", "w = 1\n"),
            ("storage/pkg/package.json", "{}"),
            ("storage/pkg/internal.ts", "export const internal = 1;\n"),
        ] {
            write(&root, path, contents);
        }
        // Past the store's read limit: listed, never read.
        write(&root, "src/huge.rs", &"// filler\n".repeat(500_000));
        #[cfg(unix)]
        std::os::unix::fs::symlink("src/main.rs", root.join("link.rs")).unwrap();
        crate::project_content::register_storage_root(&root.join("storage"));

        let ctx = bound_context(&root);
        let checkout = ctx.data_checkout().expect("the checkout is bound");
        let view = checkout.view().expect("the scan is published");
        let from_store =
            InspectInventory::from_checkout(&root, &view).expect("the view stands in for a walk");
        let walked = InspectInventory::collect(&root, || true);
        assert_eq!(from_store.sources, walked.sources);
        assert_eq!(from_store.manifests, walked.manifests);
        // The fixture exercises both sides of each rule.
        for kept in [
            "src/main.rs",
            "src/huge.rs",
            "pkg/lib.ts",
            "go/main.go",
            "deep/a/b/c.py",
        ] {
            assert!(walked.sources.contains(&root.join(kept)), "{kept}");
        }
        for left_out in [
            "src/.hidden.rs",
            "pkg/dist/out.js",
            "build/gen.rs",
            "node_modules/dep/index.js",
            "ignored/lib.rs",
            "aftignored/lib.rs",
            "storage/pkg/internal.ts",
            "link.rs",
        ] {
            assert!(!walked.sources.contains(&root.join(left_out)), "{left_out}");
        }
        assert!(walked
            .manifests
            .contains(&root.join("storage/pkg/package.json")));
        assert!(!walked
            .manifests
            .contains(&root.join("pkg/dist/package.json")));

        // A file added after the publication is listed once recorded.
        write(&root, "src/added.rs", "fn added() {}\n");
        checkout.note_paths(&[root.join("src/added.rs")]);
        let view = checkout.view().expect("a view after the change");
        let from_store = InspectInventory::from_checkout(&root, &view).unwrap();
        let walked = InspectInventory::collect(&root, || true);
        assert!(walked.sources.contains(&root.join("src/added.rs")));
        assert_eq!(from_store.sources, walked.sources);
    }

    /// The same comparison on a real checkout named by
    /// `AFT_INVENTORY_PARITY_ROOT`, printing what differs. Run by hand on
    /// repositories before trusting the store's list in a new kind of
    /// checkout.
    #[test]
    #[ignore = "reads a real checkout named by AFT_INVENTORY_PARITY_ROOT"]
    fn inventory_from_the_index_store_matches_the_walk_on_a_real_checkout() {
        let root = canonicalize_normalized(Path::new(
            &std::env::var("AFT_INVENTORY_PARITY_ROOT").expect("AFT_INVENTORY_PARITY_ROOT"),
        ));
        let ctx = bound_context(&root);
        let view = ctx.data_checkout().unwrap().view().unwrap();
        let started = std::time::Instant::now();
        let from_store = InspectInventory::from_checkout(&root, &view).expect("stands in");
        let store_time = started.elapsed();
        let started = std::time::Instant::now();
        let walked = InspectInventory::collect(&root, || true);
        let walk_time = started.elapsed();
        let only = |left: &[PathBuf], right: &[PathBuf]| {
            left.iter()
                .filter(|path| right.binary_search(path).is_err())
                .cloned()
                .collect::<Vec<_>>()
        };
        println!(
            "{}: sources {} / {}, manifests {} / {}; store {store_time:?}, walk {walk_time:?}",
            root.display(),
            from_store.sources.len(),
            walked.sources.len(),
            from_store.manifests.len(),
            walked.manifests.len(),
        );
        println!(
            "sources only in the store: {:?}",
            only(&from_store.sources, &walked.sources)
        );
        println!(
            "sources only in the walk: {:?}",
            only(&walked.sources, &from_store.sources)
        );
        println!(
            "manifests only in the store: {:?}",
            only(&from_store.manifests, &walked.manifests)
        );
        println!(
            "manifests only in the walk: {:?}",
            only(&walked.manifests, &from_store.manifests)
        );
        assert_eq!(from_store.sources, walked.sources);
        assert_eq!(from_store.manifests, walked.manifests);
    }

    #[test]
    fn inspect_inventory_manifest_hash_rereads_contents_and_discovers_additions_and_removals() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonicalize_normalized(temp.path());
        let storage = tempfile::tempdir().unwrap();
        write(&root, "pkg/package.json", "{\"main\":\"one.js\"}");
        let cache =
            crate::inspect::InspectCache::open(storage.path().to_path_buf(), root.clone()).unwrap();
        let category = crate::inspect_category::InspectCategory::Duplicates;
        let original = cache.contribution_set_hash(category).unwrap();
        let inventory = Arc::new(InspectInventory::collect(&root, || true));
        let guard = inventory.install();
        assert_eq!(cache.contribution_set_hash(category).unwrap(), original);
        let path = root.join("pkg/package.json");
        let mtime =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&path).unwrap());
        write(&root, "pkg/package.json", "{\"main\":\"two.js\"}");
        filetime::set_file_mtime(&path, mtime).unwrap();
        let changed = cache.contribution_set_hash(category).unwrap();
        assert_ne!(
            changed, original,
            "manifest content is not memoized by metadata"
        );
        drop(guard);
        assert_eq!(cache.contribution_set_hash(category).unwrap(), changed);
        for (name, contents) in [
            ("new/Cargo.toml", "[workspace]\n"),
            ("other/package.json", "{}"),
            ("go/go.mod", "module example\n"),
        ] {
            let before = cache.contribution_set_hash(category).unwrap();
            write(&root, name, contents);
            let expected = cache.contribution_set_hash(category).unwrap();
            assert_ne!(before, expected, "adding {name} changes the hash");
            let inventory = Arc::new(InspectInventory::collect(&root, || true));
            let guard = inventory.install();
            assert_eq!(cache.contribution_set_hash(category).unwrap(), expected);
            drop(guard);
            std::fs::remove_file(root.join(name)).unwrap();
            let inventory = Arc::new(InspectInventory::collect(&root, || true));
            let _guard = inventory.install();
            assert_eq!(cache.contribution_set_hash(category).unwrap(), before);
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<InspectInventory>>> = const { RefCell::new(None) };
}

pub(crate) fn current(root: &Path) -> Option<Arc<InspectInventory>> {
    CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|inventory| inventory.matches(root))
            .cloned()
    })
}

pub(super) fn inherited() -> Option<Arc<InspectInventory>> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// The whole-project source list a background job needs, built before the
/// job is admitted: without a bound index store it walks the repository, and
/// admission holds the root lifecycle lock that unbinding the root waits for.
/// Installed around the admission, it lets the job's enqueue filter this list
/// instead of walking under the lock. It carries no manifests; install it
/// only around that enqueue.
pub(crate) fn for_background_job(root: &Path) -> Arc<InspectInventory> {
    #[cfg(test)]
    if let Some(hook) = BACKGROUND_JOB_LISTING_HOOK.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
    let scope = super::JobScope::for_project(root.to_path_buf());
    Arc::new(InspectInventory {
        root: canonicalize_normalized(root),
        sources: super::manager::scope_files(root, &scope),
        manifests: Vec::new(),
    })
}

#[cfg(test)]
thread_local! {
    /// Runs once on this thread while [`for_background_job`] lists files.
    pub(crate) static BACKGROUND_JOB_LISTING_HOOK: RefCell<Option<Box<dyn FnOnce()>>> =
        const { RefCell::new(None) };
}

pub(crate) struct InventoryGuard(Option<Arc<InspectInventory>>);

impl Drop for InventoryGuard {
    fn drop(&mut self) {
        CURRENT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

impl InspectInventory {
    pub(crate) fn install(self: &Arc<Self>) -> InventoryGuard {
        InventoryGuard(CURRENT.with(|slot| slot.replace(Some(Arc::clone(self)))))
    }

    pub(super) fn matches(&self, root: &Path) -> bool {
        self.root == root || self.root == canonicalize_normalized(root)
    }

    /// The inventory of `root` as its index-store checkout lists it in
    /// `view`, without walking the folder: the same sources and manifests
    /// [`collect`](Self::collect) finds, since the store's scan applies the
    /// same ignore files, skipped folders and nested-worktree boundary, and
    /// the rules the two walks add on top (hidden paths, the walks' skipped
    /// folder names, source languages, manifest names, AFT's own files) are
    /// applied here to each member's path.
    ///
    /// `None` when the view cannot stand in for a walk: it is of another
    /// folder, a rescan against the disk is still pending, or the root is one
    /// where the source walk uses other ignore settings (confinement, or a
    /// Jujutsu-only repository).
    pub(crate) fn from_checkout(
        root: &Path,
        view: &crate::data_layer::checkout::CheckoutView,
    ) -> Option<Self> {
        Self::from_view(root, view).ok()
    }

    /// [`from_checkout`](Self::from_checkout), saying why the view cannot
    /// stand in for a walk when it cannot.
    pub(crate) fn from_view(
        root: &Path,
        view: &crate::data_layer::checkout::CheckoutView,
    ) -> Result<Self, StoreListMiss> {
        let normalized_root = canonicalize_normalized(root);
        if canonicalize_normalized(view.root()) != normalized_root {
            return Err(StoreListMiss::OtherRoot);
        }
        if walks_use_other_ignore_settings(root) {
            return Err(StoreListMiss::OtherIgnoreSettings);
        }
        if view.rescanning() {
            return Err(StoreListMiss::Rescanning);
        }
        let mut sources = Vec::new();
        let mut manifests = Vec::new();
        for record in view.current_state().paths() {
            if record.kind == aft_store::checkout::PathKind::Symlink {
                continue;
            }
            let components: Vec<&[u8]> = record.path.split(|&byte| byte == b'/').collect();
            if components
                .iter()
                .any(|component| component.is_empty() || component.starts_with(b"."))
            {
                continue;
            }
            let relative = String::from_utf8_lossy(&record.path);
            if crate::callgraph::walk_could_include(&relative) {
                if let Some(path) = member_path(root, &components) {
                    if !crate::project_content::is_excluded_file_by_path(&path) {
                        sources.push(path);
                    }
                }
            }
            let (name, directories) = components.split_last().expect("a member has a name");
            let manifest_name = matches!(*name, b"Cargo.toml" | b"package.json" | b"go.mod");
            if manifest_name
                && !directories.iter().any(|directory| {
                    entry_points::should_skip_manifest_dir(Path::new(
                        String::from_utf8_lossy(directory).as_ref(),
                    ))
                })
            {
                if let Some(path) = member_path(&normalized_root, &components) {
                    manifests.push(path);
                }
            }
        }
        sources.sort();
        manifests.sort();
        manifests.dedup();
        Ok(Self {
            root: normalized_root,
            sources,
            manifests,
        })
    }

    pub(crate) fn collect(
        root: &Path,
        keep_walking: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        #[cfg(test)]
        let _profile = super::walk_profile::Walk::start("shared_inventory");
        let normalized_root = canonicalize_normalized(root);
        let budget = Arc::new(keep_walking);
        let (mut sources, mut manifests) = if walks_use_other_ignore_settings(root) {
            let walk_budget = Arc::clone(&budget);
            let sources = crate::callgraph::walk_project_files_while(
                root,
                if crate::data_layer::read_fence::active() {
                    crate::project_walk::IgnoreFileAccess::Fenced
                } else {
                    crate::project_walk::IgnoreFileAccess::Allowed
                },
                move || walk_budget(),
            )
            .collect();
            let entry_budget = Arc::clone(&budget);
            let entries =
                entry_points::uncached_entry_point_files_while(&normalized_root, move || {
                    entry_budget()
                });
            (
                sources,
                entries
                    .into_iter()
                    .filter(|path| entry_points::is_entry_point_manifest(path))
                    .collect(),
            )
        } else {
            #[derive(Default)]
            struct Census {
                // Some directories belong to just one walk. Their descendants
                // inherit each policy's decision even when the union descends.
                directories: HashMap<PathBuf, (bool, bool)>,
                sources: Vec<PathBuf>,
                // Retaining other files would sort the whole checkout again for
                // each before/after file listing, but the hash only needs manifests.
                manifests: Vec<PathBuf>,
            }
            let census = Arc::new(Mutex::new(Census::default()));
            let collected = Arc::clone(&census);
            let filter_budget = Arc::clone(&budget);
            let walk_root = root.to_path_buf();
            let entry_root = normalized_root.clone();
            let worktrees = crate::project_content::NestedWorktreeBoundary::for_root(root);
            let mut builder = entry_points::manifest_walk_builder(root);
            builder.filter_entry(move |entry| {
                #[cfg(test)]
                super::walk_profile::entry();
                if !filter_budget() {
                    return false;
                }
                #[cfg(test)]
                crate::callgraph::PROJECT_WALK_ENTRIES.with(|count| count.set(count.get() + 1));
                let mut state = collected.lock().unwrap();
                let (source_parent, manifest_parent) = entry
                    .path()
                    .parent()
                    .and_then(|parent| state.directories.get(parent))
                    .copied()
                    .unwrap_or((true, true));
                let source = source_parent
                    && crate::callgraph::project_walk_entry_is_included(entry, &worktrees);
                // A source-admitted directory already passed the shared worktree
                // boundary check; manifest-only directories still need that check.
                let manifest = manifest_parent
                    && (!entry.file_type().is_some_and(|kind| kind.is_dir())
                        || (!entry_points::should_skip_manifest_dir(entry.path())
                            && (source
                                || !crate::search_index::is_nested_worktree_entry(
                                    entry, &worktrees,
                                ))));
                if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    state
                        .directories
                        .insert(entry.path().to_path_buf(), (source, manifest));
                } else if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    if source
                        && crate::parser::detect_language_any(
                            entry.path(),
                            cfg!(feature = "wasm-grammars"),
                        )
                        .is_some()
                    {
                        state.sources.push(entry.path().to_path_buf());
                    }
                    if manifest && entry_points::is_entry_point_manifest(entry.path()) {
                        let relative = entry
                            .path()
                            .strip_prefix(&walk_root)
                            .expect("walk entry under root");
                        state.manifests.push(entry_root.join(relative));
                    }
                }
                source || manifest
            });
            let mut walker = builder.build();
            while budget() && walker.next().is_some() {
                crate::tool_progress::report();
            }
            drop(walker);
            let mut state = census.lock().unwrap();
            (
                std::mem::take(&mut state.sources),
                std::mem::take(&mut state.manifests),
            )
        };
        sources.sort();
        manifests.sort();
        manifests.dedup();
        Self {
            root: normalized_root,
            sources,
            manifests,
        }
    }
}

/// Why the index store's list of a folder's files could not stand in for a
/// walk of the folder, so the caller walked it instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreListMiss {
    /// No index-store checkout of the folder is bound in this process.
    NotBound,
    /// The folder holds repositories, each with a checkout of its own, and
    /// has none itself.
    FolderOfRepositories,
    /// The index store never binds the folder (home folder, drive root, or
    /// every index off).
    Withheld,
    /// The checkout is bound but has not published its first scan.
    NotPublished,
    /// A rescan against the disk is pending, so the list can miss changes.
    Rescanning,
    /// The checkout is of another folder.
    OtherRoot,
    /// The source walk uses ignore settings the store's scan does not
    /// (confinement, or a Jujutsu-only repository).
    OtherIgnoreSettings,
}

impl StoreListMiss {
    /// The name logs give this reason.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NotBound => "not_bound",
            Self::FolderOfRepositories => "folder_of_repositories",
            Self::Withheld => "withheld",
            Self::NotPublished => "not_published",
            Self::Rescanning => "rescanning",
            Self::OtherRoot => "other_root",
            Self::OtherIgnoreSettings => "other_ignore_settings",
        }
    }
}

/// Whether the source walk at `root` uses ignore settings the shared walk
/// (and the index store's scan) does not: under confinement, and in a
/// Jujutsu-only repository. There the two walks run separately rather than
/// broadening either set.
fn walks_use_other_ignore_settings(root: &Path) -> bool {
    crate::context::confinement::active()
        || (root.ancestors().any(|path| path.join(".jj").exists())
            && !root.ancestors().any(|path| path.join(".git").exists()))
}

/// `base` joined with a member's `/`-separated path components, spelled with
/// this system's separator as a walk would spell it. `None` for a component
/// that is not valid UTF-8 on a system whose paths must be.
fn member_path(base: &Path, components: &[&[u8]]) -> Option<PathBuf> {
    let mut path = base.to_path_buf();
    for component in components {
        #[cfg(unix)]
        path.push(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(component));
        #[cfg(not(unix))]
        path.push(std::str::from_utf8(component).ok()?);
    }
    Some(path)
}
