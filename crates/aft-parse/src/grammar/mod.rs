//! Hash-pinned grammar availability is exposed in every build. Downloading and
//! interpreting grammars is available only with the `wasm-grammars` feature.
mod manifest;
test_support_visibility! {
mod state;
}

#[cfg(feature = "test-hooks")]
pub use manifest::initialize_test_identity;
pub use manifest::{extractor_version, manifest, GrammarManifest, PRODUCTION_MANIFEST};
pub use state::{FailureCause, GrammarState, GrammarUnavailable};

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
