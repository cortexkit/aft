pub use aft_github::gh_shim_speech::*;

#[doc(hidden)]
pub mod service {
    pub use aft_github::gh_shim_speech::service::*;
    use std::{path::Path, sync::Arc};

    #[doc(hidden)]
    pub fn start(
        root: &Path,
        relay: &Arc<crate::gh_shim_relay::GhRelay>,
        describe: Describe,
        generation: String,
        describe_supported: bool,
    ) -> Arc<Service> {
        start_with_task_decoder(
            root,
            relay,
            describe,
            generation,
            describe_supported,
            Some(crate::gh_shim_ticket::decode_cold_task),
        )
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn record_service(root: &Path) -> Arc<Service> {
        record_service_with_task_decoder(root, crate::gh_shim_ticket::decode_cold_task)
    }
}
