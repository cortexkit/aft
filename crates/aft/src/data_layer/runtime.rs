//! The data layer inside a running AFT, while it is built beside the stores
//! it replaces.
//!
//! It is off unless the process runs with `AFT_DATA_LAYER=1`, an
//! undocumented switch for development: tools read from it only where they
//! have been wired to, and only when it is on. When it is on, the first
//! query of a root binds the root's checkout and, if the checkout has no
//! publication yet, builds it on a thread of its own. AFT's own edits reach
//! it through the edit-intent listener, and outside changes through the
//! watcher's path batches.
//!
//! The builds here run on plain threads. At cutover they move onto the
//! root's executor, so an unbound root does no work and a drain cancels it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use aft_store::ids::{Digest128, FamilyId};

use super::checkout::{Checkout, CheckoutView, DataError, DataLayer, PathChange};
use crate::views::intent::{WriteIntent, WriteIntentListener, WritePhase};

/// Whether this process runs the data layer.
#[must_use]
pub(crate) fn enabled() -> bool {
    std::env::var_os("AFT_DATA_LAYER").is_some_and(|value| value == "1")
}

/// The process's data layer: one block cache for every checkout's reads.
fn layer() -> Arc<DataLayer> {
    static LAYER: OnceLock<Arc<DataLayer>> = OnceLock::new();
    Arc::clone(LAYER.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        Arc::new(DataLayer::new(256 << 20, threads))
    }))
}

/// The family a checkout at `root` belongs to: its repository's root
/// commits, so every worktree and clone of one repository shares a store.
/// A folder outside Git is a family of its own.
fn family_of(root: &Path) -> FamilyId {
    let identity = crate::search_index::canonical_git_root_commit_identity(root)
        .unwrap_or_else(|_| format!("folder:{}", root.display()));
    FamilyId::from_digest(Digest128::of(identity.as_bytes()))
}

/// One root's checkout in a running AFT.
pub(crate) struct DataCheckout {
    checkout: Arc<Checkout>,
    building: Arc<AtomicBool>,
    /// Kept so the edit-intent listener lives as long as the checkout.
    _listener: Arc<dyn WriteIntentListener>,
}

impl DataCheckout {
    /// Binds the checkout at `root`, with its family store under
    /// `storage`, and builds it in the background if it has no publication.
    ///
    /// # Errors
    ///
    /// When the family store cannot be opened.
    pub(crate) fn open(root: &Path, storage: &Path) -> Result<Arc<Self>, DataError> {
        let family = family_of(root);
        let family_dir = storage.join("data").join(family.digest().to_string());
        let checkout = Arc::new(Checkout::bind(layer(), &family_dir, family, root)?);
        let building = Arc::new(AtomicBool::new(false));
        let listener: Arc<dyn WriteIntentListener> = Arc::new(EditListener {
            checkout: Arc::downgrade(&checkout),
            root: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            building: Arc::clone(&building),
        });
        crate::views::intent::register_listener(&listener);
        let handle = Arc::new(Self {
            checkout,
            building,
            _listener: listener,
        });
        if handle.checkout.view().is_none() {
            start_build(&handle.checkout, &handle.building);
        }
        Ok(handle)
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        self.checkout.root()
    }

    #[must_use]
    pub(crate) fn view(&self) -> Option<CheckoutView> {
        self.checkout.view()
    }

    #[must_use]
    pub(crate) fn checkout(&self) -> &Checkout {
        &self.checkout
    }

    /// Records the watcher's changed `paths` (absolute).
    pub(crate) fn note_paths(&self, paths: &[PathBuf]) {
        let root = std::fs::canonicalize(self.checkout.root())
            .unwrap_or_else(|_| self.checkout.root().to_path_buf());
        record(&self.checkout, &self.building, &root, paths);
    }
}

/// Starts a build on its own thread unless one is running.
fn start_build(checkout: &Arc<Checkout>, building: &Arc<AtomicBool>) {
    if building.swap(true, Ordering::AcqRel) {
        return;
    }
    let checkout = Arc::clone(checkout);
    let running = Arc::clone(building);
    let spawned = std::thread::Builder::new()
        .name("aft-data-build".to_string())
        .spawn(move || {
            match checkout.build() {
                Ok(report) => crate::slog_info!(
                    "data layer built {}: {} files, {} parsed, {} bytes",
                    checkout.root().display(),
                    report.files,
                    report.parsed_files,
                    report.segment_bytes
                ),
                Err(error) => crate::slog_warn!(
                    "data layer build failed for {}: {error}",
                    checkout.root().display()
                ),
            }
            running.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        building.store(false, Ordering::Release);
    }
}

/// Records changed absolute `paths` under `root`: each file's current bytes,
/// or its removal. A path that is a directory (a directory created, moved or
/// removed) cannot be enumerated from here, so the checkout builds again.
fn record(checkout: &Arc<Checkout>, building: &Arc<AtomicBool>, root: &Path, paths: &[PathBuf]) {
    let mut changes = Vec::new();
    for path in paths {
        let canonical_parent = path
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .map(|parent| parent.join(path.file_name().unwrap_or_default()))
            .unwrap_or_else(|| path.clone());
        let Ok(relative) = canonical_parent.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str().map(|text| text.replace('\\', "/")) else {
            continue;
        };
        if relative.is_empty() || relative.starts_with(".git/") || relative == ".git" {
            continue;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                start_build(checkout, building);
                return;
            }
            Ok(metadata) if metadata.is_file() => match std::fs::read(path) {
                Ok(bytes) => changes.push(PathChange::Written {
                    path: relative.into_bytes(),
                    bytes: Arc::from(bytes),
                }),
                // Unreadable now: a rebuild reads it, or records why not.
                Err(_) => {
                    start_build(checkout, building);
                    return;
                }
            },
            Ok(_) => {
                // A symlink or special file: the scan decides what it is.
                start_build(checkout, building);
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                changes.push(PathChange::Removed {
                    path: relative.into_bytes(),
                });
            }
            Err(_) => {
                start_build(checkout, building);
                return;
            }
        }
    }
    checkout.record_changes(&changes);
}

/// Hears AFT's own edits.
struct EditListener {
    checkout: Weak<Checkout>,
    root: PathBuf,
    building: Arc<AtomicBool>,
}

impl WriteIntentListener for EditListener {
    fn record_change(&self, change: &WriteIntent, phase: WritePhase) {
        // After the write, the bytes are final (formatting and rollback
        // included).
        if phase != WritePhase::After {
            return;
        }
        let Some(checkout) = self.checkout.upgrade() else {
            return;
        };
        match change {
            WriteIntent::Paths(paths) => record(&checkout, &self.building, &self.root, paths),
            WriteIntent::Directory(_) => start_build(&checkout, &self.building),
        }
    }
}
