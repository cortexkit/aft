use super::*;

fn downloaded_language(path: &[u8]) -> Option<&'static str> {
    crate::data_layer::grammar::downloaded_language(&crate::data_layer::vectors::native_path(path)?)
}

fn gap_language(path: &[u8], reason: &str) -> Option<&'static str> {
    let language = downloaded_language(path)?;
    serde_json::from_str::<serde_json::Value>(reason)
        .is_ok_and(|value| value["code"] == "grammar_unavailable")
        .then_some(language)
}

pub(super) fn todo_parse_gap(reason: &str) -> PathTodos {
    if reason == crate::data_layer::grammar::INVALID_UTF8_REASON {
        // The TODO count never reads non-UTF-8 input. A call-graph gap does
        // not make its TODO count incomplete.
        PathTodos::Unrecorded
    } else {
        PathTodos::Unextracted
    }
}

impl Checkout {
    /// Re-read unavailable grammar members even when stat and content are
    /// unchanged. A ready event is not a filesystem edit.
    pub(crate) fn retry_grammar_gaps(&self) -> Result<bool, DataError> {
        self.retry_grammar_gaps_inner(false)
    }

    #[cfg(feature = "wasm-grammars")]
    pub(crate) fn retry_ready_grammar_gaps(&self) -> Result<bool, DataError> {
        self.retry_grammar_gaps_inner(true)
    }

    fn retry_grammar_gaps_inner(&self, ready_only: bool) -> Result<bool, DataError> {
        if !cfg!(feature = "wasm-grammars") {
            return Ok(false);
        }
        #[cfg(not(feature = "wasm-grammars"))]
        let _ = ready_only;
        let gaps = {
            let live = self.live.read().expect("no panics while holding it");
            live.state
                .as_deref()
                .or_else(|| live.published.as_ref().map(|p| &p.state))
                .into_iter()
                .flat_map(|state| state.skipped_parses())
                .filter_map(|(path, reason)| {
                    gap_language(path, reason).map(|language| (path.clone(), language))
                })
                .collect::<Vec<_>>()
        };
        let mut ready = BTreeMap::new();
        let paths = gaps
            .into_iter()
            .filter_map(|(path, language)| {
                ready
                    .entry(language)
                    .or_insert_with(|| {
                        crate::grammar::registry(language).is_some_and(|registry| {
                            #[cfg(feature = "wasm-grammars")]
                            if registry.status_snapshot().is_some_and(|status| {
                                ready_only
                                    || status.state == crate::grammar::GrammarState::Downloading
                            }) {
                                return false;
                            }
                            registry.stored().is_ok()
                        })
                    })
                    .then_some(path)
            })
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Ok(false);
        }
        #[cfg(all(
            test,
            feature = "wasm-grammars",
            feature = "test-hooks",
            not(all(target_os = "windows", target_arch = "aarch64"))
        ))]
        {
            use crate::data_layer::test_gates::{self, Point};
            test_gates::arrive(&self.root, Point::GrammarRetryRead);
            if test_gates::fail_grammar_retry(&self.root) {
                return Err(DataError::Scan("injected grammar reread failure".into()));
            }
        }
        self.record_paths_inner(&paths, true)?;
        Ok(true)
    }

    #[cfg(feature = "wasm-grammars")]
    pub(crate) fn grammar_gap_languages(&self) -> BTreeSet<&'static str> {
        self.view().map_or_else(BTreeSet::new, |view| {
            view.current_state()
                .skipped_parses()
                .iter()
                .filter_map(|(path, reason)| gap_language(path, reason))
                .collect()
        })
    }

    pub(crate) fn has_unpublished_grammar_changes(&self) -> bool {
        cfg!(feature = "wasm-grammars")
            && self
                .live
                .read()
                .expect("no panics while holding it")
                .changes
                .keys()
                .any(|path| downloaded_language(path).is_some())
    }
}
