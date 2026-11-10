use std::path::{Path, PathBuf};

/// A repository outside the configured root that `aft_search` was given as
/// its `path`, answered from the repository's own checkout in the data
/// layer once `aft index` or another AFT has published one. A context binds
/// it once and releases it with its other idle handles.
pub(crate) struct ExternalSearch {
    pub(crate) checkout: std::sync::Arc<crate::data_layer::runtime::DataCheckout>,
    /// A search context for the repository alone, answering from
    /// `checkout` (see `AppContext::pin_data_checkout`).
    pub(crate) engine: crate::context::AppContext,
    /// Set until the first search: the bind has just compared the files on
    /// disk with the publication, so that search need not again.
    pub(crate) just_bound: std::sync::atomic::AtomicBool,
}

/// How long a repository whose store could not be bound is answered by the
/// folder scan before its store is tried again.
const BIND_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

fn failed_binds(
) -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, std::time::Instant>> {
    static FAILED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    FAILED.get_or_init(Default::default)
}

fn recently_failed(root: &Path) -> bool {
    failed_binds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .is_some_and(|at| at.elapsed() < BIND_RETRY_AFTER)
}

fn note_failure(root: &Path) {
    failed_binds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), std::time::Instant::now());
}

impl ExternalSearch {
    /// The search of the repository at `root` for `ctx`, bound now if `ctx`
    /// holds none: `None` when the data layer is off, or nothing has
    /// published a checkout of `root`. Such a repository is walked, as
    /// today, and a query never builds a store for it.
    pub(crate) fn of(
        ctx: &crate::context::AppContext,
        root: &Path,
    ) -> Option<std::sync::Arc<Self>> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        ctx.external_data_search(&root, || Self::bind(ctx, &root))
    }

    fn bind(ctx: &crate::context::AppContext, root: &Path) -> Option<Self> {
        let storage = ctx.storage_dir();
        let planes = crate::data_layer::checkout::Planes {
            text: true,
            callgraph: false,
        };
        // A checkout with no publication would start a build as it binds,
        // and a repository with no store would get one, so a query of
        // another repository looks first, with a foreground bind that
        // starts nothing.
        if !crate::data_layer::runtime::has_family_store(root, &storage) {
            return None;
        }
        let published = crate::data_layer::runtime::bind_foreground(root, &storage, planes)
            .ok()?
            .view()
            .is_some();
        if !published {
            return None;
        }
        // A store that cannot be bound is not retried on every query: each
        // try reads the publication again, and an agent's searches of one
        // other repository can come many times a minute.
        if recently_failed(root) {
            return None;
        }
        let checkout =
            crate::data_layer::runtime::DataCheckout::open_planes(root, &storage, None, planes)
                .map_err(|error| {
                    crate::slog_warn!("data layer could not bind {}: {error}", root.display());
                    note_failure(root);
                })
                .ok()?;
        let session = ctx.config();
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.to_path_buf());
        config.storage_dir = Some(storage);
        // The engine reads only the pinned checkout, never a view of its own.
        config.indexes.trigram = true;
        config.indexes.semantic = session.indexes.semantic;
        config.indexes.callgraph = false;
        config.semantic = session.semantic.clone();
        let engine = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        );
        engine.pin_data_checkout(std::sync::Arc::clone(&checkout));
        Some(Self {
            checkout,
            engine,
            just_bound: std::sync::atomic::AtomicBool::new(true),
        })
    }
}
