use std::sync::Arc;
use std::time::Duration;

use crate::config::{RerankBackendKind, SearchConfig, SemanticBackendConfig};
use crate::search_state::rerank::{self, slot, RerankSettings, SelectedBackend};

pub use rerank::NoopRerankBackend;

pub const DEFAULT_TIMEOUT_MS: u64 = rerank::DEFAULT_TIMEOUT_MS;
pub const DEFAULT_TOP_N: usize = rerank::DEFAULT_TOP_N;
pub const MAX_TIMEOUT_MS: u64 = rerank::MAX_TIMEOUT_MS;
pub const MAX_TOP_N: usize = rerank::MAX_TOP_N;
pub const MIN_TIMEOUT_MS: u64 = rerank::MIN_TIMEOUT_MS;
pub const MAX_BUILD_ATTEMPTS: u32 = slot::MAX_BUILD_ATTEMPTS;

pub fn note_io() {
    rerank::note_io();
}
pub fn io_on_this_thread() -> usize {
    rerank::io_on_this_thread()
}
pub fn backend(settings: &RerankSettings) -> RerankBackendKind {
    settings.backend
}
pub fn model(settings: &RerankSettings) -> Option<&str> {
    settings.model.as_deref()
}

pub type Constructor =
    Arc<dyn Fn(&slot::BuildInputs) -> Result<Option<SelectedBackend>, String> + Send + Sync>;

pub fn build_inputs(
    search: SearchConfig,
    semantic: SemanticBackendConfig,
    under_subc: bool,
) -> slot::BuildInputs {
    slot::BuildInputs {
        search,
        semantic,
        under_subc,
        cache_unavailable: false,
        cache_lease: None,
        cache_epoch: 0,
    }
}

pub fn search(inputs: &slot::BuildInputs) -> &SearchConfig {
    &inputs.search
}
pub fn search_mut(inputs: &mut slot::BuildInputs) -> &mut SearchConfig {
    &mut inputs.search
}
pub fn semantic_mut(inputs: &mut slot::BuildInputs) -> &mut SemanticBackendConfig {
    &mut inputs.semantic
}

pub fn build_backend(inputs: &slot::BuildInputs) -> Result<Option<SelectedBackend>, String> {
    slot::build_backend(inputs)
}
pub fn slot_with_constructor(constructor: Constructor) -> slot::BackendSlot {
    slot::BackendSlot::with_constructor(constructor)
}

pub trait BackendSlotTest {
    fn set_constructor_for_test(&self, constructor: Constructor, backoff: Duration);
    fn reconcile(&self, inputs: slot::BuildInputs);
}

pub trait BuildInputsTestExt {
    fn search_mut(&mut self) -> &mut SearchConfig;
}

impl BuildInputsTestExt for slot::BuildInputs {
    fn search_mut(&mut self) -> &mut SearchConfig {
        &mut self.search
    }
}

impl BackendSlotTest for slot::BackendSlot {
    fn set_constructor_for_test(&self, constructor: Constructor, backoff: Duration) {
        self.set_constructor_for_test(constructor, backoff);
    }
    fn reconcile(&self, inputs: slot::BuildInputs) {
        self.reconcile(inputs);
    }
}

pub mod remote {
    pub use crate::search_state::rerank::remote::remote_fixture::serve;
}

pub mod synapse {
    pub use crate::search_state::rerank::synapse::synapse_fixture::{
        config, fake_daemon, fake_daemon_gated,
    };
}

pub mod fixture {
    use crate::search_state::rerank::fixture::PackScore;
    pub use crate::search_state::rerank::fixture::{recording, FixtureBackend, FixturePack};

    pub fn scores(pack: &FixturePack) -> &[PackScore] {
        &pack.scores
    }
    pub fn fingerprint_backend(pack: &FixturePack) -> &str {
        &pack.fingerprint.backend
    }
    pub fn text_policy(pack: &FixturePack) -> &str {
        &pack.text_policy
    }
}

pub mod onnx;

pub fn set_cache_unavailable(inputs: &mut slot::BuildInputs, value: bool) {
    inputs.cache_unavailable = value;
}
