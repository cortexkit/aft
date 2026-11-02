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
        let mut sources = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
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

    #[test]
    fn inspect_inventory_manifest_hash_rereads_contents_and_discovers_additions_and_removals() {
        let temp = tempfile::tempdir().unwrap();
        let root = canonicalize_normalized(temp.path());
        let storage = tempfile::tempdir().unwrap();
        write(&root, "pkg/package.json", "{\"main\":\"one.js\"}");
        let cache =
            crate::inspect::InspectCache::open(storage.path().to_path_buf(), root.clone()).unwrap();
        let category = crate::inspect::InspectCategory::Duplicates;
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

    pub(crate) fn collect(
        root: &Path,
        keep_walking: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        #[cfg(test)]
        let _profile = super::walk_profile::Walk::start("shared_inventory");
        let normalized_root = canonicalize_normalized(root);
        let budget = Arc::new(keep_walking);
        // In confinement and jj-only roots the source walker has different ignore
        // settings. Keep the independent walkers rather than broaden either set.
        let jj_only = root.ancestors().any(|path| path.join(".jj").exists())
            && !root.ancestors().any(|path| path.join(".git").exists());
        let (mut sources, mut manifests) = if crate::context::confinement::active() || jj_only {
            let walk_budget = Arc::clone(&budget);
            let sources =
                crate::callgraph::walk_project_files_while(root, move || walk_budget()).collect();
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
                    if source && crate::parser::detect_language(entry.path()).is_some() {
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
                crate::run_tool_call::progress::report();
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
