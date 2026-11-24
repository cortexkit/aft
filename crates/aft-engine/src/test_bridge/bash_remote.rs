pub fn record_service(root: &Path) -> std::sync::Arc<crate::gh_shim_speech::service::Service> {
    crate::gh_shim_speech::service::record_service(root)
}

mod sandbox;

pub use sandbox::*;

pub mod registry_support {
    pub use crate::shell_test_fixtures::registry::*;
}

#[cfg(unix)]
pub mod remote_support {
    pub use crate::shell_test_fixtures::remote::*;
}

mod speech;
pub use speech::*;

mod fixture_paths;
pub use fixture_paths::*;
