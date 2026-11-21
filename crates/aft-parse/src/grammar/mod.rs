//! Hash-pinned grammar availability is exposed in every build. Downloading and
//! interpreting grammars is available only with the `wasm-grammars` feature.
mod extractors;
mod manifest;
test_support_visibility! {
mod state;
}

pub use extractors::{extractor_language, extractor_name, extractor_version, parse_deadline};
#[cfg(feature = "test-hooks")]
pub use manifest::initialize_test_identity;
#[cfg(feature = "test-support")]
pub use manifest::production_manifest;
pub use manifest::{manifest, manifest_for_path, manifests, GrammarManifest};
pub use state::{FailureCause, GrammarState, GrammarUnavailable};

use std::{collections::BTreeMap, sync::Arc};

type RegistryMap<T> = BTreeMap<&'static str, Arc<T>>;

fn registry_map<T>(
    entries: &'static [GrammarManifest],
    build: impl Fn(&'static GrammarManifest) -> Arc<T>,
) -> RegistryMap<T> {
    entries
        .iter()
        .map(|entry| (entry.language.as_str(), build(entry)))
        .collect()
}

test_support_visibility! {
#[cfg(feature = "wasm-grammars")]
mod download;
}
test_support_visibility! {
    #[cfg(feature = "wasm-grammars")]
    mod runtime;
}
test_support_visibility! {
#[cfg(feature = "wasm-grammars")]
mod storage;
}
#[cfg(feature = "wasm-grammars")]
pub use runtime::*;

test_support_visibility! {
    #[cfg(not(feature = "wasm-grammars"))]
    mod disabled;
}
#[cfg(not(feature = "wasm-grammars"))]
pub use disabled::*;
