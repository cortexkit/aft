//! Filename lookup over the live walk or an up-to-date view of its paths.

use std::path::{Path, PathBuf};

use crate::commands::data_search::DataText;
use crate::context::AppContext;
use crate::search_index::{walk_project_files_from_matching, PathFilters};
use crate::search_state::waits;

/// Return files that the candidate filter admits and whose name or relative
/// path ends with a query token, newest first. Use the view only when the
/// active watcher can keep it current; otherwise consult the live tree.
pub(super) fn path_lookup_paths(
    ctx: &AppContext,
    data_text: Option<&DataText>,
    project_root: &Path,
    tokens: &[&str],
    candidate_filter: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let tokens = tokens
        .iter()
        .map(|token| token.replace('\\', "/"))
        .collect::<Vec<_>>();
    let matches = |path: &Path| {
        let relative = path
            .strip_prefix(project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let file_name = path.file_name().and_then(|name| name.to_str());
        tokens.iter().any(|normalized| {
            file_name == Some(normalized.as_str()) || relative.ends_with(normalized.as_str())
        }) && candidate_filter(path)
    };
    if let Some(text) = data_text.filter(|_| watcher_keeps_view_current(ctx)) {
        // Only relative paths ending with a token can match, so the view can
        // skip other entries before building their absolute paths.
        let ends_with_token = |relative: &[u8]| {
            tokens
                .iter()
                .any(|token| relative.ends_with(token.as_bytes()))
        };
        if let Some(files) = waits::timed(waits::SearchWait::TextSearch, || {
            text.files_matching(project_root, ends_with_token, matches)
        }) {
            return files;
        }
    }
    // Reject unrelated filenames before the walker builds metadata and
    // canonical display keys for sorting; the surviving order is unchanged.
    let _walk = waits::time(waits::SearchWait::Walk);
    walk_project_files_from_matching(project_root, project_root, &PathFilters::default(), matches)
}

pub(super) fn disclose_excluded_test_match(excluded: bool, text: &mut String) {
    if excluded {
        text.push_str("\n\nMatching test or test-support files were excluded. Use includeTests: true to include them.");
    }
}

#[cfg(test)]
thread_local! {
    static WATCHER_LIVE_FOR_TEST: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn with_watcher_live_for_test<R>(live: bool, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<bool>);
    impl Drop for Restore {
        fn drop(&mut self) {
            WATCHER_LIVE_FOR_TEST.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(WATCHER_LIVE_FOR_TEST.with(|slot| slot.replace(Some(live))));
    run()
}

fn watcher_keeps_view_current(ctx: &AppContext) -> bool {
    #[cfg(test)]
    if let Some(live) = WATCHER_LIVE_FOR_TEST.with(std::cell::Cell::get) {
        return live;
    }
    ctx.watcher_runtime_active()
}
